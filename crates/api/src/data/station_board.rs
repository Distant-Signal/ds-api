//! The public station board (`GET /public/stations/{crs}/board`, DS-638;
//! docs/station-board.md): the platform and status rules, the settings and
//! the three batched reads that lay live data over a timetable board.
//!
//! **Platform precedence** ([`resolve_platform`]), first match wins:
//!
//! 1. Darwin's platform for the call, unless Darwin hides it from the
//!    public (`platsup`, `cisPlatsup`: `DarwinCall::platform_hidden`, the
//!    rule `pushport_platform` applies; or the location itself is
//!    suppressed): `confirmed` when Darwin says so, else `expected`.
//! 2. Darwin has the call and hides it: `toBeAnnounced`. Never a fallback
//!    to a booked platform, and the hidden string is never read past the
//!    check.
//! 3. The station's own LDBWS board row for the same service
//!    ([`match_ldbws`]), when exactly one row on each side shares the
//!    headcode, shown time and destination: `expected`.
//! 4. The CIF booked platform, `booked`; but at a station in
//!    [`Settings::no_booked_platform_crs`] (the London termini) a booked
//!    platform is never shown and the answer is `toBeAnnounced`.
//! 5. `none`.
//!
//! A call Darwin has cancelled has no platform: `none`.
//!
//! **Status** ([`live_status`]): `booked` means no live data, so a board
//! never claims `onTime` without a Darwin forecast or actual to say so.
//!
//! **Arrivals** (`board=arrivals`, [`BoardKind::Arrivals`]): the same
//! rules over a call's arrival: Darwin's `arrival_at` / `arrival_et`, the
//! working arrival (`wta`) to pick the visit and the CIF booked arrival.
//! The LDBWS step (3) is departures-only: `station_samples` holds the
//! departures board and the arrivals board is read only for tracked trains.

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use anyhow::{Result, bail};
use chrono::{NaiveDate, NaiveTime, Timelike};
use common::StationDeparture;
use ds_store::darwin::DarwinCall;
use serde::Serialize;
use sqlx::PgPool;

/// Turns the route on (chart `api.stationBoard.enabled`, default false).
pub const ENABLED_ENV: &str = "STATION_BOARD_ENABLED";
/// Comma-separated CRS codes of the stations whose booked platform is never
/// shown (chart `api.stationBoard.noBookedPlatformCrs`).
pub const NO_BOOKED_PLATFORM_CRS_ENV: &str = "STATION_BOARD_NO_BOOKED_PLATFORM_CRS";
/// The default of [`NO_BOOKED_PLATFORM_CRS_ENV`]: the six London termini
/// where Darwin hides 92 to 99 per cent of public platforms until they are
/// announced.
pub const DEFAULT_NO_BOOKED_PLATFORM_CRS: [&str; 6] = ["VIC", "EUS", "PAD", "KGX", "MYB", "STP"];

/// How old a station's LDBWS board may be and still name a platform.
pub const LDBWS_MAX_AGE_MINUTES: i64 = 10;

/// The two settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub no_booked_platform_crs: BTreeSet<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            no_booked_platform_crs: DEFAULT_NO_BOOKED_PLATFORM_CRS
                .iter()
                .map(|crs| (*crs).to_string())
                .collect(),
        }
    }
}

impl Settings {
    /// Reads [`ENABLED_ENV`] (default false, as the other switches) and
    /// [`NO_BOOKED_PLATFORM_CRS_ENV`] (unset is the default six; set and
    /// blank is none). A code that is not three letters is an error, so a
    /// typo stops the api instead of quietly showing booked platforms at a
    /// terminus.
    pub fn from_env() -> Result<Self> {
        let enabled = crate::data::trip_plan_live::env_bool(ENABLED_ENV, false);
        let no_booked_platform_crs = match std::env::var(NO_BOOKED_PLATFORM_CRS_ENV) {
            Err(_) => Self::default().no_booked_platform_crs,
            Ok(raw) => parse_crs_list(&raw)?,
        };
        Ok(Self {
            enabled,
            no_booked_platform_crs,
        })
    }

