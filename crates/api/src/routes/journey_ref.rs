//! The id path parameters, in either of their two forms (DS-534).
//!
//! `JourneyId` replaces `Path<i64>` on every journey route: it reads the
//! route's `{journey_id}` capture, which is the deprecated numeric id or a
//! public `j_...` id (`data::journey_public_id`), and yields the internal
//! id. `LegId` (`{leg_id}`, `l_`), `TrackingId` (`{tracking_id}`, and
//! `{train_subscription_id}` on the group route, `t_`) and `DeviceId`
//! (`{id}` on the device route, `d_`) do the same for the other kinds
//! (`data::public_id`), as do `BreakId` (`{break_id}`, `b_`) and
//! `TemplateId` (`{template_id}`, `r_`, a regular journey), `DeletionId`
//! (`{id}`, `x_`), `WebhookId` (`{id}`, `w_`) and `AwayId` (`{period_id}`,
//! `a_`). A personal token's `{id}` (`k_`) is parsed in its handler: an id
//! that names nothing there is a 204, not a 400. The other
//! captures of a route (a revision, a date) are read with a `Path` of one
//! of the small structs below, which ignore the captures they do not name.
//!
//! An unknown public id yields `NO_SUCH_ROW`, not a rejection: the
//! handler's own ownership predicate then finds nothing and answers its
//! usual 404, so a missing row, someone else's row and a mistyped public
//! id look the same, and an unauthenticated request still gets the
//! handler's 401 first when its auth extractor runs first. A capture that
//! is neither form (including another kind's public id) is rejected with
//! 400, as `Path<i64>` did.

use axum::extract::{FromRequestParts, RawPathParams};
use axum::http::StatusCode;
use axum::http::request::Parts;
use serde::Deserialize;

use crate::app::App;
use crate::data::public_id::{
    Away, Break, Deletion, Device, Journey, Kind, Leg, PublicRef, Template, Tracking, Webhook,
};

/// The internal id of the object of kind `K` that the first capture of
/// `names` the route has holds.
async fn resolve<K: Kind>(
    parts: &mut Parts,
    app: &App,
    names: &[&str],
) -> Result<i64, (StatusCode, String)> {
    let params = RawPathParams::from_request_parts(parts, app)
        .await
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    let raw = params
        .iter()
        .find(|(name, _)| names.contains(name))
        .map(|(_, value)| value)
        .ok_or_else(|| {
            tracing::error!(?names, "id extractor on a route with no such capture");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
        })?;
    let reference = PublicRef::<K>::parse(raw).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("Invalid URL: {}", PublicRef::<K>::expected()),
        )
    })?;
    reference.resolve(&app.database).await.map_err(|err| {
        let err = anyhow::Error::from(err);
        if let Some(unavailable) = crate::unavailable::response_for(&err) {
            return unavailable;
        }
        tracing::error!(error = ?err, kind = K::LABEL, "id lookup failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error".to_string(),
        )
    })
}

/// The internal id of the journey a route's `{journey_id}` capture names.
#[derive(Debug, Clone, Copy)]
pub struct JourneyId(pub i64);

impl FromRequestParts<App> for JourneyId {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        resolve::<Journey>(parts, app, &["journey_id"])
            .await
            .map(Self)
    }
}

/// The internal id of the journey leg a route's `{leg_id}` capture names.
#[derive(Debug, Clone, Copy)]
pub struct LegId(pub i64);

impl FromRequestParts<App> for LegId {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        resolve::<Leg>(parts, app, &["leg_id"]).await.map(Self)
    }
}

/// The internal id of the tracked train (`train_subscriptions` row) a
/// route's `{tracking_id}` capture, or the group route's
/// `{train_subscription_id}`, names.
#[derive(Debug, Clone, Copy)]
pub struct TrackingId(pub i64);

impl FromRequestParts<App> for TrackingId {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        resolve::<Tracking>(parts, app, &["tracking_id", "train_subscription_id"])
            .await
            .map(Self)
    }
}

/// The internal id of the push device a route's `{id}` capture names.
#[derive(Debug, Clone, Copy)]
pub struct DeviceId(pub i64);

impl FromRequestParts<App> for DeviceId {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        resolve::<Device>(parts, app, &["id"]).await.map(Self)
    }
}

/// The internal id of the journey break a route's `{break_id}` capture
/// names.
#[derive(Debug, Clone, Copy)]
pub struct BreakId(pub i64);

impl FromRequestParts<App> for BreakId {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        resolve::<Break>(parts, app, &["break_id"]).await.map(Self)
    }
}

/// The internal id of the regular journey (`journey_templates` row) a
/// route's `{template_id}` capture names.
#[derive(Debug, Clone, Copy)]
pub struct TemplateId(pub i64);

impl FromRequestParts<App> for TemplateId {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        resolve::<Template>(parts, app, &["template_id"])
            .await
            .map(Self)
    }
}

/// The internal id of the deleted-journey record (`delegated_deletions`
/// row, `x_`) a route's `{id}` capture names.
#[derive(Debug, Clone, Copy)]
pub struct DeletionId(pub i64);

impl FromRequestParts<App> for DeletionId {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        resolve::<Deletion>(parts, app, &["id"]).await.map(Self)
    }
}

/// The internal id of the webhook destination (`w_`) a route's `{id}`
/// capture names. Only an alias: the sealed secret stays bound to the
/// numeric id.
#[derive(Debug, Clone, Copy)]
pub struct WebhookId(pub i64);

impl FromRequestParts<App> for WebhookId {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        resolve::<Webhook>(parts, app, &["id"]).await.map(Self)
    }
}

/// The internal id of the time-away period (`a_`) a route's `{period_id}`
/// capture names.
#[derive(Debug, Clone, Copy)]
pub struct AwayId(pub i64);

impl FromRequestParts<App> for AwayId {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        resolve::<Away>(parts, app, &["period_id"]).await.map(Self)
    }
}

/// `{service_date}` next to a `{template_id}`.
#[derive(Debug, Deserialize)]
pub struct ServiceDateParam {
    pub service_date: chrono::NaiveDate,
}

/// `{revision_no}` next to a `{journey_id}`.
#[derive(Debug, Deserialize)]
pub struct RevisionParam {
    pub revision_no: i32,
}

/// `{leg_id}` as the caller wrote it, for a route that needs the opaque id
/// itself as well as what [`LegId`] resolves it to.
#[derive(Debug, Deserialize)]
pub struct LegParam {
    pub leg_id: String,
}

/// `{id}`, a group's, next to a `{journey_id}`.
#[derive(Debug, Deserialize)]
pub struct GroupParam {
    pub id: String,
}
