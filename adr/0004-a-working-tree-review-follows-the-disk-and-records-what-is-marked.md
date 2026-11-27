# 4. A working-tree review follows the disk, and records a capture only when it is marked

**Status:** accepted · **Date:** 2026-11-26

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

Every capture writes objects: the blobs of changed files, their trees and a synthetic commit.
Written into the reviewed repository, with refs there to pin them, one real review of a branch
left 442 loose commits of cr's identity in it, 368 of them unpinned, and about 22 MB of trees
and blobs reachable only from them, before the reviewer marked anything; git also writes while
it reads an index, dating a split index's shared file anew and writing the trees of a sparse
index's directories as it expands them.
The reviewer expects cr to keep its own review state and never to change their repository.

What cr keeps of its own grows too, unless it decides what to let go.
A real review of 100 revisions moved into its archive as 233 MB in 3 packs without deltas,
its analysis cache held another 121 MB, a file per revision and per pair of revisions, and
older versions left read-only copies of reviewed files, finished reviews and what killed
processes left behind.

## Decision

- **cr writes nothing into the reviewed repository**: no object, ref, lock, index file or date.
  Its snapshots, their pins and its sessions live in the review's archive, a bare repository
  of cr's own under the review's data (`reviews/<key>/archive/`), and the clone's identity in
  the data directory too
  ([ADR 5](0005-bare-cr-reviews-the-checkout-whatever-is-checked-out.md)).
  The data directory must resolve outside the checkout and its git directories.
  The one exception is a language server the reviewer trusts, which builds in the checkout as
  the reviewer's own Cargo does
  ([ADR 14](0014-only-background-work-reaches-the-engine.md)).
- **The reviewed repository is only read.**
  A command that writes runs against a repository cr owns.
  Every git command drops each `GIT_*` variable cr inherited or an embedder set but those that
  say how git reaches a remote and, for the reviewed repository's own commands, which
  configuration files it reads; lazy fetches, replace objects, hooks, the filesystem monitor
  and external diffs are off.
  The reviewed repository's commands read no index: git reads one even to compare trees or
  look up attributes in a commit, and dates a split index's shared file anew as it does.
  Objects cross from the reviewed repository only by an explicit list in a pack of their own,
  never through an alternate, through which git dates an object anew whenever it writes one
  it finds there.
- **Private commands see the clone's configuration as the clone does.**
  The clone's effective configuration is read in the clone, includes and conditional includes
  evaluated there, and passed to every command in a private repository as command-scope
  configuration, with no global or system file read again where the private repository is.
  The keys that describe the clone's own repository, its includes, the hooks configuration
  defines, which `core.hooksPath` does not turn off, its filter drivers, which nothing cr runs
  may start, and the `core.fsyncObjectFiles` git warns about are left out; cr's own settings
  come after it, so they win.
  Every private command runs as `git -C` in the checkout, or the git directory of a bare clone,
  so a relative path in that configuration, a remote's or an attributes file's, is the same
  one, while cr starts git from where it runs.
  The archive takes the clone's `info/attributes` too, so files are classified and renames found
  as in the clone.
- **A capture works on private copies.**
  It works in a git directory of its own: a detached `HEAD`, copies of the index with its date
  and of the shared index a split one is based on, of the sparse-checkout patterns,
  `info/exclude` and `info/attributes`, and the archive as its only object database.
  The objects of `HEAD`, then every object the copied index names, are imported before any
  command could expand a sparse index: git drops a directory it cannot expand without a word,
  so the listing tells git to expect files outside the sparse patterns and fails on anything
  git reports.
  Then changed files are staged, the tree written and the synthetic commit made in the archive.
  The watch's ignore queries read such a copy too, one per walk of the tree.
  The subject, the index's date and size and git's busy state are read before and after, and a
  capture is discarded when they moved.
  A capture pins its base and head under its session before it returns them, deriving no
  history, so whoever builds on it holds snapshots that stay.
