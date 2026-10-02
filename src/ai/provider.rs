//! LLM clients behind one interface. Messages are kept in the Anthropic
//! Messages API's shape (`{"role","content":[blocks]}`) everywhere; the
//! Ollama client translates at the edge. Nothing here knows about LaTeX.
//!
//! - [`Provider::Anthropic`]: raw HTTP to `/v1/messages` (there is no official
//!   Rust SDK). Adaptive thinking, an effort level, server-side web search
//!   and fetch when asked, and `fallbacks: "default"` on the models that take
//!   it, so a safety refusal is retried on another model server-side.
//! - [`Provider::Ollama`]: a local model through `/api/chat`, with tools and
//!   images. The quarantined side of dual mode, or everything in local mode.
//! - [`Provider::Mock`]: scripted turns for tests.

use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Tool {
    pub name: &'static str,
    pub description: String,
    pub input_schema: Value,
}

pub struct Request<'a> {
    pub system: &'a str,
    pub tools: &'a [Tool],
    pub messages: &'a [Value],
    /// Server-side web search and fetch (Anthropic only; ignored elsewhere).
    pub web: bool,
}

#[derive(Debug, Clone)]
pub struct Turn {
    /// The assistant message's content, to be echoed back verbatim.
    pub content: Vec<Value>,
    pub stop_reason: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Turn {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `(id, name, input)` for every tool the model called.
    pub fn tool_uses(&self) -> Vec<(String, String, Value)> {
        self.content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| {
                (
                    b["id"].as_str().unwrap_or("").to_string(),
                    b["name"].as_str().unwrap_or("").to_string(),
                    b["input"].clone(),
                )
            })
            .collect()
    }
}

pub enum Provider {
    Anthropic(Anthropic),
    Ollama(Ollama),
    Mock(Mock),
}

impl Provider {
    pub fn label(&self) -> String {
        match self {
            Provider::Anthropic(a) => a.model.clone(),
            Provider::Ollama(o) => format!("ollama:{}", o.model),
            Provider::Mock(_) => "mock".into(),
        }
    }

    pub async fn chat(&self, req: Request<'_>) -> anyhow::Result<Turn> {
        match self {
            Provider::Anthropic(a) => a.chat(req).await,
            Provider::Ollama(o) => o.chat(req).await,
            Provider::Mock(m) => m.chat(req),
        }
    }
}

// ---- Anthropic -----------------------------------------------------------------

pub struct Anthropic {
    pub api_key: String,
    pub model: String,
    pub effort: String,
    pub base_url: String,
    http: reqwest::Client,
    /// Which web tool generation the API accepted last (0 = newest, 1 = basic, 2 = none).
    web_variant: Mutex<u8>,
}

