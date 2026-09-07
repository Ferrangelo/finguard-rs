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

/**
 * Provides the shared app state described in the file-level comment above.
 * Refetches the list of available years whenever `refreshTick` changes, in
 * addition to whatever each route's own effects refetch.
 */
export function AppProvider({ children }: { children: ReactNode }) {
  const now = new Date();
  const [year, setYear] = useState(now.getFullYear());
  const [month, setMonth] = useState(now.getMonth() + 1);
  const [years, setYears] = useState<number[]>([now.getFullYear(), now.getFullYear() - 1]);
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
