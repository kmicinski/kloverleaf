//! A project's chat: messages between the people working on it, any of which
//! may be pinned to a point in the paper — a comment. Kept in
//! `.kloverleaf/chat.json` (never in the paper's own files, so comments never
//! reach the PDF's source or git), broadcast live over the editor socket.
//!
//! An [`Anchor`] names a file and line plus the line's text as it was
//! (`quote`): lines move as people type, so the browser re-finds the quote
//! near the old line before drawing the pin, in the editor's margin and — via
//! SyncTeX — on the PDF page. A comment made by clicking the PDF also keeps
//! the click (`pdf`), which places the pin exactly while the line is where it was.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const KEEP: usize = 5000;
const MAX_TEXT: usize = 8000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PdfSpot {
    pub page: u32,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Anchor {
    pub file: String,
    pub line: u32,
    #[serde(default)]
    pub quote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<PdfSpot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Msg {
    pub id: u64,
    pub user: String,
    pub ts: i64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    /// A comment someone marked done: its pin goes away, the message stays.
    #[serde(default)]
    pub resolved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    /// Withdrawn by its author: kept as a tombstone so open pages drop it.
    #[serde(default)]
    pub deleted: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    next: u64,
    messages: Vec<Msg>,
}

pub struct Chat {
    path: PathBuf,
    file: File,
}

impl Chat {
    pub fn open(private_dir: &Path) -> Chat {
        let path = private_dir.join("chat.json");
        let file = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Chat { path, file }
    }

    fn save(&self) {
        let tmp = self.path.with_extension("json.tmp");
        if serde_json::to_string(&self.file).ok().and_then(|s| std::fs::write(&tmp, s).ok()).is_some() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// The last `n` messages (deleted ones left out), oldest first.
    pub fn recent(&self, n: usize) -> Vec<Msg> {
        let live: Vec<&Msg> = self.file.messages.iter().filter(|m| !m.deleted).collect();
        live[live.len().saturating_sub(n)..].iter().map(|m| (*m).clone()).collect()
    }

    pub fn post(&mut self, user: &str, text: &str, anchor: Option<Anchor>) -> anyhow::Result<Msg> {
        let text = text.trim();
        anyhow::ensure!(!text.is_empty(), "empty message");
        anyhow::ensure!(text.chars().count() <= MAX_TEXT, "message too long");
        if let Some(a) = &anchor {
            anyhow::ensure!(crate::project::valid_path(&a.file), "invalid file in anchor");
        }
        self.file.next += 1;
        let msg = Msg {
            id: self.file.next,
            user: user.into(),
            ts: crate::history::now_ms(),
            text: text.into(),
            anchor: anchor.map(|mut a| {
                a.quote = a.quote.chars().take(200).collect();
                a
            }),
            resolved: false,
            resolved_by: None,
            deleted: false,
        };
        self.file.messages.push(msg.clone());
        if self.file.messages.len() > KEEP {
            let n = self.file.messages.len() - KEEP;
            self.file.messages.drain(..n);
        }
        self.save();
        Ok(msg)
    }

    /// Marks a comment done (or not). Anyone on the project may.
    pub fn resolve(&mut self, id: u64, user: &str, resolved: bool) -> anyhow::Result<Msg> {
        let m = self.file.messages.iter_mut().find(|m| m.id == id && !m.deleted).ok_or_else(|| anyhow::anyhow!("no such message"))?;
        m.resolved = resolved;
        m.resolved_by = resolved.then(|| user.to_string());
        let out = m.clone();
        self.save();
        Ok(out)
    }

    /// Withdraws a message; only its author may.
    pub fn delete(&mut self, id: u64, user: &str) -> anyhow::Result<Msg> {
        let m = self.file.messages.iter_mut().find(|m| m.id == id && !m.deleted).ok_or_else(|| anyhow::anyhow!("no such message"))?;
        anyhow::ensure!(m.user == user, "only its author can delete a message");
        m.deleted = true;
        m.text.clear();
        let out = m.clone();
        self.save();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_persist_resolve_and_delete() {
        let dir = crate::history::tmp_dir("chat");
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = Chat::open(&dir);
        let a = Anchor { file: "intro.tex".into(), line: 12, quote: "We present".into(), pdf: Some(PdfSpot { page: 1, x: 72.0, y: 300.0 }) };
        let m1 = c.post("tom", "  Should this be \\Clight?  ", Some(a.clone())).unwrap();
        assert_eq!(m1.text, "Should this be \\Clight?");
        let m2 = c.post("kris", "yes", None).unwrap();
        assert!(c.post("kris", "   ", None).is_err());
        assert!(c.post("kris", "x", Some(Anchor { file: "../etc/passwd".into(), ..a.clone() })).is_err());
        assert!(c.delete(m1.id, "kris").is_err(), "only the author deletes");
        assert!(c.resolve(m1.id, "kris", true).unwrap().resolved);
        c.delete(m2.id, "kris").unwrap();
        let again = Chat::open(&dir);
        let r = again.recent(10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].anchor.as_ref().unwrap(), &a);
        assert_eq!(r[0].resolved_by.as_deref(), Some("kris"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
