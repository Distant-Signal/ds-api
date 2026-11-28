# Station board

`GET /public/stations/{crs}/board` is a station's next departures, or with
`board=arrivals` its next arrivals: the published timetable with Darwin's live
times and platforms laid over it. It is the api half of a logged-out station
board; the website's page is a later change. It is off by default
(`STATION_BOARD_ENABLED`, one switch for both boards) and answers `404` while
off.

`GET /public/trains/search`, `/public/stations/{crs}/departures`,
`/public/stations/{crs}/schedule-departures` and `/public/trains/arrivals` are
unchanged.

## Request

| Query | Meaning |
| --- | --- |
| `limit` | Rows, 1 to 30 (default 8). A larger value is clamped; `0` or a non-number is a `400`. |
| `date` | A London service date (`YYYY-MM-DD`), within the range `GET /public/trains/search` accepts (`400` otherwise). Default today. |
| `operator` | Comma-separated ATOC codes, validated as the train search does (`400` for a malformed or unknown code). |
| `board` | `departures` (the default, the board as it always was) or `arrivals` (any case; anything else is a `400`). |

Unknown query keys are ignored, so a cache-buster works. `404` when the
station has no timetable data for the day (for `arrivals`, as a calling point
or as a terminus).

## Response

```json
{
  "crs": "PAD",
  "name": "London Paddington",
  "serviceDate": "2026-11-28",
  "generatedAt": "2026-11-28T10:00:07Z",
  "departures": [
    {
      "uid": "G12345",
      "serviceDate": "2026-11-28",
      "scheduled": "10:06",
      "dayOffset": 0,
      "expected": "10:09",
      "expectedDayOffset": 0,
      "destinationCrs": "BRI",
      "destinationName": "Bristol Temple Meads",
      "operator": "GW",
      "operatorName": "Great Western Railway",
      "headcode": "1A23",
      "platform": "12",
      "platformStatus": "likely",
      "platformBasis": "inbound",
      "status": "delayed",
      "delayMinutes": 3,
      "isCancelled": false,
      "cancelReason": null
    }
  ],
  "attribution": [
    {
      "licence": "LicenseRef-RDM-Data-Sharing-Agreement",
      "text": "powered by NationalRail (Train Information Services Ltd)"
    }
  ]
}
```

