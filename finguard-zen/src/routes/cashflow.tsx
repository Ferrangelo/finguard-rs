import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useMemo, useState } from "react";
import {
  Bar,
  BarChart,
  CartesianGrid,
  Cell,
  Legend,
  Pie,
  PieChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import { useApp } from "@/context/AppContext";
import * as api from "@/services/api";
import { MONTHS_SHORT } from "@/services/api";
import { formatRef } from "@/services/fx";
import { GlassCard } from "@/components/finguard/GlassCard";
import { MathInput } from "@/components/finguard/MathInput";
import { DarkTooltip, useChartColors, LEGEND_STYLE } from "@/components/finguard/DarkTooltip";
import { INCOME_CATEGORIES } from "@/services/types";

export const Route = createFileRoute("/cashflow")({
  head: () => ({ meta: [{ title: "Cashflow · Finguard" }] }),
  component: CashflowPage,
});

function CashflowPage() {
  const colorAt = useChartColors();
  const { year, notify, refresh, refreshTick } = useApp();
  const [income, setIncome] = useState<Record<number, Record<string, number>>>({});
  const [spending, setSpending] = useState<Record<number, Record<string, number>>>({});

  useEffect(() => {
    api.getIncome(year).then(setIncome);
    api.getMonthlySpendingByPrimary(year).then(setSpending);
  }, [year, refreshTick]);

  const months = useMemo(() => Array.from({ length: 12 }, (_, i) => i + 1), []);

  const totalSpendingByMonth = useMemo(
    () => months.map((m) => Object.values(spending[m] ?? {}).reduce((s, n) => s + n, 0)),
    [spending, months],
  );
  const totalIncomeByMonth = useMemo(
    () => months.map((m) => INCOME_CATEGORIES.reduce((s, c) => s + (income[m]?.[c] ?? 0), 0)),
    [income, months],
  );
  const savings = totalIncomeByMonth.map((inc, i) => inc - totalSpendingByMonth[i]);
  const savingsPct = totalIncomeByMonth.map((inc, i) => (inc > 0 ? (100 * savings[i]) / inc : 0));

  const chartData = months.map((m, i) => ({
    month: MONTHS_SHORT[m - 1],
    Income: totalIncomeByMonth[i],
    Spending: totalSpendingByMonth[i],
    Saving: savings[i],
  }));

  const incomePie = INCOME_CATEGORIES.map((c) => ({
    name: c,
    value: months.reduce((s, m) => s + (income[m]?.[c] ?? 0), 0),
  })).filter((d) => d.value > 0);

  const setCell = async (m: number, cat: string, v: number) => {
    setIncome((prev) => ({ ...prev, [m]: { ...(prev[m] ?? {}), [cat]: v } }));
    await api.setIncomeCell(year, m, cat, v);
    notify("success", `${cat} · ${MONTHS_SHORT[m - 1]} saved`);
    refresh();
  };

  return (
    <div className="space-y-5">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Cashflow</h1>
        <p className="text-sm text-muted-foreground">Monthly income vs. spending across {year}.</p>
      </div>

      <GlassCard title={`Cashflow grid · ${year}`}>
        <div className="scrollbar-thin overflow-x-auto">
          <table className="w-full min-w-[1100px] text-sm">
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
              {INCOME_CATEGORIES.map((cat) => {
                const total = months.reduce((s, m) => s + (income[m]?.[cat] ?? 0), 0);
                return (
                  <tr key={cat} className="hover:bg-muted/20">
                    <td className="px-3 py-1.5 font-medium">{cat}</td>
                    {months.map((m) => (
                      <td key={m} className="px-1 py-1">
                        <MathInput
                          value={income[m]?.[cat] ?? 0}
                          onCommit={(v) => setCell(m, cat, v)}
                        />
                      </td>
                    ))}
                    <td className="px-3 py-1.5 text-right font-semibold tabular-nums">
                      {formatRef(total)}
                    </td>
                  </tr>
                );
              })}
              <DerivedRow label="Income" values={totalIncomeByMonth} variant="strong" />
              <DerivedRow label="Spending" values={totalSpendingByMonth} variant="negative" />
              <DerivedRow label="Saving" values={savings} variant="positive" />
              <DerivedRow label="Saving %" values={savingsPct} variant="percent" />
            </tbody>
          </table>
        </div>
      </GlassCard>

      <div className="grid gap-5 lg:grid-cols-2">
        <GlassCard title="Income vs Spending vs Saving">
          <div className="h-80">
            <ResponsiveContainer>
              <BarChart data={chartData}>
                <CartesianGrid strokeDasharray="3 3" stroke="oklch(1 0 0 / 6%)" />
                <XAxis dataKey="month" tick={{ fontSize: 11, fill: "oklch(0.68 0.02 260)" }} />
                <YAxis tick={{ fontSize: 11, fill: "oklch(0.68 0.02 260)" }} />
                <Tooltip content={<DarkTooltip />} cursor={{ fill: "oklch(1 0 0 / 4%)" }} />
                <Legend wrapperStyle={LEGEND_STYLE} />
                <Bar dataKey="Income" fill={colorAt(0)} radius={[4, 4, 0, 0]} />
                <Bar dataKey="Spending" fill={colorAt(4)} radius={[4, 4, 0, 0]} />
                <Bar dataKey="Saving" fill={colorAt(2)} radius={[4, 4, 0, 0]} />
              </BarChart>
            </ResponsiveContainer>
          </div>
        </GlassCard>

        <GlassCard title="Income distribution">
          <div className="h-80">
            <ResponsiveContainer>
              <PieChart>
                <Pie
                  data={incomePie}
                  dataKey="value"
                  nameKey="name"
                  innerRadius={55}
                  outerRadius={110}
                  paddingAngle={2}
                >
                  {incomePie.map((_, i) => (
                    <Cell key={i} fill={colorAt(i)} stroke="oklch(0.16 0.02 265)" />
                  ))}
                </Pie>
                <Tooltip content={<DarkTooltip />} />
                <Legend verticalAlign="bottom" wrapperStyle={LEGEND_STYLE} />
              </PieChart>
            </ResponsiveContainer>
          </div>
        </GlassCard>
      </div>
    </div>
  );
}

function DerivedRow({
  label,
  values,
  variant,
}: {
  label: string;
  values: number[];
  variant: "strong" | "positive" | "negative" | "percent";
}) {
  const total =
    variant === "percent"
      ? values.reduce((s, n) => s + n, 0) / Math.max(values.length, 1)
      : values.reduce((s, n) => s + n, 0);
  const fmt = (n: number) => (variant === "percent" ? `${n.toFixed(1)}%` : formatRef(n));
  const cls =
    variant === "positive"
      ? "text-success"
      : variant === "negative"
        ? "text-destructive"
        : variant === "percent"
          ? "text-warning"
          : "text-foreground";
  const bg =
    variant === "positive"
      ? "bg-success/5"
      : variant === "negative"
        ? "bg-destructive/5"
        : variant === "percent"
          ? "bg-warning/5"
          : "bg-muted/30";
  return (
    <tr className={`${bg} font-semibold`}>
      <td className={`px-3 py-1.5 ${cls}`}>{label}</td>
      {values.map((v, i) => (
        <td key={i} className={`px-2 py-1.5 text-right tabular-nums ${cls}`}>
          {fmt(v)}
        </td>
      ))}
      <td className={`px-3 py-1.5 text-right tabular-nums ${cls}`}>{fmt(total)}</td>
    </tr>
  );
}
