//! The journey calendar (docs/calendar-feed.md): the events a user's
//! subscription feed and a single-journey download are made of, from the
//! journeys list.
//!
//! A plan view, not live status: calendar apps refresh a subscribed feed
//! every 8 to 24 hours, so an event says where and when the journey was
//! planned for, and nothing about the train. Fields are kept to the least
//! that is useful, because a run of journeys shows where someone lives and
//! works: the journey's two ends and its first departure, no train, no
//! operator, no ticket, no name the user gave it, no group or share
//! information.
//!
//! Also a Delay Repay reminder per journey that arrived late or had a
//! cancelled train: a whole-day event on the last day of the claim window
//! with an alarm a week earlier. The minimum delay and the window are the
//! last train's operator's own (`delay_repay_rules::claim_rule_for`, with
//! its sources), Delay Repay 15 and 28 days when the operator is not known.
//! Like `delay_repay_rules` it estimates and links out, and never submits
//! or records a claim.
//!
//! Off unless `CALENDAR_FEED_ENABLED` (the chart's
//! `api.calendarFeed.enabled`): until then every calendar route is `404`.

use std::sync::atomic::{AtomicBool, Ordering};

use std::collections::HashMap;

use chrono::{DateTime, Duration, NaiveDate, Utc};

use crate::data::delay_repay_rules::claim_rule_for;
use crate::data::journeys::JourneyListItem;
use crate::ics::{Alarm, Event, When};

/// The environment variable that turns the calendar feed on.
pub const CALENDAR_ENABLED_ENV: &str = "CALENDAR_FEED_ENABLED";

/// `unlisted_links.resource_type` of a feed's secret link; its
/// `resource_id` is the user's id, so a user has at most one active feed
/// link (`unlisted_links_one_active_per_resource`).
pub const RESOURCE_TYPE: &str = "calendar_feed";

/// How many days a feed lists a journey after it set off: the plan for
/// today and last week's journeys, which the claim reminders follow.
pub const PAST_DAYS: i64 = 7;

/// Days before the deadline an event's alarm shows.
pub const REMINDER_DAYS_BEFORE: u32 = 7;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Reads [`CALENDAR_ENABLED_ENV`] (default false) once, at startup, and
/// returns it.
pub fn init_from_env() -> bool {
    let enabled = crate::data::trip_plan_live::env_bool(CALENDAR_ENABLED_ENV, false);
    ENABLED.store(enabled, Ordering::Relaxed);
    enabled
}

/// Whether the calendar feed is on ([`init_from_env`]).
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Turns the feed on or off for the whole test process; a DB test that
/// needs it on says so first.
#[cfg(test)]
pub(crate) fn set_enabled_for_tests(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// What an event is made from, and no more.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts read off one journeys-list row"
)]
pub struct JourneyFacts {
    pub id: i64,
    pub departs_at: Option<DateTime<Utc>>,
    pub origin: String,
    pub destination: String,
    /// The end of a journey that is one untracked leg with a planned
    /// arrival; trains' arrivals are not known to the list.
    pub planned_end: Option<DateTime<Utc>>,
    pub travelled: bool,
    pub has_train_leg: bool,
    pub cancelled: bool,
    /// The final arrival's delay, when it is the measured one (not a
    /// forecast).
    pub final_delay_minutes: Option<i32>,
    /// The day the journey's last train ran, else its first day.
    pub service_date: NaiveDate,
    /// Whether an untracked leg (a Tube, bus or walk) is part of it.
    pub untracked: bool,
    /// The ATOC code of the last train's operator, when known
    /// (`data::train_operator`).
    pub operator_atoc: Option<String>,
}

