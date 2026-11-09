//! Running the agent: one headless `claude` (the Claude Code CLI) per turn,
//! the same way the slides and notes apps on the home server do it. The CLI
//! is given no tools of its own — its hands are the loopback MCP server in
//! `mcp.rs`, scoped to the session's scratch workspace — plus Anthropic's
//! server-side web search and fetch. It talks on stdout as `stream-json`;
//! we turn that into the AI panel's events (`turn`, `phase`, `tool`) and the
//! final result. A turn is a conversation: the CLI's `session_id` is kept on
//! the session and the next prompt resumes it, so follow-ups remember.
//!
//! Auth is the CLI's own: `CLAUDE_CODE_OAUTH_TOKEN` (from `claude setup-token`)
//! or `ANTHROPIC_API_KEY` in the environment. `HOME` must be writable — the
//! CLI keeps its transcripts there, which is what makes `--resume` work.

use super::cli::{self, Turn};
use super::{Session, mcp};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub const DEFAULT_MODEL: &str = "claude-opus-5";
pub const DEFAULT_MODELS: &str = "claude-opus-5-5,claude-opus-5,claude-sonnet-5-5,claude-haiku-4-5-20251001,claude-fable-5-1";
pub const DEFAULT_EFFORT: &str = "medium";
pub const EFFORTS: [&str; 4] = ["low", "medium", "high", "xhigh"];
const MAX_TURNS: u32 = 60;
const LOGIN_HINT: &str = "the CLI's login looks invalid: regenerate CLAUDE_CODE_OAUTH_TOKEN (`claude setup-token`) or set ANTHROPIC_API_KEY";

#[derive(Clone)]
pub struct Runner {
    /// The CLI binary — `claude` on PATH, or a full path (tests use a script).
    pub bin: String,
    pub model: String,
    pub effort: String,
}

fn mcp_config(session: &Session, port: u16, token: &str) -> std::io::Result<PathBuf> {
    let cfg = json!({"mcpServers": {"kloverleaf": {
        "type": "http",
        "url": format!("http://127.0.0.1:{port}/mcp"),
        "headers": {
            "Authorization": format!("Bearer {token}"),
            (mcp::SESSION_HEADER): session.key(),
        },
    }}});
    let path = std::env::temp_dir().join(format!("kloverleaf-mcp-{}-{}.json", std::process::id(), session.id));
    std::fs::write(&path, cfg.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// A short name for the activity log: `mcp__kloverleaf__compile` → `compile`.
fn short(name: &str) -> &str {
    name.rsplit("__").next().unwrap_or(name)
}

/// One turn. `message` is the full user message (context + request).
pub async fn run(runner: &Runner, session: &Session, message: &str, system: &str, port: u16, token: &str) -> anyhow::Result<Turn> {
    let cfg = mcp_config(session, port, token)?;
    let resume = cli::resume_id(session);
    let result = match run_once(runner, session, message, system, &cfg, resume.as_deref()).await {
        Err(e) if cli::stale_resume(session, &runner.model, resume.is_some(), &e) => run_once(runner, session, message, system, &cfg, None).await,
        other => other,
    };
    let _ = std::fs::remove_file(&cfg);
    result
}

async fn run_once(runner: &Runner, session: &Session, message: &str, system: &str, cfg: &Path, resume: Option<&str>) -> anyhow::Result<Turn> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        "--output-format".into(), "stream-json".into(), "--verbose".into(),
        "--include-partial-messages".into(),
        "--model".into(), runner.model.clone(),
        "--effort".into(), runner.effort.clone(),
        "--disable-slash-commands".into(),
        "--mcp-config".into(), cfg.to_string_lossy().into_owned(), "--strict-mcp-config".into(),
        "--allowedTools".into(), "mcp__kloverleaf,WebSearch,WebFetch".into(),
        "--disallowedTools".into(),
        "Bash,Task,Agent,Skill,TodoWrite,NotebookEdit,Write,Edit,MultiEdit,Read,Glob,Grep,LS,KillShell,BashOutput".into(),
        "--max-turns".into(), MAX_TURNS.to_string(),
        "--append-system-prompt".into(), system.to_string(),
    ];
    if let Some(sid) = resume {
        args.push("--resume".into());
        args.push(sid.to_string());
    }
    // cwd is $HOME, never the workspace: the CLI must not leave its own files
    // (CLAUDE.md, settings) where they would show up in the proposal.
    let home = std::env::var("HOME").unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    let mut cmd = Command::new(&runner.bin);
    cmd.args(&args).current_dir(&home);
    // The prompt goes in on stdin: a message starting with "-" would be read
    // as a flag, and argv has a length limit.
    let mut last_text = String::new();
    cli::drive("claude", session, cmd, Some(message), &runner.model, LOGIN_HINT, |ev, turn, progress| {
        match ev["type"].as_str().unwrap_or("") {
            "system" => {
                if ev["subtype"] == "init"
                    && let Some(sid) = ev["session_id"].as_str()
                {
                    *session.agent_session.lock().unwrap() = Some(sid.to_string());
                }
            }
            "stream_event" => {
                let e = &ev["event"];
                if e["type"] == "content_block_start" {
                    let b = &e["content_block"];
                    let phase = match b["type"].as_str().unwrap_or("") {
                        "thinking" => Some("thinking…".to_string()),
                        "tool_use" => Some(format!("{}…", short(b["name"].as_str().unwrap_or("tool")))),
                        "server_tool_use" => Some(format!("{}…", b["name"].as_str().unwrap_or("web"))),
                        "text" => Some("writing…".to_string()),
                        _ => None,
                    };
                    if let Some(phase) = phase {
                        session.emit(json!({"event": "phase", "text": phase}));
                    }
                }
            }
            "assistant" => {
                if let Some(m) = ev["message"]["model"].as_str() {
                    turn.model = m.to_string();
                }
                for block in ev["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str().unwrap_or("") {
                        "text" => {
                            let t = block["text"].as_str().unwrap_or("").trim().to_string();
                            if !t.is_empty() {
                                last_text = t.clone();
                                session.emit(json!({"event": "turn", "role": "assistant", "text": t, "model": turn.model}));
                            }
                        }
                        // MCP tools report themselves from mcp.rs; the CLI's web tools do not.
                        "tool_use" | "server_tool_use" => {
                            let name = block["name"].as_str().unwrap_or("");
                            if !name.starts_with("mcp__") {
                                session.emit(json!({"event": "tool", "role": "assistant", "name": short(name),
                                                    "input": super::brief(&block["input"]), "summary": "web"}));
                            }
                        }
                        _ => {}
                    }
                }
            }
            "result" => {
                progress.done = true;
                if let Some(sid) = ev["session_id"].as_str() {
                    *session.agent_session.lock().unwrap() = Some(sid.to_string());
                }
                let u = &ev["usage"];
                turn.input_tokens = u["input_tokens"].as_u64().unwrap_or(0)
                    + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
                    + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                turn.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                let text = ev["result"].as_str().unwrap_or("").trim().to_string();
                if ev["is_error"].as_bool().unwrap_or(false) {
                    let sub = ev["subtype"].as_str().unwrap_or("error");
                    progress.error = Some(if text.is_empty() { sub.to_string() } else { text });
                } else {
                    turn.text = if text.is_empty() { last_text.clone() } else { text };
                }
            }
            _ => {}
        }
    })
    .await
}
