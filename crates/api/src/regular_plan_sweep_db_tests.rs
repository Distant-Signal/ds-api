//! The regular journeys plan sweep against a live database
//! (`DATABASE_URL`), on synthetic days. Four trains ZPA (RGTSTA) to ZPB
//! (RGTSTB): RG0001 SW 07:50-08:30, RG0002 SW 08:12-08:45 (the usual),
//! RG0003 GW 08:10-08:40 and RG0004 SW 08:40-09:10. The sweep runs the
//! evening before (19:00 London) unless a test says otherwise.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;

use super::*;
use crate::auth::hash_session_token;
use crate::data::regular_journeys::{PlanRecurrence, PlanTemplateInput, TemplateTarget};
use crate::test_support::{FixtureCleanup, assert_synthetic_date};

const USER: &str = "TEST-REGULAR-PLAN-USER";
/// The day most tests plan.
const DATE: &str = "2099-07-07";
/// The STP cancellation test's own day (it changes the timetable).
const STP_DATE: &str = "2099-07-14";
/// A day with no timetable.
const UNPUBLISHED_DATE: &str = "2099-07-08";

fn day(text: &str) -> NaiveDate {
    let date: NaiveDate = text.parse().expect("date");
    assert_synthetic_date(date);
    date
}

/// `h:m` London time on `date` (BST in July).
fn london(date: NaiveDate, h: u32, m: u32) -> DateTime<Utc> {
    chrono_tz::Europe::London
        .from_local_datetime(&date.and_hms_opt(h, m, 0).unwrap())
        .unwrap()
        .with_timezone(&Utc)
}

/// 19:00 London the evening before `date`.
fn evening_before(date: NaiveDate) -> DateTime<Utc> {
    london(date.pred_opt().unwrap(), 19, 0)
}

async fn exec(pool: &PgPool, sql: &str) {
    sqlx::query(sql).execute(pool).await.expect(sql);
}

async fn cleanup(pool: &PgPool) -> FixtureCleanup {
    let mut statements = vec![
        format!("DELETE FROM journeys WHERE user_id = '{USER}'"),
        format!("DELETE FROM journey_templates WHERE user_id = '{USER}'"),
        format!("DELETE FROM train_subscriptions WHERE user_id = '{USER}'"),
        format!("DELETE FROM sessions WHERE user_id = '{USER}'"),
        format!("DELETE FROM users WHERE id = '{USER}'"),
        "DELETE FROM tiploc_crs WHERE tiploc IN ('RGTSTA', 'RGTSTB')".to_string(),
    ];
    for date in [DATE, STP_DATE, UNPUBLISHED_DATE] {
        statements.extend([
            format!("DELETE FROM trains WHERE service_date = '{date}'"),
            format!("DELETE FROM schedule_calling_points_full WHERE service_date = '{date}'"),
            format!("DELETE FROM schedule_destination_departures WHERE service_date = '{date}'"),
            format!("DELETE FROM schedule_services WHERE service_date = '{date}'"),
        ]);
    }
    FixtureCleanup::new(pool, statements).await
}

const TRAINS: [(&str, &str, &str, &str); 4] = [
    ("RG0001", "07:50", "08:30", "SW"),
    ("RG0002", "08:12", "08:45", "SW"),
    ("RG0003", "08:10", "08:40", "GW"),
    ("RG0004", "08:40", "09:10", "SW"),
];

async fn seed_timetable(pool: &PgPool, date: &str) {
    exec(
        pool,
        "INSERT INTO tiploc_crs (tiploc, crs, station_name, stanox, source_sequence) VALUES \
         ('RGTSTA', 'ZPA', 'Regular A', 'RGA', 1), ('RGTSTB', 'ZPB', 'Regular B', 'RGB', 1) \
         ON CONFLICT DO NOTHING",
    )
    .await;
    for (uid, dep, arr, operator) in TRAINS {
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_calling_points_full \
                     (service_date, uid, seq, tiploc, kind, booked_departure, public_departure, \
                      booked_arrival, public_arrival, day_offset, can_board, can_alight) VALUES \
                 ('{date}', '{uid}', 1, 'RGTSTA', 'origin', '{dep}', '{dep}', NULL, NULL, 0, TRUE, TRUE), \
                 ('{date}', '{uid}', 2, 'RGTSTB', 'terminate', NULL, NULL, '{arr}', '{arr}', 0, TRUE, TRUE)"
            ),
        )
        .await;
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_destination_departures \
                     (service_date, destination_crs, scheduled, train_uid, origin_crs, \
                      destination_arrival, operator_atoc, public_departure, \
                      public_destination_arrival) \
                 VALUES ('{date}', 'ZPB', '{dep}', '{uid}', 'ZPA', '{arr}', '{operator}', \
                         '{dep}', '{arr}')"
            ),
        )
        .await;
        exec(
            pool,
            &format!(
                "INSERT INTO schedule_services (service_date, uid, mode, stp, operator_atoc) \
                 VALUES ('{date}', '{uid}', 'train', 'P', '{operator}')"
            ),
        )
        .await;
    }
}

