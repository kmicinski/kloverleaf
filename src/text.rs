//! Text primitives shared by the live documents, the history store and the
//! AI module: Monaco-style deltas (UTF-16 offsets), the hash both sides of
//! the wire agree on, and line helpers.

use serde::{Deserialize, Serialize};

/// One replacement, as Monaco reports it: `length` UTF-16 units at `offset`
/// (both relative to the text *before* the whole batch) become `text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edit {
    pub offset: usize,
    pub length: usize,
    pub text: String,
}

/// Byte index of the `utf16`-th UTF-16 code unit.
pub fn byte_at(s: &str, utf16: usize) -> usize {
    let mut n = 0;
    for (i, c) in s.char_indices() {
        if n >= utf16 {
            return i;
        }
        n += c.len_utf16();
    }
    s.len()
}

/// Applies a batch of edits. Monaco reports a batch with later changes first,
/// each against the original text, so applying them in order is correct as
/// long as they do not overlap (they never do).
pub fn apply(text: &str, changes: &[Edit]) -> String {
    let mut text = text.to_string();
    for c in changes {
        let a = byte_at(&text, c.offset);
        let b = a + byte_at(&text[a..], c.length);
        text.replace_range(a..b, &c.text);
    }
    text
}

/// 32-bit FNV-1a over Unicode scalar values; `fnv1a` in `web/src/protocol.ts` must match.
pub fn fnv1a(s: &str) -> u32 {
    s.chars()
        .fold(0x811c9dc5u32, |h, c| (h ^ c as u32).wrapping_mul(0x01000193))
}

/// Lines `from..=to` (1-based, clamped) with their numbers, the way the AI reads files.
pub fn numbered(text: &str, from: usize, to: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let from = from.max(1);
    let to = to.min(lines.len());
    let mut out = String::new();
    for i in from..=to {
        out.push_str(&format!("{i:>5}| {}\n", lines[i - 1]));
    }
    out
}

/// HTML-escapes the five characters that matter in text nodes and attributes.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets() {
        let s = "a😀b"; // the emoji is one char, two UTF-16 units, four bytes
        assert_eq!(byte_at(s, 0), 0);
        assert_eq!(byte_at(s, 1), 1);
        assert_eq!(byte_at(s, 3), 5);
        assert_eq!(byte_at(s, 9), s.len());
    }

    #[test]
    fn fnv_matches_reference_values() {
        assert_eq!(fnv1a(""), 0x811c9dc5);
        assert_eq!(fnv1a("a"), 0xe40c292c);
        assert_eq!(fnv1a("foobar"), 0xbf9cf968);
    }

    #[test]
    fn batches_apply_later_changes_first() {
        let changes = vec![
            Edit { offset: 6, length: 5, text: "there".into() },
            Edit { offset: 0, length: 5, text: "hi".into() },
        ];
        assert_eq!(apply("hello world", &changes), "hi there");
    }

    #[test]
    fn numbered_clamps() {
        assert_eq!(numbered("a\nb\nc", 2, 10), "    2| b\n    3| c\n");
        assert_eq!(numbered("a", 0, 0), "");
    }
}
