//! The AI: a prompt plus where the author is (cursor, selection, queued
//! regions, the last click in the PDF, the last compile) becomes an agent
//! run over a scratch copy of the project, whose result is a *proposal* — a
//! per-file before/after the person reviews, edits and applies. Applying
//! goes through the live document like any other change, recorded in the
//! history with the prompt and model, and committed to git.
//!
//! The agent is a headless `claude` (the Claude Code CLI, `claude.rs`), the
//! way the slides and notes apps on the home server run theirs. Its only
//! tools are this server's loopback MCP endpoint (`mcp.rs`) — list, read and
//! search files, exact edits, compile, look at a PDF page, read the PDF's
//! text — all against the session's workspace, plus Anthropic's server-side
//! web search and fetch. Follow-ups resume the CLI's own conversation.
//!
//! ```text
//! client → server (inside {"type":"ai", …})
//!   {"action":"run","prompt","context":{file,line,column,selection,queue,pdf},"session"?}
//!                                        start a run, or continue the session's conversation
//!   {"action":"apply","session","files":[{path,text}]}   apply (possibly edited) files, commit, end
//!   {"action":"discard","session"}       throw the proposal away
//!   {"action":"cancel","session"}        stop a run in progress
//! server → client ({"type":"ai","session","user","event", …})
//!   started {prompt,mode}  phase {text}  turn {role,text,model}  tool {role,name,input,summary,error}
//!   proposal {text,files:[{path,before,after}],model,usage}  error {message}  cancelled  applied {files}  discarded
//! ```

pub mod claude;
pub mod cli;
pub mod codex;
pub mod mcp;
pub mod tools;

use crate::build::{Diagnostic, Severity};
use crate::live::Project;
use crate::text;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tools::Workspace;

/// Which CLI runs the turn. Both reach the project only through `/mcp`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
        }
    }
}

/// What the author picked for a turn: the agent, its model, how hard it thinks.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Choice {
    pub provider: Provider,
    pub model: String,
    pub effort: String,
}

/// A request's (partial) choice; missing parts fall back to the defaults.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ChoiceReq {
    #[serde(default)]
    pub provider: Option<Provider>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
}

/// One installed, logged-in agent CLI and what it may be asked for.
#[derive(Clone, Debug)]
pub struct ProviderInfo {
    pub bin: String,
    pub models: Vec<String>,
    pub efforts: Vec<&'static str>,
    /// Codex's `$CODEX_HOME`; unused for claude.
    pub home: PathBuf,
}

/// Everything the picker can offer.
#[derive(Clone, Debug)]
pub struct Catalog {
    pub claude: Result<ProviderInfo, String>,
    pub codex: Result<ProviderInfo, String>,
    pub default: Choice,
}

impl Catalog {
    pub fn info(&self, p: Provider) -> Result<&ProviderInfo, String> {
        match p {
            Provider::Claude => self.claude.as_ref().map_err(|e| e.clone()),
            Provider::Codex => self.codex.as_ref().map_err(|e| e.clone()),
        }
    }

    fn info_mut(&mut self, p: Provider) -> Option<&mut ProviderInfo> {
        match p {
            Provider::Claude => self.claude.as_mut().ok(),
            Provider::Codex => self.codex.as_mut().ok(),
        }
    }

    pub fn off_reason(&self) -> Option<String> {
        match (&self.claude, &self.codex) {
            (Err(a), Err(b)) => Some(format!("claude: {a}; codex: {b}")),
            _ => None,
        }
    }

    /// Fills in and validates a request against what is installed.
    pub fn resolve(&self, req: &ChoiceReq) -> anyhow::Result<Choice> {
        if let Some(r) = self.off_reason() {
            anyhow::bail!("AI is off: {r}");
        }
        let provider = req.provider.unwrap_or(self.default.provider);
        let info = self.info(provider).map_err(|e| anyhow::anyhow!("{} is unavailable: {e}", provider.label()))?;
        let model = match &req.model {
            Some(m) => m.clone(),
            None if provider == self.default.provider => self.default.model.clone(),
            None => info.models.first().cloned().unwrap_or_default(),
        };
        anyhow::ensure!(model.chars().all(|c| c.is_ascii_alphanumeric() || "-._".contains(c)), "bad model id");
        anyhow::ensure!(info.models.contains(&model), "unknown {} model {model:?}", provider.label());
        let effort = match &req.effort {
            Some(e) => e.clone(),
            None if info.efforts.contains(&self.default.effort.as_str()) => self.default.effort.clone(),
            None => "medium".into(),
        };
        anyhow::ensure!(info.efforts.contains(&effort.as_str()), "unknown effort {effort:?} for {}", provider.label());
        Ok(Choice { provider, model, effort })
    }
}

