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
poppler's `pdftoppm`/`pdftotext`/`pdfinfo` for the AI's view of the PDF — and,
for the AI, the `claude` CLI (see §AI).

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
| AI | `src/ai/`, `web/src/ai.ts` | The agent CLIs (`claude.rs`, `codex.rs`, what they share in `cli.rs`), the loopback MCP tools over a scratch workspace (`mcp.rs`, `tools.rs`), sessions, conversations, proposals. |
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

### Tracking a repository (GitHub)

A project can mirror a directory of someone else's repository and push back
to it: on the project list, *track a GitHub repository*, paste a link like
`https://github.com/owner/repo/tree/main/paper` (or `POST /api/projects`
with `{name, repo, branch?, subdir?}`). The server keeps a bare clone in
`.kloverleaf/upstream.git` and remembers `base`, the upstream commit the
project last agreed with (`src/upstream.rs`).

- **Their pushes come in by themselves.** Every minute
  (`KLOVERLEAF_UPSTREAM_POLL_SECS`) the server fetches; when nobody has typed
  for 20 s it three-way merges the upstream's directory into the live texts
  (lines first; inside a conflicting run of lines, words — so two people
  editing different sentences of one paragraph-per-line LaTeX paragraph
  merge cleanly). Merged-in text is attributed to a `git` origin named after
  the commits' authors.
- **⌘S pushes.** Everything since the last push becomes one commit on top of
  the upstream branch — linear history, only the tracked directory touched,
  authored as you (`KLOVERLEAF_GIT_IDENTITIES`). A message can be typed in
  the ⇅ panel; otherwise it is `Edit <files>`. If someone pushed meanwhile it
  merges first and retries.
- **Leaving doesn't lose work.** The local git autosave still runs every 30 s;
  and when the last editor has been gone 90 s with unpushed work, the server
  pushes it (`Autosave from kloverleaf: …`). The panel can turn that off.
- **Conflicts** stop the merge (nothing is applied halfway). The ⇅ chip turns
  red; *resolve…* shows each place side by side (yours / theirs, with what it
  was before) to keep mine / take theirs / both / edit. A decision is keyed by
  the conflict's contents, so it survives a re-merge if that text didn't move.
- **Where others are working.** The panel lists recent changes by other
  people (last 30 days, from `git blame` of the upstream head) grouped by file
  and `\section`; their lines get an orange mark in the gutter (darker =
  more recent; hover for who, when and the commit message).

**Which upstream branch** is a project setting: the branch menu at the top of
the ⇅ panel (or `POST /api/projects/{p}/upstream/branch {branch}`, managers
only) switches it. The switch is a merge with the old `base` as ancestor, so
the project becomes the new branch's directory plus whatever you had not
pushed yet, and pushes go to the new branch from then on.

The project's own (kloverleaf) branches still work; only the kloverleaf
branch the project was created on syncs. The upstream never sees `.kloverleaf/` (the `.gitignore`
line is stripped going out). ssh to GitHub uses `KLOVERLEAF_GIT_SSH_KEY`
(else `$HOME/.ssh`); give it a key that can write to that one repository — a
deploy key — rather than an account key: whoever can edit the project can
push to the upstream with it.

### Branches

Time is a tree, not a line. From any version in the history — a dot on the
tree map, or the moment the slider is parked on — *branch from here* forks a
named branch whose working tree is that snapshot; the header's `⎇` chip
lists the branches and switches between them (the editor reloads every file,
the PDF recompiles). Each branch keeps its own fine-grained history and
provenance, and its timeline is its lineage: its own entries plus the
parent's up to the fork. Every branch is also a real git branch of the same
name, so a clone sees them all. The AI is told which branch it is on, where
it forked from, and what happened there lately.

### Who wrote what

Every file carries a provenance map — which run of text came from which
person, which AI apply (with its prompt, model and the version it addressed),
which git push, restore or upload — kept beside the history, never in the
text. Runs the AI wrote are tinted in the editor; other people's are tinted
differently; yours are plain. Hover for the story, click an AI run for the
prompt and a way to travel back to the text it was asked about, ⌘⇧M to make
a run yours. The header's `● AI 23% · jade 8%` is the file's split; click it
to hide the colours.

Attribution is decided in blocks, not characters: keystrokes are the
typist's; a whole replacement (an applied proposal, a push, a restore) is
diffed against the previous text by line, then by word inside changed lines,
and small unchanged islands next to edits are absorbed into them, so an
inserted paragraph is one run and the author's sentence it landed in stays
the author's. Whitespace-only changes take their neighbour's author. The
rules are versioned: when they change, every map is rebuilt from the history
on the next open (`POST /api/projects/<p>/provenance/rebuild` does it on
demand, admins only).

### Guests

*invite* in the editor's header makes a single-use link. Whoever opens it
picks a name and a password and becomes a guest of that project only: the
editor, the API, git over HTTP and the AI all work for them there, the
project list shows only their projects, they cannot create projects, and any
other request is refused (403). A guest can be invited to more projects with
the same name and password. A guest session is the person: it is honoured
even behind a proxy that also knows them by a household account, so what
they write is attributed to the guest, not the account; the header names
the session (`jade (guest) · leave`). Admins — the proxy-authenticated
owner, or anyone with the shared password — see everything; `GET/POST
/api/projects/<p>/invites`, `DELETE …/invites/<token>`, `POST …/members`
and `DELETE …/members/<user>` manage access. Guests live in
`projects/.members.sqlite` (Argon2id hashes; five wrong passwords lock a
name for 30 s). This is what lets kloverleaf sit on the open internet with
only its own login: set `KLOVERLEAF_TRUST_REMOTE_USER=0`, give admins the
shared password, and let guests in by link.

