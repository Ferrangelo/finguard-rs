// These types mirror the JSON DTOs the Rust backend serves from
// backend/src/api.rs (the `*Json` structs near the top of that file). The
// route list in backend/src/api.rs (grep for `.route(`) is the source of
// truth for the API surface. If a field here changes shape, the matching
// Rust struct must change in the same commit, and vice versa.

export type Currency = "EUR" | "USD" | "GBP" | "CHF" | "JPY";

/**
 * Mirrors `ExpenseJson` in backend/src/api.rs. `id` is a stable,
 * backend-assigned opaque string ID. Do not parse it. `fx_rate` and
 * `rate_date` are read-only: the server
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

/**
 * Mirrors `ExpenseListJson`, the body of `GET /api/expenses`. A currency
 * that could not be resolved does not drop its rows: they stay in
 * `expenses` with `fx_rate: 0` and `rate_date: ""` (see `Expense`), and the
 * currency is named in `unavailable_currencies`. `fx_rate` is never
 * legitimately `0` for a resolved row, so a caller can treat `fx_rate === 0`
 * as "no rate for this row" without cross-checking `unavailable_currencies`
 * first.
 */
export interface ExpenseList {
  expenses: Expense[];
  unavailable_currencies: string[];
}

/** Mirrors `RecurringTemplateJson` in backend/src/api.rs. Has no `year`: the backend scopes recurring templates by year through query parameters, not through this shape. */
export interface RecurringTemplate {
  id: string;
  name: string;
  /**
   * `getRecurring`'s response reflects the template's real stored day: 1 to
   * 28 for a template created before this change, always 1 for one created
   * after. `addRecurring`'s response ignores whatever day was submitted and
   * always reports 1, the day every generated row now lands on regardless of
   * the template's stored value.
   */
  day: number;
  amount: number;
  currency: Currency;
  primary: string;
  secondary: string;
}

/**
 * The shape a caller builds to create a recurring template through
 * `addRecurring` in `services/api.ts`. Omits `id`, which the server assigns,
 * and `day`, which the server always sets to 1 and ignores on write (see
 * `RecurringTemplate.day`), the same way `ExpenseWrite` omits `fx_rate` and
 * `rate_date`. Adds `year`, which scopes the new template but is not part of
 * `RecurringTemplate` itself.
 */
export interface RecurringTemplateWrite extends Omit<RecurringTemplate, "id" | "day"> {
  year: number;
}

/**
 * Mirrors `ApplyRecurringResultJson` in backend/src/api.rs, the body of
 * `POST /api/recurring/apply`. `added` counts the rows the call created.
 * `skipped` lists the templates it refused to generate because the user's
 * deletion of that generated row still stands, and is empty in the ordinary
 * case; a template whose row is already in the month is neither added nor
 * skipped.
 */
export interface ApplyRecurringResult {
  added: number;
  skipped: SkippedRecurring[];
}

/**
 * Mirrors `SkippedRecurringJson` in backend/src/api.rs: one row
 * `POST /api/recurring/apply` withheld, described well enough for the user
 * to recognize the expense. `template_id` is the same value as
 * `RecurringTemplate.id` and is what `reinstateRecurring` in
 * `services/api.ts` takes. `row_id` is the ID the row would have had; it is
 * unique within one response, so it also serves as a list key. `day` is
 * server-derived and always 1: the backend generates every row on day 1 of
 * the month. `currency` is typed as
 * `Currency` to match `RecurringTemplate.currency`, but the backend column is
 * a free string, so an older template could in principle carry a code outside
 * the union.
 */
export interface SkippedRecurring {
  template_id: string;
  row_id: string;
  name: string;
  day: number;
  amount: number;
  currency: Currency;
  primary: string;
  secondary: string;
}

/**
 * Mirrors `ReinstatedRowJson` in backend/src/api.rs, the body of
 * `POST /api/recurring/reinstate`. `created` is `false` when the month
 * already held the row and nothing was written, which is a success: the row
 * is present either way.
 */
