// Generated from the api's Rust types by ts-rs (crates/api/src/api_types.rs).
// Do not edit: regenerate with `UPDATE_API_TYPES=1 cargo test -p api api_types`.

/**
 * How a candidate was found.
 */
export type AccessBasis = "hub" | "line" | "walk" | "nearest";

/**
 * One National Rail station a stop can be left for (or reached from).
 */
export type AccessCandidate = { crs: string, 
/**
 * `TUBE`, `METRO` or `WALK`.
 */
mode: string, 
/**
 * The estimate (see the module doc).
 */
minutes: number, basis: AccessBasis, 
/**
 * The line ridden (`Metropolitan`), for a ride.
 */
line?: string, 
/**
 * The stop at the station's end of a ride (`Harrow-on-the-Hill`).
 */
stationStop?: TflStopSummary, 
/**
 * Along the line for a ride, in a straight line for a walk.
 */
distanceMetres: number, 
/**
 * Stops ridden to the far end, for a ride measured along the line's
 * routes (absent for a spanning-tree estimate or a walk).
 */
stopCount?: number, 
/**
 * Where a ride changes trains between two branches of its line
 * (`Harrow-on-the-Hill`); its `minutes` include the change.
 */
changeAt?: TflStopSummary, 
/**
 * Where the ride is boarded, when that is not the plan's own stop but
 * another stop area of its station ([`with_siblings`]: Paddington
 * (H&C Line) for Paddington); `minutes` include the walk to it.
 */
boardStop?: TflStopSummary, 
/**
 * Where the ride is left, when that is not the plan's own stop but
 * another stop area of its station; `minutes` include the walk from it.
 */
alightStop?: TflStopSummary, };

/**
 * On a `/Trips/plan` transfer leg that is an access or egress leg: which
 * end, how it was found, and how far it goes. Its `minutes` are an
 * estimate.
 */
export type AccessLeg = { end: AccessLegEnd, basis: AccessBasis, distanceMetres: number, 
/**
 * See [`AccessCandidate::change_at`].
 */
changeAt?: TflStopSummary, 
/**
 * See [`AccessCandidate::board_stop`].
 */
boardStop?: TflStopSummary, 
/**
 * See [`AccessCandidate::alight_stop`].
 */
alightStop?: TflStopSummary, };

/**
 * Which end of the journey an access leg is at.
 */
export type AccessLegEnd = "origin" | "destination" | "both";

/**
 * `GET /Trips/plan`'s top-level `access`: each stop end with its
 * candidates, `null` for a station end.
 */
export type AccessResponse = { origin: StopAccess | null, destination: StopAccess | null, };

/**
 * One affected route of a [`DisruptionJson`], by CRS.
 */
export type AffectedRoute = { from: string, to: string, };

export type AttachTicketResponse = { ticketId: number, trackedTrainId: number, };

/**
 * `journey_templates.auto_commit_rule` (its CHECK constraint).
 */
export type AutoCommitRule = "earliest" | "nearest_to_now";

/**
 * One `callingPoints` entry of [`StationDepartureJson`]. `st`/`et`/`at`
 * are LDBWS's own strings: `"HH:MM"` London local time, or for `et` a
 * status word ("On time", "Delayed", "Cancelled").
 */
export type BoardCallingPoint = { crs: string, locationName: string | null, st: string | null, et: string | null, at: string | null, isCancelled: boolean, };

/**
 * `journey_breaks.cause` (its CHECK constraint).
 */
export type BreakCause = "cancelled" | "part_cancelled" | "connection_missed" | "user_missed" | "skipped_stop";

/**
 * `journey_breaks.fault` (its CHECK constraint).
 */
export type BreakFault = "operator" | "user" | "unknown";

/**
 * `journey_breaks.opened_via` (its CHECK constraint).
 */
export type BreakOpenedVia = "push" | "in_app";

/**
 * A break with its suggestions, as the break routes answer.
 */
export type BreakResponse = { id: number, journeyId: number, cause: BreakCause, fault: BreakFault, severity: number, status: BreakStatus, triggerLegId: number | null, detectedAt: string, openedVia: BreakOpenedVia | null, 
/**
 * Why the fault is what it is, when the data overrode the traveller
 * (§14 Q5): "Your earlier train was late, so this counts as a missed
 * connection."
 */
faultNote: string | null, suggestions: SuggestionSet | null, stale: boolean, };

/**
 * `journey_breaks.status` (its CHECK constraint).
 */
export type BreakStatus = "open" | "accepted" | "dismissed" | "recovered" | "expired";

/**
 * A schedule calling point's `kind`, as `schedule-query` writes it.
 */
export type CallingPointKind = "Origin" | "Intermediate" | "Terminate";

export type Cancellation = { kind: CancellationKind, 
/**
 * The first call still served, when the train starts short of its
 * booked origin (a change of origin, or cancelled at the start).
 */
startsFrom: CancellationStop | null, 
/**
 * The last call still served, when the train terminates short of its
 * booked destination.
 */
terminatesAt: CancellationStop | null, 
/**
 * The only section's `from`, or `null` with several.
 */
from: CancellationStop | null, 
/**
 * The only section's `to`, or `null` with several.
 */
to: CancellationStop | null, 
/**
 * In route order; empty when the train has no calling points.
 */
sections: Array<CancelledSection>, reason: CancellationReason | null, 
/**
 * When the cancellation (else the change of origin) was made.
 */
at: string | null, };

/**
 * Whether every public call is cancelled.
 */
export type CancellationKind = "full" | "partial";

/**
 * A place a [`CancellationSummary`] names. No `stopIndex`: a list row
 * carries no stops to index.
 */
export type CancellationPlace = { crs: string | null, name: string | null, };

export type CancellationReason = { 
/**
 * The source's own code: a TRUST delay attribution code (`"JP"`), or
 * `null` for LDBWS, which serves text only.
 */
code: string | null, 
/**
 * Passenger-readable text: Darwin's own sentence for `ldbws`, the
 * glossary description for `trust`. `null` for a TRUST code with no
 * text (`PD`, `ZW`, or one missing from the 2021 glossary).
 */
text: string | null, source: ReasonSource, };

/**
 * A call named by a cancellation.
 */
export type CancellationStop = { crs: string | null, name: string | null, 
/**
 * Index into the response's `journeyStops`.
 */
stopIndex: number, };

/**
 * The compact form of a [`Cancellation`] for rows that carry no stops
 * (`GET /Journeys/mine`, `GET /Train/mine`) and for push copy: enough to
 * say "Terminates at York" or "Starts from York" rather than plain
 * "Cancelled", and why.
 */
export type CancellationSummary = { kind: CancellationKind, startsFrom: CancellationPlace | null, terminatesAt: CancellationPlace | null, reason: CancellationReason | null, };

/**
 * A part of the route the train does not run: the calls strictly between
 * `from` and `to` are cancelled, and so is `from` at the origin end or `to`
 * at the destination end.
 */
export type CancelledSection = { from: CancellationStop, to: CancellationStop, };

/**
 * How sure the prediction is. Ordered: `Low < Medium < High`.
 */
export type Confidence = "low" | "medium" | "high";

/**
 * How one change between two consecutive legs stands.
 */
export type ConnectionState = "ok" | "atRisk" | "missed" | "unknown";

export type CreateJourneyTemplateResponse = { templateId: number, };

/**
 * Full custom-line record, returned by `GET /lines/{id}` to pre-populate
 * an edit form. `LineSummary` (above) is deliberately a smaller
 * projection used by the list endpoint for both catalogue and custom
 * lines — it lacks `stations`/`headcodePrefixes`/`destinationCrsFilter`,
 * which only exist for custom lines and are exactly what an edit form
 * needs to pre-fill.
 *
 * `isOwner` exists again, and this is load-bearing. The 2026-08-31
 * privacy hardening removed it on the (then true) grounds that "a `200`
 * from this endpoint is by construction always the real owner's own
 * line." Custom-line group sharing
 * (docs/superpowers/specs/2026-09-12-custom-line-group-sharing-design.md)
 * made that false: a granted group member now gets a `200` here too, with
 * the same full detail (§3.5 -- full detail or nothing; a custom line has
 * no private per-viewer overlay to withhold). `frontend/app/lines/[id]/page.tsx`
 * had come to use "did `getCustomLine` succeed" as its ownership gate for
 * the Edit/Delete controls, so without this flag a granted member would
 * be shown mutation controls for someone else's line whose only possible
 * outcome is a `404`. The backend is still the authority
 * (`update_line`/`delete_line` are completely grant-blind and unchanged);
 * this is the "never render a control that can only fail" half.
 *
 * `sharedWithGroups` is populated ONLY when `isOwner` is true, and is
 * `[]` for every other caller -- that, not a query-level scope, is what
 * keeps a fellow group member from learning which OTHER groups the owner
 * shared this line into (design §3.5). For the owner it lists every group
 * the line is currently granted into, including one they have since left
 * (which keeps its grant, per §2.7) -- hiding a live grant from the
 * person entitled to revoke it would be the real privacy failure. See
 * `groups::groups_shared_with_line`.
 */
export type CustomLineDetail = { id: string, name: string, operators: Array<string>, stations: Array<string>, headcodePrefixes: Array<string>, destinationCrsFilter: Array<string>, isOwner: boolean, sharedWithGroups: Array<LineGroupRef>, };

/**
 * One day of a line's full-coverage stats.
 */
export type DailyCoverageStats = { day: string, resolvedWindows: number, 
/**
 * The credits the line's data needs (`render::line_attribution`): on
 * the line routes, absent from the operator and network ones.
 */
attribution?: Array<TransitAttribution>, total: number, delayed: number, cancelled: number, skipped: number, avgDelayMinutes: number, delayRate: number, cancellationRate: number, skipRate: number, };

/**
 * One day of a line's (or operator's, or the network's) sampled stats.
 */
export type DailyStats = { day: string, sampleCycles: number, 
/**
 * The credits the line's data needs (`render::line_attribution`): on
 * the line routes, absent from the operator and network ones.
 */
attribution?: Array<TransitAttribution>, total: number, delayed: number, cancelled: number, skipped: number, avgDelayMinutes: number, delayRate: number, cancellationRate: number, skipRate: number, };

/**
 * How confident are we in this status?
 */
export type DataQuality = "knowledgebase" | "ldbws-inferred" | "trust-inferred" | "planned" | "tfl" | "transit";

/**
 * Which baseline a delay was measured against. See the module doc.
 */
export type DelayBasis = "public" | "publicSchedule" | "working";

/**
 * A rough eligibility estimate for a Delay Repay claim -- never a
 * guarantee, never proof of travel, never a claim itself. `disclaimer` is
 * intentionally NOT optional: every estimate carries its own caveat text
 * baked in, so a caller serializing this type cannot accidentally display
 * a bare percentage with no caveat attached.
 *
 * The delay it is computed from is the train's delay against the PUBLIC
 * arrival at the ticket's destination (design doc §9 decision 3), which
 * is what operators pay Delay Repay on. Before the train has arrived there
 * that delay is a projection, and the estimate says so: `provisional` is
 * `true` and `disclaimer` is [`PROVISIONAL_DISCLAIMER`]. It becomes final
 * (`provisional: false`, [`DISCLAIMER`]) once the train has arrived.
 */
export type DelayRepayEstimate = { 
/**
 * `DR15`, `DR30` or `HX` (Heathrow Express's own scheme).
 */
scheme: DelayRepayScheme, 
/**
 * The band the delay fell into: 15, 30, 60 or 120 (`HX`: 30 or 60,
 * meaning "more than").
 */
bandMinutes: number, 
/**
 * Rough percentage of `fare_basis`. `None` exactly when `borderline`:
 * the band could go either way, so no figure is shown.
 */
percentage: number | null, 
/**
 * Which fare `percentage` is of: `single` (the single fare, or half a
 * return) below 120 minutes and for a single at 120+; `return` (the
 * whole return fare) at 120+ for a return or an unknown ticket (see
 * `ticket_kind`); `ticket` (the ticket price) for Heathrow Express.
 */
fareBasis: FareBasis, 
/**
 * What the ticket's type says it is; the 120-minute band's wording
 * depends on it.
 */
ticketKind: TicketKind, 
/**
 * Caledonian Sleeper only: the percentage of the room supplement
 * (50 at 30-59 minutes, 100 at 60+). `None` otherwise, and when
 * `borderline`.
 */
roomSupplementPercentage: number | null, 
/**
 * `true` while provisional and the projection is less than
 * [`BORDERLINE_MINUTES`] above `threshold_minutes`.
 */
borderline: boolean, 
/**
 * The band threshold a `borderline` projection is just above; `None`
 * when not borderline.
 */
thresholdMinutes: number | null, 
/**
 * `true` while the train has not reached the ticket's destination yet.
 */
provisional: boolean, disclaimer: string, };

export type DelayRepayEstimateResponse = { 
/**
 * Why there is no delay at all, when that is known up front: a bus or
 * ferry is never reported live, so its delay cannot be measured.
 * `None` for a train.
 */
unmeasurableReason: string | null, 
/**
 * The delay against the PUBLIC arrival at `measured_at_crs` (design
 * doc §9 decision 3): measured once the train has arrived (or, at a
 * departure-only station, departed), projected before then
 * (`provisional`). `None` when `outcome` is `notReached`.
 */
delayMinutes: number | null, 
/**
 * `true` while `delay_minutes` (and `estimate`) are a projection.
 */
provisional: boolean, 
/**
 * See `common::public_delay::DelayBasis`; `None` exactly when
 * `delay_minutes` is.
 */
delayBasis: DelayBasis | null, 
/**
 * Where the delay was measured: the ticket's destination, else the
 * pin destination, else the terminus. Set when `delay_minutes` is, and
 * when `outcome` is `notReached` (the destination not reached).
 */
measuredAtCrs: string | null, 
/**
 * `measured_at_crs`'s station name, when known.
 */
measuredAtName: string | null, 
/**
 * See [`Outcome`]; `None` while not known yet.
 */
outcome: Outcome | null, 
/**
 * `true` for an operator running its own compensation scheme: no
 * percentage, just the claim link.
 */
ownScheme: boolean, 
/**
 * The scheme's operator, when it is one in [`OPERATORS`] (else `None`:
 * the ticket's own `operator` text is the best name).
 */
schemeOperator: string | null, estimate: DelayRepayEstimate | null, 
/**
 * Always populated, independent of `estimate`: no caller is ever left
 * with nowhere real to go.
 */
claimUrl: string, 
/**
 * Always populated, independent of `estimate`.
 */
disclaimer: string, 
/**
 * [`RULES_CHECKED_ON`].
 */
rulesCheckedOn: string, serviceMode: ServiceMode, liveTracking: boolean, };

/**
 * The Delay Repay fields both `GET .../delay-repay` and
 * `GET /Train/tickets/mine` serve (flattened into each), so the two can
 * never disagree.
 */
export type DelayRepayFields = { 
/**
 * The delay against the PUBLIC arrival at `measured_at_crs` (design
 * doc §9 decision 3): measured once the train has arrived (or, at a
 * departure-only station, departed), projected before then
 * (`provisional`). `None` when `outcome` is `notReached`.
 */
delayMinutes: number | null, 
/**
 * `true` while `delay_minutes` (and `estimate`) are a projection.
 */
provisional: boolean, 
/**
 * See `common::public_delay::DelayBasis`; `None` exactly when
 * `delay_minutes` is.
 */
delayBasis: DelayBasis | null, 
/**
 * Where the delay was measured: the ticket's destination, else the
 * pin destination, else the terminus. Set when `delay_minutes` is, and
 * when `outcome` is `notReached` (the destination not reached).
 */
measuredAtCrs: string | null, 
/**
 * `measured_at_crs`'s station name, when known.
 */
measuredAtName: string | null, 
/**
 * See [`Outcome`]; `None` while not known yet.
 */
outcome: Outcome | null, 
/**
 * `true` for an operator running its own compensation scheme: no
 * percentage, just the claim link.
 */
ownScheme: boolean, 
/**
 * The scheme's operator, when it is one in [`OPERATORS`] (else `None`:
 * the ticket's own `operator` text is the best name).
 */
schemeOperator: string | null, estimate: DelayRepayEstimate | null, 
/**
 * Always populated, independent of `estimate`: no caller is ever left
 * with nowhere real to go.
 */
claimUrl: string, 
/**
 * Always populated, independent of `estimate`.
 */
disclaimer: string, 
/**
 * [`RULES_CHECKED_ON`].
 */
rulesCheckedOn: string, };

/**
 * A Delay Repay estimate's `scheme`: `delay_repay_rules::Scheme::as_str`
 * of every scheme with bands (an own scheme has no estimate).
 */
export type DelayRepayScheme = "DR15" | "DR30" | "HX";

/**
 * A status's disruption detail.
 */
export type Disruption = { category: string, description: string, affectedStops: Array<string>, affectedRoutes: Array<AffectedRoute>, source: string | null, impactType: string | null, };

/**
 * Where an estimate came from: `train_current_state.eta_source` (its
 * CHECK constraint) and `ds_store::trains::predicted_start::BASIS`.
 */
export type EtaSource = "trust-propagated" | "darwin-estimated" | "previous-working";

/**
 * A Delay Repay estimate's `fareBasis`: `delay_repay_rules::estimate_delay_repay`.
 */
export type FareBasis = "single" | "return" | "ticket";

/**
 * How well one alternative fits the ticket (§17.3, step 3).
 */
export type Fit = "covered" | "coveredIfCancelled" | "check" | "likelyNewTicket";

/**
 * How a follow reached the journey.
 */
export type FollowVia = "group" | "link";

/**
 * Who shared a followed journey and how, for the "Shared by ..." label.
 * Only what the share already shows: a group member sees the sharer's
 * name in the group, so a group follow carries it (by the same rule as
 * every other member label, `users::MemberDisplay`) and the group's name;
 * a share link shows nothing about its owner, so a link follow carries
 * neither.
 */
export type FollowedFrom = { via: FollowVia, 
/**
 * The owner's display name; `None` for a link, or when no name is on
 * file (then `owner_tag` is set, as `GroupJourney.addedByTag`).
 */
ownerName: string | null, ownerTag: string | null, 
/**
 * The group the journey was followed through; `None` for a link.
 */
groupId: string | null, groupName: string | null, followedAt: string, };

/**
 * Why `fullCoverageStats` is (or isn't) there:
 * `common::FullCoverageAvailability` without its stats.
 */
export type FullCoverageAvailability = { "state": "not-enabled" } | { "state": "pending" } | { "state": "available" };

/**
 * A custom line granted into a group, as `GET /groups/{id}/lines/custom`
 * returns it.
 *
 * Deliberately carries only the line's identity and its attribution --
 * no stations, operators, headcode filters or status. Not because those
 * are secret from a granted member (they explicitly are not, design
 * §3.5: full detail or nothing), but because the group page reads them
 * through the ordinary `/lines/{id}` and `GET /Line/{ids}/Status` routes
 * this feature already widened, rather than duplicating a second,
 * parallel status-rendering path into the groups module that would then
 * have to be kept in sync with the first.
 */
export type GroupCustomLine = { lineId: string, lineName: string, grantedBy: string, grantedByName: string | null, 
/**
 * Same contract, same derivation, as `GroupTrain.added_by_tag`: set
 * only when there is no showable name, so a group whose `IdP` can name
 * nobody still tells its members' shares apart.
 */
grantedByTag: string | null, };

export type GroupDetailResponse = { id: string, name: string, ownerId: string, ownerName: string | null, 
/**
 * Present only when `owner_name` is absent -- the distinguishing
 * suffix for the generic placeholder; see
 * `groups::GroupMember.display_tag`.
 */
ownerTag: string | null, memberCount: number, role: GroupRole, inviteLink: InviteLink | null, };

export type GroupIdentityResponse = { id: string, name: string, };

/**
 * A shared journey's display shape for `GET /groups/{id}/journeys` --
 * `GroupTrain`'s direct analogue, one level up. Carries the FIRST leg's
 * identity/live-status fields (Judgment Call 2 in this feature's plan) so
 * the group page can render something useful without duplicating a
 * worst-status-across-legs rollup that belongs to the journey's own
 * detail page instead. `legCount` lets the row at least signal "there's
 * more" for a multi-leg journey once Phase 2 ships.
 *
 * Same hard privacy constraint `GroupTrain` documents: no ticket field,
 * no `notificationsEnabled`/exact `addedAt` -- a shared journey's group
 * row must never widen past what `group_trains` already decided was safe
 * to show a fellow member for the underlying tracked-train resource.
 */
