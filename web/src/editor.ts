// The editor page: Monaco on the left, the PDF on the right, the AI panel
// beside it. One WebSocket carries edits out and the text, forwarded edits,
// build results, lock state and AI events in (protocol in src/live.rs).
// This file wires the pieces: models per file, the file list, the lock, the
// build status and markers, SyncTeX both ways, the history drawer.

import type { Build, ClientMsg, Diagnostic, FileInfo, Lock, Region, ServerMsg } from "./protocol.js";
import { api, fnv1a, me, project, socketUrl } from "./protocol.js";
import { registerLatex } from "./latex.js";
import { PdfViewer, type SyncBox } from "./pdf.js";
import { lineDiff } from "./diff.js";
import * as ai from "./ai.js";

const MONACO = "https://cdnjs.cloudflare.com/ajax/libs/monaco-editor/0.45.0/min/vs";
const EMACS = "https://cdn.jsdelivr.net/npm/monaco-emacs@0.3.0/dist/monaco-emacs";
const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const statusEl = $("status");
const buildEl = $("build");
const status = (s: string) => { statusEl.textContent = s; };

let editor: monaco.editor.IStandaloneCodeEditor;
let ws: WebSocket;
let current: string | null = null;          // the file in the editor
let pendingOpen: { file: string; line?: number } | null = null;
let files: FileInfo[] = [];
let main: string | null = null;
let lock: Lock | null = null;
let lastBuild: Build | null = null;
let applyingRemote = false;
const models = new Map<string, monaco.editor.ITextModel>();
const send = (m: ClientMsg) => { if (ws?.readyState === WebSocket.OPEN) ws.send(JSON.stringify(m)); };
const remote = (f: () => void) => { applyingRemote = true; try { f(); } finally { applyingRemote = false; } };

// ---- editor -----------------------------------------------------------------

require.config({ paths: { vs: MONACO, "monaco-emacs": EMACS } });
require(["vs/editor/editor.main", "monaco-emacs"], (_monaco, emacs) => {
  monaco.editor.defineTheme("solarized-light", {
    base: "vs",
    inherit: true,
    rules: [
      { token: "", foreground: "657b83", background: "fdf6e3" },
      { token: "comment", foreground: "93a1a1", fontStyle: "italic" },
      { token: "keyword", foreground: "859900" },
      { token: "keyword.control", foreground: "cb4b16" },
      { token: "string", foreground: "2aa198" },
      { token: "variable", foreground: "268bd2" },
      { token: "number", foreground: "d33682" },
      { token: "type", foreground: "b58900" },
      { token: "delimiter", foreground: "93a1a1" },
      { token: "operator", foreground: "dc322f" },
      { token: "markup.heading", foreground: "cb4b16", fontStyle: "bold" },
    ],
    colors: {
      "editor.background": "#fdf6e3",
      "editor.foreground": "#657b83",
      "editor.lineHighlightBackground": "#eee8d5",
      "editor.selectionBackground": "#e6dfc6",
      "editorCursor.foreground": "#657b83",
      "editorLineNumber.foreground": "#93a1a1",
      "editorLineNumber.activeForeground": "#657b83",
      "editorIndentGuide.background": "#eee8d5",
      "editorWhitespace.foreground": "#eee8d5",
      "diffEditor.insertedTextBackground": "#85990033",
      "diffEditor.removedTextBackground": "#dc322f33",
    },
  });
  registerLatex();
  editor = monaco.editor.create($("editor"), {
    model: null,
    theme: "solarized-light",
    fontSize: 14,
    fontFamily: '"JetBrains Mono", "SF Mono", Menlo, Consolas, monospace',
    lineHeight: 1.6,
    lineNumbers: "on",
    wordWrap: "on",
    minimap: { enabled: false },
    scrollBeyondLastLine: true,
    automaticLayout: true,
    tabSize: 2,
    insertSpaces: true,
    renderWhitespace: "selection",
    padding: { top: 12, bottom: 12 },
    cursorSmoothCaretAnimation: "on",
    smoothScrolling: true,
    occurrencesHighlight: "off",
    folding: true,
    quickSuggestions: false,
    suggestOnTriggerCharacters: true,
    glyphMargin: true,
  });
  new (emacs as MonacoEmacsModule).EmacsExtension(editor).start();
  editor.onDidChangeCursorPosition(() => ai.renderContext());
  editor.onDidChangeCursorSelection(() => ai.renderContext());
  editor.addCommand(monaco.KeyMod.Alt | monaco.KeyCode.Enter, () => void forwardSync());
  editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyQ, () => ai.queueSelection());
  editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyA, () => ai.toggleAi());
  editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, () => { void api("/compile", { method: "POST" }); });
  ai.initAi({
    send,
    context: editorContext,
    currentText: (file) => models.get(file)?.getValue(),
    open: (file, line) => open(file, line),
  });
  connect();
});

