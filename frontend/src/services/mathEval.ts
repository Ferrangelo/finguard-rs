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