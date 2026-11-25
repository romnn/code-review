# 14. Only background work reaches the engine

**Status:** accepted · **Date:** 2026-11-25

## Context

The UI thread of review-tui and review-gui only draws, maps input to model calls and runs
effects; it never blocks. Until now that held by habit: the model kept the review, the live
session, the watch on the working tree and the language servers in fields any model code could
reach, so a blocking call compiled on the UI thread, and several had crept in. Reading trust took
the log's lock as a capture showed, `R` on a range resolved a tree through git, granting and
revoking trust appended to the log in the key handler, a new head stopped and waited for the
language servers, a review of the working tree started its session, and the session, the watch
and the servers waited for git, threads and processes as they were dropped.

Trust guards running reviewed code. Its writes were safe because they were synchronous: a
revocation stopped the servers under the lock a launch holds while it reads the stored grant.
Moving trust off the UI thread must keep that: no server starts on code the reviewer did not
trust, and none keeps running once the window says a revocation took effect.

## Decision

- **One engine handle, private to background work.** `review_ui::model::engine` holds the review,
  the session, the watch and the language servers. The model can neither look into the handle
  nor clone it: all it can do is queue a job that runs with the engine's parts, through the
  existing effects, whose outcome comes back as plain data, or hand the handle to the job that
  stops it. The wait for the working tree to change holds only the watch's waiter, which holds
  the watch weakly, so no wait keeps the engine alive. The opening job assembles the handle,
  splitting the checkout's session off what its opening capture saw. The title and paint read a
  plain copy of `ReviewMeta`.
- **A front end can only start a job.** A job is opaque: its one public operation runs it on a
  thread of its own and hands its completion back, so no front end can run it in place on the UI
  thread. Both apps run every job that way, a wait for the working tree included, so there is
  one effect for jobs.
- **Nothing that blocks is dropped on the UI thread.** As the app quits, the job that stops the
  engine kills the servers and lets go of the watch and the session, and quitting waits for it
  as it waits for saves. A capture still running keeps the session and the watch until it ends,
  so the stop never waits for it. Wherever the last holder of anything that blocks lets go of
  it, as when the model is dropped, a job never runs or a review opens after the app closed, it
  is dropped on a thread of its own.
- **The driver takes no input once the app closes.** From the moment the engine starts to stop,
  `Driver::update`, the one way a front end changes the model, runs nothing and returns nothing,
  so no key, click, scroll or layout acts and nothing it would change goes unsaved; no capture
  shows either. Jobs' and timers' completions keep landing through `Driver::complete`, which is
  how the engine's stop lands and the app quits.
- **Trust shows as one state, `Checking`, `Denied` or `Granted` with its scope,** which is
  `Checking` while a reading or a decision is under way, and then nothing starts.
- **One owner decides whether a language server may run.** The engine's `Servers` hold one set
  of servers, for the current showing of a head, named by the generation of the snapshot shown
  when that head became current, so a head shown again is a showing of its own. Its one lock
  serializes the readings of trust, the decisions on trust and the admission of lookups, and
  stopping a server waits for its process to exit. Wherever trust is found denied there, by a
  reading, a lookup or a revocation, every server stops in that same operation, before the
  window hears of it. Another showing's reading replaces the set the same way, and a reading
  that no longer counts, of an older showing or epoch, reads nothing and changes nothing. No job stops servers later, so none can stop
  a server started after it.
- **A decision on trust is a save,** queued with the marks in their one ordered queue, which
  quitting waits for; once the app waits to quit, it takes no new one. One is under way at most,
  since trust is being checked until it is recorded, and the save queue keeps the snapshot on
  screen meanwhile. Its job records it and reads the trust that applies after it in the owner.
  A revocation stops every server there, whatever grant still applies and whether or not it was
  recorded, so the status line says the revocation took effect only once none runs, even while a
  lookup still holds one. A revocation that cannot be recorded stops a quit the reviewer waits
  for, as a failed mark does, and the owner keeps trust denied, whatever is stored, until a
  grant is recorded.
