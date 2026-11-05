//! Tracking an Iarnród Éireann train (`POST /Journeys/irish-train`) and
//! reading it back through the tracked-train and journey routes, against a
//! live database (distant-signal's
//! docs/superpowers/specs/2026-11-05-irish-rail-train-tracking-design.md
//! §5.3).
//!
//! Fixtures: synthetic train codes (`ZI0n`), live station codes (`ZIA`,
//! `ZIB`) and users (`TEST-IRISH-*`); every cleanup is keyed on them. The
//! board history is written as the schema owner when the suite runs as a
//! narrow role (`schedule_fixture_pool`), as the writer would.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;
use crate::test_support::FixtureCleanup;

/// A tracked train's identity, pins and names: uid, resolution, CRS
/// pin, station ref, the two end names and the train's CRS origin.
type TrackedRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

async fn connect() -> PgPool {
    ds_store::test_support::connect().await
}

/// Dublin's today, as the route reads it.
fn today() -> chrono::NaiveDate {
    crate::routes::london_now()
        .with_timezone(&chrono_tz::Europe::Dublin)
        .date_naive()
}

fn cleanup_statements() -> Vec<String> {
    vec![
        "DELETE FROM journeys WHERE user_id LIKE 'TEST-IRISH-%'".to_string(),
        "DELETE FROM train_subscriptions WHERE user_id LIKE 'TEST-IRISH-%'".to_string(),
        "DELETE FROM sessions WHERE user_id LIKE 'TEST-IRISH-%'".to_string(),
        "DELETE FROM users WHERE id LIKE 'TEST-IRISH-%'".to_string(),
        "DELETE FROM trains WHERE train_uid LIKE 'ie:ZI0%'".to_string(),
        "DELETE FROM island_of_ireland_train_calls WHERE train_code LIKE 'ZI0%'".to_string(),
        "DELETE FROM island_of_ireland_trains WHERE train_code LIKE 'ZI0%'".to_string(),
        "DELETE FROM island_of_ireland_live_stations WHERE code IN ('ZIA', 'ZIB')".to_string(),
    ]
}

/// Train `code` on `date` as two boards showed it: Zesttown (`ZIA`,
/// departing 10:00) and Zorough (`ZIB`), reporting `late` minutes, last
/// seen now.
async fn seed_history(pool: &PgPool, code: &str, date: chrono::NaiveDate, late: i32) {
    let seed = ds_store::test_support::schedule_fixture_pool(pool).await;
    for (sql, binds_code) in [
        (
            "INSERT INTO island_of_ireland_live_stations (code, name) \
             VALUES ('ZIA', 'Zesttown'), ('ZIB', 'Zorough') ON CONFLICT (code) DO NOTHING",
            false,
        ),
        (
            "INSERT INTO island_of_ireland_trains (network, train_code, train_date, origin, \
             destination, origin_time, destination_time, status, late_minutes, last_location, \
             first_seen_at, last_seen_at, changed_at) VALUES \
             ('republic-of-ireland', $1, $2, 'Zesttown', 'Zorough', '10:00', '11:00', \
              'En Route', $3, 'Departed Zesttown', now(), now(), now())",
            true,
        ),
        (
            "INSERT INTO island_of_ireland_train_calls (network, train_code, train_date, \
             station_code, scheduled_departure, first_seen_at, changed_at) VALUES \
             ('republic-of-ireland', $1, $2, 'ZIA', '10:00', now(), now()), \
             ('republic-of-ireland', $1, $2, 'ZIB', NULL, now(), now())",
            true,
        ),
    ] {
        let query = sqlx::query(sql);
        let query = if binds_code {
            query.bind(code).bind(date).bind(late)
        } else {
            query
        };
        query.execute(&seed).await.expect(sql);
    }
}

