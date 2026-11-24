//! `GET /Train/by-uid/{uid}/{date}` for a train that is no longer held in
//! full (DS-638) against a live database: served from the live row while
//! the timetable rows exist, from the `trains` row alone once they are gone,
//! from a journey leg's own copy for a caller who may read that leg once the
//! row is gone too, and a 404 for everyone else.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{NaiveDate, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;
use crate::data::leg_snapshot::{LegSnapshot, TrainFacts};

const OWNER: &str = "TEST-BYUID-OWNER";
const OTHER: &str = "TEST-BYUID-OTHER";

async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to postgres")
}

async fn cleanup(pool: &PgPool) {
    for user in [OWNER, OTHER] {
        for sql in [
            "DELETE FROM unlisted_links WHERE created_by = $1",
            "DELETE FROM journeys WHERE user_id = $1",
            "DELETE FROM train_subscriptions WHERE user_id = $1",
            "DELETE FROM sessions WHERE user_id = $1",
            "DELETE FROM users WHERE id = $1",
        ] {
            sqlx::query(sql).bind(user).execute(pool).await.expect(sql);
        }
    }
    for sql in [
        "DELETE FROM trains WHERE train_uid LIKE 'BYUID%'",
        "DELETE FROM schedule_destination_departures WHERE train_uid LIKE 'BYUID%'",
        "DELETE FROM schedule_services WHERE uid LIKE 'BYUID%'",
        "DELETE FROM schedule_calling_points_full WHERE uid LIKE 'BYUID%'",
        "DELETE FROM stanox_crs WHERE stanox LIKE 'BYUIDPIN%'",
    ] {
        sqlx::query(sql).execute(pool).await.expect(sql);
    }
}

async fn session(pool: &PgPool, user: &str) -> String {
    sqlx::query("INSERT INTO users (id, name) VALUES ($1, $1) ON CONFLICT DO NOTHING")
        .bind(user)
        .execute(pool)
        .await
        .expect("seed user");
    let raw = format!("raw-byuid-{user}");
    sqlx::query(
        "INSERT INTO sessions (id, user_id, expires_at) VALUES ($1, $2, now() + INTERVAL '1 day') \
         ON CONFLICT (id) DO UPDATE SET user_id = EXCLUDED.user_id, expires_at = EXCLUDED.expires_at",
    )
    .bind(hash_session_token(&raw))
    .bind(user)
    .execute(pool)
    .await
    .expect("seed session");
    raw
}

fn router(pool: PgPool) -> axum::Router {
    crate::app::Router::new()
        .merge(super::train::router())
        .with_state(crate::test_support::inert_app(pool))
}

async fn get(
    router: &axum::Router,
    uri: &str,
    cookie: Option<&str>,
    share_token: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method("GET").uri(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, format!("distant_signal_session={cookie}"));
    }
    if let Some(token) = share_token {
        request = request.header("x-journey-share-token", token);
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned())),
    )
}

fn calling_points() -> Value {
    json!([
        {"tiploc": "ZZBYUA", "kind": "Origin", "bookedDeparture": "09:00:00",
         "publicDeparture": "09:00:00", "workingDeparture": "09:00:00"},
        {"tiploc": "ZZBYUB", "kind": "Intermediate", "bookedArrival": "09:20:00",
         "bookedDeparture": "09:21:00", "publicArrival": "09:20:00",
         "publicDeparture": "09:21:00"},
        {"tiploc": "ZZBYUC", "kind": "Terminate", "bookedArrival": "09:40:00",
         "publicArrival": "09:40:00"},
    ])
}

