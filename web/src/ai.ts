// The AI tab of the right-hand pane. The pane has two tabs, PDF and AI; the
// PDF viewer stays alive underneath while the AI tab is up. The AI view is a
// conversation: the author's prompts as bubbles, the model's replies as
// prose, each run's tool activity folded into one "working… · N steps" row
// (with the live phase inside it), errors as a red card, and the proposal as
// a card per file with an editable inline Monaco diff — apply one file, apply
// all, or discard. The composer at the bottom carries context chips (cursor,
// selection, PDF click, queued regions with notes). Follow-ups continue the
// same session. Protocol in src/ai/mod.rs.

import type { AiChoice, AiEvent, ApplyFile, AiMode, ClientMsg, Context, PdfClick, ProposedFile, Region } from "./protocol.js";
import { api, me, when } from "./protocol.js";

/** A conversation as the server lists it (see /api/projects/<p>/ai/threads). */
interface ThreadInfo { id: number; title: string; user: string; status: string; choice: AiChoice; created: number; updated: number; messages: number }

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
const threadEl = $("ai-thread");
const emptyEl = $("ai-empty");
const barEl = $("ai-bar");
const ctxEl = $("ai-ctx");
const queueEl = $("ai-queue");
const promptEl = $<HTMLTextAreaElement>("ai-prompt");
const statusEl = $("ai-status");
const modeEl = $("ai-mode");
const runBtn = $<HTMLButtonElement>("ai-run");
const cancelBtn = $<HTMLButtonElement>("ai-cancel");
const tabPdf = $("tab-pdf");
const tabAi = $("tab-ai");
// The conversation switcher, above the thread: conversations persist on the
// server and survive reloads and restarts; pick one up or start fresh.
const threadsBar = document.createElement("div");
threadsBar.id = "ai-threads";
const threadSel = document.createElement("select");
threadSel.title = "earlier conversations on this project";
const newBtn = document.createElement("button");
newBtn.className = "small";
newBtn.textContent = "+ new";
newBtn.title = "start a new conversation";
threadsBar.append(threadSel, newBtn);
let threads: ThreadInfo[] = [];

/** One run's activity row: a <details> whose summary is "working… · N steps" + the live phase. */
interface RunRow { el: HTMLDetailsElement; label: HTMLElement; phase: HTMLElement; body: HTMLElement; steps: number }

let host: AiHost;
let mode: AiMode = { mode: "off" };
let choice: AiChoice | undefined;          // the picker: which agent, model, effort
const modelSel = document.createElement("select");
const effortSel = document.createElement("select");
let queue: Region[] = [];
let lastPdf: PdfClick | undefined;
let session: number | undefined;
let running = false;
let run: RunRow | undefined;
let proposalEl: HTMLElement | undefined;
let diffEditors: { path: string; deleted: boolean; editor: monaco.editor.IStandaloneDiffEditor }[] = [];

/** What applying one card sends: a removed file stays removed unless its text was typed back in. */
function picked(d: { path: string; deleted: boolean; editor: monaco.editor.IStandaloneDiffEditor }): ApplyFile {
  const text = d.editor.getModel()!.modified.getValue();
  return d.deleted && text === "" ? { path: d.path, text, delete: true } : { path: d.path, text };
}

export function initAi(h: AiHost): void {
  host = h;
  runBtn.addEventListener("click", send);
  cancelBtn.addEventListener("click", () => { if (session !== undefined) host.send({ type: "ai", action: "cancel", session }); });
  tabPdf.addEventListener("click", showPdfTab);
  tabAi.addEventListener("click", showAiTab);
  promptEl.addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") { e.preventDefault(); send(); }
  });
  promptEl.addEventListener("input", grow);
  threadEl.parentElement!.insertBefore(threadsBar, threadEl);
  threadSel.addEventListener("change", () => { const id = Number(threadSel.value); if (id) void openThread(id); else startFresh(); });
  newBtn.addEventListener("click", startFresh);
  renderQueue();
  renderContext();
}

