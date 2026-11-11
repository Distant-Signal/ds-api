//! Recently deleted journeys (DS-474): what a deleted journey was, kept a
//! few days so its owner can save it again.
//!
//! It reuses `delegated_deletions`, the 30-day record of a connected app's
//! deletes (`data::delegated`, docs/connected-apps.md), rather than a table
//! of its own. With [`ENABLED_ENV`] on, the user's own deletes are recorded
//! there too, with `client` [`WEB`] and no grant: `DELETE /Journeys/{id}`
//! and `DELETE /Train/{trackingId}` (a tracked train is a one-leg journey)
//! write the row in the delete's own transaction
//! (`journeys::delete_journey_row_releasing_trains`). Retention deletes and
//! account deletion record nothing.
//!
//! A row holds the journey's name and, per leg, its kind, date, stations
//! or places, train UID, mode, line and planned times: what "Save again"
//! needs and no more (`journey_summary`). Nothing else of the journey is
//! kept, so saving it again brings back its shape, not its history:
//! revisions, breaks, tickets, share links, follows, group shares, its
//! regular journey link and its settings stay gone.
//!
//! How long a row lives:
//!
//! - the user's own deletes: [`RETENTION_DAYS_ENV`] days, 7 by default,
//!   7 to 31 allowed;
//! - a connected app's: 30 days ([`delegated::DELETION_RETENTION_DAYS`]),
//!   as before. They stay on the Connected apps page, and the Recently
//!   deleted list shows them as well, so they can be saved again too.
//!
//! The hourly sweep (`delegated::sweep`) deletes rows past their period,
//! whether or not the feature is on, so turning it off never strands one.
//! The user can delete one row, or every row, at once. Rows cascade with
//! the account (`ON DELETE CASCADE` on `users(id)`), and the export lists
//! them (`data::account`).
//!
//! With the flag off nothing is recorded, the routes are `404` and
//! `GET /auth/session` says `recentlyDeletedEnabled: false`.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{PgConnection, PgPool};

use crate::data::delegated;

/// The environment variable that turns the feature on (chart
/// `api.recentlyDeleted.enabled`, default false).
pub const ENABLED_ENV: &str = "RECENTLY_DELETED_ENABLED";
/// Days the user's own deletes are kept (chart
/// `api.recentlyDeleted.retentionDays`).
pub const RETENTION_DAYS_ENV: &str = "RECENTLY_DELETED_RETENTION_DAYS";
/// The default for [`RETENTION_DAYS_ENV`].
pub const DEFAULT_RETENTION_DAYS: i64 = 7;
/// The shortest [`RETENTION_DAYS_ENV`] allowed.
pub const MIN_RETENTION_DAYS: i64 = 7;
/// The longest [`RETENTION_DAYS_ENV`] allowed.
pub const MAX_RETENTION_DAYS: i64 = 31;
/// `delegated_deletions.client` of the user's own deletes, as
/// `journeys.created_via` names a journey the user saved themselves.
pub const WEB: &str = "web";
/// The most rows the list returns.
pub const MAX_LISTED: i64 = 200;

static ENABLED: AtomicBool = AtomicBool::new(false);
static RETENTION_DAYS: AtomicI64 = AtomicI64::new(DEFAULT_RETENTION_DAYS);

/// The two settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub retention_days: i64,
}

impl Settings {
    /// Reads [`ENABLED_ENV`] (default false; an unreadable value is false,
    /// with a warning, as for the other feature switches) and
    /// [`RETENTION_DAYS_ENV`] (default 7; anything but a whole number from
    /// 7 to 31 is an error, so a bad value stops the api rather than
    /// keeping deleted journeys longer than the privacy notice says).
    pub fn from_env() -> Result<Self> {
        let enabled = crate::data::trip_plan_live::env_bool(ENABLED_ENV, false);
        let retention_days =
            parse_retention_days(std::env::var(RETENTION_DAYS_ENV).ok().as_deref())?;
        Ok(Self {
            enabled,
            retention_days,
        })
    }
}

/// [`RETENTION_DAYS_ENV`]'s value: `None` (unset) or blank is the default.
pub fn parse_retention_days(raw: Option<&str>) -> Result<i64> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(DEFAULT_RETENTION_DAYS);
    };
    let days: i64 = raw.parse().with_context(|| {
        format!("{RETENTION_DAYS_ENV} must be a whole number of days, not {raw:?}")
    })?;
    ensure!(
        (MIN_RETENTION_DAYS..=MAX_RETENTION_DAYS).contains(&days),
        "{RETENTION_DAYS_ENV} must be from {MIN_RETENTION_DAYS} to {MAX_RETENTION_DAYS} days, not {days}"
    );
    Ok(days)
}

