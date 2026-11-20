//! Stage 2b of DS-534's opaque ids: whether the api still serves the numeric
//! id next to an object's opaque one, behind `API_SERVE_NUMERIC_IDS`.
//!
//! Every response that names a journey, leg, tracked train, push device,
//! break or regular journey carries the opaque id (`publicId`,
//! `journeyPublicId`, `openBreakPublicId`, `legPublicIds`, ...) beside the
//! numeric one (`id`, `journeyId`, `openBreakId`, `legIds`, ...). The
//! numeric ones stay in the schema (the `/v1` breaking-change gate forbids
//! removing a property or making it optional), so stage 2b cannot drop
//! them: it serves them as `0`, the stage 2a convention of the anonymous
//! share view ("not available: use the public id").
//!
//! The switch is [`NumericIds`], read once at startup:
//!
//! - `true` (unset, empty, the default): [`NumericIds::Served`], today's
//!   behaviour, and the layer is not installed at all;
//! - `false`: [`NumericIds::Withheld`], stage 2b. [`withhold_numeric_ids`]
//!   rewrites every JSON response on the public, delegated and token
//!   listeners.
//!
//! One mechanism, not a conditional per response type: a JSON object is
//! rewritten by its field names alone. A numeric field is the partner of an
//! opaque one when the opaque one is named after it:
//!
//! | opaque field          | numeric partner |
//! |-----------------------|-----------------|
//! | `publicId`            | `id`            |
//! | `<stem>PublicId`      | `<stem>Id`      |
//! | `<stem>PublicIds`     | `<stem>Ids`     |
//!
//! (that is how every opaque id was added: ds-api's changelog, "named as
//! for journeys".) A partner that is a number becomes `0`, each number in a
//! partner array becomes `0` (so the array keeps its length and its order
//! against the `...PublicIds` array), and a `null` stays `null` (an absent
//! optional id, such as a non-owner's `openBreakId`). The opaque field is
//! never touched. A new response type that follows the naming is covered
//! without a line here; the tests below read the generated
//! `api-types/api.ts` and fail when one does not follow it.
//!
//! Left alone, on purpose, because they have no opaque id yet or are not
//! about an opaque object: time away, webhook, personal token and connected
//! app ids, `RecentlyDeletedJourney.id`, `deletedJourneyIds` (served beside
//! `deletedJourneyCount` until a frontend reads the count, then dropped) and
//! `trainsId` (already `0`). A template leg's `id`, `RevisionLeg.ticketIds`
//! and the two deleted-journey `journeyId`s are no longer served at all
//! (DS-534). The account export
//! keeps its numeric ids ([`KeepNumericIds`]): it is the owner's own data
//! and its rows join on them.
//!
//! Only the response side. Numeric ids are still accepted in paths and
//! bodies (stage 3, counted by `distant_signal_legacy_id_requests_total`).
//!
//! The mode is visible as `distant_signal_api_serve_numeric_ids{mode}` (1 on
//! the current mode's series, 0 on the other), the rewritten responses as
//! `distant_signal_api_numeric_ids_withheld_responses_total`, and a JSON
//! response too large or of unknown size to rewrite (it is sent unchanged)
//! as `distant_signal_api_numeric_ids_unprocessed_total{reason}`.

use anyhow::bail;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hyper::body::Body as HttpBody;
use serde_json::Value;

/// The environment variable behind [`NumericIds`] (chart `api.extraEnv`).
pub const SERVE_NUMERIC_IDS_ENV: &str = "API_SERVE_NUMERIC_IDS";

const MODE_METRIC: &str = "api_serve_numeric_ids";
const WITHHELD_METRIC: &str = "api_numeric_ids_withheld_responses_total";
const UNPROCESSED_METRIC: &str = "api_numeric_ids_unprocessed_total";

/// The `mode` label values of [`MODE_METRIC`].
const MODES: [&str; 2] = ["served", "withheld"];

/// The largest JSON response that is rewritten. Every response with opaque
/// ids is a journey, a list of them or a group's rows, far below this; the
/// multi-megabyte ones (line populations) carry none.
const MAX_REWRITE_BYTES: usize = 16 * 1024 * 1024;

/// Every key that names an opaque id ends in `PublicId`, `PublicIds` or is
/// `publicId`, so each contains this. A body without it is not parsed.
const PUBLIC_ID_MARKER: &[u8] = b"ublicId";