impl Anthropic {
    pub fn new(api_key: String, model: String, effort: String) -> Anthropic {
        Anthropic {
            api_key,
            model,
            effort,
            base_url: std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into()),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20 * 60))
                .build()
                .expect("reqwest client"),
            web_variant: Mutex::new(0),
        }
    }

    fn supports_fallbacks(&self) -> bool {
        let m = self.model.as_str();
        m.starts_with("claude-fable-5") || m.starts_with("claude-opus-5") || m.starts_with("claude-sonnet-5-5")
    }

    fn supports_adaptive(&self) -> bool {
        !self.model.contains("haiku") && !self.model.contains("-4-5") && !self.model.contains("-3-")
    }

    fn web_tools(variant: u8) -> Vec<Value> {
        match variant {
            0 => vec![
                json!({"type": "web_search_20260209", "name": "web_search", "max_uses": 8}),
                json!({"type": "web_fetch_20260209", "name": "web_fetch", "max_uses": 8}),
            ],
            1 => vec![
                json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 8}),
                json!({"type": "web_fetch_20250910", "name": "web_fetch", "max_uses": 8}),
            ],
            _ => vec![],
        }
    }

    async fn chat(&self, req: Request<'_>) -> anyhow::Result<Turn> {
        loop {
            let variant = *self.web_variant.lock().unwrap();
            let mut tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}))
                .collect();
            if req.web {
                tools.extend(Self::web_tools(variant));
            }
            let mut body = json!({
                "model": self.model,
                "max_tokens": 16000,
                "system": [{"type": "text", "text": req.system, "cache_control": {"type": "ephemeral"}}],
                "tools": tools,
                "messages": req.messages,
                "output_config": {"effort": self.effort},
            });
            if self.supports_adaptive() {
                body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
            }
            let mut request = self
                .http
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json");
            if self.supports_fallbacks() {
                body["fallbacks"] = json!("default");
                request = request.header("anthropic-beta", "server-side-fallback-2026-07-01");
            }
            let resp = request.json(&body).send().await?;
            let status = resp.status();
            let text = resp.text().await?;
            if status.as_u16() == 400 && req.web && variant < 2 && text.contains("web_") {
                // This model does not take this generation of the web tools; step down.
                *self.web_variant.lock().unwrap() = variant + 1;
                continue;
            }
            anyhow::ensure!(status.is_success(), "Anthropic API {status}: {}", summarize_error(&text));
            let v: Value = serde_json::from_str(&text)?;
            let stop_reason = v["stop_reason"].as_str().unwrap_or("end_turn").to_string();
            if stop_reason == "refusal" {
                let why = v["stop_details"]["explanation"].as_str().unwrap_or("the model declined this request");
                anyhow::bail!("refused: {why}");
            }
            return Ok(Turn {
                content: v["content"].as_array().cloned().unwrap_or_default(),
                stop_reason,
                model: v["model"].as_str().unwrap_or(&self.model).to_string(),
                input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0)
                    + v["usage"]["cache_read_input_tokens"].as_u64().unwrap_or(0)
                    + v["usage"]["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
            });
        }
    }
}

fn summarize_error(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| text.chars().take(300).collect())
}

// ---- Ollama --------------------------------------------------------------------

pub struct Ollama {
    pub url: String,
    pub model: String,
    http: reqwest::Client,
}

