//! The stage 3 backfill of the secret box AAD rollout (DS-615, ds-core's
//! docs/secret-box-aad.md; the `backfill_secret_box` binary runs it): every
//! `webhook_destinations` secret still sealed in the legacy format (v1, a
//! 12-byte nonce) is opened and sealed again in v2, bound to its row id.
//!
//! The rules it keeps:
//!
//! - **Refuses to run** unless `webhook_destinations_secret_nonce_check` is
//!   validated and allows a 13-byte nonce ([`require_relaxed_constraint`]):
//!   a v2 value would otherwise violate it.
//! - **Compare and swap.** A row is rewritten only while its ciphertext,
//!   nonce and key id are still what was read, so a concurrent change is
//!   never overwritten by an older value. Zero rows updated means a
//!   concurrent write (or delete) won, counted as `lost_race`.
//! - **Never destroys a secret.** The only write is that UPDATE of the three
//!   sealed columns to a value that was opened again before the write. A row
//!   that does not open (a retired key missing from the ring, a changed
//!   value) is left untouched, counted and reported by id.
//! - **Idempotent and resumable**: only 12-byte rows are selected, keyset
//!   paged by id; a second run finds nothing to do.
//! - **Counts only.** Nothing here logs or returns a plaintext, ciphertext,
//!   nonce or key.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use ds_runtime::secret_box::{Format, KeyRing};
use sqlx::PgPool;

use crate::data::webhooks::secret_context;

/// The constraint the stage 2 migrations relax.
pub const NONCE_CHECK: &str = "webhook_destinations_secret_nonce_check";

/// Rows per batch, by default; the most a caller may ask for.
pub const DEFAULT_BATCH_SIZE: i64 = 200;
pub const MAX_BATCH_SIZE: i64 = 1000;

/// The format of a stored nonce is its length (ds-core's secret box).
const LEGACY_NONCE_LEN: i32 = 12;
const V2_NONCE_LEN: i32 = 13;

/// Rows by the stored nonce's length, the version of their format. Nothing
/// of a ciphertext is inspected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Progress {
    /// 12-byte nonce: still legacy (v1).
    pub legacy: i64,
    /// 13-byte nonce: v2.
    pub v2: i64,
    /// Any other nonce length (the table's CHECK allows none).
    pub other: i64,
    /// Destinations without a secret.
    pub no_secret: i64,
}

/// Counts [`Progress`]. The same figures as the runbook's SQL.
pub async fn progress(pool: &PgPool) -> Result<Progress> {
    let (legacy, v2, other, no_secret): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE secret_ciphertext IS NOT NULL \
                                   AND octet_length(secret_nonce) = $1), \
                count(*) FILTER (WHERE secret_ciphertext IS NOT NULL \
                                   AND octet_length(secret_nonce) = $2), \
                count(*) FILTER (WHERE secret_ciphertext IS NOT NULL \
                                   AND octet_length(secret_nonce) NOT IN ($1, $2)), \
                count(*) FILTER (WHERE secret_ciphertext IS NULL) \
         FROM webhook_destinations",
    )
    .bind(LEGACY_NONCE_LEN)
    .bind(V2_NONCE_LEN)
    .fetch_one(pool)
    .await
    .context("could not count the destinations by secret format")?;
    Ok(Progress {
        legacy,
        v2,
        other,
        no_secret,
    })
}

/// Whether a nonce CHECK's definition (`pg_get_constraintdef`) and validity
/// say a 13-byte nonce is accepted. Postgres renders the migration's
/// `IN (12, 13)` as `= ANY (ARRAY[12, 13])`; the old check was `= 12`.
pub fn nonce_check_allows_v2(definition: &str, validated: bool) -> bool {
    let numbers: Vec<&str> = definition
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .collect();
    validated
        && numbers.contains(&"12")
        && numbers.contains(&"13")
        && definition.contains("octet_length(secret_nonce)")
}

/// Errors, saying why, unless the nonce CHECK is validated and allows 12 and
/// 13 bytes (ds-core v0.18.2's migrations are applied).
pub async fn require_relaxed_constraint(pool: &PgPool) -> Result<()> {
    let row: Option<(String, bool)> = sqlx::query_as(
        "SELECT pg_get_constraintdef(oid), convalidated FROM pg_constraint \
         WHERE conrelid = 'webhook_destinations'::regclass AND conname = $1",
    )
    .bind(NONCE_CHECK)
    .fetch_optional(pool)
    .await
    .context("could not read the nonce constraint")?;
    match row {
        Some((definition, validated)) if nonce_check_allows_v2(&definition, validated) => Ok(()),
        Some((_, false)) => bail!(
            "refusing to run: {NONCE_CHECK} is not validated yet. The stage 2 migrations \
             (ds-core v0.18.2, 20261125100000 and 20261125100100) must be applied first"
        ),
        Some((_, true)) => bail!(
            "refusing to run: {NONCE_CHECK} does not allow a 13-byte nonce, so a v2 value \
             would be refused by the database. Apply ds-core v0.18.2's stage 2 migrations first"
        ),
        None => bail!(
            "refusing to run: {NONCE_CHECK} was not found on webhook_destinations, so the \
             migration state cannot be confirmed"
        ),
    }
}

