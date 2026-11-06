//! Leave by (regular journeys design §5.2) against a live database
//! (`DATABASE_URL`): `leaveBy` and `stationBufferMinutes` on the journey
//! list and detail, `PUT /Journeys/{id}/station-buffer`, the template's
//! buffer through its PUT, the occurrence's copy at "Add a date", and the
//! `GET /JourneyTemplates/station-buffer` pre-fill. Each test owns its
//! `TEST-LEAVEBY-*` users and `Q`-prefixed station codes.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{DateTime, Duration, DurationRound, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;
use crate::test_support::FixtureCleanup;

async fn connect() -> PgPool {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to postgres")
}

async fn cleanup(pool: &PgPool, users: &[&str]) -> FixtureCleanup {
    let list = users
        .iter()
        .map(|user| format!("'{user}'"))
        .collect::<Vec<_>>()
        .join(", ");
    FixtureCleanup::new(
        pool,
        [
            format!("DELETE FROM journeys WHERE user_id IN ({list})"),
            format!("DELETE FROM journey_templates WHERE user_id IN ({list})"),
            format!("DELETE FROM sessions WHERE user_id IN ({list})"),
            format!("DELETE FROM users WHERE id IN ({list})"),
        ],
    )
    .await
}

async fn seed_session(pool: &PgPool, user_id: &str) -> String {
    sqlx::query("INSERT INTO users (id, name) VALUES ($1, $1) ON CONFLICT DO NOTHING")
        .bind(user_id)
        .execute(pool)
        .await
        .expect("seed user");
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

fn router(pool: PgPool) -> axum::Router {
    crate::app::Router::new()
        .merge(crate::routes::journeys::router())
        .merge(crate::routes::journey_templates::router())
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

/// A journey of one walk leg leaving at `departure` (a whole minute).
async fn walk_journey(pool: &PgPool, user_id: &str, departure: DateTime<Utc>) -> i64 {
    let journey_id: i64 =
        sqlx::query_scalar("INSERT INTO journeys (user_id) VALUES ($1) RETURNING id")
            .bind(user_id)
            .fetch_one(pool)
            .await
            .expect("seed journey");
    sqlx::query(
        "INSERT INTO journey_legs (journey_id, leg_order, service_date, kind, match_mode, \
                                   transit_mode, origin_place, destination_place, \
                                   planned_departure, planned_arrival) \
         VALUES ($1, 1, ($2 AT TIME ZONE 'Europe/London')::date, 'untracked', 'manual', 'walk', \
                 'Home', 'Station', $2, $2 + interval '10 minutes')",
    )
    .bind(journey_id)
    .bind(departure)
    .execute(pool)
    .await
    .expect("seed walk leg");
    journey_id
}

fn at(value: &Value) -> DateTime<Utc> {
    value.as_str().expect("a time").parse().expect("RFC 3339")
}

#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database)"]
async fn the_owner_sets_a_journeys_buffer_and_reads_its_leave_by() {
    const USER: &str = "TEST-LEAVEBY-OWNER";
    const OTHER: &str = "TEST-LEAVEBY-OTHER";
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER, OTHER]).await;
    let token = seed_session(&pool, USER).await;
    let other = seed_session(&pool, OTHER).await;
    let router = router(pool.clone());
    let departure = (Utc::now() + Duration::hours(2))
        .duration_trunc(Duration::minutes(1))
        .unwrap();
    let journey_id = walk_journey(&pool, USER, departure).await;
    let uri = format!("/Journeys/{journey_id}/station-buffer");

    // None yet.
    let (status, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["stationBufferMinutes"], Value::Null);
    assert_eq!(detail["leaveBy"], Value::Null);

    // Someone else's journey: 404, and nothing changes. Out of range: 400.
    let (status, _) = call(
        &router,
        "PUT",
        &uri,
        Some(&other),
        Some(json!({ "minutes": 10 })),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for bad in [121, -1] {
        let (status, body) = call(
            &router,
            "PUT",
            &uri,
            Some(&token),
            Some(json!({ "minutes": bad })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }

    let (status, body) = call(
        &router,
        "PUT",
        &uri,
        Some(&token),
        Some(json!({ "minutes": 22 })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stationBufferMinutes"], json!(22));
    assert_eq!(
        at(&body["leaveBy"]["at"]),
        departure - Duration::minutes(22)
    );
    assert_eq!(at(&body["leaveBy"]["departure"]), departure);
    assert_eq!(body["leaveBy"]["basis"], json!("booked"));

    let (_, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey_id}"),
        Some(&token),
        None,
    )
    .await;
    assert_eq!(detail["stationBufferMinutes"], json!(22));
    assert_eq!(detail["leaveBy"], body["leaveBy"]);

    let (status, list) = call(&router, "GET", "/Journeys/mine", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let row = list
        .as_array()
        .expect("a list")
        .iter()
        .find(|row| row["id"] == json!(journey_id))
        .expect("the journey is listed");
    assert_eq!(row["stationBufferMinutes"], json!(22));
    assert_eq!(row["leaveBy"], body["leaveBy"]);

    // null clears it.
    let (status, body) = call(
        &router,
        "PUT",
        &uri,
        Some(&token),
        Some(json!({ "minutes": null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "stationBufferMinutes": null, "leaveBy": null })
    );
}

#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database)"]
async fn a_templates_buffer_is_kept_by_a_put_without_it_and_copied_to_a_new_date() {
    const USER: &str = "TEST-LEAVEBY-TEMPLATE";
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER]).await;
    let token = seed_session(&pool, USER).await;
    let router = router(pool.clone());
    let template_id: i64 = sqlx::query_scalar(
        "INSERT INTO journey_templates (user_id, days_of_week, default_match_mode) \
         VALUES ($1, 31, 'manual') RETURNING id",
    )
    .bind(USER)
    .fetch_one(&pool)
    .await
    .expect("seed template");
    let uri = format!("/JourneyTemplates/{template_id}");
    let put = |extra: Value| {
        let mut body = json!({
            "legs": [{ "originCrs": "QLA", "destinationCrs": "QLB",
                       "departWindow": { "after": "08:00:00", "before": "08:30:00" } }],
            "daysOfWeek": 31,
            "active": true,
            "startsOn": null,
            "endsOn": null,
            "defaultMatchMode": "manual",
            "autoCommitRule": null,
        });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        body
    };

    let (status, body) = call(
        &router,
        "PUT",
        &uri,
        Some(&token),
        Some(put(json!({ "stationBufferMinutes": 15 }))),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, detail) = call(&router, "GET", &uri, Some(&token), None).await;
    assert_eq!(detail["stationBufferMinutes"], json!(15));

    // An older client's PUT, without the field, keeps it.
    let (status, _) = call(&router, "PUT", &uri, Some(&token), Some(put(json!({})))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, detail) = call(&router, "GET", &uri, Some(&token), None).await;
    assert_eq!(detail["stationBufferMinutes"], json!(15));

    let (status, _) = call(
        &router,
        "PUT",
        &uri,
        Some(&token),
        Some(put(json!({ "stationBufferMinutes": 200 }))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // "Add a date" copies it onto the journey.
    let date = (Utc::now() + Duration::days(3)).date_naive();
    let (status, made) = call(
        &router,
        "POST",
        &format!("{uri}/materialize"),
        Some(&token),
        Some(json!({ "serviceDate": date })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{made}");
    let copied: Option<i16> =
        sqlx::query_scalar("SELECT station_buffer_minutes FROM journeys WHERE id = $1")
            .bind(made["journeyId"].as_i64().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(copied, Some(15));

    // null clears it on the template; the journey keeps its copy.
    let (status, _) = call(
        &router,
        "PUT",
        &uri,
        Some(&token),
        Some(put(json!({ "stationBufferMinutes": null }))),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, detail) = call(&router, "GET", &uri, Some(&token), None).await;
    assert_eq!(detail["stationBufferMinutes"], Value::Null);
}

#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database)"]
async fn the_prefill_prefers_the_template_from_that_station() {
    const USER: &str = "TEST-LEAVEBY-PREFILL";
    const OTHER: &str = "TEST-LEAVEBY-PREFILL-OTHER";
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER, OTHER]).await;
    let token = seed_session(&pool, USER).await;
    seed_session(&pool, OTHER).await;
    let router = router(pool.clone());
    let prefill = |crs: &str| format!("/JourneyTemplates/station-buffer?crs={crs}");

    let (status, body) = call(&router, "GET", &prefill("QPA"), Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "minutes": null }));
    let (status, _) = call(&router, "GET", &prefill("Q1"), Some(&token), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&router, "GET", &prefill("QPA"), None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A journey from QPA (its first train leg), updated after the template.
    let template_id: i64 = sqlx::query_scalar(
        "INSERT INTO journey_templates (user_id, days_of_week, origin_crs, destination_crs, \
                                        target_kind, target_time, station_buffer_minutes, \
                                        updated_at) \
         VALUES ($1, 31, 'QPA', 'QPB', 'depart_after', '07:45', 12, now() - interval '1 day') \
         RETURNING id",
    )
    .bind(USER)
    .fetch_one(&pool)
    .await
    .expect("seed template");
    for (user, minutes) in [(USER, 25_i16), (OTHER, 40)] {
        let journey_id: i64 = sqlx::query_scalar(
            "INSERT INTO journeys (user_id, station_buffer_minutes) VALUES ($1, $2) RETURNING id",
        )
        .bind(user)
        .bind(minutes)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO journey_legs (journey_id, leg_order, origin_crs, destination_crs, \
                                       service_date) \
             VALUES ($1, 1, 'QPA', 'QPB', CURRENT_DATE)",
        )
        .bind(journey_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    let (_, body) = call(&router, "GET", &prefill("qpa"), Some(&token), None).await;
    assert_eq!(body, json!({ "minutes": 12 }), "the template wins");
    sqlx::query("UPDATE journey_templates SET station_buffer_minutes = NULL WHERE id = $1")
        .bind(template_id)
        .execute(&pool)
        .await
        .unwrap();
    let (_, body) = call(&router, "GET", &prefill("QPA"), Some(&token), None).await;
    assert_eq!(
        body,
        json!({ "minutes": 25 }),
        "then the caller's own journey"
    );
    let (_, body) = call(&router, "GET", &prefill("QPB"), Some(&token), None).await;
    assert_eq!(body, json!({ "minutes": null }), "another station");
}
