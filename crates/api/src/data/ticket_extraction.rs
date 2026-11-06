//! Best-effort, review-before-save prefill for a journey leg's ticket
//! details: reads an Apple Wallet `.pkpass` the user already has and
//! returns a `PartialTicket` preview. This module NEVER writes to the
//! database (the user reviews the prefill and saves it on the leg,
//! `PUT /Journeys/{id}/legs/{legId}/ticket`) and NEVER decodes a barcode or
//! touches ITSO data. PDF e-tickets were read here until 2026-11; that
//! upload was removed with the standalone tickets.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What a `.pkpass` parse could recover, plus a fixed `source` describing
/// which path produced it. `None` means "not found in this file, leave for the user
/// to fill in" -- never guessed at. This is exactly what a human sees on a
/// review-before-save form pre-filled from an upload; nothing here is ever
/// written to `tracked_train_tickets` directly -- see this module's own
/// doc comment.
///
/// `Deserialize` exists only for `data::ticket_subprocess`, whose parent
/// process reads this back from the parse child's JSON reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase")]
pub struct PartialTicket {
    pub operator: Option<String>,
    pub ticket_type: Option<String>,
    /// Best-effort station identifier -- almost never a real CRS code in
    /// practice (a `.pkpass` gives station NAMES, e.g. "Kings Cross").
    /// Informational only: a leg's ticket details take their stations from
    /// the leg.
    pub origin_crs: Option<String>,
    pub destination_crs: Option<String>,
    /// The ticket's own claimed departure INSTANT, when the source format
    /// carries one -- currently only ever populated from a `.pkpass`'s
    /// `semantics.currentDepartureDate` (Apple's standardised key for
    /// exactly this, an ISO 8601 date-time string), and only `Some` when
    /// that key parses as a real `DateTime`. `None` for a `.pkpass` whose
    /// `semantics` dictionary is absent, or present but missing this one
    /// key, or a `primaryFields` heuristic match (that positional fallback
    /// is names only, per `primary_fields_origin_destination`'s own doc
    /// comment).
    ///
    /// A best-effort HINT the leg's ticket form checks against the leg's
    /// own date, never a pin: a ticket's booked departure can differ from
    /// the train the traveller actually catches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub current_departure_date: Option<DateTime<Utc>>,
    #[serde(deserialize_with = "deserialize_known_source")]
    #[ts(as = "common::ts_values::TicketSource")]
    pub source: SourceTag,
}

/// `&'static str` under another name. serde's derive implicitly *borrows*
/// any field spelled `&str` from the input, which for `'static` would demand
/// `'de: 'static` and make `PartialTicket` undeserializable from an owned
/// buffer; spelling it through an alias opts out of that, and
/// `deserialize_known_source` supplies the real value.
pub type SourceTag = &'static str;

/// Every value `PartialTicket::source` is ever set to in this module.
pub const KNOWN_SOURCES: &[&str] = &["pkpass-semantics", "pkpass-heuristic"];

/// `source` is a `&'static str`, so it can only be deserialized by mapping
/// the incoming string back onto one of [`KNOWN_SOURCES`]. Anything else is
/// an error rather than a leaked allocation.
fn deserialize_known_source<'de, D>(deserializer: D) -> Result<&'static str, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    KNOWN_SOURCES
        .iter()
        .copied()
        .find(|known| *known == value)
        .ok_or_else(|| serde::de::Error::custom(format!("unknown ticket source {value:?}")))
}

