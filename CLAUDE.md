# kloverleaf — maintenance notes

Read `README.md` first for what it does and how the parts fit. This file is
the working notes for changing the code. **Keep both in sync with reality.**

## Invariants

- **The server's text is the truth.** Every write to a text file goes through
  `Project::edit` (deltas, hash-checked) or `Project::replace` (whole text).
  Both record a history entry, broadcast an `Event::Changed`, and wake the
  build loop. Never write a text file to disk any other way while the server
  runs; a git push is the one exception and is followed by
  `reload_from_disk`, which adopts the disk as truth through `replace`.
- **Hashes must agree.** `text::fnv1a` and `fnv1a` in `web/src/protocol.ts`
  are the same function over code points; offsets are UTF-16 (Monaco's).
- **The lock gates browser edits and applies, not tools.** `Project::edit`
  takes a free lock for the editing user and refuses when another holds it;
  `ai::apply` and `restore` require it too. API `PUT`s bypass it on purpose
  (tools are the person running them).
- **History never rewinds.** `heads` only move forward; a restore is a new
  entry with `meta.restores`. Snapshot policy is inside `Store::record` —
  changing it changes replay cost, not correctness. Project-level notes
  (commits, locks) have `file = ""` and no text.
- **Branches are a column, not a copy.** Every history entry carries a
  `branch`; heads and provenance are per branch (`branch_heads`,
  `branch_provenance`); `Store::lineage_of` turns a branch into
  `[(branch, upto)]` and `list`/`summary`/`files_at`/`latest_for` filter by
  it, so an entry id is global and any version is addressable from anywhere.
  `Project::branch_from` / `switch_branch` settle (flush + commit), move git
  to a branch of the same name, then `adopt` the snapshot's texts: files the
  history never recorded (like `.gitignore`) are never removed. The current
  branch name is remembered in `.kloverleaf/branch`.
- **`.kloverleaf/` is private and gitignored**; `project::valid_path` refuses
  it and `.git`. Everything else in the project directory is user data.
- **Builds run in `.kloverleaf/out`** with `-synctex=1`; the PDF is read
  into memory after each compile and served from there. `pdf_changed` (a
  hash of the bytes) is what makes the browser reload; identical output
  after a whitespace edit does not.
- **The AI never touches the live project during a run.** It works in
  `.kloverleaf/ai/<session>/` (`tools::make_workspace`); the proposal is the
  diff between that copy's text files and the session's base. Apply goes
  through `replace` with `kind = "ai"` and the prompt in `meta`, then commits.
- **The agent is a subprocess, never an HTTP client.** `ai/claude.rs` spawns
  `claude -p --output-format stream-json` with `--strict-mcp-config` pointing
  at our own `/mcp` and every built-in file/shell tool disallowed;
  `ai/codex.rs` spawns `codex exec --json` with a `[mcp_servers.kloverleaf]`
  entry the app writes into `$CODEX_HOME/config.toml`. Both run through
  `ai/cli.rs` (spawn, the stdout event loop, the wall clock, stderr for the
  post-mortem, the retry when a resume is stale); a provider only builds its
  command and reads its own event shapes. The only way either
  touches a project is `ai/mcp.rs`, which scopes every call to the session
  named in the `X-Kloverleaf-Session` header or in the bearer's `:<session>`
  suffix (codex can only send a bearer). Keep it that way: no `Read`/`Edit`/
  `Bash` for claude, and cwd at `$HOME` / an empty scratch dir, never the
  workspace (their own files would otherwise appear in the proposal). The
  `Catalog` in `ai/mod.rs` is the single source of what may be picked; a
  session's provider is fixed, its model/effort may change per turn.
- **Provenance is attributed in blocks.** `provenance::Map::rebase` (every
  whole-text replacement: AI apply, push, restore, upload, reconnect, branch
  fork) diffs by line (patience), then by word inside changed lines, then
  runs the diff-match-patch semantic cleanup (an unchanged run flanked by
  edits and no longer than either is absorbed) and gives whitespace-only
  insertions their left neighbour's origin. Never go back to a
  character-level diff: it matched stray letters across an inserted
  paragraph and scattered one-letter "AI" islands through the author's
  prose. Keystrokes (`Map::apply`) are exact and mirrored in
  `web/src/provenance.ts`; `rebase` is server-only (the browser receives the
  whole map after a replacement). `history::PROVENANCE_VERSION` stamps the
  rules into `PRAGMA user_version`: bump it when they change and every map
  is rebuilt from the log on open (`Store::rebuild_provenance`, which
  replays each branch's lineage and reuses or re-creates the origins named
  by `meta.entry`). The test `rebase_attributes_in_blocks_not_letters` is
  the real failure case.
- **A login session names the person before the proxy does.** `auth::who`
  checks the session cookie first, then `Remote-User`: someone who accepted
  an invite behind Authelia is the guest, not the household account, so
  their edits carry their own origin. The editor header shows the session
  (`auth::session`) with a *leave* link; `/logout` returns to `/`.
- **Scope is enforced in the gates, not the handlers.** `auth::who` yields a
  `Who { user, projects, creates }` — three shapes via `Who::full` (admin:
  `projects: None`), `Who::member` (a proxy user not in `KLOVERLEAF_ADMINS`:
  their `member_projects` rows, recomputed per request, may create) and
  `Who::guest` (invite login: same rows, may not create). `gate`/`git_gate`
  refuse any request whose path names a project outside the list
  (`project_in_path`, which understands nested and un-nested paths) and any
  creation by someone without `creates`. Handlers that list projects filter
  by `may`; access management (invites, `POST …/members` shares, removals)
  requires `Who::manages(project)` = admin or a member *of that project*.
  `App::create_for` is the only creation path handlers use: it records the
  creator as a member when they are not an admin, or the project would be
  invisible to them. A guest's Basic auth is tried before the shared
  password, so a guest cannot escalate by guessing names; a member's own git
  password (`members.set_password`) lands them in the same guest-shaped
  `Who`, which is fine for git.
- **`/mcp` is loopback-only by construction**: the bearer is random per
  process and only ever written into the per-run config file (0600, deleted
  after). It is not an API for outsiders — that is `/api` with the password.
- **Tool events come from the server, not the transcript.** `mcp.rs` emits
  the pending/done `tool` pair around each call (it knows the summary); the
  stream parser only emits `turn` (assistant text), `phase` (what it is doing)
  and the final result. Adding a tool = `mcp::catalog` + `mcp::execute`.
- **Resume is best-effort.** The CLI's `session_id` lives on the session; a
  follow-up passes `--resume`. If that fails (transcript gone), one retry
  without it, announced in the log.
- **Conversations persist.** `Session::emit` writes every event into the
  project's `.kloverleaf/ai/threads.json` (`ThreadStore`); the workspace
  (`ai/<id>/`) and base texts (`ai/<id>.base.json`) stay on disk until the
  proposal is applied or discarded, so `Ai::session(project, id)` can rebuild
  a session after a restart and a follow-up still resumes the CLI. Session
  ids are per project (the MCP session key is `project:id`). A run the server
  died in is marked `interrupted` on load. The panel's switcher and
  `restore()` read `/api/projects/{p}/ai/threads`.
- **A tracked upstream is three file maps and one commit id.** `upstream.rs`
  never checks anything out: `base` (the upstream commit the project last
  agreed with), `ours` (live texts + binary files on disk, through
  `outgoing`) and `theirs` (origin's head) are read with `ls-tree`/`cat-file`
  from the bare mirror, merged per file by `merge_all`, and a push is built
  with plumbing in a private index on top of origin's head. Live texts are
  only ever written through `Project::replace_if` (compare-and-swap against
  the `ours` the merge was computed from; on a miss, merge again) — never
  write a merge result any other way, or a keystroke typed during the merge
  is lost. Only `local_branch` syncs; the upstream branch is `cfg.branch`, changed by `Mode::Track` (set it, keep `base`, then pull — never reset `base` to the new head, or unpushed work is silently dropped). `Config::pushed` lists our own commits
  so they never count as a collaborator's activity. Conflict ids hash the
  conflicting texts; the client sends *all* its picks on every resolve.

## Gotchas

- latexmk exits non-zero on TeX errors but still writes a PDF in
  nonstopmode; `Compiled.ok` is the exit status, `pdf` is what's on disk.
- **TeX logs are not UTF-8.** pdflatex copies source bytes into the log and
  wraps lines mid-character; `build::read_lossy` reads `.log`/`.blg`/`.aux`.
  A strict `read_to_string` once returned an error → empty log → *no*
  diagnostics at all for any paper with a curly quote in it.
- Diagnostics are three passes: `parse_log` (reads TeX's context after each
  error up to `l.N`, which is never fed to `track_files`), `tidy` (drops echo
  errors and summary warnings, names undefined macros, hints), and `refine`
  (reads the project: runaway arguments and unclosed environments move to
  where they started; columns from the `l.N` excerpt). Each case has a test
  built from a real log; add one when a new error shape turns up.
- `Compiled.stale`: TeX stopped before writing a PDF ("no output PDF file
  produced"), so the `main.pdf` on disk is an earlier build's. It must never
  count as a new version or be cached as a history version's PDF.
- The log's file stack (`track_files`) is a heuristic — parentheses in
  messages confuse it. Errors come from `-file-line-error` lines, which
  carry their own file, so only warnings can be misattributed.
- `synctex edit` prints absolute input paths; `parse_inverse` strips the
  canonical project directory. `-i line:col:file` matches by suffix.
- A `git push` to a non-bare repo needs a clean tree: `git_refs` flushes and
  commits before advertising refs for `receive-pack`.
- Monarch rules with capture groups must cover every character
  (`(\\begin|\\end)` not `\\(begin|end)`), or Monaco throws while tokenizing.
- `rusqlite` with `bundled` compiles SQLite from source; the first build is slow.
- The `claude` binary is glibc-linked (not static): a Debian/Ubuntu image, not
  Alpine. On the home server it is bind-mounted from the host at
  `/usr/local/bin/claude`; a running container keeps the old inode after a
  host upgrade, so recreate it to pick up a new CLI version.

## Testing

`cargo test` — unit tests next to the code; no test hits a real LLM — the
AI tests run a stand-in `claude` shell script that prints stream-json, and
exercise the MCP tools directly against a workspace. Temp dirs come from `history::tmp_dir` (unique per call;
two tests in one millisecond once collided on a shared dir). When adding a
tool to the AI, add it to `mcp::catalog` *and* `mcp::execute`, and give it a
one-line summary for the activity log.

## Deferred (deliberately not in v1)

- Token-level streaming of the model's text into the panel; `phase` events
  and the tool log are the progress today (a 25-minute wall clock bounds a
  turn). The `stream_event` deltas are already parsed in `claude.rs` if that
  changes.
- Per-keystroke presence/cursors of other people; the lock makes a single
  writer, readers see updates live.
- Local/offline models. The old Ollama and dual-LLM providers were removed
  with the move to CLIs; a third agent CLI is a new `Provider` variant plus a
  `run()` like `ai/codex.rs`.
- Renaming files (delete + create), project deletion, bib management UI.
- Upstream: linking an *existing* project to a repository (only new projects
  track one), https remotes with tokens, and an "✦ merge with AI" button in
  the conflict view (the slides app has one to copy).
