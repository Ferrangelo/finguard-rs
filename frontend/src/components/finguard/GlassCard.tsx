import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

export function GlassCard({
  children, className, title, action,
}: { children: ReactNode; className?: string; title?: ReactNode; action?: ReactNode }) {
  return (
    <section className={cn("glass rounded-xl p-5 animate-fade-in", className)}>
      {(title || action) && (
        <header className="mb-4 flex items-center justify-between gap-3">
          {title && <h2 className="text-sm font-semibold uppercase tracking-wide text-foreground/90">{title}</h2>}
          {action}
        </header>
      )}
      {children}
    </section>
  );
}