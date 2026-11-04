# Plan

A fast, native desktop tool for reviewing large, often AI-written diffs and GitHub pull requests.
It remembers what you have already reviewed down to the hunk, across pushes and force-pushes,
walks you through a change in a sensible order, and keeps reformatting and moved code from
drowning the real edits. See [prior-work.md](prior-work.md) for the survey this plan builds on.

We build the review engine once and put two desktop front ends on it, then keep the one that is
easier to maintain and grow:

- **lite**: a standalone app on `gpui` and `gpui-component`, copying the GPL Zed code it needs
  (LSP client and similar).
- **zed**: a fork of Zed with a review crate added, inheriting Zed's editor, multibuffer, LSP for
  every language, highlighting and vim mode.

No web stack and no JavaScript: everything is Rust.

## Goals

1. **Seen state per hunk that survives pushes.** After a push, rebase or force-push, only new or
   changed hunks are unreviewed; changed hunks show what changed since you last reviewed them.
2. **Format-independent diff.** Changes that provably only touch whitespace, indentation or line
   wrapping are classified as formatting-only and hidden behind a visible count; real edits inside
   reformatted code are highlighted.
3. **Move detection.** Code moved within or across files is shown as one line, "moved from
   `a.rs:120-160`"; moves with edits show their edits.
4. **Guided walkthrough.** An ordered list of steps, each showing its own hunks and why it matters.
   Walkthroughs are validated against every revision: each review unit is covered exactly once.
5. **Private annotations.** Accept, flag or annotate hunks; notes follow their hunk across
   revisions and fall back to the file when the hunk disappears.
6. **Agent round trip.** Export accepted, flagged and annotated hunks as numbered Markdown or JSON
   to hand back to Claude or Codex; agents can also drive the CLI.
7. **Code browsing with LSP** at the reviewed revision: go to definition, find references, hover.
8. **Fast on huge diffs and live on pushes.** Virtualized rendering, analysis off the UI thread, and
   a banner on new pushes instead of a reload.

The overriding safety rule: **the tool must never make a reviewer believe they reviewed code they
did not.** Whenever identity, equivalence or a move is ambiguous, it reopens the code or shows it
in full rather than guessing.

## Non-goals for the MVP

- Posting comments to GitHub (export only; posting comes after the MVP).
- Multi-user or server features; review state is local to one reviewer.
- Structural (AST) diff rendering. Tree-sitter is used only to prove formatting equivalence.
- AI walkthrough generation inside the app (the CLI emits a prompt and imports the result).

## Concepts

- **Review**: what is being reviewed, identified by a stable key. Either a GitHub PR
  (`github.com/owner/repo#123@<git-common-dir>`) or a local range in a repository
  (`<repo>:<base-ref>..<head-ref>`, or the working tree against a ref for reviewing an agent's
  uncommitted turn). A PR review is keyed per clone because its snapshots are pinned in that
  clone's object store: opening the same PR from another clone starts a separate review instead
  of reading history whose objects that clone lacks. Worktrees of one clone share its common git
  directory and therefore its review.
- **Snapshot**: an immutable commit. Either a real commit, or a synthetic commit capturing the
  working tree (see "Snapshots of uncommitted work").
- **Revision**: one observed version of a review, identified by its `(base, head)` snapshot IDs,
  where `base` is the merge base of the target and the head. A new revision is recorded whenever
  either changes. Revisions are numbered from 1 in observation order and never forgotten; they form
  a linear chain. An observation is deduplicated only against the **current** revision, the last
  one: a snapshot pair seen earlier, such as after a force-push back to an earlier head, becomes a
  new revision (its content-addressed analysis is reused), so A, B, then A again are revisions 1,
  2 and 3. Every command and front end defaults to the current revision.
- **Review unit**: the atomic unit of review state, local to one revision. Either a **chunk** (a
  maximal run of changed lines in one file with zero context lines) or a **file unit** for a change
  that is not a line change: mode change, type change, submodule update, binary or oversized
  content, empty-file addition or deletion, or a rename. File units are produced independently of
  chunks, so a file whose text and mode both change has both, and a type change (for example a
  symlink replaced by a regular file) keeps chunks for every side that is a regular text file,
  diffed against an empty other side. File units show the values that changed: modes, symlink
  targets (escaped losslessly, with `\xNN` for bytes that are not UTF-8) and submodule commits.
  Every changed tree entry yields at least one review unit, so progress and walkthrough coverage
  cannot miss a change.