export type GroupJourney = { journeyId: number, customName: string | null, legCount: number, pinOriginCrs: string | null, pinDestinationCrs: string | null, pinOriginName: string | null, pinDestinationName: string | null, pinScheduledDeparture: string | null, 
/**
 * The journey's own ends, for naming it: the first leg's planned
 * origin and the LAST leg's planned destination (`journey_legs`, not
 * the first leg's train pin above, which is empty until that leg is
 * matched and names only that leg). Names via `stations`, `None` when
 * unresolved. Additive (2026-10-20).
 */
firstOriginCrs: string | null, firstOriginName: string | null, lastDestinationCrs: string | null, lastDestinationName: string | null, serviceDate: string, resolutionStatus: ResolutionStatus | null, trainUid: string | null, status: JourneyStatus | null, 
/**
 * The delay at the sharer's own stop (their pin destination), against
 * the public timetable; see `TrackedTrainState::delay_minutes`.
 */
delayMinutes: number | null, 
/**
 * See `TrackedTrainState::delay_basis`.
 */
delayBasis: DelayBasis | null, 
/**
 * See `TrackedTrainState::delay_provisional`.
 */
delayProvisional: boolean, addedBy: string, addedByName: string | null, 
/**
 * Same contract as `GroupTrain.added_by_tag`.
 */
addedByTag: string | null, 
/**
 * The summarised leg's `trackingPhase` (see
 * `TrackedTrainState::tracking_phase`); `None` for an open or
 * untracked leg. Additive (2026-10-20).
 */
trackingPhase: TrackingPhase | null, 
/**
 * See `TrackedTrainState::first_report_expected_at`.
 */
firstReportExpectedAt: string | null, 
/**
 * The summarised leg's kind: `train` or `untracked`. The row
 * summarises the journey's first train leg, else its first leg, so
 * `untracked` means the journey has no train legs. Additive
 * (2026-10-20).
 */
kind: JourneyLegKind, 
/**
 * The untracked leg's mode (`tube`, `walk`, ...); `None` on a train
 * leg.
 */
transitMode: TransitMode | null, 
/**
 * The untracked leg's planned times; `None` on a train leg.
 */
plannedDeparture: string | null, plannedArrival: string | null, 
/**
 * The untracked leg's clock-only state; `None` on a train leg.
 */
untrackedStatus: UntrackedStatus | null, 
/**
 * Whether the CALLER follows this journey (DS-146,
 * `journey_follows`); set by the route, `false` here.
 */
following: boolean, serviceMode?: ServiceMode, liveTracking?: boolean, };

/**
 * Deliberately no `email` field, and `display_name` is deliberately never
 * an email either: `GET /groups/{id}/members` returns this to every
 * current member, and a joined-via-link member has no other relationship
 * with the rest of the group -- shipping their verified email to everyone
 * (as a field of its own, OR quietly as the `displayName` fallback for a
 * member whose `IdP` sent no name) leaks more than the feature needs. The
 * fallback is their `username` instead. `None` means "neither on file",
 * which the frontend renders as its own generic "A member" placeholder.
 * Same rule, same helper (`users::MemberDisplay`), as
 * `GroupTrain.added_by_name` one struct away.
 */
export type GroupMember = { userId: string, displayName: string | null, 
/**
 * Set when -- and only when -- `display_name` is `None`: six hex
 * characters the frontend appends to its own placeholder, so a group
 * whose `IdP` gives this app no showable name for anybody (Entra ID,
 * where `preferred_username` is the email-shaped UPN) renders "A
 * member (#a1b2c3)" per member instead of the same "A member" on
 * every single row. Derived from `user_id`, which this struct already
 * carries in the clear, and never from an email -- see
 * `users::MemberDisplay`.
 */
displayTag: string | null, role: GroupRole, joinedAt: string, };

/**
 * The three-tier role stored in `group_members.role` -- see the design
 * doc's §2.1 for why there are three tiers, not two: the creator is a
 * PERMANENT `owner`, distinct from a promotable `admin`, so an `admin`
 * can never remove/demote/act on the `owner` row itself.
 */
export type GroupRole = "owner" | "admin" | "member";

export type GroupSummary = { id: string, name: string, role: GroupRole, memberCount: number, };

/**
 * A shared train's display shape for `GET /groups/{id}/trains`. Carries
 * exactly the fields `frontend/lib/trackingName.ts`'s
 * `trackedTrainDisplayName` needs to compute the tracker's default name
 * the same way the tracker themselves would see it (spec §4: "never
 * stored, always computed"), plus live status and attribution.
 *
 * Deliberately carries NO ticket field, and NO `notificationsEnabled`/
 * exact `trackedAt` field -- spec §4's "Never shown" list. This is a
 * hard constraint: no future edit to this struct or to `list_group_trains`'s
 * query may join `tracked_train_tickets` or select
 * `train_subscriptions.notifications_enabled`/`tracked_at`.
 */
export type GroupTrain = { trainSubscriptionId: number, pinOriginCrs: string | null, pinDestinationCrs: string | null, pinOriginName: string | null, pinDestinationName: string | null, pinScheduledDeparture: string | null, serviceDate: string, resolutionStatus: ResolutionStatus, trainUid: string | null, status: JourneyStatus | null, 
/**
 * The delay at the sharer's own stop (their pin destination), against
 * the public timetable; see `TrackedTrainState::delay_minutes`.
 */
delayMinutes: number | null, 
/**
 * See `TrackedTrainState::delay_basis`.
 */
delayBasis: DelayBasis | null, 
/**
 * See `TrackedTrainState::delay_provisional`.
 */
delayProvisional: boolean, customName: string | null, addedBy: string, addedByName: string | null, 
/**
 * Set only when `added_by_name` is `None`, and appended by the
 * frontend to its "a member" placeholder so two sharers whose `IdP`
 * gives this app no showable name are still told apart. Same
 * contract, same derivation, as `GroupMember.display_tag`.
 */
addedByTag: string | null, 
/**
 * See `TrackedTrainState::tracking_phase`. Derived from the
 * subscription's resolution and live status and public timetable data
 * only, so it widens nothing past the "Never shown" list. Additive
 * (2026-10-20).
 */
trackingPhase: TrackingPhase | null, 
/**
 * See `TrackedTrainState::first_report_expected_at`.
 */
firstReportExpectedAt: string | null, serviceMode: ServiceMode, liveTracking: boolean, };

/**
 * One half hour of a line's full-coverage stats.
 */
export type HalfHourlyCoverageStats = { halfHourStart: string, resolvedWindows: number, 
/**
 * The credits the line's data needs (`render::line_attribution`): on
 * the line routes, absent from the operator and network ones.
 */
attribution?: Array<TransitAttribution>, total: number, delayed: number, cancelled: number, skipped: number, avgDelayMinutes: number, delayRate: number, cancellationRate: number, skipRate: number, };

/**
 * One half hour of sampled stats.
 */
export type HalfHourlyStats = { halfHourStart: string, sampleCycles: number, 
/**
 * The credits the line's data needs (`render::line_attribution`): on
 * the line routes, absent from the operator and network ones.
 */
attribution?: Array<TransitAttribution>, total: number, delayed: number, cancelled: number, skipped: number, avgDelayMinutes: number, delayRate: number, cancellationRate: number, skipRate: number, };

/**
 * Renders `serde_json::Value` field-by-field via `json!()`, exactly like
 * `crates/api/src/render.rs::status_to_json` -- deliberately NOT
 * `#[derive(Serialize)] #[serde(rename_all = "camelCase")]` on a struct
 * that embeds `validity_periods` directly, because `rename_all` is not
 * inherited into a nested type. See this plan's Status note Correction A
 * and Global Constraints for the concrete failure mode that would produce
 * (a response that's camelCase at the top level but `snake_case` inside
 * every validity period). Pure function, no I/O -- unit-testable without
 * a database, matching `to_tfl_shape`'s own testable shape in `render.rs`.
 * `GET /public/incidents/{incidentId}`. `description` is raw HTML.
 * `currentlyAffectsLines` is computed per request and may be empty.
 */
export type IncidentDetail = { incidentId: string, summary: string, description: string, operators: Array<string>, affectedStations: Array<string>, priority: number, validityPeriods: Array<ValidityPeriod>, isPlanned: boolean, isCleared: boolean, firstSeenAt: string, fetchedAt: string, sourceRemovedAt: string | null, currentlyAffectsLines: Array<IncidentLineRef>, history: Array<IncidentHistoryEntry>, state: IncidentState, startsAt: string | null, validityEndedAt: string | null, };

/**
 * One earlier version of an incident, as the feed published it.
 */
export type IncidentHistoryEntry = { summary: string, description: string, operators: Array<string>, affectedStations: Array<string>, priority: number, validityPeriods: Array<ValidityPeriod>, isPlanned: boolean, isCleared: boolean, recordedAt: string, };

/**
 * One item of `GET /public/incidents/recently-cleared`; see
 * docs/api-changelog.md (2026-10-12) for the shape. `affectedLines` names
 * catalogue lines only (all `affected_lines` can hold), so no private
 * custom line can appear.
 * A line an incident affects, by catalogue id and name.
 */
export type IncidentLineRef = { id: string, name: string, };

/**
 * `GET /public/incidents`: a page of matches, newest first.
 */
export type IncidentSearchResponse = { results: Array<IncidentSummary>, 
/**
 * Pass back as `after` for the next page; `null` on the last.
 */
nextCursor: string | null, 
/**
 * Every match across all pages; first page only, `null` with `after`.
 */
total: number | null, };

/**
 * Where an incident stands: `queries::IncidentState::as_str`.
 */
export type IncidentState = "active" | "upcoming" | "ended" | "cleared";

/**
 * One row of `GET /public/incidents`: lighter than [`IncidentDetailJson`]
 * (no description, validity periods, history or current lines).
 */
export type IncidentSummary = { incidentId: string, summary: string, operators: Array<string>, affectedStations: Array<string>, affectedLines: Array<string>, priority: number, isPlanned: boolean, isCleared: boolean, firstSeenAt: string, fetchedAt: string, sourceRemovedAt: string | null, state: IncidentState, startsAt: string | null, validityEndedAt: string | null, };

/**
 * `state`, `startsAt` and `validityEndedAt` (2026-10-20), shared by a list
 * row and the detail so the two can't disagree. See
 * `queries::IncidentState` for the rule.
 * Where an incident stands (2026-10-20, `queries::IncidentState`): its
 * `state`, when its next validity window starts (for `upcoming`), and when
 * its last one finished (once every one has). Times RFC 3339 (`+00:00`).
 */
export type IncidentTiming = { state: IncidentState, startsAt: string | null, validityEndedAt: string | null, };

export type InviteLink = { 
/**
 * `Some` only when this value came straight from
 * [`rotate_invite_link`] -- tokens are stored hashed (2026-09-26 review,
 * L14; `unlisted_links::hash_link_token`, the same SHA-256 scheme
 * `sessions.id` uses), so [`get_active_invite_link`] can report that a
 * link is active and when it expires, but can never recover its token.
 */
token: string | null, expiresAt: string, };

export type JoinPreview = { groupId: string, groupName: string, memberCount: number, };

export type JoinResponse = { groupId: string, };

export type JourneyDetailResponse = { id: number, customName: string | null, createdAt: string, legs: Array<JourneyLegDetailResponse>, 
/**
 * `'web'`, or the delegated client that saved it (spec 2026-10-14
 * §5.8). Only for the journey's owner: absent from the public
 * share-link route and from a group member's view, which have no use
 * for how the owner saved it.
 */
createdVia?: string, 
/**
 * Whether the CALLER owns this journey, as opposed to reading it via a
 * group it's been shared into (`journey_readable_by`). The frontend
 * gates every owner-only action (share-to-group button, unmatched-leg
 * candidate picker, matched-leg "Change train") on this flag -- the
 * backend still refuses all three regardless for a non-owner, but
 * showing them at all to a fellow group member who can only ever get a
 * 404 is its own bug. See this plan's final-review findings (I1).
 */
isOwner: boolean, 
/**
 * The journey's currently-active unlisted share link, if any --
 * `Some` only when `is_owner` is `true` (see `build_journey_detail_response`).
 * A non-owner (a fellow group member, or a visitor who reached this
 * journey via the share link itself) never sees a live, usable token
 * this way -- design doc §4.
 */
shareLink: ShareLinkResponse | null, 
/**
 * Set when the caller follows this journey (DS-146): who shared it and
 * how. Session callers only; absent otherwise.
 */
followedFrom?: FollowedFrom, 
/**
 * Whether the caller could follow this journey through a group it is
 * shared into (DS-146), i.e. `POST /Journeys/{id}/follow` with no body
 * would succeed. Session callers who don't own or already follow it;
 * absent (false) otherwise. A share-link visitor follows with the
 * link's token instead.
 */
canFollow?: boolean, 
/**
 * The open break, `null` when the journey works.
 */
openBreak: OpenBreakSummary | null, 
/**
 * How many plans the journey has had (1: only the booked one).
 */
revisionCount: number, 
/**
 * Suggestion pushes muted for this journey (owner only).
 */
suggestionsMuted?: boolean, 
/**
 * The `/plan` operator filter the journey keeps (owner only; additive,
 * 2026-10-31). Absent when it has none.
 */
operatorFilter?: JourneyOperatorFilter, };

export type JourneyLegDetailResponse = { id: number, originCrs: string | null, 
/**
 * `None` whenever `origin_crs` is `None`, or there is no `stations`
 * reference row for the code (a real, if rare, gap) -- see
 * `data::journeys::JourneyLegWithNamesRow`'s own doc comment.
 */
originName: string | null, destinationCrs: string | null, 
/**
 * See `origin_name`'s doc comment -- same mechanism, joined on
 * `destination_crs`.
 */
destinationName: string | null, serviceDate: string, departAfter: string | null, departBefore: string | null, arriveAfter: string | null, arriveBefore: string | null, matchMode: MatchMode, 
/**
 * Whether this leg was ever created via a window search -- `true` for
 * a `window`-mode leg (even one with all four `depart_*`/`arrive_*`
 * bounds left unset, a deliberate "any train, any time" search) or a
 * template-materialized leg (always a search); `false` for a
 * `pin`/`knownTrain`-mode leg, which never had a window to search.
 * Backs `JourneyLegCard.tsx`'s `hasWindow`/"Change train" gate --
 * 19-pass security/bug review, journeys area, Medium finding 1. See
 * `data::journeys::JourneyLegRow::window_searched`'s own doc comment
 * for the full "why a fourth NULL-checkable field isn't enough"
 * reasoning.
 */
windowSearched: boolean, 
/**
 * `Some` once a train is bound -- the EXACT same shape
 * `GET /Train/{trackingId}` returns
 * (`train_tracking::TRACKED_TRAIN_STATE_SELECT`, `attach_journey_stops`,
 * `blend_darwin_eta`, all reused unchanged; design doc §4). `None`
 * for an unmatched (`train_subscription_id IS NULL`) leg.
 */
trackedTrainState: TrackedTrainState | null, 
/**
 * `null` when the leg has no matched train yet, or no known
 * origin/destination to check against (nothing to report -- not
 * "checked and clean"). See `station_skip.rs` and this plan's §5.2.
 */
legSkip: LegSkipResponse | null, 
/**
 * `train` or `untracked` (spec 2026-10-20 §2.4). Read this before
 * treating `trackedTrainState: null` as "needs a train": an untracked
 * leg never has one. For an untracked leg, `originName`/
 * `destinationName` are its place names and the fields below are set;
 * for a train leg they are `null`.
 */
kind: JourneyLegKind, transitMode: TransitMode | null, lineName: string | null, plannedDeparture: string | null, plannedArrival: string | null, 
/**
 * The untracked leg's clock-only state at read time.
 */
untrackedStatus: UntrackedStatus | null, 
/**
 * The `TfL` stop ids of an untracked leg's places, when they were
 * picked from `GET /tfl/stops` (additive, 2026-10-20); `null` for free
 * text and on a train leg.
 */
originStopId: string | null, destinationStopId: string | null, 
/**
 * A window leg's search filters (DS-215, additive 2026-10-31): the
 * Between stations filters its candidates apply,
 * `journeys::LegSearchFilters`'s camelCase keys, unset ones absent;
 * `null` when it has none.
 */
searchFilters: LegSearchFilters | null, 
/**
 * The change onto this leg from the one before, judged server-side on
 * live times with the station's minimum change time (spec 2026-10-22
 * §5.1, `data::journey_feasibility`). `null` on the first leg; absent
 * altogether when it couldn't be worked out (the frontend then falls
 * back to its own gap check). Additive, 2026-10-22.
 */
connection?: LegConnection | null | null, 
/**
 * The legs of the plan before the latest change that this leg
 * replaced (additive, 2026-10-23); empty for a leg the latest change
 * kept or for a journey never changed.
 */
replacesLegIds: Array<number>, 
/**
 * A timetabled untracked leg's network ride, as saved (DS-237,
 * additive): its network (`tfl`), its line's local id (`victoria`)
 * and its headsign; `null` on every other leg.
 */
transitNetwork: string | null, transitLineId: string | null, headsign: string | null, 
/**
 * What the network's active timetable says about that ride now
 * (`data::timetabled_legs::LegTimetable`): scheduled times only,
 * never live. Absent on other legs, and when it couldn't be read.
 */
timetable?: LegTimetable, };

/**
 * `journey_legs.kind` (its CHECK constraint).
 */
export type JourneyLegKind = "train" | "untracked";

/**
 * A proposed window-mode journey leg -- the read side's flattened
 * `depart_after`/`depart_before` shape, matching
 * `routes::journeys::JourneyLegDetailResponse`'s own established
 * read-side convention (see `common::TimeWindow`'s doc comment for why
 * that convention exists) rather than introducing a nested-object shape
 * only this one response would use. No `arrive_after`/`arrive_before`:
 * a ticket's `current_departure_date` only ever tells us when the
 * traveller meant to LEAVE, never when they'd arrive, so there is nothing
 * non-guessed to propose for an arrival window (see this module's own
 * "never guessed at" posture, shared with `ticket_extraction`).
 */
export type JourneyLegProposal = { originCrs: string | null, destinationCrs: string | null, 
/**
 * `None` whenever the ticket has no `current_departure_date` at all --
 * there is no honest calendar day to propose without it. The caller's
 * own `/track` form already defaults an empty `serviceDate` to today,
 * same as every other visit to that page.
 */
serviceDate: string | null, departAfter: string | null, departBefore: string | null, };

/**
 * One row of `GET /Journeys/mine` -- deliberately lighter than the full
 * `GET /Journeys/{id}` detail (`routes::journeys::JourneyDetailResponse`),
 * mirroring `TrackedTrainListItem`'s own "list is lighter than detail"
 * split. This surfaces a single leg's fields directly, not a nested array
 * -- the "current leg" (the earliest leg that isn't already `completed`,
 * or the journey's LAST leg if every leg is `completed`). A genuine
 * multi-leg rollup (design doc §3's "worst status across legs" idiom) is a
 * later phase's job.
 */
