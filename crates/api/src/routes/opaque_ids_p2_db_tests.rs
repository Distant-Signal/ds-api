//! Opaque ids of the second batch (DS-534) against a live database: deleted
//! journey records (`x_`, the Recently deleted and Connected apps lists),
//! personal API tokens (`k_`), webhook destinations (`w_`) and time-away
//! periods (`a_`).
//!
//! For each family: the list and the create or update response carry the
//! `publicId` beside the numeric `id`; every route that takes an id accepts
//! the number or the opaque id; an unknown opaque id answers what an unknown
//! number does; a malformed one is a 400 (except a token's, which names no
//! token and so is the usual 204); another user's id, in either form, is
//! not found. The legacy counter counts numeric use only, and with the
//! numeric ids withheld (`API_SERVE_NUMERIC_IDS=false`) the numeric `id` is
//! `0` while the `publicId` stays.
//!
//! Fixtures are under one `opx-*` user prefix per test.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::app::App;
use crate::auth::hash_session_token;
use crate::numeric_ids::{NumericIds, leaks, withhold_numeric_ids};
use crate::personal_tokens::{PersonalTokenSettings, PersonalTokens};

const ORIGIN: &str = "https://example.invalid";
/// A well-formed id of each kind that no row has.
const NO_DELETION: &str = "x_00000000000000000000000000000000";
const NO_TOKEN: &str = "k_00000000000000000000000000000000";
const NO_WEBHOOK: &str = "w_00000000000000000000000000000000";
const NO_AWAY: &str = "a_00000000000000000000000000000000";

async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to postgres")
}

/// The website's routes for the four families, with the numeric ids
/// withheld exactly when `policy` says so.
fn router(pool: PgPool, policy: NumericIds) -> axum::Router {
    let mut state = Arc::try_unwrap(crate::test_support::inert_app(pool)).expect("a fresh app");
    state.personal_tokens = Arc::new(PersonalTokens::new(PersonalTokenSettings {
        enabled: true,
        listen_addr: "127.0.0.1:0".to_string(),
        public_base_url: "https://ds-api.example.invalid".to_string(),
        max_active_per_user: 5,
        ..PersonalTokenSettings::default()
    }));
    let app: App = Arc::new(state);
    let router = crate::app::Router::new()
        .merge(super::journeys::router())
        .nest("/public", super::public_router())
        .with_state(app);
    if policy.serves() {
        router
    } else {
        router.layer(axum::middleware::from_fn_with_state(
            policy,
            withhold_numeric_ids,
        ))
    }
}

async fn cleanup(pool: &PgPool, prefix: &str) {
    sqlx::query("DELETE FROM users WHERE id LIKE $1")
        .bind(format!("{prefix}-%"))
        .execute(pool)
        .await
        .expect("delete fixture users");
}

/// Seeds a user and a session; returns the raw cookie value.
async fn seed_user(pool: &PgPool, user: &str) -> String {
    sqlx::query("INSERT INTO users (id, name) VALUES ($1, $1) ON CONFLICT (id) DO NOTHING")
        .bind(user)
        .execute(pool)
        .await
        .expect("seed user");
    let raw = format!("raw-opx-{user}");
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

async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    cookie: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, format!("distant_signal_session={cookie}"))
        .header(header::ORIGIN, ORIGIN);
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A row of `delegated_deletions` of `client`; returns (id, public id).
async fn seed_deletion(pool: &PgPool, user: &str, client: &str) -> (i64, String) {
    sqlx::query_as(
        "INSERT INTO delegated_deletions (user_id, client, journey_id, summary) \
         VALUES ($1, $2, 0, '{\"legs\": []}') RETURNING id, public_id",
    )
    .bind(user)
    .bind(client)
    .fetch_one(pool)
    .await
    .expect("seed a deletion")
}

