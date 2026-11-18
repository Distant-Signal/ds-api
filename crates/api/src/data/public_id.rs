//! Opaque public ids for the objects the api hands out (DS-534).
//!
//! A numeric `id` is a global sequence: exposed, it shows how many rows the
//! platform holds and lets a caller probe neighbouring ids. Each object
//! kind has a `public_id` column (ds-core migrations `20261117100000`,
//! `20261118100000`, `20261118110000` and `20261118120000`): a one-letter
//! prefix, an underscore and 32 lowercase hex digits, 122 random bits, not
//! derived from `id`.
//!
//! | Kind | Table | Prefix |
//! |---|---|---|
//! | [`Journey`] | `journeys` | `j_` |
//! | [`Leg`] | `journey_legs` | `l_` |
//! | [`Tracking`] | `train_subscriptions` | `t_` |
//! | [`Device`] | `push_subscriptions` | `d_` |
//!
//! During the deprecation window every route takes either form in the path
//! or in a request body: a segment that parses as an integer is a legacy
//! id, a segment of the kind's public shape is a public id, anything else
//! is rejected. The prefix means a public id can never be mistaken for a
//! number, and each kind has its own, so a leg id pasted where a tracking
//! id is expected is rejected rather than looked up.
//!
//! Resolution only maps a public id to the internal id. Ownership is still
//! decided by the SQL predicates each route already has, so an unknown
//! public id resolves to [`NO_SUCH_ROW`] and takes the same 404 path as a
//! legacy id nobody owns.
//!
//! Every legacy id that is resolved is counted in
//! `distant_signal_legacy_id_requests_total{kind}`: the evidence that no
//! client still sends numeric ids, which gates their removal.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::marker::PhantomData;

use serde::{Deserialize, Deserializer};
use sqlx::PgPool;

/// An internal id no row has (every id column starts at 1). What an unknown
/// public id resolves to, so the route's own ownership predicate finds
/// nothing and answers its usual 404.
pub const NO_SUCH_ROW: i64 = 0;

/// Length of a public id: the prefix and 32 hex digits.
const PUBLIC_ID_LEN: usize = 34;

/// The counter of requests that named an object by its numeric id.
const LEGACY_METRIC: &str = "legacy_id_requests_total";

/// One kind of object that has a public id.
pub trait Kind {
    /// The prefix of every public id of this kind, underscore included.
    const PREFIX: &'static str;
    /// The kind's name in messages.
    const NOUN: &'static str;
    /// The `kind` label of the legacy-id counter.
    const LABEL: &'static str;
    /// `SELECT id FROM <table> WHERE public_id = $1`.
    const RESOLVE: &'static str;
    /// `SELECT id, public_id FROM <table> WHERE id = ANY($1)`.
    const BATCH: &'static str;
    /// `SELECT public_id FROM <table> WHERE id = $1`.
    const ONE: &'static str;
}

/// `journeys.public_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Journey;

/// `journey_legs.public_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leg;

/// `train_subscriptions.public_id`: the tracking id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tracking;

/// `push_subscriptions.public_id`: a push device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device;

impl Kind for Journey {
    const PREFIX: &'static str = "j_";
    const NOUN: &'static str = "journey";
    const LABEL: &'static str = "journey";
    const RESOLVE: &'static str = "SELECT id FROM journeys WHERE public_id = $1";
    const BATCH: &'static str = "SELECT id, public_id FROM journeys WHERE id = ANY($1)";
    const ONE: &'static str = "SELECT public_id FROM journeys WHERE id = $1";
}

impl Kind for Leg {
    const PREFIX: &'static str = "l_";
    const NOUN: &'static str = "leg";
    const LABEL: &'static str = "leg";
    const RESOLVE: &'static str = "SELECT id FROM journey_legs WHERE public_id = $1";
    const BATCH: &'static str = "SELECT id, public_id FROM journey_legs WHERE id = ANY($1)";
    const ONE: &'static str = "SELECT public_id FROM journey_legs WHERE id = $1";
}

