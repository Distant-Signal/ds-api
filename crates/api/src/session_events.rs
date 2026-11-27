//! Session lifecycle metrics and logs (DS-638): why a session was created,
//! extended, rejected or ended. Observability only: nothing here changes
//! what a request is allowed to do. docs/session-revocation.md ("Session
//! metrics and logs") is the reader's guide.
//!
//! Two counters, both with a closed set of label values:
//!
//! * `distant_signal_api_session_events_total{event, reason}`
//! * `distant_signal_api_sessions_deleted_total{reason}`: session rows
//!   deleted, summed (the first counts operations)
//! * `distant_signal_api_session_probe_total{result}`: what
//!   `GET /auth/session` answered
//!
//! Privacy: no label and no log field carries a user id, username, session
//! token, cookie value, IP address or user agent. A log line may carry
//! [`pseudonym`] of the internal user id, so "the same user keeps logging
//! in again" is visible without naming anyone. A pseudonym is never a
//! label.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

pub(crate) const SESSION_EVENTS_METRIC: &str = "api_session_events_total";
pub(crate) const SESSIONS_DELETED_METRIC: &str = "api_sessions_deleted_total";
pub(crate) const SESSION_PROBE_METRIC: &str = "api_session_probe_total";

/// Fixed, versioned domain separator for [`pseudonym`]. Changing it changes
/// every pseudonym, so it is never edited in place.
const PSEUDONYM_DOMAIN: &[u8] = b"distant-signal/api/session-log-pseudonym/v1\0";

/// A stable, non-reversible-by-inspection tag for a user in log lines: the
/// first 12 hex characters (48 bits) of SHA-256 over a fixed domain string
/// and the internal user id.
///
/// This is a pseudonym, not anonymisation: the hash is unsalted, so anyone
/// holding the list of user ids can recompute it. That is acceptable only
/// because the ids are opaque identity-provider subjects, the tag is
/// truncated, and it is never a metric label or an API field. It exists so
/// an operator can see that two log lines concern the same account.
#[must_use]
pub fn pseudonym(user_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(PSEUDONYM_DOMAIN);
    hasher.update(user_id.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    digest[..12].to_string()
}

/// Why an authenticated-route session lookup found no live session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// No `Cookie` header, or none named `distant_signal_session`.
    NoCookie,
    /// A cookie, but its hashed id has no `sessions` row: never existed
    /// (or a forged or truncated value), revoked, deleted by a logout or a
    /// re-login, or swept after it expired.
    Unknown,
    /// The row exists and its expiry passed before the absolute cap: unused
    /// for `SESSION_TTL_DAYS`.
    ExpiredIdle,
    /// The row exists and its expiry is at the absolute cap
    /// (`created_at + SESSION_MAX_AGE_DAYS`).
    ExpiredAbsolute,
    /// The row exists but its user's `sessions_invalidated_at` is later than
    /// the row's `created_at`. Revocation deletes the rows in the same
    /// transaction, so this is reachable only through a race (a session
    /// committed by a login concurrent with a revocation).
    Invalidated,
}

impl RejectReason {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoCookie => "no_cookie",
            Self::Unknown => "unknown",
            Self::ExpiredIdle => "expired_idle",
            Self::ExpiredAbsolute => "expired_absolute",
            Self::Invalidated => "invalidated",
        }
    }

    /// The two reasons any anonymous visitor produces. They are counted,
    /// but logged at DEBUG, so a flood of anonymous requests cannot flood
    /// the log.
    #[must_use]
    pub fn is_routine(self) -> bool {
        matches!(self, Self::NoCookie | Self::Unknown)
    }
}

const ALL_REJECT_REASONS: [RejectReason; 5] = [
    RejectReason::NoCookie,
    RejectReason::Unknown,
    RejectReason::ExpiredIdle,
    RejectReason::ExpiredAbsolute,
    RejectReason::Invalidated,
];

/// What the `sessions` row for a cookie that failed the live lookup looks
/// like (`users::get_session_row_state`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionRowState {
    pub user_id: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    /// The database's `NOW()` for the same statement.
    pub checked_at: DateTime<Utc>,
    /// `created_at` is behind the user's `sessions_invalidated_at`.
    pub invalidated: bool,
}

/// Names the reason a lookup that found no live session failed, from the
/// row (if any) behind the cookie. Pure.
///
/// A row that reads as live here (the lookup raced a concurrent extension
/// or the clock edge) is `Unknown`: there is nothing better to say.
#[must_use]
pub fn classify_missing_row(row: Option<&SessionRowState>, max_age_days: i32) -> RejectReason {
    let Some(row) = row else {
        return RejectReason::Unknown;
    };
    if row.invalidated {
        return RejectReason::Invalidated;
    }
    if row.expires_at <= row.checked_at {
        // Expiry only ever moves to the cap or earlier, so an expiry at the
        // cap is the absolute limit; anything before it lapsed idle.
        let cap = row.created_at + chrono::Duration::days(i64::from(max_age_days));
        return if row.expires_at >= cap {
            RejectReason::ExpiredAbsolute
        } else {
            RejectReason::ExpiredIdle
        };
    }
    RejectReason::Unknown
}

