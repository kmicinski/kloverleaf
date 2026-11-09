// Who wrote what, in colour. The server keeps a provenance map per file —
// run-length spans, each naming an origin (a person, or one AI apply with its
// prompt and the version it addressed, or a git push / restore / upload) —
// and this module mirrors it: it applies the same deltas the editor sends and
// receives (the algorithm matches src/provenance.rs; keep them equal), paints
// the runs as inline decorations, answers hovers, and on a click opens the
// origin's story: the prompt, with a way to travel back to the text it was
// asked about. Nothing here touches the document itself.

import type { Edit, Origin, ProvPayload } from "./protocol.js";
import { api, me, postJson, when } from "./protocol.js";

export interface Span { len: number; origin: number }

export class ProvMap {
  spans: Span[] = [];

  static from(json: [number, number][]): ProvMap {
    const m = new ProvMap();
    m.spans = json.map(([len, origin]) => ({ len, origin }));
    m.normalize();
    return m;
  }

  length(): number { return this.spans.reduce((n, s) => n + s.len, 0); }

  normalize(): void {
    const out: Span[] = [];
    for (const s of this.spans) {
      if (s.len === 0) continue;
      const last = out[out.length - 1];
      if (last && last.origin === s.origin) last.len += s.len; else out.push({ ...s });
    }
    this.spans = out;
  }

  slice(from: number, to: number): Span[] {
    const out: Span[] = [];
    let pos = 0;
    for (const s of this.spans) {
      const a = pos, b = pos + s.len;
      pos = b;
      if (b <= from) continue;
      if (a >= to) break;
      out.push({ len: Math.min(b, to) - Math.max(a, from), origin: s.origin });
    }
    return out;
  }

  private splice(at: number, remove: number, insert?: Span): void {
    const total = this.length();
    at = Math.min(at, total);
    const end = Math.min(at + remove, total);
    const out = this.slice(0, at);
    if (insert) out.push(insert);
    out.push(...this.slice(end, total));
    this.spans = out;
    this.normalize();
  }

  /** One editor batch: each edit against the text before the batch, later edits first (Monaco's order). */
  apply(changes: Edit[], origin: number): void {
    for (const c of changes) {
      const n = c.text.length; // UTF-16 units, like Monaco offsets
      this.splice(c.offset, c.length, n > 0 ? { len: n, origin } : undefined);
    }
  }

  /** Reassigns `from..to` to `origin` — "this is mine now". */
  set(from: number, to: number, origin: number): void {
    to = Math.min(to, this.length());
    if (from < to) this.splice(from, to - from, { len: to - from, origin });
  }

  /** The span under `offset`, with its bounds. */
  at(offset: number): { span: Span; start: number; end: number } | null {
    let pos = 0;
    for (const s of this.spans) {
      if (offset < pos + s.len) return { span: s, start: pos, end: pos + s.len };
      pos += s.len;
    }
    return null;
  }
}

export interface ProvHost {
  editor(): monaco.editor.IStandaloneCodeEditor;
  model(file: string): monaco.editor.ITextModel | undefined;
  fileOf(model: monaco.editor.ITextModel): string | undefined;
  /** Time-travel the editor to a history entry (opens the drawer). */
  travelTo(entry: number): void;
  status(s: string): void;
}

const maps = new Map<string, ProvMap>();
const origins = new Map<number, Origin>();
const collections = new Map<string, monaco.editor.IEditorDecorationsCollection>();
let host: ProvHost;
let myOrigin = 0;
let hidden = false;
let refreshTimer: number | undefined;
let pop: HTMLElement | null = null;

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

export function initProvenance(h: ProvHost): void {
  host = h;
  try { hidden = localStorage.getItem("prov-hidden") === "1"; } catch { /* private mode */ }
  document.body.classList.toggle("prov-hidden", hidden);
  $("prov-stat").addEventListener("click", () => {
    hidden = !hidden;
    document.body.classList.toggle("prov-hidden", hidden);
    try { localStorage.setItem("prov-hidden", hidden ? "1" : "0"); } catch { /* ignore */ }
    renderStat();
  });
  for (const lang of ["latex", "markdown", "plaintext", "json"]) {
    monaco.languages.registerHoverProvider(lang, { provideHover: (model, position) => hover(model, position) });
  }
  const ed = host.editor();
  ed.onMouseDown((e) => {
    if (!e.target.position || e.event.rightButton) { closePop(); return; }
    const model = ed.getModel();
    const file = model && host.fileOf(model);
    if (!model || !file) return;
    const hit = maps.get(file)?.at(model.getOffsetAt(e.target.position));
    const o = hit && origins.get(hit.span.origin);
    if (hit && o && o.kind !== "human") openPop(file, hit, o, e.event.posx, e.event.posy); else closePop();
  });
  ed.onDidChangeModel(() => { closePop(); refresh(); });
  document.addEventListener("keydown", (e) => { if (e.key === "Escape" && pop) closePop(); });
}

