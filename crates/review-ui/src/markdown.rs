//! A small Markdown reader for pull request descriptions: headings, paragraphs, lists, code
//! blocks, and inline code, emphasis and links.

use std::ops::Range;

/// A block of a Markdown document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// A heading of level 1 to 6.
    Heading(usize, Inline),
    /// A paragraph.
    Paragraph(Inline),
    /// A list item; `ordered` carries its number.
    Item {
        /// The number of an ordered item.
        ordered: Option<usize>,
        /// The nesting depth, from the indentation.
        depth: usize,
        /// The item's text.
        text: Inline,
    },
    /// A fenced code block.
    Code(String),
    /// A horizontal rule.
    Rule,
}

/// Text with inline spans.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inline {
    /// The text without Markdown punctuation.
    pub text: String,
    /// The spans, in order.
    pub spans: Vec<(Range<usize>, Span)>,
}

/// An inline style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Span {
    /// `code`.
    Code,
    /// **bold** or __bold__.
    Strong,
    /// *emphasis* or _emphasis_.
    Emphasis,
    /// A link to the URL.
    Link(String),
}

/// Parses `source` into blocks.
#[must_use]
pub fn parse(source: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut paragraph: Vec<&str> = Vec::new();
    let mut code: Option<Vec<&str>> = None;
    let flush = |paragraph: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        if !paragraph.is_empty() {
            blocks.push(Block::Paragraph(inline(&paragraph.join(" "))));
            paragraph.clear();
        }
    };
    for line in source.lines() {
        if let Some(lines) = &mut code {
            if line.trim_start().starts_with("```") {
                blocks.push(Block::Code(lines.join("\n")));
                code = None;
            } else {
                lines.push(line);
            }
            continue;
        }
        let trimmed = line.trim_start();
        let depth = (line.len() - trimmed.len()) / 2;
        if trimmed.starts_with("```") {
            flush(&mut paragraph, &mut blocks);
            code = Some(Vec::new());
        } else if trimmed.is_empty() {
            flush(&mut paragraph, &mut blocks);
        } else if let Some(level) = heading_level(trimmed) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Heading(level, inline(trimmed[level..].trim())));
        } else if matches!(trimmed, "---" | "***" | "___") {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Rule);
        } else if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Item {
                ordered: None,
                depth,
                text: inline(rest),
            });
        } else if let Some((number, rest)) = ordered_item(trimmed) {
            flush(&mut paragraph, &mut blocks);
            blocks.push(Block::Item {
                ordered: Some(number),
                depth,
                text: inline(rest),
            });
        } else {
            paragraph.push(trimmed);
        }
    }
    if let Some(lines) = code {
        blocks.push(Block::Code(lines.join("\n")));
    }
    flush(&mut paragraph, &mut blocks);
    blocks
}

fn heading_level(line: &str) -> Option<usize> {
    let level = line.chars().take_while(|&ch| ch == '#').count();
    ((1..=6).contains(&level) && line[level..].starts_with(' ')).then_some(level)
}

fn ordered_item(line: &str) -> Option<(usize, &str)> {
    let (number, rest) = line.split_once(". ")?;
    Some((number.parse().ok()?, rest))
}

/// Parses inline code, emphasis and links.
#[must_use]
pub fn inline(source: &str) -> Inline {
    let mut out = Inline::default();
    let mut rest = source;
    while let Some(ch) = rest.chars().next() {
        let start = out.text.len();
        match ch {
            '`' => {
                if let Some(end) = rest[1..].find('`') {
                    out.text.push_str(&rest[1..=end]);
                    out.spans.push((start..out.text.len(), Span::Code));
                    rest = &rest[end + 2..];
                    continue;
                }
            }
            '*' | '_' => {
                let double = rest.starts_with("**") || rest.starts_with("__");
                let marker = if double { &rest[..2] } else { &rest[..1] };
                if let Some(end) = rest[marker.len()..].find(marker).filter(|&end| end > 0) {
                    let inner = &rest[marker.len()..marker.len() + end];
                    out.text.push_str(inner);
                    let span = if double { Span::Strong } else { Span::Emphasis };
                    out.spans.push((start..out.text.len(), span));
                    rest = &rest[marker.len() * 2 + end..];
                    continue;
                }
            }
            '[' => {
                if let Some(close) = rest.find("](")
                    && let Some(end) = rest[close + 2..].find(')')
                {
                    out.text.push_str(&rest[1..close]);
                    let url = rest[close + 2..close + 2 + end].to_owned();
                    out.spans.push((start..out.text.len(), Span::Link(url)));
                    rest = &rest[close + 3 + end..];
                    continue;
                }
            }
            _ => {}
        }
        out.text.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::*;

    #[test]
    fn blocks_parse() {
        let blocks = parse(indoc! {"
            ## Summary

            One
            two.

            - a `b`
              - nested
            1. first

            ```sh
            cargo test
            ```
            ---
        "});
        assert_eq!(blocks[0], Block::Heading(2, inline("Summary")));
        assert_eq!(blocks[1], Block::Paragraph(inline("One two.")));
        assert!(
            matches!(&blocks[2], Block::Item { ordered: None, depth: 0, text } if text.text == "a b")
        );
        assert!(matches!(&blocks[3], Block::Item { depth: 1, .. }));
        assert!(matches!(
            &blocks[4],
            Block::Item {
                ordered: Some(1),
                ..
            }
        ));
        assert_eq!(blocks[5], Block::Code("cargo test".to_owned()));
        assert_eq!(blocks[6], Block::Rule);
    }

    #[test]
    fn inline_spans_parse() {
        let parsed = inline("Use `f()` with **care**, see [docs](https://x.y) and *this*.");
        assert_eq!(parsed.text, "Use f() with care, see docs and this.");
        let kinds: Vec<&Span> = parsed.spans.iter().map(|(_, span)| span).collect();
        assert_eq!(
            kinds,
            [
                &Span::Code,
                &Span::Strong,
                &Span::Link("https://x.y".to_owned()),
                &Span::Emphasis
            ]
        );
        assert_eq!(&parsed.text[parsed.spans[2].0.clone()], "docs");
        // Unclosed markers stay as text.
        assert_eq!(inline("a * b and `c").text, "a * b and `c");
    }
}