/// Whether the api serves the numeric id beside an opaque one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NumericIds {
    /// Today's behaviour: the real numeric id.
    #[default]
    Served,
    /// Stage 2b: `0` (see the module doc).
    Withheld,
}

impl NumericIds {
    /// [`SERVE_NUMERIC_IDS_ENV`] from the environment.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::parse(std::env::var(SERVE_NUMERIC_IDS_ENV).ok().as_deref())
    }

    /// Unset or empty is [`Self::Served`]; otherwise `true` or `false` in
    /// any case. Anything else is an error, so a typo cannot silently turn
    /// the numeric ids off (or on).
    pub fn parse(value: Option<&str>) -> anyhow::Result<Self> {
        match value.map(str::trim) {
            None | Some("") => Ok(Self::Served),
            Some(v) if v.eq_ignore_ascii_case("true") => Ok(Self::Served),
            Some(v) if v.eq_ignore_ascii_case("false") => Ok(Self::Withheld),
            Some(v) => bail!("{SERVE_NUMERIC_IDS_ENV} must be true or false, got {v:?}"),
        }
    }

    /// Whether the numeric ids are served.
    #[must_use]
    pub const fn serves(self) -> bool {
        matches!(self, Self::Served)
    }

    /// The `mode` label of the mode gauge.
    #[must_use]
    pub const fn mode(self) -> &'static str {
        match self {
            Self::Served => MODES[0],
            Self::Withheld => MODES[1],
        }
    }

    /// Sets the mode gauge (1 on this mode, 0 on the other) and keys the
    /// rewrite counters at 0.
    pub fn register_metrics(self) {
        for mode in MODES {
            metrics::gauge!(
                ds_runtime::metrics::metric_name(MODE_METRIC),
                "mode" => mode
            )
            .set(if mode == self.mode() { 1.0 } else { 0.0 });
        }
        metrics::counter!(ds_runtime::metrics::metric_name(WITHHELD_METRIC)).increment(0);
        for reason in ["too_large", "unsized"] {
            metrics::counter!(
                ds_runtime::metrics::metric_name(UNPROCESSED_METRIC),
                "reason" => reason
            )
            .increment(0);
        }
    }
}

/// A response extension: this response keeps its numeric ids whatever the
/// mode. The account export sets it.
#[derive(Debug, Clone, Copy)]
pub struct KeepNumericIds;

/// The layer, only installed when the numeric ids are withheld:
/// `axum::middleware::from_fn_with_state(policy, numeric_ids::withhold_numeric_ids)`.
pub async fn withhold_numeric_ids(
    State(policy): State<NumericIds>,
    request: Request,
    next: Next,
) -> Response {
    let response = next.run(request).await;
    if policy.serves() {
        return response;
    }
    rewrite(response).await
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("json"))
}

async fn rewrite(response: Response) -> Response {
    if response.extensions().get::<KeepNumericIds>().is_some() || !is_json(response.headers()) {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let reason = match HttpBody::size_hint(&body).exact() {
        None => Some("unsized"),
        Some(size) if size > u64::try_from(MAX_REWRITE_BYTES).unwrap_or(u64::MAX) => {
            Some("too_large")
        }
        Some(_) => None,
    };
    if let Some(reason) = reason {
        metrics::counter!(
            ds_runtime::metrics::metric_name(UNPROCESSED_METRIC),
            "reason" => reason
        )
        .increment(1);
        tracing::warn!(reason, "a JSON response was sent with its numeric ids");
        return Response::from_parts(parts, body);
    }
    let bytes = match axum::body::to_bytes(body, MAX_REWRITE_BYTES).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::error!(error = ?err, "could not read a response body to withhold its numeric ids");
            return (StatusCode::INTERNAL_SERVER_ERROR, "response failed").into_response();
        }
    };
    let Some(rewritten) = withhold_bytes(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    metrics::counter!(ds_runtime::metrics::metric_name(WITHHELD_METRIC)).increment(1);
    parts
        .headers
        .insert(header::CONTENT_LENGTH, HeaderValue::from(rewritten.len()));
    Response::from_parts(parts, Body::from(rewritten))
}

