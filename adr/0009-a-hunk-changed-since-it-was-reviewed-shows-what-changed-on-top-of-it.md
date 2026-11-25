# 9. A hunk changed since it was reviewed shows what changed on top of it

**Status:** accepted · **Date:** 2026-11-23

## Context

A hunk the reviewer approved at one revision that changed at a later one is changed since
reviewed. It showed its whole current change, base to head, and then nested blocks titled
"since revision 5: the change itself changed" and "since revision 5: the target branch moved",
each a line diff of one side of the change. The nested blocks repeated most of the lines the
hunk had just shown, had no syntax colors, since their lines had no place in a file, and a
one-line edit to a long approved addition showed the whole addition twice. lazygit shows what
is not staged yet on top of what is: only the difference from the staged version.

The reviewed change is a safe base for that only when the base lines it changes are the same
lines now. When the target branch moved, the head may now remove or replace code the target
added, and no comparison of the two heads shows that.

## Decision

- **The engine decides what a chunk that changed since it was reviewed shows**
  (`RevisionView::since_reviewed`). It shows what changed on top of the reviewed change
  (`SinceReviewed::Delta`) when it was approved rather than flagged, the file's base entry and
  the chunk's base lines, insertion point included, are the same as reviewed, the head file has
  the same path and mode, and every link back to the reviewed unit is one-to-one and unchanged
  or changed, never untrusted or ambiguous. The reviewed change and the delta then make the
  current change. Otherwise it shows the whole current change (`SinceReviewed::Whole`), with the
  reason: it was flagged, the target branch moved, the change now affects other base lines, the
  match is uncertain, or the code is the same as reviewed. It shows even where the whole change
  is reformatting or a move.
- **A delta is the reviewed head's lines against the current head's,** as runs: each run a chunk
  of the lines that differ, with an edit script over plain words and punctuation, so every
  whitespace change shows. It is computed from the history alone, without reading blobs, and
  only when the hunk is laid out.
- **A delta shows as a new change does,** each run through
  [ADR 6](0006-a-hunk-shows-its-code-once-with-the-tokens-that-changed.md)'s script: a line once
  with the tokens that changed, or the reviewed lines above the current ones. The lines the same
  as reviewed, before, between and after the runs, are context like any unchanged line, folded
  into gaps in the hunks view. The file view shows every line. Each run is a stop and a mark of
  its own ([ADR 10](0010-each-change-since-a-review-is-approved-on-its-own.md)).
- **A reviewed line knows where it is in the head it was reviewed in,** for its syntax colors,
  and has no line number. That head is highlighted like the shown files. Side by side, the
  reviewed lines are on the left and the current ones on the right.
- **`cr show` prints the same comparison,** with three lines the same as reviewed around each
  run, and `cr show --json` keeps the interdiff of both sides for scripts.

## Consequences

- What changed since a review shows in no nested rows; they remain only for a move's edits.
- A change against a moved target branch shows whole, as a new change would, under a header that
  says why.
- A file unit shows its current entries; what it was when reviewed is in `cr show --json`.
- Highlighting is indexed per shown snapshot, so a mark another session makes on an earlier
  revision while this one shows a later one can leave a reviewed head unhighlighted until the
  next snapshot shows.

## References

- `crates/review-core/src/view.rs` (`since_reviewed`, `SinceReviewed`, `Delta`, `Reopened`)
- `crates/review-ui/src/rows/mod.rs` (`Pusher::delta`, `header_tag`)
- `crates/review-ui/src/paint/code.rs` (`syntax_layer`)
- `crates/review-cli/src/render.rs` (`render_delta`)
- PLAN.md, "Carrying state across revisions"
