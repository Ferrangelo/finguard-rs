// Currency conversion and formatting for display. No conversion is applied:
// every rate in FX is 1, so toRef returns its input unchanged. The rate table
// and its call sites are kept as the single place to add real rates when live
// conversion lands. The backend does not convert either
// (convert_in_ref_currency in backend/src/df_operations.rs multiplies by 1.0),
// so UI totals match backend aggregates over the same rows.
import type { Currency } from "./types";

// Identity rates. Every currency is 1, so no conversion happens. Real rates,
// fetched or backend-supplied, go here.
export const FX: Record<Currency, number> = {
  EUR: 1,
  USD: 1,
  GBP: 1,
  CHF: 1,
  JPY: 1,
};

/** Returns `amount` unchanged, because every `FX` rate is 1. Kept as the extension point for real conversion. */
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