- **Unit and move IDs**: `u<revision>-<index>` and `m<revision>-<index>`, such as `u3-0007` and
  `m3-0002`, local to one revision. Indices are zero-based and written with at least four digits,
  so the first unit and the first move of revision 1 are `u1-0000` and `m1-0000`.
- **Hunk**: the display unit. Units within a few lines of each other are shown together, like a
  unified-diff hunk. A hunk is reviewed when all its units are.
- **Correspondence**: a mapping between the units of two revisions, computed from positions in
  both snapshots and confirmed by content (see "Carrying state across revisions"). Review state
  moves between revisions only along an unambiguous correspondence.
- **Mark**: a reviewer's verdict on a unit at a revision: `seen`, `accepted`, `flagged`, or
  `unseen`, which explicitly requires review again and stops inheritance from earlier marks. A mark
  records the unit's full evidence (both sides of the change, paths, modes, object IDs) and the
  revision it was made on. Verdicts, including move acknowledgements, go only on units of the
  current revision, checked under the lock; with revisions in observation order, review state then
  always flows forward in time and an older verdict can never overtake a newer one. Notes may go on
  any revision.
- **Note**: free text attached to a unit at a revision, carried along correspondences.
- **Walkthrough**: ordered steps over the units of the revision it was written for, carried to
  later revisions through correspondences.

## Engine design

### Git access

Shell out to the `git` CLI rather than linking a git library: it supports every repository layout
users have (partial clones, worktrees, alternates), matches what `git diff` shows, and keeps the
engine's dependency tree small enough to embed in Zed without version conflicts. The git layer
produces verified immutable inputs; everything after it is pure analysis over object IDs.

- A long-lived `git cat-file --batch` process reads blobs.
- `git diff-tree -r -z --raw -M <base> <head>` lists changed entries with modes, object IDs and
  rename detection.
- GitHub PR metadata (target branch, head) comes from `gh pr view <n> --json ...`.
- Fetches go into operation-specific refs (`refs/code-review/tmp/<id>`), never `FETCH_HEAD`, and
  snapshot identity is taken from the fetched objects, not from an earlier observation. Every fetch
  passes `--no-write-fetch-head --refmap=` so a remote's configured refspecs never update
  remote-tracking refs; nothing outside `refs/code-review/*` changes.
- Every snapshot is pinned under `refs/code-review/<review>/snap/<object-id>`, named by its own
  object ID, so pinning is idempotent: a ref that already exists holds the same object by
  construction. A force-push or GC cannot lose a pinned snapshot, and a crash after pinning leaves
  only a harmless extra pin, reconciled against the event log later.
- `git merge-base --all` computes the base. Several merge bases are reported to the user and the
  first is used. A shallow clone without the merge base is deepened with `--deepen` from the PR's
  remote (or `origin` for local reviews), wanting the target and the clone's shallow boundary
  commits but never the head, which may be a synthetic working-tree capture or an unpublished
  commit; wants the remote does not know are dropped one by one. Deepening has the same isolation
  as every other fetch; when it cannot find a merge base, the review fails with an explicit error.
- Partial clones: analysis reads every blob it needs, which makes git fetch missing objects, and
  the analysis cache stores the review evidence (the text of each unit), so past reviews stay
  reproducible offline. Browsing an old revision needs its objects and may need the network.

### Snapshots of uncommitted work

To review an agent's uncommitted turn, capture the working tree without touching the user's index:
with a temporary `GIT_INDEX_FILE` holding a copy of the user's index (which keeps git's stat cache
and sparse-checkout entries; `git read-tree HEAD` when there is no index), `git add -A` (tracked
and untracked files, respecting `.gitignore`), `git write-tree` and `git commit-tree` with a fixed
identity and date produce a synthetic snapshot commit, pinned like any other. Staged and unstaged changes are captured together. Two captures of
different contents are two revisions; a capture identical to the current revision deduplicates.

### Line diff and units

For each changed entry, diff old and new blobs line by line with imara-diff (Histogram). Each
change region with zero context becomes a chunk. Path, mode, type and opaque-content changes become
file units (see "Review unit"), identified by their old and new paths, modes, types and object IDs;
an entry can have both chunks and file units.

### Format-independent classification

