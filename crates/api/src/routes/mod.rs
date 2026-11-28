use crate::app::Router;

pub mod account;
pub mod admin;
pub mod auth;
pub mod calendar;
#[cfg(test)]
#[expect(
    clippy::too_many_lines,
    reason = "test code: scenario tests read top to bottom"
)]
mod calendar_db_tests;
#[cfg(test)]
mod change_time_db_tests;
pub mod connected_apps;
#[cfg(test)]
#[expect(
    clippy::too_many_lines,
    reason = "test code: scenario tests read top to bottom"
)]
mod delegated_db_tests;
pub mod departures;
pub mod feeds;
pub mod freshness;
pub mod groups;
pub mod health;
pub mod history_retention;
pub mod incidents;
#[cfg(test)]
mod irish_train_db_tests;
pub mod island_of_ireland;
#[cfg(test)]
mod journey_boarding_db_tests;
#[cfg(test)]
#[expect(
    clippy::too_many_lines,
    reason = "test code: scenario tests read top to bottom"
)]
mod journey_follows_db_tests;
#[cfg(test)]
#[expect(
    clippy::too_many_lines,
    reason = "test code: scenario tests read top to bottom"
)]
mod journey_history_order_db_tests;
pub mod journey_irish_train;
pub mod journey_itinerary;
pub mod journey_ref;
pub mod journey_repeat;
pub mod journey_replan;
pub mod journey_templates;
pub mod journey_tickets;
#[cfg(test)]
mod journey_timetable_db_tests;
pub mod journeys;
#[cfg(test)]
mod leave_by_db_tests;
#[cfg(test)]
mod leg_snapshot_db_tests;
#[cfg(test)]
mod leg_validation_db_tests;
pub mod line_status;
pub mod line_timetable;
pub mod line_trains_summary;
pub mod lines;
#[cfg(test)]
mod notification_settings_db_tests;
pub mod notifications;
#[cfg(test)]
mod numeric_ids_db_tests;
#[cfg(test)]
mod opaque_ids_p2_db_tests;
pub mod operator_history;
pub mod operators;
pub mod personal_tokens;
#[cfg(test)]
#[expect(
    clippy::too_many_lines,
    reason = "test code: scenario tests read top to bottom"
)]
mod personal_tokens_db_tests;
pub mod preferences;
pub mod provisional;
pub mod recently_deleted;
#[cfg(test)]
#[expect(
    clippy::too_many_lines,
    reason = "test code: scenario tests read top to bottom"
)]
mod recently_deleted_db_tests;
pub mod reference;
pub mod regular_plan;
pub mod reliability;
pub mod schedule_rows;
pub mod stanox_crs;
pub mod station_board;
pub mod station_stats;
pub mod tfl_stop_page;
pub mod tfl_stops;
pub mod time_away;
pub mod train;
#[cfg(test)]
mod train_by_uid_retained_db_tests;
pub mod trains;
pub mod transit;
pub mod trips;
pub mod usage;
pub mod version;
pub mod webhooks;

/// A field that is present, `null` or not: `Some(None)` for `null`.
#[expect(
    clippy::option_option,
    reason = "absent (unchanged) and null (cleared) are different requests"
)]
pub(crate) fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer).map(Some)
}

/// The current instant on the Europe/London wall clock.
///
/// Every rail-day-keyed table this API reads (`schedule_network_departures`,
/// `schedule_line_population`, `schedule_destination_departures`, ...) is
/// keyed by London service date, never UTC: for an hour every night (all of
/// 23:00-00:00 UTC during BST) `Utc::now().date_naive()` is still London's
/// yesterday. Routes -- and the db tests that seed "today" for them -- must
/// all derive "today"/"now" from this one function so they can never
/// disagree about which day it is.
///
/// Test builds can pin it per thread with [`pin_london_now_for_tests`], so a
/// db test no longer depends on the wall clock (DB review 2026-09-27 B4).
///
/// The offset is London's at that instant and is fixed once read: to add a
/// duration, or to compare wall-clock values across a clock change, convert
/// to UTC first (`.to_utc()`) and read London again with
/// `common::zone::london_local`.
pub(crate) fn london_now() -> chrono::DateTime<chrono::FixedOffset> {
    #[cfg(test)]
    if let Some(pinned) = PINNED_NOW.with(std::cell::Cell::get) {
        return common::zone::london_local(pinned);
    }
    common::zone::london_local(chrono::Utc::now())
}

