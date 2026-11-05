//! The `OpenAPI` 3.1 document of the token listener's `/v1` routes
//! (docs/specs/personal-api-tokens.md §12), generated from the code:
//!
//! - the paths, scopes and cache lifetimes from [`TOKEN_ROUTES`];
//! - each typed response body's JSON Schema (2020-12, which `OpenAPI` 3.1
//!   uses as is) derived by `schemars` from the same Rust types the
//!   handlers serialize, next to their `ts_rs::TS`;
//! - the envelope, the attribution block and the error body by hand,
//!   because `gate::envelope_and_cache` builds them by hand.
//!
//! The line status, station disruption and departures routes return
//! TfL-shaped JSON built as `serde_json::Value` (`crate::render`), so their
//! items are documented as open objects: their shape is the website's
//! public one (docs/public-api.md), and the breaking-change check cannot
//! see inside them.
//!
//! `api-types/openapi/v1.json` is the committed copy;
//! `openapi_document_is_current` fails when it is stale. Regenerate with
//!
//! ```sh
//! UPDATE_OPENAPI=1 cargo test -p api openapi_document_is_current
//! ```
//!
//! CI compares the committed copy with main's and fails on a breaking
//! change (`oasdiff breaking`, Distant-Signal/ci's `openapi-breaking`
//! action). A breaking change needs a `/v2`, never an edit to v1.

use std::sync::OnceLock;

use schemars::generate::SchemaSettings;
use serde_json::{Map, Value, json};

use super::{PersonalScope, RouteClass, TOKEN_ROUTES, TokenRoute};

/// The committed copy, relative to this crate's manifest directory.
#[cfg(test)]
pub(crate) const PATH: &str = "../../api-types/openapi/v1.json";

/// The document's own route on the token listener.
pub const OPENAPI_PATH: &str = "/v1/openapi.json";

/// The public base URL of the production deployment, for `servers`.
const SERVER_URL: &str = "https://ds-api.cursed.solutions";

/// What a route's `data` is.
enum Body {
    /// A schema: a `$ref` into the components that `schemars` derived, or
    /// one written here (a small object, or open TfL-shaped objects).
    Data(Value),
    /// The document itself (not wrapped).
    Document,
}

/// One route parameter.
struct Param {
    name: &'static str,
    location: &'static str,
    required: bool,
    schema: Value,
    description: &'static str,
}

fn param(
    name: &'static str,
    location: &'static str,
    required: bool,
    schema: Value,
    description: &'static str,
) -> Param {
    Param {
        name,
        location,
        required,
        schema,
        description,
    }
}

fn tfl_items(what: &str) -> Body {
    Body::Data(json!({
        "type": "array",
        "description": format!(
            "{what}, in the TfL-shaped form the website's public routes serve \
             (docs/public-api.md). Each item is an object; its fields are not \
             described here."
        ),
        "items": { "type": "object" },
    }))
}

fn crs() -> Value {
    json!({ "type": "string", "pattern": "^[A-Za-z]{3}$" })
}

fn detail() -> Param {
    param(
        "detail",
        "query",
        false,
        json!({ "type": "boolean", "default": false }),
        "Include each status's disruption details.",
    )
}

struct Generator(schemars::SchemaGenerator);

impl Generator {
    fn new() -> Self {
        let settings = SchemaSettings::draft2020_12().with(|settings| {
            settings.definitions_path = "/components/schemas".into();
        });
        Self(settings.into_generator())
    }

    fn typed<T: schemars::JsonSchema>(&mut self) -> Body {
        Body::Data(self.0.subschema_for::<T>().to_value())
    }

    fn array_of<T: schemars::JsonSchema>(&mut self) -> Body {
        let items = self.0.subschema_for::<T>().to_value();
        Body::Data(json!({ "type": "array", "items": items }))
    }
}