- **Every snapshot that shows has its trust read again by the owner.** A capture or `R` shows
  the new diff at once, with trust `Checking` until the reading lands. A pull request's review
  is trusted as a whole, as before.
- **A trust epoch orders what lands.** It moves on with every decision, every other snapshot
  that shows and every denial the window takes in, and a reading or a lookup's answer lands only
  in the epoch it was asked in, so an answer from before a revocation never lands, even after
  trust is granted again.
- **A lookup is admitted by its ticket and the stored grant.** A lookup carries a ticket: the
  showing and the epoch it started in. Under the owner's lock, before it finds or launches a
  server, a ticket that no longer counts is refused before the stored grant is read, which may
  wait for another writer; then the grant is read, and the epoch asked for again. A lookup of an
  epoch in which the owner found trust denied is refused as denied. A launched server is in the
  set before the lock is released; the handshake and the request run outside it. Once the
  server answers, the stored grant is read again: gone, as when another window revoked it
  meanwhile, it stops the servers as a denial found anywhere does, and the answer is dropped.

## Consequences

- No code on the UI thread compiles that blocks on the engine or runs a job in place; the module
  boundary and the opaque job are the checks, the latter with a `compile_fail` doctest.
- Quitting waits for one more job, which stops the servers and the session.
- review-gui runs jobs on threads of their own as review-tui does, rather than on gpui's
  background executor; the model bounds how many run at once, highlighting included.
- A front end changes the model only through `Driver::update`, whose value is `None` once the
  app closes, and hands it completions only through `Driver::complete`.
- The bottom bar says `checking trust` for the moment a reading takes after every capture.
- `Ctrl+T` answers at once with "Granting trust…" or "Revoking trust…", and says the outcome
  once the job confirms it; `g d` and `Ctrl+T` say trust is being checked while it is.
- A failed revocation leaves the grant stored, so the review is trusted again when it opens
  anew; the status line says so.
- Out of reach still: another window's revocation landing within milliseconds of this window's
  launch, which only the stored grant checked under the lock catches; the processes a server
  starts itself, which killing the server does not stop; and the terminal's frames and clipboard
  writes on review-tui's main loop.
- A lookup that finds trust denied while the reviewer moved to another buffer stops the servers,
  but its answer does not land, so the window shows the denial once trust is next read or
  another lookup finds it.

## Rejected alternatives

- **A background owner thread per window, taking data-only commands.** It would cover races
  between windows and reap servers at exit, at several times the size: a command enum, its
  replies and a close handshake, for races that only an asynchronous rewrite introduces.
- **Holding the review's lock across a server's spawn.** It would serialize launches across
  windows, but a lock held across a process spawn in review-core stalls every other writer of
  the review, marks included.
- **A resettable flag that a revocation sets and a launch reads.** Whoever resets it can let a
  stale launch through; an epoch that only moves on cannot.
- **Retiring servers in a job of its own after a head change or a denial.** Retirement decided
  by a last-use epoch that reuse rewrites drew a race in two review rounds running: a process
  from before a denial survived it once a lookup reused it, and a late retirement stopped a
  server started after a new grant. Stopping the servers in the operation that finds the denial
  or the new showing leaves nothing to run late.
- **A closed `Input` enum that every reviewer action goes through.** Gating the driver, already
  the only way a front end changes the model, covers every present and future input the same,
  without moving every input method behind an enum.

## References

- `crates/review-ui/src/model/engine.rs` (`Engine`, `Parts`, `Servers`, `Ticket`, `Waiting`)
- `crates/review-ui/src/model/effects.rs` (`Job`, `Driver`)
- `crates/review-ui/src/model/trust.rs`
- `crates/review-ui/src/model/buffer.rs` (`find_definition`)
- `crates/review-ui/src/model/quit.rs`
- `crates/review-ui/src/model/marks.rs` (`Save`)
