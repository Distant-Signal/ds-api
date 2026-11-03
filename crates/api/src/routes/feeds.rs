//! Atom feeds of line status changes and incidents (docs/feeds.md):
//!
//! - `GET /public/feeds/network`: every public National Rail and `TfL` line;
//! - `GET /public/feeds/lines/{id}`: one catalogue or `TfL` line;
//! - `GET /public/feeds/operators/{code}`: one operator's lines, as
//!   `GET /public/operators/{code}` rolls them up.
//!
//! Anonymous and identical for every caller, so they can be cached
//! (`Cache-Control: public`, an `ETag` over the body, `304` on a matching
//! `If-None-Match`). Custom lines are never in a feed: the line universe is
//! the catalogue plus `TfL`-ingested lines only, as for the operator
//! rollups. Rate limited per client IP in the `/Train/by-uid` budget
//! (`rate_limit::classify`).
//!
//! An entry is one change of a line's severity rank (`common::severity_rank`,
//! with `TfL`'s overnight "Service Closed" counted as good service, so a
//! Tube line closing for the night is not news), or one Knowledgebase
//! incident, updated when its stored text, windows or state change. Entry
//! ids are `tag:` URIs built from stored identities (the history row id,
//! the incident id), so they never change once published. The newest
//! [`MAX_ENTRIES`] from the last [`WINDOW_DAYS`] days are listed.
//!
//! Every feed's `<rights>` and every entry's carry the credits its data
//! needs (`Credit`), as the site's footer and `/attribution` do, and the
//! titles say the feed is Distant Signal's and unofficial.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use common::{DataQuality, LineStatus, Severity};
use sha2::{Digest, Sha256};

use crate::app::{App, Router};
use crate::atom::{self, Entry, Feed};
use crate::data::feeds::{self, FeedIncidentRow, IncidentScope, StatusHistoryRow};
use crate::data::{operators, queries};

pub fn router() -> Router {
    Router::new()
        .route("/feeds/network", axum::routing::get(network_feed))
        .route("/feeds/lines/{id}", axum::routing::get(line_feed))
        .route("/feeds/operators/{code}", axum::routing::get(operator_feed))
}

/// Entries per feed, newest first.
pub const MAX_ENTRIES: usize = 50;

/// How far back a feed looks. `line_status_history` is usually kept for
/// less (`history_retention_days`, 7 by default).
pub const WINDOW_DAYS: i64 = 30;

/// History rows read per request at most, a bound on the work one request
/// can cause on the network feed.
const MAX_HISTORY_ROWS: i64 = 5000;

/// `max-age` for the edge and the reader: line status is recomputed every
/// few minutes, and readers poll far less often.
pub const CACHE_CONTROL: &str = "public, max-age=300";

pub const CONTENT_TYPE: &str = "application/atom+xml; charset=utf-8";

/// The date in every `tag:` URI. Fixed: changing it would change every id.
const TAG_DATE: &str = "2026";

/// Who published the feed: always Distant Signal, never a data source.
const AUTHOR: &str = "Distant Signal";

/// `frontend/components/OpenDataAttribution.tsx`'s
/// `NON_AFFILIATION_STATEMENT`, verbatim.
pub const NON_AFFILIATION_STATEMENT: &str = "Distant Signal is an independent, unofficial service and is not affiliated with or endorsed by National Rail, Network Rail, Rail Delivery Group, TfL, the National Transport Authority, Iarnród Éireann, Translink or any train operator.";

const NATIONAL_RAIL_URL: &str = "https://www.nationalrail.co.uk";
const NRIL_LICENCE_URL: &str = "https://www.networkrail.co.uk/who-we-are/transparency-and-ethics/transparency/open-data-feeds/network-rail-infrastructure-limited-data-feeds-licence/";

/// A credit a feed or entry carries. The wording of the first four is the
/// licences' own, as `frontend/components/OpenDataAttribution.tsx` and
/// `frontend/lib/tflAttribution.ts` render it; the last two mark what
/// Distant Signal itself worked out or wrote.
///
/// `TfL`'s Ordnance Survey and Geomni statements are not needed: they cover
/// `TfL`'s geographic data (stop locations), and no feed carries any.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Credit {
    /// Darwin (the departure data behind National Rail line status) and
    /// Knowledgebase Stations: both required strings, on one line.
    NationalRail,
    /// Knowledgebase incidents, published by Rail Delivery Group.
    KnowledgebaseIncidents,
    /// TRUST, behind full-coverage line status (NRIL's prescribed statement).
    NetworkRail,
    /// `TfL`'s line status (its required statement).
    Tfl,
    /// A National Rail line's status is Distant Signal's own reading of the
    /// data, never an official status.
    DsStatus,
    /// A delay summary Distant Signal wrote from live data.
    DsSummary,
}

impl Credit {
    pub fn text(self) -> String {
        match self {
            Self::NationalRail => {
                format!("powered by NationalRail (Train Information Services Ltd): {NATIONAL_RAIL_URL}")
            }
            Self::KnowledgebaseIncidents => {
                "Incident text from the National Rail Knowledgebase, published by Rail Delivery Group."
                    .to_string()
            }
            Self::NetworkRail => format!(
                "Contains Information of Network Rail Infrastructure Limited licensed under the following licence: {NRIL_LICENCE_URL}"
            ),
            Self::Tfl => "Powered by TfL Open Data".to_string(),
            Self::DsStatus => {
                "Line status worked out by Distant Signal from this data; not an official status."
                    .to_string()
            }
            Self::DsSummary => {
                "Delay summary written by Distant Signal from live departure and train movement data."
                    .to_string()
            }
        }
    }

