# Backfilling webhook secrets to the v2 sealed format (DS-615, stage 3)

`backfill_secret_box` re-seals every `webhook_destinations` secret that is
still in the legacy format (v1) as v2, bound to its table, column and row id.
The rollout and the formats are ds-core's
[docs/secret-box-aad.md](https://github.com/Distant-Signal/ds-core/blob/main/docs/secret-box-aad.md);
this is the runbook for stage 3. Code: `crates/api/src/bin/backfill_secret_box.rs`
and `crates/api/src/data/secret_box_backfill.rs`.

It is a binary of the api image (like `maintenance`), not a `ds-api`
subcommand: nothing starts it. It does nothing until an operator runs it as a
one-off Job.

## Preconditions

Run it only when all of these hold. The Job checks the first itself; the rest
are on the operator, and the run refuses without `--confirm-v2-readers`.

1. ds-core v0.18.2's stage 2 migrations are applied
   (`webhook_destinations_secret_nonce_check` is validated and allows 12 or
   13 bytes). The Job reads `pg_constraint` and **refuses to run**, saying
   why, if the check is missing, not validated, or still `= 12`: a v2 value
   would be refused by the database.
2. Every reader of the secret columns opens v2: the api and the notifier in
   production are on builds that call `open_in` (the api from ds-api #82, the
   notifier from ds-workers #47), and no older Job or replica remains.
   Check the chart's pinned digests, not tags. A reader on older code reads a
   v2 value as undecryptable (the notifier skips the destination and counts
   `notifier_webhook_send_total{outcome="undecryptable"}`).
3. The write format flip (ds-core `DEFAULT_WRITE_FORMAT = V2`, then the api
   bump) is deployed, or about to be. The backfill does not depend on it (it
   writes v2 explicitly), but while the api still writes v1 new legacy rows
   keep appearing and the run ends non-zero for them. Run it after the flip.
4. The Job has the api's `DATABASE_URL` (the api role: SELECT and UPDATE on
   `webhook_destinations`; no grant change is needed) and the api's
   `WEBHOOK_SECRET_KEYS`, which must hold **every key a legacy secret was
   sealed with**. The first key is the active one: converted rows are sealed
   with it, which also rotates them onto it.

## What it does

- Selects legacy rows (`secret_ciphertext IS NOT NULL AND
  octet_length(secret_nonce) = 12`), keyset-paged by `id`, `--batch-size`
  rows (default 200) per transaction, sleeping `--pause-ms` (default 100)
  between batches.
- Opens each value (v1), seals it again as v2 for its row id, opens the new
  value to check it, then updates with a compare and swap: the row is
  written only if its ciphertext, nonce and key id are still what was read.
  Zero rows updated means someone else changed or deleted it meanwhile; it is
  left as it now is and counted as `lost_race`. It never overwrites a newer
  value with an older one.
- A row that does not open (a key missing from the ring, or a changed value)
  is left untouched, logged by id, and fails the run. It never deletes or
  nulls a secret; the only statement that writes is that compare and swap.
- Repeats the pass (at most three) while races were lost. Safe to re-run at
  any time: a second run finds no legacy rows.
- Logs counts and destination ids only. No secret, key, ciphertext or nonce
  is printed or returned.

Flags (each also an environment variable): `--confirm-v2-readers`
(`BACKFILL_CONFIRM_V2_READERS`, required), `--dry-run` (`BACKFILL_DRY_RUN`),
`--batch-size` (`BACKFILL_BATCH_SIZE`, 1 to 1000), `--pause-ms`
(`BACKFILL_PAUSE_MS`), `--connect-deadline-secs`
(`BACKFILL_CONNECT_DEADLINE_SECS`).

## Running it

1. **Dry run first**: the same Job with `--dry-run`. It opens every legacy row
   and writes nothing, so it shows whether the key ring can open them all.
2. Run it for real: a one-off Job from the api image, the api's environment
   (`DATABASE_URL`, `WEBHOOK_SECRET_KEYS`), command
   `backfill_secret_box --confirm-v2-readers`, `restartPolicy: Never`.

Expected log lines (JSON in the cluster), the numbers being counts:

```text
webhook secret keys loaded            active_key="<id>"
secret format before the backfill     legacy=N v2=M other=0 no_secret=K dry_run=false
backfill batch done                   scanned=200 converted=200 lost_race=0 unopenable=0
secret format after the backfill      legacy=0 v2=N+M other=0 no_secret=K converted=N lost_race=0 unopenable=0
```

Exit 0 means no legacy row remains. Non-zero means one of:

- `refusing to run: ... nonce_check ...`: stage 2 is not applied; nothing was
  read or written.
- `refusing to run: pass --confirm-v2-readers`.
- `N legacy secret(s) did not open ... (destination ids [...])`: those rows
  are untouched. Add the missing key to `WEBHOOK_SECRET_KEYS` and re-run, or
  decide what to do with the destination (the user can re-create it).
- `N legacy secret(s) remain`: rows changed or were created during the run;
  run it again.

## Verifying

Progress is the stored nonce's length (the version); no byte of a
ciphertext is inspected. As the schema owner or the api role:

```sql
SELECT count(*) FILTER (WHERE octet_length(secret_nonce) = 12) AS legacy,
       count(*) FILTER (WHERE octet_length(secret_nonce) = 13) AS v2
FROM webhook_destinations WHERE secret_ciphertext IS NOT NULL;
```

Do not use a first-byte test such as `substr(secret_nonce, 1, 1) = '\x02'`:
a legacy nonce is 12 random bytes, so about 1 in 256 legacy rows would match.

Stage 3 is done when `legacy = 0` and
`distant_signal_secret_box_legacy_opens_total` (the api and the notifier) has
not increased for at least a week. Also check
`notifier_webhook_send_total{outcome="undecryptable"}` stays flat after a run,
and send a test delivery to a destination of each kind. Stage 4 (removing the
legacy format and tightening the CHECK to `= 13`) waits on both.

## Rollback

A converted row is still a valid sealed value for readers on 1b code, so
there is nothing to undo while those readers run. If a reader that cannot
open v2 is found afterwards, roll that reader forward; do not try to
convert rows back (the legacy format is kept only for reads). The Job never
changes the key ring, so a wrong `WEBHOOK_SECRET_KEYS` shows up as a failed
dry run before any write, and a rotated-to key is the first key of the ring
the readers also have.
