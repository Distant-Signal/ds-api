//! The `/trains` page's two schedule views that are not the calling-point
//! search (2026-10-30):
//!
//! * `GET /public/trains/arrivals` -- a station's scheduled arrivals for a
//!   date and time window: every train that sets down there, its
//!   terminating trains included. The calling-point search
//!   (`get_trains_search`) only sees departures, since a terminus has no
//!   departure row.
//! * `GET /public/trains/by-headcode` -- the trains running under one
//!   headcode on a date, each as a row at its origin, for finding a
//!   specific train when you know its headcode but not where it runs.
//!
//! Both read the published timetable only (`schedule_destination_departures`,
//! and `schedule_services` for the headcode's uids), like the search, and
//! render rows through `render::calling_point_departure_json`, so a client
//! shows them with the search's own row component.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{
    WindowBound, WindowBoundJson, WindowDayOffsets, WindowTimes, check_known_operators,
    check_search_date, internal_error, non_empty, normalize_crs, normalize_limit, normalize_time,
    parse_date, parse_day_offset, parse_operators, resolve_window,
};
use crate::app::App;
use crate::data::queries;
use crate::data::stop_filters;
use crate::render::calling_point_departure_json;
use crate::routes::schedule_rows::TrainSearchRow;

/// The most trains `by-headcode` returns. A headcode is reused across the
/// network (a `2C10` in Kent and another in Scotland), but rarely more than
/// a handful of times a day.
const MAX_HEADCODE_MATCHES: i64 = 50;

/// Query parameters of `GET /public/trains/arrivals`. Unknown names are a
/// `400`, as for the search.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArrivalsParams {
    /// Required, a 3-letter station code.
    station: String,
    /// Optional `"YYYY-MM-DD"`, the service date; today by default, and the
    /// same accepted range as the search's `date`.
    date: Option<String>,
    /// Optional `"HH:MM"` bounds on the arrival, inclusive, with
    /// `from_day_offset`/`to_day_offset` as the search has them. With no
    /// `from`, today's search starts at `now` and any other date's at
    /// midnight.
    from: Option<String>,
    to: Option<String>,
    from_day_offset: Option<String>,
    to_day_offset: Option<String>,
    /// Optional, a 3-letter station code: only trains that start there
    /// (the search's `origin`). The stop filters look at the same rows
    /// here, so an `origin` in `avoid` or `avoid_stop` is a `400`.
    origin: Option<String>,
    /// Optional (2026-10-30, DS-213), a 3-letter station code: only trains
    /// that terminate there (the search's `terminates_at`). `station`
    /// itself is allowed and is the common case: only the trains that end
    /// here, the rows with `terminates: true` (and a working that passes
    /// through and comes back to end here).
    terminates_at: Option<String>,
    /// Optional comma-separated ATOC codes: only trains run by one of them
    /// (the search's `operator`, validated the same way).
    operator: Option<String>,
    /// Optional (2026-10-30), the search's advanced stop filters, over the
    /// part of the train's run BEFORE it reaches `station`: `calls_at`,
    /// `via`, `avoid` and `avoid_stop` (see `super::stop_filters`).
    calls_at: Option<String>,
    via: Option<String>,
    avoid: Option<String>,
    avoid_stop: Option<String>,
    /// Optional (2026-10-30, DS-213), the search's `previously_calls_at`
    /// and `previously_stops_at`, also before `station`: here
    /// `previously_calls_at` means what `calls_at` does, and
    /// `previously_stops_at` leaves out the train's origin.
    previously_calls_at: Option<String>,
    previously_stops_at: Option<String>,
    /// Page size, as the search's.
    limit: Option<String>,
    /// A previous page's `nextCursor`.
    after: Option<String>,
}

/// Query parameters of `GET /public/trains/by-headcode`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HeadcodeParams {
    /// Required, a 4-character headcode: a digit, a letter, two digits
    /// (`1S00`). Case-insensitive.
    headcode: String,
    /// Optional `"YYYY-MM-DD"`, as the search's `date`.
    date: Option<String>,
}

/// Validates and uppercases a headcode (the CIF Train Identity).
fn normalize_headcode(raw: &str) -> Result<String, (StatusCode, String)> {
    let code = raw.trim().to_ascii_uppercase();
    let bytes = code.as_bytes();
    let valid = bytes.len() == 4
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_alphabetic()
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit();
    if valid {
        Ok(code)
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "headcode must be a digit, a letter and two digits, like 1S00".to_string(),
        ))
    }
}

