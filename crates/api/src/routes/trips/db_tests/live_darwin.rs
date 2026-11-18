//! Darwin Push Port times in the per-train reads of `/Trips/plan` (DS-591):
//! where TRUST has no data for a train, or none for a call, Darwin's actual
//! and forecast times fill in; TRUST stays the default where it has data.
//! Each test seeds its own synthetic stations and trains for [`live_date`]
//! and pins "now" to [`live_now`] (09:30 BST). Darwin rows are
//! `TDW-<uid>`.

use serde_json::Value;

use super::*;

/// A fixed message time for every seeded Darwin row, so each location is in
/// its service's newest schedule (`ds_store::darwin::calls_for_trains`
/// drops rows older than the newest schedule's).
const DARWIN_TS: &str = "2026-10-05T07:00:00Z";

/// One seeded `darwin_calls` row: TIPLOC, working arrival and departure
/// (`""` for none), forecast and actual arrival, forecast and actual
/// departure.
struct DarwinCall<'a> {
    tiploc: &'a str,
    wta: &'a str,
    wtd: &'a str,
    arrival_et: Option<&'a str>,
    arrival_at: Option<&'a str>,
    departure_et: Option<&'a str>,
    departure_at: Option<&'a str>,
}

impl<'a> DarwinCall<'a> {
    fn origin(tiploc: &'a str, wtd: &'a str) -> Self {
        Self {
            tiploc,
            wta: "",
            wtd,
            arrival_et: None,
            arrival_at: None,
            departure_et: Some(wtd),
            departure_at: None,
        }
    }

    fn terminus(tiploc: &'a str, wta: &'a str) -> Self {
        Self {
            tiploc,
            wta,
            wtd: "",
            arrival_et: None,
            arrival_at: None,
            departure_et: None,
            departure_at: None,
        }
    }

    fn arriving_forecast(mut self, et: &'a str) -> Self {
        self.arrival_et = Some(et);
        self
    }

    fn arriving_actual(mut self, at: &'a str) -> Self {
        self.arrival_at = Some(at);
        self
    }
}

