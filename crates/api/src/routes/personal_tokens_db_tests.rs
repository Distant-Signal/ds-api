//! Personal API tokens end to end, against a live database
//! (docs/specs/personal-api-tokens.md): created on the management routes
//! with a session cookie, then used on the token listener's router, where
//! the real gate, limiter, envelope and handlers run.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode, header};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use crate::app::App;
use crate::auth::hash_session_token;
use crate::personal_tokens::{PersonalTokenSettings, PersonalTokens};

const ORIGIN: &str = "https://example.invalid";

async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to postgres")
}

fn settings() -> PersonalTokenSettings {
    PersonalTokenSettings {
        enabled: true,
        listen_addr: "127.0.0.1:0".to_string(),
        public_base_url: "https://ds-api.example.invalid".to_string(),
        max_active_per_user: 3,
        ..PersonalTokenSettings::default()
    }
}

struct Harness {
    /// The website's router: `/public/*` behind the main listener's layers.
    main: axum::Router,
    /// The token listener's router.
    tokens: axum::Router,
}

fn harness(pool: PgPool, settings: PersonalTokenSettings) -> Harness {
    let mut state = Arc::try_unwrap(crate::test_support::inert_app(pool)).expect("a fresh app");
    state.personal_tokens = Arc::new(PersonalTokens::new(settings));
    let app: App = Arc::new(state);
    let main = crate::app::Router::new()
        .merge(super::journeys::router())
        .nest("/public", super::public_router())
        .with_state(app.clone())
        .layer(axum::middleware::from_fn(
            crate::personal_tokens::refuse_on_main_listener,
        ));
    let tokens = crate::personal_tokens::router(&app, &crate::edge::EdgeSettings::default())
        .with_state(app.clone());
    Harness { main, tokens }
}

async fn cleanup(pool: &PgPool, prefix: &str) {
    sqlx::query("DELETE FROM users WHERE id LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await
        .expect("delete fixture users");
}

/// Seeds a user and a session; returns the raw cookie value.
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

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, value)
}

fn cookie(raw: &str) -> String {
    format!("distant_signal_session={raw}")
}

