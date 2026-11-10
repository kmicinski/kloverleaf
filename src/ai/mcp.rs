//! The agent's hands: a loopback MCP server (JSON-RPC 2.0 over HTTP at
//! `/mcp`) whose tools act on one session's scratch [`Workspace`]. The headless
//! `claude` run in `claude.rs` is told about it through a per-run config file
//! carrying a bearer token (random, per process — the endpoint is never meant
//! for anything but the local CLI) and an `X-Kloverleaf-Session` header that
//! names the session, so each run only ever sees its own copy of the project.
//!
//! Every call is also reported to the editor as a `tool` event (pending, then
//! done with a one-line summary), which is the activity log in the AI panel.

use super::tools::Workspace;
use crate::build;
use crate::text;
use crate::{Shared, project};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;

pub const SESSION_HEADER: &str = "x-kloverleaf-session";
const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Deserialize)]
struct Request {
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

fn ok(id: Value, result: Value) -> Response {
    axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn err(id: Value, code: i64, message: impl Into<String>) -> Response {
    axum::Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})).into_response()
}

/// A tool's reply: content blocks for the model, a one-line summary for the log.
#[derive(Debug)]
pub struct Reply {
    pub content: Vec<Value>,
    pub summary: String,
}

fn text(s: impl Into<String>, summary: impl Into<String>) -> Result<Reply, String> {
    Ok(Reply { content: vec![json!({"type": "text", "text": s.into()})], summary: summary.into() })
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

pub fn catalog() -> Vec<Value> {
    vec![
        json!({"name": "list_files",
               "description": "List every file in the project with its size; the main .tex file is marked. Call this first if you do not know the layout.",
               "inputSchema": schema(json!({}), &[])}),
        json!({"name": "read_file",
               "description": "Read a text file (LaTeX, BibTeX, …) with line numbers. Give `from`/`to` (1-based, inclusive) to read a range; without them a long file returns its first 400 lines.",
               "inputSchema": schema(json!({
                   "path": {"type": "string", "description": "Path relative to the project root, e.g. chapters/intro.tex"},
                   "from": {"type": "integer", "minimum": 1},
                   "to": {"type": "integer", "minimum": 1}
               }), &["path"])}),
        json!({"name": "search_files",
               "description": "Search every text file for a pattern (a regular expression unless `literal` is true). Returns up to 100 matching lines as path:line: text.",
               "inputSchema": schema(json!({
                   "pattern": {"type": "string"},
                   "literal": {"type": "boolean", "description": "Match the pattern as plain text"}
               }), &["pattern"])}),
        json!({"name": "write_edits",
               "description": "Edit a text file by exact replacement. Each edit's `old` must occur exactly once in the file (include enough surrounding lines to make it unique) and is replaced by `new`. To create a file, or replace one wholesale, give one edit with `old` set to \"\" — then `new` becomes the whole content. Edits apply in order; the call fails as a whole if any `old` is missing or ambiguous, and nothing is written.",
               "inputSchema": schema(json!({
                   "path": {"type": "string"},
                   "edits": {"type": "array", "minItems": 1,
                             "items": schema(json!({"old": {"type": "string"}, "new": {"type": "string"}}), &["old", "new"])}
               }), &["path", "edits"])}),
        json!({"name": "delete_file",
               "description": "Delete a text file from the project (the proposal shows it as removed).",
               "inputSchema": schema(json!({"path": {"type": "string"}}), &["path"])}),
        json!({"name": "list_commits",
               "description": "The project's commits, newest first: id, date, author, message. Without `upstream`, the project's own history (kloverleaf commits: AI applies, pulls, pushes, manual commits; add `autosaves` to include the 30-second autosaves). With `upstream: true`, the history of the GitHub repository the project tracks (only commits touching its directory). Use this to find the commit to give `restore_commit`.",
               "inputSchema": schema(json!({
                   "upstream": {"type": "boolean"},
                   "autosaves": {"type": "boolean"},
                   "n": {"type": "integer", "minimum": 1, "maximum": 300, "description": "How many (default 40)"}
               }), &[])}),
        json!({"name": "restore_commit",
               "description": "Go back to a commit: make the project's text files exactly what they were at `commit` (an id or unique prefix from list_commits, from the project's history or the tracked GitHub repository). Files the commit has are written, files it lacks are deleted. Give `paths` to restore only those files. Binary files (images, PDFs) are left alone. Like every edit, this only changes your copy; the author reviews the result as a proposal.",
               "inputSchema": schema(json!({
                   "commit": {"type": "string"},
                   "paths": {"type": "array", "items": {"type": "string"}}
               }), &["commit"])}),
        json!({"name": "compile",
               "description": "Run latexmk on the project and return whether it succeeded and the diagnostics (file, line, severity, message). Do this after editing to check your work.",
               "inputSchema": schema(json!({}), &[])}),
        json!({"name": "view_pdf_page",
               "description": "Look at one page of the compiled PDF as an image, to check layout, figures, tables or math as the reader sees them. Pages are 1-based. Compile first if you changed something.",
               "inputSchema": schema(json!({"page": {"type": "integer", "minimum": 1}}), &["page"])}),
        json!({"name": "pdf_text",
               "description": "The text of the compiled PDF (one page, or all pages without `page`), as extracted by pdftotext — what actually rendered, with hyphenation and numbering resolved.",
               "inputSchema": schema(json!({"page": {"type": "integer", "minimum": 1}}), &[])}),
    ]
}

/// Runs one tool against a workspace. `Err` is a message for the model
/// (`isError: true`), never a transport failure.
pub async fn execute(ws: &Workspace, name: &str, input: &Value) -> Result<Reply, String> {
    let s = |k: &str| input[k].as_str().map(str::to_string);
    let n = |k: &str| input[k].as_u64().map(|v| v as usize);
    match name {
        "list_files" => {
            let main = ws.main();
            let lines: Vec<String> = ws
                .files()
                .iter()
                .map(|f| {
                    format!(
                        "{}  ({} bytes{}{})",
                        f.path,
                        f.size,
                        if f.text { "" } else { ", binary" },
                        if main.as_deref() == Some(&f.path) { ", main" } else { "" }
                    )
                })
                .collect();
            text(lines.join("\n"), format!("listed {} files", lines.len()))
        }
        "read_file" => {
            let path = s("path").ok_or("path is required")?;
            let t = ws.read(&path).map_err(|e| format!("{e:#}"))?;
            let total = t.lines().count();
            let (from, to) = match (n("from"), n("to")) {
                (None, None) if total > 400 => (1, 400),
                (f, t) => (f.unwrap_or(1), t.unwrap_or(total)),
            };
            let mut out = text::numbered(&t, from, to);
            if to < total || from > 1 {
                out.push_str(&format!("[lines {from}-{} of {total}]\n", to.min(total)));
            }
            text(out, format!("read {path}:{from}-{}", to.min(total)))
        }
        "search_files" => {
            let pattern = s("pattern").ok_or("pattern is required")?;
            let re = if input["literal"].as_bool().unwrap_or(false) {
                regex::Regex::new(&regex::escape(&pattern))
            } else {
                regex::Regex::new(&pattern)
            }
            .map_err(|e| format!("bad pattern: {e}"))?;
            let mut hits = Vec::new();
            'files: for (path, t) in ws.texts() {
                for (i, line) in t.lines().enumerate() {
                    if re.is_match(line) {
                        hits.push(format!("{path}:{}: {}", i + 1, line.trim_end()));
                        if hits.len() >= 100 {
                            break 'files;
                        }
                    }
                }
            }
            let count = hits.len();
            text(if hits.is_empty() { "no matches".to_string() } else { hits.join("\n") }, format!("searched: {count} hit(s)"))
        }
        "write_edits" => {
            let path = s("path").ok_or("path is required")?;
            let edits = input["edits"].as_array().ok_or("edits is required")?;
            if !project::valid_path(&path) || !project::is_text(&path) {
                return Err(format!("{path} is not a text file path"));
            }
            let mut t = ws.read(&path).unwrap_or_default();
            for (i, e) in edits.iter().enumerate() {
                let (old, new) = (e["old"].as_str().unwrap_or(""), e["new"].as_str().unwrap_or(""));
                if old.is_empty() {
                    t = new.to_string();
                    continue;
                }
                match t.matches(old).count() {
                    1 => t = t.replacen(old, new, 1),
                    0 => return Err(format!("edit {}: `old` not found in {path}; read the file again and copy the text exactly", i + 1)),
                    k => return Err(format!("edit {}: `old` occurs {k} times in {path}; include more context to make it unique", i + 1)),
                }
            }
            ws.write(&path, &t).map_err(|e| format!("{e:#}"))?;
            text(format!("wrote {path} ({} lines)", t.lines().count()), format!("edited {path} ({} edit(s))", edits.len()))
        }
        "delete_file" => {
            let path = s("path").ok_or("path is required")?;
            ws.remove(&path).map_err(|e| format!("{e:#}"))?;
            text(format!("deleted {path}"), format!("deleted {path}"))
        }
        "list_commits" => {
            let n = n("n").unwrap_or(40).clamp(1, 300);
            let upstream = input["upstream"].as_bool().unwrap_or(false);
            let commits = if upstream {
                crate::upstream::log(&ws.project, n).await
            } else if input["autosaves"].as_bool().unwrap_or(false) {
                crate::gitd::log(&ws.project, n).await
            } else {
                let count = format!("-{n}");
                crate::gitd::log_of(&ws.project, &["log", &count, "--invert-grep", "--grep=^autosave$"]).await
            }
            .map_err(|e| format!("{e:#}"))?;
            let lines: Vec<String> = commits
                .iter()
                .map(|c| {
                    format!("{}  {}  {}  {}", &c.sha[..c.sha.len().min(10)], c.date, c.author, c.message)
                })
                .collect();
            let which = if upstream { "upstream" } else { "project" };
            text(if lines.is_empty() { "no commits".into() } else { lines.join("\n") }, format!("listed {} {which} commit(s)", lines.len()))
        }
        "restore_commit" => {
            let rev = s("commit").ok_or("commit is required")?;
            let (sha, from, then) = match crate::gitd::texts_at(&ws.project, &rev).await {
                Ok((sha, t)) => (sha, "project", t),
                Err(local) => match crate::upstream::texts_at(&ws.project, &rev).await {
                    Ok((sha, t)) => (sha, "upstream", t),
                    Err(_) => return Err(format!("no commit {rev} in the project's history or the tracked repository ({local:#}); list_commits shows what exists")),
                },
            };
            let now = ws.texts();
            let only: Option<Vec<String>> = input["paths"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect());
            let wanted = |p: &str| only.as_ref().is_none_or(|o| o.iter().any(|x| x == p));
            let (mut written, mut removed) = (Vec::new(), Vec::new());
            for (path, t) in &then {
                if wanted(path) && now.get(path) != Some(t) {
                    ws.write(path, t).map_err(|e| format!("{e:#}"))?;
                    written.push(path.clone());
                }
            }
            for path in now.keys() {
                if wanted(path) && !then.contains_key(path) && path != ".gitignore" {
                    ws.remove(path).map_err(|e| format!("{e:#}"))?;
                    removed.push(path.clone());
                }
            }
            if let Some(o) = &only {
                let missing: Vec<&String> = o.iter().filter(|p| !then.contains_key(*p) && !now.contains_key(*p)).collect();
                if !missing.is_empty() {
                    return Err(format!("not in commit {sha:.10} nor the project: {missing:?}"));
                }
            }
            let short = &sha[..10];
            let mut out = format!("restored {from} commit {short}: {} file(s) written, {} deleted", written.len(), removed.len());
            for p in &written {
                out.push_str(&format!("\n  wrote {p}"));
            }
            for p in &removed {
                out.push_str(&format!("\n  deleted {p}"));
            }
            text(out, format!("restored {short}: {} written, {} deleted", written.len(), removed.len()))
        }
        "compile" => {
            let main = ws.main().ok_or("no main .tex file")?;
            let c = build::compile(&ws.dir, &main, ws.latex_timeout).await.map_err(|e| format!("could not compile: {e:#}"))?;
            let errors = c.diagnostics.iter().filter(|d| d.severity == build::Severity::Error).count();
            let warnings = c.diagnostics.iter().filter(|d| d.severity == build::Severity::Warning).count();
            let body = json!({"ok": c.ok, "ms": c.ms, "pdf": c.pdf.is_some() && !c.stale, "diagnostics": c.diagnostics});
            text(body.to_string(), format!("compiled in {}ms: {errors} error(s), {warnings} warning(s)", c.ms))
        }
        "view_pdf_page" => {
            let pdf = ws.pdf().ok_or("no PDF yet: compile first")?;
            let page = n("page").unwrap_or(1) as u32;
            let pages = build::page_count(&pdf).await;
            if page == 0 || page > pages {
                return Err(format!("page {page} does not exist; the PDF has {pages} page(s)"));
            }
            let png = build::render_page(&pdf, page, 110).await.map_err(|e| format!("{e:#}"))?;
            Ok(Reply {
                content: vec![
                    json!({"type": "text", "text": format!("page {page} of {pages}")}),
                    json!({"type": "image", "mimeType": "image/png",
                           "data": base64::engine::general_purpose::STANDARD.encode(&png)}),
                ],
                summary: format!("viewed page {page}/{pages}"),
            })
        }
        "pdf_text" => {
            let pdf = ws.pdf().ok_or("no PDF yet: compile first")?;
            let page = n("page").map(|p| p as u32);
            let t = build::page_text(&pdf, page).await.map_err(|e| format!("{e:#}"))?;
            let t: String = t.chars().take(60_000).collect();
            text(t, match page { Some(p) => format!("read PDF text of page {p}"), None => "read PDF text".into() })
        }
        other => Err(format!("unknown tool {other}")),
    }
}

