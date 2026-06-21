import { useEffect, useMemo, useRef, useState } from "react";
import { cn } from "@/lib/utils";

export function Combobox({
  value, onChange, options, placeholder, className,
}: {
  value: string;
  onChange: (v: string) => void;
  options: string[];
  placeholder?: string;
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const onDoc = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onDoc);
    return () => document.removeEventListener("mousedown", onDoc);
  }, []);

  const filtered = useMemo(() => {
    const q = value.toLowerCase().trim();
    return options.filter((o) => o.toLowerCase().includes(q)).slice(0, 8);
  }, [value, options]);

  return (
    <div ref={ref} className={cn("relative", className)}>
      <input
        type="text"
        value={value}
        placeholder={placeholder}
        onChange={(e) => { onChange(e.target.value); setOpen(true); }}
        onFocus={() => setOpen(true)}
        className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm transition-colors focus:border-primary/60 focus:outline-none focus:ring-2 focus:ring-ring/40"
      />
      {open && filtered.length > 0 && (
        <ul className="absolute z-30 mt-1 max-h-56 w-full overflow-auto rounded-md border border-border bg-popover/95 p-1 text-sm shadow-elegant backdrop-blur-md animate-fade-in scrollbar-thin">
          {filtered.map((o) => (
            <li key={o}>
              <button
                type="button"
                className="block w-full rounded px-2 py-1 text-left hover:bg-accent/30"
                onMouseDown={(e) => { e.preventDefault(); onChange(o); setOpen(false); }}
              >
                {o}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}