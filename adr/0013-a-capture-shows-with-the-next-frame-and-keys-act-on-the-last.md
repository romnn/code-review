# 13. The opened review and each capture show with the next frame, and a key acts on what the last frame showed

**Status:** accepted · **Date:** 2026-11-28

## Context

A live review shows each capture of the working tree in place of the snapshot on screen
([ADR 4](0004-a-working-tree-review-follows-the-disk-and-records-what-is-marked.md)). A capture
that lands whenever its job finishes lets a key handled after it, before the next frame, act on
code the reviewer has not seen. Coding agents edit and run git in the checkout all the time, so
whatever prevents that must not refuse Space on code the reviewer has read.

Both apps draw their first frame at once and open the review behind it, since opening can take
seconds: finding the repository, its locks, a confirmed capture and the whole history. The
opened review lands as a job's outcome, just as a capture does, and review-tui handles it in
the same batch as the keys typed while it opened.

Reading the whole history is what makes opening slow: every revision's analysis and the
correspondences between them, decoded from the cache, or computed again after the analysis
format changed. A checkout review of 100 revisions took about 3 s to show, and about a minute
after a format bump, while its file list and the changes of the revision on screen need one
snapshot pair. Carried marks, notes and partial approvals need the whole chain, so until it is
read no unit may show as reviewed.

## Decision

- **The checkout's review shows its last recorded revision at once, its review state pending.**
  The pending view reads nothing derived: the log's last complete revision record, and that
  revision's analysis when the cache holds it in this version's format, or else the files one
  comparison of its trees in the archive finds, renames not looked for, while a job analyzes the
  revision. It shows the actual files, restores the selection saved with the review, and shows
  each file's changes in full once they are analyzed; the files pane says "Last recorded
  revision · checking review state". The file it shows first comes with its text, read with
  the analysis, and a file whose text is still being read shows nothing until it lands, so
  every diff lays out once, with its context. It withholds everything review state decides: reviewed
  totals, automatic approval, folds by review state and what changed since a review. The file
  header keeps the place of the meter it withholds, blank and as wide as the meter that shows
  there once the review is verified, so nothing in the header moves then. A review without a
  recorded revision shows its panes empty, with no card, until it is verified.
- **Only the verified review takes marks.** The pending view is a display of its own, never a
  `Snapshot`, and only a snapshot has a view of the review's history, which the review keys'
  target lookup needs, so keys, clicks and the buffer alike find nothing to mark there and say
  the review state is still being checked; `R` records nothing either. A key on the pending view
  records nothing and is never replayed once the review shows, and a key held down from it acts
  on nothing the verified review shows until it is pressed again.
- **The model waits for two jobs, and then the confirmed capture runs.** One job reads the
  history, as opening did before, reading on from what the pending view read of the log, so
  records appended meanwhile show and a log replaced meanwhile ends the review over the
  pending view; after a cache miss it starts once the revision's analysis landed, which the
  history takes rather than reading or computing it again. The first lookup of the branch's
  pull request ([ADR 5](0005-bare-cr-reviews-the-checkout-whatever-is-checked-out.md)) starts as
  the opened checkout reaches the model, for the branch the opener read. Neither job waits for
  the other; once both ended, the model queues one confirmed capture on top of the history,
  compared with the base that lookup decides, so the first view that takes marks is on that
  base and no view jumps from one base to another. A capture that finds the checkout on
  another branch or upstream than the lookup was for is dropped, that branch is looked up, and
  the capture is taken again. While git is busy in the checkout or cr's data is full, the
  capture verifies the last recorded revision, which then takes marks. A failure says why over
  the pending view, which stays. The replay and the capture run behind what the reviewer looks
  at, in the background lane of the app's pool
  ([ADR 14](0014-only-background-work-reaches-the-engine.md)), and the lookup, which waits for
  `gh`, on a thread of its own. A lookup that lands with other news before that capture shows,
  as a retarget, has the capture taken again, and only the newest capture asked for shows; one that
  finds the checkout detached looks up the branch the capture finds.
