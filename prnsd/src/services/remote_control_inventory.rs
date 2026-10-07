use personal_rns::interfaces::{
    ConnectionState, InterfaceId, InterfaceKind, InterfaceSnapshot, Membership,
};
use personal_rns::node_introspection::InterfaceInventoryEntry;
use personal_rns::remote_control::{
    RemoteControlInterfaceCard, RemoteControlInterfaceCardError,
    RemoteControlInterfaceConfigOutcome, RemoteControlInterfaceContinuation,
    RemoteControlInterfaceCursor, RemoteControlInterfaceEntry, RemoteControlInterfaceInventory,
    RemoteControlInterfaceInventoryError, RemoteControlInterfacePage, RemoteControlInterfacePeer,
    RemoteControlInterfacePeerPage, RemoteControlInterfacePeersOutcome,
    RemoteControlPeerContinuation, RemoteControlPeerCursor, RemoteControlPeerPage,
    REMOTE_CONTROL_INTERFACE_INVENTORY_CAP, REMOTE_CONTROL_INTERFACE_PEER_CAP,
};

pub(super) fn remote_control_inventory_from_snapshots(
    snapshots: &[InterfaceSnapshot],
    page: RemoteControlInterfacePage,
) -> Result<RemoteControlInterfaceInventory, RemoteControlInterfaceInventoryError> {
    reject_duplicate_snapshot_ids(snapshots)?;
    let mut inventory = RemoteControlInterfaceInventory::empty();
    let mut after = match page {
        RemoteControlInterfacePage::First => None,
        RemoteControlInterfacePage::After(cursor) => Some(cursor.id()),
    };
    while inventory.entries().len() < REMOTE_CONTROL_INTERFACE_INVENTORY_CAP {
        let Some(snapshot) = snapshots
            .iter()
            .filter(|snapshot| operator_interface(snapshot))
            .filter(|snapshot| after.is_none_or(|after| snapshot.id.as_bytes() > after.as_bytes()))
            .min_by_key(|snapshot| *snapshot.id.as_bytes())
        else {
            break;
        };
        let Some(kind) = operator_kind(snapshot) else {
            break;
        };
        inventory.push(RemoteControlInterfaceEntry {
            id: snapshot.id,
            kind,
            mode: snapshot.mode,
            connection: snapshot.connection,
            enabled: snapshot.connection != ConnectionState::Disabled,
            tx_bytes: snapshot.tx_bytes,
            rx_bytes: snapshot.rx_bytes,
            links: snapshot.links.saturating_add(snapshot.transported_links),
            rate_bytes_per_sec: rate_bytes_per_sec(snapshot),
        })?;
        after = Some(snapshot.id);
    }
    if let Some(last) = inventory.entries().last() {
        let has_more = snapshots.iter().any(|snapshot| {
            operator_interface(snapshot) && snapshot.id.as_bytes() > last.id.as_bytes()
        });
        if has_more {
            inventory.set_continuation(RemoteControlInterfaceContinuation::More(
                RemoteControlInterfaceCursor::after(last.id),
            ))?;
        }
    }
    Ok(inventory)
}

pub(super) fn remote_control_interface_config(
    entries: &[InterfaceInventoryEntry],
    id: InterfaceId,
    group: Option<&str>,
) -> Result<RemoteControlInterfaceConfigOutcome, RemoteControlInterfaceCardError> {
    let Some(entry) = entries
        .iter()
        .find(|entry| entry.snapshot.id == id && operator_interface(&entry.snapshot))
    else {
        return Ok(RemoteControlInterfaceConfigOutcome::UnknownInterface);
    };
    let Some(kind) = operator_kind(&entry.snapshot) else {
        return Ok(RemoteControlInterfaceConfigOutcome::UnknownInterface);
    };
    let mut card = RemoteControlInterfaceCard::empty();
    let configured_name = entry
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| kind.name());
    card.set_name(configured_name)?;
    if let Some(group) = group.filter(|group| !group.is_empty()) {
        card.set_group(group)?;
    }
    if let Some(failure) = entry.snapshot.failure_reason {
        card.set_failure(failure)?;
    }
    card.destinations = entry.snapshot.destinations;
    card.transported_links = entry.snapshot.transported_links;
    Ok(RemoteControlInterfaceConfigOutcome::Card(card))
}

