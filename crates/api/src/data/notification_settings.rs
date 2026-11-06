//! A user's push settings (`notification_preferences`, DS-153): a switch
//! per kind of push, their delay threshold, their quiet hours and the
//! regular journeys' leave-by settings. No row
//! means every switch on, the notifier's default threshold and no quiet
//! hours: the behaviour before any of this existed. The notifier reads
//! the same row in each of its candidate queries.

use anyhow::Result;
use chrono::NaiveTime;
use sqlx::PgPool;

/// How far ahead a snooze may run.
pub const MAX_SNOOZE: chrono::TimeDelta = chrono::TimeDelta::days(7);

/// The thresholds a user may pick, in minutes. `None` is the notifier's
/// `TRAIN_DELAY_THRESHOLD_MINUTES` (15 unless the operator changed it).
pub const DELAY_THRESHOLDS: [i16; 4] = [5, 10, 15, 30];

/// The leave-by lead a user may pick, in minutes (and its CHECK).
pub const LEAVE_BY_LEAD_MINUTES: std::ops::RangeInclusive<i16> = 0..=5;

/// A daily window in which pushes are held (crates/notifier/src/hold.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuietHours {
    pub start: NaiveTime,
    pub end: NaiveTime,
    /// An IANA zone name, already checked by the caller.
    pub timezone: String,
}

/// Every setting, as stored (or the defaults without a row).
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per kind of push, as the table has them"
)]
pub struct NotificationSettings {
    pub journey_suggestions: bool,
    pub line_status: bool,
    pub train_delay: bool,
    pub train_cancel: bool,
    pub predicted_start: bool,
    pub station_skip: bool,
    pub template_attention: bool,
    /// Regular journeys' leave-by push (on by default).
    pub leave_by: bool,
    /// Whether a leave-by push still comes in quiet hours (on by default;
    /// a snooze always stops it).
    pub leave_by_quiet_hours: bool,
    /// Minutes before the leave-by time to send it, 0 to 5.
    pub leave_by_lead_minutes: i16,
    /// One push an hour before the day's first leave-by (off by default).
    pub morning_summary: bool,
    pub delay_threshold_minutes: Option<i16>,
    pub quiet_hours: Option<QuietHours>,
    /// Pushes are held until then, as in quiet hours. `None` when not
    /// snoozed, a past time included.
    pub snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            journey_suggestions: true,
            line_status: true,
            train_delay: true,
            train_cancel: true,
            predicted_start: true,
            station_skip: true,
            template_attention: true,
            leave_by: true,
            leave_by_quiet_hours: true,
            leave_by_lead_minutes: 0,
            morning_summary: false,
            delay_threshold_minutes: None,
            quiet_hours: None,
            snoozed_until: None,
        }
    }
}

/// A change to some settings: `None` leaves a setting as it is. The two
/// nullable settings take `Some(None)` to clear them (the default
/// threshold; no quiet hours).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotificationSettingsChange {
    pub journey_suggestions: Option<bool>,
    pub line_status: Option<bool>,
    pub train_delay: Option<bool>,
    pub train_cancel: Option<bool>,
    pub predicted_start: Option<bool>,
    pub station_skip: Option<bool>,
    pub template_attention: Option<bool>,
    pub leave_by: Option<bool>,
    pub leave_by_quiet_hours: Option<bool>,
    pub leave_by_lead_minutes: Option<i16>,
    pub morning_summary: Option<bool>,
    pub delay_threshold_minutes: Option<Option<i16>>,
    pub quiet_hours: Option<Option<QuietHours>>,
    pub snoozed_until: Option<Option<chrono::DateTime<chrono::Utc>>>,
}

#[derive(sqlx::FromRow)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one switch per kind of push, as the table has them"
)]
struct SettingsRow {
    journey_suggestions: bool,
    line_status: bool,
    train_delay: bool,
    train_cancel: bool,
    predicted_start: bool,
    station_skip: bool,
    template_attention: bool,
    leave_by: bool,
    leave_by_quiet_hours: bool,
    leave_by_lead_minutes: i16,
    morning_summary: bool,
    delay_threshold_minutes: Option<i16>,
    quiet_hours_start: Option<NaiveTime>,
    quiet_hours_end: Option<NaiveTime>,
    quiet_hours_timezone: Option<String>,
    snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
}