/// A personal token of `user` (never usable: its hash is random); returns
/// (id, public id).
async fn seed_token(pool: &PgPool, user: &str) -> (i64, String) {
    sqlx::query_as(
        "INSERT INTO personal_api_tokens (user_id, name, token_hash, hint, scopes, expires_at) \
         VALUES ($1, 'Display', sha256(convert_to(gen_random_uuid()::text, 'UTF8')), 'AbC1', \
                 '{lines:read}', NOW() + INTERVAL '90 days') RETURNING id, public_id",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("seed a token")
}

/// A webhook destination of `user` with no secret; returns (id, public id).
async fn seed_webhook(pool: &PgPool, user: &str, path: &str) -> (i64, String) {
    sqlx::query_as(
        "INSERT INTO webhook_destinations (user_id, kind, name, url) \
         VALUES ($1, 'generic', 'Hook', 'https://hooks.example.invalid/' || $2) \
         RETURNING id, public_id",
    )
    .bind(user)
    .bind(path)
    .fetch_one(pool)
    .await
    .expect("seed a webhook")
}

/// A time-away period of `user` in 2099; returns (id, public id).
async fn seed_away(pool: &PgPool, user: &str) -> (i64, String) {
    sqlx::query_as(
        "INSERT INTO user_away_periods (user_id, starts_on, ends_on, label) \
         VALUES ($1, '2099-03-01', '2099-03-05', 'Trip') RETURNING id, public_id",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("seed a period")
}

async fn exists(pool: &PgPool, table: &str, id: i64) -> bool {
    sqlx::query_scalar(&format!(
        "SELECT EXISTS (SELECT 1 FROM {table} WHERE id = $1)"
    ))
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api opaque_ids_p2 -- --ignored --test-threads=1`"]
#[expect(
    clippy::too_many_lines,
    reason = "long but linear: one request per id form and per outcome"
)]
async fn deleted_journey_records_are_listed_and_addressed_by_either_id() {
    crate::data::recently_deleted::set_for_tests(true, 7);
    let pool = connect().await;
    let prefix = "opx-del";
    cleanup(&pool, prefix).await;
    let (owner, other) = (format!("{prefix}-owner"), format!("{prefix}-other"));
    let cookie = seed_user(&pool, &owner).await;
    seed_user(&pool, &other).await;
    let (first, first_public) = seed_deletion(&pool, &owner, "web").await;
    let (second, second_public) = seed_deletion(&pool, &owner, "web").await;
    let (third, third_public) = seed_deletion(&pool, &owner, "web").await;
    let (app_row, app_public) = seed_deletion(&pool, &owner, "ds-mcp").await;
    let (theirs, theirs_public) = seed_deletion(&pool, &other, "web").await;
    assert!(first_public.starts_with("x_") && first_public.len() == 34);
    let router = router(pool.clone(), NumericIds::Served);

    // Recently deleted: every row, the app's too, with both ids.
    let (status, list) = call(&router, "GET", "/Journeys/recently-deleted", &cookie, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 4, "{list}");
    let seen: Vec<(i64, &str)> = items
        .iter()
        .map(|item| {
            (
                item["id"].as_i64().unwrap(),
                item["publicId"].as_str().unwrap(),
            )
        })
        .collect();
    for (id, public) in [
        (first, &first_public),
        (second, &second_public),
        (third, &third_public),
        (app_row, &app_public),
    ] {
        assert!(seen.contains(&(id, public.as_str())), "{seen:?}");
    }

    // Connected apps: the app's delete, one id for both lists.
    let (status, apps) = call(
        &router,
        "GET",
        "/public/account/connected-apps",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{apps}");
    let deletions = apps["recentDeletions"].as_array().unwrap();
    assert_eq!(deletions.len(), 1, "{apps}");
    assert_eq!(deletions[0]["id"], app_row);
    assert_eq!(deletions[0]["publicId"], app_public);

    // "Delete permanently", in both forms.
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/Journeys/recently-deleted/{first_public}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!exists(&pool, "delegated_deletions", first).await);
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/Journeys/recently-deleted/{second}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!exists(&pool, "delegated_deletions", second).await);

    // Unknown: 404, as an unknown number is. Malformed or another kind's: 400.
    for id in [NO_DELETION, "999999999"] {
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/Journeys/recently-deleted/{id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
    }
    for id in ["x_nothex", "x_", NO_TOKEN, "nope"] {
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/Journeys/recently-deleted/{id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{id}");
    }

    // Another user's row, in either form: not found, and still there.
    for id in [theirs_public.clone(), theirs.to_string()] {
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/Journeys/recently-deleted/{id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
        let (status, _) = call(
            &router,
            "POST",
            &format!("/Journeys/recently-deleted/{id}/restore"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "restore {id}");
    }
    assert!(exists(&pool, "delegated_deletions", theirs).await);

    // "Save again": the id resolves in both forms (the fixture has no legs,
    // so a found row is a 422 and a missing one a 404).
    for id in [third_public.clone(), third.to_string()] {
        let (status, body) = call(
            &router,
            "POST",
            &format!("/Journeys/recently-deleted/{id}/restore"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{id}: {body}");
    }
    for (id, want) in [
        (NO_DELETION, StatusCode::NOT_FOUND),
        ("x_nothex", StatusCode::BAD_REQUEST),
    ] {
        let (status, _) = call(
            &router,
            "POST",
            &format!("/Journeys/recently-deleted/{id}/restore"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, want, "restore {id}");
    }

    cleanup(&pool, prefix).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api opaque_ids_p2 -- --ignored --test-threads=1`"]
#[expect(
    clippy::too_many_lines,
    reason = "long but linear: one request per id form and per outcome"
)]
async fn personal_tokens_are_listed_and_revoked_by_either_id() {
    let pool = connect().await;
    let prefix = "opx-tok";
    cleanup(&pool, prefix).await;
    let (owner, other) = (format!("{prefix}-owner"), format!("{prefix}-other"));
    let cookie = seed_user(&pool, &owner).await;
    seed_user(&pool, &other).await;
    let router = router(pool.clone(), NumericIds::Served);

    // Created through the route: the response carries both ids.
    let (status, created) = call(
        &router,
        "POST",
        "/public/account/personal-tokens",
        &cookie,
        Some(json!({"name": "Display", "scopes": ["lines:read"], "lifetimeDays": 90})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let created_id = created["id"].as_i64().unwrap();
    let created_public = created["publicId"].as_str().unwrap().to_owned();
    assert!(created_public.starts_with("k_") && created_public.len() == 34);
    let (second, second_public) = seed_token(&pool, &owner).await;
    let (theirs, theirs_public) = seed_token(&pool, &other).await;

    let (status, list) = call(
        &router,
        "GET",
        "/public/account/personal-tokens",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let tokens = list["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 2, "{list}");
    let seen: Vec<(i64, &str)> = tokens
        .iter()
        .map(|token| {
            (
                token["id"].as_i64().unwrap(),
                token["publicId"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(
        seen.contains(&(created_id, created_public.as_str())),
        "{seen:?}"
    );
    assert!(seen.contains(&(second, second_public.as_str())), "{seen:?}");

    // Another user's token, in either form: 204 and untouched.
    for id in [theirs_public.clone(), theirs.to_string()] {
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/public/account/personal-tokens/{id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{id}");
    }
    let revoked_at: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT revoked_at FROM personal_api_tokens WHERE id = $1")
            .bind(theirs)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoked_at.is_none());

    // Unknown or malformed ids name no token: 204, as an unknown number does.
    for id in [NO_TOKEN, "k_nothex", NO_WEBHOOK, "nope", "999999999"] {
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/public/account/personal-tokens/{id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{id}");
    }
    let (_, list) = call(
        &router,
        "GET",
        "/public/account/personal-tokens",
        &cookie,
        None,
    )
    .await;
    assert_eq!(list["tokens"].as_array().unwrap().len(), 2, "{list}");

    // Revoked by the opaque id, then by the number.
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/public/account/personal-tokens/{created_public}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/public/account/personal-tokens/{second}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = call(
        &router,
        "GET",
        "/public/account/personal-tokens",
        &cookie,
        None,
    )
    .await;
    assert!(list["tokens"].as_array().unwrap().is_empty(), "{list}");

    cleanup(&pool, prefix).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api opaque_ids_p2 -- --ignored --test-threads=1`"]
#[expect(
    clippy::too_many_lines,
    reason = "long but linear: one request per id form and per outcome"
)]
async fn webhook_destinations_are_listed_and_addressed_by_either_id() {
    crate::data::webhooks::set_enabled_for_tests(true);
    let pool = connect().await;
    let prefix = "opx-web";
    cleanup(&pool, prefix).await;
    let (owner, other) = (format!("{prefix}-owner"), format!("{prefix}-other"));
    let cookie = seed_user(&pool, &owner).await;
    seed_user(&pool, &other).await;
    let (first, first_public) = seed_webhook(&pool, &owner, "one").await;
    let (second, second_public) = seed_webhook(&pool, &owner, "two").await;
    let (theirs, theirs_public) = seed_webhook(&pool, &other, "theirs").await;
    assert!(first_public.starts_with("w_") && first_public.len() == 34);
    let router = router(pool.clone(), NumericIds::Served);

    let (status, list) = call(&router, "GET", "/public/account/webhooks", &cookie, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let destinations = list["destinations"].as_array().unwrap();
    assert_eq!(destinations.len(), 2, "{list}");
    assert_eq!(destinations[0]["id"], first);
    assert_eq!(destinations[0]["publicId"], first_public);
    assert_eq!(destinations[1]["id"], second);
    assert_eq!(destinations[1]["publicId"], second_public);

    // Turned off by the opaque id, on by the number; the answer carries both.
    let (status, row) = call(
        &router,
        "PATCH",
        &format!("/public/account/webhooks/{first_public}"),
        &cookie,
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(row["enabled"], false);
    assert_eq!(row["id"], first);
    assert_eq!(row["publicId"], first_public);
    let (status, row) = call(
        &router,
        "PATCH",
        &format!("/public/account/webhooks/{first}"),
        &cookie,
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{row}");
    assert_eq!(row["enabled"], true);
    assert_eq!(row["publicId"], first_public);

    // Unknown: 404 to update, 204 to delete (idempotent). Malformed: 400.
    for id in [NO_WEBHOOK, "999999999"] {
        let (status, _) = call(
            &router,
            "PATCH",
            &format!("/public/account/webhooks/{id}"),
            &cookie,
            Some(json!({"enabled": false})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/public/account/webhooks/{id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{id}");
    }
    for id in ["w_nothex", NO_TOKEN, "nope"] {
        for method in ["PATCH", "DELETE"] {
            let (status, _) = call(
                &router,
                method,
                &format!("/public/account/webhooks/{id}"),
                &cookie,
                Some(json!({"enabled": false})),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {id}");
        }
    }

    // Another user's destination, in either form: not found, still there,
    // still on.
    for id in [theirs_public.clone(), theirs.to_string()] {
        let (status, _) = call(
            &router,
            "PATCH",
            &format!("/public/account/webhooks/{id}"),
            &cookie,
            Some(json!({"enabled": false})),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/public/account/webhooks/{id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{id}");
    }
    assert!(exists(&pool, "webhook_destinations", theirs).await);
    let enabled: bool =
        sqlx::query_scalar("SELECT enabled FROM webhook_destinations WHERE id = $1")
            .bind(theirs)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(enabled);

    // Deleted by the opaque id, then by the number.
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/public/account/webhooks/{first_public}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!exists(&pool, "webhook_destinations", first).await);
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/public/account/webhooks/{second}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!exists(&pool, "webhook_destinations", second).await);

    cleanup(&pool, prefix).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api opaque_ids_p2 -- --ignored --test-threads=1`"]
#[expect(
    clippy::too_many_lines,
    reason = "long but linear: one request per id form and per outcome"
)]
async fn time_away_periods_are_listed_and_addressed_by_either_id() {
    let pool = connect().await;
    let prefix = "opx-away";
    cleanup(&pool, prefix).await;
    let (owner, other) = (format!("{prefix}-owner"), format!("{prefix}-other"));
    let cookie = seed_user(&pool, &owner).await;
    seed_user(&pool, &other).await;
    let (theirs, theirs_public) = seed_away(&pool, &other).await;
    let router = router(pool.clone(), NumericIds::Served);
    // Within the year ahead a period may start (the 2099 rows are seeded).
    let start = crate::routes::london_today() + chrono::Duration::days(20);
    let body = |label: &str| json!({"startsOn": start.to_string(), "endsOn": (start + chrono::Duration::days(2)).to_string(), "label": label});

    // Created: the response and the list carry both ids.
    let (status, created) = call(
        &router,
        "POST",
        "/public/preferences/time-away",
        &cookie,
        Some(body("Trip")),
    )
    .await;
    assert!(status.is_success(), "{status}: {created}");
    let id = created["period"]["id"].as_i64().unwrap();
    let public = created["period"]["publicId"].as_str().unwrap().to_owned();
    assert!(public.starts_with("a_") && public.len() == 34);
    let (second, second_public) = seed_away(&pool, &owner).await;
    let (status, list) = call(
        &router,
        "GET",
        "/public/preferences/time-away",
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let periods = list.as_array().unwrap();
    assert_eq!(periods.len(), 2, "{list}");
    let seen: Vec<(i64, &str)> = periods
        .iter()
        .map(|period| {
            (
                period["id"].as_i64().unwrap(),
                period["publicId"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(seen.contains(&(id, public.as_str())), "{seen:?}");
    assert!(seen.contains(&(second, second_public.as_str())), "{seen:?}");

    // Replaced by the opaque id and by the number.
    for (path_id, label) in [
        (public.clone(), "By opaque id"),
        (id.to_string(), "By number"),
    ] {
        let (status, updated) = call(
            &router,
            "PUT",
            &format!("/public/preferences/time-away/{path_id}"),
            &cookie,
            Some(body(label)),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path_id}: {updated}");
        assert_eq!(updated["period"]["label"], label);
        assert_eq!(updated["period"]["id"], id);
        assert_eq!(updated["period"]["publicId"], public);
    }

    // Unknown: 404. Malformed or another kind's: 400.
    for path_id in [NO_AWAY, "999999999"] {
        let (status, _) = call(
            &router,
            "PUT",
            &format!("/public/preferences/time-away/{path_id}"),
            &cookie,
            Some(body("x")),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "PUT {path_id}");
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/public/preferences/time-away/{path_id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "DELETE {path_id}");
    }
    for path_id in ["a_nothex", NO_WEBHOOK, "nope"] {
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/public/preferences/time-away/{path_id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path_id}");
    }

    // Another user's period, in either form: not found, still there.
    for path_id in [theirs_public.clone(), theirs.to_string()] {
        let (status, _) = call(
            &router,
            "PUT",
            &format!("/public/preferences/time-away/{path_id}"),
            &cookie,
            Some(body("x")),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "PUT {path_id}");
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("/public/preferences/time-away/{path_id}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "DELETE {path_id}");
    }
    assert!(exists(&pool, "user_away_periods", theirs).await);

    // Deleted by the opaque id, then by the number.
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/public/preferences/time-away/{public}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!exists(&pool, "user_away_periods", id).await);
    let (status, _) = call(
        &router,
        "DELETE",
        &format!("/public/preferences/time-away/{second}"),
        &cookie,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!exists(&pool, "user_away_periods", second).await);

    cleanup(&pool, prefix).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api opaque_ids_p2 -- --ignored --test-threads=1`"]
async fn the_four_families_withhold_their_numeric_ids() {
    crate::data::recently_deleted::set_for_tests(true, 7);
    crate::data::webhooks::set_enabled_for_tests(true);
    let pool = connect().await;
    let prefix = "opx-nid";
    cleanup(&pool, prefix).await;
    let owner = format!("{prefix}-owner");
    let cookie = seed_user(&pool, &owner).await;
    seed_deletion(&pool, &owner, "web").await;
    let (_, app_public) = seed_deletion(&pool, &owner, "ds-mcp").await;
    let (_, token_public) = seed_token(&pool, &owner).await;
    let (_, webhook_public) = seed_webhook(&pool, &owner, "nid").await;
    let (_, away_public) = seed_away(&pool, &owner).await;
    let served = router(pool.clone(), NumericIds::Served);
    let withheld = router(pool.clone(), NumericIds::Withheld);
    let start = crate::routes::london_today() + chrono::Duration::days(20);

    // Each list, with the place its rows are in and the opaque id they keep.
    let cases: [(&str, &[&str], &str); 5] = [
        ("/Journeys/recently-deleted", &["items", "0"], &app_public),
        (
            "/public/account/connected-apps",
            &["recentDeletions", "0"],
            &app_public,
        ),
        (
            "/public/account/personal-tokens",
            &["tokens", "0"],
            &token_public,
        ),
        (
            "/public/account/webhooks",
            &["destinations", "0"],
            &webhook_public,
        ),
        ("/public/preferences/time-away", &["0"], &away_public),
    ];
    for (uri, path, public) in cases {
        let row = |value: &Value| -> Value {
            path.iter()
                .fold(value.clone(), |at, key| match key.parse::<usize>() {
                    Ok(index) => at[index].clone(),
                    Err(_) => at[*key].clone(),
                })
        };
        let (status, on) = call(&served, "GET", uri, &cookie, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {on}");
        assert!(row(&on)["id"].as_i64().unwrap() > 0, "{uri} served: {on}");
        assert_eq!(row(&on)["publicId"], public, "{uri}");

        let (status, off) = call(&withheld, "GET", uri, &cookie, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {off}");
        assert_eq!(row(&off)["id"], 0, "{uri} withheld: {off}");
        assert_eq!(row(&off)["publicId"], public, "{uri}");
        assert!(
            leaks(&off).is_empty(),
            "{uri} still serves {:?}",
            leaks(&off)
        );
    }

    // The create and update responses carry the pair too.
    let (status, created) = call(
        &withheld,
        "POST",
        "/public/account/personal-tokens",
        &cookie,
        Some(json!({"name": "New", "scopes": ["lines:read"], "lifetimeDays": 90})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["id"], 0);
    assert!(created["publicId"].as_str().unwrap().starts_with("k_"));
    let (status, created) = call(
        &withheld,
        "POST",
        "/public/preferences/time-away",
        &cookie,
        Some(json!({"startsOn": start.to_string(), "endsOn": start.to_string()})),
    )
    .await;
    assert!(status.is_success(), "{created}");
    assert_eq!(created["period"]["id"], 0);
    assert!(
        created["period"]["publicId"]
            .as_str()
            .unwrap()
            .starts_with("a_")
    );
    let (status, updated) = call(
        &withheld,
        "PATCH",
        &format!("/public/account/webhooks/{webhook_public}"),
        &cookie,
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["id"], 0);
    assert_eq!(updated["publicId"], webhook_public);

    cleanup(&pool, prefix).await;
}

/// Numeric use of each id is counted in
/// `distant_signal_legacy_id_requests_total{kind}`; opaque use is not.
#[test]
#[ignore = "requires a live database; run with `cargo test -p api opaque_ids_p2 -- --ignored --test-threads=1`"]
fn only_numeric_use_of_the_four_ids_is_counted() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(numeric_and_opaque_requests(&handle));
    });
}

async fn numeric_and_opaque_requests(handle: &metrics_exporter_prometheus::PrometheusHandle) {
    crate::data::recently_deleted::set_for_tests(true, 7);
    crate::data::webhooks::set_enabled_for_tests(true);
    let pool = connect().await;
    let prefix = "opx-cnt";
    cleanup(&pool, prefix).await;
    let owner = format!("{prefix}-owner");
    let cookie = seed_user(&pool, &owner).await;
    let router = router(pool.clone(), NumericIds::Served);

    let (deletion, deletion_public) = seed_deletion(&pool, &owner, "web").await;
    let (token, token_public) = seed_token(&pool, &owner).await;
    let (webhook, webhook_public) = seed_webhook(&pool, &owner, "one").await;
    let (away, away_public) = seed_away(&pool, &owner).await;
    // (kind, route, the id in both forms, what the numeric repeat of the
    // delete answers once the row has gone).
    let cases = [
        (
            "deletion",
            "/Journeys/recently-deleted",
            deletion,
            deletion_public,
            StatusCode::NOT_FOUND,
        ),
        (
            "token",
            "/public/account/personal-tokens",
            token,
            token_public,
            StatusCode::NO_CONTENT,
        ),
        (
            "webhook",
            "/public/account/webhooks",
            webhook,
            webhook_public,
            StatusCode::NO_CONTENT,
        ),
        (
            "away",
            "/public/preferences/time-away",
            away,
            away_public,
            StatusCode::NOT_FOUND,
        ),
    ];
    let counted =
        |kind: &str| format!(r#"distant_signal_legacy_id_requests_total{{kind="{kind}"}} "#);
    let count = |kind: &str| -> u64 {
        let rendered = handle.render();
        let prefix = counted(kind);
        rendered
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .map_or(0, |value| value.trim().parse().unwrap())
    };

    // The opaque form: answered, and not counted.
    for (kind, base, _, opaque, _) in &cases {
        let before = count(kind);
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("{base}/{opaque}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{base}");
        assert_eq!(count(kind), before, "{kind}: an opaque id was counted");
    }
    // The number: counted once.
    for (kind, base, numeric, _, repeat) in &cases {
        let before = count(kind);
        let (status, _) = call(
            &router,
            "DELETE",
            &format!("{base}/{numeric}"),
            &cookie,
            None,
        )
        .await;
        assert_eq!(status, *repeat, "{base}");
        assert_eq!(
            count(kind),
            before + 1,
            "{kind}: a numeric id was not counted"
        );
    }
    cleanup(&pool, prefix).await;
}
