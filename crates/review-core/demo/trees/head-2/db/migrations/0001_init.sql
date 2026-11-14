-- The ledger's first schema: accounts, entries and the balances view.
BEGIN;

CREATE TABLE accounts (
    id          TEXT PRIMARY KEY CHECK (length(id) BETWEEN 1 AND 63),
    kind        TEXT NOT NULL CHECK (kind IN ('assets', 'liabilities', 'income', 'expenses')),
    currency    CHAR(3) NOT NULL DEFAULT 'EUR',
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE entries (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id    TEXT NOT NULL REFERENCES accounts (id) ON DELETE RESTRICT,
    amount_cents  BIGINT NOT NULL,
    booked_at     DATE NOT NULL,
    memo          TEXT,
    batch         INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX entries_by_account ON entries (account_id, booked_at);

-- Accounts without entries have a balance of zero.
CREATE VIEW balances AS
SELECT a.id AS account,
       a.currency,
       COALESCE(SUM(e.amount_cents), 0) AS balance_cents,
       COUNT(e.id) AS entry_count
FROM accounts AS a
LEFT JOIN entries AS e ON e.account_id = a.id
GROUP BY a.id, a.currency;

INSERT INTO accounts (id, kind) VALUES
    ('assets:cash', 'assets'),
    ('assets:bank', 'assets'),
    ('expenses:food', 'expenses'),
    ('income:salary', 'income');

COMMIT;
