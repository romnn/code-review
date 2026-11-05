---
name: app-qa
description: Drive the review desktop app (review-lite, and the Zed fork once it has a QA mode) headlessly, take and read screenshots, and hunt UX and functional issues against the project's quality bar. Use when building, changing, reviewing, or QA-ing any front-end behavior, layout, keybinding, scrolling, or rendering, and before reporting UI work as done.
---

# App QA

Every UI change is checked by driving the real app and looking at it. Unit tests and `state` JSON
prove behavior; screenshots prove what the reviewer actually sees. Do both, and judge every
screenshot critically against the quality bar below. Never report UI work as done from code alone.

## Interactive session

`scripts/qa/session.sh` keeps the app running in a headless sway output between calls. It
renders like GNOME, with client-side decorations, at 1600x1000.

```sh
scripts/qa/session.sh start --demo          # builds, starts sway, launches the demo review
scripts/qa/session.sh cmd file src/engine.rs  # one command, prints its one-line answer
scripts/qa/session.sh cmd key 0             # into the diff
scripts/qa/session.sh cmd state             # JSON of the visible UI state
scripts/qa/session.sh shot 01-engine        # prints target/qa/01-engine.png
scripts/qa/session.sh log                   # the app's stderr, for panics and warnings
scripts/qa/session.sh stop                  # always, before you report
```

- Then read every screenshot with the Read tool, which displays the image, and look at it before
  you send the next command.
- The app arguments after `start` are those of `review-lite`. They include `--demo`,
  `--demo-revision N`, a `base..head` range, `--worktree`, `--font-size`, `--decorations client`
  and `-C <repo>`.
- `start` replaces a running session.
- The app keeps its settings in `target/qa/<session>-config.toml` and its demo in
  `target/qa/<session>-demo`, never the user's configuration or `target/demo`; `QA_SESSION=name`
  (default `session`) keeps an agent's session apart from others. Delete the
  configuration file for the defaults; it outlives restarts so remembered widths can be checked.
- Environment:
  - `QA_SIZE=1280x800` tests a smaller window.
  - `QA_BINARY=target/release/review-lite` tests a release build, which is the build for timing.
  - `QA_VULKAN=lavapipe` uses software rendering when the GPU path fails.
- Every process runs under `timeout` (`QA_TTL`, 3600 s by default). Still, run `stop` when you
  are done, and never wait on a process with `pgrep -f`.

## Commands

Each command answers only after its background work has finished and a frame has rendered, so a
screenshot taken right after the answer shows the result. The authoritative list is the module
comment in `crates/review-lite/src/app/qa.rs`; read it when a command here fails.

