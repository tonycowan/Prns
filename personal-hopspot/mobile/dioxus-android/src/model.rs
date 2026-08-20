//! Mock Hopspot management model mirroring core face concepts.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Kept for future live engine mapping.
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Kept for future live engine mapping.
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Kept for future live engine mapping.
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct InterfaceCard {
    pub id: u32,
    pub kind: InterfaceKind,
    pub connection: ConnectionState,
    pub failure_reason: Option<&'static str>,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub links: u32,
    pub peers: Option<u32>,
    pub destinations: u32,
    pub activity_age: Option<&'static str>,
    pub detail_lines: Vec<&'static str>,
}

impl InterfaceCard {
    pub fn subtitle(&self) -> String {
        if let Some(reason) = self.failure_reason {
            return reason.to_string();
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct LimitRow {
    pub name: &'static str,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub message: String,
    pub shown_at: Instant,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DemoState {
    pub engine: EngineState,
    pub uptime: &'static str,
    pub interface_count: u32,
    pub online_interface_count: u32,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub cards: Vec<InterfaceCard>,
    pub limits: Vec<LimitRow>,
    pub sleeping: bool,
    pub notice: Option<Notice>,
    pub rns_config: String,
}

impl DemoState {
    pub fn sample() -> Self {
        Self {
            engine: EngineState::Running,
            uptime: "1h 12m",
            interface_count: 6,
            online_interface_count: 4,
            rx_bytes: 1_842_112,
            tx_bytes: 923_441,
            cards: sample_cards(),
            limits: sample_limits(),
            sleeping: false,
            notice: None,
            rns_config: sample_rns_config(),
        }
    }

    pub fn flash(&mut self, message: impl Into<String>) {
        self.notice = Some(Notice {
            message: message.into(),
            shown_at: Instant::now(),
        });
    }

    pub fn clear_stale_notice(&mut self) {
        if let Some(notice) = &self.notice {
            if notice.shown_at.elapsed() > Duration::from_secs(2) {
                self.notice = None;
            }
        }
    }

    pub fn announce(&mut self) {
        self.flash("Announcing");
    }

    pub fn toggle_sleep(&mut self) {
        self.sleeping = !self.sleeping;
        if self.sleeping {
            self.flash("Sleeping");
        } else {
            self.flash("Awake");
        }
    }

    pub fn toggle_power(&mut self, id: u32) {
        let Some(index) = self.cards.iter().position(|card| card.id == id) else {
            return;
        };
        let label = self.cards[index].kind.label();
        if self.cards[index].connection.is_powered_on() {
            self.cards[index].connection = ConnectionState::Disabled;
            self.cards[index].failure_reason = None;
            self.flash(format!("{label} off"));
        } else {
            self.cards[index].connection = ConnectionState::Connected;
            self.flash(format!("{label} on"));
        }
        self.recount_online();
    }

    pub fn copy_rns_config(&mut self) {
        self.flash("RNS config copied");
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

fn sample_cards() -> Vec<InterfaceCard> {
    vec![
        InterfaceCard {
            id: 1,
            kind: InterfaceKind::Usb,
            connection: ConnectionState::Disconnected,
            failure_reason: None,
            tx_bytes: 0,
            rx_bytes: 0,
            links: 0,
            peers: None,
            destinations: 0,
            activity_age: None,
            detail_lines: vec!["Peer: none", "Waiting for accessory"],
        },
        InterfaceCard {
            id: 2,
            kind: InterfaceKind::Lan,
            connection: ConnectionState::Connected,
            failure_reason: None,
            tx_bytes: 512_400,
            rx_bytes: 1_204_800,
            links: 3,
            peers: Some(2),
            destinations: 8,
            activity_age: Some("12s"),
            detail_lines: vec!["AutoInterface on WLAN0", "Peers: 2 live"],
        },
        InterfaceCard {
            id: 3,
            kind: InterfaceKind::Ble,
            connection: ConnectionState::Connected,
            failure_reason: None,
            tx_bytes: 88_120,
            rx_bytes: 64_200,
            links: 1,
            peers: Some(1),
            destinations: 3,
            activity_age: Some("4s"),
            detail_lines: vec!["Bluetooth Auto", "Recovery: idle"],
        },
        InterfaceCard {
            id: 4,
            kind: InterfaceKind::WifiAware,
            connection: ConnectionState::Failed,
            failure_reason: Some("Wi-Fi Aware unavailable"),
            tx_bytes: 0,
            rx_bytes: 0,
            links: 0,
            peers: None,
            destinations: 0,
            activity_age: None,
            detail_lines: vec!["Platform link did not start"],
        },
        InterfaceCard {
            id: 5,
            kind: InterfaceKind::Local,
            connection: ConnectionState::Connected,
            failure_reason: None,
            tx_bytes: 220_000,
            rx_bytes: 310_000,
            links: 2,
            peers: Some(2),
            destinations: 2,
            activity_age: Some("1s"),
            detail_lines: vec![
                "Shared instance TCP 127.0.0.1:37428",
                "RPC control 37429",
                "Fleet members roll into Local",
            ],
        },
        InterfaceCard {
            id: 6,
            kind: InterfaceKind::App,
            connection: ConnectionState::Connected,
            failure_reason: None,
            tx_bytes: 40_000,
            rx_bytes: 55_000,
            links: 1,
            peers: None,
            destinations: 1,
            activity_age: Some("8s"),
            detail_lines: vec!["Local client of this shared instance"],
        },
    ]
}

fn sample_limits() -> Vec<LimitRow> {
    vec![
        LimitRow {
            name: "Destinations",
            value: "128".into(),
        },
        LimitRow {
            name: "Announces",
            value: "64".into(),
        },
        LimitRow {
            name: "Links",
            value: "32".into(),
        },
        LimitRow {
            name: "MTU",
            value: "500".into(),
        },
        LimitRow {
            name: "Resource buffer",
            value: "16 KiB".into(),
        },
        LimitRow {
            name: "Receipts",
            value: "48".into(),
        },
    ]
}

fn sample_rns_config() -> String {
    "# This template is used to generate a\n\
     # running configuration for Sideband's\n\
     # internal RNS instance.\n\
     \n\
     [reticulum]\n\
       enable_transport = TRANSPORT_IS_ENABLED\n\
       local_hops_delta = LOCAL_HOPS_DELTA\n\
       share_instance = Yes\n\
       shared_instance_type = tcp\n\
       instance_control_port = 37429\n\
       rpc_key = <device-local-key>\n\
       panic_on_interface_error = No\n\
     \n\
     [logging]\n\
       loglevel = 3\n\
     \n\
     [interfaces]\n"
        .into()
}
