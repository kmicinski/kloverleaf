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
use crate::provenance::{self, Map};
use crate::project::{self, FileInfo};
use crate::text::{self, Edit};
use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    /// The PDF is an earlier build's: this one stopped before writing one.
    pub stale: bool,
    pub log: String,
}

impl BuildOutcome {
    pub fn summary(&self) -> Value {
        json!({
            "version": self.version, "ok": self.ok, "diags": self.diagnostics,
            "ms": self.ms, "pdf": self.pdf_changed, "hasPdf": self.pdf.is_some(),
            "stale": self.stale,
        })
    }
}

pub enum Event {
    /// `prov`: for a delta, the typist's origin id; for a text, `{spans, origins}`.
    Changed { file: String, change: Change, origin: u64, user: String, prov: Value },
    /// The provenance of a file changed without its text (a claim).
    Provenance { file: String, prov: Value },
    Saved { file: String, error: Option<String> },
    Building,
    Built(Arc<BuildOutcome>),
    Lock(Option<Lock>),
    Files { files: Vec<FileInfo>, main: Option<String> },
    Committed { sha: String, message: String },
    /// The project moved to another branch: every text is new, reopen everything.
    Branch { name: String },
    Ai(Value),
    /// The tracked upstream repository's state changed (see `upstream.rs`).
    Upstream(Value),
}

enum BuildReq {
    Dirty,
    Now(oneshot::Sender<Arc<BuildOutcome>>),
}

