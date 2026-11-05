//! `GET /public/island-of-ireland/stations`, `/lines`: read-only listing of
//! the Iarnród Éireann (and, once built, NIR) station/line catalogue.
//! `GET /public/island-of-ireland/stations/{id}/departures`: raw
//! pass-through of the live `island_of_ireland_station_samples` board
//! (Tier B), for a catalogue station id (through its `liveCode`) or a live
//! `StationCode`. Unauthenticated, read-only, no pagination -- the whole
//! catalogue is a few hundred rows at most. The web app's Irish Rail
//! pages read them (docs/api-changelog.md, 2026-11-04).
//! `GET /public/island-of-ireland/trains/{trainCode}/{date}`: one Iarnród
//! Éireann train's board history (`island_of_ireland_trains` and its
//! calls), read with `common::island_of_ireland::train_state`, for the web
//! app's train page (distant-signal's
//! docs/superpowers/specs/2026-11-05-irish-rail-train-tracking-design.md
//! §5.2). A train no board has shown is not found: unknown, never
//! "on time".

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use common::island_of_ireland::train_state::{self, LastLocation, LiveState};
use common::island_of_ireland::{IslandOfIrelandDeparture, IslandOfIrelandNetwork};
use serde::{Deserialize, Serialize};

use crate::app::{App, Router};
use crate::data::island_of_ireland::{
    self, IslandOfIrelandLineResponse, IslandOfIrelandStationResponse,
};

/// `Cache-Control` for the two whole-catalogue dumps below
/// (`list_stations`/`list_lines`) -- same rationale and value as
/// `routes::stanox_crs::STANOX_CRS_CACHE_CONTROL` (see that constant's doc
/// comment): a slow-changing reference catalogue served in full to
/// anonymous callers with no cache header at all is cheap bandwidth/DB
/// amplification (finding "Whole-table dumps served uncached to anonymous
/// callers"). Deliberately NOT applied to `get_station_departures` below --
/// that route is a live departure board (`island_of_ireland_station_samples`),
/// re-polled continuously, not a reference table, so caching it would show
/// stale live data instead of merely saving a cheap query.
const CATALOGUE_CACHE_CONTROL: &str = "public, max-age=3600";

pub fn router() -> Router {
    Router::new()
        .route(
            "/island-of-ireland/stations",
            axum::routing::get(list_stations),
        )
        .route("/island-of-ireland/lines", axum::routing::get(list_lines))
        .route(
            "/island-of-ireland/stations/{id}/departures",
            axum::routing::get(get_station_departures),
        )
        .route(
            "/island-of-ireland/trains/{train_code}/{date}",
            axum::routing::get(get_train),
        )
}

#[derive(Debug, Deserialize)]
struct NetworkFilter {
    #[serde(default)]
    network: Option<String>,
}

#[expect(
    clippy::ref_option,
    reason = "callers hold the Option by reference in a struct field"
)]
fn parse_network(
    raw: &Option<String>,
) -> Result<Option<IslandOfIrelandNetwork>, (StatusCode, String)> {
    match raw.as_deref() {
        None => Ok(None),
        Some("republic-of-ireland") => Ok(Some(IslandOfIrelandNetwork::RepublicOfIreland)),
        Some("northern-ireland") => Ok(Some(IslandOfIrelandNetwork::NorthernIreland)),
        Some(other) => Err((
            StatusCode::BAD_REQUEST,
            format!("unrecognized network filter: {other}"),
        )),
    }
}

