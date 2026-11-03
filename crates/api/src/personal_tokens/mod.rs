//! Read-only personal API tokens (docs/specs/personal-api-tokens.md; the
//! operator's view is docs/personal-api-tokens.md).
//!
//! A user creates a token on the Connected apps page
//! (`routes::personal_tokens`, session cookie only) and uses it from
//! something they run themselves. The token works **only** on the token
//! listener (`PERSONAL_TOKENS_LISTEN_ADDR`, port 8083), which serves the
//! [`router`] below and nothing else: an allowlist of `/v1` read routes
//! ([`TOKEN_ROUTES`]). Decision D-14, as amended on 2026-11-03 (spec §4).
//!
//! The pieces, outermost first:
//!
//! 1. [`gate::strip_ambient_credentials`]: removes `Cookie`, `X-Real-IP`
//!    and `X-Forwarded-*`, so no cookie or client-chosen address reaches a
//!    handler on this listener.
//! 2. [`gate::token_gate`]: the bearer, its format and checksum, the lookup
//!    by SHA-256 (constant-time compared), expiry and idle expiry, the
//!    route's scope, the rate buckets, and the audit line.
//! 3. [`gate::envelope_and_cache`]: `{data, attribution}` around a JSON
//!    `2xx`, `{error, message}` for an error, and `Cache-Control`, `ETag`
//!    and `DS-Poll-Interval`.
//!
//! Handlers reading user data take `DelegatedOrSessionUser`, which reads
//! the [`PersonalTokenAuth`] the gate leaves and checks the scope again.
//!
//! On every other listener a `dsp_` bearer is refused before anything else
//! looks at it ([`refuse_on_main_listener`]).

pub mod gate;
pub mod openapi;
pub mod token;

use std::net::IpAddr;

use anyhow::{Context, Result, ensure};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use serde::{Deserialize, Serialize};

use crate::app::{App, Router};
use crate::rate_limit::KeyedLimiter;

/// A scope a personal token may hold (spec §2.4). All read only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum PersonalScope {
    #[serde(rename = "journeys:read")]
    JourneysRead,
    #[serde(rename = "lines:read")]
    LinesRead,
    #[serde(rename = "trains:read")]
    TrainsRead,
    #[serde(rename = "preferences:read")]
    PreferencesRead,
}

impl PersonalScope {
    pub const ALL: [PersonalScope; 4] = [
        PersonalScope::JourneysRead,
        PersonalScope::LinesRead,
        PersonalScope::TrainsRead,
        PersonalScope::PreferencesRead,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            PersonalScope::JourneysRead => "journeys:read",
            PersonalScope::LinesRead => "lines:read",
            PersonalScope::TrainsRead => "trains:read",
            PersonalScope::PreferencesRead => "preferences:read",
        }
    }

    pub fn parse(value: &str) -> Option<PersonalScope> {
        PersonalScope::ALL
            .into_iter()
            .find(|scope| scope.as_str() == value)
    }

    /// The personal scope that stands for a delegated one, where the two
    /// name the same read (`DelegatedOrSessionUser` on the token listener).
    pub fn for_delegated(scope: crate::auth::delegated::Scope) -> Option<PersonalScope> {
        use crate::auth::delegated::Scope;
        match scope {
            Scope::JourneysRead => Some(PersonalScope::JourneysRead),
            Scope::PreferencesRead => Some(PersonalScope::PreferencesRead),
            Scope::JourneysWrite | Scope::JourneysDelete => None,
        }
    }
}

/// Which rate bucket set a route is charged to (spec §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteClass {
    /// No token needed (`/v1/status`).
    Open,
    /// The token, user and pod-wide buckets.
    Read,
    /// Those, plus the smaller train bucket (`/v1/Train/*`, `/v1/stations/*`).
    Train,
}

/// A data source a route's response can carry (spec §5.2). The wording is
/// `crate::render`'s credits (shared with the public routes and feeds), and
/// the website's (`frontend/components/OpenDataAttribution.tsx`), checked
/// by `the_attribution_matches_the_website`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Source {
    pub name: &'static str,
    pub notice: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<&'static str>,
}

