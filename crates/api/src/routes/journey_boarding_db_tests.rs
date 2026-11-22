//! A leg's boarding-station evidence on `GET /Journeys/{id}`
//! (`legs[].delayRepay.boarding*`, DS-638) against a live database: when
//! the train actually left the station the passenger boarded at, and how
//! late that was; null, never a guess, when TRUST reported nothing there.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::NaiveDate;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;

const USER: &str = "TEST-BOARDING-DEPARTURE";
/// Winter, so London time is UTC.
const DATE: &str = "2099-01-14";

async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to postgres")
}

fn date() -> NaiveDate {
    DATE.parse().expect("date")
}

async fn cleanup(pool: &PgPool) {
    for sql in [
        "DELETE FROM journeys WHERE user_id = $1",
        "DELETE FROM train_subscriptions WHERE user_id = $1",
        "DELETE FROM sessions WHERE user_id = $1",
        "DELETE FROM users WHERE id = $1",
    ] {
        sqlx::query(sql).bind(USER).execute(pool).await.expect(sql);
    }
    for sql in [
        "DELETE FROM trains WHERE train_uid LIKE 'BDEP%'",
        "DELETE FROM schedule_calling_points_full WHERE uid LIKE 'BDEP%'",
        "DELETE FROM tiploc_crs WHERE tiploc LIKE 'BDEPTL%'",
    ] {
        sqlx::query(sql).execute(pool).await.expect(sql);
    }
}

async fn session(pool: &PgPool) -> String {
    sqlx::query("INSERT INTO users (id, name) VALUES ($1, $1) ON CONFLICT DO NOTHING")
        .bind(USER)
        .execute(pool)
        .await
        .expect("seed user");
    let raw = format!("raw-boarding-departure-{USER}");
    sqlx::query(
        "INSERT INTO sessions (id, user_id, expires_at) VALUES ($1, $2, now() + INTERVAL '1 day') \
         ON CONFLICT (id) DO UPDATE SET user_id = EXCLUDED.user_id, expires_at = EXCLUDED.expires_at",
    )
    .bind(hash_session_token(&raw))
    .bind(USER)
    .execute(pool)
    .await
    .expect("seed session");
    raw
}

fn router(pool: PgPool) -> axum::Router {
    crate::app::Router::new()
        .merge(super::journeys::router())
        .with_state(crate::test_support::inert_app(pool))
}

async fn get(router: &axum::Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::COOKIE, format!("distant_signal_session={cookie}"))
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
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

/// A train ZBA 09:00 -> ZBB 09:20/09:21 -> ZBC 09:40.
async fn seed_train(pool: &PgPool, uid: &str, train_id: &str) -> i64 {
    let calling_points = json!([
        {"tiploc": "BDEPTLA", "kind": "Origin", "bookedDeparture": "09:00:00",
         "publicDeparture": "09:00:00", "workingDeparture": "09:00:00"},
        {"tiploc": "BDEPTLB", "kind": "Intermediate", "bookedArrival": "09:20:00",
         "bookedDeparture": "09:21:00", "publicArrival": "09:20:00",
         "publicDeparture": "09:22:00", "workingArrival": "09:20:00",
         "workingDeparture": "09:21:00"},
        {"tiploc": "BDEPTLC", "kind": "Terminate", "bookedArrival": "09:40:00",
         "publicArrival": "09:40:00"},
    ]);
    sqlx::query_scalar(
        "INSERT INTO trains (train_uid, service_date, train_id, origin_crs, destination_crs, \
                             calling_points) \
         VALUES ($1, $2, $3, 'ZBA', 'ZBC', $4) RETURNING id",
    )
    .bind(uid)
    .bind(date())
    .bind(train_id)
    .bind(calling_points)
    .fetch_one(pool)
    .await
    .expect("seed train")
}

/// The same train's CIF calls, so a movement with no public time of its own
/// can be moved from the working time to the public one.
async fn seed_schedule(pool: &PgPool, uid: &str) {
    for (tiploc, crs, stanox) in [
        ("BDEPTLA", "ZBA", "BDA"),
        ("BDEPTLB", "ZBB", "BDB"),
        ("BDEPTLC", "ZBC", "BDC"),
    ] {
        sqlx::query(
            "INSERT INTO tiploc_crs (tiploc, crs, station_name, stanox, source_sequence) \
             VALUES ($1, $2, $2, $3, 1) ON CONFLICT DO NOTHING",
        )
        .bind(tiploc)
        .bind(crs)
        .bind(stanox)
        .execute(pool)
        .await
        .expect("seed tiploc_crs");
    }
    sqlx::query(
        "INSERT INTO schedule_calling_points_full \
             (service_date, uid, seq, tiploc, kind, booked_arrival, booked_departure, \
              public_arrival, public_departure, working_arrival, working_departure, \
              day_offset, can_board, can_alight) VALUES \
         ($1, $2, 0, 'BDEPTLA', 'origin', NULL, '09:00', NULL, '09:00', NULL, '09:00', 0, TRUE, FALSE), \
         ($1, $2, 1, 'BDEPTLB', 'intermediate', '09:20', '09:21', '09:20', '09:22', '09:20', '09:21', 0, TRUE, TRUE), \
         ($1, $2, 2, 'BDEPTLC', 'terminate', '09:40', NULL, '09:40', NULL, '09:40', NULL, 0, FALSE, TRUE)",
    )
    .bind(date())
    .bind(uid)
    .execute(pool)
    .await
    .expect("seed schedule");
}

