# Personal API tokens: operator guide

A Distant Signal (DS) user can create a read-only **personal API token** on
the Connected apps page and use it from something they run themselves, such
as Home Assistant, a phone shortcut or a script. The token works only on a
separate API hostname, served by its own listener in the api. Design, threat
model and the amended decision D-14:
[the personal API tokens spec](specs/personal-api-tokens.md). Client-facing
contract: [api-changelog.md](api-changelog.md), 2026-11-03.

**It is off by default.** With `api.personalTokens.enabled: false` (the
default) the api binds no token listener, the chart renders no token Service
and no NetworkPolicy rule for it, the management routes answer `404
feature_disabled`, and the page shows no token section. A `dsp_` token sent
to the website's API is refused either way.

## How a request is accepted

1. The client sends `Authorization: Bearer dsp_...` to the API hostname
   (production: `https://ds-api.cursed.solutions`). The Cloudflare tunnel
   routes it to the `<release>-api-tokens` ClusterIP Service, port 8083.
   The NetworkPolicy admits that port only from the tunnel connector
   (`networkPolicy.tunnel.tokenApi`).
2. The token listener serves only its own router: the `/v1` routes in
   `TOKEN_ROUTES` (`crates/api/src/personal_tokens/mod.rs`), `GET` and
   `HEAD` only. Any other path is a `404`, any other method a `405`.
3. `Cookie`, `X-Real-IP` and `X-Forwarded-*` are removed. No cookie is a
   credential here.
4. The gate parses the token (prefix, length, base62, CRC32 checksum),
   looks it up by SHA-256, compares the hash in constant time, and refuses
   it if it is revoked, expired or unused for 90 days (an idle token is
   revoked on the spot). It then checks the route's scope.
5. The rate buckets: per token, per token on the train routes, per user,
   all tokens together, and failed authentications per client address
   (`CF-Connecting-IP` when `trustCfConnectingIp`).
6. The handler runs (the same handler as the website's route), and the
   response is wrapped as `{data, attribution}` with `Cache-Control:
   private, max-age=N`, an `ETag` and `DS-Poll-Interval: 60`.

On the website's listener and the delegated listener, any `dsp_` bearer is
answered `401 invalid_token` before anything else reads it, and logged
`event=personal_token_rejected reason=wrong_listener`. The frontend's `/api`
proxy never forwards `Authorization` in the first place.

## Turning it on

Production waited for the token-creation notice (spec §15, decision 3),
which has now shipped: the notifier sends one "New API token created" web
push per new token to each of the user's devices, whatever their quiet
hours (`personal_api_tokens.push_notified_at`, in the notifier's
`grant_notice` cycle), and every page shows a banner for 7 days naming the
token, with a link to revoke it. Then:

1. DS chart values (Ranma-Config, `clusters/mine-bringer/apps/distant-signal.yaml`):

   ```yaml
   api:
     personalTokens:
       enabled: true
       publicBaseUrl: https://ds-api.cursed.solutions
       trustCfConnectingIp: true
   networkPolicy:
     tunnel:
       tokenApi: true
   ```

   `networkPolicy.tunnel.api` stays `false` and `api.rateLimit.trustXRealIp`
   stays `true`. The render fails if `trustCfConnectingIp` is on without
   `networkPolicy.enabled`, `networkPolicy.tunnel.enabled` and
   `networkPolicy.tunnel.tokenApi`.
2. cloudflared's egress policy (`netpol-cloudflared.yaml`): allow the api
   pods (`app.kubernetes.io/instance: distant-signal`,
   `app.kubernetes.io/component: api`) on TCP 8083.
3. Last, the tunnel's public hostname in the Cloudflare dashboard:
   `ds-api.cursed.solutions` to
   `http://distant-signal-api-tokens.distant-signal.svc.cluster.local:8083`.
4. Check:
   - `curl https://ds-api.cursed.solutions/v1/status` answers `200`.
   - A token created on the page gets `200` from `/v1/me`.
   - A token without `journeys:read` gets `403` from `/v1/Journeys/mine`.
   - The same token sent to `https://ds.cursed.solutions/api/...` gets
     `401`.
   - After Revoke, the next request gets `401`.
   - Loki has the `personal_token_*` audit lines.

The migration `20261103120000_personal_api_tokens` is expand-only. With
per-service roles on, re-run the role setup so the api gets its grants on
`personal_api_tokens`.

## Users' side

`/account/connected-apps` lists the user's working tokens (name, `dsp_...`
and the last 4 characters, scopes, created, expires, last used) with
Revoke and Revoke all, and a form to create one: a name, scopes and a
lifetime of 7, 30, 90 (default) or 365 days. The token is shown once. At
most 10 working tokens per user (`maxActivePerUser`).

