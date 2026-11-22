//! A train leg's own copy of its train (DS-643, `data::leg_snapshot`)
//! against a live database: the sweep writes it once the leg has its train
//! and adds the outcome when the leg completes, it writes nothing twice,
//! and once the `trains` row is pruned (`trains_id` goes NULL) the journey
//! route still serves the leg's train, stops, platform and delay from it.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::NaiveDate;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;
use crate::data::leg_outcomes::snapshot_legs;

const USER: &str = "TEST-LEG-SNAPSHOT";
const UID: &str = "LEGSNAP1";

async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to postgres")
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
    sqlx::query("DELETE FROM trains WHERE train_uid LIKE 'LEGSNAP%'")
        .execute(pool)
        .await
        .expect("delete trains");
}

async fn session(pool: &PgPool) -> String {
    sqlx::query("INSERT INTO users (id, name) VALUES ($1, $1) ON CONFLICT DO NOTHING")
        .bind(USER)
        .execute(pool)
        .await
        .expect("seed user");
    let raw = format!("raw-leg-snapshot-{USER}");
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

/// A train with three timetabled calls, a platform and a TRUST id.
async fn seed_train(pool: &PgPool, uid: &str, date: NaiveDate) -> i64 {
    let calling_points = json!([
        {"tiploc": "ZZSNAPA", "kind": "Origin", "bookedDeparture": "09:00:00",
         "publicDeparture": "09:00:00", "workingDeparture": "09:00:00"},
        {"tiploc": "ZZSNAPB", "kind": "Intermediate", "bookedArrival": "09:20:00",
         "bookedDeparture": "09:21:00", "publicArrival": "09:20:00",
         "publicDeparture": "09:21:00"},
        {"tiploc": "ZZSNAPC", "kind": "Terminate", "bookedArrival": "09:40:00",
         "publicArrival": "09:40:00"},
    ]);
    // TRUST's train id is unique per date: only the first train has `1A23`.
    let train_id = if uid == UID {
        "1A23".to_owned()
    } else {
        format!("9{}", &uid[uid.len() - 3..])
    };
    sqlx::query_scalar(
        "INSERT INTO trains (train_uid, service_date, train_id, origin_crs, destination_crs, \
                             calling_points, platform, planned_platform) \
         VALUES ($1, $2, $3, 'ZTA', 'ZTC', $4, '5', '4') RETURNING id",
    )
    .bind(uid)
    .bind(date)
    .bind(train_id)
    .bind(calling_points)
    .fetch_one(pool)
    .await
    .expect("seed train")
}

/// A resolved subscription of `trains_id`, bound to a new leg of `journey`.
async fn seed_leg(
    pool: &PgPool,
    journey: i64,
    order: i32,
    date: NaiveDate,
    trains_id: Option<i64>,
) -> (i64, i64) {
    let subscription: i64 = sqlx::query_scalar(
        "INSERT INTO train_subscriptions (user_id, service_date, resolution_status, trains_id) \
         VALUES ($1, $2, 'resolved', $3) RETURNING id",
    )
    .bind(USER)
    .bind(date)
    .bind(trains_id)
    .fetch_one(pool)
    .await
    .expect("seed subscription");
    let leg: i64 = sqlx::query_scalar(
        "INSERT INTO journey_legs (journey_id, leg_order, service_date, origin_crs, \
             destination_crs, match_mode, train_subscription_id) \
         VALUES ($1, $2, $3, 'ZTA', 'ZTC', 'auto', $4) RETURNING id",
    )
    .bind(journey)
    .bind(order)
    .bind(date)
    .bind(subscription)
    .fetch_one(pool)
    .await
    .expect("seed leg");
    (leg, subscription)
}

type Stored = (
    Option<String>,
    Option<Value>,
    Option<chrono::DateTime<chrono::Utc>>,
);

async fn stored(pool: &PgPool, leg: i64) -> Stored {
    sqlx::query_as(
        "SELECT final_train_uid, final_snapshot, final_snapshot_at FROM journey_legs WHERE id = $1",
    )
    .bind(leg)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database); --test-threads=1"]
#[expect(
    clippy::too_many_lines,
    reason = "test code: one scenario, read top to bottom"
)]
async fn a_leg_keeps_its_train_after_the_train_is_pruned() {
    let pool = connect().await;
    cleanup(&pool).await;
    let cookie = session(&pool).await;
    // Winter, so London time is UTC.
    let date = NaiveDate::from_ymd_opt(2026, 1, 14).unwrap();

    let train = seed_train(&pool, UID, date).await;
    let journey: i64 =
        sqlx::query_scalar("INSERT INTO journeys (user_id) VALUES ($1) RETURNING id")
            .bind(USER)
            .fetch_one(&pool)
            .await
            .unwrap();
    let (leg, _subscription) = seed_leg(&pool, journey, 1, date, Some(train)).await;
    // A second leg whose train was pruned before any snapshot existed.
    let (lost_leg, _) = seed_leg(&pool, journey, 2, date, None).await;
    let router = router(pool.clone());
    let uri = format!("/Journeys/{journey}");

    // While the train is held the leg is live.
    let (status, detail) = get(&router, &uri, &cookie).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["legs"][0]["trainDetail"], "live");
    assert_eq!(detail["legs"][0]["snapshotAt"], Value::Null);
    assert_eq!(detail["legs"][1]["trainDetail"], "unavailable");

    // Written as soon as the leg has its train, without an outcome.
    snapshot_legs(&pool).await.unwrap();
    let (uid, snapshot, written_at) = stored(&pool, leg).await;
    assert_eq!(uid.as_deref(), Some(UID));
    let snapshot = snapshot.expect("snapshotted");
    let written_at = written_at.expect("final_snapshot_at");
    assert_eq!(snapshot["v"], 1);
    assert_eq!(snapshot["source"], "live");
    assert_eq!(snapshot["trainUid"], UID);
    assert_eq!(snapshot["trainId"], "1A23");
    assert_eq!(snapshot["platform"], "5");
    assert_eq!(snapshot["callingPoints"].as_array().unwrap().len(), 3);
    assert!(snapshot.get("finalArrivalDelayMinutes").is_none());
    assert_eq!(stored(&pool, lost_leg).await.1, None, "no source, no copy");

    // Idempotent: nothing is written again.
    snapshot_legs(&pool).await.unwrap();
    assert_eq!(stored(&pool, leg).await.2, Some(written_at));

    // The outcome is merged when the leg completes, and the row read again.
    sqlx::query("UPDATE trains SET platform = '6' WHERE id = $1")
        .bind(train)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE journey_legs SET final_arrival_delay_minutes = 7, final_train_cancelled = FALSE, \
             final_recorded_at = now() WHERE id = $1",
    )
    .bind(leg)
    .execute(&pool)
    .await
    .unwrap();
    snapshot_legs(&pool).await.unwrap();
    let (_, snapshot, completed_at) = stored(&pool, leg).await;
    let snapshot = snapshot.unwrap();
    assert_eq!(snapshot["finalArrivalDelayMinutes"], 7);
    assert_eq!(snapshot["trainCancelled"], false);
    assert_eq!(snapshot["platform"], "6", "the row is read again");
    let completed_at = completed_at.unwrap();
    assert!(completed_at > written_at);
    snapshot_legs(&pool).await.unwrap();
    assert_eq!(stored(&pool, leg).await.2, Some(completed_at), "idempotent");

    // The aggregator prunes the train: `trains_id` goes NULL.
    sqlx::query("DELETE FROM trains WHERE id = $1")
        .bind(train)
        .execute(&pool)
        .await
        .unwrap();
    let remaining: Option<i64> = sqlx::query_scalar(
        "SELECT ts.trains_id FROM train_subscriptions ts \
         JOIN journey_legs jl ON jl.train_subscription_id = ts.id WHERE jl.id = $1",
    )
    .bind(leg)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, None);
    snapshot_legs(&pool).await.unwrap();
    assert_eq!(stored(&pool, leg).await.2, Some(completed_at), "idempotent");

    let (status, detail) = get(&router, &uri, &cookie).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let served = &detail["legs"][0];
    assert_eq!(served["trainDetail"], "snapshot");
    assert!(served["snapshotAt"].is_string(), "{served}");
    let state = &served["trackedTrainState"];
    assert_eq!(state["trainUid"], UID);
    assert_eq!(state["trainId"], "1A23");
    assert_eq!(state["delayMinutes"], 7);
    assert_eq!(state["status"], "completed");
    assert_eq!(state["scheduleCallingPoints"].as_array().unwrap().len(), 3);
    let stops = state["journeyStops"]
        .as_array()
        .expect("stops from the copy");
    assert_eq!(stops.len(), 3);
    assert_eq!(stops[0]["tiploc"], "ZZSNAPA");
    assert_eq!(
        stops[0]["scheduledDeparture"], "2026-01-14T09:00:00Z",
        "the booked time"
    );
    assert_eq!(stops[0]["platform"], "6", "the origin platform");
    assert_eq!(stops[2]["tiploc"], "ZZSNAPC");
    // No snapshot, no train row: stated, not silently empty.
    assert_eq!(detail["legs"][1]["trainDetail"], "unavailable");
    assert_eq!(detail["legs"][1]["snapshotAt"], Value::Null);

    // "Change train" drops the old train's copy.
    let other = seed_train(&pool, "LEGSNAP2", date).await;
    let (_, other_subscription) = seed_leg(&pool, journey, 3, date, Some(other)).await;
    // Bind the new subscription to the snapshotted leg.
    assert!(
        crate::data::journeys::set_leg_train_subscription(
            &pool,
            journey,
            leg,
            USER,
            other_subscription
        )
        .await
        .unwrap()
    );
    assert_eq!(stored(&pool, leg).await, (None, None, None));

    cleanup(&pool).await;
}

