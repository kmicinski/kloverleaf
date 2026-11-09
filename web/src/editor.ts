// The editor page: Monaco on the left, the PDF on the right, the AI panel
// beside it. One WebSocket carries edits out and the text, forwarded edits,
// build results, lock state and AI events in (protocol in src/live.rs).
// This file wires the pieces: models per file, the file list, the lock, the
// build status and markers, SyncTeX both ways, the history drawer.

import type { Build, ClientMsg, Diagnostic, FileInfo, Lock, Region, ServerMsg } from "./protocol.js";
import { api, fnv1a, me, postJson, project, socketUrl, when } from "./protocol.js";
import { knownBibKeys, knownLabels, levenshtein, registerLatex, registerLatexNavigation, setProjectFiles } from "./latex.js";
import { PdfViewer, type SyncBox } from "./pdf.js";
import { lineDiff } from "./diff.js";
import * as ai from "./ai.js";
import * as prov from "./provenance.js";
import * as up from "./upstream.js";
import { renderTree, type TreeData } from "./tree.js";

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
let branchName = "main";
let applyingRemote = false;
const models = new Map<string, monaco.editor.ITextModel>();
/** Sends if the socket is up; returns whether it went. */
const send = (m: ClientMsg): boolean => { if (ws?.readyState === WebSocket.OPEN) { ws.send(JSON.stringify(m)); return true; } return false; };
// Hash of each file's text as the server last knew it. While the socket is
// down edits stay local and this stops moving; on reconnect a file whose
// text no longer matches it has unsent work, which is pushed to the server
// instead of being overwritten by the server's older copy.
const synced = new Map<string, number>();
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
  registerLatexNavigation(project);
  registerQuickFixes();
  // F12 / ⌘-click on a \ref, \cite or file name: a location in another file
  // opens through our own file switching (Monaco only knows this one editor).
  monaco.editor.registerEditorOpener({
    openCodeEditor(_source, resource, selectionOrPosition) {
      const file = resource.path.replace(/^\//, "");
      const line = selectionOrPosition && "startLineNumber" in selectionOrPosition ? selectionOrPosition.startLineNumber : selectionOrPosition && "lineNumber" in selectionOrPosition ? selectionOrPosition.lineNumber : undefined;
      const column = selectionOrPosition && "startColumn" in selectionOrPosition ? selectionOrPosition.startColumn : selectionOrPosition && "column" in selectionOrPosition ? selectionOrPosition.column : undefined;
      if (!files.some((f) => f.path === file)) return false;
      open(file, line, column);
      return true;
    },
  });
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
    // Completions as you type a command, like Overleaf; no random-word noise.
    quickSuggestions: { other: true, comments: false, strings: true },
    quickSuggestionsDelay: 60,
    suggestOnTriggerCharacters: true,
    wordBasedSuggestions: "off",
    snippetSuggestions: "inline",
    acceptSuggestionOnEnter: "on",
    tabCompletion: "on",
    suggest: { showWords: false, snippetsPreventQuickSuggestions: false, preview: true, filterGraceful: true, localityBonus: true },
    glyphMargin: true,
  });
  // Ctrl-O = open-line, as in Emacs and Overleaf: a newline goes in at the
  // cursor and the cursor stays put, so at the start of a line the line moves
  // down and you are left on the new blank one. Bound here, before the Emacs
  // extension sees the key, so it behaves the same on every platform and on
  // wrapped (logical) lines.
  editor.onKeyDown((e) => {
    if (!(e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey && e.keyCode === monaco.KeyCode.KeyO)) return;
    e.preventDefault();
    e.stopPropagation();
    if (editor.getOption(monaco.editor.EditorOption.readOnly)) return;
    const sel = editor.getSelection();
    if (!sel) return;
    const p = sel.getPosition();
    editor.executeEdits("open-line", [{ range: new monaco.Range(p.lineNumber, p.column, p.lineNumber, p.column), text: "\n", forceMoveMarkers: true }], [new monaco.Selection(p.lineNumber, p.column, p.lineNumber, p.column)]);
    editor.revealPosition(p);
  });
  // ---- Emacs extras the extension lacks: paragraph and sentence motion,
  // fill-paragraph, and M-x (Monaco's command palette, where every "LaTeX:"
  // action below is listed). Keys are caught here, ahead of the extension.
  // The kill ring, as Emacs has it (the extension's loses entries on M-y).
  editor.onKeyDown((e) => {
    const act = (f: () => void) => { e.preventDefault(); e.stopPropagation(); f(); };
    if (e.ctrlKey && !e.altKey && !e.metaKey && !e.shiftKey) {
      switch (e.keyCode) {
        case monaco.KeyCode.KeyK: act(killLine); return;
        case monaco.KeyCode.KeyW: act(() => killRegion(true)); return;
        case monaco.KeyCode.KeyY: act(() => void yank()); return;
      }
    }
    if (e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey) {
      switch (e.keyCode) {
        case monaco.KeyCode.KeyW: act(() => killRegion(false)); return;
        case monaco.KeyCode.KeyY: act(yankPop); return;
        case monaco.KeyCode.KeyD: act(() => killWord(1)); return;
        case monaco.KeyCode.Backspace: act(() => killWord(-1)); return;
      }
    }
    // Anything else ends a kill/yank sequence (so the next kill starts a new ring entry).
    if (!(e.ctrlKey && e.keyCode === monaco.KeyCode.KeyU)) ring.last = "other";
  });
  editor.onMouseDown(() => { ring.last = "other"; });
  editor.onKeyDown((e) => {
    if (!e.altKey || e.ctrlKey || e.metaKey) return;
    const act = (f: () => void) => { e.preventDefault(); e.stopPropagation(); f(); };
    switch (e.keyCode) {
      case monaco.KeyCode.BracketRight: act(() => paragraphMove(1, e.shiftKey)); break;   // M-] / M-}
      case monaco.KeyCode.BracketLeft: act(() => paragraphMove(-1, e.shiftKey)); break;   // M-[ / M-{
      case monaco.KeyCode.KeyX: if (!e.shiftKey) act(() => editor.trigger("emacs", "editor.action.quickCommand", null)); break; // M-x
      case monaco.KeyCode.KeyQ: if (!e.shiftKey) act(fillParagraph); break;              // M-q
      case monaco.KeyCode.KeyE: if (!e.shiftKey) act(() => sentenceMove(1)); break;      // M-e
      case monaco.KeyCode.KeyA: if (!e.shiftKey) act(() => sentenceMove(-1)); break;     // M-a
    }
  });
  const action = (id: string, label: string, run: () => void, keybindings?: number[]) =>
    editor.addAction({ id, label, keybindings, contextMenuGroupId: "latex", run });
  action("latex.paragraph.next", "LaTeX: Next paragraph (M-] / M-})", () => paragraphMove(1, false));
  action("latex.paragraph.prev", "LaTeX: Previous paragraph (M-[ / M-{)", () => paragraphMove(-1, false));
  action("latex.sentence.next", "LaTeX: End of sentence (M-e)", () => sentenceMove(1));
  action("latex.sentence.prev", "LaTeX: Start of sentence (M-a)", () => sentenceMove(-1));
  action("latex.fill", "LaTeX: Fill paragraph to 80 columns (M-q)", fillParagraph);
  action("latex.emph", "LaTeX: Emphasize selection \\emph{…}", () => wrapSelection("\\emph{", "}"), [monaco.KeyMod.CtrlCmd | monaco.KeyMod.Alt | monaco.KeyCode.KeyE]);
  action("latex.bold", "LaTeX: Bold selection \\textbf{…}", () => wrapSelection("\\textbf{", "}"), [monaco.KeyMod.CtrlCmd | monaco.KeyMod.Alt | monaco.KeyCode.KeyB]);
  action("latex.tt", "LaTeX: Monospace selection \\texttt{…}", () => wrapSelection("\\texttt{", "}"));
  action("latex.math", "LaTeX: Inline math $…$ around selection", () => wrapSelection("$", "$"));
  action("latex.env", "LaTeX: Insert environment…", insertEnvironment, [monaco.KeyMod.CtrlCmd | monaco.KeyMod.Alt | monaco.KeyCode.KeyN]);
  action("latex.compile", "LaTeX: Compile now — and push, in a project that tracks a repository (⌘S)", save);
  action("latex.upstream.push", "Git: Push to the tracked repository", () => void up.push());
  action("latex.sync", "LaTeX: Show this spot in the PDF (⌥⏎)", () => void forwardSync());
  action("latex.log", "LaTeX: Toggle the compile log", () => $("btn-log").click());
  action("latex.ai.ask", "AI: Ask about the selection (⌘⇧Q)", () => ai.queueSelection());
  action("latex.ai.toggle", "AI: Toggle the AI tab (⌘⇧A)", () => ai.toggleAi());
  action("latex.prov.claim", "Provenance: Make this mine (⌘⇧M)", () => void prov.claimSelection());
  action("latex.history", "History: Open the timeline", () => $("btn-history").click());
  action("latex.keys", "Help: Keyboard shortcuts", showKeys);
  $("btn-keys").addEventListener("click", showKeys);
  new (emacs as MonacoEmacsModule).EmacsExtension(editor).start();
  editor.onDidChangeCursorPosition(() => { ai.renderContext(); renderOutline(); });
  editor.onDidChangeModelContent(() => { renderOutline(); renderWordCount(); });
  editor.onDidChangeModel(() => renderWordCount());
  editor.onMouseUp((e) => { if (e.target.position && !e.event.rightButton) void forwardSync(true); });
  editor.onDidChangeCursorSelection(() => ai.renderContext());
  editor.addCommand(monaco.KeyMod.Alt | monaco.KeyCode.Enter, () => void forwardSync());
  editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyQ, () => ai.queueSelection());
  editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyA, () => ai.toggleAi());
  editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS, save);
  editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyM, () => void prov.claimSelection());
  prov.initProvenance({
    editor: () => editor,
    model: (file) => models.get(file),
    fileOf: (m) => { for (const [f, mm] of models) if (mm === m) return f; return undefined; },
    travelTo: (entry) => void travelTo(entry),
    status,
  });
  ai.initAi({
    send,
    context: editorContext,
    currentText: (file) => models.get(file)?.getValue(),
    open: (file, line) => open(file, line),
  });
  connect();
});

