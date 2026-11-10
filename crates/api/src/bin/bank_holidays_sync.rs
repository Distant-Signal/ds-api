//! `bank_holidays_sync`: ONE fetch of GOV.UK's bank holidays, then exit. The
//! chart's `bank-holidays` `CronJob` (`api.bankHolidays.cronjob.enabled`)
//! runs it from the api image, as the api role, in place of the api's
//! in-process loop (`api::bank_holidays_sync`, switched off with
//! `BANK_HOLIDAYS_LOOP_ENABLED=false`):
//!
//! ```text
//!   DATABASE_URL=postgres://... bank_holidays_sync
//! ```
//!
//! It decides as the loop does (`api::bank_holidays_sync::due`, with the
//! loop's settings, `BANK_HOLIDAYS_URL` and `BANK_HOLIDAYS_INTERVAL_HOURS`):
//! when the last success is under the interval old, or the last attempt
//! under 50 minutes old, it exits 0 having fetched nothing, so the chart can
//! run it every few hours and a failed fetch is retried at the next run,
//! while GOV.UK is still fetched about daily. `--force` (or
//! `BANK_HOLIDAYS_FORCE=true`) fetches regardless, for a run by hand. Else it
//! calls `api::bank_holidays_sync::sync_once`. There is no advisory lock (the
//! `CronJob` forbids overlap; a plausible response is applied in one
//! transaction, so two runs, or a run beside an old api pod's loop during the
//! rollout, are harmless).
//!
//! The attempt is recorded in `bank_holidays_sync` either way, as the loop
//! records it. The exit status is 0 only when a plausible response was
//! applied: a failed fetch (transport, status, size) or an implausible
//! response exits 1, failing the Job, which retries within its
//! `backoffLimit`. The stored holidays are never changed by either.
//!
//! It connects with the shared retry (`ds_runtime::startup`), giving up
//! after `BANK_HOLIDAYS_CONNECT_DEADLINE_SECS` (default 120), as
//! `maintenance` does for a pod whose `NetworkPolicy` isn't programmed yet.
//! Metrics are not exported (a Job has nothing to scrape): the api
//! replicas' gauges, read from the table, and the chart's CronJob alerts
//! cover it.

use clap::Parser;
use ds_runtime::secret::Secret;

use api::bank_holidays_sync::{SyncConfig, client, due, sync_once};
use api::data::bank_holidays::{self, SyncResult};

/// `pg_stat_activity.application_name`.
const APPLICATION_NAME: &str = "distant-signal-bank-holidays-sync";
/// One query at a time. Overridable with `DATABASE_MAX_CONNECTIONS`.
const DEFAULT_MAX_CONNECTIONS: u32 = 2;

#[derive(Debug, Parser)]
#[command(name = "bank_holidays_sync")]
struct Args {
    #[arg(long, env, hide_env_values = true)]
    database_url: Secret,
    /// How long to keep retrying the first connection (a saturated node, a
    /// late `NetworkPolicy`) before exiting non-zero. Seconds, at least 1.
    #[arg(long, env = "BANK_HOLIDAYS_CONNECT_DEADLINE_SECS", default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
    connect_deadline_secs: u64,
    /// Fetch even if the last success is recent.
    #[arg(long, env = "BANK_HOLIDAYS_FORCE", default_value_t = false)]
    force: bool,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    ds_runtime::logging::exit_code(run().await)
}

async fn run() -> anyhow::Result<()> {
    let args = Args::parse();
    ds_runtime::logging::init_with_filter(
        "bank-holidays-sync",
        ds_runtime::logging::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| ds_runtime::logging::EnvFilter::new("info")),
    );
    let config = SyncConfig::from_env();
    let client = client()?;
    let settings =
        ds_runtime::pg::PoolSettings::from_env(APPLICATION_NAME, DEFAULT_MAX_CONNECTIONS)?;
    let pool = ds_runtime::startup::retry_until_ready_within(
        "postgres",
        ds_runtime::startup::CONNECT_BACKOFF,
        std::time::Duration::from_secs(args.connect_deadline_secs),
        || settings.connect(args.database_url.expose()),
    )
    .await?;
    let result = run_once(&pool, &client, &config, args.force, chrono::Utc::now()).await;
    pool.close().await;
    match result? {
        Some(result) => outcome(result),
        None => Ok(()),
    }
}

/// [`sync_once`] unless the last attempt says a fetch isn't due (and `force`
/// is off): `None` then, having touched nothing.
async fn run_once(
    pool: &sqlx::PgPool,
    client: &reqwest::Client,
    config: &SyncConfig,
    force: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<Option<SyncResult>> {
    if !force {
        let state = bank_holidays::sync_state(pool).await?;
        if !due(state.as_ref(), now, config.interval()) {
            tracing::info!("bank holidays are fresh; nothing to fetch");
            return Ok(None);
        }
    }
    sync_once(pool, client, &config.url, now).await.map(Some)
}

/// The exit status for a run's result: `Ok` only for an applied response.
fn outcome(result: SyncResult) -> anyhow::Result<()> {
    match result {
        SyncResult::Ok => Ok(()),
        SyncResult::FetchFailed => {
            anyhow::bail!("the bank holidays fetch failed; the stored holidays are unchanged")
        }
        SyncResult::Invalid => anyhow::bail!(
            "the bank holidays response was refused as implausible; the stored holidays are \
             unchanged"
        ),
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn only_an_applied_response_exits_zero() {
        assert!(outcome(SyncResult::Ok).is_ok());
        let failed = outcome(SyncResult::FetchFailed).unwrap_err().to_string();
        assert!(failed.contains("fetch failed"), "{failed}");
        let invalid = outcome(SyncResult::Invalid).unwrap_err().to_string();
        assert!(invalid.contains("implausible"), "{invalid}");
    }

    /// The `CronJob` gets the api's `DATABASE_URL`, read the same way as the
    /// server's.
    #[test]
    fn the_database_url_matches_the_api_server() {
        let find = |command: &clap::Command| {
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_id() == "database_url")
                .expect("no database_url")
                .clone();
            (
                arg.get_env().map(std::ffi::OsStr::to_os_string),
                arg.get_default_values().to_vec(),
            )
        };
        assert_eq!(
            find(&Args::command()),
            find(&api::data::config::ServiceArguments::command())
        );
    }

    #[test]
    fn the_connect_deadline_reads_its_variable() {
        let arg = Args::command()
            .get_arguments()
            .find(|arg| arg.get_id() == "connect_deadline_secs")
            .cloned()
            .unwrap();
        assert_eq!(
            arg.get_env(),
            Some(std::ffi::OsStr::new("BANK_HOLIDAYS_CONNECT_DEADLINE_SECS"))
        );
        assert_eq!(arg.get_default_values(), ["120"]);
        let url = "--database-url=postgres://unused";
        let parsed =
            Args::try_parse_from(["bank_holidays_sync", url, "--connect-deadline-secs=30"]);
        assert_eq!(parsed.unwrap().connect_deadline_secs, 30);
        assert!(
            Args::try_parse_from(["bank_holidays_sync", url, "--connect-deadline-secs=0"]).is_err()
        );
    }
}
