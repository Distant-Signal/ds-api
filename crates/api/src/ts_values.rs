//! The value sets of string fields the api serves from `TEXT` columns or
//! string constants, for the generated TypeScript only (`api_types`, repo-split
//! spec §5.3). A field opts in with `#[ts(as = "...")]`; these enums are
//! never built, and serde never sees them. Each names where its values are
//! enforced, so a new value is added here in the same change.

use ts_rs::TS;

/// `train_subscriptions.resolution_status` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum ResolutionStatus {
    Pending,
    ScheduleMatched,
    Resolved,
    Unresolved,
}

/// `train_current_state.status` (its CHECK constraint): a train's own
/// status, served as `status` on train and journey responses.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum JourneyStatus {
    AwaitingActivation,
    EnRoute,
    Cancelled,
    Completed,
}

/// Where an estimate came from: `train_current_state.eta_source` (its
/// CHECK constraint) and `ds_store::trains::predicted_start::BASIS`.
#[derive(TS)]
#[ts(rename_all = "kebab-case")]
pub enum EtaSource {
    TrustPropagated,
    DarwinEstimated,
    PreviousWorking,
}

/// `journey_legs.kind` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum JourneyLegKind {
    Train,
    Untracked,
}

/// `journey_legs.match_mode` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum MatchMode {
    Unmatched,
    Manual,
    Auto,
}

/// An untracked leg's `transit_mode`: `data::journeys::TRANSIT_MODES`,
/// checked on every write.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum TransitMode {
    Tube,
    Dlr,
    Bus,
    Tram,
    Metro,
    Rail,
    Walk,
    Transfer,
    Ferry,
    Taxi,
    Cycle,
    Other,
}

/// `journeys.operator_filter_mode` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum OperatorMode {
    Only,
    Avoid,
    Prefer,
}

/// `journey_breaks.cause` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum BreakCause {
    Cancelled,
    PartCancelled,
    ConnectionMissed,
    UserMissed,
    SkippedStop,
}

/// `journey_breaks.fault` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum BreakFault {
    Operator,
    User,
    Unknown,
}

/// `GET /Trips/plan`'s `results`, checked by `plan_trip_request`.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum TripPlanResults {
    Fastest,
    Options,
}

/// `GET /Trips/plan`'s `preferOperatorResult`:
/// `trip_planning_itinerary::PREFER_OUTCOMES`.
#[derive(TS)]
#[ts(rename_all = "camelCase")]
pub enum PreferOperatorResult {
    None,
    TooSlow,
    Labelled,
    Added,
}

/// How a trip plan passed a via (`ViaSatisfied::how`, from
/// `trip_planner::ViaHow`).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum ViaHow {
    Call,
    Pass,
    Walk,
}

/// Where a trip plan stopped for a waypoint choice
/// (`WaypointSatisfied::how`).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum WaypointHow {
    Call,
    Walk,
    Origin,
    Destination,
}

/// `journey_breaks.status` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum BreakStatus {
    Open,
    Accepted,
    Dismissed,
    Recovered,
    Expired,
}

/// `journey_breaks.opened_via` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum BreakOpenedVia {
    Push,
    InApp,
}

/// `journey_revisions.reason` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum RevisionReason {
    Booked,
    Replace,
    AutoShift,
    Revert,
}

/// `journey_revisions.cause` (its CHECK constraint): a break's causes
/// ([`BreakCause`]) and `user_choice`.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum RevisionCause {
    Cancelled,
    PartCancelled,
    ConnectionMissed,
    UserMissed,
    UserChoice,
    SkippedStop,
}

/// `journey_revisions.fault` (its CHECK constraint): a break's faults
/// ([`BreakFault`]) and `none`.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum RevisionFault {
    Operator,
    User,
    None,
    Unknown,
}

/// `journey_revisions.actor` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum RevisionActor {
    Owner,
    System,
}

/// `journey_revision_legs.fate` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum RevisionLegFate {
    Planned,
    Travelled,
    Missed,
    Cancelled,
    PartCancelled,
    NotUsed,
}

/// `tracked_train_tickets.source` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "kebab-case")]
pub enum TicketSource {
    Manual,
    PkpassSemantics,
    PkpassHeuristic,
    PdfHeuristic,
}

/// A Delay Repay estimate's `scheme`: `delay_repay_rules::Scheme::as_str`
/// of every scheme with bands (an own scheme has no estimate).
#[derive(TS)]
pub enum DelayRepayScheme {
    #[ts(rename = "DR15")]
    Dr15,
    #[ts(rename = "DR30")]
    Dr30,
    #[ts(rename = "HX")]
    HeathrowExpress,
}

/// A Delay Repay estimate's `fareBasis`: `delay_repay_rules::estimate_delay_repay`.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum FareBasis {
    Single,
    Return,
    Ticket,
}

/// `journey_templates.default_match_mode` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum TemplateMatchMode {
    Manual,
    Auto,
}

/// `journey_templates.auto_commit_rule` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum AutoCommitRule {
    Earliest,
    NearestToNow,
}