    /// Whether `crs` never shows a booked platform.
    #[must_use]
    pub fn no_booked_platform(&self, crs: &str) -> bool {
        self.no_booked_platform_crs
            .contains(&crs.trim().to_ascii_uppercase())
    }
}

/// A comma-separated list of CRS codes, upper-cased and deduplicated.
pub fn parse_crs_list(raw: &str) -> Result<BTreeSet<String>> {
    let mut codes = BTreeSet::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        if entry.len() != 3 || !entry.chars().all(|c| c.is_ascii_alphabetic()) {
            bail!("{NO_BOOKED_PLATFORM_CRS_ENV}: {entry:?} is not a 3-letter station code");
        }
        codes.insert(entry.to_ascii_uppercase());
    }
    Ok(codes)
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Reads the settings once at startup; an invalid list fails startup.
pub fn init_from_env() -> Result<&'static Settings> {
    let settings = Settings::from_env()?;
    Ok(SETTINGS.get_or_init(|| settings))
}

/// The settings this process runs with ([`init_from_env`]); the defaults
/// (off) when startup never read them.
pub fn settings() -> &'static Settings {
    SETTINGS.get_or_init(|| Settings::from_env().unwrap_or_default())
}

/// What a row's `platform` rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "StationBoardPlatformStatus")]
pub enum BoardPlatformStatus {
    /// Darwin's platform, confirmed by the operator.
    Confirmed,
    /// A live platform (Darwin's, or the station's own board's) not yet
    /// confirmed.
    Expected,
    /// The timetable's platform, with no live word.
    Booked,
    /// Not announced: Darwin hides it, or the station never shows a booked
    /// platform and no live one is public.
    ToBeAnnounced,
    /// No platform known.
    None,
}

/// Where a row stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "StationBoardStatus")]
pub enum BoardStatus {
    Cancelled,
    /// Running late (a forecast or actual at least a minute after the
    /// booked time, or Darwin's "delayed, time unknown").
    Delayed,
    /// A Darwin forecast or actual within the booked minute or earlier.
    OnTime,
    /// No live data: the booked time only.
    Booked,
}

/// Which board: the services leaving the station or the ones arriving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "StationBoardKind")]
pub enum BoardKind {
    #[default]
    Departures,
    Arrivals,
}

impl BoardKind {
    /// The `board` query value: `departures` or `arrivals`, any case; `None`
    /// for anything else.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "departures" => Some(Self::Departures),
            "arrivals" => Some(Self::Arrivals),
            _ => None,
        }
    }
}

/// What Darwin says about the platform of one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DarwinPlatform<'a> {
    pub platform: Option<&'a str>,
    pub confirmed: bool,
    /// Hidden from the public: a suppressed platform or location.
    pub hidden: bool,
    pub cancelled: bool,
}

fn clean(platform: Option<&str>) -> Option<String> {
    platform
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

/// The platform to show and what it rests on. See the module docs; the
/// hidden branch returns before it reads Darwin's platform at all.
#[must_use]
pub fn resolve_platform(
    darwin: Option<DarwinPlatform<'_>>,
    ldbws: Option<&str>,
    booked: Option<&str>,
    no_booked_platform: bool,
) -> (Option<String>, BoardPlatformStatus) {
    if let Some(darwin) = darwin {
        if darwin.cancelled {
            return (None, BoardPlatformStatus::None);
        }
        if darwin.hidden {
            return (None, BoardPlatformStatus::ToBeAnnounced);
        }
        if let Some(platform) = clean(darwin.platform) {
            let status = if darwin.confirmed {
                BoardPlatformStatus::Confirmed
            } else {
                BoardPlatformStatus::Expected
            };
            return (Some(platform), status);
        }
    }
    if let Some(platform) = clean(ldbws) {
        return (Some(platform), BoardPlatformStatus::Expected);
    }
    if no_booked_platform {
        return (None, BoardPlatformStatus::ToBeAnnounced);
    }
    match clean(booked) {
        Some(platform) => (Some(platform), BoardPlatformStatus::Booked),
        None => (None, BoardPlatformStatus::None),
    }
}

/// What Darwin says about the time of one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DarwinTimes {
    pub cancelled: bool,
    /// The actual time of the event the board shows (departure, or arrival
    /// on the arrivals board), else the forecast, as minutes of the day.
    pub expected_minute: Option<u32>,
    /// The actual time is known: the train has left (arrived, on the
    /// arrivals board).
    pub gone: bool,
    /// Darwin's "delayed, time unknown".
    pub delayed_unknown: bool,
}

