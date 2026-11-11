//! JSON/text API for tools — an LLM driving a project from outside, scripts,
//! the editor's own non-socket needs. Same authority as the editor: writes go
//! through the live project, so open editors update at once.
//!
//! ```text
//! GET    /api/projects                              → [{name, main, here}]  (here: who has it open)
//! POST   /api/projects                 ← {name}     → {name}
//! GET    /api/projects/{p}/files                    → [{path, size, text}]
//! GET    /api/projects/{p}/files/{path}             → the file
//! PUT    /api/projects/{p}/files/{path} ← bytes     → 204   (text files become live documents)
//! DELETE /api/projects/{p}/files/{path}             → 204
//! POST   /api/projects/{p}/main        ← {file}     → 204
//! POST   /api/projects/{p}/compile                  → {version, ok, diags, ms, pdf}  (waits for the compile)
//! GET    /api/projects/{p}/build                    → the last compile, same shape
//! GET    /api/projects/{p}/log                      → latexmk's log
//! GET    /api/projects/{p}/pdf                      → the PDF  (also /project/{p}/pdf)
//! GET    /api/projects/{p}/page/{n}.png?dpi=        → one page rendered
//! GET    /api/projects/{p}/sync/forward?file&line&col → [{page, h, v, width, height}]  (top-left origin, pt)
//! GET    /api/projects/{p}/sync/inverse?page&x&y    → {file, line, column} | null
//! GET    /api/projects/{p}/history?file&before&limit → [entries]
//! GET    /api/projects/{p}/history/{id}?file=       → {entry, text, at}  (text of `file` as of that entry)
//! GET    /api/projects/{p}/history/{id}/pdf         → the project compiled as it was then
//! POST   /api/projects/{p}/history/{id}/restore     → 204   (new entry; history never rewinds)
//! GET    /api/projects/{p}/here                     → [{id, user, file}]  (open editor pages)
//! GET    /api/projects/{p}/commits?n                → [{sha, ts, author, message}]
//! POST   /api/projects/{p}/commit      ← {message}  → {sha} | null
//! POST   /api/projects/{p}/ai          ← {prompt, context?} → {session, text, files, model, …}  (synchronous)
//! POST   /api/projects/{p}/ai/{s}/apply ← {files?}  → {applied}
//! DELETE /api/projects/{p}/ai/{s}                   → 204
//! GET    /api/projects/{p}/invites                  → {invites, members}      (managers)
//! POST   /api/projects/{p}/invites                  → {token, url}            (managers)
//! DELETE /api/projects/{p}/invites/{token}          → 204
//! POST   /api/projects/{p}/members     ← {user}     → {members}   share with a proxy account (managers)
//! DELETE /api/projects/{p}/members/{user}           → 204
//! POST   /api/me/password              ← {password} → 204   a member's git password (not for guests)
//! GET    /api/projects/{p}/upstream                 → status of the tracked repository (no network)
//! POST   /api/projects/{p}/upstream/pull            → {result: pulled|up-to-date|conflicts, …}
//! POST   /api/projects/{p}/upstream/push  ← {message?} → {result: pushed|pulled|up-to-date|conflicts, …}
//! POST   /api/projects/{p}/upstream/resolve ← {picks: {id: {pick, text?}}, push, message?} → same
//! GET    /api/projects/{p}/upstream/activity?file   → collaborators' recent lines (file) or hot spots
//! POST   /api/projects/{p}/upstream/settings ← {push_on_leave} → 204
//! POST   /api/projects/{p}/upstream/branch ← {branch} → like pull   track another upstream branch (managers)
//! ```
//!
//! `POST /api/projects` also takes `{name, repo, branch?, subdir?}`: a project
//! that tracks `subdir` of that repository (see `upstream.rs`).
//!
//! Authenticate with the session cookie, `Authorization: Bearer <KLOVERLEAF_PASSWORD>`
//! (acting as `X-User`, default `tool`), or `Remote-User` behind the proxy.

