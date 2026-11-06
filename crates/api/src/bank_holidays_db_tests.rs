//! Bank holidays (regular journeys design §8) against a live database
//! (`DATABASE_URL`): the sync against a stand-in GOV.UK (no test reaches
//! GOV.UK), a moved holiday, `nextDate` and `nextSkips` per division, and
//! the routes (`skipBankHolidays`, `nextSkips`, the division preference).
//!
//! `bank_holidays` is one table for the whole service, so the sync and
//! apply tests write only dates in 2090 to 2092 (each test its own year),
//! and the route test only Northern Ireland's rows titled `TEST-BH`, for a
//! user whose division is Northern Ireland: no other test's user (England
//! and Wales by default) can see them.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::auth::hash_session_token;
use crate::bank_holidays_sync::{client, sync_once};
use crate::data::bank_holidays::{self, BankHoliday, Parsed, SyncResult};
use crate::data::regular_journeys;
use crate::test_support::FixtureCleanup;

async fn connect() -> PgPool {
    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set to run this test");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to postgres")
}

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

/// A GOV.UK-shaped body: each division with `events`, as `(date, title)`.
fn body(divisions: &[(&str, &[(&str, &str)])]) -> String {
    let mut value = serde_json::Map::new();
    for (division, events) in divisions {
        let events: Vec<Value> = events
            .iter()
            .map(
                |(date, title)| json!({"title": title, "date": date, "notes": "", "bunting": true}),
            )
            .collect();
        value.insert(
            (*division).to_string(),
            json!({"division": division, "events": events}),
        );
    }
    Value::Object(value).to_string()
}

const YEAR_2090: &[(&str, &str)] = &[
    ("2090-01-02", "New Year's Day"),
    ("2090-04-14", "Good Friday"),
    ("2090-05-01", "Early May bank holiday"),
    ("2090-05-29", "Spring bank holiday"),
    ("2090-12-25", "Christmas Day"),
    ("2090-12-26", "Boxing Day"),
];