/// A row's status and delay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveStatus {
    pub status: BoardStatus,
    pub delay_minutes: Option<u32>,
    /// Minutes since the service date's midnight at which the train is now
    /// expected, when Darwin gave a time.
    pub expected_total: Option<u32>,
}

/// Status from Darwin's word on the call and the booked shown time
/// (`shown_total`: minutes since the service date's midnight, day offset
/// included). `None` or no time at all is `booked`.
#[must_use]
pub fn live_status(darwin: Option<DarwinTimes>, shown_total: u32) -> LiveStatus {
    let booked = LiveStatus {
        status: BoardStatus::Booked,
        delay_minutes: None,
        expected_total: None,
    };
    let Some(darwin) = darwin else {
        return booked;
    };
    if darwin.cancelled {
        return LiveStatus {
            status: BoardStatus::Cancelled,
            ..booked
        };
    }
    if let Some(minute) = darwin.expected_minute {
        // The nearest day for the forecast's clock time to the shown one.
        let shown = i64::from(shown_total);
        let delta = (i64::from(minute) - shown.rem_euclid(1440) + 720).rem_euclid(1440) - 720;
        let expected_total = u32::try_from((shown + delta).max(0)).unwrap_or(0);
        let delay = u32::try_from(delta.max(0)).unwrap_or(0);
        return LiveStatus {
            status: if delay >= 1 {
                BoardStatus::Delayed
            } else {
                BoardStatus::OnTime
            },
            delay_minutes: Some(delay),
            expected_total: Some(expected_total),
        };
    }
    if darwin.delayed_unknown {
        return LiveStatus {
            status: BoardStatus::Delayed,
            ..booked
        };
    }
    booked
}

/// A Darwin time (`HH:MM`, or `HH:MM:SS`) as minutes of the day.
#[must_use]
pub fn darwin_minute(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    let time = NaiveTime::parse_from_str(raw, "%H:%M:%S")
        .or_else(|_| NaiveTime::parse_from_str(raw, "%H:%M"))
        .ok()?;
    Some(time.hour() * 60 + time.minute())
}

/// The LDBWS platform for each board row (keyed by position in `rows`) that
/// shares a headcode, shown time (`HH:MM`) and destination with exactly one
/// service on the station's board, where no other row of `rows` shares
/// them. LDBWS's `serviceID` is a board-relative token and its `rsid` is
/// not unique per day, and the schedule rows carry no rsid, so nothing
/// weaker is a safe join: a row without a headcode, or an ambiguous match
/// on either side, gets nothing and falls to the next rule.
#[must_use]
pub fn match_ldbws(board: &[StationDeparture], rows: &[LdbwsKey<'_>]) -> HashMap<usize, String> {
    let mut out = HashMap::new();
    for (index, row) in rows.iter().enumerate() {
        let Some(headcode) = row.headcode.filter(|h| !h.trim().is_empty()) else {
            continue;
        };
        let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
        let on_board: Vec<&StationDeparture> = board
            .iter()
            .filter(|d| {
                d.headcode.as_deref().is_some_and(|h| same(h, headcode))
                    && d.scheduled.trim() == row.shown
                    && same(&d.destination_crs, row.destination_crs)
            })
            .collect();
        let rivals = rows
            .iter()
            .filter(|other| {
                other.headcode.is_some_and(|h| same(h, headcode))
                    && other.shown == row.shown
                    && same(other.destination_crs, row.destination_crs)
            })
            .count();
        if let [only] = on_board.as_slice()
            && rivals == 1
            && !only.is_cancelled
            && let Some(platform) = clean(only.platform.as_deref())
        {
            out.insert(index, platform);
        }
    }
    out
}

/// What [`match_ldbws`] joins a schedule row on.
#[derive(Debug, Clone, Copy)]
pub struct LdbwsKey<'a> {
    pub headcode: Option<&'a str>,
    /// The shown (public) departure, `HH:MM`.
    pub shown: &'a str,
    pub destination_crs: &'a str,
}

