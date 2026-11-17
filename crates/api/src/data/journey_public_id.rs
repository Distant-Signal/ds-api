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
//! The kinds share one implementation, `data::public_id`, which also
//! serves legs (`l_`), tracked trains (`t_`) and push devices (`d_`). This
//! module keeps the journey-specific names.
//!
//! Resolution only maps a public id to the internal id. Ownership is
//! still decided by the SQL predicates each route already has, so an
//! unknown public id resolves to [`NO_SUCH_JOURNEY`] and takes the same
//! 404 path as a legacy id nobody owns.

use std::collections::HashMap;

use sqlx::PgPool;

use crate::data::public_id::{self, Journey};

pub use crate::data::public_id::JourneyRef;

/// The prefix of every public journey id.
pub const PREFIX: &str = "j_";

/// An internal id no journey has (`journeys.id` starts at 1). What an
/// unknown public id resolves to, so the route's own ownership predicate
/// finds nothing and answers its usual 404.
pub const NO_SUCH_JOURNEY: i64 = public_id::NO_SUCH_ROW;

/// Whether `s` has the shape of a public id (which says nothing about
/// whether a journey has it).
#[must_use]
pub fn is_public_id(s: &str) -> bool {
    public_id::has_public_shape(PREFIX, s)
}

/// The public ids of `ids`, keyed by internal id. A journey that does not
/// exist is absent.
///
/// # Errors
/// The query failed.
pub async fn public_ids(pool: &PgPool, ids: &[i64]) -> anyhow::Result<HashMap<i64, String>> {
    public_id::public_ids::<Journey>(pool, ids).await
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
    public_id::public_id::<Journey, _>(executor, id).await
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
        assert!(matches!(
            JourneyRef::parse(VALID),
            Some(public_id::PublicRef::Public(id, _)) if id == VALID
        ));
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
        assert!(matches!(text, public_id::PublicRef::Public(id, _) if id == VALID));
        let numeric_text: JourneyRef = serde_json::from_str("\"42\"").unwrap();
        assert_eq!(numeric_text, JourneyRef::Legacy(42));
        assert!(serde_json::from_str::<JourneyRef>("\"nope\"").is_err());
        assert!(serde_json::from_str::<JourneyRef>("1.5").is_err());
    }
}
