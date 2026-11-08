//! Regular journeys, phase 3 (distant-signal's
//! docs/superpowers/specs/2026-11-06-regular-journeys-design.md §6): the
//! day's train for an occurrence of a regular journey with a target, as the
//! plan sweep (`regular_plan_sweep`) chooses it and the journey page shows
//! it.
//!
//! An occurrence's plan lives on its `journeys` row:
//!
//! - `regular_plan`: a [`StoredPlan`], the choice, up to
//!   [`MAX_ALTERNATIVES`] alternatives, why it was chosen, and the train
//!   legs the sweep committed itself (`sweepLegIds`);
//! - `regular_plan_status`: `pending` (nothing planned yet: no timetable
//!   for the day, or the target already passed), `suggested` (remind mode:
//!   nothing committed), `committed` (auto mode committed it, or the owner
//!   accepted a suggestion) or `no_service` (no itinerary on a published
//!   day);
//! - `regular_plan_marker`: the schedule publish it was planned against;
//! - `regular_planned_at`: when, which the accept route checks.
//!
//! The choice ([`choose`]), in order: the itinerary whose trains are the
//! usual itinerary's; else one whose first train leaves within
//! [`SAME_OPERATOR_WINDOW_MINUTES`] of the usual first departure with the
//! same operator (the nearest); else the target's own best (earliest
//! arrival for `depart_after`, latest departure for `arrive_by`).

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Timelike, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::types::Json;

use crate::data::journey_alternatives::{self, Planned, SuggestionLeg};
use crate::data::journeys::{ItineraryLeg, UntrackedLeg};
use crate::data::regular_journeys::UsualItinerary;
use crate::data::trip_planning_itinerary::PlannedItinerary;

/// `journeys.regular_plan_status` (its CHECK constraint).
pub const STATUSES: [&str; 4] = ["pending", "suggested", "committed", "no_service"];

/// At most this many alternatives are kept besides the choice (DS-398:
/// four, up from two; the sweep looks at up to six trains).
pub const MAX_ALTERNATIVES: usize = 4;

/// How far the first train may leave from the usual first departure and
/// still count as "the same train" when the operator matches (a timetable
/// change gives a train a new UID).
pub const SAME_OPERATOR_WINDOW_MINUTES: i64 = 5;

/// One leg of a planned option: a train to track, or a Tube ride, bus or
/// walk saved as an untracked leg (as `POST /Journeys/itinerary` takes
/// it). Times are the timetable's (the booked plan).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(rename = "RegularPlanLeg")]
pub enum PlanLeg {
    Train {
        train_uid: String,
        service_date: NaiveDate,
        origin_crs: Option<String>,
        origin_name: Option<String>,
        destination_crs: Option<String>,
        destination_name: Option<String>,
        departure: DateTime<Utc>,
        arrival: DateTime<Utc>,
        operator: Option<String>,
        operator_name: Option<String>,
    },
    Untracked {
        #[ts(as = "crate::ts_values::TransitMode")]
        #[schemars(with = "crate::ts_values::TransitMode")]
        transit_mode: String,
        origin_name: Option<String>,
        destination_name: Option<String>,
        line_name: Option<String>,
        origin_stop_id: Option<String>,
        destination_stop_id: Option<String>,
        planned_departure: DateTime<Utc>,
        planned_arrival: DateTime<Utc>,
    },
}

/// One way to make the day's journey.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "RegularPlanOption")]
pub struct PlanOption {
    pub departure: DateTime<Utc>,
    pub arrival: DateTime<Utc>,
    pub change_count: u32,
    pub legs: Vec<PlanLeg>,
}

impl PlanOption {
    /// The trains, in order: what identifies an option across two plans.
    pub fn train_key(&self) -> Vec<(String, NaiveDate)> {
        self.legs
            .iter()
            .filter_map(|leg| match leg {
                PlanLeg::Train {
                    train_uid,
                    service_date,
                    ..
                } => Some((train_uid.clone(), *service_date)),
                PlanLeg::Untracked { .. } => None,
            })
            .collect()
    }