/// How a run behaves.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Rows per batch (and per transaction), 1 to [`MAX_BATCH_SIZE`].
    pub batch_size: i64,
    /// Sleep after each batch, so the run does not hog the database.
    pub pause: Duration,
    /// Open every legacy row and say what would change, writing nothing.
    pub dry_run: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            batch_size: DEFAULT_BATCH_SIZE,
            pause: Duration::from_millis(100),
            dry_run: false,
        }
    }
}

/// What a pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// Legacy rows seen.
    pub scanned: u64,
    /// Rows rewritten as v2.
    pub converted: u64,
    /// Rows a dry run would rewrite (they opened).
    pub would_convert: u64,
    /// Rows changed or deleted by someone else between the read and the
    /// write: left as they now are.
    pub lost_race: u64,
    /// Ids of rows that did not open (or whose new value did not open again):
    /// untouched. Ids only, never a value.
    pub unopenable: Vec<i64>,
}

type LegacyRow = (i64, Vec<u8>, Vec<u8>, String);

/// One pass over every legacy row, in id order.
pub async fn run_pass(pool: &PgPool, ring: &KeyRing, options: Options) -> Result<Report> {
    if !(1..=MAX_BATCH_SIZE).contains(&options.batch_size) {
        bail!("the batch size must be between 1 and {MAX_BATCH_SIZE}");
    }
    let mut report = Report::default();
    let mut last = 0_i64;
    loop {
        let mut tx = pool.begin().await?;
        let rows: Vec<LegacyRow> = sqlx::query_as(
            "SELECT id, secret_ciphertext, secret_nonce, secret_key_id \
             FROM webhook_destinations \
             WHERE secret_ciphertext IS NOT NULL AND octet_length(secret_nonce) = $1 \
               AND id > $2 \
             ORDER BY id LIMIT $3",
        )
        .bind(LEGACY_NONCE_LEN)
        .bind(last)
        .bind(options.batch_size)
        .fetch_all(&mut *tx)
        .await
        .context("could not read a batch of legacy destinations")?;
        let Some(&(last_id, ..)) = rows.last() else {
            tx.rollback().await?;
            return Ok(report);
        };
        for (id, ciphertext, nonce, key_id) in rows {
            report.scanned += 1;
            let Some(sealed) = convert(ring, id, &ciphertext, &nonce, &key_id) else {
                tracing::warn!(
                    destination_id = id,
                    "a legacy secret does not open with the key ring; left untouched"
                );
                report.unopenable.push(id);
                continue;
            };
            if options.dry_run {
                report.would_convert += 1;
                continue;
            }
            let swapped = swap(
                &mut tx,
                id,
                &Stored {
                    ciphertext,
                    nonce,
                    key_id,
                },
                &sealed,
            )
            .await
            .with_context(|| format!("could not rewrite destination {id}"))?;
            if swapped {
                report.converted += 1;
            } else {
                report.lost_race += 1;
            }
        }
        tx.commit().await?;
        last = last_id;
        tracing::info!(
            scanned = report.scanned,
            converted = report.converted,
            lost_race = report.lost_race,
            unopenable = report.unopenable.len(),
            "backfill batch done"
        );
        if !options.pause.is_zero() {
            tokio::time::sleep(options.pause).await;
        }
    }
}

/// A secret's three columns as read.
struct Stored {
    ciphertext: Vec<u8>,
    nonce: Vec<u8>,
    key_id: String,
}

/// The compare and swap: writes `new` over destination `id` only while its
/// columns are still `expected`. Whether it did; `false` means someone else
/// changed or deleted the row since it was read, and it is left as it is.
async fn swap(
    tx: &mut sqlx::PgConnection,
    id: i64,
    expected: &Stored,
    new: &ds_runtime::secret_box::Sealed,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE webhook_destinations \
         SET secret_ciphertext = $1, secret_nonce = $2, secret_key_id = $3 \
         WHERE id = $4 AND secret_ciphertext = $5 AND secret_nonce = $6 \
           AND secret_key_id = $7",
    )
    .bind(new.ciphertext.as_slice())
    .bind(new.nonce.as_slice())
    .bind(new.key_id.as_str())
    .bind(id)
    .bind(expected.ciphertext.as_slice())
    .bind(expected.nonce.as_slice())
    .bind(expected.key_id.as_str())
    .execute(tx)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// Opens a legacy value and seals it again as v2 for row `id` with the active
