//! The traveller's extra time to change trains (DS-757) against a live
//! database: `PUT /preferences/change-time` and its validation, who reads
//! the setting back, and whose reads of a journey the buffer shapes.
//! `#[sqlx::test]` modules run in the superuser pass only
//! (`SQLX_TEST_SKIPS` in scripts/db-tests.py).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;
use crate::data::journey_feasibility::{ConnectionRules, pin_rules_for_tests};

const ORIGIN: &str = "https://example.invalid";
/// Winter, so London time is UTC; far enough ahead that no train is live.
const DATE: &str = "2099-01-14";

fn router(pool: PgPool) -> axum::Router {
    let expected: Option<Arc<str>> = Some(Arc::from(ORIGIN));
    crate::app::Router::new()
        .merge(super::preferences::router())
        .merge(super::journeys::router())
        .with_state(crate::test_support::inert_app(pool))
        .layer(axum::middleware::from_fn_with_state(
            expected,
            crate::auth::reject_cross_origin_cookie_mutation,
        ))
}

async fn user(pool: &PgPool, id: &str, extra: i16) {
    sqlx::query("INSERT INTO users (id, name, change_extra_minutes) VALUES ($1, $1, $2)")
        .bind(id)
        .bind(extra)
        .execute(pool)
        .await
        .expect("insert user");
}

/// A session for `user_id`; the raw cookie value.
async fn session(pool: &PgPool, user_id: &str) -> String {
    let raw = format!("raw-change-time-{user_id}");
    sqlx::query(
        "INSERT INTO sessions (id, user_id, expires_at) VALUES ($1, $2, now() + INTERVAL '1 day')",
    )
    .bind(hash_session_token(&raw))
    .bind(user_id)
    .execute(pool)
    .await
    .expect("insert session");
    raw
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
    body: Option<String>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, format!("distant_signal_session={cookie}"));
    }
    if let Some(origin) = origin {
        request = request.header(header::ORIGIN, origin);
    }
    let request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body)),
        None => request.body(Body::empty()),
    }
    .expect("request");
    let response = router.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}

