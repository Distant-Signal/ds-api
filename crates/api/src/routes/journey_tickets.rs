//! A journey leg's ticket details and its Delay Repay estimate
//! (2026-11-06; `data::leg_tickets`):
//!
//! | Route | What |
//! |---|---|
//! | `PUT /Journeys/{id}/legs/{legId}/ticket` | Save the leg's ticket type, one-company flag and name |
//! | `DELETE /Journeys/{id}/legs/{legId}/ticket` | Remove them |
//! | `POST /Journeys/{id}/legs/{legId}/ticket/pkpass` | Read an Apple Wallet pass into a prefill (nothing saved) |
//!
//! All three are the owner's only, cookie sessions only, 404 for anyone
//! else. `GET /Journeys/{id}` serves each of the owner's train legs'
//! `ticket` and `delayRepay` ([`leg_delay_repays`]): the estimate needs no
//! ticket, which only says single or return (the 120-minute band) and
//! which company's scheme applies when it is valid on one only.
//!
//! The Wallet parse runs in the resource-limited child process
//! (`data::ticket_subprocess`) after the cheap structural pre-checks
//! (`data::ticket_precheck`), as it did on the `/Train/.../tickets/pkpass`
//! routes this replaces.

use std::sync::LazyLock;

use axum::Json;
use axum::extract::{DefaultBodyLimit, Multipart, Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::app::{App, Router};
use crate::auth::AuthenticatedUser;
use crate::data::leg_tickets::{
    self, LegLookup, LegTicket, TicketDetails, TicketSource, TicketType,
};
use crate::data::schedule_services::{ServiceMode, ServiceModeFields};
use crate::data::ticket_subprocess::{ParseFailure, TicketKind, TicketParser};
use crate::data::{delay_repay_rules, ticket_extraction, ticket_precheck, ticket_subprocess};

pub fn router() -> Router {
    Router::new()
        .route(
            "/Journeys/{journey_id}/legs/{leg_id}/ticket",
            axum::routing::put(put_leg_ticket).delete(delete_leg_ticket),
        )
        .route(
            "/Journeys/{journey_id}/legs/{leg_id}/ticket/pkpass",
            // 8 MiB: generous for a real Wallet pass (tens of KB to low MB),
            // bounded against abuse; `ticket_precheck` holds the same cap.
            axum::routing::post(post_leg_ticket_pkpass).layer(DefaultBodyLimit::max(
                ticket_precheck::MAX_PKPASS_UPLOAD_BYTES,
            )),
        )
}

/// `PUT /Journeys/{id}/legs/{legId}/ticket`'s body.
#[derive(Debug, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename = "LegTicketRequest", optional_fields = nullable)]
pub(crate) struct LegTicketBody {
    ticket_type: TicketType,
    /// Valid on the leg's train company's trains only.
    #[serde(default)]
    operator_only: bool,
    /// Absent, `null` or blank: no name.
    #[serde(default)]
    name: Option<String>,
    /// `wallet` when the details came from a Wallet pass prefill;
    /// `manual` when absent.
    #[serde(default)]
    source: Option<TicketSource>,
    /// The prefill's departure, kept with a `wallet` source only.
    #[serde(default)]
    departure_date: Option<DateTime<Utc>>,
}

fn not_found() -> (StatusCode, String) {
    (
        StatusCode::NOT_FOUND,
        "no journey leg with that id".to_string(),
    )
}

/// The 409 for a leg with no train to keep ticket details on.
const NO_TRAIN_MESSAGE: &str = "Pick this leg's train first, then add your ticket.";

/// The 400 for a one-company ticket on a train whose company isn't known.
const NO_OPERATOR_MESSAGE: &str = "We don't know which company runs this train yet, so we \
    can't save the ticket as valid on that company only.";

async fn put_leg_ticket(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path((journey_id, leg_id)): Path<(i64, i64)>,
    Json(body): Json<LegTicketBody>,
) -> Result<Json<LegTicket>, (StatusCode, String)> {
    let name = crate::data::train_tracking::validate_custom_name(body.name.as_deref())
        .map_err(|msg| (StatusCode::BAD_REQUEST, msg))?;
    let (subscription_id, train_uid, service_date) =
        match leg_tickets::lookup_leg(&app.database, &user.id, journey_id, leg_id)
            .await
            .map_err(internal_error("read journey leg"))?
        {
            LegLookup::NotFound => return Err(not_found()),
            LegLookup::NoTrain => {
                return Err((StatusCode::CONFLICT, NO_TRAIN_MESSAGE.to_string()));
            }
            LegLookup::Train {
                subscription_id,
                train_uid,
                service_date,
            } => (subscription_id, train_uid, service_date),
        };
    let operator_code = if body.operator_only {
        let code = match train_uid.as_deref() {
            Some(uid) => {
                crate::data::train_operator::operator_for_train(&app.database, uid, service_date)
                    .await
                    .map_err(internal_error("read the train's operator"))?
                    .map(|operator| operator.code)
            }
            None => None,
        };
        // A train no longer held keeps the company it was saved with.
        let code = match code {
            Some(code) => Some(code),
            None => leg_tickets::for_subscriptions(&app.database, &user.id, &[subscription_id])
                .await
                .map_err(internal_error("read ticket details"))?
                .remove(&subscription_id)
                .and_then(|ticket| ticket.operator_code),
        };
        Some(code.ok_or((StatusCode::BAD_REQUEST, NO_OPERATOR_MESSAGE.to_string()))?)
    } else {
        None
    };
    let ticket = leg_tickets::set_for_subscription(
        &app.database,
        &user.id,
        subscription_id,
        &TicketDetails {
            ticket_type: body.ticket_type,
            operator_code,
            name,
            source: body.source.unwrap_or(TicketSource::Manual),
            departure_date: body.departure_date,
        },
    )
    .await
    .map_err(internal_error("save ticket details"))?;
    Ok(Json(ticket))
}

async fn delete_leg_ticket(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path((journey_id, leg_id)): Path<(i64, i64)>,
) -> Result<StatusCode, (StatusCode, String)> {
    match leg_tickets::lookup_leg(&app.database, &user.id, journey_id, leg_id)
        .await
        .map_err(internal_error("read journey leg"))?
    {
        LegLookup::NotFound => Err(not_found()),
        LegLookup::NoTrain => Ok(StatusCode::NO_CONTENT),
        LegLookup::Train {
            subscription_id, ..
        } => {
            leg_tickets::clear_for_subscription(&app.database, &user.id, subscription_id)
                .await
                .map_err(internal_error("remove ticket details"))?;
            Ok(StatusCode::NO_CONTENT)
        }
    }
}

/// What a Wallet pass says, for the leg's ticket form to show before the
/// owner saves it. Nothing is stored.
#[derive(Debug, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "LegTicketPrefill")]
pub(crate) struct LegTicketPrefill {
    ticket_type: TicketType,
    /// The pass's ticket type says it is for one company only.
    operator_only: bool,
    /// The ticket type as the pass printed it.
    ticket_type_text: Option<String>,
    /// The departure the pass gives, when it gives one.
    departure_date: Option<DateTime<Utc>>,
    /// Whether that departure is on the leg's own date (London); `null`
    /// when the pass gives none.
    departure_date_matches_leg: Option<bool>,
}

fn prefill(
    ticket: &ticket_extraction::PartialTicket,
    leg_date: chrono::NaiveDate,
) -> LegTicketPrefill {
    let text = ticket
        .ticket_type
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty());
    LegTicketPrefill {
        ticket_type: TicketType::from_text(text),
        operator_only: text.is_some_and(|t| {
            t.to_lowercase()
                .split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w == "only")
        }),
        ticket_type_text: text.map(str::to_string),
        departure_date: ticket.current_departure_date,
        departure_date_matches_leg: ticket
            .current_departure_date
            .map(|at| at.with_timezone(&chrono_tz::Europe::London).date_naive() == leg_date),
    }
}

