//! Compiling: latexmk in the project's private output directory, the log
//! parsed into diagnostics with file and line, SyncTeX both ways, and page
//! images for the AI. Everything here is a function over a directory, so the
//! same code compiles a project and an AI session's scratch copy.
//!
//! The per-project build loop (debounce, flush, broadcast) is in `live.rs`;
//! this module does not know about documents or sockets.

use crate::project;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: String,
    pub line: Option<usize>,
    pub severity: Severity,
    pub message: String,
    /// 1-based UTF-16 columns (Monaco's) of the culprit on `line`, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<usize>,
    /// What this usually means and how to fix it, in plain words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// TeX's own excerpt: the source up to the point it stopped (`l.12 …`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Parse-time scratch: a runaway argument's text (to find where it
    /// started), TeX's excerpt of the line up to the error, and the first
    /// context line (which ends with an undefined macro's name).
    #[serde(skip)]
    runaway: Option<String>,
    #[serde(skip)]
    before: Option<String>,
    #[serde(skip)]
    top: Option<String>,
}

impl Diagnostic {
    pub fn new(file: impl Into<String>, line: Option<usize>, severity: Severity, message: impl Into<String>) -> Self {
        Diagnostic { file: file.into(), line, severity, message: message.into(), ..Default::default() }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Error,
    Warning,
    Info,
}

/// What one compile produced. `pdf` is `None` when no PDF came out at all;
/// with `nonstopmode` a document with errors usually still produces one.
#[derive(Debug)]
pub struct Compiled {
    pub ok: bool,
    /// TeX stopped before writing a PDF: `pdf` (if any) is an earlier build's.
    pub stale: bool,
    pub diagnostics: Vec<Diagnostic>,
    pub pdf: Option<Vec<u8>>,
    pub log: String,
    pub ms: u64,
}

/// Runs latexmk on `main` inside `dir`, writing to `.kloverleaf/out`.
pub async fn compile(dir: &Path, main: &str, timeout: Duration) -> anyhow::Result<Compiled> {
    let start = Instant::now();
    let out = project::out_dir(dir);
    std::fs::create_dir_all(&out)?;
    let main_text = std::fs::read_to_string(dir.join(main)).unwrap_or_default();
    let engine = project::engine_of(&main_text);
    let mut cmd = Command::new("latexmk");
    cmd.current_dir(dir)
        .arg(engine.latexmk_flag())
        // -f: keep going past TeX errors so bibtex/biber and the rerun passes still
        // happen — otherwise one typo anywhere leaves every \ref as ?? and every
        // \cite as [?] until the document is error-free (drafts never are).
        .arg("-f")
        .arg("-interaction=nonstopmode")
        .arg("-file-line-error")
        .arg("-synctex=1")
        .arg(format!("-outdir={}", project::OUT_DIR))
        .arg(main)
        .env("max_print_line", "10000")
        .env("error_line", "254")
        .env("half_error_line", "238")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(o) => o?,
        Err(_) => {
            return Ok(Compiled {
                ok: false,
                diagnostics: vec![Diagnostic::new(main, None, Severity::Error, format!("latexmk timed out after {}s", timeout.as_secs()))],
                stale: true,
                pdf: None,
                log: String::new(),
                ms: start.elapsed().as_millis() as u64,
            });
        }
    };
    let stem = Path::new(main)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "main".into());
    let log = read_lossy(&out.join(format!("{stem}.log")));
    let mut diagnostics = parse_log(&log, main);
    let blg = read_lossy(&out.join(format!("{stem}.blg")));
    diagnostics.extend(parse_blg(&blg));
    refine(&mut diagnostics, main, |f| std::fs::read_to_string(dir.join(f)).ok(), &tex_files(dir));
    let stale = log.contains("no output PDF file produced") || log.contains("No pages of output");
    if !output.status.success() && diagnostics.iter().all(|d| d.severity != Severity::Error) {
        // latexmk failed without a TeX error in the log (missing file, bibtex, …): surface its own output.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let tail: String = stderr
            .lines()
            .chain(stdout.lines())
            .filter(|l| l.contains("rror") || l.contains("not found") || l.contains("Failed"))
            .take(5)
            .collect::<Vec<_>>()
            .join("\n");
        diagnostics.push(Diagnostic::new(
            main,
            None,
            Severity::Error,
            if tail.is_empty() { format!("latexmk exited with {}", output.status) } else { tail },
        ));
    }
    // A label that recorded an empty number renders as "Figure " — usually a
    // \label placed after a caption that sat inside extra braces. TeX says
    // nothing about it, so look at the .aux ourselves.
    let aux = read_lossy(&out.join(format!("{stem}.aux")));
    for label in empty_labels(&aux) {
        let (file, line) = find_label(dir, main, &label);
        diagnostics.push(Diagnostic::new(
            file,
            line,
            Severity::Warning,
            format!("\\label{{{label}}} recorded an empty number: \\ref will print nothing. It probably follows a \\caption that is wrapped in extra braces — remove them, or put the \\label inside the caption."),
        ));
    }
    let pdf = std::fs::read(out.join(format!("{stem}.pdf"))).ok();
    Ok(Compiled {
        ok: output.status.success(),
        stale,
        diagnostics,
        pdf,
        log,
        ms: start.elapsed().as_millis() as u64,
    })
}

/// TeX writes 8-bit bytes into its log and aux files as it reads them (a
/// curly quote in pdflatex input arrives as raw UTF-8 bytes, cut wherever
/// a line wraps), so they are not always valid UTF-8 — and a strict read
/// returned nothing, which silently dropped every error of such a project.
fn read_lossy(path: &Path) -> String {
    std::fs::read(path).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default()
}

pub fn pdf_path(dir: &Path, main: &str) -> PathBuf {
    let stem = Path::new(main).file_stem().unwrap_or_default().to_string_lossy();
    project::out_dir(dir).join(format!("{stem}.pdf"))
}

// ---- log parsing ---------------------------------------------------------------