async fn seed_user(pool: &PgPool) -> String {
    exec(
        pool,
        &format!("INSERT INTO users (id, name) VALUES ('{USER}', '{USER}') ON CONFLICT DO NOTHING"),
    )
    .await;
    let raw = format!("test-raw-session-token-for-{USER}");
    sqlx::query(
        "INSERT INTO sessions (id, user_id, refresh_token, created_at, expires_at) \
         VALUES ($1, $2, NULL, NOW(), NOW() + make_interval(days => 14)) \
         ON CONFLICT (id) DO UPDATE SET user_id = EXCLUDED.user_id, \
             expires_at = EXCLUDED.expires_at",
    )
    .bind(hash_session_token(&raw))
    .bind(USER)
    .execute(pool)
    .await
    .expect("seed session");
    raw
}

/// A regular journey ZPA to ZPB in `mode` with target `kind` at `time`,
/// whose usual train is `usual_uid` leaving at `usual_departs` run by
/// `usual_operator`, and its occurrence on `date` (unmatched, as the
/// notifier mints it). Returns the occurrence's journey id.
async fn occurrence(
    pool: &PgPool,
    mode: &str,
    kind: &str,
    time: &str,
    usual: (&str, &str, &str),
    date: NaiveDate,
) -> i64 {
    let (usual_uid, usual_departs, usual_operator) = usual;
    let usual_itinerary = serde_json::from_value(serde_json::json!({
        "legs": [{
            "kind": "train", "trainUid": usual_uid, "atocCode": usual_operator,
            "originCrs": "ZPA", "destinationCrs": "ZPB", "departs": usual_departs
        }]
    }))
    .expect("usual itinerary");
    let outcome = regular_journeys::create_plan_templates(
        pool,
        USER,
        &PlanRecurrence {
            days_of_week: 127,
            starts_on: None,
            ends_on: None,
            default_match_mode: mode.to_string(),
            skip_bank_holidays: true,
        },
        &PlanTemplateInput {
            custom_name: None,
            origin_crs: "ZPA".to_string(),
            destination_crs: "ZPB".to_string(),
            target: TemplateTarget {
                kind: kind.to_string(),
                time: time.parse().expect("time"),
                day_offset: 0,
            },
            plan_options: None,
            usual_itinerary,
            station_buffer_minutes: None,
        },
        None,
    )
    .await
    .expect("create template");
    let regular_journeys::CreatePlanOutcome::Created { id, .. } = outcome else {
        panic!("over the cap");
    };
    crate::data::journey_templates::materialize_template(pool, id, USER, date)
        .await
        .expect("mint")
        .expect("template exists")
        .journey_id
}

fn config() -> SweepConfig {
    SweepConfig {
        enabled: true,
        ..SweepConfig::default()
    }
}

/// `(status, plan)` of an occurrence.
async fn plan_of(pool: &PgPool, journey_id: i64) -> (Option<String>, Value) {
    let (status, plan): (Option<String>, Option<Value>) =
        sqlx::query_as("SELECT regular_plan_status, regular_plan FROM journeys WHERE id = $1")
            .bind(journey_id)
            .fetch_one(pool)
            .await
            .expect("read plan");
    (status, plan.unwrap_or(Value::Null))
}

/// Each leg's `(train UID, match_mode)`, in order.
async fn legs_of(pool: &PgPool, journey_id: i64) -> Vec<(Option<String>, String)> {
    sqlx::query_as(
        "SELECT tr.train_uid, jl.match_mode FROM journey_legs jl \
         LEFT JOIN train_subscriptions ts ON ts.id = jl.train_subscription_id \
         LEFT JOIN trains tr ON tr.id = ts.trains_id \
         WHERE jl.journey_id = $1 ORDER BY jl.leg_order",
    )
    .bind(journey_id)
    .fetch_all(pool)
    .await
    .expect("read legs")
}