/** On connect and reconnect: the server's conversations, and the one to show. */
export async function restore(): Promise<void> {
  const r = await api<{ threads: ThreadInfo[] }>("/ai/threads").catch(() => ({ threads: [] as ThreadInfo[] }));
  threads = r.threads;
  renderThreads();
  const pick = session !== undefined && threads.some((t) => t.id === session)
    ? session
    : threads.find((t) => t.status === "running" || t.status === "idle" || t.status === "interrupted")?.id;
  if (pick !== undefined) await openThread(pick);
  else startFresh();
}

function renderThreads(): void {
  const opts = threads.map((t) => {
    const o = document.createElement("option");
    o.value = String(t.id);
    const mark = t.status === "running" ? "● " : t.status === "interrupted" ? "⚠ " : t.status === "applied" ? "✓ " : t.status === "discarded" ? "× " : "";
    o.textContent = `${mark}${t.title.slice(0, 48)}${t.title.length > 48 ? "…" : ""} · ${when(t.updated)}`;
    return o;
  });
  const fresh = document.createElement("option");
  fresh.value = "0";
  fresh.textContent = "new conversation";
  threadSel.replaceChildren(fresh, ...opts);
  threadSel.value = String(session ?? 0);
  threadsBar.hidden = threads.length === 0;
}

function clearThread(): void {
  clearProposal();
  finishRun("");
  run = undefined;
  for (const child of [...threadEl.children]) if (child !== emptyEl) child.remove();
  emptyEl.hidden = false;
}

function startFresh(): void {
  session = undefined;
  setRunning(false);
  clearThread();
  threadSel.value = "0";
  promptEl.focus();
}

/** Shows a stored conversation: its messages replayed, its proposal if still open. */
async function openThread(id: number): Promise<void> {
  const r = await api<{ thread: { id: number; status: string; choice: AiChoice; messages: AiEvent[] }; proposal: ProposedFile[] | null }>(`/ai/threads/${id}`).catch(() => null);
  if (!r) { startFresh(); return; }
  clearThread();
  session = id;
  threadSel.value = String(id);
  setRunning(false);
  for (const ev of r.thread.messages) replay(ev);
  finishRun("done");
  if (r.thread.status === "running") { setRunning(true); run = startRun(); }
  else if (r.proposal && r.thread.status !== "applied" && r.thread.status !== "discarded") showProposal(r.proposal);
  if (r.thread.status === "applied" || r.thread.status === "discarded") {
    msg("note", r.thread.status === "applied" ? "this conversation was applied — a new question starts a new one" : "this conversation was discarded — a new question starts a new one");
    session = undefined;
  }
  threadEl.scrollTop = threadEl.scrollHeight;
}

/** A stored event, rendered like a live one (minus running-state changes). */
function replay(ev: AiEvent): void {
  switch (ev.event) {
    case "started": finishRun("done"); msg("prompt", ev.prompt); run = startRun(); break;
    case "turn": msg("turn", ev.text, ev.role); break;
    case "tool": toolRow(ev); break;
    case "proposal": finishRun("done"); if (ev.text) msg("turn", ev.text); break;
    case "error": finishRun("failed"); msg("error", ev.message); break;
    case "cancelled": finishRun("cancelled"); msg("note", "cancelled"); break;
    case "applied": msg("note", `applied: ${(ev.files as string[]).join(", ") || "nothing changed"}`); break;
    case "discarded": msg("note", "discarded"); break;
    default: break;
  }
}