export type JourneyListItem = { id: number, customName: string | null, createdAt: string, 
/**
 * `'web'`, or the delegated client that saved the journey (spec
 * 2026-10-14 §5.8). A delegated client may delete only its own
 * (§5.5a), so its list needs this. Absent on a followed journey's row:
 * how its owner saved it is not the follower's business.
 */
createdVia?: string, legId: number, originCrs: string | null, destinationCrs: string | null, 
/**
 * The current leg's own `journey_legs.service_date` -- the date the
 * traveller is travelling, NOT `created_at` (the date they set the
 * journey up, which can be weeks earlier). A list row cannot name
 * which journey it is without it; `/track/mine`'s tracked-train rows
 * beside it have printed a real service date since they shipped.
 */
serviceDate: string, 
/**
 * Resolved display names for `origin_crs`/`destination_crs`, via the
 * same `LEFT JOIN stations ... ON s.crs = UPPER(...)` mechanism
 * `train_tracking::list_tracked_trains_for_user` already uses (see
 * its comment for why `UPPER` is mandatory). `None` when the code is
 * itself `None`, or when no reference row exists for it --
 * `lib/stationLabel.ts`'s `routeLabel` degrades both ends to bare
 * codes together in that case, rather than mixing one resolved name
 * with one bare code.
 */
originName: string | null, destinationName: string | null, 
/**
 * The whole journey's ends, for naming it: the FIRST leg's origin and
 * the LAST leg's destination (by `leg_order`), whatever the current
 * leg is. `origin_crs`/`destination_crs` above are the current leg's
 * own ends, which for a journey not yet under way are the first
 * leg's -- naming a planned three-leg journey from them showed it
 * ending at its first change. Names resolve as above. Additive
 * (2026-10-20).
 */
firstOriginCrs: string | null, firstOriginName: string | null, lastDestinationCrs: string | null, lastDestinationName: string | null, 
/**
 * The first leg's `service_date`: the day the journey starts, which
 * is what names it. `service_date` above is the current leg's.
 */
firstServiceDate: string, 
/**
 * How many legs the journey has.
 */
legCount: number, matchMode: MatchMode, trainSubscriptionId: number | null, resolutionStatus: ResolutionStatus | null, status: JourneyStatus | null, 
/**
 * The delay at the passenger's own stop, against the PUBLIC timetable
 * (`data::stop_delay`, design doc §9 decision 2): the current leg's destination. `None` until
 * known.
 */
delayMinutes: number | null, 
/**
 * What `delay_minutes` was measured against (`public`, `publicSchedule`
 * or `working`, see `common::public_delay::DelayBasis`); `None` exactly
 * when `delay_minutes` is.
 */
delayBasis: DelayBasis | null, 
/**
 * `true` while `delay_minutes` is a forecast (the train has not reported
 * at that stop yet).
 */
delayProvisional: boolean, 
/**
 * The current leg's kind: `train` or `untracked` (spec 2026-10-20
 * §2.4). For an untracked leg `originName`/`destinationName` carry its
 * place names, and the five fields below are set.
 */
kind: JourneyLegKind, transitMode: TransitMode | null, lineName: string | null, plannedDeparture: string | null, plannedArrival: string | null, 
/**
 * The untracked leg's clock-only state; `None` for a train leg.
 */
untrackedStatus: UntrackedStatus | null, 
/**
 * The current train leg's `trackingPhase` (see
 * `train_tracking::TrackedTrainState::tracking_phase`); `None` for an
 * open or untracked leg.
 */
trackingPhase: TrackingPhase | null, firstReportExpectedAt: string | null, 
/**
 * The whole journey, not just the current leg (additive, 2026-10-21):
 * how many legs are train legs, have a cancelled train, are train legs
 * with no train picked, are over and have started. A train leg is over
 * once its train is `completed` or has reached the leg's destination
 * per `travel_order::train_arrived_sql`; an untracked leg once its
 * planned arrival has passed.
 */
trainLegCount: number, 
/**
 * Whether the journey is over, by the same rule that splits the
 * list's travel order (`travel_order`): the last leg has actually
 * arrived (a cancelled last train once its departure has passed), or
 * every leg is finished. Rows come still-to-come first, so the first
 * `true` is where past journeys begin.
 */
travelled: boolean, 
/**
 * The list's sort key (DS-178): when the journey sets off, its first
 * leg's departure (see [`list_journeys_for_user`] for the fallbacks),
 * so a client merging rows from another list (a train tracked on its
 * own) can place them without re-deriving it. Additive, 2026-10-26.
 */
departsAt: string | null, cancelledLegCount: number, unmatchedLegCount: number, finishedLegCount: number, startedLegCount: number, 
/**
 * The delay at the journey's final arrival: the last train leg's
 * public delay at its own destination. A journey is on time or late
 * by this, not by an earlier leg. `None` when not known, when the
 * journey has no train leg, or when that train is cancelled.
 */
finalArrivalDelayMinutes: number | null, finalArrivalDelayProvisional: boolean, 
/**
 * The journey's open break (mid-journey changes spec 2026-10-22 §7),
 * `None` when it works. Additive, 2026-10-23.
 */
openBreakId: number | null, 
/**
 * The current leg's train's compact cancellation
 * (`data::cancellation`'s list form): full or partial, where it now
 * starts or terminates, and why. `null` when no part of its run is
 * cancelled, and for an open or untracked leg. Additive (2026-10-27).
 */
cancellation: CancellationSummary | null, 
/**
 * Set on a journey the caller follows rather than owns (DS-146,
 * `include=followed` only): who shared it and how. Such a row is
 * read-only. Absent on the caller's own journeys.
 */
followedFrom?: FollowedFrom, 
/**
 * Set on a journey another member shared into one or more of the
 * caller's groups (DS-178, `include=shared` only): one entry per such
 * group, newest share first. Such a row is read-only. Absent on the
 * caller's own journeys; a followed journey that is also shared into
 * one of their groups carries both.
 */
sharedVia?: Array<SharedVia>, };

/**
 * A stored filter, as `GET /Journeys/{id}` serves it.
 */
export type JourneyOperatorFilter = { 
/**
 * `only`, `avoid` or `prefer`.
 */
mode: OperatorMode, 
/**
 * The operators as applied, groups expanded.
 */
codes: Array<string>, 
/**
 * As chosen (`group:GTR`).
 */
entries: Array<string>, };

/**
 * `train_current_state.status` (its CHECK constraint): a train's own
 * status, served as `status` on train and journey responses.
 */
export type JourneyStatus = "awaiting_activation" | "en_route" | "cancelled" | "completed";

/**
 * One calling point of a train's journey, booked schedule merged with the
 * latest reported live data for that location -- see this module's own
 * doc comment and the design doc §2/§3.
 */
export type JourneyStop = { crs: string | null, 
/**
 * The stop's display name: the station's name for a station, else the
 * `tiploc_locations` (or CORPUS) name -- `Heathrow Terminal 3 (bus
 * stop)`, `Marylebone 10 Signal`. `None` only for a TIPLOC no source
 * knows.
 */
name: string | null, 
/**
 * What kind of place this is (`station`, `bus_stop`, `ferry_terminal`,
 * `junction`, `siding`, `passing_point`, `other`); `station` whenever
 * `crs` is set, `None` when nothing is known.
 */
locationType: LocationType | null, 
/**
 * For a bus stop or ferry terminal, the station it belongs to (a
 * `stations` CRS), to link to instead of a page of its own.
 */
parentCrs: string | null, tiploc: string | null, kind: "Origin" | "Intermediate" | "Terminate" | null, actualArrival: string | null, actualDeparture: string | null, 
/**
 * The working time to the minute (`timetable.working_minute_arrival`)
 * plus the train's current overall delay, filled in by
 * `apply_delay_estimates` (below) ONLY while `actual_arrival` is still
 * `None` -- i.e. only for a stop live movement data hasn't reported an
 * actual arrival for yet. Always `None` on a freshly-built stop, same
 * as `actual_arrival`/`actual_departure` above, until that pass runs.
 */
estimatedArrival: string | null, 
/**
 * See `estimated_arrival`'s own doc comment -- same contract, gated on
 * `actual_departure` instead.
 */
estimatedDeparture: string | null, 
/**
 * Where `estimated_arrival`/`estimated_departure` came from when it is
 * not the train's own TRUST delay: `"previous-working"` when they are a
 * predicted late start from the incoming train (DS-127). `None` for
 * the existing estimate (and for a stop with none).
 */
estimateSource: string | null, lastEventType: string | null, variationStatus: string | null, 
/**
 * How late the train was at this stop, in whole minutes (negative is
 * early), measured against the PUBLIC timetable on the side the stop's
 * latest TRUST report was for (an arrival against the public arrival,
 * a departure against the public departure). `delay_basis` says which
 * public time was used, or that none was known and the working
 * timetable was used instead; see `common::public_delay` (design doc
 * §9 decision 2, §11). `None` until the train reports here.
 *
 * See `apply_stop_status`'s own doc comment for the one exception:
 * cleared to `None` for a [`StopStatus::Skipped`] stop even when a
 * TRUST `PASS` event supplied a value here first.
 */
delayMinutes: number | null, 
/**
 * The baseline `delay_minutes` was measured against: `public` (TRUST's
 * own public time), `publicSchedule` (the CIF public time, when TRUST
 * sent none, e.g. a movement matched from the backlog) or `working`
 * (no public time in that direction: a pass, or the departure of a
 * set-down-only stop). `None` exactly when `delay_minutes` is.
 */
delayBasis: DelayBasis | null, 
/**
 * See [`StopStatus`]'s own doc comment. Computed by `apply_stop_status`,
 * after the live movement overlay -- always `StopStatus::Unknown` on a
 * freshly-built stop, same "None/default until the relevant pass runs"
 * contract every other computed field on this struct already has.
 */
stopStatus: StopStatus, 
/**
 * `Some` only when `stop_status` is [`StopStatus::Skipped`] -- see
 * [`SkipSource`]'s own doc comment for why this lives here rather than
 * nested inside `stop_status` itself.
 */
skipSource: SkipSource | null, 
/**
 * The CURRENT (live/expected) Darwin platform for this stop, from two
 * sources: the origin's pin-time snapshot (`apply_origin_platform`)
 * and, for ANY departing calling point whose station `poller-ldbws`
 * samples, this train's row on that station's current departure board
 * (`stop_board::apply_station_sample_board`, the same unique match as
 * `board`, which wins when its row has a platform; a cancelled row's
 * platform is served with `platform_status` `Cancelled`).
 * Darwin/LDBWS's board only ever reports a station's OWN platform
 * for a service departing FROM it
 * (`poller-ldbws/src/schema.rs`'s `RdmCallingPoint` carries no
 * platform field at all), so a terminating stop, a stop at an
 * unsampled station, a stop whose board row is ambiguous or more than
 * 10 minutes old, or a service that has already left a station's
 * board is `None` -- exactly "not known", never a fabricated value and
 * never another train's platform.
 */
platform: string | null, 
/**
 * The EARLIEST Darwin platform observed for this stop, reconstructing
 * "planned" the same way `common::StationDeparture.planned_platform`
 * does -- see that field's own doc comment. NOT the CIF timetable's
 * booked platform. `None` under the same conditions as `platform`
 * above.
 */
plannedPlatform: string | null, 
/**
 * Whether the live `platform` differs from the one the stop was meant
 * to use. Since DS-127 the comparison is against the timetable's
 * `booked_platform` whenever both are known (catching a change made
 * before the poller first saw the row, and surviving poller
 * restarts), ignoring case, spaces and a sub-platform letter on one
 * side only ("4" against "4A"). Without a booked platform it falls
 * back to `platform` against `planned_platform` (the first platform
 * the poller saw). Always `false` for a stop with no live platform.
 */
platformChanged: boolean, 
/**
 * Whether the train still calls at `platform`: `Some` exactly when
 * `platform` is, `None` otherwise. See [`PlatformStatus`].
 */
platformStatus: PlatformStatus | null, 
/**
 * The TIMETABLED platform from the CIF schedule itself
 * (`schedule_query::CallingPoint::platform`: `LO`/`LT` `19..22`, `LI`
 * `33..36`), independent of any Darwin data -- known for every calling
 * point whose CIF record carries one, including stops at stations
 * `poller-ldbws` never samples and the terminating stop. Deliberately a
 * SEPARATE field from `planned_platform` (Darwin's earliest-observed
 * reconstruction) and never folded into `platform_changed`: the two
 * "planned" notions come from different systems, and a CIF platform
 * can legitimately differ in form from Darwin's (e.g. a sub-platform
 * suffix), so comparing them would risk a false "platform changed".
 * `None` when the CIF field is blank, or the schedule row predates this
 * field -- "not known", never guessed. Since DS-127 it is what
 * `platform_changed` compares against when both are known.
 */
bookedPlatform: string | null, 
/**
 * What `display_platform` rests on (DS-127): `confirmed` when
 * `platform_evidence` is set (DS-180, or Darwin's confirmation, DS-152; `platform` is then that
 * platform), else `board` when a live board shows `platform`, `booked`
 * when only `booked_platform` is known (show it as provisional),
 * `None` when none is. Set by
 * `api::data::journey::apply_platform_basis`.
 */
platformBasis: PlatformBasis | null, 
/**
 * Darwin's Push Port has a platform for this stop but suppresses it
 * from public display (`platsup`, or a station CIS's `cisPlatsup`;
 * DS-152). That platform is not served: `platform` stays as any board
 * shows it (boards hide it too), and `displayPlatform` falls back to
 * the booked one. So "hidden" and "not allocated" differ: `true` here
 * means a platform exists but is not yet public.
 */
platformSuppressed: boolean, 
/**
 * The platform to show (DS-127): the live `platform` when known, else
 * `booked_platform`. A server-side convenience so every client applies
 * the same precedence; `platform` and `booked_platform` keep their
 * meanings.
 */
displayPlatform: string | null, 
/**
 * LDBWS-style live status of this stop (`OnTime`, `Late`, `Cancelled`,
 * `NoReport`, `Arrived`, `Departed`, `Scheduled`), served as `status`.
 * See `data::stop_live_status` for the mapping. Computed last, by the
 * train-detail routes once the train's own status is known, so it is
 * `None` on a freshly built stop.
 */
status: LiveStopStatus | null, 
/**
 * How many minutes late this stop is expected to be; `Some` only when
 * `live_status` is `Late`.
 */
lateMinutes: number | null, 
/**
 * Whether this call is in a cancelled part of the train's run (DS-181):
 * the train is cancelled, terminated short before it, starts after it,
 * or is cancelled between two stations around it. Set with
 * `live_status` by the train-detail routes from the train's
 * `cancellation` (`api::data::cancellation`); `false` on a freshly built
 * stop. A skipped call (`stop_status` `Skipped`) is not counted here.
 */
cancelled: boolean, 
/**
 * This train's row on the stop's current LDBWS departure board
 * (Darwin's `delayReason`/`cancelReason`/`isCancelled`/`etd`), or
 * `None` when there is no unique fresh match. Set by
 * `stop_board::apply_station_sample_board`; see that module's doc for
 * the matching rules and when it is null. Serialized as `null`, never
 * omitted.
 */
board: StopBoard | null, scheduledArrival: string | null, scheduledDeparture: string | null, publicArrival: string | null, publicDeparture: string | null, workingArrival: string | null, workingDeparture: string | null, workingPass: string | null, canBoard: boolean, canAlight: boolean, requestStop: boolean, passingPoint: boolean, };

/**
 * `GET /JourneyTemplates/{id}`'s response. `daysOfWeek`/`active`/
 * `startsOn`/`endsOn`/`defaultMatchMode`/`autoCommitRule` are all real,
 * round-tripped fields (so a future Phase C edit UI has somewhere to
 * read/write) -- this plan's own frontend (Task 8) renders NONE of them
 * as editable controls; see that task's own scope note.
 */
export type JourneyTemplateDetailResponse = { id: number, customName: string | null, createdAt: string, updatedAt: string, daysOfWeek: number | null, active: boolean, startsOn: string | null, endsOn: string | null, defaultMatchMode: TemplateMatchMode, autoCommitRule: AutoCommitRule | null, legs: Array<JourneyTemplateLegDetailResponse>, };

export type JourneyTemplateLegDetailResponse = { id: number, originCrs: string | null, originName: string | null, destinationCrs: string | null, destinationName: string | null, departAfter: string | null, departBefore: string | null, arriveAfter: string | null, arriveBefore: string | null, };

/**
 * One row of `GET /JourneyTemplates/mine` -- deliberately lighter than the
 * full detail response, mirroring `journeys::JourneyListItem`'s own
 * "list is lighter than detail" split. Summarizes a multi-leg template as
 * "first leg's origin -> last leg's destination," the same rollup
 * `app/journeys/[id]/page.tsx`'s `defaultJourneyTitle` computes client-side
 * for a journey with no `customName` -- computed server-side here instead
 * since a list row has no per-leg detail to compute it from client-side.
 */
export type JourneyTemplateListItem = { id: number, customName: string | null, createdAt: string, legCount: number, firstOriginCrs: string | null, firstOriginName: string | null, lastDestinationCrs: string | null, lastDestinationName: string | null, active: boolean, daysOfWeek: number | null, };

export type JsonValue = number | string | boolean | Array<JsonValue> | { [key in string]: JsonValue } | null;

/**
 * A change that works only because the train it changes onto is running
 * late (DS-141): served as a train leg's `live.reliesOnLateDeparture` on
 * `/Trips/plan` (`T` a local clock time), and as a saved journey's
 * `connection.reliesOnLateDeparture` and an alternative's train leg's
 * (`T` an instant). Absent otherwise.
 *
 * The live overlay keeps a late train late (no recovery is modelled), so
 * such a change is offered, but the train could make up time and leave
 * nearer its booked time, before the traveller can reach it.
 */
export type LateDepartureReliance<T> = { 
/**
 * The onward train's timetabled (public, else working) departure.
 */
scheduledDeparture: T, 
/**
 * Its expected (live) departure, which the change was judged on.
 */
expectedDeparture: T, 
/**
 * `expected_departure` less `scheduled_departure`, in minutes.
 */
delayMinutes: number, 
/**
 * The spare minutes the change would have, after its minimum change
 * time, were the train to leave on time: always negative (that many
 * minutes short).
 */
slackIfOnTimeMinutes: number, };

/**
 * The change onto a leg from the one before it (`connection` on every
 * leg of `GET /Journeys/{id}` after the first).
 */
export type LegConnection = { 
/**
 * Minutes from the previous leg's expected arrival (reported, else
 * estimated, else timetabled) to this leg's expected departure.
 * Negative when this leg leaves first. `null` when either is unknown.
 */
gapMinutes: number | null, 
/**
 * The time the change needs: the boarding station's minimum change
 * time, a fixed link's minutes plus that, or 0 staying on one train.
 * `null` when not known (see [`ConnectionState::Unknown`]) or when the
 * next leg is untracked (a walk or a Tube ride starts when you do).
 */
minimumChangeMinutes: number | null, state: ConnectionState, 
/**
 * Set when the change works only because the next train is running
 * late (additive, DS-141; absent otherwise): it would not, on the
 * next train's timetabled departure. The state is judged as before
 * (usually `ok`); this says the train could leave earlier than
 * expected. See [`trip_plan_live::LateDepartureReliance`].
 */
reliesOnLateDeparture?: LateDepartureReliance<string>, 
/**
 * Onto a timetabled leg (DS-237, additive; absent otherwise): the
 * ride its timetable has first from when the traveller gets there,
 * when that is not the planned one (the traveller arrives late, or
 * the timetable has moved it). The changes after it are judged from
 * its arrival. Scheduled times, never live.
 */
nextTimetabled?: TimetabledRide, };

/**
 * A train leg's live status, served as the leg's `live`.
 */
export type LegLive = { 
/**
 * `Cancelled`, `Departed`, `Late`, `Delayed`, `OnTime` or `Scheduled`
 * -- the `stop_live_status` vocabulary, for the boarding call.
 */
status: string, 
/**
 * The train does not call at the boarding or alighting stop today.
 */
cancelled: boolean, 
/**
 * Minutes late departing the boarding stop, when known or propagated.
 */
delayMinutes: number | null, 
/**
 * Minutes late arriving at the alighting stop, when known or propagated.
 */
arrivalDelayMinutes: number | null, 
/**
 * Darwin's cancel/delay text, else TRUST's cancellation reason.
 */
reason: string | null, 
/**
 * `darwin` or `trust`, when `reason` is set.
 */
reasonSource: string | null, 
/**
 * Live platform at the boarding stop (fresh board only).
 */
platform: string | null, 
/**
 * When the newest fact behind this was observed.
 */
observedAt: string | null, 
/**
 * Whether the change onto this leg from the previous train leg still
 * works on live times; `None` for the first train leg.
 */
interchangeFeasible: boolean | null, 
/**
 * Set when that change works only because this train is running late
 * (additive, DS-141; absent otherwise): see [`LateDepartureReliance`].
 */
reliesOnLateDeparture?: LateDepartureReliance<string>, };

/**
 * A window leg's search filters (DS-215): the Trains section's Between
 * stations filters, saved with the leg's window so its candidates list
 * only the trains they allow. Each value is the text
 * `GET /public/trains/search` takes for the same filter (`operator`,
 * `origin` as `startsAt`, `terminates_at`, `previously_calls_at`,
 * `previously_stops_at`, `calls_at`, `via`, `avoid`, `avoid_stop`), so the
 * same parsers check it (`routes::trains::leg_filters`). `startsAt` is
 * that search's `origin`, renamed here because a leg's `origin` is where
 * the traveller boards. The wire shape and the
 * `journey_legs.search_filters` column's JSON are both this struct.
 */