    /// Every credit a National Rail line's entries can need.
    const NATIONAL_RAIL_LINE: [Self; 5] = [
        Self::NationalRail,
        Self::KnowledgebaseIncidents,
        Self::NetworkRail,
        Self::DsStatus,
        Self::DsSummary,
    ];
}

/// `credits` as one statement, in a fixed order.
pub fn credits_text(credits: &BTreeSet<Credit>) -> String {
    credits
        .iter()
        .map(|credit| credit.text())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whose line it is, which decides its credits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineSource {
    /// A line in the catalogue: status worked out by Distant Signal from
    /// National Rail data.
    NationalRail,
    /// A line whose status `TfL` publishes.
    Tfl,
}

/// A line a feed covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedLine {
    pub name: String,
    pub source: LineSource,
}

/// What one feed covers, resolved from its path.
#[derive(Debug, Clone)]
pub struct FeedScope {
    /// After `/feeds/`: `network`, `lines/{id}`, `operators/{code}`.
    pub path: String,
    pub title: String,
    /// The site page the feed follows.
    pub page_path: String,
    pub lines: HashMap<String, FeedLine>,
    /// `None`: the feed lists no incidents (a `TfL` line's).
    pub incidents: Option<IncidentScope>,
}

impl FeedScope {
    /// The feed-level credits: every credit any of its entries can carry.
    fn credits(&self) -> BTreeSet<Credit> {
        let mut credits = BTreeSet::new();
        for line in self.lines.values() {
            match line.source {
                LineSource::NationalRail => credits.extend(Credit::NATIONAL_RAIL_LINE),
                LineSource::Tfl => {
                    credits.insert(Credit::Tfl);
                }
            }
        }
        if self.incidents.is_some() {
            credits.extend([Credit::NationalRail, Credit::KnowledgebaseIncidents]);
        }
        credits
    }
}

/// The site's public origin and the `tag:` authority derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// `https://host[:port]`, no trailing slash.
    pub origin: String,
    /// The host alone.
    pub host: String,
}

impl Site {
    /// From `sso_redirect_url`, the frontend's own public URL (as
    /// `auth::expected_browser_origin` uses it).
    pub fn from_redirect_url(sso_redirect_url: &str) -> Option<Self> {
        let origin = crate::auth::expected_browser_origin(sso_redirect_url)?;
        let authority = origin.split_once("://")?.1;
        let host = authority
            .rsplit_once(':')
            .map_or(authority, |(host, _port)| host)
            .to_string();
        (!host.is_empty() && origin != "null").then_some(Self { origin, host })
    }

    fn tag(&self, specific: &str) -> String {
        format!("tag:{},{TAG_DATE}:{specific}", self.host)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }
}

/// `segment` percent-encoded for a URL path or a `tag:` URI: everything
/// but RFC 3986's unreserved characters.
pub fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// A line's level for change detection: its worst status's rank, with
/// `TfL`'s overnight closure counted as good service.
fn feed_rank(severity: Severity) -> u8 {
    if severity == Severity::ServiceClosed {
        0
    } else {
        common::severity_rank(severity)
    }
}

/// The worst status, as `LineStatusReport::worst_severity` picks it.
fn worst_severity(statuses: &[LineStatus]) -> Severity {
    statuses
        .iter()
        .map(|status| status.severity)
        .max_by_key(|&severity| common::severity_rank(severity))
        .unwrap_or(Severity::GoodService)
}

/// One severity change on one line.
#[derive(Debug, Clone)]
pub struct StatusChange<'a> {
    pub row: &'a StatusHistoryRow,
    pub previous: Severity,
    pub current: Severity,
}

/// The rows (oldest first) where a line's [`feed_rank`] differs from its
/// previous row's. A line's first row is its baseline, never a change.
pub fn status_changes(rows: &[StatusHistoryRow]) -> Vec<StatusChange<'_>> {
    let mut last: HashMap<&str, Severity> = HashMap::new();
    let mut changes = Vec::new();
    for row in rows {
        let current = worst_severity(&row.statuses);
        if let Some(previous) = last.insert(row.line_id.as_str(), current)
            && feed_rank(previous) != feed_rank(current)
        {
            changes.push(StatusChange {
                row,
                previous,
                current,
            });
        }
    }
    changes
}

/// The credits of one status change's entry.
fn status_credits(source: LineSource, statuses: &[LineStatus]) -> BTreeSet<Credit> {
    let mut credits = BTreeSet::new();
    if source == LineSource::Tfl {
        credits.insert(Credit::Tfl);
        return credits;
    }
    credits.extend([Credit::NationalRail, Credit::DsStatus]);
    for status in statuses {
        match status.data_quality {
            DataQuality::Knowledgebase | DataQuality::Planned
                if common::severity_rank(status.severity) > 0 =>
            {
                credits.insert(Credit::KnowledgebaseIncidents);
            }
            DataQuality::LdbwsInferred => {
                credits.insert(Credit::DsSummary);
            }
            DataQuality::TrustInferred => {
                credits.extend([Credit::DsSummary, Credit::NetworkRail]);
            }
            DataQuality::Tfl => {
                credits.insert(Credit::Tfl);
            }
            _ => {}
        }
    }
    credits
}

fn paragraph(text: &str) -> String {
    format!("<p>{}</p>", atom::escape_text(text))
}

fn sources_paragraph(site: &Site, credits: &BTreeSet<Credit>) -> String {
    format!(
        "<p>Sources: {} <a href=\"{}\">Data sources and licences</a>.</p>",
        atom::escape_text(&credits_text(credits)),
        atom::escape_text(&site.url("/attribution"))
    )
}