/** ⌘S: compile now; in a project that tracks a repository, also push what changed since the last push. */
function save(): void {
  void api("/compile", { method: "POST" });
  if (up.isTracked()) void up.push();
}

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
    const local = m.getValue();
    if (local !== text) {
      if (fnv1a(local) !== synced.get(file)) {
        // Unsent edits here: ours win — the server adopts this text (provenance rebased).
        const hash = fnv1a(local);
        if (send({ type: "resync", file, text: local, hash })) { synced.set(file, hash); status("sent the edits made while offline"); }
        return m;
      }
      const pos = editor.getModel() === m ? editor.getPosition() : null;
      remote(() => m!.pushEditOperations([], [{ range: m!.getFullModelRange(), text }], () => null));
      if (pos) editor.setPosition(pos);
    }
    synced.set(file, fnv1a(text));
    return m;
  }
  const language = /\.(tex|sty|cls|bib|tikz|ltx|dtx)$/i.test(file) ? "latex" : /\.(md)$/i.test(file) ? "markdown" : /\.json$/i.test(file) ? "json" : "plaintext";
  m = monaco.editor.createModel(text, language, monaco.Uri.parse(`kloverleaf:/${file}`));
  synced.set(file, fnv1a(text));
  m.onDidChangeContent((e) => {
    if (applyingRemote) return;
    const changes = e.changes.map((c) => ({ offset: c.rangeOffset, length: c.rangeLength, text: c.text }));
    prov.applyLocal(file, changes);
    up.localEdit();
    const hash = fnv1a(m!.getValue());
    if (send({ type: "edit", file, changes, hash })) { synced.set(file, hash); status("editing"); }
    else status("offline — your edits are kept here and sent when the connection returns");
  });
  models.set(file, m);
  if (lastBuild) markers(lastBuild.diags);
  return m;
}

/** Shows a file in the editor, fetching its text if this page has not seen it. */
function open(file: string, line?: number, column?: number, focus = true): void {
  const info = files.find((f) => f.path === file);
  if (info && !info.text) { window.open(`/api/projects/${project}/files/${file}`, "_blank"); return; }
  const m = models.get(file);
  if (!m) { pendingOpen = { file, line }; send({ type: "open", file }); return; }
  if (travel) leaveTravel();
  if (editor.getModel() !== m) editor.setModel(m);
  current = file;
  prov.refresh();
  void up.fileShown(file);
  renderFiles();
  renderOutline();
  if (line) {
    const pos = { lineNumber: line, column: column ?? 1 };
    editor.setPosition(pos);
    if (column) { editor.revealPositionInCenter(pos); flashPosition(pos); } else { editor.revealLineInCenter(line); flashPosition(pos, true); }
  }
  if (focus) editor.focus();
  ai.renderContext();
}

/** A short glow on the word a PDF click landed on. */
let flashDecorations: monaco.editor.IEditorDecorationsCollection | null = null;
function flashPosition(pos: { lineNumber: number; column: number }, wholeLine = false): void {
  const model = editor.getModel();
  if (!model) return;
  const word = wholeLine ? null : model.getWordAtPosition(pos);
  const range = word ? new monaco.Range(pos.lineNumber, word.startColumn, pos.lineNumber, word.endColumn) : new monaco.Range(pos.lineNumber, 1, pos.lineNumber, model.getLineMaxColumn(pos.lineNumber));
  flashDecorations ??= editor.createDecorationsCollection();
  flashDecorations.set([{ range, options: word ? { inlineClassName: "sync-flash" } : { className: "sync-flash-line", isWholeLine: true } }]);
  window.setTimeout(() => flashDecorations?.clear(), 2500);
}

