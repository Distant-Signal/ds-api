//! The `personal_api_tokens` table (migration `20261103120000`): read-only
//! personal API tokens, stored only as the SHA-256 of the token. See
//! `crate::personal_tokens` and docs/specs/personal-api-tokens.md §2, §8.

use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;

/// Why a token ended (`personal_api_tokens.revoked_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    /// Revoke, or Revoke all, on the Connected apps page.
    User,
    /// `POST /public/admin/users/revoke-sessions`, or the operator's SQL.
    Admin,
    /// Unused for `PERSONAL_TOKENS_IDLE_DAYS`.
    Inactive,
    /// Past `expires_at` (set by the sweep; the token was refused from
    /// `expires_at` on either way).
    Expired,
}

impl RevokeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            RevokeReason::User => "user",
            RevokeReason::Admin => "admin",
            RevokeReason::Inactive => "inactive",
            RevokeReason::Expired => "expired",
        }
    }
}

/// An unrevoked token, as the token gate needs it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ActiveToken {
    pub id: i64,
    pub user_id: String,
    pub name: String,
    pub token_hash: Vec<u8>,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// The unrevoked token with this hash, expired or not (the caller decides).
pub async fn find_active_by_hash(pool: &PgPool, hash: &[u8]) -> Result<Option<ActiveToken>> {
    Ok(sqlx::query_as::<_, ActiveToken>(
        "SELECT id, user_id, name, token_hash, scopes, created_at, expires_at, last_used_at \
         FROM personal_api_tokens WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(hash)
    .fetch_optional(pool)
    .await?)
}

/// Records a use, at most once a minute per token (§2.3).
pub async fn touch(pool: &PgPool, token_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE personal_api_tokens SET last_used_at = now() \
         WHERE id = $1 AND revoked_at IS NULL \
           AND (last_used_at IS NULL OR last_used_at < now() - INTERVAL '1 minute')",
    )
    .bind(token_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A token as the Connected apps page and the account export show it.
/// Never carries the hash.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "PersonalTokenRow")]
pub struct TokenRow {
    pub id: i64,
    pub name: String,
    /// The token's last 4 characters, shown as `dsp_...hint`.
    pub hint: String,
    pub scopes: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// The user's tokens that still work: unrevoked and unexpired, newest
/// first.
pub async fn list_active(pool: &PgPool, user_id: &str) -> Result<Vec<TokenRow>> {
    Ok(sqlx::query_as::<_, TokenRow>(
        "SELECT id, name, hint, scopes, created_at, expires_at, last_used_at \
         FROM personal_api_tokens \
         WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now() \
         ORDER BY created_at DESC, id DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

/// What [`create`] did.
#[derive(Debug)]
pub enum CreateOutcome {
    Created(TokenRow),
    /// The user already holds `max_active` working tokens.
    TooMany,
    /// No such user.
    NoUser,
}

/// How long the "a token was created" banner shows (as a new grant's).
pub const NEW_TOKEN_NOTICE_DAYS: i64 = 7;

/// The user's newest unrevoked token created in the last
/// [`NEW_TOKEN_NOTICE_DAYS`], for the banner every page shows.
pub async fn recent_token(pool: &PgPool, user_id: &str) -> Result<Option<TokenRow>> {
    Ok(sqlx::query_as::<_, TokenRow>(
        "SELECT id, name, hint, scopes, created_at, expires_at, last_used_at \
         FROM personal_api_tokens \
         WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now() \
           AND created_at > now() - make_interval(days => $2) \
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(user_id)
    .bind(i32::try_from(NEW_TOKEN_NOTICE_DAYS)?)
    .fetch_optional(pool)
    .await?)
}

/// The columns of a new token row.
#[derive(Debug)]
pub struct NewTokenRow<'a> {
    pub user_id: &'a str,
    pub name: &'a str,
    /// SHA-256 of the token; the token itself is never stored.
    pub hash: &'a [u8],
    pub hint: &'a str,
    pub scopes: &'a [String],
    pub lifetime_days: i64,
}

/// A new token row, unless the user already holds `max_active` working
/// tokens. The count and the insert run under a lock on the user's row,
/// so two concurrent creates cannot both pass the cap.
pub async fn create(
    pool: &PgPool,
    new: &NewTokenRow<'_>,
    max_active: i64,
) -> Result<CreateOutcome> {
    ensure!(new.lifetime_days > 0, "a token's lifetime must be positive");
    let mut tx = pool.begin().await?;
    let found: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(new.user_id)
        .fetch_optional(&mut *tx)
        .await?;
    if found.is_none() {
        tx.rollback().await?;
        return Ok(CreateOutcome::NoUser);
    }
    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM personal_api_tokens \
         WHERE user_id = $1 AND revoked_at IS NULL AND expires_at > now()",
    )
    .bind(new.user_id)
    .fetch_one(&mut *tx)
    .await?;
    if active >= max_active {
        tx.rollback().await?;
        return Ok(CreateOutcome::TooMany);
    }
    let row = sqlx::query_as::<_, TokenRow>(
        "INSERT INTO personal_api_tokens (user_id, name, token_hash, hint, scopes, expires_at) \
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(days => $6)) \
         RETURNING id, name, hint, scopes, created_at, expires_at, last_used_at",
    )
    .bind(new.user_id)
    .bind(new.name)
    .bind(new.hash)
    .bind(new.hint)
    .bind(new.scopes)
    .bind(i32::try_from(new.lifetime_days)?)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(CreateOutcome::Created(row))
}

/// Revokes one token, if it is the user's and still unrevoked. `None` for
/// `user_id` revokes it whoever owns it (the gate's idle revocation).
pub async fn revoke(
    pool: &PgPool,
    token_id: i64,
    user_id: Option<&str>,
    reason: RevokeReason,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE personal_api_tokens SET revoked_at = now(), revoked_reason = $3 \
         WHERE id = $1 AND ($2::text IS NULL OR user_id = $2) AND revoked_at IS NULL",
    )
    .bind(token_id)
    .bind(user_id)
    .bind(reason.as_str())
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Revokes every unrevoked token of `user_id`; returns how many.
pub async fn revoke_all_for_user<'c, E>(
    executor: E,
    user_id: &str,
    reason: RevokeReason,
) -> Result<u64>
where
    E: sqlx::PgExecutor<'c>,
{
    let result = sqlx::query(
        "UPDATE personal_api_tokens SET revoked_at = now(), revoked_reason = $2 \
         WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .bind(reason.as_str())
    .execute(executor)
    .await?;
    Ok(result.rows_affected())
}

/// What one [`sweep`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    pub expired: u64,
    pub inactive: u64,
    pub purged: u64,
}

impl SweepOutcome {
    pub fn total(self) -> u64 {
        self.expired + self.inactive + self.purged
    }
}

/// The hourly pass (§8.5): marks tokens past `expires_at` `expired`,
/// revokes tokens unused for `idle_days` (`inactive`), and deletes rows
/// that ended more than `retention_days` ago. Idempotent.
pub async fn sweep(
    pool: &PgPool,
    now: DateTime<Utc>,
    idle_days: i64,
    retention_days: i64,
) -> Result<SweepOutcome> {
    let expired = sqlx::query(
        "UPDATE personal_api_tokens SET revoked_at = $1, revoked_reason = 'expired' \
         WHERE revoked_at IS NULL AND expires_at <= $1",
    )
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();
    let inactive = sqlx::query(
        "UPDATE personal_api_tokens SET revoked_at = $1, revoked_reason = 'inactive' \
         WHERE revoked_at IS NULL \
           AND COALESCE(last_used_at, created_at) < $1 - make_interval(days => $2)",
    )
    .bind(now)
    .bind(i32::try_from(idle_days)?)
    .execute(pool)
    .await?
    .rows_affected();
    let purged = sqlx::query(
        "DELETE FROM personal_api_tokens \
         WHERE revoked_at IS NOT NULL AND revoked_at < $1 - make_interval(days => $2)",
    )
    .bind(now)
    .bind(i32::try_from(retention_days)?)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(SweepOutcome {
        expired,
        inactive,
        purged,
    })
}

/// [`sweep`], logged, with the `personal_tokens_active` gauge refreshed.
/// Shared by the api's hourly loop and the `maintenance` binary. `None`
/// when it failed (already logged).
pub async fn run_sweep(
    pool: &PgPool,
    now: DateTime<Utc>,
    idle_days: i64,
    retention_days: i64,
) -> Option<SweepOutcome> {
    let outcome = match sweep(pool, now, idle_days, retention_days).await {
        Ok(outcome) => outcome,
        Err(err) => {
            tracing::error!(error = ?err, "personal-token sweep failed; will retry next interval");
            return None;
        }
    };
    for (reason, count) in [
        (RevokeReason::Expired, outcome.expired),
        (RevokeReason::Inactive, outcome.inactive),
    ] {
        if count > 0 {
            tracing::info!(
                audit = true,
                event = "personal_token_revoked",
                reason = reason.as_str(),
                count,
                "personal tokens ended by the sweep"
            );
        }
    }
    if outcome.total() > 0 {
        tracing::info!(?outcome, "personal-token sweep finished");
    }
    match active_count(pool).await {
        #[expect(
            clippy::cast_precision_loss,
            reason = "a token count is far below 2^52"
        )]
        Ok(count) => metrics::gauge!(ds_runtime::metrics::metric_name(
            "api_personal_tokens_active"
        ))
        .set(count as f64),
        Err(err) => tracing::warn!(error = ?err, "could not count active personal tokens"),
    }
    Some(outcome)
}

/// Unrevoked, unexpired tokens, for `distant_signal_api_personal_tokens_active`.
pub async fn active_count(pool: &PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM personal_api_tokens WHERE revoked_at IS NULL AND expires_at > now()",
    )
    .fetch_one(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_match_the_check_constraint() {
        let migration = ds_store::migrate::migration_sql(20_261_103_120_000);
        for reason in [
            RevokeReason::User,
            RevokeReason::Admin,
            RevokeReason::Inactive,
            RevokeReason::Expired,
        ] {
            assert!(
                migration.contains(&format!("'{}'", reason.as_str())),
                "{} is not allowed by personal_api_tokens.revoked_reason's CHECK",
                reason.as_str()
            );
        }
    }
}