#[cfg(test)]
thread_local! {
    static PINNED_NOW: std::cell::Cell<Option<chrono::DateTime<chrono::Utc>>> =
        const { std::cell::Cell::new(None) };
}

/// Pins [`london_now`] (and so [`london_today`]) to `instant` on the current
/// thread until the returned guard is dropped. A `#[tokio::test]` uses the
/// current-thread runtime, so the handler under test (driven through
/// `oneshot`) runs on the same thread and sees the pinned value; a
/// `multi_thread` test would not.
#[cfg(test)]
#[must_use = "the pin is lifted as soon as the guard is dropped"]
pub(crate) fn pin_london_now_for_tests(instant: chrono::DateTime<chrono::Utc>) -> PinnedLondonNow {
    PINNED_NOW.with(|cell| cell.set(Some(instant)));
    PinnedLondonNow(())
}

/// Guard returned by [`pin_london_now_for_tests`].
#[cfg(test)]
pub(crate) struct PinnedLondonNow(());

#[cfg(test)]
impl Drop for PinnedLondonNow {
    fn drop(&mut self) {
        PINNED_NOW.with(|cell| cell.set(None));
    }
}

/// London-local "today" -- see [`london_now`].
pub(crate) fn london_today() -> chrono::NaiveDate {
    london_now().date_naive()
}

// Moved to `ds_store::validate` (ingest architecture plan 1A.2): the
// ingest half of `train_tracking` needs them too.
pub(crate) use ds_store::validate::{
    is_ascii_code, is_crs_code, validate_code_list, validate_short_text,
};

pub fn public_router() -> Router {
    // `health::router()` already declares its own `/health` route, so this
    // must `merge` (mount directly under `/public`) rather than `nest`
    // another `/health` prefix on top of it — nesting here previously
    // produced `/public/health/health` instead of the intended
    // `/public/health`, discovered while wiring up the docker-compose
    // healthcheck in Task 6's end-to-end verification.
    //
    // `line_status::router()` is deliberately NOT merged in here. This
    // function's output is always nested under `/public` in `main.rs`
    // (load-bearing: `docker-compose.yml`'s healthcheck hits
    // `/public/health` and `crates/api/Dockerfile`'s HEALTHCHECK comment
    // says the same), but the four line-status endpoints must be
    // reachable at the unprefixed paths DESIGN.md specifies
    // (`GET /Line/Mode/national-rail/Status`, `GET /StopPoint/{crs}/Disruption`,
    // etc.) so that clients already built against TfL's own API work
    // unchanged — that's the entire point of mimicking TfL's response
    // shape. Nesting them under `/public` like `health` would silently
    // break that compatibility. `main.rs` merges `line_status::router()`
    // directly onto the top-level router instead; it's still
    // unauthenticated, just not routed through this particular function.
    Router::new()
        .merge(health::router())
        .merge(freshness::router())
        .merge(version::router())
        .merge(history_retention::router())
        .merge(incidents::router())
        .merge(lines::router())
        .merge(notifications::router())
        .merge(operator_history::router())
        .merge(operators::router())
        .merge(preferences::router())
        .merge(time_away::router())
        .merge(reference::router())
        .merge(island_of_ireland::router())
        .merge(auth::router())
        .merge(admin::router())
        .merge(account::router())
        .merge(connected_apps::router())
        .merge(personal_tokens::router())
        .merge(webhooks::router())
        .merge(groups::router())
        .merge(station_stats::router())
        .merge(departures::router())
        .merge(station_board::router())
        .merge(calendar::router())
        .merge(feeds::router())
        .merge(stanox_crs::router())
        .merge(trains::router())
        .merge(transit::router())
        .merge(tfl_stops::router())
        .merge(tfl_stop_page::router())
        .merge(usage::router())
}

