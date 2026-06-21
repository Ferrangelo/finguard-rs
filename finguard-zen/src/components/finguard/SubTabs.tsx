import { cn } from "@/lib/utils";

export function SubTabs<T extends string>({
  value, onChange, options,
}: { value: T; onChange: (v: T) => void; options: ReadonlyArray<{ value: T; label: string }> }) {
  return (
    <div className="inline-flex rounded-xl border border-border/60 bg-surface/40 p-1">
      {options.map((o) => (
        <button
          key={o.value}
          onClick={() => onChange(o.value)}
          className={cn(
            "rounded-lg px-3 py-1.5 text-sm font-medium transition-all",
            value === o.value
              ? "bg-gradient-brand text-background"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}