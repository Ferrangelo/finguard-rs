// This module is the single point of contact with the Rust backend's JSON
// REST API. Every function here corresponds to one route registered in
// backend/src/main.rs (grep `.route(` there for the current, authoritative
// list); the route list in main.rs is the source of truth for the API
// surface, not this file. Request and response shapes mirror the backend's
// `*Json` DTO structs and payload structs in main.rs. If a call's shape
// changes on either side, update the matching Rust handler/struct and the
// TypeScript types in `./types.ts` together.
import type {
  Categories,
  CategoryTotals,
  CreditDebtRow,
  Currency,
  CurrencySettings,
  Expense,
  ExpenseList,
  ExpenseWrite,
  InvestmentAsset,
  InvestmentCategory,
  LiquidityRow,
  MappingRule,
  MonthlyFxRates,
  MonthlySpending,
  NetworthAllocation,
  NetworthEvolution,
  RecurringTemplate,
} from "./types";

/**
 * Shared fetch wrapper for every backend call. On a non-2xx response it
 * reads the body, tries to parse it as `{ error: string }` (the shape every
 * Axum handler returns via `AppError`, see backend/src/http_error.rs), and
 * throws an `Error` with that message. Falls back to the raw response text,
 * then to `HTTP <status>`, if the body is not that shape. On success it
 * returns the parsed JSON body, or `undefined` if the response has no JSON
 * content type (used by endpoints that reply with an empty body, such as
 * deletes).
 */
async function apiFetch<T>(url: string, options?: RequestInit): Promise<T> {
  const res = await fetch(url, options);
  if (!res.ok) {
    const text = await res.text();
    let message = text;
    try {
      const parsed = JSON.parse(text) as { error?: string };
      if (parsed && typeof parsed.error === "string") message = parsed.error;
    } catch {
      // Not JSON (or empty body) - fall back to the raw text below.
    }
    throw new Error(message || `HTTP ${res.status}`);
  }
  const ct = res.headers.get("content-type") ?? "";
  if (ct.includes("application/json")) return res.json() as Promise<T>;
  return undefined as unknown as T;
}

/** GET /api/years. Lists the years that have any stored data on disk. */
export async function listYears(): Promise<number[]> {
  return apiFetch("/api/years");
}

// No-op: the Rust backend auto-initialises data files on first access.
export async function ensureYear(_year: number): Promise<void> {}

export interface ExpenseFilter {
  name?: string;
  category?: string;
  min?: number;
  max?: number;
}

/**
 * GET /api/expenses. Fetches expenses for one year, optionally scoped to a
 * single `month` and filtered by name substring, category, and amount
 * range. Without `month`, the backend loops over all 12 months and
 * concatenates their per-month expense files, skipping months whose data
 * file does not exist rather than erroring. `id` in each returned `Expense`
 * is only unique within its own year and month: it is the row's position in
 * that month's Parquet file, not a globally unique identifier.
 *
 * A currency that could not be resolved does not drop its rows or fail the
 * request: they come back with `fx_rate: 0` (see `ExpenseList`), and the
 * currency is named in the response's `unavailable_currencies`.
 */
export async function getExpenses(
  year: number,
  month?: number,
  filter?: ExpenseFilter,
): Promise<ExpenseList> {
  const p = new URLSearchParams({ year: String(year) });
  if (month != null) p.set("month", String(month));
  if (filter?.name) p.set("name", filter.name);
  if (filter?.category) p.set("category", filter.category);
  if (filter?.min != null) p.set("min", String(filter.min));
  if (filter?.max != null) p.set("max", String(filter.max));
  return apiFetch(`/api/expenses?${p}`);
}

/**
 * Convenience wrapper around `getExpenses` for the current calendar year
 * only. Despite the name, it does not fetch every year the backend has
 * stored; callers that need historical years must call `getExpenses`
 * directly with that year. Drops `unavailable_currencies` from the result,
 * so a caller that needs to warn about a degraded currency should call
 * `getExpenses` directly instead of this wrapper.
 */