use crate::live::Project;
use crate::{Shared, auth};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

type ApiResult<T> = Result<T, (StatusCode, String)>;

/// Any error (anyhow's chain, rusqlite's, …) as a response with this status.
fn bad<E: std::fmt::Display>(status: StatusCode) -> impl Fn(E) -> (StatusCode, String) {
    move |e| (status, format!("{e:#}"))
}

fn project(app: &Shared, name: &str) -> ApiResult<Project> {
    app.project(name).ok_or((StatusCode::NOT_FOUND, "no such project".into()))
}

fn user(app: &Shared, headers: &HeaderMap) -> String {
    auth::identity(app, headers).unwrap_or_else(|| "tool".into())
}

/// The compiled PDF on disk, for the SyncTeX and page-image tools.
fn pdf_file(p: &Project) -> ApiResult<std::path::PathBuf> {
    p.pdf_path().filter(|p| p.is_file()).ok_or((StatusCode::NOT_FOUND, "no PDF yet".into()))
}

fn branches_json(p: &Project) -> Json<Value> {
    Json(json!({"current": p.branch(), "branches": p.history.branches()}))
}

pub async fn projects(State(app): State<Shared>, headers: HeaderMap) -> Json<Value> {
    let who = auth::who(&app, &headers);
    let list: Vec<Value> = app
        .projects
        .lock()
        .unwrap()
        .iter()
        .filter(|(name, _)| who.as_ref().is_none_or(|w| w.may(name)))
        .map(|(name, p)| json!({"name": name, "main": p.main(), "here": p.here_users()}))
        .collect();
    Json(json!(list))
}

#[derive(Deserialize)]
pub struct NewProject {
    pub name: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub subdir: Option<String>,
}

pub async fn create_project(State(app): State<Shared>, headers: HeaderMap, Json(body): Json<NewProject>) -> ApiResult<Json<Value>> {
    let who = auth::who(&app, &headers).filter(|w| w.creates).ok_or((StatusCode::FORBIDDEN, "guests cannot create projects".into()))?;
    match body.repo.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        Some(repo) => app
            .create_tracked_for(&body.name, repo, body.branch.as_deref(), body.subdir.as_deref().filter(|s| !s.trim().is_empty()), &who)
            .await
            .map(|_| ()),
        None => app.create_for(&body.name, &who).await.map(|_| ()),
    }
    .map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(Json(json!({"name": body.name})))
}

/// A member's password for git over HTTP Basic (admins have the shared one,
/// guests chose theirs on the invite page).
pub async fn set_password(State(app): State<Shared>, headers: HeaderMap, Json(body): Json<Password>) -> ApiResult<StatusCode> {
    let who = auth::who(&app, &headers).filter(|w| w.creates && !w.admin()).ok_or((StatusCode::FORBIDDEN, "members only".into()))?;
    app.members.set_password(&who.user, &body.password).map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct Password {
    pub password: String,
}

// ---- access: invites, shares (managers = admins + the project's members) ------

fn manager(app: &Shared, headers: &HeaderMap, project: &str) -> ApiResult<()> {
    if auth::manages(app, headers, project) { Ok(()) } else { Err((StatusCode::FORBIDDEN, "not yours to share".into())) }
}

/// The project's invites and members.
pub async fn invites(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    manager(&app, &headers, &p)?;
    let _ = project(&app, &p)?;
    let invites: Vec<Value> = app.members.invites_for(&p).into_iter().map(|i| json!({
        "token": i.token, "url": format!("{}/invite/{}", app.public_url, i.token), "created_by": i.created_by,
        "created": i.created, "used_by": i.used_by, "used_at": i.used_at,
    })).collect();
    Ok(Json(json!({"invites": invites, "members": app.members.members_of(&p)})))
}

/// A fresh invite link for the project.
pub async fn create_invite(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    manager(&app, &headers, &p)?;
    let _ = project(&app, &p)?;
    let by = user(&app, &headers);
    let token = app.members.create_invite(&p, &by).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!({"token": token, "url": format!("{}/invite/{token}", app.public_url)})))
}

