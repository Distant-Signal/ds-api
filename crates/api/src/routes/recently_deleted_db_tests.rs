//! Recently deleted journeys against a live database (`DATABASE_URL`):
//! recording on a web delete, the list, "Save again", "Delete
//! permanently", "Empty now", expiry, the account cascade, another user's
//! view, and the flag off. "Today" is pinned to [`DAY`] (synthetic); the
//! rows' `deleted_at` is the database's real `now()`.
//!
//! Each test sets the process-wide flag first (nextest runs each test in
//! its own process; run with `--test-threads=1` otherwise).

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{NaiveDate, TimeZone, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;
use crate::data::recently_deleted;
use crate::test_support::FixtureCleanup;

/// The pinned "today".
const DAY: &str = "2099-09-14";
/// Fixture UIDs, never real ones.
const UIDS: [&str; 3] = ["ZRD001", "ZRD002", "ZRD003"];

fn day() -> NaiveDate {
    let date: NaiveDate = DAY.parse().unwrap();
    crate::test_support::assert_synthetic_date(date);
    date
}

fn pin_today() -> crate::routes::PinnedLondonNow {
    crate::routes::pin_london_now_for_tests(Utc.with_ymd_and_hms(2099, 9, 14, 6, 0, 0).unwrap())
}

async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to postgres")
}

async fn cleanup(pool: &PgPool, users: &[&str]) -> FixtureCleanup {
    let mut statements = Vec::new();
    for user in users {
        statements.extend([
            format!(
                "DELETE FROM unlisted_links WHERE resource_type = 'journey' \
                 AND resource_id IN (SELECT id::text FROM journeys WHERE user_id = '{user}')"
            ),
            format!("DELETE FROM journeys WHERE user_id = '{user}'"),
            format!("DELETE FROM train_subscriptions WHERE user_id = '{user}'"),
            format!("DELETE FROM users WHERE id = '{user}'"),
        ]);
    }
    let uids = UIDS.map(|uid| format!("'{uid}'")).join(", ");
    statements.push(format!("DELETE FROM trains WHERE train_uid IN ({uids})"));
    FixtureCleanup::new(pool, statements).await
}

async fn seed_user(pool: &PgPool, user: &str) -> String {
    sqlx::query("INSERT INTO users (id, name) VALUES ($1, $1) ON CONFLICT DO NOTHING")
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
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
    .unwrap();
    raw
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
    .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()));
    (status, value)
}

/// A walk, then two trains, on [`DAY`], named "Work"; returns the
/// itinerary response.
async fn save_journey(router: &axum::Router, token: &str) -> Value {
    let at = |h, m| {
        chrono_tz::Europe::London
            .from_local_datetime(&day().and_hms_opt(h, m, 0).unwrap())
            .unwrap()
            .with_timezone(&Utc)
    };
    let (status, body) = call(
        router,
        "POST",
        "/Journeys/itinerary",
        token,
        Some(json!({"customName": "Work", "legs": [
            {"kind": "untracked", "transitMode": "walk", "originName": "Home",
             "destinationName": "Station", "plannedDeparture": at(7, 30),
             "plannedArrival": at(7, 45)},
            {"trainUid": UIDS[0], "serviceDate": DAY, "originCrs": "EUS", "destinationCrs": "MKC"},
            {"trainUid": UIDS[1], "serviceDate": DAY},
        ]})),
    )
    .await;
    assert!(status.is_success(), "{status} {body}");
    body
}

async fn rows(pool: &PgPool, user: &str) -> Vec<(String, Value)> {
    sqlx::query_as("SELECT client, summary FROM delegated_deletions WHERE user_id = $1 ORDER BY id")
        .bind(user)
        .fetch_all(pool)
        .await
        .unwrap()
}

/// Inserts a row as a web delete `days_ago` days ago; returns its id.
async fn insert_row(pool: &PgPool, user: &str, days_ago: i32, summary: Value) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO delegated_deletions (user_id, client, journey_id, deleted_at, summary) \
         VALUES ($1, 'web', 0, now() - make_interval(days => $2), $3) RETURNING id",
    )
    .bind(user)
    .bind(days_ago)
    .bind(summary)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// With the flag off (the default), a delete is exactly as before: nothing
