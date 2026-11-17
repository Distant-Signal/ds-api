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
//!
//! A re-key also ends every session the account had: they authenticated
//! under the previous subject, and the login that caused the re-key gets
//! its own session after [`login`] returns. Personal API tokens and
//! delegated grants are the person's and follow the account, unless
//! [`Options::revoke_tokens_on_rekey`] is set. With
//! [`Options::require_strong_match`] a primary-sub match must be backed by
//! the Discord id (see [`Refusal`]).

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

/// Suffix of `distant_signal_api_account_rekey_refused_total{reason}`: one
/// per re-key the strong-match rule declined (see [`Refusal`]).
pub const REFUSED_METRIC: &str = "api_account_rekey_refused_total";

/// Registers the counters at 0 for each label, so an alert's
/// `increase()` sees the first one after a restart.
pub fn register_metrics() {
    for refusal in [
        Refusal::PrimarySubNoDiscordId,
        Refusal::PrimarySubDiscordIdUnverified,
    ] {
        metrics::counter!(
            ds_runtime::metrics::metric_name(REFUSED_METRIC),
            "reason" => refusal.label()
        )
        .increment(0);
    }
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

/// A re-key declined by `ACCOUNT_CONTINUITY_REQUIRE_STRONG_MATCH`: the
/// login goes ahead as a new account (the same as when nothing matches), so
/// someone who can only set the primary-sub claim cannot take an account
/// over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The primary sub matched an account, but neither it nor the token
    /// carries a Discord id to back the match.
    PrimarySubNoDiscordId,
    /// The primary sub matched an account, but only one side has a Discord
    /// id, so the match is not corroborated.
    PrimarySubDiscordIdUnverified,
}

impl Refusal {
    pub fn label(self) -> &'static str {
        match self {
            Self::PrimarySubNoDiscordId => "primary_sub_no_discord_id",
            Self::PrimarySubDiscordIdUnverified => "primary_sub_discord_id_unverified",
        }
    }
}

/// The settings of a continuity login (`Config`'s
/// `account_continuity_*` fields). Both default to off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// A primary-sub match needs a corroborating Discord id: see
    /// [`Refusal`].
    pub require_strong_match: bool,
    /// A re-key also revokes the account's personal API tokens and
    /// delegated grants (they are kept by default).
    pub revoke_tokens_on_rekey: bool,
}

/// What a re-key revoked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Revoked {
    pub sessions: u64,
    pub tokens: u64,
    pub grants: u64,
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
/// login), what happened, any conflict, any declined re-key and what a
/// re-key revoked.
#[derive(Debug)]
pub struct Login {
    pub user: Option<User>,
    pub outcome: Outcome,
    pub conflict: Option<Conflict>,
    pub refused: Option<Refusal>,
    pub revoked: Revoked,
}

/// [`choose`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Chosen {
    NoMatch,
    Rekey(String, MatchedBy),
    Refused(Refusal),
}

/// Which account a login with no account of its own takes over: the
/// Discord id's match, else the primary sub's, unless they disagree.
/// `by_primary` is the primary sub's account and its stored Discord id.
/// With `require_strong` a primary-sub match needs the stored and claimed
/// Discord ids to be present and equal.
fn choose(
    by_discord: Option<&str>,
    by_primary: Option<(&str, Option<&str>)>,
    discord_claim: Option<&str>,
    require_strong: bool,
) -> Result<Chosen, Conflict> {
    match (by_discord, by_primary) {
        (Some(discord), Some((primary, _))) if discord != primary => Err(Conflict::AmbiguousMatch),
        (Some(discord), _) => Ok(Chosen::Rekey(discord.to_string(), MatchedBy::DiscordId)),
        (None, Some((primary, stored))) => match (stored, discord_claim) {
            (Some(stored), Some(claim)) if stored != claim => {
                Err(Conflict::PrimarySubAccountHasAnotherDiscordId)
            }
            (Some(_), Some(_)) => Ok(Chosen::Rekey(primary.to_string(), MatchedBy::PrimarySub)),
            _ if !require_strong => Ok(Chosen::Rekey(primary.to_string(), MatchedBy::PrimarySub)),
            (None, None) => Ok(Chosen::Refused(Refusal::PrimarySubNoDiscordId)),
            _ => Ok(Chosen::Refused(Refusal::PrimarySubDiscordIdUnverified)),
        },
        (None, None) => Ok(Chosen::NoMatch),
    }
}

