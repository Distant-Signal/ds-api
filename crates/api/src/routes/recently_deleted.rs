//! Recently deleted journeys (`data::recently_deleted`), cookie-only, each
//! `404` while `RECENTLY_DELETED_ENABLED` is off:
//!
//! | Route | What |
//! |---|---|
//! | `GET /Journeys/recently-deleted` | The caller's deleted journeys still kept, newest first |
//! | `POST /Journeys/recently-deleted/{id}/restore` | "Save again": a new journey with the deleted one's legs; the row goes |
//! | `DELETE /Journeys/recently-deleted/{id}` | "Delete permanently": the row goes now |
//! | `DELETE /Journeys/recently-deleted` | "Empty now": every row of the caller's goes now |
//!
//! Not delegated routes and not personal-token routes: only the signed-in
//! user, through the site, sees or acts on the list. Another user's row is
//! `404`, as a missing one is. The router-wide Origin check covers the
//! three writes.
//!
//! "Save again" brings back the journey's shape, not its history: the
//! name and each leg, with its date, stations or places and planned
//! times, saved as `POST /Journeys/itinerary` saves them (the journey and
//! train caps, `journeys::create_journey_from_itinerary`). Each leg's
//! outcome is in the response, by its position in the deleted journey:
//!
//! - a train leg whose train can still be tracked is tracked again
//!   (`tracked`);
//! - a train leg with no train, a train past the tracking horizon, a past
//!   train whose record has gone (more than
//!   `TRACK_BY_UID_MAX_DAYS_BEHIND` days ago, no `trains` row left), or one
//!   that no longer calls at its stations comes back with its stations and
//!   date but no train (`without_train`, with `no_train`, `too_far_ahead`,
//!   `train_gone` or `train_changed`), for the traveller to pick one;
//! - an untracked leg comes back as it was (`untracked`), its stops if they
//!   still exist, its network ride (DS-237) not: unless it is incomplete
//!   (a row from before planned arrivals were kept) or more than
//!   `TRACK_BY_UID_MAX_DAYS_BEHIND` days old or past the horizon, when it is
//!   left out (`left_out`, `incomplete`, `too_old` or `too_far_ahead`).
//!
//! When no leg can come back, `422 nothing_to_restore` and the row stays.
//! Nothing else returns: revisions, breaks, tickets, share links, follows,
//! group shares, the regular journey it came from, its operator filter and
//! its leave-by settings are gone with the old journey.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::app::{App, Router};
use crate::auth::AuthenticatedUser;
use crate::data::{delegated, journeys, leg_validation, recently_deleted, train_tracking};
use crate::routes::journey_itinerary::RouteError;

pub fn router() -> Router {
    Router::new()
        .route(
            "/Journeys/recently-deleted",
            axum::routing::get(get_list).delete(delete_all),
        )
        .route(
            "/Journeys/recently-deleted/{id}",
            axum::routing::delete(delete_one),
        )
        .route(
            "/Journeys/recently-deleted/{id}/restore",
            axum::routing::post(post_restore),
        )
}

fn not_found() -> Response {
    RouteError::coded(
        StatusCode::NOT_FOUND,
        "not_found",
        "no recently deleted journey with that id",
    )
    .render(false, "bad_request")
}

fn internal(operation: &'static str) -> impl Fn(anyhow::Error) -> Response {
    move |err| {
        if let Some(unavailable) = crate::unavailable::response_for(&err) {
            return unavailable.into_response();
        }
        tracing::error!(error = ?err, operation, "recently deleted request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to {operation}"),
        )
            .into_response()
    }
}

/// `GET /Journeys/recently-deleted`'s body.
#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecentlyDeletedResponse {
    /// Days the caller's own deletes are kept
    /// (`RECENTLY_DELETED_RETENTION_DAYS`).
    retention_days: i64,
    /// Days a connected app's deletes are kept (30).
    connected_app_retention_days: i64,
    items: Vec<recently_deleted::RecentlyDeletedJourney>,
}

