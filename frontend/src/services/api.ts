import type {
  Categories, CreditDebtRow, Currency, Expense, InvestmentAsset,
  InvestmentCategory, LiquidityRow, MappingRule, RecurringTemplate,
} from "./types";

async function apiFetch<T>(url: string, options?: RequestInit): Promise<T> {
  const res = await fetch(url, options);
  if (!res.ok) {
    const text = await res.text();
    throw new Error(text || `HTTP ${res.status}`);
  }
  const ct = res.headers.get("content-type") ?? "";
  if (ct.includes("application/json")) return res.json() as Promise<T>;
  return undefined as unknown as T;
}

export async function listYears(): Promise<number[]> {
  return apiFetch("/api/years");
}

// No-op: the Rust backend auto-initialises data files on first access.
export async function ensureYear(_year: number): Promise<void> {}

export interface ExpenseFilter {
  name?: string; category?: string; min?: number; max?: number;
}

export async function getExpenses(year: number, month?: number, filter?: ExpenseFilter): Promise<Expense[]> {
  const p = new URLSearchParams({ year: String(year) });
  if (month != null) p.set("month", String(month));
  if (filter?.name) p.set("name", filter.name);
  if (filter?.category) p.set("category", filter.category);
  if (filter?.min != null) p.set("min", String(filter.min));
  if (filter?.max != null) p.set("max", String(filter.max));
  return apiFetch(`/api/expenses?${p}`);
}

export async function getAllExpenses(): Promise<Expense[]> {
  return getExpenses(new Date().getFullYear());
}

export async function upsertExpense(input: Omit<Expense, "id"> & { id?: string }): Promise<Expense> {
  return apiFetch("/api/expenses", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ...input, id: input.id ?? "" }),
  });
}

export async function deleteExpense(id: string, year: number, month: number): Promise<void> {
  const p = new URLSearchParams({ year: String(year), month: String(month) });
  await apiFetch(`/api/expenses/${encodeURIComponent(id)}?${p}`, { method: "DELETE" });
}

export async function getRecurring(year: number): Promise<RecurringTemplate[]> {
  return apiFetch(`/api/recurring?year=${year}`);
}

export async function addRecurring(
  t: Omit<RecurringTemplate, "id"> & { year: number },
): Promise<RecurringTemplate> {
  return apiFetch("/api/recurring", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(t),
  });
}

export async function deleteRecurring(id: string, year: number): Promise<void> {
  await apiFetch(`/api/recurring/${encodeURIComponent(id)}?year=${year}`, { method: "DELETE" });
}

export async function applyRecurring(year: number, month: number): Promise<number> {
  return apiFetch("/api/recurring/apply", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, month }),
  });
}

// Backend returns { id, match_str, primary, secondary } — map to MappingRule { id, match, primary, secondary }
type BackendMapping = { id: string; match_str: string; primary: string; secondary: string };

export async function getMappings(): Promise<MappingRule[]> {
  const data: BackendMapping[] = await apiFetch("/api/mappings");
  return data.map((m) => ({ id: m.id, match: m.match_str, primary: m.primary, secondary: m.secondary }));
}

export async function addMapping(m: Omit<MappingRule, "id">): Promise<MappingRule> {
  const data: BackendMapping = await apiFetch("/api/mappings", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id: "", match_str: m.match, primary: m.primary, secondary: m.secondary }),
  });
  return { id: data.id, match: data.match_str, primary: data.primary, secondary: data.secondary };
}

export async function deleteMapping(id: string): Promise<void> {
  await apiFetch(`/api/mappings/${encodeURIComponent(id)}`, { method: "DELETE" });
}

export function lookupMapping(name: string, rules: MappingRule[]): MappingRule | undefined {
  const n = name.toLowerCase();
  return rules.find((r) => n.includes(r.match));
}

export async function getCategories(): Promise<Categories> {
  return apiFetch("/api/categories");
}

