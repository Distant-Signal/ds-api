//! `GET /Trips/plan?results=fastest&count=` (DS-398): several trains on a
//! direct route, each search a minute on from the last train found, in
//! departure order.
//!
//! Seven direct trains ZSA (TSERA) to ZSB (TSERB) on a synthetic day, half
//! an hour apart and half an hour long: TSE070 07:00-07:30, TSE073
//! 07:30-08:00, ... TSE100 10:00-10:30.

use super::*;

const DATE: &str = "2099-06-24";

const TRAINS: [(&str, &str, &str); 7] = [
    ("TSE070", "07:00", "07:30"),
    ("TSE073", "07:30", "08:00"),
    ("TSE080", "08:00", "08:30"),
    ("TSE083", "08:30", "09:00"),
    ("TSE090", "09:00", "09:30"),
    ("TSE093", "09:30", "10:00"),
    ("TSE100", "10:00", "10:30"),
];

async fn exec(pool: &PgPool, sql: &str) {
    sqlx::query(sql).execute(pool).await.expect(sql);
}

async fn cleanup(pool: &PgPool) {
    for sql in [
        format!("DELETE FROM schedule_calling_points_full WHERE service_date = '{DATE}'"),
        format!("DELETE FROM schedule_services WHERE service_date = '{DATE}'"),
        "DELETE FROM stanox_crs WHERE stanox LIKE 'TESTSERIES-%'".to_string(),
    ] {
        sqlx::query(&sql).execute(pool).await.ok();
    }
}

async fn seed(pool: &PgPool) {
    crate::test_support::assert_synthetic_date(DATE.parse().expect("date"));
    cleanup(pool).await;
    exec(
        pool,
        "INSERT INTO stanox_crs (stanox, crs, tiploc, station_name, source_sequence) VALUES \
         ('TESTSERIES-A', 'ZSA', 'TSERA', 'Test series A', 1), \
         ('TESTSERIES-B', 'ZSB', 'TSERB', 'Test series B', 1) \
         ON CONFLICT (stanox) DO NOTHING",
    )
    .await;
    for (uid, dep, arr) in TRAINS {
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_calling_points_full \
                     (service_date, uid, seq, tiploc, kind, booked_departure, public_departure, \
                      booked_arrival, public_arrival, day_offset, can_board, can_alight) VALUES \
                 ('{DATE}', '{uid}', 1, 'TSERA', 'origin', '{dep}', '{dep}', NULL, NULL, 0, TRUE, TRUE), \
                 ('{DATE}', '{uid}', 2, 'TSERB', 'terminate', NULL, NULL, '{arr}', '{arr}', 0, TRUE, TRUE)"
            ),
        )
        .await;
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_services (service_date, uid, mode, stp, operator_atoc) \
                 VALUES ('{DATE}', '{uid}', 'train', 'P', 'SW')"
            ),
        )
        .await;
    }
}

async fn plan(router: &axum::Router, query: &str) -> (StatusCode, Value) {
    get(
        router.clone(),
        format!("/Trips/plan?origin=ZSA&destination=ZSB&date={DATE}&{query}"),
    )
    .await
}

/// Each itinerary's first train's UID, in the order served.
fn uids(body: &Value) -> Vec<String> {
    body["segments"][0]["itineraries"]
        .as_array()
        .expect("itineraries")
        .iter()
        .map(|itinerary| {
            itinerary["legs"][0]["trainUid"]
                .as_str()
                .expect("a train leg")
                .to_string()
        })
        .collect()
}