Formatting-only is a claim that two pieces of code are equivalent, so it is only made when it can
be proven, and is never used as identity:

- Supported languages are parsed with tree-sitter on the **whole** old and new blobs.
- Each side is serialized as its canonical tree: node kinds, field names, nesting and leaf text
  (comments and string literals verbatim, zero-width nodes such as automatic semicolons included),
  and nothing about whitespace, except where whitespace is syntax: any non-whitespace byte between
  sibling nodes (a token the grammar hides), whitespace between JSX children and inside JSX element
  names, whitespace inside lexemes a grammar splits into several tokens (Rust lifetimes and labels
  after `'`, macro metavariables after `$`, `$x:frag` bindings, tuple-index chains such as
  `x.0.1`), and the indentation of every Python logical line. Python re-indentation is therefore never formatting-only, even
  when uniform; wrapping inside brackets still is.
- In the MVP, a file's chunks are **formatting-only** exactly when the two canonical trees are equal
  and neither side has a parse error. Whole-file equality is the simplest rule that cannot hide a
  structural change: dedenting a Python statement out of an `if` block changes the tree, and so
  does JavaScript's `return\nisAdmin;`.
- In a file with real changes, no chunk is formatting-only, but a leaf-level diff still highlights
  which tokens really changed so reformatting noise is easy to read past. Nothing is hidden there.
- A per-chunk proof is later work. It must include each leaf's attachment to its enclosing
  structure (for example the root path of node kinds and field names), not just the subtree around
  the chunk, since a Python dedent changes attachment without changing the statement itself.
- Each language's predicate is validated by its own test suite of equivalent and non-equivalent
  pairs before the language counts as supported.
- Inside other chunks of supported languages, a leaf-level diff highlights which tokens really
  changed.
- MVP languages: Rust, Python, JavaScript, TypeScript and TSX, Go, JSON and TOML. Grammar versions
  are pinned to the ones Zed uses so the engine can link into the Zed fork. Like Zed, JavaScript is
  parsed with the TSX grammar. Zed ships no TOML grammar (TOML is an extension there), so
  `tree-sitter-toml-ng` has no Zed pin; it has no `links` key and shares Zed's
  `tree-sitter-language`.
- Unsupported languages and parse errors are never formatting-only. Lines whose only difference is
  whitespace may be dimmed there, but they stay visible and unreviewed.

### Move detection

Moves are a presentation over review units, not units of their own:

- Across all files of a revision, removed and added line runs are matched by content, at least 3
  lines and 20 tokens long. A plain move requires exact text in the MVP. Whitespace-trimmed
  matching may only *propose* a moved with edits pairing, whose differences, including indentation,
  stay visible. Recognizing re-indented moves as plain moves waits for the per-chunk formatting
  proof.
- Assignments are one-to-one and non-overlapping: each removed run pairs with at most one added
  run. A block copied to two places shows one move and one plain addition.
- **Moved**: both sides equal after normalization. Shown as one collapsed line, "moved from
  `old.rs:120-160`" at the destination and "moved to `new.rs:40-80`" at the source. Acknowledging
  it marks only the units the move covers completely, on both sides; a unit with changes outside
  the move stays unreviewed until those are reviewed too.
- **Moved with edits**: at least 80 % of the lines equal. Shown expanded by default with the inner
  diff between the two blocks; never part of a noise category.
- Any part of a unit not covered by a move stays visible as an ordinary change.

### Noise classification

Generated and vendored files are collapsed by default: `linguist-generated` or `-diff` in
`.gitattributes`, known lockfiles (`Cargo.lock`, `package-lock.json`, `pnpm-lock.yaml`,
`yarn.lock`, `go.sum`, `poetry.lock`, `uv.lock`), snapshots (`*.snap`), and files whose first lines
carry `@generated` or `DO NOT EDIT`. Collapsed units still need a mark; the UI offers one action to
mark a whole category, and every category stays visible as a count.

### Carrying state across revisions

Review state never moves by content hash alone. For each pair of consecutive revisions the engine
computes a correspondence between their units:

1. **Map files**: renames between the two heads and between the two bases, from
   `git diff-tree -M`. A rename is ambiguous when another file has equal similarity for the same
   source or destination; units in ambiguously mapped files have no unchanged correspondence.
