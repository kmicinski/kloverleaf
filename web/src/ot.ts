// Operational transformation, the ot.js model — the browser's half of
// src/ot.rs (keep the two in step). An op walks the whole document: a
// positive number retains that many UTF-16 units, a negative one deletes, a
// string inserts. JavaScript string lengths are UTF-16 units already.

import type { Edit } from "./protocol.js";

export type Comp = number | string;

export class Op {
  ops: Comp[] = [];
  base = 0;
  target = 0;

  retain(n: number): this {
    if (n <= 0) return this;
    this.base += n; this.target += n;
    const last = this.ops[this.ops.length - 1];
    if (typeof last === "number" && last > 0) this.ops[this.ops.length - 1] = last + n;
    else this.ops.push(n);
    return this;
  }

  insert(s: string): this {
    if (!s) return this;
    this.target += s.length;
    const ops = this.ops, n = ops.length;
    const last = ops[n - 1], prev = ops[n - 2];
    if (typeof last === "string") ops[n - 1] = last + s;
    else if (typeof last === "number" && last < 0) {
      // Insert before a trailing delete, so equal ops have one spelling.
      if (typeof prev === "string") ops[n - 2] = prev + s;
      else { ops[n - 1] = s; ops.push(last); }
    } else ops.push(s);
    return this;
  }

  delete(n: number): this {
    if (n <= 0) return this;
    this.base += n;
    const last = this.ops[this.ops.length - 1];
    if (typeof last === "number" && last < 0) this.ops[this.ops.length - 1] = last - n;
    else this.ops.push(-n);
    return this;
  }

  isNoop(): boolean { return this.ops.every((c) => typeof c === "number" && c > 0); }

  toJSON(): Comp[] { return this.ops; }

  static fromJSON(a: Comp[]): Op {
    const op = new Op();
    for (const c of a) {
      if (typeof c === "string") op.insert(c);
      else if (c > 0) op.retain(c);
      else op.delete(-c);
    }
    return op;
  }

  /** A Monaco batch (offsets relative to the text before it) on a document of `len` units. */
  static fromEdits(edits: Edit[], len: number): Op {
    const sorted = [...edits].sort((a, b) => a.offset - b.offset);
    const op = new Op();
    let pos = 0;
    for (const e of sorted) {
      op.retain(e.offset - pos).delete(e.length).insert(e.text);
      pos = e.offset + e.length;
    }
    return op.retain(len - pos);
  }

  /** As replacements relative to the text before the op, last first (how Monaco and provenance take them). */
  toEdits(): Edit[] {
    const out: Edit[] = [];
    let pos = 0;
    for (const c of this.ops) {
      const last = out[out.length - 1];
      if (typeof c === "string") {
        if (last && last.offset + last.length === pos) last.text += c;
        else out.push({ offset: pos, length: 0, text: c });
      } else if (c > 0) pos += c;
      else {
        if (last && last.offset + last.length === pos) last.length += -c;
        else out.push({ offset: pos, length: -c, text: "" });
        pos += -c;
      }
    }
    return out.reverse();
  }
}

/** One op after the other, as a single op. */
export function compose(a: Op, b: Op): Op {
  if (a.target !== b.base) throw new Error("compose: ops do not line up");
  const out = new Op();
  const A = [...a.ops], B = [...b.ops];
  let i = 0, j = 0;
  let x: Comp | undefined = A[i++], y: Comp | undefined = B[j++];
  for (;;) {
    if (x === undefined && y === undefined) break;
    if (typeof x === "number" && x < 0) { out.delete(-x); x = A[i++]; continue; }
    if (typeof y === "string") { out.insert(y); y = B[j++]; continue; }
    if (x === undefined || y === undefined) throw new Error("compose: ops do not line up");
    if (typeof x === "number" && typeof y === "number") {
      if (y > 0) {
        const k = Math.min(x, y);
        out.retain(k);
        x = x > k ? x - k : A[i++]; y = y > k ? y - k : B[j++];
      } else {
        const k = Math.min(x, -y);
        out.delete(k);
        x = x > k ? x - k : A[i++]; y = -y > k ? y + k : B[j++];
      }
    } else if (typeof x === "string") {
      if (typeof y === "number" && y > 0) {
        const k = Math.min(x.length, y);
        out.insert(x.slice(0, k));
        x = x.length > k ? x.slice(k) : A[i++]; y = y > k ? y - k : B[j++];
      } else {
        const k = Math.min(x.length, -(y as number));
        x = x.length > k ? x.slice(k) : A[i++]; y = -(y as number) > k ? (y as number) + k : B[j++];
      }
    }
  }
  return out;
}

/** `[a', b']` for two ops on the same document; at the same position `a`'s insert goes first.
 *  The server transforms a page's op as `a` against what it missed, so a page
 *  transforms its own pending op as `a` against the server's too. */
export function transform(a: Op, b: Op): [Op, Op] {
  if (a.base !== b.base) throw new Error("transform: ops are not on the same document");
  const a1 = new Op(), b1 = new Op();
  const A = [...a.ops], B = [...b.ops];
  let i = 0, j = 0;
  let x: Comp | undefined = A[i++], y: Comp | undefined = B[j++];
  for (;;) {
    if (x === undefined && y === undefined) break;
    if (typeof x === "string") { a1.insert(x); b1.retain(x.length); x = A[i++]; continue; }
    if (typeof y === "string") { a1.retain(y.length); b1.insert(y); y = B[j++]; continue; }
    if (x === undefined || y === undefined) throw new Error("transform: ops are not on the same document");
    const n = Math.abs(x), m = Math.abs(y), k = Math.min(n, m);
    if (x > 0 && y > 0) { a1.retain(k); b1.retain(k); }
    else if (x < 0 && y > 0) a1.delete(k);
    else if (x > 0 && y < 0) b1.delete(k);
    x = n > k ? Math.sign(x) * (n - k) : A[i++];
    y = m > k ? Math.sign(y) * (m - k) : B[j++];
  }
  return [a1, b1];
}