    /// The first train's departure and operator.
    fn first_train(&self) -> Option<(DateTime<Utc>, Option<&str>)> {
        self.legs.iter().find_map(|leg| match leg {
            PlanLeg::Train {
                departure,
                operator,
                ..
            } => Some((*departure, operator.as_deref())),
            PlanLeg::Untracked { .. } => None,
        })
    }

    /// The legs to save, with each train resolved (train UID and date to
    /// the shared `trains` row id). `None` when one isn't.
    pub fn itinerary_legs(
        &self,
        trains_ids: &HashMap<(String, NaiveDate), i64>,
    ) -> Option<Vec<ItineraryLeg>> {
        self.legs
            .iter()
            .map(|leg| match leg {
                PlanLeg::Train {
                    train_uid,
                    service_date,
                    origin_crs,
                    destination_crs,
                    ..
                } => Some(ItineraryLeg::Train {
                    trains_id: *trains_ids.get(&(train_uid.clone(), *service_date))?,
                    service_date: *service_date,
                    origin_crs: origin_crs.clone(),
                    destination_crs: destination_crs.clone(),
                }),
                PlanLeg::Untracked {
                    transit_mode,
                    origin_name,
                    destination_name,
                    line_name,
                    origin_stop_id,
                    destination_stop_id,
                    planned_departure,
                    planned_arrival,
                } => Some(ItineraryLeg::Untracked(UntrackedLeg {
                    transit_mode: transit_mode.clone(),
                    origin_place: origin_name.clone(),
                    destination_place: destination_name.clone(),
                    line_name: line_name.clone(),
                    planned_departure: *planned_departure,
                    planned_arrival: *planned_arrival,
                    origin_stop_id: origin_stop_id.clone(),
                    destination_stop_id: destination_stop_id.clone(),
                    timetable: None,
                })),
            })
            .collect()
    }
}

/// Why the choice was made.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[ts(rename = "RegularPlanReason")]
pub enum ChoiceReason {
    /// The usual itinerary's trains.
    Usual,
    /// The usual operator, within 5 minutes of the usual departure.
    SameOperator,
    /// The target's own best: the usual train doesn't run that day.
    Best,
}

/// `journeys.regular_plan`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredPlan {
    #[serde(default)]
    pub reason: Option<ChoiceReason>,
    #[serde(default)]
    pub choice: Option<PlanOption>,
    #[serde(default)]
    pub alternatives: Vec<PlanOption>,
    /// The train legs the sweep committed itself (auto mode); empty
    /// otherwise.
    #[serde(default)]
    pub sweep_leg_ids: Vec<i64>,
    /// The option the owner accepted (0: the choice, then the
    /// alternatives), once they have.
    #[serde(default)]
    pub accepted_index: Option<u8>,
}

impl StoredPlan {
    /// The choice, then the alternatives: what an accept's `index` counts.
    pub fn option(&self, index: usize) -> Option<&PlanOption> {
        self.choice.iter().chain(&self.alternatives).nth(index)
    }
}

/// `GET /Journeys/{id}`'s `regularPlan` (owner only): the day's train for
/// a regular journey's occurrence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "RegularPlan")]
pub struct RegularPlanResponse {
    /// `pending`, `suggested`, `committed` or `no_service`.
    #[ts(as = "crate::ts_values::RegularPlanStatus")]
    #[schemars(with = "crate::ts_values::RegularPlanStatus")]
    pub status: String,
    /// When it was planned: what `POST .../regular-plan/accept` echoes
    /// back.
    pub planned_at: Option<DateTime<Utc>>,
    /// `null` while `pending` or `no_service`.
    pub reason: Option<ChoiceReason>,
    pub choice: Option<PlanOption>,
    pub alternatives: Vec<PlanOption>,
    /// The option the owner accepted (0: the choice), `null` until then.
    pub accepted_index: Option<u8>,
}

