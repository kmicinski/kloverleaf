//! Live projects: the in-memory text of every file, the edit lock, the build
//! loop, and the WebSocket protocol the editor speaks.
//!
//! One [`Project`] per directory holds the authoritative text of each text
//! file. Every writer — a browser (deltas), a tool or the AI (whole text), a
//! git push (reload) — goes through [`Project::edit`] / [`Project::replace`],
//! which record the change in the history store, broadcast an [`Event`] to
//! every connection, and wake the build loop. The build loop waits for typing
//! to pause (`DEBOUNCE`), flushes the texts to disk, runs latexmk, and
//! broadcasts the result; the PDF stays in memory and is served from there.
//!
//! ```text
//! server → client
//!   {"type":"init","user","files","main","lock","build","ai"}   on connect
//!   {"type":"text","file","text"}          whole text: answer to `open`, after an external
//!                                          replace, or when an edit was refused (desync, lock)
//!   {"type":"edit","file","changes"}       another editor's delta, to apply locally
//!   {"type":"saved","file","error"?}       the write to disk finished
//!   {"type":"building"}                    latexmk started
//!   {"type":"built","version","ok","diags","ms","pdf"}
//!                                          latexmk finished; `pdf` says whether the PDF changed
//!   {"type":"lock","lock"}                 {user, since, until} or null
//!   {"type":"files","files","main"}        the file list changed
//!   {"type":"committed","sha","message"}
//!   {"type":"refused","file","reason"}     an edit was not applied (followed by `text`)
//!   {"type":"ai", …}                       see `ai`
//! client → server
//!   {"type":"open","file"}                 get the text (the connection is then an editor of it)
//!   {"type":"edit","file","changes":[{offset,length,text}],"hash"}
//!                                          Monaco's content changes (UTF-16 offsets, each relative
//!                                          to the text before the batch) and an FNV-1a hash of the result
//!   {"type":"ping"}                        keeps the lock alive while the page is open
//!   {"type":"lock","action":"take"|"release"|"steal"}
//!   {"type":"ai", …}                       see `ai`
//! ```
//!
//! The lock: whoever edits first holds it; it expires `LOCK_TTL` after the
//! holder's last ping, and anyone may steal it. Edits by anyone else are
//! refused and answered with the server's text.

use crate::build::{self, Compiled, Diagnostic};
use crate::history::Store;
use crate::project::{self, FileInfo};
use crate::text::{self, Edit};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fs, io};
use tokio::sync::{broadcast, mpsc, oneshot};

const DEBOUNCE: Duration = Duration::from_millis(250);
const LOCK_TTL: i64 = 90_000;