export type LegSearchFilters = { operator?: string, startsAt?: string, terminatesAt?: string, previouslyCallsAt?: string, previouslyStopsAt?: string, callsAt?: string, via?: string, avoid?: string, avoidStop?: string, };

export type LegSkipResponse = { originSkipped: boolean, destinationSkipped: boolean, };

/**
 * A leg's `timetable` on `GET /Journeys/{id}`: what the network's active
 * timetable says about it now. Scheduled times only.
 */
export type LegTimetable = { network: string, 
/**
 * `null` when the active feed no longer has the line.
 */
line: LineView | null, state: MatchState, 
/**
 * The ride found (`matched` or `retimed`); else `null`.
 */
scheduledDeparture: string | null, scheduledArrival: string | null, headsign: string | null, 
/**
 * Boarding to alighting; empty without a ride.
 */
calls: Array<TimetabledCall>, 
/**
 * How often the line runs between the two stations around the
 * planned time; absent with too few rides to say.
 */
headwayMinutes?: number, 
/**
 * Current disruption on the line (the plan's transit leg notices).
 */
notices: Array<TransitLegNotice>, 
/**
 * On a network whose stations are `TfL`'s stops (`tfl`, phase C), so
 * with `TfL`'s live arrivals: which of
 * the boarding stop's predictions are for this ride's line and reach
 * the alighting stop. Absent elsewhere, and when no pattern of the
 * line serves the two stops.
 */
liveArrivals?: LiveArrivalsFilter, };

export type LineCatalogueStation = { crs: string, name: string | null, role: string, };

/**
 * Minimal cross-source projection — just enough to answer "what stations
 * and operators does this line cover", for both catalogue and custom
 * lines alike. Deliberately separate from `CustomLineDetail`/`get_line`:
 * that endpoint is custom-only by design (its 404-for-a-catalogue-id
 * behavior is how the frontend detail page tells custom and catalogue
 * lines apart — see `frontend/app/lines/[id]/page.tsx`'s `isCustom`
 * check), so extending it to also serve catalogue lines would silently
 * break that detection instead of adding a tooltip.
 */
export type LineDefinitionSummary = { stations: Array<string>, operators: Array<string>, };

/**
 * A line train's `direction` by the line's catalogue order.
 */
export type LineDirection = "up" | "down" | "loop";

/**
 * `lineDue`: Europe/London local `HH:MM:SS`, `dayOffset` days after the
 * service date.
 */
export type LineDue = { time: string, dayOffset: number, };

/**
 * A group a custom line is shared into, as the line's OWNER sees it on
 * their own edit page ("Shared with: Family, Commute Buddies").
 */
export type LineGroupRef = { id: string, name: string, };

/**
 * Where a `GET /public/lines` row comes from: `routes::lines::list_lines`.
 */
export type LineSource = "catalogue" | "custom" | "tfl";

export type LineStationRef = { crs: string, name: string | null, };

/**
 * One status of a [`LineStatusReportJson`].
 */
export type LineStatus = { statusSeverity: number, statusSeverityDescription: string, reason: string, dataQuality: DataQuality, validityPeriods: Array<ValidityPeriod>, sampleStats?: SampleStats, sampleAvailability: SampleAvailability, fullCoverageStats?: SampleStats, fullCoverageAvailability: FullCoverageAvailability, 
/**
 * Only with `?detail=true` (and always on `StopPoint`).
 */
disruption?: Disruption, };

/**
 * A line's status report, TfL-shaped: `GET /Line/Mode/{mode}/Status`,
 * `GET /Line/{ids}/Status`, `GET /StopPoint/{crs}/Disruption` and the
 * status history.
 */
export type LineStatusReport = { $type: "DistantSignal.LineStatusReport", id: string, name: string, modeName: string, operators: Array<string>, 
/**
 * RFC 3339, `+00:00`.
 */
computedAt: string, lineStatuses: Array<LineStatus>, 
/**
 * A line with a `TfL` counterpart merged away from `/public/lines`:
 * the counterpart's current statuses (see
 * [`to_tfl_shape_with_overlay`]). Absent otherwise.
 */
tflStatus?: Array<LineStatus>, 
/**
 * The current-status routes' disruptions announced for the line that
 * have not started yet ([`upcoming_json`]). Absent from the history
 * and `StopPoint` routes.
 */
upcoming?: Array<UpcomingDisruption>, 
/**
 * The credits the line's data needs ([`line_attribution`]), plus
 * `TfL`'s with a `tflStatus` overlay.
 */
attribution: Array<TransitAttribution>, };

export type LineSummary = { id: string, name: string, category: string, operators: Array<string>, source: LineSource, };

/**
 * A time with its day offset, as the API writes `lineDue`.
 */
export type LineTime = { time: string, dayOffset: number, };

export type LineTimetablePage = { lineId: string, date: string, scopeApplied: boolean, scopes: Array<LineTrainScope>, directions: Array<LineDirection> | null, from: string | null, to: string | null, at: string | null, stations: Array<LineCatalogueStation>, 
/**
 * The day's trains under `scope`, `from` and `to`, per scope and
 * direction (`none` without one) -- before `dir`, `at` and the
 * cursor: the direction tabs' counts.
 */
counts: { [key in string]: { [key in string]: number } }, trains: Array<LineTimetableTrain>, 
/**
 * Pass back as `after` for the next page; `null` on the last.
 */
nextCursor: string | null, };

export type LineTimetableTrain = { 
/**
 * When the train is listed: its departure from `from`, else its time
 * on the line (`lineDue`).
 */
time: LineTime, 
/**
 * Its arrival at `to`, when `to` was given.
 */
arrival: LineTime | null, uid: string, operator: string | null, scope: LineTrainScope | null, direction: LineDirection | null, lineDue: LineTime | null, origin: LineStationRef | null, destination: LineStationRef | null, onLineStops: Array<LineTrainStop>, live: LineTrainSummaryLive | null, serviceMode: ServiceMode, liveTracking: boolean, };

/**
 * One calling point of a `schedule_line_population` entry, served
 * verbatim as a line train's `callingPoints`: `schedule-query`'s
 * `records::CallingPoint` serialization (snake case, times `HH:MM:SS`),
 * which the api never parses. A field added there is added here.
 */
export type LineTrainCallingPoint = { tiploc: string, kind: CallingPointKind, booked_arrival: string | null, booked_departure: string | null, is_half_minute_arrival: boolean, is_half_minute_departure: boolean, day_offset: number, 
/**
 * CIF's packed activity codes.
 */
activity: string, public_arrival: string | null, public_departure: string | null, platform: string | null, 
/**
 * A passing point's booked pass; absent at a call.
 */
booked_pass?: string, };

/**
 * One `GET /public/lines/{id}/trains` result entry, as built by
 * [`line_train_json`]. Same wire shape the route has always had (it used
 * to be a `json!` object with exactly these keys).
 */
export type LineTrainEntry = { uid: string, 
/**
 * The population entry's own calling points, verbatim.
 */
callingPoints: Array<LineTrainCallingPoint> | null, scheduleOriginCrs: string | null, scheduleOriginName: string | null, scheduleDestinationCrs: string | null, scheduleDestinationName: string | null, liveStatus: LineTrainLiveStatus | null, scope?: LineTrainScope, direction?: LineDirection, runFirstCrs?: string, runLastCrs?: string, lineDue?: LineDue, serviceMode: ServiceMode, liveTracking: boolean, };

/**
 * A line train's live state, when a `trains` row exists for it: a
 * hand-picked subset of `PublicTrainState` (no calling points or journey
 * stops).
 */
export type LineTrainLiveStatus = { trainsId: number, trainId: string | null, originCrs: string | null, originName: string | null, destinationCrs: string | null, destinationName: string | null, scheduledDeparture: string | null, status: JourneyStatus | null, lastReportedLocation: string | null, lastEventType: string | null, delayMinutes: number | null, delayBasis: DelayBasis | null, delayProvisional: boolean, nextCallingPoint: string | null, etaNext: string | null, etaSource: EtaSource | null, operatorCode: string | null, operatorName: string | null, cancelled: boolean, cancelReasonCode: string | null, cancelReason: string | null, changeOfOriginReasonCode: string | null, changeOfOriginReason: string | null, };

/**
 * A line train's `scope`: `schedule_line_population`'s membership
 * (`data::queries` population entries; line membership design).
 */
export type LineTrainScope = "line" | "shared" | "touch";

export type LineTrainStop = { crs: string, time: string, dayOffset: number, };

export type LineTrainSummary = { uid: string, operator: string | null, scope: LineTrainScope | null, direction: LineDirection | null, lineDue: LineTime | null, origin: LineStationRef | null, destination: LineStationRef | null, onLineStops: Array<LineTrainStop>, live: LineTrainSummaryLive | null, serviceMode: ServiceMode, liveTracking: boolean, };

/**
 * The compact live status the summary carries.
 */
export type LineTrainSummaryLive = { status: JourneyStatus | null, delayMinutes: number | null, delayProvisional: boolean, cancelled: boolean, 
/**
 * The cancellation's TRUST glossary text while `cancelled` (DS-181;
 * `PublicTrainState::cancel_reason`), `null` without one. These rows
 * have no calling points to place a partial cancellation on.
 */
cancelReason: string | null, lastReportedLocation: string | null, };

/**
 * The whole summary body.
 */
export type LineTrainsSummary = { lineId: string, date: string, scopeApplied: boolean, scopes: Array<LineTrainScope>, window: LineTrainsWindow | null, at: string | null, directions: Array<LineDirection> | null, stations: Array<LineCatalogueStation>, 
/**
 * Trains in the window, per scope and direction (`none` for a train
 * without one), before the `direction` filter -- for tab counts.
 */
counts: { [key in string]: { [key in string]: number } }, truncated: boolean, trains: Array<LineTrainSummary>, 
/**
 * Only with `at`.
 */
running: Array<LineTrainSummary> | null, };

export type LineTrainsWindow = { from: string, to: string, };

/**
 * A ride's line, as a plan's transit leg shows it.
 */
export type LineView = { 
/**
 * The public id, `tfl:victoria`, `metrolink:blue`.
 */
id: string, name: string, colour: string | null, 
/**
 * `tube`, `dlr`, `tram` (a line's own, else the network's).
 */
mode: string | null, };

/**
 * What a client picks from `GET /public/tfl/stops/{stopId}/arrivals` for
 * a timetabled leg (DS-237, phase C): the groups of `lineId`, and in them
 * the trains whose destination is one of `destinations`, the last stations
 * of the line's timetabled patterns from the boarding stop through the
 * alighting one. Indicative: a prediction is not the leg's ride.
 */
export type LiveArrivalsFilter = { 
/**
 * The boarding stop, `TfL`'s spelling (`940GZZLUEUS`).
 */
stopId: string, 
/**
 * `TfL`'s line id (`victoria`; the network's local ids are `TfL`'s).
 */
lineId: string, 
/**
 * Station names, as the timetable names them, sorted.
 */
destinations: Array<string>, };

/**
 * See the module doc for each value's meaning. `PascalCase` on the wire,
 * like `StopStatus` beside it.
 */
export type LiveStopStatus = "OnTime" | "Late" | "Cancelled" | "NoReport" | "Arrived" | "Departed" | "Scheduled";

/**
 * A non-station location suggestion's `kind`:
 * `reference::Suggestion::from_location`.
 */
export type LocationSuggestionKind = "bus" | "ferry";

/**
 * What kind of place a TIPLOC is, as far as a passenger cares.
 *
 * Derived by `schedule-reference` from, in order: whether the TIPLOC is a
 * station (a STANOX and a bookable CRS), which kinds of service call there
 * (CIF train status: bus or ship services only), then name heuristics
 * ([`classify_name`]), then whether rail services only ever pass it.
 */
export type LocationType = "station" | "bus_stop" | "ferry_terminal" | "junction" | "siding" | "passing_point" | "other";

/**
 * `journey_legs.match_mode` (its CHECK constraint).
 */
export type MatchMode = "unmatched" | "manual" | "auto";

/**
 * How the leg's ride stands in today's timetable.
 */
export type MatchState = "matched" | "retimed" | "notInTimetable" | "noService" | "noTimetable";

export type MaterializeTemplateResponse = { journeyId: number, legIds: Array<number>, };

/**
 * A station returned by [`nearest_stations`]: the same `code`/`name` shape
 * as [`Suggestion`] plus the great-circle distance from the caller's point,
 * in kilometres (this app has no other established distance unit anywhere
 * in its schema or docs -- see `docs/superpowers/specs/2026-08-31-other-uk-
 * transit-networks-research.md` and `2026-09-05-ireland-vs-northern-
 * ireland-friction-research.md`, both of which quote UK/Ireland network
 * distances in km -- so km is the default for a UK-facing app rather than
 * miles).
 */
export type NearbyStation = { code: string, name: string, distanceKm: number, };

/**
 * Why a segment has no itineraries -- served as the segment's
 * `noResultReason` (absent when it has some). Adapted from `train-mcp`'s
 * `ConstraintFailure` (`src/timetable/plan/constraints.ts`): "no journey
 * found" is nearly useless once several constraints are in play, so this
 * names the one that made the query infeasible.
 *
 * `constraint` is one of:
 *
 * - `maxChanges` (`results=options`): itineraries exist, but every one
 *   needs more changes than the cap;
 * - `avoid`, `avoidStop`, `avoidChange`: an itinerary exists without that
 *   list (`values`), none with it;
 * - `avoidCombined`: one exists without the avoid lists, but dropping any
 *   single list is not enough (`values` is every avoided code);
 * - `via` (2026-10-06): one exists without the pass-through vias; `values`
 *   is the single via whose removal alone is enough, or every via when no
 *   single one is (every segment carries it);
 * - `departAfter` / `arriveBy`: nothing leaves late enough / arrives early
 *   enough (`values` is the time, `HH:MM`), though something runs that day;
 * - `noRoute`: nothing reaches the destination that service day at all
 *   (under the avoid lists, if any);
 * - `previousSegment` / `nextSegment`: a waypoint segment that was not
 *   searched, because the one it chains from found nothing;
 * - `operator` / `avoidOperator` (2026-10-31, `?operator=` /
 *   `?avoidOperator=`): an itinerary exists without the operator filter,
 *   none with it (`values` is the filter's codes; `linkOperators` names
 *   the operators whose links or connecting rides, rather than trains,
 *   the route needs). `operator` also when none of the chosen operators
 *   runs that day. A filter that only fails together with the avoid lists
 *   is `avoidCombined`, its `values` then holding the operator codes too.
 */
export type NoResultReason = { constraint: string, values: Array<string>, message: string, 
/**
 * `operator` / `avoidOperator` only (2026-10-31): the operators whose
 * links or connecting rides, not trains, made the difference (`["LT"]`:
 * crossing London needs the Tube). Absent otherwise.
 */
linkOperators?: Array<string>, };

/**
 * The open break as `GET /Journeys/{id}` serves it.
 */
export type OpenBreakSummary = { id: number, cause: BreakCause, fault: BreakFault, severity: number, triggerLegId: number | null, detectedAt: string, suggestionsReady: boolean, stale: boolean, 
/**
 * The sheet's headline facts, when suggestions are stored: where, to
 * where, and the best option's arrival.
 */
stationName: string | null, destinationName: string | null, suggestionCount: number, firstArrival: string | null, };

/**
 * `journeys.operator_filter_mode` (its CHECK constraint).
 */
export type OperatorMode = "only" | "avoid" | "prefer";

/**
 * One operator's status rollup: `GET /public/operators` and
 * `GET /public/operators/{code}`. `sampleStats` is the stats (camelCase)
 * rendered by `render::sample_stats_json`, absent when there are none.
 */
export type OperatorSummary = { code: string, name: string, lineIds: Array<string>, worstSeverity: number, reason: string, worstLineId: string | null, worstLineName: string | null, computedAt: string | null, sampleStats?: SampleStats, };

/**
 * One label of a `results=options` journey (DS-196), served as
 * `{"kind": "fastest"}`, `{"kind": "leavesLatest"}`,
 * `{"kind": "fewestChanges"}` or `{"kind": "alternative", "viaCrs": "MYB"}`
 * (`viaCrs` may be `null`).
 */
export type OptionLabel = { "kind": "fastest" } | { "kind": "leavesLatest" } | { "kind": "fewestChanges" } | { "kind": "alternative", 
/**
 * The station that tells it apart from the fastest option.
 */
viaCrs: string | null, } | { "kind": "operatorOnly", 
/**
 * The preferred operators' codes.
 */
operators: Array<string>, };

/**
 * Whether the train got the passenger to the ticket's destination, from
 * TRUST and Darwin (`data::delay_repay_outcome`).
 */
export type Outcome = "arrived" | "departedOnly" | "notReached";

/**
 * What a `.pkpass`/PDF parse could recover -- the same fillable fields as
 * `common::TicketEntryRequest`, minus a user-chosen `source` (this is
 * fixed per parse path) plus a fixed `source` describing which one
 * produced it. `None` means "not found in this file, leave for the user
 * to fill in" -- never guessed at. This is exactly what a human sees on a
 * review-before-save form pre-filled from an upload; nothing here is ever
 * written to `tracked_train_tickets` directly -- see this module's own
 * doc comment.
 *
 * `Deserialize` exists only for `data::ticket_subprocess`, whose parent
 * process reads this back from the parse child's JSON reply.
 */
export type PartialTicket = { operator: string | null, ticketType: string | null, 
/**
 * Best-effort station identifier -- almost never a real CRS code in
 * practice (neither `.pkpass` nor PDF extraction publishes one; both
 * give station NAMES, e.g. "Kings Cross"). Deliberately NOT
 * normalized here: `train_tracking::validate_ticket_entry`'s existing
 * CRS-format check is what actually forces a human to correct this
 * into a real code before it can be saved -- see this plan's Global
 * Constraints.
 */
originCrs: string | null, destinationCrs: string | null, 
/**
 * The ticket's own claimed departure INSTANT, when the source format
 * carries one -- currently only ever populated from a `.pkpass`'s
 * `semantics.currentDepartureDate` (Apple's standardised key for
 * exactly this, an ISO 8601 date-time string), and only `Some` when
 * that key parses as a real `DateTime`. `None` for every PDF-sourced
 * ticket (no PDF date/time extraction exists in this module -- neither
 * tier's ticket-type/route regexes attempt one, and adding one would
 * be a new, separate false-positive surface against unstructured
 * text); also `None` for a `.pkpass` whose `semantics` dictionary is
 * absent, or present but missing this one key, or a `primaryFields`
 * heuristic match (that positional fallback is names only, per
 * `primary_fields_origin_destination`'s own doc comment -- it has
 * nothing dateable to read either).
 *
 * This is deliberately a best-effort HINT for a journey-leg search
 * window (`data::journey_leg_proposal::propose_window_leg`), never a
 * hard pin: a ticket's booked departure can genuinely differ from the
 * train the traveller actually catches (an earlier/later service on
 * the same ticket, a rebooked journey, ...), so nothing in this
 * codebase ever treats this field as an exact scheduled-departure
 * match target the way `TrackPinRequest::scheduled_departure` is.
 */
currentDepartureDate?: string, source: TicketSource, };

/**
 * One of the badged picks (§17.6), in display order.
 */
export type Pick = "recommended" | "fastest" | "fewestChanges" | "sameOperator" | "noTube" | "rejoin";

export type PlannedItinerary = { legs: Array<PlannedLeg>, 
/**
 * Changes between National Rail trains (rides less one): what
 * `maxChanges`, `exceedsRecommendedChanges` and the options' change
 * trade-off count.
 */
changeCount: number, 
/**
 * Changes made within `TfL` access legs (additive, 2026-10-30): one
 * per access leg with `access.changeAt` (Uxbridge to Heathrow changes
 * at Acton Town). Not part of `change_count`; a client wanting every
 * change adds the two. Set by [`count_access_changes`] once the legs
 * are final.
 */
accessChangeCount: number, totalDurationMinutes: number, 
/**
 * `results=fastest` only -- see this plan's Judgment Call 3.
 */
exceedsRecommendedChanges?: boolean, 
/**
 * Live overlay only (absent otherwise): no cancelled leg and no change
 * that live times make impossible.
 */
liveFeasible?: boolean, 
/**
 * The first leg is the same train the previous segment's itinerary rode
 * into the waypoint: the traveller stays aboard and makes no change
 * there (additive, 2026-09-29; always `false` for the first segment).
 */
continuesPreviousTrain: boolean, 
/**
 * Only on a request planned with transit networks (`transit=true` or a
 * tram stop end; additive, 2026-10-24, DS-164): whether any leg is
 * `kind: "transit"`, so its times are scheduled, not live. Absent
 * otherwise.
 */
scheduledOnly?: boolean, 
/**
 * `results=options` only (additive, 2026-10-30, DS-196): what this
 * option is -- see [`OptionLabel`]. The same on every segment's part
 * of one journey. Absent in `fastest` mode.
 */
labels?: Array<OptionLabel>, };

