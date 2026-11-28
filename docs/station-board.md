# Station board

`GET /public/stations/{crs}/board` is a station's next departures: the
published timetable with Darwin's live times and platforms laid over it. It
is the api half of a logged-out station board; the website's page is a later
change. It is off by default (`STATION_BOARD_ENABLED`) and answers `404` while
off.

It is a new route. `GET /public/trains/search`, `/public/stations/{crs}/departures`
and `/public/stations/{crs}/schedule-departures` are unchanged.

## Request

| Query | Meaning |
| --- | --- |
| `limit` | Rows, 1 to 30 (default 8). A larger value is clamped; `0` or a non-number is a `400`. |
| `date` | A London service date (`YYYY-MM-DD`), within the range `GET /public/trains/search` accepts (`400` otherwise). Default today. |
| `operator` | Comma-separated ATOC codes, validated as the train search does (`400` for a malformed or unknown code). |

Departures only. The arrivals view of the timetable is
`GET /public/trains/arrivals`. Unknown query keys are ignored, so a
cache-buster works. `404` when the station has no timetable data for the day.

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
      "platform": null,
      "platformStatus": "toBeAnnounced",
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

- `scheduled` is the public (booked) departure shown to passengers, `HH:MM`,
  on day `dayOffset` (1 after the service date's midnight) of `serviceDate`.
  `expected` is Darwin's actual or forecast departure with its own
  `expectedDayOffset`; both are `null` with no live data.
- `uid` and `serviceDate` name the train for `GET /Train/by-uid/{uid}/{serviceDate}`.
  Just after London midnight a row can be the previous service date's
  (its `serviceDate` is then yesterday and its `dayOffset` is 1).
- `attribution` is the credit the data needs; republish it with the data.
- `Cache-Control: public, s-maxage=20, stale-while-revalidate=30`.

### `status`

| Value | Means |
| --- | --- |
| `cancelled` | Darwin has cancelled the call. `cancelReason` is the reason's text when Darwin gave a known code. |
| `delayed` | A Darwin forecast or actual at least a minute after the booked time (`delayMinutes`), or Darwin's "delayed, time unknown" (`delayMinutes` and `expected` are `null`). |
| `onTime` | A Darwin forecast or actual within the booked minute or earlier (`delayMinutes` 0). |
| `booked` | No live data. The board never says `onTime` without Darwin's word. |

### `platformStatus`

| Value | `platform` | Means |
| --- | --- | --- |
| `confirmed` | the platform | Darwin's platform, confirmed by the operator, not hidden from the public. |
| `expected` | the platform | A live platform not yet confirmed: Darwin's, else the station's own LDBWS board's. |
| `booked` | the platform | The timetable's platform, with no live word. |
| `toBeAnnounced` | `null` | Not announced (see the precedence below). Show "Platform to be announced". |
| `none` | `null` | No platform known (or the train is cancelled). |

## Platform precedence

For each row, first match wins:

1. **Darwin's platform** for the call, unless Darwin hides it from the
   public. Hidden is the platform flags `pushport_platform` already refuses
   (`platsup`, `cisPlatsup`: `DarwinCall::platform_hidden`) or a suppressed
   location (`suppr`). `confirmed` when Darwin says confirmed, else `expected`.
2. **Darwin has the call and hides it**: `toBeAnnounced`. Never a fallback to
   a booked platform, and the hidden platform string is never read past the
   check, so it cannot reach the JSON (the DB tests search the whole body for
   it).
3. **The station's own LDBWS board** (`station_samples`), for rows with
   neither of the above, when the board is no more than 10 minutes old and
   exactly one board row and exactly one timetable row share the headcode,
   the shown time and the destination: `expected`. LDBWS's `serviceID` is a
   board-relative token and its `rsid` is not unique per day, and the board
   carries no train UID, so a weaker join is not safe. A row with no headcode,
   or an ambiguous match on either side, skips this step. The board is read
   only when some row can use it, and only for rows whose calendar day is
   today.
4. **The CIF booked platform** (`schedule_calling_points_full.platform`),
   `booked`, except at the stations in `STATION_BOARD_NO_BOOKED_PLATFORM_CRS`
   (below), where it is never shown and the row is `toBeAnnounced`.
5. `none`.

A call Darwin has cancelled has no platform (`none`).
`darwin_calls.planned_platform` is not used (it is empty in production).

### London termini

At the six London termini Darwin flags 92 to 99 per cent of public
departures' platforms as suppressed until they are announced, and the
timetable's platform there is a guess passengers must not walk to. So at
`STATION_BOARD_NO_BOOKED_PLATFORM_CRS` (default `VIC,EUS,PAD,KGX,MYB,STP`) a
booked platform is never shown: the board says `toBeAnnounced`
("Platform to be announced") unless a public live platform exists (Darwin's,
unhidden, or the LDBWS board's). A row with no booked platform there is also
`toBeAnnounced`.

### Licence assumption

Platforms from Darwin are treated as permitted for logged-out display. This is
the owner's decision (2026-11-28), not something verified against the Rail Data
Marketplace terms here. Darwin's own suppression flags are honoured in full:
a platform Darwin says is not for the public is never served.

## How the board is read

Four reads, whatever the size, none per row:

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

A DB test runs `EXPLAIN` on the Darwin, booked-platform and TIPLOC reads with
sequential scans disabled and asserts the indexes above (and no sequential
scan of `darwin_calls` or `darwin_services`).

Rows that have left (a Darwin actual departure) or are no longer due (the
forecast, else the booked time, is before the current minute) are dropped
before the cut to `limit`. Rows are ordered by their shown booked time.

## Abuse and load

GET reads are unclassified by the rate limiter, and this route adds no tier.
What caps abuse is the cache (`s-maxage=20`, so a shared cache serves each
distinct URL once per 20 s), `limit` at 30 and the cheap queries above.

## Operating it

| Setting | Default | |
| --- | --- | --- |
| `STATION_BOARD_ENABLED` | `false` | `404` while off. |
| `STATION_BOARD_NO_BOOKED_PLATFORM_CRS` | `VIC,EUS,PAD,KGX,MYB,STP` | Comma-separated 3-letter CRS codes; set and blank means none; a bad code stops the api at startup. |

Both are in `--print-config-defaults`. The chart values (ds-deploy) are a later
change, after the image that reads them.

**Rollback**: set `STATION_BOARD_ENABLED=false` (or unset it) and restart the
api: the route is `404` again. No migration, no grant and no stored state
change with the switch.

**Grants** (the `api` role, `db-grants.yaml`): `SELECT` on `darwin_services`,
`darwin_calls`, `schedule_destination_departures`,
`schedule_calling_points_full`, `tiploc_crs`, `station_samples`, `stations`
and `tocs`. Nothing new is needed.
