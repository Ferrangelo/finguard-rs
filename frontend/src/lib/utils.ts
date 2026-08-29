import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

/**
 * Combines conditional class name inputs (`clsx`) and then resolves
 * conflicting Tailwind utility classes so the last one wins (`twMerge`),
 * e.g. `cn("px-2", condition && "px-4")` yields `"px-4"` when `condition`
 * is true, instead of both classes being applied.
 */
export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}