impl From<&JourneyListItem> for JourneyFacts {
    fn from(item: &JourneyListItem) -> Self {
        let name = |name: &Option<String>, crs: &Option<String>| {
            name.clone()
                .or_else(|| crs.clone())
                .unwrap_or_else(|| "somewhere".to_string())
        };
        let single_untracked = item.leg_count == 1 && item.kind == "untracked";
        Self {
            id: item.id,
            departs_at: item.departs_at,
            origin: name(&item.first_origin_name, &item.first_origin_crs),
            destination: name(&item.last_destination_name, &item.last_destination_crs),
            planned_end: single_untracked.then_some(item.planned_arrival).flatten(),
            travelled: item.travelled,
            has_train_leg: item.train_leg_count > 0,
            cancelled: item.cancelled_leg_count > 0,
            final_delay_minutes: if item.final_arrival_delay_provisional {
                None
            } else {
                item.final_arrival_delay_minutes
            },
            service_date: item.final_service_date.unwrap_or(item.first_service_date),
            untracked: item.kind == "untracked" || item.leg_count > item.train_leg_count,
            operator_atoc: None,
        }
    }
}

/// The text every event carries: the credits the data behind it needs
/// (`routes::feeds::Credit`, as the Atom feeds do) and the
/// non-affiliation statement.
pub fn credits_text(untracked: bool) -> String {
    use crate::routes::feeds::{Credit, NON_AFFILIATION_STATEMENT};
    let mut credits = vec![Credit::NationalRail, Credit::NetworkRail];
    if untracked {
        credits.push(Credit::Tfl);
    }
    let credits: Vec<String> = credits.into_iter().map(Credit::text).collect();
    format!(
        "{} {NON_AFFILIATION_STATEMENT} Full credits: /attribution",
        credits.join(" ")
    )
}

/// The event for a journey's plan, or `None` when it has no known start
/// or set off before `since`.
pub fn journey_event(facts: &JourneyFacts, host: &str, since: DateTime<Utc>) -> Option<Event> {
    let departs_at = facts.departs_at?;
    if departs_at < since {
        return None;
    }
    Some(Event {
        uid: format!("journey-{}@{host}", facts.id),
        start: When::At(departs_at),
        end: facts.planned_end.filter(|end| *end > departs_at),
        summary: format!("Journey: {} to {}", facts.origin, facts.destination),
        description: format!(
            "Planned in Distant Signal (unofficial). Times are as planned when this \
             calendar was made; check Distant Signal for the live train. {}",
            credits_text(facts.untracked)
        ),
        alarm: None,
    })
}

/// The Delay Repay reminder for a journey that is over and arrived late (or
/// had a cancelled train), while its claim window is still open on `today`.
pub fn claim_reminder(facts: &JourneyFacts, host: &str, today: NaiveDate) -> Option<Event> {
    if !facts.travelled || !facts.has_train_leg {
        return None;
    }
    let rule = claim_rule_for(facts.operator_atoc.as_deref());
    let late = facts
        .final_delay_minutes
        .is_some_and(|delay| delay >= rule.min_delay_minutes);
    if !late && !facts.cancelled {
        return None;
    }
    let deadline = facts.service_date + Duration::days(rule.window_days);
    if deadline < today {
        return None;
    }
    let what = if facts.cancelled {
        "had a cancelled train"
    } else {
        "arrived late"
    };
    Some(Event {
        uid: format!("claim-{}@{host}", facts.id),
        start: When::Day(deadline),
        end: None,
        summary: format!(
            "Delay Repay claim deadline: {} to {}",
            facts.origin, facts.destination
        ),
        description: format!(
            "Your journey on {} {what}. You may be able to claim Delay Repay from the \
             train company, which pays from a delay of {} minutes and takes claims for {} \
             days after the journey. Check your train company's own page, which is the one \
             that counts. Distant Signal estimates this and never makes a claim for you. {}",
            facts.service_date.format("%-d %B %Y"),
            rule.min_delay_minutes,
            rule.window_days,
            credits_text(facts.untracked)
        ),
        alarm: Some(Alarm {
            days_before: REMINDER_DAYS_BEFORE,
        }),
    })
}