/// The service date a request names: `date`, window-checked as the
/// search's, else today.
async fn requested_date(
    app: &App,
    raw: Option<&str>,
    today: chrono::NaiveDate,
) -> Result<chrono::NaiveDate, (StatusCode, String)> {
    match raw.filter(|s| !s.trim().is_empty()) {
        Some(raw) => {
            let date = parse_date(raw)?;
            check_search_date(app, date, today).await?;
            Ok(date)
        }
        None => Ok(today),
    }
}

/// The `404` the search gives for a date with no timetable at all.
fn unpublished(service_date: chrono::NaiveDate, today: chrono::NaiveDate) -> (StatusCode, String) {
    let day = if service_date == today {
        "today".to_string()
    } else {
        service_date.format("%Y-%m-%d").to_string()
    };
    (
        StatusCode::NOT_FOUND,
        format!("no CIF-derived schedule data has been published for {day}"),
    )
}

/// One `schedule_destination_departures` row, as both views read it.
#[derive(Debug, sqlx::FromRow)]
struct BoardRow {
    train_uid: String,
    origin_crs: String,
    destination_crs: String,
    true_origin_crs: Option<String>,
    scheduled: chrono::NaiveTime,
    day_offset: i16,
    public_departure: Option<chrono::NaiveTime>,
    calling_point_arrival: Option<chrono::NaiveTime>,
    public_calling_point_arrival: Option<chrono::NaiveTime>,
    destination_arrival: Option<chrono::NaiveTime>,
    destination_arrival_day_offset: i16,
    public_destination_arrival: Option<chrono::NaiveTime>,
    operator_atoc: Option<String>,
    headcode: Option<String>,
}

const BOARD_COLUMNS: &str = "train_uid, origin_crs, destination_crs, true_origin_crs, scheduled, \
     day_offset, public_departure, calling_point_arrival, public_calling_point_arrival, \
     destination_arrival, destination_arrival_day_offset, public_destination_arrival, \
     operator_atoc, headcode";

impl BoardRow {
    /// The search's raw row shape, for `calling_point_departure_json`.
    fn raw_json(&self, service_date: chrono::NaiveDate) -> Value {
        let hms = |t: chrono::NaiveTime| t.format("%H:%M:%S").to_string();
        json!({
            "uid": self.train_uid,
            "service_date": service_date,
            "day_offset": self.day_offset,
            "destination_crs": self.destination_crs,
            "true_origin_crs": self.true_origin_crs,
            "scheduled": hms(self.scheduled),
            "destination_arrival": self.destination_arrival.map(hms),
            "destination_arrival_day_offset": self.destination_arrival_day_offset,
            "operator_atoc": self.operator_atoc,
            "headcode": self.headcode,
            "public_departure": self.public_departure.map(hms),
            "public_destination_arrival": self.public_destination_arrival.map(hms),
        })
    }

    /// This call's departure as shown (public, else working) and its day
    /// offset.
    fn shown_departure(&self) -> (chrono::NaiveTime, i64) {
        let working_offset = i64::from(self.day_offset);
        match self.public_departure {
            Some(public) => (
                public,
                nearest_day_offset(self.scheduled, working_offset, public),
            ),
            None => (self.scheduled, working_offset),
        }
    }

    /// The arrival at this row's own calling point (an intermediate call),
    /// as shown, with its day offset: the working arrival dated from the
    /// call's departure, then the public one from that. `None` when the
    /// call has no arrival (the train starts here).
    fn call_arrival(&self) -> Option<(chrono::NaiveTime, i64)> {
        let working = self.calling_point_arrival.map(|t| {
            (
                t,
                nearest_day_offset(self.scheduled, i64::from(self.day_offset), t),
            )
        });
        let reference = working.unwrap_or((self.scheduled, i64::from(self.day_offset)));
        match self.public_calling_point_arrival {
            Some(public) => Some((public, nearest_day_offset(reference.0, reference.1, public))),
            None => working,
        }
    }

    /// The arrival at the train's terminus, as shown, with its day offset.
    fn terminus_arrival(&self) -> Option<(chrono::NaiveTime, i64)> {
        let working = self
            .destination_arrival
            .map(|t| (t, i64::from(self.destination_arrival_day_offset)));
        match (self.public_destination_arrival, working) {
            (Some(public), Some((time, offset))) => {
                Some((public, nearest_day_offset(time, offset, public)))
            }
            (Some(public), None) => Some((public, i64::from(self.destination_arrival_day_offset))),
            (None, working) => working,
        }
    }
}

/// `time`'s day offset, taken the short way round midnight from
/// `reference` on day `reference_offset` (`queries::nearest_day_offset`'s
/// rule).
fn nearest_day_offset(
    reference: chrono::NaiveTime,
    reference_offset: i64,
    time: chrono::NaiveTime,
) -> i64 {
    const HALF_DAY_SECONDS: i64 = 12 * 60 * 60;
    let delta = (time - reference).num_seconds();
    if delta > HALF_DAY_SECONDS {
        (reference_offset - 1).max(0)
    } else if delta < -HALF_DAY_SECONDS {
        reference_offset + 1
    } else {
        reference_offset
    }
}