/// Turns a LaTeX log into diagnostics. Errors come from `-file-line-error`
/// lines (`./file.tex:12: message`) and from `! message`; after each, TeX's
/// context is read up to the `l.12 …` line, which says how far into the line
/// TeX got (that is where the culprit is) and names an undefined macro.
/// Warnings come from `LaTeX Warning:` / `Package x Warning:` with `on input
/// line N`; overfull boxes become infos. The current file is tracked through
/// TeX's parentheses so warnings land in the right file. `tidy` then drops
/// what is noise to an author and attaches a plain-words hint; `refine`
/// (which can read the project) moves errors to where the fix goes.
pub fn parse_log(log: &str, main: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut stack: Vec<String> = vec![main.to_string()];
    let lines: Vec<&str> = log.lines().collect();
    let mut i = 0;
    let mut pending_warning: Option<(Diagnostic, String)> = None;
    let mut runaway: Option<String> = None;
    let fle = regex::Regex::new(r"^(\./)?([^:\s()]+\.[A-Za-z]+):(\d+): (.*)$").unwrap();
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        if let Some((d, pkg)) = pending_warning.as_mut() {
            let cont = line.strip_prefix(&format!("({pkg})")).map(str::trim);
            match cont {
                Some(more) if !more.is_empty() => {
                    d.message.push(' ');
                    d.message.push_str(more);
                    if let Some(n) = input_line(more) {
                        d.line = Some(n);
                    }
                    continue;
                }
                _ => {
                    let (d, _) = pending_warning.take().unwrap();
                    out.push(d);
                }
            }
        }
        let current = stack.last().cloned().unwrap_or_else(|| main.to_string());

        let error = if let Some(c) = fle.captures(line) {
            Some(Diagnostic::new(normalize(&c[2]), c[3].parse().ok(), Severity::Error, c[4].trim()))
        } else {
            line.strip_prefix("! ").map(|msg| Diagnostic::new(current.clone(), None, Severity::Error, msg.trim()))
        };
        if let Some(mut d) = error {
            d.runaway = runaway.take();
            // The context is source text: its parentheses must not reach track_files.
            i = read_error_context(&lines, i, &mut d, &fle);
            out.push(d);
            continue;
        }
        if line.starts_with("Runaway ") && line.ends_with('?') {
            runaway = lines.get(i).map(|s| s.trim().to_string());
            i += 1;
            continue;
        }
        track_files(line, &mut stack);

        if let Some(msg) = line.strip_prefix("LaTeX Warning: ") {
            out.push(Diagnostic::new(current, input_line(msg), Severity::Warning, msg.trim()));
        } else if let Some((pkg, msg)) = package_warning(line) {
            pending_warning = Some((Diagnostic::new(current, input_line(&msg), Severity::Warning, msg), pkg));
        } else if line.starts_with("Overfull \\hbox") || line.starts_with("Underfull \\hbox") {
            let at = line
                .split("at lines ")
                .nth(1)
                .and_then(|s| s.split(['-', ' ']).next())
                .and_then(|n| n.parse().ok())
                .or_else(|| line.split("at line ").nth(1).and_then(|s| s.trim().parse().ok()));
            out.push(Diagnostic::new(current, at, Severity::Info, line.trim()));
        }
    }
    if let Some((d, _)) = pending_warning {
        out.push(d);
    }
    tidy(out)
}

/// Reads what TeX prints after an error, through the `l.N …` line and its
/// continuation (stopping early at the next error): the first context line is
/// where TeX was — for an undefined macro it ends with the macro — and the
/// `l.N` line is the source up to the point TeX stopped. Returns the index of
/// the first line not consumed.
fn read_error_context(lines: &[&str], mut i: usize, d: &mut Diagnostic, fle: &regex::Regex) -> usize {
    let start = i;
    let mut shown: Vec<&str> = Vec::new();
    while i < lines.len() && i < start + 40 {
        let l = lines[i];
        if l.starts_with("! ") || fle.is_match(l) || l.starts_with("Here is how much of TeX's memory") {
            break;
        }
        i += 1;
        let t = l.trim_end();
        let boiler = t.is_empty()
            || t.starts_with("See the LaTeX manual")
            || t.starts_with("Type  H <return>")
            || t.starts_with("Type X to quit")
            || t.starts_with("or enter new name")
            || t.starts_with("Enter file name");
        if d.top.is_none() && !boiler && !t.starts_with("<inserted text>") && !t.starts_with("<to be read again>") {
            d.top = Some(t.trim_start().to_string());
        }
        if !boiler && shown.len() < 6 {
            shown.push(t);
        }
        if let Some(n) = error_line(t) {
            if d.line.is_none() {
                d.line = Some(n);
            }
            d.before = Some(t.split_once(' ').map(|(_, b)| b.to_string()).unwrap_or_default());
            // The continuation (indented to where the l.N line ended) is the rest of the source line.
            if let Some(next) = lines.get(i).filter(|n| n.starts_with(' ') || n.is_empty()) {
                if !next.trim().is_empty() {
                    shown.push(next.trim_end());
                }
                i += 1;
            }
            break;
        }
    }
    if let Some(m) = d.message.strip_prefix("LaTeX Error: ") {
        d.message = m.to_string();
    }
    if !shown.is_empty() {
        d.context = Some(shown.join("\n"));
    }
    i
}

/// Drops what is noise to an author, names undefined macros, and adds hints.
/// `Emergency stop` follows the error that caused it (which may lack a line,
/// e.g. a missing `\input` file): its line moves there and it goes. Errors
/// TeX reports at `\end{document}` after an earlier error are that error's
/// echo (an unclosed `$` or `{` being closed at the end). Summary warnings
/// ("There were undefined citations", "… Rerun to get …") repeat the
/// individual ones, and latexmk reruns by itself.
fn tidy(all: Vec<Diagnostic>) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = Vec::new();
    for mut d in all {
        let is_error = d.severity == Severity::Error;
        if is_error && d.message.starts_with("==> Fatal error occurred") {
            continue;
        }
        if is_error && d.message == "Emergency stop." {
            if let Some(prev) = out.iter_mut().rev().find(|p| p.severity == Severity::Error) {
                if prev.line.is_none() && prev.file == d.file {
                    prev.line = d.line;
                    prev.before = d.before.take();
                    if prev.context.is_none() {
                        prev.context = d.context.take();
                    }
                }
                continue;
            }
            let no_end = d.context.as_deref().is_some_and(|c| c.contains("no legal \\end found"));
            d.message = if no_end { "The document ended without \\end{document}.".into() } else { "TeX had to stop here.".into() };
        }
        if d.severity == Severity::Warning
            && (d.message.starts_with("There were ")
                || d.message.contains("Rerun to get")
                || d.message.starts_with("Label(s) may have changed")
                || d.message.starts_with("Some images may lack"))
        {
            continue;
        }
        if is_error
            && d.before.as_deref().map(str::trim) == Some("\\end{document}")
            && !d.message.contains("ended by")
            && out.iter().any(|p| p.severity == Severity::Error)
        {
            continue;
        }
        if out.iter().any(|p| p.file == d.file && p.line == d.line && p.message == d.message) {
            continue;
        }
        // `\begin{document} ended by \end{theorem}` after `Environment theorem undefined`: the same mistake.
        if let Some(end) = d.message.split(" ended by \\end{").nth(1).and_then(|r| r.split('}').next())
            && out.iter().any(|p| p.message == format!("Environment {end} undefined."))
        {
            continue;
        }
        if d.message.starts_with("Undefined control sequence")
            && let Some(name) = d.top.as_deref().and_then(trailing_macro)
        {
            d.message = format!("Undefined control sequence {name}");
        }
        d.hint = hint(&d);
        out.push(d);
    }
    out
}

/// `\fooo` from `l.2 We use \fooo` or `\x ->\fooo`.
fn trailing_macro(s: &str) -> Option<String> {
    let re = regex::Regex::new(r"(\\[A-Za-z@]+\*?|\\.)\s*$").unwrap();
    re.captures(s).map(|c| c[1].to_string())
}

