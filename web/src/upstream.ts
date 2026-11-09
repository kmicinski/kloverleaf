// The tracked upstream repository (src/upstream.rs): the header chip, the
// sync panel (push with a message, pull, unpushed files, incoming commits,
// where collaborators are working), the conflict resolver, and gutter marks
// on lines a collaborator changed recently.
//
// ⌘S pushes (the editor calls `push()`); the server pulls by itself every
// minute while nobody types and pushes leftover work once the last editor has
// gone, so the chip is mostly a status light.

import { api, postJson, when } from "./protocol.js";

export interface UpChange { path: string; status: "added" | "modified" | "deleted" }
export interface UpCommit { sha: string; author: string; email: string; ts: number; message: string; files: string[] }
export type UpPiece = { kind: "same"; text: string } | { kind: "conflict"; id: string; base: string; ours: string; theirs: string };
export interface UpConflict { path: string; whole: boolean; id: string; ours_exists: boolean; theirs_exists: boolean; pieces: UpPiece[] }
export interface UpStatus {
  tracked: boolean; label: string; url: string; web: string | null; branch: string; branches: string[]; subdir: string;
  local_branch: string; on_branch: boolean; base: string; head: string;
  behind: UpCommit[]; ahead: UpChange[]; conflicts: UpConflict[]; push_on_leave: boolean;
  last_fetch: number | null; error: string | null; busy: string | null;
  last_push: { at: number; sha: string; user: string; message: string; files: UpChange[]; leaving: boolean } | null;
  last_pull: { at: number; sha: string; authors: string[]; commits: number; files: UpChange[] } | null;
}
export type UpResult = { result: "pushed" | "pulled" | "up-to-date" | "conflicts"; conflicts?: UpConflict[]; pushed?: { sha: string; message: string; files: UpChange[] }; pulled?: { authors: string[]; commits: number; files: UpChange[] } | null };
export type UpEvent = { event: "busy"; busy: string } | ({ event: "status"; last?: UpResult } & UpStatus);
interface Zone { file: string; from: number; to: number; author: string; ts: number; sha: string; summary: string; section: string | null }
interface Spot { file: string; section: string | null; author: string; ts: number; from: number; to: number; lines: number; summary: string }
type Pick = { pick: "ours" | "theirs" | "both" | "theirs-first" } | { pick: "text"; text: string };

interface Host {
  editor: () => monaco.editor.IStandaloneCodeEditor;
  current: () => string | null;
  open: (file: string, line?: number) => void;
  status: (s: string) => void;
}

let host: Host;
let tracked = false;
let st: UpStatus | null = null;
let busy: string | null = null;
let localEdits = false;              // typed since the last status: the chip shows unpushed work at once
let panel: HTMLElement | null = null;
let mergeView: HTMLElement | null = null;
const chip = document.getElementById("upstream-chip") as HTMLButtonElement;
const SHOW_ZONES = "kloverleaf.upstream.zones";
let showZones = (() => { try { return localStorage.getItem(SHOW_ZONES) !== "0"; } catch { return true; } })();
let zones: Zone[] = [];
let zoneFile: string | null = null;
let zoneDecorations: monaco.editor.IEditorDecorationsCollection | null = null;
let tip: HTMLElement | null = null;
// Decisions survive a re-merge: the server settles every id it still finds, so send them all each time.
const picks = new Map<string, Pick>();

