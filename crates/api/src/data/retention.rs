//! Storage-limitation pruning of personal data (UK legal audit LEG-5,
//! 2026-09-27; UK GDPR Art. 5(1)(e)).
//!
//! Run hourly from `main.rs`'s `session_cleanup_sweep_loop`, beside the
//! existing expired-session and dead-link prunes. Every limit is a config
//! value (`ServiceArguments::past_travel_retention_days`,
//! `stale_push_subscription_days`, `inactive_account_retention_days`) and 0
//! disables that part. See `docs/personal-data-retention.md` for the full
//! schedule, including the parts pruned elsewhere (sessions, OIDC login
//! state, dead share/invite links).
//!
//! Deliberately NOT pruned by age: anything a user expects to keep while
//! they use the account -- journey templates (they recur), groups and
//! memberships, pins/preferences, custom lines, and upcoming or recent
//! travel.

use anyhow::Result;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::Serialize;
use sqlx::PgPool;

use crate::data::account;

/// The configured limits, in days. 0 disables that limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Tracked trains, standalone tickets, journeys and template skip
    /// markers whose travel date is more than this many days ago.
    pub past_travel_days: i64,
    /// Push subscriptions of users who have not logged in for this many
    /// days (and whose subscription was not renewed in that time either).
    pub stale_push_subscription_days: i64,
    /// Whole accounts with no login for this many days and no live session.
    pub inactive_account_days: i64,
    /// Regular-journey occurrences (journeys made from a template) whose
    /// last leg is more than this many days ago, when nothing on them
    /// matters for Delay Repay ([`prune_regular_occurrences`]; regular
    /// journeys design §4.2, decision 11).
    pub regular_journey_days: i64,
}

/// Rows deleted by one [`prune_personal_data`] run, for logging and tests.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct RetentionOutcome {
    pub journeys: u64,
    /// Regular-journey occurrences deleted by the shorter rule.
    pub regular_journeys: u64,
    pub tracked_trains: u64,
    pub standalone_tickets: u64,
    pub template_skipped_dates: u64,
    pub push_subscriptions: u64,
    pub accounts: u64,
    /// Journey follows whose share no longer lets the follower in (an
    /// expired share link, or a cleanup that raced the follow; DS-146).
    pub lapsed_follows: u64,
    /// `usage_daily` rows older than 13 months (opt-in usage analytics,
    /// DS-190). Not personal data, but kept no longer than it is useful.
    pub usage_daily_rows: u64,
    /// Train legs whose final arrival was recorded this run
    /// (`leg_outcomes::record_final_arrivals`). Not a deletion, so not in
    /// [`Self::total`].
    pub recorded_leg_outcomes: u64,
}

impl RetentionOutcome {
    pub fn total(&self) -> u64 {
        self.journeys
            + self.regular_journeys
            + self.tracked_trains
            + self.standalone_tickets
            + self.template_skipped_dates
            + self.push_subscriptions
            + self.accounts
            + self.lapsed_follows
            + self.usage_daily_rows
    }
}

/// How many inactive accounts one run deletes at most, so a first run
/// against a large backlog does not hold one long sweep. The rest go on the
/// next run.
const MAX_ACCOUNTS_PER_RUN: i64 = 100;

/// The London service date `days` before `now`'s own London date.
fn cutoff_date(now: DateTime<Utc>, days: i64) -> NaiveDate {
    now.with_timezone(&chrono_tz::Europe::London).date_naive() - Duration::days(days)
}