/// Pure: given `pass.json`'s already-parsed content, returns a
/// `PartialTicket`, preferring Apple's standardised `semantics` dictionary
/// (`departureStationName`/`destinationStationName`) when present, falling
/// back to the positional `primaryFields` convention Apple's own `PassKit`
/// docs specify for a boarding/transit pass (exactly two entries:
/// departure, then arrival, in that order -- positional, not per-issuer
/// label-string matching, since the ordering is Apple's own convention,
/// not each issuer's choice) when it isn't. See
/// docs/superpowers/specs/2026-08-29-journey-ticket-tracking-design.md's
/// Open Question 1: which real UK retailers populate `semantics` is
/// unconfirmed, so both paths are implemented, not just the optimistic
/// one -- obtain 1-2 real sample passes to confirm this split's real-world
/// hit rate before relying on it heavily.
/// `ticket_type` is read from `boardingPass.auxiliaryFields` by exact
/// `key` match (`"ticketType"`) via `keyed_field_value` -- `None` if that
/// key isn't present, never guessed from label text or other fields.
pub fn parse_pass_json(pass: &serde_json::Value) -> anyhow::Result<PartialTicket> {
    let boarding_pass = pass
        .get("boardingPass")
        .ok_or_else(|| anyhow::anyhow!("not a boardingPass-style pkpass"))?;
    let transit_type = boarding_pass
        .get("transitType")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    anyhow::ensure!(
        transit_type == "PKTransitTypeTrain",
        "not a train boarding pass (transitType = {transit_type:?})"
    );

    let operator = pass
        .get("organizationName")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let semantics = boarding_pass.get("semantics");

    let (origin, destination, source) =
        if let Some((origin, destination)) = semantics.and_then(semantics_origin_destination) {
            (Some(origin), Some(destination), "pkpass-semantics")
        } else {
            let (origin, destination) = primary_fields_origin_destination(boarding_pass);
            (origin, destination, "pkpass-heuristic")
        };

    let ticket_type = boarding_pass
        .get("auxiliaryFields")
        .and_then(|fields| keyed_field_value(fields, "ticketType"));

    let current_departure_date = semantics.and_then(semantics_current_departure_date);

    // Diagnostic only -- never surfaced in PartialTicket, the frontend, or
    // any persisted row. debug-level specifically so it costs nothing in
    // default-configured production logging and cannot become a de facto
    // data-collection channel without a deliberate decision to promote it.
    // See Decision 3 of
    // docs/superpowers/specs/2026-09-02-ticket-processing-improvements-design.md.
    tracing::debug!(barcode_format = ?barcode_format(pass), "parsed .pkpass");

    Ok(PartialTicket {
        operator,
        ticket_type,
        origin_crs: origin,
        destination_crs: destination,
        current_departure_date,
        source,
    })
}

fn semantics_origin_destination(semantics: &serde_json::Value) -> Option<(String, String)> {
    let origin = semantics
        .get("departureStationName")
        .and_then(|v| v.as_str())?;
    let destination = semantics
        .get("destinationStationName")
        .and_then(|v| v.as_str())?;
    Some((origin.to_string(), destination.to_string()))
}