/// Pins [`london_now`] and [`london_today`] (through the test pin) around both
/// clock changes of 2025 and 2026 and at the UTC/London date boundary. The
/// expected values come from the IANA rules via an unrelated implementation.
#[cfg(test)]
mod london_now_pinning_tests {
    use super::*;

    /// `(pinned instant, London time as RFC 3339, London date)`.
    #[rustfmt::skip]
    const NOW: &[(&str, &str, &str)] = &[
        ("2025-03-29T22:00:00Z", "2025-03-29T22:00:00+00:00", "2025-03-29"),
        ("2025-03-29T23:00:00Z", "2025-03-29T23:00:00+00:00", "2025-03-29"),
        ("2025-03-29T23:30:00Z", "2025-03-29T23:30:00+00:00", "2025-03-29"),
        ("2025-03-30T00:00:00Z", "2025-03-30T00:00:00+00:00", "2025-03-30"),
        ("2025-03-30T00:30:00Z", "2025-03-30T00:30:00+00:00", "2025-03-30"),
        ("2025-03-30T00:59:00Z", "2025-03-30T00:59:00+00:00", "2025-03-30"),
        ("2025-03-30T01:00:00Z", "2025-03-30T02:00:00+01:00", "2025-03-30"),
        ("2025-03-30T01:01:00Z", "2025-03-30T02:01:00+01:00", "2025-03-30"),
        ("2025-03-30T01:30:00Z", "2025-03-30T02:30:00+01:00", "2025-03-30"),
        ("2025-03-30T01:59:00Z", "2025-03-30T02:59:00+01:00", "2025-03-30"),
        ("2025-03-30T02:00:00Z", "2025-03-30T03:00:00+01:00", "2025-03-30"),
        ("2025-03-30T02:30:00Z", "2025-03-30T03:30:00+01:00", "2025-03-30"),
        ("2025-03-30T03:00:00Z", "2025-03-30T04:00:00+01:00", "2025-03-30"),
        ("2025-03-30T12:00:00Z", "2025-03-30T13:00:00+01:00", "2025-03-30"),
        ("2025-03-30T22:59:00Z", "2025-03-30T23:59:00+01:00", "2025-03-30"),
        ("2025-03-30T23:00:00Z", "2025-03-31T00:00:00+01:00", "2025-03-31"),
        ("2025-03-30T23:30:00Z", "2025-03-31T00:30:00+01:00", "2025-03-31"),
        ("2025-03-31T00:30:00Z", "2025-03-31T01:30:00+01:00", "2025-03-31"),
        ("2025-10-25T22:00:00Z", "2025-10-25T23:00:00+01:00", "2025-10-25"),
        ("2025-10-25T23:00:00Z", "2025-10-26T00:00:00+01:00", "2025-10-26"),
        ("2025-10-25T23:30:00Z", "2025-10-26T00:30:00+01:00", "2025-10-26"),
        ("2025-10-26T00:00:00Z", "2025-10-26T01:00:00+01:00", "2025-10-26"),
        ("2025-10-26T00:30:00Z", "2025-10-26T01:30:00+01:00", "2025-10-26"),
        ("2025-10-26T00:59:00Z", "2025-10-26T01:59:00+01:00", "2025-10-26"),
        ("2025-10-26T01:00:00Z", "2025-10-26T01:00:00+00:00", "2025-10-26"),
        ("2025-10-26T01:01:00Z", "2025-10-26T01:01:00+00:00", "2025-10-26"),
        ("2025-10-26T01:30:00Z", "2025-10-26T01:30:00+00:00", "2025-10-26"),
        ("2025-10-26T01:59:00Z", "2025-10-26T01:59:00+00:00", "2025-10-26"),
        ("2025-10-26T02:00:00Z", "2025-10-26T02:00:00+00:00", "2025-10-26"),
        ("2025-10-26T02:30:00Z", "2025-10-26T02:30:00+00:00", "2025-10-26"),
        ("2025-10-26T03:00:00Z", "2025-10-26T03:00:00+00:00", "2025-10-26"),
        ("2025-10-26T12:00:00Z", "2025-10-26T12:00:00+00:00", "2025-10-26"),
        ("2025-10-26T22:59:00Z", "2025-10-26T22:59:00+00:00", "2025-10-26"),
        ("2025-10-26T23:00:00Z", "2025-10-26T23:00:00+00:00", "2025-10-26"),
        ("2025-10-26T23:30:00Z", "2025-10-26T23:30:00+00:00", "2025-10-26"),
        ("2025-10-27T00:30:00Z", "2025-10-27T00:30:00+00:00", "2025-10-27"),
        ("2025-12-31T22:00:00Z", "2025-12-31T22:00:00+00:00", "2025-12-31"),
        ("2025-12-31T22:59:00Z", "2025-12-31T22:59:00+00:00", "2025-12-31"),
        ("2025-12-31T23:00:00Z", "2025-12-31T23:00:00+00:00", "2025-12-31"),
        ("2025-12-31T23:30:00Z", "2025-12-31T23:30:00+00:00", "2025-12-31"),
        ("2025-12-31T23:59:00Z", "2025-12-31T23:59:00+00:00", "2025-12-31"),
        ("2026-01-01T00:00:00Z", "2026-01-01T00:00:00+00:00", "2026-01-01"),
        ("2026-01-01T00:30:00Z", "2026-01-01T00:30:00+00:00", "2026-01-01"),
        ("2026-01-01T00:59:00Z", "2026-01-01T00:59:00+00:00", "2026-01-01"),
        ("2026-01-01T01:00:00Z", "2026-01-01T01:00:00+00:00", "2026-01-01"),
        ("2026-01-01T01:30:00Z", "2026-01-01T01:30:00+00:00", "2026-01-01"),
        ("2026-01-01T01:59:00Z", "2026-01-01T01:59:00+00:00", "2026-01-01"),
        ("2026-01-01T02:00:00Z", "2026-01-01T02:00:00+00:00", "2026-01-01"),
        ("2026-01-01T02:30:00Z", "2026-01-01T02:30:00+00:00", "2026-01-01"),
        ("2026-01-15T22:00:00Z", "2026-01-15T22:00:00+00:00", "2026-01-15"),
        ("2026-01-15T22:59:00Z", "2026-01-15T22:59:00+00:00", "2026-01-15"),
        ("2026-01-15T23:00:00Z", "2026-01-15T23:00:00+00:00", "2026-01-15"),
        ("2026-01-15T23:30:00Z", "2026-01-15T23:30:00+00:00", "2026-01-15"),
        ("2026-01-15T23:59:00Z", "2026-01-15T23:59:00+00:00", "2026-01-15"),
        ("2026-01-16T00:00:00Z", "2026-01-16T00:00:00+00:00", "2026-01-16"),
        ("2026-01-16T00:30:00Z", "2026-01-16T00:30:00+00:00", "2026-01-16"),
        ("2026-01-16T00:59:00Z", "2026-01-16T00:59:00+00:00", "2026-01-16"),
        ("2026-01-16T01:00:00Z", "2026-01-16T01:00:00+00:00", "2026-01-16"),
        ("2026-01-16T01:30:00Z", "2026-01-16T01:30:00+00:00", "2026-01-16"),
        ("2026-01-16T01:59:00Z", "2026-01-16T01:59:00+00:00", "2026-01-16"),
        ("2026-01-16T02:00:00Z", "2026-01-16T02:00:00+00:00", "2026-01-16"),
        ("2026-01-16T02:30:00Z", "2026-01-16T02:30:00+00:00", "2026-01-16"),
        ("2026-03-28T22:00:00Z", "2026-03-28T22:00:00+00:00", "2026-03-28"),
        ("2026-03-28T23:00:00Z", "2026-03-28T23:00:00+00:00", "2026-03-28"),
        ("2026-03-28T23:30:00Z", "2026-03-28T23:30:00+00:00", "2026-03-28"),
        ("2026-03-29T00:00:00Z", "2026-03-29T00:00:00+00:00", "2026-03-29"),
        ("2026-03-29T00:30:00Z", "2026-03-29T00:30:00+00:00", "2026-03-29"),
        ("2026-03-29T00:59:00Z", "2026-03-29T00:59:00+00:00", "2026-03-29"),
        ("2026-03-29T01:00:00Z", "2026-03-29T02:00:00+01:00", "2026-03-29"),
        ("2026-03-29T01:01:00Z", "2026-03-29T02:01:00+01:00", "2026-03-29"),
        ("2026-03-29T01:30:00Z", "2026-03-29T02:30:00+01:00", "2026-03-29"),
        ("2026-03-29T01:59:00Z", "2026-03-29T02:59:00+01:00", "2026-03-29"),
        ("2026-03-29T02:00:00Z", "2026-03-29T03:00:00+01:00", "2026-03-29"),
        ("2026-03-29T02:30:00Z", "2026-03-29T03:30:00+01:00", "2026-03-29"),
        ("2026-03-29T03:00:00Z", "2026-03-29T04:00:00+01:00", "2026-03-29"),
        ("2026-03-29T12:00:00Z", "2026-03-29T13:00:00+01:00", "2026-03-29"),
        ("2026-03-29T22:59:00Z", "2026-03-29T23:59:00+01:00", "2026-03-29"),
        ("2026-03-29T23:00:00Z", "2026-03-30T00:00:00+01:00", "2026-03-30"),
        ("2026-03-29T23:30:00Z", "2026-03-30T00:30:00+01:00", "2026-03-30"),
        ("2026-03-30T00:30:00Z", "2026-03-30T01:30:00+01:00", "2026-03-30"),
        ("2026-06-30T22:00:00Z", "2026-06-30T23:00:00+01:00", "2026-06-30"),
        ("2026-06-30T22:59:00Z", "2026-06-30T23:59:00+01:00", "2026-06-30"),
        ("2026-06-30T23:00:00Z", "2026-07-01T00:00:00+01:00", "2026-07-01"),
        ("2026-06-30T23:30:00Z", "2026-07-01T00:30:00+01:00", "2026-07-01"),
        ("2026-06-30T23:59:00Z", "2026-07-01T00:59:00+01:00", "2026-07-01"),
        ("2026-07-01T00:00:00Z", "2026-07-01T01:00:00+01:00", "2026-07-01"),
        ("2026-07-01T00:30:00Z", "2026-07-01T01:30:00+01:00", "2026-07-01"),
        ("2026-07-01T00:59:00Z", "2026-07-01T01:59:00+01:00", "2026-07-01"),
        ("2026-07-01T01:00:00Z", "2026-07-01T02:00:00+01:00", "2026-07-01"),
        ("2026-07-01T01:30:00Z", "2026-07-01T02:30:00+01:00", "2026-07-01"),
        ("2026-07-01T01:59:00Z", "2026-07-01T02:59:00+01:00", "2026-07-01"),
        ("2026-07-01T02:00:00Z", "2026-07-01T03:00:00+01:00", "2026-07-01"),
        ("2026-07-01T02:30:00Z", "2026-07-01T03:30:00+01:00", "2026-07-01"),
        ("2026-07-15T22:00:00Z", "2026-07-15T23:00:00+01:00", "2026-07-15"),
        ("2026-07-15T22:59:00Z", "2026-07-15T23:59:00+01:00", "2026-07-15"),
        ("2026-07-15T23:00:00Z", "2026-07-16T00:00:00+01:00", "2026-07-16"),
        ("2026-07-15T23:30:00Z", "2026-07-16T00:30:00+01:00", "2026-07-16"),
        ("2026-07-15T23:59:00Z", "2026-07-16T00:59:00+01:00", "2026-07-16"),
        ("2026-07-16T00:00:00Z", "2026-07-16T01:00:00+01:00", "2026-07-16"),
        ("2026-07-16T00:30:00Z", "2026-07-16T01:30:00+01:00", "2026-07-16"),
        ("2026-07-16T00:59:00Z", "2026-07-16T01:59:00+01:00", "2026-07-16"),
        ("2026-07-16T01:00:00Z", "2026-07-16T02:00:00+01:00", "2026-07-16"),
        ("2026-07-16T01:30:00Z", "2026-07-16T02:30:00+01:00", "2026-07-16"),
        ("2026-07-16T01:59:00Z", "2026-07-16T02:59:00+01:00", "2026-07-16"),
        ("2026-07-16T02:00:00Z", "2026-07-16T03:00:00+01:00", "2026-07-16"),
        ("2026-07-16T02:30:00Z", "2026-07-16T03:30:00+01:00", "2026-07-16"),
        ("2026-10-24T22:00:00Z", "2026-10-24T23:00:00+01:00", "2026-10-24"),
        ("2026-10-24T23:00:00Z", "2026-10-25T00:00:00+01:00", "2026-10-25"),
        ("2026-10-24T23:30:00Z", "2026-10-25T00:30:00+01:00", "2026-10-25"),
        ("2026-10-25T00:00:00Z", "2026-10-25T01:00:00+01:00", "2026-10-25"),
        ("2026-10-25T00:30:00Z", "2026-10-25T01:30:00+01:00", "2026-10-25"),
        ("2026-10-25T00:59:00Z", "2026-10-25T01:59:00+01:00", "2026-10-25"),
        ("2026-10-25T01:00:00Z", "2026-10-25T01:00:00+00:00", "2026-10-25"),
        ("2026-10-25T01:01:00Z", "2026-10-25T01:01:00+00:00", "2026-10-25"),
        ("2026-10-25T01:30:00Z", "2026-10-25T01:30:00+00:00", "2026-10-25"),
        ("2026-10-25T01:59:00Z", "2026-10-25T01:59:00+00:00", "2026-10-25"),
        ("2026-10-25T02:00:00Z", "2026-10-25T02:00:00+00:00", "2026-10-25"),
        ("2026-10-25T02:30:00Z", "2026-10-25T02:30:00+00:00", "2026-10-25"),
        ("2026-10-25T03:00:00Z", "2026-10-25T03:00:00+00:00", "2026-10-25"),
        ("2026-10-25T12:00:00Z", "2026-10-25T12:00:00+00:00", "2026-10-25"),
        ("2026-10-25T22:59:00Z", "2026-10-25T22:59:00+00:00", "2026-10-25"),
        ("2026-10-25T23:00:00Z", "2026-10-25T23:00:00+00:00", "2026-10-25"),
        ("2026-10-25T23:30:00Z", "2026-10-25T23:30:00+00:00", "2026-10-25"),
        ("2026-10-26T00:30:00Z", "2026-10-26T00:30:00+00:00", "2026-10-26"),
    ];

    #[test]
    fn london_now_and_today_follow_the_pinned_instant() {
        for &(instant, local, date) in NOW {
            let _pin = pin_london_now_for_tests(instant.parse().unwrap());
            assert_eq!(london_now().to_rfc3339(), local, "london_now at {instant}");
            assert_eq!(
                london_today(),
                date.parse().unwrap(),
                "london_today at {instant}"
            );
        }
    }

    #[test]
    fn the_pin_is_lifted_when_the_guard_is_dropped() {
        {
            let _pin = pin_london_now_for_tests("2020-01-01T00:00:00Z".parse().unwrap());
            assert_eq!(london_today(), "2020-01-01".parse().unwrap());
        }
        assert!(london_today() > "2025-01-01".parse().unwrap());
    }
}
