//! UK bank holidays (regular journeys phase 5; distant-signal's
//! docs/superpowers/specs/2026-11-06-regular-journeys-design.md §8): the
//! GOV.UK copy in `bank_holidays`, kept by `crate::bank_holidays_sync`, and
//! each user's division (`users.bank_holiday_division`).
//!
//! GOV.UK publishes `https://www.gov.uk/bank-holidays.json` under the Open
//! Government Licence v3.0: one object per division, keyed by
//! `england-and-wales`, `scotland` and `northern-ireland`, each
//! `{division, events: [{title, date, notes, bunting}]}`.
//!
//! A response is applied only when it is plausible ([`parse`]): every
//! division is there, each with at least [`MIN_EVENTS_PER_DIVISION`]
//! events, every date and title readable, no date twice. Anything else
//! changes nothing. A plausible response replaces each division's rows
//! inside the response's own date range for that division ([`apply`]):
//! every event is upserted and every row in the range it no longer lists
//! is deleted, so a moved holiday moves. Rows outside the range (older
//! years GOV.UK has dropped) are kept.

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, NaiveDate, Utc};
use ds_store::bank_holidays::DIVISIONS;
use serde::Deserialize;
use sqlx::PgPool;

/// The fewest events a division may list for a response to be applied
/// (§8.2). A real one lists every bank holiday from 2019 (over 80 each).
pub const MIN_EVENTS_PER_DIVISION: usize = 5;

/// The longest title kept; GOV.UK's longest is under 50 characters.
const MAX_TITLE_CHARS: usize = 200;

/// One bank holiday.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BankHoliday {
    pub date: NaiveDate,
    pub title: String,
}

/// A plausible response: each division's holidays, ascending by date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub divisions: BTreeMap<&'static str, Vec<BankHoliday>>,
}

/// Why a response was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    /// Not the bank holidays JSON.
    Shape(String),
    MissingDivision(&'static str),
    TooFewEvents {
        division: &'static str,
        events: usize,
    },
    BadEvent {
        division: &'static str,
        problem: String,
    },
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Invalid::Shape(err) => write!(f, "not the bank holidays JSON: {err}"),
            Invalid::MissingDivision(division) => write!(f, "division {division} is missing"),
            Invalid::TooFewEvents { division, events } => write!(
                f,
                "division {division} lists {events} events, fewer than {MIN_EVENTS_PER_DIVISION}"
            ),
            Invalid::BadEvent { division, problem } => write!(f, "division {division}: {problem}"),
        }
    }
}

#[derive(Deserialize)]
struct WireDivision {
    events: Vec<WireEvent>,
}

#[derive(Deserialize)]
struct WireEvent {
    title: String,
    date: String,
}

/// Reads a GOV.UK response, and refuses one that isn't plausible (the
/// module docs). Divisions it doesn't know are ignored; `notes` and
/// `bunting` aren't kept.
pub fn parse(body: &[u8]) -> Result<Parsed, Invalid> {
    let wire: HashMap<String, WireDivision> =
        serde_json::from_slice(body).map_err(|err| Invalid::Shape(err.to_string()))?;
    let mut divisions = BTreeMap::new();
    for division in DIVISIONS {
        let events = &wire
            .get(division)
            .ok_or(Invalid::MissingDivision(division))?
            .events;
        if events.len() < MIN_EVENTS_PER_DIVISION {
            return Err(Invalid::TooFewEvents {
                division,
                events: events.len(),
            });
        }
        let bad = |problem: String| Invalid::BadEvent { division, problem };
        let mut seen = HashSet::new();
        let mut holidays = Vec::with_capacity(events.len());
        for event in events {
            let date = NaiveDate::parse_from_str(&event.date, "%Y-%m-%d")
                .map_err(|_| bad(format!("unreadable date {:?}", event.date)))?;
            let title = event.title.trim();
            if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
                return Err(bad(format!("unusable title on {date}")));
            }
            if !seen.insert(date) {
                return Err(bad(format!("{date} is listed twice")));
            }
            holidays.push(BankHoliday {
                date,
                title: title.to_string(),
            });
        }
        holidays.sort_by_key(|holiday| holiday.date);
        divisions.insert(division, holidays);
    }
    Ok(Parsed { divisions })
}

/// What [`apply`] changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Applied {
    /// Rows inserted or updated (every event).
    pub upserted: u64,
    /// Rows inside a division's range that the response no longer lists.
    pub deleted: u64,
}

