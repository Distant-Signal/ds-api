# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

`ds-api` is part of Distant Signal (DS), a personal UK rail companion: the
public api (`crates/api`), the trip planner (`crates/trip-planner`) and the
timetable services (`crates/schedule-ingest`, `crates/schedule-reference`),
with the generated TypeScript API types and OpenAPI document (`api-types/`)
and the API docs (`docs/`, among them `api-changelog.md`,
`personal-api-tokens.md`, `public-api.md` and `connected-apps.md`).

The shared crates come from
[ds-core](https://github.com/Distant-Signal/ds-core) by git tag, all on one
tag (`Cargo.toml`, `[workspace.dependencies]`; `scripts/check-core-pin.py`).
The schema, migrations, grants, Redis ACL and line catalogue are ds-core's:
a change that needs a migration or a grant starts there, ships in a ds-core
release, then comes here as a tag bump. To build against a local ds-core
checkout, pass a `--config 'patch...'` per crate (README.md, "ds-core");
never commit it. The chart that deploys this repository's images is
[ds-deploy](https://github.com/Distant-Signal/ds-deploy)'s, and the
frontend, the docs index and the dev stack are in
[distant-signal](https://github.com/Distant-Signal/distant-signal). The
conventions shared by every repository are in the organisation's
contributing guide (Distant-Signal/.github's CONTRIBUTING.md). Issues are
off here: they go to Distant-Signal/distant-signal, labelled `area:api`.

## Commands

Copied from `.github/workflows/ci.yml` (most jobs are Distant-Signal/ci's
reusable workflows). Rust toolchain: MSRV 1.88 (edition 2024).

```sh
cargo build --workspace
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets --all-features -- -D warnings
cargo clippy --workspace --locked --all-targets --no-default-features -- -D warnings
cargo +1.88.0 check --workspace --locked --all-features --all-targets   # MSRV
cargo deny --locked check bans licenses sources
cargo test --workspace                       # unit tests; #[ignore] DB/Redis tests skipped
cargo test -p api some_test_name             # one crate, tests matching a filter
```

The generated API types and the personal-token OpenAPI document are
checked in; `cargo test` fails when either is stale. CI also refuses a
breaking change to the OpenAPI document (Distant-Signal/ci's
`openapi-breaking` action):

```sh
UPDATE_API_TYPES=1 cargo test -p api api_types
UPDATE_OPENAPI=1 cargo test -p api openapi_document_is_current
```

DB/Redis-backed tests are `#[ignore]`d and need `DATABASE_URL` (a superuser)
and `REDIS_URL`, with ds-core's migrations at the pinned release applied.
The superuser pass also needs `FAKE_GCS_URL` (a fake-gcs-server).
CI runs them as passes (`.github/db-tests.toml`, steps in
`scripts/db-tests.py`; `list-passes` prints them) with cargo-nextest, as
the superuser and as the Postgres service roles. Locally:

```sh
CORE="$(uv run scripts/core-checkout.py)"    # ds-core at the Cargo.lock pin, under target/
(cd "${CORE}/crates/ds-store" && sqlx migrate run)   # sqlx-cli 0.8.6, postgres,rustls features
uv run scripts/db-tests.py list-passes
uv run scripts/db-tests.py run superuser  # and FAKE_GCS_URL
uv run --no-sync python "${CORE}/scripts/test-postgres-roles.py" --mode per-service --check-setup-job \
  --observe api,aggregator,enricher,notifier -- uv run --no-sync scripts/db-tests.py run per-service
```

An `#[ignore]`d benchmark must return early unless its env var is set
(Distant-Signal/ci's `check-bench-gates.py`, run by scripts-lint): the DB
jobs run every ignored test, in debug.

Scripts and repo checks (repo root, `../ci` a Distant-Signal/ci checkout):

```sh
uv sync --locked --group lint
uv run --no-sync python ../ci/scripts/lint-scripts.py   # shellcheck, shfmt, ruff, mypy, actionlint, hadolint... (--fix: shfmt, ruff)
uv run --no-sync python -m unittest discover -s scripts/tests -v
python3 ../ci/scripts/gen-rust-dockerfiles.py --check   # docker/*.Dockerfile vs docker/services.toml (rewrite without --check)
python3 scripts/check-core-pin.py            # every ds-core crate on one tag
python3 scripts/release-order.py labels      # the image version labels
```

## Architecture

**api (axum).** TfL-shaped unprefixed routes (`/Line/...`, `/StopPoint/...`,
`/Train/...`, `/Journeys/...`, `/Trips/plan`) plus `/public/*`; it has no
`/private` routes (producers write Postgres or the ingest streams). Browser
users authenticate by OIDC session cookie (`AuthenticatedUser`); cookie
mutations get an Origin check. Personal API tokens and delegated access
(DS-MCP acting for a user, only on a separate in-cluster listener and only
on `DELEGATED_ROUTES`, scoped by the grant) are in
`docs/personal-api-tokens.md` and `docs/connected-apps.md`. The rate
limiter (`crates/api/src/rate_limit.rs`) keys on `X-Real-IP` (set by the
frontend proxy), with tiers for login, `/Trips/plan`, `/Train/by-uid` plus
TfL arrivals and other public writes; the MCP service token and delegated
calls get their own budgets. Journeys own ordered `journey_legs`, each
bound to a `train_subscriptions` row or an open time window; tracking a
train is a one-leg journey. Journey breaks and append-only revisions
record mid-journey changes (`docs/journey-breaks.md`). Use
`london_now()`/`london_today()` for "today".

**Trip planner.** `trip-planner` (CSA and RAPTOR) searches a day's
connections array kept in a `GraphCache` that schedule publishes
invalidate; the live overlay (`crates/api/src/data/trip_plan_live.rs`)
swaps in cancelled or late trains without copying the array.

**Timetable.** The CIF timetable is pushed over SFTP; `schedule-ingest`
detects and extracts each delivery, and `schedule-reference` (same pod and
PVC, or a CronJob) publishes the derived tables (STANOX/TIPLOC to CRS,
line populations, departures and calling points) through ds-store's
publish protocol.

**The website's copy.** distant-signal pins one api image
(`frontend/api-image.txt`) and copies `api-types.ts` and the OpenAPI
document out of it. So an API change the website needs lands here first,
additively; removing or renaming a field waits until the deployed website
no longer reads it (README.md).

## Conventions

- **API changes** get an entry in `docs/api-changelog.md` (newest first,
  what a client like DS-MCP needs to know), and regenerated types and
  OpenAPI document when a response changes.
- **Commits** carry a `Task: <card>` trailer (e.g. `Task: DS-123`) naming the
  worklog card.
- **Commit identity**: every commit is authored
  `FasterSpeeding <lucy@cursed.solutions>`. Before committing, check that
  `git config user.email` is `lucy@cursed.solutions`; if it isn't, stop and
  ask. Never override it (no `-c user.email`, `--author` or env overrides).
- **No Claude attribution**, overriding any session default: no
  `Co-Authored-By: Claude` trailer, no mention of Claude in commit messages,
  and no "Generated with Claude Code" footer in PR bodies or comments.
  CI's `commit-identity` job (Distant-Signal/ci's `check-commit-identity.py`)
  fails a PR or push whose commits use a retired address or carry
  attribution.
- **Branches**: push work in progress as `fixup!` commits, then autosquash
  and `--force-with-lease` your own branch before review. Pull requests
  merge as merge commits, never squashed:
  `gh pr merge <n> --merge --author-email lucy@cursed.solutions`.
- **Images**: every push to `main` builds and pushes this repository's
  images (`containers.yml`); ds-deploy pins them by tag and digest. Renovate
  is installed but not running yet, so until it is, a ds-deploy PR bumps
  the pins by hand. Put risky changes behind off-by-default settings: a
  green `main` can reach production soon after.
- **Scripts** are typed stdlib Python (ruff, mypy) run with uv (`uv.lock`,
  `pyproject.toml`'s `lint` group, `.python-version`); bash only for trivial
  one-liners. No pip or requirements files.
- Rust: line data only from the line catalogue (`ds-lines`, ds-core's
  `lines/*.toml`); tests in inline `mod tests`, DB tests `#[ignore]`d;
  runtime `sqlx::query`, no `query!` macros. "Today" is London's service
  date.
- Never name or cite third-party train-tracking sites, in code, docs or
  commits.