#[expect(
    clippy::too_many_arguments,
    reason = "test code: one column per argument"
)]
async fn event(
    pool: &PgPool,
    trains_id: i64,
    key: &str,
    event_type: &str,
    crs: &str,
    planned: &str,
    actual: &str,
    gbtt: Option<&str>,
    age_days: i32,
) {
    let stamp = |time: &str| format!("{DATE}T{time}Z");
    sqlx::query(
        "INSERT INTO train_movement_events \
            (trains_id, dedup_key, msg_type, event_type, loc_crs, planned_timestamp, \
             actual_timestamp, gbtt_timestamp, variation_status, raw_body, received_at) \
         VALUES ($1, $2, '0003', $3, $4, $5::timestamptz, $6::timestamptz, $7::timestamptz, \
                 'LATE', '{}'::jsonb, now() - make_interval(days => $8))",
    )
    .bind(trains_id)
    .bind(format!("bdep-{key}"))
    .bind(event_type)
    .bind(crs)
    .bind(stamp(planned))
    .bind(stamp(actual))
    .bind(gbtt.map(stamp))
    .bind(age_days)
    .execute(pool)
    .await
    .expect("seed event");
}

/// A resolved subscription of `trains_id`, bound to a new leg of `journey`.
async fn seed_leg(
    pool: &PgPool,
    journey: i64,
    order: i32,
    trains_id: Option<i64>,
    origin: &str,
) -> i64 {
    let subscription: i64 = sqlx::query_scalar(
        "INSERT INTO train_subscriptions (user_id, service_date, resolution_status, trains_id) \
         VALUES ($1, $2, 'resolved', $3) RETURNING id",
    )
    .bind(USER)
    .bind(date())
    .bind(trains_id)
    .fetch_one(pool)
    .await
    .expect("seed subscription");
    sqlx::query_scalar(
        "INSERT INTO journey_legs (journey_id, leg_order, service_date, origin_crs, \
             destination_crs, match_mode, train_subscription_id) \
         VALUES ($1, $2, $3, $4, 'ZBC', 'auto', $5) RETURNING id",
    )
    .bind(journey)
    .bind(order)
    .bind(date())
    .bind(origin)
    .bind(subscription)
    .fetch_one(pool)
    .await
    .expect("seed leg")
}

fn boarding(leg: &Value) -> (&Value, &Value, &Value, &Value) {
    let repay = &leg["delayRepay"];
    for key in [
        "boardingCrs",
        "boardingActualDeparture",
        "boardingDelayMinutes",
        "boardingDelayBasis",
    ] {
        assert!(repay.get(key).is_some(), "{key} is always present: {leg}");
    }
    (
        &repay["boardingCrs"],
        &repay["boardingActualDeparture"],
        &repay["boardingDelayMinutes"],
        &repay["boardingDelayBasis"],
    )
}

