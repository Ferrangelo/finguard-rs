//! Currency exchange rates, sourced from Frankfurter (a free, keyless API over
//! European Central Bank reference rates), cached on disk, and resolved into
//! the user's configured reference currency.
//!
//! The cache file lives at `$XDG_DATA_HOME/finguard/fx_rates.json` (see
//! [`crate::paths::get_fx_rates_path`]):
//!
//! ```text
//! { "base": "EUR", "rates": { "2026-09-04": { "USD": 1.0812, "GBP": 0.8365 } } }
//! ```
//!
//! The cache also carries `aliases` (a requested weekend/holiday date mapped
//! to the earlier date Frankfurter actually published) and `latest_date` /
//! `latest_fetched_at` (bookkeeping for the last `/latest` fetch), both
//! omitted above since a file written before they existed loads the same as
//! one with them empty.
//!
//! Rates are always cached EUR-based, exactly as the ECB publishes them,
//! regardless of the user's reference currency (see [`crate::config::CurrencySettings`]).
//! A non-EUR reference currency is handled by computing a cross rate from two
//! EUR-based quotes at lookup time.
//!
//! The ECB publishes on working days only, so Frankfurter's `date` field is
//! often earlier than the date requested. Every resolver here reports the
//! date its rate actually came from, and callers must not assume it equals
//! the date they asked for.
//!
//! Setting the `FINGUARD_FX_OFFLINE` environment variable (to any value)
//! disables every network call; resolvers then work purely from the cache.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use chrono::{Datelike, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::{Error, Result};
use crate::paths;

const FRANKFURTER_BASE_URL: &str = "https://api.frankfurter.app";

/// How long a single Frankfurter request may run before it is treated as
/// failed. The offline fallbacks ([`nearest_earlier`], [`newest_cached`])
/// only run on `Err`, so a request that hangs instead of erroring bypasses
/// them and parks the handling task indefinitely; a bounded timeout is what
/// turns a hang into the `Err` those fallbacks are waiting for.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long establishing the TCP/TLS connection alone may take, as a tighter
/// bound than [`REQUEST_TIMEOUT`] for the case where the network never even
/// reaches Frankfurter (e.g. a black-holed route or a captive portal).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a successful `/latest` fetch stays fresh enough to answer a
/// repeat [`resolve_latest_eur_rates`] call from the cache instead of the
/// network. `monthly_rates_lenient` resolves one currency at a time, so one
/// live-mode chart request already makes one `/latest` round trip per
/// currency in the portfolio; this threshold keeps a second request moments
/// later, or the next chart re-render, from repeating every one of them. The
/// ECB publishes roughly once per working day, so an hour is conservative
/// against the publication schedule while still bounding how long a rate
/// that changed can stay stale.
const LATEST_STALENESS_THRESHOLD_SECS: i64 = 60 * 60;

/// The first date Frankfurter's ECB reference series publishes a rate for.
///
/// An exact single-date fetch for a date before this returns 404, and a
/// range fetch spanning back before it silently truncates its results to
/// this date rather than erroring. A `NaiveDate` outside the range the
/// backend's own dates can hold (for example a null or zero expense date
/// read back as the Unix epoch day, 1970-01-01) is always before this
/// constant, so every lookup for it would otherwise pay one failing
/// round trip per call with no way to cache the failure.
const FX_SERIES_START: NaiveDate = match NaiveDate::from_ymd_opt(1999, 1, 4) {
    Some(date) => date,
    None => unreachable!(),
};

/// The shared client for every Frankfurter request, built once and reused so
/// repeated rate lookups do not each pay for a fresh connection pool the way
/// `reqwest::get`'s implicit default client would.
///
/// `ClientBuilder::build` does environment-dependent work beyond applying the
/// timeouts set here (DNS resolver setup, `HTTP_PROXY`/`HTTPS_PROXY`/`NO_PROXY`
/// parsing, and, for the rustls backend, loading the native root certificate
/// store), and it is fallible on purpose. The outcome is cached in the
/// `OnceLock` either way, so a broken environment fails the same way on every
/// call instead of retrying the environment-dependent work each time.
fn http_client() -> Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<std::result::Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                // `reqwest::Error`'s `Display` prints only the error kind
                // ("builder error") and drops the cause, which cannot tell a
                // missing root certificate store from a malformed
                // `HTTP_PROXY`. `Debug` keeps the whole chain, and this
                // string is the only record left once the `OnceLock` is set.
                .map_err(|e| format!("{e:?}"))
        })
        .as_ref()
        .map_err(|e| Error::Network(format!("failed to build the Frankfurter HTTP client: {e}")))
}

/// The rate needed to convert one unit of the requested currency into the
/// reference currency, and the date whose published rate satisfied the
/// lookup.
///
/// `rate_date` is `None` only for the trivial identity case, where the
/// requested currency already *is* the reference currency: no lookup ever
/// happens, so no external date applies.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedRate {
    /// Multiply an amount in the requested currency by this to get an amount
    /// in the reference currency.
    pub rate: f64,
    /// The date the underlying EUR-based rate was published for, or `None`
    /// for the reference-currency identity case.
    pub rate_date: Option<NaiveDate>,
}

/// The on-disk shape of the rate cache. Always EUR-based, regardless of the
/// user's reference currency.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RateCache {
    base: String,
    #[serde(default)]
    rates: BTreeMap<String, BTreeMap<String, f64>>,
    /// Maps a requested date Frankfurter had no rate for (a weekend or
    /// holiday) to the earlier date it returned instead, whose rates already
    /// live in `rates`. Populated only for a requested date at least
    /// [`ALIAS_MIN_AGE_DAYS`] before today, by [`store_historical_rates`]; see
    /// [`should_alias_as`] for why that window exists. Absent in a cache file
    /// written before this field existed, which `#[serde(default)]` treats as
    /// "no aliases known yet" rather than an error.
    #[serde(default)]
    aliases: BTreeMap<String, String>,
    /// The key in `rates` that the most recent successful `/latest` fetch
    /// returned, paired with `latest_fetched_at`. `None` in a cache file
    /// written before this field existed.
    #[serde(default)]
    latest_date: Option<String>,
    /// Unix timestamp (seconds, UTC) of the last successful `/latest` fetch;
    /// see [`LATEST_STALENESS_THRESHOLD_SECS`]. `None` is always treated as
    /// stale, which is also what a cache file written before this field
    /// existed deserializes to.
    #[serde(default)]
    latest_fetched_at: Option<i64>,
}

impl Default for RateCache {
    fn default() -> Self {
        Self {
            base: "EUR".to_string(),
            rates: BTreeMap::new(),
            aliases: BTreeMap::new(),
            latest_date: None,
            latest_fetched_at: None,
        }
    }
}

/// Frankfurter's response shape for both `/{date}` and `/latest`.
#[derive(Debug, Deserialize)]
struct FrankfurterResponse {
    date: String,
    rates: BTreeMap<String, f64>,
}

/// Frankfurter's response shape for `/{start}..{end}` range requests: one
/// EUR-based table per published day, keyed by date. Weekends and holidays
/// have no entry; only working days appear. The sibling `amount`, `base`,
/// `start_date` and `end_date` fields carry no rates, so they are not mapped
/// here (serde ignores them).
#[derive(Debug, Deserialize)]
struct FrankfurterRangeResponse {
    rates: BTreeMap<String, BTreeMap<String, f64>>,
}

/// Load the rate cache from disk. Returns an empty EUR-based cache if the
/// file does not exist yet.
///
/// Delegates to [`load_snapshot`] and clones, so every caller shares one
/// memoized parse without any change to its own error behavior.
fn load_cache() -> Result<RateCache> {
    Ok((*load_snapshot()?).clone())
}

/// Memoized file identity plus its parsed snapshot; see [`FX_SNAPSHOT_MEMO`].
type SnapshotMemo = Option<(PathBuf, SystemTime, u64, Arc<RateCache>)>;

/// Process-wide memo of the last parsed FX cache file, gated on file
/// identity `(path, mtime, len)`. Any save rewrites the file, changing mtime
/// or length, so a hit cannot serve rates from before the latest save.
///
/// The critical sections only compare the key and clone one `Arc`; the lock
/// is never held across `await` and never meets [`cache_write_lock`], so it
/// cannot deadlock against the locked write paths.
static FX_SNAPSHOT_MEMO: RwLock<SnapshotMemo> = RwLock::new(None);

/// Counts actual cache-file parses in test builds, so a test can prove
/// repeat loads share one parse.
#[cfg(test)]
static PARSE_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Load the rate cache snapshot, parsing only when the file identity changed
/// since the last parse. A missing file returns the default exactly as
/// [`load_cache`] always has, without touching the memo; an unreadable file
/// or unparsable body errors without touching it, so the next lookup retries.
///
/// Metadata/read race: the file may change between `metadata` and
/// `read_to_string`. A mismatched pair either misses the memo (one extra
/// parse) or caches content under the pre-read key; the next lookup then
/// mismatches and reparses, so the staleness heals on the following call. A
/// batch already freezes whatever snapshot it loaded, so either outcome
/// preserves batch semantics.
fn load_snapshot() -> Result<Arc<RateCache>> {
    let path = paths::get_fx_rates_path()?;
    if !path.exists() {
        return Ok(Arc::new(RateCache::default()));
    }
    // Metadata failures fall back to a direct parse with no memo involvement,
    // mirroring `load_cache`'s error behavior rather than inventing a key.
    let (mtime, len) = match std::fs::metadata(&path)
        .ok()
        .and_then(|meta| meta.modified().ok().map(|mtime| (mtime, meta.len())))
    {
        Some(key) => key,
        None => {
            let contents = std::fs::read_to_string(&path)?;
            let cache: RateCache = serde_json::from_str(&contents)?;
            #[cfg(test)]
            PARSE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return Ok(Arc::new(cache));
        }
    };
    {
        let guard = FX_SNAPSHOT_MEMO
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached_path, cached_mtime, cached_len, cached)) = guard.as_ref()
            && *cached_path == path
            && *cached_mtime == mtime
            && *cached_len == len
        {
            return Ok(cached.clone());
        }
    }
    let contents = std::fs::read_to_string(&path)?;
    let cache: RateCache = serde_json::from_str(&contents)?;
    #[cfg(test)]
    PARSE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let snapshot = Arc::new(cache);
    {
        let mut guard = FX_SNAPSHOT_MEMO
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some((path, mtime, len, snapshot.clone()));
    }
    Ok(snapshot)
}

/// Persist the rate cache to disk (pretty-printed, matching the config files).
fn save_cache(cache: &RateCache) -> Result<()> {
    config::write_json(&paths::get_fx_rates_path()?, cache)
}

/// The process-wide lock serializing every on-disk cache load/modify/save
/// cycle (see [`store_historical_rates`] and [`store_latest_rates`]), so two
/// resolvers saving different dates concurrently merge instead of one
/// overwriting the other's snapshot.
///
/// Only ever held around that cycle, never around a network call: a
/// Frankfurter request can run for up to [`REQUEST_TIMEOUT`] before it
/// resolves, and holding this lock across it would block every other rate
/// lookup in the process for that long. Built lazily the same way as
/// [`http_client`], since a `tokio::sync::Mutex` has no `const` constructor.
fn cache_write_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// `true` when `FINGUARD_FX_OFFLINE` is set, meaning no network call may be
/// attempted and resolvers must work purely from the cache.
fn is_offline() -> bool {
    std::env::var_os("FINGUARD_FX_OFFLINE").is_some()
}

fn format_date(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

fn parse_date(date_str: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(date_str, "%Y-%m-%d").map_err(|e| {
        Error::Network(format!(
            "cached FX rate key '{date_str}' is not a valid date: {e}"
        ))
    })
}

/// Fetch and parse a Frankfurter response, mapping any failure to
/// [`Error::Network`] so callers can distinguish it from a lookup miss.
///
/// `kind` (`"single"` or `"latest"`) labels the diag line emitted on
/// failure. Success stays silent; the batch summary covers it. The URL
/// itself is never logged because it embeds dates.
async fn fetch_rates(url: &str, kind: &str, op: u64) -> Result<(NaiveDate, BTreeMap<String, f64>)> {
    let start = std::time::Instant::now();
    let fail = |e: &reqwest::Error| {
        crate::diag::event(
            op,
            "fx",
            format!(
                "fetch {kind} FAIL {} in {}ms",
                classify_fetch_error(e),
                start.elapsed().as_millis()
            ),
        );
    };
    let response = http_client()?
        .get(url)
        .send()
        .await
        .map_err(|e| {
            fail(&e);
            Error::Network(format!("request to {url} failed: {e}"))
        })?
        .error_for_status()
        .map_err(|e| {
            fail(&e);
            Error::Network(format!("Frankfurter returned an error for {url}: {e}"))
        })?;
    let body: FrankfurterResponse = response.json().await.map_err(|e| {
        fail(&e);
        Error::Network(format!(
            "Frankfurter response from {url} was not the expected shape: {e}"
        ))
    })?;
    let date = parse_date(&body.date)?;
    Ok((date, body.rates))
}

/// Classify a Frankfurter request failure for the diag log: timeouts,
/// refused connections, and HTTP statuses (notably 429 rate limits) point
/// at very different fixes, and none of this leaks dates or amounts.
fn classify_fetch_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        "connect".to_string()
    } else if e.is_decode() {
        "decode".to_string()
    } else if let Some(status) = e.status() {
        format!("http_{}", status.as_u16())
    } else if e.is_builder() {
        "client".to_string()
    } else {
        "other".to_string()
    }
}

/// Fetch the EUR-based rates Frankfurter published on or before `date`. The
/// returned date is frequently earlier than requested, since the ECB does not
/// publish on weekends or holidays.
async fn fetch_historical(date: NaiveDate, op: u64) -> Result<(NaiveDate, BTreeMap<String, f64>)> {
    let url = format!("{FRANKFURTER_BASE_URL}/{}?base=EUR", format_date(date));
    fetch_rates(&url, "single", op).await
}

/// Parse a Frankfurter `{start}..{end}` range body into one EUR-based table
/// per published day.
///
/// Pure so tests cover it without the network; [`fetch_range`] is the only
/// caller that fetches the body it parses.
fn parse_range_response(body: &str) -> Result<BTreeMap<NaiveDate, BTreeMap<String, f64>>> {
    let parsed: FrankfurterRangeResponse = serde_json::from_str(body).map_err(|e| {
        Error::Network(format!(
            "Frankfurter range response was not the expected shape: {e}"
        ))
    })?;
    let mut days = BTreeMap::new();
    for (date_str, rates) in parsed.rates {
        days.insert(parse_date(&date_str)?, rates);
    }
    Ok(days)
}

