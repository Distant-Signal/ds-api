//! A journey leg's ticket details (2026-11-06): an optional ticket type
//! (single, return, advance, off-peak, season or unknown), whether it is
//! valid on one train company only, and an optional name. They replace the
//! standalone tickets and the PDF upload, and they only ever refine an
//! estimate: Delay Repay is estimated on every tracked GB train leg
//! without them (`routes::journey_tickets`).
//!
//! The store is still `tracked_train_tickets`, one row on the leg's train
//! subscription, so nothing had to be migrated or dropped, and everything
//! that already follows a ticket keeps working: a replacement moves it to
//! the new train (`journeys::release_leg_subscription`), a revision
//! snapshot records its id, the alternatives' ticket fit reads it, the
//! account export lists it and retention keeps a regular journey with one.
//!
//! - `ticket_type` holds one of [`TicketType`]'s stored spellings. A row
//!   written before 2026-11 can hold free text, read with
//!   [`TicketType::from_text`].
//! - `operator` is set exactly when the ticket is for one train company
//!   only: that company's ATOC code (`delay_repay_rules::scheme_for` and
//!   the alternatives' ticket fit both read a code there).
//! - `custom_name` is the ticket's name; `source` says whether the details
//!   came from an Apple Wallet pass.
//!
//! A leg holds ticket details only once it has a train: an unmatched or
//! untracked leg has no subscription to hang them on. Two legs bound to one
//! subscription (the same train in two journeys) share them.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// The kind of ticket, as far as Delay Repay and the alternatives care.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "LegTicketType")]
#[schemars(rename = "LegTicketType")]
pub enum TicketType {
    Single,
    Return,
    /// Always a single, valid on the booked train only.
    Advance,
    /// An Off-Peak or Super Off-Peak ticket that doesn't say single or
    /// return.
    OffPeak,
    Season,
    Unknown,
}

impl TicketType {
    /// The value stored in `tracked_train_tickets.ticket_type`.
    pub fn stored(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Return => "return",
            Self::Advance => "advance",
            Self::OffPeak => "off_peak",
            Self::Season => "season",
            Self::Unknown => "unknown",
        }
    }

    /// A stored value, or free text (a row from before 2026-11, a Wallet
    /// pass's own wording), by whole words: a season wins, then an
    /// Advance, a return, a single, then an Off-Peak that says neither.
    pub fn from_text(text: Option<&str>) -> Self {
        let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
            return Self::Unknown;
        };
        let lower = text.to_lowercase();
        let words: Vec<&str> = lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        let has = |options: &[&str]| words.iter().any(|w| options.contains(w));
        if has(&["season", "flexi"]) {
            Self::Season
        } else if has(&["advance"]) {
            Self::Advance
        } else if has(&["return", "rtn"]) {
            Self::Return
        } else if has(&["single", "sgl", "sngl"]) {
            Self::Single
        } else if has(&["offpeak"])
            || lower.contains("off peak")
            || lower.contains("off_peak")
            || lower.contains("off-peak")
        {
            Self::OffPeak
        } else {
            Self::Unknown
        }
    }
}

/// Where a leg's ticket details came from.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS, schemars::JsonSchema,
)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "LegTicketSource")]
#[schemars(rename = "LegTicketSource")]
pub enum TicketSource {
    /// Typed in.
    Manual,
    /// Prefilled from an Apple Wallet pass, then saved.
    Wallet,
}

impl TicketSource {
    fn from_stored(source: &str) -> Self {
        if source.starts_with("pkpass") {
            Self::Wallet
        } else {
            Self::Manual
        }
    }
}

/// A leg's ticket details, as `GET /Journeys/{id}` serves them on each of
/// the owner's train legs and `PUT .../ticket` answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
#[ts(rename = "LegTicket")]
#[schemars(rename = "LegTicket")]
pub struct LegTicket {
    pub ticket_type: TicketType,
    /// Valid on [`Self::operator_code`]'s trains only.
    pub operator_only: bool,
    /// The train company it is limited to (an ATOC code), set exactly when
    /// `operator_only` is.
    pub operator_code: Option<String>,
    pub name: Option<String>,
    pub source: TicketSource,
    /// The departure the Wallet pass gave, when it gave one.
    pub departure_date: Option<DateTime<Utc>>,
    /// The `tracked_train_tickets` row.
    #[serde(skip)]
    pub id: i64,
}