/// A `trains` row with three timetabled calls and a finished current state.
async fn seed_train(pool: &PgPool, uid: &str, date: NaiveDate) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO trains (train_uid, service_date, train_id, origin_crs, destination_crs, \
                             calling_points, platform, planned_platform) \
         VALUES ($1, $2, '161A23T101', 'ZBA', 'ZBC', $3, '5', '4') RETURNING id",
    )
    .bind(uid)
    .bind(date)
    .bind(calling_points())
    .fetch_one(pool)
    .await
    .expect("seed train");
    sqlx::query(
        "INSERT INTO train_current_state \
            (trains_id, status, last_reported_location, last_event_type, delay_minutes, \
             next_calling_point, updated_at) \
         VALUES ($1, 'completed', 'ZZBYUC', 'ARRIVAL', 7, NULL, NOW())",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("seed current state");
    id
}

async fn seed_timetable_row(pool: &PgPool, uid: &str, date: NaiveDate) {
    sqlx::query(
        "INSERT INTO schedule_destination_departures \
             (service_date, origin_crs, destination_crs, scheduled, train_uid, headcode) \
         VALUES ($1, 'ZBA', 'ZBC', TIME '09:00', $2, '1A23')",
    )
    .bind(date)
    .bind(uid)
    .execute(pool)
    .await
    .expect("seed timetable row");
}

/// A journey of `user` with one train leg whose subscription has no train
/// (pruned) and which holds the copy of `uid` made at `date`.
async fn seed_snapshotted_leg(pool: &PgPool, user: &str, uid: &str, date: NaiveDate) -> i64 {
    let snapshot = LegSnapshot::from_train(
        TrainFacts {
            train_uid: uid.to_owned(),
            train_id: Some("161A23T101".to_owned()),
            service_date: date,
            origin_crs: Some("ZBA".to_owned()),
            destination_crs: Some("ZBC".to_owned()),
            scheduled_departure: None,
            matched_line_id: None,
            platform: Some("5".to_owned()),
            planned_platform: Some("4".to_owned()),
            skipped_stations: Vec::new(),
            calling_points: Some(calling_points()),
        },
        Utc::now(),
    )
    .with_outcome(Some(7), Some(false));
    let journey: i64 =
        sqlx::query_scalar("INSERT INTO journeys (user_id) VALUES ($1) RETURNING id")
            .bind(user)
            .fetch_one(pool)
            .await
            .unwrap();
    let subscription: i64 = sqlx::query_scalar(
        "INSERT INTO train_subscriptions (user_id, service_date, resolution_status, trains_id) \
         VALUES ($1, $2, 'resolved', NULL) RETURNING id",
    )
    .bind(user)
    .bind(date)
    .fetch_one(pool)
    .await
    .expect("seed subscription");
    sqlx::query(
        "INSERT INTO journey_legs (journey_id, leg_order, service_date, origin_crs, \
             destination_crs, match_mode, train_subscription_id, final_train_uid, \
             final_snapshot, final_snapshot_at) \
         VALUES ($1, 1, $2, 'ZBA', 'ZBC', 'auto', $3, $4, $5, now())",
    )
    .bind(journey)
    .bind(date)
    .bind(subscription)
    .bind(uid)
    .bind(snapshot.to_value())
    .execute(pool)
    .await
    .expect("seed leg");
    journey
}

/// (a) A live train with timetable rows, (b) the same once the timetable
/// rows are gone: both are served from the `trains` row.
#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database); --test-threads=1"]
async fn a_held_train_is_served_with_or_without_its_timetable_rows() {
    let pool = connect().await;
    cleanup(&pool).await;
    let date = NaiveDate::from_ymd_opt(2026, 1, 14).unwrap();
    seed_train(&pool, "BYUID01", date).await;
    seed_timetable_row(&pool, "BYUID01", date).await;
    let router = router(pool.clone());
    let uri = format!("/Train/by-uid/BYUID01/{date}");

    let (status, body) = get(&router, &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["trainDetail"], "live");
    assert_eq!(body["snapshotAt"], Value::Null);
    assert_eq!(body["trainUid"], "BYUID01");
    assert_eq!(body["headcode"], "1A23", "the timetable's headcode");
    assert_eq!(body["journeyStops"].as_array().unwrap().len(), 3);

    // The timetable rows age out (about 8 days); the `trains` row stays.
    sqlx::query("DELETE FROM schedule_destination_departures WHERE train_uid = 'BYUID01'")
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = get(&router, &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["trainDetail"], "live");
    assert_eq!(body["trainUid"], "BYUID01");
    assert_eq!(body["trainId"], "161A23T101");
    assert_eq!(body["originCrs"], "ZBA");
    assert_eq!(body["destinationCrs"], "ZBC");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["lastReportedLocation"], "ZZBYUC");
    assert_eq!(body["headcode"], "1A23", "from the TRUST id");
    let stops = body["journeyStops"].as_array().expect("stops from the row");
    assert_eq!(stops.len(), 3);
    assert_eq!(stops[0]["tiploc"], "ZZBYUA");

    cleanup(&pool).await;
}

