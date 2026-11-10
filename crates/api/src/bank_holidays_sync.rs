//! The bank holidays sync (distant-signal's
//! docs/superpowers/specs/2026-11-06-regular-journeys-design.md §8.2): one
//! fetch of GOV.UK's bank holidays JSON for the whole service, never per
//! user, into `bank_holidays` (`data::bank_holidays`).
//!
//! A `ds_store::loops` loop under its own advisory lock
//! (`ds_runtime::advisory_locks::BANK_HOLIDAYS_SYNC`, so with several api
//! replicas one runs it). It wakes every [`CHECK_EVERY`] (or every
//! `BANK_HOLIDAYS_INTERVAL_HOURS`, if shorter) and reads
//! `bank_holidays_sync`, so a restart or a new replica doesn't fetch again
//! early. It fetches when the last success is `BANK_HOLIDAYS_INTERVAL_HOURS`
//! (24) old or there has been none, and after a failure tries again
//! [`RETRY_AFTER`] later rather than a day later.
//!
//! A fetch that fails (transport, status, a body over [`MAX_BODY_BYTES`])
//! or a response that isn't plausible (`data::bank_holidays::parse`)
//! changes no row; a plausible one is applied in one transaction
//! (`data::bank_holidays::apply`), so a moved holiday moves.
//!
//! Outbound: one `GET` with a [`FETCH_TIMEOUT`] timeout, as
//! `data::tfl_arrivals` calls `TfL` (the chart's api egress already allows
//! 443 to the internet). Metrics: `api_bank_holidays_sync_total{result}`
//! (`ok`, `fetch_failed`, `invalid`), and the gauges
//! `api_bank_holidays_latest_date_seconds{division}` (the latest holiday
//! known, as a Unix time) and `api_bank_holidays_last_success_seconds`,
//! set by the replica holding the lock on each tick.
//!
//! Off unless `BANK_HOLIDAYS_ENABLED` (the chart's `api.bankHolidays.enabled`,
//! default false). With it off, or before the first success, the table is
//! empty and nothing is skipped.
//!
//! The loop can instead be switched off with `BANK_HOLIDAYS_LOOP_ENABLED=false`
//! (default true, so nothing changes until the chart sets it) when a
//! Kubernetes `CronJob` runs the `bank_holidays_sync` binary
//! (`src/bin/bank_holidays_sync.rs`: [`sync_once`], then exit non-zero unless
//! the fetch was applied) instead. A rollout of the api then never leaves the
//! loop's lock held by an old pod, because there is no lock. Each api replica
//! still refreshes the two gauges from the table every [`CHECK_EVERY`]
//! ([`gauge_loop`], no lock, no fetch), so `DistantSignalBankHolidaysStale`
//! keeps measuring the table, whoever fills it.

use std::time::Duration as StdDuration;

use anyhow::Context as _;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

use crate::data::bank_holidays::{self, SyncResult, SyncState};

/// GOV.UK's bank holidays JSON.
pub const DEFAULT_URL: &str = "https://www.gov.uk/bank-holidays.json";

/// How often the loop wakes to check whether a fetch is due.
pub const CHECK_EVERY: StdDuration = StdDuration::from_secs(3600);

/// How long after a failed attempt the next one is due. Shorter than
/// [`CHECK_EVERY`], so the next tick after a failure retries.
pub const RETRY_AFTER: Duration = Duration::minutes(50);

/// The whole request's timeout.
pub const FETCH_TIMEOUT: StdDuration = StdDuration::from_secs(30);

/// The largest body read; GOV.UK's is about 22 KB.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

/// The sync's settings, from the environment (chart `api.bankHolidays`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConfig {
    /// `BANK_HOLIDAYS_ENABLED` (default false).
    pub enabled: bool,
    /// `BANK_HOLIDAYS_LOOP_ENABLED` (default true): whether this process
    /// runs the fetch loop. `false` when the `CronJob` fetches; the gauges
    /// are still kept (see [`gauge_loop`]).
    pub loop_enabled: bool,
    /// `BANK_HOLIDAYS_URL` ([`DEFAULT_URL`]).
    pub url: String,
    /// `BANK_HOLIDAYS_INTERVAL_HOURS` (24): how old the last success may
    /// get before the next fetch, at least 1.
    pub interval_hours: u64,
}

/// What an api process does about the sync ([`SyncConfig::mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `BANK_HOLIDAYS_ENABLED` is off: nothing.
    Off,
    /// The fetch loop, under its advisory lock (today's behaviour).
    Loop,
    /// The `CronJob` fetches: only [`gauge_loop`].
    GaugesOnly,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            loop_enabled: true,
            url: DEFAULT_URL.to_string(),
            interval_hours: 24,
        }
    }
}

