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
//! Also a Delay Repay reminder per journey that may have arrived late or had
//! a cancelled train: a whole-day event on the last day of the claim window
//! with alarms a week and a day earlier, worded as a prompt to check. The minimum delay and the window are the
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

use crate::data::delay_repay_rules::{claim_rule_for, operator_rule_for};
use crate::data::journeys::JourneyListItem;
use crate::ics::{Alarm, Event, When};
use crate::routes::feeds::Site;

/// The environment variable that turns the calendar feed on.
pub const CALENDAR_ENABLED_ENV: &str = "CALENDAR_FEED_ENABLED";

/// `unlisted_links.resource_type` of a feed's secret link; its
/// `resource_id` is the user's id, so a user has at most one active feed
/// link (`unlisted_links_one_active_per_resource`).
pub const RESOURCE_TYPE: &str = "calendar_feed";

/// How many days a feed lists a journey after it set off: the plan for
/// today and last week's journeys, which the claim reminders follow.
pub const PAST_DAYS: i64 = 7;

/// Days before the last claim day that a reminder's alarms show: a week
/// ahead, and a day ahead for a journey whose week-ahead date has already
/// passed when the feed is first read.
pub const REMINDER_DAYS_BEFORE: [u32; 2] = [7, 1];

/// How many journeys a feed reads. The journeys list puts the journeys
/// still to come first, then the travelled ones newest first, and its own
/// cap (`train_tracking::MINE_LIST_LIMIT`, 100) let a user with many
/// planned journeys (a regular journey's occurrences among them) lose the
/// last week's plans and the claim reminders of up to 28 days ago. A feed
/// is read every 8 to 24 hours, so it can afford a deliberately larger
/// read; a journey past this many newer ones gets no event or reminder.
pub const FEED_LIST_LIMIT: i64 = 500;

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

/// A journey created at or after this instant (Unix seconds,
/// 2026-11-25T00:00:00Z) is named in calendars by its public id
/// (`journey-j_...@host`); an earlier one keeps `journey-{id}@host`, so an
/// event's UID never changes (a changed UID would show every upcoming event
/// twice in a subscriber's calendar) and the numeric UIDs age out as those
/// journeys are travelled: the feed lists only journeys that have not set
/// off. A few days after the public-id release is deployed, so a journey
/// made between an earlier constant and the deploy would not change UID once.
pub const PUBLIC_UID_FROM_UNIX: i64 = 1_795_564_800;

/// What an event is made from, and no more.
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts read off one journeys-list row"
)]
pub struct JourneyFacts {
    pub id: i64,
    /// `journeys.public_id`.
    pub public_id: String,
    /// When the journey was made: which form its event UID takes
    /// ([`PUBLIC_UID_FROM_UNIX`]).
    pub created_at: DateTime<Utc>,
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
    /// Whether Delay Repay can apply to the journey's end at all: its last
    /// train is a GB train (not an Iarnród Éireann `ie:` one, which Delay
    /// Repay does not cover) with a known destination, so its delay is the
    /// one measured where the journey ends, as
    /// `journey_tickets::leg_delay_repays` requires. Without one there is no
    /// reminder, for a delay or a cancellation.
    pub delay_repay_applies: bool,
}

impl JourneyFacts {
    /// What names the journey in an event UID and a download's file name: its
    /// public id when it was created at or after [`PUBLIC_UID_FROM_UNIX`],
    /// else its numeric id (also when it has no public id).
    #[must_use]
    pub fn uid_key(&self) -> String {
        if self.created_at.timestamp() >= PUBLIC_UID_FROM_UNIX && !self.public_id.is_empty() {
            self.public_id.clone()
        } else {
            self.id.to_string()
        }
    }
}

/// [`JourneyFacts::delay_repay_applies`] from the last train leg's uid and
/// destination. With no destination the journeys list falls back to the
/// train's running delay wherever it is, which says nothing about where
/// the journey ends.
pub fn delay_repay_applies(
    final_train_uid: Option<&str>,
    final_destination_crs: Option<&str>,
) -> bool {
    let last_train_irish =
        final_train_uid.is_some_and(common::island_of_ireland::tracking::is_train_uid);
    !last_train_irish && final_destination_crs.is_some()
}

