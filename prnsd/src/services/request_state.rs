use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use personal_rns::identity::IdentityHash;
use personal_rns::interfaces::{
    ConnectionState, InterfaceId, InterfaceKind, InterfaceMode, InterfaceSnapshot, Membership,
};
use personal_rns::manifold::tokio::TokioInterfaceStatus;
use personal_rns::node_introspection::logical_interface_inventory;
use personal_rns::node_introspection::InterfaceInventoryEntry;
use personal_rns::remote_control::{
    RemoteControlBuildVersion, RemoteControlGroupOutcome, RemoteControlInterfaceCard,
    RemoteControlInterfaceConfigOutcome, RemoteControlInterfaceEntry, RemoteControlInterfaceGroup,
    RemoteControlInterfaceInventory, RemoteControlInterfaceInventoryError,
    RemoteControlInterfacePeer, RemoteControlInterfacePeerPage, RemoteControlInterfacePeersOutcome,
    RemoteControlInterfacePower, RemoteControlModeOutcome, RemoteControlPowerOutcome,
    RemoteControlSleepOutcome, REMOTE_CONTROL_INTERFACE_PEER_CAP,
};
use personal_rns::rns_remote_management::RemoteTransportStatus;
use personal_rns::runtime::{PrnsNodeHandle, RemoteControlHostControls};
use personal_rns::InterfaceStatus;

use crate::nnpages::NnPagesCatalog;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportStatusIdentity {
    pub transport: IdentityHash,
    pub network: Option<IdentityHash>,
    pub probe_responder: Option<personal_rns::wire::DestinationHash>,
}

#[derive(Default)]
pub struct SoftInterfacePowerRegistry {
    statuses: Mutex<HashMap<InterfaceId, TokioInterfaceStatus>>,
}

impl SoftInterfacePowerRegistry {
    #[allow(dead_code)]
    pub fn register(&self, status: TokioInterfaceStatus) {
        let Ok(mut statuses) = self.statuses.lock() else {
            return;
        };
        let id = status.id();
        let _ = statuses.insert(id, status);
    }

    pub fn set_power(
        &self,
        id: InterfaceId,
        power: RemoteControlInterfacePower,
    ) -> RemoteControlPowerOutcome {
        let Ok(statuses) = self.statuses.lock() else {
            return RemoteControlPowerOutcome::Failed;
        };
        let Some(status) = statuses.get(&id) else {
            return RemoteControlPowerOutcome::UnknownInterface;
        };
        match power {
            RemoteControlInterfacePower::On => status.enable(),
            RemoteControlInterfacePower::Off => status.disable(),
        }
        RemoteControlPowerOutcome::Applied
    }

    pub fn sleep_all(&self) -> RemoteControlSleepOutcome {
        let Ok(statuses) = self.statuses.lock() else {
            return RemoteControlSleepOutcome::Failed;
        };
        if statuses.is_empty() {
            return RemoteControlSleepOutcome::Unavailable;
        }
        for status in statuses.values() {
            status.disable();
        }
        RemoteControlSleepOutcome::Applied
    }

    pub fn wake_all(&self) -> RemoteControlSleepOutcome {
        let Ok(statuses) = self.statuses.lock() else {
            return RemoteControlSleepOutcome::Failed;
        };
        if statuses.is_empty() {
            return RemoteControlSleepOutcome::Unavailable;
        }
        for status in statuses.values() {
            status.enable();
        }
        RemoteControlSleepOutcome::Applied
    }
}

#[derive(Clone)]
pub struct DaemonRequestState {
    handle: PrnsNodeHandle,
    transport: Option<TransportStatusIdentity>,
    started: Instant,
    nnpages: NnPagesCatalog,
    soft_power: Arc<SoftInterfacePowerRegistry>,
}

impl DaemonRequestState {
    pub fn new(
        handle: PrnsNodeHandle,
        transport: Option<TransportStatusIdentity>,
        started: Instant,
        nnpages: NnPagesCatalog,
        soft_power: Arc<SoftInterfacePowerRegistry>,
    ) -> Self {
        Self {
            handle,
            transport,
            started,
            nnpages,
            soft_power,
        }
    }

