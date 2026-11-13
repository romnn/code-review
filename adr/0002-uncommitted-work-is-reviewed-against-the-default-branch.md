# 2. Uncommitted work is reviewed against the default branch, under a branch-free key

**Status:** accepted · **Date:** 2026-11-13

## Context

A common way to start work is to edit on the default branch, run `git checkout -b <branch>` and
open a pull request only later.
Until then there is no pull request to review, and often no commit either, yet the hunks are
already there to be read and accepted.
Opening without a target used to fall through to `<default branch>..<branch>`, which leaves out
everything uncommitted and fails outright when the branch has no commit of its own.

Reviews are stored under a key, and marks belong to the key.
The question is which key: one that names the branch changes when the work moves with
`checkout -b`, and the reviewer would lose every mark made before the move.

## Decision

Opening without a target chooses, in order:

1. the review last opened for this checkout and branch;
2. the branch's open pull request;
3. on a branch whose working tree has uncommitted changes, the working tree against the default
   branch;
4. `<default branch>..<branch>`.

For rule 3:

- **Uncommitted means what a capture records**: staged and unstaged changes and untracked files
  that are not ignored.
  The check builds the same tree a working-tree capture builds and compares it with `HEAD`'s, so
  the two cannot disagree, whatever git's status settings say.
  Changes inside a submodule's checkout do not count, because a capture records only the
  submodule's commit.
- **The key names the checkout and the default branch, not the branch**:
  `<checkout>:<default branch>..WORKTREE`.
  Work begun on the default branch and moved with `git checkout -b` stays one review; rule 1 then
  ties the new branch to it once it is opened there, and it follows the branch after commits.
- **A detached `HEAD` never gets rule 3.**
  A working-tree review is trusted to run language servers and build tools, and a detached commit
  may be someone else's code.
  A bare repository has no working tree, so it never gets it either.
- **A key is never reopened for a different target.**
  A range ending at a branch named `WORKTREE` would share the working-tree key, so opening one
  over the other fails with `KeyConflict`, checked under `review.lock`.

Marks move between revisions only along unambiguous correspondences, as everywhere else, so a
working-tree review that changes under the reviewer never claims code they did not see.

## Consequences

- `cr` on a branch with only uncommitted work opens a review at once, with nothing to commit or
  push first.
- Work committed in full on a new branch before any review is opened there gets a range review of
  its own, whose units start unreviewed; the marks stay with the working-tree review.
- A remembered review wins over a pull request opened later; opening the pull request once by
  number or URL switches to it.
- An open working-tree review stays trusted if the same checkout is later switched to a detached
  commit, as an explicit `--worktree` review does.
  Recording per revision whether its snapshot came from a branch would close this.
- A collision-proof key format would make `KeyConflict` impossible by construction, but needs a
  migration of stored keys; the runtime check is enough for a branch literally named `WORKTREE`.
- Captures keep the copied index's modification time.
  Git rereads a file only when it is as new as the index ("racily clean"), so a copy dated now
  made a same-size edit from the index's own second invisible to the capture.

## References

- `crates/review-core/src/review/mod.rs` (`ReviewTarget::for_current_branch`, `Detected`,
  `Review::open`)
- `crates/review-core/src/git/snapshot.rs` (`has_uncommitted_changes`, `capture_worktree`,
  `copy_index`)
- `crates/review-core/tests/acceptance/current_branch.rs`
- PLAN.md, "CLI (MVP)", the rules for opening without a target
