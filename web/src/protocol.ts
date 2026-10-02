// Wire types shared by the editor, the PDF viewer and the AI panel. The
// server side and the protocol's description are in src/live.rs and src/ai/mod.rs.

export interface Edit { offset: number; length: number; text: string }
export interface FileInfo { path: string; size: number; text: boolean }
export interface Diagnostic { file: string; line: number | null; severity: "error" | "warning" | "info"; message: string }
export interface Lock { user: string; since: number; until: number }
export interface Build { version: number; ok: boolean; diags: Diagnostic[]; ms: number; pdf: boolean; hasPdf: boolean }
export interface AiMode { mode: "off" | "anthropic" | "local" | "dual"; model?: string; local?: string; reason?: string }

export type ServerMsg =
  | { type: "init"; user: string; files: FileInfo[]; main: string | null; lock: Lock | null; build: Build | null; building: boolean; ai: AiMode }
  | { type: "text"; file: string; text: string }
  | { type: "edit"; file: string; changes: Edit[] }
  | { type: "saved"; file: string; error?: string | null }
  | { type: "building" }
  | ({ type: "built" } & Build)
  | { type: "lock"; lock: Lock | null }
  | { type: "files"; files: FileInfo[]; main: string | null }
  | { type: "committed"; sha: string; message: string }
  | { type: "refused"; file: string; reason: string }
  | { type: "lagged" }
  | ({ type: "ai" } & AiEvent);

export interface Region { file: string; from: number; to: number; text: string; note?: string }
export interface PdfClick { page: number; x: number; y: number; file?: string; line?: number }
export interface Context { file?: string; line?: number; column?: number; selection?: Region; queue: Region[]; pdf?: PdfClick }
export interface ProposedFile { path: string; before: string; after: string }

export type AiEvent = { session?: number; user: string } & (
  | { event: "started"; prompt: string; mode: AiMode }
  | { event: "turn"; role: string; text: string; model: string }
  | { event: "tool"; role: string; name: string; input: Record<string, unknown>; summary: string; error?: boolean }
  | { event: "proposal"; text: string; files: ProposedFile[]; model: string; usage: { input: number; output: number } }
  | { event: "error"; message: string }
  | { event: "cancelled" }
  | { event: "applied"; files: string[] }
  | { event: "discarded" }
);

export type ClientMsg =
  | { type: "open"; file: string }
  | { type: "edit"; file: string; changes: Edit[]; hash: number }
  | { type: "ping" }
  | { type: "lock"; action: "take" | "steal" | "release" }
  | { type: "ai"; action: "run"; prompt: string; context: Context; session?: number }
  | { type: "ai"; action: "apply"; session: number; files: { path: string; text: string }[] }
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

export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const r = await fetch(`/api/projects/${project}${path}`, init);
  if (!r.ok) throw new Error(`${r.status} ${await r.text()}`);
  const ct = r.headers.get("content-type") ?? "";
  return (ct.includes("json") ? r.json() : r.text()) as Promise<T>;
}
