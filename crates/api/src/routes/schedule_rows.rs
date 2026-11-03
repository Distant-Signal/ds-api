//! The live and origin fields the CIF schedule lists carry on top of their
//! timetable fields, so a client can show them the way the line view
//! (`GET /public/lines/{id}/trains?view=summary`) does (2026-10-07).
//!
//! Used by `GET /public/trains/search` (`routes::trains`) and
//! `GET /public/stations/{crs}/schedule-departures` (`routes::departures`).
//! Every helper here works on one page of already-rendered rows and reads
//! the database in batches (one query per kind of lookup, never one per
//! row), so a page costs the same few round trips however many rows it
//! has. The fields are additive: no existing key on a row is touched.

use std::collections::HashMap;

use serde_json::Value;
use sqlx::PgPool;

use crate::data::schedule_services::{ServiceMode, ServiceModeFields};
use crate::render::{CallingPointDepartureJson, ScheduleDepartureJson};
use crate::routes::line_trains_summary::{LiveSummary, live_summaries};

/// A rendered schedule row these helpers fill in: its `uid` and true
/// origin, and the `originName`, `live` and service-mode fields they set.
pub(crate) trait ScheduleRow {
    fn uid(&self) -> Option<&str>;
    fn origin_crs(&self) -> Option<&str>;
    fn set_origin_name(&mut self, name: Option<String>);
    fn set_live(&mut self, live: Option<LiveSummary>);
    fn set_service_mode(&mut self, mode: ServiceMode);
}

impl ScheduleRow for ScheduleDepartureJson {
    fn uid(&self) -> Option<&str> {
        self.uid.as_deref()
    }
    fn origin_crs(&self) -> Option<&str> {
        self.origin_crs.as_deref()
    }
    fn set_origin_name(&mut self, name: Option<String>) {
        self.origin_name = name;
    }
    fn set_live(&mut self, live: Option<LiveSummary>) {
        self.live = live;
    }
    fn set_service_mode(&mut self, mode: ServiceMode) {
        self.service = ServiceModeFields(mode);
    }
}

/// One row of the train search, arrivals and headcode boards
/// (`routes::trains`): the calling-point departure and what every board
/// adds to it. The board-specific fields are absent elsewhere.
#[derive(Debug, Clone, serde::Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "TrainSearchResult")]
#[expect(
    clippy::option_option,
    reason = "a `stopsAt*` field is absent without `stops_at` and `null` without the time"
)]
pub(crate) struct TrainSearchRow {
    #[serde(flatten)]
    pub departure: CallingPointDepartureJson,
    /// The row's service date: the searched date, or with
    /// `across_midnight` the day before.
    pub service_date: String,
    pub origin_name: Option<String>,
    pub live: Option<LiveSummary>,
    #[serde(flatten)]
    pub service: ServiceModeFields,
    /// Days after `serviceDate` that `scheduled` falls on. Absent for a
    /// train terminating at an arrivals board's station.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub day_offset: Option<i64>,
    /// `workingDeparture`'s day offset (the search only).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub working_day_offset: Option<u64>,
    /// The search with `stops_at`: the arrival there (public, `HH:MM`),
    /// its day offset, and the same for the working arrival. `null`
    /// together when the call has no such time.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stops_at_arrival: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stops_at_arrival_day_offset: Option<Option<i64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stops_at_working_arrival: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stops_at_working_arrival_day_offset: Option<Option<i64>>,
}

impl TrainSearchRow {
    /// `departure` on `service_date`, before any lookup fills it in.
    pub(crate) fn new(departure: CallingPointDepartureJson, service_date: String) -> Self {
        Self {
            departure,
            service_date,
            origin_name: None,
            live: None,
            service: ServiceModeFields::default(),
            day_offset: None,
            working_day_offset: None,
            stops_at_arrival: None,
            stops_at_arrival_day_offset: None,
            stops_at_working_arrival: None,
            stops_at_working_arrival_day_offset: None,
        }
    }
}

impl ScheduleRow for TrainSearchRow {
    fn uid(&self) -> Option<&str> {
        self.departure.uid.as_deref()
    }
    fn origin_crs(&self) -> Option<&str> {
        self.departure.origin_crs.as_deref()
    }
    fn set_origin_name(&mut self, name: Option<String>) {
        self.origin_name = name;
    }
    fn set_live(&mut self, live: Option<LiveSummary>) {
        self.live = live;
    }
    fn set_service_mode(&mut self, mode: ServiceMode) {
        self.service = ServiceModeFields(mode);
    }
}

/// The distinct `uid`s of `rows`, sorted.
fn row_uids<T: ScheduleRow>(rows: &[T]) -> Vec<String> {
    let mut uids: Vec<String> = rows
        .iter()
        .filter_map(ScheduleRow::uid)
        .map(str::to_string)
        .collect();
    uids.sort_unstable();
    uids.dedup();
    uids
}

