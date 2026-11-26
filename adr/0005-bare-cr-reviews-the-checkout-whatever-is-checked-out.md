# 5. Bare `cr` reviews the checkout, whatever is checked out

**Status:** accepted · **Date:** 2026-11-26

## Context

`cr` without a target has to open some review of the checkout, and the work in a checkout keeps
changing under it: edits, commits, branches, pull requests, renames and merges.
A review chosen once, when `cr` opens, cannot follow that (see "Rejected alternatives").

The reviewer wants one window that stays open for hours across many pull requests: it shows
uncommitted and committed work alike, knows the branch and its pull request, and empties once
the work has landed.

## Decision

- **Bare `cr` opens the checkout review, and nothing else.**
  There is one per working tree, keyed by its path and a clone id: a random id that cr keeps in
  its data directory, bound to the clone's common git directory by its resolved path and what
  the filesystem tells the directory apart by (device, inode and creation time; on Windows the
  volume and file index).
  Linked worktrees share their clone's id, and another clone has another.
  A clone made anew at the same path is another directory to the filesystem, so it gets a new
  id, and so a new review, which leaves the old one stored under its key; a running window
  that sees its clone replaced stops and says so.
  The clone is checked before every capture, save and use of trust, finding the checkout's
  common git directory anew each time, so a worktree path that another clone's worktree took
  is another clone even while the first clone is still there.
  Bindings are made one at a time, so of two first opens at once, the second takes the first's
  id.
  A path has one binding: another directory at a bound path gets a new id, which replaces the
  binding.
  An earlier version kept the id as a ref in the clone. Only the first binding of a path, one
  that was never bound, keeps that id, and only when a review of a checkout with it is stored,
  that review's checkout is still a worktree of this very clone, resolved read-only, and no
  binding holds the id yet.
  So a copy of the clone, which carries the ref too, gets an id of its own: at another path
  whichever is opened first, and with a worktree at the review's own path once the clone was
  bound; a clone restored at a path bound since never takes the old review back either. The
  ref itself is only read.
  Opening that review by its key binds the clone as opening the checkout does.
  Its history holds only captures of the disk, recorded when something is marked
  ([ADR 4](0004-a-working-tree-review-follows-the-disk-and-records-what-is-marked.md)) or when
  a command prints it.
- **Pull requests and ranges named explicitly are snapshot reviews:** fetched pushed heads,
  never live, with histories of their own.
  A pull request is fetched into a transient repository of cr's own that reads the clone's
  configuration as the clone reads it, running in the checkout, with explicit refspecs, `--no-write-fetch-head --refmap=`, and no tags,
  submodules or maintenance; its ends are imported into the review's archive and the rest of
  what it fetched goes with it
  ([ADR 4](0004-a-working-tree-review-follows-the-disk-and-records-what-is-marked.md)).
  Commands without `--review` act on the checkout review.
- **A key is never reopened for a different target.**
  A `--worktree` review is keyed `<checkout>:<base>..WORKTREE`, which a range ending at a branch
  named `WORKTREE` would share, so opening one over the other fails with `KeyConflict`, checked
  under `review.lock`.
- **The subject is observed on every capture, never stored as identity**: the branch, the real
  `HEAD`, the branch's pull request from a bounded background lookup, and the base.
  The base is the remote-tracking ref of an open pull request's base branch; else the remote
  default branch; else the local default branch; else, on an unborn or unrelated `HEAD`, the
  empty tree. The snapshot base is its merge base with the capture.
  Revisions record the branch, `HEAD` and pull request number as labels.
- **History stays linear.**
  Marks carry from each revision to the next along correspondences, so committing, `checkout -b`,
  renaming, pushing and opening a pull request keep every approval.
  Work on another branch shows new except an edit identical in bytes, position, surroundings and
  base file.
  Switching back to a branch after marking elsewhere reopens its units rather than resuming them.