/// `POST /Journeys/{id}/legs/{legId}/ticket/pkpass`: one multipart field,
/// `file`. REVIEW-BEFORE-SAVE: the parse touches no database; the owner
/// saves the prefill with `PUT .../ticket`.
async fn post_leg_ticket_pkpass(
    State(app): State<App>,
    user: AuthenticatedUser,
    Path((journey_id, leg_id)): Path<(i64, i64)>,
    multipart: Multipart,
) -> Result<Json<LegTicketPrefill>, (StatusCode, String)> {
    let leg_date = match leg_tickets::lookup_leg(&app.database, &user.id, journey_id, leg_id)
        .await
        .map_err(internal_error("read journey leg"))?
    {
        LegLookup::NotFound => return Err(not_found()),
        LegLookup::NoTrain => return Err((StatusCode::CONFLICT, NO_TRAIN_MESSAGE.to_string())),
        LegLookup::Train { service_date, .. } => service_date,
    };
    let ticket = handle_pkpass_upload(&TICKET_PARSER, multipart).await?;
    Ok(Json(prefill(&ticket, leg_date)))
}

/// Read the file, pre-check it, parse it in a child process.
///
/// 1. [`ticket_precheck`] rejects the common pathological shapes (wrong
///    magic bytes, zip bombs, nested archives, ZIP64) with a 413/415/422
///    and a specific message, before a slot is taken or a process started.
/// 2. [`ticket_subprocess::TicketParser`] runs the real parse in
///    `api parse-ticket pkpass`, a child with `RLIMIT_AS`/`RLIMIT_CPU`,
///    `SIGKILLed` at [`TICKET_PARSE_TIMEOUT`] (504) or when this future is
///    dropped. Its slots count live children, so a stuck parse holds a slot
///    for at most the timeout.
///
/// Status mapping: see [`ticket_parse_failure_response`].
async fn handle_pkpass_upload(
    parser: &TicketParser,
    mut multipart: Multipart,
) -> Result<ticket_extraction::PartialTicket, (StatusCode, String)> {
    let bytes = read_single_file_field(&mut multipart, "file").await?;
    if let Err(rejection) = ticket_precheck::precheck_pkpass(&bytes) {
        return Err(ticket_precheck_response(&rejection));
    }
    parser
        .parse(TicketKind::Pkpass, bytes)
        .await
        .map_err(ticket_parse_failure_response)
}

