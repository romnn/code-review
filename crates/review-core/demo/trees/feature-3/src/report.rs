//! Reports over a ledger.

use crate::engine::Ledger;

/// Collects the rows of a balance report.
pub fn rows(ledger: &Ledger) -> Vec<(String, i64)> {
    ledger.balances().map(|(account, cents)| (account.clone(), *cents)).collect()
}
