//! The pickers' content: the query line, the entries, the preview of a content hit and the
//! footer.

use std::ops::Range;

use super::files::progress_glyph;
use super::{Palette, Piece, Run, Style, Styled, Weight};
use crate::color::Hsla;
use crate::model::picker::{Kind, Picker, Scope, occurrences};
use crate::model::plural;

/// How many lines the preview shows above a hit.
const PREVIEW_ABOVE: usize = 8;

/// How many lines the preview shows at most.
const PREVIEW_LINES: usize = 80;

/// How many characters of a preview line show.
const PREVIEW_CHARS: usize = 400;

/// The picker's query line: what it finds, then the field, the count and the scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// `files ›` or `grep ›`, semibold in the accent.
    pub title: Piece,
    /// How many entries there are, or that the search runs or failed.
    pub count: Piece,
    /// `changed files` or `whole tree`, on [`Self::scope_background`].
    pub scope: Piece,
    /// The scope's background.
    pub scope_background: Hsla,
}

/// The open picker's query line.
#[must_use]
pub fn query(picker: &Picker, palette: &Palette) -> Query {
    let title = match picker.kind {
        Kind::Files => "files",
        Kind::Content => "grep",
    };
    let scope = match picker.scope {
        Scope::Changed => "changed files",
        Scope::Tree => "whole tree",
    };
    let count = if picker.searching {
        "searching…".to_owned()
    } else if let Some(error) = &picker.error {
        error.clone()
    } else if picker.truncated {
        format!("first {}", picker.entries.len())
    } else {
        plural(
            picker.entries.len(),
            if picker.kind == Kind::Files {
                "file"
            } else {
                "hit"
            },
        )
    };
    Query {
        title: Piece::new(format!("{title} ›"), palette.accent).weighted(Weight::Semibold),
        count: Piece::new(count, palette.dim),
        scope: Piece::new(scope, palette.text),
        scope_background: palette.raised,
    }
}

/// What the picker's footer says the keys do.
#[must_use]
pub fn footer(picker: &Picker, palette: &Palette) -> Piece {
    let mode = if picker.editing {
        "typing · Esc for the list"
    } else {
        "list · i to type · Esc closes"
    };
    Piece::new(mode, palette.faint)
}

/// The backdrop over the window while a picker is open: the editor background, nearly opaque,
/// so the code behind never reads as part of the picker's preview.
#[must_use]
pub fn backdrop(palette: &Palette) -> Hsla {
    palette.background.alpha(0.85)
}

/// The picker's border: the accent while its list has the keys, otherwise the dim text color,
/// either of which stands out from the backdrop and the picker's own surface in every theme.
#[must_use]
pub fn border(palette: &Palette, editing: bool) -> Hsla {
    if editing { palette.dim } else { palette.accent }
}

/// An entry of the picker's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The change kind's letter, colored by kind, or a blank for a file outside the review.
    pub letter: Piece,
    /// The path or the hit.
    pub content: Content,
    /// The selected entry's background.
    pub background: Option<Hsla>,
}

/// What an entry of the picker shows after its letter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// A file: its path with the query's matches, and its review progress.
    File {
        /// The path, with the matched characters in bold accent.
        path: Styled,
        /// The path's color: its review state's, or dim outside the review.
        color: Hsla,
        /// The reviewed and total hunks, such as `2/5`, and the progress glyph in a column
        /// as wide as the tree's, for a file under review.
        progress: Option<[Piece; 2]>,
    },
    /// A content hit: where it is and the line with its matches.
    Hit {
        /// `path:line`, cut at a share of the row.
        location: Piece,
        /// The line, with the matches in bold accent.
        text: Styled,
        /// The line's color.
        color: Hsla,
    },
}

/// The picker's entry `index`.
#[must_use]
pub fn row(picker: &Picker, palette: &Palette, index: usize) -> Option<Row> {
    let entry = picker.entries.get(index)?;
    let change = entry.change;
    let letter = change.map_or_else(
        || Piece::new(" ", palette.faint),
        |change| Piece::new(change.letter.to_string(), palette.tone(change.tone)),
    );
    let content = match entry.line {
        None => Content::File {
            path: matches(&entry.path, &entry.highlights, palette),
            color: change.map_or(palette.dim, |change| palette.progress(change.progress)),
            progress: change.map(|change| {
                let color = palette.progress(change.progress);
                [
                    Piece::new(format!("{}/{}", change.reviewed, change.total), color),
                    Piece::new(progress_glyph(change.progress), color),
                ]
            }),
        },
        Some(line) => Content::Hit {
            location: Piece::new(format!("{}:{}", entry.path, line + 1), palette.dim),
            text: matches(&entry.text, &entry.highlights, palette),
            color: palette.text,
        },
    };
    Some(Row {
        letter,
        content,
        background: (index == picker.selected).then_some(palette.raised),
    })
}

