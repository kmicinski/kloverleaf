//! Guests and members. An admin (the proxy-authenticated owner, or whoever
//! holds the shared password) sends an invite link for one project; the guest opens it,
//! picks a name and a password, and from then on everything they do is
//! scoped to that project: the editor, the API, git over HTTP, the AI. They
//! never see the project list beyond their own, cannot create projects, and
//! every other request is refused — so the app can sit on the open internet
//! with only its own login in front of guests.
//!
//! The same `member_projects` table also scopes *members*: proxy-authenticated
//! people who are not in `KLOVERLEAF_ADMINS` (`auth.rs`). They have no row in
//! `members` unless they set a git password (`set_password`), which lets them
//! clone over HTTP Basic; `add_member` is how a project is shared with them.
//!
//! Stored in `projects/.members.sqlite` (beside the projects, so it lives in
//! the data volume). Passwords are Argon2id hashes. Failed logins back off per
//! name (5 misses → 30 s lockout) on top of the login form's own second.

use crate::history::{now_ms, query_all};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Members {
    conn: Mutex<Connection>,
    misses: Mutex<HashMap<String, (u32, Instant)>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Invite {
    pub token: String,
    pub project: String,
    pub created_by: String,
    pub created: i64,
    pub used_by: Option<String>,
    pub used_at: Option<i64>,
}

pub fn valid_name(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_alphanumeric() || "._-@".contains(c))
}

impl Members {
    pub fn open(root: &Path) -> rusqlite::Result<Members> {
        let dir = root.join("projects");
        let _ = std::fs::create_dir_all(&dir);
        let conn = Connection::open(dir.join(".members.sqlite"))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS members (user TEXT PRIMARY KEY, hash TEXT NOT NULL, created INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS member_projects (user TEXT NOT NULL, project TEXT NOT NULL, PRIMARY KEY (user, project));
             CREATE TABLE IF NOT EXISTS invites (token TEXT PRIMARY KEY, project TEXT NOT NULL, created_by TEXT NOT NULL, created INTEGER NOT NULL, used_by TEXT, used_at INTEGER);",
        )?;
        Ok(Members { conn: Mutex::new(conn), misses: Mutex::new(HashMap::new()) })
    }

    /// A new single-use invite for `project`; the token is the link.
    pub fn create_invite(&self, project: &str, by: &str) -> rusqlite::Result<String> {
        let token = crate::text::random_hex(24);
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO invites (token, project, created_by, created) VALUES (?1, ?2, ?3, ?4)",
            params![token, project, by, now_ms()],
        )?;
        Ok(token)
    }

    pub fn invite(&self, token: &str) -> Option<Invite> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(&format!("{INVITE_COLUMNS} WHERE token=?1"), params![token], row_invite).optional().ok().flatten()
    }

    pub fn invites_for(&self, project: &str) -> Vec<Invite> {
        let conn = self.conn.lock().unwrap();
        query_all(&conn, &format!("{INVITE_COLUMNS} WHERE project=?1 ORDER BY created DESC"), params![project], row_invite)
    }

    pub fn revoke(&self, token: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM invites WHERE token=?1 AND used_by IS NULL", params![token]).map(|n| n > 0).unwrap_or(false)
    }

    /// Claims an invite: a new guest with this name and password, or an
    /// existing guest (same password) gaining the project. Returns the
    /// guest's projects.
    pub fn claim(&self, token: &str, user: &str, password: &str) -> Result<Vec<String>, String> {
        let user = user.trim();
        if !valid_name(user) {
            return Err("name: letters, digits, . _ - @ (at most 64)".into());
        }
        if password.len() < 8 {
            return Err("password: at least 8 characters".into());
        }
        let inv = self.invite(token).ok_or("this invite link is not valid")?;
        if inv.used_by.is_some() {
            return Err("this invite link was already used".into());
        }
        let mut conn = self.conn.lock().unwrap();
        let existing: Option<String> = conn.query_row("SELECT hash FROM members WHERE user=?1", params![user], |r| r.get(0)).optional().map_err(|e| e.to_string())?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        match existing {
            Some(hash) => {
                if !verify_hash(&hash, password) {
                    return Err("that name is taken (or the password does not match it)".into());
                }
            }
            None => {
                let salt = SaltString::encode_b64(&rand::random::<[u8; 16]>()).map_err(|e| e.to_string())?;
                let hash = Argon2::default().hash_password(password.as_bytes(), &salt).map_err(|e| e.to_string())?.to_string();
                tx.execute("INSERT INTO members (user, hash, created) VALUES (?1, ?2, ?3)", params![user, hash, now_ms()]).map_err(|e| e.to_string())?;
            }
        }
        tx.execute("INSERT OR IGNORE INTO member_projects (user, project) VALUES (?1, ?2)", params![user, inv.project]).map_err(|e| e.to_string())?;
        tx.execute("UPDATE invites SET used_by=?2, used_at=?3 WHERE token=?1", params![token, user, now_ms()]).map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        drop(conn);
        Ok(self.projects_of(user))
    }

    pub fn exists(&self, user: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT 1 FROM members WHERE user=?1", params![user], |_| Ok(())).optional().ok().flatten().is_some()
    }

    /// The guest's projects if the password is right; `None` otherwise (or while locked out).
    pub fn verify(&self, user: &str, password: &str) -> Option<Vec<String>> {
        {
            let mut m = self.misses.lock().unwrap();
            if let Some((n, since)) = m.get(user)
                && *n >= 5
            {
                if since.elapsed() < Duration::from_secs(30) {
                    return None;
                }
                m.remove(user);
            }
        }
        let hash: Option<String> = {
            let conn = self.conn.lock().unwrap();
            conn.query_row("SELECT hash FROM members WHERE user=?1", params![user], |r| r.get(0)).optional().ok().flatten()
        };
        let ok = hash.as_deref().is_some_and(|h| verify_hash(h, password));
        let mut m = self.misses.lock().unwrap();
        if ok {
            m.remove(user);
            drop(m);
            Some(self.projects_of(user))
        } else {
            let e = m.entry(user.to_string()).or_insert((0, Instant::now()));
            e.0 += 1;
            e.1 = Instant::now();
            None
        }
    }

    pub fn projects_of(&self, user: &str) -> Vec<String> {
        let conn = self.conn.lock().unwrap();
        query_all(&conn, "SELECT project FROM member_projects WHERE user=?1 ORDER BY project", params![user], |r| r.get(0))
    }

    pub fn members_of(&self, project: &str) -> Vec<String> {
        let conn = self.conn.lock().unwrap();
        query_all(&conn, "SELECT user FROM member_projects WHERE project=?1 ORDER BY user", params![project], |r| r.get(0))
    }

    /// Gives `user` access to `project` (a share to a household account, or a
    /// member's own new project). True if it was new.
    pub fn add_member(&self, project: &str, user: &str) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().unwrap();
        conn.execute("INSERT OR IGNORE INTO member_projects (user, project) VALUES (?1, ?2)", params![user, project]).map(|n| n > 0)
    }

    /// Sets (or replaces) the password `user` logs in with over HTTP Basic —
    /// how a member without the shared password uses git. Their projects are
    /// unchanged.
    pub fn set_password(&self, user: &str, password: &str) -> Result<(), String> {
        let user = user.trim();
        if !valid_name(user) {
            return Err("name: letters, digits, . _ - @ (at most 64)".into());
        }
        if password.len() < 8 {
            return Err("password: at least 8 characters".into());
        }
        let salt = SaltString::encode_b64(&rand::random::<[u8; 16]>()).map_err(|e| e.to_string())?;
        let hash = Argon2::default().hash_password(password.as_bytes(), &salt).map_err(|e| e.to_string())?.to_string();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO members (user, hash, created) VALUES (?1, ?2, ?3) ON CONFLICT(user) DO UPDATE SET hash=excluded.hash",
            params![user, hash, now_ms()],
        )
        .map_err(|e| e.to_string())?;
        self.misses.lock().unwrap().remove(user);
        Ok(())
    }

    pub fn remove_member(&self, project: &str, user: &str) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM member_projects WHERE project=?1 AND user=?2", params![project, user]).map(|n| n > 0).unwrap_or(false)
    }
}