pub async fn revoke_invite(State(app): State<Shared>, Path((p, token)): Path<(String, String)>, headers: HeaderMap) -> ApiResult<StatusCode> {
    manager(&app, &headers, &p)?;
    let _ = project(&app, &p)?;
    if app.members.revoke(&token) { Ok(StatusCode::NO_CONTENT) } else { Err((StatusCode::NOT_FOUND, "no such unused invite".into())) }
}

pub async fn remove_member(State(app): State<Shared>, Path((p, who)): Path<(String, String)>, headers: HeaderMap) -> ApiResult<StatusCode> {
    manager(&app, &headers, &p)?;
    let _ = project(&app, &p)?;
    if app.members.remove_member(&p, &who) { Ok(StatusCode::NO_CONTENT) } else { Err((StatusCode::NOT_FOUND, "not a member".into())) }
}

#[derive(Deserialize)]
pub struct Share {
    pub user: String,
}

/// Share the project with someone the proxy already knows (a household
/// account): from then on it is in their list. No password involved.
pub async fn add_member(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap, Json(body): Json<Share>) -> ApiResult<Json<Value>> {
    manager(&app, &headers, &p)?;
    let _ = project(&app, &p)?;
    let user = body.user.trim();
    if !crate::members::valid_name(user) {
        return Err((StatusCode::BAD_REQUEST, "name: letters, digits, . _ - @".into()));
    }
    app.members.add_member(&p, user).map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(json!({"members": app.members.members_of(&p)})))
}

pub async fn files(State(app): State<Shared>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    Ok(Json(json!({"files": p.files(), "main": p.main()})))
}

pub async fn get_file(State(app): State<Shared>, Path((p, path)): Path<(String, String)>) -> ApiResult<Response> {
    let p = project(&app, &p)?;
    let bytes = p.read_file(&path).ok_or((StatusCode::NOT_FOUND, "no such file".into()))?;
    let mime = mime_of(&path);
    Ok(([(header::CONTENT_TYPE, mime)], bytes).into_response())
}

