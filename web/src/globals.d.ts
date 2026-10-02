// Globals provided by scripts the pages load before ours.
/// <reference path="../node_modules/monaco-editor/monaco.d.ts" />

/** Monaco's AMD loader (loader.min.js); monaco-emacs is loaded through it too. */
declare const require: {
  config(c: { paths: Record<string, string> }): void;
  (deps: string[], cb: (...modules: unknown[]) => void): void;
};

/** What the monaco-emacs module exports. */
interface MonacoEmacsModule {
  EmacsExtension: new (editor: monaco.editor.IStandaloneCodeEditor) => { start(): void };
}