**Members** are the middle tier, for a proxy that lets in more people than
the household: set `KLOVERLEAF_ADMINS=kris,kat` and every other
`Remote-User` is a *member* — they see only the projects they created or
were shared, may create projects (each becomes theirs), and may share or
invite within those; they never see anyone else's. *share* in the editor's
header (or `POST /api/projects/<p>/members {"user"}`) adds a login the
proxy already knows — no password involved — to a project. A member has no
shared password, so for `git clone` they set their own on the project list
(`POST /me/password`, at least 8 characters) and use it over HTTP Basic;
it scopes them exactly like a guest's. Unset `KLOVERLEAF_ADMINS` and every
proxy user is an admin, as before.

### Navigation

Hover a `\ref` to see the caption or heading it points at, a `\cite` to see
the BibTeX entry, an `\includegraphics` to see the picture. F12 or ⌘-click
jumps to the `\label`, the entry, or the file. The outline in the files pane
lists sections, figures and tables; click to jump. Completions follow
Overleaf's: commands with argument snippets as you type, environments with
their `\end`, labels and citation keys with context, project files and
packages where they belong.

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
plus the CLI's own web search and fetch. When it
finishes you get a diff per file in an editable diff editor; apply it, edit
it first, ask a follow-up in the same session, or discard. Applying goes
through the live document (so it needs the lock), is recorded in the history
with the prompt, and is committed to git.

The agent is a headless CLI run once per turn — **`claude`** (Claude Code) or
**`codex`** (OpenAI's Codex CLI), picked per question in the panel along with
the model and how hard it thinks — exactly as the slides and notes apps on
the same server run theirs. Neither gets tools of its own: their hands are
this server's loopback MCP endpoint (`/mcp`, bearer-token gated, one scratch
workspace per session), plus the CLI's own web search. Their streamed output
becomes the panel's activity log; the CLI's session/thread id is kept on the
session so a follow-up resumes the same conversation (switching agents starts
a new one). Nothing in the agent's output is trusted as an edit — only what
it wrote through the tools, as a diff.

Setup. Claude: the `claude` binary on `PATH` (or `KLOVERLEAF_CLAUDE_BIN`) and
`CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token` (or `ANTHROPIC_API_KEY`).
Codex: the `codex` binary (or `KLOVERLEAF_CODEX_BIN`) and a logged-in
`$CODEX_HOME/auth.json` (`codex login` on the host; the app writes
`$CODEX_HOME/config.toml` itself). Both need a writable `HOME`. Each agent
is offered only when it is installed and logged in; with neither, the panel
says why. Menus: `KLOVERLEAF_CLAUDE_MODELS` and `KLOVERLEAF_CODEX_MODELS`
(comma lists); the default pick: `KLOVERLEAF_AI_PROVIDER` (`claude`|`codex`),
`KLOVERLEAF_AI_MODEL`, `KLOVERLEAF_AI_EFFORT`. `KLOVERLEAF_AI_MODE=off`
disables the panel. Codex runs unsandboxed in an empty scratch directory
(the container has no kernel sandbox); the live project is never its cwd.

Conversations are kept with the project (`.kloverleaf/ai/threads.json`): the
panel's switcher lists them, a reload or a server restart brings the open one
back with its proposal, and a follow-up continues it. The AI's edits show up
as a draft diff while it is still working.

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
KLOVERLEAF_PASSWORD=… CLAUDE_CODE_OAUTH_TOKEN=… docker compose up -d --build
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
| `KLOVERLEAF_ADMINS` | unset ⇒ every proxy user | Comma-separated `Remote-User` names with full access; everyone else the proxy admits is a member (own + shared projects only) |
| `KLOVERLEAF_PUBLIC_URL` | `http://<bind>` | Shown in the clone instructions |
| `KLOVERLEAF_AUTOCOMMIT_SECS` | `30` | Autocommit interval |
| `KLOVERLEAF_LATEX_TIMEOUT_SECS` | `120` | latexmk is killed after this |
| `KLOVERLEAF_AI_MODE` | on | `off` disables the AI |
| `KLOVERLEAF_AI_PROVIDER`, `KLOVERLEAF_AI_MODEL`, `KLOVERLEAF_AI_EFFORT` | `claude`, `claude-opus-5`, `medium` | The default pick in the panel |
| `KLOVERLEAF_CLAUDE_MODELS`, `KLOVERLEAF_CODEX_MODELS` | built-in lists | What the model menu offers |
| `KLOVERLEAF_CLAUDE_BIN`, `KLOVERLEAF_CODEX_BIN` | `claude`, `codex` | The CLI binaries (name on PATH or a path) |
| `CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY` | unset | Claude's credential |
| `CODEX_HOME` | `$HOME/.codex` | Codex's login (`auth.json`) and threads |
| `KLOVERLEAF_UPSTREAM_POLL_SECS` | `60` | How often tracked repositories are fetched (and merged when idle) |
| `KLOVERLEAF_GIT_SSH_KEY` | unset ⇒ `$HOME/.ssh` | Private key for ssh remotes of tracked projects |
| `KLOVERLEAF_GIT_IDENTITIES` | unset | `login=Name <email>;…` — who upstream commits are authored as (else `login <login@kloverleaf>`) |

## Developing

`make test` runs the unit tests (log and SyncTeX parsing, the history store's
replay, the lock, the git round trip, the MCP tools over a workspace, and the
AI turn against a stand-in `claude` script that speaks stream-json). The browser code is plain TypeScript
compiled by `tsc` (no bundler); Monaco, monaco-emacs and PDF.js load from
CDNs. The Rust binary embeds `web/dist/*.js`, so run `make web` after
touching `web/src`.
