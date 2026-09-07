// Currency formatting and reference-to-display conversion. The backend now
// resolves the real fx rate behind every convertible amount (see
// `Expense.fx_rate` and `MonthlyFxRate.rate_to_reference` in
// `services/types.ts`), so this file no longer fakes conversion with an
// identity table. A caller that has an amount already in the reference
// currency and a `rate_to_reference` value uses `referenceToDisplay` below to
// show it in another currency; a caller that has a raw expense amount and its
// own `fx_rate` computes `amount * fx_rate` directly, per the contract
// documented on `Expense`.
import type { Currency } from "./types";

/** Formats `amount` as a localized currency string (`en-IE` locale). The caller must pass the currency `amount` is actually denominated in; there is no default, since the reference currency is a user setting, not always EUR. */
export function formatRef(amount: number, currency: Currency): string {
  return new Intl.NumberFormat("en-IE", {
    style: "currency",
    currency,
    maximumFractionDigits: 2,
  }).format(amount);
}

/** Formats `amount` as a compact currency string (e.g. "€1.2K") for space-constrained labels. The caller must pass the currency `amount` is actually denominated in; there is no default, since the reference currency is a user setting, not always EUR. */
export function formatCompact(amount: number, currency: Currency): string {
  return new Intl.NumberFormat("en-IE", {
    style: "currency",
    currency,
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(amount);
}

/**
 * Converts `amountInReference`, an amount already in the reference
 * currency, into a display currency, given that currency's
 * `rate_to_reference` from `getMonthlyFxRates`. `rate_to_reference`
 * multiplies an amount in the display currency to reach the reference
 * currency, so going the other way divides. Returns `null` instead of
 * `Infinity` or `NaN` when `rate` is missing, zero, or not finite, so a
 * caller can render a fallback rather than a nonsense money figure.
 */
export function referenceToDisplay(
  amountInReference: number,
  rate: number | undefined,
): number | null {
  if (rate === undefined || !Number.isFinite(rate) || rate === 0) return null;
  return amountInReference / rate;
}

export const CURRENCIES: Currency[] = ["EUR", "USD", "GBP", "CHF", "JPY"];
