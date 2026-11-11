//! The value sets of string fields the api serves from `TEXT` columns or
//! string constants, for the generated TypeScript only (`api_types`, repo-split
//! spec §5.3). A field opts in with `#[ts(as = "...")]`; these enums are
//! never built, and serde never sees them. The ones a personal-token `/v1`
//! response reaches also derive `schemars::JsonSchema`, and their fields add
//! `#[schemars(with = "...")]`, for the `OpenAPI` document. Each names where its values are
//! enforced, so a new value is added here in the same change.

use ts_rs::TS;

/// `train_subscriptions.resolution_status` (its CHECK constraint).
#[derive(TS, schemars::JsonSchema)]
#[ts(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
pub enum ResolutionStatus {
    Pending,
    ScheduleMatched,
    Resolved,
    Unresolved,
}

/// `train_current_state.status` (its CHECK constraint): a train's own
/// status, served as `status` on train and journey responses.
#[derive(TS, schemars::JsonSchema)]
#[ts(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
pub enum JourneyStatus {
    AwaitingActivation,
    EnRoute,
    Cancelled,
    Completed,
}

/// Where an estimate came from: `train_current_state.eta_source` (its
/// CHECK constraint) and `ds_store::trains::predicted_start::BASIS`.
#[derive(TS, schemars::JsonSchema)]
#[ts(rename_all = "kebab-case")]
#[schemars(rename_all = "kebab-case")]
pub enum EtaSource {
    TrustPropagated,
    DarwinEstimated,
    PreviousWorking,
}

/// `journey_legs.kind` (its CHECK constraint).
#[derive(TS, schemars::JsonSchema)]
#[ts(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
pub enum JourneyLegKind {
    Train,
    Untracked,
}

/// `journey_legs.match_mode` (its CHECK constraint).
#[derive(TS, schemars::JsonSchema)]
#[ts(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
pub enum MatchMode {
    Unmatched,
    Manual,
    Auto,
}

/// An untracked leg's `transit_mode`: `data::journeys::TRANSIT_MODES`,
/// checked on every write.
#[derive(TS, schemars::JsonSchema)]
#[ts(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
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
#[derive(TS, schemars::JsonSchema)]
#[ts(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
pub enum OperatorMode {
    Only,
    Avoid,
    Prefer,
}

/// `journey_breaks.cause` (its CHECK constraint).
#[derive(TS, schemars::JsonSchema)]
#[ts(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
pub enum BreakCause {
    Cancelled,
    PartCancelled,
    ConnectionMissed,
    UserMissed,
    SkippedStop,
}

/// `journey_breaks.fault` (its CHECK constraint).
#[derive(TS, schemars::JsonSchema)]
#[ts(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
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

/// `journey_templates.target_kind` (its CHECK constraint): a regular
/// journey's target.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum TemplateTargetKind {
    DepartAfter,
    ArriveBy,
}

/// `journey_templates.recurrence_kind` as the api accepts it (R2):
/// `weekly` (the weekdays) or `dates` (only the extra dates). `cycle` is
/// kept for shift rotas and not offered.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum RecurrenceKind {
    Weekly,
    Dates,
}

/// Why a regular journey passes over a date (`nextSkips`,
/// `upcomingDates`; `ds_store::recurrence::SkipReason`). When more than
/// one applies: `paused`, then `away`, then `skipped`, then
/// `bank_holiday`. More may follow: show one you don't know as skipped.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum NextSkipReason {
    Paused,
    Away,
    Skipped,
    BankHoliday,
}

/// `users.bank_holiday_division` and `bank_holidays.division` (their CHECK
/// constraints; `ds_store::bank_holidays::DIVISIONS`).
#[derive(TS, schemars::JsonSchema)]
#[ts(rename_all = "kebab-case")]
#[schemars(rename_all = "kebab-case")]
pub enum BankHolidayDivision {
    EnglandAndWales,
    Scotland,
    NorthernIreland,
}

/// `journeys.regular_plan_status` (its CHECK constraint): where a regular
/// journey's day stands (`data::regular_plan`).
#[derive(TS, schemars::JsonSchema)]
#[ts(rename_all = "snake_case")]
#[schemars(rename_all = "snake_case")]
pub enum RegularPlanStatus {
    Pending,
    Suggested,
    Committed,
    NoService,
}

/// `journey_templates.auto_commit_rule` (its CHECK constraint).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum AutoCommitRule {
    Earliest,
    NearestToNow,
}

/// Where an incident stands: `queries::IncidentState::as_str`.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum IncidentState {
    Active,
    Upcoming,
    Ended,
    Cleared,
}

/// A line train's `scope`: `schedule_line_population`'s membership
/// (`data::queries` population entries; line membership design).
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum LineTrainScope {
    Line,
    Shared,
    Touch,
}

/// A line train's `direction` by the line's catalogue order.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum LineDirection {
    Up,
    Down,
    Loop,
}

/// A schedule calling point's `kind`, as `schedule-query` writes it.
#[derive(TS)]
pub enum CallingPointKind {
    Origin,
    Intermediate,
    Terminate,
}