/// The summary, body and parameters of each route in [`TOKEN_ROUTES`];
/// `None` for a route this function does not know, which
/// `the_document_covers_exactly_the_token_routes` turns into a failure.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per route, in TOKEN_ROUTES order"
)]
fn describe(generator: &mut Generator, path: &str) -> Option<(&'static str, Body, Vec<Param>)> {
    let string = || json!({ "type": "string" });
    Some(match path {
        "/v1/status" => (
            "Whether the API is up. Needs no token.",
            Body::Data(json!({
                "type": "object",
                "required": ["ok"],
                "properties": { "ok": { "type": "boolean", "const": true } },
            })),
            vec![],
        ),
        OPENAPI_PATH => ("This document. Needs no token.", Body::Document, vec![]),
        "/v1/me" => (
            "The token making the request: its name, scopes and lifetime.",
            generator.typed::<super::MeResponse>(),
            vec![],
        ),
        "/v1/Journeys/mine" => (
            "Your journeys in travel order: what is still to come or under way first (soonest first), then what you have travelled (most recent first). Only your own: journeys you follow or that were shared with you are not included.",
            generator.array_of::<crate::data::journeys::JourneyListItem>(),
            vec![],
        ),
        "/v1/Journeys/{journey_id}" => (
            "One of your journeys, with its legs and their live state.",
            generator.typed::<crate::routes::journeys::JourneyDetailResponse>(),
            vec![param(
                "journey_id",
                "path",
                true,
                json!({ "type": "integer", "format": "int64" }),
                "The journey's id, from /v1/Journeys/mine.",
            )],
        ),
        "/v1/Line/Mode/{mode}/Status" => (
            "The status of every line of one or more modes.",
            tfl_items("TfL-shaped Line objects with their lineStatuses"),
            vec![
                param(
                    "mode",
                    "path",
                    true,
                    string(),
                    "One or more modes, comma-separated, e.g. national-rail or tube,national-rail.",
                ),
                detail(),
            ],
        ),
        "/v1/Line/{ids}/Status" => (
            "The status of one or more lines.",
            tfl_items("TfL-shaped Line objects with their lineStatuses"),
            vec![
                param(
                    "ids",
                    "path",
                    true,
                    string(),
                    "One or more line ids, comma-separated.",
                ),
                detail(),
            ],
        ),
        "/v1/StopPoint/{crs}/Disruption" => (
            "Disruption affecting the lines that serve a station.",
            tfl_items("TfL-shaped Disruption objects"),
            vec![param(
                "crs",
                "path",
                true,
                crs(),
                "The station's three-letter code.",
            )],
        ),
        "/v1/Train/by-uid/{train_uid}/{date}" => (
            "A train by its timetable id and date, with its calls and live state.",
            generator.typed::<crate::routes::train::PublicTrainResponse>(),
            vec![
                param(
                    "train_uid",
                    "path",
                    true,
                    string(),
                    "The timetable's train id, e.g. C11052.",
                ),
                param(
                    "date",
                    "path",
                    true,
                    json!({ "type": "string", "format": "date" }),
                    "The service date (YYYY-MM-DD).",
                ),
            ],
        ),
        "/v1/island-of-ireland/trains/{train_code}/{date}" => (
            "An Iarnród Éireann train by its code and the day it started, as the live departure boards showed it: its stations with their times and its last report. Kept 14 days (30 for a tracked train); a train no board has shown is not found.",
            generator.typed::<crate::routes::island_of_ireland::IslandOfIrelandTrainResponse>(),
            vec![
                param(
                    "train_code",
                    "path",
                    true,
                    string(),
                    "The train code, 1 to 8 letters and digits, e.g. A120.",
                ),
                param(
                    "date",
                    "path",
                    true,
                    json!({ "type": "string", "format": "date" }),
                    "The day the train started (YYYY-MM-DD), in Irish time.",
                ),
            ],
        ),
        "/v1/stations/{crs}/departures" => (
            "Live departures from a station.",
            tfl_items("Departure objects"),
            vec![
                param(
                    "crs",
                    "path",
                    true,
                    crs(),
                    "The station's three-letter code.",
                ),
                param(
                    "callingAt",
                    "query",
                    false,
                    crs(),
                    "Only trains that call at this station later.",
                ),
            ],
        ),
        "/v1/preferences" => (
            "Your pinned lines, stations and operators.",
            generator.typed::<crate::routes::preferences::PreferencesResponse>(),
            vec![],
        ),
        "/v1/preferences/notifications" => (
            "Your notification settings.",
            generator.typed::<crate::routes::preferences::NotificationPreferences>(),
            vec![],
        ),
        _ => return None,
    })
}

fn error_response(description: &str) -> Value {
    json!({
        "description": description,
        "content": {
            "application/json": { "schema": { "$ref": "#/components/schemas/ErrorBody" } }
        },
    })
}