- `board` echoes which board this is.
- `scheduled` is the public (booked) departure shown to passengers, `HH:MM`,
  on day `dayOffset` (1 after the service date's midnight) of `serviceDate`.
  `expected` is Darwin's actual or forecast departure with its own
  `expectedDayOffset`; both are `null` with no live data.
- `uid` and `serviceDate` name the train for `GET /Train/by-uid/{uid}/{serviceDate}`.
  Just after London midnight a row can be the previous service date's
  (its `serviceDate` is then yesterday and its `dayOffset` is 1).
- `attribution` is the credit the data needs; republish it with the data.
- `Cache-Control: public, s-maxage=20, stale-while-revalidate=30`.

### The arrivals board (`board=arrivals`)

The same response, with the rows in `arrivals` (and `departures` an empty
array; on the departures board `arrivals` is absent, so a departures response
gains only `board`):

```json
{
  "crs": "PAD",
  "name": "London Paddington",
  "serviceDate": "2026-11-28",
  "generatedAt": "2026-11-28T10:00:07Z",
  "board": "arrivals",
  "departures": [],
  "arrivals": [
    {
      "uid": "G12345",
      "serviceDate": "2026-11-28",
      "scheduled": "10:06",
      "dayOffset": 0,
      "expected": "10:09",
      "expectedDayOffset": 0,
      "originCrs": "BRI",
      "originName": "Bristol Temple Meads",
      "destinationCrs": "PAD",
      "destinationName": "London Paddington",
      "terminates": true,
      "operator": "GW",
      "operatorName": "Great Western Railway",
      "headcode": "1A23",
      "platform": null,
      "platformStatus": "toBeAnnounced",
      "status": "delayed",
      "delayMinutes": 3,
      "isCancelled": false,
      "cancelReason": null
    }
  ],
  "attribution": [ ... ]
}
```

- `scheduled` and `expected` are the public (booked) and Darwin's actual or
  forecast **arrival** (`dayOffset` and `expectedDayOffset` as above).
- `originCrs` / `originName` are where the train started; `destinationCrs` /
  `destinationName` where it ends, and `terminates` says that is this
  station (`destinationCrs` is then the station's own code). A station that
  only some of a train's journey passes has the train as an arrival at its
  call, with `terminates: false`.
- Rows are services that arrive here: a call a passenger can get off at
  (pick-up-only calls are not) and every train that ends here. A service that
  only starts here has no arrival and is not on the board.
- The window mirrors the departures board: from 15 minutes before now (a late
  train stays on it), and a train Darwin reports as arrived (an actual
  arrival) is dropped, as a departed train is on the departures board.
  Just after London midnight the previous service date's trains still arriving
  appear with `serviceDate` yesterday and `dayOffset` 1.
- `status`, `delayMinutes`, `platform` and `platformStatus` use the values
  below, read from Darwin's arrival (`arrival_at`, `arrival_et`,
  "delayed, time unknown" for the arrival) and the booked arrival.

### `status`

| Value | Means |
| --- | --- |
| `cancelled` | Darwin has cancelled the call. `cancelReason` is the reason's text when Darwin gave a known code. |
| `delayed` | A Darwin forecast or actual at least a minute after the booked time (`delayMinutes`), or Darwin's "delayed, time unknown" (`delayMinutes` and `expected` are `null`). |
| `onTime` | A Darwin forecast or actual within the booked minute or earlier (`delayMinutes` 0). |
| `booked` | No live data. The board never says `onTime` without Darwin's word. |

### `platformStatus`

| Value | `platform` | `platformBasis` | Means |
| --- | --- | --- | --- |
| `confirmed` | the platform | `null` | Darwin's platform, confirmed by the operator, not hidden from the public. |
| `expected` | the platform | `null` | A live platform not yet confirmed: Darwin's, else the station's own LDBWS board's. |
| `likely` | the platform | `inbound` or `booked` | A prediction, not an announcement: show it labelled as such (see below). |
| `booked` | the platform | `null` | The timetable's platform, with no live word, where Darwin hides nothing. |
| `toBeAnnounced` | `null` | `null` | Not announced and nothing independent predicts one. Show "Platform to be announced". |
| `none` | `null` | `null` | No platform known (or the train is cancelled). |

`platformBasis` for `likely`: `inbound` is the platform the working that forms
this service (`schedule_turnarounds`: a timetable association, or inferred
from the platform sequence) arrives at at this station, from Darwin's arrival
call or, when Darwin has none for it, TRUST's report that the unit arrived
there and has not moved on (as the train view's `platformBasis` rule 2 reads
it); stock normally leaves from where it arrived. `booked` is the CIF
platform.

A client must tolerate a `platformStatus` it does not know. `booked` stays for
rows where Darwin hides nothing and the station is not listed (below); it was
shipped before `likely` and keeps its meaning.

## Platform precedence

For each row, first match wins:

1. A call Darwin has cancelled: `none`.
2. **Darwin's platform** for the call, unless Darwin hides it from the
   public. Hidden is the platform flags `pushport_platform` already refuses
   (`platsup`, `cisPlatsup`: `DarwinCall::platform_hidden`) or a suppressed
   location (`suppr`). `confirmed` when Darwin says confirmed, else `expected`.
3. **The station's own LDBWS board** (`station_samples`; **departures board
   only**: it holds the station's departures, and its arrivals board is kept
   for tracked trains alone, so the arrivals board skips this step), for rows
   Darwin does not hide, when the board is no more than 10 minutes old and exactly one
   board row and exactly one timetable row share the headcode, the shown time
   and the destination: `expected`. LDBWS's `serviceID` is a board-relative
   token and its `rsid` is not unique per day, and the board carries no train
   UID, so a weaker join is not safe. A row with no headcode, or an ambiguous
   match on either side, skips this step. The board is read only when some row
   can use it, and only for rows whose calendar day is today.
4. **The inbound working's platform** (departures board only: an arrival's
   platform is where it arrives): `likely`, basis `inbound` (above).
5. **The CIF booked platform** (`schedule_calling_points_full.platform`):
   `likely`, basis `booked`, where Darwin hides this call or the station is in
   `STATION_BOARD_NO_BOOKED_PLATFORM_CRS`; else `booked`.
