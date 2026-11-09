//! What the agent CLIs have in common. A turn is one subprocess that streams
//! JSON events on stdout: [`drive`] spawns it, parks it on the session so a
//! cancel can kill it, hands every event to the provider's handler, keeps a
//! wall clock, collects stderr for the post-mortem, and turns the end into a
//! [`Turn`] or an error the author can act on. [`stale_resume`] is the
//! follow-up rule: a conversation the CLI can no longer pick up (its
//! transcript is gone) is retried once afresh, announced in the log.

use super::Session;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const WALL_CLOCK: Duration = Duration::from_secs(25 * 60);

pub struct Turn {
    pub text: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// How a turn is going, as the provider's event handler reports it.
#[derive(Default)]
pub struct Progress {
    /// The CLI said it is finished (well or badly); without this the run "exited without a result".
    pub done: bool,
    pub error: Option<String>,
}

/// Is `bin` runnable? A bare name is looked up on PATH.
pub fn find(bin: &str) -> Option<PathBuf> {
    let p = Path::new(bin);
    if p.components().count() > 1 {
        return p.is_file().then(|| p.to_path_buf());
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|d| Path::new(d).join(bin))
        .find(|c| c.is_file())
}

/// The CLI's id for this conversation, to resume on a follow-up.
pub fn resume_id(session: &Session) -> Option<String> {
    session.agent_session.lock().unwrap().clone()
}

/// A follow-up failed: was it the resume (the CLI no longer knows the
/// conversation)? Then say so in the log, forget the id, and answer `true`
/// so the caller runs the turn once more, afresh.
pub fn stale_resume(session: &Session, model: &str, resumed: bool, e: &anyhow::Error) -> bool {
    if !resumed || !e.to_string().contains("without a result") {
        return false;
    }
    session.emit(json!({"event": "turn", "role": "assistant", "text": "(could not resume the earlier conversation; starting a new one)", "model": model}));
    *session.agent_session.lock().unwrap() = None;
    true
}

/// One turn of `cmd` (`name` is for messages). `stdin`, if given, is written
/// to the process from a task, so a long message cannot deadlock against our
/// reading of its stdout. `on_event` sees every JSON line and fills in the
/// turn and the progress; `login_hint` is what to do about an auth failure.
pub async fn drive(
    name: &str,
    session: &Session,
    mut cmd: Command,
    stdin: Option<&str>,
    model: &str,
    login_hint: &str,
    mut on_event: impl FnMut(&Value, &mut Turn, &mut Progress),
) -> anyhow::Result<Turn> {
    let mut child = cmd
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not start {name}: {e}"))?;
    if let Some(msg) = stdin {
        let mut pipe = child.stdin.take().expect("piped stdin");
        let msg = msg.to_string();
        tokio::spawn(async move {
            let _ = pipe.write_all(msg.as_bytes()).await;
            let _ = pipe.shutdown().await;
        });
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let stderr_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });
    *session.child.lock().unwrap() = Some(child);

    let mut lines = BufReader::new(stdout).lines();
    let mut turn = Turn { text: String::new(), model: model.to_string(), input_tokens: 0, output_tokens: 0 };
    let mut progress = Progress::default();
    let deadline = tokio::time::Instant::now() + WALL_CLOCK;
    loop {
        let line = match tokio::time::timeout_at(deadline, lines.next_line()).await {
            Ok(Ok(Some(l))) => l,
            Ok(_) => break,
            Err(_) => {
                progress.error = Some(format!("gave up after {} minutes", WALL_CLOCK.as_secs() / 60));
                if let Some(c) = session.child.lock().unwrap().as_mut() {
                    let _ = c.start_kill();
                }
                break;
            }
        };
        if let Ok(ev) = serde_json::from_str::<Value>(&line) {
            on_event(&ev, &mut turn, &mut progress);
        }
    }

    let child = session.child.lock().unwrap().take();
    let status = match child {
        Some(mut c) => c.wait().await.ok(),
        None => None,
    };
    let stderr = stderr_task.await.unwrap_or_default();
    if session.cancel.load(Ordering::Relaxed) {
        anyhow::bail!("cancelled");
    }
    if let Some(e) = progress.error {
        anyhow::bail!("{}", explain(&e, &stderr, login_hint));
    }
    if !progress.done {
        let tail = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
        let exit = match status {
            Some(s) if !s.success() => format!(" ({s})"),
            _ => String::new(),
        };
        anyhow::bail!("{name} exited{exit} without a result: {}", explain(tail, &stderr, login_hint));
    }
    Ok(turn)
}

/// Turns the CLI's terser failures into something the author can act on.
fn explain(message: &str, stderr: &str, login_hint: &str) -> String {
    let all = format!("{message}\n{stderr}").to_ascii_lowercase();
    if ["401", "unauthorized", "oauth", "not logged in", "authentication", "login"].iter().any(|n| all.contains(n)) {
        return format!("{message} — {login_hint}");
    }
    if message.is_empty() { "no details".into() } else { message.into() }
}
