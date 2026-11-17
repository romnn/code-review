# 5. A checkout continues the review of its branch in its working tree

**Status:** accepted · **Date:** 2026-11-17 · supersedes rule 1 of
[ADR 2](0002-uncommitted-work-is-reviewed-against-the-default-branch.md) and the scope of
[ADR 4](0004-a-working-tree-review-follows-the-disk-and-records-what-is-marked.md)

## Context

A pull request review records only the pull request's pushed heads, and a range review only
what its range resolves to.
A reviewer who approved every hunk of a pull request in a checkout of its branch and then edited
it, or let an agent edit it, saw nothing of the edits: opening without a target reopened the pull
request review, which stayed green however often it was reopened.
Only a review of the working tree follows the disk (ADR 4), and a branch with a pull request
never gets one, because the remembered review wins (ADR 2).

Making the pull request review itself follow the checkout would mix two sources in one history:
fetched heads and captures of the disk.
A push the checkout lacks would record a fetched head between two captures, dropping every local
approval it does not hold and reopening them as new once the checkout catches up; a capture taken
against one fetch could be recorded after another; and a local revert of an unreviewed change
would make the pull request read as reviewed.

## Decision

- **The pull request or range review stays as it is.**
  Its history holds only what its target resolves to.
- **A checkout continues it in a review of its own working tree.**
  The continuation is a working-tree review, keyed by the checkout and a digest of the continued
  review's key, whose metadata names the continued review and revision.
  Its target is the continued revision's target commit, fixed, so its captures are compared from
  the same merge base until the branch merges the target.
  From there it is an ordinary working-tree review: it follows the disk, shows captures as
  provisional revisions, records them when they are marked, and carries marks along
  correspondences.
- **It is seeded once, all at once.**
  Its first revision is a copy of the continued revision, the same snapshot pair and so the same
  analysis, unit indices and evidence digests.
  Each unit reviewed there gets the same verdict; one reset, or changed since it was reviewed, is
  marked unseen, so it stays to review and no policy approves it; a new unit gets no mark.
  Notes attached to its units come along, and so does the continued review's trust, decided or
  by default, since a pull request checked out locally may be someone else's code.
  All of it is read from one reading of the continued review's log, pinned and cached first, and
  written as the whole log in one rename, so a crash leaves all of it or none.
- **Opening without a target continues where it can.**
  When the review chosen as before is a pull request or range review, and the checkout has a
  continuation of it, the continuation opens, whatever the checkout holds now, so a push or a
  rebase never strands its marks.
  Otherwise, when the checkout is on the reviewed branch, a local branch, with a `HEAD` that
  contains the head of the review's current revision, the continuation is created and opens.
  Otherwise the review itself opens, and says why the checkout is not followed.
  The continuation is then the remembered review, so the commands act on what the apps show.
  Naming the pull request or range always opens it.
- **A capture of a working tree with nothing beyond `HEAD` is `HEAD` itself**, so committed
  work deduplicates with a revision that recorded the commit.
- **`r` refreshes**, as `R` did.

## Consequences

- `cr` in a checkout of a reviewed branch shows uncommitted and unpushed work at once, keeping
  every approval whose hunk is unchanged; an edit shows within about a second (ADR 4).
- Marks made in the continuation stay there: the pull request review, opened by name, shows the
  pull request's own state. Carrying marks back when the pushed head matches a recorded capture
  is left for later.
- A branch that shares the pull request's head branch name and contains its head, but is not a
  checkout of it, is continued too. That costs no safety: approvals carry only to units equal at
  trusted positions to what the reviewer approved, and everything else shows unreviewed.
- The first `cr` after a pull request or range is first opened opens that review; the next one,
  once it has a revision, continues it.
- Marks do not yet survive reformatting between revisions: a link whose change differs only in
  formatting still reopens the unit, until correspondence can prove which unit survived.

## References

- `crates/review-core/src/review/continuation.rs` (`Review::continued_in`, the seed)
- `crates/review-core/src/review/mod.rs` (`Review::open_for_current_branch`, `Detected`,
  `Continues`)
- `crates/review-core/src/git/snapshot.rs` (`Repo::capture_worktree`)
- `crates/review-core/src/store/log.rs` (`publish_initial`)
- `crates/review-core/tests/acceptance/continuation.rs`
