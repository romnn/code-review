// The web view of the ledger.

export interface Balance {
  account: string;
  cents: number;
}

export function formatBalance(balance: Balance, currency = "EUR"): string {
  const amount = (balance.cents / 100).toFixed(2);
  return `${balance.account}: ${amount} ${currency}`;
}

export function total(balances: Balance[]): number {
  return balances.reduce((sum, balance) => sum + balance.cents, 0);
}