export async function getAllExpenses(): Promise<Expense[]> {
  return (await getExpenses(new Date().getFullYear())).expenses;
}

/**
 * POST /api/expenses. Creates a new expense when `input.id` is missing or
 * empty, or edits the existing row at that index when it is set. The
 * backend infers create vs. edit from whether `id` is empty, so this
 * function always sends an `id` field (defaulting to `""`) rather than
 * omitting it. The row still saves even when its currency's rate cannot be
 * resolved; the returned `Expense` then carries `fx_rate: 0` and
 * `rate_date: ""`, the same "unknown" pair `getExpenses` uses.
 */
export async function upsertExpense(input: ExpenseWrite): Promise<Expense> {
  return apiFetch("/api/expenses", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...input, id: input.id ?? "" }),
  });
}

/**
 * DELETE /api/expenses/:id. `year` and `month` are required query
 * parameters because `id` alone (a per-month row index) does not identify
 * which month's file to edit.
 */
export async function deleteExpense(id: string, year: number, month: number): Promise<void> {
  const p = new URLSearchParams({ year: String(year), month: String(month) });
  await apiFetch(`/api/expenses/${encodeURIComponent(id)}?${p}`, { method: "DELETE" });
}

/** GET /api/recurring. Lists the recurring expense templates configured for a year. `id` is the template's row index within that year's file. */
export async function getRecurring(year: number): Promise<RecurringTemplate[]> {
  return apiFetch(`/api/recurring?year=${year}`);
}

/** POST /api/recurring. Adds a new recurring template for `t.year`. The backend assigns the new `id`. */
export async function addRecurring(
  t: Omit<RecurringTemplate, "id"> & { year: number },
): Promise<RecurringTemplate> {
  return apiFetch("/api/recurring", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(t),
  });
}

/** DELETE /api/recurring/:id. `year` scopes which year's template file to edit. */
export async function deleteRecurring(id: string, year: number): Promise<void> {
  await apiFetch(`/api/recurring/${encodeURIComponent(id)}?year=${year}`, { method: "DELETE" });
}

/**
 * POST /api/recurring/apply. Materializes every recurring template for
 * `year` into that year and month's expense file (skipping any template
 * already applied that month) and returns the count of expenses added.
 */
export async function applyRecurring(year: number, month: number): Promise<number> {
  return apiFetch("/api/recurring/apply", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, month }),
  });
}

// The backend's MappingRuleJson names the pattern field `match_str` (`match`
// is a Rust keyword). This local type and the two functions below translate
// between that wire shape and the frontend's `MappingRule.match` field.
type BackendMapping = { id: string; match_str: string; primary: string; secondary: string };

/** GET /api/mappings. Lists every configured name-to-category mapping rule. */
export async function getMappings(): Promise<MappingRule[]> {
  const data: BackendMapping[] = await apiFetch("/api/mappings");
  return data.map((m) => ({
    id: m.id,
    match: m.match_str,
    primary: m.primary,
    secondary: m.secondary,
  }));
}

/**
 * POST /api/mappings. Adds a mapping rule. The backend trims and
 * lowercases `match`, `primary`, and `secondary` before storing them, so
 * the returned rule's fields may differ in case or whitespace from what was
 * submitted; `id` is set to the normalized `match` string, since mappings
 * are keyed by their match pattern.
 */
export async function addMapping(m: Omit<MappingRule, "id">): Promise<MappingRule> {
  const data: BackendMapping = await apiFetch("/api/mappings", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      id: "",
      match_str: m.match,
      primary: m.primary,
      secondary: m.secondary,
    }),
  });
  return { id: data.id, match: data.match_str, primary: data.primary, secondary: data.secondary };
}

/** DELETE /api/mappings/:id. `id` is the mapping's match string (see `addMapping`). */
export async function deleteMapping(id: string): Promise<void> {
  await apiFetch(`/api/mappings/${encodeURIComponent(id)}`, { method: "DELETE" });
}

/**
 * Matches `name` (normalized with trim and lowercase) to a mapping rule's
 * `match` field. This mirrors the backend's exact-match lookup in
 * `config::get_mapping` (backend/src/config.rs) to ensure the UI suggests
 * only mappings the backend will actually apply.
 */
