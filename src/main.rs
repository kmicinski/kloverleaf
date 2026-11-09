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
//! (default 30), `KLOVERLEAF_LATEX_TIMEOUT_SECS` (default 120), for tracked
//! repositories (`upstream.rs`) `KLOVERLEAF_UPSTREAM_POLL_SECS` (default 60),
//! `KLOVERLEAF_GIT_SSH_KEY` and `KLOVERLEAF_GIT_IDENTITIES`, and the AI's
//! (`ai/mod.rs`): `KLOVERLEAF_AI_MODE` (`off` to disable), `KLOVERLEAF_AI_MODEL`,
//! `KLOVERLEAF_AI_EFFORT`, `KLOVERLEAF_CLAUDE_BIN`, and the `claude` CLI's own
//! `CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY` + a writable `HOME`.

mod ai;
mod api;
mod auth;
mod build;
mod gitd;
mod history;
mod live;
mod members;
mod pages;
mod project;
mod provenance;
mod text;
mod upstream;

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
    /// `KLOVERLEAF_ADMINS`: the proxy users with full access. Empty = every
    /// proxy user (the single-household default); otherwise the rest are members.
    pub admins: Vec<String>,
    pub dev_user: String,
    pub public_url: String,
    pub sessions: Mutex<HashMap<String, auth::Who>>,
    pub members: members::Members,
    pub projects: Mutex<BTreeMap<String, Project>>,
    pub ai: ai::Ai,
    pub latex_timeout: Duration,
    pub upstreams: upstream::Upstreams,
}

pub type Shared = Arc<App>;

impl App {
    pub fn project(&self, name: &str) -> Option<Project> {
        self.projects.lock().unwrap().get(name).cloned()
    }

    pub async fn create(&self, name: &str) -> anyhow::Result<Project> {
        let p = Project::create(&self.root, name, self.latex_timeout).await?;
        self.projects.lock().unwrap().insert(name.to_string(), p.clone());
        Ok(p)
    }

    /// `create`, and — for a member rather than an admin — put the new project
    /// in their list, since that list is all they can see.
    pub async fn create_for(&self, name: &str, who: &auth::Who) -> anyhow::Result<Project> {
        let p = self.create(name).await?;
        if !who.admin() {
            self.members.add_member(name, &who.user)?;
        }
        Ok(p)
    }

