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
use std::sync::OnceLock;
use std::time::Duration;

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

/// Load the rate cache from disk. Returns an empty EUR-based cache if the
/// file does not exist yet.
fn load_cache() -> Result<RateCache> {
    let path = paths::get_fx_rates_path()?;
    if !path.exists() {
        return Ok(RateCache::default());
    }
    let contents = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&contents)?)
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
async fn fetch_rates(url: &str) -> Result<(NaiveDate, BTreeMap<String, f64>)> {
    let response = http_client()?
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Network(format!("request to {url} failed: {e}")))?
        .error_for_status()
        .map_err(|e| Error::Network(format!("Frankfurter returned an error for {url}: {e}")))?;
    let body: FrankfurterResponse = response.json().await.map_err(|e| {
        Error::Network(format!(
            "Frankfurter response from {url} was not the expected shape: {e}"
        ))
    })?;
    let date = parse_date(&body.date)?;
    Ok((date, body.rates))
}

/// Fetch the EUR-based rates Frankfurter published on or before `date`. The
/// returned date is frequently earlier than requested, since the ECB does not
/// publish on weekends or holidays.
async fn fetch_historical(date: NaiveDate) -> Result<(NaiveDate, BTreeMap<String, f64>)> {
    let url = format!("{FRANKFURTER_BASE_URL}/{}?base=EUR", format_date(date));
    fetch_rates(&url).await
}

/// Fetch the newest EUR-based rates Frankfurter has published.
async fn fetch_latest() -> Result<(NaiveDate, BTreeMap<String, f64>)> {
    fetch_rates(&format!("{FRANKFURTER_BASE_URL}/latest?base=EUR")).await
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
        match fetch_historical(date).await {
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
                    let (prev_year, prev_month) = if today_month == 1 {
                        (today_year - 1, 12)
                    } else {
                        (today_year, today_month - 1)
                    };
                    month_end_rate(prev_year, prev_month, currency).await?
                }
                config::CurrentMonthRateMode::Live => live_rate(currency).await?,
            };
            current_period.insert(currency.clone(), resolved.rate);
        }
    }

    for month in 1..=12u32 {
        // Every month before the in-progress one is a completed month, and
        // keeps its own frozen month-end rate rather than the current one.
        let is_completed = (year, month) < (today_year, today_month);
        let mut month_rates = MonthRates::new();
        for currency in &needed {
            let rate = if is_completed {
                month_end_rate(year, month, currency).await?.rate
            } else {
                current_period[currency]
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
/// failing outright: `main.rs`'s `get_monthly_fx_rates_handler`,
/// `get_networth_evolution_handler`, and `get_networth_allocation_handler`
/// all resolve through this function rather than each re-implementing the
/// per-currency retry loop.
pub async fn monthly_rates_lenient(
    year: i32,
    currencies: &[String],
) -> Result<(MonthlyRates, Vec<String>)> {
    let reference_currency = config::get_currency_settings()?
        .reference_currency
        .trim()
        .to_uppercase();

    let mut needed: Vec<String> = currencies
        .iter()
        .map(|c| c.trim().to_uppercase())
        .filter(|c| *c != reference_currency)
        .collect();
    needed.sort();
    needed.dedup();

    let mut combined = MonthlyRates {
        reference_currency,
        rates: BTreeMap::new(),
    };
    let mut unavailable_currencies = Vec::new();

    for currency in &needed {
        match monthly_rates(year, std::slice::from_ref(currency)).await {
            Ok(resolved) => {
                for (month, month_rates) in resolved.rates {
                    combined.rates.entry(month).or_default().extend(month_rates);
                }
            }
            Err(_) => unavailable_currencies.push(currency.clone()),
        }
    }

    Ok((combined, unavailable_currencies))
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
    let reference_currency = config::get_currency_settings()?
        .reference_currency
        .trim()
        .to_uppercase();

    let mut rates = HashMap::with_capacity(keys.len());
    let mut unavailable_currencies: Vec<String> = Vec::new();

    for (date, currency) in keys {
        let currency = currency.trim().to_uppercase();
        if currency == reference_currency {
            continue;
        }
        match rate_on(*date, &currency).await {
            Ok(resolved) => {
                rates.insert((*date, currency), resolved);
            }
            Err(_) => {
                if !unavailable_currencies.contains(&currency) {
                    unavailable_currencies.push(currency);
                }
            }
        }
    }

    unavailable_currencies.sort();
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
}
