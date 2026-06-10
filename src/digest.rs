//! Strongly-typed OCI content digest values.
//!
//! Spec: <https://github.com/opencontainers/image-spec/blob/main/descriptor.md#digests>
//!
//! A digest takes the form `algorithm:hex` where `algorithm` is a token
//! (e.g. `sha256`, `sha512`) and the hex portion is the lowercase hex
//! encoding of the digest produced by that algorithm.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error;

/// A validated `algorithm:hex` digest reference.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest {
    raw: String,
    algo_end: usize,
}

impl Digest {
    /// Parse the input string into a digest, validating the form
    /// `algorithm:hex_digits` where `hex_digits` length matches the algorithm
    /// (when known).
    pub fn parse(input: &str) -> crate::Result<Self> {
        let (algo, hex) = input
            .split_once(':')
            .ok_or_else(|| error::Error::InvalidDigest {
                reason: format!("expected '<algorithm>:<hex>', got '{input}'"),
            })?;
        if algo.is_empty() {
            return Err(error::Error::InvalidDigest {
                reason: "algorithm component is empty".to_string(),
            });
        }
        if hex.is_empty() {
            return Err(error::Error::InvalidDigest {
                reason: "hex component is empty".to_string(),
            });
        }
        if !algo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-' || c == '_')
        {
            return Err(error::Error::InvalidDigest {
                reason: format!("invalid characters in algorithm '{algo}'"),
            });
        }
        if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(error::Error::InvalidDigest {
                reason: format!("hex component '{hex}' contains non-hex characters"),
            });
        }
        let expected_len = match algo {
            "sha256" => Some(64),
            "sha512" => Some(128),
            _ => None,
        };
        if let Some(expected) = expected_len
            && hex.len() != expected
        {
            return Err(error::Error::InvalidDigest {
                reason: format!(
                    "expected {} hex characters for {}, got {}",
                    expected,
                    algo,
                    hex.len()
                ),
            });
        }
        Ok(Self {
            raw: input.to_string(),
            algo_end: algo.len(),
        })
    }

    /// The algorithm component (e.g. `sha256`).
    pub fn algorithm(&self) -> &str {
        &self.raw[..self.algo_end]
    }

    /// The hex-encoded digest value (lowercase, validated).
    pub fn hex(&self) -> &str {
        // +1 for the colon separator.
        &self.raw[self.algo_end + 1..]
    }

    /// The full canonical `algorithm:hex` representation.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// First N hex characters of the digest, used as a short label.
    pub fn short(&self, n: usize) -> &str {
        let hex = self.hex();
        &hex[..n.min(hex.len())]
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl FromStr for Digest {
    type Err = crate::error::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_sha256() {
        let s = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let d = Digest::parse(s).expect("should parse");
        assert_eq!(d.algorithm(), "sha256");
        assert_eq!(d.hex().len(), 64);
        assert_eq!(d.as_str(), s);
        assert_eq!(d.short(9).len(), 9);
    }

    #[test]
    fn parse_valid_sha512() {
        let hex = "a".repeat(128);
        let s = format!("sha512:{hex}");
        let d = Digest::parse(&s).expect("sha512 should parse");
        assert_eq!(d.algorithm(), "sha512");
        assert_eq!(d.hex().len(), 128);
    }

    #[test]
    fn parse_unknown_algo_skips_length_check() {
        // Unknown algorithms accept any hex length so we stay forward-compat
        // with future spec algorithms.
        let d = Digest::parse("blake3:dead").expect("should parse");
        assert_eq!(d.algorithm(), "blake3");
        assert_eq!(d.hex(), "dead");
    }

    #[test]
    fn rejects_missing_colon() {
        assert!(Digest::parse("sha256-abcdef").is_err());
    }

    #[test]
    fn rejects_empty_algorithm() {
        assert!(Digest::parse(":abcd").is_err());
    }

    #[test]
    fn rejects_empty_hex() {
        assert!(Digest::parse("sha256:").is_err());
    }

    #[test]
    fn rejects_wrong_sha256_length() {
        assert!(Digest::parse("sha256:abc").is_err());
    }

    #[test]
    fn rejects_non_hex() {
        let s = "sha256:".to_string() + &"z".repeat(64);
        assert!(Digest::parse(&s).is_err());
    }

    #[test]
    fn random_strings_never_panic() {
        // Deterministic pseudo-random strings; we only care that parse
        // returns Result without panicking.
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        for _ in 0..1000 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = (seed % 80) as usize;
            let s: String = (0..len)
                .map(|i| {
                    let b = ((seed >> (i % 56)) & 0xff) as u8;
                    char::from(b.clamp(0x20, 0x7e))
                })
                .collect();
            let _ = Digest::parse(&s);
        }
    }
}
