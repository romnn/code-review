# 4. A working-tree review follows the disk, and records a capture only when it is marked

**Status:** accepted · **Date:** 2026-11-14

## Context

A review of a working tree, the checkout review
([ADR 5](0005-bare-cr-reviews-the-checkout-whatever-is-checked-out.md)) or one opened with
`--worktree`, is mostly opened while an agent or the reviewer still edits it.
Until now it showed the working tree as captured when it opened, and `R` recorded another
capture as a new revision.
Following the disk the way lazygit does means capturing it on every change.

Recording every capture would give an agent's hour of edits hundreds of revisions: a pinned
ref, an analysis file and a correspondence each, all reread whenever the history is read, and
an overview nobody can scan.
Most of those captures are on screen for seconds and never reviewed.

A watcher of the working tree has its own costs: a recursive inotify watch of a large
repository also watches `target/` and `node_modules/`, a hundred thousand directories in a
real monorepo, and every write a build makes there.

## Decision

- **The window follows the working tree.**
  A review of the working tree watches it and captures it when something a capture records
  changes, and shows each capture in place of the snapshot on screen, keeping the reviewer's
  file, hunk, top row and folds through the correspondence between the two.
  `[review] live = false` turns this off.
- **A capture is provisional until it is marked.**
  It shows as the revision after the last recorded one, but is not recorded: no event, no
  `snap/` pin, nothing in the caches.
  Marking it, or noting on it, records it first, under the same `review.lock`, and only when the
  log still ends at the revision it was compared with; otherwise the mark fails as on any
  revision that is no longer current.
  `R`, `cr open` and opening the review record captures as before.
- **Only recorded revisions take part in review state.**
  Marks carry from the last recorded revision to the capture along the correspondence between
  the two, as between any consecutive revisions.
  A change that went away and came back with no mark in between therefore reads as reviewed:
  an unchanged link requires the change byte-for-byte equal at a trusted position, so it is
  exactly a change the reviewer marked.
  Once a capture in between is marked, it is recorded, and the chain runs through it as before.
- **A capture derives only what changed.**
  The review keeps the history it last read, of the log up to just past its last complete
  record, and the last capture on top of it, by what was captured.
  With neither changed, a capture is that history again.
  A capture of other code keeps the recorded history and proves only its link from the last
  recorded revision.
  Records appended meanwhile, by this window or another window or agent, apply in log order,
  and review state is derived again from the earliest revision they add or mark.
  One reducer does all of it, so a history read again is the one read from the start.
  A history never changes once read, so a mark acts on exactly the history its frame showed.
  A capture reads the log before and after it looks: one during which the review
  recorded another revision is discarded, one during which a mark was saved shows it, and the
  window captures again when a capture was read before a mark it saved.
- **Another log in the review's place ends the review in the window.**
  The log only grows, so replacing it while a window is open, such as restoring a backup, is
  outside its contract; a supported restore happens with the review closed.
  A window notices one where it cheaply can: a log gone or shorter than what it read, another
  file where the platform tells files apart, or other bytes in the last 64 it read.
  A mark, an approval or a note, and the capture it records, is written only to a log that
  still starts with what the history it was made on was read from, checked under the
  exclusive `review.lock` before anything is written or truncated.
  Either ends the review in the window as a replaced clone does
  ([ADR 5](0005-bare-cr-reviews-the-checkout-whatever-is-checked-out.md)): it says to reopen
  the review, stops watching, captures and saves nothing more, and drops the marks not saved
  yet, saying so and stopping a quit that waited for them; every clone of the review then
  reads and writes the log no more.
- **A review key never acts on code the reviewer has not seen:** a capture shows only with the
  next frame ([ADR 13](0013-a-capture-shows-with-the-next-frame-and-keys-act-on-the-last.md)).
- **Only what git does not ignore is watched.**
  On Linux and the BSDs every directory git does not ignore gets a watch of its own, found by
  asking git about each level of the tree; on macOS and Windows one watch covers the tree and
  git's rules filter its events.
  Reads never count, so a capture never triggers the next.
  A capture every 30 seconds catches what a watch cannot see, and every 5 seconds replaces the
  watch when the system refuses one; every 5 minutes it first walks the tree again, catching a
  directory whose events were lost without a word.

## Consequences

- An edit shows within about a second: a burst of changes counts once it goes quiet for 150 ms,
  and at most a second after it began.
  Idle, a review costs no CPU beyond the periodic capture, about 0.13 s every 30 s on a
  checkout of 13,000 files, and its walk, about 0.5 s every 5 minutes; that checkout has 3,500
  watched directories of its 146,000.
- The log grows with what the reviewer marks, not with what the agent saves.
- Each shown capture is pinned by a ref of its own under `refs/code-review/<key>/live/`; a
  capture releases the pins of earlier ones its window no longer shows.
  Opening the review again releases the pins another window of the same review still uses,
  which costs that window only the pin, not the objects, which git keeps for two weeks.
- A capture of new code computes the whole analysis of the working tree against its base, about
  0.2 s for a diff of 20,000 lines, and its correspondence with the last recorded revision.
  On a generated review of 70 revisions and 3,654 records, a capture of nothing new spends
  under 0.2 ms of its 35–45 ms on the log and the history and the rest on git, one after a mark
  from another window under 1 ms, and one of an edit 8–14 ms on its link besides its analysis.
- Opening reads the whole log once and decodes every revision's analysis and correspondence,
  and a partial approval derives the history from the start under the lock.
- Reading trust after each capture reads the whole log.
- A mark, an approval or a note digests the log up to what its history was read from, under
  the lock, besides reading the whole log as it did.
- A log replaced in place by one as long, ending in the same 64 bytes, on a platform that does
  not tell files apart, still shows until a mark is attempted on it.
- Go to definition on a capture checks out a worktree of that capture and starts its language
  servers there, as on any revision; a new capture stops the servers of the old one, so each
  capture used for browsing indexes again.
- Changes the watch cannot see show within 30 seconds: a ref moving without the index changing,
  a submodule's commit, `info/exclude` and `core.excludesFile`.
- Captures keep the copied index's modification time.
  Git rereads a file only when it is as new as the index ("racily clean"), so a copy dated now
  would make a same-size edit from the index's own second invisible to the capture.

## Rejected alternatives

- **Reading a replaced log again from the start.** The window orders the histories it gets by
  the log's length, which compares only within one log: a history of another log as long
  passed for the one on screen, so approvals the log no longer holds stayed, and a slow read
  of the old log could put its history back after the new one was read. Each fix left another
  such case.

## References

- `crates/review-core/src/review/live.rs` (`Review::live_history`, `Review::settle`)
- `crates/review-core/src/review/memo.rs`, `crates/review-core/src/review/mod.rs`
  (`Review::history`)
- `crates/review-core/src/history.rs` (`History::extended`, `History::is_provisional`,
  `History::version`)
- `crates/review-core/src/git/snapshot.rs` (`capture_worktree`, `copy_index`)
- `crates/review-ui/src/watch.rs` (`Watch`, `Waiter`)
- `crates/review-ui/src/model/live.rs`
- `crates/review-core/tests/acceptance/live.rs`