const esc = (s: string) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);
const ago = (ts: number) => {
  const s = Math.max(0, (Date.now() - ts) / 1000);
  if (s < 90) return "just now";
  if (s < 5400) return `${Math.round(s / 60)} min ago`;
  if (s < 129600) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} d ago`;
};
const commitUrl = (sha: string) => (st?.web ? `${st.web}/commit/${sha}` : null);
const theirName = () => {
  const names = [...new Set((st?.behind ?? []).map((c) => c.author))];
  return names.length ? names.join(", ") : "theirs";
};

export function initUpstream(h: Host, isTracked: boolean): void {
  if (host) return; // a reconnect re-sends init
  host = h;
  tracked = isTracked;
  chip.hidden = !tracked;
  if (!tracked) return;
  chip.addEventListener("click", () => (panel ? closePanel() : void openPanel()));
  void refresh();
  const ed = h.editor();
  ed.onMouseMove((e) => {
    const t = e.target;
    const gutter = t.type === monaco.editor.MouseTargetType.GUTTER_LINE_DECORATIONS || t.type === monaco.editor.MouseTargetType.GUTTER_LINE_NUMBERS;
    const line = t.position?.lineNumber;
    const z = gutter && line ? zones.find((z) => z.from <= line && line <= z.to) : undefined;
    if (!z) { hideTip(); return; }
    showTip(z, e.event.browserEvent.clientX, e.event.browserEvent.clientY);
  });
  ed.onMouseLeave(() => hideTip());
}

export function isTracked(): boolean { return tracked; }

/** On the page's own edits: until the next status, there is unpushed work. */
export function localEdit(): void {
  if (!tracked || localEdits) return;
  localEdits = true;
  renderChip();
}

async function refresh(): Promise<void> {
  const s = await api<UpStatus>("/upstream").catch(() => null);
  if (s?.tracked) { setStatus(s); }
}

function setStatus(s: UpStatus): void {
  const headMoved = st?.head !== s.head;
  st = s;
  busy = s.busy;
  localEdits = false;
  renderChip();
  if (panel) renderPanel();
  if (headMoved) { zoneFile = null; void fileShown(host.current()); }
}

export function event(m: UpEvent): void {
  if (!tracked) return;
  if (m.event === "busy") { busy = m.busy; renderChip(); return; }
  setStatus(m);
  const last = m.last;
  if (last?.pulled && last.result !== "conflicts") {
    const p = last.pulled;
    host.status(`merged ${p.commits} commit${p.commits === 1 ? "" : "s"} from ${p.authors.join(", ") || m.label} into ${p.files.map((f) => f.path.split("/").pop()).join(", ") || "nothing"}`);
  }
  if (last?.result === "conflicts" && !mergeView && m.conflicts.length) {
    host.status(`${m.conflicts.length} file${m.conflicts.length === 1 ? "" : "s"} conflict with ${theirName()} — open the ⇅ chip to resolve`);
  }
}

function renderChip(): void {
  if (!st) { chip.textContent = "⇅ …"; return; }
  chip.classList.remove("ok", "ahead", "warn", "busy");
  let text: string;
  let cls: string;
  if (busy) { text = `⇅ ${busy}…`; cls = "busy"; }
  else if (st.error) { text = "⇅ ⚠ error"; cls = "warn"; }
  else if (!st.on_branch) { text = `⇅ off (on another branch)`; cls = "warn"; }
  else if (st.conflicts.length) { text = `⇅ ⚠ ${st.conflicts.length} conflict${st.conflicts.length === 1 ? "" : "s"}`; cls = "warn"; }
  else {
    const parts: string[] = [];
    if (st.behind.length) parts.push(`↓${st.behind.length}`);
    if (st.ahead.length) parts.push(`↑${st.ahead.length}`);
    else if (localEdits) parts.push("↑");
    text = `⇅ ${parts.length ? parts.join(" ") : "✓"}`;
    cls = parts.length ? "ahead" : "ok";
  }
  chip.textContent = text;
  chip.classList.add(cls);
  chip.title = `${st.label} @ ${st.branch} — ${st.ahead.length ? `${st.ahead.length} file(s) not pushed yet (⌘S pushes)` : "nothing unpushed"}${st.behind.length ? ` · ${st.behind.length} incoming commit(s)` : ""}${st.error ? `\n${st.error}` : ""}`;
}

// ---- push / pull ---------------------------------------------------------------------

/** ⌘S: push everything since the last push as one commit (pulling first). */
export async function push(message?: string): Promise<void> {
  if (!tracked) return;
  host.status(`pushing to ${st?.label ?? "upstream"}…`);
  const r = await postJson<UpResult>("/upstream/push", { message: message ?? null }).catch((e) => { host.status(`push failed: ${String(e).slice(0, 160)}`); return null; });
  if (r) report(r);
}

async function pull(): Promise<void> {
  host.status(`checking ${st?.label ?? "upstream"}…`);
  const r = await api<UpResult>("/upstream/pull", { method: "POST" }).catch((e) => { host.status(`pull failed: ${String(e).slice(0, 160)}`); return null; });
  if (r) report(r);
}

/** Follow another upstream branch: its directory merged in, unpushed work kept. */
async function track(branch: string): Promise<void> {
  if (!st || branch === st.branch) return;
  const unpushed = st.ahead.length ? `\n\nYour ${st.ahead.length} unpushed file(s) are kept and will be pushed to ${branch} next time.` : "";
  if (!window.confirm(`Track ${branch} instead of ${st.branch}?\n\nThe project becomes ${branch}'s ${st.subdir || "files"}; pushes go to ${branch} from now on.${unpushed}`)) { renderPanel(); return; }
  host.status(`switching to ${branch}…`);
  const r = await postJson<UpResult>("/upstream/branch", { branch }).catch((e) => { host.status(`switch failed: ${String(e).slice(0, 160)}`); return null; });
  if (r) { report(r); if (r.result !== "conflicts") host.status(`now tracking ${branch}`); }
  void loadSpots();
}