/// One conversation with the agent, as kept on disk (`.kloverleaf/ai/threads.json`)
/// so it survives reloads and restarts. The scratch workspace and the base
/// texts (`.kloverleaf/ai/<id>/`, `<id>.base.json`) live beside it until the
/// proposal is applied or discarded.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Thread {
    pub id: u64,
    pub user: String,
    pub title: String,
    pub choice: Choice,
    #[serde(default)]
    pub agent_session: Option<String>,
    /// idle · running · applied · discarded · interrupted (server restarted mid-run)
    pub status: String,
    pub created: i64,
    pub updated: i64,
    #[serde(default)]
    pub messages: Vec<Value>,
    #[serde(default)]
    pub base_heads: BTreeMap<String, i64>,
}

#[derive(Default, Serialize, Deserialize)]
struct ThreadFile {
    next: u64,
    threads: Vec<Thread>,
}

/// A project's conversations, loaded once, written after every change.
pub struct ThreadStore {
    path: PathBuf,
    file: ThreadFile,
}

const MAX_THREADS: usize = 40;
const MAX_MESSAGES: usize = 400;

impl ThreadStore {
    fn load(project: &Project) -> ThreadStore {
        let path = ai_dir(project).join("threads.json");
        let mut file: ThreadFile = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        if file.next == 0 {
            file.next = 1;
        }
        // A run the server died in the middle of: say so, instead of "running…" forever.
        for t in file.threads.iter_mut().filter(|t| t.status == "running") {
            t.status = "interrupted".into();
            t.messages.push(json!({"event": "error", "message": "this run was interrupted (the server restarted); ask again to continue"}));
        }
        ThreadStore { path, file }
    }

    fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json.tmp");
        if serde_json::to_string(&self.file).ok().and_then(|s| std::fs::write(&tmp, s).ok()).is_some() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    fn thread_mut(&mut self, id: u64) -> Option<&mut Thread> {
        self.file.threads.iter_mut().find(|t| t.id == id)
    }

    fn record(&mut self, id: u64, ev: &Value) {
        let kind = ev["event"].as_str().unwrap_or("");
        // Live-only chatter is not kept: phases, pending tool rows, drafts.
        if kind == "phase" || kind == "draft" || (kind == "tool" && ev["summary"] == "…") {
            return;
        }
        if let Some(t) = self.thread_mut(id) {
            let mut e = ev.clone();
            if let Some(o) = e.as_object_mut() {
                o.remove("session");
                o.remove("user");
                if kind == "proposal" {
                    // The diff is recomputed from the workspace on reload; keep only the paths.
                    let paths: Vec<Value> = o.get("files").and_then(|f| f.as_array()).map(|a| a.iter().map(|f| f["path"].clone()).collect()).unwrap_or_default();
                    o.insert("files".into(), Value::Array(paths));
                }
            }
            t.messages.push(e);
            if t.messages.len() > MAX_MESSAGES {
                let n = t.messages.len() - MAX_MESSAGES;
                t.messages.drain(..n);
            }
            t.updated = crate::history::now_ms();
            match kind {
                "started" => t.status = "running".into(),
                "proposal" | "error" | "cancelled" => t.status = "idle".into(),
                "applied" => t.status = "applied".into(),
                "discarded" => t.status = "discarded".into(),
                _ => {}
            }
            self.save();
        }
    }
}

pub struct Ai {
    pub catalog: Catalog,
    sessions: Mutex<HashMap<(String, u64), Arc<Session>>>,
    threads: Mutex<HashMap<String, Arc<Mutex<ThreadStore>>>>,
    /// Projects seen, by name — to rebuild a session from disk.
    projects: Mutex<HashMap<String, Project>>,
    latex_timeout: Duration,
    /// Bearer for `/mcp`; random per process, known only to the CLIs we spawn.
    pub mcp_token: String,
    /// The port this server listens on, for the loopback MCP URL.
    pub mcp_port: u16,
}

pub struct Session {
    pub id: u64,
    pub project: Project,
    pub user: String,
    pub prompt: String,
    pub ws: Workspace,
    base: BTreeMap<String, String>,
    /// Each file's history head when the session began: the version the prompt addressed.
    base_heads: BTreeMap<String, i64>,
    /// The project's conversation log, where every event of this session is kept.
    log: Arc<Mutex<ThreadStore>>,
    /// The agent for this conversation (the provider is fixed; model/effort may change per turn).
    pub choice: Mutex<Choice>,
    /// The CLI's own session/thread id, resumed on follow-ups.
    pub agent_session: Mutex<Option<String>>,
    /// The CLI process while a turn runs, so cancel can kill it.
    pub child: Mutex<Option<tokio::process::Child>>,
    pub cancel: AtomicBool,
    busy: AtomicBool,
    task: Mutex<Option<tokio::task::AbortHandle>>,
    model: Mutex<String>,
}

