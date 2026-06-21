import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useMemo, useState } from "react";
import {
  BarChart, Bar, CartesianGrid, Cell, Legend, Line, LineChart,
  Pie, PieChart, ResponsiveContainer, Tooltip, XAxis, YAxis,
} from "recharts";
import { Plus, Play, Pencil, X } from "lucide-react";
import { useApp } from "@/context/AppContext";
import * as api from "@/services/api";
import { MONTHS, MONTHS_SHORT } from "@/services/api";
import { CURRENCIES, formatRef, toRef } from "@/services/fx";
import { evalMath } from "@/services/mathEval";
import { GlassCard } from "@/components/finguard/GlassCard";
import { SubTabs } from "@/components/finguard/SubTabs";
import { Combobox } from "@/components/finguard/Combobox";
import { ConfirmButton } from "@/components/finguard/ConfirmButton";
import { DarkTooltip, colorAt } from "@/components/finguard/DarkTooltip";
import type {
  Categories, Currency, Expense, MappingRule, RecurringTemplate,
} from "@/services/types";

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
          <p className="text-sm text-muted-foreground">Track, categorize and analyze every euro that leaves.</p>
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
  const { year, month, notify, refresh, refreshTick } = useApp();
  const [rows, setRows] = useState<Expense[]>([]);
  const [cats, setCats] = useState<Categories>({ primary: [], secondary: [] });
  const [mappings, setMappings] = useState<MappingRule[]>([]);
  const [filter, setFilter] = useState<{ name: string; category: string; min: string; max: string }>({
    name: "", category: "", min: "", max: "",
  });
  const [editing, setEditing] = useState<Expense | null>(null);
  const [adding, setAdding] = useState(false);

  useEffect(() => {
    api.getCategories().then(setCats);
    api.getMappings().then(setMappings);
  }, [refreshTick]);

  useEffect(() => {
    api.getExpenses(year, month, {
      name: filter.name || undefined,
      category: filter.category || undefined,
      min: filter.min ? Number(filter.min) : undefined,
      max: filter.max ? Number(filter.max) : undefined,
    }).then(setRows);
  }, [year, month, filter, refreshTick]);

  const totalRef = useMemo(() => rows.reduce((s, e) => s + toRef(e.amount, e.currency), 0), [rows]);

  return (
    <div className="grid gap-5 lg:grid-cols-[1fr_360px]">
      <GlassCard
        title={`${MONTHS[month - 1]} ${year} · ${rows.length} entries`}
        action={
          <div className="flex items-center gap-2 text-sm">
            <span className="text-muted-foreground">Total</span>
            <span className="font-semibold text-gradient">{formatRef(totalRef)}</span>
          </div>
        }
      >
        <div className="mb-3 grid grid-cols-2 gap-2 md:grid-cols-4">
          <input placeholder="Filter name…" value={filter.name}
            onChange={(e) => setFilter((f) => ({ ...f, name: e.target.value }))}
            className="rounded-md border border-border bg-surface/50 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none" />
          <input placeholder="Filter category…" value={filter.category}
            onChange={(e) => setFilter((f) => ({ ...f, category: e.target.value }))}
            className="rounded-md border border-border bg-surface/50 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none" />
          <input placeholder="Min €" value={filter.min} inputMode="decimal"
            onChange={(e) => setFilter((f) => ({ ...f, min: e.target.value }))}
            className="rounded-md border border-border bg-surface/50 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none" />
          <input placeholder="Max €" value={filter.max} inputMode="decimal"
            onChange={(e) => setFilter((f) => ({ ...f, max: e.target.value }))}
            className="rounded-md border border-border bg-surface/50 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none" />
        </div>

        <div className="scrollbar-thin overflow-x-auto">
          <table className="w-full min-w-[760px] text-sm">
            <thead>
              <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
                <th className="px-3 py-2 font-medium">Date</th>
                <th className="px-3 py-2 font-medium">Name</th>
                <th className="px-3 py-2 text-right font-medium">Amount</th>
                <th className="px-3 py-2 font-medium">Curr</th>
                <th className="px-3 py-2 text-right font-medium">Ref €</th>
                <th className="px-3 py-2 font-medium">Primary</th>
                <th className="px-3 py-2 font-medium">Secondary</th>
                <th className="px-3 py-2 text-right font-medium"></th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border/40">
              {rows.length === 0 && (
                <tr><td colSpan={8} className="px-3 py-8 text-center text-muted-foreground">No expenses match.</td></tr>
              )}
              {rows.map((e) => {
                const dateStr = `${e.year}-${String(e.month).padStart(2, "0")}-${String(e.day).padStart(2, "0")}`;
                return (
                  <tr key={e.id} className="transition-colors hover:bg-muted/30">
                    <td className="px-3 py-2 font-mono text-xs text-muted-foreground">{dateStr}</td>
                    <td className="px-3 py-2 font-medium">{e.name}</td>
                    <td className="px-3 py-2 text-right tabular-nums">{e.amount.toFixed(2)}</td>
                    <td className="px-3 py-2 text-xs text-muted-foreground">{e.currency}</td>
                    <td className="px-3 py-2 text-right tabular-nums">{formatRef(toRef(e.amount, e.currency))}</td>
                    <td className="px-3 py-2"><CategoryChip name={e.primary} /></td>
                    <td className="px-3 py-2"><CategoryChip name={e.secondary} variant="muted" /></td>
                    <td className="px-3 py-2">
                      <div className="flex justify-end gap-1">
                        <button onClick={() => { setEditing(e); setAdding(false); }}
                          className="rounded-md border border-border p-1 text-muted-foreground transition-colors hover:border-primary/60 hover:text-primary">
                          <Pencil className="h-3.5 w-3.5" />
                        </button>
                        <ConfirmButton onConfirm={async () => {
                          await api.deleteExpense(e.id, e.year, e.month);
                          notify("success", `Deleted "${e.name}"`);
                          refresh();
                        }} />
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
          <button onClick={() => setAdding(true)}
            className="hover-lift inline-flex w-full items-center justify-center gap-2 rounded-xl bg-gradient-brand px-4 py-3 text-sm font-semibold text-background">
            <Plus className="h-4 w-4" /> Add expense
          </button>
        )}
        {(adding || editing) && (
          <ExpenseForm
            initial={editing ?? undefined}
            categories={cats}
            mappings={mappings}
            onCancel={() => { setEditing(null); setAdding(false); }}
            onSubmit={async (data) => {
              await api.upsertExpense(data);
              notify("success", editing ? `Updated "${data.name}"` : `Added "${data.name}"`);
              setEditing(null); setAdding(false);
              refresh();
            }}
          />
        )}
      </div>
    </div>
  );
}

function CategoryChip({ name, variant = "default" }: { name: string; variant?: "default" | "muted" }) {
  if (!name) return <span className="text-muted-foreground/60">—</span>;
  return (
    <span className={
      variant === "default"
        ? "inline-flex items-center rounded-md border border-primary/30 bg-primary/10 px-2 py-0.5 text-xs text-primary"
        : "inline-flex items-center rounded-md border border-border bg-muted/40 px-2 py-0.5 text-xs text-muted-foreground"
    }>{name}</span>
  );
}

function ExpenseForm({
  initial, categories, mappings, onSubmit, onCancel,
}: {
  initial?: Expense;
  categories: Categories;
  mappings: MappingRule[];
  onSubmit: (e: Omit<Expense, "id"> & { id?: string }) => Promise<void>;
  onCancel: () => void;
}) {
  const { year, month, notify } = useApp();
  const [name, setName] = useState(initial?.name ?? "");
  const [day, setDay] = useState<string>(String(initial?.day ?? new Date().getDate()));
  const [amount, setAmount] = useState<string>(initial ? String(initial.amount) : "");
  const [currency, setCurrency] = useState<Currency>(initial?.currency ?? "EUR");
  const [primary, setPrimary] = useState(initial?.primary ?? "");
  const [secondary, setSecondary] = useState(initial?.secondary ?? "");

  const onNameChange = (v: string) => {
    setName(v);
    if (!initial && v.length >= 2) {
      const m = api.lookupMapping(v, mappings);
      if (m) { setPrimary(m.primary); setSecondary(m.secondary); }
    }
  };

  const submit = async () => {
    const amt = evalMath(amount);
    const dayNum = Math.max(1, Math.min(31, Math.floor(Number(day) || 1)));
    if (!name.trim() || !Number.isFinite(amt)) {
      notify("error", "Name and a numeric amount are required");
      return;
    }
    await onSubmit({
      id: initial?.id,
      year, month, day: dayNum,
      name: name.trim(), amount: amt, currency,
      primary, secondary,
    });
    setName(""); setAmount(""); setPrimary(""); setSecondary("");
  };

  return (
    <GlassCard
      title={initial ? "Edit expense" : "Add expense"}
      action={<button onClick={onCancel} className="text-muted-foreground hover:text-foreground"><X className="h-4 w-4" /></button>}
    >
      <div className="space-y-3">
        <Field label="Name">
          <input value={name} onChange={(e) => onNameChange(e.target.value)}
            placeholder="Lidl, Rent…"
            className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none" />
        </Field>
        <div className="grid grid-cols-2 gap-3">
          <Field label="Day">
            <input type="number" min={1} max={31} value={day} onChange={(e) => setDay(e.target.value)}
              className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none" />
          </Field>
          <Field label="Amount (math ok)">
            <input value={amount} onChange={(e) => setAmount(e.target.value)} placeholder="10+5.5"
              className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm tabular-nums focus:border-primary/60 focus:outline-none" />
          </Field>
        </div>
        <Field label="Currency">
          <select value={currency} onChange={(e) => setCurrency(e.target.value as Currency)}
            className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none">
            {CURRENCIES.map((c) => <option key={c} value={c}>{c}</option>)}
          </select>
        </Field>
        <Field label="Primary category">
          <Combobox value={primary} onChange={setPrimary} options={categories.primary} placeholder="Groceries…" />
        </Field>
        <Field label="Secondary category">
          <Combobox value={secondary} onChange={setSecondary} options={categories.secondary} placeholder="Supermarket…" />
        </Field>
        <div className="flex gap-2 pt-2">
          <button onClick={submit}
            className="hover-lift inline-flex flex-1 items-center justify-center gap-2 rounded-md bg-gradient-brand px-3 py-2 text-sm font-semibold text-background">
            {initial ? "Save changes" : "Add expense"}
          </button>
          <button onClick={onCancel}
            className="rounded-md border border-border px-3 py-2 text-sm text-muted-foreground hover:text-foreground">
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
      <span className="mb-1 block text-[11px] font-medium uppercase tracking-wider text-muted-foreground">{label}</span>
      {children}
    </label>
  );
}

// ────────────────────────────────────────────────────────────── Summary
function SummaryTab() {
  const { year, month, refreshTick } = useApp();
  const [kind, setKind] = useState<"primary" | "secondary">("primary");
  const [yearExpenses, setYearExpenses] = useState<Expense[]>([]);
  const [selMonths, setSelMonths] = useState<number[]>([Math.max(1, month - 1), month]);
  const [selCats, setSelCats] = useState<string[]>([]);

  useEffect(() => {
    api.getExpenses(year).then(setYearExpenses);
  }, [year, refreshTick]);

  const catOf = (e: Expense) => (kind === "primary" ? e.primary : e.secondary) || "Uncategorized";

  const monthTotals = useMemo(() => {
    const map = new Map<string, number>();
    for (const e of yearExpenses) {
      if (e.month !== month) continue;
      map.set(catOf(e), (map.get(catOf(e)) ?? 0) + toRef(e.amount, e.currency));
    }
    return Array.from(map.entries()).map(([name, value]) => ({ name, value })).sort((a, b) => b.value - a.value);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [yearExpenses, month, kind]);

  const yearTable = useMemo(() => {
    const cats = new Set<string>();
    const grid: Record<string, number[]> = {};
    for (const e of yearExpenses) {
      const c = catOf(e); cats.add(c);
      if (!grid[c]) grid[c] = Array(12).fill(0);
      grid[c][e.month - 1] += toRef(e.amount, e.currency);
    }
    return Array.from(cats).sort().map((c) => ({ category: c, months: grid[c], total: grid[c].reduce((s, n) => s + n, 0) }));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [yearExpenses, kind]);

  const yearPie = useMemo(
    () => yearTable.map((r) => ({ name: r.category, value: r.total })).sort((a, b) => b.value - a.value),
    [yearTable],
  );

  const allCats = useMemo(() => yearTable.map((r) => r.category), [yearTable]);

  // default-select top 3 categories on first load
  useEffect(() => {
    if (selCats.length === 0 && allCats.length > 0) {
      setSelCats(yearPie.slice(0, 3).map((p) => p.name));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [allCats.join("|")]);

  const compareData = useMemo(() => {
    if (selMonths.length === 0) return [];
    const cats = new Set<string>();
    const buckets: Record<number, Record<string, number>> = {};
    for (const m of selMonths) buckets[m] = {};
    for (const e of yearExpenses) {
      if (!selMonths.includes(e.month)) continue;
      cats.add(catOf(e));
      buckets[e.month][catOf(e)] = (buckets[e.month][catOf(e)] ?? 0) + toRef(e.amount, e.currency);
    }
    return Array.from(cats).map((c) => {
      const r: Record<string, number | string> = { category: c };
      for (const m of selMonths) r[MONTHS_SHORT[m - 1]] = buckets[m][c] ?? 0;
      return r;
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [yearExpenses, selMonths, kind]);

  const trendData = useMemo(() => {
    return MONTHS_SHORT.map((m, i) => {
      const row: Record<string, number | string> = { month: m };
      for (const c of selCats) {
        const total = yearExpenses
          .filter((e) => e.month === i + 1 && catOf(e) === c)
          .reduce((s, e) => s + toRef(e.amount, e.currency), 0);
        row[c] = total;
      }
      return row;
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [yearExpenses, selCats, kind]);

  const toggleMonth = (m: number) =>
    setSelMonths((prev) => prev.includes(m) ? prev.filter((x) => x !== m) : prev.length >= 3 ? [...prev.slice(1), m] : [...prev, m]);
  const toggleCat = (c: string) =>
    setSelCats((prev) => prev.includes(c) ? prev.filter((x) => x !== c) : prev.length >= 3 ? [...prev.slice(1), c] : [...prev, c]);

  return (
    <div className="space-y-5">
      <div className="flex items-center gap-3">
        <span className="text-sm text-muted-foreground">Group by</span>
        <SubTabs value={kind} onChange={setKind} options={[
          { value: "primary", label: "Primary" },
          { value: "secondary", label: "Secondary" },
        ]} />
      </div>

      <div className="grid gap-5 lg:grid-cols-2">
        <GlassCard title={`${MONTHS[month - 1]} ${year} totals`}>
          <div className="grid gap-4 md:grid-cols-[1fr_220px]">
            <div className="scrollbar-thin max-h-72 overflow-y-auto">
              <table className="w-full text-sm">
                <tbody className="divide-y divide-border/40">
                  {monthTotals.length === 0 && (
                    <tr><td className="py-6 text-center text-muted-foreground">No data</td></tr>
                  )}
                  {monthTotals.map((r, i) => (
                    <tr key={r.name}>
                      <td className="px-2 py-1.5 font-medium">
                        <span className="mr-2 inline-block h-2 w-2 rounded-full" style={{ background: colorAt(i) }} />
                        {r.name}
                      </td>
                      <td className="px-2 py-1.5 text-right tabular-nums">{formatRef(r.value)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div className="h-56">
              <ResponsiveContainer>
                <PieChart>
                  <Pie data={monthTotals} dataKey="value" nameKey="name" innerRadius={40} outerRadius={75} paddingAngle={2}>
                    {monthTotals.map((_, i) => <Cell key={i} fill={colorAt(i)} stroke="oklch(0.16 0.02 265)" />)}
                  </Pie>
                  <Tooltip content={<DarkTooltip />} />
                </PieChart>
              </ResponsiveContainer>
            </div>
          </div>
        </GlassCard>

        <GlassCard title={`Year ${year} pie`}>
          <div className="h-72">
            <ResponsiveContainer>
              <PieChart>
                <Pie data={yearPie} dataKey="value" nameKey="name" innerRadius={55} outerRadius={100} paddingAngle={2}>
                  {yearPie.map((_, i) => <Cell key={i} fill={colorAt(i)} stroke="oklch(0.16 0.02 265)" />)}
                </Pie>
                <Tooltip content={<DarkTooltip />} />
                <Legend verticalAlign="bottom" wrapperStyle={{ fontSize: 11 }} />
              </PieChart>
            </ResponsiveContainer>
          </div>
        </GlassCard>
      </div>

      <GlassCard title={`Year ${year} by ${kind} category`}>
        <div className="scrollbar-thin overflow-x-auto">
          <table className="w-full min-w-[920px] text-sm">
            <thead>
              <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
                <th className="px-3 py-2 font-medium">Category</th>
                {MONTHS_SHORT.map((m) => <th key={m} className="px-2 py-2 text-right font-medium">{m}</th>)}
                <th className="px-3 py-2 text-right font-medium">Total</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border/40">
              {yearTable.map((r) => (
                <tr key={r.category} className="hover:bg-muted/30">
                  <td className="px-3 py-1.5 font-medium">{r.category}</td>
                  {r.months.map((v, i) => (
                    <td key={i} className="px-2 py-1.5 text-right text-xs tabular-nums text-muted-foreground">
                      {v > 0 ? formatRef(v).replace("€", "") : "—"}
                    </td>
                  ))}
                  <td className="px-3 py-1.5 text-right font-semibold tabular-nums">{formatRef(r.total)}</td>
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
                <button key={m} onClick={() => toggleMonth(idx)}
                  className={active
                    ? "rounded-md bg-gradient-brand px-2 py-1 text-xs font-semibold text-background"
                    : "rounded-md border border-border bg-surface/40 px-2 py-1 text-xs text-muted-foreground hover:text-foreground"}>
                  {m}
                </button>
              );
            })}
          </div>
          <div className="h-72">
            <ResponsiveContainer>
              <BarChart data={compareData}>
                <CartesianGrid strokeDasharray="3 3" stroke="oklch(1 0 0 / 6%)" />
                <XAxis dataKey="category" tick={{ fontSize: 10, fill: "oklch(0.68 0.02 260)" }} interval={0} angle={-25} textAnchor="end" height={60} />
                <YAxis tick={{ fontSize: 10, fill: "oklch(0.68 0.02 260)" }} />
                <Tooltip content={<DarkTooltip total />} cursor={{ fill: "oklch(1 0 0 / 4%)" }} />
                <Legend wrapperStyle={{ fontSize: 11 }} />
                {selMonths.map((m, i) => (
                  <Bar key={m} dataKey={MONTHS_SHORT[m - 1]} fill={colorAt(i)} radius={[4, 4, 0, 0]} />
                ))}
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
                <button key={c} onClick={() => toggleCat(c)}
                  className={active
                    ? "rounded-md bg-gradient-brand px-2 py-1 text-xs font-semibold text-background"
                    : "rounded-md border border-border bg-surface/40 px-2 py-1 text-xs text-muted-foreground hover:text-foreground"}>
                  {c}
                </button>
              );
            })}
          </div>
          <div className="h-72">
            <ResponsiveContainer>
              <LineChart data={trendData}>
                <CartesianGrid strokeDasharray="3 3" stroke="oklch(1 0 0 / 6%)" />
                <XAxis dataKey="month" tick={{ fontSize: 10, fill: "oklch(0.68 0.02 260)" }} />
                <YAxis tick={{ fontSize: 10, fill: "oklch(0.68 0.02 260)" }} />
                <Tooltip content={<DarkTooltip />} cursor={{ stroke: "oklch(1 0 0 / 10%)" }} />
                <Legend wrapperStyle={{ fontSize: 11 }} />
                {selCats.map((c, i) => (
                  <Line key={c} type="monotone" dataKey={c} stroke={colorAt(i)} strokeWidth={2.5} dot={{ r: 3 }} activeDot={{ r: 5 }} />
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
  const [form, setForm] = useState({ name: "", day: "1", amount: "", currency: "EUR" as Currency, primary: "", secondary: "" });

  useEffect(() => {
    api.getRecurring(year).then(setItems);
    api.getCategories().then(setCats);
  }, [refreshTick]);

  const submit = async () => {
    const amt = evalMath(form.amount);
    if (!form.name.trim() || !Number.isFinite(amt)) { notify("error", "Name and amount required"); return; }
    await api.addRecurring({
      year,
      name: form.name.trim(),
      day: Math.max(1, Math.min(28, Number(form.day) || 1)),
      amount: amt, currency: form.currency, primary: form.primary, secondary: form.secondary,
    });
    setForm({ name: "", day: "1", amount: "", currency: "EUR", primary: "", secondary: "" });
    notify("success", "Template added"); refresh();
  };

  const apply = async () => {
    notify("loading", "Applying recurring…");
    const n = await api.applyRecurring(year, month);
    notify("success", `Added ${n} entries to ${MONTHS[month - 1]} ${year}`);
    refresh();
  };

  return (
    <div className="grid gap-5 lg:grid-cols-[1fr_360px]">
      <GlassCard
        title={`${items.length} recurring templates`}
        action={
          <button onClick={apply}
            className="hover-lift inline-flex items-center gap-2 rounded-lg bg-gradient-brand px-3 py-1.5 text-xs font-semibold text-background">
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
                  <td className="px-3 py-2"><CategoryChip name={r.primary} /></td>
                  <td className="px-3 py-2"><CategoryChip name={r.secondary} variant="muted" /></td>
                  <td className="px-3 py-2 text-right">
                    <ConfirmButton onConfirm={async () => {
                      await api.deleteRecurring(r.id, year);
                      notify("success", `Removed "${r.name}"`); refresh();
                    }} />
                  </td>
                </tr>
              ))}
              {items.length === 0 && (
                <tr><td colSpan={7} className="px-3 py-8 text-center text-muted-foreground">No recurring templates yet.</td></tr>
              )}
            </tbody>
          </table>
        </div>
      </GlassCard>

      <GlassCard title="Add recurring template">
        <div className="space-y-3">
          <Field label="Name">
            <input value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })}
              className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none" />
          </Field>
          <div className="grid grid-cols-2 gap-3">
            <Field label="Day (1-28)">
              <input type="number" min={1} max={28} value={form.day} onChange={(e) => setForm({ ...form, day: e.target.value })}
                className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none" />
            </Field>
            <Field label="Amount">
              <input value={form.amount} onChange={(e) => setForm({ ...form, amount: e.target.value })}
                className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none" />
            </Field>
          </div>
          <Field label="Currency">
            <select value={form.currency} onChange={(e) => setForm({ ...form, currency: e.target.value as Currency })}
              className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm">
              {CURRENCIES.map((c) => <option key={c} value={c}>{c}</option>)}
            </select>
          </Field>
          <Field label="Primary"><Combobox value={form.primary} onChange={(v) => setForm({ ...form, primary: v })} options={cats.primary} /></Field>
          <Field label="Secondary"><Combobox value={form.secondary} onChange={(v) => setForm({ ...form, secondary: v })} options={cats.secondary} /></Field>
          <button onClick={submit}
            className="hover-lift inline-flex w-full items-center justify-center gap-2 rounded-md bg-gradient-brand px-3 py-2 text-sm font-semibold text-background">
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

  useEffect(() => {
    api.getMappings().then(setItems);
    api.getCategories().then(setCats);
  }, [refreshTick]);

  const submit = async () => {
    if (!form.match.trim() || !form.primary.trim()) { notify("error", "Match and primary required"); return; }
    await api.addMapping({ match: form.match.trim(), primary: form.primary, secondary: form.secondary });
    setForm({ match: "", primary: "", secondary: "" });
    notify("success", "Mapping added"); refresh();
  };

  return (
    <div className="grid gap-5 lg:grid-cols-[1fr_360px]">
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
                  <td className="px-3 py-2"><CategoryChip name={r.primary} /></td>
                  <td className="px-3 py-2"><CategoryChip name={r.secondary} variant="muted" /></td>
                  <td className="px-3 py-2 text-right">
                    <ConfirmButton onConfirm={async () => {
                      await api.deleteMapping(r.id);
                      notify("success", "Mapping removed"); refresh();
                    }} />
                  </td>
                </tr>
              ))}
              {items.length === 0 && (
                <tr><td colSpan={4} className="px-3 py-8 text-center text-muted-foreground">No mapping rules yet.</td></tr>
              )}
            </tbody>
          </table>
        </div>
      </GlassCard>

      <GlassCard title="Add mapping rule">
        <div className="space-y-3">
          <Field label="Match substring (case-insensitive)">
            <input value={form.match} onChange={(e) => setForm({ ...form, match: e.target.value })} placeholder="lidl"
              className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none" />
          </Field>
          <Field label="Primary"><Combobox value={form.primary} onChange={(v) => setForm({ ...form, primary: v })} options={cats.primary} /></Field>
          <Field label="Secondary"><Combobox value={form.secondary} onChange={(v) => setForm({ ...form, secondary: v })} options={cats.secondary} /></Field>
          <button onClick={submit}
            className="hover-lift inline-flex w-full items-center justify-center gap-2 rounded-md bg-gradient-brand px-3 py-2 text-sm font-semibold text-background">
            <Plus className="h-4 w-4" /> Add rule
          </button>
        </div>
      </GlassCard>
    </div>
  );
}