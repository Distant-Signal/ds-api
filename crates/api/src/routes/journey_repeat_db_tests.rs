//! "Track this journey again" and "Move this journey" against a live
//! database (`DATABASE_URL`), on synthetic days. The journey: a bus to ZTA,
//! TA0001 (SW) ZTA 07:50 to ZTB 08:20, TB0001 (SN) ZTB 08:30 to ZTC 08:50,
//! then a walk; planned on `DAY1`, at 06:00 London unless a test says
//! otherwise.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;
use crate::test_support::{FixtureCleanup, assert_synthetic_date};

const USER: &str = "TEST-JOURNEY-REPEAT-USER";
const VIEWER: &str = "TEST-JOURNEY-REPEAT-VIEWER";
const OTHER: &str = "TEST-JOURNEY-REPEAT-OTHER";
const GROUP: &str = "TEST-JOURNEY-REPEAT-GROUP";
/// The journey's own day, and the same timetable the next day.
const DAY1: &str = "2099-08-04";
const DAY2: &str = "2099-08-05";
/// Only a direct train, ZTA to ZTC: nothing via ZTB.
const DIRECT_ONLY: &str = "2099-08-06";
/// TA0001 renamed TA9001 and three minutes later.
const RENAMED: &str = "2099-08-07";
/// No timetable.
const UNPUBLISHED: &str = "2099-08-08";

type Train = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
);

/// `(uid, from tiploc, to tiploc, departs, arrives, operator)`.
const USUAL: [Train; 4] = [
    ("TA0001", "TRPTSA", "TRPTSB", "07:50", "08:20", "SW"),
    ("TA0002", "TRPTSA", "TRPTSB", "08:20", "08:50", "SW"),
    ("TB0001", "TRPTSB", "TRPTSC", "08:30", "08:50", "SN"),
    ("TB0002", "TRPTSB", "TRPTSC", "09:00", "09:20", "SN"),
];
const DIRECT: Train = ("TD0001", "TRPTSA", "TRPTSC", "07:52", "08:40", "GW");
const RENAMED_FIRST: Train = ("TA9001", "TRPTSA", "TRPTSB", "07:53", "08:22", "SW");

fn day(text: &str) -> NaiveDate {
    let date: NaiveDate = text.parse().expect("date");
    assert_synthetic_date(date);
    date
}

fn london(date: NaiveDate, h: u32, m: u32) -> DateTime<Utc> {
    chrono_tz::Europe::London
        .from_local_datetime(&date.and_hms_opt(h, m, 0).unwrap())
        .unwrap()
        .with_timezone(&Utc)
}

async fn exec(pool: &PgPool, sql: &str) {
    sqlx::query(sql).execute(pool).await.expect(sql);
}

async fn cleanup(pool: &PgPool) -> FixtureCleanup {
    let mut statements = vec![format!("DELETE FROM groups WHERE id = '{GROUP}'")];
    for user in [USER, VIEWER, OTHER] {
        statements.extend([
            format!("DELETE FROM journeys WHERE user_id = '{user}'"),
            format!("DELETE FROM journey_templates WHERE user_id = '{user}'"),
            format!("DELETE FROM train_subscriptions WHERE user_id = '{user}'"),
            format!("DELETE FROM sessions WHERE user_id = '{user}'"),
            format!("DELETE FROM users WHERE id = '{user}'"),
        ]);
    }
    statements
        .push("DELETE FROM tiploc_crs WHERE tiploc IN ('TRPTSA', 'TRPTSB', 'TRPTSC')".to_string());
    for date in [DAY1, DAY2, DIRECT_ONLY, RENAMED, UNPUBLISHED] {
        statements.extend([
            format!("DELETE FROM trains WHERE service_date = '{date}'"),
            format!("DELETE FROM schedule_calling_points_full WHERE service_date = '{date}'"),
            format!("DELETE FROM schedule_destination_departures WHERE service_date = '{date}'"),
            format!("DELETE FROM schedule_services WHERE service_date = '{date}'"),
        ]);
    }
    FixtureCleanup::new(pool, statements).await
}

fn crs_of(tiploc: &str) -> &'static str {
    match tiploc {
        "TRPTSA" => "ZTA",
        "TRPTSB" => "ZTB",
        _ => "ZTC",
    }
}