/// Fetch every EUR-based rate table Frankfurter published in `min..=max`
/// with a single range request, keyed by the day each table was published
/// for. A weekend or holiday inside the span simply has no entry.
///
/// Logs one line either way: the requested span length in days (a count, not
/// dates) and the merged table count on success, the classified cause on
/// failure. The URL itself is never logged because it embeds dates.
async fn fetch_range(
    min: NaiveDate,
    max: NaiveDate,
    op: u64,
) -> Result<BTreeMap<NaiveDate, BTreeMap<String, f64>>> {
    let start = std::time::Instant::now();
    let span_days = max.signed_duration_since(min).num_days();
    let url = format!(
        "{FRANKFURTER_BASE_URL}/{}..{}?base=EUR",
        format_date(min),
        format_date(max)
    );
    let fail = |cause: &str| {
        crate::diag::event(
            op,
            "fx",
            format!(
                "fetch range FAIL {cause} span {span_days}d in {}ms",
                start.elapsed().as_millis()
            ),
        );
    };
    let body = http_client()?
        .get(&url)
        .send()
        .await
        .map_err(|e| {
            fail(&classify_fetch_error(&e));
            Error::Network(format!("request to {url} failed: {e}"))
        })?
        .error_for_status()
        .map_err(|e| {
            fail(&classify_fetch_error(&e));
            Error::Network(format!("Frankfurter returned an error for {url}: {e}"))
        })?
        .text()
        .await
        .map_err(|e| {
            fail(&classify_fetch_error(&e));
            Error::Network(format!(
                "Frankfurter response from {url} was not readable: {e}"
            ))
        })?;
    let days = parse_range_response(&body).inspect_err(|_| fail("decode"))?;
    crate::diag::event(
        op,
        "fx",
        format!(
            "fetch range ok span {span_days}d tables {} in {}ms",
            days.len(),
            start.elapsed().as_millis()
        ),
    );
    Ok(days)
}

/// Fetch the newest EUR-based rates Frankfurter has published.
async fn fetch_latest() -> Result<(NaiveDate, BTreeMap<String, f64>)> {
    fetch_rates(
        &format!("{FRANKFURTER_BASE_URL}/latest?base=EUR"),
        "latest",
        0,
    )
    .await
}

/// Return the largest cached date strictly before `date_str`, if any.
fn nearest_earlier(
    cache: &RateCache,
    date_str: &str,
) -> Result<Option<(NaiveDate, BTreeMap<String, f64>)>> {
    // `BTreeMap<String, _>::range` cannot be bounded by a bare `&str` (the
    // `RangeTo<&T>` impl of `RangeBounds<T>` requires `T: Sized`), so bound it
    // with an owned `String` instead; this map is small enough that the
    // allocation is not worth avoiding.
    match cache.rates.range(..date_str.to_string()).next_back() {
        Some((found_str, rates)) => Ok(Some((parse_date(found_str)?, rates.clone()))),
        None => Ok(None),
    }
}

/// Return the most recently cached date, if any.
fn newest_cached(cache: &RateCache) -> Result<Option<(NaiveDate, BTreeMap<String, f64>)>> {
    match cache.rates.iter().next_back() {
        Some((found_str, rates)) => Ok(Some((parse_date(found_str)?, rates.clone()))),
        None => Ok(None),
    }
}

/// Look up `date` in `cache` without any network access: an exact key match
/// first, then a recorded alias to an earlier published date (see
/// [`resolve_eur_rates_on`] and the `aliases` field of [`RateCache`]).
///
/// The returned date is always the date the rate was actually published for.
/// An aliased hit reports the alias's target, never `date` itself, so a
/// cached weekend or holiday lookup is never mistaken for a publication date.
fn cached_rate_for(
    cache: &RateCache,
    date: NaiveDate,
) -> Result<Option<(BTreeMap<String, f64>, NaiveDate)>> {
    let date_str = format_date(date);
    if let Some(rates) = cache.rates.get(&date_str) {
        return Ok(Some((rates.clone(), date)));
    }
    if let Some(published_str) = cache.aliases.get(&date_str)
        && let Some(rates) = cache.rates.get(published_str)
    {
        return Ok(Some((rates.clone(), parse_date(published_str)?)));
    }
    Ok(None)
}

/// The minimum age, in days, `requested` must have in [`should_alias_as`]
/// before a Frankfurter substitution for it is trusted as a permanent alias.
///
/// A substitution is ambiguous on its own: it can mean `requested` is a
/// weekend or holiday, whose nearest earlier publication never changes, or it
/// can mean the ECB simply has not published that business day's own rate
/// yet, which is temporary and must not be frozen. Requiring `requested` to
/// be at least this many days old resolves the ambiguity in favor of "no
/// rate was ever coming": seven days is comfortably beyond any plausible
/// publication delay, while expense data is dominated by dates well in the
/// past, so the cost is only that a weekend inside the last week re-fetches
/// on every repeat lookup instead of hitting the alias, which is rare and
/// cheap.
const ALIAS_MIN_AGE_DAYS: i64 = 7;

/// Whether a successful fetch for `requested` should also be cached as an
/// alias of `returned` (see [`resolve_eur_rates_on`]).
///
/// `true` only when Frankfurter actually substituted an earlier date *and*
/// `requested` is at least [`ALIAS_MIN_AGE_DAYS`] before `today`; see that
/// constant for why the window exists. Today itself is always younger than
/// the window, so it can never be frozen by an alias once more rates publish
/// later in the day.
fn should_alias_as(requested: NaiveDate, returned: NaiveDate, today: NaiveDate) -> bool {
    returned != requested && today.signed_duration_since(requested).num_days() >= ALIAS_MIN_AGE_DAYS
}

/// Merge a freshly fetched historical rate table into the on-disk cache,
/// serialized against concurrent callers by [`cache_write_lock`] so two
/// resolvers saving different dates cannot clobber each other's entry.
/// Reloads the cache under the lock, right before writing, rather than
/// reusing an earlier snapshot, so a concurrent writer's save is never lost.
///
/// `alias_for`, when given, additionally records it as an alias of
/// `returned_date`; see [`should_alias_as`] for when a caller should pass one.
async fn store_historical_rates(
    returned_date: NaiveDate,
    rates: &BTreeMap<String, f64>,
    alias_for: Option<NaiveDate>,
) -> Result<()> {
    let _guard = cache_write_lock().lock().await;
    let mut cache = load_cache()?;
    let returned_date_str = format_date(returned_date);
    cache.rates.insert(returned_date_str.clone(), rates.clone());
    if let Some(requested_date) = alias_for {
        cache
            .aliases
            .insert(format_date(requested_date), returned_date_str);
    }
    save_cache(&cache)
}

/// Resolve the EUR-based rate table published on or before `date`: an exact
/// or aliased cache hit first, then the network (unless offline), then the
/// nearest earlier cached date as a fallback, then a hard error.
async fn resolve_eur_rates_on(date: NaiveDate) -> Result<(BTreeMap<String, f64>, NaiveDate)> {
    let date_str = format_date(date);
    let cache = load_cache()?;

    if let Some((rates, actual_date)) = cached_rate_for(&cache, date)? {
        return Ok((rates, actual_date));
    }

    if !is_offline() {
        match fetch_historical(date, 0).await {
            Ok((returned_date, rates)) => {
                let today = Local::now().date_naive();
                let alias_for = should_alias_as(date, returned_date, today).then_some(date);
                store_historical_rates(returned_date, &rates, alias_for).await?;
                return Ok((rates, returned_date));
            }
            Err(network_err) => {
                return match nearest_earlier(&cache, &date_str)? {
                    Some((found_date, rates)) => Ok((rates, found_date)),
                    None => Err(network_err),
                };
            }
        }
    }

    match nearest_earlier(&cache, &date_str)? {
        Some((found_date, rates)) => Ok((rates, found_date)),
        None => Err(Error::NotFound(format!(
            "no cached FX rate on or before {date_str}, and FINGUARD_FX_OFFLINE disables the \
             network"
        ))),
    }
}

/// Return the cached `/latest` result if it is still within
/// [`LATEST_STALENESS_THRESHOLD_SECS`] of when it was fetched, without any
/// network access.
///
/// Treats a cache file written before `latest_date`/`latest_fetched_at`
/// existed (both `None`) as stale, so an old cache file never errors here and
/// never serves a wrong rate; it simply falls through to the normal
/// network/fallback path exactly as it did before this staleness check
/// existed. Also treats a `latest_fetched_at` in the future as stale, which
/// can only happen from clock skew or a corrupted cache, rather than treating
/// it as arbitrarily fresh.
fn fresh_latest_from_cache(
    cache: &RateCache,
) -> Result<Option<(BTreeMap<String, f64>, NaiveDate)>> {
    let (Some(fetched_at), Some(date_str)) = (cache.latest_fetched_at, cache.latest_date.as_ref())
    else {
        return Ok(None);
    };
    let age_secs = Utc::now().timestamp() - fetched_at;
    if !(0..LATEST_STALENESS_THRESHOLD_SECS).contains(&age_secs) {
        return Ok(None);
    }
    match cache.rates.get(date_str) {
        Some(rates) => Ok(Some((rates.clone(), parse_date(date_str)?))),
        None => Ok(None),
    }
}

/// Merge a freshly fetched `/latest` rate table into the on-disk cache and
/// record it as the newest known "latest" fetch, serialized the same way as
/// [`store_historical_rates`] (see that function for why).
async fn store_latest_rates(returned_date: NaiveDate, rates: &BTreeMap<String, f64>) -> Result<()> {
    let _guard = cache_write_lock().lock().await;
    let mut cache = load_cache()?;
    let date_str = format_date(returned_date);
    cache.rates.insert(date_str.clone(), rates.clone());
    cache.latest_date = Some(date_str);
    cache.latest_fetched_at = Some(Utc::now().timestamp());
    save_cache(&cache)
}

/// Resolve the newest EUR-based rate table: a fresh-enough cached `/latest`
/// result first (see [`fresh_latest_from_cache`]), then the network (unless
/// offline, since only the network can say what is actually newest), then the
/// most recently cached date as a fallback, then a hard error.
async fn resolve_latest_eur_rates() -> Result<(BTreeMap<String, f64>, NaiveDate)> {
    let cache = load_cache()?;

    if let Some(fresh) = fresh_latest_from_cache(&cache)? {
        return Ok(fresh);
    }

    if !is_offline() {
        match fetch_latest().await {
            Ok((returned_date, rates)) => {
                store_latest_rates(returned_date, &rates).await?;
                return Ok((rates, returned_date));
            }
            Err(network_err) => {
                return match newest_cached(&cache)? {
                    Some((found_date, rates)) => Ok((rates, found_date)),
                    None => Err(network_err),
                };
            }
        }
    }

    match newest_cached(&cache)? {
        Some((found_date, rates)) => Ok((rates, found_date)),
        None => Err(Error::NotFound(
            "no cached FX rate is available, and FINGUARD_FX_OFFLINE disables the network"
                .to_string(),
        )),
    }
}

/// Look up an EUR-based quote for `currency` (EUR itself is implicitly 1.0,
/// since Frankfurter's base currency is never a key in its own `rates` map).
fn eur_rate_of(currency: &str, eur_rates: &BTreeMap<String, f64>) -> Result<f64> {
    if currency == "EUR" {
        return Ok(1.0);
    }
    eur_rates.get(currency).copied().ok_or_else(|| {
        Error::NotFound(format!(
            "no published EUR-based rate for currency '{currency}'"
        ))
    })
}

/// Convert an EUR-based rate table into the rate from `currency` to
/// `reference_currency`, via the identity `rate(X -> Y) = eur(Y) / eur(X)`.
fn cross_rate(
    currency: &str,
    reference_currency: &str,
    eur_rates: &BTreeMap<String, f64>,
) -> Result<f64> {
    if currency == reference_currency {
        return Ok(1.0);
    }
    let currency_rate = eur_rate_of(currency, eur_rates)?;
    let reference_rate = eur_rate_of(reference_currency, eur_rates)?;
    Ok(reference_rate / currency_rate)
}

/// Convert `(rates, actual_date)` for a non-reference currency into a
/// [`ResolvedRate`], or short-circuit to the 1.0 identity when `currency`
/// already is the reference currency (skipping the cache/network lookup
/// entirely).
async fn resolve<F, Fut>(currency: &str, reference_currency: &str, fetch: F) -> Result<ResolvedRate>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(BTreeMap<String, f64>, NaiveDate)>>,
{
    let currency = currency.trim().to_uppercase();
    let reference_currency = reference_currency.trim().to_uppercase();

    if currency == reference_currency {
        return Ok(ResolvedRate {
            rate: 1.0,
            rate_date: None,
        });
    }

    let (rates, actual_date) = fetch().await?;
    let rate = cross_rate(&currency, &reference_currency, &rates)?;
    Ok(ResolvedRate {
        rate,
        rate_date: Some(actual_date),
    })
}

/// Return the rate to convert an amount in `currency` into the user's
/// configured reference currency, as published on or before `date`.
///
/// When `currency` is the reference currency, returns `1.0` with no cache or
/// network lookup. Otherwise, an exact cache hit is used first; on a miss,
/// the network is consulted (unless `FINGUARD_FX_OFFLINE` is set), and on a
/// network failure the nearest earlier cached date is used instead. The
/// returned [`ResolvedRate::rate_date`] tells the caller which date actually
/// backed the rate, which may be earlier than `date`.
///
/// # Errors
///
/// Returns [`Error::NotFound`] if no cached rate is available and the network
/// is disabled or fails, and never substitutes a made-up rate in that case.
pub async fn rate_on(date: NaiveDate, currency: &str) -> Result<ResolvedRate> {
    let reference_currency = config::get_currency_settings()?.reference_currency;
    resolve(currency, &reference_currency, || resolve_eur_rates_on(date)).await
}

/// Return the rate to convert an amount in `currency` into the reference
/// currency, as published on or before the last calendar day of `year`-`month`.
///
/// See [`rate_on`] for the cache/network/fallback behavior and error
/// conditions; this is a thin wrapper that resolves the month-end date first.
pub async fn month_end_rate(year: i32, month: u32, currency: &str) -> Result<ResolvedRate> {
    let last_day = last_day_of_month(year, month)?;
    rate_on(last_day, currency).await
}

/// Return the newest published rate to convert an amount in `currency` into
/// the reference currency.
///
/// When `currency` is the reference currency, returns `1.0` with no cache or
/// network lookup. Otherwise the network is always consulted first (unless
/// `FINGUARD_FX_OFFLINE` is set), since only the network can say what is
/// actually the newest published rate; on a network failure or when offline,
/// the most recently cached date is used instead. See [`rate_on`] for the
/// error condition when nothing is cached and the network is unavailable.
pub async fn live_rate(currency: &str) -> Result<ResolvedRate> {
    let reference_currency = config::get_currency_settings()?.reference_currency;
    resolve(currency, &reference_currency, resolve_latest_eur_rates).await
}

/// One resolved rate per currency for a single month, keyed by uppercase ISO
/// currency code.
type MonthRates = BTreeMap<String, f64>;

/// A rate to convert every currency actually present in a year's net-worth
/// data into the reference currency, one rate per calendar month.
///
/// Built once per chart request by [`monthly_rates`] so a chart function can
/// convert many rows without an async lookup per row; see that function for
/// the month-selection rule. [`MonthlyRates::rate`] is the lookup used by the
/// caller.
#[derive(Debug, Clone, Default)]
pub struct MonthlyRates {
    reference_currency: String,
    /// Keyed by month (1..=12). Never holds the reference currency, since
    /// that case is always the 1.0 identity handled directly by `rate`.
    rates: BTreeMap<u32, MonthRates>,
}

