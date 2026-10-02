//! `kloverleaf` — a hosted LaTeX editor. Sources in Monaco on the left, the
//! PDF on the right, recompiled as you type, SyncTeX both ways; every project
//! a git repository served over HTTP; every edit in a durable history; an AI
//! that can read the text and the PDF and propose changes. See README.md.
//!
//! Everything lives under `KLOVERLEAF_ROOT` (default: the working directory):
//!
//! ```text
//! projects/<name>/   one directory per project — a git working tree (`project.rs`)
//! ```
//!
//! Environment: `KLOVERLEAF_ROOT`, `KLOVERLEAF_BIND` (default `127.0.0.1:7200`),
//! `KLOVERLEAF_PASSWORD`, `KLOVERLEAF_TRUST_REMOTE_USER` (Authelia),
//! `KLOVERLEAF_DEV_USER`, `KLOVERLEAF_PUBLIC_URL`, `KLOVERLEAF_AUTOCOMMIT_SECS`
//! (default 30), `KLOVERLEAF_LATEX_TIMEOUT_SECS` (default 120), and the AI's
//! (`ai/mod.rs`): `ANTHROPIC_API_KEY`, `KLOVERLEAF_AI_MODE`, `KLOVERLEAF_AI_MODEL`,
//! `KLOVERLEAF_AI_EFFORT`, `OLLAMA_URL`, `KLOVERLEAF_LOCAL_MODEL`.

mod ai;
mod api;
mod auth;
mod build;
mod gitd;
mod history;
mod live;
mod pages;
mod project;
mod text;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post};
use axum::{Router, middleware};
use live::Project;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{env, fs};

pub struct App {
    pub root: PathBuf,
    pub password: Option<String>,
    pub trust_remote_user: bool,
    pub dev_user: String,
    pub public_url: String,
    pub sessions: Mutex<HashMap<String, String>>,
    pub projects: Mutex<BTreeMap<String, Project>>,
    pub ai: ai::Ai,
    pub latex_timeout: Duration,
}

pub type Shared = Arc<App>;

impl App {
    pub fn project(&self, name: &str) -> Option<Project> {
        self.projects.lock().unwrap().get(name).cloned()
    }

    pub async fn create(&self, name: &str) -> anyhow::Result<Project> {
        anyhow::ensure!(project::valid_name(name), "invalid project name {name:?}");
        anyhow::ensure!(self.project(name).is_none(), "project {name} exists");
        let p = Project::create(&self.root, name, self.latex_timeout).await?;
        self.projects.lock().unwrap().insert(name.to_string(), p.clone());
        Ok(p)
    }

    /// Opens every `projects/*` directory with a `.tex` file in it.
    async fn load_projects(&self) -> anyhow::Result<()> {
        let dir = self.root.join("projects");
        fs::create_dir_all(&dir)?;
        for entry in fs::read_dir(&dir)?.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !project::valid_name(&name) || !entry.path().is_dir() {
                continue;
            }
            gitd::init(&entry.path()).await?;
            let p = Project::open(&self.root, &name, self.latex_timeout)?;
            p.schedule_build();
            self.projects.lock().unwrap().insert(name, p);
        }
        Ok(())
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.into())
}

