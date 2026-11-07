//! Reliability, leg ticket details and the removed ticket routes against a
//! live database (`DATABASE_URL`): a synthetic day with three trains from
//! ZLC to ZLD (one operator, LNER's `GR`), and the ways a journey's train
//! changes (Change train, a replacement, the regular journeys sweep).

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{NaiveDate, TimeZone, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;
use crate::data::reliability;
use crate::test_support::{FixtureCleanup, assert_synthetic_date};

const USER: &str = "TEST-RELIABILITY-USER";
const OTHER: &str = "TEST-RELIABILITY-OTHER";
const DATE: &str = "2099-08-08";
/// The sweep test's own day (it changes the timetable).
const SWEEP_DATE: &str = "2099-08-15";

fn day(text: &str) -> NaiveDate {
    let date: NaiveDate = text.parse().expect("date");
    assert_synthetic_date(date);
    date
}

/// The day after the fixture day: every leg on it is over.
fn day_after() -> NaiveDate {
    day(DATE).succ_opt().expect("next day")
}

async fn connect() -> PgPool {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .connect(&database_url)
        .await
        .expect("connect to postgres")
}

/// `h:m` London on `date` (BST in August).
fn london(date: NaiveDate, h: u32, m: u32) -> chrono::DateTime<Utc> {
    chrono_tz::Europe::London
        .from_local_datetime(&date.and_hms_opt(h, m, 0).expect("time"))
        .unwrap()
        .with_timezone(&Utc)
}

fn pin_at(h: u32, m: u32) -> crate::routes::PinnedLondonNow {
    crate::routes::pin_london_now_for_tests(london(day(DATE), h, m))
}

async fn cleanup(pool: &PgPool) -> FixtureCleanup {
    let mut statements = Vec::new();
    for user in [USER, OTHER] {
        statements.extend([
            format!("DELETE FROM journeys WHERE user_id = '{user}'"),
            format!("DELETE FROM journey_templates WHERE user_id = '{user}'"),
            format!("DELETE FROM tracked_train_tickets WHERE user_id = '{user}'"),
            format!("DELETE FROM train_subscriptions WHERE user_id = '{user}'"),
            format!("DELETE FROM journey_idempotency_keys WHERE user_id = '{user}'"),
            format!("DELETE FROM sessions WHERE user_id = '{user}'"),
            format!("DELETE FROM users WHERE id = '{user}'"),
        ]);
    }
    for date in [DATE, SWEEP_DATE] {
        statements.extend([
            format!("DELETE FROM trains WHERE service_date = '{date}'"),
            format!("DELETE FROM schedule_calling_points_full WHERE service_date = '{date}'"),
            format!("DELETE FROM schedule_destination_departures WHERE service_date = '{date}'"),
            format!("DELETE FROM schedule_services WHERE service_date = '{date}'"),
        ]);
    }
    statements.push("DELETE FROM tiploc_crs WHERE tiploc IN ('RLTSTC', 'RLTSTD')".to_string());
    FixtureCleanup::new(pool, statements).await
}

async fn exec(pool: &PgPool, sql: &str) {
    sqlx::query(sql).execute(pool).await.expect(sql);
}

async fn seed_session(pool: &PgPool, user_id: &str) -> String {
    exec(
        pool,
        &format!(
            "INSERT INTO users (id, name) VALUES ('{user_id}', '{user_id}') ON CONFLICT DO NOTHING"
        ),
    )
    .await;
    let raw = format!("test-raw-session-token-for-{user_id}");
    sqlx::query(
        "INSERT INTO sessions (id, user_id, refresh_token, created_at, expires_at) \
         VALUES ($1, $2, NULL, NOW(), NOW() + make_interval(days => 14)) \
         ON CONFLICT (id) DO UPDATE SET user_id = EXCLUDED.user_id, \
             expires_at = EXCLUDED.expires_at",
    )
    .bind(hash_session_token(&raw))
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed session");
    raw
}

/// Three trains ZLC (RLTSTC) to ZLD (RLTSTD) on `date`, all LNER (`GR`):
/// RL0001 10:00-11:00, RL0002 10:30-11:30, RL0003 11:00-12:00.
async fn seed_timetable(pool: &PgPool, date: &str) {
    exec(
        pool,
        "INSERT INTO tiploc_crs (tiploc, crs, station_name, stanox, source_sequence) VALUES \
         ('RLTSTC', 'ZLC', 'Reliability C', 'RLC', 1), ('RLTSTD', 'ZLD', 'Reliability D', 'RLD', 1) \
         ON CONFLICT DO NOTHING",
    )
    .await;
    for (uid, dep, arr) in [
        ("RL0001", "10:00", "11:00"),
        ("RL0002", "10:30", "11:30"),
        ("RL0003", "11:00", "12:00"),
    ] {
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_calling_points_full \
                     (service_date, uid, seq, tiploc, kind, booked_departure, public_departure, \
                      booked_arrival, public_arrival, day_offset, can_board, can_alight) VALUES \
                 ('{date}', '{uid}', 1, 'RLTSTC', 'origin', '{dep}', '{dep}', NULL, NULL, 0, TRUE, TRUE), \
                 ('{date}', '{uid}', 2, 'RLTSTD', 'terminate', NULL, NULL, '{arr}', '{arr}', 0, TRUE, TRUE)"
            ),
        )
        .await;
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_destination_departures \
                     (service_date, destination_crs, scheduled, train_uid, origin_crs, \
                      destination_arrival, operator_atoc, public_departure, \
                      public_destination_arrival) \
                 VALUES ('{date}', 'ZLD', '{dep}', '{uid}', 'ZLC', '{arr}', 'GR', '{dep}', '{arr}')"
            ),
        )
        .await;
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_services (service_date, uid, mode, stp, operator_atoc) \
                 VALUES ('{date}', '{uid}', 'train', 'P', 'GR')"
            ),
        )
        .await;
    }
}