/// Reads `semantics.currentDepartureDate` -- Apple's standardised key,
/// confirmed present in Apple's own semantic-tags schema (see this module's
/// doc comment's cross-reference to
/// docs/superpowers/specs/2026-08-29-journey-ticket-tracking-design.md's
/// research section) -- and parses it as an RFC 3339/ISO 8601 date-time.
/// `None` if the key is absent, not a string, or doesn't parse as a real
/// date-time -- same "leave it blank, don't guess" contract as every other
/// optional read in this module; a malformed value is exactly as useless as
/// a missing one, and is never worth surfacing as a parse error for a
/// preview endpoint the user reviews before anything is saved anyway.
fn semantics_current_departure_date(semantics: &serde_json::Value) -> Option<DateTime<Utc>> {
    semantics
        .get("currentDepartureDate")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

/// Apple's `PassKit` docs specify a boarding-pass-style pass's
/// `primaryFields` array holds exactly two entries for a transit pass:
/// departure, then arrival, in that order. Returns `(None, None)` for
/// anything that doesn't match that exact two-field shape, rather than
/// guessing at which field is which.
fn primary_fields_origin_destination(
    boarding_pass: &serde_json::Value,
) -> (Option<String>, Option<String>) {
    let Some(fields) = boarding_pass
        .get("primaryFields")
        .and_then(|v| v.as_array())
    else {
        return (None, None);
    };
    match fields.as_slice() {
        [origin, destination] => (
            origin
                .get("value")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            destination
                .get("value")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        ),
        _ => (None, None),
    }
}

/// Looks up an entry in a `PassKit` field array (`primaryFields`,
/// `auxiliaryFields`, `secondaryFields` -- all the same `{key, label,
/// value}` shape) by its machine-readable `key`, not by its
/// issuer-chosen, freely-reworded `label` text. Returns `None` if `fields`
/// isn't an array, or no entry has that exact key, or the matching
/// entry's `value` isn't a string -- same "leave it blank, don't guess"
/// contract as every other optional read in this module.
fn keyed_field_value(fields: &serde_json::Value, key: &str) -> Option<String> {
    fields
        .as_array()?
        .iter()
        .find(|f| f.get("key").and_then(|v| v.as_str()) == Some(key))?
        .get("value")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Reads only the barcode's `format` string (e.g.
/// `"PKBarcodeFormatAztec"`) from `pass.json`'s singular `"barcode"`
/// object or, per Apple's newer `PassKit` convention, the first entry of
/// the plural `"barcodes"` array -- documented container metadata,
/// structurally no different from `organizationName` or `transitType`,
/// both already read elsewhere in this module. NEVER reads `"message"`,
/// the barcode payload -- that field is categorically off limits, see
/// this module's own doc comment and
/// docs/superpowers/specs/2026-09-02-ticket-processing-improvements-design.md's
/// Explicitly out of scope section.
fn barcode_format(pass: &serde_json::Value) -> Option<String> {
    pass.get("barcode")
        .or_else(|| {
            pass.get("barcodes")
                .and_then(|b| b.as_array())
                .and_then(|a| a.first())
        })
        .and_then(|b| b.get("format"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

use std::io::Read;

/// `pass.json` is plain-text JSON, and real ones are a few KB -- this
/// bounds every ZIP-entry read in this function against a zip-bomb-style
/// small-file/huge-decompressed-content mismatch (see this plan's Global
/// Constraints on file upload hygiene).
pub(crate) const MAX_ENTRY_BYTES: u64 = 1_000_000; // 1 MiB

/// Thin wrapper: unzips the `.pkpass` container, reads `pass.json`,
/// deserializes it, and hands off to `parse_pass_json` (the actual logic,
/// fully unit-tested above). Not unit-tested beyond the round-trip smoke
/// test below -- this function's own job (calling into the `zip` crate
/// correctly) is thin enough that `parse_pass_json`'s own tests carry the
/// real coverage, mirroring `auth::oidc::OidcClient`'s untested-plumbing
/// precedent.
pub fn parse_pkpass(bytes: &[u8]) -> anyhow::Result<PartialTicket> {
    let cursor = std::io::Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|err| anyhow::anyhow!("not a valid .pkpass (zip) file: {err}"))?;
    let mut entry = archive
        .by_name("pass.json")
        .map_err(|err| anyhow::anyhow!("pass.json not found in .pkpass archive: {err}"))?;

    let mut buf = Vec::new();
    entry.by_ref().take(MAX_ENTRY_BYTES).read_to_end(&mut buf)?;

    let pass: serde_json::Value = serde_json::from_slice(&buf)
        .map_err(|err| anyhow::anyhow!("pass.json is not valid JSON: {err}"))?;
    parse_pass_json(&pass)
}

#[cfg(test)]
mod pass_json_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn semantics_present_is_preferred_and_labelled_accordingly() {
        let pass = json!({
            "organizationName": "LNER",
            "boardingPass": {
                "transitType": "PKTransitTypeTrain",
                "primaryFields": [{"key":"origin","label":"FROM","value":"Kings Cross"}],
                "semantics": {
                    "departureStationName": "Kings Cross",
                    "destinationStationName": "Edinburgh"
                }
            }
        });
        let ticket = parse_pass_json(&pass).unwrap();
        assert_eq!(ticket.operator, Some("LNER".to_string()));
        assert_eq!(ticket.origin_crs, Some("Kings Cross".to_string()));
        assert_eq!(ticket.destination_crs, Some("Edinburgh".to_string()));
        assert_eq!(ticket.source, "pkpass-semantics");
    }

    #[test]
    fn semantics_absent_falls_back_to_the_two_field_primary_fields_heuristic() {
        let pass = json!({
            "organizationName": "Trainline",
            "boardingPass": {
                "transitType": "PKTransitTypeTrain",
                "primaryFields": [
                    {"key":"origin","label":"FROM","value":"London Waterloo"},
                    {"key":"destination","label":"TO","value":"Woking"}
                ]
            }
        });
        let ticket = parse_pass_json(&pass).unwrap();
        assert_eq!(ticket.origin_crs, Some("London Waterloo".to_string()));
        assert_eq!(ticket.destination_crs, Some("Woking".to_string()));
        assert_eq!(ticket.source, "pkpass-heuristic");
    }

    #[test]
    fn a_primary_fields_array_of_the_wrong_length_yields_none_not_a_guess() {
        let pass = json!({
            "boardingPass": {
                "transitType": "PKTransitTypeTrain",
                "primaryFields": [{"key":"a","value":"1"}, {"key":"b","value":"2"}, {"key":"c","value":"3"}]
            }
        });
        let ticket = parse_pass_json(&pass).unwrap();
        assert_eq!(ticket.origin_crs, None);
        assert_eq!(ticket.destination_crs, None);
        assert_eq!(ticket.source, "pkpass-heuristic");
    }

    #[test]
    fn a_non_train_transit_type_is_rejected() {
        let pass = json!({"boardingPass": {"transitType": "PKTransitTypeAir"}});
        assert!(parse_pass_json(&pass).is_err());
    }

    #[test]
    fn a_pass_with_no_boarding_pass_at_all_is_rejected() {
        let pass = json!({"organizationName": "Not A Boarding Pass"});
        assert!(parse_pass_json(&pass).is_err());
    }

    #[test]
    fn ticket_type_is_read_from_auxiliary_fields_by_key() {
        let pass = json!({
            "organizationName": "Southern",
            "boardingPass": {
                "transitType": "PKTransitTypeTrain",
                "primaryFields": [
                    {"key":"origin","label":"FROM","value":"East Croydon"},
                    {"key":"destination","label":"TO","value":"Brighton"}
                ],
                "auxiliaryFields": [
                    {"key": "ticketType", "label": "TICKET TYPE", "value": "Super Off-Peak Return"}
                ]
            }
        });
        let ticket = parse_pass_json(&pass).unwrap();
        assert_eq!(
            ticket.ticket_type,
            Some("Super Off-Peak Return".to_string())
        );
    }

    #[test]
    fn ticket_type_ignores_a_field_with_the_wrong_key() {
        let pass = json!({
            "boardingPass": {
                "transitType": "PKTransitTypeTrain",
                "auxiliaryFields": [
                    {"key": "railcard", "label": "TICKET TYPE DISCOUNT", "value": "Network Railcard"}
                ]
            }
        });
        let ticket = parse_pass_json(&pass).unwrap();
        assert_eq!(
            ticket.ticket_type, None,
            "must match by the key field exactly, not by label text that happens to mention ticket type"
        );
    }

    #[test]
    fn ticket_type_is_never_guessed_at() {
        let pass = json!({"boardingPass": {"transitType": "PKTransitTypeTrain"}});
        assert_eq!(parse_pass_json(&pass).unwrap().ticket_type, None);
    }

    #[test]
    fn current_departure_date_is_read_from_semantics_when_present() {
        let pass = json!({
            "boardingPass": {
                "transitType": "PKTransitTypeTrain",
                "semantics": {
                    "departureStationName": "Kings Cross",
                    "destinationStationName": "Edinburgh",
                    "currentDepartureDate": "2026-09-22T18:32:00Z"
                }
            }
        });
        let ticket = parse_pass_json(&pass).unwrap();
        assert_eq!(
            ticket.current_departure_date,
            Some("2026-09-22T18:32:00Z".parse().unwrap())
        );
    }

    #[test]
    fn current_departure_date_is_none_when_semantics_omits_it() {
        let pass = json!({
            "boardingPass": {
                "transitType": "PKTransitTypeTrain",
                "semantics": {
                    "departureStationName": "Kings Cross",
                    "destinationStationName": "Edinburgh"
                }
            }
        });
        assert_eq!(parse_pass_json(&pass).unwrap().current_departure_date, None);
    }

    #[test]
    fn current_departure_date_is_none_when_semantics_is_absent_entirely() {
        // Falls back to the `primaryFields` heuristic for origin/destination
        // (see `primary_fields_origin_destination`'s own doc comment) --
        // that positional match is names only, so there is nothing dateable
        // to read either.
        let pass = json!({
            "boardingPass": {
                "transitType": "PKTransitTypeTrain",
                "primaryFields": [
                    {"key":"origin","label":"FROM","value":"London Waterloo"},
                    {"key":"destination","label":"TO","value":"Woking"}
                ]
            }
        });
        let ticket = parse_pass_json(&pass).unwrap();
        assert_eq!(ticket.source, "pkpass-heuristic");
        assert_eq!(ticket.current_departure_date, None);
    }

    #[test]
    fn current_departure_date_is_none_when_the_value_does_not_parse_as_a_real_date_time() {
        let pass = json!({
            "boardingPass": {
                "transitType": "PKTransitTypeTrain",
                "semantics": {
                    "departureStationName": "Kings Cross",
                    "destinationStationName": "Edinburgh",
                    "currentDepartureDate": "not-a-real-date"
                }
            }
        });
        assert_eq!(parse_pass_json(&pass).unwrap().current_departure_date, None);
    }
}

#[cfg(test)]
mod barcode_format_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_format_from_the_singular_barcode_object() {
        // The `message` value here is deliberately an obvious
        // non-payload placeholder -- never anything resembling a real
        // RSP-6 payload shape. See this plan's Global Constraints.
        let pass = json!({
            "barcode": {
                "format": "PKBarcodeFormatAztec",
                "message": "PLACEHOLDER-NOT-A-REAL-PAYLOAD",
                "messageEncoding": "iso-8859-1"
            }
        });
        assert_eq!(
            barcode_format(&pass),
            Some("PKBarcodeFormatAztec".to_string())
        );
    }

    #[test]
    fn falls_back_to_the_plural_barcodes_array() {
        let pass = json!({
            "barcodes": [
                {"format": "PKBarcodeFormatQR", "message": "PLACEHOLDER-NOT-A-REAL-PAYLOAD"}
            ]
        });
        assert_eq!(barcode_format(&pass), Some("PKBarcodeFormatQR".to_string()));
    }

    #[test]
    fn returns_none_when_neither_field_is_present() {
        let pass = json!({"organizationName": "LNER"});
        assert_eq!(barcode_format(&pass), None);
    }
}

#[cfg(test)]
mod parse_pkpass_tests {
    use super::*;
    use std::io::Write;

    fn build_pkpass(pass_json: &serde_json::Value) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            writer
                .start_file("pass.json", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(pass_json.to_string().as_bytes()).unwrap();
            writer.finish().unwrap();
        }
        buf
    }

    #[test]
    fn a_well_formed_pkpass_round_trips_through_the_full_pipeline() {
        let pass = serde_json::json!({
            "organizationName": "LNER",
            "boardingPass": {
                "transitType": "PKTransitTypeTrain",
                "semantics": {"departureStationName": "Kings Cross", "destinationStationName": "Edinburgh"}
            }
        });
        let bytes = build_pkpass(&pass);
        let ticket = parse_pkpass(&bytes).unwrap();
        assert_eq!(ticket.operator, Some("LNER".to_string()));
        assert_eq!(ticket.source, "pkpass-semantics");
    }

    #[test]
    fn a_zip_with_no_pass_json_is_rejected() {
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            writer
                .start_file("readme.txt", zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(b"not a pass").unwrap();
            writer.finish().unwrap();
        }
        assert!(parse_pkpass(&buf).is_err());
    }

    #[test]
    fn bytes_that_are_not_a_zip_at_all_are_rejected() {
        assert!(parse_pkpass(b"this is definitely not a zip file").is_err());
    }
}