export function setMyOrigin(id: number): void { myOrigin = id; }

/** The server's map for a file (on open, after a replacement, after a claim). */
export function setMap(file: string, p: ProvPayload | null | undefined): void {
  if (!p) return;
  maps.set(file, ProvMap.from(p.spans));
  for (const [id, o] of Object.entries(p.origins)) origins.set(Number(id), o);
  refresh(file);
}

/** My own keystrokes, before they are sent. */
export function applyLocal(file: string, changes: Edit[]): void {
  maps.get(file)?.apply(changes, myOrigin);
  refresh(file);
}

/** Someone else's (or the API's) delta, as broadcast. */
export function applyRemote(file: string, changes: Edit[], origin: number | undefined, user: string | undefined): void {
  const m = maps.get(file);
  if (!m) return;
  const id = origin ?? 0;
  m.apply(changes, id);
  if (id && !origins.has(id)) {
    origins.set(id, { id, kind: "human", user: user ?? "?", ts: Date.now(), meta: {} });
    void fetchOrigins([id]);
  }
  refresh(file);
}

async function fetchOrigins(ids: number[]): Promise<void> {
  const got = await api<Origin[]>(`/provenance/origins?ids=${ids.join(",")}`).catch(() => [] as Origin[]);
  for (const o of got) origins.set(o.id, o);
  refresh();
}

/** Re-attributes the editor's selection (or the span under the cursor) to me. */
export async function claimSelection(): Promise<void> {
  const ed = host.editor();
  const model = ed.getModel();
  const file = model && host.fileOf(model);
  if (!model || !file) return;
  const sel = ed.getSelection();
  let from: number, to: number;
  if (sel && !sel.isEmpty()) {
    from = model.getOffsetAt(sel.getStartPosition()); to = model.getOffsetAt(sel.getEndPosition());
  } else {
    const pos = ed.getPosition();
    const hit = pos && maps.get(file)?.at(model.getOffsetAt(pos));
    if (!hit) return;
    from = hit.start; to = hit.end;
  }
  await claim(file, from, to);
}

async function claim(file: string, from: number, to: number): Promise<void> {
  maps.get(file)?.set(from, to, myOrigin);
  refresh(file);
  await postJson("/provenance/claim", { file, from, to })
    .then(() => host.status("marked as yours"), (e) => host.status(`claim failed: ${String(e).slice(0, 80)}`));
}

function classOf(o: Origin | undefined): string | null {
  if (!o || o.id === 0) return null;
  switch (o.kind) {
    case "ai": return "prov-ai";
    case "git": return "prov-git";
    case "restore": case "upload": return "prov-import";
    default: return o.user === me ? null : "prov-other";
  }
}

/** Rebuilds the decorations of the model in the editor (debounced). */
export function refresh(file?: string): void {
  window.clearTimeout(refreshTimer);
  refreshTimer = window.setTimeout(() => {
    const ed = host.editor();
    const model = ed.getModel();
    const current = model && host.fileOf(model);
    if (!model || !current || (file && file !== current)) { renderStat(); return; }
    const m = maps.get(current);
    if (!m) return;
    const decorations: monaco.editor.IModelDeltaDecoration[] = [];
    let pos = 0;
    for (const s of m.spans) {
      const cls = classOf(origins.get(s.origin));
      if (cls) {
        const a = model.getPositionAt(pos), b = model.getPositionAt(pos + s.len);
        decorations.push({ range: new monaco.Range(a.lineNumber, a.column, b.lineNumber, b.column), options: { inlineClassName: cls, stickiness: monaco.editor.TrackedRangeStickiness.NeverGrowsWhenTypingAtEdges } });
      }
      pos += s.len;
    }
    let coll = collections.get(current);
    if (!coll) { coll = ed.createDecorationsCollection(); collections.set(current, coll); }
    coll.set(decorations);
    renderStat();
  }, 60);
}

/** Share of a line range written by the AI (0..1), or null when unknown — for per-section badges. */
export function aiShare(file: string, fromLine: number, toLine: number): number | null {
  const m = maps.get(file), model = host.model(file);
  if (!m || !model) return null;
  const a = model.getOffsetAt({ lineNumber: Math.max(1, fromLine), column: 1 });
  const b = model.getOffsetAt({ lineNumber: Math.min(toLine, model.getLineCount()), column: model.getLineMaxColumn(Math.min(toLine, model.getLineCount())) });
  if (b <= a) return null;
  let ai = 0;
  for (const s of m.slice(a, b)) if (origins.get(s.origin)?.kind === "ai") ai += s.len;
  return ai / (b - a);
}