/** The agent picker: a model menu grouped by agent (Claude, Codex) and an effort menu. */
export function setMode(m: AiMode): void {
  mode = m;
  runBtn.disabled = m.mode === "off" || running;
  if (m.mode === "off" || !m.providers?.length) {
    modeEl.textContent = `off — ${m.reason ?? ""}`;
    return;
  }
  let saved: AiChoice | undefined;
  try { saved = JSON.parse(localStorage.getItem("ai-choice") ?? "null") ?? undefined; } catch { /* ignore */ }
  const valid = (c?: AiChoice) => !!c && m.providers!.some((p) => p.id === c.provider && p.models.includes(c.model) && p.efforts.includes(c.effort));
  choice = valid(saved) ? saved : m.default;
  modelSel.id = "ai-model";
  modelSel.title = "which agent and model writes the proposal";
  modelSel.replaceChildren(...m.providers.map((p) => {
    const g = document.createElement("optgroup");
    g.label = p.label;
    for (const mod of p.models) {
      const o = document.createElement("option");
      o.value = `${p.id}/${mod}`;
      o.textContent = mod.replace(/^claude-/, "").replace(/-\d{8}$/, "");
      g.appendChild(o);
    }
    return g;
  }));
  effortSel.id = "ai-effort";
  effortSel.title = "how hard it thinks";
  const unavailable = Object.entries(m.unavailable ?? {}).map(([k, v]) => `${k}: ${v}`).join("; ");
  modelSel.onchange = () => {
    const [provider, model] = modelSel.value.split("/") as [AiChoice["provider"], string];
    const p = m.providers!.find((x) => x.id === provider)!;
    const switched = choice && choice.provider !== provider;
    const effort = p.efforts.includes(choice?.effort ?? "") ? choice!.effort : p.efforts.includes("medium") ? "medium" : p.efforts[0];
    choice = { provider, model, effort };
    renderEfforts(p.efforts);
    persist();
    if (switched && session !== undefined) {
      session = undefined;
      msg("note", `switched to ${p.label}: the next question starts a new conversation (the open proposal stays until you apply or discard it)`);
    }
  };
  effortSel.onchange = () => { if (choice) { choice = { ...choice, effort: effortSel.value }; persist(); } };
  modelSel.value = `${choice!.provider}/${choice!.model}`;
  renderEfforts(m.providers.find((p) => p.id === choice!.provider)!.efforts);
  modeEl.replaceChildren(modelSel, effortSel);
  if (unavailable) modeEl.title = `not offered — ${unavailable}`;
}

function renderEfforts(efforts: string[]): void {
  effortSel.replaceChildren(...efforts.map((e) => { const o = document.createElement("option"); o.value = e; o.textContent = e; return o; }));
  effortSel.value = choice?.effort ?? efforts[0];
}

function persist(): void {
  try { localStorage.setItem("ai-choice", JSON.stringify(choice)); } catch { /* ignore */ }
}

/** Switches the right-hand pane to the AI tab (true), the PDF tab (false), or the other one. */
export function toggleAi(show?: boolean): void {
  const on = show ?? panel.hidden;
  panel.hidden = !on;
  tabAi.classList.toggle("on", on);
  tabPdf.classList.toggle("on", !on);
  $("btn-ai").classList.toggle("on", on);
  document.body.classList.toggle("ai-open", on);
  if (on) { renderContext(); promptEl.focus(); }
}

const showAiTab = (): void => toggleAi(true);
const showPdfTab = (): void => toggleAi(false);

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

/** Opens the AI tab with a prompt (and a region queued), and sends it right away. */
export function askAbout(prompt: string, region?: Region): void {
  if (region) { queue.push(region); renderQueue(); }
  toggleAi(true);
  promptEl.value = prompt;
  grow();
  if (mode.mode !== "off" && !running) send();
}

// ---- context chips -----------------------------------------------------------

function chip(kind: string, key: string, text: string): HTMLElement {
  const el = document.createElement("span");
  el.className = `ai-chip ${kind}`;
  const k = document.createElement("span");
  k.className = "k";
  k.textContent = key;
  el.append(k, document.createTextNode(text));
  return el;
}

/** Called by the editor whenever the cursor moves (cheap: three chips, no inputs). */
export function renderContext(): void {
  if (panel.hidden || !host) return;
  const c = host.context();
  const chips: HTMLElement[] = [];
  if (c.file && c.line) chips.push(chip("cursor", "cursor", `${c.file}:${c.line}`));
  if (c.selection) chips.push(chip("selection", "selection", `${c.selection.from}–${c.selection.to}`));
  if (lastPdf) chips.push(chip("pdf", "pdf", `p.${lastPdf.page}${lastPdf.line ? ` → ${lastPdf.file}:${lastPdf.line}` : ""}`));
  ctxEl.replaceChildren(...chips);
}

