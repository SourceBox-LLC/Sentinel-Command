//! HMAC-SHA256 and the two base64 alphabets, in one place.
//!
//! Three call sites need the same primitive — the Svix webhook
//! signature, the agent wakeup signature, and the unsubscribe token —
//! and each had grown its own copy. Written out rather than taken from
//! a crate because the construction is four lines and the project
//! already depends on `sha2`; kept in one module because three copies
//! of a keyed hash is how one of them ends up subtly different.

use sha2::{Digest, Sha256};

/// HMAC-SHA256, as `hmac.new(key, message, sha256).digest()`.
///
/// The block size is 64 bytes, a longer key is hashed down to fit, and
/// a shorter one is zero-padded.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut key = key.to_vec();
    if key.len() > 64 {
        key = Sha256::digest(&key).to_vec();
    }
    key.resize(64, 0);
    let ipad: Vec<u8> = key.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = key.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::digest([&ipad[..], message].concat());
    Sha256::digest([&opad[..], &inner[..]].concat()).into()
}

/// `.hexdigest()` — lower case, no separators.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

const STANDARD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `base64.b64encode` — the `+/` alphabet, padded. What Svix signatures
/// are written in.
pub fn base64_standard(bytes: &[u8]) -> String {
    encode(bytes, STANDARD, true)
}

/// `base64.urlsafe_b64encode(...).rstrip(b"=")` — the `-_` alphabet
/// with the padding stripped, which is how JWT writes its segments.
pub fn base64_url_nopad(bytes: &[u8]) -> String {
    encode(bytes, URL_SAFE, false)
}

/// The inverse of `base64_standard`, for the Svix signing secret,
/// which arrives base64-encoded behind a `whsec_` prefix. Lenient in
/// the one way Python's decoder is: padding ends the input rather than
/// being counted.
pub fn base64_standard_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for ch in input.bytes() {
        if ch == b'=' {
            break;
        }
        let val = STANDARD.iter().position(|&c| c == ch)? as u32;
        buf = (buf << 6) | val;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

fn encode(bytes: &[u8], alphabet: &[u8; 64], pad: bool) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(alphabet[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked against `hmac.new(key, msg, sha256).hexdigest()`.
    #[test]
    fn the_keyed_hash_matches_pythons() {
        assert_eq!(
            hex(&hmac_sha256(b"test-agent-key", b"{\"ts\": 1700000000}")),
            "d8d69079da8018fcf0f299cd1396f0445ce4023b2554cae02282d60a6704ef76"
        );
        // A key longer than the 64-byte block is hashed first.
        assert_eq!(
            hex(&hmac_sha256(&[b'k'; 100], b"{\"ts\": 1700000000}")),
            "7156dc62e67bd499330d3e7ee935d377bfc4ce7c41600b3bee801de0a6e1d434"
        );
        // Exactly the block size takes neither branch.
        assert_eq!(
            hex(&hmac_sha256(&[b'k'; 64], b"")),
            "83026a325aaee70e36cfe607536aa1054104ad1077c36134810d4ccded1ccd3b"
        );
    }

    /// Checked against `base64.b64encode` and
    /// `base64.urlsafe_b64encode(...).rstrip(b"=")`.
    #[test]
    fn both_alphabets_match_pythons() {
        // Every input length modulo three, so the padding branch is
        // reached from each side.
        for (input, standard, url) in [
            (&b""[..], "", ""),
            (b"f", "Zg==", "Zg"),
            (b"fo", "Zm8=", "Zm8"),
            (b"foo", "Zm9v", "Zm9v"),
            (b"foob", "Zm9vYg==", "Zm9vYg"),
        ] {
            assert_eq!(base64_standard(input), standard, "{input:?}");
            assert_eq!(base64_url_nopad(input), url, "{input:?}");
        }
        // The two alphabets differ only in the last two characters,
        // which these bytes are chosen to reach.
        assert_eq!(base64_standard(&[0xfb, 0xff, 0xbf]), "+/+/");
        assert_eq!(base64_url_nopad(&[0xfb, 0xff, 0xbf]), "-_-_");
    }

    #[test]
    fn the_standard_alphabet_round_trips() {
        for raw in [&b""[..], b"a", b"ab", b"abc", b"abcd", &[0u8, 255, 16][..]] {
            let encoded = base64_standard(raw);
            assert_eq!(base64_standard_decode(&encoded).as_deref(), Some(raw), "{encoded}");
        }
        // A character outside the alphabet is a refusal, not a skip.
        assert_eq!(base64_standard_decode("ab!c"), None);
    }
}
