# Ledger guide

This guide walks through recording entries, checking balances and reconciling them with a
bank statement. For the file formats, see [the v2 notes](v2.md).

## Recording entries

Entries are applied in batches of at most `LIMIT` entries; the rest wait for the next batch:

```rust
use ledger::{Entry, Ledger};

let mut ledger = Ledger::default();
let entries = vec![Entry::new("assets:cash", 12_50), Entry::new("income:gift", -12_50)];
let applied = ledger.apply_batch(&entries);
assert_eq!(applied, entries.len());
```

Amounts are always **integers in cents**, never floats.

## Installing

The install script picks the archive for your platform and checks its checksum:

```bash
curl -fsSL https://example.com/install.sh | bash -s -- --prefix "$HOME/.local"
export PATH="$HOME/.local/bin:$PATH"
ledger --version
```

On Windows, run `scripts/install.ps1` instead.

## Reconciling

`Ledger::reconcile` lists every account whose balance differs from the expected one:

1. Export the statement's closing balances.
2. Run `ledger reconcile --expected balances.json`.
3. Look into each *mismatch* it prints.

## Android

The Android app calls the same API, and falls back to the native ledger offline:

```kotlin
val client = LedgerClient(baseUrl = "https://ledger.example.com")
lifecycleScope.launch {
    val balances = client.balances(currency = "EUR")
    adapter.submitList(balances.filter { it.cents != 0L })
}
```

## Storage

The first migration creates the tables; a balance is the sum of an account's entries:

```sql
SELECT account_id, SUM(amount_cents) AS balance_cents
FROM entries
WHERE booked_at < DATE '2024-02-01'
GROUP BY account_id
HAVING SUM(amount_cents) <> 0
ORDER BY account_id;
```

> **Note:** migrations never change a booked entry; corrections are new entries.

- [x] Record entries
- [ ] Import bank statements