export function lookupMapping(name: string, rules: MappingRule[]): MappingRule | undefined {
  const n = name.trim().toLowerCase();
  return rules.find((r) => r.match === n);
}

/** GET /api/categories. Returns the full set of known primary and secondary expense categories. */
export async function getCategories(): Promise<Categories> {
  return apiFetch("/api/categories");
}

/** POST /api/categories/:kind. Registers a new known category name and returns the updated category set. */
export async function addCategory(
  kind: "primary" | "secondary",
  name: string,
): Promise<Categories> {
  return apiFetch(`/api/categories/${kind}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ kind, name }),
  });
}

/**
 * DELETE /api/categories/:kind/:name. Fails with a 400 (surfaced as a
 * thrown `Error`) if any stored expense still totals a nonzero amount in
 * this category across all years: the backend computes the category's
 * all-time total first and refuses the delete rather than orphaning
 * existing expense rows. Also fails with a 400 while `name`'s own rows
 * include an unresolved currency (see `getCategoryTotals`'s
 * `unavailable_currencies_by_category`), since a degraded total for this
 * category could hide expenses that still use it; another category's
 * unresolved currency does not block this one. Callers should check
 * `getCategoryTotals` before offering the delete rather than relying on this
 * call to report that state.
 */
export async function deleteCategory(
  kind: "primary" | "secondary",
  name: string,
): Promise<Categories> {
  return apiFetch(`/api/categories/${kind}/${encodeURIComponent(name)}`, { method: "DELETE" });
}

/**
 * GET /api/categories/totals. Sums every expense's reference-currency amount
 * by category name (of the given kind) across all years. A category whose
 * rows are all in an unresolved currency is absent from `totals` rather than
 * present at `0` (see `CategoryTotals`). `unavailable_currencies` nonempty
 * means some total on this page is a lower bound; `unavailable_currencies_by_category`
 * says exactly which ones, keyed by the same raw name used in `totals`.
 * `deleteCategory` refuses only when the category being deleted appears in
 * that per-category map, so gate a delete action on that entry, not on
 * `unavailable_currencies`.
 */
export async function getCategoryTotals(kind: "primary" | "secondary"): Promise<CategoryTotals> {
  return apiFetch(`/api/categories/totals?kind=${kind}`);
}

/**
 * GET /api/cashflow/income. Returns income entered for `year`, keyed by
 * month number (1 to 12, always present for all 12 months) and then by
 * income category name (one of the four `IncomeCategory` values, always
 * present, defaulting to 0 when unset).
 */
export async function getIncome(year: number): Promise<Record<number, Record<string, number>>> {
  return apiFetch(`/api/cashflow/income?year=${year}`);
}

/** POST /api/cashflow/income. Sets one month/category income cell. `category` must be one of the `IncomeCategory` values or the backend rejects it. */
export async function setIncomeCell(
  year: number,
  month: number,
  category: string,
  amount: number,
): Promise<void> {
  await apiFetch("/api/cashflow/income", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, month, category, amount }),
  });
}

/**
 * GET /api/cashflow/spending. Returns total spending per primary category
 * for `year`, keyed by month (1 to 12, always present) and then by category
 * name; a category with no recorded spending that month is simply absent
 * from the month's map rather than present with a 0. Computed at read time
 * from `year`'s expense rows, each converted at its own date's rate. A
 * category/month combination whose rows are all in an unresolved currency is
 * excluded the same way (see `MonthlySpending`), so `unavailable_currencies`
 * nonempty means every reported amount is a lower bound.
 */
export async function getMonthlySpendingByPrimary(year: number): Promise<MonthlySpending> {
  return apiFetch(`/api/cashflow/spending?year=${year}`);
}

/** GET /api/investments. Lists every investment asset with its qty/price data for `year` (see `InvestmentAsset.data`). */
export async function getInvestments(year: number): Promise<InvestmentAsset[]> {
  return apiFetch(`/api/investments?year=${year}`);
}

/**
 * POST /api/investments. Creates a new asset in `year` (defaulting to the
 * current year) with all 12 months at qty 0, price 0. `currency` falls back
 * to the reference currency in the backend when omitted.
 */
export async function addInvestment(
  name: string,
  category: InvestmentCategory,
  link?: string,
  year?: number,
  currency?: Currency,
): Promise<InvestmentAsset> {
  return apiFetch("/api/investments", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      name,
      category,
      link,
      year: year ?? new Date().getFullYear(),
      currency,
    }),
  });
}

/**
 * PUT /api/investments/:id. Updates an asset's name, category, link, and/or
 * currency. `id` is the asset's current name. If `patch.name` differs from
 * `id`, the backend renames the asset first and then applies the other
 * changes under the new name, so a rename and any other field change can be
 * sent together in one call.
 */
export async function updateInvestmentMeta(
  id: string,
  patch: Partial<Pick<InvestmentAsset, "name" | "category" | "link" | "currency">>,
  year: number,
): Promise<void> {
  await apiFetch(`/api/investments/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, ...patch }),
  });
}