/// One Push Port location at the station, as the board reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BoardCall {
    pub uid: String,
    pub ssd: NaiveDate,
    pub cancel_reason: Option<String>,
    #[sqlx(flatten)]
    pub call: DarwinCall,
    pub departure_at: Option<String>,
    pub departure_delayed: bool,
    pub arrival_at: Option<String>,
    pub arrival_delayed: bool,
    pub is_suppressed: bool,
}

impl BoardCall {
    /// Hidden from the public: the platform flags of `DarwinCall`, or the
    /// location itself.
    #[must_use]
    pub fn hidden(&self) -> bool {
        self.call.platform_hidden() || self.is_suppressed
    }

    #[must_use]
    pub fn platform_view(&self) -> DarwinPlatform<'_> {
        DarwinPlatform {
            platform: self.call.platform.as_deref(),
            confirmed: self.call.platform_confirmed,
            hidden: self.hidden(),
            cancelled: self.call.is_cancelled,
        }
    }

    /// What Darwin says about the call's time on `kind`'s board: the
    /// departure, or the arrival.
    #[must_use]
    pub fn times(&self, kind: BoardKind) -> DarwinTimes {
        let (actual, forecast, delayed_unknown) = match kind {
            BoardKind::Departures => (
                self.departure_at.as_deref(),
                self.call.departure_et.as_deref(),
                self.departure_delayed,
            ),
            BoardKind::Arrivals => (
                self.arrival_at.as_deref(),
                self.call.arrival_et.as_deref(),
                self.arrival_delayed,
            ),
        };
        let actual = actual.and_then(darwin_minute);
        DarwinTimes {
            cancelled: self.call.is_cancelled,
            expected_minute: actual.or_else(|| forecast.and_then(darwin_minute)),
            gone: actual.is_some(),
            delayed_unknown,
        }
    }
}

/// One CIF calling point at the station, with its booked platform.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BookedCall {
    pub uid: String,
    pub service_date: NaiveDate,
    pub booked_departure: Option<NaiveTime>,
    pub booked_arrival: Option<NaiveTime>,
    pub platform: Option<String>,
}

/// The call of `calls` that is the visit with the working departure
/// `working` (to the minute); failing that, the only call there is.
pub fn pick_visit<T>(
    calls: &[T],
    working: NaiveTime,
    minute_of: impl Fn(&T) -> Option<u32>,
) -> Option<&T> {
    let wanted = working.hour() * 60 + working.minute();
    calls
        .iter()
        .find(|call| minute_of(call) == Some(wanted))
        .or(match calls {
            [only] => Some(only),
            _ => None,
        })
}

/// The TIPLOCs of `crs` (`tiploc_crs_crs_idx`).
pub const TIPLOCS_SQL: &str = "SELECT tiploc FROM tiploc_crs WHERE UPPER(crs) = $1";

