# Session revocation: admin revoke and Authentik back-channel logout

A login creates a session that ends after `SESSION_TTL_DAYS` (default 14)
without use, or `SESSION_MAX_AGE_DAYS` (default 90) after the login however
active it is: each authenticated request slides the expiry, at most once per
`SESSION_EXTEND_THROTTLE_HOURS` (default 24). The
session carries the user's Authentik `groups` claim as it was at login
(`users.groups`), and every group-gated feature reads it from there. Today
that is admin (`ADMIN_GROUP`) alone: the chatbot is open to any live
session (its `CHATBOT_ACCESS_GROUP` gate was removed on 2026-10-14).

Code: `crates/api/src/routes/admin.rs`, `crates/api/src/routes/auth.rs`
(`backchannel_logout`), `crates/api/src/auth/backchannel_logout.rs` and
`crates/api/src/data/users.rs` (`revoke_all_sessions`). The metrics and
logs that say why a session ended are in `crates/api/src/session_events.rs`
(see "Session metrics and logs" below).

## How to end a session

| Who | How | Effect |
| --- | --- | --- |
| The user | Account menu, "log out other sessions" (`POST /api/auth/sessions/revoke-others`) | Ends all of their sessions and reissues one for the current browser. The optional body `{"revokeCalendarFeed": true, "revokePersonalTokens": true, "revokeShareLinks": true, "revokeInviteLinks": true}` (all default false) also revokes the calendar feed link, every personal API token, the journey share links the user created and the group invite links the user created |
| The user | Account menu, "log out everywhere" (`POST /api/auth/logout-everywhere`, off by default: see "Log out everywhere" below) | Ends all of their sessions, this browser's included, then the site sends the browser to the identity provider's end-session URL |
| An admin | `POST /api/admin/users/revoke-sessions` | Ends all of the target user's sessions |
| Authentik | Back-channel logout to `POST /api/auth/backchannel-logout` | Ends all of that user's sessions |

All four set `users.sessions_invalidated_at` and delete the user's `sessions`
rows in one transaction. The next request from a revoked browser is anonymous.
The user can log in again immediately unless they are also disabled in
Authentik.

**Connected apps.** The admin revoke and the Authentik back-channel logout
also revoke every delegated-access grant the user holds (reasons `admin` and
`backchannel`), in the same transaction, so an assistant connected on the
Connected apps page stops working on its next request and the user must
connect it again. "Log out other sessions" keeps the grants but refuses
delegated tokens issued before it. See [connected-apps.md](connected-apps.md).

## Log out everywhere

Off by default. Two settings (chart values to be added in ds-deploy:
`api.logoutEverywhere.enabled` for `LOGOUT_EVERYWHERE_ENABLED`, and
`api.logoutEverywhere.endSessionUrl` for `SSO_END_SESSION_URL`):

- `LOGOUT_EVERYWHERE_ENABLED` (default `false`). Off: `POST
  /auth/logout-everywhere` answers `404` and `GET /auth/session` says
  `logoutEverywhereEnabled: false`, so the site hides the menu item.
- `SSO_END_SESSION_URL`: the identity provider's `end_session_endpoint`, for
  the `distant-signal` provider
  `https://ds-sso.cursed.solutions/application/o/distant-signal/end-session/`.
  It is given explicitly, not read from OIDC discovery: discovery is lazy
  here (nothing at startup touches the network) and the discovery client does
  not carry the endpoint. It must be an absolute `https` URL with a host and no
  user info, query or fragment; the api refuses to start otherwise (even with
  the switch off), and refuses to start with the switch on and no URL.

