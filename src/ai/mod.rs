//! The AI: a prompt plus where the author is (cursor, selection, queued
//! regions, the last click in the PDF, the last compile) becomes an agent
//! run over a scratch copy of the project, whose result is a *proposal* — a
//! per-file before/after the person reviews, edits and applies. Applying
//! goes through the live document like any other change, recorded in the
//! history with the prompt and model, and committed to git.
//!
//! Three modes (`KLOVERLEAF_AI_MODE`):
//!
//! - `anthropic` (default when `ANTHROPIC_API_KEY` is set): one frontier
//!   model sees everything and has every tool, including server-side web
//!   search and fetch.
//! - `local`: the same, through Ollama.
//! - `dual`: the dual-LLM pattern. A *privileged* frontier model plans,
//!   searches the web and decides, but never receives a line of the document
//!   — only file names, sizes and error locations. A *quarantined* local
//!   model does all reading and writing, one `delegate` call at a time, and
//!   reports back only structurally (which files changed, whether it
//!   compiles). The boundary is enforced by [`tools::Scope`], not by prompting.
//!
//! ```text
//! client → server (inside {"type":"ai", …})
//!   {"action":"run","prompt","context":{file,line,column,selection,queue,pdf},"session"?}
//!                                        start a run, or continue the session's conversation
//!   {"action":"apply","session","files":[{path,text}]}   apply (possibly edited) files, commit, end
//!   {"action":"discard","session"}       throw the proposal away
//!   {"action":"cancel","session"}        stop a run in progress
//! server → client ({"type":"ai","session","user","event", …})
//!   started {prompt}  turn {role,text,model}  tool {role,name,input,summary,error}  
//!   proposal {text,files:[{path,before,after}],usage}  error {message}  cancelled  applied {files}  discarded
//! ```

pub mod provider;
pub mod tools;

use crate::build::{Diagnostic, Severity};
use crate::live::Project;
use crate::text;
use provider::{Provider, Request, Tool};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tools::{Scope, Workspace};

const MAX_TURNS: usize = 80;

pub enum Mode {
    Off(String),
    Single(Arc<Provider>),
    Dual { privileged: Arc<Provider>, quarantined: Arc<Provider> },
}

pub struct Ai {
    pub mode: Mode,
    pub http: reqwest::Client,
    sessions: Mutex<HashMap<u64, Arc<Session>>>,
    next: AtomicU64,
    latex_timeout: Duration,
}

