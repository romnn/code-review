# 11. A held review key acts only on what its press began on

**Status:** accepted, its terminal timing superseded by
[ADR 12](0012-every-key-a-terminal-sends-is-a-press.md) · **Date:** 2026-11-24

## Context

Space approves the selected hunk, or the selected change since review
([ADR 10](0010-each-change-since-a-review-is-approved-on-its-own.md)), and moves on to the next
one that needs the reviewer, as lazygit moves on after staging a hunk. A held Space repeats, so
the move gives the repeats a new target the reviewer never saw. So do a capture or another
window's approval that leaves the selection on another change, and a press that reveals what it
acts on: Space on a folded unit, on a hunk scrolled away, or in the review buffer on a change
only the diff shows.

A terminal inside tmux sends a held key as the same bytes as taps. gpui's `is_held` tells a
repeat from a press, and so would the kitty keyboard protocol's event types where a terminal
speaks them, with every key reported as an escape code, since otherwise a key that types text
still comes as plain bytes. The reviewer works in plain tmux, so no protocol can be required.

## Decision

- **Space approves and moves on at once,** in every terminal, with or without a keyboard
  protocol, for a whole hunk and a change since review alike: no key waits for a save, so
  every key is resolved against what shows when it comes.
- **A review key that repeats acts only on what its press began on,** told apart exactly: a
  change since review by the engine's `ShownRun`, anything else by its units. A repeat on
  anything else, or on anything after its press revealed what it acts on, is refused, and
  says so; a new press acts.
- **A repeat is what the front end says it is where it can tell:** review-gui passes gpui's
  `is_held`. review-tui tells nothing: keys of one review action closer together than half a
  second are one key held down. Asking a terminal for the kitty protocol's event types, to
  make the guard exact there, is a later enhancement; nothing will depend on it.
- **A capture on screen takes marks while the next one is under way.** A mark marks what the
  reviewer saw, which the engine records only as shown and checks under the review's lock;
  only busy git and a replaced clone refuse marks, since the capture on screen may then not be
  what the checkout is. A hunk that changed in the working tree still refuses review keys for
  a second ([ADR 4](0004-a-working-tree-review-follows-the-disk-and-records-what-is-marked.md)).

## Consequences

- Two taps of Space within half a second in a terminal are one held key: the second, now on
  the next change, is refused and asks for a new press.
- Every way the selection changes is covered by one comparison of targets, not by a guard for
  each way the rows are laid out again.

## References

- `crates/review-ui/src/model/guard.rs`
- `crates/review-ui/src/model/live.rs` (`carried_change`, `live_refusal`)
- `crates/review-ui/src/model/marks.rs` (`approve_change`)
- `crates/review-gui/src/app/keys.rs`
