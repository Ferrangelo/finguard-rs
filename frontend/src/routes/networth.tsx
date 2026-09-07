import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState, type ReactNode } from "react";
import {
  Area,
  AreaChart,
  CartesianGrid,
  Cell,
  Legend,
  Line,
  Pie,
  PieChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
  ComposedChart,
} from "recharts";
import { ExternalLink, Plus } from "lucide-react";
import { useApp } from "@/context/AppContext";
import * as api from "@/services/api";
import { MONTHS, MONTHS_SHORT } from "@/services/api";
import { CURRENCIES, formatRef, referenceToDisplay } from "@/services/fx";
import { GlassCard } from "@/components/finguard/GlassCard";
import { SubTabs } from "@/components/finguard/SubTabs";
import { MathInput } from "@/components/finguard/MathInput";
import { ConfirmButton } from "@/components/finguard/ConfirmButton";
import { DarkTooltip, useChartColors, LEGEND_STYLE } from "@/components/finguard/DarkTooltip";
import type {
  CreditDebtRow,
  CurrencySettings,
  CurrentMonthRateMode,
  Currency,
  InvestmentAsset,
  InvestmentCategory,
  LiquidityRow,
  MonthlyFxRates,
  NetworthAllocation,
  NetworthEvolution,
} from "@/services/types";
import { useTheme } from "@/context/ThemeContext";

// Net Worth page: investment holdings, liquidity and credits/debts, and
// their combined evolution, split across three sub-tabs (see
// `SUB_OPTIONS`) that share the page's selected year (and, for the total
// view, month) from `AppContext`.
//
// Data flow: like expenses.tsx, this file does not use TanStack Query.
// Each tab fetches its own data with `useEffect` into `services/api.ts`
// and keeps it in local `useState`, refetching whenever `year` or
// `AppContext`'s `refreshTick` changes. Every fetch is preceded by
// `api.ensureYear(year)`, a no-op kept for readability at each call site,
// since the backend auto-creates a year's data files on first access.
//
// Every fetch effect guards its `setState` calls with a per-run `active`
// flag, flipped to `false` in the cleanup function. Without it, switching
// the year (or, on TotalTab, the display currency) fast enough lets an
// older request's response resolve after a newer one and overwrite it,
// showing stale figures with no way to tell them apart from current ones.
//
// Optimistic cell edits: `InvestmentsTab.setCell`, `LiquidityTab.setLiqCell`,
// and `LiquidityTab.setCdCell` update local state immediately (so the input
// reflects the typed value with no round trip latency), then call the
// matching `api.set*Cell` function. If that call rejects, each one notifies
// the error and still calls `refresh()`, so the optimistic value is replaced
// by server truth on the next refetch instead of staying displayed as saved.
//
// Sub-tabs:
// - InvestmentsTab: per-asset monthly quantity, price, or computed value
//   (qty * price), viewed one metric at a time via the holdings/prices/value
//   toggle.
// - LiquidityTab: cash/bank rows and credit/debt rows, each rendered by the
//   shared `LiquiditySection` table component. Its "Total" row converts each
//   row into the reference currency with that row's own currency and the
//   matching calendar month's rate, fetched once per year via
//   `getMonthlyFxRates`.
// - TotalTab: the net worth grid, allocation pie, and evolution chart all
//   come pre-aggregated and pre-converted (into the reference currency) from
//   `getNetworthEvolution`/`getNetworthAllocation`; this file no longer sums
//   raw rows itself. A display-currency selector re-converts each month's
//   reference-currency figure with *that month's own* rate from
//   `getMonthlyFxRates`, rather than one current rate, so the shown curve
//   reflects what the holdings were actually worth in that currency at that
//   time (a deliberate product choice, not an approximation).

// Shared by every tab below, for the two failure modes a fetch on this page
// can hit: the request itself can reject, or it can succeed with a figure
// that is not really known yet (still loading) or not really convertible
// (no exchange rate). Neither may be displayed as if it were real data; see
// `ErrorBanner` and `fmtOrDash`.

/** Extracts a readable message from a caught value. A rejected fetch can throw anything, not only an `Error`. */
function errorMessage(err: unknown, fallback: string): string {
  return err instanceof Error ? err.message : fallback;
}

/**
 * This page's one visual treatment for "a request failed", so a genuine
 * fetch failure never renders as the empty-data state it would otherwise be
 * indistinguishable from. Every tab below reuses it instead of inventing
 * its own error styling.
 */
function ErrorBanner({ message }: { message: string }) {
  return (
    <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
      {message}
    </div>
  );
}

/**
 * Renders a reference-currency amount, or a marked dash when `value` is
 * `null`. `null` here always means "no exchange rate was available to
 * compute this", which must never be conflated with a real balance of
 * zero, so this is used instead of calling `formatRef` directly wherever a
 * figure could be missing.
 */
function fmtOrDash(value: number | null, currency: Currency): ReactNode {
  if (value === null) {
    return (
      <span className="text-warning" title="No exchange rate available for this period">
        —
      </span>
    );
  }
  return formatRef(value, currency);
}

export const Route = createFileRoute("/networth")({
  head: () => ({ meta: [{ title: "Net Worth · Finguard" }] }),
  component: NetWorthPage,
});

type Sub = "investments" | "liquidity" | "total";
const SUB_OPTIONS: ReadonlyArray<{ value: Sub; label: string }> = [
  { value: "investments", label: "Investments" },
  { value: "liquidity", label: "Liquidity & Debts" },
  { value: "total", label: "Total Net Worth" },
];

function NetWorthPage() {
  const [sub, setSub] = useState<Sub>("investments");
  // Read once here and thread down explicitly, rather than each tab calling
  // useApp() for currency settings itself, matching cashflow.tsx and
  // categories.tsx.
  const { currencySettings } = useApp();
  const refCurrency = currencySettings.reference_currency;
  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">Net Worth</h1>
          <p className="text-sm text-muted-foreground">
            Investments, liquidity and the full evolution of your wealth.
          </p>
        </div>
        <SubTabs value={sub} onChange={setSub} options={SUB_OPTIONS} />
      </div>
      {sub === "investments" && <InvestmentsTab refCurrency={refCurrency} />}
      {sub === "liquidity" && <LiquidityTab refCurrency={refCurrency} />}
      {sub === "total" && <TotalTab currencySettings={currencySettings} />}
    </div>
  );
}

const INV_CATS: InvestmentCategory[] = ["Stocks/ETF", "Commodities", "Bonds"];
const LIQ_CATS = ["Bank/Broker account", "Cash", "Other"] as const;

