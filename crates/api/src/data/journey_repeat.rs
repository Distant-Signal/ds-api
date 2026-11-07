//! "Track this journey again" and "Move this journey": the same route on
//! another day or at another time, preferring the same trains
//! (`GET /Journeys/{id}/repeat`, `routes::journey_repeat`).
//!
//! The route is read from a revision of the journey: revision 1, the
//! booked plan, to track a journey again (what the traveller planned, not
//! what a break or a missed train made of it), or the latest revision, the
//! plan as it stands, to move a journey that hasn't started. From its legs
//! ([`shape`]):
//!
//! - the trains run from the first train leg's boarding station to the last
//!   train leg's alighting station, through the stations they change at
//!   (the search is planned `via` them first, then without them when that
//!   finds nothing);
//! - the train legs become a [`UsualItinerary`], so the choice is the plan
//!   sweep's (`regular_plan::choose`): the same trains, else the same
//!   operator within 5 minutes of the first train's time, else the
//!   earliest arrival ([`pick`]);
//! - the untracked legs before the first train and after the last (a walk
//!   or a bus to the station) are kept, moved by as much as the train next
//!   to them moved ([`with_untracked`]).
//!
//! A journey with an Irish train (there is no Irish timetable to plan
//! from), a network ride (`transitNetwork`: the planner's transit legs are
//! not repeated), a stop of over [`WAYPOINT_DWELL_MINUTES`] between two
//! trains (a waypoint visit, which a plan from A to B would leave out), or
//! a train leg with no train or no stations is [`Unsupported`].

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Timelike, Utc};
use serde::Serialize;
use sqlx::PgPool;

use crate::data::journeys::{LEG_KIND_TRAIN, LEG_KIND_UNTRACKED};
use crate::data::regular_journeys::{UsualItinerary, UsualItineraryLeg};
use crate::data::regular_plan::{self, ChoiceReason, PlanLeg, PlanOption};

/// A stop between two trains longer than this is a waypoint visit: the
/// journey is [`Unsupported::Waypoints`].
pub const WAYPOINT_DWELL_MINUTES: i64 = 120;

/// Which plan of the journey to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Revision 1, the booked plan: tracking the journey again.
    Booked,
    /// The latest revision, the plan as it stands: moving the journey.
    Current,
}

/// One leg of the revision read.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SourceLeg {
    pub kind: String,
    pub train_uid: Option<String>,
    pub service_date: NaiveDate,
    pub atoc_code: Option<String>,
    pub origin_crs: Option<String>,
    pub destination_crs: Option<String>,
    pub booked_departure: Option<DateTime<Utc>>,
    pub booked_arrival: Option<DateTime<Utc>>,
    pub transit_mode: Option<String>,
    pub origin_place: Option<String>,
    pub destination_place: Option<String>,
    pub line_name: Option<String>,
    pub planned_departure: Option<DateTime<Utc>>,
    pub planned_arrival: Option<DateTime<Utc>>,
    pub origin_stop_id: Option<String>,
    pub destination_stop_id: Option<String>,
    pub transit_network: Option<String>,
}

/// The legs of the journey's revision 1 ([`Basis::Booked`]) or latest
/// revision ([`Basis::Current`]), in order. Empty when it has none.
pub async fn source_legs(
    pool: &PgPool,
    journey_id: i64,
    basis: Basis,
) -> anyhow::Result<Vec<SourceLeg>> {
    Ok(sqlx::query_as(
        "SELECT l.kind, l.train_uid, l.service_date, l.atoc_code, l.origin_crs, \
                l.destination_crs, l.booked_departure, l.booked_arrival, l.transit_mode, \
                l.origin_place, l.destination_place, l.line_name, l.planned_departure, \
                l.planned_arrival, l.origin_stop_id, l.destination_stop_id, l.transit_network \
         FROM journey_revision_legs l JOIN journey_revisions r ON r.id = l.revision_id \
         WHERE r.journey_id = $1 \
           AND r.revision_no = CASE WHEN $2 THEN 1 \
                                    ELSE (SELECT MAX(revision_no) FROM journey_revisions \
                                          WHERE journey_id = $1) END \
         ORDER BY l.position",
    )
    .bind(journey_id)
    .bind(basis == Basis::Booked)
    .fetch_all(pool)
    .await?)
}