async fn list_stations(
    State(app): State<App>,
    Query(filter): Query<NetworkFilter>,
) -> Result<
    (
        [(header::HeaderName, &'static str); 1],
        Json<Vec<IslandOfIrelandStationResponse>>,
    ),
    (StatusCode, String),
> {
    let network = parse_network(&filter.network)?;
    let stations = island_of_ireland::list_stations(&app.database, network)
        .await
        .map_err(internal_error)?;
    Ok((
        [(header::CACHE_CONTROL, CATALOGUE_CACHE_CONTROL)],
        Json(stations),
    ))
}

async fn list_lines(
    State(app): State<App>,
    Query(filter): Query<NetworkFilter>,
) -> Result<
    (
        [(header::HeaderName, &'static str); 1],
        Json<Vec<IslandOfIrelandLineResponse>>,
    ),
    (StatusCode, String),
> {
    let network = parse_network(&filter.network)?;
    let lines = island_of_ireland::list_lines(&app.database, network)
        .await
        .map_err(internal_error)?;
    Ok((
        [(header::CACHE_CONTROL, CATALOGUE_CACHE_CONTROL)],
        Json(lines),
    ))
}

/// `id` is a catalogue station id, answered from its `liveCode`'s board,
/// or a live `StationCode` itself. 404 when the station has no live code,
/// or `island_of_ireland_station_samples` has no row for the code yet --
/// identical honesty split to `routes::departures::get_station_departures`.
/// `200 []` is the same "row exists, board is genuinely empty right now"
/// fact that route already draws.
///
/// The body stays a bare array; the board's age is in the headers:
/// `X-Polled-At` (the sample's `polled_at`, RFC 3339) and `Last-Modified`
/// (the same instant as an HTTP date).
async fn get_station_departures(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<
    (
        [(header::HeaderName, String); 2],
        Json<Vec<IslandOfIrelandDepartureResponse>>,
    ),
    (StatusCode, String),
> {
    let Some(code) = island_of_ireland::live_board_key(&app.database, &id)
        .await
        .map_err(internal_error)?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("island-of-ireland station has no live departure board: {id}"),
        ));
    };
    let Some(sample) = island_of_ireland::latest_station_sample(&app.database, &code)
        .await
        .map_err(internal_error)?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no sample data collected for island-of-ireland station: {id}"),
        ));
    };

    Ok((
        polled_at_headers(sample.polled_at),
        Json(sample.departures.iter().map(departure_json).collect()),
    ))
}

/// The header carrying a board's `polled_at` as RFC 3339.
pub(crate) const POLLED_AT_HEADER: &str = "x-polled-at";

/// `X-Polled-At` and `Last-Modified` for a board polled at `polled_at`.
fn polled_at_headers(
    polled_at: chrono::DateTime<chrono::Utc>,
) -> [(header::HeaderName, String); 2] {
    [
        (
            header::HeaderName::from_static(POLLED_AT_HEADER),
            polled_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
        (
            header::LAST_MODIFIED,
            polled_at.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        ),
    ]
}

/// One row of `GET /public/island-of-ireland/stations/{id}/departures`:
/// camelCase, matching `render::station_departure_json`'s convention for a
/// public departures endpoint, every absent value a present `null`. Times
/// are the upstream's own `HH:MM` strings.
#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IslandOfIrelandDepartureResponse {
    train_code: String,
    origin: String,
    destination: String,
    scheduled_arrival: Option<String>,
    scheduled_departure: Option<String>,
    expected_arrival: Option<String>,
    expected_departure: Option<String>,
    late_minutes: i32,
    status: String,
    due_in_minutes: Option<i32>,
    /// The day the train started (`YYYY-MM-DD`): with `trainCode`, its
    /// train page. `null` until the poller sends it.
    #[ts(type = "string | null")]
    train_date: Option<chrono::NaiveDate>,
    /// When the train leaves its origin and reaches its destination,
    /// `HH:MM`.
    origin_time: Option<String>,
    destination_time: Option<String>,
    /// The feed's last report, e.g. `Departed Dundalk`; `null` when none.
    last_location: Option<String>,
}