/// What the common TeX errors usually mean, in an author's words.
fn hint(d: &Diagnostic) -> Option<String> {
    let m = d.message.as_str();
    let cap = |re: &str| regex::Regex::new(re).unwrap().captures(m).map(|c| c[1].to_string());
    let s = if m.starts_with("Undefined control sequence") {
        let name = m.strip_prefix("Undefined control sequence").unwrap().trim();
        let name = if name.is_empty() { "This command" } else { name };
        format!("{name} is not defined: check the spelling, or add the \\usepackage or \\newcommand that provides it.")
    } else if m.starts_with("Missing $ inserted") {
        "Something that only works in math (_ ^ \\alpha …) is in ordinary text, or a $ is missing. Wrap the math in $…$, or write \\_ for a literal underscore.".into()
    } else if m.starts_with("File ended while scanning use of") || m.starts_with("Paragraph ended before") {
        "An argument was never closed: a { is missing its }.".into()
    } else if m.starts_with("Missing } inserted") || m.starts_with("Too many }'s") || m.starts_with("Extra }, or forgotten") {
        "The braces don't balance near here: look for an unclosed { or a stray }.".into()
    } else if m.starts_with("Missing { inserted") {
        "A command expected a {…} argument here, or math is unbalanced.".into()
    } else if let Some(env) = cap(r"Environment (\S+) undefined") {
        format!("Nothing defines the {env} environment: load its package, or define it (e.g. \\newtheorem{{{env}}}{{…}}).")
    } else if m.contains(" ended by \\end{") {
        "Every \\begin{…} needs a matching \\end{…}, in the right order.".into()
    } else if let Some(f) = cap(r"File `([^']+)' not found") {
        format!("No file {f}: check the name and the folder, or upload it.")
    } else if m.starts_with("Misplaced alignment tab character &") {
        "& only separates columns in tables and aligned math; write \\& for an ampersand.".into()
    } else if m.starts_with("Extra alignment tab") {
        "This row has more & columns than the table's column spec.".into()
    } else if m.starts_with("Double superscript") || m.starts_with("Double subscript") {
        "Group it: x^{a^b} or {x^a}^b.".into()
    } else if m.starts_with("Illegal parameter number") || m.contains("macro parameter character #") {
        "# is special in LaTeX; write \\# for the symbol.".into()
    } else if let Some(c) = cap(r"Command (\\\S+) already defined") {
        format!("{c} exists already: use \\renewcommand, or pick another name.")
    } else if m.starts_with("Can be used only in preamble") {
        "Move this before \\begin{document}.".into()
    } else if m.starts_with("Missing \\begin{document}") {
        "There is text (or a stray character) in the preamble, before \\begin{document}.".into()
    } else if m.starts_with("Not in outer par mode") {
        "A float (figure/table) or marginpar is inside a box or another float.".into()
    } else if m.starts_with("Too many unprocessed floats") {
        "Too many figures/tables are waiting for a place: add \\clearpage, or loosen their [placement].".into()
    } else if let Some(p) = cap(r"Option clash for package (\S+)") {
        format!("{p} is loaded twice with different options: load it once, with all of them.")
    } else if m.starts_with("Display math should end with $$") || m.starts_with("Bad math environment delimiter") {
        "Math delimiters don't pair up: \\[ with \\], \\( with \\), $$ with $$.".into()
    } else if m.starts_with("The document ended without") {
        "Add \\end{document} at the end of the main file.".into()
    } else if let Some(k) = cap(r"Reference [`'](\S+)' on page \d+ undefined") {
        format!("No \\label{{{k}}} anywhere: check the spelling (it prints as ??).")
    } else if let Some(k) = cap(r"Citation [`'](\S+)' on page \d+ undefined") {
        format!("No bibliography entry {k}: check the key and the .bib file (it prints as [?]).")
    } else if m.starts_with("Label `") && m.contains("multiply defined") {
        "Two \\label{…}s have the same name: \\ref will point at one of them at random.".into()
    } else {
        return None;
    };
    Some(s)
}

/// BibTeX's and biber's complaints, from the `.blg`: a malformed entry keeps
/// the bibliography from updating, and the `.log` never says why.
pub fn parse_blg(blg: &str) -> Vec<Diagnostic> {
    let bibtex = regex::Regex::new(r"^(.*)---line (\d+) of file (.+)$").unwrap();
    let biber_at = regex::Regex::new(r"([^/\s]+\.bib)_\d+\.utf8, line (\d+), (.*)$").unwrap();
    let lines: Vec<&str> = blg.lines().collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if let Some(c) = bibtex.captures(l) {
            let mut msg = c[1].trim().to_string();
            if msg.is_empty() && i > 0 {
                msg = lines[i - 1].trim().to_string();
            }
            let warning = msg.starts_with("Warning--");
            let msg = msg.trim_start_matches("Warning--").to_string();
            let mut d = Diagnostic::new(c[3].trim(), c[2].parse().ok(), if warning { Severity::Warning } else { Severity::Error }, format!("BibTeX: {msg}"));
            d.hint = Some("This entry in the .bib file is malformed (often a missing comma, brace or quote), so the bibliography is not updating.".into());
            out.push(d);
        } else if let Some(m) = l.strip_prefix("I couldn't open database file ") {
            out.push(Diagnostic::new(m.trim(), None, Severity::Error, format!("BibTeX could not open {}", m.trim())));
        } else if let Some(m) = l.strip_prefix("ERROR - ") {
            let mut d = Diagnostic::new("", None, Severity::Error, format!("biber: {}", m.trim()));
            if let Some(c) = biber_at.captures(m) {
                d.file = c[1].to_string();
                d.line = c[2].parse().ok();
                d.message = format!("biber: {}", c[3].trim());
            }
            out.push(d);
        }
    }
    out.dedup();
    out
}

/// Moves errors to where the fix goes, now that the project can be read: a
/// runaway argument to the command that opened it (TeX reports the place it
/// gave up, often the `\input` of the file, or the end of the document), an
/// environment that never closed to its `\begin`, and every error with an
/// `l.N` excerpt gets the culprit's columns on its line. `files` are the
/// project's `.tex` files, searched with the diagnostic's own file first.
pub fn refine(diags: &mut [Diagnostic], main: &str, read: impl Fn(&str) -> Option<String>, files: &[String]) {
    let texts: Vec<(String, String)> = files.iter().filter_map(|f| read(f).map(|t| (f.clone(), t))).collect();
    let ordered = |first: &str| {
        let mut v: Vec<&(String, String)> = texts.iter().collect();
        v.sort_by_key(|(f, _)| (f != first, f != main));
        v
    };
    let env_re = regex::Regex::new(r"\\begin\{([^}]+)\} on input line (\d+) ended by \\end\{([^}]+)\}").unwrap();
    for d in diags.iter_mut() {
        if let Some(r) = d.runaway.clone()
            && (d.message.starts_with("File ended while scanning use of") || d.message.starts_with("Paragraph ended before"))
            && let Some((file, line, cols, cmd)) = find_runaway(&ordered(&d.file), &r)
        {
            let why = if d.message.starts_with("File") { "it runs to the end of the file" } else { "it runs into a blank line" };
            d.message = format!("{cmd}{{ is never closed: {why}");
            d.hint = Some(format!("Add the missing }} to close {cmd}{{…}}."));
            (d.file, d.line, d.column, d.end_column, d.before) = (file, Some(line), Some(cols.0), Some(cols.1), None);
        }
        if let Some(c) = env_re.captures(&d.message.clone()) {
            let (env, n, end) = (c[1].to_string(), c[2].parse::<usize>().unwrap_or(0), c[3].to_string());
            let begin = format!("\\begin{{{env}}}");
            let hit = ordered(&d.file).into_iter().find_map(|(f, t)| {
                let l = t.lines().nth(n.checked_sub(1)?)?;
                l.find(&begin).map(|b| (f.clone(), utf16_col(l, b), utf16_col(l, b + begin.len())))
            });
            if let Some((file, from, to)) = hit {
                if end == "document" {
                    d.message = format!("{begin} is never closed (\\end{{document}} came first)");
                    d.hint = Some(format!("Add \\end{{{env}}} where the {env} should end."));
                    (d.file, d.line, d.column, d.end_column, d.before) = (file, Some(n), Some(from), Some(to), None);
                } else {
                    d.hint = Some(format!("The {begin} is at {file}:{n}; this \\end{{{end}}} doesn't match it. Every \\begin needs its own \\end, innermost first."));
                }
            }
        }
        if d.column.is_none()
            && let (Some(before), Some(n)) = (d.before.as_deref(), d.line)
            && let Some((_, t)) = texts.iter().find(|(f, _)| *f == d.file)
            && let Some(l) = t.lines().nth(n.saturating_sub(1))
            && let Some((a, b)) = culprit_columns(l, before, d.top.as_deref().and_then(trailing_macro).as_deref())
        {
            (d.column, d.end_column) = (Some(a), Some(b));
        }
    }
}

