import { createFileRoute } from "@tanstack/react-router";
import { useEffect, useRef, useState } from "react";
import { Check, Copy, RefreshCw } from "lucide-react";
import { GlassCard } from "@/components/finguard/GlassCard";
import { ConfirmButton } from "@/components/finguard/ConfirmButton";
import * as api from "@/services/api";
import type {
  SyncCounts,
  SyncDiscoveryReply,
  SyncLast,
  SyncListener,
  SyncNowResult,
  SyncStatus,
} from "@/services/types";

export const Route = createFileRoute("/sync")({
  head: () => ({ meta: [{ title: "Sync · Finguard" }] }),
  component: SyncPage,
});

function ErrorBanner({ message }: { message: string }) {
  return (
    <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
      {message}
    </div>
  );
}

function dateTime(ms: number) {
  return new Date(ms).toLocaleString();
}

function Counts({ counts }: { counts: SyncCounts }) {
  return (
    <div className="grid grid-cols-2 gap-2 text-xs text-muted-foreground sm:grid-cols-4">
      <span>
        Sent: <b className="text-foreground">{counts.sent}</b>
      </span>
      <span>
        Received: <b className="text-foreground">{counts.received}</b>
      </span>
      <span>
        Applied: <b className="text-foreground">{counts.applied}</b>
      </span>
      <span>
        Skipped: <b className="text-foreground">{counts.skipped}</b>
      </span>
      <span>
        Unplaceable: <b className="text-foreground">{counts.unplaceable}</b>
      </span>
      {counts.peer && (
        <>
          <span>
            Peer applied: <b className="text-foreground">{counts.peer.applied}</b>
          </span>
          <span>
            Peer skipped: <b className="text-foreground">{counts.peer.skipped}</b>
          </span>
          <span>
            Peer unplaceable: <b className="text-foreground">{counts.peer.unplaceable}</b>
          </span>
          <span>
            Already known: <b className="text-foreground">{counts.peer.already_known}</b>
          </span>
        </>
      )}
    </div>
  );
}

function LastRound({ last }: { last: SyncLast | null }) {
  if (!last)
    return <p className="text-sm text-muted-foreground">No sync rounds have run since startup.</p>;
  return (
    <div className="space-y-2 text-sm">
      <div className="flex flex-wrap gap-x-4 gap-y-1">
        <span>
          Outcome: <b>{last.outcome}</b>
        </span>
        {last.plan && (
          <span>
            Plan: <b>{last.plan}</b>
          </span>
        )}
        <span className="text-muted-foreground">{dateTime(last.finished_at_ms)}</span>
      </div>
      {last.peer_device_id && (
        <p className="text-xs text-muted-foreground">Peer: {last.peer_device_id}</p>
      )}
      <Counts counts={last.counts} />
      {last.error && <ErrorBanner message={last.error} />}
    </div>
  );
}

function LogHealth({ status }: { status: SyncStatus }) {
  const health = status.log_health;
  return (
    <GlassCard title="Change log health">
      <p className={health.reliable ? "text-sm text-emerald-400" : "text-sm text-destructive"}>
        {health.reliable ? "Reliable" : "Problems detected"}
      </p>
      {health.problems.length > 0 && (
        <ul className="mt-2 list-disc space-y-1 pl-5 text-sm text-muted-foreground">
          {health.problems.map((problem) => (
            <li key={problem}>{problem}</li>
          ))}
        </ul>
      )}
      {health.clock_ahead_hours != null && (
        <p className="mt-2 text-xs text-muted-foreground">
          The newest change is dated {health.clock_ahead_hours} hours ahead of this device clock.
        </p>
      )}
      {health.error && <ErrorBanner message={health.error} />}
    </GlassCard>
  );
}