pub(super) fn remote_control_interface_peers_from_snapshots(
    snapshots: &[InterfaceSnapshot],
    id: InterfaceId,
    requested_page: RemoteControlPeerPage,
) -> Result<RemoteControlInterfacePeersOutcome, RemoteControlInterfaceInventoryError> {
    reject_duplicate_snapshot_ids(snapshots)?;
    let Some(supervisor) = snapshots
        .iter()
        .find(|snapshot| snapshot.id == id && operator_interface(snapshot))
    else {
        return Ok(RemoteControlInterfacePeersOutcome::UnknownInterface);
    };
    let mut after = match requested_page {
        RemoteControlPeerPage::First => None,
        RemoteControlPeerPage::After(cursor) => Some(cursor.id()),
    };
    let mut page = RemoteControlInterfacePeerPage::empty(supervisor.id);
    while page.peers.len() < REMOTE_CONTROL_INTERFACE_PEER_CAP {
        let Some(peer) = snapshots
            .iter()
            .filter_map(|snapshot| remote_control_peer_for_supervisor(id, snapshot))
            .filter(|peer| after.is_none_or(|after| peer.id.as_bytes() > after.as_bytes()))
            .min_by_key(|peer| *peer.id.as_bytes())
        else {
            break;
        };
        after = Some(peer.id);
        page.push(peer)?;
    }
    if let Some(last) = page.peers.last() {
        let has_more = snapshots
            .iter()
            .filter_map(|snapshot| remote_control_peer_for_supervisor(id, snapshot))
            .any(|peer| peer.id.as_bytes() > last.id.as_bytes());
        if has_more {
            page.set_continuation(RemoteControlPeerContinuation::More(
                RemoteControlPeerCursor::after(last.id),
            ))?;
        }
    }
    Ok(RemoteControlInterfacePeersOutcome::Page(page))
}

fn reject_duplicate_snapshot_ids(
    snapshots: &[InterfaceSnapshot],
) -> Result<(), RemoteControlInterfaceInventoryError> {
    for (index, snapshot) in snapshots.iter().enumerate() {
        if snapshots
            .iter()
            .skip(index.saturating_add(1))
            .any(|other| other.id == snapshot.id)
        {
            return Err(RemoteControlInterfaceInventoryError::NonAscending);
        }
    }
    Ok(())
}

fn operator_interface(snapshot: &InterfaceSnapshot) -> bool {
    operator_kind(snapshot).is_some()
}

fn operator_kind(snapshot: &InterfaceSnapshot) -> Option<InterfaceKind> {
    if matches!(snapshot.membership, Membership::FleetMember { .. }) {
        return None;
    }
    match snapshot.id.kind() {
        Some(kind) if kind.supervisor_kind().is_none() => Some(kind),
        None => Some(InterfaceKind::UsbAutoDevice),
        Some(_) => None,
    }
}

fn remote_control_peer_for_supervisor(
    supervisor_id: InterfaceId,
    snapshot: &InterfaceSnapshot,
) -> Option<RemoteControlInterfacePeer> {
    match snapshot.membership {
        Membership::FleetMember {
            supervisor_id: owner,
        } if owner == supervisor_id => Some(RemoteControlInterfacePeer {
            id: snapshot.id,
            connection: snapshot.connection,
            tx_bytes: snapshot.tx_bytes,
            rx_bytes: snapshot.rx_bytes,
            links: snapshot.links,
            destinations: snapshot.destinations,
            rate_bytes_per_sec: rate_bytes_per_sec(snapshot),
            radio: snapshot.radio,
            details: snapshot.details,
        }),
        Membership::Independent | Membership::FleetMember { .. } => None,
    }
}

fn rate_bytes_per_sec(snapshot: &InterfaceSnapshot) -> u32 {
    snapshot
        .transfer_rates
        .map(|rates| rates.rx_bps.saturating_add(rates.tx_bps) / 8)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use personal_rns::interfaces::{
        ConnectionState, InterfaceGravity, InterfaceId, InterfaceKind, InterfaceMode,
        InterfaceSnapshot, Membership, PeerDetails, RadioIndication, INTERFACE_ID_LEN,
    };
    use personal_rns::remote_control::{
        RemoteControlInterfaceInventory, RemoteControlInterfacePage,
    };

    use super::remote_control_inventory_from_snapshots;

    fn snapshot(kind: InterfaceKind) -> InterfaceSnapshot {
        let mut bytes = [0u8; INTERFACE_ID_LEN];
        bytes[0] = kind as u8;
        InterfaceSnapshot {
            id: InterfaceId::new(bytes),
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
            membership: Membership::Independent,
            radio: RadioIndication::for_kind(Some(kind)),
            details: PeerDetails::NotApplicable,
        }
    }

    #[test]
    fn inventory_lists_supervisors_and_omits_fleet_members() {
        let supervisor = snapshot(InterfaceKind::BluetoothAuto);
        let supervisor_id = supervisor.id;
        let mut member = snapshot(InterfaceKind::BluetoothPeer);
        member.id = InterfaceId::new({
            let mut bytes = [0u8; INTERFACE_ID_LEN];
            bytes[0] = InterfaceKind::BluetoothPeer as u8;
            bytes[1] = 0xab;
            bytes
        });
        member.membership = Membership::FleetMember { supervisor_id };
        let inventory = remote_control_inventory_from_snapshots(
            &[member, supervisor],
            RemoteControlInterfacePage::First,
        )
        .unwrap_or_else(|_| RemoteControlInterfaceInventory::empty());
        let entry = inventory.entries().first();
        assert!(entry.is_some_and(|entry| {
            entry.id == supervisor_id && entry.kind == InterfaceKind::BluetoothAuto
        }));
    }
}
