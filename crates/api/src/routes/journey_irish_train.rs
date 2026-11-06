//! `POST /Journeys/irish-train`: "Track this train" for an Iarnród Éireann
//! train (distant-signal's
//! docs/superpowers/specs/2026-11-05-irish-rail-train-tracking-design.md
//! §5.3).
//!
//! The train must be one a departure board has shown, on Dublin's today
//! or yesterday: there is no Irish timetable, so a train we haven't seen
//! is unknown and can't be tracked. The route builds the `ie:<CODE>` uid
//! itself (`common::island_of_ireland::tracking::train_uid`) and never
//! upper-cases the prefix, so the row it makes is the row the writer
//! derives the state of. It is a route of its own, not a `POST /Journeys`
//! leg mode, so `validate_train_uid` and the `knownTrain` checks stay
//! GB-only.
//!
//! A session (with the Origin check every cookie mutation gets) or a
//! delegated caller with `journeys:write` (`DELEGATED_ROUTES`). Tracking a
//! train already tracked hands back that journey (200) rather than making
//! a second one (201).

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use common::island_of_ireland::{IslandOfIrelandNetwork, tracking};
use serde::{Deserialize, Serialize};

use crate::app::App;
use crate::auth::delegated::{DelegatedOrSessionUser, WriteScope};
use crate::data::island_of_ireland::{self, TrainHistory};
use crate::routes::journey_itinerary::RouteError;

/// `POST /Journeys/irish-train`'s body.
#[derive(Debug, Clone, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TrackIrishTrainRequest {
    /// The train code, like `A120` (trimmed and upper-cased).
    train_code: String,
    /// The day the train started, `YYYY-MM-DD`: Dublin's today or
    /// yesterday.
    service_date: NaiveDate,
    /// The live `StationCode` of the board it was tracked from, like
    /// `CNLLY`: one the train was seen at. Optional.
    #[serde(default)]
    #[ts(optional)]
    from_station: Option<String>,
}

/// `POST /Journeys/irish-train`'s response: the one-leg journey and the
/// tracked train (`GET /Train/{trackingId}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrackIrishTrainResponse {
    journey_id: i64,
    tracking_id: i64,
}

pub(crate) async fn post_irish_train(
    State(app): State<App>,
    user: DelegatedOrSessionUser<WriteScope>,
    body: Bytes,
) -> Response {
    let delegated = user.is_delegated();
    match track(&app, &user.user_id, &body).await {
        Ok(response) => response,
        Err(err) => err.render(delegated, "invalid_request"),
    }
}

/// Dublin's date now (Irish trains' service dates are Dublin's). Read
/// through `london_now`, which a test can pin: the two zones share their
/// offsets, but the date is said in the zone it means.
fn dublin_today() -> NaiveDate {
    crate::routes::london_now()
        .with_timezone(&chrono_tz::Europe::Dublin)
        .date_naive()
}

/// The request, checked: the code, a date of Dublin's today or yesterday,
/// and a `fromStation` that is a well-formed station code.
fn validate(
    body: &[u8],
    today: NaiveDate,
) -> Result<(String, NaiveDate, Option<String>), RouteError> {
    let request: TrackIrishTrainRequest = serde_json::from_slice(body)
        .map_err(|err| RouteError::Legacy(StatusCode::BAD_REQUEST, err.to_string()))?;
    let code = tracking::normalise_train_code(&request.train_code).ok_or_else(|| {
        RouteError::Legacy(
            StatusCode::BAD_REQUEST,
            "Train codes are 1 to 8 letters and digits, like A120.".to_string(),
        )
    })?;
    if request.service_date != today && request.service_date + chrono::Days::new(1) != today {
        return Err(RouteError::Legacy(
            StatusCode::BAD_REQUEST,
            "You can track a train running today or yesterday.".to_string(),
        ));
    }
    let from = match request.from_station.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => Some(
            tracking::station_ref(raw)
                .and_then(|r| tracking::station_code_from_ref(&r).map(str::to_owned))
                .ok_or_else(|| {
                    RouteError::Legacy(
                        StatusCode::BAD_REQUEST,
                        "That isn't a station this train calls at.".to_string(),
                    )
                })?,
        ),
    };
    Ok((code, request.service_date, from))
}

/// `Origintime` on the train's date, in Europe/Dublin.
fn origin_departure(history: &TrainHistory) -> Option<DateTime<Utc>> {
    let time = NaiveTime::parse_from_str(history.origin_time.as_deref()?, "%H:%M").ok()?;
    chrono_tz::Europe::Dublin
        .from_local_datetime(&history.train_date.and_time(time))
        .earliest()
        .map(|at| at.with_timezone(&Utc))
}