pub const NATIONAL_RAIL: Source = Source {
    name: "National Rail",
    notice: crate::render::NATIONAL_RAIL_CREDIT,
    url: Some("https://www.nationalrail.co.uk"),
};
pub const NETWORK_RAIL: Source = Source {
    name: "Network Rail",
    notice: "Contains Information of Network Rail Infrastructure Limited licensed under the following licence",
    url: Some(
        "https://www.networkrail.co.uk/who-we-are/transparency-and-ethics/transparency/open-data-feeds/network-rail-infrastructure-limited-data-feeds-licence/",
    ),
};
pub const RAIL_DELIVERY_GROUP: Source = Source {
    name: "Rail Delivery Group",
    notice: crate::render::KNOWLEDGEBASE_INCIDENTS_CREDIT,
    url: None,
};
pub const RSP: Source = Source {
    name: "RSP",
    notice: "Source: RSP",
    url: None,
};
pub const TFL: Source = Source {
    name: "Transport for London",
    notice: crate::render::TFL_CREDIT,
    url: Some("https://tfl.gov.uk/corporate/terms-and-conditions/transport-data-service"),
};

/// The non-affiliation statement every token response carries, the
/// website's own (`NON_AFFILIATION_STATEMENT`).
pub const NON_AFFILIATION: &str = "Distant Signal is an independent, unofficial service and is not affiliated with or endorsed by National Rail, Network Rail, Rail Delivery Group, TfL, the National Transport Authority, Iarnród Éireann, Translink or any train operator.";

const LINE_SOURCES: &[Source] = &[NATIONAL_RAIL, RAIL_DELIVERY_GROUP, TFL];
const TRAIN_SOURCES: &[Source] = &[NATIONAL_RAIL, NETWORK_RAIL, RSP, RAIL_DELIVERY_GROUP];
const DEPARTURE_SOURCES: &[Source] = &[NATIONAL_RAIL, NETWORK_RAIL, RSP];
const JOURNEY_SOURCES: &[Source] = &[NATIONAL_RAIL, NETWORK_RAIL, RSP, RAIL_DELIVERY_GROUP, TFL];

/// One `/v1` route a personal token may reach (spec §5.1).
#[derive(Debug, Clone, Copy)]
pub struct TokenRoute {
    /// The matched path, as axum reports it.
    pub path: &'static str,
    /// `None`: any valid token (`/v1/me`) or none at all ([`RouteClass::Open`]).
    pub scope: Option<PersonalScope>,
    pub class: RouteClass,
    /// `Cache-Control: private, max-age=` this; 0 sends `no-store`.
    pub max_age_secs: u32,
    pub sources: &'static [Source],
}

/// Every route on the token listener. [`router`] mounts exactly these;
/// `the_router_and_the_table_agree` pins the two together.
pub const TOKEN_ROUTES: &[TokenRoute] = &[
    TokenRoute {
        path: "/v1/status",
        scope: None,
        class: RouteClass::Open,
        max_age_secs: 60,
        sources: &[],
    },
    TokenRoute {
        path: openapi::OPENAPI_PATH,
        scope: None,
        class: RouteClass::Open,
        max_age_secs: 3600,
        sources: &[],
    },
    TokenRoute {
        path: "/v1/me",
        scope: None,
        class: RouteClass::Read,
        max_age_secs: 0,
        sources: &[],
    },
    TokenRoute {
        path: "/v1/Journeys/mine",
        scope: Some(PersonalScope::JourneysRead),
        class: RouteClass::Read,
        max_age_secs: 30,
        sources: JOURNEY_SOURCES,
    },
    TokenRoute {
        path: "/v1/Journeys/{journey_id}",
        scope: Some(PersonalScope::JourneysRead),
        class: RouteClass::Read,
        max_age_secs: 30,
        sources: JOURNEY_SOURCES,
    },
    TokenRoute {
        path: "/v1/Line/Mode/{mode}/Status",
        scope: Some(PersonalScope::LinesRead),
        class: RouteClass::Read,
        max_age_secs: 60,
        sources: LINE_SOURCES,
    },
    TokenRoute {
        path: "/v1/Line/{ids}/Status",
        scope: Some(PersonalScope::LinesRead),
        class: RouteClass::Read,
        max_age_secs: 60,
        sources: LINE_SOURCES,
    },
    TokenRoute {
        path: "/v1/StopPoint/{crs}/Disruption",
        scope: Some(PersonalScope::LinesRead),
        class: RouteClass::Read,
        max_age_secs: 60,
        sources: LINE_SOURCES,
    },
    TokenRoute {
        path: "/v1/Train/by-uid/{train_uid}/{date}",
        scope: Some(PersonalScope::TrainsRead),
        class: RouteClass::Train,
        max_age_secs: 30,
        sources: TRAIN_SOURCES,
    },
    TokenRoute {
        path: "/v1/stations/{crs}/departures",
        scope: Some(PersonalScope::TrainsRead),
        class: RouteClass::Train,
        max_age_secs: 30,
        sources: DEPARTURE_SOURCES,
    },
    TokenRoute {
        path: "/v1/preferences",
        scope: Some(PersonalScope::PreferencesRead),
        class: RouteClass::Read,
        max_age_secs: 300,
        sources: &[],
    },
    TokenRoute {
        path: "/v1/preferences/notifications",
        scope: Some(PersonalScope::PreferencesRead),
        class: RouteClass::Read,
        max_age_secs: 300,
        sources: &[],
    },
];