/// The entry for one severity change.
pub fn status_entry(site: &Site, line: &FeedLine, change: &StatusChange<'_>) -> Entry {
    let row = change.row;
    let headline = format!(
        "Now {}, was {}.",
        change.current.description(),
        change.previous.description()
    );
    let credits = status_credits(line.source, &row.statuses);
    let mut content = paragraph(&headline);
    for status in &row.statuses {
        let reason = status.reason.trim();
        if !reason.is_empty() && reason != status.severity.description() {
            content.push_str(&paragraph(reason));
        }
    }
    content.push_str(&sources_paragraph(site, &credits));
    Entry {
        id: site.tag(&format!(
            "line-status/{}/{}",
            encode_segment(&row.line_id),
            row.id
        )),
        title: format!("{}: {}", line.name, change.current.description()),
        published: row.computed_at,
        updated: row.computed_at,
        link: site.url(&format!("/lines/{}", encode_segment(&row.line_id))),
        category: ("line-status".to_string(), "Line status".to_string()),
        summary: format!("{}: {headline}", line.name),
        content_html: content,
        rights: credits_text(&credits),
    }
}

/// "3 Nov 2026 14:05", London time.
fn london_time(at: DateTime<Utc>) -> String {
    at.with_timezone(&chrono_tz::Europe::London)
        .format("%-d %b %Y %H:%M")
        .to_string()
}

/// The entry for one incident.
pub fn incident_entry(site: &Site, incident: &FeedIncidentRow) -> Entry {
    let kind = if incident.is_planned {
        "Planned works"
    } else {
        "Incident"
    };
    let summary = incident.summary.trim();
    let mut content = paragraph(summary);
    let periods = queries::parse_validity_periods(&incident.validity_periods);
    for period in &periods {
        let window = match period.to_date {
            Some(to) => format!(
                "From {} to {}.",
                london_time(period.from_date),
                london_time(to)
            ),
            None => format!("From {}.", london_time(period.from_date)),
        };
        content.push_str(&paragraph(&window));
    }
    if incident.is_cleared {
        content.push_str(&paragraph("Cleared."));
    } else if incident.source_removed_at.is_some() {
        content.push_str(&paragraph("No longer listed by National Rail."));
    }
    let credits = BTreeSet::from([Credit::NationalRail, Credit::KnowledgebaseIncidents]);
    content.push_str(&sources_paragraph(site, &credits));
    Entry {
        id: site.tag(&format!(
            "incident/{}",
            encode_segment(&incident.incident_id)
        )),
        title: format!("{kind}: {summary}"),
        published: incident.first_seen_at,
        updated: incident.updated_at,
        link: site.url(&format!(
            "/incidents/{}",
            encode_segment(&incident.incident_id)
        )),
        category: if incident.is_planned {
            ("planned-works".to_string(), "Planned works".to_string())
        } else {
            ("incident".to_string(), "Incident".to_string())
        },
        summary: summary.to_string(),
        content_html: content,
        rights: credits_text(&credits),
    }
}

/// The whole feed: the newest [`MAX_ENTRIES`] of the status changes and
/// incidents, newest first (ties by id, so the order is stable).
pub fn build_feed(
    site: &Site,
    scope: &FeedScope,
    history: &[StatusHistoryRow],
    incidents: &[FeedIncidentRow],
) -> Feed {
    let mut entries: Vec<Entry> = status_changes(history)
        .iter()
        .filter_map(|change| {
            let line = scope.lines.get(&change.row.line_id)?;
            Some(status_entry(site, line, change))
        })
        .chain(
            incidents
                .iter()
                .map(|incident| incident_entry(site, incident)),
        )
        .collect();
    entries.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
    entries.truncate(MAX_ENTRIES);
    let updated = entries
        .iter()
        .map(|entry| entry.updated)
        .max()
        .unwrap_or(DateTime::UNIX_EPOCH);
    let mut rights = credits_text(&scope.credits());
    rights.push(' ');
    rights.push_str(NON_AFFILIATION_STATEMENT);
    let _ = write!(rights, " Full credits: {}.", site.url("/attribution"));
    Feed {
        id: site.tag(&format!("feeds/{}", scope.path)),
        title: format!("{} (unofficial, from Distant Signal)", scope.title),
        subtitle: format!(
            "Line status changes and incidents, from Distant Signal. {NON_AFFILIATION_STATEMENT}"
        ),
        updated,
        self_url: site.url(&format!("/api/feeds/{}", scope.path)),
        alternate_url: site.url(&scope.page_path),
        rights,
        author_name: AUTHOR.to_string(),
        author_uri: site.url("/"),
        entries,
    }
}

/// A strong `ETag` over the document.
pub fn etag(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    let mut tag = String::from("\"");
    for byte in &digest[..16] {
        let _ = write!(tag, "{byte:02x}");
    }
    tag.push('"');
    tag
}

/// Whether `If-None-Match` names `etag` (or is `*`). Weak comparison, as
/// RFC 9110 asks for this header.
pub fn if_none_match_hits(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
        })
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "used as a map_err callback, which passes the error by value"
)]
fn internal_error(err: anyhow::Error) -> Response {
    if let Some(unavailable) = crate::unavailable::response_for(&err) {
        return unavailable.into_response();
    }
    tracing::error!(error = ?err, "feed query failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "query failed").into_response()
}

fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, format!("no such {what}")).into_response()
}

