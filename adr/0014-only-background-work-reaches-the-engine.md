# 14. Only background work reaches the engine

**Status:** accepted · **Date:** 2026-11-27

## Context

The UI thread of review-tui and review-gui only draws, maps input to model calls and runs
effects; it never blocks. Until now that held by habit: the model kept the review, the watch on
the working tree and the language servers in fields any model code could reach, so a blocking
call compiled on the UI thread, and several had crept in. Reading trust took the log's lock as a
capture showed, granting and revoking trust appended to the log in the key handler, a new head
stopped and waited for the language servers, and the watch and the servers waited for git,
threads and processes as they were dropped.

Trust guards running the reviewed code. A language server reads the workspace's build
configuration, which can name any program as a `rustc` wrapper, and with its builds on it runs
build scripts and proc macros, which can run anything and write anywhere the reviewer can. Its
writes were safe because they were synchronous: a revocation stopped the servers under the lock a
launch holds while it reads the stored grant. Moving trust off the UI thread must keep that: no
server starts on code the reviewer did not trust, and none keeps running once the window says a
revocation took effect.

Previews read and process whole files. A changed image is decoded and fitted, and a changed PDF
has each version's page text extracted and diffed and one page pair rendered, through pdfium, a
C++ library that is not thread-safe and is loaded only when needed. Each takes far longer than a
frame on a large file, and the reviewer keeps moving between files and pages meanwhile.

Background work is finite or it waits. The finite kind, such as opening and verifying the
review, analyses, texts, highlights, previews, captures and lookups of pull requests, runs
until it is done and competes for the machine's cores; after a change of the analysis format,
verifying a 100-revision review replays its history for a minute. The kind that waits, for the
working tree to change or a language server to answer, holds a thread for as long as that
takes. Saves and the engine's stop must never wait behind anything, since quitting waits for
them.

Where a server runs decides what it can do and what a grant means. It needs the code on disk.
The reviewer reviews the checkout, a branch's working tree against its base, with their own and
agents' edits arriving through the live sync; pull requests that are not checked out are not
reviewed in practice. rust-analyzer is useful only with its builds on, in the project's own build
state.

## Decision

- **One engine handle, private to background work.** `review_ui::model::engine` holds the review,
  the checkout's review that holds the trust in it, the watch and the language servers. The
  model can neither look into the handle nor clone it: all it can do is queue a job that runs
  with the engine's parts, through the existing effects, whose outcome comes back as plain data,
  or hand the handle to the job that stops it. The wait for the working tree to change holds only
  the watch's waiter, which holds the watch weakly, so no wait keeps the engine alive. The
  opening job assembles the handle. The title and paint read a plain copy of `ReviewMeta`.
- **Each job holds the review it reads.** A job holds the engine's parts from when it is queued
  until it ends, and with them the review, whose lease
  ([ADR 4](0004-a-working-tree-review-follows-the-disk-and-records-what-is-marked.md)) keeps
  every snapshot the job reads, such as a capture the window no longer shows, from being
  compacted away; nothing else pins what a window shows.
- **A front end can only start a job.** A job is opaque: its one public operation runs it in the
  background and hands its completion back, so no front end can run it in place on the UI
  thread. Both apps run every job that way, a wait for the working tree included, so there is
  one effect for jobs.
