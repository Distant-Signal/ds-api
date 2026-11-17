//! The mid-journey routes against a live database (`DATABASE_URL`): a
//! synthetic day with three trains from ZRC to ZRD, a journey on the
//! first, and the whole flow (miss, alternatives, accept, history, Undo).

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::NaiveDate;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;
use crate::test_support::{FixtureCleanup, assert_synthetic_date};

const USER: &str = "TEST-REPLAN-USER";
const OTHER: &str = "TEST-REPLAN-OTHER";
const DATE: &str = "2099-05-05";
/// The day after [`DATE`]: for the searches that run past its midnight.
const NEXT_DATE: &str = "2099-05-06";

fn date() -> NaiveDate {
    let date: NaiveDate = DATE.parse().expect("date");
    assert_synthetic_date(date);
    date
}

async fn connect() -> PgPool {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .connect(&database_url)
        .await
        .expect("connect to postgres")
}

/// 08:00 London on the fixture day, before the first train: the tracking
/// horizon and "now" are judged from here.
fn pin_now() -> crate::routes::PinnedLondonNow {
    use chrono::TimeZone;
    crate::routes::pin_london_now_for_tests(
        chrono::Utc.with_ymd_and_hms(2099, 5, 5, 7, 0, 0).unwrap(),
    )
}

async fn cleanup(pool: &PgPool) -> FixtureCleanup {
    let mut statements = Vec::new();
    for user in [USER, OTHER] {
        statements.extend([
            format!("DELETE FROM journeys WHERE user_id = '{user}'"),
            format!("DELETE FROM tracked_train_tickets WHERE user_id = '{user}'"),
            format!("DELETE FROM train_subscriptions WHERE user_id = '{user}'"),
            format!("DELETE FROM journey_idempotency_keys WHERE user_id = '{user}'"),
            format!("DELETE FROM notification_preferences WHERE user_id = '{user}'"),
            format!("DELETE FROM sessions WHERE user_id = '{user}'"),
            format!("DELETE FROM users WHERE id = '{user}'"),
        ]);
    }
    statements.extend([
        format!("DELETE FROM trains WHERE service_date = '{DATE}'"),
        format!("DELETE FROM schedule_calling_points_full WHERE service_date = '{DATE}'"),
        format!("DELETE FROM schedule_destination_departures WHERE service_date = '{DATE}'"),
        format!("DELETE FROM schedule_services WHERE service_date = '{DATE}'"),
        format!("DELETE FROM trains WHERE service_date = '{NEXT_DATE}'"),
        format!("DELETE FROM schedule_calling_points_full WHERE service_date = '{NEXT_DATE}'"),
        format!("DELETE FROM schedule_destination_departures WHERE service_date = '{NEXT_DATE}'"),
        format!("DELETE FROM schedule_services WHERE service_date = '{NEXT_DATE}'"),
        "DELETE FROM tiploc_crs WHERE tiploc IN ('RPTSTC', 'RPTSTD', 'RPTSTE')".to_string(),
    ]);
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

/// Three trains ZRC (RPTSTC) to ZRD (RPTSTD), all one operator: RP0001
/// 10:00-11:00, RP0002 10:30-11:30, RP0003 11:00-12:00.
async fn seed_timetable(pool: &PgPool) {
    exec(
        pool,
        "INSERT INTO tiploc_crs (tiploc, crs, station_name, stanox, source_sequence) VALUES \
         ('RPTSTC', 'ZRC', 'Replan C', 'RPC', 1), ('RPTSTD', 'ZRD', 'Replan D', 'RPD', 1)",
    )
    .await;
    for (uid, dep, arr) in [
        ("RP0001", "10:00", "11:00"),
        ("RP0002", "10:30", "11:30"),
        ("RP0003", "11:00", "12:00"),
    ] {
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_calling_points_full \
                     (service_date, uid, seq, tiploc, kind, booked_departure, public_departure, \
                      booked_arrival, public_arrival, day_offset, can_board, can_alight) VALUES \
                 ('{DATE}', '{uid}', 1, 'RPTSTC', 'origin', '{dep}', '{dep}', NULL, NULL, 0, TRUE, TRUE), \
                 ('{DATE}', '{uid}', 2, 'RPTSTD', 'terminate', NULL, NULL, '{arr}', '{arr}', 0, TRUE, TRUE)"
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
                 VALUES ('{DATE}', 'ZRD', '{dep}', '{uid}', 'ZRC', '{arr}', 'GR', '{dep}', '{arr}')"
            ),
        )
        .await;
        // Their operator, as the planner reads it (operator filtering P0).
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_services (service_date, uid, mode, stp, operator_atoc) \
                 VALUES ('{DATE}', '{uid}', 'train', 'P', 'GR')"
            ),
        )
        .await;
    }
}

/// Onward from ZRD (RPTSTD) to ZRE (RPTSTE): RP0004 11:10-12:00 and
/// RP0005 11:40-12:30, for a two-leg journey changing at ZRD.
async fn seed_onward(pool: &PgPool) {
    exec(
        pool,
        "INSERT INTO tiploc_crs (tiploc, crs, station_name, stanox, source_sequence) VALUES \
         ('RPTSTE', 'ZRE', 'Replan E', 'RPE', 1)",
    )
    .await;
    for (uid, dep, arr) in [("RP0004", "11:10", "12:00"), ("RP0005", "11:40", "12:30")] {
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_calling_points_full \
                     (service_date, uid, seq, tiploc, kind, booked_departure, public_departure, \
                      booked_arrival, public_arrival, day_offset, can_board, can_alight) VALUES \
                 ('{DATE}', '{uid}', 1, 'RPTSTD', 'origin', '{dep}', '{dep}', NULL, NULL, 0, TRUE, TRUE), \
                 ('{DATE}', '{uid}', 2, 'RPTSTE', 'terminate', NULL, NULL, '{arr}', '{arr}', 0, TRUE, TRUE)"
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
                 VALUES ('{DATE}', 'ZRE', '{dep}', '{uid}', 'ZRD', '{arr}', 'GR', '{dep}', '{arr}')"
            ),
        )
        .await;
    }
}

/// Live state for `uid`'s train: `status`, `delay` minutes late, reported
/// at `updated_at` (an RFC 3339 instant).
async fn seed_live(pool: &PgPool, uid: &str, status: &str, delay: i32, updated_at: &str) {
    let train =
        format!("SELECT id FROM trains WHERE service_date = '{DATE}' AND train_uid = '{uid}'");
    exec(
        pool,
        &format!("DELETE FROM train_current_state WHERE trains_id IN ({train})"),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO train_current_state (trains_id, status, delay_minutes, updated_at) \
             SELECT id, '{status}', {delay}, '{updated_at}' FROM ({train}) t"
        ),
    )
    .await;
}

async fn break_count(pool: &PgPool, journey_id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM journey_breaks WHERE journey_id = $1")
        .bind(journey_id)
        .fetch_one(pool)
        .await
        .expect("count breaks")
}

/// One more ZRC to ZRD train, `uid`, leaving at `dep` and arriving at
/// `arr` (both `HH:MM` on the fixture day), for a search made after the
/// first three have run.
async fn seed_late_train(pool: &PgPool, uid: &str, dep: &str, arr: &str) {
    exec(
        pool,
        &format!(
            "INSERT INTO schedule_calling_points_full \
                 (service_date, uid, seq, tiploc, kind, booked_departure, public_departure, \
                  booked_arrival, public_arrival, day_offset, can_board, can_alight) VALUES \
             ('{DATE}', '{uid}', 1, 'RPTSTC', 'origin', '{dep}', '{dep}', NULL, NULL, 0, TRUE, TRUE), \
             ('{DATE}', '{uid}', 2, 'RPTSTD', 'terminate', NULL, NULL, '{arr}', '{arr}', 0, TRUE, TRUE)"
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
             VALUES ('{DATE}', 'ZRD', '{dep}', '{uid}', 'ZRC', '{arr}', 'GR', '{dep}', '{arr}')"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO schedule_services (service_date, uid, mode, stp, operator_atoc) \
             VALUES ('{DATE}', '{uid}', 'train', 'P', 'GR')"
        ),
    )
    .await;
}

/// The instant `hour:minute` London, `days` after the fixture day.
fn london_at(days: i64, hour: u32, minute: u32) -> chrono::DateTime<chrono::Utc> {
    let local = (date() + chrono::Duration::days(days))
        .and_hms_opt(hour, minute, 0)
        .expect("time");
    crate::data::eta_blend::london_to_utc(local).expect("a London time")
}

/// The body of a `POST .../replace` taking `uid` for the leg.
fn replace_body(cause: &str, uid: &str) -> Value {
    serde_json::json!({
        "cause": cause,
        "legs": [{
            "trainUid": uid, "serviceDate": DATE,
            "originCrs": "ZRC", "destinationCrs": "ZRD"
        }]
    })
}

fn router(pool: PgPool) -> axum::Router {
    crate::app::Router::new()
        .merge(crate::routes::journeys::router())
        .nest("/public", crate::routes::preferences::router())
        .with_state(crate::test_support::inert_app(pool))
}

