//! The regular journeys plan sweep (distant-signal's
//! docs/superpowers/specs/2026-11-06-regular-journeys-design.md §6.2): the
//! planner chooses the day's train for each occurrence of a regular journey
//! with a target. Every `REGULAR_PLAN_SWEEP_INTERVAL_SECS`, under its own
//! advisory lock (`ds_runtime::advisory_locks::REGULAR_PLAN_SWEEP`, so with
//! several api replicas one runs it), at most
//! `REGULAR_PLAN_SWEEP_MAX_OCCURRENCES_PER_TICK` occurrences, the api takes
//! each occurrence dated London's today or tomorrow whose template has a
//! target and that
//!
//! - hasn't started, has never had a break and has only its booked plan
//!   (`journey_revisions::still_planning`'s rule);
//! - has no train its owner picked (a `manual` train leg): the owner's pick
//!   is never replaced, nor suggested over;
//! - is due: never planned, planned against an older schedule publish
//!   (`regular_plan_marker`), or the morning check (the first tick after
//!   `REGULAR_PLAN_SWEEP_MORNING_CHECK_HOUR`, 05:00, London time on the
//!   service date, when it was planned before then).
//!
//! For each it
//!
//! 1. leaves a day with no published timetable (and a target already past,
//!    or in the missing hour of a clock change) `pending`: "no service" is
//!    never decided on a day the timetable doesn't cover;
//! 2. plans the target (on a weekday with a different time, R3's
//!    `data::day_overrides`, that day's target, and later that day's usual
//!    train) through its own planning slot (`ReplanSlot::Regular`,
//!    one permit, never a user's or the break sweep's), with the template's
//!    vias, avoids, `maxChanges` and operator filter, `results=options`
//!    with live data, so a train known to be cancelled is never picked;
//! 3. chooses (`regular_plan::choose`): the usual trains, else the usual
//!    operator within 5 minutes, else the target's best;
//! 4. in remind mode stores the choice and up to two alternatives
//!    (`suggested`) and commits nothing; in auto mode commits the choice
//!    over every leg with `journey_replace::replace` (`committed`; the
//!    journey is still being planned, so revision 1, the booked plan, is
//!    refreshed). A committed day is replaced again only when one of its
//!    trains no longer runs that day in the timetable (an STP cancellation
//!    or a CIF change caught by the morning check or a new publish); a
//!    train late or cancelled in live data is a break's concern (phase 4),
//!    never this sweep's;
//! 5. with no itinerary on a published day, stores `no_service` (the
//!    notifier's "needs attention" push reads it).
//!
//! The api never pushes. Off unless `REGULAR_PLAN_SWEEP_ENABLED` (the
//! chart's `api.regularJourneys.planSweep.enabled`, default false).

use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Timelike, Utc};
use sqlx::types::Json;

use crate::app::App;
use crate::data::journey_replace::{self, RegularWrite, Replaced, Replacement};
use crate::data::journey_revisions::{Reason, RevisionMeta};
use crate::data::regular_journeys::{self, PlanOptions, UsualItinerary};
use crate::data::regular_plan::{self, PlanOption, StoredPlan};
use crate::data::trip_planning_itinerary::TimeBound;
use crate::routes::trips::ReplanSlot;

/// The sweep's tunables, from the environment (chart
/// `api.regularJourneys.planSweep`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepConfig {
    /// `REGULAR_PLAN_SWEEP_ENABLED` (default false).
    pub enabled: bool,
    /// `REGULAR_PLAN_SWEEP_INTERVAL_SECS` (300).
    pub interval_secs: u64,
    /// `REGULAR_PLAN_SWEEP_MAX_OCCURRENCES_PER_TICK` (200).
    pub max_occurrences_per_tick: i64,
    /// `REGULAR_PLAN_SWEEP_MORNING_CHECK_HOUR` (5): London's hour of the
    /// morning check, 0 to 23.
    pub morning_check_hour: u32,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_secs: 300,
            max_occurrences_per_tick: 200,
            morning_check_hour: 5,
        }
    }
}

