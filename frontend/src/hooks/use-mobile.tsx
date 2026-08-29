import * as React from "react";

const MOBILE_BREAKPOINT = 768;

/**
 * Tracks whether the viewport is narrower than `MOBILE_BREAKPOINT` (768px),
 * updating on resize. Returns `false` during the initial render before the
 * effect runs (including on the server, where there is no `window`), so
 * the first client render matches the server-rendered markup and avoids a
 * hydration mismatch.
 */
export function useIsMobile() {
  const [isMobile, setIsMobile] = React.useState<boolean | undefined>(undefined);

  React.useEffect(() => {
    const mql = window.matchMedia(`(max-width: ${MOBILE_BREAKPOINT - 1}px)`);
    const onChange = () => {
      setIsMobile(window.innerWidth < MOBILE_BREAKPOINT);
    };
    mql.addEventListener("change", onChange);
    setIsMobile(window.innerWidth < MOBILE_BREAKPOINT);
    return () => mql.removeEventListener("change", onChange);
  }, []);

  return !!isMobile;
}