/// Writes a plausible response in one transaction: per division, upserts
/// every event and deletes the rows between its first and last date that
/// it no longer lists.
pub async fn apply(pool: &PgPool, parsed: &Parsed, now: DateTime<Utc>) -> anyhow::Result<Applied> {
    let mut tx = pool.begin().await?;
    let mut applied = Applied::default();
    for (division, holidays) in &parsed.divisions {
        let (Some(first), Some(last)) = (holidays.first(), holidays.last()) else {
            continue;
        };
        let dates: Vec<NaiveDate> = holidays.iter().map(|holiday| holiday.date).collect();
        let titles: Vec<&str> = holidays
            .iter()
            .map(|holiday| holiday.title.as_str())
            .collect();
        applied.deleted += sqlx::query(
            "DELETE FROM bank_holidays \
             WHERE division = $1 AND holiday_date BETWEEN $2 AND $3 \
               AND NOT (holiday_date = ANY($4))",
        )
        .bind(division)
        .bind(first.date)
        .bind(last.date)
        .bind(&dates)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        applied.upserted += sqlx::query(
            "INSERT INTO bank_holidays (division, holiday_date, title, fetched_at) \
             SELECT $1, d, t, $4 FROM unnest($2::date[], $3::text[]) AS e(d, t) \
             ON CONFLICT (division, holiday_date) \
             DO UPDATE SET title = EXCLUDED.title, fetched_at = EXCLUDED.fetched_at",
        )
        .bind(division)
        .bind(&dates)
        .bind(&titles)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(applied)
}

/// A sync attempt's outcome, as `bank_holidays_sync.last_result` and the
/// `result` label of `api_bank_holidays_sync_total` store it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncResult {
    Ok,
    /// Transport, status, size or body read.
    FetchFailed,
    /// Read, but not plausible ([`Invalid`]).
    Invalid,
}

impl SyncResult {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncResult::Ok => "ok",
            SyncResult::FetchFailed => "fetch_failed",
            SyncResult::Invalid => "invalid",
        }
    }
}

/// `bank_holidays_sync`'s one row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SyncState {
    pub last_attempt_at: DateTime<Utc>,
    pub last_result: String,
    pub last_success_at: Option<DateTime<Utc>>,
}

/// The sync's state, `None` before its first attempt.
pub async fn sync_state(pool: &PgPool) -> anyhow::Result<Option<SyncState>> {
    Ok(sqlx::query_as(
        "SELECT last_attempt_at, last_result, last_success_at FROM bank_holidays_sync",
    )
    .fetch_optional(pool)
    .await?)
}

/// Records an attempt at `now`; an [`SyncResult::Ok`] one is also the
/// last success.
pub async fn record_attempt(
    pool: &PgPool,
    result: SyncResult,
    now: DateTime<Utc>,
) -> anyhow::Result<()> {
    let success = (result == SyncResult::Ok).then_some(now);
    sqlx::query(
        "INSERT INTO bank_holidays_sync (id, last_attempt_at, last_result, last_success_at) \
         VALUES (TRUE, $1, $2, $3) \
         ON CONFLICT (id) DO UPDATE SET last_attempt_at = EXCLUDED.last_attempt_at, \
             last_result = EXCLUDED.last_result, \
             last_success_at = COALESCE(EXCLUDED.last_success_at, \
                                        bank_holidays_sync.last_success_at)",
    )
    .bind(now)
    .bind(result.as_str())
    .bind(success)
    .execute(pool)
    .await?;
    Ok(())
}

/// The latest date known per division (none for a division with no rows).
pub async fn latest_dates(pool: &PgPool) -> anyhow::Result<Vec<(String, NaiveDate)>> {
    Ok(sqlx::query_as(
        "SELECT division, MAX(holiday_date) FROM bank_holidays GROUP BY division ORDER BY 1",
    )
    .fetch_all(pool)
    .await?)
}

/// The user's chosen division, `None` when they haven't chosen (England
/// and Wales is read then).
pub async fn user_division(pool: &PgPool, user_id: &str) -> anyhow::Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT bank_holiday_division FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await?
            .flatten(),
    )
}

