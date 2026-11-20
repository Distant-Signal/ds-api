//! Per-client-IP rate limits on the api's expensive or write-amplifying
//! public routes (API-5, TRIPS-1; the user's revised DQ3 decision).
//!
//! Production has no ingress-nginx: public traffic arrives Cloudflare tunnel
//! -> cloudflared -> frontend -> api. So the socket peer of almost every
//! request is the frontend pod, and the real client is only known from the
//! `X-Real-IP` header the frontend proxy sets (from `CF-Connecting-IP`,
//! overwriting whatever the client sent). The key is therefore:
//!
//! - `X-Real-IP`, when [`RateLimitSettings::trust_x_real_ip`] is on (the
//!   default: the api Service is ClusterIP-only, so only in-cluster pods can
//!   set it);
//! - otherwise the socket peer address.
//!
//! `API_RATE_LIMIT_TRUSTED_PROXIES` (CIDRs, default empty = any peer, the
//! behaviour above) narrows whose `X-Real-IP` is believed: from any other
//! peer the header is ignored and the key is the socket peer.
//!
//! `X-Forwarded-For` is never read: a client can put anything in it. A
//! request with no usable `X-Real-IP` falls back to the peer, which is
//! usually the frontend pod, so all such requests share one bucket; that is
//! counted in `distant_signal_api_rate_limit_peer_fallback_total` and logged
//! once, so it is visible. IPv6 clients are keyed by their /64.
//!
//! Limited ([`classify`]):
//!
//! - login: `/public/auth/login` and `/public/auth/callback` (each login
//!   writes an `oidc_login_state` row and sweeps expired ones);
//! - `/Trips/plan` (the most expensive anonymous read);
//! - `/Train/by-uid/*` (anonymous, and a cache miss runs schedule matching),
//!   and `/public/tfl/stops/{id}/arrivals` in the same budget (a cache miss
//!   calls `TfL`), and the Atom feeds (`/public/feeds/*`, DB reads a
//!   feed reader polls; usually answered from the edge cache), and the
//!   Irish train page's `/public/island-of-ireland/trains/*` (an
//!   anonymous page of DB reads, uncached);
//! - `POST /public/events` (opt-in usage analytics, DS-190), in a budget of
//!   its own so a chatty page can't spend a visitor's public-write budget.
//!   Anonymous, it is per client IP; under a verified MCP bearer it is
//!   charged to the `svc:mcp` bucket of the events class (one bucket for
//!   the whole service, never per IP: the bearer is checked before the key
//!   is chosen);
//! - every other public write (non-GET/HEAD/OPTIONS), with a generous
//!   default, as a backstop for anonymous write amplification.
//!
//! Cheap cacheable reads stay unlimited, including the sitemap's id lists
//! (`/public/stations/codes`, `/public/stations/index`,
//! `/public/tfl/stops/ids`).
//!
//! A limited request gets `429 Too Many Requests` with `Retry-After`, and is counted in
//! `distant_signal_api_rate_limited_total{class,caller}`.
//!
//! ## Link-preview reads
//!
//! The frontend renders link previews (page metadata and `card.png`) on its
//! own behalf, not the visitor's, through Next's data cache, which keys on
//! the request headers: a per-visitor `X-Real-IP` would give every visitor
//! their own cache entry, so those reads carry none and, before this class,
//! all keyed on the frontend pod and shared one `train_by_uid` bucket with
//! every other peer-keyed read: a crawler walking `/train/<uid>/<date>`
//! could spend it and make every preview (and every other such read) fail.
//! The frontend now marks the `GET /Train/by-uid/*` preview read with
//! `X-DS-Preview: 1` ([`PREVIEW_HEADER`]). Such a request is charged to its
//! own bucket (`svc:preview`, `API_RATE_LIMIT_TRAIN_PREVIEW_*`, default
//! 600/min burst 200: cached reads, one api call per train a minute), so
//! it can neither be starved by other reads nor starve them. The marker is
//! honoured only on `/Train/by-uid/*`, only for an anonymous request (a
//! bearer decides first), and only under the same trust rule as
//! `X-Real-IP` (`API_RATE_LIMIT_TRUST_X_REAL_IP` and a peer inside
//! `API_RATE_LIMIT_TRUSTED_PROXIES`): the frontend's `/api` proxy builds
//! its upstream headers from scratch and never forwards it, so a browser
//! cannot set it. A client that reaches the api directly (in-cluster only)
//! can set it, and gets the preview bucket instead of its own per-IP one:
//! still a finite budget, the same exposure `X-Real-IP` has.
//!
//! ## Trusted service callers (the MCP)
//!
//! The Distant-Signal-MCP calls these public routes in-cluster, directly
//! (not through the frontend), so it has no `X-Real-IP` and would otherwise
//! share one small per-peer bucket for all of its users. It is recognised by
//! the internal OAuth machinery the `/private` ingest routes used until
//! release B (ingest phase 5): an Authentik
//! client-credentials JWT, verified against the cached JWKS by the api's one
//! shared [`ServiceTokenVerifier`](crate::auth::internal_oauth::ServiceTokenVerifier),
//! whose `groups` claim contains `INTERNAL_OAUTH_GROUP_MCP`. Such a request
//! is NOT exempt: it is charged to a separate, finite budget
//! (`API_RATE_LIMIT_MCP_*`, 5x the public defaults), keyed on the caller
//! identity (`svc:mcp`, one bucket for the whole service, never per IP), so
//! a runaway loop or a leaked credential is still bounded.
//!
//! The rules ([`ServiceCallerAuth`], [`bearer_decision`]):
//!
//! - no `Authorization: Bearer` header: anonymous, per IP, exactly as above
//!   (the frontend proxy never forwards an `Authorization` header, so the
//!   browser path never pays for token verification);
//! - a bearer that verifies AND carries the MCP group: the MCP budget;
//! - a bearer that fails verification (malformed, expired, bad signature,
//!   wrong issuer or audience): `401`, never a silent fall-back to the
//!   anonymous bucket. It is rejected before any handler runs, so there is
//!   nothing to bypass, and a misconfigured MCP fails loudly instead of
//!   being quietly squeezed into the in-cluster peer's tiny bucket;
//! - a bearer that verifies but lacks the MCP group (another service's
//!   credential): `403`, same reasoning; logged with its `sub`;
//! - login routes ignore the bearer entirely (the MCP has no business
//!   logging in; they keep the anonymous per-IP limit);
//! - `INTERNAL_OAUTH_GROUP_MCP` empty: the feature is inert and any bearer
//!   is ignored (anonymous, per IP, as before this existed).
//!
//! The bearer is read only here. End-user auth on public routes is the
//! `distant_signal_session` cookie (`crate::auth::AuthenticatedUser`), which
//! never looks at `Authorization`, so the two cannot collide: an MCP request
//! is still anonymous as far as every handler is concerned.
//!
//! ## Delegated callers (a registered client acting for a user)
//!
//! A delegated token (docs/connected-apps.md) is routed by its issuer and
//! audience in `crate::auth::delegated::gate`, which runs before this
//! middleware, verifies it with that client's verifier and leaves a
//! [`DelegatedToken`](crate::auth::delegated::DelegatedToken) in the
//! request's extensions. Such a request never reaches the internal-token
//! path below (so it is no longer a `401` there) and never uses the
//! per-IP or `svc:mcp` buckets. It is charged to two buckets of its class
//! (`delegated_read`, `delegated_write`, `delegated_delete`), and both
//! must pass (spec §5.10, D-11):
//!
//! - the user's (`user:<client>/<sub hash>`): by default 60/min burst 20
//!   reads, 10/min burst 5 writes, 5/min burst 3 deletes;
//! - the client's (`svc:<client>-delegated`): the same times
//!   `API_RATE_LIMIT_DELEGATED_CLIENT_MULTIPLIER` (20), a ceiling for a
//!   runaway loop across many users or a compromised client.
//!
//! The daily caps (50 saves, 20 deletes) are database counts in the
//! handlers, not here: GCRA in memory per pod cannot express a day.
//!
//! The algorithm is GCRA (a token bucket kept as one timestamp per key):
//! `per_minute` sustained, `burst` at once. State is per pod, in memory.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use std::future::Future;
use std::pin::Pin;

use anyhow::{Context, Result, ensure};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::internal_oauth::{ServiceClaims, VerifyError};

/// The header the frontend proxy sets to the real client address.
pub const REAL_IP_HEADER: &str = "x-real-ip";

/// The marker the frontend puts on a link-preview read of
/// `GET /Train/by-uid/*` (value `1`); see the module doc.
pub const PREVIEW_HEADER: &str = "x-ds-preview";

/// Which limit a request falls under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitClass {
    Login,
    TripPlan,
    TrainByUid,
    PublicWrite,
    /// `POST /public/events`: usage analytics.
    Events,
    /// A verified delegated request on a read route.
    DelegatedRead,
    /// ... on the itinerary save route.
    DelegatedWrite,
    /// ... on the delete route.
    DelegatedDelete,
}

impl LimitClass {
    pub fn label(self) -> &'static str {
        match self {
            LimitClass::Login => "login",
            LimitClass::TripPlan => "trip_plan",
            LimitClass::TrainByUid => "train_by_uid",
            LimitClass::PublicWrite => "public_write",
            LimitClass::Events => "events",
            LimitClass::DelegatedRead => "delegated_read",
            LimitClass::DelegatedWrite => "delegated_write",
            LimitClass::DelegatedDelete => "delegated_delete",
        }
    }
}

/// Who a limited request is charged to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Caller {
    /// Anonymous (or a signed-in end user): keyed per client IP.
    Public,
    /// The Distant-Signal-MCP, proven by an internal OAuth token: one
    /// bucket for the whole service.
    Mcp,
    /// A delegated client acting for one user: that user's bucket.
    DelegatedUser,
    /// A delegated client: its ceiling across every user.
    DelegatedClient,
    /// The frontend's link-preview reads (`X-DS-Preview`): one bucket for
    /// all of them.
    Preview,
}

impl Caller {
    pub fn label(self) -> &'static str {
        match self {
            Caller::Public => "public",
            Caller::Mcp => "mcp",
            Caller::DelegatedUser => "delegated_user",
            Caller::DelegatedClient => "delegated_client",
            Caller::Preview => "preview",
        }
    }
}

/// A limiter bucket key: a client IP, or a service identity (`svc:mcp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientKey {
    Ip(IpAddr),
    Service(Caller),
    /// A delegated client's user: the client's index in `DELEGATED_CLIENTS`
    /// and a 64-bit digest of the `sub` (the key never holds the `sub`).
    DelegatedUser {
        client: u16,
        user: u64,
    },
    /// A delegated client, across all its users.
    DelegatedClient(u16),
}

impl ClientKey {
    fn caller(self) -> Caller {
        match self {
            ClientKey::Ip(_) => Caller::Public,
            ClientKey::Service(caller) => caller,
            ClientKey::DelegatedUser { .. } => Caller::DelegatedUser,
            ClientKey::DelegatedClient(_) => Caller::DelegatedClient,
        }
    }
}

impl std::fmt::Display for ClientKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientKey::Ip(ip) => write!(f, "{ip}"),
            ClientKey::Service(caller) => write!(f, "svc:{}", caller.label()),
            ClientKey::DelegatedUser { client, user } => write!(f, "user:{client}/{user:016x}"),
            ClientKey::DelegatedClient(client) => write!(f, "svc:{client}-delegated"),
        }
    }
}

/// The limit class of a request, or `None` for an unlimited one.
/// `path` is the full request path as the top-level router sees it.
pub fn classify(method: &Method, path: &str) -> Option<LimitClass> {
    if path == "/public/auth/login" || path == "/public/auth/callback" {
        return Some(LimitClass::Login);
    }
    if path == "/Trips/plan" || is_journey_replan(path) {
        return Some(LimitClass::TripPlan);
    }
    if path.starts_with("/Train/by-uid/")
        || is_tfl_arrivals(path)
        || path.starts_with("/public/feeds/")
        || path.starts_with("/public/calendar/")
        || path.starts_with("/public/island-of-ireland/trains/")
    {
        return Some(LimitClass::TrainByUid);
    }
    if path == "/public/events" {
        return Some(LimitClass::Events);
    }
    if !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return Some(LimitClass::PublicWrite);
    }
    None
}

