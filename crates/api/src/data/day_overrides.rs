//! A regular journey's different time on some days (the scheduling
//! options' R3, ds-core v0.13.0 and v0.14.0): for one ISO weekday (1
//! Monday to 7 Sunday) of a template with a target, that day's own target
//! and usual itinerary (`journey_template_day_overrides`), and the template
//! legs derived from it (`journey_template_legs.variant_weekday`). Days
//! without one use the template's own.
//!
//! - The notifier's mint takes a date's variant legs when it has any, else
//!   the template's own (`ds_store::recurrence::leg_variant_sql`), and so
//!   does "Run now" ([`crate::data::journey_templates::materialize_template`]).
//! - The plan sweep plans an occurrence on that weekday with the override's
//!   target and usual itinerary (`regular_plan_sweep::candidates_sql`).
//! - The pick reminder's "usual train" link reads the override's itinerary
//!   (ds-workers#20).
//! - Phase 4's rerouting must read the override too.
//!
//! An override belongs to one template: a return journey has its own.
//!
//! Off unless `REGULAR_JOURNEY_DAY_OVERRIDES_ENABLED` (the chart's
//! `api.regularJourneys.dayOverrides.enabled`): until then a request that
//! sets one is refused. A notifier that doesn't know `variant_weekday`
//! would mint the template's own legs and the variant legs into one
//! journey, so the flag goes on only once the variant-aware notifier
//! (ds-workers eac006c, core v0.13.0) is deployed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::NaiveTime;
use serde::{Deserialize, Serialize};
use sqlx::types::Json;

use crate::data::journey_templates::TemplateLegInput;
use crate::data::regular_journeys::{self, TemplateTarget, UsualItinerary};

/// The environment variable that turns the overrides on.
pub const DAY_OVERRIDES_ENABLED_ENV: &str = "REGULAR_JOURNEY_DAY_OVERRIDES_ENABLED";

/// The message a request that sets an override gets while they are off.
pub const NOT_AVAILABLE_MESSAGE: &str = "A different time on some days isn't available yet.";

static ENABLED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static ENABLED_FOR_TEST: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Reads [`DAY_OVERRIDES_ENABLED_ENV`] (default false) once, at startup,
/// and returns it.
pub fn init_from_env() -> bool {
    let enabled = crate::data::trip_plan_live::env_bool(DAY_OVERRIDES_ENABLED_ENV, false);
    ENABLED.store(enabled, Ordering::Relaxed);
    enabled
}

/// Whether the overrides are on ([`init_from_env`]).
pub fn enabled() -> bool {
    #[cfg(test)]
    if let Some(enabled) = ENABLED_FOR_TEST.with(std::cell::Cell::get) {
        return enabled;
    }
    ENABLED.load(Ordering::Relaxed)
}

/// Turns the overrides on (or off) for this test thread until the guard
/// is dropped, as `recurrence_pattern::set_enabled_for_tests` does.
#[cfg(test)]
#[must_use = "the setting is lifted as soon as the guard is dropped"]
pub(crate) fn set_enabled_for_tests(enabled: bool) -> EnabledForTests {
    ENABLED_FOR_TEST.with(|cell| cell.set(Some(enabled)));
    EnabledForTests(())
}

/// Guard returned by [`set_enabled_for_tests`].
#[cfg(test)]
pub(crate) struct EnabledForTests(());

#[cfg(test)]
impl Drop for EnabledForTests {
    fn drop(&mut self) {
        ENABLED_FOR_TEST.with(|cell| cell.set(None));
    }
}

/// One weekday's different time, as a create or a `PUT` gives it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct DayOverrideRequest {
    /// ISO weekday: 1 Monday to 7 Sunday (not `daysOfWeek`'s bits). An
    /// override for a day the journey doesn't run on has no effect.
    pub weekday: i16,
    /// That day's target.
    pub target: TemplateTarget,
    /// The train picked for that day's target, as `usualItinerary`: the
    /// day's template legs come from it.
    pub usual_itinerary: UsualItinerary,
}

/// One weekday's different time, as `GET /JourneyTemplates/{id}` serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct DayOverride {
    /// ISO weekday: 1 Monday to 7 Sunday.
    pub weekday: i16,
    pub target: TemplateTarget,
    pub usual_itinerary: Option<UsualItinerary>,
}