/// Applies `policy` as of `now`. Each part runs as its own statement(s);
/// one failing aborts the rest of this run, and the next hourly run
/// retries everything (every part is idempotent).
///
/// First, whatever the policy, it records the final arrival of every train
/// leg whose train is over (`leg_outcomes`, DS-379), while the train's data
/// is still held: the regular-journey rule reads it. Nothing is deleted
/// when that fails.
pub async fn prune_personal_data(
    pool: &PgPool,
    policy: RetentionPolicy,
    now: DateTime<Utc>,
) -> Result<RetentionOutcome> {
    let mut outcome = RetentionOutcome {
        recorded_leg_outcomes: crate::data::leg_outcomes::record_final_arrivals(
            pool,
            cutoff_date(now, 0),
        )
        .await?,
        ..RetentionOutcome::default()
    };

    if policy.past_travel_days > 0 {
        let date = cutoff_date(now, policy.past_travel_days);
        let instant = now - Duration::days(policy.past_travel_days);
        prune_past_travel(pool, date, instant, &mut outcome).await?;
    }

    if policy.regular_journey_days > 0 {
        let date = cutoff_date(now, policy.regular_journey_days);
        outcome.regular_journeys = prune_regular_occurrences(pool, date).await?;
    }

    if policy.stale_push_subscription_days > 0 {
        let cutoff = now - Duration::days(policy.stale_push_subscription_days);
        outcome.push_subscriptions = sqlx::query(
            "DELETE FROM push_subscriptions p USING users u \
             WHERE p.user_id = u.id AND u.last_login_at < $1 AND p.last_seen_at < $1",
        )
        .bind(cutoff)
        .execute(pool)
        .await?
        .rows_affected();
    }

    if policy.inactive_account_days > 0 {
        let cutoff = now - Duration::days(policy.inactive_account_days);
        let inactive: Vec<String> = sqlx::query_scalar(
            "SELECT u.id FROM users u \
             WHERE u.last_login_at < $1 \
               AND NOT EXISTS (SELECT 1 FROM sessions s WHERE s.user_id = u.id AND s.expires_at > $2) \
             ORDER BY u.last_login_at \
             LIMIT $3",
        )
        .bind(cutoff)
        .bind(now)
        .bind(MAX_ACCOUNTS_PER_RUN)
        .fetch_all(pool)
        .await?;
        for user_id in inactive {
            // Same path as self-service deletion, so group ownership is
            // handed over rather than the group vanishing.
            if account::delete_account(pool, &user_id).await?.is_some() {
                outcome.accounts += 1;
            }
        }
    }

    // Not a retention limit, so not switchable: a follow is only ever
    // honoured while its share is live (`journey_follows::VALID_FOLLOW`),
    // and a share link's expiry is the one lapse no request deletes.
    outcome.lapsed_follows = crate::data::journey_follows::prune_lapsed_follows(pool).await?;

    // Not switchable either: the 13 months the privacy page promises.
    outcome.usage_daily_rows = crate::data::usage::prune(pool, now).await?;

    Ok(outcome)
}

/// The lowest Delay Repay band: a train at least this late, or cancelled,
/// keeps its journey for the longer rule (`delay_repay_rules`, DR15).
pub const REGULAR_JOURNEY_KEEP_DELAY_MINUTES: i32 = 15;

/// How many occurrences one run deletes at most; the rest go next hour.
const MAX_REGULAR_OCCURRENCES_PER_RUN: i64 = 500;

