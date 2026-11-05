//! Storage for `island_of_ireland_stations`/`island_of_ireland_lines`
//! (Tier A) and `island_of_ireland_station_samples` (Tier B) --
//! docs/superpowers/specs/2026-09-05-ireland-rail-support-design.md. Same
//! upsert-on-id, no-history shape as `crates/api/src/data/queries.rs`'s
//! `upsert_stations`/`upsert_tocs`.

use anyhow::Result;
use common::island_of_ireland::{
    IslandOfIrelandDeparture, IslandOfIrelandNetwork, IslandOfIrelandStationSample,
};
use sqlx::PgPool;

// Moved to `ds_store::samples::island_of_ireland` (ingest architecture plan 1A.5).
use ds_store::samples::island_of_ireland::network_wire;

fn network_from_wire(wire: &str) -> Result<IslandOfIrelandNetwork> {
    match wire {
        "northern-ireland" => Ok(IslandOfIrelandNetwork::NorthernIreland),
        "republic-of-ireland" => Ok(IslandOfIrelandNetwork::RepublicOfIreland),
        other => anyhow::bail!("unrecognized island_of_ireland network: {other}"),
    }
}

// Moved to `ds_store::samples::island_of_ireland` (ingest architecture plan 1A.5).
pub use ds_store::samples::island_of_ireland::{
    last_lines_fetch, last_stations_fetch, upsert_lines, upsert_stations,
};

/// One row of `GET /public/island-of-ireland/stations`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct IslandOfIrelandStationResponse {
    /// The catalogue id: a GTFS `stop_id` (`7020IR2162`) or an NIR slug
    /// (`nir-bangor`).
    pub id: String,
    /// The display name.
    pub name: String,
    #[ts(type = "\"northern-ireland\" | \"republic-of-ireland\"")]
    pub network: IslandOfIrelandNetwork,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Iarnród Éireann's live-board `StationCode` (`BFSTC`), or `null`
    /// when the station has no live board or no confident match.
    pub live_code: Option<String>,
    /// The source's own name when `name` is a tidied form of it
    /// (`HOLLYWOOD RAIL HALT`), else `null`.
    pub raw_name: Option<String>,
}

/// One row of `GET /public/island-of-ireland/lines`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct IslandOfIrelandLineResponse {
    /// The line id; a corridor the GTFS feed splits by direction has one
    /// row (`DUB-BFT`, not `DUB-BFT-I`/`DUB-BFT-O`).
    pub id: String,
    /// The display name, e.g. `Dublin - Belfast`.
    pub name: String,
    /// The feed's short name, when it has one.
    pub short_name: Option<String>,
    #[ts(type = "\"northern-ireland\" | \"republic-of-ireland\"")]
    pub network: IslandOfIrelandNetwork,
    /// The operator's name.
    pub agency: Option<String>,
    /// `#RRGGBB`, from the feed; `null` when it gives none.
    pub color: Option<String>,
    /// `#RRGGBB` text colour to draw on `color`.
    pub text_color: Option<String>,
    /// Ordered catalogue station ids.
    pub stations: Vec<String>,
}

/// `(id, name, network, latitude, longitude, live_code, raw_name)`.
type StationRow = (
    String,
    String,
    String,
    Option<f64>,
    Option<f64>,
    Option<String>,
    Option<String>,
);

/// Backs `GET /public/island-of-ireland/stations` (Task A3) -- the whole
/// catalogue is small (~150-300 rows across both networks), so this is a
/// plain unpaginated list, optionally filtered by network, ordered by name
/// (then id, so equal names keep one order) -- same ordering choice as
/// `reference::get_all_tocs`.
pub async fn list_stations(
    pool: &PgPool,
    network: Option<IslandOfIrelandNetwork>,
) -> Result<Vec<IslandOfIrelandStationResponse>> {
    let rows: Vec<StationRow> = sqlx::query_as(
        "SELECT id, name, network, latitude, longitude, live_code, raw_name \
         FROM island_of_ireland_stations \
         WHERE $1::text IS NULL OR network = $1 ORDER BY name, id",
    )
    .bind(network.map(network_wire))
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(
            |(id, name, network, latitude, longitude, live_code, raw_name)| {
                Ok(IslandOfIrelandStationResponse {
                    id,
                    name,
                    network: network_from_wire(&network)?,
                    latitude,
                    longitude,
                    live_code,
                    raw_name,
                })
            },
        )
        .collect()
}

