//! Structured UI snapshot for the Dioxus management face.
//!
//! Same data path as [`crate::face::HopspotFace`]: `interface_snapshots` →
//! `snapshots_to_cards` / peer rows, plus health and join metadata.

use core::fmt::Write as _;

use personal_hopspot_core::{snapshots_to_cards, Card, CardKind};
use personal_rns::interfaces::{ConnectionState, InterfaceId, InterfaceSnapshot, Membership};
use personal_rns::storage::{GrowableHeap, StorageCapacity, StorageLayout};
use serde::Serialize;

use crate::engine::{
    classify, engine_state, interface_snapshots, last_failure, rpc_key_hex, runtime_health,
    LOCAL_RNS_PORT, RPC_PORT,
};

const MAX_CARDS: usize = 16;

#[derive(Clone, Debug, Serialize)]
pub struct UiLiveSnapshot {
    pub engine: &'static str,
    pub engine_failure: Option<&'static str>,
    pub uptime_ms: u64,
    pub interface_count: u32,
    pub online_interface_count: u32,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub local_rns_port: u16,
    pub rpc_port: u16,
    pub rpc_key_hex: Option<String>,
    pub cards: Vec<UiLiveCard>,
    pub limits: Vec<UiLiveLimit>,
    pub rns_config: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct UiLiveCard {
    pub id: String,
    pub kind: &'static str,
    pub label: String,
    pub connection: &'static str,
    pub failure_reason: Option<&'static str>,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub links: u32,
    pub peers: Option<u32>,
    pub destinations: u32,
    pub activity_age_secs: Option<u32>,
    pub detail_lines: Vec<String>,
    pub peer_list: Vec<UiLivePeer>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UiLivePeer {
    pub label: String,
    pub connection: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct UiLiveLimit {
    pub name: &'static str,
    pub value: String,
}

#[must_use]
pub fn capture_ui_live_snapshot() -> UiLiveSnapshot {
    let snapshots = interface_snapshots();
    let cards = snapshots_to_cards::<MAX_CARDS>(&snapshots, classify);
    let health = runtime_health();
    let rpc_key = rpc_key_hex();
    let engine = match engine_state() {
        personal_hopspot_core::MobileEngineState::Stopped => "stopped",
        personal_hopspot_core::MobileEngineState::Starting => "starting",
        personal_hopspot_core::MobileEngineState::Running => "running",
        personal_hopspot_core::MobileEngineState::Failed => "failed",
    };
    let engine_failure = match last_failure() {
        personal_hopspot_core::MobileEngineFailure::None => None,
        other => Some(other.wire_name()),
    };

    let live_cards: Vec<UiLiveCard> = cards
        .iter()
        .map(|card| map_card(card, &snapshots))
        .collect();

    UiLiveSnapshot {
        engine,
        engine_failure,
        uptime_ms: health.as_ref().map(|h| h.uptime_millis).unwrap_or(0),
        interface_count: health.as_ref().map(|h| h.interface_count).unwrap_or(0),
        online_interface_count: health
            .as_ref()
            .map(|h| h.online_interface_count)
            .unwrap_or(0),
        rx_bytes: health.as_ref().map(|h| h.rx_bytes).unwrap_or(0),
        tx_bytes: health.as_ref().map(|h| h.tx_bytes).unwrap_or(0),
        local_rns_port: LOCAL_RNS_PORT,
        rpc_port: RPC_PORT,
        rpc_key_hex: rpc_key.clone(),
        cards: live_cards,
        limits: static_limits(),
        rns_config: rns_config_template(rpc_key.as_deref()),
    }
}

#[must_use]
pub fn capture_ui_live_snapshot_json() -> String {
    serde_json::to_string(&capture_ui_live_snapshot()).unwrap_or_else(|_| "{}".into())
}

#[must_use]
pub fn parse_interface_id_hex(hex: &str) -> Option<InterfaceId> {
    let hex = hex.trim();
    if hex.len() != 16 {
        return None;
    }
    let mut bytes = [0u8; 8];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = from_hex_nibble(chunk[0])?;
        let lo = from_hex_nibble(chunk[1])?;
        bytes[i] = (hi << 4) | lo;
    }
    Some(InterfaceId::new(bytes))
}

fn from_hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn map_card(card: &Card, snapshots: &[InterfaceSnapshot]) -> UiLiveCard {
    let id = hex_id(card.id());
    let peer_list = peers_for(card.id(), snapshots);
    let detail_lines = detail_lines_for(card);
    UiLiveCard {
        id,
        kind: kind_wire(card.kind(), card.label()),
        label: card.label().to_string(),
        connection: connection_wire(card.connection()),
        failure_reason: card.failure_reason(),
        tx_bytes: card.tx_bytes(),
        rx_bytes: card.rx_bytes(),
        links: card.links(),
        peers: card.peers(),
        destinations: card.destinations(),
        activity_age_secs: card.last_activity_secs(),
        detail_lines,
        peer_list,
    }
}

fn peers_for(supervisor: InterfaceId, snapshots: &[InterfaceSnapshot]) -> Vec<UiLivePeer> {
    snapshots
        .iter()
        .filter_map(|snapshot| {
            if let Membership::FleetMember { supervisor_id } = snapshot.membership {
                if supervisor_id == supervisor {
                    let bytes = snapshot.id.as_bytes();
                    let mut label = String::new();
                    let _ = write!(label, "{:02x}{:02x}", bytes[1], bytes[2]);
                    return Some(UiLivePeer {
                        label,
                        connection: connection_wire(snapshot.connection),
                    });
                }
            }
            None
        })
        .collect()
}

fn detail_lines_for(card: &Card) -> Vec<String> {
    match card.kind() {
        CardKind::Tcp if card.label() == "Local" => vec![
            format!("Shared instance TCP 127.0.0.1:{LOCAL_RNS_PORT}"),
            format!("RPC control {RPC_PORT}"),
        ],
        CardKind::Peer if card.label() == "App" => {
            vec!["Local client of this shared instance".into()]
        }
        _ => Vec::new(),
    }
}

fn kind_wire(kind: CardKind, label: &str) -> &'static str {
    match kind {
        CardKind::Usb => "usb",
        CardKind::Wifi | CardKind::WifiStation | CardKind::WifiStationDisabled => match label {
            "Direct" => "wifi_direct",
            "Aware" => "wifi_aware",
            _ => "lan",
        },
        CardKind::Ble => "ble",
        CardKind::Tcp => "local",
        CardKind::Peer => "app",
        CardKind::LoRa => "lora",
        CardKind::EspNow => "espnow",
    }
}

fn connection_wire(connection: ConnectionState) -> &'static str {
    match connection {
        ConnectionState::Initializing => "initializing",
        ConnectionState::Connected => "connected",
        ConnectionState::Degraded => "degraded",
        ConnectionState::Reconnecting => "reconnecting",
        ConnectionState::Failed => "failed",
        ConnectionState::Disconnected => "disconnected",
        ConnectionState::Disabled => "disabled",
        ConnectionState::Unknown => "unknown",
    }
}

