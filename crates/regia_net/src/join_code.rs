// SPDX-License-Identifier: MPL-2.0
//! Short join codes a player can read out or type.
//!
//! Six characters from a 31-letter alphabet without look-alike glyphs (`0`/`O`, `1`/`I`/`L`), about
//! 887 million codes. Minting is deterministic from a 64-bit seed (mixed with the SplitMix64
//! finalizer, Steele, Lea and Flood, OOPSLA 2014) so tests need no random source, and nearby seeds
//! give unrelated codes.
//!
//! The mixer is public and invertible, so a code minted from a guessable seed (a counter, a time)
//! is itself guessable. A service whose codes grant something (the relay's `Lookup` hands out the
//! host's address and bind key) must mint from a secret-derived seed, as `relay::RelayCore` does.

/// The code alphabet (uppercase).
pub const JOIN_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// Code length.
pub const JOIN_CODE_LEN: usize = 6;

fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The code for `seed` (deterministic; see the module docs before using a predictable seed).
pub fn mint_join_code(seed: u64) -> String {
    let base = JOIN_CODE_ALPHABET.len() as u64;
    let mut n = mix(seed);
    let mut out = String::with_capacity(JOIN_CODE_LEN);
    for _ in 0..JOIN_CODE_LEN {
        out.push(char::from(JOIN_CODE_ALPHABET[(n % base) as usize]));
        n /= base;
    }
    out
}

/// The canonical (uppercase, no whitespace) form of a typed code, or `None` when it is not a code.
pub fn normalize_join_code(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(JOIN_CODE_LEN);
    for ch in raw.chars().filter(|c| !c.is_whitespace() && *c != '-') {
        let upper = ch.to_ascii_uppercase();
        if !upper.is_ascii() || !JOIN_CODE_ALPHABET.contains(&(upper as u8)) {
            return None;
        }
        out.push(upper);
        if out.len() > JOIN_CODE_LEN {
            return None;
        }
    }
    (out.len() == JOIN_CODE_LEN).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_codes_are_valid_and_spread() {
        let codes: Vec<String> = (0..1000u64).map(mint_join_code).collect();
        for c in &codes {
            assert_eq!(normalize_join_code(c).as_deref(), Some(c.as_str()));
        }
        let mut unique = codes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), codes.len(), "no collisions in the first 1000 seeds");
        assert_ne!(
            codes[0][..3],
            codes[1][..3],
            "neighboring seeds do not share a prefix"
        );
    }

    #[test]
    fn normalize_accepts_typing_variants_and_rejects_lookalikes() {
        assert_eq!(normalize_join_code(" abc-def ").as_deref(), Some("ABCDEF"));
        assert!(normalize_join_code("ABCDE").is_none());
        assert!(normalize_join_code("ABCDEFG").is_none());
        assert!(normalize_join_code("ABCDE0").is_none());
        assert!(normalize_join_code("ABCDEI").is_none());
        assert!(normalize_join_code("ABCDEé").is_none());
    }
}
