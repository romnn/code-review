# 12. Every key a terminal sends is a press

**Status:** accepted · **Date:** 2026-11-24 · supersedes the terminal part of
[ADR 11](0011-a-held-review-key-acts-only-on-what-its-press-began-on.md)

## Context

ADR 11 made a review key that repeats act only on what its press began on. A terminal sends a
held key's repeats as the same bytes as taps, so review-tui guessed: keys of one review action
closer together than half a second were one key held down. Reviewers tap faster than that. With
four similar changes on screen, four quick taps of Space approved the first and refused the
rest with "the key is held down", which lazygit, whose staging flow cr follows, never does.

## Decision

- **Where the front end cannot tell a repeat from a press, every key is a press and acts,**
  as in lazygit. No timing guess refuses a key.
- **Where it can tell, a repeat still acts only on what its press began on:** review-gui
  passes gpui's `is_held`, so a held Space there approves one change and stops. A terminal
  keyboard protocol that reports repeats would do the same for review-tui, as an enhancement.

## Consequences

- In a terminal, holding Space approves change after change at the key's repeat rate, as it
  stages hunk after hunk in lazygit. Space still never leaves the file by itself.
- `Model::key` takes whether the front end reports a repeat, a plain `bool`.

## References

- `crates/review-ui/src/model/guard.rs`
- `crates/review-tui/src/app.rs`
- `crates/review-gui/src/app/keys.rs`
