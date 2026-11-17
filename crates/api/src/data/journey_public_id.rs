//! Opaque public journey ids (DS-534).
//!
//! `journeys.id` is a global sequence: exposed, it shows how many journeys
//! the platform holds and lets a caller probe neighbouring ids.
//! `journeys.public_id` (ds-core migration `20261117100000`) is what the
//! api hands out instead: `j_` and 32 lowercase hex digits, 122 random
//! bits, not derived from `id`.
//!
//! During the deprecation window every journey route takes either form in
//! the path or a request body: a segment that parses as an integer is a
//! legacy id, a segment of the public shape is a public id, anything else
//! is rejected as it always was. The `j_` prefix means a public id can
//! never be mistaken for a number. Responses carry `publicId` next to the
//! deprecated numeric `id` (docs/api-changelog.md).
//!
//! Resolution only maps a public id to the internal id. Ownership is
//! still decided by the SQL predicates each route already has, so an
//! unknown public id resolves to [`NO_SUCH_JOURNEY`] and takes the same
//! 404 path as a legacy id nobody owns.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer};
use sqlx::PgPool;

/// The prefix of every public journey id.
pub const PREFIX: &str = "j_";

/// Length of a public id: the prefix and 32 hex digits.
const PUBLIC_ID_LEN: usize = 34;

/// An internal id no journey has (`journeys.id` starts at 1). What an
/// unknown public id resolves to, so the route's own ownership predicate
/// finds nothing and answers its usual 404.
pub const NO_SUCH_JOURNEY: i64 = 0;

/// A journey as the caller named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JourneyRef {
    /// The deprecated numeric id.
    Legacy(i64),
    /// A `journeys.public_id`.
    Public(String),
}

/// Whether `s` has the shape of a public id (which says nothing about
/// whether a journey has it).
#[must_use]
pub fn is_public_id(s: &str) -> bool {
    s.len() == PUBLIC_ID_LEN
        && s.strip_prefix(PREFIX)
            .is_some_and(|rest| rest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

impl JourneyRef {
    /// Reads a path segment or a body string: a public id, else an
    /// integer (what `Path<i64>` accepted), else `None`.
    #[must_use]
    pub fn parse(segment: &str) -> Option<Self> {
        if is_public_id(segment) {
            return Some(Self::Public(segment.to_owned()));
        }
        segment.parse::<i64>().ok().map(Self::Legacy)
    }

    /// The internal id: free for a legacy id, one indexed lookup for a
    /// public one ([`NO_SUCH_JOURNEY`] if no journey has it).
    ///
    /// # Errors
    /// The lookup failed.
    pub async fn resolve(&self, pool: &PgPool) -> sqlx::Result<i64> {
        match self {
            Self::Legacy(id) => Ok(*id),
            Self::Public(public_id) => {
                let id: Option<i64> =
                    sqlx::query_scalar("SELECT id FROM journeys WHERE public_id = $1")
                        .bind(public_id)
                        .fetch_optional(pool)
                        .await?;
                Ok(id.unwrap_or(NO_SUCH_JOURNEY))
            }
        }
    }
}

/// A journey id in a JSON request body: the deprecated number or a public
/// id string. Resolve it with [`JourneyRef::resolve`].
impl<'de> Deserialize<'de> for JourneyRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Number(i64),
            Text(String),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Number(id) => Ok(Self::Legacy(id)),
            Wire::Text(text) => Self::parse(&text)
                .ok_or_else(|| serde::de::Error::custom("a journey id is a number or a j_ id")),
        }
    }
}

/// The public ids of `ids`, keyed by internal id. A journey that does not
/// exist is absent.
///
/// # Errors
/// The query failed.
pub async fn public_ids(pool: &PgPool, ids: &[i64]) -> anyhow::Result<HashMap<i64, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, public_id FROM journeys WHERE id = ANY($1)")
            .bind(ids)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

/// The public id of one journey, empty if it does not exist. Takes any
/// executor, so a handler holding a transaction reads it on that
/// transaction's connection rather than waiting for a second one.
///
/// # Errors
/// The query failed.
pub async fn public_id<'e, E: sqlx::PgExecutor<'e>>(
    executor: E,
    id: i64,
) -> anyhow::Result<String> {
    let found: Option<String> = sqlx::query_scalar("SELECT public_id FROM journeys WHERE id = $1")
        .bind(id)
        .fetch_optional(executor)
        .await?;
    Ok(found.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "j_0123456789abcdef0123456789abcdef";

    #[test]
    fn public_ids_are_recognised_by_shape() {
        assert!(is_public_id(VALID));
        assert!(!is_public_id("j_0123456789abcdef0123456789abcde"));
        assert!(!is_public_id("j_0123456789ABCDEF0123456789abcdef"));
        assert!(!is_public_id("k_0123456789abcdef0123456789abcdef"));
        assert!(!is_public_id("0123456789abcdef0123456789abcdef01"));
        assert!(!is_public_id("j_0123456789abcdeg0123456789abcdef"));
    }

    #[test]
    fn a_public_id_is_never_a_number() {
        assert_eq!(
            JourneyRef::parse(VALID),
            Some(JourneyRef::Public(VALID.to_owned()))
        );
        assert_eq!(JourneyRef::parse("42"), Some(JourneyRef::Legacy(42)));
        assert_eq!(JourneyRef::parse("j_42"), None);
        assert_eq!(JourneyRef::parse(""), None);
        assert_eq!(JourneyRef::parse("42x"), None);
    }

    #[test]
    fn a_body_id_is_a_number_or_a_public_id() {
        let number: JourneyRef = serde_json::from_str("42").unwrap();
        assert_eq!(number, JourneyRef::Legacy(42));
        let text: JourneyRef = serde_json::from_str(&format!("\"{VALID}\"")).unwrap();
        assert_eq!(text, JourneyRef::Public(VALID.to_owned()));
        let numeric_text: JourneyRef = serde_json::from_str("\"42\"").unwrap();
        assert_eq!(numeric_text, JourneyRef::Legacy(42));
        assert!(serde_json::from_str::<JourneyRef>("\"nope\"").is_err());
        assert!(serde_json::from_str::<JourneyRef>("1.5").is_err());
    }
}
