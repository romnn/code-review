//! Text and JSON rendering of reviews.

use std::fmt::Write as _;
use std::ops::Range;

use color_eyre::eyre;
use review_core::analysis::{EntrySide, NoiseCategory, UnitKind};
use review_core::walkthrough::{Partition, StepKind};
use review_core::{
    Collapse, DiffLineKind, Interdiff, LineEnding, Location, NoteAttachment, Recorded, ReviewKey,
    RevisionView, SideDiff, SplitLine, Standing, Status, UnitId, UnitState, split_line_ending,
};
use serde::Serialize;

/// Applies ANSI styles when color is enabled.
#[derive(Debug, Clone, Copy)]
pub struct Painter {
    pub color: bool,
}

impl Painter {
    fn paint(self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    fn red(self, text: &str) -> String {
        self.paint("31", text)
    }

    fn green(self, text: &str) -> String {
        self.paint("32", text)
    }

    fn yellow(self, text: &str) -> String {
        self.paint("33", text)
    }

    fn bold(self, text: &str) -> String {
        self.paint("1", text)
    }

    fn dim(self, text: &str) -> String {
        self.paint("2", text)
    }
}

pub fn open_summary(key: &ReviewKey, recorded: &Recorded, status: &Status) -> String {
    let mut out = String::new();
    let verb = if recorded.created {
        "recorded"
    } else {
        "unchanged at"
    };
    // Writing to a `String` cannot fail.
    writeln!(
        out,
        "{key}: {verb} revision {} (base {}, head {})",
        recorded.number,
        recorded.base.short(),
        recorded.head.short()
    )
    .ok();
    if recorded.merge_bases.len() > 1 {
        writeln!(
            out,
            "warning: {} merge bases; using the first, {}",
            recorded.merge_bases.len(),
            recorded.base.short()
        )
        .ok();
    }
    out.push_str(&totals(status));
    out
}

fn totals(status: &Status) -> String {
    let total = &status.total;
    format!(
        "{} units: {} new, {} changed since reviewed, {} reviewed, {} accepted, {} flagged, {} approved automatically as formatting-only; collapsed: {} noise, {} formatting-only, {} moved\n",
        total.units,
        total.new,
        total.changed,
        total.reviewed,
        total.accepted,
        total.flagged,
        total.auto_approved,
        total.noise,
        total.formatting_only,
        total.moved
    )
}

pub fn status(status: &Status) -> String {
    let mut out = format!("revision {}: {}", status.revision, totals(status));
    let width = status
        .files
        .iter()
        .map(|file| file.path.len())
        .max()
        .unwrap_or(4)
        .max(4);
    // Writing to a `String` cannot fail.
    writeln!(
        out,
        "\n{:<width$}  {:>5} {:>7} {:>8} {:>8} {:>7} {:>4} {:>5}",
        "file", "new", "changed", "reviewed", "accepted", "flagged", "auto", "noise"
    )
    .ok();
    for file in &status.files {
        let counts = &file.counts;
        writeln!(
            out,
            "{:<width$}  {:>5} {:>7} {:>8} {:>8} {:>7} {:>4} {:>5}",
            file.path,
            counts.new,
            counts.changed,
            counts.reviewed,
            counts.accepted,
            counts.flagged,
            counts.auto_approved,
            counts.noise
        )
        .ok();
    }
    out
}

pub fn partition(view: &RevisionView<'_>, parts: &Partition) -> String {
    let mut out = String::new();
    // Writing to a `String` cannot fail.
    if let (Some(title), Some(written_for)) = (&parts.title, parts.written_for) {
        writeln!(out, "{title} (written for revision {written_for})").ok();
    } else {
        writeln!(out, "default order (no walkthrough imported)").ok();
    }
    for (index, step) in parts.steps.iter().enumerate() {
        let reviewed = step
            .units
            .iter()
            .filter(|&&unit| view.standing(unit).is_ok_and(Standing::is_reviewed))
            .count();
        writeln!(
            out,
            "{:>3}. {} [{reviewed}/{}]",
            index + 1,
            step.title,
            step.units.len()
        )
        .ok();
        if !step.why.is_empty() {
            writeln!(out, "     {}", step.why).ok();
        }
    }
    for warning in &parts.warnings {
        writeln!(out, "warning: {warning}").ok();
    }
    out
}

/// A unit's standing as its line shows it: its review state, or that it was approved
/// automatically, which no mark records.
fn standing_label(view: &RevisionView<'_>, unit: UnitId) -> eyre::Result<String> {
    Ok(match view.standing(unit)? {
        Standing::AutoApproved => "approved automatically · reformatted only".to_owned(),
        _ => state_label(view.state(unit)?),
    })
}

fn state_label(state: UnitState) -> String {
    match state {
        UnitState::New => "new".to_owned(),
        UnitState::Unseen { .. } => "unseen".to_owned(),
        UnitState::Reviewed {
            verdict, inherited, ..
        } => {
            if inherited {
                format!("{} (carried)", verdict.name())
            } else {
                verdict.name().to_owned()
            }
        }
        UnitState::ChangedSinceReviewed { reviewed, .. } => format!("changed since {reviewed}"),
    }
}

fn location_text(location: &Location) -> String {
    format!("{}:{}-{}", location.path, location.line, location.end_line)
}

/// Renders the steps of a partition with each unit's change.
pub fn show(
    view: &RevisionView<'_>,
    parts: &Partition,
    expand: bool,
    painter: Painter,
) -> eyre::Result<String> {
    let mut out = String::new();
    let record = view.record();
    // Writing to a `String` cannot fail.
    writeln!(
        out,
        "revision {} (base {}, head {})",
        view.number(),
        record.map_or("?", |record| record.base.short()),
        record.map_or("?", |record| record.head.short())
    )
    .ok();
    for (index, step) in parts.steps.iter().enumerate() {
        writeln!(
            out,
            "\n{}",
            painter.bold(&format!("== {}. {} ==", index + 1, step.title))
        )
        .ok();
        if !step.why.is_empty() {
            writeln!(out, "{}", painter.dim(&step.why)).ok();
        }
        let mut hidden_formatting = 0;
        let mut hidden_approved = 0;
        let mut hidden_noise: Vec<NoiseCategory> = Vec::new();
        for &unit in &step.units {
            match view.collapse(unit)? {
                Some(Collapse::Noise(category)) if !expand => hidden_noise.push(category),
                Some(Collapse::FormattingOnly) if !expand => {
                    hidden_formatting += 1;
                    hidden_approved += usize::from(view.standing(unit)? == Standing::AutoApproved);
                }
                Some(Collapse::Moved(_)) if !expand => {
                    render_move_line(&mut out, view, unit, painter)?;
                }
                _ => render_unit(&mut out, view, unit, painter)?,
            }
        }
        if hidden_formatting > 0 {
            let approved = match hidden_approved {
                0 => String::new(),
                all if all == hidden_formatting => ", approved automatically".to_owned(),
                some => format!(", {some} approved automatically"),
            };
            writeln!(
                out,
                "  {}",
                painter.dim(&format!(
                    "{hidden_formatting} formatting-only units hidden{approved}"
                ))
            )
            .ok();
        }
        hidden_noise.sort();
        let mut categories = hidden_noise.clone();
        categories.dedup();
        for category in categories {
            let count = hidden_noise
                .iter()
                .filter(|&&noise| noise == category)
                .count();
            writeln!(
                out,
                "  {}",
                painter.dim(&format!("{count} {} units collapsed", category.name()))
            )
            .ok();
        }
    }
    for warning in &parts.warnings {
        writeln!(out, "warning: {warning}").ok();
    }
    Ok(out)
}

fn render_move_line(
    out: &mut String,
    view: &RevisionView<'_>,
    unit: UnitId,
    painter: Painter,
) -> eyre::Result<()> {
    let state = standing_label(view, unit)?;
    let description = match view.collapse(unit)? {
        Some(Collapse::Moved(found)) => found.describe_for(unit),
        _ => None,
    }
    .unwrap_or_else(|| "collapsed".to_owned());
    // Writing to a `String` cannot fail.
    writeln!(
        out,
        "  {unit} [{state}] {} {}",
        location_text(&view.location(unit)?),
        painter.dim(&description)
    )
    .ok();
    Ok(())
}

fn render_unit(
    out: &mut String,
    view: &RevisionView<'_>,
    unit: UnitId,
    painter: Painter,
) -> eyre::Result<()> {
    let record = view.unit(unit)?;
    let label = standing_label(view, unit)?;
    let label = if view.standing(unit)?.is_reviewed() {
        painter.dim(&label)
    } else {
        painter.yellow(&label)
    };
    // Writing to a `String` cannot fail.
    writeln!(
        out,
        "  {} [{label}] {}",
        painter.bold(&unit.to_string()),
        location_text(&view.location(unit)?)
    )
    .ok();
    match &record.kind {
        UnitKind::Chunk(chunk) => {
            render_lines(out, &chunk.removed, &chunk.removed_highlights, '-', painter);
            render_lines(out, &chunk.added, &chunk.added_highlights, '+', painter);
            if chunk.whitespace_only && !record.formatting_only {
                writeln!(
                    out,
                    "    {}",
                    painter.dim("(whitespace only; not proven equivalent)")
                )
                .ok();
            }
        }
        UnitKind::File(_) => {
            writeln!(out, "    {}", view.describe(unit)?.unwrap_or_default()).ok();
        }
    }
    for found in &view.moves_touching(unit) {
        let description = found.describe_for(unit).unwrap_or_default();
        if found.target.unit == unit {
            writeln!(
                out,
                "    {}",
                painter.dim(&format!("{description}; edits:"))
            )
            .ok();
            if let Some(inner) = &found.inner {
                render_diff_lines(out, inner, painter);
            }
        } else {
            writeln!(out, "    {}", painter.dim(&description)).ok();
        }
    }
    if let Some(interdiff) = view.interdiff(unit)? {
        render_interdiff(out, &interdiff, painter);
    }
    for note in view.notes() {
        if note.attached == (NoteAttachment::Unit { unit }) {
            writeln!(out, "    note ({}): {}", note.written_on, note.text).ok();
        }
    }
    Ok(())
}

/// Writes the lines of an interdiff side or a move's inner diff.
fn render_diff_lines(out: &mut String, diff: &SideDiff, painter: Painter) {
    for line in &diff.lines {
        let text = format!("{}{}", line.text, crlf_marker(line.ending));
        let rendered = match line.kind {
            DiffLineKind::Same => format!("       {text}"),
            DiffLineKind::Old => painter.red(&format!("     - {text}")),
            DiffLineKind::New => painter.green(&format!("     + {text}")),
        };
        // Writing to a `String` cannot fail.
        writeln!(out, "{rendered}").ok();
        if line.ending == LineEnding::Missing {
            writeln!(out, "       {}", painter.dim(NO_FINAL_NEWLINE)).ok();
        }
    }
}

fn render_interdiff(out: &mut String, interdiff: &Interdiff, painter: Painter) {
    let side = |out: &mut String, title: &str, diff: &SideDiff| {
        if !diff.changed {
            return;
        }
        // Writing to a `String` cannot fail.
        writeln!(out, "    {}", painter.yellow(title)).ok();
        render_diff_lines(out, diff, painter);
    };
    // Writing to a `String` cannot fail.
    writeln!(out, "    interdiff against {}:", interdiff.reviewed).ok();
    if !interdiff.base.changed && !interdiff.head.changed {
        writeln!(
            out,
            "    {}",
            painter.dim("identical to the reviewed version, but it changed in between")
        )
        .ok();
    }
    side(
        out,
        "removed side changed (the target branch moved):",
        &interdiff.base,
    );
    side(
        out,
        "added side changed (the change itself changed):",
        &interdiff.head,
    );
}

/// Shown after a line that has no terminator, as `git diff` does.
const NO_FINAL_NEWLINE: &str = "\\ No newline at end of file";

/// Returns the visible marker for a CRLF terminator, so CRLF and LF lines never look alike.
fn crlf_marker(ending: LineEnding) -> &'static str {
    if ending == LineEnding::CrLf {
        "␍"
    } else {
        ""
    }
}

