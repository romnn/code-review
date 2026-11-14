//! The tree-house tier: tree-house parses a text and its injections and runs Helix's queries
//! over them, and injections in languages it has no row for go to bat over exactly their ranges.

use std::borrow::Cow;
use std::ops::Range;

use ropey::RopeSlice;
use syntect::parsing::SyntaxReference;
use tree_house::highlighter::Highlighter;
use tree_house::{Budget, OwnedInjectionLanguageMarker, UnresolvedInjection};

use super::budget::Deadline;
use super::languages::{BAT_MARKER, Languages, PLAIN_MARKER};
use super::registry::Engine;
use super::tones::Tone;
use super::{Run, Runs, bat};

/// Highlights `text` as `language`, a tree-house-tier row or a template's variant: its runs in
/// bytes of `text`, ordered and without plain ones.
///
/// One deadline bounds it all: tree-house's parse, injections and locals get the time left as
/// their timeout, and the highlighter's queries a [`Budget`] of the time left after them.
/// Nothing is highlighted when the parse does not finish in time, and nothing after the point
/// where the highlighter runs out; an injection bat highlights after the deadline keeps the
/// host's tones.
pub(super) fn highlight(
    languages: &Languages,
    language: tree_house::Language,
    text: &str,
    deadline: Deadline,
) -> Vec<Run> {
    if languages.config_of(language).is_none() {
        return Vec::new();
    }
    // Every offset below is in bytes: tree-sitter's coordinates are, and the rope is only read
    // through byte-indexed slices.
    let source = RopeSlice::from(text);
    let Ok(syntax) = tree_house::Syntax::new(source, language, deadline.remaining(), languages)
    else {
        return Vec::new();
    };
    let budget = Budget::new(deadline.remaining(), None);
    let host = host_runs(&syntax, source, languages, &budget);
    // A spent budget is a passed deadline: bat would not start anyway.
    if host.halted {
        return host.runs;
    }
    let injections = syntax.unresolved_injections(u32::MAX);
    bridge(host.runs, &injections, languages, text, deadline)
}

/// What tree-house's highlighter made of a host document.
pub(super) struct Host {
    /// The runs, ordered and without plain ones.
    pub(super) runs: Vec<Run>,
    /// Whether the budget ran out before the end, which leaves the rest plain.
    pub(super) halted: bool,
}

/// The runs tree-house's highlighter produces over `source` within `budget`: each stretch
/// between two events takes the innermost active highlight.
///
/// A highlighter whose budget runs out drops its highlights where it stopped and reports no
/// further event, so the rest of the text stays plain and nothing it had not finished shows.
pub(super) fn host_runs(
    syntax: &tree_house::Syntax,
    source: RopeSlice<'_>,
    languages: &Languages,
    budget: &Budget<'_>,
) -> Host {
    let end = u32::try_from(source.len_bytes()).unwrap_or(u32::MAX);
    let mut runs = Runs::default();
    let mut highlighter = Highlighter::new_with_budget(syntax, source, languages, .., budget);
    let mut position = 0;
    let mut tone = Tone::Plain;
    while position < end && highlighter.halted().is_none() {
        // `u32::MAX` marks the end of the events.
        let next = highlighter.next_event_offset().min(end);
        if next > position {
            runs.push(to_usize(position)..to_usize(next), tone);
            position = next;
        } else if next < position {
            // The events never go back on a tree parsed in full; if they do, the rest stays
            // plain rather than painted out of order.
            break;
        }
        if position >= end {
            break;
        }
        highlighter.advance();
        tone = highlighter
            .active_highlights()
            .next_back()
            .and_then(|highlight| Tone::from_index(highlight.get()))
            .unwrap_or(Tone::Plain);
    }
    Host {
        runs: runs.into_vec(),
        halted: highlighter.halted().is_some(),
    }
}

/// Gives every injection tree-house left unresolved that bat can highlight bat's runs, plain
/// ones included, over exactly its ranges in place of the host's.
///
/// Each injection is one input to bat, its ranges joined in order, so a construct may continue
/// across the host's gaps but never from one injection into another.
fn bridge(
    host: Vec<Run>,
    injections: &[UnresolvedInjection],
    languages: &Languages,
    text: &str,
    deadline: Deadline,
) -> Vec<Run> {
    let mut cleared: Vec<Range<usize>> = Vec::new();
    let mut added: Vec<Run> = Vec::new();
    for injection in injections {
        if deadline.expired() {
            break;
        }
        let Some(syntax) = bat_syntax(languages, &injection.marker) else {
            continue;
        };
        let ranges: Vec<Range<usize>> = injection
            .ranges
            .iter()
            .map(|range| to_usize(range.start)..to_usize(range.end).min(text.len()))
            .filter(|range| range.start < range.end)
            .collect();
        let input = Joined::new(text, &ranges);
        let runs = bat::highlight(syntax, &input.text, deadline);
        // An injection whose runs are not mapped in time keeps the host's tones.
        let Some(runs) = input.to_host(&runs, deadline) else {
            break;
        };
        added.extend(runs);
        cleared.extend(ranges);
    }
    overlay(host, cleared, added)
}