- **One pool runs the finite work, what the reviewer looks at first.** The app has one pool of
  worker threads, one per core and two at least, which starts a worker only when a job finds
  every worker busy. Each job names its lane as it is queued, and a worker takes the oldest job
  of the most urgent lane: `Selected`, what the reviewer looks at (the opened review, the
  analysis of the revision on screen, the selected file's text, highlights and preview, a
  picker's search); then `Shown`, the rest of what shows (the other files' texts, a few at a
  time and one read after another, their highlights, captures of the working tree and the
  watch's start, readings of trust); then `Background`, what checks or fetches behind it (the
  review's verification, fetches, lookups of pull requests), which never takes the last
  worker. So neither a replay of the whole history nor a slow `gh` keeps what shows waiting
  for a worker: at worst it waits for another job that shows to end. A job that waits outside the app (for
  the working tree or a language server), a save, the jobs that stop the engine or end the
  review in the window, and the watch on the grant, which must stop the servers within its
  interval, each run on a thread of their own, which nothing in the pool holds back.
  The pool cancels nothing; a job that a newer request may supersede checks for itself whether
  it is still wanted, as a picker's search does before it lists the tree and again before it
  matches. The selected file's text, as a preview, has one read outstanding at a time, which
  reads the blob the selected file wants as it starts, whatever a read of the other files'
  claimed: moving across files while it waits retargets it, and the selected file never waits
  for a read of others. It looks the blob's size up first, so an oversized one, such as a
  large asset's, holds it only for that lookup, never while git streams its bytes.
- **Nothing that blocks is dropped on the UI thread.** As the app quits, the job that stops the
  engine kills the servers and lets go of the watch, and quitting waits for it as it waits for
  saves. A job still running keeps the review, and a capture the watch, until it ends, so the
  stop never waits for it. The watch starts outside its slot's lock and is installed
  under it. The job that ends the review in a window whose log or clone was replaced and the
  job that stops the engine both close the slot for good under the same lock, so a watch that
  finishes starting after that is dropped by the job that started it. Its start, and each
  change it saw, lands only while the review goes on in the window and the engine is not
  stopping, so a late one arms no wait and no capture. Wherever the last holder of anything
  that blocks lets go of it, as when the model is dropped, a job never runs or a review opens
  after the app closed, it is dropped on a thread of its own.
- **The driver takes no input once the app closes.** From the moment the engine starts to stop,
  `Driver::update`, the one way a front end changes the model, runs nothing and returns nothing,
  so no key, click, scroll or layout acts and nothing it would change goes unsaved; no capture
  shows either. Jobs' and timers' completions keep landing through `Driver::complete`, which is
  how the engine's stop lands and the app quits.
- **Previews are jobs too, keyed by what they read.** One job reads both versions of a changed opaque
  file: a PDF's page text, paired and diffed, or else, where the front end shows images, both
  images decoded and fitted for their comparison. Another renders the one page pair the
  navigator selects into the image comparison's rasters, only while the reviewer shows the
  pages. Each is keyed by both blobs, a render by both page indices as well, and lands only
  while its key is still what the selected file shows, so a late result never shows under
  another file or page. One job of each kind runs at a time and one result of each is kept; the
  key shown when a job ends starts next. A job whose key the selected file no longer shows, as
  once the reviewer moved on while it waited for a worker, stops at its next boundary (before
  it reads, before it decodes or renders each version, and before each page's text) and lands
  nothing, so it lets go of pdfium, which every render waits for, as soon as it can; once
  told, it stays stopped even if its key shows again, which starts it anew. Every bound holds
  before the memory it guards is taken: a blob's size before it is read, a PDF's page count
  before any page and each page's characters before its text is copied. pdfium is loaded by
  the first PDF job, never at startup, and holds its one lock only while it reads or renders,
  never while the job diffs. A PDF the job cannot read, too large, encrypted or broken, or
  pdfium failing to load, shows as a binary file whose header says why.
- **Language servers run only in the checkout, on the code on disk.** A server runs in the
  checkout's working tree, its current directory and workspace root, with rust-analyzer's builds
  on and the project's own target directory. It is the one exception to cr writing nothing into
  the reviewed repository
  ([ADR 4](0004-a-working-tree-review-follows-the-disk-and-records-what-is-marked.md)): once the
  reviewer trusts the checkout, the server builds there, writing `target/` as the reviewer's own
  Cargo would; cr's own commands still write nothing there. Go to definition runs only where what
  shows is the code on disk: the newest revision of a review of the working tree, the checkout's
  review or one opened with `--worktree`. A review of a range or a pull request, or an older
  revision, says so before it asks for trust: check out the branch and review it there.
- **One grant per checkout and clone.** The checkout's review holds it, keyed by the working tree
  and the clone, as a `trust` event of scope `in_checkout` in its log. It covers whatever the
  checkout holds after later edits and branch switches, until it is revoked, and only while the
  checkout is the same clone. A `--worktree` review shares it: the opening job binds the
  checkout's review of the clone there then. The question asks afresh for running in place, and
  says that build scripts, proc macros and the workspace's configuration can run any program
  and write in the checkout. Decisions recorded for tools running in copies of the code, of the
  whole review, one snapshot or the checkout's copies, stay readable in the log and grant
  nothing.
- **A lookup checks the disk before the question and after the answer.** The lookup job compares
  the tree of the head the last frame showed with a capture of the working tree taken now, with
  every filter off, recording and pinning nothing, refused while git is busy
  (`Review::tree_on_disk`); the capture takes the path every capture takes, writing what it
  needs into an object database of its own that reads the archive of the checkout's review and
  goes with it, so it adds nothing to the archive and nothing to the checkout. Code that is not
  on disk is refused before any server is asked, and on an untrusted checkout before the trust
  question, so the reviewer never grants trust for a lookup that cannot run; an answer is
  dropped when the disk, the view or the trust changed while it ran. The file asked about must hold on disk the
  text on screen, so the cursor's line and column name the same bytes, which a file git keeps at
  its staged version under a filter, or one a sparse checkout leaves off disk, need not. A
  definition in a file of the snapshot on screen must hold on disk the text of its blob there, for
  the same reason; one elsewhere, such as in the standard library, opens as read from disk. It
  opens in the buffer on screen only when that buffer shows the same text.
- **A server outlives the captures and holds no text of the client's.** It keeps the index it
  built while the reviewer and agents edit. Each question opens the file with its text, which is
  its text on disk, and closes it once answered, so the server reads every file from disk again,
  through its own watch on the files, and never keeps text the file no longer has.
- **Trust shows as one state, `Checking`, `Denied` or `Granted`,** which is `Checking` while a
  decision is under way, and then nothing starts. While trust is read again, the last reading
  stays, a grant shown dimmed as no longer current; a lookup meanwhile relies on the owner's
  admission, which reads the stored grant itself.
- **One owner decides whether a language server may run.** The engine's `Servers` hold the one
  set of servers running in the checkout. Its one lock serializes the readings of trust, the
  decisions on trust and the admission of lookups. Wherever trust is found denied there, by a
  reading, a lookup, the watch on the grant or a revocation, every server stops in that same
  operation, before the window hears of it. A reading of a view that is not the checkout's code,
  such as an older revision, stops them the same way. The worker that finds the checkout replaced
  by another clone, a capture or a lookup, stops them there, and so does the job that ends the
  review in a window whose log or clone was replaced. A reading that no longer counts, of an
  older epoch, reads nothing and changes nothing. No job stops servers later, so none can stop a
  server started after it, except where the review ended and no lookup starts one any more.
- **Stopping a server asks it to exit, then kills its process group.** It sends a `shutdown`
  request and an `exit` notification and closes the server's input, and waits up to two seconds
  for the server to exit, so a server stops what it started itself, as rust-analyzer stops the
  commands it runs in process groups of their own. Then it kills the server's process group,
  which holds the server and what it started there and left running, such as Cargo, build
  scripts and proc macros, and reaps the server. Once it returns, the server and every process
  of its group are gone; a process the server started outside its group and did not stop itself
  is out of reach. The server is reaped only after its group is killed, so no other group that
  takes its id is ever signalled.
- **A decision on trust is a save,** queued with the marks in their one ordered queue, which
  quitting waits for; once the app waits to quit, it takes no new one. One is under way at most,
  since trust is being checked until it is recorded, and the save queue keeps the snapshot on
  screen meanwhile. Its job records it and reads the trust that applies after it in the owner.
  A revocation stops every server there, whether or not it was recorded, so the status line says
  the revocation took effect only once none runs, even while a lookup still holds one. A
  revocation that cannot be recorded stops a quit the reviewer waits for, as a failed mark does,
  and the owner keeps trust denied, whatever is stored, until a grant is recorded.
- **Every snapshot that shows has its trust read again by the owner.** A capture or `R` shows
  the new diff at once, with the trust last read until the reading lands.
- **While a server may run, the owner reads the grant every 30 seconds, whatever shows.** A
  lookup arms the watch, and each reading that finds a server running arms the next. It reads
  the grant and the clone through the checkout's review bound as the window opened, so it covers
  a `--worktree` review too, and runs whether the window follows the disk or not, an overlay
  holds captures back or git is busy. So a revocation in another window, or another clone at the
  checkout's path, stops this window's servers within 30 seconds and the time they take to exit.
- **A trust epoch orders what lands.** It moves on with every decision, every other snapshot
  that shows and every denial the window takes in, and a reading or a lookup's answer lands only
  in the epoch it was asked in, so an answer from before a revocation never lands, even after
  trust is granted again.
- **A lookup is admitted by its epoch and the stored grant.** Under the owner's lock, before it
  finds or launches a server, a lookup whose epoch no longer counts is refused before the stored
  grant is read, which may wait for another writer; then the grant is read, the clone checked,
  and the epoch asked for again. A lookup of an epoch in which the owner found trust denied is
  refused as denied. A launched server is in the set before the lock is released; the handshake
  and the request run outside it. Once the server answers, the stored grant is read again: gone,
  as when another window revoked it meanwhile, it stops the servers as a denial found anywhere
  does, and the answer is dropped.

## Consequences

- No code on the UI thread compiles that blocks on the engine or runs a job in place; the module
  boundary and the opaque job are the checks, the latter with a `compile_fail` doctest.
- Quitting waits for one more job, which stops the servers and the watch.
- A job queued on a capture keeps the review open until it ends, so another open neither
  compacts the archive nor drops the review meanwhile.
- A changed binary file is read once, to learn whether it is a PDF and, if not, to decode it,
  and a PDF's text diff needs no image support, so terminals without graphics show it too. Stepping through pages renders each pair only when it shows; nothing is prefetched or
  cached beyond the last result.
- review-gui runs jobs in the model's pool as review-tui does, rather than on gpui's background
  executor; the pool bounds how many finite jobs run at once to the cores, and highlighting
  bounds its own share of them.
- On a machine with fewer cores than the work, the selected file's text, preview and
  highlights run before queued texts of other files, captures and the review's verification,
  and one worker is always left for them; on one with more, the pool changes nothing, since
  every job finds a worker at once.
- Stepping past PDFs or pages faster than they read or render reads and renders only what
  shows, at the cost of reading again a file left and come back to before its read ended.
- A front end changes the model only through `Driver::update`, whose value is `None` once the
  app closes, and hands it completions only through `Driver::complete`.
- The bottom bar keeps the trust last read for the moment a reading takes after every capture,
  a grant dimmed, so its width never changes; it says `checking trust` only while a decision is
  recorded.
- `Ctrl+T` asks the trust question or answers at once with "Revoking trust…", and says the
  outcome once the job confirms it; `g d` and `Ctrl+T` say trust is being checked while it is.
- Go to definition is unavailable for pull requests that are not checked out, ranges and older
  revisions; their reviews read the code from git objects as before. Nothing is copied: no data
  directory grows by a copy of the sources per snapshot, and the read-only copies older versions
  left go when an open has their review to itself, and as the demo rebuilds
  ([ADR 4](0004-a-working-tree-review-follows-the-disk-and-records-what-is-marked.md)).
- rust-analyzer builds in the checkout's target directory, so its builds share the project's
  build state and its lock with the reviewer's own Cargo runs.
- A lookup reads the working tree's tree twice, each as long as a capture's look at it takes,
  and while captures keep showing, as while an agent edits, an answer for a snapshot no longer
  on screen is dropped; the server keeps its index for the next question.
- The server reads the files it is not asked about through its own watch on them, so an answer
  within moments of an edit of another file may still be about that file as it was; the window
  shows the edit only once its capture lands, which takes longer.
- A revocation, a quit or a replaced clone waits up to two seconds for each server that ignores
  the request to exit, under the owner's lock.
- A failed revocation leaves the grant stored, so the checkout is trusted again when the review
  opens anew; the status line says so.
- Out of reach still: another window's revocation landing within milliseconds of this window's
  launch, which only the stored grant checked under the lock catches; a process a server started
  outside its process group and did not stop when asked to exit; and the terminal's frames and
  clipboard writes on review-tui's main loop.
- A lookup that finds trust denied while the reviewer moved to another buffer stops the servers,
  but its answer does not land, so the window shows the denial once trust is next read or
  another lookup finds it.

## Rejected alternatives

- **Language servers in read-only copies of each reviewed snapshot.** They worked for any review,
  pull requests and older revisions included, but each copied the sources of a snapshot tree into
  the review's data and kept it forever, read-only copies broke plain deletion, rust-analyzer had
  to run with builds off and index from scratch so as not to build in them, and a grant per
  snapshot or per pull request promised a boundary the server, which reads build inputs outside
  the tree, could not keep. Most of the races trust and the servers had to close lived on those
  paths, for reviews the reviewer does not do.
- **Restarting the servers whenever the code changes.** It would never answer about stale text,
  but rust-analyzer indexing a large workspace again after every edit is unusable; opening only
  the file asked about, for the question alone, leaves every other file to the server's reading
  of the disk at no such cost.
- **Keeping the files asked about open, and sending each again when it changed.** The server
  treats an open file's text as the client's and ignores the disk for it, so a file asked about
  earlier and edited since answered with its old text when another file's question led into it.
- **A background owner thread per window, taking data-only commands.** It would cover races
  between windows and reap servers at exit, at several times the size: a command enum, its
  replies and a close handshake, for races that only an asynchronous rewrite introduces.
- **Holding the review's lock across a server's spawn.** It would serialize launches across
  windows, but a lock held across a process spawn in review-core stalls every other writer of
  the review, marks included.
- **A resettable flag that a revocation sets and a launch reads.** Whoever resets it can let a
  stale launch through; an epoch that only moves on cannot.
- **Retiring servers in a job of its own after a denial.** Retirement decided by a last-use epoch
  that reuse rewrites drew a race in two review rounds running: a process from before a denial
  survived it once a lookup reused it, and a late retirement stopped a server started after a new
  grant. Stopping the servers in the operation that finds the denial leaves nothing to run late.
- **A scheduler of dependent jobs, or an async runtime.** Each job is one closure that runs to
  its end; the few that follow another, such as the verification after the analysis it takes,
  are queued by the outcome of the first, so the pool needs no graph, and review-core stays
  synchronous.
- **Cancelling the history's verification between revisions.** Nothing supersedes it while
  the window shows the review: it runs once per opening, and quitting ends the process, so a
  check between revisions would never stop anything.
- **Prefetching the pages around the one shown, or caching renders across files.** Each render
  takes a moment the reviewer barely notices, while memory for every page of a large PDF would
  not be bounded by the one comparison shown.
- **A closed `Input` enum that every reviewer action goes through.** Gating the driver, already
  the only way a front end changes the model, covers every present and future input the same,
  without moving every input method behind an enum.

## References

- `crates/review-ui/src/model/engine.rs` (`Engine`, `Parts`, `WatchSlot`, `Servers`, `Waiting`)
- `crates/review-ui/src/model/effects.rs` (`Job`, `Driver`), `crates/review-ui/src/model/pool.rs`
  (`Pool`, `Lane`)
- `crates/review-ui/src/model/trust.rs`
- `crates/review-ui/src/model/buffer.rs` (`go_to_definition`, `look_up`, `find_definition`)
- `crates/review-ui/src/lsp.rs` (`LanguageServer`, `Stopper`)
- `crates/review-core/src/review/trust.rs` (`Review::trusted`, `Review::checkout_review`,
  `Review::tree_on_disk`)
- `crates/review-ui/src/model/quit.rs`
- `crates/review-ui/src/model/marks.rs` (`Save`)
- `crates/review-ui/src/model/compare.rs` (`Running`), `crates/review-ui/src/model/pages.rs`,
  `crates/review-ui/src/images/mod.rs`, `crates/review-ui/src/pdf.rs`