pub enum Change {
    Delta(Vec<Edit>),
    Text(Arc<str>),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Lock {
    pub user: String,
    pub since: i64,
    pub until: i64,
}

pub struct BuildOutcome {
    pub version: u64,
    pub ok: bool,
    pub diagnostics: Vec<Diagnostic>,
    pub ms: u64,
    pub pdf: Option<Arc<Vec<u8>>>,
    pub pdf_changed: bool,
    pub log: String,
}

impl BuildOutcome {
    pub fn summary(&self) -> Value {
        json!({
            "version": self.version, "ok": self.ok, "diags": self.diagnostics,
            "ms": self.ms, "pdf": self.pdf_changed, "hasPdf": self.pdf.is_some(),
        })
    }
}

pub enum Event {
    Changed { file: String, change: Change, origin: u64 },
    Saved { file: String, error: Option<String> },
    Building,
    Built(Arc<BuildOutcome>),
    Lock(Option<Lock>),
    Files { files: Vec<FileInfo>, main: Option<String> },
    Committed { sha: String, message: String },
    Ai(Value),
}

enum BuildReq {
    Dirty,
    Now(oneshot::Sender<Arc<BuildOutcome>>),
}

struct DocState {
    text: String,
    version: u64,
    saved: u64,
}

/// Handle to a project's live state; clones share it.
#[derive(Clone)]
pub struct Project(Arc<Inner>);

pub struct Inner {
    pub name: String,
    pub dir: PathBuf,
    docs: Mutex<BTreeMap<String, DocState>>,
    pub history: Store,
    lock: Mutex<Option<Lock>>,
    tx: broadcast::Sender<Arc<Event>>,
    build_tx: mpsc::UnboundedSender<BuildReq>,
    latest: Mutex<Option<Arc<BuildOutcome>>>,
    pub building: AtomicBool,
    main: Mutex<Option<String>>,
    latex_timeout: Duration,
    /// Serialises compiles of historical versions (time travel), so scrubbing
    /// the timeline never runs more than one latexmk at a time.
    preview: tokio::sync::Mutex<()>,
}

impl std::ops::Deref for Project {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl Project {
    /// Opens `projects/<name>`, loading every text file; must run inside tokio.
    pub fn open(root: &Path, name: &str, latex_timeout: Duration) -> io::Result<Project> {
        let dir = root.join("projects").join(name);
        fs::create_dir_all(dir.join(project::PRIVATE_DIR))?;
        let history = Store::open(&dir.join(project::PRIVATE_DIR).join("history.sqlite"))
            .map_err(|e| io::Error::other(e.to_string()))?;
        let files = project::list_files(&dir);
        let mut docs = BTreeMap::new();
        for f in files.iter().filter(|f| f.text) {
            if let Ok(text) = fs::read_to_string(dir.join(&f.path)) {
                docs.insert(f.path.clone(), DocState { text, version: 0, saved: 0 });
            }
        }
        let main = project::main_file(&dir, &files);
        let (build_tx, build_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            name: name.into(),
            dir,
            docs: Mutex::new(docs),
            history,
            lock: Mutex::new(None),
            tx: broadcast::channel(256).0,
            build_tx,
            latest: Mutex::new(None),
            building: AtomicBool::new(false),
            main: Mutex::new(main),
            latex_timeout,
            preview: tokio::sync::Mutex::new(()),
        });
        let project = Project(inner);
        tokio::spawn(build_loop(project.clone(), build_rx));
        Ok(project)
    }

    /// Creates `projects/<name>` with a starter document and a git repository.
    pub async fn create(root: &Path, name: &str, latex_timeout: Duration) -> anyhow::Result<Project> {
        anyhow::ensure!(project::valid_name(name), "invalid project name {name:?}");
        let dir = root.join("projects").join(name);
        anyhow::ensure!(!dir.exists(), "project {name} exists");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("main.tex"), project::STARTER)?;
        crate::gitd::init(&dir).await?;
        let p = Project::open(root, name, latex_timeout)?;
        p.history.record("main.tex", "kloverleaf", "create", None, project::STARTER, json!({}))?;
        crate::gitd::commit(&dir, "new project", None).await?;
        p.schedule_build();
        Ok(p)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
        self.tx.subscribe()
    }

    fn emit(&self, e: Event) {
        let _ = self.tx.send(Arc::new(e));
    }

    pub fn emit_ai(&self, v: Value) {
        self.emit(Event::Ai(v));
    }

    pub fn text(&self, file: &str) -> Option<String> {
        self.docs.lock().unwrap().get(file).map(|d| d.text.clone())
    }

