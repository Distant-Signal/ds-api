//! Every direct train between two stations (DS-398): each train that a
//! passenger may board at the origin and leave at the destination, without
//! changing, in departure order. Unlike the fastest search, a stopping
//! train overtaken by a later fast one is still listed.
//!
//! One pass over the day's connections (merged with a live overlay, so a
//! cancelled train drops out and a late one carries its live times):
//! a train is boarded at its first connection leaving an origin `TIPLOC`
//! with `can_board`, and found at its first later connection reaching a
//! destination `TIPLOC` with `can_alight`. A connection the restrictions
//! block (an avoided station, an operator ruled out) ends that ride.

use schedule_query::{Connection, StopCode, TripCode};

use crate::csa::{Journey, JourneyLeg, TrainLeg};
use crate::dense::FxMap;
use crate::overlay::ConnectionOverlay;
use crate::restrictions::{Restrictions, blocks};

/// Which end of the day a [`direct_journeys`] search pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectBound {
    /// Trains leaving the origin at or after these minutes: the first
    /// `limit` by departure.
    DepartAfter(u32),
    /// Trains reaching the destination by these minutes: the last `limit`
    /// by departure.
    ArriveBy(u32),
}

/// The stations a [`direct_journeys`] search runs between.
pub struct DirectOptions<'a> {
    /// Sorted by `departure_min`, as `build_connections` returns them.
    pub connections: &'a [Connection],
    pub from_tiplocs: &'a [String],
    pub to_tiplocs: &'a [String],
}

