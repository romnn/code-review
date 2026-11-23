# 8. Long lines wrap onto screen rows that own no text

**Status:** accepted · **Date:** 2026-11-23

## Context

The diff lays out each line of code as one row, and a line wider than its room is cut at the
right edge with a `…` and read by panning sideways. A file of prose, such as a prompt in TOML or
Markdown whose paragraphs are single lines of several hundred characters, then shows only the
start of each paragraph, and panning reads one paragraph at a time.

Wrapping makes how many rows a line takes depend on the diff's width. Selections, find matches,
copying and the reviewer's place all name text by row, so if a wrapped piece were a row of its
own, a resize would change what they mean.

## Decision

- **`w` wraps long lines of code onto as many rows as they need,** in the diff and in either
  layout, and `w` again cuts them as before. `[diff] wrap` remembers it, and the settings list
  offers it. The whole file as a buffer does not wrap, and keeps `w` as a word motion.
- **The rows stay what they were; screen rows are a projection of them.** `Layout::rows` keeps
  every line whole, and `Layout::screen` lays them out on the rows the list scrolls, each a piece
  of one row, one per row while lines do not wrap. Selections, find matches, copying and search
  name rows and bytes of their whole lines. Scrolling, hunk ranges, the ruler and pointer
  navigation count screen rows. A front end hands the model a screen row and a byte of the
  piece it drew, and `Model::diff_pos` turns that into a place in the text.
- **A width change only lays the screen rows out again,** keeping the same byte of the same line
  at the top, so a selection, the current match and the search's place survive a resize.
- **A line breaks after the last space that fits and otherwise as far as it fits, never inside a
  cluster,** so a tab, an escaped control character or an emoji is never split. A cluster wider
  than the room takes a row alone and keeps the clip marker. A piece after the first has no line
  number and a dim `↪` in the sign column. In a pair, a side whose line ended on an earlier
  piece shows only its background.
- **Wrapped code never pans;** the tool's own rows, such as a long note or a symlink's target,
  still pan as far as they run past their room.

## Consequences

- Every place that turns a scroll offset into a row goes through `Layout::row_on_screen` or
  `Layout::first_on_screen`, and every place that scrolls to a row goes through `Layout::starts`.
- A piece is a byte of one cell's text, so it carries over only where that text is known to be
  the same: rewrapping and changes of style, size or density keep it on the same rows, and `f`,
  opening a file from a folder's diff and a folder's rebuild keep it when the line found again
  shows the very same text, of the same file and on the same side. A change of layout, view,
  folding or policy, a file's remembered place and a live update show the line from its first
  piece.

## References

- `crates/review-ui/src/rows/wrap.rs` (`project`, `ScreenRow`, `WrapRoom`)
- `crates/review-ui/src/paint/display.rs` (`DisplayLine::wrap`)
- `crates/review-ui/src/model/layout.rs` (`wrap_room`, `follow_wrap`, `code_offset`)
- `crates/review-ui/src/model/select.rs` (`diff_pos`, `diff_shown`)
- `crates/review-ui/src/paint/code.rs` (`code_cell`)
- PLAN.md, "Views"