/**
 * [`PlannedLeg`]'s wire shape for its generated TypeScript (`api_types`):
 * the derived `train` and `transfer` legs, or a `transit` leg, which its
 * `Serialize` writes in a train leg's place.
 */
export type PlannedLeg = PlannedRailOrTransferLeg | TransitLeg;

export type PlannedRailOrTransferLeg = { "kind": "train", trainUid: string, serviceDate: string, originCrs: string | null, destinationCrs: string | null, 
/**
 * Local civil clock time, matching this app's established
 * "CIF times are Europe/London local, rendered as HH:MM, never
 * converted to UTC on this wire shape" convention (e.g.
 * `schedule_query::ScheduleDeparture::scheduled`).
 *
 * The PUBLIC departure at the boarding call (arrival at the
 * alighting call), falling back to the working time where the call
 * has none in that direction -- the times the search planned on.
 * The working times until `trip_leg_details::attach_leg_details`
 * fills the public ones in. Working (WTT) on the wire until
 * 2026-10-20 (design doc §9 decision 1); see `working_*`.
 */
scheduledDeparture: string, scheduledArrival: string, 
/**
 * How many calendar days past `service_date` `scheduled_departure`
 * actually falls on (additive, 2026-09-28): a later waypoint
 * segment now searches from the previous segment's arrival, so an
 * onward train can board after midnight on a service that started
 * the evening before -- this says so rather than leaving a bare
 * `00:30` that reads as the morning of `service_date`.
 */
departureDayOffset: number, 
/**
 * How many calendar days past `service_date` `scheduled_arrival`
 * actually falls on -- same field, same meaning, as
 * `schedule_query::records::CallingPoint::day_offset`.
 */
arrivalDayOffset: number, 
/**
 * The working-timetable (WTT) departure at the boarding call and
 * arrival at the alighting call, truncated to the minute, and their
 * days past `service_date` (additive, 2026-10-20: what
 * `scheduled_*` served until then). What
 * `trip_leg_details::attach_leg_details` finds the calls by.
 */
workingDeparture: string, workingArrival: string, workingDepartureDayOffset: number, workingArrivalDayOffset: number, 
/**
 * CIF booked (timetabled) platform at the boarding calling point,
 * and at the alighting one -- `schedule_calling_points_full.platform`.
 * Never live/Darwin. `None` until `trip_leg_details::attach_leg_details`
 * fills them in; stays `None` when the CIF field is blank.
 */
bookedDeparturePlatform: string | null, bookedArrivalPlatform: string | null, 
/**
 * The public (GBTT) departure at the boarding call and arrival at
 * the alighting call -- what the passenger timetable and station
 * screens show, and what the planner searched on (falling back to
 * the working time for a call with none). `scheduled_departure`/
 * `scheduled_arrival` are the same times with that fallback filled
 * in. `None` until
 * `trip_leg_details::attach_leg_details` fills them in, and when the
 * CIF has no public time there (or the schedule predates the
 * column). Local clock time, like `scheduled_*`.
 */
publicDeparture: string | null, publicArrival: string | null, 
/**
 * Days past `service_date` of `public_departure`/`public_arrival`.
 * Usually the WTT time's own offset; differs when rounding crosses
 * midnight (a 23:59H WTT arrival is a 00:00 public one).
 */
publicDepartureDayOffset: number | null, publicArrivalDayOffset: number | null, 
/**
 * The schedule's `BX` ATOC operator code (e.g. `"SW"`), via
 * `schedule_services.operator_atoc` (buses and ferries too, since
 * 2026-10-31). Filled in by
 * `trip_leg_details::attach_leg_details`; `None` when unknown.
 */
operator: string | null, 
/**
 * The schedule's CIF `BS` Train Identity -- the 4-character
 * signalling headcode (e.g. `"1S00"`), via
 * `schedule_destination_departures.headcode`. Filled in by
 * `trip_leg_details::attach_leg_details`; `None` when unknown or
 * when the stored rows disagree. NOT the TRUST 10-char train id.
 */
headcode: string | null, 
/**
 * The live overlay's view of this leg (`data::trip_plan_live`):
 * absent unless the overlay was applied (so `live=false` is the
 * pre-overlay shape exactly), then `null` (nothing known, or
 * outside the live window) or a [`crate::data::trip_plan_live::LegLive`].
 */
live?: LegLive | null | null, serviceMode: ServiceMode, liveTracking: boolean, } | { "kind": "transfer", mode: string, originCrs: string | null, destinationCrs: string | null, minutes: number, 
/**
 * The line ridden (`Victoria`), where it is known (additive,
 * 2026-10-21): the curated link's own line
 * (`crates/common/reference-data/supplementary-fixed-links.csv`), else the one
 * line `originStop` and `destinationStop` share. Absent otherwise.
 * Filled in by `tfl_stops::attach_transfer_lines`. Two transfers
 * that are one ride on one line, carrying on the same way through
 * the stop between them (Euston to Victoria, Victoria to Vauxhall),
 * are served as one leg (2026-10-22, `tfl_stops::merge_transfer_rides`).
 */
line?: string, 
/**
 * For a `TUBE` or `METRO` transfer, the `TfL` stop at each end
 * (`{id, name, modes, lines}`; additive, 2026-10-20, docs/superpowers/
 * specs/2026-10-20-tfl-stop-lookup-design.md §5), so a client saving
 * the transfer as an untracked leg can send its `originStopId` and
 * place name. Absent when unknown. Filled in by
 * `tfl_stops::attach_transfer_stops`.
 */
originStop?: TflStopSummary, destinationStop?: TflStopSummary, 
/**
 * Present only on an access or egress leg between a `TfL` stop end
 * and a National Rail station (additive, 2026-10-23, DS-135): which
 * end, how the station was found and how far the leg goes. Its
 * `minutes` are an estimate. Filled in by
 * `tfl_access::AccessEnds::finish_legs`.
 */
access?: AccessLeg, 
/**
 * Present only on a walk between a National Rail station and a
 * transit network's tram stop (additive, 2026-10-24, DS-164): the
 * stop, and the walk's own estimated minutes (`minutes` also
 * holds the tram-side change time). The tram end's `*Crs` is then
 * `null`. Filled in by `transit_planning` dressing.
 */
transitWalk?: TransitWalk, 
/**
 * Present only on a Tube hop between two National Rail stations
 * naming both its Tube stations (additive, 2026-11-02): the walks
 * between each station and its Tube station that `minutes`
 * include (`originMinutes`, `destinationMinutes`), so the ride is
 * the rest. Filled in by `tube_link_walks::attach_station_walks`.
 */
stationWalks?: StationWalks, 
/**
 * Who runs the link (additive, 2026-10-31): `LT` for a Tube link
 * or ride, `ds:DLR` and so on (planner operator filtering spec
 * §6.5). Absent for a walk or when no operator is known.
 */
operator?: string, 
/**
 * `true` for an estimated walk standing in for the Tube, which only
 * a plan whose operator filter rules the Tube out uses (additive,
 * 2026-10-31, spec §5.1). Absent otherwise.
 */
estimatedWalk?: boolean, };

/**
 * `JourneyStop.platformBasis` (DS-127): what the stop's `displayPlatform`
 * rests on. Serialized lowercase; `null` when there is no platform at
 * all. Clients must treat an unknown value as `board`.
 */
export type PlatformBasis = "booked" | "board" | "confirmed";

/**
 * `JourneyStop.platformStatus`: what the stop's `platform` means for a
 * passenger. Serialized lowercase (`"active"`, `"cancelled"`); `null` when
 * there is no platform.
 *
 * A string enum rather than a `platformCancelled` boolean so that further
 * states (e.g. "provisional" before Darwin confirms an allocation) can be
 * added without another field; clients should render an unknown value as
 * `active`. "Changed" is deliberately not a state: `platformChanged` is
 * orthogonal (a cancelled call can also have changed platform) and is
 * already served.
 */
export type PlatformStatus = "active" | "cancelled";

/**
 * A train's `predictedStart` (spec section 7). Additive and nullable on
 * the wire: absent knowledge is `null`, never a guess of "on time".
 */
export type PredictedStart = { 
/**
 * Minutes late T is expected to start; `None` at low confidence.
 */
lateMinutes: number | null, confidence: Confidence, 
/**
 * Always [`BASIS`].
 */
basis: string, computedAt: string, previousWorking: PreviousWorking, };

/**
 * `GET /Trips/plan`'s `preferOperatorResult`:
 * `trip_planning_itinerary::PREFER_OUTCOMES`.
 */
export type PreferOperatorResult = "none" | "tooSlow" | "labelled" | "added";

/**
 * `predictedStart.previousWorking`: the incoming train the prediction
 * rests on.
 */
export type PreviousWorking = { trainUid: string, serviceDate: string, 
/**
 * [`LINK_INFERRED`] (the platform sequence) or [`LINK_ASSOCIATION`]
 * (Darwin's live next-working link).
 */
link: string, 
/**
 * P's `train_current_state.status` (`en_route`, `completed`,
 * `awaiting_activation`), `None` without a state row.
 */
status: string | null, 
/**
 * P's current TRUST delay, against the working timetable.
 */
delayMinutes: number | null, 
/**
 * When P's state was last reported.
 */
observedAt: string | null, bookedTurnaroundMinutes: number, };

/**
 * `GET /Train/by-uid/{uid}/{date}`'s body: the [`PublicTrainState`]
 * fields, plus `provisional`/`provisionalFrom` for the service date
 * (2026-10-08, see `routes::provisional`). Flattened, so the existing
 * fields keep their names and place.
 *
 * [`PublicTrainState`]: crate::data::trains::PublicTrainState
 */
export type PublicTrainResponse = { 
/**
 * The SHARED `trains` row's own surrogate key, serialized as
 * `trainsId`. Deliberately NOT named `id`, which is what this struct
 * used to call it: every `/Train/{trackingId}` route in this app
 * interprets its path id as a `train_subscriptions.id`, a completely
 * different `BIGSERIAL` space that also starts at 1 -- and the
 * frontend page for this route was feeding this field straight into
 * `RenameTrainButton`/`DeleteTrainButton`/`TicketPanel` as a
 * `trackingId`, so a logged-in visitor could rename or delete an
 * unrelated subscription of their OWN that happened to share the
 * number. Ownership scoping made cross-user damage impossible, but not
 * same-user damage. The name is the fix on this side; the frontend no
 * longer renders those controls on this page at all (see
 * `frontend/app/train/[uid]/[date]/page.tsx`).
 */
trainsId: number, trainUid: string, serviceDate: string, originCrs: string | null, originName: string | null, destinationCrs: string | null, destinationName: string | null, scheduledDeparture: string | null, callingPoints: JsonValue | null, 
/**
 * TRUST's 10-character movement-feed train id (`trains.train_id`, e.g.
 * `"721S00MF25"`) -- set on TRUST activation. NOT the headcode, even
 * though it embeds one; see `headcode` below for that.
 */
trainId: string | null, 
/**
 * The CIF `BS` Train Identity -- the 4-character signalling headcode
 * (e.g. `"1S00"`) -- read from the published schedule
 * (`schedule_destination_departures.headcode`), not from TRUST. `None`
 * when no row carries one or the rows disagree.
 */
headcode: string | null, status: JourneyStatus | null, lastReportedLocation: string | null, lastEventType: string | null, 
/**
 * The delay at the passenger's own stop, against the PUBLIC timetable
 * (`data::stop_delay`, design doc §9 decision 2): a public train page has no stop of the passenger's own, so this is the delay at the latest call the train reported at, else (nothing reported at a call yet) TRUST's running delay with `delay_basis: working`. `None` until
 * known.
 */
delayMinutes: number | null, 
/**
 * What `delay_minutes` was measured against (`public`, `publicSchedule`
 * or `working`, see `common::public_delay::DelayBasis`); `None` exactly
 * when `delay_minutes` is.
 */
delayBasis: DelayBasis | null, 
/**
 * `true` while `delay_minutes` is a forecast (the train has not reported
 * at that stop yet).
 */
delayProvisional: boolean, nextCallingPoint: string | null, etaNext: string | null, etaSource: EtaSource | null, 
/**
 * See `train_tracking::TrackedTrainState::journey_stops`'s doc
 * comment -- same contract, populated the same "read row, then
 * overlay" way by `routes::train::get_by_uid_and_date`. This struct
 * already carries `trains_id` on the wire (unlike `TrackedTrainState`,
 * where it's an internal-only addition), so no extra field is needed
 * to know which `trains_id` to key the overlay query on. `#[sqlx(skip)]`,
 * not `#[sqlx(default)]` -- see
 * `train_tracking::TrackedTrainState::journey_stops`'s doc comment for
 * why: `JourneyStop` doesn't implement `sqlx::Type`/`Decode`, and
 * `#[sqlx(default)]`'s generated code still needs that bound even
 * though this column is never selected.
 */
journeyStops: Array<JourneyStop> | null, 
/**
 * See `train_tracking::TrackedTrainState::may_have_arrived`'s own doc
 * comment -- same contract, populated the same "read row, then
 * overlay" way by `routes::train::attach_journey_stops_public`.
 */
mayHaveArrived: boolean, 
/**
 * A predicted late start from the incoming train (DS-127,
 * `ds_store::trains::predicted_start`), served as `predictedStart`:
 * `null` unless the train has no TRUST report yet, is within 3 hours
 * of departure, has a previous working whose delay the booked
 * turnaround cannot absorb, and the origin's board shows no time of
 * its own. Filled by `routes::train::attach_journey_stops(_public)`.
 */
predictedStart: PredictedStart | null, 
/**
 * The operating company's ATOC code (for example `"SW"`), from the CIF
 * schedule. Serialized as `operatorCode`. Filled after the read by
 * `data::train_operator`, hence `#[sqlx(skip)]`. `None` when no single
 * code is known; see that module's doc for when that happens.
 */
operatorCode: string | null, 
/**
 * The `tocs` display name for `operator_code` (for example
 * `"South Western Railway"`). Serialized as `operatorName`. `None` when
 * `operator_code` is `None` or the code has no `tocs` row.
 */
operatorName: string | null, 
/**
 * Whether the train is cancelled: `status == "cancelled"`. Filled
 * after the read by `data::train_reasons`, like every field below.
 */
cancelled: boolean, 
/**
 * The TRUST cancellation reason code (e.g. `"TG"`), only while
 * `cancelled` is true.
 */
cancelReasonCode: string | null, 
/**
 * `cancel_reason_code`'s delay attribution glossary text (e.g.
 * `"Driver"`). `None` for a code the glossary lacks or a system code
 * (`PD`, `ZW`). There is no delay-reason equivalent: TRUST carries none.
 */
cancelReason: string | null, 
/**
 * The TRUST change-of-origin reason code, when the train's origin was
 * changed.
 */
changeOfOriginReasonCode: string | null, 
/**
 * Its glossary text, under the same rules as `cancel_reason`.
 */
changeOfOriginReason: string | null, 
/**
 * What part of the run is cancelled and why (DS-181): full or partial,
 * the cancelled sections, terminated short or started short, and the
 * best reason with its source. `null` when nothing is cancelled,
 * including after a reinstatement. Filled with the fields above; see
 * `data::cancellation`. The older `cancelled`/`cancelReason*` fields
 * stay for compatibility.
 */
cancellation: Cancellation | null, serviceMode: ServiceMode, liveTracking: boolean, provisional: boolean, provisionalFrom: string, trackingPhase: TrackingPhase | null, firstReportExpectedAt: string | null, };

/**
 * Where a cancellation's reason came from; see the module doc for the
 * precedence.
 */
export type ReasonSource = "darwin" | "ldbws" | "trust";

/**
 * One item of `GET /public/incidents/recently-cleared` (2026-10-12): an
 * incident National Rail has cleared but still lists.
 */
export type RecentlyClearedIncident = { incidentId: string, summary: string, description: string, operators: Array<string>, affectedLines: Array<IncidentLineRef>, clearedAt: string, lastListedAt: string, 
/**
 * The end time the last live text stated, if any.
 */
expectedUntil: string | null, 
/**
 * The last live text said "expected until the end of the day".
 */
expectedUntilEndOfDay: boolean, };

/**
 * `GET /public/incidents/recently-cleared`'s body.
 */
export type RecentlyClearedResponse = { results: Array<RecentlyClearedIncident>, };

export type RenameResponse = { 
/**
 * The normalized value actually stored -- `None` if the name was
 * cleared, `Some(trimmed)` otherwise. Never echoes back an
 * un-normalized value the caller sent.
 */
customName: string | null, };

/**
 * What a replacement answers (`POST .../replan/accept`, `.../replace`,
 * `.../revert`), and what an `Idempotency-Key` replays.
 */
export type ReplaceResponse = { journeyId: number, 
/**
 * The revision now current (1 when revision 1 was refreshed).
 */
revisionNo: number, newLegIds: Array<number>, 
/**
 * The revision an Undo goes back to; `None` when the change refreshed
 * the booked plan (nothing to undo to).
 */
undoRevisionNo: number | null, };

/**
 * One ranked alternative.
 */
export type ReplanSuggestion = { 
/**
 * Stable within its set (`s1`, `s2`, ...): what an accept names.
 */
id: string, departure: string, arrival: string, changeCount: number, 
/**
 * Minutes after the booked arrival at the same destination; `None`
 * when the booked arrival is unknown or the destination differs.
 */
delayMinutes: number | null, 
/**
 * The last current leg this alternative replaces.
 */
throughLegId: number, 
/**
 * The later booked leg it gets you to in time for, kept (§5.5).
 */
rejoinsLegId: number | null, legs: Array<ReplanSuggestionLeg>, cost: number, fit: Fit | null, fitLabel: string | null, fitReason: string | null, picks: Array<Pick>, pickLabels: Array<string>, 
/**
 * One of the (at most three) cards; the rest are "More options".
 */
card: boolean, 
/**
 * The best route on only the journey's preferred operators
 * (additive, 2026-10-31): their codes. Absent otherwise.
 */
operatorOnly?: Array<string>, 
/**
 * The journey's operator filter left no suggestion, so this one uses
 * other operators (additive, 2026-10-31). Absent when `false`.
 */
outsideOperatorFilter?: boolean, };

/**
 * One leg of an alternative: a train to track, or a Tube ride, bus or
 * walk saved as an untracked leg (as `POST /Journeys/itinerary` takes it).
 */
export type ReplanSuggestionLeg = { "kind": "train", trainUid: string, serviceDate: string, originCrs: string | null, originName: string | null, destinationCrs: string | null, destinationName: string | null, 
/**
 * Live (else timetabled) times at the leg's two calls.
 */
departure: string, arrival: string, 
/**
 * Timetabled (public) times, for "+N min".
 */
scheduledDeparture: string, scheduledArrival: string, operator: string | null, operatorName: string | null, platform: string | null, cancelled: boolean, 
/**
 * Set when the change onto this train works only because it is
 * running late (additive, DS-141): it could leave earlier. Absent
 * otherwise, and from a set saved before.
 */
reliesOnLateDeparture?: LateDepartureReliance<string>, } | { "kind": "untracked", transitMode: TransitMode, originName: string | null, destinationName: string | null, lineName: string | null, originStopId: string | null, destinationStopId: string | null, plannedDeparture: string, plannedArrival: string, };

/**
 * `train_subscriptions.resolution_status` (its CHECK constraint).
 */
export type ResolutionStatus = "pending" | "schedule_matched" | "resolved" | "unresolved";

/**
 * How `GET /public/trains/resolve` matched: `train_resolve::MatchedOn`.
 */
export type ResolveMatchedOn = "rsid" | "rsidPrefix" | "timetable";