    /// Every text file's current text — what the AI's scratch copy starts from.
    pub fn texts(&self) -> BTreeMap<String, String> {
        self.docs.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.text.clone())).collect()
    }

    pub fn files(&self) -> Vec<FileInfo> {
        project::list_files(&self.dir)
    }

    pub fn main(&self) -> Option<String> {
        self.main.lock().unwrap().clone()
    }

    pub fn set_main(&self, file: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.text(file).is_some(), "no such text file");
        project::write_config(&self.dir, &project::Config { main: Some(file.into()) })?;
        *self.main.lock().unwrap() = Some(file.into());
        self.emit(Event::Files { files: self.files(), main: Some(file.into()) });
        self.schedule_build();
        Ok(())
    }

    pub fn latest(&self) -> Option<Arc<BuildOutcome>> {
        self.latest.lock().unwrap().clone()
    }

    // ---- lock ----------------------------------------------------------------

    pub fn lock(&self) -> Option<Lock> {
        let mut g = self.lock.lock().unwrap();
        if g.as_ref().is_some_and(|l| l.until < crate::history::now_ms()) {
            *g = None;
        }
        g.clone()
    }

    /// Takes the lock for `user` if it is free (or theirs, or `steal`). Returns whether they hold it.
    pub fn take_lock(&self, user: &str, steal: bool) -> bool {
        let before = self.lock();
        let now = crate::history::now_ms();
        let ok = {
            let mut g = self.lock.lock().unwrap();
            match g.as_mut() {
                Some(l) if l.user == user => {
                    l.until = now + LOCK_TTL;
                    true
                }
                Some(_) if !steal => false,
                _ => {
                    *g = Some(Lock { user: user.into(), since: now, until: now + LOCK_TTL });
                    true
                }
            }
        };
        let after = self.lock();
        if ok && before.as_ref().map(|l| &l.user) != after.as_ref().map(|l| &l.user) {
            let _ = self.history.note(user, "lock", json!({"stolen_from": before.map(|l| l.user)}));
            self.emit(Event::Lock(after));
        }
        ok
    }

    pub fn release_lock(&self, user: &str) {
        let mut g = self.lock.lock().unwrap();
        if g.as_ref().is_some_and(|l| l.user == user) {
            *g = None;
            drop(g);
            self.emit(Event::Lock(None));
        }
    }

    /// Renews the lock if `user` holds it (the editor pings while open).
    pub fn renew_lock(&self, user: &str) {
        let mut g = self.lock.lock().unwrap();
        if let Some(l) = g.as_mut()
            && l.user == user
        {
            l.until = crate::history::now_ms() + LOCK_TTL;
        }
    }

    // ---- editing -------------------------------------------------------------

    /// Applies an editor's delta on behalf of `user`. `Err(reason)` when the
    /// hash does not match (the client's text was not the server's) or
    /// someone else holds the lock; nothing changes then.
    pub fn edit(&self, file: &str, changes: &[Edit], hash: u32, origin: u64, user: &str) -> Result<(), &'static str> {
        if !self.take_lock(user, false) {
            return Err("locked");
        }
        let mut docs = self.docs.lock().unwrap();
        let Some(doc) = docs.get_mut(file) else { return Err("no such file") };
        let next = text::apply(&doc.text, changes);
        if text::fnv1a(&next) != hash {
            return Err("desync");
        }
        doc.text = next;
        doc.version += 1;
        let _ = self.history.record(file, user, "delta", Some(changes), &doc.text, json!({}));
        drop(docs);
        self.after_change(file, Change::Delta(changes.to_vec()), origin);
        Ok(())
    }

    /// Replaces the whole text of a file (tools, the AI, restores, git),
    /// creating the file if it is new. `kind` and `meta` go to the history.
    pub fn replace(&self, file: &str, text: String, origin: u64, user: &str, kind: &str, meta: Value) -> anyhow::Result<()> {
        anyhow::ensure!(project::valid_path(file) && project::is_text(file), "not a text file path");
        let mut docs = self.docs.lock().unwrap();
        let fresh = !docs.contains_key(file);
        let doc = docs.entry(file.to_string()).or_insert(DocState { text: String::new(), version: 0, saved: 0 });
        if !fresh && doc.text == text {
            return Ok(());
        }
        doc.text = text;
        doc.version += 1;
        let _ = self.history.record(file, user, kind, None, &doc.text, meta);
        let change = Change::Text(doc.text.as_str().into());
        drop(docs);
        if fresh {
            self.save(file);
            self.emit(Event::Files { files: self.files(), main: self.main() });
            if self.main().is_none() {
                *self.main.lock().unwrap() = project::main_file(&self.dir, &self.files());
            }
        }
        self.after_change(file, change, origin);
        Ok(())
    }

    fn after_change(&self, file: &str, change: Change, origin: u64) {
        self.emit(Event::Changed { file: file.into(), change, origin });
        self.schedule_build();
        let (p, file) = (self.clone(), file.to_string());
        let version = self.docs.lock().unwrap().get(&file).map(|d| d.version).unwrap_or(0);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let current = p.docs.lock().unwrap().get(&file).map(|d| d.version).unwrap_or(0);
            if current == version {
                p.save(&file);
            }
        });
    }

    /// Writes one file to disk if its text changed since the last write.
    fn save(&self, file: &str) {
        let mut docs = self.docs.lock().unwrap();
        let Some(doc) = docs.get_mut(file) else { return };
        if doc.saved == doc.version && self.dir.join(file).exists() {
            return;
        }
        let path = self.dir.join(file);
        let result = fs::create_dir_all(path.parent().unwrap())
            .and_then(|()| fs::write(&path, &doc.text));
        if result.is_ok() {
            doc.saved = doc.version;
        }
        drop(docs);
        self.emit(Event::Saved { file: file.into(), error: result.err().map(|e| e.to_string()) });
    }

    /// Writes every changed text to disk — before a compile or a commit.
    pub fn flush(&self) {
        let files: Vec<String> = self.docs.lock().unwrap().keys().cloned().collect();
        for f in files {
            self.save(&f);
        }
    }

    // ---- files ---------------------------------------------------------------

    /// Stores a file: text files become live documents, anything else goes to disk.
    pub fn write_file(&self, path: &str, bytes: Vec<u8>, user: &str) -> anyhow::Result<()> {
        anyhow::ensure!(project::valid_path(path), "invalid path");
        if project::is_text(path) {
            let text = String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("not UTF-8"))?;
            return self.replace(path, text, 0, user, "replace", json!({"source": "upload"}));
        }
        let full = self.dir.join(path);
        fs::create_dir_all(full.parent().unwrap())?;
        fs::write(full, bytes)?;
        self.emit(Event::Files { files: self.files(), main: self.main() });
        self.schedule_build();
        Ok(())
    }

    pub fn delete_file(&self, path: &str, user: &str) -> anyhow::Result<()> {
        anyhow::ensure!(project::valid_path(path), "invalid path");
        let full = self.dir.join(path);
        anyhow::ensure!(full.is_file(), "no such file");
        if let Some(doc) = self.docs.lock().unwrap().remove(path) {
            let _ = self.history.record(path, user, "delete", None, "", json!({"size": doc.text.len()}));
        }
        fs::remove_file(full)?;
        if self.main().as_deref() == Some(path) {
            *self.main.lock().unwrap() = project::main_file(&self.dir, &self.files());
        }
        self.emit(Event::Files { files: self.files(), main: self.main() });
        Ok(())
    }

    pub fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        if !project::valid_path(path) {
            return None;
        }
        if let Some(t) = self.text(path) {
            return Some(t.into_bytes());
        }
        fs::read(self.dir.join(path)).ok()
    }

    /// After a git push (or any change made on disk behind our back): adopt
    /// the files on disk as the truth, telling every editor.
    pub fn reload_from_disk(&self, user: &str) -> anyhow::Result<()> {
        let files = self.files();
        let known: Vec<String> = self.docs.lock().unwrap().keys().cloned().collect();
        for f in files.iter().filter(|f| f.text) {
            let on_disk = fs::read_to_string(self.dir.join(&f.path))?;
            if self.text(&f.path).as_deref() != Some(on_disk.as_str()) {
                self.replace(&f.path, on_disk, 0, user, "replace", json!({"source": "git"}))?;
                let mut docs = self.docs.lock().unwrap();
                if let Some(d) = docs.get_mut(&f.path) {
                    d.saved = d.version;
                }
            }
        }
        for k in known {
            if !files.iter().any(|f| f.path == k) {
                self.docs.lock().unwrap().remove(&k);
            }
        }
        *self.main.lock().unwrap() = project::main_file(&self.dir, &files);
        self.emit(Event::Files { files, main: self.main() });
        self.schedule_build();
        Ok(())
    }

    // ---- building ------------------------------------------------------------

    pub fn schedule_build(&self) {
        let _ = self.build_tx.send(BuildReq::Dirty);
    }

    /// Compiles now (after the current compile, if one is running) and waits.
    pub async fn build_now(&self) -> Option<Arc<BuildOutcome>> {
        let (tx, rx) = oneshot::channel();
        self.build_tx.send(BuildReq::Now(tx)).ok()?;
        rx.await.ok()
    }

    pub fn pdf_path(&self) -> Option<PathBuf> {
        self.main().map(|m| build::pdf_path(&self.dir, &m))
    }

    /// The PDF of the project as it was at history entry `id`: the texts of
    /// that moment in a scratch copy (today's figures), compiled once and
    /// cached at `.kloverleaf/history/<id>.pdf`.
    pub async fn pdf_at(&self, id: i64) -> anyhow::Result<Vec<u8>> {
        let cache = self.dir.join(project::PRIVATE_DIR).join("history");
        let cached = cache.join(format!("{id}.pdf"));
        if let Ok(bytes) = fs::read(&cached) {
            return Ok(bytes);
        }
        let _guard = self.preview.lock().await;
        if let Ok(bytes) = fs::read(&cached) {
            return Ok(bytes);
        }
        let texts = self.history.files_at(id)?;
        anyhow::ensure!(!texts.is_empty(), "nothing in the history at that point");
        let dir = cache.join("work");
        let ws = crate::ai::tools::make_workspace(&dir, &self.dir, &texts, self.latex_timeout)?;
        let main = ws.main().ok_or_else(|| anyhow::anyhow!("no main .tex file at that point"))?;
        let compiled = build::compile(&dir, &main, self.latex_timeout).await?;
        let pdf = compiled.pdf.ok_or_else(|| anyhow::anyhow!("that version does not compile to a PDF"))?;
        fs::create_dir_all(&cache)?;
        fs::write(&cached, &pdf)?;
        Ok(pdf)
    }

    pub async fn committed(&self, sha: &str, message: &str, user: &str) {
        let _ = self.history.note(user, "commit", json!({"sha": sha, "message": message}));
        self.emit(Event::Committed { sha: sha.into(), message: message.into() });
    }
}

