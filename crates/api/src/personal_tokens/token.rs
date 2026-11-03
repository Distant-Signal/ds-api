//! The personal API token format (docs/specs/personal-api-tokens.md §2.1,
//! §2.2):
//!
//! ```text
//! dsp_<32 base62 characters: random><6 base62 characters: CRC32 of the random part>
//! ```
//!
//! The random part is about 190 bits from the OS RNG. The checksum is not a
//! security control: it lets secret scanners recognise a real token, and
//! lets the api refuse a mistyped one before any database work. Only the
//! SHA-256 of the whole token is stored, and a stored hash is compared in
//! constant time.

use rand::Rng;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Every personal token starts with this.
pub const PREFIX: &str = "dsp_";
/// Base62 characters of randomness after the prefix.
pub const RANDOM_LEN: usize = 32;
/// Base62 characters of checksum at the end.
pub const CHECKSUM_LEN: usize = 6;
/// The whole token's length.
pub const TOKEN_LEN: usize = PREFIX.len() + RANDOM_LEN + CHECKSUM_LEN;
/// Characters of the token shown as its hint (all checksum).
pub const HINT_LEN: usize = 4;

const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// SHA-256 of a whole token string, as stored in
/// `personal_api_tokens.token_hash`.
pub type TokenHash = [u8; 32];

/// A newly created token: the plaintext (shown once), its hash and hint.
pub struct NewToken {
    pub token: String,
    pub hash: TokenHash,
    pub hint: String,
}

/// Never prints the token itself.
impl std::fmt::Debug for NewToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewToken")
            .field("token", &"dsp_[redacted]")
            .field("hint", &self.hint)
            .finish_non_exhaustive()
    }
}

/// A fresh token from the OS RNG: each random character is drawn
/// uniformly from the 62-character alphabet.
pub fn generate() -> NewToken {
    let mut rng = OsRng;
    let random: String = (0..RANDOM_LEN)
        .map(|_| char::from(ALPHABET[rng.gen_range(0..ALPHABET.len())]))
        .collect();
    let token = format!("{PREFIX}{random}{}", checksum(&random));
    let hint = token[TOKEN_LEN - HINT_LEN..].to_string();
    NewToken {
        hash: hash(&token),
        token,
        hint,
    }
}

/// The CRC32 (IEEE) of `random`, as 6 base62 characters, left-padded with
/// `0`. `u32::MAX` is below 62^6, so 6 always suffice.
fn checksum(random: &str) -> String {
    let mut value = u64::from(crc32fast::hash(random.as_bytes()));
    let mut out = [b'0'; CHECKSUM_LEN];
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[usize::try_from(value % 62).unwrap_or(0)];
        value /= 62;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// SHA-256 of the whole token string.
pub fn hash(token: &str) -> TokenHash {
    Sha256::digest(token.as_bytes()).into()
}

/// Whether `candidate` is shaped like a personal token at all (the prefix),
/// so the main listener can refuse one without parsing it.
pub fn looks_like_token(candidate: &str) -> bool {
    candidate.starts_with(PREFIX)
}

/// The hash of `candidate` when it is a well-formed token (prefix, length,
/// alphabet and checksum), else `None`, with no database work.
pub fn parse(candidate: &str) -> Option<TokenHash> {
    let rest = candidate.strip_prefix(PREFIX)?;
    if candidate.len() != TOKEN_LEN || !rest.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    let (random, check) = rest.split_at(RANDOM_LEN);
    if !bool::from(checksum(random).as_bytes().ct_eq(check.as_bytes())) {
        return None;
    }
    Some(hash(candidate))
}

/// Constant-time equality of two token hashes.
pub fn hashes_equal(a: &[u8], b: &[u8]) -> bool {
    bool::from(a.ct_eq(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_token_has_the_documented_shape() {
        let token = generate();
        assert_eq!(token.token.len(), TOKEN_LEN);
        assert_eq!(TOKEN_LEN, 42);
        assert!(token.token.starts_with("dsp_"));
        assert!(token.token[4..].bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_eq!(token.hint, &token.token[38..]);
        assert_eq!(token.hash, hash(&token.token));
        // The secret-scanning pattern documented in the spec matches it.
        let pattern = regex::Regex::new(r"\bdsp_[0-9A-Za-z]{38}\b").unwrap();
        assert!(pattern.is_match(&token.token));
    }

    #[test]
    fn a_generated_token_parses_to_its_hash() {
        for _ in 0..100 {
            let token = generate();
            assert_eq!(parse(&token.token), Some(token.hash));
        }
    }

    #[test]
    fn tokens_are_not_repeated() {
        let tokens: std::collections::HashSet<String> =
            (0..1000).map(|_| generate().token).collect();
        assert_eq!(tokens.len(), 1000);
    }

    #[test]
    fn malformed_tokens_are_refused_without_a_lookup() {
        let good = generate().token;
        let mut flipped = good.clone().into_bytes();
        // Change one random character: the checksum no longer matches.
        flipped[10] = if flipped[10] == b'a' { b'b' } else { b'a' };
        let flipped = String::from_utf8(flipped).unwrap();
        for bad in [
            "",
            "dsp_",
            &good[..good.len() - 1],
            &format!("{good}x"),
            &good.replacen("dsp_", "dsx_", 1),
            &good.replacen(&good[5..6], "-", 1),
            flipped.as_str(),
        ] {
            assert_eq!(parse(bad), None, "{bad:?} parsed");
        }
    }

    #[test]
    fn the_checksum_is_fixed_width_and_known() {
        // CRC32("123456789") is 0xCBF43926 = 3421780262, which is
        // "3jZRME" in this base62 alphabet, already 6 characters.
        assert_eq!(checksum("123456789"), "3jZRME");
        // Small values are left-padded.
        assert_eq!(checksum("").len(), CHECKSUM_LEN);
    }

    #[test]
    fn the_debug_form_never_shows_the_token() {
        let token = generate();
        let printed = format!("{token:?}");
        assert!(!printed.contains(&token.token));
        assert!(!printed.contains(&token.token[4..20]));
    }

    #[test]
    fn hash_comparison_is_exact() {
        let a = hash("dsp_a");
        let b = hash("dsp_b");
        assert!(hashes_equal(&a, &a));
        assert!(!hashes_equal(&a, &b));
        assert!(!hashes_equal(&a, &a[..31]));
    }
}
