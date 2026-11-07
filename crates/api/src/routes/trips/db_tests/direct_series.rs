//! `GET /Trips/plan?results=fastest&count=&direct=all` (DS-398): every
//! direct train in departure order, a stopping train overtaken by a faster
//! one too, with the connecting journeys of the fastest series between.
//!
//! On a synthetic day, ZDA (TDIRA) to ZDB (TDIRB): DIR080 08:00-08:30,
//! the stopper DIR081 08:05-09:20 (GW; overtaken by DIR083), DIR083
//! 08:30-09:00, DIR100 10:00-10:30, and one connection by ZDC: DIR090
//! 09:00-09:10 to ZDC, then DIR092 09:25-09:45 on to ZDB.

use super::*;

const DATE: &str = "2099-06-25";

const TRAINS: [(&str, &str, &str, &str, &str, &str); 6] = [
    ("DIR080", "TDIRA", "TDIRB", "08:00", "08:30", "SW"),
    ("DIR081", "TDIRA", "TDIRB", "08:05", "09:20", "GW"),
    ("DIR083", "TDIRA", "TDIRB", "08:30", "09:00", "SW"),
    ("DIR090", "TDIRA", "TDIRC", "09:00", "09:10", "SW"),
    ("DIR092", "TDIRC", "TDIRB", "09:25", "09:45", "SW"),
    ("DIR100", "TDIRA", "TDIRB", "10:00", "10:30", "SW"),
];

async fn exec(pool: &PgPool, sql: &str) {
    sqlx::query(sql).execute(pool).await.expect(sql);
}

async fn cleanup(pool: &PgPool) {
    for sql in [
        format!("DELETE FROM schedule_calling_points_full WHERE service_date = '{DATE}'"),
        format!("DELETE FROM schedule_services WHERE service_date = '{DATE}'"),
        "DELETE FROM stanox_crs WHERE stanox LIKE 'TESTDIRECT-%'".to_string(),
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
         ('TESTDIRECT-A', 'ZDA', 'TDIRA', 'Test direct A', 1), \
         ('TESTDIRECT-B', 'ZDB', 'TDIRB', 'Test direct B', 1), \
         ('TESTDIRECT-C', 'ZDC', 'TDIRC', 'Test direct C', 1) \
         ON CONFLICT (stanox) DO NOTHING",
    )
    .await;
    for (uid, from, to, dep, arr, operator) in TRAINS {
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_calling_points_full \
                     (service_date, uid, seq, tiploc, kind, booked_departure, public_departure, \
                      booked_arrival, public_arrival, day_offset, can_board, can_alight) VALUES \
                 ('{DATE}', '{uid}', 1, '{from}', 'origin', '{dep}', '{dep}', NULL, NULL, 0, TRUE, TRUE), \
                 ('{DATE}', '{uid}', 2, '{to}', 'terminate', NULL, NULL, '{arr}', '{arr}', 0, TRUE, TRUE)"
            ),
        )
        .await;
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_services (service_date, uid, mode, stp, operator_atoc) \
                 VALUES ('{DATE}', '{uid}', 'train', 'P', '{operator}')"
            ),
        )
        .await;
    }
}

async fn plan(router: &axum::Router, query: &str) -> (StatusCode, Value) {
    get(
        router.clone(),
        format!("/Trips/plan?origin=ZDA&destination=ZDB&date={DATE}&{query}"),
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
async fn direct_all_lists_the_stopper_and_the_connection_in_departure_order() {
    let pool = connect().await;
    seed(&pool).await;
    let router = test_router(test_app(pool.clone()));

    // The fastest series alone misses the stopper.
    let (status, fastest) = plan(&router, "departAfter=07:55&results=fastest&count=5").await;
    assert_eq!(status, StatusCode::OK, "{fastest:?}");
    assert_eq!(uids(&fastest), ["DIR080", "DIR083", "DIR090", "DIR100"]);
    assert!(fastest.get("direct").is_none(), "{fastest:?}");

    // Every direct train, with the connection between.
    let (status, body) = plan(
        &router,
        "departAfter=07:55&results=fastest&count=5&direct=all",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(
        uids(&body),
        ["DIR080", "DIR081", "DIR083", "DIR090", "DIR100"]
    );
    assert_eq!(
        departures(&body),
        ["08:00", "08:05", "08:30", "09:00", "10:00"]
    );
    assert_eq!(body["direct"], "all");
    assert_eq!(body["count"], 5);
    assert_eq!(body["journeys"][1]["arrival"]["time"], "09:20:00");
    assert_eq!(body["journeys"][3]["changeCount"], 1);

    // Three: as far as both lists are complete; Later carries on.
    let (_, first) = plan(
        &router,
        "departAfter=07:55&results=fastest&count=3&direct=all",
    )
    .await;
    assert_eq!(uids(&first), ["DIR080", "DIR081", "DIR083"]);
    let (_, later) = plan(
        &router,
        "departAfter=08:31&results=fastest&count=3&direct=all",
    )
    .await;
    assert_eq!(uids(&later), ["DIR090", "DIR100"]);

    // Only SW: no stopper.
    let (_, sw) = plan(
        &router,
        "departAfter=07:55&results=fastest&count=5&direct=all&operator=SW",
    )
    .await;
    assert_eq!(uids(&sw), ["DIR080", "DIR083", "DIR090", "DIR100"]);

    cleanup(&pool).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api \
            routes::trips -- --ignored --test-threads=1`"]
async fn arrive_by_direct_all_keeps_the_last_that_make_it() {
    let pool = connect().await;
    seed(&pool).await;
    let router = test_router(test_app(pool.clone()));

    let (status, body) = plan(&router, "arriveBy=10:30&results=fastest&count=5&direct=all").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(
        uids(&body),
        ["DIR080", "DIR081", "DIR083", "DIR090", "DIR100"]
    );

    // By 09:30, two: the stopper (by 09:20) and the 08:30, not the 08:00.
    let (_, two) = plan(&router, "arriveBy=09:30&results=fastest&count=2&direct=all").await;
    assert_eq!(uids(&two), ["DIR081", "DIR083"]);

    cleanup(&pool).await;
}

#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api \
            routes::trips -- --ignored --test-threads=1`"]
async fn direct_needs_count_and_is_only_all() {
    let pool = connect().await;
    let router = test_router(test_app(pool));
    for (query, message) in [
        ("departAfter=08:00&direct=all", "direct=all needs count"),
        (
            "departAfter=08:00&count=5&direct=some",
            "direct must be 'all', not 'some'",
        ),
    ] {
        let (status, body) = get(
            router.clone(),
            format!("/Trips/plan?origin=ZDA&destination=ZDB&date=2099-01-01&{query}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {body:?}");
        assert!(
            body.as_str().is_some_and(|text| text.contains(message)),
            "{query}: {body:?}"
        );
    }
}