async fn seed_user(pool: &PgPool, user: &str) -> String {
    sqlx::query("INSERT INTO users (id, name) VALUES ($1, $1) ON CONFLICT (id) DO NOTHING")
        .bind(user)
        .execute(pool)
        .await
        .expect("seed user");
    let raw = format!("raw-session-{user}");
    sqlx::query(
        "INSERT INTO sessions (id, user_id, expires_at) VALUES ($1, $2, now() + INTERVAL '1 day') \
         ON CONFLICT (id) DO UPDATE SET expires_at = EXCLUDED.expires_at",
    )
    .bind(hash_session_token(&raw))
    .bind(user)
    .execute(pool)
    .await
    .expect("seed session");
    raw
}

/// `(status, body)` of `method uri` as the session `cookie`.
async fn call(
    pool: &PgPool,
    method: &str,
    uri: &str,
    cookie: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let router = crate::app::Router::new()
        .merge(super::journeys::router())
        .merge(super::train::router())
        .with_state(crate::test_support::inert_app(pool.clone()));
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, format!("distant_signal_session={cookie}"))
        .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
        .unwrap();
    let response = router.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 << 20)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    (
        status,
        serde_json::from_str(&text).unwrap_or(Value::String(text)),
    )
}

/// Tracking: the refusals, then a train made bound at once with its names
/// and the board it was tracked from, idempotent; every read marks it
/// Irish and names it; with no state row yet the reads take the board
/// history's state, and once the writer has written one, that.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api irish_train_db_tests \
            -- --ignored --test-threads=1`"]
#[expect(clippy::too_many_lines, reason = "one scenario, request by request")]
async fn an_irish_train_is_tracked_from_its_board_history_and_read_back() {
    let pool = connect().await;
    let guard_pool = ds_store::test_support::schedule_fixture_pool(&pool).await;
    let _cleanup = FixtureCleanup::new(&guard_pool, cleanup_statements()).await;
    let date = today();
    seed_history(&pool, "ZI01", date, 4).await;
    let user = "TEST-IRISH-TRACK";
    let cookie = seed_user(&pool, user).await;
    let track = |body: Value| {
        let (pool, cookie) = (&pool, &cookie);
        async move { call(pool, "POST", "/Journeys/irish-train", cookie, Some(body)).await }
    };

    let (status, body) = track(json!({ "trainCode": "ZI09", "serviceDate": date })).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body,
        json!("We haven't seen this train on a departure board.")
    );
    let (status, body) =
        track(json!({ "trainCode": "ZI01", "serviceDate": date, "fromStation": "ZIC" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = track(json!({
        "trainCode": "ZI01",
        "serviceDate": date - chrono::Days::new(2),
    }))
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, created) =
        track(json!({ "trainCode": " zi01", "serviceDate": date, "fromStation": "zia" })).await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let tracking_id = created["trackingId"].as_i64().unwrap();
    let journey_id = created["journeyId"].as_i64().unwrap();

    let row: TrackedRow = sqlx::query_as(
        "SELECT tr.train_uid, ts.resolution_status, ts.pin_origin_crs, ts.pin_origin_ref, \
                    tr.origin_name, tr.destination_name, tr.origin_crs \
             FROM train_subscriptions ts JOIN trains tr ON tr.id = ts.trains_id WHERE ts.id = $1",
    )
    .bind(tracking_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "ie:ZI01".into(),
            "resolved".into(),
            None,
            Some("ie:ZIA".into()),
            Some("Zesttown".into()),
            Some("Zorough".into()),
            None
        )
    );

    // Again: the same journey, nothing new.
    let (status, again) = track(json!({ "trainCode": "ZI01", "serviceDate": date })).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again, created);

    let irish = json!({ "trainCode": "ZI01", "serviceDate": date });
    let (status, train) = call(
        &pool,
        "GET",
        &format!("/Train/{tracking_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{train}");
    assert_eq!(train["irishTrain"], irish);
    assert_eq!(train["pinOriginName"], "Zesttown");
    assert_eq!(train["pinDestinationName"], "Zorough");
    assert_eq!(train["operatorName"], "Iarnród Éireann");
    assert_eq!(
        train["status"], "en_route",
        "the board history's state: {train}"
    );
    assert_eq!(train["delayMinutes"], 4);
    assert_eq!(train["lastReportedLocation"], "Zesttown");
    assert_eq!(train["lastEventType"], "DEPARTURE");
    assert_eq!(train["trackingPhase"], Value::Null);
    assert_eq!(train["journeyStops"], Value::Null);

    let (status, mine) = call(&pool, "GET", "/Train/mine", &cookie, None).await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    let item = &mine.as_array().unwrap()[0];
    assert_eq!(item["irishTrain"], irish);
    assert_eq!(item["pinOriginName"], "Zesttown");
    assert_eq!(item["delayMinutes"], 4);

    let (status, journeys) = call(&pool, "GET", "/Journeys/mine", &cookie, None).await;
    assert_eq!(status, StatusCode::OK, "{journeys}");
    let journey = journeys
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == journey_id)
        .unwrap_or_else(|| panic!("{journeys}"));
    assert_eq!(journey["irishTrain"], irish);
    assert_eq!(journey["originName"], "Zesttown");
    assert_eq!(journey["destinationName"], "Zorough");
    assert_eq!(journey["status"], "en_route");

    let (status, detail) = call(
        &pool,
        "GET",
        &format!("/Journeys/{journey_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let leg = &detail["legs"][0];
    assert_eq!(leg["originName"], "Zesttown", "{detail}");
    assert_eq!(leg["destinationName"], "Zorough", "{detail}");
    assert_eq!(leg["trackedTrainState"]["irishTrain"], irish, "{detail}");

    // The writer's state, once there is one, wins over the history.
    let trains_id: i64 =
        sqlx::query_scalar("SELECT trains_id FROM train_subscriptions WHERE id = $1")
            .bind(tracking_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let writer = ds_store::test_support::writer_pool(&pool).await;
    sqlx::query(
        "INSERT INTO train_current_state (trains_id, status, delay_minutes, \
         last_reported_location, last_event_type, event_time) \
         VALUES ($1, 'en_route', 16, 'Zmid', 'ARRIVAL', now())",
    )
    .bind(trains_id)
    .execute(&writer)
    .await
    .unwrap();
    let (_, train) = call(
        &pool,
        "GET",
        &format!("/Train/{tracking_id}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(train["delayMinutes"], 16, "{train}");
    assert_eq!(train["lastReportedLocation"], "Zmid");
}

/// The GB-only routes never reach an Irish train: the public by-uid read
/// refuses an `ie:` uid in any case (it would otherwise be upper-cased),
/// and the journey-break sweep never picks one up.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api irish_train_db_tests \
            -- --ignored --test-threads=1`"]