impl SyncConfig {
    pub fn from_env() -> Self {
        use crate::data::trip_plan_live::{env_bool, env_num};
        let default = Self::default();
        let url = std::env::var("BANK_HOLIDAYS_URL")
            .ok()
            .map(|url| url.trim().to_string())
            .filter(|url| !url.is_empty())
            .unwrap_or(default.url);
        Self {
            enabled: env_bool("BANK_HOLIDAYS_ENABLED", default.enabled),
            loop_enabled: env_bool("BANK_HOLIDAYS_LOOP_ENABLED", default.loop_enabled),
            url,
            interval_hours: env_num("BANK_HOLIDAYS_INTERVAL_HOURS", default.interval_hours).max(1),
        }
    }

    /// What this process does about the sync.
    pub fn mode(&self) -> Mode {
        match (self.enabled, self.loop_enabled) {
            (false, _) => Mode::Off,
            (true, true) => Mode::Loop,
            (true, false) => Mode::GaugesOnly,
        }
    }

    pub fn interval(&self) -> Duration {
        Duration::hours(
            i64::try_from(self.interval_hours)
                .unwrap_or(i64::MAX / 3600)
                .min(24 * 366),
        )
    }

    /// How often the loop wakes: [`CHECK_EVERY`], or the interval if
    /// shorter.
    fn tick(&self) -> StdDuration {
        CHECK_EVERY.min(StdDuration::from_secs(
            self.interval_hours.saturating_mul(3600),
        ))
    }
}

/// Whether a fetch is due at `now`: never attempted; or the last success
/// is `interval` old (or there was none) and the last attempt is at least
/// [`RETRY_AFTER`] old.
pub fn due(state: Option<&SyncState>, now: DateTime<Utc>, interval: Duration) -> bool {
    let Some(state) = state else {
        return true;
    };
    let fresh = state
        .last_success_at
        .is_some_and(|success| now - success < interval);
    !fresh && now - state.last_attempt_at >= RETRY_AFTER.min(interval)
}

/// The HTTP client the sync fetches with.
pub fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .context("build the bank holidays HTTP client")
}

/// One `GET url`: the body, or why it couldn't be read.
async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|err| err.without_url().to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("status {status}"));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return Err(format!("body over the {MAX_BODY_BYTES}-byte limit"));
    }
    let body = response
        .bytes()
        .await
        .map_err(|err| err.without_url().to_string())?;
    if body.len() > MAX_BODY_BYTES {
        return Err(format!("body over the {MAX_BODY_BYTES}-byte limit"));
    }
    Ok(body.to_vec())
}

/// Fetches `url` once and applies a plausible response, recording the
/// attempt either way. A failed or implausible response changes no
/// holiday. `Err` only for a database error.
pub async fn sync_once(
    pool: &PgPool,
    client: &reqwest::Client,
    url: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<SyncResult> {
    let result = match fetch(client, url).await {
        Err(err) => {
            tracing::warn!(error = %err, "bank holidays fetch failed; keeping the stored holidays");
            SyncResult::FetchFailed
        }
        Ok(body) => match bank_holidays::parse(&body) {
            Err(err) => {
                tracing::warn!(error = %err, "bank holidays response refused; keeping the stored holidays");
                SyncResult::Invalid
            }
            Ok(parsed) => {
                let applied = bank_holidays::apply(pool, &parsed, now).await?;
                tracing::info!(
                    upserted = applied.upserted,
                    deleted = applied.deleted,
                    "bank holidays synced"
                );
                SyncResult::Ok
            }
        },
    };
    bank_holidays::record_attempt(pool, result, now).await?;
    metrics::counter!(
        ds_runtime::metrics::metric_name("api_bank_holidays_sync_total"),
        "result" => result.as_str()
    )
    .increment(1);
    Ok(result)
}

/// One tick: fetch if [`due`], then refresh the gauges.
async fn tick(
    pool: &PgPool,
    client: &reqwest::Client,
    config: &SyncConfig,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let state = bank_holidays::sync_state(pool).await?;
    if due(state.as_ref(), now, config.interval()) {
        sync_once(pool, client, &config.url, now).await?;
    }
    refresh_gauges(pool).await
}

/// Sets `api_bank_holidays_last_success_seconds` and
/// `api_bank_holidays_latest_date_seconds{division}` from the tables.
/// Read-only, so any replica may do it, whoever fetched.
pub async fn refresh_gauges(pool: &PgPool) -> anyhow::Result<()> {
    let state = bank_holidays::sync_state(pool).await?;
    if let Some(success) = state.and_then(|state| state.last_success_at) {
        #[expect(clippy::cast_precision_loss, reason = "a Unix time in seconds")]
        metrics::gauge!(ds_runtime::metrics::metric_name(
            "api_bank_holidays_last_success_seconds"
        ))
        .set(success.timestamp() as f64);
    }
    for (division, latest) in bank_holidays::latest_dates(pool).await? {
        let seconds = latest
            .and_time(chrono::NaiveTime::MIN)
            .and_utc()
            .timestamp();
        #[expect(clippy::cast_precision_loss, reason = "a Unix time in seconds")]
        metrics::gauge!(
            ds_runtime::metrics::metric_name("api_bank_holidays_latest_date_seconds"),
            "division" => division
        )
        .set(seconds as f64);
    }
    Ok(())
}

/// With `BANK_HOLIDAYS_LOOP_ENABLED=false` (the `CronJob` fetches): every
/// [`CHECK_EVERY`] until `shutdown`, [`refresh_gauges`]. No advisory lock
/// and no `ds_store::loops` cycle, so the writer-loop alerts have nothing to
/// expect of it and a rollout leaves nothing unowned.
pub async fn gauge_loop(pool: PgPool, shutdown: impl std::future::Future<Output = ()>) {
    tokio::pin!(shutdown);
    let mut interval = tokio::time::interval(CHECK_EVERY);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = &mut shutdown => return,
            _ = interval.tick() => {}
        }
        if let Err(err) = refresh_gauges(&pool).await {
            tracing::warn!(error = ?err, "bank holidays gauges not refreshed; will retry next tick");
        }
    }
}

