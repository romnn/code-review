# 13. A capture shows with the next frame, and a key acts on what the last frame showed

**Status:** accepted · **Date:** 2026-11-24 · supersedes the rule that a changed hunk refuses
review keys in [ADR 4](0004-a-working-tree-review-follows-the-disk-and-records-what-is-marked.md),
and the refusal of marks while git is busy in
[ADR 11](0011-a-held-review-key-acts-only-on-what-its-press-began-on.md)

## Context

A live review shows each capture of the working tree in place of the snapshot on screen. A
capture landed whenever its job finished, so a key handled after it, before the next frame,
acted on code the reviewer had not seen. Two refusals covered that: a hunk that changed in the
last capture refused review keys for a second, and every mark was refused while git was busy in
the checkout. Both are guesses about time, not about what showed. With coding agents editing
and running git in the checkout, they kept refusing Space on code the reviewer had read, and
the next capture's status line replaced the refusal before it could be read.

## Decision

- **A capture shows only in the turn that draws the next frame.** It waits in the live view's
  one waiting slot until then: review-tui shows it as it lays out a frame, after handling the
  input that came since the last one, and review-gui in gpui's next-frame callback, which runs
  right before gpui draws. So no live capture changes what a key acts on before a frame showed
  it. A key's own effects, such as Space moving on to the next change, apply at once, so quick
  taps keep working, and so do the outcomes of the reviewer's own actions, such as a save.
- **No timer and no busy git refuses a review key.** A mark names the snapshot and units it was
  made on, and the engine records exactly that; a later capture carries it only along the usual
  correspondences, so code that changed since shows as still to review. The busy checks around
  a capture stay, and the overview says what git is doing; a checkout replaced by another clone
  still stops marks.
- **The status line keeps the outcome of the reviewer's last action through captures,** and
  what it said about how the snapshot stood, such as that a file is done or which file is next,
  clears once another capture shows, since it may no longer hold; the header's progress, derived
  from what shows, tells how the new one stands. A capture that only brings edits says nothing.
  A branch or base that moved and a capture with nothing to review still say so, and a refresh
  the reviewer asked for answers while its request still shows.

## Consequences

- Saves, watch events and timers land as soon as they finish; only a capture waits for a frame.
- A front end must show waiting captures where it draws, and the model tests draw a frame in
  `settle`.
- Marks on a capture another one replaced are still refused by the engine, which records marks
  only on the current revision; that changes separately.

## References

- `crates/review-ui/src/model/live.rs` (`offer`, `publish`, `publishes`)
- `crates/review-tui/src/app.rs` (`App::render`)
- `crates/review-gui/src/app/mod.rs` (`publish_with_next_frame`)
