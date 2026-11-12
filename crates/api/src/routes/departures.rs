//! `GET /public/stations/{crs}/departures`: today's live departure board
//! for `crs`, straight from `station_samples`, no aggregation. Backs the
//! trip-search picker on `/track`
//! (docs/superpowers/specs/2026-09-03-trip-search-design.md). Sibling to
//! `station_stats.rs`, same `latest_station_sample` read, same honesty
//! split, deliberately not merged into that file: this returns raw rows
//! for a picker, not computed per-operator stats -- different callers,
//! different wire shapes, no shared logic beyond the DB read itself.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde::Deserialize;
use serde_json::Value;

use crate::app::{App, Router};
use crate::data::queries;
use crate::render::{
    ScheduleDepartureJson, StationDepartureJson, schedule_departure_json, station_departure_json,
};
use crate::routes::schedule_rows;

pub fn router() -> Router {
    Router::new()
        .route(
            "/stations/{crs}/departures",
            axum::routing::get(get_station_departures),
        )
        .route(
            "/stations/{crs}/schedule-departures",
            axum::routing::get(get_station_schedule_departures),
        )
}

/// 404 when `station_samples` has no row for `crs` at all -- identical
/// honesty split to `station_stats.rs::get_station_sample_stats`. `200 []`
/// is the same "row exists, board is genuinely empty right now" fact that
/// route already draws.
///
/// **Resolving a row to `GET /Train/by-uid/{uid}/{date}`.** A row's
/// `serviceId` is RDM/LDBWS's opaque, board-relative token (see
/// `common::StationDeparture::service_id`) -- this app stores no Darwin
/// `rid` and no CIF `train_uid` for it, and the public LDBWS board this is
/// sampled from supplies neither, so there is deliberately no
/// `serviceId`- or `rid`-keyed train lookup and no `trainUid` on these
/// rows. A caller holding a board row (from here, or from its own LDBWS
/// call) resolves it with ONE call:
///
/// `GET /public/trains/resolve?station={crs}&date={localDate}&time={std}&rsid={rsid}&destination={destinationCrs}&operator={operator}`
///
/// which answers `{"trainUid","serviceDate","matchedOn","href"}` (`href`
/// is the `/Train/by-uid/...` path), `404` when nothing matches, or `409`
/// listing the candidates when several do -- it never guesses. See
/// `routes::trains::get_trains_resolve` and `data::train_resolve::resolve`.
/// LDBWS's `rsid` (the CIF Retail Service ID, which `schedule-reference`
/// now publishes on every schedule row) makes that an exact join; without
/// one, the route falls back to the timetable heuristic this comment used
/// to spell out by hand:
///
/// 1. calls at `station` whose CIF WORKING (booked) time is within +-2
///    minutes of the board's PUBLIC `std` (they can differ by a minute or
///    so), on the given date and -- for a `std` shortly after midnight --
///    on the PREVIOUS service date (CIF dates a service by where it
///    starts);
/// 2. narrowed to those whose destination and operator (ATOC code) equal
///    the row's;
/// 3. exactly one distinct `uid` left is the train; none or several is
///    unresolved.
///
/// That heuristic is the best-effort `(date, station, time, destination)`
/// correlation the train-tracking design already accepts
/// (docs/superpowers/specs/2026-08-28-train-tracking-design.md,
/// "Darwin RID"); `matchedOn: "timetable"` flags it.
///
/// The CIF-derived sibling route, `GET /public/stations/{crs}/schedule-departures`
/// (below), DOES carry `uid` per row, keyed on today's London service date
/// (`dayOffset` only moves the departure's calendar day, never the service
/// date), so a caller that can pick from that board instead of the live one
/// needs no resolution step: `GET /Train/by-uid/{uid}/{today}` directly.
///
/// **`?callingAt={CRS}`** (optional) keeps only the rows whose
/// `callingPoints` (LDBWS `subsequentCallingPoints`) include that station,
/// or whose `destinationCrs` is it. A cancelled call still counts: the row
/// is still booked to call there, and its `callingPoints` entry says
/// `isCancelled`. A malformed code is a `400`. Rows stored before the
/// poller kept calling points match on `destinationCrs` only.
///
/// **`X-Generated-At`** (response header, RFC 3339 UTC) is when this board
/// was polled (`station_samples.polled_at`). It is a header, not a body
/// field, because the body is a bare JSON array that existing clients
/// (`frontend/components/TrackTrainForm.tsx`, the MCP) index directly;
/// wrapping it in an object would break them.
pub(crate) async fn get_station_departures(
    State(app): State<App>,
    Path(crs): Path<String>,
    Query(params): Query<StationDeparturesParams>,
) -> Result<(HeaderMap, Json<Vec<StationDepartureJson>>), (StatusCode, String)> {
    let calling_at = match params.calling_at.as_deref().map(str::trim) {
        None => None,
        Some(code) if super::is_crs_code(code) => Some(code.to_ascii_uppercase()),
        Some(code) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("callingAt must be a 3-letter station code, got: {code:?}"),
            ));
        }
    };
    let Some(sample) = queries::latest_station_sample(&app.database, &crs)
        .await
        .map_err(internal_error)?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no sample data collected for station: {crs}"),
        ));
    };

    // Item 5 (2026-09-22 UX review follow-up): the live picker on `/track`
    // used to show raw CRS destination codes because this route never
    // joined station names -- `station_samples.departures` is a JSONB
    // array, not individual rows, so there is no per-row SQL join target
    // the way `TRACKED_TRAIN_STATE_SELECT` has for a single-row read.
    // Instead, collect every row's destination code, resolve them all in
    // one batched query (`station_names_for_crs_batch`, the same
    // multi-CRS lookup `routes::lines.rs` already uses), and hand the map
    // to the renderer -- see `render::station_departure_json`'s own doc
    // comment.
    let departures: Vec<&common::StationDeparture> = sample
        .departures
        .iter()
        .filter(|d| {
            calling_at
                .as_deref()
                .is_none_or(|at| d.calls_at(at) || d.destination_crs.eq_ignore_ascii_case(at))
        })
        .collect();
    let destination_crs: Vec<String> = departures
        .iter()
        .map(|d| d.destination_crs.clone())
        .collect();
    let destination_names = queries::station_names_for_crs_batch(&app.database, &destination_crs)
        .await
        .map_err(internal_error)?;

    // Order preserved exactly as stored -- `parse_departures` never
    // re-sorts (poller-ldbws/src/schema.rs), and RDM's own board is
    // already chronological by convention. No new sort introduced here.
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(
        &sample
            .polled_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    ) {
        headers.insert(GENERATED_AT_HEADER, value);
    }
    Ok((
        headers,
        Json(
            departures
                .into_iter()
                .map(|d| station_departure_json(d, &destination_names))
                .collect(),
        ),
    ))
}

