# Actual, estimated and booked times

Which time fields on the train, journey and Delay Repay responses are
recorded actuals, which are estimates and which are the booked timetable, and
the fields that say so. For a client such as DS-MCP that has to tell "the
train left at 09:07" from "the train is expected to leave at 09:07".

Source: TRUST movement reports (`train_movement_events`) are the only source
of actual times. TRUST reports only at timing points, so a station that is
not one never has an actual: its actual is `null`, never a guess. Field names
are as served (camelCase). The changelog (`api-changelog.md`) dates each
addition.

## `journeyStops[]` (train and journey leg responses)

One entry per calling point. Per stop, the overlay applies ONE winning TRUST
report for the stop's visit (the latest received): an ARRIVAL, a DEPARTURE or
a PASS.

| Field | What it is |
|---|---|
| `timetable.publicArrival`, `publicDeparture` | Booked, public timetable (what a passenger is sold). |
| `timetable.workingArrival`, `workingDeparture`, `workingPass` | Booked, working timetable. |
| `timetable.scheduledArrival`, `scheduledDeparture` | Booked: the public time, else the working time to the minute. |
| `actualArrival`, `actualDeparture` | Recorded: set from the winning TRUST report. An ARRIVAL sets `actualArrival` only, a DEPARTURE `actualDeparture` only, a PASS both (except at a booked calling point, where a PASS means the train ran through and sets neither). `null` otherwise. |
| `actualArrivalIsActual`, `actualDepartureIsActual` | `true` exactly when the matching `actual*` was set from a TRUST report (a PASS's time included); `false` when it is `null`. Always present. `false` means "no recorded actual", never "on time" and never "estimated". |
| `estimatedArrival`, `estimatedDeparture` | Estimated: the working time plus the train's running delay (see `estimateSource`). Only set while the matching `actual*` is `null`; never alongside an actual. |
| `estimateSource` | Where the estimates came from, see below. |
| `delayMinutes`, `delayBasis` | Measured on the winning report's side (an arrival against the public arrival, a departure against the public departure); `null` until the train reports at the stop. `delayBasis` below. |
| `lastEventType`, `variationStatus` | The winning report's type (`ARRIVAL`, `DEPARTURE`, `PASS`) and TRUST's own on time/late/early word. |

`estimateSource` has exactly two values. Its only emitter is
`apply_predicted_start_estimates` (`crates/api/src/data/journey.rs`):

- `"previous-working"`: the estimates are a predicted late start, propagated
  from the incoming train that forms this one (DS-127), while the origin has
  no live time of its own and the train has no TRUST report yet. A prediction
  about a train that has not started.
- `null`: the estimates (if any) come from the train's own TRUST running
  delay, or there is none. `null` does not mean "actual".

## Delay fields

`delayBasis` (also `boardingDelayBasis`) says which baseline a delay was
measured against, in this order:

- `public`: TRUST's own public time for the movement (`gbtt_timestamp`).
- `publicSchedule`: the public time from the CIF schedule at that call, when
  TRUST sent none (TRUST's working time moved by the schedule's public minus
  working gap).
- `working`: the working timetable, when the call has no public time in that
  direction (a pass, or the departure of a set-down-only stop) or there is no
  schedule row.

`delayMinutes` is whole minutes truncated toward zero, positive is late.
`delayProvisional` (train responses) and `provisional` (Delay Repay) are
`true` while the delay is a forecast for a stop the train has not reported at
yet, `false` once it is measured.

## Train responses (`/Train/...`, a leg's `trackedTrainState`)

- `delayMinutes`, `delayBasis`, `delayProvisional`: the delay at the
  passenger's own stop (the pin destination), measured once the train has
  reported there and forecast before then (`delayProvisional: true`).
- `status`, `lastReportedLocation`, `lastEventType`: the train's latest
  report, a fact.
- `etaNext`, `etaSource`: an estimate (`etaSource` names its origin).

## Legs and journeys

- `legs[].delayRepay` (`GET /Journeys/{id}`): `delayMinutes`, `delayBasis` and
  `provisional` are measured at the leg's destination (`measuredAtCrs`),
  projected while `provisional`. `outcome` (`arrived`, `departedOnly`,
  `notReached`) says whether the train got there.
- Boarding end of a leg, on `legs[].delayRepay` (DS-638), always present,
  `null` when unknown:
  - `boardingCrs`: where the passenger boards.
  - `boardingActualDeparture`: when the train ACTUALLY left there, from a
    TRUST DEPARTURE report. `null` when none was reported (never a booked time
    or an estimate), for a bus or ferry, and once the train data is pruned.
  - `boardingDelayMinutes`, `boardingDelayBasis`: that departure against the
    booked public departure; `null` when either is missing. The basis is judged
    at the boarding station, so it can differ from `delayBasis`.
- `GET /Journeys/reliability` `journeys[].actualArrival`: derived (the booked
  arrival plus the measured delay), not a time TRUST reported.

## Trip plans

`POST /Trips/plan` `legs[].live`: `arrivalIsActual` is present (and `true`)
only when the alighting arrival is an actual; it is absent, not `false`,
otherwise (docs/trip-plan-live.md). The journey stop flags above are the
other convention: always present, `false` when there is no recorded actual.