/// The Push Port locations at the station's TIPLOCs for each
/// `(uid, ssd)`: the live service Darwin heard of last and only the
/// locations of its newest schedule, exactly the filter of
/// `ds_store::darwin::calls_for_trains` (whose reasoning applies), but
/// narrowed to the station so a board reads a handful of rows, not every
/// stop of every train. `darwin_services_uid_ssd`, then `darwin_calls`'
/// primary key on `rid`. Never a scan of `darwin_calls` by station.
pub const DARWIN_SQL: &str = "\
WITH k AS (SELECT DISTINCT * FROM UNNEST($1::text[], $2::date[]) AS k(uid, ssd)), \
s AS ( \
    SELECT DISTINCT ON (d.uid, d.ssd) d.rid, d.uid, d.ssd, d.cancel_reason \
    FROM k JOIN darwin_services d ON d.uid = k.uid AND d.ssd = k.ssd \
    WHERE NOT d.is_deleted \
    ORDER BY d.uid, d.ssd, d.message_ts DESC \
) \
SELECT s.uid, s.ssd, s.cancel_reason, c.tiploc, c.wta, c.wtd, c.wtp, c.planned_platform, \
       c.is_cancelled, c.platform, c.platform_confirmed, c.platform_suppressed, \
       c.platform_cis_suppressed, c.platform_basis, c.arrival_et, c.departure_et, \
       c.updated_at, c.departure_at, c.departure_delayed, c.arrival_at, c.arrival_delayed, \
       c.is_suppressed \
FROM s \
JOIN darwin_calls c ON c.rid = s.rid AND c.tiploc = ANY($3::text[]) \
CROSS JOIN LATERAL (SELECT max(x.schedule_ts) AS newest FROM darwin_calls x WHERE x.rid = s.rid) n \
WHERE n.newest IS NULL OR GREATEST(c.schedule_ts, c.ts_ts) >= n.newest";

/// The booked calls at the station's TIPLOCs for each `(uid, service_date)`
/// (`schedule_calling_points_full`'s primary key).
pub const BOOKED_SQL: &str = "\
SELECT p.uid, p.service_date, p.booked_departure, p.booked_arrival, p.platform \
FROM UNNEST($1::text[], $2::date[]) AS k(uid, service_date) \
JOIN schedule_calling_points_full p ON p.service_date = k.service_date AND p.uid = k.uid \
WHERE p.tiploc = ANY($3::text[])";

/// The TIPLOCs of `crs`.
pub async fn tiplocs_for_crs(pool: &PgPool, crs: &str) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(TIPLOCS_SQL)
        .bind(crs.trim().to_ascii_uppercase())
        .fetch_all(pool)
        .await?)
}

/// [`DARWIN_SQL`], by train.
pub async fn darwin_calls_at(
    pool: &PgPool,
    trains: &[(String, NaiveDate)],
    tiplocs: &[String],
) -> Result<HashMap<(String, NaiveDate), Vec<BoardCall>>> {
    if trains.is_empty() || tiplocs.is_empty() {
        return Ok(HashMap::new());
    }
    let uids: Vec<&str> = trains.iter().map(|(uid, _)| uid.as_str()).collect();
    let dates: Vec<NaiveDate> = trains.iter().map(|(_, date)| *date).collect();
    let rows: Vec<BoardCall> = sqlx::query_as(DARWIN_SQL)
        .bind(&uids)
        .bind(&dates)
        .bind(tiplocs)
        .fetch_all(pool)
        .await?;
    let mut out: HashMap<(String, NaiveDate), Vec<BoardCall>> = HashMap::new();
    for row in rows {
        out.entry((row.uid.clone(), row.ssd)).or_default().push(row);
    }
    Ok(out)
}

