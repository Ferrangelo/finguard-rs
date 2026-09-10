import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useMemo, useState } from "react";
import {
  BarChart,
  Bar,
  CartesianGrid,
  Cell,
  Legend,
  Line,
  LineChart,
  Pie,
  PieChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import { Plus, Play, Pencil, X, ArrowUp, ArrowDown } from "lucide-react";
import { useApp } from "@/context/AppContext";
import * as api from "@/services/api";
import { MONTHS, MONTHS_SHORT } from "@/services/api";
import { CURRENCIES, formatRef } from "@/services/fx";
import { evalMath } from "@/services/mathEval";
import { GlassCard } from "@/components/finguard/GlassCard";
import { SubTabs } from "@/components/finguard/SubTabs";
import { Combobox } from "@/components/finguard/Combobox";
import { ConfirmButton } from "@/components/finguard/ConfirmButton";
import { useChartColors, LEGEND_STYLE } from "@/components/finguard/DarkTooltip";
import type {
  Categories,
  Currency,
  Expense,
  ExpenseWrite,
  MappingRule,
  RecurringTemplate,
} from "@/services/types";
import { useTheme } from "@/context/ThemeContext";

// Expenses page: transaction entry, category summaries, recurring
// templates, and name-to-category mapping rules, split across four
// sub-tabs (see `SUB_OPTIONS`) that share the page's year/month selection
// from `AppContext`.
//
// Data flow: this file does not use TanStack Query. Each tab fetches its
// own data with a plain `useEffect` call into `services/api.ts` and stores
// the result in local `useState`. Mutations call the matching `api.*`
// function directly, then call `refresh()` from `AppContext`, which bumps
// `refreshTick`; every tab's fetch `useEffect` depends on `refreshTick`, so
// one `refresh()` call re-runs every tab's fetch the next time it renders,
// including tabs that are not currently visible.
//
// Every fetch effect guards its `setState` calls with a per-run `active`
// flag, flipped to `false` in the cleanup function. Without it, switching
// year/month/filter fast enough lets an older request's response resolve
// after a newer one and overwrite it, showing stale data with no way to
// tell it apart from current data.
//
// Sub-tabs:
// - DetailedTab: the current month's expense list, with inline filtering,
//   add/edit form, and delete.
// - SummaryTab: year-wide charts and tables aggregated by primary or
//   secondary category, derived client-side from the year's full expense
//   list (`api.getExpenses(year)` with no month filter).
// - RecurringTab: recurring expense templates and the "apply to this
//   month" action.
// - MappingsTab: name-substring-to-category mapping rules.

/** Extracts a readable message from a caught value. A rejected fetch can throw anything, not only an `Error`. */
function errorMessage(err: unknown, fallback: string): string {
  return err instanceof Error ? err.message : fallback;
}

/**
 * This page's one visual treatment for "a request failed", so a genuine
 * fetch failure never renders as the empty-data state it would otherwise be
 * indistinguishable from. Every tab below reuses it instead of inventing its
 * own error styling.
 */
function ErrorBanner({ message }: { message: string }) {
  return (
    <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
      {message}
    </div>
  );
}

export const Route = createFileRoute("/expenses")({
  head: () => ({ meta: [{ title: "Expenses · Finguard" }] }),
  component: ExpensesPage,
});

type Sub = "detailed" | "summary" | "recurring" | "mappings";
const SUB_OPTIONS: ReadonlyArray<{ value: Sub; label: string }> = [
  { value: "detailed", label: "Detailed" },
  { value: "summary", label: "Summary" },
  { value: "recurring", label: "Recurring" },
  { value: "mappings", label: "Mappings" },
];

function ExpensesPage() {
  const [sub, setSub] = useState<Sub>("detailed");
  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Expenses</h1>
          <p className="text-sm text-muted-foreground">
            Track, categorize and analyze every expense.
          </p>
        </div>
        <SubTabs value={sub} onChange={setSub} options={SUB_OPTIONS} />
      </div>
      {sub === "detailed" && <DetailedTab />}
      {sub === "summary" && <SummaryTab />}
      {sub === "recurring" && <RecurringTab />}
      {sub === "mappings" && <MappingsTab />}
    </div>
  );
}