function renderQueue(): void {
  queueEl.replaceChildren();
  queue.forEach((r, i) => {
    const pill = document.createElement("span");
    pill.className = "ai-chip queued";
    const link = document.createElement("a");
    link.href = "#";
    link.textContent = `${r.file}:${r.from}${r.to !== r.from ? `–${r.to}` : ""}`;
    link.title = "show in the editor";
    link.addEventListener("click", (e) => { e.preventDefault(); host.open(r.file, r.from); });
    const note = document.createElement("input");
    note.className = "note";
    note.placeholder = "note";
    note.value = r.note ?? "";
    note.addEventListener("input", () => { r.note = note.value || undefined; });
    const x = document.createElement("button");
    x.className = "x";
    x.textContent = "×";
    x.title = "remove";
    x.addEventListener("click", () => { queue.splice(i, 1); renderQueue(); });
    pill.append(link, note, x);
    queueEl.appendChild(pill);
  });
}

function context(): Context {
  const c = host.context();
  return { file: c.file, line: c.line, column: c.column, selection: c.selection, queue: queue.slice(), pdf: lastPdf };
}

// ---- composer ----------------------------------------------------------------

function grow(): void {
  promptEl.style.height = "auto";
  promptEl.style.height = `${Math.min(promptEl.scrollHeight, window.innerHeight * 0.3)}px`;
}

function send(): void {
  const prompt = promptEl.value.trim();
  if (!prompt || running || mode.mode === "off") return;
  host.send({ type: "ai", action: "run", prompt, context: context(), session, ...(choice ?? {}) });
  promptEl.value = "";
  grow();
  setRunning(true);
}

function setRunning(on: boolean): void {
  running = on;
  runBtn.disabled = on || mode.mode === "off";
  cancelBtn.hidden = !on;
  panel.classList.toggle("running", on);
  statusEl.textContent = on ? "running…" : "";
}

// ---- the thread --------------------------------------------------------------

function append(el: HTMLElement): void {
  emptyEl.hidden = true;
  threadEl.appendChild(el);
  threadEl.scrollTop = threadEl.scrollHeight;
}

function hasEntries(): boolean {
  return threadEl.children.length > 1; // the empty-state hint is always the first child
}

/** A pink tag naming a role other than the assistant's (a reviewer, a second model). */
function roleTag(el: HTMLElement, role?: string): void {
  if (!role || role === "assistant") return;
  const tag = document.createElement("span");
  tag.className = "role";
  tag.textContent = role;
  el.appendChild(tag);
}

/** A message in the thread: the author's prompt (bubble), the model's prose, a note, or an error card. */
function msg(kind: string, text: string, role?: string): HTMLElement {
  const el = document.createElement("div");
  el.className = `ai-msg ${kind}`;
  roleTag(el, role);
  el.appendChild(document.createTextNode(text));
  append(el);
  return el;
}

/** One tool call in the current run's row; a pending row ("…") is replaced by its done twin. */
function toolRow(ev: Extract<AiEvent, { event: "tool" }>): void {
  const r = run ?? (run = startRun());
  const args = Object.entries(ev.input).map(([k, v]) => `${k}=${typeof v === "string" ? v : JSON.stringify(v)}`).join(" ");
  const line = `${ev.name} ${args} → ${ev.summary}`;
  const last = r.body.lastElementChild as HTMLElement | null;
  if (last?.dataset.pending === ev.name && ev.summary !== "…") {
    last.textContent = line;
    last.dataset.pending = "";
    last.classList.toggle("error", !!ev.error);
  } else {
    const el = document.createElement("div");
    el.className = `tool${ev.error ? " error" : ""}`;
    roleTag(el, ev.role);
    el.appendChild(document.createTextNode(line));
    el.dataset.pending = ev.summary === "…" ? ev.name : "";
    r.body.appendChild(el);
    r.steps++;
  }
  r.label.textContent = `working… · ${stepsText(r.steps)}`;
  threadEl.scrollTop = threadEl.scrollHeight;
}