    /// A new project that mirrors `subdir` of a git repository (`upstream.rs`):
    /// cloned and laid out in a hidden staging directory, then moved into place,
    /// so a failed clone leaves nothing behind.
    pub async fn create_tracked_for(&self, name: &str, repo: &str, branch: Option<&str>, subdir: Option<&str>, who: &auth::Who) -> anyhow::Result<Project> {
        anyhow::ensure!(project::valid_name(name), "invalid project name {name:?}");
        let dir = self.root.join("projects").join(name);
        anyhow::ensure!(!dir.exists() && self.project(name).is_none(), "project {name} exists");
        let src = upstream::parse_source(repo)?;
        let staging = self.root.join("projects").join(format!(".import-{name}"));
        let _ = fs::remove_dir_all(&staging);
        let laid_out = async {
            let mirror = staging.join(project::PRIVATE_DIR).join("upstream.git");
            let (branch, subdir) = upstream::clone(&mirror, &src, branch, subdir).await?;
            upstream::import(&staging, &src, &branch, &subdir, "main").await
        }
        .await;
        let (cfg, n) = match laid_out {
            Ok(x) => x,
            Err(e) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(e);
            }
        };
        fs::rename(&staging, &dir)?;
        gitd::init(&dir).await?;
        let p = Project::open(&self.root, name, self.latex_timeout)?;
        let label = upstream::label(&cfg);
        p.imported(&label, serde_json::json!({"source": "git", "upstream": &cfg.base[..8], "repo": &label}));
        gitd::commit(&dir, &format!("import {label} at {} ({n} files)", &cfg.base[..8]), None).await?;
        p.schedule_build();
        self.ai.remember(&p);
        self.projects.lock().unwrap().insert(name.to_string(), p.clone());
        if !who.admin() {
            self.members.add_member(name, &who.user)?;
        }
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
            // The working tree must sit on the branch the history remembers.
            let want = p.branch();
            if gitd::current_branch(&entry.path()).await.ok().as_deref() != Some(want.as_str())
                && gitd::checkout(&entry.path(), &want, false).await.is_err()
            {
                let _ = gitd::checkout(&entry.path(), &want, true).await;
            }
            p.schedule_build();
            self.ai.remember(&p);
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
    fs::create_dir_all(root.join("projects"))?;
    let latex_timeout = Duration::from_secs(env_or("KLOVERLEAF_LATEX_TIMEOUT_SECS", "120").parse().unwrap_or(120));
    let members = members::Members::open(&root)?;
    let app: Shared = Arc::new(App {
        root,
        password: env::var("KLOVERLEAF_PASSWORD").ok().filter(|p| !p.is_empty()),
        trust_remote_user: matches!(env_or("KLOVERLEAF_TRUST_REMOTE_USER", "0").as_str(), "1" | "true" | "yes"),
        admins: env_or("KLOVERLEAF_ADMINS", "").split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
        dev_user: env_or("KLOVERLEAF_DEV_USER", "local"),
        public_url: env_or("KLOVERLEAF_PUBLIC_URL", &format!("http://{bind}")),
        sessions: Default::default(),
        members,
        projects: Default::default(),
        ai: ai::Ai::from_env(latex_timeout, &bind),
        latex_timeout,
        upstreams: Default::default(),
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
        .route("/projects/{p}/history/summary", get(api::history_summary))
        .route("/projects/{p}/history/tree", get(api::history_tree))
        .route("/projects/{p}/branches", get(api::branches).post(api::create_branch))
        .route("/projects/{p}/branches/{name}/switch", post(api::switch_branch))
        .route("/projects/{p}/provenance", get(api::provenance))
        .route("/projects/{p}/provenance/origins", get(api::origins))
        .route("/projects/{p}/provenance/claim", post(api::claim))
        .route("/projects/{p}/provenance/rebuild", post(api::rebuild_provenance))
        .route("/projects/{p}/history/{id}", get(api::history_entry))
        .route("/projects/{p}/history/{id}/restore", post(api::restore))
        .route("/projects/{p}/history/{id}/pdf", get(api::history_pdf))
        .route("/projects/{p}/lock", get(api::get_lock).post(api::post_lock))
        .route("/projects/{p}/commits", get(api::commits))
        .route("/projects/{p}/commit", post(api::commit))
        .route("/projects/{p}/ai", post(api::ai_run))
        .route("/projects/{p}/ai/threads", get(api::ai_threads))
        .route("/projects/{p}/ai/threads/{s}", get(api::ai_thread))
        .route("/projects/{p}/invites", get(api::invites).post(api::create_invite))
        .route("/projects/{p}/invites/{token}", delete(api::revoke_invite))
        .route("/projects/{p}/members", post(api::add_member))
        .route("/projects/{p}/members/{user}", delete(api::remove_member))
        .route("/me/password", post(api::set_password))
        .route("/projects/{p}/upstream", get(api::upstream_status))
        .route("/projects/{p}/upstream/pull", post(api::upstream_pull))
        .route("/projects/{p}/upstream/push", post(api::upstream_push))
        .route("/projects/{p}/upstream/resolve", post(api::upstream_resolve))
        .route("/projects/{p}/upstream/activity", get(api::upstream_activity))
        .route("/projects/{p}/upstream/settings", post(api::upstream_settings))
        .route("/projects/{p}/upstream/branch", post(api::upstream_branch))
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
        .route("/me/password", post(pages::set_password))
        .route("/edit/{name}", get(pages::editor))
        .route("/project/{name}/pdf", get(pages::pdf))
        .route("/project/{name}/ws", get(pages::ws))
        .route_layer(gate());
    let router = Router::new()
        .route("/login", get(auth::login_form).post(auth::login))
        .route("/logout", get(auth::logout))
        .route("/invite/{token}", get(auth::invite_form).post(auth::invite_claim))
        .route("/static/{file}", get(pages::static_file))
        // The agent's loopback MCP endpoint: its own bearer token is the gate.
        .route("/mcp", post(ai::mcp::handler))
        .merge(pages)
        .nest("/api", api)
        .nest("/git", git)
        .with_state(app.clone());

    let every = Duration::from_secs(env_or("KLOVERLEAF_AUTOCOMMIT_SECS", "30").parse().unwrap_or(30));
    tokio::spawn(autocommit(app.clone(), every));
    let poll = Duration::from_secs(env_or("KLOVERLEAF_UPSTREAM_POLL_SECS", "60").parse().unwrap_or(60).max(10));
    tokio::spawn(upstream::poll(app.clone(), poll));

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