/// The planner's itineraries for `date` as options, in the planner's
/// order, leaving out any with a cancelled train (known from live data),
/// any that leaves before `now`, and any whose times can't be placed.
#[expect(
    clippy::implicit_hasher,
    reason = "the names come from journey_alternatives' lookups, std hasher maps"
)]
pub fn options_from(
    itineraries: &[PlannedItinerary],
    date: NaiveDate,
    now: DateTime<Utc>,
    names: &HashMap<String, String>,
    operator_names: &HashMap<String, String>,
) -> Vec<PlanOption> {
    itineraries
        .iter()
        .filter_map(|itinerary| {
            let planned = Planned {
                itinerary: itinerary.clone(),
                date,
                through_leg_id: 0,
                rejoins_leg_id: None,
                operator_only: None,
                outside_operator_filter: false,
            };
            let legs = journey_alternatives::suggestion_legs(&planned, names, operator_names)?;
            if legs.iter().any(|leg| {
                matches!(
                    leg,
                    SuggestionLeg::Train {
                        cancelled: true,
                        ..
                    }
                )
            }) || !legs
                .iter()
                .any(|leg| matches!(leg, SuggestionLeg::Train { .. }))
            {
                return None;
            }
            let legs: Vec<PlanLeg> = legs.into_iter().map(plan_leg).collect();
            let departure = legs.first().map(leg_departure)?;
            let arrival = legs.last().map(leg_arrival)?;
            (departure >= now).then_some(PlanOption {
                departure,
                arrival,
                change_count: itinerary.change_count,
                legs,
            })
        })
        .collect()
}

fn plan_leg(leg: SuggestionLeg) -> PlanLeg {
    match leg {
        SuggestionLeg::Train {
            train_uid,
            service_date,
            origin_crs,
            origin_name,
            destination_crs,
            destination_name,
            scheduled_departure,
            scheduled_arrival,
            operator,
            operator_name,
            ..
        } => PlanLeg::Train {
            train_uid,
            service_date,
            origin_crs,
            origin_name,
            destination_crs,
            destination_name,
            departure: scheduled_departure,
            arrival: scheduled_arrival,
            operator,
            operator_name,
        },
        SuggestionLeg::Untracked {
            transit_mode,
            origin_name,
            destination_name,
            line_name,
            origin_stop_id,
            destination_stop_id,
            planned_departure,
            planned_arrival,
        } => PlanLeg::Untracked {
            transit_mode,
            origin_name,
            destination_name,
            line_name,
            origin_stop_id,
            destination_stop_id,
            planned_departure,
            planned_arrival,
        },
    }
}

fn leg_departure(leg: &PlanLeg) -> DateTime<Utc> {
    match leg {
        PlanLeg::Train { departure, .. } => *departure,
        PlanLeg::Untracked {
            planned_departure, ..
        } => *planned_departure,
    }
}

fn leg_arrival(leg: &PlanLeg) -> DateTime<Utc> {
    match leg {
        PlanLeg::Train { arrival, .. } => *arrival,
        PlanLeg::Untracked {
            planned_arrival, ..
        } => *planned_arrival,
    }
}

/// Minutes from `date`'s midnight on the London clock (past 1440 for the
/// next day).
fn london_minutes(at: DateTime<Utc>, date: NaiveDate) -> i64 {
    let local = at.with_timezone(&chrono_tz::Europe::London);
    (local.date_naive() - date).num_days() * 1440
        + i64::from(local.hour()) * 60
        + i64::from(local.minute())
}

/// The order `kind` (`depart_after` or `arrive_by`) ranks options by,
/// best first: earliest arrival (then the later departure, then fewer
/// changes) for `depart_after`; latest departure (then the earlier
/// arrival, then fewer changes) for `arrive_by`.
fn target_order(kind: &str, options: &[PlanOption]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..options.len()).collect();
    if kind == "arrive_by" {
        order.sort_by(|&a, &b| {
            let (a, b) = (&options[a], &options[b]);
            b.departure
                .cmp(&a.departure)
                .then(a.arrival.cmp(&b.arrival))
                .then(a.change_count.cmp(&b.change_count))
        });
    } else {
        order.sort_by(|&a, &b| {
            let (a, b) = (&options[a], &options[b]);
            a.arrival
                .cmp(&b.arrival)
                .then(b.departure.cmp(&a.departure))
                .then(a.change_count.cmp(&b.change_count))
        });
    }
    order
}

