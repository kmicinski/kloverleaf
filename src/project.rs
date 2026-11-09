//! A project on disk. `projects/<name>/` is a git working tree holding the
//! sources (`main.tex`, other `.tex`/`.bib` files, figures) and a private
//! `.kloverleaf/` directory the server owns:
//!
//! ```text
//! projects/<name>/
//!   main.tex, *.tex, *.bib, figures…   the sources — every one a plain file, tracked by git
//!   .gitignore                         ignores .kloverleaf/
//!   .kloverleaf/project.json           {"main": "main.tex"}; written when the main file is chosen
//!   .kloverleaf/out/                   latexmk's output directory (pdf, log, synctex)
//!   .kloverleaf/history.sqlite         the edit log (`history.rs`)
//!   .kloverleaf/ai/<session>/          scratch copies the AI edits and compiles (`ai`)
//! ```
//!
//! This module knows the layout, which files are text (and so live documents),
//! how the main file and the engine are chosen, and nothing about editing.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const PRIVATE_DIR: &str = ".kloverleaf";
pub const OUT_DIR: &str = ".kloverleaf/out";

/// Extensions the editor opens as text and the AI may read and edit.
const TEXT_EXTENSIONS: &[&str] = &[
    "tex", "bib", "cls", "sty", "bst", "txt", "md", "tikz", "clo", "def", "cfg", "ltx", "dtx",
    "ins", "csv", "dat", "json", "toml", "yaml", "yml", "bbx", "cbx", "lbx", "svg", "gitignore",
];

#[derive(Debug, Clone, Serialize)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
    pub text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Pdflatex,
    Xelatex,
    Lualatex,
}

impl Engine {
    pub fn latexmk_flag(self) -> &'static str {
        match self {
            Engine::Pdflatex => "-pdf",
            Engine::Xelatex => "-xelatex",
            Engine::Lualatex => "-lualatex",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub main: Option<String>,
}

/// Project, file and user names double as directory names and URL segments.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// A relative path inside a project: normal components only, nothing under
/// the private directory or `.git`.
pub fn valid_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 512 || path.ends_with('/') {
        return false;
    }
    let p = Path::new(path);
    let mut first = true;
    for c in p.components() {
        match c {
            Component::Normal(seg) => {
                let seg = seg.to_string_lossy();
                if first && (seg == PRIVATE_DIR || seg == ".git") {
                    return false;
                }
                if seg.chars().any(|ch| ch.is_control() || ch == '\\') {
                    return false;
                }
            }
            _ => return false,
        }
        first = false;
    }
    true
}

pub fn is_text(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((_, ext)) => TEXT_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()),
        None => matches!(name, "Makefile" | "latexmkrc" | "README"),
    }
}

