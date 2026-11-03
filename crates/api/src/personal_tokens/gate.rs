//! The token listener's middleware (docs/specs/personal-api-tokens.md
//! §3.3, §3.4, §5.2, §6). See the module doc of `crate::personal_tokens`
//! for the order they run in.

use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use super::{BucketKey, PersonalScope, PersonalTokenAuth, RouteClass, token, token_route};
use crate::app::App;
use crate::auth::delegated::{error_response, log_hash};

/// Cloudflare's client-address header, set by its edge (spec §6.2).
pub const CF_CONNECTING_IP: &str = "cf-connecting-ip";
/// The poll hint header (spec §6.3).
pub const POLL_INTERVAL_HEADER: &str = "ds-poll-interval";
/// The largest body the envelope buffers (spec §5.2).
const MAX_ENVELOPE_BODY: usize = 4 * 1024 * 1024;

/// Removes every ambient credential and client-chosen address before
/// anything else on this listener sees the request: no cookie can
/// authenticate here, and no handler can read an `X-Real-IP` a client set.
pub async fn strip_ambient_credentials(mut request: Request, next: Next) -> Response {
    let headers = request.headers_mut();
    headers.remove(header::COOKIE);
    headers.remove(crate::rate_limit::REAL_IP_HEADER);
    headers.remove("x-forwarded-for");
    headers.remove("x-forwarded-host");
    headers.remove("x-forwarded-proto");
    next.run(request).await
}

/// Why a request was refused, for the audit line and the metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejection {
    Missing,
    Malformed,
    Unknown,
    Expired,
    Inactive,
    InsufficientScope,
    RateLimitedAuth,
}

impl Rejection {
    fn as_str(self) -> &'static str {
        match self {
            Rejection::Missing => "missing",
            Rejection::Malformed => "malformed",
            Rejection::Unknown => "unknown",
            Rejection::Expired => "expired",
            Rejection::Inactive => "inactive",
            Rejection::InsufficientScope => "insufficient_scope",
            Rejection::RateLimitedAuth => "rate_limited_auth",
        }
    }
}

fn reject(
    reason: Rejection,
    token_id: Option<i64>,
    client: Option<IpAddr>,
    response: Response,
) -> Response {
    metrics::counter!(
        ds_runtime::metrics::metric_name("api_personal_token_rejected_total"),
        "reason" => reason.as_str()
    )
    .increment(1);
    tracing::warn!(
        audit = true,
        event = "personal_token_rejected",
        reason = reason.as_str(),
        token_id,
        client_hash = client.map(|ip| log_hash(&ip.to_string())),
        "personal token request rejected"
    );
    response
}

fn invalid_token() -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "invalid_token",
        "send a valid personal API token as Authorization: Bearer dsp_...",
    )
}

fn rate_limited(retry_after: Duration) -> Response {
    let secs = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
    let mut response = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "too many requests; please slow down",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
    response
}

/// The client address for the failed-authentication bucket: Cloudflare's
/// `CF-Connecting-IP` when trusted, else the socket peer.
fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>, trust_cf: bool) -> Option<IpAddr> {
    if trust_cf
        && let Some(ip) = headers
            .get(CF_CONNECTING_IP)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<IpAddr>().ok())
    {
        return Some(crate::rate_limit::normalise(ip));
    }
    peer.map(|peer| crate::rate_limit::normalise(peer.ip()))
}

