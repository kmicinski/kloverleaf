//! Who wrote what. Every text file carries a *provenance map*: a run-length
//! list of spans over its text, each pointing at an **origin** — a person, or
//! one AI apply (which remembers the prompt, the model, the version of the
//! file it addressed and the history entry it produced), or a git push, a
//! restore, an upload. The map lives beside the history in the project's
//! private store, never in the text: clones and PDFs are untouched.
//!
//! It moves with the text. Keystrokes (`apply`) shift and split spans and
//! insert the typist's origin, so editing inside AI prose makes those
//! characters the author's own — which is how a draft becomes theirs. Whole
//! replacements (`rebase`) are diffed against the previous text — by line,
//! then by word, with small unchanged islands absorbed — so unchanged runs
//! keep their history and only inserted text takes the new origin, in blocks.
//!
//! Units are UTF-16 code units, like Monaco's offsets and [`crate::text::Edit`];
//! the browser mirrors `apply` in `web/src/provenance.ts`, so keep them equal.
//! Origin `0` is "before tracking" (text that predates the map).

use crate::text::Edit;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

pub const UNKNOWN: u32 = 0;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub len: usize,
    pub origin: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Map {
    pub spans: Vec<Span>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Origin {
    pub id: u32,
    pub kind: String,
    pub user: String,
    pub ts: i64,
    pub meta: Value,
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The origin kind a history entry's replacement text carries: one fresh
/// origin per AI apply, restore, git push or upload; `None` means the text is
/// the author's own (a person's keystrokes, a new file, a reconnect).
pub fn origin_kind(kind: &str, meta: &Value) -> Option<&'static str> {
    match kind {
        "ai" => Some("ai"),
        "restore" => Some("restore"),
        "replace" if meta["source"] == "git" => Some("git"),
        "replace" if meta["source"] == "upload" => Some("upload"),
        _ => None,
    }
}

impl Map {
    pub fn uniform(len: usize, origin: u32) -> Map {
        let mut m = Map { spans: if len > 0 { vec![Span { len, origin }] } else { vec![] } };
        m.normalize();
        m
    }

    pub fn len(&self) -> usize {
        self.spans.iter().map(|s| s.len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Merges neighbours with one origin and drops empty spans.
    pub fn normalize(&mut self) {
        let mut out: Vec<Span> = Vec::with_capacity(self.spans.len());
        for s in self.spans.drain(..) {
            if s.len == 0 {
                continue;
            }
            match out.last_mut() {
                Some(l) if l.origin == s.origin => l.len += s.len,
                _ => out.push(s),
            }
        }
        self.spans = out;
    }

    /// Spans covering `from..to`.
    pub fn slice(&self, from: usize, to: usize) -> Vec<Span> {
        let mut out = Vec::new();
        let mut pos = 0;
        for s in &self.spans {
            let (a, b) = (pos, pos + s.len);
            pos = b;
            if b <= from {
                continue;
            }
            if a >= to {
                break;
            }
            out.push(Span { len: b.min(to) - a.max(from), origin: s.origin });
        }
        out
    }

    /// Replaces `at..at+remove` with `insert` (if any). The core of `apply`.
    fn splice(&mut self, at: usize, remove: usize, insert: Option<Span>) {
        let total = self.len();
        let (at, end) = (at.min(total), (at + remove).min(total));
        let mut out = self.slice(0, at);
        if let Some(s) = insert {
            out.push(s);
        }
        out.extend(self.slice(end, total));
        self.spans = out;
        self.normalize();
    }

    /// One editor batch, with the same conventions as [`crate::text::apply`]:
    /// each edit is against the text before the batch, later edits first.
    pub fn apply(&mut self, changes: &[Edit], origin: u32) {
        for c in changes {
            let n = utf16_len(&c.text);
            self.splice(c.offset, c.length, (n > 0).then_some(Span { len: n, origin }));
        }
    }

    /// Reassigns `from..to` to `origin` — "this is mine now".
    pub fn set(&mut self, from: usize, to: usize, origin: u32) {
        let to = to.min(self.len());
        if from < to {
            self.splice(from, to - from, Some(Span { len: to - from, origin }));
        }
    }

    /// The map for `new` given this map was for `old`: unchanged runs keep
    /// their origins, inserted text gets `origin`.
    ///
    /// Attribution is decided in blocks, not characters. A character-level
    /// diff would happily match the `e` of one word to the `e` of another,
    /// scattering one-letter "unchanged" islands through an inserted
    /// paragraph (and one-letter "AI" islands through the author's prose).
    /// So: lines are diffed first (patience, so unique lines anchor), each
    /// run of changed lines is re-diffed by *word*, and then any unchanged
    /// run that is small next to the edits around it is absorbed into them —
    /// the "semantic cleanup" rule of diff-match-patch. What survives as
    /// unchanged is a real run of the old text in its old place.
    pub fn rebase(&self, old: &str, new: &str, origin: u32) -> Map {
        let base = if self.len() == utf16_len(old) { self.clone() } else { Map::uniform(utf16_len(old), UNKNOWN) };
        let old_lines = split_lines(old);
        let new_lines = split_lines(new);
        let (lo, ln) = (offsets(&old_lines), offsets(&new_lines));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let ops = similar::capture_diff_slices_deadline(similar::Algorithm::Patience, &old_lines, &new_lines, Some(deadline));
        let mut out = Map::default();
        for d in ops {
            let (tag, o, n) = d.as_tag_tuple();
            match tag {
                similar::DiffTag::Equal => out.spans.extend(base.slice(lo[o.start], lo[o.end])),
                similar::DiffTag::Delete => {}
                similar::DiffTag::Insert => {
                    let ws = new_lines[n.start..n.end].iter().all(|l| l.trim().is_empty());
                    push_new(&mut out, ln[n.end] - ln[n.start], origin, ws);
                }
                similar::DiffTag::Replace => {
                    let old_hunk: String = old_lines[o.start..o.end].concat();
                    let new_hunk: String = new_lines[n.start..n.end].concat();
                    refine(&base, lo[o.start], &old_hunk, &new_hunk, origin, deadline, &mut out);
                }
            }
        }
        out.normalize();
        debug_assert_eq!(out.len(), utf16_len(new));
        out.fitted(new)
    }

    pub fn origins(&self) -> BTreeSet<u32> {
        self.spans.iter().map(|s| s.origin).collect()
    }

    /// `[[len, origin], …]` — compact, and what the browser reads.
    pub fn to_json(&self) -> Value {
        Value::Array(self.spans.iter().map(|s| serde_json::json!([s.len, s.origin])).collect())
    }

    pub fn from_json(v: &Value) -> Option<Map> {
        let mut m = Map::default();
        for s in v.as_array()? {
            let a = s.as_array()?;
            m.spans.push(Span { len: a.first()?.as_u64()? as usize, origin: a.get(1)?.as_u64()? as u32 });
        }
        m.normalize();
        Some(m)
    }

    /// A map that fits `text`: this one if the lengths agree, else all-unknown.
    pub fn fitted(self, text: &str) -> Map {
        let n = utf16_len(text);
        if self.len() == n { self } else { Map::uniform(n, UNKNOWN) }
    }
}

/// Lines with their terminators, so concatenating them gives the text back.
fn split_lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

/// UTF-16 offset of each piece's start, plus the total at the end.
fn offsets(pieces: &[&str]) -> Vec<usize> {
    let mut p = Vec::with_capacity(pieces.len() + 1);
    p.push(0usize);
    for s in pieces {
        p.push(p.last().unwrap() + utf16_len(s));
    }
    p
}

/// Words, runs of whitespace, and single other characters — the units
/// attribution is decided in, so a tint never starts in the middle of a word.
fn tokens(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut kind = 0u8; // 0 none, 1 word, 2 space
    for (i, c) in s.char_indices() {
        let k = if c.is_alphanumeric() || c == '_' { 1 } else if c.is_whitespace() { 2 } else { 3 };
        if i > 0 && (k != kind || k == 3) {
            out.push(&s[start..i]);
            start = i;
        }
        kind = k;
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// One stretch of a word diff, in UTF-16 units of the old and new text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
    equal: bool,
    old: usize,
    new: usize,
    /// The new side is only whitespace (a changed line break, a double
    /// space): nobody's prose, so it takes its neighbour's origin.
    ws: bool,
}

/// The semantic cleanup: an unchanged run flanked by changes on both sides
/// is absorbed when it is no longer than the larger of its neighbours (or
/// is tiny outright). Repeats until nothing changes, like diff-match-patch.
fn cleanup(mut pieces: Vec<Piece>) -> Vec<Piece> {
    const TINY: usize = 3;
    let merge = |v: Vec<Piece>| -> Vec<Piece> {
        let mut out: Vec<Piece> = Vec::with_capacity(v.len());
        for p in v {
            if p.old == 0 && p.new == 0 {
                continue;
            }
            match out.last_mut() {
                Some(l) if l.equal == p.equal => {
                    l.old += p.old;
                    l.new += p.new;
                    l.ws &= p.ws;
                }
                _ => out.push(p),
            }
        }
        out
    };
    pieces = merge(pieces);
    loop {
        let mut changed = false;
        let mut i = 1;
        while i + 1 < pieces.len() {
            let (a, e, b) = (pieces[i - 1], pieces[i], pieces[i + 1]);
            if e.equal && !a.equal && !b.equal {
                let n = e.old;
                let (wa, wb) = (a.old.max(a.new), b.old.max(b.new));
                if n <= TINY || (n <= wa && n <= wb) {
                    pieces[i].equal = false;
                    changed = true;
                }
            }
            i += 1;
        }
        if !changed {
            return pieces;
        }
        pieces = merge(pieces);
    }
}

/// Attributes one run of changed lines by word and appends the result to `out`:
/// `old_hunk` starts at UTF-16 offset `at` of the old text.
fn refine(base: &Map, at: usize, old_hunk: &str, new_hunk: &str, origin: u32, deadline: std::time::Instant, out: &mut Map) {
    let (ot, nt) = (tokens(old_hunk), tokens(new_hunk));
    let (oo, no) = (offsets(&ot), offsets(&nt));
    let ops = similar::capture_diff_slices_deadline(similar::Algorithm::Patience, &ot, &nt, Some(deadline));
    let pieces: Vec<Piece> = ops
        .iter()
        .map(|d| {
            let (tag, o, n) = d.as_tag_tuple();
            Piece {
                equal: tag == similar::DiffTag::Equal,
                old: oo[o.end] - oo[o.start],
                new: no[n.end] - no[n.start],
                ws: nt[n.start..n.end].iter().all(|t| t.chars().all(char::is_whitespace)),
            }
        })
        .collect();
    let mut pos = at;
    for p in cleanup(pieces) {
        if p.equal {
            out.spans.extend(base.slice(pos, pos + p.old));
        } else if p.new > 0 {
            push_new(out, p.new, origin, p.ws);
        }
        pos += p.old;
    }
}

/// Appends inserted text: whitespace-only runs continue the span before them.
fn push_new(out: &mut Map, len: usize, origin: u32, ws: bool) {
    let origin = match out.spans.last() {
        Some(l) if ws => l.origin,
        _ => origin,
    };
    out.spans.push(Span { len, origin });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(offset: usize, length: usize, text: &str) -> Edit {
        Edit { offset, length, text: text.into() }
    }

    #[test]
    fn typing_inside_ai_text_makes_it_mine() {
        let mut m = Map::uniform(10, 7); // "0123456789" by the AI
        m.apply(&[e(4, 2, "AB")], 1); // human overtypes "45"
        assert_eq!(m.spans, vec![Span { len: 4, origin: 7 }, Span { len: 2, origin: 1 }, Span { len: 4, origin: 7 }]);
        m.apply(&[e(10, 0, "!")], 1); // append
        assert_eq!(m.len(), 11);
        assert_eq!(m.spans.last().unwrap().origin, 1);
        m.apply(&[e(0, 11, "")], 1); // delete everything
        assert!(m.is_empty());
    }

    #[test]
    fn batches_apply_later_edits_first_like_text_apply() {
        let text = "hello world";
        let mut m = Map::uniform(11, 1);
        let changes = [e(6, 5, "there"), e(0, 5, "hi")]; // later first
        let after = crate::text::apply(text, &changes);
        m.apply(&changes, 2);
        assert_eq!(after, "hi there");
        assert_eq!(m.len(), utf16_len(&after));
        assert_eq!(m.spans, vec![Span { len: 2, origin: 2 }, Span { len: 1, origin: 1 }, Span { len: 5, origin: 2 }]);
    }

    #[test]
    fn rebase_keeps_unchanged_runs() {
        let old = "The quick brown fox.\n";
        let m = Map::uniform(utf16_len(old), 1);
        let new = "The quick, clever brown fox jumps.\n";
        let r = m.rebase(old, new, 9);
        assert_eq!(r.len(), utf16_len(new));
        // Only the inserted words are the AI's; every old word stays the author's.
        assert_eq!(r.spans, vec![Span { len: 9, origin: 1 }, Span { len: 8, origin: 9 }, Span { len: 10, origin: 1 }, Span { len: 6, origin: 9 }, Span { len: 2, origin: 1 }]);
        // Astral characters count as two units, like Monaco.
        let r2 = Map::uniform(utf16_len("a b"), 1).rebase("a b", "a 😀 b", 9);
        assert_eq!(r2.spans, vec![Span { len: 2, origin: 1 }, Span { len: 3, origin: 9 }, Span { len: 1, origin: 1 }]);
        // Attribution is by word: a letter typed into a word makes the word the writer's.
        let r3 = Map::uniform(utf16_len("ab"), 1).rebase("ab", "a😀b", 9);
        assert_eq!(r3.spans, vec![Span { len: 4, origin: 9 }]);
    }

    #[test]
    fn rebase_attributes_in_blocks_not_letters() {
        // The real failure: a char-level diff matched stray letters of the
        // old preamble inside the AI's new lines and vice versa.
        let old = "\\documentclass{article}\n\\usepackage{graphicx} % figures\n\\pagestyle{empty}\n\n\\begin{document}\nDatalog engines are fast.\n\\end{document}\n";
        let new = "\\documentclass{article}\n\\usepackage{graphicx} % figures\n\\usepackage{xcolor}\n\\usepackage{microtype} % tighter justification\n\\newcommand{\\thesisbox}[1]{\\fbox{#1}}\n\\pagestyle{empty}\n\n\\begin{document}\n\\thesisbox{\\textbf{Thesis.} Datalog engines are fast because every operator owns its data.}\nDatalog engines are fast.\n\\end{document}\n";
        let r = Map::uniform(utf16_len(old), 1).rebase(old, new, 9);
        assert_eq!(r.len(), utf16_len(new));
        let human: usize = r.spans.iter().filter(|s| s.origin == 1).map(|s| s.len).sum();
        assert_eq!(human, utf16_len(old), "every old character survived as the author's");
        assert!(r.spans.iter().all(|s| s.len >= 8), "no fragments: {:?}", r.spans);
        assert_eq!(r.spans.iter().filter(|s| s.origin == 9).count(), 2, "two AI blocks: {:?}", r.spans);
    }

    #[test]
    fn rebase_rewrite_of_a_sentence_is_one_block() {
        let old = "We present a novel approach to X that beats Y.\n";
        let new = "This paper presents a new technique for X, outperforming Y.\n";
        let r = Map::uniform(utf16_len(old), 1).rebase(old, new, 9);
        // Scattered single words (" ", "X", "Y") must not survive as islands.
        assert!(r.spans.iter().all(|s| s.len >= 4), "{:?}", r.spans);
        let ai: usize = r.spans.iter().filter(|s| s.origin == 9).map(|s| s.len).sum();
        assert!(ai > r.len() * 3 / 4, "{:?}", r.spans);
    }

    #[test]
    fn rebase_insert_mid_line_tints_only_the_insert() {
        let old = "First. Second. Third.\n";
        let new = "First. Second. New sentence here. Third.\n";
        let r = Map::uniform(utf16_len(old), 1).rebase(old, new, 9);
        assert_eq!(r.spans, vec![Span { len: 15, origin: 1 }, Span { len: 19, origin: 9 }, Span { len: 7, origin: 1 }]);
    }

    #[test]
    fn rebase_whitespace_changes_are_nobodys() {
        // A reflowed line break or a doubled space is not prose: it keeps its neighbour's author.
        let old = "one two\nthree four\n";
        let new = "one two three\nfour\n";
        let r = Map::uniform(utf16_len(old), 1).rebase(old, new, 9);
        assert_eq!(r.spans, vec![Span { len: utf16_len(new), origin: 1 }], "{:?}", r.spans);
        // Blank lines inserted between the author's paragraphs stay the author's.
        let r2 = Map::uniform(3, 1).rebase("a\nb", "a\n\n\nb", 9);
        assert_eq!(r2.spans, vec![Span { len: 5, origin: 1 }]);
        // But a whitespace run at the head of an AI insertion belongs to the insertion.
        let r3 = Map::uniform(2, 1).rebase("a\n", "a\n\nNew paragraph.\n", 9);
        assert_eq!(r3.spans, vec![Span { len: 2, origin: 1 }, Span { len: 16, origin: 9 }]);
    }

    #[test]
    fn rebase_of_unrelated_old_map_length_starts_unknown() {
        let r = Map::uniform(3, 1).rebase("hello", "hello world", 9);
        assert_eq!(r.spans, vec![Span { len: 5, origin: UNKNOWN }, Span { len: 6, origin: 9 }]);
    }

    #[test]
    fn tokens_split_words_spaces_and_marks() {
        assert_eq!(tokens("\\emph{a b}\n"), vec!["\\", "emph", "{", "a", " ", "b", "}", "\n"]);
        assert_eq!(tokens(""), Vec::<&str>::new());
    }

    #[test]
    fn set_and_json_round_trip() {
        let mut m = Map::uniform(8, 9);
        m.set(2, 5, 1);
        let j = m.to_json();
        assert_eq!(j.to_string(), "[[2,9],[3,1],[3,9]]");
        assert_eq!(Map::from_json(&j).unwrap(), m);
        assert_eq!(m.clone().fitted("12345678"), m);
        assert_eq!(m.fitted("123"), Map::uniform(3, UNKNOWN));
    }
}
