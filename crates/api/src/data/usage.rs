//! Opt-in usage analytics storage (DS-190; `crate::usage_events`).
//!
//! `usage_daily` holds one count per day, event, dimension and signed-in
//! or not: no user id, IP address, user agent or session id, so a row can
//! never be tied back to anyone. `users.analytics_opt_in` is a signed-in
//! user's choice.

use anyhow::Result;
use chrono::{DateTime, Months, NaiveDate, Utc};
use sqlx::PgPool;

/// How long daily counts are kept: 13 months, so a month can be compared
/// with the same month a year earlier.
pub const RETENTION_MONTHS: u32 = 13;

/// Adds one to `(day, event, dim, signed_in)`.
pub async fn increment(
    pool: &PgPool,
    day: NaiveDate,
    event: &str,
    dim: &str,
    signed_in: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO usage_daily (day, event, dim, signed_in, count) VALUES ($1, $2, $3, $4, 1) \
         ON CONFLICT (day, event, dim, signed_in) DO UPDATE SET count = usage_daily.count + 1",
    )
    .bind(day)
    .bind(event)
    .bind(dim)
    .bind(signed_in)
    .execute(pool)
    .await?;
    Ok(())
}

/// The first London day still kept as of `now`: days before it go.
pub fn retention_cutoff(now: DateTime<Utc>) -> NaiveDate {
    let today = now.with_timezone(&chrono_tz::Europe::London).date_naive();
    today
        .checked_sub_months(Months::new(RETENTION_MONTHS))
        .unwrap_or(NaiveDate::MIN)
}

/// Deletes days older than [`RETENTION_MONTHS`]; returns the rows deleted.
pub async fn prune(pool: &PgPool, now: DateTime<Utc>) -> Result<u64> {
    Ok(sqlx::query("DELETE FROM usage_daily WHERE day < $1")
        .bind(retention_cutoff(now))
        .execute(pool)
        .await?
        .rows_affected())
}

/// A user's analytics choice; `None` if there is no such user.
pub async fn opt_in(pool: &PgPool, user_id: &str) -> Result<Option<bool>> {
    Ok(
        sqlx::query_scalar("SELECT analytics_opt_in FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?,
    )
}

/// Sets a user's analytics choice; `false` if there is no such user.
pub async fn set_opt_in(pool: &PgPool, user_id: &str, opt_in: bool) -> Result<bool> {
    Ok(
        sqlx::query("UPDATE users SET analytics_opt_in = $2 WHERE id = $1")
            .bind(user_id)
            .bind(opt_in)
            .execute(pool)
            .await?
            .rows_affected()
            > 0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cutoff_is_thirteen_months_before_the_london_date() {
        // 23:30 UTC on 2026-06-30 is already 1 July in London (BST).
        let now = "2026-06-30T23:30:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            retention_cutoff(now),
            NaiveDate::from_ymd_opt(2025, 6, 1).unwrap()
        );
        // Month ends clamp.
        let now = "2026-03-31T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            retention_cutoff(now),
            NaiveDate::from_ymd_opt(2025, 2, 28).unwrap()
        );
    }
}

#[cfg(test)]
pub(crate) mod db_tests {
    use super::*;
    use crate::data::account::db_tests::{cleanup, connect, seed_user};

    /// A dim no real event has, so these tests only see their own rows.
    pub(crate) const TEST_DIM: &str = "/test-usage-daily";