/// The `.tex` files of a project, relative to it (private dirs skipped).
pub fn tex_files(dir: &Path) -> Vec<String> {
    walkdir::WalkDir::new(dir)
        .max_depth(6)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'))
        .flatten()
        .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|x| x == "tex"))
        .filter_map(|e| e.path().strip_prefix(dir).ok().map(|p| p.to_string_lossy().into_owned()))
        .collect()
}

/// Where a runaway argument starts: TeX prints its text (`{bold and never
/// closed. More …`) with its own spacing, so both sides are compared with
/// whitespace removed. Returns the file, line, the columns of the command
/// that took the argument (or of the brace), and the command.
fn find_runaway(texts: &[&(String, String)], runaway: &str, ) -> Option<(String, usize, (usize, usize), String)> {
    let want: String = runaway.chars().filter(|c| !c.is_whitespace()).take(40).collect();
    if want.chars().count() < 4 {
        return None;
    }
    for (f, t) in texts {
        let (squeezed, at): (String, Vec<usize>) = t.char_indices().filter(|(_, c)| !c.is_whitespace()).map(|(i, c)| (c, i)).unzip();
        let Some(k) = squeezed.find(&want) else { continue };
        let pos = at[squeezed[..k].chars().count()];
        // The command just before the brace: `\textbf{`, `\emph*{`.
        let head = &t[..pos];
        let cmd_start = head
            .rfind('\\')
            .filter(|&b| head[b + 1..].chars().all(|c| c.is_ascii_alphabetic() || c == '@' || c == '*') && b + 1 < head.len());
        let from = cmd_start.unwrap_or(pos);
        let cmd = cmd_start.map(|b| head[b..].to_string()).unwrap_or_default();
        let line_start = t[..from].rfind('\n').map_or(0, |n| n + 1);
        let line_no = t[..from].matches('\n').count() + 1;
        let l = t[line_start..].lines().next().unwrap_or("");
        return Some((f.clone(), line_no, (utf16_col(l, from - line_start), utf16_col(l, pos + 1 - line_start)), cmd));
    }
    None
}

/// Columns of the culprit in `line`, given TeX's excerpt of the line up to
/// where it stopped (maybe with a leading `...`). The culprit is `token` when
/// the excerpt ends with it (an undefined macro), else the last word.
fn culprit_columns(line: &str, before: &str, token: Option<&str>) -> Option<(usize, usize)> {
    let before = before.strip_prefix("...").unwrap_or(before).trim_end();
    if before.is_empty() {
        return None;
    }
    let tail_from = before.char_indices().rev().nth(39).map_or(0, |(i, _)| i);
    let tail = &before[tail_from..];
    let end = if line.starts_with(before) { before.len() } else { line.find(tail)? + tail.len() };
    let command = regex::Regex::new(r"\\[A-Za-z@]+\*?(\[[^\]]*\])?\{[^{}]*\}$").unwrap();
    let piece = match token.filter(|t| before.ends_with(t)) {
        Some(t) => t.len(),
        None if command.is_match(before) => before.len() - command.find(before).unwrap().start(),
        None => {
            let word = before.rsplit(char::is_whitespace).next().unwrap_or("");
            word.char_indices().rev().nth(29).map_or(word.len(), |(i, _)| word.len() - i)
        }
    };
    let start = end.checked_sub(piece).filter(|&s| line.is_char_boundary(s))?;
    Some((utf16_col(line, start), utf16_col(line, end)))
}

/// 1-based UTF-16 column (Monaco's) of byte offset `b` in `line`.
fn utf16_col(line: &str, b: usize) -> usize {
    line[..b.min(line.len())].encode_utf16().count() + 1
}