/// One calling point of a `schedule_line_population` entry, served
/// verbatim as a line train's `callingPoints`: `schedule-query`'s
/// `records::CallingPoint` serialization (snake case, times `HH:MM:SS`),
/// which the api never parses. A field added there is added here.
#[derive(TS)]
pub struct LineTrainCallingPoint {
    pub tiploc: String,
    pub kind: CallingPointKind,
    pub booked_arrival: Option<String>,
    pub booked_departure: Option<String>,
    pub is_half_minute_arrival: bool,
    pub is_half_minute_departure: bool,
    pub day_offset: u32,
    /// CIF's packed activity codes.
    pub activity: String,
    pub public_arrival: Option<String>,
    pub public_departure: Option<String>,
    pub platform: Option<String>,
    /// A passing point's booked pass; absent at a call.
    #[ts(optional)]
    pub booked_pass: Option<String>,
}

/// How `GET /public/trains/resolve` matched: `train_resolve::MatchedOn`.
#[derive(TS)]
#[ts(rename_all = "camelCase")]
pub enum ResolveMatchedOn {
    Rsid,
    RsidPrefix,
    Timetable,
}

/// A non-station location suggestion's `kind`:
/// `reference::Suggestion::from_location`.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum LocationSuggestionKind {
    Bus,
    Ferry,
}

/// `GET /public/stations/{crs}/accessibility`: the station's RDM
/// accessibility JSON filtered to `reference::ACCESSIBILITY_KEYS` (each
/// present only when the feed published it, non-null), every value the
/// feed's own JSON, untouched.
#[derive(TS)]
#[ts(rename_all = "camelCase")]
pub struct StationAccessibility {
    #[ts(optional)]
    pub station_accessibility: Option<serde_json::Value>,
    #[ts(optional)]
    pub staff_assistance: Option<serde_json::Value>,
    #[ts(optional)]
    pub toilets_and_changing: Option<serde_json::Value>,
    #[ts(optional)]
    pub lifts: Option<serde_json::Value>,
    #[ts(optional)]
    pub transport_links: Option<serde_json::Value>,
    #[ts(optional)]
    pub cycling: Option<serde_json::Value>,
    #[ts(optional)]
    pub car_parks: Option<serde_json::Value>,
    #[ts(optional)]
    pub drop_off_pick_up: Option<serde_json::Value>,
    #[ts(optional)]
    pub platform_facilities: Option<serde_json::Value>,
    #[ts(optional)]
    pub station_facilities: Option<serde_json::Value>,
    #[ts(optional)]
    pub help_and_support: Option<serde_json::Value>,
    #[ts(optional)]
    pub lounges_and_waiting: Option<serde_json::Value>,
}

/// Where a `GET /public/lines` row comes from: `routes::lines::list_lines`.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum LineSource {
    Catalogue,
    Custom,
    Tfl,
}

/// Where a transit network's line status comes from:
/// `routes::transit::status_source`.
#[derive(TS)]
#[ts(rename_all = "snake_case")]
pub enum TransitStatusSource {
    Tfl,
    Situations,
}

/// `delegated_deletions.summary`: the journey a connected app or (Recently
/// deleted) the user deleted, as `recently_deleted::journey_summary`
/// stores it. Rows written before a field existed lack it.
#[derive(TS)]
#[ts(rename_all = "camelCase", optional_fields = nullable)]
pub struct DeletedJourneySummary {
    pub custom_name: Option<String>,
    pub legs: Option<Vec<DeletedJourneyLeg>>,
}

/// One leg of a [`DeletedJourneySummary`].
#[derive(TS)]
#[ts(rename_all = "camelCase", optional_fields = nullable)]
pub struct DeletedJourneyLeg {
    pub service_date: Option<String>,
    pub origin_crs: Option<String>,
    pub destination_crs: Option<String>,
    pub train_uid: Option<String>,
    pub kind: Option<JourneyLegKind>,
    pub transit_mode: Option<TransitMode>,
    pub origin_name: Option<String>,
    pub destination_name: Option<String>,
    pub line_name: Option<String>,
    pub planned_departure: Option<String>,
    /// Kept since 2026-11-11 (DS-474), for "Save again".
    pub planned_arrival: Option<String>,
    pub origin_stop_id: Option<String>,
    pub destination_stop_id: Option<String>,
}

/// A trip plan train leg's live `status`: `trip_plan_live`'s boarding-call
/// status.
#[derive(TS)]
pub enum LegLiveStatus {
    Cancelled,
    Departed,
    Delayed,
    Late,
    OnTime,
    Scheduled,
}

/// Which constraint made a trip-plan segment infeasible:
/// `trip_planning_itinerary::NoResultReason::constraint`.
#[derive(TS)]
#[ts(rename_all = "camelCase")]
pub enum NoResultConstraint {
    MaxChanges,
    Avoid,
    AvoidStop,
    AvoidChange,
    AvoidCombined,
    Via,
    DepartAfter,
    ArriveBy,
    NoRoute,
    PreviousSegment,
    NextSegment,
    Operator,
    AvoidOperator,
}

/// `common::island_of_ireland::IslandOfIrelandNetwork` as served (kebab
/// case): for the `OpenAPI` document only (the TypeScript spells it out
/// with `#[ts(type = ...)]`).
#[derive(schemars::JsonSchema)]
#[schemars(rename_all = "kebab-case")]
pub enum IslandOfIrelandNetworkWire {
    NorthernIreland,
    RepublicOfIreland,
}