pub struct Session {
    pub id: u64,
    pub project: Project,
    pub user: String,
    pub prompt: String,
    pub ws: Workspace,
    base: BTreeMap<String, String>,
    messages: Mutex<Vec<Value>>,
    cancel: AtomicBool,
    busy: AtomicBool,
    task: Mutex<Option<tokio::task::AbortHandle>>,
    model: Mutex<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Region {
    pub file: String,
    pub from: usize,
    pub to: usize,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct PdfClick {
    pub page: u32,
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Context {
    #[serde(default)]
    pub file: Option<String>,
    #[serde(default)]
    pub line: Option<usize>,
    #[serde(default)]
    pub column: Option<usize>,
    #[serde(default)]
    pub selection: Option<Region>,
    #[serde(default)]
    pub queue: Vec<Region>,
    #[serde(default)]
    pub pdf: Option<PdfClick>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProposedFile {
    pub path: String,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub session: u64,
    pub text: String,
    pub files: Vec<ProposedFile>,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Single,
    Privileged,
    Quarantined,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Single => "assistant",
            Role::Privileged => "coordinator",
            Role::Quarantined => "local",
        }
    }
}

impl Ai {
    pub fn from_env(latex_timeout: Duration) -> Ai {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let key = env("ANTHROPIC_API_KEY");
        let anthropic = || {
            key.clone().map(|k| {
                Arc::new(Provider::Anthropic(provider::Anthropic::new(
                    k,
                    env("KLOVERLEAF_AI_MODEL").unwrap_or_else(|| "claude-fable-5-1".into()),
                    env("KLOVERLEAF_AI_EFFORT").unwrap_or_else(|| "high".into()),
                )))
            })
        };
        let ollama = || {
            Arc::new(Provider::Ollama(provider::Ollama::new(
                env("OLLAMA_URL").unwrap_or_else(|| "http://127.0.0.1:11434".into()),
                env("KLOVERLEAF_LOCAL_MODEL").unwrap_or_else(|| "qwen3".into()),
            )))
        };
        let mode = match env("KLOVERLEAF_AI_MODE").as_deref().unwrap_or("anthropic") {
            "off" => Mode::Off("KLOVERLEAF_AI_MODE=off".into()),
            "local" => Mode::Single(ollama()),
            "dual" => match anthropic() {
                Some(p) => Mode::Dual { privileged: p, quarantined: ollama() },
                None => Mode::Off("dual mode needs ANTHROPIC_API_KEY for the privileged model".into()),
            },
            _ => match anthropic() {
                Some(p) => Mode::Single(p),
                None => Mode::Off("set ANTHROPIC_API_KEY (or KLOVERLEAF_AI_MODE=local with Ollama)".into()),
            },
        };
        Ai::new(mode, latex_timeout)
    }

    pub fn new(mode: Mode, latex_timeout: Duration) -> Ai {
        Ai {
            mode,
            http: reqwest::Client::builder().timeout(Duration::from_secs(60)).build().expect("reqwest"),
            sessions: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            latex_timeout,
        }
    }

    pub fn describe(&self) -> Value {
        match &self.mode {
            Mode::Off(reason) => json!({"mode": "off", "reason": reason}),
            Mode::Single(p) => json!({"mode": if matches!(**p, Provider::Ollama(_)) { "local" } else { "anthropic" }, "model": p.label()}),
            Mode::Dual { privileged, quarantined } => {
                json!({"mode": "dual", "model": privileged.label(), "local": quarantined.label()})
            }
        }
    }

    pub fn session(&self, id: u64) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(&id).cloned()
    }

    /// Starts a session: a scratch copy of the project as it is right now.
    pub fn new_session(&self, project: Project, user: &str, prompt: &str) -> anyhow::Result<Arc<Session>> {
        if let Mode::Off(reason) = &self.mode {
            anyhow::bail!("AI is off: {reason}");
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let base = project.texts();
        let dir = project.dir.join(crate::project::PRIVATE_DIR).join("ai").join(id.to_string());
        let ws = tools::make_workspace(&dir, &project.dir, &base, self.latex_timeout)?;
        let s = Arc::new(Session {
            id,
            project,
            user: user.into(),
            prompt: prompt.into(),
            ws,
            base,
            messages: Mutex::new(Vec::new()),
            cancel: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            task: Mutex::new(None),
            model: Mutex::new(String::new()),
        });
        self.sessions.lock().unwrap().insert(id, s.clone());
        Ok(s)
    }

    pub fn end_session(&self, id: u64) {
        if let Some(s) = self.sessions.lock().unwrap().remove(&id) {
            s.cancel.store(true, Ordering::Relaxed);
            if let Some(t) = s.task.lock().unwrap().take() {
                t.abort();
            }
            let _ = std::fs::remove_dir_all(&s.ws.dir);
        }
    }

    /// One user turn: the first prompt or a follow-up on the same scratch copy.
    pub async fn run(&self, s: &Session, prompt: &str, ctx: &Context) -> anyhow::Result<Outcome> {
        if s.busy.swap(true, Ordering::Relaxed) {
            anyhow::bail!("this session is already running");
        }
        s.cancel.store(false, Ordering::Relaxed);
        let result = self.run_inner(s, prompt, ctx).await;
        s.busy.store(false, Ordering::Relaxed);
        result
    }

    async fn run_inner(&self, s: &Session, prompt: &str, ctx: &Context) -> anyhow::Result<Outcome> {
        let (role, provider) = match &self.mode {
            Mode::Off(r) => anyhow::bail!("AI is off: {r}"),
            Mode::Single(p) => (Role::Single, p.clone()),
            Mode::Dual { privileged, .. } => (Role::Privileged, privileged.clone()),
        };
        let scope = if role == Role::Privileged { Scope::Metadata } else { Scope::Full };
        let diags = s.project.latest().map(|b| b.diagnostics.clone()).unwrap_or_default();
        let user_msg = format!("{}\n\n## Request\n\n{}", context_block(ctx, scope, &s.ws, &diags), prompt);
        let mut messages = s.messages.lock().unwrap().clone();
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": user_msg}]}));
        let (text, model, input_tokens, output_tokens) = self.run_loop(s, role, &provider, &mut messages, ctx).await?;
        *s.messages.lock().unwrap() = messages;
        *s.model.lock().unwrap() = model.clone();
        Ok(Outcome { session: s.id, text, files: self.proposal(s), model, input_tokens, output_tokens })
    }

    /// The workspace's text files that differ from the session's base.
    pub fn proposal(&self, s: &Session) -> Vec<ProposedFile> {
        let mut out = Vec::new();
        for (path, after) in s.ws.texts() {
            let before = s.base.get(&path).cloned().unwrap_or_default();
            if before != after {
                out.push(ProposedFile { path, before, after });
            }
        }
        out
    }

    async fn run_loop(
        &self,
        s: &Session,
        role: Role,
        provider: &Provider,
        messages: &mut Vec<Value>,
        ctx: &Context,
    ) -> anyhow::Result<(String, String, u64, u64)> {
        let (scope, web, network, system) = match role {
            Role::Single => (Scope::Full, true, true, SYSTEM_SINGLE),
            Role::Privileged => (Scope::Metadata, true, true, SYSTEM_PRIVILEGED),
            Role::Quarantined => (Scope::Full, false, false, SYSTEM_QUARANTINED),
        };
        let mut tool_defs = tools::definitions(scope, network);
        if role == Role::Privileged {
            tool_defs.push(delegate_tool());
        }
        let emit = |v: Value| {
            let mut v = v;
            v["session"] = json!(s.id);
            v["user"] = json!(s.user);
            s.project.emit_ai(v);
        };
        let (mut in_tok, mut out_tok) = (0u64, 0u64);
        let mut model;
        for _ in 0..MAX_TURNS {
            anyhow::ensure!(!s.cancel.load(Ordering::Relaxed), "cancelled");
            let turn = provider
                .chat(Request { system, tools: &tool_defs, messages, web })
                .await?;
            in_tok += turn.input_tokens;
            out_tok += turn.output_tokens;
            model = turn.model.clone();
            let text = turn.text();
            if !text.trim().is_empty() {
                emit(json!({"event": "turn", "role": role.name(), "text": text, "model": turn.model}));
            }
            messages.push(json!({"role": "assistant", "content": turn.content}));
            match turn.stop_reason.as_str() {
                "tool_use" => {
                    let mut results = Vec::new();
                    for (id, name, input) in turn.tool_uses() {
                        anyhow::ensure!(!s.cancel.load(Ordering::Relaxed), "cancelled");
                        emit(json!({"event": "tool", "role": role.name(), "name": name, "input": brief(&input), "summary": "…"}));
                        let out = if name == "delegate" && role == Role::Privileged {
                            self.delegate(s, &input, ctx).await
                        } else {
                            tools::execute(&s.ws, scope, &name, &input, &self.http).await
                        };
                        emit(json!({"event": "tool", "role": role.name(), "name": name, "input": brief(&input), "summary": out.summary, "error": out.is_error}));
                        results.push(json!({"type": "tool_result", "tool_use_id": id, "content": out.content, "is_error": out.is_error}));
                    }
                    messages.push(json!({"role": "user", "content": results}));
                }
                "pause_turn" => continue,
                "max_tokens" => {
                    messages.push(json!({"role": "user", "content": [{"type": "text", "text": "You stopped at the output limit. Continue from where you left off."}]}));
                }
                _ => return Ok((text, model, in_tok, out_tok)),
            }
        }
        anyhow::bail!("stopped after {MAX_TURNS} turns without finishing")
    }

    /// Dual mode: the privileged model's one way to touch the text. A fresh
    /// quarantined conversation per call; what comes back is structural.
    async fn delegate(&self, s: &Session, input: &Value, ctx: &Context) -> tools::Output {
        let Mode::Dual { quarantined, .. } = &self.mode else {
            return tools::Output { content: vec![json!({"type": "text", "text": "not in dual mode"})], is_error: true, summary: "delegate unavailable".into() };
        };
        let task = input["task"].as_str().unwrap_or("").trim().to_string();
        if task.is_empty() {
            return tools::Output { content: vec![json!({"type": "text", "text": "task is required"})], is_error: true, summary: "delegate: no task".into() };
        }
        let before = s.ws.texts();
        let diags = s.ws.compiled.lock().unwrap().clone().unwrap_or_default();
        let user_msg = format!(
            "{}\n\n## The author's original request\n\n{}\n\n## Your task from the coordinator\n\n{}",
            context_block(ctx, Scope::Full, &s.ws, &diags),
            s.prompt,
            task
        );
        let mut messages = vec![json!({"role": "user", "content": [{"type": "text", "text": user_msg}]})];
        let result = Box::pin(self.run_loop(s, Role::Quarantined, quarantined, &mut messages, ctx)).await;
        let after = s.ws.texts();
        let mut changed = Vec::new();
        for (path, text) in &after {
            let old = before.get(path).map(String::as_str).unwrap_or("");
            if old != text {
                changed.push(json!({
                    "path": path,
                    "lines_before": old.lines().count(),
                    "lines_after": text.lines().count(),
                    "new": !before.contains_key(path),
                }));
            }
        }
        let compiled = s.ws.compiled.lock().unwrap().clone().map(|d| {
            json!({
                "errors": d.iter().filter(|x| x.severity == Severity::Error).count(),
                "warnings": d.iter().filter(|x| x.severity == Severity::Warning).count(),
            })
        });
        match result {
            Ok(_) => {
                let n = changed.len();
                let body = json!({
                    "status": "finished",
                    "files_changed": changed,
                    "last_compile": compiled,
                    "note": "The local assistant's words are not shown to you by design; only this structural summary is.",
                });
                tools::Output { content: vec![json!({"type": "text", "text": body.to_string()})], is_error: false, summary: format!("delegated: {n} file(s) changed") }
            }
            Err(e) => tools::Output {
                content: vec![json!({"type": "text", "text": format!("the local assistant failed: {e:#}")})],
                is_error: true,
                summary: format!("delegate failed: {e:#}"),
            },
        }
    }

    /// Applies (possibly hand-edited) proposed files to the live project, commits, ends the session.
    pub async fn apply(&self, s: &Session, files: Vec<(String, String)>) -> anyhow::Result<Vec<String>> {
        anyhow::ensure!(s.project.take_lock(&s.user, false), "someone else holds the edit lock");
        let model = s.model.lock().unwrap().clone();
        let meta = json!({"session": s.id, "prompt": s.prompt, "model": model});
        let mut applied = Vec::new();
        for (path, text) in files {
            s.project.replace(&path, text, 0, &s.user, "ai", meta.clone())?;
            applied.push(path);
        }
        s.project.flush();
        let short: String = s.prompt.chars().take(60).collect::<String>().replace('\n', " ");
        let message = format!("ai: {short}{}", if s.prompt.chars().count() > 60 { "…" } else { "" });
        if let Ok(Some(sha)) = crate::gitd::commit(&s.project.dir, &message, Some(&s.user)).await {
            s.project.committed(&sha, &message, &s.user).await;
        }
        self.end_session(s.id);
        Ok(applied)
    }
}

fn delegate_tool() -> Tool {
    Tool {
        name: "delegate",
        description: "Hand a task to the local assistant, which can read and edit the document's text, compile it and look at the PDF — things you cannot do. Describe the task fully in plain language (what to change, where, in what style, what you found on the web); it also sees the author's request and context. You get back only which files changed and whether the project compiles.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"task": {"type": "string"}},
            "required": ["task"],
            "additionalProperties": false
        }),
    }
}