2. **Map positions**: line maps from a line diff of each file's old head against its new head, and
   old base against new base. A line diff picks one alignment among equally good ones, so a
   position is trusted only when the unit is distinguishable: its change appears once in the file
   on both revisions, or its surrounding context lines match too.
3. **Candidates**: a unit of the newer revision corresponds to a unit of the older one when the
   older unit's position maps onto it, on the head side for added lines and on the base side for
   removed lines. A deletion-only chunk maps through its insertion point.
4. **Confirm**: the correspondence is **unchanged** when both sides of the change are equal
   (exactly in the MVP; formatting normalization waits for the per-chunk proof), both footprints
   map exactly on the head and the base side, and the unit is distinguishable; **changed** when the
   change differs. An equal change whose position is not trusted is reviewed again.
5. **Ambiguity reopens**: a unit with several candidate predecessors (merges), a predecessor with
   several candidate successors (splits), or no positional candidate at all has no unchanged
   correspondence. Content equality without positional correspondence, such as an identical edit
   appearing elsewhere in the file, never carries a mark.

Correspondences chain across revisions, so each unit knows its **last reviewed predecessor**: the
nearest earlier unit in its chain with a mark, even if intermediate revisions were never reviewed.
A unit then is:

- **reviewed** when every link back to that marked predecessor is unchanged and no explicit
  `unseen` mark lies on the chain;
- **changed since reviewed** otherwise, shown with an interdiff of the two changes: both the removed
  and the added sides, with base-side differences (the target branch moved) distinguished from
  head-side differences (the PR changed);
- **new** when it has no marked predecessor.

Notes follow the same chains. A note whose chain ends, or splits ambiguously, attaches to the file
and is flagged as outdated, keeping its original revision and text as provenance.

### Walkthroughs

A walkthrough is JSON written for one revision:

```json
{
  "version": 1,
  "review": "github.com/owner/repo#123",
  "revision": 3,
  "title": "Add token-based move detection",
  "steps": [
    {
      "title": "New tokenizer",
      "why": "Everything else builds on the token stream",
      "units": ["u3-0007", "u3-0012"]
    }
  ]
}
```

- Unit IDs are revision-local. Import rejects a walkthrough whose review or revision does not match,
  and unknown or duplicate IDs.
- For every revision displayed, the engine derives a fresh partition: each current unit sits in
  exactly one step. Units inherit the step of their corresponding predecessor; stale references are
  dropped with a warning; units without a predecessor in the walkthrough go to a "New since
  walkthrough" step; formatting-only, moved and noise units left unassigned go to a "Noise" step,
  and any other unassigned unit to "Other changes" with a warning.
- Each step renders only its own units; a neighboring unit of another step appears as context, not
  as part of the step.
- `cr walkthrough prompt` prints a prompt with a compact unit catalog (ID, path, line range, short
  excerpt) for an agent to answer; `cr walkthrough import` validates and stores the result.
- Without an imported walkthrough, a deterministic default orders files by directory, puts tests
  after the code they test, and puts noise last.

### Storage

Per review, under `$XDG_DATA_HOME/code-review/reviews/<review-key>/`:

- `events.jsonl`: an append-only event log of revisions observed, marks, notes and walkthrough
  imports.
- `analysis/<base>-<head>.json`: the analysis of a revision (units, evidence, classifications,
  moves), content-addressed by its snapshot IDs, written atomically (temporary file and rename) and
  recomputable from git.
- `review.lock`: serializes state transitions (held exclusively) and log reads (held shared).

Rules:

- **State transitions are serialized as a whole.** Recording a revision takes `review.lock`,
  rereads the log, compares the snapshot pair with the current revision's, pins both snapshots,
  publishes the analysis, appends the event with the next number, and only then releases the lock.
  The appended event is the commit point: a revision number exists only once its event is in the log, so a crash before
  that leaves nothing to clean up but idempotent pins and a content-addressed analysis file.
  Background work carries the snapshot IDs it was started for, so a stale result can never replace
  a newer one.
- **Appends are complete or absent.** A writer holds the lock across `write_all` of one
  newline-terminated record and `sync_data` before reporting success. On opening the log under the
  lock, a trailing record without a newline (a writer that died) is truncated, because it was never
  acknowledged.