/// Every source file under `dir`, relative paths with `/`, sorted; the private
/// directory, `.git` and latexmk leftovers are skipped.
pub fn list_files(dir: &Path) -> Vec<FileInfo> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !(e.depth() == 1 && (name == PRIVATE_DIR || name == ".git")) && name != ".DS_Store"
        })
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(dir)
            .unwrap()
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        if is_build_byproduct(&rel) {
            continue;
        }
        out.push(FileInfo {
            size: entry.metadata().map(|m| m.len()).unwrap_or(0),
            text: is_text(&rel),
            path: rel,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

pub fn is_build_byproduct(rel: &str) -> bool {
    let ext = rel.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    matches!(
        ext,
        "aux" | "log" | "fls" | "fdb_latexmk" | "synctex.gz" | "out" | "toc" | "lof" | "lot"
            | "bbl" | "blg" | "bcf" | "run.xml" | "nav" | "snm" | "vrb" | "xdv"
    ) || rel.ends_with(".synctex.gz")
}

pub fn read_config(dir: &Path) -> Config {
    fs::read_to_string(dir.join(PRIVATE_DIR).join("project.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn write_config(dir: &Path, config: &Config) -> std::io::Result<()> {
    let private = dir.join(PRIVATE_DIR);
    fs::create_dir_all(&private)?;
    fs::write(
        private.join("project.json"),
        serde_json::to_string_pretty(config).unwrap(),
    )
}

/// The file latexmk is pointed at: the configured one if it exists, else
/// `main.tex`, else the first `.tex` with a `\documentclass`, else the first `.tex`.
pub fn main_file(dir: &Path, files: &[FileInfo]) -> Option<String> {
    let config = read_config(dir);
    if let Some(main) = config.main
        && files.iter().any(|f| f.path == main)
    {
        return Some(main);
    }
    if files.iter().any(|f| f.path == "main.tex") {
        return Some("main.tex".into());
    }
    let tex: Vec<&FileInfo> = files.iter().filter(|f| f.path.ends_with(".tex")).collect();
    let texts: Vec<(&str, String)> = tex
        .iter()
        .filter_map(|f| fs::read_to_string(dir.join(&f.path)).ok().map(|t| (f.path.as_str(), t)))
        .collect();
    for (_, text) in &texts {
        if let Some(root) = root_of(text)
            && files.iter().any(|f| f.path == root)
        {
            return Some(root);
        }
    }
    for (path, text) in &texts {
        if text.contains("\\documentclass") {
            return Some(path.to_string());
        }
    }
    tex.first().map(|f| f.path.clone())
}

/// `% !TEX program = xelatex` in the first lines of the main file, as TeXShop and
/// Overleaf read it; default pdflatex.
pub fn engine_of(text: &str) -> Engine {
    for line in text.lines().take(20) {
        let l = line.trim_start_matches('%').trim().to_ascii_lowercase();
        if let Some(rest) = l.strip_prefix("!tex program")
            && let Some((_, v)) = rest.split_once('=')
        {
            return match v.trim() {
                "xelatex" => Engine::Xelatex,
                "lualatex" => Engine::Lualatex,
                _ => Engine::Pdflatex,
            };
        }
    }
    Engine::Pdflatex
}

/// `% !TEX root = other.tex`, when a file is a chapter of another.
pub fn root_of(text: &str) -> Option<String> {
    for line in text.lines().take(20) {
        let l = line.trim_start_matches('%').trim();
        if l.to_ascii_lowercase().starts_with("!tex root")
            && let Some((_, v)) = l.split_once('=')
        {
            let v = v.trim().trim_start_matches("./");
            if valid_path(v) {
                return Some(v.to_string());
            }
        }
    }
    None
}

pub const STARTER: &str = r#"\documentclass[11pt]{article}
\usepackage[utf8]{inputenc}
\usepackage{amsmath,amssymb,amsthm}
\usepackage{hyperref}

\title{Untitled}
\author{}
\date{\today}

\begin{document}
\maketitle

\section{Introduction}

Start writing here. The PDF on the right follows as you type; click it to
jump back to the source.

\end{document}
"#;

pub fn out_dir(dir: &Path) -> PathBuf {
    dir.join(OUT_DIR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        assert!(valid_name("thesis-2026"));
        assert!(!valid_name(".hidden"));
        assert!(!valid_name("a/b"));
        assert!(valid_path("chapters/intro.tex"));
        assert!(!valid_path("../etc/passwd"));
        assert!(!valid_path("/abs.tex"));
        assert!(!valid_path(".kloverleaf/history.sqlite"));
        assert!(!valid_path(".git/config"));
        assert!(valid_path("figs/.gitkeep"));
    }

    #[test]
    fn text_classification() {
        assert!(is_text("main.tex"));
        assert!(is_text("refs.bib"));
        assert!(is_text(".gitignore"));
        assert!(!is_text("fig.png"));
        assert!(!is_text("paper.pdf"));
    }

    #[test]
    fn magic_comments() {
        assert_eq!(engine_of("% !TEX program = xelatex\n\\documentclass{article}"), Engine::Xelatex);
        assert_eq!(engine_of("%!TeX program=LuaLaTeX"), Engine::Lualatex);
        assert_eq!(engine_of("\\documentclass{article}"), Engine::Pdflatex);
        assert_eq!(root_of("% !TEX root = ./main.tex\n"), Some("main.tex".into()));
        assert_eq!(root_of("hello"), None);
    }
}