/** Updates the switcher's entry for the open conversation. */
function markThread(status: string): void {
  const t = threads.find((x) => x.id === session);
  if (t) t.status = status;
  renderThreads();
}

function startRun(): RunRow {
  const el = document.createElement("details");
  el.className = "ai-run";
  el.open = true;
  const summary = document.createElement("summary");
  const label = document.createElement("span");
  label.className = "label";
  label.textContent = "working…";
  const phase = document.createElement("span");
  phase.className = "phase";
  summary.append(label, phase);
  const body = document.createElement("div");
  body.className = "steps";
  el.append(summary, body);
  append(el);
  return { el, label, phase, body, steps: 0 };
}

const stepsText = (n: number) => `${n} step${n === 1 ? "" : "s"}`;

/** Ends the current run's row: final label, no phase, folded. */
function finishRun(word: string): void {
  if (!run) return;
  run.label.textContent = `${word} · ${stepsText(run.steps)}`;
  run.phase.textContent = "";
  run.el.open = false;
  run = undefined;
}

/** Events from the socket; ones from other people's sessions are shown as a line only. */
export function aiEvent(ev: AiEvent): void {
  if (ev.user !== me) {
    if (ev.event === "started") msg("note", `${ev.user} asked the AI: ${ev.prompt}`);
    if (ev.event === "applied") msg("note", `${ev.user} applied AI changes to ${ev.files.join(", ")}`);
    return;
  }
  switch (ev.event) {
    case "started":
      if (session !== undefined && ev.session !== session) break; // another conversation of mine (other tab)
      if (session === undefined && hasEntries()) clearThread();
      session = ev.session;
      if (ev.session !== undefined && !threads.some((t) => t.id === ev.session)) {
        threads.unshift({ id: ev.session, title: ev.prompt, user: ev.user, status: "running", choice: ev.choice ?? choice!, created: Date.now(), updated: Date.now(), messages: 1 });
        renderThreads();
      }
      msg("prompt", ev.prompt);
      if (ev.choice) statusEl.textContent = `${ev.choice.provider} · ${ev.choice.model} · ${ev.choice.effort}`;
      finishRun("done");
      run = startRun();
      break;
    case "phase":
      if (run) run.phase.textContent = ev.text;
      else if (running) statusEl.textContent = ev.text;
      break;
    case "turn":
      msg("turn", ev.text, ev.role);
      break;
    case "tool":
      toolRow(ev);
      break;
    case "draft":
      if (ev.session === session) showProposal(ev.files, true);
      break;
    case "proposal":
      if (ev.session !== session) break;
      setRunning(false);
      finishRun("done");
      if (ev.text) msg("turn", ev.text);
      statusEl.textContent = `${ev.model} · ${ev.usage.input}→${ev.usage.output} tokens`;
      showProposal(ev.files);
      markThread("idle");
      break;
    case "error":
      setRunning(false);
      finishRun("failed");
      msg("error", ev.message);
      break;
    case "cancelled":
      setRunning(false);
      finishRun("cancelled");
      msg("note", "cancelled");
      break;
    case "applied":
    case "discarded":
      if (ev.session !== session) break;
      msg("note", ev.event === "applied" ? `applied: ${ev.files.join(", ") || "nothing changed"}` : "discarded");
      clearProposal();
      markThread(ev.event);
      session = undefined;
      renderThreads();
      if (ev.event === "applied") { queue = []; renderQueue(); }
      break;
  }
}

// ---- the proposal ------------------------------------------------------------