/// `body` with its numeric ids withheld, or `None` when there is nothing to
/// change (no opaque id in it, not JSON, or no numeric partner left to
/// change) and it should be sent as it is.
#[must_use]
pub fn withhold_bytes(body: &[u8]) -> Option<Vec<u8>> {
    if !body
        .windows(PUBLIC_ID_MARKER.len())
        .any(|window| window == PUBLIC_ID_MARKER)
    {
        return None;
    }
    let mut value: Value = serde_json::from_slice(body).ok()?;
    if withhold(&mut value) == 0 {
        return None;
    }
    serde_json::to_vec(&value).ok()
}

/// The numeric field an opaque field stands for (see the module doc).
#[must_use]
pub fn numeric_partner(opaque_key: &str) -> Option<String> {
    if opaque_key == "publicId" {
        return Some("id".to_owned());
    }
    if let Some(stem) = opaque_key.strip_suffix("PublicIds") {
        return Some(format!("{stem}Ids"));
    }
    opaque_key
        .strip_suffix("PublicId")
        .map(|stem| format!("{stem}Id"))
}

/// The opaque field a numeric field would have (the inverse of
/// [`numeric_partner`]), or `None` when `key` is not named like an id.
#[must_use]
pub fn opaque_partner(numeric_key: &str) -> Option<String> {
    if numeric_key == "id" {
        return Some("publicId".to_owned());
    }
    if let Some(stem) = numeric_key.strip_suffix("Ids") {
        return Some(format!("{stem}PublicIds"));
    }
    numeric_key
        .strip_suffix("Id")
        .map(|stem| format!("{stem}PublicId"))
}

/// Sets to `0` every numeric field that has an opaque partner, anywhere in
/// `value`. Returns how many numbers it changed (a number that is already
/// `0` is not counted).
pub fn withhold(value: &mut Value) -> usize {
    match value {
        Value::Object(map) => {
            let partners: Vec<String> = map
                .keys()
                .map(String::as_str)
                .filter_map(numeric_partner)
                .collect();
            let mut changed = 0;
            for partner in partners {
                if let Some(field) = map.get_mut(&partner) {
                    changed += zero(field);
                }
            }
            for field in map.values_mut() {
                changed += withhold(field);
            }
            changed
        }
        Value::Array(items) => items.iter_mut().map(withhold).sum(),
        _ => 0,
    }
}

/// A numeric id (or an array of them) to `0`; anything else is left alone.
fn zero(value: &mut Value) -> usize {
    if let Value::Array(items) = value {
        return items.iter_mut().map(zero).sum();
    }
    if value.is_number() && value.as_i64() != Some(0) {
        *value = Value::from(0);
        return 1;
    }
    0
}

