//! `backfill_secret_box`: stage 3 of the secret box AAD rollout (DS-615,
//! ds-core's docs/secret-box-aad.md, and this repository's
//! docs/secret-box-backfill.md). One run of the api image as a Kubernetes
//! Job, with the api's database role and `WEBHOOK_SECRET_KEYS`:
//!
//! ```text
//!   DATABASE_URL=postgres://... WEBHOOK_SECRET_KEYS=... \
//!       backfill_secret_box --confirm-v2-readers [--dry-run]
//! ```
//!
//! It opens every `webhook_destinations` secret still sealed in the legacy
//! format (a 12-byte nonce) and seals it again as v2, bound to its row id,
//! with the ring's active key (`api::data::secret_box_backfill`). Nothing
//! runs it on its own: it is inert until someone starts the Job.
//!
//! Preconditions, which the Job cannot all see: ds-core v0.18.2's stage 2
//! migrations are applied (the run refuses, saying why, unless the nonce
//! CHECK is validated and allows 13 bytes), and every reader of the column
//! (this api, the notifier, any other Job) runs code that opens v2.
//! `--confirm-v2-readers` (or `BACKFILL_CONFIRM_V2_READERS=true`) says the
//! operator has checked that second one; without it the run refuses.
//!
//! The exit status is 0 only when no legacy row remains (a dry run: when
//! every one opens). A row that does not open is left untouched and fails
//! the run, naming its id. Logs are counts and ids only; no secret, key or
//! ciphertext is ever printed.

use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::Parser;
use ds_runtime::secret::Secret;
use ds_runtime::secret_box::KeyRing;

use api::data::secret_box_backfill::{DEFAULT_BATCH_SIZE, MAX_BATCH_SIZE, Options, run};
use api::data::webhooks::SECRET_KEYS_ENV;

/// `pg_stat_activity.application_name`.
const APPLICATION_NAME: &str = "distant-signal-backfill-secret-box";
/// One batch at a time. Overridable with `DATABASE_MAX_CONNECTIONS`.
const DEFAULT_MAX_CONNECTIONS: u32 = 2;

#[derive(Debug, Parser)]
#[command(name = "backfill_secret_box")]
struct Args {
    #[arg(long, env, hide_env_values = true)]
    database_url: Secret,
    /// The key ring, as the api reads it (`<id>:<base64>[,...]`, the first
    /// key seals). It must hold every key a legacy secret was sealed with.
    #[arg(long, env = "WEBHOOK_SECRET_KEYS", hide_env_values = true)]
    webhook_secret_keys: Option<Secret>,
    /// The operator has confirmed every reader of the secret columns opens
    /// v2 values.
    #[arg(long, env = "BACKFILL_CONFIRM_V2_READERS", default_value_t = false)]
    confirm_v2_readers: bool,
    /// Open every legacy row and report, writing nothing.
    #[arg(long, env = "BACKFILL_DRY_RUN", default_value_t = false)]
    dry_run: bool,
    /// Rows per batch (and per transaction).
    #[arg(long, env = "BACKFILL_BATCH_SIZE", default_value_t = DEFAULT_BATCH_SIZE, value_parser = clap::value_parser!(i64).range(1..=MAX_BATCH_SIZE))]
    batch_size: i64,
    /// Milliseconds to sleep after each batch.
    #[arg(long, env = "BACKFILL_PAUSE_MS", default_value_t = 100)]
    pause_ms: u64,
    /// How long to keep retrying the first connection before exiting
    /// non-zero. Seconds, at least 1.
    #[arg(long, env = "BACKFILL_CONNECT_DEADLINE_SECS", default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..))]
    connect_deadline_secs: u64,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    ds_runtime::logging::exit_code(execute().await)
}

async fn execute() -> anyhow::Result<()> {
    let args = Args::parse();
    ds_runtime::logging::init_with_filter(
        "backfill-secret-box",
        ds_runtime::logging::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| ds_runtime::logging::EnvFilter::new("info")),
    );
    if !args.confirm_v2_readers {
        bail!(
            "refusing to run: pass --confirm-v2-readers (or BACKFILL_CONFIRM_V2_READERS=true) \
             once the api and the notifier in production run code that opens v2 secrets \
             (docs/secret-box-backfill.md)"
        );
    }
    let keys = args
        .webhook_secret_keys
        .as_ref()
        .with_context(|| format!("{SECRET_KEYS_ENV} is not set"))?;
    let ring = KeyRing::parse(keys.expose())
        .map_err(|err| anyhow::anyhow!("{SECRET_KEYS_ENV} is not usable: {err}"))?;
    tracing::info!(active_key = ring.active_id(), "webhook secret keys loaded");
    let settings =
        ds_runtime::pg::PoolSettings::from_env(APPLICATION_NAME, DEFAULT_MAX_CONNECTIONS)?;
    let pool = ds_runtime::startup::retry_until_ready_within(
        "postgres",
        ds_runtime::startup::CONNECT_BACKOFF,
        Duration::from_secs(args.connect_deadline_secs),
        || settings.connect(args.database_url.expose()),
    )
    .await?;
    let options = Options {
        batch_size: args.batch_size,
        pause: Duration::from_millis(args.pause_ms),
        dry_run: args.dry_run,
    };
    let result = run(&pool, &ring, options).await;
    pool.close().await;
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    /// The Job gets the api's `DATABASE_URL`, read the same way as the
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
    fn the_key_ring_reads_the_apis_variable() {
        let arg = Args::command()
            .get_arguments()
            .find(|arg| arg.get_id() == "webhook_secret_keys")
            .cloned()
            .unwrap();
        assert_eq!(arg.get_env(), Some(std::ffi::OsStr::new(SECRET_KEYS_ENV)));
    }

    #[test]
    fn the_batch_size_is_bounded() {
        let url = "--database-url=postgres://unused";
        assert!(Args::try_parse_from(["backfill_secret_box", url, "--batch-size=0"]).is_err());
        assert!(Args::try_parse_from(["backfill_secret_box", url, "--batch-size=1001"]).is_err());
        let parsed = Args::try_parse_from(["backfill_secret_box", url, "--batch-size=50"]);
        assert_eq!(parsed.unwrap().batch_size, 50);
    }

    #[test]
    fn it_does_not_confirm_the_readers_on_its_own() {
        let url = "--database-url=postgres://unused";
        let parsed = Args::try_parse_from(["backfill_secret_box", url]).unwrap();
        assert!(!parsed.confirm_v2_readers);
        assert!(!parsed.dry_run);
    }
}