function editorContext(): { file?: string; line?: number; column?: number; selection?: Region } {
  const model = editor.getModel();
  if (!model || !current) return {};
  const pos = editor.getPosition();
  const sel = editor.getSelection();
  const selection = sel && !sel.isEmpty()
    ? { file: current, from: sel.startLineNumber, to: sel.endLineNumber, text: model.getValueInRange(new monaco.Range(sel.startLineNumber, 1, sel.endLineNumber, model.getLineMaxColumn(sel.endLineNumber))) }
    : undefined;
  return { file: current, line: pos?.lineNumber, column: pos?.column, selection };
}

/** Creates the model for a file's text and wires its edits to the socket. */
function model(file: string, text: string): monaco.editor.ITextModel {
  let m = models.get(file);
  if (m) {
    if (fnv1a(m.getValue()) !== fnv1a(text)) {
      const pos = editor.getModel() === m ? editor.getPosition() : null;
      remote(() => m!.pushEditOperations([], [{ range: m!.getFullModelRange(), text }], () => null));
      if (pos) editor.setPosition(pos);
    }
    return m;
  }
  const language = /\.(tex|sty|cls|bib|tikz|ltx|dtx)$/i.test(file) ? "latex" : /\.(md)$/i.test(file) ? "markdown" : /\.json$/i.test(file) ? "json" : "plaintext";
  m = monaco.editor.createModel(text, language, monaco.Uri.parse(`kloverleaf:/${file}`));
  m.onDidChangeContent((e) => {
    if (applyingRemote) return;
    status("editing");
    send({
      type: "edit",
      file,
      changes: e.changes.map((c) => ({ offset: c.rangeOffset, length: c.rangeLength, text: c.text })),
      hash: fnv1a(m!.getValue()),
    });
  });
  models.set(file, m);
  if (lastBuild) markers(lastBuild.diags);
  return m;
}

/** Shows a file in the editor, fetching its text if this page has not seen it. */
function open(file: string, line?: number): void {
  const info = files.find((f) => f.path === file);
  if (info && !info.text) { window.open(`/api/projects/${project}/files/${file}`, "_blank"); return; }
  const m = models.get(file);
  if (!m) { pendingOpen = { file, line }; send({ type: "open", file }); return; }
  if (travel) leaveTravel();
  if (editor.getModel() !== m) editor.setModel(m);
  current = file;
  renderFiles();
  if (line) { editor.revealLineInCenter(line); editor.setPosition({ lineNumber: line, column: 1 }); }
  editor.focus();
  ai.renderContext();
}

// ---- socket -----------------------------------------------------------------

function connect(): void {
  ws = new WebSocket(socketUrl());
  ws.onopen = () => status("connected");
  ws.onclose = () => { status("disconnected, retrying…"); setTimeout(connect, 1000); };
  ws.onmessage = (ev) => handle(JSON.parse(ev.data) as ServerMsg);
}

setInterval(() => send({ type: "ping" }), 20_000);