function ListenerCard({
  listener,
  onIssueCode,
  code,
  expiresAt,
  attempts,
}: {
  listener: SyncListener;
  onIssueCode: () => void;
  code: string | null;
  expiresAt: number | null;
  attempts: number | null;
}) {
  const [copied, setCopied] = useState(false);
  const copy = async () => {
    if (!listener.address_hint) return;
    await navigator.clipboard.writeText(listener.address_hint);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1500);
  };
  return (
    <GlassCard
      title="Desktop listener"
      action={
        <button
          type="button"
          onClick={onIssueCode}
          className="inline-flex items-center gap-1 rounded-md border border-border px-2 py-1 text-xs hover:border-primary/60"
        >
          <RefreshCw className="h-3 w-3" /> Replace code
        </button>
      }
    >
      <div className="space-y-2 text-sm">
        <p className={listener.listening ? "text-emerald-400" : "text-destructive"}>
          {listener.listening ? `Listening on port ${listener.port}` : "Not listening"}
        </p>
        {listener.address_hint && (
          <div className="flex flex-wrap items-center gap-2 text-muted-foreground">
            <span>
              Address guess: <b className="text-foreground">{listener.address_hint}</b>
            </span>
            <button
              type="button"
              onClick={copy}
              aria-label="Copy address"
              className="rounded border border-border p-1"
            >
              {copied ? <Check className="h-3 w-3" /> : <Copy className="h-3 w-3" />}
            </button>
          </div>
        )}
        {listener.bind_error && (
          <ErrorBanner message={`Could not open the sync port: ${listener.bind_error}`} />
        )}
        <p className="text-xs text-muted-foreground">
          The listener stops {listener.stops_after_seconds} seconds after this page closes.
        </p>
        {code && expiresAt && (
          <div className="rounded-lg border border-primary/30 bg-primary/5 p-3">
            <p className="text-xs text-muted-foreground">Pairing code</p>
            <p className="my-1 text-3xl font-bold tracking-[0.35em]">{code}</p>
            <p className="text-xs text-muted-foreground">
              Expires {dateTime(expiresAt)}. Wrong attempts allowed: {attempts ?? "unknown"}.
            </p>
          </div>
        )}
        {!code && expiresAt && (
          <p className="text-sm text-muted-foreground">
            An active code expires {dateTime(expiresAt)}. Its digits are only available on the page
            that issued it.
          </p>
        )}
        {!code && !expiresAt && (
          <p className="text-sm text-muted-foreground">No active pairing code.</p>
        )}
      </div>
    </GlassCard>
  );
}

function SyncPage() {
  const [status, setStatus] = useState<SyncStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [pairCode, setPairCode] = useState<{
    code: string;
    expiresAt: number;
    attempts: number;
  } | null>(null);
  const activeRef = useRef(true);
  const pairCodeRequestedRef = useRef(false);

  const loadStatus = async () => {
    try {
      const next = await api.getSyncStatus();
      if (activeRef.current) {
        setStatus(next);
        setError(null);
      }
    } catch (err) {
      if (activeRef.current)
        setError(err instanceof Error ? err.message : "Could not load sync status.");
    } finally {
      if (activeRef.current) setLoading(false);
    }
  };

  useEffect(() => {
    activeRef.current = true;
    void loadStatus();
    return () => {
      activeRef.current = false;
    };
  }, []);

  useEffect(() => {
    if (status?.role !== "hub") return;
    let heartbeatActive = true;
    const heartbeat = async () => {
      try {
        const listener = await api.syncListen();
        if (heartbeatActive && activeRef.current) {
          setStatus((current) => (current ? { ...current, listener } : current));
          setError(null);
        }
      } catch (err) {
        if (heartbeatActive && activeRef.current)
          setError(err instanceof Error ? err.message : "Could not contact the sync listener.");
      }
    };
    // The backend closes the listener after a grace period, so refresh it well before then.
    void heartbeat();
    const interval = window.setInterval(() => {
      void heartbeat();
    }, 20_000);
    return () => {
      heartbeatActive = false;
      window.clearInterval(interval);
    };
  }, [status?.role]);

  useEffect(() => {
    if (status?.role !== "hub" || !status.listener || pairCodeRequestedRef.current) return;
    const expires = status.listener.pair_code_expires_at_ms;
    if (expires && expires > Date.now()) return;
    pairCodeRequestedRef.current = true;
    void api
      .issueSyncPairCode()
      .then((issued) => {
        if (activeRef.current)
          setPairCode({
            code: issued.code,
            expiresAt: issued.expires_at_ms,
            attempts: issued.attempts_allowed,
          });
      })
      .catch((err) => {
        if (activeRef.current)
          setError(err instanceof Error ? err.message : "Could not issue a pairing code.");
      });
  }, [status?.role, status?.listener]);

  if (loading) return <p className="text-sm text-muted-foreground">Loading sync status...</p>;
  if (error && !status)
    return (
      <div className="space-y-4">
        <PageIntro />
        <ErrorBanner message={error} />
      </div>
    );
  if (!status || (status.role !== "hub" && status.role !== "phone"))
    return (
      <div className="space-y-4">
        <PageIntro />
        <ErrorBanner message="This backend reported an unknown sync role." />
      </div>
    );
  return (
    <div className="space-y-5">
      <PageIntro />
      {error && <ErrorBanner message={error} />}
      {status.role === "hub" ? (
        <HubView
          status={status}
          pairCode={pairCode}
          setPairCode={setPairCode}
          refresh={loadStatus}
        />
      ) : (
        <PhoneView status={status} refresh={loadStatus} />
      )}
    </div>
  );
}