fn departure_json(d: &IslandOfIrelandDeparture) -> IslandOfIrelandDepartureResponse {
    IslandOfIrelandDepartureResponse {
        train_code: d.train_code.clone(),
        origin: d.origin.clone(),
        destination: d.destination.clone(),
        scheduled_arrival: d.scheduled_arrival.clone(),
        scheduled_departure: d.scheduled_departure.clone(),
        expected_arrival: d.expected_arrival.clone(),
        expected_departure: d.expected_departure.clone(),
        late_minutes: d.late_minutes,
        status: d.status.clone(),
        due_in_minutes: d.due_in_minutes,
        train_date: d.train_date,
        origin_time: d.origin_time.clone(),
        destination_time: d.destination_time.clone(),
        last_location: d.last_location.clone(),
    }
}

/// `GET /public/island-of-ireland/trains/{trainCode}/{date}`.
#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IslandOfIrelandTrainResponse {
    #[ts(type = "\"northern-ireland\" | \"republic-of-ireland\"")]
    #[schemars(with = "crate::ts_values::IslandOfIrelandNetworkWire")]
    network: IslandOfIrelandNetwork,
    train_code: String,
    /// The day the train started, `YYYY-MM-DD`.
    #[ts(type = "string")]
    service_date: chrono::NaiveDate,
    origin: String,
    destination: String,
    /// `HH:MM`, Irish local time; `null` when the boards didn't say.
    origin_time: Option<String>,
    destination_time: Option<String>,
    live: IslandOfIrelandTrainLive,
    /// The stations whose boards showed it, in running order.
    calls: Vec<IslandOfIrelandTrainCall>,
    /// When a board first and last showed the train.
    #[ts(type = "string")]
    first_seen_at: chrono::DateTime<chrono::Utc>,
    #[ts(type = "string")]
    last_seen_at: chrono::DateTime<chrono::Utc>,
}

/// The train's state as the boards show it (`train_state::interpret`).
#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IslandOfIrelandTrainLive {
    state: LiveState,
    /// Minutes late (negative: early), only while the train reports;
    /// `null` otherwise. Never a guess.
    late_minutes: Option<i32>,
    last_location: Option<LastLocation>,
    /// When a board last showed the train.
    #[ts(type = "string")]
    observed_at: chrono::DateTime<chrono::Utc>,
}

/// One station whose board showed the train, with that board's times
/// (`HH:MM`, Irish local time; `null` where the board had none).
#[derive(Debug, Clone, PartialEq, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IslandOfIrelandTrainCall {
    /// The live board's `StationCode`.
    station_code: String,
    /// The catalogue station (its page), when one has this live code.
    station_id: Option<String>,
    /// The catalogue name, else the live list's; `null` when neither knows
    /// the code.
    name: Option<String>,
    scheduled_arrival: Option<String>,
    scheduled_departure: Option<String>,
    expected_arrival: Option<String>,
    expected_departure: Option<String>,
}

/// The train code as the history stores it: trimmed, upper-cased, 1 to 8
/// letters and digits (the tables' `CHECK`).
fn parse_train_code(raw: &str) -> Option<String> {
    let code = raw.trim().to_ascii_uppercase();
    ((1..=8).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric()))
        .then_some(code)
}

pub(crate) async fn get_train(
    State(app): State<App>,
    Path((train_code, date)): Path<(String, String)>,
) -> Result<
    (
        [(header::HeaderName, &'static str); 1],
        Json<IslandOfIrelandTrainResponse>,
    ),
    (StatusCode, String),
> {
    let Some(code) = parse_train_code(&train_code) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "train codes are 1 to 8 letters and digits, like A120".to_string(),
        ));
    };
    let Ok(date) = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d") else {
        return Err((StatusCode::BAD_REQUEST, "dates are YYYY-MM-DD".to_string()));
    };
    let Some(history) = island_of_ireland::train_history(
        &app.database,
        IslandOfIrelandNetwork::RepublicOfIreland,
        &code,
        date,
    )
    .await
    .map_err(internal_error)?
    else {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no departure board has shown train {code} on {date}"),
        ));
    };
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(train_json(history, chrono::Utc::now())),
    ))
}