async fn gb_only_routes_leave_irish_trains_alone() {
    let pool = connect().await;
    let guard_pool = ds_store::test_support::schedule_fixture_pool(&pool).await;
    let _cleanup = FixtureCleanup::new(&guard_pool, cleanup_statements()).await;
    let date = today();
    seed_history(&pool, "ZI02", date, 4).await;
    let user = "TEST-IRISH-GB";
    let cookie = seed_user(&pool, user).await;
    let (status, created) = call(
        &pool,
        "POST",
        "/Journeys/irish-train",
        &cookie,
        Some(json!({ "trainCode": "ZI02", "serviceDate": date })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    for uid in ["ie:ZI02", "IE:ZI02", "ie:zi02"] {
        let (status, body) = call(
            &pool,
            "GET",
            &format!("/Train/by-uid/{uid}/{date}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uid}: {body}");
    }

    // Departing now by its pin: inside the sweep's window, but Irish.
    sqlx::query(
        "UPDATE train_subscriptions SET pin_scheduled_departure = now() WHERE user_id = $1",
    )
    .bind(user)
    .execute(&pool)
    .await
    .unwrap();
    let app = crate::test_support::inert_app(pool.clone());
    let swept = crate::journey_break_sweep::run_sweep(&app, chrono::Utc::now())
        .await
        .unwrap();
    assert_eq!(swept.journeys, 0, "{swept:?}");
}