- **A slow, missing or failing `gh` delays only the first view that takes marks.** The
  pending view stays navigable until the lookup's 10-second timeout. One that `gh` could not
  answer, missing, not logged in, failing, timed out or naming a pull request the branch does
  not pull, and a branch git cannot read to look up, let the capture compare with the default
  branch, which the header says: `PR base unavailable · using origin/main`. A repository without a GitHub remote, or a branch without
  a pull request or commits, needs no word and no lookup. A later lookup says
  `Checking PR base…` in the bar's slot while it runs; one that finds the pull request
  captures again, the view on screen marked `updating…` and still taking marks, and what
  changed is said as that capture shows, from what it and the one before were of.
- **The bottom bar says what the verification does.** While the history is replayed, the
  bar's left side says `Checking review state 37/100`: the revisions the replay finished out of
  all it reads, the log's last revision number until the replay first reports. Each revision's
  analysis counts as it is replayed, so after a change of the analysis format the count is the
  analyses done. It reports only what the replay measured, as plain text whose count is padded
  to the total's width, so its width never changes while it counts, and the revision before it
  makes way when room runs out. Then it says `Checking PR base…` while the lookup is
  outstanding and `Comparing checkout…` while the capture runs, each within the room kept for
  the count. It shows with the frames drawn anyway and with a tick four times a second while
  the replay runs, never a frame per revision.
- **The opened review and each capture show only in the turn that draws the next frame,** and so
  do the pending view's analysis and the verified review, each keeping the selected file. The
  opened review, or why it could not be opened, waits in the model, what a job brought for the
  pending view in it, and a capture in the live view's one waiting slot, until then: review-tui
  shows them as it lays out a frame, after handling the input that came since the last one, and
  review-gui in gpui's next-frame callback, which runs right before gpui draws. So no key acts on
  a review, its verification or a capture before a frame showed it: one handled before the
  review's first frame acts on the empty panes, where nothing is to review, and one before the
  verified review's on the pending view, where nothing is to mark. A key's own effects, such as
  Space moving on to the next change, apply at once, so quick taps keep working, and so do the
  outcomes of the reviewer's own actions, such as a save.
- **The opened review shows whatever the reviewer is in the middle of,** since before it no
  rows showed that it could pull from under them; what the pending view waits for, and a
  capture, waits while marks are saved or an overlay, a picker, a drag or a search being typed
  is under way. A review that opens after the
  app closed never shows, and its engine stops on a thread of its own as it is dropped
  ([ADR 14](0014-only-background-work-reaches-the-engine.md)).
- **The panes are there from the first frame, and fill as the review lands.** The first frame
  lays out the files pane and the diff pane with their headers, empty, where the review will
  show: the files pane's progress lines only their labels, the file header without a file.
  The file tree fills in when the review opens, then the diff; no card stands in for them,
  neither while the review opens nor while the pending view waits for its analysis, since
  opening is quick enough that a card only flashes. What goes on is on the status line,
  "Opening the review…" and then the pending view's "checking the review state…", and on the
  bottom bar in text of one width, "opening…" beside a spinner, then the count of the
  revisions replayed and what the confirmed capture waits for. Only the key that quits shows
  before the review does, as review keys do nothing yet; an overlay opened meanwhile, such as
  the settings, shows its own keys.
  The cards that say something stay: a review that could not be opened, a checkout or log
  replaced, nothing to review and a file to pick. review-tui exits with an error once the
  reviewer quits a review that could not be opened; quitting while the review still opens is
  no failure.
  Nothing on the way holds a key or a frame back for the terminal: review-tui asks it nothing
  ([ADR 12](0012-every-key-a-terminal-sends-is-a-press.md)).
- **No timer and no busy git refuses a review key.** A capture on screen takes marks while the
  next one is under way: a mark names the snapshot and units it was made on, and the engine
  records exactly that; a later capture carries it only along the usual correspondences, so code
  that changed since shows as still to review. The busy checks around a capture stay, and the
  overview says what git is doing; a checkout replaced by another clone, or a review's log
  replaced, still stops marks.
- **The status line keeps the outcome of the reviewer's last action through captures,** and
  what it said about how the snapshot stood, such as that a file is done or which file is next,
  clears once another capture shows, since it may no longer hold; the header's progress, derived
  from what shows, tells how the new one stands. A capture that only brings edits says nothing.
  A branch or base that moved and a capture with nothing to review still say so, and a refresh
  the reviewer asked for answers while its request still shows.

