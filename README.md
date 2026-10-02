# kloverleaf

A hosted LaTeX editor in the shape of Overleaf: you type on the left, the
PDF on the right recompiles as you type, and a click in either side finds the
place in the other. Every project is a git repository you can clone and push
to. Every edit lands in a durable history. An AI that can read the sources,
look at the PDF, search the web and compile proposes changes you review as a
diff before they are applied. One Rust binary; the browser side is a little
TypeScript around Monaco and PDF.js.

```
make                                  # tsc for web/src, then cargo build --release
KLOVERLEAF_PASSWORD=secret cargo run  # http://127.0.0.1:7200/
```

Needs `latexmk` and a TeX distribution (`synctex` comes with it), `git`, and
poppler's `pdftoppm`/`pdftotext`/`pdfinfo` for the AI's view of the PDF.

## How it fits together

```
projects/<name>/*.tex ──▶ live.rs: Project (text of every file, the lock, the build loop)
        ▲    │                 │ debounce 250ms, flush, latexmk ──▶ .kloverleaf/out/main.pdf ──▶ browser (PDF.js, pages swapped in place)
 git push    │                 ├──▶ history.rs: every change, as a tree, in .kloverleaf/history.sqlite
        │    └── autocommit every 30s ──▶ .git ◀── /git/<name>.git (smart HTTP: clone, pull, push)
        │
        └── ai/: a prompt + cursor/selection/queued regions/PDF click ──▶ agent over a scratch copy ──▶ proposal (diff) ──▶ apply
```

| Layer | Where | What it is |
|---|---|---|
| Project | `src/project.rs` | The directory layout, which files are text, main-file and engine detection (`% !TEX program`, `% !TEX root`). |
| Live | `src/live.rs`, `web/src/editor.ts` | In-memory documents, the edit lock, the build loop, the WebSocket protocol, autosave. |
| Build | `src/build.rs` | latexmk, the log parsed into diagnostics with file and line, SyncTeX both ways, page images and text. |
| History | `src/history.rs` | The edit log: deltas, replacements, snapshots, git commits and lock events, in SQLite per project. |
| Git | `src/gitd.rs` | Repository setup, autocommit, smart HTTP by spawning `git upload-pack`/`receive-pack`. |
| AI | `src/ai/`, `web/src/ai.ts` | Providers (Anthropic, Ollama, mock), tools over a scratch workspace, the agent loop, dual-LLM mode, proposals. |
| App | `src/main.rs`, `auth.rs`, `api.rs`, `pages.rs` | Routes, identity, the tool API, the pages. |
| Viewer | `web/src/pdf.ts` | PDF.js, one canvas per page, rendered lazily and swapped in place on each new version. |
| Language | `web/src/latex.ts` | Monaco's LaTeX tokenizer and completions (commands, environments, `\ref` labels, BibTeX keys). |

Each source file starts with a comment that explains its part; read those
before the code.

### Editing

The server holds the authoritative text of every text file. The editor sends
Monaco's deltas with a hash of the result; a mismatch is refused and answered
with the full text, which heals any drift. Tools and the AI replace whole
files through the API and every open editor updates at once. Text is written
to disk one second after the last change and always before a compile.

**The lock.** Whoever edits first holds the project's edit lock; everyone
else's editor is read-only until the holder releases it, it expires (90s
after their page stops pinging), or someone steals it from the header. A lock
change is noted in the history.

**Compiling.** Typing pauses for 250ms → latexmk (`-pdf`, or `-xelatex` /
`-lualatex` from a `% !TEX program =` comment) in `.kloverleaf/out` with
SyncTeX. A document with errors still yields a PDF in nonstopmode, so you keep
seeing pages; errors and warnings become markers in the editor and the log is
a click away. The PDF stays in memory and is served from there; only a PDF
whose bytes changed is reloaded in the browser, and pages are re-rendered
into fresh canvases that are swapped in one at a time, so nothing blanks.

**SyncTeX.** Alt-Enter (or the ⇢ pdf button) highlights the cursor's line in
the PDF. Double-click in the PDF to jump to the source; a single click only
records the position as context for the AI.

### Files

The left pane lists the project. Text files (`.tex`, `.bib`, `.sty`, …) open
in the editor; anything else opens in a new tab. Upload with the ↑ button,
by dropping files on the page, or `curl -T fig.png -u user:password
http://…/api/projects/<p>/files/figs/fig.png`. The ★ marks the main file;
hover another `.tex` to make it the main.

### Git

`projects/<name>/` *is* the repository. The server commits every 30 seconds
when something changed (`autosave`), after every applied AI proposal (with
the prompt as the message), and before accepting a push. Clone it:

```
git clone http://user:<KLOVERLEAF_PASSWORD>@host/git/<name>.git
```

Pushes update the working tree (`receive.denyCurrentBranch=updateInstead`)
and the live documents reload from disk, so a push shows up in open editors
within a second and triggers a compile. `.kloverleaf/` is ignored by git.

### History

Every change to every text file is an entry in `.kloverleaf/history.sqlite`:
keystroke deltas, whole-text replacements (API, AI, git, restores), with
author, time and metadata — an AI edit carries its prompt and model, a commit
entry its sha. Full texts are stored only at snapshots (every non-delta
entry, and every 100 deltas or 32 KB along a chain), and the text at any
entry is the nearest snapshot plus a replay of the deltas below it, so the
log costs little and any version is a few hundred rows away. Entries point at
their parent; a restore creates a new entry that points at the current head
and names what it restored, so heads never rewind and "current text" is
always unambiguous.