function PageIntro() {
  return (
    <div>
      <h1 className="text-2xl font-bold tracking-tight">Sync</h1>
      <p className="text-sm text-muted-foreground">
        Pair your desktop and phone, then exchange local changes.
      </p>
    </div>
  );
}

function HubView({
  status,
  pairCode,
  setPairCode,
  refresh,
}: {
  status: SyncStatus;
  pairCode: { code: string; expiresAt: number; attempts: number } | null;
  setPairCode: (value: { code: string; expiresAt: number; attempts: number } | null) => void;
  refresh: () => Promise<void>;
}) {
  const [issueError, setIssueError] = useState<string | null>(null);
  const [unpairError, setUnpairError] = useState<string | null>(null);
  const listener = status.listener ?? {
    listening: false,
    port: 3112,
    address_hint: null,
    bind_error: null,
    pair_code_expires_at_ms: null,
    stops_after_seconds: 60,
  };
  const issueCode = async () => {
    try {
      const issued = await api.issueSyncPairCode();
      setPairCode({
        code: issued.code,
        expiresAt: issued.expires_at_ms,
        attempts: issued.attempts_allowed,
      });
      setIssueError(null);
    } catch (err) {
      setIssueError(err instanceof Error ? err.message : "Could not issue a pairing code.");
    }
  };
  const unpair = async (deviceId: string) => {
    try {
      await api.unpairSync(deviceId);
      setUnpairError(null);
      await refresh();
    } catch (err) {
      setUnpairError(err instanceof Error ? err.message : "Could not unpair this phone.");
    }
  };
  return (
    <>
      <div className="grid gap-5 lg:grid-cols-2">
        <ListenerCard
          listener={listener}
          onIssueCode={() => void issueCode()}
          code={pairCode?.code ?? null}
          expiresAt={pairCode?.expiresAt ?? listener.pair_code_expires_at_ms}
          attempts={pairCode?.attempts ?? null}
        />
        {issueError && <ErrorBanner message={issueError} />}
        <GlassCard title="This desktop">
          <p className="text-sm text-muted-foreground">
            Device ID: <b className="text-foreground">{status.device_id}</b>
          </p>
          <p className="mt-2 text-sm text-muted-foreground">
            Key fingerprint: <b className="text-foreground">{status.key_fingerprint}</b>
          </p>
          <p className="mt-3 text-xs text-muted-foreground">
            Phones should compare their returned fingerprint with this value.
          </p>
          <p className="mt-2 text-xs text-muted-foreground">
            Discoverable while this page is open.
          </p>
        </GlassCard>
      </div>
      <GlassCard title={`Paired phones (${status.peers.length})`}>
        {status.peers.length === 0 ? (
          <p className="text-sm text-muted-foreground">No phones are paired.</p>
        ) : (
          <div className="space-y-3">
            {status.peers.map((peer) => (
              <div
                key={peer.device_id}
                className="flex flex-wrap items-center justify-between gap-3 border-b border-border/40 pb-3 last:border-0 last:pb-0"
              >
                <div className="text-sm">
                  <p className="font-medium">{peer.device_id}</p>
                  <p className="text-xs text-muted-foreground">
                    Fingerprint: {peer.key_fingerprint ?? "not available"}
                  </p>
                  <p className="text-xs text-muted-foreground">
                    Paired: {dateTime(peer.paired_at_ms)}
                  </p>
                </div>
                <ConfirmButton
                  label="Unpair"
                  confirmLabel="Confirm unpair"
                  onConfirm={() => void unpair(peer.device_id)}
                />
              </div>
            ))}
          </div>
        )}
        {unpairError && <ErrorBanner message={unpairError} />}
        {status.peers_error && <ErrorBanner message={status.peers_error} />}
      </GlassCard>
      <LogHealth status={status} />
      <GlassCard title="Last round">
        <LastRound last={status.last_sync} />
      </GlassCard>
    </>
  );
}

