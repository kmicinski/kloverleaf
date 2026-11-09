//! Who is asking, and what they may touch. Ways in, checked in this order:
//!
//! 1. The session cookie set by the login form or an invite — a guest's
//!    (scoped) or an admin's (name + the shared password). It comes first
//!    because it is the most deliberate: someone who accepted an invitation
//!    *is* that guest, even when the proxy in front also knows them as a
//!    household account (otherwise everything they wrote would be attributed
//!    to that account). The editor's header names the session and offers to
//!    leave it.
//! 2. `Remote-User` from the reverse proxy when `KLOVERLEAF_TRUST_REMOTE_USER=1`
//!    (Authelia in front of Caddy, as on the home server) — the proxy has
//!    already authenticated the person; we take the name. Full access, unless
//!    `KLOVERLEAF_ADMINS` names the admins: then anyone else the proxy lets in
//!    is a *member* — confined, like a guest, to the projects they belong to
//!    (the ones they created, or were shared), but allowed to create more.
//! 3. `Authorization: Bearer <KLOVERLEAF_PASSWORD>` for tools, acting as the
//!    `X-User` header or `tool`. Full access.
//! 4. `Authorization: Basic user:<password>` — what `git` sends. A guest's own
//!    password scopes them to their projects; the shared password is full access.
//!
//! Guests come from invite links (`members.rs`): `/invite/<token>` lets them
//! pick a name and password for one project. Everything a guest does must
//! name a project they belong to; the project list shows only theirs; they
//! cannot create projects. Without a password and without a trusted proxy
//! the server is open as `KLOVERLEAF_DEV_USER` (default `local`); it binds to
//! localhost by default, and `main` warns about it. TLS is the proxy's job.

use crate::text::escape;
use crate::{Shared, pages};
use axum::Form;
use axum::extract::{OriginalUri, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use base64::Engine;
use serde::Deserialize;
use std::time::Duration;
use subtle::ConstantTimeEq;

/// An authenticated caller: the name, the projects they are confined to
/// (`None` = everything — an admin), and whether they may create projects.
/// Three shapes: an admin (everything), a member (their projects, may
/// create), a guest (their projects, may not).
#[derive(Clone, Debug)]
pub struct Who {
    pub user: String,
    pub projects: Option<Vec<String>>,
    pub creates: bool,
}

impl Who {
    pub fn full(user: String) -> Who {
        Who { user, projects: None, creates: true }
    }
    pub fn member(user: String, projects: Vec<String>) -> Who {
        Who { user, projects: Some(projects), creates: true }
    }
    pub fn guest(user: String, projects: Vec<String>) -> Who {
        Who { user, projects: Some(projects), creates: false }
    }
    pub fn admin(&self) -> bool {
        self.projects.is_none()
    }
    pub fn may(&self, project: &str) -> bool {
        self.projects.as_ref().is_none_or(|p| p.iter().any(|x| x == project))
    }
    /// May share the project, invite guests to it, remove members: admins,
    /// and members of that project. Guests never.
    pub fn manages(&self, project: &str) -> bool {
        self.admin() || (self.creates && self.may(project))
    }
}

fn eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

fn clean_user(s: &str) -> Option<String> {
    crate::members::valid_name(s).then(|| s.trim().to_string())
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

/// The caller, with scope.
pub fn who(app: &Shared, headers: &HeaderMap) -> Option<Who> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Some(w) = session(app, headers) {
        return Some(w);
    }
    if app.trust_remote_user
        && let Some(u) = header("Remote-User").and_then(clean_user)
    {
        return Some(if app.admins.is_empty() || app.admins.iter().any(|a| *a == u) {
            Who::full(u)
        } else {
            let projects = app.members.projects_of(&u);
            Who::member(u, projects)
        });
    }
    if let Some(auth) = header("authorization") {
        if let Some(token) = auth.strip_prefix("Bearer ")
            && app.password.as_deref().is_some_and(|p| eq(token, p))
        {
            return Some(Who::full(header("x-user").and_then(clean_user).unwrap_or_else(|| "tool".into())));
        }
        if let Some(b64) = auth.strip_prefix("Basic ")
            && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim())
            && let Ok(s) = String::from_utf8(bytes)
            && let Some((user, pass)) = s.split_once(':')
        {
            if let Some(u) = clean_user(user)
                && app.members.exists(&u)
                && let Some(projects) = app.members.verify(&u, pass)
            {
                return Some(Who::guest(u, projects));
            }
            if app.password.as_deref().is_some_and(|p| eq(pass, p)) {
                return Some(Who::full(clean_user(user).unwrap_or_else(|| "git".into())));
            }
        }
    }
    if app.password.is_none() && !app.trust_remote_user {
        return Some(Who::full(app.dev_user.clone()));
    }
    None
}

/// The caller as named by a login/invite session cookie, if they have one.
pub fn session(app: &Shared, headers: &HeaderMap) -> Option<Who> {
    let id = cookie(headers, "session")?;
    app.sessions.lock().unwrap().get(&id).cloned()
}

