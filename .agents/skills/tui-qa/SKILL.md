---
name: tui-qa
description: Drive the review-tui terminal app off screen and in a real terminal under tmux, read its screens, and hunt UX and functional issues against the project's quality bar. Use when building, changing, reviewing, or QA-ing review-tui's behavior, layout, keybindings, scrolling, or rendering, or anything in review-ui (check review-gui too, with gui-qa), and before reporting that work as done.
---

# TUI QA

Every review-tui change is checked by driving the real app and reading its screen. Unit tests
and `state` JSON prove behavior; the screen proves what the reviewer actually sees. Do both,
and judge every screen critically against the quality bar below. Never report UI work as done
from code alone. The desktop app has its own skill, `gui-qa`; a change to review-ui is checked
with both.

## Off screen: QA scripts

`review-tui --qa` reads the same line protocol as review-gui's QA mode on standard input and
draws into an off-screen buffer instead of a terminal. `scripts/qa/run.sh` plays a script of
`launch`, `shot`, `expect <regex>` and protocol lines against it:

```sh
task qa:tui -- scripts/qa/smoke-tui.qa    # the regression script
task qa:tui -- target/qa/tui/probe.qa     # a script of your own for the change at hand
```

- Configuration, demo, logs and shots live under `target/qa/tui/`, apart from review-gui's and
  from the user's configuration.
- `shot <name>` writes the screen to `target/qa/tui/<name>.txt` and, with SGR colors, to
  `<name>.ansi`. Read the `.txt` to check the layout, and `cat` the `.ansi` in a terminal or
  read its escape sequences to check colors.
- Write a throwaway probe script for the feature you changed, run it, read every shot, edit and
  rerun. `include scripts/qa/smoke-common.qa` replays the shared walk when you need its state.

## Commands

The authoritative list is the module comment in `crates/review-tui/src/qa.rs`; read it when a
command here fails. Positions are terminal cells, starting on a 160×50 screen.

| Command | Effect |
|---|---|
| `key <keystrokes>` | Types keystrokes as a terminal sends them, through the same normalization real keys take: `key j`, `key shift-s`, `key z a`. |
| `action <name>` | Does what one of the model's actions does, as its key would. |
| `paste <json string>` | Pastes as a terminal's bracketed paste; a paste must never act as keys. |
| `click <column> <row> [count] [shift]` | Presses the left button on a cell, `count` times in a row. |
| `drag <column> <row> <column> <row>` | Presses on the first cell, moves a cell at a time, releases on the second. |
| `wheel <turns> [<column> <row>] [shift]` | Turns the wheel, three rows a turn, over the diff or at a cell. |
| `scroll <rows>` / `pan <cells>` | Scrolls the diff by rows or moves its code left by cells. |
| `file <path>` / `find <text>` | Shows a file's diff, or selects the hunk of the next row containing the text. |
| `resize <columns>x<rows>` | Resizes the screen. |
| `clipboard` / `primary` | What the app last copied (sent through OSC 52) or put in the primary selection, as JSON. |
| `state` | The observable state as one line of JSON, as review-gui's, with geometry in cells. |
| `rapid <command>` | Answers without waiting for the work the command starts. |
| `quit` | Quits as `q` does, once pending marks are saved. |

## In a real terminal: tmux

The off-screen buffer cannot show what a terminal does with the app's output: the alternate
screen, the cursor, raw mode, resize signals and the terminal's own colors. Check those in a
detached tmux server of your own:

```sh
tmux -L qa-tui new-session -d -x 160 -y 50 \
    'timeout --foreground 1800 target/debug/review-tui --demo'
tmux -L qa-tui send-keys j j Enter
tmux -L qa-tui capture-pane -e -p                     # the screen with its colors
tmux -L qa-tui display -p '#{alternate_on}'           # 1 while the app runs
tmux -L qa-tui resize-window -x 80 -y 24              # a small terminal
tmux -L qa-tui send-keys q
tmux -L qa-tui kill-server                            # always, before you report
```

- `timeout --foreground` bounds the app: a background process group cannot take the terminal.
- After it quits, check that `#{alternate_on}` is 0 and that the shell's `stty` settings are
  restored, also after `ctrl-z` and `fg` and after a `kill` from another shell.
- Diagnostics go to `$XDG_STATE_HOME/code-review/review-tui.log` and are printed again on exit.

## Regression script

`scripts/qa/smoke-tui.qa` includes `scripts/qa/smoke-common.qa`, the checks that read the same
in review-gui and review-tui because they do not depend on geometry. When a feature works, add
its steps and an `expect` to `smoke-common.qa` when both front ends answer alike, or else to
`smoke-tui.qa` (and `smoke.qa` for review-gui), so it stays working. Keep both smoke scripts
green at every report.

## QA pass: find the issues yourself

Act as a demanding reviewer using the app for the first time, and walk the whole flow, not only
the feature you changed:

1. Open the demo, read the overview (`i`), move through the files with `j`/`k`, and fold and
   unfold folders with `h`/`l` and Enter.
2. Open a diff with `0`, step through hunks with `j`/`k`, approve with Space, flag with `!`,
   reset with `u`, and approve a whole file and a folder from the tree. Check that the glyphs,
   counts, progress bar, folding and the bottom bar react at once and correctly.
3. Race input against saving with `rapid` lines (approve, then leave the file at once) and check
   that the mark lands on exactly the hunk approved and the screen shows the latest selection.
4. Open the whole file with `=`, use the vim motions and `/` with `n`/`N`; expand context and
   follow a move. Cycle the diff styles with `|`, toggle side by side with `s` and pan it, and
   open the settings list with `T`: preview themes, styles and densities, and check every
   theme's contrast in every style.
5. Select text with the mouse, copy it, and check `clipboard`; paste into the `/` bars.
6. Scroll with the wheel and with the keys, and drag the divider.
7. Relaunch with `--demo-revision 2` on the same data and check that carried marks, changed
   since reviewed with interdiffs, new hunks and notes are all right.
8. Judge the screens at 160×50, 120×40 and 80×24, and narrower than side by side allows,
   looking for clipping, overlap, wrapping, misaligned columns and cut-off wide characters.

On every screen, check against the quality bar (PLAN.md, "Product direction"), within what a
terminal can show:

- **Scannability:** code reads as an uninterrupted grid; annotations (hunk headers, gaps, notes,
  tags) sit on their own rows, muted, and never look like code; the selected hunk and the
  bottom bar's target are obvious; scope breadcrumbs are present.
- **Polish:** the theme's palette, unchanged code always syntax-colored, consistent spacing,
  aligned columns, no flicker or layout shift, a visible cursor and focus, and designed empty,
  loading and error states.
- **Correctness:** the diff colors match the configured style; moves, formatting-only and
  generated files are labeled; line numbers and ranges are right; nothing makes unreviewed code
  look reviewed.
- **Terminal hygiene:** the app leaves the terminal as it found it, survives resizes and job
  control, and never lets a paste or an escape sequence act as a command.

Fix every issue you find, re-run the same steps, and read the screens again. Report what you
found and fixed, and the 3-4 screens that best show the result.
