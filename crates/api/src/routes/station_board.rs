//! `GET /public/stations/{crs}/board`: a station's next departures, the
//! published timetable with Darwin's live times and platforms laid over it
//! (DS-638; docs/station-board.md has the design, the platform precedence,
//! the London termini policy and the licence assumption).
//!
//! Off by default: with `STATION_BOARD_ENABLED` unset or `false` the route
//! answers `404`. Departures only; `GET /public/trains/arrivals` is the
//! arrivals view of the timetable.
//!
//! Three batched reads, whatever the board's size: the timetable rows
//! (`schedule_destination_departures`, the search `GET /public/trains/search`
//! and `schedule-departures` use), Darwin's locations at the station for
//! those trains (`darwin_services_uid_ssd`, then `darwin_calls`' primary
//! key) and the CIF booked platforms (`schedule_calling_points_full`'s
//! primary key), plus the station's TIPLOCs and the names. The station's
//! LDBWS board (`station_samples`) is read only when some row has no
//! Darwin platform and no Darwin word that it is hidden.

use std::collections::HashMap;
use std::sync::Arc;

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
    self, BoardCall, BoardPlatformStatus, BoardStatus, BookedCall, LDBWS_MAX_AGE_MINUTES, LdbwsKey,
    Settings,
};
use crate::data::{journey_alternatives, queries, train_reasons};
use crate::render::{NATIONAL_RAIL_CREDIT, RDM_LICENCE, public_else_working};
use crate::routes::departures::board_depart_from;
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
    /// Never a platform Darwin hides from the public.
    platform: Option<String>,
    platform_status: BoardPlatformStatus,
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
    departures: Vec<StationBoardDepartureJson>,
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
}

/// A timetable row, parsed.
struct BoardRow {
    uid: String,
    service_date: NaiveDate,
    working: NaiveTime,
    /// The shown (public) departure, `HH:MM`, and its day offset.
    shown: String,
    day_offset: u32,
    /// Minutes from the service date's midnight to the shown time.
    shown_total: u32,
    destination_crs: String,
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
    // The first minute still on the board, in train-day minutes of the
    // service date; the read starts a little earlier for a late train.
    let threshold = board_depart_from(query.service_date, query.today, query.now.time());
    let lookback = if query.service_date <= query.today {
        LOOKBACK_MINUTES
    } else {
        0
    };
    let page = queries::search_schedule_calling_point_departures(
        pool,
        &queries::CallingPointSearch {
            station_crs: &station,
            service_date: query.service_date,
            depart_from: (threshold - lookback).max(0),
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
    let mut rows: Vec<BoardRow> = page.departures.iter().filter_map(parse_row).collect();
    if rows.is_empty()
        && !queries::schedule_destination_departures_has_station(pool, &station, query.service_date)
            .await?
    {
        return Ok(None);
    }
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
                        station_board::darwin_minute(&c.call.wtd)
                    })
                }),
                booked: booked.get(&key).and_then(|calls| {
                    station_board::pick_visit(calls, row.working, |c| {
                        c.booked_departure.map(minute_of)
                    })
                }),
            }
        })
        .collect();
    let from_board = ldbws_platforms(pool, query, &rows, &live).await?;

    let no_booked = settings.no_booked_platform(&station);
    let mut kept: Vec<(usize, StationBoardDepartureJson)> = Vec::new();
    for (index, (row, live)) in rows.iter().zip(&live).enumerate() {
        let times = live.darwin.map(BoardCall::times);
        let status = station_board::live_status(times, row.shown_total);
        // Gone: the train has left, or it is not due any more.
        let due = status.expected_total.unwrap_or(row.shown_total);
        let departed = times.is_some_and(|t| t.departed);
        // On the row's own service date's clock: a train of the day before
        // counts its minutes from that day's midnight.
        let from = board_depart_from(row.service_date, query.today, query.now.time());
        let past = u32::try_from(from).is_ok_and(|from| due < from);
        if departed || past {
            continue;
        }
        let (platform, platform_status) = station_board::resolve_platform(
            live.darwin.map(BoardCall::platform_view),
            from_board.get(&index).map(String::as_str),
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
                platform,
                platform_status,
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
        if kept.len() >= usize::try_from(query.limit).unwrap_or(usize::MAX) {
            break;
        }
    }

    let mut codes: Vec<String> = vec![station.clone()];
    codes.extend(kept.iter().map(|(i, _)| rows[*i].destination_crs.clone()));
    let names = queries::station_names_for_crs_batch(pool, &codes).await?;
    let mut operators: Vec<String> = kept
        .iter()
        .filter_map(|(_, d)| d.operator.clone())
        .collect();
    operators.sort_unstable();
    operators.dedup();
    let operator_names = journey_alternatives::operator_names(pool, &operators).await;
    let departures = kept
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
            departure
        })
        .collect();
    Ok(Some(StationBoardResponse {
        crs: station.clone(),
        name: names.get(&station).cloned(),
        service_date: query.service_date.to_string(),
        generated_at: query
            .now
            .to_utc()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        departures,
        attribution: vec![TransitAttribution {
            licence: RDM_LICENCE.to_string(),
            text: NATIONAL_RAIL_CREDIT.to_string(),
        }],
    }))
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
                 departure_at, departure_delayed) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
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
        .execute(pool)
        .await
        .unwrap();
    }

    async fn station(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO tiploc_crs (tiploc, crs, station_name, stanox, source_sequence) \
             VALUES ($1, $2, 'Zed Bay', '99901', 1)",
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

        // The hidden platforms are nowhere in the answer, nor the booked
        // platforms they would have fallen back to.
        for hidden in ["9Z", "9Y", "3B", "3C"] {
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

        for uid in ["ZB0003", "ZB0004"] {
            let hidden = row(&board, uid);
            assert_eq!(hidden["platform"], Value::Null, "{uid}");
            assert_eq!(hidden["platformStatus"], "toBeAnnounced", "{uid}");
        }
        assert_eq!(row(&board, "ZB0003")["status"], "onTime");

        let booked = row(&board, "ZB0005");
        assert_eq!(booked["platform"], "6");
        assert_eq!(booked["platformStatus"], "booked");
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
    async fn a_location_suppressed_call_never_shows_a_platform(pool: PgPool) {
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
        assert!(!text.contains("9X") && !text.contains("3D"), "{text}");
        assert_eq!(row(&board, "ZB0030")["platformStatus"], "toBeAnnounced");
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_terminus_never_shows_a_booked_platform(pool: PgPool) {
        let _now = crate::routes::pin_london_now_for_tests(ten_oclock());
        seed_board(&pool).await;
        let router = router(pool, enabled(&["ZBA"]));
        let (board, text) = board(&router, "/stations/ZBA/board?limit=30").await;
        // The booked platforms of the rows with no live platform: 6, 7, 3B...
        for uid in ["ZB0005", "ZB0006", "ZB0008", "ZB0009"] {
            let departure = row(&board, uid);
            assert_eq!(departure["platform"], Value::Null, "{uid}");
            assert_eq!(departure["platformStatus"], "toBeAnnounced", "{uid}");
        }
        assert!(!text.contains("3B") && !text.contains("3C"), "{text}");
        // A public live platform still shows.
        assert_eq!(row(&board, "ZB0001")["platform"], "4");
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
        assert_eq!(row(&fresh, "ZB0003")["platformStatus"], "toBeAnnounced");

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

        let tiploc_plan = plan(&pool, &TIPLOCS_SQL.replace("$1", "'ZBA'")).await;
        assert!(tiploc_plan.contains("tiploc_crs_crs_idx"), "{tiploc_plan}");
    }
}
