# Public read API

Distant Signal serves a small, anonymous, read-only JSON API on the main
site, `https://ds.cursed.solutions`. Its line status routes use the same
paths and response shape as TfL's Unified API `Line` and `StopPoint`
routes, so a client written for those can read Distant Signal's lines
too. It is Distant Signal's own service: independent and unofficial, and
not an endpoint of TfL, National Rail or anyone else.

No sign-in, cookie or key is used. Every caller gets the same answer: a
signed-in visitor's private custom lines never appear here. Requests are
rate limited per IP address. Only `GET` and `HEAD` are accepted; any
other method is answered `405`, and any path not listed below `404`.

## Routes

| Path | Answer | Cached for |
| --- | --- | --- |
| `/Line/Mode/{modes}/Status` | Current status of every line of the given modes | 30 s |
| `/Line/{ids}/Status` | Current status of the given lines | 30 s |
| `/StopPoint/{crs}/Disruption` | Disrupted lines calling at a station | 30 s |
| `/Line/{id}/Status/{from}/to/{to}` | A line's status history | 60 s |
| `/Line/{id}/Stats/{from}/to/{to}` | Daily sample statistics | 60 s |
| `/Line/{id}/Stats/HalfHourly/{from}/to/{to}` | Half-hourly statistics | 60 s |
| `/Line/{id}/Stats/Hourly/{from}/to/{to}` | Hourly statistics | 60 s |
| `/Line/{id}/Stats/SixHourly/{from}/to/{to}` | Six-hourly statistics | 60 s |
| `/Line/{id}/Stats/Coverage/{from}/to/{to}` | Daily coverage statistics | 60 s |
| `/Line/{id}/Stats/Coverage/HalfHourly/{from}/to/{to}` | Half-hourly coverage statistics | 60 s |

- `{modes}`: comma-separated, from `national-rail`, `tube`, `dlr`,
  `overground`, `elizabeth-line`, `tram`.
- `{ids}`, `{id}`: line ids as the site uses them (`wcml`,
  `tfl-victoria`), lower case; `{ids}` is comma-separated.
- `{crs}`: a three-letter station code (`EUS`).
- `{from}`, `{to}`: a date (`2026-11-01`) for the daily routes, an
  ISO 8601 instant with an offset (`2026-11-01T00:00:00Z`) for the rest.
- `?detail=true` on the two status routes adds each status's
  `disruption`. No other query parameter is read.

The cache time is the `s-maxage` in `Cache-Control`; a cache may serve a
stale copy while it refreshes for another 30 s (status) or 300 s
(history and statistics). Errors are sent `no-store`. A `503` or `429`
carries `Retry-After`. Any site may read the answers from a browser
(`Access-Control-Allow-Origin: *`).

The api changelog (`docs/api-changelog.md`) records changes to these
shapes.

## Attribution

Each response is a bare JSON array, as TfL's is, so the credits travel on
each element: `attribution` is an array of `{licence, text}`. Republish
the `text` with the data. `licence` is an SPDX-style id:

| `licence` | Used for |
| --- | --- |
| `LicenseRef-RDM-Data-Sharing-Agreement` | National Rail lines: "powered by NationalRail (Train Information Services Ltd)", and on a status "Incidents from the Knowledgebase, published by Rail Delivery Group" |
| `LicenseRef-TfL-Transport-Data-Service` | TfL lines, and a National Rail line's `tflStatus`: "Powered by TfL Open Data" |

Every response also carries `Link: </attribution>; rel="license"`,
the site's page of data sources and licences.

## Feeds

Atom feeds of line status changes and incidents, per line, per operator
and for the whole network, are at `/api/feeds/...`, with the same credits
in `<rights>`: see `docs/feeds.md`.

## Example

```sh
curl https://ds.cursed.solutions/Line/Mode/national-rail/Status
```

```json
[
  {
    "$type": "DistantSignal.LineStatusReport",
    "id": "wcml",
    "name": "West Coast Main Line",
    "modeName": "national-rail",
    "operators": ["VT"],
    "computedAt": "2026-11-03T09:00:00+00:00",
    "lineStatuses": [{ "statusSeverity": 10, "statusSeverityDescription": "Good Service", "...": "..." }],
    "upcoming": [],
    "attribution": [
      {
        "licence": "LicenseRef-RDM-Data-Sharing-Agreement",
        "text": "powered by NationalRail (Train Information Services Ltd)"
      },
      {
        "licence": "LicenseRef-RDM-Data-Sharing-Agreement",
        "text": "Incidents from the Knowledgebase, published by Rail Delivery Group"
      }
    ]
  }
]
```

## How it is served

The frontend's route handlers `frontend/app/Line/[...path]/route.ts` and
`frontend/app/StopPoint/[crs]/Disruption/route.ts` (shared code in
`frontend/lib/tflPublicApi.ts`) check the path against the list above,
segment by segment, and fetch the api's route of the same path with no
cookie and no `Authorization`, sending only `X-Real-IP`. The site's own
pages read these routes from the api directly, as before.