/// Chooses among `options` (see the module doc) for a target of `kind` on
/// `date`: the choice's index and why, then up to [`MAX_ALTERNATIVES`]
/// alternatives, the closest to the target first taken, listed by
/// departure. `None` with no options.
pub fn choose(
    usual: Option<&UsualItinerary>,
    kind: &str,
    date: NaiveDate,
    options: &[PlanOption],
) -> Option<(usize, ChoiceReason, Vec<usize>)> {
    if options.is_empty() {
        return None;
    }
    let order = target_order(kind, options);
    let usual_trains: Vec<&crate::data::regular_journeys::UsualItineraryLeg> = usual
        .map(|usual| {
            usual
                .legs
                .iter()
                .filter(|leg| leg.kind == crate::data::journeys::LEG_KIND_TRAIN)
                .collect()
        })
        .unwrap_or_default();
    let usual_uids: Vec<&str> = usual_trains
        .iter()
        .filter_map(|leg| leg.train_uid.as_deref())
        .collect();
    let by_uids = (!usual_uids.is_empty() && usual_uids.len() == usual_trains.len())
        .then(|| {
            order.iter().copied().find(|&index| {
                let key = options[index].train_key();
                key.len() == usual_uids.len()
                    && key
                        .iter()
                        .zip(&usual_uids)
                        .all(|((uid, _), usual)| uid.eq_ignore_ascii_case(usual))
            })
        })
        .flatten();
    let (choice, reason) = if let Some(index) = by_uids {
        (index, ChoiceReason::Usual)
    } else if let Some(index) = usual_trains.first().and_then(|first| {
        let departs = first.departs?;
        let usual_min = i64::from(first.departs_day_offset) * 1440
            + i64::from(departs.hour()) * 60
            + i64::from(departs.minute());
        let operator = first.atoc_code.as_deref()?;
        order
            .iter()
            .copied()
            .filter_map(|index| {
                let (departure, train_operator) = options[index].first_train()?;
                let gap = (london_minutes(departure, date) - usual_min).abs();
                (gap <= SAME_OPERATOR_WINDOW_MINUTES
                    && train_operator.is_some_and(|op| op.eq_ignore_ascii_case(operator)))
                .then_some((gap, index))
            })
            .min_by_key(|(gap, _)| *gap)
            .map(|(_, index)| index)
    }) {
        (index, ChoiceReason::SameOperator)
    } else {
        (order[0], ChoiceReason::Best)
    };
    let mut alternatives: Vec<usize> = order
        .into_iter()
        .filter(|&index| index != choice)
        .take(MAX_ALTERNATIVES)
        .collect();
    // Shown in departure order (DS-398); an accept's `index` counts the
    // stored order, so it follows.
    alternatives.sort_by(|&a, &b| {
        (options[a].departure, options[a].arrival).cmp(&(options[b].departure, options[b].arrival))
    });
    Some((choice, reason, alternatives))
}

/// Writes an occurrence's plan, status, marker (`None` keeps it) and
/// `regular_planned_at = now`.
pub async fn write(
    conn: &mut sqlx::PgConnection,
    journey_id: i64,
    plan: &StoredPlan,
    status: &str,
    marker: Option<&str>,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    debug_assert!(STATUSES.contains(&status));
    sqlx::query(
        "UPDATE journeys SET regular_plan = $2, regular_plan_status = $3, \
                regular_plan_marker = COALESCE($4, regular_plan_marker), regular_planned_at = $5 \
         WHERE id = $1",
    )
    .bind(journey_id)
    .bind(Json(plan))
    .bind(status)
    .bind(marker)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// An occurrence's plan as stored: `(plan, status, marker, planned_at)`.
pub type StoredRow = (
    Option<StoredPlan>,
    Option<String>,
    Option<String>,
    Option<DateTime<Utc>>,
);

/// Reads an occurrence's stored plan.
pub async fn read_stored(pool: &PgPool, journey_id: i64) -> anyhow::Result<StoredRow> {
    type Row = (
        Option<Json<serde_json::Value>>,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
    );
    let row: Option<Row> = sqlx::query_as(
        "SELECT regular_plan, regular_plan_status, regular_plan_marker, regular_planned_at \
         FROM journeys WHERE id = $1",
    )
    .bind(journey_id)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        Some((plan, status, marker, planned_at)) => (
            crate::data::regular_journeys::decode_stored(plan),
            status,
            marker,
            planned_at,
        ),
        None => (None, None, None, None),
    })
}

