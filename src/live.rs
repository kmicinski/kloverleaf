//! Live projects: the in-memory text of every file, who is here, the build
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
//!   {"type":"init","user","files","main","here","build","ai"}   on connect
//!   {"type":"text","file","text","rev"}    whole text at revision `rev`: answer to `open`,
//!                                          or when an edit could not be placed
//!   {"type":"edit","file","rev","op","changes","user","prov","client","seq"}
//!                                          revision `rev` of the file (anyone's change, already
//!                                          transformed); `changes` is `op` as Monaco edits
//!   {"type":"ack","file","rev","seq"}      your op `seq` became revision `rev`
//!   {"type":"caught","file","rev"}         the end of the `edit`s answering `open` with `rev`
//!   {"type":"here","here":[{id,user,file,line,column}]} who has the project open, and where
//!   {"type":"chat","msg"}                  a chat message, new or changed (resolved, deleted)
//!   {"type":"saved","file","error"?}       the write to disk finished
//!   {"type":"building"}                    latexmk started
//!   {"type":"built","version","ok","diags","ms","pdf"}
//!                                          latexmk finished; `pdf` says whether the PDF changed
//!   {"type":"files","files","main"}        the file list changed
//!   {"type":"committed","sha","message"}
//!   {"type":"refused","file","reason"}     an edit was not applied (followed by `text`)
//!   {"type":"ai", …}                       see `ai`
//! client → server
//!   {"type":"open","file","rev"?}          the text; with `rev`, the revisions since it as
//!                                          `edit`s instead when the log still has them
//!   {"type":"edit","file","rev","op","client","seq"}
//!                                          an ot.js op (see `ot.rs`) against revision `rev`
//!   {"type":"resync","file","text","hash"} unsent work the log can no longer place: ours wins
//!   {"type":"viewing","file","line"?,"column"?}  where this page's cursor is (for `here`)
//!   {"type":"chat","text","anchor"?}       post to the project's chat (`chat.rs`); an anchor pins it
//!   {"type":"chat-resolve","id","resolved"} / {"type":"chat-delete","id"}
//!   {"type":"ai", …}                       see `ai`
//! ```
//!
//! Everyone edits at once, as in Overleaf. Each file has a revision number
//! and a log of the ops that made the recent revisions; an op sent against an
//! older revision is transformed past the ones it missed (`ot::transform`),
//! applied, and broadcast — to its sender as an `ack`, through the same
//! channel, so a page always sees the revisions before its own in order. The
//! page does the mirror image for its unacknowledged ops (`web/src/ot.ts`).

use crate::build::{self, Compiled, Diagnostic};
use crate::history::Store;
use crate::provenance::{self, Map};
use crate::project::{self, FileInfo};
use crate::ot::{self, Op};
use crate::text;
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
/// Revisions kept per file for transforming late ops and catching pages up.
const LOG_KEEP: usize = 2000;

/// One revision of a file, as the log keeps it and the socket sends it.
#[derive(Clone)]
pub struct Rev {
    pub rev: u64,
    pub op: Op,
    /// The page (and its op number) that sent it; `None` for tools, the AI, git.
    pub client: Option<String>,
    pub seq: u64,
    pub user: String,
    /// The typist's origin id for a keystroke, `{spans, origins}` for a replacement.
    pub prov: Value,
}

impl Rev {
    fn wire(&self, file: &str) -> Value {
        json!({"type": "edit", "file": file, "rev": self.rev, "op": self.op.to_json(), "changes": self.op.to_edits(),
               "user": self.user, "prov": self.prov, "client": self.client, "seq": self.seq})
    }
}