/// Every `KLOVERLEAF_AUTOCOMMIT_SECS`: flush each project and commit if anything changed.
async fn autocommit(app: Shared, every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        let projects: Vec<Project> = app.projects.lock().unwrap().values().cloned().collect();
        for p in projects {
            p.flush();
            match gitd::commit(&p.dir, "autosave", None).await {
                Ok(Some(sha)) => p.committed(&sha, "autosave", "kloverleaf").await,
                Ok(None) => {}
                Err(e) => eprintln!("project {}: autocommit: {e:#}", p.name),
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(env_or("KLOVERLEAF_ROOT", ".")).canonicalize()?;
    let bind = env_or("KLOVERLEAF_BIND", "127.0.0.1:7200");
    let latex_timeout = Duration::from_secs(env_or("KLOVERLEAF_LATEX_TIMEOUT_SECS", "120").parse().unwrap_or(120));
    let app: Shared = Arc::new(App {
        root,
        password: env::var("KLOVERLEAF_PASSWORD").ok().filter(|p| !p.is_empty()),
        trust_remote_user: matches!(env_or("KLOVERLEAF_TRUST_REMOTE_USER", "0").as_str(), "1" | "true" | "yes"),
        dev_user: env_or("KLOVERLEAF_DEV_USER", "local"),
        public_url: env_or("KLOVERLEAF_PUBLIC_URL", &format!("http://{bind}")),
        sessions: Default::default(),
        projects: Default::default(),
        ai: ai::Ai::from_env(latex_timeout),
        latex_timeout,
    });
    app.load_projects().await?;

    let gate = || middleware::from_fn_with_state(app.clone(), auth::gate);
    let api = Router::new()
        .route("/projects", get(api::projects).post(api::create_project))
        .route("/projects/{p}/files", get(api::files))
        .route("/projects/{p}/files/{*path}", get(api::get_file).put(api::put_file).delete(api::delete_file))
        .route("/projects/{p}/main", post(api::set_main))
        .route("/projects/{p}/compile", post(api::compile))
        .route("/projects/{p}/build", get(api::build))
        .route("/projects/{p}/log", get(api::log))
        .route("/projects/{p}/pdf", get(api::pdf))
        .route("/projects/{p}/page/{n}", get(api::page))
        .route("/projects/{p}/sync/forward", get(api::sync_forward))
        .route("/projects/{p}/sync/inverse", get(api::sync_inverse))
        .route("/projects/{p}/history", get(api::history))
        .route("/projects/{p}/history/{id}", get(api::history_entry))
        .route("/projects/{p}/history/{id}/restore", post(api::restore))
        .route("/projects/{p}/history/{id}/pdf", get(api::history_pdf))
        .route("/projects/{p}/lock", get(api::get_lock).post(api::post_lock))
        .route("/projects/{p}/commits", get(api::commits))
        .route("/projects/{p}/commit", post(api::commit))
        .route("/projects/{p}/ai", post(api::ai_run))
        .route("/projects/{p}/ai/{s}/apply", post(api::ai_apply))
        .route("/projects/{p}/ai/{s}", delete(api::ai_discard))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .route_layer(gate());
    let git = Router::new()
        .route("/{repo}/info/refs", get(pages::git_refs))
        .route("/{repo}/{service}", post(pages::git_rpc))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .route_layer(middleware::from_fn_with_state(app.clone(), auth::git_gate));
    let pages = Router::new()
        .route("/", get(pages::index))
        .route("/new", post(pages::create))
        .route("/edit/{name}", get(pages::editor))
        .route("/project/{name}/pdf", get(pages::pdf))
        .route("/project/{name}/ws", get(pages::ws))
        .route_layer(gate());
    let router = Router::new()
        .route("/login", get(auth::login_form).post(auth::login))
        .route("/logout", get(auth::logout))
        .route("/static/{file}", get(pages::static_file))
        .merge(pages)
        .nest("/api", api)
        .nest("/git", git)
        .with_state(app.clone());

    let every = Duration::from_secs(env_or("KLOVERLEAF_AUTOCOMMIT_SECS", "30").parse().unwrap_or(30));
    tokio::spawn(autocommit(app.clone(), every));

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!(
        "kloverleaf: {} project(s) under {}, auth: {}, ai: {} — http://{bind}/",
        app.projects.lock().unwrap().len(),
        app.root.display(),
        if app.trust_remote_user {
            "Remote-User (proxy)"
        } else if app.password.is_some() {
            "password"
        } else {
            "NONE — open to anyone who can reach this port (set KLOVERLEAF_PASSWORD)"
        },
        app.ai.describe(),
    );
    axum::serve(listener, router).await?;
    Ok(())
}
