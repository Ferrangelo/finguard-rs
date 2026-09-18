import { ConfirmButton } from "@/components/finguard/ConfirmButton";
import { GlassCard } from "@/components/finguard/GlassCard";
import type { SkippedRecurring } from "@/services/types";

/**
 * Lists the recurring templates that one `applyRecurring` call withheld
 * because the user had deleted that generated row, and offers each one back
 * through `onReinstate`.
 *
 * Skipping is the expected outcome of "apply after a delete", not a failure,
 * so this uses the page's ordinary `GlassCard` panel rather than the
 * destructive error styling. The caller renders it only when `items` is not
 * empty, which keeps the page unchanged after the usual apply that skips
 * nothing.
 *
 * `onReinstate` owns the request, its errors, and removing the row from
 * `items`. This component only reports progress: a `row_id` in `busyRowIds`
 * replaces that row's button, so a second click cannot start a second
 * request for the same row.
 */
export function SkippedRecurringPanel({
  items,
  periodLabel,
  busyRowIds,
  onReinstate,
}: {
  items: SkippedRecurring[];
  /** The month the skipped rows belong to, for example `"March 2026"`. */
  periodLabel: string;
  busyRowIds: string[];
  onReinstate: (item: SkippedRecurring) => void;
}) {
  const one = items.length === 1;
  return (
    <GlassCard title={`Not added to ${periodLabel}`}>
      <p className="mb-4 text-xs text-muted-foreground">
        {one ? "This row was" : `These ${items.length} rows were`} deleted after an earlier apply,
        so this apply left {one ? "it" : "them"} out. Add back anything you still want in{" "}
        {periodLabel}.
      </p>
      <div className="scrollbar-thin overflow-x-auto">
        <table className="w-full min-w-[640px] text-sm">
          <thead>
            <tr className="text-left text-[11px] uppercase tracking-wider text-muted-foreground">
              <th className="px-3 py-2 font-medium">Name</th>
              <th className="px-3 py-2 text-right font-medium">Day</th>
              <th className="px-3 py-2 text-right font-medium">Amount</th>
              <th className="px-3 py-2 font-medium">Curr</th>
              <th className="px-3 py-2 font-medium">Category</th>
              <th className="px-3 py-2"></th>
            </tr>
          </thead>
          <tbody className="divide-y divide-border/40">
            {items.map((item) => (
              <tr key={item.row_id} className="hover:bg-muted/30">
                <td className="px-3 py-2 font-medium">{item.name}</td>
                <td className="px-3 py-2 text-right tabular-nums">{item.day}</td>
                <td className="px-3 py-2 text-right tabular-nums">{item.amount.toFixed(2)}</td>
                <td className="px-3 py-2 text-xs text-muted-foreground">{item.currency}</td>
                <td className="px-3 py-2 text-xs text-muted-foreground">
                  {[item.primary, item.secondary].filter(Boolean).join(" · ") || "—"}
                </td>
                <td className="px-3 py-2 text-right">
                  {busyRowIds.includes(item.row_id) ? (
                    <span className="text-xs text-muted-foreground">Adding…</span>
                  ) : (
                    // ConfirmButton's own colors mark a delete. Putting a row
                    // back is not one, so both of its states are re-tinted to
                    // the primary color here; the label change and the pulse
                    // still distinguish the armed state.
                    <ConfirmButton
                      label="Add back"
                      confirmLabel="Confirm add back"
                      className="border-primary/40 bg-primary/10 text-primary hover:border-primary hover:bg-primary/20 hover:text-primary"
                      onConfirm={() => onReinstate(item)}
                    />
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </GlassCard>
  );
}
