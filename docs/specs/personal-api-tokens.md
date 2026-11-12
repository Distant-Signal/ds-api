# Read-only personal API tokens on a separate API hostname

Status: accepted (decisions in §15, 2026-11-03). Task DS-286. Written 2026-11-03. This is item 2 of the
integrations roadmap.

A Distant Signal (DS) user creates a **personal API token** on the
Connected apps page and pastes it into something they run themselves, such
as Home Assistant, an Apple Shortcut, an e-ink board or a script. That
thing then reads the user's journeys, line status, trains and settings from
a **separate API hostname**. Personal tokens are read-only: no route on the
token API changes anything.

The design reuses the delegated-access building blocks
(docs/connected-apps.md): a listener of its own, a route allowlist, scopes,
idle expiry, a kill switch and per-caller rate buckets. Decision D-14 is
relaxed for this one class of credential (§4).

## Contents

1. Summary
2. Tokens
3. The API hostname and listener
4. Decision D-14, amended
5. Routes, responses and attribution
6. Abuse limits and caching
7. Management on the Connected apps page
8. Audit, revocation and the kill switch
9. Privacy
10. Ranma-Config needs
11. Migration and rollout
12. OpenAPI
13. Threat model
14. Implementation plan
15. Decisions (2026-11-03)

## 1. Summary

- **Token:** `dsp_` followed by 32 random base62 characters (about 190 bits)
  and a 6-character CRC32 checksum. It is stored only as its SHA-256 hash,
  has a name, scopes, an expiry (at most 365 days), a last-used time and a
  90-day idle expiry. It is shown once, when it is created.
- **Scopes:** `journeys:read`, `lines:read`, `trains:read` and
  `preferences:read`. There are no write scopes.
- **Where it works:** only on a new **token listener** in the api (port
  8083). That listener serves an allowlisted `/v1` router and nothing else.
  Cloudflare tunnel publishes it as `ds-api.cursed.solutions` (§3.1 explains
  why that name and not `api.ds.cursed.solutions`). The browser path is
  unchanged: the frontend's `/api` proxy still never forwards
  `Authorization`, and the main api listener refuses `dsp_` tokens.
- **Limits:** per-token and per-user rate buckets, a lower bucket for the
  train routes, a pod-wide ceiling, and a per-IP bucket for failed
  authentication. Responses carry `Cache-Control: private`, an `ETag` and a
  60-second poll hint.
- **Off by default:** `api.personalTokens.enabled: false`. With it off, the
  api binds no token listener, the chart renders no token Service and no
  NetworkPolicy rule, the management routes answer `404`, and the Connected
  apps page shows no token section.

## 2. Tokens

### 2.1 Format

```
dsp_<32 base62 characters: random><6 base62 characters: CRC32 of the random part>
```

- **Random part:** 32 characters, each drawn uniformly from
  `[0-9A-Za-z]` by rejection sampling from the OS RNG (`rand::rngs::OsRng`).
  That gives 32 × log2(62), about 190 bits, above the required 128.
- **Checksum:** the CRC32 (IEEE) of the random part, as 6 base62 characters,
  left-padded with `0`. It is not a security control. It lets secret
  scanners recognise a real token without calling DS, and it lets the api
  reject a mistyped or truncated token before any database lookup.
- **Prefix:** `dsp_` ("DS personal") is distinctive and fixed, so GitHub
  secret scanning (custom patterns, and a partner registration later),
  gitleaks and trufflehog can match `\bdsp_[0-9A-Za-z]{38}\b`. The total
  length is 42 characters. It has no `-` or `_` after the prefix, so it
  selects with a double click.
- **Display hint:** the last 4 characters (all checksum), shown as
  `dsp_…AbC1` so a user can tell their tokens apart. Showing checksum
  characters reveals nothing about the random part.

### 2.2 Storage

- `token_hash` is `SHA-256(the whole token string)`, 32 bytes (`bytea`,
  `UNIQUE`). A fast hash is right for a 190-bit random secret: a slow
  password hash defends low-entropy input, which this is not.
- There is no pepper. A database dump alone already cannot be replayed (the
  hash is not the token), and a pepper would add a secret to rotate without
  changing that. This matches `sessions.id` and `unlisted_links.token_hash`.
- **Lookup and comparison:** the api computes the hash and selects the row
  by it through the unique index. It then compares the stored hash with the
  computed one in constant time (`subtle::ConstantTimeEq`). An equality
  lookup on the hash of a high-entropy secret leaks nothing useful through
  timing, but the explicit comparison makes the property obvious and
  testable.
- The plaintext exists only in the `POST` response that creates the token.
  It is never logged, stored, put in a URL, or sent to the notifier.

### 2.3 Lifetime

| Property | Rule |
|---|---|
| Expiry | Chosen at creation: 7, 30, 90 (default) or 365 days. No "never". `api.personalTokens.maxLifetimeDays` (365) caps it |
| Idle expiry | Revoked (`inactive`) when unused for 90 days (`idleDays`), the same rule as delegated grants (D-6). Checked on use and by the hourly maintenance pass |
| Last used | `last_used_at` is updated at most once a minute per token (`... WHERE last_used_at IS NULL OR last_used_at < now() - interval '1 minute'`), so polling does not write on every request |
| Per user | At most 10 active tokens (`maxActivePerUser`). Creating an 11th answers `409 token_limit` |
| Name | 1 to 60 characters, required, and not unique |
| Scopes | A non-empty subset of the four scopes, fixed at creation. To change them, create a new token |