/// `GET /public/tfl/stops/{id}/arrivals` (DS-136): a cache miss calls
/// `TfL`, so it shares the `/Train/by-uid/*` budget, which is sized for an
/// anonymous page that refreshes itself.
fn is_tfl_arrivals(path: &str) -> bool {
    path.strip_prefix("/public/tfl/stops/")
        .and_then(|rest| rest.strip_suffix("/arrivals"))
        .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

/// A journey route that runs the planner (spec 2026-10-22 §7): the
/// alternatives (which "I missed this train" shows), the re-plan preview
/// and "Track this journey again" (`/repeat`) cost what a `/Trips/plan`
/// does, so they share its budget.
fn is_journey_replan(path: &str) -> bool {
    path.starts_with("/Journeys/")
        && (path.ends_with("/alternatives")
            || path.ends_with("/replan/preview")
            || path.ends_with("/replan/accept")
            || path.ends_with("/repeat"))
}

/// Rate-limit settings, from the environment.
#[derive(Debug, Clone, PartialEq, Eq, clap::Parser)]
#[command(name = "api-rate-limit", no_binary_name = true)]
pub struct RateLimitSettings {
    /// Master switch.
    #[arg(long, env = "API_RATE_LIMIT_ENABLED", default_value_t = true, action = clap::ArgAction::Set)]
    pub enabled: bool,
    /// Key on `X-Real-IP` when present. Only safe while every client reaches
    /// the api through a proxy that overwrites it (the frontend); the api
    /// Service is ClusterIP-only.
    #[arg(long, env = "API_RATE_LIMIT_TRUST_X_REAL_IP", default_value_t = true, action = clap::ArgAction::Set)]
    pub trust_x_real_ip: bool,
    /// Comma-separated CIDR ranges (`10.0.0.0/8,fd00::/8`) of the peers whose
    /// `X-Real-IP` is believed. Empty (the default) believes any direct
    /// caller, as before; set it to the frontend pods' range to ignore the
    /// header from every other peer (the key is then the socket peer).
    #[arg(long, env = "API_RATE_LIMIT_TRUSTED_PROXIES", default_value = "")]
    pub trusted_proxies: String,

    #[arg(long, env = "API_RATE_LIMIT_LOGIN_PER_MINUTE", default_value_t = 10)]
    pub login_per_minute: u32,
    #[arg(long, env = "API_RATE_LIMIT_LOGIN_BURST", default_value_t = 20)]
    pub login_burst: u32,

    #[arg(
        long,
        env = "API_RATE_LIMIT_TRIP_PLAN_PER_MINUTE",
        default_value_t = 20
    )]
    pub trip_plan_per_minute: u32,
    #[arg(long, env = "API_RATE_LIMIT_TRIP_PLAN_BURST", default_value_t = 10)]
    pub trip_plan_burst: u32,

    #[arg(
        long,
        env = "API_RATE_LIMIT_TRAIN_BY_UID_PER_MINUTE",
        default_value_t = 120
    )]
    pub train_by_uid_per_minute: u32,
    #[arg(long, env = "API_RATE_LIMIT_TRAIN_BY_UID_BURST", default_value_t = 60)]
    pub train_by_uid_burst: u32,

    #[arg(
        long,
        env = "API_RATE_LIMIT_PUBLIC_WRITE_PER_MINUTE",
        default_value_t = 120
    )]
    pub public_write_per_minute: u32,
    #[arg(long, env = "API_RATE_LIMIT_PUBLIC_WRITE_BURST", default_value_t = 60)]
    pub public_write_burst: u32,

    /// `POST /public/events`, per client IP; under an MCP bearer it is the
    /// `svc:mcp` bucket of this class, not per IP (the MCP has no business
    /// sending events). A page sends a few per view.
    #[arg(long, env = "API_RATE_LIMIT_EVENTS_PER_MINUTE", default_value_t = 60)]
    pub events_per_minute: u32,
    #[arg(long, env = "API_RATE_LIMIT_EVENTS_BURST", default_value_t = 30)]
    pub events_burst: u32,

    /// Link-preview reads of `/Train/by-uid/*` (`X-DS-Preview`): one bucket
    /// for the frontend's cached preview fetches, see the module doc.
    #[arg(
        long,
        env = "API_RATE_LIMIT_TRAIN_PREVIEW_PER_MINUTE",
        default_value_t = 600
    )]
    pub train_preview_per_minute: u32,
    #[arg(
        long,
        env = "API_RATE_LIMIT_TRAIN_PREVIEW_BURST",
        default_value_t = 200
    )]
    pub train_preview_burst: u32,

    /// The MCP's own budget (one bucket for the whole service, see the
    /// module doc). Login has none: the bearer is ignored there.
    #[arg(
        long,
        env = "API_RATE_LIMIT_MCP_TRIP_PLAN_PER_MINUTE",
        default_value_t = 100
    )]
    pub mcp_trip_plan_per_minute: u32,
    #[arg(long, env = "API_RATE_LIMIT_MCP_TRIP_PLAN_BURST", default_value_t = 30)]
    pub mcp_trip_plan_burst: u32,
    #[arg(
        long,
        env = "API_RATE_LIMIT_MCP_TRAIN_BY_UID_PER_MINUTE",
        default_value_t = 600
    )]
    pub mcp_train_by_uid_per_minute: u32,
    #[arg(
        long,
        env = "API_RATE_LIMIT_MCP_TRAIN_BY_UID_BURST",
        default_value_t = 200
    )]
    pub mcp_train_by_uid_burst: u32,
    #[arg(
        long,
        env = "API_RATE_LIMIT_MCP_PUBLIC_WRITE_PER_MINUTE",
        default_value_t = 600
    )]
    pub mcp_public_write_per_minute: u32,
    #[arg(
        long,
        env = "API_RATE_LIMIT_MCP_PUBLIC_WRITE_BURST",
        default_value_t = 200
    )]
    pub mcp_public_write_burst: u32,

    /// Delegated budgets (spec §5.10, D-11), per user; the client's ceiling
    /// is these times `delegated_client_multiplier`.
    #[arg(
        long,
        env = "API_RATE_LIMIT_DELEGATED_READ_PER_MINUTE",
        default_value_t = 60
    )]
    pub delegated_read_per_minute: u32,
    #[arg(
        long,
        env = "API_RATE_LIMIT_DELEGATED_READ_BURST",
        default_value_t = 20
    )]
    pub delegated_read_burst: u32,
    #[arg(
        long,
        env = "API_RATE_LIMIT_DELEGATED_WRITE_PER_MINUTE",
        default_value_t = 10
    )]
    pub delegated_write_per_minute: u32,
    #[arg(
        long,
        env = "API_RATE_LIMIT_DELEGATED_WRITE_BURST",
        default_value_t = 5
    )]
    pub delegated_write_burst: u32,
    #[arg(
        long,
        env = "API_RATE_LIMIT_DELEGATED_DELETE_PER_MINUTE",
        default_value_t = 5
    )]
    pub delegated_delete_per_minute: u32,
    #[arg(
        long,
        env = "API_RATE_LIMIT_DELEGATED_DELETE_BURST",
        default_value_t = 3
    )]
    pub delegated_delete_burst: u32,
    #[arg(
        long,
        env = "API_RATE_LIMIT_DELEGATED_CLIENT_MULTIPLIER",
        default_value_t = 20
    )]
    pub delegated_client_multiplier: u32,
}

impl Default for RateLimitSettings {
    /// The declared defaults (kept equal by
    /// `tests::defaults_match_the_declared_values`).
    fn default() -> Self {
        Self {
            enabled: true,
            trust_x_real_ip: true,
            trusted_proxies: String::new(),
            login_per_minute: 10,
            login_burst: 20,
            trip_plan_per_minute: 20,
            trip_plan_burst: 10,
            train_by_uid_per_minute: 120,
            train_by_uid_burst: 60,
            public_write_per_minute: 120,
            public_write_burst: 60,
            events_per_minute: 60,
            events_burst: 30,
            train_preview_per_minute: 600,
            train_preview_burst: 200,
            mcp_trip_plan_per_minute: 100,
            mcp_trip_plan_burst: 30,
            mcp_train_by_uid_per_minute: 600,
            mcp_train_by_uid_burst: 200,
            mcp_public_write_per_minute: 600,
            mcp_public_write_burst: 200,
            delegated_read_per_minute: 60,
            delegated_read_burst: 20,
            delegated_write_per_minute: 10,
            delegated_write_burst: 5,
            delegated_delete_per_minute: 5,
            delegated_delete_burst: 3,
            delegated_client_multiplier: 20,
        }
    }
}

impl RateLimitSettings {
    pub fn from_env() -> Result<Self> {
        use clap::Parser;
        let settings = Self::try_parse_from(std::iter::empty::<String>())
            .context("invalid api rate-limit settings in the environment")?;
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<()> {
        parse_cidrs(&self.trusted_proxies).context("invalid API_RATE_LIMIT_TRUSTED_PROXIES")?;
        for (class, (per_minute, burst)) in [
            (LimitClass::Login, (self.login_per_minute, self.login_burst)),
            (
                LimitClass::TripPlan,
                (self.trip_plan_per_minute, self.trip_plan_burst),
            ),
            (
                LimitClass::TrainByUid,
                (self.train_by_uid_per_minute, self.train_by_uid_burst),
            ),
            (
                LimitClass::PublicWrite,
                (self.public_write_per_minute, self.public_write_burst),
            ),
            (
                LimitClass::Events,
                (self.events_per_minute, self.events_burst),
            ),
        ] {
            ensure!(
                per_minute > 0 && burst > 0,
                "the {} rate limit needs a per-minute rate and a burst of at least 1",
                class.label()
            );
        }
        ensure!(
            self.train_preview_per_minute > 0 && self.train_preview_burst > 0,
            "the link-preview rate limit needs a per-minute rate and a burst of at least 1"
        );
        for (class, (per_minute, burst)) in [
            (
                LimitClass::TripPlan,
                (self.mcp_trip_plan_per_minute, self.mcp_trip_plan_burst),
            ),
            (
                LimitClass::TrainByUid,
                (
                    self.mcp_train_by_uid_per_minute,
                    self.mcp_train_by_uid_burst,
                ),
            ),
            (
                LimitClass::PublicWrite,
                (
                    self.mcp_public_write_per_minute,
                    self.mcp_public_write_burst,
                ),
            ),
        ] {
            ensure!(
                per_minute > 0 && burst > 0,
                "the MCP {} rate limit needs a per-minute rate and a burst of at least 1 \
                 (it is a finite budget, never an exemption)",
                class.label()
            );
        }
        for (class, (per_minute, burst)) in [
            (
                LimitClass::DelegatedRead,
                (self.delegated_read_per_minute, self.delegated_read_burst),
            ),
            (
                LimitClass::DelegatedWrite,
                (self.delegated_write_per_minute, self.delegated_write_burst),
            ),
            (
                LimitClass::DelegatedDelete,
                (
                    self.delegated_delete_per_minute,
                    self.delegated_delete_burst,
                ),
            ),
        ] {
            ensure!(
                per_minute > 0 && burst > 0,
                "the {} rate limit needs a per-minute rate and a burst of at least 1",
                class.label()
            );
        }
        ensure!(
            self.delegated_client_multiplier > 0,
            "API_RATE_LIMIT_DELEGATED_CLIENT_MULTIPLIER must be at least 1"
        );
        Ok(())
    }

    fn quota(&self, class: LimitClass, caller: Caller) -> Quota {
        let (per_minute, burst) = match (caller, class) {
            (_, LimitClass::Login) => (self.login_per_minute, self.login_burst),
            (_, LimitClass::Events) => (self.events_per_minute, self.events_burst),
            (Caller::Preview, LimitClass::TrainByUid) => {
                (self.train_preview_per_minute, self.train_preview_burst)
            }
            (_, LimitClass::DelegatedRead) => {
                (self.delegated_read_per_minute, self.delegated_read_burst)
            }
            (_, LimitClass::DelegatedWrite) => {
                (self.delegated_write_per_minute, self.delegated_write_burst)
            }
            (_, LimitClass::DelegatedDelete) => (
                self.delegated_delete_per_minute,
                self.delegated_delete_burst,
            ),
            (Caller::Mcp, LimitClass::TripPlan) => {
                (self.mcp_trip_plan_per_minute, self.mcp_trip_plan_burst)
            }
            (Caller::Mcp, LimitClass::TrainByUid) => (
                self.mcp_train_by_uid_per_minute,
                self.mcp_train_by_uid_burst,
            ),
            (Caller::Mcp, LimitClass::PublicWrite) => (
                self.mcp_public_write_per_minute,
                self.mcp_public_write_burst,
            ),
            // Delegated callers are only ever charged delegated classes
            // (`enforce`); the public quota is the conservative answer.
            (
                Caller::Public | Caller::DelegatedUser | Caller::DelegatedClient | Caller::Preview,
                LimitClass::TripPlan,
            ) => (self.trip_plan_per_minute, self.trip_plan_burst),
            (
                Caller::Public | Caller::DelegatedUser | Caller::DelegatedClient,
                LimitClass::TrainByUid,
            ) => (self.train_by_uid_per_minute, self.train_by_uid_burst),
            (
                Caller::Public | Caller::DelegatedUser | Caller::DelegatedClient | Caller::Preview,
                LimitClass::PublicWrite,
            ) => (self.public_write_per_minute, self.public_write_burst),
        };
        // The client's ceiling across all its users (spec §5.10).
        if caller == Caller::DelegatedClient {
            return Quota::new(
                per_minute.saturating_mul(self.delegated_client_multiplier),
                burst.saturating_mul(self.delegated_client_multiplier),
            );
        }
        Quota::new(per_minute, burst)
    }
}

#[derive(Debug, Clone, Copy)]
struct Quota {
    /// Time one request "costs".
    interval: Duration,
    /// How far ahead of now the bucket may run: `interval * burst`.
    tolerance: Duration,
}

impl Quota {
    fn new(per_minute: u32, burst: u32) -> Self {
        let interval = Duration::from_secs(60) / per_minute.max(1);
        Self {
            interval,
            tolerance: interval * burst.max(1),
        }
    }
}

/// Past this many tracked keys, fully recovered ones are pruned; if that
/// isn't enough, the table is cleared (fail open) rather than grow without
/// bound under a spoofed-address flood.
const MAX_TRACKED_KEYS: usize = 100_000;
/// Once still over [`MAX_TRACKED_KEYS`] after dropping recovered keys, the
/// `1/EVICT_DENOMINATOR` of the table whose buckets recover soonest (the
/// least recently charged) is evicted: a spoofed-address flood costs those
/// keys their (nearly spent) history, not every client's budget.
const EVICT_DENOMINATOR: usize = 10;

/// Brings an over-full table back under [`MAX_TRACKED_KEYS`] by evicting the
/// entries with the earliest theoretical arrival time, instead of wiping it.
fn evict_oldest<K: Eq + std::hash::Hash>(tat: &mut HashMap<K, Instant>, what: &str) {
    if tat.len() < MAX_TRACKED_KEYS {
        return;
    }
    let evict = (tat.len() / EVICT_DENOMINATOR).max(tat.len() + 1 - MAX_TRACKED_KEYS);
    tracing::warn!(
        keys = tat.len(),
        evict,
        "{what} table full; evicting the oldest keys"
    );
    metrics::counter!(ds_runtime::metrics::metric_name(
        "api_rate_limit_table_resets_total"
    ))
    .increment(1);
    let mut times: Vec<Instant> = tat.values().copied().collect();
    let (_, cutoff, _) = times.select_nth_unstable(evict - 1);
    let cutoff = *cutoff;
    let mut removed = 0;
    tat.retain(|_, at| {
        if removed < evict && *at <= cutoff {
            removed += 1;
            false
        } else {
            true
        }
    });
}

/// Prune fully recovered keys every this many checks.
const PRUNE_EVERY: u64 = 4096;

/// Verifies an internal OAuth bearer token. Implemented by
/// [`AppState`](crate::app::AppState) (delegating to its one shared
/// `internal_oauth_verifier`, so every check shares a single
/// JWKS cache, negative-`kid` cache and refetch cooldown) and by
/// `ServiceTokenVerifier` itself (tests).
pub trait ServiceTokenCheck: Send + Sync + 'static {
    fn verify_service_token<'a>(
        &'a self,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ServiceClaims, VerifyError>> + Send + 'a>>;
}