/// Writes the lines of one chunk side with `sign`, highlights, and line-ending markers.
fn render_lines(
    out: &mut String,
    text: &str,
    highlights: &[Range<usize>],
    sign: char,
    painter: Painter,
) {
    for line in highlighted_lines(text, highlights, painter) {
        let rendered = format!("{sign}{}{}", line.text, crlf_marker(line.ending));
        let colored = if sign == '-' {
            painter.red(&rendered)
        } else {
            painter.green(&rendered)
        };
        // Writing to a `String` cannot fail.
        writeln!(out, "    {colored}").ok();
        if line.ending == LineEnding::Missing {
            writeln!(out, "    {}", painter.dim(NO_FINAL_NEWLINE)).ok();
        }
    }
}

/// A rendered line and how it ended.
struct RenderedLine {
    text: String,
    ending: LineEnding,
}

/// Splits `text` into lines without terminators, with the highlighted byte ranges emphasized.
fn highlighted_lines(
    text: &str,
    highlights: &[Range<usize>],
    painter: Painter,
) -> Vec<RenderedLine> {
    let mut lines = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let SplitLine { content, ending } = split_line_ending(line);
        let mut rendered = String::new();
        let mut position = 0;
        for range in highlights {
            let start = range.start.saturating_sub(offset).min(content.len());
            let end = range.end.saturating_sub(offset).min(content.len());
            if range.end <= offset || start >= end || start < position {
                continue;
            }
            let (Some(before), Some(highlighted)) =
                (content.get(position..start), content.get(start..end))
            else {
                continue;
            };
            rendered.push_str(before);
            rendered.push_str(&painter.paint("7", highlighted));
            position = end;
        }
        rendered.push_str(content.get(position..).unwrap_or_default());
        lines.push(RenderedLine {
            text: rendered,
            ending,
        });
        offset += line.len();
    }
    lines
}