/// Creates a token through the management route; returns the plaintext.
async fn create(h: &Harness, raw: &str, scopes: &[&str]) -> (StatusCode, Value) {
    let (status, headers, body) = send(
        &h.main,
        "POST",
        "/public/account/personal-tokens",
        &[("cookie", &cookie(raw)), ("origin", ORIGIN)],
        Some(json!({"name": "Kitchen display", "scopes": scopes, "lifetimeDays": 90})),
    )
    .await;
    if status == StatusCode::CREATED {
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }
    (status, body)
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

#[tokio::test]
#[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
            personal_tokens_db_tests -- --ignored --test-threads=1`"]
async fn a_created_token_reads_within_its_scopes_only() {
    let pool = connect().await;
    cleanup(&pool, "TEST-PTOK-A").await;
    let h = harness(pool.clone(), settings());
    let raw = seed_user(&pool, "TEST-PTOK-A1").await;

    let (status, body) = create(&h, &raw, &["journeys:read", "lines:read"]).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let token = body["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("dsp_") && token.len() == 42);
    assert_eq!(body["hint"], token[38..]);
    assert_eq!(body["scopes"], json!(["journeys:read", "lines:read"]));

    // Stored hashed: the plaintext is nowhere in the table.
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM personal_api_tokens WHERE user_id = $1")
            .bind("TEST-PTOK-A1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, crate::personal_tokens::token::hash(&token).to_vec());

    // The page lists it, without the token.
    let (status, _, list) = send(
        &h.main,
        "GET",
        "/public/account/personal-tokens",
        &[("cookie", &cookie(&raw))],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["apiBaseUrl"], "https://ds-api.example.invalid");
    assert_eq!(list["tokens"].as_array().unwrap().len(), 1);
    assert!(!list.to_string().contains(&token));

    // /v1/me, wrapped, with the caching headers.
    let auth = bearer(&token);
    let (status, headers, me) = send(
        &h.tokens,
        "GET",
        "/v1/me",
        &[("authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["data"]["name"], "Kitchen display");
    assert_eq!(me["data"]["scopes"], json!(["journeys:read", "lines:read"]));
    assert!(me["attribution"]["statement"].is_string());
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");

    // A scoped read, with an ETag that answers 304.
    let (status, headers, mine) = send(
        &h.tokens,
        "GET",
        "/v1/Journeys/mine",
        &[("authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    assert_eq!(mine["data"], json!([]));
    assert!(
        !mine["attribution"]["sources"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(headers[header::CACHE_CONTROL], "private, max-age=30");
    assert_eq!(headers[header::VARY], "Authorization");
    assert_eq!(headers["ds-poll-interval"], "60");
    let etag = headers[header::ETAG].to_str().unwrap().to_string();
    let (status, _, _) = send(
        &h.tokens,
        "GET",
        "/v1/Journeys/mine",
        &[("authorization", &auth), ("if-none-match", &etag)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);

    // Outside its scopes: 403 with the scope it needs.
    let (status, headers, body) = send(
        &h.tokens,
        "GET",
        "/v1/preferences",
        &[("authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "insufficient_scope");
    assert!(
        headers[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .contains("preferences:read")
    );

    // No write anywhere: a POST is a 405, an unknown path a 404.
    let (status, _, _) = send(
        &h.tokens,
        "POST",
        "/v1/Journeys/mine",
        &[("authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let (status, _, _) = send(
        &h.tokens,
        "GET",
        "/v1/Train/mine",
        &[("authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // last_used_at was recorded.
    let used: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM personal_api_tokens WHERE user_id = $1")
            .bind("TEST-PTOK-A1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(used.is_some());

    // The same token on the website's listener: refused, never looked up.
    let (status, _, body) = send(
        &h.main,
        "GET",
        "/Journeys/mine",
        &[("authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "invalid_token");

    cleanup(&pool, "TEST-PTOK-A").await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
            personal_tokens_db_tests -- --ignored --test-threads=1`"]
async fn bad_tokens_and_cookies_are_refused() {
    let pool = connect().await;
    cleanup(&pool, "TEST-PTOK-B").await;
    let h = harness(pool.clone(), settings());
    let raw = seed_user(&pool, "TEST-PTOK-B1").await;

    // The status route needs no token.
    let (status, _, body) = send(&h.tokens, "GET", "/v1/status", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["ok"], true);

    // A session cookie is no credential on the token listener.
    let (status, headers, body) = send(
        &h.tokens,
        "GET",
        "/v1/Journeys/mine",
        &[("cookie", &cookie(&raw))],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "invalid_token");
    assert!(headers.contains_key(header::WWW_AUTHENTICATE));

    // Malformed, unknown (well-formed, never issued) and other schemes.
    let unknown = crate::personal_tokens::token::generate().token;
    for value in [
        "Bearer dsp_short".to_string(),
        bearer(&unknown),
        format!("Basic {unknown}"),
        format!("bearer {unknown}"),
    ] {
        let (status, _, _) = send(
            &h.tokens,
            "GET",
            "/v1/me",
            &[("authorization", &value)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{value}");
    }

    // Revoked: refused on the next request.
    let (_, body) = create(&h, &raw, &["lines:read"]).await;
    let token = body["token"].as_str().unwrap().to_string();
    let id = body["id"].as_i64().unwrap();
    let auth = bearer(&token);
    let (status, _, _) = send(
        &h.tokens,
        "GET",
        "/v1/me",
        &[("authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(
        &h.main,
        "DELETE",
        &format!("/public/account/personal-tokens/{id}"),
        &[("cookie", &cookie(&raw)), ("origin", ORIGIN)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(
        &h.tokens,
        "GET",
        "/v1/me",
        &[("authorization", &auth)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Expired, and idle (which also revokes it as inactive).
    let (_, body) = create(&h, &raw, &["lines:read"]).await;
    let expired = body["token"].as_str().unwrap().to_string();
    sqlx::query(
        "UPDATE personal_api_tokens SET created_at = now() - INTERVAL '2 days', \
         expires_at = now() - INTERVAL '1 second' WHERE id = $1",
    )
    .bind(body["id"].as_i64().unwrap())
    .execute(&pool)
    .await
    .unwrap();
    let (status, _, _) = send(
        &h.tokens,
        "GET",
        "/v1/me",
        &[("authorization", &bearer(&expired))],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (_, body) = create(&h, &raw, &["lines:read"]).await;
    let idle = body["token"].as_str().unwrap().to_string();
    let idle_id = body["id"].as_i64().unwrap();
    sqlx::query(
        "UPDATE personal_api_tokens SET created_at = now() - INTERVAL '100 days', \
         last_used_at = now() - INTERVAL '91 days' WHERE id = $1",
    )
    .bind(idle_id)
    .execute(&pool)
    .await
    .unwrap();
    let (status, _, _) = send(
        &h.tokens,
        "GET",
        "/v1/me",
        &[("authorization", &bearer(&idle))],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let reason: Option<String> =
        sqlx::query_scalar("SELECT revoked_reason FROM personal_api_tokens WHERE id = $1")
            .bind(idle_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("inactive"));

    cleanup(&pool, "TEST-PTOK-B").await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
            personal_tokens_db_tests -- --ignored --test-threads=1`"]
async fn the_rate_buckets_and_the_failure_bucket_hold() {
    let pool = connect().await;
    cleanup(&pool, "TEST-PTOK-C").await;
    let h = harness(
        pool.clone(),
        PersonalTokenSettings {
            token_per_minute: 1,
            token_burst: 2,
            auth_failure_per_minute: 1,
            auth_failure_burst: 2,
            ..settings()
        },
    );
    let raw = seed_user(&pool, "TEST-PTOK-C1").await;
    let (_, body) = create(&h, &raw, &["lines:read"]).await;
    let auth = bearer(body["token"].as_str().unwrap());
    let mut statuses = Vec::new();
    for _ in 0..3 {
        let (status, headers, body) = send(
            &h.tokens,
            "GET",
            "/v1/me",
            &[("authorization", &auth)],
            None,
        )
        .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            assert!(headers.contains_key(header::RETRY_AFTER));
            assert_eq!(body["error"], "rate_limited");
        }
        statuses.push(status);
    }
    assert_eq!(
        statuses,
        [
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS
        ]
    );

    // Failures from one address: after the burst, 429 before any lookup.
    let peer: SocketAddr = "198.51.100.9:5000".parse().unwrap();
    let mut statuses = Vec::new();
    for _ in 0..3 {
        let request = Request::builder()
            .uri("/v1/me")
            .header("authorization", "Bearer dsp_nope")
            .extension(ConnectInfo(peer))
            .body(Body::empty())
            .unwrap();
        statuses.push(h.tokens.clone().oneshot(request).await.unwrap().status());
    }
    assert_eq!(
        statuses,
        [
            StatusCode::UNAUTHORIZED,
            StatusCode::UNAUTHORIZED,
            StatusCode::TOO_MANY_REQUESTS
        ]
    );

    cleanup(&pool, "TEST-PTOK-C").await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
            personal_tokens_db_tests -- --ignored --test-threads=1`"]
async fn management_rules_hold() {
    let pool = connect().await;
    cleanup(&pool, "TEST-PTOK-D").await;
    let h = harness(pool.clone(), settings());
    let raw = seed_user(&pool, "TEST-PTOK-D1").await;
    let other = seed_user(&pool, "TEST-PTOK-D2").await;

    // Cross-site and bad requests.
    let (status, _, _) = send(
        &h.main,
        "POST",
        "/public/account/personal-tokens",
        &[
            ("cookie", &cookie(&raw)),
            ("origin", "https://evil.invalid"),
        ],
        Some(json!({"name": "x", "scopes": ["lines:read"], "lifetimeDays": 90})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, body) = send(
        &h.main,
        "POST",
        "/public/account/personal-tokens",
        &[("cookie", &cookie(&raw)), ("origin", ORIGIN)],
        Some(json!({"name": "x", "scopes": ["journeys:write"], "lifetimeDays": 90})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_request");
    // No session.
    let (status, _, _) = send(&h.main, "GET", "/public/account/personal-tokens", &[], None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The cap (3 in this harness).
    for _ in 0..3 {
        assert_eq!(
            create(&h, &raw, &["lines:read"]).await.0,
            StatusCode::CREATED
        );
    }
    let (status, body) = create(&h, &raw, &["lines:read"]).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "token_limit");

    // Another user cannot revoke mine.
    let mine: i64 = sqlx::query_scalar(
        "SELECT id FROM personal_api_tokens WHERE user_id = $1 ORDER BY id LIMIT 1",
    )
    .bind("TEST-PTOK-D1")
    .fetch_one(&pool)
    .await
    .unwrap();
    let (status, _, _) = send(
        &h.main,
        "DELETE",
        &format!("/public/account/personal-tokens/{mine}"),
        &[("cookie", &cookie(&other)), ("origin", ORIGIN)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let active = crate::data::personal_tokens::list_active(&pool, "TEST-PTOK-D1")
        .await
        .unwrap();
    assert_eq!(active.len(), 3);

    // Revoke all, then an admin revocation of a new one.
    let (status, _, _) = send(
        &h.main,
        "DELETE",
        "/public/account/personal-tokens",
        &[("cookie", &cookie(&raw)), ("origin", ORIGIN)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(
        crate::data::personal_tokens::list_active(&pool, "TEST-PTOK-D1")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        create(&h, &raw, &["lines:read"]).await.0,
        StatusCode::CREATED
    );
    let revocation = crate::data::users::revoke_all_sessions(
        &pool,
        "TEST-PTOK-D1",
        crate::data::delegated::RevokeReason::Admin,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(revocation.tokens_revoked, 1);
    // A back-channel logout keeps the user's tokens.
    let raw = seed_user(&pool, "TEST-PTOK-D1").await;
    assert_eq!(
        create(&h, &raw, &["lines:read"]).await.0,
        StatusCode::CREATED
    );
    let revocation = crate::data::users::revoke_all_sessions(
        &pool,
        "TEST-PTOK-D1",
        crate::data::delegated::RevokeReason::Backchannel,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(revocation.tokens_revoked, 0);

    // The sweep: expired rows end, and rows long ended are deleted.
    sqlx::query(
        "UPDATE personal_api_tokens SET created_at = now() - INTERVAL '2 days', \
         expires_at = now() - INTERVAL '1 day' WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind("TEST-PTOK-D1")
    .execute(&pool)
    .await
    .unwrap();
    let outcome = crate::data::personal_tokens::sweep(&pool, chrono::Utc::now(), 90, 365)
        .await
        .unwrap();
    assert!(outcome.expired >= 1);
    sqlx::query(
        "UPDATE personal_api_tokens SET revoked_at = now() - INTERVAL '400 days', \
         created_at = now() - INTERVAL '500 days', expires_at = now() - INTERVAL '450 days' \
         WHERE user_id = $1",
    )
    .bind("TEST-PTOK-D1")
    .execute(&pool)
    .await
    .unwrap();
    crate::data::personal_tokens::sweep(&pool, chrono::Utc::now(), 90, 365)
        .await
        .unwrap();
    let left: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM personal_api_tokens WHERE user_id = $1")
            .bind("TEST-PTOK-D1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(left, 0);

    // Off: the management routes are a 404.
    let raw = seed_user(&pool, "TEST-PTOK-D1").await;
    let off = harness(pool.clone(), PersonalTokenSettings::default());
    let (status, _, body) = send(
        &off.main,
        "GET",
        "/public/account/personal-tokens",
        &[("cookie", &cookie(&raw))],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "feature_disabled");

    cleanup(&pool, "TEST-PTOK-D").await;
}

/// A `MakeWriter` that keeps everything written to it.
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// No log line, at any level, carries a token or its random part: not on
/// creation, use, a scope refusal, the wrong listener or revocation.
#[tokio::test]
#[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
            personal_tokens_db_tests -- --ignored --test-threads=1`"]
async fn tokens_never_reach_the_logs() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(captured.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let pool = connect().await;
    cleanup(&pool, "TEST-PTOK-E").await;
    let h = harness(pool.clone(), settings());
    let raw = seed_user(&pool, "TEST-PTOK-E1").await;
    let (status, body) = create(&h, &raw, &["lines:read"]).await;
    assert_eq!(status, StatusCode::CREATED);
    let token = body["token"].as_str().unwrap().to_string();
    let auth = bearer(&token);
    for (router, uri) in [
        (&h.tokens, "/v1/me"),
        (&h.tokens, "/v1/Journeys/mine"),
        (&h.main, "/Journeys/mine"),
        (&h.main, "/public/account/personal-tokens"),
    ] {
        send(router, "GET", uri, &[("authorization", &auth)], None).await;
    }
    send(
        &h.main,
        "DELETE",
        "/public/account/personal-tokens",
        &[("cookie", &cookie(&raw)), ("origin", ORIGIN)],
        None,
    )
    .await;
    send(
        &h.tokens,
        "GET",
        "/v1/me",
        &[("authorization", &auth)],
        None,
    )
    .await;

    let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("personal_token_created"), "{logs}");
    assert!(logs.contains("personal_token_request"), "{logs}");
    assert!(logs.contains("wrong_listener"), "{logs}");
    assert!(!logs.contains(&token), "a token was logged");
    assert!(
        !logs.contains(&token[4..36]),
        "a token's random part was logged"
    );
    cleanup(&pool, "TEST-PTOK-E").await;
}

/// The banner every page shows (spec §15 decision 3): the newest token
/// created in the last 7 days, only while the feature is on, and gone
/// once it is revoked.
#[tokio::test]
#[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
            personal_tokens_db_tests -- --ignored --test-threads=1`"]
async fn the_notice_names_a_new_token_for_seven_days() {
    let pool = connect().await;
    cleanup(&pool, "TEST-PTOK-F").await;
    let h = harness(pool.clone(), settings());
    let raw = seed_user(&pool, "TEST-PTOK-F1").await;
    let notice = |router: axum::Router, raw: String| async move {
        let (status, _, body) = send(
            &router,
            "GET",
            "/public/account/connected-apps-notice",
            &[("cookie", &cookie(&raw))],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["tokenNotice"].clone()
    };
    assert_eq!(notice(h.main.clone(), raw.clone()).await, Value::Null);
    let (_, body) = create(&h, &raw, &["lines:read"]).await;
    let id = body["id"].as_i64().unwrap();
    let shown = notice(h.main.clone(), raw.clone()).await;
    assert_eq!(shown["name"], "Kitchen display");
    assert!(shown.get("token").is_none());
    // Off: no token notice even with a new token.
    let off = harness(pool.clone(), PersonalTokenSettings::default());
    assert_eq!(notice(off.main.clone(), raw.clone()).await, Value::Null);
    // Older than 7 days, or revoked: none.
    sqlx::query(
        "UPDATE personal_api_tokens SET created_at = now() - INTERVAL '8 days' WHERE id = $1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(notice(h.main.clone(), raw.clone()).await, Value::Null);
    let (_, body) = create(&h, &raw, &["lines:read"]).await;
    crate::data::personal_tokens::revoke(
        &pool,
        body["id"].as_i64().unwrap(),
        None,
        crate::data::personal_tokens::RevokeReason::User,
    )
    .await
    .unwrap();
    assert_eq!(notice(h.main.clone(), raw.clone()).await, Value::Null);
    cleanup(&pool, "TEST-PTOK-F").await;
}