fn ok_response(route: &TokenRoute, body: Body) -> Value {
    match body {
        Body::Document => json!({
            "description": "The `OpenAPI` document.",
            "content": { "application/json": { "schema": { "type": "object" } } },
        }),
        Body::Data(data) => json!({
            "description": "The data, with its attribution.",
            "headers": {
                "Cache-Control": {
                    "description": if route.max_age_secs == 0 {
                        "no-store".to_string()
                    } else {
                        format!("private, max-age={}", route.max_age_secs)
                    },
                    "schema": { "type": "string" },
                },
                "ETag": {
                    "description": "Weak; send it back as If-None-Match for a 304.",
                    "schema": { "type": "string" },
                },
                "DS-Poll-Interval": {
                    "description": "The shortest sensible poll interval, in seconds.",
                    "schema": { "type": "integer" },
                },
            },
            "content": {
                "application/json": {
                    "schema": {
                        "type": "object",
                        "required": ["data", "attribution"],
                        "properties": {
                            "data": data,
                            "attribution": { "$ref": "#/components/schemas/Attribution" },
                        },
                    }
                }
            },
        }),
    }
}

fn operation(route: &TokenRoute, generator: &mut Generator) -> Option<Value> {
    let (summary, body, params) = describe(generator, route.path)?;
    let mut responses = Map::new();
    responses.insert("200".to_string(), ok_response(route, body));
    if route.class != RouteClass::Open {
        responses.insert(
            "304".to_string(),
            json!({ "description": "Not modified (If-None-Match matched the ETag)." }),
        );
        responses.insert(
            "401".to_string(),
            error_response("No token, or a malformed, unknown, revoked, expired or idle one."),
        );
        if route.scope.is_some() {
            responses.insert(
                "403".to_string(),
                error_response("The token lacks this route's scope (`insufficient_scope`)."),
            );
        }
        responses.insert(
            "429".to_string(),
            json!({
                "description": "Rate limited (`rate_limited`); wait Retry-After seconds.",
                "headers": { "Retry-After": { "schema": { "type": "integer" } } },
                "content": {
                    "application/json": {
                        "schema": { "$ref": "#/components/schemas/ErrorBody" }
                    }
                },
            }),
        );
    }
    let mut op = Map::new();
    op.insert("summary".to_string(), json!(summary));
    op.insert("operationId".to_string(), json!(operation_id(route.path)));
    if !params.is_empty() {
        let params = params
            .into_iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "in": p.location,
                    "required": p.required,
                    "schema": p.schema,
                    "description": p.description,
                })
            })
            .collect();
        op.insert("parameters".to_string(), Value::Array(params));
    }
    if route.class == RouteClass::Open {
        op.insert("security".to_string(), json!([]));
    }
    if let Some(scope) = route.scope {
        op.insert("x-ds-scope".to_string(), json!(scope.as_str()));
    }
    op.insert("responses".to_string(), Value::Object(responses));
    Some(json!({ "get": op }))
}

/// `getV1JourneysMine` from `/v1/Journeys/mine`: a stable id per path.
fn operation_id(path: &str) -> String {
    let mut id = String::from("get");
    for part in path.split('/').filter(|part| !part.is_empty()) {
        let part = part.trim_start_matches('{').trim_end_matches('}');
        for word in part.split(['-', '_', '.']) {
            let mut chars = word.chars();
            if let Some(first) = chars.next() {
                id.extend(first.to_uppercase());
                id.push_str(chars.as_str());
            }
        }
    }
    id
}

fn attribution_schema() -> Value {
    json!({
        "type": "object",
        "description": "The credits the data needs. Show them wherever you show the data.",
        "required": ["sources", "statement"],
        "properties": {
            "sources": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["name", "notice"],
                    "properties": {
                        "name": { "type": "string" },
                        "notice": { "type": "string", "description": "The wording to show, unchanged." },
                        "url": { "type": "string", "format": "uri" },
                    },
                },
            },
            "statement": { "type": "string", "description": "Distant Signal's non-affiliation statement." },
        },
    })
}