impl LegTicket {
    /// The `operator` text Delay Repay reads: the one company it is valid
    /// on, else nothing (the train's own operator decides).
    pub fn operator(&self) -> Option<&str> {
        self.operator_code.as_deref()
    }

    /// The type text Delay Repay reads for single or return
    /// (`delay_repay_rules::ticket_kind`).
    pub fn kind_text(&self) -> &'static str {
        self.ticket_type.stored()
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    tracked_train_id: i64,
    operator: Option<String>,
    ticket_type: Option<String>,
    custom_name: Option<String>,
    source: String,
    current_departure_date: Option<DateTime<Utc>>,
}

impl From<Row> for LegTicket {
    fn from(row: Row) -> Self {
        let operator_code = row
            .operator
            .map(|o| o.trim().to_string())
            .filter(|o| !o.is_empty());
        Self {
            ticket_type: TicketType::from_text(row.ticket_type.as_deref()),
            operator_only: operator_code.is_some(),
            operator_code,
            name: row.custom_name,
            source: TicketSource::from_stored(&row.source),
            departure_date: row.current_departure_date,
            id: row.id,
        }
    }
}

/// The ticket details on each of `subscription_ids` that has some: the
/// newest row when an older release left more than one.
pub async fn for_subscriptions(
    pool: &PgPool,
    user_id: &str,
    subscription_ids: &[i64],
) -> anyhow::Result<HashMap<i64, LegTicket>> {
    if subscription_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT DISTINCT ON (tracked_train_id) id, tracked_train_id, operator, ticket_type, \
                custom_name, source, current_departure_date \
         FROM tracked_train_tickets \
         WHERE tracked_train_id = ANY($1) AND user_id = $2 \
         ORDER BY tracked_train_id, id DESC",
    )
    .bind(subscription_ids)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.tracked_train_id, LegTicket::from(row)))
        .collect())
}

/// What `PUT .../ticket` stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketDetails {
    pub ticket_type: TicketType,
    /// The company it is limited to, when it is.
    pub operator_code: Option<String>,
    pub name: Option<String>,
    pub source: TicketSource,
    /// The Wallet pass's departure; kept only with a Wallet source.
    pub departure_date: Option<DateTime<Utc>>,
}

/// A leg as [`set_for_leg`] and [`clear_for_leg`] need it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegLookup {
    /// No such leg on a journey of this user's.
    NotFound,
    /// A leg with no train yet (or an untracked leg): nowhere to keep
    /// ticket details.
    NoTrain,
    /// The leg's subscription, with its train's UID and date when known.
    Train {
        subscription_id: i64,
        train_uid: Option<String>,
        service_date: chrono::NaiveDate,
    },
}

