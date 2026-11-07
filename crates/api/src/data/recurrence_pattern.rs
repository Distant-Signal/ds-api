//! A regular journey's recurrence pattern beyond its weekdays (the
//! scheduling options' R2, ds-core v0.12.0): its kind (`weekly`, or
//! `dates` for a list of dates), every how many weeks it runs from which
//! week, and the extra dates it also runs on. `ds_store::recurrence`
//! decides the dates from them, for the notifier's mint and the api's
//! `nextDate` and `upcomingDates` alike.
//!
//! Off unless `REGULAR_JOURNEY_RECURRENCE_ENABLED` (the chart's
//! `api.regularJourneys.recurrence.enabled`): until then a request that
//! sets any of it is refused, and every template stays weekly, every week.
//! The notifier that understands the kinds ships first (ds-workers#17),
//! so no kind the mint can't read is ever written.

use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{Duration, NaiveDate};
use serde::{Deserialize, Serialize};

/// The environment variable that turns the patterns on.
pub const RECURRENCE_ENABLED_ENV: &str = "REGULAR_JOURNEY_RECURRENCE_ENABLED";

static ENABLED: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static ENABLED_FOR_TEST: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Reads [`RECURRENCE_ENABLED_ENV`] (default false) once, at startup, and
/// returns it.
pub fn init_from_env() -> bool {
    let enabled = crate::data::trip_plan_live::env_bool(RECURRENCE_ENABLED_ENV, false);
    ENABLED.store(enabled, Ordering::Relaxed);
    enabled
}

/// Whether the patterns are on ([`init_from_env`]).
pub fn enabled() -> bool {
    #[cfg(test)]
    if let Some(enabled) = ENABLED_FOR_TEST.with(std::cell::Cell::get) {
        return enabled;
    }
    ENABLED.load(Ordering::Relaxed)
}

/// Turns the patterns on (or off) for this test thread until the guard is
/// dropped, as `routes::pin_london_now_for_tests` pins the clock.
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

/// The most extra dates a template may have.
pub const MAX_EXTRA_DATES: usize = 100;
/// How far ahead an extra date may be, in days from London's today: the
/// evaluator's horizon (`ds_store::recurrence::DEFAULT_HORIZON_DAYS`).
pub const MAX_EXTRA_DATE_DAYS_AHEAD: i64 = 400;

/// A pattern as a request may give it: each field optional. On a create
/// an absent field is the default (weekly, every week, no anchor, no extra
/// dates); on a `PUT` it keeps the stored value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(optional_fields = nullable)]
pub struct PatternRequest {
    /// `"weekly"` (the weekdays, the default) or `"dates"` (only
    /// `extraDates`).
    #[serde(default)]
    #[ts(as = "Option<crate::ts_values::RecurrenceKind>")]
    pub recurrence_kind: Option<String>,
    /// Every this many weeks, 1 to 4 (default 1). Above 1 needs
    /// `anchorWeek`.
    #[serde(default)]
    pub interval_weeks: Option<i16>,
    /// A day of a week that runs ("Starting this week" sends this week's
    /// Monday). Stored as that week's Monday.
    #[serde(default)]
    pub anchor_week: Option<NaiveDate>,
    /// Dates it also runs on (for `"dates"`, its only dates), from today
    /// to 400 days ahead, at most 100. Replaces the stored list.
    #[serde(default)]
    pub extra_dates: Option<Vec<NaiveDate>>,
}

impl PatternRequest {
    /// Whether the request sets nothing of the pattern.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.recurrence_kind.is_none()
            && self.interval_weeks.is_none()
            && self.anchor_week.is_none()
            && self.extra_dates.is_none()
    }
}

/// A validated pattern, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pattern {
    pub kind: String,
    pub interval_weeks: i16,
    pub anchor_week: Option<NaiveDate>,
    pub extra_dates: Vec<NaiveDate>,
}

impl Default for Pattern {
    fn default() -> Self {
        Self {
            kind: ds_store::recurrence::KIND_WEEKLY.to_string(),
            interval_weeks: 1,
            anchor_week: None,
            extra_dates: Vec::new(),
        }
    }
}

/// The message for a pattern while the patterns are off.
pub const NOT_AVAILABLE_MESSAGE: &str =
    "Repeating every few weeks, or on dates you choose, isn't available yet.";