/// Sets `live` on every row: the line summary's compact live status
/// (`LiveSummary`) for the row's `(uid, service_date)`, or `null` when that
/// train has no live state.
///
/// No live state is the common case: a train TRUST has not activated yet,
/// or one nothing has looked up. A failed read is logged and also renders
/// `null`, so live status can never break the timetable list around it,
/// the same posture as [`annotate_service_modes`].
pub(crate) async fn attach_live<T: ScheduleRow>(
    pool: &PgPool,
    service_date: chrono::NaiveDate,
    rows: &mut [T],
) {
    let uids = row_uids(rows);
    let live = if uids.is_empty() {
        HashMap::new()
    } else {
        match live_summaries(pool, &uids, service_date).await {
            Ok(live) => live,
            Err(err) => {
                tracing::warn!(error = ?err, %service_date, "could not read live state for schedule rows");
                HashMap::new()
            }
        }
    };
    for row in rows.iter_mut() {
        let summary = row.uid().and_then(|uid| live.get(uid)).cloned();
        row.set_live(summary);
    }
}

/// Sets `originName` on every row from its `originCrs` through `names` (the
/// same batched `crs -> name` map the route already reads for
/// `destinationName`); `null` when the row has no `originCrs` or the code
/// has no station name.
pub(crate) fn attach_origin_names<T: ScheduleRow>(rows: &mut [T], names: &HashMap<String, String>) {
    for row in rows.iter_mut() {
        let name = row
            .origin_crs()
            .and_then(|crs| names.get(&crs.to_uppercase()))
            .cloned();
        row.set_origin_name(name);
    }
}

/// Sets every row's `serviceMode`/`liveTracking` from `schedule_services`,
/// with one lookup for the whole page. A database error is logged and
/// leaves every row a train.
pub(crate) async fn annotate_service_modes<T: ScheduleRow>(
    pool: &PgPool,
    service_date: chrono::NaiveDate,
    rows: &mut [T],
) {
    let uids = row_uids(rows);
    let modes =
        crate::data::schedule_services::modes_for_or_trains(pool, service_date, &uids).await;
    for row in rows.iter_mut() {
        let mode = row.uid().map_or(ServiceMode::Train, |uid| {
            crate::data::schedule_services::mode_in(&modes, uid)
        });
        row.set_service_mode(mode);
    }
}

/// Each `uid`'s true origin CRS (the schedule's first calling point) on
/// `service_date`, read from `schedule_destination_departures` in one query
/// over the `(train_uid, service_date, scheduled)` index. A uid whose
/// origin never resolved to a CRS, or that has no rows that day, is absent.
pub(crate) async fn true_origins(
    pool: &PgPool,
    service_date: chrono::NaiveDate,
    uids: &[String],
) -> anyhow::Result<HashMap<String, String>> {
    if uids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT ON (train_uid) train_uid, true_origin_crs \
         FROM schedule_destination_departures \
         WHERE train_uid = ANY($1) AND service_date = $2 \
           AND true_origin_crs IS NOT NULL \
         ORDER BY train_uid",
    )
    .bind(uids)
    .bind(service_date)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// [`true_origins`] for the uids of `rows`, logging and returning an empty
/// map on a failed read (an origin is display-only; the list still loads).
pub(crate) async fn true_origins_for_rows(
    pool: &PgPool,
    service_date: chrono::NaiveDate,
    rows: &[Value],
) -> HashMap<String, String> {
    let mut uids: Vec<String> = rows
        .iter()
        .filter_map(|row| row.get("uid").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    uids.sort_unstable();
    uids.dedup();
    match true_origins(pool, service_date, &uids).await {
        Ok(origins) => origins,
        Err(err) => {
            tracing::warn!(error = ?err, %service_date, "could not read schedule origins");
            HashMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(uid: Option<&str>, origin: Option<&str>) -> TrainSearchRow {
        TrainSearchRow::new(
            CallingPointDepartureJson {
                uid: uid.map(str::to_string),
                scheduled: Some("09:00".to_string()),
                station_crs: "RDG".to_string(),
                origin_crs: origin.map(str::to_string),
                destination_crs: None,
                destination_name: None,
                destination_arrival: None,
                destination_arrival_day_offset: 0,
                public_departure: None,
                public_destination_arrival: None,
                working_departure: None,
                working_destination_arrival: None,
                operator: None,
                identity: None,
            },
            "2026-10-20".to_string(),
        )
    }

    #[test]
    fn origin_name_is_looked_up_case_insensitively_and_null_when_unknown() {
        let mut rows = vec![
            row(Some("A1"), Some("pad")),
            row(Some("A2"), Some("ZZZ")),
            row(Some("A3"), None),
        ];
        let names = HashMap::from([("PAD".to_string(), "London Paddington".to_string())]);
        attach_origin_names(&mut rows, &names);
        let rows = serde_json::to_value(rows).unwrap();
        assert_eq!(rows[0]["originName"], "London Paddington");
        assert!(rows[1]["originName"].is_null());
        assert!(rows[2]["originName"].is_null());
        assert!(rows[2].as_object().unwrap().contains_key("originName"));
    }

    #[test]
    fn row_uids_are_distinct_and_skip_rows_without_one() {
        let rows = vec![
            row(Some("B"), None),
            row(Some("A"), None),
            row(Some("B"), None),
            row(None, None),
        ];
        assert_eq!(row_uids(&rows), vec!["A".to_string(), "B".to_string()]);
    }
}