## Consequences

- Saves, watch events and timers land as soon as they finish; only the opened review, what the
  pending view waits for and a capture wait for a frame.
- The capture is taken once the history is read and the pull request looked up, so the
  verified review shows the working tree as it is then rather than as it was when the replay
  began.
- Both front ends keep 30 cells for the bar's left side, room for the count of a review of up
  to 999 revisions, so a narrow window drops a key hint two cells sooner.
- On a scratch review of 100 revisions over 200 files and 3,313 units, the first frame that can
  be navigated shows 50 ms after start, warm or after a change of the analysis format, where
  the review used to show after 0.6 s warm and 24 s after a format change; after the format
  change the selected file's changes show at 0.35 s. The verified review takes as long as the
  whole review used to.
- Opening still maintains the review when nothing else has it open, compacting the archive
  when due, and moves a review an earlier version kept into its archive, before the pending
  view shows.
- While the review state is checked, the whole file opens as in the verified review, but the
  overview, the pickers and go to definition read the verified review and say they wait for it.
  A whole file open when the verified review shows opens again from it, or the diff shows
  while it has not read the file; a verified review with nothing to review clears the pending
  view's rows.
- A front end must show what waits where it draws, and the model tests draw a frame in `settle`.
- The panes keep their places from the first frame on, a width set by the reviewer included;
  a files pane fitted to its tree, the default, widens once when the tree lands if its names
  need more than the narrowest fit.
- Marks on a capture another one replaced are still refused by the engine, which records marks
  only on the current revision; that changes separately.

## Rejected alternatives

- **The opening job waiting for the pull request's lookup, or the verification job waiting for
  it in the background.** The first delays the first frame by a `gh` round trip; the second
  holds a worker while it waits, and on a pool of two, where background work takes one worker,
  would wait for a lookup queued behind it.

- **A history of the last recorded revision alone, its units new, as the pending view.** It
  gave pending units real-looking states that counted and folded as review state does, and a
  `Snapshot` whose view reached the writer: its empty log prefix matched every log, and
  recording a mark on a recorded revision checks only that it is the last one. A list of
  actions the pending view refused had to name every action that could reach a write.
- **A persisted summary of the standings, read at opening.** It would have to be invalidated by
  the exact log prefix and both proof formats, and a format bump would invalidate it exactly
  when opening needs it.

- **Refusing review keys on a hunk that changed in the last capture,** for a second and while a
  key pressed before the change keeps repeating, **and every mark while git is busy in the
  checkout.** Both guess at time, not at what showed: with coding agents editing and running
  git in the checkout, they kept refusing Space on code the reviewer had read, and the next
  capture's status line replaced the refusal before it could be read.

## References

- `crates/review-ui/src/model/live.rs` (`offer`, `publish`, `publishes`, `live_refusal`)
- `crates/review-ui/src/model/mod.rs` (`Model::offer_opened`, `Model::show_opened`,
  `Model::sidebar_shown`)
- `crates/review-ui/src/model/pending.rs` (`Pending`, `Progress`, `replay`, `confirm`,
  `Model::confirm_opening`, `Model::show_arrived`), `crates/review-ui/src/model/pulls.rs`
  (`Pulls::settled`, `Pulls::unknown`),
  `crates/review-ui/src/model/marks.rs` (`review_target`), `crates/review-ui/src/rows/mod.rs`
  (`Source`)
- `crates/review-core/src/review/mod.rs` (`Review::last_recorded`, `Review::cached_analysis`,
  `Review::analyze_recorded`, `Review::changed_files`, `Review::history_reporting`),
  `crates/review-core/src/view.rs` (`Changes`)
- `crates/review-ui/src/paint/bars.rs` (`bar_hints`, `checking_label`),
  `crates/review-ui/src/paint/header.rs` (`no_file`)
- `crates/review-tui/src/app.rs` (`App::render`), `crates/review-tui/tests/terminal.rs`
- `crates/review-gui/src/app/mod.rs` (`publish_with_next_frame`)
