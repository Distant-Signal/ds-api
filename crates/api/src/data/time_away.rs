//! A user's time away (`user_away_periods`, ds-core v0.11.0): inclusive
//! date ranges, each with an optional label, during which none of their
//! regular journeys makes a journey (decision 1 of the scheduling
//! options: time away is per user and applies to all of them).
//! `ds_store::recurrence` reads it as the skip reason `away`, for the
//! notifier's mint and the api's `nextDate` and `upcomingDates` alike.
//!
//! Adding or moving a period deletes the user's untouched occurrences
//! already minted inside it (decision 7), as pausing does, without writing
//! tombstones: the rule itself now skips those dates. A period is pruned
//! once it has ended ([`prune_ended`]); at most [`MAX_PERIODS_PER_USER`]
//! are kept.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Serialize;
use sqlx::PgPool;

use crate::data::regular_journeys::{self, london_date};

/// The most time away periods a user may have.
pub const MAX_PERIODS_PER_USER: i64 = 50;
/// The longest period, in days between its first and last date (the
/// table's CHECK).
pub const MAX_PERIOD_DAYS: i64 = 366;
/// How far ahead a period may start, in days from London's today.
pub const MAX_DAYS_AHEAD: i64 = 366;
/// The longest label, in characters (the table's CHECK).
pub const MAX_LABEL_CHARS: usize = 60;

/// One period, as `GET /preferences/time-away` serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "TimeAwayPeriod")]
pub struct TimeAwayPeriod {
    pub id: i64,
    /// The first date away (a London date).
    pub starts_on: NaiveDate,
    /// The last date away, inclusive.
    pub ends_on: NaiveDate,
    /// The user's own name for it, or `null`.
    pub label: Option<String>,
}

/// A period to save, validated by [`validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodInput {
    pub starts_on: NaiveDate,
    pub ends_on: NaiveDate,
    pub label: Option<String>,
}

/// Checks a period against London's `today`: it ends on or after its
/// start, no later than [`MAX_PERIOD_DAYS`] after it, hasn't ended, and
/// starts within [`MAX_DAYS_AHEAD`]. A blank label is no label.
///
/// # Errors
///
/// The message to show.
pub fn validate(
    starts_on: NaiveDate,
    ends_on: NaiveDate,
    label: Option<&str>,
    today: NaiveDate,
) -> Result<PeriodInput, String> {
    if ends_on < starts_on {
        return Err("The last day away must be on or after the first.".to_string());
    }
    if (ends_on - starts_on).num_days() > MAX_PERIOD_DAYS {
        return Err("Time away can be up to a year long.".to_string());
    }
    if ends_on < today {
        return Err("That time away has already ended.".to_string());
    }
    if starts_on > today + Duration::days(MAX_DAYS_AHEAD) {
        return Err("Time away can start up to a year ahead.".to_string());
    }
    let label = label.map(str::trim).filter(|label| !label.is_empty());
    if label.is_some_and(|label| label.chars().count() > MAX_LABEL_CHARS) {
        return Err(format!(
            "A name for your time away can be up to {MAX_LABEL_CHARS} characters."
        ));
    }
    Ok(PeriodInput {
        starts_on,
        ends_on,
        label: label.map(str::to_string),
    })
}

/// The user's periods that haven't ended, soonest first.
///
/// # Errors
///
/// The query's.
pub async fn list(
    pool: &PgPool,
    user_id: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<TimeAwayPeriod>> {
    Ok(sqlx::query_as(
        "SELECT id, starts_on, ends_on, label FROM user_away_periods \
         WHERE user_id = $1 AND ends_on >= $2 ORDER BY starts_on, id",
    )
    .bind(user_id)
    .bind(london_date(now))
    .fetch_all(pool)
    .await?)
}

/// What [`create`] and [`update`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    /// Saved; `deleted_journey_ids` are the untouched journeys already
    /// added inside it, now deleted.
    Saved {
        period: TimeAwayPeriod,
        deleted_journey_ids: Vec<i64>,
    },
    /// The user already has [`MAX_PERIODS_PER_USER`] periods.
    TooMany,
    /// No such period of the user's ([`update`]).
    NotFound,
}

