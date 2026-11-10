//! The journey calendar (docs/calendar-feed.md), opt-in and off unless
//! `CALENDAR_FEED_ENABLED`:
//!
//! - `GET`/`POST`/`DELETE /public/account/calendar-feed`: whether the
//!   caller has a feed link, make (or replace) it, and turn it off. Session
//!   cookie only; the mutations also need a same-origin `Origin` or
//!   `Referer`, as the personal-token routes do. The `POST` response is
//!   the only place the token is ever shown (`Cache-Control: no-store`):
//!   it is stored hashed (`unlisted_links`), like a journey share link.
//! - `GET /public/calendar/{token}`: the feed, an iCalendar document of the
//!   caller's journeys (`data::calendar_feed`). Authenticated by the token
//!   alone, which is the whole credential: `404` for an unknown, revoked
//!   or switched-off token, with nothing to tell them apart. A trailing
//!   `.ics` is ignored, so `/api/calendar/{token}.ics` works for apps that
//!   want a file name. Rate limited per client IP in the `/Train/by-uid`
//!   budget, never cached by a shared cache (`Cache-Control: private`), and
//!   `Referrer-Policy: no-referrer`. Its path is logged as the route
//!   template (`main::redact_unmatched_path` covers the unmatched case).
//! - `GET /Journeys/{id}/calendar.ics`: one of the caller's own journeys as
//!   a download, cookie only; `404` for anyone else's.
//!
//! The link has no expiry: a calendar app cannot be told a link is about
//! to lapse, and one that quietly stops updating is worse than one the
//! user turns off. Making a new link revokes the old one.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, routing::get};
use chrono::Utc;
use serde::Serialize;

use crate::app::{App, Router};
use crate::auth::delegated::error_response;
use crate::auth::{self, AuthenticatedUser};
use crate::data::calendar_feed::{self, RESOURCE_TYPE};
use crate::data::unlisted_links;
use crate::ics::{self, Calendar};
use crate::routes::feeds::Site;

pub fn router() -> Router {
    Router::new().route("/calendar/{token}", get(feed)).route(
        "/account/calendar-feed",
        get(status).post(enable).delete(disable),
    )
}

pub const CONTENT_TYPE: &str = "text/calendar; charset=utf-8";

/// `GET /public/account/calendar-feed`.
#[derive(Debug, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CalendarFeedStatus {
    /// Whether the caller has a working feed link. The link itself is
    /// never shown again after it is made.
    active: bool,
}

/// `POST /public/account/calendar-feed`: the only response that carries
/// the token.
#[derive(Debug, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CalendarFeedCreated {
    /// The secret. The feed is `/api/calendar/{token}.ics` on the site.
    token: String,
}

fn disabled() -> Response {
    error_response(
        StatusCode::NOT_FOUND,
        "feature_disabled",
        "the calendar feed is not available",
    )
}

fn same_origin(app: &App, headers: &HeaderMap) -> bool {
    match auth::expected_browser_origin(&app.config.sso_redirect_url) {
        None => true,
        Some(expected) => auth::is_same_origin(
            headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()),
            headers.get(header::REFERER).and_then(|v| v.to_str().ok()),
            &expected,
        ),
    }
}

fn database_error(operation: &'static str) -> impl Fn(anyhow::Error) -> Response {
    move |err| {
        if let Some(unavailable) = crate::unavailable::response_for(&err) {
            return unavailable.into_response();
        }
        tracing::error!(error = ?err, operation, "calendar feed query failed");
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "something went wrong",
        )
    }
}

async fn status(State(app): State<App>, user: AuthenticatedUser) -> Response {
    if !calendar_feed::enabled() {
        return disabled();
    }
    match unlisted_links::get_active_link(&app.database, RESOURCE_TYPE, &user.id).await {
        Ok(link) => no_store(Json(CalendarFeedStatus {
            active: link.is_some(),
        })),
        Err(err) => database_error("read the link")(err),
    }
}

async fn enable(State(app): State<App>, user: AuthenticatedUser, headers: HeaderMap) -> Response {
    if !calendar_feed::enabled() {
        return disabled();
    }
    if !same_origin(&app, &headers) {
        return error_response(
            StatusCode::FORBIDDEN,
            "cross_site",
            "cross-site request rejected",
        );
    }
    match unlisted_links::rotate_link(&app.database, RESOURCE_TYPE, &user.id, &user.id, None).await
    {
        Ok(link) => no_store(Json(CalendarFeedCreated { token: link.token })),
        Err(err) => database_error("make the link")(err),
    }
}

