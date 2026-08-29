//! Hopspot management model: mock sample data + live snapshot apply.

use serde::Deserialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineState {
    Stopped,
    Starting,
    Running,
    Failed,
}

impl EngineState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "Stopped",
            Self::Starting => "Starting",
            Self::Running => "Running",
            Self::Failed => "Failed",
        }
    }

    pub fn chip_class(self) -> &'static str {
        match self {
            Self::Running => "ok",
            Self::Starting => "warn",
            Self::Failed => "bad",
            Self::Stopped => "off",
        }
    }

    pub fn from_wire(value: &str) -> Self {
        match value {
            "starting" => Self::Starting,
            "running" => Self::Running,
            "failed" => Self::Failed,
            _ => Self::Stopped,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Initializing,
    Connected,
    Degraded,
    Reconnecting,
    Failed,
    Disconnected,
    Disabled,
    Unknown,
}

impl ConnectionState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Initializing => "Initializing",
            Self::Connected => "Connected",
            Self::Degraded => "Degraded",
            Self::Reconnecting => "Retrying",
            Self::Failed => "Failed",
            Self::Disconnected => "Disconnected",
            Self::Disabled => "Off",
            Self::Unknown => "Unknown",
        }
    }

    pub fn short_label(self) -> &'static str {
        match self {
            Self::Initializing => "Init",
            Self::Connected => "Live",
            Self::Degraded => "Degr",
            Self::Reconnecting => "Retry",
            Self::Failed => "Fail",
            Self::Disconnected => "Disc",
            Self::Disabled => "Off",
            Self::Unknown => "Unkn",
        }
    }

    pub fn chip_class(self) -> &'static str {
        match self {
            Self::Connected => "ok",
            Self::Degraded | Self::Reconnecting | Self::Initializing => "warn",
            Self::Failed => "bad",
            Self::Disconnected | Self::Disabled | Self::Unknown => "off",
        }
    }

    pub fn is_powered_on(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub fn from_wire(value: &str) -> Self {
        match value {
            "initializing" => Self::Initializing,
            "connected" => Self::Connected,
            "degraded" => Self::Degraded,
            "reconnecting" => Self::Reconnecting,
            "failed" => Self::Failed,
            "disconnected" => Self::Disconnected,
            "disabled" => Self::Disabled,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterfaceKind {
    Usb,
    Lan,
    Ble,
    WifiDirect,
    WifiAware,
    Local,
    App,
    LoRa,
}

impl InterfaceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Usb => "USB",
            Self::Lan => "LAN",
            Self::Ble => "BLE",
            Self::WifiDirect => "Direct",
            Self::WifiAware => "Aware",
            Self::Local => "Local",
            Self::App => "App",
            Self::LoRa => "LoRa",
        }
    }

    pub fn short_icon(self) -> &'static str {
        match self {
            Self::Usb => "USB",
            Self::Lan => "LAN",
            Self::Ble => "BLE",
            Self::WifiDirect => "P2P",
            Self::WifiAware => "NAN",
            Self::Local => "TCP",
            Self::App => "APP",
            Self::LoRa => "LR",
        }
    }

    pub fn from_wire(value: &str) -> Self {
        match value {
            "usb" => Self::Usb,
            "ble" => Self::Ble,
            "wifi_direct" => Self::WifiDirect,
            "wifi_aware" => Self::WifiAware,
            "local" => Self::Local,
            "app" => Self::App,
            "lora" => Self::LoRa,
            _ => Self::Lan,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PeerInfo {
    pub label: String,
    pub connection: ConnectionState,
}

impl PeerInfo {
    pub fn new(label: impl Into<String>, connection: ConnectionState) -> Self {
        Self {
            label: label.into(),
            connection,
        }
    }

    pub fn row_label(&self) -> String {
        format!("P {} {}", self.label, self.connection.short_label())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct InterfaceCard {
    pub id: String,
    pub kind: InterfaceKind,
    pub connection: ConnectionState,
    pub failure_reason: Option<String>,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub links: u32,
    pub peers: Option<u32>,
    pub destinations: u32,
    pub activity_age: Option<String>,
    pub detail_lines: Vec<String>,
    pub peer_list: Vec<PeerInfo>,
}

impl InterfaceCard {
    pub fn subtitle(&self) -> String {
        if let Some(reason) = &self.failure_reason {
            return reason.clone();
        }
        if self.connection == ConnectionState::Connected {
            let peers = self.peers.unwrap_or(self.destinations);
            return format!(
                "↑ {} · ↓ {} · {} links · {} peers",
                fmt_bytes(self.tx_bytes),
                fmt_bytes(self.rx_bytes),
                self.links,
                peers
            );
        }
        self.connection.label().to_string()
    }

    pub fn connected_peers(&self) -> Vec<&PeerInfo> {
        self.peer_list
            .iter()
            .filter(|peer| {
                matches!(
                    peer.connection,
                    ConnectionState::Connected | ConnectionState::Degraded
                )
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LimitRow {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DemoState {
    pub engine: EngineState,
    pub uptime: String,
    pub interface_count: u32,
    pub online_interface_count: u32,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub cards: Vec<InterfaceCard>,
    pub limits: Vec<LimitRow>,
    pub sleeping: bool,
    pub rns_config: String,
    /// Live BLE Auto discovery group (Android persistence), when known.
    pub ble_discovery_group: Option<String>,
    /// When true, power/sleep/announce go through the Hopspot service bridge.
    pub live: bool,
}

impl DemoState {
    pub fn sample() -> Self {
        Self {
            engine: EngineState::Running,
            uptime: "1h 12m".into(),
            interface_count: 6,
            online_interface_count: 4,
            rx_bytes: 1_842_112,
            tx_bytes: 923_441,
            cards: sample_cards(),
            limits: sample_limits(),
            sleeping: false,
            rns_config: sample_rns_config(),
            ble_discovery_group: Some("reticulum".into()),
            live: false,
        }
    }

    pub fn announce(&mut self) {
        if self.live {
            crate::backend::announce();
        }
    }

    pub fn toggle_sleep(&mut self) {
        self.sleeping = !self.sleeping;
        if self.live {
            if self.sleeping {
                crate::backend::sleep_interfaces();
            } else {
                crate::backend::wake_interfaces();
            }
        }
    }

    pub fn toggle_power(&mut self, id: &str) {
        let Some(index) = self.cards.iter().position(|card| card.id == id) else {
            return;
        };
        if self.live {
            crate::backend::toggle_interface(id);
        }
        // Optimistic flip so the action button updates before the next snapshot.
        if self.cards[index].connection.is_powered_on() {
            self.cards[index].connection = ConnectionState::Disabled;
            self.cards[index].failure_reason = None;
        } else {
            self.cards[index].connection = ConnectionState::Connected;
        }
        self.recount_online();
    }

    pub fn set_ble_discovery_group(&mut self, group_id: &str) -> bool {
        let group_id = group_id.trim();
        if group_id.is_empty() {
            return false;
        }
        if self.live {
            if !crate::backend::set_ble_discovery_group(group_id) {
                return false;
            }
            self.ble_discovery_group = Some(group_id.to_string());
            return true;
        }
        self.ble_discovery_group = Some(group_id.to_string());
        true
    }

    /// Apply a live snapshot without clobbering ephemeral UI (sleep flag).
    pub fn apply_live_json(&mut self, json: &str) {
        let Ok(snap) = serde_json::from_str::<LiveSnapshotWire>(json) else {
            return;
        };
        let sleeping = self.sleeping;
        *self = snap.into_state();
        self.live = true;
        self.sleeping = sleeping;
    }

    /// Update counters/uptime only — avoids replacing `cards` (which remounts detail UI).
    pub fn apply_live_metrics(&mut self, json: &str) {
        let Ok(snap) = serde_json::from_str::<LiveSnapshotWire>(json) else {
            return;
        };
        self.uptime = fmt_uptime(snap.uptime_ms);
        self.rx_bytes = snap.rx_bytes;
        self.tx_bytes = snap.tx_bytes;
        for card in &mut self.cards {
            if let Some(next) = snap.cards.iter().find(|c| c.id == card.id) {
                card.tx_bytes = next.tx_bytes;
                card.rx_bytes = next.rx_bytes;
                card.activity_age = next.activity_age_secs.map(|secs| format!("{secs}s"));
            }
        }
    }

    fn recount_online(&mut self) {
        self.online_interface_count = self
            .cards
            .iter()
            .filter(|card| card.connection == ConnectionState::Connected)
            .count() as u32;
        self.interface_count = self.cards.len() as u32;
    }
}

#[derive(Debug, Deserialize)]
struct LiveSnapshotWire {
    engine: String,
    uptime_ms: u64,
    interface_count: u32,
    online_interface_count: u32,
    rx_bytes: u64,
    tx_bytes: u64,
    cards: Vec<LiveCardWire>,
    limits: Vec<LiveLimitWire>,
    rns_config: String,
    #[serde(default)]
    ble_discovery_group: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LiveCardWire {
    id: String,
    kind: String,
    connection: String,
    failure_reason: Option<String>,
    tx_bytes: u64,
    rx_bytes: u64,
    links: u32,
    peers: Option<u32>,
    destinations: u32,
    activity_age_secs: Option<u32>,
    detail_lines: Vec<String>,
    peer_list: Vec<LivePeerWire>,
}

#[derive(Debug, Deserialize)]
struct LivePeerWire {
    label: String,
    connection: String,
}

#[derive(Debug, Deserialize)]
struct LiveLimitWire {
    name: String,
    value: String,
}

impl LiveSnapshotWire {
    fn into_state(self) -> DemoState {
        DemoState {
            engine: EngineState::from_wire(&self.engine),
            uptime: fmt_uptime(self.uptime_ms),
            interface_count: self.interface_count,
            online_interface_count: self.online_interface_count,
            rx_bytes: self.rx_bytes,
            tx_bytes: self.tx_bytes,
            cards: self
                .cards
                .into_iter()
                .map(|card| InterfaceCard {
                    id: card.id,
                    kind: InterfaceKind::from_wire(&card.kind),
                    connection: ConnectionState::from_wire(&card.connection),
                    failure_reason: card.failure_reason,
                    tx_bytes: card.tx_bytes,
                    rx_bytes: card.rx_bytes,
                    links: card.links,
                    peers: card.peers,
                    destinations: card.destinations,
                    activity_age: card.activity_age_secs.map(|secs| format!("{secs}s")),
                    detail_lines: card.detail_lines,
                    peer_list: card
                        .peer_list
                        .into_iter()
                        .map(|peer| {
                            PeerInfo::new(peer.label, ConnectionState::from_wire(&peer.connection))
                        })
                        .collect(),
                })
                .collect(),
            limits: self
                .limits
                .into_iter()
                .map(|row| LimitRow {
                    name: row.name,
                    value: row.value,
                })
                .collect(),
            sleeping: false,
            rns_config: self.rns_config,
            ble_discovery_group: self.ble_discovery_group,
            live: true,
        }
    }
}

pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[unit])
    } else if value >= 100.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

fn fmt_uptime(ms: u64) -> String {
    let total_secs = ms / 1000;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

fn sample_cards() -> Vec<InterfaceCard> {
    vec![
        InterfaceCard {
            id: "1".into(),
            kind: InterfaceKind::Usb,
            connection: ConnectionState::Disconnected,
            failure_reason: None,
            tx_bytes: 0,
            rx_bytes: 0,
            links: 0,
            peers: None,
            destinations: 0,
            activity_age: None,
            detail_lines: vec!["Waiting for accessory".into()],
            peer_list: vec![],
        },
        InterfaceCard {
            id: "2".into(),
            kind: InterfaceKind::Lan,
            connection: ConnectionState::Connected,
            failure_reason: None,
            tx_bytes: 512_400,
            rx_bytes: 1_204_800,
            links: 3,
            peers: Some(2),
            destinations: 8,
            activity_age: Some("12s".into()),
            detail_lines: vec!["AutoInterface on WLAN0".into()],
            peer_list: vec![
                PeerInfo::new("a1f3", ConnectionState::Connected),
                PeerInfo::new("0c2e", ConnectionState::Connected),
                PeerInfo::new("77b0", ConnectionState::Disconnected),
            ],
        },
        InterfaceCard {
            id: "3".into(),
            kind: InterfaceKind::Ble,
            connection: ConnectionState::Connected,
            failure_reason: None,
            tx_bytes: 88_120,
            rx_bytes: 64_200,
            links: 1,
            peers: Some(1),
            destinations: 3,
            activity_age: Some("4s".into()),
            detail_lines: vec![],
            peer_list: vec![PeerInfo::new("MacBook", ConnectionState::Connected)],
        },
        InterfaceCard {
            id: "4".into(),
            kind: InterfaceKind::WifiAware,
            connection: ConnectionState::Failed,
            failure_reason: Some("Wi-Fi Aware unavailable".into()),
            tx_bytes: 0,
            rx_bytes: 0,
            links: 0,
            peers: None,
            destinations: 0,
            activity_age: None,
            detail_lines: vec!["Platform link did not start".into()],
            peer_list: vec![],
        },
        InterfaceCard {
            id: "5".into(),
            kind: InterfaceKind::Local,
            connection: ConnectionState::Connected,
            failure_reason: None,
            tx_bytes: 220_000,
            rx_bytes: 310_000,
            links: 2,
            peers: Some(2),
            destinations: 2,
            activity_age: Some("1s".into()),
            detail_lines: vec![
                "Shared instance TCP 127.0.0.1:37428".into(),
                "RPC control 37429".into(),
            ],
            peer_list: vec![
                PeerInfo::new("Sideband", ConnectionState::Connected),
                PeerInfo::new("Termux", ConnectionState::Connected),
            ],
        },
        InterfaceCard {
            id: "6".into(),
            kind: InterfaceKind::App,
            connection: ConnectionState::Connected,
            failure_reason: None,
            tx_bytes: 40_000,
            rx_bytes: 55_000,
            links: 1,
            peers: None,
            destinations: 1,
            activity_age: Some("8s".into()),
            detail_lines: vec!["Local client of this shared instance".into()],
            peer_list: vec![],
        },
    ]
}

fn sample_limits() -> Vec<LimitRow> {
    vec![
        LimitRow {
            name: "Destinations".into(),
            value: "128".into(),
        },
        LimitRow {
            name: "Announces".into(),
            value: "64".into(),
        },
        LimitRow {
            name: "Links".into(),
            value: "32".into(),
        },
        LimitRow {
            name: "MTU".into(),
            value: "500".into(),
        },
        LimitRow {
            name: "Resource buffer".into(),
            value: "16 KiB".into(),
        },
        LimitRow {
            name: "Receipts".into(),
            value: "48".into(),
        },
    ]
}

fn sample_rns_config() -> String {
    // Keep in sync with android rust `ui_live::rns_config_template`.
    String::from(
        "\
# This template is used to generate a
# running configuration for Sideband's
# internal RNS instance. Incorrect changes
# or addition here may cause Sideband to
# fail starting up or working properly.
#
# If Sideband detects that Reticulum
# aborts at startup, due to an error in
# configuration, any template changes
# will be reset to this default.

[reticulum]
  # Don't change these lines, use the UI
  # settings instead. Removing them from
  # the config template will break these
  # settings controls in the UI.
  enable_transport = TRANSPORT_IS_ENABLED
  local_hops_delta = LOCAL_HOPS_DELTA

  # Changing this setting will cause
  # Sideband to not work.
  share_instance = Yes

  # Personal Hopspot
  shared_instance_type = tcp
  shared_instance_port = 37428
  instance_control_port = 37429
  rpc_key = <device-local-key>
  panic_on_interface_error = No

# Logging is controlled by settings
# in the UI, so this section is mostly
# not relevant in Sideband.
[logging]
  loglevel = 3

# No additional interfaces are currently
# defined, but you can use this section
# to add additional custom interfaces.
[interfaces]
",
    )
}
