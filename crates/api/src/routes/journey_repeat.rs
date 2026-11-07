//! "Track this journey again" and "Move this journey" (`data::journey_repeat`).
//!
//! | Route | What |
//! |---|---|
//! | `GET /Journeys/{id}/repeat?date=&time=&basis=` | The same route planned on `date` from `time`, preferring the same trains. Saves nothing |
//! | `POST /Journeys/{id}/move` | Replaces every leg of a journey that hasn't started |
//!
//! `GET .../repeat` is for the owner and, with a session, anyone the
//! journey is shared with in a group (`journey_readable_by`); a share-link
//! visitor has no session for it. A delegated caller (`journeys:read`,
//! `DELEGATED_ROUTES`) reads its own journeys only. It runs the planner,
//! so it shares `/Trips/plan`'s rate-limit budget and a user's planning
//! slot (`ReplanSlot::Request`: a 503 when every slot is busy), never the
//! plan sweep's. Tracking again saves through `POST /Journeys/itinerary`.
//!
//! `POST .../move` is cookie-only and owner-only (404 otherwise), and not a
//! delegated route: like every other change to a journey's legs, it is
//! `journey_replace::replace` (D-8). The page previews it with
//! `GET .../repeat?basis=current`.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{NaiveDate, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};

use crate::app::{App, Router};
use crate::auth::AuthenticatedUser;
use crate::auth::delegated::{DelegatedOrSessionUser, ReadScope};
use crate::data::journey_repeat::{self, Basis, Unsupported};
use crate::data::journey_replace::{self, Replacement};
use crate::data::journey_revisions::{Reason, RevisionMeta};
use crate::data::regular_journeys::{self, PlanOptions};
use crate::data::regular_plan::{self, ChoiceReason, PlanOption};
use crate::data::trip_planning_itinerary::TimeBound;
use crate::data::{journey_breaks, journeys};
use crate::routes::journey_itinerary::RouteError;
use crate::routes::journey_replan::{internal, now_utc, replaced_response, require_owner};
use crate::routes::trips::ReplanSlot;

pub fn router() -> Router {
    Router::new()
        .route(
            "/Journeys/{journey_id}/repeat",
            axum::routing::get(get_repeat),
        )
        .route(
            "/Journeys/{journey_id}/move",
            axum::routing::post(post_move),
        )
}

fn not_found() -> RouteError {
    RouteError::coded(
        StatusCode::NOT_FOUND,
        "not_found",
        "no journey with that id",
    )
}

fn bad_request(message: &'static str) -> RouteError {
    RouteError::coded(StatusCode::BAD_REQUEST, "bad_request", message)
}

/// `GET /Journeys/{id}/repeat`'s query.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RepeatParams {
    /// `YYYY-MM-DD`, today or later; default today if the first train's
    /// time is still ahead today, else tomorrow.
    #[serde(default)]
    date: Option<NaiveDate>,
    /// `HH:MM` London; default the first train's booked time.
    #[serde(default)]
    time: Option<String>,
    /// `booked` (default: revision 1) or `current` (the plan as it stands,
    /// the owner's only: what a move previews).
    #[serde(default)]
    basis: Option<String>,
}

/// What `GET /Journeys/{id}/repeat` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename = "JourneyRepeatStatus")]
pub(crate) enum RepeatStatus {
    /// A choice, and maybe alternatives.
    Planned,
    /// No timetable for the date yet.
    Pending,
    /// No way there that day.
    NoService,
    /// The journey can't be planned again (`unsupported` says why).
    Unsupported,
}

/// `GET /Journeys/{id}/repeat`'s body.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "JourneyRepeatResponse")]
pub(crate) struct RepeatResponse {
    status: RepeatStatus,
    /// Why, when `status` is `unsupported`.
    unsupported: Option<Unsupported>,
    /// The day and London time planned (the defaults when not asked for).
    date: NaiveDate,
    /// `HH:MM`.
    time: String,
    /// The first train's booked London time in the plan read, `HH:MM`;
    /// `null` when unsupported.
    original_time: Option<String>,
    /// As a regular plan's: `usual` (the same trains), `same_operator` or
    /// `best`.
    reason: Option<ChoiceReason>,
    /// Ready to save: train legs as `POST /Journeys/itinerary` takes them
    /// (`trainUid`, `serviceDate`, `originCrs`, `destinationCrs`) and the
    /// untracked legs before and after the trains, moved with them.
    choice: Option<PlanOption>,
    alternatives: Vec<PlanOption>,
    /// The stations the plan went via (the original changes); empty when
    /// it was planned without them.
    via: Vec<String>,
}

