//! Cheap structural pre-checks on an uploaded ticket file, run BEFORE any
//! real parser (`zip`) sees the bytes (M13, 2026-09-27). Only Apple
//! Wallet `.pkpass` files are read; the PDF upload was removed in 2026-11.
//!
//! These checks never decompress anything and never build an object graph.
//! They read fixed-size headers, walk a zip central directory whose entry
//! count is capped first, and run linear byte scans over an input that is
//! already size-capped. The whole pass takes well under a few milliseconds
//! on the largest input the route accepts, so it runs inline on the
//! request's own task, ahead of acquiring a parse slot.
//!
//! What this is NOT: a guarantee that the real parse will be well behaved.
//! Every figure checked here is one the file itself *declares* (a zip entry's
//! uncompressed size), and a hostile file can lie about
//! them. The real parse therefore still runs in a killable, resource-limited
//! child process (`data::ticket_subprocess`); these checks exist to turn the
//! common pathological shapes (zip bombs, nested archives) into a prompt,
//! specific 4xx instead of a
//! child that burns its whole budget first.

use super::ticket_extraction::MAX_ENTRY_BYTES;

/// Why an upload was refused before parsing. Each variant maps to one HTTP
/// status in `routes::train` (413 / 415 / 422), and `message` is written to
/// be shown to the user as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// The file is larger than this kind of ticket can plausibly be.
    TooLarge(String),
    /// The magic bytes say this is not the kind of file the route expects.
    WrongType(String),
    /// The container is structurally unacceptable (malformed, a zip bomb,
    /// a nested archive, too many objects, ...).
    Unacceptable(String),
}