async fn rows(pool: &PgPool, from: NaiveDate, to: NaiveDate) -> Vec<(String, NaiveDate, String)> {
    sqlx::query_as(
        "SELECT division, holiday_date, title FROM bank_holidays \
         WHERE holiday_date BETWEEN $1 AND $2 ORDER BY 1, 2",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database)"]
async fn a_good_response_is_applied_and_a_bad_one_changes_nothing() {
    // The sync's one row is overwritten by each attempt below (the api
    // role can't delete it), so no cleanup is needed for it.
    let pool = connect().await;
    let _cleanup = FixtureCleanup::new(
        &pool,
        ["DELETE FROM bank_holidays WHERE holiday_date BETWEEN '2090-01-01' AND '2090-12-31'"],
    )
    .await;
    let server = MockServer::start().await;
    let url = format!("{}/bank-holidays.json", server.uri());
    let http = client().unwrap();
    let year = (date(2090, 1, 1), date(2090, 12, 31));
    let now = Utc.with_ymd_and_hms(2090, 1, 1, 3, 0, 0).unwrap();

    let good = body(&[
        ("england-and-wales", YEAR_2090),
        ("scotland", YEAR_2090),
        ("northern-ireland", YEAR_2090),
    ]);
    let mock = Mock::given(method("GET"))
        .and(path("/bank-holidays.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(good))
        .expect(1)
        .mount_as_scoped(&server)
        .await;
    assert_eq!(
        sync_once(&pool, &http, &url, now).await.unwrap(),
        SyncResult::Ok
    );
    drop(mock);
    let stored = rows(&pool, year.0, year.1).await;
    assert_eq!(stored.len(), 18);
    assert!(stored.contains(&(
        "scotland".to_string(),
        date(2090, 12, 25),
        "Christmas Day".to_string()
    )));
    let state = bank_holidays::sync_state(&pool).await.unwrap().unwrap();
    assert_eq!(state.last_result, "ok");
    assert_eq!(state.last_success_at, Some(now));

    // Scotland missing: nothing changes, even England and Wales's rows
    // the response still lists differently.
    let partial = body(&[
        ("england-and-wales", &YEAR_2090[..5]),
        ("northern-ireland", YEAR_2090),
    ]);
    let later = now + Duration::hours(25);
    let mock = Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(partial))
        .mount_as_scoped(&server)
        .await;
    assert_eq!(
        sync_once(&pool, &http, &url, later).await.unwrap(),
        SyncResult::Invalid
    );
    drop(mock);
    assert_eq!(rows(&pool, year.0, year.1).await, stored);

    // A division with too few events, an HTML error page, a 503: the same.
    let few = body(&[
        ("england-and-wales", &YEAR_2090[..2]),
        ("scotland", YEAR_2090),
        ("northern-ireland", YEAR_2090),
    ]);
    for (status, response, expected) in [
        (200, few, SyncResult::Invalid),
        (
            200,
            "<html>Service unavailable</html>".to_string(),
            SyncResult::Invalid,
        ),
        (503, String::new(), SyncResult::FetchFailed),
    ] {
        let mock = Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(status).set_body_string(response))
            .mount_as_scoped(&server)
            .await;
        assert_eq!(
            sync_once(&pool, &http, &url, later).await.unwrap(),
            expected
        );
        drop(mock);
        assert_eq!(rows(&pool, year.0, year.1).await, stored);
    }
    let state = bank_holidays::sync_state(&pool).await.unwrap().unwrap();
    assert_eq!(state.last_result, "fetch_failed");
    assert_eq!(state.last_attempt_at, later);
    assert_eq!(
        state.last_success_at,
        Some(now),
        "a failure keeps the last success"
    );
}

fn parsed(events: &[(NaiveDate, &str)]) -> Parsed {
    let holidays: Vec<BankHoliday> = events
        .iter()
        .map(|(date, title)| BankHoliday {
            date: *date,
            title: (*title).to_string(),
        })
        .collect();
    Parsed {
        divisions: ds_store::bank_holidays::DIVISIONS
            .into_iter()
            .map(|division| (division, holidays.clone()))
            .collect(),
    }
}

#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database)"]
async fn a_moved_holiday_is_replaced() {
    let pool = connect().await;
    let _cleanup = FixtureCleanup::new(
        &pool,
        ["DELETE FROM bank_holidays WHERE holiday_date BETWEEN '2091-01-01' AND '2091-12-31'"],
    )
    .await;
    let year = (date(2091, 1, 1), date(2091, 12, 31));
    let now = Utc.with_ymd_and_hms(2091, 1, 1, 3, 0, 0).unwrap();
    let first = [
        (date(2091, 1, 1), "New Year's Day"),
        (date(2091, 3, 30), "Good Friday"),
        (date(2091, 5, 7), "Early May bank holiday"),
        (date(2091, 5, 28), "Spring bank holiday"),
        (date(2091, 12, 25), "Christmas Day"),
    ];
    bank_holidays::apply(&pool, &parsed(&first), now)
        .await
        .unwrap();
    // The Early May bank holiday moves to the Friday; GOV.UK no longer
    // lists New Year's Day 2091 (before the new range): it stays.
    let second = [
        (date(2091, 3, 30), "Good Friday"),
        (date(2091, 5, 4), "Early May bank holiday (moved)"),
        (date(2091, 5, 28), "Spring bank holiday"),
        (date(2091, 8, 27), "Summer bank holiday"),
        (date(2091, 12, 25), "Christmas Day"),
    ];
    let applied = bank_holidays::apply(&pool, &parsed(&second), now + Duration::days(1))
        .await
        .unwrap();
    assert_eq!(applied.deleted, 3, "7 May in each division");
    assert_eq!(applied.upserted, 15);
    let england: Vec<(NaiveDate, String)> = rows(&pool, year.0, year.1)
        .await
        .into_iter()
        .filter(|(division, _, _)| division == "england-and-wales")
        .map(|(_, date, title)| (date, title))
        .collect();
    assert_eq!(
        england.iter().map(|(date, _)| *date).collect::<Vec<_>>(),
        [
            date(2091, 1, 1),
            date(2091, 3, 30),
            date(2091, 5, 4),
            date(2091, 5, 28),
            date(2091, 8, 27),
            date(2091, 12, 25),
        ]
    );
    assert_eq!(england[2].1, "Early May bank holiday (moved)");
}

async fn seed_user(pool: &PgPool, user_id: &str, division: Option<&str>) {
    sqlx::query(
        "INSERT INTO users (id, name, bank_holiday_division) VALUES ($1, $1, $2) \
         ON CONFLICT (id) DO UPDATE SET bank_holiday_division = EXCLUDED.bank_holiday_division",
    )
    .bind(user_id)
    .bind(division)
    .execute(pool)
    .await
    .expect("seed user");
}

/// A weekday template (Monday to Friday) owned by `user_id`.
async fn weekday_template(pool: &PgPool, user_id: &str, skip: bool) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO journey_templates (user_id, days_of_week, skip_bank_holidays) \
         VALUES ($1, 31, $2) RETURNING id",
    )
    .bind(user_id)
    .bind(skip)
    .fetch_one(pool)
    .await
    .expect("seed template")
}

