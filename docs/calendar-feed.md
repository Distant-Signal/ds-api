# Journey calendar feed

An opt-in calendar of a user's saved and repeating journeys, for any
calendar app that subscribes to an iCalendar (`.ics`) URL, with a Delay
Repay claim reminder after a late journey. Off unless
`CALENDAR_FEED_ENABLED` (the chart's `api.calendarFeed.enabled`, default
false): until then every route below answers `404`.

It is a plan view, not live status. Calendar apps refresh a subscribed feed
every 8 to 24 hours (the feed asks for 12, which most apps treat as a hint),
so an event says where and when a journey was planned for and nothing about
the train. The site's pages are the live view.

## Routes

| Route | Auth | What |
|---|---|---|
| `GET /public/account/calendar-feed` | session cookie | `{active}`: whether the caller has a feed link |
| `POST /public/account/calendar-feed` | session cookie, same origin | makes the link (revoking any earlier one) and returns `{token}`, once, `no-store` |
| `DELETE /public/account/calendar-feed` | session cookie, same origin | revokes the link, `204`, idempotent |
| `GET /public/calendar/{token}` | the token | the feed; a trailing `.ics` on the token is ignored |
| `GET /Journeys/{id}/calendar.ics` | session cookie | one of the caller's own journeys as an attachment; `404` for anyone else's |

The site serves the feed at `/api/calendar/{token}.ics`.

## The secret link

The same primitive as a journey share link (`data::unlisted_links`, no
schema change): a 32-byte random token, stored as its SHA-256, so a database
read cannot hand out working feeds and the token is shown once, when it is
made. `resource_type` is `calendar_feed` and `resource_id` the user's id,
so a user has one active link; making a new one revokes the old. The link
has no expiry: a calendar app cannot be told a link is about to lapse, and
one that quietly stops updating is worse than one the user turns off.
Deleting the account removes the link's rows with the user.

A token for any other resource type is `404` here, as an unknown one is,
with nothing to tell them apart. The token is the whole credential, so the
route sends `Cache-Control: private`, `Referrer-Policy: no-referrer` and
`X-Robots-Tag: noindex`, is rate limited per client IP in the
`/Train/by-uid` budget, and is logged as its route template (an unmatched
path as `/public/calendar/[REDACTED]`).

## What an event says

One event per journey of the caller's own (followed and group-shared
journeys are never in it), from a week ago on: `Journey: {first origin} to
{last destination}`, starting at the first departure. No train, operator,
ticket, platform, delay, name the user gave the journey, share or group
detail. An end time is given only for a journey that is one planned
non-train leg; a train's arrival is not known to the journeys list, so
other events have a start only.

Repeating journeys appear as the journeys the notifier has made from them
(a regular journey's occurrences are journeys, up to a few days ahead), not
as a recurrence rule.

The description carries the data licences' credits as the Atom feeds do
(`routes::feeds::Credit`: National Rail, Network Rail, and TfL when a
journey has a non-train leg), the non-affiliation statement and `/attribution`.

## Delay Repay reminder

For a journey that is over and whose final arrival was measured at least
the operator's minimum delay late, or that had a cancelled train, a
whole-day event on the last day of the operator's claim window (counted
from the journey's day), with an alarm 7 days before, while that window is
open. Like `delay_repay_rules` it estimates and links out: nothing here
submits, records or tracks a claim, and the text sends the traveller to
their train company's own page, which is the one that counts.

The rule is the last train's operator's (`delay_repay_rules::claim_rule_for`,
the CIF schedule's ATOC code through `data::train_operator`), checked on
2026-11-09 against the operators' own pages (sources and which pages could
only be confirmed through their search-result text are in the module doc of
`data/delay_repay_rules.rs`):

| Operator | Reminder from a delay of | Claim window |
|---|---|---|
| LNER, CrossCountry, ScotRail, Caledonian Sleeper, Hull Trains, Lumo | 30 minutes | 28 days |
| Heathrow Express, Elizabeth line, London Overground, Merseyrail ("more than 30 minutes") | 31 minutes | 28 days |
| Grand Central | 60 minutes | 28 days |
| Every other operator, and a train whose operator is not known (Delay Repay 15; GWR, Southeastern and Avanti West Coast say so, the rest are assumed) | 15 minutes | 28 days |

A cancelled train gets a reminder at any operator. When the journey has
several trains, the last one's operator is used, because the delay is the
one at the final arrival.

## Privacy

A feed shows travel patterns to whoever holds the link, and to the
calendar provider, which fetches it. The page that makes the link says so
and that anyone with it can see the journeys; `/privacy` lists the data
flow. The fields above are the least that is useful.