/// The `l.12 …` line TeX prints after an error: the line number.
fn error_line(line: &str) -> Option<usize> {
    line.strip_prefix("l.")?.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// `Package hyperref Warning: msg` / `Class article Warning: msg` → (name, msg).
fn package_warning(line: &str) -> Option<(String, String)> {
    for kind in ["Package ", "Class "] {
        if let Some(rest) = line.strip_prefix(kind)
            && let Some((name, msg)) = rest.split_once(" Warning: ")
        {
            return Some((name.to_string(), msg.trim().to_string()));
        }
    }
    None
}

fn input_line(msg: &str) -> Option<usize> {
    let idx = msg.rfind("on input line ")?;
    msg[idx + "on input line ".len()..]
        .trim_end_matches('.')
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// Pushes every `(./file` and pops every `)` — the convention TeX uses to
/// show which file it is reading. Message text full of parentheses makes
/// this a heuristic, which is why errors prefer `-file-line-error` lines
/// and error context (source text) is never fed through it.
fn track_files(line: &str, stack: &mut Vec<String>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                let rest = &line[i + 1..];
                let end = rest
                    .find(|c: char| c.is_whitespace() || c == '(' || c == ')')
                    .unwrap_or(rest.len());
                let name = &rest[..end];
                if looks_like_file(name) {
                    stack.push(normalize(name));
                    i += 1 + end;
                    continue;
                }
                i += 1;
            }
            b')' => {
                if stack.len() > 1 {
                    stack.pop();
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
}

fn looks_like_file(s: &str) -> bool {
    (s.starts_with("./") || s.starts_with('/') || s.contains('/'))
        && s.rsplit('/').next().is_some_and(|n| n.contains('.'))
}

fn normalize(path: &str) -> String {
    path.strip_prefix("./").unwrap_or(path).to_string()
}

// ---- SyncTeX -------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SyncBox {
    pub page: u32,
    pub h: f64,
    pub v: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SourcePos {
    pub file: String,
    pub line: usize,
    pub column: i64,
}

/// Source line → boxes in the PDF (top-left origin, PDF points).
pub async fn forward(dir: &Path, pdf: &Path, file: &str, line: usize, col: usize) -> anyhow::Result<Vec<SyncBox>> {
    let out = Command::new("synctex")
        .current_dir(dir)
        .arg("view")
        .arg("-i")
        .arg(format!("{line}:{}:{file}", col.max(1)))
        .arg("-o")
        .arg(pdf)
        .stdin(Stdio::null())
        .output()
        .await?;
    Ok(parse_forward(&String::from_utf8_lossy(&out.stdout)))
}

pub fn parse_forward(s: &str) -> Vec<SyncBox> {
    let mut boxes = Vec::new();
    let mut cur: Option<SyncBox> = None;
    for line in s.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        let num = || v.trim().parse::<f64>().unwrap_or(0.0);
        match k {
            "Page" => {
                if let Some(b) = cur.take() {
                    boxes.push(b);
                }
                cur = Some(SyncBox { page: num() as u32, h: 0.0, v: 0.0, width: 0.0, height: 0.0 });
            }
            "h" => cur.as_mut().map(|b| b.h = num()).unwrap_or(()),
            "v" => cur.as_mut().map(|b| b.v = num()).unwrap_or(()),
            "W" => cur.as_mut().map(|b| b.width = num()).unwrap_or(()),
            "H" => cur.as_mut().map(|b| b.height = num()).unwrap_or(()),
            _ => {}
        }
    }
    if let Some(b) = cur {
        boxes.push(b);
    }
    boxes.dedup();
    boxes
}

/// PDF position (top-left origin, points) → source file and line.
pub async fn inverse(dir: &Path, pdf: &Path, page: u32, x: f64, y: f64) -> anyhow::Result<Option<SourcePos>> {
    let out = Command::new("synctex")
        .current_dir(dir)
        .arg("edit")
        .arg("-o")
        .arg(format!("{page}:{x:.2}:{y:.2}:{}", pdf.display()))
        .stdin(Stdio::null())
        .output()
        .await?;
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    Ok(parse_inverse(&String::from_utf8_lossy(&out.stdout), &canonical))
}

pub fn parse_inverse(s: &str, dir: &Path) -> Option<SourcePos> {
    let mut file = None;
    let mut line = None;
    let mut column = -1;
    for l in s.lines() {
        if let Some(v) = l.strip_prefix("Input:") {
            let v = v.trim();
            let rel = Path::new(v)
                .strip_prefix(dir)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| v.to_string());
            file = Some(rel.replace("/./", "/").trim_start_matches("./").to_string());
        } else if let Some(v) = l.strip_prefix("Line:") {
            line = v.trim().parse().ok();
        } else if let Some(v) = l.strip_prefix("Column:") {
            column = v.trim().parse().unwrap_or(-1);
        }
        if file.is_some() && line.is_some() {
            break;
        }
    }
    Some(SourcePos { file: file?, line: line?, column })
}

/// Labels whose `\newlabel` carries an empty first field (`{{}{page}}`).
pub fn empty_labels(aux: &str) -> Vec<String> {
    let re = regex::Regex::new(r"\\newlabel\{([^}@]+)\}\{\{\}\{").unwrap();
    re.captures_iter(aux).map(|c| c[1].to_string()).collect()
}

/// The text file and line holding `\label{name}`, searching main first, then
/// every other text file.
fn find_label(dir: &Path, main: &str, name: &str) -> (String, Option<usize>) {
    let needle = format!("\\label{{{name}}}");
    let mut files = vec![main.to_string()];
    files.extend(project::list_files(dir).into_iter().filter(|f| f.text && f.path != main).map(|f| f.path));
    for f in files {
        if let Ok(text) = std::fs::read_to_string(dir.join(&f))
            && let Some(i) = text.lines().position(|l| l.contains(&needle))
        {
            return (f, Some(i + 1));
        }
    }
    (main.to_string(), None)
}

/// The words of a source line as the reader will see them: commands and
/// their arguments for refs/cites/labels dropped (`\Cref{fig:x}` renders as
/// "Figure 1", not "fig x"), other command names dropped, braces and `~`
/// ignored. Returns (char offset, core) per word.
pub fn source_words(line: &str) -> Vec<(usize, String)> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '%' {
            break;
        }
        if c == '\\' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i].is_ascii_alphabetic() {
                i += 1;
            }
            let name: String = chars[start + 1..i].iter().collect();
            let refish = matches!(name.as_str(), "cite" | "citep" | "citet" | "autocite" | "ref" | "Cref" | "cref" | "eqref" | "autoref" | "pageref" | "label" | "includegraphics" | "usepackage" | "input" | "include" | "vspace" | "hspace" | "begin" | "end");
            if refish {
                while i < chars.len() && (chars[i] == '[' || chars[i] == '{') {
                    let mut depth = 0;
                    while i < chars.len() {
                        if chars[i] == '[' || chars[i] == '{' { depth += 1; }
                        if chars[i] == ']' || chars[i] == '}' { depth -= 1; if depth == 0 { i += 1; break; } }
                        i += 1;
                    }
                }
            }
            continue;
        }
        if c.is_alphanumeric() {
            let start = i;
            while i < chars.len() && chars[i].is_alphanumeric() {
                i += 1;
            }
            let w: String = chars[start..i].iter().flat_map(|c| c.to_lowercase()).collect();
            out.push((start, w));
            continue;
        }
        i += 1;
    }
    out
}

/// A rendered word on a page: its box, the text as printed, and its searchable core.
struct PdfWord {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    text: String,
    core: String,
}

/// One text line of a page: its vertical extent and its words, left to right.
struct PdfLine {
    y0: f64,
    y1: f64,
    words: Vec<PdfWord>,
}