impl SweepConfig {
    pub fn from_env() -> Self {
        use crate::data::trip_plan_live::{env_bool, env_num};
        let default = Self::default();
        Self {
            enabled: env_bool("REGULAR_PLAN_SWEEP_ENABLED", default.enabled),
            interval_secs: env_num("REGULAR_PLAN_SWEEP_INTERVAL_SECS", default.interval_secs)
                .max(1),
            max_occurrences_per_tick: env_num(
                "REGULAR_PLAN_SWEEP_MAX_OCCURRENCES_PER_TICK",
                default.max_occurrences_per_tick,
            )
            .max(1),
            morning_check_hour: env_num(
                "REGULAR_PLAN_SWEEP_MORNING_CHECK_HOUR",
                default.morning_check_hour,
            )
            .min(23),
        }
    }
}

/// The sweep as a `ds_store::loops` loop, under its advisory lock.
pub fn loop_spec(app: App, config: &SweepConfig) -> ds_store::loops::LoopSpec {
    let config = config.clone();
    ds_store::loops::LoopSpec::new(
        ds_runtime::advisory_locks::REGULAR_PLAN_SWEEP,
        StdDuration::from_secs(config.interval_secs),
        move |_pool| {
            let app = app.clone();
            let config = config.clone();
            async move {
                let started = Instant::now();
                let result = run_sweep(&app, &config, Utc::now()).await;
                metrics::histogram!(ds_runtime::metrics::metric_name(
                    "api_regular_plan_sweep_seconds"
                ))
                .record(started.elapsed().as_secs_f64());
                match result {
                    Ok(swept) => {
                        if swept.occurrences > 0 {
                            tracing::info!(?swept, "regular plan sweep");
                        }
                        Ok(())
                    }
                    Err(err) => {
                        tracing::error!(error = ?err, "regular plan sweep failed; will retry next interval");
                        Err(err)
                    }
                }
            }
        },
    )
}

/// What one tick did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Swept {
    pub occurrences: usize,
    pub suggested: usize,
    pub committed: usize,
    pub kept: usize,
    pub no_service: usize,
    pub pending: usize,
    pub skipped: usize,
}

/// What the sweep did with one occurrence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Remind mode: the choice and alternatives stored.
    Suggested,
    /// Auto mode: the choice committed over the legs.
    Committed,
    /// Auto mode, already committed and every train still runs: the plan
    /// and marker refreshed, the legs left alone.
    Kept,
    /// No itinerary on a published day.
    NoService,
    /// Nothing decided: no timetable for the day, a target already past,
    /// or options the sweep can't plan.
    Pending,
    /// The journey changed under the sweep (a pick, a break, a start):
    /// nothing written.
    Skipped,
}

impl Outcome {
    fn label(self) -> &'static str {
        match self {
            Outcome::Suggested => "suggested",
            Outcome::Committed => "committed",
            Outcome::Kept => "kept",
            Outcome::NoService => "no_service",
            Outcome::Pending => "pending",
            Outcome::Skipped => "skipped",
        }
    }
}

/// One due occurrence, as the candidate query reads it.
#[derive(Debug, sqlx::FromRow)]
struct Candidate {
    journey_id: i64,
    user_id: String,
    service_date: NaiveDate,
    default_match_mode: String,
    origin_crs: String,
    destination_crs: String,
    target_kind: String,
    target_time: NaiveTime,
    target_day_offset: i16,
    plan_options: Option<Json<serde_json::Value>>,
    usual_itinerary: Option<Json<serde_json::Value>>,
    regular_plan_status: Option<String>,
    regular_planned_at: Option<DateTime<Utc>>,
}