export interface ReinstatedRow {
  row_id: string;
  created: boolean;
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

/**
 * Mirrors `MonthlySpendingJson` in backend/src/api.rs, the body of
 * `GET /api/cashflow/spending`. `months` keys every calendar month `1`
 * through `12` (always present, even with no expense rows that month) to a
 * category-name -> reference-currency-amount map. Same lower-bound caveat as
 * `CategoryTotals.totals`: a category/month combination whose rows are all
 * in an unresolved currency is absent rather than present at `0`. Note
 * `GET /api/cashflow/income`, which the cashflow page fetches alongside this
 * endpoint, still returns the older bare `Record<month, Record<category,
 * number>>` shape directly, with no `unavailable_currencies` wrapper: do not
 * reuse this type for it.
 */
export interface MonthlySpending {
  months: Record<number, Record<string, number>>;
  unavailable_currencies: string[];
}

// Matches `df_operations::INVESTMENT_CATEGORIES` in the backend.
export type InvestmentCategory = "Stocks/ETF" | "Commodities" | "Bonds";

/**
 * Mirrors `InvestmentAssetJson` in backend/src/api.rs. `id` is the asset
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
 * Mirrors `LiquidityRowJson` in backend/src/api.rs. `id` is the row name.
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
 * Mirrors `CreditDebtRowJson` in backend/src/api.rs. `id` is the row name.
 * `data` (balances, positive or negative) has the same one-year-per-fetch,
 * all-12-months-present shape as `LiquidityRow.data`.
 */
export interface CreditDebtRow {
  id: string;
  name: string;
  currency: Currency;
  data: Record<number, Record<number, number>>;
}

/** Mirrors `CategoriesJson` in backend/src/api.rs: the full set of known primary and secondary expense categories. */
export interface Categories {
  primary: string[];
  secondary: string[];
}

/**
 * Mirrors `CategoryTotalsJson` in backend/src/api.rs, the body of
 * `GET /api/categories/totals`. `totals` sums every expense's
 * reference-currency amount by category name of the requested kind. A
 * category whose rows are all in an unresolved currency is absent from
 * `totals` rather than present at `0`.
 *
 * `unavailable_currencies` covers every category of the requested kind, so
 * it answers "is anything on this page incomplete", not "is this particular
 * total incomplete"; use `unavailable_currencies_by_category` for the
 * per-category answer, keyed by the same raw category name used in
 * `totals`. `DELETE /api/categories/:kind/:name` refuses to delete only when
 * the deleted category's own name appears there, so a client offering a
 * delete should gate on that entry rather than on `unavailable_currencies`.
 */
export interface CategoryTotals {
  totals: Record<string, number>;
  unavailable_currencies: string[];
  unavailable_currencies_by_category: Record<string, string[]>;
}

/** Mirrors `config::CurrentMonthRateMode` in backend/src/config.rs. `"previous_month_end"` freezes the in-progress month at the prior month's close; `"live"` always uses the newest published rate. */
export type CurrentMonthRateMode = "previous_month_end" | "live";

/** Mirrors `CurrencySettingsJson` in backend/src/api.rs, the body of `GET`/`PUT /api/settings/currency`. */
export interface CurrencySettings {
  reference_currency: Currency;
  current_month_rate_mode: CurrentMonthRateMode;
}

/**
 * One calendar month's resolved rates in `MonthlyFxRates`, mirroring
 * `MonthlyFxRateJson` in backend/src/api.rs.
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
 * Mirrors `MonthlyFxRatesJson` in backend/src/api.rs, the body of
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

/** One stacked component of `NetworthEvolution`, mirroring `NetworthSeriesJson` in backend/src/api.rs. */
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
  /**
   * Currencies used by the year's investment, liquidity, or credit/debt
   * rows that could not be resolved into the reference currency; the
   * backend excludes them from every component series and `net_worth`
   * rather than failing the whole request. Empty when everything resolved,
   * and the reference currency itself never appears here. Optional because
   * older backend responses may not send it yet: a caller must treat an
   * absent field the same as an empty array, not as "everything resolved."
   */
  unavailable_currencies?: string[];
}

/** One slice of `NetworthAllocation`, mirroring `NetworthPieSliceJson` in backend/src/api.rs. */
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
  /** Same meaning and same optionality caveat as `NetworthEvolution.unavailable_currencies`. */
  unavailable_currencies?: string[];
}

export interface SyncPeer {
  device_id: string;
  role: string;
  paired_at_ms: number;
  key_fingerprint: string | null;
  address: string | null;
}

export interface SyncLogHealth {
  reliable: boolean;
  problems: string[];
  clock_ahead_hours: number | null;
  error: string | null;
}

export interface SyncListener {
  listening: boolean;
  port: number;
  address_hint: string | null;
  bind_error: string | null;
  pair_code_expires_at_ms: number | null;
  stops_after_seconds: number;
}

export interface SyncPeerCounts {
  applied: number;
  skipped: number;
  unplaceable: number;
  already_known: number;
}

export interface SyncCounts {
  sent: number;
  received: number;
  applied: number;
  skipped: number;
  unplaceable: number;
  peer: SyncPeerCounts | null;
}

export interface SyncLast {
  finished_at_ms: number;
  peer_device_id: string | null;
  outcome: string;
  plan: string | null;
  counts: SyncCounts;
  error: string | null;
}

export interface SyncStatus {
  role: string;
  device_id: string;
  key_fingerprint: string;
  peers: SyncPeer[];
  peers_error: string | null;
  log_health: SyncLogHealth;
  listener: SyncListener | null;
  last_sync: SyncLast | null;
  settings_sync_pending: string[];
}

export interface SyncPairCode {
  code: string;
  expires_at_ms: number;
  attempts_allowed: number;
}

export interface SyncPairResult {
  hub_device_id: string;
  hub_key_fingerprint: string;
  address: string;
}

/** Mirrors `SyncDiscoveryReplyJson` in api.rs. */
export interface SyncDiscoveryReply {
  address: string;
  device_id: string;
  key_fingerprint: string;
}

export type SyncDiscoveryResult = SyncDiscoveryReply[];

export interface SyncResetPreview {
  rows_per_table: Record<string, number>;
  year_folders: number;
  unreadable_files: number;
  unsent_entries: number;
  settings_entries: number;
}

export interface SyncNowResult {
  outcome: string;
  plan: string;
  push_first: boolean;
  counts: SyncCounts;
  reset_preview: SyncResetPreview | null;
  backup_folder: string | null;
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