/// Train-day minutes of a time on day `day_offset`.
fn minutes(time: chrono::NaiveTime, day_offset: i64) -> i32 {
    queries::train_day_minutes(time, u32::try_from(day_offset.clamp(0, 7)).unwrap_or(0))
}

/// One arrival on the board, before rendering.
struct Arrival {
    row: BoardRow,
    time: chrono::NaiveTime,
    day_offset: i64,
    terminates: bool,
}

impl Arrival {
    fn key(&self) -> (i32, &str) {
        (
            minutes(self.time, self.day_offset),
            self.row.train_uid.as_str(),
        )
    }

    /// The raw row, the station and the arrival fields, for `render_rows`.
    fn render_parts(self, service_date: chrono::NaiveDate) -> (Value, ArrivalFields) {
        let fields = ArrivalFields {
            arrival: self.time.format("%H:%M").to_string(),
            arrival_day_offset: self.day_offset,
            terminates: self.terminates,
            // The row read for a terminating train is its first call, not
            // a departure from `station`: there is none, so no `dayOffset`
            // either.
            day_offset: (!self.terminates).then(|| self.row.shown_departure().1),
        };
        (self.row.raw_json(service_date), fields)
    }
}

/// What an arrival adds to its board row.
struct ArrivalFields {
    arrival: String,
    arrival_day_offset: i64,
    terminates: bool,
    day_offset: Option<i64>,
}

/// What an arrival changes on its search row: its `dayOffset`, and no
/// departure for a train terminating at the station.
fn apply_arrival(row: &mut TrainSearchRow, fields: &ArrivalFields) {
    row.day_offset = fields.day_offset;
    if fields.terminates {
        row.departure.scheduled = None;
        row.departure.public_departure = None;
        row.departure.working_departure = None;
    }
}

fn arrival_row(row: TrainSearchRow, fields: ArrivalFields) -> TrainArrivalRow {
    TrainArrivalRow {
        row,
        arrival: fields.arrival,
        arrival_day_offset: fields.arrival_day_offset,
        terminates: fields.terminates,
    }
}

/// One row of `GET /public/trains/arrivals`: the search's row plus the
/// arrival at the station. For a train terminating there, `scheduled`,
/// `publicDeparture` and `workingDeparture` are `null` (there is no
/// departure onward) and `dayOffset` is absent.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "TrainArrivalResult")]
pub(crate) struct TrainArrivalRow {
    #[serde(flatten)]
    row: TrainSearchRow,
    /// `"HH:MM"`, the public arrival, else the working one.
    arrival: String,
    arrival_day_offset: i64,
    terminates: bool,
}

/// `GET /public/trains/arrivals`'s body.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrainArrivalsPage {
    results: Vec<TrainArrivalRow>,
    next_cursor: Option<String>,
    service_date: chrono::NaiveDate,
    window: ArrivalsWindowJson,
    #[serde(flatten)]
    certainty: crate::routes::provisional::TimetableCertainty,
}

/// The arrivals board's `window` as applied: `to` is `null` when
/// unbounded.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(rename = "TrainArrivalsWindow")]
pub(crate) struct ArrivalsWindowJson {
    from: WindowBoundJson,
    to: Option<WindowBoundJson>,
}

/// `GET /public/trains/by-headcode`'s body.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrainHeadcodePage {
    results: Vec<TrainSearchRow>,
    service_date: chrono::NaiveDate,
    #[serde(flatten)]
    certainty: crate::routes::provisional::TimetableCertainty,
}

struct ArrivalsFilter<'a> {
    /// The window, train-day minutes, inclusive.
    from_minutes: i32,
    to_minutes: Option<i32>,
    /// Only trains starting here.
    origin: Option<&'a str>,
    /// Only trains terminating here.
    terminates_at: Option<&'a str>,
    /// Only trains run by one of these.
    operators: Option<&'a [String]>,
    /// Only rows after this `(minute, uid)` (a cursor).
    after: Option<(i32, &'a str)>,
}

