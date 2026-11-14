//! Small helpers.

/// Removes a trailing `#` comment.
pub fn trim_comment(line: &str) -> &str {
    line.split_once('#').map_or(line, |(code, _)| code).trim_end()
}

/// Formats cents as `12.50`.
pub fn format_cents(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, (cents % 100).abs())
}