/// Adds a period for `user_id` and deletes their untouched journeys inside
/// it, in one transaction. The user's row is locked first, so two adds at
/// once can't pass the cap together.
///
/// # Errors
///
/// The queries'.
pub async fn create(
    pool: &PgPool,
    user_id: &str,
    input: &PeriodInput,
    now: DateTime<Utc>,
) -> anyhow::Result<SaveOutcome> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM user_away_periods WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
    if count >= MAX_PERIODS_PER_USER {
        tx.rollback().await?;
        return Ok(SaveOutcome::TooMany);
    }
    let period: TimeAwayPeriod = sqlx::query_as(
        "INSERT INTO user_away_periods (user_id, starts_on, ends_on, label) \
         VALUES ($1, $2, $3, $4) RETURNING id, starts_on, ends_on, label",
    )
    .bind(user_id)
    .bind(input.starts_on)
    .bind(input.ends_on)
    .bind(input.label.as_deref())
    .fetch_one(&mut *tx)
    .await?;
    let deleted = regular_journeys::delete_untouched_occurrences(
        &mut tx,
        user_id,
        None,
        input.starts_on,
        Some(input.ends_on),
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(SaveOutcome::Saved {
        period,
        deleted_journey_ids: deleted,
    })
}

/// Replaces period `id` of `user_id` and deletes their untouched journeys
/// inside its new dates, in one transaction. A journey the old dates had
/// kept from being added comes back on the next mint if it is still due.
///
/// # Errors
///
/// The queries'.
pub async fn update(
    pool: &PgPool,
    user_id: &str,
    id: i64,
    input: &PeriodInput,
    now: DateTime<Utc>,
) -> anyhow::Result<SaveOutcome> {
    let mut tx = pool.begin().await?;
    let period: Option<TimeAwayPeriod> = sqlx::query_as(
        "UPDATE user_away_periods SET starts_on = $3, ends_on = $4, label = $5 \
         WHERE id = $1 AND user_id = $2 RETURNING id, starts_on, ends_on, label",
    )
    .bind(id)
    .bind(user_id)
    .bind(input.starts_on)
    .bind(input.ends_on)
    .bind(input.label.as_deref())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(period) = period else {
        tx.rollback().await?;
        return Ok(SaveOutcome::NotFound);
    };
    let deleted = regular_journeys::delete_untouched_occurrences(
        &mut tx,
        user_id,
        None,
        input.starts_on,
        Some(input.ends_on),
        now,
    )
    .await?;
    tx.commit().await?;
    Ok(SaveOutcome::Saved {
        period,
        deleted_journey_ids: deleted,
    })
}

/// Deletes period `id` of `user_id`. `false` when it isn't theirs. The
/// journeys it kept from being added come back on the next mint if they
/// are still due.
///
/// # Errors
///
/// The query's.
pub async fn delete(pool: &PgPool, user_id: &str, id: i64) -> anyhow::Result<bool> {
    Ok(
        sqlx::query("DELETE FROM user_away_periods WHERE id = $1 AND user_id = $2")
            .bind(id)
            .bind(user_id)
            .execute(pool)
            .await?
            .rows_affected()
            > 0,
    )
}

/// Deletes every period that ended before London's `today`: past time
/// away skips nothing. Run by the hourly retention pass, whatever its
/// settings.
///
/// # Errors
///
/// The query's.
pub async fn prune_ended(pool: &PgPool, today: NaiveDate) -> anyhow::Result<u64> {
    Ok(
        sqlx::query("DELETE FROM user_away_periods WHERE ends_on < $1")
            .bind(today)
            .execute(pool)
            .await?
            .rows_affected(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }

    #[test]
    fn validate_checks_the_dates_and_trims_the_label() {
        let today = d(11, 7);
        assert_eq!(
            validate(d(11, 10), d(11, 14), Some("  Holiday "), today),
            Ok(PeriodInput {
                starts_on: d(11, 10),
                ends_on: d(11, 14),
                label: Some("Holiday".to_string()),
            })
        );
        // Under way already: fine.
        assert!(validate(d(11, 1), d(11, 7), None, today).is_ok());
        assert_eq!(
            validate(d(11, 1), d(11, 10), Some("   "), today)
                .unwrap()
                .label,
            None
        );
        assert!(validate(d(11, 10), d(11, 9), None, today).is_err());
        assert!(validate(d(11, 1), d(11, 6), None, today).is_err(), "ended");
        assert!(validate(d(11, 10), d(11, 10) + Duration::days(367), None, today).is_err());
        assert!(validate(d(11, 10), d(11, 10) + Duration::days(366), None, today).is_ok());
        assert!(
            validate(
                today + Duration::days(367),
                today + Duration::days(368),
                None,
                today
            )
            .is_err()
        );
        assert!(validate(d(11, 10), d(11, 11), Some(&"x".repeat(61)), today).is_err());
        assert!(validate(d(11, 10), d(11, 11), Some(&"é".repeat(60)), today).is_ok());
    }
}
