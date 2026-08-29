// These types mirror the JSON DTOs the Rust backend serves from
// backend/src/main.rs (the `*Json` structs near the top of that file). The
// route list in backend/src/main.rs (grep for `.route(`) is the source of
// truth for the API surface. If a field here changes shape, the matching
// Rust struct must change in the same commit, and vice versa.

export type Currency = "EUR" | "USD" | "GBP" | "CHF" | "JPY";

/** Mirrors `ExpenseJson` in backend/src/main.rs. `id` is the backend's stringified row index. */
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
 * categories to an expense whose name contains `match`). The backend's
 * `MappingRuleJson` names this field `match_str` instead of `match` because
 * `match` is a Rust keyword; `services/api.ts` translates between the two
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
 * after the runtime coerces them back to numbers.
 */
export interface InvestmentAsset {
  id: string;
  name: string;
  category: InvestmentCategory;
  link?: string;
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

// StatusKind and StatusMessage are frontend-only UI state (shown in the
// header's StatusPill); they have no backend counterpart.
export type StatusKind = "idle" | "loading" | "success" | "error";
/** A transient status notification shown in the header. `ts` records the creation time (`Date.now()`), set by `AppContext`'s `notify`. */
export interface StatusMessage {
  kind: StatusKind;
  text: string;
  ts: number;
}