impl ServiceTokenCheck for crate::auth::internal_oauth::ServiceTokenVerifier {
    fn verify_service_token<'a>(
        &'a self,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ServiceClaims, VerifyError>> + Send + 'a>> {
        Box::pin(self.verify(token))
    }
}

impl ServiceTokenCheck for crate::app::AppState {
    fn verify_service_token<'a>(
        &'a self,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ServiceClaims, VerifyError>> + Send + 'a>> {
        Box::pin(self.internal_oauth_verifier.verify(token))
    }
}

/// How the limiter recognises the MCP: see the module doc.
pub struct ServiceCallerAuth {
    verifier: Arc<dyn ServiceTokenCheck>,
    mcp_group: String,
}

impl ServiceCallerAuth {
    /// `None` when `mcp_group` is blank: the feature is then inert.
    pub fn new(verifier: Arc<dyn ServiceTokenCheck>, mcp_group: &str) -> Option<Self> {
        let mcp_group = mcp_group.trim();
        (!mcp_group.is_empty()).then(|| Self {
            verifier,
            mcp_group: mcp_group.to_string(),
        })
    }
}

/// What a verified-or-not bearer token earns on a limited route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BearerDecision {
    /// Charge the MCP's own budget.
    Mcp,
    /// Refuse with this status; never fall back to the anonymous bucket.
    Reject(StatusCode),
}

/// The pure half of the bearer check: `verified` is the verifier's result.
/// Only a token that verifies AND lists `mcp_group` earns the MCP budget.
pub fn bearer_decision(
    verified: &Result<ServiceClaims, VerifyError>,
    mcp_group: &str,
) -> BearerDecision {
    match verified {
        Err(_) => BearerDecision::Reject(StatusCode::UNAUTHORIZED),
        Ok(claims) if !mcp_group.is_empty() && claims.groups.iter().any(|g| g == mcp_group) => {
            BearerDecision::Mcp
        }
        Ok(_) => BearerDecision::Reject(StatusCode::FORBIDDEN),
    }
}

/// An IP range, as `addr/prefix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    fn contains(&self, ip: IpAddr) -> bool {
        let (net, ip) = (self.addr.to_canonical(), ip.to_canonical());
        let prefix = u32::from(self.prefix);
        match (net, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// Parses a comma-separated CIDR list; a bare address is a single-host range.
fn parse_cidrs(list: &str) -> Result<Vec<Cidr>> {
    let mut out = Vec::new();
    for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let (addr, prefix) = match entry.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (entry, None),
        };
        let addr: IpAddr = addr
            .parse()
            .with_context(|| format!("`{entry}` is not an IP range"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|prefix| *prefix <= max)
                .with_context(|| format!("`{entry}` has an invalid prefix length"))?,
            None => max,
        };
        // An IPv4-mapped IPv6 range is matched as IPv4 (peers are canonical),
        // so it is stored as IPv4 with the `::ffff:0:0/96` part of the prefix
        // removed; a shorter prefix cannot be expressed and is refused.
        let (addr, prefix) = match addr.to_canonical() {
            IpAddr::V4(v4) if addr.is_ipv6() => {
                let v4_prefix = prefix.checked_sub(96).with_context(|| {
                    format!("`{entry}` is an IPv4-mapped range shorter than /96")
                })?;
                (IpAddr::V4(v4), v4_prefix)
            }
            _ => (addr, prefix),
        };
        out.push(Cidr { addr, prefix });
    }
    Ok(out)
}

/// The limiter: one GCRA timestamp per (class, client key).
pub struct RateLimiter {
    settings: RateLimitSettings,
    /// `settings.trusted_proxies`, parsed; empty trusts every peer.
    trusted_proxies: Vec<Cidr>,
    service_callers: Option<ServiceCallerAuth>,
    state: Mutex<LimiterState>,
    warned_about_peer_fallback: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct LimiterState {
    /// Theoretical arrival time per key: the bucket is empty until then.
    tat: HashMap<(LimitClass, ClientKey), Instant>,
    checks: u64,
}

/// Where a request's key came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    RealIpHeader,
    Peer,
}

impl RateLimiter {
    /// A limiter with no trusted service callers: every bearer is ignored.
    pub fn new(settings: RateLimitSettings) -> Arc<Self> {
        Self::with_service_callers(settings, None)
    }

    /// A limiter that recognises the MCP (see the module doc); `None` is
    /// the same as [`RateLimiter::new`].
    pub fn with_service_callers(
        settings: RateLimitSettings,
        service_callers: Option<ServiceCallerAuth>,
    ) -> Arc<Self> {
        let trusted_proxies = parse_cidrs(&settings.trusted_proxies).unwrap_or_else(|err| {
            // Checked by `validate` at startup; an unparsable list here
            // trusts nobody (a range matching only 0.0.0.0, never a peer)
            // rather than everybody.
            tracing::error!(error = ?err, "invalid trusted proxy list; trusting no peer's X-Real-IP");
            vec![Cidr {
                addr: IpAddr::from([0, 0, 0, 0]),
                prefix: 32,
            }]
        });
        Arc::new(Self {
            settings,
            trusted_proxies,
            service_callers,
            state: Mutex::new(LimiterState::default()),
            warned_about_peer_fallback: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// `Ok` if a request by `client` in `class` is allowed at `now` (and
    /// records it), else `Err(retry_after)`.
    fn check_at(&self, class: LimitClass, client: ClientKey, now: Instant) -> Result<(), Duration> {
        self.check_all_at(class, &[client], now)
            .map_err(|(_, retry_after)| retry_after)
    }

    /// Charges one request in `class` to every one of `clients`' buckets,
    /// or to none: `Err((key, retry_after))`, the first bucket that refused
    /// and the longest wait, and then nothing is recorded. A delegated
    /// request is charged to its user's and its client's buckets, and one
    /// the client's bucket refuses must not also spend the user's (review
    /// finding, 2026-11: it did, so a busy client drained its users' own
    /// budgets with requests that never ran).
    #[expect(
        clippy::expect_used,
        reason = "a poisoned lock means another thread already panicked"
    )]
    fn check_all_at(
        &self,
        class: LimitClass,
        clients: &[ClientKey],
        now: Instant,
    ) -> Result<(), (ClientKey, Duration)> {
        let mut state = self.state.lock().expect("rate limiter lock poisoned");
        state.checks += 1;
        if state.checks.is_multiple_of(PRUNE_EVERY) || state.tat.len() >= MAX_TRACKED_KEYS {
            state.tat.retain(|_, tat| *tat > now);
            evict_oldest(&mut state.tat, "rate limiter");
        }
        let mut updates = Vec::with_capacity(clients.len());
        let mut refused: Option<(ClientKey, Duration)> = None;
        for &client in clients {
            let quota = self.settings.quota(class, client.caller());
            let key = (class, client);
            let tat = state.tat.get(&key).copied().unwrap_or(now).max(now);
            let new_tat = tat + quota.interval;
            let ahead = new_tat - now;
            if ahead > quota.tolerance {
                let wait = ahead.saturating_sub(quota.tolerance);
                refused = Some(match refused {
                    Some((first, longest)) => (first, longest.max(wait)),
                    None => (client, wait),
                });
            } else {
                updates.push((key, new_tat));
            }
        }
        if let Some(refused) = refused {
            return Err(refused);
        }
        state.tat.extend(updates);
        Ok(())
    }

    /// Who a limited request is charged to, or the rejection for a bad
    /// bearer. `Ok(None)`: anonymous, key it per IP. See the module doc.
    async fn service_caller(
        &self,
        class: LimitClass,
        headers: &HeaderMap,
    ) -> Result<Option<Caller>, Box<Response>> {
        let Some(auth) = &self.service_callers else {
            return Ok(None);
        };
        if class == LimitClass::Login {
            return Ok(None);
        }
        let Some(token) = crate::auth::bearer_token(headers) else {
            return Ok(None);
        };
        let verified = auth.verifier.verify_service_token(&token).await;
        match bearer_decision(&verified, &auth.mcp_group) {
            BearerDecision::Mcp => Ok(Some(Caller::Mcp)),
            BearerDecision::Reject(status) => {
                let reason = if status == StatusCode::UNAUTHORIZED {
                    "invalid_token"
                } else {
                    "wrong_group"
                };
                metrics::counter!(
                    ds_runtime::metrics::metric_name("api_rate_limit_service_auth_rejected_total"),
                    "class" => class.label(),
                    "reason" => reason
                )
                .increment(1);
                Err(Box::new(if let Ok(claims) = verified {
                    tracing::warn!(
                        sub = %claims.sub,
                        class = class.label(),
                        "valid internal oauth token without the MCP group on a public \
                         rate-limited route; rejected 403"
                    );
                    (
                        StatusCode::FORBIDDEN,
                        "this service credential is not allowed on public routes",
                    )
                        .into_response()
                } else {
                    let mut response =
                        (StatusCode::UNAUTHORIZED, "invalid bearer token").into_response();
                    response.headers_mut().insert(
                        header::WWW_AUTHENTICATE,
                        HeaderValue::from_static("Bearer error=\"invalid_token\""),
                    );
                    response
                }))
            }
        }
    }

    /// The key for a request: see the module doc.
    pub fn client_key(
        &self,
        headers: &HeaderMap,
        peer: Option<SocketAddr>,
    ) -> Option<(IpAddr, KeySource)> {
        if self.settings.trust_x_real_ip
            && self.peer_is_trusted_proxy(peer)
            && let Some(ip) = headers
                .get(REAL_IP_HEADER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<IpAddr>().ok())
        {
            return Some((normalise(ip), KeySource::RealIpHeader));
        }
        peer.map(|peer| (normalise(peer.ip()), KeySource::Peer))
    }

    /// Whether `peer`'s `X-Real-IP` is believed: always when no trusted
    /// range is configured, else only from a peer inside one.
    fn peer_is_trusted_proxy(&self, peer: Option<SocketAddr>) -> bool {
        self.trusted_proxies.is_empty()
            || peer.is_some_and(|peer| self.trusted_proxies.iter().any(|c| c.contains(peer.ip())))
    }

    /// Whether an anonymous request is the frontend's link-preview read:
    /// `GET /Train/by-uid/*` carrying [`PREVIEW_HEADER`] from a peer whose
    /// `X-Real-IP` would be believed (the module doc's trust rule).
    fn is_preview_read(
        &self,
        class: LimitClass,
        path: &str,
        headers: &HeaderMap,
        peer: Option<SocketAddr>,
    ) -> bool {
        class == LimitClass::TrainByUid
            && path.starts_with("/Train/by-uid/")
            && self.settings.trust_x_real_ip
            && self.peer_is_trusted_proxy(peer)
            && headers
                .get(PREVIEW_HEADER)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.trim() == "1")
    }