async fn get_list(State(app): State<App>, user: AuthenticatedUser) -> Response {
    if !recently_deleted::enabled() {
        return not_found();
    }
    let retention_days = recently_deleted::retention_days();
    match recently_deleted::list(&app.database, &user.id, retention_days).await {
        Ok(items) => Json(RecentlyDeletedResponse {
            retention_days,
            connected_app_retention_days: delegated::DELETION_RETENTION_DAYS,
            items,
        })
        .into_response(),
        Err(err) => internal("list recently deleted journeys")(err),
    }
}

async fn delete_one(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(id): Path<i64>,
) -> Response {
    if !recently_deleted::enabled() {
        return not_found();
    }
    match recently_deleted::delete_one(&app.database, &user.id, id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(err) => internal("delete a recently deleted journey")(err),
    }
}

async fn delete_all(State(app): State<App>, user: AuthenticatedUser) -> Response {
    if !recently_deleted::enabled() {
        return not_found();
    }
    match recently_deleted::delete_all(&app.database, &user.id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => internal("empty recently deleted journeys")(err),
    }
}

/// One leg of a stored summary (`crate::ts_values::DeletedJourneyLeg`).
/// Every field optional: older rows lack some.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SummaryLeg {
    service_date: Option<NaiveDate>,
    origin_crs: Option<String>,
    destination_crs: Option<String>,
    train_uid: Option<String>,
    kind: Option<String>,
    transit_mode: Option<String>,
    origin_name: Option<String>,
    destination_name: Option<String>,
    line_name: Option<String>,
    planned_departure: Option<DateTime<Utc>>,
    planned_arrival: Option<DateTime<Utc>>,
    origin_stop_id: Option<String>,
    destination_stop_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    custom_name: Option<String>,
    #[serde(default)]
    legs: Vec<SummaryLeg>,
}

/// What happened to one leg on "Save again".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename = "RestoredLegOutcome")]
pub(crate) enum Outcome {
    /// Its train is tracked again.
    Tracked,
    /// An untracked leg (a Tube ride, bus or walk), back as it was.
    Untracked,
    /// Its stations and date are back, with no train.
    WithoutTrain,
    /// It could not come back.
    LeftOut,
}

/// Why a leg came back without its train, or not at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename = "RestoredLegReason")]
pub(crate) enum Reason {
    /// It had no train when it was deleted.
    NoTrain,
    /// A past train whose record has gone.
    TrainGone,
    /// The train no longer calls at the leg's stations.
    TrainChanged,
    /// Its date is past the tracking horizon.
    TooFarAhead,
    /// An untracked leg more than a week old.
    TooOld,
    /// Too little was kept to save it.
    Incomplete,
}

/// One leg in `POST .../restore`'s body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "RestoredLeg", optional_fields = nullable)]
pub(crate) struct RestoredLeg {
    /// Its position in the deleted journey, from 1.
    leg: u32,
    outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<Reason>,
}

/// `POST /Journeys/recently-deleted/{id}/restore`'s body.
#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "RestoreDeletedJourneyResponse")]
pub(crate) struct RestoreResponse {
    /// The new journey.
    journey_id: i64,
    legs: Vec<RestoredLeg>,
}

/// A summary leg, judged without the database.
#[derive(Debug, Clone, PartialEq)]
enum Planned {
    Train {
        train_uid: String,
        service_date: NaiveDate,
        origin_crs: Option<String>,
        destination_crs: Option<String>,
    },
    Untracked {
        leg: journeys::UntrackedLeg,
        origin_stop_id: Option<String>,
        destination_stop_id: Option<String>,
    },
    Unmatched {
        service_date: NaiveDate,
        origin_crs: Option<String>,
        destination_crs: Option<String>,
        reason: Reason,
    },
    LeftOut(Reason),
}

