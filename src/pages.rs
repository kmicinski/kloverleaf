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

pub async fn static_file(Path(file): Path<String>) -> Response {
    let (mime, body) = match file.as_str() {
        "editor.js" => ("text/javascript", include_str!("../web/dist/editor.js")),
        "protocol.js" => ("text/javascript", include_str!("../web/dist/protocol.js")),
        "pdf.js" => ("text/javascript", include_str!("../web/dist/pdf.js")),
        "ai.js" => ("text/javascript", include_str!("../web/dist/ai.js")),
        "latex.js" => ("text/javascript", include_str!("../web/dist/latex.js")),
        "diff.js" => ("text/javascript", include_str!("../web/dist/diff.js")),
        "editor.css" => ("text/css", include_str!("../web/editor.css")),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

pub async fn index(State(app): State<Shared>, headers: HeaderMap) -> Html<String> {
    let user = auth::identity(&app, &headers).unwrap_or_default();
    let rows: String = app
        .projects
        .lock()
        .unwrap()
        .iter()
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
    let logout = if app.password.is_some() { r#" · <a href="/logout">log out</a>"# } else { "" };
    Html(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>kloverleaf</title>
<link rel="stylesheet" href="/static/editor.css"></head>
<body class="page"><h1>kloverleaf</h1><p class="muted">{user}{logout}</p><ul class="projects">{rows}</ul>
<form method="post" action="/new"><input name="name" placeholder="new-project-name" pattern="[A-Za-z0-9][A-Za-z0-9._-]*" required> <button>create</button></form>
</body></html>"#,
        user = escape(&user),
    ))
}

#[derive(Deserialize)]
pub struct NewProject {
    name: String,
}

pub async fn create(State(app): State<Shared>, Form(form): Form<NewProject>) -> Result<Redirect, (StatusCode, String)> {
    if app.project(&form.name).is_none() {
        app.create(&form.name).await.map_err(|e| (StatusCode::BAD_REQUEST, format!("{e:#}")))?;
    }
    Ok(Redirect::to(&format!("/edit/{}", form.name)))
}

pub async fn editor(State(app): State<Shared>, Path(name): Path<String>, headers: HeaderMap) -> Result<Html<String>, StatusCode> {
    if app.project(&name).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let user = auth::identity(&app, &headers).unwrap_or_default();
    Ok(Html(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>{name} · kloverleaf</title>
<link rel="stylesheet" href="/static/editor.css">
<script src="https://cdnjs.cloudflare.com/ajax/libs/monaco-editor/0.45.0/min/vs/loader.min.js"></script>
</head>
<body data-project="{name}" data-user="{user}">
<header>
  <a href="/">projects</a><strong>{name}</strong>
  <span id="status" class="muted"></span>
  <span id="build" class="muted"></span>
  <span class="spacer"></span>
  <span id="lock" class="muted"></span><button id="lock-btn" class="small">take lock</button>
  <button id="btn-files" class="small toggle on">files</button>
  <button id="btn-history" class="small toggle">history</button>
  <button id="btn-ai" class="small toggle">ai</button>
  <button id="btn-log" class="small toggle">log</button>
  <a href="/project/{name}/pdf" target="_blank">pdf</a>
</header>
<main>
  <aside id="files"><div class="pane-title">files <span class="spacer"></span><button id="file-new" class="tiny" title="new file">+</button><button id="file-upload" class="tiny" title="upload">↑</button></div><ul id="file-list"></ul><input type="file" id="file-input" multiple hidden></aside>
  <div id="editor"></div>
  <div id="divider"></div>
  <div id="pdf-pane">
    <div id="pdf-toolbar"><button id="pdf-sync" class="small" title="show the cursor's position in the PDF (⌥⏎)">⇢ pdf</button><span class="spacer"></span><span id="pdf-pages" class="muted"></span><button id="pdf-out" class="small">−</button><button id="pdf-in" class="small">+</button><button id="pdf-fit" class="small">fit</button></div>
    <div id="pdf-scroll"><div id="pdf-pages-wrap"></div></div>
    <div id="pdf-empty" class="muted">compiling…</div>
  </div>
  <aside id="ai-panel" hidden>
    <div class="pane-title">ai <span id="ai-mode" class="muted"></span><span class="spacer"></span><button id="ai-close" class="tiny">×</button></div>
    <div id="ai-context"></div>
    <div id="ai-queue"></div>
    <textarea id="ai-prompt" rows="4" placeholder="What should change? Select text and press ⌘⇧Q to queue regions for attention."></textarea>
    <div class="row"><button id="ai-run">run</button><button id="ai-cancel" class="small" hidden>cancel</button><span class="spacer"></span><span id="ai-status" class="muted"></span></div>
    <div id="ai-log"></div>
    <div id="ai-proposal" hidden></div>
  </aside>
</main>
<aside id="history" hidden>
  <div class="row"><strong>history</strong><input type="range" id="history-slider" min="0" max="0" value="0" title="scrub: the editor and the PDF show the document as it was"><span id="history-label" class="muted">now</span><button id="history-restore" class="small" disabled>restore this version</button><button id="history-now" class="small" disabled>back to now</button><button id="history-close" class="tiny" title="close">×</button></div>
  <div id="history-chips"></div>
</aside>
<pre id="log-pane" hidden></pre>
<script type="module" src="/static/editor.js"></script>
</body></html>"#,
        user = escape(&user)
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
