//! UI-facing model (serde-friendly snapshots for polling).

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionPhase {
    Starting,
    WaitingForHopspot,
    Connected,
    Failed(String),
}

impl ConnectionPhase {
    pub fn label(&self) -> &str {
        match self {
            Self::Starting => "Starting…",
            Self::WaitingForHopspot => "Waiting for Hopspot",
            Self::Connected => "Connected (LocalClient)",
            Self::Failed(_) => "Failed",
        }
    }

    pub fn class_name(&self) -> &'static str {
        match self {
            Self::Connected => "status-ok",
            Self::WaitingForHopspot | Self::Starting => "status-warn",
            Self::Failed(_) => "status-bad",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeardAnnounce {
    pub destination_hex: String,
    pub hops: u8,
    pub interface: String,
    /// Local time when this peer's announce was last heard.
    pub at: String,
    pub seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatDirection {
    Out,
    In,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatLine {
    pub direction: ChatDirection,
    pub peer_hex: String,
    pub text: String,
    pub status: String,
    pub at: String,
    pub seq: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RangePromptKind {
    OneShot,
    Auto,
}

/// Role in an auto range-check session.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AutoRangeRole {
    /// Initiator: sends `Range check` 10s after each completed ping.
    Driver,
    /// Acceptor: silent auto-replies only (does not ping).
    Responder,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutoRangeSession {
    pub peer_hex: String,
    pub role: AutoRangeRole,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RangePrompt {
    pub peer_hex: String,
    pub latitude: f64,
    pub longitude: f64,
    pub kind: RangePromptKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub phase: ConnectionPhase,
    pub destination_hex: Option<String>,
    pub bus: String,
    pub announce_count: u64,
    pub last_announce: Option<String>,
    pub heard: Vec<HeardAnnounce>,
    pub messages: Vec<ChatLine>,
    pub pending_range_prompt: Option<RangePrompt>,
    /// Silent auto-reply owed while an auto session is active (no Accept modal).
    pub pending_auto_reply: Option<RangePrompt>,
    /// Active auto range-check session, if any.
    pub auto_range: Option<AutoRangeSession>,
    pub live: bool,
}

impl Snapshot {
    #[cfg(not(feature = "live"))]
    pub fn sample() -> Self {
        Self {
            phase: ConnectionPhase::WaitingForHopspot,
            destination_hex: Some("0123456789abcdef0123456789abcdef".into()),
            bus: "127.0.0.1:37428 (mock)".into(),
            announce_count: 0,
            last_announce: None,
            heard: vec![HeardAnnounce {
                destination_hex: "aabbccddeeff00112233445566778899".into(),
                hops: 1,
                interface: "local-client · aabbccddeeff00".into(),
                at: "Aug 22, 9:30:00 AM".into(),
                seq: 1,
            }],
            messages: vec![],
            pending_range_prompt: None,
            pending_auto_reply: None,
            auto_range: None,
            live: false,
        }
    }
}

pub fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Strip whitespace and optional angle brackets from a pasted destination hash.
pub fn normalize_dest_hex(input: &str) -> String {
    input
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect()
}

pub fn parse_dest_hex(hex: &str) -> Result<[u8; 16], String> {
    let hex = normalize_dest_hex(hex);
    if hex.len() != 32 {
        return Err("Destination hash must be 32 hex characters.".into());
    }
    let mut out = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|_| "bad hex".to_string())?;
        out[i] = u8::from_str_radix(s, 16).map_err(|_| format!("bad hex at byte {i}"))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_dest_hex_strips_brackets_and_spaces() {
        assert_eq!(
            normalize_dest_hex("  <86c08aa4 a982a566 aca7425b e56ae478>  "),
            "86c08aa4a982a566aca7425be56ae478"
        );
    }

    #[test]
    fn parse_dest_hex_accepts_bracketed_paste() {
        let bytes = parse_dest_hex("<86c08aa4a982a566aca7425be56ae478>").expect("parse");
        assert_eq!(hex_bytes(&bytes), "86c08aa4a982a566aca7425be56ae478");
    }
}