/// key, checking the new value opens before it is returned. `None` if either
/// step fails.
fn convert(
    ring: &KeyRing,
    id: i64,
    ciphertext: &[u8],
    nonce: &[u8],
    key_id: &str,
) -> Option<ds_runtime::secret_box::Sealed> {
    let context = secret_context(id);
    let old = ds_runtime::secret_box::Sealed {
        ciphertext: ciphertext.to_vec(),
        nonce: nonce.to_vec(),
        key_id: key_id.to_string(),
    };
    let plaintext = ring.open_in(&old, &context).ok()?;
    let new = ring.seal_as(&plaintext, &context, Format::V2).ok()?;
    let again = ring.open_in(&new, &context).ok()?;
    (again == plaintext && new.nonce.len() == 13).then_some(new)
}

/// A whole run: the gate, then passes until one loses no race (at most
/// `MAX_PASSES`), and the final counts. `Err` when a row does not open or a
/// legacy row remains afterwards (outside a dry run), so the Job fails.
pub async fn run(pool: &PgPool, ring: &KeyRing, options: Options) -> Result<Report> {
    const MAX_PASSES: u32 = 3;
    require_relaxed_constraint(pool).await?;
    let before = progress(pool).await?;
    tracing::info!(
        legacy = before.legacy,
        v2 = before.v2,
        other = before.other,
        no_secret = before.no_secret,
        dry_run = options.dry_run,
        "secret format before the backfill"
    );
    let mut total = Report::default();
    for pass in 1..=MAX_PASSES {
        let report = run_pass(pool, ring, options).await?;
        let lost = report.lost_race;
        if pass == 1 {
            total.scanned = report.scanned;
        }
        total.converted += report.converted;
        total.would_convert = report.would_convert;
        total.lost_race += report.lost_race;
        total.unopenable = report.unopenable;
        if lost == 0 {
            break;
        }
    }
    let after = progress(pool).await?;
    tracing::info!(
        legacy = after.legacy,
        v2 = after.v2,
        other = after.other,
        no_secret = after.no_secret,
        converted = total.converted,
        would_convert = total.would_convert,
        lost_race = total.lost_race,
        unopenable = total.unopenable.len(),
        "secret format after the backfill"
    );
    if !total.unopenable.is_empty() {
        bail!(
            "{} legacy secret(s) did not open and were left untouched (destination ids {:?}); \
             check WEBHOOK_SECRET_KEYS has every key they were sealed with",
            total.unopenable.len(),
            total.unopenable
        );
    }
    if !options.dry_run && after.legacy > 0 {
        bail!(
            "{} legacy secret(s) remain (rows changed during the run, or created since); \
             run it again",
            after.legacy
        );
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use ds_runtime::secret_box::Sealed;
    use sqlx::PgPool;

    use super::*;

    // 32 bytes of base64: 43 characters and a pad, the last one with clear
    // trailing bits.
    const KEY_A: &str = concat!("k1:", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "=");
    const KEY_B: &str = concat!("k2:", "QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ", "=");

    fn key_ring(spec: &str) -> KeyRing {
        KeyRing::parse(spec).expect("a usable key ring")
    }

    #[test]
    fn the_constraint_text_is_read() {
        let relaxed = "CHECK (((secret_nonce IS NULL) OR (octet_length(secret_nonce) = ANY (ARRAY[12, 13]))))";
        let old = "CHECK (((secret_nonce IS NULL) OR (octet_length(secret_nonce) = 12)))";
        let v2_only = "CHECK (((secret_nonce IS NULL) OR (octet_length(secret_nonce) = 13)))";
        assert!(nonce_check_allows_v2(relaxed, true));
        assert!(!nonce_check_allows_v2(relaxed, false), "not validated");
        assert!(!nonce_check_allows_v2(old, true));
        assert!(!nonce_check_allows_v2(v2_only, true), "stage 4 is not this");
        assert!(!nonce_check_allows_v2(
            "CHECK (a = ANY (ARRAY[12, 13]))",
            true
        ));
    }

    #[test]
    fn a_legacy_value_converts_to_v2_for_its_row_and_only_its_row() {
        let ring = key_ring(KEY_A);
        let legacy = ring.seal("whsec_x").unwrap();
        let sealed = convert(&ring, 7, &legacy.ciphertext, &legacy.nonce, &legacy.key_id).unwrap();
        assert_eq!(sealed.nonce.len(), 13);
        assert_eq!(sealed.nonce[0], 2);
        assert_eq!(
            ring.open_in(&sealed, &secret_context(7)).unwrap(),
            "whsec_x"
        );
        assert!(ring.open_in(&sealed, &secret_context(8)).is_err());
    }

    #[test]
    fn a_value_that_does_not_open_does_not_convert() {
        let sealer = key_ring(KEY_B);
        let legacy = sealer.seal("whsec_x").unwrap();
        // The ring lacks key k2.
        let missing = key_ring(KEY_A);
        assert!(
            convert(
                &missing,
                1,
                &legacy.ciphertext,
                &legacy.nonce,
                &legacy.key_id
            )
            .is_none()
        );
        // A changed ciphertext.
        let mut tampered: Sealed = sealer.seal("whsec_x").unwrap();
        tampered.ciphertext[0] ^= 1;
        assert!(
            convert(
                &sealer,
                1,
                &tampered.ciphertext,
                &tampered.nonce,
                &tampered.key_id
            )
            .is_none()
        );
    }

    #[test]
    fn a_conversion_rotates_to_the_active_key() {
        let old = key_ring(KEY_A).seal("whsec_x").unwrap();
        let both = key_ring(&format!("{KEY_B},{KEY_A}"));
        let sealed = convert(&both, 3, &old.ciphertext, &old.nonce, &old.key_id).unwrap();
        assert_eq!(sealed.key_id, "k2");
        assert_eq!(
            both.open_in(&sealed, &secret_context(3)).unwrap(),
            "whsec_x"
        );
    }

    async fn user(pool: &PgPool) {
        sqlx::query("INSERT INTO users (id) VALUES ('TEST-USER-BACKFILL')")
            .execute(pool)
            .await
            .expect("insert user");
    }

    /// Inserts a destination, sealing `secret` as `format` for its own id.
    async fn destination(pool: &PgPool, ring: &KeyRing, n: u32, format: Format) -> i64 {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO webhook_destinations (user_id, kind, name, url) \
             VALUES ('TEST-USER-BACKFILL', 'generic', 'n', $1) RETURNING id",
        )
        .bind(format!("https://{n}.example/"))
        .fetch_one(pool)
        .await
        .expect("insert destination");
        let sealed = ring
            .seal_as(&format!("whsec_{n}"), &secret_context(id), format)
            .unwrap();
        write(pool, id, &sealed).await;
        id
    }

    async fn write(pool: &PgPool, id: i64, sealed: &Sealed) {
        sqlx::query(
            "UPDATE webhook_destinations \
             SET secret_ciphertext = $1, secret_nonce = $2, secret_key_id = $3 WHERE id = $4",
        )
        .bind(sealed.ciphertext.as_slice())
        .bind(sealed.nonce.as_slice())
        .bind(sealed.key_id.as_str())
        .bind(id)
        .execute(pool)
        .await
        .expect("write the secret");
    }

    async fn stored(pool: &PgPool, id: i64) -> Sealed {
        let (ciphertext, nonce, key_id): (Vec<u8>, Vec<u8>, String) = sqlx::query_as(
            "SELECT secret_ciphertext, secret_nonce, secret_key_id \
             FROM webhook_destinations WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("read the secret");
        Sealed {
            ciphertext,
            nonce,
            key_id,
        }
    }

    fn quick() -> Options {
        Options {
            batch_size: 2,
            pause: Duration::ZERO,
            dry_run: false,
        }
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn legacy_rows_become_v2_and_a_second_run_does_nothing(pool: PgPool) {
        let ring = key_ring(KEY_A);
        user(&pool).await;
        let mut ids = Vec::new();
        for n in 0..5 {
            ids.push(destination(&pool, &ring, n, Format::Legacy).await);
        }
        let already_v2 = destination(&pool, &ring, 9, Format::V2).await;
        let v2_before = stored(&pool, already_v2).await;
        // A destination with no secret stays that way.
        sqlx::query(
            "INSERT INTO webhook_destinations (user_id, kind, name, url) \
             VALUES ('TEST-USER-BACKFILL', 'ntfy', 'n', 'https://none.example/')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let first = run(&pool, &ring, quick()).await.unwrap();
        assert_eq!((first.scanned, first.converted), (5, 5));
        for (n, id) in ids.iter().enumerate() {
            let sealed = stored(&pool, *id).await;
            assert_eq!(sealed.nonce.len(), 13);
            assert_eq!(
                ring.open_in(&sealed, &secret_context(*id)).unwrap(),
                format!("whsec_{n}")
            );
        }
        assert_eq!(
            stored(&pool, already_v2).await,
            v2_before,
            "v2 rows untouched"
        );
        let done = progress(&pool).await.unwrap();
        assert_eq!(
            done,
            Progress {
                legacy: 0,
                v2: 6,
                other: 0,
                no_secret: 1
            }
        );

        let second = run(&pool, &ring, quick()).await.unwrap();
        assert_eq!((second.scanned, second.converted), (0, 0));
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_dry_run_writes_nothing(pool: PgPool) {
        let ring = key_ring(KEY_A);
        user(&pool).await;
        let id = destination(&pool, &ring, 1, Format::Legacy).await;
        let before = stored(&pool, id).await;
        let options = Options {
            dry_run: true,
            ..quick()
        };
        let report = run(&pool, &ring, options).await.unwrap();
        assert_eq!(
            (report.scanned, report.would_convert, report.converted),
            (1, 1, 0)
        );
        assert_eq!(stored(&pool, id).await, before);
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_concurrent_change_is_not_overwritten(pool: PgPool) {
        let ring = key_ring(KEY_A);
        user(&pool).await;
        let id = destination(&pool, &ring, 1, Format::Legacy).await;
        let legacy = stored(&pool, id).await;
        // The row is read as `legacy`, then someone writes a new value before
        // the backfill's UPDATE: its compare-and-swap matches nothing.
        let newer = ring
            .seal_as("whsec_newer", &secret_context(id), Format::V2)
            .unwrap();
        write(&pool, id, &newer).await;
        let sealed = convert(&ring, id, &legacy.ciphertext, &legacy.nonce, &legacy.key_id).unwrap();
        let expected = Stored {
            ciphertext: legacy.ciphertext.clone(),
            nonce: legacy.nonce.clone(),
            key_id: legacy.key_id.clone(),
        };
        let mut conn = pool.acquire().await.unwrap();
        assert!(!swap(&mut conn, id, &expected, &sealed).await.unwrap());
        assert_eq!(stored(&pool, id).await, newer);
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn a_row_that_does_not_open_fails_the_run_and_is_not_touched(pool: PgPool) {
        let sealer = key_ring(KEY_B);
        let ring = key_ring(KEY_A);
        user(&pool).await;
        let good = destination(&pool, &ring, 1, Format::Legacy).await;
        let orphan = destination(&pool, &sealer, 2, Format::Legacy).await;
        let orphan_before = stored(&pool, orphan).await;

        let err = run(&pool, &ring, quick()).await.unwrap_err().to_string();
        assert!(err.contains(&orphan.to_string()), "{err}");
        assert!(!err.contains("whsec_"), "no secret in the error");
        assert_eq!(stored(&pool, orphan).await, orphan_before, "left as it was");
        assert_eq!(
            stored(&pool, good).await.nonce.len(),
            13,
            "the others still convert"
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM webhook_destinations WHERE secret_ciphertext IS NOT NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 2, "nothing was nulled or deleted");
    }

    #[sqlx::test(migrator = "ds_store::migrate::MIGRATOR")]
    #[ignore = "needs DATABASE_URL (a role that can create databases)"]
    async fn it_refuses_while_the_constraint_forbids_a_13_byte_nonce(pool: PgPool) {
        let ring = key_ring(KEY_A);
        require_relaxed_constraint(&pool)
            .await
            .expect("relaxed by the migrations");
        // As before the stage 2 migrations.
        sqlx::query(&format!(
            "ALTER TABLE webhook_destinations DROP CONSTRAINT {NONCE_CHECK}, \
             ADD CONSTRAINT {NONCE_CHECK} \
             CHECK (secret_nonce IS NULL OR octet_length(secret_nonce) = 12)"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let err = run(&pool, &ring, quick()).await.unwrap_err().to_string();
        assert!(err.contains("refusing to run"), "{err}");
        assert!(err.contains("13-byte"), "{err}");
        // Relaxed but not yet validated.
        sqlx::query(&format!(
            "ALTER TABLE webhook_destinations DROP CONSTRAINT {NONCE_CHECK}, \
             ADD CONSTRAINT {NONCE_CHECK} \
             CHECK (secret_nonce IS NULL OR octet_length(secret_nonce) IN (12, 13)) NOT VALID"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let err = run(&pool, &ring, quick()).await.unwrap_err().to_string();
        assert!(err.contains("not validated"), "{err}");
    }
}
