//! Test credentials: an optional, off-by-default trust root for automated
//! checks against the read-only reference routes (docs/test-auth.md).
//!
//! When `TEST_AUTH_ENABLED` is true, the personal-token listener (port
//! 8083, `crate::personal_tokens`) also accepts a ds-sso
//! client-credentials JWT from exactly one issuer
//! ([`TEST_AUTH_ISSUER`], provider `distant-signal-test`). Nothing else
//! accepts it:
//!
//! - **Listener.** Only [`crate::personal_tokens::gate::token_gate`] calls
//!   [`serve`]. The main (public) listener and the delegated listener
//!   refuse a bearer naming this issuer or audience before anything else
//!   reads it ([`refuse_off_token_listener`]), enabled or not.
//! - **Verification.** RS256 only (any other `alg`, `none` and `HS*`
//!   included, is refused before the key lookup), the signature against the
//!   issuer's JWKS (OIDC discovery and the cached, cooled-down refetch of
//!   [`ServiceTokenVerifier`], shared with the delegated verifier), the
//!   exact `iss`, `aud`, `exp`, `nbf`, a required `iat`, the delegated
//!   verifier's clock-skew leeway ([`CLOCK_SKEW_LEEWAY`]), a lifetime of at
//!   most [`MAX_TOKEN_LIFETIME_SECS`], a non-empty `sub` and the required
//!   group in `groups`.
//! - **Routes.** `GET` only, and only the [`TOKEN_ROUTES`] entries whose
//!   scope is one of [`ALLOWED_SCOPES`] (`lines:read`, `trains:read`):
//!   reference data. `/v1/me`, journeys and preferences answer `403`.
//! - **Identity.** A synthetic sandbox principal `test:<sub>`
//!   ([`TestPrincipal`]): never a `users` row, no journeys, preferences,
//!   grants or writes. The handlers it reaches take no user.
//! - **Credentials.** A test token with a session cookie, or with a second
//!   `Authorization` header (a `dsp_` personal token among them), is
//!   refused `400 ambiguous_credentials`.
//! - **Rate.** Its own buckets ([`TestBucketKey`]), keyed by a digest of
//!   `sub`, apart from the personal tokens'. Failed verifications charge the
//!   listener's failed-authentication bucket like a bad personal token.
//! - **Audit.** `event=test_auth` per request (`sub`, route, status) and
//!   `event=test_auth_rejected` per refusal. The token is never logged.
//!
//! There is no per-token revocation (the tokens live an hour); the kill
//! switch is `TEST_AUTH_ENABLED=false` (chart `api.testAuth.enabled`).

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::auth::delegated::{ClientConfig, DelegatedTokenCheck, error_response};
use crate::auth::internal_oauth::{Audience, CLOCK_SKEW_LEEWAY, ServiceTokenVerifier};
use crate::personal_tokens::{PersonalScope, RouteClass, TOKEN_ROUTES, TokenRoute};
use crate::rate_limit::KeyedLimiter;

/// The only issuer test credentials may come from: ds-sso's
/// `distant-signal-test` provider. `TEST_AUTH_ISSUER` must equal it exactly
/// when the feature is on.
pub const TEST_AUTH_ISSUER: &str =
    "https://ds-sso.cursed.solutions/application/o/distant-signal-test/";
/// The `aud` a test token carries (the provider's client id).
pub const DEFAULT_AUDIENCE: &str = "distant-signal-test";
/// The group the token's `groups` claim must contain.
pub const DEFAULT_REQUIRED_GROUP: &str = "ds-test";
/// The longest a token may live (`exp` minus `iat`), in seconds. ds-sso
/// issues one-hour tokens and there is no per-token revocation, so a token
/// minted with a long life (a provider setting changed by mistake) is
/// refused rather than trusted for weeks.
pub const MAX_TOKEN_LIFETIME_SECS: i64 = 24 * 60 * 60;
/// The sandbox principal is this prefix and the token's `sub`.
pub const PRINCIPAL_PREFIX: &str = "test:";
/// The scopes a test token holds: reference data only.
pub const ALLOWED_SCOPES: [PersonalScope; 2] =
    [PersonalScope::LinesRead, PersonalScope::TrainsRead];

/// Settings from the environment (chart `api.testAuth`).
#[derive(Debug, Clone, PartialEq, Eq, clap::Parser)]
#[command(name = "api-test-auth", no_binary_name = true)]
pub struct TestAuthSettings {
    /// Master switch, off by default. Off: every test token is refused on
    /// every listener, and no JWKS is ever fetched. Needs
    /// `PERSONAL_TOKENS_ENABLED` (the only listener that accepts one).
    #[arg(long, env = "TEST_AUTH_ENABLED", default_value_t = false, action = clap::ArgAction::Set)]
    pub enabled: bool,
    /// The issuer, which must be [`TEST_AUTH_ISSUER`] exactly while
    /// enabled. Its JWKS is found by OIDC discovery.
    #[arg(long, env = "TEST_AUTH_ISSUER", default_value = TEST_AUTH_ISSUER)]
    pub issuer: String,
    /// The `aud` every test token must carry. Distinct from the SSO,
    /// internal and delegated audiences (the startup guard).
    #[arg(long, env = "TEST_AUTH_AUDIENCE", default_value = DEFAULT_AUDIENCE)]
    pub audience: String,
    /// The group the `groups` claim must contain.
    #[arg(long, env = "TEST_AUTH_REQUIRED_GROUP", default_value = DEFAULT_REQUIRED_GROUP)]
    pub required_group: String,
    /// Requests per `sub` per minute, and the burst.
    #[arg(long, env = "TEST_AUTH_RATE_PER_MINUTE", default_value_t = 30)]
    pub per_minute: u32,
    #[arg(long, env = "TEST_AUTH_RATE_BURST", default_value_t = 10)]
    pub burst: u32,
    /// Requests per `sub` per minute on the train routes, and the burst.
    #[arg(long, env = "TEST_AUTH_RATE_TRAINS_PER_MINUTE", default_value_t = 10)]
    pub trains_per_minute: u32,
    #[arg(long, env = "TEST_AUTH_RATE_TRAINS_BURST", default_value_t = 5)]
    pub trains_burst: u32,
    /// Every test token together per minute, and the burst.
    #[arg(long, env = "TEST_AUTH_RATE_GLOBAL_PER_MINUTE", default_value_t = 120)]
    pub global_per_minute: u32,
    #[arg(long, env = "TEST_AUTH_RATE_GLOBAL_BURST", default_value_t = 30)]
    pub global_burst: u32,
}