/**
 * `journey_revisions.actor` (its CHECK constraint).
 */
export type RevisionActor = "owner" | "system";

/**
 * `journey_revisions.cause` (its CHECK constraint): a break's causes
 * ([`BreakCause`]) and `user_choice`.
 */
export type RevisionCause = "cancelled" | "part_cancelled" | "connection_missed" | "user_missed" | "user_choice" | "skipped_stop";

/**
 * `journey_revisions.fault` (its CHECK constraint): a break's faults
 * ([`BreakFault`]) and `none`.
 */
export type RevisionFault = "operator" | "user" | "none" | "unknown";

/**
 * One revision leg as served by `GET /Journeys/{id}/revisions`.
 */
export type RevisionLeg = { position: number, sourceLegId: number | null, kind: JourneyLegKind, trainUid: string | null, serviceDate: string, atocCode: string | null, originCrs: string | null, originName: string | null, destinationCrs: string | null, destinationName: string | null, bookedDeparture: string | null, bookedArrival: string | null, transitMode: TransitMode | null, originPlace: string | null, destinationPlace: string | null, lineName: string | null, plannedDeparture: string | null, plannedArrival: string | null, ticketIds: Array<number>, fate: RevisionLegFate, 
/**
 * An untracked leg's stops and network ride (DS-237, additive), so
 * going back to this plan keeps them; `null` otherwise and on a
 * revision written before them.
 */
originStopId: string | null, destinationStopId: string | null, transitNetwork: string | null, transitLineId: string | null, headsign: string | null, };

/**
 * `journey_revision_legs.fate` (its CHECK constraint).
 */
export type RevisionLegFate = "planned" | "travelled" | "missed" | "cancelled" | "part_cancelled" | "not_used";

/**
 * `journey_revisions.reason` (its CHECK constraint).
 */
export type RevisionReason = "booked" | "replace" | "auto_shift" | "revert";

/**
 * One plan as `GET /Journeys/{id}/revisions` serves it.
 */
export type RevisionResponse = { 
/**
 * Whether the plan can be gone back to ([`revert_allowed`]); never
 * the current one.
 */
canRevert: boolean, revisionNo: number, createdAt: string, reason: RevisionReason, cause: RevisionCause | null, fault: RevisionFault | null, breakId: number | null, actor: RevisionActor, isCompensationBaseline: boolean, legs: Array<RevisionLeg>, };

/**
 * `GET /Journeys/{id}/revisions`'s body: every plan, newest first.
 */
export type RevisionsResponse = { revisions: Array<RevisionResponse>, };

/**
 * The body both role-changing routes return: the member whose role
 * changed, and the role they now hold. One struct rather than one per
 * direction -- `promote` and `demote` are exact inverses and their
 * responses were byte-identical in shape, so a second copy would only be
 * a thing to keep in sync.
 */
export type RoleChangeResponse = { userId: string, role: GroupRole, };

/**
 * Why `sampleStats` is (or isn't) there: `common::SampleAvailability`
 * without its stats, which `sampleStats` already carries.
 */
export type SampleAvailability = { "state": "no-coverage" } | { "state": "below-threshold", observed: number, required: number, } | { "state": "available" };

/**
 * A status's sample: `common::SampleStats` on the wire.
 */
export type SampleStats = { total: number, delayed: number, cancelled: number, skipped: number, avgDelayMinutes: number, };

/**
 * One row of `GET /public/stations/{crs}/schedule-departures`
 * ([`schedule_departure_json`]): the timetabled departure, the train's true
 * origin and its live state and service mode, which the route fills in.
 */
export type ScheduleDepartureRow = { 
/**
 * `null` only for a stored row without one, which `schedule-reference`
 * never writes: typed as always set.
 */
uid: string, 
/**
 * `null` only for a stored row with no departure time, which a
 * departure row always has: typed as always set.
 */
scheduled: string, publicDeparture: string | null, workingDeparture: string | null, workingDayOffset: number, dayOffset: number, destinationCrs: string | null, destinationName: string | null, originCrs: string | null, originName: string | null, live: LineTrainSummaryLive | null, serviceMode: ServiceMode, liveTracking: boolean, };

/**
 * A time on the plan's clock: `time` (`HH:MM:SS`) and how many days after
 * `date` it falls (`dayOffset`).
 */
export type SegmentClock = { time: string, dayOffset: number, };

/**
 * What kind of vehicle a schedule is. The stored spelling is
 * [`Self::as_db_str`] (`replacement_bus`), the wire spelling camelCase
 * (`replacementBus`).
 */
export type ServiceMode = "train" | "replacementBus" | "bus" | "ferry";

/**
 * The unlisted share link `unlisted_links::UnlistedLink` mapped onto the
 * wire -- deliberately its own type rather than reusing `UnlistedLink`
 * directly, so this route's response shape doesn't change if that
 * module's internal shape ever does.
 */
export type ShareLinkResponse = { 
/**
 * `Some` ONLY in the response to the `POST .../share-link` that just
 * minted it. Tokens are stored hashed (2026-09-26 review, L14,
 * `unlisted_links::hash_link_token`), so `GET /Journeys/{id}` can say
 * that an active link exists and when it expires, but can never show
 * its URL again -- the owner regenerates to get a fresh copyable one.
 */
token: string | null, expiresAt: string | null, };

/**
 * One custom line granted into one group the CALLER is a member of --
 * [`GroupCustomLine`] plus the two fields that only make sense once rows
 * from several groups land in one list (`group_id`/`group_name`, the
 * "from <group>" tag the home page renders).
 */
export type SharedCustomLine = { groupId: string, groupName: string, lineId: string, lineName: string, grantedBy: string, grantedByName: string | null, 
/**
 * Same contract as `GroupCustomLine.granted_by_tag`.
 */
grantedByTag: string | null, };

/**
 * One journey shared into one group the CALLER is a member of --
 * [`GroupJourney`] plus the two fields that only make sense once rows
 * from several groups land in one list, mirroring [`SharedTrain`]
 * exactly. Not consumed by any frontend page in this phase -- see this
 * feature's plan, Judgment Call 4 -- but built now for parity with
 * `group_trains`'s own route/function set, per spec §6.
 */
export type SharedJourney = { groupId: string, groupName: string, journeyId: number, customName: string | null, legCount: number, pinOriginCrs: string | null, pinDestinationCrs: string | null, pinOriginName: string | null, pinDestinationName: string | null, pinScheduledDeparture: string | null, 
/**
 * The journey's own ends, for naming it: the first leg's planned
 * origin and the LAST leg's planned destination (`journey_legs`, not
 * the first leg's train pin above, which is empty until that leg is
 * matched and names only that leg). Names via `stations`, `None` when
 * unresolved. Additive (2026-10-20).
 */
firstOriginCrs: string | null, firstOriginName: string | null, lastDestinationCrs: string | null, lastDestinationName: string | null, serviceDate: string, resolutionStatus: ResolutionStatus | null, trainUid: string | null, status: JourneyStatus | null, 
/**
 * The delay at the sharer's own stop (their pin destination), against
 * the public timetable; see `TrackedTrainState::delay_minutes`.
 */
delayMinutes: number | null, 
/**
 * See `TrackedTrainState::delay_basis`.
 */
delayBasis: DelayBasis | null, 
/**
 * See `TrackedTrainState::delay_provisional`.
 */
delayProvisional: boolean, addedBy: string, addedByName: string | null, addedByTag: string | null, 
/**
 * The summarised leg's `trackingPhase` (see
 * `TrackedTrainState::tracking_phase`); `None` for an open or
 * untracked leg. Additive (2026-10-20).
 */
trackingPhase: TrackingPhase | null, 
/**
 * See `TrackedTrainState::first_report_expected_at`.
 */
firstReportExpectedAt: string | null, 
/**
 * The summarised leg's kind: `train` or `untracked`. The row
 * summarises the journey's first train leg, else its first leg, so
 * `untracked` means the journey has no train legs. Additive
 * (2026-10-20).
 */
kind: JourneyLegKind, 
/**
 * The untracked leg's mode (`tube`, `walk`, ...); `None` on a train
 * leg.
 */
transitMode: TransitMode | null, 
/**
 * The untracked leg's planned times; `None` on a train leg.
 */
plannedDeparture: string | null, plannedArrival: string | null, 
/**
 * The untracked leg's clock-only state; `None` on a train leg.
 */
untrackedStatus: UntrackedStatus | null, 
/**
 * Whether the CALLER follows this journey (DS-146,
 * `journey_follows`); set by the route, `false` here.
 */
following: boolean, serviceMode?: ServiceMode, liveTracking?: boolean, };

/**
 * One train shared into one group the CALLER is a member of -- `GroupTrain`
 * (the per-group `GET /groups/{id}/trains` shape) plus the two fields that
 * only make sense once rows from several groups land in one list:
 * `group_id`/`group_name`, the "from <group>" attribution
 * `/track/mine` tags each row with.
 *
 * Inherits `GroupTrain`'s hard privacy constraint verbatim (spec §4's
 * "Never shown" list): no ticket field, no `notificationsEnabled`, no
 * exact `trackedAt`. This shape is strictly MORE exposed than
 * `GroupTrain` -- it reaches a member without them opening the group at
 * all -- so no future edit may widen it past what the group detail page
 * already shows the same member.
 */
export type SharedTrain = { groupId: string, groupName: string, trainSubscriptionId: number, pinOriginCrs: string | null, pinDestinationCrs: string | null, pinOriginName: string | null, pinDestinationName: string | null, pinScheduledDeparture: string | null, serviceDate: string, resolutionStatus: ResolutionStatus, trainUid: string | null, status: JourneyStatus | null, 
/**
 * The delay at the sharer's own stop (their pin destination), against
 * the public timetable; see `TrackedTrainState::delay_minutes`.
 */
delayMinutes: number | null, 
/**
 * See `TrackedTrainState::delay_basis`.
 */
delayBasis: DelayBasis | null, 
/**
 * See `TrackedTrainState::delay_provisional`.
 */
delayProvisional: boolean, customName: string | null, addedBy: string, addedByName: string | null, 
/**
 * Same contract as `GroupTrain.added_by_tag`.
 */
addedByTag: string | null, 
/**
 * See `TrackedTrainState::tracking_phase`. Derived from the
 * subscription's resolution and live status and public timetable data
 * only, so it widens nothing past the "Never shown" list. Additive
 * (2026-10-20).
 */
trackingPhase: TrackingPhase | null, 
/**
 * See `TrackedTrainState::first_report_expected_at`.
 */
firstReportExpectedAt: string | null, serviceMode: ServiceMode, liveTracking: boolean, };

/**
 * One group a journey reached the caller through (`GET /Journeys/mine`'s
 * `sharedVia`, DS-178): the same attribution a group's own shared row
 * shows (`GroupJourney.addedByName`/`addedByTag`), and no more.
 */
export type SharedVia = { groupId: string, groupName: string, 
/**
 * The sharer's display name (`users::MemberDisplay`); `None` when no
 * name is on file, then `added_by_tag` is set.
 */
addedByName: string | null, addedByTag: string | null, addedAt: string, };

/**
 * Which signal(s) support a [`StopStatus::Skipped`] verdict -- carried as
 * a SIBLING field on [`JourneyStop`] (`skip_source`), never nested inside
 * `StopStatus` itself. That mirrors this app's existing "surface
 * provenance as its own field, never collapse it into the primary value"
 * convention -- `EtaBadge.tsx`'s `etaSource` badge, shown alongside (never
 * instead of) the ETA it qualifies, is the precedent this follows -- so a
 * `Skipped` stop's `stop_status` always serializes as the same flat
 * string regardless of source, and a consumer that only cares "was this
 * stop skipped" never has to pattern-match a nested value to find out.
 *
 * The two sources are not equally trustworthy, and this type exists so
 * that difference is never silently flattened away:
 *
 * - `Darwin` is Darwin/LDBWS's own explicit per-calling-point
 *   `isCancelled` flag for this specific service
 *   (`common::StationDeparture.skipped_stations`) -- the operator's own
 *   timetable system stating outright that this call will not happen
 *   today. Treated as authoritative.
 * - `Trust` is inferred purely from a reported TRUST `PASS` event at a
 *   booked stop -- real running data, but an INFERENCE about what a
 *   `PASS` message means for a public calling point, not a first-party
 *   "this call was withdrawn" signal (see
 *   docs/superpowers/specs/2026-09-04-option-b-live-consumer-design.md's
 *   still-open PASS-mapping caveat). A consumer must word this more
 *   softly than the `Darwin` case.
 * - `Both` is the two signals independently agreeing -- as confident as
 *   `Darwin` alone, just worth surfacing that TRUST's own running data
 *   corroborates it.
 */
export type SkipSource = "Darwin" | "Trust" | "Both";

/**
 * `GET /public/stations/{crs}/accessibility`: the station's RDM
 * accessibility JSON filtered to `reference::ACCESSIBILITY_KEYS` (each
 * present only when the feed published it, non-null), every value the
 * feed's own JSON, untouched.
 */
export type StationAccessibility = { stationAccessibility?: JsonValue, staffAssistance?: JsonValue, toiletsAndChanging?: JsonValue, lifts?: JsonValue, transportLinks?: JsonValue, cycling?: JsonValue, carParks?: JsonValue, dropOffPickUp?: JsonValue, platformFacilities?: JsonValue, stationFacilities?: JsonValue, helpAndSupport?: JsonValue, loungesAndWaiting?: JsonValue, };

/**
 * `GET /public/stations/codes`'s body.
 */
export type StationCodes = { codes: Array<string>, };

/**
 * One row of `GET /public/stations/{crs}/departures`: a live board row,
 * every absent value a present `null`. `scheduled`/`estimated` are LDBWS's
 * own strings.
 */
export type StationDeparture = { serviceId: string, operator: string, destinationCrs: string, destinationName: string | null, scheduled: string, estimated: string, isCancelled: boolean, delayMinutes: number, cancelReason: string | null, delayReason: string | null, skippedStations: Array<string>, platform: string | null, plannedPlatform: string | null, platformChanged: boolean, rsid: string | null, callingPoints: Array<BoardCallingPoint>, };

/**
 * One operator's row of `GET /public/stations/{crs}/sample-stats`.
 * `sampleAvailability` is never `no-coverage` here (the route's own
 * invariant); `fullCoverageAvailability` is always present.
 */
export type StationOperatorSampleStats = { operator: string, sampleAvailability: SampleAvailability, sampleStats?: SampleStats, fullCoverageStats?: SampleStats, fullCoverageAvailability: FullCoverageAvailability, };

/**
 * The walks a Tube transfer leg's `minutes` include, at each end
 * (additive, 2026-11-02).
 */
export type StationWalks = { 
/**
 * From the National Rail station to the Tube station.
 */
originMinutes: number, 
/**
 * From the Tube station to the National Rail station.
 */
destinationMinutes: number, };

/**
 * One end of a plan that is a `TfL` stop, and its candidates.
 */
export type StopAccess = { stop: TflStopSummary, candidates: Array<AccessCandidate>, };

/**
 * `journeyStops[].board`. See the module doc.
 */
export type StopBoard = { 
/**
 * Darwin's passenger delay text, verbatim.
 */
delayReason: string | null, 
/**
 * Darwin's passenger cancellation text, verbatim.
 */
cancelReason: string | null, 
/**
 * The whole service is cancelled at this station.
 */
isCancelled: boolean, 
/**
 * `etd - std` in minutes (0 for early), when `etd` is a time or
 * "On time"; `null` when it is a status word ("Delayed",
 * "Cancelled"), so an unknown delay never reads as on time.
 */
delayMinutes: number | null, 
/**
 * LDBWS `etd`, verbatim: `"HH:MM"` (London local), `"On time"`,
 * `"Delayed"` or `"Cancelled"`.
 */
estimated: string, 
/**
 * When the board was polled (`station_samples.polled_at`).
 */
observedAt: string, };

/**
 * Whether -- and to what degree of confidence -- a stop was actually
 * called at, independent of (but computed from) `actual_arrival`/
 * `actual_departure`/`last_event_type` on [`JourneyStop`]. Exists because
 * those fields alone can no longer safely distinguish, for a genuine
 * booked calling point, "not yet reached" from "the train ran through
 * without calling" -- both now leave `actual_arrival`/`actual_departure`
 * `None` (see `overlay_movement_events`'s own doc comment on its `"PASS"`
 * arm, the fix this type is the planned follow-up to). `apply_stop_status`
 * (below) is the only place this is computed.
 *
 * `Unknown` covers every stop this whole distinction does not apply to at
 * all: an `Origin`/`Terminate` call (no two-sided booked stop to begin
 * with) or an `Intermediate` entry missing a scheduled arrival or
 * departure (a CIF timing point with a blank public time, never a real
 * calling point) -- the exact same `booked_calling_point` gate
 * `overlay_movement_events`'s `"PASS"` arm already uses, so the two can
 * never disagree about which stops this applies to. Whether such a stop
 * was "reached" is still answered by `actual_arrival`/`actual_departure`
 * alone, exactly as before this type existed.
 *
 * Plain `PascalCase` variant names on the wire, no `rename_all` override --
 * matching `schedule_query::CallingPointKind`'s own convention, the field
 * this one sits right next to on every `JourneyStop`
 * (`frontend/lib/types.ts`'s `JourneyStopKind` mirrors it the same way).
 */
export type StopStatus = "Unknown" | "Scheduled" | "Called" | "Skipped";

/**
 * One hourly or six-hourly bucket of sampled stats.
 */
export type SubDailyStats = { bucketStart: string, sampleCycles: number, 
/**
 * The credits the line's data needs (`render::line_attribution`): on
 * the line routes, absent from the operator and network ones.
 */
attribution?: Array<TransitAttribution>, total: number, delayed: number, cancelled: number, skipped: number, avgDelayMinutes: number, delayRate: number, cancellationRate: number, skipRate: number, };

export type Suggestion = { code: string, name: string, 
/**
 * Only on a `?stops=true` station search's bus stops and ferry
 * terminals: `bus` or `ferry`. Absent for a station or operator.
 */
kind?: LocationSuggestionKind, 
/**
 * The stop's parent station, when it has one (same rule as
 * `JourneyStop.parentCrs`).
 */
parentCrs?: string, };

/**
 * The alternatives for one break point, as served and stored.
 */
export type SuggestionSet = { generatedAt: string, 
/**
 * The break's fingerprint when planned (§5.4); empty with no break.
 */
fingerprint: string, 
/**
 * The first leg replaced.
 */
fromLegId: number, originCrs: string, originName: string | null, destinationCrs: string, destinationName: string | null, departAfter: string, 
/**
 * The booked arrival at the destination (the compensation baseline's).
 */
bookedArrival: string | null, 
/**
 * The sheet's one-line ticket hint (§17.5).
 */
hint: string | null, 
/**
 * The leg's train, when it is a train leg.
 */
trigger: TriggerTrain | null, 
/**
 * Cards first, then the rest by cost.
 */
suggestions: Array<ReplanSuggestion>, 
/**
 * Set by the route on read: stored suggestions that need a re-plan.
 */
stale: boolean, };

/**
 * The body of `409 suggestion_stale`: the coded error, with the break's
 * fresh suggestions.
 */
export type SuggestionStaleResponse = { error: "suggestion_stale", message: string, break: BreakResponse, };

/**
 * `journey_templates.default_match_mode` (its CHECK constraint).
 */
export type TemplateMatchMode = "manual" | "auto";

/**
 * One line serving a stop.
 */
export type TflStopLine = { 
/**
 * `TfL`'s line id (`victoria`, `dlr`, `tram`).
 */
id: string, 
/**
 * `TfL`'s line name (`Victoria`).
 */
name: string, };

/**
 * One stop as the api serves it.
 */
export type TflStopSummary = { 
/**
 * The `NaPTAN` id (`940GZZLUEUS`): what a leg stores.
 */
id: string, 
/**
 * `Euston`.
 */
name: string, 
/**
 * `tube`, `dlr`, `tram`.
 */
modes: Array<string>, lines: Array<TflStopLine>, };

export type TicketCreatedResponse = { ticketId: number, };

/**
 * Whether a ticket is a single or a return, as far as its free-text
 * `ticket_type` clearly says.
 */
export type TicketKind = "single" | "return" | "unknown";