/// The authenticated user's name, if any (most callers only need the name).
pub fn identity(app: &Shared, headers: &HeaderMap) -> Option<String> {
    who(app, headers).map(|w| w.user)
}

pub fn is_admin(app: &Shared, headers: &HeaderMap) -> bool {
    who(app, headers).is_some_and(|w| w.admin())
}

/// May the caller manage `project`'s access (share it, invite, remove members)?
pub fn manages(app: &Shared, headers: &HeaderMap, project: &str) -> bool {
    who(app, headers).is_some_and(|w| w.manages(project))
}

/// The project a request path is about, for scope checks: `/edit/{p}`,
/// `/project/{p}/…`, `/api/projects/{p}/…`, `/git/{p}.git/…` (and the same
/// without the nest prefix, as a nested router sees them).
pub fn project_in_path(path: &str) -> Option<&str> {
    let p = path.trim_start_matches('/');
    let rest = p
        .strip_prefix("edit/")
        .or_else(|| p.strip_prefix("project/"))
        .or_else(|| p.strip_prefix("api/projects/"))
        .or_else(|| p.strip_prefix("projects/"))
        .or_else(|| p.strip_prefix("git/"))?;
    let name = rest.split('/').next()?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    (!name.is_empty()).then_some(name)
}

/// Is this request inside the caller's scope? Admins: always. Members and
/// guests: only their projects, plus the project list itself (filtered
/// elsewhere); project creation only for those who may create.
fn in_scope(w: &Who, path: &str, method: &axum::http::Method) -> bool {
    if w.admin() {
        return true;
    }
    match project_in_path(path) {
        Some(p) => w.may(p),
        None => {
            let p = path.trim_start_matches('/');
            // the list pages are fine; creating (POST /new, POST /api/projects) needs the right
            w.creates || !(method == axum::http::Method::POST && (p == "new" || p == "api/projects" || p == "projects"))
        }
    }
}

fn full_path(req: &Request) -> String {
    req.extensions().get::<OriginalUri>().map(|u| u.0.path().to_string()).unwrap_or_else(|| req.uri().path().to_string())
}

/// What a gate decides; the two gates differ only in how they challenge a stranger.
enum Admit {
    Allowed,
    /// Known, but this is not their project.
    Refused,
    /// Nobody we know.
    Unknown,
}

fn admit(app: &Shared, req: &Request) -> Admit {
    match who(app, req.headers()) {
        Some(w) if in_scope(&w, &full_path(req), req.method()) => Admit::Allowed,
        Some(_) => Admit::Refused,
        None => Admit::Unknown,
    }
}

const REFUSED: (StatusCode, &str) = (StatusCode::FORBIDDEN, "not your project");

/// Middleware for everything but the login page: browsers are sent to log in, tools get 401.
pub async fn gate(State(app): State<Shared>, req: Request, next: Next) -> Response {
    match admit(&app, &req) {
        Admit::Allowed => next.run(req).await,
        Admit::Refused => REFUSED.into_response(),
        Admit::Unknown => {
            let wants_html = req
                .headers()
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|a| a.contains("text/html"));
            if wants_html {
                Redirect::to(&format!("/login?next={}", full_path(&req))).into_response()
            } else {
                StatusCode::UNAUTHORIZED.into_response()
            }
        }
    }
}

/// Middleware for the git routes: HTTP Basic, so `git` prompts for the password.
pub async fn git_gate(State(app): State<Shared>, req: Request, next: Next) -> Response {
    match admit(&app, &req) {
        Admit::Allowed => next.run(req).await,
        Admit::Refused => REFUSED.into_response(),
        Admit::Unknown => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, r#"Basic realm="kloverleaf""#)],
            "authentication required",
        )
            .into_response(),
    }
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
    pages::shell(
        "login",
        &format!(
            r#"<form method="post" action="/login" class="login">
<label>name <input name="user" autocomplete="username" autofocus pattern="[A-Za-z0-9._@-]+" required></label>
<label>password <input type="password" name="password" autocomplete="current-password"></label>
<input type="hidden" name="next" value="{}"><button>log in</button><p class="error">{}</p></form>"#,
            escape(next),
            escape(error)
        ),
    )
}

pub async fn login_form(State(app): State<Shared>, Query(q): Query<NextParam>, headers: HeaderMap) -> Response {
    if who(&app, &headers).is_some() {
        return Redirect::to(safe_next(&q.next)).into_response();
    }
    page(&q.next, "").into_response()
}

fn safe_next(next: &str) -> &str {
    if next.starts_with('/') && !next.starts_with("//") { next } else { "/" }
}

fn start_session(app: &Shared, w: Who) -> String {
    let id = crate::text::random_hex(32);
    app.sessions.lock().unwrap().insert(id.clone(), w);
    id
}

fn session_cookie(id: &str) -> (header::HeaderName, String) {
    (header::SET_COOKIE, format!("session={id}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000"))
}