async fn seed_trains(pool: &PgPool, date: &str, trains: &[Train]) {
    exec(
        pool,
        "INSERT INTO tiploc_crs (tiploc, crs, station_name, stanox, source_sequence) VALUES \
         ('TRPTSA', 'ZTA', 'Repeat A', 'RPA', 1), ('TRPTSB', 'ZTB', 'Repeat B', 'RPB', 1), \
         ('TRPTSC', 'ZTC', 'Repeat C', 'RPC', 1) ON CONFLICT DO NOTHING",
    )
    .await;
    for (uid, from, to, dep, arr, operator) in trains {
        let (origin, destination) = (crs_of(from), crs_of(to));
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_calling_points_full \
                     (service_date, uid, seq, tiploc, kind, booked_departure, public_departure, \
                      booked_arrival, public_arrival, day_offset, can_board, can_alight) VALUES \
                 ('{date}', '{uid}', 1, '{from}', 'origin', '{dep}', '{dep}', NULL, NULL, 0, TRUE, TRUE), \
                 ('{date}', '{uid}', 2, '{to}', 'terminate', NULL, NULL, '{arr}', '{arr}', 0, TRUE, TRUE)"
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
                 VALUES ('{date}', '{destination}', '{dep}', '{uid}', '{origin}', '{arr}', \
                         '{operator}', '{dep}', '{arr}')"
            ),
        )
        .await;
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_services (service_date, uid, mode, stp, operator_atoc) \
                 VALUES ('{date}', '{uid}', 'train', 'P', '{operator}')"
            ),
        )
        .await;
    }
}

async fn seed_timetables(pool: &PgPool) {
    let mut both = USUAL.to_vec();
    both.push(DIRECT);
    seed_trains(pool, DAY1, &both).await;
    seed_trains(pool, DAY2, &both).await;
    seed_trains(pool, DIRECT_ONLY, &[DIRECT]).await;
    let renamed: Vec<Train> = std::iter::once(RENAMED_FIRST)
        .chain(USUAL[1..].iter().copied())
        .collect();
    seed_trains(pool, RENAMED, &renamed).await;
}

async fn seed_user(pool: &PgPool, user: &str) -> String {
    exec(
        pool,
        &format!("INSERT INTO users (id, name) VALUES ('{user}', '{user}') ON CONFLICT DO NOTHING"),
    )
    .await;
    let raw = format!("test-raw-session-token-for-{user}");
    sqlx::query(
        "INSERT INTO sessions (id, user_id, refresh_token, created_at, expires_at) \
         VALUES ($1, $2, NULL, NOW(), NOW() + make_interval(days => 14)) \
         ON CONFLICT (id) DO UPDATE SET user_id = EXCLUDED.user_id, \
             expires_at = EXCLUDED.expires_at",
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
        .merge(crate::routes::journeys::router())
        .with_state(crate::test_support::inert_app(pool))
}

async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, format!("distant_signal_session={token}"));
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