/// Sets (or with `None` clears) the user's division. The caller checks it
/// is one of `ds_store::bank_holidays::DIVISIONS`.
pub async fn set_user_division(
    pool: &PgPool,
    user_id: &str,
    division: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE users SET bank_holiday_division = $2 WHERE id = $1")
        .bind(user_id)
        .bind(division)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// GOV.UK's bank holidays JSON as fetched on 2026-11-06 (Open
    /// Government Licence v3.0), so no test reaches GOV.UK.
    pub(crate) const RECORDED: &str =
        include_str!("../../tests/fixtures/gov-uk-bank-holidays-2026-11-06.json");

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn dates(parsed: &Parsed, division: &str) -> Vec<NaiveDate> {
        parsed.divisions[division].iter().map(|h| h.date).collect()
    }

    #[test]
    fn the_recorded_copy_parses() {
        let parsed = parse(RECORDED.as_bytes()).unwrap();
        assert_eq!(
            parsed.divisions.keys().copied().collect::<Vec<_>>(),
            ["england-and-wales", "northern-ireland", "scotland"]
        );
        let england = dates(&parsed, "england-and-wales");
        let scotland = dates(&parsed, "scotland");
        let ni = dates(&parsed, "northern-ireland");
        // Easter Monday 2027 (29 March): England and Wales and Northern
        // Ireland, not Scotland. Scotland's summer bank holiday is the
        // first Monday of August, and its 2nd January falls on Monday 4
        // January in 2027.
        assert!(england.contains(&date(2027, 3, 29)));
        assert!(ni.contains(&date(2027, 3, 29)));
        assert!(!scotland.contains(&date(2027, 3, 29)));
        assert!(scotland.contains(&date(2027, 8, 2)));
        assert!(!england.contains(&date(2027, 8, 2)));
        assert!(scotland.contains(&date(2027, 1, 4)));
        assert!(!england.contains(&date(2027, 1, 4)));
        // St Patrick's Day only in Northern Ireland.
        assert!(ni.contains(&date(2027, 3, 17)));
        assert!(!england.contains(&date(2027, 3, 17)));
        assert!(england.windows(2).all(|w| w[0] < w[1]), "ascending");
        let first = &parsed.divisions["england-and-wales"][0];
        assert_eq!(first.date, date(2019, 1, 1));
        assert_eq!(first.title, "New Year\u{2019}s Day");
    }

    fn without(division: &str) -> String {
        let mut value: serde_json::Value = serde_json::from_str(RECORDED).unwrap();
        value.as_object_mut().unwrap().remove(division);
        value.to_string()
    }

    fn with_events(division: &str, events: serde_json::Value) -> String {
        let mut value: serde_json::Value = serde_json::from_str(RECORDED).unwrap();
        value[division]["events"] = events;
        value.to_string()
    }

    #[test]
    fn a_partial_or_odd_response_is_refused() {
        assert_eq!(
            parse(without("scotland").as_bytes()),
            Err(Invalid::MissingDivision("scotland"))
        );
        let few = serde_json::json!([
            {"title": "New Year's Day", "date": "2027-01-01", "notes": "", "bunting": true}
        ]);
        assert_eq!(
            parse(with_events("northern-ireland", few).as_bytes()),
            Err(Invalid::TooFewEvents {
                division: "northern-ireland",
                events: 1
            })
        );
        let event = |date: &str, title: &str| serde_json::json!({"title": title, "date": date});
        let five = |last: serde_json::Value| {
            serde_json::Value::Array(vec![
                event("2027-01-01", "A"),
                event("2027-04-02", "B"),
                event("2027-05-03", "C"),
                event("2027-05-31", "D"),
                last,
            ])
        };
        for last in [
            event("27 Dec 2027", "E"),
            event("2027-12-27", "  "),
            event("2027-01-01", "E"),
        ] {
            assert!(matches!(
                parse(with_events("england-and-wales", five(last)).as_bytes()),
                Err(Invalid::BadEvent { .. })
            ));
        }
        assert!(
            parse(with_events("england-and-wales", five(event("2027-12-27", "E"))).as_bytes())
                .is_ok()
        );
        for body in ["", "[]", "{\"england-and-wales\": 1}", "<html>"] {
            assert!(parse(body.as_bytes()).is_err(), "{body}");
        }
    }

    #[test]
    fn results_read_as_the_check_constraint_spells_them() {
        assert_eq!(SyncResult::Ok.as_str(), "ok");
        assert_eq!(SyncResult::FetchFailed.as_str(), "fetch_failed");
        assert_eq!(SyncResult::Invalid.as_str(), "invalid");
    }
}
