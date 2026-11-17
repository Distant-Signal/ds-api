//! Per-user and per-group quotas that hold under concurrent requests
//! (2026-11-17 review, L10).
//!
//! A quota used to be a `COUNT(*)` on the pool followed by an `INSERT` in a
//! separate statement, so a burst of concurrent requests could all read the
//! same count and every one of them insert. A creating transaction now
//! takes the quota's advisory lock first ([`lock`] or [`lock_user`]),
//! counts on its own connection and inserts, and the lock is released by
//! the commit: the next request to take it counts the committed row. The
//! routes keep their count on the pool as a cheap early refusal, with the
//! same status and message, but the answer that counts is the one given
//! here.
//!
//! **Lock order.** A transaction takes every quota lock it needs in one
//! call, at its start, before any row lock or any other advisory lock, and
//! in [`Kind`] order (`lock_user` does that), so two transactions can never
//! wait on each other. Keep a lock short: take it as the transaction
//! begins its writes, never across an external call or a pool query.
//!
//! The locks are keyed `quota:<kind>:<id>`, an `hashtext` of which is the
//! advisory key; a collision only makes two unrelated quotas wait for each
//! other.
//!
//! Quotas whose count and insert already ran under a lock on the owner's
//! `users` row (webhooks, time away periods, personal tokens, delegated
//! deletes) are unchanged.

use std::fmt;

use axum::http::StatusCode;
use sqlx::PgConnection;

use crate::data::{custom_lines, groups, journey_templates, journeys, train_tracking};

/// What a quota counts. The order is the lock order: a transaction that
/// needs two of these takes the lower one first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// Groups a user belongs to (key: the user id).
    GroupMemberships,
    /// Custom lines a user owns (key: the user id).
    CustomLines,
    /// Journey templates a user owns (key: the user id).
    Templates,
    /// Hand-made journeys a user owns (key: the user id).
    Journeys,
    /// Subscriptions (pins) a user holds (key: the user id).
    Pins,
    /// Journeys and trains shared into a group (key: the group id).
    GroupShares,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::GroupMemberships => "group_memberships",
            Self::CustomLines => "custom_lines",
            Self::Templates => "templates",
            Self::Journeys => "journeys",
            Self::Pins => "pins",
            Self::GroupShares => "group_shares",
        }
    }
}

/// Takes `kind`'s advisory lock for `id` until the transaction on `conn`
/// ends. Blocks while another transaction holds it.
///
/// # Errors
///
/// The query's.
pub async fn lock(conn: &mut PgConnection, kind: Kind, id: &str) -> anyhow::Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('quota:' || $1 || ':' || $2))")
        .bind(kind.name())
        .bind(id)
        .execute(conn)
        .await?;
    Ok(())
}

/// Takes the locks for a user's journey and subscription quotas, in
/// [`Kind`] order: the journeys one when `journeys`, then the pins one when
/// `pins`.
///
/// # Errors
///
/// The queries'.
pub async fn lock_user(
    conn: &mut PgConnection,
    user_id: &str,
    journeys: bool,
    pins: bool,
) -> anyhow::Result<()> {
    if journeys {
        lock(&mut *conn, Kind::Journeys, user_id).await?;
    }
    if pins {
        lock(&mut *conn, Kind::Pins, user_id).await?;
    }
    Ok(())
}

/// A quota that would be passed. A data function that makes the check
/// returns it as the error's source; a route answers it with
/// [`response_for`], the `400` the route's own early check gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exceeded {
    Groups,
    CustomLines,
    Templates,
    Journeys,
    /// Subscriptions dated London's today or later.
    FuturePins,
    /// Subscriptions dated in the recent past.
    RecentPastPins,
    GroupShares,
}

/// How many journeys and trains one group may have shared into it. A
/// member shares a trip or two at a time, and a group is a circle of
/// friends (the busiest has a dozen); 200 is far above that and under
/// `groups::MAX_GROUP_LIST_ITEMS`, so a group's lists are never cut short.
/// Before this the count per group was unbounded.
pub const MAX_SHARES_PER_GROUP: i64 = 200;