/// Deletes regular-journey occurrences (journeys with `source_template_id`
/// set) with no leg dated on or after `date`, when nothing on them matters
/// for Delay Repay:
///
/// - no leg has ticket details (`data::leg_tickets`: a
///   `tracked_train_tickets` row on its train), on the current plan or on
///   a recorded plan's leg whose details are still kept
///   (`journey_revision_legs.ticket_ids`);
/// - no plan recorded a leg as cancelled, part cancelled or missed;
/// - no leg's train is known to have been cancelled or at least
///   [`REGULAR_JOURNEY_KEEP_DELAY_MINUTES`] late at the leg's destination,
///   as recorded on the leg (`final_train_cancelled`,
///   `final_arrival_delay_minutes`; `leg_outcomes`, DS-379), or by the
///   last push sent for it (`train_notification_state`);
/// - no leg is still waiting for its outcome to be recorded while its
///   train's data is held (the next pass records it first).
///
/// The rest wait for the 548-day rule, as does an occurrence whose
/// template was deleted (its `source_template_id` is then null). Each
/// journey's trains are released as deleting a journey releases them
/// (`journeys::delete_journey_row_releasing_trains`, DS-93), with its share
/// links.
///
/// A tracked train's live state is pruned after about 30 days (ds-workers'
/// `trainsRetentionDays`), which is why the outcome is recorded on the leg
/// first. Only a leg whose train was gone before the columns existed
/// (2026-11) has no recorded outcome; for those the push is the only
/// record.
pub async fn prune_regular_occurrences(pool: &PgPool, date: NaiveDate) -> Result<u64> {
    let doomed: Vec<(i64, String)> = sqlx::query_as(
        "SELECT j.id, j.user_id FROM journeys j \
         WHERE j.source_template_id IS NOT NULL \
           AND EXISTS (SELECT 1 FROM journey_legs l WHERE l.journey_id = j.id) \
           AND NOT EXISTS (SELECT 1 FROM journey_legs l \
                           WHERE l.journey_id = j.id AND l.service_date >= $1) \
           AND NOT EXISTS ( \
               SELECT 1 FROM journey_legs l \
               LEFT JOIN train_subscriptions ts ON ts.id = l.train_subscription_id \
               LEFT JOIN train_notification_state ns ON ns.tracked_train_id = ts.id \
               WHERE l.journey_id = j.id \
                 AND (l.final_train_cancelled \
                      OR l.final_arrival_delay_minutes >= $2 \
                      OR (l.final_recorded_at IS NULL AND ts.trains_id IS NOT NULL) \
                      OR EXISTS (SELECT 1 FROM tracked_train_tickets t \
                                 WHERE t.tracked_train_id = ts.id) \
                      OR ns.last_notified_status = 'cancelled' \
                      OR ns.last_notified_delay_minutes >= $2)) \
           AND NOT EXISTS ( \
               SELECT 1 FROM journey_revisions r \
               JOIN journey_revision_legs rl ON rl.revision_id = r.id \
               WHERE r.journey_id = j.id \
                 AND (EXISTS (SELECT 1 FROM tracked_train_tickets t \
                              WHERE t.id = ANY(rl.ticket_ids)) \
                      OR rl.fate IN ('cancelled', 'part_cancelled', 'missed'))) \
         ORDER BY j.id \
         LIMIT $3",
    )
    .bind(date)
    .bind(REGULAR_JOURNEY_KEEP_DELAY_MINUTES)
    .bind(MAX_REGULAR_OCCURRENCES_PER_RUN)
    .fetch_all(pool)
    .await?;
    let mut deleted = 0;
    for (journey_id, user_id) in doomed {
        let mut tx = pool.begin().await?;
        sqlx::query(
            "DELETE FROM unlisted_links WHERE resource_type = 'journey' AND resource_id = $1",
        )
        .bind(journey_id.to_string())
        .execute(&mut *tx)
        .await?;
        if crate::data::journeys::delete_journey_row_releasing_trains(&mut tx, journey_id, &user_id)
            .await?
        {
            deleted += 1;
        }
        tx.commit().await?;
    }
    Ok(deleted)
}

