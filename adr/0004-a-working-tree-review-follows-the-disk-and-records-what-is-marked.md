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
- A capture computes the whole analysis of the working tree against its base, about 0.2 s for a
  diff of 20,000 lines, and the history is rebuilt on each one; analyses already read stay in
  memory, so nothing is decoded twice.
- Go to definition on a capture checks out a worktree of that capture and starts its language
  servers there, as on any revision; a new capture stops the servers of the old one, so each
  capture used for browsing indexes again.
- Changes the watch cannot see show within 30 seconds: a ref moving without the index changing,
  a submodule's commit, `info/exclude` and `core.excludesFile`.
- Captures keep the copied index's modification time.
  Git rereads a file only when it is as new as the index ("racily clean"), so a copy dated now
  would make a same-size edit from the index's own second invisible to the capture.

## References

- `crates/review-core/src/review/live.rs` (`Review::live_history`, `Review::settle`)
- `crates/review-core/src/history.rs` (`History::is_provisional`, `History::version`)
- `crates/review-core/src/git/snapshot.rs` (`capture_worktree`, `copy_index`)
- `crates/review-ui/src/watch.rs` (`Watch`, `Waiter`)
- `crates/review-ui/src/model/live.rs`
- `crates/review-core/tests/acceptance/live.rs`