impl Ollama {
    pub fn new(url: String, model: String) -> Ollama {
        Ollama {
            url: url.trim_end_matches('/').to_string(),
            model,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30 * 60))
                .build()
                .expect("reqwest client"),
        }
    }

    /// Anthropic-shaped messages → Ollama's. Tool results become `tool`
    /// messages; images inside them ride on a following `user` message.
    fn convert(system: &str, messages: &[Value]) -> Vec<Value> {
        let mut out = vec![json!({"role": "system", "content": system})];
        for m in messages {
            let role = m["role"].as_str().unwrap_or("user");
            let blocks: Vec<Value> = match &m["content"] {
                Value::String(s) => vec![json!({"type": "text", "text": s})],
                Value::Array(a) => a.clone(),
                _ => vec![],
            };
            if role == "assistant" {
                let text: Vec<&str> = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
                let calls: Vec<Value> = blocks
                    .iter()
                    .filter(|b| b["type"] == "tool_use")
                    .map(|b| json!({"function": {"name": b["name"], "arguments": b["input"]}}))
                    .collect();
                let mut msg = json!({"role": "assistant", "content": text.join("\n")});
                if !calls.is_empty() {
                    msg["tool_calls"] = Value::Array(calls);
                }
                out.push(msg);
                continue;
            }
            let mut texts = Vec::new();
            let mut images = Vec::new();
            for b in &blocks {
                match b["type"].as_str() {
                    Some("text") => texts.push(b["text"].as_str().unwrap_or("").to_string()),
                    Some("image") => images.push(b["source"]["data"].clone()),
                    Some("tool_result") => {
                        let mut t = String::new();
                        match &b["content"] {
                            Value::String(s) => t.push_str(s),
                            Value::Array(parts) => {
                                for p in parts {
                                    if p["type"] == "text" {
                                        t.push_str(p["text"].as_str().unwrap_or(""));
                                    } else if p["type"] == "image" {
                                        images.push(p["source"]["data"].clone());
                                        t.push_str("\n[image attached below]");
                                    }
                                }
                            }
                            _ => {}
                        }
                        out.push(json!({"role": "tool", "content": t}));
                    }
                    _ => {}
                }
            }
            if !texts.is_empty() || !images.is_empty() {
                let mut msg = json!({"role": "user", "content": texts.join("\n")});
                if !images.is_empty() {
                    msg["images"] = Value::Array(images);
                }
                out.push(msg);
            }
        }
        out
    }

    async fn chat(&self, req: Request<'_>) -> anyhow::Result<Turn> {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.input_schema}}))
            .collect();
        let body = json!({
            "model": self.model,
            "messages": Self::convert(req.system, req.messages),
            "tools": tools,
            "stream": false,
            "options": {"num_ctx": 32768},
        });
        let resp = self.http.post(format!("{}/api/chat", self.url)).json(&body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        anyhow::ensure!(status.is_success(), "Ollama {status}: {}", summarize_error(&text));
        let v: Value = serde_json::from_str(&text)?;
        let msg = &v["message"];
        let mut content = Vec::new();
        if let Some(t) = msg["content"].as_str().filter(|t| !t.trim().is_empty()) {
            content.push(json!({"type": "text", "text": t}));
        }
        let mut calls = 0;
        if let Some(tc) = msg["tool_calls"].as_array() {
            for (i, c) in tc.iter().enumerate() {
                calls += 1;
                let input = match &c["function"]["arguments"] {
                    Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
                    other => other.clone(),
                };
                content.push(json!({
                    "type": "tool_use",
                    "id": format!("call_{}_{}", v["created_at"].as_str().unwrap_or("0").len(), i),
                    "name": c["function"]["name"],
                    "input": input,
                }));
            }
        }
        Ok(Turn {
            content,
            stop_reason: if calls > 0 { "tool_use".into() } else { "end_turn".into() },
            model: self.model.clone(),
            input_tokens: v["prompt_eval_count"].as_u64().unwrap_or(0),
            output_tokens: v["eval_count"].as_u64().unwrap_or(0),
        })
    }
}

// ---- Mock ----------------------------------------------------------------------

/// Scripted provider for tests: returns the queued turns in order and records
/// every request's messages, so a test can assert what the model was shown.
pub struct Mock {
    turns: Mutex<std::collections::VecDeque<Vec<Value>>>,
    pub seen: Mutex<Vec<Vec<Value>>>,
}

impl Mock {
    pub fn new(turns: Vec<Vec<Value>>) -> Mock {
        Mock { turns: Mutex::new(turns.into()), seen: Mutex::new(Vec::new()) }
    }

    fn chat(&self, req: Request<'_>) -> anyhow::Result<Turn> {
        self.seen.lock().unwrap().push(req.messages.to_vec());
        let content = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![json!({"type": "text", "text": "done"})]);
        let calls = content.iter().any(|b| b["type"] == "tool_use");
        Ok(Turn {
            content,
            stop_reason: if calls { "tool_use".into() } else { "end_turn".into() },
            model: "mock".into(),
            input_tokens: 0,
            output_tokens: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_conversion_flattens_blocks() {
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "reading"},
                {"type": "tool_use", "id": "t1", "name": "read_file", "input": {"path": "a"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "page"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAA"}}
                ]}
            ]}),
        ];
        let out = Ollama::convert("sys", &msgs);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[2]["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(out[3]["role"], "tool");
        assert_eq!(out[4]["images"][0], "AAA");
    }

    #[test]
    fn turn_helpers() {
        let t = Turn {
            content: vec![
                json!({"type": "text", "text": "a"}),
                json!({"type": "tool_use", "id": "1", "name": "x", "input": {"k": 1}}),
            ],
            stop_reason: "tool_use".into(),
            model: "m".into(),
            input_tokens: 0,
            output_tokens: 0,
        };
        assert_eq!(t.text(), "a");
        assert_eq!(t.tool_uses()[0].1, "x");
    }
}
