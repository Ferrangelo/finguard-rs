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

use std::collections::BTreeMap;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::{Error, Result};
use crate::paths;

const FRANKFURTER_BASE_URL: &str = "https://api.frankfurter.app";

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
}

impl Default for RateCache {
    fn default() -> Self {
        Self {
            base: "EUR".to_string(),
            rates: BTreeMap::new(),
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
    let response = reqwest::get(url)
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

/// Resolve the EUR-based rate table published on or before `date`: an exact
/// cache hit first, then the network (unless offline), then the nearest
/// earlier cached date as a fallback, then a hard error.
async fn resolve_eur_rates_on(date: NaiveDate) -> Result<(BTreeMap<String, f64>, NaiveDate)> {
    let date_str = format_date(date);
    let cache = load_cache()?;

    if let Some(rates) = cache.rates.get(&date_str) {
        return Ok((rates.clone(), date));
    }

    if !is_offline() {
        match fetch_historical(date).await {
            Ok((returned_date, rates)) => {
                let mut cache = cache;
                cache
                    .rates
                    .insert(format_date(returned_date), rates.clone());
                save_cache(&cache)?;
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

/// Resolve the newest EUR-based rate table: the network first (unless
/// offline, since only the network can say what is actually newest), then the
/// most recently cached date as a fallback, then a hard error.
///
/// Unlike [`resolve_eur_rates_on`], there is no "exact cache hit" here: the
/// crate has no system-clock dependency, so it cannot tell whether a cached
/// entry already reflects today's publication without asking the network.
async fn resolve_latest_eur_rates() -> Result<(BTreeMap<String, f64>, NaiveDate)> {
    let cache = load_cache()?;

    if !is_offline() {
        match fetch_latest().await {
            Ok((returned_date, rates)) => {
                let mut cache = cache;
                cache
                    .rates
                    .insert(format_date(returned_date), rates.clone());
                save_cache(&cache)?;
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
}
