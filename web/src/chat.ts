// The project chat, and comments: a message pinned to a point in the paper.
// Messages live on the server (src/chat.rs) and arrive over the editor's
// socket. A pinned message shows three ways: in this list, as a coloured
// mark in the editor's gutter on its line, and as a pin on the PDF page
// (placed with SyncTeX, so it follows the text from build to build). Pins
// never touch the .tex files.
//
// Lines move as people type, so an anchor also keeps the line's text
// (`quote`); `locate` finds it again near where it was.

import type { ChatAnchor, ChatMsg, ClientMsg, SyncSpot } from "./protocol.js";
import { me, when } from "./protocol.js";
import { colorOf, ensureUserStyle } from "./people.js";
import * as ai from "./ai.js";

export interface Pin { id: number; page: number; x: number; y: number; color: string; title: string }

export interface ChatHost {
  send(m: ClientMsg): boolean;
  editor(): monaco.editor.IStandaloneCodeEditor;
  current(): string | null;
  model(file: string): monaco.editor.ITextModel | undefined;
  open(file: string, line?: number): void;
  forward(file: string, line: number): Promise<SyncSpot[]>;
  setPins(pins: Pin[]): void;
  status(s: string): void;
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const panel = $("chat-panel");
const list = $("chat-list");
const emptyEl = $("chat-empty");
const textEl = $<HTMLTextAreaElement>("chat-text");
const anchorEl = $("chat-anchor");
const tabChat = $("tab-chat");
const tabPdf = $("tab-pdf");
const tabAi = $("tab-ai");
const resolvedBox = $<HTMLInputElement>("chat-resolved");

let host: ChatHost;
const msgs = new Map<number, ChatMsg>();
let pending: ChatAnchor | null = null;
let armed = false;
let unread = 0;
let glyphs: monaco.editor.IEditorDecorationsCollection | undefined;
let pdfVersion = 0;
const spotCache = new Map<string, SyncSpot | null>();

export function initChat(h: ChatHost): void {
  host = h;
  glyphs = host.editor().createDecorationsCollection();
  tabChat.addEventListener("click", () => showChat(true));
  // The AI module switches between its tab and the PDF; either one hides the chat.
  document.addEventListener("kl-pane", (e) => { if ((e as CustomEvent).detail !== "chat") hide(); });
  $("chat-send").addEventListener("click", post);
  $("chat-pin-here").addEventListener("click", () => { pinAtCursor(); textEl.focus(); });
  $("chat-pin-pdf").addEventListener("click", armPdf);
  $("pdf-comment").addEventListener("click", armPdf);
  resolvedBox.addEventListener("change", render);
  textEl.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); post(); }
    if (e.key === "Escape") { pending = null; renderAnchor(); }
  });
  host.editor().onMouseDown((e) => {
    if (e.target.type !== monaco.editor.MouseTargetType.GUTTER_GLYPH_MARGIN || !e.target.position) return;
    const file = host.current();
    const line = e.target.position.lineNumber;
    const hit = [...msgs.values()].find((m) => m.anchor?.file === file && !m.resolved && locate(m.anchor!) === line);
    if (hit) focusMessage(hit.id);
  });
  let timer: number | undefined;
  host.editor().onDidChangeModelContent(() => { window.clearTimeout(timer); timer = window.setTimeout(refreshMarks, 600); });
  // (After the switch: the editor changes model before the page records which file it is.)
  host.editor().onDidChangeModel(() => setTimeout(refreshGlyphs, 0));
}

/** On connect: the server's recent messages. */
export function setMessages(all: ChatMsg[]): void {
  msgs.clear();
  for (const m of all) msgs.set(m.id, m);
  render();
  refreshMarks();
}

/** A message, new or changed (resolved, deleted). */
export function chatEvent(m: ChatMsg): void {
  const fresh = !msgs.has(m.id);
  if (m.deleted) msgs.delete(m.id); else msgs.set(m.id, m);
  if (fresh && !m.deleted && m.user !== me && panel.hidden) { unread++; renderTab(); }
  render();
  refreshMarks();
  if (fresh && m.user === me) list.scrollTop = list.scrollHeight;
}

/** A new PDF: pins are placed again (SyncTeX positions moved with it). */
export function pdfChanged(version: number): void {
  if (version === pdfVersion) return;
  pdfVersion = version;
  spotCache.clear();
  void refreshPins();
}

// ---- the panel ------------------------------------------------------------------