/// One unit in `cr show --json`.
#[derive(Serialize)]
pub struct UnitJson {
    id: UnitId,
    state: UnitState,
    /// Where the unit stands under the policy: its state, or approved automatically.
    standing: Standing,
    location: Location,
    /// `chunk`, or the file unit's kind such as `symlink` or `type`.
    kind: String,
    /// A file unit's full description; chunks carry their lines instead.
    description: Option<String>,
    /// The entry at the base: path, mode, object ID and symlink target.
    old: Option<EntrySide>,
    /// The entry at the head.
    new: Option<EntrySide>,
    removed: Option<String>,
    added: Option<String>,
    formatting_only: bool,
    moved: bool,
    noise: Option<NoiseCategory>,
    interdiff: Option<Interdiff>,
    notes: Vec<String>,
}

/// One step in `cr show --json`.
#[derive(Serialize)]
pub struct StepJson {
    title: String,
    why: String,
    kind: StepKind,
    units: Vec<UnitJson>,
}

/// The document `cr show --json` prints.
#[derive(Serialize)]
pub struct ShowJson {
    revision: u32,
    title: Option<String>,
    steps: Vec<StepJson>,
    warnings: Vec<String>,
}

pub fn show_json(view: &RevisionView<'_>, parts: &Partition) -> eyre::Result<ShowJson> {
    let notes = view.notes();
    let steps = parts
        .steps
        .iter()
        .map(|step| {
            let units = step
                .units
                .iter()
                .map(|&unit| {
                    let record = view.unit(unit)?;
                    let file = view.file(unit)?;
                    let chunk = record.chunk();
                    let kind = match &record.kind {
                        UnitKind::Chunk(_) => "chunk".to_owned(),
                        UnitKind::File(kind) => serde_json::to_value(kind)?
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                    };
                    Ok(UnitJson {
                        id: unit,
                        state: view.state(unit)?,
                        standing: view.standing(unit)?,
                        location: view.location(unit)?,
                        kind,
                        description: view.describe(unit)?,
                        old: file.old.clone(),
                        new: file.new.clone(),
                        removed: chunk.map(|chunk| chunk.removed.clone()),
                        added: chunk.map(|chunk| chunk.added.clone()),
                        formatting_only: record.formatting_only,
                        moved: record.moved,
                        noise: view.noise(unit)?,
                        interdiff: view.interdiff(unit)?,
                        notes: notes
                            .iter()
                            .filter(|note| note.attached == NoteAttachment::Unit { unit })
                            .map(|note| note.text.clone())
                            .collect(),
                    })
                })
                .collect::<eyre::Result<Vec<_>>>()?;
            Ok(StepJson {
                title: step.title.clone(),
                why: step.why.clone(),
                kind: step.kind,
                units,
            })
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    Ok(ShowJson {
        revision: view.number().get(),
        title: parts.title.clone(),
        steps,
        warnings: parts.warnings.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(text: &str, sign: char) -> String {
        let mut out = String::new();
        render_lines(&mut out, text, &[], sign, Painter { color: false });
        out
    }

    /// A missing final newline and CRLF terminators get explicit markers, so changing only the
    /// line ending never renders as two identical lines, and lone carriage returns are kept.
    #[test]
    fn line_endings_are_visible() {
        assert_eq!(rendered("value\n", '-'), "    -value\n");
        assert_eq!(
            rendered("value", '+'),
            "    +value\n    \\ No newline at end of file\n"
        );
        assert_eq!(rendered("value\r\n", '+'), "    +value␍\n");
        assert_eq!(rendered("value\r\r\n", '+'), "    +value\r␍\n");
    }
}
