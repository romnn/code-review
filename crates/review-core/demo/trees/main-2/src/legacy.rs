//! Compatibility with the version 1 file format.

/// The header line of a version 1 file.
pub const HEADER: &str = "LEDGER V1";

/// Returns whether `text` starts with a version 1 header.
pub fn is_v1(text: &str) -> bool {
    text.lines().next() == Some(HEADER)
}

/// Converts a version 1 line `account;cents` into the current `account 12.50` form.
pub fn convert_line(line: &str) -> Option<String> {
    let (account, cents) = line.split_once(';')?;
    let cents: i64 = cents.trim().parse().ok()?;
    Some(format!("{account} {}", crate::util::format_cents(cents)))
}

/// Converts a whole version 1 file, skipping its header.
pub fn convert(text: &str) -> Vec<String> {
    text.lines().skip(1).filter_map(convert_line).collect()
}

/// Counts the entries in a version 1 file.
pub fn count(text: &str) -> usize {
    text.lines().skip(1).filter(|line| line.contains(';')).count()
}