- **Readers tail safely.** A reader holds a shared lock on `review.lock` for each poll, so it never
  reads while a writer truncates or appends: a read racing recovery could otherwise stitch a dead
  writer's partial record to the record that replaces it. A reader advances its offset only past
  complete records and never keeps the bytes of an incomplete trailing line: it rereads from its
  last complete-record boundary every time, because recovery may truncate that tail and another
  writer may append a different record at the same offset. A corrupt record in the interior of the
  log is a hard error, never skipped.

An event log keeps the state human-readable, diffable and easy to sync, and avoids linking SQLite,
which would conflict with Zed's own `libsqlite3-sys`.

### Live updates

For PR reviews, poll every 30 seconds (configurable): `git ls-remote` for the PR head and the target
branch, and `gh pr view` for the target branch name, since a PR can be retargeted. A change of
either side fetches into a temporary ref, verifies the fetched objects, records a revision and
analyzes it in the background. The UI shows a banner such as "New push: 3 new and 2 changed
units" and applies it on request, keeping the cursor on the current unit.

### Code browsing at a revision

Each revision being browsed gets its own detached worktree at its head snapshot, created lazily
under the review's data directory and shared by every client viewing that revision. A client
switching revisions switches its project to the other worktree; worktrees no client uses are
reclaimed. Worktrees are snapshots: the front ends open them read-only, so an editor save cannot
change what is being reviewed.

## Workspace layout

| Path | Crate | License | Purpose |
|---|---|---|---|
| `crates/review-core` | `review-core` | MIT OR Apache-2.0 | Engine: git layer, snapshots, units, formatting proof, moves, noise, correspondence, walkthroughs, event store, export. Synchronous API; callers run it on their own background threads. |
| `crates/review-cli` | `review-cli` (binary `cr`) | MIT OR Apache-2.0 | CLI over the engine, with `--json` output for agents. |
| `crates/review-lite` | `review-lite` | GPL-3.0-or-later | The standalone gpui + gpui-component app. GPL because it may copy Zed code. |
| Zed fork (separate repo) | `code_review` | GPL-3.0-or-later | Review crate inside a Zed fork, depending on `review-core` by path. |

`review-core` must stay embeddable in Zed: no async runtime, no SQLite, no git library, and only
small, common dependencies (imara-diff, serde, serde_json, thiserror, a hash crate, and tree-sitter
with grammar versions matching Zed's).

## CLI (MVP)

| Command | Purpose |
|---|---|
| `cr open <pr-number \| pr-url \| base..head \| --worktree [ref]>` | Start or refresh a review; records a revision if anything changed and prints a summary. |
| `cr status` | Counts per file and in total: new, changed since reviewed, reviewed, accepted, flagged, noise. |
| `cr show [--unreviewed] [--step N] [--file PATH]` | Colored text rendering with moves collapsed and formatting-only units hidden behind counts. |
| `cr mark <unit>... seen \| accepted \| flagged \| unseen` | Set review state. |
| `cr note <unit> <text>` | Add a note. |
| `cr export [--format md \| json]` | Numbered list of flagged and annotated units with `file:line`, unit ID and excerpt, for an agent. |
| `cr walkthrough prompt \| import <file> \| show` | Agent-generated walkthroughs. |
| `cr watch` | Poll for new revisions and print what changed. |

Every command accepts `--review <key>`, defaulting to the review for the current repository and
branch, and `--json`.

## Front ends

Both front ends first get the same **thin slice**, which tests the risky integration seams before
either becomes a full product:

- Open a review by PR number, PR URL or range; show the revision and its progress.
- Sidebar with walkthrough steps, each with progress.
- Main view: the current step's hunks, virtualized; moved and formatting-only units collapsed to
  one line with a toggle; changed-since-reviewed units show their interdiff.
- Keyboard: next and previous unit, next and previous step, mark seen and advance, accept, flag.
- Scroll a 50,000-line diff smoothly.
- Go to definition from a hunk into the revision's worktree.

Notes, the new-push banner, find-references and the rest of the feature set follow once one front
end is chosen.

### lite

As built for M2 (`crates/review-lite`):

- **Stack.** `gpui-pre` 0.3.9 and `gpui-component` 0.8.0, both Apache-2.0. 0.9.0 and 0.8.1 are
  newer but still inside the repository's 14-day dependency cooldown; move to 0.9.0 (which uses
  `gpui-pre` 0.3.11) once it clears. `gpui-pre-platform` is built with the `wayland` feature only,
  so the Linux build links nothing from X11 and needs only `libxkbcommon-dev`. The app is dark
  only; it ignores the system appearance.
