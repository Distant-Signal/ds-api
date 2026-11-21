//! A train leg's own copy of its train's timetable (DS-643), kept as long as
//! the journey is.
//!
//! ds-workers prunes `trains` (and with it the leg's subscription's
//! `trains_id`, `ON DELETE SET NULL`) about 30 days after the service date,
//! and `schedule_services` goes after about 8, but a journey is kept for
//! `past_travel_days` (548). Without a copy, an old matched leg reads "matched
//! to a timetabled train, waiting for its first movement" with no stops. So
//! [`crate::data::leg_outcomes`]'s hourly sweep writes this document to
//! `journey_legs.final_snapshot` (with `final_train_uid` and
//! `final_snapshot_at`) as soon as the leg has its train, and the journey
//! routes render from it once `trains_id` is gone ([`apply`]).
//!
//! # The document (version 1)
//!
//! One JSON object, camelCase keys. A key whose value is unknown is absent,
//! never `null`; readers treat an absent key and `null` alike, and ignore
//! keys they do not know (a later `v` adds keys; it never changes these).
//!
//! | key | type | meaning |
//! |---|---|---|
//! | `v` | `1` | the document version; required |
//! | `source` | `"live"`, `"archive"`, `"subscription"` | where it was made from: the live `trains` row (the sweep), an archive of one (a restore), or only what the subscription held (a restore) |
//! | `capturedAt` | RFC 3339 | when the source was read |
//! | `trainUid` | string | the CIF train UID |
//! | `trainId` | string | TRUST's daily train id (headcode and RID), when known |
//! | `serviceDate` | `YYYY-MM-DD` | London's service date |
//! | `originCrs`, `destinationCrs` | string | the train's own ends |
//! | `scheduledDeparture` | RFC 3339 | the train's timetabled departure from its origin |
//! | `matchedLineId` | string | the line catalogue id it was matched on |
//! | `platform`, `plannedPlatform` | string | the origin platform captured at pin time and the planned one |
//! | `skippedStations` | `[string]` | Darwin's skipped-stop snapshot, absent when empty |
//! | `callingPoints` | `[object]` | exactly `trains.calling_points`, the JSON `data::journey::build_journey_stops` reads (`RawCallingPoint`): `tiploc`, `kind`, `bookedArrival`, `bookedDeparture`, `dayOffset`, `platform` and the public and working times; required, may be empty |
//! | `finalArrivalDelayMinutes` | integer | the leg's outcome (DS-379), once the leg is over and the delay was measured |
//! | `trainCancelled` | boolean | likewise |
//!
//! The outcome keys are the leg's `final_arrival_delay_minutes` and
//! `final_train_cancelled` copied in by the sweep when the leg completes
//! (and `final_snapshot_at` then moves past `final_recorded_at`, which is
//! how the sweep knows it is done). A restore writes the same document with
//! `source` `"archive"` or `"subscription"`, sets `final_train_uid` to its
//! `trainUid` and `final_snapshot_at` to now.
//!
//! Size: see the numbers in `docs/api-changelog.md`'s entry; the calling
//! points are almost all of it.
//!
//! Personal data: it holds nothing about the user (a public timetable and
//! the train's identity), but where and when a person travelled is theirs,
//! so it lives on the leg: it is deleted with its journey, exported with the
//! leg (`data::account::export_account` exports every `journey_legs`
//! column) and removed with the account.

use std::collections::HashMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::data::train_tracking::TrackedTrainState;

/// The document version this code writes and reads.
pub const VERSION: u32 = 1;

/// Where a snapshot was made from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Source {
    /// The live `trains` row, by the hourly sweep.
    Live,
    /// An archive of a `trains` row, by a restore.
    Archive,
    /// Only what the leg's subscription held, by a restore.
    Subscription,
}

/// The version 1 document; see the module docs for each key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegSnapshot {
    pub v: u32,
    pub source: Source,
    pub captured_at: DateTime<Utc>,
    pub train_uid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub train_id: Option<String>,
    pub service_date: NaiveDate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_crs: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_crs: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_departure: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_line_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_platform: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_stations: Vec<String>,
    /// `trains.calling_points` as stored.
    #[serde(default)]
    pub calling_points: Vec<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_arrival_delay_minutes: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub train_cancelled: Option<bool>,
}