/// The bat syntax of an injection's marker: a variant's data in a bat syntax by its name, then
/// from a bat-tier row that names it, else by bat's own lookup; a variant's plain data has
/// none.
fn bat_syntax(
    languages: &Languages,
    marker: &OwnedInjectionLanguageMarker,
) -> Option<&'static SyntaxReference> {
    if let OwnedInjectionLanguageMarker::Name(name) = marker {
        if name.as_ref() == PLAIN_MARKER {
            return None;
        }
        if let Some(syntax) = name.strip_prefix(BAT_MARKER) {
            return bat::by_name(syntax);
        }
    }
    let row = match marker {
        OwnedInjectionLanguageMarker::Name(name) => languages.by_name(name),
        OwnedInjectionLanguageMarker::Match(name) => languages.by_token(name),
        OwnedInjectionLanguageMarker::Filename(path) => languages.by_path(path),
        OwnedInjectionLanguageMarker::Shebang(interpreter) => languages.by_interpreter(interpreter),
    };
    if let Some(row) = row.and_then(|index| languages.row(index)) {
        return match row.engine {
            Engine::Bat { syntax } => bat::by_name(syntax),
            // A tree-house-tier row tree-house did not take has no configuration; bat does
            // not second-guess it.
            Engine::TreeHouse { .. } => None,
        };
    }
    match marker {
        OwnedInjectionLanguageMarker::Name(token) | OwnedInjectionLanguageMarker::Match(token) => {
            bat::by_token(token.trim())
        }
        OwnedInjectionLanguageMarker::Filename(path) => bat::by_path(path.trim()),
        OwnedInjectionLanguageMarker::Shebang(interpreter) => bat::by_interpreter(interpreter),
    }
}

/// How many runs [`Joined::to_host`] maps between two looks at the clock.
const DEADLINE_STRIDE: usize = 256;

/// An injection's ranges joined into one text, which maps its offsets back to the host's.
struct Joined<'a> {
    text: Cow<'a, str>,
    /// Each range's start in the joined text and in the host, in order.
    pieces: Vec<Piece>,
}

/// Where one host range lies in the joined text.
struct Piece {
    joined: usize,
    host: Range<usize>,
}

impl<'a> Joined<'a> {
    /// Joins the `ranges` of `host`, which are ordered and do not overlap.
    fn new(host: &'a str, ranges: &[Range<usize>]) -> Self {
        let mut text = String::new();
        let mut pieces = Vec::new();
        for range in ranges {
            let Some(part) = host.get(range.clone()) else {
                continue;
            };
            pieces.push(Piece {
                joined: text.len(),
                host: range.clone(),
            });
            text.push_str(part);
        }
        // One range, the common case, needs no copy.
        let text = match (ranges, pieces.as_slice()) {
            ([range], [_]) => host
                .get(range.clone())
                .map_or(Cow::Owned(text), Cow::Borrowed),
            _ => Cow::Owned(text),
        };
        Self { text, pieces }
    }

    /// `runs`, ordered runs in bytes of the joined text, as runs in bytes of the host, each split
    /// where it crosses from one range to the next; `None` when `deadline` passes first.
    ///
    /// The runs and the pieces are both in order, so one cursor walks the pieces once, and the
    /// work grows with the runs and the pieces, not with their product.
    fn to_host(&self, runs: &[Run], deadline: Deadline) -> Option<Vec<Run>> {
        let mut mapped = Vec::with_capacity(runs.len());
        let mut first = 0;
        for (index, run) in runs.iter().enumerate() {
            // Reading the clock costs more than mapping a run, so it is read every so many.
            if index % DEADLINE_STRIDE == 0 && deadline.expired() {
                return None;
            }
            // Pieces that end before this run end before every later one.
            while self
                .pieces
                .get(first)
                .is_some_and(|piece| piece.joined + piece.host.len() <= run.range.start)
            {
                first += 1;
            }
            for piece in self.pieces.get(first..).unwrap_or_default() {
                if piece.joined >= run.range.end {
                    break;
                }
                let start = run.range.start.max(piece.joined);
                let end = run.range.end.min(piece.joined + piece.host.len());
                if start < end {
                    mapped.push(Run {
                        range: piece.host.start + (start - piece.joined)
                            ..piece.host.start + (end - piece.joined),
                        tone: run.tone,
                    });
                }
            }
        }
        Some(mapped)
    }
}