- **The archive holds every snapshot whole.**
  Each recorded or shown base and head is in it with every tree and blob it reaches, under its
  own object ID; a gitlink keeps its mode and ID only.
  A pack shows as a whole and a tree is written only once what it reaches is there, so a
  snapshot a reader gets is whole; the reviewed repository collecting or losing it afterwards
  changes nothing.
  Recorded snapshots are pinned by `refs/snap/<id>` and shown captures by their session's
  `refs/live/` in the archive.
  An object the reviewed repository lacks, as in a partial clone or after it collected one
  during an import, fails the capture or the recording, naming it.
- **The archive keeps every recorded snapshot until the review is dropped, and nothing else
  longer than something needs it.**
  Nothing expires: a recorded revision stays readable, so review state carries along every
  link of the history.
  What the log does not record goes once nothing uses it: captures a window showed, `HEAD`
  imported for a capture, and a `refs/snap/` pin a recording wrote before it died without
  appending its record.
- **Only an open that has the review to itself removes anything.**
  Each review has a lease, `locks/<digest>.lock` in the data directory, named for the name of
  its directory as the filesystem resolves it, the key encoded in ASCII, in lowercase, so a
  link to the directory and spellings of a key that a filesystem ignoring case takes for one
  directory share it; the file is never moved or removed.
  On a filesystem that tells case apart, two reviews whose keys differ only in case share one
  too, which only keeps a drop or maintenance of one waiting for the other.
  Everything that uses the review holds the lease shared for as long as it does: windows,
  commands, sessions, probes and blob readers, a window's background jobs among them.
  An open first tries to take it exclusively without waiting.
  When it can, nothing else uses the review, so the open removes, before it lets anything in,
  the pins of every session, what captures, probes, fetches and compactions left in the
  archive's `scratch/`, half-written packs, what drops left in `dropped/`, and the read-only
  copies older versions made; it compacts the archive when it is due, and lets go.
  It then starts over as an open that maintained nothing, under the lease held shared like
  any other user, so a drop in between makes it open the review anew, or fail to load it by
  its key.
  An open that finds the review in use leaves all of that for a later one: a review that is
  always open somewhere is not compacted until it is not.
  Maintenance follows no symbolic link out of the data directory, and compacts nothing while
  `scratch/` leads out of it.
- **Compaction stages a pack, and cr installs it.**
  `git pack-objects` packs everything the recorded snapshots reach, without their parents,
  with deltas, into a directory of its own under `scratch/`, as one pack whatever the clone's
  configuration says; it must print exactly one pack whose pack and index are both there,
  and the snapshots are checked to be whole in that pack alone.
  Only then does cr flush the pack and its index to disk, move them beside the packs they
  replace, over any file with their names, the index last, make the moves durable, through
  to the disk on Windows and by syncing the directory on Unix, and remove those packs and
  every loose object.
  Git deletes nothing in the archive, so a compaction killed at any point leaves every
  recorded snapshot readable: before the move the old packs hold everything, after it the new
  one does, and the next open that has the review to itself sweeps what was left.
  An open compacts an archive that no compaction finished yet, as after a move into it, whose
  packs have no deltas, and one that holds more loose objects or packs than git's own
  `gc.auto` and `gc.autoPackLimit` allow; while the data directory is over its limit, any
  archive with more than one pack or a loose object.
- **Probes add nothing to the archive.** Asking which tree is on disk, or what git ignores,
  imports and writes into an object database of the probe's own under `scratch/`, which reads
  the archive's as an alternate and goes with the probe.
