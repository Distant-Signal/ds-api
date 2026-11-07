//! Regular journeys, phase 1 (distant-signal's
//! docs/superpowers/specs/2026-11-06-regular-journeys-design.md §3, §4.2):
//! a journey template with a **target** (origin, destination, a
//! depart-after or arrive-by time and a day offset), the plan options it
//! was planned with, the **usual itinerary** the user picked, and an
//! optional **pair** (the return journey, a sibling template). "Regular
//! journeys" is the UI's name; the code and the API keep `JourneyTemplate`.
//!
//! This module holds what is new in phase 1: the wire and stored shapes of
//! the target, options and itinerary, creating a template (or a pair) from
//! a plan, pausing and resuming a pair, skipping a date, unlinking a pair,
//! and the "next date" the list shows. `data::journey_templates` still owns
//! the template rows, their legs and "Run now".
//!
//! **Occurrences.** Each due date gets an ordinary `journeys` row with
//! `source_template_id` set (the notifier mints them, the evening before
//! from phase 1). An occurrence has **started** once a leg is dated before
//! London's today, an untracked leg's planned departure has passed, or a
//! train leg's booked departure from its own origin has passed (the
//! timetable's, else the pinned one; revision 1's booked departure counts
//! too). A train leg with no train picked yet never makes it started.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::types::Json;

use crate::data::journey_templates::{self, TemplateLegInput};
use crate::data::journeys;

/// `journey_templates.target_kind` (its CHECK constraint).
pub const TARGET_KINDS: [&str; 2] = ["depart_after", "arrive_by"];

/// A regular journey's target: leave after, or arrive by, a London clock
/// time, on the service date or (`dayOffset` 1) the day after it, as
/// `/Trips/plan`'s `dayOffset`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct TemplateTarget {
    /// `depart_after` or `arrive_by`.
    #[ts(as = "crate::ts_values::TemplateTargetKind")]
    pub kind: String,
    /// `HH:MM` (seconds optional).
    pub time: NaiveTime,
    /// 0 or 1. Absent means 0.
    #[serde(default)]
    pub day_offset: i16,
}

/// The `/Trips/plan` options a regular journey was planned with, as that
/// route's query parameters take them, so a later plan for the same target
/// can repeat them. Unknown keys are dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(optional_fields = nullable)]
pub struct PlanOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waypoints: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avoid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avoid_stop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avoid_change: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_changes: Option<i16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avoid_operator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefer_operator: Option<String>,
}

/// The longest value one plan option may have.
const PLAN_OPTION_MAX_CHARS: usize = 300;
/// The highest `maxChanges` `/Trips/plan` takes.
const PLAN_OPTION_MAX_CHANGES: i16 = 12;

/// The itinerary the user picked when they made the regular journey,
/// trimmed to what a later day needs: clock times, not instants, since the
/// template has no date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct UsualItinerary {
    pub legs: Vec<UsualItineraryLeg>,
}

/// One leg of a [`UsualItinerary`]. A train leg (`kind` absent or `train`)
/// has a train UID, its stations and its booked departure; an untracked
/// leg (`kind: "untracked"`) its mode, places and minutes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(optional_fields = nullable)]
pub struct UsualItineraryLeg {
    /// `train` or `untracked`.
    #[serde(default = "train_kind")]
    #[ts(as = "crate::ts_values::JourneyLegKind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub train_uid: Option<String>,
    /// The planned date the train was picked on. Request only: the
    /// train's calls are checked against that day's timetable, and it is
    /// not stored.
    #[serde(default, skip_serializing)]
    pub service_date: Option<NaiveDate>,
    /// The operator's two-character code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub atoc_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_crs: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_crs: Option<String>,
    /// London clock time of the booked departure from the leg's origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub departs: Option<NaiveTime>,
    /// Days after the service date `departs` falls on (0 or 1).
    #[serde(default)]
    pub departs_day_offset: i16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrives: Option<NaiveTime>,
    #[serde(default)]
    pub arrives_day_offset: i16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(as = "Option<crate::ts_values::TransitMode>")]
    pub transit_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_name: Option<String>,
    /// An untracked leg's length.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<i16>,
}

fn train_kind() -> String {
    journeys::LEG_KIND_TRAIN.to_string()
}

/// The most legs a usual itinerary may have: the journey's own cap.
pub const MAX_USUAL_ITINERARY_LEGS: usize = 20;

fn is_crs(code: &str) -> bool {
    code.chars().count() == 3 && code.chars().all(|c| c.is_ascii_alphabetic())
}

/// A trimmed, upper-case station code, or the error naming `what`.
pub fn normalize_crs(code: &str, what: &str) -> Result<String, String> {
    let code = code.trim();
    if !is_crs(code) {
        return Err(format!(
            "Enter a valid {what} station. Station codes are three letters, like WOK or EUS."
        ));
    }
    Ok(code.to_ascii_uppercase())
}

/// Checks a target: a known kind and a day offset of 0 or 1.
pub fn validate_target(target: &TemplateTarget) -> Result<(), String> {
    if !TARGET_KINDS.contains(&target.kind.as_str()) {
        return Err("target.kind must be depart_after or arrive_by.".to_string());
    }
    if !(0..=1).contains(&target.day_offset) {
        return Err("target.dayOffset must be 0 or 1.".to_string());
    }
    Ok(())
}

/// Checks plan options: every value at most [`PLAN_OPTION_MAX_CHARS`]
/// characters, `maxChanges` 0 to 12. The planner itself judges the values
/// when it plans with them.
pub fn validate_plan_options(options: &PlanOptions) -> Result<(), String> {
    let texts = [
        &options.waypoints,
        &options.via,
        &options.avoid,
        &options.avoid_stop,
        &options.avoid_change,
        &options.transit,
        &options.operator,
        &options.avoid_operator,
        &options.prefer_operator,
    ];
    if texts.iter().any(|text| {
        text.as_ref()
            .is_some_and(|t| t.chars().count() > PLAN_OPTION_MAX_CHARS)
    }) {
        return Err(format!(
            "Each plan option can be at most {PLAN_OPTION_MAX_CHARS} characters."
        ));
    }
    if options
        .max_changes
        .is_some_and(|changes| !(0..=PLAN_OPTION_MAX_CHANGES).contains(&changes))
    {
        return Err(format!(
            "planOptions.maxChanges must be 0 to {PLAN_OPTION_MAX_CHANGES}."
        ));
    }
    Ok(())
}

