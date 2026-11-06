//! `/JourneyTemplates/...`: durable, reusable journey templates (Phase B).
//! See docs/superpowers/specs/2026-09-22-reusable-repeating-journeys-design.md
//! and
//! docs/superpowers/plans/2026-09-22-reusable-journeys-phaseB-durable-templates-plan.md.
//! Every route here requires an authenticated session (`AuthenticatedUser`),
//! same posture as `routes::journeys`. Materialization
//! (`POST .../materialize`) is manual/synchronous only in this phase -- no
//! scheduler, no automated sweep; that's Phase C's job, not this file's.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use common::TimeWindow;
use serde::{Deserialize, Serialize};

use crate::app::{App, Router};
use crate::auth::AuthenticatedUser;
use crate::data::journey_templates::{self, TemplateLegInput};
use crate::data::journeys;
use crate::data::regular_journeys::{self, PlanOptions, TemplateTarget, UsualItinerary};
use crate::data::train_tracking;

pub fn router() -> Router {
    Router::new()
        .route(
            "/JourneyTemplates",
            axum::routing::post(post_journey_template),
        )
        .route(
            "/JourneyTemplates/mine",
            axum::routing::get(get_my_journey_templates),
        )
        .route(
            "/JourneyTemplates/{template_id}",
            axum::routing::get(get_journey_template)
                .put(put_journey_template)
                .delete(delete_journey_template),
        )
        .route(
            "/JourneyTemplates/{template_id}/materialize",
            axum::routing::post(post_materialize_journey_template),
        )
        .route(
            "/JourneyTemplates/{template_id}/active",
            axum::routing::put(put_journey_template_active),
        )
        .route(
            "/JourneyTemplates/{template_id}/skip",
            axum::routing::post(post_journey_template_skip),
        )
        .route(
            "/JourneyTemplates/{template_id}/skip/{service_date}",
            axum::routing::delete(delete_journey_template_skip),
        )
        .route(
            "/JourneyTemplates/{template_id}/pair",
            axum::routing::delete(delete_journey_template_pair),
        )
}

/// One manually-entered template leg on the wire -- shared by
/// `CreateJourneyTemplateRequest::Manual` and `PutJourneyTemplateRequest`.
/// Field-for-field the `window`-mode shape of `journeys::CreateJourneyLegRequest`
/// minus `service_date` (a template leg is date-less) and minus `mode`
/// itself (there's only one leg shape here, no pin/knownTrain equivalent
/// for a template -- see this plan's Judgment Call 2 for why that's fine).
#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TemplateLegRequest {
    origin_crs: String,
    destination_crs: String,
    #[serde(default)]
    #[ts(optional = nullable)]
    depart_window: TimeWindow,
    #[serde(default)]
    #[ts(optional = nullable)]
    arrive_window: TimeWindow,
}

/// `POST /JourneyTemplates`'s two mutually-exclusive creation shapes --
/// mirrors `journeys::CreateJourneyLegRequest`'s own tagged-enum shape and
/// serde attributes exactly (same `rename_all_fields` requirement, same
/// reasoning).
#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(
    tag = "mode",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[ts(optional_fields = nullable)]
#[expect(
    clippy::large_enum_variant,
    reason = "a request body, deserialised once per request"
)]
pub(crate) enum CreateJourneyTemplateRequest {
    /// Build a new template directly from caller-supplied legs. See this
    /// plan's Judgment Call 5: the frontend built in this plan never
    /// actually sends this variant (no "start from nothing" UI ships in
    /// Phase B) -- it exists because the task brief asks for both "create"
    /// and "promote," and because `PutJourneyTemplateRequest` needs this
    /// exact leg-list shape anyway.
    Manual {
        #[serde(default)]
        custom_name: Option<String>,
        legs: Vec<TemplateLegRequest>,
    },
    /// Promotes an existing journey the caller OWNS (not merely
    /// group-shared-with -- see this plan's Judgment Call 3) into a
    /// durable template. Reads the journey's own legs
    /// (`journeys::list_legs_for_journey`) and copies each one's
    /// origin/destination/window, no `service_date`, no train binding. A
    /// leg with a train and no window (tracked from a known train or saved
    /// from a plan) takes a departure window around that train's booked
    /// departure instead (`journey_templates::window_around_departure`).
    /// This is §6 item 2's "Make this a template" button.
    FromJourney {
        #[serde(default)]
        custom_name: Option<String>,
        journey_id: i64,
    },
    /// A regular journey from a plan (regular journeys design §4.2): the
    /// target it was planned for, the plan's options and the itinerary the
    /// user picked as their usual train. The template legs come from the
    /// itinerary's train legs, each with a departure window around its
    /// booked departure. With `return`, the return journey is created too
    /// (from `destinationCrs` back to `originCrs`, the same days) and the
    /// two are linked as a pair; the cap counts both.
    Plan {
        #[serde(default)]
        custom_name: Option<String>,
        origin_crs: String,
        destination_crs: String,
        target: TemplateTarget,
        #[serde(default)]
        plan_options: Option<PlanOptions>,
        usual_itinerary: UsualItinerary,
        /// Required: a regular journey recurs.
        days_of_week: i16,
        #[serde(default)]
        starts_on: Option<NaiveDate>,
        #[serde(default)]
        ends_on: Option<NaiveDate>,
        /// `manual` (the default: remind me to pick) or `auto`.
        #[serde(default)]
        #[ts(as = "Option<crate::ts_values::TemplateMatchMode>")]
        default_match_mode: Option<String>,
        #[serde(default, rename = "return")]
        return_journey: Option<ReturnJourneyRequest>,
    },
}

/// The return journey of a plan-mode create: its own target, options and
/// usual itinerary.
#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(optional_fields = nullable)]
pub(crate) struct ReturnJourneyRequest {
    #[serde(default)]
    custom_name: Option<String>,
    target: TemplateTarget,
    #[serde(default)]
    plan_options: Option<PlanOptions>,
    usual_itinerary: UsualItinerary,
}

/// `PUT /JourneyTemplates/{id}`'s body -- full-resource replace (Judgment
/// Calls 1 and 4). Same `TemplateLegRequest` shape as the `Manual` create
/// variant; no `mode` tag needed since there's only one shape for an edit.
///
/// `defaultMatchMode: "auto"` is refused (400) while any leg has no time at
/// all (`journey_templates::validate_auto_mode_legs`).
///
/// The six recurrence fields (`days_of_week` through `auto_commit_rule`)
/// are deliberately REQUIRED here, unlike `custom_name`'s
/// `#[serde(default)]` -- this is a full-resource-replace endpoint (every
/// save resends the whole `legs` list too), so making these required means
/// a caller can never accidentally wipe out a template's recurrence config
/// by omitting them from a plain name/leg edit. The frontend always
/// populates every field from the currently-loaded template before
/// submitting.
#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PutJourneyTemplateRequest {
    #[serde(default)]
    #[ts(optional = nullable)]
    custom_name: Option<String>,
    legs: Vec<TemplateLegRequest>,
    days_of_week: Option<i16>,
    active: bool,
    starts_on: Option<NaiveDate>,
    ends_on: Option<NaiveDate>,
    #[ts(as = "crate::ts_values::TemplateMatchMode")]
    default_match_mode: String,
    #[ts(as = "Option<crate::ts_values::AutoCommitRule>")]
    auto_commit_rule: Option<String>,
    /// Regular journeys (additive): each optional, and an absent one keeps
    /// the stored value. `target` needs `originCrs` and `destinationCrs`
    /// unless the template already has a target; `planOptions` and
    /// `usualItinerary` need a target.
    #[serde(default)]
    #[ts(optional)]
    origin_crs: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    destination_crs: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    target: Option<TemplateTarget>,
    #[serde(default)]
    #[ts(optional)]
    plan_options: Option<PlanOptions>,
    #[serde(default)]
    #[ts(optional)]
    usual_itinerary: Option<UsualItinerary>,
}

#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MaterializeTemplateRequest {
    /// Required, no server-side default -- matches
    /// `journeys::CreateJourneyLegRequest`'s own convention that every
    /// leg-creation wire type takes an explicit `serviceDate`, never an
    /// implicit "today". The frontend's "Run now" button (Task 9) defaults
    /// its own date field to today client-side and lets the user change
    /// it before submitting, the same "user is present and picks a date"
    /// posture the design doc's §1 describes for the whole "reusable"
    /// trigger mechanism.
    service_date: NaiveDate,
}

#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateJourneyTemplateResponse {
    template_id: i64,
    /// The return journey's template, for a `plan` create with `return`;
    /// `null` otherwise.
    return_template_id: Option<i64>,
}

#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetTemplateActiveRequest {
    active: bool,
}

/// `PUT /JourneyTemplates/{id}/active`'s response.
#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetTemplateActiveResponse {
    /// The template and, if it has one, its pair.
    template_ids: Vec<i64>,
    /// Untouched journeys a pause deleted.
    deleted_journey_ids: Vec<i64>,
}

#[derive(Debug, Default, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(optional_fields = nullable)]
pub(crate) struct SkipTemplateDateRequest {
    /// Default: the template's `nextDate`.
    #[serde(default)]
    service_date: Option<NaiveDate>,
    /// Skip the paired return journey's date too.
    #[serde(default)]
    pair: Option<bool>,
}

/// `POST /JourneyTemplates/{id}/skip`'s response.
#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SkipTemplateDateResponse {
    service_date: NaiveDate,
    template_ids: Vec<i64>,
    /// Journeys already added for that date, now deleted.
    deleted_journey_ids: Vec<i64>,
}

#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MaterializeTemplateResponse {
    journey_id: i64,
    leg_ids: Vec<i64>,
}

#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JourneyTemplateLegDetailResponse {
    id: i64,
    origin_crs: Option<String>,
    origin_name: Option<String>,
    destination_crs: Option<String>,
    destination_name: Option<String>,
    depart_after: Option<NaiveTime>,
    depart_before: Option<NaiveTime>,
    arrive_after: Option<NaiveTime>,
    arrive_before: Option<NaiveTime>,
}

/// `GET /JourneyTemplates/{id}`'s response. `daysOfWeek`/`active`/
/// `startsOn`/`endsOn`/`defaultMatchMode`/`autoCommitRule` are all real,
/// round-tripped fields (so a future Phase C edit UI has somewhere to
/// read/write) -- this plan's own frontend (Task 8) renders NONE of them
/// as editable controls; see that task's own scope note.
#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JourneyTemplateDetailResponse {
    id: i64,
    custom_name: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    days_of_week: Option<i16>,
    active: bool,
    starts_on: Option<NaiveDate>,
    ends_on: Option<NaiveDate>,
    #[ts(as = "crate::ts_values::TemplateMatchMode")]
    default_match_mode: String,
    #[ts(as = "Option<crate::ts_values::AutoCommitRule>")]
    auto_commit_rule: Option<String>,
    legs: Vec<JourneyTemplateLegDetailResponse>,
    /// Regular journeys (additive): `null` for a template without a
    /// target. See `JourneyTemplateListItem` for `nextDate` and
    /// `skippedDates`.
    origin_crs: Option<String>,
    origin_name: Option<String>,
    destination_crs: Option<String>,
    destination_name: Option<String>,
    target: Option<TemplateTarget>,
    plan_options: Option<PlanOptions>,
    usual_itinerary: Option<UsualItinerary>,
    paired_template_id: Option<i64>,
    next_date: Option<NaiveDate>,
    skipped_dates: Vec<NaiveDate>,
}

