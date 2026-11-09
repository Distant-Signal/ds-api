//! `/public/account/webhooks`: a user's outbound webhook destinations
//! (DS-415), managed from the notification settings.
//!
//! Every route takes `AuthenticatedUser`, the session cookie, and nothing
//! else (no personal token or delegated grant reaches them); the mutations
//! also need a same-origin `Origin` or `Referer`. With `WEBHOOKS_ENABLED`
//! off every route answers `404 feature_disabled`.
//!
//! * `GET`: the presets, the cap and the user's destinations.
//! * `POST {kind, name, url, secret?}`: a new destination. A `generic` one
//!   gets a generated signing secret, in this response only, with
//!   `Cache-Control: no-store`. `ntfy` takes an optional access token and
//!   `gotify` its app token (`secret`); neither is shown again.
//! * `PATCH /{id} {enabled}`: turn one on (a fresh start after the
//!   notifier turned it off) or off.
//! * `DELETE /{id}`: delete one. `204`, idempotent.
//!
//! The URL is checked here once (https, public address) and again by the
//! notifier at connect time, which is what stops DNS rebinding. Errors are
//! JSON `{error, message}`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::app::{App, Router};
use crate::auth::AuthenticatedUser;
use crate::auth::delegated::error_response;
use crate::data::webhooks::{
    self as data, CreateOutcome, Kind, MAX_NAME_CHARS, MAX_PER_USER, MAX_SECRET_CHARS,
    MAX_URL_CHARS, NewWebhook, WebhookRow,
};
use crate::routes::personal_tokens::{cross_site, same_origin};

pub fn router() -> Router {
    Router::new()
        .route(
            "/account/webhooks",
            axum::routing::get(list_webhooks).post(create_webhook),
        )
        .route(
            "/account/webhooks/{id}",
            axum::routing::patch(update_webhook).delete(delete_webhook),
        )
}

/// A preset the page can offer.
#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebhookKindView {
    /// `generic`, `ntfy` or `gotify`.
    kind: String,
    /// Whether a secret is asked for: `none` (a generated signing secret
    /// is shown once), `optional` or `required`.
    secret: String,
}

/// `GET /public/account/webhooks`.
#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebhooksResponse {
    kinds: Vec<WebhookKindView>,
    max_destinations: i64,
    destinations: Vec<WebhookRow>,
}

#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreateWebhookRequest {
    kind: String,
    name: String,
    url: String,
    #[serde(default)]
    secret: Option<String>,
}

#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UpdateWebhookRequest {
    enabled: bool,
}

/// `POST /public/account/webhooks`: the only response that carries a
/// generated signing secret.
#[derive(Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreatedWebhookResponse {
    /// The generated signing secret of a `generic` destination, shown
    /// once. Absent for the other presets.
    secret: Option<String>,
    destination: WebhookRow,
}

/// Never prints the secret.
impl std::fmt::Debug for CreatedWebhookResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedWebhookResponse")
            .field("destination", &self.destination)
            .finish_non_exhaustive()
    }
}

fn disabled() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "feature_disabled",
        "webhooks are not available",
    )
}

fn database_error(operation: &'static str) -> impl Fn(anyhow::Error) -> Response {
    move |err| {
        if let Some(unavailable) = crate::unavailable::response_for(&err) {
            return unavailable.into_response();
        }
        tracing::error!(error = ?err, operation, "webhook request failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("failed to {operation}"),
        )
    }
}

fn invalid(message: &str) -> Response {
    error_response(StatusCode::BAD_REQUEST, "invalid_request", message)
}

/// A checked create request.
#[derive(Debug, PartialEq, Eq)]
struct Valid {
    kind: Kind,
    name: String,
    url: String,
    /// The secret the user gave (`ntfy` token, `gotify` app token).
    secret: Option<String>,
}

