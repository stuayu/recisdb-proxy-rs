//! Node identity and simple one-time pairing primitives.
//!
//! Transport ownership is independent from Tailscale/Cloudflare identity: a
//! path can change without changing the recisdb node. Pairing establishes an
//! application-level shared credential, so trusting a VPN/tunnel by itself is
//! never the authentication boundary.

use serde::{Deserialize, Serialize};
use std::fmt;

use super::types::NodeId;

fn random_hex<const N: usize>() -> String {
    let mut bytes = [0u8; N];
    getrandom::getrandom(&mut bytes).expect("OS RNG unavailable for node credential");
    let mut out = String::with_capacity(N * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// Long-lived application credential shared by a paired node.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeCredential(String);

impl NodeCredential {
    pub fn random() -> Self {
        Self(random_hex::<32>())
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("node credential must be 32-byte hex");
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Constant-work comparison for fixed-length ASCII credentials.
    pub fn matches(&self, candidate: &str) -> bool {
        if candidate.len() != self.0.len() {
            return false;
        }
        self.0
            .as_bytes()
            .iter()
            .zip(candidate.as_bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }
}

impl fmt::Debug for NodeCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NodeCredential(**redacted**)")
    }
}

/// Short-lived, one-time GUI pairing code. It is random rather than derived
/// from a long-lived secret, so showing it as a QR/code leaks nothing after
/// the pending pairing expires or is consumed.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingCode(String);

impl PairingCode {
    pub fn random() -> Self {
        // 64 bits of random material; format groups improve transcription.
        let raw = random_hex::<8>().to_ascii_uppercase();
        Self(format!("{}-{}-{}-{}", &raw[0..4], &raw[4..8], &raw[8..12], &raw[12..16]))
    }

    pub fn parse(value: &str) -> Result<Self, &'static str> {
        let normalized: String = value
            .chars()
            .filter(|c| *c != '-' && !c.is_whitespace())
            .collect::<String>()
            .to_ascii_uppercase();
        if normalized.len() != 16 || !normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("pairing code must contain 16 hexadecimal characters");
        }
        Ok(Self(format!(
            "{}-{}-{}-{}",
            &normalized[0..4],
            &normalized[4..8],
            &normalized[8..12],
            &normalized[12..16]
        )))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairingCode(**redacted**)")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeIdentity {
    pub node_id: NodeId,
    pub display_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingAcceptance {
    pub identity: NodeIdentity,
    pub credential: NodeCredential,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_debug_never_leaks_secret() {
        let c = NodeCredential::random();
        let raw = c.expose().to_owned();
        assert!(!format!("{c:?}").contains(&raw));
        assert!(c.matches(&raw));
    }

    #[test]
    fn pairing_code_is_human_normalized() {
        let code = PairingCode::parse("abcd ef01-2345 6789").unwrap();
        assert_eq!(code.as_str(), "ABCD-EF01-2345-6789");
    }
}
