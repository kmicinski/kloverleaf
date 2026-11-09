//! The edit log: every change to every text file of a project, durable, as a
//! tree. One SQLite file per project (`.kloverleaf/history.sqlite`).
//!
//! The scheme is log-structured: an entry is a *delta* (Monaco's changes) or
//! a *replacement* (whole text — tools, AI, git pushes, restores), and points
//! at its parent. Full texts are stored only at *snapshots*: every
//! non-delta entry, and otherwise every `SNAPSHOT_EVERY` entries or
//! `SNAPSHOT_BYTES` bytes of delta along a chain. Reading the text at any
//! entry is "nearest snapshot above, replay the deltas below", bounded by the
//! snapshot policy, so a lookup touches at most a few hundred rows.
//!
//! Entries reference git: a `commit` entry (file `""`) records the sha of each
//! autocommit, so the log in git and the log here can be cross-read. AI edits
//! carry the prompt and model in `meta`.
//!
//! Heads are per file; `restore` creates a new entry whose parent is the
//! current head (history never rewinds), with `meta.restores` naming the
//! source. Branches therefore appear only in the parent links, never in the
//! heads, which keeps "current text" unambiguous.

use crate::provenance::{self, Map, Origin};
use crate::text::{self, Edit};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OptionalExtension, Params, Row, params, params_from_iter};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const SNAPSHOT_EVERY: i64 = 100;
const SNAPSHOT_BYTES: i64 = 32 * 1024;

/// Bumped when the attribution rules change (`provenance::Map::rebase`):
/// a store below this version has every map rebuilt from the log on open.
const PROVENANCE_VERSION: i64 = 1;

pub struct Store {
    conn: Mutex<Connection>,
    /// The branch new entries go to and heads are read from (see `branches`).
    branch: Mutex<String>,
}

/// A line of development: forked from `parent` at `from_entry` (None for `main`).
#[derive(Debug, Clone, Serialize)]
pub struct Branch {
    pub name: String,
    pub parent: Option<String>,
    pub from_entry: Option<i64>,
    pub created: i64,
    pub created_by: String,
    pub note: String,
    /// Latest entry id on the branch itself, and its time.
    pub head: Option<i64>,
    pub head_ts: Option<i64>,
    pub entries: i64,
}

/// One node of the history tree (see `Store::tree`).
#[derive(Debug, Clone, Serialize)]
pub struct TreeEvent {
    pub id: i64,
    pub first: i64,
    pub count: i64,
    pub ts: i64,
    pub branch: String,
    pub file: String,
    pub author: String,
    pub kind: String,
    pub label: String,
}

/// One slice of the timeline (see `Store::summary`).
#[derive(Debug, Clone, Serialize)]
pub struct Bucket {
    pub t0: i64,
    pub t1: i64,
    pub count: i64,
    pub first: i64,
    pub last: i64,
    pub kinds: BTreeMap<String, i64>,
    pub authors: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: i64,
    pub file: String,
    pub parent: Option<i64>,
    pub ts: i64,
    pub author: String,
    pub kind: String,
    pub hash: u32,
    pub size: i64,
    pub meta: Value,
    /// For coalesced runs of keystrokes: the first entry id and how many.
    pub first: i64,
    pub count: i64,
    pub branch: String,
}

