# Test credentials: operator guide

Test credentials let an automated check (a smoke test, a synthetic probe)
read Distant Signal's reference data with a short-lived ds-sso
client-credentials token instead of a person's personal API token. They are
a second trust root on the personal-token listener only, and they are
narrower than a personal token: reference data, `GET` only, no user.

**It is off by default.** With `api.testAuth.enabled: false` (the default,
`TEST_AUTH_ENABLED=false`) the api builds no verifier, fetches no JWKS, and
refuses every test token on every listener. Turning it off again is the
kill switch: there is no per-token revocation, since the tokens live an
hour.

Code: `crates/api/src/auth/test_auth.rs`. The ds-sso side (the provider,
the client and minting a token) is Ranma-Config's
docs/specs/ds-sso-test-credentials.md.

## What a test token is

A JWT that ds-sso's `distant-signal-test` OAuth2 provider issues to a
client-credentials client in the `ds-test` group. The api checks, in this
order, and refuses (`401 invalid_token`) on the first failure:

1. The header's `alg` is `RS256`. `none`, `HS*`, `RS384`, `PS256` and every
   other algorithm are refused before any key lookup.
2. The signature, against the issuer's JWKS. The JWKS URL comes from OIDC
   discovery on the issuer; keys are cached in the process and refetched
   only for an unknown `kid`, at most once every 5 seconds, with an unknown
   `kid` remembered for 30 seconds (the same `ServiceTokenVerifier` as the
   delegated and internal verifiers). Not a logout token.
3. `iss` equals `TEST_AUTH_ISSUER` exactly, `aud` contains
   `TEST_AUTH_AUDIENCE`.
4. `exp`, `nbf` (when present) and `iat` (required: a token without it is
   refused), each with the delegated verifier's 60-second clock-skew
   leeway. A lifetime (`exp` minus `iat`) of at most 24 hours: there is no
   per-token revocation, so a long-lived token is refused. A non-empty `sub`.
5. `groups` contains `TEST_AUTH_REQUIRED_GROUP`.

## Where it is accepted

Only on the personal-token listener (`PERSONAL_TOKENS_LISTEN_ADDR`, port
8083, the API hostname), so test credentials need personal tokens on
(`PERSONAL_TOKENS_ENABLED`); the api refuses to start otherwise. The
token listener's gate hands any request whose `Authorization` header names
the test issuer or audience (unverified, to pick the path) to the test
verifier; it never reaches the personal-token lookup.

| Request | Answer |
|---|---|
| `GET` a `lines:read` or `trains:read` route: `/v1/Line/Mode/{mode}/Status`, `/v1/Line/{ids}/Status`, `/v1/StopPoint/{crs}/Disruption`, `/v1/Train/by-uid/{train_uid}/{date}`, `/v1/island-of-ireland/trains/{train_code}/{date}`, `/v1/stations/{crs}/departures` | The handler, wrapped as `{data, attribution}` like a personal token's |
| `GET /v1/status`, `GET /v1/openapi.json` | Open, as for everyone |
| `GET /v1/me`, `/v1/Journeys/*`, `/v1/preferences*` | `403 insufficient_scope` |
| Any other method | `405 method_not_allowed` |
| The token with a session cookie, or with a second `Authorization` header (a `dsp_` token, say) | `400 ambiguous_credentials` |
| The token on the website's listener or the delegated listener | `401 invalid_token`, logged `reason=wrong_listener`, whether or not the feature is on |
| Feature off | `401 invalid_token`, `reason=disabled` |

The test `test_tokens_reach_reference_routes_only` pins the open routes: a route added to
`TOKEN_ROUTES` with `lines:read` or `trains:read` is open to test
credentials, so it must read no user.

## The sandbox principal

A verified token becomes the principal `test:<sub>` for that request. It is
never inserted into `users` and has no journeys, preferences, grants or
writes: the handlers it can reach take no user, and the user-data
extractor (`DelegatedOrSessionUser`) refuses it with `403` as a last line.
A DB test (`test_credentials_never_touch_users`) checks that no `users`,
`sessions`, `personal_api_tokens` or `journeys` row appears.