function PhoneView({ status, refresh }: { status: SyncStatus; refresh: () => Promise<void> }) {
  const hub = status.peers.find((peer) => peer.role === "hub");
  const [address, setAddress] = useState(hub?.address ?? "");
  const [code, setCode] = useState("");
  const [pairedFingerprint, setPairedFingerprint] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [round, setRound] = useState<SyncNowResult | null>(null);
  const [discoveries, setDiscoveries] = useState<SyncDiscoveryReply[]>([]);
  const [discovering, setDiscovering] = useState(false);
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);

  const discover = async () => {
    setDiscovering(true);
    setDiscoveryError(null);
    try {
      setDiscoveries(await api.discoverSync());
    } catch (err) {
      setDiscoveryError(err instanceof Error ? err.message : "Could not search for desktops.");
    } finally {
      setDiscovering(false);
    }
  };

  useEffect(() => {
    if (hub) return;
    let active = true;
    setDiscovering(true);
    setDiscoveryError(null);
    void api
      .discoverSync()
      .then((found) => {
        if (active) setDiscoveries(found);
      })
      .catch((err) => {
        if (active)
          setDiscoveryError(err instanceof Error ? err.message : "Could not search for desktops.");
      })
      .finally(() => {
        if (active) setDiscovering(false);
      });
    return () => {
      active = false;
    };
  }, [hub]);

  const pair = async () => {
    const trimmedAddress = address.trim();
    const trimmedCode = code.trim();
    if (!trimmedAddress || !trimmedCode) return;
    setBusy(true);
    setError(null);
    try {
      const result = await api.pairSync(trimmedAddress, trimmedCode);
      setPairedFingerprint(result.hub_key_fingerprint);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Pairing failed.");
    } finally {
      setBusy(false);
    }
  };
  const runSync = async (confirmReset = false) => {
    setBusy(true);
    setError(null);
    try {
      const result = await api.syncNow(confirmReset);
      setRound(result);
      if (
        result.outcome === "reset_needed" &&
        result.reset_preview &&
        canAutoConfirm(result.reset_preview)
      ) {
        // Only an empty preview is safe to confirm without asking.
        const confirmed = await api.syncNow(true);
        setRound(confirmed);
      }
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Sync failed.");
    } finally {
      setBusy(false);
    }
  };
  const unpair = async () => {
    if (!hub) return;
    try {
      await api.unpairSync(hub.device_id);
      setPairedFingerprint(null);
      setRound(null);
      await refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : "Could not unpair this desktop.");
    }
  };

  return (
    <>
      {status.peers_error && <ErrorBanner message={status.peers_error} />}
      {!hub ? (
        <GlassCard title="Pair with your desktop">
          <div className="space-y-3">
            <p className="text-sm text-muted-foreground">
              Enter the desktop's address and the six-digit code shown on its Sync page.
            </p>
            <div className="rounded-md border border-border/60 bg-surface/30 p-3 text-sm">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <span className="text-muted-foreground">
                  {discovering ? "Searching for desktops..." : "Nearby desktops"}
                </span>
                <button
                  type="button"
                  disabled={discovering}
                  onClick={() => void discover()}
                  className="inline-flex items-center gap-1 rounded-md border border-border px-2 py-1 text-xs hover:border-primary/60 disabled:opacity-50"
                >
                  <RefreshCw className="h-3 w-3" /> {discovering ? "Searching" : "Retry"}
                </button>
              </div>
              {discoveries.length > 0 ? (
                <div className="mt-2 space-y-2">
                  {discoveries.map((found) => (
                    <button
                      type="button"
                      key={`${found.address}-${found.device_id}`}
                      onClick={() => setAddress(found.address)}
                      className="block w-full rounded border border-border/60 p-2 text-left hover:border-primary/60"
                    >
                      <span className="block font-medium">{found.address}</span>
                      <span className="block text-xs text-muted-foreground">
                        {found.device_id} · Fingerprint: {found.key_fingerprint}
                      </span>
                    </button>
                  ))}
                </div>
              ) : (
                !discovering &&
                !discoveryError && (
                  <p className="mt-2 text-xs text-muted-foreground">
                    No desktop found. Use the address box below, and make sure the desktop's Sync
                    page is open.
                  </p>
                )
              )}
              {discoveryError && (
                <div className="mt-2">
                  <ErrorBanner message={discoveryError} />
                </div>
              )}
            </div>
            <div className="flex flex-col gap-2 sm:flex-row">
              <input
                value={address}
                onChange={(event) => setAddress(event.target.value)}
                onKeyDown={(event) => event.key === "Enter" && !busy && void pair()}
                placeholder="host or host:port"
                className="flex-1 rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm focus:border-primary/60 focus:outline-none"
              />
              <input
                value={code}
                onChange={(event) => setCode(event.target.value.replace(/\D/g, "").slice(0, 6))}
                onKeyDown={(event) => event.key === "Enter" && !busy && void pair()}
                inputMode="numeric"
                placeholder="6-digit code"
                className="w-full rounded-md border border-border bg-surface/60 px-2.5 py-1.5 text-sm sm:w-36"
              />
              <button
                type="button"
                disabled={busy}
                onClick={() => void pair()}
                className="rounded-md bg-gradient-brand px-4 py-1.5 text-sm font-semibold text-background disabled:opacity-50"
              >
                Pair
              </button>
            </div>
            {error && <ErrorBanner message={error} />}
          </div>
        </GlassCard>
      ) : (
        <>
          <GlassCard
            title="Paired desktop"
            action={
              <ConfirmButton
                label="Unpair"
                confirmLabel="Confirm unpair"
                onConfirm={() => void unpair()}
              />
            }
          >
            <p className="text-sm text-muted-foreground">
              Address: <b className="text-foreground">{hub.address ?? address}</b>
            </p>
            <p className="mt-2 text-sm text-muted-foreground">
              Hub fingerprint:{" "}
              <b className="text-foreground">
                {pairedFingerprint ?? hub.key_fingerprint ?? "not available"}
              </b>
            </p>
            <p className="mt-2 text-xs text-muted-foreground">
              Compare this fingerprint with the one shown on the desktop's Sync page.
            </p>
          </GlassCard>
          <GlassCard
            title="Run sync"
            action={
              <button
                type="button"
                disabled={busy}
                onClick={() => void runSync()}
                className="inline-flex items-center gap-1 rounded-md bg-gradient-brand px-3 py-1.5 text-sm font-semibold text-background disabled:opacity-50"
              >
                <RefreshCw className="h-4 w-4" /> Sync now
              </button>
            }
          >
            {error && <ErrorBanner message={error} />}
            {round ? (
              <RoundResult result={round} onConfirm={() => void runSync(true)} />
            ) : (
              <p className="text-sm text-muted-foreground">No round has run on this page.</p>
            )}
          </GlassCard>
        </>
      )}
      <LogHealth status={status} />
      <GlassCard title="Last round">
        <LastRound last={status.last_sync} />
      </GlassCard>
    </>
  );
}

