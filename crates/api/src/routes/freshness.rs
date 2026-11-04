//! `/public/freshness`: how fresh the data sources feeding the status API
//! are: stations reference data, TOC reference data, the raw incidents
//! feed, the `TfL` line-status feed, the CIF SCHEDULE feed recorded by
//! `schedule-ingest`, the Network Rail CORPUS extract `schedule-ingest`
//! loads, and the three island-of-Ireland feeds (the Republic timetable
//! and live boards, the Northern Ireland stations), which are `null` on an
//! install that does not run them. Unauthenticated, read-only, same
//! `public_router()` pattern as `reference.rs`. Two queries, run together:
//! `queries::data_freshness` for the GB sources and
//! `island_of_ireland_freshness` (below) for the Irish ones. GB
//! station-samples is deliberately omitted: it's per-station polling data,
//! not one of the sources this endpoint reports on.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::app::{App, Router};
use crate::data::queries;

pub fn router() -> Router {
    Router::new().route("/freshness", axum::routing::get(get_freshness))
}

#[derive(Debug, Serialize, PartialEq, ts_rs::TS)]
pub struct DataFreshness {
    pub stations: Option<DateTime<Utc>>,
    pub tocs: Option<DateTime<Utc>>,
    pub incidents: Option<DateTime<Utc>>,
    /// When `TfL` line status last landed. Unlike its three siblings this is
    /// not a poller-fed raw table but the `computed_at` of the TfL-owned
    /// `line_status` rows themselves — for this source, ingest and
    /// computation are the same event.
    pub tfl: Option<DateTime<Utc>>,
    /// When a CIF SCHEDULE feed delivery was last recorded by
    /// `schedule-ingest`'s push to `/private/schedule-feed-ingests`. Its own
    /// new source, not previously reported by this endpoint.
    pub schedule_feed: Option<DateTime<Utc>>,
    /// The `delivered_at` (the delivered file's own mtime) of the newest
    /// Network Rail CORPUS extract loaded into `corpus_locations`
    /// (`MAX(corpus_deliveries.delivered_at)`, the same read as
    /// `data::corpus::last_corpus_delivery`). `None` until the first load,
    /// and for as long as `scheduleFeed.corpus.enabled` stays off.
    pub corpus: Option<DateTime<Utc>>,
    /// When the Republic of Ireland timetable (Irish Rail's services from
    /// the National Transport Authority's GTFS, poller-irish-rail-gtfs,
    /// daily) last landed: the newer of its stations and lines
    /// `ingest_freshness` sources. `None` until its first delivery, and
    /// for as long as the feed is off on this install, so a client can
    /// leave it out.
    pub irish_rail_timetable: Option<DateTime<Utc>>,
    /// When the Irish Rail live departure boards (poller-irish-rail-live,
    /// every 5 minutes) were last polled: the newest Republic of Ireland
    /// station sample's `polled_at`. `None` until the first poll, and while
    /// the feed is off.
    pub irish_rail_live: Option<DateTime<Utc>>,
    /// When the Northern Ireland stations and lines (Translink's, from
    /// `OpenDataNI`, poller-nir-stations, daily) last landed, as
    /// `irish_rail_timetable`. `None` until the first delivery, and while
    /// the feed is off.
    pub northern_ireland_stations: Option<DateTime<Utc>>,
}

async fn get_freshness(
    State(app): State<App>,
) -> Result<Json<DataFreshness>, (StatusCode, String)> {
    let (
        [stations, tocs, incidents, tfl, schedule_feed, corpus],
        [
            irish_rail_timetable,
            irish_rail_live,
            northern_ireland_stations,
        ],
    ) = tokio::try_join!(
        queries::data_freshness(&app.database),
        island_of_ireland_freshness(&app.database),
    )
    .map_err(internal_error)?;
    Ok(Json(DataFreshness {
        stations,
        tocs,
        incidents,
        tfl,
        schedule_feed,
        corpus,
        irish_rail_timetable,
        irish_rail_live,
        northern_ireland_stations,
    }))
}

