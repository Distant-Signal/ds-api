# Recovering pruned trains from a point-in-time restore

`scripts/extract-pitr-trains.py` (DS-663, follows DS-638) puts back the full
live rows of trains that were pruned while a journey leg was still bound to
them. The aggregator now pins (keeps) a `trains` row and its children while a
leg is bound to it, but legs that lost their train before that only got the
timetable back as a snapshot (`docs/leg-snapshot-restore.md`). pgBackRest
backups still hold the originals, so a scratch Postgres restored to a point
before the prune has them.

It writes SQL for someone else to apply. It reads the scratch read-only
(`SET default_transaction_read_only = on`, `BEGIN READ ONLY`), never writes
to any database, and prints counts only (opaque subscription ids with
`--verbose`; never uids). Everything that names a train is written mode 0600.

## What is restored

For each target train, with its original ids:

| table | rows | identity |
|---|---|---|
| `trains` | 1 | `id` |
| `train_movement_events` | about 18 | `id` (`bigserial`; also unique on `(trains_id, dedup_key)`) |
| `train_current_state` | 0 or 1 | `id` (`bigserial`; unique on `trains_id`) |
| `train_reasons` | 0 to 3 | primary key `(trains_id, msg_type)` |
| `train_service_labels` | 0 or 1 | primary key `trains_id` |

