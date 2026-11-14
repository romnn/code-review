# Agent guidelines

Native Rust tool for reviewing large diffs and GitHub pull requests. Read [PLAN.md](PLAN.md) for
the design and milestones, [prior-work.md](prior-work.md) for the survey behind it, and the
decision records in [adr/](adr) before changing an area one governs. Keep them up to date when a
decision changes: an accepted ADR changes only through a new one that supersedes it.

## Layout

- `crates/review-core`: the engine (MIT OR Apache-2.0).
- `crates/review-cli`: the `cr` binary, the only one released: the terminal app without a command,
  and commands for scripts and agents (GPL-3.0-or-later, for the app it contains).
- `crates/review-ui`: the front ends' shared model, free of any UI toolkit (GPL-3.0-or-later,
  like the apps it is the model of): the review window's state and behavior as actions in and
  effects out, the keymap and key matcher, configuration, frame geometry and scrolling, syntax
  highlighting, and `paint`, every drawing decision that is not geometry. Its themes are TOML data
  in `crates/review-ui/themes/`, each citing its source palette. Highlighting follows
  [ADR 3](adr/0003-tree-house-highlights-a-curated-set-and-bat-the-rest.md): one manifest of
  languages, tree-house with Helix's queries (MPL-2.0, which review-languages embeds) for a
  curated set, template languages among them, and bat's syntaxes for the rest.
- `crates/review-languages`: `languages.toml`, the one manifest of highlighted languages (tier,
  grammar, queries, names, detection, bat syntax), whose build script checks it, fetches the
  tree-house tier's grammars beyond review-core's at the commits Helix pins, compiles them,
  fetches Helix's queries at the manifest's commit, and generates the tables review-ui's
  registry reads (MIT OR Apache-2.0 for its own code, each grammar under its license, Helix's
  queries under the MPL-2.0, and its own Jinja queries in `queries/` under the Apache-2.0 of the
  grammar they derive from). Tuning a language is editing its row.
  `crates/review-grammar-sources` parses the manifest and finds, checks and fetches the
  grammars' sources and the queries for it, and runs `task grammars:fetch`.
- `crates/review-gui`: the gpui + gpui-component desktop app (GPL-3.0-or-later), a view over
  review-ui in the lazygit-style flow described under "Product direction" in PLAN.md.
- `crates/review-tui`: the ratatui terminal app (GPL-3.0-or-later), a view over the same model
  that works as review-gui does, within what a terminal can show.

## Invariants

- Never let the tool make a reviewer believe they reviewed code they did not. When identity,
  equivalence or a move is ambiguous, reopen or show the code in full.
- Review state moves between revisions only along unambiguous correspondences between review units,
  never by content hash alone, line numbers, or display hunks.
- Formatting-only is a proof over tree-sitter syntax trees; unsupported languages and parse errors
  are never formatting-only.
- `review-core` stays UI-agnostic and embeddable in other applications: synchronous API, no
  async runtime, no SQLite, no git library (shell out to `git` and `gh`), and only small common
  dependencies. The graph has exactly one tree-sitter runtime, the romnn/tree-sitter fork that
  `[patch.crates-io]` selects for `tree-sitter` and `tree-sitter-language`; a grammar crate may
  depend only on `tree-sitter-language`, and `crates/review-ui/tests/one_runtime.rs` checks the
  locked graph. Check any new dependency for a runtime of its own or a `links` conflict.
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
- Front ends never reimplement engine logic; anything every front end needs belongs in
  `review-core`.
- Anything review-gui and review-tui share that is not engine logic belongs in `review-ui`: state,
  behavior, keys, configuration, and what to draw in which colors. A front end only maps the
  model's state onto its surface and its input onto model calls, and runs the effects the model
  queues.

## Building and testing

- Use the `task` commands in `taskfile.yaml`: `task check`, `task test`, `task lint`, and before
  calling work done `task lint:fc`, `task test:fc`, `task lint:ast-grep` and `task spellcheck`.
  Builds use the project's own `target/`.
