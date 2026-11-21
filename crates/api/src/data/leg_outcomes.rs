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
//! - the train has terminated (`completed`): the measured delay, else no
//!   delay (its report there never came; a forecast is never recorded,
//!   because a reader cannot tell it from a measurement);
//! - the train was cancelled, once its service date is past (a same-day
//!   reinstatement is still possible): cancelled, with the measured delay
//!   when it reported at the leg's destination before the cancellation
//!   (it was cancelled after the passenger got off, so readers take the
//!   pair as "arrived": `journey_tickets::with_recorded`);
//! - two days after the service date, whatever has been measured by then,
//!   which is nothing when no train was ever matched or its data is
//!   already gone.
//!
//! Only a delay measured at the leg's own destination is recorded: a leg
//! with no destination is recorded with no delay, never the delay at
//! wherever the train last reported.
//!
//! Rows recorded before 2026-11-17 may hold a forecast as the delay of a
//! terminated train that never reported at the destination; nothing in
//! the row tells those apart, so they read as measured.
//!   This is also the backfill: the first passes record every leg dated
//!   before the columns existed, the ones whose trains are still held with
//!   their outcome.
//!
//! A leg with no subscription (never matched, or its train untracked) is
//! not recorded. Untracked legs have no train and are skipped.
//!
//! # The leg's own copy of its train (DS-643)
//!
//! The outcome is all `trains` pruning used to leave on the leg, so an old
//! matched leg had no stops. [`snapshot_legs`] runs after the outcomes, in
//! the same pass, and keeps `journey_legs.final_train_uid`, `final_snapshot`
//! and `final_snapshot_at` ([`crate::data::leg_snapshot`] is the document):
//!
//! - as soon as the leg's subscription has a `trains` row with calling
//!   points (they are static, so it does not wait for the leg to finish):
//!   the document without an outcome;
//! - when the leg's outcome is recorded: the outcome keys, and, while the
//!   `trains` row is still held, the row read again (the platform and
//!   skipped stops move during the day). `final_snapshot_at` then passes
//!   `final_recorded_at`, which is what marks a leg done.
//!
//! A leg is only written while it still has the subscription the row was
//! read for, so "Change train" and the sweep cannot cross. A leg with a
//! snapshot and no subscription (its train was untracked) has the copy
//! cleared. `train_subscriptions` has no calling points of its own (the
//! column went with `tracked_trains`' in 20260906140000), so when the
//! `trains` row is already gone there is no source to snapshot from.

use chrono::NaiveDate;
use sqlx::PgPool;

