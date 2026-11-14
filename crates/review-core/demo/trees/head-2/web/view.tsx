// The balances table and the reconciliation report.

import { useEffect, useMemo, useState } from "react";

import { type Balance, Currency, type LedgerClient, type Mismatch } from "./api";

interface BalancesProps {
  client: LedgerClient;
  currency?: Currency;
}

const format = (cents: number, currency: Currency): string =>
  new Intl.NumberFormat(undefined, { style: "currency", currency }).format(cents / 100);

export function Balances({ client, currency = Currency.Eur }: BalancesProps) {
  const [balances, setBalances] = useState<Balance[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    client
      .balances(currency)
      .then((loaded) => {
        if (!cancelled) setBalances(loaded);
      })
      .catch((reason: unknown) => setError(String(reason)));
    return () => {
      cancelled = true;
    };
  }, [client, currency]);

  const total = useMemo(() => balances.reduce((sum, { cents }) => sum + cents, 0), [balances]);

  if (error !== null) {
    return <p className="error">Could not load balances: {error}</p>;
  }
  return (
    <table className="balances">
      <thead>
        <tr>
          <th scope="col">Account</th>
          <th scope="col" className="amount">
            Balance
          </th>
        </tr>
      </thead>
      <tbody>
        {balances.map(({ account, cents }) => (
          <tr key={account} className={cents < 0 ? "negative" : undefined}>
            <td>{account}</td>
            <td className="amount">{format(cents, currency)}</td>
          </tr>
        ))}
      </tbody>
      <tfoot>
        <tr>
          <th scope="row">Total</th>
          <td className="amount">{format(total, currency)}</td>
        </tr>
      </tfoot>
    </table>
  );
}

export function MismatchList({ mismatches }: { mismatches: readonly Mismatch[] }) {
  if (mismatches.length === 0) {
    return <p>Every account reconciles.</p>;
  }
  return (
    <ul className="mismatches">
      {mismatches.map(({ account, expected, actual }) => (
        <li key={account}>
          <strong>{account}</strong>: expected {expected}, found {actual} ({actual - expected})
          {actual > expected && <span className="badge">over</span>}
        </li>
      ))}
    </ul>
  );
}