/// The paths of the numeric fields in `value` that have an opaque
/// partner and are not `0` or `null`: what a withheld response must not
/// have. Shared with the DB tests.
#[cfg(test)]
pub(crate) fn leaks(value: &Value) -> Vec<String> {
    fn walk(value: &Value, path: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                let partners: Vec<String> = map
                    .keys()
                    .map(String::as_str)
                    .filter_map(numeric_partner)
                    .collect();
                for partner in &partners {
                    if let Some(field) = map.get(partner) {
                        let nonzero = |v: &Value| v.is_number() && v.as_i64() != Some(0);
                        let leaking = match field {
                            Value::Array(items) => items.iter().any(nonzero),
                            other => nonzero(other),
                        };
                        if leaking {
                            out.push(format!("{path}.{partner}"));
                        }
                    }
                }
                for (key, field) in map {
                    walk(field, &format!("{path}.{key}"), out);
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    walk(item, &format!("{path}[{index}]"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(value, "$", &mut out);
    out
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::get;
    use serde_json::json;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn the_flag_defaults_to_served_and_a_typo_is_an_error() {
        assert_eq!(NumericIds::parse(None).unwrap(), NumericIds::Served);
        assert_eq!(NumericIds::parse(Some("")).unwrap(), NumericIds::Served);
        assert_eq!(
            NumericIds::parse(Some(" TRUE ")).unwrap(),
            NumericIds::Served
        );
        assert_eq!(
            NumericIds::parse(Some("false")).unwrap(),
            NumericIds::Withheld
        );
        assert_eq!(
            NumericIds::parse(Some("False")).unwrap(),
            NumericIds::Withheld
        );
        assert!(NumericIds::parse(Some("off")).is_err());
        assert!(NumericIds::parse(Some("0")).is_err());
        assert_eq!(NumericIds::default(), NumericIds::Served);
        assert_eq!(NumericIds::Served.mode(), "served");
        assert_eq!(NumericIds::Withheld.mode(), "withheld");
    }

    #[test]
    fn partners_follow_the_naming() {
        assert_eq!(numeric_partner("publicId").as_deref(), Some("id"));
        assert_eq!(
            numeric_partner("openBreakPublicId").as_deref(),
            Some("openBreakId")
        );
        assert_eq!(
            numeric_partner("singleLegJourneyPublicIds").as_deref(),
            Some("singleLegJourneyIds")
        );
        assert_eq!(numeric_partner("journeyId"), None);
        assert_eq!(opaque_partner("id").as_deref(), Some("publicId"));
        assert_eq!(opaque_partner("legIds").as_deref(), Some("legPublicIds"));
        assert_eq!(
            opaque_partner("trainSubscriptionId").as_deref(),
            Some("trainSubscriptionPublicId")
        );
        assert_eq!(opaque_partner("trainUid"), None);
    }

    #[test]
    fn a_journey_detail_loses_every_numeric_id_but_keeps_the_rest() {
        let mut detail = json!({
            "id": 7,
            "publicId": "j_0000",
            "sourceTemplateId": 3,
            "sourceTemplatePublicId": "r_0000",
            "customName": "Commute",
            "legs": [{
                "id": 11,
                "publicId": "l_0000",
                "replacesLegIds": [5, 6],
                "replacesLegPublicIds": ["l_0001", "l_0002"],
                "trackedTrainState": {
                    "id": 21,
                    "publicId": "t_0000",
                    "journeyIds": [7, 8],
                    "journeyPublicIds": ["j_0000", "j_0001"],
                    "singleLegJourneyIds": [],
                    "singleLegJourneyPublicIds": [],
                    "trainsId": 0
                }
            }],
            "replan": { "openBreak": {
                "id": 31, "publicId": "b_0000",
                "triggerLegId": 11, "triggerLegPublicId": "l_0000",
                "severity": 2
            } }
        });
        assert_eq!(withhold(&mut detail), 10);
        assert_eq!(
            detail,
            json!({
                "id": 0,
                "publicId": "j_0000",
                "sourceTemplateId": 0,
                "sourceTemplatePublicId": "r_0000",
                "customName": "Commute",
                "legs": [{
                    "id": 0,
                    "publicId": "l_0000",
                    "replacesLegIds": [0, 0],
                    "replacesLegPublicIds": ["l_0001", "l_0002"],
                    "trackedTrainState": {
                        "id": 0,
                        "publicId": "t_0000",
                        "journeyIds": [0, 0],
                        "journeyPublicIds": ["j_0000", "j_0001"],
                        "singleLegJourneyIds": [],
                        "singleLegJourneyPublicIds": [],
                        "trainsId": 0
                    }
                }],
                "replan": { "openBreak": {
                    "id": 0, "publicId": "b_0000",
                    "triggerLegId": 0, "triggerLegPublicId": "l_0000",
                    "severity": 2
                } }
            })
        );
        assert_eq!(leaks(&detail), Vec::<String>::new());
    }

    #[test]
    fn a_null_numeric_id_stays_null_and_one_without_an_opaque_partner_stays() {
        let mut body = json!([
            { "openBreakId": null, "openBreakPublicId": null, "id": 4, "publicId": "j_0000" },
            // Time away, webhooks, personal tokens, connected apps, deleted
            // journeys: numeric ids with no opaque one.
            { "id": 9, "deletedJourneyIds": [1, 2], "journeyId": 12 },
            // An `id` that is a string is not a number.
            { "id": "x", "publicId": "j_0001" }
        ]);
        assert_eq!(withhold(&mut body), 1);
        assert_eq!(
            body,
            json!([
                { "openBreakId": null, "openBreakPublicId": null, "id": 0, "publicId": "j_0000" },
                { "id": 9, "deletedJourneyIds": [1, 2], "journeyId": 12 },
                { "id": "x", "publicId": "j_0001" }
            ])
        );
    }

    #[test]
    fn a_body_without_an_opaque_id_is_not_rewritten() {
        assert_eq!(withhold_bytes(br#"{"id":4,"trainsId":0}"#), None);
        assert_eq!(withhold_bytes(b"not json but publicId"), None);
        // Opaque ids, but every numeric partner is already 0 or null.
        assert_eq!(withhold_bytes(br#"{"id":0,"publicId":"j_0000"}"#), None);
    }

    fn router(policy: NumericIds) -> Router {
        let body = || async {
            (
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"id":7,"publicId":"j_0000","customName":"a"}"#,
            )
        };
        Router::new()
            .route("/journey", get(body))
            .route(
                "/export",
                get(|| async {
                    let mut response = (
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"id":7,"publicId":"j_0000"}"#,
                    )
                        .into_response();
                    response.extensions_mut().insert(KeepNumericIds);
                    response
                }),
            )
            .route(
                "/ics",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/calendar")],
                        r#"{"id":7,"publicId":"j_0000"}"#,
                    )
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                policy,
                withhold_numeric_ids,
            ))
    }

    async fn get_body(router: &Router, uri: &str) -> (HeaderMap, String) {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn the_layer_rewrites_a_json_response_and_fixes_its_length() {
        let (headers, body) = get_body(&router(NumericIds::Withheld), "/journey").await;
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            value,
            json!({ "id": 0, "publicId": "j_0000", "customName": "a" })
        );
        assert_eq!(
            headers.get(header::CONTENT_LENGTH).unwrap(),
            &body.len().to_string()
        );
    }

    #[tokio::test]
    async fn the_layer_leaves_an_export_and_a_non_json_response_alone() {
        let router = router(NumericIds::Withheld);
        let (_, export) = get_body(&router, "/export").await;
        assert_eq!(export, r#"{"id":7,"publicId":"j_0000"}"#);
        let (_, ics) = get_body(&router, "/ics").await;
        assert_eq!(ics, r#"{"id":7,"publicId":"j_0000"}"#);
    }

    #[tokio::test]
    async fn served_changes_nothing() {
        let (_, body) = get_body(&router(NumericIds::Served), "/journey").await;
        assert_eq!(body, r#"{"id":7,"publicId":"j_0000","customName":"a"}"#);
    }

    // The generated types (`api-types/api.ts`) are every response and
    // request the web app reads, so they are the list of what carries an
    // opaque id. The tests below read it, so a type that gains one without
    // following the naming fails here instead of leaking in stage 2b.

    /// An object type of `api.ts`: its name and its fields' (name, type).
    struct ObjectType {
        name: String,
        fields: Vec<(String, String)>,
    }

    fn strip_comments(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("/*") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            match after.find("*/") {
                Some(end) => rest = &after[end + 2..],
                None => {
                    rest = "";
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Splits `text` on top-level commas (not inside `{}`, `<>`, `[]`, `()`).
    fn split_top_level(text: &str) -> Vec<String> {
        let mut parts = Vec::new();
        let mut depth = 0_i32;
        let mut current = String::new();
        for ch in text.chars() {
            match ch {
                '{' | '<' | '[' | '(' => depth += 1,
                '}' | '>' | ']' | ')' => depth -= 1,
                _ => {}
            }
            if ch == ',' && depth == 0 {
                parts.push(std::mem::take(&mut current));
            } else {
                current.push(ch);
            }
        }
        if !current.trim().is_empty() {
            parts.push(current);
        }
        parts
    }

    fn object_types() -> Vec<ObjectType> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(crate::api_types::PATH);
        let text = strip_comments(&std::fs::read_to_string(path).expect("read api.ts"));
        // The values after the types are not declarations.
        let types = text.split("\nexport const ").next().unwrap();
        let mut out = Vec::new();
        for declaration in types.split("\nexport type ").skip(1) {
            let Some((name, rest)) = declaration.split_once('=') else {
                continue;
            };
            let rest = rest.trim().trim_end_matches(';').trim();
            let Some(body) = rest.strip_prefix('{').and_then(|r| r.strip_suffix('}')) else {
                continue;
            };
            let fields = split_top_level(body)
                .into_iter()
                .filter_map(|field| {
                    let (key, ty) = field.split_once(':')?;
                    let key = key.trim().trim_end_matches('?');
                    key.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
                        .then(|| (key.to_owned(), ty.trim().to_owned()))
                })
                .collect();
            out.push(ObjectType {
                name: name.trim().to_owned(),
                fields,
            });
        }
        out
    }

    /// Numeric id fields that sit in a type with opaque ids and have none of
    /// their own, on purpose (each has no opaque id yet or is not an
    /// object's id): the template responses' deleted journey ids. They are
    /// served only until a frontend that reads `deletedJourneyCount` is
    /// deployed; a later change drops them and these entries with them.
    const UNPAIRED_NUMERIC_IDS: [(&str, &str); 2] = [
        ("SetTemplateActiveResponse", "deletedJourneyIds"),
        ("SkipTemplateDateResponse", "deletedJourneyIds"),
    ];

    fn has_opaque_id(object: &ObjectType) -> bool {
        object
            .fields
            .iter()
            .any(|(key, _)| numeric_partner(key).is_some())
    }

    #[test]
    fn every_numeric_id_with_an_opaque_sibling_in_the_api_types_is_withheld() {
        let types = object_types();
        let mut covered = 0;
        for object in types.iter().filter(|object| has_opaque_id(object)) {
            // A value for each pair: numbers for the numeric field, strings
            // for the opaque one, arrays where the type is one.
            let mut fields = serde_json::Map::new();
            for (key, _) in &object.fields {
                let Some(partner) = numeric_partner(key) else {
                    continue;
                };
                let Some((_, numeric_type)) = object.fields.iter().find(|(k, _)| *k == partner)
                else {
                    continue;
                };
                if numeric_type.starts_with("Array<") {
                    fields.insert(partner, json!([7, 8]));
                    fields.insert(key.clone(), json!(["x", "y"]));
                } else {
                    fields.insert(partner, json!(7));
                    fields.insert(key.clone(), json!("x"));
                }
                covered += 1;
            }
            let mut value = Value::Object(fields);
            withhold(&mut value);
            assert!(
                leaks(&value).is_empty(),
                "{} still serves {:?}",
                object.name,
                leaks(&value)
            );
        }
        // Not vacuous: the parse found the families.
        assert!(covered >= 40, "only {covered} id pairs found in api.ts");
    }

    #[test]
    fn no_type_with_an_opaque_id_has_a_numeric_id_without_one() {
        let types = object_types();
        let mut allowed_seen = 0;
        for object in types.iter().filter(|object| has_opaque_id(object)) {
            for (key, ty) in &object.fields {
                let Some(opaque) = opaque_partner(key) else {
                    continue;
                };
                if !ty.contains("number") || object.fields.iter().any(|(k, _)| *k == opaque) {
                    continue;
                }
                assert!(
                    UNPAIRED_NUMERIC_IDS.contains(&(object.name.as_str(), key.as_str())),
                    "{}.{key} is a numeric id in a type that has opaque ids, but has no \
                     `{opaque}` beside it: add the opaque field (named after it, so the \
                     numeric one is withheld in stage 2b, see numeric_ids.rs) or list it in \
                     UNPAIRED_NUMERIC_IDS with the reason",
                    object.name
                );
                allowed_seen += 1;
            }
        }
        assert_eq!(
            allowed_seen,
            UNPAIRED_NUMERIC_IDS.len(),
            "UNPAIRED_NUMERIC_IDS has an entry no type needs any more"
        );
    }

    /// Ids that no consumer used are not served at all (DS-534), and the
    /// count that replaces `deletedJourneyIds` is.
    #[test]
    fn ids_with_no_use_are_gone_from_the_api_types() {
        let types = object_types();
        let fields = |name: &str| {
            types
                .iter()
                .find(|object| object.name == name)
                .unwrap_or_else(|| panic!("{name} is not in api.ts"))
                .fields
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>()
        };
        assert!(!fields("RevisionLeg").contains(&"ticketIds"));
        assert!(!fields("JourneyTemplateLegDetailResponse").contains(&"id"));
        assert!(!fields("RecentlyDeletedJourney").contains(&"journeyId"));
        assert!(!fields("ConnectedAppDeletion").contains(&"journeyId"));
        for name in [
            "TimeAwayResponse",
            "SetTemplateActiveResponse",
            "SkipTemplateDateResponse",
        ] {
            let found = fields(name);
            assert!(found.contains(&"deletedJourneyCount"), "{name}");
            assert!(found.contains(&"deletedJourneyIds"), "{name}");
        }
    }
}