/// A rejected lookup, with what the log line may say about it. Never holds
/// a raw user id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub reason: RejectReason,
    /// [`pseudonym`] of the row's user, when a row was found.
    pub user: Option<String>,
    pub session_age_hours: Option<i64>,
    /// How long ago the row expired, for the two expired reasons.
    pub expired_hours_ago: Option<i64>,
}

impl Rejection {
    fn bare(reason: RejectReason) -> Self {
        Self {
            reason,
            user: None,
            session_age_hours: None,
            expired_hours_ago: None,
        }
    }

    fn from_row(reason: RejectReason, row: &SessionRowState) -> Self {
        let expired = matches!(
            reason,
            RejectReason::ExpiredIdle | RejectReason::ExpiredAbsolute
        );
        Self {
            reason,
            user: Some(pseudonym(&row.user_id)),
            session_age_hours: Some((row.checked_at - row.created_at).num_hours()),
            expired_hours_ago: expired.then(|| (row.checked_at - row.expires_at).num_hours()),
        }
    }
}

/// The outcome of resolving a session cookie.
#[derive(Debug)]
pub enum Resolution<U> {
    Valid(U),
    Rejected(Rejection),
}

/// A boxed, `Send` future of a database read: what [`resolve`]'s two reads
/// return. Boxed (not `async` closures) so the extractors' futures stay
/// provably `Send`.
pub type ReadFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<T>> + Send + 'a>>;

/// Resolves a cookie to a user, classifying a failure. The one place that
/// decides "no cookie / unknown / expired / ..." for both
/// `AuthenticatedUser` and the session probe.
///
/// `lookup` is the live-session query. `diagnose` (one indexed read of the
/// row by its hashed id) runs ONLY when `lookup` found nothing: the success
/// path executes exactly one query, and a request with no cookie none. A
/// failing `diagnose` degrades to `Unknown`; it never changes the outcome,
/// which is already a rejection. Both get the hashed id, never the token.
pub async fn resolve<'a, U>(
    token: Option<&str>,
    max_age_days: i32,
    lookup: impl Fn(String) -> ReadFuture<'a, Option<U>>,
    diagnose: impl Fn(String) -> ReadFuture<'a, Option<SessionRowState>>,
) -> anyhow::Result<Resolution<U>> {
    let Some(token) = token else {
        return Ok(Resolution::Rejected(Rejection::bare(
            RejectReason::NoCookie,
        )));
    };
    let hashed = crate::auth::hash_session_token(token);
    if let Some(user) = lookup(hashed.clone()).await? {
        return Ok(Resolution::Valid(user));
    }
    let row = match diagnose(hashed).await {
        Ok(row) => row,
        Err(err) => {
            tracing::debug!(error = ?err, "could not classify a rejected session cookie");
            None
        }
    };
    let reason = classify_missing_row(row.as_ref(), max_age_days);
    Ok(Resolution::Rejected(match row {
        Some(row) if reason != RejectReason::Unknown => Rejection::from_row(reason, &row),
        _ => Rejection::bare(reason),
    }))
}

/// How a session came to exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateReason {
    /// An OIDC login.
    Login,
    /// A login that re-authenticated the user for a sensitive action
    /// (account deletion).
    Reauth,
    /// "Log out other sessions": the session reissued for the caller.
    RevokeOthersReissue,
}

impl CreateReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Reauth => "reauth",
            Self::RevokeOthersReissue => "revoke_others_reissue",
        }
    }
}

/// Why session rows were deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevokeReason {
    /// The identity provider's back-channel logout.
    Backchannel,
    /// An administrator's revoke.
    Admin,
    /// "Log out other sessions".
    RevokeOthers,
    /// The user logged out of this browser.
    UserLogout,
    /// The user deleted their account (sessions go with it).
    AccountDeletion,
    /// A login replaced the session its browser carried.
    LoginReplaced,
    /// Account continuity re-keyed the account to a new subject.
    Rekey,
    /// The periodic sweep of expired rows (not a user-visible logout: those
    /// sessions had already lapsed).
    ExpirySweep,
}

impl RevokeReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Backchannel => "backchannel",
            Self::Admin => "admin",
            Self::RevokeOthers => "revoke_others",
            Self::UserLogout => "user_logout",
            Self::AccountDeletion => "account_deletion",
            Self::LoginReplaced => "login_replaced",
            Self::Rekey => "rekey",
            Self::ExpirySweep => "expiry_sweep",
        }
    }
}