function report(r: UpResult): void {
  switch (r.result) {
    case "pushed": host.status(`pushed ${r.pushed!.sha.slice(0, 7)} · ${r.pushed!.message.split("\n")[0]}`); break;
    case "pulled": host.status(`merged ${r.pulled?.commits ?? 0} commit(s) from ${r.pulled?.authors.join(", ") || "upstream"}${r.pushed ? "" : " — nothing of yours to push"}`); break;
    case "up-to-date": host.status("up to date — nothing to push"); break;
    case "conflicts": host.status(`conflicts with ${theirName()} — resolve them to finish`); void openMerge(r.conflicts ?? []); break;
  }
}

// ---- the panel ----------------------------------------------------------------------

async function openPanel(): Promise<void> {
  panel = document.createElement("div");
  panel.id = "upstream-panel";
  const rect = chip.getBoundingClientRect();
  panel.style.top = `${rect.bottom + 4}px`;
  panel.style.left = `${Math.max(8, Math.min(rect.left, window.innerWidth - 480))}px`;
  document.body.appendChild(panel);
  renderPanel();
  setTimeout(() => document.addEventListener("mousedown", outside), 0);
  await refresh();
  void loadSpots();
}

function outside(e: MouseEvent): void {
  if (panel && !panel.contains(e.target as Node) && e.target !== chip) closePanel();
}

function closePanel(): void {
  panel?.remove();
  panel = null;
  document.removeEventListener("mousedown", outside);
}

let spots: Spot[] | null = null;
async function loadSpots(): Promise<void> {
  const r = await api<{ spots: Spot[] }>("/upstream/activity").catch(() => null);
  spots = r?.spots ?? [];
  if (panel) renderPanel();
}