/// The sync as a `ds_store::loops` loop, under its advisory lock.
pub fn loop_spec(pool: PgPool, config: &SyncConfig) -> anyhow::Result<ds_store::loops::LoopSpec> {
    let config = config.clone();
    let client = client()?;
    Ok(ds_store::loops::LoopSpec::new(
        ds_runtime::advisory_locks::BANK_HOLIDAYS_SYNC,
        config.tick(),
        move |_lock_pool| {
            let pool = pool.clone();
            let client = client.clone();
            let config = config.clone();
            async move {
                let result = tick(&pool, &client, &config, Utc::now()).await;
                if let Err(err) = &result {
                    tracing::error!(error = ?err, "bank holidays sync failed; will retry next tick");
                }
                result
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap() + Duration::hours(hours)
    }

    fn state(attempt: i64, result: &str, success: Option<i64>) -> SyncState {
        SyncState {
            last_attempt_at: at(attempt),
            last_result: result.to_string(),
            last_success_at: success.map(at),
        }
    }

    #[test]
    fn a_fetch_is_due_daily_and_retried_after_a_failure() {
        let day = Duration::hours(24);
        assert!(due(None, at(0), day), "never attempted");
        let ok = state(0, "ok", Some(0));
        assert!(!due(Some(&ok), at(23), day));
        assert!(due(Some(&ok), at(24), day));
        // Failed an hour after a success a day old: retried after 50 min.
        let failed = state(24, "fetch_failed", Some(0));
        assert!(!due(Some(&failed), at(24) + Duration::minutes(49), day));
        assert!(due(Some(&failed), at(25), day));
        // Never succeeded.
        let never = state(0, "invalid", None);
        assert!(!due(Some(&never), at(0) + Duration::minutes(10), day));
        assert!(due(Some(&never), at(1), day));
    }

    #[test]
    fn the_loop_switch_leaves_only_the_gauges() {
        let config = |enabled, loop_enabled| SyncConfig {
            enabled,
            loop_enabled,
            ..SyncConfig::default()
        };
        assert_eq!(SyncConfig::default().mode(), Mode::Off);
        assert_eq!(config(true, true).mode(), Mode::Loop);
        assert_eq!(config(true, false).mode(), Mode::GaugesOnly);
        // The feature switch wins: off is off, with or without the loop.
        assert_eq!(config(false, false).mode(), Mode::Off);
    }

    #[test]
    fn the_config_defaults_and_bounds() {
        let config = SyncConfig::default();
        assert!(!config.enabled);
        assert!(
            config.loop_enabled,
            "the loop stays on until the chart turns it off"
        );
        assert_eq!(config.url, "https://www.gov.uk/bank-holidays.json");
        assert_eq!(config.interval_hours, 24);
        assert_eq!(config.tick(), CHECK_EVERY);
        assert_eq!(config.interval(), Duration::hours(24));
        let huge = SyncConfig {
            interval_hours: u64::MAX,
            ..SyncConfig::default()
        };
        assert_eq!(huge.interval(), Duration::hours(24 * 366));
        assert_eq!(huge.tick(), CHECK_EVERY);
    }
}

#[cfg(test)]
#[path = "bank_holidays_db_tests.rs"]
mod db_tests;
