//! `GET /public/stations/{crs}/board[?board=departures|arrivals]`: a
//! station's next departures (the default) or arrivals, the published
//! timetable with Darwin's live times and platforms laid over it (DS-638,
//! DS-765; docs/station-board.md has the design, the platform precedence,
//! the London termini policy and the licence assumption).
//!
//! Off by default: with `STATION_BOARD_ENABLED` unset or `false` the route
//! answers `404`.
//!
//! Three batched reads, whatever the board's size: the timetable rows
//! (`schedule_destination_departures`: for departures the search
//! `GET /public/trains/search` and `schedule-departures` use, for arrivals
//! the calls at the station over the calling-point index plus the trains
//! that end there over the primary key's `(service_date, destination_crs)`
//! prefix), Darwin's locations at the station for those trains
//! (`darwin_services_uid_ssd`, then `darwin_calls`' primary key) and the CIF
//! booked platforms (`schedule_calling_points_full`'s primary key), plus the
//! station's TIPLOCs and the names. The station's LDBWS board
//! (`station_samples`, a departures board) is read only for the departures
//! board, and only when some row has no Darwin platform and no Darwin word
//! that it is hidden. For the departures with no public platform of their own,
//! the inbound working's arrival platform is read in batches too
//! (`data::station_board::inbound_platforms`: the turnaround keys, the
//! inbound's Darwin calls and, when Darwin has none, TRUST's arrival), giving
//! a labelled `likely` platform.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::{Extension, Json};
use chrono::{NaiveDate, NaiveTime, Timelike};
use common::transit::TransitAttribution;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;

use crate::app::{App, Router};
use crate::data::station_board::{
    self, BoardCall, BoardKind, BoardPlatformBasis, BoardPlatformStatus, BoardStatus, BookedCall,
    LDBWS_MAX_AGE_MINUTES, LdbwsKey, Settings,
};
use crate::data::{journey_alternatives, queries, train_reasons};
use crate::render::{NATIONAL_RAIL_CREDIT, RDM_LICENCE, public_else_working};
use crate::routes::departures::board_depart_from;
use crate::routes::trains::boards::{BOARD_COLUMNS, BoardRow as ScheduleRow};
use crate::routes::trains::{
    check_known_operators, check_search_date, normalize_crs, parse_date, parse_operators,
};

/// The rows a board has when `limit` is not given.
const DEFAULT_LIMIT: u32 = 8;
/// The most rows a board may ask for; a larger `limit` is clamped.
const MAX_LIMIT: u32 = 30;
/// How far back from now the timetable is read, in minutes: a train booked a
/// little ago but running late is still on the board.
const LOOKBACK_MINUTES: i32 = 15;
/// Rows read beyond `limit`: the look-back's, and the ones that have left.
const EXTRA_ROWS: i64 = 40;
/// How far a call's public (GBTT) arrival may sit from its working one for
/// the arrivals read's index range to still reach it, in minutes: the same
/// hour `queries::PUBLIC_WORKING_SLACK_MINUTES` measured for departures.
const PUBLIC_ARRIVAL_SLACK_MINUTES: i32 = 60;

/// `Cache-Control` of a board: the edge may keep it 20 s, then serve it
/// stale for 30 s while it refreshes.
const CACHE_CONTROL: &str = "public, s-maxage=20, stale-while-revalidate=30";

pub fn router() -> Router {
    router_with(station_board::settings().clone())
}

/// The route with `settings` (tests pick the switch; [`router`] reads the
/// environment).
pub(crate) fn router_with(settings: Settings) -> Router {
    Router::new()
        .route(
            "/stations/{crs}/board",
            axum::routing::get(get_station_board),
        )
        .layer(Extension(Arc::new(settings)))
}

/// Query string. Unknown keys are ignored, as for `/departures`, so a
/// client's cache-buster keeps working.
#[derive(Debug, Deserialize)]
struct BoardParams {
    limit: Option<String>,
    date: Option<String>,
    operator: Option<String>,
    board: Option<String>,
}

/// One row of the board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "StationBoardDeparture")]
pub(crate) struct StationBoardDepartureJson {
    /// The CIF train UID, for `GET /Train/by-uid/{uid}/{serviceDate}`.
    uid: String,
    service_date: String,
    /// The booked time shown to the public, `HH:MM`, on day `dayOffset`
    /// (1 after the service date's midnight).
    scheduled: String,
    day_offset: u32,
    /// Darwin's actual or forecast time, `HH:MM`; `null` with no live data.
    expected: Option<String>,
    expected_day_offset: Option<u32>,
    destination_crs: Option<String>,
    destination_name: Option<String>,
    /// ATOC code.
    operator: Option<String>,
    operator_name: Option<String>,
    headcode: Option<String>,
    /// Never a platform Darwin hides from the public for this call. With
    /// `platformStatus` `likely` it is a prediction, not an announcement:
    /// show it labelled as such.
    platform: Option<String>,
    platform_status: BoardPlatformStatus,
    /// What a `likely` platform rests on (`inbound`: the working that forms
    /// this service arrives there; `booked`: the timetable's plan); `null`
    /// for every other status.
    platform_basis: Option<BoardPlatformBasis>,
    status: BoardStatus,
    delay_minutes: Option<u32>,
    is_cancelled: bool,
    cancel_reason: Option<String>,
}

/// One row of the arrivals board: a [`StationBoardDepartureJson`]'s fields,
/// with the origin, and `scheduled` / `expected` the arrival.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "StationBoardArrival")]
pub(crate) struct StationBoardArrivalJson {
    /// The CIF train UID, for `GET /Train/by-uid/{uid}/{serviceDate}`.
    uid: String,
    service_date: String,
    /// The booked ARRIVAL shown to the public, `HH:MM`, on day `dayOffset`
    /// (1 after the service date's midnight).
    scheduled: String,
    day_offset: u32,
    /// Darwin's actual or forecast arrival, `HH:MM`; `null` with no live
    /// data.
    expected: Option<String>,
    expected_day_offset: Option<u32>,
    /// Where the train started its journey.
    origin_crs: Option<String>,
    origin_name: Option<String>,
    /// Where the train ends its journey: this station's own code when it
    /// terminates here.
    destination_crs: Option<String>,
    destination_name: Option<String>,
    /// The train ends its journey at this station.
    terminates: bool,
    /// ATOC code.
    operator: Option<String>,
    operator_name: Option<String>,
    headcode: Option<String>,
    /// As on a departure: never a platform Darwin hides; `likely` is a
    /// prediction.
    platform: Option<String>,
    platform_status: BoardPlatformStatus,
    /// What a `likely` platform rests on; `null` for every other status.
    platform_basis: Option<BoardPlatformBasis>,
    status: BoardStatus,
    delay_minutes: Option<u32>,
    is_cancelled: bool,
    cancel_reason: Option<String>,
}

/// The body of the route.
#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "StationBoard")]
pub(crate) struct StationBoardResponse {
    crs: String,
    name: Option<String>,
    service_date: String,
    /// When this board was built, RFC 3339 UTC.
    generated_at: String,
    /// Which board this is: the `board` asked for.
    board: BoardKind,
    /// The departures board's rows; empty on the arrivals board.
    departures: Vec<StationBoardDepartureJson>,
    /// The arrivals board's rows; absent on the departures board.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    arrivals: Option<Vec<StationBoardArrivalJson>>,
    /// The credits the data needs; republish them with it.
    attribution: Vec<TransitAttribution>,
}

type Failure = (StatusCode, String);

fn not_found() -> Failure {
    (StatusCode::NOT_FOUND, "not found".to_string())
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used as a map_err callback, which passes the error by value"
)]
fn internal_error(err: anyhow::Error) -> Failure {
    if let Some(unavailable) = crate::unavailable::response_for(&err) {
        return unavailable;
    }
    tracing::error!(error = ?err, "station board query failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "query failed".to_string(),
    )
}

/// `limit`: 1 to [`MAX_LIMIT`], a larger one clamped; anything else is a
/// `400`.
fn parse_limit(raw: Option<&str>) -> Result<u32, Failure> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(DEFAULT_LIMIT);
    };
    match raw.parse::<u32>() {
        Ok(0) | Err(_) => Err((
            StatusCode::BAD_REQUEST,
            format!("limit must be a whole number from 1 to {MAX_LIMIT}"),
        )),
        Ok(limit) => Ok(limit.min(MAX_LIMIT)),
    }
}

/// `board`: `departures` (the default) or `arrivals`; anything else is a
/// `400`.
fn parse_board(raw: Option<&str>) -> Result<BoardKind, Failure> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(BoardKind::Departures);
    };
    BoardKind::parse(raw).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "board must be departures or arrivals".to_string(),
        )
    })
}

async fn get_station_board(
    State(app): State<App>,
    Extension(settings): Extension<Arc<Settings>>,
    Path(crs): Path<String>,
    Query(params): Query<BoardParams>,
) -> Result<(HeaderMap, Json<StationBoardResponse>), Failure> {
    if !settings.enabled {
        return Err(not_found());
    }
    let crs = normalize_crs("crs", &crs)?;
    let limit = parse_limit(params.limit.as_deref())?;
    let kind = parse_board(params.board.as_deref())?;
    let now = super::london_now();
    let today = now.date_naive();
    let service_date = match params.date.as_deref() {
        None => today,
        Some(raw) => {
            let date = parse_date(raw)?;
            check_search_date(&app, date, today).await?;
            date
        }
    };
    let operators = parse_operators(params.operator.as_deref())?;
    if let Some(codes) = &operators {
        check_known_operators(&app, codes, &[service_date]).await?;
    }
    let query = BoardQuery {
        crs: &crs,
        service_date,
        today,
        now,
        limit,
        operators: operators.as_deref(),
        kind,
    };
    let board = build_board(&app.database, &settings, &query)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                format!("no timetable data for station: {crs}"),
            )
        })?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    Ok((headers, Json(board)))
}

/// What one board is built for.
struct BoardQuery<'a> {
    crs: &'a str,
    service_date: NaiveDate,
    today: NaiveDate,
    now: chrono::DateTime<chrono::FixedOffset>,
    limit: u32,
    operators: Option<&'a [String]>,
    kind: BoardKind,
}

/// A timetable row, parsed.
struct BoardRow {
    uid: String,
    service_date: NaiveDate,
    /// The working time of the call's visit the board shows: the departure,
    /// or the arrival on the arrivals board.
    working: NaiveTime,
    /// The shown (public) time (the departure, or on the arrivals board the
    /// arrival), `HH:MM`, and its day offset.
    shown: String,
    day_offset: u32,
    /// Minutes from the service date's midnight to the shown time.
    shown_total: u32,
    destination_crs: String,
    /// The train's true origin (arrivals board only).
    origin_crs: Option<String>,
    /// The train ends here (arrivals board only).
    terminates: bool,
    operator: Option<String>,
    headcode: Option<String>,
}

fn parse_row(row: &Value) -> Option<BoardRow> {
    let text = |key: &str| row.get(key).and_then(Value::as_str);
    let uid = text("uid")?.to_string();
    let service_date = NaiveDate::parse_from_str(text("service_date")?, "%Y-%m-%d").ok()?;
    let working = NaiveTime::parse_from_str(text("scheduled")?, "%H:%M:%S").ok()?;
    let working_offset = row.get("day_offset").and_then(Value::as_u64).unwrap_or(0);
    let (shown, offset) = public_else_working(row, "public_departure", "scheduled", working_offset);
    let shown = shown?;
    let time = NaiveTime::parse_from_str(&shown, "%H:%M").ok()?;
    let day_offset = u32::try_from(offset).ok()?;
    Some(BoardRow {
        uid,
        service_date,
        working,
        shown_total: day_offset * 1440 + time.hour() * 60 + time.minute(),
        shown,
        day_offset,
        destination_crs: text("destination_crs")?.to_string(),
        origin_crs: None,
        terminates: false,
        operator: text("operator_atoc").map(str::to_string),
        headcode: text("headcode").map(str::to_string),
    })
}

