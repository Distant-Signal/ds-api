//! "Your reliability" (`GET /Journeys/reliability`, 2026-11-06): how the
//! trains the caller actually took ran, and what Delay Repay each finished
//! journey may be owed, from their journeys rather than their train
//! subscriptions (mid-journey changes spec 2026-10-22 §9).
//!
//! **Trains.** One entry per train leg of each journey's current plan,
//! once it is over (its date is past, or its outcome is recorded):
//!
//! - the leg's recorded outcome (`journey_legs.final_*`, `leg_outcomes`,
//!   DS-379), which outlives the train's own data (about 30 days);
//! - else, for a leg not recorded yet, its train's live state: the public
//!   delay at the leg's destination and whether it is cancelled.
//!
//! Plus, for every revision whose cause is the railway's (`cancelled`,
//! `part_cancelled`, `connection_missed`, `skipped_stop`), the booked
//! train it replaced (the superseded revision's leg recorded as cancelled,
//! part cancelled or missed), counted as a cancellation. A train the
//! traveller chose to leave (`user_choice`), or missed through their own
//! fault (`user_missed`), is not counted against the railway, and a train
//! swapped while still planning was never travelled: revision 1 is simply
//! rewritten, so it leaves no trace here. Subscriptions are never read as
//! the population: every switch deletes the old one, and one subscription
//! can serve two journeys' legs.
//!
//! **Delay Repay**, once per journey (§9.3): the delay at the journey's
//! final destination against the compensation baseline's booked arrival
//! (§9.2). With no change after booking (or after a passenger-fault or
//! chosen change, which moves the baseline) that is the last train leg's
//! own delay; after a railway-fault change it is the end-to-end delay
//! against the booked plan, and a re-route to a different final station
//! gets no percentage. The scheme is the baseline's last train's operator
//! (or the one company a leg's ticket is valid on), and a leg's ticket
//! type says single or return.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use sqlx::PgPool;

use crate::data::delay_repay_rules::{self, DelayRepayFields, Outcome};
use crate::data::leg_tickets::LegTicket;
use crate::data::stop_delay::{DelayBasis, StopDelay};

/// Journeys read at most, newest first: about a year of daily commuting.
pub const MAX_JOURNEYS: i64 = 400;

/// The revision causes that are the railway's fault (§4.2).
pub const OPERATOR_FAULT_CAUSES: &[&str] = &[
    "cancelled",
    "part_cancelled",
    "connection_missed",
    "skipped_stop",
];

/// One train the caller took, or was booked on until the railway let
/// them down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "ReliabilityTrain")]
pub struct ReliabilityTrain {
    pub journey_id: i64,
    /// The current plan's leg; `null` for a replaced booked train.
    pub leg_id: Option<i64>,
    pub train_uid: Option<String>,
    pub service_date: NaiveDate,
    pub origin_crs: Option<String>,
    pub destination_crs: Option<String>,
    pub destination_name: Option<String>,
    /// The public delay at the leg's destination, in minutes; `null` when
    /// not known, or for a cancellation.
    pub delay_minutes: Option<i32>,
    pub cancelled: bool,
    /// `true` when this is the leg's recorded outcome; `false` when it was
    /// read from the train's live state (not recorded yet).
    pub recorded: bool,
    /// Set for a booked train the railway's fault replaced: the cause of
    /// the change (`cancelled`, `part_cancelled`, `connection_missed` or
    /// `skipped_stop`). It counts as a cancellation.
    #[ts(as = "Option<crate::ts_values::RevisionCause>")]
    pub replaced_because: Option<String>,
}

/// One finished journey's Delay Repay estimate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "ReliabilityJourney")]
pub struct ReliabilityJourney {
    pub journey_id: i64,
    pub custom_name: Option<String>,
    /// The last train leg's date.
    pub service_date: NaiveDate,
    /// Where the delay is measured: the booked plan's final station.
    pub destination_crs: Option<String>,
    pub destination_name: Option<String>,
    /// The baseline's booked (public) arrival there, when known.
    pub booked_arrival: Option<DateTime<Utc>>,
    /// The arrival there on the current plan, when known.
    pub actual_arrival: Option<DateTime<Utc>>,
    /// Measured against the booked plan after the railway changed it.
    pub against_booked_plan: bool,
    /// The current plan ends somewhere else: no percentage.
    pub different_destination: bool,
    #[serde(flatten)]
    pub delay_repay: DelayRepayFields,
}

