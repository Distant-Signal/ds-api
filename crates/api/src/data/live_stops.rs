//! Stops for live-only trains (DS-534): a train with a live `trains` row
//! but no CIF schedule rows, such as a late-added special the daily full
//! timetable never carried (`U06535` on 2026-11-16, headcode `1Z72`).
//!
//! [`crate::data::journey::build_journey_stops_batch_sourced`] reads a
//! train's stops from `trains.calling_points`, else from
//! `schedule_calling_points_full`. When both are empty it used to return no
//! stops at all. With the fallbacks on, it tries, in order:
//!
//! 1. **Darwin** (`DARWIN_STOPS_FALLBACK_ENABLED`, default off): the Darwin
//!    Push Port's own schedule for the service (`darwin_services`,
//!    `darwin_calls`), found by `(uid, ssd)`, else by the TRUST id's
//!    headcode on the date when exactly one Darwin-only service carries it.
//!    The calls become the same [`RawCallingPoint`]s the CIF path builds, so
//!    TIPLOC to CRS, names, movement events, platforms and delay estimates
//!    all work unchanged. A deleted service, or one with every call
//!    cancelled, gives no stops (and stops the TRUST fallback too: Darwin
//!    says it does not run); a part-cancelled one keeps its other calls.
//! 2. **TRUST** (`TRUST_STOPS_FALLBACK_ENABLED`, default off), only when
//!    Darwin has no row at all: the stops the train has already reported
//!    (`train_movement_events` arrivals and departures, at a STANOX the
//!    `stanox_crs` crosswalk knows). Nothing ahead of the last report is
//!    invented.
//!
//! CIF always wins: neither fallback runs for a train with CIF stops. The
//! source is served as `scheduleSource` on `GET /Train/by-uid/{uid}/{date}`,
//! the only route that turns the fallbacks on (see
//! `routes::train::attach_journey_stops_public`); every other caller of
//! `build_journey_stops_batch` keeps CIF-only stops.
//!
//! Darwin's tables are pruned a few days after the service date, so an
//! older live-only train has no Darwin stops; the TRUST fallback (also
//! pruned, but later) or nothing remains, and the route still answers.
//!
//! Each fallback hit counts on `distant_signal_api_journey_stops_fallback_total`
//! with `source` `darwin` or `trust`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, NaiveDate, NaiveTime, Timelike, Utc};
use serde::Serialize;
use sqlx::PgPool;

use ds_store::darwin::DarwinCall;
use ds_store::trains::types::{RawCallingPoint, RawTimetable};
use schedule_query::CallingPointKind;

use crate::data::journey::JourneyStopsRequest;

/// Turns the Darwin fallback on (default false).
pub const DARWIN_ENABLED_ENV: &str = "DARWIN_STOPS_FALLBACK_ENABLED";
/// Turns the TRUST fallback on (default false).
pub const TRUST_ENABLED_ENV: &str = "TRUST_STOPS_FALLBACK_ENABLED";

/// `distant_signal_api_journey_stops_fallback_total{source}`: one per train
/// whose stops came from a fallback.
pub const FALLBACK_METRIC: &str = "api_journey_stops_fallback_total";

static DARWIN_ENABLED: AtomicBool = AtomicBool::new(false);
static TRUST_ENABLED: AtomicBool = AtomicBool::new(false);

/// Where a train's stops came from, served as `scheduleSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ScheduleSource {
    /// The CIF timetable (`trains.calling_points` or
    /// `schedule_calling_points_full`): every scheduled train.
    Cif,
    /// The Darwin Push Port's schedule for a train the CIF timetable lacks.
    Darwin,
    /// Only the stops TRUST has reported so far: no stops ahead are known.
    Trust,
}

impl ScheduleSource {
    fn label(self) -> &'static str {
        match self {
            ScheduleSource::Cif => "cif",
            ScheduleSource::Darwin => "darwin",
            ScheduleSource::Trust => "trust",
        }
    }
}

/// Which fallbacks a stop build may use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LiveStopFallbacks {
    pub darwin: bool,
    pub trust: bool,
}

impl LiveStopFallbacks {
    /// CIF only: what every caller but the public train route uses.
    pub const OFF: Self = Self {
        darwin: false,
        trust: false,
    };

    fn any(self) -> bool {
        self.darwin || self.trust
    }
}

/// Reads [`DARWIN_ENABLED_ENV`] and [`TRUST_ENABLED_ENV`] (both default
/// false) once, at startup, registers the fallback counter at 0, and
/// returns the setting.
pub fn init_from_env() -> LiveStopFallbacks {
    let fallbacks = LiveStopFallbacks {
        darwin: crate::data::trip_plan_live::env_bool(DARWIN_ENABLED_ENV, false),
        trust: crate::data::trip_plan_live::env_bool(TRUST_ENABLED_ENV, false),
    };
    DARWIN_ENABLED.store(fallbacks.darwin, Ordering::Relaxed);
    TRUST_ENABLED.store(fallbacks.trust, Ordering::Relaxed);
    for source in [ScheduleSource::Darwin, ScheduleSource::Trust] {
        metrics::counter!(
            ds_runtime::metrics::metric_name(FALLBACK_METRIC),
            "source" => source.label()
        )
        .absolute(0);
    }
    fallbacks
}

/// The fallbacks [`init_from_env`] turned on.
pub fn fallbacks() -> LiveStopFallbacks {
    LiveStopFallbacks {
        darwin: DARWIN_ENABLED.load(Ordering::Relaxed),
        trust: TRUST_ENABLED.load(Ordering::Relaxed),
    }
}

fn record_hit(source: ScheduleSource) {
    metrics::counter!(
        ds_runtime::metrics::metric_name(FALLBACK_METRIC),
        "source" => source.label()
    )
    .increment(1);
}

type RawResult = anyhow::Result<Option<Vec<RawCallingPoint>>>;

/// Fills each request whose CIF read came back empty (`Ok(None)`) from the
/// enabled fallbacks, setting its `sources` entry. Best effort: a failed
/// fallback read is logged and leaves those trains without stops, never
/// failing the batch's CIF trains.
///
/// Returns the Push Port calls of every train whose stops came from Darwin,
/// keyed like `ds_store::darwin::calls_for_trains`, so the platform overlay
/// also covers a service found by headcode (its Darwin UID differs).
pub(crate) async fn fill_fallbacks(
    pool: &PgPool,
    requests: &[JourneyStopsRequest<'_>],
    raws: &mut [RawResult],
    sources: &mut [ScheduleSource],
    fallbacks: LiveStopFallbacks,
) -> HashMap<(String, NaiveDate), Vec<DarwinCall>> {
    let mut darwin_calls = HashMap::new();
    if !fallbacks.any() {
        return darwin_calls;
    }
    let empty: Vec<usize> = raws
        .iter()
        .enumerate()
        .filter(|(_, raw)| matches!(raw, Ok(None)))
        .map(|(index, _)| index)
        .collect();
    if empty.is_empty() {
        return darwin_calls;
    }

    // Trains Darwin says do not run: no TRUST fallback either.
    let mut withdrawn: Vec<usize> = Vec::new();
    if fallbacks.darwin {
        match darwin_services_for(pool, requests, &empty).await {
            Ok(found) => {
                for (index, service) in found {
                    if service.is_withdrawn() {
                        withdrawn.push(index);
                        continue;
                    }
                    let raw = raw_calling_points_from_darwin(&service.calls);
                    if raw.is_empty() {
                        continue;
                    }
                    record_hit(ScheduleSource::Darwin);
                    raws[index] = Ok(Some(raw));
                    sources[index] = ScheduleSource::Darwin;
                    let request = &requests[index];
                    darwin_calls.insert(
                        (request.train_uid.to_string(), request.service_date),
                        service.calls.into_iter().map(|call| call.call).collect(),
                    );
                }
            }
            Err(err) => tracing::warn!(error = ?err, "could not read Darwin stops fallback"),
        }
    }

    if fallbacks.trust {
        let still_empty: Vec<usize> = empty
            .into_iter()
            .filter(|index| matches!(raws[*index], Ok(None)) && !withdrawn.contains(index))
            .collect();
        if !still_empty.is_empty() {
            let trains_ids: Vec<i64> = still_empty
                .iter()
                .map(|&index| requests[index].trains_id)
                .collect();
            match trust_events_for(pool, &trains_ids).await {
                Ok(events) => {
                    for index in still_empty {
                        let request = &requests[index];
                        let Some(events) = events.get(&request.trains_id) else {
                            continue;
                        };
                        let raw = raw_calling_points_from_trust(events, request.service_date);
                        if raw.is_empty() {
                            continue;
                        }
                        record_hit(ScheduleSource::Trust);
                        raws[index] = Ok(Some(raw));
                        sources[index] = ScheduleSource::Trust;
                    }
                }
                Err(err) => tracing::warn!(error = ?err, "could not read TRUST stops fallback"),
            }
        }
    }
    darwin_calls
}

// --------------------------------------------------------------------------
// Darwin
// --------------------------------------------------------------------------

/// One `darwin_calls` row: the overlay's [`DarwinCall`] plus the public
/// times.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct DarwinStopCall {
    pub pta: Option<String>,
    pub ptd: Option<String>,
    #[sqlx(flatten)]
    pub call: DarwinCall,
}

/// A Darwin service chosen for a request, with its calls.
#[derive(Debug)]
struct DarwinService {
    is_deleted: bool,
    calls: Vec<DarwinStopCall>,
}

impl DarwinService {
    /// Deleted (no longer runs) or cancelled as a whole: every call is
    /// cancelled. Darwin's schedule-level `cancelReason` is not enough on
    /// its own: it is also set for a part cancellation (a train cut short
    /// or missing one stop), which still runs.
    fn is_withdrawn(&self) -> bool {
        self.is_deleted
            || (!self.calls.is_empty() && self.calls.iter().all(|call| call.call.is_cancelled))
    }
}

#[derive(sqlx::FromRow)]
struct ServiceRow {
    idx: i32,
    rid: String,
    is_deleted: bool,
}

#[derive(sqlx::FromRow)]
struct CallRow {
    rid: String,
    #[sqlx(flatten)]
    call: DarwinStopCall,
}

/// The Darwin service for each request index in `indexes`, by `(uid, ssd)`
/// (the `darwin_services_uid_ssd` index), else by headcode: the TRUST id's
/// headcode (`trains.train_id` characters 3 to 6) on the service date,
/// accepted only when exactly one live Darwin service that has no CIF
/// schedule and no other `trains` row of its own carries it (headcodes
/// repeat across the country, and a service with its own row is another
/// train).
/// That second read uses `darwin_services_ssd`; a `(ssd, train_id)` index
/// would serve it better if it ever shows up as slow (ds-core migration,
/// not added here).
///
/// When Darwin holds several RIDs for one `(uid, ssd)`, a live one is
/// preferred over a deleted one, then the newest message.
async fn darwin_services_for(
    pool: &PgPool,
    requests: &[JourneyStopsRequest<'_>],
    indexes: &[usize],
) -> anyhow::Result<Vec<(usize, DarwinService)>> {
    let idx: Vec<i32> = indexes
        .iter()
        .map(|&i| i32::try_from(i).unwrap_or(i32::MAX))
        .collect();
    let uids: Vec<&str> = indexes.iter().map(|&i| requests[i].train_uid).collect();
    let dates: Vec<NaiveDate> = indexes.iter().map(|&i| requests[i].service_date).collect();
    let mut services: Vec<ServiceRow> = sqlx::query_as(
        "SELECT DISTINCT ON (k.idx) k.idx, d.rid, d.is_deleted \
         FROM UNNEST($1::int[], $2::text[], $3::date[]) AS k(idx, uid, ssd) \
         JOIN darwin_services d ON d.uid = k.uid AND d.ssd = k.ssd \
         ORDER BY k.idx, d.is_deleted, d.message_ts DESC",
    )
    .bind(&idx)
    .bind(&uids)
    .bind(&dates)
    .fetch_all(pool)
    .await?;

    let by_uid: Vec<i32> = services.iter().map(|s| s.idx).collect();
    let rest: Vec<i32> = idx
        .iter()
        .copied()
        .filter(|i| !by_uid.contains(i))
        .collect();
    if !rest.is_empty() {
        let trains_ids: Vec<i64> = rest
            .iter()
            .map(|&i| requests[usize::try_from(i).unwrap_or(usize::MAX)].trains_id)
            .collect();
        let candidates: Vec<ServiceRow> = sqlx::query_as(
            "SELECT k.idx, d.rid, d.is_deleted \
             FROM UNNEST($1::int[], $2::bigint[]) AS k(idx, trains_id) \
             JOIN trains tr ON tr.id = k.trains_id \
             JOIN darwin_services d \
               ON d.ssd = tr.service_date AND d.train_id = UPPER(SUBSTR(tr.train_id, 3, 4)) \
             WHERE LENGTH(tr.train_id) = 10 \
               AND NOT d.is_deleted \
               AND NOT EXISTS (SELECT 1 FROM schedule_services s \
                                WHERE s.uid = d.uid AND s.service_date = d.ssd) \
               AND NOT EXISTS (SELECT 1 FROM trains other \
                                WHERE other.train_uid = d.uid \
                                  AND other.service_date = d.ssd \
                                  AND other.id <> tr.id)",
        )
        .bind(&rest)
        .bind(&trains_ids)
        .fetch_all(pool)
        .await?;
        let mut counts: HashMap<i32, usize> = HashMap::new();
        for candidate in &candidates {
            *counts.entry(candidate.idx).or_default() += 1;
        }
        services.extend(
            candidates
                .into_iter()
                .filter(|candidate| counts.get(&candidate.idx) == Some(&1)),
        );
    }
    if services.is_empty() {
        return Ok(Vec::new());
    }

    let rids: Vec<&str> = services.iter().map(|s| s.rid.as_str()).collect();
    let rows: Vec<CallRow> = sqlx::query_as(
        "SELECT rid, tiploc, wta, wtd, wtp, pta, ptd, planned_platform, is_cancelled, platform, \
                platform_confirmed, platform_suppressed, platform_cis_suppressed, platform_basis, \
                arrival_et, departure_et, updated_at \
         FROM darwin_calls WHERE rid = ANY($1)",
    )
    .bind(&rids)
    .fetch_all(pool)
    .await?;
    let mut calls: HashMap<String, Vec<DarwinStopCall>> = HashMap::new();
    for row in rows {
        calls.entry(row.rid).or_default().push(row.call);
    }
    Ok(services
        .into_iter()
        .map(|service| {
            (
                usize::try_from(service.idx).unwrap_or(usize::MAX),
                DarwinService {
                    is_deleted: service.is_deleted,
                    calls: calls.remove(&service.rid).unwrap_or_default(),
                },
            )
        })
        .collect())
}

/// A Darwin time, `"HH:MM"` or `"HH:MM:SS"` (a half minute); `None` when
/// blank or malformed.
fn darwin_time(raw: Option<&str>) -> Option<NaiveTime> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    NaiveTime::parse_from_str(raw, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(raw, "%H:%M"))
        .ok()
}