/// See [`get_station_departures`]'s doc comment.
pub(crate) const GENERATED_AT_HEADER: HeaderName = HeaderName::from_static("x-generated-at");

/// Query string of [`get_station_departures`]. Unknown keys are ignored
/// (not `deny_unknown_fields`): this route took no query string before, so
/// a client's cache-buster must keep working.
#[derive(Debug, Deserialize)]
pub(crate) struct StationDeparturesParams {
    #[serde(rename = "callingAt")]
    calling_at: Option<String>,
}

/// `GET /public/stations/{crs}/schedule-departures`: the next CIF
/// SCHEDULE-derived scheduled departures from `crs` -- the whole-network
/// trip-search fallback picker's backing route, see
/// docs/superpowers/specs/2026-09-04-whole-network-trip-search-design.md
/// Decision 1.
///
/// **The board is computed on read** (2026-11-12, DS-435): the next
/// [`BOARD_SIZE`] departures from now, from `schedule_destination_departures`
/// ([`schedule_board`]), the same table and index `GET /public/trains/search`
/// reads. That table holds today through today+28 from the first
/// `schedule-reference` publish after the previous evening's delivery, so a
/// day's board exists all day. This route used to read only
/// `schedule_network_departures`, which `schedule-reference` published for
/// today alone, once per delivery (about 21:35 UTC): from London midnight
/// until then every station was a `404`, and once there the stored board was
/// the first ten departures from publish time, nearly all gone.
///
/// Only a station with no timetable rows that day falls back to the stored
/// `schedule_network_departures` board for `(crs, date)` as it is. The split
/// is the same honest one as `get_station_departures` above: `404` when
/// neither has the station (it is not on the timetable, or not on a
/// published day); `200 []` when it is but nothing is left to come.
///
/// **`?date=YYYY-MM-DD`** (optional, 2026-10-07) serves another London
/// service date's board instead of today's. Only today and yesterday are
/// accepted (anything else is a `400`): yesterday exists so a caller between
/// London midnight and the 02:00 rail-day rollover can still see the
/// previous evening's services that call here after midnight (its rows with
/// `dayOffset >= 1`). Every row in a response carries the requested date as
/// its service date; `dayOffset` is relative to it.
async fn get_station_schedule_departures(
    State(app): State<App>,
    Path(crs): Path<String>,
    Query(params): Query<ScheduleDeparturesParams>,
) -> Result<Json<Vec<ScheduleDepartureJson>>, (StatusCode, String)> {
    // `today` is London-local, not UTC -- see `routes::trains`'s
    // `london_now`/`today` split (a628841) for the original incident this
    // mirrors: during the 00:00-01:00 BST window, `Utc::now().date_naive()`
    // is still yesterday in London, so a UTC "today" served yesterday's
    // CIF-derived schedule (or 404'd) for the first hour of every service
    // day. The schedule tables are keyed by London rail-day date, never
    // UTC, so the lookup key must be computed the same way.
    let now = super::london_now();
    let today = now.date_naive();
    let service_date = schedule_departures_date(params.date, today)?;
    let Some(rows) = schedule_board(&app.database, &crs, service_date, today, now.time())
        .await
        .map_err(internal_error)?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no CIF-derived schedule data for station: {crs}"),
        ));
    };

    // Same batched name-lookup enrichment as `get_station_departures`
    // above, over this route's own `destination_crs` (nullable here --
    // absent codes are simply skipped, `station_names_for_crs_batch`
    // handles an empty input already).
    //
    // `originCrs`/`originName` (2026-10-07): the stored rows carry no
    // origin, so each uid's true origin comes from
    // `schedule_destination_departures` in one batched read, and its code
    // joins the same name lookup.
    let origins = schedule_rows::true_origins_for_rows(&app.database, service_date, &rows).await;
    let destination_crs: Vec<String> = rows
        .iter()
        .filter_map(|d| d.get("destination_crs").and_then(Value::as_str))
        .map(str::to_string)
        .chain(origins.values().cloned())
        .collect();
    let destination_names = queries::station_names_for_crs_batch(&app.database, &destination_crs)
        .await
        .map_err(internal_error)?;
    let mut rendered: Vec<ScheduleDepartureJson> = rows
        .iter()
        .map(|d| {
            let mut row = schedule_departure_json(d, &destination_names);
            // Blanked like `/trains/search`'s `originCrs`: an X-prefixed
            // pseudo-code is not a station.
            row.origin_crs = d
                .get("uid")
                .and_then(Value::as_str)
                .and_then(|uid| origins.get(uid))
                .filter(|crs| queries::is_bookable_crs(crs))
                .cloned();
            row
        })
        .collect();
    schedule_rows::attach_origin_names(&mut rendered, &destination_names);
    schedule_rows::attach_live(&app.database, service_date, &mut rendered).await;
    schedule_rows::annotate_service_modes(&app.database, service_date, &mut rendered).await;
    Ok(Json(rendered))
}

/// How many departures the board holds: the same ten the stored
/// `schedule_network_departures` board and a live board have.
const BOARD_SIZE: i64 = 10;

/// The lower bound of the board's departure window, in train-day minutes of
/// `service_date` (`queries::train_day_minutes`): the current London time
/// for today, the same instant a day later on the clock for yesterday (its
/// after-midnight rows have `dayOffset >= 1`), the start of the day for a
/// later date. A board never lists a departure that has gone.
fn board_depart_from(
    service_date: chrono::NaiveDate,
    today: chrono::NaiveDate,
    now: chrono::NaiveTime,
) -> i32 {
    match u32::try_from((today - service_date).num_days()) {
        Ok(days_ago) => queries::train_day_minutes(now, days_ago),
        Err(_) => 0,
    }
}

/// The board [`get_station_schedule_departures`] serves for `crs` on
/// `service_date`, as stored-board JSON rows (`uid`, `scheduled`,
/// `day_offset`, `destination_crs`, `public_departure`), or `None` when the
/// station has no board for the day at all.
///
/// The next [`BOARD_SIZE`] departures from `now` by
/// `queries::search_schedule_calling_point_departures` (boardable calls
/// only, public time first, train-day order); empty with the station on the
/// day's timetable is `Some(vec![])`. A station with no timetable rows falls
/// back to its stored `schedule_network_departures` rows for the date,
/// unfiltered.
async fn schedule_board(
    pool: &sqlx::PgPool,
    crs: &str,
    service_date: chrono::NaiveDate,
    today: chrono::NaiveDate,
    now: chrono::NaiveTime,
) -> anyhow::Result<Option<Vec<Value>>> {
    let station = queries::normalize_code(crs);
    let page = queries::search_schedule_calling_point_departures(
        pool,
        &queries::CallingPointSearch {
            station_crs: &station,
            service_date,
            depart_from: board_depart_from(service_date, today, now),
            depart_to: None,
            true_origin_crs: None,
            terminates_at: None,
            stops_at: None,
            arrive_from: None,
            arrive_to: None,
            operators: None,
            stop_filter: None,
            across_midnight: false,
            after: None,
            limit: BOARD_SIZE,
        },
    )
    .await?;
    if let Some(page) = page {
        if !page.departures.is_empty() {
            return Ok(Some(page.departures));
        }
        if queries::schedule_destination_departures_has_station(pool, &station, service_date)
            .await?
        {
            return Ok(Some(Vec::new()));
        }
    }
    Ok(
        queries::latest_schedule_network_departures(pool, &station, service_date)
            .await?
            .map(|stored| stored.as_array().cloned().unwrap_or_default()),
    )
}

