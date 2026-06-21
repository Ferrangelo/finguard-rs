import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useMemo, useState } from "react";
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
import { CURRENCIES, formatRef, toRef } from "@/services/fx";
import { GlassCard } from "@/components/finguard/GlassCard";
import { SubTabs } from "@/components/finguard/SubTabs";
import { MathInput } from "@/components/finguard/MathInput";
import { ConfirmButton } from "@/components/finguard/ConfirmButton";
import { DarkTooltip, useChartColors, LEGEND_STYLE } from "@/components/finguard/DarkTooltip";
import type {
  CreditDebtRow,
  Currency,
  InvestmentAsset,
  InvestmentCategory,
  LiquidityRow,
} from "@/services/types";

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
      {sub === "investments" && <InvestmentsTab />}
      {sub === "liquidity" && <LiquidityTab />}
      {sub === "total" && <TotalTab />}
    </div>
  );
}

const INV_CATS: InvestmentCategory[] = ["Stocks/ETF", "Commodities", "Bonds"];
const LIQ_CATS = ["Bank/Broker account", "Cash", "Other"] as const;

// ────────────────────────────────────────────────────────────── Investments
function InvestmentsTab() {
  const colorAt = useChartColors();
  const { year, notify, refresh, refreshTick } = useApp();
  const [assets, setAssets] = useState<InvestmentAsset[]>([]);
  const [view, setView] = useState<"holdings" | "prices" | "value">("value");
  const [adding, setAdding] = useState(false);
  const [editId, setEditId] = useState<string | null>(null);

  useEffect(() => {
    api.ensureYear(year).then(() => api.getInvestments(year).then(setAssets));
  }, [year, refreshTick]);

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
    await api.setInvestmentCell(id, year, m, field, v);
    notify("success", "Saved");
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
          onCancel={() => setAdding(false)}
          onCreate={async (name, cat, link) => {
            await api.addInvestment(name, cat, link, year);
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
                  <td colSpan={16} className="px-3 py-8 text-center text-muted-foreground">
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
  onCreate,
  onCancel,
}: {
  onCreate: (name: string, cat: InvestmentCategory, link?: string) => Promise<void>;
  onCancel: () => void;
}) {
  const [name, setName] = useState("");
  const [cat, setCat] = useState<InvestmentCategory>("Stocks/ETF");
  const [link, setLink] = useState("");
  return (
    <GlassCard title="New investment asset">
      <div className="grid gap-3 md:grid-cols-[2fr_1fr_2fr_auto]">
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
        <input
          placeholder="Link (optional)"
          value={link}
          onChange={(e) => setLink(e.target.value)}
          className="rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
        />
        <div className="flex gap-2">
          <button
            onClick={() => name.trim() && onCreate(name.trim(), cat, link || undefined)}
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
function LiquidityTab() {
  const { year, notify, refresh, refreshTick } = useApp();
  const [liq, setLiq] = useState<LiquidityRow[]>([]);
  const [cd, setCd] = useState<CreditDebtRow[]>([]);

  useEffect(() => {
    api.ensureYear(year).then(() => {
      api.getLiquidity(year).then(setLiq);
      api.getCreditsDebts(year).then(setCd);
    });
  }, [year, refreshTick]);

  const setLiqCell = async (id: string, m: number, v: number) => {
    setLiq((prev) =>
      prev.map((r) =>
        r.id !== id
          ? r
          : { ...r, data: { ...r.data, [year]: { ...(r.data[year] ?? {}), [m]: v } } },
      ),
    );
    await api.setLiquidityCell(id, year, m, v);
    notify("success", "Saved");
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
    await api.setCreditDebtCell(id, year, m, v);
    notify("success", "Saved");
    refresh();
  };

  return (
    <div className="space-y-5">
      <LiquiditySection
        title="Liquidity"
        rows={liq}
        year={year}
        valueFor={(r, m) => toRef(r.data[year]?.[m] ?? 0, r.currency)}
        onCellCommit={setLiqCell}
        renderAddForm={() => (
          <AddLiquidityForm
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
        valueFor={(r, m) => toRef(r.data[year]?.[m] ?? 0, r.currency)}
        onCellCommit={setCdCell}
        renderAddForm={() => (
          <AddCreditDebtForm
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

function LiquiditySection<R extends BaseRow>({
  title,
  rows,
  year,
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
              <td className="px-3 py-2">Total (€)</td>
              {metaHeaders.map((h) => (
                <td key={h} />
              ))}
              {totals.map((t, i) => (
                <td
                  key={i}
                  className={`px-2 py-2 text-right tabular-nums ${signed && t < 0 ? "text-destructive" : ""}`}
                >
                  {formatRef(t)}
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
  onCreate,
}: {
  onCreate: (n: string, c: LiquidityRow["category"], cur: Currency) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [cat, setCat] = useState<LiquidityRow["category"]>("Bank/Broker account");
  const [cur, setCur] = useState<Currency>("EUR");
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
  onCreate,
}: {
  onCreate: (n: string, cur: Currency) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [cur, setCur] = useState<Currency>("EUR");
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
function TotalTab() {
  const colorAt = useChartColors();
  const { year, month, refreshTick } = useApp();
  const [assets, setAssets] = useState<InvestmentAsset[]>([]);
  const [liq, setLiq] = useState<LiquidityRow[]>([]);
  const [cd, setCd] = useState<CreditDebtRow[]>([]);

  useEffect(() => {
    api.ensureYear(year).then(() => {
      api.getInvestments(year).then(setAssets);
      api.getLiquidity(year).then(setLiq);
      api.getCreditsDebts(year).then(setCd);
    });
  }, [year, refreshTick]);

  const months = Array.from({ length: 12 }, (_, i) => i + 1);

  const invByCatMonthly = useMemo(() => {
    const out: Record<string, number[]> = {};
    for (const c of INV_CATS) out[c] = Array(12).fill(0);
    for (const a of assets) {
      for (const m of months) {
        const cell = a.data[year]?.[m] ?? { qty: 0, price: 0 };
        out[a.category][m - 1] += cell.qty * cell.price;
      }
    }
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [assets, year]);

  const liqTotal = months.map((m) =>
    liq.reduce((s, r) => s + toRef(r.data[year]?.[m] ?? 0, r.currency), 0),
  );
  const cdTotal = months.map((m) =>
    cd.reduce((s, r) => s + toRef(r.data[year]?.[m] ?? 0, r.currency), 0),
  );
  const invTotal = months.map((_, i) => INV_CATS.reduce((s, c) => s + invByCatMonthly[c][i], 0));
  const total = months.map((_, i) => invTotal[i] + liqTotal[i] + cdTotal[i]);

  // Previous year december fallback for January delta
  const prevYear = year - 1;
  const decPrev = useMemo(() => {
    const inv = INV_CATS.reduce((s, c) => {
      return (
        s +
        assets.reduce((ss, a) => {
          if (a.category !== c) return ss;
          const cell = a.data[prevYear]?.[12] ?? { qty: 0, price: 0 };
          return ss + cell.qty * cell.price;
        }, 0)
      );
    }, 0);
    const l = liq.reduce((s, r) => s + toRef(r.data[prevYear]?.[12] ?? 0, r.currency), 0);
    const c = cd.reduce((s, r) => s + toRef(r.data[prevYear]?.[12] ?? 0, r.currency), 0);
    return inv + l + c;
  }, [assets, liq, cd, prevYear]);

  const delta = total.map((t, i) => t - (i === 0 ? decPrev : total[i - 1]));
  const deltaPct = total.map((t, i) => {
    const base = i === 0 ? decPrev : total[i - 1];
    return base > 0 ? (100 * (t - base)) / base : 0;
  });

  // Allocation pie for active month
  const pieData = useMemo(() => {
    const out: { name: string; value: number }[] = [];
    for (const c of INV_CATS) {
      const v = invByCatMonthly[c][month - 1];
      if (v) out.push({ name: c, value: v });
    }
    if (liqTotal[month - 1]) out.push({ name: "Liquidity", value: liqTotal[month - 1] });
    if (cdTotal[month - 1] !== 0)
      out.push({ name: "Credits/Debts", value: Math.abs(cdTotal[month - 1]) });
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [invByCatMonthly, liqTotal, cdTotal, month]);

  // Evolution data
  const evoData = months.map((m, i) => ({
    month: MONTHS_SHORT[m - 1],
    Investments: invTotal[i],
    Liquidity: liqTotal[i],
    "Credits/Debts": cdTotal[i],
    Total: total[i],
  }));

  return (
    <div className="space-y-5">
      <GlassCard title={`Net Worth grid · ${year}`}>
        <div className="scrollbar-thin overflow-x-auto">
          <table className="w-full min-w-[1100px] text-sm">
            <thead>
              <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
                <th className="px-3 py-2 font-medium">Component</th>
                {MONTHS_SHORT.map((m) => (
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
                  {invByCatMonthly[c].map((v, i) => (
                    <td key={i} className="px-2 py-1.5 text-right text-xs tabular-nums">
                      {formatRef(v)}
                    </td>
                  ))}
                </tr>
              ))}
              <tr className="hover:bg-muted/20">
                <td className="px-3 py-1.5">Liquidity total</td>
                {liqTotal.map((v, i) => (
                  <td key={i} className="px-2 py-1.5 text-right text-xs tabular-nums">
                    {formatRef(v)}
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
                    {formatRef(v)}
                  </td>
                ))}
              </tr>
              <tr className="bg-muted/30 font-semibold">
                <td className="px-3 py-2 text-gradient">Total Net Worth</td>
                {total.map((v, i) => (
                  <td key={i} className="px-2 py-2 text-right tabular-nums">
                    {formatRef(v)}
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
                    {formatRef(v)}
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
        <GlassCard title={`Allocation · ${MONTHS[month - 1]} ${year}`}>
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
                <Tooltip content={<DarkTooltip />} />
                <Legend verticalAlign="bottom" wrapperStyle={LEGEND_STYLE} />
              </PieChart>
            </ResponsiveContainer>
          </div>
        </GlassCard>

        <GlassCard title={`Evolution · ${year}`}>
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
                <XAxis dataKey="month" tick={{ fontSize: 11, fill: "oklch(0.68 0.02 260)" }} />
                <YAxis tick={{ fontSize: 11, fill: "oklch(0.68 0.02 260)" }} />
                <Tooltip content={<DarkTooltip total />} />
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
                  stroke="oklch(0.95 0.02 260)"
                  strokeWidth={3}
                  dot={{ r: 4 }}
                  activeDot={{ r: 6 }}
                />
              </ComposedChart>
            </ResponsiveContainer>
          </div>
        </GlassCard>
      </div>
    </div>
  );
}