/// is recorded and every route is a 404.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api recently_deleted_db_tests -- --ignored --test-threads=1`"]
async fn with_the_flag_off_nothing_is_recorded_and_the_routes_are_404() {
    const USER: &str = "TEST-RECENTLY-DELETED-OFF";
    recently_deleted::set_for_tests(false, 7);
    let _pin = pin_today();
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER]).await;
    let token = seed_user(&pool, USER).await;
    let router = router(pool.clone());

    let saved = save_journey(&router, &token).await;
    let journey = saved["journeyId"].as_i64().unwrap();
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/Journeys/{journey}"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(rows(&pool, USER).await, Vec::new());

    let id = insert_row(&pool, USER, 0, json!({"legs": []})).await;
    for (method, uri) in [
        ("GET", "/Journeys/recently-deleted".to_string()),
        ("DELETE", "/Journeys/recently-deleted".to_string()),
        ("DELETE", format!("/Journeys/recently-deleted/{id}")),
        ("POST", format!("/Journeys/recently-deleted/{id}/restore")),
    ] {
        let (status, body) = call(&router, method, &uri, &token, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}: {body}");
    }
    assert_eq!(rows(&pool, USER).await.len(), 1, "nothing was deleted");
}

/// The whole round trip: a web delete is recorded in its own transaction,
/// listed (not on the Connected apps page), saved again with its name and
/// legs (its trains tracked again, its share link not back), and leaves
/// the list.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api recently_deleted_db_tests -- --ignored --test-threads=1`"]
async fn a_deleted_journey_is_listed_and_saved_again() {
    const USER: &str = "TEST-RECENTLY-DELETED-TRIP";
    recently_deleted::set_for_tests(true, 7);
    let _pin = pin_today();
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER]).await;
    let token = seed_user(&pool, USER).await;
    let router = router(pool.clone());

    let saved = save_journey(&router, &token).await;
    let journey = saved["journeyId"].as_i64().unwrap();
    let (status, _) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey}/share-link"),
        &token,
        None,
    )
    .await;
    assert!(status.is_success(), "{status}");
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/Journeys/{journey}"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let recorded = rows(&pool, USER).await;
    assert_eq!(recorded.len(), 1);
    let (client, summary) = &recorded[0];
    assert_eq!(client, "web");
    assert_eq!(summary["customName"], "Work");
    let legs = summary["legs"].as_array().unwrap();
    assert_eq!(legs.len(), 3);
    assert_eq!(legs[0]["kind"], "untracked");
    assert!(legs[0]["plannedArrival"].is_string(), "{summary}");
    assert_eq!(legs[1]["trainUid"], UIDS[0]);
    assert_eq!(legs[1]["originCrs"], "EUS");
    let app_list = crate::data::delegated::recent_deletions(&pool, USER)
        .await
        .unwrap();
    assert_eq!(app_list.len(), 0, "not on the Connected apps page");

    let (status, list) = call(&router, "GET", "/Journeys/recently-deleted", &token, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["retentionDays"], 7);
    assert_eq!(list["connectedAppRetentionDays"], 30);
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["journeyId"], journey);
    assert_eq!(items[0]["client"], "web");
    let deleted_at: chrono::DateTime<Utc> =
        serde_json::from_value(items[0]["deletedAt"].clone()).unwrap();
    let expires_at: chrono::DateTime<Utc> =
        serde_json::from_value(items[0]["expiresAt"].clone()).unwrap();
    assert_eq!(expires_at - deleted_at, chrono::Duration::days(7));
    let id = items[0]["id"].as_i64().unwrap();

    let (status, restored) = call(
        &router,
        "POST",
        &format!("/Journeys/recently-deleted/{id}/restore"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{restored}");
    assert_eq!(
        restored["legs"],
        json!([
            {"leg": 1, "outcome": "untracked"},
            {"leg": 2, "outcome": "tracked"},
            {"leg": 3, "outcome": "tracked"},
        ])
    );
    let new_journey = restored["journeyId"].as_i64().unwrap();
    assert_ne!(new_journey, journey);
    let (status, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{new_journey}"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["customName"], "Work");
    let detail_legs = detail["legs"].as_array().unwrap();
    assert_eq!(detail_legs.len(), 3);
    let tracked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM journey_legs \
         WHERE journey_id = $1 AND train_subscription_id IS NOT NULL",
    )
    .bind(new_journey)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tracked, 2, "both trains tracked again");
    let links: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM unlisted_links WHERE resource_type = 'journey' AND resource_id = $1",
    )
    .bind(new_journey.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(links, 0, "the share link does not come back");

    assert_eq!(rows(&pool, USER).await, Vec::new(), "off the list");
    let (status, _) = call(
        &router,
        "POST",
        &format!("/Journeys/recently-deleted/{id}/restore"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "only once");
}

/// Stopping tracking a train deletes its one-leg journey: recorded too.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api recently_deleted_db_tests -- --ignored --test-threads=1`"]
async fn untracking_a_train_records_its_journey() {
    const USER: &str = "TEST-RECENTLY-DELETED-TRAIN";
    recently_deleted::set_for_tests(true, 7);
    let _pin = pin_today();
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER]).await;
    let token = seed_user(&pool, USER).await;
    let router = router(pool.clone());

    let (status, saved) = call(
        &router,
        "POST",
        "/Journeys/itinerary",
        &token,
        Some(json!({"legs": [{"trainUid": UIDS[2], "serviceDate": DAY}]})),
    )
    .await;
    assert!(status.is_success(), "{saved}");
    let tracking = saved["trackingId"].as_i64().unwrap();
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/Train/{tracking}"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let recorded = rows(&pool, USER).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].1["legs"][0]["trainUid"], UIDS[2]);
}