impl Rejection {
    pub fn message(&self) -> &str {
        match self {
            Self::TooLarge(msg) | Self::WrongType(msg) | Self::Unacceptable(msg) => msg,
        }
    }
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for Rejection {}

const ZIP_LOCAL_HEADER: &[u8] = b"PK\x03\x04";
const ZIP_CENTRAL_HEADER: &[u8] = b"PK\x01\x02";
const ZIP_EOCD: &[u8] = b"PK\x05\x06";
const PDF_MAGIC: &[u8] = b"%PDF-";

/// Upper bound on a `.pkpass` upload. Equal to `routes::train`'s
/// `DefaultBodyLimit` (8 MiB), stated again here so the check holds even for
/// a caller that isn't behind that layer (the child process, a test).
pub const MAX_PKPASS_UPLOAD_BYTES: usize = 8 * 1024 * 1024;

/// A real `.pkpass` has `pass.json`, `manifest.json`, `signature`, a handful
/// of icon/logo/strip images at 1x/2x/3x, and optionally one
/// `<lang>.lproj/pass.strings` per localisation. 256 leaves room for dozens
/// of localisations.
pub const MAX_ZIP_ENTRIES: usize = 256;

/// Sum of every entry's declared uncompressed size. The images in a pass are
/// already-compressed PNGs, so this is close to the file size for any real
/// pass; 32 MiB is four times the upload cap.
pub const MAX_ZIP_TOTAL_UNCOMPRESSED_BYTES: u64 = 32 * 1024 * 1024;

/// Declared uncompressed/compressed ratio above which an entry is treated
/// as a compression bomb. Deflated JSON and text reach roughly 10:1; 100:1
/// only comes from long runs of repeated bytes. Only applied to entries
/// that inflate past [`ZIP_RATIO_CHECK_MIN_BYTES`], so a tiny, highly
/// repetitive `pass.strings` doesn't trip it.
pub const MAX_ZIP_COMPRESSION_RATIO: u64 = 100;
pub const ZIP_RATIO_CHECK_MIN_BYTES: u64 = 64 * 1024;

/// Directory depth inside the archive. A pass is flat apart from
/// `<lang>.lproj/` folders, so anything deeper than this is not a pass.
pub const MAX_ZIP_PATH_DEPTH: usize = 4;

/// Extensions of archive formats a pass never contains. An archive inside
/// the archive is how recursive zip bombs are built.
const NESTED_ARCHIVE_EXTENSIONS: &[&str] = &[
    ".zip",
    ".pkpass",
    ".pkpasses",
    ".jar",
    ".gz",
    ".tgz",
    ".bz2",
    ".xz",
    ".7z",
    ".rar",
    ".tar",
];

/// Pre-checks a `.pkpass` upload: size, zip magic, then a walk of the
/// central directory (entry count, multi-disk and ZIP64 refusal,
/// encryption, compression method, total declared uncompressed size,
/// per-entry compression ratio, nested archives, path depth, and a
/// `pass.json` at the root no larger than the parser will read).
#[expect(
    clippy::too_many_lines,
    reason = "long but linear; splitting it would scatter its shared state across helpers"
)]
pub fn precheck_pkpass(bytes: &[u8]) -> Result<(), Rejection> {
    if bytes.len() > MAX_PKPASS_UPLOAD_BYTES {
        return Err(Rejection::TooLarge(format!(
            "this .pkpass is too large ({} bytes; the limit is {} bytes)",
            bytes.len(),
            MAX_PKPASS_UPLOAD_BYTES
        )));
    }
    if bytes.starts_with(PDF_MAGIC) {
        return Err(Rejection::WrongType(
            "this file is a PDF, not a .pkpass; only Apple Wallet passes can be read".to_string(),
        ));
    }
    if !bytes.starts_with(ZIP_LOCAL_HEADER) {
        return Err(Rejection::WrongType(
            "this is not a .pkpass file (a .pkpass is a zip archive, and this file isn't one)"
                .to_string(),
        ));
    }

    let unacceptable = |msg: &str| Rejection::Unacceptable(format!("this .pkpass {msg}"));

    let eocd = find_eocd(bytes).ok_or_else(|| {
        unacceptable("is truncated or damaged (no zip end-of-central-directory record)")
    })?;
    let disk = le_u16(bytes, eocd + 4);
    let cd_disk = le_u16(bytes, eocd + 6);
    let entries_on_disk = le_u16(bytes, eocd + 8);
    let entries_total = le_u16(bytes, eocd + 10);
    let cd_size = le_u32(bytes, eocd + 12);
    let cd_offset = le_u32(bytes, eocd + 16);

    if disk != 0 || cd_disk != 0 || entries_on_disk != entries_total {
        return Err(unacceptable("is a multi-part zip archive"));
    }
    if entries_total == u16::MAX || cd_size == u32::MAX || cd_offset == u32::MAX {
        return Err(unacceptable(
            "uses ZIP64 extensions, which no real pass needs",
        ));
    }
    if usize::from(entries_total) > MAX_ZIP_ENTRIES {
        return Err(unacceptable(&format!(
            "has {entries_total} files in it; a pass has at most {MAX_ZIP_ENTRIES}"
        )));
    }
    let cd_start = cd_offset as usize;
    let cd_end = cd_start
        .checked_add(cd_size as usize)
        .filter(|&end| end <= eocd)
        .ok_or_else(|| unacceptable("is damaged (its central directory is out of bounds)"))?;

    let mut pos = cd_start;
    let mut seen = 0usize;
    let mut total_uncompressed: u64 = 0;
    let mut pass_json_size: Option<u64> = None;
    while pos < cd_end {
        if pos + 46 > cd_end || &bytes[pos..pos + 4] != ZIP_CENTRAL_HEADER {
            return Err(unacceptable("is damaged (bad central directory entry)"));
        }
        seen += 1;
        if seen > usize::from(entries_total) {
            return Err(unacceptable(
                "is damaged (more central directory entries than it declares)",
            ));
        }
        let flags = le_u16(bytes, pos + 8);
        let method = le_u16(bytes, pos + 10);
        let compressed = u64::from(le_u32(bytes, pos + 20));
        let uncompressed = u64::from(le_u32(bytes, pos + 24));
        let name_len = usize::from(le_u16(bytes, pos + 28));
        let extra_len = usize::from(le_u16(bytes, pos + 30));
        let comment_len = usize::from(le_u16(bytes, pos + 32));
        let name_start = pos + 46;
        let next = name_start + name_len + extra_len + comment_len;
        if next > cd_end {
            return Err(unacceptable(
                "is damaged (a central directory entry runs past its end)",
            ));
        }
        let name = String::from_utf8_lossy(&bytes[name_start..name_start + name_len]);

        if flags & 0x0001 != 0 {
            return Err(unacceptable("contains encrypted files"));
        }
        // 0 = stored, 8 = deflate: the only two methods the `zip` crate is
        // built with here, and the only two Apple's tooling produces.
        if method != 0 && method != 8 {
            return Err(unacceptable(&format!(
                "uses an unsupported compression method ({method})"
            )));
        }
        if compressed == u64::from(u32::MAX) || uncompressed == u64::from(u32::MAX) {
            return Err(unacceptable(
                "uses ZIP64 extensions, which no real pass needs",
            ));
        }
        if uncompressed > ZIP_RATIO_CHECK_MIN_BYTES
            && uncompressed > compressed.saturating_mul(MAX_ZIP_COMPRESSION_RATIO)
        {
            return Err(unacceptable(&format!(
                "contains a file ({name}) that decompresses to more than {MAX_ZIP_COMPRESSION_RATIO} \
                 times its stored size, which looks like a zip bomb"
            )));
        }
        total_uncompressed = total_uncompressed.saturating_add(uncompressed);
        if total_uncompressed > MAX_ZIP_TOTAL_UNCOMPRESSED_BYTES {
            return Err(unacceptable(&format!(
                "decompresses to more than {MAX_ZIP_TOTAL_UNCOMPRESSED_BYTES} bytes, far more than \
                 any real pass"
            )));
        }
        let lower = name.to_ascii_lowercase();
        if NESTED_ARCHIVE_EXTENSIONS
            .iter()
            .any(|ext| lower.ends_with(ext))
        {
            return Err(unacceptable(&format!(
                "contains another archive ({name}); nested archives are not accepted"
            )));
        }
        if name.split('/').filter(|s| !s.is_empty()).count() > MAX_ZIP_PATH_DEPTH {
            return Err(unacceptable(&format!(
                "has files nested too deeply ({name})"
            )));
        }
        if name == "pass.json" {
            pass_json_size = Some(uncompressed);
        }
        pos = next;
    }
    if seen != usize::from(entries_total) {
        return Err(unacceptable(
            "is damaged (fewer central directory entries than it declares)",
        ));
    }

    match pass_json_size {
        None => Err(unacceptable("has no pass.json, so it isn't a Wallet pass")),
        Some(size) if size > MAX_ENTRY_BYTES => Err(unacceptable(&format!(
            "has a pass.json of {size} bytes; the limit is {MAX_ENTRY_BYTES}"
        ))),
        Some(_) => Ok(()),
    }
}