6. `toBeAnnounced` where Darwin hides the call or the station is listed; else
   `none`.

`darwin_calls.planned_platform` is not used (it is empty in production).

### The owner decision and the suppression rule

The first release never showed a timetable platform at the six London
termini, where Darwin flags 92 to 99 per cent of public departures'
platforms as suppressed until they are announced. The owner has overridden
that (2026-11-28): show a likely platform wherever one can be derived,
always labelled as a prediction, and say "to be announced" only when nothing
can be derived.

The suppression is still honoured in full:

- A platform Darwin flags suppressed (`platsup`, `cisPlatsup`) or a
  suppressed location is never served for that service's own call, and its
  string is never read past the check, so it cannot reach the JSON (the DB
  tests search the whole body for it).
- A hidden **inbound arrival** is never a source of a prediction, and
  neither is TRUST for that working: a prediction comes only from the
  timetable, or from an inbound working Darwin itself shows publicly (or has
  no word on).
- A cancelled call has no platform, and a cancelled inbound working
  predicts nothing.

When the real platform is announced Darwin takes precedence at once, so a
wrong prediction flips to the announced platform on the next board (the row's
`platformStatus` changes from `likely`; clients should label, not pin).

### What is not predicted: the usual platform

A third basis, the platform a service most often uses here, was considered and
not shipped. The only history of real platforms is `train_movement_events.platform`
(TRUST, since 2026-10-26), and DS writes movement events only for trains it
tracks (`trains`/`tracked_trains`), so for most services there is nothing to
count; Darwin's own calls are kept 2 days. A query would be cheap
(`trains` by `(train_uid, service_date)`, then `train_movement_events_trains_id`)
but would answer for almost no row, and a 3-observation majority from
tracked trains only is not reliable. A real platform history table would be
a ds-core change (a pre-aggregated per station and service key table fed by
the full-coverage consumer).

### Licence assumption

Platforms from Darwin are treated as permitted for logged-out display. This is
the owner's decision (2026-11-28), not something verified against the Rail Data
Marketplace terms here. Darwin's own suppression flags are honoured in full:
a platform Darwin says is not for the public is never served.

## How the board is read

Reads, whatever the size, none per row (the departures board; the
arrivals board's differences follow):

1. **Timetable rows**: `search_schedule_calling_point_departures`, the read
   behind the train search and `schedule-departures`: a bounded range scan of
   `schedule_destination_departures_calling_point_idx` (`service_date`,
   `origin_crs`, `scheduled`, `train_uid`) from 15 minutes before now (so a
   late train stays on the board), `limit + 40` rows, plus the previous service
   date's trains still running after its midnight when the date is today.
2. **Darwin**: `darwin_services` by `(uid, ssd)` pairs (index
   `darwin_services_uid_ssd`), then `darwin_calls` by `rid` and the station's
   TIPLOCs (the primary key `(rid, tiploc, wta, wtd, wtp)`), keeping only the
   locations of the newest schedule, as `ds_store::darwin::calls_for_trains`
   does. `darwin_calls` has no TIPLOC index and is never scanned by station.
   A visit is picked by its working departure minute, else the only call at
   the station.
3. **CIF booked platforms**: `schedule_calling_points_full` by
   `(service_date, uid)` and TIPLOC (its primary key), picked the same way.
4. The station's TIPLOCs (`tiploc_crs_crs_idx`), the names and, if needed, the
   LDBWS board (`station_samples` by primary key).
5. **Inbound workings**, only for the rows on the board (after the cut to
   `limit`) with no public platform of their own, and only when there are
   any: `schedule_turnarounds` by `(service_date, uid)` (its primary key,
   restricted to this station's TIPLOCs as the turnaround location); then the
   inbound trains' Darwin calls with the same statement as step 2 (the inbound
   `(uid, ssd)`, `darwin_services_uid_ssd`, then `darwin_calls`' primary key; never
   a scan by station); then, only for inbound workings Darwin has no platform
   for, the train view's `previous_working_arrivals` (`trains`' unique
   `(train_uid, service_date)`, then `train_movement_events_trains_id`, one
   latest arrival per working). At most three statements, each a key lookup
   over at most `limit` keys.

