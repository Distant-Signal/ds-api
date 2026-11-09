//! The journey calendar routes against a live database
//! (docs/calendar-feed.md): the secret link's lifecycle, what the feed
//! holds, and the single-journey download. The flag is process-wide, so
//! run with `--test-threads=1`.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;
use crate::data::calendar_feed;
use crate::data::journeys::{UntrackedLegInput, create_journey_with_untracked_leg};

const ORIGIN: &str = "https://example.invalid";

async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to postgres")
}

fn router(pool: PgPool) -> axum::Router {
    let app: crate::app::App = crate::test_support::inert_app(pool);
    let _ = Arc::strong_count(&app);
    crate::app::Router::new()
        .merge(super::journeys::router())
        .nest("/public", super::public_router())
        .with_state(app)
}

async fn cleanup(pool: &PgPool, prefix: &str) {
    sqlx::query("DELETE FROM users WHERE id LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await
        .expect("delete fixture users");
}

async fn seed_user(pool: &PgPool, user: &str) -> String {
    sqlx::query(
        "INSERT INTO users (id, name, username) VALUES ($1, $1, $1) ON CONFLICT (id) DO NOTHING",
    )
    .bind(user)
    .execute(pool)
    .await
    .expect("seed user");
    let raw = format!("raw-session-{user}");
    sqlx::query(
        "INSERT INTO sessions (id, user_id, expires_at) VALUES ($1, $2, now() + INTERVAL '1 day') \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(hash_session_token(&raw))
    .bind(user)
    .execute(pool)
    .await
    .expect("seed session");
    raw
}

/// A tube journey tomorrow with names the feed must not carry.
async fn seed_journey(pool: &PgPool, user: &str) -> i64 {
    let departure = Utc::now() + Duration::days(1);
    let leg = crate::data::journeys::validate_untracked_leg(
        UntrackedLegInput {
            transit_mode: "tube",
            origin_name: Some("Brixton"),
            destination_name: Some("Oxford Circus"),
            line_name: Some("Victoria"),
            planned_departure: Some(departure),
            planned_arrival: Some(departure + Duration::minutes(20)),
        },
        crate::routes::london_today(),
    )
    .expect("a valid leg");
    let (journey_id, _) =
        create_journey_with_untracked_leg(pool, user, Some("Secret home to work"), &leg)
            .await
            .expect("seed journey");
    journey_id
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, String) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn cookie(raw: &str) -> String {
    format!("distant_signal_session={raw}")
}

const IGNORE: &str = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                      calendar_db_tests -- --ignored --test-threads=1`";

#[tokio::test]
#[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api calendar_db_tests -- --ignored --test-threads=1`"]
async fn the_feed_link_is_made_shown_once_used_replaced_and_turned_off() {
    let _ = IGNORE;
    calendar_feed::set_enabled_for_tests(true);
    let pool = connect().await;
    cleanup(&pool, "TEST-CAL-A").await;
    let app = router(pool.clone());
    let raw = seed_user(&pool, "TEST-CAL-A1").await;
    let journey_id = seed_journey(&pool, "TEST-CAL-A1").await;
    let c = cookie(&raw);

    // Off until the user turns it on.
    let (status, _, body) = send(
        &app,
        "GET",
        "/public/account/calendar-feed",
        &[("cookie", &c)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"active": false})
    );

    // A cross-site POST is refused; the same-origin one makes the link.
    let (status, _, _) = send(
        &app,
        "POST",
        "/public/account/calendar-feed",
        &[("cookie", &c), ("origin", "https://evil.invalid")],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, headers, body) = send(
        &app,
        "POST",
        "/public/account/calendar-feed",
        &[("cookie", &c), ("origin", ORIGIN)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    let token = serde_json::from_str::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();

    // Stored hashed, as a calendar_feed link with no expiry; never listed again.
    let (hash, expires): (String, Option<chrono::DateTime<Utc>>) = sqlx::query_as(
        "SELECT token_hash, expires_at FROM unlisted_links \
         WHERE resource_type = 'calendar_feed' AND resource_id = $1 AND revoked_at IS NULL",
    )
    .bind("TEST-CAL-A1")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(hash, crate::data::unlisted_links::hash_link_token(&token));
    assert!(expires.is_none());
    let (_, _, body) = send(
        &app,
        "GET",
        "/public/account/calendar-feed",
        &[("cookie", &c)],
    )
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"active": true})
    );
    assert!(!body.contains(&token));

    // The feed needs no cookie, is private, and holds the plan and no more.
    let uri = format!("/public/calendar/{token}.ics");
    let (status, headers, feed) = send(&app, "GET", &uri, &[]).await;
    assert_eq!(status, StatusCode::OK, "{feed}");
    assert!(
        headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/calendar")
    );
    assert!(
        headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("private")
    );
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    assert!(feed.starts_with("BEGIN:VCALENDAR\r\n"), "{feed}");
    assert!(feed.contains(&format!("UID:journey-{journey_id}@example.invalid")));
    assert!(feed.contains("SUMMARY:Journey: Brixton to Oxford Circus"));
    // Content lines are folded at 75 octets: unfold before reading text.
    let unfolded = feed.replace("\r\n ", "");
    assert!(unfolded.contains("powered by NationalRail"));
    for private in ["Secret home to work", "Victoria", "TEST-CAL-A1", &token] {
        assert!(!unfolded.contains(private), "feed leaks {private}");
    }
    // Without the suffix works too.
    let (status, _, _) = send(&app, "GET", &format!("/public/calendar/{token}"), &[]).await;
    assert_eq!(status, StatusCode::OK);

    // A new link replaces the old one, which stops working at once.
    let (_, _, body) = send(
        &app,
        "POST",
        "/public/account/calendar-feed",
        &[("cookie", &c), ("origin", ORIGIN)],
    )
    .await;
    let new_token = serde_json::from_str::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(new_token, token);
    assert_eq!(send(&app, "GET", &uri, &[]).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/public/calendar/{new_token}.ics"),
            &[]
        )
        .await
        .0,
        StatusCode::OK
    );

    // Turning it off revokes it; a second turn-off is still 204.
    for _ in 0..2 {
        let (status, _, _) = send(
            &app,
            "DELETE",
            "/public/account/calendar-feed",
            &[("cookie", &c), ("origin", ORIGIN)],
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("/public/calendar/{new_token}.ics"),
            &[]
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    cleanup(&pool, "TEST-CAL-A").await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api calendar_db_tests -- --ignored --test-threads=1`"]
async fn a_feed_holds_only_its_own_users_journeys_and_other_tokens_are_refused() {
    calendar_feed::set_enabled_for_tests(true);
    let pool = connect().await;
    cleanup(&pool, "TEST-CAL-B").await;
    let app = router(pool.clone());
    let raw_a = seed_user(&pool, "TEST-CAL-B1").await;
    let raw_b = seed_user(&pool, "TEST-CAL-B2").await;
    let journey_a = seed_journey(&pool, "TEST-CAL-B1").await;
    let journey_b = seed_journey(&pool, "TEST-CAL-B2").await;
    let make = |raw: String| {
        let app = app.clone();
        async move {
            let (_, _, body) = send(
                &app,
                "POST",
                "/public/account/calendar-feed",
                &[("cookie", &cookie(&raw)), ("origin", ORIGIN)],
            )
            .await;
            serde_json::from_str::<Value>(&body).unwrap()["token"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    let token_a = make(raw_a.clone()).await;
    let _token_b = make(raw_b.clone()).await;
    let (_, _, feed) = send(&app, "GET", &format!("/public/calendar/{token_a}.ics"), &[]).await;
    assert!(feed.contains(&format!("journey-{journey_a}@")));
    assert!(!feed.contains(&format!("journey-{journey_b}@")));

    // A journey share link's token is not a calendar token.
    let (_, _, body) = send(
        &app,
        "POST",
        &format!("/Journeys/{journey_a}/share-link"),
        &[("cookie", &cookie(&raw_a)), ("origin", ORIGIN)],
    )
    .await;
    let share = serde_json::from_str::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, _, _) = send(&app, "GET", &format!("/public/calendar/{share}.ics"), &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = send(&app, "GET", "/public/calendar/not-a-token.ics", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The account routes need a session.
    let (status, _, _) = send(&app, "GET", "/public/account/calendar-feed", &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    cleanup(&pool, "TEST-CAL-B").await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api calendar_db_tests -- --ignored --test-threads=1`"]
async fn a_journey_downloads_for_its_owner_only() {
    calendar_feed::set_enabled_for_tests(true);
    let pool = connect().await;
    cleanup(&pool, "TEST-CAL-C").await;
    let app = router(pool.clone());
    let raw_owner = seed_user(&pool, "TEST-CAL-C1").await;
    let raw_other = seed_user(&pool, "TEST-CAL-C2").await;
    let journey = seed_journey(&pool, "TEST-CAL-C1").await;
    let uri = format!("/Journeys/{journey}/calendar.ics");

    let (status, headers, body) = send(&app, "GET", &uri, &[("cookie", &cookie(&raw_owner))]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        headers[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("attachment")
    );
    assert!(body.contains("SUMMARY:Journey: Brixton to Oxford Circus"));
    assert!(!body.contains("Secret home to work"));
    // The tube leg's own planned end is kept.
    assert!(body.contains("DTEND:"));

    assert_eq!(
        send(&app, "GET", &uri, &[("cookie", &cookie(&raw_other))])
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(&app, "GET", &uri, &[]).await.0,
        StatusCode::UNAUTHORIZED
    );
    cleanup(&pool, "TEST-CAL-C").await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api calendar_db_tests -- --ignored --test-threads=1`"]
async fn every_calendar_route_is_404_while_the_feature_is_off() {
    calendar_feed::set_enabled_for_tests(false);
    let pool = connect().await;
    cleanup(&pool, "TEST-CAL-D").await;
    let app = router(pool.clone());
    let raw = seed_user(&pool, "TEST-CAL-D1").await;
    let journey = seed_journey(&pool, "TEST-CAL-D1").await;
    let c = cookie(&raw);
    for (method, uri) in [
        ("GET", "/public/account/calendar-feed".to_string()),
        ("POST", "/public/account/calendar-feed".to_string()),
        ("DELETE", "/public/account/calendar-feed".to_string()),
        ("GET", "/public/calendar/anything.ics".to_string()),
        ("GET", format!("/Journeys/{journey}/calendar.ics")),
    ] {
        let (status, _, _) = send(&app, method, &uri, &[("cookie", &c), ("origin", ORIGIN)]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
    calendar_feed::set_enabled_for_tests(true);
    cleanup(&pool, "TEST-CAL-D").await;
}