/// The [`TOKEN_ROUTES`] entry for a matched path.
pub fn token_route(matched_path: &str) -> Option<&'static TokenRoute> {
    TOKEN_ROUTES.iter().find(|route| route.path == matched_path)
}

/// The lifetimes the page offers, in days (spec §2.3), before
/// `PERSONAL_TOKENS_MAX_LIFETIME_DAYS` trims them.
pub const LIFETIME_CHOICES: [i64; 4] = [7, 30, 90, 365];

/// The `DS-Poll-Interval` hint, in seconds (spec §6.3).
pub const POLL_INTERVAL_SECS: u32 = 60;

/// Settings from the environment (chart `api.personalTokens`).
#[derive(Debug, Clone, PartialEq, Eq, clap::Parser)]
#[command(name = "api-personal-tokens", no_binary_name = true)]
pub struct PersonalTokenSettings {
    /// Master switch, off by default. Off: no listener, the management
    /// routes answer 404, and the sweep does not run.
    #[arg(long, env = "PERSONAL_TOKENS_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub enabled: bool,
    /// The token listener. Bound only while enabled.
    #[arg(
        long,
        env = "PERSONAL_TOKENS_LISTEN_ADDR",
        default_value = "0.0.0.0:8083"
    )]
    pub listen_addr: String,
    /// The API hostname's base URL, shown on the Connected apps page.
    #[arg(long, env = "PERSONAL_TOKENS_PUBLIC_BASE_URL", default_value = "")]
    pub public_base_url: String,
    /// Key the failed-authentication bucket on `CF-Connecting-IP` rather
    /// than the socket peer (the cloudflared pod). Safe only while the
    /// `NetworkPolicy` admits nothing but cloudflared to the port (spec §6.2).
    #[arg(long, env = "PERSONAL_TOKENS_TRUST_CF_CONNECTING_IP", default_value_t = false, action = clap::ArgAction::Set)]
    pub trust_cf_connecting_ip: bool,
    /// Working tokens one user may hold.
    #[arg(
        long,
        env = "PERSONAL_TOKENS_MAX_ACTIVE_PER_USER",
        default_value_t = 10
    )]
    pub max_active_per_user: i64,
    /// The longest lifetime the page offers.
    #[arg(long, env = "PERSONAL_TOKENS_MAX_LIFETIME_DAYS", default_value_t = 365)]
    pub max_lifetime_days: i64,
    /// A token unused this long is revoked (`inactive`).
    #[arg(long, env = "PERSONAL_TOKENS_IDLE_DAYS", default_value_t = 90)]
    pub idle_days: i64,
    /// Days an ended token's row is kept before the sweep deletes it. Also
    /// read by the `maintenance` binary.
    #[arg(
        long,
        env = "PERSONAL_TOKENS_REVOKED_RETENTION_DAYS",
        default_value_t = 365
    )]
    pub revoked_retention_days: i64,
    #[arg(
        long,
        env = "PERSONAL_TOKENS_RATE_TOKEN_PER_MINUTE",
        default_value_t = 30
    )]
    pub token_per_minute: u32,
    #[arg(long, env = "PERSONAL_TOKENS_RATE_TOKEN_BURST", default_value_t = 10)]
    pub token_burst: u32,
    #[arg(
        long,
        env = "PERSONAL_TOKENS_RATE_TRAINS_PER_MINUTE",
        default_value_t = 10
    )]
    pub trains_per_minute: u32,
    #[arg(long, env = "PERSONAL_TOKENS_RATE_TRAINS_BURST", default_value_t = 5)]
    pub trains_burst: u32,
    #[arg(
        long,
        env = "PERSONAL_TOKENS_RATE_USER_PER_MINUTE",
        default_value_t = 60
    )]
    pub user_per_minute: u32,
    #[arg(long, env = "PERSONAL_TOKENS_RATE_USER_BURST", default_value_t = 20)]
    pub user_burst: u32,
    #[arg(
        long,
        env = "PERSONAL_TOKENS_RATE_GLOBAL_PER_MINUTE",
        default_value_t = 1200
    )]
    pub global_per_minute: u32,
    #[arg(long, env = "PERSONAL_TOKENS_RATE_GLOBAL_BURST", default_value_t = 200)]
    pub global_burst: u32,
    #[arg(
        long,
        env = "PERSONAL_TOKENS_RATE_AUTH_FAILURE_PER_MINUTE",
        default_value_t = 10
    )]
    pub auth_failure_per_minute: u32,
    #[arg(
        long,
        env = "PERSONAL_TOKENS_RATE_AUTH_FAILURE_BURST",
        default_value_t = 10
    )]
    pub auth_failure_burst: u32,
}