impl From<&JourneyListItem> for JourneyFacts {
    fn from(item: &JourneyListItem) -> Self {
        let name = |name: &Option<String>, crs: &Option<String>| {
            name.clone()
                .or_else(|| crs.clone())
                .unwrap_or_else(|| "somewhere".to_string())
        };
        let single_untracked = item.leg_count == 1 && item.kind == "untracked";
        let delay_repay_applies = delay_repay_applies(
            item.final_train_uid.as_deref(),
            item.final_destination_crs.as_deref(),
        );
        Self {
            id: item.id,
            public_id: item.public_id.clone(),
            created_at: item.created_at,
            departs_at: item.departs_at,
            origin: name(&item.first_origin_name, &item.first_origin_crs),
            destination: name(&item.last_destination_name, &item.last_destination_crs),
            planned_end: single_untracked.then_some(item.planned_arrival).flatten(),
            travelled: item.travelled,
            has_train_leg: item.train_leg_count > 0,
            cancelled: item.cancelled_leg_count > 0,
            final_delay_minutes: if item.final_arrival_delay_provisional || !delay_repay_applies {
                None
            } else {
                item.final_arrival_delay_minutes
            },
            service_date: item.final_service_date.unwrap_or(item.first_service_date),
            untracked: item.kind == "untracked" || item.leg_count > item.train_leg_count,
            operator_atoc: None,
            delay_repay_applies,
        }
    }
}

/// The text every event carries: the credits the data behind it needs
/// (`routes::feeds::Credit`, as the Atom feeds do) and the
/// non-affiliation statement, then the full credits page's address (a
/// calendar app cannot follow a relative path, so it is absolute).
pub fn credits_text(untracked: bool, origin: &str) -> String {
    use crate::routes::feeds::{Credit, NON_AFFILIATION_STATEMENT};
    let mut credits = vec![Credit::NationalRail, Credit::NetworkRail];
    if untracked {
        credits.push(Credit::Tfl);
    }
    let credits: Vec<String> = credits.into_iter().map(Credit::text).collect();
    format!(
        "Credits: {} {NON_AFFILIATION_STATEMENT} Full credits: {origin}/attribution",
        credits.join(" ")
    )
}

/// The event for a journey's plan, or `None` when it has no known start
/// or set off before `since`.
pub fn journey_event(facts: &JourneyFacts, site: &Site, since: DateTime<Utc>) -> Option<Event> {
    let departs_at = facts.departs_at?;
    if departs_at < since {
        return None;
    }
    Some(Event {
        uid: format!("journey-{}@{}", facts.uid_key(), site.host),
        start: When::At(departs_at),
        end: facts.planned_end.filter(|end| *end > departs_at),
        summary: format!("Journey: {} to {}", facts.origin, facts.destination),
        description: format!(
            "Planned in Distant Signal (unofficial). Times are as planned when this \
             calendar was made; check Distant Signal for the live train. {}",
            credits_text(facts.untracked, &site.origin)
        ),
        alarms: Vec::new(),
    })
}