/// Up to `limit` direct trains, each a one-leg [`Journey`], in departure
/// order (see the module doc).
#[must_use]
pub fn direct_journeys(
    options: &DirectOptions<'_>,
    bound: DirectBound,
    limit: usize,
    overlay: Option<&ConnectionOverlay>,
    restrictions: Option<&Restrictions>,
) -> Vec<Journey> {
    if limit == 0 {
        return Vec::new();
    }
    let codes = |tiplocs: &[String]| -> Vec<StopCode> {
        let mut codes: Vec<StopCode> = tiplocs
            .iter()
            .map(|tiploc| StopCode::new(tiploc).normalized())
            .collect();
        codes.sort_unstable_by_key(|code| code.index());
        codes.dedup();
        codes
    };
    let (origins, destinations) = (codes(options.from_tiplocs), codes(options.to_tiplocs));
    let (from_min, deadline) = match bound {
        DirectBound::DepartAfter(from) => (from, None),
        DirectBound::ArriveBy(by) => (0, Some(by)),
    };
    let mut boarded: FxMap<TripCode, Connection> = FxMap::default();
    let mut found: Vec<Journey> = Vec::new();
    for connection in crate::overlay::connections_from(options.connections, overlay, from_min) {
        if deadline.is_some_and(|by| connection.departure_min > by) {
            break;
        }
        if let Some(board) = boarded.get(&connection.uid) {
            if blocks(restrictions, connection) {
                boarded.remove(&connection.uid);
                continue;
            }
            let reached = connection.can_alight
                && destinations.contains(&connection.to_tiploc.normalized())
                && deadline.is_none_or(|by| connection.arrival_min <= by);
            if reached {
                found.push(Journey {
                    legs: vec![JourneyLeg::Train(TrainLeg {
                        uid: board.uid.to_string(),
                        from_tiploc: board.from_tiploc.to_string(),
                        to_tiploc: connection.to_tiploc.to_string(),
                        departure_min: board.departure_min,
                        arrival_min: connection.arrival_min,
                        working_departure_min: board.working_departure_min,
                        working_arrival_min: connection.working_arrival_min,
                    })],
                    departure_min: board.departure_min,
                    arrival_min: connection.arrival_min,
                });
                boarded.remove(&connection.uid);
            }
            continue;
        }
        if connection.can_board
            && origins.contains(&connection.from_tiploc.normalized())
            && !blocks(restrictions, connection)
        {
            // A ride that is its own last hop.
            if connection.can_alight
                && destinations.contains(&connection.to_tiploc.normalized())
                && deadline.is_none_or(|by| connection.arrival_min <= by)
            {
                found.push(Journey {
                    legs: vec![JourneyLeg::Train(TrainLeg {
                        uid: connection.uid.to_string(),
                        from_tiploc: connection.from_tiploc.to_string(),
                        to_tiploc: connection.to_tiploc.to_string(),
                        departure_min: connection.departure_min,
                        arrival_min: connection.arrival_min,
                        working_departure_min: connection.working_departure_min,
                        working_arrival_min: connection.working_arrival_min,
                    })],
                    departure_min: connection.departure_min,
                    arrival_min: connection.arrival_min,
                });
            } else {
                boarded.insert(connection.uid, connection.clone());
            }
        }
    }
    found.sort_by_key(|journey| (journey.departure_min, journey.arrival_min));
    match bound {
        DirectBound::DepartAfter(_) => found.truncate(limit),
        DirectBound::ArriveBy(_) => {
            let skip = found.len().saturating_sub(limit);
            found.drain(..skip);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use schedule_query::OperatorId;

    fn hop(uid: &str, from: &str, to: &str, dep: u32, arr: u32) -> Connection {
        Connection {
            operator: OperatorId::UNKNOWN,
            uid: uid.into(),
            from_tiploc: from.into(),
            to_tiploc: to.into(),
            departure_min: dep,
            arrival_min: arr,
            working_departure_min: dep,
            working_arrival_min: arr,
            can_board: true,
            can_alight: true,
        }
    }

    /// A fast train F (08:30 A, 09:00 C) overtakes a stopper S (08:05 A,
    /// 08:40 B, 09:20 C); T (07:00 A to 07:30 B) never reaches C; X (08:10)
    /// leaves C, not A.
    fn day() -> Vec<Connection> {
        let mut connections = vec![
            hop("T", "A", "B", 420, 450),
            hop("S", "A", "B", 485, 520),
            hop("X", "C", "A", 490, 520),
            hop("S", "B", "C", 522, 560),
            hop("F", "A", "C", 510, 540),
        ];
        connections.sort_by_key(|c| c.departure_min);
        connections
    }

    fn uids(journeys: &[Journey]) -> Vec<String> {
        journeys
            .iter()
            .map(|journey| match &journey.legs[0] {
                JourneyLeg::Train(leg) => leg.uid.clone(),
                JourneyLeg::Transfer(_) => String::new(),
            })
            .collect()
    }

    fn search(connections: &[Connection], bound: DirectBound, limit: usize) -> Vec<Journey> {
        direct_journeys(
            &DirectOptions {
                connections,
                from_tiplocs: &["A".to_string()],
                to_tiplocs: &["C".to_string()],
            },
            bound,
            limit,
            None,
            None,
        )
    }

    #[test]
    fn every_direct_train_is_listed_in_departure_order_the_stopper_too() {
        let connections = day();
        let found = search(&connections, DirectBound::DepartAfter(480), 8);
        assert_eq!(uids(&found), ["S", "F"]);
        assert_eq!((found[0].departure_min, found[0].arrival_min), (485, 560));
        match &found[0].legs[0] {
            JourneyLeg::Train(leg) => assert_eq!(
                (leg.from_tiploc.as_str(), leg.to_tiploc.as_str()),
                ("A", "C")
            ),
            JourneyLeg::Transfer(_) => panic!("a train leg"),
        }
        // From after the stopper left, only the fast train.
        assert_eq!(
            uids(&search(&connections, DirectBound::DepartAfter(486), 8)),
            ["F"]
        );
        // The first one only.
        assert_eq!(
            uids(&search(&connections, DirectBound::DepartAfter(0), 1)),
            ["S"]
        );
    }

    #[test]
    fn arrive_by_keeps_the_last_trains_that_make_it() {
        let connections = day();
        assert_eq!(
            uids(&search(&connections, DirectBound::ArriveBy(600), 8)),
            ["S", "F"]
        );
        // The stopper arrives too late for 09:10; the fast train is in time.
        assert_eq!(
            uids(&search(&connections, DirectBound::ArriveBy(550), 8)),
            ["F"]
        );
        assert_eq!(
            uids(&search(&connections, DirectBound::ArriveBy(600), 1)),
            ["F"]
        );
        assert_eq!(
            uids(&search(&connections, DirectBound::ArriveBy(539), 8)),
            Vec::<String>::new()
        );
    }

    #[test]
    fn no_boarding_or_alighting_and_restrictions_are_honoured() {
        let mut connections = day();
        for connection in &mut connections {
            if connection.uid.as_str() == "S" && connection.to_tiploc.as_str() == "C" {
                connection.can_alight = false;
            }
        }
        assert_eq!(
            uids(&search(&connections, DirectBound::DepartAfter(480), 8)),
            ["F"]
        );

        // Never call at B: the stopper is out.
        let connections = day();
        let avoid = Restrictions::new(
            Vec::new(),
            vec!["B".to_string()],
            std::collections::HashMap::new(),
        );
        let found = direct_journeys(
            &DirectOptions {
                connections: &connections,
                from_tiplocs: &["A".to_string()],
                to_tiplocs: &["C".to_string()],
            },
            DirectBound::DepartAfter(480),
            8,
            None,
            Some(&avoid),
        );
        assert_eq!(uids(&found), ["F"]);
    }
}