fn internal_error(operation: &'static str) -> impl Fn(anyhow::Error) -> (StatusCode, String) {
    move |err| {
        if let Some(unavailable) = crate::unavailable::response_for(&err) {
            return unavailable;
        }
        tracing::error!(error = ?err, operation, "journey template request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to {operation}"),
        )
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "callers hand over values they no longer need"
)]
fn to_template_leg_input(
    leg: TemplateLegRequest,
) -> Result<TemplateLegInput, (StatusCode, String)> {
    journey_templates::validate_template_leg(
        &leg.origin_crs,
        &leg.destination_crs,
        &leg.depart_window,
        &leg.arrive_window,
    )
    .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
    Ok(TemplateLegInput {
        origin_crs: Some(leg.origin_crs.trim().to_ascii_uppercase()),
        destination_crs: Some(leg.destination_crs.trim().to_ascii_uppercase()),
        depart_after: leg.depart_window.after,
        depart_before: leg.depart_window.before,
        arrive_after: leg.arrive_window.after,
        arrive_before: leg.arrive_window.before,
    })
}

/// One leg of a journey being promoted to a template: its stations and
/// window. A leg made by tracking a train or saving a plan has no window
/// of its own; copied verbatim it would become a leg with no time, which
/// the sweep has no window to remind or pick for, so it takes a departure
/// window around its train's booked departure (`booked`, by leg id, from
/// `journey_templates::booked_departures_for_journey`) instead. A leg with
/// a window keeps it.
fn promoted_leg_input(
    leg: journeys::JourneyLegWithNamesRow,
    booked: &std::collections::HashMap<i64, NaiveTime>,
) -> TemplateLegInput {
    let mut input = TemplateLegInput {
        origin_crs: leg.origin_crs,
        destination_crs: leg.destination_crs,
        depart_after: leg.depart_after,
        depart_before: leg.depart_before,
        arrive_after: leg.arrive_after,
        arrive_before: leg.arrive_before,
    };
    if journey_templates::leg_is_fully_open(&input)
        && let Some(departs) = booked.get(&leg.id)
    {
        let (after, before) = journey_templates::window_around_departure(*departs);
        input.depart_after = Some(after);
        input.depart_before = Some(before);
    }
    input
}

/// `POST /JourneyTemplates` -- create (`Manual`) or promote-from-journey
/// (`FromJourney`). Rejects an empty `legs` array with 400 for `Manual`
/// mode (see `data::journey_templates::create_template`'s own
/// non-empty-invariant doc comment); rejects a source journey with any
/// leg missing an `origin_crs`/`destination_crs` with 400 for `FromJourney`
/// mode (the "no schedule data yet" gap named in Task 1's migration
/// comment -- rather than silently dropping that leg or minting a
/// half-blank template leg, this fails loudly with an actionable message).
#[expect(
    clippy::similar_names,
    clippy::too_many_lines,
    reason = "the similar names are distinct domain terms; one arm per creation shape"
)]
async fn post_journey_template(
    State(app): State<App>,
    user: AuthenticatedUser,
    Json(body): Json<CreateJourneyTemplateRequest>,
) -> Result<Json<CreateJourneyTemplateResponse>, (StatusCode, String)> {
    // Per-user cap (2026-09-26 review, L10) -- both arms below create a new
    // template. Same route-level count-then-insert shape as
    // `routes::lines`'s custom-line cap (and the same accepted race).
    let owned = journey_templates::count_templates_for_user(&app.database, &user.id)
        .await
        .map_err(internal_error("count journey templates"))?;
    if owned >= journey_templates::MAX_JOURNEY_TEMPLATES_PER_USER {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "You already have {} journey templates, which is the maximum. Delete one to \
                 make room for a new one.",
                journey_templates::MAX_JOURNEY_TEMPLATES_PER_USER
            ),
        ));
    }
    match body {
        CreateJourneyTemplateRequest::Manual { custom_name, legs } => {
            if legs.is_empty() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "A template needs at least one leg.".to_string(),
                ));
            }
            journey_templates::validate_leg_count(legs.len())
                .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
            // 2026-09 Signal Box Audit Low finding: same unvalidated
            // custom_name write path `routes::journeys::post_journey` had
            // (see that fix's own doc comment) -- this one lives right
            // next to it in the sibling templates route, and was equally
            // missing the trim/length-cap every other custom-name write in
            // this codebase applies.
            let custom_name = train_tracking::validate_custom_name(custom_name.as_deref())
                .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
            let leg_inputs = legs
                .into_iter()
                .map(to_template_leg_input)
                .collect::<Result<Vec<_>, _>>()?;
            let template_id = journey_templates::create_template(
                &app.database,
                &user.id,
                custom_name.as_deref(),
                &leg_inputs,
            )
            .await
            .map_err(internal_error("create journey template"))?;
            Ok(Json(CreateJourneyTemplateResponse {
                template_id,
                return_template_id: None,
            }))
        }
        CreateJourneyTemplateRequest::FromJourney {
            custom_name,
            journey_id,
        } => {
            // Same custom_name cap as the `Manual` arm above -- see its own
            // doc comment.
            let custom_name = train_tracking::validate_custom_name(custom_name.as_deref())
                .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
            // Ownership, not mere readability -- Judgment Call 3.
            let owner = journeys::journey_owner(&app.database, journey_id)
                .await
                .map_err(internal_error("check journey ownership"))?;
            if owner.as_deref() != Some(user.id.as_str()) {
                return Err((StatusCode::NOT_FOUND, "no journey with that id".to_string()));
            }

            // Train legs only (spec 2026-10-20 §2.4): a template is a
            // repeating train search, and an untracked Tube or walk leg has
            // nothing to search for.
            let source_legs: Vec<_> = journeys::list_legs_for_journey(&app.database, journey_id)
                .await
                .map_err(internal_error("read journey legs"))?
                .into_iter()
                .filter(|leg| leg.kind == journeys::LEG_KIND_TRAIN)
                .collect();
            if source_legs.is_empty() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "This journey has no train legs to save as a template.".to_string(),
                ));
            }
            // Defense in depth alongside `Manual` mode's own check above:
            // a source journey's leg count isn't itself capped anywhere
            // today (each leg is added one `POST /Journeys/{id}/legs` call
            // at a time, never as a single array), but there's no reason to
            // let an already-oversized journey mint an equally-oversized
            // template here either.
            journey_templates::validate_leg_count(source_legs.len())
                .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
            if source_legs
                .iter()
                .any(|leg| leg.origin_crs.is_none() || leg.destination_crs.is_none())
            {
                return Err((
                    StatusCode::BAD_REQUEST,
                    "At least one leg of this journey has no known origin/destination yet. \
                     Try again once its train has schedule data."
                        .to_string(),
                ));
            }
            let booked =
                journey_templates::booked_departures_for_journey(&app.database, journey_id)
                    .await
                    .map_err(internal_error("read booked departures"))?;
            let leg_inputs: Vec<TemplateLegInput> = source_legs
                .into_iter()
                .map(|leg| promoted_leg_input(leg, &booked))
                .collect();

            let template_id = journey_templates::create_template(
                &app.database,
                &user.id,
                custom_name.as_deref(),
                &leg_inputs,
            )
            .await
            .map_err(internal_error("create journey template from journey"))?;
            Ok(Json(CreateJourneyTemplateResponse {
                template_id,
                return_template_id: None,
            }))
        }
        CreateJourneyTemplateRequest::Plan {
            custom_name,
            origin_crs,
            destination_crs,
            target,
            plan_options,
            usual_itinerary,
            days_of_week,
            starts_on,
            ends_on,
            default_match_mode,
            return_journey,
        } => {
            let default_match_mode = default_match_mode.unwrap_or_else(|| "manual".to_string());
            let auto_commit_rule = (default_match_mode == "auto").then_some("nearest_to_now");
            journey_templates::validate_template_recurrence(
                &default_match_mode,
                auto_commit_rule,
                Some(days_of_week),
                starts_on,
                ends_on,
            )
            .map_err(bad_request)?;
            let outbound = plan_template_input(
                &app,
                custom_name.as_deref(),
                &origin_crs,
                &destination_crs,
                target,
                plan_options,
                &usual_itinerary,
            )
            .await?;
            let return_input = match return_journey {
                Some(ret) => Some(
                    plan_template_input(
                        &app,
                        ret.custom_name.as_deref(),
                        &outbound.destination_crs,
                        &outbound.origin_crs,
                        ret.target,
                        ret.plan_options,
                        &ret.usual_itinerary,
                    )
                    .await
                    .map_err(|(status, message)| (status, format!("Return journey: {message}")))?,
                ),
                None => None,
            };
            let recurrence = regular_journeys::PlanRecurrence {
                days_of_week,
                starts_on,
                ends_on,
                default_match_mode,
            };
            let outcome = regular_journeys::create_plan_templates(
                &app.database,
                &user.id,
                &recurrence,
                &outbound,
                return_input.as_ref(),
            )
            .await
            .map_err(internal_error("create regular journey"))?;
            match outcome {
                regular_journeys::CreatePlanOutcome::Created { id, return_id } => {
                    Ok(Json(CreateJourneyTemplateResponse {
                        template_id: id,
                        return_template_id: return_id,
                    }))
                }
                regular_journeys::CreatePlanOutcome::OverCap => Err((
                    StatusCode::BAD_REQUEST,
                    format!(
                        "You can have at most {} regular journeys, and a return journey counts \
                         as one. Delete one to make room.",
                        journey_templates::MAX_JOURNEY_TEMPLATES_PER_USER
                    ),
                )),
            }
        }
    }
}

fn bad_request(message: String) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message)
}

/// Validates one template of a plan-mode create: the name, the stations
/// (different from each other), the target, the plan options and the usual
/// itinerary, whose trains must call at their stations on the date they
/// were planned for (`422` otherwise, as `POST /Journeys/itinerary`).
async fn plan_template_input(
    app: &App,
    custom_name: Option<&str>,
    origin_crs: &str,
    destination_crs: &str,
    target: TemplateTarget,
    plan_options: Option<PlanOptions>,
    usual_itinerary: &UsualItinerary,
) -> Result<regular_journeys::PlanTemplateInput, (StatusCode, String)> {
    let custom_name = train_tracking::validate_custom_name(custom_name).map_err(bad_request)?;
    let origin_crs = regular_journeys::normalize_crs(origin_crs, "origin").map_err(bad_request)?;
    let destination_crs =
        regular_journeys::normalize_crs(destination_crs, "destination").map_err(bad_request)?;
    if origin_crs == destination_crs {
        return Err(bad_request(
            "The origin and destination must be different stations.".to_string(),
        ));
    }
    regular_journeys::validate_target(&target).map_err(bad_request)?;
    if let Some(options) = &plan_options {
        regular_journeys::validate_plan_options(options).map_err(bad_request)?;
    }
    let usual_itinerary =
        regular_journeys::validate_usual_itinerary(usual_itinerary).map_err(bad_request)?;
    if let Some((leg, rejection)) =
        regular_journeys::check_itinerary_calls(&app.database, &usual_itinerary)
            .await
            .map_err(internal_error("check the usual train's calling points"))?
    {
        let message = if usual_itinerary.legs.len() > 1 {
            format!("Leg {leg}: {}", rejection.message)
        } else {
            rejection.message
        };
        return Err((StatusCode::UNPROCESSABLE_ENTITY, message));
    }
    Ok(regular_journeys::PlanTemplateInput {
        custom_name,
        origin_crs,
        destination_crs,
        target,
        plan_options,
        usual_itinerary,
    })
}