/// Checks the request without any I/O: the name, the preset's secret rules
/// and the URL's shape. The address check (DNS) is separate and async.
fn validate(request: &CreateWebhookRequest) -> Result<Valid, &'static str> {
    let kind = Kind::parse(&request.kind).ok_or("unknown destination type")?;
    let name = request.name.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err("name must be 1 to 60 characters");
    }
    if name.chars().any(char::is_control) {
        return Err("name must not contain control characters");
    }
    let url = request.url.trim().to_string();
    if url.is_empty() || url.len() > MAX_URL_CHARS {
        return Err("address must be 1 to 2048 characters");
    }
    let parsed = openidconnect::url::Url::parse(&url).map_err(|_| "address must be a valid URL")?;
    if parsed.scheme() != "https" {
        return Err("address must start with https://");
    }
    if parsed.host().is_none() {
        return Err("address must have a host");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("address must not contain a username or password");
    }
    if parsed.fragment().is_some() {
        return Err("address must not contain a #fragment");
    }
    let secret = request
        .secret
        .as_deref()
        .map(str::trim)
        .filter(|secret| !secret.is_empty())
        .map(ToString::to_string);
    if let Some(secret) = &secret
        && (secret.chars().count() > MAX_SECRET_CHARS || secret.chars().any(char::is_control))
    {
        return Err("secret must be at most 512 characters, with no control characters");
    }
    match (kind, &secret) {
        (Kind::Generic, Some(_)) => {
            return Err("a generic destination gets a generated signing secret");
        }
        (Kind::Gotify, None) => return Err("a Gotify destination needs its app token"),
        _ => {}
    }
    Ok(Valid {
        kind,
        name,
        url,
        secret,
    })
}