function renderPanel(): void {
  if (!panel || !st) return;
  const s = st;
  const where = s.web ? `<a href="${esc(s.web)}/tree/${esc(s.branch)}/${esc(s.subdir)}" target="_blank">${esc(s.label)}</a>` : esc(s.label);
  const message = (panel.querySelector("#up-message") as HTMLTextAreaElement | null)?.value ?? "";
  const files = (list: UpChange[]) => list.map((c) => `<li class="ch-${c.status}"><span class="tag">${c.status[0].toUpperCase()}</span> ${esc(c.path)}</li>`).join("");
  const commits = s.behind.map((c) => `<li><a ${commitUrl(c.sha) ? `href="${esc(commitUrl(c.sha)!)}" target="_blank"` : ""}><code>${c.sha.slice(0, 7)}</code></a> <b>${esc(c.author)}</b> <span class="muted">${ago(c.ts)}</span><br>${esc(c.message)} <span class="muted">${esc(c.files.join(", "))}</span></li>`).join("");
  const spotRows = spots === null
    ? `<li class="muted">reading the upstream's recent history…</li>`
    : spots.length === 0
      ? `<li class="muted">no one else has changed this paper in the last 30 days</li>`
      : spots.map((p, i) => `<li class="spot" data-i="${i}" title="${esc(p.summary)}"><span class="age ${age(p.ts)}"></span><b>${esc(p.author)}</b> · ${p.section ? `§ ${esc(p.section)}` : esc(p.file)} <span class="muted">${esc(p.file)}:${p.from}${p.to > p.from ? `–${p.to}` : ""} · ${p.lines} line${p.lines === 1 ? "" : "s"} · ${ago(p.ts)}</span></li>`).join("");
  const lastPush = s.last_push ? `<div class="muted">last push: <a ${commitUrl(s.last_push.sha) ? `href="${esc(commitUrl(s.last_push.sha)!)}" target="_blank"` : ""}><code>${s.last_push.sha.slice(0, 7)}</code></a> by ${esc(s.last_push.user)} ${ago(s.last_push.at)}${s.last_push.leaving ? " (after the editor closed)" : ""}</div>` : "";
  panel.innerHTML = `
    <div class="up-head">⇅ ${where} <span class="muted">@</span><select id="up-branch" title="the upstream branch this project pulls from and pushes to">${(s.branches.includes(s.branch) ? s.branches : [s.branch, ...s.branches]).map((b) => `<option${b === s.branch ? " selected" : ""}>${esc(b)}</option>`).join("")}</select><span class="spacer"></span><button class="tiny" id="up-close">×</button></div>
    <div class="muted">${busy ? `${busy}…` : s.last_fetch ? `checked ${ago(s.last_fetch)}` : "checked at startup"} · pulls by itself while you aren't typing</div>
    ${s.error ? `<div class="err up-error">${esc(s.error)}</div>` : ""}
    ${!s.on_branch ? `<div class="err">Syncing is off: this project is on another kloverleaf branch. Switch back to <b>${esc(s.local_branch)}</b> to sync.</div>` : ""}
    ${s.conflicts.length ? `<div class="up-conflicts">⚠ ${s.conflicts.length} file${s.conflicts.length === 1 ? "" : "s"} changed on both sides in ways that don't merge: <button class="small" id="up-resolve">resolve…</button></div>` : ""}
    <h4>yours, not pushed yet ${s.ahead.length ? `<span class="muted">(${s.ahead.length})</span>` : ""}</h4>
    ${s.ahead.length ? `<ul class="up-files">${files(s.ahead)}</ul>` : `<div class="muted">${localEdits ? "edits since the last check…" : "nothing — the upstream has all your work"}</div>`}
    <textarea id="up-message" rows="2" placeholder="commit message (optional — defaults to “Edit <files>”)">${esc(message)}</textarea>
    <div class="row"><button class="small" id="up-push" ${s.on_branch ? "" : "disabled"}>push now (⌘S)</button><button class="small" id="up-pull" ${s.on_branch ? "" : "disabled"}>pull now</button><span class="spacer"></span><label class="muted"><input type="checkbox" id="up-leave" ${s.push_on_leave ? "checked" : ""}> push my work when I leave</label></div>
    ${lastPush}
    ${s.behind.length ? `<h4>incoming <span class="muted">(${s.behind.length})</span></h4><ul class="up-commits">${commits}</ul>` : ""}
    <h4>where others are working <span class="spacer"></span><label class="muted"><input type="checkbox" id="up-zones" ${showZones ? "checked" : ""}> mark their lines</label></h4>
    <ul class="up-spots">${spotRows}</ul>`;
  panel.querySelector("#up-close")!.addEventListener("click", closePanel);
  panel.querySelector("#up-push")!.addEventListener("click", () => void push((panel?.querySelector("#up-message") as HTMLTextAreaElement).value.trim() || undefined).then(() => { const t = panel?.querySelector("#up-message") as HTMLTextAreaElement | null; if (t) t.value = ""; }));
  panel.querySelector("#up-pull")!.addEventListener("click", () => void pull());
  panel.querySelector("#up-branch")!.addEventListener("change", (e) => void track((e.target as HTMLSelectElement).value));
  panel.querySelector("#up-resolve")?.addEventListener("click", () => { closePanel(); void openMerge(s.conflicts); });
  panel.querySelector("#up-leave")!.addEventListener("change", (e) => void postJson("/upstream/settings", { push_on_leave: (e.target as HTMLInputElement).checked }).then(refresh));
  panel.querySelector("#up-zones")!.addEventListener("change", (e) => {
    showZones = (e.target as HTMLInputElement).checked;
    try { localStorage.setItem(SHOW_ZONES, showZones ? "1" : "0"); } catch { /* private window */ }
    renderZones();
  });
  panel.querySelectorAll<HTMLElement>(".spot").forEach((li) => li.addEventListener("click", () => {
    const p = spots![Number(li.dataset.i)];
    closePanel();
    host.open(p.file, p.from);
  }));
}

function age(ts: number): string {
  const d = (Date.now() - ts) / 86_400_000;
  return d < 2 ? "hot" : d < 8 ? "warm" : "cool";
}

// ---- gutter marks: lines a collaborator changed recently ----------------------------

/** The editor switched to `file`: fetch and mark its collaborator lines. */
export async function fileShown(file: string | null): Promise<void> {
  if (!tracked || !file || file === zoneFile) { renderZones(); return; }
  zoneFile = file;
  const r = await api<{ zones: Zone[] }>(`/upstream/activity?file=${encodeURIComponent(file)}`).catch(() => null);
  if (zoneFile !== file) return;
  zones = r?.zones ?? [];
  renderZones();
}

function renderZones(): void {
  const ed = host.editor();
  zoneDecorations ??= ed.createDecorationsCollection();
  if (!showZones || zoneFile !== host.current()) { zoneDecorations.clear(); return; }
  zoneDecorations.set(zones.map((z) => ({
    range: new monaco.Range(z.from, 1, z.to, 1),
    options: {
      isWholeLine: true,
      linesDecorationsClassName: `up-zone up-${age(z.ts)}`,
      overviewRuler: { color: age(z.ts) === "hot" ? "#cb4b16cc" : "#cb4b1666", position: monaco.editor.OverviewRulerLane.Left },
      stickiness: monaco.editor.TrackedRangeStickiness.NeverGrowsWhenTypingAtEdges,
    },
  })));
}

function showTip(z: Zone, x: number, y: number): void {
  tip ??= Object.assign(document.createElement("div"), { id: "up-tip" });
  if (!tip.isConnected) document.body.appendChild(tip);
  tip.innerHTML = `<b>${esc(z.author)}</b> · ${ago(z.ts)} <span class="muted">${when(z.ts)}</span><br>${esc(z.summary)} <code class="muted">${esc(z.sha.slice(0, 7))}</code>${z.section ? `<br><span class="muted">§ ${esc(z.section)}, lines ${z.from}–${z.to}</span>` : ""}`;
  tip.style.left = `${x + 14}px`;
  tip.style.top = `${y + 10}px`;
}

function hideTip(): void { tip?.remove(); }

// ---- the conflict resolver -----------------------------------------------------------

async function openMerge(conflicts: UpConflict[]): Promise<void> {
  mergeView?.remove();
  if (!conflicts.length) return;
  const view = document.createElement("div");
  view.id = "merge-view";
  mergeView = view;
  const them = theirName();
  const ids = conflicts.flatMap((c) => (c.whole ? [c.id] : c.pieces.flatMap((p) => (p.kind === "conflict" ? [p.id] : []))));
  const total = ids.length;
  const context = (text: string, where: "before" | "after") => {
    const lines = text.replace(/\n$/, "").split("\n");
    const keep = where === "before" ? lines.slice(-3) : lines.slice(0, 3);
    const cut = lines.length > 3;
    return `<pre class="ctx">${where === "before" && cut ? "…\n" : ""}${esc(keep.join("\n"))}${where === "after" && cut ? "\n…" : ""}</pre>`;
  };
  let body = "";
  for (const c of conflicts) {
    body += `<section><h3>${esc(c.path)}</h3>`;
    if (c.whole) {
      const mine = c.ours_exists ? "your version" : "deleted (by you)";
      const theirs = c.theirs_exists ? `${esc(them)}'s version` : `deleted (by ${esc(them)})`;
      body += `<div class="conflict" data-id="${c.id}"><p class="muted">Both sides changed this file and it can't be merged line by line.</p>
        <div class="picks"><button class="small" data-pick="ours">keep ${mine}</button><button class="small" data-pick="theirs">take ${theirs}</button></div></div>`;
    } else {
      c.pieces.forEach((p, i) => {
        if (p.kind === "same") {
          if (i > 0) body += context(p.text, "after");
          if (i < c.pieces.length - 1) body += context(p.text, "before");
          return;
        }
        body += `<div class="conflict" data-id="${p.id}">
          ${p.base.trim() ? `<details class="was"><summary class="muted">what it was before either of you changed it</summary><pre>${esc(p.base)}</pre></details>` : ""}
          <div class="sides"><div class="side ours"><div class="who">yours</div><pre>${esc(p.ours) || '<span class="muted">(removed)</span>'}</pre></div>
          <div class="side theirs"><div class="who">${esc(them)}</div><pre>${esc(p.theirs) || '<span class="muted">(removed)</span>'}</pre></div></div>
          <div class="picks"><button class="small" data-pick="ours">keep mine</button><button class="small" data-pick="theirs">take theirs</button><button class="small" data-pick="both">both (mine first)</button><button class="small" data-pick="theirs-first">both (theirs first)</button><button class="small" data-pick="edit">edit…</button></div>
          <textarea class="custom" rows="4" hidden></textarea></div>`;
      });
    }
    body += `</section>`;
  }
  view.innerHTML = `<div class="merge-box">
    <div class="merge-head"><strong>Merge ${esc(st?.label ?? "upstream")}</strong><span class="muted">${total} place${total === 1 ? "" : "s"} where you and ${esc(them)} changed the same text. Everything else merged by itself.</span><span class="spacer"></span><button class="tiny" id="merge-close">×</button></div>
    <div class="merge-body">${body}</div>
    <div class="merge-foot"><span id="merge-left" class="muted"></span><span class="spacer"></span><input id="merge-message" placeholder="commit message (optional)"><button class="small" id="merge-apply" disabled>apply</button><button class="small" id="merge-push" disabled>apply &amp; push</button></div></div>`;
  document.body.appendChild(view);
  const left = view.querySelector("#merge-left")!;
  const applyBtn = view.querySelector("#merge-apply") as HTMLButtonElement;
  const pushBtn = view.querySelector("#merge-push") as HTMLButtonElement;
  const update = () => {
    const n = ids.filter((id) => !picks.has(id)).length;
    left.textContent = n ? `${n} still to decide` : "all decided";
    applyBtn.disabled = pushBtn.disabled = n > 0;
  };
  view.querySelectorAll<HTMLElement>(".conflict").forEach((box) => {
    const id = box.dataset.id!;
    const custom = box.querySelector("textarea.custom") as HTMLTextAreaElement | null;
    box.querySelectorAll<HTMLButtonElement>("[data-pick]").forEach((b) => b.addEventListener("click", () => {
      box.querySelectorAll("[data-pick]").forEach((x) => x.classList.remove("on"));
      b.classList.add("on");
      box.classList.add("decided");
      const pick = b.dataset.pick!;
      if (pick === "edit" && custom) {
        custom.hidden = false;
        if (!custom.value) custom.value = (box.querySelector(".ours pre") as HTMLElement).innerText;
        custom.focus();
        picks.set(id, { pick: "text", text: custom.value });
        custom.oninput = () => picks.set(id, { pick: "text", text: custom.value });
      } else {
        if (custom) custom.hidden = true;
        picks.set(id, { pick } as Pick);
      }
      update();
    }));
  });
  const finish = async (push: boolean) => {
    applyBtn.disabled = pushBtn.disabled = true;
    left.textContent = push ? "merging and pushing…" : "merging…";
    const message = (view.querySelector("#merge-message") as HTMLInputElement).value.trim() || `Merge ${them}'s changes`;
    const r = await postJson<UpResult>("/upstream/resolve", { picks: Object.fromEntries(picks), push, message }).catch((e) => { left.textContent = String(e).slice(0, 200); return null; });
    if (!r) { update(); return; }
    if (r.result === "conflicts") {
      // The text moved (yours or theirs) — what's left is shown again; decisions that still apply are kept.
      host.status("the text changed while you were deciding — here is what is left");
      void openMerge(r.conflicts ?? []);
      return;
    }
    picks.clear();
    closeMerge();
    report(r);
  };
  applyBtn.addEventListener("click", () => void finish(false));
  pushBtn.addEventListener("click", () => void finish(true));
  view.querySelector("#merge-close")!.addEventListener("click", closeMerge);
  update();
}

function closeMerge(): void {
  mergeView?.remove();
  mergeView = null;
}