impl Kind for Tracking {
    const PREFIX: &'static str = "t_";
    const NOUN: &'static str = "tracking";
    const LABEL: &'static str = "tracking";
    const RESOLVE: &'static str = "SELECT id FROM train_subscriptions WHERE public_id = $1";
    const BATCH: &'static str = "SELECT id, public_id FROM train_subscriptions WHERE id = ANY($1)";
    const ONE: &'static str = "SELECT public_id FROM train_subscriptions WHERE id = $1";
}

impl Kind for Device {
    const PREFIX: &'static str = "d_";
    const NOUN: &'static str = "device";
    const LABEL: &'static str = "device";
    const RESOLVE: &'static str = "SELECT id FROM push_subscriptions WHERE public_id = $1";
    const BATCH: &'static str = "SELECT id, public_id FROM push_subscriptions WHERE id = ANY($1)";
    const ONE: &'static str = "SELECT public_id FROM push_subscriptions WHERE id = $1";
}

/// The `kind` labels of the legacy-id counter.
pub const LEGACY_KINDS: [&str; 4] = [Journey::LABEL, Leg::LABEL, Tracking::LABEL, Device::LABEL];

/// The counter of members-route requests that named a member by the bare
/// 32 hex digits `group_members.public_id` held before it got its `m_`
/// prefix (ds-core 20261118130000).
const LEGACY_MEMBER_METRIC: &str = "legacy_member_id_requests_total";

/// Prefix of `group_members.public_id` (member ids are not a [`Kind`]: they
/// have no numeric legacy id to resolve).
pub const MEMBER_PREFIX: &str = "m_";

/// A members route's `{member}` segment, by shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberSegment<'a> {
    /// `m_` and 32 lowercase hex digits.
    Prefixed(&'a str),
    /// 32 lowercase hex digits: a member id from before the prefix, still
    /// in an open tab or an in-flight request.
    BareHex(&'a str),
    /// Anything else: matched exactly against `public_id` (it finds nothing
    /// unless a row holds it) or taken as the caller's own account id,
    /// which is an arbitrary string, so it cannot be rejected as malformed.
    Other(&'a str),
}

/// Classifies a member route segment.
#[must_use]
pub fn classify_member_segment(segment: &str) -> MemberSegment<'_> {
    let is_hex =
        |s: &str| s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    match segment.strip_prefix(MEMBER_PREFIX) {
        Some(rest) if is_hex(rest) => MemberSegment::Prefixed(segment),
        _ if is_hex(segment) => MemberSegment::BareHex(segment),
        _ => MemberSegment::Other(segment),
    }
}

/// The `group_members.public_id` values a segment can name: a bare id also
/// names its prefixed form and the reverse, so the same member resolves before and after
/// the ds-core migration that prefixes the column. A bare-hex segment is
/// counted in `distant_signal_legacy_member_id_requests_total`.
#[must_use]
pub fn member_candidates(segment: &str) -> Vec<String> {
    match classify_member_segment(segment) {
        MemberSegment::BareHex(bare) => {
            metrics::counter!(ds_runtime::metrics::metric_name(LEGACY_MEMBER_METRIC)).increment(1);
            vec![bare.to_owned(), format!("{MEMBER_PREFIX}{bare}")]
        }
        MemberSegment::Prefixed(s) => {
            vec![s.to_owned(), s[MEMBER_PREFIX.len()..].to_owned()]
        }
        MemberSegment::Other(s) => vec![s.to_owned()],
    }
}

/// Keys the legacy-id counters at 0, so the first legacy id after a restart
/// is visible to `increase()`/`rate()`.
pub fn register_metrics() {
    metrics::counter!(ds_runtime::metrics::metric_name(LEGACY_MEMBER_METRIC)).increment(0);
    for kind in LEGACY_KINDS {
        metrics::counter!(
            ds_runtime::metrics::metric_name(LEGACY_METRIC),
            "kind" => kind
        )
        .increment(0);
    }
}

/// An object of kind `K` as the caller named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicRef<K> {
    /// The deprecated numeric id.
    Legacy(i64),
    /// The kind's `public_id`.
    Public(String, PhantomData<fn() -> K>),
}