async fn prune_past_travel(
    pool: &PgPool,
    date: NaiveDate,
    instant: DateTime<Utc>,
    outcome: &mut RetentionOutcome,
) -> Result<()> {
    let mut tx = pool.begin().await?;

    // A journey is past when it has no leg on or after the cutoff date. A
    // leg-less journey (only ever transient, mid-creation) is judged by
    // its creation time instead. Its share links go with it: they are
    // polymorphic rows with no FK to journeys.
    let doomed: Vec<i64> = sqlx::query_scalar(
        "SELECT j.id FROM journeys j \
         WHERE j.created_at < $2 \
           AND NOT EXISTS ( \
               SELECT 1 FROM journey_legs l WHERE l.journey_id = j.id AND l.service_date >= $1)",
    )
    .bind(date)
    .bind(instant)
    .fetch_all(&mut *tx)
    .await?;
    if !doomed.is_empty() {
        sqlx::query(
            "DELETE FROM unlisted_links \
             WHERE resource_type = 'journey' AND resource_id = ANY($1::bigint[]::text[])",
        )
        .bind(&doomed)
        .execute(&mut *tx)
        .await?;
        outcome.journeys = sqlx::query("DELETE FROM journeys WHERE id = ANY($1)")
            .bind(&doomed)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }

    // A journey deleted above needs no release of its legs' trains
    // (`journeys::delete_journey_row_releasing_trains`, DS-93): every one of
    // its legs is before the cutoff, and a leg's subscription carries the
    // leg's own service date (both are the train's), so the statement below
    // deletes those subscriptions in this same transaction, whether or not
    // they were also tracked on their own. Production on 2026-10-21: 23
    // bound legs, none with a different date from its subscription.
    //
    // Any surviving journey leg still pointing at a train about to go is
    // marked unmatched, as `train_tracking::delete_tracked_train` does; the
    // FK then sets its `train_subscription_id` to NULL. Tickets attached to
    // the train, its notification state and its group shares cascade.
    sqlx::query(
        "UPDATE journey_legs SET match_mode = 'unmatched' \
         WHERE train_subscription_id IN (SELECT id FROM train_subscriptions WHERE service_date < $1)",
    )
    .bind(date)
    .execute(&mut *tx)
    .await?;
    outcome.tracked_trains = sqlx::query("DELETE FROM train_subscriptions WHERE service_date < $1")
        .bind(date)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    // Tickets not attached to any tracked train: judged by their departure
    // date when known, else by when they were added.
    outcome.standalone_tickets = sqlx::query(
        "DELETE FROM tracked_train_tickets \
         WHERE tracked_train_id IS NULL AND COALESCE(current_departure_date, created_at) < $1",
    )
    .bind(instant)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    // "Not this date" markers on templates only ever matter for today (the
    // notifier's recurrence sweep mints only today's occurrence).
    outcome.template_skipped_dates =
        sqlx::query("DELETE FROM journey_template_skipped_dates WHERE service_date < $1")
            .bind(date)
            .execute(&mut *tx)
            .await?
            .rows_affected();

    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutoff_date_uses_the_london_date() {
        // 23:30 UTC on 2026-06-30 is already 1 July in London (BST).
        let now = "2026-06-30T23:30:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(
            cutoff_date(now, 1),
            NaiveDate::from_ymd_opt(2026, 6, 30).unwrap()
        );
    }

    #[test]
    fn outcome_total_sums_every_part() {
        let outcome = RetentionOutcome {
            journeys: 1,
            regular_journeys: 9,
            tracked_trains: 2,
            standalone_tickets: 3,
            template_skipped_dates: 4,
            push_subscriptions: 5,
            accounts: 6,
            lapsed_follows: 7,
            usage_daily_rows: 8,
            recorded_leg_outcomes: 100,
        };
        assert_eq!(outcome.total(), 45, "recording an outcome deletes nothing");
    }
}

#[cfg(test)]
#[expect(
    clippy::too_many_lines,
    reason = "test code: scenario tests read top to bottom"
)]
mod db_tests {
    use super::*;
    use crate::data::account::db_tests::{cleanup, connect, seed_every_feature, seed_user};