impl MonthlyRates {
    /// Return the rate to convert an amount in `currency` for `month`
    /// (1..=12) into the reference currency.
    ///
    /// The reference currency always returns `1.0` with no lookup. Any other
    /// currency must have been included in the `currencies` argument to
    /// [`monthly_rates`] that built this table.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotFound`] for a currency that was not resolved for
    /// `month`, rather than silently substituting `1.0`: that substitution is
    /// exactly the conversion bug this table exists to prevent.
    pub fn rate(&self, month: u32, currency: &str) -> Result<f64> {
        let currency = currency.trim().to_uppercase();
        if currency == self.reference_currency {
            return Ok(1.0);
        }
        self.rates
            .get(&month)
            .and_then(|month_rates| month_rates.get(&currency))
            .copied()
            .ok_or_else(|| {
                Error::NotFound(format!(
                    "no resolved FX rate for '{currency}' in month {month:02}; it was not \
                     included in the currencies passed to monthly_rates"
                ))
            })
    }

    /// Build a table directly from already-resolved rates, bypassing the
    /// cache and the network entirely.
    ///
    /// `#[cfg(test)]` compiles this into every unit test binary in the crate
    /// (not just this module's own), which is what lets another module's
    /// tests, such as `plots`'s currency-conversion tests, hand-build a
    /// deterministic table without seeding `fx`'s own disk cache.
    #[cfg(test)]
    pub(crate) fn for_test(reference_currency: &str, rates: BTreeMap<u32, MonthRates>) -> Self {
        Self {
            reference_currency: reference_currency.trim().to_uppercase(),
            rates,
        }
    }
}

/// The month-end dates that back one year of [`monthly_rates`].
///
/// Pure and currency-independent: [`monthly_rates_core`] resolves exactly
/// these dates through [`month_end_rate`], and [`monthly_rates_lenient`]
/// pre-warms the cache for them with one range fetch before its per-currency
/// loop, so the two lists cannot drift apart.
///
/// Never contains a date served only by [`live_rate`]: under
/// [`config::CurrentMonthRateMode::Live`] the in-progress month and every
/// later month resolve through the network-first live lookup, which must keep
/// hitting the network unconditionally.
struct YearRateDates {
    /// `(month, last day)` for every completed month of the year.
    completed: Vec<(u32, NaiveDate)>,
    /// Last day of the month before `today`, backing the in-progress month
    /// and every later month when the year reaches the current period under
    /// [`config::CurrentMonthRateMode::PreviousMonthEnd`]. `None` for fully
    /// past years, and always `None` under `Live`.
    previous_month_end: Option<NaiveDate>,
}

/// Compute the [`YearRateDates`] for `year` given `today` and `mode`.
///
/// A month of `year` is completed exactly when `(year, month)` precedes
/// `(today.year(), today.month())`, the same rule [`monthly_rates_core`]
/// applies in its fetch loop. When `year` reaches the current period and
/// `mode` is `PreviousMonthEnd`, the in-progress month's rate comes from the
/// previous month's close (December of the prior year for January).
fn year_rate_dates(
    today: NaiveDate,
    year: i32,
    mode: config::CurrentMonthRateMode,
) -> Result<YearRateDates> {
    let mut completed = Vec::new();
    for month in 1..=12u32 {
        if (year, month) < (today.year(), today.month()) {
            completed.push((month, last_day_of_month(year, month)?));
        }
    }
    let reaches_current = (year, 12u32) >= (today.year(), today.month());
    let previous_month_end =
        if reaches_current && mode == config::CurrentMonthRateMode::PreviousMonthEnd {
            let (prev_year, prev_month) = if today.month() == 1 {
                (today.year() - 1, 12)
            } else {
                (today.year(), today.month() - 1)
            };
            Some(last_day_of_month(prev_year, prev_month)?)
        } else {
            None
        };
    Ok(YearRateDates {
        completed,
        previous_month_end,
    })
}

impl YearRateDates {
    /// Every date [`monthly_rates_core`] resolves through [`month_end_rate`]
    /// for this year: each completed month's last day, plus the previous
    /// month-end backing the current period when present. Sorted and
    /// deduplicated, since that previous month-end can coincide with a
    /// completed month of the same year.
    fn prewarm_dates(&self) -> Vec<NaiveDate> {
        let mut dates: Vec<NaiveDate> = self.completed.iter().map(|(_, date)| *date).collect();
        if let Some(prev) = self.previous_month_end
            && !dates.contains(&prev)
        {
            dates.push(prev);
        }
        dates.sort();
        dates
    }
}

/// Resolve one rate per currency in `currencies` for every month of `year`,
/// given an explicit `today` so the completed/in-progress/future rule (see
/// [`monthly_rates`]) is testable without depending on the real date.
async fn monthly_rates_core(
    today: NaiveDate,
    year: i32,
    currencies: &[String],
) -> Result<MonthlyRates> {
    let settings = config::get_currency_settings()?;
    let reference_currency = settings.reference_currency.trim().to_uppercase();

    let mut needed: Vec<String> = currencies
        .iter()
        .map(|c| c.trim().to_uppercase())
        .filter(|c| *c != reference_currency)
        .collect();
    needed.sort();
    needed.dedup();

    let mut rates: BTreeMap<u32, MonthRates> = BTreeMap::new();
    if needed.is_empty() {
        return Ok(MonthlyRates {
            reference_currency,
            rates,
        });
    }

    let today_year = today.year();
    let today_month = today.month();

    // The exact month-end dates this call resolves, shared with the
    // [`monthly_rates_lenient`] prewarm so the two cannot drift apart.
    let rate_dates = year_rate_dates(today, year, settings.current_month_rate_mode)?;

    // The in-progress month and every later month share one rate per
    // currency (a future month tracks the in-progress month so the series has
    // no discontinuity), so resolve it once here instead of once per month.
    // Skipped entirely when `year` is fully in the past, so an old year never
    // triggers a live-rate network call it does not need.
    let mut current_period: MonthRates = BTreeMap::new();
    if (year, 12u32) >= (today_year, today_month) {
        for currency in &needed {
            let resolved = match settings.current_month_rate_mode {
                config::CurrentMonthRateMode::PreviousMonthEnd => {
                    let prev = rate_dates.previous_month_end.expect(
                        "year_rate_dates sets previous_month_end whenever the year \
                         reaches the current period under PreviousMonthEnd",
                    );
                    // `month_end_rate` is a thin wrapper resolving the
                    // month-end date first; `prev` already is that date.
                    rate_on(prev, currency).await?
                }
                config::CurrentMonthRateMode::Live => live_rate(currency).await?,
            };
            current_period.insert(currency.clone(), resolved.rate);
        }
    }

    for month in 1..=12u32 {
        // Every month before the in-progress one is a completed month, and
        // keeps its own frozen month-end rate rather than the current one.
        // `rate_dates.completed` holds exactly those months with their last
        // days, so a month absent from it takes the current-period rate.
        let completed = rate_dates
            .completed
            .iter()
            .find(|(m, _)| *m == month)
            .map(|(_, date)| *date);
        let mut month_rates = MonthRates::new();
        for currency in &needed {
            let rate = match completed {
                Some(last_day) => rate_on(last_day, currency).await?.rate,
                None => current_period[currency],
            };
            month_rates.insert(currency.clone(), rate);
        }
        rates.insert(month, month_rates);
    }

    Ok(MonthlyRates {
        reference_currency,
        rates,
    })
}

/// Resolve one rate per currency in `currencies` for every month of `year`,
/// for converting net-worth rows into the reference currency.
///
/// A completed month uses its own frozen month-end rate
/// ([`month_end_rate`]). The in-progress month (the real current calendar
/// month) follows [`config::CurrentMonthRateMode`]: `PreviousMonthEnd` uses
/// the preceding month's month-end rate (December of the prior year for
/// January), `Live` uses [`live_rate`]. A future month uses the same rate as
/// the in-progress month, so the series has no discontinuity; those months
/// hold no data anyway.
///
/// Resolves only the currencies actually present in `currencies`, and never
/// looks up the reference currency, so a user whose net worth is entirely in
/// the reference currency triggers no cache lookup and no network call.
pub async fn monthly_rates(year: i32, currencies: &[String]) -> Result<MonthlyRates> {
    monthly_rates_core(Local::now().date_naive(), year, currencies).await
}