impl Default for PersonalTokenSettings {
    #[expect(
        clippy::expect_used,
        reason = "the defaults are constants checked by a test"
    )]
    fn default() -> Self {
        use clap::Parser;
        Self::try_parse_from(std::iter::empty::<String>())
            .expect("the personal-token defaults parse")
    }
}

impl PersonalTokenSettings {
    pub fn from_env() -> Result<Self> {
        use clap::Parser;
        let settings = Self::try_parse_from(std::iter::empty::<String>())
            .context("invalid personal-token settings in the environment")?;
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            (1..=100).contains(&self.max_active_per_user),
            "PERSONAL_TOKENS_MAX_ACTIVE_PER_USER must be 1 to 100"
        );
        ensure!(
            (LIFETIME_CHOICES[0]..=LIFETIME_CHOICES[3]).contains(&self.max_lifetime_days),
            "PERSONAL_TOKENS_MAX_LIFETIME_DAYS must be 7 to 365"
        );
        ensure!(
            (1..=3650).contains(&self.idle_days),
            "PERSONAL_TOKENS_IDLE_DAYS must be 1 to 3650"
        );
        ensure!(
            (1..=36_500).contains(&self.revoked_retention_days),
            "PERSONAL_TOKENS_REVOKED_RETENTION_DAYS must be 1 to 36500"
        );
        for (name, value) in [
            ("TOKEN", self.token_per_minute),
            ("TOKEN_BURST", self.token_burst),
            ("TRAINS", self.trains_per_minute),
            ("TRAINS_BURST", self.trains_burst),
            ("USER", self.user_per_minute),
            ("USER_BURST", self.user_burst),
            ("GLOBAL", self.global_per_minute),
            ("GLOBAL_BURST", self.global_burst),
            ("AUTH_FAILURE", self.auth_failure_per_minute),
            ("AUTH_FAILURE_BURST", self.auth_failure_burst),
        ] {
            ensure!(
                value > 0,
                "PERSONAL_TOKENS_RATE_{name} (per minute or burst) must be at least 1"
            );
        }
        if self.enabled {
            ensure!(
                !self.listen_addr.trim().is_empty(),
                "PERSONAL_TOKENS_LISTEN_ADDR must be set when PERSONAL_TOKENS_ENABLED is true"
            );
        }
        Ok(())
    }

    /// The lifetimes the page offers.
    pub fn lifetimes(&self) -> Vec<i64> {
        LIFETIME_CHOICES
            .into_iter()
            .filter(|days| *days <= self.max_lifetime_days)
            .collect()
    }
}