/// The candidates: due occurrences of target templates dated today or
/// tomorrow (London), oldest service date first. On a weekday with a
/// different time (R3, `data::day_overrides`) the target and usual
/// itinerary are that day's; the stations and plan options are the
/// template's. `$1` now, `$2` London's
/// today, `$3` the schedule marker, `$4` the morning check hour, `$5` the
/// limit.
fn candidates_sql() -> String {
    let started = regular_journeys::occurrence_started_sql("j", "$1", "$2");
    format!(
        "SELECT j.id AS journey_id, j.user_id, d.service_date, jt.default_match_mode, \
                jt.origin_crs, jt.destination_crs, \
                COALESCE(o.target_kind, jt.target_kind) AS target_kind, \
                COALESCE(o.target_time, jt.target_time) AS target_time, \
                COALESCE(o.target_day_offset, jt.target_day_offset) AS target_day_offset, \
                jt.plan_options, \
                CASE WHEN o.template_id IS NOT NULL THEN o.usual_itinerary \
                     ELSE jt.usual_itinerary END AS usual_itinerary, \
                j.regular_plan_status, j.regular_planned_at \
         FROM journey_templates jt \
         JOIN journeys j ON j.source_template_id = jt.id AND j.user_id = jt.user_id \
         CROSS JOIN LATERAL (SELECT MIN(l.service_date) AS service_date \
                             FROM journey_legs l WHERE l.journey_id = j.id) d \
         LEFT JOIN journey_template_day_overrides o \
                ON o.template_id = jt.id \
               AND o.weekday = EXTRACT(ISODOW FROM d.service_date)::smallint \
         WHERE jt.target_kind IS NOT NULL \
           AND d.service_date BETWEEN $2 AND $2 + 1 \
           AND NOT EXISTS (SELECT 1 FROM journey_breaks b WHERE b.journey_id = j.id) \
           AND NOT EXISTS (SELECT 1 FROM journey_revisions r \
                           WHERE r.journey_id = j.id AND r.revision_no > 1) \
           AND NOT EXISTS (SELECT 1 FROM journey_legs m WHERE m.journey_id = j.id \
                           AND m.kind = 'train' AND m.match_mode = 'manual') \
           AND NOT {started} \
           AND (j.regular_plan_status IS NULL \
                OR j.regular_plan_marker IS DISTINCT FROM $3 \
                OR (d.service_date = $2 \
                    AND $1 >= (d.service_date + make_time($4, 0, 0)) AT TIME ZONE 'Europe/London' \
                    AND j.regular_planned_at < \
                        (d.service_date + make_time($4, 0, 0)) AT TIME ZONE 'Europe/London')) \
         ORDER BY d.service_date, j.id LIMIT $5"
    )
}

/// One tick (see the module doc), as of `now`. An occurrence that fails is
/// logged and skipped; the tick goes on.
pub async fn run_sweep(
    app: &App,
    config: &SweepConfig,
    now: DateTime<Utc>,
) -> anyhow::Result<Swept> {
    let today = regular_journeys::london_date(now);
    let marker = crate::routes::trips::schedule_marker(&app.database).await?;
    let candidates: Vec<Candidate> = sqlx::query_as(&candidates_sql())
        .bind(now)
        .bind(today)
        .bind(&marker)
        .bind(i32::try_from(config.morning_check_hour).unwrap_or(5))
        .bind(config.max_occurrences_per_tick)
        .fetch_all(&app.database)
        .await?;
    let mut swept = Swept::default();
    for candidate in candidates {
        swept.occurrences += 1;
        let journey_id = candidate.journey_id;
        let outcome = match plan_occurrence(app, &candidate, &marker, now).await {
            Ok(outcome) => outcome,
            Err(err) => {
                tracing::warn!(error = ?err, journey_id, "regular plan sweep: one occurrence failed");
                metrics::counter!(
                    ds_runtime::metrics::metric_name("api_regular_plan_total"),
                    "result" => "error"
                )
                .increment(1);
                continue;
            }
        };
        metrics::counter!(
            ds_runtime::metrics::metric_name("api_regular_plan_total"),
            "result" => outcome.label()
        )
        .increment(1);
        match outcome {
            Outcome::Suggested => swept.suggested += 1,
            Outcome::Committed => swept.committed += 1,
            Outcome::Kept => swept.kept += 1,
            Outcome::NoService => swept.no_service += 1,
            Outcome::Pending => swept.pending += 1,
            Outcome::Skipped => swept.skipped += 1,
        }
    }
    Ok(swept)
}

/// The target as an instant, `None` in the missing hour of a spring clock
/// change (that day's target is skipped, as the notifier's stage 2 does).
fn target_instant(date: NaiveDate, time: NaiveTime, day_offset: i16) -> Option<DateTime<Utc>> {
    let day = date + Duration::days(i64::from(day_offset));
    chrono_tz::Europe::London
        .from_local_datetime(&day.and_time(time))
        .earliest()
        .map(|at| at.with_timezone(&Utc))
}