async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
    idempotency_key: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::COOKIE, format!("distant_signal_session={token}"));
    }
    if let Some(key) = idempotency_key {
        builder = builder.header("idempotency-key", key);
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

/// A one-leg journey on RP0001, saved as the planner saves one.
async fn create_journey(router: &axum::Router, token: &str) -> (i64, i64) {
    let (status, body) = call(
        router,
        "POST",
        "/Journeys/itinerary",
        Some(token),
        Some(serde_json::json!({
            "legs": [{
                "trainUid": "RP0001", "serviceDate": DATE,
                "originCrs": "ZRC", "destinationCrs": "ZRD"
            }]
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (
        body["journeyId"].as_i64().expect("journeyId"),
        body["legId"].as_i64().expect("legId"),
    )
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
#[expect(
    clippy::too_many_lines,
    reason = "test code: one scenario, read top to bottom"
)]
async fn a_missed_train_is_replaced_kept_in_history_and_undone() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;

    // Nothing broken yet: no banner, one plan.
    let (status, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(detail["openBreak"].is_null(), "{detail}");
    assert_eq!(detail["revisionCount"], 1, "{detail}");
    assert_eq!(detail["suggestionsMuted"], false, "{detail}");
    assert_eq!(detail["legs"][0]["replacesLegIds"], serde_json::json!([]));

    // A traveller-marked miss left open from before "I missed this train"
    // stopped opening breaks (an Undo can reopen one too), planned: its
    // suggestions, never the missed train itself.
    let seeded: i64 = sqlx::query_scalar(
        "INSERT INTO journey_breaks (journey_id, user_id, cause, fault, severity, \
             trigger_leg_id, break_position, fingerprint) \
         VALUES ($1, $2, 'user_missed', 'user', 1, $3, 1, '') RETURNING id",
    )
    .bind(journey_id)
    .bind(USER)
    .bind(leg_id)
    .fetch_one(&pool)
    .await
    .expect("seed break");
    let (status, missed) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/replan/preview"),
        Some(&token),
        Some(serde_json::json!({ "breakId": seeded })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{missed}");
    assert_eq!(missed["cause"], "user_missed");
    assert_eq!(missed["fault"], "user");
    assert_eq!(missed["triggerLegId"], leg_id);
    let suggestions = missed["suggestions"]["suggestions"]
        .as_array()
        .expect("suggestions");
    let uids: Vec<&str> = suggestions
        .iter()
        .filter_map(|s| s["legs"][0]["trainUid"].as_str())
        .collect();
    assert!(uids.contains(&"RP0002"), "{missed}");
    assert!(!uids.contains(&"RP0001"), "{missed}");
    let first = &suggestions[0];
    assert!(first["card"].as_bool().unwrap_or_default());
    assert!(
        first["pickLabels"]
            .as_array()
            .is_some_and(|labels| labels.contains(&Value::from("Recommended"))),
        "{first}"
    );
    // With no ticket, the sheet asks for one; a passenger-fault miss warns.
    assert!(missed["suggestions"]["hint"].is_string(), "{missed}");
    let break_id = missed["id"].as_i64().expect("break id");

    // The banner on the journey page, and the open break with `stale`.
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(detail["openBreak"]["id"], break_id, "{detail}");
    assert_eq!(detail["openBreak"]["suggestionsReady"], true, "{detail}");
    let (status, current) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/breaks/current?src=push"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{current}");
    assert_eq!(current["id"], break_id);
    // Stamped as first opened from the push.
    let opened_via: Option<String> =
        sqlx::query_scalar("SELECT opened_via FROM journey_breaks WHERE id = $1")
            .bind(break_id)
            .fetch_one(&pool)
            .await
            .expect("break");
    assert_eq!(opened_via.as_deref(), Some("push"));

    // Someone else gets 404 everywhere.
    let other = seed_session(&pool, OTHER).await;
    for (method, uri) in [
        ("GET", format!("/Journeys/{journey_id}/breaks/current")),
        ("GET", format!("/Journeys/{journey_id}/revisions")),
        (
            "GET",
            format!("/Journeys/{journey_id}/legs/{leg_id}/alternatives?cause=missed"),
        ),
    ] {
        let (status, body) = call(&router, method, &uri, Some(&other), None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {body}");
    }
    // And no session at all is a 401.
    let (status, _) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/revisions"),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A ticket on the booked train: it must follow the change, never be
    // cascaded away.
    let (ticket_id,): (i64,) = sqlx::query_as(
        "INSERT INTO tracked_train_tickets (tracked_train_id, user_id, ticket_type, source) \
         SELECT train_subscription_id, $2, 'Advance Single', 'manual' FROM journey_legs \
         WHERE id = $1 RETURNING id",
    )
    .bind(leg_id)
    .bind(USER)
    .fetch_one(&pool)
    .await
    .expect("seed ticket");

    // Take RP0002, twice with one Idempotency-Key: one change.
    let pick = suggestions
        .iter()
        .find(|s| s["legs"][0]["trainUid"] == "RP0002")
        .expect("RP0002 offered");
    let accept_body = serde_json::json!({
        "breakId": break_id,
        "suggestionId": pick["id"],
        "fingerprint": missed["suggestions"]["fingerprint"],
    });
    let (status, accepted) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/replan/accept"),
        Some(&token),
        Some(accept_body.clone()),
        Some("accept-once"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert_eq!(accepted["revisionNo"], 2, "{accepted}");
    assert_eq!(accepted["undoRevisionNo"], 1, "{accepted}");
    let (status, again) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/replan/accept"),
        Some(&token),
        Some(accept_body),
        Some("accept-once"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(again, accepted);

    // The journey now rides RP0002; the booked plan is kept, the missed
    // leg marked missed, and this passenger-fault change is the new
    // compensation baseline.
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(
        detail["legs"][0]["trackedTrainState"]["trainUid"], "RP0002",
        "{detail}"
    );
    assert_eq!(detail["revisionCount"], 2);
    assert_eq!(
        detail["legs"][0]["replacesLegIds"],
        serde_json::json!([leg_id])
    );
    assert!(detail["openBreak"].is_null(), "{detail}");
    let (moved,): (bool,) = sqlx::query_as(
        "SELECT t.tracked_train_id = jl.train_subscription_id FROM tracked_train_tickets t, \
         journey_legs jl WHERE t.id = $1 AND jl.journey_id = $2",
    )
    .bind(ticket_id)
    .bind(journey_id)
    .fetch_one(&pool)
    .await
    .expect("ticket");
    assert!(moved, "the ticket moved to the same operator's new train");
    let (status, history) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/revisions"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{history}");
    let revisions = history["revisions"].as_array().expect("revisions");
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0]["revisionNo"], 2);
    assert_eq!(revisions[0]["cause"], "user_missed");
    assert_eq!(revisions[0]["isCompensationBaseline"], true);
    assert_eq!(revisions[1]["reason"], "booked");
    assert_eq!(revisions[1]["legs"][0]["trainUid"], "RP0001");
    assert_eq!(revisions[1]["legs"][0]["fate"], "missed");
    assert_eq!(revisions[1]["canRevert"], true, "{history}");

    // Undo: back to the booked train, as revision 3, and the break is open
    // again.
    let (status, undone) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/revisions/1/revert"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{undone}");
    assert_eq!(undone["revisionNo"], 3);
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(
        detail["legs"][0]["trackedTrainState"]["trainUid"], "RP0001",
        "{detail}"
    );
    assert_eq!(detail["openBreak"]["id"], break_id, "{detail}");

    // The ticket is back on the booked train's subscription.
    let (on_leg,): (bool,) = sqlx::query_as(
        "SELECT t.tracked_train_id = jl.train_subscription_id FROM tracked_train_tickets t, \
         journey_legs jl WHERE t.id = $1 AND jl.journey_id = $2",
    )
    .bind(ticket_id)
    .bind(journey_id)
    .fetch_one(&pool)
    .await
    .expect("ticket");
    assert!(on_leg, "the ticket follows the plan");

    // Dismissed, it is gone.
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/breaks/{break_id}/dismiss"),
        Some(&token),
        Some(serde_json::json!({ "reason": "willMakeIt" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, _) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/breaks/current"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn choosing_another_train_before_travel_updates_the_booked_plan() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;

    let (status, alternatives) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/alternatives?cause=choice"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{alternatives}");
    assert!(
        alternatives["suggestions"]
            .as_array()
            .is_some_and(|s| !s.is_empty()),
        "{alternatives}"
    );

    let (status, replaced) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/replace"),
        Some(&token),
        Some(serde_json::json!({
            "cause": "choice",
            "legs": [{
                "trainUid": "RP0003", "serviceDate": DATE,
                "originCrs": "ZRC", "destinationCrs": "ZRD"
            }]
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    // Before travel and with no break, the booked plan itself changes.
    assert_eq!(replaced["revisionNo"], 1, "{replaced}");
    assert!(replaced["undoRevisionNo"].is_null());
    let (_, history) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/revisions"),
        Some(&token),
        None,
        None,
    )
    .await;
    let revisions = history["revisions"].as_array().expect("revisions");
    assert_eq!(revisions.len(), 1, "{history}");
    assert_eq!(revisions[0]["legs"][0]["trainUid"], "RP0003");
    assert_eq!(date().to_string(), DATE);
}

/// Taking another train after "I missed this train" before the journey
/// starts is still a missed train, not a planning edit: its own revision,
/// with the booked train kept as missed and an Undo, and no break.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_train_missed_before_departure_is_its_own_plan_with_undo() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;

    let (status, replaced) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/replace"),
        Some(&token),
        Some(serde_json::json!({
            "cause": "missed",
            "legs": [{
                "trainUid": "RP0002", "serviceDate": DATE,
                "originCrs": "ZRC", "destinationCrs": "ZRD"
            }]
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["revisionNo"], 2, "{replaced}");
    assert_eq!(replaced["undoRevisionNo"], 1, "{replaced}");
    let (_, history) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/revisions"),
        Some(&token),
        None,
        None,
    )
    .await;
    let revisions = history["revisions"].as_array().expect("revisions");
    assert_eq!(revisions[0]["cause"], "user_missed", "{history}");
    assert_eq!(revisions[0]["fault"], "user", "{history}");
    assert_eq!(revisions[1]["legs"][0]["trainUid"], "RP0001", "{history}");
    assert_eq!(revisions[1]["legs"][0]["fate"], "missed", "{history}");
    assert_eq!(break_count(&pool, journey_id).await, 0);
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn suggestions_can_be_muted_per_journey_and_turned_off() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, _) = create_journey(&router, &token).await;

    let (status, _) = call(
        &router,
        "PUT",
        &format!("/Journeys/{journey_id}/suggestions-muted"),
        Some(&token),
        Some(serde_json::json!({ "muted": true })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(detail["suggestionsMuted"], true);

    let (status, prefs) = call(
        &router,
        "GET",
        "/public/preferences/notifications",
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{prefs}");
    assert_eq!(prefs["journeySuggestions"], true, "{prefs}");
    let (status, _) = call(
        &router,
        "PUT",
        "/public/preferences/notifications",
        Some(&token),
        Some(serde_json::json!({ "journeySuggestions": false })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, prefs) = call(
        &router,
        "GET",
        "/public/preferences/notifications",
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(prefs["journeySuggestions"], false, "{prefs}");
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_cancelled_train_opens_a_break_on_read_and_it_recovers() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;

    exec(
        &pool,
        &format!(
            "INSERT INTO train_current_state (trains_id, status) \
             SELECT id, 'cancelled' FROM trains WHERE service_date = '{DATE}' AND train_uid = 'RP0001'"
        ),
    )
    .await;
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    let open = &detail["openBreak"];
    assert_eq!(open["cause"], "cancelled", "{detail}");
    assert_eq!(open["triggerLegId"], leg_id);
    assert_eq!(open["suggestionsReady"], false);
    let break_id = open["id"].as_i64().expect("break id");

    // The sheet re-plans the stale (unplanned) break.
    let (status, preview) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/replan/preview"),
        Some(&token),
        Some(serde_json::json!({ "breakId": break_id })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["stale"], false, "{preview}");
    assert!(
        preview["suggestions"]["suggestions"]
            .as_array()
            .is_some_and(|s| !s.is_empty()),
        "{preview}"
    );
    // The journey list says so too.
    let (_, mine) = call(&router, "GET", "/Journeys/mine", Some(&token), None, None).await;
    let row = mine
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == journey_id))
        .expect("journey listed");
    assert_eq!(row["openBreakId"], break_id, "{row}");

    // Reinstated: the next read ends the break.
    exec(
        &pool,
        &format!(
            "UPDATE train_current_state SET status = 'awaiting_activation' WHERE trains_id IN \
             (SELECT id FROM trains WHERE service_date = '{DATE}' AND train_uid = 'RP0001')"
        ),
    )
    .await;
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert!(detail["openBreak"].is_null(), "{detail}");
    let (status,): (String,) = sqlx::query_as("SELECT status FROM journey_breaks WHERE id = $1")
        .bind(break_id)
        .fetch_one(&pool)
        .await
        .expect("break");
    assert_eq!(status, "recovered");
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn the_sweep_opens_a_break_plans_it_and_ends_it_once_recovered() {
    use chrono::TimeZone;
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;
    let app = crate::test_support::inert_app(pool.clone());
    let now = chrono::Utc.with_ymd_and_hms(2099, 5, 5, 8, 30, 0).unwrap();

    // Nothing broken: nothing written.
    let swept = crate::journey_break_sweep::run_sweep(&app, now)
        .await
        .expect("sweep");
    assert!(swept.journeys >= 1, "{swept:?}");
    assert_eq!(swept.breaks, 0);

    exec(
        &pool,
        &format!(
            "INSERT INTO train_current_state (trains_id, status) \
             SELECT id, 'cancelled' FROM trains WHERE service_date = '{DATE}' AND train_uid = 'RP0001'"
        ),
    )
    .await;
    let swept = crate::journey_break_sweep::run_sweep(&app, now)
        .await
        .expect("sweep");
    assert_eq!((swept.breaks, swept.planned), (1, 1), "{swept:?}");
    let row = crate::data::journey_breaks::open_break(&pool, journey_id)
        .await
        .expect("read")
        .expect("open break");
    assert_eq!(row.cause, "cancelled");
    assert_eq!(row.trigger_leg_id, Some(leg_id));
    let suggestions = row.suggestions.expect("planned");
    assert!(
        suggestions["suggestions"]
            .as_array()
            .is_some_and(|s| !s.is_empty()),
        "{suggestions}"
    );
    assert_eq!(suggestions["trigger"]["legId"], leg_id);

    // Seen again unchanged: confirmed, not planned again.
    let swept = crate::journey_break_sweep::run_sweep(&app, now)
        .await
        .expect("sweep");
    assert_eq!((swept.breaks, swept.planned), (1, 0), "{swept:?}");
    let row = crate::data::journey_breaks::open_break(&pool, journey_id)
        .await
        .expect("read")
        .expect("open break");
    assert_eq!(row.confirmations, 2);

    exec(
        &pool,
        &format!(
            "UPDATE train_current_state SET status = 'awaiting_activation' WHERE trains_id IN \
             (SELECT id FROM trains WHERE service_date = '{DATE}' AND train_uid = 'RP0001')"
        ),
    )
    .await;
    let swept = crate::journey_break_sweep::run_sweep(&app, now)
        .await
        .expect("sweep");
    assert_eq!(swept.recovered, 1, "{swept:?}");
    assert!(
        crate::data::journey_breaks::open_break(&pool, journey_id)
            .await
            .expect("read")
            .is_none()
    );
}

/// The prod report (2026-10-22): "You missed the ..." on the first leg of a
/// journey whose train left on time. Departure time passing is no evidence
/// of a miss, and looking at the options "I missed this train" offers is
/// not saying so: neither opens a break. Only taking another train records
/// the miss, and only then as the traveller's.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn an_on_time_first_leg_after_departure_is_never_missed_until_the_traveller_says_so() {
    use chrono::TimeZone;
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;

    // 10:15 London: RP0001 left on time at 10:00 and is on its way.
    let _later = crate::routes::pin_london_now_for_tests(
        chrono::Utc.with_ymd_and_hms(2099, 5, 5, 9, 15, 0).unwrap(),
    );
    seed_live(&pool, "RP0001", "en_route", 0, "2099-05-05T09:14:00Z").await;
    let journey = format!("/Journeys/{journey_id}");
    let (status, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert!(detail["openBreak"].is_null(), "{detail}");
    assert!(detail["legs"][0]["connection"].is_null(), "{detail}");

    // "I missed this train" shows the options, and that is all it does.
    let (status, alternatives) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/alternatives?cause=missed"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{alternatives}");
    let offered: Vec<&str> = alternatives["suggestions"]
        .as_array()
        .expect("suggestions")
        .iter()
        .filter_map(|s| s["legs"][0]["trainUid"].as_str())
        .collect();
    assert!(offered.contains(&"RP0002"), "{alternatives}");
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert!(detail["openBreak"].is_null(), "{detail}");
    assert_eq!(break_count(&pool, journey_id).await, 0);

    // The old "mark it missed" route is gone: a stale tab calling it gets
    // a plain 404, and nothing is written.
    let (status, _) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/missed"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(break_count(&pool, journey_id).await, 0);

    // Taking RP0002 is the traveller saying they missed RP0001: their own
    // fault (no change onto a first leg), kept in the history, no banner.
    let (status, replaced) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/replace"),
        Some(&token),
        Some(serde_json::json!({
            "cause": "missed",
            "legs": [{
                "trainUid": "RP0002", "serviceDate": DATE,
                "originCrs": "ZRC", "destinationCrs": "ZRD"
            }]
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    let (_, history) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/revisions"),
        Some(&token),
        None,
        None,
    )
    .await;
    let revisions = history["revisions"].as_array().expect("revisions");
    assert_eq!(revisions[0]["cause"], "user_missed", "{history}");
    assert_eq!(revisions[0]["fault"], "user", "{history}");
    assert_eq!(revisions[1]["legs"][0]["fate"], "missed", "{history}");
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert!(detail["openBreak"].is_null(), "{detail}");
    assert_eq!(break_count(&pool, journey_id).await, 0);
}

/// A later leg is missed only when the inbound train's live arrival leaves
/// too little time to change: on time, nothing; 30 minutes late into a
/// 10-minute change, the connection is missed and the page opens the
/// break on the later leg, never on the first.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_late_inbound_breaks_the_connection_onto_the_later_leg_only() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_onward(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (status, body) = call(
        &router,
        "POST",
        "/Journeys/itinerary",
        Some(&token),
        Some(serde_json::json!({
            "legs": [
                { "trainUid": "RP0001", "serviceDate": DATE, "originCrs": "ZRC", "destinationCrs": "ZRD" },
                { "trainUid": "RP0004", "serviceDate": DATE, "originCrs": "ZRD", "destinationCrs": "ZRE" }
            ]
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let journey_id = body["journeyId"].as_i64().expect("journeyId");
    let journey = format!("/Journeys/{journey_id}");

    seed_live(&pool, "RP0001", "en_route", 0, "2099-05-05T07:00:00Z").await;
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert!(detail["openBreak"].is_null(), "{detail}");
    assert!(detail["legs"][0]["connection"].is_null(), "{detail}");
    assert_eq!(detail["legs"][1]["connection"]["state"], "ok", "{detail}");

    seed_live(&pool, "RP0001", "en_route", 30, "2099-05-05T07:00:00Z").await;
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert!(detail["legs"][0]["connection"].is_null(), "{detail}");
    assert_eq!(
        detail["legs"][1]["connection"]["state"], "missed",
        "{detail}"
    );
    let open = &detail["openBreak"];
    assert_eq!(open["cause"], "connection_missed", "{detail}");
    assert_eq!(open["triggerLegId"], detail["legs"][1]["id"], "{detail}");
}

/// The cleanup migration: an open `user_missed` break (left by the old
/// sheet, which opened one just by loading) is dismissed, a settled one is
/// left alone, and a second run changes nothing.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn the_cleanup_dismisses_unconfirmed_missed_breaks_once() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;
    let insert = |status: &str| {
        format!(
            "INSERT INTO journey_breaks (journey_id, user_id, cause, fault, severity, \
                 trigger_leg_id, break_position, fingerprint, status) \
             VALUES ({journey_id}, '{USER}', 'user_missed', 'user', 1, {leg_id}, 1, 'f', \
                 '{status}') RETURNING id"
        )
    };
    let settled: i64 = sqlx::query_scalar(&insert("accepted"))
        .fetch_one(&pool)
        .await
        .expect("settled break");
    let open: i64 = sqlx::query_scalar(&insert("open"))
        .fetch_one(&pool)
        .await
        .expect("open break");
    let journey = format!("/Journeys/{journey_id}");
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert_eq!(detail["openBreak"]["id"], open, "{detail}");

    let migration = ds_store::migrate::migration_sql(20_261_023_091_000);
    for _ in 0..2 {
        let mut tx = pool.begin().await.expect("begin");
        sqlx::raw_sql(migration)
            .execute(&mut *tx)
            .await
            .expect("cleanup");
        tx.commit().await.expect("commit");
    }
    let rows: Vec<(i64, String, Option<String>)> = sqlx::query_as(
        "SELECT id, status, dismiss_reason FROM journey_breaks WHERE journey_id = $1 ORDER BY id",
    )
    .bind(journey_id)
    .fetch_all(&pool)
    .await
    .expect("breaks");
    assert_eq!(
        rows,
        vec![
            (settled, "accepted".to_string(), None),
            (open, "dismissed".to_string(), Some("other".to_string())),
        ]
    );
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert!(detail["openBreak"].is_null(), "{detail}");
}

/// DS-141: a change that works only because the next train is running
/// late. RP0001 (ZRC 10:00 to ZRD 11:00) then RP0004 (ZRD 11:10), five
/// minutes to change at ZRD; "now" is 10:30. RP0001 30 late is in at 11:30
/// and RP0004 30 late leaves 11:40: the change is `ok`, and flagged, since
/// on time RP0004 would leave 25 minutes before the traveller could reach
/// it. With RP0001 on time the change works on time too: not flagged. The
/// alternatives from RP0001 carry the same flag on a late RP0004 boarded
/// from RP0002 (in 11:30).
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_change_that_relies_on_a_late_next_train_is_flagged() {
    use chrono::TimeZone;
    let _now = crate::routes::pin_london_now_for_tests(
        chrono::Utc.with_ymd_and_hms(2099, 5, 5, 9, 30, 0).unwrap(),
    );
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_onward(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (status, body) = call(
        &router,
        "POST",
        "/Journeys/itinerary",
        Some(&token),
        Some(serde_json::json!({
            "legs": [
                { "trainUid": "RP0001", "serviceDate": DATE, "originCrs": "ZRC", "destinationCrs": "ZRD" },
                { "trainUid": "RP0004", "serviceDate": DATE, "originCrs": "ZRD", "destinationCrs": "ZRE" }
            ]
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let journey_id = body["journeyId"].as_i64().expect("journeyId");
    let journey = format!("/Journeys/{journey_id}");

    seed_live(&pool, "RP0001", "en_route", 30, "2099-05-05T09:29:00Z").await;
    seed_live(&pool, "RP0004", "en_route", 30, "2099-05-05T09:29:00Z").await;
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    let connection = &detail["legs"][1]["connection"];
    assert_eq!(connection["state"], "ok", "{detail}");
    assert_eq!(connection["gapMinutes"], 10, "{detail}");
    assert_eq!(
        connection["reliesOnLateDeparture"],
        serde_json::json!({
            "scheduledDeparture": "2099-05-05T10:10:00Z",
            "expectedDeparture": "2099-05-05T10:40:00Z",
            "delayMinutes": 30,
            "slackIfOnTimeMinutes": -25,
        }),
        "{detail}"
    );
    assert!(detail["openBreak"].is_null(), "{detail}");

    seed_live(&pool, "RP0001", "en_route", 0, "2099-05-05T09:29:00Z").await;
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    let connection = &detail["legs"][1]["connection"];
    assert_eq!(connection["state"], "ok", "{detail}");
    assert!(
        connection.get("reliesOnLateDeparture").is_none(),
        "{detail}"
    );

    let leg_id = detail["legs"][0]["id"].as_i64().expect("leg id");
    let (status, alternatives) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/alternatives?cause=choice"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{alternatives}");
    let suggestions = alternatives["suggestions"].as_array().expect("suggestions");
    let mut flagged = 0;
    for suggestion in suggestions {
        let legs = suggestion["legs"].as_array().expect("legs");
        for pair in legs.windows(2) {
            let reliance = pair[1].get("reliesOnLateDeparture");
            if pair[0]["trainUid"] == "RP0002" && pair[1]["trainUid"] == "RP0004" {
                let reliance = reliance.expect("RP0002 into RP0004 relies on it running late");
                assert_eq!(reliance["delayMinutes"], 30, "{suggestion}");
                flagged += 1;
            } else {
                assert!(reliance.is_none(), "{suggestion}");
            }
        }
    }
    assert!(flagged > 0, "{alternatives}");
}

/// Operator filtering P5 (spec 2026-10-31 §5.6, §6.9): a journey keeps the
/// operator filter its plan was saved with, its page echoes it, `PUT
/// .../operator-filter` changes or clears it, and the replans honour it,
/// falling back to the best unfiltered route, flagged, when it leaves none.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
#[expect(
    clippy::too_many_lines,
    reason = "test code: one scenario, read top to bottom"
)]
async fn a_journeys_operator_filter_is_kept_echoed_and_honoured_by_replans() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());

    // Saved with "only LNER" (every fixture train is LNER).
    let (status, body) = call(
        &router,
        "POST",
        "/Journeys/itinerary",
        Some(&token),
        Some(serde_json::json!({
            "legs": [{
                "trainUid": "RP0001", "serviceDate": DATE,
                "originCrs": "ZRC", "destinationCrs": "ZRD"
            }],
            "operatorFilter": {"mode": "only", "entries": ["gr"]}
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let journey_id = body["journeyId"].as_i64().unwrap();
    let leg_id = body["legId"].as_i64().unwrap();
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(
        detail["operatorFilter"],
        serde_json::json!({"mode": "only", "codes": ["GR"], "entries": ["GR"]}),
        "{detail}"
    );

    // A missed train: the replan rides LNER, no flag.
    let seeded: i64 = sqlx::query_scalar(
        "INSERT INTO journey_breaks (journey_id, user_id, cause, fault, severity, \
             trigger_leg_id, break_position, fingerprint) \
         VALUES ($1, $2, 'user_missed', 'user', 1, $3, 1, '') RETURNING id",
    )
    .bind(journey_id)
    .bind(USER)
    .bind(leg_id)
    .fetch_one(&pool)
    .await
    .expect("seed break");
    let preview = |body: Value| {
        let router = router.clone();
        let token = token.clone();
        async move {
            call(
                &router,
                "POST",
                &format!("/Journeys/{journey_id}/replan/preview"),
                Some(&token),
                Some(body),
                None,
            )
            .await
        }
    };
    let (status, missed) = preview(serde_json::json!({ "breakId": seeded })).await;
    assert_eq!(status, StatusCode::OK, "{missed}");
    let suggestions = missed["suggestions"]["suggestions"].as_array().unwrap();
    assert!(!suggestions.is_empty(), "{missed}");
    assert!(
        suggestions
            .iter()
            .all(|s| s.get("outsideOperatorFilter").is_none()),
        "{missed}"
    );

    // Avoiding LNER leaves nothing: the best route anyway, flagged.
    let (status, body) = call(
        &router,
        "PUT",
        &format!("/Journeys/{journey_id}/operator-filter"),
        Some(&token),
        Some(serde_json::json!({ "operatorFilter": {"mode": "avoid", "entries": ["GR"]} })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, avoided) = preview(serde_json::json!({ "breakId": seeded })).await;
    assert_eq!(status, StatusCode::OK, "{avoided}");
    let suggestions = avoided["suggestions"]["suggestions"].as_array().unwrap();
    assert_eq!(suggestions.len(), 1, "{avoided}");
    assert_eq!(suggestions[0]["outsideOperatorFilter"], true, "{avoided}");

    // Preferring LNER labels its route.
    call(
        &router,
        "PUT",
        &format!("/Journeys/{journey_id}/operator-filter"),
        Some(&token),
        Some(serde_json::json!({ "operatorFilter": {"mode": "prefer", "entries": ["GR"]} })),
        None,
    )
    .await;
    let (_, preferred) = preview(serde_json::json!({ "breakId": seeded })).await;
    let suggestions = preferred["suggestions"]["suggestions"].as_array().unwrap();
    assert!(
        suggestions
            .iter()
            .any(|s| s["operatorOnly"] == serde_json::json!(["GR"])),
        "{preferred}"
    );

    // A bad filter is a 400; someone else's journey a 404; null clears it.
    let (status, _) = call(
        &router,
        "PUT",
        &format!("/Journeys/{journey_id}/operator-filter"),
        Some(&token),
        Some(serde_json::json!({ "operatorFilter": {"mode": "sometimes", "entries": ["GR"]} })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = call(
        &router,
        "PUT",
        &format!("/Journeys/{journey_id}/operator-filter"),
        Some(&token),
        Some(serde_json::json!({ "operatorFilter": {"mode": "only", "entries": ["Q9"]} })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let other = seed_session(&pool, OTHER).await;
    let (status, _) = call(
        &router,
        "PUT",
        &format!("/Journeys/{journey_id}/operator-filter"),
        Some(&other),
        Some(serde_json::json!({ "operatorFilter": null })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &router,
        "PUT",
        &format!("/Journeys/{journey_id}/operator-filter"),
        Some(&token),
        Some(serde_json::json!({ "operatorFilter": null })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert!(detail.get("operatorFilter").is_none(), "{detail}");

    // Saving with a bad filter is refused, and saves nothing.
    let (status, body) = call(
        &router,
        "POST",
        "/Journeys/itinerary",
        Some(&token),
        Some(serde_json::json!({
            "legs": [{"trainUid": "RP0002", "serviceDate": DATE}],
            "operatorFilter": {"mode": "only", "entries": []}
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

/// A timetabled tram leg (DS-237) between RP0001 and RP0005, on the test
/// transit network: the tram line every ten minutes from 11:05 London
/// (20 minutes to the far stop), the leg planned on the 11:05. Six minutes
/// late into ZRD, the traveller misses it and takes the 11:15, in at
/// 11:35, leaving five minutes for the 11:40: at risk. Shifting the
/// planned ride instead would have ended it at 11:26 and called the
/// change fine.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_late_train_re_times_a_timetabled_leg_and_judges_the_next_change_from_it() {
    use common::transit::{TimetableRows, TransitTrip};
    use ds_store::test_support::transit::{A, B, cleanup as transit_cleanup, feed, seed_feed};
    const NETWORK: &str = "test-replan-tt";
    let _now = pin_now();
    let _switch = crate::data::transit_planning::set_switch_for_tests(&[NETWORK]);
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_onward(&pool).await;
    let (mut parts, mut manifest) = feed(NETWORK, date());
    let starts = [11 * 3600 + 300, 11 * 3600 + 900, 11 * 3600 + 1500];
    for (part, _) in &mut parts {
        match &mut part.rows {
            TimetableRows::Patterns(patterns) => {
                for pattern in patterns.iter_mut().filter(|p| p.pattern_id == "out") {
                    pattern.stops[1].arr_offset_s = 1200;
                    pattern.stops[1].dep_offset_s = 1200;
                }
            }
            TimetableRows::Trips(trips) => {
                *trips = starts
                    .iter()
                    .map(|start| TransitTrip {
                        trip_id: format!("T{start}"),
                        line_id: Some("blue".to_owned()),
                        pattern_id: "out".to_owned(),
                        service_id: "s1".to_owned(),
                        start_s: *start,
                        headsign: Some("Bbb".to_owned()),
                        replacement_bus: false,
                    })
                    .collect();
            }
            _ => {}
        }
    }
    manifest.counts.trips = 3;
    seed_feed(&pool, &parts, &manifest, date()).await;

    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (status, body) = call(
        &router,
        "POST",
        "/Journeys/itinerary",
        Some(&token),
        Some(serde_json::json!({
            "legs": [
                { "trainUid": "RP0001", "serviceDate": DATE, "originCrs": "ZRC", "destinationCrs": "ZRD" },
                {
                    "kind": "untracked", "transitMode": "tram",
                    "originStopId": A, "destinationStopId": B,
                    "plannedDeparture": "2099-05-05T10:05:00Z", "plannedArrival": "2099-05-05T10:25:00Z",
                    "transitNetwork": NETWORK, "transitLineId": "blue", "headsign": "Bbb"
                },
                { "trainUid": "RP0005", "serviceDate": DATE, "originCrs": "ZRD", "destinationCrs": "ZRE" }
            ]
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let journey = format!(
        "/Journeys/{}",
        body["journeyId"].as_i64().expect("journeyId")
    );

    // On time: the planned tram, and the 11:40 with 15 minutes to spare.
    seed_live(&pool, "RP0001", "en_route", 0, "2099-05-05T07:00:00Z").await;
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert_eq!(
        detail["legs"][1]["timetable"]["state"], "matched",
        "{detail}"
    );
    assert!(
        detail["legs"][1]["connection"]
            .get("nextTimetabled")
            .is_none(),
        "{detail}"
    );
    assert_eq!(detail["legs"][2]["connection"]["state"], "ok", "{detail}");

    seed_live(&pool, "RP0001", "en_route", 6, "2099-05-05T07:00:00Z").await;
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    let onto_tram = &detail["legs"][1]["connection"];
    assert_eq!(onto_tram["state"], "ok", "{detail}");
    assert_eq!(onto_tram["gapMinutes"], 9, "{detail}");
    assert_eq!(
        onto_tram["nextTimetabled"],
        serde_json::json!({
            "departure": "2099-05-05T10:15:00Z",
            "arrival": "2099-05-05T10:35:00Z",
        })
    );
    assert_eq!(
        detail["legs"][2]["connection"]["state"], "atRisk",
        "{detail}"
    );
    assert_eq!(detail["legs"][2]["connection"]["gapMinutes"], 5, "{detail}");
    transit_cleanup(&pool, NETWORK).await;
}

/// A dismissed break stays dismissed (review J1): neither the next page
/// read nor the sweep opens the same cancellation again, so the banner and
/// the push never come back for it.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_dismissed_break_is_not_opened_again_for_the_same_cause() {
    use chrono::TimeZone;
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, _) = create_journey(&router, &token).await;
    seed_live(&pool, "RP0001", "cancelled", 0, "2099-05-05T07:00:00Z").await;
    let journey = format!("/Journeys/{journey_id}");

    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    let break_id = detail["openBreak"]["id"].as_i64().expect("open break");
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/breaks/{break_id}/dismiss"),
        Some(&token),
        Some(serde_json::json!({ "reason": "notTravelling" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert!(detail["openBreak"].is_null(), "{detail}");
    let app = crate::test_support::inert_app(pool.clone());
    let now = chrono::Utc.with_ymd_and_hms(2099, 5, 5, 8, 30, 0).unwrap();
    for _ in 0..2 {
        let swept = crate::journey_break_sweep::run_sweep(&app, now)
            .await
            .expect("sweep");
        assert_eq!(swept.breaks, 0, "{swept:?}");
    }
    assert_eq!(break_count(&pool, journey_id).await, 1);
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert!(detail["openBreak"].is_null(), "{detail}");
}

/// A dismissed break holds a repeat back only until the leg recovers
/// (review J1 follow-up): an ETA tick alone never reopens it, a recovery
/// marks it cleared, and the same cancellation after that is a new break.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_dismissed_break_reopens_for_the_same_cause_only_after_the_leg_recovers() {
    use chrono::TimeZone;
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, _) = create_journey(&router, &token).await;
    seed_live(&pool, "RP0001", "cancelled", 0, "2099-05-05T07:00:00Z").await;
    let journey = format!("/Journeys/{journey_id}");
    let app = crate::test_support::inert_app(pool.clone());
    let now = chrono::Utc.with_ymd_and_hms(2099, 5, 5, 8, 30, 0).unwrap();
    let cleared = || async {
        let (cleared,): (bool,) = sqlx::query_as(
            "SELECT cleared_at IS NOT NULL FROM journey_breaks WHERE journey_id = $1 \
             ORDER BY id LIMIT 1",
        )
        .bind(journey_id)
        .fetch_one(&pool)
        .await
        .expect("first break");
        cleared
    };

    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    let break_id = detail["openBreak"]["id"].as_i64().expect("open break");
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/breaks/{break_id}/dismiss"),
        Some(&token),
        Some(serde_json::json!({ "reason": "notTravelling" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // Still cancelled, and the live times move (an ETA tick): no new break.
    seed_live(&pool, "RP0001", "cancelled", 7, "2099-05-05T07:05:00Z").await;
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert!(detail["openBreak"].is_null(), "{detail}");
    let swept = crate::journey_break_sweep::run_sweep(&app, now)
        .await
        .expect("sweep");
    assert_eq!(swept.breaks, 0, "{swept:?}");
    assert_eq!(break_count(&pool, journey_id).await, 1);
    assert!(!cleared().await);

    // The leg recovers: the read marks the dismissed break cleared.
    exec(
        &pool,
        &format!(
            "UPDATE train_current_state SET status = 'awaiting_activation' WHERE trains_id IN \
             (SELECT id FROM trains WHERE service_date = '{DATE}' AND train_uid = 'RP0001')"
        ),
    )
    .await;
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert!(detail["openBreak"].is_null(), "{detail}");
    assert!(cleared().await);
    assert_eq!(break_count(&pool, journey_id).await, 1);

    // Cancelled again, the same way: a new break opens, by read and sweep.
    seed_live(&pool, "RP0001", "cancelled", 0, "2099-05-05T07:10:00Z").await;
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    let new_id = detail["openBreak"]["id"].as_i64().expect("a new break");
    assert_ne!(new_id, break_id, "{detail}");
    assert_eq!(break_count(&pool, journey_id).await, 2);
    let swept = crate::journey_break_sweep::run_sweep(&app, now)
        .await
        .expect("sweep");
    assert_eq!(swept.recovered, 0, "{swept:?}");
    assert_eq!(break_count(&pool, journey_id).await, 2);
}

/// The sweep marks a dismissed break cleared when it finds the leg
/// healthy, with no page read.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn the_sweep_marks_a_dismissed_break_cleared_once_the_leg_recovers() {
    use chrono::TimeZone;
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, _) = create_journey(&router, &token).await;
    seed_live(&pool, "RP0001", "cancelled", 0, "2099-05-05T07:00:00Z").await;
    let journey = format!("/Journeys/{journey_id}");
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    let break_id = detail["openBreak"]["id"].as_i64().expect("open break");
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/breaks/{break_id}/dismiss"),
        Some(&token),
        Some(serde_json::json!({ "reason": "notTravelling" })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    exec(
        &pool,
        &format!(
            "UPDATE train_current_state SET status = 'awaiting_activation' WHERE trains_id IN \
             (SELECT id FROM trains WHERE service_date = '{DATE}' AND train_uid = 'RP0001')"
        ),
    )
    .await;
    let app = crate::test_support::inert_app(pool.clone());
    let now = chrono::Utc.with_ymd_and_hms(2099, 5, 5, 8, 30, 0).unwrap();
    crate::journey_break_sweep::run_sweep(&app, now)
        .await
        .expect("sweep");
    let (cleared,): (bool,) =
        sqlx::query_as("SELECT cleared_at IS NOT NULL FROM journey_breaks WHERE id = $1")
            .bind(break_id)
            .fetch_one(&pool)
            .await
            .expect("break");
    assert!(cleared);
    // Marked once: a second healthy sweep leaves it be.
    assert!(
        !crate::data::journey_breaks::clear_dismissed(&pool, journey_id)
            .await
            .expect("clear")
    );
}

/// A skipped stop is judged alike by a page read and the sweep (review
/// J2): the break a read opens is confirmed by the sweep, not ended as
/// recovered and opened again by the next read.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn the_sweep_keeps_a_skipped_stop_break_a_page_read_opened() {
    use chrono::TimeZone;
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    exec(&pool, "DELETE FROM station_samples WHERE crs = 'ZRC'").await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;
    // RP0001's board at ZRC says it no longer calls at ZRD (the board is
    // matched by the train's destination).
    exec(
        &pool,
        &format!(
            "UPDATE train_subscriptions SET pin_destination_crs = 'ZRD' WHERE user_id = '{USER}'"
        ),
    )
    .await;
    sqlx::query(
        "INSERT INTO station_samples (crs, polled_at, departures) VALUES ('ZRC', NOW(), $1)",
    )
    .bind(serde_json::json!([{
        "service_id": "replan-skip", "operator": "GR", "destination_crs": "ZRD",
        "scheduled": "10:00", "estimated": "On time", "is_cancelled": false,
        "delay_minutes": 0, "skipped_stations": ["ZRD"], "calling_points": []
    }]))
    .execute(&pool)
    .await
    .expect("seed board");

    let journey = format!("/Journeys/{journey_id}");
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert_eq!(detail["openBreak"]["cause"], "skipped_stop", "{detail}");
    assert_eq!(detail["openBreak"]["triggerLegId"], leg_id, "{detail}");
    let break_id = detail["openBreak"]["id"].as_i64().expect("break id");

    let app = crate::test_support::inert_app(pool.clone());
    let now = chrono::Utc.with_ymd_and_hms(2099, 5, 5, 8, 30, 0).unwrap();
    let swept = crate::journey_break_sweep::run_sweep(&app, now)
        .await
        .expect("sweep");
    assert_eq!((swept.breaks, swept.recovered), (1, 0), "{swept:?}");
    let row = crate::data::journey_breaks::open_break(&pool, journey_id)
        .await
        .expect("read")
        .expect("still open");
    assert_eq!(row.id, break_id);
    assert_eq!(row.cause, "skipped_stop");
    // The same fingerprint as the read's: confirmed, not reset.
    assert_eq!(row.confirmations, 2);
    assert_eq!(break_count(&pool, journey_id).await, 1);
    exec(&pool, "DELETE FROM station_samples WHERE crs = 'ZRC'").await;
}

/// A two-leg journey RP0001 then RP0004 that is no longer being planned
/// (a dismissed break on it); returns its id and leg ids.
async fn create_started_two_leg_journey(
    pool: &PgPool,
    router: &axum::Router,
    token: &str,
) -> (i64, Vec<i64>) {
    let (status, body) = call(
        router,
        "POST",
        "/Journeys/itinerary",
        Some(token),
        Some(serde_json::json!({
            "legs": [
                { "trainUid": "RP0001", "serviceDate": DATE, "originCrs": "ZRC", "destinationCrs": "ZRD" },
                { "trainUid": "RP0004", "serviceDate": DATE, "originCrs": "ZRD", "destinationCrs": "ZRE" }
            ]
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let journey_id = body["journeyId"].as_i64().expect("journeyId");
    let legs: Vec<i64> = body["legs"]
        .as_array()
        .expect("legs")
        .iter()
        .map(|leg| leg["legId"].as_i64().expect("legId"))
        .collect();
    sqlx::query(
        "INSERT INTO journey_breaks (journey_id, user_id, cause, fault, severity, \
             trigger_leg_id, break_position, fingerprint, status, resolved_at) \
         VALUES ($1, $2, 'connection_missed', 'operator', 1, $3, 2, '', 'dismissed', now())",
    )
    .bind(journey_id)
    .bind(USER)
    .bind(legs[1])
    .execute(pool)
    .await
    .expect("seed a dismissed break");
    (journey_id, legs)
}

/// Going back to a plan that had more legs after the current ones (review
/// J4): the missing legs come back, where this was a 409.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn reverting_to_a_longer_plan_brings_its_later_legs_back() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_onward(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, legs) = create_started_two_leg_journey(&pool, &router, &token).await;

    // Deleting the second leg, once started, is a plan of its own.
    let (status, body) = call(
        &router,
        "DELETE",
        &format!("/Journeys/{journey_id}/legs/{}", legs[1]),
        Some(&token),
        None,
        None,
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    let (_, revisions) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/revisions"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(revisions["revisions"][0]["revisionNo"], 2, "{revisions}");
    assert_eq!(revisions["revisions"][1]["canRevert"], true, "{revisions}");

    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/revisions/1/revert"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    let uids: Vec<&str> = detail["legs"]
        .as_array()
        .expect("legs")
        .iter()
        .filter_map(|leg| leg["trackedTrainState"]["trainUid"].as_str())
        .collect();
    assert_eq!(uids, ["RP0001", "RP0004"], "{detail}");
}

/// An `Idempotency-Key` names one request to one journey and leg (review
/// J5): the same key on another journey's revert is a conflict, never the
/// first journey's answer replayed with nothing done.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn an_idempotency_key_is_scoped_to_its_journey() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_onward(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let mut journeys = Vec::new();
    for _ in 0..2 {
        let (journey_id, legs) = create_started_two_leg_journey(&pool, &router, &token).await;
        let (status, body) = call(
            &router,
            "DELETE",
            &format!("/Journeys/{journey_id}/legs/{}", legs[1]),
            Some(&token),
            None,
            None,
        )
        .await;
        assert!(status.is_success(), "{status} {body}");
        journeys.push(journey_id);
    }
    let revert = |journey_id: i64| format!("/Journeys/{journey_id}/revisions/1/revert");
    let (status, first) = call(
        &router,
        "POST",
        &revert(journeys[0]),
        Some(&token),
        None,
        Some("revert-key"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    // A retry of the same request replays it.
    let (status, again) = call(
        &router,
        "POST",
        &revert(journeys[0]),
        Some(&token),
        None,
        Some("revert-key"),
    )
    .await;
    assert_eq!((status, &again), (StatusCode::OK, &first));
    let (status, body) = call(
        &router,
        "POST",
        &revert(journeys[1]),
        Some(&token),
        None,
        Some("revert-key"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_ne!(body, first);
}

/// Accepting from stale suggestions re-plans them, and the replacement
/// starts from the leg the fresh set starts from (review J6), not the one
/// the stored set named.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn an_accept_after_a_re_plan_starts_from_the_fresh_sets_leg() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_onward(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, legs) = create_started_two_leg_journey(&pool, &router, &token).await;
    seed_live(&pool, "RP0004", "cancelled", 0, "2099-05-05T07:00:00Z").await;
    let journey = format!("/Journeys/{journey_id}");

    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    assert_eq!(detail["openBreak"]["cause"], "cancelled", "{detail}");
    assert_eq!(detail["openBreak"]["triggerLegId"], legs[1], "{detail}");
    let break_id = detail["openBreak"]["id"].as_i64().expect("break id");
    let (status, preview) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/replan/preview"),
        Some(&token),
        Some(serde_json::json!({ "breakId": break_id })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    let pick = preview["suggestions"]["suggestions"]
        .as_array()
        .expect("suggestions")
        .iter()
        .find(|s| s["legs"][0]["trainUid"] == "RP0005")
        .expect("RP0005 offered")
        .clone();
    // The stored set goes stale and names the first leg, as a set planned
    // before the break's trigger moved would.
    sqlx::query(
        "UPDATE journey_breaks SET suggestions = jsonb_set(jsonb_set(suggestions, \
             '{fromLegId}', to_jsonb($2::bigint)), '{fingerprint}', '\"moved\"') \
         WHERE id = $1",
    )
    .bind(break_id)
    .bind(legs[0])
    .execute(&pool)
    .await
    .expect("age the stored set");

    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/replan/accept"),
        Some(&token),
        Some(serde_json::json!({ "breakId": break_id, "suggestionId": pick["id"] })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, detail) = call(&router, "GET", &journey, Some(&token), None, None).await;
    let uids: Vec<&str> = detail["legs"]
        .as_array()
        .expect("legs")
        .iter()
        .filter_map(|leg| leg["trackedTrainState"]["trainUid"].as_str())
        .collect();
    assert_eq!(uids, ["RP0001", "RP0005"], "{detail}");
    assert_eq!(detail["legs"][0]["id"], legs[0], "{detail}");
}

/// Saving an itinerary never holds its transaction's connection while it
/// waits for another (review J8): with a pool of one connection the save,
/// and its replay, still go through.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn an_itinerary_saves_on_a_pool_of_one_connection() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let one = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&database_url)
        .await
        .expect("connect");
    let router = router(one);
    let body = serde_json::json!({
        "legs": [{
            "trainUid": "RP0001", "serviceDate": DATE,
            "originCrs": "ZRC", "destinationCrs": "ZRD"
        }]
    });
    let (status, first) = call(
        &router,
        "POST",
        "/Journeys/itinerary",
        Some(&token),
        Some(body.clone()),
        Some("one-connection"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, again) = call(
        &router,
        "POST",
        "/Journeys/itinerary",
        Some(&token),
        Some(body),
        Some("one-connection"),
    )
    .await;
    assert_eq!((status, &again), (StatusCode::OK, &first));
    let (status, other) = call(
        &router,
        "POST",
        "/Journeys/itinerary",
        Some(&token),
        Some(serde_json::json!({
            "legs": [{ "trainUid": "RP0002", "serviceDate": DATE }]
        })),
        Some("one-connection"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{other}");
}

/// A browser session's `Idempotency-Key` on `POST /Journeys/itinerary` is
/// honoured: concurrent duplicates make one journey and answer alike, a
/// retry replays the first answer, another body is a 409 (the key is bound to the route name too:
/// `an_itinerary_key_hash_is_bound_to_the_route`),
/// and no key keeps saving a journey each time.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_session_itinerary_key_makes_one_journey_per_key() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let body = serde_json::json!({
        "legs": [{
            "trainUid": "RP0001", "serviceDate": DATE,
            "originCrs": "ZRC", "destinationCrs": "ZRD"
        }]
    });
    let send = |key: Option<&'static str>, body: Value| {
        let (router, token) = (router.clone(), token.clone());
        async move {
            call(
                &router,
                "POST",
                "/Journeys/itinerary",
                Some(&token),
                Some(body),
                key,
            )
            .await
        }
    };
    let ((status_a, first), (status_b, second)) = tokio::join!(
        send(Some("session-key"), body.clone()),
        send(Some("session-key"), body.clone())
    );
    assert_eq!((status_a, status_b), (StatusCode::OK, StatusCode::OK));
    assert_eq!(first, second, "concurrent duplicates answer alike");
    let (status, again) = send(Some("session-key"), body.clone()).await;
    assert_eq!((status, &again), (StatusCode::OK, &first));
    let count = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM journeys WHERE user_id = $1")
            .bind(USER)
            .fetch_one(&pool)
            .await
            .expect("count")
    };
    assert_eq!(count().await, 1, "one journey for one key");

    let other_body = serde_json::json!({ "legs": [{ "trainUid": "RP0002", "serviceDate": DATE }] });
    let (status, conflict) = send(Some("session-key"), other_body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(count().await, 1);

    // No key: each request saves a journey, as before.
    for _ in 0..2 {
        let (status, saved) = send(None, body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
    }
    assert_eq!(count().await, 3);
}

/// Someone else's journey and no journey at all answer an operator filter
/// alike (review J9): 404, whatever the filter, never the 400 that told
/// them the journey exists.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn an_operator_filter_on_someone_elses_journey_is_a_404_whatever_its_codes() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let other = seed_session(&pool, OTHER).await;
    let router = router(pool.clone());
    let (journey_id, _) = create_journey(&router, &token).await;
    let filter = serde_json::json!({
        "operatorFilter": { "mode": "only", "entries": ["Q9"] }
    });
    let (status, body) = call(
        &router,
        "PUT",
        &format!("/Journeys/{journey_id}/operator-filter"),
        Some(&token),
        Some(filter.clone()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "the owner's: {body}");
    for id in [journey_id, i64::MAX] {
        let (status, body) = call(
            &router,
            "PUT",
            &format!("/Journeys/{id}/operator-filter"),
            Some(&other),
            Some(filter.clone()),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}: {body}");
    }
}

/// "I missed this train" after the train has run on to its destination
/// (DS-534): the miss window, not the train's arrival, decides, so the
/// alternatives come and taking one is recorded as a miss. "Choose another
/// train" on the same leg is still refused.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_train_missed_after_it_ran_on_to_its_destination_is_still_replaced() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_late_train(&pool, "RP0006", "12:45", "13:45").await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;

    // 12:30 London: RP0001 reached ZRD at 11:00 (and its 30 minutes of
    // grace are over), so it has arrived.
    let _later = crate::routes::pin_london_now_for_tests(london_at(0, 12, 30));
    let alternatives = format!("/Journeys/{journey_id}/legs/{leg_id}/alternatives");
    let (status, body) = call(
        &router,
        "GET",
        &format!("{alternatives}?cause=choice"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, "This leg is already travelled.");
    let (status, found) = call(
        &router,
        "GET",
        &format!("{alternatives}?cause=missed"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    let offered: Vec<&str> = found["suggestions"]
        .as_array()
        .expect("suggestions")
        .iter()
        .filter_map(|s| s["legs"][0]["trainUid"].as_str())
        .collect();
    assert!(offered.contains(&"RP0006"), "{found}");

    // A free choice of another train is still refused on replace; the
    // miss is taken.
    let replace = format!("/Journeys/{journey_id}/legs/{leg_id}/replace");
    let (status, body) = call(
        &router,
        "POST",
        &replace,
        Some(&token),
        Some(replace_body("choice", "RP0006")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, "This leg is already travelled.");
    let (status, replaced) = call(
        &router,
        "POST",
        &replace,
        Some(&token),
        Some(replace_body("missed", "RP0006")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["revisionNo"], 2, "{replaced}");
    let (_, history) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/revisions"),
        Some(&token),
        None,
        None,
    )
    .await;
    let revisions = history["revisions"].as_array().expect("revisions");
    assert_eq!(revisions[0]["cause"], "user_missed", "{history}");
    assert_eq!(revisions[0]["fault"], "user", "{history}");
    assert_eq!(revisions[1]["legs"][0]["trainUid"], "RP0001", "{history}");
    assert_eq!(revisions[1]["legs"][0]["fate"], "missed", "{history}");
    assert_eq!(break_count(&pool, journey_id).await, 0);
}

/// The miss window ends at 04:00 London the morning after the leg's
/// service date, whatever the train did (DS-534).
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_miss_is_refused_once_the_window_has_closed() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;

    // 04:30 the morning after: closed, for every cause that uses the window.
    let _closed = crate::routes::pin_london_now_for_tests(london_at(1, 4, 30));
    let alternatives = format!("/Journeys/{journey_id}/legs/{leg_id}/alternatives");
    let replace = format!("/Journeys/{journey_id}/legs/{leg_id}/replace");
    for cause in ["missed", "cancelled", "connection"] {
        let (status, body) = call(
            &router,
            "GET",
            &format!("{alternatives}?cause={cause}"),
            Some(&token),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{cause}: {body}");
        assert_eq!(body, "It's too late to change this journey.", "{cause}");
        let (status, body) = call(
            &router,
            "POST",
            &replace,
            Some(&token),
            Some(replace_body(cause, "RP0002")),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{cause}: {body}");
        assert_eq!(body, "It's too late to change this journey.", "{cause}");
    }
    let revisions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM journey_revisions WHERE journey_id = $1")
            .bind(journey_id)
            .fetch_one(&pool)
            .await
            .expect("count revisions");
    assert_eq!(revisions, 1, "nothing was replaced");

    // 03:30 the same morning: still open.
    let _open = crate::routes::pin_london_now_for_tests(london_at(1, 3, 30));
    let (status, replaced) = call(
        &router,
        "POST",
        &replace,
        Some(&token),
        Some(replace_body("missed", "RP0002")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
}

/// A cancelled train never counts as arrived for the window: its
/// timetabled arrival plus 30 minutes passing neither refuses "Find another
/// way" nor stops the page opening the break (DS-534).
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_cancelled_train_past_its_arrival_still_gets_a_break_and_alternatives() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_late_train(&pool, "RP0006", "12:45", "13:45").await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO train_current_state (trains_id, status) \
             SELECT id, 'cancelled' FROM trains WHERE service_date = '{DATE}' AND train_uid = 'RP0001'"
        ),
    )
    .await;

    let _later = crate::routes::pin_london_now_for_tests(london_at(0, 12, 30));
    let (status, found) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/alternatives?cause=cancelled"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(detail["openBreak"]["cause"], "cancelled", "{detail}");
    assert_eq!(detail["openBreak"]["triggerLegId"], leg_id, "{detail}");
}

/// `?from=` moves where the search starts (DS-534): from the leg's own
/// boarding station by default, from the station the traveller names
/// otherwise; a code that is not a station is a 400.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn alternatives_can_be_searched_from_another_station() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_onward(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, legs) = create_started_two_leg_journey(&pool, &router, &token).await;
    let alternatives = format!("/Journeys/{journey_id}/legs/{}/alternatives", legs[0]);

    // By default: from the leg's boarding station, ZRC.
    let (status, found) = call(
        &router,
        "GET",
        &format!("{alternatives}?cause=missed"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found["originCrs"], "ZRC", "{found}");

    // From ZRD, where the traveller is: only trains leaving there.
    let (status, found) = call(
        &router,
        "GET",
        &format!("{alternatives}?cause=missed&from=zrd"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(found["originCrs"], "ZRD", "{found}");
    let suggestions = found["suggestions"].as_array().expect("suggestions");
    assert!(!suggestions.is_empty(), "{found}");
    for suggestion in suggestions {
        assert_eq!(suggestion["legs"][0]["originCrs"], "ZRD", "{found}");
    }
    let offered: Vec<&str> = suggestions
        .iter()
        .filter_map(|s| s["legs"][0]["trainUid"].as_str())
        .collect();
    assert!(offered.contains(&"RP0004"), "{found}");

    // Not three letters, and three letters that are no station.
    for from in ["ZR", "ZRDD", "Z1D", "QQQ"] {
        let (status, body) = call(
            &router,
            "GET",
            &format!("{alternatives}?cause=missed&from={from}"),
            Some(&token),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{from}: {body}");
    }
}

/// The set says what the leg's train has done (DS-534): not left, left but
/// not arrived, arrived. Facts only: all three still answer 200 for a miss.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn the_alternatives_say_whether_the_train_has_left_and_arrived() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_late_train(&pool, "RP0006", "12:45", "13:45").await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;
    let uri = format!("/Journeys/{journey_id}/legs/{leg_id}/alternatives?cause=missed");

    // 08:00: RP0001 leaves at 10:00.
    let (status, found) = call(&router, "GET", &uri, Some(&token), None, None).await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(
        found["leg"],
        serde_json::json!({ "departed": false, "arrived": false }),
        "{found}"
    );

    // 10:15: it left on time and is on its way.
    let _under_way = crate::routes::pin_london_now_for_tests(london_at(0, 10, 15));
    seed_live(&pool, "RP0001", "en_route", 0, "2099-05-05T09:14:00Z").await;
    let (status, found) = call(&router, "GET", &uri, Some(&token), None, None).await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(
        found["leg"],
        serde_json::json!({ "departed": true, "arrived": false }),
        "{found}"
    );

    // 12:30: it has reached ZRD.
    let _later = crate::routes::pin_london_now_for_tests(london_at(0, 12, 30));
    let (status, found) = call(&router, "GET", &uri, Some(&token), None, None).await;
    assert_eq!(status, StatusCode::OK, "{found}");
    assert_eq!(
        found["leg"],
        serde_json::json!({ "departed": true, "arrived": true }),
        "{found}"
    );
}

/// Taking another train for a missed one replaces the legs after it too,
/// and a later leg that has already run no longer refuses the whole
/// replace (DS-534): it is `not_used` in the plan it leaves. A free choice
/// over the same range is still refused.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_later_leg_that_already_ran_is_not_used_instead_of_refusing_the_replace() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_onward(&pool).await;
    seed_late_train(&pool, "RP0006", "12:45", "13:45").await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, legs) = create_started_two_leg_journey(&pool, &router, &token).await;

    // 12:30: RP0001 reached ZRD at 11:00 and RP0004 reached ZRE at 12:00.
    let _later = crate::routes::pin_london_now_for_tests(london_at(0, 12, 30));
    let replace = format!("/Journeys/{journey_id}/legs/{}/replace", legs[0]);
    let mut body = replace_body("choice", "RP0006");
    body["throughLegId"] = serde_json::json!(legs[1]);
    let (status, refused) = call(
        &router,
        "POST",
        &replace,
        Some(&token),
        Some(body.clone()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused, "This leg is already travelled.");

    body["cause"] = serde_json::json!("missed");
    let (status, replaced) = call(&router, "POST", &replace, Some(&token), Some(body), None).await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    let (_, history) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/revisions"),
        Some(&token),
        None,
        None,
    )
    .await;
    let earlier = &history["revisions"][1]["legs"];
    assert_eq!(earlier[0]["trainUid"], "RP0001", "{history}");
    assert_eq!(earlier[0]["fate"], "missed", "{history}");
    assert_eq!(earlier[1]["trainUid"], "RP0004", "{history}");
    assert_eq!(earlier[1]["fate"], "not_used", "{history}");
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    let uids: Vec<&str> = detail["legs"]
        .as_array()
        .expect("legs")
        .iter()
        .filter_map(|leg| leg["trackedTrainState"]["trainUid"].as_str())
        .collect();
    assert_eq!(uids, ["RP0006"], "{detail}");
}

/// Undoing "I missed this train" stays possible until the replacement's
/// first new train leaves, at most an hour after the change (DS-534); a
/// replacement that has been under way too long, and one made for another
/// cause, keep the 2 minutes.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_missed_train_can_be_undone_until_the_replacement_leaves() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_late_train(&pool, "RP0006", "12:45", "13:45").await;
    seed_late_train(&pool, "RP0007", "15:00", "16:00").await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (soon, soon_leg) = create_journey(&router, &token).await;
    // Another journey, on RP0002 (a train can sit in one journey of yours
    // only once, and a replacement releases it).
    let (status, saved) = call(
        &router,
        "POST",
        "/Journeys/itinerary",
        Some(&token),
        Some(serde_json::json!({
            "legs": [{
                "trainUid": "RP0002", "serviceDate": DATE,
                "originCrs": "ZRC", "destinationCrs": "ZRD"
            }]
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let late = saved["journeyId"].as_i64().expect("journeyId");
    let late_leg = saved["legId"].as_i64().expect("legId");

    // 12:30: the traveller missed RP0001 (the first journey takes the 12:45,
    // the second the 15:00). The change is dated to the pinned clock.
    let _changed = crate::routes::pin_london_now_for_tests(london_at(0, 12, 30));
    for (journey_id, leg_id, uid) in [(soon, soon_leg, "RP0006"), (late, late_leg, "RP0007")] {
        let (status, replaced) = call(
            &router,
            "POST",
            &format!("/Journeys/{journey_id}/legs/{leg_id}/replace"),
            Some(&token),
            Some(replace_body("missed", uid)),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{replaced}");
        sqlx::query(
            "UPDATE journey_revisions SET created_at = $2 WHERE journey_id = $1 AND revision_no = 2",
        )
        .bind(journey_id)
        .bind(london_at(0, 12, 30))
        .execute(&pool)
        .await
        .expect("date the change");
    }
    let can_revert = |journey_id: i64| {
        let router = router.clone();
        let token = token.clone();
        async move {
            let (_, history) = call(
                &router,
                "GET",
                &format!("/Journeys/{journey_id}/revisions"),
                Some(&token),
                None,
                None,
            )
            .await;
            history["revisions"][1]["canRevert"]
                .as_bool()
                .expect("canRevert")
        }
    };

    // 12:40: well past the 2 minutes, before either replacement leaves.
    let _ten_minutes = crate::routes::pin_london_now_for_tests(london_at(0, 12, 40));
    assert!(can_revert(soon).await);
    assert!(can_revert(late).await);

    // 12:50: the 12:45 has left; the 15:00 has not.
    let _later = crate::routes::pin_london_now_for_tests(london_at(0, 12, 50));
    assert!(!can_revert(soon).await);
    assert!(can_revert(late).await);
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{soon}/revisions/1/revert"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, "That plan no longer works.");

    // 13:35: the 15:00 is still to leave, but an hour has passed.
    let _capped = crate::routes::pin_london_now_for_tests(london_at(0, 13, 35));
    assert!(!can_revert(late).await);

    // 13:25: inside the hour: Undo goes back to the booked train.
    let _inside = crate::routes::pin_london_now_for_tests(london_at(0, 13, 25));
    let (status, undone) = call(
        &router,
        "POST",
        &format!("/Journeys/{late}/revisions/1/revert"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{undone}");
    assert_eq!(undone["revisionNo"], 3, "{undone}");
}

async fn leg_public_id_of(pool: &PgPool, leg_id: i64) -> String {
    sqlx::query_scalar("SELECT public_id FROM journey_legs WHERE id = $1")
        .bind(leg_id)
        .fetch_one(pool)
        .await
        .expect("leg public id")
}

async fn leg_ids_of(pool: &PgPool, journey_id: i64) -> Vec<i64> {
    sqlx::query_scalar("SELECT id FROM journey_legs WHERE journey_id = $1 ORDER BY leg_order")
        .bind(journey_id)
        .fetch_all(pool)
        .await
        .expect("leg ids")
}

async fn revision_count_of(pool: &PgPool, journey_id: i64) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM journey_revisions WHERE journey_id = $1")
        .bind(journey_id)
        .fetch_one(pool)
        .await
        .expect("count revisions")
}

/// A replace over both legs of a two-leg journey under one
/// `Idempotency-Key`, named by `first`'s ids (the path leg and
/// `throughLegId`) and then retried by the other form's. The replace
/// deletes both legs, so a retry by opaque ids has nothing to look up:
/// it must still be the same request (DS-534).
async fn replace_retried_in_the_other_id_form(public_first: bool) {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_onward(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, legs) = create_started_two_leg_journey(&pool, &router, &token).await;
    let numeric = (legs[0].to_string(), serde_json::json!(legs[1]));
    let opaque = (
        leg_public_id_of(&pool, legs[0]).await,
        serde_json::json!(leg_public_id_of(&pool, legs[1]).await),
    );
    let (first, other) = if public_first {
        (opaque, numeric)
    } else {
        (numeric, opaque)
    };
    let send = |form: &(String, Value), uid: &str| {
        let router = router.clone();
        let token = token.clone();
        let uri = format!("/Journeys/{journey_id}/legs/{}/replace", form.0);
        let mut body = replace_body("choice", uid);
        body["throughLegId"] = form.1.clone();
        async move {
            call(
                &router,
                "POST",
                &uri,
                Some(&token),
                Some(body),
                Some("replace-either-form"),
            )
            .await
        }
    };

    let (status, answer) = send(&first, "RP0002").await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert!(
        answer.get("replacedLegs").is_none(),
        "the stored legs are never served: {answer}"
    );
    let legs_after = leg_ids_of(&pool, journey_id).await;
    assert_eq!(legs_after.len(), 1, "both legs were replaced by one");
    let revisions_after = revision_count_of(&pool, journey_id).await;

    // The retry in the other form, and in the first form again, replay.
    for (label, form) in [("other form", &other), ("same form", &first)] {
        let (status, again) = send(form, "RP0002").await;
        assert_eq!(
            (status, &again),
            (StatusCode::OK, &answer),
            "{label}: the first response, replayed"
        );
        assert_eq!(leg_ids_of(&pool, journey_id).await, legs_after, "{label}");
        assert_eq!(
            revision_count_of(&pool, journey_id).await,
            revisions_after,
            "{label}"
        );
    }

    // A different request under the key is still a conflict, in either
    // form, and so is a leg id the key never replaced.
    for (label, form) in [("other form", &other), ("first form", &first)] {
        let (status, body) = send(form, "RP0003").await;
        assert_eq!(status, StatusCode::CONFLICT, "{label}: {body}");
        assert_eq!(
            body, "This Idempotency-Key was already used for a different request.",
            "{label}"
        );
    }
    let stranger = (
        "l_00000000000000000000000000000000".to_string(),
        other.1.clone(),
    );
    let (status, body) = send(&stranger, "RP0002").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(leg_ids_of(&pool, journey_id).await, legs_after);
    assert_eq!(revision_count_of(&pool, journey_id).await, revisions_after);
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn an_idempotent_replace_by_number_is_replayed_for_the_opaque_ids() {
    replace_retried_in_the_other_id_form(false).await;
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn an_idempotent_replace_by_opaque_ids_is_replayed_for_the_numbers() {
    replace_retried_in_the_other_id_form(true).await;
}

/// Whether `GET /Journeys/mine` lists journey `journey_id` as over.
async fn listed_as_travelled(router: &axum::Router, token: &str, journey_id: i64) -> bool {
    let (_, mine) = call(router, "GET", "/Journeys/mine", Some(token), None, None).await;
    mine.as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == journey_id))
        .and_then(|row| row["travelled"].as_bool())
        .unwrap_or_else(|| panic!("the journey is listed with `travelled`: {mine}"))
}

/// Nothing is stored about a miss, so a journey is over or still to come
/// from its current legs (DS-534): one whose only train has run is over,
/// and goes back to still-to-come once that leg is replaced by a train
/// that has not left, and over again when the replacement has run.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_past_journey_is_upcoming_again_after_a_replacement_for_a_train_still_to_come() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_late_train(&pool, "RP0006", "12:45", "13:45").await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;
    assert!(!listed_as_travelled(&router, &token, journey_id).await);

    // The list judges "over" in SQL against the database's own clock, not
    // the pinned one, so the trains' finish is stated by their state: the
    // booked train has completed.
    let _later = crate::routes::pin_london_now_for_tests(london_at(0, 12, 30));
    seed_live(&pool, "RP0001", "completed", 0, "2099-05-05T10:00:00Z").await;
    assert!(listed_as_travelled(&router, &token, journey_id).await);

    let (status, replaced) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/replace"),
        Some(&token),
        Some(replace_body("missed", "RP0006")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert!(
        !listed_as_travelled(&router, &token, journey_id).await,
        "the 12:45 is still to come"
    );

    // The replacement has completed too.
    seed_live(&pool, "RP0006", "completed", 0, "2099-05-05T13:00:00Z").await;
    assert!(listed_as_travelled(&router, &token, journey_id).await);
}

/// What the miss window refuses beyond the plain cases (DS-534): a free
/// choice on a leg with a break open is judged by the window, not the
/// train; `from` does not get past a closed window (the window is judged
/// before the station is); and the window closes at 04:00 exactly.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_closed_window_refuses_a_break_leg_and_a_from_station_and_closes_at_four() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO train_current_state (trains_id, status) \
             SELECT id, 'cancelled' FROM trains WHERE service_date = '{DATE}' AND train_uid = 'RP0001'"
        ),
    )
    .await;
    // A page read opens the break; it stays open in the table until
    // another read (this test makes none) ends it.
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(detail["openBreak"]["cause"], "cancelled", "{detail}");

    let alternatives = format!("/Journeys/{journey_id}/legs/{leg_id}/alternatives");
    let replace = format!("/Journeys/{journey_id}/legs/{leg_id}/replace");
    let too_late = "It's too late to change this journey.";

    // 04:30 the morning after.
    let _closed = crate::routes::pin_london_now_for_tests(london_at(1, 4, 30));
    // No cause is a free choice, which a break open on the leg turns into
    // a window judgement: miss_window_closed, not leg_travelled.
    for uri in [alternatives.clone(), format!("{alternatives}?cause=choice")] {
        let (status, body) = call(&router, "GET", &uri, Some(&token), None, None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{uri}: {body}");
        assert_eq!(body, too_late, "{uri}");
    }
    let (status, body) = call(
        &router,
        "POST",
        &replace,
        Some(&token),
        Some(replace_body("choice", "RP0002")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, too_late);
    // A station that exists, one that does not and one that is not a code:
    // all the window's answer.
    for from in ["ZRC", "QQQ", "ZR"] {
        let (status, body) = call(
            &router,
            "GET",
            &format!("{alternatives}?cause=missed&from={from}"),
            Some(&token),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "from {from}: {body}");
        assert_eq!(body, too_late, "from {from}");
    }

    // 04:00 sharp is closed, a minute before it is open.
    let _four = crate::routes::pin_london_now_for_tests(london_at(1, 4, 0));
    let (status, body) = call(
        &router,
        "POST",
        &replace,
        Some(&token),
        Some(replace_body("missed", "RP0002")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(revision_count_of(&pool, journey_id).await, 1);
    let _before_four = crate::routes::pin_london_now_for_tests(london_at(1, 3, 59));
    let (status, body) = call(
        &router,
        "POST",
        &replace,
        Some(&token),
        Some(replace_body("missed", "RP0002")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A break's stored suggestions carry no opaque leg ids (older sets never
/// did), so they are filled when the break is served, in one lookup, and a
/// leg that no longer exists leaves its id empty without failing the read.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_stored_set_is_served_with_the_legs_opaque_ids_and_empty_ones_for_gone_legs() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;
    let leg_public_id = leg_public_id_of(&pool, leg_id).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO train_current_state (trains_id, status) \
             SELECT id, 'cancelled' FROM trains WHERE service_date = '{DATE}' AND train_uid = 'RP0001'"
        ),
    )
    .await;
    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
        None,
    )
    .await;
    let break_id = detail["openBreak"]["id"].as_i64().expect("break id");
    let (status, preview) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/replan/preview"),
        Some(&token),
        Some(serde_json::json!({ "breakId": break_id })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");

    // As stored an old set was: no opaque ids, and a leg that has since
    // gone (999999999) as one suggestion's through leg.
    let stored: Value = sqlx::query_scalar("SELECT suggestions FROM journey_breaks WHERE id = $1")
        .bind(break_id)
        .fetch_one(&pool)
        .await
        .expect("the stored set");
    let mut old = stored.clone();
    old["fromLegPublicId"] = serde_json::json!("");
    if old["trigger"].is_object() {
        old["trigger"]["legPublicId"] = serde_json::json!("");
    }
    let suggestions = old["suggestions"].as_array_mut().expect("suggestions");
    assert!(!suggestions.is_empty(), "{stored}");
    suggestions[0]["throughLegId"] = serde_json::json!(999_999_999);
    suggestions[0]["throughLegPublicId"] = serde_json::json!("");
    suggestions[0]["rejoinsLegId"] = serde_json::json!(leg_id);
    suggestions[0]["rejoinsLegPublicId"] = Value::Null;
    for later in suggestions.iter_mut().skip(1) {
        let object = later.as_object_mut().expect("a suggestion");
        object.remove("throughLegPublicId");
        object.remove("rejoinsLegPublicId");
    }
    sqlx::query("UPDATE journey_breaks SET suggestions = $2 WHERE id = $1")
        .bind(break_id)
        .bind(&old)
        .execute(&pool)
        .await
        .expect("store the old set");

    let (status, current) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/breaks/current"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{current}");
    let set = &current["suggestions"];
    assert_eq!(set["fromLegPublicId"], leg_public_id.as_str(), "{current}");
    if set["trigger"].is_object() {
        assert_eq!(set["trigger"]["legPublicId"], leg_public_id.as_str());
    }
    assert_eq!(set["suggestions"][0]["throughLegPublicId"], "", "{current}");
    assert_eq!(
        set["suggestions"][0]["rejoinsLegPublicId"],
        leg_public_id.as_str(),
        "{current}"
    );
    for later in set["suggestions"]
        .as_array()
        .expect("suggestions")
        .iter()
        .skip(1)
    {
        assert_eq!(
            later["throughLegPublicId"],
            leg_public_id.as_str(),
            "{current}"
        );
        assert!(later["rejoinsLegPublicId"].is_null(), "{current}");
    }
}

fn next_date() -> NaiveDate {
    let date: NaiveDate = NEXT_DATE.parse().expect("date");
    assert_synthetic_date(date);
    date
}

/// One more ZRC to ZRD train, `uid`, on `service_date` (`DATE` or
/// `NEXT_DATE`), leaving at `dep` and arriving at `arr` (`HH:MM`).
/// `day_offset` is how many days past `service_date` both times fall: 1 for
/// a train that leaves after midnight but still belongs to that date.
async fn seed_train_on(
    pool: &PgPool,
    service_date: &str,
    uid: &str,
    dep: &str,
    arr: &str,
    day_offset: u8,
) {
    exec(
        pool,
        &format!(
            "INSERT INTO schedule_calling_points_full \
                 (service_date, uid, seq, tiploc, kind, booked_departure, public_departure, \
                  booked_arrival, public_arrival, day_offset, can_board, can_alight) VALUES \
             ('{service_date}', '{uid}', 1, 'RPTSTC', 'origin', '{dep}', '{dep}', NULL, NULL, {day_offset}, TRUE, TRUE), \
             ('{service_date}', '{uid}', 2, 'RPTSTD', 'terminate', NULL, NULL, '{arr}', '{arr}', {day_offset}, TRUE, TRUE)"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO schedule_destination_departures \
                 (service_date, destination_crs, scheduled, train_uid, origin_crs, \
                  destination_arrival, operator_atoc, public_departure, \
                  public_destination_arrival, day_offset, destination_arrival_day_offset) \
             VALUES ('{service_date}', 'ZRD', '{dep}', '{uid}', 'ZRC', '{arr}', 'GR', '{dep}', \
                     '{arr}', {day_offset}, {day_offset})"
        ),
    )
    .await;
    exec(
        pool,
        &format!(
            "INSERT INTO schedule_services (service_date, uid, mode, stp, operator_atoc) \
             VALUES ('{service_date}', '{uid}', 'train', 'P', 'GR')"
        ),
    )
    .await;
}

/// `GET .../alternatives?cause=missed` for the journey's leg: the set.
async fn missed_alternatives(
    router: &axum::Router,
    token: &str,
    journey_id: i64,
    leg_id: i64,
) -> Value {
    let (status, found) = call(
        router,
        "GET",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/alternatives?cause=missed"),
        Some(token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{found}");
    found
}

/// Each suggestion's first train: `(trainUid, serviceDate)`.
fn first_trains(found: &Value) -> Vec<(String, String)> {
    found["suggestions"]
        .as_array()
        .expect("suggestions")
        .iter()
        .map(|suggestion| {
            let leg = &suggestion["legs"][0];
            (
                leg["trainUid"].as_str().expect("trainUid").to_string(),
                leg["serviceDate"]
                    .as_str()
                    .expect("serviceDate")
                    .to_string(),
            )
        })
        .collect()
}

/// The suggestion whose first train is `uid`.
fn suggestion_on<'a>(found: &'a Value, uid: &str) -> &'a Value {
    found["suggestions"]
        .as_array()
        .expect("suggestions")
        .iter()
        .find(|suggestion| suggestion["legs"][0]["trainUid"] == uid)
        .unwrap_or_else(|| panic!("{uid} in {found}"))
}

fn instant_of(value: &Value) -> chrono::DateTime<chrono::Utc> {
    value
        .as_str()
        .and_then(|text| text.parse().ok())
        .unwrap_or_else(|| panic!("an instant, got {value}"))
}

/// A search after the fixture day's midnight is on that day's clock (minutes
/// past 1440) and also looks at the next day's trains: a traveller who
/// missed the last train of the night is offered the first of the morning,
/// with its own service date and times. Replacing with it, and undoing that,
/// work across the two dates.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
#[expect(
    clippy::too_many_lines,
    reason = "test code: one scenario, read top to bottom"
)]
async fn a_miss_after_midnight_is_offered_the_next_mornings_trains() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    // The last train of the evening, and a night train that leaves at 00:20,
    // both on the fixture day; the next day's first two trains.
    seed_late_train(&pool, "RP0006", "23:50", "23:59").await;
    seed_train_on(&pool, DATE, "RPO001", "00:20", "01:20", 1).await;
    seed_train_on(&pool, NEXT_DATE, "RPN001", "05:00", "06:00", 0).await;
    seed_train_on(&pool, NEXT_DATE, "RPN002", "05:30", "06:30", 0).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;

    // 00:10 the next morning: the night train still to come on the fixture
    // day, then the next day's trains, from one search over both.
    let _night = crate::routes::pin_london_now_for_tests(london_at(1, 0, 10));
    let found = missed_alternatives(&router, &token, journey_id, leg_id).await;
    let offered = first_trains(&found);
    for (uid, service_date) in [("RPO001", DATE), ("RPN001", NEXT_DATE)] {
        assert!(
            offered
                .iter()
                .any(|(u, d)| (u.as_str(), d.as_str()) == (uid, service_date)),
            "{uid} on {service_date} in {found}"
        );
    }
    assert_eq!(
        instant_of(&suggestion_on(&found, "RPO001")["departure"]),
        london_at(1, 0, 20),
        "{found}"
    );

    // 03:30: only the next day's trains are left, with their own service
    // date and instants (05:00 on the 6th, not the 5th).
    let _morning = crate::routes::pin_london_now_for_tests(london_at(1, 3, 30));
    let found = missed_alternatives(&router, &token, journey_id, leg_id).await;
    assert_eq!(found["originCrs"], "ZRC", "{found}");
    assert_eq!(
        found["leg"],
        serde_json::json!({ "departed": true, "arrived": true }),
        "{found}"
    );
    let offered = first_trains(&found);
    assert!(!offered.is_empty(), "{found}");
    for (uid, service_date) in &offered {
        assert_eq!(service_date, NEXT_DATE, "{uid} in {found}");
        assert!(uid.starts_with("RPN"), "{uid} in {found}");
    }
    let uids: Vec<&str> = offered.iter().map(|(uid, _)| uid.as_str()).collect();
    assert!(
        uids.contains(&"RPN001") && uids.contains(&"RPN002"),
        "{found}"
    );
    let first = suggestion_on(&found, "RPN001");
    assert_eq!(
        instant_of(&first["departure"]),
        london_at(1, 5, 0),
        "{found}"
    );
    assert_eq!(instant_of(&first["arrival"]), london_at(1, 6, 0), "{found}");
    let leg = &first["legs"][0];
    assert_eq!(instant_of(&leg["departure"]), london_at(1, 5, 0), "{found}");
    assert_eq!(instant_of(&leg["arrival"]), london_at(1, 6, 0), "{found}");
    assert_eq!(
        instant_of(&leg["scheduledDeparture"]),
        london_at(1, 5, 0),
        "{found}"
    );

    // Taking it replaces the leg with one dated the next day, as a missed
    // train: its own revision, the missed leg kept in the history.
    let (status, replaced) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/legs/{leg_id}/replace"),
        Some(&token),
        Some(serde_json::json!({
            "cause": "missed",
            "legs": [{
                "trainUid": "RPN001", "serviceDate": NEXT_DATE,
                "originCrs": "ZRC", "destinationCrs": "ZRD"
            }]
        })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{replaced}");
    assert_eq!(replaced["revisionNo"], 2, "{replaced}");
    let leg_dates = || async {
        sqlx::query_scalar::<_, NaiveDate>(
            "SELECT service_date FROM journey_legs WHERE journey_id = $1 ORDER BY leg_order",
        )
        .bind(journey_id)
        .fetch_all(&pool)
        .await
        .expect("leg dates")
    };
    assert_eq!(leg_dates().await, [next_date()]);
    let kept: Vec<(String, NaiveDate)> = sqlx::query_as(
        "SELECT l.train_uid, l.service_date FROM journey_revisions r \
         JOIN journey_revision_legs l ON l.revision_id = r.id \
         WHERE r.journey_id = $1 ORDER BY r.revision_no, l.position",
    )
    .bind(journey_id)
    .fetch_all(&pool)
    .await
    .expect("revision legs");
    assert_eq!(
        kept,
        [
            ("RP0001".to_string(), date()),
            ("RPN001".to_string(), next_date())
        ]
    );
    let (_, history) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}/revisions"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(history["revisions"][0]["cause"], "user_missed", "{history}");
    assert_eq!(
        history["revisions"][1]["legs"][0]["fate"], "missed",
        "{history}"
    );

    // The change is dated to the pinned clock (revisions take the database's
    // own time), then the Undo brings back the original leg on its own date.
    sqlx::query(
        "UPDATE journey_revisions SET created_at = $2 WHERE journey_id = $1 AND revision_no = 2",
    )
    .bind(journey_id)
    .bind(london_at(1, 3, 30))
    .execute(&pool)
    .await
    .expect("date the change");
    let (status, reverted) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey_id}/revisions/1/revert"),
        Some(&token),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reverted}");
    assert_eq!(leg_dates().await, [date()]);
}

/// Before midnight on the fixture day the search is as it was: its own
/// day's trains only, though the next day has a timetable.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_miss_before_midnight_is_not_offered_the_next_days_trains() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_late_train(&pool, "RP0006", "23:50", "23:59").await;
    seed_train_on(&pool, NEXT_DATE, "RPN001", "05:00", "06:00", 0).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;

    // 23:30: the 23:50 is the only train left; the 05:00 next morning is
    // left out.
    let _evening = crate::routes::pin_london_now_for_tests(london_at(0, 23, 30));
    let found = missed_alternatives(&router, &token, journey_id, leg_id).await;
    let offered = first_trains(&found);
    assert!(!offered.is_empty(), "{found}");
    for (uid, service_date) in &offered {
        assert_eq!(
            (uid.as_str(), service_date.as_str()),
            ("RP0006", DATE),
            "{found}"
        );
    }
}

/// Past midnight with nothing published for the next day: the fixture day's
/// own night train is offered and the search does not fail.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_miss_after_midnight_with_no_timetable_for_the_next_day_gets_the_days_own_trains() {
    let _now = pin_now();
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool).await;
    seed_train_on(&pool, DATE, "RPO001", "00:20", "01:20", 1).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let (journey_id, leg_id) = create_journey(&router, &token).await;

    let _night = crate::routes::pin_london_now_for_tests(london_at(1, 0, 10));
    let found = missed_alternatives(&router, &token, journey_id, leg_id).await;
    let offered = first_trains(&found);
    assert!(!offered.is_empty(), "{found}");
    for (uid, service_date) in &offered {
        assert_eq!(
            (uid.as_str(), service_date.as_str()),
            ("RPO001", DATE),
            "{found}"
        );
    }
}