/// Minutes from the board's service date's midnight to the row's shown
/// time, negative for a row of an earlier service date (its own clock runs
/// a day ahead), for ordering rows of two dates.
fn board_total(row: &BoardRow, board_date: NaiveDate) -> i64 {
    i64::from(row.shown_total) + (row.service_date - board_date).num_days() * 1440
}

fn minute_of(time: NaiveTime) -> u32 {
    time.hour() * 60 + time.minute()
}

/// The shown time of `total` minutes since the service date's midnight:
/// `HH:MM` and the day offset.
fn clock(total: u32) -> (String, u32) {
    let minute = total % 1440;
    (
        format!("{:02}:{:02}", minute / 60, minute % 60),
        total / 1440,
    )
}

/// Whether the LDBWS board can name this row's platform: its calendar day
/// is today, which is the only day a station's live board speaks for.
fn is_today(row: &BoardRow, today: NaiveDate) -> bool {
    row.service_date + chrono::Duration::days(i64::from(row.day_offset)) == today
}

/// What each timetable row's live data says, by position in the rows.
struct Live<'a> {
    darwin: Option<&'a BoardCall>,
    booked: Option<&'a BookedCall>,
}

/// Whether Darwin has no public platform for the call: no call, a blank
/// one or a hidden one, and the call is not cancelled.
fn has_no_public_platform(darwin: Option<&BoardCall>) -> bool {
    darwin.is_none_or(|call| {
        let view = call.platform_view();
        !view.cancelled && (view.hidden || view.platform.is_none_or(|p| p.trim().is_empty()))
    })
}

/// The LDBWS platform for the rows that can still use one, by position
/// (module docs: only when some row has neither a Darwin platform nor a
/// Darwin word that it is hidden, and only from a fresh board).
async fn ldbws_platforms(
    pool: &PgPool,
    query: &BoardQuery<'_>,
    rows: &[BoardRow],
    live: &[Live<'_>],
) -> anyhow::Result<HashMap<usize, String>> {
    let needs = |live: &Live<'_>| match live.darwin {
        None => true,
        Some(call) => {
            let view = call.platform_view();
            !(view.cancelled || view.hidden || view.platform.is_some_and(|p| !p.trim().is_empty()))
        }
    };
    let candidates: Vec<usize> = (0..rows.len())
        .filter(|&i| is_today(&rows[i], query.today))
        .collect();
    if !candidates.iter().any(|&i| needs(&live[i])) {
        return Ok(HashMap::new());
    }
    let Some(sample) = queries::latest_station_sample(pool, query.crs).await? else {
        return Ok(HashMap::new());
    };
    let age = query.now.to_utc() - sample.polled_at;
    if age > chrono::Duration::minutes(LDBWS_MAX_AGE_MINUTES) {
        return Ok(HashMap::new());
    }
    let keys: Vec<LdbwsKey<'_>> = candidates
        .iter()
        .map(|&i| LdbwsKey {
            headcode: rows[i].headcode.as_deref(),
            shown: &rows[i].shown,
            destination_crs: &rows[i].destination_crs,
        })
        .collect();
    Ok(station_board::match_ldbws(&sample.departures, &keys)
        .into_iter()
        .map(|(key_index, platform)| (candidates[key_index], platform))
        .filter(|(i, _)| needs(&live[*i]))
        .collect())
}

/// The first minute of the board still shown, and the first read: the
/// board's own `depart_from` (train-day minutes of the service date) and
/// that less the look-back for a late train.
fn board_window(query: &BoardQuery<'_>) -> (i32, i32) {
    let threshold = board_depart_from(query.service_date, query.today, query.now.time());
    let lookback = if query.service_date <= query.today {
        LOOKBACK_MINUTES
    } else {
        0
    };
    (threshold, (threshold - lookback).max(0))
}

/// The departures board's timetable rows, or `None` when the station has no
/// timetable data for the day.
async fn departure_rows(
    pool: &PgPool,
    query: &BoardQuery<'_>,
    station: &str,
) -> anyhow::Result<Option<Vec<BoardRow>>> {
    let (_, read_from) = board_window(query);
    let page = queries::search_schedule_calling_point_departures(
        pool,
        &queries::CallingPointSearch {
            station_crs: station,
            service_date: query.service_date,
            depart_from: read_from,
            depart_to: None,
            true_origin_crs: None,
            terminates_at: None,
            stops_at: None,
            arrive_from: None,
            arrive_to: None,
            operators: query.operators,
            stop_filter: None,
            // Yesterday's trains still running after its midnight, for a
            // board read just after London midnight.
            across_midnight: query.service_date == query.today,
            after: None,
            limit: i64::from(query.limit) + EXTRA_ROWS,
        },
    )
    .await?;
    let Some(page) = page else {
        return Ok(None);
    };
    let rows: Vec<BoardRow> = page.departures.iter().filter_map(parse_row).collect();
    if rows.is_empty()
        && !queries::schedule_destination_departures_has_station(pool, station, query.service_date)
            .await?
    {
        return Ok(None);
    }
    Ok(Some(rows))
}

/// The calls at a station with an arrival, for one service date: the day-0
/// band over `schedule_destination_departures_calling_point_idx`
/// (`service_date, origin_crs, scheduled`), narrowed to the working times
/// that can still hold an arrival on the board (`$3`; a call's arrival is
/// never after its own departure, and its public time is within an hour of
/// the working one, so `scheduled >= from - 1h` loses none), and the rows
/// past the date's midnight. `$4` turns the day-0 band off (the previous
/// date reads only its after-midnight rows). A pick-up-only call
/// (`can_alight = false`) is not an arrival, and the train's own origin
/// row has no arrival at all.
static ARRIVAL_CALLS_SQL: LazyLock<String> = LazyLock::new(|| {
    let call = |band: &str| {
        format!(
            "SELECT {BOARD_COLUMNS} FROM schedule_destination_departures \
             WHERE service_date = $1 AND origin_crs = $2 \
               AND can_alight IS NOT FALSE \
               AND (calling_point_arrival IS NOT NULL OR public_calling_point_arrival IS NOT NULL) \
               AND {band}"
        )
    };
    format!(
        "({}) UNION ALL ({})",
        call("$4::bool AND day_offset = 0 AND scheduled >= $3::time"),
        call("day_offset >= 1")
    )
});

/// The trains that end at a station, one row each (the first call read), for
/// one service date: the primary key's `(service_date, destination_crs)`
/// prefix. The terminus has no row of its own (it has no departure), so this
/// is the only way to it. NOTE the cost: every departure-bearing call of
/// every train ending here that day is read (about 10,000 rows for the
/// busiest destination), because no index leads with the arrival time
/// (docs/station-board.md, "How the arrivals board is read").
static ARRIVAL_TERMINI_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT DISTINCT ON (train_uid) {BOARD_COLUMNS} FROM schedule_destination_departures \
         WHERE service_date = $1 AND destination_crs = $2 \
           AND (destination_arrival IS NOT NULL OR public_destination_arrival IS NOT NULL) \
         ORDER BY train_uid, day_offset, scheduled"
    )
});

/// A timetable row as an arrivals board row: its arrival as shown (public,
/// else working) and the working arrival Darwin's and the CIF's call is
/// found by. `None` when the row has no arrival.
fn arrival_row(raw: &ScheduleRow, service_date: NaiveDate, terminates: bool) -> Option<BoardRow> {
    let ((time, offset), working) = if terminates {
        (
            raw.terminus_arrival()?,
            raw.destination_arrival.or(raw.public_destination_arrival)?,
        )
    } else {
        (
            raw.call_arrival()?,
            raw.calling_point_arrival
                .or(raw.public_calling_point_arrival)?,
        )
    };
    let day_offset = u32::try_from(offset).ok()?;
    Some(BoardRow {
        uid: raw.train_uid.clone(),
        service_date,
        working,
        shown: time.format("%H:%M").to_string(),
        day_offset,
        shown_total: day_offset * 1440 + minute_of(time),
        destination_crs: raw.destination_crs.clone(),
        origin_crs: raw.true_origin_crs.clone(),
        terminates,
        operator: raw.operator_atoc.clone(),
        headcode: raw.headcode.clone(),
    })
}

/// The arrivals board's timetable rows, or `None` when the station has no
/// timetable data for the day: calls and termini of the service date and,
/// for today, of the previous date's trains still running after its
/// midnight, cut to the board's window and operators, in the order of the
/// arrival shown, at most `limit + EXTRA_ROWS`.
async fn arrival_rows(
    pool: &PgPool,
    query: &BoardQuery<'_>,
    station: &str,
) -> anyhow::Result<Option<Vec<BoardRow>>> {
    let (_, read_from) = board_window(query);
    // The earliest working departure a row on the board can have: an hour
    // under the first minute read (a public time may be an hour off the
    // working one).
    let first_minute =
        u32::try_from((read_from - PUBLIC_ARRIVAL_SLACK_MINUTES).max(0)).unwrap_or(0);
    // A board of an earlier service date starts after its day-0 band ends.
    let first_working = NaiveTime::from_hms_opt(first_minute / 60, first_minute % 60, 0);
    let day_zero = first_working.is_some() && first_minute < 1440;
    let first_working = first_working.unwrap_or(NaiveTime::MIN);
    let mut dates = vec![(query.service_date, day_zero)];
    if query.service_date == query.today
        && let Some(previous) = query.service_date.pred_opt()
    {
        dates.push((previous, false));
    }
    let mut rows: Vec<BoardRow> = Vec::new();
    let mut any_read = false;
    for (date, same_day) in dates {
        let calls: Vec<ScheduleRow> = sqlx::query_as(ARRIVAL_CALLS_SQL.as_str())
            .bind(date)
            .bind(station)
            .bind(first_working)
            .bind(same_day)
            .fetch_all(pool)
            .await?;
        let termini: Vec<ScheduleRow> = sqlx::query_as(ARRIVAL_TERMINI_SQL.as_str())
            .bind(date)
            .bind(station)
            .fetch_all(pool)
            .await?;
        any_read |= date == query.service_date && (!calls.is_empty() || !termini.is_empty());
        let keep = |row: &BoardRow| {
            board_total(row, query.service_date) >= i64::from(read_from)
                && query.operators.is_none_or(|codes| {
                    row.operator
                        .as_ref()
                        .is_some_and(|code| codes.contains(code))
                })
        };
        rows.extend(
            calls
                .iter()
                .filter_map(|raw| arrival_row(raw, date, false))
                .filter(|row| keep(row)),
        );
        rows.extend(
            termini
                .iter()
                .filter_map(|raw| arrival_row(raw, date, true))
                .filter(|row| keep(row)),
        );
    }
    if !any_read && !has_arrival_timetable(pool, station, query.service_date).await? {
        return Ok(None);
    }
    rows.sort_by_key(|row| (board_total(row, query.service_date), row.working));
    rows.truncate(usize::try_from(i64::from(query.limit) + EXTRA_ROWS).unwrap_or(usize::MAX));
    Ok(Some(rows))
}

/// Whether the station is on the day's timetable at all, as a calling point
/// or as a terminus (the `404` of an arrivals board with nothing to show).
async fn has_arrival_timetable(
    pool: &PgPool,
    station: &str,
    service_date: NaiveDate,
) -> anyhow::Result<bool> {
    if queries::schedule_destination_departures_has_station(pool, station, service_date).await? {
        return Ok(true);
    }
    let probe: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM schedule_destination_departures \
         WHERE service_date = $1 AND destination_crs = $2 LIMIT 1",
    )
    .bind(service_date)
    .bind(station)
    .fetch_optional(pool)
    .await?;
    Ok(probe.is_some())
}