fn chosen_uid(plan: &Value) -> &str {
    plan["choice"]["legs"][0]["trainUid"]
        .as_str()
        .unwrap_or_default()
}

fn app(pool: &PgPool) -> App {
    crate::test_support::inert_app(pool.clone())
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn remind_mode_suggests_the_usual_train_and_commits_nothing() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, DATE).await;
    seed_user(&pool).await;
    let date = day(DATE);
    let journey = occurrence(
        &pool,
        "manual",
        "depart_after",
        "07:45",
        ("RG0002", "08:12", "SW"),
        date,
    )
    .await;

    let swept = run_sweep(&app(&pool), &config(), evening_before(date))
        .await
        .expect("sweep");
    assert_eq!((swept.occurrences, swept.suggested), (1, 1), "{swept:?}");
    let (status, plan) = plan_of(&pool, journey).await;
    assert_eq!(status.as_deref(), Some("suggested"));
    assert_eq!(plan["reason"], "usual", "{plan}");
    assert_eq!(chosen_uid(&plan), "RG0002", "{plan}");
    // The alternatives by earliest arrival: RG0001, RG0003.
    let alternatives: Vec<&str> = plan["alternatives"]
        .as_array()
        .expect("alternatives")
        .iter()
        .map(|option| option["legs"][0]["trainUid"].as_str().unwrap())
        .collect();
    assert_eq!(alternatives, ["RG0001", "RG0003"]);
    // Nothing committed: the leg is still unmatched.
    assert_eq!(
        legs_of(&pool, journey).await,
        [(None, "unmatched".to_string())]
    );

    // Planned against this publish: not due again the same evening.
    let swept = run_sweep(
        &app(&pool),
        &config(),
        evening_before(date) + Duration::minutes(5),
    )
    .await
    .expect("sweep");
    assert_eq!(swept.occurrences, 0, "{swept:?}");
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_new_uid_falls_back_to_the_same_operator_and_arrive_by_takes_the_latest() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, DATE).await;
    seed_user(&pool).await;
    let date = day(DATE);
    // The usual SW train had another UID and left at 08:14.
    let renamed = occurrence(
        &pool,
        "manual",
        "depart_after",
        "07:45",
        ("RG9999", "08:14", "SW"),
        date,
    )
    .await;
    // Arrive by 08:46, with no usual train that day: the latest departure
    // that makes it, RG0002 (08:12), not RG0003 (08:10).
    let arrive_by = occurrence(
        &pool,
        "manual",
        "arrive_by",
        "08:46",
        ("RG8888", "06:00", "XR"),
        date,
    )
    .await;

    let swept = run_sweep(&app(&pool), &config(), evening_before(date))
        .await
        .expect("sweep");
    assert_eq!(swept.suggested, 2, "{swept:?}");
    let (_, plan) = plan_of(&pool, renamed).await;
    assert_eq!(plan["reason"], "same_operator", "{plan}");
    assert_eq!(chosen_uid(&plan), "RG0002", "{plan}");
    let (_, plan) = plan_of(&pool, arrive_by).await;
    assert_eq!(plan["reason"], "best", "{plan}");
    assert_eq!(chosen_uid(&plan), "RG0002", "{plan}");
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn nothing_is_decided_on_an_unpublished_day() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_user(&pool).await;
    let date = day(UNPUBLISHED_DATE);
    let journey = occurrence(
        &pool,
        "auto",
        "depart_after",
        "07:45",
        ("RG0002", "08:12", "SW"),
        date,
    )
    .await;

    let swept = run_sweep(&app(&pool), &config(), evening_before(date))
        .await
        .expect("sweep");
    assert_eq!((swept.pending, swept.no_service), (1, 0), "{swept:?}");
    let (status, plan) = plan_of(&pool, journey).await;
    assert_eq!(status.as_deref(), Some("pending"));
    assert!(plan.is_null(), "{plan}");
    assert_eq!(
        legs_of(&pool, journey).await,
        [(None, "unmatched".to_string())]
    );
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn auto_mode_commits_and_the_morning_check_replans_an_stp_cancellation() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, STP_DATE).await;
    seed_user(&pool).await;
    let owner = ds_store::test_support::schedule_fixture_pool(&pool).await;
    let _publishes = FixtureCleanup::new(
        &owner,
        [
            "DELETE FROM schedule_reference_publishes WHERE delivery LIKE 'TEST-REGULAR-PLAN-%'"
                .to_string(),
        ],
    )
    .await;
    let date = day(STP_DATE);
    let journey = occurrence(
        &pool,
        "auto",
        "depart_after",
        "07:45",
        ("RG0002", "08:12", "SW"),
        date,
    )
    .await;

    let swept = run_sweep(&app(&pool), &config(), evening_before(date))
        .await
        .expect("sweep");
    assert_eq!(swept.committed, 1, "{swept:?}");
    let legs = legs_of(&pool, journey).await;
    assert_eq!(legs, [(Some("RG0002".to_string()), "auto".to_string())]);
    let (status, plan) = plan_of(&pool, journey).await;
    assert_eq!(status.as_deref(), Some("committed"));
    let leg_id: i64 = sqlx::query_scalar("SELECT id FROM journey_legs WHERE journey_id = $1")
        .bind(journey)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(plan["sweepLegIds"], serde_json::json!([leg_id]), "{plan}");
    // Revision 1, the booked plan, was refreshed with the train.
    let (revisions, booked): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM journey_revisions WHERE journey_id = $1), \
                (SELECT rl.booked_departure FROM journey_revisions r \
                 JOIN journey_revision_legs rl ON rl.revision_id = r.id \
                 WHERE r.journey_id = $1 AND r.revision_no = 1)",
    )
    .bind(journey)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((revisions, booked), (1, Some(london(date, 8, 12))));

    // The morning check with nothing changed keeps the legs.
    let swept = run_sweep(&app(&pool), &config(), london(date, 5, 10))
        .await
        .expect("sweep");
    assert_eq!((swept.occurrences, swept.kept), (1, 1), "{swept:?}");

    // An STP cancellation of RG0002 arrives with a new publish; the next
    // morning check (here, the first after the publish) replans: no SW
    // train within 5 minutes, so the earliest arrival, RG0001.
    exec(
        &owner,
        &format!(
            "DELETE FROM schedule_calling_points_full WHERE service_date = '{STP_DATE}' \
             AND uid = 'RG0002'"
        ),
    )
    .await;
    exec(
        &owner,
        &format!(
            "DELETE FROM schedule_destination_departures WHERE service_date = '{STP_DATE}' \
             AND train_uid = 'RG0002'"
        ),
    )
    .await;
    exec(
        &owner,
        "INSERT INTO schedule_reference_publishes (delivery, completed_at) \
         SELECT 'TEST-REGULAR-PLAN-1', COALESCE(max(completed_at), now()) + interval '1 second' \
         FROM schedule_reference_publishes",
    )
    .await;
    let swept = run_sweep(&app(&pool), &config(), london(date, 5, 20))
        .await
        .expect("sweep");
    assert_eq!(swept.committed, 1, "{swept:?}");
    assert_eq!(
        legs_of(&pool, journey).await,
        [(Some("RG0001".to_string()), "auto".to_string())]
    );
    let (_, plan) = plan_of(&pool, journey).await;
    assert_eq!(plan["reason"], "best", "{plan}");
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn a_journey_with_a_break_or_a_started_one_is_left_alone() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, DATE).await;
    seed_user(&pool).await;
    let date = day(DATE);
    let broken = occurrence(
        &pool,
        "auto",
        "depart_after",
        "07:45",
        ("RG0002", "08:12", "SW"),
        date,
    )
    .await;
    sqlx::query(
        "INSERT INTO journey_breaks (journey_id, user_id, cause, fault, severity, \
             break_position, fingerprint, status) \
         VALUES ($1, $2, 'cancelled', 'operator', 3, 1, 'f', 'dismissed')",
    )
    .bind(broken)
    .bind(USER)
    .execute(&pool)
    .await
    .expect("seed break");

    let swept = run_sweep(&app(&pool), &config(), evening_before(date))
        .await
        .expect("sweep");
    assert_eq!(swept.occurrences, 0, "{swept:?}");
    assert_eq!(plan_of(&pool, broken).await.0, None);
    assert_eq!(
        legs_of(&pool, broken).await,
        [(None, "unmatched".to_string())]
    );

    // Once its booked first departure has passed, a journey has started.
    exec(
        &pool,
        &format!("DELETE FROM journey_breaks WHERE journey_id = {broken}"),
    )
    .await;
    exec(
        &pool,
        &format!(
            "UPDATE journey_revision_legs SET booked_departure = '{}' \
             WHERE revision_id IN (SELECT id FROM journey_revisions WHERE journey_id = {broken})",
            london(date, 7, 0).to_rfc3339()
        ),
    )
    .await;
    let swept = run_sweep(&app(&pool), &config(), london(date, 7, 30))
        .await
        .expect("sweep");
    assert_eq!(swept.occurrences, 0, "{swept:?}");
}