pub async fn put_file(
    State(app): State<Shared>,
    Path((p, path)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let p = project(&app, &p)?;
    p.write_file(&path, body.to_vec(), &user(&app, &headers)).map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_file(State(app): State<Shared>, Path((p, path)): Path<(String, String)>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let p = project(&app, &p)?;
    p.delete_file(&path, &user(&app, &headers)).map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct MainFile {
    file: String,
}

pub async fn set_main(State(app): State<Shared>, Path(p): Path<String>, Json(body): Json<MainFile>) -> ApiResult<StatusCode> {
    project(&app, &p)?.set_main(&body.file).map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn compile(State(app): State<Shared>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let b = p.build_now().await.ok_or((StatusCode::INTERNAL_SERVER_ERROR, "build loop gone".into()))?;
    Ok(Json(b.summary()))
}

pub async fn build(State(app): State<Shared>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    Ok(Json(p.latest().map(|b| b.summary()).unwrap_or(Value::Null)))
}

pub async fn log(State(app): State<Shared>, Path(p): Path<String>) -> ApiResult<Response> {
    let p = project(&app, &p)?;
    let log = p.latest().map(|b| b.log.clone()).unwrap_or_default();
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], log).into_response())
}

pub async fn pdf(State(app): State<Shared>, Path(p): Path<String>) -> ApiResult<Response> {
    let p = project(&app, &p)?;
    let pdf = p.latest().and_then(|b| b.pdf.clone()).ok_or((StatusCode::NOT_FOUND, "no PDF yet".into()))?;
    Ok((
        [(header::CONTENT_TYPE, "application/pdf"), (header::CACHE_CONTROL, "no-store")],
        pdf.as_ref().clone(),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct Dpi {
    #[serde(default)]
    dpi: Option<u32>,
}

pub async fn page(State(app): State<Shared>, Path((p, n)): Path<(String, String)>, Query(q): Query<Dpi>) -> ApiResult<Response> {
    let p = project(&app, &p)?;
    let n: u32 = n.trim_end_matches(".png").parse().map_err(|_| (StatusCode::BAD_REQUEST, "page".into()))?;
    let png = crate::build::render_page(&pdf_file(&p)?, n, q.dpi.unwrap_or(110).clamp(30, 300))
        .await
        .map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(([(header::CONTENT_TYPE, "image/png")], png).into_response())
}

#[derive(Deserialize)]
pub struct Forward {
    file: String,
    line: usize,
    #[serde(default)]
    col: usize,
}

pub async fn sync_forward(State(app): State<Shared>, Path(p): Path<String>, Query(q): Query<Forward>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let pdf = pdf_file(&p)?;
    if !crate::project::valid_path(&q.file) {
        return Err((StatusCode::BAD_REQUEST, "file".into()));
    }
    let boxes = crate::build::forward(&p.dir, &pdf, &q.file, q.line, q.col)
        .await
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?;
    // SyncTeX narrows to a paragraph's boxes; the words around the cursor find
    // the exact rendered word (see build::forward_word).
    if let Some(text) = p.text(&q.file)
        && let Some(line) = text.lines().nth(q.line.saturating_sub(1))
        && let Some(b) = crate::build::forward_word(&pdf, line, q.col, &boxes).await
    {
        return Ok(Json(json!([b])));
    }
    Ok(Json(json!(boxes)))
}

#[derive(Deserialize)]
pub struct Inverse {
    page: u32,
    x: f64,
    y: f64,
}

pub async fn sync_inverse(State(app): State<Shared>, Path(p): Path<String>, Query(q): Query<Inverse>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let pdf = pdf_file(&p)?;
    let mut pos = crate::build::inverse(&p.dir, &pdf, q.page, q.x, q.y)
        .await
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?;
    // SyncTeX gives a line (often approximate); the clicked words find the
    // exact line and column. See build.rs "click → source".
    if let Some(sp) = pos.as_mut()
        && let Some((idx, words)) = crate::build::words_at(&pdf, q.page, q.x, q.y).await
        && let Some(text) = p.text(&sp.file)
        && let Some((line, col)) = crate::build::locate(&text, &words, idx, sp.line)
    {
        sp.line = line;
        sp.column = col as i64;
    }
    Ok(Json(json!(pos)))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    after: Option<i64>,
    /// A branch name (its lineage), `*` for everything, or unset for the current branch.
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    before: Option<i64>,
    #[serde(default)]
    limit: Option<usize>,
}

pub async fn history(State(app): State<Shared>, Path(p): Path<String>, Query(q): Query<HistoryQuery>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let list = p
        .history
        .list(q.file.as_deref(), q.before, q.after, q.limit.unwrap_or(60).clamp(1, 500), q.branch.as_deref())
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(json!(list)))
}

#[derive(Deserialize)]
pub struct SummaryQuery {
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    buckets: Option<usize>,
    #[serde(default)]
    branch: Option<String>,
}

/// The history as a tree: branches and their notable events, for the map.
pub async fn history_tree(State(app): State<Shared>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let events = p.history.tree(600).map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(json!({"current": p.branch(), "branches": p.history.branches(), "events": events})))
}

pub async fn branches(State(app): State<Shared>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    Ok(branches_json(&project(&app, &p)?))
}

#[derive(Deserialize)]
pub struct NewBranch {
    name: String,
    /// The entry to fork at; unset = now (the current branch's latest state).
    #[serde(default)]
    from: Option<i64>,
    #[serde(default)]
    note: String,
}

/// Forks a branch at an entry and switches to it.
pub async fn create_branch(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap, Json(b): Json<NewBranch>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    p.branch_from(b.from, &b.name, &user(&app, &headers), &b.note).await.map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(branches_json(&p))
}

pub async fn switch_branch(State(app): State<Shared>, Path((p, name)): Path<(String, String)>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    p.switch_branch(&name, &user(&app, &headers)).await.map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(branches_json(&p))
}

/// The whole timeline as `buckets` time slices with counts — the density
/// strip the history drawer zooms with.
pub async fn history_summary(State(app): State<Shared>, Path(p): Path<String>, Query(q): Query<SummaryQuery>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let b = p.history.summary(q.file.as_deref(), q.buckets.unwrap_or(120), q.branch.as_deref()).map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(json!(b)))
}

#[derive(Deserialize)]
pub struct ProvQuery {
    file: String,
}

/// `{spans: [[len, origin], …], origins: {id: {kind, user, ts, meta}}}` for a file.
pub async fn provenance(State(app): State<Shared>, Path(p): Path<String>, Query(q): Query<ProvQuery>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let v = p.provenance(&q.file).ok_or((StatusCode::NOT_FOUND, "no such file".into()))?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub struct OriginsQuery {
    ids: String,
}

pub async fn origins(State(app): State<Shared>, Path(p): Path<String>, Query(q): Query<OriginsQuery>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let ids: Vec<u32> = q.ids.split(',').filter_map(|s| s.trim().parse().ok()).take(500).collect();
    Ok(Json(json!(p.origins(ids))))
}

#[derive(Deserialize)]
pub struct ClaimBody {
    file: String,
    from: usize,
    to: usize,
}

/// Recomputes the project's provenance maps from its history (admins).
pub async fn rebuild_provenance(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    if !auth::is_admin(&app, &headers) {
        return Err((StatusCode::FORBIDDEN, "admins only".into()));
    }
    let p = project(&app, &p)?;
    let n = tokio::task::spawn_blocking(move || p.rebuild_provenance())
        .await
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(json!({"rebuilt": n})))
}

/// "This is mine": re-attributes a range (UTF-16 offsets) to the caller.
pub async fn claim(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap, Json(b): Json<ClaimBody>) -> ApiResult<StatusCode> {
    let p = project(&app, &p)?;
    let user = user(&app, &headers);
    p.claim(&b.file, b.from, b.to, &user).map_err(bad(StatusCode::NOT_FOUND))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct EntryQuery {
    #[serde(default)]
    file: Option<String>,
}

/// The entry, and the text at that moment: of the entry's own file, or of
/// `?file=` — "what this file was when that happened", for time travel.
pub async fn history_entry(State(app): State<Shared>, Path((p, id)): Path<(String, i64)>, Query(q): Query<EntryQuery>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let entry = p.history.entry(id).ok_or((StatusCode::NOT_FOUND, "no such entry".into()))?;
    let at = match &q.file {
        Some(f) if *f != entry.file => p.history.latest_for(f, id),
        _ => Some(id),
    };
    let text = match at {
        Some(at) => p.history.text_at(at).map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?,
        None => None,
    };
    Ok(Json(json!({"entry": entry, "text": text, "at": at})))
}

pub async fn history_pdf(State(app): State<Shared>, Path((p, id)): Path<(String, i64)>) -> ApiResult<Response> {
    let p = project(&app, &p)?;
    let pdf = p.pdf_at(id).await.map_err(bad(StatusCode::NOT_FOUND))?;
    Ok(([(header::CONTENT_TYPE, "application/pdf"), (header::CACHE_CONTROL, "private, max-age=3600")], pdf).into_response())
}

/// Restores `?file=` (or the entry's file) to what it was at that entry.
pub async fn restore(State(app): State<Shared>, Path((p, id)): Path<(String, i64)>, Query(q): Query<EntryQuery>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let p = project(&app, &p)?;
    let mut entry = p.history.entry(id).ok_or((StatusCode::NOT_FOUND, "no such entry".into()))?;
    if let Some(f) = q.file.filter(|f| *f != entry.file) {
        let at = p.history.latest_for(&f, id).ok_or((StatusCode::NOT_FOUND, "file did not exist then".into()))?;
        entry = p.history.entry(at).ok_or((StatusCode::NOT_FOUND, "no such entry".into()))?;
    }
    let id = entry.id;
    let text = p
        .history
        .text_at(id)
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?
        .ok_or((StatusCode::BAD_REQUEST, "entry has no text".into()))?;
    let user = user(&app, &headers);
    p.replace(&entry.file, text, 0, &user, "restore", json!({"restores": id}))
        .map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_here(State(app): State<Shared>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(project(&app, &p)?.here())))
}

#[derive(Deserialize)]
pub struct Count {
    #[serde(default)]
    n: Option<usize>,
}

pub async fn commits(State(app): State<Shared>, Path(p): Path<String>, Query(q): Query<Count>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let log = crate::gitd::log(&p.dir, q.n.unwrap_or(50).clamp(1, 1000))
        .await
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(json!(log)))
}

