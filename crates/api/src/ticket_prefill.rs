//! Proof that a ticket's Wallet details came from a pass read for that leg.
//!
//! `POST .../ticket/pkpass` stores nothing (review-before-save), so
//! `PUT .../ticket` with `source: "wallet"` had nothing to check: any
//! caller could claim a pass and any `departureDate`. The upload now returns
//! a `prefillToken`, an HMAC-SHA256 over the caller, the leg and the
//! departure the pass gave, with an expiry, and the save accepts `wallet`
//! only with a token that matches (or for a ticket already saved from a
//! pass, unchanged). Stateless, so any replica checks it.
//!
//! The key is derived from the OIDC client secret (every replica has it, it
//! is never empty: `AppState::init`) with a purpose label, so it is not the
//! secret itself and means nothing to any other use of it.

use chrono::{DateTime, SecondsFormat, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// How long a pass's prefill may be saved after it was read.
pub const TOKEN_TTL_SECS: i64 = 3600;

fn mac_hex(
    secret: &str,
    user_id: &str,
    leg_id: i64,
    departure: Option<DateTime<Utc>>,
    expires_at: i64,
) -> String {
    let mut key = Sha256::new();
    key.update(b"distant-signal ticket prefill v1\0");
    key.update(secret.as_bytes());
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&key.finalize())
        .expect("HMAC accepts a key of any length");
    // The user id is length-prefixed so no two inputs share an encoding.
    let departure = departure.map_or_else(String::new, |at| {
        at.to_rfc3339_opts(SecondsFormat::Nanos, true)
    });
    mac.update(
        format!(
            "v1|{}|{user_id}|{leg_id}|{departure}|{expires_at}",
            user_id.len()
        )
        .as_bytes(),
    );
    format!("{:x}", mac.finalize().into_bytes())
}

/// The token for a pass read for `leg_id` that gave `departure`.
#[must_use]
pub fn issue(
    secret: &str,
    user_id: &str,
    leg_id: i64,
    departure: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> String {
    let expires_at = now.timestamp().saturating_add(TOKEN_TTL_SECS);
    format!(
        "{expires_at}.{}",
        mac_hex(secret, user_id, leg_id, departure, expires_at)
    )
}

/// Whether `token` was issued by [`issue`] for these, and has not expired.
#[must_use]
pub fn verify(
    secret: &str,
    user_id: &str,
    leg_id: i64,
    departure: Option<DateTime<Utc>>,
    token: &str,
    now: DateTime<Utc>,
) -> bool {
    let Some((expires_at, mac)) = token.split_once('.') else {
        return false;
    };
    let Ok(expires_at) = expires_at.parse::<i64>() else {
        return false;
    };
    let now = now.timestamp();
    if expires_at < now || expires_at > now.saturating_add(TOKEN_TTL_SECS) {
        return false;
    }
    let expected = mac_hex(secret, user_id, leg_id, departure, expires_at);
    expected.as_bytes().ct_eq(mac.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        text.parse().unwrap()
    }

    #[test]
    fn a_token_matches_only_its_own_user_leg_and_departure() {
        let now = at("2026-11-17T12:00:00Z");
        let departure = Some(at("2026-11-17T13:30:00Z"));
        let token = issue("secret", "user-1", 7, departure, now);
        assert!(verify("secret", "user-1", 7, departure, &token, now));
        assert!(!verify("secret", "user-2", 7, departure, &token, now));
        assert!(!verify("secret", "user-1", 8, departure, &token, now));
        assert!(!verify(
            "secret",
            "user-1",
            7,
            Some(at("2026-11-18T13:30:00Z")),
            &token,
            now
        ));
        assert!(!verify("secret", "user-1", 7, None, &token, now));
        assert!(!verify("other", "user-1", 7, departure, &token, now));
    }

    #[test]
    fn a_token_expires_and_a_forged_expiry_fails() {
        let now = at("2026-11-17T12:00:00Z");
        let token = issue("secret", "user-1", 7, None, now);
        let later = now + chrono::Duration::seconds(TOKEN_TTL_SECS);
        assert!(verify("secret", "user-1", 7, None, &token, later));
        let expired = later + chrono::Duration::seconds(1);
        assert!(!verify("secret", "user-1", 7, None, &token, expired));
        let (_, mac) = token.split_once('.').unwrap();
        let far = now.timestamp() + 10 * TOKEN_TTL_SECS;
        assert!(!verify(
            "secret",
            "user-1",
            7,
            None,
            &format!("{far}.{mac}"),
            now
        ));
        for junk in ["", ".", "x.y", "123", "9999999999.abc"] {
            assert!(!verify("secret", "user-1", 7, None, junk, now), "{junk}");
        }
    }
}