/// The Delay Repay reminder for a journey that is over and may have arrived
/// late (or had a cancelled train), while its claim window is still open on
/// `today`.
///
/// Worded as a prompt to check, never as a deadline or a fact: the date is
/// an estimate from the service date and the operator's window, the delay
/// is Distant Signal's own reading for the last train only, and Distant
/// Signal does not know the ticket. The alarm carries its own hedge because
/// a pop-up shows nothing else.
pub fn claim_reminder(facts: &JourneyFacts, site: &Site, today: NaiveDate) -> Option<Event> {
    if !facts.travelled || !facts.has_train_leg || !facts.delay_repay_applies {
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
    let date = facts.service_date.format("%-d %B %Y");
    // A rule is worded as a company's own only when the company has one;
    // the default is the usual rule, not any one company's.
    let (delay_rule, window_rule) = match operator_rule_for(facts.operator_atoc.as_deref()) {
        Some(operator) => (
            format!(
                "{} usually pays from a delay of {} minutes.",
                operator.name, rule.min_delay_minutes
            ),
            format!(
                "{} usually takes claims for up to {} days after the journey.",
                operator.name, rule.window_days
            ),
        ),
        None => (
            format!(
                "Most train companies pay from a delay of {} minutes, but yours may differ.",
                rule.min_delay_minutes
            ),
            format!(
                "Most train companies take claims for up to {} days after the journey, but \
                 yours may differ.",
                rule.window_days
            ),
        ),
    };
    let what = if facts.cancelled {
        format!(
            "Your journey on {date} included a cancelled train. You may be able to claim \
             Delay Repay for a cancellation, depending on what happened to your journey and \
             your ticket. {window_rule}"
        )
    } else {
        format!(
            "Your journey on {date} may have arrived late enough to claim Delay Repay. \
             {delay_rule} {window_rule}"
        )
    };
    let alarm_text = format!(
        "Check Delay Repay for {} to {}. Check your train company's own rules: claims \
         are usually open for up to {} days after the journey.",
        facts.origin, facts.destination, rule.window_days
    );
    Some(Event {
        uid: format!("claim-{}@{}", facts.uid_key(), site.host),
        start: When::Day(deadline),
        end: None,
        summary: format!(
            "Check Delay Repay: {} to {}",
            facts.origin, facts.destination
        ),
        description: format!(
            "{what} Check your train company's own page, which is the one that counts. \
             Distant Signal estimates this from its own train data, does not know your ticket \
             and never makes a claim for you. {}",
            credits_text(facts.untracked, &site.origin)
        ),
        alarms: REMINDER_DAYS_BEFORE
            .into_iter()
            .map(|days_before| Alarm {
                days_before,
                text: alarm_text.clone(),
            })
            .collect(),
    })
}

/// A user's feed events: each journey's plan, then each open claim
/// reminder, in the journeys list's order.
pub async fn feed_events(
    pool: &sqlx::PgPool,
    user_id: &str,
    site: &Site,
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<Event>> {
    feed_events_capped(pool, user_id, site, now, FEED_LIST_LIMIT).await
}

/// [`feed_events`] reading at most `limit` journeys.
pub(crate) async fn feed_events_capped(
    pool: &sqlx::PgPool,
    user_id: &str,
    site: &Site,
    now: DateTime<Utc>,
    limit: i64,
) -> anyhow::Result<Vec<Event>> {
    let rows = crate::data::journeys::list_journeys_for_user_capped(
        pool,
        user_id,
        crate::data::journeys::ListInclude::default(),
        limit,
    )
    .await?;
    events_for_rows(pool, &rows, site, now, now - Duration::days(PAST_DAYS)).await
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
    site: &Site,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
) -> anyhow::Result<Vec<Event>> {
    let operators = operators_by_journey(pool, rows).await?;
    Ok(events_for(rows, &operators, site, now, since))
}

/// The mapping from the rows and each journey's operator code.
#[expect(
    clippy::implicit_hasher,
    reason = "only ever called with the std map operators_by_journey makes"
)]
pub fn events_for(
    rows: &[JourneyListItem],
    operators: &HashMap<i64, String>,
    site: &Site,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
) -> Vec<Event> {
    let facts: Vec<JourneyFacts> = rows
        .iter()
        .map(|row| JourneyFacts {
            operator_atoc: operators.get(&row.id).cloned(),
            ..JourneyFacts::from(row)
        })
        .collect();
    events_from_facts(&facts, site, now, since)
}

