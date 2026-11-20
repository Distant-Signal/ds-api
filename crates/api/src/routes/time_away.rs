//! `/public/preferences/time-away`: the user's time away
//! (`data::time_away`), which every regular journey of theirs passes over
//! (reason `away`). Session only; not offered to delegated clients.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::app::{App, Router};
use crate::auth::AuthenticatedUser;
use crate::data::regular_journeys::london_date;
use crate::data::time_away::{self, SaveOutcome, TimeAwayPeriod};
use crate::routes::journey_ref::AwayId;

pub fn router() -> Router {
    Router::new()
        .route(
            "/preferences/time-away",
            axum::routing::get(get_time_away).post(post_time_away),
        )
        .route(
            "/preferences/time-away/{period_id}",
            axum::routing::put(put_time_away).delete(delete_time_away),
        )
}

/// `POST`/`PUT /public/preferences/time-away`'s body.
#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(optional_fields = nullable)]
pub(crate) struct TimeAwayRequest {
    /// The first date away (a London date).
    starts_on: NaiveDate,
    /// The last date away, inclusive.
    ends_on: NaiveDate,
    /// The user's own name for it, up to 60 characters.
    #[serde(default)]
    label: Option<String>,
}

/// `POST`/`PUT /public/preferences/time-away`'s response.
#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TimeAwayResponse {
    period: TimeAwayPeriod,
    /// Journeys already added for those dates that nobody had touched, now
    /// deleted.
    deleted_journey_ids: Vec<i64>,
    // `deleted_journey_ids.len()` (DS-534, additive). The ids are served
    // only until a frontend that reads this count is deployed, then a later
    // change drops them: nothing can use a deleted journey's id.
    deleted_journey_count: usize,
}

fn internal_error(err: &anyhow::Error) -> (StatusCode, String) {
    if let Some(unavailable) = crate::unavailable::response_for(err) {
        return unavailable;
    }
    tracing::error!(error = ?err, "time away operation failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "operation failed".to_string(),
    )
}

fn validated(body: &TimeAwayRequest) -> Result<time_away::PeriodInput, (StatusCode, String)> {
    let today = london_date(super::london_now().to_utc());
    time_away::validate(body.starts_on, body.ends_on, body.label.as_deref(), today)
        .map_err(|message| (StatusCode::BAD_REQUEST, message))
}

fn saved(outcome: SaveOutcome) -> Result<Json<TimeAwayResponse>, (StatusCode, String)> {
    match outcome {
        SaveOutcome::Saved {
            period,
            deleted_journey_ids,
        } => Ok(Json(TimeAwayResponse {
            period,
            deleted_journey_count: deleted_journey_ids.len(),
            deleted_journey_ids,
        })),
        SaveOutcome::TooMany => Err((
            StatusCode::CONFLICT,
            format!(
                "You can have up to {} times away. Delete one first.",
                time_away::MAX_PERIODS_PER_USER
            ),
        )),
        SaveOutcome::NotFound => Err((
            StatusCode::NOT_FOUND,
            "no time away with that id".to_string(),
        )),
    }
}

/// `GET /public/preferences/time-away`: the caller's time away that hasn't
/// ended, soonest first.
async fn get_time_away(
    State(app): State<App>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<TimeAwayPeriod>>, (StatusCode, String)> {
    let periods = time_away::list(&app.database, &user.id, super::london_now().to_utc())
        .await
        .map_err(|err| internal_error(&err))?;
    Ok(Json(periods))
}

/// `POST /public/preferences/time-away` `{startsOn, endsOn, label?}`: adds
/// time away, and deletes the untouched journeys already added inside it.
/// `400` for dates that are backwards, over a year long, over or more than
/// a year ahead; `409` at the cap.
async fn post_time_away(
    State(app): State<App>,
    user: AuthenticatedUser,
    Json(body): Json<TimeAwayRequest>,
) -> Result<Json<TimeAwayResponse>, (StatusCode, String)> {
    let input = validated(&body)?;
    let outcome = time_away::create(
        &app.database,
        &user.id,
        &input,
        super::london_now().to_utc(),
    )
    .await
    .map_err(|err| internal_error(&err))?;
    saved(outcome)
}

/// `PUT /public/preferences/time-away/{id}`: replaces one, as `POST` adds.
async fn put_time_away(
    State(app): State<App>,
    user: AuthenticatedUser,
    AwayId(period_id): AwayId,
    Json(body): Json<TimeAwayRequest>,
) -> Result<Json<TimeAwayResponse>, (StatusCode, String)> {
    let input = validated(&body)?;
    let outcome = time_away::update(
        &app.database,
        &user.id,
        period_id,
        &input,
        super::london_now().to_utc(),
    )
    .await
    .map_err(|err| internal_error(&err))?;
    saved(outcome)
}

/// `DELETE /public/preferences/time-away/{id}`: `204`, or `404` when it
/// isn't the caller's. Journeys it held back come back on the next mint.
async fn delete_time_away(
    State(app): State<App>,
    user: AuthenticatedUser,
    AwayId(period_id): AwayId,
) -> Result<StatusCode, (StatusCode, String)> {
    if time_away::delete(&app.database, &user.id, period_id)
        .await
        .map_err(|err| internal_error(&err))?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((
            StatusCode::NOT_FOUND,
            "no time away with that id".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `deletedJourneyCount` is the length of `deletedJourneyIds`, which is
    /// still served until a frontend that reads the count is deployed.
    #[test]
    fn the_response_counts_the_journeys_it_deleted() {
        for deleted in [vec![], vec![4, 5]] {
            let Json(response) = saved(SaveOutcome::Saved {
                period: TimeAwayPeriod {
                    id: 1,
                    public_id: "a_0123456789abcdef0123456789abcdef".into(),
                    starts_on: NaiveDate::from_ymd_opt(2026, 12, 1).unwrap(),
                    ends_on: NaiveDate::from_ymd_opt(2026, 12, 3).unwrap(),
                    label: None,
                },
                deleted_journey_ids: deleted.clone(),
            })
            .unwrap();
            let value = serde_json::to_value(response).unwrap();
            assert_eq!(value["deletedJourneyIds"], serde_json::json!(deleted));
            assert_eq!(value["deletedJourneyCount"], deleted.len());
        }
    }
}