/// A fresh scratch directory for tests (unique even within one millisecond).
#[cfg(test)]
pub fn tmp_dir(prefix: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "kl-{prefix}-{}-{}-{}",
        std::process::id(),
        now_ms(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Every row of a query, mapped; an error (a table that is not there yet) is an empty list.
pub fn query_all<T>(conn: &Connection, sql: &str, p: impl Params, f: impl FnMut(&Row) -> rusqlite::Result<T>) -> Vec<T> {
    conn.prepare(sql)
        .and_then(|mut stmt| stmt.query_map(p, f).map(|rows| rows.flatten().collect()))
        .unwrap_or_default()
}

/// `[(branch, upto)]` as SQL: "entry `e` lies on this lineage", with its parameters.
fn lineage_sql(lineage: &[(String, Option<i64>)]) -> (String, Vec<Sql>) {
    let mut parts = Vec::new();
    let mut params = Vec::new();
    for (b, upto) in lineage {
        params.push(Sql::from(b.clone()));
        match upto {
            None => parts.push("e.branch=?"),
            Some(u) => {
                parts.push("(e.branch=? AND e.id<=?)");
                params.push(Sql::from(*u));
            }
        }
    }
    (format!("({})", parts.join(" OR ")), params)
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Store> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS entries (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               file TEXT NOT NULL,
               parent INTEGER,
               ts INTEGER NOT NULL,
               author TEXT NOT NULL,
               kind TEXT NOT NULL,
               changes TEXT,
               hash INTEGER NOT NULL,
               size INTEGER NOT NULL,
               depth INTEGER NOT NULL,
               bytes INTEGER NOT NULL,
               meta TEXT NOT NULL DEFAULT '{}'
             );
             CREATE INDEX IF NOT EXISTS entries_file ON entries(file, id);
             CREATE TABLE IF NOT EXISTS snapshots (entry INTEGER PRIMARY KEY, text TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS heads (file TEXT PRIMARY KEY, entry INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS origins (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               kind TEXT NOT NULL,
               user TEXT NOT NULL,
               ts INTEGER NOT NULL,
               meta TEXT NOT NULL DEFAULT '{}'
             );
             CREATE TABLE IF NOT EXISTS provenance (file TEXT PRIMARY KEY, spans TEXT NOT NULL);",
        )?;
        // Branches (added 2026-10): entries carry a branch; heads and provenance
        // are per branch. Older stores migrate in place.
        let has_branch: bool = conn
            .prepare("PRAGMA table_info(entries)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .flatten()
            .any(|c| c == "branch");
        if !has_branch {
            conn.execute_batch("ALTER TABLE entries ADD COLUMN branch TEXT NOT NULL DEFAULT 'main';")?;
        }
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS entries_branch ON entries(branch, id);
             CREATE TABLE IF NOT EXISTS branches (
               name TEXT PRIMARY KEY,
               parent TEXT,
               from_entry INTEGER,
               created INTEGER NOT NULL,
               created_by TEXT NOT NULL,
               note TEXT NOT NULL DEFAULT ''
             );
             CREATE TABLE IF NOT EXISTS branch_heads (branch TEXT NOT NULL, file TEXT NOT NULL, entry INTEGER NOT NULL, PRIMARY KEY (branch, file));
             CREATE TABLE IF NOT EXISTS branch_provenance (branch TEXT NOT NULL, file TEXT NOT NULL, spans TEXT NOT NULL, PRIMARY KEY (branch, file));
             INSERT OR IGNORE INTO branches (name, parent, from_entry, created, created_by, note) VALUES ('main', NULL, NULL, 0, 'kloverleaf', '');
             INSERT OR IGNORE INTO branch_heads (branch, file, entry) SELECT 'main', file, entry FROM heads;
             INSERT OR IGNORE INTO branch_provenance (branch, file, spans) SELECT 'main', file, spans FROM provenance;",
        )?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        let store = Store { conn: Mutex::new(conn), branch: Mutex::new("main".into()) };
        if version < PROVENANCE_VERSION {
            let t0 = std::time::Instant::now();
            match store.rebuild_provenance() {
                Ok(n) if n > 0 => eprintln!("history {}: rebuilt {n} provenance map(s) in {:.1?}", path.display(), t0.elapsed()),
                Ok(_) => {}
                Err(e) => eprintln!("history {}: provenance rebuild failed: {e}", path.display()),
            }
            let _ = store.conn.lock().unwrap().pragma_update(None, "user_version", PROVENANCE_VERSION);
        }
        Ok(store)
    }

    // ---- branches ------------------------------------------------------------

    pub fn branch(&self) -> String {
        self.branch.lock().unwrap().clone()
    }

    /// Switches which branch new entries go to. The caller loads the texts.
    pub fn set_branch(&self, name: &str) {
        *self.branch.lock().unwrap() = name.to_string();
    }

    /// `[(branch, upto)]`: this branch (no bound), then its parent up to the
    /// fork entry, and so on — the entries that make up a branch's past.
    fn lineage_of(conn: &Connection, branch: &str) -> Vec<(String, Option<i64>)> {
        let mut out: Vec<(String, Option<i64>)> = vec![(branch.to_string(), None)];
        let mut cur = branch.to_string();
        for _ in 0..64 {
            let row: Option<(Option<String>, Option<i64>)> = conn
                .query_row("SELECT parent, from_entry FROM branches WHERE name=?1", params![cur], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
                .ok()
                .flatten();
            let Some((Some(parent), Some(from))) = row else { break };
            if out.iter().any(|(b, _)| *b == parent) {
                break;
            }
            out.push((parent.clone(), Some(from)));
            cur = parent;
        }
        out
    }

    /// The lineage clause for a branch name, `*` (everything), or the current branch.
    fn lineage_clause(&self, conn: &Connection, branch: Option<&str>) -> (String, Vec<Sql>) {
        match branch {
            Some("*") => ("1".to_string(), vec![]),
            Some(b) => lineage_sql(&Self::lineage_of(conn, b)),
            None => lineage_sql(&Self::lineage_of(conn, &self.branch())),
        }
    }

    /// The lineage clause of the branch entry `id` is on.
    fn lineage_at(&self, conn: &Connection, id: i64) -> (String, Vec<Sql>) {
        let branch: Option<String> = conn.query_row("SELECT branch FROM entries WHERE id=?1", params![id], |r| r.get(0)).optional().ok().flatten();
        self.lineage_clause(conn, Some(branch.as_deref().unwrap_or("main")))
    }

    /// `(file, entry, kind)` of each file's latest entry at or before `upto` on a lineage.
    fn heads_along(conn: &Connection, (clause, params): (String, Vec<Sql>), upto: i64) -> rusqlite::Result<Vec<(String, i64, String)>> {
        let sql = format!(
            "SELECT e.file, e.id, e.kind FROM entries e
             JOIN (SELECT file, MAX(id) AS id FROM entries e WHERE id<=?1 AND file!='' AND {clause} GROUP BY file) m ON e.id = m.id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let all = std::iter::once(Sql::from(upto)).chain(params);
        let rows = stmt.query_map(params_from_iter(all), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    pub fn branches(&self) -> Vec<Branch> {
        let conn = self.conn.lock().unwrap();
        query_all(
            &conn,
            "SELECT b.name, b.parent, b.from_entry, b.created, b.created_by, b.note,
                    (SELECT MAX(id) FROM entries e WHERE e.branch=b.name),
                    (SELECT MAX(ts) FROM entries e WHERE e.branch=b.name),
                    (SELECT COUNT(*) FROM entries e WHERE e.branch=b.name)
             FROM branches b ORDER BY b.created, b.name",
            [],
            |r| {
                Ok(Branch {
                    name: r.get(0)?,
                    parent: r.get(1)?,
                    from_entry: r.get(2)?,
                    created: r.get(3)?,
                    created_by: r.get(4)?,
                    note: r.get(5)?,
                    head: r.get(6)?,
                    head_ts: r.get(7)?,
                    entries: r.get(8)?,
                })
            },
        )
    }

    pub fn branch_exists(&self, name: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT 1 FROM branches WHERE name=?1", params![name], |_| Ok(())).optional().ok().flatten().is_some()
    }

    /// Forks `name` off `parent` at `from_entry`: its heads are each file's
    /// latest entry on the parent's lineage at that point, so the new branch
    /// starts exactly there and its first edits chain onto those entries.
    pub fn create_branch(&self, name: &str, parent: &str, from_entry: i64, user: &str, note: &str) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let heads = Self::heads_along(&conn, self.lineage_clause(&conn, Some(parent)), from_entry)?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO branches (name, parent, from_entry, created, created_by, note) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![name, parent, from_entry, now_ms(), user, note],
        )?;
        for (file, id, kind) in heads {
            if kind != "delete" {
                tx.execute("INSERT OR REPLACE INTO branch_heads (branch, file, entry) VALUES (?1, ?2, ?3)", params![name, file, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The files (and the entry each is at) that a branch currently has.
    pub fn branch_files(&self, name: &str) -> Vec<(String, i64)> {
        let conn = self.conn.lock().unwrap();
        query_all(&conn, "SELECT file, entry FROM branch_heads WHERE branch=?1", params![name], |r| Ok((r.get(0)?, r.get(1)?)))
    }

    /// The history as a tree: every branch point, every notable entry (AI,
    /// restore, push, upload, commit, delete, create) on every branch, and
    /// typing runs coalesced per branch/file/author/30s — newest `limit`.
    pub fn tree(&self, limit: usize) -> rusqlite::Result<Vec<TreeEvent>> {
        const COALESCE_MS: i64 = 30_000;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, file, ts, author, kind, meta, branch FROM entries ORDER BY id DESC LIMIT ?1")?;
        let raw = (limit * 40).clamp(1000, 20000) as i64;
        let rows = stmt.query_map(params![raw], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?))
        })?;
        let mut out: Vec<TreeEvent> = Vec::new();
        for (id, file, ts, author, kind, meta, branch) in rows.flatten() {
            if kind == "lock" {
                continue;
            }
            if kind == "delta"
                && let Some(last) = out.last_mut()
                && last.kind == "delta"
                && last.branch == branch
                && last.file == file
                && last.author == author
                && last.ts - ts <= COALESCE_MS
            {
                last.first = id;
                last.count += 1;
                last.label = format!("typed {} changes", last.count);
                continue;
            }
            if out.len() >= limit {
                break;
            }
            let meta: Value = serde_json::from_str(&meta).unwrap_or(Value::Null);
            let label = match kind.as_str() {
                "delta" => "typed 1 change".to_string(),
                "ai" => format!("AI: {}", meta["prompt"].as_str().unwrap_or("").chars().take(80).collect::<String>()),
                "commit" => format!("git {} {}", meta["sha"].as_str().unwrap_or("").chars().take(7).collect::<String>(), meta["message"].as_str().unwrap_or("")),
                "replace" => match meta["source"].as_str() {
                    Some("git") => "pushed over git".into(),
                    Some("reconnect") => "re-synced".into(),
                    Some(s) => format!("replaced ({s})"),
                    None => "replaced".into(),
                },
                "restore" => format!("restored #{}", meta["restores"].as_i64().unwrap_or(0)),
                "create" => "created".into(),
                "delete" => "deleted".into(),
                k => k.to_string(),
            };
            out.push(TreeEvent { id, first: id, count: 1, ts, branch, file, author, kind, label });
        }
        out.reverse();
        Ok(out)
    }

    /// A short, human story of this branch for the AI: where it forked from,
    /// and the last `n` notable things that happened on it.
    pub fn story(&self, n: usize) -> String {
        let branch = self.branch();
        let (fork, events) = {
            let conn = self.conn.lock().unwrap();
            let fork: Option<(String, i64, i64)> = conn
                .query_row(
                    "SELECT b.parent, b.from_entry, e.ts FROM branches b JOIN entries e ON e.id=b.from_entry WHERE b.name=?1",
                    params![branch],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .ok()
                .flatten();
            let events: Vec<(i64, String, String, String)> = query_all(
                &conn,
                "SELECT ts, author, kind, meta FROM entries e WHERE e.branch=?1 AND kind IN ('ai','restore','replace','create','delete') ORDER BY id DESC LIMIT ?2",
                params![branch, n as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            );
            (fork, events)
        };
        let when = |ts: i64| {
            let secs = ts / 1000;
            let days = secs / 86_400;
            let (h, m) = ((secs % 86_400) / 3600, (secs % 3600) / 60);
            // civil date from days since epoch (Howard Hinnant's algorithm)
            let z = days + 719_468;
            let era = z.div_euclid(146_097);
            let doe = z - era * 146_097;
            let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
            let y = yoe + era * 400;
            let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
            let mp = (5 * doy + 2) / 153;
            let d = doy - (153 * mp + 2) / 5 + 1;
            let mo = if mp < 10 { mp + 3 } else { mp - 9 };
            let y = if mo <= 2 { y + 1 } else { y };
            format!("{y}-{mo:02}-{d:02} {h:02}:{m:02} UTC")
        };
        let mut s = format!("Branch: {branch}");
        if let Some((parent, from, ts)) = fork {
            s.push_str(&format!(" (forked from {parent} at entry #{from}, {})", when(ts)));
        }
        s.push('\n');
        if events.is_empty() {
            s.push_str("Nothing notable on this branch yet besides typing.\n");
        }
        for (ts, author, kind, meta) in events.iter().rev() {
            let meta: Value = serde_json::from_str(meta).unwrap_or(Value::Null);
            let what = match kind.as_str() {
                "ai" => format!("AI applied a proposal for: {}", meta["prompt"].as_str().unwrap_or("").chars().take(160).collect::<String>().replace('\n', " ")),
                "restore" => format!("restored an earlier version (entry #{})", meta["restores"].as_i64().unwrap_or(0)),
                "replace" => match meta["source"].as_str() {
                    Some("git") => "text arrived over git push".into(),
                    Some("upload") => "a file was uploaded".into(),
                    Some("reconnect") => "offline edits re-synced".into(),
                    _ => "a file was replaced".into(),
                },
                "create" => "project created".into(),
                "delete" => "a file was deleted".into(),
                k => k.to_string(),
            };
            s.push_str(&format!("- {} · {author}: {what}\n", when(*ts)));
        }
        s
    }

    pub fn head(&self, file: &str) -> Option<i64> {
        let conn = self.conn.lock().unwrap();
        let branch = self.branch();
        conn.query_row("SELECT entry FROM branch_heads WHERE branch=?1 AND file=?2", params![branch, file], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    /// Records a change to `file` whose result is `after`. `changes` is the
    /// delta that produced it (None for replacements). Returns the entry id.
    pub fn record(
        &self,
        file: &str,
        author: &str,
        kind: &str,
        changes: Option<&[Edit]>,
        after: &str,
        meta: Value,
    ) -> rusqlite::Result<i64> {
        let branch = self.branch();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let parent: Option<i64> = tx
            .query_row("SELECT entry FROM branch_heads WHERE branch=?1 AND file=?2", params![branch, file], |r| r.get(0))
            .optional()?;
        let (pdepth, pbytes): (i64, i64) = match parent {
            Some(p) => tx.query_row(
                "SELECT depth, bytes FROM entries WHERE id=?1",
                params![p],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?,
            None => (0, 0),
        };
        let delta_bytes: i64 = changes
            .map(|cs| cs.iter().map(|c| (c.text.len() + 16) as i64).sum())
            .unwrap_or(after.len() as i64);
        let snapshot = changes.is_none()
            || parent.is_none()
            || pdepth + 1 >= SNAPSHOT_EVERY
            || pbytes + delta_bytes >= SNAPSHOT_BYTES;
        let (depth, bytes) = if snapshot { (0, 0) } else { (pdepth + 1, pbytes + delta_bytes) };
        tx.execute(
            "INSERT INTO entries (file, parent, ts, author, kind, changes, hash, size, depth, bytes, meta, branch)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                file,
                parent,
                now_ms(),
                author,
                kind,
                changes.map(|c| serde_json::to_string(c).unwrap()),
                text::fnv1a(after) as i64,
                after.chars().count() as i64,
                depth,
                bytes,
                meta.to_string(),
                branch,
            ],
        )?;
        let id = tx.last_insert_rowid();
        if snapshot {
            tx.execute("INSERT INTO snapshots (entry, text) VALUES (?1, ?2)", params![id, after])?;
        }
        tx.execute(
            "INSERT INTO branch_heads (branch, file, entry) VALUES (?1, ?2, ?3) ON CONFLICT(branch, file) DO UPDATE SET entry=excluded.entry",
            params![branch, file, id],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// A project-level note (git commit, lock events): file `""`, no text.
    pub fn note(&self, author: &str, kind: &str, meta: Value) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let branch = self.branch();
        conn.execute(
            "INSERT INTO entries (file, parent, ts, author, kind, changes, hash, size, depth, bytes, meta, branch)
             VALUES ('', NULL, ?1, ?2, ?3, NULL, 0, 0, 0, 0, ?4, ?5)",
            params![now_ms(), author, kind, meta.to_string(), branch],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn entry(&self, id: i64) -> Option<Entry> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, file, parent, ts, author, kind, hash, size, meta, branch FROM entries WHERE id=?1",
            params![id],
            row_entry,
        )
        .optional()
        .ok()
        .flatten()
    }

    /// The text of `file` as of entry `id`.
    pub fn text_at(&self, id: i64) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut chain: Vec<Vec<Edit>> = Vec::new();
        let mut cur = Some(id);
        while let Some(e) = cur {
            if let Some(text) = conn
                .query_row("SELECT text FROM snapshots WHERE entry=?1", params![e], |r| r.get::<_, String>(0))
                .optional()?
            {
                let mut text = text;
                for changes in chain.iter().rev() {
                    text = text::apply(&text, changes);
                }
                return Ok(Some(text));
            }
            let row: Option<(Option<i64>, Option<String>)> = conn
                .query_row(
                    "SELECT parent, changes FROM entries WHERE id=?1",
                    params![e],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((parent, changes)) = row else { return Ok(None) };
            chain.push(changes.map(|c| serde_json::from_str(&c).unwrap_or_default()).unwrap_or_default());
            cur = parent;
        }
        Ok(None)
    }

    /// The last entry of `file` at or before entry `id` — "what this file was
    /// when that happened", for any entry on the project's timeline.
    pub fn latest_for(&self, file: &str, id: i64) -> Option<i64> {
        let conn = self.conn.lock().unwrap();
        let (clause, params) = self.lineage_at(&conn, id);
        let sql = format!("SELECT e.id FROM entries e WHERE e.file=?1 AND e.id<=?2 AND {clause} ORDER BY e.id DESC LIMIT 1");
        let all = [Sql::from(file.to_string()), Sql::from(id)].into_iter().chain(params);
        conn.query_row(&sql, params_from_iter(all), |r| r.get(0)).optional().ok().flatten()
    }

    /// Every text file as of entry `id` (files deleted by then are absent).
    pub fn files_at(&self, id: i64) -> rusqlite::Result<BTreeMap<String, String>> {
        let heads = {
            let conn = self.conn.lock().unwrap();
            Self::heads_along(&conn, self.lineage_at(&conn, id), id)?
        };
        let mut out = BTreeMap::new();
        for (file, eid, kind) in heads {
            if kind == "delete" {
                continue;
            }
            if let Some(text) = self.text_at(eid)? {
                out.insert(file, text);
            }
        }
        Ok(out)
    }

    /// Newest first. Runs of keystrokes by one author within `COALESCE_MS` of
    /// each other collapse into one row (`first`, `count`), so the list reads
    /// like a timeline instead of a keylogger.
    pub fn list(&self, file: Option<&str>, before: Option<i64>, after: Option<i64>, limit: usize, branch: Option<&str>) -> rusqlite::Result<Vec<Entry>> {
        const COALESCE_MS: i64 = 30_000;
        let conn = self.conn.lock().unwrap();
        let raw_limit = (limit * 50).clamp(200, 5000) as i64;
        let (clause, lparams) = self.lineage_clause(&conn, branch);
        let sql = format!(
            "SELECT e.id, e.file, e.parent, e.ts, e.author, e.kind, e.hash, e.size, e.meta, e.branch FROM entries e
             WHERE (?1 IS NULL OR e.file=?1 OR e.file='') AND (?2 IS NULL OR e.id<?2) AND (?4 IS NULL OR e.id>?4) AND {clause}
             ORDER BY e.id DESC LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql)?;
        let all = [Sql::from(file.map(str::to_string)), Sql::from(before), Sql::from(raw_limit), Sql::from(after)].into_iter().chain(lparams);
        let rows = stmt.query_map(params_from_iter(all), row_entry)?;
        let mut out: Vec<Entry> = Vec::new();
        for e in rows.flatten() {
            if let Some(last) = out.last_mut()
                && last.kind == "delta"
                && e.kind == "delta"
                && last.file == e.file
                && last.author == e.author
                && last.ts - e.ts <= COALESCE_MS
            {
                last.first = e.id;
                last.count += 1;
                continue;
            }
            if out.len() >= limit {
                break;
            }
            out.push(e);
        }
        Ok(out)
    }

    /// The timeline at a glance, for files with thousands of entries: the
    /// span of time split into `buckets` equal slices, each with its counts
    /// by kind and its first/last entry id (to load that window with `list`).
    pub fn summary(&self, file: Option<&str>, buckets: usize, branch: Option<&str>) -> rusqlite::Result<Vec<Bucket>> {
        let conn = self.conn.lock().unwrap();
        let (clause, lparams) = self.lineage_clause(&conn, branch);
        let p1: Vec<Sql> = std::iter::once(Sql::from(file.map(str::to_string))).chain(lparams).collect();
        let (min, max): (Option<i64>, Option<i64>) = conn.query_row(
            &format!("SELECT MIN(e.ts), MAX(e.ts) FROM entries e WHERE (?1 IS NULL OR e.file=?1 OR e.file='') AND {clause}"),
            params_from_iter(p1.clone()),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let (Some(min), Some(max)) = (min, max) else { return Ok(vec![]) };
        let buckets = buckets.clamp(1, 1000);
        let width = ((max - min) / buckets as i64).max(1);
        let mut out: Vec<Bucket> = (0..buckets)
            .map(|i| Bucket { t0: min + width * i as i64, t1: min + width * (i as i64 + 1), count: 0, first: 0, last: 0, kinds: BTreeMap::new(), authors: BTreeSet::new() })
            .collect();
        out.last_mut().unwrap().t1 = max + 1;
        let mut stmt = conn.prepare(&format!(
            "SELECT e.id, e.ts, e.kind, e.author FROM entries e WHERE (?1 IS NULL OR e.file=?1 OR e.file='') AND {clause} ORDER BY e.id ASC"
        ))?;
        let rows = stmt.query_map(params_from_iter(p1), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
        for (id, ts, kind, author) in rows.flatten() {
            let i = (((ts - min) / width) as usize).min(buckets - 1);
            let b = &mut out[i];
            if b.count == 0 {
                b.first = id;
            }
            b.last = id;
            b.count += 1;
            *b.kinds.entry(kind).or_insert(0) += 1;
            b.authors.insert(author);
        }
        Ok(out)
    }

    // ---- provenance --------------------------------------------------------

    /// The one origin standing for everything `user` types.
    pub fn human_origin(&self, user: &str) -> rusqlite::Result<u32> {
        let conn = self.conn.lock().unwrap();
        if let Some(id) = conn
            .query_row("SELECT id FROM origins WHERE kind='human' AND user=?1", params![user], |r| r.get::<_, i64>(0))
            .optional()?
        {
            return Ok(id as u32);
        }
        conn.execute("INSERT INTO origins (kind, user, ts, meta) VALUES ('human', ?1, ?2, '{}')", params![user, now_ms()])?;
        Ok(conn.last_insert_rowid() as u32)
    }

    /// A fresh origin for one event: an AI apply (meta: prompt, model,
    /// session, base), a git push, a restore, an upload.
    pub fn new_origin(&self, kind: &str, user: &str, meta: &Value) -> rusqlite::Result<u32> {
        let conn = self.conn.lock().unwrap();
        conn.execute("INSERT INTO origins (kind, user, ts, meta) VALUES (?1, ?2, ?3, ?4)", params![kind, user, now_ms(), meta.to_string()])?;
        Ok(conn.last_insert_rowid() as u32)
    }

    /// Adds keys to an origin's meta (e.g. the history entry it produced).
    pub fn extend_origin(&self, id: u32, extra: &Value) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        let cur: String = conn.query_row("SELECT meta FROM origins WHERE id=?1", params![id as i64], |r| r.get(0))?;
        let mut v: Value = serde_json::from_str(&cur).unwrap_or_else(|_| json!({}));
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in e.iter() {
                o.insert(k.to_string(), val.clone());
            }
        }
        conn.execute("UPDATE origins SET meta=?2 WHERE id=?1", params![id as i64, v.to_string()])?;
        Ok(())
    }

    pub fn origins(&self, ids: impl IntoIterator<Item = u32>) -> Vec<Origin> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare("SELECT id, kind, user, ts, meta FROM origins WHERE id=?1") {
            Ok(s) => s,
            Err(_) => return vec![],
        };
        ids.into_iter()
            .filter(|&id| id != provenance::UNKNOWN)
            .filter_map(|id| {
                stmt.query_row(params![id as i64], |r| {
                    Ok(Origin {
                        id: r.get::<_, i64>(0)? as u32,
                        kind: r.get(1)?,
                        user: r.get(2)?,
                        ts: r.get(3)?,
                        meta: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_else(|_| json!({})),
                    })
                })
                .ok()
            })
            .collect()
    }

    pub fn provenance(&self, file: &str) -> Option<Map> {
        let conn = self.conn.lock().unwrap();
        let branch = self.branch();
        let spans: String = conn.query_row("SELECT spans FROM branch_provenance WHERE branch=?1 AND file=?2", params![branch, file], |r| r.get(0)).ok()?;
        Map::from_json(&serde_json::from_str(&spans).ok()?)
    }

    pub fn set_provenance(&self, file: &str, map: &Map) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        write_provenance(&conn, &self.branch(), file, map)
    }

    /// The origin a replacement entry's text should carry: the one recorded
    /// for it at the time, else a new one of the right kind (an AI apply
    /// keeps its prompt and model), else the author as a person.
    fn origin_for(&self, id: i64, author: &str, kind: &str, meta: &Value) -> rusqlite::Result<u32> {
        let found: Option<i64> = self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT id FROM origins WHERE json_extract(meta, '$.entry') = ?1 AND kind != 'human' ORDER BY id LIMIT 1", params![id], |r| r.get(0))
            .optional()?;
        if let Some(o) = found {
            return Ok(o as u32);
        }
        if kind == "create" {
            return Ok(provenance::UNKNOWN); // the starter document is nobody's
        }
        match provenance::origin_kind(kind, meta) {
            Some(k) => {
                let mut m = meta.clone();
                if let Some(o) = m.as_object_mut() {
                    o.insert("entry".into(), json!(id));
                }
                self.new_origin(k, author, &m)
            }
            None => self.human_origin(author),
        }
    }

    /// Recomputes every branch's provenance maps by replaying its lineage
    /// through the current attribution rules: deltas are the typist's, a
    /// replacement is diffed against the text before it (`Map::rebase`).
    /// Returns the number of maps written. Used by the version migration in
    /// `open` and by the `provenance/rebuild` API.
    pub fn rebuild_provenance(&self) -> rusqlite::Result<usize> {
        let branches: Vec<String> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT name FROM branches ORDER BY created, name")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut written = 0;
        for branch in branches {
            // (id, file, author, kind, changes, meta, hash) along the lineage, oldest first.
            let rows: Vec<(i64, String, String, String, Option<String>, String, i64)> = {
                let conn = self.conn.lock().unwrap();
                let (clause, params) = self.lineage_clause(&conn, Some(&branch));
                let sql = format!("SELECT e.id, e.file, e.author, e.kind, e.changes, e.meta, e.hash FROM entries e WHERE e.file != '' AND {clause} ORDER BY e.id");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params_from_iter(params), |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))
                })?;
                rows.collect::<Result<_, _>>()?
            };
            let mut texts: BTreeMap<String, String> = BTreeMap::new();
            let mut maps: BTreeMap<String, Map> = BTreeMap::new();
            for (id, file, author, kind, changes, meta, hash) in rows {
                if kind == "delete" {
                    texts.remove(&file);
                    maps.remove(&file);
                    continue;
                }
                let meta: Value = serde_json::from_str(&meta).unwrap_or_else(|_| json!({}));
                let old = texts.remove(&file).unwrap_or_default();
                let mut map = maps.remove(&file).unwrap_or_default().fitted(&old);
                let delta: Option<Vec<Edit>> = changes.and_then(|c| serde_json::from_str(&c).ok());
                let new = match &delta {
                    Some(d) => {
                        let t = text::apply(&old, d);
                        if text::fnv1a(&t) as i64 == hash { Some(t) } else { None }
                    }
                    None => None,
                };
                let new = match new {
                    Some(t) => {
                        map.apply(delta.as_ref().unwrap(), self.human_origin(&author)?);
                        t
                    }
                    None => {
                        // A replacement, or a delta chain that does not add up: take the recorded text.
                        let Some(t) = self.text_at(id)? else { continue };
                        let origin = if delta.is_some() { self.human_origin(&author)? } else { self.origin_for(id, &author, &kind, &meta)? };
                        map = map.rebase(&old, &t, origin);
                        t
                    }
                };
                maps.insert(file.clone(), map.fitted(&new));
                texts.insert(file, new);
            }
            let conn = self.conn.lock().unwrap();
            conn.execute("DELETE FROM branch_provenance WHERE branch=?1", params![branch])?;
            for (file, map) in &maps {
                write_provenance(&conn, &branch, file, map)?;
                written += 1;
            }
        }
        Ok(written)
    }

    pub fn delete_provenance(&self, file: &str) {
        let conn = self.conn.lock().unwrap();
        let branch = self.branch();
        let _ = conn.execute("DELETE FROM branch_provenance WHERE branch=?1 AND file=?2", params![branch, file]);
    }
}

fn write_provenance(conn: &Connection, branch: &str, file: &str, map: &Map) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO branch_provenance (branch, file, spans) VALUES (?1, ?2, ?3) ON CONFLICT(branch, file) DO UPDATE SET spans=excluded.spans",
        params![branch, file, map.to_json().to_string()],
    )?;
    Ok(())
}

fn row_entry(r: &Row) -> rusqlite::Result<Entry> {
    let id: i64 = r.get(0)?;
    Ok(Entry {
        id,
        file: r.get(1)?,
        parent: r.get(2)?,
        ts: r.get(3)?,
        author: r.get(4)?,
        kind: r.get(5)?,
        hash: r.get::<_, i64>(6)? as u32,
        size: r.get(7)?,
        meta: serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or(Value::Null),
        first: id,
        count: 1,
        branch: r.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open(&tmp_dir("hist").join("h.sqlite")).unwrap()
    }

    #[test]
    fn replays_deltas_from_snapshots() {
        let s = store();
        let mut text = String::new();
        let first = s.record("a.tex", "me", "replace", None, "", serde_json::json!({})).unwrap();
        let mut ids = vec![first];
        for i in 0..250 {
            let ch = format!("{} ", i % 10);
            let changes = vec![Edit { offset: text.chars().count(), length: 0, text: ch.clone() }];
            text.push_str(&ch);
            ids.push(s.record("a.tex", "me", "delta", Some(&changes), &text, serde_json::json!({})).unwrap());
        }
        assert_eq!(s.text_at(*ids.last().unwrap()).unwrap().unwrap(), text);
        assert_eq!(s.text_at(ids[10]).unwrap().unwrap(), "0 1 2 3 4 5 6 7 8 9 ");
        assert_eq!(s.text_at(first).unwrap().unwrap(), "");
        assert_eq!(s.head("a.tex"), ids.last().copied());
        let list = s.list(Some("a.tex"), None, None, 10, None).unwrap();
        assert_eq!(list[0].kind, "delta");
        assert!(list[0].count > 1, "keystrokes coalesce");
        assert_eq!(list.last().unwrap().kind, "replace");
        let note = s.note("git", "commit", serde_json::json!({})).unwrap();
        assert_eq!(s.latest_for("a.tex", note), ids.last().copied());
        assert_eq!(s.latest_for("a.tex", ids[10]), Some(ids[10]));
        assert_eq!(s.files_at(ids[10]).unwrap()["a.tex"], "0 1 2 3 4 5 6 7 8 9 ");
        s.record("b.tex", "me", "replace", None, "bee", serde_json::json!({})).unwrap();
        let gone = s.record("b.tex", "me", "delete", None, "", serde_json::json!({})).unwrap();
        assert!(!s.files_at(gone).unwrap().contains_key("b.tex"));
    }

    #[test]
    fn rebuild_replays_attribution_from_the_log() {
        let s = store();
        let kris = s.human_origin("kris").unwrap();
        let t1 = "Hello world.\n";
        s.record("a.tex", "kris", "replace", None, t1, json!({"source": "upload"})).unwrap();
        let t2 = "Hello big world.\n";
        let d = vec![Edit { offset: 6, length: 0, text: "big ".into() }];
        s.record("a.tex", "kris", "delta", Some(&d), t2, json!({})).unwrap();
        let t3 = "Hello big world.\nAn AI paragraph follows here.\n";
        let ai = s.record("a.tex", "kris", "ai", None, t3, json!({"prompt": "add a paragraph", "model": "m"})).unwrap();
        // No map was ever written (as for a store from before tracking); rebuild derives one.
        assert!(s.provenance("a.tex").is_none());
        assert_eq!(s.rebuild_provenance().unwrap(), 1);
        let m = s.provenance("a.tex").unwrap();
        assert_eq!(m.len(), t3.encode_utf16().count());
        let origins = s.origins(m.origins());
        let upload = origins.iter().find(|o| o.kind == "upload").expect("an upload origin was created");
        let aio = origins.iter().find(|o| o.kind == "ai").expect("an AI origin was created");
        assert_eq!(aio.meta["entry"], json!(ai));
        assert_eq!(aio.meta["prompt"], json!("add a paragraph"));
        assert_eq!(
            m.spans,
            vec![
                crate::provenance::Span { len: 6, origin: upload.id },
                crate::provenance::Span { len: 4, origin: kris },
                crate::provenance::Span { len: 7, origin: upload.id },
                crate::provenance::Span { len: 30, origin: aio.id },
            ]
        );
        // Idempotent: a second rebuild reuses the origins it made.
        assert_eq!(s.rebuild_provenance().unwrap(), 1);
        assert_eq!(s.provenance("a.tex").unwrap(), m);
    }

    #[test]
    fn notes_and_meta() {
        let s = store();
        let id = s.note("git", "commit", serde_json::json!({"sha": "abc"})).unwrap();
        let e = s.entry(id).unwrap();
        assert_eq!(e.meta["sha"], "abc");
        assert!(s.text_at(id).unwrap().is_none());
    }
}
