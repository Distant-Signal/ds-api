//! Tracked Iarnród Éireann trains in the api's read models (distant-signal's
//! docs/superpowers/specs/2026-11-05-irish-rail-train-tracking-design.md
//! §5.3 and §5.4).
//!
//! A tracked Irish train is a `trains` row whose uid is `ie:<CODE>`
//! (`common::island_of_ireland::tracking`), with no CRS anywhere. The
//! reads mark it with `irishTrain` so clients can link to the Irish train
//! page, name its ends from `trains.origin_name`/`destination_name` (in
//! their SQL), and give it the operator's name. Its live state is the
//! writer's `train_current_state`, derived from the boards. Until the
//! writer has written one (it does at the next board that shows the
//! train), [`attach`] reads the same state from the stored board history,
//! the way the writer would. The api itself never writes
//! `train_current_state` (its role may only read it).

use chrono::{DateTime, NaiveDate, Utc};
use common::island_of_ireland::train_state::{self, CurrentState};
use common::island_of_ireland::{IslandOfIrelandNetwork, tracking};
use serde::Serialize;
use sqlx::PgPool;

/// `irishTrain` on a tracked train, a journey or a group row: which Irish
/// train page it is (`/island-of-ireland/iarnrod-eireann/trains/{trainCode}/{serviceDate}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ts_rs::TS, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IrishTrain {
    /// The train code, like `A120`.
    pub train_code: String,
    /// The day the train started (`Traindate`), `YYYY-MM-DD`.
    pub service_date: NaiveDate,
}

/// The [`IrishTrain`] of a row with `train_uid` on `service_date`, or
/// `None` for a GB train (or no train).
pub fn irish_train(train_uid: Option<&str>, service_date: NaiveDate) -> Option<IrishTrain> {
    let code = tracking::train_code_from_uid(train_uid?)?;
    Some(IrishTrain {
        train_code: code.to_owned(),
        service_date,
    })
}

/// A read-model row that can carry a tracked Irish train.
pub(crate) trait IrishTrainRow {
    /// The row's `train_uid` and service date.
    fn train_key(&self) -> Option<(&str, NaiveDate)>;
    /// Marks the row as Irish: `irishTrain`, and the operator's name where
    /// the row has one.
    fn set_irish_train(&mut self, train: IrishTrain);
    /// Whether the row has no `train_current_state` yet.
    fn lacks_state(&self) -> bool;
    /// Fills the live fields from the board history (no state row yet).
    fn set_board_state(&mut self, state: CurrentState);
}

/// Marks every Irish row in `rows` and, for those with no
/// `train_current_state` yet, reads their state from the board history at
/// `now` (one query for all of them). A read error is logged and leaves
/// the state unknown, never "on time".
pub(crate) async fn attach<T: IrishTrainRow>(pool: &PgPool, rows: &mut [T], now: DateTime<Utc>) {
    let mut wanted: Vec<(usize, String, NaiveDate)> = Vec::new();
    for (index, row) in rows.iter_mut().enumerate() {
        let Some(train) = row
            .train_key()
            .and_then(|(uid, date)| irish_train(Some(uid), date))
        else {
            continue;
        };
        if row.lacks_state() {
            wanted.push((index, train.train_code.clone(), train.service_date));
        }
        row.set_irish_train(train);
    }
    if wanted.is_empty() {
        return;
    }
    match board_states(pool, &wanted, now).await {
        Ok(states) => {
            for (index, state) in states {
                if let Some(row) = rows.get_mut(index) {
                    row.set_board_state(state);
                }
            }
        }
        Err(err) => {
            tracing::warn!(error = ?err, "could not read Irish board history for tracked trains");
        }
    }
}

#[derive(sqlx::FromRow)]
struct HistoryRow {
    train_code: String,
    train_date: NaiveDate,
    destination: String,
    status: String,
    late_minutes: Option<i32>,
    last_location: Option<String>,
    last_seen_at: DateTime<Utc>,
}

/// The state each `(index, code, date)` would get from the writer now
/// (`train_state::current_state`), for those with a history row that says
/// something current.
async fn board_states(
    pool: &PgPool,
    wanted: &[(usize, String, NaiveDate)],
    now: DateTime<Utc>,
) -> anyhow::Result<Vec<(usize, CurrentState)>> {
    let rows: Vec<HistoryRow> = sqlx::query_as(
        "SELECT h.train_code, h.train_date, h.destination, h.status, h.late_minutes, \
                h.last_location, h.last_seen_at \
         FROM island_of_ireland_trains h \
         JOIN UNNEST($2::text[], $3::date[]) AS k(train_code, train_date) \
           ON h.train_code = k.train_code AND h.train_date = k.train_date \
         WHERE h.network = $1",
    )
    .bind(ds_store::samples::island_of_ireland::network_wire(
        IslandOfIrelandNetwork::RepublicOfIreland,
    ))
    .bind(
        wanted
            .iter()
            .map(|(_, code, _)| code.as_str())
            .collect::<Vec<_>>(),
    )
    .bind(wanted.iter().map(|(_, _, date)| *date).collect::<Vec<_>>())
    .fetch_all(pool)
    .await?;
    let mut states = Vec::new();
    for (index, code, date) in wanted {
        let Some(history) = rows
            .iter()
            .find(|row| row.train_code == *code && row.train_date == *date)
        else {
            continue;
        };
        let read = train_state::interpret(
            &train_state::Observation {
                status: &history.status,
                late_minutes: history.late_minutes,
                last_location: history.last_location.as_deref(),
                destination: &history.destination,
                last_seen_at: history.last_seen_at,
            },
            now,
        );
        if let Some(state) = train_state::current_state(&read) {
            states.push((*index, state));
        }
    }
    Ok(states)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_well_formed_ie_uid_is_an_irish_train() {
        let date: NaiveDate = "2026-11-05".parse().unwrap();
        assert_eq!(
            irish_train(Some("ie:A120"), date),
            Some(IrishTrain {
                train_code: "A120".into(),
                service_date: date
            })
        );
        assert_eq!(irish_train(Some("C12345"), date), None);
        assert_eq!(irish_train(Some("luas:3"), date), None);
        assert_eq!(irish_train(None, date), None);
    }
}