and `train_subscriptions.trains_id` relinked. Every column is carried (`to_jsonb`
of the row, re-inserted by `jsonb_populate_record` with explicit column
lists), so `timestamptz` values keep their instant to the microsecond and
text, `text[]` and `jsonb` need no quoting (the documents are dollar-quoted).
No table has a generated or identity column (checked on a database built from
all of ds-core's migrations up to `20261120130200`), so every column is
written. The scratch (Nov 2026 migrations) and the live schema are identical
for these tables: the live schema only adds `journey_legs` columns. The
script holds the column lists, rejects a scratch row whose keys differ, and
the batch aborts if the live column sets differ (`schema differs`). Its test
schema is `scripts/tests/fixtures/pitr-train-schema.sql`, `pg_dump -s` of
those migrations.

Sequences need no `setval`: restored ids are far below the live sequences.
The batch asserts it per table, because each table has its own sequence and
very different id ranges (on production `max(trains.id)` is about 8.3 million
while event ids reach 20 million): the `DO $guard$` block at the top of every
batch has three separate checks, each comparing the live `max(id)` of one table
with the highest id restored into that same table (`trains`,
`train_movement_events`, `train_current_state`), each with its own message
(`trains ids are not above the restored trains ids`, `event ids ...`,
`state ids ...`). Never compare one table's max with another table's ids (the
first version did, and refused to run on production). `preflight.sql` reports
the same three checks as `*_ids_ok`.

## Guards (the batch never overwrites)

Per train, one INSERT statement (data-modifying CTEs; the children read the
CTE of the train row actually inserted, so a train and its children go in
together or not at all). The train is skipped when:

* a live `trains` row has its `id` (id collision: different content, or an
  earlier run already restored it),
* a live row has the same `(train_uid, service_date)` or `(train_id,
  service_date)` (a re-tracked train made a fresh row: no duplicate),
* an event id or its current-state id is taken in the live tables.

The one UPDATE of an existing row is the relink:

```sql
UPDATE train_subscriptions SET trains_id = <id>
WHERE id = <sub> AND trains_id IS NULL AND service_date = '<date>'
  AND EXISTS (SELECT 1 FROM trains x WHERE x.id = <id>
              AND x.train_uid = '<uid>' AND x.service_date = '<date>');
```

so a skipped train (live row exists, id collision) relinks nothing, and a
re-run is a no-op. A subscription that already has a train, or a different
service date, is left alone. Children use `ON CONFLICT DO NOTHING` as well.

## Matching

A target `{"subscriptionId", "trainUid", "serviceDate", "target": "T1"|"T2"}`
is the `trains` row with that `(train_uid, service_date)`. None: counted
`no_match`. More than one (the unique constraint forbids it; checked anyway):
`ambiguous`. A train whose rows contain a NUL byte, whose columns are not the
known ones, or whose children do not point at it: `rejected`. All are skipped
and counted, never guessed.

## Older backups

A backup from before `train_service_labels` existed (the 2026-11-09 one, T1)
has no such table. The extract script cannot name a missing table in its
query, so its read-only session first runs
`SELECT to_regclass('public.train_service_labels') IS NOT NULL ... \gset` and
puts the label subquery in a psql variable: the real subquery when the table
exists (same result as before), `NULL::jsonb` when it does not. Every train
then has no label, the plan counts 0 labels to insert, the batch has no label
INSERT and `verify.sql` expects 0 labels. The batch's schema guard still
checks the LIVE column sets, which do have the table.

## Running it, per target

T1 is the scratch restored at 2026-11-09T23:50Z, T2 at 2026-11-18T23:50Z.
`targets.jsonl` has one line per leg; `--target` selects the lines for the
scratch you are pointing at. Per target (the `--psql` prefix reaches that
scratch; the script appends `-v ON_ERROR_STOP=1` and sends SQL on stdin):

```sh
python3 -I scripts/extract-pitr-trains.py extract --targets targets.jsonl \
    --target T1 --out extract-T1.jsonl --emit-sql sql-T1 \
    --psql 'kubectl exec -i <scratch-pod> -- psql -X -At -U postgres -d distant_signal'
python3 -I scripts/extract-pitr-trains.py plan --extract extract-T1.jsonl
```

`extract` prints counts (`extracted`, `no_match`, `ambiguous`, `rejected`,
and what would be inserted per table). `emit` and `preflight` re-write the SQL
from an extract file (`emit --extract ... --emit-sql DIR`, `preflight
--extract ... --out preflight.sql`); the directory has `batch-0001.sql` (one
transaction per `--batch-size` trains, default 25, with `lock_timeout` 5s and
`statement_timeout` 120s), `preflight.sql`, `verify.sql`, `rollback.sql` and
`manifest.json` (counts, sizes, SHA-256s; no ids).

On the live database, as a role that can write these tables:

1. `psql -X -At -f preflight.sql` (read-only; counts only). First row:
   `trains`, `already_restored`, `id_collision`, `live_row_exists`,
   `child_id_taken` (these can overlap), then `subscriptions`,
   `subscriptions_relinkable`, `subscriptions_already_linked`,
   `subscriptions_missing`. Second row: every `*_ok` must be `t`. A train in
   any hazard column is skipped by the batch; investigate before applying.
2. `psql -X -v ON_ERROR_STOP=1 -f batch-0001.sql` for each batch, in order.
   A failed batch rolls back whole. Re-running is safe.
3. `psql -X -At -f verify.sql`: each `*_identical` equals its `*_expected`
   (row for row, `to_jsonb(live) = extracted`), except for trains the
   preflight reported as skipped; `subscriptions_linked` equals
   `subscriptions_expected`.

Every row comparison in `verify.sql` is looked up by primary key first
(`x.id = ...`, `x.trains_id = ... AND x.msg_type = ...`) and only then
compared as jsonb. An unkeyed `WHERE to_jsonb(x) = <row>` converts every row
of the table for every extracted row: on 8 million events it ran for hours and
timed out. A unit test fails on any `to_jsonb(x) =` in the generated
`verify.sql` or `preflight.sql` without a key predicate on the same alias, and
a database test checks `verify.sql` runs in under 2 seconds with 200,000
events in the table. `preflight.sql` compares by key only.

Before sealing anything, run the generated `preflight.sql` and `verify.sql`,
and the guard block of a batch (read-only: `BEGIN; ... ROLLBACK;`, or
just the `DO` block) against the LIVE database. Both of the first-run
failures (the guard refusing to run, `verify.sql` timing out) only show on
production-sized live data, never on the small test databases.

Run T1 and T2 separately. A train present in both would be restored by the
first and left alone by the second.

## Rollback

`rollback.sql` unlinks the subscriptions it linked and deletes the restored
trains by id, uid and date (children cascade; a train another subscription
points at is kept). It also removes anything written under those rows since,
so run it soon after, or not at all. The leg snapshots from DS-638 stay, so
the legs still render from them.

## Size

About 18 events per train. A train document is mostly the `calling_points`
and event `raw_body` jsonb: expect roughly 10 to 40 KB per train, so 9 trains
are well under 1 MB of batch SQL, and `verify.sql` embeds the same data again.

## Hand-off

The SQL names trains and subscriptions, and reading the scratch needs
cluster access, so the recommendation is a sealed one-off Job (as for the
`8tsd` leg restore): the owner runs `extract` against each scratch pod from
their machine (read-only), reviews `plan` and `preflight`, and the batches
are applied by a Job that mounts them from a sealed Secret and runs `psql`
with the api's role, then `verify.sql`. Do not apply before the aggregator
pin (ds-workers #58) is deployed, or the restored trains can be pruned again
at once. The backups this depends on expire on 2026-11-29: extract before
then; the extract files carry everything needed afterwards.
