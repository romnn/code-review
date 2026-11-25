# 12. Every key a terminal sends is a press

**Status:** accepted · **Date:** 2026-11-24

## Context

[ADR 11](0011-a-held-review-key-acts-only-on-what-its-press-began-on.md) makes a review key that
repeats act only on what its press began on, where the front end tells a repeat from a press.
A terminal inside tmux sends a held key's repeats as the same bytes as taps. The kitty keyboard
protocol's event types tell them apart where a terminal speaks them, with every key reported as
an escape code, since otherwise a key that types text still comes as plain bytes. The reviewer
works in plain tmux, so no protocol can be required.

## Decision

- **Where the front end cannot tell a repeat from a press, every key is a press and acts,**
  as in lazygit. No timing guess refuses a key.
- **A terminal keyboard protocol that reports repeats would give review-tui ADR 11's guard,**
  as an enhancement; nothing depends on it.
- **review-tui asks every terminal for the kitty keyboard protocol's disambiguation without
  asking whether it speaks it.** It pushes the protocol's flags as it takes the terminal over
  and pops them as it gives it back; a terminal without the protocol ignores both, and its keys
  come as it sends them, which the key handling takes either way. The app waits for no answer
  from the terminal, so none that never comes, or comes in part, holds up a key or a frame.
  The Linux console (`TERM=linux`) is the one terminal not asked: older kernels end the
  sequence at its `>` or `<` and print the rest, `1u`, on the screen.

## Consequences

- In a terminal, holding Space approves change after change at the key's repeat rate, as it
  stages hunk after hunk in lazygit. Space still never leaves the file by itself.
- `Model::key` takes whether the front end reports a repeat, a plain `bool`.
- A terminal that speaks the protocol sends keys such as Ctrl+I and Tab apart from the moment
  the terminal is taken over, before the first frame; Windows' console API, which crossterm uses
  there, takes no flags, so none are pushed there.

## Rejected alternatives

- **Telling a held key by timing:** keys of one review action closer together than half a
  second are one key held down. Reviewers tap faster than that: with four similar changes on
  screen, four quick taps of Space approved the first and refused the rest with "the key is held
  down", which lazygit, whose staging flow cr follows, never does.
- **Asking the terminal first, with crossterm's probe,** and pushing the flags only where it
  says yes. The probe waits two seconds for a terminal that answers neither of its questions,
  and without any deadline for the second answer once the first came, holding every key typed
  meanwhile, `q` included. Run while the terminal is in cooked mode, as when a signal gave it
  back first, it turns raw mode on itself, which the app exiting meanwhile leaves on. All it
  decides is whether to push flags that a terminal without the protocol ignores.
- **A probe of the app's own,** the main thread writing the questions and the input thread
  recognizing the answers within one deadline, without touching the terminal's modes. It would
  be safe, but nothing needs the answer.

## References

- `crates/review-ui/src/model/guard.rs`
- `crates/review-tui/src/app.rs`
- `crates/review-tui/src/terminal.rs` (`take_over`), `crates/review-tui/tests/terminal.rs`
- `crates/review-gui/src/app/keys.rs`