/// Why a journey can't be planned again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename = "JourneyRepeatUnsupported")]
pub enum Unsupported {
    /// An Irish train: there is no Irish timetable to plan from.
    Irish,
    /// A ride on a transit network.
    Transit,
    /// A waypoint visit between two trains, or a regular journey planned
    /// with waypoints.
    Waypoints,
    /// No train leg, or one with no train or no stations.
    Incomplete,
}

/// The route [`shape`] reads from a journey's legs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub origin: String,
    pub destination: String,
    /// The stations the trains change at, in order.
    pub vias: Vec<String>,
    /// The untracked legs before the first train, and after the last.
    pub leading: Vec<SourceLeg>,
    pub trailing: Vec<SourceLeg>,
    /// The first train's booked departure, and the last train's booked
    /// arrival.
    pub first_departure: DateTime<Utc>,
    pub last_arrival: Option<DateTime<Utc>>,
    /// The train legs as the plan sweep's usual itinerary.
    pub usual: UsualItinerary,
}

fn crs(code: Option<&String>) -> Option<String> {
    code.map(|c| c.trim().to_ascii_uppercase())
        .filter(|c| !c.is_empty())
}

/// The London date and clock time of `at`.
pub fn london_clock(at: DateTime<Utc>) -> (NaiveDate, NaiveTime) {
    let local = at.with_timezone(&chrono_tz::Europe::London);
    (
        local.date_naive(),
        NaiveTime::from_hms_opt(local.hour(), local.minute(), 0).unwrap_or_default(),
    )
}

/// `time` on `date` in London as an instant (the earlier one in the
/// repeated autumn hour, the hour after in the missing spring one).
pub fn london_instant(date: NaiveDate, time: NaiveTime) -> DateTime<Utc> {
    let local = date.and_time(time);
    chrono_tz::Europe::London
        .from_local_datetime(&local)
        .earliest()
        .or_else(|| {
            chrono_tz::Europe::London
                .from_local_datetime(&(local + Duration::hours(1)))
                .earliest()
        })
        .map_or_else(|| local.and_utc(), |at| at.with_timezone(&Utc))
}

/// The train legs (with their stations, `ends`) as the plan sweep's usual
/// itinerary: London clock times, days counted from the first train's
/// date.
fn usual_itinerary(trains: &[&SourceLeg], ends: &[(String, String)]) -> UsualItinerary {
    let base_date = trains[0].service_date;
    UsualItinerary {
        legs: trains
            .iter()
            .zip(ends)
            .map(|(train, (origin, destination))| {
                let clock = |at: Option<DateTime<Utc>>| {
                    at.map(|at| {
                        let (date, time) = london_clock(at);
                        (
                            time,
                            i16::try_from((date - base_date).num_days()).unwrap_or(0),
                        )
                    })
                };
                let departs = clock(train.booked_departure);
                let arrives = clock(train.booked_arrival);
                UsualItineraryLeg {
                    kind: LEG_KIND_TRAIN.to_string(),
                    train_uid: train.train_uid.clone(),
                    service_date: None,
                    atoc_code: train.atoc_code.clone(),
                    origin_crs: Some(origin.clone()),
                    destination_crs: Some(destination.clone()),
                    departs: departs.map(|(time, _)| time),
                    departs_day_offset: departs.map_or(0, |(_, offset)| offset),
                    arrives: arrives.map(|(time, _)| time),
                    arrives_day_offset: arrives.map_or(0, |(_, offset)| offset),
                    transit_mode: None,
                    origin_name: None,
                    destination_name: None,
                    line_name: None,
                    minutes: None,
                }
            })
            .collect(),
    }
}