/// The `trains` columns a snapshot is made from.
#[derive(Debug, Clone, Default)]
pub struct TrainFacts {
    pub train_uid: String,
    pub train_id: Option<String>,
    pub service_date: NaiveDate,
    pub origin_crs: Option<String>,
    pub destination_crs: Option<String>,
    pub scheduled_departure: Option<DateTime<Utc>>,
    pub matched_line_id: Option<String>,
    pub platform: Option<String>,
    pub planned_platform: Option<String>,
    pub skipped_stations: Vec<String>,
    pub calling_points: Option<serde_json::Value>,
}

impl LegSnapshot {
    /// The snapshot of a live `trains` row, without an outcome.
    pub fn from_train(train: TrainFacts, captured_at: DateTime<Utc>) -> Self {
        let calling_points = match train.calling_points {
            Some(serde_json::Value::Array(points)) => points,
            _ => Vec::new(),
        };
        Self {
            v: VERSION,
            source: Source::Live,
            captured_at,
            train_uid: train.train_uid,
            train_id: train.train_id,
            service_date: train.service_date,
            origin_crs: train.origin_crs,
            destination_crs: train.destination_crs,
            scheduled_departure: train.scheduled_departure,
            matched_line_id: train.matched_line_id,
            platform: train.platform,
            planned_platform: train.planned_platform,
            skipped_stations: train.skipped_stations,
            calling_points,
            final_arrival_delay_minutes: None,
            train_cancelled: None,
        }
    }

    /// This snapshot with the leg's recorded outcome.
    #[must_use]
    pub fn with_outcome(mut self, delay_minutes: Option<i32>, cancelled: Option<bool>) -> Self {
        self.final_arrival_delay_minutes = delay_minutes;
        self.train_cancelled = cancelled;
        self
    }

    /// Reads a stored document; `None` for one of another version or that
    /// does not fit the shape.
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        let snapshot: Self = serde_json::from_value(value.clone()).ok()?;
        (snapshot.v == VERSION).then_some(snapshot)
    }

    /// The JSON to store.
    ///
    /// # Panics
    ///
    /// Never: every field serializes.
    #[expect(
        clippy::expect_used,
        reason = "a struct of strings, numbers and JSON values always serializes"
    )]
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("a leg snapshot serializes")
    }

    /// The train's status as the outcome gives it: `completed` or
    /// `cancelled`, `None` when none was recorded.
    fn status(&self) -> Option<&'static str> {
        match self.train_cancelled {
            Some(true) => Some("cancelled"),
            Some(false) => Some("completed"),
            None => None,
        }
    }
}

/// What the journey routes say about where a train leg's detail comes from
/// (`trainDetail`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TrainDetail {
    /// The shared `trains` row is still held: live data.
    Live,
    /// The row is gone; the stops, platform and outcome come from the leg's
    /// own copy (`snapshotAt` says when it was last written). There is no
    /// live data.
    Snapshot,
    /// The row is gone and the leg holds no copy: the train's detail is no
    /// longer available.
    Unavailable,
}

/// Whether a leg whose tracked state is `state` has a train it ought to
/// show detail for: it was matched (`schedule_matched` or `resolved`), so a
/// missing `trains` row means it was pruned, not that it has not arrived
/// yet.
pub fn was_matched(state: &TrackedTrainState) -> bool {
    matches!(
        state.resolution_status.as_str(),
        "resolved" | "schedule_matched"
    )
}

/// Each leg's stored snapshot and when it was written, for `leg_ids`.
pub async fn read_for_legs(
    pool: &PgPool,
    leg_ids: &[i64],
) -> anyhow::Result<HashMap<i64, (serde_json::Value, DateTime<Utc>)>> {
    if leg_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i64, serde_json::Value, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT id, final_snapshot, final_snapshot_at FROM journey_legs \
         WHERE id = ANY($1) AND final_snapshot IS NOT NULL",
    )
    .bind(leg_ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, snapshot, at)| (id, (snapshot, at.unwrap_or_default())))
        .collect())
}

