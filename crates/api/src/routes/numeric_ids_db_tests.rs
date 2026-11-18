//! Stage 2b of DS-534's opaque ids against a live database: the same request
//! answered with the numeric ids served and withheld (`API_SERVE_NUMERIC_IDS`,
//! `numeric_ids`), for each family that has an opaque id: journeys and their
//! legs and breaks, tracked trains, group rows, regular journeys and push
//! devices.
//!
//! Each case checks that the served answer does carry numeric ids (so the
//! case would notice a family that stopped), that the withheld one carries
//! none, and that every opaque id is the same in both.
//!
//! Fixtures come from `data::journey_follows::db_tests`, under one `nid-*`
//! user prefix per test.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

use crate::auth::hash_session_token;
use crate::data::journey_follows::db_tests::{cleanup, exec, seed_journey, seed_shared_journey};
use crate::numeric_ids::{NumericIds, leaks, numeric_partner, withhold_numeric_ids};

async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to postgres")
}

/// The api's routes as `main` serves them for the families under test, with
/// the layer installed exactly when `policy` withholds.
fn router(pool: PgPool, policy: NumericIds) -> axum::Router {
    let router = crate::app::Router::new()
        .merge(super::journeys::router())
        .merge(super::groups::router())
        .merge(super::train::router())
        .merge(super::journey_templates::router())
        .nest("/public", super::notifications::router())
        .with_state(crate::test_support::inert_app(pool));
    if policy.serves() {
        router
    } else {
        router.layer(axum::middleware::from_fn_with_state(
            policy,
            withhold_numeric_ids,
        ))
    }
}

