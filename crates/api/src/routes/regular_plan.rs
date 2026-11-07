//! `POST /Journeys/{id}/regular-plan/accept` (regular journeys spec
//! 2026-11-06 §6.4): the one-tap pick of a regular journey's day. The plan
//! sweep (`regular_plan_sweep`) stores the choice and up to two
//! alternatives on the occurrence (`GET /Journeys/{id}`'s `regularPlan`);
//! this replaces every leg of the journey with the one picked, as auto mode
//! would, in one transaction (`journey_replace::replace`). The legs it
//! saves are the owner's pick (`manual`), so the sweep never replaces them.
//!
//! Cookie-only and owner-only (another user's journey is a 404), and not a
//! delegated route.
//!
//! It also holds the searches the sweep and "Track this journey again"
//! (`journey_repeat`) share: [`departures`], the next few trains from a
//! target through a given planning slot, and [`plan_options`], those
//! itineraries as the options [`regular_plan::choose`] picks from.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::app::{App, Router};
use crate::auth::AuthenticatedUser;
use crate::data::journey_alternatives;
use crate::data::journey_replace::{self, RegularWrite, Replacement};
use crate::data::journey_revisions::RevisionMeta;
use crate::data::regular_plan::{self, PlanOption, StoredPlan};
use crate::data::trip_planning_itinerary::{PlannedItinerary, PlannedLeg};
use crate::routes::journey_itinerary::RouteError;
use crate::routes::journey_replan::{internal, now_utc, render, replaced_response, require_owner};
use crate::routes::trips::{ReplanQuery, ReplanSlot};

pub fn router() -> Router {
    Router::new().route(
        "/Journeys/{journey_id}/regular-plan/accept",
        axum::routing::post(post_accept),
    )
}

/// `POST /Journeys/{id}/regular-plan/accept`'s body.
#[derive(Debug, Deserialize, Serialize, ts_rs::TS)]
#[ts(rename = "RegularPlanAcceptRequest")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AcceptBody {
    /// 0: the choice; 1 and 2: the alternatives, in order.
    index: u8,
    /// `regularPlan.plannedAt` as read: a 409 when the plan changed since.
    planned_at: DateTime<Utc>,
}

fn conflict(code: &'static str, message: &'static str) -> RouteError {
    RouteError::coded(StatusCode::CONFLICT, code, message)
}

/// `POST /Journeys/{id}/regular-plan/accept` `{index, plannedAt}`:
/// replaces the journey's legs with option `index` of its regular plan.
/// 404 when the journey isn't the caller's, has no plan with options, or
/// has no such option; 409 `plan_changed` when the plan changed since it
/// was read (or the journey has started or has a break), 409
/// `option_departed` when the option's first leg has left. Answers as
/// `POST .../replan/accept` does (`ReplaceResponse`).
async fn post_accept(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(journey_id): Path<i64>,
    Json(body): Json<AcceptBody>,
) -> Response {
    render(
        async {
            require_owner(&app, journey_id, &user.id).await?;
            let now = now_utc();
            let (plan, status, _, planned_at) =
                regular_plan::read_stored(&app.database, journey_id)
                    .await
                    .map_err(internal("read regular plan"))?;
            let no_plan = || {
                RouteError::coded(
                    StatusCode::NOT_FOUND,
                    "not_found",
                    "This journey has no train to pick; reload the journey.",
                )
            };
            let plan: StoredPlan = plan
                .filter(|_| matches!(status.as_deref(), Some("suggested" | "committed")))
                .ok_or_else(no_plan)?;
            if planned_at != Some(body.planned_at) {
                return Err(conflict(
                    "plan_changed",
                    "This journey's plan changed; reload the journey.",
                ));
            }
            let option = plan
                .option(usize::from(body.index))
                .cloned()
                .ok_or_else(no_plan)?;
            if option.departure < now - Duration::minutes(1) {
                return Err(conflict(
                    "option_departed",
                    "That train has already left; pick another.",
                ));
            }
            let mut trains = HashMap::new();
            for (uid, date) in option.train_key() {
                let id = crate::data::trains::find_or_create_train(&app.database, &uid, date)
                    .await
                    .map_err(internal("find or create train"))?;
                trains.insert((uid, date), id);
            }
            let legs = option.itinerary_legs(&trains).ok_or_else(|| {
                internal("resolve regular plan legs")(anyhow::anyhow!("a train was not resolved"))
            })?;
            let leg_ids: Vec<(i64,)> = sqlx::query_as(
                "SELECT id FROM journey_legs WHERE journey_id = $1 ORDER BY leg_order",
            )
            .bind(journey_id)
            .fetch_all(&app.database)
            .await
            .map_err(|err| internal("read journey legs")(err.into()))?;
            let (Some((first,)), Some((last,))) = (leg_ids.first(), leg_ids.last()) else {
                return Err(no_plan());
            };
            let accepted = StoredPlan {
                accepted_index: Some(body.index),
                ..plan
            };
            let outcome = journey_replace::replace(
                &app.database,
                &Replacement {
                    journey_id,
                    user_id: &user.id,
                    from_leg_id: *first,
                    through_leg_id: Some(*last),
                    legs: &legs,
                    meta: RevisionMeta::user_choice(),
                    trigger_fate: "not_used",
                    now,
                    idempotency: None,
                    regular: Some(RegularWrite {
                        plan: &accepted,
                        status: "committed",
                        marker: None,
                        read_planned_at: planned_at,
                        sweep: false,
                    }),
                },
            )
            .await
            .map_err(internal("replace legs"))?;
            if matches!(outcome, journey_replace::Replaced::Done { .. }) {
                metrics::counter!(
                    ds_runtime::metrics::metric_name("api_regular_plan_accepted_total"),
                    "index" => match body.index { 0 => "0", 1 => "1", _ => "2" }
                )
                .increment(1);
            }
            replaced_response(outcome)
        }
        .await,
    )
}