/// A short rendering of a tool's input for the activity log (no file contents).
fn brief(input: &Value) -> Value {
    let mut v = json!({});
    if let Some(o) = input.as_object() {
        for (k, val) in o {
            v[k] = match val {
                Value::String(s) if s.chars().count() > 80 => json!(format!("{}…", s.chars().take(80).collect::<String>())),
                Value::Array(a) => json!(format!("[{} items]", a.len())),
                other => other.clone(),
            };
        }
    }
    v
}

/// What the model is told about where the author is. In `Metadata` scope,
/// locations only; in `Full` scope, the text too, and the current file when small.
fn context_block(ctx: &Context, scope: Scope, ws: &Workspace, diags: &[Diagnostic]) -> String {
    let mut s = String::from("## Where the author is\n\n");
    let main = ws.main();
    let files: Vec<String> = ws
        .files()
        .iter()
        .map(|f| format!("{}{}", f.path, if main.as_deref() == Some(&f.path) { " (main)" } else { "" }))
        .collect();
    s.push_str(&format!("Project files: {}\n", files.join(", ")));
    if let (Some(file), Some(line)) = (&ctx.file, ctx.line) {
        s.push_str(&format!("Cursor: {file} line {line}{}\n", ctx.column.map(|c| format!(", column {c}")).unwrap_or_default()));
    }
    let region = |r: &Region, label: &str| -> String {
        let head = format!("{label}: {} lines {}-{}{}", r.file, r.from, r.to, r.note.as_ref().map(|n| format!(" — note: {n}")).unwrap_or_default());
        match scope {
            Scope::Full if !r.text.is_empty() => format!("{head}\n```\n{}\n```\n", r.text),
            _ => format!("{head}\n"),
        }
    };
    if let Some(sel) = &ctx.selection {
        s.push_str(&region(sel, "Selection"));
    }
    if !ctx.queue.is_empty() {
        s.push_str("\nQueued regions — the author marked these for attention, in order:\n");
        for (i, r) in ctx.queue.iter().enumerate() {
            s.push_str(&region(r, &format!("{}.", i + 1)));
        }
    }
    if let Some(p) = &ctx.pdf {
        s.push_str(&format!(
            "Last click in the PDF: page {}{}\n",
            p.page,
            match (&p.file, p.line) {
                (Some(f), Some(l)) => format!(", which SyncTeX maps to {f} line {l}"),
                _ => String::new(),
            }
        ));
    }
    let errors: Vec<&Diagnostic> = diags.iter().filter(|d| d.severity == Severity::Error).collect();
    let warnings = diags.iter().filter(|d| d.severity == Severity::Warning).count();
    if errors.is_empty() && diags.is_empty() {
        s.push_str("Last compile: no information yet\n");
    } else if errors.is_empty() {
        s.push_str(&format!("Last compile: succeeded, {warnings} warning(s)\n"));
    } else {
        s.push_str(&format!("Last compile: {} error(s), {warnings} warning(s)\n", errors.len()));
        for d in errors.iter().take(10) {
            match scope {
                Scope::Full => s.push_str(&format!("  - {}:{}: {}\n", d.file, d.line.unwrap_or(0), d.message)),
                Scope::Metadata => s.push_str(&format!("  - {}:{}\n", d.file, d.line.unwrap_or(0))),
            }
        }
    }
    if scope == Scope::Full
        && let Some(file) = &ctx.file
        && let Ok(text) = ws.read(file)
        && text.len() <= 60_000
    {
        s.push_str(&format!("\n## Current file: {file}\n\n{}", text::numbered(&text, 1, usize::MAX)));
    }
    s
}

