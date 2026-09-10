import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { Plus } from "lucide-react";
import { useApp } from "@/context/AppContext";
import * as api from "@/services/api";
import { formatRef } from "@/services/fx";
import { GlassCard } from "@/components/finguard/GlassCard";
import type { Categories, CategoryTotals, Currency } from "@/services/types";

// Categories page: two side-by-side registries (primary and secondary
// category names) with their all-time expense totals, add and delete
// actions. Fetches with `useEffect` into `services/api.ts` on mount and on
// `AppContext`'s `refreshTick`, following the same pattern as the other
// route files in this app (no TanStack Query).
export const Route = createFileRoute("/categories")({
  head: () => ({ meta: [{ title: "Categories · Finguard" }] }),
  component: CategoriesPage,
});

const EMPTY_TOTALS: CategoryTotals = {
  totals: {},
  unavailable_currencies: [],
  unavailable_currencies_by_category: {},
};

/**
 * This page's one visual treatment for a degraded currency state, matching
 * the styling `ErrorBanner` uses on the expenses and net-worth pages.
 */
function ErrorBanner({ message }: { message: string }) {
  return (
    <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
      {message}
    </div>
  );
}

function CategoriesPage() {
  const { notify, refresh, refreshTick, currencySettings } = useApp();
  const refCurrency = currencySettings.reference_currency;
  const [cats, setCats] = useState<Categories>({ primary: [], secondary: [] });
  const [pri, setPri] = useState<CategoryTotals>(EMPTY_TOTALS);
  const [sec, setSec] = useState<CategoryTotals>(EMPTY_TOTALS);

  useEffect(() => {
    let active = true;
    api.getCategories().then((cats) => active && setCats(cats));
    api.getCategoryTotals("primary").then((pri) => active && setPri(pri));
    api.getCategoryTotals("secondary").then((sec) => active && setSec(sec));
    return () => {
      active = false;
    };
  }, [refreshTick]);

  // Combined for one page-level notice; the delete-guard block in
  // CategoryColumn below still checks each kind's own list, since the
  // backend's refusal is scoped to that kind's totals call.
  const unavailableCurrencies = Array.from(
    new Set([...pri.unavailable_currencies, ...sec.unavailable_currencies]),
  );

  return (
    <div className="space-y-5">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Categories</h1>
        <p className="text-sm text-muted-foreground">Manage your primary and secondary category registries.</p>
      </div>

      {unavailableCurrencies.length > 0 && (
        <ErrorBanner
          message={`Could not resolve exchange rates for ${unavailableCurrencies.join(", ")}. All-time totals below are a lower bound, and deleting a category is disabled until rates are available.`}
        />
      )}

      <div className="grid gap-5 lg:grid-cols-2">
        <CategoryColumn
          title="Primary categories" kind="primary" list={cats.primary} totals={pri.totals} unavailableCurrenciesByCategory={pri.unavailable_currencies_by_category} currency={refCurrency}
          onAdd={async (n) => { await api.addCategory("primary", n); notify("success", `Added "${n}"`); refresh(); }}
          onDelete={async (n) => { await api.deleteCategory("primary", n); notify("success", `Deleted "${n}"`); refresh(); }}
        />
        <CategoryColumn
          title="Secondary categories" kind="secondary" list={cats.secondary} totals={sec.totals} unavailableCurrenciesByCategory={sec.unavailable_currencies_by_category} currency={refCurrency}
          onAdd={async (n) => { await api.addCategory("secondary", n); notify("success", `Added "${n}"`); refresh(); }}
          onDelete={async (n) => { await api.deleteCategory("secondary", n); notify("success", `Deleted "${n}"`); refresh(); }}
        />
      </div>
    </div>
  );
}

function CategoryColumn({
  title, kind, list, totals, unavailableCurrenciesByCategory, currency, onAdd, onDelete,
}: {
  title: string;
  kind: "primary" | "secondary";
  list: string[];
  totals: Record<string, number>;
  unavailableCurrenciesByCategory: Record<string, string[]>;
  currency: Currency;
  onAdd: (n: string) => Promise<void>;
  onDelete: (n: string) => Promise<void>;
}) {
  const [name, setName] = useState("");

  const submit = async () => {
    const v = name.trim();
    if (!v) return;
    await onAdd(v);
    setName("");
  };

  return (
    <GlassCard title={title} action={<span className="text-[11px] text-muted-foreground">{list.length} total</span>}>
      <div className="mb-3 flex gap-2">
        <input value={name} onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && submit()}
          placeholder={`New ${kind} category`}
          className="flex-1 rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none" />
        <button onClick={submit}
          className="hover-lift inline-flex items-center gap-1 rounded-md bg-gradient-brand px-3 py-1.5 text-sm font-semibold text-background">
          <Plus className="h-4 w-4" /> Add
        </button>
      </div>

      <div className="scrollbar-thin max-h-[480px] overflow-y-auto">
        <table className="w-full text-sm">
          <thead>
            <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
              <th className="px-3 py-2 font-medium">Category</th>
              <th className="px-3 py-2 text-right font-medium">All-time total</th>
              <th className="px-3 py-2 text-right font-medium"></th>
            </tr>
          </thead>
          <tbody className="divide-y divide-border/40">
            {list.map((c) => {
              const t = totals[c] ?? 0;
              // This condition mirrors the backend's delete guard in
              // delete_category_handler (backend/src/main.rs). The
              // backend rejects a delete when total.abs() >= 1e-9, using that
              // tolerance for float rounding. The UI shows a delete button only
              // when the backend will accept it.
              const hasExpenses = Math.abs(t) >= 1e-9;
              // NOTE: mirrors delete_category_handler's second refusal reason
              // (backend/src/main.rs), which is still awaiting a product
              // ruling. Remove this block, and the
              // unavailableCurrenciesByCategory prop it depends on, if that
              // refusal is dropped. Scoped to this category's own name (`c`,
              // the same raw name used as the `totals` key and sent to
              // deleteCategory), not the page-wide unavailable_currencies:
              // another category's unresolved currency does not block this
              // one, matching the backend's per-category guard.
              const blockingCurrencies = unavailableCurrenciesByCategory[c] ?? [];
              const ratesUnavailable = blockingCurrencies.length > 0;
              return (
                <tr key={c} className="hover:bg-muted/20">
                  <td className="px-3 py-2 font-medium">{c}</td>
                  <td className="px-3 py-2 text-right tabular-nums">{formatRef(t, currency)}</td>
                  <td className="px-3 py-2 text-right">
                    {hasExpenses ? (
                      <span className="text-[11px] italic text-muted-foreground">has existing expenses</span>
                    ) : ratesUnavailable ? (
                      <span
                        className="text-[11px] italic text-muted-foreground"
                        title={`Cannot delete "${c}" right now: no exchange rate for ${blockingCurrencies.join(", ")}.`}
                      >
                        rates unavailable
                      </span>
                    ) : (
                      <button onClick={() => onDelete(c)}
                        className="rounded-md border border-border px-2 py-1 text-xs text-muted-foreground transition-colors hover:border-destructive/60 hover:text-destructive">
                        Delete
                      </button>
                    )}
                  </td>
                </tr>
              );
            })}
            {list.length === 0 && (
              <tr><td colSpan={3} className="px-3 py-8 text-center text-muted-foreground">No categories yet.</td></tr>
            )}
          </tbody>
        </table>
      </div>
    </GlassCard>
  );
}