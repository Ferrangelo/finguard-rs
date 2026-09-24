import { cn } from "@/lib/utils";

/**
 * A pill-style segmented control for selecting one of `options` (each an
 * arbitrary string-literal `value` paired with a display `label`). Fully
 * controlled: renders whichever option matches `value` as selected and
 * calls `onChange` with the clicked option's value, without holding its
 * own selection state.
 */
export function SubTabs<T extends string>({
  value,
  onChange,
  options,
}: {
  value: T;
  onChange: (v: T) => void;
  options: ReadonlyArray<{ value: T; label: string }>;
}) {
  return (
    <div className="max-w-full overflow-x-auto rounded-xl border border-border/60 bg-surface/40 p-1">
      <div className="inline-flex">
        {options.map((o) => (
          <button
            key={o.value}
            onClick={() => onChange(o.value)}
            className={cn(
              "shrink-0 rounded-lg px-3 py-1.5 text-sm font-medium transition-all",
              value === o.value
                ? "bg-gradient-brand text-background"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {o.label}
          </button>
        ))}
      </div>
    </div>
  );
}