// ────────────────────────────────────────────────────────────── Investments
function InvestmentsTab({ refCurrency }: { refCurrency: Currency }) {
  const colorAt = useChartColors();
  const { year, notify, refresh, refreshTick } = useApp();
  const [assets, setAssets] = useState<InvestmentAsset[]>([]);
  const [view, setView] = useState<"holdings" | "prices" | "value">("value");
  const [adding, setAdding] = useState(false);
  const [editId, setEditId] = useState<string | null>(null);
  // Distinct from `assets` staying empty: a fetch failure must not render
  // as "No investments yet.", since that reads as "you own nothing", the
  // opposite of a load error.
  const [loadError, setLoadError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    setLoadError(null);
    api
      .ensureYear(year)
      .then(() => api.getInvestments(year))
      .then((assets) => active && setAssets(assets))
      .catch((err) => {
        if (active) setLoadError(errorMessage(err, "Failed to load investments"));
      });
    return () => {
      active = false;
    };
  }, [year, refreshTick]);

  // Optimistic update: see the file-level comment on cell edits above.
  const setCell = async (id: string, m: number, field: "qty" | "price", v: number) => {
    setAssets((prev) =>
      prev.map((a) => {
        if (a.id !== id) return a;
        const d = { ...a.data };
        if (!d[year]) d[year] = {};
        const cur = d[year][m] ?? { qty: 0, price: 0 };
        d[year] = { ...d[year], [m]: { ...cur, [field]: v } };
        return { ...a, data: d };
      }),
    );
    try {
      await api.setInvestmentCell(id, year, m, field, v);
      notify("success", "Saved");
    } catch (err) {
      notify("error", err instanceof Error ? err.message : "Save failed");
    }
    refresh();
  };

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <SubTabs
          value={view}
          onChange={setView}
          options={[
            { value: "holdings", label: "Holdings (qty)" },
            { value: "prices", label: "Prices" },
            { value: "value", label: "Value (qty × price)" },
          ]}
        />
        <button
          onClick={() => setAdding(true)}
          className="hover-lift ml-auto inline-flex items-center gap-1 rounded-md bg-gradient-brand px-3 py-1.5 text-sm font-semibold text-background"
        >
          <Plus className="h-4 w-4" /> Add asset
        </button>
      </div>

      {adding && (
        <AddInvestmentForm
          refCurrency={refCurrency}
          onCancel={() => setAdding(false)}
          onCreate={async (name, cat, link, currency) => {
            await api.addInvestment(name, cat, link, year, currency);
            notify("success", `Added "${name}"`);
            setAdding(false);
            refresh();
          }}
        />
      )}

      {loadError && <ErrorBanner message={`Could not load investments: ${loadError}`} />}

      <GlassCard title={`Investments · ${year}`}>
        <div className="scrollbar-thin overflow-x-auto">
          <table className="w-full min-w-[1200px] text-sm">
            <thead>
              <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
                <th className="px-3 py-2 font-medium">Asset</th>
                <th className="px-3 py-2 font-medium">Category</th>
                <th className="px-3 py-2 font-medium">Currency</th>
                <th className="px-3 py-2 font-medium">Link</th>
                {MONTHS_SHORT.map((m) => (
                  <th key={m} className="px-2 py-2 text-right font-medium">
                    {m}
                  </th>
                ))}
                <th className="px-3 py-2 text-right font-medium"></th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border/40">
              {assets.map((a) => {
                const isEdit = editId === a.id;
                return (
                  <tr key={a.id} className="hover:bg-muted/20">
                    <td className="px-3 py-1.5">
                      {isEdit ? (
                        <input
                          defaultValue={a.name}
                          onBlur={(e) =>
                            api
                              .updateInvestmentMeta(a.id, { name: e.target.value }, year)
                              .then(refresh)
                          }
                          className="rounded border border-border bg-surface/60 px-2 py-0.5 text-sm"
                        />
                      ) : (
                        <span className="font-medium">{a.name}</span>
                      )}
                    </td>
                    <td className="px-3 py-1.5">
                      {isEdit ? (
                        <select
                          defaultValue={a.category}
                          onChange={(e) =>
                            api
                              .updateInvestmentMeta(
                                a.id,
                                { category: e.target.value as InvestmentCategory },
                                year,
                              )
                              .then(refresh)
                          }
                          className="rounded border border-border bg-surface/60 px-2 py-0.5 text-xs"
                        >
                          {INV_CATS.map((c) => (
                            <option key={c} value={c}>
                              {c}
                            </option>
                          ))}
                        </select>
                      ) : (
                        <span className="inline-flex items-center rounded-md border border-border bg-muted/30 px-2 py-0.5 text-xs">
                          {a.category}
                        </span>
                      )}
                    </td>
                    <td className="px-3 py-1.5">
                      {isEdit ? (
                        <select
                          defaultValue={a.currency}
                          onChange={(e) =>
                            api
                              .updateInvestmentMeta(
                                a.id,
                                { currency: e.target.value as Currency },
                                year,
                              )
                              .then(refresh)
                          }
                          className="rounded border border-border bg-surface/60 px-2 py-0.5 text-xs"
                        >
                          {CURRENCIES.map((c) => (
                            <option key={c} value={c}>
                              {c}
                            </option>
                          ))}
                        </select>
                      ) : (
                        <span className="text-xs text-muted-foreground">{a.currency}</span>
                      )}
                    </td>
                    <td className="px-3 py-1.5">
                      {isEdit ? (
                        <input
                          defaultValue={a.link ?? ""}
                          onBlur={(e) =>
                            api
                              .updateInvestmentMeta(a.id, { link: e.target.value }, year)
                              .then(refresh)
                          }
                          className="w-32 rounded border border-border bg-surface/60 px-2 py-0.5 text-xs"
                        />
                      ) : a.link ? (
                        <a
                          href={a.link}
                          target="_blank"
                          rel="noreferrer"
                          className="inline-flex items-center gap-1 text-xs text-primary hover:underline"
                        >
                          link <ExternalLink className="h-3 w-3" />
                        </a>
                      ) : (
                        <span className="text-xs text-muted-foreground/60">—</span>
                      )}
                    </td>
                    {MONTHS_SHORT.map((_, i) => {
                      const m = i + 1;
                      const cell = a.data[year]?.[m] ?? { qty: 0, price: 0 };
                      if (view === "value") {
                        return (
                          <td key={m} className="px-2 py-1.5 text-right tabular-nums text-xs">
                            {(cell.qty * cell.price).toFixed(0)}
                          </td>
                        );
                      }
                      const field = view === "holdings" ? "qty" : "price";
                      return (
                        <td key={m} className="px-1 py-1">
                          <MathInput
                            value={cell[field]}
                            onCommit={(v) => setCell(a.id, m, field, v)}
                          />
                        </td>
                      );
                    })}
                    <td className="px-3 py-1.5 text-right">
                      <div className="flex justify-end gap-1">
                        <button
                          onClick={() => setEditId(isEdit ? null : a.id)}
                          className="rounded-md border border-border px-2 py-0.5 text-xs text-muted-foreground hover:text-primary"
                        >
                          {isEdit ? "Done" : "Edit"}
                        </button>
                        <ConfirmButton
                          onConfirm={async () => {
                            await api.deleteInvestment(a.id, year);
                            notify("success", `Removed ${a.name}`);
                            refresh();
                          }}
                        />
                      </div>
                    </td>
                  </tr>
                );
              })}
              {assets.length === 0 && !loadError && (
                <tr>
                  <td colSpan={17} className="px-3 py-8 text-center text-muted-foreground">
                    No investments yet.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </GlassCard>
    </div>
  );
}