- **One tree-sitter.** `gpui-component`'s tree-sitter highlighter is an optional feature and stays
  off, so the dependency graph has exactly one tree-sitter, the one `review-core` pins to Zed's
  revision, and no `links` conflict. Syntax highlighting in the app waits for a highlighter that
  shares that version.
- **Rendering.** A sidebar of walkthrough steps with reviewed/total counts, and a `uniform_list`
  of single-line rows for the selected step, laid out from the engine's answers: collapse reasons,
  moves, interdiffs, display hunks, descriptions and notes. Front ends never decide these
  themselves; navigation order, step progress and next-unreviewed come from
  `walkthrough::Partition`. Marks go through `Review::mark` on a background thread, then the
  history is reloaded and the rows rebuilt from it. `gpui-component`'s editor and its diff view
  are not used: a uniform list of prepared rows is the simplest thing that stays smooth at 50,000
  lines.
- **Go to definition.** A minimal synchronous stdio JSON-RPC client (`lsp.rs`, about 400 lines)
  speaks `initialize`, `didOpen` and `textDocument/definition` to rust-analyzer (and, if
  installed, gopls, pyright and typescript-language-server) rooted at the revision's worktree, on
  gpui's background executor. Zed's `lsp` crate was the alternative, but it is async on Zed's
  executor and pulls in its settings, fs and http layers; one request type does not justify that.
  Build scripts, proc macros and `cargo check` stay off in rust-analyzer, but `cargo metadata`
  and `rustc` still run under the reviewed tree's Cargo configuration, which can name any
  program as a `rustc` wrapper. Running a language server on a reviewed head is therefore
  arbitrary code execution, and it is gated on trust (below). Find references and hover reuse the
  same client later.
- **Trust.** No tool that can execute reviewed code starts unless the review is trusted.
  `Review::trust` and `Review::set_trusted` in `review-core` store the decision as a `trust` event
  in the review's log, so every front end shares it. Without a decision, the working tree and a
  range whose head is a local branch are trusted, and pull requests and other ranges, such as a
  remote-tracking ref or a bare commit, are not. A local branch can still hold someone else's
  code, for example one made by `gh pr checkout`; revoke trust for it. In review-lite, `g` on an
  untrusted review shows a one-line hint, and `T` explains what will run and trusts on a second
  press, or revokes trust and stops the language servers. The demo's head is a local branch, so it
  is trusted.
- **Scrolling.** `review-lite --bench-scroll` scrolls every row of the review once, 30 rows per
  frame, and prints frame-interval and per-frame build-time percentiles. On the perf fixture
  (60,000 changed lines, 70,000 rows) it holds 60 Hz, with about 1 ms of app work per frame.

### Demo review

`review_core::demo` materializes a built-in, deterministic sample pull request from the plain files
under `crates/review-core/demo/` into `target/demo/repo`, with its review data in
`target/demo/data`: fixed commit IDs, three revisions (revision 2 is a force-push that carries,
changes and adds units; revision 3 returns one hunk to its revision-1 form), a walkthrough and a
few marks. It covers every feature the front ends render: formatting-only files, a re-wrap with a
real edit, a plain move and a move with edits, a rename with edits, mode, symlink and binary
changes, deletion-only chunks and noise. `--demo` rebuilds it and opens revision 1, and
`--demo-revision N` advances to revision N keeping marks, in `cr` and `review-lite` alike. It is
the shared fixture for both front ends; the Zed fork opens the same review.

### QA harness

`review-lite --qa` reads line commands on standard input (`key`, `action`, `find`, `scroll`,
`resize`, `state`, `quit`) and answers each with one line once its background work is done and a
frame has rendered; `state` prints the visible UI state as JSON. `scripts/qa/run.sh` (`task qa --
<script>`) runs the app inside a headless sway, feeds it a script, checks answers with `expect`
lines and saves `grim` screenshots to `target/qa/`. `scripts/qa/smoke.qa` walks the demo. The Zed
fork reuses the runner with its own QA mode.

### zed

- Clone Zed into `~/dev/zed-review` with `upstream` pointing at `zed-industries/zed`; build with
  `CARGO_TARGET_DIR=/mnt/ssd2/cargo-targets/zed-review`. Whether and where to publish the fork is
  decided later (a GitHub fork of a public repository is always public).