/// The page's words (pdftotext's boxes) grouped into text lines by vertical
/// overlap, top to bottom.
async fn pdf_lines(pdf: &Path, page: u32) -> Vec<PdfLine> {
    let Ok(out) = Command::new("pdftotext")
        .args(["-bbox", "-f", &page.to_string(), "-l", &page.to_string()])
        .arg(pdf)
        .arg("-")
        .stdin(Stdio::null())
        .output()
        .await
    else {
        return vec![];
    };
    let html = String::from_utf8_lossy(&out.stdout);
    let re = regex::Regex::new(r#"<word xMin="([\d.]+)" yMin="([\d.]+)" xMax="([\d.]+)" yMax="([\d.]+)">([^<]*)</word>"#).unwrap();
    let mut lines: Vec<PdfLine> = Vec::new();
    for c in re.captures_iter(&html) {
        let f = |i: usize| c[i].parse::<f64>().unwrap_or(0.0);
        let text = html_unescape(&c[5]);
        let w = PdfWord { x0: f(1), y0: f(2), x1: f(3), y1: f(4), core: core(&text), text };
        match lines.iter_mut().find(|l| w.y0 < l.y1 - 1.0 && w.y1 > l.y0 + 1.0) {
            Some(l) => {
                l.y0 = l.y0.min(w.y0);
                l.y1 = l.y1.max(w.y1);
                l.words.push(w);
            }
            None => lines.push(PdfLine { y0: w.y0, y1: w.y1, words: vec![w] }),
        }
    }
    for l in &mut lines {
        l.words.sort_by(|a, b| a.x0.total_cmp(&b.x0));
    }
    lines.sort_by(|a, b| a.y0.total_cmp(&b.y0));
    lines
}

fn same_word(a: &str, b: &str) -> bool {
    // Hyphenated or ligatured words: a long-enough prefix counts.
    a == b || (a.len() >= 5 && b.len() >= 5 && (a.starts_with(b) || b.starts_with(a)))
}

/// Forward sync, word-precise: the cursor's word on `line` at `col` (1-based),
/// found on the PDF by matching the words around it against the page's text
/// lines (SyncTeX only narrows to a paragraph). `anchors` are SyncTeX's boxes
/// for the line: the page(s) to search, and a tie-breaker on vertical distance.
pub async fn forward_word(pdf: &Path, line: &str, col: usize, anchors: &[SyncBox]) -> Option<SyncBox> {
    let words = source_words(line);
    let at = col.saturating_sub(1);
    let idx = words.iter().position(|(o, w)| *o <= at && at < *o + w.chars().count())
        .or_else(|| words.iter().rposition(|(o, _)| *o <= at))
        .or_else(|| (!words.is_empty()).then_some(0))?;
    let key = words[idx].1.clone();
    let context: Vec<&str> = words.iter().enumerate().filter(|(i, _)| *i != idx && i.abs_diff(idx) <= 4).map(|(_, (_, w))| w.as_str()).filter(|w| w.len() >= 3).collect();
    let mut pages: Vec<u32> = anchors.iter().map(|b| b.page).collect();
    pages.sort_unstable();
    pages.dedup();
    if pages.is_empty() {
        pages.push(1);
    }
    let mut best: Option<(i64, f64, SyncBox)> = None;
    for page in pages {
        for pl in pdf_lines(pdf, page).await {
            let pl = pl.words;
            let Some(hit) = pl.iter().position(|w| same_word(&w.core, &key)) else { continue };
            let mut score = 3i64;
            score += context.iter().filter(|c| pl.iter().any(|w| same_word(&w.core, c))).count() as i64 * 2;
            // Prefer the PDF line that lies inside SyncTeX's boxes for this source line.
            let ymid = (pl[hit].y0 + pl[hit].y1) / 2.0;
            let dist = anchors.iter().filter(|b| b.page == page).map(|b| ((b.v - b.height / 2.0) - ymid).abs()).fold(f64::MAX, f64::min);
            if dist < 6.0 {
                score += 2;
            }
            let w = &pl[hit];
            let cand = SyncBox { page, h: w.x0, v: w.y1, width: w.x1 - w.x0, height: w.y1 - w.y0 };
            if best.as_ref().is_none_or(|(s, d, _)| score > *s || (score == *s && dist < *d)) {
                best = Some((score, dist, cand));
            }
        }
    }
    best.filter(|(s, _, _)| *s >= 5 || context.is_empty()).map(|(_, _, b)| b)
}

// ---- click → source, precisely -------------------------------------------------
//
// SyncTeX (with pdflatex) knows only which *line* produced a box, and after
// TeX reflows a paragraph that line is often the paragraph's first, or a
// neighbour. So the click is resolved from the words themselves: pdftotext's
// word boxes give the words on the clicked text line, and the source line
// that contains that phrase (nearest to SyncTeX's hint) wins, down to the
// column of the clicked word — or of the `\cite`/`\ref` behind a `[12]`.

/// The words on the text line under (x, y) on `page`, and which of them was
/// clicked (the one containing the point, else the nearest on that line).
pub async fn words_at(pdf: &Path, page: u32, x: f64, y: f64) -> Option<(usize, Vec<String>)> {
    let line = pdf_lines(pdf, page)
        .await
        .into_iter()
        .filter(|l| y >= l.y0 - 2.0 && y <= l.y1 + 2.0)
        .min_by(|p, q| ((p.y0 + p.y1) / 2.0 - y).abs().total_cmp(&((q.y0 + q.y1) / 2.0 - y).abs()))?;
    let idx = line
        .words
        .iter()
        .enumerate()
        .map(|(i, w)| (i, if x < w.x0 { w.x0 - x } else if x > w.x1 { x - w.x1 } else { 0.0 }))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|(_, d)| *d < 60.0)?
        .0;
    Some((idx, line.words.into_iter().map(|w| w.text).collect()))
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'")
}

/// The searchable part of a rendered word: its leading run of letters and
/// digits, lowercased — so "Grammatech’s," matches `Grammatech's` in the source
/// and "(see" matches `see`.
fn core(w: &str) -> String {
    w.trim_start_matches(|c: char| !c.is_alphanumeric())
        .chars()
        .take_while(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Lowercased characters of a line, one per original char, so positions map 1:1.
fn lower_chars(line: &str) -> Vec<char> {
    line.chars().map(|c| c.to_lowercase().next().unwrap_or(c)).collect()
}

/// Whole-word occurrences of `key` (lowercase chars) in `hay`: char offsets.
fn word_hits(hay: &[char], key: &[char]) -> Vec<usize> {
    let mut out = Vec::new();
    if key.is_empty() || hay.len() < key.len() {
        return out;
    }
    for at in 0..=hay.len() - key.len() {
        if &hay[at..at + key.len()] != key {
            continue;
        }
        let before = at == 0 || !hay[at - 1].is_alphanumeric();
        let after = at + key.len() == hay.len() || !hay[at + key.len()].is_alphanumeric();
        if before && after {
            out.push(at);
        }
    }
    out
}

/// The commands that print as a `[12]` / `[?]` mark, as written in the source.
const REF_COMMANDS: [&str; 7] = ["\\cite", "\\ref", "\\eqref", "\\autoref", "\\cref", "\\Cref", "\\pageref"];

/// Is the rendered word a reference mark like `[12]`, `[3, 4]` or `[?]`?
fn is_ref_mark(w: &str) -> bool {
    let w = w.trim();
    w.len() >= 3 && w.starts_with('[') && w.ends_with(']') && w[1..w.len() - 1].chars().all(|c| c.is_ascii_digit() || ",? –-".contains(c))
}

/// The source line and column for the clicked word, given the words on its
/// PDF line and SyncTeX's line hint. `None` when nothing matches well enough.
pub fn locate(text: &str, words: &[String], idx: usize, hint: usize) -> Option<(usize, usize)> {
    let clicked = words.get(idx)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let key = core(clicked);
    let context: Vec<String> = words
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != idx && i.abs_diff(idx) <= 3)
        .map(|(_, w)| core(w))
        .filter(|w| w.len() >= 3)
        .collect();
    let lowered: Vec<Vec<char>> = lines.iter().map(|l| lower_chars(l)).collect();
    let key_chars: Vec<char> = key.chars().collect();
    let has = |l: &[char], w: &str| -> bool { let w: Vec<char> = w.chars().collect(); l.windows(w.len()).any(|x| x == w.as_slice()) };
    let near = |i: usize| i.abs_diff(hint.saturating_sub(1));
    let mut best: Option<(i64, usize)> = None; // (score, line index)
    for (i, l) in lowered.iter().enumerate() {
        let mut score = 0i64;
        if is_ref_mark(clicked) {
            if REF_COMMANDS.iter().any(|c| has(l, c)) {
                score += 3;
            }
        } else if key_chars.len() >= 2 && !word_hits(l, &key_chars).is_empty() {
            score += 3;
        }
        if score == 0 {
            continue;
        }
        score += context.iter().filter(|w| has(l, w)).count() as i64 * 2;
        // Prefer SyncTeX's neighbourhood; a far line needs real context to win.
        let dist = near(i);
        score -= if dist <= 2 { 0 } else if dist <= 30 { 1 } else { 3 };
        if best.is_none_or(|(s, j)| score > s || (score == s && near(i) < near(j))) {
            best = Some((score, i));
        }
    }
    let (score, i) = best?;
    if score < 3 {
        return None;
    }
    let col = column_of(lines[i], clicked).unwrap_or(1);
    Some((i + 1, col))
}

/// Where on `line` the clicked `word` lives (1-based column), or where the
/// `\cite`/`\ref` that produced a `[12]` / `[?]` does.
pub fn column_of(line: &str, word: &str) -> Option<usize> {
    if is_ref_mark(word) {
        return REF_COMMANDS.iter().find_map(|cmd| line.find(cmd)).map(|i| line[..i].chars().count() + 1);
    }
    let key: Vec<char> = core(word).chars().collect();
    if key.is_empty() {
        return None;
    }
    let hay = lower_chars(line);
    if let Some(at) = word_hits(&hay, &key).first() {
        return Some(at + 1);
    }
    hay.windows(key.len()).position(|w| w == key.as_slice()).map(|at| at + 1)
}

// ---- pages for the AI ------------------------------------------------------------

/// Number of pages, from `pdfinfo` (counting `/Type /Page` objects ourselves
/// is unreliable); 1 when it is missing.
pub async fn page_count(pdf: &Path) -> u32 {
    let out = Command::new("pdfinfo").arg(pdf).stdin(Stdio::null()).output().await;
    if let Ok(o) = out {
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some(v) = l.strip_prefix("Pages:") {
                return v.trim().parse().unwrap_or(1);
            }
        }
    }
    1
}