const ALL_REVOKE_REASONS: [RevokeReason; 8] = [
    RevokeReason::Backchannel,
    RevokeReason::Admin,
    RevokeReason::RevokeOthers,
    RevokeReason::UserLogout,
    RevokeReason::AccountDeletion,
    RevokeReason::LoginReplaced,
    RevokeReason::Rekey,
    RevokeReason::ExpirySweep,
];

/// Why a back-channel logout request was not acted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackchannelRejectReason {
    /// The `logout_token` failed validation (or named only a `sid`).
    InvalidToken,
    /// The token was valid but names no user here (answered 200).
    UnknownUser,
    /// No usable `logout_token` field, or the body was too large.
    Other,
}

impl BackchannelRejectReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::InvalidToken => "invalid_token",
            Self::UnknownUser => "unknown_user",
            Self::Other => "other",
        }
    }
}

fn event(event: &'static str, reason: &'static str) {
    metrics::counter!(
        ds_runtime::metrics::metric_name(SESSION_EVENTS_METRIC),
        "event" => event,
        "reason" => reason
    )
    .increment(1);
}

/// A session was created.
pub fn record_created(reason: CreateReason, user_id: &str) {
    event("created", reason.as_str());
    tracing::info!(
        session_lifecycle = true,
        event = "created",
        reason = reason.as_str(),
        user = %pseudonym(user_id),
        "session created"
    );
}

/// A lookup on an authenticated route (or the session probe) found no live
/// session. Counted always; logged at INFO unless the reason is routine.
pub fn record_rejected(rejection: &Rejection) {
    let reason = rejection.reason.as_str();
    event("rejected", reason);
    if rejection.reason.is_routine() {
        tracing::debug!(
            session_lifecycle = true,
            event = "rejected",
            reason,
            "session rejected"
        );
        return;
    }
    tracing::info!(
        session_lifecycle = true,
        event = "rejected",
        reason,
        user = rejection.user.as_deref().unwrap_or("-"),
        session_age_hours = rejection.session_age_hours,
        expired_hours_ago = rejection.expired_hours_ago,
        "session rejected"
    );
}

/// What `GET /auth/session` answered: `authenticated` or a [`RejectReason`].
pub fn record_probe(result: Option<RejectReason>) {
    metrics::counter!(
        ds_runtime::metrics::metric_name(SESSION_PROBE_METRIC),
        "result" => result.map_or("authenticated", RejectReason::as_str)
    )
    .increment(1);
}

/// A due session's expiry was slid forward.
pub fn record_extended(user_id: &str, session_age_hours: i64, new_expiry_in_hours: i64) {
    event("extended", "slide");
    tracing::info!(
        session_lifecycle = true,
        event = "extended",
        reason = "slide",
        user = %pseudonym(user_id),
        session_age_hours,
        new_expiry_in_hours,
        "session expiry extended"
    );
}

/// Sliding a due session's expiry failed (the request carried on).
pub fn record_extend_failed(user_id: &str) {
    event("extend_failed", "db_error");
    tracing::info!(
        session_lifecycle = true,
        event = "extend_failed",
        reason = "db_error",
        user = %pseudonym(user_id),
        "session expiry extension failed"
    );
}

/// Session rows were deleted. `user_id` is the owner when the caller knows
/// it; `sessions_deleted` may be 0 (a logout with a stale cookie).
pub fn record_revoked(reason: RevokeReason, user_id: Option<&str>, sessions_deleted: u64) {
    event("revoked", reason.as_str());
    metrics::counter!(
        ds_runtime::metrics::metric_name(SESSIONS_DELETED_METRIC),
        "reason" => reason.as_str()
    )
    .increment(sessions_deleted);
    let user = user_id.map(pseudonym);
    tracing::info!(
        session_lifecycle = true,
        event = "revoked",
        reason = reason.as_str(),
        user = user.as_deref().unwrap_or("-"),
        sessions_deleted,
        "sessions deleted"
    );
}

/// The expiry sweep deleted `deleted` rows. Counted only when it deleted
/// any; the sweep's own caller logs the line.
pub fn record_swept(deleted: u64) {
    if deleted == 0 {
        return;
    }
    event("revoked", RevokeReason::ExpirySweep.as_str());
    metrics::counter!(
        ds_runtime::metrics::metric_name(SESSIONS_DELETED_METRIC),
        "reason" => RevokeReason::ExpirySweep.as_str()
    )
    .increment(deleted);
}

/// A back-channel logout request was not acted on. Counter only: the
/// handler's existing WARN lines say why, and the token is never logged.
pub fn record_backchannel_rejected(reason: BackchannelRejectReason) {
    event("backchannel_rejected", reason.as_str());
}