/// The end-of-central-directory record is 22 bytes plus a comment of up to
/// 65535 bytes, so it starts somewhere in the last 65557 bytes. Scans
/// backwards for its signature and accepts the first candidate whose
/// comment length is consistent with its position.
fn find_eocd(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 22 {
        return None;
    }
    let lowest = bytes.len().saturating_sub(22 + usize::from(u16::MAX));
    (lowest..=bytes.len() - 22).rev().find(|&pos| {
        &bytes[pos..pos + 4] == ZIP_EOCD
            && pos + 22 + usize::from(le_u16(bytes, pos + 20)) <= bytes.len()
    })
}

fn le_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn le_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

#[cfg(test)]
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Test fixtures shared with `data::ticket_subprocess` and the
/// `ticket_parse_subprocess` integration test: a real pkpass.
#[doc(hidden)]
pub mod fixtures {
    use std::io::Write;

    /// A `.pkpass` containing `files` (name, contents), deflated.
    #[expect(clippy::expect_used, reason = "test fixture builder writing to memory")]
    pub fn zip_with(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            for (name, contents) in files {
                writer
                    .start_file(*name, zip::write::SimpleFileOptions::default())
                    .expect("start zip entry");
                writer.write_all(contents).expect("write zip entry");
            }
            writer.finish().expect("finish zip");
        }
        buf
    }

    pub fn train_pass_json() -> Vec<u8> {
        serde_json::json!({
            "organizationName": "LNER",
            "boardingPass": {
                "transitType": "PKTransitTypeTrain",
                "semantics": {
                    "departureStationName": "Kings Cross",
                    "destinationStationName": "Edinburgh",
                    "currentDepartureDate": "2026-09-27T10:00:00Z"
                },
                "auxiliaryFields": [{"key": "ticketType", "label": "TYPE", "value": "Advance Single"}]
            }
        })
        .to_string()
        .into_bytes()
    }

    pub fn train_pkpass() -> Vec<u8> {
        zip_with(&[
            ("pass.json", &train_pass_json()),
            ("manifest.json", b"{}"),
            ("en.lproj/pass.strings", b"\"a\" = \"b\";"),
        ])
    }
}