// ---- the kill ring --------------------------------------------------------------
//
// Emacs semantics: consecutive kills grow one ring entry (C-k C-k takes two
// lines as one; a backward kill prepends); C-y yanks the newest entry and
// M-y right after swaps it for older ones, rotating the ring; a kill also
// lands in the system clipboard, and C-y prefers fresh clipboard text that
// did not come from a kill here. The ring survives reloads (localStorage).

const RING_MAX = 60;
const ring = {
  entries: [] as string[],            // newest last
  last: "other" as "kill" | "yank" | "other",
  yankIndex: 0,                        // which entry the last yank used (0 = newest)
  yankRange: null as monaco.Range | null,
  clipboardMirror: "",                 // what we last wrote to the clipboard
};
try { ring.entries = JSON.parse(localStorage.getItem("kill-ring") ?? "[]"); } catch { /* ignore */ }

function ringPush(text: string, append: "front" | "back" | null): void {
  if (!text) return;
  if (append && ring.last === "kill" && ring.entries.length) {
    const top = ring.entries.pop()!;
    ring.entries.push(append === "back" ? top + text : text + top);
  } else ring.entries.push(text);
  while (ring.entries.length > RING_MAX) ring.entries.shift();
  ring.last = "kill";
  try { localStorage.setItem("kill-ring", JSON.stringify(ring.entries)); } catch { /* ignore */ }
  const top = ring.entries[ring.entries.length - 1];
  ring.clipboardMirror = top;
  void navigator.clipboard?.writeText(top).catch(() => undefined);
}

function cut(range: monaco.Range, append: "front" | "back" | null, keep = false): void {
  const m = editor.getModel();
  if (!m || range.isEmpty()) return;
  const text = m.getValueInRange(range);
  ringPush(text, append);
  if (!keep && !editor.getOption(monaco.editor.EditorOption.readOnly)) {
    editor.executeEdits("kill", [{ range, text: "" }], [new monaco.Selection(range.startLineNumber, range.startColumn, range.startLineNumber, range.startColumn)]);
  }
}

/** C-k: to the end of the line; at the end of a line, the line break itself. */
function killLine(): void {
  const m = editor.getModel(), pos = editor.getPosition();
  if (!m || !pos) return;
  const end = m.getLineMaxColumn(pos.lineNumber);
  const range = pos.column >= end && pos.lineNumber < m.getLineCount()
    ? new monaco.Range(pos.lineNumber, pos.column, pos.lineNumber + 1, 1)
    : new monaco.Range(pos.lineNumber, pos.column, pos.lineNumber, end);
  cut(range, "back");
}

/** C-w kills the region, M-w copies it; the mark is whatever the selection is. */
function killRegion(remove: boolean): void {
  const sel = editor.getSelection();
  if (!sel || sel.isEmpty()) { status(remove ? "C-w: no region" : "M-w: no region"); return; }
  cut(sel, null, !remove);
  if (!remove) editor.setPosition(sel.getPosition());
}

/** M-d / M-DEL: kill a word forward or backward (consecutive ones grow the entry). */
function killWord(dir: 1 | -1): void {
  const m = editor.getModel(), pos = editor.getPosition();
  if (!m || !pos) return;
  const off = m.getOffsetAt(pos);
  const text = m.getValue();
  let to = off;
  if (dir > 0) {
    while (to < text.length && !/[\p{L}\p{N}]/u.test(text[to])) to++;
    while (to < text.length && /[\p{L}\p{N}]/u.test(text[to])) to++;
    cut(monaco.Range.fromPositions(pos, m.getPositionAt(to)), "back");
  } else {
    while (to > 0 && !/[\p{L}\p{N}]/u.test(text[to - 1])) to--;
    while (to > 0 && /[\p{L}\p{N}]/u.test(text[to - 1])) to--;
    cut(monaco.Range.fromPositions(m.getPositionAt(to), pos), "front");
  }
}

async function yank(): Promise<void> {
  if (editor.getOption(monaco.editor.EditorOption.readOnly)) return;
  // Text copied elsewhere since our last kill goes first, like Emacs's interprogram paste.
  try {
    const clip = await navigator.clipboard?.readText();
    if (clip && clip !== ring.clipboardMirror && clip !== ring.entries[ring.entries.length - 1]) {
      ring.entries.push(clip);
      while (ring.entries.length > RING_MAX) ring.entries.shift();
      ring.clipboardMirror = clip;
    }
  } catch { /* no clipboard access: the ring alone */ }
  const text = ring.entries[ring.entries.length - 1];
  if (!text) { status("kill ring is empty"); return; }
  const sel = editor.getSelection();
  if (!sel) return;
  const start = sel.getStartPosition();
  editor.executeEdits("yank", [{ range: sel, text }]);
  const end = editor.getModel()!.getPositionAt(editor.getModel()!.getOffsetAt(start) + text.length);
  ring.yankRange = monaco.Range.fromPositions(start, end);
  ring.yankIndex = 0;
  ring.last = "yank";
  editor.setPosition(end);
}

/** M-y right after a yank: replace what was yanked with the next older entry. */
function yankPop(): void {
  if (ring.last !== "yank" || !ring.yankRange) { status("M-y: previous command was not a yank"); return; }
  if (ring.entries.length < 2) return;
  ring.yankIndex = (ring.yankIndex + 1) % ring.entries.length;
  const text = ring.entries[ring.entries.length - 1 - ring.yankIndex];
  const start = ring.yankRange.getStartPosition();
  editor.executeEdits("yank-pop", [{ range: ring.yankRange, text }]);
  const m = editor.getModel()!;
  const end = m.getPositionAt(m.getOffsetAt(start) + text.length);
  ring.yankRange = monaco.Range.fromPositions(start, end);
  ring.last = "yank";
  editor.setPosition(end);
  status(`yank-pop ${ring.yankIndex + 1}/${ring.entries.length}`);
}

// ---- Emacs-style motion and editing ----------------------------------------------

const blank = (m: monaco.editor.ITextModel, ln: number) => m.getLineContent(ln).trim() === "";

function moveTo(target: monaco.IPosition, select: boolean): void {
  if (select) {
    const sel = editor.getSelection()!;
    editor.setSelection(new monaco.Selection(sel.selectionStartLineNumber, sel.selectionStartColumn, target.lineNumber, target.column));
  } else editor.setPosition(target);
  editor.revealPositionInCenterIfOutsideViewport(target);
}