- A `code_review` crate registers an action to open a review. It opens the revision's worktree as
  the project, then a review item: a multibuffer with one excerpt per hunk in walkthrough order,
  step headers as blocks, and a buffer diff against the base text so Zed renders the hunks
  natively.
- A dock panel lists steps and progress; mark actions are bound in the keymap; review state shows in
  the gutter. LSP, highlighting and browsing come from Zed.
- The patch to upstream files stays minimal (workspace member, registration in `zed` init, keymap
  entries) so rebasing on upstream stays cheap.

## Milestones

1. **M0: research and plan.** This file and [prior-work.md](prior-work.md).
2. **M1: engine and CLI.** `review-core` and `cr`, done when the acceptance tests below pass.
3. **M2: lite slice.** The thin slice on gpui + gpui-component; built, see [lite](#lite).
4. **M3: zed slice.** The same slice as a Zed fork crate.
5. **M4: compare and choose.** Measure both, then grow the chosen one to the full feature set.
6. **Later:** post comments and reviews to GitHub; import historical PR pushes from GitHub's
   force-push timeline events; an MCP server over the engine for agents; more languages; in-app
   walkthrough generation; risk hints per file; CI and lint results inline.

## Acceptance tests for M1

Each runs against temporary git repositories built in the test:

- Force-push carry-over: mark all units seen, amend one chunk, rebase onto a newer base and
  force-update the head; only the amended chunk is unreviewed, shown as changed since reviewed.
- Line shift: an unrelated edit above a reviewed chunk does not reopen it.
- Duplicate edit: an identical edit added earlier in the file is new, and the reviewed one stays
  reviewed.
- Skipped revision: reviewed at revision 1, unreviewed changes at 2, more at 3; revision 3 shows the
  interdiff against revision 1.
- Base-only change: the head stays, the target branch changes the removed side of a reviewed chunk;
  the chunk is changed since reviewed and the interdiff shows the removed side.
- Deletion-only chunks carry over and reopen like others.
- Ambiguity: two identical files with identical edits, renamed in a rebase, reopen; so does an edit
  repeated in a file whose context does not distinguish it.
- `unseen` on an inherited mark keeps the unit unreviewed in later revisions.
- Merge and split of chunks reopen, and notes keep their provenance.
- Re-wrapping a Rust call is formatting-only; `git diff -w` would show it.
- A file with a re-wrapped call and one real edit has no formatting-only chunk, and the real edit's
  tokens are highlighted.
- JavaScript `return isAdmin;` to `return\nisAdmin;` is not formatting-only.
- Re-indenting a Python block is not formatting-only, and neither is dedenting one statement out of
  an `if` block.
- A change inside a multi-line string literal is not formatting-only.
- An unsupported language is never formatting-only.
- A function moved to another file is one move; a block copied to two places is one move and one
  addition; a moved block with a one-line edit is expanded and shows that edit.
- A YAML block moved and re-nested is not collapsed as a plain move.
- Acknowledging a move that only partly covers a chunk leaves that chunk unreviewed.
- A mode-only change, an empty-file addition and a pure rename each produce a file unit; a mode
  change on a file with text changes adds a file unit next to its chunks.
- A renamed file keeps its reviewed chunks.
- Uncommitted work: two different captures are two revisions; the user's index is unchanged.
- Two processes recording a revision for different heads at once get distinct revision numbers and
  consistent pins.
- A writer killed mid-append leaves a log that reopens with the partial record truncated; a reader
  stopped at any byte boundary loses no complete record, including when the partial record it saw is
  truncated and replaced by a different one.
- A crash after pinning but before the revision event leaves a state from which the next attempt
  succeeds.
- A walkthrough survives a new revision: units keep their steps, new units go to "New since
  walkthrough", and stale references are dropped.
- Lockfiles and `@generated` files are classified as noise and still need a mark.
- A 50,000-line diff is analyzed in under a second in release mode.

## Choosing between the front ends (M4)

Compare on: clean and incremental build time, binary size, startup time, scrolling a 50,000-line
diff, LSP quality, lines of code we own, upstream churn (lite: gpui and gpui-component releases;
zed: rebase conflicts per month), and how easy the walkthrough and marking UI were to build.

## Open questions

- How should stacked PRs be handled: one review per PR, or a review spanning the stack?
- Where should the Zed fork live, given that a GitHub fork of a public repository is public?
