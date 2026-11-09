//! The `webhook_destinations` table (ds-core migration `20261109120000`):
//! a user's own outbound webhook destinations (DS-415). The api creates,
//! lists, switches and deletes them; the notifier delivers to them and
//! records their health (ds-workers' `webhook` module).
//!
//! Presets: `generic` (signed JSON; the api generates the signing secret and
//! shows it once), `ntfy` (an optional bearer token) and `gotify` (the app
//! token). A secret is never returned after the create response.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ds_runtime::secret_box::{KeyRing, Sealed};
use rand::Rng;
use rand::rngs::OsRng;
use serde::Serialize;
use sqlx::PgPool;

/// The environment variable that turns the routes on.
pub const WEBHOOKS_ENABLED_ENV: &str = "WEBHOOKS_ENABLED";

/// Destinations one user may have.
pub const MAX_PER_USER: i64 = 5;

/// The longest name, URL and secret (the migration's CHECKs).
pub const MAX_NAME_CHARS: usize = 60;
pub const MAX_URL_CHARS: usize = 2048;
pub const MAX_SECRET_CHARS: usize = 512;

/// The prefix of a generated signing secret.
const SECRET_PREFIX: &str = "whsec_";
const SECRET_LEN: usize = 40;
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

static ENABLED: AtomicBool = AtomicBool::new(false);

/// The environment variable holding the key ring that encrypts destination
/// secrets (`<id>:<base64 of 32 bytes>[,<id>:<base64>...]`, the first key
/// encrypts; see `ds_runtime::secret_box`).
pub const SECRET_KEYS_ENV: &str = "WEBHOOK_SECRET_KEYS";

static KEYS: OnceLock<KeyRing> = OnceLock::new();

/// Reads [`WEBHOOKS_ENABLED_ENV`] (default false) and [`SECRET_KEYS_ENV`]
/// once, at startup, and returns whether the routes are on. Enabled without
/// a usable key ring they stay off, with an error in the log: a secret is
/// never stored unencrypted.
pub fn init_from_env() -> bool {
    let requested = crate::data::trip_plan_live::env_bool(WEBHOOKS_ENABLED_ENV, false);
    let enabled = requested && init_keys(std::env::var(SECRET_KEYS_ENV).ok().as_deref());
    ENABLED.store(enabled, Ordering::Relaxed);
    enabled
}

fn init_keys(value: Option<&str>) -> bool {
    match value.map(KeyRing::parse) {
        Some(Ok(ring)) => {
            tracing::info!(key_ids = ?ring, "webhook secret keys loaded");
            KEYS.set(ring).is_ok() || KEYS.get().is_some()
        }
        Some(Err(err)) => {
            tracing::error!(
                error = %err,
                env = SECRET_KEYS_ENV,
                "{WEBHOOKS_ENABLED_ENV} is on but {SECRET_KEYS_ENV} is not usable; webhooks stay disabled"
            );
            false
        }
        None => {
            tracing::error!(
                env = SECRET_KEYS_ENV,
                "{WEBHOOKS_ENABLED_ENV} is on but {SECRET_KEYS_ENV} is not set; webhooks stay disabled"
            );
            false
        }
    }
}

/// Encrypts a destination's secret with the active key. `None` when there
/// is no key ring (the routes are off then, so this is not reached).
pub fn seal(secret: &str) -> Option<Sealed> {
    KEYS.get()?.seal(secret).ok()
}

/// Whether the webhook routes are on ([`init_from_env`]).
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// A preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Generic,
    Ntfy,
    Gotify,
}

impl Kind {
    pub fn parse(kind: &str) -> Option<Kind> {
        match kind {
            "generic" => Some(Kind::Generic),
            "ntfy" => Some(Kind::Ntfy),
            "gotify" => Some(Kind::Gotify),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Generic => "generic",
            Kind::Ntfy => "ntfy",
            Kind::Gotify => "gotify",
        }
    }
}

/// A fresh signing secret from the OS RNG (about 238 bits).
pub fn generate_secret() -> String {
    let mut rng = OsRng;
    let random: String = (0..SECRET_LEN)
        .map(|_| char::from(ALPHABET[rng.gen_range(0..ALPHABET.len())]))
        .collect();
    format!("{SECRET_PREFIX}{random}")
}