/// A limiter bucket on the token listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BucketKey {
    Token(i64),
    TokenTrains(i64),
    /// A 64-bit digest of the user id, so the key never holds it.
    User(u64),
    Global,
    AuthFailure(IpAddr),
}

/// The feature's settings and rate state, in `AppState`. `Default` is off.
#[derive(Default)]
pub struct PersonalTokens {
    pub settings: PersonalTokenSettings,
    pub limiter: KeyedLimiter<BucketKey>,
}

impl std::fmt::Debug for PersonalTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonalTokens")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl PersonalTokens {
    pub fn new(settings: PersonalTokenSettings) -> Self {
        Self {
            settings,
            limiter: KeyedLimiter::default(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.settings.enabled
    }

    /// The token listener's address while enabled.
    pub fn listen_addr(&self) -> Option<&str> {
        self.settings
            .enabled
            .then_some(self.settings.listen_addr.as_str())
    }
}

/// What the gate leaves for the handler: a verified token's identity.
#[derive(Debug, Clone)]
pub struct PersonalTokenAuth {
    pub token_id: i64,
    pub user_id: String,
    pub name: String,
    pub scopes: Vec<PersonalScope>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// On the public and delegated listeners: a `dsp_` bearer is refused
/// before the rate limiter or `delegated::gate` sees it, and never looked
/// up (spec §3.3). Layer it outermost on the main router.
pub async fn refuse_on_main_listener(request: Request, next: Next) -> Response {
    if let Some(token) = crate::auth::bearer_token(request.headers())
        && token::looks_like_token(&token)
    {
        metrics::counter!(
            ds_runtime::metrics::metric_name("api_personal_token_rejected_total"),
            "reason" => "wrong_listener"
        )
        .increment(1);
        tracing::warn!(
            audit = true,
            event = "personal_token_rejected",
            reason = "wrong_listener",
            "a personal token was sent to a listener that does not accept it"
        );
        return crate::auth::delegated::error_response(
            StatusCode::UNAUTHORIZED,
            "invalid_token",
            "personal API tokens work only on the API hostname",
        );
    }
    next.run(request).await
}

/// `GET /v1/status`: no token needed, for a blackbox probe.
async fn status() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "ok": true }))
}

/// `GET /v1/me`: the token itself, so a client can check what it holds.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MeResponse {
    name: String,
    scopes: Vec<&'static str>,
    created_at: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
}

async fn me(axum::Extension(auth): axum::Extension<PersonalTokenAuth>) -> axum::Json<MeResponse> {
    axum::Json(MeResponse {
        name: auth.name,
        scopes: auth.scopes.iter().map(|scope| scope.as_str()).collect(),
        created_at: auth.created_at,
        expires_at: auth.expires_at,
    })
}