#[derive(Deserialize)]
pub struct CommitBody {
    #[serde(default)]
    message: Option<String>,
}

pub async fn commit(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap, Json(body): Json<CommitBody>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let user = user(&app, &headers);
    p.flush();
    let message = body.message.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| format!("checkpoint by {user}"));
    let sha = crate::gitd::commit(&p.dir, &message, Some(&user))
        .await
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?;
    if let Some(sha) = &sha {
        p.committed(sha, &message, &user).await;
    }
    Ok(Json(json!({"sha": sha})))
}

#[derive(Deserialize)]
pub struct AiBody {
    prompt: String,
    #[serde(default)]
    context: Option<crate::ai::Context>,
    #[serde(default)]
    session: Option<u64>,
    #[serde(flatten)]
    choice: crate::ai::ChoiceReq,
}

pub async fn ai_run(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap, Json(body): Json<AiBody>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let user = user(&app, &headers);
    let choice = app.ai.catalog.resolve(&body.choice).map_err(bad(StatusCode::BAD_REQUEST))?;
    app.ai.remember(&p);
    let session = match body.session {
        Some(id) => app.ai.session(&p.name, id).ok_or((StatusCode::NOT_FOUND, "no such session".into()))?,
        None => app.ai.new_session(p, &user, &body.prompt, choice.clone()).map_err(bad(StatusCode::BAD_REQUEST))?,
    };
    let outcome = app.ai.turn(&session, &body.prompt, &body.context.unwrap_or_default(), choice).await.map_err(bad(StatusCode::BAD_GATEWAY))?;
    Ok(Json(json!(outcome)))
}

