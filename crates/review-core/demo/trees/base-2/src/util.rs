//! Small helpers.

/// Removes a trailing `#` comment.
pub fn trim_comment(line: &str) -> &str {
    line.split_once('#').map_or(line, |(code, _)| code).trim_end()
}

/// Computes a simple position-weighted checksum of the entries' amounts.
pub fn checksum(amounts: &[i64]) -> i64 {
    let mut sum = 0i64;
    for (index, amount) in amounts.iter().enumerate() {
        let weight = i64::try_from(index).unwrap_or(i64::MAX).wrapping_add(1);
        sum = sum.wrapping_add(amount.wrapping_mul(weight));
    }
    sum
}

/// Formats cents as `12.50`.
pub fn format_cents(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, (cents % 100).abs())
}