### 2.4 Scopes

| Scope | Lets the token read |
|---|---|
| `journeys:read` | The user's journeys and their legs (`GET /v1/Journeys/mine`, `GET /v1/Journeys/{id}`) |
| `lines:read` | Line status and station disruption (catalogue lines; the user's custom lines are not included in v1) |
| `trains:read` | A train by UID and date, and live station departures |
| `preferences:read` | Pinned lines, stations and operators, and notification settings |

`lines:read` and `trains:read` cover data that is public on the website.
They are still scoped so a user can give a display only what it needs, and
so that token traffic to them is rate limited per token.

### 2.5 Token scopes and delegated scopes

Personal token scopes are a separate enum (`PersonalScope`) from delegated
`Scope`. `journeys:read` and `preferences:read` are spelled the same in
both. A delegated client never accepts a personal token, and the token
listener never accepts a delegated token.

## 3. The API hostname and listener

### 3.1 Hostname

**Recommended: `ds-api.cursed.solutions`.** It follows `ds-mcp.cursed.solutions`.
Cloudflare's free Universal SSL certificate covers `cursed.solutions` and
`*.cursed.solutions` only. A second-level name such as
`api.ds.cursed.solutions` would need Advanced Certificate Manager, a paid
add-on, or it would fail TLS. The chart takes the public base URL as a value
(`api.personalTokens.publicBaseUrl`), so the name is Ranma-Config's choice
(decision 1 in §15).

### 3.2 Why a listener in the api, not a route in the frontend

Production has no Ingress. The Cloudflare tunnel sends `ds.cursed.solutions`
to the frontend Service only. Two shapes were considered:

**(a) A dedicated listener in the api (chosen).** The api binds a third
listener, next to the public one (8080) and the delegated one (8082). It
serves a small router that contains only the `/v1` token routes (§5),
built from the same handlers. A new ClusterIP Service publishes it, and the
tunnel routes the new hostname to that Service.

**(b) A token gateway in the frontend.** The frontend would accept
`Authorization`, call the api with it and pass the result back.

(a) is chosen because:

1. **The browser proxy stays exactly as it is.** (b) makes the frontend
   handle bearer credentials, the thing the proxy was written never to do,
   and puts the "forward `Authorization` or not" decision in the same
   process as the cookie path.
2. **The route allowlist is structural.** The listener's router holds only
   the token routes, so a `/Train/track` or a `/public/admin/...` request
   is a plain 404 there. It is not a check that could be forgotten. Pointing
   the tunnel at the whole api instead would publish every root-mounted
   route and the whole `/public/*` tree.
3. **Client IPs stay correct on both paths.** The chart refuses
   `networkPolicy.tunnel.api: true` while `api.rateLimit.trustXRealIp` is
   on, because a request straight from the tunnel could pick its own
   `X-Real-IP`. A separate listener leaves that guard and the main
   listener's per-IP limits untouched. The token listener never reads
   `X-Real-IP`. Its buckets key on the token and the user (§6), so they
   don't depend on the IP at all. The one IP-keyed bucket, for failed
   authentication, uses `CF-Connecting-IP` only when
   `trustCfConnectingIp` is on (§6.2).
4. **No new hop or process.** (b) adds a hop and a second implementation of
   token checks in TypeScript.

### 3.3 How the listener works

- `PERSONAL_TOKENS_ENABLED=true` and `PERSONAL_TOKENS_LISTEN_ADDR`
  (`0.0.0.0:8083`) bind it. With the flag off it is not bound.
- **The token router** (`crate::personal_tokens::router`):
  - It nests the allowlisted handlers under `/v1` (§5).
  - Outermost first, its layers are: request tracing; `strip_ambient_credentials`,
    which removes `Cookie`, `X-Real-IP` and `X-Forwarded-*` from the
    request; `token_gate` (§3.4); the token rate limiter (§6); the
    response envelope and caching layer (§5.2, §6.3); and the public
    timeout and unavailable layers that the main router uses.
  - It has **no** CORS layer and **no** Origin check. No cookie can reach
    it, so there is nothing for an Origin check to protect.
  - Only `GET` and `HEAD` are routed. Anything else is a `405`.
- **The router is separate, not a copy.** The delegated listener serves a
  marked copy of the whole main router. The token listener does not: it
  serves its own router. A route is reachable through a token only when it
  is listed in `TOKEN_ROUTES` and mounted in `personal_tokens::router`. A
  test pins the two lists together, as `DELEGATED_ROUTES` is pinned.
- **The main listener refuses `dsp_` tokens.** `delegated::gate` already
  routes bearers. It gains one rule: any bearer that starts with `dsp_`,
  arriving on the public or delegated listener, is answered `401
  invalid_token` and logged `event=personal_token_rejected
  reason=wrong_listener`. It is never looked up. That way a token pasted
  into the website's own origin, or sent to the in-cluster port, fails
  loudly rather than falling through to the anonymous path.
- **The frontend proxy is unchanged** and still never forwards
  `Authorization`. A test in `frontend/app/api/[...path]/route.test.ts`
  already pins that and stays as it is.

### 3.4 `token_gate`

Every request on the token listener passes through it, in this order:

1. `GET /v1/status` (unauthenticated, `{"ok":true}`, for the blackbox
   probe) passes without a token. Every other route needs one.
2. It reads `Authorization: Bearer <token>`. There is no query-string or
   cookie form. A missing token or another scheme gets `401
   invalid_token` with `WWW-Authenticate: Bearer`.
3. It parses the token: the prefix, the length, the base62 alphabet and the
   checksum. A failure gets the same `401` and a charge to the
   failed-authentication bucket, with no database work.
4. It hashes the token and looks it up. The row must exist and not be
   revoked, expired or idle for 90 days. An idle token is revoked
   (`inactive`) on the spot. Any failure is the same `401` (the reason is
   logged, not returned) and a charge to the failed-authentication bucket.
5. It checks the route's scope from `TOKEN_ROUTES` against the token's
   scopes. A missing scope gets `403 insufficient_scope`, with
   `WWW-Authenticate: Bearer error="insufficient_scope", scope="..."`.
6. It inserts a `PersonalTokenAuth { token_id, user_id, scopes,
   route_scope }` extension, touches `last_used_at` (rate-limited, as in
   §2.3), runs the request, and logs the audit line (§8.1).

**Handlers that read user data** (journeys and preferences) use the existing
`DelegatedOrSessionUser<S>` extractor. It gains a third source,
`Via::PersonalToken { token_id }`, taken from the `PersonalTokenAuth`
extension. It maps `S::SCOPE` to a personal scope and checks it again.
Checking twice is deliberate: the extractor is the last line of defence if a
route is ever mounted without its table entry. On the token listener,
the cookie fallback can never apply, because `strip_ambient_credentials`
removed the cookie and `token_gate` refuses a request with no token.

`GET /public/preferences` (pinned lines, stations and operators) moves from
`AuthenticatedUser` to `DelegatedOrSessionUser<PreferencesReadScope>`. It is
not added to `DELEGATED_ROUTES`, so delegated clients still cannot reach it.

## 4. Decision D-14, amended

The original decision (delegated-access spec, §0a, third round):

> **D-14, Public ingress.** Delegated tokens are **refused** when the api is
> reached through the public tunnel or ingress, and accepted only from the
> in-cluster ds-mcp. DS tells the two apart by the listener a request arrived
> on, never by a header.

**Amended text (2026-11-03, the user's decision):**

> **D-14 (amended 2026-11-03), Public ingress.** Delegated tokens (a
> registered client acting for a user) are still **refused** on every
> public path and accepted only on the in-cluster delegated listener.
> **One exception:** read-only **personal API tokens** (`dsp_…`), created by
> a user for themselves on the Connected apps page, are accepted from the
> public internet, under all of these conditions:
>
> 1. Only on the dedicated token listener, published at the API hostname.
> 2. Only on the `/v1` read routes listed in `TOKEN_ROUTES`, and only within
>    the token's scopes. No personal-token scope allows a write.
> 3. Each is hashed at rest, expires, is rate limited per token, and can be
>    revoked by the user, by an administrator, and globally (§8).
>
> No other credential is accepted on that listener. A personal token is
> refused on every other listener. The browser `/api` proxy never forwards
> `Authorization`. As before, DS tells the paths apart by the listener a
> request arrived on, never by a header. Any further relaxation, such as
> write scopes, third-party OAuth clients or tokens on the browser origin,
> needs a new decision.

The amendment is recorded in this spec. The delegated-access spec's
decision table gets a one-line "Amended 2026-11-03, see
docs/specs/personal-api-tokens.md §4" note, and docs/connected-apps.md
points here.

## 5. Routes, responses and attribution

### 5.1 `TOKEN_ROUTES` (v1)

Every route is `GET` (and `HEAD`), nested under `/v1`, with the path and
query parameters of the existing handler it reuses.

| Path | Scope | Handler | `max-age` |
|---|---|---|---|
| `/v1/status` | none | new, `{"ok":true}` | 60 |
| `/v1/me` | any | new: the token's name, scopes, `createdAt` and `expiresAt` | 0 (`no-store`) |
| `/v1/Journeys/mine` | `journeys:read` | `journeys::get_my_journeys` | 30 |
| `/v1/Journeys/{journey_id}` | `journeys:read` | `journeys::get_journey` | 30 |
| `/v1/Line/Mode/{mode}/Status` | `lines:read` | `line_status::get_mode_status` | 60 |
| `/v1/Line/{ids}/Status` | `lines:read` | `line_status::get_line_status` | 60 |
| `/v1/StopPoint/{crs}/Disruption` | `lines:read` | `line_status::get_stop_point_disruption` | 60 |
| `/v1/Train/by-uid/{train_uid}/{date}` | `trains:read` | `train::get_by_uid_and_date` | 30 |
| `/v1/stations/{crs}/departures` | `trains:read` | `departures` (`/public/stations/{crs}/departures`) | 30 |
| `/v1/preferences` | `preferences:read` | `preferences::get_preferences` | 300 |
| `/v1/preferences/notifications` | `preferences:read` | `preferences::get_notification_preferences` | 300 |

The line handlers take `OptionalAuthenticatedUser`. On this listener it
is always anonymous, because the cookie was stripped, so token callers see
catalogue lines only. Custom lines can come in a later version with
`lines:read`.

The path stays TfL-shaped like the rest of the api, so a client can move
between the website's API and this one. `/v1` is the version: a breaking
change to any of these shapes needs `/v2` (§12).

### 5.2 The response envelope and `attribution`

A `2xx` JSON response on the token listener is wrapped:

```json
{
  "data": <the handler's body, unchanged>,
  "attribution": {
    "sources": [
      {"name": "National Rail", "notice": "powered by NationalRail (Train Information Services Ltd)", "url": "https://www.nationalrail.co.uk"},
      {"name": "Network Rail", "notice": "Contains Information of Network Rail Infrastructure Limited licensed under the following licence", "url": "<the NRIL licence page>"},
      {"name": "RSP", "notice": "Source: RSP"},
      {"name": "Transport for London", "notice": "Powered by TfL Open Data", "url": "<TfL's transport data terms>"}
    ],
    "statement": "Distant Signal is an independent, unofficial service and is not affiliated with or endorsed by ..."
  }
}
```

- **Which sources:** each route lists the sources its data can come from,
  as a static table next to `TOKEN_ROUTES`. Lines use RDG Knowledgebase,
  TfL and the TOC feeds. Trains use Darwin, Network Rail TRUST, the RSP
  timetable and KB Stations. Journeys use all of those. Preferences carry
  no third-party data, so they get an empty `sources` list.
- **The notice text** is the credits DS already carries (the footer and
  `/privacy`; the full list is in the integrations research, "Licensing and
  privacy flags"), so there is one source of truth on each side: a Rust constant table
  in `crate::personal_tokens`, which a test checks against the frontend's
  credits components. `statement` is the website's non-affiliation
  statement.
- **Errors** (`4xx` and `5xx`) are not wrapped. They use the delegated error
  contract, `{"error": "...", "message": "..."}`.
- **Size:** the wrapping middleware buffers the body up to 4 MiB. Anything
  larger (none of these routes comes close) gets a `500` and a log line,
  rather than a truncated body.

Wrapping is done in one middleware rather than in each handler, so the
handlers stay shared with the website unchanged. The envelope also gives v1
room for later top-level fields, such as `meta`.

## 6. Abuse limits and caching

### 6.1 Rate buckets

These use the GCRA algorithm of `crate::rate_limit`, through a small
keyed table of its own (`KeyedLimiter`), which charges every bucket that
applies or none. State is in memory, per pod, like every other bucket.

| Bucket | Key | Default | Value |
|---|---|---|---|
| Token reads | `ptok:<token id>` | 30/min, burst 10 | `api.personalTokens.rateLimit.token` |
| Token train reads (`/v1/Train/*`, `/v1/stations/*`; also charged to the token bucket) | `ptok:<token id>` | 10/min, burst 5 | `...rateLimit.tokenTrains` |
| User, across all their tokens | `puser:<user id digest>` | 60/min, burst 20 | `...rateLimit.user` |
| Every token together (per pod) | `ptok:all` | 1200/min, burst 200 | `...rateLimit.global` |
| Failed authentication | the client IP (§6.2) | 10/min, burst 10 | `...rateLimit.authFailure` |

- A request must pass every bucket that applies. A refusal gets `429
  rate_limited` with `Retry-After`, in the delegated error contract.
- Failed authentication is charged **after** a failure, and the bucket is
  also checked **before** the lookup. A client over its failure budget gets
  `429` without any database work.
- Metrics: `distant_signal_api_rate_limited_total{class="personal_token", caller}`,
  with callers `personal_token` (any of the token, user, train and
  pod-wide buckets) and `personal_auth_failure`.

### 6.2 Client IP on the token listener

- The socket peer is always a cloudflared pod.
- With `api.personalTokens.trustCfConnectingIp: true`, the failure bucket
  keys on `CF-Connecting-IP`. Cloudflare's edge sets that header and
  overwrites any value the client sent. IPv6 addresses are keyed by their
  /64, as in the main limiter.
- Trusting the header is safe only while cloudflared is the only thing that
  can reach port 8083. The chart's NetworkPolicy rule for the port admits
  only `networkPolicy.tunnel` pods (§10). The chart refuses to render
  `trustCfConnectingIp: true` unless `networkPolicy.enabled` and
  `networkPolicy.tunnel.tokenApi` are both on.
- With the flag off, the bucket keys on the peer. That is one shared bucket
  for all failures, which is acceptable as a fallback, and it is counted.

### 6.3 Caching headers and the minimum poll interval

On every `2xx`:

- `Cache-Control: private, max-age=<per route, §5.1>`. It is `private` so
  Cloudflare and shared caches never store a user's data. Cloudflare does
  not cache `private` responses.
- `ETag: W/"<first 16 hex characters of SHA-256 of the envelope>"`.
- `If-None-Match` matching the ETag gets a `304` with no body. The handler
  still runs, so a `304` still costs a request against the budget, but it
  saves bandwidth for the e-ink and ESP32 clients.
- `Vary: Authorization`, `X-Content-Type-Options: nosniff`.
- `DS-Poll-Interval: 60`, a hint in seconds. The recipe docs say to poll no
  more than once a minute.

The **minimum poll interval** is enforced by the per-token bucket, not by
a per-path timer. 30 a minute with a burst of 10 lets a dashboard read
about five endpoints a minute with room to spare. A client polling one
endpoint every 2 seconds is refused within a minute. Home Assistant's
default 30-second scan of five sensors fits within the budget.

## 7. Management on the Connected apps page

### 7.1 api routes (session cookie only)

These are under `/public/account/personal-tokens`, through the existing
frontend proxy. They take `AuthenticatedUser`, the session cookie, and are
covered by the existing same-origin check on cookie mutations. The `/v1`
listener never mounts them, so a token cannot create or revoke tokens.

| Route | Does |
|---|---|
| `GET /public/account/personal-tokens` | `{enabled, apiBaseUrl, scopes: [...], lifetimes: [7,30,90,365], tokens: [{id, name, hint, scopes, createdAt, expiresAt, lastUsedAt}]}`, for active tokens only. With the feature off: `404 feature_disabled` |
| `POST /public/account/personal-tokens` `{name, scopes, lifetimeDays}` | Creates a token: `201 {token, id, name, hint, scopes, createdAt, expiresAt}`. The plaintext `token` appears only here, with `Cache-Control: no-store`. `400 invalid_request`, `409 token_limit` |
| `DELETE /public/account/personal-tokens/{id}` | Revokes it (`user`). Idempotent `204`, scoped to the caller's own tokens |
| `DELETE /public/account/personal-tokens` | Revokes all of the user's tokens (`user`). `204` |

The `POST` route is in the existing `PublicWrite` limit class.

### 7.2 UI

There is a new "Personal API tokens" section under the connected apps list.
It is rendered only when the `GET` route answers `enabled: true`.

- **The list:** each token shows its name, `dsp_…hint`, its scopes as
  plain-language labels, when it was created, when it expires, and when it
  was last used (or "Never used"). Each has a **Revoke** button with an
  inline confirmation, not `window.confirm`, which artifacts and some
  browsers block. "Revoke all" appears when there are two or more.
- **The create form:** a name, scope checkboxes (none ticked by default;
  each has a one-line description), and an expiry select (default 90 days).
- **After creation:** the token is shown once in a read-only field with a
  Copy button. Next to it: "Copy it now. You won't see it again.", the
  API's base URL, and a link to the recipes doc. Leaving or reloading the
  page drops it.
- **Copy rules:** sentence case; failures use `describeFailure` ("Couldn't
  create the token."); no internal vocabulary. The word is "token", never
  "PAT".
- The 7-day "connected" banner and push notice that grants have
  (`ConnectedAppNotice`) are reused for token creation in a later PR (§14,
  PR 6). Production waits for it (§15 decision 3).

## 8. Audit, revocation and the kill switch

### 8.1 Audit log lines (`audit=true`)

None of these lines carries the token, its hash, or a request body.

| Event | Fields |
|---|---|
| `personal_token_created` | `user_id`, `token_id`, `scopes`, `expires_at`, `session_hash` (`log_hash` of the session id) |
| `personal_token_revoked` | `user_id`, `token_id`, `reason` |
| `personal_token_request` | `token_id`, `user_id`, `method`, `route` (the matched template, never the raw path), `status`, `scopes` |
| `personal_token_rejected` | `reason` (`missing`, `malformed`, `unknown`, `revoked`, `expired`, `inactive`, `insufficient_scope`, `wrong_listener`, `rate_limited_auth`), `token_id` when one was found, and the `log_hash` of the client IP |

### 8.2 Metrics

- `distant_signal_api_personal_token_requests_total{route,status}`
- `distant_signal_api_personal_token_rejected_total{reason}`
- `distant_signal_api_personal_tokens_active`, a gauge set by the hourly
  sweep
- The `personal_*` callers on `distant_signal_api_rate_limited_total`

### 8.3 Revocation

| Trigger | Effect |
|---|---|
| Revoke, or Revoke all, on the page | Revoked (`user`) |
| Admin `POST /public/admin/users/revoke-sessions` | Every token of that user revoked (`admin`), along with the sessions and grants |
| 90 days unused | Revoked (`inactive`) on the next request or by the hourly sweep |
| Expiry | Refused from `expires_at`. The sweep marks it `expired` |
| Account deletion | Rows cascade |
| Authentik logout, back-channel logout, "log out other sessions" | **Kept** (§15 decision 2) |

Tokens survive the user's logouts on purpose. A token exists to keep a
device working for months. Ending a browser session should not break the
user's Home Assistant. A suspected compromise is handled with Revoke on
the page, or by an admin with revoke-sessions.

### 8.4 Kill switch, in increasing order of reach

1. **Revoke every token** (SQL, at once, from any replica):
   ```sql
   UPDATE personal_api_tokens SET revoked_at = now(), revoked_reason = 'admin'
   WHERE revoked_at IS NULL;
   ```
2. **Cut the hostname at Cloudflare.** Remove the tunnel's public hostname
   in the dashboard. Every token request then gets a 404 from the edge
   within seconds. Nothing in the cluster changes.
3. **Turn the feature off.** `api.personalTokens.enabled: false` and deploy.
   The listener is not bound, the Service and the NetworkPolicy rule are
   not rendered, and the management routes and the page section disappear.
   Existing rows are kept, so turning it back on restores the tokens that
   have not expired.

docs/personal-api-tokens.md, the operator guide written with the
implementation PR, carries this list.

### 8.5 Retention

The hourly maintenance pass (the api loop or the `api-maintenance`
CronJob) marks expired tokens `expired`, revokes idle ones, and deletes
rows 365 days after their revocation or expiry
(`api.personalTokens.revokedRetentionDays`). docs/personal-data-retention.md
gains a row. The account export includes each token's name, hint, scopes,
and created, expiry, last-used and revoked times. It never includes the
hash.

## 9. Privacy

`/privacy` gets a new section after "Connecting your AI assistant". The
copy follows docs/style-guide.md, "Writing":

> **Personal API tokens**
>
> This is optional. You can create a personal API token in Connected apps
> on your account page, to let something you run yourself, such as a home
> dashboard or a phone shortcut, read your Distant Signal data. A token can
> only read. It can't change or delete anything. You choose what it can
> read (your journeys, line status, trains, or your settings) and when it
> expires, up to a year.
>
> Anyone who has the token can read what it allows, so keep it private. We
> store only a scrambled form of it, so we can't show it to you again. If
> it's lost or leaked, revoke it in Connected apps. That takes effect
> straight away. A token also stops working after 90 days without use.
>
> Where the token's data goes after that depends on what you put it in.
> That app or device isn't run by us.
>
> We log each request made with a token (your account, which token, the
> time, what it asked for and whether it worked) and keep those logs for
> the same time as our other request logs. After a token is revoked or
> expires, we keep a record of it (its name, what it could read, and when it
> was created, last used and ended) for a year, then delete it.

The "what we store" table gets a row: "Personal API tokens: the name,
permissions, a scrambled form of the token, and when it was created, last
used, expires or was revoked."

The section ships with the frontend PR, behind the same flag. The page
renders it only when the feature is on. `scripts/check-copy.py` must pass.

## 10. Ranma-Config needs

These are the exact changes, for the session that owns Ranma-Config. The
release is `distant-signal` in the `distant-signal` namespace, so the
chart's new Service is `distant-signal-api-tokens`, port 8083.

1. **Cloudflare tunnel public hostname.** This is set in the Cloudflare
   Zero Trust dashboard (Networks > Tunnels > the tunnel > Public
   Hostname), not in git:

   | Hostname | Service |
   |---|---|
   | `ds-api.cursed.solutions` | `http://distant-signal-api-tokens.distant-signal.svc.cluster.local:8083` |

   Also update the hostname list in the comment block of
   `clusters/base/apps/cloudflared.yaml`.
2. **cloudflared egress** (`clusters/<cluster>/apps/netpol-cloudflared.yaml`,
   policy `cloudflared-egress`). Add a rule for the api pods' token port.
   kube-router checks egress after the Service DNAT, so it is the pod port:

   ```yaml
   # distant-signal-api-tokens:8083 -> api pods :8083
   # (ds-api.cursed.solutions, personal API tokens; DS
   # docs/specs/personal-api-tokens.md). Matching ingress rule: the
   # chart's distant-signal-api policy (networkPolicy.tunnel.tokenApi).
   - to:
       - namespaceSelector:
           matchLabels:
             kubernetes.io/metadata.name: distant-signal
         podSelector:
           matchLabels:
             app.kubernetes.io/instance: distant-signal
             app.kubernetes.io/component: api
     ports:
       - protocol: TCP
         port: 8083
   ```
3. **DS values** (`clusters/<cluster>/apps/distant-signal.yaml`), once
   the chart version with the feature is deployed:

   ```yaml
   api:
     personalTokens:
       enabled: true
       publicBaseUrl: https://ds-api.cursed.solutions
       trustCfConnectingIp: true
   networkPolicy:
     tunnel:
       tokenApi: true      # admits cloudflared to the api pods' port 8083 only
   ```

   `networkPolicy.tunnel.api` stays `false`, and `api.rateLimit.trustXRealIp`
   stays `true`. The ingress rule is rendered by the chart, so
   `netpol-distant-signal.yaml` needs no new policy. Only its header
   comment changes, from "nothing public routes to the api" to "nothing
   public routes to the api's main port; port 8083 (personal tokens) is
   admitted from cloudflared by the chart".
4. **No new Service, Ingress or Secret in Ranma-Config.** The chart owns the
   Service. Tokens need no secret.
5. **Optional, recommended:** a blackbox probe of
   `https://ds-api.cursed.solutions/v1/status` (expect `200`) in
   `monitoring-blackbox.yaml`. Also a Cloudflare rate-limiting rule on the
   hostname (the free plan allows one), such as 120 requests a minute per
   IP, as an outer bound in front of the api's own buckets.

The order is: the DS chart with the feature (off) is deployed, then items
2 and 3 in one Ranma-Config change, then item 1. The hostname comes last so
it never points at a port that is not open yet.

## 11. Migration and rollout

### 11.1 Migration

`crates/ds-store/migrations/<timestamp>_personal_api_tokens.sql` adds a new
table, so the change is expand-only. The timestamp must be newer than
every migration on main when the PR is opened
(`check-migration-order.py`).

```sql
SET LOCAL lock_timeout = '5s';

CREATE TABLE personal_api_tokens (
    id BIGSERIAL PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name TEXT NOT NULL CHECK (char_length(name) BETWEEN 1 AND 60),
    token_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(token_hash) = 32),
    hint TEXT NOT NULL CHECK (hint ~ '^[0-9A-Za-z]{4}$'),
    scopes TEXT[] NOT NULL CHECK (
        cardinality(scopes) BETWEEN 1 AND 4
        AND scopes <@ ARRAY['journeys:read','lines:read','trains:read','preferences:read']
    ),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL,
    last_used_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    revoked_reason TEXT CHECK (revoked_reason IN ('user','admin','inactive','expired')),
    CHECK ((revoked_at IS NULL) = (revoked_reason IS NULL)),
    CHECK (expires_at > created_at)
);

CREATE INDEX personal_api_tokens_user_active
    ON personal_api_tokens (user_id) WHERE revoked_at IS NULL;
CREATE INDEX personal_api_tokens_active_expires_at
    ON personal_api_tokens (expires_at) WHERE revoked_at IS NULL;
CREATE INDEX personal_api_tokens_revoked_at
    ON personal_api_tokens (revoked_at) WHERE revoked_at IS NOT NULL;
```

- **Locks:** the only lock on an existing table is the `SHARE ROW
  EXCLUSIVE` the foreign key takes on `users`. It is brief, behind
  `lock_timeout`. The `delegated_grants` migration did the same.
- **No `NOT VALID` step:** every constraint is on the new, empty table.
  The `NOT VALID`-then-`VALIDATE` rule applies to constraints added to
  existing tables, and this migration adds none. So there is no second,
  validating migration. `migration_index_locking.rs` checks this.
- **Indexes** are on the new table, so they are plain `CREATE INDEX` and
  need no `CONCURRENTLY`.
- **Checksum lock:** the migration's line is appended to
  `crates/ds-store/tests/migration_checksums.lock` in the same PR.
- **Grants:** in `crates/ds-store/db-grants.yaml`, add
  `personal_api_tokens: {class: personal, grants: {api: SIUD}}` and its
  sequence, `personal_api_tokens_id_seq: {table: personal_api_tokens}`. Then
  run the sync script and `gen-db-grants.py render`, and `gen-db-grants.py
  check` against a migrated database. The notifier gets a column grant
  only in PR 6, when it starts sending the creation notice.
- **The schema gate** (`ds_store::schema`) is updated if it pins the
  newest migration.

### 11.2 Rollout

Each PR can be deployed on its own, and the feature is off until a
Ranma-Config change turns it on.

1. The migration is deployed and the table sits empty. No code reads it.
2. The api ships with `api.personalTokens.enabled: false`. Nothing is bound,
   and the management routes answer `404`.
3. The frontend ships. The section stays hidden while the `GET` answers
   `404`.
4. Ranma-Config turns it on (§10). Then verify:
   - `curl https://ds-api.cursed.solutions/v1/status` gives `200`.
   - Create a token on the page; `GET /v1/me` with it gives `200`.
   - `GET /v1/Journeys/mine` with a token lacking `journeys:read` gives `403`.
   - The same token sent to `https://ds.cursed.solutions/api/...` gives
     `401` (the proxy drops it, so the route sees no session).
   - Revoke the token; the next request gives `401`.
   - Check the `personal_token_*` audit lines in Loki.

## 12. OpenAPI

**As built (2026-11-03):** the schemas come from `schemars` 1 (JSON
Schema 2020-12, which OpenAPI 3.1 uses natively) rather than `utoipa`: it
reads the same serde attributes the handlers serialize with, flattening
included, so the types need only a derive next to their `ts_rs::TS`. The
document is generated by a test (like `api_types`), not a binary, and
served at `/v1/openapi.json`. The breaking-change check is
Distant-Signal/ci's reusable `openapi-breaking` action (oasdiff 1.34.0,
pinned and checksum-verified).

The read subset gets an OpenAPI 3.1 document. It can ship as its own later
PR (PR 5), because nothing at runtime depends on it.

- **Generation:** `utoipa` 5 (MIT/Apache-2.0, already allowed by
  `deny.toml`'s licence list):
  - `#[derive(ToSchema)]` goes on the response types the `/v1` routes return.
    Most already derive `ts_rs::TS`, so the two derives sit together.
  - `#[utoipa::path]` goes on thin wrappers in `personal_tokens::routes`.
  - The envelope is a generic `Envelope<T>` schema.
  - Errors share one `ErrorBody`.
  - The handlers that return `serde_json::Value` (line status) get explicit
    schemas written to match their TfL shape. A test then deserialises
    fixtures against them.
- **Publishing:** a small binary, `cargo run -p api --bin openapi`, writes
  `api-types/openapi/v1.json`, which is committed. The token listener
  serves it at `GET /v1/openapi.json`, unauthenticated, with a long
  `max-age`. The docs link to it.
- **CI:**
  - An `openapi` job regenerates the file and fails on any diff, as
    `sync-api-types` does for the TypeScript types.
  - It then runs `oasdiff breaking` (pinned release binary, checksum
    verified, in a typed stdlib Python wrapper, `scripts/check-openapi.py`)
    between main's committed spec and the PR's. It fails on any breaking
    change. The escape is a new `/vN` (a new file), never editing v1.
  - A unit test checks that every `TOKEN_ROUTES` entry has a path in the
    spec, and that every path in the spec is in the table.
- **Changelog:** docs/api-changelog.md gets an entry for v1, and one for
  every later additive change.

## 13. Threat model

| Threat | Mitigation |
|---|---|
| A token leaks (committed, pasted, logged by a third party) | `dsp_` prefix and checksum for secret scanning; read-only; scoped; expiry ≤ 365 d; idle 90 d; user revoke; per-token buckets limit the damage |
| A database dump | Only SHA-256 hashes of 190-bit secrets are stored, so they can't be replayed |
| Guessing or brute force | 190 bits; failure bucket per IP; malformed tokens rejected before the database |
| A token is used to write or to reach admin routes | The token router has only `GET` routes in `TOKEN_ROUTES`; other paths are 404 and other methods 405. The extractor re-checks the scope |
| A token is sent to the main site | The proxy drops `Authorization`. The api's `gate` refuses `dsp_` on the main and delegated listeners (`wrong_listener`) |
| Cookie confusion on the token host | `Cookie` is stripped. The token host is a different origin and DS never sets cookies for it. No CORS, so another site's JavaScript can't read responses |
| Rate-limit evasion with a spoofed IP | Token buckets key on the token and the user, not the IP. `CF-Connecting-IP` is trusted only behind the NetworkPolicy, and only for the failure bucket |
| Load from many polling clients | Per-token, per-user and pod-wide buckets; `private, max-age` and ETag; a 60-second poll hint; the Cloudflare edge rule |
| A stolen browser session mints a long-lived token | The token is listed on the page and revoke-sessions revokes it. The push notice and banner come in PR 6, and production waits for them (§15 decision 3) |
| Timing side channels | Lookup by the hash of a high-entropy token, plus a constant-time comparison of the hashes |
| Tokens in logs | No log line takes the header or the token. The trace span records the matched route, not the raw URI. A test sends a token and asserts that the captured logs don't contain it |

## 14. Implementation plan

Each PR can be deployed on its own, and each is off until §10.

1. **This spec** (docs only).
2. **Migration and grants:** the table, the lock line, `db-grants.yaml`
   and the rendered SQL. Deployable alone: an unused empty table.
3. **api:**
   - `crate::personal_tokens`: the format, the hash, `TOKEN_ROUTES`, the
     router, `token_gate`, the envelope and caching layer, and the rate
     classes.
   - `data::personal_tokens`, with the sweep.
   - The management routes.
   - `DelegatedOrSessionUser` gains `Via::PersonalToken`, and the
     `wrong_listener` rule goes into `delegated::gate`.
   - The third listener in `main.rs`.
   - Chart: values, env, `containerPort`, the `<release>-api-tokens`
     Service, the NetworkPolicy rule (`networkPolicy.tunnel.tokenApi`), the
     render guards, the README values table, and a CI render assertion.
   - Docs: docs/personal-api-tokens.md (the operator guide),
     docs/api-changelog.md, and docs/connected-apps.md's pointer.

   It ships with `enabled: false`.
4. **Frontend:** the Connected apps section, the `/privacy` section, and
   the `ts-rs` types through `sync-api-types`.
5. **OpenAPI** (§12) and the CI breaking-change check.
6. **Creation notice:** a notifier push and the 7-day banner, reusing
   `ConnectedAppNotice`, plus the notifier column grant.
7. **Recipes:** docs pages for Home Assistant REST sensors and Apple
   Shortcuts.

## 15. Decisions (2026-11-03)

The user answered the four open questions on 2026-11-03, each as
recommended.

1. **Hostname: `ds-api.cursed.solutions`.** Cloudflare's free certificate
   covers it (`api.ds.cursed.solutions` would need Advanced Certificate
   Manager), and it matches `ds-mcp.cursed.solutions`. §10 uses it.
2. **Tokens survive the user's logouts.** Authentik logout, back-channel
   logout and "log out other sessions" keep them. They end on Revoke,
   Revoke all, admin revoke-sessions, idle, expiry or account deletion.
   A separate **"Revoke all tokens"** action sits alongside "Log out other
   sessions" on the account page, so a user who wants everything ended
   has it in one place (frontend PR, §14 PR 4).
3. **Production waits for the creation notice.** Ranma-Config turns
   `api.personalTokens.enabled` on only after PR 6 (the push notice and
   the 7-day banner on token creation) has shipped. Testing with the
   owner's own account before then is fine.
4. **No CORS in v1.** The token listener sends no
   `Access-Control-Allow-*` headers. A browser integration that needs it
   is a later, separate decision.
