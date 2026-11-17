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
//!   a per-field check that the next new field would forget. It scans the
//!   body as the handler reads it and buffers nothing, so a route's own
//!   body limit and the checks that run before the body is read (session,
//!   rate limit) are not widened.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hyper::body::Body as HttpBody;

/// What a refused request is told. No field names: the frontend shows it as
/// is.
pub const NUL_MESSAGE: &str = "That text contains a character that can't be used.";

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

/// A streaming search for a NUL in JSON text: a raw NUL byte, or a `\u0000`
/// escape that is not itself escaped (`\\u0000` is a backslash and the text
/// `u0000`). It keeps its place between chunks, so a body split anywhere is
/// judged the same as the whole.
#[derive(Debug, Default)]
struct NulScan {
    /// The backslashes just seen are an odd run: the next byte is escaped.
    odd_run: bool,
    /// How much of `u0000` follows a live escape backslash.
    matched: usize,
}

impl NulScan {
    const ESCAPE_TAIL: &'static [u8] = b"u0000";

    /// Feeds `chunk`; `true` once a NUL has been seen.
    fn feed(&mut self, chunk: &[u8]) -> bool {
        for &byte in chunk {
            if byte == 0 {
                return true;
            }
            if byte == b'\\' {
                self.odd_run = !self.odd_run;
                self.matched = 0;
            } else if self.odd_run && byte == Self::ESCAPE_TAIL[0] {
                self.odd_run = false;
                self.matched = 1;
            } else if self.matched > 0 && byte == Self::ESCAPE_TAIL[self.matched] {
                self.matched += 1;
                if self.matched == Self::ESCAPE_TAIL.len() {
                    return true;
                }
            } else {
                self.odd_run = false;
                self.matched = 0;
            }
        }
        false
    }
}

/// Whether the JSON text `body` holds a string with a NUL in it: a raw NUL
/// byte, or a `\u0000` escape that is not itself escaped (`\\u0000` is a
/// backslash and the text `u0000`).
#[must_use]
pub fn json_has_nul(body: &[u8]) -> bool {
    NulScan::default().feed(body)
}

fn refusal() -> Response {
    (StatusCode::BAD_REQUEST, NUL_MESSAGE).into_response()
}

/// A request body that watches the bytes go by, chunk by chunk, as the
/// handler's extractor reads them. Nothing is buffered here, so the route's
/// own limit (a `DefaultBodyLimit`, the 2 MiB default) still decides how much
/// is read, and a caller refused before the body is read (no session, a rate
/// limit) costs nothing. On a NUL it sets `found` and fails the read.
struct ScanBody {
    inner: Body,
    scan: NulScan,
    found: Arc<AtomicBool>,
}

