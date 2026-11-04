# ds-api

Part of [Distant Signal](https://github.com/Distant-Signal/distant-signal).
The public api, the trip planner (`trip-planner`) and the timetable services (`schedule-ingest`, `schedule-reference`), with the generated TypeScript API types (`api-types/api.ts`) and the API changelog (`docs/api-changelog.md`).

## Images

Every push to `main` builds and publishes (`.github/workflows/containers.yml`):

- `ghcr.io/distant-signal/images/api`
- `ghcr.io/distant-signal/images/schedule-ingest`
- `ghcr.io/distant-signal/images/schedule-reference`

The deployed versions are pinned by digest in
[ds-deploy](https://github.com/Distant-Signal/ds-deploy). Renovate is
installed there but not running yet, so until it is, a ds-deploy pull
request bumps a pin by hand.

## The frontend's types and OpenAPI document

`api-types/api.ts` (generated from the api's Rust types with ts-rs) and
`api-types/openapi/v1.json` (the personal-token API's OpenAPI document)
are checked in; `cargo test` fails when either is stale:

```sh
UPDATE_API_TYPES=1 cargo test -p api api_types
UPDATE_OPENAPI=1 cargo test -p api openapi_document_is_current
```

The api image carries both, at `/usr/share/distant-signal/api-types.ts` and
`/usr/share/distant-signal/openapi-v1.json`, and its build tag
(`build.<run id>-<commit>`) is their version. The website
([distant-signal](https://github.com/Distant-Signal/distant-signal)) pins
one api image in `frontend/api-image.txt`, copies both files out of it
(`scripts/sync-api-types.py`, digest-verified) and runs its e2e tests
against it. So a change the website needs lands here first, additively (a
new optional field or a new route, with a `docs/api-changelog.md` entry);
then the website bumps its pin (`uv run scripts/sync-api-types.py pin
--latest`) in the pull request that uses it. Removing or renaming a field
waits until the deployed website no longer reads it.

## ds-core

The shared crates (`common`, `ds-runtime`, `health-http`, `trust-schema`, `schedule-query`, `ds-store`, `ds-lines`) come from
[ds-core](https://github.com/Distant-Signal/ds-core) as git dependencies,
all on one tag (`Cargo.toml`, `[workspace.dependencies]`). Once Renovate
runs here it opens one `renovate/ds-core` PR per ds-core release; until
then, bump every crate's tag together by hand.

To build against a local ds-core checkout beside this one, without
committing anything, pass a patch for each crate, e.g.:

```sh
cargo --config 'patch."https://github.com/Distant-Signal/ds-core".common.path="../ds-core/crates/common"' test
```

## Development

```sh
cargo test --workspace                         # the tests that need no database
uv run scripts/db-tests.py list-passes         # the DB test passes (DATABASE_URL, REDIS_URL)
python3 ../ci/scripts/gen-rust-dockerfiles.py  # regenerate docker/*.Dockerfile
```

The schema, grants and Redis ACL belong to ds-core: a change that needs a
migration or a grant starts there.

## Issues

File them in
[Distant-Signal/distant-signal](https://github.com/Distant-Signal/distant-signal/issues),
with the `area:api` label: this repository has Issues turned off. The
contributing guide for every repository is the organisation's
[CONTRIBUTING.md](https://github.com/Distant-Signal/.github/blob/main/CONTRIBUTING.md);
[CLAUDE.md](CLAUDE.md) lists this repository's commands and conventions.

## Licence

AGPL-3.0-or-later (`LICENSE`).