/// The login callback's upsert with account continuity on, in one
/// transaction (module docs). `claims` are this login's continuity claims
/// (`auth::oidc::continuity_claims`); `options` are the settings.
///
/// Locks each candidate row (`FOR UPDATE`), so two concurrent first logins
/// of one user serialise: the second finds the row already re-keyed to its
/// sub and takes the ordinary path.
pub async fn login(
    pool: &PgPool,
    identity: &OidcIdentity,
    claims: &ContinuityClaims,
    options: &Options,
) -> Result<Login> {
    let new_sub = identity.sub.as_str();
    let discord_claim = claims.discord_id.as_deref();
    let mut tx = pool.begin().await?;

    let own: Option<String> = sqlx::query_scalar("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(new_sub)
        .fetch_optional(&mut *tx)
        .await?;

    let mut outcome = Outcome::Upserted;
    let mut refused = None;
    let mut revoked = Revoked::default();
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
            options.require_strong_match,
        );
        match chosen {
            Err(conflict) => {
                tx.rollback().await?;
                return Ok(Login {
                    user: None,
                    outcome,
                    conflict: Some(conflict),
                    refused: None,
                    revoked,
                });
            }
            Ok(Chosen::Rekey(from, matched_by)) => {
                rekey(&mut tx, &from, new_sub).await?;
                revoked = revoke_old_access(&mut tx, new_sub, options).await?;
                outcome = Outcome::Rekeyed { from, matched_by };
            }
            Ok(Chosen::Refused(refusal)) => refused = Some(refusal),
            Ok(Chosen::NoMatch) => {}
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
        refused,
        revoked,
    })
}

/// Ends what the re-keyed account's previous holder could still use: every
/// session (the callback creates the new login's session after this, so it
/// is not among them) and, with `revoke_tokens_on_rekey`, the personal API
/// tokens and delegated grants. Tokens and grants are marked revoked with
/// the `admin` reason, the one the `revoked_reason` check allows for an
/// operator-side revocation.
async fn revoke_old_access(
    conn: &mut sqlx::PgConnection,
    account: &str,
    options: &Options,
) -> Result<Revoked> {
    let sessions = sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(account)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    let mut revoked = Revoked {
        sessions,
        ..Revoked::default()
    };
    if options.revoke_tokens_on_rekey {
        revoked.tokens = crate::data::personal_tokens::revoke_all_for_user(
            &mut *conn,
            account,
            crate::data::personal_tokens::RevokeReason::Admin,
        )
        .await?;
        revoked.grants = crate::data::delegated::revoke_grants_for_user(
            &mut *conn,
            account,
            None,
            crate::data::delegated::RevokeReason::Admin,
        )
        .await?;
    }
    Ok(revoked)
}

