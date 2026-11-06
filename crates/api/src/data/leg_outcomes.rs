//! Each train leg's outcome, recorded on the leg while its train's data is
//! still held (DS-379): `journey_legs.final_arrival_delay_minutes`,
//! `final_train_cancelled` and `final_recorded_at`.
//!
//! ds-workers' retention deletes `trains`, `train_current_state` and the
//! movements about 30 days after the service date (`trainsRetentionDays`),
//! but regular journeys' 90-day rule ([`super::retention`]) keeps an
//! occurrence whose train arrived at least 15 minutes late or was
//! cancelled. [`record_final_arrivals`] runs at the start of every
//! retention pass (the hourly loop or the `api-maintenance` `CronJob`),
//! whether or not that rule is on, so the outcomes are there when it is
//! turned on. It is the only writer of the three columns.
//!
//! The delay is the one the journey page shows: the train's public delay
//! at the leg's own destination (`ds_store::trains::stop_delay`). A leg is
//! recorded once, when its train is over ([`decide`]):
//!
//! - the train has reported at the leg's destination: the measured delay;
//! - the train has terminated (`completed`): the measured delay, else the
//!   last forecast (its report there never came);
//! - the train was cancelled, once its service date is past (a same-day
//!   reinstatement is still possible): cancelled;
//! - two days after the service date, whatever is known by then, which is
//!   nothing when no train was ever matched or its data is already gone.
//!   This is also the backfill: the first passes record every leg dated
//!   before the columns existed, the ones whose trains are still held with
//!   their outcome.
//!
//! A leg with no subscription (never matched, or its train untracked) is
//! not recorded. Untracked legs have no train and are skipped.

use chrono::NaiveDate;
use sqlx::PgPool;

use crate::data::stop_delay::{StopDelay, StopDelayTarget};

/// Legs read per batch.
const BATCH: i64 = 500;

/// Batches one pass reads at most; the rest wait for the next pass, oldest
/// legs first, so a large backfill takes a few hours.
const MAX_BATCHES: usize = 20;

/// What the pass reads for one unrecorded leg.
#[derive(Debug, Clone, sqlx::FromRow)]
struct LegFacts {
    id: i64,
    service_date: NaiveDate,
    trains_id: Option<i64>,
    train_uid: Option<String>,
    destination_crs: Option<String>,
    status: Option<String>,
    working_delay_minutes: Option<i32>,
}

/// What to write for a leg, or nothing yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// The train may still report: look again next pass.
    Wait,
    /// Write these once. `None`: not known.
    Record {
        delay_minutes: Option<i32>,
        cancelled: Option<bool>,
    },
}

/// Whether a leg's train is over, and what to record if it is. `today` is
/// London's service date; `delay` the public delay at the leg's
/// destination (`None` when not known).
pub(crate) fn decide(
    service_date: NaiveDate,
    today: NaiveDate,
    train_held: bool,
    status: Option<&str>,
    delay: Option<StopDelay>,
) -> Decision {
    // Matches `travel_order::train_arrived_sql`: two days on, every train
    // has arrived or never will.
    let stale = service_date < today - chrono::Days::new(1);
    if !train_held {
        return if stale {
            Decision::Record {
                delay_minutes: None,
                cancelled: None,
            }
        } else {
            Decision::Wait
        };
    }
    let measured = delay.filter(|d| !d.provisional).map(|d| d.minutes);
    match status {
        Some("cancelled") => {
            if service_date < today {
                Decision::Record {
                    delay_minutes: measured,
                    cancelled: Some(true),
                }
            } else {
                Decision::Wait
            }
        }
        _ if measured.is_some() => Decision::Record {
            delay_minutes: measured,
            cancelled: Some(false),
        },
        Some("completed") => Decision::Record {
            delay_minutes: delay.map(|d| d.minutes),
            cancelled: Some(false),
        },
        _ if stale => Decision::Record {
            delay_minutes: delay.map(|d| d.minutes),
            cancelled: status.map(|_| false),
        },
        _ => Decision::Wait,
    }
}

static UNRECORDED_LEGS_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let destination =
        ds_store::trains::leg_crs_sql(ds_store::trains::LegEnd::Destination, "jl", "ts", "tr");
    format!(
        "SELECT jl.id, jl.service_date, ts.trains_id, tr.train_uid, \
                {destination} AS destination_crs, cs.status, \
                cs.delay_minutes AS working_delay_minutes \
         FROM journey_legs jl \
         JOIN train_subscriptions ts ON ts.id = jl.train_subscription_id \
         LEFT JOIN trains tr ON tr.id = ts.trains_id \
         LEFT JOIN train_current_state cs ON cs.trains_id = ts.trains_id \
         WHERE jl.kind = 'train' AND jl.final_recorded_at IS NULL \
           AND jl.service_date <= $1 AND jl.id > $2 \
         ORDER BY jl.id \
         LIMIT $3"
    )
});