/// Reads the settings once, at startup, and keeps them for [`enabled`] and
/// [`retention_days`].
pub fn init_from_env() -> Result<Settings> {
    let settings = Settings::from_env()?;
    ENABLED.store(settings.enabled, Ordering::Relaxed);
    RETENTION_DAYS.store(settings.retention_days, Ordering::Relaxed);
    Ok(settings)
}

/// Whether the user's own deletes are recorded and the list is served.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Days the user's own deletes are kept.
pub fn retention_days() -> i64 {
    RETENTION_DAYS.load(Ordering::Relaxed)
}

/// Sets both for the whole test process; a DB test that needs the feature
/// says so first.
#[cfg(test)]
pub(crate) fn set_for_tests(enabled: bool, retention_days: i64) {
    ENABLED.store(enabled, Ordering::Relaxed);
    RETENTION_DAYS.store(retention_days, Ordering::Relaxed);
}

/// The SQL for how many days a row of `client` lives: `$web` for the
/// user's own, [`delegated::DELETION_RETENTION_DAYS`] for an app's.
fn kept_days_sql(web_days_param: &str) -> String {
    format!(
        "(CASE WHEN client = '{WEB}' THEN {web_days_param} ELSE {} END)",
        delegated::DELETION_RETENTION_DAYS
    )
}

/// What journey `journey_id` is, as a `delegated_deletions.summary`:
/// `{customName, legs: [...]}` with each leg's kind, date, stations,
/// train UID, mode, line, places, stops and planned times, in order
/// (`crate::ts_values::DeletedJourneySummary`). `None` when there is no
/// such journey. Read on the caller's transaction, after it locked the
/// journey and its legs.
pub(crate) async fn journey_summary(
    conn: &mut PgConnection,
    journey_id: i64,
) -> Result<Option<serde_json::Value>> {
    Ok(sqlx::query_scalar(
        "SELECT jsonb_build_object('customName', j.custom_name, 'legs', ( \
             SELECT COALESCE(jsonb_agg(jsonb_build_object( \
                 'serviceDate', jl.service_date, 'originCrs', jl.origin_crs, \
                 'destinationCrs', jl.destination_crs, 'trainUid', tr.train_uid, \
                 'kind', jl.kind, 'transitMode', jl.transit_mode, \
                 'originName', jl.origin_place, 'destinationName', jl.destination_place, \
                 'lineName', jl.line_name, 'plannedDeparture', jl.planned_departure, \
                 'plannedArrival', jl.planned_arrival, \
                 'originStopId', jl.origin_stop_id, 'destinationStopId', jl.destination_stop_id) \
                 ORDER BY jl.leg_order), '[]'::jsonb) \
             FROM journey_legs jl \
             LEFT JOIN train_subscriptions ts ON ts.id = jl.train_subscription_id \
             LEFT JOIN trains tr ON tr.id = ts.trains_id \
             WHERE jl.journey_id = j.id)) \
         FROM journeys j WHERE j.id = $1",
    )
    .bind(journey_id)
    .fetch_optional(&mut *conn)
    .await?)
}

/// Records `user_id`'s own delete of journey `journey_id` on the caller's
/// transaction, before the journey row goes. Does nothing for a journey
/// that is not there.
pub(crate) async fn record_web_deletion(
    conn: &mut PgConnection,
    user_id: &str,
    journey_id: i64,
) -> Result<()> {
    let Some(summary) = journey_summary(&mut *conn, journey_id).await? else {
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO delegated_deletions (user_id, client, grant_id, journey_id, summary) \
         VALUES ($1, $2, NULL, $3, $4)",
    )
    .bind(user_id)
    .bind(WEB)
    .bind(journey_id)
    .bind(&summary)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// One recently deleted journey, as the list shows it.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct RecentlyDeletedJourney {
    pub id: i64,
    /// The deleted journey's id. It no longer exists; a journey saved again
    /// gets a new one.
    pub journey_id: i64,
    /// `web` for the user's own delete, else the connected app's name.
    pub client: String,
    pub deleted_at: DateTime<Utc>,
    /// When the sweep deletes the row for good.
    pub expires_at: DateTime<Utc>,
    #[ts(as = "crate::ts_values::DeletedJourneySummary")]
    pub summary: serde_json::Value,
}