/** M-} / M-{: to the blank line after (before) the current paragraph, as in Emacs. */
function paragraphMove(dir: 1 | -1, select: boolean): void {
  const m = editor.getModel(), pos = editor.getPosition();
  if (!m || !pos) return;
  const n = m.getLineCount();
  let ln = pos.lineNumber;
  if (dir > 0) {
    while (ln < n && blank(m, ln)) ln++;
    while (ln < n && !blank(m, ln)) ln++;
    moveTo({ lineNumber: ln, column: blank(m, ln) ? 1 : m.getLineMaxColumn(ln) }, select);
  } else {
    if (ln > 1 && (pos.column === 1 || blank(m, ln))) ln--;
    while (ln > 1 && blank(m, ln)) ln--;
    while (ln > 1 && !blank(m, ln - 1)) ln--;
    moveTo({ lineNumber: ln > 1 ? ln - 1 : 1, column: 1 }, select);
  }
}

/** The paragraph (run of non-blank lines) around a line: [first, last]. */
function paragraphBounds(m: monaco.editor.ITextModel, ln: number): [number, number] | null {
  if (blank(m, ln)) return null;
  let a = ln, b = ln;
  while (a > 1 && !blank(m, a - 1)) a--;
  while (b < m.getLineCount() && !blank(m, b + 1)) b++;
  return [a, b];
}

/** M-e / M-a: sentence ends are . ! ? (plus closing quotes/brackets) followed by space or the paragraph's end. */
function sentenceMove(dir: 1 | -1): void {
  const m = editor.getModel(), pos = editor.getPosition();
  if (!m || !pos) return;
  const bounds = paragraphBounds(m, pos.lineNumber);
  if (!bounds) { paragraphMove(dir, false); return; }
  const start = m.getOffsetAt({ lineNumber: bounds[0], column: 1 });
  const end = m.getOffsetAt({ lineNumber: bounds[1], column: m.getLineMaxColumn(bounds[1]) });
  const text = m.getValueInRange(monaco.Range.fromPositions(m.getPositionAt(start), m.getPositionAt(end)));
  const here = m.getOffsetAt(pos) - start;
  const ends: number[] = [];
  const re = /[.!?][)"'\]]*(?=\s|$)/g;
  for (let x = re.exec(text); x; x = re.exec(text)) ends.push(x.index + x[0].length);
  let target: number;
  if (dir > 0) target = ends.find((e) => e > here) ?? text.length;
  else {
    const prev = [...ends].reverse().find((e) => e < here - 1);
    target = prev === undefined ? 0 : prev;
    while (target < text.length && /\s/.test(text[target])) target++;
  }
  moveTo(m.getPositionAt(start + target), false);
}

/** M-q: re-wrap the paragraph at 80 columns. Prose only — a paragraph holding
 *  \begin/\end, display math or comment lines is left alone. Undoable. */
function fillParagraph(): void {
  const m = editor.getModel(), pos = editor.getPosition();
  if (!m || !pos || editor.getOption(monaco.editor.EditorOption.readOnly)) return;
  const bounds = paragraphBounds(m, pos.lineNumber);
  if (!bounds) { status("fill: not in a paragraph"); return; }
  const lines: string[] = [];
  for (let ln = bounds[0]; ln <= bounds[1]; ln++) lines.push(m.getLineContent(ln));
  if (lines.some((l) => /^\s*(%|\\begin|\\end|\\\[|\\\]|\\item|\\section|\\subsection|\\caption|\\label|\\includegraphics)/.test(l) || /\\\\\s*$/.test(l))) {
    status("fill: this paragraph has structure (environments, comments, \\\\) — not reflowing it");
    return;
  }
  const indent = lines[0].match(/^\s*/)![0];
  const words = lines.join(" ").trim().split(/\s+/);
  const out: string[] = [];
  let cur = indent;
  for (const w of words) {
    if (cur.trim() && (cur + " " + w).length > 80) { out.push(cur); cur = indent + w; }
    else cur = cur.trim() ? cur + " " + w : indent + w;
  }
  if (cur.trim()) out.push(cur);
  const range = new monaco.Range(bounds[0], 1, bounds[1], m.getLineMaxColumn(bounds[1]));
  editor.executeEdits("fill-paragraph", [{ range, text: out.join("\n") }]);
  editor.setPosition({ lineNumber: bounds[0], column: 1 });
  status(`filled ${lines.length} → ${out.length} lines`);
}

/** Wraps the selection (or inserts an empty pair with the cursor inside). */
function wrapSelection(before: string, after: string): void {
  const m = editor.getModel(), sel = editor.getSelection();
  if (!m || !sel || editor.getOption(monaco.editor.EditorOption.readOnly)) return;
  const text = m.getValueInRange(sel);
  editor.executeEdits("wrap", [{ range: sel, text: before + text + after }]);
  if (!text) { const p = sel.getStartPosition(); editor.setPosition({ lineNumber: p.lineNumber, column: p.column + before.length }); }
}

function insertEnvironment(): void {
  const name = window.prompt("environment", "itemize");
  if (!name) return;
  const pos = editor.getPosition();
  if (!pos || editor.getOption(monaco.editor.EditorOption.readOnly)) return;
  const inner = name === "itemize" || name === "enumerate" ? "  \\item " : "  ";
  editor.executeEdits("env", [{ range: new monaco.Range(pos.lineNumber, pos.column, pos.lineNumber, pos.column), text: `\\begin{${name}}\n${inner}\n\\end{${name}}` }]);
  editor.setPosition({ lineNumber: pos.lineNumber + 1, column: inner.length + 1 });
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
      setFiles(m.files, m.main); setLock(m.lock); ai.setMode(m.ai); prov.setMyOrigin(m.origin);
      branchName = m.branch; renderBranchChip();
      up.initUpstream({ editor: () => editor, current: () => current, open: (file, line) => open(file, line), status }, m.upstream);
      void ai.restore();
      if (m.build) built(m.build, true); else if (m.building) buildEl.textContent = "compiling…";
      // Re-sync every model this page already has (after a reconnect), then open the main file.
      for (const file of models.keys()) send({ type: "open", file });
      if (!current) open(main ?? files.find((f) => f.text)?.path ?? "");
      status("synced");
      break;
    case "text": {
      model(m.file, m.text);
      prov.setMap(m.file, m.prov);
      if (pendingOpen?.file === m.file) { const { file, line } = pendingOpen; pendingOpen = null; open(file, line); }
      status("synced");
      break;
    }
    case "edit": {
      const mod = models.get(m.file);
      if (!mod) break;
      prov.applyRemote(m.file, m.changes, m.origin, m.user);
      remote(() => mod.applyEdits(m.changes.map((c) => {
        const a = mod.getPositionAt(c.offset), b = mod.getPositionAt(c.offset + c.length);
        return { range: new monaco.Range(a.lineNumber, a.column, b.lineNumber, b.column), text: c.text };
      })));
      synced.set(m.file, fnv1a(mod.getValue()));
      break;
    }
    case "resynced":
      prov.setMap(m.file, m.prov);
      status("re-synced");
      break;
    case "branch":
      // Every text is new on the other branch: reload what this page has open.
      branchName = m.branch; renderBranchChip();
      setFiles(m.files, m.main);
      leaveTravel();
      for (const f of models.keys()) { synced.delete(f); send({ type: "open", file: f }); }
      if (!current && main) open(main);
      status(`on branch ${m.branch}`);
      if (!history.hidden) void openHistory();
      break;
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
      setFiles(m.files, m.main);
      if (!current && main) open(main);
      break;
    case "committed":
      status(`committed ${m.sha.slice(0, 7)}`);
      break;
    case "refused":
      status(m.reason === "locked" ? `read-only: ${lock?.user ?? "someone"} holds the lock` : `refused: ${m.reason}`);
      // The server's text follows; a desync means ours was wrong, so let it win.
      if (m.reason === "desync") { const mod = models.get(m.file); if (mod) synced.set(m.file, fnv1a(mod.getValue())); }
      break;
    case "lagged":
      for (const file of models.keys()) send({ type: "open", file });
      break;
    case "ai":
      ai.aiEvent(m);
      break;
    case "prov":
      prov.setMap(m.file, m.prov);
      break;
    case "upstream":
      up.event(m);
      break;
  }
}