const SETTINGS_COLUMNS: &str = "journey_suggestions, line_status, train_delay, train_cancel, \
     predicted_start, station_skip, template_attention, leave_by, leave_by_quiet_hours, \
     leave_by_lead_minutes, morning_summary, delay_threshold_minutes, \
     quiet_hours_start, quiet_hours_end, quiet_hours_timezone, \
     CASE WHEN snoozed_until > now() THEN snoozed_until END AS snoozed_until";

impl From<SettingsRow> for NotificationSettings {
    fn from(row: SettingsRow) -> Self {
        let quiet_hours = match (
            row.quiet_hours_start,
            row.quiet_hours_end,
            row.quiet_hours_timezone,
        ) {
            (Some(start), Some(end), Some(timezone)) => Some(QuietHours {
                start,
                end,
                timezone,
            }),
            _ => None,
        };
        Self {
            journey_suggestions: row.journey_suggestions,
            line_status: row.line_status,
            train_delay: row.train_delay,
            train_cancel: row.train_cancel,
            predicted_start: row.predicted_start,
            station_skip: row.station_skip,
            template_attention: row.template_attention,
            leave_by: row.leave_by,
            leave_by_quiet_hours: row.leave_by_quiet_hours,
            leave_by_lead_minutes: row.leave_by_lead_minutes,
            morning_summary: row.morning_summary,
            delay_threshold_minutes: row.delay_threshold_minutes,
            quiet_hours,
            snoozed_until: row.snoozed_until,
        }
    }
}

/// `user_id`'s settings, the defaults when they have no row.
pub async fn get(pool: &PgPool, user_id: &str) -> Result<NotificationSettings> {
    let row: Option<SettingsRow> = sqlx::query_as(&format!(
        "SELECT {SETTINGS_COLUMNS} FROM notification_preferences WHERE user_id = $1"
    ))
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(NotificationSettings::from).unwrap_or_default())
}

/// Applies `change` to `user_id`'s settings (creating their row) and
/// returns them all.
pub async fn update(
    pool: &PgPool,
    user_id: &str,
    change: &NotificationSettingsChange,
) -> Result<NotificationSettings> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO notification_preferences (user_id) VALUES ($1) \
         ON CONFLICT (user_id) DO NOTHING",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    let quiet = change.quiet_hours.as_ref();
    let quiet_hours = quiet.and_then(Option::as_ref);
    let row: SettingsRow = sqlx::query_as(&format!(
        "UPDATE notification_preferences SET \
             journey_suggestions = COALESCE($2, journey_suggestions), \
             line_status = COALESCE($3, line_status), \
             train_delay = COALESCE($4, train_delay), \
             train_cancel = COALESCE($5, train_cancel), \
             predicted_start = COALESCE($6, predicted_start), \
             station_skip = COALESCE($7, station_skip), \
             template_attention = COALESCE($8, template_attention), \
             delay_threshold_minutes = CASE WHEN $9 THEN $10 ELSE delay_threshold_minutes END, \
             quiet_hours_start = CASE WHEN $11 THEN $12 ELSE quiet_hours_start END, \
             quiet_hours_end = CASE WHEN $11 THEN $13 ELSE quiet_hours_end END, \
             quiet_hours_timezone = CASE WHEN $11 THEN $14 ELSE quiet_hours_timezone END, \
             snoozed_until = CASE WHEN $15 THEN $16 ELSE snoozed_until END, \
             leave_by = COALESCE($17, leave_by), \
             leave_by_quiet_hours = COALESCE($18, leave_by_quiet_hours), \
             leave_by_lead_minutes = COALESCE($19, leave_by_lead_minutes), \
             morning_summary = COALESCE($20, morning_summary), \
             updated_at = now() \
         WHERE user_id = $1 \
         RETURNING {SETTINGS_COLUMNS}"
    ))
    .bind(user_id)
    .bind(change.journey_suggestions)
    .bind(change.line_status)
    .bind(change.train_delay)
    .bind(change.train_cancel)
    .bind(change.predicted_start)
    .bind(change.station_skip)
    .bind(change.template_attention)
    .bind(change.delay_threshold_minutes.is_some())
    .bind(change.delay_threshold_minutes.flatten())
    .bind(quiet.is_some())
    .bind(quiet_hours.map(|q| q.start))
    .bind(quiet_hours.map(|q| q.end))
    .bind(quiet_hours.map(|q| q.timezone.clone()))
    .bind(change.snoozed_until.is_some())
    .bind(change.snoozed_until.flatten())
    .bind(change.leave_by)
    .bind(change.leave_by_quiet_hours)
    .bind(change.leave_by_lead_minutes)
    .bind(change.morning_summary)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row.into())
}