impl Session {
    /// A session over an existing workspace: fresh (`new_session`) or brought
    /// back from the project's thread log after a restart (`Ai::session`).
    #[allow(clippy::too_many_arguments)]
    fn new(id: u64, project: Project, thread: &Thread, ws: Workspace, base: BTreeMap<String, String>, log: Arc<Mutex<ThreadStore>>) -> Session {
        Session {
            id,
            project,
            user: thread.user.clone(),
            prompt: thread.title.clone(),
            ws,
            base,
            base_heads: thread.base_heads.clone(),
            log,
            choice: Mutex::new(thread.choice.clone()),
            agent_session: Mutex::new(thread.agent_session.clone()),
            child: Mutex::new(None),
            cancel: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            task: Mutex::new(None),
            model: Mutex::new(thread.choice.model.clone()),
        }
    }

    /// An AI event about this session: to every open editor of the project, and into the log.
    pub fn emit(&self, mut ev: Value) {
        ev["session"] = json!(self.id);
        ev["user"] = json!(self.user);
        self.log.lock().unwrap().record(self.id, &ev);
        if let Some(sid) = self.agent_session.lock().unwrap().clone()
            && let Some(t) = self.log.lock().unwrap().thread_mut(self.id)
            && t.agent_session.as_deref() != Some(sid.as_str())
        {
            t.agent_session = Some(sid);
        }
        self.project.emit_ai(ev);
    }

    /// `project:id`, how the MCP endpoint is told which workspace a call is for.
    pub fn key(&self) -> String {
        format!("{}:{}", self.project.name, self.id)
    }

    fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(t) = self.task.lock().unwrap().take() {
            t.abort();
        }
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.start_kill();
        }
        self.busy.store(false, Ordering::Relaxed);
    }
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

/// Where a project's sessions live: the thread log, each workspace, each base.
fn ai_dir(project: &Project) -> PathBuf {
    project.dir.join(crate::project::PRIVATE_DIR).join("ai")
}

/// The texts a session started from, kept until it is applied or discarded.
fn base_file(project: &Project, id: u64) -> PathBuf {
    ai_dir(project).join(format!("{id}.base.json"))
}