/// The island-of-Ireland feeds' timestamps in one query:
/// `[irish_rail_timetable, irish_rail_live, northern_ireland_stations]`.
///
/// - The Republic timetable (poller-irish-rail-gtfs, daily) and the
///   Northern Ireland stations (poller-nir-stations, daily) each record a
///   stations and a lines `ingest_freshness` source per network
///   (`ds_store::samples::island_of_ireland::sources`); a feed's time is
///   the newer of its two.
/// - The Republic live boards (poller-irish-rail-live, every 5 minutes)
///   record no freshness source, but each poll moves every polled
///   station's `polled_at`, so the newest Republic sample is the last
///   poll. The table is one row per station (a few hundred at most).
///
/// Each is `None` until its feed first reports, which on an install that
/// runs without it is always.
async fn island_of_ireland_freshness(
    pool: &sqlx::PgPool,
) -> anyhow::Result<[Option<DateTime<Utc>>; 3]> {
    use ds_store::samples::island_of_ireland::{network_wire, sources};
    type Ts = Option<DateTime<Utc>>;
    let row: (Ts, Ts, Ts) = sqlx::query_as(
        "SELECT \
            (SELECT MAX(fetched_at) FROM ingest_freshness WHERE source = ANY($1)), \
            (SELECT MAX(polled_at) FROM island_of_ireland_station_samples WHERE network = $3), \
            (SELECT MAX(fetched_at) FROM ingest_freshness WHERE source = ANY($2))",
    )
    .bind([sources::STATIONS_GTFS, sources::LINES_GTFS])
    .bind([sources::STATIONS_NIR, sources::LINES_NIR])
    .bind(network_wire(
        common::island_of_ireland::IslandOfIrelandNetwork::RepublicOfIreland,
    ))
    .fetch_one(pool)
    .await?;
    Ok([row.0, row.1, row.2])
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used as a map_err callback, which passes the error by value"
)]
fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    if let Some(unavailable) = crate::unavailable::response_for(&err) {
        return unavailable;
    }
    tracing::error!(error = ?err, "data freshness query failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "query failed".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn serializes_missing_data_as_null() {
        let freshness = DataFreshness {
            stations: None,
            tocs: None,
            incidents: None,
            tfl: None,
            schedule_feed: None,
            corpus: None,
            irish_rail_timetable: None,
            irish_rail_live: None,
            northern_ireland_stations: None,
        };
        let json = serde_json::to_value(&freshness).unwrap();
        assert!(json["stations"].is_null());
        assert!(json["tocs"].is_null());
        assert!(json["incidents"].is_null());
        assert!(json["tfl"].is_null());
        assert!(json["schedule_feed"].is_null());
        assert!(json["corpus"].is_null());
        assert!(json["irish_rail_timetable"].is_null());
        assert!(json["irish_rail_live"].is_null());
        assert!(json["northern_ireland_stations"].is_null());
    }

    #[test]
    fn round_trips_a_present_timestamp() {
        let ts = Utc.with_ymd_and_hms(2026, 7, 15, 9, 0, 0).unwrap();
        let freshness = DataFreshness {
            stations: Some(ts),
            tocs: None,
            incidents: None,
            tfl: None,
            schedule_feed: Some(ts),
            corpus: Some(ts),
            irish_rail_timetable: None,
            irish_rail_live: Some(ts),
            northern_ireland_stations: None,
        };
        let json = serde_json::to_value(&freshness).unwrap();
        let roundtripped: DateTime<Utc> = json["stations"].as_str().unwrap().parse().unwrap();
        assert_eq!(roundtripped, ts);
        let schedule_feed_roundtripped: DateTime<Utc> =
            json["schedule_feed"].as_str().unwrap().parse().unwrap();
        assert_eq!(schedule_feed_roundtripped, ts);
        let corpus_roundtripped: DateTime<Utc> = json["corpus"].as_str().unwrap().parse().unwrap();
        assert_eq!(corpus_roundtripped, ts);
        let live_roundtripped: DateTime<Utc> =
            json["irish_rail_live"].as_str().unwrap().parse().unwrap();
        assert_eq!(live_roundtripped, ts);
    }
}

/// Database-gated, one throwaway database per test (`#[sqlx::test]`): the
/// CORPUS case loads through `replace_corpus_locations`, which replaces the
/// WHOLE `corpus_locations` table, so it must never share a database that
/// may hold a real extract.
#[cfg(test)]
mod db_tests {
    use axum::body::Body;
    use axum::http::Request;
    use chrono::TimeZone;
    use sqlx::PgPool;
    use tower::ServiceExt;

    use super::*;
    use crate::app::{App, AppState};
    use crate::auth::oidc::{OidcClient, OidcConfig};
    use crate::data::config::{LineCatalogue, ServiceArguments};
    use crate::data::corpus::{CorpusLocation, replace_corpus_locations};

    /// Copied from `routes::reference::db_tests::test_app` (colocated
    /// per-file, like its siblings). Every field an inert placeholder except
    /// `database` -- this route touches nothing else on `App`.
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