`POST /api/auth/logout-everywhere` (`POST /auth/logout-everywhere` on the
api): cookie session only, the same strict Origin/Referer check as logout
(`403` cross-site, `401` with no live session; delegated and personal tokens
are not accepted). In one transaction it sets `sessions_invalidated_at`,
deletes every `sessions` row of the user (the current browser's too) and moves
each live delegated grant's `not_before` to now. It then clears the session
cookie and answers `200 {"endSessionUrl": "..."}` (`Cache-Control:
no-store`); the site clears its own browser-held account data and navigates
to that URL. The metric and log reason is `logout_everywhere`
(`session_events_total{event="revoked",reason="logout_everywhere"}`, rows in
`sessions_deleted_total`).

**Grants.** The call itself behaves like "log out other sessions" for connected
apps: the grants are kept and refuse delegated tokens issued before now. (It
does not revoke them: `delegated_grants.revoked_reason` has no value for it,
and adding one is a ds-core migration.) The identity provider's back-channel
logout then does: ending the browser's provider session sends a logout token
for the user to `POST /auth/backchannel-logout`, which revokes the grants
(reason `backchannel`), as above. That call finds no sessions and still
answers `200`; a repeat finds no grants either and is still `200`. If the
browser never reaches the end-session URL, or the provider does not send the
back-channel logout, the user is logged out of Distant Signal everywhere and
their connected apps are refused their older tokens, but the grants stay until
the user disconnects them.

**What it ends.** Every Distant Signal session of the user, on every device. With
the provider prerequisite below, this browser's identity provider session,
and through the back-channel logouts every connected app's tokens (DS-MCP's
tokens and the Distant Signal delegated grant). **What it does not end.**
Identity provider sessions in other browsers (a login there can sign the user
straight back in to Distant Signal), and the upstream Discord or Google sessions, which stay. Personal API tokens, the
calendar feed and share and invite links are untouched. The honest summary:
it logs the user out of Distant Signal everywhere and out of the provider on
this browser, not out of everything.

**Provider prerequisite (infra).** The provider's invalidation flow must be
`default-invalidation-flow` (with its User Logout stage), not the empty
`default-provider-invalidation-flow`: with the empty one the end-session URL
ends only the provider token, no back-channel logout is sent and the browser's
provider session survives. Do not turn the switch on until that is live. With
no parameters and no registered post-logout redirect URI the endpoint needs
no `id_token_hint` and redirects straight into the flow; the site sends none,
and the user lands on the provider's own page afterwards.

**Rollback** is the switch: set `LOGOUT_EVERYWHERE_ENABLED` to `false`
(or unset it). The route is `404` and the menu item disappears on the next
session check. Sessions already ended stay ended.

## Recent login for account deletion

`DELETE /api/account` answers `403 recent_login_required` unless the
session's login is at most `SESSION_RECENT_LOGIN_MINUTES` old (default 15;
1 to 1440; the api refuses to start outside that). The login time is
`sessions.created_at`: a login sets it, "log out other sessions" carries it
over to the session it reissues, and nothing else moves it, so a stolen
cookie cannot freshen itself. The site satisfies it by sending the user to
`/api/auth/login?reauth=1&return_to=...`, which asks the identity provider
for `prompt=login` and `max_age=0`. Authentik honours both, and does not
need any setting for it. The callback also refuses (redirecting with `login=failed`, no new session) a re-authentication
whose ID token's `auth_time` is outside the window (a provider that
ignored the prompt) or that is for a different account than the browser's
session. A provider that sends no `auth_time` is accepted on the strength
of the prompt alone.

## When a group change takes effect

`users.groups` is overwritten from the ID token on every login and on no other
occasion. It is never merged, so a removed group disappears on the next login.

- **Adding** someone to a group takes effect at their next login. They can log
  out and back in to get it straight away.
- **Removing** someone from a group, or disabling them, does NOT take effect
  on existing sessions by itself. Those sessions keep the old groups until
  they end: `SESSION_TTL_DAYS` after their last use, and for a session in
  daily use up to `SESSION_MAX_AGE_DAYS` after its login. To make it take
  effect now:
  1. Remove the group (or disable the user) in Authentik.
  2. Revoke their sessions here, either through an admin revoke or through
     Authentik's back-channel logout (see below).
  3. Their next login reads the new groups. A disabled user can't log in.

The admin check itself uses `users.groups`. An admin removed from
`ADMIN_GROUP` keeps admin rights until their next login, unless another admin
revokes their sessions.

## Admin revoke

Off by default. Set `api.adminGroup` in the chart (`ADMIN_GROUP` env) to an
Authentik group, e.g. `distant-signal-admins`. When it is empty, the endpoint
returns 403 to everyone. Startup logs which state applies.

The request must come from a logged-in browser session whose `groups` contain
the admin group. It must also carry a same-origin `Origin` (or `Referer`), the
same strict check `logout` uses. There is no admin UI. From the devtools
console on the site:

```js
await fetch('/api/admin/users/revoke-sessions', {
  method: 'POST',
  headers: { 'Content-Type': 'application/json' },
  body: JSON.stringify({ username: 'their-authentik-username' }), // or { userId: '<OIDC sub>' }
}).then(r => r.json())
// => { userId: '...', sessionsRevoked: 2 }
```

`userId` is the user's Authentik subject (`sub`, stored as `users.id`).
`username` is their Authentik username (`preferred_username`) as of their
last sign-in to Distant Signal, matched case-insensitively. Distant Signal
does not request or store email addresses, so there is no lookup by email;
a body with an `email` field gets `400 email_lookup_removed_use_username`.