/// One weekday's different time as the list serves it: no itinerary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct DayOverrideSummary {
    /// ISO weekday: 1 Monday to 7 Sunday.
    pub weekday: i16,
    pub target: TemplateTarget,
}

impl From<DayOverride> for DayOverrideSummary {
    fn from(value: DayOverride) -> Self {
        Self {
            weekday: value.weekday,
            target: value.target,
        }
    }
}

/// The English name of ISO weekday `weekday` (1 to 7), for messages.
#[must_use]
pub fn weekday_name(weekday: i16) -> &'static str {
    match weekday {
        1 => "Monday",
        2 => "Tuesday",
        3 => "Wednesday",
        4 => "Thursday",
        5 => "Friday",
        6 => "Saturday",
        _ => "Sunday",
    }
}

/// Checks a request's overrides without the database: refused while off
/// (unless there are none), at most one per weekday, weekdays 1 to 7, each
/// target valid and each itinerary valid (normalised). Returns them in
/// weekday order. The itineraries' calls are checked by the caller
/// (`regular_journeys::check_itinerary_calls`).
pub fn validate(overrides: &[DayOverrideRequest]) -> Result<Vec<DayOverrideRequest>, String> {
    if overrides.is_empty() {
        return Ok(Vec::new());
    }
    if !enabled() {
        return Err(NOT_AVAILABLE_MESSAGE.to_string());
    }
    let mut seen = [false; 8];
    let mut validated = Vec::with_capacity(overrides.len());
    for item in overrides {
        let Some(slot) = usize::try_from(item.weekday)
            .ok()
            .filter(|day| (1..=7).contains(day))
        else {
            return Err("A different time needs a weekday from 1 (Monday) to 7 (Sunday).".into());
        };
        if seen[slot] {
            return Err(format!(
                "{} has more than one different time.",
                weekday_name(item.weekday)
            ));
        }
        seen[slot] = true;
        let day = weekday_name(item.weekday);
        regular_journeys::validate_target(&item.target).map_err(|err| format!("{day}: {err}"))?;
        let usual_itinerary = regular_journeys::validate_usual_itinerary(&item.usual_itinerary)
            .map_err(|err| format!("{day}: {err}"))?;
        validated.push(DayOverrideRequest {
            weekday: item.weekday,
            target: item.target.clone(),
            usual_itinerary,
        });
    }
    validated.sort_by_key(|item| item.weekday);
    Ok(validated)
}

/// Replaces template `template_id`'s overrides and their variant legs with
/// `overrides` (already validated), inside the caller's transaction. Each
/// override's legs come from its itinerary
/// (`regular_journeys::template_legs_from_itinerary`), numbered from 1.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    reason = "a template has a handful of legs"
)]
pub async fn store(
    conn: &mut sqlx::PgConnection,
    template_id: i64,
    overrides: &[DayOverrideRequest],
) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM journey_template_legs WHERE template_id = $1 AND variant_weekday IS NOT NULL",
    )
    .bind(template_id)
    .execute(&mut *conn)
    .await?;
    sqlx::query("DELETE FROM journey_template_day_overrides WHERE template_id = $1")
        .bind(template_id)
        .execute(&mut *conn)
        .await?;
    for item in overrides {
        sqlx::query(
            "INSERT INTO journey_template_day_overrides \
                (template_id, weekday, target_kind, target_time, target_day_offset, \
                 usual_itinerary) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(template_id)
        .bind(item.weekday)
        .bind(&item.target.kind)
        .bind(item.target.time)
        .bind(item.target.day_offset)
        .bind(Json(&item.usual_itinerary))
        .execute(&mut *conn)
        .await?;
        for (index, leg) in regular_journeys::template_legs_from_itinerary(&item.usual_itinerary)
            .iter()
            .enumerate()
        {
            insert_variant_leg(conn, template_id, item.weekday, (index + 1) as i32, leg).await?;
        }
    }
    Ok(())
}

