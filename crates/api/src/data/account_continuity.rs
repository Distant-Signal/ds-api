//! Account continuity across a change of identity provider: the login
//! callback's user upsert when `SSO_DISCORD_ID_CLAIM` or
//! `SSO_PRIMARY_SUB_CLAIM` is set (docs/connected-apps.md, "Account
//! continuity"; Ranma-Config docs/specs/ds-public-auth-idp.md).
//!
//! `users.id` is the OIDC `sub`, and the delegated-token, revoke and
//! back-channel logout paths use a token's `sub` directly, so it must stay
//! equal to the sub the current identity provider issues. When public
//! login moves to `ds-sso`, every user arrives with a new sub. On that
//! first login, in one transaction:
//!
//! 1. a row whose id is the new sub exists: the ordinary upsert;
//! 2. else a row matches the token's Discord id (`users.discord_id`) or
//!    has the forwarded primary sub as its id: it is re-keyed in place,
//!    `UPDATE users SET id = <new sub>`, which the `ON UPDATE CASCADE`
//!    foreign keys (ds-core 20261104090200) carry to every table that
//!    references it; `group_train_share_moves.added_by`, which copies the
//!    id with no foreign key, is updated by hand;
//! 3. else a new user is created.
//!
//! The Discord id claim is stored on every login. A match that would be
//! ambiguous refuses the re-key: see [`Conflict`].

use anyhow::Result;
use sqlx::PgPool;

use crate::auth::oidc::{ContinuityClaims, OidcIdentity};
use crate::data::users::{self, User};

/// Suffix of `distant_signal_api_account_rekeys_total{matched_by}`: one per
/// account re-keyed to a new sub.
pub const REKEYS_METRIC: &str = "api_account_rekeys_total";
/// Suffix of `distant_signal_api_account_continuity_conflicts_total{kind}`:
/// one per refused re-key or refused Discord id (see [`Conflict`]).
pub const CONFLICTS_METRIC: &str = "api_account_continuity_conflicts_total";

/// Registers both counters at 0 for each label, so an alert's
/// `increase()` sees the first one after a restart.
pub fn register_metrics() {
    for matched_by in [MatchedBy::DiscordId, MatchedBy::PrimarySub] {
        metrics::counter!(
            ds_runtime::metrics::metric_name(REKEYS_METRIC),
            "matched_by" => matched_by.label()
        )
        .increment(0);
    }
    for kind in [
        Conflict::DiscordIdHeldByAnotherAccount,
        Conflict::AmbiguousMatch,
        Conflict::PrimarySubAccountHasAnotherDiscordId,
    ] {
        metrics::counter!(
            ds_runtime::metrics::metric_name(CONFLICTS_METRIC),
            "kind" => kind.label()
        )
        .increment(0);
    }
}

/// Which claim found the account a login re-keyed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchedBy {
    DiscordId,
    PrimarySub,
}

impl MatchedBy {
    pub fn label(self) -> &'static str {
        match self {
            Self::DiscordId => "discord_id",
            Self::PrimarySub => "primary_sub",
        }
    }
}

/// A login whose continuity claims point at more than one account, or at
/// an account that is not theirs. Never resolved automatically: an
/// operator merges the accounts by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// The new sub already has an account, and another account holds the
    /// token's Discord id (typically: the user logged in through the new
    /// identity provider before the Discord ids were loaded). The login
    /// goes ahead as the new sub's account, without storing the Discord
    /// id or re-keying anything.
    DiscordIdHeldByAnotherAccount,
    /// The Discord id and the primary sub match two different accounts.
    /// The login is refused: creating a third account would only add to
    /// the merge.
    AmbiguousMatch,
    /// The primary sub's account has a different Discord id from the
    /// token's. The login is refused.
    PrimarySubAccountHasAnotherDiscordId,
}