const INVITE_COLUMNS: &str = "SELECT token, project, created_by, created, used_by, used_at FROM invites";

fn row_invite(r: &rusqlite::Row) -> rusqlite::Result<Invite> {
    Ok(Invite { token: r.get(0)?, project: r.get(1)?, created_by: r.get(2)?, created: r.get(3)?, used_by: r.get(4)?, used_at: r.get(5)? })
}

fn verify_hash(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash).map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invites_make_scoped_guests() {
        let root = crate::history::tmp_dir("members");
        let m = Members::open(&root).unwrap();
        let t = m.create_invite("paper", "kris").unwrap();
        assert!(m.claim(&t, "jade", "short").is_err(), "weak password refused");
        assert_eq!(m.claim(&t, "jade", "correct horse").unwrap(), vec!["paper"]);
        assert!(m.claim(&t, "someone", "correct horse").is_err(), "single use");
        assert_eq!(m.verify("jade", "correct horse"), Some(vec!["paper".to_string()]));
        assert_eq!(m.verify("jade", "wrong"), None);
        // A second invite to another project, same guest, same password.
        let t2 = m.create_invite("slides", "kris").unwrap();
        assert!(m.claim(&t2, "jade", "different").is_err(), "cannot take a name with another password");
        assert_eq!(m.claim(&t2, "jade", "correct horse").unwrap(), vec!["paper", "slides"]);
        assert_eq!(m.members_of("paper"), vec!["jade"]);
        assert!(m.invite(&t).unwrap().used_by.is_some());
        // Lockout after five misses.
        for _ in 0..5 {
            assert_eq!(m.verify("jade", "nope"), None);
        }
        assert_eq!(m.verify("jade", "correct horse"), None, "locked out");
        // A shared project and a member's git password.
        assert!(m.add_member("paper", "tom").unwrap());
        assert!(!m.add_member("paper", "tom").unwrap(), "idempotent");
        assert_eq!(m.projects_of("tom"), vec!["paper"]);
        assert!(!m.exists("tom"), "no password yet");
        assert!(m.set_password("tom", "short").is_err());
        m.set_password("tom", "a git password").unwrap();
        assert_eq!(m.verify("tom", "a git password"), Some(vec!["paper".to_string()]));
        m.set_password("tom", "another one!").unwrap();
        assert_eq!(m.verify("tom", "a git password"), None);
        assert_eq!(m.verify("tom", "another one!"), Some(vec!["paper".to_string()]));
        std::fs::remove_dir_all(root).unwrap();
    }
}