#[cfg(test)]
#[expect(
    clippy::cast_possible_truncation,
    reason = "test code: casts of small known test values"
)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn assert_unacceptable(result: Result<(), Rejection>, needle: &str) {
        match result {
            Err(Rejection::Unacceptable(msg)) => assert!(
                msg.contains(needle),
                "expected a rejection mentioning {needle:?}, got {msg:?}"
            ),
            other => panic!("expected Unacceptable({needle:?}), got {other:?}"),
        }
    }

    /// Stored (method 0) entries so the compressed size equals the data
    /// length and the test controls the declared sizes exactly.
    fn stored_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;
        let mut buf = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, contents) in files {
                writer.start_file(*name, options).unwrap();
                writer.write_all(contents).unwrap();
            }
            writer.finish().unwrap();
        }
        buf
    }

    /// Offset of the central directory entry for `name` in `zip`.
    fn central_entry(zip: &[u8], name: &str) -> usize {
        let mut pos = 0;
        loop {
            let at = pos + find(&zip[pos..], ZIP_CENTRAL_HEADER).expect("central entry");
            let name_len = usize::from(le_u16(zip, at + 28));
            if &zip[at + 46..at + 46 + name_len] == name.as_bytes() {
                return at;
            }
            pos = at + 4;
        }
    }

    #[test]
    fn a_real_pkpass_passes() {
        assert_eq!(precheck_pkpass(&train_pkpass()), Ok(()));
    }

    #[test]
    fn an_oversized_pkpass_is_too_large() {
        let mut bytes = train_pkpass();
        bytes.resize(MAX_PKPASS_UPLOAD_BYTES + 1, 0);
        assert!(matches!(
            precheck_pkpass(&bytes),
            Err(Rejection::TooLarge(_))
        ));
    }

    #[test]
    fn a_pdf_sent_to_the_pkpass_route_is_the_wrong_type_and_says_so() {
        match precheck_pkpass(b"%PDF-1.4\n1 0 obj null endobj\nstartxref\n0\n%%EOF\n") {
            Err(Rejection::WrongType(msg)) => assert!(msg.contains("PDF"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn arbitrary_bytes_are_the_wrong_type() {
        let png = b"\x89PNG\r\n\x1a\n not a ticket";
        assert!(matches!(precheck_pkpass(png), Err(Rejection::WrongType(_))));
    }

    #[test]
    fn a_zip_with_too_many_entries_is_rejected() {
        let names: Vec<String> = (0..=MAX_ZIP_ENTRIES).map(|i| format!("f{i}.txt")).collect();
        let pass = train_pass_json();
        let mut files: Vec<(&str, &[u8])> = vec![("pass.json", &pass)];
        files.extend(names.iter().map(|n| (n.as_str(), &b"x"[..])));
        assert_unacceptable(precheck_pkpass(&zip_with(&files)), "files in it");
    }

    #[test]
    fn a_zip_whose_declared_total_size_is_huge_is_rejected() {
        // Stored entries whose declared sizes are patched up to just under
        // the ratio threshold's reach but past the total cap: the check
        // reads only the central directory, never the data.
        let chunk = vec![b'a'; 1024];
        let pass = train_pass_json();
        let mut zip = stored_zip(&[("pass.json", &pass), ("a.png", &chunk), ("b.png", &chunk)]);
        for name in ["a.png", "b.png"] {
            let at = central_entry(&zip, name);
            // compressed = 1 MiB, uncompressed = 17 MiB: ratio 17 < 100.
            zip[at + 20..at + 24].copy_from_slice(&(1024u32 * 1024).to_le_bytes());
            zip[at + 24..at + 28].copy_from_slice(&(17u32 * 1024 * 1024).to_le_bytes());
        }
        assert_unacceptable(precheck_pkpass(&zip), "decompresses to more than");
    }

    #[test]
    fn a_highly_compressed_entry_is_rejected_as_a_zip_bomb() {
        let zeros = vec![0u8; 4 * 1024 * 1024];
        let pass = train_pass_json();
        let zip = zip_with(&[("pass.json", &pass), ("strip.png", &zeros)]);
        assert_unacceptable(precheck_pkpass(&zip), "zip bomb");
    }

    #[test]
    fn a_small_highly_repetitive_entry_is_not_a_zip_bomb() {
        let zeros = vec![0u8; 32 * 1024];
        let pass = train_pass_json();
        let zip = zip_with(&[("pass.json", &pass), ("pad.strings", &zeros)]);
        assert_eq!(precheck_pkpass(&zip), Ok(()));
    }

    #[test]
    fn a_nested_archive_is_rejected() {
        let inner = train_pkpass();
        let pass = train_pass_json();
        let zip = zip_with(&[("pass.json", &pass), ("inner.zip", &inner)]);
        assert_unacceptable(precheck_pkpass(&zip), "nested archives");
    }

    #[test]
    fn deeply_nested_paths_are_rejected() {
        let pass = train_pass_json();
        let zip = zip_with(&[("pass.json", &pass), ("a/b/c/d/e.txt", b"x")]);
        assert_unacceptable(precheck_pkpass(&zip), "nested too deeply");
    }

    #[test]
    fn a_zip_without_pass_json_is_rejected() {
        let zip = zip_with(&[("readme.txt", b"hello")]);
        assert_unacceptable(precheck_pkpass(&zip), "no pass.json");
    }

    #[test]
    fn a_pass_json_larger_than_the_parser_reads_is_rejected() {
        let big = vec![b' '; (MAX_ENTRY_BYTES + 1) as usize];
        let zip = stored_zip(&[("pass.json", &big)]);
        assert_unacceptable(precheck_pkpass(&zip), "pass.json of");
    }

    #[test]
    fn a_zip_with_no_end_of_central_directory_is_rejected() {
        let mut zip = train_pkpass();
        let eocd = find_eocd(&zip).unwrap();
        zip.truncate(eocd);
        assert_unacceptable(precheck_pkpass(&zip), "truncated or damaged");
    }

    #[test]
    fn an_encrypted_entry_is_rejected() {
        let mut zip = train_pkpass();
        let at = central_entry(&zip, "manifest.json");
        zip[at + 8] |= 1;
        assert_unacceptable(precheck_pkpass(&zip), "encrypted");
    }

    #[test]
    fn a_zip64_marker_is_rejected() {
        let mut zip = train_pkpass();
        let at = central_entry(&zip, "manifest.json");
        zip[at + 24..at + 28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_unacceptable(precheck_pkpass(&zip), "ZIP64");
    }
}
