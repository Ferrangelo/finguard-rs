// Client-side currency conversion and formatting for display purposes only
// (charts, totals across mixed currencies). Conversion rates are fixed
// constants, not fetched from any live source or from the backend, so they
// drift from real exchange rates over time. The backend's own reference-
// currency conversion (`convert_in_ref_currency` in
// backend/src/df_operations.rs) is a rate-1.0 stub that does not actually
// convert, so backend-computed totals (e.g. category totals, cashflow
// summaries) do not currency-convert; only this module's client-side totals
// do, so a mixed-currency total shown in the UI can differ from the
// backend's own aggregates for the same data.
import type { Currency } from "./types";

// Fixed EUR-referenced conversion rates. EUR is the reference currency
// (rate 1); every other rate is "how many EUR one unit of that currency is
// worth", so `amount * FX[currency]` converts into EUR.
export const FX: Record<Currency, number> = {
  EUR: 1,
  USD: 0.92,
  GBP: 1.17,
  CHF: 1.04,
  JPY: 0.0061,
};

/** Converts `amount` from `currency` into the EUR reference currency using the fixed `FX` rates. */
export function toRef(amount: number, currency: Currency): number {
  return amount * (FX[currency] ?? 1);
}

/** Formats `amount` as a localized currency string (`en-IE` locale), EUR by default. */
export function formatRef(amount: number, currency: Currency = "EUR"): string {
  return new Intl.NumberFormat("en-IE", {
    style: "currency",
    currency,
    maximumFractionDigits: 2,
  }).format(amount);
}

/** Formats `amount` as a compact EUR string (e.g. "€1.2K") for space-constrained labels. */
export function formatCompact(amount: number): string {
  return new Intl.NumberFormat("en-IE", {
    style: "currency",
    currency: "EUR",
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(amount);
}

export const CURRENCIES: Currency[] = ["EUR", "USD", "GBP", "CHF", "JPY"];