use crate::data::leg_snapshot::{LegSnapshot, TrainFacts};
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
            delay_minutes: None,
            cancelled: Some(false),
        },
        _ if stale => Decision::Record {
            delay_minutes: None,
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
    // Best effort: the copy never holds up the outcomes or the pruning that
    // follows, and the next pass tries again.
    match snapshot_legs(pool).await {
        Ok(snapshotted) if snapshotted > 0 => {
            tracing::info!(snapshotted, "snapshotted journey legs' trains");
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(error = ?err, "could not snapshot journey legs' trains"),
    }
    Ok(recorded)
}

/// What the snapshot pass reads for one leg.
#[derive(Debug, Clone, sqlx::FromRow)]
struct SnapshotFacts {
    id: i64,
    subscription_id: i64,
    snapshot: Option<serde_json::Value>,
    recorded: bool,
    delay_minutes: Option<i32>,
    cancelled: Option<bool>,
    train_uid: Option<String>,
    train_id: Option<String>,
    train_service_date: Option<NaiveDate>,
    origin_crs: Option<String>,
    destination_crs: Option<String>,
    scheduled_departure: Option<chrono::DateTime<chrono::Utc>>,
    matched_line_id: Option<String>,
    platform: Option<String>,
    planned_platform: Option<String>,
    skipped_stations: Option<Vec<String>>,
    calling_points: Option<serde_json::Value>,
}

impl SnapshotFacts {
    fn train(&self) -> Option<TrainFacts> {
        Some(TrainFacts {
            train_uid: self.train_uid.clone()?,
            train_id: self.train_id.clone(),
            service_date: self.train_service_date?,
            origin_crs: self.origin_crs.clone(),
            destination_crs: self.destination_crs.clone(),
            scheduled_departure: self.scheduled_departure,
            matched_line_id: self.matched_line_id.clone(),
            platform: self.platform.clone(),
            planned_platform: self.planned_platform.clone(),
            skipped_stations: self.skipped_stations.clone().unwrap_or_default(),
            calling_points: self.calling_points.clone(),
        })
    }
}

/// The document to store for a leg now, or `None` when there is nothing to
/// write (no source, or one it cannot read). Pure, so it is tested
/// without a database.
fn next_snapshot(facts: &SnapshotFacts, now: chrono::DateTime<chrono::Utc>) -> Option<LegSnapshot> {
    let outcome = facts
        .recorded
        .then_some((facts.delay_minutes, facts.cancelled));
    let existing = match &facts.snapshot {
        Some(value) => Some(LegSnapshot::parse(value)?),
        None => None,
    };
    let fresh = facts
        .train()
        .map(|train| LegSnapshot::from_train(train, now));
    let base = match (fresh, existing) {
        // The row is held: read again, keeping the earlier copy's stops if
        // the row has lost them.
        (Some(mut fresh), existing) => {
            if fresh.calling_points.is_empty() {
                match existing {
                    Some(earlier) => fresh.calling_points = earlier.calling_points,
                    // Nothing static to keep yet: wait for the timetable, or
                    // for the outcome.
                    None if outcome.is_none() => return None,
                    None => {}
                }
            }
            fresh
        }
        // The row is gone: only the outcome can still be added.
        (None, Some(existing)) => {
            outcome?;
            existing
        }
        (None, None) => return None,
    };
    Some(match outcome {
        Some((delay, cancelled)) => base.with_outcome(delay, cancelled),
        None => base,
    })
}

static SNAPSHOT_LEGS_SQL: &str = "\
    SELECT jl.id, ts.id AS subscription_id, jl.final_snapshot AS snapshot, \
           jl.final_recorded_at IS NOT NULL AS recorded, \
           jl.final_arrival_delay_minutes AS delay_minutes, \
           jl.final_train_cancelled AS cancelled, \
           tr.train_uid, tr.train_id, tr.service_date AS train_service_date, \
           tr.origin_crs, tr.destination_crs, tr.scheduled_departure, tr.matched_line_id, \
           tr.platform, tr.planned_platform, tr.skipped_stations, tr.calling_points \
    FROM journey_legs jl \
    JOIN train_subscriptions ts ON ts.id = jl.train_subscription_id \
    LEFT JOIN trains tr ON tr.id = ts.trains_id \
    WHERE jl.kind = 'train' AND jl.id > $1 \
      AND ( \
          (jl.final_snapshot IS NULL AND tr.id IS NOT NULL \
           AND (jl.final_recorded_at IS NOT NULL \
                OR (jsonb_typeof(tr.calling_points) = 'array' \
                    AND jsonb_array_length(tr.calling_points) > 0))) \
          OR (jl.final_snapshot IS NOT NULL AND jl.final_recorded_at IS NOT NULL \
              AND jl.final_snapshot_at < jl.final_recorded_at)) \
    ORDER BY jl.id \
    LIMIT $2";

/// Writes the snapshot of every train leg that needs one (module docs), at
/// most `MAX_BATCHES * BATCH` legs read per call. Returns how many legs it
/// wrote. Idempotent: a leg is written only while it has no snapshot or
/// its outcome is newer than it, and only while it still has the
/// subscription it was read with.
pub async fn snapshot_legs(pool: &PgPool) -> anyhow::Result<u64> {
    let mut after = 0_i64;
    let mut written = 0;
    for _ in 0..MAX_BATCHES {
        let legs: Vec<SnapshotFacts> = sqlx::query_as(SNAPSHOT_LEGS_SQL)
            .bind(after)
            .bind(BATCH)
            .fetch_all(pool)
            .await?;
        let Some(last) = legs.last() else { break };
        after = last.id;
        let full = i64::try_from(legs.len()).unwrap_or(i64::MAX) == BATCH;
        written += snapshot_batch(pool, &legs).await?;
        if !full {
            break;
        }
    }
    clear_orphan_snapshots(pool).await?;
    Ok(written)
}

async fn snapshot_batch(pool: &PgPool, legs: &[SnapshotFacts]) -> anyhow::Result<u64> {
    let now = chrono::Utc::now();
    let mut ids = Vec::new();
    let mut subscriptions = Vec::new();
    let mut uids = Vec::new();
    let mut documents = Vec::new();
    for leg in legs {
        let Some(snapshot) = next_snapshot(leg, now) else {
            continue;
        };
        ids.push(leg.id);
        subscriptions.push(leg.subscription_id);
        uids.push(snapshot.train_uid.clone());
        documents.push(snapshot.to_value().to_string());
    }
    if ids.is_empty() {
        return Ok(0);
    }
    let result = sqlx::query(
        "UPDATE journey_legs jl \
         SET final_train_uid = v.uid, final_snapshot = v.document::jsonb, \
             final_snapshot_at = now() \
         FROM UNNEST($1::bigint[], $2::bigint[], $3::text[], $4::text[]) \
              AS v(id, subscription_id, uid, document) \
         WHERE jl.id = v.id AND jl.train_subscription_id = v.subscription_id \
           AND (jl.final_snapshot IS NULL OR jl.final_snapshot_at < jl.final_recorded_at)",
    )
    .bind(&ids)
    .bind(&subscriptions)
    .bind(&uids)
    .bind(&documents)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Clears the copy off a leg that no longer has a subscription (its train
/// was untracked, or retention took the subscription): nothing shows it, and
/// it should not outlive the leg's train.
async fn clear_orphan_snapshots(pool: &PgPool) -> anyhow::Result<u64> {
    let mut cleared = 0;
    loop {
        let result = sqlx::query(
            "UPDATE journey_legs SET final_train_uid = NULL, final_snapshot = NULL, \
                 final_snapshot_at = NULL \
             WHERE id IN ( \
                 SELECT id FROM journey_legs \
                 WHERE final_snapshot IS NOT NULL AND train_subscription_id IS NULL \
                 ORDER BY id LIMIT $1)",
        )
        .bind(BATCH)
        .execute(pool)
        .await?;
        cleared += result.rows_affected();
        if i64::try_from(result.rows_affected()).unwrap_or(i64::MAX) < BATCH {
            return Ok(cleared);
        }
    }
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
        )
        // No stop to measure at means no measurement, never the delay at
        // wherever the train last reported (`stop_delay::target`'s fallback).
        .filter(|target| target.stop_crs.is_some())
        {
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

    /// Review 2026-11 (C4): a forecast is never recorded, so it can never
    /// be read back as a measured delay once the train's data is pruned.
    #[test]
    fn a_terminated_train_with_no_report_at_the_destination_records_no_delay() {
        let today = d(TODAY);
        assert_eq!(
            decide(today, today, true, Some("completed"), Some(forecast(14))),
            record(None, Some(false))
        );
        assert_eq!(
            decide(today, today, true, Some("completed"), Some(measured(14))),
            record(Some(14), Some(false))
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
            record(None, Some(false)),
            "a forecast is not a measurement"
        );
        assert_eq!(
            decide(stale, today, true, None, None),
            record(None, None),
            "a train never activated"
        );
    }

    fn facts() -> SnapshotFacts {
        SnapshotFacts {
            id: 1,
            subscription_id: 2,
            snapshot: None,
            recorded: false,
            delay_minutes: None,
            cancelled: None,
            train_uid: Some("C1".to_owned()),
            train_id: None,
            train_service_date: Some(d(TODAY)),
            origin_crs: None,
            destination_crs: None,
            scheduled_departure: None,
            matched_line_id: None,
            platform: Some("1".to_owned()),
            planned_platform: None,
            skipped_stations: Some(Vec::new()),
            calling_points: Some(serde_json::json!([{"tiploc": "A", "kind": "Origin"}])),
        }
    }

    #[test]
    fn a_held_train_with_calling_points_is_snapshotted_before_the_leg_completes() {
        let snapshot = next_snapshot(&facts(), chrono::Utc::now()).unwrap();
        assert_eq!(snapshot.train_uid, "C1");
        assert_eq!(snapshot.calling_points.len(), 1);
        assert_eq!(snapshot.final_arrival_delay_minutes, None);
        assert_eq!(snapshot.train_cancelled, None);
    }

    #[test]
    fn a_train_with_no_timetable_waits_for_one_or_for_the_outcome() {
        let mut leg = facts();
        leg.calling_points = None;
        assert_eq!(next_snapshot(&leg, chrono::Utc::now()), None);
        leg.recorded = true;
        leg.delay_minutes = Some(3);
        leg.cancelled = Some(false);
        let snapshot = next_snapshot(&leg, chrono::Utc::now()).unwrap();
        assert_eq!(snapshot.calling_points, Vec::<serde_json::Value>::new());
        assert_eq!(snapshot.final_arrival_delay_minutes, Some(3));
    }

    #[test]
    fn the_outcome_is_merged_and_a_gone_train_keeps_its_earlier_copy() {
        let earlier = next_snapshot(&facts(), chrono::Utc::now()).unwrap();
        let mut leg = facts();
        leg.snapshot = Some(earlier.to_value());
        leg.recorded = true;
        leg.delay_minutes = Some(17);
        leg.cancelled = Some(false);
        // The row is gone: only the outcome is added.
        leg.train_uid = None;
        leg.calling_points = None;
        let merged = next_snapshot(&leg, chrono::Utc::now()).unwrap();
        assert_eq!(merged.calling_points, earlier.calling_points);
        assert_eq!(merged.platform, earlier.platform);
        assert_eq!(merged.final_arrival_delay_minutes, Some(17));
        // No outcome yet and no row: nothing to write.
        leg.recorded = false;
        assert_eq!(next_snapshot(&leg, chrono::Utc::now()), None);
        // No row and no copy: nothing to write.
        leg.snapshot = None;
        leg.recorded = true;
        assert_eq!(next_snapshot(&leg, chrono::Utc::now()), None);
    }

    #[test]
    fn a_copy_of_another_version_is_left_alone() {
        let mut leg = facts();
        leg.snapshot = Some(serde_json::json!({"v": 2}));
        leg.recorded = true;
        assert_eq!(next_snapshot(&leg, chrono::Utc::now()), None);
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
        // No destination anywhere (leg, pin or train): the arrival at RDG is
        // just where the train last reported, not the leg's end.
        let t = seed_train(&pool, "LEGOUT8", days(1), Some("completed"), Some(22)).await;
        sqlx::query("UPDATE trains SET destination_crs = NULL WHERE id = $1")
            .bind(t)
            .execute(&pool)
            .await
            .unwrap();
        let nowhere = seed_leg(&pool, days(1), true, Some(t)).await;
        sqlx::query("UPDATE journey_legs SET destination_crs = NULL WHERE id = $1")
            .bind(nowhere)
            .execute(&pool)
            .await
            .unwrap();

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
        assert_eq!(
            recorded(&pool, nowhere).await,
            (None, Some(false), true),
            "no destination, no delay"
        );

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