/// The response for `history` at `now`: the live state read once, and the
/// calls in running order (by timetabled time, departure before arrival,
/// a time before the origin's counted as the next day; a call with no time
/// last, then by station code).
fn train_json(
    history: island_of_ireland::TrainHistory,
    now: chrono::DateTime<chrono::Utc>,
) -> IslandOfIrelandTrainResponse {
    let read = train_state::interpret(
        &train_state::Observation {
            status: &history.status,
            late_minutes: history.late_minutes,
            last_location: history.last_location.as_deref(),
            destination: &history.destination,
            last_seen_at: history.last_seen_at,
        },
        now,
    );
    let origin_time = history.origin_time.as_deref();
    let order = |c: &island_of_ireland::TrainHistoryCall| {
        let time = c
            .scheduled_departure
            .as_deref()
            .or(c.scheduled_arrival.as_deref())
            .or(c.expected_departure.as_deref())
            .or(c.expected_arrival.as_deref());
        (
            time.and_then(|t| train_state::running_order_minutes(t, origin_time))
                .unwrap_or(u32::MAX),
            c.station_code.clone(),
        )
    };
    let mut calls = history.calls;
    calls.sort_by_cached_key(order);
    IslandOfIrelandTrainResponse {
        network: history.network,
        train_code: history.train_code,
        service_date: history.train_date,
        origin: history.origin,
        destination: history.destination,
        origin_time: history.origin_time,
        destination_time: history.destination_time,
        live: IslandOfIrelandTrainLive {
            state: read.state,
            late_minutes: read.late_minutes,
            last_location: read.last_location,
            observed_at: history.last_seen_at,
        },
        calls: calls
            .into_iter()
            .map(|c| IslandOfIrelandTrainCall {
                station_code: c.station_code,
                station_id: c.station_id,
                name: c.name,
                scheduled_arrival: c.scheduled_arrival,
                scheduled_departure: c.scheduled_departure,
                expected_arrival: c.expected_arrival,
                expected_departure: c.expected_departure,
            })
            .collect(),
        first_seen_at: history.first_seen_at,
        last_seen_at: history.last_seen_at,
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
    tracing::error!(error = ?err, "island-of-ireland catalogue query failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "query failed".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_network_accepts_both_known_values_and_none() {
        assert_eq!(parse_network(&None).unwrap(), None);
        assert_eq!(
            parse_network(&Some("republic-of-ireland".to_string())).unwrap(),
            Some(IslandOfIrelandNetwork::RepublicOfIreland)
        );
        assert_eq!(
            parse_network(&Some("northern-ireland".to_string())).unwrap(),
            Some(IslandOfIrelandNetwork::NorthernIreland)
        );
    }

    #[test]
    fn parse_network_rejects_unknown_values() {
        assert!(parse_network(&Some("mars".to_string())).is_err());
    }

    #[test]
    fn the_board_age_headers_carry_polled_at() {
        let polled_at = "2026-11-04T09:05:03.250Z".parse().unwrap();
        let [(name, value), (last_name, last_value)] = polled_at_headers(polled_at);
        assert_eq!(name.as_str(), "x-polled-at");
        assert_eq!(value, "2026-11-04T09:05:03Z");
        assert_eq!(last_name, header::LAST_MODIFIED);
        assert_eq!(last_value, "Wed, 04 Nov 2026 09:05:03 GMT");
    }

    #[test]
    fn train_codes_are_trimmed_upper_cased_and_bounded() {
        assert_eq!(parse_train_code(" a120 ").as_deref(), Some("A120"));
        assert_eq!(parse_train_code("E101").as_deref(), Some("E101"));
        assert_eq!(parse_train_code(""), None);
        assert_eq!(parse_train_code("ie:A120"), None);
        assert_eq!(parse_train_code("ABCDEFGHI"), None);
    }

    fn history_call(
        code: &str,
        arrival: Option<&str>,
        departure: Option<&str>,
    ) -> island_of_ireland::TrainHistoryCall {
        island_of_ireland::TrainHistoryCall {
            station_code: code.into(),
            station_id: None,
            name: Some(code.into()),
            scheduled_arrival: arrival.map(Into::into),
            scheduled_departure: departure.map(Into::into),
            expected_arrival: arrival.map(Into::into),
            expected_departure: departure.map(Into::into),
        }
    }

    fn history(
        status: &str,
        late: i32,
        last_location: Option<&str>,
    ) -> island_of_ireland::TrainHistory {
        island_of_ireland::TrainHistory {
            network: IslandOfIrelandNetwork::RepublicOfIreland,
            train_code: "A120".into(),
            train_date: chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap(),
            origin: "Dublin Connolly".into(),
            destination: "Belfast".into(),
            origin_time: Some("23:30".into()),
            destination_time: Some("01:40".into()),
            status: status.into(),
            late_minutes: Some(late),
            last_location: last_location.map(Into::into),
            first_seen_at: "2026-11-05T22:00:00Z".parse().unwrap(),
            last_seen_at: "2026-11-05T23:50:00Z".parse().unwrap(),
            calls: vec![
                history_call("BFSTC", Some("01:40"), None),
                history_call("NOTIM", None, None),
                history_call("DDALK", Some("00:25"), Some("00:26")),
                history_call("CNLLY", None, Some("23:30")),
                history_call("DGHDA", Some("23:58"), Some("23:59")),
            ],
        }
    }

    /// Calls come in running order past midnight, a call with no time
    /// last; the live state is `train_state`'s.
    #[test]
    fn train_json_orders_the_calls_and_reads_the_live_state() {
        let now = "2026-11-05T23:52:00Z".parse().unwrap();
        let json = train_json(history("En Route", 3, Some("Departed Drogheda")), now);
        let codes: Vec<&str> = json.calls.iter().map(|c| c.station_code.as_str()).collect();
        assert_eq!(codes, ["CNLLY", "DGHDA", "DDALK", "BFSTC", "NOTIM"]);
        let value = serde_json::to_value(&json).unwrap();
        assert_eq!(
            value["live"],
            serde_json::json!({
                "state": "running",
                "lateMinutes": 3,
                "lastLocation": {"event": "departed", "place": "Drogheda"},
                "observedAt": "2026-11-05T23:50:00Z",
            })
        );
        assert_eq!(value["serviceDate"], "2026-11-05");
        assert_eq!(value["network"], "republic-of-ireland");
    }

    /// "No Information" with `Late` 0 is no report and no delay, not "on
    /// time".
    #[test]
    fn a_train_with_no_report_has_no_delay() {
        let now = "2026-11-05T23:52:00Z".parse().unwrap();
        let json = train_json(history("No Information", 0, None), now);
        assert_eq!(json.live.state, LiveState::NoReport);
        assert_eq!(json.live.late_minutes, None);
    }

    #[test]
    fn departure_json_maps_every_field_to_camel_case() {
        let departure = IslandOfIrelandDeparture {
            train_code: "A101".to_string(),
            origin: "Belfast".to_string(),
            destination: "Dublin Connolly".to_string(),
            scheduled_arrival: None,
            scheduled_departure: Some("06:00".to_string()),
            expected_arrival: None,
            expected_departure: Some("06:00".to_string()),
            late_minutes: 0,
            status: "On Time".to_string(),
            due_in_minutes: Some(5),
            train_date: chrono::NaiveDate::from_ymd_opt(2026, 11, 5),
            origin_time: Some("06:00".to_string()),
            destination_time: Some("08:15".to_string()),
            last_location: None,
        };
        let json = serde_json::to_value(departure_json(&departure)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "trainCode": "A101",
                "origin": "Belfast",
                "destination": "Dublin Connolly",
                "scheduledArrival": null,
                "scheduledDeparture": "06:00",
                "expectedArrival": null,
                "expectedDeparture": "06:00",
                "lateMinutes": 0,
                "status": "On Time",
                "dueInMinutes": 5,
                "trainDate": "2026-11-05",
                "originTime": "06:00",
                "destinationTime": "08:15",
                "lastLocation": null,
            })
        );
    }
}