/// Records `pending` (with the marker, so the occurrence waits for the next
/// publish or the morning check rather than every tick) unless the
/// occurrence already has a plan, which is then left as it is.
async fn mark_pending(
    app: &App,
    candidate: &Candidate,
    marker: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Outcome> {
    sqlx::query(
        "UPDATE journeys SET regular_plan_status = 'pending', regular_plan_marker = $2, \
                regular_planned_at = $3 \
         WHERE id = $1 AND (regular_plan_status IS NULL OR regular_plan_status = 'pending')",
    )
    .bind(candidate.journey_id)
    .bind(marker)
    .bind(now)
    .execute(&app.database)
    .await?;
    Ok(Outcome::Pending)
}

/// Writes a plan that commits nothing, if the occurrence's plan is still
/// the one read (`regular_planned_at`). `Skipped` when it isn't.
async fn store(
    app: &App,
    candidate: &Candidate,
    plan: &StoredPlan,
    status: &str,
    marker: &str,
    now: DateTime<Utc>,
    outcome: Outcome,
) -> anyhow::Result<Outcome> {
    let written = sqlx::query(
        "UPDATE journeys SET regular_plan = $2, regular_plan_status = $3, \
                regular_plan_marker = $4, regular_planned_at = $5 \
         WHERE id = $1 AND regular_planned_at IS NOT DISTINCT FROM $6 \
           AND NOT EXISTS (SELECT 1 FROM journey_legs m WHERE m.journey_id = $1 \
                           AND m.kind = 'train' AND m.match_mode = 'manual')",
    )
    .bind(candidate.journey_id)
    .bind(Json(plan))
    .bind(status)
    .bind(marker)
    .bind(now)
    .bind(candidate.regular_planned_at)
    .execute(&app.database)
    .await?;
    Ok(if written.rows_affected() == 1 {
        outcome
    } else {
        Outcome::Skipped
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "linear: the steps of the module doc, in order"
)]
async fn plan_occurrence(
    app: &App,
    candidate: &Candidate,
    marker: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Outcome> {
    let date = candidate.service_date;
    let Some(target_at) = target_instant(date, candidate.target_time, candidate.target_day_offset)
    else {
        return mark_pending(app, candidate, marker, now).await;
    };
    // A target already behind us: nothing to plan for (an arrive-by can
    // still be planned until then).
    if target_at <= now {
        return mark_pending(app, candidate, marker, now).await;
    }
    if !crate::data::queries::schedule_destination_departures_published_for(&app.database, date)
        .await?
    {
        return mark_pending(app, candidate, marker, now).await;
    }
    let options: PlanOptions =
        regular_journeys::decode_stored(candidate.plan_options.clone()).unwrap_or_default();
    let usual: Option<UsualItinerary> =
        regular_journeys::decode_stored(candidate.usual_itinerary.clone());
    let minutes = candidate.target_time.hour() * 60 + candidate.target_time.minute();
    let time = if candidate.target_kind == "arrive_by" {
        TimeBound::ArriveBy(minutes)
    } else {
        TimeBound::DepartAfter(minutes)
    };
    let query = match crate::routes::trips::regular_plan_query(
        app,
        &candidate.origin_crs,
        &candidate.destination_crs,
        date,
        time,
        u8::try_from(candidate.target_day_offset).unwrap_or(0),
        &options,
    )
    .await
    {
        Ok(query) => query,
        Err(reason) => {
            tracing::info!(
                journey_id = candidate.journey_id,
                reason,
                "regular plan sweep: the plan options can't be planned"
            );
            return mark_pending(app, candidate, marker, now).await;
        }
    };
    let Some(itineraries) =
        crate::routes::regular_plan::departures(app, query, ReplanSlot::Regular)
            .await
            .map_err(|(status, message)| anyhow::anyhow!("plan failed ({status}): {message}"))?
    else {
        // No graph for the date: unpublished after all.
        return mark_pending(app, candidate, marker, now).await;
    };
    let planned = crate::routes::regular_plan::plan_options(app, &itineraries, date, now).await;
    let Some((choice, reason, alternatives)) =
        regular_plan::choose(usual.as_ref(), &candidate.target_kind, date, &planned)
    else {
        if !itineraries.is_empty() {
            // Every way there leaves too soon, or is cancelled today:
            // nothing to decide (a cancellation is a break's concern).
            return mark_pending(app, candidate, marker, now).await;
        }
        return store(
            app,
            candidate,
            &StoredPlan::default(),
            "no_service",
            marker,
            now,
            Outcome::NoService,
        )
        .await;
    };
    let pick = |index: usize| -> PlanOption { planned[index].clone() };
    let plan = StoredPlan {
        reason: Some(reason),
        choice: Some(pick(choice)),
        alternatives: alternatives.into_iter().map(pick).collect(),
        sweep_leg_ids: Vec::new(),
        accepted_index: None,
    };
    if candidate.default_match_mode != "auto" {
        return store(
            app,
            candidate,
            &plan,
            "suggested",
            marker,
            now,
            Outcome::Suggested,
        )
        .await;
    }

    // Auto mode. Already committed by the sweep, and every train still
    // runs: keep the legs, refresh the plan.
    if candidate.regular_plan_status.as_deref() == Some("committed")
        && let Some(trains) =
            regular_plan::committed_trains(&app.database, candidate.journey_id).await?
        && !trains.is_empty()
        && regular_plan::trains_run(&app.database, &trains).await?
    {
        let (stored, ..) = regular_plan::read_stored(&app.database, candidate.journey_id).await?;
        let kept = StoredPlan {
            sweep_leg_ids: stored.map(|s| s.sweep_leg_ids).unwrap_or_default(),
            ..plan
        };
        return store(
            app,
            candidate,
            &kept,
            "committed",
            marker,
            now,
            Outcome::Kept,
        )
        .await;
    }
    let chosen = plan.choice.clone().unwrap_or_else(|| pick(choice));
    let mut trains = std::collections::HashMap::new();
    for (uid, train_date) in chosen.train_key() {
        let id = crate::data::trains::find_or_create_train(&app.database, &uid, train_date).await?;
        trains.insert((uid, train_date), id);
    }
    let Some(legs) = chosen.itinerary_legs(&trains) else {
        anyhow::bail!("a chosen train was not resolved");
    };
    let leg_ids: Vec<(i64,)> =
        sqlx::query_as("SELECT id FROM journey_legs WHERE journey_id = $1 ORDER BY leg_order")
            .bind(candidate.journey_id)
            .fetch_all(&app.database)
            .await?;
    let (Some((first,)), Some((last,))) = (leg_ids.first(), leg_ids.last()) else {
        return Ok(Outcome::Skipped);
    };
    let outcome = journey_replace::replace(
        &app.database,
        &Replacement {
            journey_id: candidate.journey_id,
            user_id: &candidate.user_id,
            from_leg_id: *first,
            through_leg_id: Some(*last),
            legs: &legs,
            meta: RevisionMeta {
                reason: Reason::Replace,
                cause: "regular_plan".to_string(),
                fault: "none".to_string(),
                break_id: None,
                actor: "system",
            },
            trigger_fate: "not_used",
            now,
            idempotency: None,
            regular: Some(RegularWrite {
                plan: &plan,
                status: "committed",
                marker: Some(marker),
                read_planned_at: candidate.regular_planned_at,
                sweep: true,
            }),
        },
    )
    .await?;
    Ok(match outcome {
        Replaced::Done { .. } => Outcome::Committed,
        _ => Outcome::Skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sweep_is_off_unless_turned_on() {
        let config = SweepConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.interval_secs, 300);
        assert_eq!(config.max_occurrences_per_tick, 200);
        assert_eq!(config.morning_check_hour, 5);
    }

    #[test]
    fn a_target_in_the_missing_spring_hour_has_no_instant() {
        let date = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap();
        assert!(target_instant(date, NaiveTime::from_hms_opt(1, 30, 0).unwrap(), 0).is_none());
        // In the repeated October hour, the earlier instant (BST) wins.
        let date = NaiveDate::from_ymd_opt(2026, 10, 25).unwrap();
        let at = target_instant(date, NaiveTime::from_hms_opt(1, 30, 0).unwrap(), 0).unwrap();
        assert_eq!(at.to_rfc3339(), "2026-10-25T00:30:00+00:00");
        // A day offset of 1 is the next day's clock.
        let at = target_instant(date, NaiveTime::from_hms_opt(0, 30, 0).unwrap(), 1).unwrap();
        assert_eq!(at.to_rfc3339(), "2026-10-26T00:30:00+00:00");
    }
}

#[cfg(test)]
#[path = "regular_plan_sweep_db_tests.rs"]
mod db_tests;