fn list(env: Option<String>, default: &str) -> Vec<String> {
    env.as_deref().unwrap_or(default).split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

impl Ai {
    /// `KLOVERLEAF_AI_MODE=off` disables everything. Otherwise each CLI is
    /// offered if it is installed and logged in: claude (`KLOVERLEAF_CLAUDE_BIN`,
    /// `CLAUDE_CODE_OAUTH_TOKEN` or `ANTHROPIC_API_KEY`) and codex
    /// (`KLOVERLEAF_CODEX_BIN`, `$CODEX_HOME/auth.json`). Model lists come from
    /// `KLOVERLEAF_CLAUDE_MODELS` / `KLOVERLEAF_CODEX_MODELS`; the default pick
    /// from `KLOVERLEAF_AI_PROVIDER`, `KLOVERLEAF_AI_MODEL`, `KLOVERLEAF_AI_EFFORT`.
    pub fn from_env(latex_timeout: Duration, bind: &str) -> Ai {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let off = env("KLOVERLEAF_AI_MODE").as_deref() == Some("off");
        let port = bind.rsplit(':').next().and_then(|p| p.parse().ok()).unwrap_or(7200);

        let claude_bin = env("KLOVERLEAF_CLAUDE_BIN").unwrap_or_else(|| "claude".into());
        let claude = if off {
            Err("KLOVERLEAF_AI_MODE=off".to_string())
        } else if cli::find(&claude_bin).is_none() {
            Err(format!("the `{claude_bin}` CLI is not installed"))
        } else if env("CLAUDE_CODE_OAUTH_TOKEN").is_none() && env("ANTHROPIC_API_KEY").is_none() {
            Err("no credentials: set CLAUDE_CODE_OAUTH_TOKEN (`claude setup-token`) or ANTHROPIC_API_KEY".to_string())
        } else {
            Ok(ProviderInfo { bin: claude_bin, models: list(env("KLOVERLEAF_CLAUDE_MODELS"), claude::DEFAULT_MODELS), efforts: claude::EFFORTS.to_vec(), home: PathBuf::new() })
        };

        let codex_bin = env("KLOVERLEAF_CODEX_BIN").unwrap_or_else(|| "codex".into());
        let home = codex::home();
        let codex = if off {
            Err("KLOVERLEAF_AI_MODE=off".to_string())
        } else if cli::find(&codex_bin).is_none() {
            Err(format!("the `{codex_bin}` CLI is not installed"))
        } else if let Err(e) = codex::available(&home) {
            Err(e)
        } else if let Err(e) = codex::write_config(&home, port) {
            Err(format!("could not write {}: {e}", home.join("config.toml").display()))
        } else {
            Ok(ProviderInfo { bin: codex_bin, models: list(env("KLOVERLEAF_CODEX_MODELS"), codex::DEFAULT_MODELS), efforts: codex::EFFORTS.to_vec(), home })
        };

        let mut default = Choice {
            provider: match env("KLOVERLEAF_AI_PROVIDER").as_deref() {
                Some("codex") => Provider::Codex,
                _ => Provider::Claude,
            },
            model: env("KLOVERLEAF_AI_MODEL").unwrap_or_else(|| claude::DEFAULT_MODEL.into()),
            effort: env("KLOVERLEAF_AI_EFFORT").unwrap_or_else(|| claude::DEFAULT_EFFORT.into()),
        };
        let mut catalog = Catalog { claude, codex, default: default.clone() };
        // Fall back to whichever agent is actually there, and make sure the
        // configured default model is on offer (an operator may name a model
        // the list does not know yet).
        if catalog.info(default.provider).is_err() {
            default.provider = if catalog.claude.is_ok() { Provider::Claude } else { Provider::Codex };
            default.model = catalog.info(default.provider).ok().and_then(|i| i.models.first().cloned()).unwrap_or_default();
        }
        if let Some(i) = catalog.info_mut(default.provider) {
            if !i.models.contains(&default.model) {
                i.models.insert(0, default.model.clone());
            }
            if !i.efforts.contains(&default.effort.as_str()) {
                default.effort = "medium".into();
            }
        }
        catalog.default = default;
        Ai::new(catalog, latex_timeout, port)
    }

    pub fn new(catalog: Catalog, latex_timeout: Duration, mcp_port: u16) -> Ai {
        Ai {
            catalog,
            sessions: Mutex::new(HashMap::new()),
            threads: Mutex::new(HashMap::new()),
            projects: Mutex::new(HashMap::new()),
            latex_timeout,
            mcp_token: text::random_hex(32),
            mcp_port,
        }
    }

    /// What the picker shows: the agents on offer, their models and efforts, the default.
    pub fn describe(&self) -> Value {
        let c = &self.catalog;
        if let Some(reason) = c.off_reason() {
            return json!({"mode": "off", "reason": reason});
        }
        let mut providers = Vec::new();
        let mut unavailable = serde_json::Map::new();
        for p in [Provider::Claude, Provider::Codex] {
            match c.info(p) {
                Ok(i) => providers.push(json!({"id": p, "label": p.label(), "models": i.models, "efforts": i.efforts})),
                Err(e) => {
                    unavailable.insert(p.label().to_lowercase(), json!(e));
                }
            }
        }
        json!({"mode": "on", "default": c.default, "providers": providers, "unavailable": unavailable,
               // kept for older clients / logs
               "model": c.default.model, "effort": c.default.effort})
    }

    /// Registers a project (so sessions can be rebuilt from disk) and loads
    /// its conversations; interrupted runs are marked on load.
    pub fn remember(&self, project: &Project) {
        self.projects.lock().unwrap().insert(project.name.clone(), project.clone());
        let _ = self.log_for(project);
    }

    /// The project's conversation log (loaded on first use).
    pub fn log_for(&self, project: &Project) -> Arc<Mutex<ThreadStore>> {
        let mut map = self.threads.lock().unwrap();
        map.entry(project.name.clone()).or_insert_with(|| Arc::new(Mutex::new(ThreadStore::load(project)))).clone()
    }

    /// The conversations of a project, newest first, without their messages.
    pub fn threads(&self, project: &Project) -> Vec<Value> {
        let log = self.log_for(project);
        let g = log.lock().unwrap();
        let mut out: Vec<Value> = g
            .file
            .threads
            .iter()
            .map(|t| json!({"id": t.id, "title": t.title, "user": t.user, "status": t.status, "choice": t.choice, "created": t.created, "updated": t.updated, "messages": t.messages.len()}))
            .collect();
        out.reverse();
        out
    }

    /// One conversation with its messages, plus the live proposal if its workspace is still there.
    pub fn thread(&self, project: &Project, id: u64) -> Option<Value> {
        let log = self.log_for(project);
        let t = log.lock().unwrap().file.threads.iter().find(|t| t.id == id).cloned()?;
        let proposal = self.session(&project.name, id).map(|s| self.proposal(&s));
        Some(json!({"thread": t, "proposal": proposal}))
    }

    /// A live session, or one brought back from disk (its workspace and base
    /// are still there until it is applied or discarded).
    pub fn session(&self, project: &str, id: u64) -> Option<Arc<Session>> {
        if let Some(s) = self.sessions.lock().unwrap().get(&(project.to_string(), id)) {
            return Some(s.clone());
        }
        let p = self.projects.lock().unwrap().get(project)?.clone();
        let log = self.log_for(&p);
        let t = log.lock().unwrap().file.threads.iter().find(|t| t.id == id && (t.status == "idle" || t.status == "interrupted"))?.clone();
        let dir = ai_dir(&p).join(id.to_string());
        if !dir.is_dir() {
            return None;
        }
        let base: BTreeMap<String, String> = std::fs::read_to_string(base_file(&p, id)).ok().and_then(|s| serde_json::from_str(&s).ok())?;
        let ws = Workspace { dir, latex_timeout: self.latex_timeout };
        let s = Arc::new(Session::new(id, p, &t, ws, base, log));
        self.sessions.lock().unwrap().insert((project.to_string(), id), s.clone());
        Some(s)
    }

    /// Starts a session: a scratch copy of the project as it is right now, for one agent.
    pub fn new_session(&self, project: Project, user: &str, prompt: &str, choice: Choice) -> anyhow::Result<Arc<Session>> {
        if let Some(reason) = self.catalog.off_reason() {
            anyhow::bail!("AI is off: {reason}");
        }
        self.remember(&project);
        let log = self.log_for(&project);
        let id = {
            let mut g = log.lock().unwrap();
            let id = g.file.next;
            g.file.next += 1;
            id
        };
        let base = project.texts();
        let base_heads: BTreeMap<String, i64> = base.keys().filter_map(|f| project.history.head(f).map(|h| (f.clone(), h))).collect();
        let ws = tools::make_workspace(&ai_dir(&project).join(id.to_string()), &project.dir, &base, self.latex_timeout)?;
        std::fs::write(base_file(&project, id), serde_json::to_string(&base)?)?;
        let now = crate::history::now_ms();
        let thread = Thread {
            id,
            user: user.into(),
            title: prompt.chars().take(120).collect(),
            choice,
            agent_session: None,
            status: "idle".into(),
            created: now,
            updated: now,
            messages: vec![],
            base_heads,
        };
        {
            let mut g = log.lock().unwrap();
            g.file.threads.push(thread.clone());
            // Old finished conversations fall off the end.
            while g.file.threads.len() > MAX_THREADS {
                if let Some(i) = g.file.threads.iter().position(|t| t.status == "applied" || t.status == "discarded") {
                    g.file.threads.remove(i);
                } else {
                    break;
                }
            }
            g.save();
        }
        let name = project.name.clone();
        let s = Arc::new(Session { prompt: prompt.into(), model: Mutex::new(String::new()), ..Session::new(id, project, &thread, ws, base, log) });
        self.sessions.lock().unwrap().insert((name, id), s.clone());
        Ok(s)
    }

    pub fn end_session(&self, project: &str, id: u64) {
        if let Some(s) = self.sessions.lock().unwrap().remove(&(project.to_string(), id)) {
            s.stop();
            let _ = std::fs::remove_dir_all(&s.ws.dir);
            let _ = std::fs::remove_file(base_file(&s.project, id));
        }
    }

    /// Throws a proposal away: tells the editors (if the session is known) and
    /// removes its workspace. Returns whether there was a session to tell.
    pub fn discard(&self, project: &str, id: u64) -> bool {
        let known = self.session(project, id).map(|s| s.emit(json!({"event": "discarded"}))).is_some();
        self.end_session(project, id);
        known
    }

    /// One turn with its events — `started`, then `proposal` or `error` — so
    /// the socket panel and the synchronous API tell the same story.
    pub async fn turn(&self, s: &Session, prompt: &str, ctx: &Context, choice: Choice) -> anyhow::Result<Outcome> {
        s.emit(json!({"event": "started", "prompt": prompt, "mode": self.describe(), "choice": choice}));
        let result = self.run(s, prompt, ctx, Some(choice)).await;
        match &result {
            Ok(o) => s.emit(json!({"event": "proposal", "text": o.text, "files": o.files, "model": o.model,
                                   "usage": {"input": o.input_tokens, "output": o.output_tokens}})),
            Err(e) => s.emit(json!({"event": "error", "message": format!("{e:#}")})),
        }
        result
    }

    /// One user turn: the first prompt or a follow-up on the same scratch copy.
    /// A new `choice` may change the model or effort, not the agent.
    pub async fn run(&self, s: &Session, prompt: &str, ctx: &Context, choice: Option<Choice>) -> anyhow::Result<Outcome> {
        if let Some(c) = choice {
            let mut cur = s.choice.lock().unwrap();
            if c.provider != cur.provider {
                anyhow::bail!("switching between {} and {} starts a new conversation — apply or discard this proposal first", cur.provider.label(), c.provider.label());
            }
            *cur = c;
        }
        if s.busy.swap(true, Ordering::Relaxed) {
            anyhow::bail!("this session is already running");
        }
        s.cancel.store(false, Ordering::Relaxed);
        let result = self.run_inner(s, prompt, ctx).await;
        s.busy.store(false, Ordering::Relaxed);
        result
    }

    async fn run_inner(&self, s: &Session, prompt: &str, ctx: &Context) -> anyhow::Result<Outcome> {
        let choice = s.choice.lock().unwrap().clone();
        let info = self.catalog.info(choice.provider).map_err(|e| anyhow::anyhow!("{} is unavailable: {e}", choice.provider.label()))?.clone();
        let diags = s.project.latest().map(|b| b.diagnostics.clone()).unwrap_or_default();
        let story = s.project.history.story(8);
        let message = format!("{}\n## Where this draft has been\n\n{story}\n## Request\n\n{}", context_block(ctx, &s.ws, &diags), prompt);
        let (text, model, input_tokens, output_tokens) = match choice.provider {
            Provider::Claude => {
                let r = claude::Runner { bin: info.bin, model: choice.model, effort: choice.effort };
                let t = claude::run(&r, s, &message, SYSTEM, self.mcp_port, &self.mcp_token).await?;
                (t.text, t.model, t.input_tokens, t.output_tokens)
            }
            Provider::Codex => {
                let r = codex::Runner { bin: info.bin, model: choice.model, effort: choice.effort, home: info.home };
                let t = codex::run(&r, s, &message, SYSTEM, &self.mcp_token).await?;
                (t.text, t.model, t.input_tokens, t.output_tokens)
            }
        };
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

    /// Applies (possibly hand-edited) proposed files to the live project, commits, ends the session.
    pub async fn apply(&self, s: &Session, files: Vec<(String, String)>) -> anyhow::Result<Vec<String>> {
        anyhow::ensure!(s.project.take_lock(&s.user, false), "someone else holds the edit lock");
        let model = s.model.lock().unwrap().clone();
        let mut applied = Vec::new();
        for (path, text) in files {
            // `base` is the entry this file was at when the AI read it — what
            // the prompt addressed; provenance hovers can travel back to it.
            let meta = json!({"session": s.id, "prompt": s.prompt, "model": model, "base": s.base_heads.get(&path)});
            s.project.replace(&path, text, 0, &s.user, "ai", meta)?;
            applied.push(path);
        }
        s.project.flush();
        let short: String = s.prompt.chars().take(60).collect::<String>().replace('\n', " ");
        let message = format!("ai: {short}{}", if s.prompt.chars().count() > 60 { "…" } else { "" });
        if let Ok(Some(sha)) = crate::gitd::commit(&s.project.dir, &message, Some(&s.user)).await {
            s.project.committed(&sha, &message, &s.user).await;
        }
        self.end_session(&s.project.name, s.id);
        Ok(applied)
    }
}

/// A short rendering of a tool's input for the activity log (no file contents).
pub(crate) fn brief(input: &Value) -> Value {
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

/// What the model is told about where the author is, and the current file when small.
fn context_block(ctx: &Context, ws: &Workspace, diags: &[Diagnostic]) -> String {
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
        if r.text.is_empty() { format!("{head}\n") } else { format!("{head}\n```\n{}\n```\n", r.text) }
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
            s.push_str(&format!("  - {}:{}: {}\n", d.file, d.line.unwrap_or(0), d.message));
        }
    }
    if let Some(file) = &ctx.file
        && let Ok(text) = ws.read(file)
        && text.len() <= 60_000
    {
        s.push_str(&format!("\n## Current file: {file}\n\n{}", text::numbered(&text, 1, usize::MAX)));
    }
    s
}

const SYSTEM: &str = "You are the writing assistant inside kloverleaf, a LaTeX editor. The author is working on the document in front of them and has asked you for something; the message tells you where they are (cursor, selection, regions they queued for you, the last place they clicked in the PDF) and how the last compile went.

You work on a scratch copy of the project through the kloverleaf tools — they are the only way to see or change it. Read what you need (read_file, search_files), make the edits with write_edits (exact, unique `old` strings — read first, then copy exactly), compile to check, and view_pdf_page when layout, figures or math matter. The author will review your changes as a diff before anything is applied, so be decisive: do the work rather than describing it or asking permission. Keep the author's voice, macros and conventions; change what was asked and what is clearly needed to make it work, not more. Use web search when you need a citation, a package's syntax, or a fact. If the request is ambiguous in a way that matters, pick the reasonable reading and say what you assumed.

When you are done, reply briefly in plain prose: what you changed and why, anything you could not do, and anything the author should check.";

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
            let req: ChoiceReq = serde_json::from_value(v.clone()).unwrap_or_default();
            let choice = match app.ai.catalog.resolve(&req) {
                Ok(c) => c,
                Err(e) => {
                    emit(json!({"event": "error", "message": format!("{e:#}")}));
                    return;
                }
            };
            app.ai.remember(&project);
            let session = match session_id {
                Some(id) => app.ai.session(&project.name, id).ok_or_else(|| anyhow::anyhow!("that conversation is gone (applied, discarded, or its workspace was removed) — start a new one")),
                None => app.ai.new_session(project.clone(), &user, &prompt, choice.clone()),
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
                let _ = app2.ai.turn(&s2, &prompt, &ctx, choice).await;
            });
            *s.task.lock().unwrap() = Some(task.abort_handle());
        }
        "apply" => {
            app.ai.remember(&project);
            let Some(s) = session_id.and_then(|id| app.ai.session(&project.name, id)) else {
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
                match app.ai.apply(&s, files).await {
                    Ok(applied) => s.emit(json!({"event": "applied", "files": applied})),
                    Err(e) => s.emit(json!({"event": "error", "message": format!("{e:#}")})),
                }
            });
        }
        "discard" => {
            app.ai.remember(&project);
            if let Some(id) = session_id
                && !app.ai.discard(&project.name, id)
            {
                emit(json!({"event": "discarded"})); // the panel still wants to clear
            }
        }
        "cancel" => {
            if let Some(s) = session_id.and_then(|id| app.ai.session(&project.name, id)) {
                s.stop();
                s.emit(json!({"event": "cancelled"}));
            }
        }
        _ => emit(json!({"event": "error", "message": format!("unknown action {action:?}")})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn project() -> (std::path::PathBuf, Project) {
        let root = crate::history::tmp_dir("ai");
        std::fs::create_dir_all(root.join("projects")).unwrap();
        let p = Project::create(&root, "p", Duration::from_secs(30)).await.unwrap();
        p.replace("main.tex", "\\documentclass{article}\n\\begin{document}\nSECRET sentence.\n\\end{document}\n".into(), 0, "kris", "replace", json!({})).unwrap();
        (root, p)
    }

    /// A stand-in for the CLI: echoes stream-json the way `claude -p` does.
    /// It edits nothing itself (that is the MCP server's job, exercised below).
    fn fake_claude(root: &std::path::Path, body: &str) -> Catalog {
        let bin = root.join("fake-claude");
        std::fs::write(&bin, format!("#!/bin/sh\ncat >/dev/null\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Catalog {
            claude: Ok(ProviderInfo { bin: bin.to_string_lossy().into_owned(), models: vec!["test-model".into()], efforts: claude::EFFORTS.to_vec(), home: PathBuf::new() }),
            codex: Err("not in tests".into()),
            default: Choice { provider: Provider::Claude, model: "test-model".into(), effort: "low".into() },
        }
    }

    #[tokio::test]
    async fn a_turn_edits_through_mcp_and_applies_as_a_proposal() {
        let (root, p) = project().await;
        let catalog = fake_claude(
            &root,
            r#"echo '{"type":"system","subtype":"init","session_id":"sess-1"}'
echo '{"type":"assistant","message":{"model":"claude-test","content":[{"type":"text","text":"Working."}]}}'
echo '{"type":"result","subtype":"success","is_error":false,"result":"Replaced the sentence.","session_id":"sess-1","usage":{"input_tokens":10,"cache_read_input_tokens":5,"output_tokens":3}}'"#,
        );
        let ai = Ai::new(catalog, Duration::from_secs(30), 7200);
        let pick = ai.catalog.resolve(&ChoiceReq::default()).unwrap();
        assert_eq!(pick.model, "test-model");
        assert!(ai.catalog.resolve(&ChoiceReq { model: Some("nope".into()), ..Default::default() }).is_err());
        assert!(ai.catalog.resolve(&ChoiceReq { provider: Some(Provider::Codex), ..Default::default() }).is_err());
        let s = ai.new_session(p.clone(), "kris", "make it public", pick).unwrap();
        // What the CLI would do through /mcp: an exact edit on the workspace.
        let r = mcp::execute(&s.ws, "write_edits", &json!({"path": "main.tex", "edits": [{"old": "SECRET sentence.", "new": "Public sentence."}]})).await.unwrap();
        assert!(r.summary.starts_with("edited main.tex"));
        let ctx = Context { file: Some("main.tex".into()), line: Some(3), ..Default::default() };
        let o = ai.run(&s, "make it public", &ctx, None).await.unwrap();
        assert_eq!(o.text, "Replaced the sentence.");
        assert_eq!(o.model, "claude-test");
        assert_eq!((o.input_tokens, o.output_tokens), (15, 3));
        assert_eq!(s.agent_session.lock().unwrap().as_deref(), Some("sess-1"));
        assert_eq!(o.files.len(), 1);
        assert!(o.files[0].after.contains("Public sentence."));
        assert!(p.text("main.tex").unwrap().contains("SECRET"), "nothing applied yet");
        let files = o.files.iter().map(|f| (f.path.clone(), f.after.clone())).collect();
        ai.apply(&s, files).await.unwrap();
        assert!(p.text("main.tex").unwrap().contains("Public sentence."));
        assert!(ai.session(&p.name, s.id).is_none());
        assert!(!s.ws.dir.exists(), "workspace removed");
        let head = p.history.entry(p.history.head("main.tex").unwrap()).unwrap();
        assert_eq!(head.kind, "ai");
        assert_eq!(head.meta["prompt"], "make it public");
        assert_eq!(head.meta["model"], "claude-test");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_failed_turn_is_an_error_not_a_proposal() {
        let (root, p) = project().await;
        let catalog = fake_claude(&root, r#"echo '{"type":"result","subtype":"error_during_execution","is_error":true,"result":"","session_id":"x"}'"#);
        let pick = catalog.default.clone();
        let ai = Ai::new(catalog, Duration::from_secs(30), 7200);
        let s = ai.new_session(p.clone(), "kris", "x", pick.clone()).unwrap();
        let e = ai.run(&s, "x", &Context::default(), None).await.unwrap_err().to_string();
        assert!(e.contains("error_during_execution"), "{e}");
        let silent = fake_claude(&root, "exit 3");
        let ai = Ai::new(silent, Duration::from_secs(30), 7200);
        let s = ai.new_session(p.clone(), "kris", "x", pick.clone()).unwrap();
        let e = ai.run(&s, "x", &Context::default(), None).await.unwrap_err().to_string();
        assert!(e.contains("without a result"), "{e}");
        let other = Choice { provider: Provider::Codex, model: "gpt".into(), effort: "low".into() };
        let e = ai.run(&s, "x", &Context::default(), Some(other)).await.unwrap_err().to_string();
        assert!(e.contains("new conversation"), "{e}");
        let off = Ai::new(Catalog { claude: Err("no CLI".into()), codex: Err("no codex".into()), default: pick.clone() }, Duration::from_secs(30), 7200);
        assert!(off.new_session(p, "kris", "x", pick).is_err());
        assert_eq!(off.describe()["mode"], "off");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn context_mentions_everything_the_author_pointed_at() {
        let dir = crate::history::tmp_dir("ctx");
        std::fs::write(dir.join("main.tex"), "a\nb\nc\n").unwrap();
        let ws = Workspace { dir: dir.clone(), latex_timeout: Duration::from_secs(1) };
        let ctx = Context {
            file: Some("main.tex".into()),
            line: Some(2),
            selection: Some(Region { file: "main.tex".into(), from: 2, to: 2, text: "b".into(), note: Some("here".into()) }),
            queue: vec![],
            pdf: Some(PdfClick { page: 3, file: Some("main.tex".into()), line: Some(1), ..Default::default() }),
            column: None,
        };
        let s = context_block(&ctx, &ws, &[]);
        for needle in ["main.tex (main)", "Cursor: main.tex line 2", "lines 2-2 — note: here", "page 3, which SyncTeX maps to main.tex line 1", "no information yet", "## Current file: main.tex"] {
            assert!(s.contains(needle), "missing {needle:?} in {s}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