/// `GET /Journeys/reliability`'s body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "ReliabilityResponse")]
pub struct ReliabilityResponse {
    /// Newest first.
    pub trains: Vec<ReliabilityTrain>,
    /// One per finished journey whose arrival is known, newest first.
    pub journeys: Vec<ReliabilityJourney>,
}

/// A current train leg, as read.
#[derive(Debug, Clone, sqlx::FromRow)]
struct LegRow {
    leg_id: i64,
    journey_id: i64,
    custom_name: Option<String>,
    leg_order: i32,
    service_date: NaiveDate,
    subscription_id: Option<i64>,
    trains_id: Option<i64>,
    train_uid: Option<String>,
    origin_crs: Option<String>,
    destination_crs: Option<String>,
    destination_name: Option<String>,
    final_delay: Option<i32>,
    final_cancelled: Option<bool>,
    recorded: bool,
    status: Option<String>,
    working_delay: Option<i32>,
    /// The leg's booked public arrival in the latest revision.
    booked_arrival: Option<DateTime<Utc>>,
    atoc_code: Option<String>,
}

static LEGS_SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    use ds_store::trains::{LegEnd, leg_crs_sql};
    let origin = leg_crs_sql(LegEnd::Origin, "jl", "ts", "tr");
    let destination = leg_crs_sql(LegEnd::Destination, "jl", "ts", "tr");
    format!(
        "WITH recent AS ( \
             SELECT j.id FROM journeys j \
             WHERE j.user_id = $1 \
               AND EXISTS (SELECT 1 FROM journey_legs l WHERE l.journey_id = j.id \
                             AND l.kind = 'train' \
                             AND (l.service_date < $2 OR l.final_recorded_at IS NOT NULL)) \
             ORDER BY (SELECT MAX(l.service_date) FROM journey_legs l \
                        WHERE l.journey_id = j.id) DESC, j.id DESC \
             LIMIT $3) \
         SELECT jl.id AS leg_id, jl.journey_id, j.custom_name, jl.leg_order, jl.service_date, \
                jl.train_subscription_id AS subscription_id, ts.trains_id, \
                COALESCE(tr.train_uid, rl.train_uid) AS train_uid, \
                UPPER(COALESCE({origin}, rl.origin_crs)) AS origin_crs, \
                UPPER(COALESCE({destination}, rl.destination_crs)) AS destination_crs, \
                sd.name AS destination_name, \
                jl.final_arrival_delay_minutes AS final_delay, \
                jl.final_train_cancelled AS final_cancelled, \
                jl.final_recorded_at IS NOT NULL AS recorded, \
                cs.status, cs.delay_minutes AS working_delay, \
                rl.booked_arrival, rl.atoc_code \
         FROM journey_legs jl \
         JOIN recent ON recent.id = jl.journey_id \
         JOIN journeys j ON j.id = jl.journey_id \
         LEFT JOIN train_subscriptions ts ON ts.id = jl.train_subscription_id \
         LEFT JOIN trains tr ON tr.id = ts.trains_id \
         LEFT JOIN train_current_state cs ON cs.trains_id = ts.trains_id \
         LEFT JOIN LATERAL ( \
             SELECT l.train_uid, l.origin_crs, l.destination_crs, l.booked_arrival, l.atoc_code \
             FROM journey_revisions r \
             JOIN journey_revision_legs l ON l.revision_id = r.id \
             WHERE r.journey_id = jl.journey_id AND l.source_leg_id = jl.id \
             ORDER BY r.revision_no DESC LIMIT 1) rl ON TRUE \
         LEFT JOIN stations sd \
             ON sd.crs = UPPER(COALESCE({destination}, rl.destination_crs))::bpchar \
         WHERE jl.kind = 'train' \
           AND (jl.service_date < $2 OR jl.final_recorded_at IS NOT NULL) \
         ORDER BY jl.service_date DESC, jl.journey_id DESC, jl.leg_order"
    )
});