/// Leg `leg_id` of journey `journey_id`, when `user_id` owns it.
pub async fn lookup_leg(
    pool: &PgPool,
    user_id: &str,
    journey_id: i64,
    leg_id: i64,
) -> anyhow::Result<LegLookup> {
    let row: Option<(Option<i64>, Option<String>, chrono::NaiveDate)> = sqlx::query_as(
        "SELECT jl.train_subscription_id, tr.train_uid, jl.service_date \
         FROM journey_legs jl \
         JOIN journeys j ON j.id = jl.journey_id \
         LEFT JOIN train_subscriptions ts ON ts.id = jl.train_subscription_id \
         LEFT JOIN trains tr ON tr.id = ts.trains_id \
         WHERE jl.id = $1 AND jl.journey_id = $2 AND j.user_id = $3",
    )
    .bind(leg_id)
    .bind(journey_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(match row {
        None => LegLookup::NotFound,
        Some((None, _, _)) => LegLookup::NoTrain,
        Some((Some(subscription_id), train_uid, service_date)) => LegLookup::Train {
            subscription_id,
            train_uid,
            service_date,
        },
    })
}

/// Stores `details` as the ticket on `subscription_id` (one of
/// `user_id`'s): the newest existing row is updated, so its id (in any
/// revision snapshot) stays, and any older ones are deleted. Returns what
/// is now stored.
pub async fn set_for_subscription(
    pool: &PgPool,
    user_id: &str,
    subscription_id: i64,
    details: &TicketDetails,
) -> anyhow::Result<LegTicket> {
    let mut tx = pool.begin().await?;
    // Serialises two saves on one subscription.
    let owned: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM train_subscriptions WHERE id = $1 AND user_id = $2 FOR UPDATE",
    )
    .bind(subscription_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    anyhow::ensure!(
        owned.is_some(),
        "subscription {subscription_id} is not the user's"
    );
    let source = match details.source {
        TicketSource::Manual => "manual",
        TicketSource::Wallet => "pkpass-semantics",
    };
    let departure = details
        .departure_date
        .filter(|_| details.source == TicketSource::Wallet);
    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM tracked_train_tickets WHERE tracked_train_id = $1 AND user_id = $2 \
         ORDER BY id DESC LIMIT 1",
    )
    .bind(subscription_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let row: Row = match existing {
        Some((id,)) => {
            sqlx::query(
                "DELETE FROM tracked_train_tickets \
                 WHERE tracked_train_id = $1 AND user_id = $2 AND id <> $3",
            )
            .bind(subscription_id)
            .bind(user_id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            sqlx::query_as(
                "UPDATE tracked_train_tickets \
                 SET ticket_type = $2, operator = $3, custom_name = $4, source = $5, \
                     current_departure_date = $6, origin_crs = NULL, destination_crs = NULL \
                 WHERE id = $1 \
                 RETURNING id, tracked_train_id, operator, ticket_type, custom_name, source, \
                           current_departure_date",
            )
            .bind(id)
            .bind(details.ticket_type.stored())
            .bind(&details.operator_code)
            .bind(&details.name)
            .bind(source)
            .bind(departure)
            .fetch_one(&mut *tx)
            .await?
        }
        None => {
            sqlx::query_as(
                "INSERT INTO tracked_train_tickets \
                     (tracked_train_id, user_id, ticket_type, operator, custom_name, source, \
                      current_departure_date) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 RETURNING id, tracked_train_id, operator, ticket_type, custom_name, source, \
                           current_departure_date",
            )
            .bind(subscription_id)
            .bind(user_id)
            .bind(details.ticket_type.stored())
            .bind(&details.operator_code)
            .bind(&details.name)
            .bind(source)
            .bind(departure)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    tx.commit().await?;
    Ok(row.into())
}

/// Removes the ticket details on `subscription_id`. Returns whether there
/// were any.
pub async fn clear_for_subscription(
    pool: &PgPool,
    user_id: &str,
    subscription_id: i64,
) -> anyhow::Result<bool> {
    Ok(sqlx::query(
        "DELETE FROM tracked_train_tickets WHERE tracked_train_id = $1 AND user_id = $2",
    )
    .bind(subscription_id)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_type_is_read_from_stored_values_and_free_text() {
        for kind in [
            TicketType::Single,
            TicketType::Return,
            TicketType::Advance,
            TicketType::OffPeak,
            TicketType::Season,
            TicketType::Unknown,
        ] {
            assert_eq!(TicketType::from_text(Some(kind.stored())), kind);
        }
        for (text, kind) in [
            ("Advance Single", TicketType::Advance),
            ("Off-Peak Day Return", TicketType::Return),
            ("Super Off-Peak", TicketType::OffPeak),
            ("Anytime Day Single", TicketType::Single),
            ("7 Day Season", TicketType::Season),
            ("Flexi Season", TicketType::Season),
            ("Rover", TicketType::Unknown),
            ("  ", TicketType::Unknown),
        ] {
            assert_eq!(TicketType::from_text(Some(text)), kind, "{text}");
        }
        assert_eq!(TicketType::from_text(None), TicketType::Unknown);
    }

    #[test]
    fn the_wire_spellings_are_camel_case() {
        assert_eq!(
            serde_json::to_value(TicketType::OffPeak).unwrap(),
            serde_json::json!("offPeak")
        );
        assert_eq!(
            serde_json::from_value::<TicketType>(serde_json::json!("advance")).unwrap(),
            TicketType::Advance
        );
    }

    #[test]
    fn a_ticket_limited_to_one_company_says_which() {
        let ticket = LegTicket::from(Row {
            id: 1,
            tracked_train_id: 2,
            operator: Some(" GR ".to_string()),
            ticket_type: Some("off_peak".to_string()),
            custom_name: None,
            source: "pkpass-heuristic".to_string(),
            current_departure_date: None,
        });
        assert!(ticket.operator_only);
        assert_eq!(ticket.operator(), Some("GR"));
        assert_eq!(ticket.ticket_type, TicketType::OffPeak);
        assert_eq!(ticket.source, TicketSource::Wallet);
        let json = serde_json::to_value(&ticket).unwrap();
        assert!(json.get("id").is_none(), "{json}");
        assert_eq!(json["ticketType"], "offPeak");
    }
}