async fn insert_variant_leg(
    conn: &mut sqlx::PgConnection,
    template_id: i64,
    weekday: i16,
    leg_order: i32,
    leg: &TemplateLegInput,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO journey_template_legs \
            (template_id, leg_order, origin_crs, destination_crs, \
             depart_after, depart_before, arrive_after, arrive_before, variant_weekday) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(template_id)
    .bind(leg_order)
    .bind(&leg.origin_crs)
    .bind(&leg.destination_crs)
    .bind(leg.depart_after)
    .bind(leg.depart_before)
    .bind(leg.arrive_after)
    .bind(leg.arrive_before)
    .bind(weekday)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The overrides of each of `template_ids`, weekday order. A template
/// without any is absent.
pub async fn load<'c, E>(
    executor: E,
    template_ids: &[i64],
) -> anyhow::Result<HashMap<i64, Vec<DayOverride>>>
where
    E: sqlx::PgExecutor<'c>,
{
    #[expect(
        clippy::type_complexity,
        reason = "the tuple mirrors the columns of the SQL row it decodes"
    )]
    let rows: Vec<(
        i64,
        i16,
        String,
        NaiveTime,
        i16,
        Option<Json<serde_json::Value>>,
    )> = sqlx::query_as(
        "SELECT template_id, weekday, target_kind, target_time, target_day_offset, \
                usual_itinerary \
         FROM journey_template_day_overrides WHERE template_id = ANY($1) \
         ORDER BY template_id, weekday",
    )
    .bind(template_ids)
    .fetch_all(executor)
    .await?;
    let mut out: HashMap<i64, Vec<DayOverride>> = HashMap::new();
    for (template_id, weekday, kind, time, day_offset, itinerary) in rows {
        out.entry(template_id).or_default().push(DayOverride {
            weekday,
            target: TemplateTarget {
                kind,
                time,
                day_offset,
            },
            usual_itinerary: regular_journeys::decode_stored(itinerary),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(weekday: i16) -> DayOverrideRequest {
        DayOverrideRequest {
            weekday,
            target: TemplateTarget {
                kind: "depart_after".to_string(),
                time: NaiveTime::from_hms_opt(7, 15, 0).unwrap(),
                day_offset: 0,
            },
            usual_itinerary: UsualItinerary {
                legs: vec![
                    serde_json::from_value(serde_json::json!({
                        "trainUid": "C12345",
                        "originCrs": "gld",
                        "destinationCrs": "WAT",
                        "departs": "07:21"
                    }))
                    .unwrap(),
                ],
            },
        }
    }

    #[test]
    fn none_is_fine_while_off() {
        let _off = set_enabled_for_tests(false);
        assert_eq!(validate(&[]), Ok(Vec::new()));
        assert_eq!(validate(&[item(5)]), Err(NOT_AVAILABLE_MESSAGE.to_string()));
    }

    #[test]
    fn weekdays_are_iso_distinct_and_sorted() {
        let _on = set_enabled_for_tests(true);
        let got = validate(&[item(7), item(1), item(5)]).unwrap();
        assert_eq!(got.iter().map(|o| o.weekday).collect::<Vec<_>>(), [1, 5, 7]);
        // The itinerary is normalised.
        assert_eq!(
            got[0].usual_itinerary.legs[0].origin_crs.as_deref(),
            Some("GLD")
        );
        for bad in [0, 8, -1] {
            assert!(validate(&[item(bad)]).is_err(), "{bad}");
        }
        assert_eq!(
            validate(&[item(5), item(5)]),
            Err("Friday has more than one different time.".to_string())
        );
    }

    #[test]
    fn the_target_and_itinerary_are_checked() {
        let _on = set_enabled_for_tests(true);
        let mut bad_target = item(5);
        bad_target.target.kind = "leave_at".to_string();
        assert!(validate(&[bad_target]).unwrap_err().starts_with("Friday: "));
        let mut bad_offset = item(2);
        bad_offset.target.day_offset = 2;
        assert!(
            validate(&[bad_offset])
                .unwrap_err()
                .starts_with("Tuesday: ")
        );
        let mut no_legs = item(3);
        no_legs.usual_itinerary.legs.clear();
        assert!(validate(&[no_legs]).unwrap_err().starts_with("Wednesday: "));
    }

    #[test]
    fn weekday_names() {
        assert_eq!(
            (1..=7).map(weekday_name).collect::<Vec<_>>(),
            [
                "Monday",
                "Tuesday",
                "Wednesday",
                "Thursday",
                "Friday",
                "Saturday",
                "Sunday"
            ]
        );
    }
}