fn optional_text(value: Option<&String>, max: usize, what: &str) -> Result<Option<String>, String> {
    let Some(value) = value.map(|v| v.trim()).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if value.chars().count() > max {
        return Err(format!("{what} can be at most {max} characters."));
    }
    Ok(Some(value.to_string()))
}

/// Validates and normalises a usual itinerary: 1 to 20 legs, at least one
/// a train leg. A train leg needs a valid UID, both station codes and its
/// departure time, and takes no untracked fields; an untracked leg needs a
/// known mode and takes no train fields. Day offsets are 0 or 1. The
/// trains' calls are checked separately ([`check_itinerary_calls`]).
pub fn validate_usual_itinerary(itinerary: &UsualItinerary) -> Result<UsualItinerary, String> {
    if itinerary.legs.is_empty() || itinerary.legs.len() > MAX_USUAL_ITINERARY_LEGS {
        return Err(format!(
            "The usual itinerary needs between 1 and {MAX_USUAL_ITINERARY_LEGS} legs."
        ));
    }
    let mut legs = Vec::with_capacity(itinerary.legs.len());
    for (index, leg) in itinerary.legs.iter().enumerate() {
        let numbered = |message: String| {
            if itinerary.legs.len() > 1 {
                format!("Leg {}: {message}", index + 1)
            } else {
                message
            }
        };
        legs.push(validate_usual_leg(leg).map_err(numbered)?);
    }
    if !legs.iter().any(|leg| leg.kind == journeys::LEG_KIND_TRAIN) {
        return Err("The usual itinerary needs at least one train leg.".to_string());
    }
    Ok(UsualItinerary { legs })
}

fn validate_usual_leg(leg: &UsualItineraryLeg) -> Result<UsualItineraryLeg, String> {
    if !(0..=1).contains(&leg.departs_day_offset) || !(0..=1).contains(&leg.arrives_day_offset) {
        return Err("A day offset must be 0 or 1.".to_string());
    }
    match leg.kind.as_str() {
        journeys::LEG_KIND_TRAIN => validate_usual_train_leg(leg),
        journeys::LEG_KIND_UNTRACKED => validate_usual_untracked_leg(leg),
        _ => Err("A leg's kind must be train or untracked.".to_string()),
    }
}

fn validate_usual_train_leg(leg: &UsualItineraryLeg) -> Result<UsualItineraryLeg, String> {
    if leg.transit_mode.is_some()
        || leg.origin_name.is_some()
        || leg.destination_name.is_some()
        || leg.line_name.is_some()
        || leg.minutes.is_some()
    {
        return Err(
            "A train leg takes no transitMode, originName, destinationName, lineName or \
             minutes."
                .to_string(),
        );
    }
    let Some(train_uid) = leg.train_uid.as_deref() else {
        return Err("A train leg needs a trainUid.".to_string());
    };
    journeys::validate_train_uid(train_uid)?;
    let origin = normalize_crs(leg.origin_crs.as_deref().unwrap_or_default(), "origin")?;
    let destination = normalize_crs(
        leg.destination_crs.as_deref().unwrap_or_default(),
        "destination",
    )?;
    let Some(departs) = leg.departs else {
        return Err("A train leg needs its departure time.".to_string());
    };
    let atoc_code = match leg.atoc_code.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(code)
            if code.chars().count() == 2 && code.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            Some(code.to_ascii_uppercase())
        }
        Some(_) => return Err("atocCode must be two letters or digits.".to_string()),
    };
    Ok(UsualItineraryLeg {
        kind: journeys::LEG_KIND_TRAIN.to_string(),
        train_uid: Some(train_uid.trim().to_ascii_uppercase()),
        service_date: leg.service_date,
        atoc_code,
        origin_crs: Some(origin),
        destination_crs: Some(destination),
        departs: Some(departs),
        departs_day_offset: leg.departs_day_offset,
        arrives: leg.arrives,
        arrives_day_offset: leg.arrives_day_offset,
        transit_mode: None,
        origin_name: None,
        destination_name: None,
        line_name: None,
        minutes: None,
    })
}

fn validate_usual_untracked_leg(leg: &UsualItineraryLeg) -> Result<UsualItineraryLeg, String> {
    if leg.train_uid.is_some()
        || leg.service_date.is_some()
        || leg.atoc_code.is_some()
        || leg.origin_crs.is_some()
        || leg.destination_crs.is_some()
    {
        return Err(
            "An untracked leg takes no trainUid, serviceDate, atocCode, originCrs or \
             destinationCrs."
                .to_string(),
        );
    }
    let mode = leg
        .transit_mode
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if !journeys::TRANSIT_MODES.contains(&mode.as_str()) {
        return Err(format!(
            "transitMode must be one of: {}.",
            journeys::TRANSIT_MODES.join(", ")
        ));
    }
    if leg
        .minutes
        .is_some_and(|minutes| !(0..=1440).contains(&minutes))
    {
        return Err("minutes must be 0 to 1440.".to_string());
    }
    Ok(UsualItineraryLeg {
        kind: journeys::LEG_KIND_UNTRACKED.to_string(),
        train_uid: None,
        service_date: None,
        atoc_code: None,
        origin_crs: None,
        destination_crs: None,
        departs: leg.departs,
        departs_day_offset: leg.departs_day_offset,
        arrives: leg.arrives,
        arrives_day_offset: leg.arrives_day_offset,
        transit_mode: Some(mode),
        origin_name: optional_text(
            leg.origin_name.as_ref(),
            journeys::UNTRACKED_PLACE_MAX_CHARS,
            "originName",
        )?,
        destination_name: optional_text(
            leg.destination_name.as_ref(),
            journeys::UNTRACKED_PLACE_MAX_CHARS,
            "destinationName",
        )?,
        line_name: optional_text(
            leg.line_name.as_ref(),
            journeys::UNTRACKED_LINE_MAX_CHARS,
            "lineName",
        )?,
        minutes: leg.minutes,
    })
}

