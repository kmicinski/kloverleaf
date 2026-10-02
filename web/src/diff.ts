// A line diff (Myers' O(ND) algorithm with common prefix/suffix trimmed),
// enough to shade what changed between a historical version and now.
// Hunks are 0-based: `aLen` lines at `aStart` in `a` correspond to `bLen`
// lines at `bStart` in `b`; one of the lengths may be 0.

export interface Hunk { aStart: number; aLen: number; bStart: number; bLen: number }

export function lineDiff(a: string[], b: string[]): Hunk[] {
  let pre = 0;
  while (pre < a.length && pre < b.length && a[pre] === b[pre]) pre++;
  let suf = 0;
  while (suf < a.length - pre && suf < b.length - pre && a[a.length - 1 - suf] === b[b.length - 1 - suf]) suf++;
  const A = a.slice(pre, a.length - suf), B = b.slice(pre, b.length - suf);
  const n = A.length, m = B.length;
  if (n === 0 && m === 0) return [];
  if (n === 0 || m === 0) return [{ aStart: pre, aLen: n, bStart: pre, bLen: m }];
  const max = n + m;
  const LIMIT = 3000; // beyond this many edits, "everything changed" is a fine answer
  const off = max;
  const v = new Int32Array(2 * max + 2);
  const trace: Int32Array[] = [];
  outer: for (let d = 0; d <= Math.min(max, LIMIT); d++) {
    trace.push(v.slice());
    for (let k = -d; k <= d; k += 2) {
      let x = k === -d || (k !== d && v[off + k - 1] < v[off + k + 1]) ? v[off + k + 1] : v[off + k - 1] + 1;
      let y = x - k;
      while (x < n && y < m && A[x] === B[y]) { x++; y++; }
      v[off + k] = x;
      if (x >= n && y >= m) break outer;
    }
    if (d === LIMIT) return [{ aStart: pre, aLen: n, bStart: pre, bLen: m }];
  }
  // Walk back through the snapshots to recover the edit script.
  const ops: ("=" | "-" | "+")[] = [];
  let x = n, y = m;
  for (let d = trace.length - 1; d >= 0; d--) {
    const vv = trace[d];
    const k = x - y;
    const prevK = k === -d || (k !== d && vv[off + k - 1] < vv[off + k + 1]) ? k + 1 : k - 1;
    const prevX = vv[off + prevK], prevY = prevX - prevK;
    while (x > prevX && y > prevY) { ops.push("="); x--; y--; }
    if (d > 0) { if (x === prevX) { ops.push("+"); y--; } else { ops.push("-"); x--; } }
  }
  ops.reverse();
  const hunks: Hunk[] = [];
  let ai = pre, bi = pre;
  let cur: Hunk | null = null;
  for (const op of ops) {
    if (op === "=") { cur = null; ai++; bi++; continue; }
    if (!cur) { cur = { aStart: ai, aLen: 0, bStart: bi, bLen: 0 }; hunks.push(cur); }
    if (op === "-") { cur.aLen++; ai++; } else { cur.bLen++; bi++; }
  }
  return hunks;
}