| Status | Meaning |
| --- | --- |
| 200 | Revoked; `sessionsRevoked` is the number of rows deleted (0 is fine) |
| 400 | Body must contain exactly one of `userId` or `username` (`give_exactly_one_of_userId_or_username`), or used the removed `email` field |
| 401 | Caller not logged in |
| 403 | Cross-site request, caller not in `ADMIN_GROUP`, or `ADMIN_GROUP` unset |
| 404 | No such user (username matching is case-insensitive, against the username stored at their last sign-in) |
| 409 | More than one user has that username (`username_ambiguous`); use `userId` |

Each revocation is logged at WARN with `audit=true event=admin_session_revoke
admin_user_id=… target_user_id=… sessions_revoked=…`. A non-admin attempt is
logged with `event=admin_session_revoke_denied`.

## Authentik back-channel logout

The endpoint implements OpenID Connect Back-Channel Logout 1.0. Authentik
POSTs `logout_token=<JWT>` whenever an Authentik session that logged in to
Distant Signal is deleted. That happens when the user logs out of Authentik,
an admin deletes the session, or the user is deactivated (which deletes all
their sessions).

The api validates the token before doing anything:
- the signature, against the JWKS discovered from `SSO_ISSUER_URL`;
- the `typ` header, which must be `logout+jwt` if present;
- `iss` must equal `SSO_ISSUER_URL`, and `aud` must contain `SSO_CLIENT_ID`;
- `iat` must be present, not in the future and at most 1 hour old, and `exp`
  (if present) must not have passed;
- the back-channel logout `events` member must be present;
- `sub` must be present;
- there must be no `nonce`.

It then ends all of that user's sessions. Responses:
- 200 for a valid token, including one for an unknown user;
- 400 for an invalid token;
- 500 if the database write fails (Authentik retries).

Every response carries `Cache-Control: no-store`. Each revocation is logged at
INFO with `audit=true event=backchannel_logout user_id=…`. A rejected token is
logged at WARN with the reason.

**Per user, not per session.** Authentik sends one token per Authentik session,
with both `sub` and `sid`. The api doesn't store `sid` and ends every session
of the user. As a result, logging out of Authentik on one device also logs
that user out of Distant Signal on their other devices. A token carrying only
`sid` is refused with 400. Authentik 2026.8 always sends `sub`.

### Limits

- **Authentik only notifies while the access token from that login is still
  valid.** In 2026.8, `providers/oauth2/signals.py` sends the logout only for
  unexpired `AccessToken` rows of the ending session. The default
  `access_token_validity` is `hours=1`, so a session deleted more than an hour
  after the Distant Signal login produces no back-channel call. The provider
  setting below raises it to match the session's absolute lifetime
  (`SESSION_MAX_AGE_DAYS`): a sliding session can outlive the idle timeout,
  so a validity of only `SESSION_TTL_DAYS` would leave a session in daily use
  past that point with no back-channel call. Distant Signal discards
  the access token at login and never uses it, so a longer validity doesn't
  widen what a leaked Distant Signal database or log can do.
- It doesn't cover group changes. Removing a group ends no Authentik session.
  Use an admin revoke.
- The provider must sign with an asymmetric key (RS256). Without a signing
  key, Authentik uses HS256 with the client secret, which the JWKS can't
  verify, and every token is rejected (400, `UnknownKey` or `Malformed` in the
  api log).

### Authentik configuration (for the Ranma-Config session)

These settings are on the production Authentik (2026.8.1,
`https://sso.example.com`). They apply to the OAuth2/OpenID provider
behind the `distant-signal` application (issuer
`https://sso.example.com/application/o/distant-signal/`), not the
`distant-signal-internal` one.

1. **Provider > Edit > Advanced protocol settings** (blueprint attrs in
   brackets):
   - **Logout URI** (`logout_uri`):
     `https://ds.cursed.solutions/api/auth/backchannel-logout`
   - **Logout Method** (`logout_method`): `Back-channel` (`backchannel`, the
     model default)
   - **Access token validity** (`access_token_validity`): `days=90`, equal to
     `api.sessionMaxAgeDays` / `SESSION_MAX_AGE_DAYS` (it was `days=14`,
     `api.sessionTtlDays`, before sessions slid). See Limits.
   - **Signing Key** (`signing_key`): must be set to an RSA certificate. It
     almost certainly is already, since logins verify ID tokens against the
     JWKS.