fn hh_mm(time: NaiveTime) -> String {
    format!("{:02}:{:02}", time.hour(), time.minute())
}

/// The plan options of the regular journey a journey was made from.
async fn template_options(app: &App, journey_id: i64) -> Result<PlanOptions, RouteError> {
    let row: Option<(Option<sqlx::types::Json<serde_json::Value>>,)> = sqlx::query_as(
        "SELECT jt.plan_options FROM journeys j \
         JOIN journey_templates jt ON jt.id = j.source_template_id AND jt.user_id = j.user_id \
         WHERE j.id = $1",
    )
    .bind(journey_id)
    .fetch_optional(&app.database)
    .await
    .map_err(|err| internal("read regular journey")(err.into()))?;
    Ok(row
        .and_then(|(options,)| regular_journeys::decode_stored(options))
        .unwrap_or_default())
}

/// `GET /Journeys/{id}/repeat?date=&time=&basis=`: see the module doc.
/// 400 for a date before today or a bad time, 404 for a journey the caller
/// can't read (or, with `basis=current`, doesn't own).
async fn get_repeat(
    State(app): State<App>,
    user: DelegatedOrSessionUser<ReadScope>,
    Path(journey_id): Path<i64>,
    Query(params): Query<RepeatParams>,
) -> Response {
    let delegated = user.is_bearer();
    match repeat(&app, &user, journey_id, params).await {
        Ok(body) => Json(body).into_response(),
        Err(err) => err.render(delegated, "bad_request"),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "linear: read, check, plan via the changes, then without"
)]
async fn repeat(
    app: &App,
    user: &DelegatedOrSessionUser<ReadScope>,
    journey_id: i64,
    params: RepeatParams,
) -> Result<RepeatResponse, RouteError> {
    let basis = match params.basis.as_deref() {
        None | Some("booked") => Basis::Booked,
        Some("current") => Basis::Current,
        Some(_) => return Err(bad_request("basis must be booked or current.")),
    };
    let time = params
        .time
        .as_deref()
        .map(|text| NaiveTime::parse_from_str(text.trim(), "%H:%M"))
        .transpose()
        .map_err(|_| bad_request("time must be HH:MM."))?;
    let owner = journeys::journey_owner(&app.database, journey_id)
        .await
        .map_err(internal("read journey owner"))?
        .ok_or_else(not_found)?;
    let is_owner = owner == user.user_id;
    if (user.is_bearer() || basis == Basis::Current) && !is_owner {
        return Err(not_found());
    }
    if !is_owner
        && !journeys::journey_readable_by(&app.database, journey_id, &user.user_id)
            .await
            .map_err(internal("check journey readability"))?
    {
        return Err(not_found());
    }
    let now = now_utc();
    let today = journey_repeat::london_clock(now).0;
    if params.date.is_some_and(|date| date < today) {
        return Err(bad_request("date must be today or later."));
    }

    let legs = journey_repeat::source_legs(&app.database, journey_id, basis)
        .await
        .map_err(internal("read journey plan"))?;
    let options = template_options(app, journey_id).await?;
    let shape = if options
        .waypoints
        .as_deref()
        .is_some_and(|w| !w.trim().is_empty())
    {
        Err(Unsupported::Waypoints)
    } else if options
        .transit
        .as_deref()
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("true"))
    {
        Err(Unsupported::Transit)
    } else {
        journey_repeat::shape(&legs)
    };
    let shape = match shape {
        Ok(shape) => shape,
        Err(why) => {
            let time = time.unwrap_or_default();
            return Ok(RepeatResponse {
                status: RepeatStatus::Unsupported,
                unsupported: Some(why),
                date: params
                    .date
                    .unwrap_or_else(|| journey_repeat::default_date(time, now)),
                time: hh_mm(time),
                original_time: None,
                reason: None,
                choice: None,
                alternatives: Vec::new(),
                via: Vec::new(),
            });
        }
    };
    let original = journey_repeat::london_clock(shape.first_departure).1;
    let time = time.unwrap_or(original);
    let date = params
        .date
        .unwrap_or_else(|| journey_repeat::default_date(time, now));
    let mut response = RepeatResponse {
        status: RepeatStatus::Pending,
        unsupported: None,
        date,
        time: hh_mm(time),
        original_time: Some(hh_mm(original)),
        reason: None,
        choice: None,
        alternatives: Vec::new(),
        via: Vec::new(),
    };
    if !crate::data::queries::schedule_destination_departures_published_for(&app.database, date)
        .await
        .map_err(internal("read timetable"))?
    {
        return Ok(response);
    }

    let target = journey_repeat::london_instant(date, time);
    let usual = journey_repeat::usual_at(&shape.usual, time, original);
    // From a few minutes early, so the same operator's train a little
    // earlier than it used to be is found.
    let minutes = (time.hour() * 60 + time.minute())
        .saturating_sub(u32::try_from(regular_plan::SAME_OPERATOR_WINDOW_MINUTES).unwrap_or(0));
    let mut attempts: Vec<(Vec<String>, PlanOptions)> = Vec::new();
    if !shape.vias.is_empty() {
        attempts.push((
            shape.vias.clone(),
            PlanOptions {
                via: Some(shape.vias.join(",")),
                ..options.clone()
            },
        ));
    }
    attempts.push((Vec::new(), options));
    for (via, options) in attempts {
        let Ok(query) = crate::routes::trips::regular_plan_query(
            app,
            &shape.origin,
            &shape.destination,
            date,
            TimeBound::DepartAfter(minutes),
            0,
            &options,
        )
        .await
        else {
            continue;
        };
        let Some(itineraries) =
            crate::routes::regular_plan::departures(app, query, ReplanSlot::Request)
                .await
                .map_err(RouteError::from)?
        else {
            return Ok(response);
        };
        let planned = crate::routes::regular_plan::plan_options(app, &itineraries, date, now).await;
        let Some((choice, reason, alternatives)) =
            journey_repeat::pick(&usual, date, target, &planned)
        else {
            continue;
        };
        let whole = |index: usize| journey_repeat::with_untracked(&shape, planned[index].clone());
        response.status = RepeatStatus::Planned;
        response.reason = Some(reason);
        response.choice = Some(whole(choice));
        response.alternatives = alternatives.into_iter().map(whole).collect();
        response.via = via;
        metrics::counter!(
            ds_runtime::metrics::metric_name("api_journey_repeat_total"),
            "reason" => match reason {
                ChoiceReason::Usual => "usual",
                ChoiceReason::SameOperator => "same_operator",
                ChoiceReason::Best => "best",
            }
        )
        .increment(1);
        return Ok(response);
    }
    response.status = RepeatStatus::NoService;
    Ok(response)
}