/// `(id, name, short_name, network, agency, color, text_color, stations)`.
type LineRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    serde_json::Value,
);

pub async fn list_lines(
    pool: &PgPool,
    network: Option<IslandOfIrelandNetwork>,
) -> Result<Vec<IslandOfIrelandLineResponse>> {
    let rows: Vec<LineRow> = sqlx::query_as(
        "SELECT id, name, short_name, network, agency, color, text_color, stations \
         FROM island_of_ireland_lines \
         WHERE $1::text IS NULL OR network = $1 ORDER BY name, id",
    )
    .bind(network.map(network_wire))
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(
            |(id, name, short_name, network, agency, color, text_color, stations)| {
                Ok(IslandOfIrelandLineResponse {
                    id,
                    name,
                    short_name,
                    network: network_from_wire(&network)?,
                    agency,
                    color,
                    text_color,
                    stations: serde_json::from_value(stations)?,
                })
            },
        )
        .collect()
}

/// The live-board key for `id`: the catalogue station's `live_code` when
/// `id` is a catalogue id, `None` when it is one without a live code, or
/// `id` itself when it is no catalogue id (a live `StationCode`, as the
/// route took before the catalogue carried live codes).
pub async fn live_board_key(pool: &PgPool, id: &str) -> Result<Option<String>> {
    let row: Option<(Option<String>,)> =
        sqlx::query_as("SELECT live_code FROM island_of_ireland_stations WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
    Ok(match row {
        Some((live_code,)) => live_code,
        None => Some(id.to_owned()),
    })
}

// Moved to `ds_store::samples::island_of_ireland` (ingest architecture plan 1A.5).
pub use ds_store::samples::island_of_ireland::{
    last_station_samples_fetch, upsert_station_samples,
};

/// Backs `GET /public/island-of-ireland/stations/{id}/departures` (Task
/// B3) -- raw pass-through, mirrors `queries::latest_station_sample`
/// (`crates/api/src/data/queries.rs:918-934`) exactly, one level down in a
/// different table.
pub async fn latest_station_sample(
    pool: &PgPool,
    station_id: &str,
) -> Result<Option<IslandOfIrelandStationSample>> {
    use sqlx::Row;
    let row = sqlx::query(
        "SELECT station_id, network, polled_at, departures FROM island_of_ireland_station_samples \
         WHERE station_id = $1",
    )
    .bind(station_id)
    .fetch_optional(pool)
    .await?;

    row.map(|row| {
        let network: String = row.try_get("network")?;
        let departures_json: serde_json::Value = row.try_get("departures")?;
        Ok(IslandOfIrelandStationSample {
            station_id: row.try_get("station_id")?,
            network: network_from_wire(&network)?,
            polled_at: row.try_get("polled_at")?,
            departures: serde_json::from_value::<Vec<IslandOfIrelandDeparture>>(departures_json)?,
        })
    })
    .transpose()
}

/// One train's board history: `island_of_ireland_trains` and its
/// `island_of_ireland_train_calls`, as the writer recorded them (ds-core's
/// `record_train_history`). Raw feed values; the route reads them with
/// `common::island_of_ireland::train_state`.
#[derive(Debug, Clone, PartialEq)]
pub struct TrainHistory {
    pub network: IslandOfIrelandNetwork,
    pub train_code: String,
    pub train_date: chrono::NaiveDate,
    pub origin: String,
    pub destination: String,
    pub origin_time: Option<String>,
    pub destination_time: Option<String>,
    pub status: String,
    pub late_minutes: Option<i32>,
    pub last_location: Option<String>,
    pub first_seen_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    /// In no particular order.
    pub calls: Vec<TrainHistoryCall>,
}

/// One station whose board showed the train, with that board's times.
#[derive(Debug, Clone, PartialEq)]
pub struct TrainHistoryCall {
    /// The live `StationCode`.
    pub station_code: String,
    /// The catalogue station whose `live_code` it is, if any.
    pub station_id: Option<String>,
    /// The catalogue station's name, else the live list's.
    pub name: Option<String>,
    pub scheduled_arrival: Option<String>,
    pub scheduled_departure: Option<String>,
    pub expected_arrival: Option<String>,
    pub expected_departure: Option<String>,
}

/// The board history of `train_code` on `train_date` in `network`, or
/// `None` when no board has shown it (or it has been pruned).
pub async fn train_history(
    pool: &PgPool,
    network: IslandOfIrelandNetwork,
    train_code: &str,
    train_date: chrono::NaiveDate,
) -> Result<Option<TrainHistory>> {
    use sqlx::Row;
    let wire = network_wire(network);
    let Some(row) = sqlx::query(
        "SELECT origin, destination, origin_time, destination_time, status, late_minutes, \
                last_location, first_seen_at, last_seen_at \
         FROM island_of_ireland_trains \
         WHERE network = $1 AND train_code = $2 AND train_date = $3",
    )
    .bind(wire)
    .bind(train_code)
    .bind(train_date)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let calls = sqlx::query(
        "SELECT c.station_code, s.id AS station_id, COALESCE(s.name, ls.name) AS name, \
                c.scheduled_arrival, c.scheduled_departure, c.expected_arrival, \
                c.expected_departure \
         FROM island_of_ireland_train_calls c \
         LEFT JOIN LATERAL ( \
             SELECT id, name FROM island_of_ireland_stations \
             WHERE live_code = c.station_code AND network = c.network \
             ORDER BY id LIMIT 1 \
         ) s ON TRUE \
         LEFT JOIN island_of_ireland_live_stations ls ON ls.code = c.station_code \
         WHERE c.network = $1 AND c.train_code = $2 AND c.train_date = $3",
    )
    .bind(wire)
    .bind(train_code)
    .bind(train_date)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|c| {
        Ok(TrainHistoryCall {
            station_code: c.try_get("station_code")?,
            station_id: c.try_get("station_id")?,
            name: c.try_get("name")?,
            scheduled_arrival: c.try_get("scheduled_arrival")?,
            scheduled_departure: c.try_get("scheduled_departure")?,
            expected_arrival: c.try_get("expected_arrival")?,
            expected_departure: c.try_get("expected_departure")?,
        })
    })
    .collect::<Result<Vec<_>>>()?;
    Ok(Some(TrainHistory {
        network,
        train_code: train_code.to_owned(),
        train_date,
        origin: row.try_get("origin")?,
        destination: row.try_get("destination")?,
        origin_time: row.try_get("origin_time")?,
        destination_time: row.try_get("destination_time")?,
        status: row.try_get("status")?,
        late_minutes: row.try_get("late_minutes")?,
        last_location: row.try_get("last_location")?,
        first_seen_at: row.try_get("first_seen_at")?,
        last_seen_at: row.try_get("last_seen_at")?,
        calls,
    }))
}

