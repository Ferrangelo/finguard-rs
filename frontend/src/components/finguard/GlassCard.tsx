import type { ReactNode } from "react";
import { cn } from "@/lib/utils";

/**
 * The app's standard panel container (frosted-glass background via the
 * `glass` utility class), with an optional header row showing `title` on
 * the left and `action` (e.g. a button or summary) on the right. The
 * header is omitted entirely when neither `title` nor `action` is given.
 */
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