/// Query string of [`get_station_schedule_departures`]. Unknown keys are
/// ignored, as for [`StationDeparturesParams`].
#[derive(Debug, Deserialize)]
struct ScheduleDeparturesParams {
    date: Option<chrono::NaiveDate>,
}

/// The bucket [`get_station_schedule_departures`] serves: `today` when no
/// `date` was asked for, the asked-for date when it is today or yesterday,
/// otherwise a `400`.
fn schedule_departures_date(
    requested: Option<chrono::NaiveDate>,
    today: chrono::NaiveDate,
) -> Result<chrono::NaiveDate, (StatusCode, String)> {
    match requested {
        None => Ok(today),
        Some(date) if date == today || today.pred_opt() == Some(date) => Ok(date),
        Some(date) => Err((
            StatusCode::BAD_REQUEST,
            format!("date must be today ({today}) or yesterday, got: {date}"),
        )),
    }
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used as a map_err callback, which passes the error by value"
)]
fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    if let Some(unavailable) = crate::unavailable::response_for(&err) {
        return unavailable;
    }
    tracing::error!(error = ?err, "station departures query failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "query failed".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn route_path_parses_and_dispatches() {
        // Cheap insurance the exact path-segment syntax is right -- mirrors
        // `station_stats.rs`'s own probe-router precedent for the same
        // concern.
        async fn probe(Path(crs): Path<String>) -> String {
            crs
        }

        let app: axum::Router =
            axum::Router::new().route("/stations/{crs}/departures", axum::routing::get(probe));

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/stations/EDB/departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "EDB");
    }
}

/// HTTP-layer tests exercised against a live database -- mirrors
/// `station_stats.rs::db_tests`'s seed/assert/delete pattern and its
/// `test_app` helper for constructing a real `App` around a live pool
/// without needing every other part of `AppState::init` (OIDC discovery,
/// etc). Uses `ZQT`/`ZQU` fixture CRS codes -- deliberately not `ZQQ`/
/// `ZQR`/`ZQS`, which `station_stats.rs`'s own `db_tests` already claims in
/// the same reserved `Z…` fixture namespace, since both files' tests may
/// run against the same test database.
#[cfg(test)]
mod db_tests {
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::PgPool;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;

    use super::*;
    use crate::app::{App, AppState};
    use crate::auth::oidc::{OidcClient, OidcConfig};
    use crate::data::config::{LineCatalogue, ServiceArguments};

    /// Copied from `routes::station_stats::db_tests::test_app` (that
    /// module's own doc comment: colocated per-file rather than shared,
    /// until a third file needs it too). Every field an inert placeholder
    /// except `database`, which the caller supplies -- this route touches
    /// nothing else on `App`.
    fn test_app(pool: PgPool) -> App {
        let config = ServiceArguments {
            bind_url: "0.0.0.0:0".to_string(),
            database_url: String::new(),
            migration_database_url: None,
            internal_oauth_issuer_url: "https://example.invalid".to_string(),
            internal_oauth_client_id: "test-internal-oauth-client".to_string(),
            internal_oauth_group_mcp: "srv-ds-mcp".to_string(),
            transit_networks: Vec::new(),
            source_images_file: None,
            admin_group: String::new(),
            sso_issuer_url: "https://example.invalid".to_string(),
            sso_client_id: "test-client".to_string(),
            sso_client_secret: "test-secret".to_string(),
            sso_redirect_url: "https://example.invalid/callback".to_string(),
            sso_post_login_redirect_url: "https://example.invalid/".to_string(),
            sso_discord_id_claim: String::new(),
            sso_primary_sub_claim: String::new(),
            session_ttl_days: 14,
            session_max_age_days: 90,
            session_extend_throttle_hours: 24,
            history_retention_days: 7,
            daily_stats_retention_days: 300,
            half_hourly_stats_retention_hours: 840,
            metrics_enabled: false,
            metrics_port: 9091,
            defaults_file: None,
            lines: LineCatalogue(vec![]),
            vapid_public_key: "test-vapid-public-key".to_string(),
            full_coverage_enabled_default: false,
            schedule_match_interval_secs: 300,
            reconciliation_sweep_interval_secs: 300,
            schedule_enrichment_grace_minutes: 30,
            backlog_match_sweep_interval_secs: 300,
            session_cleanup_interval_secs: 3600,
            background_loops: true,
            past_travel_retention_days: 548,
            regular_journey_retention_days: 0,
            stale_push_subscription_days: 365,
            inactive_account_retention_days: 0,
        };

        std::sync::Arc::new(AppState {
            // Built from the same catalogue the real `AppState::init`
            // builds it from, so a test never gets a matcher that
            // disagrees with its own `config.lines`.
            line_matcher: common::matcher::LineMatcher::new(&config.lines),
            config,
            database: pool,
            oidc: OidcClient::new(OidcConfig {
                issuer_url: "https://example.invalid".to_string(),
                client_id: "test-client".to_string(),
                client_secret: "test-secret".to_string(),
                redirect_url: "https://example.invalid/callback".to_string(),
            })
            .expect("construct placeholder oidc client"),
            internal_oauth_verifier: crate::auth::internal_oauth::ServiceTokenVerifier::new(
                "https://example.invalid".to_string(),
                "test-internal-oauth-client".to_string(),
            )
            .expect("construct placeholder internal-oauth verifier"),
            delegated: std::sync::Arc::default(),
            tfl_arrivals: std::sync::Arc::default(),
            personal_tokens: std::sync::Arc::default(),
            schedule_crs_line_index: std::collections::HashMap::new(),
        })
    }

