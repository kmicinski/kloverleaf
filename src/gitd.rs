//! Git: every project is a working tree with a repository, committed on a
//! timer, and served over smart HTTP at `/git/<name>.git` so `git clone`,
//! `pull` and `push` work with the editor's password.
//!
//! Pushes land in the working tree (`receive.denyCurrentBranch=updateInstead`):
//! before a push is accepted the project is flushed and committed, so the tree
//! is clean; after it, the live documents are reloaded from disk. The smart
//! protocol is spoken by spawning `git upload-pack` / `git receive-pack`
//! with `--stateless-rpc`, which is exactly what `git-http-backend` does.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub const AUTHOR: &str = "kloverleaf <kloverleaf@localhost>";

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .await?;
    anyhow::ensure!(
        out.status.success(),
        "git {}: {}",
        args.first().unwrap_or(&""),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The checked-out branch name.
pub async fn current_branch(dir: &Path) -> anyhow::Result<String> {
    Ok(git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await?.trim().to_string())
}

/// Switches to `name`, creating it from HEAD when `create`. The caller has
/// committed first, so the working tree is clean and the switch cannot fail
/// on local changes; `.kloverleaf/` is ignored and untouched.
pub async fn checkout(dir: &Path, name: &str, create: bool) -> anyhow::Result<()> {
    if create {
        git(dir, &["checkout", "-q", "-b", name]).await?;
    } else {
        git(dir, &["checkout", "-q", name]).await?;
    }
    Ok(())
}

/// Makes `dir` a repository if it is not one; idempotent.
pub async fn init(dir: &Path) -> anyhow::Result<()> {
    if !dir.join(".git").exists() {
        git(dir, &["init", "-q", "-b", "main"]).await?;
    }
    git(dir, &["config", "receive.denyCurrentBranch", "updateInstead"]).await?;
    git(dir, &["config", "user.name", "kloverleaf"]).await?;
    git(dir, &["config", "user.email", "kloverleaf@localhost"]).await?;
    ensure_ignore(dir)?;
    Ok(())
}

/// `.kloverleaf/` must stay out of git (the history database lives there);
/// adds the line if the project's `.gitignore` lacks it. Idempotent.
pub fn ensure_ignore(dir: &Path) -> std::io::Result<()> {
    let ignore = dir.join(".gitignore");
    let want = ".kloverleaf/\n";
    match std::fs::read_to_string(&ignore) {
        Ok(s) if s.lines().any(|l| l.trim() == ".kloverleaf/" || l.trim() == ".kloverleaf") => Ok(()),
        Ok(s) => std::fs::write(&ignore, format!("{}{}{want}", s, if s.ends_with('\n') || s.is_empty() { "" } else { "\n" })),
        Err(_) => std::fs::write(&ignore, want),
    }
}

pub async fn is_dirty(dir: &Path) -> bool {
    git(dir, &["status", "--porcelain", "--untracked-files=all"])
        .await
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Stages everything and commits when something changed; returns the new sha.
pub async fn commit(dir: &Path, message: &str, author: Option<&str>) -> anyhow::Result<Option<String>> {
    if !is_dirty(dir).await {
        return Ok(None);
    }
    git(dir, &["add", "-A"]).await?;
    let author = author
        .filter(|a| !a.is_empty())
        .map(|a| format!("{a} <{a}@kloverleaf>"))
        .unwrap_or_else(|| AUTHOR.to_string());
    git(dir, &["commit", "-q", "-m", message, "--author", &author]).await?;
    Ok(Some(head(dir).await?))
}

pub async fn head(dir: &Path) -> anyhow::Result<String> {
    Ok(git(dir, &["rev-parse", "HEAD"]).await?.trim().to_string())
}

#[derive(serde::Serialize)]
pub struct Commit {
    pub sha: String,
    pub ts: i64,
    pub author: String,
    pub message: String,
    /// The author date as git prints it, with its time zone.
    pub date: String,
}

pub async fn log(dir: &Path, n: usize) -> anyhow::Result<Vec<Commit>> {
    log_of(dir, &["log", &format!("-{n}")]).await
}

/// `git log` with its own arguments (a ref, `--grep`, `-- path`), parsed.
pub async fn log_of(dir: &Path, args: &[&str]) -> anyhow::Result<Vec<Commit>> {
    let mut all = args.to_vec();
    all.insert(1, "--format=%H%x1f%at%x1f%an%x1f%s%x1f%ai");
    let out = git(dir, &all).await?;
    Ok(out
        .lines()
        .filter_map(|l| {
            let mut it = l.split('\x1f');
            Some(Commit {
                sha: it.next()?.to_string(),
                ts: it.next()?.parse().ok()?,
                author: it.next()?.to_string(),
                message: it.next().unwrap_or("").to_string(),
                date: it.next().unwrap_or("").to_string(),
            })
        })
        .collect())
}

/// A revision the agent named: a hex id or a ref-ish name, never an option.
pub fn plain_rev(rev: &str) -> bool {
    !rev.is_empty() && !rev.starts_with('-') && rev.chars().all(|c| c.is_ascii_alphanumeric() || "._/~^-".contains(c))
}

/// Every text file of commit `rev` in the project's own repository (path →
/// text), with the commit's full id. `.gitignore` is the app's, not content.
pub async fn texts_at(dir: &Path, rev: &str) -> anyhow::Result<(String, BTreeMap<String, String>)> {
    anyhow::ensure!(plain_rev(rev), "not a commit name: {rev}");
    let sha = git(dir, &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")]).await?.trim().to_string();
    let mut files = BTreeMap::new();
    for path in git(dir, &["ls-tree", "-r", "-z", "--name-only", &sha]).await?.split('\0') {
        if path.is_empty() || path == ".gitignore" || !crate::project::valid_path(path) || !crate::project::is_text(path) {
            continue;
        }
        files.insert(path.to_string(), git(dir, &["show", &format!("{sha}:{path}")]).await?);
    }
    Ok((sha, files))
}

// ---- smart HTTP ------------------------------------------------------------------

/// A pkt-line: 4 hex digits of length (including the 4) then the payload.
pub fn pkt_line(s: &str) -> Vec<u8> {
    let mut v = format!("{:04x}", s.len() + 4).into_bytes();
    v.extend_from_slice(s.as_bytes());
    v
}

/// `GET info/refs?service=…`: the advertisement, prefixed as the smart protocol requires.
pub async fn advertise(dir: &Path, service: &str) -> anyhow::Result<Vec<u8>> {
    let cmd = service.strip_prefix("git-").unwrap_or(service);
    let out = Command::new("git")
        .arg(cmd)
        .arg("--stateless-rpc")
        .arg("--advertise-refs")
        .arg(dir)
        .stdin(Stdio::null())
        .output()
        .await?;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
    let mut body = pkt_line(&format!("# service={service}\n"));
    body.extend_from_slice(b"0000");
    body.extend_from_slice(&out.stdout);
    Ok(body)
}

/// `POST git-upload-pack` / `git-receive-pack`: the request body goes to the
/// command's stdin, its stdout is the response.
pub async fn rpc(dir: &Path, service: &str, body: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let cmd = service.strip_prefix("git-").unwrap_or(service);
    let mut child = Command::new("git")
        .arg(cmd)
        .arg("--stateless-rpc")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
        let _ = stdin.shutdown().await;
    });
    let out = child.wait_with_output().await?;
    let _ = writer.await;
    anyhow::ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkt_lines() {
        assert_eq!(pkt_line("# service=git-upload-pack\n"), b"001e# service=git-upload-pack\n".to_vec());
    }

    #[tokio::test]
    async fn init_commit_log() {
        let dir = std::env::temp_dir().join(format!("kl-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        init(&dir).await.unwrap();
        init(&dir).await.unwrap(); // idempotent
        assert_eq!(std::fs::read_to_string(dir.join(".gitignore")).unwrap(), ".kloverleaf/\n");
        std::fs::write(dir.join("main.tex"), "x").unwrap();
        let sha = commit(&dir, "first", Some("kris")).await.unwrap().unwrap();
        assert_eq!(sha.len(), 40);
        assert!(commit(&dir, "nothing", None).await.unwrap().is_none());
        let log = log(&dir, 5).await.unwrap();
        assert_eq!(log[0].message, "first");
        assert_eq!(log[0].author, "kris");
        let adv = advertise(&dir, "git-upload-pack").await.unwrap();
        assert!(adv.starts_with(b"001e# service=git-upload-pack\n0000"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
