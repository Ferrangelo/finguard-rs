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

const NBSP = "\u00A0";
const NNBSP = "\u202F";

/**
 * Rebuilds `Intl.NumberFormat.formatToParts()` output into a string with two
 * fixed-width, non-breaking gaps in place of `en-IE`'s comma and (for some
 * currencies) built-in space: a narrow no-break space between digit groups,
 * matching the narrower gap style used for thousands separators, and a
 * regular non-breaking space between a currency symbol/code and the first
 * digit, wide enough to read as a separate token. `Intl.NumberFormat` has no
 * option that produces this shape directly, and other locales that group
 * with spaces have decimal and sign conventions that are not guaranteed to
 * match `en-IE` across runtimes, so this rebuilds the string from `en-IE`'s
 * own parts instead of switching locale. `en-IE` already inserts a `literal`
 * separator between the currency code and the amount for some currencies,
 * e.g. `"CHF 1,430.78"`; that literal is turned into the currency-to-digit
 * NBSP instead of adding a second one after `currency`, so the gap is always
 * exactly one space wide.
 */
function joinPartsWithNbsp(parts: Intl.NumberFormatPart[]): string {
  let result = "";
  for (let i = 0; i < parts.length; i++) {
    const part = parts[i];
    if (part.type === "group") {
      result += NNBSP;
    } else if (part.type === "literal") {
      result += NBSP;
    } else if (part.type === "currency") {
      result += part.value;
      if (parts[i + 1]?.type !== "literal") result += NBSP;
    } else {
      result += part.value;
    }
  }
  return result;
}

/** Formats `amount` as a localized currency string (`en-IE` locale). The caller must pass the currency `amount` is actually denominated in; there is no default, since the reference currency is a user setting, not always EUR. */
export function formatRef(amount: number, currency: Currency): string {
  const formatter = new Intl.NumberFormat("en-IE", {
    style: "currency",
    currency,
    maximumFractionDigits: 2,
  });
  return joinPartsWithNbsp(formatter.formatToParts(amount));
}

/** Formats `amount` as a compact currency string (e.g. "€1.2K") for space-constrained labels. The caller must pass the currency `amount` is actually denominated in; there is no default, since the reference currency is a user setting, not always EUR. */
export function formatCompact(amount: number, currency: Currency): string {
  const formatter = new Intl.NumberFormat("en-IE", {
    style: "currency",
    currency,
    notation: "compact",
    maximumFractionDigits: 1,
  });
  return joinPartsWithNbsp(formatter.formatToParts(amount));
}

/** Formats a plain number with a non-breaking space thousands separator, for space-constrained numeric labels (e.g. chart tooltips) that carry no currency symbol. */
export function formatNumberGrouped(value: number): string {
  return joinPartsWithNbsp(new Intl.NumberFormat("en-IE").formatToParts(value));
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

/**
 * Converts `amount`, denominated in a given currency, into the reference
 * currency, given that currency's `rate_to_reference` from
 * `getMonthlyFxRates`. This is the *inverse* direction of `referenceToDisplay`
 * above, which divides a reference-currency amount to reach a display
 * currency: do not confuse the two, since swapping them silently produces a
 * plausible-looking but wrong figure. Returns `null` instead of `Infinity` or
 * `NaN` when `rate` is missing, zero, or not finite, so a caller can render a
 * fallback rather than a nonsense money figure.
 */
export function nativeToReference(amount: number, rate: number | undefined): number | null {
  if (rate === undefined || !Number.isFinite(rate) || rate === 0) return null;
  return amount * rate;
}

export const CURRENCIES: Currency[] = ["EUR", "USD", "GBP", "CHF", "JPY"];
