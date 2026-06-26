import { Link, useLocation } from "@tanstack/react-router";
import {
  Shield,
  Wallet,
  TrendingUp,
  DollarSign,
  PiggyBank,
  LineChart,
  PieChart,
  Layers,
} from "lucide-react";
import { useApp } from "@/context/AppContext";
import { MONTHS } from "@/services/api";
import { StatusPill } from "./StatusPill";
import { ThemeSwitcher } from "@/components/ThemeSwitcher";
import { cn } from "@/lib/utils";

const TABS = [
  { to: "/expenses", label: "Expenses", icon: Wallet },
  { to: "/cashflow", label: "Cashflow", icon: LineChart },
  { to: "/networth", label: "NetWorth", icon: PieChart },
  { to: "/categories", label: "Categories", icon: Layers },
] as const;

export function Header() {
  const { year, month, years, setYear, setMonth } = useApp();
  const loc = useLocation();

  return (
    <header className="sticky top-0 z-40 border-b border-border/60 bg-background/60 backdrop-blur-xl">
      <div className="mx-auto flex max-w-[1600px] flex-wrap items-center gap-4 px-6 py-3">
        <div className="flex items-center gap-3">
          <div
            className="relative grid h-9 w-9 place-items-center rounded-xl bg-gradient-brand"
            style={{ boxShadow: "var(--shadow-glow)" }}
          >
            <Wallet className="h-5 w-5 text-background" strokeWidth={2.5} />
          </div>
          <div className="flex flex-col leading-none">
            <span className="text-lg font-bold tracking-tight text-gradient">Finguard</span>
            <span className="mt-0.5 text-[10px] uppercase tracking-[0.18em] text-muted-foreground">
              Personal Finance Tracker
            </span>
          </div>
        </div>

        <nav className="ml-6 hidden items-center gap-1 rounded-xl border border-border/60 bg-surface/40 p-1 md:flex">
          {TABS.map((t) => {
            const active = loc.pathname.startsWith(t.to);
            const Icon = t.icon;
            return (
              <Link
                key={t.to}
                to={t.to}
                className={cn(
                  "inline-flex items-center gap-2 rounded-lg px-3 py-1.5 text-sm font-medium transition-all",
                  active
                    ? "bg-gradient-brand text-background"
                    : "text-muted-foreground hover:bg-muted/60 hover:text-foreground",
                )}
              >
                <Icon className="h-4 w-4" />
                {t.label}
              </Link>
            );
          })}
        </nav>

        <div className="ml-auto flex flex-wrap items-center gap-2">
          <select
            aria-label="Year"
            value={year}
            onChange={(e) => setYear(Number(e.target.value))}
            className="rounded-lg border border-border bg-surface/70 px-3 py-1.5 text-sm font-medium text-foreground transition-colors hover:bg-surface focus:outline-none focus:ring-2 focus:ring-ring"
          >
            {years.map((y) => (
              <option key={y} value={y}>
                {y}
              </option>
            ))}
          </select>
          <select
            aria-label="Month"
            value={month}
            onChange={(e) => setMonth(Number(e.target.value))}
            className="rounded-lg border border-border bg-surface/70 px-3 py-1.5 text-sm font-medium text-foreground transition-colors hover:bg-surface focus:outline-none focus:ring-2 focus:ring-ring"
          >
            {MONTHS.map((m, i) => (
              <option key={m} value={i + 1}>
                {m}
              </option>
            ))}
          </select>
          <ThemeSwitcher />
          <StatusPill />
        </div>
      </div>

      <nav className="border-t border-border/60 px-6 py-2 md:hidden">
        <div className="flex gap-1 overflow-x-auto">
          {TABS.map((t) => {
            const active = loc.pathname.startsWith(t.to);
            return (
              <Link
                key={t.to}
                to={t.to}
                className={cn(
                  "rounded-md px-3 py-1.5 text-sm font-medium",
                  active ? "bg-gradient-brand text-background" : "text-muted-foreground",
                )}
              >
                {t.label}
              </Link>
            );
          })}
        </div>
      </nav>
    </header>
  );
}
