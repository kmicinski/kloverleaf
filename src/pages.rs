//! The app's own pages — project list, editor — the static assets they use,
//! the PDF, the socket, and the git smart-HTTP endpoints. HTML lives here as
//! Rust strings; the browser code is TypeScript in `web/src`, compiled by
//! `tsc` into `web/dist` and embedded at build time (`make web`).

use crate::text::escape;
use crate::{Shared, auth, live};
use axum::Form;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;
use std::io::Read;

/// `(name, body)` of every static asset, embedded at build time.
const STATIC: &[(&str, &str)] = &[
    ("editor.js", include_str!("../web/dist/editor.js")),
    ("provenance.js", include_str!("../web/dist/provenance.js")),
    ("tree.js", include_str!("../web/dist/tree.js")),
    ("protocol.js", include_str!("../web/dist/protocol.js")),
    ("pdf.js", include_str!("../web/dist/pdf.js")),
    ("ai.js", include_str!("../web/dist/ai.js")),
    ("latex.js", include_str!("../web/dist/latex.js")),
    ("diff.js", include_str!("../web/dist/diff.js")),
    ("upstream.js", include_str!("../web/dist/upstream.js")),
    ("editor.css", include_str!("../web/editor.css")),
];

pub async fn static_file(Path(file): Path<String>) -> Response {
    let Some((name, body)) = STATIC.iter().find(|(name, _)| *name == file) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mime = if name.ends_with(".css") { "text/css" } else { "text/javascript" };
    ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-cache")], *body).into_response()
}

/// The plain pages (project list, login, invitation): the stylesheet, the
/// name, and `body`.
pub fn shell(title: &str, body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>kloverleaf · {title}</title>
<link rel="stylesheet" href="/static/editor.css"></head>
<body class="page"><h1>kloverleaf</h1>{body}</body></html>"#
    ))
}