/// The journey (see the module doc), saved through `POST
/// /Journeys/itinerary` on `DAY1`.
async fn save_journey(router: &axum::Router, token: &str) -> i64 {
    let date = day(DAY1);
    let (status, body) = call(
        router,
        "POST",
        "/Journeys/itinerary",
        token,
        Some(json!({"legs": [
            {"kind": "untracked", "transitMode": "bus", "originName": "Home",
             "destinationName": "Repeat A", "plannedDeparture": london(date, 7, 30),
             "plannedArrival": london(date, 7, 45)},
            {"trainUid": "TA0001", "serviceDate": DAY1, "originCrs": "ZTA", "destinationCrs": "ZTB"},
            {"trainUid": "TB0001", "serviceDate": DAY1, "originCrs": "ZTB", "destinationCrs": "ZTC"},
            {"kind": "untracked", "transitMode": "walk", "originName": "Repeat C",
             "destinationName": "Office", "plannedDeparture": london(date, 8, 55),
             "plannedArrival": london(date, 9, 5)},
        ]})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    body["journeyId"].as_i64().expect("journey id")
}

/// An option's train UIDs.
fn uids(option: &Value) -> Vec<String> {
    option["legs"]
        .as_array()
        .expect("legs")
        .iter()
        .filter(|leg| leg["kind"] == "train")
        .map(|leg| leg["trainUid"].as_str().unwrap().to_string())
        .collect()
}

/// An option's legs as `POST /Journeys/itinerary` (and `/move`) take them.
fn request_legs(option: &Value) -> Value {
    option["legs"]
        .as_array()
        .expect("legs")
        .iter()
        .map(|leg| {
            if leg["kind"] == "train" {
                json!({"trainUid": leg["trainUid"], "serviceDate": leg["serviceDate"],
                       "originCrs": leg["originCrs"], "destinationCrs": leg["destinationCrs"]})
            } else {
                json!({"kind": "untracked", "transitMode": leg["transitMode"],
                       "originName": leg["originName"], "destinationName": leg["destinationName"],
                       "plannedDeparture": leg["plannedDeparture"],
                       "plannedArrival": leg["plannedArrival"]})
            }
        })
        .collect()
}

async fn setup(pool: &PgPool) -> (FixtureCleanup, String) {
    let cleanup = cleanup(pool).await;
    seed_timetables(pool).await;
    let token = seed_user(pool, USER).await;
    (cleanup, token)
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_two_leg_journey_repeats_with_the_same_trains_and_its_walks_moved() {
    let pool = connect().await;
    let (_cleanup, token) = setup(&pool).await;
    let _now = crate::routes::pin_london_now_for_tests(london(day(DAY1), 6, 0));
    let router = router(pool.clone());
    let journey = save_journey(&router, &token).await;

    // No date: today, since 07:50 is still ahead.
    let (status, body) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey}/repeat"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (body["date"].as_str(), body["time"].as_str()),
        (Some(DAY1), Some("07:50")),
        "{body}"
    );

    let (status, body) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey}/repeat?date={DAY2}"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "planned", "{body}");
    assert_eq!(body["reason"], "usual", "{body}");
    // Via the change, so not the faster direct train.
    assert_eq!(body["via"], json!(["ZTB"]), "{body}");
    let choice = &body["choice"];
    assert_eq!(uids(choice), ["TA0001", "TB0001"], "{body}");
    let legs = choice["legs"].as_array().unwrap();
    assert_eq!(legs.len(), 4, "{body}");
    let next = day(DAY2);
    assert_eq!(legs[0]["transitMode"], "bus");
    assert_eq!(
        legs[0]["plannedDeparture"],
        json!(london(next, 7, 30)),
        "{body}"
    );
    assert_eq!(
        legs[3]["plannedArrival"],
        json!(london(next, 9, 5)),
        "{body}"
    );
    assert_eq!(legs[1]["serviceDate"], DAY2);
    // Never an earlier train among the alternatives.
    for alternative in body["alternatives"].as_array().unwrap() {
        assert_eq!(uids(alternative), ["TA0002", "TB0002"], "{body}");
    }

    // Saved as a new journey through the itinerary route.
    let (status, saved) = call(
        &router,
        "POST",
        "/Journeys/itinerary",
        &token,
        Some(json!({"legs": request_legs(choice)})),
    )
    .await;
    assert!(status.is_success(), "{status} {saved}");
    assert_ne!(saved["journeyId"].as_i64(), Some(journey));

    // A later time: the same operator near it.
    let (_, body) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey}/repeat?date={DAY2}&time=08:20"),
        &token,
        None,
    )
    .await;
    assert_eq!(body["reason"], "same_operator", "{body}");
    assert_eq!(uids(&body["choice"]), ["TA0002", "TB0002"], "{body}");
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn the_booked_plan_is_repeated_not_the_current_legs() {
    let pool = connect().await;
    let (_cleanup, token) = setup(&pool).await;
    let _now = crate::routes::pin_london_now_for_tests(london(day(DAY1), 6, 0));
    let router = router(pool.clone());
    let journey = save_journey(&router, &token).await;
    // Revision 1 booked the later pair; the legs say the earlier one.
    let date = day(DAY1);
    for (position, uid, dep, arr) in [
        (2, "TA0002", london(date, 8, 20), london(date, 8, 50)),
        (3, "TB0002", london(date, 9, 0), london(date, 9, 20)),
    ] {
        sqlx::query(
            "UPDATE journey_revision_legs SET train_uid = $3, booked_departure = $4, \
                    booked_arrival = $5 \
             WHERE position = $2 AND revision_id = (SELECT id FROM journey_revisions \
                                                    WHERE journey_id = $1 AND revision_no = 1)",
        )
        .bind(journey)
        .bind(position)
        .bind(uid)
        .bind(dep)
        .bind(arr)
        .execute(&pool)
        .await
        .expect("edit revision 1");
    }
    let (_, body) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey}/repeat?date={DAY2}"),
        &token,
        None,
    )
    .await;
    assert_eq!(
        (body["reason"].as_str(), body["time"].as_str()),
        (Some("usual"), Some("08:20")),
        "{body}"
    );
    assert_eq!(uids(&body["choice"]), ["TA0002", "TB0002"], "{body}");

    // An Irish train in the booked plan: not planned again.
    sqlx::query(
        "UPDATE journey_revision_legs SET train_uid = 'ie:A120' \
         WHERE position = 2 AND revision_id = (SELECT id FROM journey_revisions \
                                               WHERE journey_id = $1 AND revision_no = 1)",
    )
    .bind(journey)
    .execute(&pool)
    .await
    .expect("edit revision 1");
    let (status, body) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey}/repeat?date={DAY2}"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (body["status"].as_str(), body["unsupported"].as_str()),
        (Some("unsupported"), Some("irish")),
        "{body}"
    );
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_train_that_no_longer_runs_falls_back_and_an_unpublished_day_is_pending() {
    let pool = connect().await;
    let (_cleanup, token) = setup(&pool).await;
    let _now = crate::routes::pin_london_now_for_tests(london(day(DAY1), 6, 0));
    let router = router(pool.clone());
    let journey = save_journey(&router, &token).await;
    let get = |date: &str| {
        let router = router.clone();
        let token = token.clone();
        let uri = format!("/Journeys/{journey}/repeat?date={date}");
        async move { call(&router, "GET", &uri, &token, None).await.1 }
    };

    // TA0001 has a new UID and leaves 3 minutes later: same operator.
    let body = get(RENAMED).await;
    assert_eq!(body["reason"], "same_operator", "{body}");
    assert_eq!(uids(&body["choice"]), ["TA9001", "TB0001"], "{body}");

    // Nothing runs via ZTB: planned without the via, the direct train.
    let body = get(DIRECT_ONLY).await;
    assert_eq!(
        (body["status"].as_str(), body["reason"].as_str()),
        (Some("planned"), Some("best")),
        "{body}"
    );
    assert_eq!(body["via"], json!([]), "{body}");
    assert_eq!(uids(&body["choice"]), ["TD0001"], "{body}");

    let body = get(UNPUBLISHED).await;
    assert_eq!(body["status"], "pending", "{body}");
    assert!(body["choice"].is_null(), "{body}");

    // A day gone by is refused.
    let (status, _) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey}/repeat?date=2099-08-03"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn group_viewers_may_repeat_but_others_and_movers_get_404() {
    let pool = connect().await;
    let (_cleanup, token) = setup(&pool).await;
    let viewer = seed_user(&pool, VIEWER).await;
    let other = seed_user(&pool, OTHER).await;
    let _now = crate::routes::pin_london_now_for_tests(london(day(DAY1), 6, 0));
    let router = router(pool.clone());
    let journey = save_journey(&router, &token).await;
    exec(
        &pool,
        &format!(
            "INSERT INTO groups (id, name, created_by) VALUES ('{GROUP}', 'Repeat', '{USER}')"
        ),
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO group_members (group_id, user_id, role) VALUES \
             ('{GROUP}', '{USER}', 'owner'), ('{GROUP}', '{VIEWER}', 'member')"
        ),
    )
    .await;
    exec(
        &pool,
        &format!(
            "INSERT INTO group_journeys (group_id, journey_id, added_by) \
             VALUES ('{GROUP}', {journey}, '{USER}')"
        ),
    )
    .await;
    let uri = format!("/Journeys/{journey}/repeat?date={DAY2}");
    let (status, body) = call(&router, "GET", &uri, &viewer, None).await;
    assert_eq!(
        (status, body["status"].as_str()),
        (StatusCode::OK, Some("planned")),
        "{body}"
    );
    let (status, _) = call(&router, "GET", &uri, &other, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &router,
        "GET",
        &format!("{uri}&basis=current"),
        &viewer,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey}/move"),
        &viewer,
        Some(json!({"legs": request_legs(&body["choice"])})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The repeat plans in a user's slot: the plan sweeps' slots held don't
/// stop it, every user's slot held does.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn the_repeat_uses_a_users_slot_not_the_sweeps() {
    let pool = connect().await;
    let (_cleanup, token) = setup(&pool).await;
    let _now = crate::routes::pin_london_now_for_tests(london(day(DAY1), 6, 0));
    let router = router(pool.clone());
    let journey = save_journey(&router, &token).await;
    let uri = format!("/Journeys/{journey}/repeat?date={DAY2}");
    {
        let _regular = crate::routes::trips::hold_regular_sweep_slot().await;
        let _break = crate::routes::trips::hold_break_sweep_slot().await;
        let (status, body) = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            call(&router, "GET", &uri, &token, None),
        )
        .await
        .expect("not waiting on a sweep's slot");
        assert_eq!(
            (status, body["status"].as_str()),
            (StatusCode::OK, Some("planned")),
            "{body}"
        );
    }
    let _users = crate::routes::trips::hold_request_slots().await;
    let (status, _) = call(&router, "GET", &uri, &token, None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// `(revision count, revision 1's train UIDs and dates)`.
async fn revisions_of(pool: &PgPool, journey: i64) -> (i64, Vec<(Option<String>, NaiveDate)>) {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM journey_revisions WHERE journey_id = $1")
            .bind(journey)
            .fetch_one(pool)
            .await
            .unwrap();
    let booked = sqlx::query_as(
        "SELECT l.train_uid, l.service_date FROM journey_revision_legs l \
         JOIN journey_revisions r ON r.id = l.revision_id \
         WHERE r.journey_id = $1 AND r.revision_no = 1 ORDER BY l.position",
    )
    .bind(journey)
    .fetch_all(pool)
    .await
    .unwrap();
    (count, booked)
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
#[expect(clippy::too_many_lines, reason = "one scenario: move, then move again")]
async fn a_move_replans_every_leg_and_keeps_the_journey() {
    let pool = connect().await;
    let (_cleanup, token) = setup(&pool).await;
    let _now = crate::routes::pin_london_now_for_tests(london(day(DAY1), 6, 0));
    let router = router(pool.clone());
    let journey = save_journey(&router, &token).await;
    // Made from a regular journey, whose sweep had suggested a train.
    let template: i64 = sqlx::query_scalar(
        "INSERT INTO journey_templates (user_id, custom_name) VALUES ($1, 'Commute') RETURNING id",
    )
    .bind(USER)
    .fetch_one(&pool)
    .await
    .expect("template");
    sqlx::query(
        "UPDATE journeys SET source_template_id = $2, regular_plan = '{}', \
                regular_plan_status = 'suggested', regular_planned_at = now() WHERE id = $1",
    )
    .bind(journey)
    .bind(template)
    .execute(&pool)
    .await
    .unwrap();

    let (status, preview) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey}/repeat?basis=current&date={DAY2}&time=08:20"),
        &token,
        None,
    )
    .await;
    assert_eq!(
        (status, preview["status"].as_str()),
        (StatusCode::OK, Some("planned")),
        "{preview}"
    );
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey}/move"),
        &token,
        Some(json!({"legs": request_legs(&preview["choice"])})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revisionNo"], 1, "{body}");
    // Still being planned: the booked plan is the moved one.
    let next = day(DAY2);
    assert_eq!(
        revisions_of(&pool, journey).await,
        (
            1,
            vec![
                (None, next),
                (Some("TA0002".to_string()), next),
                (Some("TB0002".to_string()), next),
                (None, next),
            ]
        )
    );
    let (kept, status): (Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT source_template_id, regular_plan_status FROM journeys WHERE id = $1",
    )
    .bind(journey)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((kept, status), (Some(template), None));

    // With a later revision, a move is the next one; the others stay.
    {
        let mut tx = pool.begin().await.unwrap();
        crate::data::journey_revisions::append_revision(
            &mut tx,
            journey,
            &crate::data::journey_revisions::RevisionMeta::user_choice(),
            None,
            Utc::now(),
        )
        .await
        .expect("revision 2");
        tx.commit().await.unwrap();
    }
    let (_, preview) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey}/repeat?basis=current&date={DAY2}&time=07:50"),
        &token,
        None,
    )
    .await;
    assert_eq!(uids(&preview["choice"]), ["TA0001", "TB0001"], "{preview}");
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey}/move"),
        &token,
        Some(json!({"legs": request_legs(&preview["choice"])})),
    )
    .await;
    assert_eq!(
        (status, body["revisionNo"].as_i64()),
        (StatusCode::OK, Some(3)),
        "{body}"
    );
    let (count, booked) = revisions_of(&pool, journey).await;
    assert_eq!(count, 3);
    assert_eq!(
        booked[1].0.as_deref(),
        Some("TA0002"),
        "revision 1 is untouched"
    );
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_journey_that_has_started_is_not_moved() {
    let pool = connect().await;
    let (_cleanup, token) = setup(&pool).await;
    let router = router(pool.clone());
    let journey = {
        let _now = crate::routes::pin_london_now_for_tests(london(day(DAY1), 6, 0));
        save_journey(&router, &token).await
    };
    let (_, preview) = {
        let _now = crate::routes::pin_london_now_for_tests(london(day(DAY1), 6, 0));
        call(
            &router,
            "GET",
            &format!("/Journeys/{journey}/repeat?basis=current&date={DAY2}"),
            &token,
            None,
        )
        .await
    };
    // The bus left at 07:30.
    let _now = crate::routes::pin_london_now_for_tests(london(day(DAY1), 7, 35));
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey}/move"),
        &token,
        Some(json!({"legs": request_legs(&preview["choice"])})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body, "This journey has started, so it can't be moved.");
    assert_eq!(
        revisions_of(&pool, journey).await.1[1].0.as_deref(),
        Some("TA0001")
    );
}

async fn connect() -> PgPool {
    ds_store::test_support::connect().await
}
