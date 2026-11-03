//! The TypeScript types the frontend reads this api with (repo-split spec
//! §5.3), generated from the response and request types by `ts-rs`.
//!
//! [`render`] walks every root in [`visit_roots`] and each type it
//! reaches, and writes one declaration per type into a single file,
//! `api-types/api.ts` at the repository root, sorted by name. The frontend
//! keeps a copy at `frontend/lib/api-types.ts` (`scripts/sync-api-types.py`).
//!
//! `api_types_are_current` fails when the checked-in file is stale.
//! Regenerate it with
//!
//! ```sh
//! UPDATE_API_TYPES=1 cargo test -p api api_types
//! uv run scripts/sync-api-types.py write
//! ```
//!
//! A type joins by deriving `ts_rs::TS` next to its `Serialize` or
//! `Deserialize` (types in `common` and `ds-store` under their `ts`
//! feature) and, if nothing already reaches it, by a line in
//! [`visit_roots`]. `i64` fields are `number`, as `serde_json` sends them.

use std::any::TypeId;
use std::collections::BTreeMap;

use ts_rs::{Config, TS, TypeVisitor};

/// The checked-in file, relative to this crate's manifest directory.
pub(crate) const PATH: &str = "../../api-types/api.ts";

const HEADER: &str = "\
// Generated from the api's Rust types by ts-rs (crates/api/src/api_types.rs).
// Do not edit: regenerate with `UPDATE_API_TYPES=1 cargo test -p api api_types`.
";

/// Every type the frontend names directly: response bodies, request
/// bodies and the fields it reads on its own. Their dependencies are found
/// by walking, so they need no line here.
fn visit_roots(v: &mut Collector) {
    // Trains.
    v.visit::<crate::data::train_tracking::TrackedTrainState>();
    v.visit::<crate::data::train_tracking::TrackedTrainListItem>();
    v.visit::<crate::routes::train::PublicTrainResponse>();
    // Journeys.
    v.visit::<crate::routes::journeys::JourneyDetailResponse>();
    v.visit::<crate::data::journeys::JourneyListItem>();
    // Trips.
    v.visit::<crate::routes::trips::TripPlanResponse>();
    // Groups.
    v.visit::<crate::data::groups::GroupSummary>();
    v.visit::<crate::routes::groups::GroupIdentityResponse>();
    v.visit::<crate::routes::groups::GroupDetailResponse>();
    v.visit::<crate::data::groups::GroupMember>();
    v.visit::<crate::routes::groups::RoleChangeResponse>();
    v.visit::<crate::data::groups::InviteLink>();
    v.visit::<crate::data::groups::JoinPreview>();
    v.visit::<crate::routes::groups::JoinResponse>();
    v.visit::<crate::data::groups::GroupTrain>();
    v.visit::<crate::data::groups::SharedTrain>();
    v.visit::<crate::data::groups::GroupJourney>();
    v.visit::<crate::data::groups::SharedJourney>();
    v.visit::<crate::data::groups::GroupCustomLine>();
    v.visit::<crate::data::groups::SharedCustomLine>();
    v.visit::<crate::data::groups::LineGroupRef>();
    // Tickets and Delay Repay.
    v.visit::<crate::data::train_tracking::TrackedTrainTicket>();
    v.visit::<crate::data::train_tracking::TicketListItem>();
    v.visit::<crate::routes::train::TicketCreatedResponse>();
    v.visit::<crate::routes::train::AttachTicketResponse>();
    v.visit::<crate::routes::train::RenameResponse>();
    v.visit::<crate::routes::train::DelayRepayEstimateResponse>();
    v.visit::<crate::data::delay_repay_rules::DelayRepayFields>();
    v.visit::<crate::data::ticket_extraction::PartialTicket>();
    v.visit::<crate::data::journey_leg_proposal::JourneyLegProposal>();
    // Journey templates.
    v.visit::<crate::data::journey_templates::JourneyTemplateListItem>();
    v.visit::<crate::routes::journey_templates::JourneyTemplateDetailResponse>();
    v.visit::<crate::routes::journey_templates::CreateJourneyTemplateResponse>();
    v.visit::<crate::routes::journey_templates::MaterializeTemplateResponse>();
    // Breaks, alternatives and replans.
    v.visit::<crate::data::journey_alternatives::SuggestionSet>();
    v.visit::<crate::routes::journey_replan::BreakResponse>();
    v.visit::<crate::routes::journey_replan::SuggestionStaleResponse>();
    v.visit::<crate::data::journey_replace::ReplaceResponse>();
    v.visit::<crate::routes::journey_replan::RevisionsResponse>();
    // Line status and stats.
    v.visit::<crate::render::LineStatusReportJson>();
    v.visit::<crate::routes::line_status::DailyStats>();
    v.visit::<crate::routes::line_status::HalfHourlyStats>();
    v.visit::<crate::routes::line_status::SubDailyStats>();
    v.visit::<crate::routes::line_status::DailyCoverageStats>();
    v.visit::<crate::routes::line_status::HalfHourlyCoverageStats>();
    v.visit::<crate::routes::operators::OperatorSummaryJson>();
    // Incidents.
    v.visit::<crate::routes::incidents::IncidentDetailJson>();
    v.visit::<crate::routes::incidents::IncidentTimingJson>();
    v.visit::<crate::routes::incidents::IncidentSearchResponse>();
    v.visit::<crate::routes::incidents::RecentlyClearedResponse>();
    // Stations.
    v.visit::<crate::data::reference::Suggestion>();
    v.visit::<crate::data::reference::NearbyStation>();
    v.visit::<crate::routes::reference::StationCodes>();
    v.visit::<crate::ts_values::StationAccessibility>();
    v.visit::<crate::routes::station_stats::StationOperatorSampleStatsJson>();
    v.visit::<crate::render::StationDepartureJson>();
    v.visit::<crate::render::ScheduleDepartureJson>();
    // Lines.
    v.visit::<crate::routes::lines::LineSummary>();
    v.visit::<crate::routes::lines::CustomLineDetail>();
    v.visit::<crate::routes::lines::LineDefinitionSummary>();
    v.visit::<crate::render::LineTrainJson>();
    v.visit::<crate::routes::line_trains_summary::SummaryJson>();
    v.visit::<crate::routes::line_timetable::TimetableJson>();
    // Train search.
    v.visit::<crate::routes::trains::TrainSearchPage>();
    v.visit::<crate::routes::trains::boards::TrainArrivalsPage>();
    v.visit::<crate::routes::trains::boards::TrainHeadcodePage>();
    v.visit::<crate::routes::trains::TrainSearchDates>();
    v.visit::<crate::routes::trains::TrainSearchOperators>();
    v.visit::<crate::routes::trains::TrainResolveResult>();
    // Personal API tokens (Connected apps).
    v.visit::<crate::routes::personal_tokens::PersonalTokensResponse>();
    v.visit::<crate::routes::personal_tokens::CreatePersonalTokenRequest>();
    v.visit::<crate::routes::personal_tokens::CreatedPersonalTokenResponse>();
}

