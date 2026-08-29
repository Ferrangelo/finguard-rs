// Bridges runtime errors to the Lovable editor's error overlay. Lovable
// injects `window.__lovableEvents` when the app runs inside its editor
// preview; outside that environment this is undefined, so calls here are
// a no-op everywhere else (production, local dev outside Lovable, tests).
type LovableErrorOptions = {
  mechanism?: "manual" | "onerror" | "unhandledrejection" | "react_error_boundary";
  handled?: boolean;
  severity?: "error" | "warning" | "info";
};

type LovableEvents = {
  captureException?: (
    error: unknown,
    context?: Record<string, unknown>,
    options?: LovableErrorOptions,
  ) => void;
};

declare global {
  interface Window {
    __lovableEvents?: LovableEvents;
  }
}

/**
 * Reports `error` to Lovable's injected error hook, if present, tagging it
 * as an unhandled React error boundary catch with the current pathname.
 * `context` is merged into the reported payload; on the server (no
 * `window`) this is a no-op.
 */
export function reportLovableError(error: unknown, context: Record<string, unknown> = {}) {
  if (typeof window === "undefined") return;
  window.__lovableEvents?.captureException?.(
    error,
    {
      source: "react_error_boundary",
      route: window.location.pathname,
      ...context,
    },
    {
      mechanism: "react_error_boundary",
      handled: false,
      severity: "error",
    },
  );
}