    async fn count(pool: &PgPool, sql: &str, user: &str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(user)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|err| panic!("{sql}: {err}"))
    }

    const OFF: RetentionPolicy = RetentionPolicy {
        past_travel_days: 0,
        stale_push_subscription_days: 0,
        inactive_account_days: 0,
        regular_journey_days: 0,
    };

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api -- --ignored --test-threads=1`"]
    async fn past_travel_is_pruned_and_recent_travel_and_templates_are_kept() {
        let pool = connect().await;
        let user = "ret-travel-user";
        cleanup(&pool, "ret-travel-").await;
        seed_user(&pool, user).await;

        // Old: a tracked train 600 days ago with an attached ticket, a
        // standalone ticket for then, a journey whose only leg was then
        // (with a share link), and a template skip marker for then.
        // Recent: the same set dated today.
        for (days_ago, label) in [(600, "old"), (0, "new")] {
            sqlx::query(
                "INSERT INTO train_subscriptions (user_id, service_date, resolution_status, custom_name) \
                 VALUES ($1, CURRENT_DATE - $2::int, 'pending', $3)",
            )
            .bind(user)
            .bind(days_ago)
            .bind(label)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO tracked_train_tickets (tracked_train_id, user_id, source) \
                 SELECT id, $1, 'manual' FROM train_subscriptions WHERE user_id = $1 AND custom_name = $2",
            )
            .bind(user)
            .bind(label)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO tracked_train_tickets (user_id, source, custom_name, created_at, current_departure_date) \
                 VALUES ($1, 'manual', $2, NOW() - make_interval(days => $3), NOW() - make_interval(days => $3))",
            )
            .bind(user)
            .bind(label)
            .bind(days_ago)
            .execute(&pool)
            .await
            .unwrap();
            let journey: i64 = sqlx::query_scalar(
                "INSERT INTO journeys (user_id, custom_name, created_at) \
                 VALUES ($1, $2, NOW() - make_interval(days => $3)) RETURNING id",
            )
            .bind(user)
            .bind(label)
            .bind(days_ago)
            .fetch_one(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO journey_legs (journey_id, leg_order, service_date) \
                 VALUES ($1, 0, CURRENT_DATE - $2::int)",
            )
            .bind(journey)
            .bind(days_ago)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO unlisted_links (token_hash, resource_type, resource_id, created_by) \
                 VALUES ($1, 'journey', $2, $3)",
            )
            .bind(format!("ret-travel-{label}"))
            .bind(journey.to_string())
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        }
        let template: i64 = sqlx::query_scalar(
            "INSERT INTO journey_templates (user_id, custom_name, created_at) \
             VALUES ($1, 'Old commute', NOW() - INTERVAL '900 days') RETURNING id",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO journey_template_skipped_dates (template_id, service_date) \
             VALUES ($1, CURRENT_DATE - 600), ($1, CURRENT_DATE)",
        )
        .bind(template)
        .execute(&pool)
        .await
        .unwrap();

        // Disabled: nothing happens.
        let outcome = prune_personal_data(&pool, OFF, Utc::now()).await.unwrap();
        assert_eq!(outcome.total(), 0);

        let policy = RetentionPolicy {
            past_travel_days: 548,
            ..OFF
        };
        let outcome = prune_personal_data(&pool, policy, Utc::now())
            .await
            .unwrap();
        assert!(outcome.journeys >= 1 && outcome.tracked_trains >= 1);
        assert!(outcome.standalone_tickets >= 1 && outcome.template_skipped_dates >= 1);

        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM train_subscriptions WHERE user_id = $1",
                user
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM tracked_train_tickets WHERE user_id = $1",
                user
            )
            .await,
            2,
            "the new attached ticket and the new standalone ticket survive"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM journeys WHERE user_id = $1 AND custom_name = 'new'",
                user
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM journeys WHERE user_id = $1",
                user
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM unlisted_links WHERE created_by = $1",
                user
            )
            .await,
            1,
            "the old journey's share link went with it"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM journey_templates WHERE user_id = $1",
                user
            )
            .await,
            1,
            "templates are kept for the life of the account"
        );
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM journey_template_skipped_dates s JOIN journey_templates t ON t.id = s.template_id WHERE t.user_id = $1",
                user
            )
            .await,
            1
        );

        // Idempotent.
        let again = prune_personal_data(&pool, policy, Utc::now())
            .await
            .unwrap();
        assert_eq!(
            again.journeys + again.tracked_trains + again.standalone_tickets,
            0
        );
        cleanup(&pool, "ret-travel-").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api -- --ignored --test-threads=1`"]
    async fn stale_push_subscriptions_are_pruned_only_for_long_absent_users() {
        let pool = connect().await;
        cleanup(&pool, "ret-push-").await;
        for (user, last_login_days_ago) in [("ret-push-away", 400), ("ret-push-here", 1)] {
            seed_user(&pool, user).await;
            sqlx::query(
                "UPDATE users SET last_login_at = NOW() - make_interval(days => $2) WHERE id = $1",
            )
            .bind(user)
            .bind(last_login_days_ago)
            .execute(&pool)
            .await
            .unwrap();
            // Both subscriptions are old: only the absent user's goes.
            sqlx::query(
                "INSERT INTO push_subscriptions (user_id, endpoint, p256dh, auth, created_at, last_seen_at) \
                 VALUES ($1, 'https://push.example/' || $1, 'k', 'a', NOW() - INTERVAL '500 days', NOW() - INTERVAL '500 days')",
            )
            .bind(user)
            .execute(&pool)
            .await
            .unwrap();
        }
        let policy = RetentionPolicy {
            stale_push_subscription_days: 365,
            ..OFF
        };
        let outcome = prune_personal_data(&pool, policy, Utc::now())
            .await
            .unwrap();
        assert!(outcome.push_subscriptions >= 1);
        let sql = "SELECT COUNT(*) FROM push_subscriptions WHERE user_id = $1";
        assert_eq!(count(&pool, sql, "ret-push-away").await, 0);
        assert_eq!(count(&pool, sql, "ret-push-here").await, 1);
        cleanup(&pool, "ret-push-").await;
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api -- --ignored --test-threads=1`"]
    async fn inactive_accounts_are_deleted_only_when_enabled_and_without_a_live_session() {
        let pool = connect().await;
        cleanup(&pool, "ret-inact-").await;
        let (gone, other) = ("ret-inact-gone", "ret-inact-other");
        seed_every_feature(&pool, gone, other).await;
        seed_user(&pool, "ret-inact-session").await;
        sqlx::query(
            "UPDATE users SET last_login_at = NOW() - INTERVAL '800 days' WHERE id IN ($1, 'ret-inact-session')",
        )
        .bind(gone)
        .execute(&pool)
        .await
        .unwrap();
        // `gone`'s seeded session is expired; `ret-inact-session` has a live one.
        sqlx::query("UPDATE sessions SET expires_at = NOW() - INTERVAL '1 day' WHERE user_id = $1")
            .bind(gone)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO sessions (id, user_id, expires_at) VALUES ('ret-inact-live', 'ret-inact-session', NOW() + INTERVAL '1 day')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // Off by default: nothing is deleted.
        prune_personal_data(&pool, OFF, Utc::now()).await.unwrap();
        let users = "SELECT COUNT(*) FROM users WHERE id = $1";
        assert_eq!(count(&pool, users, gone).await, 1);

        let policy = RetentionPolicy {
            inactive_account_days: 730,
            regular_journey_days: 0,
            ..OFF
        };
        let outcome = prune_personal_data(&pool, policy, Utc::now())
            .await
            .unwrap();
        assert!(outcome.accounts >= 1);
        assert_eq!(count(&pool, users, gone).await, 0);
        assert_eq!(
            count(&pool, users, other).await,
            1,
            "recently active user kept"
        );
        assert_eq!(
            count(&pool, users, "ret-inact-session").await,
            1,
            "live session kept"
        );
        // Group ownership handed over exactly as for self-service deletion.
        let role: String = sqlx::query_scalar(
            "SELECT role FROM group_members WHERE group_id = 'ret-inact-gone-g-shared' AND user_id = $1",
        )
        .bind(other)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(role, "owner");
        cleanup(&pool, "ret-inact-").await;
    }

    /// The usage-analytics sweep runs even with every switchable limit off.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api -- --ignored --test-threads=1`"]
    async fn old_usage_counts_are_pruned_with_every_limit_off() {
        use crate::data::usage::{self, db_tests};
        let pool = connect().await;
        db_tests::clear(&pool).await;
        let now = Utc::now();
        let cutoff = usage::retention_cutoff(now);
        let old = cutoff - Duration::days(1);
        usage::increment(&pool, old, "page_view", db_tests::TEST_DIM, false)
            .await
            .unwrap();
        usage::increment(&pool, cutoff, "page_view", db_tests::TEST_DIM, false)
            .await
            .unwrap();
        let outcome = prune_personal_data(&pool, OFF, now).await.unwrap();
        assert!(outcome.usage_daily_rows >= 1);
        assert_eq!(db_tests::count(&pool, old, "page_view", false).await, 0);
        assert_eq!(db_tests::count(&pool, cutoff, "page_view", false).await, 1);
        db_tests::clear(&pool).await;
    }

    /// A regular-journey occurrence: a journey from `template` with one
    /// train leg `days_ago`, bound to a fresh subscription. Returns
    /// `(journey id, subscription id)`.
    async fn seed_regular_occurrence(
        pool: &PgPool,
        user: &str,
        template: Option<i64>,
        days_ago: i32,
    ) -> (i64, i64) {
        let subscription: i64 = sqlx::query_scalar(
            "INSERT INTO train_subscriptions (user_id, service_date, resolution_status) \
             VALUES ($1, CURRENT_DATE - $2::int, 'pending') RETURNING id",
        )
        .bind(user)
        .bind(days_ago)
        .fetch_one(pool)
        .await
        .unwrap();
        let journey: i64 = sqlx::query_scalar(
            "INSERT INTO journeys (user_id, source_template_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user)
        .bind(template)
        .fetch_one(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO journey_legs (journey_id, leg_order, service_date, match_mode, \
                 train_subscription_id) \
             VALUES ($1, 1, CURRENT_DATE - $2::int, 'auto', $3)",
        )
        .bind(journey)
        .bind(days_ago)
        .bind(subscription)
        .execute(pool)
        .await
        .unwrap();
        (journey, subscription)
    }

    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api -- --ignored --test-threads=1`"]
    async fn regular_occurrences_are_pruned_unless_they_matter_for_delay_repay() {
        let pool = connect().await;
        let user = "ret-regular-user";
        cleanup(&pool, "ret-regular-").await;
        seed_user(&pool, user).await;
        let template: i64 = sqlx::query_scalar(
            "INSERT INTO journey_templates (user_id, days_of_week) VALUES ($1, 31) RETURNING id",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();

        let (plain, plain_train) = seed_regular_occurrence(&pool, user, Some(template), 100).await;
        let (recent, _) = seed_regular_occurrence(&pool, user, Some(template), 60).await;
        // Ticket details saved on its leg (`leg_tickets`): kept.
        let (ticketed, ticketed_train) =
            seed_regular_occurrence(&pool, user, Some(template), 100).await;
        crate::data::leg_tickets::set_for_subscription(
            &pool,
            user,
            ticketed_train,
            &crate::data::leg_tickets::TicketDetails {
                ticket_type: crate::data::leg_tickets::TicketType::Return,
                operator_code: None,
                name: None,
                source: crate::data::leg_tickets::TicketSource::Manual,
                departure_date: None,
            },
        )
        .await
        .unwrap();
        // A recorded plan named ticket details since removed: they no
        // longer keep it.
        let (cleared_ticket, _) = seed_regular_occurrence(&pool, user, Some(template), 100).await;
        sqlx::query(
            "WITH r AS (INSERT INTO journey_revisions \
                            (journey_id, revision_no, reason, actor, is_compensation_baseline) \
                        VALUES ($1, 1, 'booked', 'owner', TRUE) RETURNING id) \
             INSERT INTO journey_revision_legs (revision_id, position, kind, service_date, ticket_ids) \
             SELECT id, 1, 'train', CURRENT_DATE - 100, ARRAY[-1]::bigint[] FROM r",
        )
        .bind(cleared_ticket)
        .execute(&pool)
        .await
        .unwrap();
        // Pushed as 20 minutes late: Delay Repay may be due.
        let (late, late_train) = seed_regular_occurrence(&pool, user, Some(template), 100).await;
        sqlx::query(
            "INSERT INTO train_notification_state \
                 (user_id, tracked_train_id, last_notified_status, last_notified_delay_minutes, \
                  last_notified_at) \
             VALUES ($1, $2, 'en_route', 20, now())",
        )
        .bind(user)
        .bind(late_train)
        .execute(&pool)
        .await
        .unwrap();
        // Pushed as 10 minutes late: under the lowest band.
        let (slightly_late, slightly_late_train) =
            seed_regular_occurrence(&pool, user, Some(template), 100).await;
        sqlx::query(
            "INSERT INTO train_notification_state \
                 (user_id, tracked_train_id, last_notified_status, last_notified_delay_minutes, \
                  last_notified_at) \
             VALUES ($1, $2, 'en_route', 10, now())",
        )
        .bind(user)
        .bind(slightly_late_train)
        .execute(&pool)
        .await
        .unwrap();
        // Its train still held and cancelled.
        let (cancelled, cancelled_train) =
            seed_regular_occurrence(&pool, user, Some(template), 100).await;
        let trains_id: i64 = sqlx::query_scalar(
            "INSERT INTO trains (train_uid, service_date) \
             VALUES ('RETREG1', CURRENT_DATE - 100) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO train_current_state (trains_id, status) VALUES ($1, 'cancelled')")
            .bind(trains_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE train_subscriptions SET trains_id = $2 WHERE id = $1")
            .bind(cancelled_train)
            .bind(trains_id)
            .execute(&pool)
            .await
            .unwrap();
        // Not from a template (or its template was deleted): the long rule.
        let (hand_built, _) = seed_regular_occurrence(&pool, user, None, 100).await;
        // Outcomes recorded on the leg while the train was held (DS-379);
        // the train itself is long gone.
        let record = |journey: i64, delay: Option<i32>, cancelled: bool| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "UPDATE journey_legs SET final_arrival_delay_minutes = $2, \
                         final_train_cancelled = $3, final_recorded_at = now() \
                     WHERE journey_id = $1",
                )
                .bind(journey)
                .bind(delay)
                .bind(cancelled)
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        let (recorded_late, _) = seed_regular_occurrence(&pool, user, Some(template), 100).await;
        record(recorded_late, Some(15), false).await;
        let (recorded_on_time, _) = seed_regular_occurrence(&pool, user, Some(template), 100).await;
        record(recorded_on_time, Some(14), false).await;
        let (recorded_cancelled, _) =
            seed_regular_occurrence(&pool, user, Some(template), 100).await;
        record(recorded_cancelled, None, true).await;

        let policy = RetentionPolicy {
            regular_journey_days: 90,
            ..OFF
        };
        let outcome = prune_personal_data(&pool, policy, Utc::now())
            .await
            .unwrap();
        assert!(outcome.regular_journeys >= 2);
        let exists = |id: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM journeys WHERE id = $1)",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert!(!exists(plain).await, "a plain old occurrence goes");
        assert!(
            !exists(cleared_ticket).await,
            "ticket details since removed don't keep it"
        );
        assert!(!exists(slightly_late).await, "10 minutes late goes");
        assert!(
            !exists(recorded_on_time).await,
            "recorded 14 minutes late goes"
        );
        for kept in [
            recent,
            ticketed,
            late,
            cancelled,
            hand_built,
            recorded_late,
            recorded_cancelled,
        ] {
            assert!(exists(kept).await, "{kept} is kept");
        }
        // Its train is released with it (DS-93).
        let train_left: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM train_subscriptions WHERE id = $1)")
                .bind(plain_train)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(!train_left);

        // Off at 0.
        let (again, _) = seed_regular_occurrence(&pool, user, Some(template), 100).await;
        prune_personal_data(&pool, OFF, Utc::now()).await.unwrap();
        assert!(exists(again).await);

        sqlx::query("DELETE FROM trains WHERE id = $1")
            .bind(trains_id)
            .execute(&pool)
            .await
            .unwrap();
        cleanup(&pool, "ret-regular-").await;
    }

    /// A leg whose train is still held but whose outcome is not recorded
    /// yet keeps its journey: the next pass records it first (DS-379).
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api -- --ignored --test-threads=1`"]
    async fn a_regular_occurrence_waits_for_its_outcome_to_be_recorded() {
        let pool = connect().await;
        let user = "ret-unrecorded-user";
        cleanup(&pool, "ret-unrecorded-").await;
        seed_user(&pool, user).await;
        let template: i64 = sqlx::query_scalar(
            "INSERT INTO journey_templates (user_id, days_of_week) VALUES ($1, 31) RETURNING id",
        )
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
        let (held, held_train) = seed_regular_occurrence(&pool, user, Some(template), 100).await;
        let trains_id: i64 = sqlx::query_scalar(
            "INSERT INTO trains (train_uid, service_date) \
             VALUES ('RETUNR1', CURRENT_DATE - 100) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE train_subscriptions SET trains_id = $2 WHERE id = $1")
            .bind(held_train)
            .bind(trains_id)
            .execute(&pool)
            .await
            .unwrap();
        let date = cutoff_date(Utc::now(), 90);
        prune_regular_occurrences(&pool, date).await.unwrap();
        let exists = || async {
            sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM journeys WHERE id = $1)")
                .bind(held)
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        assert!(exists().await, "kept until its outcome is recorded");
        // Recorded on time (no activation, two days on: not known): it goes.
        crate::data::leg_outcomes::record_final_arrivals(&pool, cutoff_date(Utc::now(), 0))
            .await
            .unwrap();
        prune_regular_occurrences(&pool, date).await.unwrap();
        assert!(!exists().await, "goes once recorded with nothing to keep");

        sqlx::query("DELETE FROM trains WHERE id = $1")
            .bind(trains_id)
            .execute(&pool)
            .await
            .unwrap();
        cleanup(&pool, "ret-unrecorded-").await;
    }
}