**Time travel.** The history button opens a timeline along the bottom:
a slider and a chip per event (keystroke runs coalesced, replacements,
AI edits with their prompt, git commits, lock changes). Scrub it and the
editor *becomes* the open file as it was at that moment — read-only, with
the lines that have changed since shaded and a green mark wherever text was
added later — and, when you pause, the PDF pane shows that version compiled
(the project's texts of that moment in a scratch copy; compiled once per
entry and cached). *Restore this version* adopts it through the live
document; *back to now* returns. The arrow keys step the slider.

### AI

Open the panel (ai button, ⌘⇧A). The model is told where you are: the
cursor, the selection, regions you queued (select text, ⌘⇧Q, optionally with
a note each), the last place you clicked in the PDF (mapped to a source line
by SyncTeX), and the last compile's errors. It works on a scratch copy of the
project with these tools: list, read and search files, make exact
replacements, compile, look at a PDF page as an image, read the PDF's text,
fetch a URL, and (Anthropic) server-side web search and fetch. When it
finishes you get a diff per file in an editable diff editor; apply it, edit
it first, ask a follow-up in the same session, or discard. Applying goes
through the live document (so it needs the lock), is recorded in the history
with the prompt, and is committed to git.

Modes (`KLOVERLEAF_AI_MODE`):

- `anthropic` (default when `ANTHROPIC_API_KEY` is set) — `KLOVERLEAF_AI_MODEL`
  (default `claude-fable-5-1`), `KLOVERLEAF_AI_EFFORT` (default `high`).
  Adaptive thinking, prompt caching on the system prompt, server-side
  `fallbacks: "default"` on the models that take it (a safety refusal is
  retried on another model inside the same call), web search and fetch.
- `local` — everything through Ollama (`OLLAMA_URL`, default
  `http://127.0.0.1:11434`; `KLOVERLEAF_LOCAL_MODEL`, default `qwen3`). Pick a
  model with tool support.
- `dual` — the dual-LLM pattern for private text. A *privileged* frontier
  model plans, searches the web and decides but never receives a line of the
  document: its tools are file names and sizes, compile status with error
  *locations*, the web, and `delegate`. A *quarantined* local model does all
  reading and writing, one delegate call at a time, with the author's request
  and context, and reports back only which files changed and whether the
  project compiles. The boundary is a type (`tools::Scope`), not a prompt; a
  test asserts the privileged side never sees the text.

There is also a synchronous API for tools: `POST /api/projects/<p>/ai` with
`{prompt, context?}` returns the proposal; `POST …/ai/<session>/apply` applies it.

## API for tools

Authenticate with the session cookie, `Authorization: Bearer <KLOVERLEAF_PASSWORD>`
(acting as the `X-User` header, default `tool`), HTTP Basic (what git sends),
or `Remote-User` behind a trusted proxy. The full list is at the top of
`src/api.rs`; the shape of a session is: list files, `GET` a file, edit,
`PUT` it back, `POST compile`, read the diagnostics, `GET page/1.png` to
look.

## Running it for real

```
KLOVERLEAF_PASSWORD=… ANTHROPIC_API_KEY=… docker compose up -d --build
```

binds 127.0.0.1:7200; put a reverse proxy with TLS in front. Behind Authelia
(Caddy `forward_auth`), set `KLOVERLEAF_TRUST_REMOTE_USER=1` and the proxy's
`Remote-User` header is the identity; the password then only matters for git
over HTTP and for tools, which don't carry a browser session — bypass the
proxy's auth for `/git/*` (and `/api/*` if tools call in from outside), as
Atlas does for `/ics/*`. Browser sessions live in memory, so a restart logs
everyone out. The image installs `texlive-full`; trim it in the Dockerfile
if the size matters.

| Variable | Default | Purpose |
|---|---|---|
| `KLOVERLEAF_ROOT` | `.` | Where `projects/` lives |
| `KLOVERLEAF_BIND` | `127.0.0.1:7200` | Listen address |
| `KLOVERLEAF_PASSWORD` | unset | The shared password. Unset and no trusted proxy ⇒ open, as `KLOVERLEAF_DEV_USER` (`local`) — local development only |
| `KLOVERLEAF_TRUST_REMOTE_USER` | `0` | Trust the proxy's `Remote-User` header (Authelia) |
| `KLOVERLEAF_PUBLIC_URL` | `http://<bind>` | Shown in the clone instructions |
| `KLOVERLEAF_AUTOCOMMIT_SECS` | `30` | Autocommit interval |
| `KLOVERLEAF_LATEX_TIMEOUT_SECS` | `120` | latexmk is killed after this |
| `ANTHROPIC_API_KEY`, `KLOVERLEAF_AI_MODE`, `KLOVERLEAF_AI_MODEL`, `KLOVERLEAF_AI_EFFORT`, `OLLAMA_URL`, `KLOVERLEAF_LOCAL_MODEL` | see AI | |

## Developing

`make test` runs the unit tests (log and SyncTeX parsing, the history store's
replay, the lock, the git round trip, the AI loop against a scripted provider
including the dual-mode boundary). The browser code is plain TypeScript
compiled by `tsc` (no bundler); Monaco, monaco-emacs and PDF.js load from
CDNs. The Rust binary embeds `web/dist/*.js`, so run `make web` after
touching `web/src`.