    async fn get_freshness_json(pool: &PgPool) -> serde_json::Value {
        let app: axum::Router = Router::new()
            .merge(router())
            .with_state(test_app(pool.clone()));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/freshness")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn corpus_is_null_before_any_delivery(pool: PgPool) {
        let json = get_freshness_json(&pool).await;
        assert!(
            json.get("corpus").is_some(),
            "corpus key must always be present: {json}"
        );
        assert!(json["corpus"].is_null(), "no delivery yet: {json}");
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn corpus_reports_the_newest_delivery(pool: PgPool) {
        let older = Utc.with_ymd_and_hms(2026, 8, 1, 3, 0, 0).unwrap();
        let newer = Utc.with_ymd_and_hms(2026, 9, 1, 3, 0, 0).unwrap();
        let location = CorpusLocation {
            nlc: "559500".to_string(),
            stanox: Some("87219".to_string()),
            tiploc: Some("CLPHMJN".to_string()),
            crs: Some("CLJ".to_string()),
            uic: None,
            nlc_desc: Some("CLAPHAM JUNCTION LONDON".to_string()),
            nlc_desc16: None,
        };
        // Loaded newest first: the field is the newest delivery, not the
        // most recent load.
        for delivered_at in [newer, older] {
            replace_corpus_locations(
                &pool,
                delivered_at,
                "CORPUSExtract.json.gz",
                std::slice::from_ref(&location),
            )
            .await
            .unwrap();
        }

        let json = get_freshness_json(&pool).await;
        let corpus: DateTime<Utc> = json["corpus"].as_str().unwrap().parse().unwrap();
        assert_eq!(corpus, newer);
        assert_eq!(
            crate::data::corpus::refresh_last_delivery_metric(&pool)
                .await
                .unwrap(),
            Some(newer)
        );
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn island_of_ireland_sources_are_null_until_they_report(pool: PgPool) {
        let json = get_freshness_json(&pool).await;
        for key in [
            "irish_rail_timetable",
            "irish_rail_live",
            "northern_ireland_stations",
        ] {
            assert!(
                json.get(key).is_some(),
                "{key} must always be present: {json}"
            );
            assert!(json[key].is_null(), "{key}: nothing reported yet: {json}");
        }
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn island_of_ireland_sources_report_their_newest_data(pool: PgPool) {
        use ds_store::samples::island_of_ireland::sources;

        let gtfs_stations = Utc.with_ymd_and_hms(2026, 11, 3, 4, 0, 0).unwrap();
        let gtfs_lines = Utc.with_ymd_and_hms(2026, 11, 3, 4, 5, 0).unwrap();
        let nir_stations = Utc.with_ymd_and_hms(2026, 11, 2, 5, 0, 0).unwrap();
        for (source, at) in [
            (sources::STATIONS_GTFS, gtfs_stations),
            (sources::LINES_GTFS, gtfs_lines),
            (sources::STATIONS_NIR, nir_stations),
        ] {
            sqlx::query("INSERT INTO ingest_freshness (source, fetched_at) VALUES ($1, $2)")
                .bind(source)
                .bind(at)
                .execute(&pool)
                .await
                .unwrap();
        }
        let live_older = Utc.with_ymd_and_hms(2026, 11, 3, 9, 0, 0).unwrap();
        let live_newer = Utc.with_ymd_and_hms(2026, 11, 3, 9, 5, 0).unwrap();
        // A Northern Ireland sample (none today) is not the Republic's
        // live feed, however new.
        let other_network = Utc.with_ymd_and_hms(2026, 11, 3, 10, 0, 0).unwrap();
        for (station, network, at) in [
            ("ZLIVE1", "republic-of-ireland", live_older),
            ("ZLIVE2", "republic-of-ireland", live_newer),
            ("ZLIVE3", "northern-ireland", other_network),
        ] {
            sqlx::query(
                "INSERT INTO island_of_ireland_station_samples (station_id, network, polled_at) \
                 VALUES ($1, $2, $3)",
            )
            .bind(station)
            .bind(network)
            .bind(at)
            .execute(&pool)
            .await
            .unwrap();
        }

        let json = get_freshness_json(&pool).await;
        let field = |key: &str| -> DateTime<Utc> { json[key].as_str().unwrap().parse().unwrap() };
        assert_eq!(field("irish_rail_timetable"), gtfs_lines);
        assert_eq!(field("irish_rail_live"), live_newer);
        assert_eq!(field("northern_ireland_stations"), nir_stations);
        assert!(json["stations"].is_null(), "GB sources untouched: {json}");
    }
}
