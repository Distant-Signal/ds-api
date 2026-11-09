//! Leave-by times (regular journeys design §5): a journey's station buffer
//! (the user's own minutes to get to the station, margin included) and the
//! time to leave that it gives, from `ds_store::journeys::leave_by_sql`,
//! the same SQL the notifier's leave-by push reads.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;

/// The buffers a user may set, in minutes (and the columns' CHECKs).
pub const STATION_BUFFER_MINUTES: std::ops::RangeInclusive<i16> = 0..=120;

/// Checks a station buffer, or says what a 400 says.
pub fn validate_station_buffer(minutes: Option<i16>) -> Result<(), String> {
    match minutes {
        Some(minutes) if !STATION_BUFFER_MINUTES.contains(&minutes) => {
            Err("The time to get to the station must be 0 to 120 minutes.".to_string())
        }
        _ => Ok(()),
    }
}

/// What a leave-by is timed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub enum LeaveByBasis {
    /// The plan's first timed leg (lateness never moves it).
    Booked,
    /// The first train is cancelled: the open break's best covered
    /// suggestion.
    Suggestion,
    /// A remind-me occurrence nobody has picked a train for: the plan
    /// sweep's suggestion, the usual train. Not tracked or booked.
    Usual,
    /// As [`Self::Usual`], for the usual operator's nearest train (a retime
    /// or a new UID) rather than the usual train itself.
    Closest,
}

/// Declared as an extensible enum (`x-extensible-enum`, not `enum` or a
/// `oneOf` of constants): the set grows (`usual` and `closest` were added
/// after `booked` and `suggestion`), and the `OpenAPI` breaking-change check
/// rejects a new value in a closed one. The meanings are in the description.
impl schemars::JsonSchema for LeaveByBasis {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "LeaveByBasis".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "What a leave-by is timed from. The set may gain values: \
                treat an unknown one as `booked`. `booked`: the plan's first timed \
                leg (lateness never moves it). `suggestion`: the first train is \
                cancelled, so the open break's best covered suggestion. `usual`: a \
                remind-me occurrence nobody has picked a train for, so the plan \
                sweep's suggestion, the usual train; not tracked or booked. \
                `closest`: as `usual`, for the usual operator's nearest train (a \
                retime or a new UID) rather than the usual train itself.",
            "x-extensible-enum": ["booked", "suggestion", "usual", "closest"]
        })
    }
}

impl LeaveByBasis {
    /// `ds_store::journeys::leave_by_sql`'s `basis` column.
    fn from_sql(basis: &str) -> Self {
        match basis {
            ds_store::journeys::BASIS_SUGGESTION => Self::Suggestion,
            ds_store::journeys::BASIS_USUAL => Self::Usual,
            ds_store::journeys::BASIS_CLOSEST => Self::Closest,
            _ => Self::Booked,
        }
    }
}

/// When to leave for a journey.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LeaveBy {
    /// `departure` less the station buffer.
    pub at: DateTime<Utc>,
    /// The departure it is timed from: the first timed leg's booked
    /// departure, or the suggestion's (a cancelled train's replacement, or
    /// the plan sweep's for a train nobody has picked).
    pub departure: DateTime<Utc>,
    pub basis: LeaveByBasis,
}

/// A journey's buffer and leave-by.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JourneyLeaveBy {
    pub station_buffer_minutes: Option<i16>,
    pub leave_by: Option<LeaveBy>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    station_buffer_minutes: Option<i16>,
    leave_by_at: Option<DateTime<Utc>>,
    departure: Option<DateTime<Utc>>,
    basis: Option<String>,
}

/// The buffer and leave-by of each of `ids` that `user_id` owns. A journey
/// they don't own is absent: the buffer is the owner's own business.
pub async fn for_journeys(
    pool: &PgPool,
    ids: &[i64],
    user_id: &str,
) -> anyhow::Result<HashMap<i64, JourneyLeaveBy>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<Row> = sqlx::query_as(&format!(
        "SELECT j.id, j.station_buffer_minutes, lb.leave_by_at, lb.departure, lb.basis \
         FROM journeys j \
         LEFT JOIN LATERAL ({}) lb ON TRUE \
         WHERE j.id = ANY($1) AND j.user_id = $2",
        ds_store::journeys::leave_by_sql("j")
    ))
    .bind(ids)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let leave_by = match (row.leave_by_at, row.departure, row.basis.as_deref()) {
                (Some(at), Some(departure), Some(basis)) => Some(LeaveBy {
                    at,
                    departure,
                    basis: LeaveByBasis::from_sql(basis),
                }),
                _ => None,
            };
            (
                row.id,
                JourneyLeaveBy {
                    station_buffer_minutes: row.station_buffer_minutes,
                    leave_by,
                },
            )
        })
        .collect())
}