## Turning it on

1. ds-sso: the `distant-signal-test` provider and a client in `ds-test`
   (Ranma-Config's spec).
2. Confirm Authentik puts `groups` in that client's client-credentials
   tokens ("Checking a real token" below). Without it every token is
   refused `missing_group`.
3. DS chart values (Ranma-Config, `clusters/<cluster>/apps/distant-signal.yaml`):

   ```yaml
   api:
     testAuth:
       enabled: true
   ```

   The issuer, audience and group default to the values below; leave them.
   Personal tokens must already be on.
4. Check: a minted token gets `200` from
   `https://ds-api.cursed.solutions/v1/Line/Mode/national-rail/Status` and
   `403` from `/v1/me`; the same token sent to the website's `/api/...`
   gets `401`; Loki has `event=test_auth` lines.

## Settings

| Variable | Chart value | Default |
|---|---|---|
| `TEST_AUTH_ENABLED` | `api.testAuth.enabled` | `false` |
| `TEST_AUTH_ISSUER` | `api.testAuth.issuerUrl` | `https://ds-sso.cursed.solutions/application/o/distant-signal-test/` (the only value accepted while enabled) |
| `TEST_AUTH_AUDIENCE` | `api.testAuth.audience` | `distant-signal-test` |
| `TEST_AUTH_REQUIRED_GROUP` | `api.testAuth.requiredGroup` | `ds-test` |
| `TEST_AUTH_RATE_PER_MINUTE`, `TEST_AUTH_RATE_BURST` | | 30, 10 (per `sub`) |
| `TEST_AUTH_RATE_TRAINS_PER_MINUTE`, `TEST_AUTH_RATE_TRAINS_BURST` | | 10, 5 (per `sub`, train routes) |
| `TEST_AUTH_RATE_GLOBAL_PER_MINUTE`, `TEST_AUTH_RATE_GLOBAL_BURST` | | 120, 30 (every test token) |

Startup guards, while enabled: the issuer is exactly the one above; the
audience differs from `SSO_CLIENT_ID`, `INTERNAL_OAUTH_CLIENT_ID` and every
`DELEGATED_CLIENTS` audience; the issuer (trailing slash aside) differs from
`SSO_ISSUER_URL`, `INTERNAL_OAUTH_ISSUER_URL` and every delegated issuer;
personal tokens are on. A violation stops the api from starting.

## Limits

The buckets are per pod and apart from the personal tokens': per `sub`, per
`sub` on the train routes, and every test token together. Over a limit:
`429 rate_limited` with `Retry-After`. A token that fails verification is
charged to the listener's failed-authentication bucket per client address,
like a bad personal token.

## Observability

- Audit log lines (`audit=true`): `event=test_auth` per request (`sub`,
  `principal`, method, route template, status) and
  `event=test_auth_rejected` per refusal (`reason`, and `sub` once
  verified). The token is never logged; a test checks it.
- Metrics: `distant_signal_api_test_auth_requests_total{route,status}`,
  `distant_signal_api_test_auth_rejected_total{reason}` and
  `distant_signal_api_rate_limited_total{class="personal_token",caller="test_auth"}`.

## Checking a real token

It is not yet confirmed that Authentik includes the `groups` claim in a
client-credentials token. An ignored test checks a real one; it returns
early (passes) unless `DS_TEST_AUTH_REAL_TOKEN` is set, so CI's DB jobs
never run it. Mint a token with Ranma-Config's
`scripts/ds-sso-mint-test-creds.sh` (it prints the access token), then,
from this repository, with network access to ds-sso:

```sh
DS_TEST_AUTH_REAL_TOKEN="<the access token>" cargo test -p api \
  real_ds_sso_client_credentials_token_carries_the_ds_test_group -- --ignored
```

It fails with the claim it found when `groups` is missing or lacks
`ds-test`, or with the refusal reason when the token does not verify
against the real issuer. Do not paste the token anywhere else; it expires
within the hour.
