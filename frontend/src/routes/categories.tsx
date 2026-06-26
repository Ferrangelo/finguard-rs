import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useState } from "react";
import { Plus } from "lucide-react";
import { useApp } from "@/context/AppContext";
import * as api from "@/services/api";
import { formatRef } from "@/services/fx";
import { GlassCard } from "@/components/finguard/GlassCard";
import type { Categories } from "@/services/types";

export const Route = createFileRoute("/categories")({
  head: () => ({ meta: [{ title: "Categories · Finguard" }] }),
  component: CategoriesPage,
});

function CategoriesPage() {
  const { notify, refresh, refreshTick } = useApp();
  const [cats, setCats] = useState<Categories>({ primary: [], secondary: [] });
  const [pri, setPri] = useState<Record<string, number>>({});
  const [sec, setSec] = useState<Record<string, number>>({});

  useEffect(() => {
    api.getCategories().then(setCats);
    api.getCategoryTotals("primary").then(setPri);
    api.getCategoryTotals("secondary").then(setSec);
  }, [refreshTick]);

  return (
    <div className="space-y-5">
      <div>
        <h1 className="text-2xl font-bold tracking-tight">Categories</h1>
        <p className="text-sm text-muted-foreground">Manage your primary and secondary category registries.</p>
      </div>

      <div className="grid gap-5 lg:grid-cols-2">
        <CategoryColumn
          title="Primary categories" kind="primary" list={cats.primary} totals={pri}
          onAdd={async (n) => { await api.addCategory("primary", n); notify("success", `Added "${n}"`); refresh(); }}
          onDelete={async (n) => { await api.deleteCategory("primary", n); notify("success", `Deleted "${n}"`); refresh(); }}
        />
        <CategoryColumn
          title="Secondary categories" kind="secondary" list={cats.secondary} totals={sec}
          onAdd={async (n) => { await api.addCategory("secondary", n); notify("success", `Added "${n}"`); refresh(); }}
          onDelete={async (n) => { await api.deleteCategory("secondary", n); notify("success", `Deleted "${n}"`); refresh(); }}
        />
      </div>
    </div>
  );
}

function CategoryColumn({
  title, kind, list, totals, onAdd, onDelete,
}: {
  title: string;
  kind: "primary" | "secondary";
  list: string[];
  totals: Record<string, number>;
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
              const hasExpenses = t > 0;
              return (
                <tr key={c} className="hover:bg-muted/20">
                  <td className="px-3 py-2 font-medium">{c}</td>
                  <td className="px-3 py-2 text-right tabular-nums">{formatRef(t)}</td>
                  <td className="px-3 py-2 text-right">
                    {hasExpenses ? (
                      <span className="text-[11px] italic text-muted-foreground">has existing expenses</span>
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