/// The route of `legs` (see the module doc).
pub fn shape(legs: &[SourceLeg]) -> Result<Shape, Unsupported> {
    if legs.iter().any(|leg| {
        leg.train_uid
            .as_deref()
            .is_some_and(|uid| uid.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("ie:")))
    }) {
        return Err(Unsupported::Irish);
    }
    if legs.iter().any(|leg| leg.transit_network.is_some()) {
        return Err(Unsupported::Transit);
    }
    let trains: Vec<&SourceLeg> = legs
        .iter()
        .filter(|leg| leg.kind == LEG_KIND_TRAIN)
        .collect();
    let (Some(first_index), Some(last_index)) = (
        legs.iter().position(|leg| leg.kind == LEG_KIND_TRAIN),
        legs.iter().rposition(|leg| leg.kind == LEG_KIND_TRAIN),
    ) else {
        return Err(Unsupported::Incomplete);
    };
    let mut ends = Vec::with_capacity(trains.len());
    for train in &trains {
        let (Some(origin), Some(destination)) = (
            crs(train.origin_crs.as_ref()),
            crs(train.destination_crs.as_ref()),
        ) else {
            return Err(Unsupported::Incomplete);
        };
        if train.train_uid.is_none() || train.booked_departure.is_none() {
            return Err(Unsupported::Incomplete);
        }
        ends.push((origin, destination));
    }
    for pair in trains.windows(2) {
        if let (Some(arrival), Some(departure)) = (pair[0].booked_arrival, pair[1].booked_departure)
            && departure - arrival > Duration::minutes(WAYPOINT_DWELL_MINUTES)
        {
            return Err(Unsupported::Waypoints);
        }
    }
    let untracked = |range: &[SourceLeg]| -> Result<Vec<SourceLeg>, Unsupported> {
        range
            .iter()
            .filter(|leg| leg.kind == LEG_KIND_UNTRACKED)
            .map(|leg| {
                if leg.planned_departure.is_some() && leg.planned_arrival.is_some() {
                    Ok(leg.clone())
                } else {
                    Err(Unsupported::Incomplete)
                }
            })
            .collect()
    };
    let leading = untracked(&legs[..first_index])?;
    let trailing = untracked(&legs[last_index + 1..])?;
    let origin = ends[0].0.clone();
    let destination = ends[ends.len() - 1].1.clone();
    let mut vias: Vec<String> = Vec::new();
    for pair in ends.windows(2) {
        for code in [&pair[0].1, &pair[1].0] {
            if *code != origin && *code != destination && vias.last() != Some(code) {
                vias.push(code.clone());
            }
        }
    }
    let first_departure = trains[0].booked_departure.unwrap_or_default();
    let usual = usual_itinerary(&trains, &ends);
    Ok(Shape {
        origin,
        destination,
        vias,
        leading,
        trailing,
        first_departure,
        last_arrival: trains[trains.len() - 1].booked_arrival,
        usual,
    })
}

/// The day to plan when none is asked for: today if the first train's
/// clock time is still ahead today, else tomorrow.
pub fn default_date(time: NaiveTime, now: DateTime<Utc>) -> NaiveDate {
    let (today, _) = london_clock(now);
    if london_instant(today, time) > now {
        today
    } else {
        today + Duration::days(1)
    }
}

/// The usual itinerary for a search at `time`, the first train's time
/// having been `original`. At another time the same trains are no longer
/// what was asked for: their UIDs are dropped, and "the same operator
/// within 5 minutes" means within 5 minutes of `time`.
pub fn usual_at(usual: &UsualItinerary, time: NaiveTime, original: NaiveTime) -> UsualItinerary {
    let mut usual = usual.clone();
    if time == original {
        return usual;
    }
    for leg in &mut usual.legs {
        leg.train_uid = None;
    }
    if let Some(first) = usual.legs.first_mut() {
        first.departs = Some(time);
        first.departs_day_offset = 0;
    }
    usual
}

/// The choice among `options` for a search from `target` on `date`: the
/// plan sweep's ([`regular_plan::choose`]), except that the target's best
/// and the alternatives never leave before `target` (the search starts
/// [`regular_plan::SAME_OPERATOR_WINDOW_MINUTES`] earlier, so that the
/// same operator's train a few minutes early is found).
pub fn pick(
    usual: &UsualItinerary,
    date: NaiveDate,
    target: DateTime<Utc>,
    options: &[PlanOption],
) -> Option<(usize, ChoiceReason, Vec<usize>)> {
    let (choice, reason, _) = regular_plan::choose(Some(usual), "depart_after", date, options)?;
    let on_time: Vec<usize> = (0..options.len())
        .filter(|&index| options[index].departure >= target)
        .collect();
    let subset = |indexes: &[usize]| -> Vec<PlanOption> {
        indexes
            .iter()
            .map(|&index| options[index].clone())
            .collect()
    };
    let choice = if reason == ChoiceReason::Best {
        regular_plan::choose(None, "depart_after", date, &subset(&on_time))
            .map_or(choice, |(best, _, _)| on_time[best])
    } else {
        choice
    };
    let rest: Vec<usize> = on_time
        .into_iter()
        .filter(|&index| index != choice)
        .collect();
    let alternatives = regular_plan::choose(None, "depart_after", date, &subset(&rest))
        .map(|(first, _, others)| {
            std::iter::once(first)
                .chain(others)
                .take(regular_plan::MAX_ALTERNATIVES)
                .map(|index| rest[index])
                .collect()
        })
        .unwrap_or_default();
    Some((choice, reason, alternatives))
}