/// `base` with every range in `cleared` taken out and `added` put in, ordered.
///
/// `added` lies within `cleared`, so nothing of `base` is painted over it; should two
/// injections overlap, the first one's run keeps the bytes.
pub(super) fn overlay(
    base: Vec<Run>,
    mut cleared: Vec<Range<usize>>,
    mut added: Vec<Run>,
) -> Vec<Run> {
    if cleared.is_empty() {
        return base;
    }
    cleared.sort_unstable_by_key(|range| range.start);
    let mut kept: Vec<Run> = Vec::with_capacity(base.len() + added.len());
    let mut holes = cleared.iter().peekable();
    for run in base {
        let mut rest = run.range.clone();
        // Holes that end before this run cannot touch later runs either.
        while holes.peek().is_some_and(|hole| hole.end <= rest.start) {
            holes.next();
        }
        for hole in holes.clone() {
            if hole.start >= rest.end {
                break;
            }
            if hole.start > rest.start {
                kept.push(Run {
                    range: rest.start..hole.start,
                    tone: run.tone,
                });
            }
            rest.start = rest.start.max(hole.end);
            if rest.start >= rest.end {
                break;
            }
        }
        if rest.start < rest.end {
            kept.push(Run {
                range: rest,
                tone: run.tone,
            });
        }
    }
    added.sort_by_key(|run| run.range.start);
    kept.extend(added);
    kept.sort_by_key(|run| run.range.start);
    // Drop what an earlier run already covers.
    let mut runs: Vec<Run> = Vec::with_capacity(kept.len());
    for mut run in kept {
        if let Some(last) = runs.last() {
            run.range.start = run.range.start.max(last.range.end);
        }
        if run.range.start < run.range.end {
            runs.push(run);
        }
    }
    runs
}

/// A tree-sitter offset as an index into the text.
fn to_usize(offset: u32) -> usize {
    usize::try_from(offset).unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// A host of `count` pieces, `x` each, between `{{a}}` actions, and the pieces joined.
    fn pieces(count: usize) -> (String, Vec<Range<usize>>) {
        let host = "x{{a}}".repeat(count);
        let ranges = (0..count).map(|index| index * 6..index * 6 + 1).collect();
        (host, ranges)
    }

    /// Thousands of short pieces map back in one pass: every run lands on its own piece, and
    /// the mapping takes far less than the highlighting budget, as a scan of every piece for
    /// every run would not.
    #[test]
    fn many_pieces_map_back_in_one_pass() {
        let count = 50_000;
        let (host, ranges) = pieces(count);
        let joined = Joined::new(&host, &ranges);
        let runs: Vec<Run> = (0..count)
            .map(|index| Run {
                range: index..index + 1,
                tone: Tone::Keyword,
            })
            .collect();
        let started = Instant::now();
        let mapped = joined
            .to_host(&runs, Deadline::after(Duration::from_secs(600)))
            .unwrap_or_default();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(mapped.len(), count);
        assert!(
            mapped
                .iter()
                .enumerate()
                .all(|(index, run)| run.range == (index * 6..index * 6 + 1))
        );

        // A run across three pieces splits into one run per piece.
        let across = joined
            .to_host(
                &[Run {
                    range: 1..4,
                    tone: Tone::String,
                }],
                Deadline::after(Duration::from_secs(600)),
            )
            .unwrap_or_default();
        let ranges: Vec<Range<usize>> = across.into_iter().map(|run| run.range).collect();
        assert_eq!(ranges, [6..7, 12..13, 18..19]);
    }

    /// A deadline that passed stops the mapping, so the injection keeps the host's tones.
    #[test]
    fn a_passed_deadline_stops_the_mapping() {
        let (host, ranges) = pieces(10);
        let joined = Joined::new(&host, &ranges);
        let runs = [Run {
            range: 0..10,
            tone: Tone::Keyword,
        }];
        let passed = Deadline::after(Duration::ZERO);
        std::thread::sleep(Duration::from_millis(1));
        assert_eq!(joined.to_host(&runs, passed), None);
    }
}
