//! Text formatting of reports.

/// Renders balances as an aligned two-column table.
pub fn render_table(rows: &[(String, i64)]) -> String {
    let width = rows.iter().map(|(account, _)| account.len()).max().unwrap_or(0);
    let mut out = String::new();
    for (account, cents) in rows {
        let amount = crate::util::format_cents(*cents);
        out.push_str(&format!("{account:<width$} | {amount:>12}\n"));
    }
    out
}