- **The analysis cache keeps to a budget.**
  Every review of a data directory shares 256 MiB of analyses and correspondences, files whose
  modification time is when they were last written or read.
  Writing an entry and removing the files used least recently until the caches fit are one
  step under the data directory's `cache.lock`, so no entry escapes the count, and every open
  evicts too, whatever its maintenance does; an entry larger than the budget is never
  written.
  A file this version cannot read is removed when it is read.
  The cache of a review whose move into its archive is unfinished is the baseline the move
  compares the archive with, so nothing removes, rewrites or evicts it until the move is done,
  and the move derives that baseline as the versions before it did: with the links they
  cached, read as they wrote them, and with renames found as git's configuration has them.
  It compares that baseline with what the archive derives as a review will from then on,
  without the cache and with renames found within the fixed limit.
  A miss computes the entry again from the archive, and a failure to read, write or evict an
  entry fails nothing: a revision's analysis is published once its record is appended and the
  log's lock is free.
  A correspondence records the digests of the two analyses whose units it links and is used
  only with those very analyses, from the cache and from memory alike, so an analysis computed
  again with other unit numbers never takes a link of the one before; git finds the renames
  of a correspondence with a fixed limit, so a configuration edit never makes a cached link
  disagree with one computed again, while analyses keep git's configured rename detection, so
  one cached by an earlier version still equals one computed now.
- **A data directory retains at most 10 GiB beside its caches and transient directories.**
  Before anything that grows what it retains writes, cr sums the files of the data directory
  but the analysis caches and the archives' `scratch/`: before a capture imports anything,
  before a refresh imports a base and head pair that is not the last revision's, before a
  walkthrough is appended, and, under the log's lock, before a mark, an approval or a note
  records the capture it was made on.
  Over the limit, it refuses the addition, leaving the archive and the log as they were, and
  says to close every window of a review with space to reclaim and open it again, which
  compacts it, or to drop a review with `cr drop`; a running process never reclaims space
  itself.
  A mark, an approval or a note on a recorded revision is never refused, nor one whose capture
  the log turns out to record already.
  The check is soft: 10 GiB is where the next addition is refused, and an addition, or
  several at once, can go past it.
  Recorded revisions stay readable and markable at the limit: a window opens on its review's
  last recorded revision, and so does `cr show` on the checkout, saying why; a window whose
  capture or mark is refused keeps what it shows and says once that cr's data is full.
- **`cr drop <review>` removes a review by its key**, also once its checkout is gone.
  It takes the review's lease exclusively, or reports the review in use, and holds it while it
  moves the directory aside, to `dropped/` in the data directory, and removes it, read-only
  files included; what a removal cut short leaves, a later open removes.
  It refuses when a symbolic link leads `reviews/`, the review's directory or `dropped/` out of
  the data directory, and removes a link inside the review, never following it.
  An open waiting on a drop finds the review gone: loading it by its key fails, and opening its
  target makes a new one.
- **Ancestry comes from the reviewed repository, read only.**
  A synthetic commit's parent is the `HEAD` it was taken on, so merge bases are found among the
  reviewed repository's commits; only the first merge base is imported.
  Pull requests, and a shallow clone that lacks the history to find a merge base, are fetched
  into a transient repository that reads the clone's configuration
  ([ADR 5](0005-bare-cr-reviews-the-checkout-whatever-is-checked-out.md)).
- **Every snapshot the log records is in the archive**, which every open checks under
  `review.lock` by asking the archive for each snapshot's commit, importing any it lacks from
  the reviewed repository and verifying it whole, and by listing its pins, pinning any snapshot
  that has none; a snapshot whose commit is there but not all it reaches is copied whole
  again.
  So a review an earlier version kept moves into its archive on its first open, while that
  version's refs still pin its snapshots, and a revision such a version still running records
  later moves on the next open.
  The first move counts only once a replay of the log from the archive alone, with no cache,
  derives exactly the history the reviewed repository does with the review's cache, the one
  the reviewer saw: marks, partial approvals, notes, walkthroughs and standings included; then
  a marker records the cutover.
  What the archive's commands write, its objects, packs and refs, reaches the disk before git
  reports it written, so before the marker; commands that only read take none of those
  settings.
  Until then the log, its bytes, an unfinished record at its end included, and the reviewed
  repository stay as they were, and the next open tries again.
  The key, revision numbers, events and evidence digests never change.
- **The window follows the working tree.**
  A review of the working tree watches it and captures it when something a capture records
  changes, and shows each capture in place of the snapshot on screen, keeping the reviewer's
  file, hunk, top row and folds through the correspondence between the two.
  `[review] live = false` turns this off.