/** The project's file list changed: redraw it and drop models of files that are gone. */
function setFiles(list: FileInfo[], mainFile: string | null): void {
  files = list; main = mainFile;
  renderFiles();
  setProjectFiles(files.map((f) => f.path));
  for (const f of [...models.keys()]) {
    if (files.some((x) => x.path === f)) continue;
    models.get(f)?.dispose(); models.delete(f); synced.delete(f);
    if (current === f) { current = null; editor.setModel(null); }
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

// ---- problems: the build status opens a list of every diagnostic; click to jump ------

let problemsEl: HTMLElement | null = null;
function closeProblems(): void { problemsEl?.remove(); problemsEl = null; }
buildEl.style.cursor = "pointer";
buildEl.title = "click for the list of problems";
buildEl.addEventListener("click", () => {
  if (problemsEl) { closeProblems(); return; }
  const diags = lastBuild?.diags.filter((d) => d.severity !== "info") ?? [];
  const box = document.createElement("div");
  box.id = "problems";
  if (!diags.length) {
    const p = document.createElement("div");
    p.className = "muted";
    p.textContent = lastBuild ? "no errors or warnings" : "not compiled yet";
    box.appendChild(p);
  }
  for (const d of diags) {
    const row = document.createElement("div");
    row.className = `problem ${d.severity}${/undefined/.test(d.message) ? " undefined" : ""}`;
    const where = document.createElement("span");
    where.className = "where";
    where.textContent = `${d.file.replace(/^\.\//, "")}${d.line ? `:${d.line}` : ""}`;
    const what = document.createElement("span");
    what.textContent = d.message;
    row.append(where, what);
    if (d.line) row.addEventListener("click", () => { closeProblems(); open(d.file.replace(/^\.\//, ""), d.line!, undefined, true); });
    box.appendChild(row);
  }
  const rect = buildEl.getBoundingClientRect();
  box.style.left = `${Math.min(rect.left, window.innerWidth - 520)}px`;
  box.style.top = `${rect.bottom + 4}px`;
  document.body.appendChild(box);
  problemsEl = box;
  setTimeout(() => document.addEventListener("mousedown", (e) => { if (problemsEl && !problemsEl.contains(e.target as Node) && e.target !== buildEl) closeProblems(); }, { once: true }), 0);
});

function built(b: Build, initial: boolean): void {
  lastBuild = b;
  if (problemsEl) { closeProblems(); }
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

/** An undefined reference/citation warning names the key; find it on the line for a tight marker. */
function undefinedKey(message: string): { kind: "ref" | "cite"; key: string } | null {
  const r = /Reference [`'"]([^'`"]+)['`"] .*undefined/.exec(message);
  if (r) return { kind: "ref", key: r[1] };
  const c = /Citation [`'"]([^'`"]+)['`"] .*undefined/.exec(message);
  if (c) return { kind: "cite", key: c[1] };
  return null;
}

function markers(diags: Diagnostic[]): void {
  const sev = { error: monaco.MarkerSeverity.Error, warning: monaco.MarkerSeverity.Warning, info: monaco.MarkerSeverity.Hint };
  for (const [file, m] of models) {
    const mine = diags.filter((d) => d.file.replace(/^\.\//, "") === file);
    monaco.editor.setModelMarkers(m, "latex", mine.map((d) => {
      const line = Math.min(Math.max(d.line ?? 1, 1), m.getLineCount());
      let startColumn = 1, endColumn = m.getLineMaxColumn(line);
      let severity = sev[d.severity];
      const u = undefinedKey(d.message);
      if (u) {
        // Something will print as ?? — that is an error for the reader, whatever TeX calls it.
        severity = monaco.MarkerSeverity.Error;
        const text = m.getLineContent(line);
        const i = text.indexOf(`{${u.key}}`) >= 0 ? text.indexOf(`{${u.key}}`) + 1 : text.indexOf(u.key);
        if (i >= 0) { startColumn = i + 1; endColumn = i + 1 + u.key.length; }
      }
      return { severity, message: d.message, startLineNumber: line, startColumn, endLineNumber: line, endColumn, code: u ? `undefined-${u.kind}` : undefined };
    }));
  }
}

/** Quick fixes on markers (the lightbulb, ⌘.): the nearest label or key for an
 *  undefined reference, and "fix with AI" for anything the compiler flagged.
 *  Needs Monaco, so it runs from the editor's setup, not at module load. */
function registerQuickFixes(): void {
  monaco.languages.registerCodeActionProvider("latex", {
    provideCodeActions(model, range, context) {
      const actions: monaco.languages.CodeAction[] = [];
      for (const mk of context.markers) {
        const u = undefinedKey(mk.message);
        if (u) {
          const pool = u.kind === "ref" ? knownLabels() : knownBibKeys();
          const ranked = pool.map((k) => ({ k, d: levenshtein(k.toLowerCase(), u.key.toLowerCase()) })).filter((x) => x.d <= Math.max(3, Math.floor(u.key.length / 2))).sort((a, b) => a.d - b.d).slice(0, 3);
          for (const { k } of ranked) {
            actions.push({
              title: `Did you mean ${k}?`, kind: "quickfix", isPreferred: ranked[0].k === k, diagnostics: [mk],
              edit: { edits: [{ resource: model.uri, versionId: undefined, textEdit: { range: new monaco.Range(mk.startLineNumber, mk.startColumn, mk.endLineNumber, mk.endColumn), text: k } }] },
            });
          }
        }
        const file = current ?? "";
        actions.push({
          title: `Fix with AI: ${mk.message.slice(0, 60)}${mk.message.length > 60 ? "…" : ""}`, kind: "quickfix", diagnostics: [mk],
          command: { id: "kloverleaf.ai.fix", title: "Fix with AI", arguments: [{ file, line: mk.startLineNumber, message: mk.message }] },
        });
      }
      void range;
      return { actions, dispose() { /* nothing to free */ } };
    },
  });
  monaco.editor.registerCommand("kloverleaf.ai.fix", (_accessor, arg: { file: string; line: number; message: string }) => {
    ai.askAbout(`Fix this compile problem at ${arg.file} line ${arg.line}: ${arg.message}. Change only what is needed, then compile to confirm.`, { file: arg.file, from: arg.line, to: arg.line, text: models.get(arg.file)?.getLineContent(arg.line) ?? "" });
  });
}

// ---- word count ----------------------------------------------------------------------

const wcEl = $("wc");
let wcTimer: number | undefined;
/** Prose words of the open file (commands, comments, math and preamble stripped), plus reading time. */
function renderWordCount(): void {
  window.clearTimeout(wcTimer);
  wcTimer = window.setTimeout(() => {
    const m = editor.getModel();
    if (!m) { wcEl.textContent = ""; return; }
    let t = m.getValue();
    const body = t.indexOf("\\begin{document}");
    if (body >= 0) t = t.slice(body + 16);
    t = t.replace(/%.*$/gm, " ")
      .replace(/\\begin\{(equation|align|gather|displaymath|eqnarray|verbatim|lstlisting|tikzpicture|tabular)\*?\}[\s\S]*?\\end\{\1\*?\}/g, " ")
      .replace(/\$\$[\s\S]*?\$\$|\\\[[\s\S]*?\\\]|\$[^$\n]*\$/g, " x ")
      .replace(/\\(label|ref|cref|Cref|eqref|cite\w*|includegraphics|usepackage|documentclass|input|include|bibliography\w*|vspace|hspace|newcommand|renewcommand)\*?(\[[^\]]*\])?\{[^}]*\}/g, " ")
      .replace(/\\[A-Za-z@]+\*?/g, " ")
      .replace(/[{}\[\]~]/g, " ");
    const words = t.match(/[\p{L}\p{N}][\p{L}\p{N}'’-]*/gu)?.length ?? 0;
    const minutes = Math.max(1, Math.round(words / 230));
    wcEl.textContent = words ? `${words.toLocaleString()} words · ${minutes} min` : "";
  }, 300);
}

let forwardRequest = 0;
/** Lights the cursor's word (or line) in the PDF. `quiet` skips the status line on a miss (clicks). */
async function forwardSync(quiet = false): Promise<void> {
  const pos = editor.getPosition();
  const model = editor.getModel();
  if (!pos || !current || !model || travel) return;
  const req = ++forwardRequest;
  try {
    const boxes = await api<SyncBox[]>(`/sync/forward?file=${encodeURIComponent(current)}&line=${pos.lineNumber}&col=${pos.column}`);
    if (req !== forwardRequest) return;
    if (boxes.length) pdf.highlight(boxes); else if (!quiet) status("no SyncTeX position for this line");
  } catch (e) { if (!quiet) status(`sync: ${(e as Error).message}`); }
}

async function inverseSync(page: number, x: number, y: number, jump: boolean): Promise<void> {
  try {
    const pos = await api<{ file: string; line: number; column: number } | null>(`/sync/inverse?page=${page}&x=${x.toFixed(2)}&y=${y.toFixed(2)}`);
    ai.setPdfClick({ page, x, y, file: pos?.file, line: pos?.line });
    // A click puts you on that word in the source, ready to type (jump = double-click, same thing).
    if (pos) open(pos.file, pos.line, pos.column > 0 ? pos.column : undefined, true);
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
      b.addEventListener("click", () => void postJson("/main", { file: f.path }));
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

// ---- keys: the cheat sheet ---------------------------------------------------------

const KEYS: [string, string][] = [
  ["⌘S", "compile now (+ push, if the project tracks a repository)"], ["⌥⏎ · click", "show this spot in the PDF"], ["click in the PDF", "go to that word"],
  ["⌘⇧A", "AI tab"], ["⌘⇧Q", "queue the selection for the AI"], ["⌘⇧M", "make this text mine (provenance)"],
  ["M-x", "command palette (every LaTeX command)"], ["⌘⌥E / ⌘⌥B", "emph / bold the selection"], ["⌘⌥N", "insert an environment"],
  ["F12 · ⌘-click", "go to the label, BibTeX entry or file"], ["⌘.", "quick fix (did you mean…, fix with AI)"],
  ["C-a C-e C-f C-b C-n C-p", "Emacs motion"], ["M-f M-b", "word motion"], ["M-] M-[  (M-} M-{)", "paragraph motion"], ["M-e M-a", "sentence motion"],
  ["C-k M-d M-DEL C-w M-w", "kill (the ring accumulates)"], ["C-y M-y", "yank, then cycle the ring"], ["M-q", "fill paragraph"],
  ["C-o", "open line"], ["C-s C-r", "incremental search"], ["C-SPC", "set mark"], ["C-x u", "undo"], ["C-g", "cancel"],
];
let keysEl: HTMLElement | null = null;
function showKeys(): void {
  if (keysEl) { keysEl.remove(); keysEl = null; return; }
  const box = document.createElement("div");
  box.id = "keys";
  const h = document.createElement("div");
  h.className = "pane-title";
  h.textContent = "keys";
  const x = document.createElement("button");
  x.className = "tiny";
  x.textContent = "×";
  x.addEventListener("click", () => { box.remove(); keysEl = null; });
  h.append(document.createElement("span"), x);
  (h.firstElementChild as HTMLElement).className = "spacer";
  box.appendChild(h);
  for (const [k, what] of KEYS) {
    const row = document.createElement("div");
    row.className = "key";
    const kk = document.createElement("kbd");
    kk.textContent = k;
    const w = document.createElement("span");
    w.textContent = what;
    row.append(kk, w);
    box.appendChild(row);
  }
  document.body.appendChild(box);
  keysEl = box;
  const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") { box.remove(); keysEl = null; document.removeEventListener("keydown", onKey); } };
  document.addEventListener("keydown", onKey);
}

// ---- access (admins and this project's members): share with an existing login, or
// ---- invite — a link that gives someone their own login for this project ------

const shareBtn = $<HTMLButtonElement>("btn-share");
const inviteBtn = $<HTMLButtonElement>("btn-invite");
if (document.body.dataset.manages === "1") {
  shareBtn.hidden = false;
  shareBtn.addEventListener("click", async () => {
    const user = window.prompt("Share this project with someone who already logs in here. Their login name:")?.trim();
    if (!user) return;
    const r = await api<{ members: string[] }>("/members", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ user }) })
      .catch((e) => { status(`share: ${String(e).slice(0, 80)}`); return null; });
    if (r) status(`shared with ${user} — members: ${r.members.join(", ")}`);
  });
  inviteBtn.hidden = false;
  inviteBtn.addEventListener("click", async () => {
    const r = await api<{ url: string }>("/invites", { method: "POST" }).catch((e) => { status(`invite: ${String(e).slice(0, 80)}`); return null; });
    if (!r) return;
    try { await navigator.clipboard.writeText(r.url); status("invite link copied — it works once"); } catch { /* fall through */ }
    window.prompt("Send this link. Whoever opens it picks a name and password and can only see this project. It works once.", r.url);
  });
}

// ---- outline: sections, figures, tables of the open file ----------------------------

const outlineEl = $("outline");
let outlineTimer: number | undefined;

/** Headings and floats of the file in the editor, as a clickable list. */
function renderOutline(): void {
  window.clearTimeout(outlineTimer);
  outlineTimer = window.setTimeout(() => {
    const m = editor.getModel();
    if (!m || travel) return;
    const items: { line: number; level: number; text: string; kind: string }[] = [];
    const lines = m.getLinesContent();
    let float: { kind: string; line: number } | null = null;
    for (let i = 0; i < lines.length; i++) {
      const l = lines[i];
      if (/^\s*%/.test(l)) continue;
      const h = /\\(part|chapter|section|subsection|subsubsection|paragraph)\*?\{([^}]*)/.exec(l);
      if (h) { items.push({ line: i + 1, level: ["part", "chapter", "section", "subsection", "subsubsection", "paragraph"].indexOf(h[1]), text: h[2].replace(/\\[A-Za-z]+\{?/g, "").replace(/[{}]/g, ""), kind: "heading" }); continue; }
      const f = /\\begin\{(figure|table|wrapfigure|algorithm|listing)\*?\}/.exec(l);
      if (f) { float = { kind: f[1], line: i + 1 }; continue; }
      const cap = float && /\\caption\{([^}]*)/.exec(l);
      if (float && cap) { items.push({ line: float.line, level: 9, text: cap[1].replace(/\\[A-Za-z]+\{?/g, "").replace(/[{}]/g, "").slice(0, 60), kind: float.kind }); float = null; continue; }
      if (float && /\\end\{(figure|table|wrapfigure|algorithm|listing)\*?\}/.test(l)) { items.push({ line: float.line, level: 9, text: `(${float.kind})`, kind: float.kind }); float = null; }
    }
    const pos = editor.getPosition()?.lineNumber ?? 0;
    const currentIdx = items.reduce((best, it, i) => (it.line <= pos ? i : best), -1);
    const headings = items.filter((it) => it.kind === "heading");
    outlineEl.replaceChildren(...items.map((it, i) => {
      const li = document.createElement("li");
      li.className = `${it.kind}${i === currentIdx ? " current" : ""}`;
      li.style.paddingLeft = `${0.6 + Math.min(it.level, 5) * 0.6}rem`;
      li.textContent = it.kind === "heading" ? it.text : `${it.kind === "table" ? "▦" : "▣"} ${it.text}`;
      li.title = `line ${it.line}`;
      if (it.kind === "heading" && current) {
        // How much of this section the AI wrote — the "make it mine" progress bar, per section.
        const next = headings.find((h) => h.line > it.line && h.level <= it.level);
        const share = prov.aiShare(current, it.line, (next?.line ?? lines.length + 1) - 1);
        if (share !== null && share >= 0.01) {
          const b = document.createElement("span");
          b.className = "ai-share";
          b.textContent = `${Math.round(share * 100)}%`;
          b.title = `${Math.round(share * 100)}% of this section is AI-written text`;
          li.appendChild(b);
        }
      }
      li.addEventListener("click", () => { editor.revealLineInCenter(it.line); editor.setPosition({ lineNumber: it.line, column: 1 }); editor.focus(); });
      return li;
    }));
    $("files").querySelector(".outline-title")!.toggleAttribute("hidden", items.length === 0);
  }, 150);
}

// ---- branches -----------------------------------------------------------------

interface BranchInfo { name: string; parent: string | null; from_entry: number | null; note: string; entries: number; head_ts: number | null }
const branchChip = $<HTMLButtonElement>("branch-chip");
let branchMenu: HTMLElement | null = null;

function renderBranchChip(): void {
  branchChip.textContent = `⎇ ${branchName}`;
  branchChip.classList.toggle("on", branchName !== "main");
}

branchChip.addEventListener("click", async () => {
  if (branchMenu) { branchMenu.remove(); branchMenu = null; return; }
  const r = await api<{ current: string; branches: BranchInfo[] }>("/branches").catch(() => null);
  if (!r) return;
  const menu = document.createElement("div");
  menu.id = "branch-menu";
  for (const b of r.branches) {
    const row = document.createElement("div");
    row.className = `item${b.name === r.current ? " current" : ""}`;
    row.textContent = `${b.name === r.current ? "● " : "○ "}${b.name}`;
    row.title = `${b.parent ? `forked from ${b.parent}` : "the trunk"}${b.note ? ` · ${b.note}` : ""} · ${b.entries} entries`;
    if (b.name !== r.current) row.addEventListener("click", () => { closeBranchMenu(); void switchBranch(b.name); });
    menu.appendChild(row);
  }
  const add = document.createElement("div");
  add.className = "item new";
  add.textContent = "+ branch from now…";
  add.addEventListener("click", () => { closeBranchMenu(); void createBranch(); });
  menu.appendChild(add);
  const rect = branchChip.getBoundingClientRect();
  menu.style.left = `${rect.left}px`;
  menu.style.top = `${rect.bottom + 4}px`;
  document.body.appendChild(menu);
  branchMenu = menu;
  setTimeout(() => document.addEventListener("mousedown", (e) => { if (branchMenu && !branchMenu.contains(e.target as Node) && e.target !== branchChip) closeBranchMenu(); }, { once: true }), 0);
});

function closeBranchMenu(): void { branchMenu?.remove(); branchMenu = null; }

async function switchBranch(name: string): Promise<void> {
  status(`switching to ${name}…`);
  await api(`/branches/${encodeURIComponent(name)}/switch`, { method: "POST" }).catch((e) => status(`switch failed: ${String(e).slice(0, 100)}`));
}

/** Forks a branch at an entry (or now) after asking for a name; the server then switches to it. */
async function createBranch(from?: number): Promise<void> {
  const name = window.prompt(from ? "name for a branch starting from that version" : "name for a branch starting from now", "")?.trim();
  if (!name) return;
  status(`branching ${name}…`);
  await postJson("/branches", { name, from }).catch((e) => status(`branch failed: ${String(e).slice(0, 100)}`));
}

// ---- history: time travel ----------------------------------------------------
//
// The history strip is a timeline. Scrubbing it swaps the editor to the
// document as it was at that entry (read-only, lines that have changed since
// shaded in place, marks where text was added later) and, once you pause,
// the PDF pane to that version compiled. "restore" adopts the version through
// the live document; "back to now" returns.

interface Entry { id: number; file: string; ts: number; author: string; kind: string; meta: Record<string, unknown>; first: number; count: number; branch?: string }
interface Bucket { t0: number; t1: number; count: number; first: number; last: number; kinds: Record<string, number>; authors: string[] }

const history = $("history");
const slider = $<HTMLInputElement>("history-slider");
const historyLabel = $("history-label");
const restoreBtn = $<HTMLButtonElement>("history-restore");
const nowBtn = $<HTMLButtonElement>("history-now");
const branchHereBtn = $<HTMLButtonElement>("history-branch");
let tree: TreeData | null = null;
branchHereBtn.addEventListener("click", () => { if (travel) void createBranch(travel.entry.id); });
let timeline: Entry[] = [];                 // oldest first; the slider's rightmost position is "now"
let buckets: Bucket[] = [];                 // the whole history as time slices (the density strip)
let loaded: { first: number; last: number } | null = null; // the id range the timeline currently shows
let anchor = -1;                            // strip: shift-click extends from here
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

const fileQuery = () => (current ? `&file=${encodeURIComponent(current)}` : "");

/** Shows the drawer and (re)loads the density strip for the open file. */
async function showDrawer(): Promise<void> {
  history.hidden = false;
  $("btn-history").classList.add("on");
  buckets = await api<Bucket[]>(`/history/summary?buckets=140${fileQuery()}`).catch(() => [] as Bucket[]);
}

async function openHistory(): Promise<void> {
  await showDrawer();
  tree = await api<TreeData>("/history/tree").catch(() => null);
  await loadWindow();
  renderTreeMap();
  describe(null);
  slider.focus();
}

function renderTreeMap(): void {
  const box = $("history-tree");
  if (!tree || tree.branches.length < 2 && tree.events.length < 2) { box.hidden = true; return; }
  box.hidden = false;
  renderTree(box, tree, {
    preview: (id) => void travelTo(id),
    switchTo: (name) => void switchBranch(name),
    branchAt: (id) => void createBranch(id),
  }, travel?.entry.id);
}

/** Loads one stretch of the timeline (by entry id); no bounds = the latest. */
async function loadWindow(after?: number, before?: number, limit = 200, branch?: string): Promise<void> {
  const q = `/history?limit=${limit}${after !== undefined ? `&after=${after}` : ""}${before !== undefined ? `&before=${before}` : ""}${branch ? `&branch=${encodeURIComponent(branch)}` : ""}${fileQuery()}`;
  const entries = await api<Entry[]>(q).catch(() => [] as Entry[]);
  timeline = entries.reverse();
  loaded = timeline.length ? { first: Math.min(timeline[0].first || timeline[0].id, timeline[0].id), last: timeline[timeline.length - 1].id } : null;
  slider.max = String(timeline.length);
  slider.value = String(timeline.length);
  renderChips();
  renderStrip();
}

/** The density strip: one bar per time slice; click a bar (shift-click to extend) to load that stretch. */
function renderStrip(): void {
  const strip = $("history-strip");
  const total = buckets.reduce((n, b) => n + b.count, 0);
  const max = Math.max(1, ...buckets.map((b) => b.count));
  strip.replaceChildren(...buckets.map((b, i) => {
    const bar = document.createElement("div");
    const ai = b.kinds.ai ?? 0;
    bar.className = `bar${ai > 0 ? " ai" : ""}${loaded && b.count && b.last >= loaded.first && b.first <= loaded.last ? " in" : ""}`;
    bar.style.height = `${Math.max(4, Math.round((100 * b.count) / max))}%`;
    bar.title = `${when(b.t0)} – ${new Date(b.t1).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })} · ${b.count} change${b.count === 1 ? "" : "s"}${ai ? ` · ${ai} by AI` : ""}`;
    bar.addEventListener("click", (e) => {
      if (!b.count) return;
      const [a, z] = e.shiftKey && anchor >= 0 ? [Math.min(anchor, i), Math.max(anchor, i)] : [i, i];
      if (!e.shiftKey) anchor = i;
      const inRange = buckets.slice(a, z + 1).filter((x) => x.count);
      void loadWindow(inRange[0].first - 1, inRange[inRange.length - 1].last + 1, 500);
    });
    return bar;
  }));
  const range = $("history-range");
  range.textContent = timeline.length
    ? `showing ${timeline.length} of ${total} changes · ${when(timeline[0].ts)} → ${when(timeline[timeline.length - 1].ts)}${total > timeline.length ? " · click the strip to look elsewhere" : ""}`
    : total ? `${total} changes · click the strip` : "no history yet";
}

/** Jumps the drawer to one entry (from a provenance popover): the text as it was then. */
async function travelTo(id: number): Promise<void> {
  if (history.hidden) await showDrawer();
  const indexOf = () => timeline.findIndex((e) => e.id === id || (e.first <= id && id <= e.id));
  let i = indexOf();
  if (i < 0) {
    await loadWindow(id - 200, id + 200, 400, "*");
    i = indexOf();
  }
  if (i < 0) { status("that version is not in this file's history"); return; }
  slider.value = String(i);
  await scrub();
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
    c.textContent = `${new Date(e.ts).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })} ${e.author}`;
    c.title = `${new Date(e.ts).toLocaleString()} · ${whatHappened(e)}`;
    c.addEventListener("click", () => { slider.value = String(i); void scrub(); });
    return c;
  }), (() => { const n = document.createElement("span"); n.className = "chip now"; n.textContent = "now"; n.addEventListener("click", () => { slider.value = String(timeline.length); void scrub(); }); return n; })());
  markChip();
}

function markChip(): void {
  const i = Number(slider.value);
  const chips = $("history-chips");
  chips.querySelectorAll(".chip").forEach((c, j) => {
    c.classList.toggle("current", j === i && i < timeline.length);
    if (j !== i) return;
    // Keep the current chip visible without the strip lurching on every step.
    const el = c as HTMLElement;
    const left = el.offsetLeft - chips.scrollLeft, right = left + el.offsetWidth;
    if (left < 0 || right > chips.clientWidth) el.scrollIntoView({ inline: "nearest", block: "nearest" });
  });
}

function describe(entry: Entry | null, extra = ""): void {
  historyLabel.textContent = entry
    ? `${when(entry.ts)} · ${entry.author}${entry.branch && entry.branch !== branchName ? ` · ⎇ ${entry.branch}` : ""} · ${whatHappened(entry)}${extra}`
    : "now";
  restoreBtn.disabled = !entry;
  nowBtn.disabled = !entry;
  branchHereBtn.disabled = !entry;
  if (tree) renderTreeMap();
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
