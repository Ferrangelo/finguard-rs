import { useEffect, useState } from "react";
import { evalMath } from "@/services/mathEval";
import { cn } from "@/lib/utils";

export function MathInput({
  value, onCommit, placeholder, className,
}: {
  value: number;
  onCommit: (v: number) => void;
  placeholder?: string;
  className?: string;
}) {
  const [draft, setDraft] = useState<string>(Number.isFinite(value) ? String(value) : "");
  const [focused, setFocused] = useState(false);

  useEffect(() => {
    if (!focused) setDraft(Number.isFinite(value) ? String(value) : "");
  }, [value, focused]);

  const commit = () => {
    const v = evalMath(draft);
    if (Number.isFinite(v)) { onCommit(v); setDraft(String(v)); }
    else setDraft(Number.isFinite(value) ? String(value) : "");
  };

  return (
    <input
      type="text"
      inputMode="decimal"
      value={draft}
      placeholder={placeholder}
      onFocus={() => setFocused(true)}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => { setFocused(false); commit(); }}
      onKeyDown={(e) => {
        if (e.key === "Enter") (e.target as HTMLInputElement).blur();
        if (e.key === "Escape") { setDraft(String(value)); (e.target as HTMLInputElement).blur(); }
      }}
      className={cn(
        "w-full rounded-md border border-transparent bg-transparent px-2 py-1 text-right text-sm tabular-nums",
        "focus:border-primary/60 focus:bg-surface/70 focus:outline-none",
        className,
      )}
    />
  );
}