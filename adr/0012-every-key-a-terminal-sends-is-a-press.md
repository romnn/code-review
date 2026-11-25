# 12. Every key a terminal sends is a press

**Status:** accepted · **Date:** 2026-11-24

## Context

[ADR 11](0011-a-held-review-key-acts-only-on-what-its-press-began-on.md) makes a review key that
repeats act only on what its press began on, where the front end tells a repeat from a press.
A terminal inside tmux sends a held key's repeats as the same bytes as taps. The kitty keyboard
protocol's event types tell them apart where a terminal speaks them, with every key reported as
an escape code, since otherwise a key that types text still comes as plain bytes. The reviewer
works in plain tmux, so no protocol can be required.

## Decision

- **Where the front end cannot tell a repeat from a press, every key is a press and acts,**
  as in lazygit. No timing guess refuses a key.
- **A terminal keyboard protocol that reports repeats would give review-tui ADR 11's guard,**
  as an enhancement; nothing depends on it.

## Consequences

- In a terminal, holding Space approves change after change at the key's repeat rate, as it
  stages hunk after hunk in lazygit. Space still never leaves the file by itself.
- `Model::key` takes whether the front end reports a repeat, a plain `bool`.

## Rejected alternatives

- **Telling a held key by timing:** keys of one review action closer together than half a
  second are one key held down. Reviewers tap faster than that: with four similar changes on
  screen, four quick taps of Space approved the first and refused the rest with "the key is held
  down", which lazygit, whose staging flow cr follows, never does.

## References

- `crates/review-ui/src/model/guard.rs`
- `crates/review-tui/src/app.rs`
- `crates/review-gui/src/app/keys.rs`