export async function addCategory(kind: "primary" | "secondary", name: string): Promise<Categories> {
  return apiFetch(`/api/categories/${kind}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ kind, name }),
  });
}

export async function deleteCategory(kind: "primary" | "secondary", name: string): Promise<Categories> {
  return apiFetch(`/api/categories/${kind}/${encodeURIComponent(name)}`, { method: "DELETE" });
}

export async function getCategoryTotals(kind: "primary" | "secondary"): Promise<Record<string, number>> {
  return apiFetch(`/api/categories/totals?kind=${kind}`);
}

export async function getIncome(year: number): Promise<Record<number, Record<string, number>>> {
  return apiFetch(`/api/cashflow/income?year=${year}`);
}

export async function setIncomeCell(year: number, month: number, category: string, amount: number): Promise<void> {
  await apiFetch("/api/cashflow/income", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, month, category, amount }),
  });
}

export async function getMonthlySpendingByPrimary(year: number): Promise<Record<number, Record<string, number>>> {
  return apiFetch(`/api/cashflow/spending?year=${year}`);
}

export async function getInvestments(year: number): Promise<InvestmentAsset[]> {
  return apiFetch(`/api/investments?year=${year}`);
}

export async function addInvestment(
  name: string, category: InvestmentCategory, link?: string, year?: number,
): Promise<InvestmentAsset> {
  return apiFetch("/api/investments", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name, category, link, year: year ?? new Date().getFullYear() }),
  });
}

export async function updateInvestmentMeta(
  id: string,
  patch: Partial<Pick<InvestmentAsset, "name" | "category" | "link">>,
  year: number,
): Promise<void> {
  await apiFetch(`/api/investments/${encodeURIComponent(id)}`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, ...patch }),
  });
}

export async function deleteInvestment(id: string, year: number): Promise<void> {
  await apiFetch(`/api/investments/${encodeURIComponent(id)}?year=${year}`, { method: "DELETE" });
}

export async function setInvestmentCell(
  id: string, year: number, month: number, field: "qty" | "price", value: number,
): Promise<void> {
  await apiFetch("/api/investments/cell", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, year, month, field, value }),
  });
}

export async function getLiquidity(year: number): Promise<LiquidityRow[]> {
  return apiFetch(`/api/liquidity?year=${year}`);
}

export async function addLiquidity(
  name: string, category: LiquidityRow["category"], currency: Currency, year: number,
): Promise<LiquidityRow> {
  return apiFetch("/api/liquidity", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, name, category, currency }),
  });
}

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

export async function deleteLiquidity(id: string, year: number): Promise<void> {
  await apiFetch(`/api/liquidity/${encodeURIComponent(id)}?year=${year}`, { method: "DELETE" });
}

export async function setLiquidityCell(id: string, year: number, month: number, value: number): Promise<void> {
  await apiFetch("/api/liquidity/cell", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, year, month, value }),
  });
}

export async function getCreditsDebts(year: number): Promise<CreditDebtRow[]> {
  return apiFetch(`/api/credits_debts?year=${year}`);
}

export async function addCreditDebt(name: string, currency: Currency, year: number): Promise<CreditDebtRow> {
  return apiFetch("/api/credits_debts", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ year, name, currency }),
  });
}

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

export async function deleteCreditDebt(id: string, year: number): Promise<void> {
  await apiFetch(`/api/credits_debts/${encodeURIComponent(id)}?year=${year}`, { method: "DELETE" });
}

export async function setCreditDebtCell(id: string, year: number, month: number, value: number): Promise<void> {
  await apiFetch("/api/credits_debts/cell", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ id, year, month, value }),
  });
}

export const MONTHS = [
  "January","February","March","April","May","June",
  "July","August","September","October","November","December",
] as const;
export const MONTHS_SHORT = ["Jan","Feb","Mar","Apr","May","Jun","Jul","Aug","Sep","Oct","Nov","Dec"] as const;