/// A page with the project open.
#[derive(Debug, Clone, Serialize)]
pub struct Here {
    pub id: u64,
    pub user: String,
    pub file: Option<String>,
    pub line: Option<u32>,
    pub column: Option<u32>,
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
    /// A new revision of a file; `origin` is the connection that sent it (0 = none).
    Changed { file: String, rev: Rev, origin: u64 },
    /// The provenance of a file changed without its text (a claim).
    Provenance { file: String, prov: Value },
    Saved { file: String, error: Option<String> },
    Building,
    Built(Arc<BuildOutcome>),
    Here(Vec<Here>),
    Chat(crate::chat::Msg),
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
    /// The revision: bumped by every change, never reset while the server runs.
    version: u64,
    /// The ops that made the last `LOG_KEEP` revisions, oldest first.
    log: std::collections::VecDeque<Rev>,
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
    here: Mutex<BTreeMap<u64, Here>>,
    chat: Mutex<crate::chat::Chat>,
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
                docs.insert(f.path.clone(), DocState { text, version: 0, log: Default::default(), saved: 0, prov });
            }
        }
        let main = project::main_file(&dir, &files);
        let chat = crate::chat::Chat::open(&dir.join(project::PRIVATE_DIR));
        let (build_tx, build_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            name: name.into(),
            dir,
            docs: Mutex::new(docs),
            history,
            here: Mutex::new(BTreeMap::new()),
            chat: Mutex::new(chat),
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

    // ---- who is here --------------------------------------------------------

    pub fn here(&self) -> Vec<Here> {
        self.here.lock().unwrap().values().cloned().collect()
    }

    /// The people with the project open, each once.
    pub fn here_users(&self) -> Vec<String> {
        let mut u: Vec<String> = self.here.lock().unwrap().values().map(|h| h.user.clone()).collect();
        u.sort();
        u.dedup();
        u
    }

    // ---- chat ------------------------------------------------------------------

    pub fn chat_recent(&self, n: usize) -> Vec<crate::chat::Msg> {
        self.chat.lock().unwrap().recent(n)
    }

    pub fn chat_post(&self, user: &str, text: &str, anchor: Option<crate::chat::Anchor>) -> anyhow::Result<crate::chat::Msg> {
        let m = self.chat.lock().unwrap().post(user, text, anchor)?;
        self.emit(Event::Chat(m.clone()));
        Ok(m)
    }

    pub fn chat_resolve(&self, id: u64, user: &str, resolved: bool) -> anyhow::Result<()> {
        let m = self.chat.lock().unwrap().resolve(id, user, resolved)?;
        self.emit(Event::Chat(m));
        Ok(())
    }

    pub fn chat_delete(&self, id: u64, user: &str) -> anyhow::Result<()> {
        let m = self.chat.lock().unwrap().delete(id, user)?;
        self.emit(Event::Chat(m));
        Ok(())
    }

    fn set_here(&self, id: u64, h: Option<Here>) {
        {
            let mut g = self.here.lock().unwrap();
            match h {
                Some(h) => g.insert(id, h),
                None => g.remove(&id),
            };
        }
        self.emit(Event::Here(self.here()));
    }

    // ---- editing -------------------------------------------------------------

    /// Applies a page's op, made against revision `base`, on behalf of
    /// `user`: transformed past the revisions it missed, applied, logged,
    /// broadcast. Returns the new revision. `Err(reason)` when it cannot be
    /// placed (a revision the log no longer has, or an op for another text);
    /// nothing changes then.
    #[allow(clippy::too_many_arguments)]
    pub fn edit(&self, file: &str, base: u64, op: Op, client: &str, seq: u64, origin: u64, user: &str) -> Result<u64, &'static str> {
        let mut docs = self.docs.lock().unwrap();
        let Some(doc) = docs.get_mut(file) else { return Err("no such file") };
        if base > doc.version {
            return Err("desync");
        }
        let missed = (doc.version - base) as usize;
        if missed > doc.log.len() {
            return Err("desync");
        }
        let mut op = op;
        for r in doc.log.iter().skip(doc.log.len() - missed) {
            op = ot::transform(&op, &r.op)?.0;
        }
        if op.base != doc.text.encode_utf16().count() {
            return Err("desync");
        }
        let changes = op.to_edits();
        doc.text = text::apply(&doc.text, &changes);
        doc.version += 1;
        self.touched(user);
        let _ = self.history.record(file, user, "delta", Some(&changes), &doc.text, json!({}));
        let me = self.human_origin(user);
        doc.prov.apply(&changes, me);
        let _ = self.history.set_provenance(file, &doc.prov);
        let rev = Rev { rev: doc.version, op, client: Some(client.into()), seq, user: user.into(), prov: json!(me) };
        let n = rev.rev;
        // Broadcast under the lock: revisions must reach every page in order.
        self.emit(Event::Changed { file: file.into(), rev: rev.clone(), origin });
        Self::log(doc, rev);
        drop(docs);
        self.after_change(file);
        Ok(n)
    }

    fn log(doc: &mut DocState, rev: Rev) {
        doc.log.push_back(rev);
        while doc.log.len() > LOG_KEEP {
            doc.log.pop_front();
        }
    }

    /// The text and its revision, or — when `since` is given and the log
    /// still reaches back to it — the revisions after `since`.
    pub fn catch_up(&self, file: &str, since: Option<u64>) -> Option<Result<(Vec<Value>, u64), (String, u64)>> {
        let docs = self.docs.lock().unwrap();
        let doc = docs.get(file)?;
        if let Some(since) = since
            && since <= doc.version
            && (doc.version - since) as usize <= doc.log.len()
        {
            let n = (doc.version - since) as usize;
            return Some(Ok((doc.log.iter().skip(doc.log.len() - n).map(|r| r.wire(file)).collect(), doc.version)));
        }
        Some(Err((doc.text.clone(), doc.version)))
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
        let doc = docs.entry(file.to_string()).or_insert(DocState { text: String::new(), version: 0, log: Default::default(), saved: 0, prov: Map::default() });
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
        // It travels as the smallest replacement, so typing elsewhere in the file survives it.
        let op = ot::Op::diff(&doc.text, &text);
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
        let rev = Rev { rev: doc.version, op, client: None, seq: 0, user: user.into(), prov: self.prov_value(&doc.prov) };
        self.emit(Event::Changed { file: file.into(), rev: rev.clone(), origin });
        Self::log(doc, rev);
        drop(docs);
        if fresh {
            self.save(file);
            if self.main().is_none() {
                self.refresh_main();
            }
            self.files_changed();
        }
        self.after_change(file);
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
        // Revisions keep counting up across the switch (with an empty log), so
        // a page's op against the old branch can never be placed on this one.
        let next_rev = docs.values().map(|d| d.version).max().unwrap_or(0) + 1;
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
            docs.insert(file, DocState { text, version: next_rev, log: Default::default(), saved: next_rev, prov });
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

    /// After a revision was broadcast: compile soon, save once typing pauses.
    fn after_change(&self, file: &str) {
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
    Open { file: String, rev: Option<u64> },
    Edit { file: String, rev: u64, op: Value, client: String, seq: u64 },
    /// The browser's text, with edits the server never saw and can no longer
    /// place (its log moved on, or the page was offline long). Replaces the
    /// server's copy — through `replace`, so provenance is rebased and the
    /// history records it.
    Resync { file: String, text: String, hash: u32 },
    Viewing { file: Option<String>, line: Option<u32>, column: Option<u32> },
    Chat { text: String, anchor: Option<crate::chat::Anchor> },
    #[serde(rename = "chat-resolve")]
    ChatResolve { id: u64, resolved: bool },
    #[serde(rename = "chat-delete")]
    ChatDelete { id: u64 },
    /// Older pages ping; nothing to keep alive any more.
    Ping,
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

/// Sends a file's text at its revision, or the revisions since `since`.
async fn send_text(sink: &mut Sink, p: &Project, file: &str, since: Option<u64>) -> Result<(), axum::Error> {
    match p.catch_up(file, since) {
        Some(Ok((revs, rev))) => {
            for r in revs {
                sink.send(msg(r)).await?;
            }
            sink.send(msg(json!({"type": "caught", "file": file, "rev": rev}))).await?;
        }
        Some(Err((text, rev))) => {
            sink.send(msg(json!({"type": "text", "file": file, "text": text, "rev": rev, "prov": p.provenance(file)}))).await?;
        }
        None => sink.send(msg(json!({"type": "refused", "file": file, "reason": "no such file"}))).await?,
    }
    Ok(())
}

async fn run(socket: WebSocket, app: crate::Shared, p: Project, user: String) -> Result<(), axum::Error> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut rx = p.subscribe();
    let (mut sink, mut stream) = socket.split();
    p.set_here(id, Some(Here { id, user: user.clone(), file: None, line: None, column: None }));
    let sent = sink
        .send(msg(json!({
            "type": "init", "user": user, "files": p.files(), "main": p.main(), "here": p.here(), "conn": id,
            "chat": p.chat_recent(500),
            "build_id": crate::pages::build_id(),
            "build": p.latest().map(|b| b.summary()), "building": p.building.load(Ordering::Relaxed),
            "ai": app.ai.describe(),
            "origin": p.human_origin(&user),
            "branch": p.branch(),
            "upstream": crate::upstream::tracked(&p.dir),
            // Pages loaded before the move to concurrent editing speak the old
            // edit format, which this server ignores: their own code reads this
            // as "someone holds the lock", turns read-only and shows the name.
            "lock": {"user": "kloverleaf was updated — reload this page;", "since": 0, "until": i64::MAX},
        })))
        .await;
    p.viewers.fetch_add(1, Ordering::Relaxed);
    let result = match sent {
        Ok(()) => serve(&mut sink, &mut stream, &mut rx, &app, &p, &user, id).await,
        Err(e) => Err(e),
    };
    p.viewers.fetch_sub(1, Ordering::Relaxed);
    p.set_here(id, None);
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
                    Ok(ClientMsg::Open { file, rev }) => send_text(sink, p, &file, rev).await?,
                    Ok(ClientMsg::Edit { file, rev, op, client, seq }) => {
                        let placed = ot::Op::from_json(&op).and_then(|op| p.edit(&file, rev, op, &client, seq, id, &user));
                        if let Err(reason) = placed {
                            // The page heals from the text; its unplaced work comes back as a resync.
                            sink.send(msg(json!({"type": "refused", "file": file, "reason": reason}))).await?;
                            send_text(sink, p, &file, None).await?;
                        }
                    }
                    Ok(ClientMsg::Resync { file, text, hash }) => {
                        if text::fnv1a(&text) != hash {
                            continue; // mangled in transit; the client will ask again
                        }
                        match p.replace(&file, text, id, &user, "replace", json!({"source": "reconnect"})) {
                            Ok(()) => {
                                let rev = p.catch_up(&file, None).and_then(|r| r.err()).map(|(_, rev)| rev);
                                sink.send(msg(json!({"type": "resynced", "file": file, "rev": rev, "prov": p.provenance(&file)}))).await?
                            }
                            Err(e) => sink.send(msg(json!({"type": "refused", "file": file, "reason": e.to_string()}))).await?,
                        }
                    }
                    Ok(ClientMsg::Viewing { file, line, column }) => p.set_here(id, Some(Here { id, user: user.clone(), file, line, column })),
                    Ok(ClientMsg::Chat { text, anchor }) => {
                        if let Err(e) = p.chat_post(&user, &text, anchor) {
                            sink.send(msg(json!({"type": "chat-error", "message": e.to_string()}))).await?;
                        }
                    }
                    Ok(ClientMsg::ChatResolve { id: m, resolved }) => {
                        if let Err(e) = p.chat_resolve(m, &user, resolved) {
                            sink.send(msg(json!({"type": "chat-error", "message": e.to_string()}))).await?;
                        }
                    }
                    Ok(ClientMsg::ChatDelete { id: m }) => {
                        if let Err(e) = p.chat_delete(m, &user) {
                            sink.send(msg(json!({"type": "chat-error", "message": e.to_string()}))).await?;
                        }
                    }
                    Ok(ClientMsg::Ping) => {}
                    Ok(ClientMsg::Ai(v)) => crate::ai::handle(app.clone(), p.clone(), user.clone(), id, v),
                    Err(_) => {}
                }
            }
            update = rx.recv() => match update {
                Ok(ev) => {
                    let out = match &*ev {
                        Event::Changed { file, rev, origin } => {
                            if *origin == id {
                                // Our own op: an ack, in order with the revisions before it.
                                // (A resync's replacement is answered by `resynced` instead.)
                                if rev.client.is_none() { continue }
                                json!({"type": "ack", "file": file, "rev": rev.rev, "seq": rev.seq})
                            } else {
                                rev.wire(file)
                            }
                        }
                        Event::Provenance { file, prov } => json!({"type": "prov", "file": file, "prov": prov}),
                        Event::Saved { file, error } => json!({"type": "saved", "file": file, "error": error}),
                        Event::Building => json!({"type": "building"}),
                        Event::Built(b) => { let mut v = b.summary(); v["type"] = "built".into(); v }
                        Event::Here(h) => json!({"type": "here", "here": h}),
                        Event::Chat(m) => json!({"type": "chat", "msg": m}),
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

    fn op(edits: &[(usize, usize, &str)], len: usize) -> Op {
        let e: Vec<text::Edit> = edits.iter().map(|(o, l, t)| text::Edit { offset: *o, length: *l, text: (*t).into() }).collect();
        Op::from_edits(&e, len).unwrap()
    }

    #[tokio::test]
    async fn concurrent_editors_both_land() {
        let root = root();
        let p = Project::create(&root, "t", Duration::from_secs(30)).await.unwrap();
        assert_eq!(p.main().as_deref(), Some("main.tex"));
        p.replace("main.tex", "hello world".into(), 0, "kris", "replace", json!({})).unwrap();
        let Some(Err((_, r0))) = p.catch_up("main.tex", None) else { panic!() };
        // Kris and Tom both edit revision r0; Tom's arrives second and is transformed.
        let r1 = p.edit("main.tex", r0, op(&[(6, 5, "there"), (0, 5, "hi")], 11), "k", 1, 1, "kris").unwrap();
        assert_eq!(r1, r0 + 1);
        assert_eq!(p.text("main.tex").unwrap(), "hi there");
        let r2 = p.edit("main.tex", r0, op(&[(11, 0, "!")], 11), "t", 1, 2, "tom").unwrap();
        assert_eq!(p.text("main.tex").unwrap(), "hi there!");
        // A tool replaces the text while Kris's next op is still in flight against r2.
        p.replace("main.tex", "hi there! (ed.)".into(), 0, "ai", "replace", json!({})).unwrap();
        p.edit("main.tex", r2, op(&[(0, 0, "Oh, ")], 9), "k", 2, 1, "kris").unwrap();
        assert_eq!(p.text("main.tex").unwrap(), "Oh, hi there! (ed.)");
        // A page that missed revisions is caught up with the ops, in order.
        let Some(Ok((revs, _))) = p.catch_up("main.tex", Some(r1)) else { panic!() };
        assert_eq!(revs.len(), 3);
        assert_eq!(revs[0]["user"], "tom");
        assert_eq!(revs[2]["client"], "k");
        // Unplaceable: from the future, or for a different text.
        assert_eq!(p.edit("main.tex", 999, op(&[], 19), "k", 3, 1, "kris"), Err("desync"));
        assert!(p.edit("main.tex", r2 + 2, op(&[(0, 0, "x")], 3), "k", 3, 1, "kris").is_err());
        assert!(p.here().is_empty());
        p.set_here(7, Some(Here { id: 7, user: "tom".into(), file: Some("main.tex".into()), line: Some(3), column: Some(1) }));
        assert_eq!(p.here_users(), vec!["tom".to_string()]);
        // history has it all
        let head = p.history.head("main.tex").unwrap();
        assert_eq!(p.history.text_at(head).unwrap().unwrap(), "Oh, hi there! (ed.)");
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