A DB test runs `EXPLAIN` on the Darwin, booked-platform, turnaround and TIPLOC reads with
sequential scans disabled and asserts the indexes above (`schedule_turnarounds_pkey` for the turnaround read, and no sequential
scan of `darwin_calls` or `darwin_services`).

Rows that have left (a Darwin actual departure) or are no longer due (the
forecast, else the booked time, is before the current minute) are dropped
before the cut to `limit`. Rows are ordered by their shown booked time.

## How the arrivals board is read

The Darwin, booked-platform and TIPLOC reads are the same (a visit is picked
by its working **arrival** minute, `darwin_calls.wta` and
`schedule_calling_points_full.booked_arrival`, else the only call at the
station); there is no LDBWS read. The timetable read is its own, over the same
table, `schedule_destination_departures`, which has one row per
departure-bearing calling point of each train (so a train's last stop, where it
ends, has no row of its own):

- **Calls**: `service_date`, `origin_crs` (the calling point), with an arrival
  (`calling_point_arrival` or its public time) and `can_alight`. A `UNION ALL`
  of two bands, as the departures search reads them: day-0 rows, a range scan
  of `schedule_destination_departures_calling_point_idx` (`service_date`,
  `origin_crs`, `scheduled`) from the first working time that can hold a row on
  the board (a call's arrival is never after its own departure, and a public
  time sits within an hour of the working one, so `scheduled >= from - 1h`
  loses none), and the rows past the date's midnight. For today the previous
  service date's after-midnight rows are read too. The cost is the rest of the
  station's day, not `limit + 40`: up to roughly a thousand index entries at
  the busiest stations. A train's own origin row has no arrival, so a service
  that starts here is never an arrival.
- **Termini**: the trains that end at the station (`destination_crs`),
  `DISTINCT ON (train_uid)`, over the primary key's `(service_date,
  destination_crs)` prefix. **This is the expensive half**: no index leads
  with the arrival time, so it reads every departure-bearing call of every
  train that ends here that day (about 10,000 rows for the busiest
  destination in the sizing of the table's migration) and filters on the
  arrival in the heap. It is the shape `GET /public/trains/arrivals` already
  uses (there with no time bound at all), and it runs once per distinct board
  URL per 20 s behind the cache. If that proves too heavy, the smallest fix
  is a ds-core index on `(service_date, destination_crs,
  destination_arrival_day_offset, destination_arrival)` (a partial index
  `WHERE destination_arrival IS NOT NULL` is enough), after which this read
  becomes a bounded range scan; it is not needed to ship the board, and this
  change adds none.

The rows are then sorted by the arrival shown, cut to `limit + 40` and given
Darwin's and the booked data in the same batched reads as the departures
board. A DB test runs `EXPLAIN` on the two timetable reads with sequential
scans disabled and asserts the indexes above.

## Abuse and load

GET reads are unclassified by the rate limiter, and this route adds no tier.
What caps abuse is the cache (`s-maxage=20`, so a shared cache serves each
distinct URL once per 20 s), `limit` at 30 and the cheap queries above.

## Operating it

| Setting | Default | |
| --- | --- | --- |
| `STATION_BOARD_ENABLED` | `false` | `404` while off. |
| `STATION_BOARD_NO_BOOKED_PLATFORM_CRS` | empty | Comma-separated 3-letter CRS codes whose booked platform is shown only as `likely` (basis `booked`), never as `booked`; set and blank means none; a bad code stops the api at startup. It defaulted to `VIC,EUS,PAD,KGX,MYB,STP` (booked platform withheld) until the owner decision above; kept for emergency use. |

Both are in `--print-config-defaults`. The chart values (ds-deploy) are a later
change, after the image that reads them.

**Rollback**: set `STATION_BOARD_ENABLED=false` (or unset it) and restart the
api: the route is `404` again. No migration, no grant and no stored state
change with the switch.

**Grants** (the `api` role, `db-grants.yaml`): `SELECT` on `darwin_services`,
`darwin_calls`, `schedule_destination_departures`,
`schedule_calling_points_full`, `schedule_turnarounds`, `trains`,
`train_movement_events`, `tiploc_crs`, `station_samples`, `stations` and `tocs`.
Nothing new is needed (the api already reads them).