impl Default for TestAuthSettings {
    #[expect(
        clippy::expect_used,
        reason = "the defaults are constants checked by a test"
    )]
    fn default() -> Self {
        use clap::Parser;
        Self::try_parse_from(std::iter::empty::<String>()).expect("the test-auth defaults parse")
    }
}

impl TestAuthSettings {
    pub fn from_env() -> Result<Self> {
        use clap::Parser;
        let settings = Self::try_parse_from(std::iter::empty::<String>())
            .context("invalid test-auth settings in the environment")?;
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("PER_MINUTE", self.per_minute),
            ("BURST", self.burst),
            ("TRAINS_PER_MINUTE", self.trains_per_minute),
            ("TRAINS_BURST", self.trains_burst),
            ("GLOBAL_PER_MINUTE", self.global_per_minute),
            ("GLOBAL_BURST", self.global_burst),
        ] {
            ensure!(value > 0, "TEST_AUTH_RATE_{name} must be at least 1");
        }
        if !self.enabled {
            return Ok(());
        }
        ensure!(
            self.issuer == TEST_AUTH_ISSUER,
            "TEST_AUTH_ISSUER must be exactly {TEST_AUTH_ISSUER} when TEST_AUTH_ENABLED is true \
             (got {:?})",
            self.issuer
        );
        ensure!(
            !self.audience.trim().is_empty() && self.audience.trim() == self.audience,
            "TEST_AUTH_AUDIENCE must be a non-empty client id without surrounding spaces"
        );
        ensure!(
            !self.required_group.trim().is_empty()
                && self.required_group.trim() == self.required_group,
            "TEST_AUTH_REQUIRED_GROUP must be a non-empty group name without surrounding spaces"
        );
        Ok(())
    }

    /// The startup guard: while enabled, the test audience differs from
    /// the SSO client id, the internal client id and every delegated
    /// audience, and the test issuer from the SSO, internal and every
    /// delegated issuer, so no token meant for one can pass another's
    /// checks. Also needs the personal-token listener, the only place a
    /// test token is accepted.
    pub fn ensure_distinct(
        &self,
        sso_client_id: &str,
        sso_issuer_url: &str,
        internal_client_id: &str,
        internal_issuer_url: &str,
        delegated: &[ClientConfig],
        personal_tokens_enabled: bool,
    ) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        ensure!(
            personal_tokens_enabled,
            "TEST_AUTH_ENABLED needs PERSONAL_TOKENS_ENABLED: test credentials are accepted only \
             on the personal-token listener"
        );
        let mut audiences = vec![
            ("SSO_CLIENT_ID", sso_client_id),
            ("INTERNAL_OAUTH_CLIENT_ID", internal_client_id),
        ];
        let mut issuers = vec![
            ("SSO_ISSUER_URL", sso_issuer_url),
            ("INTERNAL_OAUTH_ISSUER_URL", internal_issuer_url),
        ];
        for client in delegated {
            audiences.push(("a DELEGATED_CLIENTS audience", client.audience.as_str()));
            issuers.push(("a DELEGATED_CLIENTS issuerUrl", client.issuer_url.as_str()));
        }
        for (name, audience) in audiences {
            ensure!(
                audience != self.audience,
                "TEST_AUTH_AUDIENCE equals {name} ({audience:?}); test credentials need their own \
                 audience"
            );
        }
        for (name, issuer) in issuers {
            ensure!(
                !same_issuer(issuer, &self.issuer),
                "TEST_AUTH_ISSUER equals {name}; test credentials need their own Authentik \
                 provider"
            );
        }
        Ok(())
    }
}

fn same_issuer(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// A bucket of the test-credentials limiter: apart from the personal
/// tokens' (`crate::personal_tokens::BucketKey`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TestBucketKey {
    /// A 64-bit digest of `sub`, so the key never holds it.
    Sub(u64),
    SubTrains(u64),
    Global,
}

/// The feature: its settings, its verifier (only while enabled) and its
/// rate state. `Default` is off.
#[derive(Default)]
pub struct TestAuth {
    pub settings: TestAuthSettings,
    verifier: Option<Box<dyn DelegatedTokenCheck>>,
    pub limiter: KeyedLimiter<TestBucketKey>,
}