async fn holiday(pool: &PgPool, division: &str, on: NaiveDate, title: &str) {
    sqlx::query(
        "INSERT INTO bank_holidays (division, holiday_date, title, fetched_at) \
         VALUES ($1, $2, $3, now()) ON CONFLICT DO NOTHING",
    )
    .bind(division)
    .bind(on)
    .bind(title)
    .execute(pool)
    .await
    .expect("seed holiday");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL (a migrated database)"]
async fn next_date_skips_the_owners_bank_holidays() {
    const ENGLAND: &str = "TEST-BH-NEXT-ENGLAND";
    const SCOTLAND: &str = "TEST-BH-NEXT-SCOTLAND";
    let pool = connect().await;
    let _cleanup = FixtureCleanup::new(
        &pool,
        [
            "DELETE FROM bank_holidays WHERE holiday_date BETWEEN '2092-01-01' AND '2092-12-31'"
                .to_string(),
            format!("DELETE FROM journey_templates WHERE user_id IN ('{ENGLAND}', '{SCOTLAND}')"),
            format!("DELETE FROM users WHERE id IN ('{ENGLAND}', '{SCOTLAND}')"),
        ],
    )
    .await;
    // Thursday 27 March 2092, before Good Friday (28th) and Easter Monday
    // (31st): Good Friday everywhere, Easter Monday not in Scotland.
    let now: DateTime<Utc> = Utc.with_ymd_and_hms(2092, 3, 27, 12, 0, 0).unwrap();
    let thursday = date(2092, 3, 27);
    let good_friday = date(2092, 3, 28);
    let easter_monday = date(2092, 3, 31);
    let tuesday = date(2092, 4, 1);
    seed_user(&pool, ENGLAND, None).await;
    seed_user(&pool, SCOTLAND, Some("scotland")).await;
    let england = weekday_template(&pool, ENGLAND, true).await;
    let england_off = weekday_template(&pool, ENGLAND, false).await;
    let scotland = weekday_template(&pool, SCOTLAND, true).await;

    // No holidays known: nothing skipped.
    let next = |dates: &std::collections::HashMap<
        i64,
        (Option<NaiveDate>, Vec<regular_journeys::NextSkip>),
    >,
                id: i64| dates[&id].clone();
    let dates = regular_journeys::next_dates_and_skips(&pool, &[england], now)
        .await
        .unwrap();
    assert_eq!(next(&dates, england), (Some(thursday), vec![]));

    for division in ds_store::bank_holidays::DIVISIONS {
        holiday(&pool, division, thursday, "TEST-BH Thursday").await;
        holiday(&pool, division, good_friday, "Good Friday").await;
    }
    holiday(&pool, "england-and-wales", easter_monday, "Easter Monday").await;
    holiday(&pool, "northern-ireland", easter_monday, "Easter Monday").await;
    // Easter Monday is skipped by hand too: "skipped" wins.
    sqlx::query(
        "INSERT INTO journey_template_skipped_dates (template_id, service_date) VALUES ($1, $2)",
    )
    .bind(england)
    .bind(easter_monday)
    .execute(&pool)
    .await
    .unwrap();

    let dates =
        regular_journeys::next_dates_and_skips(&pool, &[england, england_off, scotland], now)
            .await
            .unwrap();
    let skip = |date, reason| regular_journeys::NextSkip { date, reason };
    assert_eq!(
        next(&dates, england),
        (
            Some(tuesday),
            vec![
                skip(thursday, "bank_holiday"),
                skip(good_friday, "bank_holiday"),
                skip(easter_monday, "skipped"),
            ]
        )
    );
    assert_eq!(next(&dates, england_off), (Some(thursday), vec![]));
    assert_eq!(
        next(&dates, scotland),
        (
            Some(easter_monday),
            vec![
                skip(thursday, "bank_holiday"),
                skip(good_friday, "bank_holiday")
            ]
        )
    );

    // The list says the same.
    let list = crate::data::journey_templates::list_templates_for_user(&pool, ENGLAND, now)
        .await
        .unwrap();
    let item = list.iter().find(|item| item.id == england).unwrap();
    assert_eq!(item.next_date, Some(tuesday));
    assert!(item.skip_bank_holidays);
    assert_eq!(item.next_skips.len(), 3);
    let off = list.iter().find(|item| item.id == england_off).unwrap();
    assert_eq!(off.next_date, Some(thursday));
    assert!(!off.skip_bank_holidays);
}

async fn seed_session(pool: &PgPool, user_id: &str) -> String {
    let raw = format!("test-raw-session-token-for-{user_id}");
    sqlx::query(
        "INSERT INTO sessions (id, user_id, refresh_token, created_at, expires_at) \
         VALUES ($1, $2, NULL, NOW(), NOW() + make_interval(days => 14)) \
         ON CONFLICT (id) DO UPDATE SET user_id = EXCLUDED.user_id, \
             expires_at = EXCLUDED.expires_at",
    )
    .bind(hash_session_token(&raw))
    .bind(user_id)
    .execute(pool)
    .await
    .expect("seed session");
    raw
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
#[ignore = "requires DATABASE_URL (a migrated database)"]
#[expect(clippy::too_many_lines, reason = "one scenario across the routes")]
async fn the_routes_serve_skips_and_set_the_division() {
    const USER: &str = "TEST-BH-ROUTES";
    let pool = connect().await;
    let _cleanup = FixtureCleanup::new(
        &pool,
        [
            "DELETE FROM bank_holidays WHERE division = 'northern-ireland' AND title = 'TEST-BH'"
                .to_string(),
            format!("DELETE FROM journey_templates WHERE user_id = '{USER}'"),
            format!("DELETE FROM sessions WHERE user_id = '{USER}'"),
            format!("DELETE FROM users WHERE id = '{USER}'"),
        ],
    )
    .await;
    seed_user(&pool, USER, None).await;
    let token = seed_session(&pool, USER).await;
    let router = crate::app::Router::new()
        .merge(crate::routes::journey_templates::router())
        .merge(crate::routes::preferences::router())
        .with_state(crate::test_support::inert_app(pool.clone()));

    // The division: unset, then Northern Ireland (only this user's rows
    // are Northern Ireland's TEST-BH ones), a bad value refused.
    let (status, prefs) = call(&router, "GET", "/preferences", &token, None).await;
    assert_eq!(status, StatusCode::OK, "{prefs}");
    assert_eq!(prefs["bankHolidayDivision"], Value::Null);
    let uri = "/preferences/bank-holiday-division";
    for bad in [
        json!({"division": "wales"}),
        json!({"division": "scotland", "x": 1}),
    ] {
        let (status, body) = call(&router, "PUT", uri, &token, Some(bad)).await;
        assert!(status.is_client_error(), "{status} {body}");
    }
    let (status, _) = call(
        &router,
        "PUT",
        uri,
        &token,
        Some(json!({"division": "northern-ireland"})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, prefs) = call(&router, "GET", "/preferences", &token, None).await;
    assert_eq!(prefs["bankHolidayDivision"], "northern-ireland");

    // Every day, today and tomorrow bank holidays, tomorrow also skipped.
    let template: i64 = sqlx::query_scalar(
        "INSERT INTO journey_templates (user_id, days_of_week) VALUES ($1, 127) RETURNING id",
    )
    .bind(USER)
    .fetch_one(&pool)
    .await
    .unwrap();
    let today = regular_journeys::london_date(Utc::now());
    let tomorrow = today.succ_opt().unwrap();
    let after = tomorrow.succ_opt().unwrap();
    holiday(&pool, "northern-ireland", today, "TEST-BH").await;
    holiday(&pool, "northern-ireland", tomorrow, "TEST-BH").await;
    sqlx::query(
        "INSERT INTO journey_template_skipped_dates (template_id, service_date) VALUES ($1, $2)",
    )
    .bind(template)
    .bind(tomorrow)
    .execute(&pool)
    .await
    .unwrap();
    let expected = json!([
        {"date": today.to_string(), "reason": "bank_holiday"},
        {"date": tomorrow.to_string(), "reason": "skipped"},
    ]);
    let (status, mine) = call(&router, "GET", "/JourneyTemplates/mine", &token, None).await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    let row = &mine.as_array().unwrap()[0];
    assert_eq!(row["skipBankHolidays"], true);
    assert_eq!(row["nextDate"], after.to_string());
    assert_eq!(row["nextSkips"], expected);
    let detail_uri = format!("/JourneyTemplates/{template}");
    let (status, detail) = call(&router, "GET", &detail_uri, &token, None).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["skipBankHolidays"], true);
    assert_eq!(detail["nextDate"], after.to_string());
    assert_eq!(detail["nextSkips"], expected);

    // The switch off through PUT; a PUT without it keeps it.
    let put = |skip: Option<bool>| {
        let mut body = json!({
            "legs": [{"originCrs": "QBA", "destinationCrs": "QBB"}],
            "daysOfWeek": 127,
            "active": true,
            "startsOn": null,
            "endsOn": null,
            "defaultMatchMode": "manual",
            "autoCommitRule": null,
        });
        if let Some(skip) = skip {
            body["skipBankHolidays"] = json!(skip);
        }
        body
    };
    let (status, body) = call(&router, "PUT", &detail_uri, &token, Some(put(Some(false)))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = call(&router, "PUT", &detail_uri, &token, Some(put(None))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (_, detail) = call(&router, "GET", &detail_uri, &token, None).await;
    assert_eq!(detail["skipBankHolidays"], false);
    assert_eq!(detail["nextDate"], today.to_string());
    assert_eq!(detail["nextSkips"], json!([]));

    // Back to the default division: England and Wales has no TEST-BH rows.
    let (status, _) = call(&router, "PUT", uri, &token, Some(json!({"division": null}))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, prefs) = call(&router, "GET", "/preferences", &token, None).await;
    assert_eq!(prefs["bankHolidayDivision"], Value::Null);
}