/// The token listener's routes, nested under `/v1`, with the gate and the
/// response layers. Every route here must be in [`TOKEN_ROUTES`], and only
/// `GET` (and so `HEAD`) is routed: anything else is a 405.
pub fn router(app: &App, edge: &crate::edge::EdgeSettings) -> Router {
    use axum::routing::get;

    use crate::routes::{departures, journeys, line_status, preferences, train};
    let v1 = Router::new()
        .route("/status", get(status))
        .route("/openapi.json", get(openapi::serve))
        .route("/me", get(me))
        .route("/Journeys/mine", get(journeys::get_my_journeys))
        .route("/Journeys/{journey_id}", get(journeys::get_journey))
        .route(
            "/Line/Mode/{mode}/Status",
            get(line_status::get_mode_status),
        )
        .route("/Line/{ids}/Status", get(line_status::get_line_status))
        .route(
            "/StopPoint/{crs}/Disruption",
            get(line_status::get_stop_point_disruption),
        )
        .route(
            "/Train/by-uid/{train_uid}/{date}",
            get(train::get_by_uid_and_date),
        )
        .route(
            "/stations/{crs}/departures",
            get(departures::get_station_departures),
        )
        .route("/preferences", get(preferences::get_preferences))
        .route(
            "/preferences/notifications",
            get(preferences::get_notification_preferences),
        );
    Router::new()
        .nest("/v1", v1)
        // Innermost first: the unavailable mark, the envelope, the time
        // limit, the gate, and the credential strip outermost.
        .layer(axum::middleware::from_fn_with_state(
            edge.unavailable_retry_after(),
            crate::unavailable::annotate_unavailable,
        ))
        .layer(axum::middleware::from_fn(gate::envelope_and_cache))
        .layer(edge.public_timeout_layer())
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            gate::token_gate,
        ))
        .layer(axum::middleware::from_fn(gate::strip_ambient_credentials))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_parse_and_are_off() {
        let settings = PersonalTokenSettings::default();
        assert!(!settings.enabled);
        assert_eq!(settings.listen_addr, "0.0.0.0:8083");
        settings.validate().unwrap();
        assert_eq!(settings.lifetimes(), vec![7, 30, 90, 365]);
        assert!(PersonalTokens::default().listen_addr().is_none());
    }

    #[test]
    fn a_shorter_maximum_lifetime_trims_the_choices() {
        let settings = PersonalTokenSettings {
            max_lifetime_days: 90,
            ..PersonalTokenSettings::default()
        };
        assert_eq!(settings.lifetimes(), vec![7, 30, 90]);
    }

    #[test]
    fn scopes_round_trip_and_match_the_migration() {
        let migration =
            include_str!("../../../ds-store/migrations/20261103120000_personal_api_tokens.sql");
        for scope in PersonalScope::ALL {
            assert_eq!(PersonalScope::parse(scope.as_str()), Some(scope));
            assert!(migration.contains(&format!("'{}'", scope.as_str())));
        }
        assert_eq!(PersonalScope::parse("journeys:write"), None);
    }

    /// Every user-data route needs a scope, and only `/v1/status` and the
    /// `OpenAPI` document are open.
    #[test]
    fn only_the_status_and_document_routes_are_open() {
        let open = ["/v1/status", openapi::OPENAPI_PATH];
        for route in TOKEN_ROUTES {
            assert_eq!(
                route.class == RouteClass::Open,
                open.contains(&route.path),
                "{}",
                route.path
            );
            if !open.contains(&route.path) && route.path != "/v1/me" {
                assert!(route.scope.is_some(), "{} has no scope", route.path);
            }
        }
    }

    /// The router mounts exactly [`TOKEN_ROUTES`]: compared through the
    /// source, since axum cannot list a router's routes.
    #[test]
    fn the_router_and_the_table_agree() {
        let source = include_str!("mod.rs");
        let body = &source[source.find("let v1 = Router::new()").unwrap()..];
        let body = &body[..body.find("Router::new()\n        .nest").unwrap()];
        let mut mounted: Vec<String> = Vec::new();
        for (index, _) in body.match_indices(".route(") {
            let rest = &body[index..];
            let start = rest.find('"').unwrap() + 1;
            let end = start + rest[start..].find('"').unwrap();
            mounted.push(format!("/v1{}", &rest[start..end]));
        }
        let table: Vec<String> = TOKEN_ROUTES
            .iter()
            .map(|route| route.path.to_string())
            .collect();
        assert_eq!(mounted, table);
    }

    /// The notices are the website's own words (spec §5.2).
    #[test]
    fn the_attribution_matches_the_website() {
        let site = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../frontend/components/OpenDataAttribution.tsx"),
        )
        .unwrap();
        for text in [
            NETWORK_RAIL.notice,
            NETWORK_RAIL.url.unwrap(),
            TFL.url.unwrap(),
            NON_AFFILIATION,
        ] {
            assert!(site.contains(text), "{text:?} is not on the website");
        }
        let credit = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../frontend/components/NationalRailCredit.tsx"),
        )
        .unwrap();
        assert!(credit.contains("powered by NationalRail"));
        assert!(credit.contains("(Train Information Services Ltd)"));
        assert!(credit.contains(NATIONAL_RAIL.url.unwrap()));
        let attribution_page = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../frontend/components/OpenDataAttribution.tsx"),
        )
        .unwrap();
        assert!(attribution_page.contains("Powered by TfL Open Data"));
        assert!(attribution_page.contains("Source: RSP"));
    }
}