/// Removes everything a test with this `prefix` made.
async fn cleanup_all(pool: &PgPool, prefix: &str) {
    let like = format!("{prefix}-%");
    for sql in [
        "DELETE FROM journey_template_legs WHERE template_id IN \
         (SELECT id FROM journey_templates WHERE user_id LIKE $1)",
        "DELETE FROM journey_templates WHERE user_id LIKE $1",
        "DELETE FROM push_subscriptions WHERE user_id LIKE $1",
        "DELETE FROM train_subscriptions WHERE user_id LIKE $1",
        "DELETE FROM sessions WHERE user_id LIKE $1",
    ] {
        sqlx::query(sql)
            .bind(&like)
            .execute(pool)
            .await
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    cleanup(pool, prefix).await;
}

async fn session(pool: &PgPool, user: &str) -> String {
    let raw = format!("raw-nid-{user}");
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

async fn get(router: &axum::Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let request = Request::builder()
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
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Every string under an opaque id key in `value`, in document order.
fn opaque_ids(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match value {
        Value::Object(map) => {
            for (key, field) in map {
                if numeric_partner(key).is_some() {
                    match field {
                        Value::String(id) => out.push(id.clone()),
                        Value::Array(items) => {
                            out.extend(items.iter().filter_map(Value::as_str).map(str::to_owned));
                        }
                        _ => {}
                    }
                }
                out.extend(opaque_ids(field));
            }
        }
        Value::Array(items) => {
            for item in items {
                out.extend(opaque_ids(item));
            }
        }
        _ => {}
    }
    out
}

/// `uri` for `cookie` with the numeric ids served and withheld; asserts the
/// contract (see the module doc) and returns both answers.
async fn both(pool: &PgPool, uri: &str, cookie: &str) -> (Value, Value) {
    let (status, served) = get(&router(pool.clone(), NumericIds::Served), uri, cookie).await;
    assert_eq!(status, StatusCode::OK, "{uri} served: {served}");
    let (status, withheld) = get(&router(pool.clone(), NumericIds::Withheld), uri, cookie).await;
    assert_eq!(status, StatusCode::OK, "{uri} withheld: {withheld}");
    assert!(
        !leaks(&served).is_empty(),
        "{uri} served no numeric id to withhold: {served}"
    );
    assert!(
        leaks(&withheld).is_empty(),
        "{uri} still serves {:?}: {withheld}",
        leaks(&withheld)
    );
    let opaque = opaque_ids(&served);
    assert!(!opaque.is_empty(), "{uri} has no opaque id: {served}");
    assert_eq!(opaque, opaque_ids(&withheld), "{uri} changed an opaque id");
    (served, withheld)
}

async fn add_user(pool: &PgPool, id: &str) {
    exec(pool, "INSERT INTO users (id, name) VALUES ($1, $1)", &[id]).await;
}

/// A regular journey of `user` with one leg; returns its id.
async fn seed_template(pool: &PgPool, user: &str) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO journey_templates (user_id, custom_name, days_of_week, active) \
         VALUES ($1, 'Commute', 127, TRUE) RETURNING id",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("seed template");
    sqlx::query(
        "INSERT INTO journey_template_legs (template_id, leg_order, origin_crs, destination_crs) \
         VALUES ($1, 1, 'WAT', 'RDG')",
    )
    .bind(id)
    .execute(pool)
    .await
    .expect("seed template leg");
    id
}

/// A bare tracked train of `user`; returns its id.
async fn seed_subscription(pool: &PgPool, user: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO train_subscriptions \
            (user_id, service_date, pin_origin_crs, pin_scheduled_departure) \
         VALUES ($1, CURRENT_DATE, 'WOK', NOW()) RETURNING id",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .expect("seed a tracked train")
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api numeric_ids -- --ignored --test-threads=1`"]
async fn journeys_legs_and_breaks_withhold_their_numeric_ids() {
    let pool = connect().await;
    let prefix = "nid-jr";
    cleanup_all(&pool, prefix).await;
    let owner = format!("{prefix}-owner");
    add_user(&pool, &owner).await;
    let template = seed_template(&pool, &owner).await;
    let journey = seed_journey(&pool, &owner, 2).await;
    let leg: i64 = sqlx::query_scalar("SELECT id FROM journey_legs WHERE journey_id = $1")
        .bind(journey)
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE journeys SET source_template_id = $1 WHERE id = $2")
        .bind(template)
        .bind(journey)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO journey_breaks (journey_id, user_id, cause, fault, severity, \
             trigger_leg_id, break_position, fingerprint) \
         VALUES ($1, $2, 'user_missed', 'user', 1, $3, 1, '')",
    )
    .bind(journey)
    .bind(&owner)
    .bind(leg)
    .execute(&pool)
    .await
    .unwrap();
    let cookie = session(&pool, &owner).await;

    // The list: id, legId, openBreakId, sourceTemplateId.
    let (served, withheld) = both(&pool, "/Journeys/mine", &cookie).await;
    assert_eq!(served[0]["id"], journey);
    assert_eq!(served[0]["legId"], leg);
    assert_eq!(served[0]["sourceTemplateId"], template);
    assert!(served[0]["openBreakId"].as_i64().unwrap() > 0, "{served}");
    for field in ["id", "legId", "openBreakId", "sourceTemplateId"] {
        assert_eq!(withheld[0][field], 0, "{field}: {withheld}");
    }
    assert!(withheld[0]["publicId"].as_str().unwrap().starts_with("j_"));
    assert!(
        withheld[0]["openBreakPublicId"]
            .as_str()
            .unwrap()
            .starts_with("b_")
    );
    assert!(
        withheld[0]["sourceTemplatePublicId"]
            .as_str()
            .unwrap()
            .starts_with("r_")
    );

    // The page: the journey, its legs, its open break.
    let public = served[0]["publicId"].as_str().unwrap().to_owned();
    let (served, withheld) = both(&pool, &format!("/Journeys/{public}"), &cookie).await;
    assert_eq!(served["id"], journey);
    assert_eq!(served["legs"][0]["id"], leg);
    assert!(served["openBreak"]["id"].as_i64().unwrap() > 0, "{served}");
    assert_eq!(withheld["id"], 0);
    assert_eq!(withheld["legs"][0]["id"], 0);
    assert_eq!(withheld["openBreak"]["id"], 0);
    assert_eq!(withheld["openBreak"]["triggerLegId"], 0);

    // The break on its own.
    let (served, withheld) = both(
        &pool,
        &format!("/Journeys/{public}/breaks/current"),
        &cookie,
    )
    .await;
    assert!(served["id"].as_i64().unwrap() > 0, "{served}");
    assert_eq!(withheld["id"], 0);
    assert_eq!(withheld["journeyId"], 0);
    assert_eq!(withheld["triggerLegId"], 0);

    cleanup_all(&pool, prefix).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api numeric_ids -- --ignored --test-threads=1`"]
async fn tracked_trains_withhold_their_numeric_ids() {
    let pool = connect().await;
    let prefix = "nid-tr";
    cleanup_all(&pool, prefix).await;
    let owner = format!("{prefix}-owner");
    add_user(&pool, &owner).await;
    let subscription = seed_subscription(&pool, &owner).await;
    let cookie = session(&pool, &owner).await;

    let (served, withheld) = both(&pool, "/Train/mine", &cookie).await;
    assert_eq!(served[0]["id"], subscription);
    assert_eq!(withheld[0]["id"], 0);
    let public = withheld[0]["publicId"].as_str().unwrap().to_owned();
    assert!(public.starts_with("t_"));

    let (served, withheld) = both(&pool, &format!("/Train/{public}"), &cookie).await;
    assert_eq!(served["id"], subscription);
    assert_eq!(withheld["id"], 0);
    assert_eq!(withheld["publicId"], public);

    cleanup_all(&pool, prefix).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api numeric_ids -- --ignored --test-threads=1`"]
async fn group_rows_withhold_their_numeric_ids() {
    let pool = connect().await;
    let prefix = "nid-gr";
    let journey = seed_shared_journey(&pool, prefix, 2).await;
    let owner = format!("{prefix}-owner");
    let friend = format!("{prefix}-friend");
    let group = format!("{prefix}-g");
    let subscription = seed_subscription(&pool, &owner).await;
    sqlx::query(
        "INSERT INTO group_trains (group_id, train_subscription_id, added_by) VALUES ($1, $2, $3)",
    )
    .bind(&group)
    .bind(subscription)
    .bind(&owner)
    .execute(&pool)
    .await
    .unwrap();
    let cookie = session(&pool, &friend).await;

    let (served, withheld) = both(&pool, &format!("/groups/{group}/journeys"), &cookie).await;
    assert_eq!(served[0]["journeyId"], journey);
    assert_eq!(withheld[0]["journeyId"], 0);
    let (_, withheld) = both(&pool, "/groups/shared-journeys", &cookie).await;
    assert_eq!(withheld[0]["journeyId"], 0);

    let (served, withheld) = both(&pool, &format!("/groups/{group}/trains"), &cookie).await;
    assert_eq!(served[0]["trainSubscriptionId"], subscription);
    assert_eq!(withheld[0]["trainSubscriptionId"], 0);
    let (_, withheld) = both(&pool, "/groups/shared-trains", &cookie).await;
    assert_eq!(withheld[0]["trainSubscriptionId"], 0);

    cleanup_all(&pool, prefix).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api numeric_ids -- --ignored --test-threads=1`"]
async fn regular_journeys_withhold_their_numeric_ids() {
    let pool = connect().await;
    let prefix = "nid-rj";
    cleanup_all(&pool, prefix).await;
    let owner = format!("{prefix}-owner");
    add_user(&pool, &owner).await;
    let first = seed_template(&pool, &owner).await;
    let second = seed_template(&pool, &owner).await;
    sqlx::query("UPDATE journey_templates SET paired_template_id = $1 WHERE id = $2")
        .bind(second)
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
    let cookie = session(&pool, &owner).await;

    let (served, withheld) = both(&pool, "/JourneyTemplates/mine", &cookie).await;
    let rows = served.as_array().unwrap();
    let paired = rows.iter().position(|row| row["id"] == first).unwrap();
    assert_eq!(served[paired]["pairedTemplateId"], second);
    assert_eq!(withheld[paired]["id"], 0);
    assert_eq!(withheld[paired]["pairedTemplateId"], 0);
    let public = withheld[paired]["publicId"].as_str().unwrap().to_owned();
    assert!(public.starts_with("r_"));

    let (served, withheld) = both(&pool, &format!("/JourneyTemplates/{public}"), &cookie).await;
    assert_eq!(served["id"], first);
    assert_eq!(withheld["id"], 0);
    assert_eq!(withheld["pairedTemplateId"], 0);

    cleanup_all(&pool, prefix).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api numeric_ids -- --ignored --test-threads=1`"]
async fn push_devices_withhold_their_numeric_ids() {
    let pool = connect().await;
    let prefix = "nid-dv";
    cleanup_all(&pool, prefix).await;
    let owner = format!("{prefix}-owner");
    add_user(&pool, &owner).await;
    let device: i64 = sqlx::query_scalar(
        "INSERT INTO push_subscriptions (user_id, endpoint, p256dh, auth) \
         VALUES ($1, 'https://push.example.invalid/nid-dv', 'p256dh', 'auth') RETURNING id",
    )
    .bind(&owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    let cookie = session(&pool, &owner).await;

    let (served, withheld) = both(&pool, "/public/notifications/devices", &cookie).await;
    assert_eq!(served[0]["id"], device);
    assert_eq!(withheld[0]["id"], 0);
    assert!(withheld[0]["publicId"].as_str().unwrap().starts_with("d_"));

    cleanup_all(&pool, prefix).await;
}