export function showChat(on: boolean): void {
  if (on) {
    ai.toggleAi(false);            // hides the AI panel (and would hide us: so we show after)
    panel.hidden = false;
    document.body.classList.add("chat-open");
    tabChat.classList.add("on");
    tabPdf.classList.remove("on");
    tabAi.classList.remove("on");
    unread = 0; renderTab();
    list.scrollTop = list.scrollHeight;
    textEl.focus();
  } else hide();
}

function hide(): void {
  if (panel.hidden) return;
  panel.hidden = true;
  document.body.classList.remove("chat-open");
  tabChat.classList.remove("on");
}

function renderTab(): void {
  tabChat.textContent = unread ? `💬 chat · ${unread}` : "💬 chat";
  tabChat.classList.toggle("unread", unread > 0);
}

function render(): void {
  const all = [...msgs.values()].sort((a, b) => a.id - b.id);
  const shown = all.filter((m) => resolvedBox.checked || !m.resolved);
  emptyEl.hidden = shown.length > 0;
  const atBottom = list.scrollHeight - list.scrollTop - list.clientHeight < 40;
  list.replaceChildren(emptyEl, ...shown.map(row));
  if (atBottom) list.scrollTop = list.scrollHeight;
}

function row(m: ChatMsg): HTMLElement {
  const el = document.createElement("div");
  el.className = `chat-msg${m.resolved ? " resolved" : ""}${m.anchor ? " pinned" : ""}`;
  el.dataset.id = String(m.id);
  el.style.setProperty("--who", colorOf(m.user));
  const head = document.createElement("div");
  head.className = "head";
  const name = document.createElement("strong");
  name.textContent = m.user === me ? `${m.user} (you)` : m.user;
  const time = document.createElement("span");
  time.className = "muted";
  time.textContent = when(m.ts);
  head.append(name, time);
  if (m.anchor) {
    const a = m.anchor;
    const chip = document.createElement("button");
    chip.className = "tiny anchor";
    chip.textContent = `📍 ${a.file}:${locate(a)}`;
    chip.title = a.quote ? `on: ${a.quote}` : "go there";
    chip.addEventListener("click", () => goTo(m));
    head.appendChild(chip);
  }
  const spacer = document.createElement("span");
  spacer.className = "spacer";
  head.appendChild(spacer);
  if (m.anchor) {
    const done = document.createElement("button");
    done.className = "tiny";
    done.textContent = m.resolved ? "reopen" : "✓ resolve";
    done.title = m.resolved ? `resolved by ${m.resolved_by ?? "?"} — reopen` : "mark this comment done (its pin goes away)";
    done.addEventListener("click", () => host.send({ type: "chat-resolve", id: m.id, resolved: !m.resolved }));
    head.appendChild(done);
  }
  if (m.user === me) {
    const del = document.createElement("button");
    del.className = "tiny";
    del.textContent = "×";
    del.title = "delete this message";
    del.addEventListener("click", () => { if (confirm("Delete this message?")) host.send({ type: "chat-delete", id: m.id }); });
    head.appendChild(del);
  }
  const body = document.createElement("div");
  body.className = "text";
  body.textContent = m.text;
  el.append(head, body);
  return el;
}

function focusMessage(id: number): void {
  showChat(true);
  const m = msgs.get(id);
  if (m?.resolved && !resolvedBox.checked) { resolvedBox.checked = true; render(); }
  const el = list.querySelector<HTMLElement>(`[data-id="${id}"]`);
  if (!el) return;
  el.scrollIntoView({ block: "center", behavior: "smooth" });
  el.classList.remove("flash");
  void el.offsetWidth;
  el.classList.add("flash");
}

function goTo(m: ChatMsg): void {
  if (!m.anchor) return;
  host.open(m.anchor.file, locate(m.anchor));
}

// ---- composing --------------------------------------------------------------------

function post(): void {
  const text = textEl.value.trim();
  if (!text) return;
  if (!host.send({ type: "chat", text, anchor: pending ?? undefined })) { host.status("offline — the message was not sent"); return; }
  textEl.value = "";
  pending = null;
  renderAnchor();
}

function renderAnchor(): void {
  anchorEl.replaceChildren();
  anchorEl.hidden = !pending;
  if (!pending) return;
  const label = document.createElement("span");
  label.textContent = `📍 comment on ${pending.file}:${pending.line}${pending.pdf ? ` · page ${pending.pdf.page}` : ""}`;
  label.title = pending.quote;
  const x = document.createElement("button");
  x.className = "tiny";
  x.textContent = "×";
  x.title = "send as a plain message instead";
  x.addEventListener("click", () => { pending = null; renderAnchor(); });
  anchorEl.append(label, x);
}

const quoteOf = (file: string, line: number): string => host.model(file)?.getLineContent(line).trim().slice(0, 200) ?? "";

