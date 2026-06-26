export type Currency = "EUR" | "USD" | "GBP" | "CHF" | "JPY";

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

export interface RecurringTemplate {
  id: string;
  name: string;
  day: number;
  amount: number;
  currency: Currency;
  primary: string;
  secondary: string;
}

export interface MappingRule {
  id: string;
  match: string;
  primary: string;
  secondary: string;
}

export const INCOME_CATEGORIES = [
  "Salary",
  "Interests Bank account",
  "Dividendi e Cedole",
  "Other",
] as const;
export type IncomeCategory = (typeof INCOME_CATEGORIES)[number];

export type InvestmentCategory = "Stocks/ETF" | "Commodities" | "Bonds";
export interface InvestmentAsset {
  id: string;
  name: string;
  category: InvestmentCategory;
  link?: string;
  data: Record<number, Record<number, { qty: number; price: number }>>;
}

export type LiquidityCategory = "Bank/Broker account" | "Cash" | "Other";
export interface LiquidityRow {
  id: string;
  name: string;
  category: LiquidityCategory;
  currency: Currency;
  data: Record<number, Record<number, number>>;
}

export interface CreditDebtRow {
  id: string;
  name: string;
  currency: Currency;
  data: Record<number, Record<number, number>>;
}

export interface Categories {
  primary: string[];
  secondary: string[];
}

export type StatusKind = "idle" | "loading" | "success" | "error";
export interface StatusMessage {
  kind: StatusKind;
  text: string;
  ts: number;
}