/// HTTP-layer tests exercised against a live database -- mirrors
/// `routes::stanox_crs::db_tests`'s `test_app` helper (colocated per-file
/// rather than shared, matching this crate's established convention -- see
/// that module's own doc comment for the same reasoning stated the first
/// time this was copied).
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
            stale_push_subscription_days: 365,
            inactive_account_retention_days: 0,
        };

        std::sync::Arc::new(AppState {
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

    /// Regression for "Whole-table dumps served uncached to anonymous
    /// callers": both whole-catalogue routes must carry a public,
    /// positive-max-age `Cache-Control`, independent of whether the
    /// underlying tables have any rows -- an empty catalogue is still a
    /// catalogue dump, not a per-caller answer.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                island_of_ireland_catalogue_routes_set_cache_control \
                -- --ignored --test-threads=1`"]
    async fn island_of_ireland_catalogue_routes_set_cache_control() {
        let pool = connect().await;
        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));

        for uri in ["/island-of-ireland/stations", "/island-of-ireland/lines"] {
            let response = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "uri: {uri}");
            assert_eq!(
                response
                    .headers()
                    .get(header::CACHE_CONTROL)
                    .and_then(|v| v.to_str().ok()),
                Some(CATALOGUE_CACHE_CONTROL),
                "uri {uri} must carry a public, positive-max-age Cache-Control"
            );
        }
    }

    /// A catalogue id answers from its live code's board, with the board's
    /// age in the headers; a live code still works; a station without a
    /// live code is a 404.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                departures_resolve_a_catalogue_id -- --ignored --test-threads=1`"]
    async fn departures_resolve_a_catalogue_id_through_its_live_code() {
        let pool = connect().await;
        for sql in [
            "DELETE FROM island_of_ireland_stations WHERE id LIKE 'ZQRT%'",
            "DELETE FROM island_of_ireland_station_samples WHERE station_id LIKE 'ZQRT%'",
            "INSERT INTO island_of_ireland_stations (id, name, network, live_code) VALUES \
             ('ZQRT1', 'Zesttown', 'republic-of-ireland', 'ZQRTCODE'), \
             ('ZQRT2', 'Zorough', 'northern-ireland', NULL)",
            "INSERT INTO island_of_ireland_station_samples (station_id, network, polled_at, departures) \
             VALUES ('ZQRTCODE', 'republic-of-ireland', '2026-11-04T09:05:03Z', \
             '[{\"train_code\":\"A1\",\"origin\":\"Zesttown\",\"destination\":\"Zorough\",\
             \"late_minutes\":2,\"status\":\"En Route\"}]')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let get = |uri: &'static str| {
            let router = router.clone();
            async move {
                router
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            }
        };

        for uri in [
            "/island-of-ireland/stations/ZQRT1/departures",
            "/island-of-ireland/stations/ZQRTCODE/departures",
        ] {
            let response = get(uri).await;
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(
                response.headers().get(POLLED_AT_HEADER).unwrap(),
                "2026-11-04T09:05:03Z"
            );
            assert_eq!(
                response.headers().get(header::LAST_MODIFIED).unwrap(),
                "Wed, 04 Nov 2026 09:05:03 GMT"
            );
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json[0]["trainCode"], "A1");
            assert_eq!(json[0]["lateMinutes"], 2);
        }
        for uri in [
            "/island-of-ireland/stations/ZQRT2/departures",
            "/island-of-ireland/stations/ZQRTNONE/departures",
        ] {
            assert_eq!(get(uri).await.status(), StatusCode::NOT_FOUND, "{uri}");
        }

        for sql in [
            "DELETE FROM island_of_ireland_stations WHERE id LIKE 'ZQRT%'",
            "DELETE FROM island_of_ireland_station_samples WHERE station_id LIKE 'ZQRT%'",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
    }

    /// A train the boards showed comes back with its calls joined to the
    /// catalogue (or the live list) and `no-store`; one they didn't is a
    /// 404; a malformed code or date is a 400.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                train_route -- --ignored --test-threads=1`"]
    async fn train_route_serves_the_board_history() {
        let pool = connect().await;
        for sql in [
            "DELETE FROM island_of_ireland_train_calls WHERE train_code = 'ZQ7'",
            "DELETE FROM island_of_ireland_trains WHERE train_code = 'ZQ7'",
            "DELETE FROM island_of_ireland_stations WHERE id = 'ZQRT7'",
            "DELETE FROM island_of_ireland_live_stations WHERE code LIKE 'ZQRT%'",
            "INSERT INTO island_of_ireland_stations (id, name, network, live_code) VALUES \
             ('ZQRT7', 'Zesttown', 'republic-of-ireland', 'ZQRTA')",
            "INSERT INTO island_of_ireland_live_stations (code, name, fetched_at) VALUES \
             ('ZQRTB', 'Zorough Live', now())",
            "INSERT INTO island_of_ireland_trains (network, train_code, train_date, origin, \
             destination, origin_time, destination_time, status, late_minutes, last_location, \
             first_seen_at, last_seen_at, changed_at) VALUES ('republic-of-ireland', 'ZQ7', \
             '2051-01-02', 'Zesttown', 'Zorough', '10:00', '10:40', 'En Route', 5, \
             'Departed Zesttown', now(), now(), now())",
            "INSERT INTO island_of_ireland_train_calls (network, train_code, train_date, \
             station_code, scheduled_arrival, scheduled_departure, expected_arrival, \
             expected_departure, late_minutes, first_seen_at, changed_at) VALUES \
             ('republic-of-ireland', 'ZQ7', '2051-01-02', 'ZQRTB', '10:40', NULL, '10:45', \
              NULL, 5, now(), now()), \
             ('republic-of-ireland', 'ZQ7', '2051-01-02', 'ZQRTA', NULL, '10:00', NULL, \
              '10:05', 5, now(), now())",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let router: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let get = |uri: &'static str| {
            let router = router.clone();
            async move {
                router
                    .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap()
            }
        };

        let response = get("/island-of-ireland/trains/zq7/2051-01-02").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["trainCode"], "ZQ7");
        assert_eq!(json["live"]["state"], "running");
        assert_eq!(json["live"]["lateMinutes"], 5);
        assert_eq!(json["calls"][0]["stationId"], "ZQRT7");
        assert_eq!(json["calls"][0]["name"], "Zesttown");
        assert_eq!(json["calls"][0]["expectedDeparture"], "10:05");
        assert_eq!(json["calls"][1]["stationId"], serde_json::Value::Null);
        assert_eq!(json["calls"][1]["name"], "Zorough Live");

        assert_eq!(
            get("/island-of-ireland/trains/ZQ7/2051-01-03")
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get("/island-of-ireland/trains/ie:ZQ7/2051-01-02")
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            get("/island-of-ireland/trains/ZQ7/02-01-2051")
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );

        for sql in [
            "DELETE FROM island_of_ireland_train_calls WHERE train_code = 'ZQ7'",
            "DELETE FROM island_of_ireland_trains WHERE train_code = 'ZQ7'",
            "DELETE FROM island_of_ireland_stations WHERE id = 'ZQRT7'",
            "DELETE FROM island_of_ireland_live_stations WHERE code LIKE 'ZQRT%'",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
    }
}