function AddInvestmentForm({
  refCurrency,
  onCreate,
  onCancel,
}: {
  refCurrency: Currency;
  onCreate: (
    name: string,
    cat: InvestmentCategory,
    link: string | undefined,
    currency: Currency,
  ) => Promise<void>;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [cat, setCat] = useState<InvestmentCategory>("Stocks/ETF");
  const [link, setLink] = useState("");
  const [currency, setCurrency] = useState<Currency>(refCurrency);
  return (
    <GlassCard title="New investment asset">
      <div className="grid gap-3 md:grid-cols-[2fr_1fr_1fr_2fr_auto]">
        <input
          placeholder="Asset name"
          value={name}
          onChange={(e) => setName(e.target.value)}
          className="rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
        />
        <select
          value={cat}
          onChange={(e) => setCat(e.target.value as InvestmentCategory)}
          className="rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm"
        >
          {INV_CATS.map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </select>
        <select
          value={currency}
          onChange={(e) => setCurrency(e.target.value as Currency)}
          className="rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm"
        >
          {CURRENCIES.map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </select>
        <input
          placeholder="Link (optional)"
          value={link}
          onChange={(e) => setLink(e.target.value)}
          className="rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
        />
        <div className="flex gap-2">
          <button
            onClick={() => name.trim() && onCreate(name.trim(), cat, link || undefined, currency)}
            className="rounded-md bg-gradient-brand px-3 py-1.5 text-sm font-semibold text-background"
          >
            Create
          </button>
          <button
            onClick={onCancel}
            className="rounded-md border border-border px-3 py-1.5 text-sm text-muted-foreground"
          >
            Cancel
          </button>
        </div>
      </div>
    </GlassCard>
  );
}

// ────────────────────────────────────────────────────────────── Liquidity & Credits/Debts
function LiquidityTab({ refCurrency }: { refCurrency: Currency }) {
  const { year, notify, refresh, refreshTick } = useApp();
  const [liq, setLiq] = useState<LiquidityRow[]>([]);
  const [cd, setCd] = useState<CreditDebtRow[]>([]);
  // Rates for every currency already used by this year's liquidity and
  // credits/debts rows (the default `getMonthlyFxRates` currency set), used
  // to convert each row's own balance into the reference currency for the
  // section totals below.
  const [rates, setRates] = useState<MonthlyFxRates | null>(null);
  const [liqError, setLiqError] = useState<string | null>(null);
  const [cdError, setCdError] = useState<string | null>(null);
  // Set only once the rates request settles, successfully or not, so
  // `rates === null && !ratesError` unambiguously means "still loading"
  // (see `toRefAmount`, which depends on telling that apart from "resolved,
  // but this currency has no rate").
  const [ratesError, setRatesError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    setLiqError(null);
    setCdError(null);
    setRatesError(null);
    api.ensureYear(year).then(() => {
      api
        .getLiquidity(year)
        .then((rows) => active && setLiq(rows))
        .catch((err) => {
          if (active) setLiqError(errorMessage(err, "Failed to load liquidity rows"));
        });
      api
        .getCreditsDebts(year)
        .then((rows) => active && setCd(rows))
        .catch((err) => {
          if (active) setCdError(errorMessage(err, "Failed to load credit/debt rows"));
        });
      api
        .getMonthlyFxRates(year)
        .then((rates) => active && setRates(rates))
        .catch((err) => {
          if (active) setRatesError(errorMessage(err, "Failed to load exchange rates"));
        });
    });
    return () => {
      active = false;
    };
  }, [year, refreshTick]);

  // Converts a row's own balance into the reference currency at month `m`'s
  // own rate, reporting which of three states applies rather than always
  // returning a number: `ok` (converted), `loading` (the rates request has
  // not settled yet), or `unavailable` (it settled, successfully or not,
  // but this currency still has no rate for this month). Collapsing these
  // into a plain number, with 0 for the two failure cases, is exactly the
  // bug this type exists to prevent: a missing row would silently vanish
  // from the Total row's sum instead of being visibly excluded.
  const toRefAmount = (amount: number, currency: Currency, m: number): RefAmount => {
    if (currency === refCurrency) return { kind: "ok", value: amount };
    if (rates === null) return { kind: ratesError ? "unavailable" : "loading" };
    const rate = rates.months.find((r) => r.month === m)?.rate_to_reference[currency];
    return rate ? { kind: "ok", value: amount * rate } : { kind: "unavailable" };
  };

  // Optimistic updates: see the file-level comment on cell edits above.
  const setLiqCell = async (id: string, m: number, v: number) => {
    setLiq((prev) =>
      prev.map((r) =>
        r.id !== id
          ? r
          : { ...r, data: { ...r.data, [year]: { ...(r.data[year] ?? {}), [m]: v } } },
      ),
    );
    try {
      await api.setLiquidityCell(id, year, m, v);
      notify("success", "Saved");
    } catch (err) {
      notify("error", err instanceof Error ? err.message : "Save failed");
    }
    refresh();
  };
  const setCdCell = async (id: string, m: number, v: number) => {
    setCd((prev) =>
      prev.map((r) =>
        r.id !== id
          ? r
          : { ...r, data: { ...r.data, [year]: { ...(r.data[year] ?? {}), [m]: v } } },
      ),
    );
    try {
      await api.setCreditDebtCell(id, year, m, v);
      notify("success", "Saved");
    } catch (err) {
      notify("error", err instanceof Error ? err.message : "Save failed");
    }
    refresh();
  };

  return (
    <div className="space-y-5">
      {liqError && <ErrorBanner message={`Could not load liquidity rows: ${liqError}`} />}
      {cdError && <ErrorBanner message={`Could not load credit/debt rows: ${cdError}`} />}
      {ratesError && (
        <ErrorBanner
          message={`Could not load exchange rates: ${ratesError}. Totals below may be incomplete.`}
        />
      )}
      <LiquiditySection
        title="Liquidity"
        rows={liq}
        year={year}
        refCurrency={refCurrency}
        valueFor={(r, m) => toRefAmount(r.data[year]?.[m] ?? 0, r.currency, m)}
        onCellCommit={setLiqCell}
        renderAddForm={() => (
          <AddLiquidityForm
            refCurrency={refCurrency}
            onCreate={async (n, c, cur) => {
              await api.addLiquidity(n, c, cur, year);
              notify("success", `Added ${n}`);
              refresh();
            }}
          />
        )}
        renderMeta={(r) => (
          <>
            <td className="px-3 py-1.5">
              <select
                defaultValue={r.category}
                onChange={(e) =>
                  api
                    .updateLiquidityMeta(
                      r.id,
                      { category: e.target.value as LiquidityRow["category"] },
                      year,
                    )
                    .then(refresh)
                }
                className="rounded border border-border bg-surface/60 px-2 py-0.5 text-xs"
              >
                {LIQ_CATS.map((c) => (
                  <option key={c} value={c}>
                    {c}
                  </option>
                ))}
              </select>
            </td>
            <td className="px-3 py-1.5">
              <select
                defaultValue={r.currency}
                onChange={(e) =>
                  api
                    .updateLiquidityMeta(r.id, { currency: e.target.value as Currency }, year)
                    .then(refresh)
                }
                className="rounded border border-border bg-surface/60 px-2 py-0.5 text-xs"
              >
                {CURRENCIES.map((c) => (
                  <option key={c} value={c}>
                    {c}
                  </option>
                ))}
              </select>
            </td>
          </>
        )}
        metaHeaders={["Category", "Curr"]}
        renderActions={(r) => (
          <ConfirmButton
            onConfirm={async () => {
              await api.deleteLiquidity(r.id, year);
              notify("success", `Removed ${r.name}`);
              refresh();
            }}
          />
        )}
        renderEditName={(r) => (
          <input
            defaultValue={r.name}
            onBlur={(e) =>
              api.updateLiquidityMeta(r.id, { name: e.target.value }, year).then(refresh)
            }
            className="rounded border border-transparent bg-transparent px-1 py-0.5 text-sm font-medium hover:border-border focus:border-primary/60 focus:bg-surface/60 focus:outline-none"
          />
        )}
      />

      <LiquiditySection
        title="Credits & Debts"
        rows={cd}
        year={year}
        refCurrency={refCurrency}
        valueFor={(r, m) => toRefAmount(r.data[year]?.[m] ?? 0, r.currency, m)}
        onCellCommit={setCdCell}
        renderAddForm={() => (
          <AddCreditDebtForm
            refCurrency={refCurrency}
            onCreate={async (n, cur) => {
              await api.addCreditDebt(n, cur, year);
              notify("success", `Added ${n}`);
              refresh();
            }}
          />
        )}
        renderMeta={(r) => (
          <td className="px-3 py-1.5">
            <select
              defaultValue={r.currency}
              onChange={(e) =>
                api
                  .updateCreditDebtMeta(r.id, { currency: e.target.value as Currency }, year)
                  .then(refresh)
              }
              className="rounded border border-border bg-surface/60 px-2 py-0.5 text-xs"
            >
              {CURRENCIES.map((c) => (
                <option key={c} value={c}>
                  {c}
                </option>
              ))}
            </select>
          </td>
        )}
        metaHeaders={["Curr"]}
        renderActions={(r) => (
          <ConfirmButton
            onConfirm={async () => {
              await api.deleteCreditDebt(r.id, year);
              notify("success", `Removed ${r.name}`);
              refresh();
            }}
          />
        )}
        renderEditName={(r) => (
          <input
            defaultValue={r.name}
            onBlur={(e) =>
              api.updateCreditDebtMeta(r.id, { name: e.target.value }, year).then(refresh)
            }
            className="rounded border border-transparent bg-transparent px-1 py-0.5 text-sm font-medium hover:border-border focus:border-primary/60 focus:bg-surface/60 focus:outline-none"
          />
        )}
        signed
      />
    </div>
  );
}

interface BaseRow {
  id: string;
  name: string;
  currency: Currency;
  data: Record<number, Record<number, number>>;
}

/**
 * One row's contribution to a `LiquiditySection` total for one month: `ok`
 * carries the converted amount, `loading` means the rates needed to
 * convert it have not arrived yet, and `unavailable` means they arrived (or
 * failed to) without a usable rate for this row's currency. Kept apart
 * instead of always being a number, since `LiquiditySection`'s total row
 * must render `loading` and `unavailable` differently from a real value,
 * and never as 0.
 */
type RefAmount = { kind: "ok"; value: number } | { kind: "loading" } | { kind: "unavailable" };

/**
 * Generic monthly editable table shared by the Liquidity and Credits/Debts
 * sections. `R` is the row type (`LiquidityRow` or `CreditDebtRow`); the
 * caller supplies `valueFor` to read a cell already converted into
 * `refCurrency` and the `render*` props to customize the name cell, per-row
 * metadata columns, and row actions without this component needing to know
 * about categories or currencies directly. Set `signed` to color negative
 * and positive values (used for Credits/Debts, where a row can be a debt or
 * a credit).
 */
function LiquiditySection<R extends BaseRow>({
  title,
  rows,
  year,
  refCurrency,
  valueFor,
  onCellCommit,
  renderAddForm,
  renderMeta,
  metaHeaders,
  renderActions,
  renderEditName,
  signed,
}: {
  title: string;
  rows: R[];
  year: number;
  refCurrency: Currency;
  valueFor: (r: R, m: number) => RefAmount;
  onCellCommit: (id: string, m: number, v: number) => void;
  renderAddForm: () => React.ReactNode;
  renderMeta: (r: R) => React.ReactNode;
  metaHeaders: string[];
  renderActions: (r: R) => React.ReactNode;
  renderEditName: (r: R) => React.ReactNode;
  signed?: boolean;
}) {
  const months = Array.from({ length: 12 }, (_, i) => i + 1);
  // A month's total is `loading` if any row's conversion is still pending,
  // `incomplete` if every row settled but at least one could not convert
  // (the sum below excludes it rather than treating it as 0), and `ok`
  // only when every row resolved.
  type Total =
    | { kind: "loading" }
    | { kind: "incomplete"; value: number }
    | { kind: "ok"; value: number };
  const totals: Total[] = months.map((m) => {
    const cells = rows.map((r) => valueFor(r, m));
    if (cells.some((c) => c.kind === "loading")) return { kind: "loading" };
    const value = cells.reduce((s, c) => s + (c.kind === "ok" ? c.value : 0), 0);
    return cells.some((c) => c.kind === "unavailable")
      ? { kind: "incomplete", value }
      : { kind: "ok", value };
  });

  return (
    <GlassCard title={title} action={renderAddForm()}>
      <div className="scrollbar-thin overflow-x-auto">
        <table className="w-full min-w-[1100px] text-sm">
          <thead>
            <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
              <th className="px-3 py-2 font-medium">Name</th>
              {metaHeaders.map((h) => (
                <th key={h} className="px-3 py-2 font-medium">
                  {h}
                </th>
              ))}
              {MONTHS_SHORT.map((m) => (
                <th key={m} className="px-2 py-2 text-right font-medium">
                  {m}
                </th>
              ))}
              <th className="px-3 py-2"></th>
            </tr>
          </thead>
          <tbody className="divide-y divide-border/40">
            {rows.map((r) => (
              <tr key={r.id} className="hover:bg-muted/20">
                <td className="px-3 py-1.5">{renderEditName(r)}</td>
                {renderMeta(r)}
                {months.map((m) => {
                  const v = r.data[year]?.[m] ?? 0;
                  const color = signed
                    ? v < 0
                      ? "text-destructive"
                      : v > 0
                        ? "text-success"
                        : ""
                    : "";
                  return (
                    <td key={m} className="px-1 py-1">
                      <MathInput
                        value={v}
                        onCommit={(n) => onCellCommit(r.id, m, n)}
                        className={color}
                      />
                    </td>
                  );
                })}
                <td className="px-3 py-1.5 text-right">{renderActions(r)}</td>
              </tr>
            ))}
            <tr className="bg-muted/30 font-semibold">
              <td className="px-3 py-2">Total ({refCurrency})</td>
              {metaHeaders.map((h) => (
                <td key={h} />
              ))}
              {totals.map((t, i) => (
                <td key={i} className="px-2 py-2 text-right tabular-nums">
                  {t.kind === "loading" ? (
                    <span className="text-muted-foreground" title="Loading exchange rates">
                      …
                    </span>
                  ) : t.kind === "incomplete" ? (
                    <span
                      className="text-warning"
                      title="One or more currencies could not be converted for this month; this total excludes them"
                    >
                      {formatRef(t.value, refCurrency)}*
                    </span>
                  ) : (
                    <span className={signed && t.value < 0 ? "text-destructive" : ""}>
                      {formatRef(t.value, refCurrency)}
                    </span>
                  )}
                </td>
              ))}
              <td />
            </tr>
          </tbody>
        </table>
      </div>
    </GlassCard>
  );
}

function AddLiquidityForm({
  refCurrency,
  onCreate,
}: {
  refCurrency: Currency;
  onCreate: (n: string, c: LiquidityRow["category"], cur: Currency) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [cat, setCat] = useState<LiquidityRow["category"]>("Bank/Broker account");
  const [cur, setCur] = useState<Currency>(refCurrency);
  return (
    <div className="flex flex-wrap items-center gap-2">
      <input
        placeholder="Name"
        value={name}
        onChange={(e) => setName(e.target.value)}
        className="w-32 rounded-md border border-border bg-surface/60 px-2 py-1 text-xs"
      />
      <select
        value={cat}
        onChange={(e) => setCat(e.target.value as LiquidityRow["category"])}
        className="rounded-md border border-border bg-surface/60 px-2 py-1 text-xs"
      >
        {LIQ_CATS.map((c) => (
          <option key={c} value={c}>
            {c}
          </option>
        ))}
      </select>
      <select
        value={cur}
        onChange={(e) => setCur(e.target.value as Currency)}
        className="rounded-md border border-border bg-surface/60 px-2 py-1 text-xs"
      >
        {CURRENCIES.map((c) => (
          <option key={c} value={c}>
            {c}
          </option>
        ))}
      </select>
      <button
        onClick={() => name.trim() && (onCreate(name.trim(), cat, cur), setName(""))}
        className="inline-flex items-center gap-1 rounded-md bg-gradient-brand px-2.5 py-1 text-xs font-semibold text-background"
      >
        <Plus className="h-3 w-3" /> Add
      </button>
    </div>
  );
}

function AddCreditDebtForm({
  refCurrency,
  onCreate,
}: {
  refCurrency: Currency;
  onCreate: (n: string, cur: Currency) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [cur, setCur] = useState<Currency>(refCurrency);
  return (
    <div className="flex flex-wrap items-center gap-2">
      <input
        placeholder="Name"
        value={name}
        onChange={(e) => setName(e.target.value)}
        className="w-32 rounded-md border border-border bg-surface/60 px-2 py-1 text-xs"
      />
      <select
        value={cur}
        onChange={(e) => setCur(e.target.value as Currency)}
        className="rounded-md border border-border bg-surface/60 px-2 py-1 text-xs"
      >
        {CURRENCIES.map((c) => (
          <option key={c} value={c}>
            {c}
          </option>
        ))}
      </select>
      <button
        onClick={() => name.trim() && (onCreate(name.trim(), cur), setName(""))}
        className="inline-flex items-center gap-1 rounded-md bg-gradient-brand px-2.5 py-1 text-xs font-semibold text-background"
      >
        <Plus className="h-3 w-3" /> Add
      </button>
    </div>
  );
}

// ────────────────────────────────────────────────────────────── Total Net Worth
function TotalTab({ currencySettings }: { currencySettings: CurrencySettings }) {
  const colorAt = useChartColors();
  const { year, month, refreshTick, notify, refresh } = useApp();
  const { theme } = useTheme();
  const totalStroke = theme === "arctic" ? "oklch(0.30 0.10 260)" : "oklch(0.95 0.02 260)";
  const tickColor = theme === "arctic" ? "oklch(0.48 0.022 240)" : "oklch(0.68 0.02 260)";
  const refCurrency = currencySettings.reference_currency;

  // The grid, pie, and evolution chart all come pre-aggregated and
  // pre-converted into `refCurrency` from the backend; this tab no longer
  // sums raw investment/liquidity/credits-debts rows itself. `null` means
  // either the backend found nothing to report (every value zero) for that
  // year/month, or the request has not resolved yet; `evolutionError`
  // and `allocationError` are what distinguish a genuine empty year from a
  // failed request, since both would otherwise leave the value at `null`.
  const [evolution, setEvolution] = useState<NetworthEvolution | null>(null);
  const [allocation, setAllocation] = useState<NetworthAllocation | null>(null);
  const [evolutionError, setEvolutionError] = useState<string | null>(null);
  const [allocationError, setAllocationError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    setEvolutionError(null);
    setAllocationError(null);
    api.ensureYear(year).then(() => {
      api
        .getNetworthEvolution(year)
        .then((e) => active && setEvolution(e))
        .catch((err) => {
          if (active) setEvolutionError(errorMessage(err, "Failed to load net worth evolution"));
        });
      api
        .getNetworthAllocation(year, month)
        .then((a) => active && setAllocation(a))
        .catch((err) => {
          if (active) setAllocationError(errorMessage(err, "Failed to load net worth allocation"));
        });
    });
    return () => {
      active = false;
    };
  }, [year, month, refreshTick]);

  // December of the prior year's net worth (reference currency), the
  // baseline for January's month-over-month delta. `getNetworthEvolution`
  // only ever returns `year`'s own 12 months, so the prior December figure
  // needs its own request against `year - 1`. A `null` response there means
  // no prior-year data at all, which this treats as a baseline of 0, the
  // same value the row cells themselves default to when unset; a rejected
  // request is a different case, tracked by `prevDecError` and left at
  // whatever the last successful `prevDecNetWorth` was, so January's delta
  // is marked unresolved below rather than measured against a wrong 0.
  const [prevDecNetWorth, setPrevDecNetWorth] = useState(0);
  const [prevDecError, setPrevDecError] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    setPrevDecError(null);
    api
      .getNetworthEvolution(year - 1)
      .then((e) => {
        if (active) setPrevDecNetWorth(e ? e.net_worth[11] : 0);
      })
      .catch((err) => {
        if (active)
          setPrevDecError(errorMessage(err, "Failed to load last year's December net worth"));
      });
    return () => {
      active = false;
    };
  }, [year, refreshTick]);

  // Display currency: defaults to the reference currency until the user
  // explicitly picks another one.
  const [displayCurrencyChoice, setDisplayCurrencyChoice] = useState<Currency | null>(null);
  const displayCurrency = displayCurrencyChoice ?? refCurrency;

  // Rates for the display currency, one per calendar month of `year` plus
  // December of `year - 1` for the January baseline. Fetched only when the
  // display currency differs from the reference currency, since the
  // reference currency needs no conversion at all. Reset to `null` (and
  // their errors cleared) on every re-run, including a plain display
  // currency switch, so `displayRates === null` reliably means "loading"
  // for whichever currency is currently selected, never stale data left
  // over from the previous one.
  const [displayRates, setDisplayRates] = useState<MonthlyFxRates | null>(null);
  const [prevDisplayRates, setPrevDisplayRates] = useState<MonthlyFxRates | null>(null);
  const [displayRatesError, setDisplayRatesError] = useState<string | null>(null);
  const [prevDisplayRatesError, setPrevDisplayRatesError] = useState<string | null>(null);
  useEffect(() => {
    let active = true;
    setDisplayRates(null);
    setPrevDisplayRates(null);
    setDisplayRatesError(null);
    setPrevDisplayRatesError(null);
    if (displayCurrency !== refCurrency) {
      api
        .getMonthlyFxRates(year, [displayCurrency])
        .then((r) => active && setDisplayRates(r))
        .catch((err) => {
          if (active)
            setDisplayRatesError(
              errorMessage(err, "Failed to load display-currency exchange rates"),
            );
        });
      api
        .getMonthlyFxRates(year - 1, [displayCurrency])
        .then((r) => active && setPrevDisplayRates(r))
        .catch((err) => {
          if (active)
            setPrevDisplayRatesError(
              errorMessage(err, "Failed to load prior-year display-currency exchange rates"),
            );
        });
    }
    return () => {
      active = false;
    };
  }, [year, displayCurrency, refCurrency, refreshTick]);

  // A fetch failure is treated the same as the backend reporting the
  // currency itself unavailable: either way there is no reliable rate to
  // show `displayCurrency` with, so both fall back to the reference
  // currency with the same warning banner rather than each needing its own
  // UI.
  const displayUnavailable =
    (displayRates?.unavailable_currencies.includes(displayCurrency) ?? false) ||
    displayRatesError !== null ||
    prevDisplayRatesError !== null;
  // Falls back to the reference currency, with a warning banner below,
  // rather than rendering a blank or wrong figure when the chosen display
  // currency has no resolved rate at all.
  const shownCurrency = displayUnavailable ? refCurrency : displayCurrency;
  // True only in the window between picking a non-reference display
  // currency and its rates request resolving. Gates the grid, pie, and
  // evolution chart below so they render a loading state instead of
  // briefly showing every figure as 0, which is what happened before this
  // fix: `convert`/`convertPrevDec` had no way to tell "not loaded yet"
  // apart from "no rate at all", and defaulted both to 0.
  const ratesLoading =
    shownCurrency !== refCurrency &&
    ((displayRates === null && displayRatesError === null) ||
      (prevDisplayRates === null && prevDisplayRatesError === null));

  // Converts a reference-currency amount into `shownCurrency` using *that
  // calendar month's own* rate, not one current rate: the user chose this
  // deliberately, since a single rate would hide currency movement inside
  // the portfolio curve. Returns `null`, via `referenceToDisplay`, instead
  // of a fabricated 0 when no rate resolved for this month; callers must
  // render that with `fmtOrDash` rather than treating it as a real amount.
  // By the time this runs `ratesLoading` has already gated the section that
  // calls it, so a `null` here reflects a genuine per-month gap, not the
  // rates simply not having arrived yet.
  const convert = (amountInReference: number, m: number): number | null => {
    if (shownCurrency === refCurrency) return amountInReference;
    const rate = displayRates?.months.find((r) => r.month === m)?.rate_to_reference[shownCurrency];
    return referenceToDisplay(amountInReference, rate);
  };
  const convertPrevDec = (amountInReference: number): number | null => {
    if (shownCurrency === refCurrency) return amountInReference;
    const rate = prevDisplayRates?.months.find((r) => r.month === 12)?.rate_to_reference[
      shownCurrency
    ];
    return referenceToDisplay(amountInReference, rate);
  };

  const updateRateMode = async (mode: CurrentMonthRateMode) => {
    try {
      await api.updateCurrencySettings({ ...currencySettings, current_month_rate_mode: mode });
      notify("success", "Updated current-month rate mode");
    } catch (err) {
      notify("error", err instanceof Error ? err.message : "Update failed");
    }
    refresh();
  };

  // Sends both settings fields, since `updateCurrencySettings` replaces the
  // whole object and would otherwise reset `current_month_rate_mode` to
  // whatever this closure captured. `refresh()` re-fetches `currencySettings`
  // from `AppContext`, so a rejected change (e.g. an unsupported code)
  // reverts the select to server truth instead of leaving it showing a value
  // the backend never saved.
  const updateReferenceCurrency = async (currency: Currency) => {
    try {
      await api.updateCurrencySettings({ ...currencySettings, reference_currency: currency });
      notify("success", "Updated reference currency");
    } catch (err) {
      notify("error", err instanceof Error ? err.message : "Update failed");
    }
    refresh();
  };

  const months = Array.from({ length: 12 }, (_, i) => i + 1);
  const zeros = (): number[] => Array(12).fill(0);

  // Every array below is `(number | null)[]`, not `number[]`: `null` marks
  // a month `convert` could not resolve. `ratesLoading` (checked in the
  // render below) keeps that from ever meaning "still loading" here, so a
  // `null` this far down always reflects a genuine per-month gap.
  const invByCat: Record<InvestmentCategory, (number | null)[]> = {
    "Stocks/ETF": zeros(),
    Commodities: zeros(),
    Bonds: zeros(),
  };
  let liqTotal: (number | null)[] = zeros();
  let cdTotal: (number | null)[] = zeros();
  if (evolution) {
    for (const c of INV_CATS) {
      const series = evolution.components.find((s) => s.name === c)?.values ?? zeros();
      invByCat[c] = series.map((v, i) => convert(v, i + 1));
    }
    const liqSeries = evolution.components.find((s) => s.name === "Liquidity")?.values ?? zeros();
    const cdSeries =
      evolution.components.find((s) => s.name === "Credits/Debts")?.values ?? zeros();
    liqTotal = liqSeries.map((v, i) => convert(v, i + 1));
    cdTotal = cdSeries.map((v, i) => convert(v, i + 1));
  }
  // A month's investment total is only known when every category resolved
  // for it; otherwise it propagates `null` rather than silently summing
  // just the categories that did.
  const invTotal: (number | null)[] = months.map((_, i) => {
    const vals = INV_CATS.map((c) => invByCat[c][i]);
    return vals.some((v) => v === null) ? null : (vals as number[]).reduce((s, v) => s + v, 0);
  });
  const total: (number | null)[] = evolution
    ? evolution.net_worth.map((v, i) => convert(v, i + 1))
    : zeros();

  // Each month's delta is against the prior month's total, except January,
  // which is measured against decPrev (December of the prior year,
  // converted at that same December's own rate). `prevDecError` means the
  // reference-currency baseline itself never loaded, so there is no
  // trustworthy figure to convert regardless of display currency.
  const decPrev: number | null = prevDecError ? null : convertPrevDec(prevDecNetWorth);
  const delta: (number | null)[] = total.map((t, i) => {
    const base = i === 0 ? decPrev : total[i - 1];
    return t === null || base === null ? null : t - base;
  });
  const deltaPct: (number | null)[] = total.map((t, i) => {
    const base = i === 0 ? decPrev : total[i - 1];
    if (t === null || base === null) return null;
    return base > 0 ? (100 * (t - base)) / base : 0;
  });

  // Allocation pie for the active month: the backend already picks which
  // slices qualify (only `> 0`, with "Credits"/"Debts" split by sign), so
  // this only needs to convert each slice's value. If any slice's month
  // could not be converted, the pie is not rendered at all (see below)
  // rather than drawn with a wrong proportion for that slice, so the `?? 0`
  // fallback in `pieData` is never actually shown.
  const pieValues = (allocation?.slices ?? []).map((s) => ({
    name: s.name,
    value: convert(s.value, month),
  }));
  const pieUnresolved = pieValues.some((s) => s.value === null);
  const pieData = pieValues.map((s) => ({ name: s.name, value: s.value ?? 0 }));

  const evoLabels = evolution?.months ?? MONTHS_SHORT;
  const evoData = months.map((m, i) => ({
    month: evoLabels[i],
    Investments: invTotal[i],
    Liquidity: liqTotal[i],
    "Credits/Debts": cdTotal[i],
    Total: total[i],
  }));

  // Source currencies (an investment, liquidity, or credit/debt row's own
  // currency) the backend could not resolve into the reference currency,
  // and therefore dropped from every component series, total, and slice.
  // `evolution` and `allocation` are fetched together and normally report
  // the same set, but a currency could in principle appear in one and not
  // the other (e.g. only used by a row outside the selected month), so
  // both are combined into one message.
  const missingSourceCurrencies = Array.from(
    new Set([
      ...(evolution?.unavailable_currencies ?? []),
      ...(allocation?.unavailable_currencies ?? []),
    ]),
  );

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-sm text-muted-foreground">
          Figures below are shown in {shownCurrency}
          {shownCurrency !== refCurrency ? ", each month at that month's own rate" : ""}.
        </p>
        <div className="flex flex-wrap items-center gap-4">
          <label
            className="flex items-center gap-2 text-xs text-muted-foreground"
            title="Only re-converts the figures below for viewing here. It does not change the reference currency the app's totals are computed in."
          >
            Display currency
            <select
              value={displayCurrency}
              onChange={(e) => setDisplayCurrencyChoice(e.target.value as Currency)}
              className="rounded-md border border-border bg-surface/60 px-2 py-1 text-sm text-foreground"
            >
              {CURRENCIES.map((c) => (
                <option key={c} value={c}>
                  {c}
                </option>
              ))}
            </select>
          </label>
          <label
            className="flex items-center gap-2 text-xs text-muted-foreground"
            title="The currency every total on this page is computed in. Changing it re-expresses every figure at read time and rewrites nothing on disk."
          >
            Reference currency
            <select
              value={refCurrency}
              onChange={(e) => updateReferenceCurrency(e.target.value as Currency)}
              className="rounded-md border border-border bg-surface/60 px-2 py-1 text-sm text-foreground"
            >
              {CURRENCIES.map((c) => (
                <option key={c} value={c}>
                  {c}
                </option>
              ))}
            </select>
          </label>
          <label
            className="flex items-center gap-2 text-xs text-muted-foreground"
            title="Only changes how the in-progress month is priced. Completed months stay frozen at their own month-end rate."
          >
            Current-month rate
            <select
              value={currencySettings.current_month_rate_mode}
              onChange={(e) => updateRateMode(e.target.value as CurrentMonthRateMode)}
              className="rounded-md border border-border bg-surface/60 px-2 py-1 text-sm text-foreground"
            >
              <option value="previous_month_end">Frozen (prior month-end)</option>
              <option value="live">Live</option>
            </select>
          </label>
        </div>
      </div>

      {displayUnavailable && (
        <ErrorBanner
          message={
            displayRatesError || prevDisplayRatesError
              ? `Could not load exchange rates for ${displayCurrency}: ${displayRatesError ?? prevDisplayRatesError}. Showing ${refCurrency} instead.`
              : `${displayCurrency} has no available exchange rate right now, showing ${refCurrency} instead.`
          }
        />
      )}

      {missingSourceCurrencies.length > 0 && (
        <ErrorBanner
          message={`Could not resolve exchange rates for ${missingSourceCurrencies.join(", ")}. Figures below exclude ${missingSourceCurrencies.length > 1 ? "those currencies" : "that currency"}.`}
        />
      )}

      {prevDecError && (
        <ErrorBanner
          message={`Could not load last year's December net worth: ${prevDecError}. January's change is unresolved.`}
        />
      )}

      {evolutionError ? (
        <GlassCard title={`Net Worth · ${year}`}>
          <ErrorBanner message={`Could not load net worth data: ${evolutionError}`} />
        </GlassCard>
      ) : evolution === null ? (
        <GlassCard title={`Net Worth · ${year}`}>
          <p className="px-3 py-8 text-center text-muted-foreground">
            No net worth data for {year} yet.
          </p>
        </GlassCard>
      ) : ratesLoading ? (
        <GlassCard title={`Net Worth · ${year}`}>
          <p className="px-3 py-8 text-center text-muted-foreground">
            Loading {displayCurrency} exchange rates…
          </p>
        </GlassCard>
      ) : (
        <>
          <GlassCard title={`Net Worth grid · ${year} (${shownCurrency})`}>
            <div className="scrollbar-thin overflow-x-auto">
              <table className="w-full min-w-[1100px] text-sm">
                <thead>
                  <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
                    <th className="px-3 py-2 font-medium">Component</th>
                    {evoLabels.map((m) => (
                      <th key={m} className="px-2 py-2 text-right font-medium">
                        {m}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody className="divide-y divide-border/40">
                  {INV_CATS.map((c, idx) => (
                    <tr key={c} className="hover:bg-muted/20">
                      <td className="px-3 py-1.5">
                        <span
                          className="mr-2 inline-block h-2 w-2 rounded-full"
                          style={{ background: colorAt(idx) }}
                        />
                        {c}
                      </td>
                      {invByCat[c].map((v, i) => (
                        <td key={i} className="px-2 py-1.5 text-right text-xs tabular-nums">
                          {fmtOrDash(v, shownCurrency)}
                        </td>
                      ))}
                    </tr>
                  ))}
                  <tr className="hover:bg-muted/20">
                    <td className="px-3 py-1.5">Liquidity total</td>
                    {liqTotal.map((v, i) => (
                      <td key={i} className="px-2 py-1.5 text-right text-xs tabular-nums">
                        {fmtOrDash(v, shownCurrency)}
                      </td>
                    ))}
                  </tr>
                  <tr className="hover:bg-muted/20">
                    <td className="px-3 py-1.5">Credits & Debts</td>
                    {cdTotal.map((v, i) => (
                      <td
                        key={i}
                        className={`px-2 py-1.5 text-right text-xs tabular-nums ${v !== null && v < 0 ? "text-destructive" : ""}`}
                      >
                        {fmtOrDash(v, shownCurrency)}
                      </td>
                    ))}
                  </tr>
                  <tr className="bg-muted/30 font-semibold">
                    <td className="px-3 py-2 text-gradient">Total Net Worth</td>
                    {total.map((v, i) => (
                      <td key={i} className="px-2 py-2 text-right tabular-nums">
                        {fmtOrDash(v, shownCurrency)}
                      </td>
                    ))}
                  </tr>
                  <tr className="font-semibold">
                    <td className="px-3 py-1.5">Monthly Change</td>
                    {delta.map((v, i) => (
                      <td
                        key={i}
                        className={`px-2 py-1.5 text-right tabular-nums ${v === null ? "" : v >= 0 ? "text-success" : "text-destructive"}`}
                      >
                        {v === null
                          ? fmtOrDash(null, shownCurrency)
                          : (v >= 0 ? "+" : "") + formatRef(v, shownCurrency)}
                      </td>
                    ))}
                  </tr>
                  <tr className="font-semibold">
                    <td className="px-3 py-1.5">% Change</td>
                    {deltaPct.map((v, i) => (
                      <td
                        key={i}
                        className={`px-2 py-1.5 text-right tabular-nums ${v === null ? "" : v >= 0 ? "text-success" : "text-destructive"}`}
                      >
                        {v === null
                          ? fmtOrDash(null, shownCurrency)
                          : `${v >= 0 ? "+" : ""}${v.toFixed(1)}%`}
                      </td>
                    ))}
                  </tr>
                </tbody>
              </table>
            </div>
          </GlassCard>

          <div className="grid gap-5 lg:grid-cols-[1fr_1.4fr]">
            <GlassCard title={`Allocation · ${MONTHS[month - 1]} ${year} (${shownCurrency})`}>
              {allocationError ? (
                <ErrorBanner message={`Could not load allocation: ${allocationError}`} />
              ) : pieUnresolved ? (
                <p className="px-3 py-8 text-center text-sm text-warning">
                  No exchange rate available to show this chart in {shownCurrency}.
                </p>
              ) : (
                <div className="h-80">
                  <ResponsiveContainer>
                    <PieChart>
                      <Pie
                        data={pieData}
                        dataKey="value"
                        nameKey="name"
                        innerRadius={60}
                        outerRadius={110}
                        paddingAngle={2}
                      >
                        {pieData.map((_, i) => (
                          <Cell key={i} fill={colorAt(i)} stroke="oklch(0.16 0.02 265)" />
                        ))}
                      </Pie>
                      <Tooltip content={<DarkTooltip currency={shownCurrency} />} />
                      <Legend verticalAlign="bottom" wrapperStyle={LEGEND_STYLE} />
                    </PieChart>
                  </ResponsiveContainer>
                </div>
              )}
            </GlassCard>

            <GlassCard title={`Evolution · ${year} (${shownCurrency})`}>
              <div className="h-80">
                <ResponsiveContainer>
                  <ComposedChart data={evoData}>
                    <defs>
                      <linearGradient id="g-inv" x1="0" y1="0" x2="0" y2="1">
                        <stop offset="0%" stopColor={colorAt(0)} stopOpacity={0.6} />
                        <stop offset="100%" stopColor={colorAt(0)} stopOpacity={0.05} />
                      </linearGradient>
                      <linearGradient id="g-liq" x1="0" y1="0" x2="0" y2="1">
                        <stop offset="0%" stopColor={colorAt(1)} stopOpacity={0.6} />
                        <stop offset="100%" stopColor={colorAt(1)} stopOpacity={0.05} />
                      </linearGradient>
                      <linearGradient id="g-cd" x1="0" y1="0" x2="0" y2="1">
                        <stop offset="0%" stopColor={colorAt(4)} stopOpacity={0.6} />
                        <stop offset="100%" stopColor={colorAt(4)} stopOpacity={0.05} />
                      </linearGradient>
                    </defs>
                    <CartesianGrid strokeDasharray="3 3" stroke="oklch(1 0 0 / 6%)" />
                    <XAxis dataKey="month" tick={{ fontSize: 18, fill: tickColor }} />
                    <YAxis tick={{ fontSize: 18, fill: tickColor }} />
                    <Tooltip content={<DarkTooltip total currency={shownCurrency} />} />
                    <Legend wrapperStyle={LEGEND_STYLE} />
                    <Area
                      type="monotone"
                      dataKey="Investments"
                      stackId="1"
                      stroke={colorAt(0)}
                      fill="url(#g-inv)"
                    />
                    <Area
                      type="monotone"
                      dataKey="Liquidity"
                      stackId="1"
                      stroke={colorAt(1)}
                      fill="url(#g-liq)"
                    />
                    <Area
                      type="monotone"
                      dataKey="Credits/Debts"
                      stackId="1"
                      stroke={colorAt(4)}
                      fill="url(#g-cd)"
                    />
                    <Line
                      type="monotone"
                      dataKey="Total"
                      stroke={totalStroke}
                      strokeWidth={3}
                      dot={{ r: 4 }}
                      activeDot={{ r: 6 }}
                    />
                  </ComposedChart>
                </ResponsiveContainer>
              </div>
            </GlassCard>
          </div>
        </>
      )}
    </div>
  );
}