impl Conflict {
    pub fn label(self) -> &'static str {
        match self {
            Self::DiscordIdHeldByAnotherAccount => "discord_id_held_by_another_account",
            Self::AmbiguousMatch => "ambiguous_match",
            Self::PrimarySubAccountHasAnotherDiscordId => {
                "primary_sub_account_has_another_discord_id"
            }
        }
    }

    /// Whether the login is refused (else it goes ahead without the
    /// re-key or the Discord id).
    pub fn refuses_login(self) -> bool {
        !matches!(self, Self::DiscordIdHeldByAnotherAccount)
    }
}

/// What a continuity login did besides the upsert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The new sub had an account (or none matched and one was created:
    /// see `User::created`).
    Upserted,
    /// The account `from` was re-keyed to the new sub.
    Rekeyed { from: String, matched_by: MatchedBy },
}

/// The result of [`login`]: the user (absent when a conflict refused the
/// login), what happened, and any conflict.
#[derive(Debug)]
pub struct Login {
    pub user: Option<User>,
    pub outcome: Outcome,
    pub conflict: Option<Conflict>,
}

/// Which account a login with no account of its own takes over: the
/// Discord id's match, else the primary sub's, unless they disagree.
/// `by_primary` is the primary sub's account and its stored Discord id.
fn choose(
    by_discord: Option<&str>,
    by_primary: Option<(&str, Option<&str>)>,
    discord_claim: Option<&str>,
) -> Result<Option<(String, MatchedBy)>, Conflict> {
    match (by_discord, by_primary) {
        (Some(discord), Some((primary, _))) if discord != primary => Err(Conflict::AmbiguousMatch),
        (Some(discord), _) => Ok(Some((discord.to_string(), MatchedBy::DiscordId))),
        (None, Some((primary, stored))) => match (stored, discord_claim) {
            (Some(stored), Some(claim)) if stored != claim => {
                Err(Conflict::PrimarySubAccountHasAnotherDiscordId)
            }
            _ => Ok(Some((primary.to_string(), MatchedBy::PrimarySub))),
        },
        (None, None) => Ok(None),
    }
}

/// The login callback's upsert with account continuity on, in one
/// transaction (module docs). `claims` are this login's continuity claims
/// (`auth::oidc::continuity_claims`).
///
/// Locks each candidate row (`FOR UPDATE`), so two concurrent first logins
/// of one user serialise: the second finds the row already re-keyed to its
/// sub and takes the ordinary path.
pub async fn login(
    pool: &PgPool,
    identity: &OidcIdentity,
    claims: &ContinuityClaims,
) -> Result<Login> {
    let new_sub = identity.sub.as_str();
    let discord_claim = claims.discord_id.as_deref();
    let mut tx = pool.begin().await?;

    let own: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(new_sub)
        .fetch_optional(&mut *tx)
        .await?;

    let mut outcome = Outcome::Upserted;
    if own.is_none() {
        let by_discord: Option<String> = match discord_claim {
            Some(discord) => {
                sqlx::query_scalar("SELECT id FROM users WHERE discord_id = $1 FOR UPDATE")
                    .bind(discord)
                    .fetch_optional(&mut *tx)
                    .await?
            }
            None => None,
        };
        let by_primary: Option<(String, Option<String>)> = match claims.primary_sub.as_deref() {
            Some(primary) if primary != new_sub => {
                sqlx::query_as("SELECT id, discord_id FROM users WHERE id = $1 FOR UPDATE")
                    .bind(primary)
                    .fetch_optional(&mut *tx)
                    .await?
            }
            _ => None,
        };
        let chosen = choose(
            by_discord.as_deref(),
            by_primary
                .as_ref()
                .map(|(id, stored)| (id.as_str(), stored.as_deref())),
            discord_claim,
        );
        match chosen {
            Err(conflict) => {
                tx.rollback().await?;
                return Ok(Login {
                    user: None,
                    outcome,
                    conflict: Some(conflict),
                });
            }
            Ok(Some((from, matched_by))) => {
                rekey(&mut tx, &from, new_sub).await?;
                outcome = Outcome::Rekeyed { from, matched_by };
            }
            Ok(None) => {}
        }
    }

    // Store the Discord id unless another account holds it (the unique
    // index would refuse it, aborting the transaction).
    let mut conflict = None;
    let mut store_discord = discord_claim;
    if let Some(discord) = discord_claim {
        let holder: Option<String> =
            sqlx::query_scalar("SELECT id FROM users WHERE discord_id = $1 AND id <> $2")
                .bind(discord)
                .bind(new_sub)
                .fetch_optional(&mut *tx)
                .await?;
        if holder.is_some() {
            conflict = Some(Conflict::DiscordIdHeldByAnotherAccount);
            store_discord = None;
        }
    }

    let user = users::upsert_user_in(&mut tx, identity, store_discord).await?;
    tx.commit().await?;
    Ok(Login {
        user: Some(user),
        outcome,
        conflict,
    })
}