/// Sets (or, with `None`, clears) the station buffer of journey
/// `journey_id`, for that day only. `false` when `user_id` doesn't own it.
pub async fn set_journey_buffer(
    pool: &PgPool,
    journey_id: i64,
    user_id: &str,
    minutes: Option<i16>,
) -> anyhow::Result<bool> {
    let result = sqlx::query(
        "UPDATE journeys SET station_buffer_minutes = $3, updated_at = now() \
         WHERE id = $1 AND user_id = $2",
    )
    .bind(journey_id)
    .bind(user_id)
    .bind(minutes)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// The buffer to pre-fill for a regular journey from station `crs`
/// (decision 6): the one on `user_id`'s most recently updated template from
/// there, else on their most recently updated journey from there. A
/// template is "from" its target's origin, else its first leg's; a journey
/// from its first train leg's.
pub async fn prefill_for_station(
    pool: &PgPool,
    user_id: &str,
    crs: &str,
) -> anyhow::Result<Option<i16>> {
    Ok(sqlx::query_scalar(
        "SELECT minutes FROM ( \
             SELECT jt.station_buffer_minutes AS minutes, 0 AS rank, jt.updated_at AS at \
             FROM journey_templates jt \
             WHERE jt.user_id = $1 AND jt.station_buffer_minutes IS NOT NULL \
               AND UPPER(COALESCE(jt.origin_crs, ( \
                   SELECT l.origin_crs FROM journey_template_legs l \
                   WHERE l.template_id = jt.id AND l.variant_weekday IS NULL \
                   ORDER BY l.leg_order LIMIT 1))) = $2 \
             UNION ALL \
             SELECT j.station_buffer_minutes, 1, j.updated_at \
             FROM journeys j \
             WHERE j.user_id = $1 AND j.station_buffer_minutes IS NOT NULL \
               AND UPPER(( \
                   SELECT COALESCE(jl.origin_crs, ts.pin_origin_crs) FROM journey_legs jl \
                   LEFT JOIN train_subscriptions ts ON ts.id = jl.train_subscription_id \
                   WHERE jl.journey_id = j.id AND jl.kind = 'train' \
                   ORDER BY jl.leg_order LIMIT 1)) = $2 \
         ) found \
         ORDER BY rank, at DESC \
         LIMIT 1",
    )
    .bind(user_id)
    .bind(crs.trim().to_ascii_uppercase())
    .fetch_optional(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_buffer_is_0_to_120_minutes() {
        assert!(validate_station_buffer(None).is_ok());
        assert!(validate_station_buffer(Some(0)).is_ok());
        assert!(validate_station_buffer(Some(120)).is_ok());
        assert!(validate_station_buffer(Some(121)).is_err());
        assert!(validate_station_buffer(Some(-1)).is_err());
    }

    #[test]
    fn the_basis_is_camel_case_on_the_wire() {
        assert_eq!(
            serde_json::to_value(LeaveByBasis::Suggestion).unwrap(),
            serde_json::json!("suggestion")
        );
        assert_eq!(
            serde_json::to_value(LeaveByBasis::Usual).unwrap(),
            serde_json::json!("usual")
        );
        assert_eq!(
            serde_json::to_value(LeaveByBasis::Closest).unwrap(),
            serde_json::json!("closest")
        );
    }

    /// Every basis `leave_by_sql` can give maps to its own value; an unknown
    /// one is the plan's booked time, as before.
    #[test]
    fn every_sql_basis_has_its_own_value() {
        use ds_store::journeys::{BASIS_BOOKED, BASIS_CLOSEST, BASIS_SUGGESTION, BASIS_USUAL};
        assert_eq!(LeaveByBasis::from_sql(BASIS_BOOKED), LeaveByBasis::Booked);
        assert_eq!(
            LeaveByBasis::from_sql(BASIS_SUGGESTION),
            LeaveByBasis::Suggestion
        );
        assert_eq!(LeaveByBasis::from_sql(BASIS_USUAL), LeaveByBasis::Usual);
        assert_eq!(LeaveByBasis::from_sql(BASIS_CLOSEST), LeaveByBasis::Closest);
        assert_eq!(LeaveByBasis::from_sql("other"), LeaveByBasis::Booked);
    }
}