async fn list_webhooks(State(app): State<App>, user: AuthenticatedUser) -> Response {
    if !data::enabled() {
        return disabled();
    }
    let destinations = match data::list(&app.database, &user.id).await {
        Ok(destinations) => destinations,
        Err(err) => return database_error("list webhooks")(err),
    };
    let kind = |kind: Kind, secret: &str| WebhookKindView {
        kind: kind.as_str().to_string(),
        secret: secret.to_string(),
    };
    let mut response = Json(WebhooksResponse {
        kinds: vec![
            kind(Kind::Generic, "none"),
            kind(Kind::Ntfy, "optional"),
            kind(Kind::Gotify, "required"),
        ],
        max_destinations: MAX_PER_USER,
        destinations,
    })
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn create_webhook(
    State(app): State<App>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    body: Result<Json<CreateWebhookRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if !data::enabled() {
        return disabled();
    }
    if !same_origin(&app, &headers) {
        return cross_site();
    }
    let Ok(Json(request)) = body else {
        return invalid("send {kind, name, url, secret?}");
    };
    let valid = match validate(&request) {
        Ok(valid) => valid,
        Err(message) => return invalid(message),
    };
    if ds_runtime::outbound_endpoint_guard::validate_outbound_url(&valid.url)
        .await
        .is_err()
    {
        return invalid("address must be a public https address that resolves");
    }
    let generated = (valid.kind == Kind::Generic).then(data::generate_secret);
    let plain = generated.as_deref().or(valid.secret.as_deref());
    // Encrypted before it is stored; never stored or logged as given.
    let sealed = match plain.map(data::seal) {
        None => None,
        Some(Some(sealed)) => Some(sealed),
        Some(None) => return database_error("encrypt the secret")(anyhow::anyhow!("no key")),
    };
    let secret = sealed.as_ref();
    match data::create(
        &app.database,
        &NewWebhook {
            user_id: &user.id,
            kind: valid.kind,
            name: &valid.name,
            url: &valid.url,
            secret,
        },
    )
    .await
    {
        Ok(CreateOutcome::Created(destination)) => {
            tracing::info!(
                audit = true,
                event = "webhook_created",
                user_id = %user.id,
                webhook_id = destination.id,
                kind = valid.kind.as_str(),
                "a webhook destination was created"
            );
            let mut response = (
                StatusCode::CREATED,
                Json(CreatedWebhookResponse {
                    secret: generated,
                    destination,
                }),
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Ok(CreateOutcome::TooMany) => error_response(
            StatusCode::CONFLICT,
            "webhook_limit",
            &format!("you can have at most {MAX_PER_USER} destinations; delete one first"),
        ),
        Ok(CreateOutcome::Duplicate) => error_response(
            StatusCode::CONFLICT,
            "webhook_exists",
            "you already have a destination at that address",
        ),
        Ok(CreateOutcome::NoUser) => error_response(
            StatusCode::UNAUTHORIZED,
            "not_authenticated",
            "log in again",
        ),
        Err(err) => database_error("create the webhook")(err),
    }
}

async fn update_webhook(
    State(app): State<App>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
    body: Result<Json<UpdateWebhookRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if !data::enabled() {
        return disabled();
    }
    if !same_origin(&app, &headers) {
        return cross_site();
    }
    let Ok(Json(request)) = body else {
        return invalid("send {enabled}");
    };
    match data::set_enabled(&app.database, id, &user.id, request.enabled).await {
        Ok(Some(row)) => Json(row).into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND, "not_found", "no such destination"),
        Err(err) => database_error("update the webhook")(err),
    }
}

async fn delete_webhook(
    State(app): State<App>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> Response {
    if !data::enabled() {
        return disabled();
    }
    if !same_origin(&app, &headers) {
        return cross_site();
    }
    match data::delete(&app.database, id, &user.id).await {
        Ok(deleted) => {
            if deleted {
                tracing::info!(
                    audit = true,
                    event = "webhook_deleted",
                    user_id = %user.id,
                    webhook_id = id,
                    "a webhook destination was deleted"
                );
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => database_error("delete the webhook")(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: &str, name: &str, url: &str, secret: Option<&str>) -> CreateWebhookRequest {
        CreateWebhookRequest {
            kind: kind.to_string(),
            name: name.to_string(),
            url: url.to_string(),
            secret: secret.map(ToString::to_string),
        }
    }

    #[test]
    fn good_requests_are_normalised() {
        let valid = validate(&request(
            "generic",
            "  Home  ",
            " https://h.example/x ",
            None,
        ))
        .unwrap();
        assert_eq!(valid.name, "Home");
        assert_eq!(valid.url, "https://h.example/x");
        assert_eq!(valid.secret, None);
        let ntfy = validate(&request(
            "ntfy",
            "Phone",
            "https://ntfy.example/t",
            Some(" tk_1 "),
        ))
        .unwrap();
        assert_eq!(ntfy.secret.as_deref(), Some("tk_1"));
        assert!(
            validate(&request(
                "ntfy",
                "Phone",
                "https://ntfy.example/t",
                Some("  ")
            ))
            .is_ok()
        );
        assert!(validate(&request("gotify", "G", "https://g.example", Some("AppTok"))).is_ok());
    }

    #[test]
    fn bad_requests_are_refused() {
        for bad in [
            request("discord", "n", "https://h.example", None),
            request("generic", "", "https://h.example", None),
            request("generic", &"x".repeat(61), "https://h.example", None),
            request("generic", "tab\there", "https://h.example", None),
            request("generic", "n", "http://h.example", None),
            request("generic", "n", "not a url", None),
            request("generic", "n", "https://user:pw@h.example", None),
            request("generic", "n", "https://h.example/#frag", None),
            request(
                "generic",
                "n",
                &format!("https://h.example/{}", "x".repeat(2100)),
                None,
            ),
            request("generic", "n", "https://h.example", Some("mine")),
            request("gotify", "n", "https://g.example", None),
            request("ntfy", "n", "https://n.example/t", Some(&"x".repeat(513))),
        ] {
            assert!(validate(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_create_response_never_prints_its_secret() {
        let rendered = format!(
            "{:?}",
            CreatedWebhookResponse {
                secret: Some("whsec_leak".into()),
                destination: WebhookRow {
                    id: 1,
                    kind: "generic".into(),
                    name: "n".into(),
                    url: "https://h.example".into(),
                    has_secret: true,
                    enabled: true,
                    disabled_reason: None,
                    last_success_at: None,
                    last_failure_at: None,
                    last_error: None,
                    created_at: chrono::Utc::now(),
                },
            }
        );
        assert!(!rendered.contains("whsec_leak"));
    }
}