fn crs(code: Option<&str>) -> Option<String> {
    code.map(str::trim)
        .filter(|code| !code.is_empty())
        .map(str::to_ascii_uppercase)
}

/// Whether `date` is more than `TRACK_BY_UID_MAX_DAYS_BEHIND` days before
/// `london_today`.
fn too_old(date: NaiveDate, london_today: NaiveDate) -> bool {
    date < london_today - chrono::Duration::days(train_tracking::TRACK_BY_UID_MAX_DAYS_BEHIND)
}

/// The first, database-free judgement of a leg (see the module doc).
fn classify(leg: &SummaryLeg, london_today: NaiveDate) -> Planned {
    if leg.kind.as_deref() == Some(journeys::LEG_KIND_UNTRACKED) {
        let (Some(transit_mode), Some(departure), Some(_)) = (
            leg.transit_mode.as_deref(),
            leg.planned_departure,
            leg.planned_arrival,
        ) else {
            return Planned::LeftOut(Reason::Incomplete);
        };
        let input = journeys::UntrackedLegInput {
            transit_mode,
            origin_name: leg.origin_name.as_deref(),
            destination_name: leg.destination_name.as_deref(),
            line_name: leg.line_name.as_deref(),
            planned_departure: leg.planned_departure,
            planned_arrival: leg.planned_arrival,
        };
        if let Ok(untracked) = journeys::validate_untracked_leg(input, london_today) {
            return Planned::Untracked {
                leg: untracked,
                origin_stop_id: leg.origin_stop_id.clone(),
                destination_stop_id: leg.destination_stop_id.clone(),
            };
        }
        let day = departure
            .with_timezone(&chrono_tz::Europe::London)
            .date_naive();
        return Planned::LeftOut(if too_old(day, london_today) {
            Reason::TooOld
        } else if train_tracking::beyond_tracking_horizon(day, london_today) {
            Reason::TooFarAhead
        } else {
            Reason::Incomplete
        });
    }
    let Some(service_date) = leg.service_date else {
        return Planned::LeftOut(Reason::Incomplete);
    };
    let origin_crs = crs(leg.origin_crs.as_deref());
    let destination_crs = crs(leg.destination_crs.as_deref());
    let unmatched = |reason| Planned::Unmatched {
        service_date,
        origin_crs: origin_crs.clone(),
        destination_crs: destination_crs.clone(),
        reason,
    };
    let Some(train_uid) = leg
        .train_uid
        .as_deref()
        .filter(|uid| journeys::validate_train_uid(uid).is_ok())
    else {
        return unmatched(Reason::NoTrain);
    };
    if train_tracking::beyond_tracking_horizon(service_date, london_today) {
        return unmatched(Reason::TooFarAhead);
    }
    Planned::Train {
        train_uid: train_uid.trim().to_ascii_uppercase(),
        service_date,
        origin_crs,
        destination_crs,
    }
}

/// [`classify`]'s train legs checked against the database: a past train
/// whose record has gone, or one that no longer calls at the leg's
/// stations, comes back without its train. An untracked leg keeps its stops
/// if they still exist.
async fn check(app: &App, planned: Planned) -> anyhow::Result<Planned> {
    match planned {
        Planned::Train {
            train_uid,
            service_date,
            origin_crs,
            destination_crs,
        } => {
            let unmatched = |reason| Planned::Unmatched {
                service_date,
                origin_crs: origin_crs.clone(),
                destination_crs: destination_crs.clone(),
                reason,
            };
            if too_old(service_date, crate::routes::london_today()) {
                let kept: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM trains WHERE train_uid = $1 AND service_date = $2)",
                )
                .bind(&train_uid)
                .bind(service_date)
                .fetch_one(&app.database)
                .await?;
                if !kept {
                    return Ok(unmatched(Reason::TrainGone));
                }
            }
            let rejection = leg_validation::validate_train_leg(
                &app.database,
                &train_uid,
                service_date,
                origin_crs.as_deref(),
                destination_crs.as_deref(),
            )
            .await?;
            if rejection.is_some() {
                return Ok(unmatched(Reason::TrainChanged));
            }
            Ok(Planned::Train {
                train_uid,
                service_date,
                origin_crs,
                destination_crs,
            })
        }
        Planned::Untracked {
            leg,
            origin_stop_id,
            destination_stop_id,
        } => {
            let with_stops = journeys::attach_untracked_stops(
                &app.database,
                leg.clone(),
                origin_stop_id.as_deref(),
                destination_stop_id.as_deref(),
            )
            .await?;
            Ok(Planned::Untracked {
                leg: with_stops.unwrap_or(leg),
                origin_stop_id: None,
                destination_stop_id: None,
            })
        }
        other => Ok(other),
    }
}