fn router(pool: PgPool) -> axum::Router {
    crate::app::Router::new()
        .merge(crate::routes::journeys::router())
        .merge(crate::routes::train::router())
        .with_state(crate::test_support::inert_app(pool))
}

async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::COOKIE, format!("distant_signal_session={token}"));
    }
    let request = match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    let response = router.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()));
    (status, value)
}

/// A one-leg journey on `uid`, saved as the planner saves one.
async fn create_journey(router: &axum::Router, token: &str, uid: &str) -> (i64, i64) {
    let (status, body) = call(
        router,
        "POST",
        "/Journeys/itinerary",
        Some(token),
        Some(serde_json::json!({
            "legs": [{
                "trainUid": uid, "serviceDate": DATE,
                "originCrs": "ZLC", "destinationCrs": "ZLD"
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (
        body["journeyId"].as_i64().expect("journeyId"),
        body["legId"].as_i64().expect("legId"),
    )
}

/// The leg outcomes pass's result for every leg of `journey_id`
/// (`leg_outcomes`): `delay` minutes late, or cancelled.
async fn record(pool: &PgPool, journey_id: i64, delay: Option<i32>, cancelled: bool) {
    sqlx::query(
        "UPDATE journey_legs SET final_arrival_delay_minutes = $2, final_train_cancelled = $3, \
             final_recorded_at = now() \
         WHERE journey_id = $1",
    )
    .bind(journey_id)
    .bind(delay)
    .bind(cancelled)
    .execute(pool)
    .await
    .expect("record outcome");
}

async fn reliability_of(pool: &PgPool) -> reliability::ReliabilityResponse {
    reliability::for_user(pool, USER, day_after())
        .await
        .expect("reliability")
}

/// `(train UID, delay, cancelled)` of each counted train of `journey_id`.
fn trains_of(
    response: &reliability::ReliabilityResponse,
    journey_id: i64,
) -> Vec<(String, Option<i32>, bool)> {
    let mut trains: Vec<_> = response
        .trains
        .iter()
        .filter(|t| t.journey_id == journey_id)
        .map(|t| {
            (
                t.train_uid.clone().unwrap_or_default(),
                t.delay_minutes,
                t.cancelled,
            )
        })
        .collect();
    trains.sort();
    trains
}

async fn replace(
    router: &axum::Router,
    token: &str,
    journey_id: i64,
    leg_id: i64,
    cause: &str,
    uid: &str,
) -> Value {
    let (status, body) = call(
        router,
        "POST",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/replace"),
        Some(token),
        Some(serde_json::json!({
            "cause": cause,
            "legs": [{
                "trainUid": uid, "serviceDate": DATE,
                "originCrs": "ZLC", "destinationCrs": "ZLD"
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

/// Change train while planning, and a chosen replacement after the
/// journey started: each journey counts only the train it ended up on, and
/// a chosen change (`user_choice`) is no cancellation.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn change_train_and_a_chosen_replacement_count_only_the_train_taken() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, DATE).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());

    let early = pin_at(7, 0);
    let (changed, leg) = create_journey(&router, &token, "RL0001").await;
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{changed}/legs/{leg}/train"),
        Some(&token),
        Some(serde_json::json!({ "trainUid": "RL0003", "serviceDate": DATE })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (chosen, chosen_leg) = create_journey(&router, &token, "RL0001").await;
    drop(early);

    // After RL0001 left: a free choice of RL0002 is a new revision.
    let _late = pin_at(10, 15);
    let body = replace(&router, &token, chosen, chosen_leg, "choice", "RL0002").await;
    assert_eq!(body["revisionNo"], 2, "{body}");
    record(&pool, changed, Some(3), false).await;
    record(&pool, chosen, Some(0), false).await;

    let response = reliability_of(&pool).await;
    assert_eq!(
        trains_of(&response, changed),
        [("RL0003".to_string(), Some(3), false)]
    );
    assert_eq!(
        trains_of(&response, chosen),
        [("RL0002".to_string(), Some(0), false)]
    );
    // Delay Repay once per journey, on the plan chosen.
    let journey = response
        .journeys
        .iter()
        .find(|j| j.journey_id == chosen)
        .expect("the chosen journey");
    assert!(!journey.against_booked_plan);
    assert_eq!(journey.delay_repay.delay_minutes, Some(0));
    assert_eq!(response.journeys.len(), 2);
}

/// Two journeys on one train share its subscription; changing one of them
/// leaves the other's leg counted once, and the changed one counts its
/// new train.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_shared_subscription_counts_once_per_leg() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, DATE).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let _now = pin_at(7, 0);

    let (first, first_leg) = create_journey(&router, &token, "RL0001").await;
    let (second, _) = create_journey(&router, &token, "RL0001").await;
    let shared: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT train_subscription_id) FROM journey_legs \
         WHERE journey_id = ANY($1)",
    )
    .bind(vec![first, second])
    .fetch_one(&pool)
    .await
    .expect("subscriptions");
    assert_eq!(shared, 1, "one subscription for both legs");

    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{first}/legs/{first_leg}/train"),
        Some(&token),
        Some(serde_json::json!({ "trainUid": "RL0003", "serviceDate": DATE })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    record(&pool, first, Some(1), false).await;
    record(&pool, second, Some(9), false).await;

    let response = reliability_of(&pool).await;
    assert_eq!(
        trains_of(&response, first),
        [("RL0003".to_string(), Some(1), false)]
    );
    assert_eq!(
        trains_of(&response, second),
        [("RL0001".to_string(), Some(9), false)]
    );
    assert_eq!(response.trains.len(), 2, "{:?}", response.trains);
}

/// A cancelled booked train replaced through the break: one cancellation
/// for the booked train, the replacement's own delay, and the journey's
/// Delay Repay measured against the booked arrival (both). Also read
/// through the route, and refused to anyone without a session.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_cancelled_then_replaced_train_is_one_cancellation_plus_the_delay() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, DATE).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let now = pin_at(7, 0);
    let (journey_id, leg_id) = create_journey(&router, &token, "RL0001").await;

    exec(
        &pool,
        &format!(
            "INSERT INTO train_current_state (trains_id, status) \
             SELECT id, 'cancelled' FROM trains WHERE service_date = '{DATE}' AND train_uid = 'RL0001'"
        ),
    )
    .await;
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(detail["openBreak"]["cause"], "cancelled", "{detail}");
    let body = replace(&router, &token, journey_id, leg_id, "cancelled", "RL0002").await;
    assert_eq!(body["revisionNo"], 2, "{body}");
    // The replacement, booked for 11:30, arrived 5 minutes late.
    record(&pool, journey_id, Some(5), false).await;

    let response = reliability_of(&pool).await;
    assert_eq!(
        trains_of(&response, journey_id),
        [
            ("RL0001".to_string(), None, true),
            ("RL0002".to_string(), Some(5), false),
        ]
    );
    let booked = response
        .trains
        .iter()
        .find(|t| t.train_uid.as_deref() == Some("RL0001"))
        .expect("the booked train");
    assert_eq!(booked.replaced_because.as_deref(), Some("cancelled"));
    assert_eq!(booked.leg_id, None);
    let journey = &response.journeys[0];
    assert!(journey.against_booked_plan);
    // Booked to arrive 11:00, arrived 11:35.
    assert_eq!(journey.delay_repay.delay_minutes, Some(35));
    assert_eq!(journey.booked_arrival, Some(london(day(DATE), 11, 0)));
    assert_eq!(journey.actual_arrival, Some(london(day(DATE), 11, 35)));
    let estimate = journey.delay_repay.estimate.as_ref().expect("an estimate");
    assert_eq!((estimate.scheme, estimate.band_minutes), ("DR30", 30));

    // The route, the next day.
    drop(now);
    let _next_day = crate::routes::pin_london_now_for_tests(london(day_after(), 9, 0));
    let (status, body) = call(&router, "GET", "/Journeys/reliability", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["trains"].as_array().map(Vec::len), Some(2), "{body}");
    assert_eq!(body["journeys"][0]["delayMinutes"], 35, "{body}");
    assert_eq!(body["journeys"][0]["againstBookedPlan"], true, "{body}");
    assert!(
        body["trains"]
            .as_array()
            .is_some_and(|trains| trains.iter().any(|t| t["replacedBecause"] == "cancelled")),
        "{body}"
    );
    // Someone else sees none of it; no session is a 401.
    let other = seed_session(&pool, OTHER).await;
    let (status, body) = call(&router, "GET", "/Journeys/reliability", Some(&other), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["trains"], serde_json::json!([]), "{body}");
    let (status, _) = call(&router, "GET", "/Journeys/reliability", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// A train the traveller missed through their own fault is not the
/// railway's: only the train they took counts, with no cancellation.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_train_the_traveller_missed_is_not_counted() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, DATE).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let _now = pin_at(7, 0);
    let (journey_id, leg_id) = create_journey(&router, &token, "RL0001").await;
    let body = replace(&router, &token, journey_id, leg_id, "missed", "RL0002").await;
    assert_eq!(body["revisionNo"], 2, "{body}");
    let (cause, fault): (String, String) = sqlx::query_as(
        "SELECT cause, fault FROM journey_revisions WHERE journey_id = $1 AND revision_no = 2",
    )
    .bind(journey_id)
    .fetch_one(&pool)
    .await
    .expect("revision");
    assert_eq!((cause.as_str(), fault.as_str()), ("user_missed", "user"));
    record(&pool, journey_id, Some(2), false).await;

    let response = reliability_of(&pool).await;
    assert_eq!(
        trains_of(&response, journey_id),
        [("RL0002".to_string(), Some(2), false)]
    );
    // The passenger-fault change moved the baseline: measured on RL0002.
    assert!(!response.journeys[0].against_booked_plan);
    assert_eq!(response.journeys[0].delay_repay.delay_minutes, Some(2));
}

/// The Delay Repay estimate needs no ticket, on the journey page and in
/// the reliability rollup; a ticket type says single or return. Also the
/// ticket routes' ownership and their answers.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
#[expect(
    clippy::too_many_lines,
    reason = "test code: one scenario, read top to bottom"
)]
async fn delay_repay_with_and_without_a_ticket_type() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, DATE).await;
    let token = seed_session(&pool, USER).await;
    let other = seed_session(&pool, OTHER).await;
    let router = router(pool.clone());
    let _now = pin_at(7, 0);
    let (journey_id, leg_id) = create_journey(&router, &token, "RL0001").await;
    record(&pool, journey_id, Some(125), false).await;

    // No ticket: both readings of the 120-minute band.
    let response = reliability_of(&pool).await;
    let estimate = response.journeys[0]
        .delay_repay
        .estimate
        .clone()
        .expect("an estimate");
    assert_eq!(estimate.fare_basis, "return");
    assert_eq!(
        estimate.ticket_kind,
        crate::data::delay_repay_rules::TicketKind::Unknown
    );
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
    )
    .await;
    let leg = &detail["legs"][0];
    assert!(leg["ticket"].is_null(), "{leg}");
    assert_eq!(leg["delayRepay"]["ticketKnown"], false, "{leg}");
    assert_eq!(
        leg["delayRepay"]["claimUrl"], "https://delayrepay.lner.co.uk/delayrepayV2/",
        "{leg}"
    );
    assert_eq!(leg["delayRepay"]["schemeOperator"], "LNER", "{leg}");
    // Measured from the recorded outcome: the train's live data says
    // nothing here.
    assert_eq!(leg["delayRepay"]["delayMinutes"], 125, "{leg}");
    assert_eq!(
        leg["delayRepay"]["estimate"]["fareBasis"], "return",
        "{leg}"
    );
    // Nobody else sees either.
    let (status, _) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&other),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A single, valid on LNER only, with a name.
    let uri = format!("/Journeys/{journey_id}/legs/{leg_id}/ticket");
    let ticket = serde_json::json!({
        "ticketType": "single", "operatorOnly": true, "name": "  Work  "
    });
    let (status, saved) = call(&router, "PUT", &uri, Some(&token), Some(ticket.clone())).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["ticketType"], "single");
    assert_eq!(saved["operatorCode"], "GR");
    assert_eq!(saved["name"], "Work");
    assert_eq!(saved["source"], "manual");
    let (status, _) = call(&router, "PUT", &uri, Some(&other), Some(ticket)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Saving again keeps one row.
    let (status, saved) = call(
        &router,
        "PUT",
        &uri,
        Some(&token),
        Some(serde_json::json!({ "ticketType": "single", "operatorOnly": true, "name": "Work" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tracked_train_tickets WHERE user_id = $1")
            .bind(USER)
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(rows, 1);
    let (status, body) = call(
        &router,
        "PUT",
        &uri,
        Some(&token),
        Some(serde_json::json!({ "ticketType": "firstClass" })),
    )
    .await;
    assert!(status.is_client_error(), "{status} {body}");

    let response = reliability_of(&pool).await;
    let estimate = response.journeys[0]
        .delay_repay
        .estimate
        .clone()
        .expect("an estimate");
    assert_eq!(estimate.fare_basis, "single");
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
    )
    .await;
    let leg = &detail["legs"][0];
    assert_eq!(leg["ticket"]["ticketType"], "single", "{leg}");
    assert_eq!(leg["ticket"]["operatorOnly"], true, "{leg}");
    assert_eq!(leg["delayRepay"]["ticketKnown"], true, "{leg}");
    assert_eq!(
        leg["delayRepay"]["estimate"]["fareBasis"], "single",
        "{leg}"
    );

    // Removed: the estimate stays, unknown again.
    let (status, _) = call(&router, "DELETE", &uri, Some(&token), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
    )
    .await;
    assert!(detail["legs"][0]["ticket"].is_null(), "{detail}");
    assert_eq!(detail["legs"][0]["delayRepay"]["ticketKnown"], false);

    // A leg with no train yet has nowhere to keep ticket details.
    let (status, created) = call(
        &router,
        "POST",
        "/Journeys",
        Some(&token),
        Some(serde_json::json!({
            "leg": {
                "mode": "window", "originCrs": "ZLC", "destinationCrs": "ZLD",
                "serviceDate": DATE, "departWindow": {"after": "09:00:00"}
            }
        })),
    )
    .await;
    assert!(status.is_success(), "{created}");
    let (window_journey, window_leg) = (
        created["journeyId"].as_i64().expect("journeyId"),
        created["legId"].as_i64().expect("legId"),
    );
    let (status, body) = call(
        &router,
        "PUT",
        &format!("/Journeys/{window_journey}/legs/{window_leg}/ticket"),
        Some(&token),
        Some(serde_json::json!({ "ticketType": "return" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    // A Wallet pass for a leg that isn't the caller's: 404 before any
    // parse; for a leg with no train, 409.
    for (session, uri, expected) in [
        (
            &other,
            format!("/Journeys/{journey_id}/legs/{leg_id}/ticket/pkpass"),
            StatusCode::NOT_FOUND,
        ),
        (
            &token,
            format!("/Journeys/{window_journey}/legs/{window_leg}/ticket/pkpass"),
            StatusCode::CONFLICT,
        ),
    ] {
        let boundary = "reliabilityboundary";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"ticket.pkpass\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(&crate::data::ticket_precheck::fixtures::train_pkpass());
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let request = Request::builder()
            .method("POST")
            .uri(uri.as_str())
            .header(header::COOKIE, format!("distant_signal_session={session}"))
            .header(
                header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(Body::from(body))
            .expect("request");
        let response = router.clone().oneshot(request).await.expect("oneshot");
        assert_eq!(response.status(), expected, "{uri}");
    }
}

/// The ticket routes removed on 2026-11-06 answer 410, every method,
/// with or without a session.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn the_removed_ticket_routes_are_gone() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    for (method, uri) in [
        ("POST", "/Train/tickets"),
        ("GET", "/Train/tickets/mine"),
        ("POST", "/Train/tickets/pkpass"),
        ("POST", "/Train/tickets/pdf"),
        ("DELETE", "/Train/tickets/1"),
        ("POST", "/Train/tickets/1/attach"),
        ("POST", "/Train/tickets/1/name"),
        ("GET", "/Train/tickets/1/journey-leg-proposal"),
        ("GET", "/Train/1/tickets"),
        ("POST", "/Train/1/tickets"),
        ("POST", "/Train/1/tickets/pkpass"),
        ("POST", "/Train/1/tickets/pdf"),
        ("GET", "/Train/1/tickets/2/delay-repay"),
    ] {
        for session in [Some(token.as_str()), None] {
            let (status, body) = call(&router, method, uri, session, None).await;
            assert_eq!(status, StatusCode::GONE, "{method} {uri}: {body}");
            assert_eq!(
                body,
                Value::String(crate::routes::train::REMOVED_TICKET_ROUTE_MESSAGE.to_string())
            );
        }
    }
    // The tracked-train routes beside them still answer.
    let (status, _) = call(&router, "GET", "/Train/mine", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
}

/// The regular journeys sweep commits the day's train and, after an STP
/// cancellation, replans onto another: only the train it ended up on is
/// counted.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn after_the_regular_sweep_replans_only_the_new_train_counts() {
    use crate::data::regular_journeys::{self, PlanRecurrence, PlanTemplateInput, TemplateTarget};
    use crate::regular_plan_sweep::{SweepConfig, run_sweep};

    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, SWEEP_DATE).await;
    seed_session(&pool, USER).await;
    let owner = ds_store::test_support::schedule_fixture_pool(&pool).await;
    let _publishes = FixtureCleanup::new(
        &owner,
        [
            "DELETE FROM schedule_reference_publishes WHERE delivery LIKE 'TEST-RELIABILITY-%'"
                .to_string(),
        ],
    )
    .await;
    let date = day(SWEEP_DATE);
    let usual_itinerary = serde_json::from_value(serde_json::json!({
        "legs": [{
            "kind": "train", "trainUid": "RL0002", "atocCode": "GR",
            "originCrs": "ZLC", "destinationCrs": "ZLD", "departs": "10:30"
        }]
    }))
    .expect("usual itinerary");
    let regular_journeys::CreatePlanOutcome::Created { id: template, .. } =
        regular_journeys::create_plan_templates(
            &pool,
            USER,
            &PlanRecurrence {
                days_of_week: Some(127),
                starts_on: None,
                ends_on: None,
                default_match_mode: "auto".to_string(),
                skip_bank_holidays: false,
            },
            &PlanTemplateInput {
                custom_name: None,
                origin_crs: "ZLC".to_string(),
                destination_crs: "ZLD".to_string(),
                target: TemplateTarget {
                    kind: "depart_after".to_string(),
                    time: "10:15".parse().expect("time"),
                    day_offset: 0,
                },
                plan_options: None,
                usual_itinerary,
                station_buffer_minutes: None,
            },
            None,
        )
        .await
        .expect("create template")
    else {
        panic!("over the cap");
    };
    let journey = crate::data::journey_templates::materialize_template(&pool, template, USER, date)
        .await
        .expect("mint")
        .expect("template exists")
        .journey_id;
    let app = crate::test_support::inert_app(pool.clone());
    let config = SweepConfig {
        enabled: true,
        ..SweepConfig::default()
    };
    let evening = london(date.pred_opt().expect("day before"), 19, 0);
    let swept = run_sweep(&app, &config, evening).await.expect("sweep");
    assert_eq!(swept.committed, 1, "{swept:?}");

    // RL0002 is cancelled in a new publish; the morning check replans.
    for sql in [
        format!(
            "DELETE FROM schedule_calling_points_full WHERE service_date = '{SWEEP_DATE}' \
             AND uid = 'RL0002'"
        ),
        format!(
            "DELETE FROM schedule_destination_departures WHERE service_date = '{SWEEP_DATE}' \
             AND train_uid = 'RL0002'"
        ),
        "INSERT INTO schedule_reference_publishes (delivery, completed_at) \
         SELECT 'TEST-RELIABILITY-1', COALESCE(max(completed_at), now()) + interval '1 second' \
         FROM schedule_reference_publishes"
            .to_string(),
    ] {
        exec(&owner, &sql).await;
    }
    let swept = run_sweep(&app, &config, london(date, 5, 20))
        .await
        .expect("sweep");
    assert_eq!(swept.committed, 1, "{swept:?}");
    record(&pool, journey, Some(4), false).await;

    let response = reliability::for_user(&pool, USER, date.succ_opt().expect("next day"))
        .await
        .expect("reliability");
    let trains = trains_of(&response, journey);
    assert_eq!(trains.len(), 1, "{trains:?}");
    assert_ne!(trains[0].0, "RL0002", "{trains:?}");
    assert_eq!(trains[0].1, Some(4));
}