/// `time` cut to the minute: the CIF `booked_*` shape.
fn to_minute(time: NaiveTime) -> NaiveTime {
    time.with_second(0).unwrap_or(time)
}

const DAY_SECONDS: u32 = 86_400;

/// One service's Darwin calls as [`RawCallingPoint`]s, in running order.
///
/// * Cancelled calls are left out, as is a row with no time at all.
/// * A call with a working arrival or departure is a stop (`booked_*` its
///   working times to the minute, `working_*` exact); one with only a
///   working pass time is a passing point, which the stop list hides as it
///   does a CIF pass.
/// * The public times (`pta`/`ptd`) are carried as the CIF ones are, so a
///   stop's displayed (`scheduled*`) time is its public time, and the
///   working time only where Darwin gives no public one (a working stop of
///   a charter or empty stock move, which a passenger cannot use: such a
///   side is neither boardable nor alightable here).
/// * The first and last stops are the origin and terminus.
/// * Darwin has no sequence, so the calls are put in time order: each call's
///   first working time (arrival, else pass, else departure) on a 24-hour
///   circle, cut at the largest gap. A service lasts well under a day, so
///   the largest gap is the time it is not running, and the call after it
///   is the start; a call whose time is earlier in the day than the start
///   runs past midnight (`day_offset` 1). A dwell across midnight is
///   handled by `departure_day_offset`, as for CIF.
pub(crate) fn raw_calling_points_from_darwin(calls: &[DarwinStopCall]) -> Vec<RawCallingPoint> {
    struct Timed<'a> {
        call: &'a DarwinStopCall,
        wta: Option<NaiveTime>,
        wtd: Option<NaiveTime>,
        wtp: Option<NaiveTime>,
        anchor: u32,
        then: u32,
    }
    let mut timed: Vec<Timed<'_>> = calls
        .iter()
        .filter(|call| !call.call.is_cancelled)
        .filter_map(|call| {
            let wta = darwin_time(Some(&call.call.wta));
            let wtd = darwin_time(Some(&call.call.wtd));
            let wtp = darwin_time(Some(&call.call.wtp));
            let anchor = wta.or(wtp).or(wtd)?.num_seconds_from_midnight();
            let then = wtd.or(wtp).or(wta)?.num_seconds_from_midnight();
            Some(Timed {
                call,
                wta,
                wtd,
                wtp,
                anchor,
                then,
            })
        })
        .collect();
    if timed.is_empty() {
        return Vec::new();
    }
    // A dwell across midnight (arrive 23:58, depart 00:03) sorts by its
    // arrival; `then` only breaks ties.
    timed.sort_by_key(|t| {
        let then = if t.then < t.anchor {
            t.then + DAY_SECONDS
        } else {
            t.then
        };
        (t.anchor, then)
    });
    let last = timed.len() - 1;
    let wrap_gap = timed[0].anchor + DAY_SECONDS - timed[last].anchor;
    let (start, _) = timed
        .windows(2)
        .enumerate()
        .map(|(i, pair)| (i + 1, pair[1].anchor - pair[0].anchor))
        .fold((0, wrap_gap), |best, candidate| {
            if candidate.1 > best.1 {
                candidate
            } else {
                best
            }
        });
    timed.rotate_left(start);
    let start_anchor = timed[0].anchor;

    let first_stop = timed
        .iter()
        .position(|t| t.wta.is_some() || t.wtd.is_some());
    let last_stop = timed
        .iter()
        .rposition(|t| t.wta.is_some() || t.wtd.is_some());
    timed
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let pta = darwin_time(t.call.pta.as_deref());
            let ptd = darwin_time(t.call.ptd.as_deref());
            let is_stop = t.wta.is_some() || t.wtd.is_some();
            let kind = if Some(i) == first_stop {
                CallingPointKind::Origin
            } else if Some(i) == last_stop {
                CallingPointKind::Terminate
            } else {
                CallingPointKind::Intermediate
            };
            RawCallingPoint {
                tiploc: t.call.call.tiploc.clone(),
                kind,
                booked_arrival: t.wta.map(to_minute),
                booked_departure: t.wtd.map(to_minute),
                day_offset: u8::from(t.anchor < start_anchor),
                platform: t.call.call.planned_platform.clone(),
                timetable: RawTimetable {
                    public_arrival: pta,
                    public_departure: ptd,
                    working_arrival: t.wta,
                    working_departure: t.wtd,
                    working_pass: if is_stop { None } else { t.wtp },
                    is_half_minute_arrival: t.wta.is_some_and(|w| w.second() != 0),
                    is_half_minute_departure: t.wtd.is_some_and(|w| w.second() != 0),
                    can_board: is_stop.then_some(ptd.is_some()),
                    can_alight: is_stop.then_some(pta.is_some()),
                    request_stop: None,
                },
            }
        })
        .collect()
}

// --------------------------------------------------------------------------
// TRUST
// --------------------------------------------------------------------------

/// One reported arrival or departure at a location the crosswalk knows.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct TrustStopEvent {
    pub loc_crs: String,
    pub tiploc: String,
    pub event_type: String,
    pub planned_timestamp: DateTime<Utc>,
    pub gbtt_timestamp: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
struct TrustRow {
    trains_id: i64,
    #[sqlx(flatten)]
    event: TrustStopEvent,
}

/// Each train's reported arrivals and departures with a planned time, at a
/// STANOX `stanox_crs` maps (its TIPLOC feeds the usual TIPLOC to CRS
/// pass), in planned-time order.
async fn trust_events_for(
    pool: &PgPool,
    trains_ids: &[i64],
) -> anyhow::Result<HashMap<i64, Vec<TrustStopEvent>>> {
    let rows: Vec<TrustRow> = sqlx::query_as(
        "SELECT e.trains_id, UPPER(e.loc_crs) AS loc_crs, sc.tiploc, e.event_type, \
                e.planned_timestamp, e.gbtt_timestamp \
         FROM train_movement_events e \
         JOIN stanox_crs sc ON sc.stanox = e.loc_stanox \
         WHERE e.trains_id = ANY($1) AND e.loc_crs IS NOT NULL \
           AND e.event_type IN ('ARRIVAL', 'DEPARTURE') \
           AND e.planned_timestamp IS NOT NULL \
         ORDER BY e.trains_id, e.planned_timestamp, e.received_at, e.id",
    )
    .bind(trains_ids)
    .fetch_all(pool)
    .await?;
    let mut grouped: HashMap<i64, Vec<TrustStopEvent>> = HashMap::new();
    for row in rows {
        grouped.entry(row.trains_id).or_default().push(row.event);
    }
    Ok(grouped)
}

/// A London-local time and its day offset from `service_date`.
fn london_time(at: DateTime<Utc>, service_date: NaiveDate) -> (NaiveTime, u8) {
    let local = at.with_timezone(&chrono_tz::Europe::London);
    let days = (local.date_naive() - service_date).num_days();
    (local.time(), u8::try_from(days.clamp(0, 2)).unwrap_or(0))
}

