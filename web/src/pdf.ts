// The PDF pane: PDF.js rendering every page into its own canvas, lazily, at
// device resolution. A new version of the PDF is rendered into fresh
// canvases that are swapped in one at a time, so the page you are reading
// never blanks between compiles and the scroll position is kept. Clicks
// report PDF coordinates (top-left origin, points) for SyncTeX; forward sync
// draws the boxes SyncTeX returns.

import { appFetch } from "./protocol.js";
const PDFJS = "https://cdnjs.cloudflare.com/ajax/libs/pdf.js/4.10.38/pdf.min.mjs";
const WORKER = "https://cdnjs.cloudflare.com/ajax/libs/pdf.js/4.10.38/pdf.worker.min.mjs";

interface PdfPageProxy {
  getViewport(o: { scale: number }): { width: number; height: number };
  render(o: { canvasContext: CanvasRenderingContext2D; viewport: unknown }): { promise: Promise<void>; cancel(): void };
}
interface PdfDocumentProxy { numPages: number; getPage(n: number): Promise<PdfPageProxy>; destroy(): Promise<void> }
interface PdfJs {
  GlobalWorkerOptions: { workerSrc: string };
  getDocument(o: { data: Uint8Array }): { promise: Promise<PdfDocumentProxy> };
}

export interface SyncBox { page: number; h: number; v: number; width: number; height: number }

interface PageSlot { el: HTMLElement; canvas: HTMLCanvasElement | null; rendering: number; width: number; height: number }

export class PdfViewer {
  private lib: PdfJs | null = null;
  private doc: PdfDocumentProxy | null = null;
  private pages: PageSlot[] = [];
  private scale = 1;
  private fit = true;
  private fitWidth = 0;
  private generation = 0;
  private observer: IntersectionObserver;
  private visible = new Set<number>();
  private readonly wrap: HTMLElement;
  private pins: { id: number; page: number; x: number; y: number; color: string; title: string }[] = [];
  /** A comment pin was clicked (see chat.ts). */
  onPin: (id: number) => void = () => {};