/// Registers every series at 0 at startup, so `increase()` sees the first
/// event after a restart.
pub fn register() {
    for (e, r) in [
        ("created", CreateReason::Login.as_str()),
        ("created", CreateReason::Reauth.as_str()),
        ("created", CreateReason::RevokeOthersReissue.as_str()),
        ("extended", "slide"),
        ("extend_failed", "db_error"),
    ] {
        metrics::counter!(
            ds_runtime::metrics::metric_name(SESSION_EVENTS_METRIC),
            "event" => e,
            "reason" => r
        )
        .increment(0);
    }
    for reason in ALL_REJECT_REASONS {
        metrics::counter!(
            ds_runtime::metrics::metric_name(SESSION_EVENTS_METRIC),
            "event" => "rejected",
            "reason" => reason.as_str()
        )
        .increment(0);
        metrics::counter!(
            ds_runtime::metrics::metric_name(SESSION_PROBE_METRIC),
            "result" => reason.as_str()
        )
        .increment(0);
    }
    metrics::counter!(
        ds_runtime::metrics::metric_name(SESSION_PROBE_METRIC),
        "result" => "authenticated"
    )
    .increment(0);
    for reason in ALL_REVOKE_REASONS {
        metrics::counter!(
            ds_runtime::metrics::metric_name(SESSION_EVENTS_METRIC),
            "event" => "revoked",
            "reason" => reason.as_str()
        )
        .increment(0);
        metrics::counter!(
            ds_runtime::metrics::metric_name(SESSIONS_DELETED_METRIC),
            "reason" => reason.as_str()
        )
        .increment(0);
    }
    for reason in [
        BackchannelRejectReason::InvalidToken,
        BackchannelRejectReason::UnknownUser,
        BackchannelRejectReason::Other,
    ] {
        metrics::counter!(
            ds_runtime::metrics::metric_name(SESSION_EVENTS_METRIC),
            "event" => "backchannel_rejected",
            "reason" => reason.as_str()
        )
        .increment(0);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::Duration;

    use super::*;

    const MAX_AGE: i32 = 90;

    fn row(created_days_ago: i64, expires_in_days: i64, invalidated: bool) -> SessionRowState {
        let now = Utc::now();
        SessionRowState {
            user_id: "user-1".to_string(),
            created_at: now - Duration::days(created_days_ago),
            expires_at: now + Duration::days(expires_in_days),
            checked_at: now,
            invalidated,
        }
    }

    #[test]
    fn no_row_is_unknown() {
        assert_eq!(classify_missing_row(None, MAX_AGE), RejectReason::Unknown);
    }

    #[test]
    fn an_expiry_before_the_cap_is_idle() {
        // Created 30 days ago, lapsed 2 days ago: far from the 90 day cap.
        assert_eq!(
            classify_missing_row(Some(&row(30, -2, false)), MAX_AGE),
            RejectReason::ExpiredIdle
        );
    }

    #[test]
    fn an_idle_lapse_long_after_the_cap_is_still_idle() {
        // Lapsed idle at day 40, found at day 200.
        assert_eq!(
            classify_missing_row(Some(&row(200, -160, false)), MAX_AGE),
            RejectReason::ExpiredIdle
        );
    }

    #[test]
    fn an_expiry_at_the_cap_is_absolute() {
        let mut state = row(91, -1, false);
        state.expires_at = state.created_at + Duration::days(i64::from(MAX_AGE));
        assert_eq!(
            classify_missing_row(Some(&state), MAX_AGE),
            RejectReason::ExpiredAbsolute
        );
    }

    #[test]
    fn a_row_behind_the_invalidation_marker_is_invalidated_even_if_expired() {
        assert_eq!(
            classify_missing_row(Some(&row(5, 9, true)), MAX_AGE),
            RejectReason::Invalidated
        );
        assert_eq!(
            classify_missing_row(Some(&row(30, -2, true)), MAX_AGE),
            RejectReason::Invalidated
        );
    }

    #[test]
    fn a_row_that_reads_live_is_unknown() {
        assert_eq!(
            classify_missing_row(Some(&row(5, 9, false)), MAX_AGE),
            RejectReason::Unknown
        );
    }

    #[test]
    fn reason_names_are_the_documented_label_values() {
        let names: Vec<_> = ALL_REJECT_REASONS.iter().map(|r| r.as_str()).collect();
        assert_eq!(
            names,
            [
                "no_cookie",
                "unknown",
                "expired_idle",
                "expired_absolute",
                "invalidated"
            ]
        );
        assert!(RejectReason::NoCookie.is_routine());
        assert!(RejectReason::Unknown.is_routine());
        assert!(!RejectReason::ExpiredIdle.is_routine());
    }

    #[test]
    fn the_pseudonym_is_stable_short_hex_and_does_not_contain_the_id() {
        let a = pseudonym("user-1");
        assert_eq!(a, pseudonym("user-1"));
        assert_ne!(a, pseudonym("user-2"));
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!a.contains("user"));
    }

    fn found<'a>(
        calls: &'a AtomicUsize,
    ) -> impl Fn(String) -> ReadFuture<'a, Option<&'static str>> {
        move |_| {
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Some("user"))
            })
        }
    }

    fn missing<'a>(
        calls: &'a AtomicUsize,
    ) -> impl Fn(String) -> ReadFuture<'a, Option<&'static str>> {
        move |_| {
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            })
        }
    }

    fn diagnose_with<'a>(
        calls: &'a AtomicUsize,
        state: Option<SessionRowState>,
    ) -> impl Fn(String) -> ReadFuture<'a, Option<SessionRowState>> {
        move |_| {
            let state = state.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(state)
            })
        }
    }

    /// The success path runs the live lookup once and never the diagnostic
    /// read; no cookie runs neither; a failed lookup runs the diagnostic
    /// read once.
    #[tokio::test]
    async fn only_the_failure_path_runs_the_extra_read() {
        let lookups = AtomicUsize::new(0);
        let diagnoses = AtomicUsize::new(0);
        let counts = || {
            (
                lookups.load(Ordering::SeqCst),
                diagnoses.load(Ordering::SeqCst),
            )
        };
        let stale = Some(row(30, -2, false));

        let resolved = resolve(
            Some("tok"),
            MAX_AGE,
            found(&lookups),
            diagnose_with(&diagnoses, stale.clone()),
        )
        .await
        .expect("resolve");
        assert!(matches!(resolved, Resolution::Valid("user")));
        assert_eq!(counts(), (1, 0));

        let resolved = resolve(
            None,
            MAX_AGE,
            missing(&lookups),
            diagnose_with(&diagnoses, stale.clone()),
        )
        .await
        .expect("resolve");
        assert!(matches!(resolved, Resolution::Rejected(r) if r.reason == RejectReason::NoCookie));
        assert_eq!(counts(), (1, 0));

        let resolved = resolve(
            Some("tok"),
            MAX_AGE,
            missing(&lookups),
            diagnose_with(&diagnoses, stale),
        )
        .await
        .expect("resolve");
        let Resolution::Rejected(rejection) = resolved else {
            panic!("a missing session must be rejected");
        };
        assert_eq!(rejection.reason, RejectReason::ExpiredIdle);
        assert_eq!(rejection.user, Some(pseudonym("user-1")));
        assert_eq!(counts(), (2, 1));
    }

    #[tokio::test]
    async fn a_failing_diagnostic_read_degrades_to_unknown() {
        let lookups = AtomicUsize::new(0);
        let broken = |_: String| -> ReadFuture<'_, Option<SessionRowState>> {
            Box::pin(async { Err(anyhow::anyhow!("db")) })
        };
        let resolved = resolve(Some("tok"), MAX_AGE, missing(&lookups), broken)
            .await
            .expect("resolve");
        assert!(matches!(resolved, Resolution::Rejected(r) if r.reason == RejectReason::Unknown));
    }

    #[tokio::test]
    async fn a_failing_live_lookup_is_an_error_not_a_rejection() {
        let diagnoses = AtomicUsize::new(0);
        let broken = |_: String| -> ReadFuture<'_, Option<&'static str>> {
            Box::pin(async { Err(anyhow::anyhow!("db")) })
        };
        let result = resolve(
            Some("tok"),
            MAX_AGE,
            broken,
            diagnose_with(&diagnoses, None),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(diagnoses.load(Ordering::SeqCst), 0);
    }

    fn render(record: impl FnOnce()) -> String {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, record);
        handle.render()
    }

    #[test]
    fn every_series_is_registered_at_zero_under_its_full_name() {
        let rendered = render(register);
        for line in [
            r#"distant_signal_api_session_events_total{event="created",reason="login"} 0"#,
            r#"distant_signal_api_session_events_total{event="created",reason="reauth"} 0"#,
            r#"distant_signal_api_session_events_total{event="created",reason="revoke_others_reissue"} 0"#,
            r#"distant_signal_api_session_events_total{event="rejected",reason="expired_idle"} 0"#,
            r#"distant_signal_api_session_events_total{event="rejected",reason="no_cookie"} 0"#,
            r#"distant_signal_api_session_events_total{event="extended",reason="slide"} 0"#,
            r#"distant_signal_api_session_events_total{event="extend_failed",reason="db_error"} 0"#,
            r#"distant_signal_api_session_events_total{event="revoked",reason="backchannel"} 0"#,
            r#"distant_signal_api_session_events_total{event="revoked",reason="expiry_sweep"} 0"#,
            r#"distant_signal_api_session_events_total{event="backchannel_rejected",reason="invalid_token"} 0"#,
            r#"distant_signal_api_sessions_deleted_total{reason="admin"} 0"#,
            r#"distant_signal_api_session_probe_total{result="authenticated"} 0"#,
            r#"distant_signal_api_session_probe_total{result="no_cookie"} 0"#,
        ] {
            assert!(rendered.contains(line), "{line} missing from {rendered}");
        }
    }

    #[test]
    fn recording_counts_events_and_deleted_rows() {
        let rendered = render(|| {
            record_created(CreateReason::Login, "user-1");
            record_rejected(&Rejection::bare(RejectReason::ExpiredIdle));
            record_rejected(&Rejection::bare(RejectReason::NoCookie));
            record_probe(Some(RejectReason::NoCookie));
            record_probe(None);
            record_revoked(RevokeReason::Admin, Some("user-1"), 3);
            record_swept(5);
            record_swept(0);
            record_backchannel_rejected(BackchannelRejectReason::InvalidToken);
        });
        for line in [
            r#"distant_signal_api_session_events_total{event="created",reason="login"} 1"#,
            r#"distant_signal_api_session_events_total{event="rejected",reason="expired_idle"} 1"#,
            r#"distant_signal_api_session_events_total{event="rejected",reason="no_cookie"} 1"#,
            r#"distant_signal_api_session_probe_total{result="no_cookie"} 1"#,
            r#"distant_signal_api_session_probe_total{result="authenticated"} 1"#,
            r#"distant_signal_api_session_events_total{event="revoked",reason="admin"} 1"#,
            r#"distant_signal_api_sessions_deleted_total{reason="admin"} 3"#,
            r#"distant_signal_api_session_events_total{event="revoked",reason="expiry_sweep"} 1"#,
            r#"distant_signal_api_sessions_deleted_total{reason="expiry_sweep"} 5"#,
            r#"distant_signal_api_session_events_total{event="backchannel_rejected",reason="invalid_token"} 1"#,
        ] {
            assert!(rendered.contains(line), "{line} missing from {rendered}");
        }
        // No label value is anything but a fixed reason name: no user id,
        // pseudonym or token can be one.
        assert!(!rendered.contains("user-1"));
        assert!(!rendered.contains(&pseudonym("user-1")));
    }

    /// A `MakeWriter` that keeps everything written to it.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Captured;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Every lifecycle log line carries the pseudonym and none carries the
    /// raw user id, a session token, its hash or a cookie. Ignored so it
    /// runs in the serial pass: a scoped subscriber races other tests over
    /// tracing's process-wide callsite cache.
    #[test]
    #[ignore = "reads the log output; run with `cargo test -p api \
                lifecycle_logs_never_carry -- --ignored --test-threads=1`"]
    fn lifecycle_logs_never_carry_an_identifier_or_a_credential() {
        let captured = Captured::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(captured.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let token = crate::auth::generate_session_token();
        let hashed = crate::auth::hash_session_token(&token);
        let cookie = format!("distant_signal_session={token}");
        let user = "oidc-subject-with-a-long-distinctive-value";
        let state = SessionRowState {
            user_id: user.to_string(),
            created_at: Utc::now() - Duration::days(30),
            expires_at: Utc::now() - Duration::days(2),
            checked_at: Utc::now(),
            invalidated: false,
        };

        record_created(CreateReason::Login, user);
        record_created(CreateReason::Reauth, user);
        record_created(CreateReason::RevokeOthersReissue, user);
        record_extended(user, 100, 336);
        record_extend_failed(user);
        record_revoked(RevokeReason::Backchannel, Some(user), 2);
        record_revoked(RevokeReason::UserLogout, None, 0);
        record_rejected(&Rejection::from_row(RejectReason::ExpiredIdle, &state));
        record_rejected(&Rejection::bare(RejectReason::NoCookie));
        record_backchannel_rejected(BackchannelRejectReason::InvalidToken);

        let log = String::from_utf8(captured.0.lock().expect("capture lock").clone())
            .expect("utf-8 log output")
            // The formatter quotes string fields; compare without quotes.
            .replace('"', "");
        assert!(log.contains("session_lifecycle=true"), "{log}");
        assert!(log.contains(&pseudonym(user)), "{log}");
        assert!(log.contains("reason=expired_idle"), "{log}");
        for secret in [user, token.as_str(), hashed.as_str(), cookie.as_str()] {
            assert!(!log.contains(secret), "{secret} leaked into the log: {log}");
        }
        // The routine reasons are DEBUG; the capture is at TRACE, so check
        // the level the line was written at.
        let routine = log
            .lines()
            .find(|line| line.contains("reason=no_cookie"))
            .expect("the no_cookie line");
        assert!(routine.contains("DEBUG"), "{routine}");
        let diagnostic = log
            .lines()
            .find(|line| line.contains("reason=expired_idle"))
            .expect("the expired_idle line");
        assert!(diagnostic.contains("INFO"), "{diagnostic}");
    }
}

/// The classification and the recording, against real `sessions` rows and
/// the real extractors and routes. `#[sqlx::test]` modules run in the
/// superuser pass only (`SQLX_TEST_SKIPS` in scripts/db-tests.py).
#[cfg(test)]
mod db_tests {
    use axum::body::Body;
    use axum::extract::FromRequestParts;
    use axum::http::{Request, StatusCode, header};
    use sqlx::PgPool;
    use tower::ServiceExt;