/// A 64-bit digest of a user id, so a bucket key never holds it.
fn user_digest(user_id: &str) -> u64 {
    let digest = Sha256::digest(user_id.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

fn note_rate_limited(caller: &'static str) {
    metrics::counter!(
        ds_runtime::metrics::metric_name("api_rate_limited_total"),
        "class" => "personal_token",
        "caller" => caller
    )
    .increment(1);
}

/// The gate: see the module doc and spec §3.4.
#[expect(
    clippy::too_many_lines,
    reason = "one linear sequence of checks, each with its own rejection"
)]
pub async fn token_gate(State(app): State<App>, mut request: Request, next: Next) -> Response {
    let Some(matched) = request
        .extensions()
        .get::<MatchedPath>()
        .map(|path| path.as_str().to_string())
    else {
        // No such route: a plain 404, with no token work.
        return error_response(StatusCode::NOT_FOUND, "not_found", "no such route");
    };
    let Some(route) = token_route(&matched) else {
        // Mounted but not in TOKEN_ROUTES: refuse rather than serve.
        tracing::error!(route = %matched, "a token-listener route is missing from TOKEN_ROUTES");
        return error_response(StatusCode::NOT_FOUND, "not_found", "no such route");
    };
    if route.class == RouteClass::Open {
        return next.run(request).await;
    }
    let tokens = &app.personal_tokens;
    let settings = &tokens.settings;
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    let client = client_ip(request.headers(), peer, settings.trust_cf_connecting_ip);
    let failure_bucket = client.map(BucketKey::AuthFailure);
    // A client over its failure budget is refused before any lookup.
    if let Some(key) = failure_bucket
        && let Some(wait) = tokens.limiter.would_refuse(
            key,
            settings.auth_failure_per_minute,
            settings.auth_failure_burst,
            Instant::now(),
        )
    {
        note_rate_limited("personal_auth_failure");
        return reject(Rejection::RateLimitedAuth, None, client, rate_limited(wait));
    }
    // Charges the failure bucket, then refuses.
    let fail = |reason: Rejection, token_id: Option<i64>, response: Response| {
        if let Some(key) = failure_bucket {
            let _ = tokens.limiter.check_all(
                &[(
                    key,
                    settings.auth_failure_per_minute,
                    settings.auth_failure_burst,
                )],
                Instant::now(),
            );
        }
        reject(reason, token_id, client, response)
    };

    let Some(bearer) = crate::auth::bearer_token(request.headers()) else {
        return fail(Rejection::Missing, None, invalid_token());
    };
    let Some(hash) = token::parse(&bearer) else {
        return fail(Rejection::Malformed, None, invalid_token());
    };
    drop(bearer);
    let row = match crate::data::personal_tokens::find_active_by_hash(&app.database, &hash).await {
        Ok(row) => row,
        Err(err) => {
            if let Some(unavailable) = crate::unavailable::response_for(&err) {
                return unavailable.into_response();
            }
            tracing::error!(error = ?err, "personal token lookup failed");
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "token lookup failed",
            );
        }
    };
    // The lookup is by the unique hash; the comparison is repeated in
    // constant time so the property does not rest on the index alone.
    let Some(row) = row.filter(|row| token::hashes_equal(&row.token_hash, &hash)) else {
        return fail(Rejection::Unknown, None, invalid_token());
    };
    let now = chrono::Utc::now();
    if row.expires_at <= now {
        return fail(Rejection::Expired, Some(row.id), invalid_token());
    }
    let last_use = row.last_used_at.unwrap_or(row.created_at);
    if now - last_use > chrono::Duration::days(settings.idle_days) {
        if let Err(err) = crate::data::personal_tokens::revoke(
            &app.database,
            row.id,
            None,
            crate::data::personal_tokens::RevokeReason::Inactive,
        )
        .await
        {
            tracing::warn!(error = ?err, token_id = row.id, "could not mark an idle token revoked");
        }
        return fail(Rejection::Inactive, Some(row.id), invalid_token());
    }
    let scopes: Vec<PersonalScope> = row
        .scopes
        .iter()
        .filter_map(|scope| PersonalScope::parse(scope))
        .collect();
    if let Some(needed) = route.scope
        && !scopes.contains(&needed)
    {
        let mut response = error_response(
            StatusCode::FORBIDDEN,
            "insufficient_scope",
            &format!("this token needs the {} scope", needed.as_str()),
        );
        if let Ok(value) = HeaderValue::from_str(&format!(
            "Bearer error=\"insufficient_scope\", scope=\"{}\"",
            needed.as_str()
        )) {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, value);
        }
        // Not a failed authentication: no charge to the failure bucket.
        return reject(Rejection::InsufficientScope, Some(row.id), client, response);
    }

    // The rate buckets (spec §6.1): all of them, or none is charged.
    let mut buckets = vec![
        (
            BucketKey::Token(row.id),
            settings.token_per_minute,
            settings.token_burst,
        ),
        (
            BucketKey::User(user_digest(&row.user_id)),
            settings.user_per_minute,
            settings.user_burst,
        ),
        (
            BucketKey::Global,
            settings.global_per_minute,
            settings.global_burst,
        ),
    ];
    if route.class == RouteClass::Train {
        buckets.push((
            BucketKey::TokenTrains(row.id),
            settings.trains_per_minute,
            settings.trains_burst,
        ));
    }
    if let Err(wait) = tokens.limiter.check_all(&buckets, Instant::now()) {
        note_rate_limited("personal_token");
        tracing::debug!(token_id = row.id, route = %matched, "personal token rate limited");
        return rate_limited(wait);
    }

    if let Err(err) = crate::data::personal_tokens::touch(&app.database, row.id).await {
        tracing::warn!(error = ?err, token_id = row.id, "could not record personal token use");
    }
    let token_id = row.id;
    let user_id = row.user_id.clone();
    let scope_list = scopes
        .iter()
        .map(|scope| scope.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    request.extensions_mut().insert(PersonalTokenAuth {
        token_id,
        user_id: row.user_id,
        name: row.name,
        scopes,
        created_at: row.created_at,
        expires_at: row.expires_at,
    });
    let method = request.method().clone();
    let response = next.run(request).await;
    let status = response.status();
    metrics::counter!(
        ds_runtime::metrics::metric_name("api_personal_token_requests_total"),
        "route" => matched.clone(),
        "status" => status.as_u16().to_string()
    )
    .increment(1);
    tracing::info!(
        audit = true,
        event = "personal_token_request",
        token_id,
        user_id = %user_id,
        method = %method,
        route = %matched,
        status = status.as_u16(),
        scopes = %scope_list,
        "personal token request"
    );
    response
}

