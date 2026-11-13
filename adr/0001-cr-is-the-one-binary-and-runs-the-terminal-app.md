# 1. `cr` is the one released binary, and runs the terminal app

**Status:** accepted · **Date:** 2026-11-13 · **Supersedes:** releasing `review-tui` and `cr` as
two binaries, with `cr` licensed MIT OR Apache-2.0

## Context

Releases and the Homebrew cask used to install two programs: `review-tui`, the terminal app, and
`cr`, a command-line interface for scripts and agents.
A reviewer had to know which one to type, and `cr` without a command failed with a list of
subcommands, although the obvious meaning of `cr` alone is "show me the review".

The terminal app links `review-ui`, which is GPL-3.0-or-later because its highlighting queries are
copied from Zed.
A binary that contains the app is therefore GPL-3.0-or-later too.
The alternatives considered were:

- **`cr` runs a separate `review-tui` program.**
  It keeps `cr` permissively licensed, but still ships two binaries, and `cr` breaks whenever the
  two are installed apart or at different versions.
- **`cr` without a command opens the review and prints it as text.**
  It answers a script's question, not a reviewer's, and the text rendering cannot mark, fold or
  navigate.

## Decision

`cr` is the only binary releases and the Homebrew cask install.

- Without a command, `cr` is the terminal app: `cr`, `cr 128`, `cr main..feature`,
  `cr --worktree [ref]` and `cr --demo` open that review in the terminal.
  Without a target, the review is chosen as `cr open` chooses it
  ([ADR 2](0002-uncommitted-work-is-reviewed-against-the-default-branch.md)).
- With a command (`cr open`, `cr status`, `cr show`, `cr mark`, …) it is the scriptable interface,
  with `--json` output, as before.
- A target and a command cannot both be given, and a command's name is never read as a target.
  Options that only mean something to a command (`--review`, `--json`, `--no-auto-approve`) are
  refused without one rather than ignored.
- `review-tui` is a library that `cr` runs in its own process (`review_tui::run`).
  Its `review-tui` binary stays for the QA scripts and is not released.
- `review-cli`, the crate of `cr`, is GPL-3.0-or-later.
  `review-core` stays MIT OR Apache-2.0, so the engine remains embeddable under a permissive
  license.

## Consequences

- One install, one name: the app is what a reviewer gets by typing `cr`.
- `cr` carries the terminal stack (ratatui, crossterm and the highlighting grammars), so it is
  larger and slower to build than a bare engine CLI.
- Release archives ship the GPL license next to the MIT and Apache licenses of the engine inside.
- review-gui is out of scope here.
  It needs its own packaging (an `.app` on macOS, its own binary on Linux) and is not released
  yet.

## References

- `crates/review-cli/src/main.rs` (`Cli`, `AppOptions`), `crates/review-cli/src/app.rs`
- `crates/review-tui/src/lib.rs` (`Options`, `run`), `crates/review-tui/src/main.rs`
- `.goreleaser.yaml` (one build, one archive, `binaries: [cr]`)
- PLAN.md, "Workspace layout" and "CLI (MVP)"
