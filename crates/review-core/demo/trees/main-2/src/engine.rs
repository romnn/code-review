//! Applies entries to balances.

use std::collections::BTreeMap;

use crate::parser::Entry;

/// The largest number of entries applied in one batch.
pub const LIMIT: u32 = 100;

/// Balances per account, in cents.
#[derive(Debug, Default)]
pub struct Ledger {
    balances: BTreeMap<String, i64>,
    applied: u32,
}

impl Ledger {
    /// Applies one entry.
    pub fn apply(&mut self, entry: &Entry) {
        *self.balances.entry(entry.account.clone()).or_insert(0) += entry.amount_cents;
        self.applied += 1;
    }

    /// Applies a batch, stopping at the limit.
    pub fn apply_batch(&mut self, entries: &[Entry]) -> usize {
        let take = entries.len().min(usize::try_from(LIMIT).unwrap_or(usize::MAX));
        for entry in &entries[..take] { self.apply(entry); }
        take
    }

    /// Returns the balance of an account.
    pub fn balance(&self, account: &str) -> i64 {
        self.balances.get(account).copied().unwrap_or(0)
    }

    /// Returns every account with its balance, sorted by account.
    pub fn balances(&self) -> impl Iterator<Item = (&String, &i64)> {
        self.balances.iter()
    }

    /// Writes the ledger to the legacy flat file format.
    pub fn legacy_flush(&self) -> String {
        let mut out = String::new();
        for (account, cents) in &self.balances {
            out.push_str(&format!("{account};{cents}\n"));
        }
        out
    }

    /// Returns how many entries were applied.
    pub fn applied(&self) -> u32 {
        self.applied
    }
}