- **A capture is provisional until it is marked.**
  It shows as the revision after the last recorded one, but is not recorded: no event, no
  `refs/snap/` pin, nothing in the caches.
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
  A capture reads the log before and after it looks, deriving nothing before it, so it never
  waits on the history: one during which the review recorded another revision is discarded,
  one during which a mark was saved shows it, and the window captures again when a capture was
  read before a mark it saved.
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
- Each shown capture is pinned by refs of its own under `refs/live/<session>/` in the archive;
  a capture releases the pins of earlier ones its window no longer shows, and a session that
  died leaves its pins for the next open that has the review to itself.
- The archive holds the recorded snapshots and what windows show, in a pack per import until
  compaction is due: on a generated review of 105 revisions, each a commit editing 120 lines
  among files of 1,500, the archive's 105 packs of 133 MB compacted into one of 18 MB; the
  open that compacted it took 0.86 s, where an open with nothing due takes 22 ms.
  The window that open is for waits for the compaction; quitting never compacts.
  A compaction needs room for the new pack before the old ones go.
  It drops what windows showed along with what no pin reaches, so the first capture after it
  imports `HEAD` again.
- A review that is always open somewhere is not compacted until nothing has it open, and its
  archive grows by a pack per capture meanwhile.
- A capture imports `HEAD`'s objects once per commit; checking which objects of the index it
  lacks costs a few milliseconds on a checkout of 13,000 files.
- Each addition walks the data directory first: 9 ms for one of 336 MB in about 30,000 files,
  under a millisecond for twenty small reviews; on a data directory of 45 MB, a capture of a
  checkout of 1,500 files takes 114 ms, of which the walk is 0.5 ms.
  An eviction lists every review's cache, 3 ms for 2,000 entries, and runs after each entry
  written and on each open.
- Old correspondences are computed again once, since their format records digests now.
- A review whose clone sets `diff.renameLimit`, with a change past a thousand candidates for
  a rename, can find other links within the fixed limit than the version before it found;
  its move into the archive is then refused, leaving its log and its refs in the reviewed
  repository as they were.
- Once the data directory holds 10 GiB, captures, new revisions and walkthroughs are refused
  until the reviewer opens reviews with space to reclaim with nothing else having them open,
  or drops reviews; a window opens on its review's last recorded revision, and nothing
  expires on its own.
- Each review leaves an empty lease file in `locks/`, also once dropped, so a drop never races
  an open over which file is the lease.
- A pull request's refresh fetches both ends' whole history into a transient repository, which
  is removed once the snapshots are imported: a cold fetch each time.
- The first open after the upgrade replays a review's log from the archive without a cache,
  which takes as long as analyzing every revision once; every later open lists the archive's
  pins and asks it for the commits of the log's snapshots, two git commands of a few
  milliseconds.
  A snapshot whose commit survived a crash but whose trees did not, as in a torn pack, is not
  found then; the reviewer sees an error naming the missing object.
  The refs and session files an earlier version left in the reviewed repository stay; the
  reviewer removes them once every review moved.
  Windows of older versions are closed when upgrading: one still running records its revisions
  into the reviewed repository, which the next open of the review moves into the archive, and a
  window of this version open meanwhile fails on such a revision, naming the object it lacks,
  until it is opened again; no mark is lost, since the older version's refs still pin the
  objects.
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
  A partial approval digests the log's complete records once more, for the history it derives
  from them; no writer keeps the log's bytes past opening it.
- A log replaced in place by one as long, ending in the same 64 bytes, on a platform that does
  not tell files apart, still shows until a mark is attempted on it.
- Go to definition runs a language server in the checkout itself, which outlives the captures
  and indexes once: before each question it is told the file's text, and a lookup runs only
  while the capture on screen is the code on disk
  ([ADR 14](0014-only-background-work-reaches-the-engine.md)).
- A storage path whose `..` climbs out of a directory that does not exist yet is refused before
  anything is made, since where it leads cannot be checked.
- Changes the watch cannot see show within 30 seconds: a ref moving without the index changing,
  a submodule's commit, `info/exclude` and `core.excludesFile`.
