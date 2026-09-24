// App-wide state shared across every route: the selected year/month, the
// list of years the backend has data for, the currency settings, a status
// notification, and the `refresh`/`refreshTick` pair each route uses as its
// data-invalidation signal (see the data-flow notes at the top of
// routes/expenses.tsx and routes/networth.tsx). This app has no TanStack
// Query cache to invalidate; `refresh()` incrementing `refreshTick` is what
// causes every route's `useEffect`-based fetch to depend on and re-run after
// a mutation.
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import * as api from "@/services/api";
import type { CurrencySettings, StatusMessage } from "@/services/types";

// Used while the real settings are loading or the fetch failed, so every
// consumer has a currency to format with immediately instead of branching
// on `undefined`. EUR matches the backend's own default reference currency.
const FALLBACK_CURRENCY_SETTINGS: CurrencySettings = {
  reference_currency: "EUR",
  current_month_rate_mode: "previous_month_end",
};

interface AppContextValue {
  year: number;
  month: number;
  years: number[];
  setYear: (y: number) => void;
  setMonth: (m: number) => void;
  /** False until the mount effect replaces `year`/`month`/`years` with the real date (see `PLACEHOLDER_YEAR`). A route's own data-fetch effects must wait for this before calling the backend with `year`/`month`, or they fetch the placeholder date once on every launch. */
  dateReady: boolean;
  status: StatusMessage;
  notify: (kind: StatusMessage["kind"], text: string) => void;
  refreshTick: number;
  refresh: () => void;
  /** The saved currency settings, or `FALLBACK_CURRENCY_SETTINGS` before the first fetch resolves or if it fails. Check `currencySettingsLoaded` to tell the two apart. */
  currencySettings: CurrencySettings;
  /** True once `getCurrencySettings` has resolved or failed at least once. Lets a consumer avoid formatting in the fallback currency for the split second before the real setting arrives. */
  currencySettingsLoaded: boolean;
}

const AppCtx = createContext<AppContextValue | null>(null);

// Fixed placeholder for `year`/`month`/`years` until the effect below runs.
// The Android build prerenders this component once at build time, then the
// device hydrates it later against its own clock; seeding the initial state
// from `new Date()` in both places made them disagree on any date after the
// build and triggered a hydration mismatch (React error #418) on the
// `<select>` elements in Header.tsx that render `years`. `ThemeProvider`
// (context/ThemeContext.tsx) uses the same fixed-default-then-effect
// pattern for its client-only localStorage read.
const PLACEHOLDER_YEAR = 1970;
const PLACEHOLDER_MONTH = 1;

/**
 * Provides the shared app state described in the file-level comment above.
 * Refetches the list of available years whenever `refreshTick` changes, in
 * addition to whatever each route's own effects refetch.
 */
export function AppProvider({ children }: { children: ReactNode }) {
  const [year, setYear] = useState(PLACEHOLDER_YEAR);
  const [month, setMonth] = useState(PLACEHOLDER_MONTH);
  const [years, setYears] = useState<number[]>([PLACEHOLDER_YEAR]);
  const [dateReady, setDateReady] = useState(false);
  const [status, setStatus] = useState<StatusMessage>({
    kind: "idle",
    text: "Ready",
    ts: Date.now(),
  });
  const [refreshTick, setRefreshTick] = useState(0);
  const [currencySettings, setCurrencySettings] = useState<CurrencySettings>(
    FALLBACK_CURRENCY_SETTINGS,
  );
  const [currencySettingsLoaded, setCurrencySettingsLoaded] = useState(false);

  // Replaces the placeholder above with the device's real current date.
  // Runs only on the client, after hydration, so it never affects the
  // markup React compares against the prerendered HTML. Sets `dateReady`
  // last so a route's fetch effect that checks it also sees the corrected
  // year/month in the same render, rather than firing once more against
  // the placeholder before the flag catches up.
  useEffect(() => {
    const now = new Date();
    setYear(now.getFullYear());
    setMonth(now.getMonth() + 1);
    setYears([now.getFullYear(), now.getFullYear() - 1]);
    setDateReady(true);
  }, []);

  useEffect(() => {
    api
      .listYears()
      .then(setYears)
      .catch(() => {});
  }, [refreshTick]);

  // Falls back to FALLBACK_CURRENCY_SETTINGS on error, so a route can always
  // format with *some* currency rather than crashing on an undefined value.
  useEffect(() => {
    api
      .getCurrencySettings()
      .then(setCurrencySettings)
      .catch(() => setCurrencySettings(FALLBACK_CURRENCY_SETTINGS))
      .finally(() => setCurrencySettingsLoaded(true));
  }, [refreshTick]);

  const notify = useCallback((kind: StatusMessage["kind"], text: string) => {
    setStatus({ kind, text, ts: Date.now() });
  }, []);

  // Bumping refreshTick is this app's substitute for query cache
  // invalidation: route fetch effects depend on it (see the file-level
  // comment above), so calling refresh() after a mutation causes every
  // currently mounted effect that depends on refreshTick to refetch.
  const refresh = useCallback(() => setRefreshTick((t) => t + 1), []);

  const value = useMemo(
    () => ({
      year,
      month,
      years,
      setYear,
      setMonth,
      dateReady,
      status,
      notify,
      refreshTick,
      refresh,
      currencySettings,
      currencySettingsLoaded,
    }),
    [
      year,
      month,
      years,
      dateReady,
      status,
      refreshTick,
      notify,
      refresh,
      currencySettings,
      currencySettingsLoaded,
    ],
  );

  return <AppCtx.Provider value={value}>{children}</AppCtx.Provider>;
}

/** Reads the shared app state (year/month selection, status, refresh signal). Throws if called outside an `AppProvider`. */
export function useApp(): AppContextValue {
  const ctx = useContext(AppCtx);
  if (!ctx) throw new Error("useApp must be used inside AppProvider");
  return ctx;
}
