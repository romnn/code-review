# Agent guidelines

Native Rust tool for reviewing large diffs and GitHub pull requests. Read [PLAN.md](PLAN.md) for
the design and milestones and [prior-work.md](prior-work.md) for the survey behind it. Keep both up
to date when a decision changes.

## Layout

- `crates/review-core`: the engine (MIT OR Apache-2.0).
- `crates/review-cli`: the `cr` binary (MIT OR Apache-2.0).
- `crates/review-lite`: the gpui + gpui-component desktop app (GPL-3.0-or-later), a
  lazygit-style flow described under "Product direction" in PLAN.md. Its themes are TOML data in
  `crates/review-lite/themes/`, each citing its source palette.
- The Zed-fork front end lives in a separate checkout, `~/dev/zed-review`, as the `code_review`
  crate, and depends on `review-core` by path.

## Invariants

- Never let the tool make a reviewer believe they reviewed code they did not. When identity,
  equivalence or a move is ambiguous, reopen or show the code in full.
- Review state moves between revisions only along unambiguous correspondences between review units,
  never by content hash alone, line numbers, or display hunks.
- Formatting-only is a proof over tree-sitter syntax trees; unsupported languages and parse errors
  are never formatting-only.
- `review-core` stays UI-agnostic and embeddable in Zed: synchronous API, no async runtime, no
  SQLite, no git library (shell out to `git` and `gh`), and only small common dependencies.
  tree-sitter and its grammars use the versions Zed's `Cargo.lock` pins. Check any new dependency
  against Zed's `Cargo.lock` for `links` conflicts.
- Analysis runs against pinned, immutable snapshot IDs. State transitions take `review.lock`
  exclusively and log readers take it shared; the event log is append-only, one newline-terminated
  record per event.
- Fetches never touch refs outside `refs/code-review/*` or `FETCH_HEAD`: always
  `--no-write-fetch-head --refmap=` with explicit refspecs.
- Never execute reviewed code without explicit trust. Language servers, build tools and anything
  that reads the reviewed tree's build configuration start only when `Review::trust` says the
  review is trusted.
- Code copied from Zed goes only into GPL-3.0-or-later crates, with a comment naming the source
  file and Zed commit.
- Front ends never reimplement engine logic; anything both need belongs in `review-core`.

## Building and testing

- Use the `task` commands in `taskfile.yaml`: `task check`, `task test`, `task lint`, and before
  calling work done `task lint:fc`, `task test:fc` and `task spellcheck`. Builds use the project's
  own `target/`.
- Tools come from `mise.toml`; on Linux, `task deps:linux` installs the system libraries gpui
  needs.
- Never build Rust under `/tmp`. Separate build trees, including the Zed fork's, go under
  `/mnt/ssd2/cargo-targets/<name>/`.
- Tests that need repositories create small temporary git repos with the `git` CLI.
- review-lite builds gpui for Wayland only and links only libxkbcommon; at run time it needs a
  Wayland compositor and a Vulkan driver. Check those before blaming the code.
- The built-in demo review is the shared fixture for manual and scripted checks:
  `cargo run -p review-lite -- --demo` (or `cr --demo show`) rebuilds it under `target/demo/` and
  opens revision 1, and `--demo-revision N` advances it to revision N, keeping marks.
- QA harness: `task qa -- scripts/qa/smoke.qa` runs review-lite with `--qa` in a headless sway
  (needs `sway` and `grim`), drives it with the script's commands and saves screenshots to
  `target/qa/`. Read the screenshots to check rendering. Commands are documented in
  `crates/review-lite/src/app/qa.rs` and `scripts/qa/run.sh`.
- Any front-end change is QA'd interactively with the `app-qa` skill (`.claude/skills/app-qa`,
  driven by `scripts/qa/session.sh`): drive the app, read the screenshots, and fix the UX issues you
  find before reporting.
- `review-lite --bench-scroll` scrolls a whole review and prints frame timing. The perf test
  exports its 60,000-line fixture for it with
  `REVIEW_PERF_FIXTURE_DIR=$PWD/target/perf-fixture cargo nextest run --release --run-ignored only -E 'test(perf)'`;
  open it with the range `origin/perf-base..origin/perf-head`.

## Conventions

- Follow the `rust-style`, `rust-comments` and `rust-lint` skills.
- Commit subjects use `type(scope): subject`, with the crate name as scope where it fits.
