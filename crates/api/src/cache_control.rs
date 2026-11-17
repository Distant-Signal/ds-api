//! A default `Cache-Control: private, no-store` for personal responses.
//!
//! A response to a request that carries credentials (a session cookie or an
//! `Authorization` header) is about that caller, and a shared cache must not
//! keep it. So every such response that sets no `Cache-Control` of its own
//! gets `private, no-store`, as does any response that sets a cookie.
//! A handler's explicit `Cache-Control` always wins, so the public reference
//! routes keep their `public, max-age=...`, and a credential-less request to
//! a public route is untouched.

use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::auth::{SESSION_COOKIE_NAME, parse_cookie};

/// The layer: `axum::middleware::from_fn(cache_control::default_private)`.
pub async fn default_private(request: Request, next: Next) -> Response {
    let personal = request.headers().contains_key(header::AUTHORIZATION)
        || parse_cookie(request.headers(), SESSION_COOKIE_NAME).is_some();
    let mut response = next.run(request).await;
    if (personal || response.headers().contains_key(header::SET_COOKIE))
        && !response.headers().contains_key(header::CACHE_CONTROL)
    {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    fn router() -> Router {
        Router::new()
            .route("/auth/session", get(|| async { "{}" }))
            .route("/Journeys", get(|| async { "[]" }))
            .route(
                "/public/reference/stations",
                get(|| async { ([(header::CACHE_CONTROL, "public, max-age=3600")], "[]") }),
            )
            .route("/public/health", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(default_private))
    }

    async fn cache_control(uri: &str, cookie: Option<&str>, bearer: bool) -> Option<String> {
        let mut request = Request::builder().uri(uri);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if bearer {
            request = request.header(header::AUTHORIZATION, "Bearer x");
        }
        let response = router()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        response
            .headers()
            .get(header::CACHE_CONTROL)
            .map(|v| v.to_str().unwrap().to_string())
    }

    #[tokio::test]
    async fn a_session_response_is_private_no_store() {
        let cookie = format!("{SESSION_COOKIE_NAME}=abc");
        assert_eq!(
            cache_control("/auth/session", Some(&cookie), false)
                .await
                .as_deref(),
            Some("private, no-store")
        );
    }

    #[tokio::test]
    async fn a_journeys_response_is_private_no_store() {
        let cookie = format!("{SESSION_COOKIE_NAME}=abc");
        assert_eq!(
            cache_control("/Journeys", Some(&cookie), false)
                .await
                .as_deref(),
            Some("private, no-store")
        );
        assert_eq!(
            cache_control("/Journeys", None, true).await.as_deref(),
            Some("private, no-store")
        );
    }

    #[tokio::test]
    async fn a_public_route_keeps_its_own_header_and_gains_none() {
        let cookie = format!("{SESSION_COOKIE_NAME}=abc");
        // An explicit header wins, even for a signed-in browser.
        assert_eq!(
            cache_control("/public/reference/stations", Some(&cookie), false)
                .await
                .as_deref(),
            Some("public, max-age=3600")
        );
        assert_eq!(
            cache_control("/public/reference/stations", None, false)
                .await
                .as_deref(),
            Some("public, max-age=3600")
        );
        // No credentials and no header of its own: untouched.
        assert_eq!(cache_control("/public/health", None, false).await, None);
    }
}