/// Reads the scope's data and answers with the document, or `304`.
async fn respond(app: &App, headers: &HeaderMap, scope: &FeedScope) -> Response {
    let Some(site) = Site::from_redirect_url(&app.config.sso_redirect_url) else {
        tracing::error!("SSO_REDIRECT_URL has no usable origin; feeds are unavailable");
        return (StatusCode::INTERNAL_SERVER_ERROR, "feeds unavailable").into_response();
    };
    let since = Utc::now() - chrono::Duration::days(WINDOW_DAYS);
    let line_ids: Vec<String> = scope.lines.keys().cloned().collect();
    let history = if line_ids.is_empty() {
        Vec::new()
    } else {
        match feeds::status_history_since(&app.database, &line_ids, since, MAX_HISTORY_ROWS).await {
            Ok(rows) => rows,
            Err(err) => return internal_error(err),
        }
    };
    let incidents = match &scope.incidents {
        None => Vec::new(),
        Some(incident_scope) => {
            let limit = i64::try_from(MAX_ENTRIES).unwrap_or(i64::MAX);
            match feeds::incidents_changed_since(&app.database, incident_scope, since, limit).await
            {
                Ok(rows) => rows,
                Err(err) => return internal_error(err),
            }
        }
    };
    let body = atom::render(&build_feed(&site, scope, &history, &incidents));
    let tag = etag(&body);
    let mut response = if if_none_match_hits(headers, &tag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, CONTENT_TYPE)], body).into_response()
    };
    let out = response.headers_mut();
    out.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL),
    );
    if let Ok(value) = HeaderValue::from_str(&tag) {
        out.insert(header::ETAG, value);
    }
    response
}

/// Every public line: the catalogue plus the `TfL` lines not merged into a
/// catalogue line (`operators::public_line_status_rows`, the operator
/// rollups' universe). Names from the catalogue first.
async fn public_lines(app: &App) -> anyhow::Result<HashMap<String, FeedLine>> {
    let mut lines: HashMap<String, FeedLine> = app
        .config
        .lines
        .iter()
        .map(|line| {
            (
                line.id.clone(),
                FeedLine {
                    name: line.name.clone(),
                    source: LineSource::NationalRail,
                },
            )
        })
        .collect();
    for row in operators::public_line_status_rows(&app.database, &app.config.lines).await? {
        lines.entry(row.id).or_insert(FeedLine {
            name: row.name,
            source: LineSource::Tfl,
        });
    }
    Ok(lines)
}

async fn network_feed(State(app): State<App>, headers: HeaderMap) -> Response {
    let lines = match public_lines(&app).await {
        Ok(lines) => lines,
        Err(err) => return internal_error(err),
    };
    let scope = FeedScope {
        path: "network".to_string(),
        title: "UK rail line status".to_string(),
        page_path: "/status".to_string(),
        lines,
        incidents: Some(IncidentScope::All),
    };
    respond(&app, &headers, &scope).await
}

async fn line_feed(State(app): State<App>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    let line = if let Some(line) = app.config.lines.iter().find(|line| line.id == id) {
        FeedLine {
            name: line.name.clone(),
            source: LineSource::NationalRail,
        }
    } else {
        // Any `TfL`-ingested line, merged or not. Never a custom or
        // transit line: neither is in `tfl_line_summaries`.
        match queries::tfl_line_summaries(&app.database).await {
            Ok(summaries) => match summaries.into_iter().find(|summary| summary.id == id) {
                Some(summary) => FeedLine {
                    name: summary.name,
                    source: LineSource::Tfl,
                },
                None => return not_found("line"),
            },
            Err(err) => return internal_error(err),
        }
    };
    let incidents =
        (line.source == LineSource::NationalRail).then(|| IncidentScope::Line(id.clone()));
    let scope = FeedScope {
        path: format!("lines/{}", encode_segment(&id)),
        title: format!("{} line status", line.name),
        page_path: format!("/lines/{}", encode_segment(&id)),
        lines: HashMap::from([(id, line)]),
        incidents,
    };
    respond(&app, &headers, &scope).await
}

/// The ATOC codes whose incidents an operator's feed lists: the code, or
/// for the synthetic `TfL` operator the codes its rollup folds in.
fn incident_operator_codes(code: &str) -> Vec<String> {
    if code == common::TFL_OPERATOR {
        operators::TFL_ADJACENT_OPERATORS
            .iter()
            .map(|code| (*code).to_string())
            .collect()
    } else {
        vec![code.to_string()]
    }
}