impl ArrivalsFilter<'_> {
    fn keeps(&self, arrival: &Arrival) -> bool {
        let key = arrival.key();
        key.0 >= self.from_minutes
            && self.to_minutes.is_none_or(|to| key.0 <= to)
            && self
                .origin
                .is_none_or(|origin| arrival.row.true_origin_crs.as_deref() == Some(origin))
            && self
                .terminates_at
                .is_none_or(|terminus| arrival.row.destination_crs == terminus)
            && self.operators.is_none_or(|codes| {
                arrival
                    .row
                    .operator_atoc
                    .as_ref()
                    .is_some_and(|code| codes.contains(code))
            })
            && self.after.is_none_or(|after| key > after)
    }

    /// The first `limit` arrivals kept, in order, and the cursor after the
    /// last of them when more follow.
    fn page(&self, arrivals: Vec<Arrival>, limit: usize) -> (Vec<Arrival>, Option<String>) {
        let page: Vec<Arrival> = arrivals
            .into_iter()
            .filter(|arrival| self.keeps(arrival))
            .take(limit.saturating_add(1))
            .collect();
        finish_page(page, limit)
    }

    /// [`Self::page`] with the advanced stop filters too: the arrivals
    /// kept so far are checked in chunks, each one read of its trains'
    /// calling points, until a page is full.
    async fn stop_filtered_page(
        &self,
        pool: &PgPool,
        service_date: chrono::NaiveDate,
        stop_filter: &ArrivalsStopFilter,
        arrivals: Vec<Arrival>,
        limit: usize,
    ) -> anyhow::Result<(Vec<Arrival>, Option<String>)> {
        let mut candidates = arrivals
            .into_iter()
            .filter(|arrival| self.keeps(arrival))
            .peekable();
        let mut page: Vec<Arrival> = Vec::new();
        while page.len() <= limit && candidates.peek().is_some() {
            let chunk: Vec<Arrival> = candidates.by_ref().take(STOP_FILTER_CHUNK).collect();
            let mut uids: Vec<String> = chunk.iter().map(|a| a.row.train_uid.clone()).collect();
            uids.sort_unstable();
            uids.dedup();
            let calls = stop_filters::fetch_calls(pool, service_date, &uids).await?;
            for arrival in chunk {
                if page.len() > limit {
                    break;
                }
                let departure = (!arrival.terminates)
                    .then_some((arrival.row.scheduled, arrival.row.day_offset));
                let passes = calls.get(&arrival.row.train_uid).is_some_and(|calls| {
                    stop_filters::arrival_span(calls, &stop_filter.station_tiplocs, departure)
                        .is_some_and(|span| {
                            stop_filter.filters.accepts(span)
                                && stop_filter.filters.accepts_earlier(span)
                        })
                });
                if passes {
                    page.push(arrival);
                }
            }
        }
        Ok(finish_page(page, limit))
    }
}

/// How many arrivals [`ArrivalsFilter::stop_filtered_page`] checks per
/// read of calling points.
const STOP_FILTER_CHUNK: usize = 200;

/// The arrivals board's stop filters, with the station's TIPLOCs.
struct ArrivalsStopFilter {
    filters: stop_filters::StopFilters,
    station_tiplocs: std::collections::HashSet<String>,
}

impl ArrivalsStopFilter {
    async fn resolve(
        pool: &PgPool,
        filters: &super::stop_filters::ParsedStopFilters,
        station: &str,
    ) -> Result<Self, (StatusCode, String)> {
        Ok(Self {
            filters: filters.resolve(pool).await?,
            station_tiplocs: super::stop_filters::station_tiplocs(pool, &[station])
                .await?
                .pop()
                .unwrap_or_default(),
        })
    }
}

/// `page` (at most `limit + 1` arrivals, in order) cut to `limit`, and the
/// cursor after its last row when one more followed.
fn finish_page(mut page: Vec<Arrival>, limit: usize) -> (Vec<Arrival>, Option<String>) {
    let has_more = page.len() > limit;
    page.truncate(limit);
    let next_cursor = has_more.then(|| page.last()).flatten().map(|arrival| {
        let (minute, uid) = arrival.key();
        encode_arrivals_cursor(minute, uid)
    });
    (page, next_cursor)
}

/// Every arrival at `station` on `service_date`: each call there with an
/// arrival (a train that can set down), then each train that terminates
/// there. One read each: the calls over the calling-point index, the
/// termini over the primary key's `(service_date, destination_crs)`
/// prefix, one row per train (its first).
async fn station_arrivals(
    pool: &PgPool,
    service_date: chrono::NaiveDate,
    station: &str,
) -> anyhow::Result<Vec<Arrival>> {
    let calls: Vec<BoardRow> = sqlx::query_as(&format!(
        "SELECT {BOARD_COLUMNS} FROM schedule_destination_departures \
         WHERE service_date = $1 AND origin_crs = $2 \
           AND can_alight IS NOT FALSE \
           AND (calling_point_arrival IS NOT NULL OR public_calling_point_arrival IS NOT NULL)"
    ))
    .bind(service_date)
    .bind(station)
    .fetch_all(pool)
    .await?;
    let termini: Vec<BoardRow> = sqlx::query_as(&format!(
        "SELECT DISTINCT ON (train_uid) {BOARD_COLUMNS} FROM schedule_destination_departures \
         WHERE service_date = $1 AND destination_crs = $2 \
           AND (destination_arrival IS NOT NULL OR public_destination_arrival IS NOT NULL) \
         ORDER BY train_uid, day_offset, scheduled"
    ))
    .bind(service_date)
    .bind(station)
    .fetch_all(pool)
    .await?;
    let mut arrivals: Vec<Arrival> = Vec::with_capacity(calls.len() + termini.len());
    for row in calls {
        if let Some((time, day_offset)) = row.call_arrival() {
            arrivals.push(Arrival {
                row,
                time,
                day_offset,
                terminates: false,
            });
        }
    }
    for row in termini {
        if let Some((time, day_offset)) = row.terminus_arrival() {
            arrivals.push(Arrival {
                row,
                time,
                day_offset,
                terminates: true,
            });
        }
    }
    arrivals.sort_by(|a, b| a.key().cmp(&b.key()));
    Ok(arrivals)
}