2. **Network path.** The Authentik worker sends the POST from the `authentik`
   namespace to `ds.cursed.solutions`. That name resolves to Cloudflare, and
   `netpol-authentik.yaml`'s `allow-egress-internet` already allows it. Check
   that no Cloudflare WAF, bot-fight or Access rule challenges a server-to-server
   POST to `/api/auth/backchannel-logout`. The request carries no browser
   headers.
3. **Admin group (optional).** Create an Authentik group, e.g.
   `distant-signal-admins`, and add the operators. Make sure the provider's
   `groups` scope mapping emits it. Then set `api.adminGroup:
   distant-signal-admins` in the distant-signal HelmRelease values.
4. **Verify.**
   - Log in to Distant Signal in a private window, then log out of Authentik
     in that window.
   - The worker's task list (Admin > System Tasks, or the worker log) should
     show "Back-channel logout successful".
   - The api log should show `event=backchannel_logout`.
   - Reloading Distant Signal should show you logged out.
   - Then deactivate a test user who has a live Distant Signal session, and
     confirm the same.

## Session metrics and logs

Every way a session starts, is refused, slides or ends is counted and logged,
so "why did this user have to log in again?" has an answer. This is
observability only: it changes no lifetime, no check and no response.

### Metrics

All three are on the api's Prometheus listener, registered at 0 at startup so
`increase()` sees the first event after a restart. Labels take only the fixed
values below; nothing about a user is ever a label.

`distant_signal_api_session_events_total{event, reason}`: one per event.

| `event` | `reason` | Meaning |
| --- | --- | --- |
| `created` | `login` | An OIDC login made a session |
| `created` | `reauth` | A login that re-authenticated the user (account deletion) made a session |
| `created` | `revoke_others_reissue` | "Log out other sessions" reissued the caller's session |
| `rejected` | `no_cookie` | A route that needs a login got no `Cookie` header, or none named `distant_signal_session`. Authenticated-only routes only (see the session probe below) |
| `rejected` | `unknown` | A cookie whose hashed id has no `sessions` row: never existed (or a forged or truncated value), ended by a logout, a re-login, a revoke or an account deletion, or **expired and then swept** (the sweep runs every `SESSION_CLEANUP_INTERVAL_SECS`, default one hour, in the api, or on the `apiMaintenance` cron job's schedule when `API_BACKGROUND_LOOPS=false`, so a session that lapsed longer ago than that reads as `unknown`, not `expired_idle`) |
| `rejected` | `expired_idle` | The row exists and its expiry passed before the absolute cap: not used for `SESSION_TTL_DAYS`, and not swept yet |
| `rejected` | `expired_absolute` | The row exists and its expiry is at the absolute cap (`created_at + SESSION_MAX_AGE_DAYS`) |
| `rejected` | `invalidated` | The row exists but its user's `sessions_invalidated_at` is later than the row's `created_at`. Revocation deletes the rows in the same transaction, so this needs a race (a login committing during a revocation); expect it to stay at 0 |
| `extended` | `slide` | A due session's expiry slid forward (at most once per `SESSION_EXTEND_THROTTLE_HOURS`) |
| `extend_failed` | `db_error` | The slide failed; the request went on and the next due request retries |
| `revoked` | `backchannel`, `admin`, `revoke_others`, `user_logout`, `logout_everywhere`, `account_deletion`, `login_replaced`, `rekey`, `expiry_sweep` | Session rows were deleted. `login_replaced`: a login ended the session its browser carried. `rekey`: account continuity re-keyed the account. `expiry_sweep`: the api's sweep deleted lapsed rows (counted once per sweep that deleted any; the `apiMaintenance` cron job has no metrics listener, so its sweeps are not counted) |
| `backchannel_rejected` | `invalid_token`, `unknown_user`, `other` | A back-channel logout that ended nothing: the token failed validation (or named only a `sid`), the token was valid but names no user here (answered 200), or the request had no usable `logout_token` or was too large |

`distant_signal_api_sessions_deleted_total{reason}`: session rows deleted,
summed, with the same `reason` values as `revoked` (the events counter counts
operations, this counts rows).

`distant_signal_api_session_probe_total{result}`: what the site's "am I logged
in" call (`GET /auth/session`) answered: `authenticated`, or the rejection
reason (`no_cookie`, `unknown`, `expired_idle`, `expired_absolute`,
`invalidated`). Each stale-cookie answer is also counted once in
`session_events_total{event="rejected"}`; `no_cookie` is not, because every
anonymous page load would add one. Public pages that only personalise
(`OptionalAuthenticatedUser`) record nothing.

How a refusal is classified: the live lookup (`get_session_with_user`) runs as
before. Only when it finds nothing, one extra read of the row by its hashed id
(`get_session_row_state`, the primary key) says why. A request that resolves
never runs it, and a request with no cookie runs no query at all. `expired_*`
looks at the row's expiry, not at the age of the login: an expiry at
`created_at + SESSION_MAX_AGE_DAYS` is `expired_absolute`, an earlier one is
`expired_idle`, even if the cap has also passed since.

### Logs

One JSON line per event, with `session_lifecycle=true`, `event` and `reason`
(the same values), and these fields where they apply: `user` (the pseudonym
below, `-` when the owner is not known), `session_age_hours`,
`expired_hours_ago`, `new_expiry_in_hours`, `sessions_deleted`.

- INFO, always: `created`, `extended`, `extend_failed`, `revoked`, and
  `rejected` with `expired_idle`, `expired_absolute` or `invalidated`. These
  are rare and are the diagnostic signal.
- DEBUG: `rejected` with `no_cookie` or `unknown`, which any anonymous visitor
  produces (the counter still counts them).
- Not logged: `backchannel_rejected` (counter only; the handler's existing
  WARN lines give the cause, and the logout token and its claims are never
  logged).

**Privacy.** No log line or label carries a user id, username, email, session
token, cookie value, hashed session id, IP address or user agent. A line may
carry `user`, the **pseudonym**: the first 12 hex characters of
`SHA-256("distant-signal/api/session-log-pseudonym/v1" + NUL + internal user id)`.
It is stable, so the same account's lines match, and it is never a label.
It is a pseudonym, not anonymisation: the hash is unsalted, so anyone with the
list of user ids can recompute it. That is accepted because the ids are opaque
identity-provider subjects, the tag is truncated, and it stays in the log. An
account deletion is logged without a pseudonym, as the deletion's own line is.
The older audit lines (`backchannel_logout`, `admin_session_revoke`,
`account_rekeyed`) name the account id as before; this change does not touch
them.

### Why did this user get logged out?

Find the user's pseudonym from a line you can already tie to them (an admin
revoke names the account in its audit line; compute the tag from the id as
above), then read their lines in order:

```logql
{service="api"} | json | session_lifecycle="true" | user="a1b2c3d4e5f6"
```

(Adjust the stream selector to your Loki labels.) Read the last
`rejected`/`revoked` line before the next `created`:

| What you see | What it means |
| --- | --- |
| `revoked` `backchannel` shortly before | The identity provider ended one of their sessions there (they logged out of it, it expired, or an admin deleted it), and every session here went with it. See "Per user, not per session" above |
| `revoked` `revoke_others`, `user_logout`, `logout_everywhere` or `login_replaced` | The user did it, from another device or browser (a login on a browser that carried an older session ends that one) |
| `revoked` `admin` | An operator did it |
| `rejected` `expired_idle` with `expired_hours_ago` | Unused for `SESSION_TTL_DAYS`, noticed on the next visit. `session_age_hours` says how old the login was |
| `rejected` `expired_absolute` | The 90 day cap |
| No line before the `created`, but a `login` | The cookie never reached the api (`no_cookie`, DEBUG), or its session was swept long ago (`unknown`, DEBUG): look at the probe counter below |

Fleet-wide, in PromQL:

```promql
# why sessions end, per day
sum by (event, reason) (increase(distant_signal_api_session_events_total{event=~"rejected|revoked"}[1d]))

# the identity provider ending sessions: operations and rows
increase(distant_signal_api_session_events_total{event="revoked",reason="backchannel"}[1d])
increase(distant_signal_api_sessions_deleted_total{reason="backchannel"}[1d])

# how a "logged out" answer on the site came about: no cookie at all, or a stale one
sum by (result) (increase(distant_signal_api_session_probe_total{result!="authenticated"}[1d]))

# share of site session checks that found no cookie
sum(increase(distant_signal_api_session_probe_total{result="no_cookie"}[1d]))
  / sum(increase(distant_signal_api_session_probe_total[1d]))

# logins
sum(increase(distant_signal_api_session_events_total{event="created",reason="login"}[1d]))
```

A high `no_cookie` share on the probe with few `revoked` and `expired_*` means
the browser is not sending the cookie (cleared by the browser or an in-app
view, a different cookie jar); `unknown` means the row was already gone;
`revoked` `backchannel` means the identity provider ended it.
