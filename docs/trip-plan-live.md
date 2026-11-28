# Live times in `/Trips/plan`

`GET /Trips/plan` plans on the timetable, then lays live facts over the plan
(`crates/api/src/data/trip_plan_live.rs`): a late train's later calls, a
cancelled one, and whether each change still works. This page says where the
live delay comes from. The design records are in the distant-signal
repository's `docs/superpowers/specs`.

## Sources

- **In-search snapshot** (`TRIP_PLAN_LIVE_IN_SEARCH`): every train of the
  date TRUST knows to be cancelled or late, from `train_current_state` alone,
  so the first search already runs on live times. TRUST only.
- **Per-train reads** (`fetch_train_lives_with_darwin`): the trains of the
  plan being annotated or re-planned, at most `TRIP_PLAN_LIVE_MAX_TRAINS`
  (60), inside the lookback and horizon window. TRUST movements and state,
  `train_reasons`, the station boards (Darwin's LDBWS departures) and the
  Darwin Push Port (`darwin_calls`).

TRUST has no report for a minority of trains (a station outside the line
catalogue, a train with no activation, a stale state) and reports at timing
points only, where the Push Port reports every call.

## Darwin Push Port times (DS-591)

TRUST is the default. Per side (arrival, departure) of each call of a train
read one by one:

1. A call TRUST has reported keeps TRUST's actual. Darwin is not consulted
   for it, so it never lowers a reported delay.
2. Otherwise a Push Port actual (`arrival_at`, `departure_at`) is used: an
   actual always beats a carried estimate.
3. Otherwise TRUST's carried estimate, when the train's TRUST state is fresh
   (updated within `TRIP_PLAN_LIVE_TRUST_MAX_AGE_MINUTES`, 30): the delay
   TRUST last reported, carried flat to the calls ahead. Darwin's forecast
   does not overrule it.
4. Otherwise, with no TRUST state or a stale one, the Push Port forecast
   (`arrival_et`, `departure_et`), as an estimate.

A train with no `trains` row at all (TRUST has never seen it) is read too,
and kept only if the Push Port gave it something.

Times are `HH:MM` Europe/London and are dated against the call's own
scheduled instant, so a forecast past midnight and the autumn repeated hour
resolve correctly (`eta_blend::resolve_london_estimate_near`). A call is
found in the Push Port's locations as the platform overlay does
(`pushport_platform::find_call`).

The read is one query per call of `fetch_train_lives_with_darwin`, through
`darwin_services_uid_ssd` and `darwin_calls`' primary key, and is skipped for
an empty train list. A failed read is logged and the plan is annotated from
TRUST and boards only.

Not changed: journey feasibility, journey breaks and notifications read
trains with `fetch_train_lives`, which has no Darwin; the in-search snapshot
reads no `darwin_calls`.

`TRIP_PLAN_LIVE_DARWIN_ENABLED` (default `true`) turns the Push Port fill
off. `distant_signal_api_trip_plan_darwin_fallback_total{kind="actuals"|"forecasts"}`
counts trains where Darwin filled something in.

Not applied here: a traveller's own extra time to change trains
(`users.change_extra_minutes`, DS-757). It shapes only the judgement of a
saved journey for its owner ([journey-breaks.md](journey-breaks.md));
`/Trips/plan` is anonymous and searches one cached graph, so a plan is
unchanged by it (out of scope for now).