#[derive(Deserialize, Default)]
pub struct ApplyBody {
    #[serde(default)]
    files: Option<Vec<FileText>>,
}

#[derive(Deserialize)]
pub struct FileText {
    path: String,
    #[serde(default)]
    text: String,
    /// Delete the file instead (a proposal that removes it).
    #[serde(default)]
    delete: bool,
}

pub async fn ai_apply(State(app): State<Shared>, Path((p, id)): Path<(String, u64)>, headers: HeaderMap, body: Option<Json<ApplyBody>>) -> ApiResult<Json<Value>> {
    let pr = project(&app, &p)?;
    app.ai.remember(&pr);
    let s = app.ai.session(&p, id).ok_or((StatusCode::NOT_FOUND, "no such session".into()))?;
    let files: Vec<(String, Option<String>)> = match body.and_then(|b| b.0.files) {
        Some(files) => files.into_iter().map(|f| (f.path, (!f.delete).then_some(f.text))).collect(),
        None => app.ai.proposal(&s).into_iter().map(|f| (f.path, (!f.deleted).then_some(f.after))).collect(),
    };
    let applied = app.ai.apply(&s, files, &user(&app, &headers)).await.map_err(bad(StatusCode::CONFLICT))?;
    Ok(Json(json!({"applied": applied})))
}

