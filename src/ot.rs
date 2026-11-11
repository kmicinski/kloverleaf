//! Operational transformation for concurrent editing — the ot.js model.
//!
//! An [`Op`] walks a whole document: retain `n` UTF-16 units, insert a
//! string, delete `n` units. Two people who both edited revision `r` send ops
//! against `r`; the server applies the first, then `transform`s the second
//! past it, so both survive and every replica converges. The browser runs the
//! same algorithm (`web/src/ot.ts`, kept in step with this file) for its own
//! not-yet-acknowledged edits. On the wire an op is ot.js's JSON: a positive
//! number retains, a negative number deletes, a string inserts.

use crate::text::Edit;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum C {
    Retain(usize),
    Insert(String),
    Delete(usize),
}

fn len16(s: &str) -> usize {
    s.encode_utf16().count()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Op {
    pub ops: Vec<C>,
    /// Length of the document this op applies to, and of the one it produces.
    pub base: usize,
    pub target: usize,
}

impl Op {
    pub fn retain(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.base += n;
        self.target += n;
        if let Some(C::Retain(m)) = self.ops.last_mut() {
            *m += n;
        } else {
            self.ops.push(C::Retain(n));
        }
    }

    pub fn insert(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.target += len16(s);
        // Insert before a trailing delete, so equal ops have one spelling.
        match self.ops.as_mut_slice() {
            [.., C::Insert(t)] => t.push_str(s),
            [.., C::Insert(t), C::Delete(_)] => t.push_str(s),
            [.., C::Delete(_)] => {
                let d = self.ops.pop().unwrap();
                self.ops.push(C::Insert(s.into()));
                self.ops.push(d);
            }
            _ => self.ops.push(C::Insert(s.into())),
        }
    }

    pub fn delete(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.base += n;
        if let Some(C::Delete(m)) = self.ops.last_mut() {
            *m += n;
        } else {
            self.ops.push(C::Delete(n));
        }
    }

    pub fn is_noop(&self) -> bool {
        self.ops.iter().all(|c| matches!(c, C::Retain(_)))
    }

    /// A Monaco batch (offsets relative to the text before it, any order,
    /// never overlapping) on a document of `len` units.
    pub fn from_edits(edits: &[Edit], len: usize) -> Result<Op, &'static str> {
        let mut sorted: Vec<&Edit> = edits.iter().collect();
        sorted.sort_by_key(|e| e.offset);
        let mut op = Op::default();
        let mut pos = 0;
        for e in sorted {
            if e.offset < pos || e.offset + e.length > len {
                return Err("edits overlap or run past the end");
            }
            op.retain(e.offset - pos);
            op.delete(e.length);
            op.insert(&e.text);
            pos = e.offset + e.length;
        }
        op.retain(len - pos);
        Ok(op)
    }

    /// The op as replacements relative to the text before it, last first —
    /// the order `text::apply` and `provenance::Map::apply` take them in.
    pub fn to_edits(&self) -> Vec<Edit> {
        let mut out: Vec<Edit> = Vec::new();
        let mut pos = 0;
        for c in &self.ops {
            match c {
                C::Retain(n) => pos += n,
                C::Insert(s) => match out.last_mut() {
                    Some(e) if e.offset + e.length == pos => e.text.push_str(s),
                    _ => out.push(Edit { offset: pos, length: 0, text: s.clone() }),
                },
                C::Delete(n) => {
                    match out.last_mut() {
                        Some(e) if e.offset + e.length == pos => e.length += n,
                        _ => out.push(Edit { offset: pos, length: *n, text: String::new() }),
                    }
                    pos += n;
                }
            }
        }
        out.reverse();
        out
    }

    /// The smallest single replacement taking `old` to `new` (common prefix
    /// and suffix kept) — how a whole-text replace travels as an op, so
    /// concurrent typing elsewhere in the file survives it.
    pub fn diff(old: &str, new: &str) -> Op {
        let (a, b): (Vec<u16>, Vec<u16>) = (old.encode_utf16().collect(), new.encode_utf16().collect());
        let mut pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
        // Never split a surrogate pair.
        if pre > 0 && (0xD800..0xDC00).contains(&a[pre - 1]) {
            pre -= 1;
        }
        let max_suf = a.len().min(b.len()) - pre;
        let mut suf = a.iter().rev().zip(b.iter().rev()).take(max_suf).take_while(|(x, y)| x == y).count();
        if suf > 0 && (0xDC00..0xE000).contains(&a[a.len() - suf]) {
            suf -= 1;
        }
        let mut op = Op::default();
        op.retain(pre);
        op.delete(a.len() - pre - suf);
        op.insert(&String::from_utf16_lossy(&b[pre..b.len() - suf]));
        op.retain(suf);
        op
    }

    pub fn to_json(&self) -> Value {
        Value::Array(
            self.ops
                .iter()
                .map(|c| match c {
                    C::Retain(n) => Value::from(*n as i64),
                    C::Delete(n) => Value::from(-(*n as i64)),
                    C::Insert(s) => Value::from(s.as_str()),
                })
                .collect(),
        )
    }

    pub fn from_json(v: &Value) -> Result<Op, &'static str> {
        let mut op = Op::default();
        for c in v.as_array().ok_or("an op is an array")? {
            match c {
                Value::String(s) => op.insert(s),
                Value::Number(n) => match n.as_i64().ok_or("bad number in op")? {
                    n if n > 0 => op.retain(n as usize),
                    n if n < 0 => op.delete((-n) as usize),
                    _ => return Err("zero in op"),
                },
                _ => return Err("bad component in op"),
            }
        }
        Ok(op)
    }
}