/// The board for `query`, or `None` when the station has no timetable data
/// for the day (a `404`).
#[expect(
    clippy::too_many_lines,
    reason = "one linear pipeline: read, match, resolve, cut, name"
)]
async fn build_board(
    pool: &PgPool,
    settings: &Settings,
    query: &BoardQuery<'_>,
) -> anyhow::Result<Option<StationBoardResponse>> {
    let station = queries::normalize_code(query.crs);
    let kind = query.kind;
    let rows = match kind {
        BoardKind::Departures => departure_rows(pool, query, &station).await?,
        BoardKind::Arrivals => arrival_rows(pool, query, &station).await?,
    };
    let Some(mut rows) = rows else {
        return Ok(None);
    };
    rows.sort_by_key(|row| (board_total(row, query.service_date), row.working));

    let mut trains: Vec<(String, NaiveDate)> = rows
        .iter()
        .map(|row| (row.uid.clone(), row.service_date))
        .collect();
    trains.sort_unstable();
    trains.dedup();
    let tiplocs = if trains.is_empty() {
        Vec::new()
    } else {
        station_board::tiplocs_for_crs(pool, &station).await?
    };
    let darwin = station_board::darwin_calls_at(pool, &trains, &tiplocs).await?;
    let booked = station_board::booked_calls_at(pool, &trains, &tiplocs).await?;
    let live: Vec<Live<'_>> = rows
        .iter()
        .map(|row| {
            let key = (row.uid.clone(), row.service_date);
            Live {
                darwin: darwin.get(&key).and_then(|calls| {
                    station_board::pick_visit(calls, row.working, |c| {
                        station_board::darwin_minute(match kind {
                            BoardKind::Departures => &c.call.wtd,
                            BoardKind::Arrivals => &c.call.wta,
                        })
                    })
                }),
                booked: booked.get(&key).and_then(|calls| {
                    station_board::pick_visit(calls, row.working, |c| {
                        match kind {
                            BoardKind::Departures => c.booked_departure,
                            BoardKind::Arrivals => c.booked_arrival,
                        }
                        .map(minute_of)
                    })
                }),
            }
        })
        .collect();
    // The station's own LDBWS board is a departures board: nothing for
    // arrivals.
    let from_board = match kind {
        BoardKind::Departures => ldbws_platforms(pool, query, &rows, &live).await?,
        BoardKind::Arrivals => HashMap::new(),
    };

    // The rows still on the board, cut to `limit`: only these ask for a
    // prediction.
    let mut on_board: Vec<usize> = Vec::new();
    for (index, (row, live)) in rows.iter().zip(&live).enumerate() {
        let times = live.darwin.map(|call| call.times(kind));
        let status = station_board::live_status(times, row.shown_total);
        // Gone: the train has left (arrived), or it is not due any more.
        let due = status.expected_total.unwrap_or(row.shown_total);
        let gone = times.is_some_and(|t| t.gone);
        // On the row's own service date's clock: a train of the day before
        // counts its minutes from that day's midnight.
        let from = board_depart_from(row.service_date, query.today, query.now.time());
        let past = u32::try_from(from).is_ok_and(|from| due < from);
        if gone || past {
            continue;
        }
        on_board.push(index);
        if on_board.len() >= usize::try_from(query.limit).unwrap_or(usize::MAX) {
            break;
        }
    }
    // Predictions from the inbound working, for the departures with no
    // platform of their own (a hidden one included). An arrival's platform
    // is where it arrives, so the arrivals board has none.
    let needs_prediction: Vec<(String, NaiveDate)> = match kind {
        BoardKind::Departures => on_board
            .iter()
            .filter(|&&i| !from_board.contains_key(&i) && has_no_public_platform(live[i].darwin))
            .map(|&i| (rows[i].uid.clone(), rows[i].service_date))
            .collect(),
        BoardKind::Arrivals => Vec::new(),
    };
    let inbound =
        station_board::inbound_platforms(pool, &station, &needs_prediction, &tiplocs).await?;

    let no_booked = settings.no_booked_platform(&station);
    let mut kept: Vec<(usize, StationBoardDepartureJson)> = Vec::new();
    for index in on_board {
        let (row, live) = (&rows[index], &live[index]);
        let times = live.darwin.map(|call| call.times(kind));
        let status = station_board::live_status(times, row.shown_total);
        let resolved = station_board::resolve_platform(
            live.darwin.map(BoardCall::platform_view),
            from_board.get(&index).map(String::as_str),
            inbound
                .get(&(row.uid.clone(), row.service_date))
                .map(String::as_str),
            live.booked.and_then(|c| c.platform.as_deref()),
            no_booked,
        );
        let expected = status.expected_total.map(clock);
        let is_cancelled = status.status == BoardStatus::Cancelled;
        kept.push((
            index,
            StationBoardDepartureJson {
                uid: row.uid.clone(),
                service_date: row.service_date.to_string(),
                scheduled: row.shown.clone(),
                day_offset: row.day_offset,
                expected_day_offset: expected.as_ref().map(|(_, day)| *day),
                expected: expected.map(|(text, _)| text),
                destination_crs: Some(row.destination_crs.clone())
                    .filter(|crs| queries::is_bookable_crs(crs)),
                destination_name: None,
                operator: row.operator.clone(),
                operator_name: None,
                headcode: row.headcode.clone(),
                platform: resolved.platform,
                platform_status: resolved.status,
                platform_basis: resolved.basis,
                status: status.status,
                delay_minutes: status.delay_minutes,
                is_cancelled,
                cancel_reason: live
                    .darwin
                    .filter(|_| is_cancelled)
                    .and_then(|call| call.cancel_reason.as_deref())
                    .and_then(train_reasons::reason_text)
                    .map(str::to_string),
            },
        ));
    }

    let mut codes: Vec<String> = vec![station.clone()];
    codes.extend(kept.iter().map(|(i, _)| rows[*i].destination_crs.clone()));
    codes.extend(kept.iter().filter_map(|(i, _)| rows[*i].origin_crs.clone()));
    let names = queries::station_names_for_crs_batch(pool, &codes).await?;
    let mut operators: Vec<String> = kept
        .iter()
        .filter_map(|(_, d)| d.operator.clone())
        .collect();
    operators.sort_unstable();
    operators.dedup();
    let operator_names = journey_alternatives::operator_names(pool, &operators).await;
    let finished: Vec<(usize, StationBoardDepartureJson)> = kept
        .into_iter()
        .map(|(index, mut departure)| {
            departure.destination_name = names
                .get(&rows[index].destination_crs.to_uppercase())
                .cloned();
            departure.operator_name = departure
                .operator
                .as_ref()
                .and_then(|code| operator_names.get(code))
                .cloned();
            (index, departure)
        })
        .collect();
    let (departures, arrivals) = match kind {
        BoardKind::Departures => (finished.into_iter().map(|(_, row)| row).collect(), None),
        BoardKind::Arrivals => (
            Vec::new(),
            Some(
                finished
                    .into_iter()
                    .map(|(index, row)| {
                        let origin = rows[index].origin_crs.clone();
                        StationBoardArrivalJson::new(
                            row,
                            origin.clone().filter(|crs| queries::is_bookable_crs(crs)),
                            origin.and_then(|crs| names.get(&crs.to_uppercase()).cloned()),
                            rows[index].terminates,
                        )
                    })
                    .collect(),
            ),
        ),
    };
    Ok(Some(StationBoardResponse {
        crs: station.clone(),
        name: names.get(&station).cloned(),
        service_date: query.service_date.to_string(),
        generated_at: query
            .now
            .to_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        board: kind,
        departures,
        arrivals,
        attribution: vec![TransitAttribution {
            licence: RDM_LICENCE.to_string(),
            text: NATIONAL_RAIL_CREDIT.to_string(),
        }],
    }))
}

