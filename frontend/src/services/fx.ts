import type { Currency } from "./types";

export const FX: Record<Currency, number> = {
  EUR: 1,
  USD: 0.92,
  GBP: 1.17,
  CHF: 1.04,
  JPY: 0.0061,
};

export function toRef(amount: number, currency: Currency): number {
  return amount * (FX[currency] ?? 1);
}

export function formatRef(amount: number, currency: Currency = "EUR"): string {
  return new Intl.NumberFormat("en-IE", {
    style: "currency",
    currency,
    maximumFractionDigits: 2,
  }).format(amount);
}

export function formatCompact(amount: number): string {
  return new Intl.NumberFormat("en-IE", {
    style: "currency",
    currency: "EUR",
    notation: "compact",
    maximumFractionDigits: 1,
  }).format(amount);
}

export const CURRENCIES: Currency[] = ["EUR", "USD", "GBP", "CHF", "JPY"];