/** Comment on the editor's cursor line (⌘⌥M, or the 📍 button). */
export function pinAtCursor(): void {
  const file = host.current();
  const pos = host.editor().getPosition();
  if (!file || !pos) { host.status("put the cursor where the comment goes"); return; }
  pending = { file, line: pos.lineNumber, quote: quoteOf(file, pos.lineNumber) };
  renderAnchor();
  showChat(true);
}

/** The next click on the PDF places a comment there. */
function armPdf(): void {
  hide();
  ai.toggleAi(false);
  armed = true;
  document.body.classList.add("pdf-commenting");
  host.status("click the spot in the PDF to comment on (Esc cancels)");
}

export const pdfArmed = (): boolean => armed;

export function disarm(): void {
  armed = false;
  document.body.classList.remove("pdf-commenting");
}

/** The armed click landed: `file`/`line` from SyncTeX, when it found the source. */
export function pdfComment(page: number, x: number, y: number, file: string | undefined, line: number | undefined): void {
  disarm();
  if (!file || !line) { host.status("no source position at that spot — try on the text"); return; }
  pending = { file, line, quote: quoteOf(file, line), pdf: { page, x, y } };
  host.status("");
  renderAnchor();
  showChat(true);
}

document.addEventListener("keydown", (e) => { if (e.key === "Escape" && armed) { disarm(); host.status(""); } });

// ---- where comments are -------------------------------------------------------------

/** The anchor's line now: its quote on the old line, else the nearest line that has it, else the old line. */
export function locate(a: ChatAnchor): number {
  const m = host?.model(a.file);
  if (!m) return a.line;
  const n = m.getLineCount();
  const old = Math.min(Math.max(1, a.line), n);
  if (!a.quote) return old;
  const has = (l: number) => l >= 1 && l <= n && m.getLineContent(l).includes(a.quote);
  if (has(old)) return old;
  for (let d = 1; d < n; d++) {
    if (has(old - d)) return old - d;
    if (has(old + d)) return old + d;
  }
  return old;
}

function refreshMarks(): void {
  refreshGlyphs();
  void refreshPins();
  // The 📍 file:line chips follow moved lines too.
  for (const el of list.querySelectorAll<HTMLElement>(".chat-msg.pinned")) {
    const m = msgs.get(Number(el.dataset.id));
    const chip = el.querySelector(".anchor");
    if (m?.anchor && chip) chip.textContent = `📍 ${m.anchor.file}:${locate(m.anchor)}`;
  }
}

function refreshGlyphs(): void {
  const file = host?.current();
  if (!glyphs || !file) return;
  const open = [...msgs.values()].filter((m) => m.anchor?.file === file && !m.resolved);
  glyphs.set(open.map((m) => {
    const line = locate(m.anchor!);
    const cls = ensureUserStyle(m.user);
    return {
      range: new monaco.Range(line, 1, line, 1),
      options: {
        isWholeLine: true,
        glyphMarginClassName: `chat-glyph ${cls}`,
        glyphMarginHoverMessage: { value: `💬 **${m.user}**: ${m.text.replace(/[\\`*_[\]<>]/g, "\\$&")}` },
        className: `rc-line ${cls}`,
        overviewRuler: { color: colorOf(m.user), position: monaco.editor.OverviewRulerLane.Right },
      },
    };
  }));
}

let pinRequest = 0;
async function refreshPins(): Promise<void> {
  if (!host) return;
  const req = ++pinRequest;
  const open = [...msgs.values()].filter((m) => m.anchor && !m.resolved);
  const pins: Pin[] = [];
  for (const m of open) {
    const a = m.anchor!;
    const line = locate(a);
    let spot: SyncSpot | null = null;
    // A PDF click is exact while its line is still where it was.
    if (a.pdf && line === a.line) spot = { page: a.pdf.page, x: a.pdf.x, y: a.pdf.y };
    else {
      const key = `${a.file}:${line}`;
      if (!spotCache.has(key)) spotCache.set(key, (await host.forward(a.file, line).catch(() => []))[0] ?? null);
      spot = spotCache.get(key) ?? (a.pdf ? { page: a.pdf.page, x: a.pdf.x, y: a.pdf.y } : null);
    }
    if (req !== pinRequest) return;
    if (spot) pins.push({ id: m.id, page: spot.page, x: spot.x, y: spot.y, color: colorOf(m.user), title: `${m.user}: ${m.text}` });
  }
  host.setPins(pins);
}

/** A pin on the PDF was clicked. */
export function pinClicked(id: number): void {
  focusMessage(id);
}