// ────────────────────────────────────────────────────────────── Detailed
function DetailedTab() {
  const { year, month, notify, refresh, refreshTick, currencySettings } = useApp();
  const refCurrency = currencySettings.reference_currency;
  const [rows, setRows] = useState<Expense[]>([]);
  const [cats, setCats] = useState<Categories>({ primary: [], secondary: [] });
  const [mappings, setMappings] = useState<MappingRule[]>([]);
  const [filter, setFilter] = useState<{
    name: string;
    category: string;
    min: string;
    max: string;
  }>({
    name: "",
    category: "",
    min: "",
    max: "",
  });
  const [editing, setEditing] = useState<Expense | null>(null);
  const [adding, setAdding] = useState(false);
  const [sortDir, setSortDir] = useState<"asc" | "desc">("asc");
  // Distinct from `rows` staying empty: a fetch failure must not render as
  // "No expenses match.", since that reads as "this filter has no results",
  // not "the request failed".
  const [rowsError, setRowsError] = useState<string | null>(null);
  // Currencies `getExpenses` could not resolve; the affected rows are still
  // in `rows` (with `fx_rate: 0`, rendered as a dash below), but the header
  // total is a lower bound while this is nonempty.
  const [rowsUnavailable, setRowsUnavailable] = useState<string[]>([]);
  // Categories and mappings only feed the filter/form controls, so a
  // failure here is reported without blocking the rest of the tab.
  const [supportError, setSupportError] = useState<string | null>(null);

  // Reload categories and mappings whenever a mutation elsewhere bumps
  // refreshTick (e.g. adding a category or mapping rule from another tab).
  useEffect(() => {
    let active = true;
    setSupportError(null);
    Promise.all([
      api.getCategories().then((c) => active && setCats(c)),
      api.getMappings().then((m) => active && setMappings(m)),
    ]).catch((err) => {
      if (active) setSupportError(errorMessage(err, "Failed to load categories or mappings"));
    });
    return () => {
      active = false;
    };
  }, [refreshTick]);

  // Refetch the row list on every change to year, month, the active
  // filter, or refreshTick. Empty filter fields are sent as `undefined` so
  // the backend does not apply that filter at all, rather than filtering
  // on an empty string.
  useEffect(() => {
    let active = true;
    setRowsError(null);
    api
      .getExpenses(year, month, {
        name: filter.name || undefined,
        category: filter.category || undefined,
        min: filter.min ? Number(filter.min) : undefined,
        max: filter.max ? Number(filter.max) : undefined,
      })
      .then((res) => {
        if (!active) return;
        setRows(res.expenses);
        setRowsUnavailable(res.unavailable_currencies);
      })
      .catch((err) => {
        if (active) setRowsError(errorMessage(err, "Failed to load expenses"));
      });
    return () => {
      active = false;
    };
  }, [year, month, filter, refreshTick]);

  // Sort is applied client-side to the already-filtered rows; the backend
  // does not accept a sort order.
  const sortedRows = useMemo(() => {
    const sorted = [...rows];
    sorted.sort((a, b) => {
      const cmp = a.year - b.year || a.month - b.month || a.day - b.day;
      return sortDir === "asc" ? cmp : -cmp;
    });
    return sorted;
  }, [rows, sortDir]);

  // The wire has no expense_in_ref_currency field; a caller derives the
  // reference-currency amount itself as amount * fx_rate (see Expense in
  // services/types.ts). A row with no resolved rate has fx_rate 0, so it
  // contributes nothing here; that is the same "excluded, not zero" lower
  // bound the backend's own totals endpoints use, and rowsUnavailable
  // (rendered below) tells the user when it applies.
  const totalRef = useMemo(() => rows.reduce((s, e) => s + e.amount * e.fx_rate, 0), [rows]);

  return (
    <div className="grid gap-5 lg:grid-cols-[1fr_360px]">
      <div className="space-y-3 lg:col-span-2">
        {rowsError && <ErrorBanner message={`Could not load expenses: ${rowsError}`} />}
        {rowsUnavailable.length > 0 && (
          <ErrorBanner
            message={`Could not resolve exchange rates for ${rowsUnavailable.join(", ")}. Affected rows show "—" in Ref, and Total above is a lower bound.`}
          />
        )}
        {supportError && (
          <ErrorBanner message={`Could not load categories or mappings: ${supportError}`} />
        )}
      </div>
      <GlassCard
        title={`${MONTHS[month - 1]} ${year} · ${rows.length} entries`}
        action={
          <div className="flex items-center gap-3 text-sm">
            <span className="text-muted-foreground">Total</span>
            <span className="font-semibold text-gradient">{formatRef(totalRef, refCurrency)}</span>
          </div>
        }
      >
        <div className="mb-3 grid grid-cols-2 gap-2 md:grid-cols-4">
          <input
            placeholder="Filter name…"
            value={filter.name}
            onChange={(e) => setFilter((f) => ({ ...f, name: e.target.value }))}
            className="rounded-md border border-border bg-surface/50 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
          />
          <input
            placeholder="Filter category…"
            value={filter.category}
            onChange={(e) => setFilter((f) => ({ ...f, category: e.target.value }))}
            className="rounded-md border border-border bg-surface/50 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
          />
          <input
            placeholder="Min €"
            value={filter.min}
            inputMode="decimal"
            onChange={(e) => setFilter((f) => ({ ...f, min: e.target.value }))}
            className="rounded-md border border-border bg-surface/50 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
          />
          <input
            placeholder="Max €"
            value={filter.max}
            inputMode="decimal"
            onChange={(e) => setFilter((f) => ({ ...f, max: e.target.value }))}
            className="rounded-md border border-border bg-surface/50 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
          />
        </div>

        <div className="scrollbar-thin overflow-x-auto">
          <table className="w-full min-w-[760px] text-sm">
            <thead>
              <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
                <th
                  className="cursor-pointer select-none px-3 py-2 font-medium transition-colors hover:text-foreground"
                  onClick={() => setSortDir((d) => (d === "asc" ? "desc" : "asc"))}
                >
                  <div className="flex items-center gap-1">
                    Date
                    {sortDir === "asc" ? (
                      <ArrowUp className="h-3 w-3" />
                    ) : (
                      <ArrowDown className="h-3 w-3" />
                    )}
                  </div>
                </th>
                <th className="px-3 py-2 font-medium">Name</th>
                <th className="px-3 py-2 text-right font-medium">Amount</th>
                <th className="px-3 py-2 font-medium">Curr</th>
                <th className="px-3 py-2 text-right font-medium">Ref ({refCurrency})</th>
                <th className="px-3 py-2 font-medium">Primary</th>
                <th className="px-3 py-2 font-medium">Secondary</th>
                <th className="px-3 py-2 text-right font-medium"></th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border/40">
              {sortedRows.length === 0 && !rowsError && (
                <tr>
                  <td colSpan={8} className="px-3 py-8 text-center text-muted-foreground">
                    No expenses match.
                  </td>
                </tr>
              )}
              {sortedRows.map((e) => {
                const dateStr = `${e.year}-${String(e.month).padStart(2, "0")}-${String(e.day).padStart(2, "0")}`;
                return (
                  <tr key={e.id} className="transition-colors hover:bg-muted/30">
                    <td className="px-3 py-2 font-mono text-xs text-muted-foreground">{dateStr}</td>
                    <td className="px-3 py-2 font-medium">{e.name}</td>
                    <td className="px-3 py-2 text-right tabular-nums">{e.amount.toFixed(2)}</td>
                    <td className="px-3 py-2 text-xs text-muted-foreground">{e.currency}</td>
                    <td className="px-3 py-2 text-right tabular-nums">
                      {e.fx_rate === 0 ? (
                        // A resolved rate is never exactly 0, so this marks
                        // the row's currency as one of rowsUnavailable
                        // (banner above). Showing "—" instead of
                        // e.amount * e.fx_rate avoids rendering a converted
                        // total of 0, which would look like a real, tiny
                        // figure rather than "unknown".
                        <span
                          className="text-muted-foreground"
                          title={`No exchange rate available for ${e.currency} right now.`}
                        >
                          —
                        </span>
                      ) : (
                        <span
                          title={
                            e.currency !== refCurrency
                              ? `1 ${e.currency} = ${e.fx_rate.toFixed(4)} ${refCurrency} (rate published ${e.rate_date})`
                              : undefined
                          }
                        >
                          {formatRef(e.amount * e.fx_rate, refCurrency)}
                        </span>
                      )}
                    </td>
                    <td className="px-3 py-2">
                      <CategoryChip name={e.primary} />
                    </td>
                    <td className="px-3 py-2">
                      <CategoryChip name={e.secondary} variant="muted" />
                    </td>
                    <td className="px-3 py-2">
                      <div className="flex justify-end gap-1">
                        <button
                          onClick={() => {
                            setEditing(e);
                            setAdding(false);
                          }}
                          className="rounded-md border border-border p-1 text-muted-foreground transition-colors hover:border-primary/60 hover:text-primary"
                        >
                          <Pencil className="h-3.5 w-3.5" />
                        </button>
                        <ConfirmButton
                          onConfirm={async () => {
                            await api.deleteExpense(e.id, e.year, e.month);
                            notify("success", `Deleted "${e.name}"`);
                            refresh();
                          }}
                        />
                      </div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </GlassCard>

      <div className="space-y-4">
        {!editing && !adding && (
          <button
            onClick={() => setAdding(true)}
            className="hover-lift inline-flex w-full items-center justify-center gap-2 rounded-xl bg-gradient-brand px-4 py-3 text-sm font-semibold text-background"
          >
            <Plus className="h-4 w-4" /> Add expense
          </button>
        )}
        {(adding || editing) && (
          <ExpenseForm
            initial={editing ?? undefined}
            categories={cats}
            mappings={mappings}
            onCancel={() => {
              setEditing(null);
              setAdding(false);
            }}
            onSubmit={async (data) => {
              await api.upsertExpense(data);
              notify("success", editing ? `Updated "${data.name}"` : `Added "${data.name}"`);
              setEditing(null);
              setAdding(false);
              refresh();
            }}
          />
        )}
      </div>
    </div>
  );
}

function CategoryChip({
  name,
  variant = "default",
}: {
  name: string;
  variant?: "default" | "muted";
}) {
  if (!name) return <span className="text-muted-foreground/60">—</span>;
  return (
    <span
      className={
        variant === "default"
          ? "inline-flex items-center rounded-md border border-primary/30 bg-primary/10 px-2 py-0.5 text-xs text-primary"
          : "inline-flex items-center rounded-md border border-border bg-muted/40 px-2 py-0.5 text-xs text-muted-foreground"
      }
    >
      {name}
    </span>
  );
}

function ExpenseForm({
  initial,
  categories,
  mappings,
  onSubmit,
  onCancel,
}: {
  initial?: Expense;
  categories: Categories;
  mappings: MappingRule[];
  onSubmit: (e: ExpenseWrite) => Promise<void>;
  onCancel: () => void;
}) {
  const { year, month, notify } = useApp();
  const [name, setName] = useState(initial?.name ?? "");
  const [day, setDay] = useState<string>(String(initial?.day ?? new Date().getDate()));
  const [amount, setAmount] = useState<string>(initial ? String(initial.amount) : "");
  const [currency, setCurrency] = useState<Currency>(initial?.currency ?? "EUR");
  const [primary, setPrimary] = useState(initial?.primary ?? "");
  const [secondary, setSecondary] = useState(initial?.secondary ?? "");

  // Auto-fill category from a mapping rule while adding a new expense (not
  // while editing, so an edit never silently overwrites a category the
  // user chose deliberately). Only tries once the name is at least 2
  // characters, to avoid matching on a near-empty string.
  const onNameChange = (v: string) => {
    setName(v);
    if (!initial && v.length >= 2) {
      const m = api.lookupMapping(v, mappings);
      if (m) {
        setPrimary(m.primary);
        setSecondary(m.secondary);
      }
    }
  };

  const submit = async () => {
    // `amount` accepts arithmetic expressions (e.g. "10+5.5"); evalMath
    // parses and evaluates it, returning NaN for invalid input.
    const amt = evalMath(amount);
    const dayNum = Math.max(1, Math.min(31, Math.floor(Number(day) || 1)));
    if (!name.trim() || !Number.isFinite(amt)) {
      notify("error", "Name and a numeric amount are required");
      return;
    }
    await onSubmit({
      id: initial?.id,
      year,
      month,
      day: dayNum,
      name: name.trim(),
      amount: amt,
      currency,
      primary,
      secondary,
    });
    setName("");
    setAmount("");
    setPrimary("");
    setSecondary("");
  };

  return (
    <GlassCard
      title={initial ? "Edit expense" : "Add expense"}
      action={
        <button onClick={onCancel} className="text-muted-foreground hover:text-foreground">
          <X className="h-4 w-4" />
        </button>
      }
    >
      <div className="space-y-3">
        <Field label="Name">
          <input
            value={name}
            onChange={(e) => onNameChange(e.target.value)}
            placeholder="Lidl, Rent…"
            className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
          />
        </Field>
        <div className="grid grid-cols-2 gap-3">
          <Field label="Day">
            <input
              type="number"
              min={1}
              max={31}
              value={day}
              onChange={(e) => setDay(e.target.value)}
              className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
            />
          </Field>
          <Field label="Amount (math ok)">
            <input
              value={amount}
              onChange={(e) => setAmount(e.target.value)}
              placeholder="10+5.5"
              className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm tabular-nums focus:border-primary/60 focus:outline-none"
            />
          </Field>
        </div>
        <Field label="Currency">
          <select
            value={currency}
            onChange={(e) => setCurrency(e.target.value as Currency)}
            className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
          >
            {CURRENCIES.map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </select>
        </Field>
        <Field label="Primary category">
          <Combobox
            value={primary}
            onChange={setPrimary}
            options={categories.primary}
            placeholder="Groceries…"
          />
        </Field>
        <Field label="Secondary category">
          <Combobox
            value={secondary}
            onChange={setSecondary}
            options={categories.secondary}
            placeholder="Supermarket…"
          />
        </Field>
        <div className="flex gap-2 pt-2">
          <button
            onClick={submit}
            className="hover-lift inline-flex flex-1 items-center justify-center gap-2 rounded-md bg-gradient-brand px-3 py-2 text-sm font-semibold text-background"
          >
            {initial ? "Save changes" : "Add expense"}
          </button>
          <button
            onClick={onCancel}
            className="rounded-md border border-border px-3 py-2 text-sm text-muted-foreground hover:text-foreground"
          >
            Cancel
          </button>
        </div>
      </div>
    </GlassCard>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <label className="block">
      <span className="mb-1 block text-[11px] font-medium uppercase tracking-wider text-muted-foreground">
        {label}
      </span>
      {children}
    </label>
  );
}

// ────────────────────────────────────────────────────────────── Summary

/**
 * Running total for one aggregate cell (a category-month, a category-year
 * total, or a whole month) built by folding `Expense` rows with `addRow`.
 * `resolvedRows`/`unresolvedRows` count contributing rows by whether the
 * backend resolved their `fx_rate` (an unresolved row's `fx_rate` is 0, the
 * same marker `DetailedTab` checks at row level), so `coverageOf` can tell
 * "no rows here" apart from "rows here, but we don't know their amount",
 * which look identical if only `value` is kept.
 */
interface RowSum {
  value: number;
  resolvedRows: number;
  unresolvedRows: number;
}

const ZERO_SUM: RowSum = { value: 0, resolvedRows: 0, unresolvedRows: 0 };

function addRow(sum: RowSum, e: Expense): RowSum {
  return {
    value: sum.value + e.amount * e.fx_rate,
    resolvedRows: sum.resolvedRows + (e.fx_rate !== 0 ? 1 : 0),
    unresolvedRows: sum.unresolvedRows + (e.fx_rate === 0 ? 1 : 0),
  };
}

function mergeSums(a: RowSum, b: RowSum): RowSum {
  return {
    value: a.value + b.value,
    resolvedRows: a.resolvedRows + b.resolvedRows,
    unresolvedRows: a.unresolvedRows + b.unresolvedRows,
  };
}

/**
 * "empty": no contributing row, a real absence, the normal 0/"—" case.
 * "resolved": every contributing row had a resolved rate; `value` is exact.
 * "partial": a mix; `value` is a lower bound, since the unresolved rows
 * contributed 0 instead of their real amount.
 * "unknown": every contributing row was unresolved; `value` is exactly 0 but
 * means "we don't know", not "there was nothing here". This is the
 * collision the reviewer flagged: without this case, "unknown" and "empty"
 * render identically.
 */
type Coverage = "empty" | "resolved" | "partial" | "unknown";

function coverageOf(sum: RowSum): Coverage {
  if (sum.resolvedRows === 0 && sum.unresolvedRows === 0) return "empty";
  if (sum.unresolvedRows === 0) return "resolved";
  if (sum.resolvedRows === 0) return "unknown";
  return "partial";
}

const isIncomplete = (c: Coverage) => c === "partial" || c === "unknown";

/**
 * Ranks by `value` descending, like a plain numeric sort, except a bucket
 * with "partial" or "unknown" coverage never sorts below a "resolved" or
 * "empty" one purely because its counted value came out low or zero: its
 * real value is unknown and could be larger than anything else on the page,
 * so ranking it as small would be a guess dressed up as data. Within one
 * coverage tier, still sorts by value.
 */
function rankDescending(
  a: { value: number; coverage: Coverage },
  b: { value: number; coverage: Coverage },
): number {
  const tier = (c: Coverage) => (isIncomplete(c) ? 1 : 0);
  const byTier = tier(b.coverage) - tier(a.coverage);
  return byTier !== 0 ? byTier : b.value - a.value;
}

/** Suffix used to smuggle a `Coverage` alongside a chart row's numeric value without disturbing the numeric dataKey Recharts reads for the axis/series. */
const coverageKey = (label: string) => `${label}__coverage`;

/**
 * Full currency-formatted amount (the monthTotals table and yearTable's
 * Total column). "unknown" reads as a bare dash instead of a formatted 0,
 * since a resolved 0 and an unresolved amount must not look the same;
 * "partial" keeps the number, since a lower bound is still informative, but
 * marks it so it does not read as the final figure.
 */
function AmountCell({
  value,
  coverage,
  currency,
}: {
  value: number;
  coverage: Coverage;
  currency: Currency;
}) {
  if (coverage === "unknown") {
    return (
      <span
        className="text-warning"
        title="No exchange rate available for this category's currency; amount unknown, not zero."
      >
        —
      </span>
    );
  }
  return (
    <span
      title={
        coverage === "partial"
          ? "Partial: no exchange rate available for some of this category's rows. This figure is a lower bound."
          : undefined
      }
    >
      {formatRef(value, currency)}
      {coverage === "partial" && <sup className="ml-0.5 text-warning">*</sup>}
    </span>
  );
}

/**
 * Compact (no currency symbol) amount for one yearTable month cell. Reuses
 * "—" for `coverage === "empty"`, matching the table's original empty-month
 * style, but `coverage === "unknown"` gets its own "?" glyph precisely so it
 * cannot be mistaken for that same dash, which is the exact collision the
 * reviewer found at the old `v > 0 ? v.toFixed(2) : "—"` check.
 */
function MonthCell({ value, coverage }: { value: number; coverage: Coverage }) {
  if (coverage === "empty") return <span>—</span>;
  if (coverage === "unknown") {
    return (
      <span
        className="text-warning"
        title="No exchange rate available for this category's currency this month; amount unknown, not zero."
      >
        ?
      </span>
    );
  }
  return (
    <span
      title={
        coverage === "partial"
          ? "Partial: some rows this month have no resolved rate. This figure is a lower bound."
          : undefined
      }
    >
      {value.toFixed(2)}
      {coverage === "partial" && <sup className="text-warning">*</sup>}
    </span>
  );
}

/**
 * Custom point for the category-trend line chart, replacing Recharts'
 * default filled dot. A "partial" or "unknown" point renders larger and in
 * the warning color instead of the line's own color, so an unresolved
 * month's point on the line cannot be mistaken for a small real value.
 */
function CoverageDot({
  cx,
  cy,
  color,
  coverage,
}: {
  cx?: number;
  cy?: number;
  color: string;
  coverage?: Coverage;
}) {
  const flagged = coverage != null && isIncomplete(coverage);
  return (
    <circle
      cx={cx ?? 0}
      cy={cy ?? 0}
      r={flagged ? 5 : 3}
      fill={flagged ? "var(--warning)" : color}
      stroke={flagged ? "var(--warning)" : color}
    />
  );
}

/**
 * Recovers one Recharts tooltip payload entry's `Coverage`. Recharts always
 * attaches the hovered datum's own raw row as `payload` and the series'
 * `dataKey` to every entry, even though DarkTooltip's narrower `Props` type
 * doesn't name them. `monthTotals`/`yearPie`/`yearBarData` put `coverage`
 * directly on that row; `compareData`/`trendData` instead carry it under the
 * `coverageKey(dataKey)` convention, since those rows hold one value per
 * selected month or category. Falls back to "resolved" (render plainly)
 * when neither is present, so a tooltip entry with no coverage information
 * degrades to the old, unmarked behavior instead of erroring.
 */
function coverageOfEntry(p: {
  dataKey?: string | number;
  payload?: Record<string, unknown>;
}): Coverage {
  const row = p.payload;
  if (!row) return "resolved";
  if (typeof row.coverage === "string") return row.coverage as Coverage;
  if (typeof p.dataKey === "string") {
    const c = row[coverageKey(p.dataKey)];
    if (typeof c === "string") return c as Coverage;
  }
  return "resolved";
}

/**
 * SummaryTab's own tooltip, replacing `DarkTooltip` (components/finguard,
 * shared by other pages) for every chart in this tab. `DarkTooltip` always
 * renders `formatRef(value)` for an entry, so hovering an "unknown" slice,
 * bar, or point showed a bare "€0.00": the exact table-cell bug this pass
 * fixed, reopened on the one surface DarkTooltip couldn't be told about
 * without changing a component several other pages depend on. Kept local
 * to this file instead: visually matches DarkTooltip (same classes), but an
 * "unknown" entry reads as text, never a currency amount, and a "partial"
 * entry keeps its number with the same `*`/title marker the tables use.
 */
function SummaryTooltip({
  active,
  payload,
  label,
  total,
  currency,
}: {
  active?: boolean;
  payload?: Array<{
    name: string;
    value: number;
    color?: string;
    dataKey?: string | number;
    payload?: Record<string, unknown>;
  }>;
  label?: string | number;
  total?: boolean;
  currency: Currency;
}) {
  if (!active || !payload || payload.length === 0) return null;
  const sum = payload.reduce((s, p) => s + (Number(p.value) || 0), 0);
  const anyIncomplete = payload.some((p) => isIncomplete(coverageOfEntry(p)));
  // Every constituent unknown means the total is unknown too, not a real 0:
  // the same rule the per-entry rows above already follow.
  const allUnknown = payload.every((p) => coverageOfEntry(p) === "unknown");
  return (
    <div className="rounded-lg border border-border/80 bg-popover/95 px-3 py-2 text-xs shadow-elegant backdrop-blur-md">
      {label != null && (
        <div className="mb-1 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
          {label}
        </div>
      )}
      <ul className="space-y-0.5">
        {payload.map((p, i) => {
          const coverage = coverageOfEntry(p);
          return (
            <li key={i} className="flex items-center gap-2">
              <span className="h-2 w-2 rounded-full" style={{ background: p.color }} />
              <span className="text-foreground/90">{p.name}</span>
              <span className="ml-auto font-medium text-foreground">
                {coverage === "unknown" ? (
                  <span
                    className="text-warning"
                    title="No exchange rate available; amount unknown, not zero."
                  >
                    unknown
                  </span>
                ) : (
                  <>
                    {formatRef(Number(p.value) || 0, currency)}
                    {coverage === "partial" && (
                      <sup
                        className="ml-0.5 text-warning"
                        title="Partial: some rows have no resolved rate. This figure is a lower bound."
                      >
                        *
                      </sup>
                    )}
                  </>
                )}
              </span>
            </li>
          );
        })}
        {total && payload.length > 1 && (
          <li className="mt-1 flex items-center gap-2 border-t border-border/60 pt-1 text-foreground/80">
            <span className="ml-auto text-[11px]">Total</span>
            <span className="font-semibold text-foreground">
              {allUnknown ? (
                <span
                  className="text-warning"
                  title="No exchange rate available for any entry above; total unknown, not zero."
                >
                  unknown
                </span>
              ) : (
                <>
                  {formatRef(sum, currency)}
                  {anyIncomplete && (
                    <sup
                      className="ml-0.5 text-warning"
                      title="Partial: at least one entry above has no resolved rate. This figure is a lower bound."
                    >
                      *
                    </sup>
                  )}
                </>
              )}
            </span>
          </li>
        )}
      </ul>
    </div>
  );
}

function SummaryTab() {
  const colorAt = useChartColors();
  const { year, month, refreshTick, currencySettings } = useApp();
  const refCurrency = currencySettings.reference_currency;
  const [kind, setKind] = useState<"primary" | "secondary">("primary");
  const [yearExpenses, setYearExpenses] = useState<Expense[]>([]);
  const [selMonths, setSelMonths] = useState<number[]>([Math.max(1, month - 1), month]);
  const [selCats, setSelCats] = useState<string[]>([]);
  const { theme } = useTheme();
  const tickColor = theme === "arctic" ? "oklch(0.48 0.022 240)" : "oklch(0.68 0.02 260)";
  // Distinct from `yearExpenses` staying empty: a fetch failure must not
  // render as "No data" for every chart and table below, since that reads
  // as "nothing was spent this year" rather than "the request failed".
  const [yearExpensesError, setYearExpensesError] = useState<string | null>(null);
  // Currencies `getExpenses` could not resolve for the year. Every table and
  // chart below sums amount * fx_rate, where an unresolved row's fx_rate is
  // 0, so a nonempty list here means every figure derived from yearExpenses
  // is a lower bound rather than the true total.
  const [yearExpensesUnavailable, setYearExpensesUnavailable] = useState<string[]>([]);

  // Loads every expense for the whole year once (not per-month); every
  // derived table and chart below slices this same list client-side
  // instead of making a separate request per view.
  useEffect(() => {
    let active = true;
    setYearExpensesError(null);
    api
      .getExpenses(year)
      .then((res) => {
        if (!active) return;
        setYearExpenses(res.expenses);
        setYearExpensesUnavailable(res.unavailable_currencies);
      })
      .catch((err) => {
        if (active) setYearExpensesError(errorMessage(err, "Failed to load expenses"));
      });
    return () => {
      active = false;
    };
  }, [year, refreshTick]);

  const catOf = (e: Expense) => (kind === "primary" ? e.primary : e.secondary) || "Uncategorized";

  // Per-category totals for the selected month only, ranked for the month
  // pie chart and table (see rankDescending: a partial/unknown category
  // never sinks below a resolved one just because its counted value is low).
  const monthTotals = useMemo(() => {
    const map = new Map<string, RowSum>();
    for (const e of yearExpenses) {
      if (e.month !== month) continue;
      map.set(catOf(e), addRow(map.get(catOf(e)) ?? ZERO_SUM, e));
    }
    return Array.from(map.entries())
      .map(([name, sum]) => ({ name, value: sum.value, coverage: coverageOf(sum) }))
      .sort(rankDescending);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [yearExpenses, month, kind]);

  // category -> 12-slot month array (index 0 = January) plus its yearly
  // total, one row per category that appears anywhere in the year. Each
  // month slot is a RowSum, not a bare number, so a month with only
  // unresolved rows can render as unknown (MonthCell) rather than as the
  // same empty-month dash a genuinely quiet month gets.
  const yearTable = useMemo(() => {
    const cats = new Set<string>();
    const grid: Record<string, RowSum[]> = {};
    for (const e of yearExpenses) {
      const c = catOf(e);
      cats.add(c);
      if (!grid[c]) grid[c] = Array.from({ length: 12 }, () => ZERO_SUM);
      grid[c][e.month - 1] = addRow(grid[c][e.month - 1], e);
    }
    return Array.from(cats)
      .sort()
      .map((c) => ({ category: c, months: grid[c], total: grid[c].reduce(mergeSums, ZERO_SUM) }));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [yearExpenses, kind]);

  const yearPie = useMemo(
    () =>
      yearTable
        .map((r) => ({ name: r.category, value: r.total.value, coverage: coverageOf(r.total) }))
        .sort(rankDescending),
    [yearTable],
  );

  const allCats = useMemo(() => yearTable.map((r) => r.category), [yearTable]);

  // Secondary-category bar chart caps the visible bars at the top 12 by
  // yearly total and folds every category beyond that into one "Others"
  // bar, so the chart height stays bounded regardless of how many distinct
  // secondary categories exist. yearPie's rankDescending already keeps every
  // partial/unknown category ahead of the resolved ones, so "Others" only
  // absorbs a partial/unknown category once there are more than TOP of them
  // in one year; when that happens, "Others" itself is marked "partial"
  // rather than claiming to be a complete sum, since folding an incomplete
  // category's counted (lower-bound) value into it silently would recreate
  // the same "reads as a real number" trap one level up.
  const yearBarData = useMemo(() => {
    if (kind !== "secondary") return [];
    const TOP = 12;
    if (yearPie.length <= TOP) return yearPie;
    const rest = yearPie.slice(TOP);
    const othersValue = rest.reduce((s, r) => s + r.value, 0);
    const othersCoverage: Coverage = rest.some((r) => isIncomplete(r.coverage))
      ? "partial"
      : "resolved";
    return [
      ...yearPie.slice(0, TOP),
      { name: "Others", value: othersValue, coverage: othersCoverage },
    ];
  }, [yearPie, kind]);

  // default-select top 3 categories on first load
  useEffect(() => {
    if (selCats.length === 0 && allCats.length > 0) {
      setSelCats(yearPie.slice(0, 3).map((p) => p.name));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [allCats.join("|")]);

  // Pivots the selected months (at most 3, see toggleMonth) into one row
  // per category with one column per selected month, for the grouped bar
  // chart comparing categories across months. Each month column also gets a
  // parallel `<label>__coverage` entry (see coverageKey) so the Bar's Cell
  // children below can color an unresolved bar distinctly; Recharts reads
  // only the declared numeric dataKeys for the axis/series, so the extra
  // string entries do not affect the chart's scale.
  const compareData = useMemo(() => {
    if (selMonths.length === 0) return [];
    const cats = new Set<string>();
    const buckets: Record<number, Record<string, RowSum>> = {};
    for (const m of selMonths) buckets[m] = {};
    for (const e of yearExpenses) {
      if (!selMonths.includes(e.month)) continue;
      const c = catOf(e);
      cats.add(c);
      buckets[e.month][c] = addRow(buckets[e.month][c] ?? ZERO_SUM, e);
    }
    return Array.from(cats).map((c) => {
      const r: Record<string, number | string> = { category: c };
      for (const m of selMonths) {
        const label = MONTHS_SHORT[m - 1];
        const sum = buckets[m][c] ?? ZERO_SUM;
        r[label] = sum.value;
        r[coverageKey(label)] = coverageOf(sum);
      }
      return r;
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [yearExpenses, selMonths, kind]);

  // One row per month (all 12, regardless of selection) with one column
  // per selected category (at most 3, see toggleCat), for the category
  // trend line chart. Same coverageKey convention as compareData, read by
  // each Line's custom dot below.
  const trendData = useMemo(() => {
    return MONTHS_SHORT.map((m, i) => {
      const row: Record<string, number | string> = { month: m };
      for (const c of selCats) {
        const sum = yearExpenses
          .filter((e) => e.month === i + 1 && catOf(e) === c)
          .reduce(addRow, ZERO_SUM);
        row[c] = sum.value;
        row[coverageKey(c)] = coverageOf(sum);
      }
      return row;
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [yearExpenses, selCats, kind]);

  // Both toggles cap the selection at 3 entries; selecting a 4th evicts the
  // oldest selection (FIFO) rather than rejecting the new pick.
  const toggleMonth = (m: number) =>
    setSelMonths((prev) =>
      prev.includes(m)
        ? prev.filter((x) => x !== m)
        : prev.length >= 3
          ? [...prev.slice(1), m]
          : [...prev, m],
    );
  const toggleCat = (c: string) =>
    setSelCats((prev) =>
      prev.includes(c)
        ? prev.filter((x) => x !== c)
        : prev.length >= 3
          ? [...prev.slice(1), c]
          : [...prev, c],
    );

  return (
    <div className="space-y-5">
      {yearExpensesError && (
        <ErrorBanner message={`Could not load expenses: ${yearExpensesError}`} />
      )}
      {yearExpensesUnavailable.length > 0 && (
        <ErrorBanner
          message={`Could not resolve exchange rates for ${yearExpensesUnavailable.join(", ")}. Totals, tables, and charts below are a lower bound.`}
        />
      )}
      <div className="flex items-center gap-3">
        <span className="text-sm text-muted-foreground">Group by</span>
        <SubTabs
          value={kind}
          onChange={setKind}
          options={[
            { value: "primary", label: "Primary" },
            { value: "secondary", label: "Secondary" },
          ]}
        />
      </div>

      <div className="grid gap-5 lg:grid-cols-2">
        <GlassCard title={`${MONTHS[month - 1]} ${year} totals`}>
          <div className="grid gap-4 md:grid-cols-[1fr_220px]">
            <div className="scrollbar-thin max-h-72 overflow-y-auto">
              <table className="w-full text-sm">
                <tbody className="divide-y divide-border/40">
                  {monthTotals.length === 0 && (
                    <tr>
                      <td className="py-6 text-center text-muted-foreground">No data</td>
                    </tr>
                  )}
                  {monthTotals.map((r, i) => (
                    <tr key={r.name}>
                      <td className="px-2 py-1.5 font-medium">
                        <span
                          className="mr-2 inline-block h-2 w-2 rounded-full"
                          style={{ background: colorAt(i) }}
                        />
                        {r.name}
                      </td>
                      <td className="px-2 py-1.5 text-right tabular-nums">
                        <AmountCell value={r.value} coverage={r.coverage} currency={refCurrency} />
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div className="h-56">
              <ResponsiveContainer>
                <PieChart>
                  <Pie
                    data={monthTotals}
                    dataKey="value"
                    nameKey="name"
                    innerRadius={40}
                    outerRadius={75}
                    paddingAngle={2}
                  >
                    {monthTotals.map((r, i) => (
                      <Cell
                        key={i}
                        fill={isIncomplete(r.coverage) ? "var(--warning)" : colorAt(i)}
                        stroke="oklch(0.16 0.02 265)"
                      />
                    ))}
                  </Pie>
                  <Tooltip content={<SummaryTooltip currency={refCurrency} />} />
                </PieChart>
              </ResponsiveContainer>
            </div>
          </div>
        </GlassCard>

        {kind === "secondary" ? (
          <GlassCard title={`Year ${year} — top secondary categories`}>
            <div style={{ height: Math.max(288, yearBarData.length * 36) }}>
              <ResponsiveContainer>
                <BarChart
                  data={yearBarData}
                  layout="vertical"
                  margin={{ left: 8, right: 32, top: 4, bottom: 4 }}
                >
                  <CartesianGrid
                    strokeDasharray="3 3"
                    stroke="oklch(1 0 0 / 6%)"
                    horizontal={false}
                  />
                  <XAxis type="number" tick={{ fontSize: 14, fill: tickColor }} />
                  <YAxis
                    type="category"
                    dataKey="name"
                    width={120}
                    tick={{ fontSize: 14, fill: tickColor }}
                  />
                  <Tooltip
                    content={<SummaryTooltip currency={refCurrency} />}
                    cursor={{ fill: "oklch(1 0 0 / 4%)" }}
                  />
                  <Bar dataKey="value" radius={[0, 4, 4, 0]}>
                    {yearBarData.map((entry, i) => (
                      <Cell
                        key={i}
                        fill={
                          entry.name === "Others"
                            ? "var(--muted-foreground)"
                            : isIncomplete(entry.coverage)
                              ? "var(--warning)"
                              : colorAt(i)
                        }
                      />
                    ))}
                  </Bar>
                </BarChart>
              </ResponsiveContainer>
            </div>
          </GlassCard>
        ) : (
          <GlassCard title={`Year ${year} — by primary category`}>
            <div className="h-72">
              <ResponsiveContainer>
                <PieChart>
                  <Pie
                    data={yearPie}
                    dataKey="value"
                    nameKey="name"
                    innerRadius={55}
                    outerRadius={100}
                    paddingAngle={2}
                  >
                    {yearPie.map((r, i) => (
                      <Cell
                        key={i}
                        fill={isIncomplete(r.coverage) ? "var(--warning)" : colorAt(i)}
                      />
                    ))}
                  </Pie>
                  <Tooltip content={<SummaryTooltip currency={refCurrency} />} />
                  <Legend verticalAlign="bottom" wrapperStyle={LEGEND_STYLE} />
                </PieChart>
              </ResponsiveContainer>
            </div>
          </GlassCard>
        )}
      </div>

      <GlassCard title={`Year ${year} by ${kind} category`}>
        <div className="scrollbar-thin overflow-x-auto">
          <table className="w-full min-w-[920px] text-sm">
            <thead>
              <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
                <th className="px-3 py-2 font-medium">Category</th>
                {MONTHS_SHORT.map((m) => (
                  <th key={m} className="px-2 py-2 text-right font-medium">
                    {m}
                  </th>
                ))}
                <th className="px-3 py-2 text-right font-medium">Total</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border/40">
              {yearTable.map((r) => (
                <tr key={r.category} className="hover:bg-muted/30">
                  <td className="px-3 py-1.5 font-medium">{r.category}</td>
                  {r.months.map((sum, i) => (
                    <td
                      key={i}
                      className="px-2 py-1.5 text-right text-xs tabular-nums text-muted-foreground"
                    >
                      <MonthCell value={sum.value} coverage={coverageOf(sum)} />
                    </td>
                  ))}
                  <td className="px-3 py-1.5 text-right font-semibold tabular-nums">
                    <AmountCell
                      value={r.total.value}
                      coverage={coverageOf(r.total)}
                      currency={refCurrency}
                    />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </GlassCard>

      <div className="grid gap-5 lg:grid-cols-2">
        <GlassCard
          title="Compare months"
          action={<span className="text-[11px] text-muted-foreground">Pick up to 3</span>}
        >
          <div className="mb-3 flex flex-wrap gap-1.5">
            {MONTHS_SHORT.map((m, i) => {
              const idx = i + 1;
              const active = selMonths.includes(idx);
              return (
                <button
                  key={m}
                  onClick={() => toggleMonth(idx)}
                  className={
                    active
                      ? "rounded-md bg-gradient-brand px-2 py-1 text-xs font-semibold text-background"
                      : "rounded-md border border-border bg-surface/40 px-2 py-1 text-xs text-muted-foreground hover:text-foreground"
                  }
                >
                  {m}
                </button>
              );
            })}
          </div>
          <div className="h-72">
            <ResponsiveContainer>
              <BarChart data={compareData}>
                <CartesianGrid strokeDasharray="3 3" stroke="oklch(1 0 0 / 6%)" />
                <XAxis
                  dataKey="category"
                  tick={{ fontSize: 13, fill: tickColor }}
                  interval={0}
                  angle={-25}
                  textAnchor="end"
                  height={60}
                />
                <YAxis tick={{ fontSize: 14, fill: tickColor }} />
                <Tooltip
                  content={<SummaryTooltip total currency={refCurrency} />}
                  cursor={{ fill: "oklch(1 0 0 / 4%)" }}
                />
                <Legend wrapperStyle={LEGEND_STYLE} />
                {selMonths.map((m, i) => {
                  const label = MONTHS_SHORT[m - 1];
                  return (
                    <Bar key={m} dataKey={label} fill={colorAt(i)} radius={[4, 4, 0, 0]}>
                      {compareData.map((row, j) => (
                        <Cell
                          key={j}
                          fill={
                            isIncomplete(row[coverageKey(label)] as Coverage)
                              ? "var(--warning)"
                              : colorAt(i)
                          }
                        />
                      ))}
                    </Bar>
                  );
                })}
              </BarChart>
            </ResponsiveContainer>
          </div>
        </GlassCard>

        <GlassCard
          title="Category trend"
          action={<span className="text-[11px] text-muted-foreground">Pick up to 3</span>}
        >
          <div className="mb-3 flex flex-wrap gap-1.5">
            {allCats.slice(0, 14).map((c) => {
              const active = selCats.includes(c);
              return (
                <button
                  key={c}
                  onClick={() => toggleCat(c)}
                  className={
                    active
                      ? "rounded-md bg-gradient-brand px-2 py-1 text-xs font-semibold text-background"
                      : "rounded-md border border-border bg-surface/40 px-2 py-1 text-xs text-muted-foreground hover:text-foreground"
                  }
                >
                  {c}
                </button>
              );
            })}
          </div>
          <div className="h-72">
            <ResponsiveContainer>
              <LineChart data={trendData}>
                <CartesianGrid strokeDasharray="3 3" stroke="oklch(1 0 0 / 6%)" />
                <XAxis dataKey="month" tick={{ fontSize: 14, fill: tickColor }} />
                <YAxis tick={{ fontSize: 14, fill: tickColor }} />
                <Tooltip
                  content={<SummaryTooltip currency={refCurrency} />}
                  cursor={{ stroke: "oklch(1 0 0 / 10%)" }}
                />
                <Legend wrapperStyle={LEGEND_STYLE} />
                {selCats.map((c, i) => (
                  <Line
                    key={c}
                    type="monotone"
                    dataKey={c}
                    stroke={colorAt(i)}
                    strokeWidth={2.5}
                    dot={(dotProps) => (
                      <CoverageDot
                        key={`${c}-${dotProps.index}`}
                        cx={dotProps.cx}
                        cy={dotProps.cy}
                        color={colorAt(i)}
                        coverage={dotProps.payload?.[coverageKey(c)]}
                      />
                    )}
                    activeDot={{ r: 5 }}
                  />
                ))}
              </LineChart>
            </ResponsiveContainer>
          </div>
        </GlassCard>
      </div>
    </div>
  );
}

// ────────────────────────────────────────────────────────────── Recurring
function RecurringTab() {
  const { year, month, notify, refresh, refreshTick } = useApp();
  const [items, setItems] = useState<RecurringTemplate[]>([]);
  const [cats, setCats] = useState<Categories>({ primary: [], secondary: [] });
  const [form, setForm] = useState({
    name: "",
    day: "1",
    amount: "",
    currency: "EUR" as Currency,
    primary: "",
    secondary: "",
  });
  // Distinct from `items` staying empty: a fetch failure must not render as
  // "No recurring templates yet.", since that reads as "you have none set
  // up", not "the request failed".
  const [itemsError, setItemsError] = useState<string | null>(null);
  // Categories only feed the add-template form, so a failure here is
  // reported without blocking the rest of the tab.
  const [catsError, setCatsError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    setItemsError(null);
    setCatsError(null);
    api
      .getRecurring(year)
      .then((items) => active && setItems(items))
      .catch((err) => {
        if (active) setItemsError(errorMessage(err, "Failed to load recurring templates"));
      });
    api
      .getCategories()
      .then((cats) => active && setCats(cats))
      .catch((err) => {
        if (active) setCatsError(errorMessage(err, "Failed to load categories"));
      });
    return () => {
      active = false;
    };
  }, [refreshTick]);

  const submit = async () => {
    const amt = evalMath(form.amount);
    if (!form.name.trim() || !Number.isFinite(amt)) {
      notify("error", "Name and amount required");
      return;
    }
    await api.addRecurring({
      year,
      name: form.name.trim(),
      // Clamped to 1-28 so the template's day exists in every month,
      // including February.
      day: Math.max(1, Math.min(28, Number(form.day) || 1)),
      amount: amt,
      currency: form.currency,
      primary: form.primary,
      secondary: form.secondary,
    });
    setForm({ name: "", day: "1", amount: "", currency: "EUR", primary: "", secondary: "" });
    notify("success", "Template added");
    refresh();
  };

  // Materializes every recurring template into the current month's expense
  // list (the backend skips templates already applied that month, see
  // api.applyRecurring), then refetches so DetailedTab shows the new rows.
  const apply = async () => {
    notify("loading", "Applying recurring…");
    const n = await api.applyRecurring(year, month);
    notify("success", `Added ${n} entries to ${MONTHS[month - 1]} ${year}`);
    refresh();
  };

  return (
    <div className="grid gap-5 lg:grid-cols-[1fr_360px]">
      <div className="space-y-3 lg:col-span-2">
        {itemsError && (
          <ErrorBanner message={`Could not load recurring templates: ${itemsError}`} />
        )}
        {catsError && <ErrorBanner message={`Could not load categories: ${catsError}`} />}
      </div>
      <GlassCard
        title={`${items.length} recurring templates`}
        action={
          <button
            onClick={apply}
            className="hover-lift inline-flex items-center gap-2 rounded-lg bg-gradient-brand px-3 py-1.5 text-xs font-semibold text-background"
          >
            <Play className="h-3.5 w-3.5" /> Apply to {MONTHS_SHORT[month - 1]} {year}
          </button>
        }
      >
        <div className="scrollbar-thin overflow-x-auto">
          <table className="w-full min-w-[640px] text-sm">
            <thead>
              <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
                <th className="px-3 py-2 font-medium">Name</th>
                <th className="px-3 py-2 text-right font-medium">Day</th>
                <th className="px-3 py-2 text-right font-medium">Amount</th>
                <th className="px-3 py-2 font-medium">Curr</th>
                <th className="px-3 py-2 font-medium">Primary</th>
                <th className="px-3 py-2 font-medium">Secondary</th>
                <th className="px-3 py-2"></th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border/40">
              {items.map((r) => (
                <tr key={r.id} className="hover:bg-muted/30">
                  <td className="px-3 py-2 font-medium">{r.name}</td>
                  <td className="px-3 py-2 text-right tabular-nums">{r.day}</td>
                  <td className="px-3 py-2 text-right tabular-nums">{r.amount.toFixed(2)}</td>
                  <td className="px-3 py-2 text-xs text-muted-foreground">{r.currency}</td>
                  <td className="px-3 py-2">
                    <CategoryChip name={r.primary} />
                  </td>
                  <td className="px-3 py-2">
                    <CategoryChip name={r.secondary} variant="muted" />
                  </td>
                  <td className="px-3 py-2 text-right">
                    <ConfirmButton
                      onConfirm={async () => {
                        await api.deleteRecurring(r.id, year);
                        notify("success", `Removed "${r.name}"`);
                        refresh();
                      }}
                    />
                  </td>
                </tr>
              ))}
              {items.length === 0 && !itemsError && (
                <tr>
                  <td colSpan={7} className="px-3 py-8 text-center text-muted-foreground">
                    No recurring templates yet.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </GlassCard>

      <GlassCard title="Add recurring template">
        <div className="space-y-3">
          <Field label="Name">
            <input
              value={form.name}
              onChange={(e) => setForm({ ...form, name: e.target.value })}
              className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
            />
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Day (1-28)">
              <input
                type="number"
                min={1}
                max={28}
                value={form.day}
                onChange={(e) => setForm({ ...form, day: e.target.value })}
                className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
              />
            </Field>
            <Field label="Amount">
              <input
                value={form.amount}
                onChange={(e) => setForm({ ...form, amount: e.target.value })}
                className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
              />
            </Field>
          </div>
          <Field label="Currency">
            <select
              value={form.currency}
              onChange={(e) => setForm({ ...form, currency: e.target.value as Currency })}
              className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm"
            >
              {CURRENCIES.map((c) => (
                <option key={c} value={c}>
                  {c}
                </option>
              ))}
            </select>
          </Field>
          <Field label="Primary">
            <Combobox
              value={form.primary}
              onChange={(v) => setForm({ ...form, primary: v })}
              options={cats.primary}
            />
          </Field>
          <Field label="Secondary">
            <Combobox
              value={form.secondary}
              onChange={(v) => setForm({ ...form, secondary: v })}
              options={cats.secondary}
            />
          </Field>
          <button
            onClick={submit}
            className="hover-lift inline-flex w-full items-center justify-center gap-2 rounded-md bg-gradient-brand px-3 py-2 text-sm font-semibold text-background"
          >
            <Plus className="h-4 w-4" /> Add template
          </button>
        </div>
      </GlassCard>
    </div>
  );
}

// ────────────────────────────────────────────────────────────── Mappings
function MappingsTab() {
  const { notify, refresh, refreshTick } = useApp();
  const [items, setItems] = useState<MappingRule[]>([]);
  const [cats, setCats] = useState<Categories>({ primary: [], secondary: [] });
  const [form, setForm] = useState({ match: "", primary: "", secondary: "" });
  // Distinct from `items` staying empty: a fetch failure must not render as
  // "No mapping rules yet.", since that reads as "you have none set up",
  // not "the request failed".
  const [itemsError, setItemsError] = useState<string | null>(null);
  // Categories only feed the add-rule form, so a failure here is reported
  // without blocking the rest of the tab.
  const [catsError, setCatsError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    setItemsError(null);
    setCatsError(null);
    api
      .getMappings()
      .then((items) => active && setItems(items))
      .catch((err) => {
        if (active) setItemsError(errorMessage(err, "Failed to load mapping rules"));
      });
    api
      .getCategories()
      .then((cats) => active && setCats(cats))
      .catch((err) => {
        if (active) setCatsError(errorMessage(err, "Failed to load categories"));
      });
    return () => {
      active = false;
    };
  }, [refreshTick]);

  const submit = async () => {
    if (!form.match.trim() || !form.primary.trim()) {
      notify("error", "Match and primary required");
      return;
    }
    await api.addMapping({
      match: form.match.trim(),
      primary: form.primary,
      secondary: form.secondary,
    });
    setForm({ match: "", primary: "", secondary: "" });
    notify("success", "Mapping added");
    refresh();
  };

  return (
    <div className="grid gap-5 lg:grid-cols-[1fr_360px]">
      <div className="space-y-3 lg:col-span-2">
        {itemsError && <ErrorBanner message={`Could not load mapping rules: ${itemsError}`} />}
        {catsError && <ErrorBanner message={`Could not load categories: ${catsError}`} />}
      </div>
      <GlassCard title={`${items.length} mapping rules`}>
        <div className="scrollbar-thin overflow-x-auto">
          <table className="w-full min-w-[520px] text-sm">
            <thead>
              <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
                <th className="px-3 py-2 font-medium">When name contains</th>
                <th className="px-3 py-2 font-medium">Primary</th>
                <th className="px-3 py-2 font-medium">Secondary</th>
                <th className="px-3 py-2"></th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border/40">
              {items.map((r) => (
                <tr key={r.id} className="hover:bg-muted/30">
                  <td className="px-3 py-2 font-mono text-xs">"{r.match}"</td>
                  <td className="px-3 py-2">
                    <CategoryChip name={r.primary} />
                  </td>
                  <td className="px-3 py-2">
                    <CategoryChip name={r.secondary} variant="muted" />
                  </td>
                  <td className="px-3 py-2 text-right">
                    <ConfirmButton
                      onConfirm={async () => {
                        await api.deleteMapping(r.id);
                        notify("success", "Mapping removed");
                        refresh();
                      }}
                    />
                  </td>
                </tr>
              ))}
              {items.length === 0 && !itemsError && (
                <tr>
                  <td colSpan={4} className="px-3 py-8 text-center text-muted-foreground">
                    No mapping rules yet.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </GlassCard>

      <GlassCard title="Add mapping rule">
        <div className="space-y-3">
          <Field label="Match substring (case-insensitive)">
            <input
              value={form.match}
              onChange={(e) => setForm({ ...form, match: e.target.value })}
              placeholder="lidl"
              className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
            />
          </Field>
          <Field label="Primary">
            <Combobox
              value={form.primary}
              onChange={(v) => setForm({ ...form, primary: v })}
              options={cats.primary}
            />
          </Field>
          <Field label="Secondary">
            <Combobox
              value={form.secondary}
              onChange={(v) => setForm({ ...form, secondary: v })}
              options={cats.secondary}
            />
          </Field>
          <button
            onClick={submit}
            className="hover-lift inline-flex w-full items-center justify-center gap-2 rounded-md bg-gradient-brand px-3 py-2 text-sm font-semibold text-background"
          >
            <Plus className="h-4 w-4" /> Add rule
          </button>
        </div>
      </GlassCard>
    </div>
  );
}