function handle(m: ServerMsg): void {
  switch (m.type) {
    case "init":
      files = m.files; main = m.main; setLock(m.lock); ai.setMode(m.ai);
      renderFiles();
      if (m.build) built(m.build, true); else if (m.building) buildEl.textContent = "compiling…";
      // Re-sync every model this page already has (after a reconnect), then open the main file.
      for (const file of models.keys()) send({ type: "open", file });
      if (!current) open(main ?? files.find((f) => f.text)?.path ?? "");
      status("synced");
      break;
    case "text": {
      model(m.file, m.text);
      if (pendingOpen?.file === m.file) { const { file, line } = pendingOpen; pendingOpen = null; open(file, line); }
      status("synced");
      break;
    }
    case "edit": {
      const mod = models.get(m.file);
      if (!mod) break;
      remote(() => mod.applyEdits(m.changes.map((c) => {
        const a = mod.getPositionAt(c.offset), b = mod.getPositionAt(c.offset + c.length);
        return { range: new monaco.Range(a.lineNumber, a.column, b.lineNumber, b.column), text: c.text };
      })));
      break;
    }
    case "saved":
      status(m.error ? `save failed: ${m.error}` : "saved");
      break;
    case "building":
      buildEl.textContent = "compiling…";
      break;
    case "built":
      built(m, false);
      break;
    case "lock":
      setLock(m.lock);
      break;
    case "files":
      files = m.files; main = m.main; renderFiles();
      for (const f of models.keys()) if (!files.some((x) => x.path === f)) { models.get(f)?.dispose(); models.delete(f); if (current === f) { current = null; editor.setModel(null); } }
      if (!current && main) open(main);
      break;
    case "committed":
      status(`committed ${m.sha.slice(0, 7)}`);
      break;
    case "refused":
      status(m.reason === "locked" ? `read-only: ${lock?.user ?? "someone"} holds the lock` : `refused: ${m.reason}`);
      break;
    case "lagged":
      for (const file of models.keys()) send({ type: "open", file });
      break;
    case "ai":
      ai.aiEvent(m);
      break;
  }
}

// ---- build ------------------------------------------------------------------

const pdf = new PdfViewer($("pdf-scroll"), (page, x, y, jump) => void inverseSync(page, x, y, jump), (n, zoom) => {
  $("pdf-pages").textContent = `${n} page${n === 1 ? "" : "s"} · ${Math.round(zoom * 100)}%`;
});
$("pdf-in").addEventListener("click", () => pdf.zoom(1.2));
$("pdf-out").addEventListener("click", () => pdf.zoom(1 / 1.2));
$("pdf-fit").addEventListener("click", () => pdf.zoom("fit"));
$("pdf-sync").addEventListener("click", () => void forwardSync());

function built(b: Build, initial: boolean): void {
  lastBuild = b;
  const errors = b.diags.filter((d) => d.severity === "error").length;
  const warnings = b.diags.filter((d) => d.severity === "warning").length;
  buildEl.textContent = errors ? `${errors} error${errors === 1 ? "" : "s"}` : `ok${warnings ? ` · ${warnings} warning${warnings === 1 ? "" : "s"}` : ""}`;
  buildEl.className = errors ? "err" : "muted";
  if (!initial) buildEl.textContent += ` · ${(b.ms / 1000).toFixed(1)}s`;
  markers(b.diags);
  if ((b.pdf || initial) && b.hasPdf) {
    $("pdf-empty").hidden = true;
    void pdf.load(`/project/${project}/pdf?v=${b.version}`);
  } else if (!b.hasPdf) {
    $("pdf-empty").hidden = false;
    $("pdf-empty").textContent = errors ? "no PDF: fix the errors" : "no PDF yet";
  }
  if (!$("log-pane").hidden) void showLog();
}