/// A journey as the caller named it.
pub type JourneyRef = PublicRef<Journey>;
/// A journey leg as the caller named it.
pub type LegRef = PublicRef<Leg>;
/// A tracked train (`train_subscriptions` row) as the caller named it.
pub type TrackingRef = PublicRef<Tracking>;
/// A push device as the caller named it.
pub type DeviceRef = PublicRef<Device>;

/// Whether `s` has the shape of a public id with this `prefix` (which says
/// nothing about whether a row has it).
#[must_use]
pub fn has_public_shape(prefix: &str, s: &str) -> bool {
    s.len() == PUBLIC_ID_LEN
        && s.strip_prefix(prefix)
            .is_some_and(|rest| rest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

impl<K: Kind> PublicRef<K> {
    /// Reads a path segment or a body string: a public id of this kind,
    /// else an integer (what `Path<i64>` accepted), else `None`.
    #[must_use]
    pub fn parse(segment: &str) -> Option<Self> {
        if has_public_shape(K::PREFIX, segment) {
            return Some(Self::Public(segment.to_owned(), PhantomData));
        }
        segment.parse::<i64>().ok().map(Self::Legacy)
    }

    /// What a rejected path segment or body string is told.
    #[must_use]
    pub fn expected() -> String {
        format!("a {} id is a number or a {} id", K::NOUN, K::PREFIX)
    }

    /// The internal id: free for a legacy id (which is counted), one
    /// indexed lookup for a public one ([`NO_SUCH_ROW`] if no row has it).
    ///
    /// # Errors
    /// The lookup failed.
    pub async fn resolve(&self, pool: &PgPool) -> sqlx::Result<i64> {
        match self {
            Self::Legacy(id) => {
                count_legacy(K::LABEL);
                Ok(*id)
            }
            Self::Public(public_id, _) => {
                let id: Option<i64> = sqlx::query_scalar(K::RESOLVE)
                    .bind(public_id)
                    .fetch_optional(pool)
                    .await?;
                Ok(id.unwrap_or(NO_SUCH_ROW))
            }
        }
    }
}

fn count_legacy(kind: &'static str) {
    metrics::counter!(
        ds_runtime::metrics::metric_name(LEGACY_METRIC),
        "kind" => kind
    )
    .increment(1);
}

/// An id in a JSON request body: the deprecated number or a public id
/// string. Resolve it with [`PublicRef::resolve`].
impl<'de, K: Kind> Deserialize<'de> for PublicRef<K> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Number(i64),
            Text(String),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Number(id) => Ok(Self::Legacy(id)),
            Wire::Text(text) => {
                Self::parse(&text).ok_or_else(|| serde::de::Error::custom(Self::expected()))
            }
        }
    }
}

/// The public ids of `ids`, keyed by internal id. A row that does not exist
/// is absent.
///
/// # Errors
/// The query failed.
pub async fn public_ids<'e, K: Kind, E: sqlx::PgExecutor<'e>>(
    executor: E,
    ids: &[i64],
) -> anyhow::Result<HashMap<i64, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i64, String)> = sqlx::query_as(K::BATCH)
        .bind(ids)
        .fetch_all(executor)
        .await?;
    Ok(rows.into_iter().collect())
}

/// The public id of one row, empty if it does not exist. Takes any
/// executor, so a handler holding a transaction reads it on that
/// transaction's connection rather than waiting for a second one.
///
/// # Errors
/// The query failed.
pub async fn public_id<'e, K: Kind, E: sqlx::PgExecutor<'e>>(
    executor: E,
    id: i64,
) -> anyhow::Result<String> {
    let found: Option<String> = sqlx::query_scalar(K::ONE)
        .bind(id)
        .fetch_optional(executor)
        .await?;
    Ok(found.unwrap_or_default())
}