impl std::fmt::Debug for TestAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestAuth")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl TestAuth {
    /// The verifier is built (lazily discovering the issuer on first use)
    /// only while enabled.
    pub fn from_settings(settings: TestAuthSettings) -> Result<Self> {
        let verifier: Option<Box<dyn DelegatedTokenCheck>> = if settings.enabled {
            Some(Box::new(
                ServiceTokenVerifier::new(settings.issuer.clone(), settings.audience.clone())
                    .context("test-auth verifier")?,
            ))
        } else {
            None
        };
        Ok(Self {
            settings,
            verifier,
            limiter: KeyedLimiter::default(),
        })
    }

    pub fn enabled(&self) -> bool {
        self.settings.enabled && self.verifier.is_some()
    }

    /// Whether `token`'s unverified `iss` (trailing slash aside) or `aud`
    /// names this trust root: such a token is handled (and most likely
    /// refused) here, never by another verifier.
    pub fn names(&self, token: &str) -> bool {
        let Some(peeked) = peek(token) else {
            return false;
        };
        peeked
            .iss
            .is_some_and(|iss| same_issuer(&iss, &self.settings.issuer))
            || peeked
                .aud
                .is_some_and(|aud| aud.contains(&self.settings.audience))
    }

    /// Whether any `Authorization: Bearer` header of `headers` names this
    /// trust root.
    pub fn named_in(&self, headers: &HeaderMap) -> bool {
        headers
            .get_all(header::AUTHORIZATION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .filter_map(|value| value.strip_prefix("Bearer "))
            .any(|token| self.names(token))
    }

    /// Every check of the module doc's "Verification" item.
    #[expect(
        clippy::cast_possible_wrap,
        reason = "small constant durations, far below i64::MAX seconds"
    )]
    pub(crate) async fn verify(&self, token: &str) -> Result<TestClaims, Refusal> {
        let Some(verifier) = self.verifier.as_ref().filter(|_| self.settings.enabled) else {
            return Err(Refusal::Disabled);
        };
        // The algorithm first: RS256 or nothing, before any key lookup.
        let header = token
            .split('.')
            .next()
            .and_then(|header| URL_SAFE_NO_PAD.decode(header).ok())
            .and_then(|bytes| serde_json::from_slice::<PeekedHeader>(&bytes).ok())
            .ok_or(Refusal::Malformed)?;
        if header.alg != "RS256" {
            return Err(Refusal::Algorithm);
        }
        let signed = verifier
            .verify_signed(token)
            .await
            .map_err(|_| Refusal::Signature)?;
        if signed.typ.as_deref() == Some("logout+jwt") {
            return Err(Refusal::Invalid);
        }
        let claims: TestClaims =
            serde_json::from_slice(&signed.payload).map_err(|_| Refusal::Malformed)?;
        if claims.iss != self.settings.issuer {
            return Err(Refusal::Issuer);
        }
        if !claims.aud.contains(&self.settings.audience) {
            return Err(Refusal::Audience);
        }
        let now = chrono::Utc::now().timestamp();
        let leeway = CLOCK_SKEW_LEEWAY.as_secs() as i64;
        if claims.exp.saturating_add(leeway) <= now {
            return Err(Refusal::Expired);
        }
        if let Some(nbf) = claims.nbf
            && nbf.saturating_sub(leeway) > now
        {
            return Err(Refusal::NotYetValid);
        }
        if claims.iat.saturating_sub(leeway) > now {
            return Err(Refusal::NotYetValid);
        }
        if claims.exp.saturating_sub(claims.iat) > MAX_TOKEN_LIFETIME_SECS {
            return Err(Refusal::Invalid);
        }
        if claims.sub.is_empty() {
            return Err(Refusal::Invalid);
        }
        if !claims.groups.contains(&self.settings.required_group) {
            return Err(Refusal::MissingGroup);
        }
        Ok(claims)
    }
}

/// The claims of a test token. `iat` is required: a token without it
/// fails to parse and is refused.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TestClaims {
    pub(crate) sub: String,
    pub(crate) iss: String,
    pub(crate) aud: Audience,
    pub(crate) exp: i64,
    #[serde(default)]
    pub(crate) nbf: Option<i64>,
    pub(crate) iat: i64,
    #[serde(default)]
    pub(crate) groups: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PeekedHeader {
    alg: String,
}

/// The unverified `iss` and `aud`, used only to decide which path handles
/// (or refuses) a bearer.
#[derive(Debug, Deserialize)]
struct Peeked {
    #[serde(default)]
    iss: Option<String>,
    #[serde(default)]
    aud: Option<Audience>,
}

fn peek(token: &str) -> Option<Peeked> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Why a test token was refused, for the audit line and the metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    Disabled,
    Malformed,
    Algorithm,
    Signature,
    Invalid,
    Issuer,
    Audience,
    Expired,
    NotYetValid,
    MissingGroup,
    AmbiguousCredentials,
    Method,
    InsufficientScope,
    RateLimited,
    WrongListener,
}

impl Refusal {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Refusal::Disabled => "disabled",
            Refusal::Malformed => "malformed",
            Refusal::Algorithm => "algorithm",
            Refusal::Signature => "signature",
            Refusal::Invalid => "invalid",
            Refusal::Issuer => "issuer",
            Refusal::Audience => "audience",
            Refusal::Expired => "expired",
            Refusal::NotYetValid => "not_yet_valid",
            Refusal::MissingGroup => "missing_group",
            Refusal::AmbiguousCredentials => "ambiguous_credentials",
            Refusal::Method => "method_not_allowed",
            Refusal::InsufficientScope => "insufficient_scope",
            Refusal::RateLimited => "rate_limited",
            Refusal::WrongListener => "wrong_listener",
        }
    }
}

fn refuse(reason: Refusal, sub: Option<&str>, response: Response) -> Response {
    metrics::counter!(
        ds_runtime::metrics::metric_name("api_test_auth_rejected_total"),
        "reason" => reason.as_str()
    )
    .increment(1);
    tracing::warn!(
        audit = true,
        event = "test_auth_rejected",
        reason = reason.as_str(),
        sub,
        "test credentials request rejected"
    );
    response
}

fn invalid_token() -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "invalid_token",
        "invalid bearer token",
    )
}

/// What the gate leaves for the handler: the sandbox principal. Never a
/// `users` row; no handler a test token reaches reads a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestPrincipal {
    /// The token's `sub` (ds-sso's id for the test client).
    pub sub: String,
}

impl TestPrincipal {
    /// `test:<sub>`, for logs and responses; never a user id.
    pub fn principal(&self) -> String {
        format!("{PRINCIPAL_PREFIX}{}", self.sub)
    }
}

/// Whether a test token may reach `route`: reference data only.
pub fn route_allowed(route: &TokenRoute) -> bool {
    route
        .scope
        .is_some_and(|scope| ALLOWED_SCOPES.contains(&scope))
}

/// The [`TOKEN_ROUTES`] a test token may reach.
pub fn allowed_routes() -> impl Iterator<Item = &'static TokenRoute> {
    TOKEN_ROUTES.iter().filter(|route| route_allowed(route))
}