/// The arrivals cursor: base64url of `"minutes|uid"`, the last row's
/// train-day minute and uid.
fn encode_arrivals_cursor(minute: i32, uid: &str) -> String {
    URL_SAFE_NO_PAD.encode(format!("{minute}|{uid}"))
}

fn decode_arrivals_cursor(raw: &str) -> Result<(i32, String), (StatusCode, String)> {
    let invalid = || {
        (
            StatusCode::BAD_REQUEST,
            "after must be a cursor returned by a previous request".to_string(),
        )
    };
    let bytes = URL_SAFE_NO_PAD.decode(raw.trim()).map_err(|_| invalid())?;
    let decoded = String::from_utf8(bytes).map_err(|_| invalid())?;
    let (minute, uid) = decoded.split_once('|').ok_or_else(invalid)?;
    let minute: i32 = minute.parse().map_err(|_| invalid())?;
    if uid.is_empty() || !(0..=8 * 1440).contains(&minute) {
        return Err(invalid());
    }
    Ok((minute, uid.to_string()))
}

/// Renders rows through the search's renderer, names their origins and
/// destinations, and attaches live state and service mode. Each row is a
/// stored-shape row and the station it is rendered at, and `extend` sets
/// what the board adds to the rendered row.
async fn render_rows<X>(
    app: &App,
    service_date: chrono::NaiveDate,
    rows: Vec<(Value, String, X)>,
    extend: impl Fn(&mut TrainSearchRow, &X),
) -> Result<Vec<(TrainSearchRow, X)>, (StatusCode, String)> {
    let codes: Vec<String> = rows
        .iter()
        .flat_map(|(raw, _, _)| {
            ["destination_crs", "true_origin_crs"]
                .into_iter()
                .filter_map(|key| raw.get(key).and_then(Value::as_str))
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect();
    let names: HashMap<String, String> =
        queries::station_names_for_crs_batch(&app.database, &codes)
            .await
            .map_err(internal_error)?;
    let (mut results, extras): (Vec<TrainSearchRow>, Vec<X>) = rows
        .into_iter()
        .map(|(raw, station, extra)| {
            let mut rendered = TrainSearchRow::new(
                calling_point_departure_json(&raw, &station, &names),
                service_date.to_string(),
            );
            extend(&mut rendered, &extra);
            (rendered, extra)
        })
        .unzip();
    crate::routes::schedule_rows::attach_origin_names(&mut results, &names);
    crate::routes::schedule_rows::attach_live(&app.database, service_date, &mut results).await;
    crate::routes::schedule_rows::annotate_service_modes(&app.database, service_date, &mut results)
        .await;
    Ok(results.into_iter().zip(extras).collect())
}

/// `GET /public/trains/arrivals?station=[&date=][&from=][&to=][&from_day_offset=][&to_day_offset=][&origin=][&terminates_at=][&operator=][&limit=][&after=]`
/// -- a station's scheduled arrivals (2026-10-30).
///
/// One row per arrival, in train-day order of the arrival as shown (the
/// public time, else the working one): every call at `station` a passenger
/// can get off at, and every train terminating there. A row is the search's
/// row (`uid`, `originCrs`/`originName`, `destinationCrs`/`destinationName`,
/// `operator`, `identity`, `serviceMode`, `live`, `serviceDate`) plus
/// `arrival` (`"HH:MM"`), `arrivalDayOffset` (days after `serviceDate`),
/// `terminates` (whether the train ends here) and `scheduled`/`dayOffset`,
/// the departure onward from `station` (`null`/absent for a terminating
/// train). Only `date`'s own trains: the previous date's trains arriving
/// after midnight are not included.
///
/// `200 {"results", "nextCursor", "serviceDate", "window"}` plus the
/// timetable-certainty fields; `404` when nothing is published for the
/// date; `400` for a malformed parameter.
#[expect(
    clippy::too_many_lines,
    reason = "long but linear, as `get_trains_search`: validate, read, page, render"
)]
pub(super) async fn get_trains_arrivals(
    State(app): State<App>,
    Query(params): Query<ArrivalsParams>,
) -> Result<Json<TrainArrivalsPage>, (StatusCode, String)> {
    let station = normalize_crs("station", &params.station)?;
    let origin = non_empty(&params.origin)
        .map(|s| normalize_crs("origin", s))
        .transpose()?;
    let terminates_at = non_empty(&params.terminates_at)
        .map(|s| normalize_crs("terminates_at", s))
        .transpose()?;
    let operators = parse_operators(params.operator.as_deref())?;
    let parsed_stop_filters = super::stop_filters::parse_checked(
        super::stop_filters::StopFilterParams {
            calls_at: params.calls_at.as_deref(),
            via: params.via.as_deref(),
            avoid: params.avoid.as_deref(),
            avoid_stop: params.avoid_stop.as_deref(),
            previously_calls_at: params.previously_calls_at.as_deref(),
            previously_stops_at: params.previously_stops_at.as_deref(),
        },
        &[super::stop_filters::Anchor {
            crs: &station,
            role: "the station searched",
        }],
        super::stop_filters::Board::Arrivals,
    )?;
    // Here the filters look at the run before the station, from its origin.
    if let (Some(filters), Some(origin)) = (&parsed_stop_filters, origin.as_deref()) {
        filters.check_end("origin", origin)?;
    }
    let from_time = non_empty(&params.from)
        .map(|s| normalize_time("from", s))
        .transpose()?;
    let to_time = non_empty(&params.to)
        .map(|s| normalize_time("to", s))
        .transpose()?;
    let offsets = WindowDayOffsets {
        from: parse_day_offset(
            "from_day_offset",
            &params.from_day_offset,
            "from",
            from_time,
        )?,
        to: parse_day_offset("to_day_offset", &params.to_day_offset, "to", to_time)?,
        arrival_from: None,
        arrival_to: None,
    };
    let limit = normalize_limit(params.limit.as_deref())?;
    let after = non_empty(&params.after)
        .map(decode_arrivals_cursor)
        .transpose()?;

    let london_now = crate::routes::london_now();
    let today = london_now.date_naive();
    let service_date = requested_date(&app, params.date.as_deref(), today).await?;
    if let Some(codes) = &operators {
        check_known_operators(&app, codes, &[service_date]).await?;
    }
    let stop_filter = match &parsed_stop_filters {
        Some(filters) => Some(ArrivalsStopFilter::resolve(&app.database, filters, &station).await?),
        None => None,
    };
    let default_from = if service_date == today {
        london_now.time()
    } else {
        chrono::NaiveTime::MIN
    };
    let window = resolve_window(
        WindowTimes {
            from: from_time,
            to: to_time,
            arrival_from: None,
            arrival_to: None,
        },
        offsets,
        default_from,
    );

    let arrivals = station_arrivals(&app.database, service_date, &station)
        .await
        .map_err(internal_error)?;
    if arrivals.is_empty()
        && !queries::schedule_destination_departures_published_for(&app.database, service_date)
            .await
            .map_err(internal_error)?
    {
        return Err(unpublished(service_date, today));
    }
    let filter = ArrivalsFilter {
        from_minutes: window.from.minutes(),
        to_minutes: window.to.map(WindowBound::minutes),
        origin: origin.as_deref(),
        terminates_at: terminates_at.as_deref(),
        operators: operators.as_deref(),
        after: after.as_ref().map(|(minute, uid)| (*minute, uid.as_str())),
    };
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let (page, next_cursor) = match &stop_filter {
        Some(stop_filter) => filter
            .stop_filtered_page(&app.database, service_date, stop_filter, arrivals, limit)
            .await
            .map_err(internal_error)?,
        None => filter.page(arrivals, limit),
    };
    let rows = page
        .into_iter()
        .map(|arrival| {
            let (raw, fields) = arrival.render_parts(service_date);
            (raw, station.clone(), fields)
        })
        .collect();
    let results = render_rows(&app, service_date, rows, apply_arrival).await?;
    Ok(Json(TrainArrivalsPage {
        results: results
            .into_iter()
            .map(|(row, fields)| arrival_row(row, fields))
            .collect(),
        next_cursor,
        service_date,
        window: ArrivalsWindowJson {
            from: window.from.to_json(),
            to: window.to.map(WindowBound::to_json),
        },
        certainty: crate::routes::provisional::TimetableCertainty::for_date(service_date, today),
    }))
}