/// `POST /Journeys/{id}/move`'s body.
#[derive(Debug, Deserialize, Serialize, ts_rs::TS)]
#[ts(rename = "MoveJourneyRequest")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct MoveBody {
    /// The journey's new legs, each as `POST /Journeys/itinerary` takes
    /// them (a `GET .../repeat?basis=current` option's legs).
    legs: Vec<crate::routes::journey_itinerary::ItineraryLegRequest>,
}

fn started() -> RouteError {
    RouteError::coded(
        StatusCode::CONFLICT,
        "journey_started",
        "This journey has started, so it can't be moved.",
    )
}

/// `POST /Journeys/{id}/move` `{legs}`: replaces every leg of the caller's
/// journey with `legs` (`journey_replace::replace`), which keeps the
/// journey (its id, name, regular journey and revisions). While it is still
/// being planned revision 1, the booked plan, becomes the new plan;
/// otherwise (it has had a break) the move is the next revision, and
/// resolves an open break. 409 `journey_started` once a leg has left; 404
/// when not the caller's. Answers as `POST .../replace` does
/// (`ReplaceResponse`). A day the regular plan sweep planned is no longer
/// that day: its plan is cleared, and the new legs are the owner's pick,
/// which the sweep never replaces.
async fn post_move(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(journey_id): Path<i64>,
    Json(body): Json<MoveBody>,
) -> Response {
    crate::routes::journey_replan::render(move_journey(&app, &user.id, journey_id, body).await)
}