function clearProposal(): void {
  for (const d of diffEditors) {
    const m = d.editor.getModel();
    d.editor.dispose();
    m?.original.dispose();
    m?.modified.dispose();
  }
  diffEditors = [];
  proposalEl?.remove();
  proposalEl = undefined;
  barEl.replaceChildren();
  barEl.hidden = true;
}

function apply(files: ApplyFile[]): void {
  if (session === undefined || running) return;
  host.send({ type: "ai", action: "apply", session, files });
}

function discard(): void {
  if (session === undefined || running) return;
  host.send({ type: "ai", action: "discard", session });
}

function button(text: string, cls: string, onClick: () => void): HTMLButtonElement {
  const b = document.createElement("button");
  b.className = cls;
  b.textContent = text;
  b.addEventListener("click", onClick);
  return b;
}

function showProposal(files: ProposedFile[], draft = false): void {
  clearProposal();
  const wrap = document.createElement("div");
  wrap.className = `ai-proposal${draft ? " draft" : ""}`;
  proposalEl = wrap;
  if (draft) {
    const d = document.createElement("div");
    d.className = "ai-msg note";
    d.textContent = "edits so far — still working";
    wrap.appendChild(d);
  }
  if (files.length === 0) {
    const p = document.createElement("div");
    p.className = "ai-msg note";
    p.textContent = "No file changes proposed. Ask a follow-up, or discard.";
    wrap.appendChild(p);
  }
  for (const f of files) {
    const card = document.createElement("div");
    card.className = "ai-file";
    const head = document.createElement("div");
    head.className = "head";
    const name = document.createElement("code");
    name.className = "name";
    name.textContent = f.path;
    head.appendChild(name);
    if (f.deleted || f.before === "") {
      const badge = document.createElement("span");
      badge.className = f.deleted ? "badge deleted" : "badge";
      badge.textContent = f.deleted ? "deleted" : "new";
      head.appendChild(badge);
    }
    const spacer = document.createElement("span");
    spacer.className = "spacer";
    head.appendChild(spacer);
    const current = host.currentText(f.path);
    const stale = current !== undefined && current !== f.before;
    card.appendChild(head);
    if (stale) {
      const warn = document.createElement("div");
      warn.className = "warn";
      warn.textContent = "changed since the AI read it — check the diff";
      card.appendChild(warn);
    }
    const box = document.createElement("div");
    box.className = "diff";
    card.appendChild(box);
    wrap.appendChild(card);
    const original = monaco.editor.createModel(f.before, "latex");
    const modified = monaco.editor.createModel(f.after, "latex");
    const editor = monaco.editor.createDiffEditor(box, {
      renderSideBySide: false, automaticLayout: true, originalEditable: false, readOnly: false,
      minimap: { enabled: false }, lineNumbers: "off", fontSize: 12, scrollBeyondLastLine: false,
      renderOverviewRuler: false, folding: false, wordWrap: "on", theme: "solarized-light",
      diffWordWrap: "on", hideUnchangedRegions: { enabled: true, contextLineCount: 3 },
    });
    editor.setModel({ original, modified });
    const entry = { path: f.path, deleted: !!f.deleted, editor };
    diffEditors.push(entry);
    // Size the box to its content, up to ~45vh; the thread scrolls past it.
    const fit = () => {
      const h = editor.getModifiedEditor().getContentHeight() + 6;
      box.style.height = `${Math.min(Math.max(h, 48), window.innerHeight * 0.45)}px`;
    };
    editor.getModifiedEditor().onDidContentSizeChange(fit);
    fit();
    head.appendChild(button(f.deleted ? "delete this file" : "apply this file", "small", () => apply([picked(entry)])));
  }
  append(wrap);

  // The sticky bar below the thread.
  barEl.hidden = false;
  if (files.length) {
    const hint = document.createElement("span");
    hint.className = "muted";
    hint.textContent = "edit the diff, or ask a follow-up below";
    barEl.append(
      button("apply all", "", () => apply(diffEditors.map(picked))),
      button("discard", "small", discard),
      hint,
    );
  } else {
    barEl.append(button("close", "small", discard));
  }
}