/// A guest's own password first; then the shared password (an admin).
pub async fn login(State(app): State<Shared>, Form(form): Form<Login>) -> Response {
    let user = clean_user(&form.user);
    if let Some(u) = &user
        && app.members.exists(u)
        && let Some(projects) = app.members.verify(u, &form.password)
    {
        let id = start_session(&app, Who::guest(u.clone(), projects));
        return ([session_cookie(&id)], Redirect::to(safe_next(&form.next))).into_response();
    }
    let ok = app.password.as_deref().is_some_and(|p| eq(&form.password, p));
    if let (true, Some(user)) = (ok, user) {
        let id = start_session(&app, Who::full(user));
        return ([session_cookie(&id)], Redirect::to(safe_next(&form.next))).into_response();
    }
    tokio::time::sleep(Duration::from_secs(1)).await; // blunt but effective against guessing
    let error = if ok { "name: letters, digits, . _ - @" } else { "wrong name or password" };
    (StatusCode::UNAUTHORIZED, page(&form.next, error)).into_response()
}

pub async fn logout(State(app): State<Shared>, headers: HeaderMap) -> Response {
    if let Some(id) = cookie(&headers, "session") {
        app.sessions.lock().unwrap().remove(&id);
    }
    // "/" rather than "/login": behind a trusted proxy the person is still
    // signed in as themselves and lands on the project list.
    ([(header::SET_COOKIE, "session=; Path=/; Max-Age=0")], Redirect::to("/")).into_response()
}

// ---- invites ----------------------------------------------------------------

fn invite_page(token: &str, project: &str, error: &str) -> Html<String> {
    pages::shell(
        "invitation",
        &format!(
            r#"<p>You have been invited to work on <strong>{project}</strong>. Pick a name and a password; they are yours for this project only.</p>
<form method="post" action="/invite/{token}" class="login">
<label>name <input name="user" autocomplete="username" autofocus pattern="[A-Za-z0-9._@-]+" required></label>
<label>password <input type="password" name="password" autocomplete="new-password" minlength="8" required></label>
<button>join</button><p class="error">{error}</p></form>
<p class="muted">Already have a kloverleaf name from another invitation? Use it with the same password.</p>"#,
            project = escape(project),
            token = escape(token),
            error = escape(error)
        ),
    )
}

fn no_invite(status: StatusCode, why: &str) -> Response {
    (status, pages::shell("invitation", &format!("<p>{why}</p>"))).into_response()
}

pub async fn invite_form(State(app): State<Shared>, Path(token): Path<String>) -> Response {
    match app.members.invite(&token) {
        Some(inv) if inv.used_by.is_none() => invite_page(&token, &inv.project, "").into_response(),
        Some(_) => no_invite(StatusCode::GONE, "This invitation was already used."),
        None => no_invite(StatusCode::NOT_FOUND, "No such invitation."),
    }
}

#[derive(Deserialize)]
pub struct Claim {
    user: String,
    password: String,
}

pub async fn invite_claim(State(app): State<Shared>, Path(token): Path<String>, Form(form): Form<Claim>) -> Response {
    let Some(inv) = app.members.invite(&token) else {
        return no_invite(StatusCode::NOT_FOUND, "No such invitation.");
    };
    match app.members.claim(&token, &form.user, &form.password) {
        Ok(projects) => {
            let id = start_session(&app, Who::guest(form.user.trim().to_string(), projects));
            ([session_cookie(&id)], Redirect::to(&format!("/edit/{}", inv.project))).into_response()
        }
        Err(e) => {
            tokio::time::sleep(Duration::from_secs(1)).await;
            (StatusCode::BAD_REQUEST, invite_page(&token, &inv.project, &e)).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_name_their_project() {
        assert_eq!(project_in_path("/edit/paper"), Some("paper"));
        assert_eq!(project_in_path("/project/paper/ws"), Some("paper"));
        assert_eq!(project_in_path("/api/projects/paper/files/main.tex"), Some("paper"));
        assert_eq!(project_in_path("/projects/paper/compile"), Some("paper"));
        assert_eq!(project_in_path("/git/paper.git/info/refs"), Some("paper"));
        assert_eq!(project_in_path("/paper.git/info/refs"), None);
        assert_eq!(project_in_path("/api/projects"), None);
        let guest = Who::guest("jade".into(), vec!["paper".into()]);
        let get = axum::http::Method::GET;
        let post = axum::http::Method::POST;
        assert!(in_scope(&guest, "/edit/paper", &get));
        assert!(!in_scope(&guest, "/edit/other", &get));
        assert!(in_scope(&guest, "/", &get));
        assert!(in_scope(&guest, "/api/projects", &get));
        assert!(!in_scope(&guest, "/api/projects", &post));
        assert!(!in_scope(&guest, "/new", &post));
        assert!(in_scope(&Who::full("k".into()), "/new", &post));
        // A member (a proxy user who is not an admin) is confined like a guest but may create.
        let member = Who::member("tom".into(), vec!["draft".into()]);
        assert!(in_scope(&member, "/edit/draft", &get));
        assert!(!in_scope(&member, "/edit/paper", &get));
        assert!(in_scope(&member, "/new", &post));
        assert!(in_scope(&member, "/api/projects", &post));
        assert!(member.manages("draft") && !member.manages("paper") && !guest.manages("paper"));
    }
}