impl HttpBody for ScanBody {
    type Data = axum::body::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled
            && let Some(data) = frame.data_ref()
            && this.scan.feed(data)
        {
            this.found.store(true, Ordering::Relaxed);
            return Poll::Ready(Some(Err(axum::Error::new("text with a NUL in it"))));
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// Whether a body with this `Content-Type` is text the handlers parse as
/// JSON: none at all (the `Bytes` handlers parse whatever arrives), JSON, or
/// plain text. Multipart uploads, forms and binary bodies pass untouched
/// (a ticket's bytes may hold a NUL).
fn is_scanned_content_type(value: Option<&header::HeaderValue>) -> bool {
    let Some(value) = value else { return true };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let value = value.trim().to_ascii_lowercase();
    value.contains("json") || value.starts_with("text/")
}

/// The router-wide layer: see the module docs.
pub async fn reject_nul_input(request: Request, next: Next) -> Response {
    if request
        .uri()
        .path_and_query()
        .is_some_and(|target| target.as_str().contains("%00"))
    {
        return refusal();
    }
    if matches!(request.method().as_str(), "GET" | "HEAD" | "OPTIONS")
        || !is_scanned_content_type(request.headers().get(header::CONTENT_TYPE))
    {
        return next.run(request).await;
    }
    let found = Arc::new(AtomicBool::new(false));
    let (parts, body) = request.into_parts();
    let body = Body::new(ScanBody {
        inner: body,
        scan: NulScan::default(),
        found: Arc::clone(&found),
    });
    let response = next.run(Request::from_parts(parts, body)).await;
    // Whatever the handler made of the failed read, the answer is this one.
    if found.load(Ordering::Relaxed) {
        return refusal();
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::DefaultBodyLimit;
    use axum::routing::post;
    use tower::ServiceExt;

    #[test]
    fn a_nul_is_found_only_when_it_is_really_one() {
        assert!(json_has_nul(br#"{"name":"a\u0000b"}"#));
        assert!(json_has_nul(br#"{"name":"a\\\u0000b"}"#));
        assert!(json_has_nul(b"{\"name\":\"a\0b\"}"));
        // A backslash and the text u0000, an ordinary string.
        assert!(!json_has_nul(br#"{"name":"a\\u0000b"}"#));
        assert!(!json_has_nul(br#"{"name":"a\\\\u0000b"}"#));
        assert!(!json_has_nul(br#"{"name":"a\u0001b \u000e \u0010"}"#));
        assert!(!json_has_nul(br#"{"name":"plain"}"#));
        assert!(!json_has_nul(b"u0000"));
        assert!(!json_has_nul(b"\\"));
    }

    #[test]
    fn a_body_split_anywhere_is_judged_like_the_whole() {
        for body in [
            br#"{"name":"a\u0000b"}"#.as_slice(),
            br#"{"name":"a\\\u0000b"}"#.as_slice(),
            br#"{"name":"a\\u0000b"}"#.as_slice(),
            br#"{"name":"a\\\\u0000b"}"#.as_slice(),
            br#"{"name":"a\u0001b"}"#.as_slice(),
        ] {
            let whole = json_has_nul(body);
            for at in 0..=body.len() {
                let mut scan = NulScan::default();
                let found = scan.feed(&body[..at]) || scan.feed(&body[at..]);
                assert_eq!(
                    found,
                    whole,
                    "{:?} split at {at}",
                    String::from_utf8_lossy(body)
                );
            }
        }
    }

    #[test]
    fn the_field_helper_names_no_field() {
        assert_eq!(reject_nul("fine"), Ok(()));
        let message = reject_nul("a\0b").unwrap_err();
        assert_eq!(message, NUL_MESSAGE);
        assert!(!message.to_ascii_lowercase().contains("nul"));
    }

    async fn status_at(
        route_limit: Option<usize>,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        body: &str,
    ) -> StatusCode {
        // `Bytes` as the handlers that parse their own JSON take it, and
        // `Json` as the rest do.
        let mut route = post(|_body: axum::body::Bytes| async { StatusCode::NO_CONTENT });
        if let Some(limit) = route_limit {
            route = route.layer(DefaultBodyLimit::max(limit));
        }
        let router = Router::new()
            .route("/x", route)
            .route(
                "/json",
                post(|_body: axum::Json<serde_json::Value>| async { StatusCode::NO_CONTENT }),
            )
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

    async fn status(method: &str, uri: &str, content_type: Option<&str>, body: &str) -> StatusCode {
        status_at(None, method, uri, content_type, body).await
    }

    #[tokio::test]
    async fn the_layer_refuses_a_nul_in_a_json_body_or_the_uri_only() {
        let json = Some("application/json");
        assert_eq!(
            status("POST", "/x", json, r#"{"name":"a\u0000b"}"#).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status("POST", "/json", json, r#"{"name":"a\u0000b"}"#).await,
            StatusCode::BAD_REQUEST
        );
        // No Content-Type, or plain text, to a handler that parses it itself.
        assert_eq!(
            status("POST", "/x", None, r#"{"name":"a\u0000b"}"#).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status("POST", "/x", Some("text/plain"), r#"{"name":"a\u0000b"}"#).await,
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
        // A literal `%2500` in the URI is not a `%00`.
        assert_eq!(
            status("POST", "/x?q=a%2500b", json, "{}").await,
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

    /// The layer buffers nothing, so a route's own smaller body limit
    /// still decides.
    #[tokio::test]
    async fn a_route_body_limit_still_applies_inside_the_layer() {
        let json = Some("application/json");
        let big = format!(r#"{{"name":"{}"}}"#, "a".repeat(64));
        assert_eq!(
            status_at(Some(16), "POST", "/x", json, &big).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            status_at(Some(1024), "POST", "/x", json, &big).await,
            StatusCode::NO_CONTENT
        );
    }
}
