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
- **Dual mode's boundary is `tools::Scope::Metadata`**, enforced in
  `tools::execute` (text-returning tools are refused, not merely omitted)
  and in `context_block` (locations only). `delegate` returns structure only.
  The test `dual_mode_keeps_text_away_from_the_privileged_model` asserts it;
  keep it passing when adding tools.
- **Messages are kept in Anthropic's wire shape** (`ai/provider.rs`) and
  echoed back verbatim, including thinking and server-tool blocks. Ollama
  converts at the edge. Add providers by adding a `Provider` variant.
- **Anthropic calls are raw reqwest** (no Rust SDK). Keep
  `anthropic-version: 2023-06-01`; the `fallbacks: "default"` scalar needs
  the `server-side-fallback-2026-07-01` header; web tools step down a
  generation on a 400 that names them (`web_variant`).

## Gotchas

- latexmk exits non-zero on TeX errors but still writes a PDF in
  nonstopmode; `Compiled.ok` is the exit status, `pdf` is what's on disk.
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

## Testing

`cargo test` — unit tests next to the code; no test hits a real LLM
(`Provider::Mock`). Temp dirs come from `history::tmp_dir` (unique per call;
two tests in one millisecond once collided on a shared dir). When adding a
tool to the AI, add it to `tools::definitions` *and* `tools::execute`, decide
its `Scope`, and extend the dual-mode test if it returns text.

## Deferred (deliberately not in v1)

- Streaming of the model's text; turns are non-streaming with a 20-minute
  timeout, and progress is the tool log. Add SSE parsing in
  `provider::Anthropic::chat` if the wait feels long.
- Per-keystroke presence/cursors of other people; the lock makes a single
  writer, readers see updates live.
- A client-side web search for `local`/`dual` without Anthropic's server
  tool (`fetch_url` works; a Brave/SearXNG `web_search` would slot into
  `tools::definitions(_, network)`).
- Renaming files (delete + create), project deletion, bib management UI.