/// A booked train the railway's fault replaced, as read.
#[derive(Debug, Clone, sqlx::FromRow)]
struct ReplacedRow {
    journey_id: i64,
    cause: String,
    train_uid: Option<String>,
    service_date: NaiveDate,
    origin_crs: Option<String>,
    destination_crs: Option<String>,
    destination_name: Option<String>,
}

const REPLACED_SQL: &str = "\
SELECT r.journey_id, r.cause, p.train_uid, p.service_date, p.origin_crs, p.destination_crs, \
       sd.name AS destination_name \
FROM journey_revisions r \
JOIN journey_revisions pr ON pr.journey_id = r.journey_id AND pr.revision_no = r.revision_no - 1 \
JOIN journey_revision_legs p ON p.revision_id = pr.id \
LEFT JOIN stations sd ON sd.crs = UPPER(p.destination_crs)::bpchar \
WHERE r.journey_id = ANY($1) AND r.cause = ANY($2) \
  AND p.kind = 'train' AND p.fate IN ('cancelled', 'part_cancelled', 'missed') \
ORDER BY r.journey_id, r.revision_no, p.position";

/// A journey's compensation baseline (§9.2): its last train leg.
#[derive(Debug, Clone, sqlx::FromRow)]
struct BaselineRow {
    journey_id: i64,
    /// Whether a later revision exists.
    superseded: bool,
    /// Whether a railway-fault change came after it (one of
    /// [`OPERATOR_FAULT_CAUSES`], or a miss the data blamed on a late
    /// inbound train, §4.2).
    operator_fault_after: bool,
    destination_crs: Option<String>,
    destination_name: Option<String>,
    booked_arrival: Option<DateTime<Utc>>,
    atoc_code: Option<String>,
}

const BASELINE_SQL: &str = "\
SELECT DISTINCT ON (b.journey_id) b.journey_id, \
       EXISTS (SELECT 1 FROM journey_revisions n \
                WHERE n.journey_id = b.journey_id AND n.revision_no > b.revision_no) AS superseded, \
       EXISTS (SELECT 1 FROM journey_revisions n \
                WHERE n.journey_id = b.journey_id AND n.revision_no > b.revision_no \
                  AND (n.cause = ANY($2) OR n.fault = 'operator')) AS operator_fault_after, \
       UPPER(l.destination_crs) AS destination_crs, sd.name AS destination_name, \
       l.booked_arrival, l.atoc_code \
FROM journey_revisions b \
JOIN journey_revision_legs l ON l.revision_id = b.id AND l.kind = 'train' \
LEFT JOIN stations sd ON sd.crs = UPPER(l.destination_crs)::bpchar \
WHERE b.journey_id = ANY($1) AND b.is_compensation_baseline \
ORDER BY b.journey_id, b.revision_no DESC, l.position DESC";

/// How one current leg ran, as far as is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LegRun {
    delay: Option<StopDelay>,
    cancelled: bool,
    recorded: bool,
}

/// [`LegRun`] from the recorded outcome, else the live state (`live` is
/// the public delay at the leg's destination, read for the unrecorded
/// legs whose train is still held). `None` when nothing is known.
fn leg_run(row: &LegRow, live: Option<StopDelay>) -> Option<LegRun> {
    if row.recorded {
        let cancelled = row.final_cancelled.unwrap_or(false);
        if row.final_delay.is_none() && !cancelled {
            return None;
        }
        return Some(LegRun {
            delay: row.final_delay.map(|minutes| StopDelay {
                minutes,
                basis: DelayBasis::Public,
                provisional: false,
            }),
            cancelled,
            recorded: true,
        });
    }
    let cancelled = row.status.as_deref() == Some("cancelled");
    if live.is_none() && !cancelled {
        return None;
    }
    Some(LegRun {
        delay: live,
        cancelled,
        recorded: false,
    })
}

/// Everything [`assemble`] reads, so it can stay pure.
struct Readings {
    legs: Vec<LegRow>,
    live: HashMap<i64, StopDelay>,
    replaced: Vec<ReplacedRow>,
    baselines: HashMap<i64, BaselineRow>,
    tickets: HashMap<i64, LegTicket>,
}

