import { useApp } from "@/context/AppContext";
import { CheckCircle2, CircleAlert, Loader2, Circle } from "lucide-react";

export function StatusPill() {
  const { status } = useApp();
  const Icon =
    status.kind === "success" ? CheckCircle2 :
    status.kind === "error" ? CircleAlert :
    status.kind === "loading" ? Loader2 : Circle;
  const color =
    status.kind === "success" ? "text-success border-success/40 bg-success/10" :
    status.kind === "error" ? "text-destructive border-destructive/40 bg-destructive/10" :
    status.kind === "loading" ? "text-primary border-primary/40 bg-primary/10" :
    "text-muted-foreground border-border bg-muted/30";
  return (
    <div className={`inline-flex items-center gap-2 rounded-full border px-3 py-1 text-xs font-medium transition-colors ${color}`}>
      <Icon className={`h-3.5 w-3.5 ${status.kind === "loading" ? "animate-spin" : ""}`} />
      <span className="max-w-[220px] truncate">{status.text}</span>
    </div>
  );
}