fn build() -> Value {
    let mut generator = Generator::new();
    let mut paths = Map::new();
    for route in TOKEN_ROUTES {
        if let Some(operation) = operation(route, &mut generator) {
            paths.insert(route.path.to_string(), operation);
        }
    }
    let mut schemas = Map::new();
    for (name, schema) in generator.0.take_definitions(true) {
        schemas.insert(name, schema);
    }
    schemas.insert("Attribution".to_string(), attribution_schema());
    schemas.insert(
        "ErrorBody".to_string(),
        json!({
            "type": "object",
            "required": ["error", "message"],
            "properties": {
                "error": { "type": "string", "description": "A stable code, e.g. invalid_token." },
                "message": { "type": "string" },
            },
        }),
    );
    let scopes: Vec<String> = PersonalScope::ALL
        .iter()
        .map(|scope| format!("`{}`", scope.as_str()))
        .collect();
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Distant Signal personal API",
            "version": "1",
            "description": format!(
                "Read-only access to your own Distant Signal data with a personal API token \
                 (`dsp_...`), created on the Connected apps page. Send it as \
                 `Authorization: Bearer <token>`. Each route needs one of the token's scopes \
                 ({}), named in its `x-ds-scope`. Every 2xx body is `{{data, attribution}}`; \
                 show the attribution with the data. Poll no more often than \
                 `DS-Poll-Interval` (60 s).",
                scopes.join(", ")
            ),
            "license": { "name": "AGPL-3.0-or-later", "identifier": "AGPL-3.0-or-later" },
        },
        "servers": [{ "url": SERVER_URL }],
        "security": [{ "personalToken": [] }],
        "paths": paths,
        "components": {
            "securitySchemes": {
                "personalToken": {
                    "type": "http",
                    "scheme": "bearer",
                    "description": "A personal API token: dsp_ followed by 38 letters and digits.",
                },
            },
            "schemas": schemas,
        },
    })
}

/// The document, built once.
pub fn document() -> &'static Value {
    static DOCUMENT: OnceLock<Value> = OnceLock::new();
    DOCUMENT.get_or_init(build)
}

/// The committed file's text: pretty JSON and a final newline.
#[cfg(test)]
pub(crate) fn render() -> String {
    let mut text = serde_json::to_string_pretty(document()).unwrap_or_default();
    text.push('\n');
    text
}

/// `GET /v1/openapi.json`, served as is (the envelope skips it).
pub(super) async fn serve() -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut response = axum::Json(document()).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("public, max-age=3600"),
    );
    response
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn openapi_document_is_current() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(PATH);
        let rendered = render();
        if std::env::var_os("UPDATE_OPENAPI").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &rendered).unwrap();
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == rendered,
            "api-types/openapi/v1.json is stale: run \
             `UPDATE_OPENAPI=1 cargo test -p api openapi_document_is_current`"
        );
    }

    /// Every route is documented once, and nothing else is.
    #[test]
    fn the_document_covers_exactly_the_token_routes() {
        let paths = document()["paths"].as_object().unwrap();
        let mut documented: Vec<&str> = paths.keys().map(String::as_str).collect();
        let mut routes: Vec<&str> = TOKEN_ROUTES.iter().map(|route| route.path).collect();
        documented.sort_unstable();
        routes.sort_unstable();
        assert_eq!(documented, routes);
        for route in TOKEN_ROUTES {
            assert_eq!(
                paths[route.path]["get"]["x-ds-scope"].as_str(),
                route.scope.map(PersonalScope::as_str),
                "{}",
                route.path
            );
        }
    }

    /// Every `$ref` points at a schema the document defines.
    #[test]
    fn every_reference_resolves() {
        fn walk(value: &Value, refs: &mut Vec<String>) {
            match value {
                Value::Object(map) => {
                    if let Some(Value::String(target)) = map.get("$ref") {
                        refs.push(target.clone());
                    }
                    map.values().for_each(|v| walk(v, refs));
                }
                Value::Array(items) => items.iter().for_each(|v| walk(v, refs)),
                _ => {}
            }
        }
        let document = document();
        let mut refs = Vec::new();
        walk(document, &mut refs);
        assert_ne!(refs, [] as [String; 0]);
        let schemas = document["components"]["schemas"].as_object().unwrap();
        for target in refs {
            let name = target
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("{target} is not a component reference"));
            assert!(schemas.contains_key(name), "{target} does not resolve");
        }
    }

    #[test]
    fn operation_ids_are_stable_and_unique() {
        assert_eq!(operation_id("/v1/Journeys/mine"), "getV1JourneysMine");
        assert_eq!(
            operation_id("/v1/Train/by-uid/{train_uid}/{date}"),
            "getV1TrainByUidTrainUidDate"
        );
        let mut ids: Vec<String> = TOKEN_ROUTES.iter().map(|r| operation_id(r.path)).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), TOKEN_ROUTES.len());
    }
}