struct DocState {
    text: String,
    version: u64,
    saved: u64,
    /// Who wrote each run of the text (see `provenance.rs`); always as long as `text`.
    prov: Map,
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
    human_origins: Mutex<HashMap<String, u32>>,
    /// When a person last changed a text, and who (the upstream poller waits
    /// for quiet before pulling, and pushes as them once they have left).
    last_edit: Mutex<(i64, String)>,
    /// Open editor connections.
    viewers: AtomicUsize,
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
        // The branch the working tree is on, remembered across restarts.
        if let Ok(b) = fs::read_to_string(dir.join(project::PRIVATE_DIR).join("branch"))
            && history.branch_exists(b.trim())
        {
            history.set_branch(b.trim());
        }
        let files = project::list_files(&dir);
        let mut docs = BTreeMap::new();
        for f in files.iter().filter(|f| f.text) {
            if let Ok(text) = fs::read_to_string(dir.join(&f.path)) {
                let prov = history.provenance(&f.path).unwrap_or_default().fitted(&text);
                docs.insert(f.path.clone(), DocState { text, version: 0, saved: 0, prov });
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
            human_origins: Mutex::new(HashMap::new()),
            last_edit: Mutex::new((0, String::new())),
            viewers: AtomicUsize::new(0),
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

    pub fn emit_upstream(&self, v: Value) {
        self.emit(Event::Upstream(v));
    }

    /// (ms, user) of the last change a person made.
    pub fn last_edit(&self) -> (i64, String) {
        self.last_edit.lock().unwrap().clone()
    }

    fn touched(&self, user: &str) {
        *self.last_edit.lock().unwrap() = (crate::history::now_ms(), user.to_string());
    }

    pub fn viewers(&self) -> usize {
        self.viewers.load(Ordering::Relaxed)
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
        self.files_changed();
        self.schedule_build();
        Ok(())
    }

    /// Re-detects the main file from what is on disk (after a file came or went).
    fn refresh_main(&self) {
        *self.main.lock().unwrap() = project::main_file(&self.dir, &self.files());
    }

    /// Tells every editor the file list (or the main file) changed.
    fn files_changed(&self) {
        self.emit(Event::Files { files: self.files(), main: self.main() });
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

    /// `take` | `steal` | `release`, as the socket and the API spell it.
    /// `Ok(held)` says whether `user` holds the lock afterwards.
    pub fn lock_action(&self, user: &str, action: &str) -> Result<bool, &'static str> {
        match action {
            "take" => Ok(self.take_lock(user, false)),
            "steal" => Ok(self.take_lock(user, true)),
            "release" => {
                self.release_lock(user);
                Ok(false)
            }
            _ => Err("action: take | steal | release"),
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
        self.touched(user);
        let _ = self.history.record(file, user, "delta", Some(changes), &doc.text, json!({}));
        let me = self.human_origin(user);
        doc.prov.apply(changes, me);
        let _ = self.history.set_provenance(file, &doc.prov);
        drop(docs);
        self.after_change(file, Change::Delta(changes.to_vec()), origin, user, json!(me));
        Ok(())
    }

    /// Replaces the whole text of a file (tools, the AI, restores, git),
    /// creating the file if it is new. `kind` and `meta` go to the history.
    pub fn replace(&self, file: &str, text: String, origin: u64, user: &str, kind: &str, meta: Value) -> anyhow::Result<()> {
        self.replace_if(file, None, text, origin, user, kind, meta).map(|_| ())
    }

    /// `replace`, but only if the file's text is still `expected` (`None` =
    /// whatever it is) — the upstream merge's compare-and-swap, so a merge
    /// computed from a text the person has since typed into is never applied.
    /// `Ok(false)` when it was not.
    #[allow(clippy::too_many_arguments)]
    pub fn replace_if(&self, file: &str, expected: Option<&str>, text: String, origin: u64, user: &str, kind: &str, meta: Value) -> anyhow::Result<bool> {
        anyhow::ensure!(project::valid_path(file) && project::is_text(file), "not a text file path");
        let mut docs = self.docs.lock().unwrap();
        if let Some(want) = expected
            && docs.get(file).map(|d| d.text.as_str()) != Some(want)
        {
            return Ok(false);
        }
        let fresh = !docs.contains_key(file);
        let doc = docs.entry(file.to_string()).or_insert(DocState { text: String::new(), version: 0, saved: 0, prov: Map::default() });
        if !fresh && doc.text == text {
            return Ok(true);
        }
        if meta["source"] != "git" {
            self.touched(user);
        }
        // The origin of whatever this replacement inserts: a person for their
        // own new files and reconnects, one fresh origin per AI apply / push / restore / upload.
        let origin_kind = provenance::origin_kind(kind, &meta);
        let who = match origin_kind {
            Some(k) => self.history.new_origin(k, user, &meta).unwrap_or(provenance::UNKNOWN),
            None => self.human_origin(user),
        };
        let prov = doc.prov.rebase(&doc.text, &text, who);
        doc.text = text;
        doc.version += 1;
        let entry = self.history.record(file, user, kind, None, &doc.text, meta).ok();
        if origin_kind.is_some()
            && let Some(id) = entry
        {
            let _ = self.history.extend_origin(who, &json!({"entry": id, "file": file}));
        }
        doc.prov = prov;
        let _ = self.history.set_provenance(file, &doc.prov);
        let change = Change::Text(doc.text.as_str().into());
        let prov_msg = self.prov_value(&doc.prov);
        drop(docs);
        if fresh {
            self.save(file);
            if self.main().is_none() {
                self.refresh_main();
            }
            self.files_changed();
        }
        self.after_change(file, change, origin, user, prov_msg);
        Ok(true)
    }

    /// A project laid out from an upstream repository (`upstream::import`):
    /// record every text in the history as written by one `git` origin.
    pub fn imported(&self, who: &str, meta: Value) {
        let origin = self.history.new_origin("git", who, &meta).unwrap_or(provenance::UNKNOWN);
        let mut docs = self.docs.lock().unwrap();
        let mut last = None;
        for (file, doc) in docs.iter_mut() {
            last = self.history.record(file, who, "replace", None, &doc.text, meta.clone()).ok().or(last);
            doc.prov = Map::uniform(doc.text.encode_utf16().count(), origin);
            let _ = self.history.set_provenance(file, &doc.prov);
        }
        if let Some(id) = last {
            let _ = self.history.extend_origin(origin, &json!({"entry": id}));
        }
    }

    // ---- branches ------------------------------------------------------------

    pub fn branch(&self) -> String {
        self.history.branch()
    }

    fn valid_branch(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name != "HEAD"
            && !name.starts_with(['-', '.'])
            && !name.ends_with(['.', '/'])
            && !name.contains("..")
            && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c))
    }

    /// Commits what is on this branch, so git and the history agree before a switch.
    async fn settle(&self, user: &str, why: &str) {
        self.flush();
        if let Ok(Some(sha)) = crate::gitd::commit(&self.dir, why, Some(user)).await {
            self.committed(&sha, why, user).await;
        }
    }

    /// Makes the working tree be `texts` (the branch's files): every text file
    /// written, text files not in the set removed, provenance loaded or
    /// derived, everything announced so open editors reload.
    fn adopt(&self, texts: BTreeMap<String, String>, derive_from_current: bool) -> anyhow::Result<()> {
        let mut docs = self.docs.lock().unwrap();
        let old: BTreeMap<String, (String, Map)> = docs.iter().map(|(f, d)| (f.clone(), (d.text.clone(), d.prov.clone()))).collect();
        docs.clear();
        for (file, text) in texts {
            let prov = if derive_from_current {
                // A fresh branch: what survives from the text we just left keeps
                // its attribution; text that only existed back then is untracked.
                match old.get(&file) {
                    Some((otext, oprov)) => oprov.rebase(otext, &text, provenance::UNKNOWN),
                    None => Map::uniform(text.encode_utf16().count(), provenance::UNKNOWN),
                }
            } else {
                self.history.provenance(&file).unwrap_or_default().fitted(&text)
            };
            if derive_from_current {
                let _ = self.history.set_provenance(&file, &prov);
            }
            let path = self.dir.join(&file);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(&path, &text)?;
            docs.insert(file, DocState { text, version: 1, saved: 1, prov });
        }
        // Text files the snapshot lacks go away — but only ones the history
        // ever recorded (a file that exists outside the history, like the
        // .gitignore git itself needs, is not the snapshot's to remove).
        for file in old.keys() {
            if !docs.contains_key(file) && self.history.latest_for(file, i64::MAX).is_some() {
                let _ = fs::remove_file(self.dir.join(file));
            }
        }
        drop(docs);
        let _ = crate::gitd::ensure_ignore(&self.dir);
        let _ = fs::write(self.dir.join(project::PRIVATE_DIR).join("branch"), self.branch());
        self.refresh_main();
        self.files_changed();
        self.emit(Event::Branch { name: self.branch() });
        self.schedule_build();
        Ok(())
    }

    /// Forks `name` at `from` (an entry id; None = the current state) and
    /// switches to it. The new branch's working tree is that snapshot and a
    /// git branch of the same name is created, so clones see it too.
    pub async fn branch_from(&self, from: Option<i64>, name: &str, user: &str, note: &str) -> anyhow::Result<()> {
        anyhow::ensure!(Self::valid_branch(name), "branch names: letters, digits, . _ - /");
        anyhow::ensure!(!self.history.branch_exists(name), "branch {name} already exists");
        anyhow::ensure!(self.take_lock(user, false), "someone else holds the edit lock");
        let parent = self.branch();
        self.settle(user, "autosave before branching").await;
        // The fork point: the given entry, or the newest entry we have now.
        let from = match from {
            Some(id) => {
                let e = self.history.entry(id).ok_or_else(|| anyhow::anyhow!("no such entry"))?;
                anyhow::ensure!(!e.file.is_empty() || e.kind == "commit", "that entry has no text to branch from");
                id
            }
            None => self.history.list(None, None, None, 1, None).ok().and_then(|l| l.first().map(|e| e.id)).ok_or_else(|| anyhow::anyhow!("no history yet"))?,
        };
        let texts = self.history.files_at(from)?;
        anyhow::ensure!(!texts.is_empty(), "nothing in the history at that point");
        self.history.create_branch(name, &parent, from, user, note)?;
        crate::gitd::checkout(&self.dir, name, true).await?;
        self.history.set_branch(name);
        self.adopt(texts, true)?;
        let _ = self.history.note(user, "branch", json!({"from": from, "parent": parent}));
        self.settle(user, &format!("branch {name} from #{from}")).await;
        Ok(())
    }

    /// Switches the working tree to `name`'s latest state.
    pub async fn switch_branch(&self, name: &str, user: &str) -> anyhow::Result<()> {
        anyhow::ensure!(self.history.branch_exists(name), "no branch {name}");
        if self.branch() == name {
            return Ok(());
        }
        anyhow::ensure!(self.take_lock(user, false), "someone else holds the edit lock");
        self.settle(user, "autosave before switching branch").await;
        let mut texts = BTreeMap::new();
        for (file, entry) in self.history.branch_files(name) {
            if let Some(t) = self.history.text_at(entry)? {
                texts.insert(file, t);
            }
        }
        crate::gitd::checkout(&self.dir, name, false).await?;
        self.history.set_branch(name);
        self.adopt(texts, false)?;
        Ok(())
    }

    // ---- provenance ----------------------------------------------------------

    /// `user`'s origin id (one per person per project, cached).
    pub fn human_origin(&self, user: &str) -> u32 {
        if let Some(id) = self.human_origins.lock().unwrap().get(user) {
            return *id;
        }
        let id = self.history.human_origin(user).unwrap_or(provenance::UNKNOWN);
        self.human_origins.lock().unwrap().insert(user.to_string(), id);
        id
    }

    /// `{spans, origins}` — what the browser needs to colour a file.
    fn prov_value(&self, map: &Map) -> Value {
        let origins: serde_json::Map<String, Value> = self
            .history
            .origins(map.origins())
            .into_iter()
            .map(|o| (o.id.to_string(), serde_json::to_value(&o).unwrap_or(Value::Null)))
            .collect();
        json!({"spans": map.to_json(), "origins": origins})
    }

    pub fn provenance(&self, file: &str) -> Option<Value> {
        let docs = self.docs.lock().unwrap();
        docs.get(file).map(|d| self.prov_value(&d.prov))
    }

    pub fn origins(&self, ids: Vec<u32>) -> Vec<crate::provenance::Origin> {
        self.history.origins(ids)
    }

    /// Marks `from..to` (UTF-16 offsets) of `file` as written by `user`.
    pub fn claim(&self, file: &str, from: usize, to: usize, user: &str) -> anyhow::Result<()> {
        let me = self.human_origin(user);
        let mut docs = self.docs.lock().unwrap();
        let doc = docs.get_mut(file).ok_or_else(|| anyhow::anyhow!("no such file"))?;
        doc.prov.set(from, to, me);
        let _ = self.history.set_provenance(file, &doc.prov);
        let prov = self.prov_value(&doc.prov);
        drop(docs);
        self.emit(Event::Provenance { file: file.into(), prov });
        Ok(())
    }

    /// Recomputes every file's map from the history (see `Store::rebuild_provenance`)
    /// and tells open editors.
    pub fn rebuild_provenance(&self) -> anyhow::Result<usize> {
        let n = self.history.rebuild_provenance()?;
        let mut events = Vec::new();
        {
            let mut docs = self.docs.lock().unwrap();
            for (file, doc) in docs.iter_mut() {
                doc.prov = self.history.provenance(file).unwrap_or_default().fitted(&doc.text);
                events.push(Event::Provenance { file: file.clone(), prov: self.prov_value(&doc.prov) });
            }
        }
        for e in events {
            self.emit(e);
        }
        Ok(n)
    }

    fn after_change(&self, file: &str, change: Change, origin: u64, user: &str, prov: Value) {
        self.emit(Event::Changed { file: file.into(), change, origin, user: user.into(), prov });
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
        self.files_changed();
        self.schedule_build();
        Ok(())
    }

    pub fn delete_file(&self, path: &str, user: &str) -> anyhow::Result<()> {
        anyhow::ensure!(project::valid_path(path), "invalid path");
        let full = self.dir.join(path);
        anyhow::ensure!(full.is_file(), "no such file");
        if let Some(doc) = self.docs.lock().unwrap().remove(path) {
            let _ = self.history.record(path, user, "delete", None, "", json!({"size": doc.text.len()}));
            self.history.delete_provenance(path);
        }
        fs::remove_file(full)?;
        if self.main().as_deref() == Some(path) {
            self.refresh_main();
        }
        self.files_changed();
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
        self.refresh_main();
        self.files_changed();
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
        let pdf = compiled.pdf.filter(|_| !compiled.stale).ok_or_else(|| anyhow::anyhow!("that version does not compile to a PDF"))?;
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
                diagnostics: vec![Diagnostic::new("", None, build::Severity::Error, "no main .tex file in this project")],
                ms: 0,
                pdf: None,
                pdf_changed: false,
                stale: false,
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
            Ok(Compiled { ok, stale, diagnostics, pdf, log, ms }) => {
                let hash = pdf.as_ref().map(|b| {
                    b.iter().fold(0xcbf29ce484222325u64, |h, &x| (h ^ x as u64).wrapping_mul(0x100000001b3))
                });
                let pdf_changed = pdf.is_some() && !stale && hash != last_hash;
                if pdf_changed {
                    version += 1;
                    last_hash = hash;
                }
                let pdf = match pdf {
                    Some(b) => Some(Arc::new(b)),
                    None => p.latest().and_then(|l| l.pdf.clone()),
                };
                let stale = stale && pdf.is_some();
                BuildOutcome { version, ok, diagnostics, ms, pdf, pdf_changed, stale, log }
            }
            Err(e) => BuildOutcome {
                version,
                ok: false,
                diagnostics: vec![Diagnostic::new(main.clone(), None, build::Severity::Error, format!("could not run latexmk: {e:#}"))],
                ms: 0,
                pdf: p.latest().and_then(|l| l.pdf.clone()),
                pdf_changed: false,
                stale: true,
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
    /// After a reconnect: the browser's text, which has edits the server never
    /// saw (typed while the socket was down). Replaces the server's copy —
    /// through `replace`, so provenance is rebased and the history records it.
    Resync { file: String, text: String, hash: u32 },
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

type Sink = futures_util::stream::SplitSink<WebSocket, Message>;

/// An edit the server would not take: say why, send its own text (so the
/// client heals), and the lock when that was the reason.
async fn refuse(sink: &mut Sink, p: &Project, file: &str, reason: &str) -> Result<(), axum::Error> {
    sink.send(msg(json!({"type": "refused", "file": file, "reason": reason}))).await?;
    if let Some(t) = p.text(file) {
        sink.send(msg(json!({"type": "text", "file": file, "text": t, "prov": p.provenance(file)}))).await?;
    }
    if reason == "locked" {
        sink.send(msg(json!({"type": "lock", "lock": p.lock()}))).await?;
    }
    Ok(())
}

async fn run(socket: WebSocket, app: crate::Shared, p: Project, user: String) -> Result<(), axum::Error> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut rx = p.subscribe();
    let (mut sink, mut stream) = socket.split();
    sink.send(msg(json!({
        "type": "init", "user": user, "files": p.files(), "main": p.main(), "lock": p.lock(),
        "build": p.latest().map(|b| b.summary()), "building": p.building.load(Ordering::Relaxed),
        "ai": app.ai.describe(),
        "origin": p.human_origin(&user),
        "branch": p.branch(),
        "upstream": crate::upstream::tracked(&p.dir),
    })))
    .await?;
    p.viewers.fetch_add(1, Ordering::Relaxed);
    let result = serve(&mut sink, &mut stream, &mut rx, &app, &p, &user, id).await;
    p.viewers.fetch_sub(1, Ordering::Relaxed);
    result
}

type Stream = futures_util::stream::SplitStream<WebSocket>;

async fn serve(sink: &mut Sink, stream: &mut Stream, rx: &mut broadcast::Receiver<Arc<Event>>, app: &crate::Shared, p: &Project, user: &str, id: u64) -> Result<(), axum::Error> {
    let (app, user) = (app.clone(), user.to_string());
    loop {
        tokio::select! {
            m = stream.next() => {
                let Some(Ok(Message::Text(text))) = m else {
                    if matches!(m, Some(Ok(_))) { continue } else { break }
                };
                match serde_json::from_str::<ClientMsg>(&text) {
                    Ok(ClientMsg::Open { file }) => match p.text(&file) {
                        Some(t) => sink.send(msg(json!({"type": "text", "file": file, "text": t, "prov": p.provenance(&file)}))).await?,
                        None => sink.send(msg(json!({"type": "refused", "file": file, "reason": "no such file"}))).await?,
                    },
                    Ok(ClientMsg::Edit { file, changes, hash }) => {
                        if let Err(reason) = p.edit(&file, &changes, hash, id, &user) {
                            refuse(sink, p, &file, reason).await?;
                        }
                    }
                    Ok(ClientMsg::Resync { file, text, hash }) => {
                        if text::fnv1a(&text) != hash {
                            continue; // mangled in transit; the client will ask again
                        }
                        if !p.take_lock(&user, false) {
                            refuse(sink, p, &file, "locked").await?;
                            continue;
                        }
                        match p.replace(&file, text, id, &user, "replace", json!({"source": "reconnect"})) {
                            Ok(()) => sink.send(msg(json!({"type": "resynced", "file": file, "prov": p.provenance(&file)}))).await?,
                            Err(e) => sink.send(msg(json!({"type": "refused", "file": file, "reason": e.to_string()}))).await?,
                        }
                    }
                    Ok(ClientMsg::Ping) => p.renew_lock(&user),
                    Ok(ClientMsg::Lock { action }) => {
                        let _ = p.lock_action(&user, &action);
                        sink.send(msg(json!({"type": "lock", "lock": p.lock()}))).await?;
                    }
                    Ok(ClientMsg::Ai(v)) => crate::ai::handle(app.clone(), p.clone(), user.clone(), id, v),
                    Err(_) => {}
                }
            }
            update = rx.recv() => match update {
                Ok(ev) => {
                    let out = match &*ev {
                        Event::Changed { file, change, origin, user: who, prov } => {
                            if *origin == id { continue }
                            match change {
                                Change::Delta(changes) => json!({"type": "edit", "file": file, "changes": changes, "user": who, "origin": prov}),
                                Change::Text(text) => json!({"type": "text", "file": file, "text": &**text, "prov": prov}),
                            }
                        }
                        Event::Provenance { file, prov } => json!({"type": "prov", "file": file, "prov": prov}),
                        Event::Saved { file, error } => json!({"type": "saved", "file": file, "error": error}),
                        Event::Building => json!({"type": "building"}),
                        Event::Built(b) => { let mut v = b.summary(); v["type"] = "built".into(); v }
                        Event::Lock(l) => json!({"type": "lock", "lock": l}),
                        Event::Files { files, main } => json!({"type": "files", "files": files, "main": main}),
                        Event::Committed { sha, message } => json!({"type": "committed", "sha": sha, "message": message}),
                        Event::Branch { name } => json!({"type": "branch", "branch": name, "files": p.files(), "main": p.main()}),
                        Event::Ai(v) => { let mut v = v.clone(); v["type"] = "ai".into(); v }
                        Event::Upstream(v) => { let mut v = v.clone(); v["type"] = "upstream".into(); v }
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
    async fn branches_fork_switch_and_keep_their_own_history() {
        let root = root();
        let p = Project::create(&root, "b", Duration::from_secs(30)).await.unwrap();
        p.replace("main.tex", "one\n".into(), 0, "kris", "replace", json!({})).unwrap();
        let at_one = p.history.head("main.tex").unwrap();
        p.replace("main.tex", "one two\n".into(), 0, "kris", "replace", json!({})).unwrap();
        p.replace("notes.tex", "scratch\n".into(), 0, "kris", "replace", json!({})).unwrap();
        assert_eq!(p.branch(), "main");
        // Fork at the "one" snapshot: the working tree becomes that snapshot.
        p.branch_from(Some(at_one), "idea", "kris", "try something").await.unwrap();
        assert_eq!(p.branch(), "idea");
        assert_eq!(p.text("main.tex").unwrap(), "one\n");
        assert!(p.text("notes.tex").is_none(), "a file created after the fork point is not on the branch");
        assert!(!p.dir.join("notes.tex").exists());
        assert_eq!(crate::gitd::current_branch(&p.dir).await.unwrap(), "idea");
        // Work on the branch; main is untouched.
        p.replace("main.tex", "one three\n".into(), 0, "kris", "replace", json!({})).unwrap();
        let idea_head = p.history.head("main.tex").unwrap();
        assert_eq!(p.history.entry(idea_head).unwrap().branch, "idea");
        assert_eq!(p.history.text_at(idea_head).unwrap().unwrap(), "one three\n");
        // The branch's timeline is its lineage: its own entry plus main up to the fork.
        let ids: Vec<i64> = p.history.list(Some("main.tex"), None, None, 50, None).unwrap().iter().map(|e| e.id).collect();
        assert!(ids.contains(&idea_head) && ids.contains(&at_one), "{ids:?}");
        assert!(!ids.iter().any(|i| p.history.entry(*i).unwrap().meta.get("x").is_some()));
        let main_texts = p.history.files_at(idea_head).unwrap();
        assert_eq!(main_texts.get("main.tex").map(String::as_str), Some("one three\n"));
        assert!(main_texts.get("notes.tex").is_none());
        // Back to main: its latest state, notes.tex included.
        p.switch_branch("main", "kris").await.unwrap();
        assert_eq!(p.text("main.tex").unwrap(), "one two\n");
        assert_eq!(p.text("notes.tex").unwrap(), "scratch\n");
        assert_eq!(crate::gitd::current_branch(&p.dir).await.unwrap(), "main");
        let branches = p.history.branches();
        assert_eq!(branches.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(), vec!["main", "idea"]);
        assert_eq!(branches[1].from_entry, Some(at_one));
        let tree = p.history.tree(100).unwrap();
        assert!(tree.iter().any(|e| e.branch == "idea" && e.kind == "replace"));
        assert!(p.history.story(5).starts_with("Branch: main"));
        assert!(p.branch_from(None, "bad name", "kris", "").await.is_err());
        std::fs::remove_dir_all(root).unwrap();
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