/// A destination as the settings page shows it. Never carries the secret,
/// only whether one is set.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "WebhookDestination")]
pub struct WebhookRow {
    pub id: i64,
    /// `generic`, `ntfy` or `gotify`.
    pub kind: String,
    pub name: String,
    pub url: String,
    /// Whether a secret (a signing secret, or a token) is stored.
    pub has_secret: bool,
    pub enabled: bool,
    /// Why it was turned off, when the notifier did that after repeated
    /// failures.
    pub disabled_reason: Option<String>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_failure_at: Option<DateTime<Utc>>,
    /// A short reason for the last failed delivery.
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, kind, name, url, secret_ciphertext IS NOT NULL AS has_secret, enabled, \
                       disabled_reason, last_success_at, last_failure_at, last_error, created_at";

/// The user's destinations, oldest first.
pub async fn list(pool: &PgPool, user_id: &str) -> Result<Vec<WebhookRow>> {
    Ok(sqlx::query_as::<_, WebhookRow>(&format!(
        "SELECT {COLUMNS} FROM webhook_destinations WHERE user_id = $1 ORDER BY id"
    ))
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// The columns of a new destination.
#[derive(Debug)]
pub struct NewWebhook<'a> {
    pub user_id: &'a str,
    pub kind: Kind,
    pub name: &'a str,
    pub url: &'a str,
    /// The secret, already encrypted ([`seal`]).
    pub secret: Option<&'a Sealed>,
}

/// What [`create`] did.
#[derive(Debug)]
pub enum CreateOutcome {
    Created(WebhookRow),
    /// The user already has [`MAX_PER_USER`].
    TooMany,
    /// The user already has a destination at this URL.
    Duplicate,
    /// No such user.
    NoUser,
}

/// A new destination, unless the user is at the cap or already has the
/// URL. The count and the insert run under a lock on the user's row, so two
/// concurrent creates cannot both pass the cap.
pub async fn create(pool: &PgPool, new: &NewWebhook<'_>) -> Result<CreateOutcome> {
    let mut tx = pool.begin().await?;
    let found: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(new.user_id)
        .fetch_optional(&mut *tx)
        .await?;
    if found.is_none() {
        tx.rollback().await?;
        return Ok(CreateOutcome::NoUser);
    }
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM webhook_destinations WHERE user_id = $1")
            .bind(new.user_id)
            .fetch_one(&mut *tx)
            .await?;
    if count >= MAX_PER_USER {
        tx.rollback().await?;
        return Ok(CreateOutcome::TooMany);
    }
    let inserted = sqlx::query_as::<_, WebhookRow>(&format!(
        "INSERT INTO webhook_destinations \
           (user_id, kind, name, url, secret_ciphertext, secret_nonce, secret_key_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (user_id, url) DO NOTHING RETURNING {COLUMNS}"
    ))
    .bind(new.user_id)
    .bind(new.kind.as_str())
    .bind(new.name)
    .bind(new.url)
    .bind(new.secret.map(|s| s.ciphertext.as_slice()))
    .bind(new.secret.map(|s| s.nonce.as_slice()))
    .bind(new.secret.map(|s| s.key_id.as_str()))
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = inserted else {
        tx.rollback().await?;
        return Ok(CreateOutcome::Duplicate);
    };
    tx.commit().await?;
    Ok(CreateOutcome::Created(row))
}

/// Deletes the user's destination; whether there was one.
pub async fn delete(pool: &PgPool, id: i64, user_id: &str) -> Result<bool> {
    let result = sqlx::query("DELETE FROM webhook_destinations WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Turns the user's destination on or off. Turning one on clears its
/// failure run and reason, so it gets a fresh start.
pub async fn set_enabled(
    pool: &PgPool,
    id: i64,
    user_id: &str,
    enabled: bool,
) -> Result<Option<WebhookRow>> {
    Ok(sqlx::query_as::<_, WebhookRow>(&format!(
        "UPDATE webhook_destinations SET enabled = $3, \
           consecutive_failures = CASE WHEN $3 THEN 0 ELSE consecutive_failures END, \
           disabled_reason = CASE WHEN $3 THEN NULL ELSE disabled_reason END \
         WHERE id = $1 AND user_id = $2 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(user_id)
    .bind(enabled)
    .fetch_optional(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_has_its_prefix_and_length() {
        let secret = generate_secret();
        assert!(secret.starts_with("whsec_"));
        assert_eq!(secret.len(), SECRET_PREFIX.len() + SECRET_LEN);
        assert_ne!(secret, generate_secret());
    }

    #[test]
    fn webhooks_stay_off_without_a_usable_key_ring() {
        assert!(!init_keys(None));
        assert!(!init_keys(Some("not-a-ring")));
        assert!(!init_keys(Some("")));
    }

    #[test]
    fn kinds_round_trip() {
        for kind in [Kind::Generic, Kind::Ntfy, Kind::Gotify] {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(Kind::parse("discord"), None);
    }

    /// The cap, the duplicate check, ownership and the re-enable reset
    /// against a live database (`DATABASE_URL`).
    #[tokio::test]
    #[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                webhook -- --ignored`"]
    async fn destinations_obey_the_cap_ownership_and_reenable_reset() {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        let pool = PgPool::connect(&url).await.unwrap();
        let (a, b) = ("TEST-WEBHOOK-API-A", "TEST-WEBHOOK-API-B");
        let _cleanup = ds_store::test_support::FixtureCleanup::new(
            &pool,
            [format!("DELETE FROM users WHERE id IN ('{a}', '{b}')")],
        )
        .await;
        for user in [a, b] {
            sqlx::query("INSERT INTO users (id, name) VALUES ($1, $1) ON CONFLICT DO NOTHING")
                .bind(user)
                .execute(&pool)
                .await
                .unwrap();
        }
        let ring = KeyRing::parse(&format!("k1:{}=", "A".repeat(43))).unwrap();
        let sealed = Box::leak(Box::new(ring.seal("whsec_x").unwrap()));
        let new = |user_id: &'static str, url: &'static str| NewWebhook {
            user_id,
            kind: Kind::Generic,
            name: "n",
            url,
            secret: Some(sealed),
        };
        let mut first = None;
        for n in 0..MAX_PER_USER {
            let url: &'static str = Box::leak(format!("https://h.example/{n}").into_boxed_str());
            match create(&pool, &new(a, url)).await.unwrap() {
                CreateOutcome::Created(row) => {
                    assert!(row.has_secret && row.enabled);
                    first.get_or_insert(row.id);
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(matches!(
            create(&pool, &new(a, "https://h.example/extra"))
                .await
                .unwrap(),
            CreateOutcome::TooMany
        ));
        // The cap is per user, and the same URL is fine for another user.
        assert!(matches!(
            create(&pool, &new(b, "https://h.example/0")).await.unwrap(),
            CreateOutcome::Created(_)
        ));
        assert!(matches!(
            create(&pool, &new(b, "https://h.example/0")).await.unwrap(),
            CreateOutcome::Duplicate
        ));
        let first = first.unwrap();
        // Another user can neither change nor delete it.
        assert!(set_enabled(&pool, first, b, false).await.unwrap().is_none());
        assert!(!delete(&pool, first, b).await.unwrap());
        // Re-enabling clears the notifier's failure run and reason.
        sqlx::query(
            "UPDATE webhook_destinations SET enabled = FALSE, consecutive_failures = 8, \
             disabled_reason = 'Turned off after repeated failures' WHERE id = $1",
        )
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
        let row = set_enabled(&pool, first, a, true).await.unwrap().unwrap();
        assert!(row.enabled && row.disabled_reason.is_none());
        let failures: i32 = sqlx::query_scalar(
            "SELECT consecutive_failures FROM webhook_destinations WHERE id = $1",
        )
        .bind(first)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(failures, 0);
        assert!(delete(&pool, first, a).await.unwrap());
        assert_eq!(
            list(&pool, a).await.unwrap().len(),
            usize::try_from(MAX_PER_USER - 1).unwrap()
        );
    }
}
