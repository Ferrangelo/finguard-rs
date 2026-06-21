import { formatRef } from "@/services/fx";

interface Props {
  active?: boolean;
  payload?: Array<{ name: string; value: number; color?: string }>;
  label?: string | number;
  total?: boolean;
}

export function DarkTooltip({ active, payload, label, total }: Props) {
  if (!active || !payload || payload.length === 0) return null;
  const sum = payload.reduce((s, p) => s + (Number(p.value) || 0), 0);
  return (
    <div className="rounded-lg border border-border/80 bg-popover/95 px-3 py-2 text-xs shadow-elegant backdrop-blur-md">
      {label != null && <div className="mb-1 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">{label}</div>}
      <ul className="space-y-0.5">
        {payload.map((p, i) => (
          <li key={i} className="flex items-center gap-2">
            <span className="h-2 w-2 rounded-full" style={{ background: p.color }} />
            <span className="text-foreground/90">{p.name}</span>
            <span className="ml-auto font-medium text-foreground">{formatRef(Number(p.value) || 0)}</span>
          </li>
        ))}
        {total && payload.length > 1 && (
          <li className="mt-1 flex items-center gap-2 border-t border-border/60 pt-1 text-foreground/80">
            <span className="ml-auto text-[11px]">Total</span>
            <span className="font-semibold text-foreground">{formatRef(sum)}</span>
          </li>
        )}
      </ul>
    </div>
  );
}

export const CHART_COLORS = [
  "oklch(0.78 0.14 200)",
  "oklch(0.70 0.18 295)",
  "oklch(0.74 0.16 160)",
  "oklch(0.80 0.16 75)",
  "oklch(0.68 0.20 20)",
  "oklch(0.72 0.12 240)",
  "oklch(0.76 0.14 130)",
  "oklch(0.74 0.18 340)",
  "oklch(0.70 0.13 50)",
  "oklch(0.66 0.10 260)",
];

export function colorAt(i: number) { return CHART_COLORS[i % CHART_COLORS.length]; }