| Command | Effect |
|---|---|
| `key <keystrokes>` | Types through gpui's key dispatch, so the real bindings apply: `key j`, `key shift-t`, `key z a`, `key ctrl-d`. Several keystrokes are typed one after another, so `key b a l` types into the find bar. |
| `action <name>` | Dispatches an action by name, such as `action review_lite::NextHunk`. |
| `rapid <command>` | Runs a command and answers at once, without waiting for background work or a frame; a few `rapid` lines in a row race input against saving marks and loading. `rapid state` prints the state right away. |
| `repeat <count> <ms> <keystroke>` | Types a keystroke `count` times, one every `ms`, as a held key repeats, then prints JSON with each keystroke's handling time and the input-to-frame latency percentiles over the burst; `repeat 25 33 j` in the tree measures holding `j`. Use `QA_BINARY=target/release/review-lite` for release numbers. |
| `file <path>` | Moves the files cursor to a file and shows its diff, as `j`/`k` would. |
| `click <x> <y> [count] [ctrl] [shift] [right]` | Mouse click at window coordinates; `count` 2 or 3 makes a double or triple click. |
| `drag <x1> <y1> <x2> <y2>` | Press, render a frame, move in steps, release; use it for selection, pane dividers and the title bar. |
| `hover <x> <y>` | Moves the pointer there and waits long enough for a tooltip; take a `shot` to see it. |
| `hit <x> <y>` | Names the innermost marked region there and its pointer: `close`, `maximize`, `minimize`, `title-bar`, `sidebar-divider`, `source-divider`, `tree-header`. |
| `wheel <dy>` / `scroll <px>` | Wheel input over the hunk list, or a direct scroll. |
| `sample` | The scroll offset right now, without waiting; call it several times during an animation. |
| `find <text>` | Selects the hunk of the next row containing the text. |
| `resize <W>x<H>` | Resizes the window. |
| `clipboard` | Prints the clipboard as a JSON string; use it after selecting and copying. |
| `latency` | Input-to-frame latency percentiles. |
| `state` | The observable UI state as one line of JSON: `mode` (Tree, Diff, Buffer), `area` (which key context has the keyboard), `screen`, `selected`, `hunk`, `hunk_units`, `hunk_badges`, `pending` and `queued` marks, `revision` and `newer`, visible `rows` with their `kind`, `hunk`, `tag` and `scope`, `tree` rows with change kind and `progress`, `buffer` (cursor row and column), `find` (its `target` mode, query, matches, current match and its `current_column`), `filter` (the tree's `/` filter), `picker` (kind, scope, query, `editing`, `count`, the first entries), `shown` (the file whose diff rows show) and `preview` (a held-key preview awaiting its diff), `layout`, `window_events` (title-bar moves, zooms, minimizes, menus), and `view` (`overlay`, `token_emphasis`, `diff_style`, `theme`, `density`, `layout` and the `effective_` ones that show, `previewing`, and the open `settings` list with its cursor and panel `geometry`). |

Find click coordinates by looking at a screenshot: the image's pixels are window coordinates,
because the window fills the output.

## Regression script

`task qa -- scripts/qa/smoke.qa`, which runs `scripts/qa/run.sh`, plays a checked-in script with
`launch`, `shot` and `expect <regex>` lines and fails on the first mismatch. When a feature works
in a session, add its steps and an `expect` to `smoke.qa`, so it stays working. Keep the smoke
script green at every report.

## QA pass: find the issues yourself

Act as a demanding reviewer using the app for the first time, and walk the whole flow, not only
the feature you changed:

1. Open the demo, read the overview (`i`), move through the files with `j`/`k`, and fold and
   unfold folders with `h`/`l` and Enter.
2. Open a diff with `0`, step through hunks with `j`/`k`, approve with Space, flag with `!`,
   reset with `u`, and approve a whole file and a folder from the tree. Check that the glyphs,
   counts, progress bar, folding and the bottom bar react at once and correctly.
3. Race input against saving with `rapid` lines (approve, then leave the file at once) and check
   that the mark lands on exactly the hunk approved and the view shows the latest selection.
4. Open the whole file with `=`, use the vim motions, `/` with `n`/`N`, and `gd` on a trusted
   and on an untrusted review; expand context and follow a move. Cycle the diff styles with `|`
   (plain, bold, soft), toggle side by side with `s` and pan it with `pan <px>`, and open the
   settings list with `T`: preview themes and presets with `j`/`k`, search with Tab, apply with
   Space, and check every theme's contrast in every style.
5. Select text with the mouse, copy it, and check `clipboard`.
6. Scroll with the wheel and with the keys, sampling during the animation, and check `latency`.
7. Run `start --demo-revision 2` on the same data and check that carried marks, changed since
   reviewed with interdiffs, new hunks and notes are all right.
8. Judge `src/engine.rs` at 1600x1000 and 1280x800 (`QA_SIZE`). Resize to 1280x800 and 2560x1440, and change the font size, looking for clipping, overlap,
   wrapping and misaligned columns.

On every screenshot, check against the quality bar (PLAN.md, "Product direction"):

- **Scannability:** code reads as an uninterrupted grid; annotations (hunk headers, gaps, notes,
  tags) sit on their own band, muted, and never look like code; the selected hunk and the
  bottom bar's target are obvious; scope breadcrumbs are present.
- **Polish:** a calm dark palette, monospace everywhere, unchanged code always syntax-colored, consistent spacing, aligned columns, tabular numbers, no layout
  shift, visible focus and hover, and designed empty, loading and error states. Window chrome
  shows close, minimize and maximize buttons and a title.
- **Correctness:** the diff colors match the configured style; moves, formatting-only and
  generated files are labeled; line numbers and ranges are right; nothing makes unreviewed code
  look reviewed.
- **Speed:** every answer comes quickly; the latency p99 is within one frame; the app never shows
  a frozen state.

Fix every issue you find, re-run the same steps, and take the screenshot again. Report what you
found and fixed, and the 3-4 screenshots that best show the result.

## Limits

- Title-bar dragging, double-click maximize and the window menu go through the compositor.
  Headless sway tiles windows, so check the wiring through `window_events` after `drag` and
  `click`, and say that the actual move needs a real GNOME session.

- Headless sway tiles windows, so it cannot show moving a window or a floating window's size.
  Verify those by checking that the gpui calls are wired up, and say so.
- Headless sway delivers a frame about every 40 ms, so measure frame timing with the release
  scroll benchmark on a real display, not here.
- The Zed fork speaks the same protocol through `CODE_REVIEW_QA=1`; drive it with
  `QA_SESSION=zed QA_BINARY=scripts/zed/launch.sh scripts/qa/session.sh start --demo` after
  `cargo build -p zed` in `~/dev/zed-review` (target dir `/mnt/ssd2/cargo-targets/zed-review`).
  `QA_SESSION` keeps it apart from a review-lite session. Its specifics, documented in
  `crates/code_review/src/qa.rs` of the fork:
  - `scroll` takes lines, not pixels, and `sample` reports `scroll_lines`.
  - `find` searches the buffer text; for text in detail blocks (interdiffs, moves, notes) it
    focuses the unit instead.
  - `wait <text>` waits for the active item; `definition <text>` repeats go-to-definition while
    rust-analyzer indexes, which takes a minute or two on the first launch.
  - The fork runs from its checkout (a debug build reads its assets from there) with its own
    profile under `target/zed-profile`, wiped of Zed's workspace database at each launch.
  - Headless sway gives Zed no GNOME button layout, so the title bar shows no window controls
    there; on GNOME Zed draws its own.