async fn build_loop(p: Project, mut rx: mpsc::UnboundedReceiver<BuildReq>) {
    let mut version = 0u64;
    let mut last_hash: Option<u64> = None;
    while let Some(req) = rx.recv().await {
        let mut waiters = Vec::new();
        match req {
            BuildReq::Dirty => {
                // Keep absorbing requests until typing pauses.
                loop {
                    match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                        Ok(Some(BuildReq::Dirty)) => continue,
                        Ok(Some(BuildReq::Now(w))) => waiters.push(w),
                        Ok(None) => return,
                        Err(_) => break,
                    }
                }
            }
            BuildReq::Now(w) => waiters.push(w),
        }
        let Some(main) = p.main() else {
            let outcome = Arc::new(BuildOutcome {
                version,
                ok: false,
                diagnostics: vec![Diagnostic {
                    file: String::new(),
                    line: None,
                    severity: build::Severity::Error,
                    message: "no main .tex file in this project".into(),
                }],
                ms: 0,
                pdf: None,
                pdf_changed: false,
                log: String::new(),
            });
            for w in waiters {
                let _ = w.send(outcome.clone());
            }
            p.emit(Event::Built(outcome));
            continue;
        };
        p.building.store(true, Ordering::Relaxed);
        p.emit(Event::Building);
        p.flush();
        let compiled = build::compile(&p.dir, &main, p.latex_timeout).await;
        let outcome = match compiled {
            Ok(Compiled { ok, diagnostics, pdf, log, ms }) => {
                let hash = pdf.as_ref().map(|b| {
                    b.iter().fold(0xcbf29ce484222325u64, |h, &x| (h ^ x as u64).wrapping_mul(0x100000001b3))
                });
                let pdf_changed = pdf.is_some() && hash != last_hash;
                if pdf_changed {
                    version += 1;
                    last_hash = hash;
                }
                let pdf = match pdf {
                    Some(b) => Some(Arc::new(b)),
                    None => p.latest().and_then(|l| l.pdf.clone()),
                };
                BuildOutcome { version, ok, diagnostics, ms, pdf, pdf_changed, log }
            }
            Err(e) => BuildOutcome {
                version,
                ok: false,
                diagnostics: vec![Diagnostic {
                    file: main.clone(),
                    line: None,
                    severity: build::Severity::Error,
                    message: format!("could not run latexmk: {e:#}"),
                }],
                ms: 0,
                pdf: p.latest().and_then(|l| l.pdf.clone()),
                pdf_changed: false,
                log: String::new(),
            },
        };
        let outcome = Arc::new(outcome);
        *p.latest.lock().unwrap() = Some(outcome.clone());
        p.building.store(false, Ordering::Relaxed);
        for w in waiters {
            let _ = w.send(outcome.clone());
        }
        p.emit(Event::Built(outcome));
    }
}