    pub fn handle(&self) -> &PrnsNodeHandle {
        &self.handle
    }

    pub fn nnpages(&self) -> &NnPagesCatalog {
        &self.nnpages
    }

    #[allow(dead_code)]
    pub fn soft_power(&self) -> &Arc<SoftInterfacePowerRegistry> {
        &self.soft_power
    }

    pub fn transport_status(&self) -> Option<RemoteTransportStatus> {
        self.transport.map(|identity| RemoteTransportStatus {
            transport_identity: identity.transport,
            network_identity: identity.network,
            uptime: self.started.elapsed(),
            probe_responder: identity.probe_responder,
            software_version: Some(format!("prnsd {}", env!("CARGO_PKG_VERSION"))),
        })
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn inventory_from_snapshots(
    snapshots: impl IntoIterator<Item = InterfaceSnapshot>,
) -> RemoteControlInterfaceInventory {
    inventory_from_operator_cards(snapshots.into_iter().filter_map(|snapshot| {
        operator_interface(&snapshot).then_some(OperatorCard {
            snapshot,
            name: None,
            group: None,
            config: None,
            failure: None,
            peers: Vec::new(),
        })
    }))
}

struct OperatorCard {
    snapshot: InterfaceSnapshot,
    name: Option<String>,
    group: Option<String>,
    config: Option<String>,
    failure: Option<String>,
    peers: Vec<RemoteControlInterfacePeer>,
}

fn inventory_from_operator_cards(
    cards: impl IntoIterator<Item = OperatorCard>,
) -> RemoteControlInterfaceInventory {
    let mut inventory = RemoteControlInterfaceInventory::empty();
    for card in cards {
        let Some(kind) = card.snapshot.id.kind() else {
            continue;
        };
        let entry = RemoteControlInterfaceEntry {
            id: card.snapshot.id,
            kind,
            mode: card.snapshot.mode,
            connection: card.snapshot.connection,
            enabled: card.snapshot.connection != ConnectionState::Disabled,
            tx_bytes: card.snapshot.tx_bytes,
            rx_bytes: card.snapshot.rx_bytes,
            links: card
                .snapshot
                .links
                .saturating_add(card.snapshot.transported_links),
            rate_bytes_per_sec: rate_bytes_per_sec(&card.snapshot),
        };
        match inventory.push(entry) {
            Ok(()) => {}
            Err(RemoteControlInterfaceInventoryError::Full) => break,
        }
    }
    inventory
}

fn interface_card_from_operator(card: OperatorCard) -> RemoteControlInterfaceCard {
    let mut details = RemoteControlInterfaceCard::empty();
    if let Some(name) = card.name.as_deref() {
        details.set_name(name);
    }
    if let Some(group) = card.group.as_deref() {
        details.set_group(group);
    }
    if let Some(config) = card.config.as_deref() {
        details.set_config(config);
    }
    if let Some(failure) = card.failure.as_deref() {
        details.set_failure(failure);
    }
    details.destinations = card.snapshot.destinations;
    details.transported_links = card.snapshot.transported_links;
    for peer in card.peers {
        match details.push_peer(peer) {
            Ok(()) => {}
            Err(RemoteControlInterfaceInventoryError::Full) => break,
        }
    }
    details
}

fn interface_config_from_host(
    raw: &[InterfaceInventoryEntry],
    id: InterfaceId,
) -> RemoteControlInterfaceConfigOutcome {
    let Some(entry) = logical_interface_inventory(raw.to_vec())
        .into_iter()
        .find(|entry| entry.snapshot.id == id && operator_interface(&entry.snapshot))
    else {
        return RemoteControlInterfaceConfigOutcome::UnknownInterface;
    };
    RemoteControlInterfaceConfigOutcome::Card(interface_card_from_operator(OperatorCard {
        config: interface_config_label(&entry),
        failure: entry.snapshot.failure_reason.map(str::to_string),
        group: entry.group,
        name: entry.name,
        peers: Vec::new(),
        snapshot: entry.snapshot,
    }))
}

fn inventory_from_host(raw: Vec<InterfaceInventoryEntry>) -> RemoteControlInterfaceInventory {
    inventory_from_operator_cards(logical_interface_inventory(raw).into_iter().filter_map(
        |entry| {
            if !operator_interface(&entry.snapshot) {
                return None;
            }
            Some(OperatorCard {
                config: interface_config_label(&entry),
                failure: entry.snapshot.failure_reason.map(str::to_string),
                group: entry.group,
                name: entry.name,
                peers: Vec::new(),
                snapshot: entry.snapshot,
            })
        },
    ))
}

fn interface_config_label(entry: &InterfaceInventoryEntry) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(ifac) = &entry.ifac {
        parts.push(format!("IFAC {}", ifac.size.bytes()));
    }
    if let Some(hint) = interface_kind_config(entry.snapshot.id.kind()) {
        parts.push(hint.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn interface_kind_config(kind: Option<InterfaceKind>) -> Option<&'static str> {
    match kind {
        Some(InterfaceKind::AutoWifi) => Some("Auto discovery"),
        Some(InterfaceKind::LocalServer) => Some("Shared instance"),
        Some(InterfaceKind::TcpServer) => Some("TCP listen"),
        Some(InterfaceKind::TcpClient) => Some("TCP dial"),
        Some(InterfaceKind::WebSocketServer) => Some("WebSocket listen"),
        Some(InterfaceKind::WebSocketClient) => Some("WebSocket dial"),
        Some(InterfaceKind::BackboneServer) => Some("Backbone listen"),
        Some(InterfaceKind::BackboneClient) => Some("Backbone dial"),
        Some(InterfaceKind::BluetoothAuto) => Some("BLE supervisor"),
        Some(InterfaceKind::UsbAutoHost | InterfaceKind::UsbAutoDevice) => Some("USB"),
        Some(InterfaceKind::LoRa | InterfaceKind::Rnode) => Some("LoRa"),
        Some(
            InterfaceKind::Loopback
            | InterfaceKind::Udp
            | InterfaceKind::Serial
            | InterfaceKind::WifiPeer
            | InterfaceKind::LocalClient
            | InterfaceKind::TcpServerPeer
            | InterfaceKind::BluetoothPeer
            | InterfaceKind::Kiss
            | InterfaceKind::Ax25Kiss
            | InterfaceKind::Pipe
            | InterfaceKind::BackboneServerPeer
            | InterfaceKind::EspNow
            | InterfaceKind::WebSocketServerPeer
            | InterfaceKind::WifiDirect
            | InterfaceKind::WifiDirectPeer
            | InterfaceKind::WifiAware
            | InterfaceKind::WifiAwarePeer
            | InterfaceKind::I2p
            | InterfaceKind::I2pPeer
            | InterfaceKind::Weave
            | InterfaceKind::WeavePeer,
        )
        | None => None,
    }
}

fn operator_interface(snapshot: &InterfaceSnapshot) -> bool {
    if matches!(snapshot.membership, Membership::FleetMember { .. }) {
        return false;
    }
    snapshot
        .id
        .kind()
        .is_some_and(|kind| kind.supervisor_kind().is_none())
}

fn interface_peers_from_host(
    raw: &[InterfaceInventoryEntry],
    id: InterfaceId,
    offset: u8,
) -> RemoteControlInterfacePeersOutcome {
    let Some(supervisor) = raw
        .iter()
        .find(|entry| entry.snapshot.id == id && operator_interface(&entry.snapshot))
    else {
        return RemoteControlInterfacePeersOutcome::UnknownInterface;
    };
    let mut total = 0u8;
    let mut page = RemoteControlInterfacePeerPage::empty(supervisor.snapshot.id, offset, 0);
    for member in raw {
        let Membership::FleetMember { supervisor_id } = member.snapshot.membership else {
            continue;
        };
        if supervisor_id != id {
            continue;
        }
        if total >= offset && page.peers.len() < REMOTE_CONTROL_INTERFACE_PEER_CAP {
            match page.push(remote_control_peer(&member.snapshot)) {
                Ok(()) => {}
                Err(RemoteControlInterfaceInventoryError::Full) => {}
            }
        }
        total = total.saturating_add(1);
    }
    page.total = total;
    RemoteControlInterfacePeersOutcome::Page(page)
}

fn remote_control_peer(snapshot: &InterfaceSnapshot) -> RemoteControlInterfacePeer {
    RemoteControlInterfacePeer {
        id: snapshot.id,
        connection: snapshot.connection,
        tx_bytes: snapshot.tx_bytes,
        rx_bytes: snapshot.rx_bytes,
        links: snapshot.links,
        destinations: snapshot.destinations,
        rate_bytes_per_sec: rate_bytes_per_sec(snapshot),
        radio: snapshot.radio,
        details: snapshot.details,
    }
}

fn rate_bytes_per_sec(snapshot: &InterfaceSnapshot) -> u32 {
    let Some(rates) = snapshot.transfer_rates else {
        return 0;
    };
    rates
        .rx_bps
        .saturating_add(rates.tx_bps)
        .checked_div(8)
        .unwrap_or(0)
}

impl RemoteControlHostControls for DaemonRequestState {
    fn build_version(&self) -> RemoteControlBuildVersion {
        RemoteControlBuildVersion::from_text(env!("CARGO_PKG_VERSION"))
            .unwrap_or_else(RemoteControlBuildVersion::empty)
    }

    fn inventory_interfaces(&self) -> RemoteControlInterfaceInventory {
        let inventory = inventory_from_host(self.handle.interface_inventory());
        tracing::debug!(
            event = "remote_control_inventory_built",
            entries = inventory.entries().len(),
            encoded_body_len = inventory.encoded_body_len(),
        );
        inventory
    }

    fn inventory_interface_peers(
        &self,
        id: InterfaceId,
        offset: u8,
    ) -> RemoteControlInterfacePeersOutcome {
        interface_peers_from_host(&self.handle.interface_inventory(), id, offset)
    }

    fn inventory_interface_config(&self, id: InterfaceId) -> RemoteControlInterfaceConfigOutcome {
        interface_config_from_host(&self.handle.interface_inventory(), id)
    }

    fn set_interface_power(
        &self,
        id: InterfaceId,
        power: RemoteControlInterfacePower,
    ) -> RemoteControlPowerOutcome {
        self.soft_power.set_power(id, power)
    }

    fn set_interface_mode(&self, id: InterfaceId, mode: InterfaceMode) -> RemoteControlModeOutcome {
        self.handle.set_interface_mode(id, mode)
    }

    fn set_interface_group(
        &self,
        id: InterfaceId,
        group: RemoteControlInterfaceGroup,
    ) -> RemoteControlGroupOutcome {
        self.handle.set_interface_group(id, group)
    }

    fn sleep_radios(&self) -> RemoteControlSleepOutcome {
        self.soft_power.sleep_all()
    }

    fn wake_radios(&self) -> RemoteControlSleepOutcome {
        self.soft_power.wake_all()
    }
}

#[cfg(test)]
mod tests {
    use personal_rns::interfaces::{
        ConnectionState, InterfaceGravity, InterfaceId, InterfaceKind, InterfaceMode,
        InterfaceSnapshot, Membership,
    };

    use super::{
        interface_peers_from_host, inventory_from_operator_cards, inventory_from_snapshots,
        operator_interface, OperatorCard,
    };
    use personal_rns::interfaces::InterfaceOriginKind;
    use personal_rns::node_introspection::{FrameAccountingCoverage, InterfaceInventoryEntry};
    use personal_rns::remote_control::RemoteControlInterfacePeersOutcome;

    fn snapshot(kind: InterfaceKind, tag: &[u8], membership: Membership) -> InterfaceSnapshot {
        InterfaceSnapshot {
            id: InterfaceId::from_channel_tag(kind, tag),
            mode: InterfaceMode::Full,
            gravity: InterfaceGravity::ZERO,
            connection: ConnectionState::Connected,
            failure_reason: None,
            rx_bytes: 0,
            tx_bytes: 0,
            transfer_rates: None,
            destinations: 0,
            links: 0,
            transported_links: 0,
            membership,
            radio: personal_rns::interfaces::RadioIndication::for_kind(Some(kind)),
            details: personal_rns::interfaces::PeerDetails::NotApplicable,
        }
    }

    #[test]
    fn inventory_keeps_configured_interfaces_and_drops_fleet_peers() {
        let bluetooth = snapshot(
            InterfaceKind::BluetoothAuto,
            b"ble",
            Membership::Independent,
        );
        let peer = snapshot(
            InterfaceKind::BluetoothPeer,
            b"peer",
            Membership::FleetMember {
                supervisor_id: bluetooth.id,
            },
        );
        let wifi = snapshot(InterfaceKind::AutoWifi, b"wifi", Membership::Independent);
        let inventory = inventory_from_snapshots([peer, bluetooth, wifi]);
        let kinds = inventory
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>();
        assert_eq!(
            kinds,
            [InterfaceKind::BluetoothAuto, InterfaceKind::AutoWifi]
        );
    }

    #[test]
    fn peer_kinds_are_not_operator_interfaces_even_without_membership() {
        let peer = snapshot(
            InterfaceKind::BluetoothPeer,
            b"orphan",
            Membership::Independent,
        );
        assert!(!operator_interface(&peer));
        let client = snapshot(InterfaceKind::TcpClient, b"hub", Membership::Independent);
        assert!(operator_interface(&client));
    }

    #[test]
    fn inventory_keeps_name_group_and_mode_without_embedding_peers() {
        let bluetooth = snapshot(
            InterfaceKind::BluetoothAuto,
            b"ble",
            Membership::Independent,
        );
        let inventory = inventory_from_operator_cards([OperatorCard {
            snapshot: bluetooth,
            name: Some("BLE".to_string()),
            group: Some("home".to_string()),
            config: Some("BLE supervisor".to_string()),
            failure: None,
            peers: Vec::new(),
        }]);
        assert_eq!(inventory.entries().len(), 1);
        assert_eq!(inventory.entries()[0].kind, InterfaceKind::BluetoothAuto);
        assert_eq!(inventory.entries()[0].mode, InterfaceMode::Full);
        let card = &inventory.cards()[0];
        assert_eq!(card.name.as_str(), "BLE");
        assert_eq!(card.group.as_str(), "home");
        assert_eq!(card.config.as_str(), "BLE supervisor");
        assert!(card.peers.is_empty());
    }

    fn inventory_entry(snapshot: InterfaceSnapshot) -> InterfaceInventoryEntry {
        InterfaceInventoryEntry {
            name: None,
            origin: InterfaceOriginKind::Configured,
            attachment_epoch: 0,
            frame_accounting: FrameAccountingCoverage::Unavailable,
            snapshot,
            ifac: None,
            group: None,
            rssi: None,
            group_id: None,
            members: std::vec::Vec::new(),
        }
    }

    #[test]
    fn peer_pages_come_from_a_follow_up_request() {
        let bluetooth = snapshot(
            InterfaceKind::BluetoothAuto,
            b"ble",
            Membership::Independent,
        );
        let mut peer = snapshot(
            InterfaceKind::BluetoothPeer,
            b"peer",
            Membership::FleetMember {
                supervisor_id: bluetooth.id,
            },
        );
        peer.tx_bytes = 11;
        peer.rx_bytes = 22;
        peer.links = 3;
        peer.destinations = 5;
        let raw = [inventory_entry(bluetooth), inventory_entry(peer)];
        let RemoteControlInterfacePeersOutcome::Page(page) =
            interface_peers_from_host(&raw, bluetooth.id, 0)
        else {
            panic!("BLE supervisor should page its members");
        };
        assert_eq!(page.total, 1);
        assert_eq!(page.peers.len(), 1);
        assert_eq!(page.peers[0].id, peer.id);
        assert_eq!(page.peers[0].tx_bytes, 11);
        assert_eq!(page.peers[0].rx_bytes, 22);
        assert_eq!(
            interface_peers_from_host(&raw, peer.id, 0),
            RemoteControlInterfacePeersOutcome::UnknownInterface
        );
    }
}