async fn track(app: &App, user_id: &str, body: &[u8]) -> Result<Response, RouteError> {
    let (code, service_date, from) = validate(body, dublin_today())?;
    let history = island_of_ireland::train_history(
        &app.database,
        IslandOfIrelandNetwork::RepublicOfIreland,
        &code,
        service_date,
    )
    .await
    .map_err(|err| internal(&err))?
    .ok_or_else(|| {
        RouteError::Legacy(
            StatusCode::NOT_FOUND,
            "We haven't seen this train on a departure board.".to_string(),
        )
    })?;
    if let Some(from) = &from
        && !history.calls.iter().any(|call| call.station_code == *from)
    {
        return Err(RouteError::Legacy(
            StatusCode::BAD_REQUEST,
            "That isn't a station this train calls at.".to_string(),
        ));
    }
    let train_uid = tracking::train_uid(&code).ok_or_else(|| {
        RouteError::Legacy(
            StatusCode::BAD_REQUEST,
            "Train codes are 1 to 8 letters and digits, like A120.".to_string(),
        )
    })?;
    crate::routes::journeys::enforce_journey_cap(app, user_id)
        .await
        .map_err(|(status, message)| RouteError::Legacy(status, message))?;
    crate::routes::train::enforce_pin_cap_for_train(app, user_id, &train_uid, service_date)
        .await
        .map_err(|(status, message)| RouteError::Legacy(status, message))?;

    let pin_origin_ref = from.as_deref().and_then(tracking::station_ref);
    let tracked = crate::data::journeys::track_irish_train_on_its_own(
        &app.database,
        user_id,
        &crate::data::journeys::IrishTrainToTrack {
            train_uid: &train_uid,
            service_date,
            origin_name: &history.origin,
            destination_name: &history.destination,
            scheduled_departure: origin_departure(&history),
            pin_origin_ref: pin_origin_ref.as_deref(),
        },
    )
    .await
    .map_err(|err| internal(&err))?;
    let status = if tracked.created {
        crate::usage_events::FeatureEvent::TrackTrainMatched.record();
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(TrackIrishTrainResponse {
            journey_id: tracked.journey_id,
            tracking_id: tracked.tracking_id,
        }),
    )
        .into_response())
}

fn internal(err: &anyhow::Error) -> RouteError {
    tracing::error!(error = ?err, "track an Iarnród Éireann train");
    RouteError::Legacy(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong.".to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    fn body(json: &str) -> Vec<u8> {
        json.as_bytes().to_vec()
    }

    #[test]
    fn a_request_is_normalised_and_dated_today_or_yesterday() {
        let today = date("2026-11-06");
        let (code, day, from) = validate(
            &body(r#"{"trainCode":" a120 ","serviceDate":"2026-11-05","fromStation":"cnlly"}"#),
            today,
        )
        .unwrap();
        assert_eq!(
            (code.as_str(), day, from.as_deref()),
            ("A120", date("2026-11-05"), Some("CNLLY"))
        );
        assert!(
            validate(
                &body(r#"{"trainCode":"A120","serviceDate":"2026-11-06"}"#),
                today
            )
            .is_ok()
        );
        for bad in [
            r#"{"trainCode":"A120","serviceDate":"2026-11-04"}"#,
            r#"{"trainCode":"A120","serviceDate":"2026-11-07"}"#,
            r#"{"trainCode":"A-120","serviceDate":"2026-11-06"}"#,
            r#"{"trainCode":"ie:A120","serviceDate":"2026-11-06"}"#,
            r#"{"trainCode":"A120","serviceDate":"2026-11-06","fromStation":"NOT A CODE"}"#,
            r#"{"trainCode":"A120","serviceDate":"2026-11-06","extra":1}"#,
        ] {
            assert!(
                matches!(
                    validate(&body(bad), today),
                    Err(RouteError::Legacy(StatusCode::BAD_REQUEST, _))
                ),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_origin_departure_is_dublin_time_on_the_train_date() {
        let history = TrainHistory {
            network: IslandOfIrelandNetwork::RepublicOfIreland,
            train_code: "A120".into(),
            train_date: date("2026-07-01"),
            origin: "Dublin Connolly".into(),
            destination: "Belfast".into(),
            origin_time: Some("15:50".into()),
            destination_time: None,
            status: "En Route".into(),
            late_minutes: Some(2),
            last_location: None,
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
            calls: vec![],
        };
        // Irish summer time: UTC+1.
        assert_eq!(
            origin_departure(&history),
            Some("2026-07-01T14:50:00Z".parse().unwrap())
        );
        let no_time = TrainHistory {
            origin_time: None,
            ..history
        };
        assert_eq!(origin_departure(&no_time), None);
    }
}