/**
 * A user's own tickets, across every tracked train they have -- the
 * cross-train counterpart to `TrackedTrainTicket` (which is scoped to one
 * tracked train). Carries the ticket's own six fields (unchanged from
 * `TrackedTrainTicket`) plus enough of the owning tracked train's context
 * (route, date, live delay) to make a row useful without clicking
 * through, plus a Delay Repay estimate computed inline -- see
 * `build_ticket_list_item` for why that's a pure computation, not a
 * second query per row. The last four fields are deliberately named and
 * shaped to match `DelayRepayEstimateResponse` exactly, field-for-field,
 * so the frontend can pass a `TicketListItem` straight into the
 * already-reviewed `<DelayRepayEstimate>` component with no adapter.
 *
 * `tracked_train_id` and every train-context field
 * (`serviceDate`/`pinOriginCrs`/.../`status`) are `Option` -- all `None`
 * together for a standalone ticket with no tracked train attached yet
 * (see `create_ticket`'s doc comment). `estimate`/`delayMinutes` are
 * already `None` in that case too, by construction: `build_ticket_list_item`
 * only ever computes a real estimate from a `(operator, delay_minutes)`
 * pair, and a standalone row has no `delay_minutes` to pair with. This is
 * the same "graceful `None`, never a crash" behavior
 * `get_delay_repay_estimate`'s route already guarantees for an attached
 * ticket whose train hasn't resolved/reported a delay yet -- a standalone
 * ticket is just the same case with `None` for one more reason. `claimUrl`
 * and `disclaimer` are still always populated, per that same route's
 * invariant.
 */
export type TicketListItem = { id: number, trackedTrainId: number | null, operator: string | null, ticketType: string | null, originCrs: string | null, destinationCrs: string | null, 
/**
 * See `TicketListRow::origin_name`'s doc comment.
 */
originName: string | null, destinationName: string | null, source: TicketSource, createdAt: string, serviceDate: string | null, pinOriginCrs: string | null, pinDestinationCrs: string | null, pinScheduledDeparture: string | null, resolutionStatus: ResolutionStatus | null, trainUid: string | null, status: JourneyStatus | null, customName: string | null, 
/**
 * The delay against the PUBLIC arrival at `measured_at_crs` (design
 * doc §9 decision 3): measured once the train has arrived (or, at a
 * departure-only station, departed), projected before then
 * (`provisional`). `None` when `outcome` is `notReached`.
 */
delayMinutes: number | null, 
/**
 * `true` while `delay_minutes` (and `estimate`) are a projection.
 */
provisional: boolean, 
/**
 * See `common::public_delay::DelayBasis`; `None` exactly when
 * `delay_minutes` is.
 */
delayBasis: DelayBasis | null, 
/**
 * Where the delay was measured: the ticket's destination, else the
 * pin destination, else the terminus. Set when `delay_minutes` is, and
 * when `outcome` is `notReached` (the destination not reached).
 */
measuredAtCrs: string | null, 
/**
 * `measured_at_crs`'s station name, when known.
 */
measuredAtName: string | null, 
/**
 * See [`Outcome`]; `None` while not known yet.
 */
outcome: Outcome | null, 
/**
 * `true` for an operator running its own compensation scheme: no
 * percentage, just the claim link.
 */
ownScheme: boolean, 
/**
 * The scheme's operator, when it is one in [`OPERATORS`] (else `None`:
 * the ticket's own `operator` text is the best name).
 */
schemeOperator: string | null, estimate: DelayRepayEstimate | null, 
/**
 * Always populated, independent of `estimate`: no caller is ever left
 * with nowhere real to go.
 */
claimUrl: string, 
/**
 * Always populated, independent of `estimate`.
 */
disclaimer: string, 
/**
 * [`RULES_CHECKED_ON`].
 */
rulesCheckedOn: string, };

/**
 * `tracked_train_tickets.source` (its CHECK constraint).
 */
export type TicketSource = "manual" | "pkpass-semantics" | "pkpass-heuristic" | "pdf-heuristic";

/**
 * One timetabled call of the ride, from boarding to alighting.
 */
export type TimetabledCall = { 
/**
 * The station, in the network's public spelling.
 */
stopId: string, name: string, 
/**
 * The platform's indicator (`To Bury`), when known.
 */
platform: string | null, 
/**
 * `null` at the boarding call.
 */
scheduledArrival: string | null, 
/**
 * `null` at the alighting call.
 */
scheduledDeparture: string | null, };

/**
 * A ride of a timetabled leg, by its scheduled times.
 */
export type TimetabledRide = { departure: string, arrival: string, };

/**
 * A user's own tracked-train list, lighter than `TrackedTrainState`
 * (Decision 1 of the design spec) -- excludes live movement detail
 * (`train_id`, `last_reported_location`, `last_event_type`,
 * `next_calling_point`, `eta_next`, `eta_source`), which belongs on the
 * single-train detail page, not a multi-row list. `pin_scheduled_departure`
 * is new here -- no other existing route selects it (Finding 4 of the
 * design spec). Lives API-crate-side only, same as `TrackedTrainState`/
 * `TrackedTrainTicket` -- never sent between Rust services, only
 * serialized to JSON for the frontend.
 */
export type TrackedTrainListItem = { id: number, serviceDate: string, 
/**
 * `Option`, not `String` -- see `TrackedTrainState::pin_origin_crs`'s
 * own doc comment for the full reasoning. `GET /Train/mine` returns
 * EVERY one of a user's subscriptions, so a single NR-primary
 * subscription with no schedule data yet used to fail the whole list
 * request, not merely omit itself from it.
 */
pinOriginCrs: string | null, pinDestinationCrs: string | null, 
/**
 * See `TrackedTrainState::pin_origin_name`'s doc comment -- same
 * `LEFT JOIN stations` mechanism, same `None`-means-no-reference-row
 * contract.
 */
pinOriginName: string | null, pinDestinationName: string | null, 
/**
 * `Option` for the same reason as `pin_origin_crs` directly above --
 * the two columns were relaxed together by
 * `20260906130000_nullable_pin_columns.sql` and are written (or not)
 * together by `create_subscription_for_train`.
 */
pinScheduledDeparture: string | null, resolutionStatus: ResolutionStatus, trainUid: string | null, status: JourneyStatus | null, 
/**
 * The delay at the passenger's own stop, against the PUBLIC timetable
 * (`data::stop_delay`, design doc §9 decision 2): see `TrackedTrainState::delay_minutes`. `None` until
 * known.
 */
delayMinutes: number | null, 
/**
 * What `delay_minutes` was measured against (`public`, `publicSchedule`
 * or `working`, see `common::public_delay::DelayBasis`); `None` exactly
 * when `delay_minutes` is.
 */
delayBasis: DelayBasis | null, 
/**
 * `true` while `delay_minutes` is a forecast (the train has not reported
 * at that stop yet).
 */
delayProvisional: boolean, trackedAt: string, customName: string | null, 
/**
 * See `TrackedTrainState::shared_group_count`'s doc comment -- same
 * correlated-subquery mechanism, same reason (letting
 * `/train/[uid]/[date]`'s tracking overlay, which reads this list
 * rather than `GET /Train/{trackingId}`, warn on delete too).
 */
sharedGroupCount: number, 
/**
 * See `TrackedTrainState::tracking_phase`.
 */
trackingPhase: TrackingPhase | null, 
/**
 * See `TrackedTrainState::first_report_expected_at`.
 */
firstReportExpectedAt: string | null, 
/**
 * The caller's journeys this train belongs to, ascending; empty for a
 * train tracked on its own. A journey counts when ANY of its legs (not
 * just the current one) is bound to this subscription, or to another of
 * the caller's subscriptions on the same physical train (`trains_id`):
 * a pin tracked separately that later resolved to a journey leg's train
 * is the same service, shown once, under the journey. `/track/mine`
 * folds these rows into their journey instead of listing them as
 * standalone tracked trains. Additive (2026-10-20).
 */
journeyIds: Array<number>, 
/**
 * Of `journey_ids`, the train's own journeys: the caller's one-leg
 * journeys whose leg is bound to this subscription itself. "Stop
 * tracking" (`DELETE /Train/{trackingId}`) deletes these; a train in
 * any other of `journey_ids` stays tracked there (DS-124). Additive
 * (2026-10-22).
 */
singleLegJourneyIds: Array<number>, 
/**
 * Whether the train is over, by the rule that splits this list's
 * travel order (`travel_order`): `completed`, or it has actually
 * reached its destination. Rows come still-to-come first, so the first
 * `true` is where past trains begin. Additive (2026-10-21).
 */
travelled: boolean, 
/**
 * The list's sort key (DS-178): the departure `pinScheduledDeparture`
 * serves, else the London start of `serviceDate`, so a client merging
 * these rows with `GET /Journeys/mine`'s (whose `departsAt` is the
 * same kind of key) can place them. Additive (2026-10-26).
 */
departsAt: string, 
/**
 * The train's compact cancellation (`data::cancellation`'s list
 * form): full or partial, where it now starts or terminates, and why.
 * `null` when no part of its run is cancelled. Additive (2026-10-27).
 */
cancellation: CancellationSummary | null, serviceMode: ServiceMode, liveTracking: boolean, };

/**
 * The public read-model for a tracked train, returned directly as JSON by
 * `crates/api/src/routes/train.rs`'s `GET /Train/{trackingId}` (via
 * `get_by_tracking_id` below). Unlike `TrackedTrainRow`/`TrackedTrainRef`
 * above (poller-facing, private), this never leaks the raw `user_id`
 * column -- see Task 5's brief for why that read deliberately stays
 * public/unscoped despite `tracked_trains` having a real owner.
 *
 * This module used to have a second `TRACKED_TRAIN_STATE_SELECT` reader,
 * `get_by_uid_and_date`, keyed only on `(train_uid, service_date)` with no
 * `user_id` filter at all. `GET /Train/by-uid/{uid}/{date}` stopped calling
 * it as of Task 19 (it reads `trains::get_public_train_state` instead),
 * which left it with zero callers anywhere in the workspace -- confirmed
 * again by the 19-pass Signal Box Audit's trains-area Low finding. Unlike
 * `get_by_tracking_id`, that query could match more than one
 * `train_subscriptions` row (multiple users can each pin the same
 * `train_uid`/`service_date`), so `fetch_optional` would return whichever
 * one row Postgres happened to pick -- silently handing back that OTHER
 * user's `custom_name`/`shared_group_count` (both genuinely per-user,
 * despite this struct never exposing `user_id` itself) to whatever caller
 * eventually got reconnected to it. Deleted outright rather than fixed,
 * since dead code carrying a latent cross-user leak serves no purpose
 * sitting around waiting to be reconnected.
 */
export type TrackedTrainState = { id: number, serviceDate: string, 
/**
 * `Option`, not `String`. `20260906130000_nullable_pin_columns.sql`
 * dropped this column's `NOT NULL`, and
 * `create_subscription_for_train` (the NR-primary path behind
 * `POST /Train/by-uid/{uid}/{date}/track`) sources it via
 * `INSERT ... SELECT` from the linked `trains` row -- which leaves it
 * `NULL` whenever that row has no schedule data yet. That is the
 * DEFAULT outcome of the new endpoint, not an edge case, so decoding
 * this as a bare `String` made `GET /Train/{trackingId}` fail outright
 * ("unexpected null; try decoding as an Option") for exactly the
 * subscriptions this whole redesign exists to create. See
 * `get_by_tracking_id_returns_a_row_with_null_pins_for_an_nr_primary_subscription`.
 *
 * Read as `COALESCE(pin, trains.origin_crs)` (likewise the destination
 * and `pin_scheduled_departure`): the stored pin is a snapshot taken
 * when the subscription was created, and nothing rewrites it once the
 * shared `trains` row learns its timetable moments later. Without the
 * fallback every train tracked by UID or picked for a journey leg kept
 * a `NULL` origin for good and the home page and journey view labelled
 * it "Unknown station" (DS-111). A pin the user chose still wins.
 */
pinOriginCrs: string | null, pinDestinationCrs: string | null, 
/**
 * `None` whenever the `LEFT JOIN` below found no `stations` row for
 * `pin_origin_crs` (an unrecognised code, or reference data that
 * hasn't caught up) -- see Decision 3 of the plan this join
 * implements. Every frontend consumer must fall back to the bare CRS
 * code rather than assume this is always present.
 */
pinOriginName: string | null, pinDestinationName: string | null, resolutionStatus: ResolutionStatus, trainUid: string | null, trainId: string | null, 
/**
 * The matched schedule's own terminus CRS (Decision 3 step 4 of
 * docs/superpowers/specs/2026-09-05-schedule-first-train-tracking-design.md),
 * `None` until schedule-matched (or if the terminus TIPLOC never
 * resolved to a CRS -- see `schedule_matching::attempt_schedule_match`).
 */
scheduleDestinationCrs: string | null, 
/**
 * See `pin_origin_name`'s own doc comment -- same
 * `LEFT JOIN stations` mechanism, joined on `schedule_destination_crs`.
 */
scheduleDestinationName: string | null, 
/**
 * Opaque JSONB relay of the matched entry's calling points, already
 * camelCase-shaped at write time
 * (`schedule_matching::ScheduleCallingPointDto`) -- this crate does
 * not deserialize it again on the way out.
 */
scheduleCallingPoints: JsonValue | null, status: JourneyStatus | null, lastReportedLocation: string | null, lastEventType: string | null, 
/**
 * The delay at the passenger's own stop, against the PUBLIC timetable
 * (`data::stop_delay`, design doc §9 decision 2): the pin destination, measured once the train has reported there and forecast before then. `None` until
 * known.
 */
delayMinutes: number | null, 
/**
 * What `delay_minutes` was measured against (`public`, `publicSchedule`
 * or `working`, see `common::public_delay::DelayBasis`); `None` exactly
 * when `delay_minutes` is.
 */
delayBasis: DelayBasis | null, 
/**
 * `true` while `delay_minutes` is a forecast (the train has not reported
 * at that stop yet).
 */
delayProvisional: boolean, nextCallingPoint: string | null, etaNext: string | null, etaSource: EtaSource | null, customName: string | null, 
/**
 * How many groups (`crate::data::groups`) this subscription is
 * currently shared into, via `group_trains.train_subscription_id` --
 * `0` if it isn't shared anywhere. Read via a correlated `COUNT(*)`
 * subquery directly in `TRACKED_TRAIN_STATE_SELECT` below, the same
 * "attach a count to the row that owns it" shape
 * `groups::GroupSummary`/`groups::GroupDetail` already use for their
 * own `member_count` (`SELECT ... (SELECT COUNT(*) FROM group_members
 * gm2 WHERE gm2.group_id = g.id) AS member_count ...`) -- deliberately
 * NOT a separate `count_groups_containing_train` query invoked per row
 * from a route handler, which would turn `GET /Train/mine` (a
 * potentially-many-row list) into an N+1. Exists so the frontend's
 * delete-confirmation modal (`DeleteTrainButton`) can warn "this train
 * is shared in N group(s)" without a second round-trip.
 *
 * Since DS-124 a train is shared through its own journey
 * (`group_journeys` on a one-leg journey bound to this subscription)
 * as well as through a remaining `group_trains` row: this counts the
 * distinct groups across both ([`shared_group_count_sql!`]).
 */
sharedGroupCount: number, 
/**
 * The caller's journeys this train belongs to (any leg), and of
 * those, the train's own: one-leg journeys whose leg is bound to this
 * subscription, which "Stop tracking" deletes (DS-124). Filled only by
 * the owner's `GET /Train/{trackingId}` (`attach_journey_ids`), never
 * on a journey's or a group's view of the train, which must not show
 * the owner's other journeys; absent from the JSON otherwise.
 * Additive (2026-10-22).
 */
journeyIds?: Array<number>, 
/**
 * See `journey_ids`.
 */
singleLegJourneyIds?: Array<number>, 
/**
 * Whether this subscription's delay, cancellation and skipped-stop
 * pushes are on (`train_subscriptions.notifications_enabled`, set by
 * `PATCH /Train/{trackingId}`). Owner's view only, like `journey_ids`;
 * absent from the JSON otherwise. Additive (2026-10-23).
 */
notificationsEnabled?: boolean, 
/**
 * The merged scheduled-timetable + live-overlay stop list -- `None`
 * until `train_uid` is known, or if neither backing source has
 * anything for this train (see
 * docs/superpowers/specs/2026-09-08-journey-timetable-overlay-design.md
 * §1). Populated by `routes::train`'s handlers AFTER this struct is
 * read from the DB (same "read row, then overlay a computed field"
 * pattern `blend_darwin_eta` already uses for `eta_next`/`eta_source`
 * on this same struct) -- never selected directly by
 * `TRACKED_TRAIN_STATE_SELECT`, hence `#[sqlx(skip)]`. NOT
 * `#[sqlx(default)]`: the derived `FromRow` impl still emits a
 * `try_get::<#ty, _>(..)` call for a `#[sqlx(default)]` field and only
 * swallows the resulting `ColumnNotFound` error at runtime, so it
 * still requires `JourneyStop: sqlx::Type<Postgres> +
 * sqlx::Decode<Postgres>` at compile time -- a bound this
 * never-decoded, pure computed/serialization type deliberately
 * doesn't satisfy. `#[sqlx(skip)]` instead emits a bare
 * `Default::default()` with no such bound, which is what "this column
 * is never in the SELECT" actually needs.
 */
journeyStops: Array<JourneyStop> | null, 
/**
 * Server-side replacement for the frontend's old client-only "may have
 * finished" heuristic -- `true` once now is more than 15 minutes past
 * the ESTIMATED arrival at `journey_stops`'s final calling point (see
 * `journey::may_have_arrived`). Populated the same "read row, then
 * overlay a computed field" way as `journey_stops` itself, by
 * `routes::train::attach_journey_stops`, hence `#[sqlx(skip)]` here
 * too -- defaults to `false` (never a stale true) whenever there are
 * no `journey_stops` to compute it from at all (pending/unresolved, or
 * neither backing schedule source had anything).
 */
mayHaveArrived: boolean, 
/**
 * A predicted late start from the incoming train (DS-127,
 * `ds_store::trains::predicted_start`), served as `predictedStart`:
 * `null` unless the train has no TRUST report yet, is within 3 hours
 * of departure, has a previous working whose delay the booked
 * turnaround cannot absorb, and the origin's board shows no time of
 * its own. Filled by `routes::train::attach_journey_stops(_public)`.
 */
predictedStart: PredictedStart | null, 
/**
 * The operating company's ATOC code (for example `"SW"`), from the CIF
 * schedule. Serialized as `operatorCode`. Filled after the read by
 * `data::train_operator`, hence `#[sqlx(skip)]`. `None` when no single
 * code is known; see that module's doc for when that happens.
 */
operatorCode: string | null, 
/**
 * The `tocs` display name for `operator_code` (for example
 * `"South Western Railway"`). Serialized as `operatorName`. `None` when
 * `operator_code` is `None` or the code has no `tocs` row.
 */
operatorName: string | null, 
/**
 * Whether the train is cancelled: `status == "cancelled"`. Filled
 * after the read by `data::train_reasons`, like every field below.
 */
cancelled: boolean, 
/**
 * The TRUST cancellation reason code (e.g. `"TG"`), only while
 * `cancelled` is true.
 */
cancelReasonCode: string | null, 
/**
 * `cancel_reason_code`'s delay attribution glossary text (e.g.
 * `"Driver"`). `None` for a code the glossary lacks or a system code
 * (`PD`, `ZW`). There is no delay-reason equivalent: TRUST carries none.
 */
cancelReason: string | null, 
/**
 * The TRUST change-of-origin reason code, when the train's origin was
 * changed.
 */
changeOfOriginReasonCode: string | null, 
/**
 * Its glossary text, under the same rules as `cancel_reason`.
 */
changeOfOriginReason: string | null, 
/**
 * What part of the run is cancelled and why (DS-181): full or partial,
 * the cancelled sections, terminated short or started short, and the
 * best reason with its source. `null` when nothing is cancelled,
 * including after a reinstatement. Filled with the fields above; see
 * `data::cancellation`. The older `cancelled`/`cancelReason*` fields
 * stay for compatibility.
 */
cancellation: Cancellation | null, 
/**
 * Planned, waiting for its first report, live, not in the timetable
 * or timetable-only (`data::tracking_phase`, spec 2026-10-20 §4.3).
 * Filled after the read by `tracking_phase::attach`; `None` (`null`)
 * when that lookup did not run or failed, which reads as today's
 * display.
 */
trackingPhase: TrackingPhase | null, 
/**
 * When Network Rail's first report is (or was) expected: the
 * Activation before it is activated, the origin departure after.
 * `None` once live, or when the origin departure is unknown.
 */
firstReportExpectedAt: string | null, serviceMode: ServiceMode, liveTracking: boolean, };

