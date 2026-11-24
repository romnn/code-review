# 6. A hunk shows its code once, with the tokens that changed

**Status:** accepted, its line colors amended by
[ADR 7](0007-a-hunk-shows-its-review-state-in-its-bar.md) · **Date:** 2026-11-21 ·
**Supersedes:** the non-goal "Tree-sitter proves formatting equivalence and highlights code; it
never shapes the diff" in PLAN.md, for the unified layout

## Context

A hunk shows its removed lines above its added lines, and marks the words that changed on lines
it pairs in order. A change that wraps code in a call, renames an identifier or re-indents a
block therefore shows every line it touched twice, though few tokens changed. This hunk from
agentmux takes twelve rows:

```diff
-    ThreadItem::Reasoning(Box::new(ReasoningThreadItem {
-        content: Some(Vec::new()),
-        id: id.to_owned(),
-        summary: Some(summary.iter().map(|&line| line.to_owned()).collect()),
-    }))
+    item(ThreadItemVariant::Reasoning(Box::new(
+        ReasoningThreadItem {
+            content: Some(Vec::new()),
+            id: id.to_owned(),
+            summary: Some(summary.iter().map(|&line| line.to_owned()).collect()),
+        },
+    )))
```

Aligned token by token, ignoring whitespace, it inserts `item(`, a `,` and a `)` and replaces
`ThreadItem` with `ThreadItemVariant`; everything else is the same code laid out anew. The engine
already computes that alignment for every chunk (`changed_token_ranges`, over the tree-sitter
leaves of the formatting proof's languages and generic tokens otherwise), and the formatting
proof already decides, gap by gap, when two layouts of the same tokens are equivalent. The view
keeps only highlighted ranges, which are off by default (`[diff] token_emphasis`), and shows
lines.

Move detection made it worse by matching the re-indented fields as a move nine lines away. Code
that may have stayed in its region of the file, between the same unchanged lines that mark a
place, is no longer part of a move, so this hunk has no move. That leaves in-place re-indents as
what they are, edits shown twice: the demo's `line_error` and `occupancy_line`, which a
module's closing brace moved past, used to collapse into one "moved" line and now show six and
thirteen lines removed and added again, unchanged but for their indentation.

A tree diff, such as difftastic's or GumTree's (see prior-work.md, "Diff engines"), would add
little to the display over a token alignment, chiefly which of two equal brackets is the new one,
and neither is usable as a library. What it could add later is a name for an edit, such as
"wrapped in `item(…)`".

## Decision

The unified layout shows a hunk as one copy of its code, aligned token by token, with what
changed marked in place. The engine computes the alignment, review-ui decides how to draw it,
and the front ends paint it.

- **The alignment is the engine's.** review-core keeps, for every chunk, its whitespace-blind
  token diff as an edit script (`Chunk::edits`): steps that are the same text, layout, changed
  tokens or a whitespace change, each with its byte spans on both sides, so every byte of both
  sides is accounted for in order. A whitespace change is a step of its own, never folded into
  the tokens beside it, so a Python dedent beside a rename is never read as the rename alone.
  The highlighted ranges kept before derive from it. Replacing one word by another stays a
  replacement in the script; review-ui marks only what differs, a word's added or lost part
  (`ThreadItem` to `ThreadItemVariant` reads as `Variant` added) or, in one line's code such as a
  comment or a string, the words that differ, whitespace included, and the whole change where the
  two share no word of letters or digits. The marks cover every byte a change replaced.
- **Layout is decided by the formatting proof, gap by gap.** A changed gap between two kept
  tokens (a line break, indentation, spacing) is layout, drawn as the new code has it and not
  marked, only when the proof's own rule for that gap says the two are equivalent: its wrap
  sites, its touching-token check (so `tokens!(+` and `-)` on two lines against `tokens!(+-)` is
  an edit), comments, literals, Python's indentation and Go's semicolon insertion, judged with
  both whole files, and only where no line ending changes kind. A run of whole lines inserted or
  deleted is one change, from its first line's indentation through its last line's break, when
  the line break before it and the indentation after it make up the other side's gap. A line
  break only the new side has, beside or inside changed tokens, shows as it is, so it is layout
  where the proof shows breaking the line there means nothing; one only the old side has shows as
  nothing, the tokens around it touching, so it is layout only where the proof shows they may
  touch too. Every other whitespace change is an edit and shows, as do a changed line ending and
  a missing final newline. In a language the proof does not support or a file with a parse
  error, no gap is layout.
- **What shows once, and what shows twice.** Lines go together where kept code joins an old line
  to a new one, or one change or whitespace change touches them, in groups no edit crosses. A
  group shows once, each new line in its new layout, numbered in the head file, with a `~` and no
  line color, when nothing that changed in it would hide: it has no whitespace change, and each
  deletion was made by one change on one old line that kept some of its code, has a place on one
  of the group's new lines, is at most 32 characters, and takes no more of its line than the code
  the line keeps; and every mark starts and ends between whole characters, so a deleted accent
  never joins the letter before it. Inserted tokens are marked behind a stronger green, and a
  deletion shows where it stood, struck out in red on a stronger red, which it keeps in an
  approved hunk, never copied, found, or painted over by a find match or a selection. Any other
  group shows its old lines above its new ones. A line no kept code joins shows as a removed or
  an added line of its own, as does a line of a group shown once that keeps no code and deletes
  nothing. In a hunk where a group shows both versions, unmarked code reads as kept, so there a
  line of its own marks all its code. The lines between two groups shown once, such groups and
  lines of their own, form one block, old above new, so a rewrite whose few kept words tie its
  lines into small groups reads as its old code and then its new code. The hunk above shows once, in seven rows.
- **The marks are the edit.** In the one-copy view the inserted and deleted marks are drawn
  whatever `[diff] token_emphasis` says; the option keeps governing word marks on old and new
  lines, which now are the same stronger shades, and is on by default.
- **Nothing changed is hidden.** Two checks hold: the edit script accounts for every byte of both
  sides in order, and, for every chunk of every demo revision, reading its rows back gives both
  sides byte for byte but the proven layout (leaving out what a line shown once inserted gives
  the old side, and leaving out what it deleted the new), while every whitespace change shows
  both versions.
- **One view for every comparison of two versions,** in steps. Hunks in the unified layout come
  first; a chunk that a move touches, and a formatting-only one, still show line for line. Then a move's edits and what changed since a review
  ([ADR 9](0009-a-hunk-changed-since-it-was-reviewed-shows-what-changed-on-top-of-it.md)) take the
  same script, between the move's source and target or the reviewed and the current version; they compare snippets, so
  without both whole files their whitespace is never layout. review-ui's own line pairing and
  word diff for moves (`rows/moves.rs`) give way to it. `SideDiff` keeps its lines, which
  `cr show --json` prints.
- **Side by side keeps both versions,** each column its own side's text, marked from the same
  alignment, each group's old lines beside its new ones. It is the layout for reading the old code whole, so the one-copy view has no setting
  of its own.
- **Review state does not change.** Units, marks, correspondences, formatting-only and noise
  classification, moves and hunk headers stay as they are; only how a hunk's lines are drawn
  changes.

## Consequences

- Wrapping, renaming and re-indenting read as what they are, in about half the rows. An in-place
  re-indent in Rust, such as the demo's `occupancy_line`, shows once, with the module's brace
  removed above it and added below it.
- Deleted code is read struck out inside a line where it is short, and as removed lines
  otherwise. Where "short" ends is tuned with screenshots of the demo and real pull requests.
- The analysis format changes (13), and cached analyses are recomputed. The edit script is
  larger than the highlighted ranges it replaces.
- A switch between unified and side by side finds the reader's line by its key, since the two
  build different rows.
- review-ui loses `rows/moves.rs`'s line and word alignment, engine logic in a front end, once
  moves take the script; until then the one-copy rows reuse its word alignment.
- The token diff breaks ties without regard to lines: deleting a statement next to an edited
  one that ends alike, such as two lines ending in `);`, can align the edit across both, and
  that group then shows its old lines above its new ones.
- Naming edits from a tree diff stays open.

## References

- `crates/review-core/src/analysis/edits.rs` (the edit script)
- `crates/review-core/src/analysis/build.rs` (`chunks`, each chunk's tokens and proof)
- `crates/review-core/src/syntax.rs` (leaf tokens, gap keys, wrap sites, the touching check)
- `crates/review-ui/src/rows/once.rs` (groups, what shows once, the marks)
- `crates/review-ui/src/paint/code.rs` (`marks_layer`)
- `crates/review-core/src/analysis/moves.rs` (what counts as a move)
- `crates/review-ui/src/rows/moves.rs` (the move edits this replaces)
- `crates/review-cli/src/render.rs` (`SideDiff` in `cr show --json`)
- PLAN.md, "Format-independent classification" and "Move detection"
