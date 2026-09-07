// These types mirror the JSON DTOs the Rust backend serves from
// backend/src/main.rs (the `*Json` structs near the top of that file). The
// route list in backend/src/main.rs (grep for `.route(`) is the source of
// truth for the API surface. If a field here changes shape, the matching
// Rust struct must change in the same commit, and vice versa.

export type Currency = "EUR" | "USD" | "GBP" | "CHF" | "JPY";

/**
 * Mirrors `ExpenseJson` in backend/src/main.rs. `id` is the backend's
 * stringified row index. `fx_rate` and `rate_date` are read-only: the server
 * always resolves them from `currency` and the expense's own date, and
 * ignores them on write, so a caller building a request should not set them.
 * There is no `expense_in_ref_currency` field on the wire; a caller derives
 * that amount itself as `amount * fx_rate`.
 */
export interface Expense {
  id: string;
  year: number;
  month: number;
  day: number;
  name: string;
  amount: number;
  currency: Currency;
  primary: string;
  secondary: string;
  /** Multiplier that converts `amount` into the reference currency; 1.0 when `currency` already is the reference currency. Read-only. */
  fx_rate: number;
  /** ISO `"YYYY-MM-DD"` date `fx_rate` was published for. Often earlier than the expense's own date, since the ECB publishes on working days only. Read-only. */
  rate_date: string;
}

/**
 * The shape a caller builds to create or update an expense through
 * `upsertExpense` in `services/api.ts`. Omits `id` the same way the old
 * inline `Omit<Expense, "id">` did, and also omits `fx_rate` and
 * `rate_date`: both are resolved by the server from `currency` and the
 * expense's own date on every write, and ignored if sent (see the `Expense`
 * doc comment above). A write-side type that still required them would
 * force a caller to invent values it does not have, so it leaves them out
 * instead of padding them with a placeholder.
 */
export interface ExpenseWrite extends Omit<Expense, "id" | "fx_rate" | "rate_date"> {
  id?: string;
}

/** Mirrors `RecurringTemplateJson` in backend/src/main.rs. Has no `year`: the backend scopes recurring templates by year through query parameters, not through this shape. */
export interface RecurringTemplate {
  id: string;
  name: string;
  day: number;
  amount: number;
  currency: Currency;
  primary: string;
  secondary: string;
}

/**
 * Frontend shape for a category-mapping rule (auto-assigns primary/secondary
 * categories to an expense whose trimmed, lower-cased name exactly equals `match`).
 * The backend's `MappingRuleJson` names this field `match_str` instead of `match`
 * because `match` is a Rust keyword; `services/api.ts` translates between the two
 * shapes on every mapping request.
 */
export interface MappingRule {
  id: string;
  match: string;
  primary: string;
  secondary: string;
}

// Matches `df_operations::INCOME_CATEGORIES` in the backend. The backend
// rejects any other category string for a cashflow income row.
export const INCOME_CATEGORIES = [
  "Salary",
  "Interests Bank account",
  "Dividendi e Cedole",
  "Other",
] as const;
export type IncomeCategory = (typeof INCOME_CATEGORIES)[number];

// Matches `df_operations::INVESTMENT_CATEGORIES` in the backend.
export type InvestmentCategory = "Stocks/ETF" | "Commodities" | "Bonds";

/**
 * Mirrors `InvestmentAssetJson` in backend/src/main.rs. `id` is the asset
 * name (assets are keyed by name, not a generated id). `data` is a
 * year -> month -> { qty, price } table, but a single fetch (`getInvestments`
 * takes one `year`) only ever populates the requested year's key; that one
 * year always has all 12 months present, defaulting each to
 * `{ qty: 0, price: 0 }` when unset. JSON object keys are always strings on
 * the wire; the numeric key types here describe the year and month values
 * after the runtime coerces them back to numbers. `currency` is typed as
 * `Currency` for consistency with `LiquidityRow.currency` and
 * `CreditDebtRow.currency`, but the backend column is a free string: an
 * older row could in principle hold a code outside the union.
 */
export interface InvestmentAsset {
  id: string;
  name: string;
  category: InvestmentCategory;
  link?: string;
  currency: Currency;
  data: Record<number, Record<number, { qty: number; price: number }>>;
}

// Matches `df_operations::LIQUIDITY_CATEGORIES` in the backend.
export type LiquidityCategory = "Bank/Broker account" | "Cash" | "Other";

