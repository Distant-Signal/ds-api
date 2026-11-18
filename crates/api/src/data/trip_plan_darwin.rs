//! Darwin Push Port times for the `/Trips/plan` live overlay (DS-591).
//!
//! TRUST is the planner's default live source, but it has no report for a
//! minority of trains (a station outside the line catalogue, a train with
//! no activation, a stale state) and reports at timing points only, where
//! Darwin has actuals at every call. A late train TRUST has no data for was
//! planned as on time. So [`crate::data::trip_plan_live::fetch_train_lives`]
//! (the per-train reads of a plan, never the in-search snapshot) also reads
//! `darwin_calls` for the trains it reads, here, and
//! [`crate::data::trip_plan_live::train_live`] uses them by these rules,
//! per side (arrival, departure) of each call:
//!
//! 1. A call TRUST has reported keeps TRUST's actual: Darwin is not
//!    consulted for it, so it can never lower a reported delay.
//! 2. Otherwise a Darwin actual (`arrival_at`/`departure_at`) is used: an
//!    actual always beats a carried estimate.
//! 3. Otherwise TRUST's carried estimate, when the train's TRUST state is
//!    fresh (the delay TRUST last reported, carried flat to the calls
//!    ahead). TRUST is the default: Darwin does not overrule it.
//! 4. Otherwise (no TRUST state, or a stale one) Darwin's forecast
//!    (`arrival_et`/`departure_et`), as an estimate.
//!
//! The Darwin read is one batched query per call of `fetch_train_lives`
//! (the trains of the plan being annotated or re-planned, at most
//! `TRIP_PLAN_LIVE_MAX_TRAINS` in all), through `darwin_services_uid_ssd`
//! and `darwin_calls`' primary key. It is skipped for an empty train list.
//! Journey feasibility, journey breaks and notifications do not use it.
//!
//! Darwin's times are `HH:MM` Europe/London strings. Each is dated against
//! the call's own scheduled instant ([`resolve_london_estimate_near`]), so
//! a forecast after midnight, a train running over it, and the autumn
//! repeated hour all land on the right day.
//!
//! `distant_signal_api_trip_plan_darwin_fallback_total{kind}` counts trains
//! for which Darwin filled something in: `actuals` (a Darwin actual time
//! was used) and `forecasts` (a Darwin forecast was).

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, NaiveTime, Timelike, Utc};
use ds_store::darwin::DarwinCall;
use sqlx::PgPool;

use crate::data::eta_blend::resolve_london_estimate_near;
use crate::data::journey::JourneyStop;
use crate::data::pushport_platform::find_call;

/// `distant_signal_api_trip_plan_darwin_fallback_total{kind}`: trains whose
/// live profile Darwin filled in.
pub const FALLBACK_METRIC: &str = "api_trip_plan_darwin_fallback_total";

/// The `kind` labels of [`FALLBACK_METRIC`].
const KINDS: [&str; 2] = ["actuals", "forecasts"];

/// Registers [`FALLBACK_METRIC`] at 0 at startup, so an alert's
/// `increase()` sees the first one after a restart.
pub fn register_metrics() {
    for kind in KINDS {
        metrics::counter!(ds_runtime::metrics::metric_name(FALLBACK_METRIC), "kind" => kind)
            .increment(0);
    }
}

/// What Darwin filled into one train's profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DarwinFill {
    /// At least one call's time is a Darwin actual.
    pub actuals: bool,
    /// At least one call's time is a Darwin forecast.
    pub forecasts: bool,
}

impl DarwinFill {
    pub(crate) fn merge(&mut self, other: Self) {
        self.actuals |= other.actuals;
        self.forecasts |= other.forecasts;
    }

    /// Counts the train on [`FALLBACK_METRIC`].
    pub fn record(self) {
        for (kind, hit) in [("actuals", self.actuals), ("forecasts", self.forecasts)] {
            if hit {
                metrics::counter!(
                    ds_runtime::metrics::metric_name(FALLBACK_METRIC),
                    "kind" => kind
                )
                .increment(1);
            }
        }
    }
}

/// Darwin's times for one stop of a train, as instants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StopDarwin {
    pub arrival_actual: Option<DateTime<Utc>>,
    pub arrival_forecast: Option<DateTime<Utc>>,
    pub departure_actual: Option<DateTime<Utc>>,
    pub departure_forecast: Option<DateTime<Utc>>,
}

/// The actual times of a `darwin_calls` row, which [`DarwinCall`] (ds-core)
/// does not carry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Actuals {
    arrival: Option<String>,
    departure: Option<String>,
}

/// One train's Push Port locations: `calls` (matched to stops by
/// `pushport_platform::find_call`) and, at the same index, `actuals`.
#[derive(Debug, Clone, Default)]
pub struct TrainCalls {
    calls: Vec<DarwinCall>,
    actuals: Vec<Actuals>,
}

#[derive(sqlx::FromRow)]
struct Row {
    uid: String,
    arrival_at: Option<String>,
    departure_at: Option<String>,
    #[sqlx(flatten)]
    call: DarwinCall,
}

