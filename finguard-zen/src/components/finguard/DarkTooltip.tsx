"use client";

import { useTheme } from "@/context/ThemeContext";
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
      {label != null && (
        <div className="mb-1 text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
          {label}
        </div>
      )}
      <ul className="space-y-0.5">
        {payload.map((p, i) => (
          <li key={i} className="flex items-center gap-2">
            <span className="h-2 w-2 rounded-full" style={{ background: p.color }} />
            <span className="text-foreground/90">{p.name}</span>
            <span className="ml-auto font-medium text-foreground">
              {formatRef(Number(p.value) || 0)}
            </span>
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

// Palette for dark themes — high lightness, visible on dark backgrounds
const DARK_COLORS = [
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

// Palette for arctic (light) theme — lower lightness + higher chroma, visible on white/light-grey
const ARCTIC_COLORS = [
  "oklch(0.42 0.22 230)", // deep blue
  "oklch(0.45 0.22 295)", // deep purple
  "oklch(0.42 0.20 155)", // deep green
  "oklch(0.52 0.18 75)", // deep amber
  "oklch(0.48 0.24 20)", // deep red
  "oklch(0.44 0.20 185)", // deep teal
  "oklch(0.46 0.18 130)", // deep olive
  "oklch(0.46 0.22 340)", // deep pink
  "oklch(0.50 0.20 50)", // deep orange
  "oklch(0.43 0.18 260)", // deep indigo
];

const LIGHT_THEMES = new Set(["arctic"]);

/** React hook — returns a colorAt() bound to the active theme's palette. */
export function useChartColors() {
  const { theme } = useTheme();
  const palette = LIGHT_THEMES.has(theme) ? ARCTIC_COLORS : DARK_COLORS;
  return (i: number) => palette[i % palette.length];
}

/** wrapperStyle for every Recharts <Legend> — uses arctic-specific CSS vars with fallbacks. */
export const LEGEND_STYLE: React.CSSProperties = {
  fontSize: 11,
  backgroundColor: "var(--legend-bg, var(--muted))",
  color: "var(--legend-fg, var(--muted-foreground))",
  border: "1px solid var(--legend-border, transparent)",
  borderRadius: 6,
  padding: "4px 10px",
};