    async fn connect() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
        PgPoolOptions::new()
            .connect(&database_url)
            .await
            .expect("connect to postgres")
    }

    async fn delete_fixture(pool: &PgPool, crs: &str) {
        sqlx::query("DELETE FROM station_samples WHERE crs = $1")
            .bind(crs)
            .execute(pool)
            .await
            .expect("cleanup fixture station_samples row");
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                departures -- --ignored --test-threads=1`"]
    async fn departures_no_row_for_crs_is_404_naming_the_crs() {
        let pool = connect().await;
        delete_fixture(&pool, "ZQT").await;

        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stations/ZQT/departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("ZQT"), "404 body should name the CRS: {body}");

        delete_fixture(&pool, "ZQT").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                departures -- --ignored --test-threads=1`"]
    async fn departures_a_row_present_with_empty_departures_is_200_empty_array() {
        let pool = connect().await;
        delete_fixture(&pool, "ZQT").await;

        sqlx::query(
            "INSERT INTO station_samples (crs, polled_at, departures) VALUES ('ZQT', NOW(), '[]')",
        )
        .execute(&pool)
        .await
        .expect("seed empty-departures fixture row");

        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stations/ZQT/departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!([]));

        delete_fixture(&pool, "ZQT").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                departures -- --ignored --test-threads=1`"]
    async fn departures_two_rows_render_camel_case_unfiltered_in_storage_order() {
        let pool = connect().await;
        delete_fixture(&pool, "ZQU").await;

        // Insertion order deliberately NOT chronological by `scheduled` --
        // proves the route preserves storage order rather than introducing
        // a new sort (design doc Decision 2). One cancelled entry with a
        // `cancelReason` set, one on-time entry with `cancelReason: null`.
        let departures = serde_json::json!([
            {
                "service_id": "svc-2", "operator": "ZB", "destination_crs": "BSK",
                "scheduled": "14:40", "estimated": "14:47", "is_cancelled": false,
                "delay_minutes": 7, "skipped_stations": []
            },
            {
                "service_id": "svc-1", "operator": "ZA", "destination_crs": "WAT",
                "scheduled": "14:20", "estimated": "Cancelled", "is_cancelled": true,
                "delay_minutes": 0, "cancel_reason": "fleet issue", "skipped_stations": []
            }
        ]);
        sqlx::query(
            "INSERT INTO station_samples (crs, polled_at, departures) VALUES ('ZQU', NOW(), $1)",
        )
        .bind(departures)
        .execute(&pool)
        .await
        .expect("seed two-departure fixture row");

        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stations/ZQU/departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json.as_array().unwrap().len(), 2);
        // Storage order preserved: svc-2 (delayed, ZB) inserted first,
        // svc-1 (cancelled, ZA) second -- not re-sorted by `scheduled`.
        assert_eq!(json[0]["serviceId"], "svc-2");
        assert_eq!(json[0]["operator"], "ZB");
        assert_eq!(json[0]["destinationCrs"], "BSK");
        assert_eq!(json[0]["scheduled"], "14:40");
        assert_eq!(json[0]["isCancelled"], false);
        assert_eq!(json[0]["delayMinutes"], 7);
        assert!(json[0]["cancelReason"].is_null());

        assert_eq!(json[1]["serviceId"], "svc-1");
        assert_eq!(json[1]["operator"], "ZA");
        assert_eq!(json[1]["isCancelled"], true);
        assert_eq!(json[1]["cancelReason"], "fleet issue");

        delete_fixture(&pool, "ZQU").await;
    }

    /// `rsid`, `callingPoints`, `?callingAt=` and `X-Generated-At`, end to
    /// end from the stored compact JSONB. The second row predates the
    /// poller keeping calling points (no `rsid`/`calling_points` keys) and
    /// must still render, and still match `callingAt` on its destination.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                departures -- --ignored --test-threads=1`"]
    async fn departures_serve_rsid_calling_points_calling_at_filter_and_generated_at() {
        let pool = connect().await;
        delete_fixture(&pool, "ZQV").await;
        let departures = serde_json::json!([
            {
                "service_id": "9134470ZQVTIP_1", "operator": "SW", "destination_crs": "WOK",
                "scheduled": "14:40", "estimated": "14:47", "is_cancelled": false,
                "delay_minutes": 7, "skipped_stations": ["ZQX"], "rsid": "SW123400",
                "calling_points": [
                    {"crs": "CLJ", "n": "Clapham Junction", "st": "14:47", "et": "14:54"},
                    {"crs": "ZQX", "st": "14:55", "et": "Cancelled", "x": true},
                    {"crs": "WOK", "n": "Woking", "st": "15:10", "et": "15:17"}
                ]
            },
            {
                "service_id": "legacy", "operator": "SW", "destination_crs": "CLJ",
                "scheduled": "14:50", "estimated": "On time", "is_cancelled": false,
                "delay_minutes": 0, "skipped_stations": []
            }
        ]);
        sqlx::query(
            "INSERT INTO station_samples (crs, polled_at, departures) \
             VALUES ('ZQV', '2026-09-28T13:40:05Z', $1)",
        )
        .bind(departures)
        .execute(&pool)
        .await
        .expect("seed fixture row");

        let get = |uri: &'static str| {
            let router: axum::Router = Router::new()
                .merge(router())
                .with_state(test_app(pool.clone()));
            async move {
                let response = router
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                let status = response.status();
                let generated_at = response
                    .headers()
                    .get("x-generated-at")
                    .map(|v| v.to_str().unwrap().to_string());
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (status, generated_at, body)
            }
        };

        let (status, generated_at, body) = get("/stations/ZQV/departures").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(generated_at.as_deref(), Some("2026-09-28T13:40:05Z"));
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 2);
        assert_eq!(json[0]["rsid"], "SW123400");
        assert_eq!(
            json[0]["callingPoints"],
            serde_json::json!([
                {"crs": "CLJ", "locationName": "Clapham Junction", "st": "14:47", "et": "14:54", "at": null, "isCancelled": false},
                {"crs": "ZQX", "locationName": null, "st": "14:55", "et": "Cancelled", "at": null, "isCancelled": true},
                {"crs": "WOK", "locationName": "Woking", "st": "15:10", "et": "15:17", "at": null, "isCancelled": false},
            ])
        );
        assert!(json[1]["rsid"].is_null());
        assert_eq!(json[1]["callingPoints"], serde_json::json!([]));

        // Both rows call at CLJ (the second by destination only); only the
        // first at WOK, and a cancelled call (ZQX) still counts.
        let (_, _, body) = get("/stations/ZQV/departures?callingAt=clj").await;
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 2);
        let (_, _, body) = get("/stations/ZQV/departures?callingAt=WOK").await;
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["serviceId"], "9134470ZQVTIP_1");
        let (_, _, body) = get("/stations/ZQV/departures?callingAt=ZQX").await;
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 1);
        let (status, generated_at, body) = get("/stations/ZQV/departures?callingAt=EDB").await;
        assert_eq!(status, StatusCode::OK);
        assert!(generated_at.is_some());
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            serde_json::json!([])
        );

        for bad in [
            "/stations/ZQV/departures?callingAt=",
            "/stations/ZQV/departures?callingAt=WO",
            "/stations/ZQV/departures?callingAt=W0K",
            "/stations/ZQV/departures?callingAt=WOKK",
        ] {
            let (status, _, _) = get(bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }

        delete_fixture(&pool, "ZQV").await;
    }

    /// End-to-end (storage -> route -> render) proof of the platform fields
    /// the Distant-Signal-MCP `boards` tool reads: a stored departure whose
    /// JSONB carries `platform`/`planned_platform` (exactly what
    /// `poller-ldbws` persists via `common::StationDeparture`'s serde shape,
    /// `PlatformHistory` having filled `planned_platform`) renders both as
    /// camelCase strings, and one whose JSONB omits them (the poller's
    /// `skip_serializing_if = "Option::is_none"` for an unallocated
    /// platform) renders explicit `null`s, never an absent key.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                departures -- --ignored --test-threads=1`"]
    async fn departures_render_stored_platform_fields_or_explicit_null() {
        let pool = connect().await;
        delete_fixture(&pool, "ZQP").await;

        let departures = serde_json::json!([
            {
                "service_id": "svc-plat", "operator": "SW", "destination_crs": "WAT",
                "scheduled": "10:00", "estimated": "On time", "is_cancelled": false,
                "delay_minutes": 0, "skipped_stations": [],
                "platform": "9", "planned_platform": "6"
            },
            {
                "service_id": "svc-noplat", "operator": "SW", "destination_crs": "WAT",
                "scheduled": "10:30", "estimated": "On time", "is_cancelled": false,
                "delay_minutes": 0, "skipped_stations": []
            }
        ]);
        sqlx::query(
            "INSERT INTO station_samples (crs, polled_at, departures) VALUES ('ZQP', NOW(), $1)",
        )
        .bind(departures)
        .execute(&pool)
        .await
        .expect("seed platform fixture row");

        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stations/ZQP/departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json[0]["platform"], "9");
        assert_eq!(json[0]["plannedPlatform"], "6");
        assert_eq!(json[0]["platformChanged"], true);

        let second = json[1].as_object().unwrap();
        assert!(second.contains_key("platform") && second["platform"].is_null());
        assert!(second.contains_key("plannedPlatform") && second["plannedPlatform"].is_null());
        assert_eq!(second["platformChanged"], false);

        delete_fixture(&pool, "ZQP").await;
    }

    async fn delete_schedule_departures_fixture(pool: &PgPool, crs: &str) {
        sqlx::query("DELETE FROM schedule_network_departures WHERE crs = $1")
            .bind(crs)
            .execute(pool)
            .await
            .expect("cleanup fixture schedule_network_departures rows");
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                schedule_departures -- --ignored --test-threads=1`"]
    async fn schedule_departures_no_row_for_crs_today_is_404_naming_the_crs() {
        let pool = connect().await;
        delete_schedule_departures_fixture(&pool, "ZQX").await;

        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stations/ZQX/schedule-departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8(body.to_vec()).unwrap().contains("ZQX"));

        delete_schedule_departures_fixture(&pool, "ZQX").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                schedule_departures -- --ignored --test-threads=1`"]
    async fn schedule_departures_a_row_only_for_a_different_date_is_still_404_today() {
        // Proves the route's "always today, server-side" date scoping -- a
        // stale row from a different service_date must never leak through.
        let pool = connect().await;
        delete_schedule_departures_fixture(&pool, "ZQY").await;

        let yesterday = crate::routes::london_today() - chrono::Duration::days(1);
        sqlx::query(
            "INSERT INTO schedule_network_departures (crs, service_date, departures) VALUES ('ZQY', $1, '[]')",
        )
        .bind(yesterday)
        .execute(&pool)
        .await
        .expect("seed a stale fixture row");

        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stations/ZQY/schedule-departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        delete_schedule_departures_fixture(&pool, "ZQY").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                schedule_departures -- --ignored --test-threads=1`"]
    async fn schedule_departures_uses_london_local_today_not_bare_utc() {
        // Regression for the 19-pass review's Medium finding: this route
        // used to key its lookup off `chrono::Utc::now().date_naive()`,
        // exactly the bug `routes::trains`'s `london_now` split (a628841)
        // already fixed for `/trains/search` -- during the roughly-hour-
        // long window where UTC's calendar day still lags London's (every
        // night of British Summer Time, 23:00-00:00 UTC = 00:00-01:00
        // London), a bare-UTC "today" is one day behind, so this route
        // served yesterday's CIF-derived schedule or 404'd for the first
        // hour of every service day.
        let pool = connect().await;
        delete_schedule_departures_fixture(&pool, "ZQW").await;

        let utc_today = chrono::Utc::now().date_naive();
        let london_today = crate::routes::london_today();

        if utc_today == london_today {
            // Outside the UTC/London date-boundary gap right now -- the
            // two clocks agree on the calendar day, so a route reverted to
            // bare UTC would compute the exact same date this test just
            // computed and the assertion below would pass either way.
            // Nothing this test COULD discriminate in that window; see
            // `routes::trains::tests::trains_search_hides_a_departure_inside_the_utc_vs_london_gap`
            // for the identical, established skip pattern. True no-op, not
            // lost coverage.
            return;
        }

        // Inside the gap: seed a row keyed ONLY by the wrong, bare-UTC
        // date. If the route regresses to `Utc::now().date_naive()` it
        // will find this row and return 200; the fix must 404 instead,
        // since there is no row for the correct London-local date.
        sqlx::query(
            "INSERT INTO schedule_network_departures (crs, service_date, departures) VALUES ('ZQW', $1, '[]')",
        )
        .bind(utc_today)
        .execute(&pool)
        .await
        .expect("seed a bare-UTC-dated fixture row");

        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stations/ZQW/schedule-departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "a row keyed by bare UTC \"today\" must not satisfy a London-local \"today\" \
             lookup during the UTC/London date gap; if this fails, the route has regressed \
             to `Utc::now().date_naive()`"
        );

        delete_schedule_departures_fixture(&pool, "ZQW").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                schedule_departures -- --ignored --test-threads=1`"]
    async fn schedule_departures_a_row_for_today_with_empty_departures_is_200_empty_array() {
        let pool = connect().await;
        delete_schedule_departures_fixture(&pool, "ZQZ").await;

        let today = crate::routes::london_today();
        sqlx::query(
            "INSERT INTO schedule_network_departures (crs, service_date, departures) VALUES ('ZQZ', $1, '[]')",
        )
        .bind(today)
        .execute(&pool)
        .await
        .expect("seed empty-departures fixture row");

        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stations/ZQZ/schedule-departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json, serde_json::json!([]));

        delete_schedule_departures_fixture(&pool, "ZQZ").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                schedule_departures -- --ignored --test-threads=1`"]
    async fn schedule_departures_two_rows_render_camel_case_with_trimmed_time() {
        let pool = connect().await;
        delete_schedule_departures_fixture(&pool, "ZRA").await;

        let today = crate::routes::london_today();
        let departures = serde_json::json!([
            {"uid": "C11052", "scheduled": "08:22:00", "destination_crs": "CRE"},
            {"uid": "C99999", "scheduled": "09:00:00", "destination_crs": null},
        ]);
        sqlx::query(
            "INSERT INTO schedule_network_departures (crs, service_date, departures) VALUES ('ZRA', $1, $2)",
        )
        .bind(today)
        .bind(departures)
        .execute(&pool)
        .await
        .expect("seed two-departure fixture row");

        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stations/ZRA/schedule-departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json.as_array().unwrap().len(), 2);
        assert_eq!(json[0]["uid"], "C11052");
        assert_eq!(json[0]["scheduled"], "08:22");
        assert_eq!(json[0]["destinationCrs"], "CRE");
        assert!(json[1]["destinationCrs"].is_null());
        assert!(
            json[0].get("destination_crs").is_none(),
            "no stray snake_case field"
        );

        delete_schedule_departures_fixture(&pool, "ZRA").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                schedule_departures -- --ignored --test-threads=1`"]
    async fn schedule_departures_serializes_a_post_midnight_calling_points_day_offset() {
        // The exact regression this route's own `day_offset` addition
        // targets: a calling point whose CIF-derived `scheduled` time is
        // small (early-hours) but genuinely falls on the day AFTER
        // `service_date` -- the real live-confirmed c2c Liverpool Street ->
        // Shoeburyness overnight working, UID F49687, Barking 00:07 (see
        // `schedule_query::resolve`'s own `f49687_raw` fixture doc comment).
        // `TrackTrainForm.tsx::pickCifDeparture` reads this field to derive
        // the correct `service_date` for a post-midnight pick instead of
        // always assuming "today".
        let pool = connect().await;
        delete_schedule_departures_fixture(&pool, "ZRB").await;

        let today = crate::routes::london_today();
        let departures = serde_json::json!([
            {"uid": "C11052", "scheduled": "08:22:00", "day_offset": 0, "destination_crs": "CRE"},
            {"uid": "F49687", "scheduled": "00:07:00", "day_offset": 1, "destination_crs": "SNF"},
        ]);
        sqlx::query(
            "INSERT INTO schedule_network_departures (crs, service_date, departures) VALUES ('ZRB', $1, $2)",
        )
        .bind(today)
        .bind(departures)
        .execute(&pool)
        .await
        .expect("seed day-offset fixture row");

        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stations/ZRB/schedule-departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json[0]["uid"], "C11052");
        assert_eq!(json[0]["dayOffset"], 0, "same-day calling point stays at 0");
        assert_eq!(json[1]["uid"], "F49687");
        assert_eq!(
            json[1]["dayOffset"], 1,
            "Barking 00:07's day_offset must reach the HTTP response, not just the DB row"
        );

        delete_schedule_departures_fixture(&pool, "ZRB").await;
    }

    #[test]
    fn schedule_departures_date_accepts_only_today_and_yesterday() {
        let today: chrono::NaiveDate = "2026-10-07".parse().unwrap();
        assert_eq!(schedule_departures_date(None, today), Ok(today));
        assert_eq!(schedule_departures_date(Some(today), today), Ok(today));
        let yesterday: chrono::NaiveDate = "2026-10-06".parse().unwrap();
        assert_eq!(
            schedule_departures_date(Some(yesterday), today),
            Ok(yesterday)
        );
        for rejected in ["2026-10-05", "2026-10-08"] {
            let (status, _) =
                schedule_departures_date(Some(rejected.parse().unwrap()), today).unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST, "{rejected}");
        }
    }

    #[test]
    fn board_window_starts_at_now_today_a_day_on_for_yesterday_and_at_midnight_later() {
        let today: chrono::NaiveDate = "2026-11-12".parse().unwrap();
        let now = chrono::NaiveTime::from_hms_opt(0, 30, 59).unwrap();
        assert_eq!(board_depart_from(today, today, now), 30);
        // Yesterday's board on today's clock: 00:30 is minute 1470 of it.
        assert_eq!(
            board_depart_from(today.pred_opt().unwrap(), today, now),
            1470
        );
        // A later date has no "now" to be forward of.
        assert_eq!(board_depart_from(today.succ_opt().unwrap(), today, now), 0);
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                schedule_departures -- --ignored --test-threads=1`"]
    async fn schedule_departures_date_param_serves_yesterdays_bucket_and_rejects_older() {
        // The /track picker asks for yesterday's bucket between London
        // midnight and 02:00, for the previous evening's services that call
        // here after midnight (`dayOffset >= 1`). Its `live` and origin come
        // from the requested date too, not today.
        let pool = connect().await;
        clear_live_fixtures(&pool).await;

        let today = crate::routes::london_today();
        let yesterday = today.pred_opt().unwrap();
        sqlx::query(
            "INSERT INTO schedule_network_departures (crs, service_date, departures) VALUES ('ZRC', $1, $2)",
        )
        .bind(yesterday)
        .bind(serde_json::json!([
            {"uid": "SDLVYD", "scheduled": "00:50:00", "day_offset": 1, "destination_crs": "SNF"},
        ]))
        .execute(&pool)
        .await
        .expect("seed yesterday's fixture row");
        sqlx::query("INSERT INTO stations (crs, name) VALUES ('ZSP', 'Zed Start')")
            .execute(&pool)
            .await
            .expect("seed origin station");
        // Yesterday's schedule origin and live state only: a lookup keyed on
        // today would find neither.
        sqlx::query(
            "INSERT INTO schedule_destination_departures \
                (service_date, destination_crs, scheduled, train_uid, origin_crs, true_origin_crs) \
             VALUES ($1, 'SNF', '23:40:00'::time, 'SDLVYD', 'ZRD', 'ZSP')",
        )
        .bind(yesterday)
        .execute(&pool)
        .await
        .expect("seed yesterday's schedule row");
        let (trains_id,): (i64,) = sqlx::query_as(
            "INSERT INTO trains (train_uid, service_date, train_id) VALUES ('SDLVYD', $1, 'SDLVYD') \
             RETURNING id",
        )
        .bind(yesterday)
        .fetch_one(&pool)
        .await
        .expect("seed yesterday's trains row");
        sqlx::query(
            "INSERT INTO train_current_state (trains_id, status, last_reported_location, \
                 delay_minutes, updated_at) VALUES ($1, 'en_route', 'Barking', 2, NOW())",
        )
        .bind(trains_id)
        .execute(&pool)
        .await
        .expect("seed yesterday's train_current_state row");

        let get = |uri: String| {
            let router: axum::Router = Router::new()
                .merge(router())
                .with_state(test_app(pool.clone()));
            async move {
                router
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            }
        };

        let response = get(format!(
            "/stations/ZRC/schedule-departures?date={yesterday}"
        ))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json[0]["uid"], "SDLVYD");
        assert_eq!(json[0]["dayOffset"], 1);
        assert_eq!(json[0]["originCrs"], "ZSP", "{json}");
        assert_eq!(json[0]["originName"], "Zed Start");
        assert_eq!(json[0]["live"]["status"], "en_route", "{json}");
        assert_eq!(json[0]["live"]["delayMinutes"], 2);

        // No `date`: still today's bucket only, which has no row here.
        let response = get("/stations/ZRC/schedule-departures".to_string()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let older = yesterday.pred_opt().unwrap();
        let response = get(format!("/stations/ZRC/schedule-departures?date={older}")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response = get("/stations/ZRC/schedule-departures?date=not-a-date".to_string()).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        clear_live_fixtures(&pool).await;
    }

    /// Removes the `SDLV*` live-state fixtures, their schedule rows, the
    /// `ZRC` board and the origin station. `train_current_state` first: its
    /// `trains_id` is `ON DELETE SET NULL`.
    async fn clear_live_fixtures(pool: &PgPool) {
        for sql in [
            "DELETE FROM train_current_state WHERE trains_id IN \
                (SELECT id FROM trains WHERE train_uid LIKE 'SDLV%')",
            "DELETE FROM trains WHERE train_uid LIKE 'SDLV%'",
            "DELETE FROM schedule_destination_departures WHERE train_uid LIKE 'SDLV%'",
            "DELETE FROM schedule_network_departures WHERE crs = 'ZRC'",
            "DELETE FROM stations WHERE crs = 'ZSP'",
        ] {
            sqlx::query(sql)
                .execute(pool)
                .await
                .expect("clear live fixtures");
        }
    }

    /// The 2026-10-07 additive fields: `live` (with state, cancelled, and
    /// none), `originCrs`/`originName`, with `dayOffset` unchanged.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                schedule_departures -- --ignored --test-threads=1`"]
    #[expect(
        clippy::too_many_lines,
        reason = "test code: scenario tests read top to bottom"
    )]
    async fn schedule_departures_rows_carry_live_status_and_origin_name() {
        let pool = connect().await;
        clear_live_fixtures(&pool).await;
        let today = crate::routes::london_today();
        sqlx::query("INSERT INTO stations (crs, name) VALUES ('ZSP', 'Zed Start')")
            .execute(&pool)
            .await
            .expect("seed origin station");
        let departures = serde_json::json!([
            {"uid": "SDLVLV", "scheduled": "08:22:00", "day_offset": 0, "destination_crs": "CRE"},
            {"uid": "SDLVCX", "scheduled": "09:00:00", "day_offset": 0, "destination_crs": "CRE"},
            {"uid": "SDLVNX", "scheduled": "00:07:00", "day_offset": 1, "destination_crs": "SNF"},
        ]);
        sqlx::query(
            "INSERT INTO schedule_network_departures (crs, service_date, departures) VALUES ('ZRC', $1, $2)",
        )
        .bind(today)
        .bind(departures)
        .execute(&pool)
        .await
        .expect("seed board");
        // True origins: two calls for SDLVLV (one origin, read once); none
        // for SDLVNX.
        for (uid, scheduled, origin) in [
            ("SDLVLV", "08:00:00", Some("ZSP")),
            ("SDLVLV", "08:22:00", Some("ZSP")),
            ("SDLVCX", "09:00:00", Some("XQQ")),
        ] {
            sqlx::query(
                "INSERT INTO schedule_destination_departures \
                    (service_date, destination_crs, scheduled, train_uid, origin_crs, true_origin_crs) \
                 VALUES ($1, 'CRE', $2::time, $3, 'ZRD', $4)",
            )
            .bind(today)
            .bind(scheduled)
            .bind(uid)
            .bind(origin)
            .execute(&pool)
            .await
            .expect("seed schedule row");
        }
        for (uid, status, delay) in [
            ("SDLVLV", "en_route", Some(3)),
            ("SDLVCX", "cancelled", None),
        ] {
            let (trains_id,): (i64,) = sqlx::query_as(
                "INSERT INTO trains (train_uid, service_date, train_id) VALUES ($1, $2, $1) \
                 RETURNING id",
            )
            .bind(uid)
            .bind(today)
            .fetch_one(&pool)
            .await
            .expect("seed trains row");
            sqlx::query(
                "INSERT INTO train_current_state (trains_id, status, last_reported_location, \
                     delay_minutes, updated_at) VALUES ($1, $2, 'Crewe', $3, NOW())",
            )
            .bind(trains_id)
            .bind(status)
            .bind(delay)
            .execute(&pool)
            .await
            .expect("seed train_current_state row");
        }

        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/stations/ZRC/schedule-departures")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let rows = json.as_array().unwrap();
        assert_eq!(rows.len(), 3, "{json}");

        assert_eq!(rows[0]["uid"], "SDLVLV");
        assert_eq!(rows[0]["originCrs"], "ZSP");
        assert_eq!(rows[0]["originName"], "Zed Start");
        assert_eq!(
            rows[0]["live"],
            serde_json::json!({
                "status": "en_route",
                "delayMinutes": 3,
                "delayProvisional": false,
                "cancelled": false,
                "cancelReason": null,
                "lastReportedLocation": "Crewe",
            }),
        );
        // An X-prefixed pseudo-code origin is blanked, like the search's.
        assert!(rows[1]["originCrs"].is_null(), "{}", rows[1]);
        assert!(rows[1]["originName"].is_null());
        assert_eq!(rows[1]["live"]["cancelled"], true);
        // No live state, no schedule origin; dayOffset as before.
        let third = rows[2].as_object().unwrap();
        assert!(
            third.contains_key("live") && third["live"].is_null(),
            "{json}"
        );
        assert!(third.contains_key("originName") && third["originName"].is_null());
        assert!(third["originCrs"].is_null());
        assert_eq!(third["dayOffset"], 1);

        clear_live_fixtures(&pool).await;
    }

    /// Seeds one `schedule_destination_departures` row for the calling
    /// point `crs`: the product `schedule-reference` publishes for today
    /// through today+28 at each delivery.
    async fn seed_timetable_call(
        pool: &PgPool,
        service_date: chrono::NaiveDate,
        crs: &str,
        uid: &str,
        scheduled: &str,
        day_offset: i16,
        can_board: bool,
    ) {
        sqlx::query(
            "INSERT INTO schedule_destination_departures \
                (service_date, destination_crs, scheduled, day_offset, train_uid, origin_crs, \
                 can_board) \
             VALUES ($1, 'SNF', $2::time, $3, $4, $5, $6)",
        )
        .bind(service_date)
        .bind(scheduled)
        .bind(day_offset)
        .bind(uid)
        .bind(crs)
        .bind(can_board)
        .execute(pool)
        .await
        .expect("seed timetable call");
    }

    async fn clear_board_fixtures(pool: &PgPool) {
        for sql in [
            "DELETE FROM schedule_destination_departures WHERE train_uid LIKE 'SDLB%'",
            "DELETE FROM schedule_network_departures WHERE crs IN ('ZRE', 'ZRF')",
        ] {
            sqlx::query(sql)
                .execute(pool)
                .await
                .expect("clear board fixtures");
        }
    }

    async fn get_board(pool: &PgPool, uri: &str) -> (StatusCode, Value) {
        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = router
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, json)
    }

    /// DS-435. `schedule-reference` publishes the timetable for today
    /// through today+28 at each delivery, about 21:35 UTC the evening
    /// before, and the network-departures board (once) for the delivery's
    /// day only. This is the state at London midnight: the timetable rows
    /// for today and no `schedule_network_departures` row at all. The board
    /// must still be served, from now on, and for the first minute of the
    /// day as for any other.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                schedule_departures -- --ignored --test-threads=1`"]
    async fn schedule_departures_board_exists_all_day_from_the_previous_evenings_publish() {
        let pool = connect().await;
        clear_board_fixtures(&pool).await;
        let today: chrono::NaiveDate = "2026-11-12".parse().unwrap();
        // 2026-11-12 is GMT: London time is UTC.
        let at = |hour, minute| today.and_hms_opt(hour, minute, 0).unwrap().and_utc();
        seed_timetable_call(&pool, today, "ZRE", "SDLBA1", "00:10:00", 0, true).await;
        // Twelve more, from 06:00; the board holds the next ten.
        for n in 0..12 {
            let uid = format!("SDLBB{n:02}");
            let time = format!("{:02}:00:00", 6 + n);
            seed_timetable_call(&pool, today, "ZRE", &uid, &time, 0, true).await;
        }
        // Set down only: never on a board.
        seed_timetable_call(&pool, today, "ZRE", "SDLBSD", "06:30:00", 0, false).await;
        // A late call, last of the day.
        seed_timetable_call(&pool, today, "ZRE", "SDLBZZ", "23:55:00", 0, true).await;

        let uids = |json: &Value| -> Vec<String> {
            json.as_array()
                .unwrap()
                .iter()
                .map(|row| row["uid"].as_str().unwrap().to_string())
                .collect()
        };

        {
            let _now = crate::routes::pin_london_now_for_tests(at(0, 30));
            let (status, json) = get_board(&pool, "/stations/ZRE/schedule-departures").await;
            assert_eq!(status, StatusCode::OK, "{json}");
            let got = uids(&json);
            assert_eq!(got.len(), 10, "{json}");
            // The 00:10 departure has gone; the set-down call is absent.
            assert_eq!(got[0], "SDLBB00", "{json}");
            assert!(!got.contains(&"SDLBA1".to_string()));
            assert!(!got.contains(&"SDLBSD".to_string()));
            assert_eq!(json[0]["scheduled"], "06:00");
            assert_eq!(json[0]["dayOffset"], 0);
            assert_eq!(json[0]["destinationCrs"], "SNF");
            // Lower-case and padded codes resolve like the stored board's.
            let (status, again) = get_board(&pool, "/stations/zre/schedule-departures").await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(uids(&again), got);
        }
        {
            // Mid-evening: only what is still to come, in order.
            let _now = crate::routes::pin_london_now_for_tests(at(16, 0));
            let (status, json) = get_board(&pool, "/stations/ZRE/schedule-departures").await;
            assert_eq!(status, StatusCode::OK, "{json}");
            assert_eq!(uids(&json), ["SDLBB10", "SDLBB11", "SDLBZZ"], "{json}");
        }
        {
            // Everything has gone: the station is on the timetable, so 200 [].
            let _now = crate::routes::pin_london_now_for_tests(at(23, 59));
            let (status, json) = get_board(&pool, "/stations/ZRE/schedule-departures").await;
            assert_eq!(status, StatusCode::OK, "{json}");
            assert_eq!(json, serde_json::json!([]));
            // A station with no timetable and no stored board is still 404.
            let (status, _) = get_board(&pool, "/stations/ZRF/schedule-departures").await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }

        clear_board_fixtures(&pool).await;
    }

    /// The computed board wins over a stored one (the first ten departures
    /// from its publish time, here long gone); a station with no timetable
    /// rows still gets its stored board.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                schedule_departures -- --ignored --test-threads=1`"]
    async fn schedule_departures_computed_board_replaces_a_stale_stored_one() {
        let pool = connect().await;
        clear_board_fixtures(&pool).await;
        let today: chrono::NaiveDate = "2026-11-12".parse().unwrap();
        let _now =
            crate::routes::pin_london_now_for_tests(today.and_hms_opt(12, 0, 0).unwrap().and_utc());
        sqlx::query(
            "INSERT INTO schedule_network_departures (crs, service_date, departures) \
             VALUES ('ZRE', $1, $2), ('ZRF', $1, $2)",
        )
        .bind(today)
        .bind(serde_json::json!([
            {"uid": "SDLBOLD", "scheduled": "08:00:00", "day_offset": 0, "destination_crs": "SNF"},
        ]))
        .execute(&pool)
        .await
        .expect("seed stored boards");
        seed_timetable_call(&pool, today, "ZRE", "SDLBNEW", "12:30:00", 0, true).await;

        let (status, json) = get_board(&pool, "/stations/ZRE/schedule-departures").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json.as_array().unwrap().len(), 1, "{json}");
        assert_eq!(json[0]["uid"], "SDLBNEW");
        let (status, json) = get_board(&pool, "/stations/ZRF/schedule-departures").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json[0]["uid"], "SDLBOLD");

        clear_board_fixtures(&pool).await;
    }

    /// Yesterday's board between London midnight and the 02:00 rollover:
    /// only the calls after its midnight that are still to come.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                schedule_departures -- --ignored --test-threads=1`"]
    async fn schedule_departures_yesterdays_computed_board_keeps_only_after_midnight_calls_to_come()
    {
        let pool = connect().await;
        clear_board_fixtures(&pool).await;
        let today: chrono::NaiveDate = "2026-11-12".parse().unwrap();
        let yesterday = today.pred_opt().unwrap();
        let _now =
            crate::routes::pin_london_now_for_tests(today.and_hms_opt(0, 30, 0).unwrap().and_utc());
        seed_timetable_call(&pool, yesterday, "ZRE", "SDLBY1", "23:40:00", 0, true).await;
        seed_timetable_call(&pool, yesterday, "ZRE", "SDLBY2", "00:10:00", 1, true).await;
        seed_timetable_call(&pool, yesterday, "ZRE", "SDLBY3", "00:50:00", 1, true).await;

        let (status, json) = get_board(
            &pool,
            &format!("/stations/ZRE/schedule-departures?date={yesterday}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
        assert_eq!(json.as_array().unwrap().len(), 1, "{json}");
        assert_eq!(json[0]["uid"], "SDLBY3");
        assert_eq!(json[0]["dayOffset"], 1);

        clear_board_fixtures(&pool).await;
    }
}