impl Exceeded {
    /// The message the client sees.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::Groups => format!(
                "You're already in {} groups, which is the maximum. Leave one to make room.",
                groups::MAX_GROUPS_PER_USER
            ),
            Self::CustomLines => format!(
                "You already have {} custom lines, which is the maximum. Delete one to make \
                 room for a new one.",
                custom_lines::MAX_CUSTOM_LINES_PER_USER
            ),
            Self::Templates => format!(
                "You already have {} journey templates, which is the maximum. Delete one to \
                 make room for a new one.",
                journey_templates::MAX_JOURNEY_TEMPLATES_PER_USER
            ),
            Self::Journeys => format!(
                "You already have {} journeys, which is the maximum. Delete some old ones to \
                 make room for a new one.",
                journeys::MAX_JOURNEYS_PER_USER
            ),
            Self::FuturePins => train_tracking::pin_cap_message(),
            Self::RecentPastPins => train_tracking::recent_past_pin_cap_message(),
            Self::GroupShares => format!(
                "This group already has {MAX_SHARES_PER_GROUP} shared journeys and trains, \
                 which is the maximum. Remove some to make room."
            ),
        }
    }
}

impl fmt::Display for Exceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "quota exceeded: {self:?}")
    }
}

impl std::error::Error for Exceeded {}

/// `400` and the message when `err` is an [`Exceeded`], for a route's error
/// helper to ask before it answers `500`.
#[must_use]
pub fn response_for(err: &anyhow::Error) -> Option<(StatusCode, String)> {
    err.downcast_ref::<Exceeded>()
        .map(|exceeded| (StatusCode::BAD_REQUEST, exceeded.message()))
}

/// The user's hand-made journeys, as `journeys::MAX_JOURNEYS_PER_USER`
/// counts them, read on `conn`.
///
/// # Errors
///
/// The query's.
pub async fn count_manual_journeys(conn: &mut PgConnection, user_id: &str) -> anyhow::Result<i64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM journeys WHERE user_id = $1 AND source_template_id IS NULL",
    )
    .bind(user_id)
    .fetch_one(conn)
    .await?;
    Ok(count)
}

/// Refuses with [`Exceeded::Journeys`] when the user is at the journey cap.
/// Call under [`Kind::Journeys`]'s lock.
///
/// # Errors
///
/// [`Exceeded::Journeys`], or the query's.
pub async fn ensure_journey_room(conn: &mut PgConnection, user_id: &str) -> anyhow::Result<()> {
    if count_manual_journeys(conn, user_id).await? >= journeys::MAX_JOURNEYS_PER_USER {
        return Err(Exceeded::Journeys.into());
    }
    Ok(())
}

/// Refuses with [`Exceeded::FuturePins`] when the user holds the most
/// upcoming subscriptions. Call under [`Kind::Pins`]'s lock.
///
/// # Errors
///
/// [`Exceeded::FuturePins`], or the query's.
pub async fn ensure_future_pin_room(conn: &mut PgConnection, user_id: &str) -> anyhow::Result<()> {
    let upcoming = train_tracking::count_future_subscriptions_for_user(conn, user_id).await?;
    if upcoming >= train_tracking::MAX_FUTURE_PINS_PER_USER {
        return Err(Exceeded::FuturePins.into());
    }
    Ok(())
}

/// [`ensure_future_pin_room`], unless the user already has a subscription
/// to the shared train `trains_id`, which a new one would reuse.
///
/// # Errors
///
/// As [`ensure_future_pin_room`].
pub async fn ensure_pin_room_for_train(
    conn: &mut PgConnection,
    user_id: &str,
    trains_id: i64,
) -> anyhow::Result<()> {
    if tracks_train(&mut *conn, user_id, trains_id).await? {
        return Ok(());
    }
    ensure_future_pin_room(conn, user_id).await
}

/// Whether the user already has a subscription to the shared train
/// `trains_id`, which creating one for it would reuse.
///
/// # Errors
///
/// The query's.
pub async fn tracks_train(
    conn: &mut PgConnection,
    user_id: &str,
    trains_id: i64,
) -> anyhow::Result<bool> {
    let tracked: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM train_subscriptions WHERE user_id = $1 AND trains_id = $2)",
    )
    .bind(user_id)
    .bind(trains_id)
    .fetch_one(conn)
    .await?;
    Ok(tracked)
}