/// Each journey's departure, `HH:MM`.
fn departures(body: &Value) -> Vec<String> {
    body["journeys"]
        .as_array()
        .expect("journeys")
        .iter()
        .map(|journey| journey["departure"]["time"].as_str().expect("time")[..5].to_string())
        .collect()
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api \
            routes::trips -- --ignored --test-threads=1`"]
async fn count_lists_several_trains_in_departure_order_and_pages_a_minute_on() {
    let pool = connect().await;
    seed(&pool).await;
    let router = test_router(test_app(pool.clone()));

    // One `options` search on a direct route finds one train.
    let (status, options) = plan(&router, "departAfter=07:55&results=options").await;
    assert_eq!(status, StatusCode::OK, "{options:?}");
    assert_eq!(uids(&options), ["TSE080"]);

    // `count=5`: five trains, in departure order, with the journeys
    // aligned and the count echoed.
    let (status, body) = plan(&router, "departAfter=07:55&results=fastest&count=5").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(
        uids(&body),
        ["TSE080", "TSE083", "TSE090", "TSE093", "TSE100"]
    );
    assert_eq!(
        departures(&body),
        ["08:00", "08:30", "09:00", "09:30", "10:00"]
    );
    assert_eq!(body["count"], 5);
    assert_eq!(body["results"], "fastest");
    assert_eq!(body["departAfter"]["time"], "07:55:00");
    assert_eq!(body["segments"][0]["departAfter"]["time"], "07:55:00");

    // Later: a minute after the last train shown left. Only two are left,
    // and that's not an error.
    let (status, later) = plan(&router, "departAfter=09:01&results=fastest&count=5").await;
    assert_eq!(status, StatusCode::OK, "{later:?}");
    assert_eq!(uids(&later), ["TSE093", "TSE100"]);

    // A train leaving at exactly the time asked is the first.
    let (_, exact) = plan(&router, "departAfter=08:30&results=fastest&count=2").await;
    assert_eq!(uids(&exact), ["TSE083", "TSE090"]);

    // Without `count`, `fastest` is the one train, as before, and no
    // `count` is served.
    let (_, one) = plan(&router, "departAfter=07:55&results=fastest").await;
    assert_eq!(uids(&one), ["TSE080"]);
    assert!(one.get("count").is_none(), "{one:?}");

    cleanup(&pool).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api \
            routes::trips -- --ignored --test-threads=1`"]
async fn arrive_by_count_lists_the_latest_trains_that_make_it_in_departure_order() {
    let pool = connect().await;
    seed(&pool).await;
    let router = test_router(test_app(pool.clone()));

    // Arrive by 09:10: the latest three that make it, earliest first; the
    // one arriving at 09:00 isn't found twice.
    let (status, body) = plan(&router, "arriveBy=09:10&results=fastest&count=3").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(uids(&body), ["TSE073", "TSE080", "TSE083"]);
    assert_eq!(departures(&body), ["07:30", "08:00", "08:30"]);
    assert_eq!(body["arriveBy"]["time"], "09:10:00");

    // Earlier: a minute before the first train shown arrived.
    let (_, earlier) = plan(&router, "arriveBy=07:59&results=fastest&count=3").await;
    assert_eq!(uids(&earlier), ["TSE070"]);

    // Arriving at exactly the deadline makes it.
    let (_, exact) = plan(&router, "arriveBy=09:00&results=fastest&count=2").await;
    assert_eq!(uids(&exact), ["TSE080", "TSE083"]);

    cleanup(&pool).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api \
            routes::trips -- --ignored --test-threads=1`"]
async fn count_is_refused_out_of_range_and_outside_a_timed_fastest_search() {
    let pool = connect().await;
    let router = test_router(test_app(pool));
    // Refused before any read: no timetable is needed for the date.
    for (query, message) in [
        (
            "departAfter=08:00&count=0",
            "count must be a whole number from 1 to 8",
        ),
        (
            "departAfter=08:00&count=9",
            "count must be a whole number from 1 to 8",
        ),
        (
            "departAfter=08:00&count=two",
            "count must be a whole number from 1 to 8",
        ),
        (
            "departAfter=08:00&results=options&count=5",
            "count needs results=fastest",
        ),
        ("count=5", "count needs departAfter or arriveBy"),
        (
            "departAfter=08:00&waypoints=ZSB&count=5",
            "count can't be used with waypoints",
        ),
        (
            "departAfter=00:30&dayOffset=1&acrossMidnight=true&count=5",
            "count can't be used with acrossMidnight",
        ),
    ] {
        let (status, body) = get(
            router.clone(),
            format!("/Trips/plan?origin=ZSA&destination=ZSC&date=2099-01-01&{query}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body:?}");
        assert!(
            body.as_str().is_some_and(|text| text.contains(message)),
            "{query}: {body:?}"
        );
    }
}
