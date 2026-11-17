//! Refuses NUL (U+0000) in user text before it reaches Postgres.
//!
//! A Postgres `text` column cannot hold a NUL, so a name or note carrying
//! one used to fail at the insert and answer `500` ("failed to rename
//! group", "failed to create journey (untracked leg)"). Two guards, one
//! message:
//!
//! - [`reject_nul`] for the validators of the fields that matter most
//!   (custom names, group names), so the refusal reads as that field's own
//!   `400`;
//! - [`reject_nul_input`], a router-wide layer that answers `400` for any
//!   JSON request body with a `\u0000` escape in it and any request URI
//!   with a `%00`, so every other free-text field (line names, notes,
//!   webhook labels, token names, tickets, search text) is covered without
//!   a per-field check that the next new field would forget.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// What a refused request is told. No field names: the frontend shows it as
/// is.
pub const NUL_MESSAGE: &str = "That text contains a character that can't be used.";

/// The largest body the layer buffers to look for a NUL: axum's own default
/// limit for the `Json` and `Bytes` extractors, so a body it would refuse
/// anyway is refused here the same way.
const MAX_SCANNED_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Whether `text` holds a NUL.
#[must_use]
pub fn contains_nul(text: &str) -> bool {
    text.contains('\0')
}

/// `Err` with [`NUL_MESSAGE`] when `text` holds a NUL.
pub fn reject_nul(text: &str) -> Result<(), String> {
    if contains_nul(text) {
        Err(NUL_MESSAGE.to_string())
    } else {
        Ok(())
    }
}

/// Whether the JSON text `body` holds a string with a NUL in it: a raw NUL
/// byte, or a `\u0000` escape that is not itself escaped (`\\u0000` is a
/// backslash and the text `u0000`).
#[must_use]
pub fn json_has_nul(body: &[u8]) -> bool {
    if body.contains(&0) {
        return true;
    }
    let mut i = 0;
    while i < body.len() {
        if body[i] != b'\\' {
            i += 1;
            continue;
        }
        let start = i;
        while i < body.len() && body[i] == b'\\' {
            i += 1;
        }
        // An odd run of backslashes ends in a live escape.
        if (i - start) % 2 == 1 && body[i..].starts_with(b"u0000") {
            return true;
        }
    }
    false
}

fn refusal() -> Response {
    (StatusCode::BAD_REQUEST, NUL_MESSAGE).into_response()
}

/// The router-wide layer: see the module docs. Bodies that are not JSON
/// (multipart ticket uploads, forms) pass untouched.
pub async fn reject_nul_input(request: Request, next: Next) -> Response {
    if request.uri().to_string().contains("%00") {
        return refusal();
    }
    let scan_body = request
        .headers()
        .get(header::CONTENT_TYPE)
        .is_none_or(|value| {
            value
                .to_str()
                .is_ok_and(|v| v.to_ascii_lowercase().contains("json"))
        });
    if !scan_body || matches!(request.method().as_str(), "GET" | "HEAD" | "OPTIONS") {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_SCANNED_BODY_BYTES).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    if json_has_nul(&bytes) {
        return refusal();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::post;
    use tower::ServiceExt;

    #[test]
    fn a_nul_is_found_only_when_it_is_really_one() {
        assert!(json_has_nul(br#"{"name":"a\u0000b"}"#));
        assert!(json_has_nul(br#"{"name":"a\\\u0000b"}"#));
        assert!(json_has_nul(b"{\"name\":\"a\0b\"}"));
        // A backslash and the text u0000, an ordinary string.
        assert!(!json_has_nul(br#"{"name":"a\\u0000b"}"#));
        assert!(!json_has_nul(br#"{"name":"a\u0001b \u000e"}"#));
        assert!(!json_has_nul(br#"{"name":"plain"}"#));
        assert!(!json_has_nul(b"u0000"));
        assert!(!json_has_nul(b"\\"));
    }

    #[test]
    fn the_field_helper_names_no_field() {
        assert_eq!(reject_nul("fine"), Ok(()));
        let message = reject_nul("a\0b").unwrap_err();
        assert_eq!(message, NUL_MESSAGE);
        assert!(!message.to_ascii_lowercase().contains("nul"));
    }

    async fn status(method: &str, uri: &str, content_type: Option<&str>, body: &str) -> StatusCode {
        let router = Router::new()
            .route("/x", post(|| async { StatusCode::NO_CONTENT }))
            .layer(axum::middleware::from_fn(reject_nul_input));
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        router
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn the_layer_refuses_a_nul_in_a_json_body_or_the_uri_only() {
        let json = Some("application/json");
        assert_eq!(
            status("POST", "/x", json, r#"{"name":"a\u0000b"}"#).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status("POST", "/x", None, r#"{"name":"a\u0000b"}"#).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status("POST", "/x?q=a%00b", json, "{}").await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status("POST", "/x", json, r#"{"name":"fine"}"#).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            status("POST", "/x", json, r#"{"name":"a\\u0000b"}"#).await,
            StatusCode::NO_CONTENT
        );
        // Not JSON: untouched.
        assert_eq!(
            status(
                "POST",
                "/x",
                Some("multipart/form-data; boundary=b"),
                "a\0b"
            )
            .await,
            StatusCode::NO_CONTENT
        );
    }
}