/// [`events_for`] from the facts: a journey's event, and the Delay Repay
/// reminder while its claim window is open on London's date at `now`.
fn events_from_facts(
    facts: &[JourneyFacts],
    site: &Site,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
) -> Vec<Event> {
    let today = common::zone::london_date(now);
    let mut events: Vec<Event> = facts
        .iter()
        .filter_map(|f| journey_event(f, site, since))
        .collect();
    events.extend(facts.iter().filter_map(|f| claim_reminder(f, site, today)));
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone as _;

    fn facts() -> JourneyFacts {
        JourneyFacts {
            id: 7,
            public_id: "j_00000000000000000000000000000007".to_string(),
            created_at: Utc.with_ymd_and_hms(2026, 11, 1, 9, 0, 0).unwrap(),
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
            delay_repay_applies: true,
        }
    }

    fn site() -> Site {
        Site {
            origin: "https://ds.example".to_string(),
            host: "ds.example".to_string(),
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
        let event = journey_event(&facts(), &site(), since()).expect("event");
        assert_eq!(event.summary, "Journey: York to London Kings Cross");
        assert_eq!(event.uid, "journey-7@ds.example");
        assert_eq!(event.alarms, []);
        assert!(
            event
                .description
                .contains("Credits: powered by NationalRail")
        );
        assert!(
            event
                .description
                .ends_with("Full credits: https://ds.example/attribution")
        );
        assert_eq!(event.start, When::At(facts().departs_at.unwrap()));
        assert!(event.description.contains("powered by NationalRail"));
        assert!(event.description.contains("not affiliated"));
        assert!(!event.description.contains("Powered by TfL"));
    }

    #[test]
    fn the_cutover_is_the_documented_instant() {
        assert_eq!(
            PUBLIC_UID_FROM_UNIX,
            Utc.with_ymd_and_hms(2026, 11, 25, 0, 0, 0)
                .unwrap()
                .timestamp()
        );
    }

    /// A journey made before the cutover keeps its numeric UID (so its event
    /// is not replaced in a subscriber's calendar); one made at or after it
    /// is named by its public id, in the event and in the claim reminder.
    #[test]
    fn the_uid_is_numeric_before_the_cutover_and_the_public_id_from_it() {
        let cutover = DateTime::<Utc>::from_timestamp(PUBLIC_UID_FROM_UNIX, 0).unwrap();
        let mut f = facts();
        f.created_at = cutover - Duration::seconds(1);
        assert_eq!(f.uid_key(), "7");
        assert_eq!(
            journey_event(&f, &site(), since()).unwrap().uid,
            "journey-7@ds.example"
        );
        f.created_at = cutover;
        assert_eq!(f.uid_key(), "j_00000000000000000000000000000007");
        assert_eq!(
            journey_event(&f, &site(), since()).unwrap().uid,
            "journey-j_00000000000000000000000000000007@ds.example"
        );
        f.created_at = cutover + Duration::days(30);
        assert_eq!(f.uid_key(), "j_00000000000000000000000000000007");
        // Without a public id (it was not read) the numeric one stands.
        f.public_id = String::new();
        assert_eq!(f.uid_key(), "7");
    }

    #[test]
    fn a_journey_with_an_untracked_leg_credits_tfl() {
        let mut f = facts();
        f.untracked = true;
        let event = journey_event(&f, &site(), since()).expect("event");
        assert!(event.description.contains("Powered by TfL Open Data"));
    }

    #[test]
    fn journeys_before_the_window_or_with_no_start_have_no_event() {
        let mut f = facts();
        f.departs_at = Some(now() - Duration::days(8));
        assert!(journey_event(&f, &site(), since()).is_none());
        f.departs_at = None;
        assert!(journey_event(&f, &site(), since()).is_none());
    }

    #[test]
    fn a_planned_end_is_kept_only_after_the_start() {
        let mut f = facts();
        f.planned_end = Some(f.departs_at.unwrap() + Duration::minutes(20));
        assert!(journey_event(&f, &site(), since()).unwrap().end.is_some());
        f.planned_end = Some(f.departs_at.unwrap() - Duration::minutes(1));
        assert!(journey_event(&f, &site(), since()).unwrap().end.is_none());
    }

    fn late() -> JourneyFacts {
        let mut f = facts();
        f.travelled = true;
        f.final_delay_minutes = Some(22);
        f
    }

    /// The claim window is judged on London's calendar date at `now`, not
    /// UTC's: the last hour of a summer UTC day is already London's next
    /// day. Expectations are from the IANA rules (python zoneinfo). The
    /// default claim window is 28 days from the service date.
    #[test]
    fn the_reminder_is_judged_on_londons_date_at_now() {
        // (service date, now, reminder shown)
        let cases = [
            // BST: the last claim day, 2026-08-07, ends at 23:00Z.
            ("2026-07-10", "2026-08-07T22:59:00Z", true),
            ("2026-07-10", "2026-08-07T23:00:00Z", false),
            ("2026-07-10", "2026-08-08T22:59:00Z", false),
            // GMT: the calendars agree.
            ("2026-12-01", "2026-12-29T23:59:00Z", true),
            ("2026-12-01", "2026-12-30T00:00:00Z", false),
            // The clocks go back on 2026-10-25 (the last claim day).
            ("2026-09-27", "2026-10-24T23:00:00Z", true),
            ("2026-09-27", "2026-10-25T22:59:00Z", true),
            ("2026-09-27", "2026-10-25T23:59:00Z", true),
            ("2026-09-27", "2026-10-26T00:00:00Z", false),
            // The clocks go forward on 2026-03-29 (the last claim day).
            ("2026-03-01", "2026-03-28T23:59:00Z", true),
            ("2026-03-01", "2026-03-29T22:59:00Z", true),
            ("2026-03-01", "2026-03-29T23:00:00Z", false),
        ];
        for (service_date, now, shown) in cases {
            let mut f = late();
            let service_date: NaiveDate = service_date.parse().unwrap();
            f.service_date = service_date;
            let now: DateTime<Utc> = now.parse().unwrap();
            let events = events_from_facts(&[f], &site(), now, now - Duration::days(PAST_DAYS));
            let deadline = When::Day(service_date + Duration::days(28));
            assert_eq!(
                events.iter().any(|event| event.start == deadline),
                shown,
                "service date {service_date}, now {now}"
            );
        }
    }

    #[test]
    fn a_late_journey_gets_a_reminder_on_the_last_claim_day_with_an_alarm() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        let event = claim_reminder(&late(), &site(), today).expect("reminder");
        assert_eq!(
            event.start,
            When::Day(NaiveDate::from_ymd_opt(2026, 12, 8).unwrap())
        );
        let days: Vec<u32> = event.alarms.iter().map(|alarm| alarm.days_before).collect();
        assert_eq!(days, [7, 1]);
        for alarm in &event.alarms {
            assert!(alarm.text.contains("Check your train company's own rules"));
            assert!(alarm.text.contains("up to 28 days"));
            assert!(!alarm.text.to_lowercase().contains("deadline"));
        }
        assert_eq!(
            event.summary,
            "Check Delay Repay: York to London Kings Cross"
        );
        assert!(!event.summary.to_lowercase().contains("deadline"));
        assert!(event.description.contains("may have arrived late"));
        assert!(!event.description.contains("arrived late."));
        assert!(event.description.contains("never makes a claim"));
        assert_eq!(event.uid, "claim-7@ds.example");
    }

    #[test]
    fn a_cancelled_train_gets_a_reminder_without_a_delay() {
        let mut f = late();
        f.final_delay_minutes = None;
        f.cancelled = true;
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        let event = claim_reminder(&f, &site(), today).expect("reminder");
        assert!(event.description.contains("included a cancelled train"));
        assert!(!event.description.contains("from a delay of"));
        assert!(event.description.contains("depending on what happened"));
    }

    #[test]
    fn no_reminder_for_a_short_delay_an_unfinished_or_trainless_journey_or_a_closed_window() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        let mut f = late();
        f.final_delay_minutes = Some(14);
        assert!(claim_reminder(&f, &site(), today).is_none());
        let mut f = late();
        f.travelled = false;
        assert!(claim_reminder(&f, &site(), today).is_none());
        let mut f = late();
        f.has_train_leg = false;
        assert!(claim_reminder(&f, &site(), today).is_none());
        let after = NaiveDate::from_ymd_opt(2026, 12, 9).unwrap();
        assert!(claim_reminder(&late(), &site(), after).is_none());
        let on_day = NaiveDate::from_ymd_opt(2026, 12, 8).unwrap();
        assert!(claim_reminder(&late(), &site(), on_day).is_some());
    }

    /// Review 2026-11 (C2): an Irish last train, or one with no destination
    /// (whose delay would be the running delay wherever the train was), gets
    /// no reminder, late or cancelled, as Delay Repay's estimate gives none.
    #[test]
    fn no_reminder_when_delay_repay_cannot_apply_to_the_last_train() {
        assert!(delay_repay_applies(Some("C12345"), Some("KGX")));
        assert!(!delay_repay_applies(Some("ie:A123"), Some("KGX")));
        assert!(!delay_repay_applies(Some("IE:A123"), None));
        assert!(!delay_repay_applies(Some("C12345"), None));
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        let mut f = late();
        f.delay_repay_applies = false;
        f.final_delay_minutes = Some(40);
        assert!(claim_reminder(&f, &site(), today).is_none());
        f.final_delay_minutes = None;
        f.cancelled = true;
        assert!(claim_reminder(&f, &site(), today).is_none());
    }

    #[test]
    fn the_reminder_follows_the_operators_own_threshold_and_window() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        // 22 minutes late: worth a claim at a DR15 operator, not at LNER.
        let mut f = late();
        f.operator_atoc = Some("GR".to_string());
        assert!(claim_reminder(&f, &site(), today).is_none());
        f.final_delay_minutes = Some(30);
        assert!(claim_reminder(&f, &site(), today).is_some());
        // "More than 30 minutes" starts at 31; Grand Central pays from 60.
        f.operator_atoc = Some("XR".to_string());
        assert!(claim_reminder(&f, &site(), today).is_none());
        f.final_delay_minutes = Some(31);
        assert!(claim_reminder(&f, &site(), today).is_some());
        f.operator_atoc = Some("GC".to_string());
        assert!(claim_reminder(&f, &site(), today).is_none());
        f.final_delay_minutes = Some(60);
        let event = claim_reminder(&f, &site(), today).expect("reminder");
        assert!(
            event
                .description
                .contains("Grand Central usually pays from a delay of 60 minutes")
        );
        assert!(event.description.contains("up to 28 days"));
    }

    #[test]
    fn an_unknown_operator_gets_the_documented_default() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 11).unwrap();
        let mut f = late();
        f.operator_atoc = Some("ZZ".to_string());
        let event = claim_reminder(&f, &site(), today).expect("reminder");
        assert!(
            event
                .description
                .contains("Most train companies pay from a delay of 15 minutes")
        );
        assert!(event.description.contains("but yours may differ"));
        assert!(!event.description.contains("usually pays"));
        f.operator_atoc = None;
        assert!(claim_reminder(&f, &site(), today).is_some());
    }
}
