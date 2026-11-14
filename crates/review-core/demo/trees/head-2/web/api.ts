// The ledger's HTTP API, as the web view calls it.

/** An account's id, such as `assets:cash`; branded so a plain string is not taken for one. */
export type AccountId = string & { readonly __brand: "AccountId" };

export const accountId = (raw: string): AccountId => raw as AccountId;

export enum Currency {
  Eur = "EUR",
  Usd = "USD",
  Chf = "CHF",
}

export interface Balance {
  account: AccountId;
  cents: number;
  currency: Currency;
}

export interface Mismatch {
  account: AccountId;
  expected: number;
  actual: number;
}

export interface Page<T> {
  items: readonly T[];
  next?: string;
}

export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

export class LedgerClient {
  readonly #baseUrl: string;

  constructor(
    baseUrl: string,
    private readonly token?: string,
  ) {
    this.#baseUrl = baseUrl.replace(/\/+$/, "");
  }

  async balances(currency = Currency.Eur): Promise<Balance[]> {
    const all: Balance[] = [];
    let cursor: string | undefined;
    do {
      const query = new URLSearchParams({ currency });
      if (cursor) query.set("cursor", cursor);
      const page = await this.request<Page<Balance>>(`/balances?${query}`);
      all.push(...page.items);
      cursor = page.next;
    } while (cursor);
    return all;
  }

  async balance(account: AccountId): Promise<Balance | null> {
    try {
      return await this.request<Balance>(`/accounts/${encodeURIComponent(account)}`);
    } catch (error) {
      if (error instanceof ApiError && error.status === 404) return null;
      throw error;
    }
  }

  async reconcile(expected: ReadonlyMap<AccountId, number>): Promise<Mismatch[]> {
    return this.request<Mismatch[]>("/reconcile", {
      method: "POST",
      body: JSON.stringify(Object.fromEntries(expected)),
      headers: { "Content-Type": "application/json" },
    });
  }

  private async request<T>(path: string, init: RequestInit = {}): Promise<T> {
    const headers = new Headers(init.headers);
    if (this.token) headers.set("Authorization", `Bearer ${this.token}`);
    const response = await fetch(`${this.#baseUrl}${path}`, { ...init, headers });
    if (!response.ok) {
      const method = init.method ?? "GET";
      throw new ApiError(response.status, `${method} ${path}: ${response.statusText}`);
    }
    return (await response.json()) as T;
  }
}