/// The first train leg whose train doesn't call at its origin then its
/// destination on its `serviceDate`, as `POST /Journeys/itinerary` checks
/// a leg (`leg_validation::validate_train_leg`): `(1-based leg number,
/// rejection)`. A leg without a `serviceDate`, or a train with no published
/// schedule that day, passes.
pub async fn check_itinerary_calls(
    pool: &PgPool,
    itinerary: &UsualItinerary,
) -> anyhow::Result<Option<(usize, crate::data::leg_validation::LegRejection)>> {
    for (index, leg) in itinerary.legs.iter().enumerate() {
        let (Some(uid), Some(date)) = (leg.train_uid.as_deref(), leg.service_date) else {
            continue;
        };
        if let Some(rejection) = crate::data::leg_validation::validate_train_leg(
            pool,
            uid,
            date,
            leg.origin_crs.as_deref(),
            leg.destination_crs.as_deref(),
        )
        .await?
        {
            return Ok(Some((index + 1, rejection)));
        }
    }
    Ok(None)
}

/// The template legs a usual itinerary stands for: one per train leg, in
/// order, with a departure window around its booked departure
/// (`journey_templates::window_around_departure`, phase 0's derivation for
/// a promoted train). Untracked legs have nothing to search for.
pub fn template_legs_from_itinerary(itinerary: &UsualItinerary) -> Vec<TemplateLegInput> {
    itinerary
        .legs
        .iter()
        .filter(|leg| leg.kind == journeys::LEG_KIND_TRAIN)
        .map(|leg| {
            let window = leg.departs.map(journey_templates::window_around_departure);
            TemplateLegInput {
                origin_crs: leg.origin_crs.clone(),
                destination_crs: leg.destination_crs.clone(),
                depart_after: window.map(|(after, _)| after),
                depart_before: window.map(|(_, before)| before),
                arrive_after: None,
                arrive_before: None,
            }
        })
        .collect()
}

/// One template of a plan-mode create: the outbound, or its return.
pub struct PlanTemplateInput {
    pub custom_name: Option<String>,
    pub origin_crs: String,
    pub destination_crs: String,
    pub target: TemplateTarget,
    pub plan_options: Option<PlanOptions>,
    pub usual_itinerary: UsualItinerary,
    /// Minutes to get to the origin station (0 to 120), for the leave-by
    /// (regular journeys design §5.1); `None` for no leave-by.
    pub station_buffer_minutes: Option<i16>,
}

/// The recurrence a plan-mode create gives both templates.
pub struct PlanRecurrence {
    /// `None`: no days, a saved route (journeys are made from it by hand).
    pub days_of_week: Option<i16>,
    pub starts_on: Option<NaiveDate>,
    pub ends_on: Option<NaiveDate>,
    /// `manual` or `auto`.
    pub default_match_mode: String,
    /// Skip the owner's bank holidays (phase 5; both of a pair).
    pub skip_bank_holidays: bool,
}

/// What [`create_plan_templates`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatePlanOutcome {
    Created {
        id: i64,
        return_id: Option<i64>,
    },
    /// The templates would take the user past
    /// `journey_templates::MAX_JOURNEY_TEMPLATES_PER_USER`.
    OverCap,
}

/// Creates a regular journey from a plan, and with `return_journey` its
/// return, linked as a pair, in one transaction. The cap counts both. The
/// user row is locked first, so two concurrent creates can't both pass
/// the cap.
pub async fn create_plan_templates(
    pool: &PgPool,
    user_id: &str,
    recurrence: &PlanRecurrence,
    outbound: &PlanTemplateInput,
    return_journey: Option<&PlanTemplateInput>,
) -> anyhow::Result<CreatePlanOutcome> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    let owned: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM journey_templates WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&mut *tx)
            .await?;
    let adding = if return_journey.is_some() { 2 } else { 1 };
    if owned + adding > journey_templates::MAX_JOURNEY_TEMPLATES_PER_USER {
        tx.rollback().await?;
        return Ok(CreatePlanOutcome::OverCap);
    }
    let id = insert_plan_template(&mut tx, user_id, recurrence, outbound).await?;
    let return_id = match return_journey {
        Some(input) => {
            let return_id = insert_plan_template(&mut tx, user_id, recurrence, input).await?;
            sqlx::query(
                "UPDATE journey_templates SET paired_template_id = CASE id WHEN $1 THEN $2 ELSE $1 END \
                 WHERE id IN ($1, $2)",
            )
            .bind(id)
            .bind(return_id)
            .execute(&mut *tx)
            .await?;
            Some(return_id)
        }
        None => None,
    };
    tx.commit().await?;
    Ok(CreatePlanOutcome::Created { id, return_id })
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "a template has a handful of legs"
)]
async fn insert_plan_template(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user_id: &str,
    recurrence: &PlanRecurrence,
    input: &PlanTemplateInput,
) -> anyhow::Result<i64> {
    let auto_commit_rule = (recurrence.default_match_mode == "auto").then_some("nearest_to_now");
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO journey_templates \
            (user_id, custom_name, days_of_week, active, starts_on, ends_on, default_match_mode, \
             auto_commit_rule, origin_crs, destination_crs, target_kind, target_time, \
             target_day_offset, plan_options, usual_itinerary, station_buffer_minutes, \
             skip_bank_holidays) \
         VALUES ($1, $2, $3, TRUE, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16) \
         RETURNING id",
    )
    .bind(user_id)
    .bind(input.custom_name.as_deref())
    .bind(recurrence.days_of_week)
    .bind(recurrence.starts_on)
    .bind(recurrence.ends_on)
    .bind(&recurrence.default_match_mode)
    .bind(auto_commit_rule)
    .bind(&input.origin_crs)
    .bind(&input.destination_crs)
    .bind(&input.target.kind)
    .bind(input.target.time)
    .bind(input.target.day_offset)
    .bind(input.plan_options.as_ref().map(Json))
    .bind(Json(&input.usual_itinerary))
    .bind(input.station_buffer_minutes)
    .bind(recurrence.skip_bank_holidays)
    .fetch_one(&mut **tx)
    .await?;
    for (index, leg) in template_legs_from_itinerary(&input.usual_itinerary)
        .iter()
        .enumerate()
    {
        journey_templates::insert_template_leg(tx, id, (index + 1) as i32, leg).await?;
    }
    Ok(id)
}