/// The reliability of `user_id`'s journeys as of `today` (London).
pub async fn for_user(
    pool: &PgPool,
    user_id: &str,
    today: NaiveDate,
) -> anyhow::Result<ReliabilityResponse> {
    let legs: Vec<LegRow> = sqlx::query_as(&LEGS_SQL)
        .bind(user_id)
        .bind(today)
        .bind(MAX_JOURNEYS)
        .fetch_all(pool)
        .await?;
    if legs.is_empty() {
        return Ok(ReliabilityResponse::default());
    }
    let mut journey_ids: Vec<i64> = legs.iter().map(|leg| leg.journey_id).collect();
    journey_ids.sort_unstable();
    journey_ids.dedup();

    // The live fallback: the public delay at each unrecorded leg's own
    // destination, for the trains still held.
    let unrecorded: Vec<(i64, crate::data::stop_delay::StopDelayTarget)> = legs
        .iter()
        .filter(|leg| !leg.recorded)
        .filter_map(|leg| {
            let target = crate::data::stop_delay::target(
                leg.trains_id,
                leg.train_uid.as_deref(),
                leg.service_date,
                leg.destination_crs.as_deref(),
                leg.working_delay,
            )?;
            target.stop_crs.is_some().then_some((leg.leg_id, target))
        })
        .collect();
    let live: HashMap<i64, StopDelay> = if unrecorded.is_empty() {
        HashMap::new()
    } else {
        let targets: Vec<_> = unrecorded.iter().map(|(_, t)| t.clone()).collect();
        let delays = crate::data::stop_delay::stop_delays(pool, &targets).await?;
        unrecorded
            .iter()
            .zip(delays)
            .filter_map(|((leg_id, _), delay)| Some((*leg_id, delay?)))
            .collect()
    };

    let replaced: Vec<ReplacedRow> = sqlx::query_as(REPLACED_SQL)
        .bind(&journey_ids)
        .bind(OPERATOR_FAULT_CAUSES)
        .fetch_all(pool)
        .await?;
    let baselines: HashMap<i64, BaselineRow> = sqlx::query_as::<_, BaselineRow>(BASELINE_SQL)
        .bind(&journey_ids)
        .bind(OPERATOR_FAULT_CAUSES)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(|row| (row.journey_id, row))
        .collect();
    let subscription_ids: Vec<i64> = legs.iter().filter_map(|leg| leg.subscription_id).collect();
    let tickets =
        crate::data::leg_tickets::for_subscriptions(pool, user_id, &subscription_ids).await?;
    Ok(assemble(&Readings {
        legs,
        live,
        replaced,
        baselines,
        tickets,
    }))
}

fn is_irish(uid: Option<&str>) -> bool {
    uid.is_some_and(common::island_of_ireland::tracking::is_train_uid)
}

/// Pure: the response from what was read.
fn assemble(readings: &Readings) -> ReliabilityResponse {
    let mut response = ReliabilityResponse::default();
    let runs: HashMap<i64, LegRun> = readings
        .legs
        .iter()
        .filter_map(|leg| {
            Some((
                leg.leg_id,
                leg_run(leg, readings.live.get(&leg.leg_id).copied())?,
            ))
        })
        .collect();

    response.trains = trains(readings, &runs);

    // Delay Repay, once per journey, on its last current train leg.
    let mut last_legs: Vec<&LegRow> = Vec::new();
    for leg in &readings.legs {
        match last_legs
            .iter_mut()
            .find(|l| l.journey_id == leg.journey_id)
        {
            Some(last) if last.leg_order < leg.leg_order => *last = leg,
            Some(_) => {}
            None => last_legs.push(leg),
        }
    }
    // A journey's last train leg must itself be over: `legs` holds only
    // finished ones, so a journey with a later leg still to run is skipped.
    for last in last_legs {
        if is_irish(last.train_uid.as_deref()) {
            continue;
        }
        let Some(run) = runs.get(&last.leg_id) else {
            continue;
        };
        let ticket = readings
            .legs
            .iter()
            .filter(|leg| leg.journey_id == last.journey_id)
            .filter_map(|leg| leg.subscription_id)
            .find_map(|id| readings.tickets.get(&id));
        if let Some(journey) =
            journey_delay_repay(last, *run, readings.baselines.get(&last.journey_id), ticket)
        {
            response.journeys.push(journey);
        }
    }
    response.journeys.sort_by(|a, b| {
        b.service_date
            .cmp(&a.service_date)
            .then(b.journey_id.cmp(&a.journey_id))
    });
    response
}