    /// Each tracked key's fill, folded per (class, caller): see
    /// [`FillStats`]. One pass over the table under the limiter's lock
    /// (at most [`MAX_TRACKED_KEYS`] entries: a subtraction and a compare
    /// each), run by [`RateLimiter::spawn_fill_gauges`] every
    /// [`FILL_GAUGE_INTERVAL`], never per request.
    #[expect(
        clippy::expect_used,
        reason = "a poisoned lock means another thread already panicked"
    )]
    fn fill_snapshot(&self, now: Instant) -> HashMap<(LimitClass, Caller), FillStats> {
        let state = self.state.lock().expect("rate limiter lock poisoned");
        let mut out: HashMap<(LimitClass, Caller), FillStats> = HashMap::new();
        for (&(class, client), &tat) in &state.tat {
            let caller = client.caller();
            let quota = self.settings.quota(class, caller);
            let fill = tat.saturating_duration_since(now).as_secs_f64()
                / quota.tolerance.as_secs_f64().max(f64::MIN_POSITIVE);
            let series = out.entry((class, caller)).or_default();
            series.max_fill = series.max_fill.max(fill);
            if fill >= NEAR_LIMIT_FILL {
                series.near_limit += 1;
            }
        }
        out
    }

    /// Sets the fill gauges from [`RateLimiter::fill_snapshot`]; every
    /// series with no tracked key goes back to 0.
    fn publish_fill_gauges(&self, now: Instant) {
        let snapshot = self.fill_snapshot(now);
        for (class, caller) in limited_series() {
            let stats = snapshot.get(&(class, caller)).copied().unwrap_or_default();
            metrics::gauge!(
                ds_runtime::metrics::metric_name(MAX_FILL_METRIC),
                "class" => class.label(),
                "caller" => caller.label()
            )
            .set(stats.max_fill);
            let near_limit = f64::from(stats.near_limit);
            metrics::gauge!(
                ds_runtime::metrics::metric_name(NEAR_LIMIT_METRIC),
                "class" => class.label(),
                "caller" => caller.label()
            )
            .set(near_limit);
        }
    }

    /// Publishes the fill gauges every [`FILL_GAUGE_INTERVAL`] for the life
    /// of the process. Needs a Tokio runtime; a disabled limiter tracks
    /// nothing, so it spawns nothing.
    pub fn spawn_fill_gauges(self: &Arc<Self>) {
        if !self.settings.enabled {
            return;
        }
        let limiter = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(FILL_GAUGE_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                limiter.publish_fill_gauges(Instant::now());
            }
        });
    }

    fn note_peer_fallback(&self, class: LimitClass) {
        metrics::counter!(
            ds_runtime::metrics::metric_name("api_rate_limit_peer_fallback_total"),
            "class" => class.label()
        )
        .increment(1);
        if !self
            .warned_about_peer_fallback
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(
                "a rate-limited request had no usable X-Real-IP header, so it was keyed on the \
                 socket peer (usually the frontend pod, shared by every such client); logged \
                 once, counted in distant_signal_api_rate_limit_peer_fallback_total"
            );
        }
    }
}

/// A bucket's fill at or above this counts as near its limit.
const NEAR_LIMIT_FILL: f64 = 0.8;
/// How often the fill gauges are recomputed.
const FILL_GAUGE_INTERVAL: Duration = Duration::from_secs(15);
/// `distant_signal_api_rate_limit_max_fill_ratio{class,caller}`: the fullest
/// tracked bucket, from 0 (idle) to 1 (the next request is refused). No
/// per-key label: it says how close any client gets without naming one.
const MAX_FILL_METRIC: &str = "api_rate_limit_max_fill_ratio";
/// `distant_signal_api_rate_limit_keys_near_limit{class,caller}`: tracked
/// buckets at or above [`NEAR_LIMIT_FILL`].
const NEAR_LIMIT_METRIC: &str = "api_rate_limit_keys_near_limit";

/// One (class, caller)'s fill over the tracked keys, at a moment.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct FillStats {
    max_fill: f64,
    near_limit: u32,
}

/// Every (class, caller) pair [`enforce`] can charge: the label sets of the
/// rate-limit metrics. `every_charged_pair_is_in_limited_series` drives the
/// real middleware to keep this list whole.
fn limited_series() -> Vec<(LimitClass, Caller)> {
    const ANONYMOUS: [LimitClass; 5] = [
        LimitClass::Login,
        LimitClass::TripPlan,
        LimitClass::TrainByUid,
        LimitClass::PublicWrite,
        LimitClass::Events,
    ];
    // The MCP's bearer is ignored on login.
    const MCP: [LimitClass; 4] = [
        LimitClass::TripPlan,
        LimitClass::TrainByUid,
        LimitClass::PublicWrite,
        LimitClass::Events,
    ];
    const DELEGATED: [LimitClass; 3] = [
        LimitClass::DelegatedRead,
        LimitClass::DelegatedWrite,
        LimitClass::DelegatedDelete,
    ];
    let mut out = Vec::new();
    out.extend(ANONYMOUS.map(|class| (class, Caller::Public)));
    out.extend(MCP.map(|class| (class, Caller::Mcp)));
    out.push((LimitClass::TrainByUid, Caller::Preview));
    for class in DELEGATED {
        out.push((class, Caller::DelegatedUser));
        out.push((class, Caller::DelegatedClient));
    }
    out
}

/// Registers the limiter's series at 0 when the api starts. The counters
/// are otherwise created by their first increment, already at 1, and
/// `increase()` cannot see that first hit: with the api restarting every
/// few hours, a rare limit's first 429 after each restart never showed.
/// Registers `api_rate_limited_total{class,caller}` for every pair in
/// [`limited_series`] (plus the personal-token listener's three),
/// `api_rate_limit_peer_fallback_total{class}` for the anonymous classes,
/// `api_rate_limit_service_auth_rejected_total{class,reason}`, and the fill
/// gauges. Like `route_metrics::register`, call it once the recorder is
/// installed.
pub fn register_metrics() {
    for (class, caller) in limited_series() {
        metrics::counter!(
            ds_runtime::metrics::metric_name("api_rate_limited_total"),
            "class" => class.label(),
            "caller" => caller.label()
        )
        .increment(0);
        metrics::gauge!(
            ds_runtime::metrics::metric_name(MAX_FILL_METRIC),
            "class" => class.label(),
            "caller" => caller.label()
        )
        .set(0.0);
        metrics::gauge!(
            ds_runtime::metrics::metric_name(NEAR_LIMIT_METRIC),
            "class" => class.label(),
            "caller" => caller.label()
        )
        .set(0.0);
    }
    // `crate::personal_tokens::gate` and `crate::auth::test_auth` count
    // their own refusals in the same series.
    for caller in ["personal_auth_failure", "personal_token", "test_auth"] {
        metrics::counter!(
            ds_runtime::metrics::metric_name("api_rate_limited_total"),
            "class" => "personal_token",
            "caller" => caller
        )
        .increment(0);
    }
    for class in [
        LimitClass::Login,
        LimitClass::TripPlan,
        LimitClass::TrainByUid,
        LimitClass::PublicWrite,
        LimitClass::Events,
    ] {
        metrics::counter!(
            ds_runtime::metrics::metric_name("api_rate_limit_peer_fallback_total"),
            "class" => class.label()
        )
        .increment(0);
    }
    for class in [
        LimitClass::TripPlan,
        LimitClass::TrainByUid,
        LimitClass::PublicWrite,
        LimitClass::Events,
    ] {
        for reason in ["invalid_token", "wrong_group"] {
            metrics::counter!(
                ds_runtime::metrics::metric_name("api_rate_limit_service_auth_rejected_total"),
                "class" => class.label(),
                "reason" => reason
            )
            .increment(0);
        }
    }
}

/// A GCRA table keyed by `K`, for a limiter outside [`RateLimiter`]'s
/// classes: the personal-token listener's buckets (`crate::personal_tokens`).
/// The same algorithm, pruning and fail-open bound as
/// [`RateLimiter::check_at`]. State is per pod, in memory.
pub struct KeyedLimiter<K> {
    state: Mutex<KeyedState<K>>,
}

struct KeyedState<K> {
    tat: HashMap<K, Instant>,
    checks: u64,
}

impl<K: Copy + Eq + std::hash::Hash> Default for KeyedLimiter<K> {
    fn default() -> Self {
        Self {
            state: Mutex::new(KeyedState {
                tat: HashMap::new(),
                checks: 0,
            }),
        }
    }
}

impl<K: Copy + Eq + std::hash::Hash> KeyedLimiter<K> {
    /// Charges one request to every `(key, per_minute, burst)` bucket, or to
    /// none: `Err(retry_after)` (the longest wait) when any is empty, and
    /// then nothing is recorded.
    #[expect(
        clippy::expect_used,
        reason = "a poisoned lock means another thread already panicked"
    )]
    pub fn check_all(&self, buckets: &[(K, u32, u32)], now: Instant) -> Result<(), Duration> {
        let mut state = self.state.lock().expect("keyed limiter lock poisoned");
        state.checks += 1;
        if state.checks.is_multiple_of(PRUNE_EVERY) || state.tat.len() >= MAX_TRACKED_KEYS {
            state.tat.retain(|_, tat| *tat > now);
            evict_oldest(&mut state.tat, "keyed rate limiter");
        }
        let mut updates = Vec::with_capacity(buckets.len());
        let mut wait = Duration::ZERO;
        for &(key, per_minute, burst) in buckets {
            let quota = Quota::new(per_minute, burst);
            let tat = state.tat.get(&key).copied().unwrap_or(now).max(now);
            let new_tat = tat + quota.interval;
            let ahead = new_tat - now;
            if ahead > quota.tolerance {
                wait = wait.max(ahead.saturating_sub(quota.tolerance));
            } else {
                updates.push((key, new_tat));
            }
        }
        if wait > Duration::ZERO {
            return Err(wait);
        }
        for (key, new_tat) in updates {
            state.tat.insert(key, new_tat);
        }
        Ok(())
    }

    /// Whether `key`'s bucket would refuse a request now, without charging
    /// it.
    #[expect(
        clippy::expect_used,
        reason = "a poisoned lock means another thread already panicked"
    )]
    pub fn would_refuse(
        &self,
        key: K,
        per_minute: u32,
        burst: u32,
        now: Instant,
    ) -> Option<Duration> {
        let quota = Quota::new(per_minute, burst);
        let state = self.state.lock().expect("keyed limiter lock poisoned");
        let tat = state.tat.get(&key).copied().unwrap_or(now).max(now);
        let ahead = tat + quota.interval - now;
        (ahead > quota.tolerance).then(|| ahead.saturating_sub(quota.tolerance))
    }
}

/// IPv4-mapped IPv6 as IPv4; other IPv6 cut to its /64, since one client
/// usually holds a whole /64.
pub(crate) fn normalise(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                IpAddr::V4(v4)
            } else {
                let bits = u128::from(v6) & !((1u128 << 64) - 1);
                IpAddr::V6(Ipv6Addr::from(bits))
            }
        }
    }
}