/**
 * Mirrors `LiquidityRowJson` in backend/src/main.rs. `id` is the row name.
 * `data` has the same one-year-per-fetch, all-12-months-present shape as
 * `InvestmentAsset.data`, defaulting each unset month to 0.
 */
export interface LiquidityRow {
  id: string;
  name: string;
  category: LiquidityCategory;
  currency: Currency;
  data: Record<number, Record<number, number>>;
}

/**
 * Mirrors `CreditDebtRowJson` in backend/src/main.rs. `id` is the row name.
 * `data` (balances, positive or negative) has the same one-year-per-fetch,
 * all-12-months-present shape as `LiquidityRow.data`.
 */
export interface CreditDebtRow {
  id: string;
  name: string;
  currency: Currency;
  data: Record<number, Record<number, number>>;
}

/** Mirrors `CategoriesJson` in backend/src/main.rs: the full set of known primary and secondary expense categories. */
export interface Categories {
  primary: string[];
  secondary: string[];
}

/** Mirrors `config::CurrentMonthRateMode` in backend/src/config.rs. `"previous_month_end"` freezes the in-progress month at the prior month's close; `"live"` always uses the newest published rate. */
export type CurrentMonthRateMode = "previous_month_end" | "live";

/** Mirrors `CurrencySettingsJson` in backend/src/main.rs, the body of `GET`/`PUT /api/settings/currency`. */
export interface CurrencySettings {
  reference_currency: Currency;
  current_month_rate_mode: CurrentMonthRateMode;
}

/**
 * One calendar month's resolved rates in `MonthlyFxRates`, mirroring
 * `MonthlyFxRateJson` in backend/src/main.rs.
 *
 * `rate_to_reference[code]` multiplies an amount **in that currency** by it
 * to reach `MonthlyFxRates.reference_currency`. Converting a
 * reference-currency amount **into** a display currency means **dividing**
 * by this value, not multiplying; getting the direction backward produces a
 * plausible-looking but badly wrong figure. The reference currency itself is
 * never a key, since its rate is implicitly 1.0. Keyed with `string`, not
 * `Currency`: a caller can request any currency code through
 * `getMonthlyFxRates`'s `currencies` argument, not only the five reference
 * currencies.
 */
export interface MonthlyFxRate {
  month: number;
  rate_to_reference: Record<string, number>;
}

/**
 * Mirrors `MonthlyFxRatesJson` in backend/src/main.rs, the body of
 * `GET /api/fx/monthly-rates`. `unavailable_currencies` lists codes that
 * could not be resolved for any month, typically offline with nothing
 * cached; they are omitted from `months` rather than failing the request, so
 * a caller must handle a requested currency being absent.
 */
export interface MonthlyFxRates {
  year: number;
  reference_currency: Currency;
  months: MonthlyFxRate[];
  unavailable_currencies: string[];
}

/** One stacked component of `NetworthEvolution`, mirroring `NetworthSeriesJson` in backend/src/main.rs. */
export interface NetworthSeries {
  name: string;
  values: number[];
}

/**
 * Mirrors the body of `GET /api/networth/evolution`, which serializes as
 * `null` (so `getNetworthEvolution` resolves to `null`) when every net-worth
 * value for the year is zero. Every figure is already in the reference
 * currency.
 */
export interface NetworthEvolution {
  months: string[];
  components: NetworthSeries[];
  net_worth: number[];
}

/** One slice of `NetworthAllocation`, mirroring `NetworthPieSliceJson` in backend/src/main.rs. */
export interface NetworthPieSlice {
  name: string;
  value: number;
}

/**
 * Mirrors the body of `GET /api/networth/allocation`, which serializes as
 * `null` (so `getNetworthAllocation` resolves to `null`) when no slice
 * qualifies. Every figure is already in the reference currency.
 */
export interface NetworthAllocation {
  slices: NetworthPieSlice[];
}

// StatusKind and StatusMessage are frontend-only UI state (shown in the
// header's StatusPill); they have no backend counterpart.
export type StatusKind = "idle" | "loading" | "success" | "error";
/** A transient status notification shown in the header. `ts` records the creation time (`Date.now()`), set by `AppContext`'s `notify`. */
export interface StatusMessage {
  kind: StatusKind;
  text: string;
  ts: number;
}
