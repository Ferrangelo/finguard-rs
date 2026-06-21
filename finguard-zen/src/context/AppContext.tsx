import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import * as api from "@/services/api";
import type { StatusMessage } from "@/services/types";

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
}

const AppCtx = createContext<AppContextValue | null>(null);

export function AppProvider({ children }: { children: ReactNode }) {
  const now = new Date();
  const [year, setYear] = useState(now.getFullYear());
  const [month, setMonth] = useState(now.getMonth() + 1);
  const [years, setYears] = useState<number[]>([now.getFullYear(), now.getFullYear() - 1]);
  const [status, setStatus] = useState<StatusMessage>({ kind: "idle", text: "Ready", ts: Date.now() });
  const [refreshTick, setRefreshTick] = useState(0);

  useEffect(() => {
    api.listYears().then(setYears).catch(() => {});
  }, [refreshTick]);

  const notify = useCallback((kind: StatusMessage["kind"], text: string) => {
    setStatus({ kind, text, ts: Date.now() });
  }, []);

  const refresh = useCallback(() => setRefreshTick((t) => t + 1), []);

  const value = useMemo(
    () => ({ year, month, years, setYear, setMonth, status, notify, refreshTick, refresh }),
    [year, month, years, status, refreshTick, notify, refresh],
  );

  return <AppCtx.Provider value={value}>{children}</AppCtx.Provider>;
}

export function useApp(): AppContextValue {
  const ctx = useContext(AppCtx);
  if (!ctx) throw new Error("useApp must be used inside AppProvider");
  return ctx;
}