async fn stored(pool: &PgPool, user_id: &str) -> i16 {
    sqlx::query_scalar("SELECT change_extra_minutes FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .expect("read the setting")
}

#[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
#[ignore = "needs DATABASE_URL (a role that can create databases)"]
async fn the_setting_is_validated_stored_and_session_only(pool: PgPool) {
    user(&pool, "CT-A", 0).await;
    let cookie = session(&pool, "CT-A").await;
    let router = router(pool.clone());
    let put = |body: Option<&str>, cookie: Option<&str>, origin: Option<&str>| {
        let router = router.clone();
        let body = body.map(str::to_string);
        let cookie = cookie.map(str::to_string);
        let origin = origin.map(str::to_string);
        async move {
            send(
                &router,
                "PUT",
                "/preferences/change-time",
                cookie.as_deref(),
                origin.as_deref(),
                body,
            )
            .await
        }
    };

    // The ends of the range, and a value between.
    for minutes in [0, 15, 30, 5] {
        let body = json!({ "extraMinutes": minutes }).to_string();
        let (status, _) = put(Some(&body), Some(&cookie), Some(ORIGIN)).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{minutes}");
        assert_eq!(i32::from(stored(&pool, "CT-A").await), minutes);
    }

    // Anything else is a 400 that changes nothing.
    for body in [
        r#"{"extraMinutes":-1}"#,
        r#"{"extraMinutes":31}"#,
        r#"{"extraMinutes":1.5}"#,
        r#"{"extraMinutes":"5"}"#,
        r#"{"extraMinutes":null}"#,
        "{}",
        r#"{"extraMinutes":5,"other":1}"#,
        "not json",
    ] {
        let (status, message) = put(Some(body), Some(&cookie), Some(ORIGIN)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {message}");
        assert!(
            message.as_str().is_some_and(|m| m.ends_with('.')),
            "plain copy for {body}: {message}"
        );
        assert_eq!(stored(&pool, "CT-A").await, 5, "{body}");
    }
    // The out of range number gets its own sentence.
    let (_, message) = put(Some(r#"{"extraMinutes":31}"#), Some(&cookie), Some(ORIGIN)).await;
    assert_eq!(message, json!("Choose between 0 and 30 extra minutes."));

    // No session: 401. Another site's page: 403, and nothing stored.
    let (status, _) = put(Some(r#"{"extraMinutes":9}"#), None, Some(ORIGIN)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = put(
        Some(r#"{"extraMinutes":9}"#),
        Some(&cookie),
        Some("https://evil.example"),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(stored(&pool, "CT-A").await, 5);
}

#[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
#[ignore = "needs DATABASE_URL (a role that can create databases)"]
async fn the_session_user_reads_their_own_setting_back(pool: PgPool) {
    user(&pool, "CT-A", 10).await;
    user(&pool, "CT-B", 0).await;
    let a = session(&pool, "CT-A").await;
    let b = session(&pool, "CT-B").await;
    let router = router(pool);
    let (status, body) = send(&router, "GET", "/preferences", Some(&a), None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changeExtraMinutes"], 10);
    let (_, body) = send(&router, "GET", "/preferences", Some(&b), None, None).await;
    assert_eq!(body["changeExtraMinutes"], 0, "0 is a value, not absent");
}

#[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
#[ignore = "needs DATABASE_URL (a role that can create databases)"]
async fn the_account_export_carries_the_setting(pool: PgPool) {
    user(&pool, "CT-A", 15).await;
    let account: Value = sqlx::query_scalar("SELECT to_jsonb(u) FROM users u WHERE id = $1")
        .bind("CT-A")
        .fetch_one(&pool)
        .await
        .expect("the export's own read of the row");
    assert_eq!(account["change_extra_minutes"], 15);
}

/// Train `uid` calling at `calls` (`tiploc`, kind, arrival, departure).
async fn seed_train(
    pool: &PgPool,
    uid: &str,
    calls: &[(&str, &str, Option<&str>, Option<&str>)],
) -> i64 {
    let date: chrono::NaiveDate = DATE.parse().expect("date");
    let points: Vec<Value> = calls
        .iter()
        .map(|(tiploc, kind, arrival, departure)| {
            let mut point = json!({ "tiploc": tiploc, "kind": kind });
            if let Some(arrival) = arrival {
                point["bookedArrival"] = json!(arrival);
                point["publicArrival"] = json!(arrival);
                point["workingArrival"] = json!(arrival);
            }
            if let Some(departure) = departure {
                point["bookedDeparture"] = json!(departure);
                point["publicDeparture"] = json!(departure);
                point["workingDeparture"] = json!(departure);
            }
            point
        })
        .collect();
    let trains_id: i64 = sqlx::query_scalar(
        "INSERT INTO trains (train_uid, service_date, train_id, origin_crs, destination_crs, \
                             calling_points) \
         VALUES ($1, $2, $1, 'ZCA', 'ZCC', $3) RETURNING id",
    )
    .bind(uid)
    .bind(date)
    .bind(Value::Array(points))
    .fetch_one(pool)
    .await
    .expect("seed train");
    trains_id
}

/// The owner's journey: `CHG0001` ZCA 09:00 to ZCB 09:40, then `CHG0002`
/// ZCB 09:50 to ZCC 10:20. Ten booked minutes at ZCB, whose minimum change
/// time is the 5 minute default. Returns the journey id.
async fn seed_journey(pool: &PgPool, owner: &str) -> i64 {
    let date: chrono::NaiveDate = DATE.parse().expect("date");
    for (tiploc, crs) in [("CHGTLA", "ZCA"), ("CHGTLB", "ZCB"), ("CHGTLC", "ZCC")] {
        sqlx::query(
            "INSERT INTO tiploc_crs (tiploc, crs, station_name, stanox, source_sequence) \
             VALUES ($1, $2, $2, $1, 1)",
        )
        .bind(tiploc)
        .bind(crs)
        .execute(pool)
        .await
        .expect("seed tiploc_crs");
    }
    let first = seed_train(
        pool,
        "CHG0001",
        &[
            ("CHGTLA", "Origin", None, Some("09:00:00")),
            ("CHGTLB", "Terminate", Some("09:40:00"), None),
        ],
    )
    .await;
    let second = seed_train(
        pool,
        "CHG0002",
        &[
            ("CHGTLB", "Origin", None, Some("09:50:00")),
            ("CHGTLC", "Terminate", Some("10:20:00"), None),
        ],
    )
    .await;
    let journey: i64 =
        sqlx::query_scalar("INSERT INTO journeys (user_id) VALUES ($1) RETURNING id")
            .bind(owner)
            .fetch_one(pool)
            .await
            .expect("seed journey");
    for (order, trains_id, origin, destination) in
        [(1, first, "ZCA", "ZCB"), (2, second, "ZCB", "ZCC")]
    {
        let subscription: i64 = sqlx::query_scalar(
            "INSERT INTO train_subscriptions (user_id, service_date, resolution_status, trains_id) \
             VALUES ($1, $2, 'resolved', $3) RETURNING id",
        )
        .bind(owner)
        .bind(date)
        .bind(trains_id)
        .fetch_one(pool)
        .await
        .expect("seed subscription");
        sqlx::query(
            "INSERT INTO journey_legs (journey_id, leg_order, service_date, origin_crs, \
                 destination_crs, match_mode, train_subscription_id) \
             VALUES ($1, $2, $3, $4, $5, 'auto', $6)",
        )
        .bind(journey)
        .bind(order)
        .bind(date)
        .bind(origin)
        .bind(destination)
        .bind(subscription)
        .execute(pool)
        .await
        .expect("seed leg");
    }
    journey
}

fn tight() -> ConnectionRules {
    ConnectionRules {
        tight: true,
        extra_change_minutes: 0,
    }
}

/// The change onto the second leg, as a read served it.
fn change(detail: &Value) -> &Value {
    &detail["legs"][1]["connection"]
}

#[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
#[ignore = "needs DATABASE_URL (a role that can create databases)"]
async fn only_the_owners_read_of_a_journey_carries_their_extra_minutes(pool: PgPool) {
    let tight_pin = pin_rules_for_tests(tight());
    user(&pool, "CT-OWNER", 10).await;
    user(&pool, "CT-MEMBER", 0).await;
    user(&pool, "CT-STRANGER", 0).await;
    let owner = session(&pool, "CT-OWNER").await;
    let member = session(&pool, "CT-MEMBER").await;
    let stranger = session(&pool, "CT-STRANGER").await;
    let journey = seed_journey(&pool, "CT-OWNER").await;
    let router = router(pool.clone());
    let uri = format!("/Journeys/{journey}");

    // The owner: the 5 minute minimum and their 10, against 10 booked. It
    // was booked below what they asked for, so it reads at risk from the
    // start, and not missed (the second train has not left).
    let (status, detail) = send(&router, "GET", &uri, Some(&owner), None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let connection = change(&detail);
    assert_eq!(connection["gapMinutes"], 10, "{detail}");
    assert_eq!(connection["minimumChangeMinutes"], 15, "{detail}");
    assert_eq!(connection["extraMinutes"], 10, "{detail}");
    assert_eq!(connection["state"], "atRisk", "{detail}");

    // A stranger cannot read it at all.
    let (status, _) = send(&router, "GET", &uri, Some(&stranger), None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A fellow group member: the minimum alone, as for anyone.
    let group = crate::data::groups::create_group(&pool, "Change time", "CT-OWNER")
        .await
        .expect("create group");
    sqlx::query(
        "INSERT INTO group_members (group_id, user_id, role) VALUES ($1, 'CT-MEMBER', 'member')",
    )
    .bind(&group)
    .execute(&pool)
    .await
    .expect("seed member");
    assert!(
        crate::data::groups::add_journey_to_group(&pool, &group, journey, "CT-OWNER")
            .await
            .expect("share journey")
    );
    let (status, detail) = send(&router, "GET", &uri, Some(&member), None, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let connection = change(&detail);
    assert_eq!(connection["minimumChangeMinutes"], 5, "{detail}");
    assert_eq!(connection["state"], "ok", "{detail}");
    assert!(connection.get("extraMinutes").is_none(), "{detail}");

    // A share link, with no account at all.
    let link = crate::data::unlisted_links::rotate_link(
        &pool,
        "journey",
        &journey.to_string(),
        "CT-OWNER",
        Some(chrono::Duration::days(1)),
    )
    .await
    .expect("share link");
    let (status, detail) = send(
        &router,
        "GET",
        &format!("/Journeys/shared/{}", link.token),
        None,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let connection = change(&detail);
    assert_eq!(connection["minimumChangeMinutes"], 5, "{detail}");
    assert_eq!(connection["state"], "ok", "{detail}");
    assert!(connection.get("extraMinutes").is_none(), "{detail}");

    // Without the tight rules the owner's own read is judged as it always
    // was: the setting is not applied, and not reported.
    drop(tight_pin);
    let _off = pin_rules_for_tests(ConnectionRules::default());
    let (_, detail) = send(&router, "GET", &uri, Some(&owner), None, None).await;
    let connection = change(&detail);
    assert_eq!(connection["minimumChangeMinutes"], 5, "{detail}");
    assert!(connection.get("extraMinutes").is_none(), "{detail}");
}

#[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
#[ignore = "needs DATABASE_URL (a role that can create databases)"]
async fn the_sweep_judges_a_journey_with_its_owners_setting(pool: PgPool) {
    let _pin = pin_rules_for_tests(tight());
    user(&pool, "CT-OWNER", 10).await;
    user(&pool, "CT-OTHER", 0).await;
    let journey = seed_journey(&pool, "CT-OWNER").await;
    let app = crate::test_support::inert_app(pool.clone());
    let now = chrono::Utc::now();
    let analysis = super::journey_replan::analyse(&app, journey, now)
        .await
        .expect("analyse");
    let timings = analysis.timings.expect("timings");
    assert_eq!(timings.rules.extra_change_minutes, 10);
    let connection = timings.connections[1].as_ref().expect("a connection");
    assert_eq!(connection.minimum_change_minutes, Some(15));

    // Someone else's setting changes nothing about this journey.
    sqlx::query("UPDATE users SET change_extra_minutes = 30 WHERE id = 'CT-OTHER'")
        .execute(&pool)
        .await
        .expect("set another user's setting");
    let again = super::journey_replan::analyse(&app, journey, now)
        .await
        .expect("analyse");
    assert_eq!(
        again.timings.expect("timings").rules.extra_change_minutes,
        10
    );
}