fn refused(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    RouteError::coded(status, code, message).render(false, "bad_request")
}

#[expect(
    clippy::too_many_lines,
    reason = "long but linear: read, judge each leg, check the caps, save, report"
)]
async fn post_restore(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(id): Path<i64>,
) -> Response {
    if !recently_deleted::enabled() {
        return not_found();
    }
    let retention_days = recently_deleted::retention_days();
    let row = match recently_deleted::get(&app.database, &user.id, id, retention_days).await {
        Ok(Some((_, summary))) => summary,
        Ok(None) => return not_found(),
        Err(err) => return internal("read a recently deleted journey")(err),
    };
    let summary: Summary = serde_json::from_value(row).unwrap_or_default();
    let today = crate::routes::london_today();
    let mut planned = Vec::with_capacity(summary.legs.len());
    for leg in summary
        .legs
        .iter()
        .take(crate::routes::journey_itinerary::MAX_ITINERARY_LEGS)
    {
        match check(&app, classify(leg, today)).await {
            Ok(leg) => planned.push(leg),
            Err(err) => return internal("check a deleted journey's legs")(err),
        }
    }
    if planned.iter().all(|leg| matches!(leg, Planned::LeftOut(_))) {
        return refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "nothing_to_restore",
            "None of this journey's legs can be saved again.",
        );
    }

    // The caps `POST /Journeys/itinerary` checks.
    match journeys::count_manual_journeys_for_user(&app.database, &user.id).await {
        Ok(owned) if owned >= journeys::MAX_JOURNEYS_PER_USER => {
            return refused(
                StatusCode::CONFLICT,
                "journey_limit",
                format!(
                    "You already have {} journeys, which is the maximum. Delete some old ones to \
                     make room for this one.",
                    journeys::MAX_JOURNEYS_PER_USER
                ),
            );
        }
        Ok(_) => {}
        Err(err) => return internal("count journeys")(err),
    }
    let mut new_trains: Vec<(&str, NaiveDate)> = Vec::new();
    for leg in &planned {
        let Planned::Train {
            train_uid,
            service_date,
            ..
        } = leg
        else {
            continue;
        };
        let pair = (train_uid.as_str(), *service_date);
        if new_trains.contains(&pair) {
            continue;
        }
        match train_tracking::user_tracks_train_uid(
            &app.database,
            &user.id,
            train_uid,
            *service_date,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => new_trains.push(pair),
            Err(err) => return internal("check existing subscription")(err),
        }
    }
    if !new_trains.is_empty() {
        let upcoming = match train_tracking::count_future_subscriptions_for_user(
            &app.database,
            &user.id,
        )
        .await
        {
            Ok(upcoming) => upcoming,
            Err(err) => return internal("count tracked trains")(err),
        };
        let new = i64::try_from(new_trains.len()).unwrap_or(i64::MAX);
        if upcoming.saturating_add(new) > train_tracking::MAX_FUTURE_PINS_PER_USER {
            return refused(
                StatusCode::CONFLICT,
                "pin_limit",
                train_tracking::pin_cap_message(),
            );
        }
    }

    // Each kept leg ready to save, and what the response says of each.
    let mut legs = Vec::with_capacity(planned.len());
    let mut report = Vec::with_capacity(planned.len());
    let mut trains = Vec::new();
    for (index, leg) in planned.into_iter().enumerate() {
        let position = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let (outcome, reason) = match leg {
            Planned::Train {
                train_uid,
                service_date,
                origin_crs,
                destination_crs,
            } => {
                let trains_id = match crate::data::trains::find_or_create_train(
                    &app.database,
                    &train_uid,
                    service_date,
                )
                .await
                {
                    Ok(trains_id) => trains_id,
                    Err(err) => return internal("find or create train")(err),
                };
                trains.push((legs.len(), trains_id, train_uid, service_date));
                legs.push(journeys::ItineraryLeg::Train {
                    trains_id,
                    service_date,
                    origin_crs,
                    destination_crs,
                });
                (Outcome::Tracked, None)
            }
            Planned::Untracked { leg, .. } => {
                legs.push(journeys::ItineraryLeg::Untracked(leg));
                (Outcome::Untracked, None)
            }
            Planned::Unmatched {
                service_date,
                origin_crs,
                destination_crs,
                reason,
            } => {
                legs.push(journeys::ItineraryLeg::Unmatched {
                    service_date,
                    origin_crs,
                    destination_crs,
                });
                (Outcome::WithoutTrain, Some(reason))
            }
            Planned::LeftOut(reason) => (Outcome::LeftOut, Some(reason)),
        };
        report.push(RestoredLeg {
            leg: position,
            outcome,
            reason,
        });
    }
    // A name that no longer passes the rule is dropped, not refused.
    let custom_name =
        train_tracking::validate_custom_name(summary.custom_name.as_deref()).unwrap_or_default();

    let mut tx = match app.database.begin().await {
        Ok(tx) => tx,
        Err(err) => return internal("start transaction")(err.into()),
    };
    // Off the list first, so of two concurrent saves only one goes ahead.
    match recently_deleted::take(&mut tx, &user.id, id, retention_days).await {
        Ok(true) => {}
        Ok(false) => return not_found(),
        Err(err) => return internal("take a recently deleted journey")(err),
    }
    let (journey_id, created) = match journeys::create_journey_from_itinerary(
        &mut tx,
        &user.id,
        custom_name.as_deref(),
        recently_deleted::WEB,
        &legs,
    )
    .await
    {
        Ok(created) => created,
        Err(err) => return internal("save a deleted journey again")(err),
    };
    if let Err(err) = tx.commit().await {
        return internal("commit the saved journey")(err.into());
    }

    // Best effort, after the commit, as `POST /Journeys/itinerary` does.
    for (leg_index, trains_id, train_uid, service_date) in &trains {
        if let Some(&(_, Some(tracking_id))) = created.get(*leg_index) {
            crate::routes::train::enrich_shared_train(
                &app,
                tracking_id,
                *trains_id,
                train_uid,
                *service_date,
            )
            .await;
        }
    }
    tracing::info!(
        event = "recently_deleted_restored",
        journey_id,
        legs = report.len(),
        without_train = report
            .iter()
            .filter(|leg| leg.outcome == Outcome::WithoutTrain)
            .count(),
        left_out = report
            .iter()
            .filter(|leg| leg.outcome == Outcome::LeftOut)
            .count(),
        "a deleted journey was saved again"
    );
    (
        StatusCode::CREATED,
        Json(RestoreResponse {
            journey_id,
            legs: report,
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2099, 6, 10).unwrap()
    }

    fn train(uid: Option<&str>, date: &str) -> SummaryLeg {
        SummaryLeg {
            service_date: Some(date.parse().unwrap()),
            origin_crs: Some("eus".to_string()),
            destination_crs: Some("MKC".to_string()),
            train_uid: uid.map(str::to_string),
            kind: Some("train".to_string()),
            ..SummaryLeg::default()
        }
    }

    fn walk(day: u32, arrival: bool) -> SummaryLeg {
        let at = |minute| {
            chrono_tz::Europe::London
                .with_ymd_and_hms(2099, 6, day, 9, minute, 0)
                .unwrap()
                .with_timezone(&Utc)
        };
        SummaryLeg {
            kind: Some("untracked".to_string()),
            transit_mode: Some("walk".to_string()),
            origin_name: Some("Home".to_string()),
            destination_name: Some("Station".to_string()),
            planned_departure: Some(at(0)),
            planned_arrival: arrival.then(|| at(15)),
            ..SummaryLeg::default()
        }
    }

    #[test]
    fn a_train_leg_is_tracked_again_with_its_stations() {
        assert_eq!(
            classify(&train(Some("c12345"), "2099-06-09"), today()),
            Planned::Train {
                train_uid: "C12345".to_string(),
                service_date: "2099-06-09".parse().unwrap(),
                origin_crs: Some("EUS".to_string()),
                destination_crs: Some("MKC".to_string()),
            }
        );
    }

    #[test]
    fn a_train_leg_with_no_train_comes_back_without_one() {
        for uid in [None, Some("not a uid")] {
            let Planned::Unmatched { reason, .. } = classify(&train(uid, "2099-06-09"), today())
            else {
                panic!("{uid:?}");
            };
            assert_eq!(reason, Reason::NoTrain);
        }
    }

    #[test]
    fn a_train_past_the_horizon_comes_back_without_it() {
        let Planned::Unmatched { reason, .. } =
            classify(&train(Some("C12345"), "2100-06-09"), today())
        else {
            panic!("expected unmatched");
        };
        assert_eq!(reason, Reason::TooFarAhead);
    }

    #[test]
    fn a_leg_with_no_date_is_left_out() {
        let leg = SummaryLeg {
            service_date: None,
            ..train(Some("C12345"), "2099-06-09")
        };
        assert_eq!(
            classify(&leg, today()),
            Planned::LeftOut(Reason::Incomplete)
        );
    }

    #[test]
    fn an_untracked_leg_comes_back_as_it_was() {
        let Planned::Untracked { leg, .. } = classify(&walk(9, true), today()) else {
            panic!("expected untracked");
        };
        assert_eq!(leg.transit_mode, "walk");
        assert_eq!(leg.origin_place.as_deref(), Some("Home"));
    }

    #[test]
    fn an_untracked_leg_without_its_arrival_is_left_out() {
        assert_eq!(
            classify(&walk(9, false), today()),
            Planned::LeftOut(Reason::Incomplete)
        );
    }

    #[test]
    fn an_untracked_leg_over_a_week_old_is_left_out() {
        assert_eq!(
            classify(&walk(1, true), today()),
            Planned::LeftOut(Reason::TooOld)
        );
    }

    #[test]
    fn a_summary_of_an_unknown_shape_has_no_legs() {
        let summary: Summary =
            serde_json::from_value(serde_json::json!({"legs": "nope"})).unwrap_or_default();
        assert_eq!(summary.legs.len(), 0);
        let summary: Summary = serde_json::from_value(serde_json::json!({
            "customName": "Work", "legs": [{"serviceDate": "2099-06-09", "extra": 1}]
        }))
        .unwrap();
        assert_eq!(summary.custom_name.as_deref(), Some("Work"));
        assert_eq!(summary.legs.len(), 1);
    }

    #[test]
    fn a_restored_leg_without_a_reason_has_no_reason_field() {
        let json = serde_json::to_value(RestoredLeg {
            leg: 1,
            outcome: Outcome::Tracked,
            reason: None,
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({"leg": 1, "outcome": "tracked"}));
        let json = serde_json::to_value(RestoredLeg {
            leg: 2,
            outcome: Outcome::WithoutTrain,
            reason: Some(Reason::TrainGone),
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"leg": 2, "outcome": "without_train", "reason": "train_gone"})
        );
    }
}