impl StationBoardArrivalJson {
    /// An arrivals row from the fields it shares with a departures row.
    fn new(
        row: StationBoardDepartureJson,
        origin_crs: Option<String>,
        origin_name: Option<String>,
        terminates: bool,
    ) -> Self {
        Self {
            uid: row.uid,
            service_date: row.service_date,
            scheduled: row.scheduled,
            day_offset: row.day_offset,
            expected: row.expected,
            expected_day_offset: row.expected_day_offset,
            origin_crs,
            origin_name,
            destination_crs: row.destination_crs,
            destination_name: row.destination_name,
            terminates,
            operator: row.operator,
            operator_name: row.operator_name,
            headcode: row.headcode,
            platform: row.platform,
            platform_status: row.platform_status,
            platform_basis: row.platform_basis,
            status: row.status,
            delay_minutes: row.delay_minutes,
            is_cancelled: row.is_cancelled,
            cancel_reason: row.cancel_reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_limit_defaults_clamps_and_rejects_nonsense() {
        assert_eq!(parse_limit(None).unwrap(), DEFAULT_LIMIT);
        assert_eq!(parse_limit(Some(" ")).unwrap(), DEFAULT_LIMIT);
        assert_eq!(parse_limit(Some("1")).unwrap(), 1);
        assert_eq!(parse_limit(Some("30")).unwrap(), 30);
        assert_eq!(parse_limit(Some("500")).unwrap(), MAX_LIMIT);
        for bad in ["0", "-1", "ten", "1.5"] {
            assert_eq!(
                parse_limit(Some(bad)).unwrap_err().0,
                StatusCode::BAD_REQUEST
            );
        }
    }

    #[test]
    fn rows_of_two_service_dates_order_on_one_clock() {
        let today = NaiveDate::from_ymd_opt(2099, 11, 5).unwrap();
        let mut yesterdays = parse_row(&row(Some("00:30:00"), 0)).unwrap();
        yesterdays.day_offset = 1;
        yesterdays.shown_total = 1470;
        yesterdays.service_date = today.pred_opt().unwrap();
        let mut todays = parse_row(&row(Some("00:40:00"), 0)).unwrap();
        todays.shown_total = 40;
        todays.service_date = today;
        assert_eq!(board_total(&yesterdays, today), 30);
        assert_eq!(board_total(&todays, today), 40);
    }

    #[test]
    fn a_clock_time_past_midnight_carries_a_day() {
        assert_eq!(clock(605), ("10:05".to_string(), 0));
        assert_eq!(clock(1440 + 34), ("00:34".to_string(), 1));
    }

    fn row(public: Option<&str>, day_offset: u64) -> Value {
        json!({
            "uid": "A00001",
            "service_date": "2099-11-04",
            "scheduled": "23:59:30",
            "public_departure": public,
            "day_offset": day_offset,
            "destination_crs": "BRI",
            "operator_atoc": "GW",
            "headcode": "1A23",
        })
    }

    #[test]
    fn a_row_is_shown_at_its_public_time_with_that_times_day() {
        let parsed = parse_row(&row(Some("00:00:00"), 0)).unwrap();
        assert_eq!(parsed.shown, "00:00");
        assert_eq!(
            parsed.day_offset, 1,
            "23:59H working, 00:00 public: next day"
        );
        assert_eq!(parsed.shown_total, 1440);
        let working = parse_row(&row(None, 0)).unwrap();
        assert_eq!(working.shown, "23:59");
        assert_eq!(working.shown_total, 23 * 60 + 59);
        assert_eq!(working.headcode.as_deref(), Some("1A23"));
    }

    #[test]
    fn a_row_without_a_uid_or_time_is_skipped() {
        let mut no_uid = row(None, 0);
        no_uid.as_object_mut().unwrap().remove("uid");
        assert!(parse_row(&no_uid).is_none());
        let mut no_time = row(None, 0);
        no_time.as_object_mut().unwrap().remove("scheduled");
        assert!(parse_row(&no_time).is_none());
    }

    #[test]
    fn a_board_row_is_for_today_by_its_calendar_day() {
        let today = NaiveDate::from_ymd_opt(2099, 11, 5).unwrap();
        let mut after_midnight = parse_row(&row(Some("00:30:00"), 0)).unwrap();
        after_midnight.day_offset = 1;
        after_midnight.service_date = today.pred_opt().unwrap();
        assert!(is_today(&after_midnight, today));
        after_midnight.service_date = today;
        assert!(!is_today(&after_midnight, today), "that is tomorrow");
    }

    fn schedule_row(
        uid: &str,
        origin: Option<&str>,
        destination: &str,
        working_arrival: Option<&str>,
        public_arrival: Option<&str>,
    ) -> ScheduleRow {
        let at = |raw: &str| NaiveTime::parse_from_str(raw, "%H:%M:%S").unwrap();
        ScheduleRow {
            train_uid: uid.to_string(),
            origin_crs: "ZBA".to_string(),
            destination_crs: destination.to_string(),
            true_origin_crs: origin.map(str::to_string),
            scheduled: at("10:06:00"),
            day_offset: 0,
            public_departure: Some(at("10:06:00")),
            calling_point_arrival: working_arrival.map(at),
            public_calling_point_arrival: public_arrival.map(at),
            destination_arrival: working_arrival.map(at),
            destination_arrival_day_offset: 0,
            public_destination_arrival: public_arrival.map(at),
            operator_atoc: Some("ZQ".to_string()),
            headcode: Some("1A23".to_string()),
        }
    }

    #[test]
    fn the_board_parameter_defaults_to_departures_and_rejects_anything_else() {
        assert_eq!(parse_board(None).unwrap(), BoardKind::Departures);
        assert_eq!(parse_board(Some(" ")).unwrap(), BoardKind::Departures);
        assert_eq!(
            parse_board(Some("departures")).unwrap(),
            BoardKind::Departures
        );
        assert_eq!(parse_board(Some("Arrivals")).unwrap(), BoardKind::Arrivals);
        for bad in ["bogus", "arrival", "both", "1"] {
            assert_eq!(
                parse_board(Some(bad)).unwrap_err().0,
                StatusCode::BAD_REQUEST,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_call_is_an_arrival_at_its_public_arrival_and_keeps_its_origin() {
        let day = NaiveDate::from_ymd_opt(2099, 11, 4).unwrap();
        let raw = schedule_row(
            "A00001",
            Some("ZQO"),
            "ZQT",
            Some("10:04:30"),
            Some("10:04:00"),
        );
        let row = arrival_row(&raw, day, false).unwrap();
        assert_eq!(row.shown, "10:04");
        assert_eq!(row.shown_total, 10 * 60 + 4);
        assert_eq!(row.working, NaiveTime::from_hms_opt(10, 4, 30).unwrap());
        assert_eq!(row.origin_crs.as_deref(), Some("ZQO"));
        assert_eq!(row.destination_crs, "ZQT");
        assert!(!row.terminates);
    }

    #[test]
    fn a_service_that_only_starts_here_is_not_an_arrival() {
        // The train's own origin row: a departure, no arrival of either kind.
        let day = NaiveDate::from_ymd_opt(2099, 11, 4).unwrap();
        let raw = schedule_row("A00002", Some("ZBA"), "ZQT", None, None);
        assert!(arrival_row(&raw, day, false).is_none());
    }

    #[test]
    fn a_terminating_service_is_an_arrival_at_its_destination_arrival() {
        let day = NaiveDate::from_ymd_opt(2099, 11, 4).unwrap();
        let mut raw = schedule_row("A00003", Some("ZQO"), "ZBA", Some("23:58:00"), None);
        raw.destination_arrival_day_offset = 0;
        let row = arrival_row(&raw, day, true).unwrap();
        assert!(row.terminates);
        assert_eq!(row.destination_crs, "ZBA");
        assert_eq!(row.shown_total, 23 * 60 + 58);
        // Arriving after midnight carries the day.
        raw.destination_arrival = NaiveTime::from_hms_opt(0, 20, 0);
        raw.destination_arrival_day_offset = 1;
        raw.public_destination_arrival = None;
        let late = arrival_row(&raw, day, true).unwrap();
        assert_eq!(late.day_offset, 1);
        assert_eq!(late.shown, "00:20");
        assert_eq!(late.shown_total, 1440 + 20);
    }

    fn call(forecast: Option<&str>, actual: Option<&str>) -> BoardCall {
        BoardCall {
            uid: "A00001".to_string(),
            ssd: NaiveDate::from_ymd_opt(2099, 11, 4).unwrap(),
            cancel_reason: None,
            call: ds_store::darwin::DarwinCall {
                tiploc: "ZBATIP".to_string(),
                wta: "10:04".to_string(),
                wtd: "10:06".to_string(),
                wtp: String::new(),
                planned_platform: None,
                is_cancelled: false,
                platform: Some("4".to_string()),
                platform_confirmed: true,
                platform_suppressed: false,
                platform_cis_suppressed: false,
                platform_basis: None,
                arrival_et: forecast.map(str::to_string),
                departure_et: Some("11:11".to_string()),
                updated_at: chrono::Utc::now(),
            },
            departure_at: Some("11:12".to_string()),
            departure_delayed: true,
            arrival_at: actual.map(str::to_string),
            arrival_delayed: false,
            is_suppressed: false,
        }
    }

    #[test]
    fn the_arrivals_board_reads_the_arrival_fields_not_the_departure_ones() {
        // The departure fields say 11:11 / 11:12, delayed unknown: ignored.
        let forecast = call(Some("10:09"), None);
        let times = forecast.times(BoardKind::Arrivals);
        assert_eq!(times.expected_minute, Some(10 * 60 + 9));
        assert!(!times.gone);
        assert!(!times.delayed_unknown);
        let status = station_board::live_status(Some(times), 10 * 60 + 4);
        assert_eq!(status.status, BoardStatus::Delayed);
        assert_eq!(status.delay_minutes, Some(5));
        // The same call's departures view is the departure's.
        let departing = forecast.times(BoardKind::Departures);
        assert_eq!(departing.expected_minute, Some(11 * 60 + 12));
        assert!(departing.gone);
    }

    #[test]
    fn an_actual_arrival_is_gone_and_wins_over_the_forecast() {
        let arrived = call(Some("10:09"), Some("10:03")).times(BoardKind::Arrivals);
        assert!(arrived.gone);
        assert_eq!(arrived.expected_minute, Some(10 * 60 + 3));
        let status = station_board::live_status(Some(arrived), 10 * 60 + 4);
        assert_eq!(status.status, BoardStatus::OnTime);
        // No arrival data at all: booked, never onTime.
        let silent = call(None, None).times(BoardKind::Arrivals);
        assert_eq!(
            station_board::live_status(Some(silent), 604).status,
            BoardStatus::Booked
        );
        // Darwin's "delayed, time unknown" for the arrival.
        let mut unknown = call(None, None);
        unknown.arrival_delayed = true;
        assert_eq!(
            station_board::live_status(Some(unknown.times(BoardKind::Arrivals)), 604).status,
            BoardStatus::Delayed
        );
    }

    #[test]
    fn a_cancelled_call_is_cancelled_on_the_arrivals_board() {
        let mut cancelled = call(Some("10:04"), None);
        cancelled.call.is_cancelled = true;
        let status = station_board::live_status(Some(cancelled.times(BoardKind::Arrivals)), 604);
        assert_eq!(status.status, BoardStatus::Cancelled);
        let resolved = station_board::resolve_platform(
            Some(cancelled.platform_view()),
            None,
            None,
            Some("1"),
            false,
        );
        assert_eq!(
            (resolved.platform, resolved.status),
            (None, BoardPlatformStatus::None)
        );
    }

    #[test]
    fn the_arrivals_platform_follows_the_departures_precedence() {
        let resolve = |darwin: Option<&BoardCall>, booked, listed| {
            let resolved = station_board::resolve_platform(
                darwin.map(BoardCall::platform_view),
                None,
                None,
                booked,
                listed,
            );
            (resolved.platform, resolved.status, resolved.basis)
        };
        // Darwin's public platform first.
        let live = call(Some("10:04"), None);
        assert_eq!(
            resolve(Some(&live), Some("1"), true),
            (Some("4".to_string()), BoardPlatformStatus::Confirmed, None)
        );
        // Hidden: the timetable's platform as a prediction, never Darwin's.
        let mut hidden = call(Some("10:04"), None);
        hidden.call.platform_suppressed = true;
        assert_eq!(
            resolve(Some(&hidden), Some("1"), false),
            (
                Some("1".to_string()),
                BoardPlatformStatus::Likely,
                Some(BoardPlatformBasis::Booked)
            )
        );
        assert_eq!(
            resolve(Some(&hidden), None, false),
            (None, BoardPlatformStatus::ToBeAnnounced, None)
        );
        // No Darwin: booked, or a prediction at a listed station.
        assert_eq!(
            resolve(None, Some("1"), false),
            (Some("1".to_string()), BoardPlatformStatus::Booked, None)
        );
        assert_eq!(
            resolve(None, Some("1"), true),
            (
                Some("1".to_string()),
                BoardPlatformStatus::Likely,
                Some(BoardPlatformBasis::Booked)
            )
        );
    }
}

/// The route against real rows. `#[sqlx::test]` modules run in the
/// superuser pass only (`SQLX_TEST_SKIPS` in scripts/db-tests.py).
#[cfg(test)]
mod db_tests {
    use axum::body::Body;
    use axum::http::Request;
    use chrono::{TimeZone, Utc};
    use serde_json::Value;
    use sqlx::PgPool;
    use tower::ServiceExt;

    use super::*;
    use crate::data::station_board::{DARWIN_SQL, TIPLOCS_SQL};

    const DATE: &str = "2099-11-04";
    const CRS: &str = "ZBA";
    const TIPLOC: &str = "ZBATIP";

    fn date(raw: &str) -> NaiveDate {
        NaiveDate::parse_from_str(raw, "%Y-%m-%d").unwrap()
    }

    fn time(raw: &str) -> NaiveTime {
        NaiveTime::parse_from_str(raw, "%H:%M:%S").unwrap()
    }

    /// 10:00 London on 2099-11-04 (GMT).
    fn ten_oclock() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2099, 11, 4, 10, 0, 0).unwrap()
    }

    fn enabled(no_booked: &[&str]) -> Settings {
        Settings {
            enabled: true,
            no_booked_platform_crs: no_booked.iter().map(|c| (*c).to_string()).collect(),
        }
    }

    fn router(pool: PgPool, settings: Settings) -> axum::Router {
        router_with(settings).with_state(crate::test_support::inert_app(pool))
    }

    async fn get(router: &axum::Router, uri: &str) -> (StatusCode, HeaderMap, String) {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn board(router: &axum::Router, uri: &str) -> (Value, String) {
        let (status, _, body) = get(router, uri).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        (serde_json::from_str(&body).unwrap(), body)
    }

    fn row<'a>(board: &'a Value, uid: &str) -> &'a Value {
        board["departures"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["uid"] == uid)
            .unwrap_or_else(|| panic!("{uid} is not on the board: {board}"))
    }

    fn uids(board: &Value) -> Vec<&str> {
        board["departures"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["uid"].as_str().unwrap())
            .collect()
    }

    /// One timetable departure from [`CRS`] to `ZBB`, with the CIF calling
    /// point (and its booked platform) at [`TIPLOC`].
    async fn timetable(
        pool: &PgPool,
        service_date: &str,
        uid: &str,
        working: &str,
        day_offset: i16,
        headcode: Option<&str>,
        booked_platform: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO schedule_destination_departures \
                (service_date, destination_crs, scheduled, train_uid, origin_crs, \
                 day_offset, public_departure, operator_atoc, headcode) \
             VALUES ($1, 'ZBB', $2, $3, $4, $5, $2, 'ZQ', $6)",
        )
        .bind(date(service_date))
        .bind(time(working))
        .bind(uid)
        .bind(CRS)
        .bind(day_offset)
        .bind(headcode)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO schedule_calling_points_full \
                (service_date, uid, seq, tiploc, kind, booked_departure, day_offset, platform) \
             VALUES ($1, $2, 1, $3, 'origin', $4, $5, $6)",
        )
        .bind(date(service_date))
        .bind(uid)
        .bind(TIPLOC)
        .bind(time(working))
        .bind(day_offset)
        .bind(booked_platform)
        .execute(pool)
        .await
        .unwrap();
    }