/// The preview of the selected content hit: the lines around it, or why it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preview {
    /// The file's path over the lines around the hit.
    Lines {
        /// The path.
        path: Piece,
        /// The lines, the hit's on [`Palette::raised`].
        lines: Vec<PreviewLine>,
    },
    /// Why there are no lines yet or at all.
    Note(Piece),
}

/// A line of a picker's preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewLine {
    /// The line's number, padded to the widest shown.
    pub number: Piece,
    /// Its first characters, with the query's matches on the hit's line.
    pub text: Styled,
    /// The text's color: brighter on the hit's line.
    pub color: Hsla,
    /// The hit's line's background.
    pub background: Option<Hsla>,
}

/// The preview of the selected content hit, when one is selected.
#[must_use]
pub fn preview(picker: &Picker, palette: &Palette) -> Option<Preview> {
    let entry = picker.entry()?;
    let line = entry.line.unwrap_or(0);
    let Some(text) = picker.text(&entry.path) else {
        let note = if picker.unreadable.contains(&entry.path) {
            "Binary, or could not be read."
        } else {
            "Reading the file…"
        };
        return Some(Preview::Note(Piece::new(note, palette.faint)));
    };
    let start = line.saturating_sub(PREVIEW_ABOVE);
    let needle = picker.query.as_str();
    let ignore_case = !needle.chars().any(char::is_uppercase);
    let digits = (start + PREVIEW_LINES)
        .min(text.lines.len())
        .to_string()
        .len();
    let lines = text
        .lines
        .iter()
        .enumerate()
        .skip(start)
        .take(PREVIEW_LINES)
        .map(|(index, content)| {
            let hit = index == line;
            let shown: String = content.chars().take(PREVIEW_CHARS).collect();
            let ranges = if hit {
                occurrences(&shown, needle, ignore_case)
            } else {
                Vec::new()
            };
            PreviewLine {
                number: Piece::new(
                    format!("{:>digits$}", index + 1),
                    if hit { palette.accent } else { palette.faint },
                ),
                text: matches(&shown, &ranges, palette),
                color: if hit { palette.text } else { palette.dim },
                background: hit.then_some(palette.raised),
            }
        })
        .collect();
    Some(Preview::Lines {
        path: Piece::new(entry.path.clone(), palette.dim),
        lines,
    })
}

/// `text` with `ranges` drawn as matches, in bold accent; ranges off the text's character
/// boundaries are left out.
fn matches(text: &str, ranges: &[Range<usize>], palette: &Palette) -> Styled {
    let style = Style {
        bold: true,
        ..Style::text(palette.accent)
    };
    Styled {
        text: text.to_owned(),
        runs: ranges
            .iter()
            .filter(|range| {
                range.end <= text.len()
                    && text.is_char_boundary(range.start)
                    && text.is_char_boundary(range.end)
            })
            .map(|range| Run {
                range: range.clone(),
                style,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Background;
    use crate::theme;

    /// In every built-in theme the picker's border stands out (3:1, as WCAG asks of a
    /// component's edge) from its surface and from the backdrop over the code, whose text
    /// shows through dimmed well below the picker's own.
    #[test]
    fn the_frame_stands_out_in_every_theme() {
        let (themes, problems) = theme::all(None);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(themes.len(), 9);
        for theme in &themes {
            let palette = Palette::new(theme, None, None, Background::Bold);
            let behind = theme::mix(palette.text, palette.background, backdrop(&palette).a);
            for editing in [true, false] {
                let border = border(&palette, editing);
                for (against, what) in [
                    (palette.surface, "surface"),
                    (palette.background, "backdrop"),
                ] {
                    let ratio = theme::contrast(border, against);
                    assert!(
                        ratio >= 3.0,
                        "{}: the border (editing {editing}) is {ratio:.2}:1 against the {what}",
                        theme.name
                    );
                }
            }
            let ratio = theme::contrast(behind, palette.background);
            assert!(
                ratio < 1.6,
                "{}: code behind the backdrop still reads at {ratio:.2}:1",
                theme.name
            );
        }
    }

    /// Matches off character boundaries or past the text are dropped rather than splitting a
    /// character.
    #[test]
    fn matches_keep_to_character_boundaries() {
        let (themes, _) = theme::all(None);
        let Some(theme) = themes.first() else {
            panic!("built-in themes");
        };
        let palette = Palette::new(theme, None, None, Background::Bold);
        let styled = matches("añb", &[0..1, 1..2, 1..3, 3..9], &palette);
        let ranges: Vec<Range<usize>> = styled.runs.iter().map(|run| run.range.clone()).collect();
        assert_eq!(ranges, [0..1, 1..3]);
    }
}
