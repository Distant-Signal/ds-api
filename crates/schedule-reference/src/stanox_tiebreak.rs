//! Curated tie-breaks for STANOXes that the CIF `TI` records map to two or
//! more genuine (non-`X`-prefixed) CRS codes.
//!
//! [`crate::parser::resolve`] prefers the sole non-`X` candidate of an
//! ambiguous STANOX and otherwise excludes the STANOX entirely, because a
//! TRUST `loc_stanox` names a location, not a CRS, and there is no general
//! rule to choose between two real station codes. The consequence was that
//! the TRUST movement consumers (which key on `stanox_crs`) dropped every
//! movement at five stations that Darwin does serve.
//!
//! Each row below is a human decision, reviewed once, for one STANOX. It is
//! applied only to a STANOX that would otherwise be excluded: it never
//! changes a STANOX the rule already resolves. Rows that stop matching the
//! feed are ignored with a warning (never applied blindly), and the tests in
//! `parser.rs` fail if a row no longer matches the checked-in fixture of the
//! feed's ambiguous STANOXes.
//!
//! How the answers were chosen. All five pairs are the international and the
//! domestic side of one physical station (or Poole and its ferry quay): one
//! STANOX, two TIPLOCs, two CRS. Darwin keys every actual call of these
//! stations on one of the two TIPLOCs, and CORPUS (Network Rail's own
//! location list) agrees with it, so the STANOX is given that TIPLOC's CRS.
//! Evidence, taken 2026-11-18 from production aggregates: Darwin calls for
//! `STPANCI`, `STFORDI`, `ASHFKY`, `EBSFLTI` and `POOLE` run at 120 to 395 a
//! day, against none (or 3 for `ASHFKI`) for `STPADOM`, `STFODOM`, `ASHFKI`,
//! `EBSFDOM` and `POLEFT`. The other CRS of each pair keeps its own
//! `tiploc_crs` row, so schedule and Darwin lookups by TIPLOC are unchanged.
//!
//! Left excluded on purpose: `87981` (`BRGHTTY`/`XBP` and `BRGHMHS`/`XMP`,
//! two Brighton sidings with `X` pseudo-codes and no passenger station), and
//! any STANOX not listed here.

/// One reviewed decision: this STANOX resolves to this CRS.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tiebreak {
    pub stanox: &'static str,
    pub crs: &'static str,
    /// Why this candidate and not the other. Required, so a reviewer can
    /// check the reasoning without the investigation.
    pub rationale: &'static str,
}

/// The decisions applied by [`crate::parser::resolve`]. Sorted by STANOX.
pub(crate) const TIEBREAKS: &[Tiebreak] = &[
    Tiebreak {
        stanox: "52215",
        crs: "SDI",
        rationale: "Stratford International: Darwin calls at STFORDI (SDI); STFODOM (SFA) is the domestic platform group with no Darwin calls; CORPUS lists both under this STANOX.",
    },
    Tiebreak {
        stanox: "63631",
        crs: "SPX",
        rationale: "St Pancras International: Darwin calls at STPANCI (SPX); STPADOM has no CRS in CORPUS and only resolves to STP through MSN; STP is already served by its own STANOX 63630 (STPX).",
    },
    Tiebreak {
        stanox: "86935",
        crs: "POO",
        rationale: "Poole: Darwin calls at POOLE (POO); POLEFT (PFT) is the ferry quay, with no Darwin calls.",
    },
    Tiebreak {
        stanox: "89428",
        crs: "AFK",
        rationale: "Ashford International: Darwin calls at ASHFKY (AFK); ASHFKI (ASI, platforms 3-4) has next to no Darwin calls.",
    },
    Tiebreak {
        stanox: "89530",
        crs: "EBF",
        rationale: "Ebbsfleet International: Darwin calls at EBSFLTI (EBF); EBSFDOM (EBD) is the domestic platform group with no Darwin calls.",
    },
];

/// The row for `stanox`, if a decision was recorded for it.
pub(crate) fn find<'a>(tiebreaks: &'a [Tiebreak], stanox: &str) -> Option<&'a Tiebreak> {
    tiebreaks.iter().find(|t| t.stanox == stanox)
}