/// The stops `events` (one train's, in planned-time order) report, one
/// per visit: an arrival and the departure after it at the same CRS are one
/// stop, and a departure followed by another arrival there is a second
/// visit (a circular train). A repeated report of the same event (a
/// correction) replaces the earlier one. Planned times are working times;
/// `gbtt_timestamp` is the public time, only at a public stop. Every stop is
/// `Intermediate`: TRUST does not say where the train starts or ends. The
/// last stop is only the latest report, so a caller must not read an
/// arrival there as the train finishing (`journey::confirmed_final_arrival`).
pub(crate) fn raw_calling_points_from_trust(
    events: &[TrustStopEvent],
    service_date: NaiveDate,
) -> Vec<RawCallingPoint> {
    #[derive(Default)]
    struct Visit<'a> {
        crs: &'a str,
        tiploc: &'a str,
        arrival: Option<&'a TrustStopEvent>,
        departure: Option<&'a TrustStopEvent>,
    }
    let mut visits: Vec<Visit<'_>> = Vec::new();
    for event in events {
        let is_arrival = match event.event_type.as_str() {
            "ARRIVAL" => true,
            "DEPARTURE" => false,
            _ => continue,
        };
        let same_visit = visits.last().is_some_and(|visit| {
            visit.crs == event.loc_crs && !(is_arrival && visit.departure.is_some())
        });
        if !same_visit {
            visits.push(Visit {
                crs: &event.loc_crs,
                tiploc: &event.tiploc,
                ..Visit::default()
            });
        }
        let Some(visit) = visits.last_mut() else {
            continue;
        };
        if is_arrival {
            visit.arrival = Some(event);
        } else {
            visit.departure = Some(event);
        }
    }
    visits
        .into_iter()
        .filter_map(|visit| {
            let first = visit.arrival.or(visit.departure)?;
            let (_, day_offset) = london_time(first.planned_timestamp, service_date);
            let working = |event: Option<&TrustStopEvent>| {
                event.map(|e| london_time(e.planned_timestamp, service_date).0)
            };
            let public = |event: Option<&TrustStopEvent>| {
                event
                    .and_then(|e| e.gbtt_timestamp)
                    .map(|at| london_time(at, service_date).0)
            };
            let working_arrival = working(visit.arrival);
            let working_departure = working(visit.departure);
            let public_arrival = public(visit.arrival);
            let public_departure = public(visit.departure);
            let public_stop = public_arrival.is_some() || public_departure.is_some();
            Some(RawCallingPoint {
                tiploc: visit.tiploc.to_string(),
                kind: CallingPointKind::Intermediate,
                booked_arrival: working_arrival.map(to_minute),
                booked_departure: working_departure.map(to_minute),
                day_offset,
                platform: None,
                timetable: RawTimetable {
                    public_arrival,
                    public_departure,
                    working_arrival,
                    working_departure,
                    working_pass: None,
                    is_half_minute_arrival: working_arrival.is_some_and(|w| w.second() != 0),
                    is_half_minute_departure: working_departure.is_some_and(|w| w.second() != 0),
                    // A public stop: the defaults (boarding and alighting).
                    // A working-only one: neither.
                    can_board: (!public_stop).then_some(false),
                    can_alight: (!public_stop).then_some(false),
                    request_stop: None,
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(raw: &str) -> NaiveTime {
        darwin_time(Some(raw)).expect("a valid time")
    }

    /// A Darwin row: `times` is (wta, wtd, wtp, pta, ptd), `""` for none.
    fn call(tiploc: &str, times: [&str; 5], platform: Option<&str>) -> DarwinStopCall {
        let [wta, wtd, wtp, pta, ptd] = times;
        let public = |raw: &str| (!raw.is_empty()).then(|| raw.to_string());
        DarwinStopCall {
            pta: public(pta),
            ptd: public(ptd),
            call: DarwinCall {
                tiploc: tiploc.to_string(),
                wta: wta.to_string(),
                wtd: wtd.to_string(),
                wtp: wtp.to_string(),
                planned_platform: platform.map(str::to_string),
                is_cancelled: false,
                platform: platform.map(str::to_string),
                platform_confirmed: false,
                platform_suppressed: false,
                platform_cis_suppressed: false,
                platform_basis: platform.map(|_| "planned".to_string()),
                arrival_et: None,
                departure_et: None,
                updated_at: DateTime::<Utc>::UNIX_EPOCH,
            },
        }
    }

    /// The `U06535` (1Z72, 2026-11-16) charter's shape: public, working
    /// and pass rows, stored in no particular order.
    fn u06535() -> Vec<DarwinStopCall> {
        vec![
            call("KNGX", ["21:50", "", "", "21:50", ""], Some("8")),
            call("PBRO", ["19:33", "19:41", "", "19:33", "19:41"], Some("2")),
            call("YORK", ["", "16:45", "", "", "16:45"], Some("5")),
            call("RANSKLL", ["17:57", "18:27", "", "", ""], None),
            call("DONC", ["", "", "18:40", "", ""], None),
            call("MILFDY", ["17:10", "17:18", "", "", ""], None),
            call("CNNGTNL", ["19:50", "19:52", "", "", ""], None),
            call("HNTNGDN", ["20:12:30", "20:14", "", "20:13", "20:14"], None),
            call(
                "HATFILD",
                ["21:14", "21:16", "", "21:15", "21:16"],
                Some("1"),
            ),
            call("POTRSBR", ["21:24", "21:26", "", "21:25", "21:26"], None),
        ]
    }

    fn tiplocs(raw: &[RawCallingPoint]) -> Vec<&str> {
        raw.iter().map(|cp| cp.tiploc.as_str()).collect()
    }

    #[test]
    fn darwin_calls_are_put_in_running_order_with_origin_and_terminus() {
        let raw = raw_calling_points_from_darwin(&u06535());
        assert_eq!(
            tiplocs(&raw),
            [
                "YORK", "MILFDY", "RANSKLL", "DONC", "PBRO", "CNNGTNL", "HNTNGDN", "HATFILD",
                "POTRSBR", "KNGX"
            ]
        );
        assert_eq!(raw[0].kind, CallingPointKind::Origin);
        assert_eq!(raw[9].kind, CallingPointKind::Terminate);
        assert!(
            raw[1..9]
                .iter()
                .all(|cp| cp.kind == CallingPointKind::Intermediate)
        );
        assert!(raw.iter().all(|cp| cp.day_offset == 0));
    }

    #[test]
    fn darwin_public_working_and_pass_rows_keep_their_times() {
        let raw = raw_calling_points_from_darwin(&u06535());
        let york = &raw[0];
        assert_eq!(york.booked_departure, Some(t("16:45")));
        assert_eq!(york.timetable.public_departure, Some(t("16:45")));
        assert_eq!(york.timetable.can_board, Some(true));
        assert_eq!(york.platform.as_deref(), Some("5"));

        // A working stop: booked (working) times, no public time, not a
        // stop a passenger can use.
        let milford = &raw[1];
        assert_eq!(milford.booked_arrival, Some(t("17:10")));
        assert_eq!(milford.timetable.public_arrival, None);
        assert_eq!(milford.timetable.can_board, Some(false));
        assert_eq!(milford.timetable.can_alight, Some(false));

        // A pass: no booked time, so the stop list treats it as a pass.
        let doncaster = &raw[3];
        assert_eq!(doncaster.booked_arrival, None);
        assert_eq!(doncaster.booked_departure, None);
        assert_eq!(doncaster.timetable.working_pass, Some(t("18:40")));

        // A half-minute working arrival: booked to the minute, exact
        // working time kept, public time separate.
        let huntingdon = &raw[6];
        assert_eq!(huntingdon.booked_arrival, Some(t("20:12")));
        assert_eq!(huntingdon.timetable.working_arrival, Some(t("20:12:30")));
        assert_eq!(huntingdon.timetable.public_arrival, Some(t("20:13")));

        let kings_cross = &raw[9];
        assert_eq!(kings_cross.timetable.public_arrival, Some(t("21:50")));
        assert_eq!(kings_cross.timetable.can_alight, Some(true));
        assert_eq!(kings_cross.timetable.can_board, Some(false));
    }

    #[test]
    fn darwin_cancelled_and_untimed_calls_are_left_out() {
        let mut calls = u06535();
        calls
            .iter_mut()
            .find(|c| c.call.tiploc == "PBRO")
            .expect("PBRO")
            .call
            .is_cancelled = true;
        calls.push(call("NOTIME", ["", "", "", "", ""], None));
        let raw = raw_calling_points_from_darwin(&calls);
        assert!(!tiplocs(&raw).contains(&"PBRO"));
        assert!(!tiplocs(&raw).contains(&"NOTIME"));
        assert_eq!(raw.len(), 9);
    }

    #[test]
    fn darwin_service_past_midnight_is_ordered_and_dated_across_it() {
        let calls = vec![
            call("EDINBUR", ["", "23:40", "", "", "23:40"], None),
            call("NWCSTLE", ["01:10", "01:12", "", "01:10", "01:12"], None),
            call("DUNBAR", ["23:58", "00:03", "", "23:58", "00:03"], None),
            call("BERWICK", ["", "", "00:30", "", ""], None),
            call("YORK", ["02:30", "", "", "02:30", ""], None),
        ];
        let raw = raw_calling_points_from_darwin(&calls);
        assert_eq!(
            tiplocs(&raw),
            ["EDINBUR", "DUNBAR", "BERWICK", "NWCSTLE", "YORK"]
        );
        let offsets: Vec<u8> = raw.iter().map(|cp| cp.day_offset).collect();
        // DUNBAR arrives before midnight (its departure's day comes from
        // `departure_day_offset`), the rest after.
        assert_eq!(offsets, [0, 0, 1, 1, 1]);
        assert_eq!(raw[0].kind, CallingPointKind::Origin);
        assert_eq!(raw[4].kind, CallingPointKind::Terminate);

        // Built into stops: every time strictly after the one before.
        let date = NaiveDate::from_ymd_opt(2026, 11, 16).expect("date");
        let stops: Vec<_> = raw
            .iter()
            .map(|cp| crate::data::journey::JourneyStop::from_calling_point(cp, None, date))
            .collect();
        let times: Vec<DateTime<Utc>> = stops
            .iter()
            .flat_map(|s| {
                [
                    s.timetable.working_arrival,
                    s.timetable.working_pass,
                    s.timetable.working_departure,
                ]
            })
            .flatten()
            .collect();
        assert!(times.windows(2).all(|w| w[0] < w[1]), "{times:?}");
    }

    #[test]
    fn darwin_service_starting_after_midnight_stays_on_its_date() {
        let calls = vec![
            call("B", ["00:40", "", "", "00:40", ""], None),
            call("A", ["", "00:10", "", "", "00:10"], None),
        ];
        let raw = raw_calling_points_from_darwin(&calls);
        assert_eq!(tiplocs(&raw), ["A", "B"]);
        assert!(raw.iter().all(|cp| cp.day_offset == 0));
    }

    #[test]
    fn darwin_withdrawn_service() {
        let service = |is_deleted, cancelled: &[&str]| {
            let mut calls = u06535();
            for call in &mut calls {
                call.call.is_cancelled =
                    cancelled.contains(&"*") || cancelled.contains(&call.call.tiploc.as_str());
            }
            DarwinService { is_deleted, calls }
        };
        assert!(!service(false, &[]).is_withdrawn());
        assert!(
            !service(false, &["KNGX", "POTRSBR"]).is_withdrawn(),
            "a part cancellation (cut short) still runs"
        );
        assert!(
            service(false, &["*"]).is_withdrawn(),
            "every call cancelled"
        );
        assert!(service(true, &[]).is_withdrawn());
        assert!(
            !DarwinService {
                is_deleted: false,
                calls: Vec::new()
            }
            .is_withdrawn(),
            "no calls yet: not known to be withdrawn"
        );
    }

    fn trust(crs: &str, event_type: &str, planned: &str, gbtt: Option<&str>) -> TrustStopEvent {
        TrustStopEvent {
            loc_crs: crs.to_string(),
            tiploc: format!("T{crs}"),
            event_type: event_type.to_string(),
            planned_timestamp: planned.parse().expect("instant"),
            gbtt_timestamp: gbtt.map(|g| g.parse().expect("instant")),
        }
    }

    #[test]
    fn trust_reports_become_one_stop_per_visit() {
        let date = NaiveDate::from_ymd_opt(2026, 11, 16).expect("date");
        let events = vec![
            trust(
                "YRK",
                "DEPARTURE",
                "2026-11-16T16:45:00Z",
                Some("2026-11-16T16:45:00Z"),
            ),
            trust(
                "PBO",
                "ARRIVAL",
                "2026-11-16T19:33:00Z",
                Some("2026-11-16T19:33:00Z"),
            ),
            trust(
                "PBO",
                "DEPARTURE",
                "2026-11-16T19:41:00Z",
                Some("2026-11-16T19:41:00Z"),
            ),
            // A working stop: no public time.
            trust("XYZ", "ARRIVAL", "2026-11-16T19:50:00Z", None),
            // A corrected report replaces the first.
            trust(
                "HUN",
                "ARRIVAL",
                "2026-11-16T20:12:00Z",
                Some("2026-11-16T20:13:00Z"),
            ),
            trust(
                "HUN",
                "ARRIVAL",
                "2026-11-16T20:12:30Z",
                Some("2026-11-16T20:13:00Z"),
            ),
            // Back at PBO later: a second visit, departing after midnight.
            trust("PBO", "ARRIVAL", "2026-11-16T23:59:00Z", None),
            trust("PBO", "DEPARTURE", "2026-11-17T00:05:00Z", None),
        ];
        let raw = raw_calling_points_from_trust(&events, date);
        assert_eq!(tiplocs(&raw), ["TYRK", "TPBO", "TXYZ", "THUN", "TPBO"]);
        assert!(
            raw.iter()
                .all(|cp| cp.kind == CallingPointKind::Intermediate)
        );
        assert_eq!(raw[0].booked_departure, Some(t("16:45")));
        assert_eq!(raw[0].booked_arrival, None);
        assert_eq!(raw[1].timetable.public_arrival, Some(t("19:33")));
        assert_eq!(raw[1].booked_departure, Some(t("19:41")));
        assert_eq!(raw[1].timetable.can_board, None, "a public stop");
        assert_eq!(raw[2].timetable.can_board, Some(false), "a working stop");
        assert_eq!(raw[3].booked_arrival, Some(t("20:12")));
        assert_eq!(raw[3].timetable.working_arrival, Some(t("20:12:30")));
        assert_eq!(raw[4].day_offset, 0, "arrives before midnight");
        assert_eq!(raw[4].booked_departure, Some(t("00:05")));
    }

    #[test]
    fn trust_times_are_london_local() {
        let date = NaiveDate::from_ymd_opt(2026, 7, 1).expect("date");
        let raw = raw_calling_points_from_trust(
            &[trust("YRK", "DEPARTURE", "2026-07-01T15:45:00Z", None)],
            date,
        );
        assert_eq!(raw[0].booked_departure, Some(t("16:45")), "BST");
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::data::journey::{SourcedStops, build_journey_stops_sourced};
    use sqlx::postgres::PgPoolOptions;

    // Fixture UIDs carry a `Z96` prefix and dates in 2099 (`8Z96` in a
    // RID), so the cleanup never touches real rows.
    const UID: &str = "Z96535";
    const RID: &str = "209907118Z96535";

    fn ssd() -> NaiveDate {
        NaiveDate::from_ymd_opt(2099, 7, 11).expect("date")
    }

    const DARWIN: LiveStopFallbacks = LiveStopFallbacks {
        darwin: true,
        trust: false,
    };
    const BOTH: LiveStopFallbacks = LiveStopFallbacks {
        darwin: true,
        trust: true,
    };

    async fn connect() -> PgPool {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
        PgPoolOptions::new()
            .connect(&url)
            .await
            .expect("connect to postgres")
    }

    async fn cleanup(pool: &PgPool) {
        for sql in [
            "DELETE FROM darwin_calls WHERE rid LIKE '%8Z96%' AND ssd >= '2099-01-01'",
            "DELETE FROM darwin_services WHERE rid LIKE '%8Z96%' AND ssd >= '2099-01-01'",
            "DELETE FROM schedule_calling_points_full WHERE uid LIKE 'Z96%'",
            "DELETE FROM stanox_crs WHERE tiploc LIKE 'ZT%'",
            "DELETE FROM trains WHERE train_uid LIKE 'Z96%'",
        ] {
            sqlx::query(sql).execute(pool).await.expect("cleanup");
        }
    }

    /// The fixture's stations: (TIPLOC, CRS, STANOX).
    const STATIONS: &[(&str, &str, &str)] = &[
        ("ZTYORK", "YRK", "Z9001"),
        ("ZTPBRO", "PBO", "Z9002"),
        ("ZTHNTNG", "HUN", "Z9003"),
        ("ZTHATFD", "HAT", "Z9004"),
        ("ZTPOTRS", "PBR", "Z9005"),
        ("ZTKNGX", "KGX", "Z9006"),
    ];

    async fn seed_stations(pool: &PgPool) {
        let records: Vec<common::StanoxCrsRecord> = STATIONS
            .iter()
            .map(|(tiploc, crs, stanox)| common::StanoxCrsRecord {
                stanox: (*stanox).to_string(),
                crs: (*crs).to_string(),
                tiploc: (*tiploc).to_string(),
                station_name: (*crs).to_string(),
                source_sequence: 1,
                change_time_minutes: None,
            })
            .collect();
        crate::data::queries::upsert_stanox_crs(pool, &records)
            .await
            .expect("seed stanox_crs");
    }

    async fn seed_train(pool: &PgPool, uid: &str, train_id: Option<&str>) -> i64 {
        let id = crate::data::trains::find_or_create_train(pool, uid, ssd())
            .await
            .expect("find_or_create_train");
        sqlx::query("UPDATE trains SET train_id = $2 WHERE id = $1")
            .bind(id)
            .bind(train_id)
            .execute(pool)
            .await
            .expect("set train_id");
        id
    }

    /// One `darwin_calls` row of the fixture.
    struct Call {
        tiploc: &'static str,
        wta: &'static str,
        wtd: &'static str,
        wtp: &'static str,
        pta: Option<&'static str>,
        ptd: Option<&'static str>,
        planned: Option<&'static str>,
        platform: Option<&'static str>,
        cancelled: bool,
    }

    const fn stop(
        tiploc: &'static str,
        wta: &'static str,
        wtd: &'static str,
        public: (Option<&'static str>, Option<&'static str>),
        platforms: (Option<&'static str>, Option<&'static str>),
    ) -> Call {
        Call {
            tiploc,
            wta,
            wtd,
            wtp: "",
            pta: public.0,
            ptd: public.1,
            planned: platforms.0,
            platform: platforms.1,
            cancelled: false,
        }
    }

    /// Modelled on `U06535`'s rows: stored out of order, with public,
    /// working-only, pass and cancelled calls.
    fn fixture_calls() -> Vec<Call> {
        vec![
            stop(
                "ZTKNGX",
                "21:50",
                "",
                (Some("21:50"), None),
                (Some("8"), Some("8")),
            ),
            stop(
                "ZTPBRO",
                "19:33",
                "19:41",
                (Some("19:33"), Some("19:41")),
                (Some("2"), Some("2")),
            ),
            stop(
                "ZTYORK",
                "",
                "16:45",
                (None, Some("16:45")),
                (Some("3"), Some("5")),
            ),
            stop("ZTMILFD", "17:10", "17:18", (None, None), (None, None)),
            Call {
                wtp: "18:40",
                ..stop("ZTDONC", "", "", (None, None), (None, None))
            },
            stop(
                "ZTHNTNG",
                "20:12:30",
                "20:14",
                (Some("20:13"), Some("20:14")),
                (None, None),
            ),
            stop(
                "ZTHATFD",
                "21:14",
                "21:16",
                (Some("21:15"), Some("21:16")),
                (Some("1"), Some("1")),
            ),
            Call {
                cancelled: true,
                ..stop(
                    "ZTPOTRS",
                    "21:24",
                    "21:26",
                    (Some("21:25"), Some("21:26")),
                    (None, None),
                )
            },
        ]
    }

    async fn seed_darwin_service(pool: &PgPool, uid: &str, deleted: bool, cancel_reason: &str) {
        sqlx::query(
            "INSERT INTO darwin_services (rid, uid, ssd, toc, train_id, is_passenger, is_deleted, \
                                          cancel_reason, schedule_ts, message_ts) \
             VALUES ($1, $2, $3, 'LS', '1Z96', FALSE, $4, $5, now(), now())",
        )
        .bind(RID)
        .bind(uid)
        .bind(ssd())
        .bind(deleted)
        .bind(cancel_reason)
        .execute(pool)
        .await
        .expect("seed darwin_services");
        for call in fixture_calls() {
            sqlx::query(
                "INSERT INTO darwin_calls (rid, tiploc, wta, wtd, wtp, ssd, pta, ptd, \
                                           planned_platform, platform, is_cancelled) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
            )
            .bind(RID)
            .bind(call.tiploc)
            .bind(call.wta)
            .bind(call.wtd)
            .bind(call.wtp)
            .bind(ssd())
            .bind(call.pta)
            .bind(call.ptd)
            .bind(call.planned)
            .bind(call.platform)
            .bind(call.cancelled)
            .execute(pool)
            .await
            .expect("seed darwin_calls");
        }
    }

    async fn seed_trust_departure(pool: &PgPool, trains_id: i64) {
        sqlx::query(
            "INSERT INTO train_movement_events \
                (trains_id, dedup_key, msg_type, event_type, loc_stanox, loc_crs, \
                 planned_timestamp, actual_timestamp, gbtt_timestamp, variation_status, raw_body) \
             VALUES ($1, 'z96-dep', '0003', 'DEPARTURE', 'Z9001', 'YRK', \
                     '2099-07-11T15:45:00Z', '2099-07-11T15:47:00Z', '2099-07-11T15:45:00Z', \
                     'LATE', '{}'::jsonb)",
        )
        .bind(trains_id)
        .execute(pool)
        .await
        .expect("seed movement");
    }

    async fn build(
        pool: &PgPool,
        trains_id: i64,
        uid: &str,
        fallbacks: LiveStopFallbacks,
    ) -> Option<SourcedStops> {
        let request = JourneyStopsRequest {
            trains_id,
            train_uid: uid,
            service_date: ssd(),
            calling_points_json: None,
            current_delay_minutes: None,
            skipped_stations: &[],
            platform: None,
            planned_platform: None,
            predicted_late_minutes: None,
        };
        build_journey_stops_sourced(pool, &request, fallbacks)
            .await
            .expect("build stops")
    }

    fn stop_tiplocs(sourced: &SourcedStops) -> Vec<&str> {
        sourced
            .stops
            .iter()
            .filter_map(|s| s.tiploc.as_deref())
            .collect()
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                live_stops::db_tests -- --ignored --test-threads=1`"]
    async fn darwin_only_train_gets_ordered_stops_with_platforms_and_public_times() {
        let pool = connect().await;
        cleanup(&pool).await;
        seed_stations(&pool).await;
        let trains_id = seed_train(&pool, UID, Some("161Z961T11")).await;
        seed_darwin_service(&pool, UID, false, "").await;

        // Flag off: today's behaviour, no stops.
        assert!(
            build(&pool, trains_id, UID, LiveStopFallbacks::OFF)
                .await
                .is_none()
        );

        let sourced = build(&pool, trains_id, UID, DARWIN).await.expect("stops");
        assert_eq!(sourced.source, ScheduleSource::Darwin);
        assert_eq!(
            stop_tiplocs(&sourced),
            [
                "ZTYORK", "ZTMILFD", "ZTDONC", "ZTPBRO", "ZTHNTNG", "ZTHATFD", "ZTKNGX"
            ],
            "running order, the cancelled POTRSBR call left out"
        );
        let stops = &sourced.stops;
        assert_eq!(stops[0].crs.as_deref(), Some("YRK"));
        assert_eq!(stops[0].kind, Some(CallingPointKind::Origin));
        assert_eq!(stops[6].crs.as_deref(), Some("KGX"));
        assert_eq!(stops[6].kind, Some(CallingPointKind::Terminate));
        assert!(stops[2].timetable.passing_point, "DONC is a pass");
        assert!(stops[1].crs.is_none(), "a working stop at a junction");
        // Platforms: the booked one is Darwin's plan, the live one from the
        // Push Port overlay.
        assert_eq!(stops[0].booked_platform.as_deref(), Some("3"));
        assert_eq!(stops[0].platform.as_deref(), Some("5"));
        assert_eq!(stops[3].platform.as_deref(), Some("2"));
        // Public times: PBRO's arrival is 19:33 London, in BST (18:33 UTC).
        assert_eq!(
            stops[3].timetable.public_arrival,
            Some("2099-07-11T18:33:00Z".parse().expect("instant"))
        );
        assert_eq!(stops[1].timetable.public_arrival, None);

        cleanup(&pool).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                live_stops::db_tests -- --ignored --test-threads=1`"]
    async fn cif_wins_over_darwin() {
        let pool = connect().await;
        cleanup(&pool).await;
        seed_stations(&pool).await;
        let trains_id = seed_train(&pool, UID, None).await;
        seed_darwin_service(&pool, UID, false, "").await;
        crate::data::queries::upsert_schedule_calling_points_full(
            &pool,
            &[
                crate::data::queries::ScheduleCallingPointsFullRow {
                    service_date: ssd(),
                    uid: UID.to_string(),
                    seq: 0,
                    tiploc: "ZTYORK".to_string(),
                    kind: "origin".to_string(),
                    booked_departure: Some("16:45:00".parse().expect("time")),
                    ..Default::default()
                },
                crate::data::queries::ScheduleCallingPointsFullRow {
                    service_date: ssd(),
                    uid: UID.to_string(),
                    seq: 1,
                    tiploc: "ZTKNGX".to_string(),
                    kind: "terminate".to_string(),
                    booked_arrival: Some("21:50:00".parse().expect("time")),
                    ..Default::default()
                },
            ],
        )
        .await
        .expect("seed CIF");

        let sourced = build(&pool, trains_id, UID, BOTH).await.expect("stops");
        assert_eq!(sourced.source, ScheduleSource::Cif);
        assert_eq!(stop_tiplocs(&sourced), ["ZTYORK", "ZTKNGX"]);

        cleanup(&pool).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                live_stops::db_tests -- --ignored --test-threads=1`"]
    async fn deleted_or_cancelled_darwin_service_gives_no_stops_and_no_trust_fallback() {
        let pool = connect().await;
        for (deleted, all_cancelled) in [(true, false), (false, true)] {
            cleanup(&pool).await;
            seed_stations(&pool).await;
            let trains_id = seed_train(&pool, UID, None).await;
            seed_darwin_service(&pool, UID, deleted, "104").await;
            if all_cancelled {
                sqlx::query("UPDATE darwin_calls SET is_cancelled = TRUE WHERE rid = $1")
                    .bind(RID)
                    .execute(&pool)
                    .await
                    .expect("cancel every call");
            }
            seed_trust_departure(&pool, trains_id).await;
            assert!(
                build(&pool, trains_id, UID, BOTH).await.is_none(),
                "deleted {deleted}, every call cancelled {all_cancelled}"
            );
        }

        // A part cancellation: Darwin gives a schedule-level reason too,
        // but the train still runs its other calls.
        cleanup(&pool).await;
        seed_stations(&pool).await;
        let trains_id = seed_train(&pool, UID, None).await;
        seed_darwin_service(&pool, UID, false, "104").await;
        let sourced = build(&pool, trains_id, UID, BOTH).await.expect("stops");
        assert_eq!(sourced.source, ScheduleSource::Darwin);
        assert_eq!(
            sourced.stops.len(),
            7,
            "the cancelled POTRSBR call left out"
        );
        cleanup(&pool).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                live_stops::db_tests -- --ignored --test-threads=1`"]
    async fn trust_fallback_only_when_darwin_has_nothing() {
        let pool = connect().await;
        cleanup(&pool).await;
        seed_stations(&pool).await;
        let trains_id = seed_train(&pool, UID, None).await;
        seed_trust_departure(&pool, trains_id).await;

        // TRUST flag off: nothing.
        assert!(build(&pool, trains_id, UID, DARWIN).await.is_none());

        let sourced = build(&pool, trains_id, UID, BOTH).await.expect("stops");
        assert_eq!(sourced.source, ScheduleSource::Trust);
        assert_eq!(stop_tiplocs(&sourced), ["ZTYORK"]);
        let york = &sourced.stops[0];
        assert_eq!(york.crs.as_deref(), Some("YRK"));
        assert_eq!(
            york.actual_departure,
            Some("2099-07-11T15:47:00Z".parse().expect("instant")),
            "the movement overlay still applies"
        );

        // With Darwin's schedule present, Darwin wins.
        seed_darwin_service(&pool, UID, false, "").await;
        let sourced = build(&pool, trains_id, UID, BOTH).await.expect("stops");
        assert_eq!(sourced.source, ScheduleSource::Darwin);
        assert_eq!(sourced.stops.len(), 7);

        cleanup(&pool).await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                live_stops::db_tests -- --ignored --test-threads=1`"]
    async fn darwin_service_is_found_by_headcode_when_the_uid_differs() {
        let pool = connect().await;
        cleanup(&pool).await;
        seed_stations(&pool).await;
        // The trains row's UID is not Darwin's; its TRUST id's headcode is.
        let trains_id = seed_train(&pool, "Z96999", Some("161Z961T11")).await;
        seed_darwin_service(&pool, UID, false, "").await;

        let sourced = build(&pool, trains_id, "Z96999", DARWIN)
            .await
            .expect("stops");
        assert_eq!(sourced.source, ScheduleSource::Darwin);
        assert_eq!(sourced.stops.len(), 7);
        assert_eq!(
            sourced.stops[0].platform.as_deref(),
            Some("5"),
            "the overlay covers a headcode match"
        );

        // Another live service with the same headcode: ambiguous, nothing.
        sqlx::query(
            "INSERT INTO darwin_services (rid, uid, ssd, train_id, message_ts) \
             VALUES ('209907118Z96536', 'Z96536', $1, '1Z96', now())",
        )
        .bind(ssd())
        .execute(&pool)
        .await
        .expect("seed second service");
        assert!(
            build(&pool, trains_id, "Z96999", DARWIN).await.is_none(),
            "an ambiguous headcode is not guessed"
        );

        // The one Darwin-only service with the headcode has a trains row of
        // its own: another train, not this one.
        sqlx::query("DELETE FROM darwin_services WHERE rid = '209907118Z96536'")
            .execute(&pool)
            .await
            .expect("drop second service");
        seed_train(&pool, UID, None).await;
        assert!(
            build(&pool, trains_id, "Z96999", DARWIN).await.is_none(),
            "a service another trains row owns is not borrowed"
        );

        cleanup(&pool).await;
    }
}

/// Pins [`london_time`] around both clock changes of 2025 and 2026: the London
/// time of day and the day offset from the service date, clamped to 0..=2.
#[cfg(test)]
mod dst_pinning_tests {
    use super::*;

    /// `(instant, service date, London time, day offset)`.
    #[rustfmt::skip]
    const TIMES: &[(&str, &str, &str, u8)] = &[
        ("2025-03-29T22:00:00Z", "2025-03-29", "22:00:00", 0),
        ("2025-03-29T22:00:00Z", "2025-03-28", "22:00:00", 1),
        ("2025-03-29T22:00:00Z", "2025-03-27", "22:00:00", 2),
        ("2025-03-29T22:00:00Z", "2025-03-26", "22:00:00", 2),
        ("2025-03-29T22:00:00Z", "2025-03-30", "22:00:00", 0),
        ("2025-03-29T23:00:00Z", "2025-03-29", "23:00:00", 0),
        ("2025-03-29T23:00:00Z", "2025-03-28", "23:00:00", 1),
        ("2025-03-29T23:00:00Z", "2025-03-27", "23:00:00", 2),
        ("2025-03-29T23:00:00Z", "2025-03-26", "23:00:00", 2),
        ("2025-03-29T23:00:00Z", "2025-03-30", "23:00:00", 0),
        ("2025-03-29T23:30:00Z", "2025-03-29", "23:30:00", 0),
        ("2025-03-29T23:30:00Z", "2025-03-28", "23:30:00", 1),
        ("2025-03-29T23:30:00Z", "2025-03-27", "23:30:00", 2),
        ("2025-03-29T23:30:00Z", "2025-03-26", "23:30:00", 2),
        ("2025-03-29T23:30:00Z", "2025-03-30", "23:30:00", 0),
        ("2025-03-30T00:00:00Z", "2025-03-30", "00:00:00", 0),
        ("2025-03-30T00:00:00Z", "2025-03-29", "00:00:00", 1),
        ("2025-03-30T00:00:00Z", "2025-03-28", "00:00:00", 2),
        ("2025-03-30T00:00:00Z", "2025-03-27", "00:00:00", 2),
        ("2025-03-30T00:00:00Z", "2025-03-31", "00:00:00", 0),
        ("2025-03-30T00:30:00Z", "2025-03-30", "00:30:00", 0),
        ("2025-03-30T00:30:00Z", "2025-03-29", "00:30:00", 1),
        ("2025-03-30T00:30:00Z", "2025-03-28", "00:30:00", 2),
        ("2025-03-30T00:30:00Z", "2025-03-27", "00:30:00", 2),
        ("2025-03-30T00:30:00Z", "2025-03-31", "00:30:00", 0),
        ("2025-03-30T00:59:00Z", "2025-03-30", "00:59:00", 0),
        ("2025-03-30T00:59:00Z", "2025-03-29", "00:59:00", 1),
        ("2025-03-30T00:59:00Z", "2025-03-28", "00:59:00", 2),
        ("2025-03-30T00:59:00Z", "2025-03-27", "00:59:00", 2),
        ("2025-03-30T00:59:00Z", "2025-03-31", "00:59:00", 0),
        ("2025-03-30T01:00:00Z", "2025-03-30", "02:00:00", 0),
        ("2025-03-30T01:00:00Z", "2025-03-29", "02:00:00", 1),
        ("2025-03-30T01:00:00Z", "2025-03-28", "02:00:00", 2),
        ("2025-03-30T01:00:00Z", "2025-03-27", "02:00:00", 2),
        ("2025-03-30T01:00:00Z", "2025-03-31", "02:00:00", 0),
        ("2025-03-30T01:01:00Z", "2025-03-30", "02:01:00", 0),
        ("2025-03-30T01:01:00Z", "2025-03-29", "02:01:00", 1),
        ("2025-03-30T01:01:00Z", "2025-03-28", "02:01:00", 2),
        ("2025-03-30T01:01:00Z", "2025-03-27", "02:01:00", 2),
        ("2025-03-30T01:01:00Z", "2025-03-31", "02:01:00", 0),
        ("2025-03-30T01:30:00Z", "2025-03-30", "02:30:00", 0),
        ("2025-03-30T01:30:00Z", "2025-03-29", "02:30:00", 1),
        ("2025-03-30T01:30:00Z", "2025-03-28", "02:30:00", 2),
        ("2025-03-30T01:30:00Z", "2025-03-27", "02:30:00", 2),
        ("2025-03-30T01:30:00Z", "2025-03-31", "02:30:00", 0),
        ("2025-03-30T01:59:00Z", "2025-03-30", "02:59:00", 0),
        ("2025-03-30T01:59:00Z", "2025-03-29", "02:59:00", 1),
        ("2025-03-30T01:59:00Z", "2025-03-28", "02:59:00", 2),
        ("2025-03-30T01:59:00Z", "2025-03-27", "02:59:00", 2),
        ("2025-03-30T01:59:00Z", "2025-03-31", "02:59:00", 0),
        ("2025-03-30T02:00:00Z", "2025-03-30", "03:00:00", 0),
        ("2025-03-30T02:00:00Z", "2025-03-29", "03:00:00", 1),
        ("2025-03-30T02:00:00Z", "2025-03-28", "03:00:00", 2),
        ("2025-03-30T02:00:00Z", "2025-03-27", "03:00:00", 2),
        ("2025-03-30T02:00:00Z", "2025-03-31", "03:00:00", 0),
        ("2025-03-30T02:30:00Z", "2025-03-30", "03:30:00", 0),
        ("2025-03-30T02:30:00Z", "2025-03-29", "03:30:00", 1),
        ("2025-03-30T02:30:00Z", "2025-03-28", "03:30:00", 2),
        ("2025-03-30T02:30:00Z", "2025-03-27", "03:30:00", 2),
        ("2025-03-30T02:30:00Z", "2025-03-31", "03:30:00", 0),
        ("2025-03-30T03:00:00Z", "2025-03-30", "04:00:00", 0),
        ("2025-03-30T03:00:00Z", "2025-03-29", "04:00:00", 1),
        ("2025-03-30T03:00:00Z", "2025-03-28", "04:00:00", 2),
        ("2025-03-30T03:00:00Z", "2025-03-27", "04:00:00", 2),
        ("2025-03-30T03:00:00Z", "2025-03-31", "04:00:00", 0),
        ("2025-03-30T12:00:00Z", "2025-03-30", "13:00:00", 0),
        ("2025-03-30T12:00:00Z", "2025-03-29", "13:00:00", 1),
        ("2025-03-30T12:00:00Z", "2025-03-28", "13:00:00", 2),
        ("2025-03-30T12:00:00Z", "2025-03-27", "13:00:00", 2),
        ("2025-03-30T12:00:00Z", "2025-03-31", "13:00:00", 0),
        ("2025-03-30T22:59:00Z", "2025-03-30", "23:59:00", 0),
        ("2025-03-30T22:59:00Z", "2025-03-29", "23:59:00", 1),
        ("2025-03-30T22:59:00Z", "2025-03-28", "23:59:00", 2),
        ("2025-03-30T22:59:00Z", "2025-03-27", "23:59:00", 2),
        ("2025-03-30T22:59:00Z", "2025-03-31", "23:59:00", 0),
        ("2025-03-30T23:00:00Z", "2025-03-31", "00:00:00", 0),
        ("2025-03-30T23:00:00Z", "2025-03-30", "00:00:00", 1),
        ("2025-03-30T23:00:00Z", "2025-03-29", "00:00:00", 2),
        ("2025-03-30T23:00:00Z", "2025-03-28", "00:00:00", 2),
        ("2025-03-30T23:00:00Z", "2025-04-01", "00:00:00", 0),
        ("2025-03-30T23:30:00Z", "2025-03-31", "00:30:00", 0),
        ("2025-03-30T23:30:00Z", "2025-03-30", "00:30:00", 1),
        ("2025-03-30T23:30:00Z", "2025-03-29", "00:30:00", 2),
        ("2025-03-30T23:30:00Z", "2025-03-28", "00:30:00", 2),
        ("2025-03-30T23:30:00Z", "2025-04-01", "00:30:00", 0),
        ("2025-03-31T00:30:00Z", "2025-03-31", "01:30:00", 0),
        ("2025-03-31T00:30:00Z", "2025-03-30", "01:30:00", 1),
        ("2025-03-31T00:30:00Z", "2025-03-29", "01:30:00", 2),
        ("2025-03-31T00:30:00Z", "2025-03-28", "01:30:00", 2),
        ("2025-03-31T00:30:00Z", "2025-04-01", "01:30:00", 0),
        ("2025-10-25T22:00:00Z", "2025-10-25", "23:00:00", 0),
        ("2025-10-25T22:00:00Z", "2025-10-24", "23:00:00", 1),
        ("2025-10-25T22:00:00Z", "2025-10-23", "23:00:00", 2),
        ("2025-10-25T22:00:00Z", "2025-10-22", "23:00:00", 2),
        ("2025-10-25T22:00:00Z", "2025-10-26", "23:00:00", 0),
        ("2025-10-25T23:00:00Z", "2025-10-26", "00:00:00", 0),
        ("2025-10-25T23:00:00Z", "2025-10-25", "00:00:00", 1),
        ("2025-10-25T23:00:00Z", "2025-10-24", "00:00:00", 2),
        ("2025-10-25T23:00:00Z", "2025-10-23", "00:00:00", 2),
        ("2025-10-25T23:00:00Z", "2025-10-27", "00:00:00", 0),
        ("2025-10-25T23:30:00Z", "2025-10-26", "00:30:00", 0),
        ("2025-10-25T23:30:00Z", "2025-10-25", "00:30:00", 1),
        ("2025-10-25T23:30:00Z", "2025-10-24", "00:30:00", 2),
        ("2025-10-25T23:30:00Z", "2025-10-23", "00:30:00", 2),
        ("2025-10-25T23:30:00Z", "2025-10-27", "00:30:00", 0),
        ("2025-10-26T00:00:00Z", "2025-10-26", "01:00:00", 0),
        ("2025-10-26T00:00:00Z", "2025-10-25", "01:00:00", 1),
        ("2025-10-26T00:00:00Z", "2025-10-24", "01:00:00", 2),
        ("2025-10-26T00:00:00Z", "2025-10-23", "01:00:00", 2),
        ("2025-10-26T00:00:00Z", "2025-10-27", "01:00:00", 0),
        ("2025-10-26T00:30:00Z", "2025-10-26", "01:30:00", 0),
        ("2025-10-26T00:30:00Z", "2025-10-25", "01:30:00", 1),
        ("2025-10-26T00:30:00Z", "2025-10-24", "01:30:00", 2),
        ("2025-10-26T00:30:00Z", "2025-10-23", "01:30:00", 2),
        ("2025-10-26T00:30:00Z", "2025-10-27", "01:30:00", 0),
        ("2025-10-26T00:59:00Z", "2025-10-26", "01:59:00", 0),
        ("2025-10-26T00:59:00Z", "2025-10-25", "01:59:00", 1),
        ("2025-10-26T00:59:00Z", "2025-10-24", "01:59:00", 2),
        ("2025-10-26T00:59:00Z", "2025-10-23", "01:59:00", 2),
        ("2025-10-26T00:59:00Z", "2025-10-27", "01:59:00", 0),
        ("2025-10-26T01:00:00Z", "2025-10-26", "01:00:00", 0),
        ("2025-10-26T01:00:00Z", "2025-10-25", "01:00:00", 1),
        ("2025-10-26T01:00:00Z", "2025-10-24", "01:00:00", 2),
        ("2025-10-26T01:00:00Z", "2025-10-23", "01:00:00", 2),
        ("2025-10-26T01:00:00Z", "2025-10-27", "01:00:00", 0),
        ("2025-10-26T01:01:00Z", "2025-10-26", "01:01:00", 0),
        ("2025-10-26T01:01:00Z", "2025-10-25", "01:01:00", 1),
        ("2025-10-26T01:01:00Z", "2025-10-24", "01:01:00", 2),
        ("2025-10-26T01:01:00Z", "2025-10-23", "01:01:00", 2),
        ("2025-10-26T01:01:00Z", "2025-10-27", "01:01:00", 0),
        ("2025-10-26T01:30:00Z", "2025-10-26", "01:30:00", 0),
        ("2025-10-26T01:30:00Z", "2025-10-25", "01:30:00", 1),
        ("2025-10-26T01:30:00Z", "2025-10-24", "01:30:00", 2),
        ("2025-10-26T01:30:00Z", "2025-10-23", "01:30:00", 2),
        ("2025-10-26T01:30:00Z", "2025-10-27", "01:30:00", 0),
        ("2025-10-26T01:59:00Z", "2025-10-26", "01:59:00", 0),
        ("2025-10-26T01:59:00Z", "2025-10-25", "01:59:00", 1),
        ("2025-10-26T01:59:00Z", "2025-10-24", "01:59:00", 2),
        ("2025-10-26T01:59:00Z", "2025-10-23", "01:59:00", 2),
        ("2025-10-26T01:59:00Z", "2025-10-27", "01:59:00", 0),
        ("2025-10-26T02:00:00Z", "2025-10-26", "02:00:00", 0),
        ("2025-10-26T02:00:00Z", "2025-10-25", "02:00:00", 1),
        ("2025-10-26T02:00:00Z", "2025-10-24", "02:00:00", 2),
        ("2025-10-26T02:00:00Z", "2025-10-23", "02:00:00", 2),
        ("2025-10-26T02:00:00Z", "2025-10-27", "02:00:00", 0),
        ("2025-10-26T02:30:00Z", "2025-10-26", "02:30:00", 0),
        ("2025-10-26T02:30:00Z", "2025-10-25", "02:30:00", 1),
        ("2025-10-26T02:30:00Z", "2025-10-24", "02:30:00", 2),
        ("2025-10-26T02:30:00Z", "2025-10-23", "02:30:00", 2),
        ("2025-10-26T02:30:00Z", "2025-10-27", "02:30:00", 0),
        ("2025-10-26T03:00:00Z", "2025-10-26", "03:00:00", 0),
        ("2025-10-26T03:00:00Z", "2025-10-25", "03:00:00", 1),
        ("2025-10-26T03:00:00Z", "2025-10-24", "03:00:00", 2),
        ("2025-10-26T03:00:00Z", "2025-10-23", "03:00:00", 2),
        ("2025-10-26T03:00:00Z", "2025-10-27", "03:00:00", 0),
        ("2025-10-26T12:00:00Z", "2025-10-26", "12:00:00", 0),
        ("2025-10-26T12:00:00Z", "2025-10-25", "12:00:00", 1),
        ("2025-10-26T12:00:00Z", "2025-10-24", "12:00:00", 2),
        ("2025-10-26T12:00:00Z", "2025-10-23", "12:00:00", 2),
        ("2025-10-26T12:00:00Z", "2025-10-27", "12:00:00", 0),
        ("2025-10-26T22:59:00Z", "2025-10-26", "22:59:00", 0),
        ("2025-10-26T22:59:00Z", "2025-10-25", "22:59:00", 1),
        ("2025-10-26T22:59:00Z", "2025-10-24", "22:59:00", 2),
        ("2025-10-26T22:59:00Z", "2025-10-23", "22:59:00", 2),
        ("2025-10-26T22:59:00Z", "2025-10-27", "22:59:00", 0),
        ("2025-10-26T23:00:00Z", "2025-10-26", "23:00:00", 0),
        ("2025-10-26T23:00:00Z", "2025-10-25", "23:00:00", 1),
        ("2025-10-26T23:00:00Z", "2025-10-24", "23:00:00", 2),
        ("2025-10-26T23:00:00Z", "2025-10-23", "23:00:00", 2),
        ("2025-10-26T23:00:00Z", "2025-10-27", "23:00:00", 0),
        ("2025-10-26T23:30:00Z", "2025-10-26", "23:30:00", 0),
        ("2025-10-26T23:30:00Z", "2025-10-25", "23:30:00", 1),
        ("2025-10-26T23:30:00Z", "2025-10-24", "23:30:00", 2),
        ("2025-10-26T23:30:00Z", "2025-10-23", "23:30:00", 2),
        ("2025-10-26T23:30:00Z", "2025-10-27", "23:30:00", 0),
        ("2025-10-27T00:30:00Z", "2025-10-27", "00:30:00", 0),
        ("2025-10-27T00:30:00Z", "2025-10-26", "00:30:00", 1),
        ("2025-10-27T00:30:00Z", "2025-10-25", "00:30:00", 2),
        ("2025-10-27T00:30:00Z", "2025-10-24", "00:30:00", 2),
        ("2025-10-27T00:30:00Z", "2025-10-28", "00:30:00", 0),
        ("2025-12-31T22:00:00Z", "2025-12-31", "22:00:00", 0),
        ("2025-12-31T22:00:00Z", "2025-12-30", "22:00:00", 1),
        ("2025-12-31T22:00:00Z", "2025-12-29", "22:00:00", 2),
        ("2025-12-31T22:00:00Z", "2025-12-28", "22:00:00", 2),
        ("2025-12-31T22:00:00Z", "2026-01-01", "22:00:00", 0),
        ("2025-12-31T22:59:00Z", "2025-12-31", "22:59:00", 0),
        ("2025-12-31T22:59:00Z", "2025-12-30", "22:59:00", 1),
        ("2025-12-31T22:59:00Z", "2025-12-29", "22:59:00", 2),
        ("2025-12-31T22:59:00Z", "2025-12-28", "22:59:00", 2),
        ("2025-12-31T22:59:00Z", "2026-01-01", "22:59:00", 0),
        ("2025-12-31T23:00:00Z", "2025-12-31", "23:00:00", 0),
        ("2025-12-31T23:00:00Z", "2025-12-30", "23:00:00", 1),
        ("2025-12-31T23:00:00Z", "2025-12-29", "23:00:00", 2),
        ("2025-12-31T23:00:00Z", "2025-12-28", "23:00:00", 2),
        ("2025-12-31T23:00:00Z", "2026-01-01", "23:00:00", 0),
        ("2025-12-31T23:30:00Z", "2025-12-31", "23:30:00", 0),
        ("2025-12-31T23:30:00Z", "2025-12-30", "23:30:00", 1),
        ("2025-12-31T23:30:00Z", "2025-12-29", "23:30:00", 2),
        ("2025-12-31T23:30:00Z", "2025-12-28", "23:30:00", 2),
        ("2025-12-31T23:30:00Z", "2026-01-01", "23:30:00", 0),
        ("2025-12-31T23:59:00Z", "2025-12-31", "23:59:00", 0),
        ("2025-12-31T23:59:00Z", "2025-12-30", "23:59:00", 1),
        ("2025-12-31T23:59:00Z", "2025-12-29", "23:59:00", 2),
        ("2025-12-31T23:59:00Z", "2025-12-28", "23:59:00", 2),
        ("2025-12-31T23:59:00Z", "2026-01-01", "23:59:00", 0),
        ("2026-01-01T00:00:00Z", "2026-01-01", "00:00:00", 0),
        ("2026-01-01T00:00:00Z", "2025-12-31", "00:00:00", 1),
        ("2026-01-01T00:00:00Z", "2025-12-30", "00:00:00", 2),
        ("2026-01-01T00:00:00Z", "2025-12-29", "00:00:00", 2),
        ("2026-01-01T00:00:00Z", "2026-01-02", "00:00:00", 0),
        ("2026-01-01T00:30:00Z", "2026-01-01", "00:30:00", 0),
        ("2026-01-01T00:30:00Z", "2025-12-31", "00:30:00", 1),
        ("2026-01-01T00:30:00Z", "2025-12-30", "00:30:00", 2),
        ("2026-01-01T00:30:00Z", "2025-12-29", "00:30:00", 2),
        ("2026-01-01T00:30:00Z", "2026-01-02", "00:30:00", 0),
        ("2026-01-01T00:59:00Z", "2026-01-01", "00:59:00", 0),
        ("2026-01-01T00:59:00Z", "2025-12-31", "00:59:00", 1),
        ("2026-01-01T00:59:00Z", "2025-12-30", "00:59:00", 2),
        ("2026-01-01T00:59:00Z", "2025-12-29", "00:59:00", 2),
        ("2026-01-01T00:59:00Z", "2026-01-02", "00:59:00", 0),
        ("2026-01-01T01:00:00Z", "2026-01-01", "01:00:00", 0),
        ("2026-01-01T01:00:00Z", "2025-12-31", "01:00:00", 1),
        ("2026-01-01T01:00:00Z", "2025-12-30", "01:00:00", 2),
        ("2026-01-01T01:00:00Z", "2025-12-29", "01:00:00", 2),
        ("2026-01-01T01:00:00Z", "2026-01-02", "01:00:00", 0),
        ("2026-01-01T01:30:00Z", "2026-01-01", "01:30:00", 0),
        ("2026-01-01T01:30:00Z", "2025-12-31", "01:30:00", 1),
        ("2026-01-01T01:30:00Z", "2025-12-30", "01:30:00", 2),
        ("2026-01-01T01:30:00Z", "2025-12-29", "01:30:00", 2),
        ("2026-01-01T01:30:00Z", "2026-01-02", "01:30:00", 0),
        ("2026-01-01T01:59:00Z", "2026-01-01", "01:59:00", 0),
        ("2026-01-01T01:59:00Z", "2025-12-31", "01:59:00", 1),
        ("2026-01-01T01:59:00Z", "2025-12-30", "01:59:00", 2),
        ("2026-01-01T01:59:00Z", "2025-12-29", "01:59:00", 2),
        ("2026-01-01T01:59:00Z", "2026-01-02", "01:59:00", 0),
        ("2026-01-01T02:00:00Z", "2026-01-01", "02:00:00", 0),
        ("2026-01-01T02:00:00Z", "2025-12-31", "02:00:00", 1),
        ("2026-01-01T02:00:00Z", "2025-12-30", "02:00:00", 2),
        ("2026-01-01T02:00:00Z", "2025-12-29", "02:00:00", 2),
        ("2026-01-01T02:00:00Z", "2026-01-02", "02:00:00", 0),
        ("2026-01-01T02:30:00Z", "2026-01-01", "02:30:00", 0),
        ("2026-01-01T02:30:00Z", "2025-12-31", "02:30:00", 1),
        ("2026-01-01T02:30:00Z", "2025-12-30", "02:30:00", 2),
        ("2026-01-01T02:30:00Z", "2025-12-29", "02:30:00", 2),
        ("2026-01-01T02:30:00Z", "2026-01-02", "02:30:00", 0),
        ("2026-01-15T22:00:00Z", "2026-01-15", "22:00:00", 0),
        ("2026-01-15T22:00:00Z", "2026-01-14", "22:00:00", 1),
        ("2026-01-15T22:00:00Z", "2026-01-13", "22:00:00", 2),
        ("2026-01-15T22:00:00Z", "2026-01-12", "22:00:00", 2),
        ("2026-01-15T22:00:00Z", "2026-01-16", "22:00:00", 0),
        ("2026-01-15T22:59:00Z", "2026-01-15", "22:59:00", 0),
        ("2026-01-15T22:59:00Z", "2026-01-14", "22:59:00", 1),
        ("2026-01-15T22:59:00Z", "2026-01-13", "22:59:00", 2),
        ("2026-01-15T22:59:00Z", "2026-01-12", "22:59:00", 2),
        ("2026-01-15T22:59:00Z", "2026-01-16", "22:59:00", 0),
        ("2026-01-15T23:00:00Z", "2026-01-15", "23:00:00", 0),
        ("2026-01-15T23:00:00Z", "2026-01-14", "23:00:00", 1),
        ("2026-01-15T23:00:00Z", "2026-01-13", "23:00:00", 2),
        ("2026-01-15T23:00:00Z", "2026-01-12", "23:00:00", 2),
        ("2026-01-15T23:00:00Z", "2026-01-16", "23:00:00", 0),
        ("2026-01-15T23:30:00Z", "2026-01-15", "23:30:00", 0),
        ("2026-01-15T23:30:00Z", "2026-01-14", "23:30:00", 1),
        ("2026-01-15T23:30:00Z", "2026-01-13", "23:30:00", 2),
        ("2026-01-15T23:30:00Z", "2026-01-12", "23:30:00", 2),
        ("2026-01-15T23:30:00Z", "2026-01-16", "23:30:00", 0),
        ("2026-01-15T23:59:00Z", "2026-01-15", "23:59:00", 0),
        ("2026-01-15T23:59:00Z", "2026-01-14", "23:59:00", 1),
        ("2026-01-15T23:59:00Z", "2026-01-13", "23:59:00", 2),
        ("2026-01-15T23:59:00Z", "2026-01-12", "23:59:00", 2),
        ("2026-01-15T23:59:00Z", "2026-01-16", "23:59:00", 0),
        ("2026-01-16T00:00:00Z", "2026-01-16", "00:00:00", 0),
        ("2026-01-16T00:00:00Z", "2026-01-15", "00:00:00", 1),
        ("2026-01-16T00:00:00Z", "2026-01-14", "00:00:00", 2),
        ("2026-01-16T00:00:00Z", "2026-01-13", "00:00:00", 2),
        ("2026-01-16T00:00:00Z", "2026-01-17", "00:00:00", 0),
        ("2026-01-16T00:30:00Z", "2026-01-16", "00:30:00", 0),
        ("2026-01-16T00:30:00Z", "2026-01-15", "00:30:00", 1),
        ("2026-01-16T00:30:00Z", "2026-01-14", "00:30:00", 2),
        ("2026-01-16T00:30:00Z", "2026-01-13", "00:30:00", 2),
        ("2026-01-16T00:30:00Z", "2026-01-17", "00:30:00", 0),
        ("2026-01-16T00:59:00Z", "2026-01-16", "00:59:00", 0),
        ("2026-01-16T00:59:00Z", "2026-01-15", "00:59:00", 1),
        ("2026-01-16T00:59:00Z", "2026-01-14", "00:59:00", 2),
        ("2026-01-16T00:59:00Z", "2026-01-13", "00:59:00", 2),
        ("2026-01-16T00:59:00Z", "2026-01-17", "00:59:00", 0),
        ("2026-01-16T01:00:00Z", "2026-01-16", "01:00:00", 0),
        ("2026-01-16T01:00:00Z", "2026-01-15", "01:00:00", 1),
        ("2026-01-16T01:00:00Z", "2026-01-14", "01:00:00", 2),
        ("2026-01-16T01:00:00Z", "2026-01-13", "01:00:00", 2),
        ("2026-01-16T01:00:00Z", "2026-01-17", "01:00:00", 0),
        ("2026-01-16T01:30:00Z", "2026-01-16", "01:30:00", 0),
        ("2026-01-16T01:30:00Z", "2026-01-15", "01:30:00", 1),
        ("2026-01-16T01:30:00Z", "2026-01-14", "01:30:00", 2),
        ("2026-01-16T01:30:00Z", "2026-01-13", "01:30:00", 2),
        ("2026-01-16T01:30:00Z", "2026-01-17", "01:30:00", 0),
        ("2026-01-16T01:59:00Z", "2026-01-16", "01:59:00", 0),
        ("2026-01-16T01:59:00Z", "2026-01-15", "01:59:00", 1),
        ("2026-01-16T01:59:00Z", "2026-01-14", "01:59:00", 2),
        ("2026-01-16T01:59:00Z", "2026-01-13", "01:59:00", 2),
        ("2026-01-16T01:59:00Z", "2026-01-17", "01:59:00", 0),
        ("2026-01-16T02:00:00Z", "2026-01-16", "02:00:00", 0),
        ("2026-01-16T02:00:00Z", "2026-01-15", "02:00:00", 1),
        ("2026-01-16T02:00:00Z", "2026-01-14", "02:00:00", 2),
        ("2026-01-16T02:00:00Z", "2026-01-13", "02:00:00", 2),
        ("2026-01-16T02:00:00Z", "2026-01-17", "02:00:00", 0),
        ("2026-01-16T02:30:00Z", "2026-01-16", "02:30:00", 0),
        ("2026-01-16T02:30:00Z", "2026-01-15", "02:30:00", 1),
        ("2026-01-16T02:30:00Z", "2026-01-14", "02:30:00", 2),
        ("2026-01-16T02:30:00Z", "2026-01-13", "02:30:00", 2),
        ("2026-01-16T02:30:00Z", "2026-01-17", "02:30:00", 0),
        ("2026-03-28T22:00:00Z", "2026-03-28", "22:00:00", 0),
        ("2026-03-28T22:00:00Z", "2026-03-27", "22:00:00", 1),
        ("2026-03-28T22:00:00Z", "2026-03-26", "22:00:00", 2),
        ("2026-03-28T22:00:00Z", "2026-03-25", "22:00:00", 2),
        ("2026-03-28T22:00:00Z", "2026-03-29", "22:00:00", 0),
        ("2026-03-28T23:00:00Z", "2026-03-28", "23:00:00", 0),
        ("2026-03-28T23:00:00Z", "2026-03-27", "23:00:00", 1),
        ("2026-03-28T23:00:00Z", "2026-03-26", "23:00:00", 2),
        ("2026-03-28T23:00:00Z", "2026-03-25", "23:00:00", 2),
        ("2026-03-28T23:00:00Z", "2026-03-29", "23:00:00", 0),
        ("2026-03-28T23:30:00Z", "2026-03-28", "23:30:00", 0),
        ("2026-03-28T23:30:00Z", "2026-03-27", "23:30:00", 1),
        ("2026-03-28T23:30:00Z", "2026-03-26", "23:30:00", 2),
        ("2026-03-28T23:30:00Z", "2026-03-25", "23:30:00", 2),
        ("2026-03-28T23:30:00Z", "2026-03-29", "23:30:00", 0),
        ("2026-03-29T00:00:00Z", "2026-03-29", "00:00:00", 0),
        ("2026-03-29T00:00:00Z", "2026-03-28", "00:00:00", 1),
        ("2026-03-29T00:00:00Z", "2026-03-27", "00:00:00", 2),
        ("2026-03-29T00:00:00Z", "2026-03-26", "00:00:00", 2),
        ("2026-03-29T00:00:00Z", "2026-03-30", "00:00:00", 0),
        ("2026-03-29T00:30:00Z", "2026-03-29", "00:30:00", 0),
        ("2026-03-29T00:30:00Z", "2026-03-28", "00:30:00", 1),
        ("2026-03-29T00:30:00Z", "2026-03-27", "00:30:00", 2),
        ("2026-03-29T00:30:00Z", "2026-03-26", "00:30:00", 2),
        ("2026-03-29T00:30:00Z", "2026-03-30", "00:30:00", 0),
        ("2026-03-29T00:59:00Z", "2026-03-29", "00:59:00", 0),
        ("2026-03-29T00:59:00Z", "2026-03-28", "00:59:00", 1),
        ("2026-03-29T00:59:00Z", "2026-03-27", "00:59:00", 2),
        ("2026-03-29T00:59:00Z", "2026-03-26", "00:59:00", 2),
        ("2026-03-29T00:59:00Z", "2026-03-30", "00:59:00", 0),
        ("2026-03-29T01:00:00Z", "2026-03-29", "02:00:00", 0),
        ("2026-03-29T01:00:00Z", "2026-03-28", "02:00:00", 1),
        ("2026-03-29T01:00:00Z", "2026-03-27", "02:00:00", 2),
        ("2026-03-29T01:00:00Z", "2026-03-26", "02:00:00", 2),
        ("2026-03-29T01:00:00Z", "2026-03-30", "02:00:00", 0),
        ("2026-03-29T01:01:00Z", "2026-03-29", "02:01:00", 0),
        ("2026-03-29T01:01:00Z", "2026-03-28", "02:01:00", 1),
        ("2026-03-29T01:01:00Z", "2026-03-27", "02:01:00", 2),
        ("2026-03-29T01:01:00Z", "2026-03-26", "02:01:00", 2),
        ("2026-03-29T01:01:00Z", "2026-03-30", "02:01:00", 0),
        ("2026-03-29T01:30:00Z", "2026-03-29", "02:30:00", 0),
        ("2026-03-29T01:30:00Z", "2026-03-28", "02:30:00", 1),
        ("2026-03-29T01:30:00Z", "2026-03-27", "02:30:00", 2),
        ("2026-03-29T01:30:00Z", "2026-03-26", "02:30:00", 2),
        ("2026-03-29T01:30:00Z", "2026-03-30", "02:30:00", 0),
        ("2026-03-29T01:59:00Z", "2026-03-29", "02:59:00", 0),
        ("2026-03-29T01:59:00Z", "2026-03-28", "02:59:00", 1),
        ("2026-03-29T01:59:00Z", "2026-03-27", "02:59:00", 2),
        ("2026-03-29T01:59:00Z", "2026-03-26", "02:59:00", 2),
        ("2026-03-29T01:59:00Z", "2026-03-30", "02:59:00", 0),
        ("2026-03-29T02:00:00Z", "2026-03-29", "03:00:00", 0),
        ("2026-03-29T02:00:00Z", "2026-03-28", "03:00:00", 1),
        ("2026-03-29T02:00:00Z", "2026-03-27", "03:00:00", 2),
        ("2026-03-29T02:00:00Z", "2026-03-26", "03:00:00", 2),
        ("2026-03-29T02:00:00Z", "2026-03-30", "03:00:00", 0),
        ("2026-03-29T02:30:00Z", "2026-03-29", "03:30:00", 0),
        ("2026-03-29T02:30:00Z", "2026-03-28", "03:30:00", 1),
        ("2026-03-29T02:30:00Z", "2026-03-27", "03:30:00", 2),
        ("2026-03-29T02:30:00Z", "2026-03-26", "03:30:00", 2),
        ("2026-03-29T02:30:00Z", "2026-03-30", "03:30:00", 0),
        ("2026-03-29T03:00:00Z", "2026-03-29", "04:00:00", 0),
        ("2026-03-29T03:00:00Z", "2026-03-28", "04:00:00", 1),
        ("2026-03-29T03:00:00Z", "2026-03-27", "04:00:00", 2),
        ("2026-03-29T03:00:00Z", "2026-03-26", "04:00:00", 2),
        ("2026-03-29T03:00:00Z", "2026-03-30", "04:00:00", 0),
        ("2026-03-29T12:00:00Z", "2026-03-29", "13:00:00", 0),
        ("2026-03-29T12:00:00Z", "2026-03-28", "13:00:00", 1),
        ("2026-03-29T12:00:00Z", "2026-03-27", "13:00:00", 2),
        ("2026-03-29T12:00:00Z", "2026-03-26", "13:00:00", 2),
        ("2026-03-29T12:00:00Z", "2026-03-30", "13:00:00", 0),
        ("2026-03-29T22:59:00Z", "2026-03-29", "23:59:00", 0),
        ("2026-03-29T22:59:00Z", "2026-03-28", "23:59:00", 1),
        ("2026-03-29T22:59:00Z", "2026-03-27", "23:59:00", 2),
        ("2026-03-29T22:59:00Z", "2026-03-26", "23:59:00", 2),
        ("2026-03-29T22:59:00Z", "2026-03-30", "23:59:00", 0),
        ("2026-03-29T23:00:00Z", "2026-03-30", "00:00:00", 0),
        ("2026-03-29T23:00:00Z", "2026-03-29", "00:00:00", 1),
        ("2026-03-29T23:00:00Z", "2026-03-28", "00:00:00", 2),
        ("2026-03-29T23:00:00Z", "2026-03-27", "00:00:00", 2),
        ("2026-03-29T23:00:00Z", "2026-03-31", "00:00:00", 0),
        ("2026-03-29T23:30:00Z", "2026-03-30", "00:30:00", 0),
        ("2026-03-29T23:30:00Z", "2026-03-29", "00:30:00", 1),
        ("2026-03-29T23:30:00Z", "2026-03-28", "00:30:00", 2),
        ("2026-03-29T23:30:00Z", "2026-03-27", "00:30:00", 2),
        ("2026-03-29T23:30:00Z", "2026-03-31", "00:30:00", 0),
        ("2026-03-30T00:30:00Z", "2026-03-30", "01:30:00", 0),
        ("2026-03-30T00:30:00Z", "2026-03-29", "01:30:00", 1),
        ("2026-03-30T00:30:00Z", "2026-03-28", "01:30:00", 2),
        ("2026-03-30T00:30:00Z", "2026-03-27", "01:30:00", 2),
        ("2026-03-30T00:30:00Z", "2026-03-31", "01:30:00", 0),
        ("2026-06-30T22:00:00Z", "2026-06-30", "23:00:00", 0),
        ("2026-06-30T22:00:00Z", "2026-06-29", "23:00:00", 1),
        ("2026-06-30T22:00:00Z", "2026-06-28", "23:00:00", 2),
        ("2026-06-30T22:00:00Z", "2026-06-27", "23:00:00", 2),
        ("2026-06-30T22:00:00Z", "2026-07-01", "23:00:00", 0),
        ("2026-06-30T22:59:00Z", "2026-06-30", "23:59:00", 0),
        ("2026-06-30T22:59:00Z", "2026-06-29", "23:59:00", 1),
        ("2026-06-30T22:59:00Z", "2026-06-28", "23:59:00", 2),
        ("2026-06-30T22:59:00Z", "2026-06-27", "23:59:00", 2),
        ("2026-06-30T22:59:00Z", "2026-07-01", "23:59:00", 0),
        ("2026-06-30T23:00:00Z", "2026-07-01", "00:00:00", 0),
        ("2026-06-30T23:00:00Z", "2026-06-30", "00:00:00", 1),
        ("2026-06-30T23:00:00Z", "2026-06-29", "00:00:00", 2),
        ("2026-06-30T23:00:00Z", "2026-06-28", "00:00:00", 2),
        ("2026-06-30T23:00:00Z", "2026-07-02", "00:00:00", 0),
        ("2026-06-30T23:30:00Z", "2026-07-01", "00:30:00", 0),
        ("2026-06-30T23:30:00Z", "2026-06-30", "00:30:00", 1),
        ("2026-06-30T23:30:00Z", "2026-06-29", "00:30:00", 2),
        ("2026-06-30T23:30:00Z", "2026-06-28", "00:30:00", 2),
        ("2026-06-30T23:30:00Z", "2026-07-02", "00:30:00", 0),
        ("2026-06-30T23:59:00Z", "2026-07-01", "00:59:00", 0),
        ("2026-06-30T23:59:00Z", "2026-06-30", "00:59:00", 1),
        ("2026-06-30T23:59:00Z", "2026-06-29", "00:59:00", 2),
        ("2026-06-30T23:59:00Z", "2026-06-28", "00:59:00", 2),
        ("2026-06-30T23:59:00Z", "2026-07-02", "00:59:00", 0),
        ("2026-07-01T00:00:00Z", "2026-07-01", "01:00:00", 0),
        ("2026-07-01T00:00:00Z", "2026-06-30", "01:00:00", 1),
        ("2026-07-01T00:00:00Z", "2026-06-29", "01:00:00", 2),
        ("2026-07-01T00:00:00Z", "2026-06-28", "01:00:00", 2),
        ("2026-07-01T00:00:00Z", "2026-07-02", "01:00:00", 0),
        ("2026-07-01T00:30:00Z", "2026-07-01", "01:30:00", 0),
        ("2026-07-01T00:30:00Z", "2026-06-30", "01:30:00", 1),
        ("2026-07-01T00:30:00Z", "2026-06-29", "01:30:00", 2),
        ("2026-07-01T00:30:00Z", "2026-06-28", "01:30:00", 2),
        ("2026-07-01T00:30:00Z", "2026-07-02", "01:30:00", 0),
        ("2026-07-01T00:59:00Z", "2026-07-01", "01:59:00", 0),
        ("2026-07-01T00:59:00Z", "2026-06-30", "01:59:00", 1),
        ("2026-07-01T00:59:00Z", "2026-06-29", "01:59:00", 2),
        ("2026-07-01T00:59:00Z", "2026-06-28", "01:59:00", 2),
        ("2026-07-01T00:59:00Z", "2026-07-02", "01:59:00", 0),
        ("2026-07-01T01:00:00Z", "2026-07-01", "02:00:00", 0),
        ("2026-07-01T01:00:00Z", "2026-06-30", "02:00:00", 1),
        ("2026-07-01T01:00:00Z", "2026-06-29", "02:00:00", 2),
        ("2026-07-01T01:00:00Z", "2026-06-28", "02:00:00", 2),
        ("2026-07-01T01:00:00Z", "2026-07-02", "02:00:00", 0),
        ("2026-07-01T01:30:00Z", "2026-07-01", "02:30:00", 0),
        ("2026-07-01T01:30:00Z", "2026-06-30", "02:30:00", 1),
        ("2026-07-01T01:30:00Z", "2026-06-29", "02:30:00", 2),
        ("2026-07-01T01:30:00Z", "2026-06-28", "02:30:00", 2),
        ("2026-07-01T01:30:00Z", "2026-07-02", "02:30:00", 0),
        ("2026-07-01T01:59:00Z", "2026-07-01", "02:59:00", 0),
        ("2026-07-01T01:59:00Z", "2026-06-30", "02:59:00", 1),
        ("2026-07-01T01:59:00Z", "2026-06-29", "02:59:00", 2),
        ("2026-07-01T01:59:00Z", "2026-06-28", "02:59:00", 2),
        ("2026-07-01T01:59:00Z", "2026-07-02", "02:59:00", 0),
        ("2026-07-01T02:00:00Z", "2026-07-01", "03:00:00", 0),
        ("2026-07-01T02:00:00Z", "2026-06-30", "03:00:00", 1),
        ("2026-07-01T02:00:00Z", "2026-06-29", "03:00:00", 2),
        ("2026-07-01T02:00:00Z", "2026-06-28", "03:00:00", 2),
        ("2026-07-01T02:00:00Z", "2026-07-02", "03:00:00", 0),
        ("2026-07-01T02:30:00Z", "2026-07-01", "03:30:00", 0),
        ("2026-07-01T02:30:00Z", "2026-06-30", "03:30:00", 1),
        ("2026-07-01T02:30:00Z", "2026-06-29", "03:30:00", 2),
        ("2026-07-01T02:30:00Z", "2026-06-28", "03:30:00", 2),
        ("2026-07-01T02:30:00Z", "2026-07-02", "03:30:00", 0),
        ("2026-07-15T22:00:00Z", "2026-07-15", "23:00:00", 0),
        ("2026-07-15T22:00:00Z", "2026-07-14", "23:00:00", 1),
        ("2026-07-15T22:00:00Z", "2026-07-13", "23:00:00", 2),
        ("2026-07-15T22:00:00Z", "2026-07-12", "23:00:00", 2),
        ("2026-07-15T22:00:00Z", "2026-07-16", "23:00:00", 0),
        ("2026-07-15T22:59:00Z", "2026-07-15", "23:59:00", 0),
        ("2026-07-15T22:59:00Z", "2026-07-14", "23:59:00", 1),
        ("2026-07-15T22:59:00Z", "2026-07-13", "23:59:00", 2),
        ("2026-07-15T22:59:00Z", "2026-07-12", "23:59:00", 2),
        ("2026-07-15T22:59:00Z", "2026-07-16", "23:59:00", 0),
        ("2026-07-15T23:00:00Z", "2026-07-16", "00:00:00", 0),
        ("2026-07-15T23:00:00Z", "2026-07-15", "00:00:00", 1),
        ("2026-07-15T23:00:00Z", "2026-07-14", "00:00:00", 2),
        ("2026-07-15T23:00:00Z", "2026-07-13", "00:00:00", 2),
        ("2026-07-15T23:00:00Z", "2026-07-17", "00:00:00", 0),
        ("2026-07-15T23:30:00Z", "2026-07-16", "00:30:00", 0),
        ("2026-07-15T23:30:00Z", "2026-07-15", "00:30:00", 1),
        ("2026-07-15T23:30:00Z", "2026-07-14", "00:30:00", 2),
        ("2026-07-15T23:30:00Z", "2026-07-13", "00:30:00", 2),
        ("2026-07-15T23:30:00Z", "2026-07-17", "00:30:00", 0),
        ("2026-07-15T23:59:00Z", "2026-07-16", "00:59:00", 0),
        ("2026-07-15T23:59:00Z", "2026-07-15", "00:59:00", 1),
        ("2026-07-15T23:59:00Z", "2026-07-14", "00:59:00", 2),
        ("2026-07-15T23:59:00Z", "2026-07-13", "00:59:00", 2),
        ("2026-07-15T23:59:00Z", "2026-07-17", "00:59:00", 0),
        ("2026-07-16T00:00:00Z", "2026-07-16", "01:00:00", 0),
        ("2026-07-16T00:00:00Z", "2026-07-15", "01:00:00", 1),
        ("2026-07-16T00:00:00Z", "2026-07-14", "01:00:00", 2),
        ("2026-07-16T00:00:00Z", "2026-07-13", "01:00:00", 2),
        ("2026-07-16T00:00:00Z", "2026-07-17", "01:00:00", 0),
        ("2026-07-16T00:30:00Z", "2026-07-16", "01:30:00", 0),
        ("2026-07-16T00:30:00Z", "2026-07-15", "01:30:00", 1),
        ("2026-07-16T00:30:00Z", "2026-07-14", "01:30:00", 2),
        ("2026-07-16T00:30:00Z", "2026-07-13", "01:30:00", 2),
        ("2026-07-16T00:30:00Z", "2026-07-17", "01:30:00", 0),
        ("2026-07-16T00:59:00Z", "2026-07-16", "01:59:00", 0),
        ("2026-07-16T00:59:00Z", "2026-07-15", "01:59:00", 1),
        ("2026-07-16T00:59:00Z", "2026-07-14", "01:59:00", 2),
        ("2026-07-16T00:59:00Z", "2026-07-13", "01:59:00", 2),
        ("2026-07-16T00:59:00Z", "2026-07-17", "01:59:00", 0),
        ("2026-07-16T01:00:00Z", "2026-07-16", "02:00:00", 0),
        ("2026-07-16T01:00:00Z", "2026-07-15", "02:00:00", 1),
        ("2026-07-16T01:00:00Z", "2026-07-14", "02:00:00", 2),
        ("2026-07-16T01:00:00Z", "2026-07-13", "02:00:00", 2),
        ("2026-07-16T01:00:00Z", "2026-07-17", "02:00:00", 0),
        ("2026-07-16T01:30:00Z", "2026-07-16", "02:30:00", 0),
        ("2026-07-16T01:30:00Z", "2026-07-15", "02:30:00", 1),
        ("2026-07-16T01:30:00Z", "2026-07-14", "02:30:00", 2),
        ("2026-07-16T01:30:00Z", "2026-07-13", "02:30:00", 2),
        ("2026-07-16T01:30:00Z", "2026-07-17", "02:30:00", 0),
        ("2026-07-16T01:59:00Z", "2026-07-16", "02:59:00", 0),
        ("2026-07-16T01:59:00Z", "2026-07-15", "02:59:00", 1),
        ("2026-07-16T01:59:00Z", "2026-07-14", "02:59:00", 2),
        ("2026-07-16T01:59:00Z", "2026-07-13", "02:59:00", 2),
        ("2026-07-16T01:59:00Z", "2026-07-17", "02:59:00", 0),
        ("2026-07-16T02:00:00Z", "2026-07-16", "03:00:00", 0),
        ("2026-07-16T02:00:00Z", "2026-07-15", "03:00:00", 1),
        ("2026-07-16T02:00:00Z", "2026-07-14", "03:00:00", 2),
        ("2026-07-16T02:00:00Z", "2026-07-13", "03:00:00", 2),
        ("2026-07-16T02:00:00Z", "2026-07-17", "03:00:00", 0),
        ("2026-07-16T02:30:00Z", "2026-07-16", "03:30:00", 0),
        ("2026-07-16T02:30:00Z", "2026-07-15", "03:30:00", 1),
        ("2026-07-16T02:30:00Z", "2026-07-14", "03:30:00", 2),
        ("2026-07-16T02:30:00Z", "2026-07-13", "03:30:00", 2),
        ("2026-07-16T02:30:00Z", "2026-07-17", "03:30:00", 0),
        ("2026-10-24T22:00:00Z", "2026-10-24", "23:00:00", 0),
        ("2026-10-24T22:00:00Z", "2026-10-23", "23:00:00", 1),
        ("2026-10-24T22:00:00Z", "2026-10-22", "23:00:00", 2),
        ("2026-10-24T22:00:00Z", "2026-10-21", "23:00:00", 2),
        ("2026-10-24T22:00:00Z", "2026-10-25", "23:00:00", 0),
        ("2026-10-24T23:00:00Z", "2026-10-25", "00:00:00", 0),
        ("2026-10-24T23:00:00Z", "2026-10-24", "00:00:00", 1),
        ("2026-10-24T23:00:00Z", "2026-10-23", "00:00:00", 2),
        ("2026-10-24T23:00:00Z", "2026-10-22", "00:00:00", 2),
        ("2026-10-24T23:00:00Z", "2026-10-26", "00:00:00", 0),
        ("2026-10-24T23:30:00Z", "2026-10-25", "00:30:00", 0),
        ("2026-10-24T23:30:00Z", "2026-10-24", "00:30:00", 1),
        ("2026-10-24T23:30:00Z", "2026-10-23", "00:30:00", 2),
        ("2026-10-24T23:30:00Z", "2026-10-22", "00:30:00", 2),
        ("2026-10-24T23:30:00Z", "2026-10-26", "00:30:00", 0),
        ("2026-10-25T00:00:00Z", "2026-10-25", "01:00:00", 0),
        ("2026-10-25T00:00:00Z", "2026-10-24", "01:00:00", 1),
        ("2026-10-25T00:00:00Z", "2026-10-23", "01:00:00", 2),
        ("2026-10-25T00:00:00Z", "2026-10-22", "01:00:00", 2),
        ("2026-10-25T00:00:00Z", "2026-10-26", "01:00:00", 0),
        ("2026-10-25T00:30:00Z", "2026-10-25", "01:30:00", 0),
        ("2026-10-25T00:30:00Z", "2026-10-24", "01:30:00", 1),
        ("2026-10-25T00:30:00Z", "2026-10-23", "01:30:00", 2),
        ("2026-10-25T00:30:00Z", "2026-10-22", "01:30:00", 2),
        ("2026-10-25T00:30:00Z", "2026-10-26", "01:30:00", 0),
        ("2026-10-25T00:59:00Z", "2026-10-25", "01:59:00", 0),
        ("2026-10-25T00:59:00Z", "2026-10-24", "01:59:00", 1),
        ("2026-10-25T00:59:00Z", "2026-10-23", "01:59:00", 2),
        ("2026-10-25T00:59:00Z", "2026-10-22", "01:59:00", 2),
        ("2026-10-25T00:59:00Z", "2026-10-26", "01:59:00", 0),
        ("2026-10-25T01:00:00Z", "2026-10-25", "01:00:00", 0),
        ("2026-10-25T01:00:00Z", "2026-10-24", "01:00:00", 1),
        ("2026-10-25T01:00:00Z", "2026-10-23", "01:00:00", 2),
        ("2026-10-25T01:00:00Z", "2026-10-22", "01:00:00", 2),
        ("2026-10-25T01:00:00Z", "2026-10-26", "01:00:00", 0),
        ("2026-10-25T01:01:00Z", "2026-10-25", "01:01:00", 0),
        ("2026-10-25T01:01:00Z", "2026-10-24", "01:01:00", 1),
        ("2026-10-25T01:01:00Z", "2026-10-23", "01:01:00", 2),
        ("2026-10-25T01:01:00Z", "2026-10-22", "01:01:00", 2),
        ("2026-10-25T01:01:00Z", "2026-10-26", "01:01:00", 0),
        ("2026-10-25T01:30:00Z", "2026-10-25", "01:30:00", 0),
        ("2026-10-25T01:30:00Z", "2026-10-24", "01:30:00", 1),
        ("2026-10-25T01:30:00Z", "2026-10-23", "01:30:00", 2),
        ("2026-10-25T01:30:00Z", "2026-10-22", "01:30:00", 2),
        ("2026-10-25T01:30:00Z", "2026-10-26", "01:30:00", 0),
        ("2026-10-25T01:59:00Z", "2026-10-25", "01:59:00", 0),
        ("2026-10-25T01:59:00Z", "2026-10-24", "01:59:00", 1),
        ("2026-10-25T01:59:00Z", "2026-10-23", "01:59:00", 2),
        ("2026-10-25T01:59:00Z", "2026-10-22", "01:59:00", 2),
        ("2026-10-25T01:59:00Z", "2026-10-26", "01:59:00", 0),
        ("2026-10-25T02:00:00Z", "2026-10-25", "02:00:00", 0),
        ("2026-10-25T02:00:00Z", "2026-10-24", "02:00:00", 1),
        ("2026-10-25T02:00:00Z", "2026-10-23", "02:00:00", 2),
        ("2026-10-25T02:00:00Z", "2026-10-22", "02:00:00", 2),
        ("2026-10-25T02:00:00Z", "2026-10-26", "02:00:00", 0),
        ("2026-10-25T02:30:00Z", "2026-10-25", "02:30:00", 0),
        ("2026-10-25T02:30:00Z", "2026-10-24", "02:30:00", 1),
        ("2026-10-25T02:30:00Z", "2026-10-23", "02:30:00", 2),
        ("2026-10-25T02:30:00Z", "2026-10-22", "02:30:00", 2),
        ("2026-10-25T02:30:00Z", "2026-10-26", "02:30:00", 0),
        ("2026-10-25T03:00:00Z", "2026-10-25", "03:00:00", 0),
        ("2026-10-25T03:00:00Z", "2026-10-24", "03:00:00", 1),
        ("2026-10-25T03:00:00Z", "2026-10-23", "03:00:00", 2),
        ("2026-10-25T03:00:00Z", "2026-10-22", "03:00:00", 2),
        ("2026-10-25T03:00:00Z", "2026-10-26", "03:00:00", 0),
        ("2026-10-25T12:00:00Z", "2026-10-25", "12:00:00", 0),
        ("2026-10-25T12:00:00Z", "2026-10-24", "12:00:00", 1),
        ("2026-10-25T12:00:00Z", "2026-10-23", "12:00:00", 2),
        ("2026-10-25T12:00:00Z", "2026-10-22", "12:00:00", 2),
        ("2026-10-25T12:00:00Z", "2026-10-26", "12:00:00", 0),
        ("2026-10-25T22:59:00Z", "2026-10-25", "22:59:00", 0),
        ("2026-10-25T22:59:00Z", "2026-10-24", "22:59:00", 1),
        ("2026-10-25T22:59:00Z", "2026-10-23", "22:59:00", 2),
        ("2026-10-25T22:59:00Z", "2026-10-22", "22:59:00", 2),
        ("2026-10-25T22:59:00Z", "2026-10-26", "22:59:00", 0),
        ("2026-10-25T23:00:00Z", "2026-10-25", "23:00:00", 0),
        ("2026-10-25T23:00:00Z", "2026-10-24", "23:00:00", 1),
        ("2026-10-25T23:00:00Z", "2026-10-23", "23:00:00", 2),
        ("2026-10-25T23:00:00Z", "2026-10-22", "23:00:00", 2),
        ("2026-10-25T23:00:00Z", "2026-10-26", "23:00:00", 0),
        ("2026-10-25T23:30:00Z", "2026-10-25", "23:30:00", 0),
        ("2026-10-25T23:30:00Z", "2026-10-24", "23:30:00", 1),
        ("2026-10-25T23:30:00Z", "2026-10-23", "23:30:00", 2),
        ("2026-10-25T23:30:00Z", "2026-10-22", "23:30:00", 2),
        ("2026-10-25T23:30:00Z", "2026-10-26", "23:30:00", 0),
        ("2026-10-26T00:30:00Z", "2026-10-26", "00:30:00", 0),
        ("2026-10-26T00:30:00Z", "2026-10-25", "00:30:00", 1),
        ("2026-10-26T00:30:00Z", "2026-10-24", "00:30:00", 2),
        ("2026-10-26T00:30:00Z", "2026-10-23", "00:30:00", 2),
        ("2026-10-26T00:30:00Z", "2026-10-27", "00:30:00", 0),
    ];

    #[test]
    fn london_time_matches_the_table() {
        for &(at, service_date, time, days) in TIMES {
            assert_eq!(
                london_time(at.parse().unwrap(), service_date.parse().unwrap()),
                (time.parse().unwrap(), days),
                "{at} for {service_date}"
            );
        }
    }
}
