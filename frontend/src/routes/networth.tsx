import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";
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

  useEffect(() => {
    api.ensureYear(year).then(() => api.getInvestments(year).then(setAssets));
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
              {assets.length === 0 && (
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

  useEffect(() => {
    api.ensureYear(year).then(() => {
      api.getLiquidity(year).then(setLiq);
      api.getCreditsDebts(year).then(setCd);
      api.getMonthlyFxRates(year).then(setRates);
    });
  }, [year, refreshTick]);

  // Converts a row's own balance into the reference currency at month `m`'s
  // own rate. Returns 0, rather than a wrong figure, when the row's
  // currency has no resolved rate for that month.
  const toRefAmount = (amount: number, currency: Currency, m: number): number => {
    if (currency === refCurrency) return amount;
    const rate = rates?.months.find((r) => r.month === m)?.rate_to_reference[currency];
    return rate ? amount * rate : 0;
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
  valueFor: (r: R, m: number) => number;
  onCellCommit: (id: string, m: number, v: number) => void;
  renderAddForm: () => React.ReactNode;
  renderMeta: (r: R) => React.ReactNode;
  metaHeaders: string[];
  renderActions: (r: R) => React.ReactNode;
  renderEditName: (r: R) => React.ReactNode;
  signed?: boolean;
}) {
  const months = Array.from({ length: 12 }, (_, i) => i + 1);
  const totals = months.map((m) => rows.reduce((s, r) => s + valueFor(r, m), 0));

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
                <td
                  key={i}
                  className={`px-2 py-2 text-right tabular-nums ${signed && t < 0 ? "text-destructive" : ""}`}
                >
                  {formatRef(t, refCurrency)}
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
  // the backend found nothing to report (every value zero) for that
  // year/month, not that the request is still in flight; see the file-level
  // comment for why this file skips a separate loading state, matching the
  // rest of this page.
  const [evolution, setEvolution] = useState<NetworthEvolution | null>(null);
  const [allocation, setAllocation] = useState<NetworthAllocation | null>(null);

  useEffect(() => {
    api.ensureYear(year).then(() => {
      api.getNetworthEvolution(year).then(setEvolution);
      api.getNetworthAllocation(year, month).then(setAllocation);
    });
  }, [year, month, refreshTick]);

  // December of the prior year's net worth (reference currency), the
  // baseline for January's month-over-month delta. `getNetworthEvolution`
  // only ever returns `year`'s own 12 months, so the prior December figure
  // needs its own request against `year - 1`. A `null` response there means
  // no prior-year data at all, which this treats as a baseline of 0, the
  // same value the row cells themselves default to when unset.
  const [prevDecNetWorth, setPrevDecNetWorth] = useState(0);
  useEffect(() => {
    api.getNetworthEvolution(year - 1).then((e) => setPrevDecNetWorth(e ? e.net_worth[11] : 0));
  }, [year, refreshTick]);

  // Display currency: defaults to the reference currency until the user
  // explicitly picks another one.
  const [displayCurrencyChoice, setDisplayCurrencyChoice] = useState<Currency | null>(null);
  const displayCurrency = displayCurrencyChoice ?? refCurrency;

  // Rates for the display currency, one per calendar month of `year` plus
  // December of `year - 1` for the January baseline. Fetched only when the
  // display currency differs from the reference currency, since the
  // reference currency needs no conversion at all.
  const [displayRates, setDisplayRates] = useState<MonthlyFxRates | null>(null);
  const [prevDisplayRates, setPrevDisplayRates] = useState<MonthlyFxRates | null>(null);
  useEffect(() => {
    if (displayCurrency === refCurrency) {
      setDisplayRates(null);
      setPrevDisplayRates(null);
      return;
    }
    api.getMonthlyFxRates(year, [displayCurrency]).then(setDisplayRates);
    api.getMonthlyFxRates(year - 1, [displayCurrency]).then(setPrevDisplayRates);
  }, [year, displayCurrency, refCurrency, refreshTick]);

  const displayUnavailable =
    displayRates?.unavailable_currencies.includes(displayCurrency) ?? false;
  // Falls back to the reference currency, with a warning banner below,
  // rather than rendering a blank or wrong figure when the chosen display
  // currency has no resolved rate at all.
  const shownCurrency = displayUnavailable ? refCurrency : displayCurrency;

  // Converts a reference-currency amount into `shownCurrency` using *that
  // calendar month's own* rate, not one current rate: the user chose this
  // deliberately, since a single rate would hide currency movement inside
  // the portfolio curve. `referenceToDisplay` returns `null` for a missing
  // rate, which this reports as 0 rather than propagating `NaN`.
  const convert = (amountInReference: number, m: number): number => {
    if (shownCurrency === refCurrency) return amountInReference;
    const rate = displayRates?.months.find((r) => r.month === m)?.rate_to_reference[shownCurrency];
    return referenceToDisplay(amountInReference, rate) ?? 0;
  };
  const convertPrevDec = (amountInReference: number): number => {
    if (shownCurrency === refCurrency) return amountInReference;
    const rate = prevDisplayRates?.months.find((r) => r.month === 12)?.rate_to_reference[
      shownCurrency
    ];
    return referenceToDisplay(amountInReference, rate) ?? 0;
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

  const months = Array.from({ length: 12 }, (_, i) => i + 1);
  const zeros = () => Array(12).fill(0);

  const invByCat: Record<InvestmentCategory, number[]> = {
    "Stocks/ETF": zeros(),
    Commodities: zeros(),
    Bonds: zeros(),
  };
  let liqTotal = zeros();
  let cdTotal = zeros();
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
  const invTotal = months.map((_, i) => INV_CATS.reduce((s, c) => s + invByCat[c][i], 0));
  const total = evolution ? evolution.net_worth.map((v, i) => convert(v, i + 1)) : zeros();

  // Each month's delta is against the prior month's total, except January,
  // which is measured against decPrev (December of the prior year,
  // converted at that same December's own rate).
  const decPrev = convertPrevDec(prevDecNetWorth);
  const delta = total.map((t, i) => t - (i === 0 ? decPrev : total[i - 1]));
  const deltaPct = total.map((t, i) => {
    const base = i === 0 ? decPrev : total[i - 1];
    return base > 0 ? (100 * (t - base)) / base : 0;
  });

  // Allocation pie for the active month: the backend already picks which
  // slices qualify (only `> 0`, with "Credits"/"Debts" split by sign), so
  // this only needs to convert each slice's value.
  const pieData = (allocation?.slices ?? []).map((s) => ({
    name: s.name,
    value: convert(s.value, month),
  }));

  const evoLabels = evolution?.months ?? MONTHS_SHORT;
  const evoData = months.map((m, i) => ({
    month: evoLabels[i],
    Investments: invTotal[i],
    Liquidity: liqTotal[i],
    "Credits/Debts": cdTotal[i],
    Total: total[i],
  }));

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-sm text-muted-foreground">
          Figures below are shown in {shownCurrency}
          {shownCurrency !== refCurrency ? ", each month at that month's own rate" : ""}.
        </p>
        <div className="flex flex-wrap items-center gap-4">
          <label className="flex items-center gap-2 text-xs text-muted-foreground">
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
        <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
          {displayCurrency} has no available exchange rate right now, showing {refCurrency} instead.
        </div>
      )}

      {evolution === null ? (
        <GlassCard title={`Net Worth · ${year}`}>
          <p className="px-3 py-8 text-center text-muted-foreground">
            No net worth data for {year} yet.
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
                          {formatRef(v, shownCurrency)}
                        </td>
                      ))}
                    </tr>
                  ))}
                  <tr className="hover:bg-muted/20">
                    <td className="px-3 py-1.5">Liquidity total</td>
                    {liqTotal.map((v, i) => (
                      <td key={i} className="px-2 py-1.5 text-right text-xs tabular-nums">
                        {formatRef(v, shownCurrency)}
                      </td>
                    ))}
                  </tr>
                  <tr className="hover:bg-muted/20">
                    <td className="px-3 py-1.5">Credits & Debts</td>
                    {cdTotal.map((v, i) => (
                      <td
                        key={i}
                        className={`px-2 py-1.5 text-right text-xs tabular-nums ${v < 0 ? "text-destructive" : ""}`}
                      >
                        {formatRef(v, shownCurrency)}
                      </td>
                    ))}
                  </tr>
                  <tr className="bg-muted/30 font-semibold">
                    <td className="px-3 py-2 text-gradient">Total Net Worth</td>
                    {total.map((v, i) => (
                      <td key={i} className="px-2 py-2 text-right tabular-nums">
                        {formatRef(v, shownCurrency)}
                      </td>
                    ))}
                  </tr>
                  <tr className="font-semibold">
                    <td className="px-3 py-1.5">Monthly Change</td>
                    {delta.map((v, i) => (
                      <td
                        key={i}
                        className={`px-2 py-1.5 text-right tabular-nums ${v >= 0 ? "text-success" : "text-destructive"}`}
                      >
                        {v >= 0 ? "+" : ""}
                        {formatRef(v, shownCurrency)}
                      </td>
                    ))}
                  </tr>
                  <tr className="font-semibold">
                    <td className="px-3 py-1.5">% Change</td>
                    {deltaPct.map((v, i) => (
                      <td
                        key={i}
                        className={`px-2 py-1.5 text-right tabular-nums ${v >= 0 ? "text-success" : "text-destructive"}`}
                      >
                        {v >= 0 ? "+" : ""}
                        {v.toFixed(1)}%
                      </td>
                    ))}
                  </tr>
                </tbody>
              </table>
            </div>
          </GlassCard>

          <div className="grid gap-5 lg:grid-cols-[1fr_1.4fr]">
            <GlassCard title={`Allocation · ${MONTHS[month - 1]} ${year} (${shownCurrency})`}>
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