- Release binaries are cross-built by `task build:goreleaser` in the cargo-zigbuild image with
  the current stable Rust, which the task installs over the image's older one, so local builds,
  CI and releases all use stable. The `build` workflow runs it on every pull request.
- Multi-line strings are written out with `indoc!`, `formatdoc!` or `writedoc!`, never with `\n`
  escapes or `concat!`; the ast-grep rules in `.ast-grep/rules/` check it.
- Tools come from `mise.toml`; on Linux, `task deps:linux` installs the system libraries gpui
  needs.
- review-languages' build script fetches its grammars and Helix's queries with `git` into the
  gitignored `crates/review-languages/_cache/` on a checkout's first build, so that build needs
  network.
  `task grammars:fetch` fills the cache ahead of an offline build, `CARGO_NET_OFFLINE=true`
  forbids fetching in both, and `CODE_REVIEW_GRAMMARS_DIR`, absolute or relative to
  `crates/review-languages`, points at prepared sources instead
  (`task grammars:fetch -- --into <dir>`). `task grammars:sync -- <helix commit>` moves the
  pins and the queries to another Helix commit, and `task grammars:sync:check` tests it.
- Never build Rust under `/tmp`; separate build trees go on disk outside it.
- Tests that need repositories create small temporary git repos with the `git` CLI.
- review-gui builds gpui for Wayland only and links only libxkbcommon and fontconfig; at run
  time it needs a Wayland compositor and a Vulkan driver. Check those before blaming the code.
- review-tui needs only a terminal. It opens the review before it takes over the terminal, logs
  diagnostics to `$XDG_STATE_HOME/code-review/review-tui.log` and prints them again on exit.
- The built-in demo review is the shared fixture for manual and scripted checks:
  `cargo run -p review-gui -- --demo`, `cargo run -p review-tui -- --demo` or `cr --demo show`
  rebuilds it under `target/demo/` and opens revision 1, and `--demo-revision N` advances it to
  revision N, keeping marks.
- QA harness: `task qa -- scripts/qa/smoke.qa` runs review-gui with `--qa` in a headless sway
  (needs `sway` and `grim`), drives it with the script's commands and saves screenshots to
  `target/qa/`. `task qa:tui -- scripts/qa/smoke-tui.qa` runs review-tui with `--qa`, drawing off
  screen, and saves each screen as `.txt` and `.ansi` under `target/qa/tui/`. Both scripts include
  `scripts/qa/smoke-common.qa`, the checks both front ends answer alike. Read the screenshots to
  check rendering. Commands are documented in `crates/review-gui/src/app/qa.rs`,
  `crates/review-tui/src/qa.rs` and `scripts/qa/run.sh`.
- `task screenshots` renders every scene of `scripts/screenshots/scenes.qa` in both front ends,
  dark and light, under `target/screenshots/all/`, and copies into `docs/screenshots/` only the
  images README.md shows, the only ones committed: to show another, reference it in README.md
  and rerun.
  `task screenshots:gallery` renders every theme in every diff style under
  `target/screenshots/gallery/`. Rerun it after a visible change and read the images.
- Any front-end change is QA'd interactively: review-gui with the `gui-qa` skill
  (`.agents/skills/gui-qa`, driven by `scripts/qa/session.sh`), review-tui with the `tui-qa` skill
  (`.agents/skills/tui-qa`, off screen and under tmux). Drive the app, read the screens, and fix
  the UX issues you find before reporting. A change to review-ui is checked in both front ends.
- `review-gui --bench-scroll` scrolls a whole review and prints frame timing. The perf test
  exports its 60,000-line fixture for it with
  `REVIEW_PERF_FIXTURE_DIR=$PWD/target/perf-fixture cargo nextest run --release --run-ignored only -E 'test(perf)'`;
  open it with the range `origin/perf-base..origin/perf-head`.

## Conventions

- Follow the `rust-style`, `rust-comments` and `rust-lint` skills.
- Commit subjects use `type(scope): subject`, with the crate name as scope where it fits.