/// The error slug for a status, for a handler's plain-text error.
fn error_slug(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "bad_request",
        StatusCode::UNAUTHORIZED => "invalid_token",
        StatusCode::FORBIDDEN => "forbidden",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
        StatusCode::TOO_MANY_REQUESTS => "rate_limited",
        StatusCode::SERVICE_UNAVAILABLE => "unavailable",
        StatusCode::REQUEST_TIMEOUT => "timeout",
        status if status.is_server_error() => "internal_error",
        _ => "error",
    }
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
}

/// The `ETag` of a body: weak, the first 16 hex characters of its SHA-256.
pub fn etag(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    let hex = digest[..8]
        .iter()
        .fold(String::with_capacity(16), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        });
    format!("W/\"{hex}\"")
}

fn if_none_match_hits(request_headers: &HeaderMap, tag: &str) -> bool {
    request_headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate == tag)
}

/// The response side (spec §5.2, §6.3): a JSON `2xx` is wrapped as
/// `{data, attribution}` with the caching headers; a non-JSON error becomes
/// `{error, message}`; a matching `If-None-Match` gets a `304`.
pub async fn envelope_and_cache(request: Request, next: Next) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .and_then(|path| token_route(path.as_str()));
    let request_headers = request.headers().clone();
    let response = next.run(request).await;
    let Some(route) = route else {
        return response;
    };
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_ENVELOPE_BODY).await else {
        tracing::error!(route = route.path, "a token response was too large to wrap");
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "the response was too large",
        );
    };
    if !parts.status.is_success() {
        if is_json(&parts.headers) {
            return Response::from_parts(parts, Body::from(bytes));
        }
        let mut response = error_response(
            parts.status,
            error_slug(parts.status),
            String::from_utf8_lossy(&bytes).trim(),
        );
        for name in [header::RETRY_AFTER, header::WWW_AUTHENTICATE] {
            if let Some(value) = parts.headers.get(&name) {
                response.headers_mut().insert(name, value.clone());
            }
        }
        return response;
    }
    if !is_json(&parts.headers) {
        return Response::from_parts(parts, Body::from(bytes));
    }
    let attribution = serde_json::json!({
        "sources": route.sources,
        "statement": super::NON_AFFILIATION,
    });
    let mut wrapped = Vec::with_capacity(bytes.len() + 512);
    wrapped.extend_from_slice(b"{\"data\":");
    wrapped.extend_from_slice(&bytes);
    wrapped.extend_from_slice(b",\"attribution\":");
    wrapped.extend_from_slice(attribution.to_string().as_bytes());
    wrapped.push(b'}');
    let tag = etag(&wrapped);
    let headers = &mut parts.headers;
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(
        header::CACHE_CONTROL,
        if route.max_age_secs == 0 {
            HeaderValue::from_static("no-store")
        } else {
            HeaderValue::from_str(&format!("private, max-age={}", route.max_age_secs))
                .unwrap_or_else(|_| HeaderValue::from_static("no-store"))
        },
    );
    headers.insert(header::VARY, HeaderValue::from_static("Authorization"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        POLL_INTERVAL_HEADER,
        HeaderValue::from(super::POLL_INTERVAL_SECS),
    );
    if let Ok(value) = HeaderValue::from_str(&tag) {
        headers.insert(header::ETAG, value);
    }
    if if_none_match_hits(&request_headers, &tag) {
        parts.status = StatusCode::NOT_MODIFIED;
        parts.headers.remove(header::CONTENT_TYPE);
        return Response::from_parts(parts, Body::empty());
    }
    Response::from_parts(parts, Body::from(wrapped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_failure_bucket_trusts_cloudflare_only_when_told_to() {
        let mut headers = HeaderMap::new();
        headers.insert(CF_CONNECTING_IP, HeaderValue::from_static("203.0.113.7"));
        let peer: SocketAddr = "10.0.0.5:4000".parse().unwrap();
        assert_eq!(
            client_ip(&headers, Some(peer), true),
            Some("203.0.113.7".parse().unwrap())
        );
        assert_eq!(
            client_ip(&headers, Some(peer), false),
            Some("10.0.0.5".parse().unwrap())
        );
        headers.insert(CF_CONNECTING_IP, HeaderValue::from_static("not an ip"));
        assert_eq!(
            client_ip(&headers, Some(peer), true),
            Some("10.0.0.5".parse().unwrap())
        );
    }

    #[test]
    fn if_none_match_accepts_a_list_and_the_wildcard() {
        let tag = etag(b"{}");
        let mut headers = HeaderMap::new();
        assert!(!if_none_match_hits(&headers, &tag));
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(&format!("W/\"other\", {tag}")).unwrap(),
        );
        assert!(if_none_match_hits(&headers, &tag));
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(if_none_match_hits(&headers, &tag));
    }

    #[test]
    fn plain_text_errors_get_a_slug() {
        assert_eq!(error_slug(StatusCode::NOT_FOUND), "not_found");
        assert_eq!(error_slug(StatusCode::BAD_GATEWAY), "internal_error");
        assert_eq!(error_slug(StatusCode::CONFLICT), "error");
    }
}