/// A user's feed events: each journey's plan, then each open claim
/// reminder, in the journeys list's order.
pub async fn feed_events(
    pool: &sqlx::PgPool,
    user_id: &str,
    host: &str,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<Event>> {
    let rows = crate::data::journeys::list_journeys_for_user(
        pool,
        user_id,
        crate::data::journeys::ListInclude::default(),
    )
    .await?;
    events_for_rows(pool, &rows, host, now, now - Duration::days(PAST_DAYS)).await
}

/// The ATOC code of each journey's last train's operator, by journey id,
/// from one read per service date. A journey whose train has no single
/// known operator is absent.
async fn operators_by_journey(
    pool: &sqlx::PgPool,
    rows: &[JourneyListItem],
) -> anyhow::Result<HashMap<i64, String>> {
    let mut by_date: HashMap<NaiveDate, Vec<&JourneyListItem>> = HashMap::new();
    for row in rows {
        if row.travelled
            && let Some(date) = row.final_service_date
            && row.final_train_uid.is_some()
        {
            by_date.entry(date).or_default().push(row);
        }
    }
    let mut out = HashMap::new();
    for (date, group) in by_date {
        let uids: Vec<String> = group
            .iter()
            .filter_map(|row| row.final_train_uid.clone())
            .collect();
        let operators =
            crate::data::train_operator::operators_for_trains(pool, &uids, date).await?;
        for row in group {
            if let Some(operator) = row
                .final_train_uid
                .as_ref()
                .and_then(|uid| operators.get(uid))
            {
                out.insert(row.id, operator.code.clone());
            }
        }
    }
    Ok(out)
}

/// [`events_for`] with the operators read for the journeys that are over.
pub async fn events_for_rows(
    pool: &sqlx::PgPool,
    rows: &[JourneyListItem],
    host: &str,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
) -> anyhow::Result<Vec<Event>> {
    let operators = operators_by_journey(pool, rows).await?;
    Ok(events_for(rows, &operators, host, now, since))
}

/// The mapping from the rows and each journey's operator code.
#[expect(
    clippy::implicit_hasher,
    reason = "only ever called with the std map operators_by_journey makes"
)]
pub fn events_for(
    rows: &[JourneyListItem],
    operators: &HashMap<i64, String>,
    host: &str,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
) -> Vec<Event> {
    let today = now.with_timezone(&chrono_tz::Europe::London).date_naive();
    let facts: Vec<JourneyFacts> = rows
        .iter()
        .map(|row| JourneyFacts {
            operator_atoc: operators.get(&row.id).cloned(),
            ..JourneyFacts::from(row)
        })
        .collect();
    let mut events: Vec<Event> = facts
        .iter()
        .filter_map(|f| journey_event(f, host, since))
        .collect();
    events.extend(facts.iter().filter_map(|f| claim_reminder(f, host, today)));
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn facts() -> JourneyFacts {
        JourneyFacts {
            id: 7,
            departs_at: Some(Utc.with_ymd_and_hms(2026, 11, 10, 8, 30, 0).unwrap()),
            origin: "York".to_string(),
            destination: "London Kings Cross".to_string(),
            planned_end: None,
            travelled: false,
            has_train_leg: true,
            cancelled: false,
            final_delay_minutes: None,
            service_date: NaiveDate::from_ymd_opt(2026, 11, 10).unwrap(),
            untracked: false,
            operator_atoc: None,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap()
    }

    fn since() -> DateTime<Utc> {
        now() - Duration::days(PAST_DAYS)
    }

    #[test]
    fn an_event_has_the_two_ends_and_the_start_and_nothing_else_about_the_trip() {
        let event = journey_event(&facts(), "ds.example", since()).expect("event");
        assert_eq!(event.summary, "Journey: York to London Kings Cross");
        assert_eq!(event.uid, "journey-7@ds.example");
        assert_eq!(event.start, When::At(facts().departs_at.unwrap()));
        assert!(event.description.contains("powered by NationalRail"));
        assert!(event.description.contains("not affiliated"));
        assert!(!event.description.contains("Powered by TfL"));
    }

    #[test]
    fn a_journey_with_an_untracked_leg_credits_tfl() {
        let mut f = facts();
        f.untracked = true;
        let event = journey_event(&f, "h", since()).expect("event");
        assert!(event.description.contains("Powered by TfL Open Data"));
    }

    #[test]
    fn journeys_before_the_window_or_with_no_start_have_no_event() {
        let mut f = facts();
        f.departs_at = Some(now() - Duration::days(8));
        assert!(journey_event(&f, "h", since()).is_none());
        f.departs_at = None;
        assert!(journey_event(&f, "h", since()).is_none());
    }

    #[test]
    fn a_planned_end_is_kept_only_after_the_start() {
        let mut f = facts();
        f.planned_end = Some(f.departs_at.unwrap() + Duration::minutes(20));
        assert!(journey_event(&f, "h", since()).unwrap().end.is_some());
        f.planned_end = Some(f.departs_at.unwrap() - Duration::minutes(1));
        assert!(journey_event(&f, "h", since()).unwrap().end.is_none());
    }

    fn late() -> JourneyFacts {
        let mut f = facts();
        f.travelled = true;
        f.final_delay_minutes = Some(22);
        f
    }

    #[test]
    fn a_late_journey_gets_a_reminder_on_the_last_claim_day_with_an_alarm() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        let event = claim_reminder(&late(), "h", today).expect("reminder");
        assert_eq!(
            event.start,
            When::Day(NaiveDate::from_ymd_opt(2026, 12, 8).unwrap())
        );
        assert_eq!(event.alarm, Some(Alarm { days_before: 7 }));
        assert!(event.description.contains("never makes a claim"));
        assert_eq!(event.uid, "claim-7@h");
    }

    #[test]
    fn a_cancelled_train_gets_a_reminder_without_a_delay() {
        let mut f = late();
        f.final_delay_minutes = None;
        f.cancelled = true;
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        assert!(claim_reminder(&f, "h", today).is_some());
    }

    #[test]
    fn no_reminder_for_a_short_delay_an_unfinished_or_trainless_journey_or_a_closed_window() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        let mut f = late();
        f.final_delay_minutes = Some(14);
        assert!(claim_reminder(&f, "h", today).is_none());
        let mut f = late();
        f.travelled = false;
        assert!(claim_reminder(&f, "h", today).is_none());
        let mut f = late();
        f.has_train_leg = false;
        assert!(claim_reminder(&f, "h", today).is_none());
        let after = NaiveDate::from_ymd_opt(2026, 12, 9).unwrap();
        assert!(claim_reminder(&late(), "h", after).is_none());
        let on_day = NaiveDate::from_ymd_opt(2026, 12, 8).unwrap();
        assert!(claim_reminder(&late(), "h", on_day).is_some());
    }

    #[test]
    fn the_reminder_follows_the_operators_own_threshold_and_window() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        // 22 minutes late: worth a claim at a DR15 operator, not at LNER.
        let mut f = late();
        f.operator_atoc = Some("GR".to_string());
        assert!(claim_reminder(&f, "h", today).is_none());
        f.final_delay_minutes = Some(30);
        assert!(claim_reminder(&f, "h", today).is_some());
        // "More than 30 minutes" starts at 31; Grand Central pays from 60.
        f.operator_atoc = Some("XR".to_string());
        assert!(claim_reminder(&f, "h", today).is_none());
        f.final_delay_minutes = Some(31);
        assert!(claim_reminder(&f, "h", today).is_some());
        f.operator_atoc = Some("GC".to_string());
        assert!(claim_reminder(&f, "h", today).is_none());
        f.final_delay_minutes = Some(60);
        let event = claim_reminder(&f, "h", today).expect("reminder");
        assert!(event.description.contains("from a delay of 60 minutes"));
        assert!(event.description.contains("28 days"));
    }

    #[test]
    fn an_unknown_operator_gets_the_documented_default() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        let mut f = late();
        f.operator_atoc = Some("ZZ".to_string());
        let event = claim_reminder(&f, "h", today).expect("reminder");
        assert!(event.description.contains("from a delay of 15 minutes"));
        f.operator_atoc = None;
        assert!(claim_reminder(&f, "h", today).is_some());
    }
}