/// The pin cap for a new subscription dated `service_date`: the upcoming
/// cap from London's today on, the recent-past one before it (as
/// `POST /Train/by-uid/{uid}/{date}/track` applies them).
///
/// # Errors
///
/// [`Exceeded::FuturePins`] or [`Exceeded::RecentPastPins`], or the
/// queries'.
pub async fn ensure_pin_room_for_date(
    conn: &mut PgConnection,
    user_id: &str,
    service_date: chrono::NaiveDate,
) -> anyhow::Result<()> {
    let today = crate::routes::london_today();
    if service_date >= today {
        return ensure_future_pin_room(conn, user_id).await;
    }
    let earliest = today - chrono::Duration::days(train_tracking::TRACK_BY_UID_MAX_DAYS_BEHIND);
    let recent =
        train_tracking::count_recent_past_subscriptions_for_user(conn, user_id, earliest, today)
            .await?;
    if recent >= train_tracking::MAX_RECENT_PAST_PINS_PER_USER {
        return Err(Exceeded::RecentPastPins.into());
    }
    Ok(())
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use sqlx::PgPool;
    use sqlx::postgres::PgPoolOptions;

    /// How many requests race for the last slot.
    const RACERS: usize = 8;

    async fn connect() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
        PgPoolOptions::new()
            .max_connections(16)
            .connect(&database_url)
            .await
            .expect("connect to postgres")
    }

    /// Runs `make(0)` to `make(count - 1)` at once, each on its own pool
    /// connection, and gathers their results.
    async fn race<T, F, Fut>(count: usize, make: F) -> Vec<anyhow::Result<T>>
    where
        F: Fn(usize) -> Fut,
        Fut: Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let handles: Vec<_> = (0..count).map(|n| tokio::spawn(make(n))).collect();
        let mut results = Vec::with_capacity(count);
        for handle in handles {
            results.push(handle.await.expect("racing task panicked"));
        }
        results
    }

    async fn seed_user(pool: &PgPool, id: &str) {
        sqlx::query("INSERT INTO users (id, name) VALUES ($1, $1) ON CONFLICT (id) DO NOTHING")
            .bind(id)
            .execute(pool)
            .await
            .expect("seed fixture user");
    }

    async fn wipe_user(pool: &PgPool, id: &str) {
        for sql in [
            "DELETE FROM custom_lines WHERE user_id = $1",
            "DELETE FROM journey_templates WHERE user_id = $1",
            "DELETE FROM journeys WHERE user_id = $1",
            "DELETE FROM group_members WHERE user_id = $1",
            "DELETE FROM users WHERE id = $1",
        ] {
            sqlx::query(sql)
                .bind(id)
                .execute(pool)
                .await
                .expect("wipe fixture user");
        }
    }

    /// How many of `results` are `Ok` and how many are the quota's refusal;
    /// any other error fails the test.
    fn tally<T>(results: &[anyhow::Result<T>], refusal: Exceeded) -> (usize, usize) {
        let mut ok = 0;
        let mut refused = 0;
        for result in results {
            match result {
                Ok(_) => ok += 1,
                Err(err) if err.downcast_ref::<Exceeded>() == Some(&refusal) => refused += 1,
                Err(err) => panic!("neither created nor refused: {err:?}"),
            }
        }
        (ok, refused)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                concurrent_custom_line_creates_hold_the_cap -- --ignored`"]
    async fn concurrent_custom_line_creates_hold_the_cap() {
        const USER: &str = "TEST-QUOTA-CUSTOM-LINES";
        let pool = connect().await;
        wipe_user(&pool, USER).await;
        seed_user(&pool, USER).await;
        sqlx::query(
            "INSERT INTO custom_lines \
                (id, name, operators, stations, headcode_prefixes, destination_crs_filter, \
                 user_id, created_at) \
             SELECT 'test-quota-cl-' || i, 'Quota ' || i, ARRAY['SW']::text[], \
                    ARRAY['WAT','SUR']::text[], ARRAY[]::text[], ARRAY[]::text[], $1, NOW() \
             FROM generate_series(1, $2::int) AS i",
        )
        .bind(USER)
        .bind(i32::try_from(custom_lines::MAX_CUSTOM_LINES_PER_USER - 1).unwrap())
        .execute(&pool)
        .await
        .expect("seed custom lines");

        let results = race(RACERS, |n| {
            let pool = pool.clone();
            async move {
                custom_lines::insert_custom_line_capped(
                    &pool,
                    custom_lines::NewCustomLine {
                        name: format!("Racer {n}"),
                        operators: vec!["SW".to_string()],
                        stations: vec!["WAT".to_string(), "SUR".to_string()],
                        headcode_prefixes: vec![],
                        destination_crs_filter: vec![],
                    },
                    USER,
                )
                .await
            }
        })
        .await;
        let owned: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM custom_lines WHERE user_id = $1")
            .bind(USER)
            .fetch_one(&pool)
            .await
            .unwrap();
        wipe_user(&pool, USER).await;

        assert_eq!(tally(&results, Exceeded::CustomLines), (1, RACERS - 1));
        assert_eq!(owned, custom_lines::MAX_CUSTOM_LINES_PER_USER);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                concurrent_group_creates_hold_the_membership_cap -- --ignored`"]
    async fn concurrent_group_creates_hold_the_membership_cap() {
        const USER: &str = "TEST-QUOTA-GROUPS";
        let pool = connect().await;
        sqlx::query("DELETE FROM groups WHERE id LIKE 'test-quota-groups-%'")
            .execute(&pool)
            .await
            .unwrap();
        wipe_user(&pool, USER).await;
        seed_user(&pool, USER).await;
        sqlx::query(
            "WITH g AS ( \
                 INSERT INTO groups (id, name) \
                 SELECT 'test-quota-groups-' || i, 'Quota ' || i FROM generate_series(1, $2::int) i \
                 RETURNING id) \
             INSERT INTO group_members (group_id, user_id, role) SELECT id, $1, 'owner' FROM g",
        )
        .bind(USER)
        .bind(i32::try_from(groups::MAX_GROUPS_PER_USER - 1).unwrap())
        .execute(&pool)
        .await
        .expect("seed memberships");

        let results = race(RACERS, |n| {
            let pool = pool.clone();
            async move { groups::create_group(&pool, &format!("Racer {n}"), USER).await }
        })
        .await;
        let memberships = groups::count_memberships_for_user(&pool, USER)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM groups WHERE id LIKE 'test-quota-groups-%' OR id IN \
             (SELECT group_id FROM group_members WHERE user_id = $1)",
        )
        .bind(USER)
        .execute(&pool)
        .await
        .unwrap();
        wipe_user(&pool, USER).await;

        assert_eq!(tally(&results, Exceeded::Groups), (1, RACERS - 1));
        assert_eq!(memberships, groups::MAX_GROUPS_PER_USER);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                concurrent_template_creates_hold_the_cap -- --ignored`"]
    async fn concurrent_template_creates_hold_the_cap() {
        const USER: &str = "TEST-QUOTA-TEMPLATES";
        let pool = connect().await;
        wipe_user(&pool, USER).await;
        seed_user(&pool, USER).await;
        sqlx::query(
            "INSERT INTO journey_templates (user_id) SELECT $1 FROM generate_series(1, $2::int)",
        )
        .bind(USER)
        .bind(i32::try_from(journey_templates::MAX_JOURNEY_TEMPLATES_PER_USER - 1).unwrap())
        .execute(&pool)
        .await
        .expect("seed templates");

        let results = race(RACERS, |_| {
            let pool = pool.clone();
            async move { journey_templates::create_template(&pool, USER, None, &[]).await }
        })
        .await;
        let owned = journey_templates::count_templates_for_user(&pool, USER)
            .await
            .unwrap();
        wipe_user(&pool, USER).await;

        assert_eq!(tally(&results, Exceeded::Templates), (1, RACERS - 1));
        assert_eq!(owned, journey_templates::MAX_JOURNEY_TEMPLATES_PER_USER);
    }

    /// The journey and pin quotas as a creating transaction takes them
    /// (`lock_user`, then the checks, then the insert), racing for the last
    /// journey slot and the last pin slot together.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                concurrent_journey_and_pin_creates_hold_the_caps -- --ignored`"]
    async fn concurrent_journey_and_pin_creates_hold_the_caps() {
        const USER: &str = "TEST-QUOTA-JOURNEYS-PINS";
        let pool = connect().await;
        wipe_user(&pool, USER).await;
        sqlx::query("DELETE FROM train_subscriptions WHERE user_id = $1")
            .bind(USER)
            .execute(&pool)
            .await
            .unwrap();
        seed_user(&pool, USER).await;
        sqlx::query("INSERT INTO journeys (user_id) SELECT $1 FROM generate_series(1, $2::int)")
            .bind(USER)
            .bind(i32::try_from(journeys::MAX_JOURNEYS_PER_USER - 1).unwrap())
            .execute(&pool)
            .await
            .expect("seed journeys");
        let today = crate::routes::london_today();
        sqlx::query(
            "INSERT INTO train_subscriptions (user_id, service_date) \
             SELECT $1, $2 FROM generate_series(1, $3::int)",
        )
        .bind(USER)
        .bind(today + chrono::Duration::days(1))
        .bind(i32::try_from(train_tracking::MAX_FUTURE_PINS_PER_USER - 1).unwrap())
        .execute(&pool)
        .await
        .expect("seed subscriptions");

        let results = race(RACERS, |_| {
            let pool = pool.clone();
            async move {
                let mut tx = pool.begin().await?;
                lock_user(&mut tx, USER, true, true).await?;
                ensure_journey_room(&mut tx, USER).await?;
                ensure_future_pin_room(&mut tx, USER).await?;
                sqlx::query("INSERT INTO journeys (user_id) VALUES ($1)")
                    .bind(USER)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "INSERT INTO train_subscriptions (user_id, service_date) VALUES ($1, $2)",
                )
                .bind(USER)
                .bind(today + chrono::Duration::days(1))
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok::<(), anyhow::Error>(())
            }
        })
        .await;
        let journeys_owned: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM journeys WHERE user_id = $1")
                .bind(USER)
                .fetch_one(&pool)
                .await
                .unwrap();
        let pins_held: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM train_subscriptions WHERE user_id = $1")
                .bind(USER)
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query("DELETE FROM train_subscriptions WHERE user_id = $1")
            .bind(USER)
            .execute(&pool)
            .await
            .unwrap();
        wipe_user(&pool, USER).await;

        assert_eq!(tally(&results, Exceeded::Journeys), (1, RACERS - 1));
        assert_eq!(journeys_owned, journeys::MAX_JOURNEYS_PER_USER);
        assert_eq!(pins_held, train_tracking::MAX_FUTURE_PINS_PER_USER);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires a live database; run with `DATABASE_URL=... cargo test -p api \
                concurrent_group_shares_hold_the_cap -- --ignored`"]
    async fn concurrent_group_shares_hold_the_cap() {
        const USER: &str = "TEST-QUOTA-GROUP-SHARES";
        const GROUP: &str = "test-quota-group-shares";
        let pool = connect().await;
        sqlx::query("DELETE FROM groups WHERE id = $1")
            .bind(GROUP)
            .execute(&pool)
            .await
            .unwrap();
        wipe_user(&pool, USER).await;
        seed_user(&pool, USER).await;
        sqlx::query("INSERT INTO groups (id, name) VALUES ($1, 'Quota')")
            .bind(GROUP)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO group_members (group_id, user_id, role) VALUES ($1, $2, 'owner')")
            .bind(GROUP)
            .bind(USER)
            .execute(&pool)
            .await
            .unwrap();
        let seeded = i32::try_from(MAX_SHARES_PER_GROUP - 1).unwrap();
        sqlx::query("INSERT INTO journeys (user_id) SELECT $1 FROM generate_series(1, $2::int)")
            .bind(USER)
            .bind(seeded + i32::try_from(RACERS).unwrap())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO group_journeys (group_id, journey_id, added_by, added_at) \
             SELECT $1, id, $2, NOW() FROM journeys WHERE user_id = $2 ORDER BY id LIMIT $3",
        )
        .bind(GROUP)
        .bind(USER)
        .bind(i64::from(seeded))
        .execute(&pool)
        .await
        .unwrap();
        let unshared: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM journeys WHERE user_id = $1 \
               AND id NOT IN (SELECT journey_id FROM group_journeys WHERE group_id = $2) \
             ORDER BY id",
        )
        .bind(USER)
        .bind(GROUP)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(unshared.len(), RACERS);

        let results = race(RACERS, |n| {
            let pool = pool.clone();
            let journey_id = unshared[n];
            async move { groups::add_journey_to_group(&pool, GROUP, journey_id, USER).await }
        })
        .await;
        // Sharing something already shared is still fine at the cap.
        let winner = results
            .iter()
            .position(Result::is_ok)
            .expect("one share lands");
        let again = groups::add_journey_to_group(&pool, GROUP, unshared[winner], USER).await;
        let shares: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM group_journeys WHERE group_id = $1")
                .bind(GROUP)
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::query("DELETE FROM groups WHERE id = $1")
            .bind(GROUP)
            .execute(&pool)
            .await
            .unwrap();
        wipe_user(&pool, USER).await;

        assert_eq!(tally(&results, Exceeded::GroupShares), (1, RACERS - 1));
        assert_eq!(shares, MAX_SHARES_PER_GROUP);
        assert!(
            matches!(again, Ok(true)),
            "re-sharing a shared journey must not be refused: {again:?}"
        );
    }
}