/// The trains counted: the current legs, then each journey's replaced
/// booked trains, interleaved by date (newest first).
fn trains(readings: &Readings, runs: &HashMap<i64, LegRun>) -> Vec<ReliabilityTrain> {
    let mut trains = Vec::new();
    let current: HashSet<(i64, Option<&str>, NaiveDate)> = readings
        .legs
        .iter()
        .map(|leg| (leg.journey_id, leg.train_uid.as_deref(), leg.service_date))
        .collect();
    for leg in &readings.legs {
        let Some(run) = runs.get(&leg.leg_id) else {
            continue;
        };
        trains.push(ReliabilityTrain {
            journey_id: leg.journey_id,
            leg_id: Some(leg.leg_id),
            train_uid: leg.train_uid.clone(),
            service_date: leg.service_date,
            origin_crs: leg.origin_crs.clone(),
            destination_crs: leg.destination_crs.clone(),
            destination_name: leg.destination_name.clone(),
            delay_minutes: run.delay.map(|d| d.minutes),
            cancelled: run.cancelled,
            recorded: run.recorded,
            replaced_because: None,
        });
    }
    let mut seen: HashSet<(i64, Option<String>, NaiveDate)> = HashSet::new();
    for row in &readings.replaced {
        // Undone back to the booked train, or counted already.
        if row.train_uid.is_some()
            && current.contains(&(row.journey_id, row.train_uid.as_deref(), row.service_date))
        {
            continue;
        }
        if row.train_uid.is_some()
            && !seen.insert((row.journey_id, row.train_uid.clone(), row.service_date))
        {
            continue;
        }
        trains.push(ReliabilityTrain {
            journey_id: row.journey_id,
            leg_id: None,
            train_uid: row.train_uid.clone(),
            service_date: row.service_date,
            origin_crs: row.origin_crs.clone(),
            destination_crs: row.destination_crs.clone(),
            destination_name: row.destination_name.clone(),
            delay_minutes: None,
            cancelled: true,
            recorded: true,
            replaced_because: Some(row.cause.clone()),
        });
    }
    trains.sort_by(|a, b| {
        b.service_date
            .cmp(&a.service_date)
            .then(b.journey_id.cmp(&a.journey_id))
    });
    trains
}