const SYSTEM_SINGLE: &str = "You are the writing assistant inside kloverleaf, a LaTeX editor. The author is working on the document in front of them and has asked you for something; the message tells you where they are (cursor, selection, regions they queued for you, the last place they clicked in the PDF) and how the last compile went.

You work on a scratch copy of the project. Read what you need, make the edits with write_edits (exact, unique `old` strings — read first, then copy exactly), compile to check, and look at the PDF page when layout, figures or math matter. The author will review your changes as a diff before anything is applied, so be decisive: do the work rather than describing it. Keep the author's voice, macros and conventions; change what was asked and what is clearly needed to make it work, not more. Use the web when you need a citation, a package's syntax, or a fact. If the request is ambiguous in a way that matters, pick the reasonable reading and say what you assumed.

When you are done, reply briefly: what you changed and why, anything you could not do, and anything the author should check.";

const SYSTEM_PRIVILEGED: &str = "You are the coordinator inside kloverleaf, a LaTeX editor, working in dual-LLM mode: the document is private and you never see its text — only file names, sizes and the locations of compile errors. A local assistant, which does see the text, carries out tasks you give it with `delegate`, and reports back only which files changed and whether the project compiles.

Your job is to understand the author's request, do any research it needs (web search and fetch are yours), and write the local assistant a complete, self-contained task: what to change, where (file and lines are in the context), how, in what style, including any facts, citations or snippets you found. Prefer one well-specified delegate call over many small ones. Compile to confirm the result. Then reply briefly to the author: what you asked for, what came back, and anything to check. Never ask the local assistant to send you the document's text.";