/// Collects one declaration per exportable type, keyed by its TypeScript
/// name.
pub(crate) struct Collector {
    cfg: Config,
    decls: BTreeMap<String, (TypeId, String)>,
}

impl TypeVisitor for Collector {
    fn visit<T: TS + 'static + ?Sized>(&mut self) {
        // Primitives, `Option`, `Vec` and the like have no declaration of
        // their own: walk what they wrap (their generics). Not their
        // `visit_dependencies`, which recurses without coming back here and
        // never ends on a recursive type such as `serde_json::Value`.
        if T::output_path().is_none() {
            T::visit_generics(self);
            return;
        }
        let name = T::ident(&self.cfg);
        let id = TypeId::of::<T::WithoutGenerics>();
        if let Some((seen, _)) = self.decls.get(&name) {
            assert!(
                *seen == id,
                "two Rust types export as `{name}`: give one a `#[ts(rename = \"...\")]`",
            );
            return;
        }
        let mut decl = T::docs().unwrap_or_default();
        decl.push_str("export ");
        decl.push_str(&T::decl(&self.cfg));
        self.decls.insert(name, (id, decl));
        T::visit_dependencies(self);
        T::visit_generics(self);
    }
}

/// The whole file.
pub(crate) fn render() -> String {
    let mut collector = Collector {
        cfg: Config::new().with_large_int("number"),
        decls: BTreeMap::new(),
    };
    visit_roots(&mut collector);
    let mut out = String::from(HEADER);
    for (_, decl) in collector.decls.values() {
        out.push('\n');
        out.push_str(decl);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// `StationAccessibility` describes the accessibility route's
    /// passthrough: one optional field per allowlisted key, and no other.
    #[test]
    fn station_accessibility_names_every_allowlisted_key() {
        use ts_rs::TS;
        let decl = crate::ts_values::StationAccessibility::decl(&ts_rs::Config::new());
        for key in crate::data::reference::ACCESSIBILITY_KEYS {
            assert!(decl.contains(&format!("{key}?:")), "{key} missing: {decl}");
        }
        assert_eq!(
            decl.matches("?:").count(),
            crate::data::reference::ACCESSIBILITY_KEYS.len()
        );
    }

    #[test]
    fn api_types_are_current() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(super::PATH);
        let rendered = super::render();
        if std::env::var_os("UPDATE_API_TYPES").is_some() {
            std::fs::write(&path, &rendered).expect("write api-types/api.ts");
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            committed == rendered,
            "api-types/api.ts is stale: run `UPDATE_API_TYPES=1 cargo test -p api api_types`, \
             then `uv run scripts/sync-api-types.py write`",
        );
    }
}