/// Moves the account `from` to `to`: the `users` row (cascading to every
/// foreign key) and the one copied id without a foreign key.
async fn rekey(conn: &mut sqlx::PgConnection, from: &str, to: &str) -> Result<()> {
    sqlx::query("UPDATE users SET id = $2 WHERE id = $1")
        .bind(from)
        .bind(to)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE group_train_share_moves SET added_by = $2 WHERE added_by = $1")
        .bind(from)
        .bind(to)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_match_creates() {
        assert_eq!(choose(None, None, Some("1")), Ok(None));
        assert_eq!(choose(None, None, None), Ok(None));
    }

    #[test]
    fn the_discord_match_wins_when_alone_or_the_same_account() {
        assert_eq!(
            choose(Some("old"), None, Some("1")),
            Ok(Some(("old".to_string(), MatchedBy::DiscordId)))
        );
        assert_eq!(
            choose(Some("old"), Some(("old", Some("1"))), Some("1")),
            Ok(Some(("old".to_string(), MatchedBy::DiscordId)))
        );
    }

    #[test]
    fn the_primary_sub_matches_staff() {
        assert_eq!(
            choose(None, Some(("staff", None)), None),
            Ok(Some(("staff".to_string(), MatchedBy::PrimarySub)))
        );
        // A stored Discord id with no Discord claim, or the same one, is
        // no conflict.
        assert_eq!(
            choose(None, Some(("staff", Some("1"))), None),
            Ok(Some(("staff".to_string(), MatchedBy::PrimarySub)))
        );
    }

    #[test]
    fn disagreeing_matches_are_conflicts_that_refuse_the_login() {
        let ambiguous = choose(Some("a"), Some(("b", None)), Some("1"));
        assert_eq!(ambiguous, Err(Conflict::AmbiguousMatch));
        let other = choose(None, Some(("staff", Some("2"))), Some("1"));
        assert_eq!(other, Err(Conflict::PrimarySubAccountHasAnotherDiscordId));
        assert!(Conflict::AmbiguousMatch.refuses_login());
        assert!(Conflict::PrimarySubAccountHasAnotherDiscordId.refuses_login());
        assert!(!Conflict::DiscordIdHeldByAnotherAccount.refuses_login());
    }
}

