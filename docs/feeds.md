# Atom feeds

Distant Signal publishes Atom 1.0 feeds of line status changes and
incidents, for feed readers and anything that reads RSS or Atom (chat bots,
home automation, feed-to-email). They need no account.

| Feed | Site URL | Follows |
|---|---|---|
| Network | `/api/feeds/network` | every National Rail and TfL line on `/status` |
| Line | `/api/feeds/lines/{id}` | one line, as on `/lines/{id}` |
| Operator | `/api/feeds/operators/{code}` | an operator's lines, as on `/operators/{code}/history` |

The api serves them under `/public/feeds/...`
(`crates/api/src/routes/feeds.rs`); the frontend's `/api` proxy maps the
site URLs to those. The line, operator and status pages link their feed
with `<link rel="alternate" type="application/atom+xml">`, so a reader
finds it from the page's own address, and show a small "Feed" link.

## Entries

- **Status changes.** One entry each time a line's worst status changes
  severity rank (`common::severity_rank`, the groups the site colours by),
  read from `line_status_history`. A change of wording within a rank is
  not an entry, and neither is TfL's overnight "Service Closed", which the
  feed counts as good service. A line's first row in the history is its
  baseline. The entry links to the line's page.
- **Incidents.** One entry per National Rail Knowledgebase incident, linked
  to its incident page. `published` is when DS first saw it; `updated`
  moves when its text, validity windows or cleared state change, or when
  National Rail stops listing it. The entry gives the summary, the
  windows and whether it was cleared; the full text is on the site.
  A line's feed lists the incidents matched to that line, an operator's
  those naming its code (for TfL, the London Overground and Elizabeth line
  codes), the network feed all of them. A TfL line's feed has none.

Each feed lists the newest 50 entries from the last 30 days.
`line_status_history` is kept for `HISTORY_RETENTION_DAYS` (7 by default),
so status changes reach back that far only.

Entry ids are `tag:` URIs that never change once published:
`tag:{host},2026:line-status/{line id}/{history row id}` and
`tag:{host},2026:incident/{incident id}`. `{host}` is the site's host,
taken from `SSO_REDIRECT_URL` (the frontend's public URL), so the ids
change only if the site moves to a new host.

Custom lines are never in a feed, not even their owner's: the feeds are
public, and their line set is the catalogue plus the TfL lines, as for the
operator rollups.

## Credits and branding

The data's licences make the credits conditions, so the feed's `<rights>`
lists every credit its lines can need, followed by the non-affiliation
statement and a link to `/attribution`, and each entry's own `<rights>`
(repeated at the end of its content) lists the credits for that entry's
data:

- National Rail lines: "powered by NationalRail (Train Information Services
  Ltd)" (Darwin and Knowledgebase Stations), and "Line status worked out
  by Distant Signal from this data; not an official status.", since the
  severity of a National Rail line is DS's own reading of the data. Text
  from a Knowledgebase incident adds the Rail Delivery Group credit; a
  delay summary DS wrote from departure or train movement data is marked
  as DS's, and one from train movements adds the Network Rail
  Infrastructure Limited licence statement.
- TfL lines: "Powered by TfL Open Data". The Ordnance Survey and Geomni
  statements cover TfL's geographic data (stop locations), which no feed
  carries.
- Incidents: the National Rail and Rail Delivery Group credits.

The wording matches `frontend/components/OpenDataAttribution.tsx` and
`frontend/lib/tflAttribution.ts`; change them together.

Nothing in a feed may read as an official railway, operator or TfL source:
titles say "(unofficial, from Distant Signal)", the author is always
Distant Signal, the subtitle carries the non-affiliation statement, and
the feeds have no icon or logo.

## Caching and limits

Every response carries `Cache-Control: public, max-age=300` and a strong
`ETag` over the document; a request whose `If-None-Match` matches gets a
`304` with no body. The frontend proxy relays both headers. The feeds are
identical for every caller (no cookies are read), so the edge may cache
them: Cloudflare only caches these paths with a cache rule for
`/api/feeds/*` that respects the origin's `Cache-Control`.

A cache miss reads Postgres, so the api rate-limits the feeds per client IP
in the `/Train/by-uid` budget (`API_RATE_LIMIT_TRAIN_BY_UID_*`,
`crates/api/src/rate_limit.rs`).