async fn operator_feed(
    State(app): State<App>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Response {
    let rollup = match operators::operator_rollup(&app.database, &app.config.lines, &code).await {
        Ok(Some(rollup)) => rollup,
        Ok(None) => return not_found("operator"),
        Err(err) => return internal_error(err),
    };
    let all = match public_lines(&app).await {
        Ok(lines) => lines,
        Err(err) => return internal_error(err),
    };
    let lines = rollup
        .line_ids
        .iter()
        .filter_map(|id| Some((id.clone(), all.get(id)?.clone())))
        .collect();
    let scope = FeedScope {
        path: format!("operators/{}", encode_segment(&code)),
        title: format!("{} line status", rollup.name),
        page_path: format!("/operators/{}/history", encode_segment(&code)),
        lines,
        incidents: Some(IncidentScope::Operators(incident_operator_codes(&code))),
    };
    respond(&app, &headers, &scope).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use quick_xml::events::Event;

    fn site() -> Site {
        Site::from_redirect_url("https://rail.example.org/api/auth/callback").unwrap()
    }

    fn at(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 3, 9, minute, 0).unwrap()
    }

    fn status(severity: Severity, reason: &str, quality: DataQuality) -> LineStatus {
        LineStatus {
            severity,
            reason: reason.to_string(),
            validity: common::ValidityPeriod {
                from_date: at(0),
                to_date: None,
                is_now: true,
            },
            disruption: None,
            data_quality: quality,
            sample_stats: None,
            sample_availability: common::SampleAvailability::no_coverage_default(),
            full_coverage_stats: None,
            full_coverage_availability: common::FullCoverageAvailability::not_enabled_default(),
        }
    }

    fn row(id: i64, line_id: &str, minute: u32, statuses: Vec<LineStatus>) -> StatusHistoryRow {
        StatusHistoryRow {
            id,
            line_id: line_id.to_string(),
            statuses,
            computed_at: at(minute),
        }
    }

    fn good() -> Vec<LineStatus> {
        vec![status(
            Severity::GoodService,
            "Good Service",
            DataQuality::Knowledgebase,
        )]
    }

    fn incident(id: &str, minute: u32) -> FeedIncidentRow {
        FeedIncidentRow {
            incident_id: id.to_string(),
            summary: "Signalling fault <between> A & B".to_string(),
            is_planned: false,
            is_cleared: false,
            first_seen_at: at(minute),
            source_removed_at: None,
            validity_periods: serde_json::json!([
                {"from_date": "2026-11-03T09:00:00Z", "to_date": "2026-11-03T18:00:00Z", "is_now": true}
            ]),
            updated_at: at(minute),
        }
    }

    fn scope() -> FeedScope {
        FeedScope {
            path: "network".to_string(),
            title: "UK rail line status".to_string(),
            page_path: "/status".to_string(),
            lines: HashMap::from([
                (
                    "swr-main".to_string(),
                    FeedLine {
                        name: "South Western main line".to_string(),
                        source: LineSource::NationalRail,
                    },
                ),
                (
                    "tfl-victoria".to_string(),
                    FeedLine {
                        name: "Victoria".to_string(),
                        source: LineSource::Tfl,
                    },
                ),
            ]),
            incidents: Some(IncidentScope::All),
        }
    }

    fn history() -> Vec<StatusHistoryRow> {
        vec![
            row(1, "swr-main", 0, good()),
            row(
                2,
                "tfl-victoria",
                1,
                vec![status(
                    Severity::GoodService,
                    "Good Service",
                    DataQuality::Tfl,
                )],
            ),
            row(
                3,
                "swr-main",
                5,
                vec![status(
                    Severity::MinorDelays,
                    "Delays of up to 15 minutes.",
                    DataQuality::LdbwsInferred,
                )],
            ),
            row(
                4,
                "tfl-victoria",
                6,
                vec![status(
                    Severity::SevereDelays,
                    "Signal failure.",
                    DataQuality::Tfl,
                )],
            ),
            row(5, "swr-main", 10, good()),
        ]
    }

    fn rendered() -> String {
        atom::render(&build_feed(
            &site(),
            &scope(),
            &history(),
            &[incident("ABC123", 7)],
        ))
    }

    /// Every element and its text, in document order, parsed with
    /// quick-xml: a well-formedness check and a lookup in one.
    fn parse(xml: &str) -> Vec<(Vec<String>, String)> {
        let mut reader = quick_xml::Reader::from_str(xml);
        let mut path: Vec<String> = Vec::new();
        let mut out = Vec::new();
        loop {
            match reader.read_event().expect("well-formed XML") {
                Event::Start(start) => {
                    path.push(String::from_utf8(start.name().as_ref().to_vec()).unwrap());
                    out.push((path.clone(), String::new()));
                }
                Event::Empty(start) => {
                    let mut empty = path.clone();
                    empty.push(String::from_utf8(start.name().as_ref().to_vec()).unwrap());
                    out.push((empty, String::new()));
                }
                Event::Text(text) => {
                    let text = text.decode().unwrap();
                    let unescaped = quick_xml::escape::unescape(&text).unwrap().into_owned();
                    if let Some(last) = out.iter_mut().rev().find(|(p, _)| *p == path) {
                        last.1.push_str(&unescaped);
                    }
                }
                Event::GeneralRef(reference) => {
                    let name = reference.decode().unwrap();
                    let resolved = quick_xml::escape::unescape(&format!("&{name};"))
                        .unwrap()
                        .into_owned();
                    if let Some(last) = out.iter_mut().rev().find(|(p, _)| *p == path) {
                        last.1.push_str(&resolved);
                    }
                }
                Event::End(_) => {
                    path.pop();
                }
                Event::Eof => break,
                _ => {}
            }
        }
        assert!(path.is_empty(), "every element closed");
        out
    }

    fn texts<'a>(parsed: &'a [(Vec<String>, String)], path: &[&str]) -> Vec<&'a str> {
        parsed
            .iter()
            .filter(|(p, _)| p.iter().map(String::as_str).eq(path.iter().copied()))
            .map(|(_, text)| text.as_str())
            .collect()
    }

    #[test]
    fn the_feed_is_well_formed_atom_with_its_required_elements() {
        let xml = rendered();
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"utf-8\"?>"));
        assert!(xml.contains("<feed xmlns=\"http://www.w3.org/2005/Atom\""));
        let parsed = parse(&xml);
        for required in ["id", "title", "updated", "rights", "author"] {
            assert_eq!(
                texts(&parsed, &["feed", required]).len(),
                1,
                "feed/{required}"
            );
        }
        assert_eq!(
            texts(&parsed, &["feed", "author", "name"]),
            ["Distant Signal"]
        );
        assert_eq!(
            texts(&parsed, &["feed", "id"]),
            ["tag:rail.example.org,2026:feeds/network"]
        );
        assert_eq!(
            texts(&parsed, &["feed", "updated"]),
            ["2026-11-03T09:10:00Z"]
        );
        let entries = texts(&parsed, &["feed", "entry"]).len();
        // swr-main: good -> minor (5), minor -> good (10); victoria: good ->
        // severe (6); one incident (7). The first row of each line is its
        // baseline.
        assert_eq!(entries, 4);
        for required in [
            "id",
            "title",
            "updated",
            "published",
            "link",
            "content",
            "rights",
        ] {
            assert_eq!(
                texts(&parsed, &["feed", "entry", required]).len(),
                entries,
                "entry/{required}"
            );
        }
        assert_eq!(
            texts(&parsed, &["feed", "entry", "updated"]),
            [
                "2026-11-03T09:10:00Z",
                "2026-11-03T09:07:00Z",
                "2026-11-03T09:06:00Z",
                "2026-11-03T09:05:00Z"
            ],
            "newest first"
        );
        assert_eq!(
            texts(&parsed, &["feed", "entry", "title"]),
            [
                "South Western main line: Good Service",
                "Incident: Signalling fault <between> A & B",
                "Victoria: Severe Delays",
                "South Western main line: Minor Delays",
            ]
        );
    }

    #[test]
    fn every_feed_and_entry_carries_its_credits() {
        let parsed = parse(&rendered());
        let rights = texts(&parsed, &["feed", "rights"])[0];
        for credit in [
            "powered by NationalRail (Train Information Services Ltd)",
            "Rail Delivery Group",
            "Powered by TfL Open Data",
            "Contains Information of Network Rail Infrastructure Limited licensed under the following licence",
            NON_AFFILIATION_STATEMENT,
            "https://rail.example.org/attribution",
        ] {
            assert!(
                rights.contains(credit),
                "feed rights lack {credit:?}: {rights}"
            );
        }
        let entry_rights = texts(&parsed, &["feed", "entry", "rights"]);
        let contents = texts(&parsed, &["feed", "entry", "content"]);
        // Good service again (DS's reading of NR data).
        assert!(
            entry_rights[0].contains("powered by NationalRail (Train Information Services Ltd)")
        );
        assert!(entry_rights[0].contains("not an official status"));
        // The incident: National Rail and RDG.
        assert!(
            entry_rights[1].contains("powered by NationalRail (Train Information Services Ltd)")
        );
        assert!(entry_rights[1].contains("Rail Delivery Group"));
        // The TfL line: TfL's credit, and nothing claiming it as DS's.
        assert_eq!(entry_rights[2], "Powered by TfL Open Data");
        // The LDBWS-inferred delay: marked as DS's own summary.
        assert!(entry_rights[3].contains("Delay summary written by Distant Signal"));
        assert!(entry_rights[3].contains("powered by NationalRail"));
        for (content, rights) in contents.iter().zip(&entry_rights) {
            assert!(content.contains(rights), "content repeats the credits");
        }
    }

    #[test]
    fn titles_never_read_as_an_official_source() {
        let parsed = parse(&rendered());
        let title = texts(&parsed, &["feed", "title"])[0];
        assert_eq!(
            title,
            "UK rail line status (unofficial, from Distant Signal)"
        );
        assert!(texts(&parsed, &["feed", "subtitle"])[0].contains(NON_AFFILIATION_STATEMENT));
        assert!(!rendered().contains("<icon>") && !rendered().contains("<logo>"));
    }

    #[test]
    fn entry_ids_are_stable_and_do_not_depend_on_position() {
        let site = site();
        let first: Vec<String> = build_feed(&site, &scope(), &history(), &[incident("ABC123", 7)])
            .entries
            .into_iter()
            .map(|entry| entry.id)
            .collect();
        assert_eq!(
            first,
            [
                "tag:rail.example.org,2026:line-status/swr-main/5",
                "tag:rail.example.org,2026:incident/ABC123",
                "tag:rail.example.org,2026:line-status/tfl-victoria/4",
                "tag:rail.example.org,2026:line-status/swr-main/3",
            ]
        );

        // A later change and an edited incident: the existing ids are
        // unchanged, the incident keeps its id with a new `updated`.
        let mut later = history();
        later.push(row(
            9,
            "swr-main",
            20,
            vec![status(
                Severity::Suspended,
                "Line blocked.",
                DataQuality::Knowledgebase,
            )],
        ));
        let mut edited = incident("ABC123", 7);
        edited.updated_at = at(15);
        let second = build_feed(&site, &scope(), &later, &[edited]).entries;
        let ids: Vec<&str> = second.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "tag:rail.example.org,2026:line-status/swr-main/9",
                "tag:rail.example.org,2026:incident/ABC123",
                "tag:rail.example.org,2026:line-status/swr-main/5",
                "tag:rail.example.org,2026:line-status/tfl-victoria/4",
                "tag:rail.example.org,2026:line-status/swr-main/3",
            ]
        );
        assert_eq!(second[1].published, at(7));
        assert_eq!(second[1].updated, at(15));
        // Same input, same bytes: the ETag only moves when the data does.
        let again = atom::render(&build_feed(
            &site,
            &scope(),
            &history(),
            &[incident("ABC123", 7)],
        ));
        assert_eq!(etag(&rendered()), etag(&again));
    }

    #[test]
    fn only_rank_changes_are_entries_and_overnight_closure_is_not_news() {
        let rows = vec![
            row(
                1,
                "tfl-victoria",
                0,
                vec![status(Severity::GoodService, "", DataQuality::Tfl)],
            ),
            row(
                2,
                "tfl-victoria",
                1,
                vec![status(Severity::ServiceClosed, "", DataQuality::Tfl)],
            ),
            row(
                3,
                "tfl-victoria",
                2,
                vec![status(Severity::GoodService, "", DataQuality::Tfl)],
            ),
            row(
                4,
                "tfl-victoria",
                3,
                vec![status(Severity::MinorDelays, "a", DataQuality::Tfl)],
            ),
            // Same rank, new wording: not a severity change.
            row(
                5,
                "tfl-victoria",
                4,
                vec![status(Severity::ReducedService, "b", DataQuality::Tfl)],
            ),
        ];
        let changes = status_changes(&rows);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].row.id, 4);
        assert_eq!(changes[0].previous, Severity::GoodService);
    }

    #[test]
    fn the_entry_cap_keeps_the_newest() {
        let rows: Vec<StatusHistoryRow> = (0..120)
            .map(|i| {
                let severity = if i % 2 == 0 {
                    Severity::GoodService
                } else {
                    Severity::MinorDelays
                };
                StatusHistoryRow {
                    id: i,
                    line_id: "swr-main".to_string(),
                    statuses: vec![status(severity, "", DataQuality::Knowledgebase)],
                    computed_at: at(0) + chrono::Duration::minutes(i),
                }
            })
            .collect();
        let feed = build_feed(&site(), &scope(), &rows, &[]);
        assert_eq!(feed.entries.len(), MAX_ENTRIES);
        assert!(feed.entries[0].id.ends_with("/119"));
    }

    #[test]
    fn text_from_feeds_is_escaped_in_html_content() {
        let entry = incident_entry(&site(), &incident("A/B 1", 7));
        assert_eq!(entry.link, "https://rail.example.org/incidents/A%2FB%201");
        assert_eq!(entry.id, "tag:rail.example.org,2026:incident/A%2FB%201");
        assert!(
            entry
                .content_html
                .starts_with("<p>Signalling fault &lt;between&gt; A &amp; B</p>")
        );
        assert!(
            entry
                .content_html
                .contains("<p>From 3 Nov 2026 09:00 to 3 Nov 2026 18:00.</p>")
        );
    }

    #[test]
    fn the_site_comes_from_the_redirect_url() {
        assert_eq!(
            Site::from_redirect_url("http://localhost:3000/api/auth/callback"),
            Some(Site {
                origin: "http://localhost:3000".to_string(),
                host: "localhost".to_string()
            })
        );
        assert_eq!(Site::from_redirect_url("not a url"), None);
    }

    #[test]
    fn if_none_match_accepts_lists_weak_tags_and_star() {
        let mut headers = HeaderMap::new();
        assert!(!if_none_match_hits(&headers, "\"abc\""));
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"x\", W/\"abc\""),
        );
        assert!(if_none_match_hits(&headers, "\"abc\""));
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(if_none_match_hits(&headers, "\"abc\""));
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"other\""));
        assert!(!if_none_match_hits(&headers, "\"abc\""));
    }

    #[test]
    fn the_operator_feed_lists_incidents_for_the_folded_in_codes() {
        assert_eq!(incident_operator_codes("SW"), ["SW"]);
        assert_eq!(incident_operator_codes("TfL"), ["LO", "XR"]);
    }
}

