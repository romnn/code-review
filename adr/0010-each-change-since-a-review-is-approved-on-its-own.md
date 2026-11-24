# 10. Each change since a review is approved on its own

**Status:** accepted · **Date:** 2026-11-24 · supersedes the one stop and one mark of a unit
that shows what changed since it was reviewed in
[ADR 9](0009-a-hunk-changed-since-it-was-reviewed-shows-what-changed-on-top-of-it.md), keeping
its eligibility

## Context

[ADR 9](0009-a-hunk-changed-since-it-was-reviewed-shows-what-changed-on-top-of-it.md) shows a
hunk approved at one revision that changed since as what changed on top of it: runs of lines
that differ, with the lines the same as reviewed as context. The hunk still spans its first run
to its last and is one stop and one mark, so two one-line edits 200 lines apart in an accepted
addition are one hunk whose bar runs along 200 lines of approved code, and Space approves both
edits wherever the reviewer is. lazygit stages each hunk of unstaged changes on its own.

The unit is what carries review state across revisions, with its evidence, so a run cannot be a
unit of its own. A run has no identity across revisions either: its boundaries come from a line
diff that can align differently whenever either side changes.

## Decision

- **A unit has one effective reviewed baseline.** A whole `seen` or `accepted` mark supplies its
  head lines. Approving one run supplies text: the baseline with that run's reviewed bytes
  replaced by its current bytes, line terminators and a missing final newline included. The
  engine records it as a `partial_approval` event on the unit, with the unit's evidence and the
  whole text, a tag of its own so that an older binary fails to read the log rather than taking
  it for an acceptance. When the text equals the current head lines, it records an ordinary
  `accepted` mark instead.
- **Review actions on a unit apply in log order and the latest wins.** A whole mark replaces a
  partial approval, `unseen` cuts inheritance, and a partially approved unit stays changed since
  reviewed: counts stay in units. An unchanged link carries the state with the baseline; a
  changed link carries the baseline for comparison under ADR 9's checks, and untrusted or
  ambiguous links, a moved target and the rest still show the unit whole. A head that becomes
  equal to the baseline after a changed link shows whole as the same code reopened: equality
  never approves.
- **An approval names the change the reviewer saw.** `Review::approve_run` takes the view and
  the run as it showed it: its current lines and where they are, and the reviewed lines they
  replace. Under `review.lock`, the engine derives the unit's comparison from the log as it is
  then, with the view's capture when it is a provisional one the log does not have yet, and
  approves only a run with the same current lines at the same place replacing the same reviewed
  lines; otherwise it refuses, and records nothing, not even the capture. Only an approval
  records the capture, as a mark does, when the log still ends where its history did. The app's
  queue tells presses on the same change apart by the same identity. An approval of another run of the unit since, or any unrelated record, leaves the
  run as it was, apart from where its reviewed lines are in the baseline, so it is still
  approved; a run approved or changed since is refused.
- **A run is told apart exactly.** The comparison that finds a run makes its `ShownRun`: the
  unit, its current lines and where they are, and the reviewed and current bytes. The display
  hunk carries it, and Space queues it as it is. Selection, a file's remembered place, a folder
  preview opened at a run and a switch between the file and hunks views keep the hunk equal to
  it, and a capture follows it along an unchanged link, its lines moved as the unit's. When no
  run is equal, the selection falls back to the unit's run at or after its line, a place to
  read on from that the held-key guard of
  [ADR 11](0011-a-held-review-key-acts-only-on-what-its-press-began-on.md) takes for a new
  target.
- **Each run is a display hunk,** a stop for `j`/`k` and a Space target of its own, with no
  header, so its bar runs along it alone; the file's header gives its label. The lines the same
  as reviewed around and between runs are context like any unchanged line. Space queues the
  approval like any mark, even while another saves, and moves on to the next open change at
  once, as after approving a hunk. Until it is saved, that run alone shows approved, never the
  unit, which would show its other runs approved too, and a verdict on the whole unit made
  meanwhile shows over it; the keys that go to the next open change pass it by, and a file
  whose changes all wait so is no stop. Once saved, the rows show what is left, the selection
  kept as itself and where it was on screen. A refusal rereads the review and is reported as a
  failed save naming the run's file and line, and the selection stays where the reviewer is. Flags, resets and file and folder marks act on whole units. A
  unit the reviewer folds is one hunk, and Space there unfolds it at its first run rather than
  approving it whole. A unit that shows whole stays one stop. The file's meter counts units.
- **The review buffer marks a unit's runs, not the whole unit,** the lines Space acts on there;
  Space on a run that only removes lines, which the buffer cannot show, shows it in the diff.
- **Text a partial approval left is in no file,** so its lines have no line numbers or syntax
  colors of a head; lines a whole mark approved keep both. The comparison names where its
  baseline comes from, `ReviewedBaseline::Head` or `Partial`, which the app's and `cr show`'s
  labels read.
- **`cr mark` stays at unit scope.** `cr show` and the JSON `interdiff` compare with the
  baseline.

## Consequences

- Every partial approval stores the unit's whole text once; replay reads only the latest action
  per unit and never reproduces an old diff.
- Each approval derives the review's history again under the lock, from the caches.
- A run approved in the review buffer is the one under the cursor; on a line the same as
  reviewed between runs, Space approves nothing.

## References

- `crates/review-core/src/history.rs` (`approved_text`, `state_of`)
- `crates/review-core/src/view.rs` (`Delta::approving`, `ReviewedBaseline`, `ShownRun`)
- `crates/review-core/src/review/mod.rs` (`Review::approve_run`)
- `crates/review-ui/src/rows/mod.rs` (`Hunk`, `Shown`)
- `crates/review-ui/src/model/marks.rs` (`approve_change`, `unfold_changes`)
- PLAN.md, "Carrying state across revisions" and "Views"
