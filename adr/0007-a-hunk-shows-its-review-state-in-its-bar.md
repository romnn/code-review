# 7. A hunk shows its review state in its bar

**Status:** accepted · **Date:** 2026-11-22

## Context

In the file view a hunk shows only by its lines and a bar along its left edge. Only the selected
hunk and hunks the reviewer had decided on had a bar, in the theme's accent for the selected open
hunk. A line shown once (`~`,
[ADR 6](0006-a-hunk-shows-its-code-once-with-the-tokens-that-changed.md)) is neither removed nor
added, so without a line color of its own a hunk of such lines looked the same whether approved
or not. Whether code still needed the reviewer was therefore hard to see, and so was which hunk
the keys act on once it was approved. The accent is blue in most themes and close to the green
of approved in several, such as Rosé Pine and Nightfox.

## Decision

- **Every hunk has a bar in its state's color,** as the files tree colors the same states: the
  theme's neutral text color while it still needs the reviewer, green once approved, red once
  flagged. The open color is neutral, so no theme's hues make open code read as approved. The
  selected hunk's bar is solid across the frame; the others' is narrower and quieter.
- **The selected hunk shows in the gutter and the band:** its line numbers are in the text color,
  the others' quiet, and once approved it sits on a brighter gray than the other approved hunks.
- **A line shown once has a soft line color until approved,** of the modified kind, neither red
  nor green and softer than a removed or added line's, so open code shows wherever it is.
- **Approved deleted text is struck out in gray,** not in red, so nothing in an approved hunk
  still reads as a change to review; the gray tint still marks a deleted space.
- **review-gui marks each open or flagged hunk on the diff's scrollbar track** in its state's
  color, so what is left of a long diff shows without scrolling to it.

## Consequences

- Open and approved code differ in every theme by brightness, not by hue alone. Rosé Pine and
  Rosé Pine Dawn give approved a teal, since their palette has no green; setting `done` in those
  theme files changes it.
- A terminal shows the bar as a whole cell in the selected hunk and half a cell in the others.

## References

- `crates/review-ui/src/paint/code.rs` (`Frame`, `ruler`, `gutter`, `marks_layer`)
- `crates/review-ui/src/paint/palette.rs` (`hunk_solid`, `selected_band`, `edited_line`)
- `crates/review-gui/src/app/render.rs` (the ruler)
- PLAN.md, "Views" and "Diff styles"
