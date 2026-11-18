# 5. Bare `cr` reviews the checkout, whatever is checked out

**Status:** accepted · **Date:** 2026-11-17 · supersedes
[ADR 2](0002-uncommitted-work-is-reviewed-against-the-default-branch.md) and widens
[ADR 4](0004-a-working-tree-review-follows-the-disk-and-records-what-is-marked.md)

## Context

ADR 2 chose a review once, when `cr` opened: the remembered review, else the branch's open pull
request, else the working tree, else a range.
A pull request review records only pushed heads, so a reviewer who approved every hunk of a pull
request in its checkout and then edited it saw everything stay green, even after reopening.
A clean default branch did not open at all, and nothing followed a branch switch, a new pull
request, a rename or a merge without restarting `cr`.

The reviewer wants one window that stays open for hours across many pull requests: it shows
uncommitted and committed work alike, knows the branch and its pull request, and empties once
the work has landed.

## Decision

- **Bare `cr` opens the checkout review, and nothing else.**
  There is one per working tree, keyed by its path and a clone id: a random blob that
  `refs/code-review/clone-id` points to.
  A clone replaced at the same path gets a new id, and so a new review; a running window that
  sees its clone replaced stops and says so.
  Its history holds only captures of the disk, recorded when something is marked (ADR 4) or when
  a command prints it.
- **Pull requests and ranges named explicitly are snapshot reviews**, as before: fetched pushed
  heads, never live, with histories of their own.
  The remembered review and the detection rules of ADR 2 are gone, so commands without
  `--review` act on the checkout review.
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
  Every git command runs with hooks and fsmonitor off; captures and the copies tools run in run
  with filters off, and a filtered file keeps its staged version.
  No review is trusted by default, a named range or `--worktree` included; for the checkout
  review a grant covers the exact snapshot, or the whole checkout until revoked, which the status
  bar shows, and for a range or `--worktree` review, whose key another clone at the same path
  shares, only the exact snapshot.
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

## References

- `crates/review-core/src/review/mod.rs` (`Review::open_checkout`, `Review::subject`)
- `crates/review-core/src/review/live.rs` (`Review::live_history`, `Capture`, sessions)
- `crates/review-core/src/correspond.rs` (the carry rules)
- `crates/review-core/src/git/` (the command boundary, filters, `Repo::busy`, the clone id)
- `crates/review-core/tests/acceptance/checkout.rs`