    pub(crate) async fn count(pool: &PgPool, day: NaiveDate, event: &str, signed_in: bool) -> i64 {
        sqlx::query_scalar(
            "SELECT COALESCE(SUM(count), 0)::bigint FROM usage_daily \
             WHERE day = $1 AND event = $2 AND dim = $3 AND signed_in = $4",
        )
        .bind(day)
        .bind(event)
        .bind(TEST_DIM)
        .bind(signed_in)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    pub(crate) async fn clear(pool: &PgPool) {
        sqlx::query("DELETE FROM usage_daily WHERE dim LIKE $1")
            .bind(format!("{TEST_DIM}%"))
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api -- --ignored --test-threads=1`"]
    async fn increments_aggregate_into_one_row_per_day_event_dim_and_signed_in() {
        let pool = connect().await;
        clear(&pool).await;
        let day = NaiveDate::from_ymd_opt(2026, 10, 30).unwrap();
        for _ in 0..3 {
            increment(&pool, day, "page_view", TEST_DIM, false)
                .await
                .unwrap();
        }
        increment(&pool, day, "page_view", TEST_DIM, true)
            .await
            .unwrap();
        increment(&pool, day.pred_opt().unwrap(), "page_view", TEST_DIM, false)
            .await
            .unwrap();
        assert_eq!(count(&pool, day, "page_view", false).await, 3);
        assert_eq!(count(&pool, day, "page_view", true).await, 1);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_daily WHERE dim = $1")
            .bind(TEST_DIM)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 3, "one row per (day, event, dim, signed_in)");
        clear(&pool).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api -- --ignored --test-threads=1`"]
    async fn the_sweep_deletes_days_older_than_thirteen_months_only() {
        let pool = connect().await;
        clear(&pool).await;
        let now = "2026-10-30T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let cutoff = retention_cutoff(now);
        let old = cutoff.pred_opt().unwrap();
        increment(&pool, old, "page_view", TEST_DIM, false)
            .await
            .unwrap();
        increment(&pool, cutoff, "page_view", TEST_DIM, false)
            .await
            .unwrap();
        assert!(prune(&pool, now).await.unwrap() >= 1);
        assert_eq!(count(&pool, old, "page_view", false).await, 0);
        assert_eq!(count(&pool, cutoff, "page_view", false).await, 1);
        clear(&pool).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api -- --ignored --test-threads=1`"]
    async fn the_opt_in_is_off_by_default_and_can_be_set() {
        let pool = connect().await;
        let user = "usage-optin-user";
        cleanup(&pool, "usage-optin-").await;
        seed_user(&pool, user).await;
        assert_eq!(opt_in(&pool, user).await.unwrap(), Some(false));
        assert!(set_opt_in(&pool, user, true).await.unwrap());
        assert_eq!(opt_in(&pool, user).await.unwrap(), Some(true));
        assert!(set_opt_in(&pool, user, false).await.unwrap());
        assert_eq!(opt_in(&pool, user).await.unwrap(), Some(false));
        assert!(!set_opt_in(&pool, "usage-optin-nobody", true).await.unwrap());
        assert_eq!(opt_in(&pool, "usage-optin-nobody").await.unwrap(), None);
        cleanup(&pool, "usage-optin-").await;
    }
}

/// Pins [`retention_cutoff`]: the London date, minus 13 months (clamped to the
/// month's last day), around both clock changes and the date boundary.
#[cfg(test)]
mod dst_pinning_tests {
    use super::*;

    /// `(now, cutoff)`.
    #[rustfmt::skip]
    const CUTOFFS: &[(&str, &str)] = &[
        ("2025-03-29T22:00:00Z", "2024-02-29"),
        ("2025-03-29T23:00:00Z", "2024-02-29"),
        ("2025-03-29T23:30:00Z", "2024-02-29"),
        ("2025-03-30T00:00:00Z", "2024-02-29"),
        ("2025-03-30T00:30:00Z", "2024-02-29"),
        ("2025-03-30T00:59:00Z", "2024-02-29"),
        ("2025-03-30T01:00:00Z", "2024-02-29"),
        ("2025-03-30T01:01:00Z", "2024-02-29"),
        ("2025-03-30T01:30:00Z", "2024-02-29"),
        ("2025-03-30T01:59:00Z", "2024-02-29"),
        ("2025-03-30T02:00:00Z", "2024-02-29"),
        ("2025-03-30T02:30:00Z", "2024-02-29"),
        ("2025-03-30T03:00:00Z", "2024-02-29"),
        ("2025-03-30T12:00:00Z", "2024-02-29"),
        ("2025-03-30T22:59:00Z", "2024-02-29"),
        ("2025-03-30T23:00:00Z", "2024-02-29"),
        ("2025-03-30T23:30:00Z", "2024-02-29"),
        ("2025-03-31T00:30:00Z", "2024-02-29"),
        ("2025-10-25T22:00:00Z", "2024-09-25"),
        ("2025-10-25T23:00:00Z", "2024-09-26"),
        ("2025-10-25T23:30:00Z", "2024-09-26"),
        ("2025-10-26T00:00:00Z", "2024-09-26"),
        ("2025-10-26T00:30:00Z", "2024-09-26"),
        ("2025-10-26T00:59:00Z", "2024-09-26"),
        ("2025-10-26T01:00:00Z", "2024-09-26"),
        ("2025-10-26T01:01:00Z", "2024-09-26"),
        ("2025-10-26T01:30:00Z", "2024-09-26"),
        ("2025-10-26T01:59:00Z", "2024-09-26"),
        ("2025-10-26T02:00:00Z", "2024-09-26"),
        ("2025-10-26T02:30:00Z", "2024-09-26"),
        ("2025-10-26T03:00:00Z", "2024-09-26"),
        ("2025-10-26T12:00:00Z", "2024-09-26"),
        ("2025-10-26T22:59:00Z", "2024-09-26"),
        ("2025-10-26T23:00:00Z", "2024-09-26"),
        ("2025-10-26T23:30:00Z", "2024-09-26"),
        ("2025-10-27T00:30:00Z", "2024-09-27"),
        ("2025-12-31T22:00:00Z", "2024-11-30"),
        ("2025-12-31T22:59:00Z", "2024-11-30"),
        ("2025-12-31T23:00:00Z", "2024-11-30"),
        ("2025-12-31T23:30:00Z", "2024-11-30"),
        ("2025-12-31T23:59:00Z", "2024-11-30"),
        ("2026-01-01T00:00:00Z", "2024-12-01"),
        ("2026-01-01T00:30:00Z", "2024-12-01"),
        ("2026-01-01T00:59:00Z", "2024-12-01"),
        ("2026-01-01T01:00:00Z", "2024-12-01"),
        ("2026-01-01T01:30:00Z", "2024-12-01"),
        ("2026-01-01T01:59:00Z", "2024-12-01"),
        ("2026-01-01T02:00:00Z", "2024-12-01"),
        ("2026-01-01T02:30:00Z", "2024-12-01"),
        ("2026-01-15T22:00:00Z", "2024-12-15"),
        ("2026-01-15T22:59:00Z", "2024-12-15"),
        ("2026-01-15T23:00:00Z", "2024-12-15"),
        ("2026-01-15T23:30:00Z", "2024-12-15"),
        ("2026-01-15T23:59:00Z", "2024-12-15"),
        ("2026-01-16T00:00:00Z", "2024-12-16"),
        ("2026-01-16T00:30:00Z", "2024-12-16"),
        ("2026-01-16T00:59:00Z", "2024-12-16"),
        ("2026-01-16T01:00:00Z", "2024-12-16"),
        ("2026-01-16T01:30:00Z", "2024-12-16"),
        ("2026-01-16T01:59:00Z", "2024-12-16"),
        ("2026-01-16T02:00:00Z", "2024-12-16"),
        ("2026-01-16T02:30:00Z", "2024-12-16"),
        ("2026-03-28T22:00:00Z", "2025-02-28"),
        ("2026-03-28T23:00:00Z", "2025-02-28"),
        ("2026-03-28T23:30:00Z", "2025-02-28"),
        ("2026-03-29T00:00:00Z", "2025-02-28"),
        ("2026-03-29T00:30:00Z", "2025-02-28"),
        ("2026-03-29T00:59:00Z", "2025-02-28"),
        ("2026-03-29T01:00:00Z", "2025-02-28"),
        ("2026-03-29T01:01:00Z", "2025-02-28"),
        ("2026-03-29T01:30:00Z", "2025-02-28"),
        ("2026-03-29T01:59:00Z", "2025-02-28"),
        ("2026-03-29T02:00:00Z", "2025-02-28"),
        ("2026-03-29T02:30:00Z", "2025-02-28"),
        ("2026-03-29T03:00:00Z", "2025-02-28"),
        ("2026-03-29T12:00:00Z", "2025-02-28"),
        ("2026-03-29T22:59:00Z", "2025-02-28"),
        ("2026-03-29T23:00:00Z", "2025-02-28"),
        ("2026-03-29T23:30:00Z", "2025-02-28"),
        ("2026-03-30T00:30:00Z", "2025-02-28"),
        ("2026-06-30T22:00:00Z", "2025-05-30"),
        ("2026-06-30T22:59:00Z", "2025-05-30"),
        ("2026-06-30T23:00:00Z", "2025-06-01"),
        ("2026-06-30T23:30:00Z", "2025-06-01"),
        ("2026-06-30T23:59:00Z", "2025-06-01"),
        ("2026-07-01T00:00:00Z", "2025-06-01"),
        ("2026-07-01T00:30:00Z", "2025-06-01"),
        ("2026-07-01T00:59:00Z", "2025-06-01"),
        ("2026-07-01T01:00:00Z", "2025-06-01"),
        ("2026-07-01T01:30:00Z", "2025-06-01"),
        ("2026-07-01T01:59:00Z", "2025-06-01"),
        ("2026-07-01T02:00:00Z", "2025-06-01"),
        ("2026-07-01T02:30:00Z", "2025-06-01"),
        ("2026-07-15T22:00:00Z", "2025-06-15"),
        ("2026-07-15T22:59:00Z", "2025-06-15"),
        ("2026-07-15T23:00:00Z", "2025-06-16"),
        ("2026-07-15T23:30:00Z", "2025-06-16"),
        ("2026-07-15T23:59:00Z", "2025-06-16"),
        ("2026-07-16T00:00:00Z", "2025-06-16"),
        ("2026-07-16T00:30:00Z", "2025-06-16"),
        ("2026-07-16T00:59:00Z", "2025-06-16"),
        ("2026-07-16T01:00:00Z", "2025-06-16"),
        ("2026-07-16T01:30:00Z", "2025-06-16"),
        ("2026-07-16T01:59:00Z", "2025-06-16"),
        ("2026-07-16T02:00:00Z", "2025-06-16"),
        ("2026-07-16T02:30:00Z", "2025-06-16"),
        ("2026-10-24T22:00:00Z", "2025-09-24"),
        ("2026-10-24T23:00:00Z", "2025-09-25"),
        ("2026-10-24T23:30:00Z", "2025-09-25"),
        ("2026-10-25T00:00:00Z", "2025-09-25"),
        ("2026-10-25T00:30:00Z", "2025-09-25"),
        ("2026-10-25T00:59:00Z", "2025-09-25"),
        ("2026-10-25T01:00:00Z", "2025-09-25"),
        ("2026-10-25T01:01:00Z", "2025-09-25"),
        ("2026-10-25T01:30:00Z", "2025-09-25"),
        ("2026-10-25T01:59:00Z", "2025-09-25"),
        ("2026-10-25T02:00:00Z", "2025-09-25"),
        ("2026-10-25T02:30:00Z", "2025-09-25"),
        ("2026-10-25T03:00:00Z", "2025-09-25"),
        ("2026-10-25T12:00:00Z", "2025-09-25"),
        ("2026-10-25T22:59:00Z", "2025-09-25"),
        ("2026-10-25T23:00:00Z", "2025-09-25"),
        ("2026-10-25T23:30:00Z", "2025-09-25"),
        ("2026-10-26T00:30:00Z", "2025-09-26"),
    ];

    #[test]
    fn the_retention_cutoff_matches_the_table() {
        assert_eq!(RETENTION_MONTHS, 13);
        for &(now, expected) in CUTOFFS {
            assert_eq!(
                retention_cutoff(now.parse().unwrap()),
                expected.parse().unwrap(),
                "now {now}"
            );
        }
    }
}