/// One page as PNG at `dpi` (pdftoppm).
pub async fn render_page(pdf: &Path, page: u32, dpi: u32) -> anyhow::Result<Vec<u8>> {
    let out = Command::new("pdftoppm")
        .arg("-png")
        .arg("-r")
        .arg(dpi.to_string())
        .arg("-f")
        .arg(page.to_string())
        .arg("-l")
        .arg(page.to_string())
        .arg("-singlefile")
        .arg(pdf)
        .stdin(Stdio::null())
        .output()
        .await?;
    anyhow::ensure!(out.status.success(), "pdftoppm: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(out.stdout)
}

/// Text of one page (or all) with layout kept (pdftotext).
pub async fn page_text(pdf: &Path, page: Option<u32>) -> anyhow::Result<String> {
    let mut cmd = Command::new("pdftotext");
    if let Some(p) = page {
        cmd.arg("-f").arg(p.to_string()).arg("-l").arg(p.to_string());
    }
    let out = cmd.arg("-layout").arg(pdf).arg("-").stdin(Stdio::null()).output().await?;
    anyhow::ensure!(out.status.success(), "pdftotext: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_words_read_like_the_pdf() {
        let w: Vec<String> = source_words(r"In \Cref{fig:flow-vs-classical}(a), an operator's \emph{logic} owns~it. % note").into_iter().map(|(_, w)| w).collect();
        assert_eq!(w, vec!["in", "a", "an", "operator", "s", "logic", "owns", "it"]);
        let w: Vec<(usize, String)> = source_words(r"Figure~\ref{fig:a} shows");
        assert_eq!(w, vec![(0, "figure".into()), (19, "shows".into())]);
        assert!(same_word("decou", "decouple") && !same_word("in", "inner"));
    }

    #[test]
    fn empty_label_numbers_are_spotted() {
        let aux = "\\newlabel{fig:a}{{}{1}}\n\\newlabel{fig:a@cref}{{}{[1][1][]1}}\n\\newlabel{sec:b}{{2}{3}}\n";
        assert_eq!(empty_labels(aux), vec!["fig:a".to_string()]);
    }

    #[test]
    fn clicks_resolve_to_the_right_line_and_column() {
        let text = "\\section{Intro}\nWe follow Smith~\\cite{smith20} and define\nthe operator $\\phi$ here.\n\nA theorem about the\noperator follows (see Fig.~\\ref{fig:a}).\n";
        let w = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        // The clicked word, with its neighbours, beats a wrong SyncTeX hint.
        assert_eq!(locate(text, &w("We follow Smith [12] and define"), 1, 5), Some((2, 4)));
        assert_eq!(locate(text, &w("We follow Smith [12] and define"), 3, 2), Some((2, 17)));
        // "the" alone is ambiguous; its neighbours disambiguate.
        assert_eq!(locate(text, &w("A theorem about the"), 3, 1), Some((5, 17)));
        assert_eq!(locate(text, &w("operator follows (see Fig. [?]"), 4, 6), Some((6, text.lines().nth(5).unwrap().find("\\ref").unwrap() + 1)));
        assert_eq!(locate(text, &w("zzz qqq"), 0, 1), None);
        assert_eq!(column_of("the theorem", "the"), Some(1));
        assert_eq!(column_of("a theorem the", "the"), Some(11));
        assert_eq!(column_of("as Grammatech's tool", "Grammatech’s,"), Some(4));
        assert_eq!(column_of("über das Ding", "Ding,"), Some(10));
    }

    const LOG: &str = "This is pdfTeX\n(./main.tex (./sub/part.tex\nLaTeX Warning: Reference `eq:a' on page 1 undefined on input line 2.\n) \n./main.tex:8: Undefined control sequence.\nl.8 \\undefinedmacro\n\nOverfull \\hbox (12.3pt too wide) in paragraph at lines 5--6\nPackage hyperref Warning: Token not allowed in a PDF string\n(hyperref)                removing `\\label' on input line 9.\n\n! Missing $ inserted.\n<inserted text> \n                $\nl.12 x_1\n)";

    #[test]
    fn parses_errors_warnings_and_files() {
        let d = parse_log(LOG, "main.tex");
        let find = |m: &str| d.iter().find(|x| x.message.contains(m)).cloned().unwrap();
        let w = find("Reference");
        assert_eq!((w.file.as_str(), w.line, w.severity), ("sub/part.tex", Some(2), Severity::Warning));
        let e = find("Undefined control");
        assert_eq!((e.file.as_str(), e.line, e.severity), ("main.tex", Some(8), Severity::Error));
        assert_eq!(e.message, "Undefined control sequence \\undefinedmacro");
        let o = find("Overfull");
        assert_eq!((o.line, o.severity), (Some(5), Severity::Info));
        let h = find("Token not allowed");
        assert_eq!((h.file.as_str(), h.line), ("main.tex", Some(9)));
        let m = find("Missing $");
        assert_eq!((m.file.as_str(), m.line), ("main.tex", Some(12)));
    }

    /// Runs the whole pipeline over a log and an in-memory project.
    fn diagnose(log: &str, files: &[(&str, &str)]) -> Vec<Diagnostic> {
        let mut d = parse_log(log, "main.tex");
        let names: Vec<String> = files.iter().map(|(f, _)| f.to_string()).collect();
        refine(&mut d, "main.tex", |f| files.iter().find(|(n, _)| *n == f).map(|(_, t)| t.to_string()), &names);
        d.retain(|d| d.severity != Severity::Info);
        d
    }

    const MAIN: &str = "\\documentclass{article}\n\\usepackage{amsmath}\n\\begin{document}\n\\section{Intro}\\label{sec:intro}\nHello world.\n\\input{sections/body}\n\\end{document}\n";

    #[test]
    fn undefined_macro_is_named_and_underlined() {
        let log = "(./main.tex (./sections/body.tex\n./sections/body.tex:2: Undefined control sequence.\nl.2 We use \\fooo\n                {x} here.\nThe control sequence at the end of the top line\nof your error message was never \\def'ed. If you have\n)";
        let d = diagnose(log, &[("main.tex", MAIN), ("sections/body.tex", "Some body text here.\nWe use \\fooo{x} here.\n")]);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].message, "Undefined control sequence \\fooo");
        assert_eq!((d[0].file.as_str(), d[0].line, d[0].column, d[0].end_column), ("sections/body.tex", Some(2), Some(8), Some(13)));
        assert!(d[0].hint.as_deref().unwrap().contains("\\fooo is not defined"));
        assert!(d[0].context.as_deref().unwrap().contains("{x} here."));
    }

    #[test]
    fn runaway_argument_lands_on_the_command_that_opened_it() {
        // TeX blames the \input line of main.tex; the unclosed brace is in body.tex.
        let log = "(./main.tex (./sections/body.tex)\nRunaway argument?\n{bold and never closed. More text after. Final line. \n./main.tex:6: File ended while scanning use of \\textbf .\n<inserted text> \n                \\par \nl.6 \\input{sections/body}\n                         \nI suspect you have forgotten a `}', causing me\n)";
        let body = "Some body text here.\nThis is \\textbf{bold and never closed.\nMore text after.\nFinal line.\n";
        let d = diagnose(log, &[("main.tex", MAIN), ("sections/body.tex", body)]);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].file.as_str(), d[0].line, d[0].column, d[0].end_column), ("sections/body.tex", Some(2), Some(9), Some(17)));
        assert_eq!(d[0].message, "\\textbf{ is never closed: it runs to the end of the file");
    }

    #[test]
    fn unclosed_environment_points_at_its_begin_and_echoes_go() {
        let block = |m: &str, ctx: &str| format!("./main.tex:7: {m}\n\nSee the LaTeX manual or LaTeX Companion for explanation.\nType  H <return>  for immediate help.\n{ctx}\nl.7 \\end{{document}}\n                  \nYour command was ignored.\n\n");
        let log = format!(
            "(./main.tex (./sections/body.tex)\n\n{}{}./main.tex:7: Missing }} inserted.\n<inserted text> \n                }}\nl.7 \\end{{document}}\n                  \n\n! Emergency stop.\n<*> main.tex\n                \n*** (job aborted, no legal \\end found)\n\nHere is how much of TeX's memory you used:\n 1156 strings out of 476091\n./main.tex:7:  ==> Fatal error occurred, no output PDF file produced!\n",
            block("LaTeX Error: \\begin{figure} on input line 2 ended by \\end{document}.", " ...urrenvline \\space ended by \\protect \\end {\\detokenize {document}}."),
            "./main.tex:7: You can't use `\\end' in internal vertical mode.\n\\enddocument ...o \\@@end \n                                                                                                                                                                                                                                              \nl.7 \\end{document}\n                  \n\n"
        );
        let d = diagnose(&log, &[("main.tex", MAIN), ("sections/body.tex", "Some body text here.\n\\begin{figure}\\centering x\n")]);
        assert_eq!(d.len(), 1, "{d:#?}");
        assert_eq!(d[0].message, "\\begin{figure} is never closed (\\end{document} came first)");
        assert_eq!((d[0].file.as_str(), d[0].line, d[0].column, d[0].end_column), ("sections/body.tex", Some(2), Some(1), Some(15)));
    }

    #[test]
    fn missing_file_takes_the_emergency_stops_line() {
        let log = "(./main.tex (./sections/body.tex\n! LaTeX Error: File `sections/missing.tex' not found.\n\nType X to quit or <RETURN> to proceed,\nor enter new name. (Default extension: tex)\n\nEnter file name: \n./sections/body.tex:2: Emergency stop.\n<read *> \n         \nl.2 \\input{sections/missing}\n                            ^^M\n*** (cannot \\read from terminal in nonstop modes)\n";
        let d = diagnose(log, &[("main.tex", MAIN), ("sections/body.tex", "Some body text here.\n\\input{sections/missing}\n")]);
        assert_eq!(d.len(), 1, "{d:#?}");
        assert_eq!(d[0].message, "File `sections/missing.tex' not found.");
        assert_eq!((d[0].file.as_str(), d[0].line, d[0].column, d[0].end_column), ("sections/body.tex", Some(2), Some(1), Some(25)));
    }

    #[test]
    fn math_echo_at_end_of_document_is_dropped() {
        let log = "(./main.tex (./sections/body.tex\n./sections/body.tex:2: Missing $ inserted.\n<inserted text> \n                $\nl.2 The value x_\n                1 is big.\nI've inserted a begin-math/end-math symbol since I think\n\n)\n./main.tex:7: Missing $ inserted.\n<inserted text> \n                $\nl.7 \\end{document}\n                  \nI've inserted a begin-math/end-math symbol since I think\n\nLaTeX Warning: Label(s) may have changed. Rerun to get cross-references right.\n";
        let d = diagnose(log, &[("main.tex", MAIN), ("sections/body.tex", "Some body text here.\nThe value x_1 is big.\n")]);
        assert_eq!(d.len(), 1, "{d:#?}");
        assert_eq!((d[0].line, d[0].column, d[0].end_column), (Some(2), Some(11), Some(13)));
        assert!(d[0].hint.as_deref().unwrap().contains("\\_"));
    }

    #[test]
    fn logs_that_are_not_utf8_still_parse() {
        // pdflatex copies source bytes into the log and wraps lines mid-character.
        let dir = crate::history::tmp_dir("lossy-log");
        let mut bytes = b"(./main.tex\nLaTeX Warning: Reference `a' on page 1 undefined on input line 3.\nIt\xe2\x80".to_vec();
        bytes.extend_from_slice(b"\n./main.tex:5: Undefined control sequence.\nl.5 \\nope\n\n)");
        let path = dir.join("main.log");
        std::fs::write(&path, bytes).unwrap();
        let d = parse_log(&read_lossy(&path), "main.tex");
        assert!(d.iter().any(|d| d.message == "Undefined control sequence \\nope" && d.line == Some(5)), "{d:#?}");
    }

    #[test]
    fn bibtex_errors_point_into_the_bib_file() {
        let blg = "Database file #1: refs.bib\nI was expecting a `,' or a `}'---line 12 of file refs.bib\n :   title = {foo\n :   \nI'm skipping whatever remains of this entry\nWarning--I didn't find a database entry for \"x\"\n(There was 1 error message)\n";
        let d = parse_blg(blg);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].file.as_str(), d[0].line, d[0].severity), ("refs.bib", Some(12), Severity::Error));
    }

    #[test]
    fn parses_synctex_output() {
        let fwd = "This is SyncTeX command line utility, version 1.5\nSyncTeX result begin\nOutput:x.pdf\nPage:1\nx:159.5\ny:156.5\nh:133.7\nv:156.5\nW:343.7\nH:8.1\nbefore:\noffset:-1\nmiddle:\nafter:\nOutput:x.pdf\nPage:2\nh:1\nv:2\nW:3\nH:4\nSyncTeX result end\n";
        let b = parse_forward(fwd);
        assert_eq!(b.len(), 2);
        assert_eq!(b[0], SyncBox { page: 1, h: 133.7, v: 156.5, width: 343.7, height: 8.1 });
        let inv = "SyncTeX result begin\nOutput:x.pdf\nInput:/p/dir/./sub/part.tex\nLine:7\nColumn:-1\nSyncTeX result end";
        assert_eq!(
            parse_inverse(inv, Path::new("/p/dir")),
            Some(SourcePos { file: "sub/part.tex".into(), line: 7, column: -1 })
        );
        assert_eq!(parse_inverse("nothing", Path::new("/p")), None);
    }
}