    use crate::auth::{
        AuthenticatedUser, SESSION_COOKIE_NAME, SessionProbeUser, hash_session_token,
    };

    const ORIGIN: &str = "https://example.invalid";

    fn recorder() -> metrics_exporter_prometheus::PrometheusRecorder {
        metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder()
    }

    fn request(token: Option<&str>) -> axum::http::request::Parts {
        let mut builder = Request::builder();
        if let Some(token) = token {
            builder = builder.header(header::COOKIE, format!("{SESSION_COOKIE_NAME}={token}"));
        }
        builder.body(Body::empty()).expect("request").into_parts().0
    }

    async fn user(pool: &PgPool, id: &str) {
        sqlx::query("INSERT INTO users (id) VALUES ($1)")
            .bind(id)
            .execute(pool)
            .await
            .expect("insert user");
    }

    async fn session(pool: &PgPool, token: &str, user_id: &str, created: &str, expires: &str) {
        sqlx::query(&format!(
            "INSERT INTO sessions (id, user_id, created_at, expires_at) \
             VALUES ($1, $2, NOW() - INTERVAL '{created}', NOW() + INTERVAL '{expires}')"
        ))
        .bind(hash_session_token(token))
        .bind(user_id)
        .execute(pool)
        .await
        .expect("insert session");
    }

