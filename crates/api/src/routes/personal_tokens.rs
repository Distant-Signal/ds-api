//! `/public/account/personal-tokens`: a user's read-only personal API
//! tokens, managed from the Connected apps page
//! (docs/specs/personal-api-tokens.md §7.1).
//!
//! Every route takes `AuthenticatedUser`, the session cookie, and nothing
//! else; the mutations also need a same-origin `Origin` or `Referer`. The
//! token listener never mounts these, so a token cannot create or revoke
//! tokens. With `PERSONAL_TOKENS_ENABLED` off every route answers `404
//! feature_disabled`.
//!
//! * `GET`: the settings the page needs and the user's working tokens.
//! * `POST {name, scopes, lifetimeDays}`: a new token. The plaintext is in
//!   this response only, with `Cache-Control: no-store`.
//! * `DELETE /{id}`: revoke one. `DELETE`: revoke all. Both `204`,
//!   idempotent.
//!
//! Errors are JSON `{error, message}`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::app::{App, Router};
use crate::auth::delegated::{error_response, log_hash};
use crate::auth::{self, AuthenticatedUser, SESSION_COOKIE_NAME, hash_session_token, parse_cookie};
use crate::data::personal_tokens::{self as data, CreateOutcome, RevokeReason, TokenRow};
use crate::personal_tokens::{PersonalScope, token};

pub fn router() -> Router {
    Router::new()
        .route(
            "/account/personal-tokens",
            axum::routing::get(list_tokens)
                .post(create_token)
                .delete(revoke_all),
        )
        .route(
            "/account/personal-tokens/{id}",
            axum::routing::delete(revoke_one),
        )
}

/// A scope the page can offer.
#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersonalScopeView {
    /// `journeys:read`, `lines:read`, `trains:read` or `preferences:read`.
    name: String,
}

/// `GET /public/account/personal-tokens`.
#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PersonalTokensResponse {
    /// The API hostname's base URL, for the page to show (empty when the
    /// operator has not set one).
    api_base_url: String,
    scopes: Vec<PersonalScopeView>,
    /// The lifetimes a token may have, in days.
    lifetimes: Vec<i64>,
    default_lifetime_days: i64,
    max_active: i64,
    tokens: Vec<TokenRow>,
}

#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CreatePersonalTokenRequest {
    name: String,
    scopes: Vec<String>,
    lifetime_days: i64,
}

/// `POST /public/account/personal-tokens`: the only response that ever
/// carries the token itself.
#[derive(Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreatedPersonalTokenResponse {
    /// The token. Shown once; Distant Signal keeps only its hash.
    token: String,
    #[serde(flatten)]
    row: TokenRow,
}

/// Never prints the token.
impl std::fmt::Debug for CreatedPersonalTokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatedPersonalTokenResponse")
            .field("row", &self.row)
            .finish_non_exhaustive()
    }
}

/// The default lifetime the page preselects (spec §2.3).
const DEFAULT_LIFETIME_DAYS: i64 = 90;
/// The longest name (the migration's CHECK).
const MAX_NAME_CHARS: usize = 60;

fn disabled() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "feature_disabled",
        "personal API tokens are not available",
    )
}

fn database_error(operation: &'static str) -> impl Fn(anyhow::Error) -> Response {
    move |err| {
        if let Some(unavailable) = crate::unavailable::response_for(&err) {
            return unavailable.into_response();
        }
        tracing::error!(error = ?err, operation, "personal-token request failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("failed to {operation}"),
        )
    }
}

/// The strict check `logout` and the connected-apps mutations use.
pub(crate) fn same_origin(app: &App, headers: &HeaderMap) -> bool {
    match auth::expected_browser_origin(&app.config.sso_redirect_url) {
        None => true,
        Some(expected) => auth::is_same_origin(
            headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
            headers.get(header::REFERER).and_then(|v| v.to_str().ok()),
            &expected,
        ),
    }
}

pub(crate) fn cross_site() -> Response {
    error_response(
        StatusCode::FORBIDDEN,
        "cross_site",
        "cross-site request rejected",
    )
}

fn session_hash(headers: &HeaderMap) -> Option<String> {
    parse_cookie(headers, SESSION_COOKIE_NAME).map(|token| log_hash(&hash_session_token(&token)))
}

fn default_lifetime(lifetimes: &[i64]) -> i64 {
    if lifetimes.contains(&DEFAULT_LIFETIME_DAYS) {
        DEFAULT_LIFETIME_DAYS
    } else {
        lifetimes.last().copied().unwrap_or(DEFAULT_LIFETIME_DAYS)
    }
}

async fn list_tokens(State(app): State<App>, user: AuthenticatedUser) -> Response {
    let settings = &app.personal_tokens.settings;
    if !settings.enabled {
        return disabled();
    }
    let tokens = match data::list_active(&app.database, &user.id).await {
        Ok(tokens) => tokens,
        Err(err) => return database_error("list personal tokens")(err),
    };
    let lifetimes = settings.lifetimes();
    let mut response = Json(PersonalTokensResponse {
        api_base_url: settings.public_base_url.clone(),
        scopes: PersonalScope::ALL
            .into_iter()
            .map(|scope| PersonalScopeView {
                name: scope.as_str().to_string(),
            })
            .collect(),
        default_lifetime_days: default_lifetime(&lifetimes),
        lifetimes,
        max_active: settings.max_active_per_user,
        tokens,
    })
    .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The request, checked: a trimmed name, known unique scopes and an
/// offered lifetime.
fn validate(
    request: &CreatePersonalTokenRequest,
    lifetimes: &[i64],
) -> Result<(String, Vec<String>, i64), &'static str> {
    let name = request.name.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err("name must be 1 to 60 characters");
    }
    if name.chars().any(char::is_control) {
        return Err("name must not contain control characters");
    }
    let mut scopes: Vec<PersonalScope> = Vec::new();
    for scope in &request.scopes {
        let Some(parsed) = PersonalScope::parse(scope) else {
            return Err("unknown scope");
        };
        if !scopes.contains(&parsed) {
            scopes.push(parsed);
        }
    }
    if scopes.is_empty() {
        return Err("choose at least one scope");
    }
    scopes.sort();
    if !lifetimes.contains(&request.lifetime_days) {
        return Err("lifetimeDays must be one of the offered lifetimes");
    }
    Ok((
        name,
        scopes
            .into_iter()
            .map(|scope| scope.as_str().to_string())
            .collect(),
        request.lifetime_days,
    ))
}

