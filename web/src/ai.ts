// The AI panel: what the model is told (cursor, selection, queued regions,
// the last click in the PDF), the prompt, the run's activity, and the
// proposal — a diff per file in an editable Monaco diff editor — with apply
// and discard. Follow-ups continue the same session. Protocol in src/ai/mod.rs.

import type { AiEvent, AiMode, ClientMsg, Context, PdfClick, ProposedFile, Region } from "./protocol.js";
import { me } from "./protocol.js";

export interface AiHost {
  send(m: ClientMsg): void;
  /** Current cursor and selection, from the editor. */
  context(): { file?: string; line?: number; column?: number; selection?: Region };
  /** Text of a file as the editor has it, to diff proposals against. */
  currentText(file: string): string | undefined;
  open(file: string, line?: number): void;
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const panel = $("ai-panel");
const promptEl = $<HTMLTextAreaElement>("ai-prompt");
const logEl = $("ai-log");
const proposalEl = $("ai-proposal");
const statusEl = $("ai-status");
const queueEl = $("ai-queue");
const contextEl = $("ai-context");
const runBtn = $<HTMLButtonElement>("ai-run");
const cancelBtn = $<HTMLButtonElement>("ai-cancel");

let host: AiHost;
let mode: AiMode = { mode: "off" };
let queue: Region[] = [];
let lastPdf: PdfClick | undefined;
let session: number | undefined;
let running = false;
let diffEditors: { path: string; editor: monaco.editor.IStandaloneDiffEditor }[] = [];

export function initAi(h: AiHost): void {
  host = h;
  runBtn.addEventListener("click", run);
  cancelBtn.addEventListener("click", () => { if (session !== undefined) host.send({ type: "ai", action: "cancel", session }); });
  $("ai-close").addEventListener("click", () => toggleAi(false));
  promptEl.addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); run(); }
  });
  renderQueue();
  renderContext();
}

export function setMode(m: AiMode): void {
  mode = m;
  const el = $("ai-mode");
  el.textContent = m.mode === "off" ? `off — ${m.reason ?? ""}` : m.mode === "dual" ? `dual: ${m.model} + ${m.local}` : `${m.mode}: ${m.model}`;
  runBtn.disabled = m.mode === "off";
}

export function toggleAi(show?: boolean): void {
  const on = show ?? panel.hidden;
  panel.hidden = !on;
  $("btn-ai").classList.toggle("on", on);
  document.body.classList.toggle("ai-open", on);
  if (on) { renderContext(); promptEl.focus(); }
}

export function isOpen(): boolean { return !panel.hidden; }

/** Adds the editor's selection (or the cursor's line) to the queue. */
export function queueSelection(): void {
  const c = host.context();
  const r = c.selection ?? (c.file && c.line ? { file: c.file, from: c.line, to: c.line, text: "" } : undefined);
  if (!r) return;
  queue.push(r);
  renderQueue();
  toggleAi(true);
}

export function setPdfClick(p: PdfClick): void { lastPdf = p; renderContext(); }

/** Called by the editor whenever the cursor moves (cheap: text only). */
export function renderContext(): void {
  if (panel.hidden || !host) return;
  const c = host.context();
  const parts: string[] = [];
  if (c.file && c.line) parts.push(`cursor ${c.file}:${c.line}`);
  if (c.selection) parts.push(`selection ${c.selection.from}–${c.selection.to}`);
  if (lastPdf) parts.push(`pdf p.${lastPdf.page}${lastPdf.line ? ` → ${lastPdf.file}:${lastPdf.line}` : ""}`);
  contextEl.textContent = parts.length ? `context: ${parts.join(" · ")}` : "context: (none)";
}

function renderQueue(): void {
  queueEl.replaceChildren();
  if (queue.length === 0) return;
  const title = document.createElement("div");
  title.className = "muted";
  title.textContent = `queued for attention (${queue.length}):`;
  queueEl.appendChild(title);
  queue.forEach((r, i) => {
    const row = document.createElement("div");
    row.className = "queue-item";
    const link = document.createElement("a");
    link.href = "#";
    link.textContent = `${r.file}:${r.from}${r.to !== r.from ? `–${r.to}` : ""}`;
    link.addEventListener("click", (e) => { e.preventDefault(); host.open(r.file, r.from); });
    const note = document.createElement("input");
    note.placeholder = "note (optional)";
    note.value = r.note ?? "";
    note.addEventListener("input", () => { r.note = note.value || undefined; });
    const x = document.createElement("button");
    x.className = "tiny";
    x.textContent = "×";
    x.addEventListener("click", () => { queue.splice(i, 1); renderQueue(); });
    row.append(link, note, x);
    queueEl.appendChild(row);
  });
}

function context(): Context {
  const c = host.context();
  return { file: c.file, line: c.line, column: c.column, selection: c.selection, queue: queue.slice(), pdf: lastPdf };
}

function run(): void {
  const prompt = promptEl.value.trim();
  if (!prompt || running || mode.mode === "off") return;
  logEl.replaceChildren();
  host.send({ type: "ai", action: "run", prompt, context: context(), session });
  promptEl.value = "";
  setRunning(true);
}