/// The middleware. Layer it on the top-level router with
/// `axum::middleware::from_fn_with_state(limiter, rate_limit::enforce)`.
pub async fn enforce(
    State(limiter): State<Arc<RateLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    if !limiter.settings.enabled {
        return next.run(request).await;
    }
    if let Some(delegated) = request
        .extensions()
        .get::<crate::auth::delegated::DelegatedToken>()
    {
        let class = delegated_class(delegated);
        let user = ClientKey::DelegatedUser {
            client: delegated.client_index,
            user: sub_digest(&delegated.sub),
        };
        let client = ClientKey::DelegatedClient(delegated.client_index);
        if let Err((key, retry_after)) =
            limiter.check_all_at(class, &[user, client], Instant::now())
        {
            metrics::counter!(
                ds_runtime::metrics::metric_name("api_rate_limited_total"),
                "class" => class.label(),
                "caller" => key.caller().label()
            )
            .increment(1);
            tracing::debug!(%key, class = class.label(), "delegated caller rate limited");
            return delegated_too_many_requests(retry_after);
        }
        return next.run(request).await;
    }
    let Some(class) = classify(request.method(), request.uri().path()) else {
        return next.run(request).await;
    };
    let client = match limiter.service_caller(class, request.headers()).await {
        Err(rejection) => return *rejection,
        Ok(Some(caller)) => ClientKey::Service(caller),
        Ok(None) => {
            let peer = request
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| *addr);
            if limiter.is_preview_read(class, request.uri().path(), request.headers(), peer) {
                ClientKey::Service(Caller::Preview)
            } else {
                let Some((client, source)) = limiter.client_key(request.headers(), peer) else {
                    // No header and no peer: only in tests that bypass the listener.
                    return next.run(request).await;
                };
                if source == KeySource::Peer && limiter.settings.trust_x_real_ip {
                    limiter.note_peer_fallback(class);
                }
                ClientKey::Ip(client)
            }
        }
    };
    match limiter.check_at(class, client, Instant::now()) {
        Ok(()) => next.run(request).await,
        Err(retry_after) => {
            let caller = client.caller();
            metrics::counter!(
                ds_runtime::metrics::metric_name("api_rate_limited_total"),
                "class" => class.label(),
                "caller" => caller.label()
            )
            .increment(1);
            if caller != Caller::Public {
                tracing::debug!(%client, class = class.label(), "service caller rate limited");
            }
            too_many_requests(retry_after)
        }
    }
}

/// A delegated request's class, from the scope its route needs.
fn delegated_class(token: &crate::auth::delegated::DelegatedToken) -> LimitClass {
    use crate::auth::delegated::Scope;
    match token.route_scope {
        // A read, charged like the journey reads.
        Scope::JourneysRead | Scope::PreferencesRead => LimitClass::DelegatedRead,
        Scope::JourneysWrite => LimitClass::DelegatedWrite,
        Scope::JourneysDelete => LimitClass::DelegatedDelete,
    }
}

