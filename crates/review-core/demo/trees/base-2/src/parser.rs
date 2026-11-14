//! Parses ledger entries from text.

use crate::util::trim_comment;

/// One parsed ledger entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub account: String,
    pub amount_cents: i64,
    pub memo: Option<String>,
}

/// Parses one line such as `assets:cash 12.50 lunch`.
pub fn parse_line(line: &str) -> Option<Entry> {
    let line = trim_comment(line);
    let mut parts = line.split_whitespace();
    let account = parts.next()?.to_string();
    let amount_cents = parse_amount(parts.next()?)?;
    let memo = parts.collect::<Vec<_>>().join(" ");
    Some(Entry { account, amount_cents, memo: if memo.is_empty() { None } else { Some(memo) } })
}

/// Parses `12.50` into 1250 cents.
pub fn parse_amount(text: &str) -> Option<i64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, "0"));
    let cents = fraction.chars().chain(std::iter::repeat('0')).take(2).collect::<String>();
    Some(whole.parse::<i64>().ok()? * 100 + cents.parse::<i64>().ok()?)
}

/// Parses every non-empty line, skipping lines that fail to parse.
pub fn parse_all(text: &str) -> Vec<Entry> {
    text.lines().filter(|line| !line.trim().is_empty()).filter_map(parse_line).collect()
}

/// Groups entries by account, keeping their order.
pub fn group_by_account(entries: &[Entry]) -> Vec<(String, Vec<Entry>)> {
    let mut groups: Vec<(String, Vec<Entry>)> = Vec::new();
    for entry in entries {
        match groups.iter_mut().find(|(account, _)| account == &entry.account) {
            Some((_, group)) => group.push(entry.clone()),
            None => groups.push((entry.account.clone(), vec![entry.clone()])),
        }
    }
    groups
}