/// `request` laid over `base` (the default on a create, the stored pattern
/// on a `PUT`), checked against London's `today`. `Ok(None)` when the
/// request sets nothing, so nothing changes.
///
/// # Errors
///
/// The message to show: the patterns are off; an unknown kind; an
/// interval outside 1 to 4, or above 1 with no anchor; a list of dates
/// with an interval or no dates; an extra date in the past, too far
/// ahead, or too many.
pub fn resolve(
    request: &PatternRequest,
    base: &Pattern,
    today: NaiveDate,
) -> Result<Option<Pattern>, String> {
    use ds_store::recurrence::{KIND_DATES, KIND_WEEKLY, MAX_INTERVAL_WEEKS, monday_of};

    if request.is_empty() {
        return Ok(None);
    }
    if !enabled() {
        return Err(NOT_AVAILABLE_MESSAGE.to_string());
    }
    let kind = request
        .recurrence_kind
        .clone()
        .unwrap_or_else(|| base.kind.clone());
    if kind != KIND_WEEKLY && kind != KIND_DATES {
        return Err("recurrenceKind must be \"weekly\" or \"dates\".".to_string());
    }
    let interval_weeks = request.interval_weeks.unwrap_or(base.interval_weeks);
    if !(1..=MAX_INTERVAL_WEEKS).contains(&interval_weeks) {
        return Err(format!("Choose every 1 to {MAX_INTERVAL_WEEKS} weeks."));
    }
    let anchor_week = request.anchor_week.or(base.anchor_week).map(monday_of);
    let mut extra_dates = request
        .extra_dates
        .clone()
        .unwrap_or_else(|| base.extra_dates.clone());
    extra_dates.sort_unstable();
    extra_dates.dedup();
    if kind == KIND_DATES {
        if interval_weeks != 1 {
            return Err("A list of dates doesn't repeat every few weeks.".to_string());
        }
        if extra_dates.is_empty() {
            return Err("Choose at least one date.".to_string());
        }
    } else if interval_weeks > 1 && anchor_week.is_none() {
        return Err("Choose which week it starts.".to_string());
    }
    if extra_dates.len() > MAX_EXTRA_DATES {
        return Err(format!("Choose up to {MAX_EXTRA_DATES} dates."));
    }
    if request.extra_dates.is_some() {
        if extra_dates.first().is_some_and(|first| *first < today) {
            return Err("Choose dates from today on.".to_string());
        }
        let latest = today + Duration::days(MAX_EXTRA_DATE_DAYS_AHEAD);
        if extra_dates.last().is_some_and(|last| *last > latest) {
            return Err("Choose dates up to a year ahead.".to_string());
        }
    }
    Ok(Some(Pattern {
        anchor_week: if kind == KIND_WEEKLY && interval_weeks > 1 {
            anchor_week
        } else {
            None
        },
        kind,
        interval_weeks,
        extra_dates,
    }))
}

/// Template `template_id`'s stored pattern, its extra dates from `today`
/// on. `None` when it doesn't exist.
///
/// # Errors
///
/// The query's.
pub async fn stored<'c, E>(
    executor: E,
    template_id: i64,
    today: NaiveDate,
) -> anyhow::Result<Option<Pattern>>
where
    E: sqlx::PgExecutor<'c>,
{
    let row: Option<(String, i16, Option<NaiveDate>, Vec<NaiveDate>)> = sqlx::query_as(
        "SELECT jt.recurrence_kind, jt.interval_weeks, jt.anchor_week, \
                ARRAY(SELECT e.service_date FROM journey_template_extra_dates e \
                      WHERE e.template_id = jt.id AND e.service_date >= $2 \
                      ORDER BY e.service_date) \
         FROM journey_templates jt WHERE jt.id = $1",
    )
    .bind(template_id)
    .bind(today)
    .fetch_optional(executor)
    .await?;
    Ok(
        row.map(|(kind, interval_weeks, anchor_week, extra_dates)| Pattern {
            kind,
            interval_weeks,
            anchor_week,
            extra_dates,
        }),
    )
}

/// Writes `pattern` to every template in `template_ids` (a template and
/// its pair share their recurrence, decision 2): the kind, interval and
/// anchor, and the extra dates, which replace the stored ones.
///
/// # Errors
///
/// The queries'.
pub async fn store(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    template_ids: &[i64],
    pattern: &Pattern,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE journey_templates SET recurrence_kind = $2, interval_weeks = $3, \
                anchor_week = $4, updated_at = NOW() \
         WHERE id = ANY($1)",
    )
    .bind(template_ids)
    .bind(&pattern.kind)
    .bind(pattern.interval_weeks)
    .bind(pattern.anchor_week)
    .execute(&mut **tx)
    .await?;
    sqlx::query("DELETE FROM journey_template_extra_dates WHERE template_id = ANY($1)")
        .bind(template_ids)
        .execute(&mut **tx)
        .await?;
    sqlx::query(
        "INSERT INTO journey_template_extra_dates (template_id, service_date) \
         SELECT t, d FROM unnest($1::bigint[]) AS t CROSS JOIN unnest($2::date[]) AS d",
    )
    .bind(template_ids)
    .bind(&pattern.extra_dates)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The rule a preview evaluates: `days_of_week` with `pattern`, from