async fn seed_darwin(pool: &PgPool, uid: &str, calls: &[DarwinCall<'_>]) {
    let rid = format!("TDW-{uid}");
    let ts: chrono::DateTime<chrono::Utc> = DARWIN_TS.parse().unwrap();
    sqlx::query(
        "INSERT INTO darwin_services (rid, uid, ssd, toc, is_passenger, is_deleted, \
                                      schedule_ts, message_ts) \
         VALUES ($1, $2, $3, 'LS', TRUE, FALSE, $4, $4)",
    )
    .bind(&rid)
    .bind(uid)
    .bind(live_date())
    .bind(ts)
    .execute(pool)
    .await
    .expect("seed darwin_services");
    for call in calls {
        sqlx::query(
            "INSERT INTO darwin_calls (rid, tiploc, wta, wtd, wtp, ssd, arrival_et, arrival_at, \
                                       departure_et, departure_at, schedule_ts, ts_ts) \
             VALUES ($1, $2, $3, $4, '', $5, $6, $7, $8, $9, $10, $10)",
        )
        .bind(&rid)
        .bind(call.tiploc)
        .bind(call.wta)
        .bind(call.wtd)
        .bind(live_date())
        .bind(call.arrival_et)
        .bind(call.arrival_at)
        .bind(call.departure_et)
        .bind(call.departure_at)
        .bind(ts)
        .execute(pool)
        .await
        .expect("seed darwin_calls");
    }
}

async fn cleanup_darwin(pool: &PgPool, uids: &[&str], stanox_prefix: &str) {
    for sql in [
        "DELETE FROM darwin_calls WHERE rid LIKE 'TDW-%'",
        "DELETE FROM darwin_services WHERE rid LIKE 'TDW-%'",
    ] {
        sqlx::query(sql).execute(pool).await.ok();
    }
    cleanup_live(pool, uids, stanox_prefix).await;
}

/// A `trains` row with no TRUST state at all.
async fn seed_train_row(pool: &PgPool, uid: &str) {
    sqlx::query(
        "INSERT INTO trains (train_uid, service_date) VALUES ($1, $2) \
         ON CONFLICT (train_uid, service_date) DO NOTHING",
    )
    .bind(uid)
    .bind(live_date())
    .execute(pool)
    .await
    .expect("seed trains");
}

/// `first` A -> B (10:00-10:30, or as given), then `second` B -> C at 10:40
/// and `third` B -> C at 11:10: the change at B is five minutes, so `first`
/// arriving at B after 10:35 misses `second`.
async fn seed_change(
    pool: &PgPool,
    stanox_prefix: &str,
    stations: [(&str, &str); 3],
    uids: [&str; 3],
) {
    let tiplocs = stations.map(|(_, tiploc)| tiploc);
    seed_stations(pool, stanox_prefix, &stations).await;
    seed_schedule(
        pool,
        uids[0],
        &[
            (0, tiplocs[0], "origin", None, Some("10:00:00")),
            (1, tiplocs[1], "terminate", Some("10:30:00"), None),
        ],
    )
    .await;
    for (uid, departs, arrives) in [
        (uids[1], "10:40:00", "11:00:00"),
        (uids[2], "11:10:00", "11:30:00"),
    ] {
        seed_schedule(
            pool,
            uid,
            &[
                (0, tiplocs[1], "origin", None, Some(departs)),
                (1, tiplocs[2], "terminate", Some(arrives), None),
            ],
        )
        .await;
    }
}

async fn plan(pool: &PgPool, crs: [&str; 2]) -> Value {
    let (status, body) = get(
        test_router(test_app(pool.clone())),
        format!(
            "/Trips/plan?origin={}&destination={}&date={}&departAfter=09:45",
            crs[0],
            crs[1],
            live_date()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    body
}

fn assert_live_feasible(body: &Value) {
    for itinerary in body["segments"][0]["itineraries"]
        .as_array()
        .expect("itineraries")
    {
        assert_eq!(itinerary["liveFeasible"], true, "{itinerary:?}");
    }
}

/// `TDWL`: ZLA -> ZLB -> ZLC. TDWL1 has no TRUST state (no `trains` row, or
/// a bare one) but Darwin forecasts it into ZLB at 10:45, 15 late: the 10:40
/// on is missed. The timetable (and TRUST, which knows nothing) plan TDWL1 +
/// TDWL2; with Darwin the plan is re-run onto TDWL3.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api \
            routes::trips -- --ignored --test-threads=1`"]
async fn a_late_first_train_trust_has_no_state_for_is_planned_on_darwins_forecast() {
    let pool = connect().await;
    let uids = ["TDWL1", "TDWL2", "TDWL3"];
    for with_row in [false, true] {
        cleanup_darwin(&pool, &uids, "TDWL").await;
        seed_change(
            &pool,
            "TDWL",
            [("ZLA", "TDWLA"), ("ZLB", "TDWLB"), ("ZLC", "TDWLC")],
            uids,
        )
        .await;
        if with_row {
            seed_train_row(&pool, "TDWL1").await;
        }
        let _now = crate::routes::pin_london_now_for_tests(live_now());

        // Nothing known: the timetable's answer.
        let before = plan(&pool, ["ZLA", "ZLC"]).await;
        assert_eq!(train_uids(&before, 0), ["TDWL1", "TDWL2"], "{before:?}");

        seed_darwin(
            &pool,
            "TDWL1",
            &[
                DarwinCall::origin("TDWLA", "10:00"),
                DarwinCall::terminus("TDWLB", "10:30").arriving_forecast("10:45"),
            ],
        )
        .await;
        let after = plan(&pool, ["ZLA", "ZLC"]).await;
        assert_eq!(
            train_uids(&after, 0),
            ["TDWL1", "TDWL3"],
            "with a trains row: {with_row}: {after:?}"
        );
        assert_live_feasible(&after);
        assert!(
            after["live"]["replans"].as_u64().unwrap_or(0) >= 1,
            "{after:?}"
        );
        let first = &after["segments"][0]["itineraries"][0]["legs"][0];
        assert_eq!(first["live"]["arrivalDelayMinutes"], 15, "{first:?}");
    }
    cleanup_darwin(&pool, &uids, "TDWL").await;
}

/// `TDWF`: the same change, TDWF1 fresh in TRUST at 5 late (in 10:35, just
/// in time). Darwin forecasts 15 late; TRUST is the default, so the plan
/// and the delay are TRUST's.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api \
            routes::trips -- --ignored --test-threads=1`"]
async fn a_fresh_trust_delay_is_not_altered_by_a_different_darwin_forecast() {
    let pool = connect().await;
    let uids = ["TDWF1", "TDWF2", "TDWF3"];
    cleanup_darwin(&pool, &uids, "TDWF").await;
    seed_change(
        &pool,
        "TDWF",
        [("ZFA", "TDWFA"), ("ZFB", "TDWFB"), ("ZFC", "TDWFC")],
        uids,
    )
    .await;
    seed_trust_state(&pool, "TDWF1", "en_route", Some(5), live_now()).await;
    seed_darwin(
        &pool,
        "TDWF1",
        &[
            DarwinCall::origin("TDWFA", "10:00"),
            DarwinCall::terminus("TDWFB", "10:30").arriving_forecast("10:45"),
        ],
    )
    .await;
    let _now = crate::routes::pin_london_now_for_tests(live_now());

    let body = plan(&pool, ["ZFA", "ZFC"]).await;
    assert_eq!(train_uids(&body, 0), ["TDWF1", "TDWF2"], "{body:?}");
    assert_live_feasible(&body);
    let first = &body["segments"][0]["itineraries"][0]["legs"][0];
    assert_eq!(first["live"]["arrivalDelayMinutes"], 5, "{first:?}");

    cleanup_darwin(&pool, &uids, "TDWF").await;
}

/// `TDWA`: TDWA1 (A to B, due 10:15) is 2 late in TRUST, which reports
/// nothing at B; Darwin has an actual arrival at B of 10:25. An actual beats
/// TRUST's carried estimate: TDWA2 (10:25) is missed and the plan takes
/// TDWA3.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api \
            routes::trips -- --ignored --test-threads=1`"]
async fn a_darwin_actual_at_a_call_trust_never_reported_beats_the_carried_estimate() {
    let pool = connect().await;
    let uids = ["TDWA1", "TDWA2", "TDWA3"];
    cleanup_darwin(&pool, &uids, "TDWA").await;
    seed_stations(
        &pool,
        "TDWA",
        &[("ZAA", "TDWAA"), ("ZAB", "TDWAB"), ("ZAC", "TDWAC")],
    )
    .await;
    seed_schedule(
        &pool,
        "TDWA1",
        &[
            (0, "TDWAA", "origin", None, Some("10:00:00")),
            (1, "TDWAB", "terminate", Some("10:15:00"), None),
        ],
    )
    .await;
    for (uid, departs, arrives) in [
        ("TDWA2", "10:25:00", "10:45:00"),
        ("TDWA3", "10:45:00", "11:05:00"),
    ] {
        seed_schedule(
            &pool,
            uid,
            &[
                (0, "TDWAB", "origin", None, Some(departs)),
                (1, "TDWAC", "terminate", Some(arrives), None),
            ],
        )
        .await;
    }
    seed_trust_state(&pool, "TDWA1", "en_route", Some(2), live_now()).await;
    let _now = crate::routes::pin_london_now_for_tests(live_now());

    // TRUST alone: 10:17 in, ready 10:22, the 10:25 works.
    let before = plan(&pool, ["ZAA", "ZAC"]).await;
    assert_eq!(train_uids(&before, 0), ["TDWA1", "TDWA2"], "{before:?}");

    seed_darwin(
        &pool,
        "TDWA1",
        &[
            DarwinCall::origin("TDWAA", "10:00"),
            DarwinCall::terminus("TDWAB", "10:15").arriving_actual("10:25"),
        ],
    )
    .await;
    let after = plan(&pool, ["ZAA", "ZAC"]).await;
    assert_eq!(train_uids(&after, 0), ["TDWA1", "TDWA3"], "{after:?}");
    assert_live_feasible(&after);
    let first = &after["segments"][0]["itineraries"][0]["legs"][0];
    assert_eq!(first["live"]["arrivalDelayMinutes"], 10, "{first:?}");

    cleanup_darwin(&pool, &uids, "TDWA").await;
}

/// `TDWS`: TRUST's state for TDWS1 is 45 minutes old, so it is ignored (as
/// it always was); Darwin's forecast of 15 late is used.
#[tokio::test]
#[ignore = "requires a live database; run with `cargo test -p api \
            routes::trips -- --ignored --test-threads=1`"]
async fn a_stale_trust_state_falls_back_to_darwins_forecast() {
    let pool = connect().await;
    let uids = ["TDWS1", "TDWS2", "TDWS3"];
    cleanup_darwin(&pool, &uids, "TDWS").await;
    seed_change(
        &pool,
        "TDWS",
        [("ZSA", "TDWSA"), ("ZSB", "TDWSB"), ("ZSC", "TDWSC")],
        uids,
    )
    .await;
    seed_trust_state(
        &pool,
        "TDWS1",
        "en_route",
        Some(1),
        live_now() - chrono::Duration::minutes(45),
    )
    .await;
    let _now = crate::routes::pin_london_now_for_tests(live_now());

    let before = plan(&pool, ["ZSA", "ZSC"]).await;
    assert_eq!(train_uids(&before, 0), ["TDWS1", "TDWS2"], "{before:?}");

    seed_darwin(
        &pool,
        "TDWS1",
        &[
            DarwinCall::origin("TDWSA", "10:00"),
            DarwinCall::terminus("TDWSB", "10:30").arriving_forecast("10:45"),
        ],
    )
    .await;
    let after = plan(&pool, ["ZSA", "ZSC"]).await;
    assert_eq!(train_uids(&after, 0), ["TDWS1", "TDWS3"], "{after:?}");
    assert_live_feasible(&after);

    cleanup_darwin(&pool, &uids, "TDWS").await;
}