/// `GET /public/trains/by-headcode?headcode=[&date=]` -- the trains
/// running under one headcode on a date (2026-10-30), each as a row at its
/// origin: the search's row shape with `stationCrs` the train's first
/// calling point and `scheduled` its departure from there, in departure
/// order. At most 50.
///
/// The headcode's trains are read from `schedule_services` (one row per
/// train and date); a date it has not been published for yet answers an
/// empty list. `200 {"results", "serviceDate"}` plus the
/// timetable-certainty fields; `404` when nothing at all is published for
/// the date; `400` for a malformed headcode or date.
pub(super) async fn get_trains_by_headcode(
    State(app): State<App>,
    Query(params): Query<HeadcodeParams>,
) -> Result<Json<TrainHeadcodePage>, (StatusCode, String)> {
    let headcode = normalize_headcode(&params.headcode)?;
    let today = crate::routes::london_today();
    let service_date = requested_date(&app, params.date.as_deref(), today).await?;
    // Each train's first call that day: its origin departure.
    let mut rows: Vec<BoardRow> = sqlx::query_as(&format!(
        "SELECT DISTINCT ON (train_uid) {BOARD_COLUMNS} FROM schedule_destination_departures \
         WHERE service_date = $1 \
           AND train_uid IN ( \
               SELECT uid FROM schedule_services WHERE service_date = $1 AND headcode = $2 \
               LIMIT $3) \
         ORDER BY train_uid, day_offset, scheduled"
    ))
    .bind(service_date)
    .bind(&headcode)
    .bind(MAX_HEADCODE_MATCHES)
    .fetch_all(&app.database)
    .await
    .map_err(|err| internal_error(err.into()))?;
    if rows.is_empty()
        && !queries::schedule_destination_departures_published_for(&app.database, service_date)
            .await
            .map_err(internal_error)?
    {
        return Err(unpublished(service_date, today));
    }
    rows.sort_by_key(|row| {
        let (time, day_offset) = row.shown_departure();
        (minutes(time, day_offset), row.train_uid.clone())
    });
    let rendered = rows
        .into_iter()
        .map(|row| {
            let (_, day_offset) = row.shown_departure();
            let origin = row.origin_crs.clone();
            (row.raw_json(service_date), origin, day_offset)
        })
        .collect();
    let results = render_rows(&app, service_date, rendered, |row, day_offset: &i64| {
        row.day_offset = Some(*day_offset);
    })
    .await?;
    Ok(Json(TrainHeadcodePage {
        results: results.into_iter().map(|(row, _)| row).collect(),
        service_date,
        certainty: crate::routes::provisional::TimetableCertainty::for_date(service_date, today),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board_row(scheduled: &str, public: Option<&str>) -> BoardRow {
        let t = |s: &str| chrono::NaiveTime::parse_from_str(s, "%H:%M:%S").unwrap();
        BoardRow {
            train_uid: "C12345".to_string(),
            origin_crs: "RDG".to_string(),
            destination_crs: "PAD".to_string(),
            true_origin_crs: Some("OXF".to_string()),
            scheduled: t(scheduled),
            day_offset: 0,
            public_departure: public.map(t),
            calling_point_arrival: Some(t("09:58:30")),
            public_calling_point_arrival: Some(t("09:59:00")),
            destination_arrival: Some(t("10:30:00")),
            destination_arrival_day_offset: 0,
            public_destination_arrival: None,
            operator_atoc: Some("GW".to_string()),
            headcode: Some("1A11".to_string()),
        }
    }

    /// The `json!` arrival extras before they were typed, verbatim.
    fn legacy_extras(arrival: &Arrival) -> serde_json::Map<String, Value> {
        let mut extra = serde_json::Map::new();
        extra.insert(
            "arrival".to_string(),
            json!(arrival.time.format("%H:%M").to_string()),
        );
        extra.insert("arrivalDayOffset".to_string(), json!(arrival.day_offset));
        extra.insert("terminates".to_string(), json!(arrival.terminates));
        if arrival.terminates {
            extra.insert("scheduled".to_string(), Value::Null);
            extra.insert("publicDeparture".to_string(), Value::Null);
            extra.insert("workingDeparture".to_string(), Value::Null);
        } else {
            let (_, day_offset) = arrival.row.shown_departure();
            extra.insert("dayOffset".to_string(), json!(day_offset));
        }
        extra
    }

    /// An arrivals row (calling and terminating) and a headcode row serve
    /// what the `json!` rows did, before the lookups (which the search's
    /// own test covers).
    #[test]
    fn typed_board_rows_match_the_json_rows() {
        let date: chrono::NaiveDate = "2026-10-30".parse().unwrap();
        let names = HashMap::from([("PAD".to_string(), "London Paddington".to_string())]);
        for terminates in [false, true] {
            let row = board_row("10:00:00", Some("10:00:00"));
            let arrival = Arrival {
                time: chrono::NaiveTime::from_hms_opt(9, 59, 0).unwrap(),
                day_offset: 0,
                terminates,
                row,
            };
            let mut legacy = crate::render::legacy::calling_point_departure_json(
                &arrival.row.raw_json(date),
                "RDG",
                &names,
            );
            let extras = legacy_extras(&arrival);
            if let Some(object) = legacy.as_object_mut() {
                object.insert("serviceDate".to_string(), json!(date));
                object.extend(extras);
            }
            let (stored, fields) = arrival.render_parts(date);
            let mut search_row = TrainSearchRow::new(
                calling_point_departure_json(&stored, "RDG", &names),
                date.to_string(),
            );
            apply_arrival(&mut search_row, &fields);
            let mut new = serde_json::to_value(arrival_row(search_row, fields)).unwrap();
            // The lookups' fields, which the search's own test compares.
            for key in ["originName", "live", "serviceMode", "liveTracking"] {
                new.as_object_mut().unwrap().remove(key);
            }
            assert_eq!(new, legacy);
        }

        let row = board_row("23:59:30", Some("00:00:00"));
        let (_, day_offset) = row.shown_departure();
        let mut legacy =
            crate::render::legacy::calling_point_departure_json(&row.raw_json(date), "OXF", &names);
        if let Some(object) = legacy.as_object_mut() {
            object.insert("serviceDate".to_string(), json!(date));
            object.insert("dayOffset".to_string(), json!(day_offset));
        }
        let mut new = TrainSearchRow::new(
            calling_point_departure_json(&row.raw_json(date), "OXF", &names),
            date.to_string(),
        );
        new.day_offset = Some(day_offset);
        let mut new = serde_json::to_value(new).unwrap();
        for key in ["originName", "live", "serviceMode", "liveTracking"] {
            new.as_object_mut().unwrap().remove(key);
        }
        assert_eq!(new, legacy);
    }

    #[test]
    fn headcodes_are_a_digit_a_letter_and_two_digits() {
        assert_eq!(normalize_headcode(" 1s00 ").unwrap(), "1S00");
        for bad in ["", "1S0", "1S000", "S100", "1S0A", "W12345"] {
            assert!(normalize_headcode(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn arrivals_cursor_round_trips_and_rejects_junk() {
        let cursor = encode_arrivals_cursor(1530, "C12345");
        assert_eq!(
            decode_arrivals_cursor(&cursor).unwrap(),
            (1530, "C12345".to_string())
        );
        for bad in [
            "",
            "!!",
            &URL_SAFE_NO_PAD.encode("x|C1"),
            &URL_SAFE_NO_PAD.encode("12|"),
        ] {
            assert!(
                decode_arrivals_cursor(bad).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    fn row(scheduled: &str, day_offset: i16) -> BoardRow {
        let time = |t: &str| chrono::NaiveTime::parse_from_str(t, "%H:%M").unwrap();
        BoardRow {
            train_uid: "C1".to_string(),
            origin_crs: "RDG".to_string(),
            destination_crs: "PAD".to_string(),
            true_origin_crs: Some("BRI".to_string()),
            scheduled: time(scheduled),
            day_offset,
            public_departure: None,
            calling_point_arrival: None,
            public_calling_point_arrival: None,
            destination_arrival: None,
            destination_arrival_day_offset: 0,
            public_destination_arrival: None,
            operator_atoc: None,
            headcode: None,
        }
    }

    #[test]
    fn a_call_arrival_before_midnight_is_dated_the_day_before_its_departure() {
        let time = |t: &str| chrono::NaiveTime::parse_from_str(t, "%H:%M").unwrap();
        let mut call = row("00:02", 1);
        call.calling_point_arrival = Some(time("23:58"));
        assert_eq!(call.call_arrival(), Some((time("23:58"), 0)));
        call.public_calling_point_arrival = Some(time("00:00"));
        assert_eq!(call.call_arrival(), Some((time("00:00"), 1)));
        assert_eq!(row("09:00", 0).call_arrival(), None);
    }

    #[test]
    fn a_terminus_arrival_prefers_the_public_time() {
        let time = |t: &str| chrono::NaiveTime::parse_from_str(t, "%H:%M").unwrap();
        let mut terminus = row("22:00", 0);
        terminus.destination_arrival = Some(time("23:59"));
        assert_eq!(terminus.terminus_arrival(), Some((time("23:59"), 0)));
        terminus.public_destination_arrival = Some(time("00:01"));
        assert_eq!(terminus.terminus_arrival(), Some((time("00:01"), 1)));
    }
}
