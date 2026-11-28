# Connected apps (delegated access): operator guide

A registered client, today only the Distant-Signal-MCP (ds-mcp), can act
for a Distant Signal (DS) user who connected it on the Connected apps page:
list and read their journeys and their reliability, save an itinerary,
and delete journeys it saved itself. Design and threat model:
[the delegated-access spec](https://github.com/Distant-Signal/distant-signal/blob/main/docs/superpowers/specs/2026-10-14-ds-mcp-delegated-access-design.md).
Client-facing contract: [api-changelog.md](api-changelog.md), 2026-10-15.

Personal API tokens, which a user creates for their own devices and which
work on a separate API hostname, are a different credential: see
[the personal API tokens spec](specs/personal-api-tokens.md), which also
records the amendment to decision D-14.

**It is off by default.** With `api.delegated.clients` empty (the default)
the api binds no delegated listener, the chart creates no delegated Service,
and every delegated bearer is refused.

## How a request is accepted

1. ds-mcp exchanges the user's Authentik token for a 5-minute token from the
   `distant-signal-api` provider (`iss`
   `https://sso.example.com/application/o/distant-signal-api/` until
   the move to `ds-sso`; `aud` and `azp`
   `distant-signal-api`, RS256, a `scope` claim, no `act`, no `sid`).
2. It calls the api's **delegated listener** (`api.delegated.port`, 8082) on
   the `<release>-api-delegated` ClusterIP Service. That listener marks every
   request it accepts; a delegated token arriving any other way (the
   tunnel, the Ingress, the frontend proxy, the main Service) is answered
   `401` and logged `event=delegated_rejected reason=public_listener`. The
   NetworkPolicy admits the port only from `networkPolicy.apiExtraIngressNamespaces`
   (narrowed by `apiExtraIngressPodLabels`); with none listed nothing can
   reach it. The frontend proxy never forwards `Authorization`.
3. A router-wide gate picks the client by the token's unverified `iss` and
   `aud`, then verifies it with that client's own verifier (signature
   against the provider's JWKS, exact `iss`, `aud`, `exp`, `nbf`, a required
   `iat`, not a logout token). It refuses a cookie and a bearer together,
   and any route other than the delegated ones (`DELEGATED_ROUTES`).
4. The rate limiter charges the user's and the client's delegated buckets.
5. The handler's extractor loads the user's active grant for the client,
   refuses a token issued before the grant or before the user's last "log
   out other sessions", expires a grant unused for 90 days, and requires the
   route's scope in all of the token, the grant and the client's
   `maxScopes`. An empty `scope` grants nothing.

`issuerUrl` must equal the token's `iss` exactly, trailing slash included.

A test-credentials token (ds-sso's `distant-signal-test` provider,
[test-auth.md](test-auth.md)) is refused on this listener and the
website's before the delegated gate sees it; it works only on the
personal-token listener. While test credentials are on, the startup guard
keeps their audience and issuer apart from every delegated client's.

## Turning it on

1. Authentik: the `distant-signal-api` provider and application (spec §9.6,
   done in production on 2026-10-14). The application has no group binding
   (decision D-18): any Authentik user may be exchanged for, and the DS grant
   gates use.