/// Resolve one rate per currency in `currencies` for every month of `year`,
/// same as [`monthly_rates`], except a currency that cannot be resolved does
/// not sink the whole batch: each currency is looked up on its own, so a
/// missing network and an empty cache for one currency drops only that
/// currency from the returned table and names it in the second element,
/// instead of failing every other currency's lookup too.
///
/// The reference currency is never looked up and never appears in the
/// unavailable list, matching [`monthly_rates`].
///
/// Shared by every endpoint that must degrade per currency instead of
/// failing outright: `api.rs`'s `get_monthly_fx_rates_handler`,
/// `get_networth_evolution_handler`, and `get_networth_allocation_handler`
/// all resolve through this function rather than each re-implementing the
/// per-currency retry loop.
pub async fn monthly_rates_lenient(
    year: i32,
    currencies: &[String],
) -> Result<(MonthlyRates, Vec<String>)> {
    let op = crate::diag::begin();
    let start = std::time::Instant::now();
    let settings = config::get_currency_settings()?;
    let reference_currency = settings.reference_currency.trim().to_uppercase();

    let mut needed: Vec<String> = currencies
        .iter()
        .map(|c| c.trim().to_uppercase())
        .filter(|c| *c != reference_currency)
        .collect();
    needed.sort();
    needed.dedup();

    crate::diag::event(
        op,
        "fx",
        format!(
            "monthly_rates start ref={reference_currency} year={year} currencies={}",
            needed.len()
        ),
    );
    let mut combined = MonthlyRates {
        reference_currency: reference_currency.clone(),
        rates: BTreeMap::new(),
    };
    let mut unavailable_currencies = Vec::new();

    if needed.is_empty() {
        crate::diag::event(
            op,
            "fx",
            format!(
                "monthly_rates done in {}ms months=0 unavailable=0 []",
                start.elapsed().as_millis()
            ),
        );
        return Ok((combined, unavailable_currencies));
    }

    let today = Local::now().date_naive();
    // The exact month-end dates this call resolves, shared with
    // `monthly_rates_core` so the two cannot drift apart. Practically
    // infallible (only absurd years fail); on error every currency degrades,
    // exactly as the per-currency loop would on the same error.
    let rate_dates = match year_rate_dates(today, year, settings.current_month_rate_mode) {
        Ok(dates) => dates,
        Err(_) => {
            crate::diag::event(
                op,
                "fx",
                format!(
                    "monthly_rates done in {}ms months=0 unavailable={} [{}]",
                    start.elapsed().as_millis(),
                    needed.len(),
                    needed.join(",")
                ),
            );
            return Ok((combined, needed));
        }
    };

    // One snapshot for the whole call instead of one file read per month
    // per currency. An unreadable cache degrades every currency, exactly as
    // the per-currency loop does when each lookup fails to load it. The
    // snapshot is memoized on file identity, so a warm revisit clones it
    // instead of re-parsing.
    let mut batch = match load_snapshot() {
        Ok(snapshot) => SnapBatch::new((*snapshot).clone(), op),
        Err(_) => {
            crate::diag::event(
                op,
                "fx",
                format!(
                    "monthly_rates done in {}ms months=0 unavailable={} [{}]",
                    start.elapsed().as_millis(),
                    needed.len(),
                    needed.join(",")
                ),
            );
            return Ok((combined, needed));
        }
    };

    // Pre-warm the batch snapshot for every month-end date this call will
    // resolve, with one range fetch for the whole span instead of one
    // request per month per currency below. Failures are ignored; the
    // per-month loop stays as the fallback. Never touches a `Live`
    // current-period date: `prewarm_dates` excludes it, so `live_rate`
    // keeps hitting the network.
    ensure_range_cached(&mut batch, &rate_dates.prewarm_dates()).await;

    // The in-progress month and every later month share one rate per
    // currency, mirroring `monthly_rates_core`: under `PreviousMonthEnd`
    // that is the previous month-end date resolved once here, under `Live`
    // it is the network-first live lookup, which stays untouched.
    let reaches_current = (year, 12u32) >= (today.year(), today.month());
    for currency in &needed {
        let mut current_period: Option<f64> = None;
        if reaches_current {
            match settings.current_month_rate_mode {
                config::CurrentMonthRateMode::PreviousMonthEnd => {
                    let prev = rate_dates.previous_month_end.expect(
                        "year_rate_dates sets previous_month_end whenever the year \
                         reaches the current period under PreviousMonthEnd",
                    );
                    match batch.resolve(prev, currency, &reference_currency).await {
                        Ok(resolved) => current_period = Some(resolved.rate),
                        Err(()) => {
                            unavailable_currencies.push(currency.clone());
                            continue;
                        }
                    }
                }
                config::CurrentMonthRateMode::Live => match live_rate(currency).await {
                    Ok(resolved) => current_period = Some(resolved.rate),
                    Err(_) => {
                        unavailable_currencies.push(currency.clone());
                        continue;
                    }
                },
            }
        }

        // A month that fails drops the whole currency, not just the month:
        // the per-currency `monthly_rates` call this replaces either
        // resolves fully or fails fully, so partial months must never merge.
        let mut months: BTreeMap<u32, MonthRates> = BTreeMap::new();
        let mut failed = false;
        for (month, last_day) in &rate_dates.completed {
            match batch
                .resolve(*last_day, currency, &reference_currency)
                .await
            {
                Ok(resolved) => {
                    months.insert(*month, BTreeMap::from([(currency.clone(), resolved.rate)]));
                }
                Err(()) => {
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            unavailable_currencies.push(currency.clone());
            continue;
        }
        // Every month before the in-progress one is completed and above; the
        // rest share the current-period rate resolved once per currency.
        for month in 1..=12u32 {
            months.entry(month).or_insert_with(|| {
                let current = current_period.expect(
                    "a non-completed month implies the year reaches the current \
                     period, whose rate resolved above",
                );
                BTreeMap::from([(currency.clone(), current)])
            });
        }
        for (month, month_rates) in months {
            combined.rates.entry(month).or_default().extend(month_rates);
        }
    }

    crate::diag::event(
        op,
        "fx",
        format!(
            "monthly_rates done in {}ms months={} unavailable={} [{}] fetches={} fallbacks={}",
            start.elapsed().as_millis(),
            combined.rates.len(),
            unavailable_currencies.len(),
            unavailable_currencies.join(","),
            batch.single_fetches,
            batch.stale_fallbacks
        ),
    );
    Ok((combined, unavailable_currencies))
}

/// Merge range-fetched `days` into `cache`, and alias still-unresolved
/// `key_dates` to their nearest earlier published day.
///
/// `min` is the earliest date the range fetch requested, so `days` proves
/// publication only from `min` onward. An alias is recorded only when its
/// target is at or after `min` *and* [`should_alias_as`] trusts the
/// substitution for `today`, so a recent weekend is never frozen the way an
/// old holiday is. A target before `min` is unproven: the response cannot
/// show whether a closer published day exists between it and the key date, so
/// the pre-existing cache may be stale there. Such key dates stay unresolved
/// for the per-key fallback, which fetches their true substitution directly.
///
/// Apart from its arguments this is pure, so tests cover it without the
/// network: [`ensure_range_cached`] merges through it twice, once into the
/// batch snapshot and once into the reloaded disk cache for persistence.
/// A key date already stored exactly, or already aliased, is left alone; a
/// key date with no earlier published day stays unresolved for the per-key
/// fallback.
///
/// Returns the number of day tables merged and the number of aliases
/// recorded, for the diag log.
fn merge_range_into_cache(
    cache: &mut RateCache,
    days: &BTreeMap<NaiveDate, BTreeMap<String, f64>>,
    key_dates: &[NaiveDate],
    min: NaiveDate,
    today: NaiveDate,
) -> (usize, usize) {
    for (date, rates) in days {
        cache.rates.insert(format_date(*date), rates.clone());
    }
    let mut aliases = 0;
    for key_date in key_dates {
        let key_str = format_date(*key_date);
        if cache.rates.contains_key(&key_str) || cache.aliases.contains_key(&key_str) {
            continue;
        }
        let earlier = match nearest_earlier(cache, &key_str) {
            Ok(found) => found,
            Err(_) => continue,
        };
        let Some((published_date, _)) = earlier else {
            continue;
        };
        if published_date >= min && should_alias_as(*key_date, published_date, today) {
            cache.aliases.insert(key_str, format_date(published_date));
            aliases += 1;
        }
    }
    (days.len(), aliases)
}

/// Whether `date` falls inside the window Frankfurter can actually answer,
/// [`FX_SERIES_START`] through `today` inclusive.
///
/// A date outside this window fails an exact fetch every time (404 before
/// the start, nothing published yet after today), so callers skip the
/// network round trip for it and fall back to the cache directly, the same
/// path an offline lookup already takes. Takes `today` as a parameter,
/// rather than reading the clock itself, so it is testable without it.
fn is_fetchable_date(date: NaiveDate, today: NaiveDate) -> bool {
    date >= FX_SERIES_START && date <= today
}

/// Clamp a range fetch minimum to the earliest cached day.
///
/// When the cache already holds history, fetching from the earliest cached
/// day covers every unresolved date at or after it; an older date can only
/// resolve through its own single fetch (exact for any date), so starting
/// the range at it would re-download span already on disk. Returns the
/// clamped minimum: `max(min, earliest cached day)` when the cache is
/// non-empty and the earliest key parses, else `min` unchanged. Pure, so
/// tests cover it without the network.
fn clamp_range_min(min: NaiveDate, cache: &RateCache) -> NaiveDate {
    let Some(earliest_str) = cache.rates.keys().next() else {
        return min;
    };
    let Ok(earliest) = parse_date(earliest_str) else {
        return min;
    };
    min.max(earliest)
}

/// Persist range-fetched `days` through the same locked load-modify-save
/// discipline as `store_historical_rates`: reload under
/// [`cache_write_lock`] right before writing, so a concurrent writer's save
/// is never lost.
async fn persist_range(
    days: &BTreeMap<NaiveDate, BTreeMap<String, f64>>,
    key_dates: &[NaiveDate],
    min: NaiveDate,
    today: NaiveDate,
) {
    let _guard = cache_write_lock().lock().await;
    let Ok(mut cache) = load_cache() else {
        return;
    };
    merge_range_into_cache(&mut cache, days, key_dates, min, today);
    let _ = save_cache(&cache);
}

/// Pre-warm the batch's snapshot for `key_dates` with a single Frankfurter
/// `{min}..{max}` range fetch, so the batch does not pay one HTTPS request
/// per distinct date on a cold cache.
///
/// Reads only `batch.cache` (one load per batch, done by the caller) and
/// merges fetched days into it in memory; persistence goes through
/// [`persist_range`]. Dates older than the cached history skip the range:
/// they resolve through their own exact single fetch downstream. Returns
/// the dates still missing afterwards; every failure mode returns them
/// all, leaving the per-key fallback owning them exactly as before.
///
/// All failures are ignored and the lenient `unavailable_currencies`
/// contract is unchanged. Logs one line per outcome under the batch's op
/// id. Counts only, never dates.
async fn ensure_range_cached(batch: &mut SnapBatch, key_dates: &[NaiveDate]) -> Vec<NaiveDate> {
    let op = batch.op;
    if key_dates.is_empty() {
        crate::diag::event(op, "fx", "prewarm skip: no key dates");
        return Vec::new();
    }
    if is_offline() {
        crate::diag::event(op, "fx", "prewarm skip: offline");
        return key_dates.to_vec();
    }
    let mut unresolved = Vec::new();
    for date in key_dates {
        match cached_rate_for(&batch.cache, *date) {
            Ok(None) => unresolved.push(*date),
            Ok(Some(_)) => {}
            Err(_) => return key_dates.to_vec(),
        }
    }
    if unresolved.is_empty() {
        crate::diag::event(
            op,
            "fx",
            format!("prewarm skip: {} dates already cached", key_dates.len()),
        );
        return Vec::new();
    }
    let (Some(min), Some(max)) = (
        unresolved.iter().min().copied(),
        unresolved.iter().max().copied(),
    ) else {
        return key_dates.to_vec();
    };
    let window_today = Local::now().date_naive();
    let range_min = clamp_range_min(min, &batch.cache).max(FX_SERIES_START);
    let range_max = max.min(window_today);
    // A date below `range_min` or above `window_today` cannot come from a
    // range fetch (Frankfurter has nothing to return for it), so it joins
    // `older` here rather than only being excluded from the `fetch_range`
    // call below; that keeps it in the returned still-missing list through
    // the same `in_range.is_empty()` skip this loop already had, instead of
    // needing a second empty-range check after computing `range_max`.
    let mut older = Vec::new();
    let mut in_range = Vec::new();
    for date in unresolved {
        if date < range_min || date > window_today {
            older.push(date);
        } else {
            in_range.push(date);
        }
    }
    if in_range.is_empty() {
        crate::diag::event(
            op,
            "fx",
            format!("prewarm skip: {} dates predate cached history", older.len()),
        );
        return older;
    }
    crate::diag::event(
        op,
        "fx",
        format!(
            "prewarm range: {} unresolved dates over {} days ({} older skipped)",
            in_range.len(),
            range_max.signed_duration_since(range_min).num_days(),
            older.len()
        ),
    );
    let days = match fetch_range(range_min, range_max, op).await {
        Ok(days) => days,
        Err(_) => return older.into_iter().chain(in_range).collect(),
    };
    if days.is_empty() {
        return older.into_iter().chain(in_range).collect();
    }
    let today = Local::now().date_naive();
    let (tables, aliases) =
        merge_range_into_cache(&mut batch.cache, &days, &in_range, range_min, today);
    persist_range(&days, &in_range, range_min, today).await;
    crate::diag::event(
        op,
        "fx",
        format!("prewarm merged {tables} day tables, {aliases} aliases"),
    );
    // Whatever the range did not cover stays missing for the concurrent
    // single-fetch phase, which rechecks the snapshot per date.
    let mut still_missing = older;
    for date in in_range {
        if !matches!(cached_rate_for(&batch.cache, date), Ok(Some(_))) {
            still_missing.push(date);
        }
    }
    still_missing
}

/// How one leftover date resolved in the concurrent phase.
enum MissingOutcome {
    /// The shared snapshot already covered it (the range did the work).
    Hit {
        table: BTreeMap<String, f64>,
        actual: NaiveDate,
    },
    /// A single fetch landed it (already stored through the locked path).
    Fetched {
        table: BTreeMap<String, f64>,
        returned: NaiveDate,
    },
    /// Offline, past-deadline, or fetch-failed: nearest-earlier fallback.
    Fallback {
        table: BTreeMap<String, f64>,
        actual: NaiveDate,
    },
    /// Nothing covers it: keys on this date are unavailable.
    Failed,
}

/// Nearest-earlier fallback for one date against a shared snapshot, without
/// any network access. Counts only, never dates.
fn fallback_from(snap: &RateCache, date: NaiveDate) -> (NaiveDate, MissingOutcome) {
    match nearest_earlier(snap, &format_date(date)) {
        Ok(Some((actual, table))) => (date, MissingOutcome::Fallback { table, actual }),
        _ => (date, MissingOutcome::Failed),
    }
}

/// Resolve leftover dates concurrently, bounded to small chunks so a huge
/// cold span cannot open hundreds of connections at once.
///
/// Each date resolves exactly as the sequential fallback would: snapshot
/// recheck (the range may have covered it while queued), one exact single
/// fetch stored through the locked path, else the shared snapshot's
/// nearest-earlier fallback, else unavailable. Dates past the batch
/// deadline skip the fetch and go straight to the fallback. A panicked or
/// lost task degrades its date rather than dropping it silently.
async fn fetch_missing_dates(
    snap: std::sync::Arc<RateCache>,
    dates: Vec<NaiveDate>,
    deadline: std::time::Instant,
    op: u64,
) -> Vec<(NaiveDate, MissingOutcome)> {
    const CHUNK: usize = 16;
    let mut outcomes = Vec::with_capacity(dates.len());
    for chunk in dates.chunks(CHUNK) {
        if std::time::Instant::now() >= deadline {
            for date in chunk {
                outcomes.push(fallback_from(&snap, *date));
            }
            continue;
        }
        let mut set = tokio::task::JoinSet::new();
        for date in chunk {
            let snap = snap.clone();
            let date = *date;
            set.spawn(async move {
                if let Ok(Some((table, actual))) = cached_rate_for(&snap, date) {
                    return (date, MissingOutcome::Hit { table, actual });
                }
                let today = Local::now().date_naive();
                if !is_offline()
                    && std::time::Instant::now() < deadline
                    && is_fetchable_date(date, today)
                    && let Ok((returned, table)) = fetch_historical(date, op).await
                {
                    let alias_for = should_alias_as(date, returned, today).then_some(date);
                    match store_historical_rates(returned, &table, alias_for).await {
                        Ok(()) => {
                            return (date, MissingOutcome::Fetched { table, returned });
                        }
                        Err(_) => return (date, MissingOutcome::Failed),
                    }
                }
                fallback_from(&snap, date)
            });
        }
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(outcome) => outcomes.push(outcome),
                Err(_) => {
                    crate::diag::event(op, "fx", "concurrent fetch task lost, date degrades");
                }
            }
        }
    }
    // Reconcile: any date without an outcome (a lost task above) degrades
    // rather than vanishing from the batch.
    let seen: std::collections::HashSet<NaiveDate> =
        outcomes.iter().map(|(date, _)| *date).collect();
    for date in dates {
        if !seen.contains(&date) {
            outcomes.push((date, MissingOutcome::Failed));
        }
    }
    outcomes
}

/// Total network wait one lenient batch spends before resolving the rest
/// from the cache alone.
///
/// A single Frankfurter request already gives up after [`REQUEST_TIMEOUT`],
/// but a batch fires many of them serially: on a hanging route that is
/// minutes of stall inside one API request, during which the UI shows its
/// empty state. Past this budget no more network attempts start; every
/// remaining key resolves from the snapshot (`nearest_earlier`) or lands in
/// `unavailable_currencies`, exactly the degraded shape the lenient
/// contract already returns. Warm-cache batches never touch the network,
/// so they never notice the budget.
const BATCH_NETWORK_BUDGET: Duration = Duration::from_secs(15);

/// One lenient batch's in-memory view of the rate cache.
///
/// `rate_on` re-reads `currency.json` and re-parses the whole `fx_rates.json`
/// per key, so a batch over thousands of keys pays thousands of full file
/// reads. This snapshot loads once per batch instead; fetches that land
/// mid-batch merge into it in memory, so later keys see them without
/// re-reading the file. Writes still go through the existing locked
/// load-modify-save path per fetched date, unchanged.
struct SnapBatch {
    cache: RateCache,
    op: u64,
    deadline: std::time::Instant,
    /// Single-date fetches attempted (range fetches excluded).
    single_fetches: u32,
    /// Keys answered from `nearest_earlier` after an offline miss, a fetch
    /// failure, or an exhausted budget.
    stale_fallbacks: u32,
    /// Whether the budget-exhausted line was already logged for this batch.
    budget_logged: bool,
}

impl SnapBatch {
    fn new(cache: RateCache, op: u64) -> Self {
        Self {
            cache,
            op,
            deadline: std::time::Instant::now() + BATCH_NETWORK_BUDGET,
            single_fetches: 0,
            stale_fallbacks: 0,
            budget_logged: false,
        }
    }

    /// Resolve one `(date, currency)` key into the reference currency,
    /// mirroring [`rate_on`]'s outcome key by key while reading the files
    /// only through this batch's snapshot.
    ///
    /// `Err(())` means "this key is unavailable", for every cause: the
    /// caller reports the currency and keeps the rest of the batch, exactly
    /// as the per-key loop it replaces does.
    async fn resolve(
        &mut self,
        date: NaiveDate,
        currency: &str,
        reference_currency: &str,
    ) -> std::result::Result<ResolvedRate, ()> {
        let currency = currency.trim().to_uppercase();
        let reference_currency = reference_currency.trim().to_uppercase();
        if currency == reference_currency {
            return Ok(ResolvedRate {
                rate: 1.0,
                rate_date: None,
            });
        }
        let date_str = format_date(date);
        // Snapshot hit: use it, exactly as `resolve_eur_rates_on` does. A
        // day table that lacks the currency still ends here as unavailable;
        // no network attempt follows, matching `rate_on`.
        match cached_rate_for(&self.cache, date) {
            Ok(Some((table, actual))) => {
                return cross_to_reference(&table, &currency, &reference_currency, actual)
                    .ok_or(());
            }
            Ok(None) => {}
            Err(_) => return Err(()),
        }
        // Miss. Offline, past-deadline, and fetch-failed keys all share the
        // nearest-earlier fallback below.
        if !is_offline() && std::time::Instant::now() < self.deadline {
            self.single_fetches += 1;
            if let Ok((returned, table)) = fetch_historical(date, self.op).await {
                let today = Local::now().date_naive();
                let alias_for = should_alias_as(date, returned, today).then_some(date);
                // Same locked load-modify-save as `store_historical_rates`,
                // then mirror it into this batch's snapshot so later keys
                // see the fetch without re-reading the file. A failed
                // store degrades the key rather than the batch, matching
                // the per-key path where the store error propagates as
                // that key's own failure.
                match store_historical_rates(returned, &table, alias_for).await {
                    Ok(()) => {
                        let returned_str = format_date(returned);
                        self.cache.rates.insert(returned_str.clone(), table.clone());
                        if let Some(requested) = alias_for {
                            self.cache
                                .aliases
                                .insert(format_date(requested), returned_str);
                        }
                        return cross_to_reference(
                            &table,
                            &currency,
                            &reference_currency,
                            returned,
                        )
                        .ok_or(());
                    }
                    Err(_) => return Err(()),
                }
            }
        } else if !is_offline() && !self.budget_logged {
            self.budget_logged = true;
            crate::diag::event(
                self.op,
                "fx",
                format!(
                    "network budget exhausted after {} single fetches, remaining keys resolve from cache",
                    self.single_fetches
                ),
            );
        }
        self.stale_fallbacks += 1;
        match nearest_earlier(&self.cache, &date_str) {
            Ok(Some((found_date, table))) => {
                cross_to_reference(&table, &currency, &reference_currency, found_date).ok_or(())
            }
            _ => Err(()),
        }
    }
}

/// Convert one EUR-based day table into a [`ResolvedRate`] from `currency`
/// to the reference currency, or `None` when the table names neither.
fn cross_to_reference(
    table: &BTreeMap<String, f64>,
    currency: &str,
    reference_currency: &str,
    actual_date: NaiveDate,
) -> Option<ResolvedRate> {
    let rate = cross_rate(currency, reference_currency, table).ok()?;
    Some(ResolvedRate {
        rate,
        rate_date: Some(actual_date),
    })
}