  constructor(
    private readonly scroller: HTMLElement,
    private readonly onClick: (page: number, x: number, y: number, jump: boolean) => void,
    private readonly onPages: (n: number, zoom: number) => void,
  ) {
    this.wrap = scroller.querySelector("#pdf-pages-wrap")!;
    this.observer = new IntersectionObserver((entries) => {
      for (const e of entries) {
        const i = Number((e.target as HTMLElement).dataset.page) - 1;
        if (e.isIntersecting) { this.visible.add(i); void this.render(i); } else this.visible.delete(i);
      }
    }, { root: scroller, rootMargin: "600px 0px" });
    // Coalesce resize bursts, and only refit when the width really moved: a
    // refit that changes the page height can toggle the scrollbar, which
    // changes the width, which refits… (the scroller also forces its scrollbar
    // on in CSS so this cannot oscillate).
    let timer: number | undefined;
    new ResizeObserver(() => {
      if (!this.fit || !this.doc) return;
      window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        const w = this.scroller.clientWidth;
        if (Math.abs(w - this.fitWidth) >= 2) void this.relayout();
      }, 50);
    }).observe(scroller);
  }

  /** Loads a (new version of the) PDF; pages re-render in place. */
  async load(url: string): Promise<void> {
    if (!this.lib) {
      this.lib = (await import(/* @vite-ignore */ PDFJS)) as PdfJs;
      this.lib.GlobalWorkerOptions.workerSrc = WORKER;
    }
    const gen = ++this.generation;
    // Fetch ourselves so a server's explanation (e.g. "that version does not compile") reaches the status line.
    const r = await appFetch(url);
    if (!r.ok) throw new Error(await r.text());
    const data = new Uint8Array(await r.arrayBuffer());
    if (gen !== this.generation) return;
    const doc = await this.lib.getDocument({ data }).promise;
    if (gen !== this.generation) { void doc.destroy(); return; }
    const old = this.doc;
    this.doc = doc;
    if (old) void old.destroy();
    await this.relayout();
  }

  private async relayout(): Promise<void> {
    const doc = this.doc!;
    const first = await doc.getPage(1);
    const base = first.getViewport({ scale: 1 });
    if (this.fit) {
      this.fitWidth = this.scroller.clientWidth;
      this.scale = Math.max(0.2, (this.fitWidth - 36) / base.width);
    }
    // Keep page elements that still exist so the scroll position survives.
    while (this.pages.length > doc.numPages) {
      const slot = this.pages.pop()!;
      this.observer.unobserve(slot.el);
      slot.el.remove();
    }
    for (let i = 0; i < doc.numPages; i++) {
      const page = await doc.getPage(i + 1);
      const vp = page.getViewport({ scale: this.scale });
      let slot = this.pages[i];
      if (!slot) {
        const el = document.createElement("div");
        el.className = "pdf-page";
        el.dataset.page = String(i + 1);
        el.addEventListener("click", (e) => this.click(e, i, false));
        el.addEventListener("dblclick", (e) => this.click(e, i, true));
        this.wrap.appendChild(el);
        slot = { el, canvas: null, rendering: 0, width: 0, height: 0 };
        this.pages.push(slot);
        this.observer.observe(el);
      }
      slot.width = vp.width;
      slot.height = vp.height;
      slot.el.style.width = `${vp.width}px`;
      slot.el.style.height = `${vp.height}px`;
    }
    this.onPages(doc.numPages, this.scale);
    this.drawPins();
    for (const i of this.visible) void this.render(i);
  }

  /** Renders page `i` into a fresh canvas, then swaps it in. */
  private async render(i: number): Promise<void> {
    const doc = this.doc;
    const slot = this.pages[i];
    if (!doc || !slot) return;
    const token = ++slot.rendering;
    const gen = this.generation;
    const page = await doc.getPage(i + 1);
    const dpr = window.devicePixelRatio || 1;
    const vp = page.getViewport({ scale: this.scale * dpr });
    const canvas = document.createElement("canvas");
    canvas.width = Math.floor(vp.width);
    canvas.height = Math.floor(vp.height);
    canvas.style.width = `${slot.width}px`;
    canvas.style.height = `${slot.height}px`;
    const ctx = canvas.getContext("2d")!;
    try {
      await page.render({ canvasContext: ctx, viewport: vp }).promise;
    } catch {
      return;
    }
    if (token !== slot.rendering || gen !== this.generation) return;
    const old = slot.canvas;
    slot.el.insertBefore(canvas, slot.el.firstChild);
    old?.remove();
    slot.canvas = canvas;
  }

  private click(e: MouseEvent, i: number, jump: boolean): void {
    const t = e.target as HTMLElement;
    if (t.classList.contains("sync-box") || t.closest(".chat-pin")) return;
    const r = this.pages[i].el.getBoundingClientRect();
    const x = (e.clientX - r.left) / this.scale;
    const y = (e.clientY - r.top) / this.scale;
    this.onClick(i + 1, x, y, jump);
  }

  zoom(factor: number | "fit"): void {
    if (factor === "fit") this.fit = true;
    else { this.fit = false; this.scale = Math.min(6, Math.max(0.2, this.scale * factor)); }
    if (this.doc) void this.relayout();
  }

  /** Comment pins (PDF points, top-left origin): drawn now and after every relayout. */
  setPins(pins: { id: number; page: number; x: number; y: number; color: string; title: string }[]): void {
    this.pins = pins;
    this.drawPins();
  }

  private drawPins(): void {
    for (const el of this.wrap.querySelectorAll(".chat-pin, .chat-tick")) el.remove();
    // Margin pins on nearby lines stack downwards instead of overlapping.
    const bottom = new Map<number, number>();
    for (const p of [...this.pins].sort((a, b) => a.page - b.page || a.y - b.y)) {
      const slot = this.pages[p.page - 1];
      if (!slot) continue;
      // In the page's right margin at the comment's height (the text stays
      // readable), with a short tick at the exact spot.
      const top = Math.max(p.y * this.scale, (bottom.get(p.page) ?? -Infinity) + 24);
      bottom.set(p.page, top);
      const el = document.createElement("button");
      el.className = "chat-pin";
      el.style.left = `${slot.width - 30}px`;
      el.style.top = `${top}px`;
      const tick = document.createElement("div");
      tick.className = "chat-tick";
      tick.style.left = `${p.x * this.scale - 2}px`;
      tick.style.top = `${p.y * this.scale}px`;
      tick.style.setProperty("--who", p.color);
      slot.el.appendChild(tick);
      el.style.setProperty("--who", p.color);
      el.title = p.title;
      el.textContent = "💬";
      el.addEventListener("click", (e) => { e.stopPropagation(); this.onPin(p.id); });
      slot.el.appendChild(el);
    }
  }

  /** Draws SyncTeX boxes and scrolls the first into view. */
  highlight(boxes: SyncBox[]): void {
    for (const el of this.wrap.querySelectorAll(".sync-box")) el.remove();
    let first: HTMLElement | null = null;
    for (const b of boxes) {
      const slot = this.pages[b.page - 1];
      if (!slot) continue;
      const el = document.createElement("div");
      el.className = "sync-box";
      el.style.left = `${b.h * this.scale}px`;
      el.style.top = `${(b.v - b.height) * this.scale - 2}px`;
      el.style.width = `${Math.max(4, b.width * this.scale)}px`;
      el.style.height = `${Math.max(4, b.height * this.scale) + 4}px`;
      slot.el.appendChild(el);
      first ??= el;
    }
    if (first) {
      first.scrollIntoView({ block: "center", behavior: "smooth" });
      setTimeout(() => { for (const el of this.wrap.querySelectorAll(".sync-box")) (el as HTMLElement).style.opacity = "0"; }, 2400);
      setTimeout(() => { for (const el of this.wrap.querySelectorAll(".sync-box")) el.remove(); }, 3200);
    }
  }
}