async fn move_journey(
    app: &App,
    user_id: &str,
    journey_id: i64,
    body: MoveBody,
) -> Result<Response, RouteError> {
    if body.legs.is_empty()
        || i64::try_from(body.legs.len()).unwrap_or(i64::MAX) > journeys::MAX_LEGS_PER_JOURNEY
    {
        return Err(RouteError::coded(
            StatusCode::BAD_REQUEST,
            "invalid_itinerary",
            format!(
                "Move to between 1 and {} legs.",
                journeys::MAX_LEGS_PER_JOURNEY
            ),
        ));
    }
    let today = crate::routes::london_today();
    let mut valid = Vec::with_capacity(body.legs.len());
    for leg in &body.legs {
        valid.push(crate::routes::journey_itinerary::validate_leg(leg, today)?);
    }
    require_owner(app, journey_id, user_id).await?;
    let now = now_utc();
    let started_sql = format!(
        "SELECT {} FROM journeys j WHERE j.id = $1",
        regular_journeys::occurrence_started_sql("j", "$2", "$3")
    );
    let (has_started,): (bool,) = sqlx::query_as(&started_sql)
        .bind(journey_id)
        .bind(now)
        .bind(journey_repeat::london_clock(now).0)
        .fetch_one(&app.database)
        .await
        .map_err(|err| internal("read journey")(err.into()))?;
    if has_started {
        return Err(started());
    }
    crate::routes::journey_itinerary::check_legs_call(app, &valid).await?;
    let resolved = crate::routes::journey_itinerary::resolve_legs(app, &valid).await?;
    let leg_ids: Vec<(i64,)> =
        sqlx::query_as("SELECT id FROM journey_legs WHERE journey_id = $1 ORDER BY leg_order")
            .bind(journey_id)
            .fetch_all(&app.database)
            .await
            .map_err(|err| internal("read journey legs")(err.into()))?;
    let (Some((first,)), Some((last,))) = (leg_ids.first(), leg_ids.last()) else {
        return Err(not_found());
    };
    let open = journey_breaks::open_break(&app.database, journey_id)
        .await
        .map_err(internal("read break"))?;
    let meta = match &open {
        Some(row) => RevisionMeta {
            reason: Reason::Replace,
            cause: row.cause.clone(),
            fault: row.fault.clone(),
            break_id: Some(row.id),
            actor: "owner",
        },
        None => RevisionMeta::user_choice(),
    };
    let outcome = journey_replace::replace(
        &app.database,
        &Replacement {
            journey_id,
            user_id,
            from_leg_id: *first,
            through_leg_id: Some(*last),
            legs: &resolved,
            meta,
            trigger_fate: "not_used",
            now,
            idempotency: None,
            regular: None,
        },
    )
    .await
    .map_err(internal("move journey"))?;
    match outcome {
        journey_replace::Replaced::Travelled => return Err(started()),
        journey_replace::Replaced::Done { .. } => {
            sqlx::query(
                "UPDATE journeys SET regular_plan = NULL, regular_plan_status = NULL, \
                        regular_plan_marker = NULL, regular_planned_at = NULL \
                 WHERE id = $1 AND regular_plan_status IS NOT NULL",
            )
            .bind(journey_id)
            .execute(&app.database)
            .await
            .map_err(|err| internal("clear regular plan")(err.into()))?;
            metrics::counter!(ds_runtime::metrics::metric_name("api_journey_moved_total"))
                .increment(1);
        }
        _ => {}
    }
    replaced_response(outcome)
}

#[cfg(test)]
#[path = "journey_repeat_db_tests.rs"]
mod db_tests;