async fn new_journey(pool: &PgPool) -> i64 {
    sqlx::query_scalar("INSERT INTO journeys (user_id) VALUES ($1) RETURNING id")
        .bind(USER)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Records a leg's outcome as the sweep does when the leg completes.
async fn record_outcome(pool: &PgPool, leg: i64, delay: Option<i32>, cancelled: bool) {
    sqlx::query(
        "UPDATE journey_legs SET final_arrival_delay_minutes = $2, final_train_cancelled = $3, \
             final_recorded_at = now() WHERE id = $1",
    )
    .bind(leg)
    .bind(delay)
    .bind(cancelled)
    .execute(pool)
    .await
    .unwrap();
}

/// `GET /Journeys/mine` says where each journey's current leg's train detail
/// comes from, and a pruned train's recorded outcome stands in for its
/// status and delay (DS-638), so a recovered leg is not read as one that
/// "got no report".
#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database); --test-threads=1"]
async fn the_journey_list_says_where_each_legs_train_detail_comes_from() {
    let pool = connect().await;
    cleanup(&pool).await;
    let cookie = session(&pool).await;
    let date = NaiveDate::from_ymd_opt(2026, 1, 14).unwrap();

    // A train still held: live.
    let held = seed_train(&pool, UID, date).await;
    let (live, _) = seed_leg(&pool, new_journey(&pool).await, 1, date, Some(held)).await;

    // Pruned after completing late: a copy with its outcome.
    let late_train = seed_train(&pool, "LEGSNAP2", date).await;
    let (late, _) = seed_leg(&pool, new_journey(&pool).await, 1, date, Some(late_train)).await;
    record_outcome(&pool, late, Some(7), false).await;

    // Pruned after being cancelled.
    let cancelled_train = seed_train(&pool, "LEGSNAP3", date).await;
    let (cancelled, _) = seed_leg(
        &pool,
        new_journey(&pool).await,
        1,
        date,
        Some(cancelled_train),
    )
    .await;
    record_outcome(&pool, cancelled, None, true).await;

    snapshot_legs(&pool).await.unwrap();
    for train in [late_train, cancelled_train] {
        sqlx::query("DELETE FROM trains WHERE id = $1")
            .bind(train)
            .execute(&pool)
            .await
            .unwrap();
    }

    // Pruned before any copy existed, with an outcome recorded.
    let (outcome_only, _) = seed_leg(&pool, new_journey(&pool).await, 1, date, None).await;
    record_outcome(&pool, outcome_only, Some(4), false).await;
    // Pruned with nothing kept at all.
    let (nothing, _) = seed_leg(&pool, new_journey(&pool).await, 1, date, None).await;

    // An untracked leg has no train.
    let untracked: i64 = sqlx::query_scalar(
        "INSERT INTO journey_legs (journey_id, leg_order, service_date, match_mode, \
             window_searched, kind, transit_mode, origin_place, destination_place, \
             planned_departure, planned_arrival) \
         VALUES ($1, 1, $2, 'manual', FALSE, 'untracked', 'bus', 'Zed', 'Yon', \
                 '2026-01-14T10:00:00Z', '2026-01-14T10:30:00Z') RETURNING id",
    )
    .bind(new_journey(&pool).await)
    .bind(date)
    .fetch_one(&pool)
    .await
    .unwrap();

    let router = router(pool.clone());
    let (status, list) = get(&router, "/Journeys/mine", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let rows = list.as_array().expect("an array");
    assert_eq!(rows.len(), 6, "{list}");
    let row = |leg: i64| {
        rows.iter()
            .find(|row| row["legId"] == leg)
            .unwrap_or_else(|| panic!("a row for leg {leg}: {list}"))
    };

    let live = row(live);
    assert_eq!(live["trainDetail"], "live");
    assert_eq!(live["snapshotAt"], Value::Null);

    let late = row(late);
    assert_eq!(late["trainDetail"], "snapshot");
    assert!(late["snapshotAt"].is_string(), "{late}");
    assert_eq!(late["status"], "completed");
    assert_eq!(late["delayMinutes"], 7);
    assert_eq!(late["delayBasis"], "public");
    assert_eq!(late["finalArrivalDelayMinutes"], 7);
    assert_eq!(late["finishedLegCount"], 1);
    assert_eq!(late["travelled"], true);

    let cancelled = row(cancelled);
    assert_eq!(cancelled["trainDetail"], "snapshot");
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["delayMinutes"], Value::Null);
    assert_eq!(cancelled["cancelledLegCount"], 1);

    let outcome_only = row(outcome_only);
    assert_eq!(outcome_only["trainDetail"], "unavailable");
    assert_eq!(outcome_only["snapshotAt"], Value::Null);
    assert_eq!(outcome_only["status"], "completed");
    assert_eq!(outcome_only["delayMinutes"], 4);

    let nothing = row(nothing);
    assert_eq!(nothing["trainDetail"], "unavailable");
    assert_eq!(nothing["status"], Value::Null);
    assert_eq!(nothing["delayMinutes"], Value::Null);
    assert_eq!(nothing["finalArrivalDelayMinutes"], Value::Null);

    let untracked = row(untracked);
    assert_eq!(untracked["trainDetail"], Value::Null);
    assert_eq!(untracked["snapshotAt"], Value::Null);

    cleanup(&pool).await;
}