async fn create_token(
    State(app): State<App>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    body: Result<Json<CreatePersonalTokenRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let settings = &app.personal_tokens.settings;
    if !settings.enabled {
        return disabled();
    }
    if !same_origin(&app, &headers) {
        tracing::warn!("personal-token create rejected: Origin/Referer did not match");
        return cross_site();
    }
    let Ok(Json(request)) = body else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "send {name, scopes, lifetimeDays}",
        );
    };
    let (name, scopes, lifetime_days) = match validate(&request, &settings.lifetimes()) {
        Ok(valid) => valid,
        Err(message) => {
            return error_response(StatusCode::BAD_REQUEST, "invalid_request", message);
        }
    };
    let new = token::generate();
    match data::create(
        &app.database,
        &data::NewTokenRow {
            user_id: &user.id,
            name: &name,
            hash: &new.hash,
            hint: &new.hint,
            scopes: &scopes,
            lifetime_days,
        },
        settings.max_active_per_user,
    )
    .await
    {
        Ok(CreateOutcome::Created(row)) => {
            tracing::info!(
                audit = true,
                event = "personal_token_created",
                user_id = %user.id,
                token_id = row.id,
                scopes = %scopes.join(" "),
                expires_at = %row.expires_at,
                session_hash = session_hash(&headers).as_deref(),
                "a personal API token was created"
            );
            let mut response = (
                StatusCode::CREATED,
                Json(CreatedPersonalTokenResponse {
                    token: new.token,
                    row,
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
            "token_limit",
            &format!(
                "you can have at most {} tokens; revoke one first",
                settings.max_active_per_user
            ),
        ),
        Ok(CreateOutcome::NoUser) => error_response(
            StatusCode::UNAUTHORIZED,
            "not_authenticated",
            "log in again",
        ),
        Err(err) => database_error("create the token")(err),
    }
}

fn log_revoked(user_id: &str, token_id: Option<i64>, count: u64, headers: &HeaderMap) {
    tracing::info!(
        audit = true,
        event = "personal_token_revoked",
        reason = RevokeReason::User.as_str(),
        user_id = %user_id,
        token_id,
        count,
        session_hash = session_hash(headers).as_deref(),
        "a user revoked personal API tokens"
    );
}

async fn revoke_one(
    State(app): State<App>,
    user: AuthenticatedUser,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !app.personal_tokens.settings.enabled {
        return disabled();
    }
    if !same_origin(&app, &headers) {
        return cross_site();
    }
    let Ok(id) = id.parse::<i64>() else {
        return StatusCode::NO_CONTENT.into_response();
    };
    match data::revoke(&app.database, id, Some(&user.id), RevokeReason::User).await {
        Ok(revoked) => {
            if revoked {
                log_revoked(&user.id, Some(id), 1, &headers);
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => database_error("revoke the token")(err),
    }
}

async fn revoke_all(
    State(app): State<App>,
    user: AuthenticatedUser,
    headers: HeaderMap,
) -> Response {
    if !app.personal_tokens.settings.enabled {
        return disabled();
    }
    if !same_origin(&app, &headers) {
        return cross_site();
    }
    match data::revoke_all_for_user(&app.database, &user.id, RevokeReason::User).await {
        Ok(count) => {
            if count > 0 {
                log_revoked(&user.id, None, count, &headers);
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(err) => database_error("revoke the tokens")(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &str, scopes: &[&str], lifetime_days: i64) -> CreatePersonalTokenRequest {
        CreatePersonalTokenRequest {
            name: name.to_string(),
            scopes: scopes.iter().map(ToString::to_string).collect(),
            lifetime_days,
        }
    }

    #[test]
    fn a_good_request_is_normalised() {
        let (name, scopes, days) = validate(
            &request(
                "  Kitchen display ",
                &["trains:read", "lines:read", "lines:read"],
                90,
            ),
            &[7, 30, 90, 365],
        )
        .unwrap();
        assert_eq!(name, "Kitchen display");
        assert_eq!(scopes, ["lines:read", "trains:read"]);
        assert_eq!(days, 90);
    }

    #[test]
    fn bad_requests_are_refused() {
        let lifetimes = [7, 30, 90, 365];
        for bad in [
            request("", &["lines:read"], 90),
            request(&"x".repeat(61), &["lines:read"], 90),
            request("tab\there", &["lines:read"], 90),
            request("ok", &[], 90),
            request("ok", &["journeys:write"], 90),
            request("ok", &["lines:read"], 1000),
            request("ok", &["lines:read"], 0),
        ] {
            assert!(validate(&bad, &lifetimes).is_err());
        }
        assert!(validate(&request("ok", &["lines:read"], 365), &[7, 30, 90]).is_err());
    }

    #[test]
    fn the_default_lifetime_falls_back_to_the_longest_offered() {
        assert_eq!(default_lifetime(&[7, 30, 90, 365]), 90);
        assert_eq!(default_lifetime(&[7, 30]), 30);
    }
}