fn ticket_precheck_response(rejection: &ticket_precheck::Rejection) -> (StatusCode, String) {
    let status = match rejection {
        ticket_precheck::Rejection::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
        ticket_precheck::Rejection::WrongType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ticket_precheck::Rejection::Unacceptable(_) => StatusCode::UNPROCESSABLE_ENTITY,
    };
    (status, rejection.message().to_string())
}

/// `Busy` 503, `TimedOut` 504, `Unparseable`/`ChildDied` 422 (a file that
/// crashes or exhausts the parser is a bad file, not a server fault; the
/// detail goes to the log), `Internal` 500.
fn ticket_parse_failure_response(failure: ParseFailure) -> (StatusCode, String) {
    match failure {
        ParseFailure::Busy => (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many ticket uploads are being processed right now; please retry in a moment"
                .to_string(),
        ),
        ParseFailure::Unparseable(err) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("could not read this as a train .pkpass: {err}"),
        ),
        ParseFailure::TimedOut => (
            StatusCode::GATEWAY_TIMEOUT,
            "reading this ticket took too long; try a smaller or simpler file".to_string(),
        ),
        ParseFailure::ChildDied { status, stderr } => {
            tracing::warn!(
                %status,
                stderr = %stderr.trim(),
                "ticket parse child died (resource limit or crash)"
            );
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "could not read this as a train .pkpass: the file needed more memory or CPU \
                 than any ticket should"
                    .to_string(),
            )
        }
        ParseFailure::Internal(err) => {
            tracing::error!(error = ?err, "ticket parse child failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to parse the ticket file".to_string(),
            )
        }
    }
}

/// Wall-clock budget for one ticket parse -- generous for any legitimate
/// pass (well under a second). On expiry the child is `SIGKILLed` and
/// reaped, and its slot freed, before the 504 is sent.
const TICKET_PARSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The process-wide parser behind the upload route. Children are
/// re-executions of this very binary (`current_exe`, `/usr/local/bin/api`
/// in the image); see `data::ticket_subprocess`'s module doc.
static TICKET_PARSER: LazyLock<TicketParser> = LazyLock::new(|| {
    let exe = std::env::current_exe().unwrap_or_else(|err| {
        tracing::warn!(error = ?err, "current_exe failed; re-executing via /proc/self/exe");
        std::path::PathBuf::from("/proc/self/exe")
    });
    TicketParser::new(
        exe,
        TICKET_PARSE_PERMITS,
        TICKET_PARSE_TIMEOUT,
        ticket_subprocess::ChildLimits::default(),
    )
});

/// Live parse children allowed at once (M13, 2026-09-26 review). Beyond
/// this an upload is refused immediately with a 503, not queued -- same
/// shedding posture as `routes::trips::PLAN_SLOTS`. 4 bounds the worst case
/// at 4 x `ChildLimits::address_space_bytes` (256 MiB) = 1 GiB of memory.
/// Since the children are killed at [`TICKET_PARSE_TIMEOUT`], a flood of
/// pathological uploads can hold every slot for at most that long.
const TICKET_PARSE_PERMITS: usize = 4;

/// Reads the single multipart field named `field_name` into memory.
async fn read_single_file_field(
    multipart: &mut Multipart,
    field_name: &str,
) -> Result<Vec<u8>, (StatusCode, String)> {
    while let Some(field) = multipart
        .next_field()
        .await
        // `MultipartError::status` is 413 when the body hit
        // `DefaultBodyLimit`, 400 otherwise.
        .map_err(|err| (err.status(), format!("malformed upload: {err}")))?
    {
        if field.name() == Some(field_name) {
            let bytes = field
                .bytes()
                .await
                .map_err(|err| (err.status(), format!("failed to read upload: {err}")))?;
            return Ok(bytes.to_vec());
        }
    }
    Err((
        StatusCode::BAD_REQUEST,
        format!("no '{field_name}' field in upload"),
    ))
}

fn internal_error(operation: &'static str) -> impl Fn(anyhow::Error) -> (StatusCode, String) {
    move |err| {
        if let Some(unavailable) = crate::unavailable::response_for(&err) {
            return unavailable;
        }
        tracing::error!(error = ?err, operation, "journey ticket request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to {operation}"),
        )
    }
}