/// Moves the account `from` to `to`: the `users` row (cascading to every
/// foreign key) and the copied ids without a foreign key:
/// `group_train_share_moves.added_by` and a calendar feed link's
/// `unlisted_links.resource_id` (the owner's id), so the feed's address
/// keeps working and the account page can still show and revoke it.
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
    sqlx::query(
        "UPDATE unlisted_links SET resource_id = $2 \
         WHERE resource_type = $3 AND resource_id = $1",
    )
    .bind(from)
    .bind(to)
    .bind(crate::data::calendar_feed::RESOURCE_TYPE)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rekey(from: &str, matched_by: MatchedBy) -> Chosen {
        Chosen::Rekey(from.to_string(), matched_by)
    }

    #[test]
    fn no_match_creates() {
        assert_eq!(choose(None, None, Some("1"), false), Ok(Chosen::NoMatch));
        assert_eq!(choose(None, None, None, false), Ok(Chosen::NoMatch));
        assert_eq!(choose(None, None, None, true), Ok(Chosen::NoMatch));
    }

    #[test]
    fn the_discord_match_wins_when_alone_or_the_same_account() {
        for strong in [false, true] {
            assert_eq!(
                choose(Some("old"), None, Some("1"), strong),
                Ok(rekey("old", MatchedBy::DiscordId))
            );
            assert_eq!(
                choose(Some("old"), Some(("old", Some("1"))), Some("1"), strong),
                Ok(rekey("old", MatchedBy::DiscordId))
            );
        }
    }

    #[test]
    fn the_primary_sub_matches_staff() {
        assert_eq!(
            choose(None, Some(("staff", None)), None, false),
            Ok(rekey("staff", MatchedBy::PrimarySub))
        );
        // A stored Discord id with no Discord claim, or the same one, is
        // no conflict.
        assert_eq!(
            choose(None, Some(("staff", Some("1"))), None, false),
            Ok(rekey("staff", MatchedBy::PrimarySub))
        );
        assert_eq!(
            choose(None, Some(("staff", None)), Some("1"), false),
            Ok(rekey("staff", MatchedBy::PrimarySub))
        );
    }

    #[test]
    fn a_strong_match_refuses_a_primary_sub_nothing_backs() {
        assert_eq!(
            choose(None, Some(("staff", None)), None, true),
            Ok(Chosen::Refused(Refusal::PrimarySubNoDiscordId))
        );
        assert_eq!(
            choose(None, Some(("staff", Some("1"))), None, true),
            Ok(Chosen::Refused(Refusal::PrimarySubDiscordIdUnverified))
        );
        assert_eq!(
            choose(None, Some(("staff", None)), Some("1"), true),
            Ok(Chosen::Refused(Refusal::PrimarySubDiscordIdUnverified))
        );
        // Equal ids on both sides back it (the Discord lookup finds this
        // account first, so it is only reachable through `choose` alone).
        assert_eq!(
            choose(None, Some(("staff", Some("1"))), Some("1"), true),
            Ok(rekey("staff", MatchedBy::PrimarySub))
        );
    }

    #[test]
    fn disagreeing_matches_are_conflicts_that_refuse_the_login() {
        for strong in [false, true] {
            let ambiguous = choose(Some("a"), Some(("b", None)), Some("1"), strong);
            assert_eq!(ambiguous, Err(Conflict::AmbiguousMatch));
            let other = choose(None, Some(("staff", Some("2"))), Some("1"), strong);
            assert_eq!(other, Err(Conflict::PrimarySubAccountHasAnotherDiscordId));
        }
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
    /// their personal API token, delegated grant and pin (not their
    /// session: see `a_rekey_ends_old_sessions_and_keeps_the_new_logins`), and the
    /// copied `group_train_share_moves.added_by` and calendar feed link.
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
        let feed_token = seed_feed_link(&pool, OLD).await;

        let result = login(
            &pool,
            &identity(NEW),
            &discord(DISCORD),
            &Options::default(),
        )
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

        assert_eq!(
            result.revoked,
            Revoked {
                sessions: 1,
                tokens: 0,
                grants: 0
            }
        );
        assert!(
            users::get_session_with_user(&pool, "TEST-CONTINUITY-SESSION")
                .await
                .expect("session lookup")
                .is_none(),
            "the previous holder's session ends with the re-key"
        );
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
        assert_feed_link_moved(&pool, &feed_token, NEW).await;

        // The next login is the ordinary path.
        let again = login_again(&pool, NEW, DISCORD).await;
        assert_eq!(again.outcome, Outcome::Upserted);
        clear(&pool, &[OLD, NEW]).await;
    }

    /// A calendar feed link for `user`; returns its token.
    async fn seed_feed_link(pool: &PgPool, user: &str) -> String {
        crate::data::unlisted_links::rotate_link(
            pool,
            crate::data::calendar_feed::RESOURCE_TYPE,
            user,
            user,
            None,
        )
        .await
        .expect("mint a calendar feed link")
        .token
    }

    /// The calendar feed link follows a re-key: its token resolves to the
    /// new id and the account page sees it as the active link.
    async fn assert_feed_link_moved(pool: &PgPool, token: &str, to: &str) {
        let resolved = crate::data::unlisted_links::resolve_link(pool, token)
            .await
            .expect("resolve the feed token")
            .expect("the feed link survives the re-key");
        assert_eq!(resolved.resource_id, to);
        assert!(
            crate::data::unlisted_links::get_active_link(
                pool,
                crate::data::calendar_feed::RESOURCE_TYPE,
                to
            )
            .await
            .expect("active link lookup")
            .is_some(),
            "the account page sees the moved link"
        );
    }

    async fn login_again(pool: &PgPool, sub: &str, discord_id: &str) -> Login {
        login(
            pool,
            &identity(sub),
            &discord(discord_id),
            &Options::default(),
        )
        .await
        .expect("login")
    }

    /// 2026-11-06: a first `ds-sso` login without name claims re-keys the
    /// account and keeps its name and username, and so does the next
    /// claimless login on the new sub.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api account_continuity -- \
                --ignored --test-threads=1`"]
    async fn a_rekey_and_a_claimless_login_keep_the_names() {
        const FROM: &str = "TEST-CONTINUITY-NAMES-OLD";
        const TO: &str = "TEST-CONTINUITY-NAMES-NEW";
        const SNOWFLAKE: &str = "900000000000000077";
        let pool = connect().await;
        clear(&pool, &[FROM, TO]).await;
        sqlx::query(
            "INSERT INTO users (id, name, username, discord_id) \
             VALUES ($1, 'Named Rider', 'named-rider', $2)",
        )
        .bind(FROM)
        .bind(SNOWFLAKE)
        .execute(&pool)
        .await
        .expect("seed user");
        let claimless = OidcIdentity {
            sub: TO.to_string(),
            name: None,
            preferred_username: None,
            groups: Vec::new(),
        };

        let result = login(&pool, &claimless, &discord(SNOWFLAKE), &Options::default())
            .await
            .expect("re-keying login");
        assert_eq!(
            result.outcome,
            Outcome::Rekeyed {
                from: FROM.to_string(),
                matched_by: MatchedBy::DiscordId,
            }
        );
        let user = result.user.expect("a user");
        assert_eq!(user.id, TO);
        assert!(!user.created);
        assert_eq!(user.name.as_deref(), Some("Named Rider"));
        assert_eq!(user.username.as_deref(), Some("named-rider"));

        let result = login(
            &pool,
            &claimless,
            &ContinuityClaims::default(),
            &Options::default(),
        )
        .await
        .expect("claimless login");
        assert_eq!(result.outcome, Outcome::Upserted);
        let user = result.user.expect("a user");
        assert_eq!(user.name.as_deref(), Some("Named Rider"));
        assert_eq!(user.username.as_deref(), Some("named-rider"));
        let stored: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT name, username FROM users WHERE id = $1")
                .bind(TO)
                .fetch_one(&pool)
                .await
                .expect("read the stored names");
        assert_eq!(
            stored,
            (
                Some("Named Rider".to_string()),
                Some("named-rider".to_string())
            )
        );
        clear(&pool, &[FROM, TO]).await;
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
        login(
            &pool,
            &identity(NEW),
            &ContinuityClaims::default(),
            &Options::default(),
        )
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
        let result = login(&pool, &identity(NEW), &claims, &Options::default())
            .await
            .expect("login");
        assert_eq!(result.user.expect("a user").id, NEW);
        assert_eq!(
            result.outcome,
            Outcome::Rekeyed {
                from: OLD.to_string(),
                matched_by: MatchedBy::PrimarySub
            }
        );
        assert!(
            users::get_session_with_user(&pool, "TEST-CONTINUITY-STAFF-SESSION")
                .await
                .expect("session lookup")
                .is_none(),
            "the previous holder's session ends with the re-key"
        );
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
        let result = login(&pool, &identity(NEW), &claims, &Options::default())
            .await
            .expect("login");
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

    const SESSION_OLD: &str = "TEST-CONTINUITY-REVOKE-SESSION-OLD";
    const SESSION_NEW: &str = "TEST-CONTINUITY-REVOKE-SESSION-NEW";
    const REVOKE_TOKEN_HASH: [u8; 32] = [9u8; 32];

    /// A session, a personal token and a delegated grant for `user`.
    async fn seed_access(pool: &PgPool, user: &str) {
        users::insert_session(pool, SESSION_OLD, user, 14)
            .await
            .expect("insert session");
        sqlx::query(
            "INSERT INTO personal_api_tokens (user_id, name, token_hash, hint, scopes, expires_at) \
             VALUES ($1, 'continuity', $2, 'abcd', ARRAY['journeys:read'], now() + interval '1 day')",
        )
        .bind(user)
        .bind(REVOKE_TOKEN_HASH.as_slice())
        .execute(pool)
        .await
        .expect("insert personal token");
        sqlx::query(
            "INSERT INTO delegated_grants (user_id, client, client_display_name, scopes) \
             VALUES ($1, 'continuity-test', 'Continuity', ARRAY['journeys:read'])",
        )
        .bind(user)
        .execute(pool)
        .await
        .expect("insert delegated grant");
    }

    async fn token_alive(pool: &PgPool) -> bool {
        crate::data::personal_tokens::find_active_by_hash(pool, &REVOKE_TOKEN_HASH)
            .await
            .expect("token lookup")
            .is_some()
    }

    async fn grant_alive(pool: &PgPool, user: &str) -> bool {
        crate::data::delegated::active_grant(pool, user, "continuity-test")
            .await
            .expect("grant lookup")
            .is_some()
    }

    async fn session_alive(pool: &PgPool, token: &str) -> bool {
        users::get_session_with_user(pool, token)
            .await
            .expect("session lookup")
            .is_some()
    }

    /// The previous holder's sessions end with a re-key; the session the
    /// callback creates for the new login (after `login` returns) works.
    /// The personal token and delegated grant survive by default.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api account_continuity -- \
                --ignored --test-threads=1`"]
    async fn a_rekey_ends_old_sessions_and_keeps_the_new_logins() {
        let pool = connect().await;
        clear(&pool, &[OLD, NEW]).await;
        seed_user(&pool, OLD, Some(DISCORD)).await;
        seed_access(&pool, OLD).await;

        let result = login_again(&pool, NEW, DISCORD).await;
        assert!(matches!(result.outcome, Outcome::Rekeyed { .. }));
        assert_eq!(
            result.revoked,
            Revoked {
                sessions: 1,
                tokens: 0,
                grants: 0
            }
        );
        assert!(!session_alive(&pool, SESSION_OLD).await);

        // What the callback does next.
        users::insert_session(&pool, SESSION_NEW, NEW, 14)
            .await
            .expect("insert the new login's session");
        assert!(session_alive(&pool, SESSION_NEW).await);
        assert!(token_alive(&pool).await, "tokens survive by default");
        assert!(grant_alive(&pool, NEW).await, "grants survive by default");

        // An ordinary later login revokes nothing.
        let again = login_again(&pool, NEW, DISCORD).await;
        assert_eq!(again.revoked, Revoked::default());
        assert!(session_alive(&pool, SESSION_NEW).await);
        clear(&pool, &[OLD, NEW]).await;
    }

    /// With `revoke_tokens_on_rekey` the token and grant die too.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api account_continuity -- \
                --ignored --test-threads=1`"]
    async fn the_strict_setting_also_revokes_tokens_and_grants() {
        let pool = connect().await;
        clear(&pool, &[OLD, NEW]).await;
        seed_user(&pool, OLD, Some(DISCORD)).await;
        seed_access(&pool, OLD).await;

        let options = Options {
            revoke_tokens_on_rekey: true,
            ..Options::default()
        };
        let result = login(&pool, &identity(NEW), &discord(DISCORD), &options)
            .await
            .expect("login");
        assert!(matches!(result.outcome, Outcome::Rekeyed { .. }));
        assert_eq!(
            result.revoked,
            Revoked {
                sessions: 1,
                tokens: 1,
                grants: 1
            }
        );
        assert!(!session_alive(&pool, SESSION_OLD).await);
        assert!(!token_alive(&pool).await);
        assert!(!grant_alive(&pool, NEW).await);
        clear(&pool, &[OLD, NEW]).await;
    }

    /// A primary sub with no Discord id on either side re-keys by default
    /// (the documented staff migration) and is refused with
    /// `require_strong_match`: the login becomes a new account and the old
    /// one, with its sessions, stays as it was.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api account_continuity -- \
                --ignored --test-threads=1`"]
    async fn a_weak_primary_sub_match_is_refused_only_with_the_strong_setting() {
        let pool = connect().await;
        clear(&pool, &[OLD, NEW]).await;
        seed_user(&pool, OLD, None).await;
        seed_access(&pool, OLD).await;
        let claims = ContinuityClaims {
            discord_id: None,
            primary_sub: Some(OLD.to_string()),
        };

        let strong = Options {
            require_strong_match: true,
            ..Options::default()
        };
        let result = login(&pool, &identity(NEW), &claims, &strong)
            .await
            .expect("login");
        assert_eq!(result.refused, Some(Refusal::PrimarySubNoDiscordId));
        assert_eq!(result.outcome, Outcome::Upserted);
        assert_eq!(result.revoked, Revoked::default());
        let user = result.user.expect("a user");
        assert_eq!(user.id, NEW);
        assert!(user.created, "the refused re-key creates a new account");
        assert_eq!(discord_of(&pool, OLD).await, Some(None), "OLD is untouched");
        assert!(session_alive(&pool, SESSION_OLD).await);
        clear(&pool, &[NEW]).await;

        let result = login(&pool, &identity(NEW), &claims, &Options::default())
            .await
            .expect("login");
        assert_eq!(result.refused, None);
        assert_eq!(
            result.outcome,
            Outcome::Rekeyed {
                from: OLD.to_string(),
                matched_by: MatchedBy::PrimarySub
            }
        );
        assert!(!result.user.expect("a user").created);
        clear(&pool, &[OLD, NEW]).await;
    }

    /// A Discord id match is strong: `require_strong_match` still re-keys.
    #[tokio::test]
    #[ignore = "requires a live database; run with `cargo test -p api account_continuity -- \
                --ignored --test-threads=1`"]
    async fn a_discord_match_rekeys_with_the_strong_setting() {
        let pool = connect().await;
        clear(&pool, &[OLD, NEW]).await;
        seed_user(&pool, OLD, Some(DISCORD)).await;
        let strong = Options {
            require_strong_match: true,
            ..Options::default()
        };
        let result = login(&pool, &identity(NEW), &discord(DISCORD), &strong)
            .await
            .expect("login");
        assert_eq!(result.refused, None);
        assert!(matches!(result.outcome, Outcome::Rekeyed { .. }));
        clear(&pool, &[OLD, NEW]).await;
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