/// SQL: whether occurrence `{journey}` (an alias of `journeys`) has started
/// (see the module doc), as of `{now}` (a `timestamptz` expression) and
/// London date `{today}` (a `date` expression).
pub(crate) fn occurrence_started_sql(journey: &str, now: &str, today: &str) -> String {
    use crate::data::journeys::{LegEnd, leg_crs_sql};
    let origin = leg_crs_sql(LegEnd::Origin, "occ_l", "occ_ts", "occ_tr");
    let timetabled = crate::data::travel_order::scheduled_departure_sql(
        "occ_tr.train_uid",
        "occ_tr.service_date",
        &origin,
    );
    format!(
        "(EXISTS (SELECT 1 FROM journey_legs occ_l \
                  LEFT JOIN train_subscriptions occ_ts ON occ_ts.id = occ_l.train_subscription_id \
                  LEFT JOIN trains occ_tr ON occ_tr.id = occ_ts.trains_id \
                  WHERE occ_l.journey_id = {journey}.id \
                    AND (occ_l.service_date < {today} \
                         OR occ_l.planned_departure <= {now} \
                         OR (occ_tr.id IS NOT NULL \
                             AND COALESCE({timetabled}, \
                                          CASE WHEN UPPER({origin}) = \
                                                    UPPER(COALESCE(occ_ts.pin_origin_crs, occ_tr.origin_crs)) \
                                               THEN COALESCE(occ_ts.pin_scheduled_departure, \
                                                             occ_tr.scheduled_departure) END) \
                                 <= {now}))) \
          OR EXISTS (SELECT 1 FROM journey_revisions occ_r \
                     JOIN journey_revision_legs occ_rl ON occ_rl.revision_id = occ_r.id \
                     WHERE occ_r.journey_id = {journey}.id AND occ_r.revision_no = 1 \
                       AND COALESCE(occ_rl.booked_departure, occ_rl.planned_departure) <= {now}))"
    )
}

/// SQL: whether occurrence `{journey}` is untouched by its owner: every
/// train leg is still unmatched or was committed by the sweep (`auto`), and
/// no leg's train has a ticket.
fn occurrence_untouched_sql(journey: &str) -> String {
    format!(
        "(NOT EXISTS (SELECT 1 FROM journey_legs ut_l \
                      WHERE ut_l.journey_id = {journey}.id AND ut_l.kind = 'train' \
                        AND ut_l.match_mode NOT IN ('unmatched', 'auto')) \
          AND NOT EXISTS (SELECT 1 FROM journey_legs ut_t \
                          JOIN tracked_train_tickets ut_k \
                            ON ut_k.tracked_train_id = ut_t.train_subscription_id \
                          WHERE ut_t.journey_id = {journey}.id))"
    )
}

/// The template `template_id` of `user_id` and its pair (if the pair is
/// also theirs), locked `FOR UPDATE` in id order. Empty when the template
/// isn't theirs.
pub(crate) async fn lock_with_pair(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    template_id: i64,
    user_id: &str,
) -> anyhow::Result<Vec<i64>> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM journey_templates \
         WHERE user_id = $2 \
           AND (id = $1 OR id = (SELECT paired_template_id FROM journey_templates \
                                 WHERE id = $1 AND user_id = $2)) \
           AND EXISTS (SELECT 1 FROM journey_templates WHERE id = $1 AND user_id = $2) \
         ORDER BY id FOR UPDATE",
    )
    .bind(template_id)
    .bind(user_id)
    .fetch_all(&mut **tx)
    .await?;
    Ok(ids)
}

/// What [`set_active`] did: the templates changed, and how many untouched
/// occurrences a pause deleted. `None` from [`set_active`] when the
/// template isn't the caller's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveOutcome {
    pub template_ids: Vec<i64>,
    pub deleted_journey_ids: Vec<i64>,
}

/// Pauses or resumes a template and its pair in one transaction (decision
/// 16). Pausing also deletes their occurrences dated today or later that
/// haven't started and that the owner hasn't touched
/// ([`occurrence_untouched_sql`]), releasing their trains as deleting a
/// journey does, without writing skip markers: a paused template is never
/// due, so none are needed.
pub async fn set_active(
    pool: &PgPool,
    template_id: i64,
    user_id: &str,
    active: bool,
    now: DateTime<Utc>,
) -> anyhow::Result<Option<ActiveOutcome>> {
    let today = london_date(now);
    let mut tx = pool.begin().await?;
    let ids = lock_with_pair(&mut tx, template_id, user_id).await?;
    if ids.is_empty() {
        tx.rollback().await?;
        return Ok(None);
    }
    sqlx::query(
        "UPDATE journey_templates SET active = $2, updated_at = NOW() \
         WHERE id = ANY($1) AND active <> $2",
    )
    .bind(&ids)
    .bind(active)
    .execute(&mut *tx)
    .await?;
    let mut deleted = Vec::new();
    if !active {
        let doomed: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT j.id FROM journeys j \
             WHERE j.source_template_id = ANY($1) AND j.user_id = $2 \
               AND EXISTS (SELECT 1 FROM journey_legs l WHERE l.journey_id = j.id) \
               AND NOT EXISTS (SELECT 1 FROM journey_legs l \
                               WHERE l.journey_id = j.id AND l.service_date < $4) \
               AND NOT {started} \
               AND {untouched} \
             ORDER BY j.id",
            started = occurrence_started_sql("j", "$3", "$4"),
            untouched = occurrence_untouched_sql("j"),
        ))
        .bind(&ids)
        .bind(user_id)
        .bind(now)
        .bind(today)
        .fetch_all(&mut *tx)
        .await?;
        for journey_id in doomed {
            if journeys::delete_journey_row_releasing_trains(&mut tx, journey_id, user_id).await? {
                deleted.push(journey_id);
            }
        }
    }
    tx.commit().await?;
    Ok(Some(ActiveOutcome {
        template_ids: ids,
        deleted_journey_ids: deleted,
    }))
}