/// [`BOOKED_SQL`], by train.
pub async fn booked_calls_at(
    pool: &PgPool,
    trains: &[(String, NaiveDate)],
    tiplocs: &[String],
) -> Result<HashMap<(String, NaiveDate), Vec<BookedCall>>> {
    if trains.is_empty() || tiplocs.is_empty() {
        return Ok(HashMap::new());
    }
    let uids: Vec<&str> = trains.iter().map(|(uid, _)| uid.as_str()).collect();
    let dates: Vec<NaiveDate> = trains.iter().map(|(_, date)| *date).collect();
    let rows: Vec<BookedCall> = sqlx::query_as(BOOKED_SQL)
        .bind(&uids)
        .bind(&dates)
        .bind(tiplocs)
        .fetch_all(pool)
        .await?;
    let mut out: HashMap<(String, NaiveDate), Vec<BookedCall>> = HashMap::new();
    for row in rows {
        out.entry((row.uid.clone(), row.service_date))
            .or_default()
            .push(row);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn darwin(platform: Option<&str>, confirmed: bool, hidden: bool) -> DarwinPlatform<'_> {
        DarwinPlatform {
            platform,
            confirmed,
            hidden,
            cancelled: false,
        }
    }

    #[test]
    fn a_confirmed_darwin_platform_is_confirmed() {
        assert_eq!(
            resolve_platform(
                Some(darwin(Some("4"), true, false)),
                Some("9"),
                Some("1"),
                false
            ),
            (Some("4".to_string()), BoardPlatformStatus::Confirmed)
        );
    }

    #[test]
    fn an_unconfirmed_darwin_platform_is_expected_and_beats_the_board_and_the_timetable() {
        assert_eq!(
            resolve_platform(
                Some(darwin(Some("4"), false, false)),
                Some("9"),
                Some("1"),
                false
            ),
            (Some("4".to_string()), BoardPlatformStatus::Expected)
        );
    }

    #[test]
    fn a_hidden_darwin_platform_is_never_shown_and_never_falls_back() {
        // Darwin hides "7": not the board's "9", not the booked "1".
        for no_booked in [false, true] {
            assert_eq!(
                resolve_platform(
                    Some(darwin(Some("7"), true, true)),
                    Some("9"),
                    Some("1"),
                    no_booked
                ),
                (None, BoardPlatformStatus::ToBeAnnounced)
            );
        }
        // Hidden with no platform string at all is the same.
        assert_eq!(
            resolve_platform(Some(darwin(None, false, true)), None, Some("1"), false),
            (None, BoardPlatformStatus::ToBeAnnounced)
        );
    }

    #[test]
    fn a_darwin_call_with_no_platform_falls_to_the_board_then_the_timetable() {
        assert_eq!(
            resolve_platform(
                Some(darwin(None, false, false)),
                Some(" 9 "),
                Some("1"),
                false
            ),
            (Some("9".to_string()), BoardPlatformStatus::Expected)
        );
        assert_eq!(
            resolve_platform(
                Some(darwin(Some(" "), false, false)),
                None,
                Some("1"),
                false
            ),
            (Some("1".to_string()), BoardPlatformStatus::Booked)
        );
    }

    #[test]
    fn without_darwin_the_board_then_the_booked_platform_then_none() {
        assert_eq!(
            resolve_platform(None, Some("9"), Some("1"), false),
            (Some("9".to_string()), BoardPlatformStatus::Expected)
        );
        assert_eq!(
            resolve_platform(None, None, Some("1"), false),
            (Some("1".to_string()), BoardPlatformStatus::Booked)
        );
        assert_eq!(
            resolve_platform(None, None, None, false),
            (None, BoardPlatformStatus::None)
        );
        assert_eq!(
            resolve_platform(None, None, Some(""), false),
            (None, BoardPlatformStatus::None)
        );
    }

    #[test]
    fn a_terminus_never_shows_a_booked_platform() {
        assert_eq!(
            resolve_platform(None, None, Some("1"), true),
            (None, BoardPlatformStatus::ToBeAnnounced)
        );
        // No booked platform either: still to be announced there.
        assert_eq!(
            resolve_platform(None, None, None, true),
            (None, BoardPlatformStatus::ToBeAnnounced)
        );
        // A public live platform still shows.
        assert_eq!(
            resolve_platform(
                Some(darwin(Some("12"), false, false)),
                None,
                Some("1"),
                true
            ),
            (Some("12".to_string()), BoardPlatformStatus::Expected)
        );
        assert_eq!(
            resolve_platform(None, Some("12"), Some("1"), true),
            (Some("12".to_string()), BoardPlatformStatus::Expected)
        );
    }

    #[test]
    fn a_cancelled_call_has_no_platform() {
        let mut cancelled = darwin(Some("4"), true, false);
        cancelled.cancelled = true;
        assert_eq!(
            resolve_platform(Some(cancelled), Some("9"), Some("1"), false),
            (None, BoardPlatformStatus::None)
        );
    }

    fn times(expected: Option<u32>) -> DarwinTimes {
        DarwinTimes {
            cancelled: false,
            expected_minute: expected,
            gone: false,
            delayed_unknown: false,
        }
    }

    #[test]
    fn no_live_data_is_booked_never_on_time() {
        assert_eq!(live_status(None, 600).status, BoardStatus::Booked);
        // A Darwin call with no forecast says nothing either.
        let silent = live_status(Some(times(None)), 600);
        assert_eq!(silent.status, BoardStatus::Booked);
        assert_eq!(silent.delay_minutes, None);
        assert_eq!(silent.expected_total, None);
    }

    #[test]
    fn a_forecast_is_on_time_or_delayed() {
        let on_time = live_status(Some(times(Some(600))), 600);
        assert_eq!(on_time.status, BoardStatus::OnTime);
        assert_eq!(on_time.delay_minutes, Some(0));
        assert_eq!(on_time.expected_total, Some(600));

        let late = live_status(Some(times(Some(607))), 600);
        assert_eq!(late.status, BoardStatus::Delayed);
        assert_eq!(late.delay_minutes, Some(7));
        assert_eq!(late.expected_total, Some(607));

        // Early is on time, with no negative delay.
        let early = live_status(Some(times(Some(598))), 600);
        assert_eq!(early.status, BoardStatus::OnTime);
        assert_eq!(early.delay_minutes, Some(0));
        assert_eq!(early.expected_total, Some(598));
    }

    #[test]
    fn a_forecast_across_midnight_lands_on_the_next_day() {
        // Booked 23:55 (day 0), forecast 00:10: 15 minutes late, day 1.
        let late = live_status(Some(times(Some(10))), 23 * 60 + 55);
        assert_eq!(late.status, BoardStatus::Delayed);
        assert_eq!(late.delay_minutes, Some(15));
        assert_eq!(late.expected_total, Some(1440 + 10));
        // Booked 00:05 on day 1 (1445), forecast 23:58: early, still day 0.
        let early = live_status(Some(times(Some(23 * 60 + 58))), 1445);
        assert_eq!(early.status, BoardStatus::OnTime);
        assert_eq!(early.expected_total, Some(1438));
    }

    #[test]
    fn cancelled_and_delayed_unknown_are_their_own_statuses() {
        let mut cancelled = times(Some(600));
        cancelled.cancelled = true;
        let status = live_status(Some(cancelled), 600);
        assert_eq!(status.status, BoardStatus::Cancelled);
        assert_eq!(status.delay_minutes, None);

        let mut unknown = times(None);
        unknown.delayed_unknown = true;
        let status = live_status(Some(unknown), 600);
        assert_eq!(status.status, BoardStatus::Delayed);
        assert_eq!(status.delay_minutes, None);
        assert_eq!(status.expected_total, None);
    }

    fn ldbws(
        headcode: Option<&str>,
        std: &str,
        dest: &str,
        platform: Option<&str>,
    ) -> StationDeparture {
        StationDeparture {
            service_id: "x".to_string(),
            operator: "GW".to_string(),
            destination_crs: dest.to_string(),
            scheduled: std.to_string(),
            estimated: "On time".to_string(),
            is_cancelled: false,
            delay_minutes: 0,
            cancel_reason: None,
            delay_reason: None,
            headcode: headcode.map(str::to_string),
            skipped_stations: vec![],
            platform: platform.map(str::to_string),
            planned_platform: None,
            rsid: None,
            calling_points: vec![],
        }
    }

    fn key<'a>(headcode: Option<&'a str>, shown: &'a str, dest: &'a str) -> LdbwsKey<'a> {
        LdbwsKey {
            headcode,
            shown,
            destination_crs: dest,
        }
    }

    #[test]
    fn a_board_row_matches_on_headcode_time_and_destination() {
        let board = vec![
            ldbws(Some("1A23"), "10:00", "BRI", Some("3")),
            ldbws(Some("1A24"), "10:00", "BRI", Some("4")),
        ];
        let matched = match_ldbws(&board, &[key(Some("1a23"), "10:00", "bri")]);
        assert_eq!(matched.get(&0).map(String::as_str), Some("3"));
    }

    #[test]
    fn an_ambiguous_or_unsafe_match_is_skipped() {
        // Two board rows with the same key.
        let twice = vec![
            ldbws(Some("1A23"), "10:00", "BRI", Some("3")),
            ldbws(Some("1A23"), "10:00", "BRI", Some("4")),
        ];
        assert!(match_ldbws(&twice, &[key(Some("1A23"), "10:00", "BRI")]).is_empty());
        // Two schedule rows with the same key.
        let once = vec![ldbws(Some("1A23"), "10:00", "BRI", Some("3"))];
        let rows = [
            key(Some("1A23"), "10:00", "BRI"),
            key(Some("1A23"), "10:00", "BRI"),
        ];
        assert!(match_ldbws(&once, &rows).is_empty());
        // No headcode, a different time or destination, no platform.
        assert!(match_ldbws(&once, &[key(None, "10:00", "BRI")]).is_empty());
        assert!(match_ldbws(&once, &[key(Some("1A23"), "10:01", "BRI")]).is_empty());
        assert!(match_ldbws(&once, &[key(Some("1A23"), "10:00", "CDF")]).is_empty());
        let unallocated = vec![ldbws(Some("1A23"), "10:00", "BRI", None)];
        assert!(match_ldbws(&unallocated, &[key(Some("1A23"), "10:00", "BRI")]).is_empty());
    }

    #[test]
    fn the_settings_default_off_with_the_six_termini() {
        let settings = Settings::default();
        assert!(!settings.enabled);
        for crs in DEFAULT_NO_BOOKED_PLATFORM_CRS {
            assert!(settings.no_booked_platform(crs));
        }
        assert!(settings.no_booked_platform(" pad "));
        assert!(!settings.no_booked_platform("RDG"));
    }

    #[test]
    fn the_crs_list_is_validated_upper_cased_and_deduplicated() {
        assert_eq!(
            parse_crs_list("vic, EUS ,vic,,").unwrap(),
            BTreeSet::from(["EUS".to_string(), "VIC".to_string()])
        );
        assert!(parse_crs_list("").unwrap().is_empty());
        assert!(parse_crs_list("VIC,PADD").is_err());
        assert!(parse_crs_list("V1C").is_err());
    }

    #[test]
    fn a_visit_is_picked_by_working_time_else_the_only_call() {
        let calls = [(600_u32, "a"), (700, "b")];
        let at = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        let minute = |c: &(u32, &str)| Some(c.0);
        assert_eq!(
            pick_visit(&calls, at(11, 40), minute).map(|c| c.1),
            Some("b")
        );
        assert_eq!(pick_visit(&calls, at(9, 0), minute), None);
        assert_eq!(
            pick_visit(&calls[..1], at(9, 0), minute).map(|c| c.1),
            Some("a")
        );
    }
}