/// The Push Port locations of every train in `uids` running on `date`, by
/// UID: the live service Darwin heard of last, and only the locations of
/// its newest schedule (as `ds_store::darwin::calls_for_trains`, whose
/// reasoning applies: a re-sent schedule leaves old rows behind). A train
/// Darwin does not know is absent. Never writes.
pub async fn fetch_calls(
    pool: &PgPool,
    date: NaiveDate,
    uids: &[&str],
) -> anyhow::Result<HashMap<String, TrainCalls>> {
    if uids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<Row> = sqlx::query_as(
        "WITH s AS ( \
             SELECT DISTINCT ON (d.uid) d.rid, d.uid \
             FROM darwin_services d \
             WHERE d.ssd = $1 AND d.uid = ANY($2) AND NOT d.is_deleted \
             ORDER BY d.uid, d.message_ts DESC \
         ), \
         c AS ( \
             SELECT c.*, max(c.schedule_ts) OVER (PARTITION BY c.rid) AS newest_schedule_ts \
             FROM s JOIN darwin_calls c ON c.rid = s.rid \
         ) \
         SELECT s.uid, c.tiploc, c.wta, c.wtd, c.wtp, c.planned_platform, c.is_cancelled, \
                c.platform, c.platform_confirmed, c.platform_suppressed, \
                c.platform_cis_suppressed, c.platform_basis, c.arrival_et, c.arrival_at, \
                c.departure_et, c.departure_at, c.updated_at \
         FROM s JOIN c ON c.rid = s.rid \
         WHERE c.newest_schedule_ts IS NULL \
            OR GREATEST(c.schedule_ts, c.ts_ts) >= c.newest_schedule_ts",
    )
    .bind(date)
    .bind(uids)
    .fetch_all(pool)
    .await?;
    let mut out: HashMap<String, TrainCalls> = HashMap::new();
    for row in rows {
        let train = out.entry(row.uid).or_default();
        train.calls.push(row.call);
        train.actuals.push(Actuals {
            arrival: row.arrival_at,
            departure: row.departure_at,
        });
    }
    Ok(out)
}

/// A Darwin time (`HH:MM`, or `HH:MM:SS`) to the minute; `None` when blank
/// or malformed.
fn parse_minute(raw: Option<&str>) -> Option<NaiveTime> {
    let raw = raw?.trim();
    let time = NaiveTime::parse_from_str(raw, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(raw, "%H:%M"))
        .ok()?;
    NaiveTime::from_hms_opt(time.hour(), time.minute(), 0)
}

/// `raw` as the instant nearest `scheduled`, which it estimates or
/// confirms; `None` without a scheduled time to date it against.
fn instant_near(raw: Option<&str>, scheduled: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    resolve_london_estimate_near(scheduled?, parse_minute(raw)?)
}

