import { useState } from "react";
import { cn } from "@/lib/utils";

export function ConfirmButton({
  onConfirm, label = "Delete", confirmLabel = "Confirm?", className,
}: { onConfirm: () => void; label?: string; confirmLabel?: string; className?: string }) {
  const [armed, setArmed] = useState(false);
  return (
    <button
      type="button"
      onClick={() => {
        if (armed) { onConfirm(); setArmed(false); }
        else { setArmed(true); setTimeout(() => setArmed(false), 2500); }
      }}
      className={cn(
        "rounded-md border px-2 py-1 text-xs font-medium transition-all",
        armed
          ? "border-destructive bg-destructive/20 text-destructive animate-pulse"
          : "border-border bg-transparent text-muted-foreground hover:border-destructive/60 hover:text-destructive",
        className,
      )}
    >
      {armed ? confirmLabel : label}
    </button>
  );
}