const SYSTEM_QUARANTINED: &str = "You are the local assistant inside kloverleaf, a LaTeX editor. A coordinator that cannot see the document has given you a task, along with the author's original request and where they are in the document. Carry out the task on the scratch copy of the project: read what you need, make the edits with write_edits (exact, unique `old` strings — read first, then copy exactly), compile to check, and fix what you broke. Keep the author's voice, macros and conventions. You have no web access; if the task needs a fact you do not have, do the part you can and say what is missing. When done, reply briefly with what you changed.";

/// The WebSocket dispatcher: every action runs in its own task so the socket
/// loop is never blocked; results come back as events on the project.
pub fn handle(app: crate::Shared, project: Project, user: String, _conn: u64, v: Value) {
    let action = v["action"].as_str().unwrap_or("").to_string();
    let session_id = v["session"].as_u64();
    let emit = {
        let p = project.clone();
        let u = user.clone();
        move |mut ev: Value| {
            if let Some(id) = session_id {
                ev["session"] = json!(id);
            }
            ev["user"] = json!(u);
            p.emit_ai(ev);
        }
    };
    match action.as_str() {
        "run" => {
            let prompt = v["prompt"].as_str().unwrap_or("").trim().to_string();
            if prompt.is_empty() {
                emit(json!({"event": "error", "message": "empty prompt"}));
                return;
            }
            let ctx: Context = serde_json::from_value(v["context"].clone()).unwrap_or_default();
            let session = match session_id {
                Some(id) => match app.ai.session(id) {
                    Some(s) if s.project.name == project.name => Ok(s),
                    _ => Err(anyhow::anyhow!("no such session")),
                },
                None => app.ai.new_session(project.clone(), &user, &prompt),
            };
            let s = match session {
                Ok(s) => s,
                Err(e) => {
                    emit(json!({"event": "error", "message": format!("{e:#}")}));
                    return;
                }
            };
            let app2 = app.clone();
            let s2 = s.clone();
            let task = tokio::spawn(async move {
                let emit = |mut ev: Value| {
                    ev["session"] = json!(s2.id);
                    ev["user"] = json!(s2.user);
                    s2.project.emit_ai(ev);
                };
                emit(json!({"event": "started", "prompt": prompt, "mode": app2.ai.describe()}));
                match app2.ai.run(&s2, &prompt, &ctx).await {
                    Ok(o) => emit(json!({"event": "proposal", "text": o.text, "files": o.files, "model": o.model,
                                         "usage": {"input": o.input_tokens, "output": o.output_tokens}})),
                    Err(e) => emit(json!({"event": "error", "message": format!("{e:#}")})),
                }
            });
            *s.task.lock().unwrap() = Some(task.abort_handle());
        }
        "apply" => {
            let Some(s) = session_id.and_then(|id| app.ai.session(id)) else {
                emit(json!({"event": "error", "message": "no such session"}));
                return;
            };
            let files: Vec<(String, String)> = v["files"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|f| Some((f["path"].as_str()?.to_string(), f["text"].as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            tokio::spawn(async move {
                let emit = |mut ev: Value| {
                    ev["session"] = json!(s.id);
                    ev["user"] = json!(s.user);
                    s.project.emit_ai(ev);
                };
                match app.ai.apply(&s, files).await {
                    Ok(applied) => emit(json!({"event": "applied", "files": applied})),
                    Err(e) => emit(json!({"event": "error", "message": format!("{e:#}")})),
                }
            });
        }
        "discard" => {
            if let Some(id) = session_id {
                app.ai.end_session(id);
                emit(json!({"event": "discarded"}));
            }
        }
        "cancel" => {
            if let Some(s) = session_id.and_then(|id| app.ai.session(id)) {
                s.cancel.store(true, Ordering::Relaxed);
                if let Some(t) = s.task.lock().unwrap().take() {
                    t.abort();
                }
                s.busy.store(false, Ordering::Relaxed);
                emit(json!({"event": "cancelled"}));
            }
        }
        _ => emit(json!({"event": "error", "message": format!("unknown action {action:?}")})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use provider::Mock;

    async fn project() -> (std::path::PathBuf, Project) {
        let root = crate::history::tmp_dir("ai");
        std::fs::create_dir_all(root.join("projects")).unwrap();
        let p = Project::create(&root, "p", Duration::from_secs(30)).await.unwrap();
        p.replace("main.tex", "\\documentclass{article}\n\\begin{document}\nSECRET sentence.\n\\end{document}\n".into(), 0, "kris", "replace", json!({})).unwrap();
        (root, p)
    }

    #[tokio::test]
    async fn single_mode_produces_a_proposal_and_applies_it() {
        let (root, p) = project().await;
        let mock = Mock::new(vec![
            vec![json!({"type": "tool_use", "id": "t1", "name": "write_edits", "input": {"path": "main.tex", "edits": [{"old": "SECRET sentence.", "new": "Public sentence."}]}})],
            vec![json!({"type": "text", "text": "Replaced the sentence."})],
        ]);
        let ai = Ai::new(Mode::Single(Arc::new(Provider::Mock(mock))), Duration::from_secs(30));
        let s = ai.new_session(p.clone(), "kris", "make it public").unwrap();
        let ctx = Context { file: Some("main.tex".into()), line: Some(3), ..Default::default() };
        let o = ai.run(&s, "make it public", &ctx).await.unwrap();
        assert_eq!(o.text, "Replaced the sentence.");
        assert_eq!(o.files.len(), 1);
        assert!(o.files[0].after.contains("Public sentence."));
        assert!(p.text("main.tex").unwrap().contains("SECRET"), "nothing applied yet");
        let files = o.files.iter().map(|f| (f.path.clone(), f.after.clone())).collect();
        ai.apply(&s, files).await.unwrap();
        assert!(p.text("main.tex").unwrap().contains("Public sentence."));
        assert!(ai.session(s.id).is_none());
        let head = p.history.entry(p.history.head("main.tex").unwrap()).unwrap();
        assert_eq!(head.kind, "ai");
        assert_eq!(head.meta["prompt"], "make it public");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn dual_mode_keeps_text_away_from_the_privileged_model() {
        let (root, p) = project().await;
        let privileged = Arc::new(Provider::Mock(Mock::new(vec![
            vec![json!({"type": "tool_use", "id": "r", "name": "read_file", "input": {"path": "main.tex"}})],
            vec![json!({"type": "tool_use", "id": "d", "name": "delegate", "input": {"task": "Replace the secret sentence with a public one."}})],
            vec![json!({"type": "text", "text": "Done via the local assistant."})],
        ])));
        let quarantined = Arc::new(Provider::Mock(Mock::new(vec![
            vec![json!({"type": "tool_use", "id": "q1", "name": "write_edits", "input": {"path": "main.tex", "edits": [{"old": "SECRET sentence.", "new": "Public sentence."}]}})],
            vec![json!({"type": "text", "text": "I replaced SECRET."})],
        ])));
        let ai = Ai::new(Mode::Dual { privileged: privileged.clone(), quarantined: quarantined.clone() }, Duration::from_secs(30));
        let s = ai.new_session(p.clone(), "kris", "make it public").unwrap();
        let ctx = Context {
            file: Some("main.tex".into()),
            line: Some(3),
            selection: Some(Region { file: "main.tex".into(), from: 3, to: 3, text: "SECRET sentence.".into(), note: None }),
            ..Default::default()
        };
        let o = ai.run(&s, "make it public", &ctx).await.unwrap();
        assert_eq!(o.files.len(), 1);
        assert!(o.files[0].after.contains("Public sentence."));
        let Provider::Mock(m) = &*privileged else { unreachable!() };
        let shown = serde_json::to_string(&*m.seen.lock().unwrap()).unwrap();
        assert!(!shown.contains("SECRET"), "privileged model saw document text: {shown}");
        assert!(shown.contains("lines 3-3"), "privileged model should still know locations");
        assert!(shown.contains("not a text file") || shown.contains("unknown tool") || shown.contains("error"), "read_file must be refused in metadata scope");
        let Provider::Mock(q) = &*quarantined else { unreachable!() };
        let qshown = serde_json::to_string(&*q.seen.lock().unwrap()).unwrap();
        assert!(qshown.contains("SECRET sentence."), "quarantined model gets the text");
        std::fs::remove_dir_all(root).unwrap();
    }
}