/// The public id of `id` in a map from [`public_ids`]; empty if absent.
#[must_use]
pub fn lookup<S: BuildHasher>(map: &HashMap<i64, String, S>, id: i64) -> String {
    map.get(&id).cloned().unwrap_or_default()
}

/// The public id of `id` in a map from [`public_ids`]; `None` when `id` is
/// `None` or absent from the map.
#[must_use]
pub fn lookup_opt<S: BuildHasher>(
    map: &HashMap<i64, String, S>,
    id: Option<i64>,
) -> Option<String> {
    id.and_then(|id| map.get(&id).cloned())
}

/// The public ids of `ids` in order; an id absent from the map is empty.
#[must_use]
pub fn lookup_all<S: BuildHasher>(map: &HashMap<i64, String, S>, ids: &[i64]) -> Vec<String> {
    ids.iter().map(|id| lookup(map, *id)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEG: &str = "l_0123456789abcdef0123456789abcdef";
    const TRACKING: &str = "t_0123456789abcdef0123456789abcdef";
    const DEVICE: &str = "d_0123456789abcdef0123456789abcdef";
    const JOURNEY: &str = "j_0123456789abcdef0123456789abcdef";

    #[test]
    fn member_segments_are_classified_by_shape() {
        let hex = "0123456789abcdef0123456789abcdef";
        let prefixed = format!("m_{hex}");
        assert_eq!(
            classify_member_segment(&prefixed),
            MemberSegment::Prefixed(&prefixed)
        );
        assert_eq!(classify_member_segment(hex), MemberSegment::BareHex(hex));
        for other in [
            "",
            "m_",
            "u-regular",
            "12345",
            "m_0123456789ABCDEF0123456789abcdef",
            "0123456789abcdef0123456789abcde",
            "j_0123456789abcdef0123456789abcdef",
        ] {
            assert_eq!(classify_member_segment(other), MemberSegment::Other(other));
        }
    }

    #[test]
    fn a_bare_member_id_also_names_its_prefixed_form() {
        let hex = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            member_candidates(hex),
            vec![hex.to_owned(), format!("m_{hex}")]
        );
        assert_eq!(
            member_candidates(&format!("m_{hex}")),
            vec![format!("m_{hex}"), hex.to_owned()]
        );
        assert_eq!(member_candidates("u-1"), vec!["u-1".to_owned()]);
    }

    #[test]
    fn public_ids_are_recognised_by_shape() {
        assert!(has_public_shape("l_", LEG));
        assert!(!has_public_shape("l_", "l_0123456789abcdef0123456789abcde"));
        assert!(!has_public_shape(
            "l_",
            "l_0123456789ABCDEF0123456789abcdef"
        ));
        assert!(!has_public_shape(
            "l_",
            "l_0123456789abcdeg0123456789abcdef"
        ));
        assert!(!has_public_shape(
            "l_",
            "0123456789abcdef0123456789abcdef01"
        ));
    }

    #[test]
    fn each_kind_takes_only_its_own_prefix() {
        assert!(LegRef::parse(LEG).is_some());
        assert!(TrackingRef::parse(TRACKING).is_some());
        assert!(DeviceRef::parse(DEVICE).is_some());
        assert!(JourneyRef::parse(JOURNEY).is_some());
        let all = [JOURNEY, LEG, TRACKING, DEVICE];
        for (kind, own) in all.iter().enumerate() {
            for (other, id) in all.iter().enumerate() {
                let accepted = match kind {
                    0 => JourneyRef::parse(id).is_some(),
                    1 => LegRef::parse(id).is_some(),
                    2 => TrackingRef::parse(id).is_some(),
                    _ => DeviceRef::parse(id).is_some(),
                };
                assert_eq!(accepted, kind == other, "{own} against {id}");
            }
        }
    }

    #[test]
    fn a_public_id_is_never_a_number() {
        assert_eq!(
            LegRef::parse(LEG),
            Some(PublicRef::Public(LEG.to_owned(), PhantomData))
        );
        assert_eq!(LegRef::parse("42"), Some(PublicRef::Legacy(42)));
        assert_eq!(LegRef::parse("l_42"), None);
        assert_eq!(LegRef::parse(""), None);
        assert_eq!(LegRef::parse("42x"), None);
    }

    #[test]
    fn a_body_id_is_a_number_or_a_public_id() {
        let number: LegRef = serde_json::from_str("42").unwrap();
        assert_eq!(number, PublicRef::Legacy(42));
        let text: LegRef = serde_json::from_str(&format!("\"{LEG}\"")).unwrap();
        assert_eq!(text, PublicRef::Public(LEG.to_owned(), PhantomData));
        let numeric_text: LegRef = serde_json::from_str("\"42\"").unwrap();
        assert_eq!(numeric_text, PublicRef::Legacy(42));
        assert!(serde_json::from_str::<LegRef>("\"nope\"").is_err());
        assert!(serde_json::from_str::<LegRef>(&format!("\"{TRACKING}\"")).is_err());
        assert!(serde_json::from_str::<LegRef>("1.5").is_err());
        let tracking: TrackingRef = serde_json::from_str(&format!("\"{TRACKING}\"")).unwrap();
        assert_eq!(
            tracking,
            PublicRef::Public(TRACKING.to_owned(), PhantomData)
        );
    }

    #[test]
    fn the_rejection_names_the_kind() {
        assert_eq!(LegRef::expected(), "a leg id is a number or a l_ id");
        assert_eq!(DeviceRef::expected(), "a device id is a number or a d_ id");
    }

    #[test]
    fn lookups_fill_gaps_with_nothing() {
        let map: HashMap<i64, String> = [(1, LEG.to_owned())].into_iter().collect();
        assert_eq!(lookup(&map, 1), LEG);
        assert_eq!(lookup(&map, 2), "");
        assert_eq!(lookup_opt(&map, Some(1)).as_deref(), Some(LEG));
        assert_eq!(lookup_opt(&map, Some(2)), None);
        assert_eq!(lookup_opt(&map, None), None);
        assert_eq!(
            lookup_all(&map, &[1, 2]),
            vec![LEG.to_owned(), String::new()]
        );
    }

    #[test]
    fn every_kind_is_registered_in_the_counter() {
        assert_eq!(LEGACY_KINDS, ["journey", "leg", "tracking", "device"]);
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, register_metrics);
        let rendered = handle.render();
        for kind in LEGACY_KINDS {
            let line = format!(r#"distant_signal_legacy_id_requests_total{{kind="{kind}"}} 0"#);
            assert!(rendered.contains(&line), "{line} missing from {rendered}");
        }
    }

    #[test]
    fn a_resolved_numeric_id_is_counted_and_a_public_one_is_not() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                // A legacy id resolves without a query, so the pool is
                // never used.
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://127.0.0.1:1/none")
                    .unwrap();
                assert_eq!(LegRef::parse("7").unwrap().resolve(&pool).await.unwrap(), 7);
                assert_eq!(LegRef::parse("8").unwrap().resolve(&pool).await.unwrap(), 8);
                assert_eq!(
                    DeviceRef::parse("9").unwrap().resolve(&pool).await.unwrap(),
                    9
                );
            });
        });
        let rendered = handle.render();
        assert!(
            rendered.contains(r#"distant_signal_legacy_id_requests_total{kind="leg"} 2"#),
            "{rendered}"
        );
        assert!(
            rendered.contains(r#"distant_signal_legacy_id_requests_total{kind="device"} 1"#),
            "{rendered}"
        );
        assert!(!rendered.contains(r#"kind="tracking""#), "{rendered}");
    }
}