/**
 * The public read-model for a ticket, returned directly as JSON by
 * `GET /Train/{trackingId}/tickets` (Task 3). Never leaks `user_id` --
 * same posture as `TrackedTrainState`. `tracked_train_id` is `Option<i64>`
 * -- `None` for a standalone ticket that hasn't been attached to a tracked
 * train yet (see `create_ticket`'s doc comment); every row this struct's
 * own `list_tickets_for_tracked_train` returns is guaranteed non-null by
 * that query's own `WHERE tracked_train_id = $1` filter (a NULL column
 * value can never equal a bound `i64`), but `get_ticket_owned` (used by
 * the Delay Repay route and the attach route) has no such filter and can
 * legitimately return `None` here.
 */
export type TrackedTrainTicket = { id: number, trackedTrainId: number | null, operator: string | null, ticketType: string | null, originCrs: string | null, destinationCrs: string | null, 
/**
 * See `TrackedTrainState::pin_origin_name`'s doc comment -- same
 * `LEFT JOIN stations` mechanism, joined here on the TICKET's own
 * `origin_crs`/`destination_crs`, not the pin route (which
 * `TrackedTrainListItem` already covers).
 */
originName: string | null, destinationName: string | null, 
/**
 * See `ticket_extraction::PartialTicket::current_departure_date`'s own
 * doc comment -- this is that same best-effort HINT, persisted
 * verbatim by `create_ticket`. `GET /Train/tickets/{ticketId}/journey-leg-proposal`
 * (`data::journey_leg_proposal::propose_window_leg`) is the one reader
 * that does anything with it; every other existing reader of this
 * struct (`TicketPanel.tsx`'s rendering, the Delay Repay route) simply
 * ignores it, same as any other field it doesn't need.
 */
currentDepartureDate: string | null, source: TicketSource, createdAt: string, customName: string | null, };

/**
 * The traveller-facing phase of a tracked train (spec §4.3), evaluated in
 * declaration order.
 */
export type TrackingPhase = "timetableOnly" | "live" | "noTimetable" | "planned" | "awaitingReport";

/**
 * One row of `GET /public/trains/arrivals`: the search's row plus the
 * arrival at the station. For a train terminating there, `scheduled`,
 * `publicDeparture` and `workingDeparture` are `null` (there is no
 * departure onward) and `dayOffset` is absent.
 */
export type TrainArrivalResult = { 
/**
 * `"HH:MM"`, the public arrival, else the working one.
 */
arrival: string, arrivalDayOffset: number, terminates: boolean, 
/**
 * The row's service date: the searched date, or with
 * `across_midnight` the day before.
 */
serviceDate: string, originName: string | null, live: LineTrainSummaryLive | null, 
/**
 * Days after `serviceDate` that `scheduled` falls on. Absent for a
 * train terminating at an arrivals board's station.
 */
dayOffset?: number, 
/**
 * `workingDeparture`'s day offset (the search only).
 */
workingDayOffset?: number, 
/**
 * The search with `stops_at`: the arrival there (public, `HH:MM`),
 * its day offset, and the same for the working arrival. `null`
 * together when the call has no such time.
 */
stopsAtArrival?: string | null, stopsAtArrivalDayOffset?: number | null, stopsAtWorkingArrival?: string | null, stopsAtWorkingArrivalDayOffset?: number | null, 
/**
 * `null` only for a stored row without one, which `schedule-reference`
 * never writes: typed as always set.
 */
uid: string, scheduled: string | null, stationCrs: string, originCrs: string | null, destinationCrs: string | null, destinationName: string | null, destinationArrival: string | null, destinationArrivalDayOffset: number, publicDeparture: string | null, publicDestinationArrival: string | null, workingDeparture: string | null, workingDestinationArrival: string | null, 
/**
 * The operator's ATOC code.
 */
operator: string | null, 
/**
 * The CIF Train Identity (headcode, e.g. `1S00`).
 */
identity: string | null, serviceMode: ServiceMode, liveTracking: boolean, };

/**
 * `GET /public/trains/arrivals`'s body.
 */
export type TrainArrivalsPage = { results: Array<TrainArrivalResult>, nextCursor: string | null, serviceDate: string, window: TrainArrivalsWindow, provisional: boolean, provisionalFrom: string, };

/**
 * The arrivals board's `window` as applied: `to` is `null` when
 * unbounded.
 */
export type TrainArrivalsWindow = { from: WindowBound, to: WindowBound | null, };

/**
 * `GET /public/trains/by-headcode`'s body.
 */
export type TrainHeadcodePage = { results: Array<TrainSearchResult>, serviceDate: string, provisional: boolean, provisionalFrom: string, };

/**
 * `GET /public/trains/resolve`'s 200 body: the live board row's train, as
 * `GET /Train/by-uid/{uid}/{date}` (`href`) keys it.
 */
export type TrainResolveResult = { href: string, trainUid: string, serviceDate: string, matchedOn: ResolveMatchedOn, };

/**
 * `GET /public/trains/search/dates`'s body.
 */
export type TrainSearchDates = { from: string, to: string, publishedFrom: string | null, publishedTo: string | null, provisionalFrom: string, };

/**
 * An operator the search's `operator` can name; `name` is `null` for a
 * code `tocs` lacks.
 */
export type TrainSearchOperator = { code: string, name: string | null, };

/**
 * `GET /public/trains/search/operators`'s body.
 */
export type TrainSearchOperators = { operators: Array<TrainSearchOperator>, };

/**
 * `GET /public/trains/search`'s body.
 */
export type TrainSearchPage = { results: Array<TrainSearchResult>, nextCursor: string | null, serviceDate: string, window: TrainSearchWindow, acrossMidnight: boolean, serviceDates: Array<string>, provisional: boolean, provisionalFrom: string, };

/**
 * One row of the train search, arrivals and headcode boards
 * (`routes::trains`): the calling-point departure and what every board
 * adds to it. The board-specific fields are absent elsewhere.
 */
export type TrainSearchResult = { 
/**
 * The row's service date: the searched date, or with
 * `across_midnight` the day before.
 */
serviceDate: string, originName: string | null, live: LineTrainSummaryLive | null, 
/**
 * Days after `serviceDate` that `scheduled` falls on. Absent for a
 * train terminating at an arrivals board's station.
 */
dayOffset?: number, 
/**
 * `workingDeparture`'s day offset (the search only).
 */
workingDayOffset?: number, 
/**
 * The search with `stops_at`: the arrival there (public, `HH:MM`),
 * its day offset, and the same for the working arrival. `null`
 * together when the call has no such time.
 */
stopsAtArrival?: string | null, stopsAtArrivalDayOffset?: number | null, stopsAtWorkingArrival?: string | null, stopsAtWorkingArrivalDayOffset?: number | null, 
/**
 * `null` only for a stored row without one, which `schedule-reference`
 * never writes: typed as always set.
 */
uid: string, scheduled: string | null, stationCrs: string, originCrs: string | null, destinationCrs: string | null, destinationName: string | null, destinationArrival: string | null, destinationArrivalDayOffset: number, publicDeparture: string | null, publicDestinationArrival: string | null, workingDeparture: string | null, workingDestinationArrival: string | null, 
/**
 * The operator's ATOC code.
 */
operator: string | null, 
/**
 * The CIF Train Identity (headcode, e.g. `1S00`).
 */
identity: string | null, serviceMode: ServiceMode, liveTracking: boolean, };

/**
 * The search's `window`: `from`/`to` bound the departure from `station`,
 * `arrivalFrom`/`arrivalTo` the arrival at `stops_at`; `null` where
 * unbounded.
 */
export type TrainSearchWindow = { from: WindowBound, to: WindowBound | null, arrivalFrom: WindowBound | null, arrivalTo: WindowBound | null, };

/**
 * The licence notice a network's data needs (spec §12.3).
 */
export type TransitAttribution = { 
/**
 * SPDX-style id (`OGL-UK-3.0`).
 */
licence: string, text: string, };

/**
 * A `transit` leg's TypeScript, named `TransitLeg`: `TransitLeg`'s own
 * derive takes its name from its tag value, `transit`.
 */
export type TransitLeg = { "kind": "transit", network: string, serviceMode: TransitServiceMode, liveTracking: boolean, realtime: boolean, serviceDate: string, scheduledDeparture: string, scheduledArrival: string, departureDayOffset: number, arrivalDayOffset: number, line: TransitLegLine | null, headsign: string | null, originCrs: string | null, destinationCrs: string | null, originStop: TransitLegStop, destinationStop: TransitLegStop, 
/**
 * The median gap between departures of the same line and direction
 * from the boarding platform in the hour around this one; absent with
 * fewer than three ([`headway_minutes`]).
 */
headwayMinutes?: number, notices: Array<TransitLegNotice>, };

/**
 * A transit leg's `line`: `null` for a replacement bus.
 */
export type TransitLegLine = { 
/**
 * The public id, `metrolink:blue`.
 */
id: string, name: string, colour: string | null, };

/**
 * A notice on a transit leg (spec §7.3): a current disruption on its line.
 */
export type TransitLegNotice = { 
/**
 * `disruption`.
 */
kind: string, lineId: string, 
/**
 * A `common::Severity` number.
 */
severity: number, 
/**
 * The severity's name (`Minor Delays`).
 */
description: string, 
/**
 * "Disruption reported on the Blue Line".
 */
message: string, 
/**
 * The situation's text.
 */
reason: string, 
/**
 * The situation behind it, for its page (`/transit/{network}/incidents/{id}`);
 * absent for a status from `TfL`, whose line page has the detail
 * (additive, DS-237).
 */
situationId?: string, };

/**
 * A transit leg's `originStop` or `destinationStop`.
 */
export type TransitLegStop = { 
/**
 * The station, in the network's public spelling.
 */
id: string, name: string, 
/**
 * The platform's indicator (`To Bury`), when known.
 */
platform: string | null, };

/**
 * An untracked leg's `transit_mode`: `data::journeys::TRANSIT_MODES`,
 * checked on every write.
 */
export type TransitMode = "tube" | "dlr" | "bus" | "tram" | "metro" | "rail" | "walk" | "transfer" | "ferry" | "taxi" | "cycle" | "other";

/**
 * One of `GET /Trips/plan`'s `transitNotices` (spec §7.3): a network left
 * out of the plan.
 */
export type TransitPlanNotice = { network: string, 
/**
 * Always `noTimetable` so far.
 */
reason: string, message: string, };

/**
 * A transit leg's `serviceMode`.
 */
export type TransitServiceMode = "tram" | "replacementBus" | "tube" | "dlr";

/**
 * On a rail-link walk (`kind: "transfer"`, `mode: "WALK"`): the tram stop
 * at one end and the estimated walk alone (Q2). The transfer's own
 * `minutes` also hold the tram-side change buffer.
 */
export type TransitWalk = { network: string, stop: TransitWalkStop, 
/**
 * Walking from the station to the tram stop.
 */
towardsStop: boolean, walkMinutes: number, 
/**
 * Always `true`: worked out from coordinates.
 */
estimated: boolean, };

export type TransitWalkStop = { id: string, name: string, 
/**
 * A `TfL` station's `tfl_stops` modes (`tube`, `dlr`, `tram`), so a
 * client can say which kind of station the walk reaches (DS-239);
 * left out when none are known (another network's stop).
 */
modes?: Array<string>, };

/**
 * The train whose leg no longer works, as the sheet and the push name it
 * ("the 10:16 to Charing Cross").
 */
export type TriggerTrain = { legId: number, 
/**
 * Its timetabled departure from the leg's boarding station.
 */
departure: string | null, originName: string | null, 
/**
 * The leg's own alighting station.
 */
destinationName: string | null, 
/**
 * The train's own destination (what a train is named by).
 */
trainDestinationName: string | null, };

/**
 * One whole journey across every segment.
 */
export type TripPlanJourney = { changeCount: number, 
/**
 * Additive (2026-10-30): changes within `TfL` access legs, not in
 * `changeCount`.
 */
accessChangeCount: number, departure: SegmentClock, arrival: SegmentClock, totalDurationMinutes: number, exceedsRecommendedChanges?: boolean, liveFeasible?: boolean, 
/**
 * Additive (2026-10-30, DS-196), `results=options` only: what this
 * option is (`fastest` or `leavesLatest`, `fewestChanges`,
 * `alternative` with its `viaCrs`).
 */
labels?: Array<OptionLabel>, 
/**
 * Additive (2026-10-06), with `via` only: for each via in order, the
 * leg that passed it.
 */
viaSatisfiedBy?: Array<ViaSatisfied>, 
/**
 * Additive (2026-10-07), only when a waypoint is an OR choice: for
 * each waypoint in order, where the journey stopped for it.
 */
waypointSatisfiedBy?: Array<WaypointSatisfied>, };

/**
 * The live overlay's outcome: `applied` with what it did, or why not
 * (`reason`: `disabled`, `outsideLiveWindow`, ...). The counts are present
 * only when it applied.
 */
export type TripPlanLive = { applied: boolean, reason: string | null, replans?: number, trainsRead?: number, adjustedTrains?: number, inSearch?: boolean, snapshotTrains?: number, };

/**
 * `GET /Trips/plan`'s body. Fields marked additive were added after the
 * first release, on the date given; an optional one is absent, not
 * `null`, when its condition does not hold.
 */
export type TripPlanResponse = { 
/**
 * `fastest` or `options`, as asked.
 */
results: TripPlanResults, maxChanges: number, 
/**
 * Additive (2026-09-29): the arrive-by deadline, `null` for a
 * depart-after request.
 */
arriveBy: SegmentClock | null, 
/**
 * Additive (2026-10-19): the effective depart-after start (00:00 when
 * none was given), `null` for an arrive-by request.
 */
departAfter: SegmentClock | null, 
/**
 * Additive (2026-10-20): the service dates whose trains were searched:
 * `[date]`, or `[date, date + 1]` with `acrossMidnight`. Every time in
 * this body is on `date`'s clock; each train leg also names its own
 * `serviceDate`.
 */
serviceDates: Array<string>, 
/**
 * Additive (2026-09-29): the avoid lists as applied.
 */
avoid: Array<string>, avoidStop: Array<string>, avoidChange: Array<string>, 
/**
 * Additive (2026-10-06): the pass-through vias as applied.
 */
via: Array<string>, 
/**
 * Additive (2026-10-07): each station group the request named
 * (`group:NAME` in `via`, `waypoints` or an avoid list) to its member
 * CRS codes, as expanded. `{}` when none.
 */
stationGroups: { [key in string]: Array<string> }, 
/**
 * Additive (2026-10-31): the operator filter as applied, each list
 * expanded to its operators (`[]` when unused), the groups it named
 * (`{}` when none), and the chosen operators that run nothing on
 * `date`.
 */
operator: Array<string>, avoidOperator: Array<string>, preferOperator: Array<string>, operatorGroups: { [key in string]: Array<string> }, operatorsNotRunning: Array<string>, segments: Array<TripPlanSegment>, 
/**
 * Additive (2026-09-29): the whole journeys, aligned with every
 * segment's `itineraries` (journey `j` is `itineraries[j]` of each
 * segment), with their end-to-end change count.
 */
journeys: Array<TripPlanJourney>, 
/**
 * With `live` asked for (the default): whether the live overlay
 * applied, and what it did.
 */
live?: TripPlanLive, 
/**
 * Additive (2026-10-31), with `preferOperator` only: what the extra
 * search did (`added`, `labelled`, `tooSlow` or `none`).
 */
preferOperatorResult?: PreferOperatorResult, 
/**
 * Additive (2026-10-23, DS-135), only when an end is a `TfL` stop:
 * each stop end with its access candidates (`null` for a station end).
 */
access?: AccessResponse, 
/**
 * Additive (2026-10-24, DS-164), only when planned with transit
 * networks: each network whose timetable does not cover the date, so
 * the plan was made without it.
 */
transitNotices?: Array<TransitPlanNotice>, provisional: boolean, provisionalFrom: string, };

/**
 * `GET /Trips/plan`'s `results`, checked by `plan_trip_request`.
 */
export type TripPlanResults = "fastest" | "options";

/**
 * One segment of a plan: origin to the first waypoint, waypoint to
 * waypoint, or the last waypoint to the destination.
 */
export type TripPlanSegment = { originCrs: string, destinationCrs: string, itineraries: Array<PlannedItinerary>, cappedByMaxChanges: boolean, 
/**
 * Additive (2026-09-28): when this segment was searched from:
 * `departAfter` for the first, the previous segment's arrival plus the
 * waypoint's change time for later ones; `null` when the previous
 * segment found nothing to chain from, and for an arrive-by request.
 */
departAfter: SegmentClock | null, 
/**
 * Additive (2026-09-29), arrive-by only: the latest arrival this
 * segment was searched for; `null` otherwise.
 */
arriveBy: SegmentClock | null, 
/**
 * Additive (2026-09-29): why `itineraries` is empty; `null` when it is
 * not.
 */
noResultReason: NoResultReason | null, };

/**
 * An untracked leg's state, from the clock alone (spec §2.2).
 */
export type UntrackedStatus = "planned" | "inProgress" | "done";

/**
 * One of a line's `upcoming` notes (2026-10-06): a disruption announced
 * for the line that has not started yet. `to` is exclusive and may be
 * `null` (no stated end); both RFC 3339 (`+00:00`).
 */
export type UpcomingDisruption = { from: string, to: string | null, summary: string, incidentId: string, };

/**
 * A status's validity window, times RFC 3339 (`+00:00`).
 */
export type ValidityPeriod = { fromDate: string, toDate: string | null, isNow: boolean, };

/**
 * How a trip plan passed a via (`ViaSatisfied::how`, from
 * `trip_planner::ViaHow`).
 */
export type ViaHow = "call" | "pass" | "walk";

/**
 * Which leg passed one via: served per journey as
 * `journeys[j].viaSatisfiedBy[k]` for the request's `via[k]`.
 */
export type ViaSatisfied = { 
/**
 * The via, as requested (`KGX|EUS` or `group:LON` for an OR choice).
 */
crs: string, 
/**
 * The station that satisfied it (2026-10-07): for an OR choice, the
 * member the journey passed; otherwise the via itself. A CRS, or a
 * `tiploc:` code for a bus stop or ferry terminal; `null` only if the
 * `TIPLOC` has no code (not expected).
 */
matchedCrs: string | null, 
/**
 * Index into `segments` (the part of the journey).
 */
segment: number, 
/**
 * Index into that segment's `itineraries[j].legs`.
 */
leg: number, 
/**
 * `call` (the train called there: the traveller stayed aboard, boarded
 * or alighted), `pass` (it ran through without calling, or past a
 * call the live overlay cancelled) or `walk` (a transfer leg into it).
 */
how: ViaHow, };

/**
 * Where a trip plan stopped for a waypoint choice
 * (`WaypointSatisfied::how`).
 */
export type WaypointHow = "call" | "walk" | "origin" | "destination";

/**
 * Where a journey stopped for one requested waypoint (2026-10-07): served
 * as `journeys[j].waypointSatisfiedBy[k]` for `waypoints[k]` when any
 * waypoint is an OR choice.
 */
export type WaypointSatisfied = { 
/**
 * The waypoint as requested (`KGX`, `KGX|EUS`, `group:LON`).
 */
crs: string, 
/**
 * The station the journey stopped at for it: the member, for a choice.
 * `null` only if its `TIPLOC` has no code (not expected).
 */
matchedCrs: string | null, 
/**
 * The segment that ends there (the next one starts there). For one
 * satisfied at the origin, 0 (which starts there); at the destination,
 * the last segment.
 */
segment: number, 
/**
 * `call` (a train took the traveller there: they alighted, or stayed
 * aboard when the next segment `continuesPreviousTrain`), `walk` (a
 * transfer leg into it), `origin` or `destination` (an end of the trip
 * is a member; see [`waypoints_at_ends`]).
 */
how: WaypointHow, };

/**
 * One end of a board's window as applied: `"HH:MM"` on day `dayOffset`
 * of the train day.
 */
export type WindowBound = { time: string, dayOffset: number, };