/// Resolve every `(date, currency)` key in `keys` into the reference
/// currency, without letting one unresolvable currency sink the whole batch:
/// each key is looked up on its own, and a key that fails is left out of the
/// returned map while its currency code is named in the second element.
///
/// Built for the expense endpoints, whose callers already produce exactly
/// this key set through [`crate::df_operations::distinct_rate_keys`]. Feed
/// the returned map straight to [`crate::df_operations::resolve_fact`]; a row
/// whose key is missing is the degraded case the caller must report rather
/// than convert.
///
/// The reference currency is never looked up (it converts at `1.0`, which
/// `resolve_fact` handles without a map entry) and never appears in the
/// unavailable list. Codes in that list are uppercase, deduplicated, and
/// sorted.
///
/// Mirrors [`monthly_rates_lenient`]'s error boundary deliberately, so the
/// two degrade the same way: reading the currency settings happens once, up
/// front, and its failure propagates as a genuine error, while *any* error
/// from resolving a single currency (an empty cache offline, a network
/// failure, an unreadable cache file, a currency the ECB does not publish)
/// counts as "this currency is unavailable". Classifying those kinds more
/// finely here would give the two functions two different definitions of the
/// same condition.
pub async fn rates_for_keys_lenient(
    keys: &[(NaiveDate, String)],
) -> Result<(HashMap<(NaiveDate, String), ResolvedRate>, Vec<String>)> {
    let op = crate::diag::begin();
    let start = std::time::Instant::now();
    let reference_currency = config::get_currency_settings()?
        .reference_currency
        .trim()
        .to_uppercase();

    crate::diag::event(
        op,
        "fx",
        format!(
            "rates_for_keys start ref={reference_currency} keys={}",
            keys.len()
        ),
    );

    // Load the cache once for the whole batch instead of once per key.
    // An unreadable cache fails every key lookup, exactly as the per-key
    // path does: every non-reference currency is reported once, and nothing
    // resolves. The snapshot is memoized on file identity, so a warm revisit
    // clones it instead of re-parsing.
    let mut batch = match load_snapshot() {
        Ok(snapshot) => SnapBatch::new((*snapshot).clone(), op),
        Err(_) => {
            let mut unavailable: Vec<String> = keys
                .iter()
                .map(|(_, currency)| currency.trim().to_uppercase())
                .filter(|currency| *currency != reference_currency)
                .collect();
            unavailable.sort();
            unavailable.dedup();
            crate::diag::event(
                op,
                "fx",
                format!(
                    "rates_for_keys done in {}ms resolved=0 unavailable={} [{}]",
                    start.elapsed().as_millis(),
                    unavailable.len(),
                    unavailable.join(",")
                ),
            );
            return Ok((HashMap::new(), unavailable));
        }
    };

    // Pre-warm the batch snapshot with one range fetch for the whole date
    // span, so a cold cache does not cost one HTTPS request per distinct
    // expense date below. Returns the dates still missing afterwards;
    // failures return them all, leaving the fallback owning them exactly
    // as the per-key loop it replaces does.
    let mut key_dates: Vec<NaiveDate> = keys
        .iter()
        .filter(|(_, currency)| currency.trim().to_uppercase() != reference_currency)
        .map(|(date, _)| *date)
        .collect();
    key_dates.sort();
    key_dates.dedup();
    // Era counts: key dates older than any cached history can never resolve
    // (no published table predates them), and exact-epoch hits flag rows
    // whose date was never real. Counts only, never dates.
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch exists");
    let earliest_cached = batch
        .cache
        .rates
        .keys()
        .next()
        .and_then(|s| parse_date(s).ok());
    let predate_history = earliest_cached
        .map(|earliest| key_dates.iter().filter(|d| **d < earliest).count())
        .unwrap_or(0);
    let epoch_hits = key_dates.iter().filter(|d| **d == epoch).count();
    crate::diag::event(
        op,
        "fx",
        format!("key eras: predate_history={predate_history} epoch_hits={epoch_hits}"),
    );
    ensure_range_cached(&mut batch, &key_dates).await;

    let mut rates = HashMap::with_capacity(keys.len());
    let mut unavailable_currencies: Vec<String> = Vec::new();
    let mut report_unavailable = |currency: String| {
        if !unavailable_currencies.contains(&currency) {
            unavailable_currencies.push(currency);
        }
    };

    // Phase A: snapshot hits, including everything the range just merged.
    // Misses group by date for the concurrent phase below.
    let mut by_date: std::collections::HashMap<NaiveDate, Vec<String>> =
        std::collections::HashMap::new();
    for (date, currency) in keys {
        let currency = currency.trim().to_uppercase();
        if currency == reference_currency {
            continue;
        }
        match cached_rate_for(&batch.cache, *date) {
            Ok(Some((table, actual))) => {
                match cross_to_reference(&table, &currency, &reference_currency, actual) {
                    Some(resolved) => {
                        rates.insert((*date, currency), resolved);
                    }
                    None => report_unavailable(currency),
                }
            }
            Ok(None) => {
                by_date.entry(*date).or_default().push(currency);
            }
            Err(_) => report_unavailable(currency),
        }
    }

    // Phase C: leftover dates concurrently, bounded and deadline-checked.
    // A day table missing the currency still ends as unavailable with no
    // further network attempt, matching `rate_on`.
    let mut concurrent_fetches = 0u32;
    if !by_date.is_empty() {
        let mut dates: Vec<NaiveDate> = by_date.keys().copied().collect();
        dates.sort();
        let snap = std::sync::Arc::new(batch.cache.clone());
        for (date, outcome) in fetch_missing_dates(snap, dates, batch.deadline, op).await {
            let Some(currencies) = by_date.remove(&date) else {
                continue;
            };
            let is_fetched = matches!(outcome, MissingOutcome::Fetched { .. });
            let resolved_table = match outcome {
                MissingOutcome::Hit { table, actual }
                | MissingOutcome::Fetched {
                    table,
                    returned: actual,
                }
                | MissingOutcome::Fallback { table, actual } => {
                    if is_fetched {
                        concurrent_fetches += 1;
                    }
                    Some((table, actual))
                }
                MissingOutcome::Failed => None,
            };
            match resolved_table {
                Some((table, actual)) => {
                    for currency in currencies {
                        match cross_to_reference(&table, &currency, &reference_currency, actual) {
                            Some(resolved) => {
                                rates.insert((date, currency), resolved);
                            }
                            None => report_unavailable(currency),
                        }
                    }
                }
                None => {
                    for currency in currencies {
                        report_unavailable(currency);
                    }
                }
            }
        }
    }

    unavailable_currencies.sort();
    crate::diag::event(
        op,
        "fx",
        format!(
            "rates_for_keys done in {}ms resolved={} unavailable={} [{}] concurrent={}",
            start.elapsed().as_millis(),
            rates.len(),
            unavailable_currencies.len(),
            unavailable_currencies.join(","),
            concurrent_fetches
        ),
    );
    Ok((rates, unavailable_currencies))
}