function markers(diags: Diagnostic[]): void {
  const sev = { error: monaco.MarkerSeverity.Error, warning: monaco.MarkerSeverity.Warning, info: monaco.MarkerSeverity.Hint };
  for (const [file, m] of models) {
    const mine = diags.filter((d) => d.file.replace(/^\.\//, "") === file);
    monaco.editor.setModelMarkers(m, "latex", mine.map((d) => {
      const line = Math.min(Math.max(d.line ?? 1, 1), m.getLineCount());
      return { severity: sev[d.severity], message: d.message, startLineNumber: line, startColumn: 1, endLineNumber: line, endColumn: m.getLineMaxColumn(line) };
    }));
  }
}

async function forwardSync(): Promise<void> {
  const pos = editor.getPosition();
  if (!pos || !current) return;
  try {
    const boxes = await api<SyncBox[]>(`/sync/forward?file=${encodeURIComponent(current)}&line=${pos.lineNumber}&col=${pos.column}`);
    if (boxes.length) pdf.highlight(boxes); else status("no SyncTeX position for this line");
  } catch (e) { status(`sync: ${(e as Error).message}`); }
}

async function inverseSync(page: number, x: number, y: number, jump: boolean): Promise<void> {
  try {
    const pos = await api<{ file: string; line: number } | null>(`/sync/inverse?page=${page}&x=${x.toFixed(2)}&y=${y.toFixed(2)}`);
    ai.setPdfClick({ page, x, y, file: pos?.file, line: pos?.line });
    if (jump && pos) open(pos.file, pos.line);
    else if (jump) status("no source position here");
  } catch (e) { status(`sync: ${(e as Error).message}`); }
}

// ---- lock -------------------------------------------------------------------

const lockBtn = $<HTMLButtonElement>("lock-btn");
function setLock(l: Lock | null): void {
  lock = l;
  const mine = l?.user === me;
  $("lock").textContent = l ? (mine ? "you hold the lock" : `${l.user} holds the lock`) : "unlocked";
  lockBtn.textContent = !l ? "take lock" : mine ? "release" : "steal";
  editor?.updateOptions({ readOnly: !!l && !mine });
}
lockBtn.addEventListener("click", () => send({ type: "lock", action: !lock ? "take" : lock.user === me ? "release" : "steal" }));

// ---- files ------------------------------------------------------------------

const filesPane = $("files");
$("btn-files").addEventListener("click", () => { filesPane.hidden = !filesPane.hidden; $("btn-files").classList.toggle("on", !filesPane.hidden); });

function renderFiles(): void {
  const list = $("file-list");
  list.replaceChildren(...files.map((f) => {
    const li = document.createElement("li");
    li.className = (f.path === current ? "current " : "") + (f.text ? "" : "binary");
    const a = document.createElement("a");
    a.href = "#";
    a.textContent = (f.path === main ? "★ " : "") + f.path;
    a.title = `${f.size} bytes`;
    a.addEventListener("click", (e) => { e.preventDefault(); open(f.path); });
    li.appendChild(a);
    const actions = document.createElement("span");
    actions.className = "actions";
    if (f.text && f.path.endsWith(".tex") && f.path !== main) {
      const b = document.createElement("button");
      b.className = "tiny"; b.textContent = "main"; b.title = "compile this file";
      b.addEventListener("click", () => void api("/main", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ file: f.path }) }));
      actions.appendChild(b);
    }
    const del = document.createElement("button");
    del.className = "tiny"; del.textContent = "×"; del.title = "delete";
    del.addEventListener("click", () => {
      if (del.dataset.armed) void api(`/files/${f.path}`, { method: "DELETE" }).catch((e) => status(String(e)));
      else { del.dataset.armed = "1"; del.textContent = "sure?"; setTimeout(() => { del.dataset.armed = ""; del.textContent = "×"; }, 2500); }
    });
    actions.appendChild(del);
    li.appendChild(actions);
    return li;
  }));
}

$("file-new").addEventListener("click", () => {
  const input = document.createElement("input");
  input.placeholder = "chapters/intro.tex";
  input.className = "inline";
  input.addEventListener("keydown", (e) => {
    if (e.key === "Escape") input.remove();
    if (e.key === "Enter") {
      const path = input.value.trim();
      input.remove();
      if (path) void api(`/files/${path}`, { method: "PUT", body: "" }).then(() => open(path)).catch((e) => status(String(e)));
    }
  });
  $("file-list").prepend(input);
  input.focus();
});

const fileInput = $<HTMLInputElement>("file-input");
$("file-upload").addEventListener("click", () => fileInput.click());
fileInput.addEventListener("change", async () => {
  for (const f of fileInput.files ?? []) {
    status(`uploading ${f.name}…`);
    await api(`/files/${f.name}`, { method: "PUT", body: f }).catch((e) => status(String(e)));
  }
  fileInput.value = "";
  status("uploaded");
});
document.addEventListener("dragover", (e) => e.preventDefault());
document.addEventListener("drop", async (e) => {
  e.preventDefault();
  for (const f of e.dataTransfer?.files ?? []) await api(`/files/${f.name}`, { method: "PUT", body: f }).catch((err) => status(String(err)));
});

// ---- log, history, ai toggles ------------------------------------------------

$("btn-log").addEventListener("click", () => { const p = $("log-pane"); p.hidden = !p.hidden; $("btn-log").classList.toggle("on", !p.hidden); if (!p.hidden) void showLog(); });
async function showLog(): Promise<void> {
  const text = await api<string>("/log").catch(() => "no log yet");
  const pane = $("log-pane");
  pane.textContent = text || "(empty log)";
  pane.scrollTop = pane.scrollHeight;
}