async fn get_my_journey_templates(
    State(app): State<App>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<journey_templates::JourneyTemplateListItem>>, (StatusCode, String)> {
    let rows = journey_templates::list_templates_for_user(
        &app.database,
        &user.id,
        super::london_now().to_utc(),
    )
    .await
    .map_err(internal_error("list journey templates"))?;
    Ok(Json(rows))
}

async fn get_journey_template(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(template_id): Path<i64>,
) -> Result<Json<JourneyTemplateDetailResponse>, (StatusCode, String)> {
    let template = journey_templates::get_owned_template(&app.database, template_id, &user.id)
        .await
        .map_err(internal_error("read journey template"))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "no journey template with that id".to_string(),
        ))?;
    let legs = journey_templates::list_template_legs(&app.database, template_id)
        .await
        .map_err(internal_error("list journey template legs"))?;
    let now = super::london_now().to_utc();
    let next_date = regular_journeys::next_dates(&app.database, &[template_id], now)
        .await
        .map_err(internal_error("read the next date"))?
        .get(&template_id)
        .copied()
        .flatten();
    let skipped_dates = regular_journeys::skipped_dates_from(
        &app.database,
        template_id,
        regular_journeys::london_date(now),
    )
    .await
    .map_err(internal_error("read skipped dates"))?;
    let target = template.target();

    Ok(Json(JourneyTemplateDetailResponse {
        origin_crs: template.origin_crs,
        origin_name: template.origin_name,
        destination_crs: template.destination_crs,
        destination_name: template.destination_name,
        target,
        plan_options: regular_journeys::decode_stored(template.plan_options),
        usual_itinerary: regular_journeys::decode_stored(template.usual_itinerary),
        paired_template_id: template.paired_template_id,
        next_date,
        skipped_dates,
        id: template.id,
        custom_name: template.custom_name,
        created_at: template.created_at,
        updated_at: template.updated_at,
        days_of_week: template.days_of_week,
        active: template.active,
        starts_on: template.starts_on,
        ends_on: template.ends_on,
        default_match_mode: template.default_match_mode,
        auto_commit_rule: template.auto_commit_rule,
        legs: legs
            .into_iter()
            .map(|leg| JourneyTemplateLegDetailResponse {
                id: leg.id,
                origin_crs: leg.origin_crs,
                origin_name: leg.origin_name,
                destination_crs: leg.destination_crs,
                destination_name: leg.destination_name,
                depart_after: leg.depart_after,
                depart_before: leg.depart_before,
                arrive_after: leg.arrive_after,
                arrive_before: leg.arrive_before,
            })
            .collect(),
    }))
}

async fn put_journey_template(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(template_id): Path<i64>,
    Json(body): Json<PutJourneyTemplateRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    if body.legs.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "A template needs at least one leg.".to_string(),
        ));
    }
    journey_templates::validate_leg_count(body.legs.len())
        .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
    journey_templates::validate_template_recurrence(
        &body.default_match_mode,
        body.auto_commit_rule.as_deref(),
        body.days_of_week,
        body.starts_on,
        body.ends_on,
    )
    .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
    // Same custom_name cap as `post_journey_template`'s own arms -- see
    // that fix's doc comment.
    let custom_name = train_tracking::validate_custom_name(body.custom_name.as_deref())
        .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
    let leg_inputs = body
        .legs
        .into_iter()
        .map(to_template_leg_input)
        .collect::<Result<Vec<_>, _>>()?;
    journey_templates::validate_auto_mode_legs(&body.default_match_mode, &leg_inputs)
        .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
    let target_update = put_target_update(
        &app,
        template_id,
        &user.id,
        body.origin_crs,
        body.destination_crs,
        body.target,
        body.plan_options,
        body.usual_itinerary,
    )
    .await?;
    let replaced = journey_templates::replace_template(
        &app.database,
        template_id,
        &user.id,
        custom_name.as_deref(),
        &leg_inputs,
        body.days_of_week,
        body.active,
        body.starts_on,
        body.ends_on,
        &body.default_match_mode,
        body.auto_commit_rule.as_deref(),
        &target_update,
    )
    .await
    .map_err(internal_error("replace journey template"))?;
    if !replaced {
        return Err((
            StatusCode::NOT_FOUND,
            "no journey template with that id".to_string(),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The regular-journey fields of a PUT, validated: each absent one keeps
/// its stored value. A target needs both stations unless the template
/// already has a target, and options or a usual itinerary need a target,
/// given or stored. `404` when the template isn't the caller's.
#[expect(
    clippy::too_many_arguments,
    reason = "the PUT body's five optional fields, moved in one by one"
)]
async fn put_target_update(
    app: &App,
    template_id: i64,
    user_id: &str,
    origin_crs: Option<String>,
    destination_crs: Option<String>,
    target: Option<TemplateTarget>,
    plan_options: Option<PlanOptions>,
    usual_itinerary: Option<UsualItinerary>,
) -> Result<journey_templates::TargetUpdate, (StatusCode, String)> {
    if origin_crs.is_none()
        && destination_crs.is_none()
        && target.is_none()
        && plan_options.is_none()
        && usual_itinerary.is_none()
    {
        return Ok(journey_templates::TargetUpdate::default());
    }
    let stored = journey_templates::get_owned_template(&app.database, template_id, user_id)
        .await
        .map_err(internal_error("read journey template"))?
        .ok_or((
            StatusCode::NOT_FOUND,
            "no journey template with that id".to_string(),
        ))?;
    let has_target = stored.target_kind.is_some();
    let origin_crs = origin_crs
        .map(|crs| regular_journeys::normalize_crs(&crs, "origin"))
        .transpose()
        .map_err(bad_request)?;
    let destination_crs = destination_crs
        .map(|crs| regular_journeys::normalize_crs(&crs, "destination"))
        .transpose()
        .map_err(bad_request)?;
    if let Some(target) = &target {
        regular_journeys::validate_target(target).map_err(bad_request)?;
    }
    let gets_target = has_target || target.is_some();
    if !gets_target && (origin_crs.is_some() || destination_crs.is_some()) {
        return Err(bad_request(
            "originCrs and destinationCrs need a target.".to_string(),
        ));
    }
    if !has_target && target.is_some() && (origin_crs.is_none() || destination_crs.is_none()) {
        return Err(bad_request(
            "A target needs an originCrs and a destinationCrs.".to_string(),
        ));
    }
    if !gets_target && (plan_options.is_some() || usual_itinerary.is_some()) {
        return Err(bad_request(
            "planOptions and usualItinerary need a target.".to_string(),
        ));
    }
    let origin = origin_crs.clone().or(stored.origin_crs);
    let destination = destination_crs.clone().or(stored.destination_crs);
    if origin.is_some() && origin == destination {
        return Err(bad_request(
            "The origin and destination must be different stations.".to_string(),
        ));
    }
    if let Some(options) = &plan_options {
        regular_journeys::validate_plan_options(options).map_err(bad_request)?;
    }
    let usual_itinerary = match usual_itinerary {
        Some(itinerary) => {
            let itinerary =
                regular_journeys::validate_usual_itinerary(&itinerary).map_err(bad_request)?;
            if let Some((leg, rejection)) =
                regular_journeys::check_itinerary_calls(&app.database, &itinerary)
                    .await
                    .map_err(internal_error("check the usual train's calling points"))?
            {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    format!("Leg {leg}: {}", rejection.message),
                ));
            }
            Some(itinerary)
        }
        None => None,
    };
    Ok(journey_templates::TargetUpdate {
        origin_crs,
        destination_crs,
        target,
        plan_options,
        usual_itinerary,
    })
}