/// Renders each state that has a snapshot from it: `snapshots[i]` belongs
/// to `states[i]`. Fills the timetable fields the live read takes from the
/// `trains` row, the outcome as the delay and status, and the stop list
/// (booked times only: the movements are gone). Best effort: a stop list
/// that cannot be built leaves the state's other fields set and its stops
/// `None`.
pub async fn apply(
    pool: &PgPool,
    states: &mut [TrackedTrainState],
    snapshots: &[Option<LegSnapshot>],
) {
    use crate::data::stop_delay::DelayBasis;

    let mut buildable = Vec::new();
    for (index, (state, snapshot)) in states.iter_mut().zip(snapshots).enumerate() {
        let Some(snapshot) = snapshot else { continue };
        state.train_uid = Some(snapshot.train_uid.clone());
        state.train_id.clone_from(&snapshot.train_id);
        state
            .schedule_destination_crs
            .clone_from(&snapshot.destination_crs);
        state.schedule_calling_points = (!snapshot.calling_points.is_empty())
            .then(|| serde_json::Value::Array(snapshot.calling_points.clone()));
        state
            .schedule_skipped_stations
            .clone_from(&snapshot.skipped_stations);
        state.schedule_platform.clone_from(&snapshot.platform);
        state
            .schedule_planned_platform
            .clone_from(&snapshot.planned_platform);
        if state.pin_origin_crs.is_none() {
            state.pin_origin_crs.clone_from(&snapshot.origin_crs);
        }
        if state.pin_destination_crs.is_none() {
            state
                .pin_destination_crs
                .clone_from(&snapshot.destination_crs);
        }
        if state.pin_scheduled_departure.is_none() {
            state.pin_scheduled_departure = snapshot.scheduled_departure;
        }
        state.status = snapshot.status().map(str::to_owned);
        if let Some(minutes) = snapshot.final_arrival_delay_minutes {
            state.delay_minutes = Some(minutes);
            state.delay_basis = Some(DelayBasis::Public);
            state.delay_provisional = false;
        }
        if !snapshot.calling_points.is_empty() {
            buildable.push(index);
        }
    }
    if buildable.is_empty() {
        return;
    }
    let values: Vec<Option<serde_json::Value>> = buildable
        .iter()
        .map(|&index| states[index].schedule_calling_points.clone())
        .collect();
    let built = {
        let requests: Vec<crate::data::journey::JourneyStopsRequest<'_>> = buildable
            .iter()
            .zip(&values)
            .filter_map(|(&index, value)| {
                let snapshot = snapshots[index].as_ref()?;
                Some(crate::data::journey::JourneyStopsRequest {
                    // No row, so no movements: `0` matches none.
                    trains_id: 0,
                    train_uid: &snapshot.train_uid,
                    service_date: snapshot.service_date,
                    calling_points_json: value.as_ref(),
                    current_delay_minutes: None,
                    skipped_stations: &snapshot.skipped_stations,
                    platform: snapshot.platform.as_deref(),
                    planned_platform: snapshot.planned_platform.as_deref(),
                    predicted_late_minutes: None,
                })
            })
            .collect();
        crate::data::journey::build_journey_stops_batch(pool, &requests).await
    };
    let built = match built {
        Ok(built) => built,
        Err(err) => {
            tracing::warn!(error = ?err, "could not build journey stops from leg snapshots");
            return;
        }
    };
    for (index, stops) in buildable.into_iter().zip(built) {
        match stops {
            Ok(stops) => states[index].journey_stops = stops,
            Err(err) => {
                tracing::warn!(error = ?err, "could not build journey stops from a leg snapshot");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> TrainFacts {
        TrainFacts {
            train_uid: "C12345".to_owned(),
            train_id: Some("1A23".to_owned()),
            service_date: NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(),
            origin_crs: Some("PAD".to_owned()),
            destination_crs: Some("RDG".to_owned()),
            scheduled_departure: None,
            matched_line_id: None,
            platform: Some("3".to_owned()),
            planned_platform: None,
            skipped_stations: Vec::new(),
            calling_points: Some(serde_json::json!([
                {"tiploc": "PADTON", "kind": "Origin", "bookedDeparture": "09:00:00"},
                {"tiploc": "READING", "kind": "Terminate", "bookedArrival": "09:25:00"},
            ])),
        }
    }

    #[test]
    fn the_document_is_compact_camel_case_with_unknowns_absent() {
        let snapshot = LegSnapshot::from_train(facts(), Utc::now());
        let value = snapshot.to_value();
        assert_eq!(value["v"], 1);
        assert_eq!(value["source"], "live");
        assert_eq!(value["trainUid"], "C12345");
        assert_eq!(value["serviceDate"], "2026-11-01");
        assert_eq!(value["callingPoints"].as_array().unwrap().len(), 2);
        let object = value.as_object().unwrap();
        for absent in [
            "scheduledDeparture",
            "matchedLineId",
            "plannedPlatform",
            "skippedStations",
            "finalArrivalDelayMinutes",
            "trainCancelled",
        ] {
            assert!(!object.contains_key(absent), "{absent}");
        }
    }

    #[test]
    fn it_round_trips_and_a_later_version_is_not_read() {
        let snapshot =
            LegSnapshot::from_train(facts(), Utc::now()).with_outcome(Some(17), Some(false));
        let value = snapshot.to_value();
        assert_eq!(LegSnapshot::parse(&value), Some(snapshot));
        let mut later = value;
        later["v"] = serde_json::json!(2);
        assert_eq!(LegSnapshot::parse(&later), None);
        assert_eq!(LegSnapshot::parse(&serde_json::json!({"v": 1})), None);
    }

    #[test]
    fn a_missing_optional_key_reads_as_unknown() {
        let value = serde_json::json!({
            "v": 1, "source": "archive", "capturedAt": "2026-11-02T10:00:00Z",
            "trainUid": "C1", "serviceDate": "2026-11-01", "trainId": null,
            "callingPoints": [],
        });
        let snapshot = LegSnapshot::parse(&value).unwrap();
        assert_eq!(snapshot.source, Source::Archive);
        assert_eq!(snapshot.train_id, None);
        assert_eq!(snapshot.skipped_stations, Vec::<String>::new());
        assert_eq!(snapshot.status(), None);
    }

    /// The document `scripts/restore-leg-snapshots-from-archive.py` writes
    /// (its test regenerates the file from the script's builder): it must
    /// parse as a version 1 snapshot with every key it sets understood, and
    /// serialise back to the same JSON, so the script cannot drift from
    /// this type (an unknown or misspelt key would be dropped by the
    /// round trip and fail the comparison).
    #[test]
    fn the_archive_restore_script_golden_document_is_read() {
        // Read at run time, not `include_str!`: the images' build context
        // leaves `scripts/tests` out and this file is test-only.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/tests/fixtures/leg-snapshot-archive.golden.json");
        let golden: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let snapshot = LegSnapshot::parse(&golden).expect("the golden document parses");
        assert_eq!(snapshot.source, Source::Archive);
        assert_eq!(snapshot.train_uid, "C12345");
        assert_eq!(snapshot.final_arrival_delay_minutes, Some(7));
        assert_eq!(snapshot.train_cancelled, Some(false));
        assert_eq!(snapshot.status(), Some("completed"));
        assert_eq!(snapshot.calling_points.len(), 3);
        assert_eq!(snapshot.to_value(), golden);
    }

    #[test]
    fn the_outcome_gives_the_status() {
        let snapshot = LegSnapshot::from_train(facts(), Utc::now());
        assert_eq!(
            snapshot.clone().with_outcome(None, Some(true)).status(),
            Some("cancelled")
        );
        assert_eq!(
            snapshot.with_outcome(Some(3), Some(false)).status(),
            Some("completed")
        );
    }

    #[test]
    fn a_non_array_calling_points_is_empty() {
        let mut train = facts();
        train.calling_points = Some(serde_json::json!({"not": "an array"}));
        assert_eq!(
            LegSnapshot::from_train(train, Utc::now()).calling_points,
            Vec::<serde_json::Value>::new()
        );
    }
}