// ---------------------------------------------------------------------------
// Delay Repay on a leg
// ---------------------------------------------------------------------------

/// A train leg's Delay Repay estimate, as `GET /Journeys/{id}` serves it
/// on each of the owner's tracked GB train legs (`delayRepay`). The same
/// fields the per-ticket route served until 2026-11.
#[derive(Debug, Clone, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "LegDelayRepay")]
#[schemars(rename = "LegDelayRepay")]
pub(crate) struct LegDelayRepay {
    /// The delay, where it was measured, the outcome at the destination,
    /// the scheme and the estimate, the claim link and the disclaimer.
    /// `claimUrl` and `disclaimer` are always populated, independent of
    /// `estimate`.
    #[serde(flatten)]
    pub delay_repay: delay_repay_rules::DelayRepayFields,
    /// `serviceMode`/`liveTracking` of the service.
    #[serde(flatten)]
    pub service: ServiceModeFields,
    /// Why there is no delay at all, when that is known up front: a bus or
    /// ferry is never reported live, so its delay cannot be measured.
    /// `None` for a train.
    pub unmeasurable_reason: Option<&'static str>,
    /// Whether the leg's ticket details say single or return; `false`
    /// means the 120-minute band shows both readings.
    pub ticket_known: bool,
}

/// [`LegDelayRepay::unmeasurable_reason`] for a bus or ferry.
pub(crate) const TIMETABLE_ONLY_DELAY_REPAY_REASON: &str = "Buses and ferries aren't tracked \
    live, so we can't measure a delay on this leg. If it ran late, claim with the operator \
    using the times you recorded.";

/// What was read about one leg's train, for [`build_leg_delay_repay`].
#[derive(Debug, Default)]
pub(crate) struct DelayRepayReadings {
    /// The delay against the public arrival at `measured_at_crs`.
    pub delay: Option<crate::data::stop_delay::StopDelay>,
    pub outcome: Option<delay_repay_rules::Outcome>,
    /// The train's ATOC code, from its schedule.
    pub atoc_code: Option<String>,
    pub measured_at_crs: Option<String>,
    pub measured_at_name: Option<String>,
}

/// Pure response assembly, given no I/O capability of any kind,
/// consistent with this whole feature's "the estimator's own call sites
/// stay provably read-only/pure" posture (`delay_repay_rules`' module
/// doc). A bus or ferry (`mode`) never has a delay or an outcome: they are
/// dropped, and the response says why in `unmeasurable_reason`.
pub(crate) fn build_leg_delay_repay(
    ticket: Option<&LegTicket>,
    readings: &DelayRepayReadings,
    mode: ServiceMode,
) -> LegDelayRepay {
    let live = mode.live_tracking();
    let ticket_type = ticket.map(LegTicket::kind_text);
    LegDelayRepay {
        delay_repay: delay_repay_rules::assess(delay_repay_rules::AssessInputs {
            operator: ticket.and_then(LegTicket::operator),
            ticket_type,
            atoc_code: readings.atoc_code.as_deref(),
            delay: readings.delay.filter(|_| live),
            outcome: readings.outcome.filter(|_| live),
            measured_at_crs: readings.measured_at_crs.as_deref(),
            measured_at_name: readings.measured_at_name.as_deref(),
        }),
        service: ServiceModeFields(mode),
        unmeasurable_reason: mode
            .is_timetable_only()
            .then_some(TIMETABLE_ONLY_DELAY_REPAY_REASON),
        ticket_known: delay_repay_rules::ticket_kind(ticket_type)
            != delay_repay_rules::TicketKind::Unknown,
    }
}

/// Where Delay Repay is measured on a leg, and that station's name when
/// known: the leg's own destination (where the passenger gets off), else
/// the train's pin destination, else its terminus.
pub(crate) fn delay_repay_destination(
    leg_destination: (Option<&str>, Option<&str>),
    state: &crate::data::train_tracking::TrackedTrainState,
) -> Option<(String, Option<String>)> {
    [
        leg_destination,
        (
            state.pin_destination_crs.as_deref(),
            state.pin_destination_name.as_deref(),
        ),
        (
            state.schedule_destination_crs.as_deref(),
            state.schedule_destination_name.as_deref(),
        ),
    ]
    .into_iter()
    .filter_map(|(crs, name)| Some((crs?.trim(), name)))
    .find(|(crs, _)| !crs.is_empty())
    .map(|(crs, name)| (crs.to_uppercase(), name.map(str::to_string)))
}

/// One train leg [`leg_delay_repays`] estimates.
pub(crate) struct LegDelayRepayInput<'a> {
    pub state: &'a crate::data::train_tracking::TrackedTrainState,
    /// The leg's own destination CRS and name.
    pub destination: (Option<&'a str>, Option<&'a str>),
    pub ticket: Option<&'a LegTicket>,
    /// The leg's recorded outcome (`leg_outcomes`), when it has one:
    /// `(delay at its destination, cancelled)`. It stands in for the
    /// train's live data once that is pruned.
    pub recorded: Option<(Option<i32>, Option<bool>)>,
}