async fn delete_journey_template(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(template_id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    let deleted = journey_templates::delete_template(&app.database, template_id, &user.id)
        .await
        .map_err(internal_error("delete journey template"))?;
    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            "no journey template with that id".to_string(),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /JourneyTemplates/{id}/materialize` -- the manual "Run now"
/// trigger. Literally `data::journey_templates::materialize_template`
/// called synchronously from this handler, no scheduler involved -- see
/// that function's own doc comment, and this plan's closing section for
/// exactly this signature's reuse contract for Phase C.
async fn post_materialize_journey_template(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(template_id): Path<i64>,
    Json(body): Json<MaterializeTemplateRequest>,
) -> Result<Json<MaterializeTemplateResponse>, (StatusCode, String)> {
    // The pin's 28-day horizon and per-user cap, as for a `window` leg
    // added by hand: the minted legs are unmatched until a train is picked.
    crate::routes::train::enforce_tracking_horizon(body.service_date)?;
    crate::routes::train::enforce_pin_cap(&app, &user.id).await?;
    let result = journey_templates::materialize_template(
        &app.database,
        template_id,
        &user.id,
        body.service_date,
    )
    .await
    .map_err(internal_error("materialize journey template"))?;
    let Some(materialized) = result else {
        return Err((
            StatusCode::NOT_FOUND,
            "no journey template with that id".to_string(),
        ));
    };
    Ok(Json(MaterializeTemplateResponse {
        journey_id: materialized.journey_id,
        leg_ids: materialized.leg_ids,
    }))
}

/// `PUT /JourneyTemplates/{id}/active` `{active}`: pauses or resumes the
/// template and its pair (`regular_journeys::set_active`). Pausing also
/// deletes their untouched journeys from today on that haven't started.
async fn put_journey_template_active(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(template_id): Path<i64>,
    Json(body): Json<SetTemplateActiveRequest>,
) -> Result<Json<SetTemplateActiveResponse>, (StatusCode, String)> {
    let outcome = regular_journeys::set_active(
        &app.database,
        template_id,
        &user.id,
        body.active,
        super::london_now().to_utc(),
    )
    .await
    .map_err(internal_error("pause or resume journey template"))?
    .ok_or((
        StatusCode::NOT_FOUND,
        "no journey template with that id".to_string(),
    ))?;
    Ok(Json(SetTemplateActiveResponse {
        template_ids: outcome.template_ids,
        deleted_journey_ids: outcome.deleted_journey_ids,
    }))
}

/// How far ahead a date may be skipped.
const MAX_SKIP_DAYS_AHEAD: i64 = 366;

/// `POST /JourneyTemplates/{id}/skip` `{serviceDate?, pair?}`: skips a date
/// (default: `nextDate`), for the pair too with `pair: true`
/// (`regular_journeys::skip`). `409` once that date's journey has started.
async fn post_journey_template_skip(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(template_id): Path<i64>,
    body: Option<Json<SkipTemplateDateRequest>>,
) -> Result<Json<SkipTemplateDateResponse>, (StatusCode, String)> {
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let now = super::london_now().to_utc();
    if let Some(date) = body.service_date
        && date > regular_journeys::london_date(now) + chrono::Duration::days(MAX_SKIP_DAYS_AHEAD)
    {
        return Err(bad_request(
            "You can skip a date up to a year ahead.".to_string(),
        ));
    }
    let outcome = regular_journeys::skip(
        &app.database,
        template_id,
        &user.id,
        body.service_date,
        body.pair.unwrap_or(false),
        now,
    )
    .await
    .map_err(internal_error("skip a journey template date"))?;
    match outcome {
        regular_journeys::SkipOutcome::Skipped {
            date,
            template_ids,
            deleted_journey_ids,
        } => Ok(Json(SkipTemplateDateResponse {
            service_date: date,
            template_ids,
            deleted_journey_ids,
        })),
        regular_journeys::SkipOutcome::NotFound => Err((
            StatusCode::NOT_FOUND,
            "no journey template with that id".to_string(),
        )),
        regular_journeys::SkipOutcome::NoNextDate => Err((
            StatusCode::CONFLICT,
            "This regular journey has no date to skip.".to_string(),
        )),
        regular_journeys::SkipOutcome::InThePast => {
            Err(bad_request("That date has already passed.".to_string()))
        }
        regular_journeys::SkipOutcome::Started => Err((
            StatusCode::CONFLICT,
            "That journey has already started.".to_string(),
        )),
    }
}

/// `DELETE /JourneyTemplates/{id}/skip/{date}`: undoes a skip. The next
/// sweep adds the journey again if it is still due.
async fn delete_journey_template_skip(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path((template_id, service_date)): Path<(i64, NaiveDate)>,
) -> Result<StatusCode, (StatusCode, String)> {
    let owned = regular_journeys::unskip(&app.database, template_id, &user.id, service_date)
        .await
        .map_err(internal_error("undo a skipped date"))?;
    if !owned {
        return Err((
            StatusCode::NOT_FOUND,
            "no journey template with that id".to_string(),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /JourneyTemplates/{id}/pair`: unlinks the template and its
/// return journey, both sides. `204` also when it had no pair.
async fn delete_journey_template_pair(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(template_id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    let owned = regular_journeys::unpair(&app.database, template_id, &user.id)
        .await
        .map_err(internal_error("unlink a journey template pair"))?;
    if !owned {
        return Err((
            StatusCode::NOT_FOUND,
            "no journey template with that id".to_string(),
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod wire_format_tests {
    use super::{CreateJourneyTemplateRequest, PutJourneyTemplateRequest};

    #[test]
    fn manual_mode_deserializes_its_camel_case_fields() {
        let request: CreateJourneyTemplateRequest = serde_json::from_str(
            r#"{
                "mode": "manual",
                "customName": "Weekday commute",
                "legs": [
                    {"originCrs": "WAT", "destinationCrs": "RDG",
                     "departWindow": {"after": "08:00:00"}}
                ]
            }"#,
        )
        .expect("valid manual-mode request should deserialize");
        assert!(matches!(
            request,
            CreateJourneyTemplateRequest::Manual { .. }
        ));
    }

    #[test]
    fn from_journey_mode_deserializes_its_camel_case_fields() {
        let request: CreateJourneyTemplateRequest =
            serde_json::from_str(r#"{"mode": "fromJourney", "journeyId": 42}"#)
                .expect("valid fromJourney-mode request should deserialize");
        assert!(matches!(
            request,
            CreateJourneyTemplateRequest::FromJourney { .. }
        ));
    }

    #[test]
    fn put_request_deserializes_its_camel_case_fields() {
        let request: PutJourneyTemplateRequest = serde_json::from_str(
            r#"{"customName": "Renamed",
                "legs": [{"originCrs": "WAT", "destinationCrs": "RDG"}],
                "daysOfWeek": null,
                "active": true,
                "startsOn": null,
                "endsOn": null,
                "defaultMatchMode": "manual",
                "autoCommitRule": null}"#,
        )
        .expect("valid PUT request should deserialize");
        assert_eq!(request.legs.len(), 1);
    }
}

/// Scaffolding copied verbatim from `routes::journeys::db_tests` (same
/// cross-file duplication convention that module's own doc comment
/// describes) -- `test_app`/`test_router`/`seed_session`/`connect`/
/// `request`/`post_json`/`delete_request` are byte-for-byte identical to
/// that module's own copies, plus a `put_json` helper mirroring `post_json`
/// with `method("PUT")`. `cleanup_user` is the one adaptation: this module
/// additionally seeds/mutates `journey_templates`/`journey_template_legs`
/// rows (neither of which `routes::journeys::db_tests::cleanup_user` knows
/// about), so this copy also clears those, same FK-respecting order
/// `data::journey_templates::db_tests::cleanup_user` already established.
#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    reason = "test code: casts of small known test values"
)]
mod db_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use serde_json::Value;
    use sqlx::PgPool;
    use tower::ServiceExt;

    use crate::app::{App, AppState};
    use crate::auth::hash_session_token;
    use crate::auth::oidc::{OidcClient, OidcConfig};
    use crate::data::config::{LineCatalogue, ServiceArguments};
    use crate::data::users::insert_session;

    /// Every `ServiceArguments` field filled with an inert placeholder --
    /// this file's routes don't read `config.lines` at all. Copied from
    /// `routes::journeys::db_tests::test_app`'s own empty-index default.
    fn test_app(pool: PgPool) -> App {
        let config = ServiceArguments {
            bind_url: "0.0.0.0:0".to_string(),
            database_url: String::new(),
            migration_database_url: None,
            internal_oauth_issuer_url: "https://example.invalid".to_string(),
            internal_oauth_client_id: "test-internal-oauth-client".to_string(),
            internal_oauth_group_mcp: "srv-ds-mcp".to_string(),
            transit_networks: Vec::new(),
            source_images_file: None,
            admin_group: String::new(),
            sso_issuer_url: "https://example.invalid".to_string(),
            sso_client_id: "test-client".to_string(),
            sso_client_secret: "test-secret".to_string(),
            sso_redirect_url: "https://example.invalid/callback".to_string(),
            sso_post_login_redirect_url: "https://example.invalid/".to_string(),
            sso_discord_id_claim: String::new(),
            sso_primary_sub_claim: String::new(),
            session_ttl_days: 14,
            session_max_age_days: 90,
            session_extend_throttle_hours: 24,
            history_retention_days: 7,
            daily_stats_retention_days: 300,
            half_hourly_stats_retention_hours: 840,
            metrics_enabled: false,
            metrics_port: 9091,
            defaults_file: None,
            lines: LineCatalogue(vec![]),
            vapid_public_key: "test-vapid-public-key".to_string(),
            full_coverage_enabled_default: false,
            schedule_match_interval_secs: 300,
            reconciliation_sweep_interval_secs: 300,
            schedule_enrichment_grace_minutes: 30,
            backlog_match_sweep_interval_secs: 300,
            session_cleanup_interval_secs: 3600,
            background_loops: true,
            past_travel_retention_days: 548,
            regular_journey_retention_days: 0,
            stale_push_subscription_days: 365,
            inactive_account_retention_days: 0,
        };

        std::sync::Arc::new(AppState {
            line_matcher: common::matcher::LineMatcher::new(&config.lines),
            config,
            database: pool,
            oidc: OidcClient::new(OidcConfig {
                issuer_url: "https://example.invalid".to_string(),
                client_id: "test-client".to_string(),
                client_secret: "test-secret".to_string(),
                redirect_url: "https://example.invalid/callback".to_string(),
            })
            .expect("construct placeholder oidc client"),
            internal_oauth_verifier: crate::auth::internal_oauth::ServiceTokenVerifier::new(
                "https://example.invalid".to_string(),
                "test-internal-oauth-client".to_string(),
            )
            .expect("construct placeholder internal-oauth verifier"),
            delegated: std::sync::Arc::default(),
            tfl_arrivals: std::sync::Arc::default(),
            personal_tokens: std::sync::Arc::default(),
            schedule_crs_line_index: std::collections::HashMap::new(),
        })
    }

    /// The real `journey_templates::router()`, mounted unprefixed exactly as
    /// `main.rs` does, turned into a `tower::Service` a test can drive with
    /// `.oneshot(..)`.
    fn test_router(app: App) -> axum::Router {
        crate::app::Router::new()
            .merge(super::router())
            .with_state(app)
    }

    /// Same as `test_router`, but also merges `routes::journeys::router()`
    /// into the same router -- used by
    /// `post_materialize_journey_template_mints_a_journey_with_unmatched_legs`
    /// so that test can assert against the real downstream
    /// `GET /Journeys/{id}` shape rather than reimplementing it.
    fn test_router_with_journeys(app: App) -> axum::Router {
        crate::app::Router::new()
            .merge(super::router())
            .merge(crate::routes::journeys::router())
            .with_state(app)
    }

    /// Seeds a real, resolvable session for `user_id` (creating the user if
    /// it doesn't already exist) and returns the *raw* token -- send it as
    /// `Cookie: distant_signal_session=<raw>`, never the hash `sessions`
    /// actually stores.
    async fn seed_session(pool: &PgPool, user_id: &str) -> String {
        sqlx::query("INSERT INTO users (id, name) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING")
            .bind(user_id)
            .bind(user_id)
            .execute(pool)
            .await
            .expect("seed fixture user");

        let raw_token = format!("test-raw-session-token-for-{user_id}");
        insert_session(pool, &hash_session_token(&raw_token), user_id, 14)
            .await
            .expect("seed fixture session");
        raw_token
    }

    /// Deletes a fixture user and its fixtures. Same FK-respecting order as
    /// `routes::journeys::db_tests::cleanup_user`, plus this module's own
    /// `journey_templates`/`journey_template_legs` rows -- see this module's
    /// own doc comment above for why.
    async fn cleanup_user(pool: &PgPool, user_id: &str) {
        sqlx::query(
            "DELETE FROM journey_template_legs WHERE template_id IN \
                (SELECT id FROM journey_templates WHERE user_id = $1)",
        )
        .bind(user_id)
        .execute(pool)
        .await
        .expect("cleanup fixture journey_template_legs rows");
        sqlx::query("DELETE FROM journey_templates WHERE user_id = $1")
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup fixture journey_templates rows");
        sqlx::query(
            "DELETE FROM journey_legs WHERE journey_id IN \
                (SELECT id FROM journeys WHERE user_id = $1)",
        )
        .bind(user_id)
        .execute(pool)
        .await
        .expect("cleanup fixture journey_legs rows");
        sqlx::query("DELETE FROM journeys WHERE user_id = $1")
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup fixture journeys rows");
        sqlx::query("DELETE FROM train_subscriptions WHERE user_id = $1")
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup fixture tracked_trains rows");
        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(user_id)
            .execute(pool)
            .await
            .expect("cleanup fixture user");
    }

    async fn connect() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
        sqlx::postgres::PgPoolOptions::new()
            .connect(&database_url)
            .await
            .expect("connect to postgres")
    }

    /// Issues a GET against `router`, optionally with a session cookie, and
    /// returns `(status, parsed JSON body)`. Every route in this file
    /// returns either a JSON object body or a plain-text `(StatusCode,
    /// String)` error body, so wrapping the latter as a JSON string lets
    /// every case share one return shape.
    async fn request(
        router: axum::Router,
        uri: String,
        raw_token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().uri(uri);
        if let Some(token) = raw_token {
            builder = builder.header(header::COOKIE, format!("distant_signal_session={token}"));
        }
        let req = builder.body(Body::empty()).expect("build request");
        let response = router.oneshot(req).await.expect("oneshot request");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            Value::String(String::from_utf8(bytes.to_vec()).expect("body is valid utf8"))
        });
        (status, value)
    }

    /// Issues a `POST` with a JSON body against `router`, optionally with a
    /// session cookie -- the write-path counterpart to `request` above
    /// (which only ever issues an empty-body `GET`). Same shared
    /// `(status, parsed body)` return shape, same plain-text-becomes-`Value::String`
    /// handling for a `(StatusCode, String)` error response.
    async fn post_json(
        router: axum::Router,
        uri: String,
        raw_token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .uri(uri)
            .method("POST")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = raw_token {
            builder = builder.header(header::COOKIE, format!("distant_signal_session={token}"));
        }
        let req = builder
            .body(Body::from(
                serde_json::to_vec(&body).expect("serialize request body"),
            ))
            .expect("build request");
        let response = router.oneshot(req).await.expect("oneshot request");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            Value::String(String::from_utf8(bytes.to_vec()).expect("body is valid utf8"))
        });
        (status, value)
    }

    /// Issues a `PUT` with a JSON body against `router`, optionally with a
    /// session cookie -- mirrors `post_json` exactly, `method("PUT")` swapped
    /// in for `method("POST")`.
    async fn put_json(
        router: axum::Router,
        uri: String,
        raw_token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .uri(uri)
            .method("PUT")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = raw_token {
            builder = builder.header(header::COOKIE, format!("distant_signal_session={token}"));
        }
        let req = builder
            .body(Body::from(
                serde_json::to_vec(&body).expect("serialize request body"),
            ))
            .expect("build request");
        let response = router.oneshot(req).await.expect("oneshot request");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            Value::String(String::from_utf8(bytes.to_vec()).expect("body is valid utf8"))
        });
        (status, value)
    }

    /// Issues a `DELETE` against `router`, optionally with a session
    /// cookie -- mirrors `routes::journeys::db_tests::delete_request`
    /// exactly (same empty-body-becomes-`Value::Null` handling: a success
    /// here is `204 No Content` with no body, so feeding that straight to
    /// `serde_json::from_slice` would fail to parse and mask a real body on
    /// any OTHER status).
    async fn delete_request(
        router: axum::Router,
        uri: String,
        raw_token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method("DELETE").uri(uri);
        if let Some(token) = raw_token {
            builder = builder.header(header::COOKIE, format!("distant_signal_session={token}"));
        }
        let req = builder.body(Body::empty()).expect("build request");
        let response = router.oneshot(req).await.expect("oneshot request");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                Value::String(String::from_utf8(bytes.to_vec()).expect("body is valid utf8"))
            })
        };
        (status, value)
    }

    #[test]
    fn router_builds_without_panicking() {
        let _ = super::router();
    }

    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                post_journey_template_manual_mode_creates_a_template -- --ignored --test-threads=1`"]
    async fn post_journey_template_manual_mode_creates_a_template() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-CREATE";
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));

        let (status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({
                "mode": "manual",
                "customName": "Weekday commute",
                "legs": [
                    {"originCrs": "WAT", "destinationCrs": "RDG",
                     "departWindow": {"after": "08:00:00"}}
                ]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let template_id = created["templateId"].as_i64().expect("templateId present");

        let (status, body) = request(
            router,
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["customName"], "Weekday commute");
        let legs = body["legs"].as_array().expect("legs array");
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0]["originCrs"], "WAT");
        assert_eq!(legs[0]["destinationCrs"], "RDG");

        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                post_journey_template_manual_mode_an_empty_legs_array_is_400 -- --ignored --test-threads=1`"]
    async fn post_journey_template_manual_mode_an_empty_legs_array_is_400() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-EMPTY-LEGS";
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));

        let (status, _body) = post_json(
            router,
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({
                "mode": "manual",
                "legs": []
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        cleanup_user(&pool, user_id).await;
    }

    /// Low finding #6 (2026-09-25 review): before
    /// `journey_templates::validate_leg_count` existed, a `Manual`-mode
    /// `legs` array had no upper bound at all, so this would have been
    /// accepted and paid for as `MAX_LEGS_PER_TEMPLATE + 1` single-row
    /// inserts in one transaction. Pins down a clean `400` instead.
    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                post_journey_template_manual_mode_too_many_legs_is_400 -- --ignored --test-threads=1`"]
    async fn post_journey_template_manual_mode_too_many_legs_is_400() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-TOO-MANY-LEGS";
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));

        let legs: Vec<Value> = (0..=crate::data::journey_templates::MAX_LEGS_PER_TEMPLATE)
            .map(|_| serde_json::json!({ "originCrs": "WAT", "destinationCrs": "RDG" }))
            .collect();
        let (status, body) = post_json(
            router,
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({
                "mode": "manual",
                "legs": legs
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "over-cap legs: {body:?}");

        cleanup_user(&pool, user_id).await;
    }

    /// Low finding #2 (2026-09-25 review): before this route validated
    /// `customName` at all, a multi-KB name reached `create_template`
    /// unchanged and would have been stored and rendered to every group
    /// member. Pins down that the same `common::CUSTOM_NAME_MAX_LENGTH`
    /// cap every other `custom_name` write path in this codebase already
    /// enforces (`train_tracking::validate_custom_name`) is now applied
    /// here too.
    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                post_journey_template_manual_mode_an_oversized_custom_name_is_400 -- --ignored \
                --test-threads=1`"]
    async fn post_journey_template_manual_mode_an_oversized_custom_name_is_400() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-LONG-NAME";
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));

        let oversized_name = "a".repeat(common::CUSTOM_NAME_MAX_LENGTH + 1);
        let (status, body) = post_json(
            router,
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({
                "mode": "manual",
                "customName": oversized_name,
                "legs": [{ "originCrs": "WAT", "destinationCrs": "RDG" }]
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "oversized name: {body:?}");

        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                post_journey_template_from_journey_promotes_an_owned_journey -- --ignored --test-threads=1`"]
    async fn post_journey_template_from_journey_promotes_an_owned_journey() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-PROMOTE";
        let token = seed_session(&pool, user_id).await;

        let (journey_id, _leg_id) = crate::data::journeys::create_journey_with_window_leg(
            &pool,
            user_id,
            Some("Source journey"),
            "WAT",
            "RDG",
            "2026-09-22".parse().unwrap(),
            common::TimeWindow {
                after: Some("08:00:00".parse().unwrap()),
                before: None,
            },
            common::TimeWindow::default(),
        )
        .await
        .expect("create source journey");

        let router = test_router(test_app(pool.clone()));
        let (status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({
                "mode": "fromJourney",
                "customName": "Promoted",
                "journeyId": journey_id
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let template_id = created["templateId"].as_i64().expect("templateId present");

        let (status, body) = request(
            router,
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let legs = body["legs"].as_array().expect("legs array");
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0]["originCrs"], "WAT");
        assert_eq!(legs[0]["destinationCrs"], "RDG");
        assert_eq!(legs[0]["departAfter"], "08:00:00");

        cleanup_user(&pool, user_id).await;
    }

    /// Spec 2026-10-20 §2.4: a template keeps a journey's train legs and
    /// skips its untracked ones; a journey of untracked legs only has
    /// nothing to save.
    #[tokio::test]
    #[ignore = "requires a live database: DATABASE_URL=... cargo test -p api \
                post_journey_template_from_journey_skips_untracked_legs -- --ignored --test-threads=1"]
    async fn post_journey_template_from_journey_skips_untracked_legs() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-UNTRACKED";
        let token = seed_session(&pool, user_id).await;
        let now = chrono::Utc::now();
        let tube = crate::data::journeys::validate_untracked_leg(
            crate::data::journeys::UntrackedLegInput {
                transit_mode: "tube",
                origin_name: Some("Brixton"),
                destination_name: Some("Waterloo"),
                line_name: None,
                planned_departure: Some(now),
                planned_arrival: Some(now + chrono::Duration::minutes(20)),
            },
            crate::routes::london_today(),
        )
        .unwrap();
        let (tube_only, _) =
            crate::data::journeys::create_journey_with_untracked_leg(&pool, user_id, None, &tube)
                .await
                .unwrap();
        let (mixed, _) =
            crate::data::journeys::create_journey_with_untracked_leg(&pool, user_id, None, &tube)
                .await
                .unwrap();
        crate::data::journeys::add_window_leg_to_journey(
            &pool,
            mixed,
            user_id,
            "WAT",
            "RDG",
            "2026-09-22".parse().unwrap(),
            common::TimeWindow {
                after: Some("08:00:00".parse().unwrap()),
                before: None,
            },
            common::TimeWindow::default(),
        )
        .await
        .unwrap();

        let router = test_router(test_app(pool.clone()));
        let (status, body) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({ "mode": "fromJourney", "journeyId": tube_only }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            Value::String("This journey has no train legs to save as a template.".to_string())
        );

        let (status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({ "mode": "fromJourney", "journeyId": mixed }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created:?}");
        let template_id = created["templateId"].as_i64().unwrap();
        let (_, body) = request(
            router,
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
        )
        .await;
        let legs = body["legs"].as_array().unwrap();
        assert_eq!(legs.len(), 1, "the Tube leg is skipped: {body:?}");
        assert_eq!(legs[0]["originCrs"], "WAT");

        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                post_journey_template_from_journey_a_journey_owned_by_someone_else_is_404_not_403 -- --ignored --test-threads=1`"]
    async fn post_journey_template_from_journey_a_journey_owned_by_someone_else_is_404_not_403() {
        let pool = connect().await;
        let owner_id = "TEST-ROUTE-TEMPLATE-PROMOTE-OWNER";
        let bystander_id = "TEST-ROUTE-TEMPLATE-PROMOTE-BYSTANDER";
        let owner_token = seed_session(&pool, owner_id).await;
        let bystander_token = seed_session(&pool, bystander_id).await;

        let (journey_id, _leg_id) = crate::data::journeys::create_journey_with_window_leg(
            &pool,
            owner_id,
            None,
            "WAT",
            "RDG",
            "2026-09-22".parse().unwrap(),
            common::TimeWindow::default(),
            common::TimeWindow::default(),
        )
        .await
        .expect("create source journey");

        let router = test_router(test_app(pool.clone()));
        let (status, _body) = post_json(
            router,
            "/JourneyTemplates".to_string(),
            Some(&bystander_token),
            serde_json::json!({
                "mode": "fromJourney",
                "journeyId": journey_id
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let _ = owner_token;
        cleanup_user(&pool, owner_id).await;
        cleanup_user(&pool, bystander_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                get_journey_template_a_non_owner_gets_404 -- --ignored --test-threads=1`"]
    async fn get_journey_template_a_non_owner_gets_404() {
        let pool = connect().await;
        let owner_id = "TEST-ROUTE-TEMPLATE-GET-OWNER";
        let bystander_id = "TEST-ROUTE-TEMPLATE-GET-BYSTANDER";
        let owner_token = seed_session(&pool, owner_id).await;
        let bystander_token = seed_session(&pool, bystander_id).await;
        let router = test_router(test_app(pool.clone()));

        let (_status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&owner_token),
            serde_json::json!({
                "mode": "manual",
                "legs": [{"originCrs": "WAT", "destinationCrs": "RDG"}]
            }),
        )
        .await;
        let template_id = created["templateId"].as_i64().expect("templateId present");

        let (status, _body) = request(
            router,
            format!("/JourneyTemplates/{template_id}"),
            Some(&bystander_token),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        cleanup_user(&pool, owner_id).await;
        cleanup_user(&pool, bystander_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                put_journey_template_the_owner_can_replace_its_legs -- --ignored --test-threads=1`"]
    async fn put_journey_template_the_owner_can_replace_its_legs() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-PUT";
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));

        let (_status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({
                "mode": "manual",
                "customName": "Original",
                "legs": [{"originCrs": "WAT", "destinationCrs": "RDG"}]
            }),
        )
        .await;
        let template_id = created["templateId"].as_i64().expect("templateId present");

        let (status, _body) = put_json(
            router.clone(),
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
            serde_json::json!({
                "customName": "Renamed",
                "legs": [{"originCrs": "EUS", "destinationCrs": "MAN"}],
                "daysOfWeek": null,
                "active": true,
                "startsOn": null,
                "endsOn": null,
                "defaultMatchMode": "manual",
                "autoCommitRule": null
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, body) = request(
            router,
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["customName"], "Renamed");
        let legs = body["legs"].as_array().expect("legs array");
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0]["originCrs"], "EUS");
        assert_eq!(legs[0]["destinationCrs"], "MAN");

        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                put_journey_template_round_trips_every_recurrence_field -- --ignored --test-threads=1`"]
    async fn put_journey_template_round_trips_every_recurrence_field() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-PUT-RECURRENCE";
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));

        let (_status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({
                "mode": "manual",
                "customName": "Original",
                "legs": [{"originCrs": "WAT", "destinationCrs": "RDG"}]
            }),
        )
        .await;
        let template_id = created["templateId"].as_i64().expect("templateId present");

        let (status, _body) = put_json(
            router.clone(),
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
            serde_json::json!({
                "customName": "Weekday commute",
                "legs": [{
                    "originCrs": "EUS",
                    "destinationCrs": "MAN",
                    "departWindow": {"after": "07:30:00", "before": null}
                }],
                "daysOfWeek": 31,
                "active": false,
                "startsOn": "2026-10-01",
                "endsOn": "2026-12-31",
                "defaultMatchMode": "auto",
                "autoCommitRule": "nearest_to_now"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, body) = request(
            router,
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["daysOfWeek"], 31);
        assert_eq!(body["active"], false);
        assert_eq!(body["startsOn"], "2026-10-01");
        assert_eq!(body["endsOn"], "2026-12-31");
        assert_eq!(body["defaultMatchMode"], "auto");
        assert_eq!(body["autoCommitRule"], "nearest_to_now");

        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                put_journey_template_an_invalid_default_match_mode_is_400 -- --ignored --test-threads=1`"]
    async fn put_journey_template_an_invalid_default_match_mode_is_400() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-PUT-BAD-MODE";
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));

        let (_status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({
                "mode": "manual",
                "legs": [{"originCrs": "WAT", "destinationCrs": "RDG"}]
            }),
        )
        .await;
        let template_id = created["templateId"].as_i64().expect("templateId present");

        let (status, _body) = put_json(
            router,
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
            serde_json::json!({
                "legs": [{"originCrs": "WAT", "destinationCrs": "RDG"}],
                "daysOfWeek": null,
                "active": true,
                "startsOn": null,
                "endsOn": null,
                "defaultMatchMode": "sometimes",
                "autoCommitRule": null
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                delete_journey_template_the_owner_can_delete_it -- --ignored --test-threads=1`"]
    async fn delete_journey_template_the_owner_can_delete_it() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-DELETE";
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));

        let (_status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({
                "mode": "manual",
                "legs": [{"originCrs": "WAT", "destinationCrs": "RDG"}]
            }),
        )
        .await;
        let template_id = created["templateId"].as_i64().expect("templateId present");

        let (status, _body) = delete_request(
            router.clone(),
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _body) = request(
            router,
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                post_materialize_journey_template_mints_a_journey_with_unmatched_legs -- --ignored --test-threads=1`"]
    async fn post_materialize_journey_template_mints_a_journey_with_unmatched_legs() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-MATERIALIZE";
        let token = seed_session(&pool, user_id).await;
        let router = test_router_with_journeys(test_app(pool.clone()));

        let (_status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({
                "mode": "manual",
                "legs": [
                    {"originCrs": "WAT", "destinationCrs": "RDG"},
                    {"originCrs": "RDG", "destinationCrs": "BRI"}
                ]
            }),
        )
        .await;
        let template_id = created["templateId"].as_i64().expect("templateId present");

        let (status, materialized) = post_json(
            router.clone(),
            format!("/JourneyTemplates/{template_id}/materialize"),
            Some(&token),
            serde_json::json!({ "serviceDate": "2026-10-01" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let journey_id = materialized["journeyId"]
            .as_i64()
            .expect("journeyId present");
        assert_eq!(
            materialized["legIds"]
                .as_array()
                .expect("legIds array")
                .len(),
            2
        );

        let (status, body) = request(router, format!("/Journeys/{journey_id}"), Some(&token)).await;
        assert_eq!(status, StatusCode::OK);
        let legs = body["legs"].as_array().expect("legs array");
        assert_eq!(legs.len(), 2);
        for leg in legs {
            assert_eq!(leg["matchMode"], "unmatched");
            assert_eq!(leg["serviceDate"], "2026-10-01");
        }

        sqlx::query("DELETE FROM journey_legs WHERE journey_id = $1")
            .bind(journey_id)
            .execute(&pool)
            .await
            .expect("cleanup materialized journey_legs");
        sqlx::query("DELETE FROM journeys WHERE id = $1")
            .bind(journey_id)
            .execute(&pool)
            .await
            .expect("cleanup materialized journey");
        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; see this plan's Global Constraints for the \
                DATABASE_URL incantation, then run with `cargo test -p api \
                post_materialize_journey_template_a_non_owner_gets_404 -- --ignored --test-threads=1`"]
    async fn post_materialize_journey_template_a_non_owner_gets_404() {
        let pool = connect().await;
        let owner_id = "TEST-ROUTE-TEMPLATE-MAT-OWNER";
        let bystander_id = "TEST-ROUTE-TEMPLATE-MAT-BYSTANDER";
        let owner_token = seed_session(&pool, owner_id).await;
        let bystander_token = seed_session(&pool, bystander_id).await;
        let router = test_router(test_app(pool.clone()));

        let (_status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&owner_token),
            serde_json::json!({
                "mode": "manual",
                "legs": [{"originCrs": "WAT", "destinationCrs": "RDG"}]
            }),
        )
        .await;
        let template_id = created["templateId"].as_i64().expect("templateId present");

        let (status, _body) = post_json(
            router,
            format!("/JourneyTemplates/{template_id}/materialize"),
            Some(&bystander_token),
            serde_json::json!({ "serviceDate": "2026-10-01" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        cleanup_user(&pool, owner_id).await;
        cleanup_user(&pool, bystander_id).await;
    }

    /// L10 (2026-09-26 review): `POST /JourneyTemplates` refuses a new
    /// template once the caller already has
    /// `MAX_JOURNEY_TEMPLATES_PER_USER`.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                post_journey_template_enforces_the_per_user_template_cap \
                -- --ignored --test-threads=1`"]
    async fn post_journey_template_enforces_the_per_user_template_cap() {
        use crate::data::journey_templates::MAX_JOURNEY_TEMPLATES_PER_USER;
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-CAP";
        cleanup_user(&pool, user_id).await;
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));
        let body = serde_json::json!({
            "mode": "manual",
            "legs": [
                {"originCrs": "WAT", "destinationCrs": "RDG",
                 "departWindow": {"after": "08:00:00"}}
            ]
        });

        sqlx::query(
            "INSERT INTO journey_templates (user_id) SELECT $1 FROM generate_series(1, $2::int)",
        )
        .bind(user_id)
        .bind((MAX_JOURNEY_TEMPLATES_PER_USER - 1) as i32)
        .execute(&pool)
        .await
        .expect("seed templates up to one below the cap");

        let (status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            body.clone(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the template landing ON the cap: {created:?}"
        );

        let (status, refused) =
            post_json(router, "/JourneyTemplates".to_string(), Some(&token), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{refused:?}");
        assert!(
            refused.as_str().is_some_and(|b| b.contains("maximum")),
            "{refused:?}"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM journey_templates WHERE user_id = $1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .expect("count templates");
        assert_eq!(count, MAX_JOURNEY_TEMPLATES_PER_USER);

        cleanup_user(&pool, user_id).await;
    }

    /// A leg made by tracking a train has no window; promoted, it takes one
    /// around the train's booked departure from the leg's own origin: the
    /// timetable's time when held, else the pin's.
    #[tokio::test]
    #[ignore = "requires a live database: DATABASE_URL=... cargo test -p api \
                post_journey_template_from_journey_derives_a_window_from_the_train -- --ignored --test-threads=1"]
    async fn post_journey_template_from_journey_derives_a_window_from_the_train() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-PROMOTE-TRAIN";
        cleanup_user(&pool, user_id).await;
        let token = seed_session(&pool, user_id).await;
        let date: chrono::NaiveDate = "2099-01-15".parse().unwrap();
        sqlx::query("DELETE FROM schedule_destination_departures WHERE train_uid = 'TPROMO1'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM trains WHERE train_uid = 'TPROMO1'")
            .execute(&pool)
            .await
            .unwrap();
        // January: London is on UTC, so 07:00Z is 07:00 local.
        let (trains_id,): (i64,) = sqlx::query_as(
            "INSERT INTO trains (train_uid, service_date, origin_crs, destination_crs, \
                                 scheduled_departure) \
             VALUES ('TPROMO1', $1, 'ZTA', 'ZTC', '2099-01-15T07:00:00Z') RETURNING id",
        )
        .bind(date)
        .fetch_one(&pool)
        .await
        .unwrap();
        // The timetable has it leaving ZTB, mid-route, at 07:20.
        sqlx::query(
            "INSERT INTO schedule_destination_departures \
                 (service_date, destination_crs, scheduled, train_uid, origin_crs, public_departure) \
             VALUES ($1, 'ZTC', '07:20', 'TPROMO1', 'ZTB', '07:20')",
        )
        .bind(date)
        .execute(&pool)
        .await
        .unwrap();

        let router = test_router(test_app(pool.clone()));
        let mut windows = Vec::new();
        for origin in [None, Some("ZTB")] {
            let (journey_id, _, _) = crate::data::journeys::create_journey_with_known_train_leg(
                &pool, user_id, None, trains_id, date, origin, None,
            )
            .await
            .expect("track the train");
            let (status, created) = post_json(
                router.clone(),
                "/JourneyTemplates".to_string(),
                Some(&token),
                serde_json::json!({ "mode": "fromJourney", "journeyId": journey_id }),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{created:?}");
            let template_id = created["templateId"].as_i64().unwrap();
            let (_, body) = request(
                router.clone(),
                format!("/JourneyTemplates/{template_id}"),
                Some(&token),
            )
            .await;
            let leg = &body["legs"][0];
            windows.push((
                leg["departAfter"].clone(),
                leg["departBefore"].clone(),
                leg["arriveAfter"].clone(),
            ));
        }
        assert_eq!(
            windows,
            vec![
                (
                    Value::from("06:55:00"),
                    Value::from("07:05:00"),
                    Value::Null
                ),
                (
                    Value::from("07:15:00"),
                    Value::from("07:25:00"),
                    Value::Null
                ),
            ]
        );

        cleanup_user(&pool, user_id).await;
        sqlx::query("DELETE FROM schedule_destination_departures WHERE train_uid = 'TPROMO1'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM trains WHERE train_uid = 'TPROMO1'")
            .execute(&pool)
            .await
            .unwrap();
    }

    /// `"auto"` is refused while a leg has no time at all; `"manual"` is not.
    #[tokio::test]
    #[ignore = "requires a live database: DATABASE_URL=... cargo test -p api \
                put_journey_template_auto_needs_a_time_on_every_leg -- --ignored --test-threads=1"]
    async fn put_journey_template_auto_needs_a_time_on_every_leg() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-PUT-AUTO-OPEN";
        cleanup_user(&pool, user_id).await;
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));
        let (_status, created) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({
                "mode": "manual",
                "legs": [{"originCrs": "WAT", "destinationCrs": "RDG"}]
            }),
        )
        .await;
        let template_id = created["templateId"].as_i64().expect("templateId present");
        let put = |mode: &str, legs: Value| {
            serde_json::json!({
                "legs": legs,
                "daysOfWeek": 31,
                "active": true,
                "startsOn": null,
                "endsOn": null,
                "defaultMatchMode": mode,
                "autoCommitRule": if mode == "auto" { Value::from("nearest_to_now") } else { Value::Null }
            })
        };
        let open_and_timed = serde_json::json!([
            {"originCrs": "WAT", "destinationCrs": "RDG",
             "departWindow": {"after": "08:00:00", "before": null}},
            {"originCrs": "RDG", "destinationCrs": "OXF"}
        ]);

        let (status, body) = put_json(
            router.clone(),
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
            put("auto", open_and_timed.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            Value::String(crate::data::journey_templates::AUTO_NEEDS_TIMES_MESSAGE.to_string())
        );

        let (status, _) = put_json(
            router.clone(),
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
            put("manual", open_and_timed),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // An arrival-only bound is a time too.
        let (status, _) = put_json(
            router,
            format!("/JourneyTemplates/{template_id}"),
            Some(&token),
            put(
                "auto",
                serde_json::json!([
                    {"originCrs": "WAT", "destinationCrs": "RDG",
                     "arriveWindow": {"after": null, "before": "09:00:00"}}
                ]),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        cleanup_user(&pool, user_id).await;
    }

    /// The list serves each template's mode, next run date and whether a
    /// leg has no time.
    #[tokio::test]
    #[ignore = "requires a live database: DATABASE_URL=... cargo test -p api \
                get_my_journey_templates_serves_the_next_run_and_open_legs -- --ignored --test-threads=1"]
    async fn get_my_journey_templates_serves_the_next_run_and_open_legs() {
        let pool = connect().await;
        let user_id = "TEST-ROUTE-TEMPLATE-LIST-NEXT";
        cleanup_user(&pool, user_id).await;
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));
        let today = crate::routes::london_today();
        // Every day, starting tomorrow: the next run is tomorrow.
        let (active_id,): (i64,) = sqlx::query_as(
            "INSERT INTO journey_templates (user_id, custom_name, days_of_week, active, starts_on) \
             VALUES ($1, 'Every day', 127, TRUE, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(today + chrono::Duration::days(1))
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO journey_template_legs (template_id, leg_order, origin_crs, destination_crs) \
             VALUES ($1, 1, 'WAT', 'RDG')",
        )
        .bind(active_id)
        .execute(&pool)
        .await
        .unwrap();
        let (paused_id,): (i64,) = sqlx::query_as(
            "INSERT INTO journey_templates (user_id, custom_name, days_of_week, active, \
                                            default_match_mode, auto_commit_rule) \
             VALUES ($1, 'Paused', 127, FALSE, 'auto', 'nearest_to_now') RETURNING id",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO journey_template_legs \
                 (template_id, leg_order, origin_crs, destination_crs, depart_after) \
             VALUES ($1, 1, 'WAT', 'RDG', '08:00')",
        )
        .bind(paused_id)
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) =
            request(router, "/JourneyTemplates/mine".to_string(), Some(&token)).await;
        assert_eq!(status, StatusCode::OK);
        let rows = body.as_array().unwrap();
        let row = |id: i64| rows.iter().find(|r| r["id"] == id).unwrap().clone();
        let active = row(active_id);
        assert_eq!(
            active["nextRunOn"],
            Value::from((today + chrono::Duration::days(1)).to_string())
        );
        assert_eq!(active["hasOpenLegs"], true);
        assert_eq!(active["defaultMatchMode"], "manual");
        let paused = row(paused_id);
        assert_eq!(paused["nextRunOn"], Value::Null);
        assert_eq!(paused["hasOpenLegs"], false);
        assert_eq!(paused["defaultMatchMode"], "auto");

        cleanup_user(&pool, user_id).await;
    }

    // ---- Regular journeys, phase 1 ----

    /// A plan-mode body: Woking to London Waterloo, leave after 07:45 on
    /// weekdays, the 07:48 as the usual train, and with `with_return` the
    /// 17:35 back.
    fn plan_body(with_return: bool) -> Value {
        let mut body = serde_json::json!({
            "mode": "plan",
            "originCrs": "wok",
            "destinationCrs": "WAT",
            "target": {"kind": "depart_after", "time": "07:45"},
            "planOptions": {"maxChanges": 1},
            "usualItinerary": {"legs": [
                {"kind": "untracked", "transitMode": "walk", "originName": "Home", "minutes": 10},
                {"trainUid": "w12345", "originCrs": "WOK", "destinationCrs": "WAT",
                 "atocCode": "SW", "departs": "07:48", "arrives": "08:20"}
            ]},
            "daysOfWeek": 31
        });
        if with_return {
            body["return"] = serde_json::json!({
                "target": {"kind": "depart_after", "time": "17:30"},
                "usualItinerary": {"legs": [
                    {"trainUid": "W54321", "originCrs": "WAT", "destinationCrs": "WOK",
                     "departs": "17:35"}
                ]}
            });
        }
        body
    }

    async fn create_plan(
        router: axum::Router,
        token: &str,
        with_return: bool,
    ) -> (i64, Option<i64>) {
        let (status, body) = post_json(
            router,
            "/JourneyTemplates".to_string(),
            Some(token),
            plan_body(with_return),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (
            body["templateId"].as_i64().unwrap(),
            body["returnTemplateId"].as_i64(),
        )
    }

    /// How an occurrence seeded by [`seed_occurrence`] looks.
    #[derive(Clone, Copy)]
    enum Occurrence {
        /// One unmatched train leg.
        Untouched,
        /// A train leg the owner picked a train for.
        Picked,
        /// A sweep-committed train leg whose train has a ticket.
        Ticketed,
        /// An untracked first leg that set off at `instant`, then an
        /// unmatched train leg.
        StartedAt(chrono::DateTime<chrono::Utc>),
    }

    async fn seed_occurrence(
        pool: &PgPool,
        user_id: &str,
        template_id: i64,
        date: chrono::NaiveDate,
        shape: Occurrence,
    ) -> i64 {
        let (journey_id,): (i64,) = sqlx::query_as(
            "INSERT INTO journeys (user_id, source_template_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(template_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let mut order = 1;
        if let Occurrence::StartedAt(instant) = shape {
            sqlx::query(
                "INSERT INTO journey_legs (journey_id, leg_order, service_date, match_mode, kind, \
                     transit_mode, planned_departure, planned_arrival) \
                 VALUES ($1, 1, $2, 'manual', 'untracked', 'walk', $3, $3 + interval '10 minutes')",
            )
            .bind(journey_id)
            .bind(date)
            .bind(instant)
            .execute(pool)
            .await
            .unwrap();
            order = 2;
        }
        let (match_mode, subscription) = match shape {
            Occurrence::Untouched | Occurrence::StartedAt(_) => ("unmatched", None),
            Occurrence::Picked => ("manual", None),
            Occurrence::Ticketed => {
                let (id,): (i64,) = sqlx::query_as(
                    "INSERT INTO train_subscriptions (user_id, service_date, resolution_status) \
                     VALUES ($1, $2, 'pending') RETURNING id",
                )
                .bind(user_id)
                .bind(date)
                .fetch_one(pool)
                .await
                .unwrap();
                sqlx::query(
                    "INSERT INTO tracked_train_tickets (tracked_train_id, user_id) VALUES ($1, $2)",
                )
                .bind(id)
                .bind(user_id)
                .execute(pool)
                .await
                .unwrap();
                ("auto", Some(id))
            }
        };
        sqlx::query(
            "INSERT INTO journey_legs (journey_id, leg_order, origin_crs, destination_crs, \
                 service_date, match_mode, train_subscription_id, window_searched) \
             VALUES ($1, $2, 'WOK', 'WAT', $3, $4, $5, TRUE)",
        )
        .bind(journey_id)
        .bind(order)
        .bind(date)
        .bind(match_mode)
        .bind(subscription)
        .execute(pool)
        .await
        .unwrap();
        journey_id
    }

    async fn journey_exists(pool: &PgPool, journey_id: i64) -> bool {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM journeys WHERE id = $1)")
            .bind(journey_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn mine_row(rows: &Value, id: i64) -> Value {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap()
            .clone()
    }

    /// Tuesday 10 Nov 2026, 07:00 in London (GMT).
    fn tuesday_morning() -> chrono::DateTime<chrono::Utc> {
        "2026-11-10T07:00:00Z".parse().unwrap()
    }

    fn date(text: &str) -> chrono::NaiveDate {
        text.parse().unwrap()
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                plan_mode_creates_a_linked_pair -- --ignored --test-threads=1`"]
    async fn plan_mode_creates_a_linked_pair() {
        let pool = connect().await;
        let user_id = "test-regular-plan-pair";
        cleanup_user(&pool, user_id).await;
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));

        let (id, return_id) = create_plan(router.clone(), &token, true).await;
        let return_id = return_id.expect("a return template");

        let (status, detail) = request(
            router.clone(),
            format!("/JourneyTemplates/{id}"),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["originCrs"], "WOK");
        assert_eq!(detail["destinationCrs"], "WAT");
        assert_eq!(detail["target"]["kind"], "depart_after");
        assert_eq!(detail["target"]["time"], "07:45:00");
        assert_eq!(detail["pairedTemplateId"], return_id);
        assert_eq!(detail["defaultMatchMode"], "manual");
        assert_eq!(detail["daysOfWeek"], 31);
        assert_eq!(detail["planOptions"]["maxChanges"], 1);
        let usual = &detail["usualItinerary"]["legs"];
        assert_eq!(usual[0]["kind"], "untracked");
        assert_eq!(usual[1]["trainUid"], "W12345");
        assert!(usual[1].get("serviceDate").is_none());
        // One template leg, from the train leg, with a window around 07:48.
        let legs = detail["legs"].as_array().unwrap();
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0]["originCrs"], "WOK");
        assert_eq!(legs[0]["departAfter"], "07:43:00");
        assert_eq!(legs[0]["departBefore"], "07:53:00");

        let (_, back) = request(
            router.clone(),
            format!("/JourneyTemplates/{return_id}"),
            Some(&token),
        )
        .await;
        assert_eq!(back["originCrs"], "WAT");
        assert_eq!(back["destinationCrs"], "WOK");
        assert_eq!(back["pairedTemplateId"], id);
        assert_eq!(back["target"]["time"], "17:30:00");

        // Unlinking clears both sides.
        let (status, _) = delete_request(
            router.clone(),
            format!("/JourneyTemplates/{id}/pair"),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, back) = request(
            router.clone(),
            format!("/JourneyTemplates/{return_id}"),
            Some(&token),
        )
        .await;
        assert_eq!(back["pairedTemplateId"], Value::Null);

        // A bad itinerary is refused, and nothing is created.
        let mut bad = plan_body(true);
        bad["return"]["usualItinerary"]["legs"][0]["departs"] = Value::Null;
        let (status, message) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            bad,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            message.as_str().unwrap().starts_with("Return journey: "),
            "{message}"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM journey_templates WHERE user_id = $1")
                .bind(user_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 2);

        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                plan_mode_counts_a_pair_twice_against_the_cap -- --ignored --test-threads=1`"]
    async fn plan_mode_counts_a_pair_twice_against_the_cap() {
        let pool = connect().await;
        let user_id = "test-regular-plan-cap";
        cleanup_user(&pool, user_id).await;
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));
        sqlx::query(
            "INSERT INTO journey_templates (user_id) \
             SELECT $1 FROM generate_series(1, $2::int)",
        )
        .bind(user_id)
        .bind((journey_templates_cap() - 1) as i32)
        .execute(&pool)
        .await
        .unwrap();

        let (status, _) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            plan_body(true),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "a pair needs two places");
        let count = |pool: PgPool| async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM journey_templates WHERE user_id = $1",
            )
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        assert_eq!(count(pool.clone()).await, journey_templates_cap() - 1);

        let (_, return_id) = create_plan(router, &token, false).await;
        assert_eq!(return_id, None);
        assert_eq!(count(pool.clone()).await, journey_templates_cap());

        cleanup_user(&pool, user_id).await;
    }

    fn journey_templates_cap() -> i64 {
        crate::data::journey_templates::MAX_JOURNEY_TEMPLATES_PER_USER
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                put_without_the_regular_fields_keeps_them -- --ignored --test-threads=1`"]
    async fn put_without_the_regular_fields_keeps_them() {
        let pool = connect().await;
        let user_id = "test-regular-put-keeps";
        cleanup_user(&pool, user_id).await;
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));
        let (id, _) = create_plan(router.clone(), &token, false).await;

        // A client from before regular journeys: no target fields.
        let old_shape = serde_json::json!({
            "customName": "Commute",
            "legs": [{"originCrs": "WOK", "destinationCrs": "WAT",
                      "departWindow": {"after": "07:40:00", "before": "07:55:00"}}],
            "daysOfWeek": 31, "active": true, "startsOn": null, "endsOn": null,
            "defaultMatchMode": "manual", "autoCommitRule": null
        });
        let (status, body) = put_json(
            router.clone(),
            format!("/JourneyTemplates/{id}"),
            Some(&token),
            old_shape.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        let (_, detail) = request(
            router.clone(),
            format!("/JourneyTemplates/{id}"),
            Some(&token),
        )
        .await;
        assert_eq!(detail["customName"], "Commute");
        assert_eq!(detail["originCrs"], "WOK");
        assert_eq!(detail["target"]["time"], "07:45:00");
        assert_eq!(detail["usualItinerary"]["legs"][1]["trainUid"], "W12345");
        assert_eq!(detail["planOptions"]["maxChanges"], 1);

        // A new time alone keeps the stations.
        let mut new_time = old_shape.clone();
        new_time["target"] = serde_json::json!({"kind": "arrive_by", "time": "08:30"});
        let (status, _) = put_json(
            router.clone(),
            format!("/JourneyTemplates/{id}"),
            Some(&token),
            new_time,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, detail) = request(
            router.clone(),
            format!("/JourneyTemplates/{id}"),
            Some(&token),
        )
        .await;
        assert_eq!(detail["target"]["kind"], "arrive_by");
        assert_eq!(detail["originCrs"], "WOK");

        // A template without a target needs both stations with one.
        let (status, manual) = post_json(
            router.clone(),
            "/JourneyTemplates".to_string(),
            Some(&token),
            serde_json::json!({"mode": "manual", "legs": [
                {"originCrs": "WOK", "destinationCrs": "WAT"}]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let manual_id = manual["templateId"].as_i64().unwrap();
        let mut target_only = old_shape;
        target_only["target"] = serde_json::json!({"kind": "depart_after", "time": "07:45"});
        let (status, _) = put_json(
            router.clone(),
            format!("/JourneyTemplates/{manual_id}"),
            Some(&token),
            target_only.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        target_only["originCrs"] = Value::from("WOK");
        target_only["destinationCrs"] = Value::from("WAT");
        let (status, _) = put_json(
            router,
            format!("/JourneyTemplates/{manual_id}"),
            Some(&token),
            target_only,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                pausing_pauses_the_pair_and_removes_only_untouched_journeys \
                -- --ignored --test-threads=1`"]
    #[expect(
        clippy::too_many_lines,
        reason = "test code: one scenario read top to bottom"
    )]
    async fn pausing_pauses_the_pair_and_removes_only_untouched_journeys() {
        let pool = connect().await;
        let user_id = "test-regular-pause";
        cleanup_user(&pool, user_id).await;
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));
        let (id, return_id) = create_plan(router.clone(), &token, true).await;
        let return_id = return_id.unwrap();
        let now = tuesday_morning();
        let _pin = crate::routes::pin_london_now_for_tests(now);
        let (today, tomorrow, yesterday) =
            (date("2026-11-10"), date("2026-11-11"), date("2026-11-09"));

        let today_untouched =
            seed_occurrence(&pool, user_id, id, today, Occurrence::Untouched).await;
        let tomorrow_untouched =
            seed_occurrence(&pool, user_id, id, tomorrow, Occurrence::Untouched).await;
        let tomorrow_picked =
            seed_occurrence(&pool, user_id, id, tomorrow, Occurrence::Picked).await;
        let today_started = seed_occurrence(
            &pool,
            user_id,
            id,
            today,
            Occurrence::StartedAt(now - chrono::Duration::minutes(30)),
        )
        .await;
        let yesterdays =
            seed_occurrence(&pool, user_id, id, yesterday, Occurrence::Untouched).await;
        let return_ticketed =
            seed_occurrence(&pool, user_id, return_id, tomorrow, Occurrence::Ticketed).await;
        let return_untouched =
            seed_occurrence(&pool, user_id, return_id, tomorrow, Occurrence::Untouched).await;

        let (status, body) = put_json(
            router.clone(),
            format!("/JourneyTemplates/{return_id}/active"),
            Some(&token),
            serde_json::json!({"active": false}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let mut deleted: Vec<i64> = body["deletedJourneyIds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        deleted.sort_unstable();
        let mut expected = vec![today_untouched, tomorrow_untouched, return_untouched];
        expected.sort_unstable();
        assert_eq!(deleted, expected);
        for kept in [tomorrow_picked, today_started, yesterdays, return_ticketed] {
            assert!(journey_exists(&pool, kept).await, "{kept} must be kept");
        }
        for gone in expected {
            assert!(!journey_exists(&pool, gone).await, "{gone} must be deleted");
        }
        let (_, mine) = request(
            router.clone(),
            "/JourneyTemplates/mine".to_string(),
            Some(&token),
        )
        .await;
        for template in [id, return_id] {
            let row = mine_row(&mine, template);
            assert_eq!(row["active"], false);
            assert_eq!(row["nextDate"], Value::Null);
        }
        // No skip markers: a paused template is never due.
        let markers: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM journey_template_skipped_dates WHERE template_id IN ($1, $2)",
        )
        .bind(id)
        .bind(return_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(markers, 0);

        let (status, body) = put_json(
            router.clone(),
            format!("/JourneyTemplates/{id}/active"),
            Some(&token),
            serde_json::json!({"active": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deletedJourneyIds"], serde_json::json!([]));
        let (_, mine) = request(
            router.clone(),
            "/JourneyTemplates/mine".to_string(),
            Some(&token),
        )
        .await;
        assert_eq!(mine_row(&mine, return_id)["active"], true);

        // Someone else's template is 404.
        let other = "test-regular-pause-other";
        cleanup_user(&pool, other).await;
        let other_token = seed_session(&pool, other).await;
        let (status, _) = put_json(
            router,
            format!("/JourneyTemplates/{id}/active"),
            Some(&other_token),
            serde_json::json!({"active": false}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        cleanup_user(&pool, other).await;
        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                skip_next_skips_the_pair_refuses_a_started_journey_and_undoes \
                -- --ignored --test-threads=1`"]
    #[expect(
        clippy::too_many_lines,
        reason = "test code: one scenario read top to bottom"
    )]
    async fn skip_next_skips_the_pair_refuses_a_started_journey_and_undoes() {
        let pool = connect().await;
        let user_id = "test-regular-skip";
        cleanup_user(&pool, user_id).await;
        let token = seed_session(&pool, user_id).await;
        let router = test_router(test_app(pool.clone()));
        let (id, return_id) = create_plan(router.clone(), &token, true).await;
        let return_id = return_id.unwrap();
        let now = tuesday_morning();
        let _pin = crate::routes::pin_london_now_for_tests(now);
        let (today, tomorrow) = (date("2026-11-10"), date("2026-11-11"));

        let (_, mine) = request(
            router.clone(),
            "/JourneyTemplates/mine".to_string(),
            Some(&token),
        )
        .await;
        assert_eq!(mine_row(&mine, id)["nextDate"], today.to_string());

        // Skip next, with the return: today's journey goes, both skip today.
        let todays = seed_occurrence(&pool, user_id, id, today, Occurrence::Untouched).await;
        let (status, body) = post_json(
            router.clone(),
            format!("/JourneyTemplates/{id}/skip"),
            Some(&token),
            serde_json::json!({"pair": true}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["serviceDate"], today.to_string());
        assert_eq!(body["deletedJourneyIds"], serde_json::json!([todays]));
        assert!(!journey_exists(&pool, todays).await);
        let (_, mine) = request(
            router.clone(),
            "/JourneyTemplates/mine".to_string(),
            Some(&token),
        )
        .await;
        for template in [id, return_id] {
            let row = mine_row(&mine, template);
            assert_eq!(row["nextDate"], tomorrow.to_string());
            assert_eq!(row["skippedDates"], serde_json::json!([today.to_string()]));
        }

        // Undo on the outbound only.
        let (status, _) = delete_request(
            router.clone(),
            format!("/JourneyTemplates/{id}/skip/{today}"),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, mine) = request(
            router.clone(),
            "/JourneyTemplates/mine".to_string(),
            Some(&token),
        )
        .await;
        assert_eq!(mine_row(&mine, id)["nextDate"], today.to_string());
        assert_eq!(mine_row(&mine, return_id)["nextDate"], tomorrow.to_string());

        // Today's journey has set off: it can't be skipped, and the next
        // date moves on to tomorrow.
        seed_occurrence(
            &pool,
            user_id,
            id,
            today,
            Occurrence::StartedAt(now - chrono::Duration::minutes(5)),
        )
        .await;
        let (status, _) = post_json(
            router.clone(),
            format!("/JourneyTemplates/{id}/skip"),
            Some(&token),
            serde_json::json!({"serviceDate": today}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, body) = post_json(
            router.clone(),
            format!("/JourneyTemplates/{id}/skip"),
            Some(&token),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["serviceDate"], tomorrow.to_string());
        assert_eq!(body["templateIds"], serde_json::json!([id]));
        assert_eq!(body["deletedJourneyIds"], serde_json::json!([]));

        // A past date is refused; so is someone else's template.
        let (status, _) = post_json(
            router.clone(),
            format!("/JourneyTemplates/{id}/skip"),
            Some(&token),
            serde_json::json!({"serviceDate": "2026-11-09"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // endsOn: the return ends today, which is skipped, so it has no
        // next date and skipping next is a conflict.
        sqlx::query("UPDATE journey_templates SET ends_on = $2 WHERE id = $1")
            .bind(return_id)
            .bind(today)
            .execute(&pool)
            .await
            .unwrap();
        let (_, mine) = request(
            router.clone(),
            "/JourneyTemplates/mine".to_string(),
            Some(&token),
        )
        .await;
        assert_eq!(mine_row(&mine, return_id)["nextDate"], Value::Null);
        let (status, _) = post_json(
            router,
            format!("/JourneyTemplates/{return_id}/skip"),
            Some(&token),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        cleanup_user(&pool, user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                an_occurrence_names_its_regular_journey -- --ignored --test-threads=1`"]
    async fn an_occurrence_names_its_regular_journey() {
        let pool = connect().await;
        let user_id = "test-regular-source";
        cleanup_user(&pool, user_id).await;
        let token = seed_session(&pool, user_id).await;
        let router = test_router_with_journeys(test_app(pool.clone()));
        let (id, _) = create_plan(router.clone(), &token, false).await;
        let journey_id = seed_occurrence(
            &pool,
            user_id,
            id,
            crate::routes::london_today(),
            Occurrence::Untouched,
        )
        .await;
        let (status, detail) = request(
            router.clone(),
            format!("/Journeys/{journey_id}"),
            Some(&token),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        assert_eq!(detail["sourceTemplateId"], id);
        let (status, mine) = request(router, "/Journeys/mine".to_string(), Some(&token)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(mine_row(&mine, journey_id)["sourceTemplateId"], id);
        cleanup_user(&pool, user_id).await;
    }
}