2. Chart values:

   ```yaml
   api:
     delegated:
       clients:
         - name: ds-mcp
           displayName: Distant Signal assistant
           issuerUrl: https://sso.example.com/application/o/distant-signal-api/
           audience: distant-signal-api
           maxScopes: [journeys:read, journeys:write, journeys:delete]
           enabled: true
           serviceGroup: srv-ds-mcp
   networkPolicy:
     apiExtraIngressNamespaces: [ds-mcp]
   ```

   The api refuses to start if a delegated audience equals the SSO or
   internal client id (or another client's), or a delegated issuer equals
   the SSO or internal issuer.
3. Deploy. The migration `20261015010000_delegated_access` is expand-only;
   with per-service roles on, re-run the role setup so the notifier gets
   its column grants on `delegated_grants`.
4. ds-mcp points its delegated calls at
   `http://<release>-api-delegated.<namespace>.svc:8082`.

## Users' side

- `/account/connected-apps/ds-mcp` is the consent page ds-mcp links to. It
  needs a DS login (which creates the DS account on a first visit), shows
  who is logged in and what the app may do, and issues a single-use
  confirmation token bound to the session (10 minutes). Allow creates the
  grant with the ticked scopes only. The page cannot be framed.
- After a grant is created the notifier pushes "<app> connected" to each of
  the user's devices, and every page shows a banner for 7 days.
- `/account/connected-apps` (and the account page's link) lists grants,
  with Disconnect, and "Recently deleted by the assistant": what a client
  deleted in the last 30 days.
- With `RECENTLY_DELETED_ENABLED` (off by default), the same table also
  records the user's own deletes (`client` `web`, no grant), kept
  `RECENTLY_DELETED_RETENTION_DAYS` (7 by default), and the journeys
  page's Recently deleted list shows both kinds with "Save again"
  (`crates/api/src/data/recently_deleted.rs`, docs/api-changelog.md
  2026-11-11). A connected app's deletes keep their own 30 days: they are
  the user's check on what an app did, which can go unnoticed for longer
  than their own deletes, and the privacy notice already promises 30
  days. The Connected apps page leaves out the `web` rows, and they never
  count towards a client's daily delete cap.

## Reading notification settings (`preferences:read`)

DS-153 adds a fourth scope, read only: `preferences:read` lets a client
read `GET /public/preferences/notifications` (every push switch, the delay
threshold, quiet hours and any pause) on the delegated listener. It cannot
change them; there is no delegated write to preferences.

To turn it on for ds-mcp:

1. Add `preferences:read` to the client's `maxScopes` in the chart values
   and deploy. Nothing else in DS changes.
2. ds-mcp requests the scope in its token exchange (the token's `scope`
   must include it) and adds a tool that calls the route. That is ds-mcp's
   change, not this repo's.
3. Existing grants do not include it. A user who wants it connects the app
   again from the consent page, which lists "See your notification
   settings" among the scopes; the grant notice says so too.

## Revocation

| Trigger | Effect |
|---|---|
| Disconnect | Grant revoked (`user`) |
| Admin `POST /api/admin/users/revoke-sessions` | Every grant revoked (`admin`), with the sessions |
| Authentik back-channel logout on `distant-signal` | Every grant revoked (`backchannel`) |
| ds-mcp's own logout: `POST /delegated-grants/revoke {sub}` with its `srv-ds-mcp` token | That client's grant revoked (`client_logout`); always `204` |
| 90 days unused | Revoked (`inactive`) on the next request or by the hourly sweep |
| "Log out other sessions" | Grant kept; tokens issued before it refused |
| Account deletion | Rows cascade |
| **Kill switch:** `enabled: false` for the client | Every token for it `401` at once, and its listener is not bound if no client is enabled |

For a suspected ds-mcp compromise: set `enabled: false` and deploy, rotate
the `distant-signal-api` secret in Authentik, and if needed revoke every
grant:

```sql
UPDATE delegated_grants SET revoked_at = now(), revoked_reason = 'client_disabled'
WHERE client = 'ds-mcp' AND revoked_at IS NULL;
```

`journeys.created_via = 'ds-mcp'` finds every journey the client saved, and
`delegated_deletions` (30 days) what it deleted.

## Limits

Per user: 60 reads, 10 saves and 5 deletes a minute (bursts 20, 5, 3); per
client, 20 times that (`api.rateLimit.delegated`). Per user per rolling 24
hours: 50 saves and 20 deletes (`api.delegated.dailySaveLimit`,
`dailyDeleteLimit`), answered `429 daily_limit` with `Retry-After`. An
itinerary may have 1 to 20 legs, the journey's own leg cap, for a
delegated client as for the web app.

## Errors

Every error the delegated listener answers a bearer is JSON `{error,
message}`. A handler's or extractor's plain-text error is wrapped there,
its `error` taken from the status (`bad_request`, `not_found`,
`rate_limited`, `service_unavailable`, `internal_error`), keeping
`Retry-After` (`json_errors` in `crates/api/src/auth/delegated.rs`).

### Why a call was refused (`reason`)

The refusals below also carry a string `reason`, a stable code a client can
turn into advice for the user. It is additive: `error`, `message`, the
status and `WWW-Authenticate` are as they always were, so a client that
ignores `reason` keeps working. A client should treat an unknown `reason`
as the plain error, and a missing one as "no more detail".

A `reason` is only ever sent to a caller whose token verified for the
client (signature, issuer, audience, expiry and the rest). A token that
fails verification, or is for another audience, gets the plain `401
invalid_token` with no `reason`, so the body never tells an unauthenticated
caller whether an account or a grant exists. No body carries a user
identifier. The audit log's `reason` values are unchanged.

| Status | `error` | `reason` | When | What the user should do |
|---|---|---|---|---|
| 403 | `delegation_not_granted` | `no_account` | The token's user has no Distant Signal account | Create an account on the website, then connect the app |
| 403 | `delegation_not_granted` | `never_granted` | No grant for this user and app. Also what a grant reads as once its record is purged, 365 days after it was revoked | Connect the app on the consent page, `/account/connected-apps/ds-mcp` |
| 403 | `delegation_not_granted` | `revoked_by_user` | The user pressed Disconnect | Reconnect on the consent page, if they want it back |
| 403 | `delegation_not_granted` | `revoked_by_admin` | An admin revoked the user's sessions and grants | Reconnect on the consent page |
| 403 | `delegation_not_granted` | `revoked_by_backchannel_logout` | The user logged out of Distant Signal, which ends its grants | Reconnect on the consent page |
| 403 | `delegation_not_granted` | `revoked_by_client_logout` | The app's own logout ended the grant | Reconnect on the consent page |
| 403 | `delegation_not_granted` | `revoked_inactive` | Unused for 90 days (found on the request or by the hourly sweep) | Reconnect on the consent page |
| 403 | `delegation_not_granted` | `revoked_client_disabled` | An operator revoked every grant of the app during an incident | Reconnect on the consent page once the app is back, or ask the administrator |
| 401 | `invalid_token` | `client_disabled` | The app is switched off on the server (the kill switch) | Nothing to do now; ask the administrator when it will be back |
| 401 | `invalid_token` | `token_before_grant` | The token was minted before the grant (or before "Log out other sessions") | Nothing for the user: the app should get a new token and retry once. If it persists, reconnect |
| 403 | `insufficient_scope` | `missing_scope` | The grant (or the token, or the app's maximum) lacks the route's scope. The body's `scope` and the header's `scope=` name it | Reconnect on the consent page and tick the scope; `?scopes=journeys:read%20journeys:write` pre-ticks boxes |
| 403 | `insufficient_scope` | `route_not_delegated` | This route does not accept delegated tokens (message: "this route does not accept delegated tokens") | Nothing the user can do: it is the app's bug |

`missing_scope` also has `scope`, the one scope this route needs (for
example `journeys:write`). A grant revoked because a newer consent replaced
it, or by a reason this version does not know, gets no `reason`; that only
shows in a race with a new consent. A revoked grant's record is kept 365
days (`DELEGATED_REVOKED_GRANT_RETENTION_DAYS`), so `revoked_*` turns into
`never_granted` after that. `no_account` can only follow a verified token:
grants are keyed by the token's `sub`, which is the user's id.

An expired, malformed or wrongly signed token, a wrong audience, the public
listener and a cookie sent with a bearer have no `reason` (a cookie with a
bearer is `400 ambiguous_credentials`).

## Itinerary errors

`POST /Journeys/itinerary` answers a delegated client with JSON `{error,
message}`; `message` is a sentence ds-mcp can show the user as is. The web
app gets the same message as plain text.

| Status | `error` | When |
|---|---|---|
| 400 | `idempotency_key_required` | No, or a malformed, `Idempotency-Key` |
| 400 | `invalid_itinerary` | Bad JSON, 0 or more than 20 legs, a bad UID, CRS code or name, a date past the tracking horizon, the journey or tracked-train cap |
| 409 | `idempotency_conflict` | The key was used for a different body |
| 422 | `train_does_not_call_at_origin` | The train doesn't stop at `originCrs` on that date (a pass is not a stop) |
| 422 | `train_does_not_call_at_destination` | The train doesn't stop at `destinationCrs` on that date |
| 422 | `destination_before_origin` | It stops at both, but reaches `destinationCrs` first |
| 422 | `boarding_not_allowed` | `originCrs` is set-down only for this train, or its terminus |
| 422 | `alighting_not_allowed` | `destinationCrs` is pick-up only for this train (a sleeper's `U` stops) |
| 429 | `daily_limit`, `rate_limited` | With `Retry-After` |

The 422 checks run only on train legs with `originCrs` or `destinationCrs` and
only when the timetable has the train (`schedule_calling_points_full` for
that UID and date). A train not in it yet is saved unchecked, as before,
and resolves through tracking. The first failing leg wins; with more than
one leg its message starts `Leg N: `, N counting untracked legs too. Nothing is saved on any error, and
the `Idempotency-Key` stays unused. Code: `crates/api/src/data/leg_validation.rs`.

## Retention

The hourly maintenance pass (the api loop, or the `api-maintenance`
CronJob) revokes grants unused for 90 days, deletes revoked grants 365 days
after revocation (`api.delegated.revokedGrantRetentionDays`,
`DELEGATED_REVOKED_GRANT_RETENTION_DAYS`), a client's `delegated_deletions` after 30 days (the user's own,
`client` `web`, after `RECENTLY_DELETED_RETENTION_DAYS`), `Idempotency-Key`s
after 24 hours and expired confirmation tokens. See
[personal-data-retention.md](personal-data-retention.md).

## Account continuity (removed)

Account continuity existed for the 2026-11 move of public login to
`ds-sso`: a login with a new `sub` could re-key the matching existing
account in place, matched on a Discord id claim or a primary-provider
`sub` claim. It ran once, at the 2026-11-06 cut-over, and was then turned
off: both claims were fed from a connection identifier the user could
write at the login service, which made it a takeover path. The code, the
settings (`SSO_DISCORD_ID_CLAIM`, `SSO_PRIMARY_SUB_CLAIM`,
`ACCOUNT_CONTINUITY_REQUIRE_STRONG_MATCH`,
`ACCOUNT_CONTINUITY_REVOKE_TOKENS_ON_REKEY`), the `account_rekeyed`,
`account_rekey_refused` and `account_continuity_conflict` audit lines and
the `account_rekeys`, `account_rekey_refused` and
`account_continuity_conflicts` metrics are gone. A login is now keyed on
its `sub` alone and never re-keys an account; any `ds_discord_id` or
`ds_primary_sub` claim in the ID token is ignored, and the api neither
reads nor writes `users.discord_id` (the column is dropped by ds-core).
A deployment that still sets the removed variables starts normally: the
api ignores environment variables it does not declare. See
[api-changelog.md](api-changelog.md).

## Observability

- Audit log lines (`audit=true`): `delegated_request` (every delegated
  request: client, user, grant, `jti`, route, status, scopes),
  `delegated_rejected` (with a reason and a hash of the `sub`),
  `delegated_grant_created` and `delegated_grant_refused` (with a hash of the
  session id), `delegated_grant_revoked`, `delegated_journey_saved`,
  `delegated_delete` (WARN), `delegated_revoke_denied`. Tokens and request
  bodies are never logged.
- Metrics: `distant_signal_api_delegated_requests_total{client,route,status}`,
  `distant_signal_api_delegated_rejected_total{client,reason}`,
  `distant_signal_api_delegated_grants_active{client}`,
  `distant_signal_api_rate_limited_total{caller="delegated_user"|"delegated_client"}`,
  and the notifier's `grant_notice` cycle.
