//! Who is asking. Four ways in, checked in this order:
//!
//! 1. `Remote-User` from the reverse proxy when `KLOVERLEAF_TRUST_REMOTE_USER=1`
//!    (Authelia in front of Caddy, as on the home server) — the proxy has
//!    already authenticated the person; we take the name.
//! 2. `Authorization: Bearer <KLOVERLEAF_PASSWORD>` for tools, acting as the
//!    `X-User` header or `tool`.
//! 3. `Authorization: Basic user:<KLOVERLEAF_PASSWORD>` — what `git` sends.
//! 4. The session cookie set by the login form (name + the shared password).
//!
//! Without a password and without a trusted proxy the server is open as
//! `KLOVERLEAF_DEV_USER` (default `local`); it binds to localhost by default,
//! and `main` warns about it. TLS is the reverse proxy's job.

use crate::Shared;
use crate::text::escape;
use axum::Form;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use base64::Engine;
use serde::Deserialize;
use std::time::Duration;
use subtle::ConstantTimeEq;

fn eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

fn clean_user(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_alphanumeric() || "._-@".contains(c)))
        .then(|| s.to_string())
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|c| c.strip_prefix(name).and_then(|c| c.strip_prefix('=')))
        .map(str::to_string)
}

/// The authenticated user, if any.
pub fn identity(app: &Shared, headers: &HeaderMap) -> Option<String> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if app.trust_remote_user
        && let Some(u) = header("Remote-User").and_then(clean_user)
    {
        return Some(u);
    }
    if let Some(auth) = header("authorization") {
        if let Some(token) = auth.strip_prefix("Bearer ")
            && app.password.as_deref().is_some_and(|p| eq(token, p))
        {
            return Some(header("x-user").and_then(clean_user).unwrap_or_else(|| "tool".into()));
        }
        if let Some(b64) = auth.strip_prefix("Basic ")
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim())
            && let Ok(s) = String::from_utf8(bytes)
            && let Some((user, pass)) = s.split_once(':')
            && app.password.as_deref().is_some_and(|p| eq(pass, p))
        {
            return clean_user(user).or(Some("git".into()));
        }
    }
    if let Some(id) = cookie(headers, "session")
        && let Some(user) = app.sessions.lock().unwrap().get(&id)
    {
        return Some(user.clone());
    }
    if app.password.is_none() && !app.trust_remote_user {
        return Some(app.dev_user.clone());
    }
    None
}

/// Middleware for everything but the login page: browsers are sent to log in, tools get 401.
pub async fn gate(State(app): State<Shared>, req: Request, next: Next) -> Response {
    if identity(&app, req.headers()).is_some() {
        return next.run(req).await;
    }
    let wants_html = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));
    if wants_html {
        Redirect::to(&format!("/login?next={}", req.uri().path())).into_response()
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

/// Middleware for the git routes: HTTP Basic, so `git` prompts for the password.
pub async fn git_gate(State(app): State<Shared>, req: Request, next: Next) -> Response {
    if identity(&app, req.headers()).is_some() {
        return next.run(req).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, r#"Basic realm="kloverleaf""#)],
        "authentication required",
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct Login {
    #[serde(default)]
    user: String,
    password: String,
    #[serde(default)]
    next: String,
}

#[derive(Deserialize)]
pub struct NextParam {
    #[serde(default)]
    next: String,
}

fn page(next: &str, error: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>kloverleaf · login</title>
<link rel="stylesheet" href="/static/editor.css"></head>
<body class="page"><h1>kloverleaf</h1><form method="post" action="/login" class="login">
<label>name <input name="user" autocomplete="username" autofocus pattern="[A-Za-z0-9._@-]+" required></label>
<label>password <input type="password" name="password" autocomplete="current-password"></label>
<input type="hidden" name="next" value="{}"><button>log in</button><p class="error">{}</p></form></body></html>"#,
        escape(next),
        escape(error)
    ))
}

pub async fn login_form(State(app): State<Shared>, Query(q): Query<NextParam>, headers: HeaderMap) -> Response {
    if identity(&app, &headers).is_some() {
        return Redirect::to(safe_next(&q.next)).into_response();
    }
    page(&q.next, "").into_response()
}

fn safe_next(next: &str) -> &str {
    if next.starts_with('/') && !next.starts_with("//") { next } else { "/" }
}

pub async fn login(State(app): State<Shared>, Form(form): Form<Login>) -> Response {
    let ok = app.password.as_deref().is_some_and(|p| eq(&form.password, p));
    if let (true, Some(user)) = (ok, clean_user(&form.user)) {
        let id: String = rand::random::<[u8; 32]>().iter().map(|b| format!("{b:02x}")).collect();
        app.sessions.lock().unwrap().insert(id.clone(), user);
        return (
            [(
                header::SET_COOKIE,
                format!("session={id}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000"),
            )],
            Redirect::to(safe_next(&form.next)),
        )
            .into_response();
    }
    tokio::time::sleep(Duration::from_secs(1)).await; // blunt but effective against guessing
    let error = if ok { "name: letters, digits, . _ - @" } else { "wrong password" };
    (StatusCode::UNAUTHORIZED, page(&form.next, error)).into_response()
}

pub async fn logout(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(id) = cookie(&headers, "session") {
        app.sessions.lock().unwrap().remove(&id);
    }
    ([(header::SET_COOKIE, "session=; Path=/; Max-Age=0")], Redirect::to("/login")).into_response()
}