$("btn-ai").addEventListener("click", () => ai.toggleAi());

// ---- history: time travel ----------------------------------------------------
//
// The history strip is a timeline. Scrubbing it swaps the editor to the
// document as it was at that entry (read-only, lines that have changed since
// shaded in place, marks where text was added later) and, once you pause,
// the PDF pane to that version compiled. "restore" adopts the version through
// the live document; "back to now" returns.

interface Entry { id: number; file: string; ts: number; author: string; kind: string; meta: Record<string, unknown>; first: number; count: number }

const history = $("history");
const slider = $<HTMLInputElement>("history-slider");
const historyLabel = $("history-label");
const restoreBtn = $<HTMLButtonElement>("history-restore");
const nowBtn = $<HTMLButtonElement>("history-now");
let timeline: Entry[] = [];                 // oldest first; the slider's rightmost position is "now"
let travel: { entry: Entry; text: string } | null = null;
let travelModel: monaco.editor.ITextModel | null = null;
let travelDecorations: monaco.editor.IEditorDecorationsCollection | null = null;
let travelFile: string | null = null;
let travelRequest = 0;
let pdfTimer: number | undefined;
const textCache = new Map<string, string>();

$("btn-history").addEventListener("click", () => { if (history.hidden) void openHistory(); else closeHistory(); });
$("history-close").addEventListener("click", closeHistory);
nowBtn.addEventListener("click", () => { slider.value = String(timeline.length); void scrub(); });
restoreBtn.addEventListener("click", async () => {
  if (!travel || !travelFile) return;
  await api(`/history/${travel.entry.id}/restore?file=${encodeURIComponent(travelFile)}`, { method: "POST" }).catch((e) => status(String(e)));
  leaveTravel();
  void openHistory();
});
slider.addEventListener("input", () => void scrub());
document.addEventListener("keydown", (e) => {
  if (history.hidden || document.activeElement !== slider) return;
  if (e.key === "Escape") { closeHistory(); e.preventDefault(); }
});

async function openHistory(): Promise<void> {
  history.hidden = false;
  $("btn-history").classList.add("on");
  const entries = await api<Entry[]>(`/history?limit=200${current ? `&file=${encodeURIComponent(current)}` : ""}`).catch(() => [] as Entry[]);
  timeline = entries.reverse();
  slider.max = String(timeline.length);
  slider.value = String(timeline.length);
  renderChips();
  describe(null);
  slider.focus();
}

function closeHistory(): void {
  leaveTravel();
  history.hidden = true;
  $("btn-history").classList.remove("on");
}

function whatHappened(e: Entry): string {
  return e.kind === "delta" ? `typed ${e.count} change${e.count === 1 ? "" : "s"}`
    : e.kind === "ai" ? `AI: ${String(e.meta.prompt ?? "").slice(0, 60)}`
    : e.kind === "commit" ? `git ${String(e.meta.sha ?? "").slice(0, 7)} ${e.meta.message ?? ""}`
    : e.kind === "lock" ? "took the lock" : e.kind === "replace" ? `replaced (${e.meta.source ?? "api"})`
    : e.kind === "restore" ? "restored a version" : e.kind;
}

function renderChips(): void {
  const chips = $("history-chips");
  chips.replaceChildren(...timeline.map((e, i) => {
    const c = document.createElement("span");
    c.className = `chip ${e.kind}`;
    c.textContent = `${new Date(e.ts).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })} ${e.author}: ${whatHappened(e)}`;
    c.title = new Date(e.ts).toLocaleString();
    c.addEventListener("click", () => { slider.value = String(i); void scrub(); });
    return c;
  }), (() => { const n = document.createElement("span"); n.className = "chip now"; n.textContent = "now"; n.addEventListener("click", () => { slider.value = String(timeline.length); void scrub(); }); return n; })());
  markChip();
}

function markChip(): void {
  const i = Number(slider.value);
  $("history-chips").querySelectorAll(".chip").forEach((c, j) => {
    c.classList.toggle("current", j === i && i < timeline.length);
    if (j === i) (c as HTMLElement).scrollIntoView({ inline: "center", block: "nearest" });
  });
}