/// `(a', b')` for two ops on the same document: `apply(apply(d, a), b') ==
/// apply(apply(d, b), a')`. At the same position `a`'s insert goes first.
pub fn transform(a: &Op, b: &Op) -> Result<(Op, Op), &'static str> {
    if a.base != b.base {
        return Err("ops are not on the same document");
    }
    let (mut a1, mut b1) = (Op::default(), Op::default());
    let (mut ia, mut ib) = (a.ops.iter().cloned(), b.ops.iter().cloned());
    let (mut x, mut y) = (ia.next(), ib.next());
    loop {
        if let Some(C::Insert(s)) = &x {
            a1.insert(s);
            b1.retain(len16(s));
            x = ia.next();
            continue;
        }
        if let Some(C::Insert(s)) = &y {
            a1.retain(len16(s));
            b1.insert(s);
            y = ib.next();
            continue;
        }
        let (Some(p), Some(q)) = (x.clone(), y.clone()) else {
            if x.is_none() && y.is_none() {
                break;
            }
            return Err("ops are not on the same document");
        };
        let (n, m) = match (&p, &q) {
            (C::Retain(n) | C::Delete(n), C::Retain(m) | C::Delete(m)) => (*n, *m),
            _ => unreachable!(),
        };
        let k = n.min(m);
        match (&p, &q) {
            (C::Retain(_), C::Retain(_)) => {
                a1.retain(k);
                b1.retain(k);
            }
            (C::Delete(_), C::Delete(_)) => {}
            (C::Delete(_), C::Retain(_)) => a1.delete(k),
            (C::Retain(_), C::Delete(_)) => b1.delete(k),
            _ => unreachable!(),
        }
        let rest = |c: &C, left: usize| match c {
            C::Retain(_) => C::Retain(left),
            _ => C::Delete(left),
        };
        x = if n > k { Some(rest(&p, n - k)) } else { ia.next() };
        y = if m > k { Some(rest(&q, m - k)) } else { ib.next() };
    }
    Ok((a1, b1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text;

    fn apply(s: &str, op: &Op) -> String {
        assert_eq!(op.base, len16(s));
        text::apply(s, &op.to_edits())
    }

    fn edit(offset: usize, length: usize, t: &str) -> Edit {
        Edit { offset, length, text: t.into() }
    }

    #[test]
    fn edits_round_trip_through_ops() {
        let s = "hello world";
        let edits = vec![edit(6, 5, "there"), edit(0, 5, "hi")];
        let op = Op::from_edits(&edits, len16(s)).unwrap();
        assert_eq!(apply(s, &op), "hi there");
        assert_eq!(text::apply(s, &edits), "hi there");
        assert_eq!(Op::from_json(&op.to_json()).unwrap(), op);
        assert!(Op::from_edits(&[edit(3, 20, "")], len16(s)).is_err());
    }

    #[test]
    fn concurrent_ops_converge() {
        let s = "The quick brown fox jumps";
        let cases: Vec<(Vec<Edit>, Vec<Edit>)> = vec![
            (vec![edit(4, 0, "very ")], vec![edit(10, 0, "red-")]),
            (vec![edit(4, 0, "A")], vec![edit(4, 0, "B")]),
            (vec![edit(2, 6, "")], vec![edit(5, 8, "")]),
            (vec![edit(2, 6, "X")], vec![edit(5, 8, "Y")]),
            (vec![edit(0, 25, "gone")], vec![edit(10, 0, "kept ")]),
            (vec![edit(20, 5, "runs"), edit(0, 3, "A")], vec![edit(4, 5, "slow")]),
        ];
        for (ea, eb) in cases {
            let a = Op::from_edits(&ea, len16(s)).unwrap();
            let b = Op::from_edits(&eb, len16(s)).unwrap();
            let (a1, b1) = transform(&a, &b).unwrap();
            assert_eq!(apply(&apply(s, &a), &b1), apply(&apply(s, &b), &a1), "{ea:?} vs {eb:?}");
        }
        // Ties: the first op's insert goes first.
        let (a, b) = (Op::from_edits(&[edit(4, 0, "A")], 25).unwrap(), Op::from_edits(&[edit(4, 0, "B")], 25).unwrap());
        let (_, b1) = transform(&a, &b).unwrap();
        assert_eq!(apply(&apply(s, &a), &b1), "The ABquick brown fox jumps");
    }

    #[test]
    fn random_concurrent_ops_converge() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % (n as u64).max(1)) as usize
        };
        let words = ["a", "bc", "XYZ", "\n", ""];
        for _ in 0..5000 {
            let s: String = "abcdefghijklm".chars().take(rnd(14)).collect();
            let len = s.len();
            let mut ops = Vec::new();
            for _ in 0..2 {
                let mut edits = Vec::new();
                let mut pos = 0;
                while pos < len && edits.len() < 4 {
                    let off = pos + rnd(len - pos + 1);
                    let l = rnd((len - off).min(4) + 1);
                    edits.push(edit(off, l, &format!("{}{}", words[rnd(5)], words[rnd(5)])));
                    pos = off + l + 1;
                }
                ops.push(Op::from_edits(&edits, len).unwrap());
            }
            let (a1, b1) = transform(&ops[0], &ops[1]).unwrap();
            assert_eq!(apply(&apply(&s, &ops[0]), &b1), apply(&apply(&s, &ops[1]), &a1), "{s:?} {ops:?}");
            assert_eq!(apply(&s, &Op::diff(&s, &apply(&s, &ops[0]))), apply(&s, &ops[0]));
        }
    }

    #[test]
    fn diff_is_one_minimal_replacement() {
        let op = Op::diff("abc😀def", "abc😀XYdef");
        assert_eq!(op.to_edits(), vec![edit(5, 0, "XY")]);
        assert_eq!(apply("abc😀def", &op), "abc😀XYdef");
        assert!(Op::diff("same", "same").is_noop());
        assert_eq!(apply("aaa", &Op::diff("aaa", "aa")), "aa");
        assert_eq!(apply("x😀y", &Op::diff("x😀y", "x😁y")), "x😁y");
    }
}
