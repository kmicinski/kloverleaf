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

use crate::text::{self, Edit};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const SNAPSHOT_EVERY: i64 = 100;
const SNAPSHOT_BYTES: i64 = 32 * 1024;

pub struct Store(Mutex<Connection>);

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
             CREATE TABLE IF NOT EXISTS heads (file TEXT PRIMARY KEY, entry INTEGER NOT NULL);",
        )?;
        Ok(Store(Mutex::new(conn)))
    }

    pub fn head(&self, file: &str) -> Option<i64> {
        let conn = self.0.lock().unwrap();
        conn.query_row("SELECT entry FROM heads WHERE file=?1", params![file], |r| r.get(0))
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
        let mut conn = self.0.lock().unwrap();
        let tx = conn.transaction()?;
        let parent: Option<i64> = tx
            .query_row("SELECT entry FROM heads WHERE file=?1", params![file], |r| r.get(0))
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
            "INSERT INTO entries (file, parent, ts, author, kind, changes, hash, size, depth, bytes, meta)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
            ],
        )?;
        let id = tx.last_insert_rowid();
        if snapshot {
            tx.execute("INSERT INTO snapshots (entry, text) VALUES (?1, ?2)", params![id, after])?;
        }
        tx.execute(
            "INSERT INTO heads (file, entry) VALUES (?1, ?2) ON CONFLICT(file) DO UPDATE SET entry=excluded.entry",
            params![file, id],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// A project-level note (git commit, lock events): file `""`, no text.
    pub fn note(&self, author: &str, kind: &str, meta: Value) -> rusqlite::Result<i64> {
        let conn = self.0.lock().unwrap();
        conn.execute(
            "INSERT INTO entries (file, parent, ts, author, kind, changes, hash, size, depth, bytes, meta)
             VALUES ('', NULL, ?1, ?2, ?3, NULL, 0, 0, 0, 0, ?4)",
            params![now_ms(), author, kind, meta.to_string()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn entry(&self, id: i64) -> Option<Entry> {
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT id, file, parent, ts, author, kind, hash, size, meta FROM entries WHERE id=?1",
            params![id],
            row_entry,
        )
        .optional()
        .ok()
        .flatten()
    }

    /// The text of `file` as of entry `id`.
    pub fn text_at(&self, id: i64) -> rusqlite::Result<Option<String>> {
        let conn = self.0.lock().unwrap();
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
        let conn = self.0.lock().unwrap();
        conn.query_row(
            "SELECT id FROM entries WHERE file=?1 AND id<=?2 ORDER BY id DESC LIMIT 1",
            params![file, id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten()
    }

    /// Every text file as of entry `id` (files deleted by then are absent).
    pub fn files_at(&self, id: i64) -> rusqlite::Result<std::collections::BTreeMap<String, String>> {
        let heads: Vec<(String, i64, String)> = {
            let conn = self.0.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT e.file, e.id, e.kind FROM entries e
                 JOIN (SELECT file, MAX(id) AS id FROM entries WHERE id<=?1 AND file!='' GROUP BY file) m
                 ON e.id = m.id",
            )?;
            let rows = stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut out = std::collections::BTreeMap::new();
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
    pub fn list(&self, file: Option<&str>, before: Option<i64>, limit: usize) -> rusqlite::Result<Vec<Entry>> {
        const COALESCE_MS: i64 = 30_000;
        let conn = self.0.lock().unwrap();
        let raw_limit = (limit * 50).clamp(200, 5000) as i64;
        let mut stmt = conn.prepare(
            "SELECT id, file, parent, ts, author, kind, hash, size, meta FROM entries
             WHERE (?1 IS NULL OR file=?1 OR file='') AND (?2 IS NULL OR id<?2)
             ORDER BY id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![file, before, raw_limit], row_entry)?;
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
}

fn row_entry(r: &rusqlite::Row) -> rusqlite::Result<Entry> {
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
        let list = s.list(Some("a.tex"), None, 10).unwrap();
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
    fn notes_and_meta() {
        let s = store();
        let id = s.note("git", "commit", serde_json::json!({"sha": "abc"})).unwrap();
        let e = s.entry(id).unwrap();
        assert_eq!(e.meta["sha"], "abc");
        assert!(s.text_at(id).unwrap().is_none());
    }
}
