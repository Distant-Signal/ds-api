//! When a train actually left the station a passenger boarded at, and how
//! late that was against the public timetable: the evidence a Delay Repay
//! or appeal claim needs about the boarding end of a leg
//! (`legs[].delayRepay.boarding*`). One batched, read-only look at TRUST's
//! DEPARTURE movements at the boarding CRS (`train_movement_events`, by
//! `trains_id`, with no recency filter: a train pinned for months still
//! answers) and, only to move the working time to a public one when TRUST
//! sent no public time, the CIF call there (`schedule_calling_points_full`).
//!
//! TRUST reports only at timing points, so a boarding station that is not
//! one has no DEPARTURE: that is `None`, never an estimate. An ARRIVAL or a
//! PASS there is not a departure either.

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use common::public_delay::{MeasuredDelay, Reported, ScheduledSide, reported_delay_with_gap};
use sqlx::PgPool;

/// One train and the CRS the passenger boards it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardingTarget {
    pub trains_id: i64,
    pub train_uid: String,
    pub service_date: NaiveDate,
    /// Upper-case CRS.
    pub crs: String,
}

/// The train's reported departure from the boarding station.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoardingDeparture {
    /// TRUST's actual departure time.
    pub actual: DateTime<Utc>,
    /// Against the public departure; `None` when no baseline is known or
    /// the gap is implausible.
    pub delay: Option<MeasuredDelay>,
}

#[derive(sqlx::FromRow)]
struct EventRow {
    ord: i64,
    actual_timestamp: DateTime<Utc>,
    planned_timestamp: Option<DateTime<Utc>>,
    gbtt_timestamp: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct CallRow {
    ord: i64,
    public_departure: Option<NaiveTime>,
    working_departure: Option<NaiveTime>,
}

/// The schedule's departure side at the boarding call of each target in
/// `indexes`, keyed by index. A target with no call there has no entry.
async fn schedule_sides(
    pool: &PgPool,
    targets: &[BoardingTarget],
    indexes: &[usize],
) -> anyhow::Result<std::collections::HashMap<usize, ScheduledSide>> {
    let mut sides = std::collections::HashMap::new();
    if indexes.is_empty() {
        return Ok(sides);
    }
    let ords: Vec<i64> = indexes
        .iter()
        .map(|&index| i64::try_from(index + 1))
        .collect::<Result<_, _>>()?;
    let dates: Vec<NaiveDate> = indexes.iter().map(|&i| targets[i].service_date).collect();
    let uids: Vec<String> = indexes
        .iter()
        .map(|&i| targets[i].train_uid.clone())
        .collect();
    let wanted: Vec<String> = indexes
        .iter()
        .map(|&i| targets[i].crs.trim().to_uppercase())
        .collect();
    let calls: Vec<CallRow> = sqlx::query_as(
        "SELECT DISTINCT ON (k.ord) k.ord, c.public_departure, \
                COALESCE(c.working_departure, c.booked_departure) AS working_departure \
         FROM UNNEST($1::bigint[], $2::date[], $3::text[], $4::text[]) \
              AS k(ord, service_date, uid, crs) \
         JOIN schedule_calling_points_full c \
           ON c.service_date = k.service_date AND c.uid = k.uid \
         JOIN LATERAL ( \
             SELECT UPPER(m.crs) AS crs FROM ( \
                 SELECT crs, 1 AS priority FROM tiploc_crs \
                  WHERE tiploc = UPPER(TRIM(c.tiploc)) \
                 UNION ALL \
                 SELECT crs, 2 AS priority FROM stanox_crs \
                  WHERE tiploc = UPPER(TRIM(c.tiploc)) \
                 UNION ALL \
                 SELECT crs, 3 AS priority FROM corpus_tiploc_crs \
                  WHERE $5 AND tiploc = UPPER(TRIM(c.tiploc)) \
             ) m ORDER BY m.priority LIMIT 1 \
         ) x ON x.crs = k.crs \
         WHERE COALESCE(c.working_departure, c.booked_departure) IS NOT NULL \
         ORDER BY k.ord, c.seq",
    )
    .bind(&ords)
    .bind(&dates)
    .bind(&uids)
    .bind(&wanted)
    .bind(crate::data::corpus_crosswalk::fallback_enabled())
    .fetch_all(pool)
    .await?;
    for call in calls {
        sides.insert(
            usize::try_from(call.ord - 1)?,
            ScheduledSide {
                public: call.public_departure,
                working: call.working_departure,
            },
        );
    }
    Ok(sides)
}

/// Each target's reported departure, in order (`None`: TRUST reported no
/// departure there). At most two queries for the whole slice, none for an
/// empty one.
pub async fn boarding_departures(
    pool: &PgPool,
    targets: &[BoardingTarget],
) -> anyhow::Result<Vec<Option<BoardingDeparture>>> {
    let mut out = vec![None; targets.len()];
    if targets.is_empty() {
        return Ok(out);
    }
    let ids: Vec<i64> = targets.iter().map(|t| t.trains_id).collect();
    let crs: Vec<String> = targets
        .iter()
        .map(|t| t.crs.trim().to_uppercase())
        .collect();
    // The latest report wins, as everywhere else a stop's movement is read.
    let events: Vec<EventRow> = sqlx::query_as(
        "SELECT DISTINCT ON (k.ord) k.ord, e.actual_timestamp, e.planned_timestamp, \
                e.gbtt_timestamp \
         FROM UNNEST($1::bigint[], $2::text[], $3::bigint[]) AS k(trains_id, crs, ord) \
         JOIN train_movement_events e \
           ON e.trains_id = k.trains_id AND UPPER(e.loc_crs) = k.crs \
         WHERE e.event_type = 'DEPARTURE' AND e.actual_timestamp IS NOT NULL \
         ORDER BY k.ord, e.received_at DESC, e.id DESC",
    )
    .bind(&ids)
    .bind(&crs)
    .bind((1..=i64::try_from(targets.len())?).collect::<Vec<i64>>())
    .fetch_all(pool)
    .await?;

    // The schedule's `public - working` gap, only where TRUST sent no
    // public time of its own.
    let needs_schedule: Vec<usize> = events
        .iter()
        .filter(|e| e.gbtt_timestamp.is_none())
        .map(|e| usize::try_from(e.ord - 1))
        .collect::<Result<_, _>>()?;
    let sides = schedule_sides(pool, targets, &needs_schedule).await?;

    for event in events {
        let index = usize::try_from(event.ord - 1)?;
        let gap = sides
            .get(&index)
            .copied()
            .and_then(ScheduledSide::public_minus_working);
        if let Some(slot) = out.get_mut(index) {
            *slot = Some(BoardingDeparture {
                actual: event.actual_timestamp,
                delay: reported_delay_with_gap(
                    Reported {
                        actual: event.actual_timestamp,
                        planned: event.planned_timestamp,
                        gbtt: event.gbtt_timestamp,
                    },
                    gap,
                ),
            });
        }
    }
    Ok(out)
}