/// `GET /Journeys/{id}`'s `regularPlan`: `None` when the sweep never
/// planned the journey.
pub async fn read_response(
    pool: &PgPool,
    journey_id: i64,
) -> anyhow::Result<Option<RegularPlanResponse>> {
    let (plan, status, _, planned_at) = read_stored(pool, journey_id).await?;
    let Some(status) = status else {
        return Ok(None);
    };
    let plan = plan.unwrap_or_default();
    Ok(Some(RegularPlanResponse {
        status,
        planned_at,
        reason: plan.reason,
        choice: plan.choice,
        alternatives: plan.alternatives,
        accepted_index: plan.accepted_index,
    }))
}

/// Whether every one of `trains` (UID and service date) still runs that
/// day in the published timetable: a cancelled (STP `C`) or withdrawn
/// schedule has no calling points.
pub async fn trains_run(pool: &PgPool, trains: &[(String, NaiveDate)]) -> anyhow::Result<bool> {
    let (uids, dates): (Vec<String>, Vec<NaiveDate>) = trains.iter().cloned().unzip();
    let (missing,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM unnest($1::text[], $2::date[]) AS t(uid, service_date) \
         WHERE NOT EXISTS (SELECT 1 FROM schedule_calling_points_full cp \
                           WHERE cp.service_date = t.service_date AND cp.uid = t.uid)",
    )
    .bind(&uids)
    .bind(&dates)
    .fetch_one(pool)
    .await?;
    Ok(missing == 0)
}

