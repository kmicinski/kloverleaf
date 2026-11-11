//! Tracking a remote repository: a project that mirrors a directory of
//! somebody else's git repository (typically a paper in `paper/` of a GitHub
//! repo) and pushes back to it.
//!
//! ```text
//! projects/<name>/.kloverleaf/upstream.json   {url, branch, subdir, base, local_branch, …}
//! projects/<name>/.kloverleaf/upstream.git/   a bare clone; origin's branches under refs/remotes/origin/*
//! ```
//!
//! The project itself stays an ordinary kloverleaf project (its own git repo,
//! history, branches); the upstream is a second, independent relation kept by
//! one commit id, `base`: the upstream commit whose `subdir` the project last
//! agreed with. Everything is three file maps — `base` (that commit's subdir),
//! `ours` (the live texts + binary files on disk) and `theirs` (the subdir at
//! origin's branch head) — and a per-file three-way merge between them
//! (`merge_all`): line diff3 first, and inside a conflicting run of lines a
//! second diff3 over words, because LaTeX paragraphs are often a single long
//! line and two people editing different sentences of one paragraph is not a
//! conflict. What is left are conflicts the person resolves piece by piece
//! (`Pick`), identified by a hash of their contents so a resolution survives a
//! re-merge as long as that region did not change.
//!
//! * **pull** (`sync` with `Mode::Pull`; the poller runs one every minute
//!   while nobody is typing): fetch, merge, apply the merged texts through
//!   `Project::replace_if` (compare-and-swap: if the person typed in between,
//!   merge again), `base` := theirs. Incoming text is attributed to a `git`
//!   origin named after the commits' authors.
//! * **push** (`Mode::Push`; ⌘S in the editor, or the poller once the last
//!   editor has left): pull first, then one commit on top of origin's head
//!   whose tree is origin's tree with the changed files of `subdir` replaced
//!   (`git read-tree` into a private index, `hash-object`, `update-index`,
//!   `write-tree`, `commit-tree`) — linear history, nothing outside `subdir`
//!   touched, authored as the person (`KLOVERLEAF_GIT_IDENTITIES`). A push
//!   that loses a race is retried from the fetch.
//! * **activity** — where collaborators are working: `git blame` of origin's
//!   head, lines from commits that are not ours and are recent, mapped onto
//!   the live text by a line diff, grouped by LaTeX section.
//!
//! The project's `.gitignore` carries a `.kloverleaf/` line the upstream
//! never sees: it is stripped going out and added coming in.

use crate::live::Project;
use crate::project;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use similar::{Algorithm, DiffOp};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const CONFIG: &str = "upstream.json";
const MIRROR: &str = "upstream.git";
/// Commits newer than this count as someone's current work.
const ACTIVITY_DAYS: i64 = 30;
/// Nobody typed for this long: safe to pull into the live texts.
const IDLE_MS: i64 = 20_000;
/// The last editor left this long ago with unpushed work: push it.
const LEAVE_MS: i64 = 90_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub url: String,
    pub branch: String,
    /// Directory inside the repository the project mirrors; "" = all of it.
    pub subdir: String,
    /// The upstream commit the project last agreed with.
    pub base: String,
    /// The kloverleaf branch that tracks the upstream; other branches don't sync.
    pub local_branch: String,
    /// `https://github.com/owner/repo`, for links.
    #[serde(default)]
    pub web: Option<String>,
    /// Push unpushed work once the last editor has left.
    #[serde(default = "yes")]
    pub push_on_leave: bool,
    /// Commits this project pushed (newest last) — they are not "their" activity.
    #[serde(default)]
    pub pushed: Vec<String>,
}

fn yes() -> bool {
    true
}

fn private(dir: &Path) -> PathBuf {
    dir.join(project::PRIVATE_DIR)
}

pub fn load(dir: &Path) -> Option<Config> {
    std::fs::read_to_string(private(dir).join(CONFIG)).ok().and_then(|s| serde_json::from_str(&s).ok())
}

fn save(dir: &Path, cfg: &Config) -> std::io::Result<()> {
    let tmp = private(dir).join(format!("{CONFIG}.tmp"));
    std::fs::write(&tmp, serde_json::to_string_pretty(cfg).unwrap())?;
    std::fs::rename(tmp, private(dir).join(CONFIG))
}

pub fn tracked(dir: &Path) -> bool {
    private(dir).join(CONFIG).is_file()
}

// ---- git ------------------------------------------------------------------------

/// git with no prompts and ssh that never asks: `KLOVERLEAF_GIT_SSH_KEY` if set,
/// else whatever `$HOME/.ssh` holds; unknown hosts are accepted the first time.
async fn git_in(dir: &Path, args: &[&str], env: &[(&str, &str)], stdin: Option<&[u8]>) -> anyhow::Result<Vec<u8>> {
    let mut ssh = "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new".to_string();
    if let Ok(key) = std::env::var("KLOVERLEAF_GIT_SSH_KEY")
        && !key.is_empty()
    {
        ssh.push_str(&format!(" -o IdentitiesOnly=yes -i {key}"));
    }
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", ssh)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    if let Some(bytes) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(bytes).await?;
        pipe.shutdown().await?;
        drop(pipe);
    }
    let out = tokio::time::timeout(Duration::from_secs(300), child.wait_with_output()).await??;
    anyhow::ensure!(
        out.status.success(),
        "git {}: {}",
        args.first().unwrap_or(&""),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out.stdout)
}

async fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String> {
    Ok(String::from_utf8_lossy(&git_in(dir, args, &[], None).await?).into_owned())
}

async fn rev_parse(mirror: &Path, rev: &str) -> anyhow::Result<String> {
    Ok(git(mirror, &["rev-parse", "--verify", "-q", &format!("{rev}^{{commit}}")]).await?.trim().to_string())
}

async fn fetch(mirror: &Path) -> anyhow::Result<()> {
    git(mirror, &["fetch", "-q", "--prune", "origin"]).await.map(|_| ())
}

/// origin's branches, as of the last fetch.
async fn branches(mirror: &Path) -> anyhow::Result<Vec<String>> {
    Ok(git(mirror, &["for-each-ref", "--sort=-committerdate", "--format=%(refname:strip=3)", "refs/remotes/origin/"])
        .await?
        .lines()
        .filter(|b| *b != "HEAD")
        .map(String::from)
        .collect())
}

fn origin_ref(branch: &str) -> String {
    format!("refs/remotes/origin/{branch}")
}

/// Every file of `subdir` at `rev`: relative path → (mode, blob id). Symlinks
/// and submodules are not files a LaTeX project can hold, so they are left out.
async fn tree(mirror: &Path, rev: &str, subdir: &str) -> anyhow::Result<BTreeMap<String, (String, String)>> {
    let mut args = vec!["ls-tree", "-r", "-z", "--full-tree", rev];
    let spec = format!("{subdir}/");
    if !subdir.is_empty() {
        args.push("--");
        args.push(&spec);
    }
    let out = git(mirror, &args).await?;
    let mut files = BTreeMap::new();
    for rec in out.split('\0').filter(|r| !r.is_empty()) {
        let Some((meta, path)) = rec.split_once('\t') else { continue };
        let mut it = meta.split(' ');
        let (Some(mode), Some(kind), Some(sha)) = (it.next(), it.next(), it.next()) else { continue };
        if kind != "blob" || mode == "120000" {
            continue;
        }
        let rel = if subdir.is_empty() { path } else { path.strip_prefix(&spec).unwrap_or(path) };
        if syncable(rel) {
            files.insert(rel.to_string(), (mode.to_string(), sha.to_string()));
        }
    }
    Ok(files)
}