- Captures keep the copied index's modification time.
  Git rereads a file only when it is as new as the index ("racily clean"), so a copy dated now
  would make a same-size edit from the index's own second invisible to the capture.
- A private repository reads the clone's configuration as it was when the review opened, and
  a capture as it is when the capture starts: a change to the clone's diff settings reaches
  the analysis of a running window only once it opens again.
- A capture runs three git commands more than one in the reviewed repository did: reading the
  configuration, listing the index's objects and checking which the archive lacks.
  On a generated checkout of 13,000 files a capture still takes about 0.12 s, the first one
  0.8 s while it imports `HEAD`; a walk of the tree takes 0.19 s instead of 0.16 s, and asking
  what git ignores after a change 15 ms instead of 2 ms, for the copy of the index.

## Rejected alternatives

- **Capturing into the reviewed repository**, with a temporary index in its git directory and
  pins under `refs/code-review/`: it piled objects into the reviewer's repository on every
  capture, left a temporary index behind when killed, and wrote refs and session files the
  reviewer never asked for.
- **The reviewed repository as an alternate of a private object database:** git dates an
  object it finds through an alternate anew whenever a command writes one equal to it, so
  every capture would still write to the reviewed repository; reading a split index there
  dates its shared file anew too.
- **Mirror clones, a cache database or deduplication across reviews:** one archive per review,
  git's packs and refs, the log and the existing locks suffice.
- **Expiring recorded snapshots older than the earliest one review state refers to.** A
  history with a gap needs an expired revision in replay, in the views and in every carry,
  for a saving a delta repack already makes: recorded snapshots share most objects.
- **`git repack -a -d` and `git prune` in the archive, with every pinned commit made a
  shallow root:** git then deletes what it replaces itself, so a git that outlives a killed
  cr races every later user of the archive, a drop and a review made anew at the same path
  included, and Windows can put a child in a job that ends with cr only once it runs.
  Staging the pack and installing it in cr leaves git nothing to delete.
- **Admitting additions exactly, with staged objects and counters of what each review
  holds:** a soft check at the point of each addition bounds the data directory as well, with
  no state of its own to repair after a crash.
- **A write lock that writers hold shared until what they wrote is pinned, and compaction
  exclusively:** every writer, probe, reader and job then needs a guard of its own, and each
  review found one missing; one lease held for as long as anything uses the review, with
  maintenance only when it is held alone, leaves no such gap.
- **Reclaiming space in a running process when an addition finds the data directory full:**
  it would compact archives other processes are using; at the limit cr refuses, and the next
  open that has a review to itself compacts it.
- **A lease file in the review's directory:** it moves with the directory, so a drop cannot
  hold it through the removal, and on Windows a held file keeps its directory from being
  renamed.
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
- `crates/review-core/src/git/private.rs` (`PrivateRepo`, `PrivateRepo::import`)
- `crates/review-core/src/git/mod.rs` (`Repo::clone_context`, `TRANSPORT_ENV`)
- `crates/review-core/src/git/snapshot.rs` (`PrivateRepo::capture_worktree`),
  `crates/review-core/src/git/scratch.rs` (`Scratch`), `crates/review-core/src/git/ignore.rs`
  (`IgnoreRules`)
- `crates/review-core/src/review/checkout.rs` (`Review::capture`)
- `crates/review-core/src/git/fetch.rs` (`FetchRepo`)
- `crates/review-core/src/review/archive.rs` (`Review::open_archive`)
- `crates/review-core/src/review/retention.rs` (`Held`, `maintain`, `Review::admit`,
  `Review::remove`), `crates/review-core/src/store/mod.rs` (`Lease`, `InUse`),
  `crates/review-core/src/review/cache.rs`, `crates/review-core/src/git/compact.rs`
  (`PrivateRepo::compact`)
- `crates/review-ui/src/watch.rs` (`Watch`, `Waiter`)
- `crates/review-ui/src/model/live.rs`
- `crates/review-core/tests/acceptance/live.rs`, `isolation.rs`, `migration.rs`