/// Darwin's times for each of `stops` (index for index), `None` for a stop
/// Darwin has no location for, or has cancelled. Times are dated against
/// the stop's public time (the one a delay is measured against).
pub fn resolve(stops: &[JourneyStop], train: &TrainCalls) -> Vec<Option<StopDarwin>> {
    (0..stops.len())
        .map(|index| {
            let matched = find_call(stops, index, &train.calls)?;
            let call = &train.calls[matched];
            if call.is_cancelled {
                return None;
            }
            let actuals = &train.actuals[matched];
            let timetable = &stops[index].timetable;
            let arrival = timetable.scheduled_arrival();
            let departure = timetable.scheduled_departure();
            Some(StopDarwin {
                arrival_actual: instant_near(actuals.arrival.as_deref(), arrival),
                arrival_forecast: instant_near(call.arrival_et.as_deref(), arrival),
                departure_actual: instant_near(actuals.departure.as_deref(), departure),
                departure_forecast: instant_near(call.departure_et.as_deref(), departure),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use schedule_query::CallingPointKind;

    use super::*;
    use crate::data::journey::test_support::stop;

    fn call(tiploc: &str, wta: &str, wtd: &str) -> DarwinCall {
        DarwinCall {
            tiploc: tiploc.to_string(),
            wta: wta.to_string(),
            wtd: wtd.to_string(),
            wtp: String::new(),
            planned_platform: None,
            is_cancelled: false,
            platform: None,
            platform_confirmed: false,
            platform_suppressed: false,
            platform_cis_suppressed: false,
            platform_basis: None,
            arrival_et: None,
            departure_et: None,
            updated_at: "2026-09-28T07:00:00Z".parse().expect("instant"),
        }
    }

    fn train(calls: Vec<DarwinCall>, actuals: Vec<(Option<&str>, Option<&str>)>) -> TrainCalls {
        TrainCalls {
            calls,
            actuals: actuals
                .into_iter()
                .map(|(arrival, departure)| Actuals {
                    arrival: arrival.map(str::to_string),
                    departure: departure.map(str::to_string),
                })
                .collect(),
        }
    }

    /// A stop at `tiploc` whose public and working times are `arrival` and
    /// `departure` (UTC).
    fn timed(tiploc: &str, arrival: Option<&str>, departure: Option<&str>) -> JourneyStop {
        let mut s = stop("ZZZ", tiploc, CallingPointKind::Intermediate, None);
        let parse = |t: &str| t.parse::<DateTime<Utc>>().expect("instant");
        s.timetable.public_arrival = arrival.map(parse);
        s.timetable.working_minute_arrival = arrival.map(parse);
        s.timetable.working_arrival = arrival.map(parse);
        s.timetable.public_departure = departure.map(parse);
        s.timetable.working_minute_departure = departure.map(parse);
        s.timetable.working_departure = departure.map(parse);
        s
    }

    #[test]
    fn forecasts_and_actuals_are_dated_against_the_stops_own_time() {
        // 2026-09-28 is BST: 10:00 local is 09:00Z.
        let stops = vec![timed(
            "YORK",
            Some("2026-09-28T09:00:00Z"),
            Some("2026-09-28T09:02:00Z"),
        )];
        let mut c = call("YORK", "10:00", "10:02");
        c.arrival_et = Some("10:07".to_string());
        c.departure_et = Some("10:09".to_string());
        let train = train(vec![c], vec![(Some("10:06"), None)]);
        let found = resolve(&stops, &train);
        let darwin = found[0].expect("matched");
        let at = |s: &str| Some(s.parse::<DateTime<Utc>>().expect("instant"));
        assert_eq!(darwin.arrival_forecast, at("2026-09-28T09:07:00Z"));
        assert_eq!(darwin.arrival_actual, at("2026-09-28T09:06:00Z"));
        assert_eq!(darwin.departure_forecast, at("2026-09-28T09:09:00Z"));
        assert_eq!(darwin.departure_actual, None);
    }

    #[test]
    fn a_forecast_past_midnight_is_the_next_day() {
        // Booked 23:55 BST on the 28th (22:55Z); Darwin forecasts 00:10,
        // which is the 29th, not 23 hours and 45 minutes early.
        let stops = vec![timed("YORK", Some("2026-09-28T22:55:00Z"), None)];
        let mut c = call("YORK", "23:55", "");
        c.arrival_et = Some("00:10".to_string());
        let found = resolve(&stops, &train(vec![c], vec![(None, None)]));
        let forecast = found[0].and_then(|d| d.arrival_forecast).expect("forecast");
        assert_eq!(
            forecast,
            "2026-09-28T23:10:00Z"
                .parse::<DateTime<Utc>>()
                .expect("instant")
        );
        assert_eq!(
            forecast
                - "2026-09-28T22:55:00Z"
                    .parse::<DateTime<Utc>>()
                    .expect("instant"),
            Duration::minutes(15)
        );
    }

    #[test]
    fn a_stop_booked_after_midnight_with_an_earlier_evening_time_is_early_not_a_day_late() {
        // Booked 00:05 BST on the 29th (23:05Z on the 28th); forecast
        // 23:58, the evening before: seven minutes early.
        let stops = vec![timed("YORK", Some("2026-09-28T23:05:00Z"), None)];
        let mut c = call("YORK", "00:05", "");
        c.arrival_et = Some("23:58".to_string());
        let found = resolve(&stops, &train(vec![c], vec![(None, None)]));
        let forecast = found[0].and_then(|d| d.arrival_forecast).expect("forecast");
        assert_eq!(
            forecast,
            "2026-09-28T22:58:00Z"
                .parse::<DateTime<Utc>>()
                .expect("instant")
        );
    }

    #[test]
    fn a_cancelled_or_unmatched_location_gives_nothing() {
        let stops = vec![
            timed("YORK", Some("2026-09-28T09:00:00Z"), None),
            timed("LEEDS", Some("2026-09-28T09:30:00Z"), None),
        ];
        let mut cancelled = call("YORK", "10:00", "");
        cancelled.arrival_et = Some("10:20".to_string());
        cancelled.is_cancelled = true;
        let found = resolve(&stops, &train(vec![cancelled], vec![(None, None)]));
        assert_eq!(found, [None, None]);
    }

    #[test]
    fn blank_and_malformed_times_are_ignored() {
        let stops = vec![timed("YORK", Some("2026-09-28T09:00:00Z"), None)];
        let mut c = call("YORK", "10:00", "");
        c.arrival_et = Some(String::new());
        let found = resolve(&stops, &train(vec![c], vec![(Some("soon"), None)]));
        assert_eq!(found[0], Some(StopDarwin::default()));
    }

    #[test]
    fn a_time_with_seconds_is_cut_to_the_minute() {
        assert_eq!(
            parse_minute(Some("10:07:30")),
            NaiveTime::from_hms_opt(10, 7, 0)
        );
        assert_eq!(
            parse_minute(Some(" 10:07 ")),
            NaiveTime::from_hms_opt(10, 7, 0)
        );
        assert_eq!(parse_minute(None), None);
    }
}