/// Another user can neither see, save again nor delete someone's row.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api recently_deleted_db_tests -- --ignored --test-threads=1`"]
async fn another_user_cannot_see_or_save_again_someone_s_journey() {
    const USER: &str = "TEST-RECENTLY-DELETED-OWNER";
    const OTHER: &str = "TEST-RECENTLY-DELETED-OTHER";
    recently_deleted::set_for_tests(true, 7);
    let _pin = pin_today();
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER, OTHER]).await;
    seed_user(&pool, USER).await;
    let other = seed_user(&pool, OTHER).await;
    let router = router(pool.clone());
    let id = insert_row(
        &pool,
        USER,
        0,
        json!({"legs": [{"kind": "train", "serviceDate": DAY, "trainUid": UIDS[0]}]}),
    )
    .await;

    let (status, list) = call(&router, "GET", "/Journeys/recently-deleted", &other, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["items"], json!([]));
    for (method, uri) in [
        ("POST", format!("/Journeys/recently-deleted/{id}/restore")),
        ("DELETE", format!("/Journeys/recently-deleted/{id}")),
    ] {
        let (status, _) = call(&router, method, &uri, &other, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
    let (status, _) = call(
        &router,
        "DELETE",
        "/Journeys/recently-deleted",
        &other,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(rows(&pool, USER).await.len(), 1, "the owner's row stays");
    let journeys: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM journeys WHERE user_id = $1")
        .bind(OTHER)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(journeys, 0);
}

/// "Delete permanently" takes one row, "Empty now" every row, the
/// connected apps' included.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api recently_deleted_db_tests -- --ignored --test-threads=1`"]
async fn delete_permanently_and_empty_now() {
    const USER: &str = "TEST-RECENTLY-DELETED-PURGE";
    recently_deleted::set_for_tests(true, 7);
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER]).await;
    let token = seed_user(&pool, USER).await;
    let router = router(pool.clone());
    let first = insert_row(&pool, USER, 0, json!({"legs": []})).await;
    insert_row(&pool, USER, 1, json!({"legs": []})).await;
    sqlx::query(
        "INSERT INTO delegated_deletions (user_id, client, journey_id, summary) \
         VALUES ($1, 'ds-mcp', 0, '{\"legs\": []}')",
    )
    .bind(USER)
    .execute(&pool)
    .await
    .unwrap();

    let (status, list) = call(&router, "GET", "/Journeys/recently-deleted", &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["items"].as_array().unwrap().len(), 3, "{list}");
    assert_eq!(list["items"][0]["client"], "ds-mcp", "newest first");

    let uri = format!("/Journeys/recently-deleted/{first}");
    assert_eq!(
        call(&router, "DELETE", &uri, &token, None).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        call(&router, "DELETE", &uri, &token, None).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(rows(&pool, USER).await.len(), 2);
    let (status, _) = call(
        &router,
        "DELETE",
        "/Journeys/recently-deleted",
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(rows(&pool, USER).await, Vec::new());
}

/// A row past its period is neither listed nor saved again (the sweep
/// deletes it: `delegated_db_tests`' sweep test); the period follows the
/// setting.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api recently_deleted_db_tests -- --ignored --test-threads=1`"]
async fn a_row_past_its_period_is_gone_from_the_list() {
    const USER: &str = "TEST-RECENTLY-DELETED-EXPIRY";
    let _pin = pin_today();
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER]).await;
    let token = seed_user(&pool, USER).await;
    let router = router(pool.clone());
    let summary = json!({"legs": [{"kind": "train", "serviceDate": DAY, "trainUid": UIDS[0]}]});
    let old = insert_row(&pool, USER, 8, summary.clone()).await;
    let fresh = insert_row(&pool, USER, 6, summary).await;

    recently_deleted::set_for_tests(true, 7);
    let (_, list) = call(&router, "GET", "/Journeys/recently-deleted", &token, None).await;
    let ids: Vec<i64> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![fresh]);
    let (status, _) = call(
        &router,
        "POST",
        &format!("/Journeys/recently-deleted/{old}/restore"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    recently_deleted::set_for_tests(true, 14);
    let (_, list) = call(&router, "GET", "/Journeys/recently-deleted", &token, None).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
    assert_eq!(list["retentionDays"], 14);

    let outcome = crate::data::delegated::sweep(&pool, Utc::now(), 90, 365, 7)
        .await
        .unwrap();
    assert!(outcome.deletions_purged >= 1, "{outcome:?}");
    let left: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM delegated_deletions WHERE user_id = $1")
            .bind(USER)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(left, vec![fresh]);
}

/// A past train whose record has gone comes back as a leg with its
/// stations and no train; a journey with no leg that can come back is a
/// 422 and stays on the list.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api recently_deleted_db_tests -- --ignored --test-threads=1`"]
async fn a_gone_train_comes_back_without_its_train() {
    const USER: &str = "TEST-RECENTLY-DELETED-GONE";
    recently_deleted::set_for_tests(true, 7);
    let _pin = pin_today();
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER]).await;
    let token = seed_user(&pool, USER).await;
    let router = router(pool.clone());
    let long_ago = (day() - chrono::Duration::days(20)).to_string();
    let gone = insert_row(
        &pool,
        USER,
        1,
        json!({"customName": null, "legs": [{
            "kind": "train", "serviceDate": long_ago, "trainUid": UIDS[0],
            "originCrs": "EUS", "destinationCrs": "MKC"}]}),
    )
    .await;
    let (status, restored) = call(
        &router,
        "POST",
        &format!("/Journeys/recently-deleted/{gone}/restore"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{restored}");
    assert_eq!(
        restored["legs"],
        json!([{"leg": 1, "outcome": "without_train", "reason": "train_gone"}])
    );
    let (subscription, mode, origin): (Option<i64>, String, Option<String>) = sqlx::query_as(
        "SELECT train_subscription_id, match_mode, origin_crs FROM journey_legs WHERE journey_id = $1",
    )
    .bind(restored["journeyId"].as_i64().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(subscription, None);
    assert_eq!(mode, "unmatched");
    assert_eq!(origin.as_deref(), Some("EUS"));

    // An untracked leg from before planned arrivals were kept.
    let incomplete = insert_row(
        &pool,
        USER,
        1,
        json!({"legs": [{"kind": "untracked", "transitMode": "bus",
                         "plannedDeparture": "2099-09-14T08:00:00+00:00"}]}),
    )
    .await;
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/recently-deleted/{incomplete}/restore"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(rows(&pool, USER).await.len(), 1, "it stays on the list");
}

/// Deleting the account takes the list with it, at once.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api recently_deleted_db_tests -- --ignored --test-threads=1`"]
async fn deleting_the_account_deletes_the_list() {
    const USER: &str = "TEST-RECENTLY-DELETED-ACCOUNT";
    recently_deleted::set_for_tests(true, 7);
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER]).await;
    seed_user(&pool, USER).await;
    insert_row(&pool, USER, 0, json!({"legs": []})).await;
    let export = crate::data::account::export_account(&pool, USER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        export.recently_deleted_journeys.as_array().unwrap().len(),
        1
    );
    assert_eq!(export.connected_app_deletions, json!([]));
    crate::data::account::delete_account(&pool, USER)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rows(&pool, USER).await, Vec::new());
}