// ---- WebSocket session ---------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientMsg {
    Open { file: String },
    Edit { file: String, changes: Vec<Edit>, hash: u32 },
    Ping,
    Lock { action: String },
    Ai(Value),
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn msg(v: Value) -> Message {
    Message::Text(v.to_string().into())
}

pub async fn session(socket: WebSocket, app: crate::Shared, project: Project, user: String) {
    let _ = run(socket, app, project, user).await;
}

async fn run(socket: WebSocket, app: crate::Shared, p: Project, user: String) -> Result<(), axum::Error> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut rx = p.subscribe();
    let (mut sink, mut stream) = socket.split();
    sink.send(msg(json!({
        "type": "init", "user": user, "files": p.files(), "main": p.main(), "lock": p.lock(),
        "build": p.latest().map(|b| b.summary()), "building": p.building.load(Ordering::Relaxed),
        "ai": app.ai.describe(),
    })))
    .await?;
    loop {
        tokio::select! {
            m = stream.next() => {
                let Some(Ok(Message::Text(text))) = m else {
                    if matches!(m, Some(Ok(_))) { continue } else { break }
                };
                match serde_json::from_str::<ClientMsg>(&text) {
                    Ok(ClientMsg::Open { file }) => match p.text(&file) {
                        Some(t) => sink.send(msg(json!({"type": "text", "file": file, "text": t}))).await?,
                        None => sink.send(msg(json!({"type": "refused", "file": file, "reason": "no such file"}))).await?,
                    },
                    Ok(ClientMsg::Edit { file, changes, hash }) => {
                        if let Err(reason) = p.edit(&file, &changes, hash, id, &user) {
                            sink.send(msg(json!({"type": "refused", "file": file, "reason": reason}))).await?;
                            if let Some(t) = p.text(&file) {
                                sink.send(msg(json!({"type": "text", "file": file, "text": t}))).await?;
                            }
                            if reason == "locked" {
                                sink.send(msg(json!({"type": "lock", "lock": p.lock()}))).await?;
                            }
                        }
                    }
                    Ok(ClientMsg::Ping) => p.renew_lock(&user),
                    Ok(ClientMsg::Lock { action }) => {
                        match action.as_str() {
                            "take" => { p.take_lock(&user, false); }
                            "steal" => { p.take_lock(&user, true); }
                            "release" => p.release_lock(&user),
                            _ => {}
                        }
                        sink.send(msg(json!({"type": "lock", "lock": p.lock()}))).await?;
                    }
                    Ok(ClientMsg::Ai(v)) => crate::ai::handle(app.clone(), p.clone(), user.clone(), id, v),
                    Err(_) => {}
                }
            }
            update = rx.recv() => match update {
                Ok(ev) => {
                    let out = match &*ev {
                        Event::Changed { file, change, origin } => {
                            if *origin == id { continue }
                            match change {
                                Change::Delta(changes) => json!({"type": "edit", "file": file, "changes": changes}),
                                Change::Text(text) => json!({"type": "text", "file": file, "text": &**text}),
                            }
                        }
                        Event::Saved { file, error } => json!({"type": "saved", "file": file, "error": error}),
                        Event::Building => json!({"type": "building"}),
                        Event::Built(b) => { let mut v = b.summary(); v["type"] = "built".into(); v }
                        Event::Lock(l) => json!({"type": "lock", "lock": l}),
                        Event::Files { files, main } => json!({"type": "files", "files": files, "main": main}),
                        Event::Committed { sha, message } => json!({"type": "committed", "sha": sha, "message": message}),
                        Event::Ai(v) => { let mut v = v.clone(); v["type"] = "ai".into(); v }
                    };
                    sink.send(msg(out)).await?;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    sink.send(msg(json!({"type": "lagged"}))).await?;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        let dir = crate::history::tmp_dir("live");
        fs::create_dir_all(dir.join("projects")).unwrap();
        dir
    }

    #[tokio::test]
    async fn edits_verify_and_lock() {
        let root = root();
        let p = Project::create(&root, "t", Duration::from_secs(30)).await.unwrap();
        assert_eq!(p.main().as_deref(), Some("main.tex"));
        p.replace("main.tex", "hello world".into(), 0, "kris", "replace", json!({})).unwrap();
        let changes = vec![
            Edit { offset: 6, length: 5, text: "there".into() },
            Edit { offset: 0, length: 5, text: "hi".into() },
        ];
        assert_eq!(p.edit("main.tex", &changes, 0, 1, "kris"), Err("desync"));
        assert_eq!(p.text("main.tex").unwrap(), "hello world");
        assert_eq!(p.edit("main.tex", &changes, text::fnv1a("hi there"), 1, "kris"), Ok(()));
        assert_eq!(p.text("main.tex").unwrap(), "hi there");
        assert_eq!(p.lock().unwrap().user, "kris", "first editor takes the lock");
        let more = vec![Edit { offset: 0, length: 0, text: "x".into() }];
        assert_eq!(p.edit("main.tex", &more, text::fnv1a("xhi there"), 2, "someone"), Err("locked"));
        assert!(p.take_lock("someone", true));
        assert_eq!(p.edit("main.tex", &more, text::fnv1a("xhi there"), 2, "someone"), Ok(()));
        p.release_lock("someone");
        assert!(p.lock().is_none());
        // history has it all
        let head = p.history.head("main.tex").unwrap();
        assert_eq!(p.history.text_at(head).unwrap().unwrap(), "xhi there");
        // files
        p.write_file("figs/a.bin", vec![1, 2, 3], "kris").unwrap();
        assert!(p.files().iter().any(|f| f.path == "figs/a.bin" && !f.text));
        p.write_file("chap/one.tex", b"\\section{One}".to_vec(), "kris").unwrap();
        assert_eq!(p.text("chap/one.tex").unwrap(), "\\section{One}");
        p.delete_file("chap/one.tex", "kris").unwrap();
        assert!(p.text("chap/one.tex").is_none());
        assert!(p.write_file("../escape.tex", vec![], "kris").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