async fn disable(State(app): State<App>, user: AuthenticatedUser, headers: HeaderMap) -> Response {
    if !calendar_feed::enabled() {
        return disabled();
    }
    if !same_origin(&app, &headers) {
        return error_response(
            StatusCode::FORBIDDEN,
            "cross_site",
            "cross-site request rejected",
        );
    }
    match unlisted_links::revoke_link(&app.database, RESOURCE_TYPE, &user.id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => database_error("revoke the link")(err),
    }
}

fn no_store(response: impl IntoResponse) -> Response {
    let mut response = response.into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn calendar_response(body: String, extra: &[(header::HeaderName, String)]) -> Response {
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(CONTENT_TYPE));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=3600"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::HeaderName::from_static("x-robots-tag"),
        HeaderValue::from_static("noindex"),
    );
    for (name, value) in extra {
        if let Ok(value) = HeaderValue::from_str(value) {
            headers.insert(name.clone(), value);
        }
    }
    response
}

fn document(site: &Site, events: Vec<ics::Event>, name: &str) -> String {
    ics::render(&Calendar {
        name: name.to_string(),
        description: "Your planned journeys from Distant Signal (unofficial). Calendars \
                      refresh slowly, so this is a plan, not live train status."
            .to_string(),
        generated: Utc::now(),
        prod_id: format!("-//Distant Signal//{}//EN", site.host),
        events,
    })
}

/// `GET /public/calendar/{token}`.
async fn feed(State(app): State<App>, Path(token): Path<String>) -> Response {
    if !calendar_feed::enabled() {
        return disabled();
    }
    let not_found = || (StatusCode::NOT_FOUND, "no such calendar").into_response();
    let token = token.strip_suffix(".ics").unwrap_or(&token);
    let link = match unlisted_links::resolve_link(&app.database, token).await {
        Ok(Some(link)) if link.resource_type == RESOURCE_TYPE => link,
        Ok(_) => return not_found(),
        Err(err) => return database_error("resolve the link")(err),
    };
    let Some(site) = Site::from_redirect_url(&app.config.sso_redirect_url) else {
        tracing::error!("SSO_REDIRECT_URL has no usable origin; the calendar is unavailable");
        return (StatusCode::INTERNAL_SERVER_ERROR, "calendar unavailable").into_response();
    };
    match calendar_feed::feed_events(&app.database, &link.resource_id, &site, Utc::now()).await {
        Ok(events) => calendar_response(document(&site, events, "Distant Signal journeys"), &[]),
        Err(err) => database_error("read the journeys")(err),
    }
}

/// `GET /Journeys/{journeyId}/calendar.ics`.
pub(crate) async fn journey_calendar(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path(journey_id): Path<i64>,
) -> Response {
    if !calendar_feed::enabled() {
        return disabled();
    }
    let Some(site) = Site::from_redirect_url(&app.config.sso_redirect_url) else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "calendar unavailable").into_response();
    };
    let now = Utc::now();
    let rows = match crate::data::journeys::list_journeys_for_user(
        &app.database,
        &user.id,
        crate::data::journeys::ListInclude::OWN,
    )
    .await
    {
        Ok(rows) => rows,
        Err(err) => return database_error("read the journeys")(err),
    };
    let Some(row) = rows.iter().find(|row| row.id == journey_id) else {
        return (StatusCode::NOT_FOUND, "no journey with that id").into_response();
    };
    // A single journey is a deliberate download, so it is listed whatever
    // its age; its claim reminder is kept while one is open.
    let events = match calendar_feed::events_for_rows(
        &app.database,
        std::slice::from_ref(row),
        &site,
        now,
        chrono::DateTime::<Utc>::MIN_UTC,
    )
    .await
    {
        Ok(events) => events,
        Err(err) => return database_error("read the operator")(err),
    };
    let disposition = format!("attachment; filename=\"journey-{journey_id}.ics\"");
    no_store(calendar_response(
        document(&site, events, "Distant Signal journey"),
        &[(header::CONTENT_DISPOSITION, disposition)],
    ))
}