function canAutoConfirm(preview: NonNullable<SyncNowResult["reset_preview"]>) {
  return (
    Object.values(preview.rows_per_table).every((count) => count === 0) &&
    preview.unsent_entries === 0 &&
    preview.unreadable_files === 0
  );
}

function RoundResult({ result, onConfirm }: { result: SyncNowResult; onConfirm: () => void }) {
  return (
    <div className="space-y-3 text-sm">
      <p>
        Outcome: <b>{result.outcome}</b>. Plan: <b>{result.plan}</b>.
      </p>
      <Counts counts={result.counts} />
      {result.push_first && (
        <p className="text-xs text-muted-foreground">
          The phone's changes will be pushed to the desktop before the reset.
        </p>
      )}
      {result.reset_preview && result.outcome === "reset_needed" && (
        <div className="rounded-md border border-destructive/40 bg-destructive/10 p-3">
          <p className="font-medium text-destructive">This round needs a reset.</p>
          <p className="mt-1 text-xs text-muted-foreground">
            Rows to replace:{" "}
            {Object.entries(result.reset_preview.rows_per_table)
              .map(([table, count]) => `${table} (${count})`)
              .join(", ") || "none"}
            . Year folders: {result.reset_preview.year_folders}. Unreadable files:{" "}
            {result.reset_preview.unreadable_files}. Unsent entries:{" "}
            {result.reset_preview.unsent_entries}.
          </p>
          <div className="mt-3">
            <ConfirmButton
              label="Confirm reset"
              confirmLabel="Confirm reset now"
              onConfirm={onConfirm}
            />
          </div>
        </div>
      )}
      {result.backup_folder && (
        <p className="text-xs text-muted-foreground">
          Pre-reset data backup: {result.backup_folder}
        </p>
      )}
    </div>
  );
}