/// What [`skip`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipOutcome {
    /// Skipped `dates` (ascending, each once) for these templates;
    /// `deleted_journey_ids` are the occurrences already minted for them,
    /// now deleted.
    Skipped {
        dates: Vec<NaiveDate>,
        template_ids: Vec<i64>,
        deleted_journey_ids: Vec<i64>,
    },
    /// No such template, or not the caller's.
    NotFound,
    /// No date was given and the template has no next date.
    NoNextDate,
    /// A date is before London's today.
    InThePast,
    /// A date's occurrence (this template's, or with `pair` its pair's)
    /// has already started.
    Started,
}

/// Skips `dates` (none: the template's next date,
/// [`TemplateDates::next_date`]) for template `template_id`, and with
/// `pair` for its pair too, in one transaction: all of them or, when one
/// is refused, none. Each skip is a `journey_template_skipped_dates`
/// marker; an occurrence already minted for a date is deleted as
/// `journeys::delete_journey` deletes one (which writes the same marker).
/// Refused once that occurrence has started.
pub async fn skip(
    pool: &PgPool,
    template_id: i64,
    user_id: &str,
    dates: &[NaiveDate],
    pair: bool,
    now: DateTime<Utc>,
) -> anyhow::Result<SkipOutcome> {
    let today = london_date(now);
    let mut tx = pool.begin().await?;
    let locked = lock_with_pair(&mut tx, template_id, user_id).await?;
    if locked.is_empty() {
        tx.rollback().await?;
        return Ok(SkipOutcome::NotFound);
    }
    let mut dates = dates.to_vec();
    if dates.is_empty() {
        let Some(date) = next_dates(&mut tx, &[template_id], now)
            .await?
            .get(&template_id)
            .copied()
            .flatten()
        else {
            tx.rollback().await?;
            return Ok(SkipOutcome::NoNextDate);
        };
        dates.push(date);
    }
    dates.sort_unstable();
    dates.dedup();
    if dates.first().is_some_and(|first| *first < today) {
        tx.rollback().await?;
        return Ok(SkipOutcome::InThePast);
    }
    let template_ids: Vec<i64> = if pair { locked } else { vec![template_id] };
    let occurrences: Vec<(i64, bool)> = sqlx::query_as(&format!(
        "SELECT j.id, {started} FROM journeys j \
         WHERE j.source_template_id = ANY($1) AND j.user_id = $2 \
           AND EXISTS (SELECT 1 FROM journey_legs l \
                       WHERE l.journey_id = j.id AND l.service_date = ANY($5)) \
         ORDER BY j.id",
        started = occurrence_started_sql("j", "$3", "$4"),
    ))
    .bind(&template_ids)
    .bind(user_id)
    .bind(now)
    .bind(today)
    .bind(&dates)
    .fetch_all(&mut *tx)
    .await?;
    if occurrences.iter().any(|(_, started)| *started) {
        tx.rollback().await?;
        return Ok(SkipOutcome::Started);
    }
    let mut deleted = Vec::new();
    for (journey_id, _) in occurrences {
        journeys::record_template_occurrence_skips(&mut tx, journey_id).await?;
        if journeys::delete_journey_row_releasing_trains(&mut tx, journey_id, user_id).await? {
            deleted.push(journey_id);
        }
    }
    sqlx::query(
        "INSERT INTO journey_template_skipped_dates (template_id, service_date) \
         SELECT id, d FROM unnest($1::bigint[]) AS id CROSS JOIN unnest($2::date[]) AS d \
         ON CONFLICT (template_id, service_date) DO NOTHING",
    )
    .bind(&template_ids)
    .bind(&dates)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(SkipOutcome::Skipped {
        dates,
        template_ids,
        deleted_journey_ids: deleted,
    })
}

/// Undoes a skip: removes the marker for `date` on template `template_id`.
/// The notifier's next sweep mints the occurrence again if it is still
/// due. `false` when the template isn't the caller's; `true` otherwise,
/// whether or not a marker was there.
pub async fn unskip(
    pool: &PgPool,
    template_id: i64,
    user_id: &str,
    date: NaiveDate,
) -> anyhow::Result<bool> {
    let owned: Option<i64> =
        sqlx::query_scalar("SELECT id FROM journey_templates WHERE id = $1 AND user_id = $2")
            .bind(template_id)
            .bind(user_id)
            .fetch_optional(pool)
            .await?;
    if owned.is_none() {
        return Ok(false);
    }
    sqlx::query(
        "DELETE FROM journey_template_skipped_dates WHERE template_id = $1 AND service_date = $2",
    )
    .bind(template_id)
    .bind(date)
    .execute(pool)
    .await?;
    Ok(true)
}

