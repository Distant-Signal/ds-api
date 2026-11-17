//! The `journey_id` path parameter, in either of its two forms (DS-534).
//!
//! `JourneyId` replaces `Path<i64>` on every journey route: it reads the
//! route's `{journey_id}` capture, which is the deprecated numeric id or a
//! public `j_...` id (`data::journey_public_id`), and yields the internal
//! id. The other captures of a route (a leg, a break, a revision) are read
//! with a `Path` of one of the small structs below, which ignore the
//! captures they do not name.
//!
//! An unknown public id yields `NO_SUCH_JOURNEY`, not a rejection: the
//! handler's own ownership predicate then finds nothing and answers its
//! usual 404, so a missing journey, someone else's journey and a mistyped
//! public id look the same, and an unauthenticated request still gets the
//! handler's 401 first when its auth extractor runs first. A capture that
//! is neither form is rejected with 400, as `Path<i64>` did.

use axum::extract::{FromRequestParts, RawPathParams};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::Deserialize;

use crate::app::App;
use crate::data::journey_public_id::JourneyRef;

/// The internal id of the journey a route's `{journey_id}` capture names.
#[derive(Debug, Clone, Copy)]
pub struct JourneyId(pub i64);

impl FromRequestParts<App> for JourneyId {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        let params = RawPathParams::from_request_parts(parts, app)
            .await
            .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
        let raw = params
            .iter()
            .find(|(name, _)| *name == "journey_id")
            .map(|(_, value)| value)
            .ok_or_else(|| {
                tracing::error!("JourneyId extractor on a route with no {{journey_id}} capture");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            })?;
        let journey = JourneyRef::parse(raw).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "Invalid URL: a journey id is a number or a j_ id".to_string(),
            )
        })?;
        let id = journey.resolve(&app.database).await.map_err(|err| {
            let err = anyhow::Error::from(err);
            if let Some(unavailable) = crate::unavailable::response_for(&err) {
                return unavailable;
            }
            tracing::error!(error = ?err, "journey id lookup failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
        })?;
        Ok(Self(id))
    }
}

/// `{leg_id}` next to a `{journey_id}`.
#[derive(Debug, Deserialize)]
pub struct LegParam {
    pub leg_id: i64,
}

/// `{break_id}` next to a `{journey_id}`.
#[derive(Debug, Deserialize)]
pub struct BreakParam {
    pub break_id: i64,
}

/// `{revision_no}` next to a `{journey_id}`.
#[derive(Debug, Deserialize)]
pub struct RevisionParam {
    pub revision_no: i32,
}

/// `{id}`, a group's, next to a `{journey_id}`.
#[derive(Debug, Deserialize)]
pub struct GroupParam {
    pub id: String,
}
