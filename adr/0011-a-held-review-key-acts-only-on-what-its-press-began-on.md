# 11. A held review key acts only on what its press began on

**Status:** accepted · **Date:** 2026-11-24

## Context

Space approves the selected hunk, or the selected change since review
([ADR 10](0010-each-change-since-a-review-is-approved-on-its-own.md)), and moves on to the next
one that needs the reviewer, as lazygit moves on after staging a hunk. A held Space repeats, so
the move gives the repeats a new target the reviewer never saw. So do a capture or another
window's approval that leaves the selection on another change, and a press that reveals what it
acts on: Space on a folded unit, on a hunk scrolled away, or in the review buffer on a change
only the diff shows.

gpui's `is_held` tells a repeat from a press. A terminal inside tmux sends a held key as the same
bytes as taps ([ADR 12](0012-every-key-a-terminal-sends-is-a-press.md)).

## Decision

- **Space approves and moves on at once,** in every terminal, with or without a keyboard
  protocol, for a whole hunk and a change since review alike: no key waits for a save, so
  every key is resolved against what shows when it comes.
- **A review key that repeats acts only on what its press began on,** told apart exactly: a
  change since review by the engine's `ShownRun`, anything else by its units. A repeat on
  anything else, or on anything after its press revealed what it acts on, is refused, and
  says so; a new press acts.
- **A repeat is what the front end says it is:** review-gui passes gpui's `is_held`, so a held
  Space there approves one change and stops. review-tui cannot tell, and every key it sends is a
  press (ADR 12).

## Consequences

- Every way the selection changes is covered by one comparison of targets, not by a guard for
  each way the rows are laid out again.

## References

- `crates/review-ui/src/model/guard.rs`
- `crates/review-ui/src/model/live.rs` (`carried_change`)
- `crates/review-ui/src/model/marks.rs` (`approve_change`)
- `crates/review-gui/src/app/keys.rs`