/** The header's "AI 23%" — the share of the open file the AI wrote (click hides the tint). */
function renderStat(): void {
  const el = $("prov-stat");
  const ed = host.editor();
  const model = ed.getModel();
  const file = model && host.fileOf(model);
  const m = file && maps.get(file);
  if (!m || m.length() === 0) { el.textContent = ""; return; }
  const total = m.length();
  let ai = 0, imported = 0;
  const people = new Map<string, number>();
  for (const s of m.spans) {
    const o = origins.get(s.origin);
    if (!o || o.id === 0) continue;
    if (o.kind === "ai") ai += s.len;
    else if (o.kind !== "human") imported += s.len;
    else if (o.user !== me) people.set(o.user, (people.get(o.user) ?? 0) + s.len);
  }
  const pct = (n: number) => `${Math.round((100 * n) / total)}%`;
  const parts = [`${hidden ? "◌" : "●"} AI ${pct(ai)}`];
  for (const [user, n] of [...people].sort((a, b) => b[1] - a[1]).slice(0, 3)) parts.push(`${user} ${pct(n)}`);
  if (imported) parts.push(`imported ${pct(imported)}`);
  el.textContent = parts.join(" · ");
  el.title = hidden ? "provenance colours hidden — click to show" : "share of this file written by the AI — click to hide the colours";
  el.classList.toggle("off", hidden);
}

/** One line about a non-human origin: what it was, who, when. */
function describe(o: Origin): string {
  const what = o.kind === "ai" ? `✦ AI · ${String(o.meta.model ?? "")}`
    : o.kind === "git" ? `pushed over git · ${o.user}`
    : o.kind === "restore" ? `restored from history · ${o.user}`
    : o.kind === "upload" ? `uploaded · ${o.user}`
    : `${o.kind} · ${o.user}`;
  return `${what} · ${when(o.ts)}`;
}

function hover(model: monaco.editor.ITextModel, position: monaco.Position): monaco.languages.Hover | null {
  if (hidden) return null;
  const file = host.fileOf(model);
  const hit = file && maps.get(file)?.at(model.getOffsetAt(position));
  if (!hit) return null;
  const o = origins.get(hit.span.origin);
  const a = model.getPositionAt(hit.start), b = model.getPositionAt(hit.end);
  const range = new monaco.Range(a.lineNumber, a.column, b.lineNumber, b.column);
  let md: string;
  if (!o || o.id === 0) md = "_written before provenance tracking_";
  else if (o.kind === "human") md = o.user === me ? "**written by you**" : `**written by ${o.user}**`;
  else if (o.kind === "ai") {
    const prompt = String(o.meta.prompt ?? "").replace(/\s+/g, " ");
    md = `**${describe(o)}**\n\n> ${prompt.length > 240 ? prompt.slice(0, 240) + "…" : prompt}\n\n_click for the prompt and the version it addressed · ⌘⇧M makes it yours_`;
  } else md = `**${describe(o)}**`;
  return { range, contents: [{ value: md }] };
}

function openPop(file: string, hit: { span: Span; start: number; end: number }, o: Origin, x: number, y: number): void {
  closePop();
  pop = document.createElement("div");
  pop.id = "prov-pop";
  const head = document.createElement("div");
  head.className = "pop-head";
  head.textContent = describe(o);
  pop.appendChild(head);
  if (o.kind === "ai" && o.meta.prompt) {
    const q = document.createElement("blockquote");
    q.textContent = String(o.meta.prompt);
    pop.appendChild(q);
  }
  const row = document.createElement("div");
  row.className = "row";
  const base = typeof o.meta.base === "number" ? o.meta.base : null;
  const entry = typeof o.meta.entry === "number" ? o.meta.entry : null;
  if (base !== null) {
    const b = document.createElement("button");
    b.className = "small";
    b.textContent = "the text it was asked about";
    b.title = "time-travel the editor to the version this prompt addressed";
    b.addEventListener("click", () => { closePop(); host.travelTo(base); });
    row.appendChild(b);
  }
  if (entry !== null) {
    const b = document.createElement("button");
    b.className = "small";
    b.textContent = "as it was applied";
    b.addEventListener("click", () => { closePop(); host.travelTo(entry); });
    row.appendChild(b);
  }
  const mine = document.createElement("button");
  mine.className = "small";
  mine.textContent = "make this mine";
  mine.title = "re-attribute this run of text to you";
  mine.addEventListener("click", () => { closePop(); void claim(file, hit.start, hit.end); });
  row.appendChild(mine);
  const x2 = document.createElement("button");
  x2.className = "tiny";
  x2.textContent = "×";
  x2.addEventListener("click", closePop);
  row.appendChild(x2);
  pop.appendChild(row);
  document.body.appendChild(pop);
  const w = pop.offsetWidth, h = pop.offsetHeight;
  pop.style.left = `${Math.min(x + 8, window.innerWidth - w - 8)}px`;
  pop.style.top = `${Math.min(y + 12, window.innerHeight - h - 8)}px`;
  setTimeout(() => document.addEventListener("mousedown", outside, { once: true }), 0);
}

function outside(e: MouseEvent): void {
  if (pop && !pop.contains(e.target as Node)) closePop();
  else if (pop) setTimeout(() => document.addEventListener("mousedown", outside, { once: true }), 0);
}

function closePop(): void {
  pop?.remove();
  pop = null;
}