function describe(entry: Entry | null, extra = ""): void {
  historyLabel.textContent = entry
    ? `${new Date(entry.ts).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" })} · ${entry.author} · ${whatHappened(entry)}${extra}`
    : "now";
  restoreBtn.disabled = !entry;
  nowBtn.disabled = !entry;
}

/** The slider moved: show that moment. */
async function scrub(): Promise<void> {
  markChip();
  const i = Number(slider.value);
  if (i >= timeline.length || !current) { leaveTravel(); describe(null); return; }
  const entry = timeline[i];
  const file = current;
  const req = ++travelRequest;
  const key = `${entry.id}:${file}`;
  let text = textCache.get(key);
  if (text === undefined) {
    const r = await api<{ text: string | null }>(`/history/${entry.id}?file=${encodeURIComponent(file)}`).catch(() => ({ text: null }));
    if (req !== travelRequest) return;
    text = r.text ?? "";
    textCache.set(key, text);
  }
  showTravel(entry, file, text);
  window.clearTimeout(pdfTimer);
  pdfTimer = window.setTimeout(() => {
    if (travel?.entry.id !== entry.id) return;
    status("compiling that version…");
    void pdf.load(`/api/projects/${project}/history/${entry.id}/pdf`).then(() => status(`pdf as of ${new Date(entry.ts).toLocaleTimeString()}`), (e) => status(`that version: ${String((e as Error).message ?? e).slice(0, 80)}`));
  }, 600);
}

function showTravel(entry: Entry, file: string, text: string): void {
  const live = models.get(file);
  if (!live) return;
  travelFile = file;
  travelModel ??= monaco.editor.createModel("", "latex");
  if (travelModel.getValue() !== text) travelModel.setValue(text);
  if (editor.getModel() !== travelModel) {
    editor.setModel(travelModel);
    editor.updateOptions({ readOnly: true });
    document.body.classList.add("time-travel");
  }
  travel = { entry, text };
  // Shade what has changed since then.
  const hunks = lineDiff(text.split("\n"), live.getValue().split("\n"));
  const decorations: monaco.editor.IModelDeltaDecoration[] = [];
  let removed = 0, added = 0;
  for (const h of hunks) {
    removed += h.aLen; added += h.bLen;
    if (h.aLen > 0) {
      decorations.push({ range: new monaco.Range(h.aStart + 1, 1, h.aStart + h.aLen, 1), options: { isWholeLine: true, className: "tt-changed", linesDecorationsClassName: "tt-glyph" } });
    } else {
      const line = Math.max(1, h.aStart);
      decorations.push({ range: new monaco.Range(line, 1, line, 1), options: { isWholeLine: true, className: h.aStart === 0 ? "tt-before" : "tt-after" } });
    }
  }
  travelDecorations ??= editor.createDecorationsCollection();
  travelDecorations.set(decorations);
  describe(entry, hunks.length ? ` · since then: ${added} line${added === 1 ? "" : "s"} added, ${removed} changed` : " · identical to now");
  const first = hunks[0];
  if (first) editor.revealLineInCenterIfOutsideViewport(first.aStart + 1);
}

function leaveTravel(): void {
  window.clearTimeout(pdfTimer);
  travelRequest++;
  if (!travel) return;
  travel = null;
  travelDecorations?.clear();
  document.body.classList.remove("time-travel");
  const live = travelFile ? models.get(travelFile) : null;
  if (live) editor.setModel(live);
  editor.updateOptions({ readOnly: !!lock && lock.user !== me });
  if (lastBuild?.hasPdf) void pdf.load(`/project/${project}/pdf?v=${lastBuild.version}`);
  status("back to now");
}

// ---- pane divider -------------------------------------------------------------

const divider = $("divider");
divider.addEventListener("pointerdown", (e) => {
  divider.setPointerCapture(e.pointerId);
  const left = $("editor").getBoundingClientRect().left;
  const move = (ev: PointerEvent) => document.documentElement.style.setProperty("--editor-width", `${Math.max(240, ev.clientX - left)}px`);
  const up = () => { divider.removeEventListener("pointermove", move); divider.removeEventListener("pointerup", up); };
  divider.addEventListener("pointermove", move);
  divider.addEventListener("pointerup", up);
});