/// Unlinks template `template_id` from its pair, both sides. `false` when
/// the template isn't the caller's.
pub async fn unpair(pool: &PgPool, template_id: i64, user_id: &str) -> anyhow::Result<bool> {
    let mut tx = pool.begin().await?;
    let ids = lock_with_pair(&mut tx, template_id, user_id).await?;
    if ids.is_empty() {
        tx.rollback().await?;
        return Ok(false);
    }
    sqlx::query(
        "UPDATE journey_templates SET paired_template_id = NULL, updated_at = NOW() \
         WHERE id = ANY($1) AND paired_template_id IS NOT NULL",
    )
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// The London date of `now`.
pub fn london_date(now: DateTime<Utc>) -> NaiveDate {
    now.with_timezone(&chrono_tz::Europe::London).date_naive()
}

/// How many dates `upcomingDates` lists.
pub const UPCOMING_DATES: usize = 8;

/// `skipped`: a skip marker (the user skipped the date, or deleted its
/// journey). The reasons are `ds_store::recurrence::SkipReason::as_str`.
pub const SKIP_REASON_SKIPPED: &str = "skipped";
/// `bank_holiday`: a bank holiday in the owner's division.
pub const SKIP_REASON_BANK_HOLIDAY: &str = "bank_holiday";

/// One date a template passes over before its next date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct NextSkip {
    pub date: NaiveDate,
    /// `"skipped"` or `"bank_holiday"`; `"skipped"` when both apply.
    #[ts(as = "crate::ts_values::NextSkipReason")]
    pub reason: &'static str,
}

/// One of a template's next scheduled dates (`upcomingDates`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct UpcomingDate {
    pub date: NaiveDate,
    /// `null` when a journey is made for it; otherwise why not, as
    /// [`NextSkip::reason`].
    #[ts(as = "Option<crate::ts_values::NextSkipReason>")]
    pub skip_reason: Option<&'static str>,
}

impl From<ds_store::recurrence::Upcoming> for UpcomingDate {
    fn from(day: ds_store::recurrence::Upcoming) -> Self {
        Self {
            date: day.date,
            skip_reason: day.skip.map(ds_store::recurrence::SkipReason::as_str),
        }
    }
}

/// A template's dates from London's today, by `ds_store::recurrence` (the
/// rule the notifier's mint decides with).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TemplateDates {
    /// The next due date on or after today, passing over skipped ones, and
    /// not today once today's journey has started: the date "Skip next"
    /// skips. `None` when paused, not recurring, past its end, or with no
    /// due date in `recurrence::DEFAULT_HORIZON_DAYS`.
    pub next_date: Option<NaiveDate>,
    /// As `next_date`, but not today once today's journey was added
    /// (`nextRunOn`, the old list field).
    pub next_run_on: Option<NaiveDate>,
    /// The scheduled dates from today up to, not including, `next_date`
    /// that are skipped, with why. Empty when there is no next date.
    pub next_skips: Vec<NextSkip>,
    /// The next [`UPCOMING_DATES`] scheduled dates, due or skipped, from
    /// today (from tomorrow once today's journey has started).
    pub upcoming: Vec<UpcomingDate>,
}

/// [`TemplateDates`] for one template's rule and exceptions, read from
/// `today` on.
pub(crate) fn dates_for(
    schedule: &ds_store::recurrence::Schedule,
    today: NaiveDate,
    today_started: bool,
    ran_today: bool,
) -> TemplateDates {
    use ds_store::recurrence::DEFAULT_HORIZON_DAYS;

    let (rule, exceptions) = (&schedule.rule, &schedule.exceptions);
    let tomorrow = today.succ_opt().unwrap_or(today);
    let from = if today_started { tomorrow } else { today };
    let next_date = rule.next_due(exceptions, from, DEFAULT_HORIZON_DAYS);
    let next_run_on = rule.next_due(
        exceptions,
        if ran_today { tomorrow } else { today },
        DEFAULT_HORIZON_DAYS,
    );
    let next_skips = next_date.map_or_else(Vec::new, |next| {
        rule.upcoming(exceptions, today, usize::MAX, DEFAULT_HORIZON_DAYS)
            .into_iter()
            .take_while(|day| day.date < next)
            .filter_map(|day| {
                day.skip.map(|reason| NextSkip {
                    date: day.date,
                    reason: reason.as_str(),
                })
            })
            .collect()
    });
    let upcoming = rule
        .upcoming(exceptions, from, UPCOMING_DATES, DEFAULT_HORIZON_DAYS)
        .into_iter()
        .map(UpcomingDate::from)
        .collect();
    TemplateDates {
        next_date,
        next_run_on,
        next_skips,
        upcoming,
    }
}

/// [`template_dates`]' row: whether today's journey was added, and whether
/// it has started.
#[derive(Debug, sqlx::FromRow)]
struct TodayRow {
    id: i64,
    ran_today: bool,
    today_started: bool,
}

/// The [`TemplateDates`] of each template in `template_ids`, by id. A
/// template that doesn't exist is absent. Two statements on `conn` (a connection, or a transaction's): the
/// rule and its exceptions (`ds_store::recurrence::load`, from today to
/// the horizon), and today's journey.
pub async fn template_dates(
    conn: &mut sqlx::PgConnection,
    template_ids: &[i64],
    now: DateTime<Utc>,
) -> anyhow::Result<HashMap<i64, TemplateDates>> {
    let today = london_date(now);
    let horizon_end =
        today + chrono::Duration::days(i64::from(ds_store::recurrence::DEFAULT_HORIZON_DAYS));
    let schedules =
        ds_store::recurrence::load(&mut *conn, template_ids, today, horizon_end).await?;
    let rows: Vec<TodayRow> = sqlx::query_as(&format!(
        "SELECT jt.id, \
                EXISTS (SELECT 1 FROM journeys j \
                        WHERE j.source_template_id = jt.id \
                          AND EXISTS (SELECT 1 FROM journey_legs l \
                                      WHERE l.journey_id = j.id AND l.service_date = $3)) \
                    AS ran_today, \
                EXISTS (SELECT 1 FROM journeys j \
                        WHERE j.source_template_id = jt.id \
                          AND EXISTS (SELECT 1 FROM journey_legs l \
                                      WHERE l.journey_id = j.id AND l.service_date = $3) \
                          AND {started}) AS today_started \
         FROM journey_templates jt WHERE jt.id = ANY($1)",
        started = occurrence_started_sql("j", "$2", "$3"),
    ))
    .bind(template_ids)
    .bind(now)
    .bind(today)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            let schedule = schedules.get(&row.id)?;
            Some((
                row.id,
                dates_for(schedule, today, row.today_started, row.ran_today),
            ))
        })
        .collect())
}