fn user_digest(sub: &str) -> u64 {
    let digest = Sha256::digest(sub.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

/// The personal-token gate's test-credentials branch, for a request whose
/// bearer [`TestAuth::names`] this trust root (see the module doc). Only
/// `token_gate` calls it. `charge_failure` charges the listener's
/// failed-authentication bucket.
// `allow`, not `expect`: whether this passes the line limit depends on the
// clippy version.
#[allow(
    clippy::too_many_lines,
    reason = "one linear sequence of checks, each with its own refusal"
)]
pub(crate) async fn serve(
    auth: &TestAuth,
    ambient_session_cookie: bool,
    mut request: Request,
    next: Next,
    route: &'static TokenRoute,
    matched: &str,
    charge_failure: impl FnOnce(),
) -> Response {
    if !auth.enabled() {
        charge_failure();
        return refuse(Refusal::Disabled, None, invalid_token());
    }
    // Never pick one credential of several: a session cookie (removed
    // before the gate, but noted), or more than one `Authorization` header
    // (a `dsp_` personal token beside the test token, say).
    if ambient_session_cookie
        || request
            .headers()
            .get_all(header::AUTHORIZATION)
            .iter()
            .count()
            > 1
    {
        charge_failure();
        return refuse(
            Refusal::AmbiguousCredentials,
            None,
            error_response(
                StatusCode::BAD_REQUEST,
                "ambiguous_credentials",
                "send one credential: a test token alone, without a cookie or another token",
            ),
        );
    }
    if request.method() != Method::GET {
        return refuse(
            Refusal::Method,
            None,
            error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "test credentials may only read",
            ),
        );
    }
    let Some(bearer) = crate::auth::bearer_token(request.headers()) else {
        charge_failure();
        return refuse(Refusal::Malformed, None, invalid_token());
    };
    let claims = match auth.verify(&bearer).await {
        Ok(claims) => claims,
        Err(reason) => {
            charge_failure();
            return refuse(reason, None, invalid_token());
        }
    };
    drop(bearer);
    let sub = claims.sub;
    if !route_allowed(route) {
        let mut response = error_response(
            StatusCode::FORBIDDEN,
            "insufficient_scope",
            "test credentials read reference data only (lines:read, trains:read)",
        );
        if let Ok(value) = HeaderValue::from_str(&format!(
            "Bearer error=\"insufficient_scope\", scope=\"{}\"",
            route.scope.map_or("", PersonalScope::as_str)
        )) {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, value);
        }
        return refuse(Refusal::InsufficientScope, Some(&sub), response);
    }

    let settings = &auth.settings;
    let digest = user_digest(&sub);
    let mut buckets = vec![
        (
            TestBucketKey::Sub(digest),
            settings.per_minute,
            settings.burst,
        ),
        (
            TestBucketKey::Global,
            settings.global_per_minute,
            settings.global_burst,
        ),
    ];
    if route.class == RouteClass::Train {
        buckets.push((
            TestBucketKey::SubTrains(digest),
            settings.trains_per_minute,
            settings.trains_burst,
        ));
    }
    if let Err(wait) = auth.limiter.check_all(&buckets, Instant::now()) {
        metrics::counter!(
            ds_runtime::metrics::metric_name("api_rate_limited_total"),
            "class" => "personal_token",
            "caller" => "test_auth"
        )
        .increment(1);
        return refuse(
            Refusal::RateLimited,
            Some(&sub),
            crate::personal_tokens::gate::rate_limited(wait),
        );
    }

    let principal = TestPrincipal { sub: sub.clone() };
    let principal_name = principal.principal();
    request.extensions_mut().insert(principal);
    let method = request.method().clone();
    let response = next.run(request).await;
    let status = response.status();
    metrics::counter!(
        ds_runtime::metrics::metric_name("api_test_auth_requests_total"),
        "route" => matched.to_string(),
        "status" => status.as_u16().to_string()
    )
    .increment(1);
    tracing::info!(
        audit = true,
        event = "test_auth",
        sub = %sub,
        principal = %principal_name,
        method = %method,
        route = %matched,
        status = status.as_u16(),
        "test credentials request"
    );
    response
}

