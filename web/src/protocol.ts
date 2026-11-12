// Wire types shared by the editor, the PDF viewer and the AI panel. The
// server side and the protocol's description are in src/live.rs and src/ai/mod.rs.

import type { UpEvent } from "./upstream.js";

export interface Edit { offset: number; length: number; text: string }
export interface FileInfo { path: string; size: number; text: boolean }
/** A compile problem. `column`/`end_column` (UTF-16, 1-based) mark the culprit when the log says where it is;
 *  `hint` is what it usually means; `context` is TeX's own excerpt of the source around where it stopped. */
export interface Diagnostic {
  file: string; line: number | null; severity: "error" | "warning" | "info"; message: string;
  column?: number; end_column?: number; hint?: string; context?: string;
}
/** A page with the project open: its connection, who, and the file it shows. */
export interface Here { id: number; user: string; file: string | null; line?: number | null; column?: number | null }
/** A point in the PDF (points, top-left origin). */
export interface SyncSpot { page: number; x: number; y: number }
/** Where a comment is pinned: a line (and its text then, to find it again), maybe the PDF click it came from. */
export interface ChatAnchor { file: string; line: number; quote: string; pdf?: SyncSpot }
export interface ChatMsg { id: number; user: string; ts: number; text: string; anchor?: ChatAnchor; resolved: boolean; resolved_by?: string; deleted?: boolean }
/** `stale`: this compile stopped before writing a PDF; the one shown is an earlier build's. */
export interface Build { version: number; ok: boolean; diags: Diagnostic[]; ms: number; pdf: boolean; hasPdf: boolean; stale?: boolean }
export interface Origin { id: number; kind: string; user: string; ts: number; meta: Record<string, unknown> }
/** A file's provenance: run-length spans `[len, origin]` plus the origins they name. */
export interface ProvPayload { spans: [number, number][]; origins: Record<string, Origin> }
export interface AiChoice { provider: "claude" | "codex"; model: string; effort: string }
export interface AiProvider { id: "claude" | "codex"; label: string; models: string[]; efforts: string[] }
export interface AiMode { mode: "off" | "on"; reason?: string; default?: AiChoice; providers?: AiProvider[]; unavailable?: Record<string, string> }

export type ServerMsg =
  | { type: "init"; user: string; files: FileInfo[]; main: string | null; here: Here[]; conn: number; chat: ChatMsg[]; build_id?: string; build: Build | null; building: boolean; ai: AiMode; origin: number; branch: string; upstream: boolean }
  | { type: "text"; file: string; text: string; rev: number; prov?: ProvPayload | null }
  /** Revision `rev` of a file. `prov`: the typist's origin id, or the whole map after a replacement. */
  | { type: "edit"; file: string; rev: number; op: (number | string)[]; changes: Edit[]; user: string; prov: number | ProvPayload; client: string | null; seq: number }
  | { type: "ack"; file: string; rev: number; seq: number }
  | { type: "caught"; file: string; rev: number }
  | { type: "here"; here: Here[] }
  | { type: "chat"; msg: ChatMsg }
  | { type: "chat-error"; message: string }
  | { type: "prov"; file: string; prov: ProvPayload }
  | { type: "resynced"; file: string; rev: number | null; prov: ProvPayload | null }
  | { type: "branch"; branch: string; files: FileInfo[]; main: string | null }
  | { type: "saved"; file: string; error?: string | null }
  | { type: "building" }
  | ({ type: "built" } & Build)
  | { type: "files"; files: FileInfo[]; main: string | null }
  | { type: "committed"; sha: string; message: string }
  | { type: "refused"; file: string; reason: string }
  | { type: "lagged" }
  | ({ type: "ai" } & AiEvent)
  | ({ type: "upstream" } & UpEvent);

export interface Region { file: string; from: number; to: number; text: string; note?: string }
export interface PdfClick { page: number; x: number; y: number; file?: string; line?: number }
export interface Context { file?: string; line?: number; column?: number; selection?: Region; queue: Region[]; pdf?: PdfClick }
/** `deleted`: the agent removed the file (`after` is ""). */
export interface ProposedFile { path: string; before: string; after: string; deleted?: boolean }
export interface ApplyFile { path: string; text: string; delete?: boolean }

export type AiEvent = { session?: number; user: string } & (
  | { event: "started"; prompt: string; mode: AiMode; choice?: AiChoice; by?: string }
  | { event: "phase"; text: string }
  | { event: "turn"; role: string; text: string; model: string }
  | { event: "tool"; role: string; name: string; input: Record<string, unknown>; summary: string; error?: boolean }
  | { event: "proposal"; text: string; files: ProposedFile[]; model: string; usage: { input: number; output: number } }
  | { event: "draft"; files: ProposedFile[] }
  | { event: "error"; message: string }
  | { event: "cancelled" }
  | { event: "applied"; files: string[] }
  | { event: "discarded" }
);

export type ClientMsg =
  | { type: "open"; file: string; rev?: number }
  | { type: "edit"; file: string; rev: number; op: (number | string)[]; client: string; seq: number }
  | { type: "resync"; file: string; text: string; hash: number }
  | { type: "viewing"; file: string | null; line?: number; column?: number }
  | { type: "chat"; text: string; anchor?: ChatAnchor }
  | { type: "chat-resolve"; id: number; resolved: boolean }
  | { type: "chat-delete"; id: number }
  | { type: "ai"; action: "run"; prompt: string; context: Context; session?: number; provider?: "claude" | "codex"; model?: string; effort?: string }
  | { type: "ai"; action: "apply"; session: number; files: ApplyFile[] }
  | { type: "ai"; action: "discard" | "cancel"; session: number };

/** 32-bit FNV-1a over code points; must match `fnv1a` in src/text.rs. */
export function fnv1a(s: string): number {
  let h = 0x811c9dc5;
  for (const ch of s) {
    h ^= ch.codePointAt(0)!;
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

export const project = document.body.dataset.project!;
export const me = document.body.dataset.user ?? "";

export function socketUrl(): string {
  return `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/project/${project}/ws`;
}

/** fetch for our own server. When the Authelia login lapses, every request is
 *  redirected to auth.kmicinski.com, which the browser reports only as
 *  "Failed to fetch"; not following redirects lets us see that it happened,
 *  say so, and tell the page (it reloads, which leads through the login). */
export async function appFetch(url: string, init?: RequestInit): Promise<Response> {
  const r = await fetch(url, { ...init, redirect: "manual" });
  if (r.type === "opaqueredirect" || (r.status >= 300 && r.status < 400)) {
    window.dispatchEvent(new CustomEvent("kl-login-expired"));
    throw new Error("your login has expired — reload the page to sign in again");
  }
  return r;
}

export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const r = await appFetch(`/api/projects/${project}${path}`, init);
  if (!r.ok) throw new Error(`${r.status} ${await r.text()}`);
  const ct = r.headers.get("content-type") ?? "";
  return (ct.includes("json") ? r.json() : r.text()) as Promise<T>;
}

export function postJson<T>(path: string, body: unknown): Promise<T> {
  return api<T>(path, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
}

/** A timestamp (ms) as "Oct 4, 09:12" in the viewer's locale. */
export function when(ts: number): string {
  return new Date(ts).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}