/// Whether `state` is an Iarnród Éireann train: Delay Repay is a GB
/// scheme.
fn is_irish(state: &crate::data::train_tracking::TrackedTrainState) -> bool {
    state.irish_train.is_some()
        || state
            .train_uid
            .as_deref()
            .is_some_and(common::island_of_ireland::tracking::is_train_uid)
}

/// The live delay and outcome, or the leg's recorded outcome where the
/// live data says nothing (its train's data is pruned after about 30
/// days): a recorded delay is a measurement, and a recorded cancellation
/// means the train never got there.
fn with_recorded(
    delay: Option<crate::data::stop_delay::StopDelay>,
    outcome: Option<delay_repay_rules::Outcome>,
    recorded: Option<(Option<i32>, Option<bool>)>,
) -> (
    Option<crate::data::stop_delay::StopDelay>,
    Option<delay_repay_rules::Outcome>,
) {
    let Some((recorded_delay, recorded_cancelled)) = recorded else {
        return (delay, outcome);
    };
    let delay = delay.or_else(|| {
        recorded_delay.map(|minutes| crate::data::stop_delay::StopDelay {
            minutes,
            basis: crate::data::stop_delay::DelayBasis::Public,
            provisional: false,
        })
    });
    let outcome = outcome.or_else(|| {
        if recorded_cancelled == Some(true) {
            Some(delay_repay_rules::Outcome::NotReached)
        } else {
            recorded_delay.map(|_| delay_repay_rules::Outcome::Arrived)
        }
    });
    (delay, outcome)
}