pub async fn index(State(app): State<Shared>, headers: HeaderMap) -> Html<String> {
    let who = auth::who(&app, &headers);
    let user = who.as_ref().map(|w| w.user.clone()).unwrap_or_default();
    let admin = who.as_ref().is_some_and(|w| w.admin());
    let creates = who.as_ref().is_some_and(|w| w.creates);
    let rows: String = app
        .projects
        .lock()
        .unwrap()
        .iter()
        .filter(|(name, _)| who.as_ref().is_none_or(|w| w.may(name)))
        .map(|(name, p)| {
            let lock = p
                .lock()
                .map(|l| format!(r#" <span class="muted">locked by {}</span>"#, escape(&l.user)))
                .unwrap_or_default();
            format!(
                r#"<li><a href="/edit/{name}">{name}</a>{lock} <span class="muted right"><a href="/project/{name}/pdf" target="_blank">pdf</a> · <code>git clone {}/git/{name}.git</code></span></li>"#,
                escape(&app.public_url)
            )
        })
        .collect();
    let logout = if app.password.is_some() || auth::session(&app, &headers).is_some() { r#" · <a href="/logout">log out</a>"# } else { "" };
    let create = if creates {
        r#"<form method="post" action="/new"><input name="name" placeholder="new-project-name" pattern="[A-Za-z0-9][A-Za-z0-9._-]*" required> <button>create</button></form>
<details class="track"><summary>…or track a GitHub repository</summary>
<form method="post" action="/new" class="track-form" onsubmit="this.querySelector('button').disabled=true;this.querySelector('button').textContent='cloning…'">
<input name="name" placeholder="project-name" pattern="[A-Za-z0-9][A-Za-z0-9._-]*" required>
<input name="repo" placeholder="https://github.com/owner/repo/tree/main/paper" required size="48">
<input name="subdir" placeholder="directory (if not in the link)">
<button>clone &amp; track</button>
<p class="muted">The project mirrors that directory of the repository: their pushes are merged in every minute while you aren't typing, ⌘S pushes your edits as one commit, and work left unpushed when you close the editor is pushed for you.</p>
</form></details>"#
    } else {
        ""
    };
    // A member has no shared password, so git over HTTP needs one of their own.
    let git_pw = if creates && !admin {
        let state = if app.members.exists(&user) { "change" } else { "set" };
        format!(
            r#"<form method="post" action="/me/password" class="login"><label>git password <input type="password" name="password" autocomplete="new-password" minlength="8" required></label> <button>{state}</button> <span class="muted">for <code>git clone https://{}:…@…/git/&lt;name&gt;.git</code></span></form>"#,
            escape(&user)
        )
    } else {
        String::new()
    };
    let empty = if rows.is_empty() && creates && !admin { r#"<p class="muted">No projects yet — create one below, or ask someone to share theirs with you.</p>"# } else { "" };
    shell("projects", &format!(r#"<p class="muted">{}{logout}</p>{empty}<ul class="projects">{rows}</ul>{create}{git_pw}"#, escape(&user)))
}

#[derive(Deserialize)]
pub struct Password {
    password: String,
}

/// The project list's "git password" form (members only; see `api::set_password`).
pub async fn set_password(State(app): State<Shared>, headers: HeaderMap, Form(form): Form<Password>) -> Result<Redirect, (StatusCode, String)> {
    let who = auth::who(&app, &headers).filter(|w| w.creates && !w.admin()).ok_or((StatusCode::FORBIDDEN, "members only".into()))?;
    app.members.set_password(&who.user, &form.password).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    Ok(Redirect::to("/"))
}

#[derive(Deserialize)]
pub struct NewProject {
    name: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    subdir: Option<String>,
}

pub async fn create(State(app): State<Shared>, headers: HeaderMap, Form(form): Form<NewProject>) -> Result<Redirect, (StatusCode, String)> {
    let who = auth::who(&app, &headers).filter(|w| w.creates).ok_or((StatusCode::FORBIDDEN, "guests cannot create projects".into()))?;
    let repo = form.repo.as_deref().map(str::trim).filter(|r| !r.is_empty());
    if app.project(&form.name).is_none() {
        match repo {
            Some(repo) => app.create_tracked_for(&form.name, repo, None, form.subdir.as_deref().filter(|s| !s.trim().is_empty()), &who).await.map(|_| ()),
            None => app.create_for(&form.name, &who).await.map(|_| ()),
        }
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    } else if !who.may(&form.name) {
        return Err((StatusCode::CONFLICT, "that name is taken".into()));
    }
    Ok(Redirect::to(&format!("/edit/{}", form.name)))
}

pub async fn editor(State(app): State<Shared>, Path(name): Path<String>, headers: HeaderMap) -> Result<Html<String>, StatusCode> {
    if app.project(&name).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let who = auth::who(&app, &headers);
    let user = who.as_ref().map(|w| w.user.clone()).unwrap_or_default();
    let admin = who.as_ref().is_some_and(|w| w.admin());
    let manages = if who.as_ref().is_some_and(|w| w.manages(&name)) { "1" } else { "0" };
    // Who this page acts as. A login/invite session is named and can be left;
    // a proxy identity has nothing to leave.
    let (who_label, who_title) = match auth::session(&app, &headers) {
        Some(w) if w.admin() => (format!(r#"{} · <a href="/logout">log out</a>"#, escape(&w.user)), "signed in with the shared password".to_string()),
        Some(w) => (
            format!(r#"{} <span class="muted">(guest)</span> · <a href="/logout">leave</a>"#, escape(&w.user)),
            format!("you are {}, a guest of this project; what you write is yours", w.user),
        ),
        None => (escape(&user), "you".to_string()),
    };
    Ok(Html(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>{name} · kloverleaf</title>
<link rel="stylesheet" href="/static/editor.css">
<script src="https://cdnjs.cloudflare.com/ajax/libs/monaco-editor/0.45.0/min/vs/loader.min.js"></script>
</head>
<body data-project="{name}" data-user="{user}" data-admin="{admin}" data-manages="{manages}">
<header>
  <a href="/">projects</a><strong>{name}</strong>
  <span id="status" class="muted"></span>
  <span id="build" class="muted"></span>
  <span id="wc" class="muted" title="prose words in this file · reading time at 230 wpm"></span>
  <span id="prov-stat" title="share of this file written by the AI — click to hide the colours"></span>
  <button id="branch-chip" class="small" title="branch — click for the list, or to fork a new one">⎇ main</button>
  <button id="upstream-chip" class="small" hidden>⇅ …</button>
  <span class="spacer"></span>
  <span id="lock" class="muted"></span><button id="lock-btn" class="small">take lock</button>
  <button id="btn-files" class="small toggle on">files</button>
  <button id="btn-history" class="small toggle">history</button>
  <button id="btn-ai" class="small toggle">ai</button>
  <button id="btn-log" class="small toggle">log</button>
  <button id="btn-share" class="small" title="share this project with someone who already has a login here (a household account)" hidden>share</button>
  <button id="btn-invite" class="small" title="invite someone to this project (they get their own login, for this project only)" hidden>invite</button>
  <span id="whoami" class="muted" title="{who_title}">{who}</span>
  <button id="btn-keys" class="tiny" title="keyboard shortcuts">?</button>
  <a href="/project/{name}/pdf" target="_blank">pdf</a>
</header>
<main>
  <aside id="files"><div class="pane-title">files <span class="spacer"></span><button id="file-new" class="tiny" title="new file">+</button><button id="file-upload" class="tiny" title="upload">↑</button></div><ul id="file-list"></ul><div class="pane-title outline-title">outline</div><ul id="outline"></ul><input type="file" id="file-input" multiple hidden></aside>
  <div id="editor-col"><div id="editor"></div><div id="errbar" hidden></div></div>
  <div id="divider"></div>
  <div id="pdf-pane">
    <div id="pane-tabs"><button id="tab-pdf" class="tab on">pdf</button><button id="tab-ai" class="tab" title="ai (⌘⇧A)">✦ ai</button></div>
    <div id="pdf-toolbar"><button id="pdf-sync" class="small" title="show the cursor's position in the PDF (⌥⏎)">⇢ pdf</button><span class="spacer"></span><span id="pdf-pages" class="muted"></span><button id="pdf-out" class="small">−</button><button id="pdf-in" class="small">+</button><button id="pdf-fit" class="small">fit</button></div>
    <div id="pdf-stale" hidden></div>
    <div id="pdf-scroll"><div id="pdf-pages-wrap"></div></div>
    <div id="pdf-empty" class="muted">compiling…</div>
    <aside id="ai-panel" hidden>
      <div id="ai-thread"><div id="ai-empty" class="muted">Ask for a change. Select text and ⌘⇧Q to point at regions.</div></div>
      <div id="ai-bar" hidden></div>
      <div id="ai-composer">
        <div id="ai-chips"><span id="ai-ctx"></span><span id="ai-queue"></span></div>
        <textarea id="ai-prompt" rows="2" placeholder="What should change?"></textarea>
        <div class="row"><span id="ai-mode" class="muted"></span><span id="ai-status" class="muted"></span><span class="spacer"></span><button id="ai-cancel" class="small" hidden>cancel</button><button id="ai-run" class="small" title="send (⌘⏎)">send ⏎</button></div>
      </div>
    </aside>
  </div>
</main>
<aside id="history" hidden>
  <div class="row"><strong>history</strong><input type="range" id="history-slider" min="0" max="0" value="0" title="scrub: the editor and the PDF show the document as it was"><span id="history-label" class="muted">now</span><button id="history-restore" class="small" disabled>restore this version</button><button id="history-branch" class="small" disabled title="start a new branch whose text is this version">branch from here</button><button id="history-now" class="small" disabled>back to now</button><button id="history-close" class="tiny" title="close">×</button></div>
  <div id="history-tree" title="the history as a tree: lanes are branches; click a dot to look at that moment, double-click to branch from it, click a lane name to switch"></div>
  <div id="history-strip" title="every change over time — click a bar to look at that stretch, shift-click to widen"></div>
  <div id="history-range" class="muted"></div>
  <div id="history-chips"></div>
</aside>
<pre id="log-pane" hidden></pre>
<script type="module" src="/static/editor.js"></script>
</body></html>"#,
        user = escape(&user),
        admin = if admin { "1" } else { "0" },
        who = who_label,
        who_title = escape(&who_title),
    )))
}

pub async fn pdf(State(app): State<Shared>, Path(name): Path<String>) -> Response {
    match crate::api::pdf(State(app), Path(name)).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

pub async fn ws(
    State(app): State<Shared>,
    Path(name): Path<String>,
    headers: HeaderMap,
    upgrade: axum::extract::WebSocketUpgrade,
) -> Response {
    // Cookies ride along on cross-site WebSocket handshakes; only our own pages may open one.
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(|o| o.split("://").nth(1));
    if origin.is_none() || origin != host {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(user) = auth::identity(&app, &headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match app.project(&name) {
        Some(p) => upgrade.on_upgrade(move |socket| live::session(socket, app, p, user)),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---- git smart HTTP --------------------------------------------------------------

#[derive(Deserialize)]
pub struct Service {
    #[serde(default)]
    service: String,
}

fn repo(app: &Shared, repo: &str) -> Result<live::Project, Response> {
    let name = repo.strip_suffix(".git").unwrap_or(repo);
    app.project(name)
        .ok_or_else(|| (StatusCode::NOT_FOUND, "no such repository").into_response())
}

pub async fn git_refs(State(app): State<Shared>, Path(name): Path<String>, Query(q): Query<Service>) -> Response {
    let p = match repo(&app, &name) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if q.service != "git-upload-pack" && q.service != "git-receive-pack" {
        return (StatusCode::FORBIDDEN, "smart HTTP only").into_response();
    }
    if q.service == "git-receive-pack" {
        // A push updates the working tree, which must be clean: commit what the editor has.
        p.flush();
        if let Ok(Some(sha)) = crate::gitd::commit(&p.dir, "autosave before push", None).await {
            p.committed(&sha, "autosave before push", "kloverleaf").await;
        }
    }
    match crate::gitd::advertise(&p.dir, &q.service).await {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, format!("application/x-{}-advertisement", q.service)),
                (header::CACHE_CONTROL, "no-cache".into()),
            ],
            body,
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}

pub async fn git_rpc(
    State(app): State<Shared>,
    Path((name, service)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let p = match repo(&app, &name) {
        Ok(p) => p,
        Err(r) => return r,
    };
    if service != "git-upload-pack" && service != "git-receive-pack" {
        return StatusCode::NOT_FOUND.into_response();
    }
    let body = if headers.get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()) == Some("gzip") {
        let mut out = Vec::new();
        if flate2::read::GzDecoder::new(&body[..]).read_to_end(&mut out).is_err() {
            return (StatusCode::BAD_REQUEST, "bad gzip body").into_response();
        }
        out
    } else {
        body.to_vec()
    };
    let user = auth::identity(&app, &headers).unwrap_or_else(|| "git".into());
    match crate::gitd::rpc(&p.dir, &service, body).await {
        Ok(out) => {
            if service == "git-receive-pack" {
                if let Err(e) = p.reload_from_disk(&user) {
                    eprintln!("project {}: reload after push: {e:#}", p.name);
                }
                if let Ok(sha) = crate::gitd::head(&p.dir).await {
                    p.committed(&sha, "pushed", &user).await;
                }
            }
            (
                [
                    (header::CONTENT_TYPE, format!("application/x-{service}-result")),
                    (header::CACHE_CONTROL, "no-cache".into()),
                ],
                out,
            )
                .into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")).into_response(),
    }
}
