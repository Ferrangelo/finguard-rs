// Client-side counterpart to the arithmetic grammar in
// backend/src/expr.rs (`+ - * /`, unary +/-, parentheses, decimal
// literals). The two are not connected at runtime: no API route calls
// into expr.rs, so evalMath is what actually resolves a "10+5.5"-style
// amount field before it is sent to the backend as a plain number; the
// backend only ever receives the already-evaluated numeric result.

/**
 * Evaluates a simple arithmetic expression (digits, `+ - * /`, parentheses,
 * decimal points, and whitespace only) and returns the numeric result, or `NaN` for
 * any invalid input, including division by zero, an empty string, and any
 * character outside that allowed set. A numeric `input` is returned as-is.
 */
export function evalMath(input: string | number): number {
  if (typeof input === "number") return input;
  const trimmed = String(input).trim();
  if (!trimmed) return NaN;
  if (!/^[0-9+\-*/.()\s]+$/.test(trimmed)) return NaN;
  try {
    // eslint-disable-next-line no-new-func
    const v = Function(`"use strict"; return (${trimmed});`)();
    return typeof v === "number" && Number.isFinite(v) ? v : NaN;
  } catch {
    return NaN;
  }
}