/// The bearer is `<token>` or `<token>:<session>` — codex can only send a
/// bearer (no custom headers), so the session may ride in it. Returns the
/// session named that way, if any.
fn bearer_ok(expected: &str, headers: &HeaderMap) -> Result<Option<String>, ()> {
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");
    let (token, session) = match provided.split_once(':') {
        Some((t, s)) => (t, Some(s.to_string())),
        None => (provided, None),
    };
    if token.len() == expected.len() && bool::from(token.as_bytes().ct_eq(expected.as_bytes())) { Ok(session) } else { Err(()) }
}

/// `POST /mcp`. Not behind the app's auth gate: the bearer token is the auth.
pub async fn handler(State(app): State<Shared>, headers: HeaderMap, body: String) -> Response {
    let Ok(bearer_session) = bearer_ok(&app.ai.mcp_token, &headers) else {
        return (StatusCode::UNAUTHORIZED, "invalid bearer token").into_response();
    };
    let req: Request = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return err(Value::Null, -32700, format!("parse error: {e}")),
    };
    let Some(id) = req.id else {
        return (StatusCode::ACCEPTED, "").into_response(); // a notification
    };
    match req.method.as_str() {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "kloverleaf", "version": env!("CARGO_PKG_VERSION")},
            }),
        ),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({"tools": catalog()})),
        "resources/list" => ok(id, json!({"resources": []})),
        "prompts/list" => ok(id, json!({"prompts": []})),
        "tools/call" => {
            // The session key is `project:id`, in the header (claude) or the bearer (codex).
            let session = headers
                .get(SESSION_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .or(bearer_session)
                .and_then(|key| {
                    let (project, id) = key.rsplit_once(':')?;
                    app.ai.session(project, id.parse().ok()?)
                });
            let Some(s) = session else {
                return ok(id, json!({"content": [{"type": "text", "text": "no such session"}], "isError": true}));
            };
            let name = req.params["name"].as_str().unwrap_or("").to_string();
            let input = req.params["arguments"].clone();
            // The pending row, then the same row done (the panel replaces it by name).
            let tool_event = |summary: String, error: bool| {
                s.emit(json!({"event": "tool", "role": "assistant", "name": name, "input": super::brief(&input), "summary": summary, "error": error}));
            };
            tool_event("…".into(), false);
            let result = match execute(&s.ws, &name, &input).await {
                Ok(r) => {
                    tool_event(r.summary, false);
                    if name == "write_edits" {
                        // Show the edit as it lands: the proposal so far, as a draft.
                        s.emit(json!({"event": "draft", "files": app.ai.proposal(&s)}));
                    }
                    json!({"content": r.content, "isError": false})
                }
                Err(m) => {
                    tool_event(format!("error: {m}"), true);
                    json!({"content": [{"type": "text", "text": m}], "isError": true})
                }
            };
            ok(id, result)
        }
        m => err(id, -32601, format!("method not found: {m}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ws() -> Workspace {
        let dir = crate::history::tmp_dir("mcp");
        std::fs::write(dir.join("main.tex"), "\\documentclass{article}\n\\begin{document}\nHello\nHello\n\\end{document}\n").unwrap();
        Workspace { project: dir.clone(), dir, latex_timeout: Duration::from_secs(30) }
    }

    #[tokio::test]
    async fn edits_require_unique_matches() {
        let w = ws();
        let amb = execute(&w, "write_edits", &json!({"path": "main.tex", "edits": [{"old": "Hello", "new": "Bye"}]})).await;
        assert!(amb.as_ref().err().is_some_and(|m| m.contains("2 times")), "{amb:?}");
        let ok = execute(&w, "write_edits", &json!({"path": "main.tex", "edits": [{"old": "Hello\nHello", "new": "Bye"}]})).await;
        assert!(ok.is_ok());
        assert!(w.read("main.tex").unwrap().contains("Bye\n\\end"));
        let new = execute(&w, "write_edits", &json!({"path": "refs.bib", "edits": [{"old": "", "new": "@book{k, title={T}}"}]})).await;
        assert!(new.is_ok());
        assert_eq!(w.read("refs.bib").unwrap(), "@book{k, title={T}}");
        let bad = execute(&w, "write_edits", &json!({"path": "../x.tex", "edits": [{"old": "", "new": ""}]})).await;
        assert!(bad.is_err());
        let search = execute(&w, "search_files", &json!({"pattern": "doc.*class"})).await.unwrap();
        assert!(search.summary.contains("1 hit"));
        let read = execute(&w, "read_file", &json!({"path": "main.tex", "from": 2, "to": 2})).await.unwrap();
        assert!(read.content[0]["text"].as_str().unwrap().contains("begin{document}"));
        assert!(catalog().iter().all(|t| t["inputSchema"]["type"] == "object"));
        std::fs::remove_dir_all(&w.dir).unwrap();
    }
}