pub async fn ai_discard(State(app): State<Shared>, Path((p, id)): Path<(String, u64)>) -> StatusCode {
    app.ai.discard(&p, id);
    StatusCode::NO_CONTENT
}

/// The project's conversations (newest first), without messages.
pub async fn ai_threads(State(app): State<Shared>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    app.ai.remember(&p);
    Ok(Json(json!({"threads": app.ai.threads(&p)})))
}

/// One conversation with its messages and, if still open, its proposal.
pub async fn ai_thread(State(app): State<Shared>, Path((p, id)): Path<(String, u64)>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    app.ai.remember(&p);
    Ok(Json(app.ai.thread(&p, id).ok_or((StatusCode::NOT_FOUND, "no such conversation".into()))?))
}

fn mime_of(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        _ if crate::project::is_text(path) => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// ---- the tracked upstream repository (upstream.rs) ------------------------------

pub async fn upstream_status(State(app): State<Shared>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    crate::upstream::status(&app, &p).await.map(Json).map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))
}

pub async fn upstream_pull(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    crate::upstream::sync(&app, &p, &user(&app, &headers), crate::upstream::Mode::Pull, &Default::default())
        .await
        .map(Json)
        .map_err(bad(StatusCode::BAD_GATEWAY))
}

#[derive(Deserialize, Default)]
pub struct Push {
    #[serde(default)]
    message: Option<String>,
}

pub async fn upstream_push(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap, body: Option<Json<Push>>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let message = body.map(|Json(b)| b).unwrap_or_default().message;
    crate::upstream::sync(&app, &p, &user(&app, &headers), crate::upstream::Mode::Push { message, leaving: false }, &Default::default())
        .await
        .map(Json)
        .map_err(bad(StatusCode::BAD_GATEWAY))
}

#[derive(Deserialize)]
pub struct Resolve {
    picks: std::collections::HashMap<String, crate::upstream::Pick>,
    #[serde(default)]
    push: bool,
    #[serde(default)]
    message: Option<String>,
}

/// Settles conflicts: the merge is redone with these picks; when nothing is
/// left open it is applied (and pushed, with `push`).
pub async fn upstream_resolve(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap, Json(body): Json<Resolve>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let mode = if body.push {
        crate::upstream::Mode::Push { message: body.message, leaving: false }
    } else {
        crate::upstream::Mode::Pull
    };
    crate::upstream::sync(&app, &p, &user(&app, &headers), mode, &body.picks).await.map(Json).map_err(bad(StatusCode::BAD_GATEWAY))
}

#[derive(Deserialize)]
pub struct ActivityQuery {
    #[serde(default)]
    file: Option<String>,
}

pub async fn upstream_activity(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap, Query(q): Query<ActivityQuery>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    crate::upstream::activity(&app, &p, q.file.as_deref(), &user(&app, &headers))
        .await
        .map(Json)
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))
}

#[derive(Deserialize)]
pub struct UpstreamSettings {
    push_on_leave: bool,
}

pub async fn upstream_settings(State(app): State<Shared>, Path(p): Path<String>, Json(body): Json<UpstreamSettings>) -> ApiResult<StatusCode> {
    let p = project(&app, &p)?;
    crate::upstream::set_push_on_leave(&p, body.push_on_leave).map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct TrackBranch {
    branch: String,
}

/// Track another branch of the upstream: merged in like a pull, unpushed work kept.
pub async fn upstream_branch(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap, Json(body): Json<TrackBranch>) -> ApiResult<Json<Value>> {
    manager(&app, &headers, &p)?;
    let p = project(&app, &p)?;
    crate::upstream::sync(&app, &p, &user(&app, &headers), crate::upstream::Mode::Track { branch: body.branch }, &Default::default())
        .await
        .map(Json)
        .map_err(bad(StatusCode::BAD_REQUEST))
}
