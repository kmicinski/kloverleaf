//! A [`Workspace`] is a scratch copy of a project (`.kloverleaf/ai/<session>/`):
//! the model reads, edits and compiles there, so nothing it does touches the
//! live document until the person applies the proposal. The tools themselves
//! — what the agent can do to a workspace — are served over the loopback MCP
//! endpoint in `mcp.rs`; this file is only the directory and its bookkeeping.

use crate::build;
use crate::project::{self, FileInfo};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Workspace {
    pub dir: PathBuf,
    /// The live project this is a copy of — only ever *read* (its git history,
    /// for `list_commits` / `restore_commit`), never written.
    pub project: PathBuf,
    pub latex_timeout: Duration,
}

impl Workspace {
    pub fn files(&self) -> Vec<FileInfo> {
        project::list_files(&self.dir)
    }

    pub fn main(&self) -> Option<String> {
        project::main_file(&self.dir, &self.files())
    }

    pub fn read(&self, path: &str) -> anyhow::Result<String> {
        anyhow::ensure!(project::valid_path(path), "invalid path");
        anyhow::ensure!(project::is_text(path), "{path} is not a text file");
        Ok(std::fs::read_to_string(self.dir.join(path))?)
    }

    pub fn write(&self, path: &str, text: &str) -> anyhow::Result<()> {
        anyhow::ensure!(project::valid_path(path) && project::is_text(path), "not a text file path");
        let full = self.dir.join(path);
        std::fs::create_dir_all(full.parent().unwrap())?;
        Ok(std::fs::write(full, text)?)
    }

    /// Removes a text file from the copy; the proposal then deletes it.
    pub fn remove(&self, path: &str) -> anyhow::Result<()> {
        anyhow::ensure!(project::valid_path(path) && project::is_text(path), "not a text file path");
        let full = self.dir.join(path);
        anyhow::ensure!(full.is_file(), "{path} does not exist");
        Ok(std::fs::remove_file(full)?)
    }

    /// Current texts of every text file, for diffing against the base.
    pub fn texts(&self) -> std::collections::BTreeMap<String, String> {
        self.files()
            .into_iter()
            .filter(|f| f.text)
            .filter_map(|f| std::fs::read_to_string(self.dir.join(&f.path)).ok().map(|t| (f.path, t)))
            .collect()
    }

    pub fn pdf(&self) -> Option<PathBuf> {
        self.main().map(|m| build::pdf_path(&self.dir, &m)).filter(|p| p.is_file())
    }
}

/// A scratch copy of a project. Texts come from memory (the live documents),
/// everything else is hard-linked (or copied) from disk; the previous compile's
/// PDF comes along so `view_pdf_page` works before the first `compile`.
pub fn make_workspace(
    dir: &Path,
    project_dir: &Path,
    texts: &std::collections::BTreeMap<String, String>,
    latex_timeout: Duration,
) -> anyhow::Result<Workspace> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir.join(project::OUT_DIR))?;
    for f in project::list_files(project_dir) {
        if f.text {
            continue;
        }
        let (src, dst) = (project_dir.join(&f.path), dir.join(&f.path));
        std::fs::create_dir_all(dst.parent().unwrap())?;
        if std::fs::hard_link(&src, &dst).is_err() {
            std::fs::copy(&src, &dst)?;
        }
    }
    for (path, text) in texts {
        let dst = dir.join(path);
        std::fs::create_dir_all(dst.parent().unwrap())?;
        std::fs::write(dst, text)?;
    }
    let private = project_dir.join(project::PRIVATE_DIR);
    if private.join("project.json").is_file() {
        std::fs::copy(private.join("project.json"), dir.join(project::PRIVATE_DIR).join("project.json"))?;
    }
    if let Ok(out) = std::fs::read_dir(private.join("out")) {
        for e in out.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".pdf") || name.ends_with(".synctex.gz") {
                let _ = std::fs::copy(e.path(), dir.join(project::OUT_DIR).join(&name));
            }
        }
    }
    Ok(Workspace { dir: dir.to_path_buf(), project: project_dir.to_path_buf(), latex_timeout })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_takes_texts_from_memory_not_disk() {
        let root = crate::history::tmp_dir("ws");
        let proj = root.join("p");
        std::fs::create_dir_all(proj.join(".kloverleaf/out")).unwrap();
        std::fs::write(proj.join("main.tex"), "on disk").unwrap();
        std::fs::write(proj.join("fig.png"), b"\x89PNG").unwrap();
        let mut texts = std::collections::BTreeMap::new();
        texts.insert("main.tex".to_string(), "in memory".to_string());
        let ws = make_workspace(&root.join("w"), &proj, &texts, Duration::from_secs(5)).unwrap();
        assert_eq!(ws.read("main.tex").unwrap(), "in memory");
        assert!(ws.dir.join("fig.png").is_file(), "binaries come along");
        assert!(ws.read("../main.tex").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