    /// One Push Port location at [`TIPLOC`].
    #[derive(Default)]
    #[expect(
        clippy::struct_excessive_bools,
        reason = "one flag per column of the fixture row"
    )]
    struct Darwin<'a> {
        platform: Option<&'a str>,
        confirmed: bool,
        platform_suppressed: bool,
        cis_suppressed: bool,
        location_suppressed: bool,
        forecast: Option<&'a str>,
        actual: Option<&'a str>,
        cancelled: bool,
        delayed: bool,
        cancel_reason: Option<&'a str>,
        /// The arrival forecast and actual (`arrival_et`, `arrival_at`).
        arrival_forecast: Option<&'a str>,
        arrival_actual: Option<&'a str>,
        arrival_delayed: bool,
        /// The working arrival (`wta`); `""` when the call has none.
        wta: &'a str,
    }

    async fn darwin(pool: &PgPool, service_date: &str, uid: &str, wtd: &str, call: Darwin<'_>) {
        let rid = format!("{service_date}-{uid}");
        sqlx::query(
            "INSERT INTO darwin_services (rid, uid, ssd, cancel_reason, message_ts) \
             VALUES ($1, $2, $3, $4, now())",
        )
        .bind(&rid)
        .bind(uid)
        .bind(date(service_date))
        .bind(call.cancel_reason)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO darwin_calls \
                (rid, tiploc, wtd, ssd, is_cancelled, platform, platform_confirmed, \
                 platform_suppressed, platform_cis_suppressed, is_suppressed, departure_et, \
                 departure_at, departure_delayed, arrival_et, arrival_at, arrival_delayed, wta) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
        )
        .bind(&rid)
        .bind(TIPLOC)
        .bind(wtd)
        .bind(date(service_date))
        .bind(call.cancelled)
        .bind(call.platform)
        .bind(call.confirmed)
        .bind(call.platform_suppressed)
        .bind(call.cis_suppressed)
        .bind(call.location_suppressed)
        .bind(call.forecast)
        .bind(call.actual)
        .bind(call.delayed)
        .bind(call.arrival_forecast)
        .bind(call.arrival_actual)
        .bind(call.arrival_delayed)
        .bind(call.wta)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn station(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO tiploc_crs (tiploc, crs, station_name, stanox, source_sequence) \
             VALUES ($1, $2, 'Zed Bay', '99901', 1) ON CONFLICT DO NOTHING",
        )
        .bind(TIPLOC)
        .bind(CRS)
        .execute(pool)
        .await
        .unwrap();
    }

    /// The scenario the precedence tests share, all departing [`CRS`] on
    /// [`DATE`] with the clock at 10:00.
    #[expect(
        clippy::too_many_lines,
        reason = "test code: one fixture per outcome, read top to bottom"
    )]
    async fn seed_board(pool: &PgPool) {
        station(pool).await;
        // Gone: left at 09:56.
        timetable(pool, DATE, "ZB0010", "09:55:00", 0, None, Some("1")).await;
        darwin(
            pool,
            DATE,
            "ZB0010",
            "09:55",
            Darwin {
                platform: Some("1"),
                actual: Some("09:56"),
                ..Darwin::default()
            },
        )
        .await;
        // Booked 09:50, forecast 10:03: still on the board, 13 late.
        timetable(pool, DATE, "ZB0011", "09:50:00", 0, None, None).await;
        darwin(
            pool,
            DATE,
            "ZB0011",
            "09:50",
            Darwin {
                platform: Some("2"),
                forecast: Some("10:03"),
                ..Darwin::default()
            },
        )
        .await;
        // Confirmed, on time.
        timetable(pool, DATE, "ZB0001", "10:05:00", 0, None, Some("9")).await;
        darwin(
            pool,
            DATE,
            "ZB0001",
            "10:05",
            Darwin {
                platform: Some("4"),
                confirmed: true,
                forecast: Some("10:05"),
                ..Darwin::default()
            },
        )
        .await;
        // Expected, 4 late.
        timetable(pool, DATE, "ZB0002", "10:10:00", 0, None, Some("9")).await;
        darwin(
            pool,
            DATE,
            "ZB0002",
            "10:10",
            Darwin {
                platform: Some("5"),
                forecast: Some("10:14"),
                ..Darwin::default()
            },
        )
        .await;
        // Darwin hides its platform; the timetable says 3B.
        timetable(pool, DATE, "ZB0003", "10:15:00", 0, None, Some("3B")).await;
        darwin(
            pool,
            DATE,
            "ZB0003",
            "10:15",
            Darwin {
                platform: Some("9Z"),
                platform_suppressed: true,
                forecast: Some("10:15"),
                ..Darwin::default()
            },
        )
        .await;
        // The station's own system hides it.
        timetable(pool, DATE, "ZB0004", "10:20:00", 0, None, Some("3C")).await;
        darwin(
            pool,
            DATE,
            "ZB0004",
            "10:20",
            Darwin {
                platform: Some("9Y"),
                confirmed: true,
                cis_suppressed: true,
                ..Darwin::default()
            },
        )
        .await;
        // No Darwin at all: the booked platform.
        timetable(pool, DATE, "ZB0005", "10:25:00", 0, Some("1Z05"), Some("6")).await;
        // No Darwin, no booked platform.
        timetable(pool, DATE, "ZB0006", "10:30:00", 0, None, None).await;
        // Cancelled.
        timetable(pool, DATE, "ZB0007", "10:35:00", 0, None, Some("8")).await;
        darwin(
            pool,
            DATE,
            "ZB0007",
            "10:35",
            Darwin {
                platform: Some("8"),
                cancelled: true,
                cancel_reason: Some("100"),
                ..Darwin::default()
            },
        )
        .await;
        // Darwin has the call but no platform and no forecast.
        timetable(pool, DATE, "ZB0008", "10:40:00", 0, None, Some("7")).await;
        darwin(pool, DATE, "ZB0008", "10:40", Darwin::default()).await;
        // Delayed, time unknown.
        timetable(pool, DATE, "ZB0009", "10:45:00", 0, None, None).await;
        darwin(
            pool,
            DATE,
            "ZB0009",
            "10:45",
            Darwin {
                delayed: true,
                ..Darwin::default()
            },
        )
        .await;
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn the_switch_off_answers_404(pool: PgPool) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        seed_board(&pool).await;
        let router = router(pool, Settings::default());
        let (status, _, _) = get(&router, "/stations/ZBA/board").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn each_platform_outcome_and_status_is_served_and_a_hidden_platform_never_is(
        pool: PgPool,
    ) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        seed_board(&pool).await;
        let router = router(pool, enabled(&[]));
        let (status, headers, text) = get(&router, "/stations/ZBA/board?limit=30").await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(
            headers.get(header::CACHE_CONTROL).unwrap(),
            "public, s-maxage=20, stale-while-revalidate=30"
        );
        let board: Value = serde_json::from_str(&text).unwrap();

        // Darwin's hidden platforms are nowhere in the answer.
        for hidden in ["9Z", "9Y"] {
            assert!(!text.contains(hidden), "{hidden} leaked: {text}");
        }

        // The train that has left is gone; the late one is still here.
        assert_eq!(
            uids(&board),
            [
                "ZB0011", "ZB0001", "ZB0002", "ZB0003", "ZB0004", "ZB0005", "ZB0006", "ZB0007",
                "ZB0008", "ZB0009"
            ]
        );
        assert_eq!(board["crs"], "ZBA");
        assert_eq!(board["name"], Value::Null);
        assert_eq!(board["serviceDate"], DATE);
        assert_eq!(
            board["attribution"][0]["licence"],
            "LicenseRef-RDM-Data-Sharing-Agreement"
        );

        let late = row(&board, "ZB0011");
        assert_eq!(late["scheduled"], "09:50");
        assert_eq!(late["expected"], "10:03");
        assert_eq!(late["status"], "delayed");
        assert_eq!(late["delayMinutes"], 13);
        assert_eq!(late["platform"], "2");
        assert_eq!(late["platformStatus"], "expected");

        let confirmed = row(&board, "ZB0001");
        assert_eq!(confirmed["platform"], "4");
        assert_eq!(confirmed["platformStatus"], "confirmed");
        assert_eq!(confirmed["status"], "onTime");
        assert_eq!(confirmed["delayMinutes"], 0);
        assert_eq!(confirmed["destinationCrs"], "ZBB");
        assert_eq!(confirmed["operator"], "ZQ");

        let expected = row(&board, "ZB0002");
        assert_eq!(expected["platform"], "5");
        assert_eq!(expected["platformStatus"], "expected");
        assert_eq!(expected["status"], "delayed");
        assert_eq!(expected["delayMinutes"], 4);
        assert_eq!(expected["expected"], "10:14");

        // Hidden by Darwin: the timetable's platform, labelled a prediction.
        for (uid, booked) in [("ZB0003", "3B"), ("ZB0004", "3C")] {
            let hidden = row(&board, uid);
            assert_eq!(hidden["platform"], booked, "{uid}");
            assert_eq!(hidden["platformStatus"], "likely", "{uid}");
            assert_eq!(hidden["platformBasis"], "booked", "{uid}");
        }
        assert_eq!(row(&board, "ZB0003")["status"], "onTime");

        let booked = row(&board, "ZB0005");
        assert_eq!(booked["platform"], "6");
        assert_eq!(booked["platformStatus"], "booked");
        assert_eq!(booked["platformBasis"], Value::Null);
        assert_eq!(booked["status"], "booked");
        assert_eq!(booked["expected"], Value::Null);
        assert_eq!(booked["delayMinutes"], Value::Null);

        let none = row(&board, "ZB0006");
        assert_eq!(none["platform"], Value::Null);
        assert_eq!(none["platformStatus"], "none");

        let cancelled = row(&board, "ZB0007");
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["isCancelled"], true);
        assert_eq!(cancelled["platform"], Value::Null);
        assert_eq!(cancelled["platformStatus"], "none");

        // Darwin knows the call but has nothing live: booked, never onTime.
        let silent = row(&board, "ZB0008");
        assert_eq!(silent["status"], "booked");
        assert_eq!(silent["platform"], "7");
        assert_eq!(silent["platformStatus"], "booked");

        let unknown = row(&board, "ZB0009");
        assert_eq!(unknown["status"], "delayed");
        assert_eq!(unknown["delayMinutes"], Value::Null);
        assert_eq!(unknown["expected"], Value::Null);
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_location_suppressed_call_never_shows_its_own_platform(pool: PgPool) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        station(&pool).await;
        timetable(&pool, DATE, "ZB0030", "10:05:00", 0, None, Some("3D")).await;
        darwin(
            &pool,
            DATE,
            "ZB0030",
            "10:05",
            Darwin {
                platform: Some("9X"),
                location_suppressed: true,
                ..Darwin::default()
            },
        )
        .await;
        let router = router(pool, enabled(&[]));
        let (board, text) = board(&router, "/stations/ZBA/board").await;
        assert!(!text.contains("9X"), "{text}");
        let departure = row(&board, "ZB0030");
        assert_eq!(departure["platform"], "3D");
        assert_eq!(departure["platformStatus"], "likely");
        assert_eq!(departure["platformBasis"], "booked");
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_listed_station_shows_its_booked_platform_only_as_a_prediction(pool: PgPool) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        seed_board(&pool).await;
        let router = router(pool, enabled(&["ZBA"]));
        let (board, text) = board(&router, "/stations/ZBA/board?limit=30").await;
        for (uid, platform) in [("ZB0005", "6"), ("ZB0008", "7")] {
            let departure = row(&board, uid);
            assert_eq!(departure["platform"], platform, "{uid}");
            assert_eq!(departure["platformStatus"], "likely", "{uid}");
            assert_eq!(departure["platformBasis"], "booked", "{uid}");
        }
        // Nothing derivable: to be announced.
        for uid in ["ZB0006", "ZB0009"] {
            let departure = row(&board, uid);
            assert_eq!(departure["platform"], Value::Null, "{uid}");
            assert_eq!(departure["platformStatus"], "toBeAnnounced", "{uid}");
            assert_eq!(departure["platformBasis"], Value::Null, "{uid}");
        }
        assert!(!text.contains("9Z") && !text.contains("9Y"), "{text}");
        // A public live platform still shows, unlabelled.
        assert_eq!(row(&board, "ZB0001")["platform"], "4");
        assert_eq!(row(&board, "ZB0001")["platformStatus"], "confirmed");
        assert_eq!(row(&board, "ZB0002")["platform"], "5");
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_fresh_ldbws_row_with_a_safe_match_beats_the_booked_platform(pool: PgPool) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        seed_board(&pool).await;
        let departures = serde_json::json!([{
            "service_id": "token", "operator": "ZQ", "destination_crs": "ZBB",
            "scheduled": "10:25", "estimated": "On time", "is_cancelled": false,
            "delay_minutes": 0, "headcode": "1Z05", "platform": "6L",
        }]);
        sqlx::query("INSERT INTO station_samples (crs, polled_at, departures) VALUES ($1, $2, $3)")
            .bind(CRS)
            .bind(ten_oclock() - chrono::Duration::minutes(1))
            .bind(&departures)
            .execute(&pool)
            .await
            .unwrap();
        let router = router(pool.clone(), enabled(&[]));
        let (fresh, _) = board(&router, "/stations/ZBA/board?limit=30").await;
        let departure = row(&fresh, "ZB0005");
        assert_eq!(departure["platform"], "6L");
        assert_eq!(departure["platformStatus"], "expected");
        // A hidden Darwin platform is not overridden by the board.
        assert_eq!(row(&fresh, "ZB0003")["platformStatus"], "likely");
        assert_eq!(row(&fresh, "ZB0003")["platform"], "3B");

        // An old board says nothing.
        sqlx::query("UPDATE station_samples SET polled_at = $1")
            .bind(ten_oclock() - chrono::Duration::minutes(30))
            .execute(&pool)
            .await
            .unwrap();
        let (stale, _) = board(&router, "/stations/ZBA/board?limit=30").await;
        assert_eq!(row(&stale, "ZB0005")["platform"], "6");
        assert_eq!(row(&stale, "ZB0005")["platformStatus"], "booked");
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn limit_cuts_the_board_and_bad_input_is_a_400_or_404(pool: PgPool) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        seed_board(&pool).await;
        let router = router(pool, enabled(&[]));
        let (two, _) = board(&router, "/stations/ZBA/board?limit=2").await;
        assert_eq!(uids(&two), ["ZB0011", "ZB0001"]);
        let (default, _) = board(&router, "/stations/ZBA/board").await;
        assert_eq!(default["departures"].as_array().unwrap().len(), 8);
        for uri in [
            "/stations/ZBA/board?limit=0",
            "/stations/ZB/board",
            "/stations/ZBA/board?date=tomorrow",
            "/stations/ZBA/board?operator=ZZZ",
        ] {
            assert_eq!(get(&router, uri).await.0, StatusCode::BAD_REQUEST, "{uri}");
        }
        // Not on the timetable at all.
        assert_eq!(
            get(&router, "/stations/ZBC/board").await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn just_after_midnight_the_board_has_the_previous_days_late_trains(pool: PgPool) {
        // 00:20 London on 2099-11-05.
        let _now = crate::routes::pin_london_now_for_tests(
            Utc.with_ymd_and_hms(2099, 11, 5, 0, 20, 0).unwrap(),
        );
        station(&pool).await;
        // Service date 11-04's train, 00:30 on day 1, running 4 late.
        timetable(
            &pool,
            "2099-11-04",
            "ZB0020",
            "00:30:00",
            1,
            None,
            Some("3"),
        )
        .await;
        darwin(
            &pool,
            "2099-11-04",
            "ZB0020",
            "00:30",
            Darwin {
                platform: Some("3"),
                confirmed: true,
                forecast: Some("00:34"),
                ..Darwin::default()
            },
        )
        .await;
        timetable(
            &pool,
            "2099-11-05",
            "ZB0021",
            "00:40:00",
            0,
            None,
            Some("5"),
        )
        .await;
        let router = router(pool, enabled(&[]));
        let (board, _) = board(&router, "/stations/ZBA/board").await;
        assert_eq!(board["serviceDate"], "2099-11-05");
        assert_eq!(uids(&board), ["ZB0020", "ZB0021"]);
        let late = row(&board, "ZB0020");
        assert_eq!(late["serviceDate"], "2099-11-04");
        assert_eq!(late["scheduled"], "00:30");
        assert_eq!(late["dayOffset"], 1);
        assert_eq!(late["expected"], "00:34");
        assert_eq!(late["expectedDayOffset"], 1);
        assert_eq!(late["status"], "delayed");
        assert_eq!(late["platform"], "3");
        assert_eq!(late["platformStatus"], "confirmed");
        let next = row(&board, "ZB0021");
        assert_eq!(next["serviceDate"], "2099-11-05");
        assert_eq!(next["dayOffset"], 0);
        assert_eq!(next["platformStatus"], "booked");
    }

    /// One timetable arrival at [`CRS`], for a train that calls (or ends)
    /// there, from `ZQO` to `destination`.
    struct Arriving<'a> {
        uid: &'a str,
        /// The train's true origin; the station itself for a train that
        /// starts here.
        origin: &'a str,
        destination: &'a str,
        /// Working arrival at the station (`HH:MM:SS`); `None` for a train
        /// starting here.
        arrival: Option<&'a str>,
        public_arrival: Option<&'a str>,
        /// Working departure from the station (a through train's row).
        departure: &'a str,
        can_alight: bool,
        day_offset: i16,
        booked_platform: Option<&'a str>,
    }

    impl<'a> Arriving<'a> {
        fn calls(uid: &'a str, arrival: &'a str, departure: &'a str) -> Self {
            Self {
                uid,
                origin: "ZQO",
                destination: "ZQT",
                arrival: Some(arrival),
                public_arrival: Some(arrival),
                departure,
                can_alight: true,
                day_offset: 0,
                booked_platform: None,
            }
        }

        /// Ends here, arriving at `arrival`.
        fn ends(uid: &'a str, arrival: &'a str) -> Self {
            Self {
                destination: CRS,
                ..Self::calls(uid, arrival, "00:00:00")
            }
        }
    }

    /// The timetable for an arriving train. A through train has its call
    /// row at [`CRS`]; a terminating one has an earlier call at `ZBX` (the
    /// terminus has no row of its own), both with the destination arrival.
    async fn arriving(pool: &PgPool, service_date: &str, train: &Arriving<'_>) {
        let dest_arrival = if train.destination == CRS {
            train.arrival
        } else {
            Some("23:00:00")
        };
        let (call_crs, call_arrival, call_public) = if train.destination == CRS {
            ("ZBX", None, None)
        } else {
            (CRS, train.arrival, train.public_arrival)
        };
        // A train starting here: its origin row, no arrival.
        let origin_row = train.origin == CRS;
        sqlx::query(
            "INSERT INTO schedule_destination_departures \
                (service_date, destination_crs, scheduled, train_uid, origin_crs, \
                 true_origin_crs, day_offset, public_departure, operator_atoc, headcode, \
                 calling_point_arrival, public_calling_point_arrival, destination_arrival, \
                 public_destination_arrival, destination_arrival_day_offset, can_alight) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $3, 'ZQ', '1A23', $8, $9, $10, $11, $12, $13)",
        )
        .bind(date(service_date))
        .bind(train.destination)
        .bind(time(train.departure))
        .bind(train.uid)
        .bind(call_crs)
        .bind(train.origin)
        .bind(train.day_offset)
        .bind(call_arrival.filter(|_| !origin_row).map(time))
        .bind(call_public.filter(|_| !origin_row).map(time))
        .bind(dest_arrival.map(time))
        .bind(
            train
                .public_arrival
                .filter(|_| train.destination == CRS)
                .map(time),
        )
        .bind(train.day_offset)
        .bind(train.can_alight)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO schedule_calling_points_full \
                (service_date, uid, seq, tiploc, kind, booked_arrival, booked_departure, \
                 day_offset, platform) \
             VALUES ($1, $2, 1, $3, $4, $5, $6, $7, $8)",
        )
        .bind(date(service_date))
        .bind(train.uid)
        .bind(TIPLOC)
        .bind(if train.destination == CRS {
            "terminate"
        } else {
            "intermediate"
        })
        .bind(train.arrival.map(time))
        .bind(
            (train.destination != CRS)
                .then_some(train.departure)
                .map(time),
        )
        .bind(train.day_offset)
        .bind(train.booked_platform)
        .execute(pool)
        .await
        .unwrap();
    }

    /// The arrivals scenario, all at [`CRS`] on [`DATE`] with the clock at
    /// 10:00.
    #[expect(
        clippy::too_many_lines,
        reason = "test code: one fixture per outcome, read top to bottom"
    )]
    async fn seed_arrivals(pool: &PgPool) {
        station(pool).await;
        // Arrived at 09:51: gone.
        arriving(
            pool,
            DATE,
            &Arriving::calls("ZA0003", "09:50:00", "09:52:00"),
        )
        .await;
        darwin(
            pool,
            DATE,
            "ZA0003",
            "09:52",
            Darwin {
                wta: "09:50",
                platform: Some("1"),
                arrival_actual: Some("09:51"),
                ..Darwin::default()
            },
        )
        .await;
        // Booked 09:52, forecast 10:03: still due, 11 late.
        arriving(
            pool,
            DATE,
            &Arriving::calls("ZA0004", "09:52:00", "09:54:00"),
        )
        .await;
        darwin(
            pool,
            DATE,
            "ZA0004",
            "09:54",
            Darwin {
                wta: "09:52",
                platform: Some("2"),
                arrival_forecast: Some("10:03"),
                // Its DEPARTURE data is something else entirely.
                actual: Some("09:55"),
                ..Darwin::default()
            },
        )
        .await;
        // Confirmed, on time.
        arriving(
            pool,
            DATE,
            &Arriving::calls("ZA0001", "10:05:00", "10:06:00"),
        )
        .await;
        darwin(
            pool,
            DATE,
            "ZA0001",
            "10:06",
            Darwin {
                wta: "10:05",
                platform: Some("4"),
                confirmed: true,
                arrival_forecast: Some("10:05"),
                ..Darwin::default()
            },
        )
        .await;
        // Expected platform, 4 late.
        arriving(
            pool,
            DATE,
            &Arriving::calls("ZA0002", "10:10:00", "10:11:00"),
        )
        .await;
        darwin(
            pool,
            DATE,
            "ZA0002",
            "10:11",
            Darwin {
                wta: "10:10",
                platform: Some("5"),
                arrival_forecast: Some("10:14"),
                ..Darwin::default()
            },
        )
        .await;
        // Pick-up only: nobody gets off.
        arriving(
            pool,
            DATE,
            &Arriving {
                can_alight: false,
                ..Arriving::calls("ZA0009", "10:15:00", "10:16:00")
            },
        )
        .await;
        // No Darwin: the booked platform; the board's LDBWS row (below) is
        // not used for arrivals.
        arriving(
            pool,
            DATE,
            &Arriving {
                booked_platform: Some("6"),
                ..Arriving::calls("ZA0005", "10:20:00", "10:21:00")
            },
        )
        .await;
        // Starts here: a departure, never an arrival.
        arriving(
            pool,
            DATE,
            &Arriving {
                origin: CRS,
                arrival: None,
                public_arrival: None,
                ..Arriving::calls("ZA0006", "10:25:00", "10:26:00")
            },
        )
        .await;
        // Ends here, Darwin hides the platform; the timetable says 3B.
        arriving(
            pool,
            DATE,
            &Arriving {
                booked_platform: Some("3B"),
                ..Arriving::ends("ZA0007", "10:30:00")
            },
        )
        .await;
        darwin(
            pool,
            DATE,
            "ZA0007",
            "",
            Darwin {
                wta: "10:30",
                platform: Some("9Z"),
                platform_suppressed: true,
                arrival_forecast: Some("10:30"),
                ..Darwin::default()
            },
        )
        .await;
        // Ends here, no Darwin: the booked platform 7.
        arriving(
            pool,
            DATE,
            &Arriving {
                booked_platform: Some("7"),
                ..Arriving::ends("ZA0008", "10:40:00")
            },
        )
        .await;
        // Ends here, cancelled.
        arriving(pool, DATE, &Arriving::ends("ZA0010", "10:50:00")).await;
        darwin(
            pool,
            DATE,
            "ZA0010",
            "",
            Darwin {
                wta: "10:50",
                platform: Some("8"),
                cancelled: true,
                cancel_reason: Some("100"),
                ..Darwin::default()
            },
        )
        .await;
        // Arrived more than 15 minutes ago with no live data: off the board.
        arriving(pool, DATE, &Arriving::ends("ZA0011", "09:30:00")).await;
        // A station's departures board row sharing a key with ZA0005: the
        // arrivals board must not take its platform.
        let board = serde_json::json!([{
            "service_id": "token", "operator": "ZQ", "destination_crs": "ZQT",
            "scheduled": "10:20", "estimated": "On time", "is_cancelled": false,
            "delay_minutes": 0, "headcode": "1A23", "platform": "6L",
        }]);
        sqlx::query("INSERT INTO station_samples (crs, polled_at, departures) VALUES ($1, $2, $3)")
            .bind(CRS)
            .bind(ten_oclock() - chrono::Duration::minutes(1))
            .bind(&board)
            .execute(pool)
            .await
            .unwrap();
    }

    fn arrivals(board: &Value) -> Vec<&str> {
        board["arrivals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["uid"].as_str().unwrap())
            .collect()
    }

    fn arrival<'a>(board: &'a Value, uid: &str) -> &'a Value {
        board["arrivals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["uid"] == uid)
            .unwrap_or_else(|| panic!("{uid} is not on the board: {board}"))
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn the_arrivals_board_serves_arrivals_and_terminating_trains_only(pool: PgPool) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        seed_arrivals(&pool).await;
        let router = router(pool, enabled(&[]));
        let (status, headers, text) =
            get(&router, "/stations/ZBA/board?board=arrivals&limit=30").await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(
            headers.get(header::CACHE_CONTROL).unwrap(),
            "public, s-maxage=20, stale-while-revalidate=30"
        );
        let board: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(board["board"], "arrivals");
        assert_eq!(board["departures"], serde_json::json!([]));
        assert_eq!(board["serviceDate"], DATE);
        assert!(!text.contains("9Z") && !text.contains("6L"), "{text}");

        // Gone (arrived), pick-up only, starting here and long past are
        // not on it; the late one and the terminating ones are.
        assert_eq!(
            arrivals(&board),
            [
                "ZA0004", "ZA0001", "ZA0002", "ZA0005", "ZA0007", "ZA0008", "ZA0010"
            ]
        );

        let late = arrival(&board, "ZA0004");
        assert_eq!(late["scheduled"], "09:52");
        assert_eq!(late["expected"], "10:03");
        assert_eq!(late["status"], "delayed");
        assert_eq!(late["delayMinutes"], 11);
        assert_eq!(late["platform"], "2");
        assert_eq!(late["platformStatus"], "expected");
        assert_eq!(late["originCrs"], "ZQO");
        assert_eq!(late["destinationCrs"], "ZQT");
        assert_eq!(late["terminates"], false);
        assert_eq!(late["operator"], "ZQ");
        assert_eq!(late["headcode"], "1A23");

        let confirmed = arrival(&board, "ZA0001");
        assert_eq!(confirmed["scheduled"], "10:05");
        assert_eq!(confirmed["status"], "onTime");
        assert_eq!(confirmed["delayMinutes"], 0);
        assert_eq!(confirmed["platform"], "4");
        assert_eq!(confirmed["platformStatus"], "confirmed");

        let expected = arrival(&board, "ZA0002");
        assert_eq!(expected["expected"], "10:14");
        assert_eq!(expected["delayMinutes"], 4);
        assert_eq!(expected["platformStatus"], "expected");

        // No Darwin: booked; the departures board's LDBWS 6L is not used.
        let booked = arrival(&board, "ZA0005");
        assert_eq!(booked["status"], "booked");
        assert_eq!(booked["expected"], Value::Null);
        assert_eq!(booked["platform"], "6");
        assert_eq!(booked["platformStatus"], "booked");

        // Terminating, Darwin hides the platform.
        let hidden = arrival(&board, "ZA0007");
        assert_eq!(hidden["terminates"], true);
        assert_eq!(hidden["originCrs"], "ZQO");
        assert_eq!(hidden["destinationCrs"], CRS);
        assert_eq!(hidden["scheduled"], "10:30");
        assert_eq!(hidden["platform"], "3B");
        assert_eq!(hidden["platformStatus"], "likely");
        assert_eq!(hidden["platformBasis"], "booked");
        assert_eq!(hidden["status"], "onTime");

        let ends = arrival(&board, "ZA0008");
        assert_eq!(ends["terminates"], true);
        assert_eq!(ends["platform"], "7");
        assert_eq!(ends["platformStatus"], "booked");

        let cancelled = arrival(&board, "ZA0010");
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["isCancelled"], true);
        assert_eq!(cancelled["platformStatus"], "none");
        assert_eq!(cancelled["platform"], Value::Null);
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_listed_station_shows_its_booked_platform_only_as_a_prediction_on_the_arrivals_board(
        pool: PgPool,
    ) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        seed_arrivals(&pool).await;
        let router = router(pool, enabled(&["ZBA"]));
        let (board, text) = board(&router, "/stations/ZBA/board?board=arrivals&limit=30").await;
        for uid in ["ZA0005", "ZA0007", "ZA0008"] {
            let row = arrival(&board, uid);
            assert_eq!(row["platformStatus"], "likely", "{uid}");
            assert_eq!(row["platformBasis"], "booked", "{uid}");
            assert!(row["platform"].is_string(), "{uid}");
        }
        assert!(!text.contains("9Z"), "{text}");
        // A public live platform still shows.
        assert_eq!(arrival(&board, "ZA0001")["platform"], "4");
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn the_arrivals_board_limits_filters_and_the_departures_board_is_unchanged(pool: PgPool) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        seed_arrivals(&pool).await;
        seed_board(&pool).await;
        let router = router(pool, enabled(&[]));
        let (two, _) = board(&router, "/stations/ZBA/board?board=ARRIVALS&limit=2").await;
        assert_eq!(arrivals(&two), ["ZA0004", "ZA0001"]);
        let (all, _) = board(&router, "/stations/ZBA/board?board=arrivals&limit=30").await;
        let (zq, _) = board(
            &router,
            "/stations/ZBA/board?board=arrivals&operator=zq&limit=30",
        )
        .await;
        assert_eq!(all["arrivals"], zq["arrivals"]);
        // An operator no train of the day has is a 400, as for departures.
        assert_eq!(
            get(&router, "/stations/ZBA/board?board=arrivals&operator=ZZZ")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );

        // The departures board: the default and the explicit value agree,
        // carry no arrivals and no arrival-only trains.
        let (default, text) = board(&router, "/stations/ZBA/board?limit=30").await;
        let (explicit, _) = board(&router, "/stations/ZBA/board?board=departures&limit=30").await;
        assert_eq!(default["departures"], explicit["departures"]);
        assert_eq!(default["board"], "departures");
        assert!(default.get("arrivals").is_none(), "{text}");
        assert_eq!(row(&default, "ZB0001")["platform"], "4");
        // And its rows are the departure fields: no origin.
        assert!(row(&default, "ZB0001").get("originCrs").is_none());

        assert_eq!(
            get(&router, "/stations/ZBA/board?board=bogus").await.0,
            StatusCode::BAD_REQUEST
        );
        // A station with no timetable at all.
        assert_eq!(
            get(&router, "/stations/ZBC/board?board=arrivals").await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_station_that_only_ends_trains_has_an_arrivals_board(pool: PgPool) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        // ZBT is nobody's calling point or origin: trains only end there.
        sqlx::query(
            "INSERT INTO schedule_destination_departures \
                (service_date, destination_crs, scheduled, train_uid, origin_crs, \
                 true_origin_crs, public_departure, operator_atoc, destination_arrival, \
                 public_destination_arrival) \
             VALUES ($1, 'ZBT', '09:30:00', 'ZT0001', 'ZBX', 'ZQO', '09:30:00', 'ZQ', \
                     '10:20:00', '10:20:00')",
        )
        .bind(date(DATE))
        .execute(&pool)
        .await
        .unwrap();
        let router = router(pool, enabled(&[]));
        let (board, _) = board(&router, "/stations/ZBT/board?board=arrivals").await;
        assert_eq!(arrivals(&board), ["ZT0001"]);
        assert_eq!(arrival(&board, "ZT0001")["terminates"], true);
        // The departures board of the same station has nothing: a 404.
        assert_eq!(
            get(&router, "/stations/ZBT/board").await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn just_after_midnight_the_arrivals_board_has_the_previous_days_trains(pool: PgPool) {
        // 00:20 London on 2099-11-05.
        let _now = crate::routes::pin_london_now_for_tests(
            Utc.with_ymd_and_hms(2099, 11, 5, 0, 20, 0).unwrap(),
        );
        station(&pool).await;
        // Service date 11-04's train ending here at 00:30 on day 1, 4 late.
        arriving(
            &pool,
            "2099-11-04",
            &Arriving {
                day_offset: 1,
                booked_platform: Some("3"),
                ..Arriving::ends("ZA0020", "00:30:00")
            },
        )
        .await;
        darwin(
            &pool,
            "2099-11-04",
            "ZA0020",
            "",
            Darwin {
                wta: "00:30",
                platform: Some("3"),
                confirmed: true,
                arrival_forecast: Some("00:34"),
                ..Darwin::default()
            },
        )
        .await;
        // Service date 11-04's train that arrived at 23:50: long gone.
        arriving(&pool, "2099-11-04", &Arriving::ends("ZA0022", "23:50:00")).await;
        // Today's through train at 00:40.
        arriving(
            &pool,
            "2099-11-05",
            &Arriving {
                booked_platform: Some("5"),
                ..Arriving::calls("ZA0021", "00:40:00", "00:41:00")
            },
        )
        .await;
        let router = router(pool, enabled(&[]));
        let (board, _) = board(&router, "/stations/ZBA/board?board=arrivals").await;
        assert_eq!(board["serviceDate"], "2099-11-05");
        assert_eq!(arrivals(&board), ["ZA0020", "ZA0021"]);
        let late = arrival(&board, "ZA0020");
        assert_eq!(late["serviceDate"], "2099-11-04");
        assert_eq!(late["scheduled"], "00:30");
        assert_eq!(late["dayOffset"], 1);
        assert_eq!(late["expected"], "00:34");
        assert_eq!(late["expectedDayOffset"], 1);
        assert_eq!(late["status"], "delayed");
        assert_eq!(late["terminates"], true);
        assert_eq!(late["platformStatus"], "confirmed");
        assert_eq!(arrival(&board, "ZA0021")["dayOffset"], 0);
    }

    /// A turnaround: `uid` is formed by `prev_uid`, at [`TIPLOC`].
    async fn turnaround(pool: &PgPool, uid: &str, prev_uid: &str, link: &str) {
        sqlx::query(
            "INSERT INTO schedule_turnarounds \
                (service_date, uid, prev_service_date, prev_uid, booked_turnaround_minutes, \
                 link, origin_tiploc, platform, departs_at) \
             VALUES ($1, $2, $1, $3, 30, $4, $5, NULL, $6)",
        )
        .bind(date(DATE))
        .bind(uid)
        .bind(prev_uid)
        .bind(link)
        .bind(TIPLOC)
        .bind(ten_oclock() + chrono::Duration::minutes(30))
        .execute(pool)
        .await
        .unwrap();
    }

    /// The inbound working's Darwin arrival call here.
    async fn inbound(pool: &PgPool, prev_uid: &str, call: Darwin<'_>) {
        darwin(
            pool,
            DATE,
            prev_uid,
            "",
            Darwin {
                wta: "10:20",
                ..call
            },
        )
        .await;
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_terminus_departure_is_likely_to_leave_from_its_inbound_platform(pool: PgPool) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        station(&pool).await;
        // Darwin hides the outbound platform (and "9Z" must never appear);
        // the unit arrives at platform 12 (confirmed), the timetable says 3B.
        timetable(&pool, DATE, "ZB0040", "10:30:00", 0, None, Some("3B")).await;
        darwin(
            &pool,
            DATE,
            "ZB0040",
            "10:30",
            Darwin {
                platform: Some("9Z"),
                platform_suppressed: true,
                ..Darwin::default()
            },
        )
        .await;
        turnaround(&pool, "ZB0040", "ZB0140", "association").await;
        inbound(
            &pool,
            "ZB0140",
            Darwin {
                platform: Some("12"),
                confirmed: true,
                ..Darwin::default()
            },
        )
        .await;
        // No Darwin call at all for the outbound; the inbound is expected at 11.
        timetable(&pool, DATE, "ZB0041", "10:35:00", 0, None, Some("3E")).await;
        turnaround(&pool, "ZB0041", "ZB0141", "inferred").await;
        inbound(
            &pool,
            "ZB0141",
            Darwin {
                platform: Some("11"),
                ..Darwin::default()
            },
        )
        .await;
        // The inbound's arrival platform is hidden: not a source (not even
        // as a hint), so the booked prediction is all there is.
        timetable(&pool, DATE, "ZB0042", "10:40:00", 0, None, Some("3F")).await;
        turnaround(&pool, "ZB0042", "ZB0142", "association").await;
        inbound(
            &pool,
            "ZB0142",
            Darwin {
                platform: Some("8Q"),
                confirmed: true,
                platform_suppressed: true,
                ..Darwin::default()
            },
        )
        .await;
        // The inbound's location is suppressed too.
        timetable(&pool, DATE, "ZB0043", "10:45:00", 0, None, None).await;
        turnaround(&pool, "ZB0043", "ZB0143", "association").await;
        inbound(
            &pool,
            "ZB0143",
            Darwin {
                platform: Some("8R"),
                location_suppressed: true,
                ..Darwin::default()
            },
        )
        .await;
        // The inbound is cancelled: the unit is not coming.
        timetable(&pool, DATE, "ZB0044", "10:50:00", 0, None, None).await;
        turnaround(&pool, "ZB0044", "ZB0144", "association").await;
        inbound(
            &pool,
            "ZB0144",
            Darwin {
                platform: Some("8S"),
                cancelled: true,
                ..Darwin::default()
            },
        )
        .await;
        // A cancelled outbound gets no platform, whatever its inbound says.
        timetable(&pool, DATE, "ZB0045", "10:55:00", 0, None, Some("2")).await;
        darwin(
            &pool,
            DATE,
            "ZB0045",
            "10:55",
            Darwin {
                cancelled: true,
                ..Darwin::default()
            },
        )
        .await;
        turnaround(&pool, "ZB0045", "ZB0145", "association").await;
        inbound(
            &pool,
            "ZB0145",
            Darwin {
                platform: Some("10"),
                confirmed: true,
                ..Darwin::default()
            },
        )
        .await;
        // The outbound's own public Darwin platform beats its inbound.
        timetable(&pool, DATE, "ZB0046", "11:00:00", 0, None, Some("2")).await;
        darwin(
            &pool,
            DATE,
            "ZB0046",
            "11:00",
            Darwin {
                platform: Some("6"),
                confirmed: true,
                ..Darwin::default()
            },
        )
        .await;
        turnaround(&pool, "ZB0046", "ZB0146", "association").await;
        inbound(
            &pool,
            "ZB0146",
            Darwin {
                platform: Some("10"),
                confirmed: true,
                ..Darwin::default()
            },
        )
        .await;

        let router = router(pool, enabled(&["ZBA"]));
        let (board, text) = board(&router, "/stations/ZBA/board?limit=30").await;
        // No suppressed platform string reaches the body.
        for hidden in ["9Z", "8Q", "8R", "8S"] {
            assert!(!text.contains(hidden), "{hidden} leaked: {text}");
        }
        let hidden_outbound = row(&board, "ZB0040");
        assert_eq!(hidden_outbound["platform"], "12");
        assert_eq!(hidden_outbound["platformStatus"], "likely");
        assert_eq!(hidden_outbound["platformBasis"], "inbound");
        let no_darwin = row(&board, "ZB0041");
        assert_eq!(no_darwin["platform"], "11");
        assert_eq!(no_darwin["platformStatus"], "likely");
        assert_eq!(no_darwin["platformBasis"], "inbound");
        // A hidden inbound arrival is not a source.
        let hidden_inbound = row(&board, "ZB0042");
        assert_eq!(hidden_inbound["platform"], "3F");
        assert_eq!(hidden_inbound["platformBasis"], "booked");
        for uid in ["ZB0043", "ZB0044"] {
            let departure = row(&board, uid);
            assert_eq!(departure["platform"], Value::Null, "{uid}");
            assert_eq!(departure["platformStatus"], "toBeAnnounced", "{uid}");
        }
        let cancelled = row(&board, "ZB0045");
        assert_eq!(cancelled["platform"], Value::Null);
        assert_eq!(cancelled["platformStatus"], "none");
        let announced = row(&board, "ZB0046");
        assert_eq!(announced["platform"], "6");
        assert_eq!(announced["platformStatus"], "confirmed");
        assert_eq!(announced["platformBasis"], Value::Null);
    }

    /// Outside the list the inbound still predicts, and with no hidden call
    /// the timetable platform stays `booked`.
    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn the_inbound_platform_predicts_at_any_station_and_booked_stays_booked(pool: PgPool) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        station(&pool).await;
        timetable(&pool, DATE, "ZB0050", "10:30:00", 0, None, Some("3")).await;
        turnaround(&pool, "ZB0050", "ZB0150", "association").await;
        inbound(
            &pool,
            "ZB0150",
            Darwin {
                platform: Some("2"),
                ..Darwin::default()
            },
        )
        .await;
        timetable(&pool, DATE, "ZB0051", "10:35:00", 0, None, Some("4")).await;
        let router = router(pool, enabled(&[]));
        let (board, _) = board(&router, "/stations/ZBA/board").await;
        let predicted = row(&board, "ZB0050");
        assert_eq!(predicted["platform"], "2");
        assert_eq!(predicted["platformStatus"], "likely");
        assert_eq!(predicted["platformBasis"], "inbound");
        let booked = row(&board, "ZB0051");
        assert_eq!(booked["platform"], "4");
        assert_eq!(booked["platformStatus"], "booked");
        assert_eq!(booked["platformBasis"], Value::Null);
    }

    /// The reads use the indexes the module docs name: `EXPLAIN` with
    /// sequential scans disabled, so a predicate that could not use an index
    /// would show a `Seq Scan` on the big tables.
    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn the_reads_use_their_indexes(pool: PgPool) {
        async fn plan(pool: &PgPool, sql: &str) -> String {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SET LOCAL enable_seqscan = off")
                .execute(&mut *tx)
                .await
                .unwrap();
            let lines: Vec<String> = sqlx::query_scalar(&format!("EXPLAIN {sql}"))
                .fetch_all(&mut *tx)
                .await
                .unwrap_or_else(|err| panic!("EXPLAIN {sql}: {err}"));
            tx.rollback().await.unwrap();
            lines.join("\n")
        }
        let darwin_sql = DARWIN_SQL
            .replace("$1", "ARRAY['ZB0001']")
            .replace("$2", "ARRAY['2099-11-04']")
            .replace("$3", "ARRAY['ZBATIP']");
        let darwin_plan = plan(&pool, &darwin_sql).await;
        assert!(darwin_plan.contains("darwin_calls_pkey"), "{darwin_plan}");
        assert!(
            !darwin_plan.contains("Seq Scan on darwin_calls"),
            "{darwin_plan}"
        );
        assert!(
            !darwin_plan.contains("Seq Scan on darwin_services"),
            "{darwin_plan}"
        );

        let booked_sql = station_board::BOOKED_SQL
            .replace("$1", "ARRAY['ZB0001']")
            .replace("$2", "ARRAY['2099-11-04']")
            .replace("$3", "ARRAY['ZBATIP']");
        let booked_plan = plan(&pool, &booked_sql).await;
        assert!(
            booked_plan.contains("schedule_calling_points_full_pkey"),
            "{booked_plan}"
        );

        let turnaround_sql = station_board::TURNAROUND_SQL
            .replace("$1", "ARRAY['ZB0001']")
            .replace("$2", "ARRAY['2099-11-04']")
            .replace("$3", "ARRAY['ZBATIP']");
        let turnaround_plan = plan(&pool, &turnaround_sql).await;
        assert!(
            turnaround_plan.contains("schedule_turnarounds_pkey"),
            "{turnaround_plan}"
        );

        let tiploc_plan = plan(&pool, &TIPLOCS_SQL.replace("$1", "'ZBA'")).await;
        assert!(tiploc_plan.contains("tiploc_crs_crs_idx"), "{tiploc_plan}");

        // Planner statistics from a table shaped like the real one: many
        // stations and destinations, so an index is worth choosing.
        sqlx::query(
            "INSERT INTO schedule_destination_departures \
                (service_date, destination_crs, scheduled, train_uid, origin_crs, \
                 calling_point_arrival, destination_arrival) \
             SELECT '2099-11-04', 'D' || (i % 60), make_time(i % 24, i % 60, 0), 'U' || i, \
                    'O' || (i % 400), make_time(i % 24, i % 60, 0), make_time(i % 24, i % 60, 0) \
             FROM generate_series(1, 20000) AS i",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE schedule_destination_departures")
            .execute(&pool)
            .await
            .unwrap();
        // The arrivals board's timetable reads: the calls over the
        // calling-point index, the termini over the primary key's prefix.
        let literal = |sql: &str| {
            sql.replace("$1", "'2099-11-04'::date")
                .replace("$2", "'ZBA'")
                .replace("$3", "'09:00:00'::time")
                .replace("$4", "true")
        };
        let calls_plan = plan(&pool, &literal(ARRIVAL_CALLS_SQL.as_str())).await;
        assert!(
            calls_plan.contains("schedule_destination_departures_calling_point_idx"),
            "{calls_plan}"
        );
        let termini_plan = plan(&pool, &literal(ARRIVAL_TERMINI_SQL.as_str())).await;
        assert!(
            termini_plan.contains("schedule_destination_departures_pkey"),
            "{termini_plan}"
        );
        for plan in [&calls_plan, &termini_plan] {
            assert!(
                !plan.contains("Seq Scan on schedule_destination_departures"),
                "{plan}"
            );
        }
    }
}