/// One journey's Delay Repay (§9.1, §9.2).
fn journey_delay_repay(
    last: &LegRow,
    run: LegRun,
    baseline: Option<&BaselineRow>,
    ticket: Option<&LegTicket>,
) -> Option<ReliabilityJourney> {
    // Measured against the booked plan only after the railway changed it;
    // otherwise the baseline is the plan travelled, and the last leg's own
    // delay is the journey's.
    let booked = baseline.filter(|b| b.superseded && b.operator_fault_after);
    let outcome = run.cancelled.then_some(Outcome::NotReached);
    let atoc = booked
        .and_then(|b| b.atoc_code.clone())
        .or_else(|| last.atoc_code.clone())
        .or_else(|| baseline.and_then(|b| b.atoc_code.clone()));
    let (destination_crs, destination_name, booked_arrival, actual_arrival, delay, different) =
        match booked {
            None => (
                last.destination_crs.clone(),
                last.destination_name.clone(),
                last.booked_arrival,
                last.booked_arrival
                    .zip(run.delay)
                    .map(|(at, d)| at + chrono::Duration::minutes(i64::from(d.minutes))),
                run.delay,
                false,
            ),
            Some(b) => {
                let different = b.destination_crs.is_some()
                    && last.destination_crs.is_some()
                    && b.destination_crs != last.destination_crs;
                let actual = last
                    .booked_arrival
                    .zip(run.delay)
                    .map(|(at, d)| at + chrono::Duration::minutes(i64::from(d.minutes)));
                let delay = match (b.booked_arrival, actual, run.delay) {
                    (Some(booked_at), Some(actual_at), Some(leg)) if !different => {
                        let minutes = (actual_at - booked_at).num_minutes();
                        Some(StopDelay {
                            minutes: i32::try_from(minutes).unwrap_or(i32::MAX),
                            basis: leg.basis,
                            provisional: leg.provisional,
                        })
                    }
                    _ => None,
                };
                (
                    b.destination_crs.clone(),
                    b.destination_name.clone(),
                    b.booked_arrival,
                    actual,
                    delay,
                    different,
                )
            }
        };
    if delay.is_none() && outcome.is_none() && !different {
        return None;
    }
    let ticket_type = ticket.map(LegTicket::kind_text);
    Some(ReliabilityJourney {
        journey_id: last.journey_id,
        custom_name: last.custom_name.clone(),
        service_date: last.service_date,
        destination_crs: destination_crs.clone(),
        destination_name: destination_name.clone(),
        booked_arrival,
        actual_arrival: actual_arrival.filter(|_| !run.cancelled),
        against_booked_plan: booked.is_some(),
        different_destination: different,
        delay_repay: delay_repay_rules::assess(delay_repay_rules::AssessInputs {
            operator: ticket.and_then(LegTicket::operator),
            ticket_type,
            atoc_code: atoc.as_deref(),
            delay: delay.filter(|_| !different),
            outcome,
            measured_at_crs: destination_crs.as_deref(),
            measured_at_name: destination_name.as_deref(),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(journey_id: i64, leg_id: i64, order: i32) -> LegRow {
        LegRow {
            leg_id,
            journey_id,
            custom_name: None,
            leg_order: order,
            service_date: "2026-11-02".parse().unwrap(),
            subscription_id: Some(leg_id * 10),
            trains_id: None,
            train_uid: Some(format!("T{leg_id:05}")),
            origin_crs: Some("AAA".into()),
            destination_crs: Some("BBB".into()),
            destination_name: None,
            final_delay: None,
            final_cancelled: None,
            recorded: false,
            status: None,
            working_delay: None,
            booked_arrival: Some("2026-11-02T09:00:00Z".parse().unwrap()),
            atoc_code: Some("GR".into()),
        }
    }

    fn recorded(mut row: LegRow, delay: Option<i32>, cancelled: bool) -> LegRow {
        row.recorded = true;
        row.final_delay = delay;
        row.final_cancelled = Some(cancelled);
        row
    }

    fn readings(legs: Vec<LegRow>) -> Readings {
        Readings {
            legs,
            live: HashMap::new(),
            replaced: Vec::new(),
            baselines: HashMap::new(),
            tickets: HashMap::new(),
        }
    }

    #[test]
    fn a_recorded_outcome_wins_and_the_live_state_fills_in_for_the_rest() {
        let mut live_leg = leg(2, 21, 1);
        live_leg.status = Some("late".into());
        let mut input = readings(vec![
            recorded(leg(1, 11, 1), Some(7), false),
            live_leg,
            leg(3, 31, 1),
        ]);
        input.live.insert(
            21,
            StopDelay {
                minutes: 4,
                basis: DelayBasis::Public,
                provisional: true,
            },
        );
        let response = assemble(&input);
        let by_leg = |id| response.trains.iter().find(|t| t.leg_id == Some(id));
        assert_eq!(by_leg(11).unwrap().delay_minutes, Some(7));
        assert!(by_leg(11).unwrap().recorded);
        assert_eq!(by_leg(21).unwrap().delay_minutes, Some(4));
        assert!(!by_leg(21).unwrap().recorded);
        assert!(by_leg(31).is_none(), "nothing known, nothing counted");
    }

    #[test]
    fn a_replaced_booked_train_counts_once_as_a_cancellation_unless_it_came_back() {
        let mut input = readings(vec![recorded(leg(1, 11, 1), Some(5), false)]);
        let replaced = |uid: &str| ReplacedRow {
            journey_id: 1,
            cause: "cancelled".into(),
            train_uid: Some(uid.into()),
            service_date: "2026-11-02".parse().unwrap(),
            origin_crs: Some("AAA".into()),
            destination_crs: Some("BBB".into()),
            destination_name: None,
        };
        input.replaced = vec![replaced("BOOKED"), replaced("BOOKED"), replaced("T00011")];
        let response = assemble(&input);
        let cancelled: Vec<_> = response.trains.iter().filter(|t| t.cancelled).collect();
        assert_eq!(cancelled.len(), 1, "{:?}", response.trains);
        assert_eq!(cancelled[0].train_uid.as_deref(), Some("BOOKED"));
        assert_eq!(cancelled[0].replaced_because.as_deref(), Some("cancelled"));
        assert_eq!(cancelled[0].leg_id, None);
    }

    fn baseline(superseded: bool, operator_fault_after: bool) -> BaselineRow {
        BaselineRow {
            journey_id: 1,
            superseded,
            operator_fault_after,
            destination_crs: Some("BBB".into()),
            destination_name: Some("Bee".into()),
            booked_arrival: Some("2026-11-02T08:30:00Z".parse().unwrap()),
            atoc_code: Some("GR".into()),
        }
    }

    #[test]
    fn after_a_railway_fault_change_the_delay_is_against_the_booked_arrival() {
        // Booked to arrive 08:30; the replacement was booked for 09:00 and
        // ran 12 minutes late: 42 minutes late against the booked plan.
        let mut input = readings(vec![recorded(leg(1, 11, 1), Some(12), false)]);
        input.baselines.insert(1, baseline(true, true));
        let response = assemble(&input);
        let journey = &response.journeys[0];
        assert!(journey.against_booked_plan);
        assert_eq!(journey.delay_repay.delay_minutes, Some(42));
        assert_eq!(
            journey.actual_arrival,
            Some("2026-11-02T09:12:00Z".parse().unwrap())
        );
        let estimate = journey.delay_repay.estimate.as_ref().expect("an estimate");
        assert_eq!(estimate.band_minutes, 30);
    }

    #[test]
    fn with_no_railway_fault_change_the_last_legs_own_delay_is_the_journeys() {
        for base in [
            None,
            Some(baseline(false, false)),
            Some(baseline(true, false)),
        ] {
            let mut input = readings(vec![
                recorded(leg(1, 10, 1), Some(50), false),
                recorded(leg(1, 11, 2), Some(16), false),
            ]);
            if let Some(base) = base {
                input.baselines.insert(1, base);
            }
            let response = assemble(&input);
            assert_eq!(response.journeys.len(), 1);
            assert_eq!(response.journeys[0].delay_repay.delay_minutes, Some(16));
            assert!(!response.journeys[0].against_booked_plan);
        }
    }

    #[test]
    fn a_different_final_station_gets_no_percentage() {
        let mut last = recorded(leg(1, 11, 1), Some(5), false);
        last.destination_crs = Some("CCC".into());
        let mut input = readings(vec![last]);
        input.baselines.insert(1, baseline(true, true));
        let journey = &assemble(&input).journeys[0];
        assert!(journey.different_destination);
        assert_eq!(journey.delay_repay.estimate, None);
        assert_eq!(journey.delay_repay.delay_minutes, None);
    }

    #[test]
    fn a_ticket_type_says_single_or_return_and_no_ticket_shows_both() {
        let mut input = readings(vec![recorded(leg(1, 11, 1), Some(130), false)]);
        let response = assemble(&input);
        let estimate = response.journeys[0].delay_repay.estimate.clone().unwrap();
        assert_eq!(estimate.fare_basis, "return");
        assert_eq!(estimate.ticket_kind, delay_repay_rules::TicketKind::Unknown);
        input.tickets.insert(
            110,
            LegTicket {
                ticket_type: crate::data::leg_tickets::TicketType::Single,
                operator_only: false,
                operator_code: None,
                name: None,
                source: crate::data::leg_tickets::TicketSource::Manual,
                departure_date: None,
                id: 1,
            },
        );
        let estimate = assemble(&input).journeys[0]
            .delay_repay
            .estimate
            .clone()
            .unwrap();
        assert_eq!(estimate.fare_basis, "single");
        assert_eq!(estimate.ticket_kind, delay_repay_rules::TicketKind::Single);
    }

    #[test]
    fn a_cancelled_last_train_is_not_reached_and_an_irish_one_is_skipped() {
        let mut irish = recorded(leg(2, 21, 1), Some(40), false);
        irish.train_uid = Some("ie:A120".into());
        let input = readings(vec![recorded(leg(1, 11, 1), None, true), irish]);
        let response = assemble(&input);
        assert_eq!(response.journeys.len(), 1);
        assert_eq!(
            response.journeys[0].delay_repay.outcome,
            Some(Outcome::NotReached)
        );
        assert_eq!(response.journeys[0].actual_arrival, None);
        assert_eq!(response.trains.len(), 2, "the Irish train still counts");
    }
}