/// Database-gated: run against the shared migrated database with
/// `DATABASE_URL` (CI also runs them as the api's own role). Every fixture
/// id starts `TEST-CONTINUITY-`; each test clears its own before and after.
#[cfg(test)]
mod db_tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    async fn connect() -> PgPool {
        let database_url =
            std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
        PgPoolOptions::new()
            .connect(&database_url)
            .await
            .expect("connect to postgres")
    }

    fn identity(sub: &str) -> OidcIdentity {
        OidcIdentity {
            sub: sub.to_string(),
            name: Some("Continuity Rider".to_string()),
            preferred_username: Some("continuity-rider".to_string()),
            groups: Vec::new(),
        }
    }

    fn discord(id: &str) -> ContinuityClaims {
        ContinuityClaims {
            discord_id: Some(id.to_string()),
            primary_sub: None,
        }
    }

    async fn clear(pool: &PgPool, ids: &[&str]) {
        sqlx::query("DELETE FROM users WHERE id = ANY($1)")
            .bind(ids)
            .execute(pool)
            .await
            .expect("clear fixture users");
    }

    async fn seed_user(pool: &PgPool, id: &str, discord_id: Option<&str>) {
        sqlx::query("INSERT INTO users (id, name, discord_id) VALUES ($1, 'Old', $2)")
            .bind(id)
            .bind(discord_id)
            .execute(pool)
            .await
            .expect("seed user");
    }

    async fn discord_of(pool: &PgPool, id: &str) -> Option<Option<String>> {
        sqlx::query_scalar("SELECT discord_id FROM users WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await
            .expect("read discord_id")
    }

    const OLD: &str = "TEST-CONTINUITY-OLD";
    const NEW: &str = "TEST-CONTINUITY-NEW";
    const OTHER: &str = "TEST-CONTINUITY-OTHER";
    const DISCORD: &str = "900000000000000001";

    /// A public user's first `ds-sso` login takes over their account, with
    /// their session, personal API token, delegated grant and pin, and the
    /// copied `group_train_share_moves.added_by`.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api account_continuity -- \
                --ignored --test-threads=1`"]
    async fn a_discord_match_rekeys_the_account_and_everything_follows() {
        let pool = connect().await;
        clear(&pool, &[OLD, NEW]).await;
        seed_user(&pool, OLD, Some(DISCORD)).await;

        users::insert_session(&pool, "TEST-CONTINUITY-SESSION", OLD, 14)
            .await
            .expect("insert session");
        let token_hash = [7u8; 32];
        sqlx::query(
            "INSERT INTO personal_api_tokens (user_id, name, token_hash, hint, scopes, expires_at) \
             VALUES ($1, 'continuity', $2, 'abcd', ARRAY['journeys:read'], now() + interval '1 day')",
        )
        .bind(OLD)
        .bind(token_hash.as_slice())
        .execute(&pool)
        .await
        .expect("insert personal token");
        sqlx::query(
            "INSERT INTO delegated_grants (user_id, client, client_display_name, scopes) \
             VALUES ($1, 'continuity-test', 'Continuity', ARRAY['journeys:read'])",
        )
        .bind(OLD)
        .execute(&pool)
        .await
        .expect("insert delegated grant");
        sqlx::query("INSERT INTO pinned_lines (user_id, line_id) VALUES ($1, 'wcml')")
            .bind(OLD)
            .execute(&pool)
            .await
            .expect("insert pin");
        let share_move = seed_share_move(&pool, OLD).await;

        let result = login(&pool, &identity(NEW), &discord(DISCORD))
            .await
            .expect("login");
        let user = result.user.expect("a user");
        assert_eq!(user.id, NEW);
        assert!(!user.created, "a re-key is not a new user");
        assert_eq!(
            result.outcome,
            Outcome::Rekeyed {
                from: OLD.to_string(),
                matched_by: MatchedBy::DiscordId
            }
        );
        assert_eq!(result.conflict, None);
        assert_eq!(discord_of(&pool, OLD).await, None, "the old id is gone");
        assert_eq!(
            discord_of(&pool, NEW).await,
            Some(Some(DISCORD.to_string()))
        );

        let session = users::get_session_with_user(&pool, "TEST-CONTINUITY-SESSION")
            .await
            .expect("session lookup")
            .expect("the session survives the re-key");
        assert_eq!(session.id, NEW);
        let token = crate::data::personal_tokens::find_active_by_hash(&pool, &token_hash)
            .await
            .expect("token lookup")
            .expect("the token survives the re-key");
        assert_eq!(token.user_id, NEW);
        assert!(
            crate::data::delegated::active_grant(&pool, NEW, "continuity-test")
                .await
                .expect("grant lookup")
                .is_some(),
            "the delegated grant follows the new sub"
        );
        let pins: i64 = sqlx::query_scalar("SELECT count(*) FROM pinned_lines WHERE user_id = $1")
            .bind(NEW)
            .fetch_one(&pool)
            .await
            .expect("count pins");
        assert_eq!(pins, 1);
        assert_eq!(share_move.added_by().await, NEW);
        share_move.clear().await;

        // The next login is the ordinary path.
        let again = login_again(&pool, NEW, DISCORD).await;
        assert_eq!(again.outcome, Outcome::Upserted);
        clear(&pool, &[OLD, NEW]).await;
    }

    async fn login_again(pool: &PgPool, sub: &str, discord_id: &str) -> Login {
        login(pool, &identity(sub), &discord(discord_id))
            .await
            .expect("login")
    }

    /// No account matches: a new one is created, with the Discord id.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api account_continuity -- \
                --ignored --test-threads=1`"]
    async fn no_match_creates_a_user_with_the_discord_id() {
        let pool = connect().await;
        clear(&pool, &[NEW]).await;
        let result = login_again(&pool, NEW, DISCORD).await;
        let user = result.user.expect("a user");
        assert!(user.created);
        assert_eq!(result.outcome, Outcome::Upserted);
        assert_eq!(
            discord_of(&pool, NEW).await,
            Some(Some(DISCORD.to_string()))
        );

        // A later login without the claim keeps the stored id.
        login(&pool, &identity(NEW), &ContinuityClaims::default())
            .await
            .expect("login");
        assert_eq!(
            discord_of(&pool, NEW).await,
            Some(Some(DISCORD.to_string()))
        );
        clear(&pool, &[NEW]).await;
    }

    /// The new sub already has an account (logged in before the Discord
    /// ids were loaded) and another account holds the Discord id: no
    /// re-key, the Discord id is not stored, the login goes ahead.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api account_continuity -- \
                --ignored --test-threads=1`"]
    async fn a_discord_id_held_by_another_account_is_a_collision() {
        let pool = connect().await;
        clear(&pool, &[OLD, NEW]).await;
        seed_user(&pool, OLD, Some(DISCORD)).await;
        seed_user(&pool, NEW, None).await;

        let result = login_again(&pool, NEW, DISCORD).await;
        assert_eq!(result.user.expect("a user").id, NEW);
        assert_eq!(result.outcome, Outcome::Upserted);
        assert_eq!(
            result.conflict,
            Some(Conflict::DiscordIdHeldByAnotherAccount)
        );
        assert_eq!(discord_of(&pool, NEW).await, Some(None));
        assert_eq!(
            discord_of(&pool, OLD).await,
            Some(Some(DISCORD.to_string()))
        );
        clear(&pool, &[OLD, NEW]).await;
    }

    /// Staff: the forwarded primary sub is their existing id.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api account_continuity -- \
                --ignored --test-threads=1`"]
    async fn a_primary_sub_match_rekeys_a_staff_account() {
        let pool = connect().await;
        clear(&pool, &[OLD, NEW]).await;
        seed_user(&pool, OLD, None).await;
        users::insert_session(&pool, "TEST-CONTINUITY-STAFF-SESSION", OLD, 14)
            .await
            .expect("insert session");

        let claims = ContinuityClaims {
            discord_id: None,
            primary_sub: Some(OLD.to_string()),
        };
        let result = login(&pool, &identity(NEW), &claims).await.expect("login");
        assert_eq!(result.user.expect("a user").id, NEW);
        assert_eq!(
            result.outcome,
            Outcome::Rekeyed {
                from: OLD.to_string(),
                matched_by: MatchedBy::PrimarySub
            }
        );
        let session = users::get_session_with_user(&pool, "TEST-CONTINUITY-STAFF-SESSION")
            .await
            .expect("session lookup")
            .expect("the session survives the re-key");
        assert_eq!(session.id, NEW);
        clear(&pool, &[OLD, NEW]).await;
    }

    /// The Discord id and the primary sub name two different accounts: the
    /// login is refused and nothing changes.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api account_continuity -- \
                --ignored --test-threads=1`"]
    async fn an_ambiguous_match_refuses_the_login() {
        let pool = connect().await;
        clear(&pool, &[OLD, OTHER, NEW]).await;
        seed_user(&pool, OLD, Some(DISCORD)).await;
        seed_user(&pool, OTHER, None).await;

        let claims = ContinuityClaims {
            discord_id: Some(DISCORD.to_string()),
            primary_sub: Some(OTHER.to_string()),
        };
        let result = login(&pool, &identity(NEW), &claims).await.expect("login");
        assert!(result.user.is_none());
        assert_eq!(result.conflict, Some(Conflict::AmbiguousMatch));
        assert_eq!(discord_of(&pool, NEW).await, None, "no account was created");
        assert_eq!(
            discord_of(&pool, OLD).await,
            Some(Some(DISCORD.to_string()))
        );
        assert_eq!(discord_of(&pool, OTHER).await, Some(None));
        clear(&pool, &[OLD, OTHER, NEW]).await;
    }

    /// With the feature off the callback calls the plain `upsert_user`:
    /// an unknown sub is a new account even when another account has the
    /// user's Discord id, and no Discord id is written.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api account_continuity -- \
                --ignored --test-threads=1`"]
    async fn with_the_feature_off_an_unknown_sub_is_a_new_account() {
        let pool = connect().await;
        clear(&pool, &[OLD, NEW]).await;
        seed_user(&pool, OLD, Some(DISCORD)).await;

        let user = users::upsert_user(&pool, &identity(NEW))
            .await
            .expect("upsert");
        assert!(user.created);
        assert_eq!(discord_of(&pool, NEW).await, Some(None));
        assert_eq!(
            discord_of(&pool, OLD).await,
            Some(Some(DISCORD.to_string()))
        );
        clear(&pool, &[OLD, NEW]).await;
    }

    /// A `group_train_share_moves` row naming `user`. The table's only
    /// writer is the owner, so it is seeded as the owner
    /// (`MIGRATION_DATABASE_URL`) when the suite runs as the api's role.
    async fn seed_share_move(pool: &PgPool, user: &str) -> ShareMove {
        let owner = ds_store::test_support::schedule_fixture_pool(pool).await;
        let group_id = "TEST-CONTINUITY-GROUP";
        let mut tx = owner.begin().await.expect("begin");
        sqlx::query("DELETE FROM groups WHERE id = $1")
            .bind(group_id)
            .execute(&mut *tx)
            .await
            .expect("clear group");
        sqlx::query("INSERT INTO groups (id, name, created_by) VALUES ($1, 'Continuity', $2)")
            .bind(group_id)
            .bind(user)
            .execute(&mut *tx)
            .await
            .expect("seed group");
        let train_subscription_id: i64 = sqlx::query_scalar(
            "INSERT INTO train_subscriptions (user_id, service_date) \
             VALUES ($1, DATE '2099-01-01') RETURNING id",
        )
        .bind(user)
        .fetch_one(&mut *tx)
        .await
        .expect("seed train subscription");
        let journey_id: i64 =
            sqlx::query_scalar("INSERT INTO journeys (user_id) VALUES ($1) RETURNING id")
                .bind(user)
                .fetch_one(&mut *tx)
                .await
                .expect("seed journey");
        sqlx::query(
            "INSERT INTO group_train_share_moves \
                (group_id, train_subscription_id, journey_id, added_by, added_at, created_share) \
             VALUES ($1, $2, $3, $4, now(), true)",
        )
        .bind(group_id)
        .bind(train_subscription_id)
        .bind(journey_id)
        .bind(user)
        .execute(&mut *tx)
        .await
        .expect("seed share move");
        tx.commit().await.expect("commit seeds");
        ShareMove {
            owner,
            group_id: group_id.to_string(),
        }
    }

    struct ShareMove {
        owner: PgPool,
        group_id: String,
    }

    impl ShareMove {
        async fn added_by(&self) -> String {
            sqlx::query_scalar("SELECT added_by FROM group_train_share_moves WHERE group_id = $1")
                .bind(&self.group_id)
                .fetch_one(&self.owner)
                .await
                .expect("read added_by")
        }

        async fn clear(&self) {
            sqlx::query("DELETE FROM groups WHERE id = $1")
                .bind(&self.group_id)
                .execute(&self.owner)
                .await
                .expect("clear group");
        }
    }
}
