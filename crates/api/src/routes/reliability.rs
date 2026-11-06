//! `GET /Journeys/reliability` (2026-11-06): how the caller's own trains
//! ran and what Delay Repay their finished journeys may be owed
//! (`data::reliability`). The caller's own journeys only, never a followed
//! or group-shared one. A session, or a delegated caller with
//! `journeys:read` (`DELEGATED_ROUTES`), so a connected app can answer
//! "how reliable has my commute been".

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};

use crate::app::{App, Router};
use crate::auth::delegated::{DelegatedOrSessionUser, ReadScope};
use crate::routes::journey_itinerary::RouteError;

pub fn router() -> Router {
    // A literal segment, like `mine`: matchit resolves it ahead of
    // `/Journeys/{journey_id}`.
    Router::new().route("/Journeys/reliability", axum::routing::get(get_reliability))
}

pub(crate) async fn get_reliability(
    State(app): State<App>,
    user: DelegatedOrSessionUser<ReadScope>,
) -> Response {
    match crate::data::reliability::for_user(
        &app.database,
        &user.user_id,
        crate::routes::london_today(),
    )
    .await
    {
        Ok(response) => Json(response).into_response(),
        Err(err) => {
            let error = crate::unavailable::response_for(&err).unwrap_or_else(|| {
                tracing::error!(error = ?err, "reliability request failed");
                (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to read reliability".to_string(),
                )
            });
            RouteError::from(error).render(user.is_bearer(), "bad_request")
        }
    }
}

#[cfg(test)]
mod db_tests;
