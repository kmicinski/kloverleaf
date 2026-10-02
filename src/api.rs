//! JSON/text API for tools — an LLM driving a project from outside, scripts,
//! the editor's own non-socket needs. Same authority as the editor: writes go
//! through the live project, so open editors update at once.
//!
//! ```text
//! GET    /api/projects                              → [{name, main, lock}]
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
//! GET    /api/projects/{p}/lock                     → lock | null
//! POST   /api/projects/{p}/lock        ← {action}   → lock | null   (take | steal | release)
//! GET    /api/projects/{p}/commits?n                → [{sha, ts, author, message}]
//! POST   /api/projects/{p}/commit      ← {message}  → {sha} | null
//! POST   /api/projects/{p}/ai          ← {prompt, context?} → {session, text, files, model, …}  (synchronous)
//! POST   /api/projects/{p}/ai/{s}/apply ← {files?}  → {applied}
//! DELETE /api/projects/{p}/ai/{s}                   → 204
//! ```
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

fn bad(status: StatusCode) -> impl Fn(anyhow::Error) -> (StatusCode, String) {
    move |e| (status, format!("{e:#}"))
}

fn project(app: &Shared, name: &str) -> ApiResult<Project> {
    app.project(name).ok_or((StatusCode::NOT_FOUND, "no such project".into()))
}

fn user(app: &Shared, headers: &HeaderMap) -> String {
    auth::identity(app, headers).unwrap_or_else(|| "tool".into())
}

pub async fn projects(State(app): State<Shared>) -> Json<Value> {
    let list: Vec<Value> = app
        .projects
        .lock()
        .unwrap()
        .iter()
        .map(|(name, p)| json!({"name": name, "main": p.main(), "lock": p.lock()}))
        .collect();
    Json(json!(list))
}

#[derive(Deserialize)]
pub struct NewProject {
    name: String,
}

pub async fn create_project(State(app): State<Shared>, Json(body): Json<NewProject>) -> ApiResult<Json<Value>> {
    app.create(&body.name).await.map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(Json(json!({"name": body.name})))
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
    let path = p.pdf_path().filter(|p| p.is_file()).ok_or((StatusCode::NOT_FOUND, "no PDF yet".into()))?;
    let png = crate::build::render_page(&path, n, q.dpi.unwrap_or(110).clamp(30, 300))
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
    let pdf = p.pdf_path().filter(|p| p.is_file()).ok_or((StatusCode::NOT_FOUND, "no PDF yet".into()))?;
    if !crate::project::valid_path(&q.file) {
        return Err((StatusCode::BAD_REQUEST, "file".into()));
    }
    let boxes = crate::build::forward(&p.dir, &pdf, &q.file, q.line, q.col)
        .await
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?;
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
    let pdf = p.pdf_path().filter(|p| p.is_file()).ok_or((StatusCode::NOT_FOUND, "no PDF yet".into()))?;
    let pos = crate::build::inverse(&p.dir, &pdf, q.page, q.x, q.y)
        .await
        .map_err(bad(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(json!(pos)))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
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
        .list(q.file.as_deref(), q.before, q.limit.unwrap_or(60).clamp(1, 500))
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(json!(list)))
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
        Some(at) => p.history.text_at(at).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
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
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::BAD_REQUEST, "entry has no text".into()))?;
    let user = user(&app, &headers);
    if !p.take_lock(&user, false) {
        return Err((StatusCode::CONFLICT, "someone else holds the edit lock".into()));
    }
    p.replace(&entry.file, text, 0, &user, "restore", json!({"restores": id}))
        .map_err(bad(StatusCode::BAD_REQUEST))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_lock(State(app): State<Shared>, Path(p): Path<String>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(project(&app, &p)?.lock())))
}

#[derive(Deserialize)]
pub struct LockAction {
    action: String,
}

pub async fn post_lock(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap, Json(body): Json<LockAction>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let user = user(&app, &headers);
    match body.action.as_str() {
        "take" => {
            if !p.take_lock(&user, false) {
                return Err((StatusCode::CONFLICT, "held by someone else".into()));
            }
        }
        "steal" => {
            p.take_lock(&user, true);
        }
        "release" => p.release_lock(&user),
        _ => return Err((StatusCode::BAD_REQUEST, "action: take | steal | release".into())),
    }
    Ok(Json(json!(p.lock())))
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
}

pub async fn ai_run(State(app): State<Shared>, Path(p): Path<String>, headers: HeaderMap, Json(body): Json<AiBody>) -> ApiResult<Json<Value>> {
    let p = project(&app, &p)?;
    let user = user(&app, &headers);
    let session = match body.session {
        Some(id) => app.ai.session(id).ok_or((StatusCode::NOT_FOUND, "no such session".into()))?,
        None => app.ai.new_session(p, &user, &body.prompt).map_err(bad(StatusCode::BAD_REQUEST))?,
    };
    let outcome = app
        .ai
        .run(&session, &body.prompt, &body.context.unwrap_or_default())
        .await
        .map_err(bad(StatusCode::BAD_GATEWAY))?;
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
    text: String,
}

pub async fn ai_apply(State(app): State<Shared>, Path((p, id)): Path<(String, u64)>, body: Option<Json<ApplyBody>>) -> ApiResult<Json<Value>> {
    let _ = project(&app, &p)?;
    let s = app.ai.session(id).ok_or((StatusCode::NOT_FOUND, "no such session".into()))?;
    let files: Vec<(String, String)> = match body.and_then(|b| b.0.files) {
        Some(files) => files.into_iter().map(|f| (f.path, f.text)).collect(),
        None => app.ai.proposal(&s).into_iter().map(|f| (f.path, f.after)).collect(),
    };
    let applied = app.ai.apply(&s, files).await.map_err(bad(StatusCode::CONFLICT))?;
    Ok(Json(json!({"applied": applied})))
}

pub async fn ai_discard(State(app): State<Shared>, Path((_p, id)): Path<(String, u64)>) -> StatusCode {
    app.ai.end_session(id);
    StatusCode::NO_CONTENT
}

pub fn mime_of(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "tex" | "bib" | "cls" | "sty" | "bst" | "txt" | "md" => "text/plain; charset=utf-8",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}