/// The next date of each template in `template_ids`
/// ([`TemplateDates::next_date`]), by id. A template that doesn't exist is
/// absent.
pub async fn next_dates(
    conn: &mut sqlx::PgConnection,
    template_ids: &[i64],
    now: DateTime<Utc>,
) -> anyhow::Result<HashMap<i64, Option<NaiveDate>>> {
    Ok(template_dates(conn, template_ids, now)
        .await?
        .into_iter()
        .map(|(id, dates)| (id, dates.next_date))
        .collect())
}

/// [`next_dates`] with each template's [`TemplateDates::next_skips`]: the
/// dates it passes over on the way to its next date, and why.
pub async fn next_dates_and_skips(
    pool: &PgPool,
    template_ids: &[i64],
    now: DateTime<Utc>,
) -> anyhow::Result<HashMap<i64, (Option<NaiveDate>, Vec<NextSkip>)>> {
    let mut conn = pool.acquire().await?;
    Ok(template_dates(&mut conn, template_ids, now)
        .await?
        .into_iter()
        .map(|(id, dates)| (id, (dates.next_date, dates.next_skips)))
        .collect())
}

/// Skip markers on `template_id` from London's today on, ascending.
pub async fn skipped_dates_from<'c, E>(
    executor: E,
    template_id: i64,
    today: NaiveDate,
) -> anyhow::Result<Vec<NaiveDate>>
where
    E: sqlx::PgExecutor<'c>,
{
    Ok(sqlx::query_scalar(
        "SELECT service_date FROM journey_template_skipped_dates \
         WHERE template_id = $1 AND service_date >= $2 ORDER BY service_date",
    )
    .bind(template_id)
    .bind(today)
    .fetch_all(executor)
    .await?)
}