/// A 64-bit digest of a `sub`, so a bucket key never holds the `sub`.
fn sub_digest(sub: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(sub.as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(bytes)
}

/// The delegated error contract's 429 (spec §5.15): JSON, `rate_limited`.
fn delegated_too_many_requests(retry_after: Duration) -> Response {
    let secs = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
    let mut response = crate::auth::delegated::error_response(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "too many requests; please slow down",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
    response
}

fn too_many_requests(retry_after: Duration) -> Response {
    let secs = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        "too many requests; please slow down",
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(secs.max(1)));
    response
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    use super::*;

    fn settings() -> RateLimitSettings {
        RateLimitSettings {
            login_per_minute: 60,
            login_burst: 2,
            trip_plan_per_minute: 60,
            trip_plan_burst: 2,
            train_by_uid_per_minute: 60,
            train_by_uid_burst: 2,
            public_write_per_minute: 60,
            public_write_burst: 2,
            mcp_trip_plan_per_minute: 60,
            mcp_trip_plan_burst: 5,
            mcp_train_by_uid_per_minute: 60,
            mcp_train_by_uid_burst: 5,
            mcp_public_write_per_minute: 60,
            mcp_public_write_burst: 5,
            ..RateLimitSettings::default()
        }
    }

    fn router(settings: RateLimitSettings) -> axum::Router {
        router_with(RateLimiter::new(settings))
    }

    fn router_with(limiter: Arc<RateLimiter>) -> axum::Router {
        let public = axum::Router::new()
            .route("/auth/login", get(|| async { "login" }))
            .route("/auth/callback", get(|| async { "callback" }))
            .route("/events", post(|| async { "event" }))
            .route(
                "/lines",
                post(|| async { "write" }).get(|| async { "read" }),
            );
        axum::Router::new()
            .route("/Trips/plan", get(|| async { "plan" }))
            .route("/Train/by-uid/{uid}/{date}", get(|| async { "train" }))
            .nest("/public", public)
            .layer(axum::middleware::from_fn_with_state(limiter, enforce))
    }

    async fn call(
        router: &axum::Router,
        method: &str,
        uri: &str,
        real_ip: Option<&str>,
        peer: &str,
    ) -> Response {
        let mut request = axum::http::Request::builder().method(method).uri(uri);
        if let Some(ip) = real_ip {
            request = request.header(REAL_IP_HEADER, ip);
        }
        let mut request = request.body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        router.clone().oneshot(request).await.unwrap()
    }

    const FRONTEND: &str = "10.42.0.9:40000";

    /// A request [`crate::auth::delegated::gate`] verified as delegated:
    /// the extension it leaves for this middleware.
    fn delegated_request(
        sub: &str,
        scope: crate::auth::delegated::Scope,
    ) -> axum::http::Request<Body> {
        use crate::auth::delegated::{ClientConfig, DelegatedToken, Scope};
        let mut request = axum::http::Request::builder()
            .method("POST")
            .uri("/Journeys/itinerary")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(FRONTEND.parse::<SocketAddr>().unwrap()));
        request.extensions_mut().insert(DelegatedToken {
            client: Arc::new(ClientConfig {
                name: "ds-mcp".to_string(),
                display_name: "Assistant".to_string(),
                issuer_url: "https://sso.example/application/o/distant-signal-api/".to_string(),
                audience: "distant-signal-api".to_string(),
                max_scopes: Scope::ALL.to_vec(),
                enabled: true,
                service_group: String::new(),
            }),
            client_index: 0,
            sub: sub.to_string(),
            scopes: vec![scope],
            iat: 0,
            jti: None,
            route_scope: scope,
            audit: Arc::default(),
        });
        request
    }

    #[tokio::test]
    async fn delegated_requests_use_the_user_and_client_buckets() {
        use crate::auth::delegated::Scope;
        let router = axum::Router::new()
            .route("/Journeys/itinerary", post(|| async { "saved" }))
            .layer(axum::middleware::from_fn_with_state(
                RateLimiter::new(RateLimitSettings {
                    delegated_write_per_minute: 1,
                    delegated_write_burst: 2,
                    delegated_client_multiplier: 2,
                    ..RateLimitSettings::default()
                }),
                enforce,
            ));
        for _ in 0..2 {
            let response = router
                .clone()
                .oneshot(delegated_request("alice", Scope::JourneysWrite))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        // Alice's own bucket is spent: the delegated JSON 429.
        let response = router
            .clone()
            .oneshot(delegated_request("alice", Scope::JourneysWrite))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key(header::RETRY_AFTER));
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"], "rate_limited");
        // Bob has his own bucket, but the client's ceiling (2 x 2) is now
        // reached after two of his.
        for _ in 0..2 {
            let response = router
                .clone()
                .oneshot(delegated_request("bob", Scope::JourneysWrite))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let response = router
            .clone()
            .oneshot(delegated_request("carol", Scope::JourneysWrite))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the per-client ceiling bounds every user together"
        );
    }

    /// A delegated request the client's bucket refuses spends nothing from
    /// the user's own bucket: once the client has room again, the user's
    /// full burst is still there.
    #[test]
    fn a_refused_delegated_request_charges_neither_bucket() {
        let limiter = RateLimiter::new(RateLimitSettings {
            delegated_write_per_minute: 1,
            delegated_write_burst: 2,
            delegated_client_multiplier: 1,
            ..RateLimitSettings::default()
        });
        let class = LimitClass::DelegatedWrite;
        let client = ClientKey::DelegatedClient(0);
        let user = |name: &str| ClientKey::DelegatedUser {
            client: 0,
            user: sub_digest(name),
        };
        let now = Instant::now();
        // Bob spends the client's whole burst (2 x 1).
        for _ in 0..2 {
            assert!(
                limiter
                    .check_all_at(class, &[user("bob"), client], now)
                    .is_ok()
            );
        }
        // Alice is refused by the client's bucket, many times over.
        for _ in 0..5 {
            let (key, _) = limiter
                .check_all_at(class, &[user("alice"), client], now)
                .expect_err("the client's bucket is empty");
            assert_eq!(key, client, "the bucket that refused is reported");
        }
        // Her own bucket is untouched: a lone check of it has her whole
        // burst left.
        for _ in 0..2 {
            assert!(limiter.check_at(class, user("alice"), now).is_ok());
        }
        assert!(limiter.check_at(class, user("alice"), now).is_err());
    }

    #[test]
    fn delegated_bucket_keys_never_hold_the_sub() {
        let key = ClientKey::DelegatedUser {
            client: 0,
            user: sub_digest("a4022ea6-secret-sub"),
        };
        assert!(!key.to_string().contains("a4022ea6"));
        assert_eq!(ClientKey::DelegatedClient(3).to_string(), "svc:3-delegated");
    }

    #[tokio::test]
    async fn hitting_the_limit_returns_429_with_retry_after() {
        let router = router(settings());
        for _ in 0..2 {
            let response = call(&router, "GET", "/Trips/plan", Some("203.0.113.5"), FRONTEND).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let response = call(&router, "GET", "/Trips/plan", Some("203.0.113.5"), FRONTEND).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after: u64 = response.headers()[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!((1..=2).contains(&retry_after), "{retry_after}");
    }

    #[tokio::test]
    async fn different_client_ips_have_independent_buckets() {
        let router = router(settings());
        for _ in 0..2 {
            call(
                &router,
                "GET",
                "/public/auth/login",
                Some("203.0.113.5"),
                FRONTEND,
            )
            .await;
        }
        let limited = call(
            &router,
            "GET",
            "/public/auth/login",
            Some("203.0.113.5"),
            FRONTEND,
        )
        .await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        let other = call(
            &router,
            "GET",
            "/public/auth/login",
            Some("198.51.100.7"),
            FRONTEND,
        )
        .await;
        assert_eq!(other.status(), StatusCode::OK);
    }

    /// Each class has its own bucket.
    #[tokio::test]
    async fn classes_are_independent() {
        let router = router(settings());
        for _ in 0..3 {
            call(&router, "GET", "/Trips/plan", Some("203.0.113.5"), FRONTEND).await;
        }
        let response = call(
            &router,
            "GET",
            "/Train/by-uid/C12345/2026-09-27",
            Some("203.0.113.5"),
            FRONTEND,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Usage events (DS-190) have their own per-IP budget: running it out
    /// leaves the visitor's public writes alone, and the reverse.
    #[tokio::test]
    async fn usage_events_have_their_own_budget() {
        let router = router(RateLimitSettings {
            events_per_minute: 60,
            events_burst: 2,
            ..settings()
        });
        let ip = Some("203.0.113.7");
        for _ in 0..2 {
            let response = call(&router, "POST", "/public/events", ip, FRONTEND).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let limited = call(&router, "POST", "/public/events", ip, FRONTEND).await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.headers().contains_key(header::RETRY_AFTER));
        let write = call(&router, "POST", "/public/lines", ip, FRONTEND).await;
        assert_eq!(write.status(), StatusCode::OK);
        let other = call(
            &router,
            "POST",
            "/public/events",
            Some("203.0.113.8"),
            FRONTEND,
        )
        .await;
        assert_eq!(other.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn public_writes_are_limited_but_reads_are_not() {
        let router = router(settings());
        for _ in 0..10 {
            let response = call(
                &router,
                "GET",
                "/public/lines",
                Some("203.0.113.5"),
                FRONTEND,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        for _ in 0..2 {
            call(
                &router,
                "POST",
                "/public/lines",
                Some("203.0.113.5"),
                FRONTEND,
            )
            .await;
        }
        let response = call(
            &router,
            "POST",
            "/public/lines",
            Some("203.0.113.5"),
            FRONTEND,
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    /// Header trust on: different X-Real-IP values from the same frontend
    /// peer are different clients.
    #[tokio::test]
    async fn with_header_trust_on_the_header_is_the_key() {
        let router = router(settings());
        for ip in ["203.0.113.1", "203.0.113.2", "203.0.113.3", "203.0.113.4"] {
            let response = call(&router, "GET", "/Trips/plan", Some(ip), FRONTEND).await;
            assert_eq!(response.status(), StatusCode::OK, "{ip}");
        }
    }

    /// Header trust off: the header is ignored and the socket peer is the
    /// key, so a client can't dodge the limit by varying it.
    #[tokio::test]
    async fn with_header_trust_off_the_peer_is_the_key() {
        let router = router(RateLimitSettings {
            trust_x_real_ip: false,
            ..settings()
        });
        for ip in ["203.0.113.1", "203.0.113.2"] {
            let response = call(&router, "GET", "/Trips/plan", Some(ip), "192.0.2.10:5000").await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let response = call(
            &router,
            "GET",
            "/Trips/plan",
            Some("203.0.113.3"),
            "192.0.2.10:5000",
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let other_peer = call(&router, "GET", "/Trips/plan", None, "192.0.2.11:5000").await;
        assert_eq!(other_peer.status(), StatusCode::OK);
    }

    /// X-Forwarded-For is never a key: varying it changes nothing.
    #[tokio::test]
    async fn x_forwarded_for_is_ignored() {
        let router = router(settings());
        for n in 0..3 {
            let mut request = axum::http::Request::builder()
                .uri("/Trips/plan")
                .header("x-forwarded-for", format!("198.51.100.{n}"))
                .body(Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo(FRONTEND.parse::<SocketAddr>().unwrap()));
            let response = router.clone().oneshot(request).await.unwrap();
            let expected = if n < 2 {
                StatusCode::OK
            } else {
                StatusCode::TOO_MANY_REQUESTS
            };
            assert_eq!(response.status(), expected, "request {n}");
        }
    }

    #[tokio::test]
    async fn disabled_limits_nothing() {
        let router = router(RateLimitSettings {
            enabled: false,
            ..settings()
        });
        for _ in 0..10 {
            let response = call(&router, "GET", "/Trips/plan", Some("203.0.113.5"), FRONTEND).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    #[test]
    fn the_bucket_refills_at_the_sustained_rate() {
        let limiter = RateLimiter::new(RateLimitSettings {
            trip_plan_per_minute: 60,
            trip_plan_burst: 1,
            ..settings()
        });
        let client = ClientKey::Ip("203.0.113.5".parse().unwrap());
        let start = Instant::now();
        assert!(
            limiter
                .check_at(LimitClass::TripPlan, client, start)
                .is_ok()
        );
        let retry = limiter
            .check_at(LimitClass::TripPlan, client, start)
            .expect_err("burst of 1 used up");
        assert_eq!(retry, Duration::from_secs(1));
        assert!(
            limiter
                .check_at(LimitClass::TripPlan, client, start + Duration::from_secs(1))
                .is_ok()
        );
    }

    #[test]
    fn ipv6_clients_are_keyed_by_their_slash_64() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::2".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(normalise(a), normalise(b));
        assert_ne!(normalise(a), normalise(c));
        let mapped: IpAddr = "::ffff:203.0.113.5".parse().unwrap();
        assert_eq!(normalise(mapped), "203.0.113.5".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn classification() {
        assert_eq!(
            classify(&Method::GET, "/public/auth/login"),
            Some(LimitClass::Login)
        );
        assert_eq!(
            classify(&Method::GET, "/public/auth/callback"),
            Some(LimitClass::Login)
        );
        assert_eq!(
            classify(&Method::GET, "/Trips/plan"),
            Some(LimitClass::TripPlan)
        );
        assert_eq!(
            classify(&Method::POST, "/Train/by-uid/C1/2026-09-27/track"),
            Some(LimitClass::TrainByUid)
        );
        assert_eq!(
            classify(&Method::DELETE, "/public/account"),
            Some(LimitClass::PublicWrite)
        );
        assert_eq!(
            classify(&Method::POST, "/Journeys/irish-train"),
            Some(LimitClass::PublicWrite)
        );
        assert_eq!(
            classify(&Method::GET, "/Journeys/12/repeat"),
            Some(LimitClass::TripPlan)
        );
        assert_eq!(
            classify(&Method::POST, "/Journeys/12/move"),
            Some(LimitClass::PublicWrite)
        );
        assert_eq!(
            classify(&Method::POST, "/public/events"),
            Some(LimitClass::Events)
        );
        assert_eq!(classify(&Method::GET, "/public/lines"), None);
        assert_eq!(
            classify(&Method::GET, "/public/feeds/lines/swr-main"),
            Some(LimitClass::TrainByUid)
        );
        assert_eq!(
            classify(&Method::HEAD, "/public/feeds/network"),
            Some(LimitClass::TrainByUid)
        );
        assert_eq!(
            classify(&Method::GET, "/public/calendar/some-token.ics"),
            Some(LimitClass::TrainByUid)
        );
        assert_eq!(
            classify(
                &Method::GET,
                "/public/island-of-ireland/trains/A120/2026-11-05"
            ),
            Some(LimitClass::TrainByUid)
        );
        assert_eq!(
            classify(
                &Method::GET,
                "/public/island-of-ireland/stations/CNLLY/departures"
            ),
            None
        );
        assert_eq!(
            classify(&Method::GET, "/public/tfl/stops/940GZZLURSQ/arrivals"),
            Some(LimitClass::TrainByUid)
        );
        assert_eq!(
            classify(&Method::GET, "/public/tfl/stops/940GZZLURSQ"),
            None
        );
        assert_eq!(classify(&Method::GET, "/public/tfl/stops//arrivals"), None);
        // The sitemap's id lists: one cached primary-key scan each.
        assert_eq!(classify(&Method::GET, "/public/stations/codes"), None);
        assert_eq!(classify(&Method::GET, "/public/stations/index"), None);
        assert_eq!(classify(&Method::GET, "/public/tfl/stops/ids"), None);
        assert_eq!(
            classify(&Method::GET, "/Line/Mode/national-rail/Status"),
            None
        );
    }

    #[test]
    fn mcp_budget_defaults_are_five_times_public() {
        let d = RateLimitSettings::default();
        assert_eq!(
            (d.mcp_trip_plan_per_minute, d.mcp_trip_plan_burst),
            (100, 30)
        );
        assert_eq!(
            (d.mcp_train_by_uid_per_minute, d.mcp_train_by_uid_burst),
            (600, 200)
        );
        assert_eq!(
            (d.mcp_public_write_per_minute, d.mcp_public_write_burst),
            (600, 200)
        );
        assert_eq!(d.mcp_trip_plan_per_minute, 5 * d.trip_plan_per_minute);
        assert_eq!(d.mcp_train_by_uid_per_minute, 5 * d.train_by_uid_per_minute);
        assert_eq!(d.mcp_public_write_per_minute, 5 * d.public_write_per_minute);
    }

    #[test]
    fn a_zero_mcp_rate_is_rejected() {
        let settings = RateLimitSettings {
            mcp_trip_plan_burst: 0,
            ..RateLimitSettings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn service_keys_display_as_svc_colon_caller() {
        assert_eq!(ClientKey::Service(Caller::Mcp).to_string(), "svc:mcp");
        assert_eq!(ClientKey::Service(Caller::Mcp).caller(), Caller::Mcp);
        let ip = ClientKey::Ip("203.0.113.5".parse().unwrap());
        assert_eq!(ip.caller(), Caller::Public);
    }

    fn claims(groups: &[&str]) -> ServiceClaims {
        ServiceClaims {
            sub: "srv-ds-mcp".to_string(),
            iss: "https://sso.example/".to_string(),
            aud: crate::auth::internal_oauth::Audience::Single("internal".to_string()),
            exp: i64::MAX,
            nbf: None,
            iat: None,
            groups: groups.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn bearer_decision_grants_the_mcp_budget_only_to_a_valid_token_in_the_group() {
        assert_eq!(
            bearer_decision(&Ok(claims(&["other", "srv-ds-mcp"])), "srv-ds-mcp"),
            BearerDecision::Mcp
        );
        // A valid token for another service (wrong group): refused, not
        // anonymous and certainly not the MCP budget.
        assert_eq!(
            bearer_decision(&Ok(claims(&["svc-poller-tfl"])), "srv-ds-mcp"),
            BearerDecision::Reject(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            bearer_decision(&Ok(claims(&[])), "srv-ds-mcp"),
            BearerDecision::Reject(StatusCode::FORBIDDEN)
        );
        // An empty configured group never matches (defence in depth; the
        // limiter is inert then anyway).
        assert_eq!(
            bearer_decision(&Ok(claims(&[""])), ""),
            BearerDecision::Reject(StatusCode::FORBIDDEN)
        );
        for err in [
            VerifyError::Malformed,
            VerifyError::UnknownKey,
            VerifyError::Invalid,
        ] {
            assert_eq!(
                bearer_decision(&Err(err), "srv-ds-mcp"),
                BearerDecision::Reject(StatusCode::UNAUTHORIZED)
            );
        }
    }

    /// Never consulted: proves the inert and no-bearer paths don't verify.
    struct PanickingVerifier;

    impl ServiceTokenCheck for PanickingVerifier {
        fn verify_service_token<'a>(
            &'a self,
            _token: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<ServiceClaims, VerifyError>> + Send + 'a>> {
            panic!("the verifier must not be called on this path")
        }
    }

    #[test]
    fn an_empty_mcp_group_makes_the_feature_inert() {
        assert!(ServiceCallerAuth::new(Arc::new(PanickingVerifier), "").is_none());
        assert!(ServiceCallerAuth::new(Arc::new(PanickingVerifier), "  ").is_none());
        assert!(ServiceCallerAuth::new(Arc::new(PanickingVerifier), "srv-ds-mcp").is_some());
    }

    /// No bearer: the anonymous path never touches the verifier.
    #[tokio::test]
    async fn without_a_bearer_the_verifier_is_never_called() {
        let router = router_with(RateLimiter::with_service_callers(
            settings(),
            ServiceCallerAuth::new(Arc::new(PanickingVerifier), "srv-ds-mcp"),
        ));
        for _ in 0..2 {
            let response = call(&router, "GET", "/Trips/plan", Some("203.0.113.5"), FRONTEND).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let response = call(&router, "GET", "/Trips/plan", Some("203.0.113.5"), FRONTEND).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    // --- Integration-style: a real ServiceTokenVerifier against a mock
    // Authentik JWKS (`auth::internal_oauth::test_support`). ---

    use crate::auth::internal_oauth::test_support::{mock_authentik, sign_token, valid_claims};

    /// The MCP pod calling in-cluster: no X-Real-IP, its own pod IP as peer.
    const MCP_POD: &str = "10.42.7.3:51000";

    async fn call_with_bearer(
        router: &axum::Router,
        method: &str,
        uri: &str,
        bearer: Option<&str>,
        peer: &str,
    ) -> Response {
        let mut request = axum::http::Request::builder().method(method).uri(uri);
        if let Some(token) = bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let mut request = request.body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        router.clone().oneshot(request).await.unwrap()
    }

    fn token(issuer: &str, groups: &[&str]) -> String {
        let groups: Vec<String> = groups.iter().map(ToString::to_string).collect();
        sign_token(&valid_claims(issuer, |c| {
            c["sub"] = serde_json::json!("srv-ds-mcp");
            c["groups"] = serde_json::json!(groups);
        }))
    }

    async fn mcp_router() -> (wiremock::MockServer, axum::Router) {
        let (server, verifier) = mock_authentik().await;
        let router = router_with(RateLimiter::with_service_callers(
            settings(),
            ServiceCallerAuth::new(Arc::new(verifier), "srv-ds-mcp"),
        ));
        (server, router)
    }

    /// A valid MCP token gets the MCP budget (burst 5 here, vs 2 public),
    /// keyed on the identity: changing the peer doesn't reset it, and it
    /// doesn't touch the anonymous bucket of the same peer.
    #[tokio::test]
    async fn a_valid_mcp_token_gets_its_own_finite_budget_keyed_on_identity() {
        let (server, router) = mcp_router().await;
        let mcp = token(&server.uri(), &["srv-ds-mcp"]);
        for n in 0..5 {
            let peer = if n % 2 == 0 {
                MCP_POD
            } else {
                "10.42.7.4:51000"
            };
            let response = call_with_bearer(&router, "GET", "/Trips/plan", Some(&mcp), peer).await;
            assert_eq!(response.status(), StatusCode::OK, "request {n}");
        }
        let limited =
            call_with_bearer(&router, "GET", "/Trips/plan", Some(&mcp), "10.42.9.9:1").await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.headers().contains_key(header::RETRY_AFTER));

        // The same pod without a bearer is anonymous, with its own bucket.
        let anonymous = call_with_bearer(&router, "GET", "/Trips/plan", None, MCP_POD).await;
        assert_eq!(anonymous.status(), StatusCode::OK);

        // Other classes have their own MCP buckets.
        let train = call_with_bearer(
            &router,
            "GET",
            "/Train/by-uid/C12345/2026-09-28",
            Some(&mcp),
            MCP_POD,
        )
        .await;
        assert_eq!(train.status(), StatusCode::OK);
    }

    /// Another service's valid token (wrong group) is refused 403, and
    /// never charged to, or granted, any budget.
    #[tokio::test]
    async fn a_valid_token_without_the_mcp_group_is_forbidden() {
        let (server, router) = mcp_router().await;
        let tfl = token(&server.uri(), &["svc-poller-tfl"]);
        for _ in 0..4 {
            let response =
                call_with_bearer(&router, "GET", "/Trips/plan", Some(&tfl), MCP_POD).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }

    /// Expired, garbage and wrong-audience bearers are 401 with
    /// `WWW-Authenticate`, never a silent fall-back to the anonymous bucket.
    #[tokio::test]
    async fn an_invalid_bearer_is_unauthorized_not_anonymous() {
        let (server, router) = mcp_router().await;
        let expired = sign_token(&valid_claims(&server.uri(), |c| {
            c["groups"] = serde_json::json!(["srv-ds-mcp"]);
            c["exp"] =
                serde_json::json!((chrono::Utc::now() - chrono::Duration::hours(1)).timestamp());
        }));
        let wrong_audience = sign_token(&valid_claims(&server.uri(), |c| {
            c["groups"] = serde_json::json!(["srv-ds-mcp"]);
            c["aud"] = serde_json::json!("someone-else");
        }));
        for bad in [expired.as_str(), wrong_audience.as_str(), "not-a-jwt"] {
            for _ in 0..3 {
                let response =
                    call_with_bearer(&router, "POST", "/public/lines", Some(bad), MCP_POD).await;
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{bad}");
                assert_eq!(
                    response.headers()[header::WWW_AUTHENTICATE],
                    "Bearer error=\"invalid_token\""
                );
            }
        }
    }

    /// Login ignores the bearer entirely: anonymous per-IP limit.
    #[tokio::test]
    async fn login_ignores_the_bearer() {
        let (server, router) = mcp_router().await;
        let mcp = token(&server.uri(), &["srv-ds-mcp"]);
        for _ in 0..2 {
            let response =
                call_with_bearer(&router, "GET", "/public/auth/login", Some(&mcp), MCP_POD).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let limited =
            call_with_bearer(&router, "GET", "/public/auth/login", Some("junk"), MCP_POD).await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    /// Unlimited routes (public reads) never look at the bearer.
    #[tokio::test]
    async fn unlimited_routes_ignore_the_bearer() {
        let (_server, router) = mcp_router().await;
        for uri in ["/public/lines"] {
            let response = call_with_bearer(&router, "GET", uri, Some("junk"), MCP_POD).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    /// With the feature off, even a valid MCP token is just anonymous.
    #[tokio::test]
    async fn when_inert_a_valid_mcp_token_is_anonymous() {
        let (server, _verifier) = mock_authentik().await;
        let router = router(settings());
        let mcp = token(&server.uri(), &["srv-ds-mcp"]);
        for _ in 0..2 {
            let response =
                call_with_bearer(&router, "GET", "/Trips/plan", Some(&mcp), MCP_POD).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let limited = call_with_bearer(&router, "GET", "/Trips/plan", Some(&mcp), MCP_POD).await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn a_zero_rate_is_rejected() {
        let settings = RateLimitSettings {
            login_per_minute: 0,
            ..RateLimitSettings::default()
        };
        assert!(settings.validate().is_err());
    }

    #[test]
    fn defaults_match_the_declared_values() {
        use clap::Parser;
        let parsed =
            RateLimitSettings::try_parse_from(std::iter::empty::<String>()).expect("parse");
        assert_eq!(parsed, RateLimitSettings::default());
    }

    #[test]
    fn a_full_table_evicts_the_oldest_keys_and_keeps_the_rest() {
        let now = Instant::now();
        let mut tat: HashMap<usize, Instant> = (0..MAX_TRACKED_KEYS)
            .map(|i| {
                (
                    i,
                    now + Duration::from_millis(u64::try_from(i).unwrap() + 1),
                )
            })
            .collect();
        evict_oldest(&mut tat, "test");
        assert!(tat.len() < MAX_TRACKED_KEYS);
        assert!(tat.len() >= MAX_TRACKED_KEYS - MAX_TRACKED_KEYS / EVICT_DENOMINATOR);
        assert!(!tat.contains_key(&0), "the oldest key is gone");
        assert!(
            tat.contains_key(&(MAX_TRACKED_KEYS - 1)),
            "the newest stays"
        );
        // Below the cap nothing is touched.
        let before = tat.len();
        evict_oldest(&mut tat, "test");
        assert_eq!(tat.len(), before);
    }

    #[test]
    fn cidr_lists_parse_and_match() {
        let cidrs = parse_cidrs("10.0.0.0/8, fd00::/8,192.0.2.7").unwrap();
        let limiter = |list: &str| {
            RateLimiter::new(RateLimitSettings {
                trusted_proxies: list.to_string(),
                ..RateLimitSettings::default()
            })
        };
        assert!(cidrs[0].contains("10.1.2.3".parse().unwrap()));
        assert!(!cidrs[0].contains("11.0.0.1".parse().unwrap()));
        assert!(cidrs[1].contains("fd12::1".parse().unwrap()));
        assert!(cidrs[2].contains("192.0.2.7".parse().unwrap()));
        assert!(!cidrs[2].contains("192.0.2.8".parse().unwrap()));
        assert!(parse_cidrs("10.0.0.0/33").is_err());
        assert!(parse_cidrs("nope").is_err());
        // A mapped range matches as IPv4; one shorter than /96 is refused
        // (it would otherwise wrap into a match-everything mask).
        let mapped = parse_cidrs("::ffff:10.0.0.0/104").unwrap();
        assert!(mapped[0].contains("10.9.9.9".parse().unwrap()));
        assert!(mapped[0].contains("::ffff:10.9.9.9".parse().unwrap()));
        assert!(!mapped[0].contains("11.0.0.1".parse().unwrap()));
        assert!(parse_cidrs("::ffff:10.0.0.0/64").is_err());

        let mut headers = HeaderMap::new();
        headers.insert(REAL_IP_HEADER, HeaderValue::from_static("203.0.113.9"));
        let inside: SocketAddr = "10.1.2.3:4000".parse().unwrap();
        let outside: SocketAddr = "198.51.100.1:4000".parse().unwrap();
        // Unset: any peer is believed (the default, unchanged).
        let open = limiter("");
        assert_eq!(
            open.client_key(&headers, Some(outside)).unwrap().1,
            KeySource::RealIpHeader
        );
        let scoped = limiter("10.0.0.0/8");
        assert_eq!(
            scoped.client_key(&headers, Some(inside)).unwrap().1,
            KeySource::RealIpHeader
        );
        let (ip, source) = scoped.client_key(&headers, Some(outside)).unwrap();
        assert_eq!(source, KeySource::Peer);
        assert_eq!(ip, "198.51.100.1".parse::<IpAddr>().unwrap());
    }

    // ---- metrics registered at 0, fill gauges, link-preview reads ----

    use axum_prometheus::metrics_exporter_prometheus::PrometheusBuilder;

    const RATE_LIMITED: &str = "distant_signal_api_rate_limited_total";
    const PEER_FALLBACK: &str = "distant_signal_api_rate_limit_peer_fallback_total";
    const MAX_FILL: &str = "distant_signal_api_rate_limit_max_fill_ratio";
    const NEAR_LIMIT: &str = "distant_signal_api_rate_limit_keys_near_limit";

    fn label_of<'a>(line: &'a str, name: &str) -> Option<&'a str> {
        let rest = line.split_once(&format!("{name}=\""))?.1;
        Some(rest.split_once('"')?.0)
    }

    /// The value of the series of `metric` carrying every one of `labels`.
    fn series_value(rendered: &str, metric: &str, labels: &[(&str, &str)]) -> Option<f64> {
        rendered
            .lines()
            .filter(|line| line.starts_with(&format!("{metric}{{")))
            .find(|line| {
                labels
                    .iter()
                    .all(|(name, value)| label_of(line, name) == Some(*value))
            })
            .and_then(|line| line.rsplit(' ').next()?.parse().ok())
    }

    #[test]
    fn every_series_is_registered_at_zero() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        register_metrics();
        let rendered = recorder.handle().render();
        for (class, caller) in limited_series() {
            let labels = [("class", class.label()), ("caller", caller.label())];
            for metric in [RATE_LIMITED, MAX_FILL, NEAR_LIMIT] {
                assert_eq!(
                    series_value(&rendered, metric, &labels),
                    Some(0.0),
                    "{metric} {labels:?}"
                );
            }
        }
        for class in [
            "login",
            "trip_plan",
            "train_by_uid",
            "public_write",
            "events",
        ] {
            assert_eq!(
                series_value(&rendered, PEER_FALLBACK, &[("class", class)]),
                Some(0.0),
                "peer fallback {class}"
            );
        }
        for caller in ["personal_auth_failure", "personal_token", "test_auth"] {
            assert_eq!(
                series_value(
                    &rendered,
                    RATE_LIMITED,
                    &[("class", "personal_token"), ("caller", caller)]
                ),
                Some(0.0),
                "personal token {caller}"
            );
        }
        for (class, reason) in [("trip_plan", "invalid_token"), ("events", "wrong_group")] {
            assert_eq!(
                series_value(
                    &rendered,
                    "distant_signal_api_rate_limit_service_auth_rejected_total",
                    &[("class", class), ("reason", reason)]
                ),
                Some(0.0)
            );
        }
    }

    #[tokio::test]
    async fn a_hit_increments_only_its_series() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        register_metrics();
        let router = router(settings());
        let ip = Some("203.0.113.5");
        for _ in 0..2 {
            let response = call(
                &router,
                "GET",
                "/Train/by-uid/C12345/2026-09-27",
                ip,
                FRONTEND,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let limited = call(
            &router,
            "GET",
            "/Train/by-uid/C12345/2026-09-27",
            ip,
            FRONTEND,
        )
        .await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        let rendered = recorder.handle().render();
        for (class, caller) in limited_series() {
            let hit = class == LimitClass::TrainByUid && caller == Caller::Public;
            assert_eq!(
                series_value(
                    &rendered,
                    RATE_LIMITED,
                    &[("class", class.label()), ("caller", caller.label())]
                ),
                Some(if hit { 1.0 } else { 0.0 }),
                "{} {}",
                class.label(),
                caller.label()
            );
        }
        // A real client address was present, so nothing fell back to the peer.
        for class in [
            "login",
            "trip_plan",
            "train_by_uid",
            "public_write",
            "events",
        ] {
            assert_eq!(
                series_value(&rendered, PEER_FALLBACK, &[("class", class)]),
                Some(0.0)
            );
        }
    }

    #[tokio::test]
    async fn a_peer_keyed_request_increments_only_its_fallback_series() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        register_metrics();
        let router = router(settings());
        let response = call(&router, "GET", "/Trips/plan", None, FRONTEND).await;
        assert_eq!(response.status(), StatusCode::OK);
        let rendered = recorder.handle().render();
        for class in [
            "login",
            "trip_plan",
            "train_by_uid",
            "public_write",
            "events",
        ] {
            assert_eq!(
                series_value(&rendered, PEER_FALLBACK, &[("class", class)]),
                Some(if class == "trip_plan" { 1.0 } else { 0.0 }),
                "{class}"
            );
        }
    }

    /// Drives every kind of caller into a 429 through the real middleware
    /// and checks the (class, caller) pairs it counts are exactly
    /// [`limited_series`]: a pair missing from the list would not be
    /// registered at 0, an extra one would be a series that never moves.
    #[tokio::test]
    async fn every_charged_pair_is_in_limited_series() {
        use crate::auth::delegated::Scope;
        let recorder = PrometheusBuilder::new().build_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let one = RateLimitSettings {
            login_burst: 1,
            trip_plan_burst: 1,
            train_by_uid_burst: 1,
            public_write_burst: 1,
            events_burst: 1,
            train_preview_burst: 1,
            mcp_trip_plan_burst: 1,
            mcp_train_by_uid_burst: 1,
            mcp_public_write_burst: 1,
            delegated_read_burst: 1,
            delegated_write_burst: 1,
            delegated_delete_burst: 1,
            delegated_client_multiplier: 1,
            ..settings()
        };
        let (server, verifier) = mock_authentik().await;
        let router = router_with(RateLimiter::with_service_callers(
            one.clone(),
            ServiceCallerAuth::new(Arc::new(verifier), "srv-ds-mcp"),
        ));
        for (method, uri) in [
            ("GET", "/public/auth/login"),
            ("GET", "/Trips/plan"),
            ("GET", "/Train/by-uid/C12345/2026-09-27"),
            ("POST", "/public/lines"),
            ("POST", "/public/events"),
        ] {
            for _ in 0..2 {
                call(&router, method, uri, Some("203.0.113.5"), FRONTEND).await;
            }
        }
        for _ in 0..2 {
            call_preview(
                &router,
                "/Train/by-uid/C12345/2026-09-27",
                Some("1"),
                None,
                FRONTEND,
            )
            .await;
        }
        let mcp = token(&server.uri(), &["srv-ds-mcp"]);
        for (method, uri) in [
            ("GET", "/Trips/plan"),
            ("GET", "/Train/by-uid/C12345/2026-09-27"),
            ("POST", "/public/lines"),
            ("POST", "/public/events"),
        ] {
            for _ in 0..2 {
                call_with_bearer(&router, method, uri, Some(&mcp), MCP_POD).await;
            }
        }
        let delegated = axum::Router::new()
            .route("/Journeys/itinerary", post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                RateLimiter::new(one),
                enforce,
            ));
        for scope in [
            Scope::JourneysRead,
            Scope::JourneysWrite,
            Scope::JourneysDelete,
        ] {
            // The user's own bucket, then the client's ceiling (another
            // user, whose own bucket still has room).
            for sub in ["alice", "alice", "bob"] {
                delegated
                    .clone()
                    .oneshot(delegated_request(sub, scope))
                    .await
                    .unwrap();
            }
        }
        let rendered = recorder.handle().render();
        let mut counted: Vec<(String, String)> = rendered
            .lines()
            .filter(|line| line.starts_with(&format!("{RATE_LIMITED}{{")))
            .filter_map(|line| {
                Some((
                    label_of(line, "class")?.to_string(),
                    label_of(line, "caller")?.to_string(),
                ))
            })
            .collect();
        let mut expected: Vec<(String, String)> = limited_series()
            .into_iter()
            .map(|(class, caller)| (class.label().to_string(), caller.label().to_string()))
            .collect();
        counted.sort();
        expected.sort();
        assert_eq!(counted, expected);
    }

    async fn call_preview(
        router: &axum::Router,
        uri: &str,
        marker: Option<&str>,
        real_ip: Option<&str>,
        peer: &str,
    ) -> Response {
        let mut request = axum::http::Request::builder().method("GET").uri(uri);
        if let Some(marker) = marker {
            request = request.header(PREVIEW_HEADER, marker);
        }
        if let Some(ip) = real_ip {
            request = request.header(REAL_IP_HEADER, ip);
        }
        let mut request = request.body(Body::empty()).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
        router.clone().oneshot(request).await.unwrap()
    }

    const TRAIN: &str = "/Train/by-uid/C12345/2026-09-27";

    /// A preview read has its own bucket: a crawler spending it leaves
    /// every other peer-keyed read alone, and a crowd of other reads
    /// cannot spend the previews' (the frontend pod is the peer of both).
    #[tokio::test]
    async fn preview_reads_have_their_own_bucket() {
        let shared = router(RateLimitSettings {
            train_preview_per_minute: 60,
            train_preview_burst: 3,
            ..settings()
        });
        for n in 0..3 {
            let response = call_preview(&shared, TRAIN, Some("1"), None, FRONTEND).await;
            assert_eq!(response.status(), StatusCode::OK, "preview {n}");
        }
        let limited = call_preview(&shared, TRAIN, Some("1"), None, FRONTEND).await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(limited.headers().contains_key(header::RETRY_AFTER));
        // The same peer's unmarked reads (burst 2) and a visitor's own
        // reads are untouched.
        for _ in 0..2 {
            let response = call_preview(&shared, TRAIN, None, None, FRONTEND).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let visitor = call_preview(&shared, TRAIN, None, Some("203.0.113.5"), FRONTEND).await;
        assert_eq!(visitor.status(), StatusCode::OK);
        // And the reverse: with the peer bucket spent, previews still work.
        let peer_spent = router(settings());
        for _ in 0..2 {
            call_preview(&peer_spent, TRAIN, None, None, FRONTEND).await;
        }
        let spent = call_preview(&peer_spent, TRAIN, None, None, FRONTEND).await;
        assert_eq!(spent.status(), StatusCode::TOO_MANY_REQUESTS);
        let preview = call_preview(&peer_spent, TRAIN, Some("1"), None, FRONTEND).await;
        assert_eq!(preview.status(), StatusCode::OK);
    }

    /// The marker is only believed from a peer whose `X-Real-IP` would be,
    /// only on `/Train/by-uid/*` and only as exactly `1`: anything else is
    /// the caller's own per-IP bucket, so it cannot dodge the limit.
    #[tokio::test]
    async fn the_preview_marker_cannot_dodge_the_per_ip_limit() {
        let preview_burst = RateLimitSettings {
            train_preview_per_minute: 60,
            train_preview_burst: 50,
            ..settings()
        };
        let ip = Some("203.0.113.5");
        let outside = "198.51.100.1:4000";
        // A peer outside the trusted range.
        let scoped = router(RateLimitSettings {
            trusted_proxies: "10.0.0.0/8".to_string(),
            ..preview_burst.clone()
        });
        for _ in 0..2 {
            let response = call_preview(&scoped, TRAIN, Some("1"), None, outside).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let limited = call_preview(&scoped, TRAIN, Some("1"), None, outside).await;
        assert_eq!(
            limited.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "an untrusted peer's marker is ignored"
        );
        // The trusted peer inside the same range is honoured.
        for _ in 0..5 {
            let response = call_preview(&scoped, TRAIN, Some("1"), None, FRONTEND).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        // X-Real-IP trust off: the marker goes with it.
        let untrusting = router(RateLimitSettings {
            trust_x_real_ip: false,
            ..preview_burst.clone()
        });
        for _ in 0..2 {
            call_preview(&untrusting, TRAIN, Some("1"), ip, FRONTEND).await;
        }
        let limited = call_preview(&untrusting, TRAIN, Some("1"), ip, FRONTEND).await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        // Not the exact value: the caller's own per-IP bucket, as before.
        let open = router(preview_burst);
        for (n, marker) in ["true", "0", ""].into_iter().enumerate() {
            let client = format!("203.0.113.{}", 10 + n);
            for _ in 0..2 {
                let response =
                    call_preview(&open, TRAIN, Some(marker), Some(&client), FRONTEND).await;
                assert_eq!(response.status(), StatusCode::OK);
            }
            let limited = call_preview(&open, TRAIN, Some(marker), Some(&client), FRONTEND).await;
            assert_eq!(
                limited.status(),
                StatusCode::TOO_MANY_REQUESTS,
                "{marker:?}"
            );
        }
        // Not a by-uid read: the marker means nothing there.
        let client = Some("203.0.113.6");
        for _ in 0..2 {
            call_preview(&open, "/Trips/plan", Some("1"), client, FRONTEND).await;
        }
        let limited = call_preview(&open, "/Trips/plan", Some("1"), client, FRONTEND).await;
        assert_eq!(
            limited.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "the marker means nothing outside /Train/by-uid/*"
        );
    }

    /// A bearer decides first: the marker never turns an MCP request into
    /// a preview one, nor lets a bad bearer through.
    #[tokio::test]
    async fn a_bearer_beats_the_preview_marker() {
        let (server, router) = mcp_router().await;
        let mcp = token(&server.uri(), &["srv-ds-mcp"]);
        let mut request = axum::http::Request::builder()
            .method("GET")
            .uri(TRAIN)
            .header(PREVIEW_HEADER, "1")
            .header(header::AUTHORIZATION, "Bearer not-a-token")
            .body(Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(FRONTEND.parse::<SocketAddr>().unwrap()));
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = call_with_bearer(&router, "GET", TRAIN, Some(&mcp), MCP_POD).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn preview_defaults_and_validation() {
        let d = RateLimitSettings::default();
        assert_eq!(
            (d.train_preview_per_minute, d.train_preview_burst),
            (600, 200)
        );
        let settings = RateLimitSettings {
            train_preview_burst: 0,
            ..RateLimitSettings::default()
        };
        assert!(settings.validate().is_err());
        assert_eq!(
            ClientKey::Service(Caller::Preview).to_string(),
            "svc:preview"
        );
    }

    #[test]
    fn fill_is_the_share_of_the_burst_a_key_has_spent() {
        // 60 a minute: one request costs a second; a burst of 4 is 4 s.
        let limiter = RateLimiter::new(RateLimitSettings {
            train_by_uid_per_minute: 60,
            train_by_uid_burst: 4,
            ..RateLimitSettings::default()
        });
        let t0 = Instant::now();
        let busy = ClientKey::Ip("203.0.113.1".parse().unwrap());
        let quiet = ClientKey::Ip("203.0.113.2".parse().unwrap());
        let key = (LimitClass::TrainByUid, Caller::Public);
        assert!(limiter.fill_snapshot(t0).is_empty());

        for _ in 0..3 {
            limiter.check_at(LimitClass::TrainByUid, busy, t0).unwrap();
        }
        limiter.check_at(LimitClass::TrainByUid, quiet, t0).unwrap();
        let stats = limiter.fill_snapshot(t0)[&key];
        assert_eq!(
            stats,
            FillStats {
                max_fill: 0.75,
                near_limit: 0
            }
        );

        // The fourth request empties the bucket: fill 1, the next refused.
        limiter.check_at(LimitClass::TrainByUid, busy, t0).unwrap();
        assert!(limiter.check_at(LimitClass::TrainByUid, busy, t0).is_err());
        let stats = limiter.fill_snapshot(t0)[&key];
        assert_eq!(
            stats,
            FillStats {
                max_fill: 1.0,
                near_limit: 1
            }
        );

        // It drains at the sustained rate: two seconds on, half full.
        let later = t0 + Duration::from_secs(2);
        let stats = limiter.fill_snapshot(later)[&key];
        assert_eq!(
            stats,
            FillStats {
                max_fill: 0.5,
                near_limit: 0
            }
        );

        // Recovered keys stay tracked but at 0.
        let stats = limiter.fill_snapshot(t0 + Duration::from_secs(10))[&key];
        assert_eq!(
            stats,
            FillStats {
                max_fill: 0.0,
                near_limit: 0
            }
        );

        // Another caller of the same class is its own series, so the
        // preview bucket's by-design fullness never hides a client's.
        limiter
            .check_at(LimitClass::TrainByUid, ClientKey::Service(Caller::Mcp), t0)
            .unwrap();
        let snapshot = limiter.fill_snapshot(t0);
        assert_eq!(snapshot.len(), 2);
        assert!(snapshot[&(LimitClass::TrainByUid, Caller::Mcp)].max_fill > 0.0);
    }

    #[test]
    fn the_fill_gauges_follow_the_table_and_carry_no_key_label() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let _guard = metrics::set_default_local_recorder(&recorder);
        register_metrics();
        let limiter = RateLimiter::new(RateLimitSettings {
            train_by_uid_per_minute: 60,
            train_by_uid_burst: 4,
            ..RateLimitSettings::default()
        });
        let t0 = Instant::now();
        let client = ClientKey::Ip("203.0.113.1".parse().unwrap());
        for _ in 0..4 {
            limiter
                .check_at(LimitClass::TrainByUid, client, t0)
                .unwrap();
        }
        limiter.publish_fill_gauges(t0);
        let rendered = recorder.handle().render();
        let labels = [("class", "train_by_uid"), ("caller", "public")];
        assert_eq!(series_value(&rendered, MAX_FILL, &labels), Some(1.0));
        assert_eq!(series_value(&rendered, NEAR_LIMIT, &labels), Some(1.0));
        let other = [("class", "login"), ("caller", "public")];
        assert_eq!(series_value(&rendered, MAX_FILL, &other), Some(0.0));
        for line in rendered
            .lines()
            .filter(|line| line.starts_with(MAX_FILL) || line.starts_with(NEAR_LIMIT))
        {
            assert!(!line.contains("203.0.113"), "{line}");
            assert_eq!(
                line.matches('=').count(),
                2,
                "only class and caller: {line}"
            );
        }
        // Later, the bucket has drained and the gauges fall back.
        limiter.publish_fill_gauges(t0 + Duration::from_secs(10));
        let rendered = recorder.handle().render();
        assert_eq!(series_value(&rendered, MAX_FILL, &labels), Some(0.0));
        assert_eq!(series_value(&rendered, NEAR_LIMIT, &labels), Some(0.0));
    }
}