#[cfg(test)]
mod db_tests {
    use common::island_of_ireland::{IslandOfIrelandLineDefinition, IslandOfIrelandStation};
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    async fn connect() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
        PgPoolOptions::new()
            .connect(&database_url)
            .await
            .expect("connect to postgres")
    }

    async fn delete_fixture_station(pool: &PgPool, id: &str) {
        sqlx::query("DELETE FROM island_of_ireland_stations WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .expect("cleanup fixture station");
    }

    async fn delete_fixture_line(pool: &PgPool, id: &str) {
        sqlx::query("DELETE FROM island_of_ireland_lines WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .expect("cleanup fixture line");
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                island_of_ireland -- --ignored --test-threads=1`"]
    async fn upsert_stations_then_list_round_trips_and_filters_by_network() {
        let pool = connect().await;
        delete_fixture_station(&pool, "ZIOI1").await;
        delete_fixture_station(&pool, "ZIOI2").await;

        let stations = vec![
            IslandOfIrelandStation {
                id: "ZIOI1".to_string(),
                name: "Zesttown".to_string(),
                network: IslandOfIrelandNetwork::RepublicOfIreland,
                latitude: Some(53.0),
                longitude: Some(-6.0),
                raw_name: None,
            },
            IslandOfIrelandStation {
                id: "ZIOI2".to_string(),
                name: "Zorough".to_string(),
                network: IslandOfIrelandNetwork::NorthernIreland,
                latitude: None,
                longitude: None,
                raw_name: None,
            },
        ];
        let upserted = upsert_stations(&pool, &stations).await.expect("upsert");
        assert_eq!(upserted, 2);

        let roi_only = list_stations(&pool, Some(IslandOfIrelandNetwork::RepublicOfIreland))
            .await
            .expect("list roi");
        assert!(roi_only.iter().any(|s| s.id == "ZIOI1"));
        assert!(!roi_only.iter().any(|s| s.id == "ZIOI2"));

        let all = list_stations(&pool, None).await.expect("list all");
        assert!(all.iter().any(|s| s.id == "ZIOI1"));
        assert!(all.iter().any(|s| s.id == "ZIOI2"));

        delete_fixture_station(&pool, "ZIOI1").await;
        delete_fixture_station(&pool, "ZIOI2").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                island_of_ireland -- --ignored --test-threads=1`"]
    async fn upsert_lines_stores_ordered_stations_and_repeat_upsert_replaces_not_duplicates() {
        let pool = connect().await;
        delete_fixture_line(&pool, "ZLINE1").await;

        let first = IslandOfIrelandLineDefinition {
            id: "ZLINE1".to_string(),
            name: "Zest Line".to_string(),
            network: IslandOfIrelandNetwork::RepublicOfIreland,
            stations: vec!["ZIOI1".to_string(), "ZIOI2".to_string()],
            short_name: None,
            agency: None,
            color: None,
            text_color: None,
            replaces: vec![],
        };
        upsert_lines(&pool, &[first]).await.expect("first upsert");

        let second = IslandOfIrelandLineDefinition {
            id: "ZLINE1".to_string(),
            name: "Zest Line (renamed)".to_string(),
            network: IslandOfIrelandNetwork::RepublicOfIreland,
            stations: vec!["ZIOI2".to_string(), "ZIOI1".to_string()],
            short_name: None,
            agency: None,
            color: None,
            text_color: None,
            replaces: vec![],
        };
        upsert_lines(&pool, &[second]).await.expect("second upsert");

        let lines = list_lines(&pool, None).await.expect("list");
        let line = lines
            .iter()
            .find(|l| l.id == "ZLINE1")
            .expect("row present");
        assert_eq!(line.name, "Zest Line (renamed)");
        assert_eq!(
            line.stations,
            vec!["ZIOI2".to_string(), "ZIOI1".to_string()]
        );

        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT id FROM island_of_ireland_lines WHERE id = 'ZLINE1'")
                .fetch_all(&pool)
                .await
                .expect("select");
        assert_eq!(rows.len(), 1, "upsert must replace, not duplicate");

        delete_fixture_line(&pool, "ZLINE1").await;
    }

    /// `xmin` changes exactly when Postgres writes a new row version.
    async fn xmin(pool: &PgPool, table: &str, id: &str) -> String {
        sqlx::query_scalar(&format!("SELECT xmin::text FROM {table} WHERE id = $1"))
            .bind(id)
            .fetch_one(pool)
            .await
            .expect("read xmin")
    }

    /// N1: one batched statement; an unchanged row is not rewritten, a
    /// changed one is, the last duplicate in a batch wins, and the feed's
    /// "last fetched" still advances when nothing changed.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                island_of_ireland -- --ignored --test-threads=1`"]
    async fn upsert_stations_skips_unchanged_rows_and_still_records_the_fetch() {
        let pool = connect().await;
        delete_fixture_station(&pool, "ZIOI3").await;
        delete_fixture_station(&pool, "ZIOI4").await;
        let station = |id: &str, name: &str| IslandOfIrelandStation {
            id: id.to_string(),
            name: name.to_string(),
            network: IslandOfIrelandNetwork::RepublicOfIreland,
            latitude: Some(53.0),
            longitude: None,
            raw_name: None,
        };

        // A duplicate id in one batch: the last entry wins (one statement
        // cannot touch a row twice).
        let first = vec![
            station("ZIOI3", "Old name"),
            station("ZIOI3", "Zedford"),
            station("ZIOI4", "Zeal"),
        ];
        assert_eq!(upsert_stations(&pool, &first).await.unwrap(), 3);
        let all = list_stations(&pool, None).await.unwrap();
        assert_eq!(
            all.iter()
                .find(|s| s.id == "ZIOI3")
                .map(|s| s.name.as_str()),
            Some("Zedford")
        );
        let fetched_first = last_stations_fetch(&pool).await.unwrap().expect("recorded");
        let unchanged_before = xmin(&pool, "island_of_ireland_stations", "ZIOI3").await;
        let changed_before = xmin(&pool, "island_of_ireland_stations", "ZIOI4").await;

        upsert_stations(
            &pool,
            &[station("ZIOI3", "Zedford"), station("ZIOI4", "Zeal Halt")],
        )
        .await
        .unwrap();
        assert_eq!(
            xmin(&pool, "island_of_ireland_stations", "ZIOI3").await,
            unchanged_before,
            "an unchanged station must not be rewritten"
        );
        assert_ne!(
            xmin(&pool, "island_of_ireland_stations", "ZIOI4").await,
            changed_before,
            "a changed station must be rewritten"
        );
        let fetched_second = last_stations_fetch(&pool).await.unwrap().expect("recorded");
        assert!(fetched_second >= fetched_first);
        assert_eq!(upsert_stations(&pool, &[]).await.unwrap(), 0);

        delete_fixture_station(&pool, "ZIOI3").await;
        delete_fixture_station(&pool, "ZIOI4").await;
    }

    /// The live code, raw name and line display fields are read back, and
    /// the departures key resolves a catalogue id through its live code.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                island_of_ireland -- --ignored --test-threads=1`"]
    async fn the_display_fields_and_live_code_are_served() {
        let pool = connect().await;
        delete_fixture_station(&pool, "ZIOI5").await;
        delete_fixture_station(&pool, "ZIOI6").await;
        delete_fixture_line(&pool, "ZLINE3").await;
        let nir = IslandOfIrelandStation {
            id: "ZIOI5".to_string(),
            name: "Zesthalt".to_string(),
            network: IslandOfIrelandNetwork::NorthernIreland,
            latitude: Some(54.6),
            longitude: Some(-5.8),
            raw_name: Some("ZESTHALT RAIL HALT".to_string()),
        };
        let roi = IslandOfIrelandStation {
            id: "ZIOI6".to_string(),
            name: "Zorough".to_string(),
            network: IslandOfIrelandNetwork::RepublicOfIreland,
            raw_name: None,
            ..nir.clone()
        };
        upsert_stations(&pool, &[nir.clone(), roi]).await.unwrap();
        // The writer's refresh sets live codes; stand in for it here.
        sqlx::query("UPDATE island_of_ireland_stations SET live_code = 'ZORGH' WHERE id = 'ZIOI6'")
            .execute(&pool)
            .await
            .unwrap();
        upsert_lines(
            &pool,
            &[IslandOfIrelandLineDefinition {
                id: "ZLINE3".to_string(),
                name: "Zesttown - Zorough".to_string(),
                network: IslandOfIrelandNetwork::RepublicOfIreland,
                stations: vec!["ZIOI6".to_string()],
                short_name: Some("ZR".to_string()),
                agency: Some("Zest Rail".to_string()),
                color: Some("#00A651".to_string()),
                text_color: Some("#FFFFFF".to_string()),
                replaces: vec![],
            }],
        )
        .await
        .unwrap();

        let stations = list_stations(&pool, None).await.unwrap();
        let get = |id: &str| stations.iter().find(|s| s.id == id).unwrap().clone();
        assert_eq!(get("ZIOI5").raw_name.as_deref(), Some("ZESTHALT RAIL HALT"));
        assert_eq!(get("ZIOI5").live_code, None);
        assert_eq!(get("ZIOI6").live_code.as_deref(), Some("ZORGH"));
        let json = serde_json::to_value(get("ZIOI6")).unwrap();
        assert_eq!(json["liveCode"], "ZORGH");
        assert_eq!(json["rawName"], serde_json::Value::Null);
        assert_eq!(json["network"], "republic-of-ireland");

        let lines = list_lines(&pool, Some(IslandOfIrelandNetwork::RepublicOfIreland))
            .await
            .unwrap();
        let line = lines.iter().find(|l| l.id == "ZLINE3").unwrap();
        assert_eq!(line.color.as_deref(), Some("#00A651"));
        assert_eq!(line.agency.as_deref(), Some("Zest Rail"));
        assert_eq!(line.short_name.as_deref(), Some("ZR"));
        let json = serde_json::to_value(line).unwrap();
        assert_eq!(json["textColor"], "#FFFFFF");

        assert_eq!(
            live_board_key(&pool, "ZIOI6").await.unwrap().as_deref(),
            Some("ZORGH")
        );
        assert_eq!(live_board_key(&pool, "ZIOI5").await.unwrap(), None);
        assert_eq!(
            live_board_key(&pool, "BFSTC").await.unwrap().as_deref(),
            Some("BFSTC")
        );

        delete_fixture_station(&pool, "ZIOI5").await;
        delete_fixture_station(&pool, "ZIOI6").await;
        delete_fixture_line(&pool, "ZLINE3").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                island_of_ireland -- --ignored --test-threads=1`"]
    async fn upsert_lines_skips_unchanged_rows() {
        let pool = connect().await;
        delete_fixture_line(&pool, "ZLINE2").await;
        let line = |stations: &[&str]| IslandOfIrelandLineDefinition {
            id: "ZLINE2".to_string(),
            name: "Zest Loop".to_string(),
            network: IslandOfIrelandNetwork::NorthernIreland,
            stations: stations.iter().map(ToString::to_string).collect(),
            short_name: None,
            agency: None,
            color: None,
            text_color: None,
            replaces: vec![],
        };
        upsert_lines(&pool, &[line(&["ZA", "ZB"])]).await.unwrap();
        let before = xmin(&pool, "island_of_ireland_lines", "ZLINE2").await;
        upsert_lines(&pool, &[line(&["ZA", "ZB"])]).await.unwrap();
        assert_eq!(
            xmin(&pool, "island_of_ireland_lines", "ZLINE2").await,
            before
        );
        assert!(last_lines_fetch(&pool).await.unwrap().is_some());
        // Reordering the stations is a change.
        upsert_lines(&pool, &[line(&["ZB", "ZA"])]).await.unwrap();
        assert_ne!(
            xmin(&pool, "island_of_ireland_lines", "ZLINE2").await,
            before
        );
        let lines = list_lines(&pool, None).await.unwrap();
        let stored = lines.iter().find(|l| l.id == "ZLINE2").expect("row");
        assert_eq!(stored.stations, vec!["ZB".to_string(), "ZA".to_string()]);
        delete_fixture_line(&pool, "ZLINE2").await;
    }
}

#[cfg(test)]
mod sample_db_tests {
    use sqlx::postgres::PgPoolOptions;

    use super::*;

    async fn connect() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
        PgPoolOptions::new()
            .connect(&database_url)
            .await
            .expect("connect")
    }

    async fn delete_fixture(pool: &PgPool, station_id: &str) {
        sqlx::query("DELETE FROM island_of_ireland_station_samples WHERE station_id = $1")
            .bind(station_id)
            .execute(pool)
            .await
            .expect("cleanup fixture sample");
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                upsert_then_latest_round_trips_and_repeat_upsert_replaces -- --ignored --test-threads=1`"]
    async fn upsert_then_latest_round_trips_and_repeat_upsert_replaces() {
        let pool = connect().await;
        delete_fixture(&pool, "ZSAMP1").await;

        let first = IslandOfIrelandStationSample {
            station_id: "ZSAMP1".to_string(),
            network: IslandOfIrelandNetwork::RepublicOfIreland,
            polled_at: chrono::Utc::now(),
            departures: vec![IslandOfIrelandDeparture {
                train_code: "Z1".to_string(),
                origin: "Zesttown".to_string(),
                destination: "Zorough".to_string(),
                scheduled_arrival: None,
                scheduled_departure: Some("10:00".to_string()),
                expected_arrival: None,
                expected_departure: Some("10:00".to_string()),
                late_minutes: 0,
                status: "On Time".to_string(),
                due_in_minutes: Some(3),
                train_date: None,
                origin_time: None,
                destination_time: None,
                last_location: None,
            }],
        };
        upsert_station_samples(&pool, &[first])
            .await
            .expect("first upsert");

        let fetched = latest_station_sample(&pool, "ZSAMP1")
            .await
            .expect("fetch")
            .expect("row present");
        assert_eq!(fetched.departures[0].train_code, "Z1");

        let second = IslandOfIrelandStationSample {
            station_id: "ZSAMP1".to_string(),
            network: IslandOfIrelandNetwork::RepublicOfIreland,
            polled_at: chrono::Utc::now(),
            departures: vec![],
        };
        upsert_station_samples(&pool, &[second])
            .await
            .expect("second upsert");
        let fetched = latest_station_sample(&pool, "ZSAMP1")
            .await
            .expect("fetch")
            .expect("row still present");
        assert_eq!(fetched.departures, []);

        delete_fixture(&pool, "ZSAMP1").await;
    }

    /// N1: a re-poll with the same board and the same `polled_at` writes
    /// nothing; a newer `polled_at` advances the age; the last duplicate in
    /// a batch wins.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                upsert_station_samples_skips_an_identical_poll -- --ignored --test-threads=1`"]
    async fn upsert_station_samples_skips_an_identical_poll() {
        let pool = connect().await;
        delete_fixture(&pool, "ZSAMP2").await;
        let polled_at: chrono::DateTime<chrono::Utc> = "2031-01-02T03:04:05Z".parse().unwrap();
        let sample =
            |polled_at, departures: Vec<IslandOfIrelandDeparture>| IslandOfIrelandStationSample {
                station_id: "ZSAMP2".to_string(),
                network: IslandOfIrelandNetwork::RepublicOfIreland,
                polled_at,
                departures,
            };
        let departure = IslandOfIrelandDeparture {
            train_code: "Z2".to_string(),
            origin: "Zesttown".to_string(),
            destination: "Zorough".to_string(),
            scheduled_arrival: None,
            scheduled_departure: Some("11:00".to_string()),
            expected_arrival: None,
            expected_departure: Some("11:02".to_string()),
            late_minutes: 2,
            status: "Late".to_string(),
            due_in_minutes: Some(5),
            train_date: None,
            origin_time: None,
            destination_time: None,
            last_location: None,
        };
        let xmin = |pool: PgPool| async move {
            sqlx::query_scalar::<_, String>(
                "SELECT xmin::text FROM island_of_ireland_station_samples WHERE station_id = 'ZSAMP2'",
            )
            .fetch_one(&pool)
            .await
            .expect("xmin")
        };

        upsert_station_samples(
            &pool,
            &[
                sample(polled_at, vec![]),
                sample(polled_at, vec![departure.clone()]),
            ],
        )
        .await
        .unwrap();
        let fetched = latest_station_sample(&pool, "ZSAMP2")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched.departures, vec![departure.clone()]);
        let before = xmin(pool.clone()).await;

        upsert_station_samples(&pool, &[sample(polled_at, vec![departure.clone()])])
            .await
            .unwrap();
        assert_eq!(
            xmin(pool.clone()).await,
            before,
            "identical poll rewrote the row"
        );

        let later = polled_at + chrono::Duration::minutes(1);
        upsert_station_samples(&pool, &[sample(later, vec![departure])])
            .await
            .unwrap();
        assert_ne!(xmin(pool.clone()).await, before);
        assert_eq!(
            last_station_samples_fetch(&pool)
                .await
                .unwrap()
                .map(|t| t >= later),
            Some(true)
        );

        delete_fixture(&pool, "ZSAMP2").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api \
                latest_for_an_unseen_station_is_none_not_an_error -- --ignored --test-threads=1`"]
    async fn latest_for_an_unseen_station_is_none_not_an_error() {
        let pool = connect().await;
        delete_fixture(&pool, "ZSAMPNONE").await;
        let fetched = latest_station_sample(&pool, "ZSAMPNONE")
            .await
            .expect("query");
        assert_eq!(fetched, None);
    }
}