/** DELETE /api/investments/:id. Removes the asset entirely from `year`'s holdings file. */
export async function deleteInvestment(id: string, year: number): Promise<void> {
  await apiFetch(`/api/investments/${encodeURIComponent(id)}?year=${year}`, { method: "DELETE" });
}

/** POST /api/investments/cell. Sets a single month's quantity or price for one asset. */
export async function setInvestmentCell(
  id: string,
  year: number,
  month: number,
  field: "qty" | "price",
  value: number,
): Promise<void> {
  // The backend accepts "quantity" or "price"; "qty" is only the in-memory field name.
  const wireField = field === "qty" ? "quantity" : field;
  await apiFetch("/api/investments/cell", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, year, month, field: wireField, value }),
  });
}

/** GET /api/liquidity. Lists every cash/bank row with its monthly balances for `year` (see `LiquidityRow.data`). */
export async function getLiquidity(year: number): Promise<LiquidityRow[]> {
  return apiFetch(`/api/liquidity?year=${year}`);
}

/** POST /api/liquidity. Creates a new liquidity row in `year` with all 12 months at balance 0. */
export async function addLiquidity(
  name: string,
  category: LiquidityRow["category"],
  currency: Currency,
  year: number,
): Promise<LiquidityRow> {
  return apiFetch("/api/liquidity", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, name, category, currency }),
  });
}

/**
 * PUT /api/liquidity/:id. Updates a row's name, category, and/or currency.
 * `id` is the row's current name; as with `updateInvestmentMeta`, a rename
 * is applied before the category/currency changes, so all three can be
 * sent in one call.
 */
export async function updateLiquidityMeta(
  id: string,
  patch: Partial<Pick<LiquidityRow, "name" | "category" | "currency">>,
  year: number,
): Promise<void> {
  await apiFetch(`/api/liquidity/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, ...patch }),
  });
}

/** DELETE /api/liquidity/:id. Removes the row entirely from `year`'s liquidity file. */
export async function deleteLiquidity(id: string, year: number): Promise<void> {
  await apiFetch(`/api/liquidity/${encodeURIComponent(id)}?year=${year}`, { method: "DELETE" });
}

/** POST /api/liquidity/cell. Sets a single month's balance for one row. */
export async function setLiquidityCell(
  id: string,
  year: number,
  month: number,
  value: number,
): Promise<void> {
  await apiFetch("/api/liquidity/cell", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, year, month, value }),
  });
}

/** GET /api/credits_debts. Lists every credit/debt row with its monthly balances for `year` (see `CreditDebtRow.data`). */
export async function getCreditsDebts(year: number): Promise<CreditDebtRow[]> {
  return apiFetch(`/api/credits_debts?year=${year}`);
}

/** POST /api/credits_debts. Creates a new credit/debt row in `year` with all 12 months at balance 0. */
export async function addCreditDebt(
  name: string,
  currency: Currency,
  year: number,
): Promise<CreditDebtRow> {
  return apiFetch("/api/credits_debts", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, name, currency }),
  });
}

/**
 * PUT /api/credits_debts/:id. Updates a row's name and/or currency. `id` is
 * the row's current name; as with `updateInvestmentMeta`, a rename is
 * applied before the currency change.
 */
export async function updateCreditDebtMeta(
  id: string,
  patch: Partial<Pick<CreditDebtRow, "name" | "currency">>,
  year: number,
): Promise<void> {
  await apiFetch(`/api/credits_debts/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, ...patch }),
  });
}