- **Correspondence is stricter** (PLAN.md, "Carrying state across revisions"): a unit keeps its
  mark only when its file's base entry is unchanged and its context lines are equal; a file
  whose base and head entries are identical on both sides keeps every mark, repeated edits
  included.
- **A capture is taken only when git is not busy** (a lock, a merge, a rebase, a cherry-pick, a
  revert, a bisect or unmerged entries), and is discarded when `HEAD`, the base or the index
  moved while it ran. A capture the watcher saw change while it ran is never markable.
- **Nothing runs on the reviewer's behalf without a grant.**
  Every git command runs with hooks and fsmonitor off and writes nothing into the reviewed
  repository; captures run with filters off, and a filtered file keeps its staged version.
  No checkout is trusted by default. The checkout review holds one grant for its working tree
  and clone, which covers later edits and branch switches until revoked and which the status
  bar shows; a `--worktree` review shares the grant of the clone it opened in, and a named range
  or pull request, whose code is not on disk, has none
  ([ADR 14](0014-only-background-work-reaches-the-engine.md)).
- **CLI references name their review**: unit ids print as `u1-0000@<tag>`, and a mutation with a
  tag of another review, or a bare id without `--review`, is refused.

## Consequences

- One `cr` window follows clean `main`, edits, commits, branches, pull requests, renames and
  merges without restarting; after the work lands and `main` is pulled, it is empty.
- Unpushed commits on `main` stay in the diff until the remote-tracking ref has them, so a squash
  merged locally shows until fetched. Without a remote, a commit to the local default branch
  lands its work at once and leaves the diff.
- A reviewer who juggles branches in one checkout re-reviews what reopens on return; a git
  worktree per branch avoids that. Resuming a branch's own history needs an identity for a line
  of work that branch names and ancestry cannot give.
- Approvals of earlier reviews are not imported: the checkout review starts without marks, and
  earlier reviews stay openable with `--review`.
- A file renamed or edited in the base reopens its units with the reason, even when the edit
  itself is the same.
- A changed file under a filter, such as LFS, shows only once staged.
- A collision-proof key format would make `KeyConflict` impossible by construction, but needs a
  migration of stored keys; the runtime check is enough for a branch literally named `WORKTREE`.
- A clone moved to another path, or copied, is another clone.
  A filesystem that keeps no creation time may give a clone made anew the inode of the one it
  replaced, and FAT may give it the directory entry of the one it replaced; then the review,
  and the trust it holds, continue on it.
  Its snapshots are in the archive, and marks carry only along correspondences.
  A mount that changes the device number makes the same clone another, and so a new review,
  once the earlier version's ref is gone.
- The `refs/code-review/clone-id` an earlier version wrote stays in the clone until the
  reviewer removes it.

## Rejected alternatives

- **The clone id as a ref in the clone**, `refs/code-review/clone-id`, created once: every
  worktree saw it and a new clone lacked it, but making it wrote a blob and a ref into the
  reviewed repository, which cr no longer writes to.

- **Choosing a review once, when `cr` opens:** the review last opened for this checkout and
  branch, else the branch's open pull request, else, on a branch with uncommitted changes, the
  working tree against the default branch under a key that names no branch, else
  `<default branch>..<branch>`.
  A pull request review records only pushed heads, so a reviewer who approved every hunk of a
  pull request in its checkout and then edited it saw everything stay green, even after
  reopening.
  A clean default branch did not open at all, and nothing followed a branch switch, a new pull
  request, a rename or a merge without restarting `cr`.

## References

- `crates/review-core/src/review/mod.rs` (`Review::open_checkout`, `Review::open`,
  `Review::subject`)
- `crates/review-core/src/review/live.rs` (`Review::live_history`, `Capture`, sessions)
- `crates/review-core/src/correspond.rs` (the carry rules)
- `crates/review-core/src/review/clone.rs` (the clone id)
- `crates/review-core/src/git/` (the command boundary, filters, `Repo::busy`, `FetchRepo`)
- `crates/review-core/tests/acceptance/checkout.rs`