/// (c) No `trains` row, no timetable rows, a leg's copy, the owner asking.
/// (d) Anyone else, signed in or not, gets the 404 of (e).
#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database); --test-threads=1"]
async fn a_pruned_train_is_served_from_the_owners_leg_and_nobody_elses() {
    let pool = connect().await;
    cleanup(&pool).await;
    let owner = session(&pool, OWNER).await;
    let other = session(&pool, OTHER).await;
    let date = NaiveDate::from_ymd_opt(2026, 1, 14).unwrap();
    seed_snapshotted_leg(&pool, OWNER, "BYUID02", date).await;
    let router = router(pool.clone());
    let uri = format!("/Train/by-uid/BYUID02/{date}");

    let (status, body) = get(&router, &uri, Some(&owner), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["trainDetail"], "snapshot");
    assert!(body["snapshotAt"].is_string(), "{body}");
    assert_eq!(body["trainUid"], "BYUID02");
    assert_eq!(body["trainId"], "161A23T101");
    assert_eq!(body["headcode"], "1A23");
    assert_eq!(body["originCrs"], "ZBA");
    assert_eq!(body["destinationCrs"], "ZBC");
    assert_eq!(body["status"], "completed");
    assert_eq!(body["delayMinutes"], 7);
    assert_eq!(body["trackingPhase"], Value::Null);
    assert_eq!(body["lastReportedLocation"], Value::Null, "no live data");
    let stops = body["journeyStops"]
        .as_array()
        .expect("stops from the copy");
    assert_eq!(stops.len(), 3);
    assert_eq!(stops[0]["tiploc"], "ZZBYUA");
    assert_eq!(stops[0]["platform"], "5", "the origin platform");
    assert_eq!(stops[2]["tiploc"], "ZZBYUC");

    // Not the owner's: the same answer as for a train that never existed.
    for cookie in [Some(other.as_str()), None] {
        let (status, body) = get(&router, &uri, cookie, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(
            body,
            Value::String("no known train for that uid/date".into())
        );
    }
    // A made-up or other-journey share token adds nothing.
    let (status, _) = get(&router, &uri, Some(&other), Some("no-such-link")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The date is part of the key.
    let next = date.succ_opt().unwrap();
    let (status, _) = get(
        &router,
        &format!("/Train/by-uid/BYUID02/{next}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    cleanup(&pool).await;
}

/// The share link of the leg's journey opens the train page for anyone who
/// holds it, and only for that journey's legs.
#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database); --test-threads=1"]
async fn a_share_link_opens_the_train_of_its_journey_only() {
    let pool = connect().await;
    cleanup(&pool).await;
    session(&pool, OWNER).await;
    session(&pool, OTHER).await;
    let date = NaiveDate::from_ymd_opt(2026, 1, 14).unwrap();
    let shared = seed_snapshotted_leg(&pool, OWNER, "BYUID03", date).await;
    let unshared = seed_snapshotted_leg(&pool, OTHER, "BYUID04", date).await;
    let link = crate::data::unlisted_links::rotate_link(
        &pool,
        "journey",
        &shared.to_string(),
        OWNER,
        Some(chrono::Duration::days(1)),
    )
    .await
    .unwrap();
    let router = router(pool.clone());

    let (status, body) = get(
        &router,
        &format!("/Train/by-uid/BYUID03/{date}"),
        None,
        Some(&link.token),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["trainDetail"], "snapshot");

    // The same link says nothing about another journey's train.
    let (status, _) = get(
        &router,
        &format!("/Train/by-uid/BYUID04/{date}"),
        None,
        Some(&link.token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A revoked link opens nothing.
    crate::data::unlisted_links::revoke_link(&pool, "journey", &shared.to_string())
        .await
        .unwrap();
    let (status, _) = get(
        &router,
        &format!("/Train/by-uid/BYUID03/{date}"),
        None,
        Some(&link.token),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let _ = unshared;

    cleanup(&pool).await;
}

/// (e) Nothing anywhere: the 404 it always was.
#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database); --test-threads=1"]
async fn a_train_held_nowhere_is_a_404() {
    let pool = connect().await;
    cleanup(&pool).await;
    let owner = session(&pool, OWNER).await;
    let date = NaiveDate::from_ymd_opt(2026, 1, 14).unwrap();
    let router = router(pool.clone());

    let (status, body) = get(
        &router,
        &format!("/Train/by-uid/BYUID05/{date}"),
        Some(&owner),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body,
        Value::String("no known train for that uid/date".into())
    );

    cleanup(&pool).await;
}

/// The contract with the aggregator's pin (ds-workers `prune_keeps_the_schedule_rows_of_a_pinned_train`):
/// a train a journey leg is bound to is kept with its `trains` row and
/// children, and with its `schedule_services`, `schedule_destination_departures`
/// and `schedule_calling_points_full` rows, however old the service date. Served 400 days on from exactly those
/// rows, the page still has the service mode, the operator, the headcode, the
/// stops with their actual times and the reported platform. The last block
/// shows what each schedule row is for: without them the delay falls back to
/// the working timetable, the mode reads `train` and the operator is empty,
/// which is what the pin prevents.
#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database); --test-threads=1"]
#[expect(
    clippy::too_many_lines,
    reason = "a scenario test: seed, assert, drop each row, assert again"
)]
async fn a_pinned_train_keeps_its_operator_mode_stops_and_actual_times() {
    let pool = connect().await;
    cleanup(&pool).await;
    let date = Utc::now().date_naive() - chrono::Duration::days(400);
    let trains_id = seed_train(&pool, "BYUID09", date).await;
    for (stanox, crs, tiploc) in [
        ("BYUIDPIN1", "ZBA", "ZZBYUA"),
        ("BYUIDPIN2", "ZBB", "ZZBYUB"),
        ("BYUIDPIN3", "ZBC", "ZZBYUC"),
    ] {
        sqlx::query(
            "INSERT INTO stanox_crs (stanox, crs, tiploc, station_name, source_sequence) \
             VALUES ($1, $2, $3, $2, 1) ON CONFLICT (stanox) DO NOTHING",
        )
        .bind(stanox)
        .bind(crs)
        .bind(tiploc)
        .execute(&pool)
        .await
        .expect("seed stanox_crs");
    }
    sqlx::query(
        "INSERT INTO train_movement_events \
            (trains_id, dedup_key, msg_type, event_type, loc_crs, planned_timestamp, \
             actual_timestamp, variation_status, platform, raw_body) \
         VALUES ($1, 'byuid09-dep', '0003', 'DEPARTURE', 'ZBA', \
                 ($2::date + TIME '09:00') AT TIME ZONE 'Europe/London', \
                 ($2::date + TIME '09:04') AT TIME ZONE 'Europe/London', \
                 'LATE', '5', '{}'::jsonb)",
    )
    .bind(trains_id)
    .bind(date)
    .execute(&pool)
    .await
    .expect("seed movement event");
    sqlx::query(
        "INSERT INTO schedule_services (service_date, uid, mode, stp, operator_atoc, headcode) \
         VALUES ($1, 'BYUID09', 'bus', 'P', 'ZZ', '1A23')",
    )
    .bind(date)
    .execute(&pool)
    .await
    .expect("seed schedule_services");
    sqlx::query(
        "INSERT INTO schedule_destination_departures \
             (service_date, origin_crs, destination_crs, scheduled, train_uid, headcode, operator_atoc) \
         VALUES ($1, 'ZBA', 'ZBC', TIME '09:00', 'BYUID09', '1A23', 'ZZ')",
    )
    .bind(date)
    .execute(&pool)
    .await
    .expect("seed schedule_destination_departures");
    sqlx::query(
        "INSERT INTO schedule_calling_points_full \
             (service_date, uid, seq, tiploc, kind, booked_departure, public_departure, \
              working_departure, day_offset) \
         VALUES ($1, 'BYUID09', 0, 'ZZBYUA', 'origin', TIME '09:00', TIME '09:01', \
                 TIME '09:00', 0)",
    )
    .bind(date)
    .execute(&pool)
    .await
    .expect("seed schedule_calling_points_full");
    let router = router(pool.clone());
    let uri = format!("/Train/by-uid/BYUID09/{date}");

    let (status, body) = get(&router, &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["trainDetail"], "live");
    assert_eq!(
        body["serviceMode"], "bus",
        "from the pinned schedule_services row"
    );
    assert_eq!(body["liveTracking"], false);
    assert_eq!(body["operatorCode"], "ZZ", "from the pinned departures row");
    assert_eq!(body["headcode"], "1A23");
    assert_eq!(body["status"], "completed");
    let stops = body["journeyStops"].as_array().expect("stops");
    assert_eq!(stops.len(), 3, "{body}");
    assert_eq!(stops[0]["crs"], "ZBA");
    assert!(
        stops[0]["actualDeparture"].is_string(),
        "the recorded actual departure: {}",
        stops[0]
    );
    assert_eq!(stops[0]["lastEventType"], "DEPARTURE");
    // The flags say which actual times are recorded ones: only the origin's
    // departure was reported; false is "no recorded actual", never "on time".
    assert_eq!(stops[0]["actualDepartureIsActual"], true, "{}", stops[0]);
    assert_eq!(stops[0]["actualArrivalIsActual"], false, "{}", stops[0]);
    for stop in &stops[1..] {
        assert_eq!(stop["actualArrivalIsActual"], false, "{stop}");
        assert_eq!(stop["actualDepartureIsActual"], false, "{stop}");
        assert!(stop["actualArrival"].is_null() && stop["actualDeparture"].is_null());
    }
    assert_eq!(stops[0]["platform"], "5", "the platform TRUST reported");
    // The public delay reads the schedule's public and working times at the
    // call: 4 minutes after the working time, 3 after the public one.
    assert_eq!(body["delayBasis"], "publicSchedule", "{body}");
    assert_eq!(body["delayMinutes"], 3, "{body}");

    // What the pinned rows are for.
    sqlx::query("DELETE FROM schedule_calling_points_full WHERE uid = 'BYUID09'")
        .execute(&pool)
        .await
        .unwrap();
    let (_, body) = get(&router, &uri, None, None).await;
    assert_eq!(
        body["delayBasis"], "working",
        "without the calling points the delay falls back to the working timetable: {body}"
    );
    sqlx::query("DELETE FROM schedule_services WHERE uid = 'BYUID09'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM schedule_destination_departures WHERE train_uid = 'BYUID09'")
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = get(&router, &uri, None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["serviceMode"], "train", "no row reads as a train");
    assert_eq!(body["operatorCode"], Value::Null);
    assert_eq!(
        body["headcode"], "1A23",
        "the TRUST id still gives the headcode"
    );
    assert_eq!(body["journeyStops"].as_array().unwrap().len(), 3);
    assert!(body["journeyStops"][0]["actualDeparture"].is_string());
    assert_eq!(body["journeyStops"][0]["actualDepartureIsActual"], true);

    cleanup(&pool).await;
}