| Scope | Routes |
|---|---|
| `journeys:read` | `/v1/Journeys/mine`, `/v1/Journeys/{journeyId}` (the user's own journeys only) |
| `lines:read` | `/v1/Line/Mode/{mode}/Status`, `/v1/Line/{ids}/Status`, `/v1/StopPoint/{crs}/Disruption` |
| `trains:read` | `/v1/Train/by-uid/{trainUid}/{date}`, `/v1/stations/{crs}/departures` |
| `preferences:read` | `/v1/preferences`, `/v1/preferences/notifications` |
| any | `/v1/me` |

## OpenAPI document

`GET /v1/openapi.json` (no token) serves an OpenAPI 3.1 document of the
`/v1` routes, generated from the code (`personal_tokens::openapi`: the
paths from `TOKEN_ROUTES`, the response schemas derived by `schemars` from
the handlers' own types). The committed copy is `api-types/openapi/v1.json`;
regenerate it with `UPDATE_OPENAPI=1 cargo test -p api
openapi_document_is_current`. CI's repo-checks job runs Distant-Signal/ci's
`openapi-breaking` action (oasdiff) against the base commit and fails on a
breaking change: v1 only grows. A breaking change needs a `/v2` document.

## Recipes

Step-by-step guides for using a token from things people already run:

- [Home Assistant](recipes/home-assistant.md): line status, the next
  journey and a train's delay as REST sensors, with the credits on a
  dashboard.
- [Apple Shortcuts](recipes/apple-shortcuts.md): "how's my line" and "my
  next train" with Siri.

## Revocation and the kill switch

| Trigger | Effect |
|---|---|
| Revoke, or Revoke all, on the page | Revoked (`user`) |
| Admin `POST /api/admin/users/revoke-sessions` | Every token of that user revoked (`admin`), with the sessions and grants |
| 90 days unused | Revoked (`inactive`) on the next request or by the hourly sweep |
| Expiry | Refused from `expires_at`; the sweep marks it `expired` |
| Account deletion | Rows cascade |
| Authentik logout, back-channel logout, "log out other sessions" | Kept (spec §15, decision 2) |

In increasing order of reach:

1. Revoke every token at once:

   ```sql
   UPDATE personal_api_tokens SET revoked_at = now(), revoked_reason = 'admin'
   WHERE revoked_at IS NULL;
   ```

2. Remove the tunnel's public hostname in the Cloudflare dashboard. Every
   token request then gets a 404 at the edge.
3. `api.personalTokens.enabled: false` and deploy: no listener, no Service,
   no rule, no page section. Rows are kept, so turning it back on restores
   the tokens that have not expired. The sweep does not run while it is off.

## Limits

Per pod: per token 30 a minute (burst 10); per token on the train routes 10
a minute (burst 5); per user 60 a minute (burst 20); all tokens 1200 a
minute (burst 200); failed authentications per client address 10 a minute
(burst 10). Over a limit: `429 rate_limited` with `Retry-After`. Values:
`api.personalTokens.rateLimit`.

## Retention

The hourly maintenance pass (the api loop, or the `api-maintenance`
CronJob), only while the feature is on, marks expired tokens `expired`,
revokes tokens unused for `idleDays` (90) and deletes rows that ended more
than `revokedRetentionDays` (365) ago. See
[personal-data-retention.md](personal-data-retention.md). The account
export lists every token without its hash.

## Observability

- Audit log lines (`audit=true`): `personal_token_created`,
  `personal_token_revoked`, `personal_token_request` (token id, user, route
  template, status, scopes) and `personal_token_rejected` (reason, token id
  when known, a hash of the client address). Tokens are never logged; a test
  (`tokens_never_reach_the_logs`) checks it.
- Metrics: `distant_signal_api_personal_token_requests_total{route,status}`,
  `distant_signal_api_personal_token_rejected_total{reason}`,
  `distant_signal_api_personal_tokens_active`, and
  `distant_signal_api_rate_limited_total{class="personal_token",caller}`.