    fn series(rendered: &str, line: &str) {
        assert!(rendered.contains(line), "{line} missing from {rendered}");
    }

    /// Expired idle, expired at the absolute cap, unknown, invalidated and
    /// no cookie each get their own reason, on an authenticated-only route
    /// and on the session probe.
    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn each_failure_has_its_own_reason(pool: PgPool) {
        let rec = recorder();
        let handle = rec.handle();
        let _guard = metrics::set_default_local_recorder(&rec);
        let app = crate::test_support::inert_app(pool.clone());
        user(&pool, "SE-USER-A").await;
        user(&pool, "SE-USER-B").await;
        // Idle: created 30 days ago, lapsed 2 days ago, 28 days short of the
        // 90 day cap.
        session(&pool, "idle", "SE-USER-A", "30 days", "-2 days").await;
        // Absolute: created 100 days ago, expiry exactly at the cap.
        sqlx::query(
            "INSERT INTO sessions (id, user_id, created_at, expires_at) \
             VALUES ($1, 'SE-USER-A', NOW() - INTERVAL '100 days', \
                     NOW() - INTERVAL '100 days' + INTERVAL '90 days')",
        )
        .bind(hash_session_token("absolute"))
        .execute(&pool)
        .await
        .expect("insert absolute session");
        // Invalidated: still unexpired, but behind its user's marker (only a
        // race can leave such a row behind a revocation).
        session(&pool, "invalidated", "SE-USER-B", "10 days", "5 days").await;
        sqlx::query(
            "UPDATE users SET sessions_invalidated_at = NOW() - INTERVAL '1 day' \
             WHERE id = 'SE-USER-B'",
        )
        .execute(&pool)
        .await
        .expect("set marker");

        for token in [
            None,
            Some("never-existed"),
            Some("idle"),
            Some("absolute"),
            Some("invalidated"),
        ] {
            let mut parts = request(token);
            match AuthenticatedUser::from_request_parts(&mut parts, &app).await {
                Ok(_) => panic!("{token:?} must be rejected"),
                Err((status, _)) => assert_eq!(status, StatusCode::UNAUTHORIZED),
            }
            let mut parts = request(token);
            let probe = SessionProbeUser::from_request_parts(&mut parts, &app)
                .await
                .unwrap_or_else(|_| panic!("the probe folds {token:?} into anonymous"));
            assert!(probe.0.is_none());
        }

        let rendered = handle.render();
        for reason in [
            "no_cookie",
            "unknown",
            "expired_idle",
            "expired_absolute",
            "invalidated",
        ] {
            series(
                &rendered,
                &format!(
                    r#"distant_signal_api_session_events_total{{event="rejected",reason="{reason}"}} {}"#,
                    // The probe records a rejection except for no cookie.
                    if reason == "no_cookie" { 1 } else { 2 }
                ),
            );
            series(
                &rendered,
                &format!(r#"distant_signal_api_session_probe_total{{result="{reason}"}} 1"#),
            );
        }
        assert!(
            !rendered.contains("SE-USER") && !rendered.contains("never-existed"),
            "{rendered}"
        );
    }

    /// A live session: no rejection, a due one is extended and counted, and
    /// an extension inside the throttle is not.
    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_live_session_is_not_rejected_and_a_due_one_is_extended(pool: PgPool) {
        let rec = recorder();
        let handle = rec.handle();
        let _guard = metrics::set_default_local_recorder(&rec);
        let app = crate::test_support::inert_app(pool.clone());
        user(&pool, "SE-USER-C").await;
        session(&pool, "due", "SE-USER-C", "12 days", "2 days").await;
        session(&pool, "fresh", "SE-USER-C", "12 days", "13 days 12 hours").await;

        for token in ["due", "fresh"] {
            let mut parts = request(Some(token));
            let probe = SessionProbeUser::from_request_parts(&mut parts, &app)
                .await
                .unwrap_or_else(|_| panic!("lookup"));
            assert!(probe.0.is_some());
        }
        let rendered = handle.render();
        series(
            &rendered,
            r#"distant_signal_api_session_events_total{event="extended",reason="slide"} 1"#,
        );
        series(
            &rendered,
            r#"distant_signal_api_session_probe_total{result="authenticated"} 2"#,
        );
        assert!(!rendered.contains(r#"event="rejected""#), "{rendered}");
    }

    /// "Log out other sessions" and logout, through the real routes: the
    /// revocations and the reissue are counted, and the revoked cookie then
    /// reads as unknown.
    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn revoke_others_and_logout_are_recorded(pool: PgPool) {
        let rec = recorder();
        let handle = rec.handle();
        let _guard = metrics::set_default_local_recorder(&rec);
        let app = crate::test_support::inert_app(pool.clone());
        user(&pool, "SE-USER-D").await;
        session(&pool, "this-browser", "SE-USER-D", "1 hour", "13 days").await;
        session(&pool, "other-device", "SE-USER-D", "2 days", "12 days").await;
        let router = crate::routes::auth::router().with_state(app.clone());

        let post = |path: &str, token: &str| {
            Request::builder()
                .method("POST")
                .uri(path)
                .header(header::ORIGIN, ORIGIN)
                .header(header::COOKIE, format!("{SESSION_COOKIE_NAME}={token}"))
                .body(Body::empty())
                .expect("request")
        };
        let response = router
            .clone()
            .oneshot(post("/auth/sessions/revoke-others", "this-browser"))
            .await
            .expect("revoke-others");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("a reissued cookie")
            .to_string();
        let new_token = cookie
            .strip_prefix(&format!("{SESSION_COOKIE_NAME}="))
            .and_then(|rest| rest.split(';').next())
            .expect("cookie value")
            .to_string();

        let response = router
            .clone()
            .oneshot(post("/auth/logout", &new_token))
            .await
            .expect("logout");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        // Logout again with the now-gone cookie: idempotent, nothing deleted.
        let response = router
            .oneshot(post("/auth/logout", &new_token))
            .await
            .expect("second logout");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let rendered = handle.render();
        for line in [
            r#"distant_signal_api_session_events_total{event="created",reason="revoke_others_reissue"} 1"#,
            r#"distant_signal_api_session_events_total{event="revoked",reason="revoke_others"} 1"#,
            r#"distant_signal_api_sessions_deleted_total{reason="revoke_others"} 2"#,
            r#"distant_signal_api_session_events_total{event="revoked",reason="user_logout"} 2"#,
            r#"distant_signal_api_sessions_deleted_total{reason="user_logout"} 1"#,
        ] {
            series(&rendered, line);
        }
        assert!(!rendered.contains(&new_token), "{rendered}");

        let mut parts = request(Some("this-browser"));
        let app = crate::test_support::inert_app(pool);
        assert!(
            AuthenticatedUser::from_request_parts(&mut parts, &app)
                .await
                .is_err()
        );
        series(
            &handle.render(),
            r#"distant_signal_api_session_events_total{event="rejected",reason="unknown"} 1"#,
        );
    }

    /// The re-login path: replacing a browser's prior session reports the
    /// owner it deleted, which the callback records as `login_replaced`.
    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_relogin_reports_the_session_it_replaced(pool: PgPool) {
        user(&pool, "SE-USER-E").await;
        session(&pool, "old-browser", "SE-USER-E", "3 days", "11 days").await;
        let replaced = crate::data::users::replace_session(
            &pool,
            Some(&hash_session_token("old-browser")),
            &hash_session_token("new-browser"),
            "SE-USER-E",
            14,
        )
        .await
        .expect("replace");
        assert_eq!(replaced.as_deref(), Some("SE-USER-E"));
        let none = crate::data::users::replace_session(
            &pool,
            Some(&hash_session_token("no-such")),
            &hash_session_token("another"),
            "SE-USER-E",
            14,
        )
        .await
        .expect("replace without a prior row");
        assert_eq!(none, None);
    }
}
