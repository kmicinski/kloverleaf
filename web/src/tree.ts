// The history as a tree: a subway map of branches. Each branch is a lane;
// along it, dots for what happened (typing runs, AI applies, restores,
// pushes, commits); a curve where a branch forks off its parent; a ring on
// the branch you are on. Hover tells, click previews (time travel), the lane
// name switches, double-click forks. Data: /api/projects/<p>/history/tree.

import { when } from "./protocol.js";

export interface TreeBranch { name: string; parent: string | null; from_entry: number | null; created: number; created_by: string; note: string; head: number | null; head_ts: number | null; entries: number }
export interface TreeEvent { id: number; first: number; count: number; ts: number; branch: string; file: string; author: string; kind: string; label: string }
export interface TreeData { current: string; branches: TreeBranch[]; events: TreeEvent[] }
export interface TreeHost {
  preview(id: number): void;
  switchTo(name: string): void;
  branchAt(id: number): void;
}

const STEP = 16;        // px per event along a lane
const LANE = 24;        // px between lanes
const LEFT = 110;       // room for lane names
const SVG = "http://www.w3.org/2000/svg";

function el<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number>): SVGElementTagNameMap[K] {
  const e = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
  return e;
}

/** Draws the map into `container`; `selected` is the entry being previewed, if any. */
export function renderTree(container: HTMLElement, data: TreeData, host: TreeHost, selected?: number): void {
  const events = [...data.events].sort((a, b) => a.id - b.id);
  const lanes = new Map<string, number>();
  data.branches.forEach((b, i) => lanes.set(b.name, i));
  const xOf = new Map<number, number>();
  events.forEach((e, i) => xOf.set(e.id, LEFT + 12 + i * STEP));
  // A branch's fork point sits at the last event at or before its from_entry.
  const xAt = (id: number): number => {
    let best = LEFT + 12;
    for (const e of events) { if (e.id <= id) best = xOf.get(e.id)!; else break; }
    return best;
  };
  const width = LEFT + 24 + Math.max(1, events.length) * STEP;
  const height = 14 + Math.max(1, data.branches.length) * LANE;
  const svg = el("svg", { width, height, class: "tree" });
  const y = (name: string) => 14 + (lanes.get(name) ?? 0) * LANE;

  for (const b of data.branches) {
    const yb = y(b.name);
    const own = events.filter((e) => e.branch === b.name);
    const x0 = b.from_entry !== null ? xAt(b.from_entry) : LEFT + 12;
    const x1 = own.length ? xOf.get(own[own.length - 1].id)! : x0 + STEP;
    if (b.parent && b.from_entry !== null) {
      svg.appendChild(el("path", { d: `M ${x0} ${y(b.parent)} C ${x0 + STEP} ${y(b.parent)}, ${x0} ${yb}, ${x0 + STEP} ${yb}`, class: "fork" }));
    }
    svg.appendChild(el("line", { x1: b.parent ? x0 + STEP : x0, y1: yb, x2: Math.max(x1, x0 + STEP), y2: yb, class: `lane${b.name === data.current ? " current" : ""}` }));
    const label = el("text", { x: 6, y: yb + 4, class: `lane-name${b.name === data.current ? " current" : ""}` });
    label.textContent = b.name.length > 14 ? b.name.slice(0, 13) + "…" : b.name;
    const t = el("title", {});
    t.textContent = `${b.name}${b.parent ? ` — forked from ${b.parent}` : ""}${b.note ? ` · ${b.note}` : ""} · ${b.entries} entries${b.name === data.current ? " · you are here" : " · click to switch"}`;
    label.appendChild(t);
    if (b.name !== data.current) label.addEventListener("click", () => host.switchTo(b.name));
    svg.appendChild(label);
  }
  for (const e of events) {
    const cx = xOf.get(e.id)!, cy = y(e.branch);
    const big = e.kind !== "delta";
    const node = el("circle", { cx, cy, r: big ? 4.5 : 2.5, class: `node ${e.kind}${selected !== undefined && e.first <= selected && selected <= e.id ? " selected" : ""}` });
    const t = el("title", {});
    t.textContent = `${when(e.ts)} · ${e.author} · ${e.label}${e.file ? ` · ${e.file}` : ""} · click to look, double-click to branch here`;
    node.appendChild(t);
    node.addEventListener("click", () => host.preview(e.id));
    node.addEventListener("dblclick", (ev) => { ev.preventDefault(); host.branchAt(e.id); });
    svg.appendChild(node);
  }
  const cur = events.filter((e) => e.branch === data.current);
  if (cur.length) {
    const last = cur[cur.length - 1];
    svg.appendChild(el("circle", { cx: xOf.get(last.id)!, cy: y(last.branch), r: 8, class: "here" }));
  }
  container.replaceChildren(svg);
  container.scrollLeft = container.scrollWidth;
}