/** DELETE /api/credits_debts/:id. Removes the row entirely from `year`'s credits/debts file. */
export async function deleteCreditDebt(id: string, year: number): Promise<void> {
  await apiFetch(`/api/credits_debts/${encodeURIComponent(id)}?year=${year}`, { method: "DELETE" });
}

/** POST /api/credits_debts/cell. Sets a single month's balance for one row. */
export async function setCreditDebtCell(
  id: string,
  year: number,
  month: number,
  value: number,
): Promise<void> {
  await apiFetch("/api/credits_debts/cell", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, year, month, value }),
  });
}

/** GET /api/settings/currency. Returns the reference currency and in-progress-month rate mode. */
export async function getCurrencySettings(): Promise<CurrencySettings> {
  return apiFetch("/api/settings/currency");
}

/**
 * PUT /api/settings/currency. Replaces both settings and returns the saved
 * values. Throws (via `apiFetch`) on a 400 when `settings.reference_currency`
 * is outside the five supported codes; nothing is persisted in that case.
 */
export async function updateCurrencySettings(
  settings: CurrencySettings,
): Promise<CurrencySettings> {
  return apiFetch("/api/settings/currency", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(settings),
  });
}

/**
 * GET /api/fx/monthly-rates. Resolves one rate per currency per calendar
 * month of `year`, converting *into* the reference currency (see
 * `MonthlyFxRate.rate_to_reference` for the exact direction). `currencies`
 * asks for extra codes beyond whatever `year`'s investment, liquidity, and
 * credits/debts rows already use; omit it to get exactly those.
 */
export async function getMonthlyFxRates(
  year: number,
  currencies?: string[],
): Promise<MonthlyFxRates> {
  const p = new URLSearchParams({ year: String(year) });
  if (currencies && currencies.length > 0) p.set("currencies", currencies.join(","));
  return apiFetch(`/api/fx/monthly-rates?${p}`);
}

/**
 * GET /api/networth/evolution. The net-worth evolution line chart for
 * `year`, with every row already converted into the reference currency
 * month by month. Resolves to `null` when every net-worth value is zero.
 * See `NetworthEvolution.unavailable_currencies` for currencies the backend
 * had to drop from the figures because they would not convert.
 */
export async function getNetworthEvolution(year: number): Promise<NetworthEvolution | null> {
  return apiFetch(`/api/networth/evolution?year=${year}`);
}

/**
 * GET /api/networth/allocation. The net-worth allocation pie chart for
 * `year`/`month`, with every row already converted into the reference
 * currency. Resolves to `null` when no slice qualifies. See
 * `NetworthAllocation.unavailable_currencies` for currencies the backend
 * had to drop from the slices because they would not convert.
 */
export async function getNetworthAllocation(
  year: number,
  month: number,
): Promise<NetworthAllocation | null> {
  return apiFetch(`/api/networth/allocation?year=${year}&month=${month}`);
}

// Full and abbreviated month labels, indexed 0 (January) to 11 (December).
// Backend month numbers are 1-based, so callers index these with `month - 1`.
export const MONTHS = [
  "January",
  "February",
  "March",
  "April",
  "May",
  "June",
  "July",
  "August",
  "September",
  "October",
  "November",
  "December",
] as const;
export const MONTHS_SHORT = [
  "Jan",
  "Feb",
  "Mar",
  "Apr",
  "May",
  "Jun",
  "Jul",
  "Aug",
  "Sep",
  "Oct",
  "Nov",
  "Dec",
] as const;