fn router(pool: PgPool) -> axum::Router {
    crate::app::Router::new()
        .merge(crate::routes::journeys::router())
        .with_state(crate::test_support::inert_app(pool))
}

async fn call(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::COOKIE, format!("distant_signal_session={token}"));
    let request = match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => builder.body(Body::empty()),
    }
    .expect("request");
    let response = router.clone().oneshot(request).await.expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()));
    (status, value)
}

#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn accepting_a_suggestion_saves_the_users_pick_which_the_sweep_never_replaces() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, DATE).await;
    let token = seed_user(&pool).await;
    let date = day(DATE);
    let evening = evening_before(date);
    let _now = crate::routes::pin_london_now_for_tests(evening);
    let journey = occurrence(
        &pool,
        "manual",
        "depart_after",
        "07:45",
        ("RG0002", "08:12", "SW"),
        date,
    )
    .await;
    run_sweep(&app(&pool), &config(), evening)
        .await
        .expect("sweep");
    let router = router(pool.clone());

    // The owner sees the plan on the journey page.
    let (status, detail) = call(
        &router,
        "GET",
        &format!("/Journeys/{journey}"),
        &token,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let plan = &detail["regularPlan"];
    assert_eq!(plan["status"], "suggested", "{detail}");
    assert_eq!(plan["reason"], "usual", "{detail}");
    assert_eq!(plan["choice"]["legs"][0]["trainUid"], "RG0002", "{detail}");
    let planned_at = plan["plannedAt"].clone();

    // A stale read is refused.
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey}/regular-plan/accept"),
        &token,
        Some(serde_json::json!({"index": 1, "plannedAt": "2099-01-01T00:00:00Z"})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Picking the first alternative, RG0001.
    let (status, body) = call(
        &router,
        "POST",
        &format!("/Journeys/{journey}/regular-plan/accept"),
        &token,
        Some(serde_json::json!({"index": 1, "plannedAt": planned_at})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revisionNo"], 1, "{body}");
    assert_eq!(
        legs_of(&pool, journey).await,
        [(Some("RG0001".to_string()), "manual".to_string())]
    );
    let (status, plan) = plan_of(&pool, journey).await;
    assert_eq!(status.as_deref(), Some("committed"));
    assert_eq!(plan["acceptedIndex"], 1, "{plan}");

    // The morning check never replaces the owner's pick.
    let swept = run_sweep(&app(&pool), &config(), london(date, 5, 30))
        .await
        .expect("sweep");
    assert_eq!(swept.occurrences, 0, "{swept:?}");
    assert_eq!(
        legs_of(&pool, journey).await,
        [(Some("RG0001".to_string()), "manual".to_string())]
    );
}

/// The sweep's planning slot is its own: with the break sweep's slot and
/// every user's held, it still plans.
#[tokio::test]
#[ignore = "requires a live database: DATABASE_URL"]
async fn the_sweeps_slot_is_not_the_break_sweeps_or_a_users() {
    let pool = connect().await;
    let _cleanup = cleanup(&pool).await;
    seed_timetable(&pool, DATE).await;
    let _break_slot = crate::routes::trips::hold_break_sweep_slot().await;
    let _user_slots = crate::routes::trips::hold_request_slots().await;
    let query = crate::routes::trips::ReplanQuery::depart_after(
        "ZPA".to_string(),
        "ZPB".to_string(),
        day(DATE),
        7 * 60 + 45,
        "options",
        2,
        None,
    );
    let plan = tokio::time::timeout(
        StdDuration::from_secs(60),
        crate::routes::trips::plan_for_journey(&app(&pool), &query, ReplanSlot::Regular),
    )
    .await
    .expect("the regular slot is free")
    .expect("plan")
    .expect("a timetable for the day");
    assert!(!plan.is_empty());
    // The break sweep's slot really is held.
    assert!(
        tokio::time::timeout(
            StdDuration::from_millis(200),
            crate::routes::trips::plan_for_journey(&app(&pool), &query, ReplanSlot::Sweep),
        )
        .await
        .is_err()
    );
}

async fn connect() -> PgPool {
    ds_store::test_support::connect().await
}