/// Reads a stored JSONB value as `T`, `None` when absent or not that shape
/// (a row written by a later version, say).
pub fn decode_stored<T: serde::de::DeserializeOwned>(
    value: Option<Json<serde_json::Value>>,
) -> Option<T> {
    value.and_then(|Json(value)| serde_json::from_value(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn train_leg(uid: &str, from: &str, to: &str, departs: Option<NaiveTime>) -> UsualItineraryLeg {
        UsualItineraryLeg {
            kind: "train".to_string(),
            train_uid: Some(uid.to_string()),
            service_date: None,
            atoc_code: Some("sw".to_string()),
            origin_crs: Some(from.to_string()),
            destination_crs: Some(to.to_string()),
            departs,
            departs_day_offset: 0,
            arrives: Some(time(8, 20)),
            arrives_day_offset: 0,
            transit_mode: None,
            origin_name: None,
            destination_name: None,
            line_name: None,
            minutes: None,
        }
    }

    #[test]
    fn a_target_parses_hh_mm_and_defaults_its_day_offset() {
        let target: TemplateTarget =
            serde_json::from_str(r#"{"kind": "depart_after", "time": "07:45"}"#).unwrap();
        assert_eq!(target.time, time(7, 45));
        assert_eq!(target.day_offset, 0);
        assert!(validate_target(&target).is_ok());
        let bad = TemplateTarget {
            kind: "leave_at".to_string(),
            ..target.clone()
        };
        assert!(validate_target(&bad).is_err());
        let late = TemplateTarget {
            day_offset: 2,
            ..target
        };
        assert!(validate_target(&late).is_err());
    }

    #[test]
    fn an_itinerary_is_normalised_and_its_train_legs_become_template_legs() {
        let itinerary = UsualItinerary {
            legs: vec![
                UsualItineraryLeg {
                    kind: "untracked".to_string(),
                    transit_mode: Some("Walk".to_string()),
                    origin_name: Some(" Home ".to_string()),
                    minutes: Some(12),
                    ..train_leg("X", "A", "B", None)
                }
                .with_no_train(),
                train_leg("c12345", "wok", "wat", Some(time(7, 48))),
            ],
        };
        let valid = validate_usual_itinerary(&itinerary).unwrap();
        assert_eq!(valid.legs[0].transit_mode.as_deref(), Some("walk"));
        assert_eq!(valid.legs[0].origin_name.as_deref(), Some("Home"));
        assert_eq!(valid.legs[1].train_uid.as_deref(), Some("C12345"));
        assert_eq!(valid.legs[1].origin_crs.as_deref(), Some("WOK"));
        assert_eq!(valid.legs[1].atoc_code.as_deref(), Some("SW"));
        let legs = template_legs_from_itinerary(&valid);
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].origin_crs.as_deref(), Some("WOK"));
        assert_eq!(legs[0].depart_after, Some(time(7, 43)));
        assert_eq!(legs[0].depart_before, Some(time(7, 53)));
    }

    impl UsualItineraryLeg {
        fn with_no_train(mut self) -> Self {
            self.train_uid = None;
            self.atoc_code = None;
            self.origin_crs = None;
            self.destination_crs = None;
            self
        }
    }

    #[test]
    fn an_itinerary_needs_a_timed_train_leg() {
        let untimed = UsualItinerary {
            legs: vec![train_leg("C12345", "WOK", "WAT", None)],
        };
        assert!(validate_usual_itinerary(&untimed).is_err());
        let empty = UsualItinerary { legs: vec![] };
        assert!(validate_usual_itinerary(&empty).is_err());
        let walk_only = UsualItinerary {
            legs: vec![
                UsualItineraryLeg {
                    kind: "untracked".to_string(),
                    transit_mode: Some("walk".to_string()),
                    ..train_leg("X", "A", "B", None)
                }
                .with_no_train(),
            ],
        };
        assert!(validate_usual_itinerary(&walk_only).is_err());
        let bad_station = UsualItinerary {
            legs: vec![train_leg("C12345", "WOKING", "WAT", Some(time(7, 48)))],
        };
        assert!(validate_usual_itinerary(&bad_station).is_err());
    }

    #[test]
    fn the_stored_itinerary_drops_the_service_date() {
        let mut leg = train_leg("C12345", "WOK", "WAT", Some(time(7, 48)));
        leg.service_date = NaiveDate::from_ymd_opt(2026, 11, 10);
        let json = serde_json::to_value(UsualItinerary { legs: vec![leg] }).unwrap();
        assert!(json["legs"][0].get("serviceDate").is_none());
        assert_eq!(json["legs"][0]["departs"], "07:48:00");
    }

    #[test]
    fn plan_options_are_bounded() {
        assert!(validate_plan_options(&PlanOptions::default()).is_ok());
        let options = PlanOptions {
            max_changes: Some(13),
            ..PlanOptions::default()
        };
        assert!(validate_plan_options(&options).is_err());
        let options = PlanOptions {
            via: Some("x".repeat(301)),
            ..PlanOptions::default()
        };
        assert!(validate_plan_options(&options).is_err());
    }

    fn weekdays(
        skipped: &[NaiveDate],
        bank_holidays: &[NaiveDate],
    ) -> ds_store::recurrence::Schedule {
        ds_store::recurrence::Schedule {
            user_id: "u".to_string(),
            rule: ds_store::recurrence::Rule::weekly(Some(0b001_1111)),
            exceptions: ds_store::recurrence::Exceptions {
                skipped: skipped.iter().copied().collect(),
                bank_holidays: bank_holidays.iter().copied().collect(),
                away: Vec::new(),
            },
        }
    }

    fn day(m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, d).unwrap()
    }

    #[test]
    fn the_next_date_skips_markers_and_a_started_today() {
        // Monday 9 Nov 2026, weekdays.
        let today = day(11, 9);
        let tuesday = day(11, 10);
        assert_eq!(
            dates_for(&weekdays(&[], &[]), today, false, false).next_date,
            Some(today)
        );
        let started = dates_for(&weekdays(&[], &[]), today, true, true);
        assert_eq!(started.next_date, Some(tuesday));
        assert_eq!(started.next_run_on, Some(tuesday));
        // Added but not started: still today's to skip.
        let added = dates_for(&weekdays(&[], &[]), today, false, true);
        assert_eq!(added.next_date, Some(today));
        assert_eq!(added.next_run_on, Some(tuesday));
        assert_eq!(
            dates_for(&weekdays(&[today, tuesday], &[]), today, false, false).next_date,
            Some(day(11, 11))
        );
        let mut ended = weekdays(&[], &[]);
        ended.rule.ends_on = Some(today);
        assert_eq!(dates_for(&ended, today, true, true).next_date, None);
    }

    #[test]
    fn next_skips_are_the_skipped_dates_before_the_next_and_upcoming_is_a_superset() {
        let today = day(11, 9);
        // Skipped Mon and Tue, a bank holiday on Wed that is skipped too.
        let schedule = weekdays(
            &[today, day(11, 10), day(11, 11)],
            &[day(11, 11), day(11, 12)],
        );
        let dates = dates_for(&schedule, today, false, false);
        assert_eq!(dates.next_date, Some(day(11, 13)));
        let skips: Vec<_> = dates
            .next_skips
            .iter()
            .map(|s| (s.date, s.reason))
            .collect();
        assert_eq!(
            skips,
            vec![
                (today, SKIP_REASON_SKIPPED),
                (day(11, 10), SKIP_REASON_SKIPPED),
                (day(11, 11), SKIP_REASON_SKIPPED),
                (day(11, 12), SKIP_REASON_BANK_HOLIDAY),
            ]
        );
        assert_eq!(dates.upcoming.len(), UPCOMING_DATES);
        for skip in &dates.next_skips {
            assert!(dates.upcoming.contains(&UpcomingDate {
                date: skip.date,
                skip_reason: Some(skip.reason),
            }));
        }
        assert_eq!(
            dates.upcoming[4],
            UpcomingDate {
                date: day(11, 13),
                skip_reason: None
            }
        );
        // The weekend isn't listed.
        assert_eq!(dates.upcoming[5].date, day(11, 16));
    }

    #[test]
    fn a_start_date_a_pause_or_no_days_bound_the_next_date() {
        let friday = day(11, 6);
        let mut later = weekdays(&[], &[]);
        later.rule.starts_on = Some(day(12, 1));
        assert_eq!(
            dates_for(&later, friday, false, false).next_date,
            Some(day(12, 1))
        );
        let mut paused = weekdays(&[], &[]);
        paused.rule.active = false;
        let dates = dates_for(&paused, friday, false, false);
        assert_eq!(dates.next_date, None);
        assert_eq!(dates.upcoming, Vec::new());
        let mut one_off = weekdays(&[], &[]);
        one_off.rule.days_of_week = None;
        assert_eq!(dates_for(&one_off, friday, false, false).next_date, None);
        // The weekend is passed over without being a skip.
        let dates = dates_for(&weekdays(&[], &[]), friday, true, true);
        assert_eq!(dates.next_date, Some(day(11, 9)));
        assert_eq!(dates.next_skips, Vec::new());
    }

    #[test]
    fn the_next_date_is_found_past_a_year_of_skips() {
        let today = day(11, 9);
        let mondays: Vec<_> = today.iter_weeks().take(52).collect();
        let mut schedule = weekdays(&mondays, &[]);
        schedule.rule.days_of_week = Some(1);
        let dates = dates_for(&schedule, today, false, false);
        assert_eq!(
            dates.next_date,
            Some(day(11, 9) + chrono::Duration::weeks(52))
        );
        assert_eq!(dates.next_skips.len(), 52);
        assert_eq!(dates.upcoming.len(), UPCOMING_DATES);
    }
}