/// `starts_on` to `ends_on`.
#[must_use]
pub fn preview_rule(
    days_of_week: Option<i16>,
    starts_on: Option<NaiveDate>,
    ends_on: Option<NaiveDate>,
    pattern: &Pattern,
) -> ds_store::recurrence::Rule {
    let mut rule = ds_store::recurrence::Rule::weekly(days_of_week);
    rule.kind = ds_store::recurrence::Kind::parse(&pattern.kind);
    rule.interval_weeks = pattern.interval_weeks;
    rule.anchor_week = pattern.anchor_week;
    rule.starts_on = starts_on;
    rule.ends_on = ends_on;
    rule.extra_dates = pattern.extra_dates.iter().copied().collect();
    rule
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, m, day).unwrap()
    }

    fn request() -> PatternRequest {
        PatternRequest::default()
    }

    #[test]
    fn nothing_set_changes_nothing_whether_on_or_off() {
        let _off = set_enabled_for_tests(false);
        assert_eq!(resolve(&request(), &Pattern::default(), d(11, 7)), Ok(None));
    }

    #[test]
    fn off_refuses_any_pattern() {
        let _off = set_enabled_for_tests(false);
        let every_other = PatternRequest {
            interval_weeks: Some(2),
            anchor_week: Some(d(11, 9)),
            ..request()
        };
        assert_eq!(
            resolve(&every_other, &Pattern::default(), d(11, 7)),
            Err(NOT_AVAILABLE_MESSAGE.to_string())
        );
    }

    #[test]
    fn every_other_week_needs_a_week_and_stores_its_monday() {
        let _on = set_enabled_for_tests(true);
        let today = d(11, 7);
        let base = Pattern::default();
        let mut req = PatternRequest {
            interval_weeks: Some(2),
            ..request()
        };
        assert!(resolve(&req, &base, today).is_err());
        // Thursday 12 Nov: the week of Monday 9 Nov.
        req.anchor_week = Some(d(11, 12));
        let got = resolve(&req, &base, today).unwrap().unwrap();
        assert_eq!(got.anchor_week, Some(d(11, 9)));
        assert_eq!(got.interval_weeks, 2);
        assert_eq!(got.kind, "weekly");
        for bad in [0, 5] {
            req.interval_weeks = Some(bad);
            assert!(resolve(&req, &base, today).is_err(), "{bad}");
        }
        // Back to every week: the anchor goes.
        let weekly = PatternRequest {
            interval_weeks: Some(1),
            ..request()
        };
        assert_eq!(
            resolve(&weekly, &got, today).unwrap().unwrap().anchor_week,
            None
        );
    }

    #[test]
    fn a_list_of_dates_needs_dates_and_no_interval() {
        let _on = set_enabled_for_tests(true);
        let today = d(11, 7);
        let base = Pattern::default();
        let mut req = PatternRequest {
            recurrence_kind: Some("dates".to_string()),
            ..request()
        };
        assert!(resolve(&req, &base, today).is_err());
        req.extra_dates = Some(vec![d(11, 20), d(11, 10), d(11, 20)]);
        let got = resolve(&req, &base, today).unwrap().unwrap();
        assert_eq!(got.extra_dates, vec![d(11, 10), d(11, 20)]);
        req.interval_weeks = Some(2);
        req.anchor_week = Some(d(11, 9));
        assert!(resolve(&req, &base, today).is_err());
        req.interval_weeks = None;
        req.anchor_week = None;
        req.recurrence_kind = Some("cycle".to_string());
        assert!(
            resolve(&req, &base, today).is_err(),
            "rotas aren't offered yet"
        );
    }

    #[test]
    fn extra_dates_are_checked_when_given_and_kept_when_not() {
        let _on = set_enabled_for_tests(true);
        let today = d(11, 7);
        let base = Pattern::default();
        let past = PatternRequest {
            extra_dates: Some(vec![d(11, 6)]),
            ..request()
        };
        assert!(resolve(&past, &base, today).is_err());
        let far = PatternRequest {
            extra_dates: Some(vec![today + Duration::days(401)]),
            ..request()
        };
        assert!(resolve(&far, &base, today).is_err());
        let many = PatternRequest {
            extra_dates: Some((0..101).map(|n| today + Duration::days(n)).collect()),
            ..request()
        };
        assert!(resolve(&many, &base, today).is_err());
        // A PUT changing only the interval keeps the stored dates.
        let stored = Pattern {
            extra_dates: vec![d(12, 25)],
            ..Pattern::default()
        };
        let req = PatternRequest {
            interval_weeks: Some(2),
            anchor_week: Some(d(11, 9)),
            ..request()
        };
        assert_eq!(
            resolve(&req, &stored, today).unwrap().unwrap().extra_dates,
            vec![d(12, 25)]
        );
    }

    #[test]
    fn the_preview_rule_follows_the_pattern() {
        let pattern = Pattern {
            interval_weeks: 2,
            anchor_week: Some(d(11, 9)),
            extra_dates: vec![d(11, 21)],
            ..Pattern::default()
        };
        let rule = preview_rule(Some(1), None, None, &pattern);
        let ex = ds_store::recurrence::Exceptions::default();
        let got: Vec<_> = rule
            .upcoming(&ex, d(11, 7), 3, 400)
            .into_iter()
            .map(|day| day.date)
            .collect();
        assert_eq!(got, vec![d(11, 9), d(11, 21), d(11, 23)]);
    }
}