/// The user's own deletes do not count towards a connected app's daily
/// delete cap.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api recently_deleted_db_tests -- --ignored --test-threads=1`"]
async fn web_deletes_do_not_count_towards_an_app_s_daily_cap() {
    const USER: &str = "TEST-RECENTLY-DELETED-CAP";
    let pool = connect().await;
    let _cleanup = cleanup(&pool, &[USER]).await;
    seed_user(&pool, USER).await;
    for _ in 0..3 {
        insert_row(&pool, USER, 0, json!({"legs": []})).await;
    }
    let grant: i64 = sqlx::query_scalar(
        "INSERT INTO delegated_grants (user_id, client, client_display_name, scopes) \
         VALUES ($1, 'ds-mcp', 'Assistant', ARRAY['journeys:delete']) RETURNING id",
    )
    .bind(USER)
    .fetch_one(&pool)
    .await
    .unwrap();
    let journey: i64 = sqlx::query_scalar(
        "INSERT INTO journeys (user_id, custom_name, created_via) VALUES ($1, 'x', 'ds-mcp') \
         RETURNING id",
    )
    .bind(USER)
    .fetch_one(&pool)
    .await
    .unwrap();
    let outcome =
        crate::data::delegated::delete_journey_delegated(&pool, journey, USER, "ds-mcp", grant, 2)
            .await
            .unwrap();
    assert_eq!(
        outcome,
        crate::data::delegated::DelegatedDeleteOutcome::Deleted
    );
}