async fn blob(mirror: &Path, sha: &str) -> anyhow::Result<Vec<u8>> {
    git_in(mirror, &["cat-file", "blob", sha], &[], None).await
}

/// `subdir` at `rev` as bytes, `.gitignore` normalised (see `outgoing`).
async fn snapshot(mirror: &Path, rev: &str, subdir: &str) -> anyhow::Result<Files> {
    let mut out = Files::new();
    for (path, (_, sha)) in tree(mirror, rev, subdir).await? {
        if let Some(bytes) = outgoing(&path, blob(mirror, &sha).await?) {
            out.insert(path, bytes);
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    pub sha: String,
    pub author: String,
    pub email: String,
    pub ts: i64,
    pub message: String,
    pub files: Vec<String>,
}

/// Commits in `from..to` that touch `subdir`, newest first.
async fn commits(mirror: &Path, from: &str, to: &str, subdir: &str, limit: usize) -> anyhow::Result<Vec<Commit>> {
    let range = format!("{from}..{to}");
    let n = format!("-{limit}");
    let mut args = vec!["log", &n, "--format=%x1e%H%x1f%an%x1f%ae%x1f%at%x1f%s", "--name-only", &range];
    if !subdir.is_empty() {
        args.push("--");
        args.push(subdir);
    }
    let out = git(mirror, &args).await?;
    let prefix = if subdir.is_empty() { String::new() } else { format!("{subdir}/") };
    Ok(out
        .split('\x1e')
        .filter_map(|rec| {
            let mut lines = rec.lines();
            let head = lines.next()?;
            let mut f = head.split('\x1f');
            Some(Commit {
                sha: f.next()?.to_string(),
                author: f.next()?.to_string(),
                email: f.next()?.to_string(),
                ts: f.next()?.parse::<i64>().ok()? * 1000,
                message: f.next().unwrap_or("").to_string(),
                files: lines.filter(|l| !l.is_empty()).map(|l| l.strip_prefix(&prefix).unwrap_or(l).to_string()).collect(),
            })
        })
        .collect())
}

// ---- the three file maps -----------------------------------------------------------

pub type Files = BTreeMap<String, Vec<u8>>;

/// Files that travel between the project and the upstream: no build output.
fn syncable(path: &str) -> bool {
    project::valid_path(path) && !project::is_build_byproduct(path) && !path.ends_with(".DS_Store")
}

const PRIVATE_IGNORE: &str = ".kloverleaf/";

/// A file as the upstream should see it; `None` = not a file upstream (a
/// `.gitignore` that only ever said `.kloverleaf/`).
fn outgoing(path: &str, bytes: Vec<u8>) -> Option<Vec<u8>> {
    if path != ".gitignore" {
        return Some(bytes);
    }
    let s = String::from_utf8_lossy(&bytes);
    let kept: String = s.split_inclusive('\n').filter(|l| !matches!(l.trim(), ".kloverleaf/" | ".kloverleaf")).collect();
    if kept.trim().is_empty() { None } else { Some(kept.into_bytes()) }
}

/// The inverse of `outgoing`: the project's `.gitignore` keeps `.kloverleaf/` out of its own git.
fn incoming(path: &str, bytes: &[u8]) -> Vec<u8> {
    if path != ".gitignore" {
        return bytes.to_vec();
    }
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(PRIVATE_IGNORE);
    s.push('\n');
    s.into_bytes()
}

/// The project as the upstream would see it: live texts, binary files from disk.
fn ours(p: &Project) -> Files {
    let texts = p.texts();
    let mut out = Files::new();
    for f in p.files() {
        if !syncable(&f.path) {
            continue;
        }
        let bytes = match texts.get(&f.path) {
            Some(t) => t.clone().into_bytes(),
            None => match std::fs::read(p.dir.join(&f.path)) {
                Ok(b) => b,
                Err(_) => continue,
            },
        };
        if let Some(b) = outgoing(&f.path, bytes) {
            out.insert(f.path, b);
        }
    }
    // A text file the history knows but that has not reached the disk yet.
    for (path, t) in texts {
        if syncable(&path) && !out.contains_key(&path) && path != ".gitignore" {
            out.insert(path, t.into_bytes());
        }
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub path: String,
    /// added | modified | deleted
    pub status: &'static str,
}

fn changes(from: &Files, to: &Files) -> Vec<Change> {
    let paths: BTreeSet<&String> = from.keys().chain(to.keys()).collect();
    paths
        .into_iter()
        .filter_map(|p| match (from.get(p), to.get(p)) {
            (None, Some(_)) => Some(Change { path: p.clone(), status: "added" }),
            (Some(_), None) => Some(Change { path: p.clone(), status: "deleted" }),
            (Some(a), Some(b)) if a != b => Some(Change { path: p.clone(), status: "modified" }),
            _ => None,
        })
        .collect()
}

// ---- three-way merge ----------------------------------------------------------------

/// How one conflict is settled.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "pick", rename_all = "kebab-case")]
pub enum Pick {
    Ours,
    Theirs,
    /// Ours, then theirs.
    Both,
    TheirsFirst,
    Text { text: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Piece {
    Same { text: String },
    Conflict { id: String, base: String, ours: String, theirs: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct Conflict {
    pub path: String,
    /// Whole-file conflict (binary, or deleted on one side and changed on the
    /// other): settled with `ours`/`theirs` against `id`; `pieces` is empty.
    pub whole: bool,
    pub id: String,
    pub ours_exists: bool,
    pub theirs_exists: bool,
    pub pieces: Vec<Piece>,
}

pub struct Merged {
    /// The final content of every path (None = gone).
    pub files: BTreeMap<String, Option<Vec<u8>>>,
    pub conflicts: Vec<Conflict>,
    /// Conflicts settled by a `Pick` (or by the word-level pass) — for the log.
    pub settled: usize,
}

fn conflict_id(parts: &[&[u8]]) -> String {
    let mut h = 0xcbf29ce484222325u64;
    for part in parts {
        for &b in part.iter().chain(&[0xffu8]) {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
    }
    format!("{h:016x}")
}

/// Lines with their newline, so joining tokens gives the text back exactly.
fn lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

/// Runs of whitespace and of everything else.
fn words(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut prev: Option<bool> = None;
    for (i, c) in s.char_indices() {
        let ws = c.is_whitespace();
        if prev.is_some_and(|p| p != ws) {
            out.push(&s[start..i]);
            start = i;
        }
        prev = Some(ws);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// For every token of `x`, the index of the token of `y` it is matched with.
fn matched(x: &[&str], y: &[&str]) -> Vec<Option<usize>> {
    let mut m = vec![None; x.len()];
    for op in similar::capture_diff_slices(Algorithm::Patience, x, y) {
        if let DiffOp::Equal { old_index, new_index, len } = op {
            for i in 0..len {
                m[old_index + i] = Some(new_index + i);
            }
        }
    }
    m
}

/// diff3 over token slices: alternating stable regions (all three agree) and
/// unstable ones, as ranges into base, ours, theirs.
fn diff3(o: &[&str], a: &[&str], b: &[&str]) -> Vec<(Range<usize>, Range<usize>, Range<usize>, bool)> {
    let (ma, mb) = (matched(o, a), matched(o, b));
    let (mut io, mut ia, mut ib) = (0, 0, 0);
    let mut out = Vec::new();
    loop {
        let (so, sa, sb) = (io, ia, ib);
        while io < o.len() && ma[io] == Some(ia) && mb[io] == Some(ib) {
            io += 1;
            ia += 1;
            ib += 1;
        }
        if io > so {
            out.push((so..io, sa..ia, sb..ib, true));
        }
        if io == o.len() && ia == a.len() && ib == b.len() {
            break;
        }
        // The next base token both sides still have: the end of this unstable region.
        let mut k = io;
        while k < o.len() && !(ma[k].is_some_and(|x| x >= ia) && mb[k].is_some_and(|x| x >= ib)) {
            k += 1;
        }
        let (ea, eb) = if k < o.len() { (ma[k].unwrap(), mb[k].unwrap()) } else { (a.len(), b.len()) };
        out.push((io..k, ia..ea, ib..eb, false));
        (io, ia, ib) = (k, ea, eb);
    }
    out
}

/// One three-way text merge at the granularity of `split`; `Err` holds the
/// conflicting (base, ours, theirs) runs in place, `Ok` the merged text.
fn merge_tokens<'t>(o: &'t str, a: &'t str, b: &'t str, split: fn(&str) -> Vec<&str>) -> Vec<Result<String, (String, String, String)>> {
    let (to, ta, tb) = (split(o), split(a), split(b));
    let join = |t: &[&str], r: Range<usize>| t[r].concat();
    diff3(&to, &ta, &tb)
        .into_iter()
        .map(|(ro, ra, rb, stable)| {
            if stable {
                return Ok(join(&to, ro));
            }
            let (co, ca, cb) = (join(&to, ro), join(&ta, ra), join(&tb, rb));
            if ca == co || ca == cb {
                Ok(cb)
            } else if cb == co {
                Ok(ca)
            } else {
                Err((co, ca, cb))
            }
        })
        .collect()
}

fn apply_pick(pick: &Pick, ours: &str, theirs: &str) -> String {
    let both = |x: &str, y: &str| {
        if x.is_empty() || y.is_empty() || x.ends_with('\n') { format!("{x}{y}") } else { format!("{x}\n{y}") }
    };
    match pick {
        Pick::Ours => ours.into(),
        Pick::Theirs => theirs.into(),
        Pick::Both => both(ours, theirs),
        Pick::TheirsFirst => both(theirs, ours),
        Pick::Text { text } => text.clone(),
    }
}

/// Text merge: lines, then words inside each conflicting run of lines, then the picks.
fn merge_text(path: &str, base: &str, ours: &str, theirs: &str, picks: &HashMap<String, Pick>, settled: &mut usize) -> Result<String, Vec<Piece>> {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut open = false;
    let same = |pieces: &mut Vec<Piece>, s: String| match pieces.last_mut() {
        Some(Piece::Same { text }) => text.push_str(&s),
        _ => pieces.push(Piece::Same { text: s }),
    };
    for chunk in merge_tokens(base, ours, theirs, lines) {
        match chunk {
            Ok(s) => same(&mut pieces, s),
            Err((co, ca, cb)) => {
                let fine = merge_tokens(&co, &ca, &cb, words);
                if fine.iter().all(Result::is_ok) {
                    *settled += 1;
                    same(&mut pieces, fine.into_iter().map(Result::unwrap).collect());
                    continue;
                }
                let id = conflict_id(&[path.as_bytes(), co.as_bytes(), ca.as_bytes(), cb.as_bytes()]);
                match picks.get(&id) {
                    Some(pick) => {
                        *settled += 1;
                        same(&mut pieces, apply_pick(pick, &ca, &cb));
                    }
                    None => {
                        open = true;
                        pieces.push(Piece::Conflict { id, base: co, ours: ca, theirs: cb });
                    }
                }
            }
        }
    }
    if open {
        return Err(pieces);
    }
    Ok(pieces
        .into_iter()
        .map(|p| match p {
            Piece::Same { text } => text,
            Piece::Conflict { .. } => unreachable!(),
        })
        .collect())
}

/// A three-way text merge that always produces text: conflicting runs take
/// `theirs` (for an AI apply: the proposal the person just reviewed).
pub fn merge3(path: &str, base: &str, ours: &str, theirs: &str) -> String {
    let mut settled = 0;
    match merge_text(path, base, ours, theirs, &HashMap::new(), &mut settled) {
        Ok(t) => t,
        Err(pieces) => pieces
            .into_iter()
            .map(|p| match p {
                Piece::Same { text } => text,
                Piece::Conflict { theirs, .. } => theirs,
            })
            .collect(),
    }
}

pub fn merge_all(base: &Files, ours: &Files, theirs: &Files, picks: &HashMap<String, Pick>) -> Merged {
    let paths: BTreeSet<&String> = base.keys().chain(ours.keys()).chain(theirs.keys()).collect();
    let mut files = BTreeMap::new();
    let mut conflicts = Vec::new();
    let mut settled = 0;
    for path in paths {
        let (b, o, t) = (base.get(path), ours.get(path), theirs.get(path));
        let result = if o == t || t == b {
            o.cloned()
        } else if o == b {
            t.cloned()
        } else {
            fn text(x: Option<&Vec<u8>>) -> Option<Option<&str>> {
                x.map(|v| std::str::from_utf8(v).ok())
            }
            match (text(b).unwrap_or(Some("")), text(o), text(t)) {
                (Some(bs), Some(Some(os)), Some(Some(ts))) if project::is_text(path) => {
                    match merge_text(path, bs, os, ts, picks, &mut settled) {
                        Ok(m) => Some(m.into_bytes()),
                        Err(pieces) => {
                            conflicts.push(Conflict { path: path.clone(), whole: false, id: String::new(), ours_exists: true, theirs_exists: true, pieces });
                            continue;
                        }
                    }
                }
                _ => {
                    let none: &[u8] = b"\0none";
                    let id = conflict_id(&[path.as_bytes(), b.map_or(none, |v| v), o.map_or(none, |v| v), t.map_or(none, |v| v)]);
                    match picks.get(&id) {
                        Some(Pick::Ours) => { settled += 1; o.cloned() }
                        Some(Pick::Theirs) => { settled += 1; t.cloned() }
                        Some(Pick::Text { text }) => { settled += 1; Some(text.clone().into_bytes()) }
                        _ => {
                            conflicts.push(Conflict { path: path.clone(), whole: true, id, ours_exists: o.is_some(), theirs_exists: t.is_some(), pieces: vec![] });
                            continue;
                        }
                    }
                }
            }
        };
        files.insert(path.clone(), result);
    }
    Merged { files, conflicts, settled }
}

// ---- identities, sources ------------------------------------------------------------

/// `KLOVERLEAF_GIT_IDENTITIES="kmicinski=Kris M <k@x.com>;kdriscoll=…"` → (name, email) for commits.
pub fn identity(user: &str) -> (String, String) {
    let table = std::env::var("KLOVERLEAF_GIT_IDENTITIES").unwrap_or_default();
    for entry in table.split([';', '\n']) {
        let Some((login, id)) = entry.split_once('=') else { continue };
        if login.trim() == user
            && let Some((name, rest)) = id.split_once('<')
        {
            return (name.trim().to_string(), rest.trim_end_matches(['>', ' ']).trim().to_string());
        }
    }
    (user.to_string(), format!("{user}@kloverleaf"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub url: String,
    pub web: Option<String>,
    /// Path segments after `/tree/` in a GitHub link: a branch, then the
    /// directory — ambiguous when the branch has a slash, settled after cloning.
    pub tree: Vec<String>,
}

/// A repository from what someone pastes: a GitHub page (`…/tree/main/paper`),
/// `owner/repo`, or any git URL. GitHub is reached over ssh.
pub fn parse_source(input: &str) -> anyhow::Result<Source> {
    let s = input.trim().trim_end_matches('/');
    anyhow::ensure!(!s.is_empty(), "no repository given");
    let gh = s
        .strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))
        .or_else(|| s.strip_prefix("github.com/"))
        .or_else(|| s.strip_prefix("git@github.com:"))
        .or_else(|| s.strip_prefix("ssh://git@github.com/"))
        .or_else(|| {
            let simple = s.split('/').count() == 2 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c));
            simple.then_some(s)
        });
    if let Some(rest) = gh {
        let mut seg = rest.split('/');
        let owner = seg.next().filter(|o| !o.is_empty()).ok_or_else(|| anyhow::anyhow!("no owner in {s}"))?;
        let repo = seg.next().map(|r| r.trim_end_matches(".git")).filter(|r| !r.is_empty()).ok_or_else(|| anyhow::anyhow!("no repository in {s}"))?;
        let ok = |x: &str| x.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
        anyhow::ensure!(ok(owner) && ok(repo), "unexpected characters in {owner}/{repo}");
        let tree = match seg.next() {
            Some("tree" | "blob") => seg.map(String::from).collect(),
            _ => vec![],
        };
        return Ok(Source { url: format!("git@github.com:{owner}/{repo}.git"), web: Some(format!("https://github.com/{owner}/{repo}")), tree });
    }
    anyhow::ensure!(
        s.contains("://") || s.contains('@') || s.starts_with('/'),
        "not a repository URL: {s}"
    );
    anyhow::ensure!(!s.starts_with('-') && !s.contains(char::is_whitespace), "not a repository URL: {s}");
    Ok(Source { url: s.to_string(), web: None, tree: vec![] })
}

/// Clones `src` bare into `mirror` and settles the branch and the directory:
/// explicit values win, then the GitHub link's `/tree/<branch>/<dir>`, then
/// the remote's default branch and the repository root.
pub async fn clone(mirror: &Path, src: &Source, branch: Option<&str>, subdir: Option<&str>) -> anyhow::Result<(String, String)> {
    let parent = mirror.parent().unwrap();
    std::fs::create_dir_all(parent)?;
    git(parent, &["clone", "-q", "--bare", "--", &src.url, &mirror.to_string_lossy()]).await?;
    git(mirror, &["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"]).await?;
    fetch(mirror).await?;
    let branches: BTreeSet<String> = git(mirror, &["for-each-ref", "--format=%(refname:strip=3)", "refs/remotes/origin/"])
        .await?
        .lines()
        .map(String::from)
        .collect();
    let default = git(mirror, &["symbolic-ref", "--short", "HEAD"]).await.map(|s| s.trim().to_string()).unwrap_or_else(|_| "main".into());
    let (mut b, mut d) = (default, String::new());
    // `/tree/a/b/c`: the longest prefix that is a branch, the rest is the directory.
    for n in (1..=src.tree.len()).rev() {
        let candidate = src.tree[..n].join("/");
        if branches.contains(&candidate) {
            b = candidate;
            d = src.tree[n..].join("/");
            break;
        }
    }
    if let Some(x) = branch.map(str::trim).filter(|x| !x.is_empty()) {
        b = x.to_string();
    }
    if let Some(x) = subdir {
        d = x.trim().trim_matches('/').to_string();
    }
    anyhow::ensure!(branches.contains(&b), "the repository has no branch {b:?} (it has: {})", branches.iter().cloned().collect::<Vec<_>>().join(", "));
    anyhow::ensure!(d.is_empty() || project::valid_path(&d), "invalid directory {d:?}");
    Ok((b, d))
}

/// Lays out a new project directory from the upstream: files of `subdir` at
/// the branch head, `.kloverleaf/upstream.json`. The caller has cloned into
/// `<dir>/.kloverleaf/upstream.git` and opens the project afterwards.
pub async fn import(dir: &Path, src: &Source, branch: &str, subdir: &str, local_branch: &str) -> anyhow::Result<(Config, usize)> {
    let mirror = private(dir).join(MIRROR);
    let head = rev_parse(&mirror, &origin_ref(branch)).await?;
    let files = snapshot(&mirror, &head, subdir).await?;
    anyhow::ensure!(
        files.keys().any(|f| f.ends_with(".tex")),
        "no .tex file in {}{}",
        if subdir.is_empty() { "the repository root" } else { subdir },
        if subdir.is_empty() { " — give the directory the paper is in" } else { "" }
    );
    for (path, bytes) in &files {
        let full = dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap())?;
        std::fs::write(full, incoming(path, bytes))?;
    }
    let cfg = Config {
        url: src.url.clone(),
        branch: branch.into(),
        subdir: subdir.into(),
        base: head,
        local_branch: local_branch.into(),
        web: src.web.clone(),
        push_on_leave: true,
        pushed: vec![],
    };
    save(dir, &cfg)?;
    Ok((cfg, files.len()))
}

/// The tracked branch's history touching `subdir`, newest first (as of the last fetch).
pub async fn log(dir: &Path, n: usize) -> anyhow::Result<Vec<crate::gitd::Commit>> {
    let cfg = load(dir).ok_or_else(|| anyhow::anyhow!("this project does not track a repository"))?;
    let (count, rev) = (format!("-{n}"), origin_ref(&cfg.branch));
    let mut args = vec!["log", count.as_str(), rev.as_str()];
    if !cfg.subdir.is_empty() {
        args.extend(["--", cfg.subdir.as_str()]);
    }
    crate::gitd::log_of(&private(dir).join(MIRROR), &args).await
}

/// Every text file of the project's directory at upstream commit `rev`, with
/// the commit's full id — what a pull of that commit would have given.
pub async fn texts_at(dir: &Path, rev: &str) -> anyhow::Result<(String, BTreeMap<String, String>)> {
    anyhow::ensure!(crate::gitd::plain_rev(rev), "not a commit name: {rev}");
    let cfg = load(dir).ok_or_else(|| anyhow::anyhow!("this project does not track a repository"))?;
    let mirror = private(dir).join(MIRROR);
    let sha = rev_parse(&mirror, rev).await?;
    anyhow::ensure!(!sha.is_empty(), "no commit {rev} upstream");
    let files = snapshot(&mirror, &sha, &cfg.subdir)
        .await?
        .into_iter()
        .filter(|(p, _)| p != ".gitignore" && project::is_text(p))
        .filter_map(|(p, b)| Some((p, String::from_utf8(b).ok()?)))
        .collect();
    Ok((sha, files))
}

pub fn label(cfg: &Config) -> String {
    let repo = cfg
        .web
        .as_deref()
        .and_then(|w| w.strip_prefix("https://github.com/"))
        .map(String::from)
        .unwrap_or_else(|| cfg.url.rsplit(['/', ':']).next().unwrap_or(&cfg.url).trim_end_matches(".git").to_string());
    if cfg.subdir.is_empty() { repo } else { format!("{repo}/{}", cfg.subdir) }
}

// ---- runtime state ------------------------------------------------------------------

#[derive(Default, Clone, Serialize)]
struct Runtime {
    last_fetch: Option<i64>,
    error: Option<String>,
    busy: Option<&'static str>,
    last_push: Option<Value>,
    last_pull: Option<Value>,
}

#[derive(Clone)]
struct BlameLine {
    sha: String,
    author: String,
    email: String,
    ts: i64,
    summary: String,
}

/// Per project: the sync mutex (one fetch/merge/push at a time), what the
/// panel shows between calls, and `git blame` results cached per upstream head.
#[derive(Default)]
pub struct Upstreams {
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    state: Mutex<HashMap<String, Runtime>>,
    blame: Mutex<HashMap<(String, String, String), Arc<Vec<BlameLine>>>>,
}

impl Upstreams {
    fn lock_for(&self, project: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().unwrap().entry(project.into()).or_default().clone()
    }

    fn update(&self, project: &str, f: impl FnOnce(&mut Runtime)) {
        f(self.state.lock().unwrap().entry(project.into()).or_default());
    }

    fn runtime(&self, project: &str) -> Runtime {
        self.state.lock().unwrap().get(project).cloned().unwrap_or_default()
    }
}

// ---- sync ---------------------------------------------------------------------------

pub enum Mode {
    Pull,
    Push { message: Option<String>, leaving: bool },
    /// Track another branch of the same repository, then pull it. The old
    /// `base` stays the merge's ancestor, so the project becomes that
    /// branch's directory plus whatever was not pushed yet (conflicts as usual).
    Track { branch: String },
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

fn describe_files(list: &[Change]) -> String {
    let names: Vec<&str> = list.iter().map(|c| c.path.rsplit('/').next().unwrap_or(&c.path)).collect();
    match names.len() {
        0 => "nothing".into(),
        1..=4 => names.join(", "),
        n => format!("{} and {} more", names[..3].join(", "), n - 3),
    }
}

/// Writes the merged result into the live project. `false` when the person
/// typed since `ours` was read (the merge is then redone).
fn apply(p: &Project, ours: &Files, merged: &BTreeMap<String, Option<Vec<u8>>>, who: &str, meta: &Value) -> anyhow::Result<bool> {
    for (path, result) in merged {
        let mine = ours.get(path);
        if result.as_ref() == mine {
            continue;
        }
        match result {
            Some(bytes) => {
                let bytes = incoming(path, bytes);
                if project::is_text(path) && let Ok(text) = String::from_utf8(bytes.clone()) {
                    let expected = mine.map(|m| String::from_utf8_lossy(&incoming(path, m)).into_owned());
                    if !p.replace_if(path, expected.as_deref(), text, 0, who, "replace", meta.clone())? {
                        return Ok(false);
                    }
                } else {
                    p.write_file(path, bytes, who)?;
                }
            }
            None => {
                if p.dir.join(path).is_file() {
                    p.delete_file(path, who)?;
                }
            }
        }
    }
    Ok(true)
}

/// Builds the commit that makes origin's `base` hold `ours` in `subdir`.
async fn make_commit(mirror: &Path, dir: &Path, cfg: &Config, ours: &Files, message: &str, user: &str) -> anyhow::Result<Option<(String, Vec<Change>)>> {
    let modes = tree(mirror, &cfg.base, &cfg.subdir).await?;
    let base = snapshot(mirror, &cfg.base, &cfg.subdir).await?;
    let list = changes(&base, ours);
    if list.is_empty() {
        return Ok(None);
    }
    let index = private(dir).join("upstream.index");
    let _ = std::fs::remove_file(&index);
    let index_s = index.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", index_s.as_str())];
    git_in(mirror, &["read-tree", &cfg.base], &env, None).await?;
    for c in &list {
        let full = if cfg.subdir.is_empty() { c.path.clone() } else { format!("{}/{}", cfg.subdir, c.path) };
        match ours.get(&c.path) {
            Some(bytes) => {
                let sha = String::from_utf8_lossy(&git_in(mirror, &["hash-object", "-w", "--stdin"], &[], Some(bytes)).await?).trim().to_string();
                let mode = modes.get(&c.path).map(|(m, _)| m.as_str()).unwrap_or("100644");
                git_in(mirror, &["update-index", "--add", "--cacheinfo", &format!("{mode},{sha},{full}")], &env, None).await?;
            }
            None => {
                git_in(mirror, &["update-index", "--force-remove", "--", &full], &env, None).await?;
            }
        }
    }
    let tree_id = String::from_utf8_lossy(&git_in(mirror, &["write-tree"], &env, None).await?).trim().to_string();
    let _ = std::fs::remove_file(&index);
    let (name, email) = identity(user);
    let ids = [
        ("GIT_AUTHOR_NAME", name.as_str()),
        ("GIT_AUTHOR_EMAIL", email.as_str()),
        ("GIT_COMMITTER_NAME", name.as_str()),
        ("GIT_COMMITTER_EMAIL", email.as_str()),
    ];
    let commit = String::from_utf8_lossy(&git_in(mirror, &["commit-tree", &tree_id, "-p", &cfg.base, "-F", "-"], &ids, Some(message.as_bytes())).await?)
        .trim()
        .to_string();
    Ok(Some((commit, list)))
}

/// Pull (and for `Push`, then push). Serialised per project; emits the new
/// status to every editor when done. The returned value says what happened:
/// `{"result": "pulled"|"pushed"|"up-to-date"|"conflicts", …}`.
pub async fn sync(app: &crate::Shared, p: &Project, user: &str, mode: Mode, picks: &HashMap<String, Pick>) -> anyhow::Result<Value> {
    let lock = app.upstreams.lock_for(&p.name);
    let _guard = lock.lock().await;
    let what = match mode {
        Mode::Pull => "pulling",
        Mode::Push { .. } => "pushing",
        Mode::Track { .. } => "switching branch",
    };
    app.upstreams.update(&p.name, |r| r.busy = Some(what));
    p.emit_upstream(json!({"event": "busy", "busy": what}));
    let result = sync_locked(app, p, user, &mode, picks).await;
    app.upstreams.update(&p.name, |r| {
        r.busy = None;
        match &result {
            Ok(_) => r.error = None,
            Err(e) => r.error = Some(format!("{e:#}")),
        }
    });
    let mut status = status(app, p).await.unwrap_or_else(|e| json!({"error": format!("{e:#}")}));
    status["event"] = json!("status");
    if let Ok(r) = &result {
        status["last"] = r.clone();
    }
    p.emit_upstream(status);
    result
}

async fn sync_locked(app: &crate::Shared, p: &Project, user: &str, mode: &Mode, picks: &HashMap<String, Pick>) -> anyhow::Result<Value> {
    let mut cfg = load(&p.dir).ok_or_else(|| anyhow::anyhow!("this project does not track a repository"))?;
    anyhow::ensure!(
        p.branch() == cfg.local_branch,
        "the upstream syncs with kloverleaf branch {:?}; switch to it first",
        cfg.local_branch
    );
    let mirror = private(&p.dir).join(MIRROR);
    if let Mode::Track { branch } = mode {
        let branch = branch.trim();
        fetch(&mirror).await?;
        let known = branches(&mirror).await?;
        anyhow::ensure!(known.iter().any(|b| b == branch), "the repository has no branch {branch:?} (it has: {})", known.join(", "));
        if branch != cfg.branch {
            let from = std::mem::replace(&mut cfg.branch, branch.to_string());
            save(&p.dir, &cfg)?;
            let _ = p.history.note(user, "upstream", json!({"action": "track", "from": from, "to": branch}));
        }
    }
    let mut pulled: Option<Value> = None;
    for _attempt in 0..4 {
        fetch(&mirror).await?;
        app.upstreams.update(&p.name, |r| r.last_fetch = Some(crate::history::now_ms()));
        let theirs_rev = rev_parse(&mirror, &origin_ref(&cfg.branch)).await?;
        if theirs_rev != cfg.base {
            let base = snapshot(&mirror, &cfg.base, &cfg.subdir).await?;
            let theirs = snapshot(&mirror, &theirs_rev, &cfg.subdir).await?;
            let incoming_commits = commits(&mirror, &cfg.base, &theirs_rev, &cfg.subdir, 50).await.unwrap_or_default();
            let mut authors: Vec<String> = Vec::new();
            for c in incoming_commits.iter().filter(|c| !cfg.pushed.contains(&c.sha)) {
                if !authors.contains(&c.author) {
                    authors.push(c.author.clone());
                }
            }
            let who = if authors.is_empty() { label(&cfg) } else { authors.join(", ") };
            let meta = json!({"source": "git", "upstream": short(&theirs_rev), "repo": label(&cfg), "commits": incoming_commits.len()});
            // The live text may move under us; merge again until the swap holds.
            let mut applied = false;
            let mut merged_files = Vec::new();
            for _ in 0..5 {
                let ours = ours(p);
                let merged = merge_all(&base, &ours, &theirs, picks);
                if !merged.conflicts.is_empty() {
                    return Ok(json!({"result": "conflicts", "conflicts": merged.conflicts, "theirs": theirs_rev, "settled": merged.settled}));
                }
                merged_files = changes(&ours, &merged.files.iter().filter_map(|(k, v)| v.clone().map(|v| (k.clone(), v))).collect());
                if apply(p, &ours, &merged.files, &who, &meta)? {
                    applied = true;
                    break;
                }
            }
            anyhow::ensure!(applied, "the text kept changing during the merge; try again");
            let message = format!("merge {} from {} ({})", short(&theirs_rev), label(&cfg), who);
            p.flush();
            if let Ok(Some(sha)) = crate::gitd::commit(&p.dir, &message, Some(&who)).await {
                p.committed(&sha, &message, &who).await;
            }
            let _ = p.history.note(&who, "upstream", json!({"action": "pull", "from": short(&cfg.base), "to": short(&theirs_rev), "files": merged_files.len()}));
            cfg.base = theirs_rev.clone();
            save(&p.dir, &cfg)?;
            let info = json!({
                "at": crate::history::now_ms(), "sha": theirs_rev, "authors": authors,
                "commits": incoming_commits.iter().filter(|c| !cfg.pushed.contains(&c.sha)).count(),
                "files": merged_files,
            });
            app.upstreams.update(&p.name, |r| r.last_pull = Some(info.clone()));
            pulled = Some(info);
        }
        let Mode::Push { message, leaving } = mode else {
            return Ok(match pulled {
                Some(info) => json!({"result": "pulled", "pulled": info}),
                None => json!({"result": "up-to-date"}),
            });
        };
        p.flush();
        let ours = ours(p);
        let base = snapshot(&mirror, &cfg.base, &cfg.subdir).await?;
        let list = changes(&base, &ours);
        if list.is_empty() {
            return Ok(json!({"result": if pulled.is_some() { "pulled" } else { "up-to-date" }, "pulled": pulled}));
        }
        let message = match message.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            Some(m) => m.to_string(),
            None if *leaving => format!("Autosave from kloverleaf: {}\n\nPushed after the editor was closed with unpushed work.", describe_files(&list)),
            None => format!("Edit {}", describe_files(&list)),
        };
        let Some((commit, list)) = make_commit(&mirror, &p.dir, &cfg, &ours, &message, user).await? else {
            return Ok(json!({"result": "up-to-date", "pulled": pulled}));
        };
        match git(&mirror, &["push", "-q", "--porcelain", "origin", &format!("{commit}:refs/heads/{}", cfg.branch)]).await {
            Ok(_) => {}
            Err(e) if is_rejection(&format!("{e:#}")) => continue, // someone pushed meanwhile: fetch, merge, again
            Err(e) => return Err(e),
        }
        git(&mirror, &["update-ref", &origin_ref(&cfg.branch), &commit]).await?;
        cfg.base = commit.clone();
        cfg.pushed.push(commit.clone());
        let excess = cfg.pushed.len().saturating_sub(500);
        cfg.pushed.drain(..excess);
        save(&p.dir, &cfg)?;
        let local_msg = format!("push {} to {}: {}", short(&commit), label(&cfg), message.lines().next().unwrap_or(""));
        if let Ok(Some(sha)) = crate::gitd::commit(&p.dir, &local_msg, Some(user)).await {
            p.committed(&sha, &local_msg, user).await;
        }
        let _ = p.history.note(user, "upstream", json!({"action": "push", "sha": short(&commit), "files": list.len(), "message": message}));
        let info = json!({"at": crate::history::now_ms(), "sha": commit, "user": user, "message": message, "files": list, "leaving": leaving});
        app.upstreams.update(&p.name, |r| r.last_push = Some(info.clone()));
        return Ok(json!({"result": "pushed", "pushed": info, "pulled": pulled}));
    }
    anyhow::bail!("the push kept being rejected (someone is pushing at the same moment); try again")
}

fn is_rejection(err: &str) -> bool {
    ["rejected", "non-fast-forward", "fetch first", "stale info"].iter().any(|s| err.contains(s))
}

// ---- status -------------------------------------------------------------------------

/// What the panel shows, from what is already fetched (no network).
pub async fn status(app: &crate::Shared, p: &Project) -> anyhow::Result<Value> {
    let Some(cfg) = load(&p.dir) else { return Ok(json!({"tracked": false})) };
    let mirror = private(&p.dir).join(MIRROR);
    let head = rev_parse(&mirror, &origin_ref(&cfg.branch)).await?;
    let behind: Vec<Commit> = if head == cfg.base {
        vec![]
    } else {
        commits(&mirror, &cfg.base, &head, &cfg.subdir, 50).await?.into_iter().filter(|c| !cfg.pushed.contains(&c.sha)).collect()
    };
    let ours = ours(p);
    let base = snapshot(&mirror, &cfg.base, &cfg.subdir).await?;
    let ahead = changes(&base, &ours);
    let conflicts = if head != cfg.base {
        let theirs = snapshot(&mirror, &head, &cfg.subdir).await?;
        merge_all(&base, &ours, &theirs, &HashMap::new()).conflicts
    } else {
        vec![]
    };
    let rt = app.upstreams.runtime(&p.name);
    Ok(json!({
        "tracked": true,
        "label": label(&cfg),
        "url": cfg.url, "web": cfg.web, "branch": cfg.branch, "subdir": cfg.subdir,
        "branches": branches(&mirror).await.unwrap_or_default(),
        "local_branch": cfg.local_branch, "on_branch": p.branch() == cfg.local_branch,
        "base": cfg.base, "head": head,
        "behind": behind, "ahead": ahead, "conflicts": conflicts,
        "push_on_leave": cfg.push_on_leave,
        "last_fetch": rt.last_fetch, "error": rt.error, "busy": rt.busy,
        "last_push": rt.last_push, "last_pull": rt.last_pull,
    }))
}

pub fn set_push_on_leave(p: &Project, on: bool) -> anyhow::Result<()> {
    let mut cfg = load(&p.dir).ok_or_else(|| anyhow::anyhow!("not tracked"))?;
    cfg.push_on_leave = on;
    save(&p.dir, &cfg)?;
    Ok(())
}

// ---- activity: where collaborators are working ---------------------------------------

async fn blame(app: &crate::Shared, mirror: &Path, project: &str, head: &str, path: &str) -> anyhow::Result<Arc<Vec<BlameLine>>> {
    let key = (project.to_string(), head.to_string(), path.to_string());
    if let Some(b) = app.upstreams.blame.lock().unwrap().get(&key) {
        return Ok(b.clone());
    }
    let out = git(mirror, &["blame", "--line-porcelain", "-w", head, "--", path]).await?;
    let mut lines = Vec::new();
    let mut cur = BlameLine { sha: String::new(), author: String::new(), email: String::new(), ts: 0, summary: String::new() };
    for l in out.lines() {
        if let Some(rest) = l.strip_prefix('\t') {
            let _ = rest;
            lines.push(cur.clone());
        } else if let Some(v) = l.strip_prefix("author-mail ") {
            cur.email = v.trim_matches(['<', '>']).to_string();
        } else if let Some(v) = l.strip_prefix("author-time ") {
            cur.ts = v.parse::<i64>().unwrap_or(0) * 1000;
        } else if let Some(v) = l.strip_prefix("author ") {
            cur.author = v.to_string();
        } else if let Some(v) = l.strip_prefix("summary ") {
            cur.summary = v.to_string();
        } else if l.len() >= 40 && l.as_bytes()[..40].iter().all(u8::is_ascii_hexdigit) && l.as_bytes().get(40) == Some(&b' ') {
            cur.sha = l[..40].to_string();
        }
    }
    let lines = Arc::new(lines);
    let mut cache = app.upstreams.blame.lock().unwrap();
    if cache.len() > 400 {
        cache.clear();
    }
    cache.insert(key, lines.clone());
    Ok(lines)
}

/// 1-based line in `from` → 1-based line in `to`, for lines both texts share.
fn line_map(from: &str, to: &str) -> HashMap<usize, usize> {
    let (a, b) = (lines(from), lines(to));
    let mut m = HashMap::new();
    for op in similar::capture_diff_slices(Algorithm::Patience, &a, &b) {
        if let DiffOp::Equal { old_index, new_index, len } = op {
            for i in 0..len {
                m.insert(old_index + i + 1, new_index + i + 1);
            }
        }
    }
    m
}

/// The heading each line sits under (`\section{…}` and friends), 1-based.
fn sections(text: &str) -> Vec<Option<String>> {
    let re = regex::Regex::new(r"^\s*\\(?:part|chapter|section|subsection|subsubsection|paragraph)\*?\s*(?:\[[^\]]*\])?\s*\{([^}]*)\}").unwrap();
    let mut cur = None;
    let mut out = vec![None];
    for l in text.lines() {
        if let Some(c) = re.captures(l) {
            cur = Some(c[1].trim().to_string());
        }
        out.push(cur.clone());
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub struct Zone {
    pub file: String,
    pub from: usize,
    pub to: usize,
    pub author: String,
    pub email: String,
    pub ts: i64,
    pub sha: String,
    pub summary: String,
    pub section: Option<String>,
}

/// Recent lines by other people in one file, in the live text's line numbers.
async fn file_zones(app: &crate::Shared, p: &Project, cfg: &Config, head: &str, file: &str, mine: &(String, String)) -> anyhow::Result<Vec<Zone>> {
    let mirror = private(&p.dir).join(MIRROR);
    let Some(live) = p.text(file) else { return Ok(vec![]) };
    let full = if cfg.subdir.is_empty() { file.to_string() } else { format!("{}/{}", cfg.subdir, file) };
    let theirs = match git_in(&mirror, &["cat-file", "blob", &format!("{head}:{full}")], &[], None).await {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(_) => return Ok(vec![]), // not upstream (yet)
    };
    let lines = blame(app, &mirror, &p.name, head, &full).await?;
    let map = line_map(&theirs, &live);
    let heads = sections(&live);
    let cutoff = crate::history::now_ms() - ACTIVITY_DAYS * 86_400_000;
    let mut zones: Vec<Zone> = Vec::new();
    for (i, bl) in lines.iter().enumerate() {
        if bl.ts < cutoff || cfg.pushed.contains(&bl.sha) || bl.email.eq_ignore_ascii_case(&mine.1) || bl.sha.starts_with("0000000") {
            continue;
        }
        let Some(&at) = map.get(&(i + 1)) else { continue };
        match zones.last_mut() {
            Some(z) if z.sha == bl.sha && z.to + 1 >= at && z.to <= at => z.to = at,
            _ => zones.push(Zone {
                file: file.into(),
                from: at,
                to: at,
                author: bl.author.clone(),
                email: bl.email.clone(),
                ts: bl.ts,
                sha: bl.sha[..bl.sha.len().min(10)].to_string(),
                summary: bl.summary.clone(),
                section: heads.get(at).cloned().flatten(),
            }),
        }
    }
    Ok(zones)
}

/// `file` given: its zones. Otherwise the project's hot spots — recent work
/// grouped by (file, section, author), newest first.
pub async fn activity(app: &crate::Shared, p: &Project, file: Option<&str>, viewer: &str) -> anyhow::Result<Value> {
    let Some(cfg) = load(&p.dir) else { return Ok(json!({"tracked": false})) };
    let mirror = private(&p.dir).join(MIRROR);
    let head = rev_parse(&mirror, &origin_ref(&cfg.branch)).await?;
    let mine = identity(viewer);
    if let Some(f) = file {
        return Ok(json!({"file": f, "head": head, "zones": file_zones(app, p, &cfg, &head, f, &mine).await?}));
    }
    let mut spots: BTreeMap<(String, Option<String>, String), Value> = BTreeMap::new();
    for f in p.files().iter().filter(|f| f.text && (f.path.ends_with(".tex") || f.path.ends_with(".bib"))) {
        for z in file_zones(app, p, &cfg, &head, &f.path, &mine).await.unwrap_or_default() {
            let key = (z.file.clone(), z.section.clone(), z.author.clone());
            let e = spots.entry(key).or_insert_with(|| json!({
                "file": z.file, "section": z.section, "author": z.author, "ts": 0, "from": z.from, "to": z.to, "lines": 0, "summary": "",
            }));
            e["lines"] = json!(e["lines"].as_u64().unwrap_or(0) + (z.to - z.from + 1) as u64);
            e["from"] = json!(e["from"].as_u64().unwrap_or(0).min(z.from as u64));
            e["to"] = json!(e["to"].as_u64().unwrap_or(0).max(z.to as u64));
            if z.ts > e["ts"].as_i64().unwrap_or(0) {
                e["ts"] = json!(z.ts);
                e["summary"] = json!(z.summary);
            }
        }
    }
    let mut list: Vec<Value> = spots.into_values().collect();
    list.sort_by_key(|v| std::cmp::Reverse(v["ts"].as_i64().unwrap_or(0)));
    list.truncate(20);
    Ok(json!({"head": head, "spots": list, "days": ACTIVITY_DAYS}))
}

// ---- the poller ---------------------------------------------------------------------

/// Every `every`: for each tracked project, fetch; pull when nobody is typing;
/// push the last editor's unpushed work once they have been gone a while.
pub async fn poll(app: crate::Shared, every: Duration) {
    let mut last_error: HashMap<String, String> = HashMap::new();
    loop {
        tokio::time::sleep(every).await;
        let projects: Vec<Project> = app.projects.lock().unwrap().values().cloned().collect();
        for p in projects {
            let Some(cfg) = load(&p.dir) else { continue };
            if p.branch() != cfg.local_branch || app.upstreams.lock_for(&p.name).try_lock().is_err() {
                continue;
            }
            let (last_edit, editor) = p.last_edit();
            let quiet = crate::history::now_ms() - last_edit;
            let mut outcome = Ok(json!(null));
            if quiet > IDLE_MS {
                outcome = sync(&app, &p, "upstream", Mode::Pull, &HashMap::new()).await;
            }
            if outcome.as_ref().is_ok_and(|v| v["result"] != "conflicts")
                && cfg.push_on_leave
                && p.viewers() == 0
                && quiet > LEAVE_MS
                && !editor.is_empty()
                && dirty(&p, &cfg).await
            {
                outcome = sync(&app, &p, &editor, Mode::Push { message: None, leaving: true }, &HashMap::new()).await;
                if outcome.as_ref().is_ok_and(|v| v["result"] == "pushed") {
                    eprintln!("project {}: pushed {editor}'s unpushed work to {}", p.name, label(&cfg));
                }
            }
            match outcome {
                Err(e) => {
                    let msg = format!("{e:#}");
                    if last_error.get(&p.name) != Some(&msg) {
                        eprintln!("project {}: upstream: {msg}", p.name);
                        last_error.insert(p.name.clone(), msg);
                    }
                }
                Ok(_) => {
                    last_error.remove(&p.name);
                }
            }
        }
    }
}

async fn dirty(p: &Project, cfg: &Config) -> bool {
    let mirror = private(&p.dir).join(MIRROR);
    match snapshot(&mirror, &cfg.base, &cfg.subdir).await {
        Ok(base) => !changes(&base, &ours(p)).is_empty(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge3_keeps_typing_made_while_the_ai_worked() {
        let base = "Intro.\nSECRET sentence.\nEnd.\n";
        let live = "Intro, typed meanwhile.\nSECRET sentence.\nEnd.\n";
        let ai = "Intro.\nPublic sentence.\nEnd.\n";
        assert_eq!(merge3("a.tex", base, live, ai), "Intro, typed meanwhile.\nPublic sentence.\nEnd.\n");
        // Both changed the same words: the reviewed proposal wins.
        assert_eq!(merge3("a.tex", "x y z\n", "x Q z\n", "x R z\n"), "x R z\n");
    }

    fn files(pairs: &[(&str, &str)]) -> Files {
        pairs.iter().map(|(k, v)| (k.to_string(), v.as_bytes().to_vec())).collect()
    }

    fn text(m: &Merged, path: &str) -> String {
        String::from_utf8(m.files[path].clone().unwrap()).unwrap()
    }

    #[test]
    fn sources() {
        let s = parse_source("https://github.com/pawssec/verified_decompilation/tree/main/paper").unwrap();
        assert_eq!(s.url, "git@github.com:pawssec/verified_decompilation.git");
        assert_eq!(s.web.as_deref(), Some("https://github.com/pawssec/verified_decompilation"));
        assert_eq!(s.tree, vec!["main", "paper"]);
        assert_eq!(parse_source("pawssec/verified_decompilation").unwrap().tree, Vec::<String>::new());
        assert_eq!(parse_source("git@github.com:a/b.git").unwrap().url, "git@github.com:a/b.git");
        assert_eq!(parse_source("/srv/x.git").unwrap().url, "/srv/x.git");
        assert!(parse_source("hello world").is_err());
        assert!(parse_source("-uhoh").is_err());
    }

    #[test]
    fn gitignore_round_trip() {
        assert_eq!(outgoing(".gitignore", b".kloverleaf/\n".to_vec()), None);
        assert_eq!(outgoing(".gitignore", b"/main.pdf\n.kloverleaf/\n".to_vec()).unwrap(), b"/main.pdf\n");
        assert_eq!(incoming(".gitignore", b"/main.pdf"), b"/main.pdf\n.kloverleaf/\n");
        assert_eq!(outgoing(".gitignore", incoming(".gitignore", b"/x\n")).unwrap(), b"/x\n");
    }

    #[test]
    fn line_merge_takes_both_sides() {
        let base = files(&[("a.tex", "one\ntwo\nthree\nfour\n")]);
        let ours = files(&[("a.tex", "ONE\ntwo\nthree\nfour\n")]);
        let theirs = files(&[("a.tex", "one\ntwo\nthree\nFOUR\n"), ("new.tex", "hi\n")]);
        let m = merge_all(&base, &ours, &theirs, &HashMap::new());
        assert!(m.conflicts.is_empty());
        assert_eq!(text(&m, "a.tex"), "ONE\ntwo\nthree\nFOUR\n");
        assert_eq!(text(&m, "new.tex"), "hi\n");
    }

    #[test]
    fn same_paragraph_different_sentences_merges_by_word() {
        let base = files(&[("a.tex", "We prove A. We test B. We ship C.\n")]);
        let ours = files(&[("a.tex", "We prove A rigorously. We test B. We ship C.\n")]);
        let theirs = files(&[("a.tex", "We prove A. We test B. We ship C quickly.\n")]);
        let m = merge_all(&base, &ours, &theirs, &HashMap::new());
        assert!(m.conflicts.is_empty(), "{:?}", m.conflicts);
        assert_eq!(text(&m, "a.tex"), "We prove A rigorously. We test B. We ship C quickly.\n");
        assert_eq!(m.settled, 1);
    }

    #[test]
    fn real_conflicts_are_reported_then_settled_by_id() {
        let base = files(&[("a.tex", "intro\nthe result is good\nend\n")]);
        let ours = files(&[("a.tex", "intro\nthe result is great\nend\n")]);
        let theirs = files(&[("a.tex", "intro\nthe result is fine\nend\n")]);
        let m = merge_all(&base, &ours, &theirs, &HashMap::new());
        assert_eq!(m.conflicts.len(), 1);
        let Piece::Conflict { id, ours: o, theirs: t, .. } = &m.conflicts[0].pieces[1] else { panic!("{:?}", m.conflicts[0].pieces) };
        assert_eq!((o.as_str(), t.as_str()), ("the result is great\n", "the result is fine\n"));
        for (pick, want) in [
            (Pick::Theirs, "intro\nthe result is fine\nend\n"),
            (Pick::Both, "intro\nthe result is great\nthe result is fine\nend\n"),
            (Pick::Text { text: "the result is excellent\n".into() }, "intro\nthe result is excellent\nend\n"),
        ] {
            let picks = HashMap::from([(id.clone(), pick)]);
            let m = merge_all(&base, &ours, &theirs, &picks);
            assert!(m.conflicts.is_empty());
            assert_eq!(text(&m, "a.tex"), want);
        }
    }

    #[test]
    fn deletes_and_binaries() {
        let base = files(&[("gone.tex", "x\n"), ("fig.png", "\u{1}a")]);
        let ours = files(&[("fig.png", "\u{1}b")]); // we deleted gone.tex, changed the figure
        let theirs = files(&[("gone.tex", "x\n"), ("fig.png", "\u{1}c")]);
        let m = merge_all(&base, &ours, &theirs, &HashMap::new());
        assert_eq!(m.files.get("gone.tex"), Some(&None), "deletion carried");
        assert_eq!(m.conflicts.len(), 1);
        assert!(m.conflicts[0].whole);
        let picks = HashMap::from([(m.conflicts[0].id.clone(), Pick::Theirs)]);
        let m = merge_all(&base, &ours, &theirs, &picks);
        assert_eq!(text(&m, "fig.png"), "\u{1}c");
    }

    #[test]
    fn diff3_regions() {
        let o = ["a", "b", "c"];
        let a = ["a", "x", "b", "c"];
        let b = ["a", "b", "c", "y"];
        let r = diff3(&o, &a, &b);
        let stable: Vec<bool> = r.iter().map(|x| x.3).collect();
        assert_eq!(stable, vec![true, false, true, false]);
        let merged: String = merge_tokens("a\nb\nc\n", "a\nx\nb\nc\n", "a\nb\nc\ny\n", lines).into_iter().map(Result::unwrap).collect();
        assert_eq!(merged, "a\nx\nb\nc\ny\n");
    }

    #[test]
    fn sections_and_line_maps() {
        let s = sections("x\n\\section{Intro}\nhello\n\\subsection*{Deep}\nthere\n");
        assert_eq!(s[1], None);
        assert_eq!(s[3].as_deref(), Some("Intro"));
        assert_eq!(s[5].as_deref(), Some("Deep"));
        let m = line_map("a\nb\nc\n", "new\na\nb\nc\n");
        assert_eq!(m[&1], 2);
        assert_eq!(m[&3], 4);
    }

    #[test]
    fn identities() {
        // SAFETY: tests in this module are the only readers of this variable.
        unsafe { std::env::set_var("KLOVERLEAF_GIT_IDENTITIES", "kmicinski=Kristopher Micinski <k@example.com>;x=Y <y@z>") };
        assert_eq!(identity("kmicinski"), ("Kristopher Micinski".into(), "k@example.com".into()));
        assert_eq!(identity("nobody"), ("nobody".into(), "nobody@kloverleaf".into()));
    }
}