function setRunning(on: boolean): void {
  running = on;
  runBtn.disabled = on || mode.mode === "off";
  cancelBtn.hidden = !on;
  statusEl.textContent = on ? "running…" : "";
}

function log(kind: string, text: string, role?: string): void {
  const el = document.createElement("div");
  el.className = `ai-entry ${kind}`;
  if (role && role !== "assistant") {
    const tag = document.createElement("span");
    tag.className = "role";
    tag.textContent = role;
    el.appendChild(tag);
  }
  el.appendChild(document.createTextNode(text));
  logEl.appendChild(el);
  logEl.scrollTop = logEl.scrollHeight;
}

/** Events from the socket; ones from other people's sessions are shown as a line only. */
export function aiEvent(ev: AiEvent): void {
  if (ev.user !== me) {
    if (ev.event === "started") log("muted", `${ev.user} asked the AI: ${ev.prompt}`);
    if (ev.event === "applied") log("muted", `${ev.user} applied AI changes to ${ev.files.join(", ")}`);
    return;
  }
  switch (ev.event) {
    case "started":
      session = ev.session;
      log("prompt", `» ${ev.prompt}`);
      break;
    case "turn":
      log("turn", ev.text, ev.role);
      break;
    case "tool": {
      const args = Object.entries(ev.input).map(([k, v]) => `${k}=${typeof v === "string" ? v : JSON.stringify(v)}`).join(" ");
      const line = `${ev.name} ${args} → ${ev.summary}`;
      const last = logEl.lastElementChild as HTMLElement | null;
      if (last?.dataset.pending === ev.name && ev.summary !== "…") { last.textContent = ""; last.dataset.pending = ""; last.appendChild(document.createTextNode(line)); last.classList.toggle("error", !!ev.error); }
      else { log(`tool${ev.error ? " error" : ""}`, line, ev.role); (logEl.lastElementChild as HTMLElement).dataset.pending = ev.summary === "…" ? ev.name : ""; }
      break;
    }
    case "proposal":
      setRunning(false);
      if (ev.text) log("final", ev.text);
      statusEl.textContent = `${ev.model} · ${ev.usage.input}→${ev.usage.output} tokens`;
      showProposal(ev.files);
      break;
    case "error":
      setRunning(false);
      log("error", ev.message);
      break;
    case "cancelled":
      setRunning(false);
      log("muted", "cancelled");
      break;
    case "applied":
      log("muted", `applied: ${ev.files.join(", ") || "nothing changed"}`);
      clearProposal();
      session = undefined;
      queue = [];
      renderQueue();
      break;
    case "discarded":
      clearProposal();
      session = undefined;
      break;
  }
}

function clearProposal(): void {
  for (const d of diffEditors) {
    const m = d.editor.getModel();
    d.editor.dispose();
    m?.original.dispose();
    m?.modified.dispose();
  }
  diffEditors = [];
  proposalEl.replaceChildren();
  proposalEl.hidden = true;
}

function showProposal(files: ProposedFile[]): void {
  clearProposal();
  proposalEl.hidden = false;
  if (files.length === 0) {
    const p = document.createElement("p");
    p.className = "muted";
    p.textContent = "No file changes proposed. Ask a follow-up, or discard.";
    proposalEl.appendChild(p);
  }
  for (const f of files) {
    const head = document.createElement("div");
    head.className = "row proposal-head";
    const current = host.currentText(f.path);
    const stale = current !== undefined && current !== f.before;
    head.textContent = `${f.path}${f.before === "" ? " (new)" : ""}${stale ? " — changed since the AI read it; check the diff" : ""}`;
    const box = document.createElement("div");
    box.className = "diff";
    proposalEl.append(head, box);
    const original = monaco.editor.createModel(f.before, "latex");
    const modified = monaco.editor.createModel(f.after, "latex");
    const editor = monaco.editor.createDiffEditor(box, {
      renderSideBySide: false, automaticLayout: true, originalEditable: false, readOnly: false,
      minimap: { enabled: false }, lineNumbers: "off", fontSize: 12, scrollBeyondLastLine: false,
      renderOverviewRuler: false, folding: false, wordWrap: "on", theme: "solarized-light",
      diffWordWrap: "on", hideUnchangedRegions: { enabled: true, contextLineCount: 3 },
    });
    editor.setModel({ original, modified });
    diffEditors.push({ path: f.path, editor });
  }
  const row = document.createElement("div");
  row.className = "row";
  const apply = document.createElement("button");
  apply.textContent = files.length ? "apply" : "close";
  apply.addEventListener("click", () => {
    if (session === undefined) return;
    if (!files.length) { host.send({ type: "ai", action: "discard", session }); return; }
    host.send({ type: "ai", action: "apply", session, files: diffEditors.map((d) => ({ path: d.path, text: d.editor.getModel()!.modified.getValue() })) });
  });
  const discard = document.createElement("button");
  discard.className = "small";
  discard.textContent = "discard";
  discard.addEventListener("click", () => { if (session !== undefined) host.send({ type: "ai", action: "discard", session }); });
  const hint = document.createElement("span");
  hint.className = "muted";
  hint.textContent = files.length ? "edit the diff, or ask a follow-up above" : "";
  row.append(apply, discard, hint);
  proposalEl.appendChild(row);
}
