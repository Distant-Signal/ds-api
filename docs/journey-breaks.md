# Journey breaks and suggestion pushes

Runbook for mid-journey changes (design:
`docs/superpowers/specs/2026-10-22-mid-journey-changes-design.md`).

## What runs where

- **The api** judges journeys and plans alternatives. A journey page read
  (`GET /Journeys/{id}`, by its owner) opens or ends a break at once. With
  `api.journeyBreaks.enabled`, the break sweep (`crates/api/src/journey_break_sweep.rs`)
  also runs every `api.journeyBreaks.intervalSecs` (60) under the advisory
  lock `journey_breaks` (`dslpjbrk`), so one api replica runs it whatever
  `API_BACKGROUND_LOOPS` says. It looks at today's and yesterday's journeys
  with a train leg leaving inside the live window (at most
  `maxJourneysPerTick`), opens, updates or ends each break, and plans its
  suggestions through its own planning slot (never a user's).
- **The notifier** (ds-workers' `crates/notifier/src/break_notices.rs`) reads open
  breaks with suggestions every `notifier.journeyBreaksPollIntervalSecs`
  (15) and pushes one notice per break, again only when it gets worse. A
  notice is that train's push: the train's own delay, cancel or predicted
  late start push stays quiet for it.

## The kill switch

`api.journeyBreaks.enabled` (default `false`) turns both the sweep and the
notices on or off. Off, the in-app parts still work: the banner on a
journey page, "I missed this train", the alternatives sheet, accept and
Undo. Turning it off stops pushes at the next notifier restart; open breaks
stay until they are acted on, recover or expire (04:00 after the journey's
last service date).

## Opt-outs

- Per user: `notification_preferences.journey_suggestions`
  (`/account/preferences`, "Suggest other trains when a journey breaks").
- Per journey: `journeys.suggestions_muted` ("Stop suggestions for this
  journey" in the sheet).

Either stops the push only; the train's own push then goes out as before.

A dismissed break stays dismissed: while it is the journey's latest
break and the leg has not recovered since, neither a page read nor the
sweep opens a new one for the same cause on the same leg (so it is never
pushed twice). The fingerprint is not compared: it follows the live times,
which move every few minutes. When a page read or the sweep finds the
journey working again (no break detected), it stamps `cleared_at` on the
dismissed break; after that, the same cause on the same leg going wrong
again opens a new break (and may push). A different cause or a different
leg opens a new break at once, and once any new break exists the dismissal
holds nothing back. `cleared_at` needs ds-core's
`20261117110000_journey_breaks_cleared_at.sql` (api after migration).
Nothing is stamped when the journey cannot be judged (no live timings) or
is all travelled: unknown is not healthy.

## "I missed this train"

The traveller's own word that they did not get on a train. It opens no
break; it shows alternatives (`GET /Journeys/{id}/legs/{legId}/alternatives?cause=missed`),
and taking one (`POST .../replace` with `cause: missed`) records the leg as
missed in a revision of its own (reason `replace`, cause `user_missed`,
fault from the data, never from the tap).

**What "travelled" means.** Two facts that used to be one:

- *The train arrived* (`trainArrived`, `LegArrival::arrived`): the train
  reached the leg's alighting station, by `travel_order::train_arrived_sql`
  (TRUST's reported arrival, else the timetabled arrival plus the live delay,
  else plus 30 minutes). It orders the lists, decides a journey is past and
  freezes "Choose another train". A cancelled train is never counted as
  having arrived while the miss window below is open.
- *The miss window is open* (`journey_breaks::miss_window_open`): the leg's
  service date has not yet ended at 04:00 London the next morning (the rule
  that expires breaks). It is the only guard for the causes `missed`,
  `cancelled` and `connection`, whatever the train did. A missed train very
  often runs on to its destination without the traveller, so DS data never
  blocks the request: the traveller is the authority on whether they boarded.

| Request | Refused when | Answer |
|---|---|---|
| alternatives or replace, cause `missed`, `cancelled`, `connection`, or with a break open on the leg | the first leg's window has closed | `409 miss_window_closed` ("It's too late to change this journey.") |
| alternatives or replace, cause `choice` (no break open) | a replaced leg's train has arrived | `409 leg_travelled` ("This leg is already travelled.") |
| revert, repeat, a regular journey's plan | a replaced leg's train has arrived | `409 leg_travelled` |

A replace over several legs (`throughLegId`, or an accepted break's range)
under the window is judged by its first leg only: later legs of the range
that have already run are replaced too and are `not_used` in the earlier
plan, where the whole replace used to be refused.

Where the browser sees the message only, a delegated client sees the code
as well.

The window is judged before anything else about the request: a closed
window is a `409` whatever `from` says (a code that is not a station
included), and a break still open on the leg turns even a free choice
(`cause=choice`, or none) into a window judgement, so it answers
`miss_window_closed`, not `leg_travelled`. The window closes at 04:00
London exactly (03:59 is open).

A replace sent with an `Idempotency-Key` keeps the opaque ids of the legs it
replaced (they are deleted with it) in the stored response, so a retry that
names them by `l_` id, by number or by one then the other is the same
request and replays the first answer. They are never served.

**Where it searches from.** By default the leg's own boarding station,
at the later of "now" and when the traveller could board (the previous
leg's live arrival plus the change time): right for someone who arrived late
at that station. `alternatives?from=<CRS>` searches from another station
instead (the traveller is elsewhere); the code must be a station DS knows
(`400 invalid_station` otherwise), the time floor and the searches to the
destination and back onto a later leg are unchanged. The set's
`trigger.originName` stays the leg's boarding station; its `originCrs` and
`originName` are where the search started.

**After midnight.** A search runs on the leg's own service date, on that
date's clock, so one made between 00:00 and 04:00 the next morning (while
the window is open) is at 1440 minutes or more. From there the alternatives
also search the next service date from the same moment and merge the two
plans (`across_midnight` on `ReplanQuery`, as `/Trips/plan?acrossMidnight=true`
plans: the next date's minutes are shifted a day on, so the three searches,
the next departures one by one and the rejoin onto a later leg, work
unchanged). A train of the next date is offered with `serviceDate` of that
date and its real instants; taking it replaces the leg with one dated the
next day, whose own miss window is 04:00 the morning after that date. The
original leg's window decides whether the request is allowed at all, so a
leg dated the next day never makes it, or the ordering of the journey,
wrong. Only a search that starts past midnight does this: before midnight the
next morning's trains are not offered. Only the alternatives' searches ask
for it: the regular journeys plan sweep never crosses midnight. With no
timetable published for the next date it adds nothing.
`distant_signal_api_replan_across_midnight_total` counts the searches that
loaded the next date's graph. Today's graph and tomorrow's are prewarmed
(`prewarm_graphs`), so after midnight the next date is today's and warm;
the date before it is the one that may need building on the first search.

**What the app can show.** The set carries `leg: {departed, arrived}`:
whether the train has left the boarding station (its live departure has
passed) and whether it ran to the alighting station (never for a cancelled
train). They are facts for a soft confirmation ("This train has already
reached X. Only use this if you didn't get on it."), never a refusal. `leg`
is `null` on a break's stored suggestions.

**Undo.** The just-replaced plan can be gone back to for 2 minutes
(`canRevert` on `GET /Journeys/{id}/revisions`, `POST .../revert`). After an
"I missed this train" (revision cause `user_missed`) it stays possible until
the replacement's first new train leaves, but at most an hour after the
change: the mistake ("I didn't actually miss it") is noticed late. A revert
still replaces the replacement under the arrival freeze, so it is refused
once the replacement has run, and it writes a new revision (reason
`revert`), never rewriting history. Any other revision's rule is unchanged:
an earlier plan can be gone back to while its trains are all still to leave.

**The journey afterwards.** Nothing is stored about the miss itself: a
journey is Past or Upcoming from its current legs. So a Past journey whose
last leg is replaced by a train still to come is Upcoming again, and goes
Past once the replacement arrives. A journey whose legs are all dated before
the window is still frozen. If the traveller takes no other train, nothing
changes: the leg stays as booked (see below).

**Integrity.** Nothing outside DS is touched: Delay Repay is advisory and DS
submits no claim; the booked plan is revision 1, which is append-only apart
from the fate stamp on the leg. The data, not the request, decides fault
(`missed_fault`: the railway's only when the change onto the leg already
broke on live times), so a late miss cannot make a claim. A user-fault miss
moves the compensation baseline to the replacement plan, as any
passenger-fault change does; the booked plan is still stored. The miss is
counted as `api_journey_missed_total{state}` for audit, labelled by what the
train had done when the traveller said so: `before_departure`, `departed`
or `arrived`. DS does not remember a miss when no other train is taken (no
leg-level missed record): the leg stays as booked.

## Metrics

| Metric | Labels |
|---|---|
| `api_journey_breaks_detected_total` | `cause`, `fault` |
| `api_journey_breaks_resolved_total` | `status` |
| `api_journey_break_suggestions_seconds` | `trigger` (`request`, `sweep`) |
| `api_journey_break_suggestions_total` | `result` (`found`, `none`, `error`) |
| `api_journey_break_opened_total` | `via` (`push`, `in_app`) |
| `api_journey_replan_accepted_total` | `via`, `rank`, `pick`, `fit` |
| `api_journey_missed_total` | `state` (`before_departure`, `departed`, `arrived`) |
| `api_journey_replan_stale_total` | |
| `api_journey_breaks_sweep_seconds` | |
| `notifier_journey_break_notice_total` | `result` (`sent`, `opt_out`, `muted`, `not_worse`, `acted`, `unconfirmed`, `in_app`) |

## The funnel

Aggregates only; no per-user data leaves the database.

```sql
SELECT date_trunc('week', detected_at) AS week,
       count(*) AS breaks,
       count(*) FILTER (WHERE notified_at IS NOT NULL) AS notified,
       count(*) FILTER (WHERE notified_at IS NOT NULL AND opened_via = 'push') AS opened_from_push,
       count(*) FILTER (WHERE opened_at IS NOT NULL) AS opened,
       count(*) FILTER (WHERE status = 'accepted') AS accepted,
       percentile_cont(0.5) WITHIN GROUP (ORDER BY resolved_at - detected_at)
           FILTER (WHERE status = 'accepted') AS median_time_to_accept,
       count(*) FILTER (WHERE status = 'dismissed') AS dismissed,
       count(*) FILTER (WHERE status = 'recovered') AS recovered
FROM journey_breaks
GROUP BY 1 ORDER BY 1 DESC;

SELECT dismiss_reason, count(*) FROM journey_breaks
WHERE status = 'dismissed' GROUP BY 1 ORDER BY 2 DESC;
```

Success for the first month: at least half of notified breaks opened, and
at least a third of opened breaks accepted.
