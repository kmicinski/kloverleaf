//! What the model can do, and where. A [`Workspace`] is a scratch copy of a
//! project (`.kloverleaf/ai/<session>/`): the model reads, edits and compiles
//! there, so nothing it does touches the live document until the person
//! applies the proposal. Tools are plain functions over that directory.
//!
//! [`Scope::Full`] is every tool. [`Scope::Metadata`] is the privileged side
//! of dual mode: it may learn file names, sizes and where errors are, but
//! never a line of text or a page image.

use crate::build;
use crate::project::{self, FileInfo};
use crate::text;
use super::provider::Tool;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use base64::Engine;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    Full,
    Metadata,
}

pub struct Workspace {
    pub dir: PathBuf,
    pub latex_timeout: Duration,
    pub compiled: Mutex<Option<Vec<build::Diagnostic>>>,
}

pub struct Output {
    pub content: Vec<Value>,
    pub is_error: bool,
    /// One line for the activity log in the UI.
    pub summary: String,
}

fn text_out(s: impl Into<String>, summary: impl Into<String>) -> Output {
    Output { content: vec![json!({"type": "text", "text": s.into()})], is_error: false, summary: summary.into() }
}

fn err_out(s: impl Into<String>) -> Output {
    let s = s.into();
    Output { content: vec![json!({"type": "text", "text": s.clone()})], is_error: true, summary: format!("error: {s}") }
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

fn schema(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": props, "required": required, "additionalProperties": false})
}

pub fn definitions(scope: Scope, network: bool) -> Vec<Tool> {
    let mut v = vec![
        Tool {
            name: "list_files",
            description: "List every file in the project with its size; the main .tex file is marked. Call this first if you do not know the layout.".into(),
            input_schema: schema(json!({}), &[]),
        },
        Tool {
            name: "compile",
            description: match scope {
                Scope::Full => "Run latexmk on the project and return whether it succeeded and the diagnostics (file, line, severity, message). Do this after editing to check your work.",
                Scope::Metadata => "Run latexmk on the project and return whether it succeeded, with the count of errors and warnings and their file:line locations (not their text).",
            }.into(),
            input_schema: schema(json!({}), &[]),
        },
    ];
    if scope == Scope::Full {
        v.extend([
            Tool {
                name: "read_file",
                description: "Read a text file (LaTeX, BibTeX, …) with line numbers. Give `from`/`to` (1-based, inclusive) to read a range; without them a long file returns its first 400 lines.".into(),
                input_schema: schema(json!({
                    "path": {"type": "string", "description": "Path relative to the project root, e.g. chapters/intro.tex"},
                    "from": {"type": "integer", "minimum": 1},
                    "to": {"type": "integer", "minimum": 1}
                }), &["path"]),
            },
            Tool {
                name: "search_files",
                description: "Search every text file for a pattern (a regular expression unless `literal` is true). Returns up to 100 matching lines as path:line: text.".into(),
                input_schema: schema(json!({
                    "pattern": {"type": "string"},
                    "literal": {"type": "boolean", "description": "Match the pattern as plain text"}
                }), &["pattern"]),
            },
            Tool {
                name: "write_edits",
                description: "Edit a text file by exact replacement. Each edit's `old` must occur exactly once in the file (include enough surrounding lines to make it unique) and is replaced by `new`. To create a file, or replace one wholesale, give one edit with `old` set to \"\" — then `new` becomes the whole content. Edits apply in order; the call fails as a whole if any `old` is missing or ambiguous, and nothing is written.".into(),
                input_schema: schema(json!({
                    "path": {"type": "string"},
                    "edits": {"type": "array", "items": schema(json!({"old": {"type": "string"}, "new": {"type": "string"}}), &["old", "new"]), "minItems": 1}
                }), &["path", "edits"]),
            },
            Tool {
                name: "view_pdf_page",
                description: "Look at one page of the compiled PDF as an image, to check layout, figures, tables or math as the reader sees them. Pages are 1-based. Compile first if you changed something.".into(),
                input_schema: schema(json!({"page": {"type": "integer", "minimum": 1}}), &["page"]),
            },
            Tool {
                name: "pdf_text",
                description: "The text of the compiled PDF (one page, or all pages without `page`), as extracted by pdftotext — what actually rendered, with hyphenation and numbering resolved.".into(),
                input_schema: schema(json!({"page": {"type": "integer", "minimum": 1}}), &[]),
            },
        ]);
    }
    if network {
        v.push(Tool {
            name: "fetch_url",
            description: "Fetch a web page or text resource and return its text (HTML tags stripped, truncated to 30k characters). Use it to read a paper, a package manual, or a citation page.".into(),
            input_schema: schema(json!({"url": {"type": "string"}}), &["url"]),
        });
    }
    v
}

pub async fn execute(ws: &Workspace, scope: Scope, name: &str, input: &Value, http: &reqwest::Client) -> Output {
    let s = |k: &str| input[k].as_str().map(str::to_string);
    let n = |k: &str| input[k].as_u64().map(|v| v as usize);
    match (name, scope) {
        ("list_files", _) => {
            let main = ws.main();
            let lines: Vec<String> = ws
                .files()
                .iter()
                .map(|f| {
                    format!(
                        "{}  ({} bytes{}{})",
                        f.path,
                        f.size,
                        if f.text { "" } else { ", binary" },
                        if main.as_deref() == Some(&f.path) { ", main" } else { "" }
                    )
                })
                .collect();
            text_out(lines.join("\n"), format!("listed {} files", lines.len()))
        }
        ("compile", _) => {
            let Some(main) = ws.main() else { return err_out("no main .tex file") };
            match build::compile(&ws.dir, &main, ws.latex_timeout).await {
                Ok(c) => {
                    let errors = c.diagnostics.iter().filter(|d| d.severity == build::Severity::Error).count();
                    let warnings = c.diagnostics.iter().filter(|d| d.severity == build::Severity::Warning).count();
                    *ws.compiled.lock().unwrap() = Some(c.diagnostics.clone());
                    let summary = format!("compiled in {}ms: {} error(s), {} warning(s)", c.ms, errors, warnings);
                    let body = match scope {
                        Scope::Full => json!({"ok": c.ok, "ms": c.ms, "pdf": c.pdf.is_some(), "diagnostics": c.diagnostics}),
                        Scope::Metadata => json!({
                            "ok": c.ok, "ms": c.ms, "pdf": c.pdf.is_some(), "errors": errors, "warnings": warnings,
                            "locations": c.diagnostics.iter().filter(|d| d.severity != build::Severity::Info)
                                .map(|d| json!({"file": d.file, "line": d.line, "severity": d.severity})).collect::<Vec<_>>(),
                        }),
                    };
                    Output { content: vec![json!({"type": "text", "text": body.to_string()})], is_error: false, summary }
                }
                Err(e) => err_out(format!("could not compile: {e:#}")),
            }
        }
        ("read_file", Scope::Full) => {
            let Some(path) = s("path") else { return err_out("path is required") };
            match ws.read(&path) {
                Ok(text) => {
                    let total = text.lines().count();
                    let (from, to) = match (n("from"), n("to")) {
                        (None, None) if total > 400 => (1, 400),
                        (f, t) => (f.unwrap_or(1), t.unwrap_or(total)),
                    };
                    let mut out = text::numbered(&text, from, to);
                    if to < total || from > 1 {
                        out.push_str(&format!("[lines {from}-{} of {total}]\n", to.min(total)));
                    }
                    text_out(out, format!("read {path}:{from}-{}", to.min(total)))
                }
                Err(e) => err_out(format!("{e:#}")),
            }
        }
        ("search_files", Scope::Full) => {
            let Some(pattern) = s("pattern") else { return err_out("pattern is required") };
            let re = if input["literal"].as_bool().unwrap_or(false) {
                regex::Regex::new(&regex::escape(&pattern))
            } else {
                regex::Regex::new(&pattern)
            };
            let re = match re {
                Ok(r) => r,
                Err(e) => return err_out(format!("bad pattern: {e}")),
            };
            let mut hits = Vec::new();
            for (path, text) in ws.texts() {
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        hits.push(format!("{path}:{}: {}", i + 1, line.trim_end()));
                        if hits.len() >= 100 {
                            break;
                        }
                    }
                }
            }
            let n = hits.len();
            text_out(if hits.is_empty() { "no matches".to_string() } else { hits.join("\n") }, format!("searched: {n} hit(s)"))
        }
        ("write_edits", Scope::Full) => {
            let Some(path) = s("path") else { return err_out("path is required") };
            let Some(edits) = input["edits"].as_array() else { return err_out("edits is required") };
            if !project::valid_path(&path) || !project::is_text(&path) {
                return err_out(format!("{path} is not a text file path"));
            }
            let mut text = ws.read(&path).unwrap_or_default();
            for (i, e) in edits.iter().enumerate() {
                let (old, new) = (e["old"].as_str().unwrap_or(""), e["new"].as_str().unwrap_or(""));
                if old.is_empty() {
                    text = new.to_string();
                    continue;
                }
                match text.matches(old).count() {
                    1 => text = text.replacen(old, new, 1),
                    0 => return err_out(format!("edit {}: `old` not found in {path}; read the file again and copy the text exactly", i + 1)),
                    k => return err_out(format!("edit {}: `old` occurs {k} times in {path}; include more context to make it unique", i + 1)),
                }
            }
            match ws.write(&path, &text) {
                Ok(()) => text_out(
                    format!("wrote {path} ({} lines)", text.lines().count()),
                    format!("edited {path} ({} edit(s))", edits.len()),
                ),
                Err(e) => err_out(format!("{e:#}")),
            }
        }
        ("view_pdf_page", Scope::Full) => {
            let Some(pdf) = ws.pdf() else { return err_out("no PDF yet: compile first") };
            let page = n("page").unwrap_or(1) as u32;
            let pages = build::page_count(&pdf).await;
            if page == 0 || page > pages {
                return err_out(format!("page {page} does not exist; the PDF has {pages} page(s)"));
            }
            match build::render_page(&pdf, page, 110).await {
                Ok(png) => Output {
                    content: vec![
                        json!({"type": "text", "text": format!("page {page} of {pages}")}),
                        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png",
                               "data": base64::engine::general_purpose::STANDARD.encode(&png)}}),
                    ],
                    is_error: false,
                    summary: format!("viewed page {page}/{pages}"),
                },
                Err(e) => err_out(format!("{e:#}")),
            }
        }
        ("pdf_text", Scope::Full) => {
            let Some(pdf) = ws.pdf() else { return err_out("no PDF yet: compile first") };
            let page = n("page").map(|p| p as u32);
            match build::page_text(&pdf, page).await {
                Ok(t) => {
                    let t: String = t.chars().take(60_000).collect();
                    text_out(t, match page { Some(p) => format!("read PDF text of page {p}"), None => "read PDF text".into() })
                }
                Err(e) => err_out(format!("{e:#}")),
            }
        }
        ("fetch_url", _) => {
            let Some(url) = s("url") else { return err_out("url is required") };
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return err_out("only http(s) URLs");
            }
            match fetch(http, &url).await {
                Ok(t) => text_out(t, format!("fetched {url}")),
                Err(e) => err_out(format!("fetch failed: {e:#}")),
            }
        }
        (other, _) => err_out(format!("unknown tool {other}")),
    }
}

