//! The reads behind the Atom feeds (`routes::feeds`): recent
//! `line_status_history` rows for a set of public lines, and recently
//! changed Knowledgebase incidents. See docs/feeds.md.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

/// One `line_status_history` row, statuses parsed.
#[derive(Debug, Clone)]
pub struct StatusHistoryRow {
    /// `line_status_history.id`: the entry id's stable part.
    pub id: i64,
    pub line_id: String,
    pub statuses: Vec<common::LineStatus>,
    pub computed_at: DateTime<Utc>,
}

/// The history rows of `line_ids` since `since`, newest first, at most
/// `cap` of them, plus (when the cap was not reached) each line's last row
/// before `since`, so the oldest row in the window can be compared with
/// what came before it. Returned oldest first.
///
/// A row whose stored statuses this build can't read is skipped with a
/// warning rather than failing the whole feed.
pub async fn status_history_since(
    pool: &PgPool,
    line_ids: &[String],
    since: DateTime<Utc>,
    cap: i64,
) -> Result<Vec<StatusHistoryRow>> {
    let window = sqlx::query(
        "SELECT id, line_id, statuses, computed_at FROM line_status_history \
          WHERE line_id = ANY($1) AND computed_at >= $2 \
          ORDER BY computed_at DESC, id DESC LIMIT $3",
    )
    .bind(line_ids)
    .bind(since)
    .bind(cap.saturating_add(1))
    .fetch_all(pool)
    .await?;
    let truncated = i64::try_from(window.len()).unwrap_or(i64::MAX) > cap;

    // Each line's row before the window (one index probe per line on
    // `line_status_history_line_time`). Skipped once the cap is hit: the
    // oldest rows fetched are then a line's baseline instead.
    let baselines = if truncated {
        Vec::new()
    } else {
        sqlx::query(
            "SELECT b.id, b.line_id, b.statuses, b.computed_at \
               FROM UNNEST($1::text[]) AS l(line_id) \
               CROSS JOIN LATERAL ( \
                   SELECT h.id, h.line_id, h.statuses, h.computed_at FROM line_status_history h \
                    WHERE h.line_id = l.line_id AND h.computed_at < $2 \
                    ORDER BY h.computed_at DESC, h.id DESC LIMIT 1) b",
        )
        .bind(line_ids)
        .bind(since)
        .fetch_all(pool)
        .await?
    };

    let cap = usize::try_from(cap).unwrap_or(usize::MAX);
    let mut rows: Vec<StatusHistoryRow> = baselines
        .into_iter()
        .chain(window.into_iter().take(cap))
        .filter_map(|row| match parse_history_row(&row) {
            Ok(parsed) => Some(parsed),
            Err(err) => {
                tracing::warn!(error = ?err, "unreadable line_status_history row; skipping it in a feed");
                None
            }
        })
        .collect();
    rows.sort_by_key(|row| (row.computed_at, row.id));
    Ok(rows)
}

fn parse_history_row(row: &sqlx::postgres::PgRow) -> Result<StatusHistoryRow> {
    let statuses: serde_json::Value = row.try_get("statuses")?;
    Ok(StatusHistoryRow {
        id: row.try_get("id")?,
        line_id: row.try_get("line_id")?,
        statuses: serde_json::from_value(statuses)?,
        computed_at: row.try_get("computed_at")?,
    })
}

/// One Knowledgebase incident as a feed entry needs it. Only fields whose
/// changes `updated_at` accounts for are read: the summary, the planned
/// flag, the validity windows and the cleared and no-longer-listed states.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeedIncidentRow {
    pub incident_id: String,
    pub summary: String,
    pub is_planned: bool,
    pub is_cleared: bool,
    pub first_seen_at: DateTime<Utc>,
    pub source_removed_at: Option<DateTime<Utc>>,
    pub validity_periods: serde_json::Value,
    /// The last stored change: first seen, the newest `incident_history`
    /// snapshot (written on every change to the text, validity windows or
    /// cleared flag) or leaving the feed, whichever is latest. A pure
    /// function of stored rows, so it only moves when the incident does.
    pub updated_at: DateTime<Utc>,
}

/// Which incidents a feed lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncidentScope {
    /// Every incident.
    All,
    /// Incidents `common::matcher` matched to this catalogue line.
    Line(String),
    /// Incidents naming any of these ATOC codes.
    Operators(Vec<String>),
}

/// The incidents in `scope` last changed at or after `since`, newest
/// change first, at most `limit`.
pub async fn incidents_changed_since(
    pool: &PgPool,
    scope: &IncidentScope,
    since: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<FeedIncidentRow>> {
    let (line, operators) = match scope {
        IncidentScope::All => (None, None),
        IncidentScope::Line(line) => (Some(line.as_str()), None),
        IncidentScope::Operators(codes) => (None, Some(codes.as_slice())),
    };
    // The raw `fetched_at` is a cheap superset filter: every write that
    // moves `updated_at` (an insert, a content change, the no-longer-listed
    // stamp) also sets `fetched_at` to at least that time.
    let rows = sqlx::query_as::<_, FeedIncidentRow>(
        "SELECT * FROM ( \
             SELECT i.incident_id, i.summary, i.is_planned, i.is_cleared, i.first_seen_at, \
                    i.source_removed_at, i.validity_periods, \
                    GREATEST(i.first_seen_at, h.last_change, i.source_removed_at) AS updated_at \
               FROM incidents i \
               LEFT JOIN LATERAL ( \
                   SELECT MAX(recorded_at) AS last_change FROM incident_history \
                    WHERE incident_id = i.incident_id) h ON TRUE \
              WHERE i.fetched_at >= $3 \
                AND ($1::text IS NULL OR i.affected_lines @> ARRAY[$1::text]) \
                AND ($2::text[] IS NULL OR i.operators && $2) \
         ) recent \
         WHERE updated_at >= $3 \
         ORDER BY updated_at DESC, incident_id DESC \
         LIMIT $4",
    )
    .bind(line)
    .bind(operators)
    .bind(since)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