#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database); --test-threads=1"]
#[expect(
    clippy::too_many_lines,
    reason = "test code: one scenario, read top to bottom"
)]
async fn a_leg_reports_when_its_train_left_the_boarding_station() {
    let pool = connect().await;
    cleanup(&pool).await;
    let cookie = session(&pool).await;
    let router = router(pool.clone());

    // 0: departure reported at the origin with TRUST's public time, 7 late,
    //    in an event received 500 days ago (a train pinned for ages).
    let reported = seed_train(&pool, "BDEP0001", "9B01").await;
    event(
        &pool,
        reported,
        "r1",
        "DEPARTURE",
        "ZBA",
        "09:00:00",
        "09:07:30",
        Some("09:00:00"),
        500,
    )
    .await;
    // A later correction for the same departure wins.
    event(
        &pool,
        reported,
        "r2",
        "DEPARTURE",
        "ZBA",
        "09:00:00",
        "09:08:10",
        Some("09:00:00"),
        499,
    )
    .await;
    // 1: only an arrival at the boarding station: no departure.
    let arrival_only = seed_train(&pool, "BDEP0002", "9B02").await;
    event(
        &pool,
        arrival_only,
        "a1",
        "ARRIVAL",
        "ZBA",
        "08:59:00",
        "08:58:00",
        Some("08:59:00"),
        0,
    )
    .await;
    // 2: no event at all.
    let silent = seed_train(&pool, "BDEP0003", "9B03").await;
    // 3: boarding at the intermediate stop ZBB. The train left ZBA 3 late
    //    (not the boarding stop) and ZBB 4.5 late, against the public 09:22
    //    from the working time TRUST sent (no public time of its own).
    let intermediate = seed_train(&pool, "BDEP0004", "9B04").await;
    seed_schedule(&pool, "BDEP0004").await;
    event(
        &pool,
        intermediate,
        "i1",
        "DEPARTURE",
        "ZBA",
        "09:00:00",
        "09:03:00",
        Some("09:00:00"),
        0,
    )
    .await;
    event(
        &pool,
        intermediate,
        "i2",
        "DEPARTURE",
        "ZBB",
        "09:21:00",
        "09:26:30",
        None,
        0,
    )
    .await;
    // 4: a departure with a time but no baseline anywhere.
    let no_baseline = seed_train(&pool, "BDEP0005", "9B05").await;
    sqlx::query(
        "INSERT INTO train_movement_events \
            (trains_id, dedup_key, msg_type, event_type, loc_crs, actual_timestamp, \
             variation_status, raw_body) \
         VALUES ($1, 'bdep-nb', '0003', 'DEPARTURE', 'ZBA', $2::timestamptz, 'LATE', '{}'::jsonb)",
    )
    .bind(no_baseline)
    .bind(format!("{DATE}T09:05:00Z"))
    .execute(&pool)
    .await
    .expect("seed event");

    let journey: i64 =
        sqlx::query_scalar("INSERT INTO journeys (user_id) VALUES ($1) RETURNING id")
            .bind(USER)
            .fetch_one(&pool)
            .await
            .unwrap();
    seed_leg(&pool, journey, 1, Some(reported), "ZBA").await;
    seed_leg(&pool, journey, 2, Some(arrival_only), "ZBA").await;
    seed_leg(&pool, journey, 3, Some(silent), "ZBA").await;
    seed_leg(&pool, journey, 4, Some(intermediate), "ZBB").await;
    seed_leg(&pool, journey, 5, Some(no_baseline), "ZBA").await;
    // 5: a train whose row was pruned (trains_id NULL).
    seed_leg(&pool, journey, 6, None, "zba").await;
    // 6: an untracked leg has no Delay Repay at all.
    sqlx::query(
        "INSERT INTO journey_legs (journey_id, leg_order, service_date, kind, transit_mode, \
             match_mode, planned_departure, planned_arrival) \
         VALUES ($1, 7, $2, 'untracked', 'bus', 'manual', $3::timestamptz, $4::timestamptz)",
    )
    .bind(journey)
    .bind(date())
    .bind(format!("{DATE}T10:00:00Z"))
    .bind(format!("{DATE}T10:30:00Z"))
    .execute(&pool)
    .await
    .expect("seed untracked leg");

    let (status, detail) = get(&router, &format!("/Journeys/{journey}"), &cookie).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let legs = detail["legs"].as_array().expect("legs");
    assert_eq!(legs.len(), 7, "{detail}");

    // Reported: the latest departure, 8 minutes late (09:08:10 vs 09:00),
    // on TRUST's public time.
    let (crs, actual, minutes, basis) = boarding(&legs[0]);
    assert_eq!(crs, "ZBA");
    assert_eq!(actual, "2099-01-14T09:08:10Z");
    assert_eq!(minutes, 8);
    assert_eq!(basis, "public");

    // Only an arrival there: not a departure, so nothing.
    let (crs, actual, minutes, basis) = boarding(&legs[1]);
    assert_eq!(crs, "ZBA");
    assert!(
        actual.is_null() && minutes.is_null() && basis.is_null(),
        "{}",
        legs[1]
    );

    // No event: nothing.
    let (crs, actual, minutes, basis) = boarding(&legs[2]);
    assert_eq!(crs, "ZBA");
    assert!(
        actual.is_null() && minutes.is_null() && basis.is_null(),
        "{}",
        legs[2]
    );

    // Intermediate boarding stop: its own departure, on the schedule's
    // public time (09:21 working + 1 minute gap: 09:26:30 - 09:22 = 4).
    let (crs, actual, minutes, basis) = boarding(&legs[3]);
    assert_eq!(crs, "ZBB");
    assert_eq!(actual, "2099-01-14T09:26:30Z");
    assert_eq!(minutes, 4);
    assert_eq!(basis, "publicSchedule");

    // A time with no baseline: the time, no delay.
    let (_, actual, minutes, basis) = boarding(&legs[4]);
    assert_eq!(actual, "2099-01-14T09:05:00Z");
    assert!(minutes.is_null() && basis.is_null(), "{}", legs[4]);

    // The pruned train: the object is there, every reading null.
    let (crs, actual, minutes, basis) = boarding(&legs[5]);
    assert_eq!(crs, "ZBA");
    assert!(
        actual.is_null() && minutes.is_null() && basis.is_null(),
        "{}",
        legs[5]
    );

    // The untracked leg.
    assert!(legs[6]["delayRepay"].is_null(), "{}", legs[6]);

    cleanup(&pool).await;
}