async fn fetch(http: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    let resp = http
        .get(url)
        .header("user-agent", "kloverleaf/0.1 (+https://github.com)")
        .timeout(Duration::from_secs(25))
        .send()
        .await?;
    anyhow::ensure!(resp.status().is_success(), "HTTP {}", resp.status());
    let html = resp.content_length().is_none_or(|l| l < 4_000_000);
    anyhow::ensure!(html, "response too large");
    let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body = resp.text().await?;
    let text = if ctype.contains("html") { strip_html(&body) } else { body };
    Ok(text.chars().take(30_000).collect())
}

/// Crude but serviceable: drop scripts and styles, then tags, then collapse whitespace.
pub fn strip_html(html: &str) -> String {
    let re_block = regex::Regex::new(r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>|<noscript[^>]*>.*?</noscript>|<svg[^>]*>.*?</svg>").unwrap();
    let re_tag = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    let re_ws = regex::Regex::new(r"[ \t\r\f]+").unwrap();
    let re_nl = regex::Regex::new(r"\n{3,}").unwrap();
    let s = re_block.replace_all(html, "");
    let s = s.replace("</p>", "\n").replace("<br>", "\n").replace("</div>", "\n").replace("</li>", "\n");
    let s = re_tag.replace_all(&s, " ");
    let s = s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&nbsp;", " ");
    let s = re_ws.replace_all(&s, " ");
    let s: String = s.lines().map(str::trim).collect::<Vec<_>>().join("\n");
    re_nl.replace_all(&s, "\n\n").trim().to_string()
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
    Ok(Workspace { dir: dir.to_path_buf(), latex_timeout, compiled: Mutex::new(None) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> Workspace {
        let dir = crate::history::tmp_dir("ws");
        std::fs::write(dir.join("main.tex"), "\\documentclass{article}\n\\begin{document}\nHello\nHello\n\\end{document}\n").unwrap();
        Workspace { dir, latex_timeout: Duration::from_secs(30), compiled: Mutex::new(None) }
    }

    #[tokio::test]
    async fn edits_require_unique_matches() {
        let w = ws();
        let http = reqwest::Client::new();
        let amb = execute(&w, Scope::Full, "write_edits", &json!({"path": "main.tex", "edits": [{"old": "Hello", "new": "Bye"}]}), &http).await;
        assert!(amb.is_error && amb.summary.contains("2 times"));
        let ok = execute(&w, Scope::Full, "write_edits", &json!({"path": "main.tex", "edits": [{"old": "Hello\nHello", "new": "Bye"}]}), &http).await;
        assert!(!ok.is_error, "{}", ok.summary);
        assert!(w.read("main.tex").unwrap().contains("Bye\n\\end"));
        let new = execute(&w, Scope::Full, "write_edits", &json!({"path": "refs.bib", "edits": [{"old": "", "new": "@book{k, title={T}}"}]}), &http).await;
        assert!(!new.is_error);
        assert_eq!(w.read("refs.bib").unwrap(), "@book{k, title={T}}");
        let bad = execute(&w, Scope::Full, "write_edits", &json!({"path": "../x.tex", "edits": [{"old": "", "new": ""}]}), &http).await;
        assert!(bad.is_error);
        let meta = execute(&w, Scope::Metadata, "read_file", &json!({"path": "main.tex"}), &http).await;
        assert!(meta.is_error, "metadata scope cannot read");
        let search = execute(&w, Scope::Full, "search_files", &json!({"pattern": "doc.*class"}), &http).await;
        assert!(search.summary.contains("1 hit"));
        std::fs::remove_dir_all(&w.dir).unwrap();
    }

    #[test]
    fn html_stripping() {
        let t = strip_html("<html><head><style>x{}</style><script>var a=1;</script></head><body><h1>Hi</h1><p>there &amp; you</p></body></html>");
        assert_eq!(t, "Hi there & you");
    }
}