/// How many trains one target looks at: the first `fastest` search from
/// the target, then each from just after (leave after) or just before
/// (arrive by) the one it found.
pub const SEARCHES: usize = 6;

/// The next [`SEARCHES`] ways there from `query`'s target, each a
/// `fastest` search holding `slot` (the plan sweep's own, or a user's for
/// "Track this journey again"), in the order found (later departures for
/// a leave-after target, earlier arrivals for an arrive-by one). `None`
/// when no timetable is published for the date. A user's slot that is
/// busy is the planner's 503.
pub(crate) async fn departures(
    app: &App,
    mut query: ReplanQuery,
    slot: ReplanSlot,
) -> Result<Option<Vec<PlannedItinerary>>, (StatusCode, String)> {
    let mut found: Vec<PlannedItinerary> = Vec::new();
    for _ in 0..SEARCHES {
        let list = match crate::routes::trips::plan_for_journey(app, &query, slot).await? {
            Some(list) => list,
            None if found.is_empty() => return Ok(None),
            None => break,
        };
        let Some(first) = list.into_iter().next() else {
            break;
        };
        // The minutes found already include the day offset. A minute
        // after it left, or before it arrived (DS-398: arriving by its own
        // arrival found it again).
        let next = crate::routes::trips::step_clock(query.searched_time(), &first);
        query.day_offset = 0;
        if !found
            .iter()
            .any(|seen| crate::routes::trips::same_journey(seen, &first))
        {
            found.push(first);
        }
        match next {
            Some(time) => query.time = time,
            None => break,
        }
    }
    Ok(Some(found))
}

/// `itineraries` (from [`departures`]) as options on `date`, with their
/// station and operator names (`regular_plan::options_from`: none
/// cancelled, none leaving before `now`).
pub(crate) async fn plan_options(
    app: &App,
    itineraries: &[PlannedItinerary],
    date: chrono::NaiveDate,
    now: DateTime<Utc>,
) -> Vec<PlanOption> {
    let mut codes: Vec<String> = Vec::new();
    let mut operators: Vec<String> = Vec::new();
    for itinerary in itineraries {
        for leg in &itinerary.legs {
            if let PlannedLeg::Train {
                origin_crs,
                destination_crs,
                operator,
                ..
            } = leg
            {
                codes.extend(origin_crs.iter().chain(destination_crs).cloned());
                operators.extend(operator.iter().cloned());
            }
        }
    }
    codes.sort();
    codes.dedup();
    operators.sort();
    operators.dedup();
    let names = journey_alternatives::station_names(&app.database, &codes).await;
    let operator_names = journey_alternatives::operator_names(&app.database, &operators).await;
    regular_plan::options_from(itineraries, date, now, &names, &operator_names)
}