/// The routes against a real database: the SQL, the headers and `304`.
#[cfg(test)]
mod db_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use sqlx::PgPool;
    use tower::ServiceExt;

    const LINE: &str = "ztest-feeds-line";
    const TOC: &str = "ZF";
    const INCIDENT: &str = "ztest-feeds-incident";

    async fn connect() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
        sqlx::postgres::PgPoolOptions::new()
            .connect(&database_url)
            .await
            .expect("connect to postgres")
    }

    fn catalogue_line() -> common::LineDefinition {
        common::LineDefinition {
            id: LINE.to_string(),
            name: "Z Feeds Line".to_string(),
            mode: "national-rail".to_string(),
            category: "main-line".to_string(),
            operators: vec![TOC.to_string()],
            stations: vec![],
            sample_stations: vec![],
            match_keywords: vec![],
            excluded_keywords: vec![],
            severity_overrides: HashMap::default(),
            destination_crs_filter: vec![],
            headcode_prefixes: vec![],
            full_coverage_enabled: false,
            pass_through: Vec::new(),
            crs_aliases: std::collections::BTreeMap::new(),
            trunk_for: Vec::new(),
        }
    }

    async fn cleanup(pool: &PgPool) {
        for (sql, key) in [
            ("DELETE FROM line_status_history WHERE line_id = $1", LINE),
            ("DELETE FROM line_status WHERE line_id = $1", LINE),
            (
                "DELETE FROM incident_history WHERE incident_id = $1",
                INCIDENT,
            ),
            ("DELETE FROM incidents WHERE incident_id = $1", INCIDENT),
            ("DELETE FROM tocs WHERE atoc_code = $1", TOC),
        ] {
            sqlx::query(sql)
                .bind(key)
                .execute(pool)
                .await
                .expect("cleanup");
        }
    }

    fn statuses(severity: u8, reason: &str) -> serde_json::Value {
        serde_json::json!([{
            "severity": severity, "reason": reason,
            "validity": {"from_date": "2026-01-01T00:00:00Z", "to_date": null, "is_now": true},
            "data_quality": "knowledgebase"
        }])
    }

    async fn seed(pool: &PgPool) {
        sqlx::query(
            "INSERT INTO tocs (atoc_code, name, legal_name, fetched_at) VALUES ($1, 'Z Feeds Rail', 'Z Feeds Rail', NOW())",
        )
        .bind(TOC)
        .execute(pool)
        .await
        .expect("seed toc");
        sqlx::query(
            "INSERT INTO line_status (line_id, name, mode_name, operators, statuses, computed_at) \
             VALUES ($1, 'Z Feeds Line', 'national-rail', ARRAY[$2], $3, NOW())",
        )
        .bind(LINE)
        .bind(TOC)
        .bind(statuses(10, "Good Service"))
        .execute(pool)
        .await
        .expect("seed line_status");
        for (hours_ago, severity, reason) in [
            (40 * 24, 9, "Old"),
            (35 * 24, 10, "Good Service"),
            (3, 10, "Good Service"),
            (2, 9, "Signalling fault"),
            (1, 10, "Good Service"),
        ] {
            sqlx::query(
                "INSERT INTO line_status_history (line_id, statuses, computed_at) \
                 VALUES ($1, $2, NOW() - make_interval(hours => $3))",
            )
            .bind(LINE)
            .bind(statuses(severity, reason))
            .bind(hours_ago)
            .execute(pool)
            .await
            .expect("seed history");
        }
        sqlx::query(
            "INSERT INTO incidents (incident_id, summary, description, operators, affected_stations, \
                                    priority, validity_periods, is_planned, is_cleared, fetched_at, \
                                    first_seen_at, affected_lines) \
             VALUES ($1, 'Signalling fault at Z', '<p>x</p>', ARRAY[$2], '{}', 1, '[]', FALSE, TRUE, \
                     NOW(), NOW() - INTERVAL '150 minutes', ARRAY[$3])",
        )
        .bind(INCIDENT)
        .bind(TOC)
        .bind(LINE)
        .execute(pool)
        .await
        .expect("seed incident");
        sqlx::query(
            "INSERT INTO incident_history (incident_id, summary, description, operators, affected_stations, \
                                           priority, validity_periods, is_planned, is_cleared, recorded_at) \
             VALUES ($1, 'Signalling fault at Z', '<p>x</p>', ARRAY[$2], '{}', 1, '[]', FALSE, TRUE, \
                     NOW() - INTERVAL '30 minutes')",
        )
        .bind(INCIDENT)
        .bind(TOC)
        .execute(pool)
        .await
        .expect("seed incident history");
    }

    async fn get(router: &axum::Router, uri: &str, if_none_match: Option<&str>) -> Response {
        let mut request = Request::builder().uri(uri);
        if let Some(tag) = if_none_match {
            request = request.header(header::IF_NONE_MATCH, tag);
        }
        router
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api feeds::db_tests -- --ignored --test-threads=1`"]
    async fn the_feeds_serve_cacheable_atom_from_the_history_and_incidents() {
        let pool = connect().await;
        cleanup(&pool).await;
        seed(&pool).await;
        let router: axum::Router =
            Router::new()
                .merge(router())
                .with_state(crate::test_support::inert_app_with_lines(
                    pool.clone(),
                    vec![catalogue_line()],
                ));

        let response = get(&router, &format!("/feeds/lines/{LINE}"), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers().clone();
        assert_eq!(headers[header::CONTENT_TYPE], CONTENT_TYPE);
        assert_eq!(headers[header::CACHE_CONTROL], CACHE_CONTROL);
        let tag = headers[header::ETAG].to_str().unwrap().to_string();
        let xml = body(response).await;
        assert_eq!(etag(&xml), tag);
        // Two changes in the window (the 3-hour-old row matches its
        // baseline, the 35-day-old row before the window; older rows are
        // never read) and the incident, updated by its history row.
        assert_eq!(xml.matches("<entry>").count(), 3, "{xml}");
        assert!(xml.contains("<title type=\"text\">Z Feeds Line: Minor Delays</title>"));
        assert!(xml.contains("<title type=\"text\">Incident: Signalling fault at Z</title>"));
        assert!(xml.contains("&lt;p&gt;Cleared.&lt;/p&gt;"));
        assert!(xml.contains("powered by NationalRail (Train Information Services Ltd)"));
        assert!(xml.contains(&format!("tag:example.invalid,2026:incident/{INCIDENT}")));

        let not_modified = get(&router, &format!("/feeds/lines/{LINE}"), Some(&tag)).await;
        assert_eq!(not_modified.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(not_modified.headers()[header::ETAG], tag.as_str());
        assert_eq!(body(not_modified).await, "");

        let operator = get(&router, &format!("/feeds/operators/{TOC}"), None).await;
        assert_eq!(operator.status(), StatusCode::OK);
        let operator_xml = body(operator).await;
        assert_eq!(operator_xml.matches("<entry>").count(), 3, "{operator_xml}");
        assert!(
            operator_xml.contains("Z Feeds Rail line status (unofficial, from Distant Signal)")
        );

        let network = get(&router, "/feeds/network", None).await;
        assert_eq!(network.status(), StatusCode::OK);
        assert!(body(network).await.contains("Z Feeds Line: Minor Delays"));

        for missing in [
            "/feeds/lines/custom-anything",
            "/feeds/lines/no-such-line",
            "/feeds/operators/ZZZ",
        ] {
            assert_eq!(
                get(&router, missing, None).await.status(),
                StatusCode::NOT_FOUND,
                "{missing}"
            );
        }

        cleanup(&pool).await;
    }
}