fn hex_id(id: InterfaceId) -> String {
    let mut out = String::with_capacity(16);
    for byte in id.as_bytes() {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn capacity_value(capacity: StorageCapacity) -> String {
    match capacity {
        StorageCapacity::Fixed(n) => n.to_string(),
        StorageCapacity::Dynamic => "dyn".into(),
    }
}

fn static_limits() -> Vec<UiLiveLimit> {
    let limits = <GrowableHeap as StorageLayout>::LIMITS;
    vec![
        UiLiveLimit {
            name: "Destinations",
            value: capacity_value(limits.tracked_destinations),
        },
        UiLiveLimit {
            name: "Announces",
            value: capacity_value(limits.announce_records),
        },
        UiLiveLimit {
            name: "Links",
            value: capacity_value(limits.links),
        },
        UiLiveLimit {
            name: "Channels",
            value: capacity_value(limits.channels),
        },
    ]
}

fn rns_config_template(rpc_key_hex: Option<&str>) -> String {
    let key = rpc_key_hex.unwrap_or("<device-local-key>");
    format!(
        "# This template is used to generate a\n\
         # running configuration for Sideband's\n\
         # internal RNS instance.\n\
         \n\
         [reticulum]\n\
           enable_transport = TRANSPORT_IS_ENABLED\n\
           local_hops_delta = LOCAL_HOPS_DELTA\n\
           share_instance = Yes\n\
           shared_instance_type = tcp\n\
           instance_control_port = {RPC_PORT}\n\
           rpc_key = {key}\n\
           panic_on_interface_error = No\n\
         \n\
         [logging]\n\
           loglevel = 3\n\
         \n\
         [interfaces]\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_interface_id_hex() {
        let id = InterfaceId::new([0xab, 0xcd, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
        let hex = hex_id(id);
        assert_eq!(hex, "abcd010203040506");
        assert_eq!(parse_interface_id_hex(&hex), Some(id));
        assert_eq!(parse_interface_id_hex("nope"), None);
    }

    #[test]
    fn snapshot_json_is_object_when_engine_stopped() {
        let json = capture_ui_live_snapshot_json();
        assert!(json.starts_with('{'));
        assert!(json.contains("\"engine\""));
        assert!(json.contains("\"cards\""));
    }
}