/// Return the last calendar day of `year`-`month`.
///
/// # Errors
///
/// Returns [`Error::InvalidArgument`] if `month` is not in the range 1-12, or
/// if `year`-`month` does not form a valid date.
fn last_day_of_month(year: i32, month: u32) -> Result<NaiveDate> {
    if !(1..=12).contains(&month) {
        return Err(Error::InvalidArgument(format!(
            "month must be between 1 and 12, got {month}"
        )));
    }
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_of_next_month =
        NaiveDate::from_ymd_opt(next_year, next_month, 1).ok_or_else(|| {
            Error::InvalidArgument(format!("'{year}-{month:02}' is not a valid year/month"))
        })?;
    Ok(first_of_next_month
        .pred_opt()
        .expect("the day before the 1st of a valid month always exists"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Points `XDG_DATA_HOME`, `XDG_CONFIG_HOME` and `HOME` at a fresh temp
    /// dir, and forces offline mode, so tests never touch real user data or
    /// the network. Matches the `with_temp_data_home` pattern used in
    /// `df_operations.rs`.
    fn with_temp_env_offline() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir.path());
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            std::env::set_var("HOME", dir.path());
            std::env::set_var("FINGUARD_FX_OFFLINE", "1");
        }
        dir
    }

    fn seed_cache(dates: &[(&str, &[(&str, f64)])]) {
        let mut rates = BTreeMap::new();
        for (date, pairs) in dates {
            let mut day = BTreeMap::new();
            for (currency, rate) in *pairs {
                day.insert(currency.to_string(), *rate);
            }
            rates.insert(date.to_string(), day);
        }
        save_cache(&RateCache {
            base: "EUR".to_string(),
            rates,
            ..Default::default()
        })
        .expect("seed cache");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reference_currency_is_always_1_0_with_no_lookup() {
        let _temp = with_temp_env_offline();
        // No cache file exists at all: if this looked anything up, it would
        // hit the "nothing cached, network disabled" error path instead.
        let resolved = rate_on(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(), "EUR")
            .await
            .expect("reference currency never fails");
        assert_eq!(resolved.rate, 1.0);
        assert_eq!(resolved.rate_date, None);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn exact_cache_hit_is_used_without_a_fallback() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-04", &[("USD", 1.1622)])]);

        let resolved = rate_on(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(), "USD")
            .await
            .expect("exact cache hit");
        assert_eq!(resolved.rate, 1.0 / 1.1622);
        assert_eq!(
            resolved.rate_date,
            Some(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap())
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn missing_date_falls_back_to_nearest_earlier_cached_date() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-04", &[("USD", 1.1622)])]);

        // 2026-09-06 (a Sunday) has no cached entry; the nearest earlier
        // Friday close must be used instead, and reported as such.
        let resolved = rate_on(NaiveDate::from_ymd_opt(2026, 9, 6).unwrap(), "USD")
            .await
            .expect("falls back to the nearest earlier cached date");
        assert_eq!(resolved.rate, 1.0 / 1.1622);
        assert_eq!(
            resolved.rate_date,
            Some(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap())
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn nothing_cached_and_offline_is_a_hard_error() {
        let _temp = with_temp_env_offline();

        let err = rate_on(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(), "USD")
            .await
            .expect_err("must not invent a 1.0 fallback for a non-reference currency");
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn non_eur_reference_currency_uses_a_cross_rate() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-04", &[("USD", 1.1622), ("GBP", 0.85898)])]);

        config::set_currency_settings(&config::CurrencySettings {
            reference_currency: "GBP".to_string(),
            current_month_rate_mode: config::CurrentMonthRateMode::PreviousMonthEnd,
        })
        .expect("save reference currency");

        let resolved = rate_on(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(), "USD")
            .await
            .expect("cross rate through EUR");
        assert_eq!(resolved.rate, 0.85898 / 1.1622);
        assert_eq!(
            resolved.rate_date,
            Some(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap())
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn month_end_rate_uses_the_last_calendar_day() {
        let _temp = with_temp_env_offline();
        // September has 30 days; seed the 29th so the lookup for the 30th
        // must fall back to it.
        seed_cache(&[("2026-09-29", &[("USD", 1.16)])]);

        let resolved = month_end_rate(2026, 9, "USD")
            .await
            .expect("month-end rate falls back to the last published day");
        assert_eq!(resolved.rate, 1.0 / 1.16);
        assert_eq!(
            resolved.rate_date,
            Some(NaiveDate::from_ymd_opt(2026, 9, 29).unwrap())
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn live_rate_offline_uses_the_newest_cached_date() {
        let _temp = with_temp_env_offline();
        seed_cache(&[
            ("2026-09-02", &[("USD", 1.15)]),
            ("2026-09-04", &[("USD", 1.1622)]),
        ]);

        let resolved = live_rate("USD")
            .await
            .expect("offline live rate uses the most recent cached entry");
        assert_eq!(resolved.rate, 1.0 / 1.1622);
        assert_eq!(
            resolved.rate_date,
            Some(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap())
        );
    }

    /// Seed a month-end rate for every completed month of 2026 (January
    /// through August, ahead of the `2026-09-15` "today" used throughout this
    /// test group), so resolving any of them succeeds. June and August carry
    /// distinct rates so a test can prove which one a lookup actually used;
    /// the rest carry an arbitrary filler rate.
    fn seed_completed_2026_months() {
        seed_cache(&[
            ("2026-01-31", &[("USD", 1.00)]),
            ("2026-02-28", &[("USD", 1.00)]),
            ("2026-03-31", &[("USD", 1.00)]),
            ("2026-04-30", &[("USD", 1.00)]),
            ("2026-05-31", &[("USD", 1.00)]),
            ("2026-06-30", &[("USD", 1.10)]),
            ("2026-07-31", &[("USD", 1.00)]),
            ("2026-08-31", &[("USD", 1.20)]),
        ]);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn monthly_rates_completed_month_uses_its_own_month_end_rate() {
        let _temp = with_temp_env_offline();
        // June's own month-end rate differs from every other completed
        // month's, so a lookup that used the wrong one would fail the assertion.
        seed_completed_2026_months();

        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let rates = monthly_rates_core(today, 2026, &["USD".to_string()])
            .await
            .expect("resolve monthly rates");

        assert_eq!(rates.rate(6, "USD").unwrap(), 1.0 / 1.10);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn monthly_rates_in_progress_month_previous_month_end_uses_preceding_month() {
        let _temp = with_temp_env_offline();
        seed_completed_2026_months();

        // Default settings already use `PreviousMonthEnd`.
        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let rates = monthly_rates_core(today, 2026, &["USD".to_string()])
            .await
            .expect("resolve monthly rates");

        assert_eq!(rates.rate(9, "USD").unwrap(), 1.0 / 1.20);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn monthly_rates_january_previous_month_end_reaches_back_a_year() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-12-31", &[("USD", 1.15)])]);

        let today = NaiveDate::from_ymd_opt(2027, 1, 10).unwrap();
        let rates = monthly_rates_core(today, 2027, &["USD".to_string()])
            .await
            .expect("resolve monthly rates");

        assert_eq!(rates.rate(1, "USD").unwrap(), 1.0 / 1.15);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn monthly_rates_in_progress_month_live_mode_uses_live_rate() {
        let _temp = with_temp_env_offline();
        // `seed_cache` replaces the whole cache file, so this repeats the
        // Jan-Aug filler from `seed_completed_2026_months` alongside the
        // newest cached date the offline live lookup must fall back to.
        seed_cache(&[
            ("2026-01-31", &[("USD", 1.00)]),
            ("2026-02-28", &[("USD", 1.00)]),
            ("2026-03-31", &[("USD", 1.00)]),
            ("2026-04-30", &[("USD", 1.00)]),
            ("2026-05-31", &[("USD", 1.00)]),
            ("2026-06-30", &[("USD", 1.10)]),
            ("2026-07-31", &[("USD", 1.00)]),
            ("2026-08-31", &[("USD", 1.20)]),
            ("2026-09-10", &[("USD", 1.18)]),
        ]);
        config::set_currency_settings(&config::CurrencySettings {
            reference_currency: "EUR".to_string(),
            current_month_rate_mode: config::CurrentMonthRateMode::Live,
        })
        .expect("save live mode");

        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let rates = monthly_rates_core(today, 2026, &["USD".to_string()])
            .await
            .expect("resolve monthly rates");

        // Offline live rate falls back to the newest cached date, not the
        // month-end rate that `PreviousMonthEnd` would have used.
        assert_eq!(rates.rate(9, "USD").unwrap(), 1.0 / 1.18);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn monthly_rates_future_month_matches_the_in_progress_month() {
        let _temp = with_temp_env_offline();
        seed_completed_2026_months();

        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let rates = monthly_rates_core(today, 2026, &["USD".to_string()])
            .await
            .expect("resolve monthly rates");

        assert_eq!(
            rates.rate(9, "USD").unwrap(),
            rates.rate(12, "USD").unwrap()
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn monthly_rates_reference_currency_is_never_looked_up() {
        let _temp = with_temp_env_offline();
        // No cache seeded at all: a lookup attempt for the reference currency
        // would hit the "nothing cached, network disabled" error path.

        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let rates = monthly_rates_core(today, 2026, &["EUR".to_string()])
            .await
            .expect("reference currency never fails");

        assert_eq!(rates.rate(1, "EUR").unwrap(), 1.0);
        assert_eq!(rates.rate(9, "EUR").unwrap(), 1.0);
        // No month was ever populated, proving no lookup was attempted.
        assert!(rates.rates.is_empty());
    }

    /// A resolvable currency and an unresolvable one in the same request must
    /// not affect each other: the resolvable one still gets a full month
    /// table, and only the unresolvable one is reported and left out.
    #[tokio::test]
    #[serial_test::serial]
    async fn monthly_rates_lenient_reports_one_failure_without_sinking_the_other() {
        let _temp = with_temp_env_offline();
        // Year far enough in the past that every month resolves through
        // `month_end_rate`, regardless of when this test actually runs.
        // Cached EUR-based: 1 EUR = 0.70 GBP, so a GBP amount converts into
        // the (default EUR) reference currency at 1 / 0.70, not 0.70 itself.
        seed_cache(&[("2000-01-31", &[("GBP", 0.70)])]);

        let (rates, unavailable) =
            monthly_rates_lenient(2000, &["GBP".to_string(), "USD".to_string()])
                .await
                .expect("a per-currency failure must not fail the whole batch");

        assert_eq!(unavailable, vec!["USD".to_string()]);
        assert_eq!(rates.rate(1, "GBP").unwrap(), 1.0 / 0.70);
        assert!(rates.rate(1, "USD").is_err());
    }

    /// A request naming only the reference currency needs no cache and no
    /// network at all, and must report nothing as unavailable.
    #[tokio::test]
    #[serial_test::serial]
    async fn monthly_rates_lenient_reference_currency_only_needs_no_lookup() {
        let _temp = with_temp_env_offline();
        // No cache seeded at all: a lookup attempt for the reference
        // currency would hit the "nothing cached, network disabled" error.

        let (rates, unavailable) = monthly_rates_lenient(2026, &["EUR".to_string()])
            .await
            .expect("reference currency never fails");

        assert!(unavailable.is_empty());
        assert_eq!(rates.rate(1, "EUR").unwrap(), 1.0);
    }

    // ======================================================================
    // `year_rate_dates`: the prewarm list the lenient net-worth path shares
    // with `monthly_rates_core`
    // ======================================================================

    /// A fully completed past year resolves one frozen rate per month: twelve
    /// month-end dates, and no previous-month-end for a current period that
    /// does not exist.
    #[test]
    fn year_rate_dates_fully_completed_past_year_returns_twelve_month_ends() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let dates = year_rate_dates(today, 2024, config::CurrentMonthRateMode::PreviousMonthEnd)
            .expect("a past year always resolves");

        assert_eq!(dates.completed.len(), 12);
        assert_eq!(
            dates.completed[0],
            (1, NaiveDate::from_ymd_opt(2024, 1, 31).unwrap())
        );
        assert_eq!(
            dates.completed[11],
            (12, NaiveDate::from_ymd_opt(2024, 12, 31).unwrap())
        );
        assert_eq!(dates.previous_month_end, None);
        assert_eq!(dates.prewarm_dates().len(), 12);
    }

    /// The year holding `today` excludes the in-progress month and every
    /// later month, and backs them with the previous month-end instead. That
    /// date can coincide with a completed month of the same year (August
    /// here), so the prewarm list deduplicates it.
    #[test]
    fn year_rate_dates_current_year_excludes_in_progress_month_and_adds_previous_month_end() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let dates = year_rate_dates(today, 2026, config::CurrentMonthRateMode::PreviousMonthEnd)
            .expect("the current year always resolves");

        assert_eq!(dates.completed.len(), 8);
        assert_eq!(
            dates.completed[7],
            (8, NaiveDate::from_ymd_opt(2026, 8, 31).unwrap())
        );
        assert_eq!(
            dates.previous_month_end,
            Some(NaiveDate::from_ymd_opt(2026, 8, 31).unwrap())
        );
        let prewarm = dates.prewarm_dates();
        assert_eq!(prewarm.len(), 8);
        // The in-progress month's own last day is never a prewarm date.
        assert!(!prewarm.contains(&NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()));
    }

    /// Under `Live` the in-progress month and every later month resolve
    /// through the network-first live lookup, so none of their dates may
    /// enter the prewarm list: only completed months appear.
    #[test]
    fn year_rate_dates_live_mode_never_includes_a_current_period_date() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let dates = year_rate_dates(today, 2026, config::CurrentMonthRateMode::Live)
            .expect("the current year always resolves");

        assert_eq!(dates.completed.len(), 8);
        assert_eq!(dates.previous_month_end, None);
        let prewarm = dates.prewarm_dates();
        assert_eq!(prewarm.len(), 8);
        assert!(!prewarm.contains(&NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()));
        assert!(!prewarm.contains(&NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()));
    }

    /// A future year has no completed month; under `PreviousMonthEnd` its
    /// whole series tracks the previous month-end alone.
    #[test]
    fn year_rate_dates_future_year_holds_only_the_previous_month_end() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        let dates = year_rate_dates(today, 2027, config::CurrentMonthRateMode::PreviousMonthEnd)
            .expect("a future year always resolves");

        assert!(dates.completed.is_empty());
        assert_eq!(
            dates.previous_month_end,
            Some(NaiveDate::from_ymd_opt(2026, 8, 31).unwrap())
        );
        assert_eq!(
            dates.prewarm_dates(),
            vec![NaiveDate::from_ymd_opt(2026, 8, 31).unwrap()]
        );
    }

    /// In January the previous month-end reaches back into December of the
    /// prior year, while January itself is not a completed month.
    #[test]
    fn year_rate_dates_january_previous_month_end_reaches_back_a_year() {
        let today = NaiveDate::from_ymd_opt(2027, 1, 10).unwrap();
        let dates = year_rate_dates(today, 2027, config::CurrentMonthRateMode::PreviousMonthEnd)
            .expect("January always resolves");

        assert!(dates.completed.is_empty());
        assert_eq!(
            dates.previous_month_end,
            Some(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap())
        );
    }

    /// Several currencies for one year must resolve from a seeded cache with
    /// no live network, with the exact month-end table and the
    /// nearest-earlier fallback agreeing: January reads its own table, June
    /// falls back to it. Same shape as Track A's
    /// `category_totals_convert_eur_rows_for_non_eur_reference`.
    #[tokio::test]
    #[serial_test::serial]
    async fn monthly_rates_lenient_resolves_several_currencies_from_a_seeded_cache() {
        let _temp = with_temp_env_offline();
        // Year 2000 keeps every month on the completed-month path regardless
        // of today's real date. Cached EUR-based: 1 EUR = 1.20 USD and
        // 1 EUR = 0.60 GBP, so into the USD reference EUR converts at 1.20
        // and GBP at 1.20 / 0.60 = 2.0.
        seed_cache(&[("2000-01-31", &[("USD", 1.20), ("GBP", 0.60)])]);
        config::set_currency_settings(&config::CurrencySettings {
            reference_currency: "USD".to_string(),
            current_month_rate_mode: config::CurrentMonthRateMode::PreviousMonthEnd,
        })
        .expect("switch reference currency to USD");

        let (rates, unavailable) =
            monthly_rates_lenient(2000, &["EUR".to_string(), "GBP".to_string()])
                .await
                .expect("seeded currencies resolve without the network");

        assert!(unavailable.is_empty());
        assert_eq!(rates.rate(1, "EUR").unwrap(), 1.20);
        assert_eq!(rates.rate(6, "EUR").unwrap(), 1.20);
        assert_eq!(rates.rate(1, "GBP").unwrap(), 2.0);
        assert_eq!(rates.rate(12, "GBP").unwrap(), 2.0);
    }

    // ======================================================================
    // `SnapBatch`: one snapshot per batch, one range fetch, bounded network
    // ======================================================================

    /// The batch resolver agrees with `rate_on` key by key from the same
    /// seeded cache: an exact hit, a weekend falling back to Friday, a date
    /// with nothing cached (unavailable), and a day table missing the
    /// currency (unavailable without a network attempt).
    #[tokio::test]
    #[serial_test::serial]
    async fn snap_batch_resolve_matches_rate_on_from_a_seeded_cache() {
        let _temp = with_temp_env_offline();
        // Friday close only; reference is the default EUR.
        seed_cache(&[("2026-09-04", &[("USD", 1.1622)])]);
        let cache = load_cache().expect("seeded cache loads");
        let mut batch = SnapBatch::new(cache, 0);

        let friday = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();
        let sunday = NaiveDate::from_ymd_opt(2026, 9, 6).unwrap();
        let far = NaiveDate::from_ymd_opt(2026, 1, 5).unwrap();

        // Exact hit agrees with `rate_on`, including the published date.
        let resolved = batch
            .resolve(friday, "USD", "EUR")
            .await
            .expect("exact hit resolves");
        let direct = rate_on(friday, "USD").await.expect("rate_on resolves");
        assert_eq!(resolved.rate, direct.rate);
        assert_eq!(resolved.rate_date, direct.rate_date);

        // Weekend falls back to Friday through the snapshot.
        let resolved = batch
            .resolve(sunday, "USD", "EUR")
            .await
            .expect("weekend falls back");
        assert_eq!(resolved.rate, 1.0 / 1.1622);
        assert_eq!(
            resolved.rate_date,
            Some(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap())
        );

        // Nothing cached on or before the date: unavailable, no network
        // offline.
        assert!(batch.resolve(far, "USD", "EUR").await.is_err());

        // The day is cached but names no GBP rate: unavailable, exactly as
        // `rate_on`, which also does not chase the network for a currency
        // missing from a cached day.
        assert!(batch.resolve(friday, "GBP", "EUR").await.is_err());
        assert!(rate_on(friday, "GBP").await.is_err());

        // Reference currency is the identity without any lookup.
        let identity = batch
            .resolve(far, "EUR", "EUR")
            .await
            .expect("reference never fails");
        assert_eq!(identity.rate, 1.0);
        assert_eq!(identity.rate_date, None);
    }

    /// A batch whose network budget is already spent attempts no request:
    /// with an empty cache the miss degrades straight to unavailable and
    /// the fetch counter stays at zero. Proves the bound without depending
    /// on live network timing.
    #[tokio::test]
    #[serial_test::serial]
    async fn snap_batch_zero_budget_attempts_no_network() {
        let _temp = with_temp_env_offline();
        let saved = std::env::var_os("FINGUARD_FX_OFFLINE");
        unsafe {
            std::env::remove_var("FINGUARD_FX_OFFLINE");
        }
        // No cache file exists at all: any network attempt would be the only
        // way to resolve.
        let mut batch = SnapBatch::new(load_cache().expect("empty cache loads"), 0);
        batch.deadline = std::time::Instant::now() - std::time::Duration::from_secs(1);

        let outcome = batch
            .resolve(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(), "USD", "EUR")
            .await;
        let fetches = batch.single_fetches;
        if let Some(value) = saved {
            unsafe {
                std::env::set_var("FINGUARD_FX_OFFLINE", value);
            }
        }
        assert!(outcome.is_err());
        assert_eq!(fetches, 0);
    }

    /// An unreadable cache file degrades the whole batch, exactly as the
    /// per-key path does when every lookup fails to load it: nothing
    /// resolves and every non-reference currency is reported once, sorted.
    #[tokio::test]
    #[serial_test::serial]
    async fn rates_for_keys_lenient_unreadable_cache_reports_everything_unavailable() {
        let _temp = with_temp_env_offline();
        std::fs::write(
            paths::get_fx_rates_path().expect("cache path"),
            "not json at all",
        )
        .expect("corrupt the cache");

        let friday = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();
        let (rates, unavailable) = rates_for_keys_lenient(&[
            (friday, "USD".to_string()),
            (friday, "EUR".to_string()),
            (friday, "GBP".to_string()),
        ])
        .await
        .expect("an unreadable cache still degrades");

        assert!(rates.is_empty());
        assert_eq!(unavailable, vec!["GBP".to_string(), "USD".to_string()]);
    }

    /// Several dates sharing one cached Friday all resolve through the
    /// batch fallback and agree on the published date, with no network
    /// offline.
    #[tokio::test]
    #[serial_test::serial]
    async fn rates_for_keys_lenient_falls_back_to_the_cached_friday_for_a_weekend() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-04", &[("USD", 1.1622)])]);

        let keys = ["2026-09-04", "2026-09-05", "2026-09-06", "2026-09-07"]
            .iter()
            .map(|s| {
                (
                    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap(),
                    "USD".to_string(),
                )
            })
            .collect::<Vec<_>>();
        let (rates, unavailable) = rates_for_keys_lenient(&keys)
            .await
            .expect("weekend keys resolve from the cache");

        assert!(unavailable.is_empty());
        assert_eq!(rates.len(), 4);
        for (_, resolved) in &rates {
            assert_eq!(resolved.rate, 1.0 / 1.1622);
            assert_eq!(
                resolved.rate_date,
                Some(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap())
            );
        }
    }

    // ======================================================================
    // `clamp_range_min` and the concurrent leftover phase
    // ======================================================================

    /// The clamp keeps the range minimum when the cache is empty or holds
    /// nothing newer, and lifts it to the earliest cached day otherwise, so
    /// a range never re-downloads span already on disk. Pure dates in,
    /// pure date out.
    #[test]
    fn clamp_range_min_lifts_to_the_earliest_cached_day() {
        let cache = RateCache {
            base: "EUR".to_string(),
            rates: BTreeMap::from([
                ("2020-01-31".to_string(), BTreeMap::new()),
                ("2026-09-04".to_string(), BTreeMap::new()),
            ]),
            ..Default::default()
        };
        let ancient = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let recent = NaiveDate::from_ymd_opt(2026, 9, 6).unwrap();

        assert_eq!(
            clamp_range_min(ancient, &cache),
            NaiveDate::from_ymd_opt(2020, 1, 31).unwrap()
        );
        // Already at or past the cached history: unchanged.
        assert_eq!(clamp_range_min(recent, &cache), recent);

        // Empty cache: nothing to clamp to.
        assert_eq!(clamp_range_min(ancient, &RateCache::default()), ancient);
    }

    // ======================================================================
    // `FX_SERIES_START` window: exact-fetch eligibility and the range clamp
    // ======================================================================

    /// The day before the series start is not fetchable, the start itself
    /// is, an ordinary date well inside the window is, today is, and
    /// tomorrow is not.
    #[test]
    fn is_fetchable_date_at_the_window_boundaries() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();

        assert!(!is_fetchable_date(
            FX_SERIES_START - chrono::Duration::days(1),
            today
        ));
        assert!(is_fetchable_date(FX_SERIES_START, today));
        assert!(is_fetchable_date(
            NaiveDate::from_ymd_opt(2020, 6, 15).unwrap(),
            today
        ));
        assert!(is_fetchable_date(today, today));
        assert!(!is_fetchable_date(today + chrono::Duration::days(1), today));
    }

    /// `ensure_range_cached`'s two clamp expressions, `clamp_range_min(..)
    /// .max(FX_SERIES_START)` and `max.min(today)`, applied to an unresolved
    /// span that starts before the series and ends after today: the
    /// resulting bounds are exactly the series start and today, never the
    /// raw span. Pure inputs and outputs, no timing involved.
    #[test]
    fn ensure_range_cached_bounds_clamp_to_the_publishable_window() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 20).unwrap();
        let cache = RateCache::default();
        let before_window = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let after_today = today + chrono::Duration::days(5);

        let range_min = clamp_range_min(before_window, &cache).max(FX_SERIES_START);
        let range_max = after_today.min(today);

        assert_eq!(range_min, FX_SERIES_START);
        assert_eq!(range_max, today);
    }

    /// A before-window date, an ordinary cached date, and a future date in
    /// one offline batch: the cached date resolves exactly, the future date
    /// falls back to the latest cached table through `nearest_earlier`, and
    /// the before-window date's currency is named in `unavailable` exactly
    /// once. This is the D1 property the window clamp must preserve: it
    /// changes latency, never a resolved rate or the unavailable list.
    #[tokio::test]
    #[serial_test::serial]
    async fn rates_for_keys_lenient_leaves_resolution_unchanged_across_the_window_boundaries() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-04", &[("USD", 1.1622)])]);

        let cached = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();
        let before_window = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let today = Local::now().date_naive();
        let future = today + chrono::Duration::days(30);

        let (rates, unavailable) = rates_for_keys_lenient(&[
            (cached, "USD".to_string()),
            (future, "USD".to_string()),
            (before_window, "JPY".to_string()),
        ])
        .await
        .expect("mixed batch degrades per key");

        assert_eq!(rates[&(cached, "USD".to_string())].rate, 1.0 / 1.1622);
        assert_eq!(rates[&(future, "USD".to_string())].rate, 1.0 / 1.1622);
        assert!(!rates.contains_key(&(before_window, "JPY".to_string())));
        assert_eq!(unavailable, vec!["JPY".to_string()]);
    }

    /// Several dates through the concurrent phase: an exact hit, a weekend
    /// falling back, a date with nothing on or before it (unavailable), and
    /// a cached day missing the currency (unavailable without any network).
    /// Offline, so every outcome comes from the snapshot alone.
    #[tokio::test]
    #[serial_test::serial]
    async fn rates_for_keys_lenient_resolves_mixed_dates_through_one_batch() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-04", &[("USD", 1.1622)])]);

        let friday = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();
        let sunday = NaiveDate::from_ymd_opt(2026, 9, 6).unwrap();
        let ancient = NaiveDate::from_ymd_opt(2000, 1, 5).unwrap();
        let (rates, unavailable) = rates_for_keys_lenient(&[
            (friday, "USD".to_string()),
            (sunday, "USD".to_string()),
            (ancient, "USD".to_string()),
            (friday, "GBP".to_string()),
        ])
        .await
        .expect("mixed batch degrades per key");

        assert_eq!(rates.len(), 2);
        assert_eq!(rates[&(friday, "USD".to_string())].rate, 1.0 / 1.1622);
        assert_eq!(rates[&(sunday, "USD".to_string())].rate, 1.0 / 1.1622);
        assert_eq!(unavailable, vec!["GBP".to_string(), "USD".to_string()]);
    }

    /// Under `Live` the lenient path resolves completed months from their
    /// month-ends and the current month onward from the newest cached table
    /// (the offline live fallback), with nothing unavailable.
    #[tokio::test]
    #[serial_test::serial]
    async fn monthly_rates_lenient_live_mode_uses_live_rate_for_current_months() {
        let _temp = with_temp_env_offline();
        let today = Local::now().date_naive();
        let year = today.year();
        let current_month = today.month();
        // Every month-end at 1.00, December at 1.50, so the newest cached
        // table (December 31st) is distinguishable from every month-end rate.
        let mut owned: Vec<(String, Vec<(&str, f64)>)> = Vec::new();
        for month in 1..=12u32 {
            let last = last_day_of_month(year, month).expect("valid month");
            let rate = if month == 12 { 1.50 } else { 1.00 };
            owned.push((format_date(last), vec![("USD", rate)]));
        }
        let refs: Vec<(&str, &[(&str, f64)])> = owned
            .iter()
            .map(|(date, pairs)| (date.as_str(), pairs.as_slice()))
            .collect();
        seed_cache(&refs);
        config::set_currency_settings(&config::CurrencySettings {
            reference_currency: "EUR".to_string(),
            current_month_rate_mode: config::CurrentMonthRateMode::Live,
        })
        .expect("save live mode");

        let (rates, unavailable) = monthly_rates_lenient(year, &["USD".to_string()])
            .await
            .expect("live lenient resolves from the cache");

        assert!(unavailable.is_empty());
        // The current month onward tracks the newest cached table.
        assert_eq!(rates.rate(current_month, "USD").unwrap(), 1.0 / 1.50);
        // Completed months keep their own frozen month-end rates.
        if current_month > 1 {
            assert_eq!(rates.rate(1, "USD").unwrap(), 1.0 / 1.00);
        }
    }

    /// A resolvable key and an unresolvable one in the same batch must not
    /// affect each other: the resolvable one lands in the map, and only the
    /// unresolvable one is reported and left out.
    #[tokio::test]
    #[serial_test::serial]
    async fn rates_for_keys_lenient_reports_one_failure_without_sinking_the_other() {
        let _temp = with_temp_env_offline();
        // Cached EUR-based: 1 EUR = 0.70 GBP, so a GBP amount converts into
        // the (default EUR) reference currency at 1 / 0.70, not 0.70 itself.
        seed_cache(&[("2026-09-04", &[("GBP", 0.70)])]);
        let date = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap();

        let (rates, unavailable) =
            rates_for_keys_lenient(&[(date, "GBP".to_string()), (date, "USD".to_string())])
                .await
                .expect("a per-key failure must not fail the whole batch");

        assert_eq!(unavailable, vec!["USD".to_string()]);
        assert_eq!(rates[&(date, "GBP".to_string())].rate, 1.0 / 0.70);
        assert!(!rates.contains_key(&(date, "USD".to_string())));
    }

    /// The same currency unresolvable on two dates must be named once, not
    /// once per date: the report is per currency.
    #[tokio::test]
    #[serial_test::serial]
    async fn rates_for_keys_lenient_names_an_unavailable_currency_once() {
        let _temp = with_temp_env_offline();

        let (rates, unavailable) = rates_for_keys_lenient(&[
            (
                NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(),
                "USD".to_string(),
            ),
            (
                NaiveDate::from_ymd_opt(2026, 9, 5).unwrap(),
                "USD".to_string(),
            ),
        ])
        .await
        .expect("a per-key failure must not fail the whole batch");

        assert_eq!(unavailable, vec!["USD".to_string()]);
        assert!(rates.is_empty());
    }

    /// A key naming the reference currency needs no cache and no network at
    /// all, and must report nothing as unavailable. It is left out of the map
    /// too, since `resolve_fact` handles that identity without an entry.
    #[tokio::test]
    #[serial_test::serial]
    async fn rates_for_keys_lenient_skips_the_reference_currency() {
        let _temp = with_temp_env_offline();
        // No cache seeded at all: a lookup attempt for the reference
        // currency would hit the "nothing cached, network disabled" error.

        let (rates, unavailable) = rates_for_keys_lenient(&[(
            NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(),
            "EUR".to_string(),
        )])
        .await
        .expect("reference currency never fails");

        assert!(unavailable.is_empty());
        assert!(rates.is_empty());
    }

    // ======================================================================
    // Range pre-warm: one `{min}..{max}` fetch instead of one request per date
    // ======================================================================

    /// A real Frankfurter range body carries one table per published day and
    /// no entry for weekends: 2026-08-01/02 (weekend) are absent while the
    /// surrounding working days are present.
    const SAMPLE_RANGE_BODY: &str = r#"{
        "amount": 1.0,
        "base": "EUR",
        "start_date": "2026-07-31",
        "end_date": "2026-08-05",
        "rates": {
            "2026-07-31": {"USD": 1.1485, "GBP": 0.85573},
            "2026-08-03": {"USD": 1.1535, "GBP": 0.85633},
            "2026-08-04": {"USD": 1.1515, "GBP": 0.85639},
            "2026-08-05": {"USD": 1.1554, "GBP": 0.8572}
        }
    }"#;

    /// A range body whose span starts at the earliest unresolved key date, as
    /// a real `{min}..{max}` fetch does: published tables for Monday
    /// 2026-08-03 through Friday 2026-08-07, with the weekend Saturday
    /// 2026-08-08 absent.
    const SAMPLE_RANGE_BODY_IN_KEY_SPAN: &str = r#"{
        "amount": 1.0,
        "base": "EUR",
        "start_date": "2026-08-03",
        "end_date": "2026-08-08",
        "rates": {
            "2026-08-03": {"USD": 1.1535, "GBP": 0.85633},
            "2026-08-04": {"USD": 1.1515, "GBP": 0.85639},
            "2026-08-05": {"USD": 1.1554, "GBP": 0.8572},
            "2026-08-06": {"USD": 1.1525, "GBP": 0.8568},
            "2026-08-07": {"USD": 1.1562, "GBP": 0.8576}
        }
    }"#;

    #[test]
    fn parse_range_response_parses_one_table_per_published_day() {
        let days = parse_range_response(SAMPLE_RANGE_BODY).expect("valid range body");

        assert_eq!(days.len(), 4);
        assert_eq!(
            days[&NaiveDate::from_ymd_opt(2026, 8, 4).unwrap()]["USD"],
            1.1515
        );
        assert_eq!(
            days[&NaiveDate::from_ymd_opt(2026, 7, 31).unwrap()]["GBP"],
            0.85573
        );
        // The weekend inside the span has no entry of its own.
        assert!(!days.contains_key(&NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()));
        assert!(!days.contains_key(&NaiveDate::from_ymd_opt(2026, 8, 2).unwrap()));
    }

    #[test]
    fn parse_range_response_rejects_a_body_that_is_not_a_range_table() {
        // A single-date body has `rates` as one flat table, not per-day
        // tables, so it must fail rather than parse into nonsense.
        let single = r#"{"amount": 1.0, "base": "EUR", "date": "2026-08-04",
            "rates": {"USD": 1.1515}}"#;
        assert!(parse_range_response(single).is_err());
        assert!(parse_range_response("not json at all").is_err());
    }

    /// Merging fetched days stores every day and aliases an old-enough
    /// weekend key date to its nearest earlier published day, while leaving
    /// exact-hit key dates without an alias. The weekend is not the earliest
    /// key, so its substitution lies inside the fetched span, matching a real
    /// `{min}..{max}` fetch.
    #[test]
    fn merge_range_into_cache_aliases_an_old_weekend_key_date() {
        let mut cache = RateCache::default();
        let days = parse_range_response(SAMPLE_RANGE_BODY_IN_KEY_SPAN).expect("valid range body");
        // 2026-08-08 is a Saturday with no published table; 2026-09-10 is
        // over a month later, so the substitution is old enough to freeze.
        let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let min = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let keys = [
            NaiveDate::from_ymd_opt(2026, 8, 3).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 5).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 8).unwrap(),
        ];

        merge_range_into_cache(&mut cache, &days, &keys, min, today);

        assert_eq!(cache.rates.len(), 5);
        assert_eq!(
            cache.aliases.get("2026-08-08"),
            Some(&"2026-08-07".to_string())
        );
        // Exact hits need no alias.
        assert!(!cache.aliases.contains_key("2026-08-03"));
        assert!(!cache.aliases.contains_key("2026-08-05"));
    }

    /// A key date whose nearest earlier cached day predates the fetched range
    /// must not be aliased to it: the range proves nothing before `min`, so
    /// that cached day may simply be stale. The key stays unresolved for the
    /// per-key fallback, while a day the fetch published stays a real cached
    /// day rather than an alias.
    #[test]
    fn merge_range_into_cache_does_not_alias_a_key_date_to_a_pre_min_target() {
        let mut cache = RateCache {
            base: "EUR".to_string(),
            rates: BTreeMap::from([(
                "2026-06-30".to_string(),
                BTreeMap::from([("USD".to_string(), 1.14)]),
            )]),
            ..Default::default()
        };
        let days = BTreeMap::from([(
            NaiveDate::from_ymd_opt(2026, 8, 3).unwrap(),
            BTreeMap::from([("USD".to_string(), 1.1535)]),
        )]);
        // 2026-08-01 is a Saturday; 2026-09-10 is over a month later, so an
        // alias would be trusted were its target inside the fetched span.
        let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let min = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let keys = [
            NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 3).unwrap(),
        ];

        merge_range_into_cache(&mut cache, &days, &keys, min, today);

        // The only earlier cached day, 2026-06-30, is before `min`, so nothing
        // may be frozen; the per-key fallback owns these keys.
        assert!(cache.aliases.is_empty());
        assert!(!cache.aliases.contains_key("2026-08-01"));
        assert!(!cache.aliases.contains_key("2026-08-02"));
        // The fetched Monday stays a real cached day, not an alias.
        assert!(cache.rates.contains_key("2026-08-03"));
        assert!(!cache.aliases.contains_key("2026-08-03"));
    }

    /// A weekend key date inside the alias trust window still gets the fetched
    /// days cached, but no alias is frozen for it. The earlier Friday key puts
    /// the substitution inside the fetched span, so only the age window can be
    /// what withholds the alias.
    #[test]
    fn merge_range_into_cache_leaves_a_recent_weekend_without_alias() {
        let mut cache = RateCache::default();
        let days = parse_range_response(SAMPLE_RANGE_BODY).expect("valid range body");
        // Only four days after the Saturday: inside `ALIAS_MIN_AGE_DAYS`.
        let today = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap();
        let min = NaiveDate::from_ymd_opt(2026, 7, 31).unwrap();
        let keys = [
            NaiveDate::from_ymd_opt(2026, 7, 31).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
        ];

        merge_range_into_cache(&mut cache, &days, &keys, min, today);

        assert_eq!(cache.rates.len(), 4);
        assert!(cache.aliases.is_empty());
    }

    /// A key date older than every published day stays unresolved: there is
    /// nothing earlier to alias it to, and the per-key fallback owns that
    /// case.
    #[test]
    fn merge_range_into_cache_leaves_a_key_date_with_no_earlier_day_unaliased() {
        let mut cache = RateCache::default();
        let days = parse_range_response(SAMPLE_RANGE_BODY).expect("valid range body");
        let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let min = NaiveDate::from_ymd_opt(2026, 7, 30).unwrap();
        let keys = [NaiveDate::from_ymd_opt(2026, 7, 30).unwrap()];

        merge_range_into_cache(&mut cache, &days, &keys, min, today);

        assert_eq!(cache.rates.len(), 4);
        assert!(cache.aliases.is_empty());
    }

    #[test]
    fn last_day_of_month_handles_december_and_leap_years() {
        assert_eq!(
            last_day_of_month(2026, 9).unwrap(),
            NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
        );
        assert_eq!(
            last_day_of_month(2026, 12).unwrap(),
            NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()
        );
        assert_eq!(
            last_day_of_month(2024, 2).unwrap(),
            NaiveDate::from_ymd_opt(2024, 2, 29).unwrap()
        );
    }

    #[test]
    fn last_day_of_month_rejects_out_of_range_month() {
        let err = last_day_of_month(2026, 13).unwrap_err();
        assert!(matches!(err, Error::InvalidArgument(_)));
    }

    // ======================================================================
    // Fix 1: weekend/holiday dates are cached as aliases
    // ======================================================================

    #[test]
    fn rate_cache_deserializes_a_file_written_before_this_change() {
        // No `aliases`, `latest_date`, or `latest_fetched_at` keys at all,
        // matching a cache file saved by the code before this change.
        let json = r#"{ "base": "EUR", "rates": { "2026-09-04": { "USD": 1.1622 } } }"#;
        let cache: RateCache =
            serde_json::from_str(json).expect("an old-format cache file still loads");
        assert!(cache.aliases.is_empty());
        assert!(cache.latest_date.is_none());
        assert!(cache.latest_fetched_at.is_none());
    }

    #[test]
    fn cached_rate_for_exact_hit_reports_the_requested_date() {
        let cache = RateCache {
            base: "EUR".to_string(),
            rates: BTreeMap::from([(
                "2026-09-04".to_string(),
                BTreeMap::from([("USD".to_string(), 1.1622)]),
            )]),
            ..Default::default()
        };

        let (rates, actual_date) =
            cached_rate_for(&cache, NaiveDate::from_ymd_opt(2026, 9, 4).unwrap())
                .unwrap()
                .expect("exact hit");
        assert_eq!(rates["USD"], 1.1622);
        assert_eq!(actual_date, NaiveDate::from_ymd_opt(2026, 9, 4).unwrap());
    }

    #[test]
    fn cached_rate_for_alias_hit_reports_the_true_published_date() {
        let mut cache = RateCache {
            base: "EUR".to_string(),
            rates: BTreeMap::from([(
                "2026-09-04".to_string(),
                BTreeMap::from([("USD".to_string(), 1.1622)]),
            )]),
            ..Default::default()
        };
        // 2026-09-06 is a Sunday, aliased to the Friday Frankfurter actually
        // published for.
        cache
            .aliases
            .insert("2026-09-06".to_string(), "2026-09-04".to_string());

        let (rates, actual_date) =
            cached_rate_for(&cache, NaiveDate::from_ymd_opt(2026, 9, 6).unwrap())
                .unwrap()
                .expect("alias hit");
        assert_eq!(rates["USD"], 1.1622);
        // The Sunday must not be reported as if it were a publication date.
        assert_eq!(actual_date, NaiveDate::from_ymd_opt(2026, 9, 4).unwrap());
    }

    #[test]
    fn cached_rate_for_misses_when_neither_exact_nor_alias_is_cached() {
        let cache = RateCache::default();
        assert!(
            cached_rate_for(&cache, NaiveDate::from_ymd_opt(2026, 9, 6).unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn should_alias_as_true_for_an_old_enough_date_frankfurter_substituted() {
        let requested = NaiveDate::from_ymd_opt(2026, 8, 23).unwrap(); // Sunday
        let returned = NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(); // Friday
        let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap(); // 18 days later
        assert!(should_alias_as(requested, returned, today));
    }

    #[test]
    fn should_alias_as_false_when_the_returned_date_matches_the_request() {
        let date = NaiveDate::from_ymd_opt(2026, 8, 21).unwrap();
        let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        assert!(!should_alias_as(date, date, today));
    }

    #[test]
    fn should_alias_as_false_for_todays_own_request_even_when_substituted() {
        // A request for today that Frankfurter has not published yet must
        // never be frozen, since today's answer can still change later.
        let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let returned = NaiveDate::from_ymd_opt(2026, 9, 9).unwrap();
        assert!(!should_alias_as(today, returned, today));
    }

    #[test]
    fn should_alias_as_false_for_a_recent_past_date_within_the_min_age_window() {
        // A substitution for a date only a few days old is ambiguous: it
        // might be a genuine weekend, or the ECB might simply not have
        // published that business day's own rate yet. Below
        // `ALIAS_MIN_AGE_DAYS`, it must not be frozen as an alias.
        let requested = NaiveDate::from_ymd_opt(2026, 9, 6).unwrap(); // Sunday, 4 days ago
        let returned = NaiveDate::from_ymd_opt(2026, 9, 4).unwrap(); // Friday
        let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        assert!(!should_alias_as(requested, returned, today));
    }

    #[test]
    fn should_alias_as_true_for_a_date_exactly_alias_min_age_days_old() {
        // Pins the `>=` in `should_alias_as` against `ALIAS_MIN_AGE_DAYS`
        // itself: a date exactly at the boundary must still be old enough to
        // alias. An accidental `>` here would fail this test.
        let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let requested = today - chrono::Duration::days(ALIAS_MIN_AGE_DAYS);
        let returned = requested - chrono::Duration::days(1);
        assert!(should_alias_as(requested, returned, today));
    }

    #[test]
    fn should_alias_as_false_for_a_date_one_day_younger_than_alias_min_age() {
        // The other side of the same boundary: one day inside the window
        // must not be old enough to alias yet.
        let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        let requested = today - chrono::Duration::days(ALIAS_MIN_AGE_DAYS - 1);
        let returned = requested - chrono::Duration::days(1);
        assert!(!should_alias_as(requested, returned, today));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn repeat_weekend_lookup_uses_the_cached_alias_without_erroring() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-04", &[("USD", 1.1622)])]);
        let mut cache = load_cache().unwrap();
        cache
            .aliases
            .insert("2026-09-06".to_string(), "2026-09-04".to_string());
        save_cache(&cache).unwrap();

        // 2026-09-06 has no exact cache entry, only an alias; a working alias
        // lookup succeeds offline without ever needing `nearest_earlier`.
        let resolved = rate_on(NaiveDate::from_ymd_opt(2026, 9, 6).unwrap(), "USD")
            .await
            .expect("alias hit resolves without a network call");
        assert_eq!(resolved.rate, 1.0 / 1.1622);
        assert_eq!(
            resolved.rate_date,
            Some(NaiveDate::from_ymd_opt(2026, 9, 4).unwrap())
        );
    }

    // ======================================================================
    // Fix 2: `/latest` results stay fresh for a while
    // ======================================================================

    #[test]
    fn fresh_latest_from_cache_hits_within_the_staleness_threshold() {
        let cache = RateCache {
            base: "EUR".to_string(),
            rates: BTreeMap::from([(
                "2026-09-10".to_string(),
                BTreeMap::from([("USD".to_string(), 1.18)]),
            )]),
            latest_date: Some("2026-09-10".to_string()),
            latest_fetched_at: Some(Utc::now().timestamp() - 60),
            ..Default::default()
        };

        let (rates, actual_date) = fresh_latest_from_cache(&cache).unwrap().expect("fresh hit");
        assert_eq!(rates["USD"], 1.18);
        assert_eq!(actual_date, NaiveDate::from_ymd_opt(2026, 9, 10).unwrap());
    }

    #[test]
    fn fresh_latest_from_cache_misses_once_the_threshold_has_passed() {
        let cache = RateCache {
            base: "EUR".to_string(),
            rates: BTreeMap::from([(
                "2026-09-10".to_string(),
                BTreeMap::from([("USD".to_string(), 1.18)]),
            )]),
            latest_date: Some("2026-09-10".to_string()),
            latest_fetched_at: Some(Utc::now().timestamp() - LATEST_STALENESS_THRESHOLD_SECS - 1),
            ..Default::default()
        };

        assert!(fresh_latest_from_cache(&cache).unwrap().is_none());
    }

    #[test]
    fn fresh_latest_from_cache_treats_a_future_timestamp_as_stale() {
        // A `latest_fetched_at` ahead of now can only be clock skew or a
        // corrupted cache; it must not be treated as arbitrarily fresh.
        let cache = RateCache {
            base: "EUR".to_string(),
            rates: BTreeMap::from([(
                "2026-09-10".to_string(),
                BTreeMap::from([("USD".to_string(), 1.18)]),
            )]),
            latest_date: Some("2026-09-10".to_string()),
            latest_fetched_at: Some(Utc::now().timestamp() + 120),
            ..Default::default()
        };

        assert!(fresh_latest_from_cache(&cache).unwrap().is_none());
    }

    #[test]
    fn fresh_latest_from_cache_treats_a_pre_change_cache_file_as_stale() {
        // No `latest_date`/`latest_fetched_at` at all, matching a cache file
        // saved before this change; must fall through, not error.
        let cache = RateCache::default();
        assert!(fresh_latest_from_cache(&cache).unwrap().is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_latest_eur_rates_uses_latest_date_not_simply_the_newest_cached_key() {
        let _temp = with_temp_env_offline();
        // "2099-01-01" is the lexicographically largest key, so it is what
        // `newest_cached` would pick if freshness were ignored; a real fresh
        // hit must use `latest_date` instead and must not be confused with it.
        seed_cache(&[
            ("2026-09-08", &[("USD", 1.10)]),
            ("2099-01-01", &[("USD", 9.99)]),
        ]);
        let mut cache = load_cache().unwrap();
        cache.latest_date = Some("2026-09-08".to_string());
        cache.latest_fetched_at = Some(Utc::now().timestamp() - 5);
        save_cache(&cache).unwrap();

        let resolved = live_rate("USD").await.expect("fresh cache hit");
        assert_eq!(resolved.rate, 1.0 / 1.10);
        assert_eq!(
            resolved.rate_date,
            Some(NaiveDate::from_ymd_opt(2026, 9, 8).unwrap())
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_latest_eur_rates_falls_back_once_the_freshness_threshold_has_passed() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-08", &[("USD", 1.10)])]);
        let mut cache = load_cache().unwrap();
        cache.latest_date = Some("2026-09-08".to_string());
        cache.latest_fetched_at =
            Some(Utc::now().timestamp() - LATEST_STALENESS_THRESHOLD_SECS - 1);
        save_cache(&cache).unwrap();

        // Stale by more than the threshold, and offline: must fall back to
        // the newest cached date exactly as before this change, not error.
        let resolved = live_rate("USD")
            .await
            .expect("offline fallback still works once the cache is stale");
        assert_eq!(
            resolved.rate_date,
            Some(NaiveDate::from_ymd_opt(2026, 9, 8).unwrap())
        );
    }

    // ======================================================================
    // Fix 3: concurrent cache writers do not clobber each other
    // ======================================================================

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn concurrent_cache_writes_for_different_dates_all_survive() {
        let _temp = with_temp_env_offline();

        let mut handles = Vec::new();
        for day in 1..=8u32 {
            let date = NaiveDate::from_ymd_opt(2026, 1, day).unwrap();
            handles.push(tokio::spawn(async move {
                let rates = BTreeMap::from([("USD".to_string(), day as f64)]);
                store_historical_rates(date, &rates, None).await
            }));
        }
        for handle in handles {
            handle
                .await
                .expect("writer task did not panic")
                .expect("cache write succeeded");
        }

        let cache = load_cache().expect("load cache");
        assert_eq!(cache.rates.len(), 8, "every concurrent write must survive");
        for day in 1..=8u32 {
            let key = format!("2026-01-{day:02}");
            assert_eq!(cache.rates[&key]["USD"], day as f64);
        }
    }

    // ======================================================================
    // Snapshot memo: one parse per file identity, gated on (path, mtime, len)
    // ======================================================================

    /// A seeded file loads through the memo with its rates intact.
    #[tokio::test]
    #[serial_test::serial]
    async fn snapshot_memo_seeded_file_loads() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-04", &[("USD", 1.1622)])]);

        let snapshot = load_snapshot().expect("seeded file loads");
        assert_eq!(snapshot.rates["2026-09-04"]["USD"], 1.1622);
    }

    /// Overwriting the file with different data loads the new data: any save
    /// changes mtime or length, so a hit cannot serve pre-save rates. The two
    /// seeds differ in length, so the freshness proof does not depend on
    /// filesystem mtime granularity.
    #[tokio::test]
    #[serial_test::serial]
    async fn snapshot_memo_overwrite_loads_the_new_data() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-04", &[("USD", 1.10)])]);
        assert_eq!(
            load_snapshot().expect("first load").rates["2026-09-04"]["USD"],
            1.10
        );

        seed_cache(&[
            ("2026-09-04", &[("USD", 9.99)]),
            ("2026-09-05", &[("USD", 9.98)]),
        ]);
        let snapshot = load_snapshot().expect("overwrite reparses");
        assert_eq!(snapshot.rates["2026-09-04"]["USD"], 9.99);
        assert_eq!(snapshot.rates["2026-09-05"]["USD"], 9.98);
    }

    /// Deleting the file loads the default, exactly as `load_cache` always
    /// has, rather than the memo's pre-delete entry.
    #[tokio::test]
    #[serial_test::serial]
    async fn snapshot_memo_delete_loads_default() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-04", &[("USD", 1.1622)])]);
        load_snapshot().expect("seeded file loads");

        std::fs::remove_file(paths::get_fx_rates_path().expect("cache path"))
            .expect("delete the cache");
        let snapshot = load_snapshot().expect("missing file loads default");
        assert!(snapshot.rates.is_empty());
    }

    /// The memo is keyed by path: two directories hold independent snapshots,
    /// and returning to the first still serves its own data.
    #[tokio::test]
    #[serial_test::serial]
    async fn snapshot_memo_isolated_per_directory() {
        let dir_a = tempfile::tempdir().expect("create temp dir");
        let dir_b = tempfile::tempdir().expect("create temp dir");
        unsafe {
            std::env::set_var("FINGUARD_FX_OFFLINE", "1");
        }

        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir_a.path());
            std::env::set_var("XDG_CONFIG_HOME", dir_a.path());
            std::env::set_var("HOME", dir_a.path());
        }
        seed_cache(&[("2026-09-04", &[("USD", 1.10)])]);
        assert_eq!(
            load_snapshot().expect("dir A loads").rates["2026-09-04"]["USD"],
            1.10
        );

        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir_b.path());
            std::env::set_var("XDG_CONFIG_HOME", dir_b.path());
            std::env::set_var("HOME", dir_b.path());
        }
        seed_cache(&[("2026-09-04", &[("USD", 2.20)])]);
        assert_eq!(
            load_snapshot().expect("dir B loads").rates["2026-09-04"]["USD"],
            2.20
        );

        unsafe {
            std::env::set_var("XDG_DATA_HOME", dir_a.path());
            std::env::set_var("XDG_CONFIG_HOME", dir_a.path());
            std::env::set_var("HOME", dir_a.path());
        }
        assert_eq!(
            load_snapshot().expect("dir A still isolated").rates["2026-09-04"]["USD"],
            1.10
        );
    }

    /// Repeat loads of an unchanged file parse exactly once: the second load
    /// is a memo hit. The temp dir is fresh, so the first load is a certain
    /// miss regardless of what earlier tests cached.
    #[tokio::test]
    #[serial_test::serial]
    async fn snapshot_memo_repeat_loads_parse_once() {
        let _temp = with_temp_env_offline();
        seed_cache(&[("2026-09-04", &[("USD", 1.1622)])]);

        PARSE_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);
        load_snapshot().expect("first load parses");
        load_snapshot().expect("second load hits the memo");
        load_cache().expect("delegated load hits the memo too");
        assert_eq!(
            PARSE_COUNT.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "three loads of one unchanged file parse once"
        );
    }
}