fn shifted(leg: &SourceLeg, by: Duration) -> PlanLeg {
    PlanLeg::Untracked {
        transit_mode: leg
            .transit_mode
            .clone()
            .unwrap_or_else(|| "walk".to_string()),
        origin_name: leg.origin_place.clone(),
        destination_name: leg.destination_place.clone(),
        line_name: leg.line_name.clone(),
        origin_stop_id: leg.origin_stop_id.clone(),
        destination_stop_id: leg.destination_stop_id.clone(),
        planned_departure: leg.planned_departure.unwrap_or_default() + by,
        planned_arrival: leg.planned_arrival.unwrap_or_default() + by,
    }
}

/// `option` with the shape's untracked legs around it: the leading ones
/// moved by as much as the first train's departure moved, the trailing
/// ones by as much as the last train's arrival moved (else the first
/// train's move).
pub fn with_untracked(shape: &Shape, option: PlanOption) -> PlanOption {
    let trains = || {
        option.legs.iter().filter_map(|leg| match leg {
            PlanLeg::Train {
                departure, arrival, ..
            } => Some((*departure, *arrival)),
            PlanLeg::Untracked { .. } => None,
        })
    };
    let Some((first_departure, _)) = trains().next() else {
        return option;
    };
    let lead_by = first_departure - shape.first_departure;
    let trail_by = match (trains().next_back(), shape.last_arrival) {
        (Some((_, arrival)), Some(original)) => arrival - original,
        _ => lead_by,
    };
    let legs: Vec<PlanLeg> = shape
        .leading
        .iter()
        .map(|leg| shifted(leg, lead_by))
        .chain(option.legs)
        .chain(shape.trailing.iter().map(|leg| shifted(leg, trail_by)))
        .collect();
    let leg_times = |leg: &PlanLeg| match leg {
        PlanLeg::Train {
            departure, arrival, ..
        } => (*departure, *arrival),
        PlanLeg::Untracked {
            planned_departure,
            planned_arrival,
            ..
        } => (*planned_departure, *planned_arrival),
    };
    PlanOption {
        departure: legs
            .first()
            .map_or(option.departure, |leg| leg_times(leg).0),
        arrival: legs.last().map_or(option.arrival, |leg| leg_times(leg).1),
        change_count: option.change_count,
        legs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DATE: (i32, u32, u32) = (2099, 6, 17);

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(DATE.0, DATE.1, DATE.2).unwrap()
    }

    /// `h:m` London (BST) on `date`.
    fn at_on(date: NaiveDate, h: u32, m: u32) -> DateTime<Utc> {
        london_instant(date, NaiveTime::from_hms_opt(h, m, 0).unwrap())
    }

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        at_on(date(), h, m)
    }

    fn train(
        uid: &str,
        op: &str,
        from: &str,
        to: &str,
        dep: (u32, u32),
        arr: (u32, u32),
    ) -> SourceLeg {
        SourceLeg {
            kind: "train".to_string(),
            train_uid: Some(uid.to_string()),
            service_date: date(),
            atoc_code: Some(op.to_string()),
            origin_crs: Some(from.to_string()),
            destination_crs: Some(to.to_string()),
            booked_departure: Some(at(dep.0, dep.1)),
            booked_arrival: Some(at(arr.0, arr.1)),
            transit_mode: None,
            origin_place: None,
            destination_place: None,
            line_name: None,
            planned_departure: None,
            planned_arrival: None,
            origin_stop_id: None,
            destination_stop_id: None,
            transit_network: None,
        }
    }

    fn walk(from: &str, to: &str, dep: (u32, u32), arr: (u32, u32)) -> SourceLeg {
        SourceLeg {
            kind: "untracked".to_string(),
            train_uid: None,
            atoc_code: None,
            origin_crs: None,
            destination_crs: None,
            booked_departure: None,
            booked_arrival: None,
            transit_mode: Some("bus".to_string()),
            origin_place: Some(from.to_string()),
            destination_place: Some(to.to_string()),
            planned_departure: Some(at(dep.0, dep.1)),
            planned_arrival: Some(at(arr.0, arr.1)),
            ..train("", "", "", "", (0, 0), (0, 0))
        }
    }

    fn two_legs() -> Vec<SourceLeg> {
        vec![
            walk("Home", "Woking", (7, 30), (7, 45)),
            train("W00001", "SW", "wok", "CLJ", (7, 50), (8, 15)),
            train("S00001", "SN", "CLJ", "VIC", (8, 22), (8, 30)),
            walk("Victoria", "Office", (8, 35), (8, 50)),
        ]
    }

    fn plan_train(
        uid: &str,
        op: &str,
        from: &str,
        to: &str,
        dep: DateTime<Utc>,
        arr: DateTime<Utc>,
    ) -> PlanLeg {
        PlanLeg::Train {
            train_uid: uid.to_string(),
            service_date: dep.with_timezone(&chrono_tz::Europe::London).date_naive(),
            origin_crs: Some(from.to_string()),
            origin_name: None,
            destination_crs: Some(to.to_string()),
            destination_name: None,
            departure: dep,
            arrival: arr,
            operator: Some(op.to_string()),
            operator_name: None,
        }
    }

    fn option(legs: Vec<PlanLeg>) -> PlanOption {
        let times = |leg: &PlanLeg| match leg {
            PlanLeg::Train {
                departure, arrival, ..
            } => (*departure, *arrival),
            PlanLeg::Untracked { .. } => unreachable!(),
        };
        PlanOption {
            departure: times(&legs[0]).0,
            arrival: times(legs.last().unwrap()).1,
            change_count: u32::try_from(legs.len() - 1).unwrap(),
            legs,
        }
    }

    #[test]
    fn the_shape_runs_from_the_first_train_to_the_last_via_the_change() {
        let shape = shape(&two_legs()).unwrap();
        assert_eq!(
            (shape.origin.as_str(), shape.destination.as_str()),
            ("WOK", "VIC")
        );
        assert_eq!(shape.vias, ["CLJ"]);
        assert_eq!(shape.leading.len(), 1);
        assert_eq!(shape.trailing.len(), 1);
        assert_eq!(shape.first_departure, at(7, 50));
        assert_eq!(shape.last_arrival, Some(at(8, 30)));
        let uids: Vec<_> = shape
            .usual
            .legs
            .iter()
            .map(|leg| (leg.train_uid.clone().unwrap(), leg.departs.unwrap()))
            .collect();
        assert_eq!(
            uids,
            [
                (
                    "W00001".to_string(),
                    NaiveTime::from_hms_opt(7, 50, 0).unwrap()
                ),
                (
                    "S00001".to_string(),
                    NaiveTime::from_hms_opt(8, 22, 0).unwrap()
                ),
            ]
        );
        // A walk between stations is two vias, in order.
        let mut legs = two_legs();
        legs[2].origin_crs = Some("WIM".to_string());
        assert_eq!(super::shape(&legs).unwrap().vias, ["CLJ", "WIM"]);
    }

    #[test]
    fn irish_transit_waypoint_and_incomplete_journeys_are_unsupported() {
        let mut irish = two_legs();
        irish[1].train_uid = Some("ie:A120".to_string());
        assert_eq!(shape(&irish), Err(Unsupported::Irish));
        let mut transit = two_legs();
        transit[3].transit_network = Some("metrolink".to_string());
        assert_eq!(shape(&transit), Err(Unsupported::Transit));
        // Three hours at Clapham Junction is a visit, not a change.
        let mut waypoint = two_legs();
        waypoint[2].booked_departure = Some(at(11, 16));
        assert_eq!(shape(&waypoint), Err(Unsupported::Waypoints));
        let mut no_train = two_legs();
        no_train[1].train_uid = None;
        no_train[1].booked_departure = None;
        assert_eq!(shape(&no_train), Err(Unsupported::Incomplete));
        assert_eq!(
            shape(&[walk("A", "B", (7, 0), (7, 10))]),
            Err(Unsupported::Incomplete)
        );
    }

    #[test]
    fn the_default_date_is_today_until_the_train_has_left() {
        let time = NaiveTime::from_hms_opt(7, 50, 0).unwrap();
        assert_eq!(default_date(time, at(7, 0)), date());
        assert_eq!(default_date(time, at(7, 50)), date().succ_opt().unwrap());
        assert_eq!(default_date(time, at(21, 0)), date().succ_opt().unwrap());
    }

    #[test]
    fn the_untracked_legs_move_with_the_trains_next_to_them() {
        let shape = shape(&two_legs()).unwrap();
        let next = date().succ_opt().unwrap();
        // The next day, the first train 4 minutes later, the last arriving
        // 10 minutes later.
        let planned = option(vec![
            plan_train(
                "W00002",
                "SW",
                "WOK",
                "CLJ",
                at_on(next, 7, 54),
                at_on(next, 8, 19),
            ),
            plan_train(
                "S00002",
                "SN",
                "CLJ",
                "VIC",
                at_on(next, 8, 30),
                at_on(next, 8, 40),
            ),
        ]);
        let whole = with_untracked(&shape, planned);
        assert_eq!(whole.legs.len(), 4);
        let PlanLeg::Untracked {
            planned_departure,
            planned_arrival,
            transit_mode,
            ..
        } = &whole.legs[0]
        else {
            panic!("a leading leg");
        };
        assert_eq!(
            (*planned_departure, *planned_arrival),
            (at_on(next, 7, 34), at_on(next, 7, 49))
        );
        assert_eq!(transit_mode, "bus");
        let PlanLeg::Untracked {
            planned_departure,
            planned_arrival,
            ..
        } = &whole.legs[3]
        else {
            panic!("a trailing leg");
        };
        assert_eq!(
            (*planned_departure, *planned_arrival),
            (at_on(next, 8, 45), at_on(next, 9, 0))
        );
        assert_eq!(
            (whole.departure, whole.arrival),
            (at_on(next, 7, 34), at_on(next, 9, 0))
        );
    }

    fn one(uid: &str, op: &str, dep: (u32, u32), arr: (u32, u32)) -> PlanOption {
        option(vec![plan_train(
            uid,
            op,
            "WOK",
            "VIC",
            at(dep.0, dep.1),
            at(arr.0, arr.1),
        )])
    }

    fn usual() -> UsualItinerary {
        shape(&[train("W00002", "SW", "WOK", "VIC", (8, 12), (8, 45))])
            .unwrap()
            .usual
    }

    #[test]
    fn the_same_train_then_the_same_operator_then_the_best() {
        let target = at(8, 12);
        let options = vec![
            one("W00001", "SW", (8, 9), (8, 40)),
            one("W00002", "SW", (8, 12), (8, 45)),
            one("G00003", "GW", (8, 14), (8, 41)),
            one("W00004", "SW", (8, 40), (9, 10)),
        ];
        let (choice, reason, alternatives) = pick(&usual(), date(), target, &options).unwrap();
        assert_eq!((choice, reason), (1, ChoiceReason::Usual));
        // Never one leaving before the time asked for.
        assert_eq!(alternatives, [2, 3]);

        // The usual train doesn't run: SW's 08:09 is 3 minutes off.
        let without: Vec<_> = [0, 2, 3].map(|i| options[i].clone()).to_vec();
        let (choice, reason, alternatives) = pick(&usual(), date(), target, &without).unwrap();
        assert_eq!((choice, reason), (0, ChoiceReason::SameOperator));
        assert_eq!(alternatives, [1, 2]);

        // No SW train near: the earliest arrival from the time asked.
        let others: Vec<_> = [2, 3].map(|i| options[i].clone()).to_vec();
        let (choice, reason, alternatives) = pick(&usual(), date(), target, &others).unwrap();
        assert_eq!((choice, reason), (0, ChoiceReason::Best));
        assert_eq!(alternatives, [1]);
        assert!(pick(&usual(), date(), target, &[]).is_none());
    }

    #[test]
    fn a_later_time_looks_for_the_operator_near_it() {
        let original = NaiveTime::from_hms_opt(8, 12, 0).unwrap();
        assert_eq!(usual_at(&usual(), original, original), usual());
        let later = usual_at(
            &usual(),
            NaiveTime::from_hms_opt(8, 40, 0).unwrap(),
            original,
        );
        // The usual train itself, found from 08:35, is not what was asked.
        let options = vec![
            one("W00002", "SW", (8, 12), (8, 45)),
            one("G00003", "GW", (8, 38), (8, 59)),
            one("W00004", "SW", (8, 42), (9, 10)),
        ];
        let (choice, reason, _) = pick(&later, date(), at(8, 40), &options).unwrap();
        assert_eq!((choice, reason), (2, ChoiceReason::SameOperator));
    }
}