/// `user_id`'s recently deleted journeys still in their period, newest
/// first, at most [`MAX_LISTED`].
pub async fn list(
    pool: &PgPool,
    user_id: &str,
    web_retention_days: i64,
) -> Result<Vec<RecentlyDeletedJourney>> {
    let kept = kept_days_sql("$2");
    Ok(sqlx::query_as::<_, RecentlyDeletedJourney>(&format!(
        "SELECT id, journey_id, client, deleted_at, \
                deleted_at + make_interval(days => {kept}) AS expires_at, summary \
         FROM delegated_deletions \
         WHERE user_id = $1 AND deleted_at > now() - make_interval(days => {kept}) \
         ORDER BY deleted_at DESC, id DESC LIMIT $3"
    ))
    .bind(user_id)
    .bind(i32::try_from(web_retention_days)?)
    .bind(MAX_LISTED)
    .fetch_all(pool)
    .await?)
}

/// One of `user_id`'s rows still in its period: `(client, summary)`.
pub async fn get(
    pool: &PgPool,
    user_id: &str,
    id: i64,
    web_retention_days: i64,
) -> Result<Option<(String, serde_json::Value)>> {
    let kept = kept_days_sql("$3");
    Ok(sqlx::query_as(&format!(
        "SELECT client, summary FROM delegated_deletions \
         WHERE id = $1 AND user_id = $2 AND deleted_at > now() - make_interval(days => {kept})"
    ))
    .bind(id)
    .bind(user_id)
    .bind(i32::try_from(web_retention_days)?)
    .fetch_optional(pool)
    .await?)
}

/// Takes one of `user_id`'s rows still in its period off the list, on the
/// caller's transaction, for "Save again": whichever of two concurrent
/// saves deletes it first goes ahead, the other finds nothing.
pub async fn take(
    conn: &mut PgConnection,
    user_id: &str,
    id: i64,
    web_retention_days: i64,
) -> Result<bool> {
    let kept = kept_days_sql("$3");
    let taken: Option<i64> = sqlx::query_scalar(&format!(
        "DELETE FROM delegated_deletions \
         WHERE id = $1 AND user_id = $2 AND deleted_at > now() - make_interval(days => {kept}) \
         RETURNING id"
    ))
    .bind(id)
    .bind(user_id)
    .bind(i32::try_from(web_retention_days)?)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(taken.is_some())
}

/// "Delete permanently": deletes one of `user_id`'s rows. Whether it was
/// there.
pub async fn delete_one(pool: &PgPool, user_id: &str, id: i64) -> Result<bool> {
    let result = sqlx::query("DELETE FROM delegated_deletions WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// "Empty now": deletes every one of `user_id`'s rows, the connected apps'
/// included (the list shows them). How many went.
pub async fn delete_all(pool: &PgPool, user_id: &str) -> Result<u64> {
    let result = sqlx::query("DELETE FROM delegated_deletions WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_retention_defaults_to_seven_days() {
        assert_eq!(parse_retention_days(None).unwrap(), 7);
        assert_eq!(parse_retention_days(Some("")).unwrap(), 7);
        assert_eq!(parse_retention_days(Some("  ")).unwrap(), 7);
    }

    #[test]
    fn the_retention_takes_seven_to_thirty_one_days() {
        assert_eq!(parse_retention_days(Some("7")).unwrap(), 7);
        assert_eq!(parse_retention_days(Some(" 14 ")).unwrap(), 14);
        assert_eq!(parse_retention_days(Some("31")).unwrap(), 31);
        for bad in ["6", "0", "-7", "32", "365", "7.5", "seven"] {
            assert!(parse_retention_days(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_app_s_deletes_keep_their_thirty_days() {
        assert_eq!(
            kept_days_sql("$2"),
            "(CASE WHEN client = 'web' THEN $2 ELSE 30 END)"
        );
    }

    /// `web` is a name `delegated_deletions.client`'s CHECK allows, so the
    /// user's own deletes need no migration.
    #[test]
    fn web_passes_the_client_check() {
        let migration = ds_store::migrate::migration_sql(20_261_015_010_000);
        assert!(migration.contains("client TEXT NOT NULL CHECK (client ~ '^[a-z0-9-]{1,32}$')"));
        assert!(WEB.len() <= 32 && WEB.bytes().all(|b| b.is_ascii_lowercase()));
    }
}