/// Each leg's estimate (`None` for an Irish train), in two batched reads:
/// the delay at each destination and whether the train got there. The
/// operator comes from the state (`train_operator::attach_to_tracked_states`
/// has run). Best effort: a failed read leaves those legs without a delay.
pub(crate) async fn leg_delay_repays(
    app: &App,
    inputs: &[LegDelayRepayInput<'_>],
) -> Vec<Option<LegDelayRepay>> {
    let destinations: Vec<Option<(String, Option<String>)>> = inputs
        .iter()
        .map(|input| delay_repay_destination(input.destination, input.state))
        .collect();
    let targets: Vec<Option<crate::data::stop_delay::StopDelayTarget>> = inputs
        .iter()
        .zip(&destinations)
        .map(|(input, destination)| {
            if is_irish(input.state) || input.state.service.0.is_timetable_only() {
                return None;
            }
            crate::data::stop_delay::target(
                input.state.trains_id,
                input.state.train_uid.as_deref(),
                input.state.service_date,
                destination.as_ref().map(|(crs, _)| crs.as_str()),
                input.state.working_delay_minutes,
            )
            // No stop to measure at means no measurement, never the delay
            // at wherever the train happens to be.
            .filter(|target| target.stop_crs.is_some())
        })
        .collect();
    let wanted: Vec<crate::data::stop_delay::StopDelayTarget> =
        targets.iter().flatten().cloned().collect();
    let (delays, outcomes) = if wanted.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let outcome_targets: Vec<crate::data::delay_repay_outcome::OutcomeTarget> = wanted
            .iter()
            .map(|target| crate::data::delay_repay_outcome::OutcomeTarget {
                trains_id: target.trains_id,
                train_uid: target.train_uid.clone(),
                service_date: target.service_date,
                destination_crs: target.stop_crs.clone().unwrap_or_default(),
            })
            .collect();
        let delays = crate::data::stop_delay::stop_delays(&app.database, &wanted)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = ?err, "could not read leg delays for Delay Repay");
                vec![None; wanted.len()]
            });
        let outcomes = crate::data::delay_repay_outcome::outcomes(&app.database, &outcome_targets)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = ?err, "could not read leg outcomes for Delay Repay");
                vec![None; wanted.len()]
            });
        (delays, outcomes)
    };
    let mut delays = delays.into_iter();
    let mut outcomes = outcomes.into_iter();
    inputs
        .iter()
        .zip(targets)
        .zip(destinations)
        .map(|((input, target), destination)| {
            if is_irish(input.state) {
                return None;
            }
            let (live_delay, live_outcome) = if target.is_some() {
                (delays.next().flatten(), outcomes.next().flatten())
            } else {
                (None, None)
            };
            let (delay, outcome) = with_recorded(live_delay, live_outcome, input.recorded);
            let (measured_at_crs, measured_at_name) = destination.unzip();
            Some(build_leg_delay_repay(
                input.ticket,
                &DelayRepayReadings {
                    delay,
                    outcome,
                    atoc_code: input.state.operator_code.clone(),
                    measured_at_crs,
                    measured_at_name: measured_at_name.flatten(),
                },
                input.state.service.0,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use axum::extract::FromRequest;

    use super::*;
    use crate::data::stop_delay::{DelayBasis, StopDelay};
    use crate::data::ticket_precheck::fixtures;

    fn fixed_instant() -> DateTime<Utc> {
        "2026-08-29T12:00:00Z".parse().unwrap()
    }

    fn ticket(ticket_type: TicketType, operator: Option<&str>) -> LegTicket {
        LegTicket {
            ticket_type,
            operator_only: operator.is_some(),
            operator_code: operator.map(str::to_string),
            name: None,
            source: TicketSource::Manual,
            departure_date: None,
            id: 1,
        }
    }

    fn readings(delay: Option<StopDelay>, atoc: Option<&str>) -> DelayRepayReadings {
        DelayRepayReadings {
            delay,
            atoc_code: atoc.map(str::to_string),
            measured_at_crs: Some("EDB".into()),
            ..DelayRepayReadings::default()
        }
    }

    fn arrived(minutes: i32) -> StopDelay {
        StopDelay {
            minutes,
            basis: DelayBasis::Public,
            provisional: false,
        }
    }

    fn projected(minutes: i32) -> StopDelay {
        StopDelay {
            minutes,
            basis: DelayBasis::PublicSchedule,
            provisional: true,
        }
    }

    fn state() -> crate::data::train_tracking::TrackedTrainState {
        let delay_minutes = None;
        crate::data::train_tracking::TrackedTrainState {
            service: ServiceModeFields::default(),
            id: 1,
            service_date: "2026-08-29".parse().unwrap(),
            pin_origin_crs: Some("KGX".to_string()),
            pin_destination_crs: Some("EDB".to_string()),
            pin_scheduled_departure: Some(fixed_instant()),
            pin_origin_name: Some("London Kings Cross".to_string()),
            pin_destination_name: Some("Edinburgh Waverley".to_string()),
            resolution_status: "resolved".to_string(),
            train_uid: Some("A12345".to_string()),
            train_id: Some("1A23".to_string()),
            schedule_destination_crs: None,
            schedule_destination_name: None,
            schedule_calling_points: None,
            schedule_skipped_stations: vec![],
            schedule_platform: None,
            schedule_planned_platform: None,
            status: Some("late".to_string()),
            last_reported_location: Some("York".to_string()),
            last_event_type: Some("DEPARTURE".to_string()),
            delay_minutes,
            delay_basis: None,
            delay_provisional: false,
            working_delay_minutes: delay_minutes,
            next_calling_point: Some("Newcastle".to_string()),
            eta_next: Some(fixed_instant()),
            eta_source: Some("darwin-estimated".to_string()),
            custom_name: None,
            shared_group_count: 0,
            journey_ids: None,
            single_leg_journey_ids: None,
            notifications_enabled: None,
            trains_id: Some(1),
            journey_stops: None,
            may_have_arrived: false,
            predicted_start: None,
            operator_code: None,
            operator_name: None,
            cancelled: false,
            cancel_reason_code: None,
            cancel_reason: None,
            change_of_origin_reason_code: None,
            change_of_origin_reason: None,
            cancellation: None,
            tracking_phase: None,
            first_report_expected_at: None,
            irish_train: None,
        }
    }

    /// No ticket needed: the train's own operator code picks the scheme,
    /// and a 2-hour delay shows both readings of the 120-minute band.
    #[test]
    fn a_leg_with_no_ticket_is_estimated_from_its_trains_operator() {
        let response = build_leg_delay_repay(
            None,
            &readings(Some(arrived(45)), Some("GR")),
            ServiceMode::Train,
        );
        let estimate = response.delay_repay.estimate.expect("45 minutes on LNER");
        assert_eq!(estimate.scheme, "DR30");
        assert_eq!(estimate.percentage, Some(50));
        assert_eq!(
            response.delay_repay.claim_url,
            "https://delayrepay.lner.co.uk/delayrepayV2/"
        );
        assert!(!response.ticket_known);

        let late = build_leg_delay_repay(
            None,
            &readings(Some(arrived(125)), Some("XC")),
            ServiceMode::Train,
        );
        let json = serde_json::to_value(&late).unwrap();
        assert_eq!(json["estimate"]["fareBasis"], "return");
        assert_eq!(json["estimate"]["ticketKind"], "unknown");
        assert_eq!(json["ticketKnown"], false);
    }

    /// A ticket type says single or return; a one-company ticket picks
    /// that company's scheme.
    #[test]
    fn a_ticket_type_refines_the_estimate() {
        for (kind, basis, known) in [
            (TicketType::Single, "single", true),
            (TicketType::Advance, "single", true),
            (TicketType::Return, "return", true),
            (TicketType::OffPeak, "return", false),
            (TicketType::Season, "return", false),
            (TicketType::Unknown, "return", false),
        ] {
            let response = build_leg_delay_repay(
                Some(&ticket(kind, None)),
                &readings(Some(arrived(125)), Some("XC")),
                ServiceMode::Train,
            );
            let json = serde_json::to_value(&response).unwrap();
            assert_eq!(json["estimate"]["fareBasis"], basis, "{kind:?}");
            assert_eq!(json["ticketKnown"], known, "{kind:?}");
        }
        let only = build_leg_delay_repay(
            Some(&ticket(TicketType::Single, Some("GR"))),
            &readings(Some(arrived(45)), Some("XC")),
            ServiceMode::Train,
        );
        assert_eq!(only.delay_repay.scheme_operator, Some("LNER"));
    }

    /// Design doc §9 decision 3: before the train reaches the destination
    /// the band is provisional, and says so; once it has, it is final.
    #[test]
    fn a_projected_delay_gives_a_provisional_estimate_and_an_arrival_a_final_one() {
        let response = build_leg_delay_repay(
            None,
            &readings(Some(projected(18)), Some("SE")),
            ServiceMode::Train,
        );
        assert!(response.delay_repay.provisional);
        let estimate = response.delay_repay.estimate.unwrap();
        assert!(estimate.provisional);
        assert_eq!(estimate.band_minutes, 15);
        assert_eq!(
            estimate.disclaimer,
            delay_repay_rules::PROVISIONAL_DISCLAIMER
        );
        let response = build_leg_delay_repay(
            None,
            &readings(Some(arrived(14)), Some("SE")),
            ServiceMode::Train,
        );
        assert!(!response.delay_repay.provisional);
        assert_eq!(response.delay_repay.estimate, None);
        assert_eq!(response.delay_repay.delay_minutes, Some(14));
    }

    /// A bus or ferry leg is never measured: no delay, no estimate, a
    /// reason saying why, and still the claim link and disclaimer.
    #[test]
    fn a_bus_leg_says_its_delay_cannot_be_measured() {
        for mode in [
            ServiceMode::ReplacementBus,
            ServiceMode::Bus,
            ServiceMode::Ferry,
        ] {
            let response =
                build_leg_delay_repay(None, &readings(Some(arrived(45)), Some("GR")), mode);
            assert_eq!(response.delay_repay.delay_minutes, None);
            assert_eq!(response.delay_repay.estimate, None);
            assert_eq!(
                response.unmeasurable_reason,
                Some(TIMETABLE_ONLY_DELAY_REPAY_REASON)
            );
            assert_ne!(response.delay_repay.claim_url, "");
            let json = serde_json::to_value(&response).unwrap();
            assert_eq!(json["liveTracking"], false);
        }
    }

    /// A train that did not reach the destination has no delay or
    /// estimate but names the destination; nothing known at all still
    /// carries the claim link and disclaimer.
    #[test]
    fn not_reached_and_unknown_still_carry_the_claim_link() {
        let not_reached = build_leg_delay_repay(
            None,
            &DelayRepayReadings {
                delay: Some(projected(70)),
                outcome: Some(delay_repay_rules::Outcome::NotReached),
                atoc_code: Some("GR".into()),
                measured_at_crs: Some("EDB".into()),
                measured_at_name: Some("Edinburgh Waverley".into()),
            },
            ServiceMode::Train,
        );
        let json = serde_json::to_value(&not_reached).unwrap();
        assert_eq!(json["outcome"], "notReached");
        assert!(json["delayMinutes"].is_null() && json["estimate"].is_null());
        assert_eq!(json["measuredAtName"], "Edinburgh Waverley");
        let unknown = build_leg_delay_repay(None, &readings(None, None), ServiceMode::Train);
        assert_eq!(unknown.delay_repay.estimate, None);
        assert_eq!(
            unknown.delay_repay.claim_url,
            delay_repay_rules::GENERIC_CLAIM_URL
        );
        assert_ne!(unknown.delay_repay.disclaimer, "");
    }

    #[test]
    fn a_recorded_outcome_stands_in_for_pruned_train_data() {
        use delay_repay_rules::Outcome;
        let (delay, outcome) = with_recorded(None, None, Some((Some(42), Some(false))));
        assert_eq!(delay, Some(arrived(42)));
        assert_eq!(outcome, Some(Outcome::Arrived));
        let (delay, outcome) = with_recorded(None, None, Some((None, Some(true))));
        assert_eq!(delay, None);
        assert_eq!(outcome, Some(Outcome::NotReached));
        // Live data, when there is any, wins.
        let (delay, outcome) =
            with_recorded(Some(projected(5)), None, Some((Some(42), Some(false))));
        assert_eq!(delay, Some(projected(5)));
        assert_eq!(outcome, Some(Outcome::Arrived));
        assert_eq!(with_recorded(None, None, None), (None, None));
    }

    #[test]
    fn the_destination_is_the_legs_then_the_pins_then_the_terminus() {
        let mut state = state();
        state.pin_destination_crs = Some("EDB".into());
        state.pin_destination_name = Some("Edinburgh Waverley".into());
        state.schedule_destination_crs = Some("ABD".into());
        assert_eq!(
            delay_repay_destination((Some(" ncl "), Some("Newcastle")), &state),
            Some(("NCL".into(), Some("Newcastle".into())))
        );
        assert_eq!(
            delay_repay_destination((None, None), &state),
            Some(("EDB".into(), Some("Edinburgh Waverley".into())))
        );
        state.pin_destination_crs = None;
        assert_eq!(
            delay_repay_destination((None, None), &state).map(|(crs, _)| crs),
            Some("ABD".into())
        );
    }

    #[test]
    fn a_wallet_pass_prefills_the_type_and_checks_the_date() {
        let pass = ticket_extraction::PartialTicket {
            operator: Some("LNER".into()),
            ticket_type: Some("Off-Peak Return LNER only".into()),
            origin_crs: None,
            destination_crs: None,
            current_departure_date: Some(fixed_instant()),
            source: "pkpass-semantics",
        };
        let same_day = prefill(&pass, "2026-08-29".parse().unwrap());
        assert_eq!(same_day.ticket_type, TicketType::Return);
        assert!(same_day.operator_only);
        assert_eq!(same_day.departure_date_matches_leg, Some(true));
        let other_day = prefill(&pass, "2026-08-30".parse().unwrap());
        assert_eq!(other_day.departure_date_matches_leg, Some(false));
        let bare = prefill(
            &ticket_extraction::PartialTicket {
                ticket_type: None,
                current_departure_date: None,
                ..pass
            },
            "2026-08-29".parse().unwrap(),
        );
        assert_eq!(bare.ticket_type, TicketType::Unknown);
        assert!(!bare.operator_only);
        assert_eq!(bare.departure_date_matches_leg, None);
    }

    async fn multipart_with(bytes: &[u8]) -> Multipart {
        let boundary = "ticketboundary";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"ticket\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let request = axum::http::Request::builder()
            .method("POST")
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(axum::body::Body::from(body))
            .unwrap();
        Multipart::from_request(request, &()).await.unwrap()
    }

    /// The parse children's worst-case address space: 4 slots x 256 MiB =
    /// 1 GiB (2026-09-27 decision; was 8 slots, 2 GiB).
    #[test]
    fn ticket_parse_worst_case_is_one_gib() {
        assert_eq!(TICKET_PARSE_PERMITS, 4);
        let per_child = ticket_subprocess::ChildLimits::default().address_space_bytes;
        assert_eq!(per_child, 256 * 1024 * 1024);
        assert_eq!(TICKET_PARSE_PERMITS as u64 * per_child, 1024 * 1024 * 1024);
    }

    /// A parser that must never be reached: its executable doesn't exist.
    fn unreachable_parser(slots: usize) -> TicketParser {
        TicketParser::new(
            "/nonexistent/api".into(),
            slots,
            std::time::Duration::from_secs(1),
            ticket_subprocess::ChildLimits::default(),
        )
    }

    #[tokio::test]
    async fn every_slot_busy_is_a_503() {
        let parser = unreachable_parser(0);
        let multipart = multipart_with(&fixtures::train_pkpass()).await;
        let err = handle_pkpass_upload(&parser, multipart).await.unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE, "{}", err.1);
        assert!(err.1.contains("retry"), "{}", err.1);
    }

    #[tokio::test]
    async fn precheck_rejections_map_to_413_415_and_422_without_spawning() {
        let parser = unreachable_parser(1);
        let cases: Vec<(Vec<u8>, StatusCode)> = vec![
            (
                {
                    let mut big = fixtures::train_pkpass();
                    big.resize(ticket_precheck::MAX_PKPASS_UPLOAD_BYTES + 1, 0);
                    big
                },
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
            (
                b"%PDF-1.4\n%%EOF\n".to_vec(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (
                fixtures::zip_with(&[("readme.txt", b"x")]),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
        ];
        for (bytes, expected) in cases {
            let err = handle_pkpass_upload(&parser, multipart_with(&bytes).await)
                .await
                .unwrap_err();
            assert_eq!(err.0, expected, "{}", err.1);
        }
        assert_eq!(parser.available_slots(), 1);
    }

    #[test]
    fn parse_failures_map_to_their_statuses() {
        let cases = [
            (ParseFailure::Busy, StatusCode::SERVICE_UNAVAILABLE),
            (ParseFailure::TimedOut, StatusCode::GATEWAY_TIMEOUT),
            (
                ParseFailure::Unparseable("nope".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                ParseFailure::ChildDied {
                    status: "signal 6".into(),
                    stderr: String::new(),
                },
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (
                ParseFailure::Internal(anyhow::anyhow!("boom")),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ];
        for (failure, expected) in cases {
            assert_eq!(ticket_parse_failure_response(failure).0, expected);
        }
    }

    #[test]
    fn router_builds_without_panicking() {
        let _ = router();
    }
}
