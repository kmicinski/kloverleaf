//! The second agent: OpenAI's `codex` CLI, run the way the notes and maestro
//! apps on the home server run it (`codex exec --json`, ChatGPT OAuth from
//! `$CODEX_HOME/auth.json`). It reaches the project the same way claude does
//! — the loopback MCP server — through a `[mcp_servers.kloverleaf]` entry in
//! `$CODEX_HOME/config.toml` that we write at startup; the bearer it sends
//! (`KLOVERLEAF_MCP_BEARER`, per process) carries the session id, so each run
//! only sees its own workspace. Codex has no system-prompt flag, so the
//! instructions ride at the top of the prompt. Its thread id resumes
//! follow-ups.
//!
//! The run is not sandboxed (`--dangerously-bypass-approvals-and-sandbox`,
//! as the sibling apps do: the kernel sandbox is unavailable in the
//! container). Its cwd is an empty scratch dir and the instructions say to
//! use only the kloverleaf tools; the live project is never its cwd.

use super::Session;
use super::cli::{self, Turn};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub const DEFAULT_MODELS: &str = "gpt-6.1-sol,gpt-6-sol,gpt-6-astra,gpt-5.6-sol,gpt-5.5";
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
const LOGIN_HINT: &str = "codex's login looks invalid: run `codex login` on the host so ~/.codex/auth.json is fresh";

#[derive(Clone)]
pub struct Runner {
    pub bin: String,
    pub model: String,
    pub effort: String,
    /// `$CODEX_HOME`: config.toml, auth.json, and the thread transcripts.
    pub home: PathBuf,
}

/// Where codex keeps itself: `$CODEX_HOME`, else `$HOME/.codex`.
pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("CODEX_HOME").filter(|h| !h.is_empty()) {
        return h.into();
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())).join(".codex")
}

fn scratch(home: &Path) -> PathBuf {
    home.join("kloverleaf-scratch")
}

/// `config.toml` for codex: our MCP server (bearer from the env) and trust
/// for the scratch cwd. Written once at startup, overwriting what was there —
/// this HOME belongs to kloverleaf.
pub fn write_config(home: &Path, port: u16) -> std::io::Result<()> {
    std::fs::create_dir_all(scratch(home))?;
    let toml = format!(
        "# written by kloverleaf at startup — edits here are overwritten\n\
         [mcp_servers.kloverleaf]\nurl = \"http://127.0.0.1:{port}/mcp\"\nbearer_token_env_var = \"KLOVERLEAF_MCP_BEARER\"\n\n\
         [projects.\"{}\"]\ntrust_level = \"trusted\"\n",
        scratch(home).display()
    );
    std::fs::write(home.join("config.toml"), toml)
}

pub fn available(home: &Path) -> Result<(), String> {
    if !home.join("auth.json").is_file() {
        return Err(format!("no codex login: {} is missing (mount ~/.codex/auth.json there)", home.join("auth.json").display()));
    }
    Ok(())
}

pub async fn run(runner: &Runner, session: &Session, message: &str, system: &str, token: &str) -> anyhow::Result<Turn> {
    let resume = cli::resume_id(session);
    match run_once(runner, session, message, system, token, resume.as_deref()).await {
        Err(e) if cli::stale_resume(session, &runner.model, resume.is_some(), &e) => run_once(runner, session, message, system, token, None).await,
        other => other,
    }
}

async fn run_once(runner: &Runner, session: &Session, message: &str, system: &str, token: &str, resume: Option<&str>) -> anyhow::Result<Turn> {
    let prompt = format!("## Instructions\n\n{system}\n\n{message}");
    let mut cmd = Command::new(&runner.bin);
    cmd.arg("exec");
    if resume.is_some() {
        cmd.arg("resume");
    }
    cmd.args(["--json", "--model", &runner.model, "--dangerously-bypass-approvals-and-sandbox", "--skip-git-repo-check"]);
    cmd.arg("-c").arg(format!("model_reasoning_effort=\"{}\"", runner.effort));
    match resume {
        Some(id) => cmd.arg(id),
        None => cmd.arg("--cd").arg(scratch(&runner.home)),
    };
    cmd.arg(&prompt)
        .env("CODEX_HOME", &runner.home)
        .env("KLOVERLEAF_MCP_BEARER", format!("{token}:{}", session.key()))
        .current_dir(scratch(&runner.home));
    cli::drive("codex", session, cmd, None, &runner.model, LOGIN_HINT, |ev, turn, progress| {
        match ev["type"].as_str().unwrap_or("") {
            "thread.started" => {
                if let Some(id) = ev["thread_id"].as_str() {
                    *session.agent_session.lock().unwrap() = Some(id.to_string());
                }
            }
            "item.started" => {
                let item = &ev["item"];
                match item["type"].as_str().unwrap_or("") {
                    "reasoning" => session.emit(json!({"event": "phase", "text": "thinking…"})),
                    "mcp_tool_call" => session.emit(json!({"event": "phase", "text": format!("{}…", item["tool"].as_str().unwrap_or("tool"))})),
                    "command_execution" => session.emit(json!({"event": "tool", "role": "assistant", "name": "shell",
                        "input": {"command": item["command"].as_str().unwrap_or("")}, "summary": "ran a command (not sandboxed)"})),
                    "web_search" => session.emit(json!({"event": "tool", "role": "assistant", "name": "web_search",
                        "input": {"query": item["query"].as_str().unwrap_or("")}, "summary": "web"})),
                    _ => {}
                }
            }
            "item.completed" => {
                let item = &ev["item"];
                if item["type"] == "agent_message"
                    && let Some(t) = item["text"].as_str()
                    && !t.trim().is_empty()
                {
                    turn.text = t.trim().to_string();
                    session.emit(json!({"event": "turn", "role": "assistant", "text": turn.text, "model": runner.model}));
                }
            }
            "turn.completed" => {
                progress.done = true;
                let u = &ev["usage"];
                turn.input_tokens = u["input_tokens"].as_u64().unwrap_or(0) + u["cached_input_tokens"].as_u64().unwrap_or(0);
                turn.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
            }
            "turn.failed" => {
                progress.done = true;
                progress.error = Some(ev["error"]["message"].as_str().unwrap_or("codex turn failed").to_string());
            }
            "error" => {
                progress.error = Some(ev["message"].as_str().unwrap_or("codex error").to_string());
            }
            _ => {}
        }
    })
    .await
}
