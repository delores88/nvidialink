//! Four-word codes from the 2048-word BIP-39 English list (44 bits).
//!
//! Used for (a) the pairing short-authentication string shown on the joiner
//! and matched on the admin's approval card (item 1) and (b) node fingerprint
//! words shown on the admin QR screen and the joiner's confirmation dialog
//! (item 9). Words are derived with HKDF-SHA256 (see `pairing`), then the
//! first 44 bits of the 6-byte output, big-endian, are split into four 11-bit
//! indices.

use serde::{Deserialize, Serialize};

/// Four lowercase BIP-39 English words.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Words(pub [String; 4]);

impl Words {
    pub fn joined(&self) -> String {
        self.0.join(" ")
    }
    /// Six decimal digits for human entry: the four concatenated 11-bit indices
    /// modulo 1,000,000, zero-padded. This deliberately reduces the 44-bit SAS;
    /// it is not a lossless encoding or a standalone authentication credential.
    /// Owner approval also requires the pinned device, ticket token/proof and a
    /// bounded ticket-wide failure budget. The original words remain unchanged.
    /// Invalid deserialized words have no code; never invent one or panic.
    pub fn verification_code(&self) -> Option<String> {
        let mut value = 0_u64;
        for word in &self.0 {
            value = (value << 11) | u64::from(index_of(word)?);
        }
        Some(format!("{:06}", value % 1_000_000))
    }
    /// Parse "w1 w2 w3 w4" (case-insensitive, any whitespace); every word must
    /// be on the list.
    pub fn parse(s: &str) -> Option<Words> {
        let v: Vec<String> = s.split_whitespace().map(|w| w.to_ascii_lowercase()).collect();
        if v.len() != 4 || v.iter().any(|w| index_of(w).is_none()) {
            return None;
        }
        Some(Words([v[0].clone(), v[1].clone(), v[2].clone(), v[3].clone()]))
    }
}

pub fn wordlist() -> &'static [&'static str; 2048] {
    bip39::Language::English.word_list()
}

pub fn index_of(word: &str) -> Option<u16> {
    bip39::Language::English.find_word(word)
}

/// Four 11-bit indices from the first 44 bits of `b` (big-endian).
pub fn indices_from_bytes(b: &[u8; 6]) -> [u16; 4] {
    let mut acc: u64 = 0;
    for byte in b {
        acc = (acc << 8) | u64::from(*byte);
    }
    // 48 bits in acc; take the top 44.
    let top = acc >> 4;
    [
        ((top >> 33) & 0x7ff) as u16,
        ((top >> 22) & 0x7ff) as u16,
        ((top >> 11) & 0x7ff) as u16,
        (top & 0x7ff) as u16,
    ]
}

pub fn words_from_bytes(b: &[u8; 6]) -> Words {
    let list = wordlist();
    let idx = indices_from_bytes(b);
    Words(idx.map(|i| list[i as usize].to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indices() {
        assert_eq!(indices_from_bytes(&[0, 0, 0, 0, 0, 0]), [0, 0, 0, 0]);
        assert_eq!(indices_from_bytes(&[0xff; 6]), [2047; 4]);
        // 0x002 << 33 ... first index 1: bits 00000000001 then zeros
        assert_eq!(indices_from_bytes(&[0x00, 0x20, 0, 0, 0, 0]), [1, 0, 0, 0]);
        assert_eq!(wordlist()[0], "abandon");
        assert_eq!(wordlist()[2047], "zoo");
        assert_eq!(Words::parse("Abandon ability  able about").unwrap().joined(), "abandon ability able about");
        assert!(Words::parse("abandon ability able notaword").is_none());
    }

    #[test]
    fn verification_code_exact_vectors() {
        assert_eq!(words_from_bytes(&[0; 6]).verification_code().as_deref(), Some("000000"));
        assert_eq!(words_from_bytes(&[0xff; 6]).verification_code().as_deref(), Some("044415"));
        let distinct = Words([1, 2, 3, 4].map(|index| wordlist()[index].to_owned()));
        assert_eq!(distinct.verification_code().as_deref(), Some("329348"));
        for position in 0..4 {
            let mut invalid = words_from_bytes(&[0; 6]);
            invalid.0[position] = "not-a-bip39-word".into();
            assert_eq!(invalid.verification_code(), None);
        }
    }

    #[test]
    fn verification_code_modulo_boundaries_preserve_original_words() {
        for (value, expected) in [(999_999_u64, "999999"), (1_000_000, "000000"), (1_000_001, "000001")] {
            let full = (value << 4).to_be_bytes();
            let bytes: [u8; 6] = full[2..].try_into().unwrap();
            let words = words_from_bytes(&bytes);
            let original = words.clone();
            let code = words.verification_code().unwrap();
            assert_eq!(code, expected);
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
            assert_eq!(words, original);
        }
    }
}