/// On the main (public) and delegated listeners: a bearer naming the test
/// trust root is refused before anything else reads it, whether or not the
/// feature is on. Layer it outermost on the main router, beside
/// `personal_tokens::refuse_on_main_listener`.
pub async fn refuse_off_token_listener(
    State(auth): State<Arc<TestAuth>>,
    request: Request,
    next: Next,
) -> Response {
    if auth.named_in(request.headers()) {
        return refuse(
            Refusal::WrongListener,
            None,
            error_response(
                StatusCode::UNAUTHORIZED,
                "invalid_token",
                "test credentials work only on the API hostname",
            ),
        );
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::delegated::Scope;

    #[test]
    fn the_defaults_are_off_and_name_the_spec_values() {
        let settings = TestAuthSettings::default();
        assert!(!settings.enabled);
        assert_eq!(settings.issuer, TEST_AUTH_ISSUER);
        assert_eq!(
            settings.issuer,
            "https://ds-sso.cursed.solutions/application/o/distant-signal-test/"
        );
        assert_eq!(settings.audience, "distant-signal-test");
        assert_eq!(settings.required_group, "ds-test");
        settings.validate().unwrap();
        let auth = TestAuth::default();
        assert!(!auth.enabled());
        let built = TestAuth::from_settings(settings).unwrap();
        assert!(!built.enabled());
        assert!(
            built.verifier.is_none(),
            "no verifier (and no JWKS) while off"
        );
    }

    #[test]
    fn enabled_settings_need_the_exact_issuer() {
        let ok = TestAuthSettings {
            enabled: true,
            ..TestAuthSettings::default()
        };
        ok.validate().unwrap();
        for issuer in [
            "https://ds-sso.cursed.solutions/application/o/distant-signal-test",
            "https://ds-sso.cursed.solutions/application/o/distant-signal/",
            "http://ds-sso.cursed.solutions/application/o/distant-signal-test/",
            "https://sso.example/application/o/distant-signal-test/",
            "",
        ] {
            let settings = TestAuthSettings {
                issuer: issuer.to_string(),
                ..ok.clone()
            };
            assert!(settings.validate().is_err(), "{issuer:?}");
        }
        for (audience, group) in [("", "ds-test"), ("distant-signal-test", ""), (" a", "b")] {
            let settings = TestAuthSettings {
                audience: audience.to_string(),
                required_group: group.to_string(),
                ..ok.clone()
            };
            assert!(settings.validate().is_err(), "{audience:?} {group:?}");
        }
        let zero = TestAuthSettings {
            per_minute: 0,
            ..TestAuthSettings::default()
        };
        assert!(zero.validate().is_err());
    }

    fn delegated_client(issuer: &str, audience: &str) -> ClientConfig {
        ClientConfig {
            name: "ds-mcp".to_string(),
            display_name: "Assistant".to_string(),
            issuer_url: issuer.to_string(),
            audience: audience.to_string(),
            max_scopes: vec![Scope::JourneysRead],
            enabled: true,
            service_group: String::new(),
        }
    }

    #[test]
    fn the_startup_guard_keeps_the_test_audience_and_issuer_apart() {
        let on = TestAuthSettings {
            enabled: true,
            ..TestAuthSettings::default()
        };
        let sso = "https://ds-sso.cursed.solutions/application/o/distant-signal/";
        let internal = "https://ds-sso.cursed.solutions/application/o/distant-signal-internal/";
        let mcp = [delegated_client(
            "https://ds-sso.cursed.solutions/application/o/distant-signal-api/",
            "distant-signal-api",
        )];
        let guard = |settings: &TestAuthSettings,
                     sso_id: &str,
                     sso_issuer: &str,
                     internal_id: &str,
                     internal_issuer: &str,
                     clients: &[ClientConfig],
                     tokens: bool| {
            settings.ensure_distinct(
                sso_id,
                sso_issuer,
                internal_id,
                internal_issuer,
                clients,
                tokens,
            )
        };
        guard(
            &on,
            "distant-signal",
            sso,
            "distant-signal-internal",
            internal,
            &mcp,
            true,
        )
        .unwrap();
        // Off: nothing is checked.
        guard(
            &TestAuthSettings::default(),
            "distant-signal-test",
            TEST_AUTH_ISSUER,
            "x",
            "https://x/",
            &[],
            false,
        )
        .unwrap();
        // On without the token listener.
        assert!(guard(&on, "a", sso, "b", internal, &[], false).is_err());
        // The audience equal to the SSO, internal or a delegated audience.
        assert!(guard(&on, "distant-signal-test", sso, "b", internal, &[], true).is_err());
        assert!(guard(&on, "a", sso, "distant-signal-test", internal, &[], true).is_err());
        assert!(
            guard(
                &on,
                "a",
                sso,
                "b",
                internal,
                &[delegated_client("https://other/", "distant-signal-test")],
                true
            )
            .is_err()
        );
        // The issuer equal to the SSO, internal or a delegated issuer
        // (trailing slash aside).
        let bare = TEST_AUTH_ISSUER.trim_end_matches('/');
        assert!(guard(&on, "a", bare, "b", internal, &[], true).is_err());
        assert!(guard(&on, "a", sso, "b", TEST_AUTH_ISSUER, &[], true).is_err());
        assert!(
            guard(
                &on,
                "a",
                sso,
                "b",
                internal,
                &[delegated_client(TEST_AUTH_ISSUER, "distant-signal-api")],
                true
            )
            .is_err()
        );
    }

    #[test]
    fn test_tokens_reach_reference_routes_only() {
        let allowed: Vec<&str> = allowed_routes().map(|route| route.path).collect();
        assert_eq!(
            allowed,
            [
                "/v1/Line/Mode/{mode}/Status",
                "/v1/Line/{ids}/Status",
                "/v1/StopPoint/{crs}/Disruption",
                "/v1/Train/by-uid/{train_uid}/{date}",
                "/v1/island-of-ireland/trains/{train_code}/{date}",
                "/v1/stations/{crs}/departures",
            ],
            "a route added to TOKEN_ROUTES with lines:read or trains:read is open to test \
             credentials; check it reads no user and update this list and docs/test-auth.md"
        );
        for route in TOKEN_ROUTES {
            if matches!(
                route.scope,
                None | Some(PersonalScope::JourneysRead | PersonalScope::PreferencesRead)
            ) {
                assert!(!route_allowed(route), "{}", route.path);
            }
        }
    }

    #[test]
    fn the_principal_is_prefixed() {
        let principal = TestPrincipal {
            sub: "abc".to_string(),
        };
        assert_eq!(principal.principal(), "test:abc");
    }

    /// The verifier, the token listener's gate and the other listeners'
    /// refusal, against a mock ds-sso (`internal_oauth::test_support`) and
    /// stand-in handlers that say which credential reached them.
    // `allow`, not `expect`: whether a test passes the line limit depends
    // on the clippy version.
    #[allow(
        clippy::too_many_lines,
        reason = "test code: scenario tests read top to bottom"
    )]
    mod gate_tests {
        use std::sync::Arc;

        use axum::body::Body;
        use axum::http::{HeaderMap, Request};
        use axum::routing::get;
        use serde_json::json;
        use tower::ServiceExt;

        use super::*;
        use crate::auth::internal_oauth::test_support::{
            KID, mock_authentik, sign_token, sign_token_with_typ,
        };
        use crate::personal_tokens::{
            BucketKey, PersonalTokenAuth, PersonalTokenSettings, PersonalTokens,
        };

        const SUB: &str = "ds-test-client";

        async fn handler(
            principal: Option<axum::Extension<TestPrincipal>>,
            personal: Option<axum::Extension<PersonalTokenAuth>>,
        ) -> String {
            match (principal, personal) {
                (Some(axum::Extension(principal)), None) => {
                    format!("reached as {}", principal.principal())
                }
                (None, Some(_)) => "reached as a personal token".to_string(),
                (None, None) => "reached with no credential".to_string(),
                (Some(_), Some(_)) => "reached with both".to_string(),
            }
        }

        fn lazy_pool() -> sqlx::PgPool {
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://nobody:nothing@127.0.0.1:1/none")
                .unwrap()
        }

        fn on(issuer: &str) -> TestAuthSettings {
            TestAuthSettings {
                enabled: true,
                issuer: issuer.to_string(),
                ..TestAuthSettings::default()
            }
        }

        fn app_with(test: TestAuthSettings) -> crate::app::App {
            let mut state =
                Arc::try_unwrap(crate::test_support::inert_app(lazy_pool())).expect("a fresh app");
            state.personal_tokens = Arc::new(
                PersonalTokens::new(PersonalTokenSettings {
                    enabled: true,
                    ..PersonalTokenSettings::default()
                })
                .with_test_auth(TestAuth::from_settings(test).unwrap()),
            );
            Arc::new(state)
        }

        /// The token listener's gate and credential strip in front of
        /// stand-ins for its routes (the real router's layering).
        fn token_listener(app: &crate::app::App) -> axum::Router {
            let v1 = axum::Router::new()
                .route("/status", get(handler))
                .route("/me", get(handler))
                .route("/Journeys/mine", get(handler))
                .route("/preferences", get(handler))
                .route("/Line/{ids}/Status", get(handler).post(handler))
                .route("/Train/by-uid/{train_uid}/{date}", get(handler))
                .route("/stations/{crs}/departures", get(handler));
            axum::Router::new()
                .nest("/v1", v1)
                .layer(axum::middleware::from_fn_with_state(
                    app.clone(),
                    crate::personal_tokens::gate::token_gate,
                ))
                .layer(axum::middleware::from_fn(
                    crate::personal_tokens::gate::strip_ambient_credentials,
                ))
        }

        /// The main (public) listener's credential layers, as `main.rs`
        /// stacks them, and the delegated listener's copy of it.
        fn main_listeners(app: &crate::app::App) -> [(&'static str, axum::Router); 2] {
            let router = axum::Router::new()
                .route("/Line/{ids}/Status", get(handler))
                .route("/Journeys/mine", get(handler))
                .route("/v1/Line/{ids}/Status", get(handler))
                .layer(axum::middleware::from_fn_with_state(
                    app.delegated.clone(),
                    crate::auth::delegated::gate,
                ))
                .layer(axum::middleware::from_fn(
                    crate::personal_tokens::refuse_on_main_listener,
                ))
                .layer(axum::middleware::from_fn_with_state(
                    app.personal_tokens.test_auth.clone(),
                    refuse_off_token_listener,
                ));
            [
                ("main", router.clone()),
                (
                    "delegated",
                    crate::auth::delegated::delegated_listener_router(router),
                ),
            ]
        }

        fn claims(issuer: &str, mutate: impl FnOnce(&mut serde_json::Value)) -> serde_json::Value {
            let now = chrono::Utc::now().timestamp();
            let mut claims = json!({
                "sub": SUB,
                "iss": issuer,
                "aud": DEFAULT_AUDIENCE,
                "azp": DEFAULT_AUDIENCE,
                "iat": now,
                "nbf": now,
                "exp": now + 3600,
                "groups": ["ds-test"],
            });
            mutate(&mut claims);
            claims
        }

        /// A token with `header` and no valid signature.
        fn unsigned(header: &serde_json::Value, claims: &serde_json::Value) -> String {
            let encode = |value: &serde_json::Value| {
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap())
            };
            format!(
                "{}.{}.{}",
                encode(header),
                encode(claims),
                URL_SAFE_NO_PAD.encode(b"not a signature")
            )
        }

        async fn send(
            router: &axum::Router,
            method: &str,
            uri: &str,
            headers: &[(&str, String)],
        ) -> (StatusCode, HeaderMap, String) {
            let mut request = Request::builder().method(method).uri(uri);
            for (name, value) in headers {
                request = request.header(*name, value);
            }
            let response = router
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let headers = response.headers().clone();
            let body = axum::body::to_bytes(response.into_body(), 1 << 16)
                .await
                .unwrap();
            (status, headers, String::from_utf8(body.to_vec()).unwrap())
        }

        fn bearer(token: &str) -> (&'static str, String) {
            ("authorization", format!("Bearer {token}"))
        }

        #[tokio::test]
        async fn a_good_token_verifies_and_every_bad_one_is_refused() {
            let (server, _) = mock_authentik().await;
            let uri = server.uri();
            let auth = TestAuth::from_settings(on(&uri)).unwrap();
            let good = auth
                .verify(&sign_token(&claims(&uri, |_| {})))
                .await
                .unwrap();
            assert_eq!(good.sub, SUB);
            assert_eq!(good.groups, ["ds-test"]);
            // An array `aud` naming the test audience is fine.
            auth.verify(&sign_token(&claims(&uri, |c| {
                c["aud"] = json!(["other", DEFAULT_AUDIENCE]);
            })))
            .await
            .unwrap();

            let now = chrono::Utc::now().timestamp();
            let rs256 = json!({"alg": "RS256", "kid": KID, "typ": "JWT"});
            let cases: Vec<(&str, String, Refusal)> = vec![
                (
                    "a bad signature",
                    {
                        let token = sign_token(&claims(&uri, |_| {}));
                        let (rest, _) = token.rsplit_once('.').unwrap();
                        format!("{rest}.AAAA")
                    },
                    Refusal::Signature,
                ),
                (
                    "an unsigned RS256 header",
                    unsigned(&rs256, &claims(&uri, |_| {})),
                    Refusal::Signature,
                ),
                (
                    "alg none",
                    unsigned(&json!({"alg": "none", "kid": KID}), &claims(&uri, |_| {})),
                    Refusal::Algorithm,
                ),
                (
                    "alg HS256",
                    unsigned(&json!({"alg": "HS256", "kid": KID}), &claims(&uri, |_| {})),
                    Refusal::Algorithm,
                ),
                (
                    "alg RS384",
                    unsigned(&json!({"alg": "RS384", "kid": KID}), &claims(&uri, |_| {})),
                    Refusal::Algorithm,
                ),
                (
                    "alg PS256",
                    unsigned(&json!({"alg": "PS256", "kid": KID}), &claims(&uri, |_| {})),
                    Refusal::Algorithm,
                ),
                (
                    "another issuer",
                    sign_token(&claims(&format!("{uri}/"), |_| {})),
                    Refusal::Issuer,
                ),
                (
                    "the SSO client's audience",
                    sign_token(&claims(&uri, |c| c["aud"] = json!("distant-signal"))),
                    Refusal::Audience,
                ),
                (
                    "expired",
                    sign_token(&claims(&uri, |c| c["exp"] = json!(now - 3600))),
                    Refusal::Expired,
                ),
                (
                    "nbf in the future",
                    sign_token(&claims(&uri, |c| c["nbf"] = json!(now + 3600))),
                    Refusal::NotYetValid,
                ),
                (
                    "no iat",
                    sign_token(&claims(&uri, |c| {
                        c.as_object_mut().unwrap().remove("iat");
                    })),
                    Refusal::Malformed,
                ),
                (
                    "iat in the future",
                    sign_token(&claims(&uri, |c| c["iat"] = json!(now + 3600))),
                    Refusal::NotYetValid,
                ),
                (
                    "no groups",
                    sign_token(&claims(&uri, |c| {
                        c.as_object_mut().unwrap().remove("groups");
                    })),
                    Refusal::MissingGroup,
                ),
                (
                    "another group",
                    sign_token(&claims(&uri, |c| c["groups"] = json!(["srv-ds-mcp"]))),
                    Refusal::MissingGroup,
                ),
                (
                    "a week-long token",
                    sign_token(&claims(&uri, |c| c["exp"] = json!(now + 7 * 24 * 3600))),
                    Refusal::Invalid,
                ),
                (
                    "an empty sub",
                    sign_token(&claims(&uri, |c| c["sub"] = json!(""))),
                    Refusal::Invalid,
                ),
                (
                    "a logout token",
                    sign_token_with_typ(&claims(&uri, |_| {}), "logout+jwt"),
                    Refusal::Invalid,
                ),
                ("not a JWT", "abc".to_string(), Refusal::Malformed),
            ];
            for (what, token, expected) in cases {
                assert_eq!(auth.verify(&token).await.err(), Some(expected), "{what}");
            }

            // Within the leeway either side: accepted, like the delegated
            // verifier.
            auth.verify(&sign_token(&claims(&uri, |c| {
                c["exp"] = json!(now - 30);
                c["nbf"] = json!(now + 30);
                c["iat"] = json!(now + 30);
            })))
            .await
            .unwrap();

            // Off: refused, and the issuer is never asked for keys.
            let (quiet, _) = mock_authentik().await;
            let off = TestAuth::from_settings(TestAuthSettings {
                enabled: false,
                ..on(&quiet.uri())
            })
            .unwrap();
            assert_eq!(
                off.verify(&sign_token(&claims(&quiet.uri(), |_| {})))
                    .await
                    .err(),
                Some(Refusal::Disabled)
            );
            assert!(quiet.received_requests().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn a_test_token_reads_reference_data_on_the_token_listener_only() {
            let (server, _) = mock_authentik().await;
            let app = app_with(on(&server.uri()));
            let tokens = token_listener(&app);
            let token = sign_token(&claims(&server.uri(), |_| {}));

            for uri in [
                "/v1/Line/victoria/Status",
                "/v1/Train/by-uid/C12345/2026-11-17",
                "/v1/stations/KGX/departures",
            ] {
                let (status, _, body) = send(&tokens, "GET", uri, &[bearer(&token)]).await;
                assert_eq!(status, StatusCode::OK, "{uri}: {body}");
                assert_eq!(body, format!("reached as test:{SUB}"), "{uri}");
            }
            // The open route needs nothing.
            let (status, _, _) = send(&tokens, "GET", "/v1/status", &[bearer(&token)]).await;
            assert_eq!(status, StatusCode::OK);

            // Everything that needs a user or another scope: 403.
            for uri in ["/v1/me", "/v1/Journeys/mine", "/v1/preferences"] {
                let (status, headers, body) = send(&tokens, "GET", uri, &[bearer(&token)]).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
                assert!(body.contains("insufficient_scope"), "{uri}: {body}");
                assert!(
                    headers[header::WWW_AUTHENTICATE]
                        .to_str()
                        .unwrap()
                        .contains("insufficient_scope")
                );
            }
            // GET only.
            let (status, _, body) = send(
                &tokens,
                "POST",
                "/v1/Line/victoria/Status",
                &[bearer(&token)],
            )
            .await;
            assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
            assert!(!body.contains("reached"), "{body}");

            // A bad token on the token listener: 401, never a handler.
            let expired = sign_token(&claims(&server.uri(), |c| {
                c["exp"] = json!(chrono::Utc::now().timestamp() - 3600);
            }));
            let (status, headers, body) = send(
                &tokens,
                "GET",
                "/v1/Line/victoria/Status",
                &[bearer(&expired)],
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert!(headers.contains_key(header::WWW_AUTHENTICATE));
        }

        #[tokio::test]
        async fn the_main_and_delegated_listeners_refuse_test_tokens() {
            let (server, _) = mock_authentik().await;
            let uri = server.uri();
            let tokens = [
                ("a valid token", sign_token(&claims(&uri, |_| {}))),
                (
                    "the issuer with a trailing slash",
                    sign_token(&claims(&format!("{uri}/"), |_| {})),
                ),
                (
                    "the test audience from another issuer",
                    sign_token(&claims("https://other.example/", |_| {})),
                ),
                (
                    "the test issuer with another audience",
                    sign_token(&claims(&uri, |c| c["aud"] = json!("distant-signal-api"))),
                ),
            ];
            for enabled in [true, false] {
                let app = app_with(TestAuthSettings {
                    enabled,
                    ..on(&uri)
                });
                for (listener, router) in main_listeners(&app) {
                    for (what, token) in &tokens {
                        for path in [
                            "/Line/victoria/Status",
                            "/Journeys/mine",
                            "/v1/Line/victoria/Status",
                        ] {
                            let (status, _, body) =
                                send(&router, "GET", path, &[bearer(token)]).await;
                            assert_eq!(
                                status,
                                StatusCode::UNAUTHORIZED,
                                "{listener} {path} {what} enabled={enabled}: {body}"
                            );
                            assert!(body.contains("invalid_token"), "{body}");
                        }
                    }
                    // Anything else passes the layers as before.
                    let (status, _, body) = send(&router, "GET", "/Journeys/mine", &[]).await;
                    assert_eq!(status, StatusCode::OK, "{listener}");
                    assert_eq!(body, "reached with no credential");
                }
            }
        }

        #[tokio::test]
        async fn a_test_token_with_a_cookie_or_a_personal_token_is_refused() {
            let (server, _) = mock_authentik().await;
            let app = app_with(on(&server.uri()));
            let tokens = token_listener(&app);
            let token = sign_token(&claims(&server.uri(), |_| {}));
            let personal = crate::personal_tokens::token::generate().token;
            let cookie = (
                "cookie",
                format!("{}=abc", crate::auth::SESSION_COOKIE_NAME),
            );
            for headers in [
                vec![bearer(&token), cookie.clone()],
                vec![bearer(&token), bearer(&personal)],
                vec![bearer(&personal), bearer(&token)],
                vec![bearer(&token), bearer(&token)],
            ] {
                let (status, _, body) =
                    send(&tokens, "GET", "/v1/Line/victoria/Status", &headers).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{headers:?}: {body}");
                assert!(body.contains("ambiguous_credentials"), "{body}");
            }
            // Another cookie is not a session: removed, and not a conflict.
            let (status, _, body) = send(
                &tokens,
                "GET",
                "/v1/Line/victoria/Status",
                &[bearer(&token), ("cookie", "theme=dark".to_string())],
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }

        #[tokio::test]
        async fn while_off_test_tokens_are_refused_without_fetching_keys() {
            let (server, _) = mock_authentik().await;
            let app = app_with(TestAuthSettings {
                enabled: false,
                ..on(&server.uri())
            });
            let tokens = token_listener(&app);
            let token = sign_token(&claims(&server.uri(), |_| {}));
            let (status, _, body) = send(
                &tokens,
                "GET",
                "/v1/Line/victoria/Status",
                &[bearer(&token)],
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
            assert!(!body.contains("reached"), "{body}");
            assert!(server.received_requests().await.unwrap().is_empty());
        }

        #[tokio::test]
        async fn test_tokens_have_their_own_rate_buckets() {
            let (server, _) = mock_authentik().await;
            let app = app_with(TestAuthSettings {
                per_minute: 1,
                burst: 1,
                ..on(&server.uri())
            });
            let tokens = token_listener(&app);
            let first = sign_token(&claims(&server.uri(), |_| {}));
            let other = sign_token(&claims(&server.uri(), |c| c["sub"] = json!("another")));
            let uri = "/v1/Line/victoria/Status";
            let (status, _, _) = send(&tokens, "GET", uri, &[bearer(&first)]).await;
            assert_eq!(status, StatusCode::OK);
            let (status, headers, body) = send(&tokens, "GET", uri, &[bearer(&first)]).await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
            assert!(headers.contains_key(header::RETRY_AFTER));
            // Keyed by sub: another sub has its own bucket.
            let (status, _, body) = send(&tokens, "GET", uri, &[bearer(&other)]).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            // The personal tokens' buckets were never charged.
            assert!(
                app.personal_tokens
                    .limiter
                    .would_refuse(BucketKey::Global, 1, 1, Instant::now())
                    .is_none()
            );
            assert!(
                app.personal_tokens
                    .test_auth
                    .limiter
                    .would_refuse(TestBucketKey::Global, 1, 1, Instant::now())
                    .is_some(),
                "the test-credentials global bucket was charged"
            );
        }

        /// The extractor of the user-data routes refuses the sandbox
        /// principal even if a route were ever opened to it.
        #[tokio::test]
        async fn the_user_extractor_refuses_the_sandbox_principal() {
            use crate::auth::delegated::{DelegatedOrSessionUser, ReadScope};
            let app = app_with(TestAuthSettings::default());
            let router = crate::app::Router::new()
                .route(
                    "/x",
                    get(|_user: DelegatedOrSessionUser<ReadScope>| async { "reached" }),
                )
                .layer(axum::Extension(TestPrincipal {
                    sub: SUB.to_string(),
                }))
                .with_state(app);
            let (status, _, body) = send(&router, "GET", "/x", &[bearer("x.y.z")]).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        }

        /// A `MakeWriter` that keeps everything written to it.
        #[derive(Clone, Default)]
        struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
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

        #[tokio::test]
        async fn the_audit_line_names_the_sub_and_never_the_token() {
            let captured = Captured::default();
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(captured.clone())
                .with_ansi(false)
                .finish();
            let _guard = tracing::subscriber::set_default(subscriber);

            let (server, _) = mock_authentik().await;
            let app = app_with(on(&server.uri()));
            let tokens = token_listener(&app);
            let token = sign_token(&claims(&server.uri(), |_| {}));
            let bad = sign_token(&claims(&server.uri(), |c| c["groups"] = json!([])));
            send(
                &tokens,
                "GET",
                "/v1/Line/victoria/Status",
                &[bearer(&token)],
            )
            .await;
            send(&tokens, "GET", "/v1/Line/victoria/Status", &[bearer(&bad)]).await;
            for (_, router) in main_listeners(&app) {
                send(&router, "GET", "/Journeys/mine", &[bearer(&token)]).await;
            }

            let logs = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
            assert!(logs.contains("test_auth"), "{logs}");
            assert!(logs.contains(SUB), "{logs}");
            assert!(logs.contains(&format!("test:{SUB}")), "{logs}");
            assert!(logs.contains("missing_group"), "{logs}");
            assert!(logs.contains("wrong_listener"), "{logs}");
            for secret in [&token, &bad] {
                let signature = secret.rsplit('.').next().unwrap();
                assert!(!logs.contains(signature), "a token was logged");
            }
        }

        /// Verifies a REAL ds-sso client-credentials token against the real
        /// issuer, and that Authentik puts the `groups` claim (with
        /// `ds-test`) in it, which is not yet confirmed. Returns early
        /// unless `DS_TEST_AUTH_REAL_TOKEN` is set (CI's DB jobs run every
        /// ignored test). How to run it: docs/test-auth.md, "Checking a
        /// real token".
        #[tokio::test]
        #[ignore = "needs a real ds-sso token in DS_TEST_AUTH_REAL_TOKEN and network access; \
                    see docs/test-auth.md"]
        async fn real_ds_sso_client_credentials_token_carries_the_ds_test_group() {
            let Ok(token) = std::env::var("DS_TEST_AUTH_REAL_TOKEN") else {
                return;
            };
            let token = token.trim();
            if token.is_empty() {
                return;
            }
            let claims = peek_claims(token);
            let groups = claims.get("groups").cloned().unwrap_or_default();
            assert!(
                groups
                    .as_array()
                    .is_some_and(|groups| groups.iter().any(|g| g == DEFAULT_REQUIRED_GROUP)),
                "the token's groups claim does not contain {DEFAULT_REQUIRED_GROUP}: {groups}"
            );
            let auth = TestAuth::from_settings(TestAuthSettings {
                enabled: true,
                ..TestAuthSettings::default()
            })
            .unwrap();
            let verified = auth
                .verify(token)
                .await
                .unwrap_or_else(|reason| panic!("the real token was refused: {}", reason.as_str()));
            assert!(verified.groups.iter().any(|g| g == DEFAULT_REQUIRED_GROUP));
        }

        /// The unverified payload, for the real-token test's diagnosis.
        fn peek_claims(token: &str) -> serde_json::Value {
            let payload = token.split('.').nth(1).expect("a JWT has a payload");
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).expect("base64url"))
                .expect("a JSON payload")
        }
    }
}
