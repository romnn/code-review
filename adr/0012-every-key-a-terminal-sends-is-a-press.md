# 12. Every key a terminal sends is a press

**Status:** accepted · **Date:** 2026-11-25

## Context

[ADR 11](0011-a-held-review-key-acts-only-on-what-its-press-began-on.md) makes a review key that
repeats act only on what its press began on, where the front end tells a repeat from a press.
A terminal inside tmux sends a held key's repeats as the same bytes as taps. The kitty keyboard
protocol's event types tell them apart where a terminal speaks them, with every key reported as
an escape code, since otherwise a key that types text still comes as plain bytes. The reviewer
works in plain tmux, so no protocol can be required.

Showing a changed image's versions in a terminal raises the same question for graphics: a
terminal speaks an image protocol or not, and the libraries that draw images ask it, and wait
for the answer, before they draw. Every answer arrives on standard input, which
crossterm's input thread reads; one it reads first becomes keys, and one that never comes holds
whatever waits for it.

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
- **Nothing asks the terminal whether it shows images either; crossterm stays the only reader
  of its input.** review-tui decides from what the terminal says without being asked: outside
  tmux, `TERM` is `xterm-kitty` or `xterm-ghostty`, which get the kitty graphics protocol's
  Unicode placeholders, or `TERM_PROGRAM` is `WezTerm`, which gets iTerm2's inline images;
  inside tmux, a call of `tmux display-message` on a thread of its own, killed after two
  seconds and repeated only when the app regains focus, which it asks the terminal to report,
  or is resized, reads the pane's client and settings and changes none of them, and images show only for the one client attached, running kitty or Ghostty
  with 24-bit color, in a pane whose `allow-passthrough` is already `on` or `all`.
  When another client attaches, its terminal gets every image afresh, or none. The cells' size
  in pixels comes from the system's window size, which asks the terminal nothing. Unknown,
  ambiguous or missing, the file shows its binary line. review-tui builds each protocol's
  images itself from `ratatui-image`'s kitty and iTerm2 backends, never through its picker,
  and every kitty image it sends asks for no answer (`q=2`).

## Consequences

- In a terminal, holding Space approves change after change at the key's repeat rate, as it
  stages hunk after hunk in lazygit. Space still never leaves the file by itself.
- `Model::key` takes whether the front end reports a repeat, a plain `bool`.
- Images show only in kitty, Ghostty and WezTerm, and in tmux only in kitty and Ghostty where
  passthrough is on; a terminal that names itself otherwise, WezTerm in tmux, or tmux run by
  more than one client shows a changed image as the binary line, as does a terminal that
  reports no pixel size. WezTerm draws kitty placeholders as missing glyphs, hence iTerm2's
  inline images there; passed through tmux, they land wherever tmux left the cursor.
- Images cannot use compression, which only a question would tell a terminal takes.
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
- **`ratatui-image`'s picker,** which asks the terminal for its graphics, cell size and colors.
  Its stdio query starts a reader of standard input beside crossterm's and changes the
  terminal's modes, and its deadline can pass while that reader still runs; even its
  constructors that ask nothing run `tmux set -p allow-passthrough on`, changing the
  reviewer's tmux. Its `StatefulImage` also resizes and encodes during a frame, on the main
  loop.
- **Turning tmux's passthrough on when it is off.** It is the reviewer's setting, and it lets
  any program in the pane write to the terminal behind tmux's back.

## References

- `crates/review-ui/src/model/guard.rs`
- `crates/review-tui/src/app.rs`
- `crates/review-tui/src/terminal.rs` (`take_over`), `crates/review-tui/tests/terminal.rs`
- `crates/review-tui/src/graphics.rs`, `crates/review-tui/src/images.rs`
- `crates/review-gui/src/app/keys.rs`