/// Records the outcome of every train leg whose train is over, at most
/// `MAX_BATCHES * BATCH` legs read per call. Returns how many legs it
/// recorded. Idempotent: a leg is written once (`final_recorded_at IS
/// NULL`), so two passes at once are harmless.
pub async fn record_final_arrivals(pool: &PgPool, today: NaiveDate) -> anyhow::Result<u64> {
    let mut after = 0_i64;
    let mut recorded = 0;
    for _ in 0..MAX_BATCHES {
        let legs: Vec<LegFacts> = sqlx::query_as(&UNRECORDED_LEGS_SQL)
            .bind(today)
            .bind(after)
            .bind(BATCH)
            .fetch_all(pool)
            .await?;
        let Some(last) = legs.last() else { break };
        after = last.id;
        let full = i64::try_from(legs.len()).unwrap_or(i64::MAX) == BATCH;
        recorded += record_batch(pool, &legs, today).await?;
        if !full {
            break;
        }
    }
    if recorded > 0 {
        tracing::info!(recorded, "recorded journey legs' final arrivals");
    }
    Ok(recorded)
}

async fn record_batch(pool: &PgPool, legs: &[LegFacts], today: NaiveDate) -> anyhow::Result<u64> {
    let mut positions = Vec::new();
    let mut targets: Vec<StopDelayTarget> = Vec::new();
    for (index, leg) in legs.iter().enumerate() {
        if let Some(target) = crate::data::stop_delay::target(
            leg.trains_id,
            leg.train_uid.as_deref(),
            leg.service_date,
            leg.destination_crs.as_deref(),
            leg.working_delay_minutes,
        ) {
            positions.push(index);
            targets.push(target);
        }
    }
    let mut delays: Vec<Option<StopDelay>> = vec![None; legs.len()];
    if !targets.is_empty() {
        let found = crate::data::stop_delay::stop_delays(pool, &targets).await?;
        for (index, delay) in positions.into_iter().zip(found) {
            delays[index] = delay;
        }
    }
    let mut ids = Vec::new();
    let mut minutes = Vec::new();
    let mut cancelled = Vec::new();
    for (leg, delay) in legs.iter().zip(delays) {
        if let Decision::Record {
            delay_minutes,
            cancelled: was_cancelled,
        } = decide(
            leg.service_date,
            today,
            leg.trains_id.is_some(),
            leg.status.as_deref(),
            delay,
        ) {
            ids.push(leg.id);
            minutes.push(delay_minutes);
            cancelled.push(was_cancelled);
        }
    }
    if ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "UPDATE journey_legs jl \
         SET final_arrival_delay_minutes = v.minutes, \
             final_train_cancelled = v.cancelled, \
             final_recorded_at = now() \
         FROM UNNEST($1::bigint[], $2::int[], $3::bool[]) AS v(id, minutes, cancelled) \
         WHERE jl.id = v.id AND jl.final_recorded_at IS NULL",
    )
    .bind(&ids)
    .bind(&minutes)
    .bind(&cancelled)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::stop_delay::DelayBasis;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn measured(minutes: i32) -> StopDelay {
        StopDelay {
            minutes,
            basis: DelayBasis::Public,
            provisional: false,
        }
    }

    fn forecast(minutes: i32) -> StopDelay {
        StopDelay {
            minutes,
            basis: DelayBasis::Public,
            provisional: true,
        }
    }

    fn record(delay_minutes: Option<i32>, cancelled: Option<bool>) -> Decision {
        Decision::Record {
            delay_minutes,
            cancelled,
        }
    }

    const TODAY: &str = "2026-11-06";

    #[test]
    fn a_train_that_reported_at_the_destination_is_recorded_at_once() {
        let today = d(TODAY);
        assert_eq!(
            decide(today, today, true, Some("en_route"), Some(measured(0))),
            record(Some(0), Some(false)),
            "on time"
        );
        assert_eq!(
            decide(today, today, true, Some("en_route"), Some(measured(17))),
            record(Some(17), Some(false)),
            "15 or more minutes late"
        );
    }

    #[test]
    fn a_running_train_waits() {
        let today = d(TODAY);
        assert_eq!(
            decide(today, today, true, Some("en_route"), Some(forecast(20))),
            Decision::Wait
        );
        assert_eq!(
            decide(today, today, true, Some("awaiting_activation"), None),
            Decision::Wait
        );
        assert_eq!(
            decide(
                d("2026-11-05"),
                today,
                true,
                Some("en_route"),
                Some(forecast(4))
            ),
            Decision::Wait,
            "yesterday's overnight train may still report"
        );
    }

    #[test]
    fn a_terminated_train_falls_back_to_the_forecast() {
        let today = d(TODAY);
        assert_eq!(
            decide(today, today, true, Some("completed"), Some(forecast(16))),
            record(Some(16), Some(false))
        );
        assert_eq!(
            decide(today, today, true, Some("completed"), None),
            record(None, Some(false))
        );
    }

    #[test]
    fn a_cancellation_is_recorded_once_its_day_is_past() {
        let today = d(TODAY);
        assert_eq!(
            decide(today, today, true, Some("cancelled"), None),
            Decision::Wait,
            "it may yet be reinstated"
        );
        assert_eq!(
            decide(
                d("2026-11-05"),
                today,
                true,
                Some("cancelled"),
                Some(forecast(3))
            ),
            record(None, Some(true))
        );
        assert_eq!(
            decide(
                d("2026-11-05"),
                today,
                true,
                Some("cancelled"),
                Some(measured(25))
            ),
            record(Some(25), Some(true)),
            "cancelled after the leg's destination"
        );
    }

    #[test]
    fn two_days_on_whatever_is_known_is_recorded() {
        let today = d(TODAY);
        let stale = d("2026-11-04");
        assert_eq!(
            decide(stale, today, true, Some("en_route"), Some(forecast(12))),
            record(Some(12), Some(false))
        );
        assert_eq!(
            decide(stale, today, true, None, None),
            record(None, None),
            "a train never activated"
        );
    }

    #[test]
    fn a_leg_with_no_train_held_is_recorded_unknown_after_two_days() {
        let today = d(TODAY);
        assert_eq!(
            decide(d("2026-11-05"), today, false, None, None),
            Decision::Wait,
            "it may still be matched"
        );
        assert_eq!(
            decide(d("2026-11-04"), today, false, None, None),
            record(None, None),
            "never matched, or its data is gone"
        );
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::data::account::db_tests::{cleanup, connect, seed_user};

    const USER: &str = "legout-user";

    /// A `trains` row for `uid` on `date` with `status` and, when given, an
    /// ARRIVAL at RDG `late` minutes after its public time. Returns its id.
    async fn seed_train(
        pool: &PgPool,
        uid: &str,
        date: NaiveDate,
        status: Option<&str>,
        late: Option<i64>,
    ) -> i64 {
        sqlx::query("DELETE FROM trains WHERE train_uid = $1")
            .bind(uid)
            .execute(pool)
            .await
            .unwrap();
        let trains_id: i64 = sqlx::query_scalar(
            "INSERT INTO trains (train_uid, service_date, destination_crs) \
             VALUES ($1, $2, 'RDG') RETURNING id",
        )
        .bind(uid)
        .bind(date)
        .fetch_one(pool)
        .await
        .unwrap();
        if let Some(status) = status {
            sqlx::query(
                "INSERT INTO train_current_state (trains_id, status, delay_minutes) \
                 VALUES ($1, $2, $3)",
            )
            .bind(trains_id)
            .bind(status)
            .bind(late.map(|m| i32::try_from(m).unwrap()))
            .execute(pool)
            .await
            .unwrap();
        }
        if let Some(late) = late {
            sqlx::query(
                "INSERT INTO train_movement_events \
                     (trains_id, dedup_key, msg_type, event_type, loc_crs, planned_timestamp, \
                      gbtt_timestamp, actual_timestamp, raw_body) \
                 VALUES ($1, 'legout-' || $1, '0003', 'ARRIVAL', 'RDG', \
                         ($2::date + TIME '09:00') AT TIME ZONE 'Europe/London', \
                         ($2::date + TIME '09:00') AT TIME ZONE 'Europe/London', \
                         ($2::date + TIME '09:00') AT TIME ZONE 'Europe/London' \
                             + make_interval(mins => $3::int), \
                         '{}'::jsonb)",
            )
            .bind(trains_id)
            .bind(date)
            .bind(i32::try_from(late).unwrap())
            .execute(pool)
            .await
            .unwrap();
        }
        trains_id
    }

    /// A journey with one train leg on `date` to RDG: bound to a
    /// subscription of `trains_id` when `matched`, else unmatched. Returns
    /// the leg's id.
    async fn seed_leg(
        pool: &PgPool,
        date: NaiveDate,
        matched: bool,
        trains_id: Option<i64>,
    ) -> i64 {
        let subscription: Option<i64> = if matched {
            Some(
                sqlx::query_scalar(
                    "INSERT INTO train_subscriptions \
                         (user_id, service_date, resolution_status, trains_id) \
                     VALUES ($1, $2, 'resolved', $3) RETURNING id",
                )
                .bind(USER)
                .bind(date)
                .bind(trains_id)
                .fetch_one(pool)
                .await
                .unwrap(),
            )
        } else {
            None
        };
        sqlx::query_scalar(
            "WITH j AS (INSERT INTO journeys (user_id) VALUES ($1) RETURNING id) \
             INSERT INTO journey_legs (journey_id, leg_order, service_date, destination_crs, \
                 match_mode, train_subscription_id) \
             SELECT id, 1, $2, 'RDG', $3, $4 FROM j RETURNING id",
        )
        .bind(USER)
        .bind(date)
        .bind(if matched { "auto" } else { "unmatched" })
        .bind(subscription)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    type Recorded = (Option<i32>, Option<bool>, bool);

    async fn recorded(pool: &PgPool, leg: i64) -> Recorded {
        sqlx::query_as(
            "SELECT final_arrival_delay_minutes, final_train_cancelled, \
                    final_recorded_at IS NOT NULL \
             FROM journey_legs WHERE id = $1",
        )
        .bind(leg)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    const WAITING: Recorded = (None, None, false);

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api -- --ignored --test-threads=1`"]
    async fn each_leg_is_recorded_once_its_train_is_over() {
        let pool = connect().await;
        cleanup(&pool, "legout-").await;
        seed_user(&pool, USER).await;
        let today = crate::routes::london_today();
        let days = |n: u64| today - chrono::Days::new(n);

        let t = seed_train(&pool, "LEGOUT1", today, Some("en_route"), Some(1)).await;
        let on_time = seed_leg(&pool, today, true, Some(t)).await;
        let t = seed_train(&pool, "LEGOUT2", days(1), Some("completed"), Some(17)).await;
        let late = seed_leg(&pool, days(1), true, Some(t)).await;
        let t = seed_train(&pool, "LEGOUT3", days(1), Some("cancelled"), None).await;
        let cancelled = seed_leg(&pool, days(1), true, Some(t)).await;
        let t = seed_train(&pool, "LEGOUT4", today, Some("cancelled"), None).await;
        let cancelled_today = seed_leg(&pool, today, true, Some(t)).await;
        let t = seed_train(&pool, "LEGOUT5", today, Some("en_route"), None).await;
        let running = seed_leg(&pool, today, true, Some(t)).await;
        let t = seed_train(&pool, "LEGOUT6", days(20), Some("en_route"), Some(22)).await;
        let backfilled = seed_leg(&pool, days(20), true, Some(t)).await;
        let gone = seed_leg(&pool, days(40), true, None).await;
        let never_matched = seed_leg(&pool, days(5), false, None).await;
        let tomorrow = seed_train(
            &pool,
            "LEGOUT7",
            today + chrono::Days::new(1),
            Some("completed"),
            None,
        )
        .await;
        let future = seed_leg(&pool, today + chrono::Days::new(1), true, Some(tomorrow)).await;

        let first = record_final_arrivals(&pool, today).await.unwrap();
        assert!(first >= 5, "{first}");
        assert_eq!(recorded(&pool, on_time).await, (Some(1), Some(false), true));
        assert_eq!(recorded(&pool, late).await, (Some(17), Some(false), true));
        assert_eq!(recorded(&pool, cancelled).await, (None, Some(true), true));
        assert_eq!(recorded(&pool, cancelled_today).await, WAITING);
        assert_eq!(recorded(&pool, running).await, WAITING);
        assert_eq!(
            recorded(&pool, backfilled).await,
            (Some(22), Some(false), true),
            "a leg from before the columns, its train still held"
        );
        assert_eq!(
            recorded(&pool, gone).await,
            (None, None, true),
            "its train's data already gone"
        );
        assert_eq!(recorded(&pool, never_matched).await, WAITING);
        assert_eq!(recorded(&pool, future).await, WAITING);

        // Written once: a later report changes nothing recorded.
        sqlx::query(
            "UPDATE train_movement_events SET actual_timestamp = actual_timestamp \
                 + INTERVAL '30 minutes' \
             WHERE trains_id = (SELECT id FROM trains WHERE train_uid = 'LEGOUT2')",
        )
        .execute(&pool)
        .await
        .unwrap();
        record_final_arrivals(&pool, today).await.unwrap();
        assert_eq!(recorded(&pool, late).await, (Some(17), Some(false), true));

        cleanup(&pool, "legout-").await;
        sqlx::query("DELETE FROM trains WHERE train_uid LIKE 'LEGOUT%'")
            .execute(&pool)
            .await
            .unwrap();
    }
}