/// The trains of an occurrence's train legs, in order, `None` when one has
/// no train yet.
pub async fn committed_trains(
    pool: &PgPool,
    journey_id: i64,
) -> anyhow::Result<Option<Vec<(String, NaiveDate)>>> {
    let rows: Vec<(Option<String>, Option<NaiveDate>)> = sqlx::query_as(
        "SELECT tr.train_uid, tr.service_date FROM journey_legs jl \
         LEFT JOIN train_subscriptions ts ON ts.id = jl.train_subscription_id \
         LEFT JOIN trains tr ON tr.id = ts.trains_id \
         WHERE jl.journey_id = $1 AND jl.kind = 'train' ORDER BY jl.leg_order",
    )
    .bind(journey_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(uid, date)| Some((uid?, date?)))
        .collect())
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveTime, TimeZone};

    use super::*;
    use crate::data::regular_journeys::UsualItineraryLeg;

    const DATE: (i32, u32, u32) = (2099, 6, 17);

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(DATE.0, DATE.1, DATE.2).unwrap()
    }

    /// 2099-06-17 is in BST: London is UTC+1.
    fn at(h: u32, m: u32) -> DateTime<Utc> {
        chrono_tz::Europe::London
            .from_local_datetime(&date().and_hms_opt(h, m, 0).unwrap())
            .unwrap()
            .with_timezone(&Utc)
    }

    fn option(uid: &str, operator: &str, dep: (u32, u32), arr: (u32, u32)) -> PlanOption {
        PlanOption {
            departure: at(dep.0, dep.1),
            arrival: at(arr.0, arr.1),
            change_count: 0,
            legs: vec![PlanLeg::Train {
                train_uid: uid.to_string(),
                service_date: date(),
                origin_crs: Some("WOK".to_string()),
                origin_name: None,
                destination_crs: Some("WAT".to_string()),
                destination_name: None,
                departure: at(dep.0, dep.1),
                arrival: at(arr.0, arr.1),
                operator: Some(operator.to_string()),
                operator_name: None,
            }],
        }
    }

    fn usual(uid: &str, operator: &str, h: u32, m: u32) -> UsualItinerary {
        UsualItinerary {
            legs: vec![UsualItineraryLeg {
                kind: "train".to_string(),
                train_uid: Some(uid.to_string()),
                service_date: None,
                atoc_code: Some(operator.to_string()),
                origin_crs: Some("WOK".to_string()),
                destination_crs: Some("WAT".to_string()),
                departs: NaiveTime::from_hms_opt(h, m, 0),
                departs_day_offset: 0,
                arrives: None,
                arrives_day_offset: 0,
                transit_mode: None,
                origin_name: None,
                destination_name: None,
                line_name: None,
                minutes: None,
            }],
        }
    }

    fn options() -> Vec<PlanOption> {
        vec![
            option("W00001", "SW", (7, 50), (8, 30)),
            option("W00002", "SW", (8, 12), (8, 45)),
            option("W00003", "GW", (8, 10), (8, 40)),
            option("W00004", "SW", (8, 40), (9, 10)),
        ]
    }

    #[test]
    fn the_usual_train_is_chosen_when_it_runs() {
        let usual = usual("W00002", "SW", 8, 12);
        let (choice, reason, alternatives) =
            choose(Some(&usual), "depart_after", date(), &options()).unwrap();
        assert_eq!((choice, reason), (1, ChoiceReason::Usual));
        // The rest, by departure.
        assert_eq!(alternatives, [0, 2, 3]);
    }

    #[test]
    fn up_to_four_alternatives_are_listed_by_departure() {
        let mut many = options();
        many.push(option("W00005", "SW", (9, 0), (9, 30)));
        many.push(option("W00006", "SW", (6, 30), (7, 10)));
        let (choice, _, alternatives) = choose(None, "depart_after", date(), &many).unwrap();
        // The earliest arrival (W00006) is chosen; the next four by
        // arrival are W00001, W00003, W00002, W00004, listed by departure.
        assert_eq!(choice, 5);
        assert_eq!(alternatives, [0, 2, 1, 3]);
        assert_eq!(alternatives.len(), MAX_ALTERNATIVES);
    }

    #[test]
    fn a_new_uid_falls_back_to_the_same_operator_within_five_minutes() {
        // The timetable change renamed W00002; the usual 08:14 SW train is
        // now the 08:12 (2 minutes off). GW's 08:10 is nearer but not SW.
        let renamed = usual("OLD002", "sw", 8, 14);
        let (choice, reason, _) =
            choose(Some(&renamed), "depart_after", date(), &options()).unwrap();
        assert_eq!((choice, reason), (1, ChoiceReason::SameOperator));
        // Six minutes off is too far: the target's best instead.
        let late = usual("OLD002", "SW", 8, 18);
        let (choice, reason, _) = choose(Some(&late), "depart_after", date(), &options()).unwrap();
        assert_eq!((choice, reason), (0, ChoiceReason::Best));
    }

    #[test]
    fn without_the_usual_train_the_target_decides() {
        let (choice, reason, alternatives) =
            choose(None, "depart_after", date(), &options()).unwrap();
        assert_eq!((choice, reason), (0, ChoiceReason::Best));
        assert_eq!(alternatives, [2, 1, 3]);
        // Arrive by: the latest departure wins.
        let (choice, reason, alternatives) = choose(None, "arrive_by", date(), &options()).unwrap();
        assert_eq!((choice, reason), (3, ChoiceReason::Best));
        // Taken latest departure first, listed by departure.
        assert_eq!(alternatives, [0, 2, 1]);
        assert!(choose(None, "arrive_by", date(), &[]).is_none());
    }

    #[test]
    fn a_stored_plan_round_trips_and_counts_options_from_the_choice() {
        let plan = StoredPlan {
            reason: Some(ChoiceReason::SameOperator),
            choice: Some(options()[1].clone()),
            alternatives: vec![options()[0].clone()],
            sweep_leg_ids: vec![7],
            accepted_index: None,
        };
        let value = serde_json::to_value(&plan).unwrap();
        assert_eq!(value["reason"], "same_operator");
        assert_eq!(value["sweepLegIds"], serde_json::json!([7]));
        assert_eq!(value["choice"]["legs"][0]["kind"], "train");
        let back: StoredPlan = serde_json::from_value(value).unwrap();
        assert_eq!(back, plan);
        assert_eq!(back.option(1).unwrap().train_key()[0].0, "W00001");
        assert!(back.option(2).is_none());
        // An empty object (a later version's, or no_service) reads too.
        let empty: StoredPlan = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, StoredPlan::default());
    }

    #[test]
    fn london_minutes_count_past_midnight() {
        assert_eq!(london_minutes(at(8, 12), date()), 8 * 60 + 12);
        let next = chrono_tz::Europe::London
            .from_local_datetime(&date().succ_opt().unwrap().and_hms_opt(0, 30, 0).unwrap())
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(london_minutes(next, date()), 1440 + 30);
    }
}
