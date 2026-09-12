use core::cell::Cell;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::channel::Channel;
use personal_hopspot_core as hopspot;
use personal_rns::bluetooth_auto::BluetoothAutoStatus;
use personal_rns::identity::IDENTITY_PUBLIC_KEY_LEN;
use personal_rns::interfaces::bluetooth_auto::BleIdentity;
use personal_rns::interfaces::lora::RadioProfile;
use personal_rns::interfaces::{
    InterfaceGravity, InterfaceId, InterfaceSnapshot, InterfaceStatus, Membership,
};
use personal_rns::remote_control::{
    parse_controller_public_keys, RemoteControlBuildVersion, RemoteControlControllerGrant,
    RemoteControlControllerGrants, RemoteControlGroupOutcome, RemoteControlInitialControllerGrants,
    RemoteControlInterfaceConfigOutcome, RemoteControlInterfaceGroup,
    RemoteControlInterfaceInventory, RemoteControlInterfacePeersOutcome,
    RemoteControlInterfacePower, RemoteControlLoRaOutcome, RemoteControlLoRaProfile,
    RemoteControlPowerOutcome, RemoteControlRequestSet, RemoteControlSleepOutcome,
    RemoteControlWifiStation, RemoteControlWifiStationOutcome,
};
use personal_rns::runtime::{Message, PrnsEvent, RemoteControlHostControls};

use static_cell::StaticCell;

use super::bluetooth::{BLE_SHARED, MEMBERS};
use super::{INTERFACE_STORE, LORA_CONTROL};

const SEEDED_CONTROLLER_KEY: Option<&str> = option_env!("HOPSPOT_RC_CONTROLLER_KEY");

pub(super) fn initial_controller_grants() -> RemoteControlInitialControllerGrants<'static> {
    let Some(key) = SEEDED_CONTROLLER_KEY else {
        return RemoteControlInitialControllerGrants::Nobody;
    };
    let grant = seeded_controller_grant(key);
    static SEEDED: StaticCell<[RemoteControlControllerGrant; 1]> = StaticCell::new();
    let grants = SEEDED.init([grant]);
    RemoteControlInitialControllerGrants::Grants(
        RemoteControlControllerGrants::try_from(grants.as_slice())
            .expect("the flashed controller grant is a single distinct entry"),
    )
}

fn seeded_controller_grant(key: &str) -> RemoteControlControllerGrant {
    let bytes = decode_allow_list_key(key).expect("build.rs accepted this allow-list key");
    let identity =
        parse_controller_public_keys(&bytes).expect("build.rs accepted this allow-list key");
    RemoteControlControllerGrant::new(identity, RemoteControlRequestSet::all())
        .expect("the complete request set is not empty")
}

fn decode_allow_list_key(key: &str) -> Option<[u8; IDENTITY_PUBLIC_KEY_LEN]> {
    if key.len() != IDENTITY_PUBLIC_KEY_LEN * 2 {
        return None;
    }
    let mut bytes = [0u8; IDENTITY_PUBLIC_KEY_LEN];
    for (index, pair) in key.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = decode_hex_byte(pair[0], pair[1])?;
    }
    Some(bytes)
}

fn decode_hex_byte(high: u8, low: u8) -> Option<u8> {
    Some(hex_nibble(high)? << 4 | hex_nibble(low)?)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub(super) struct HopspotRemoteControlState {
    pub(super) lora: &'static personal_rns::manifold::embassy::EmbassyInterfaceStatus,
    pub(super) usb: &'static personal_rns::manifold::embassy::EmbassyInterfaceStatus,
    pub(super) modes: hopspot::InterfaceModeTable,
    pub(super) ble_identity: Option<BleIdentity>,
}

static PENDING_REMOTE_LORA_PROFILE: BlockingMutex<
    CriticalSectionRawMutex,
    Cell<Option<RadioProfile>>,
> = BlockingMutex::new(Cell::new(None));

pub(super) static REMOTE_PAIRING_EVENTS: Channel<
    CriticalSectionRawMutex,
    personal_rns::runtime::RemoteControlTargetPairingConfirmation,
    1,
> = Channel::new();

impl HopspotRemoteControlState {
    fn set_radios_enabled(&self, enabled: bool) {
        if enabled {
            self.lora.enable();
            self.usb.enable();
        } else {
            self.lora.disable();
            self.usb.disable();
        }
        let ble = BluetoothAutoStatus::new(&BLE_SHARED);
        if enabled {
            ble.enable();
        } else {
            ble.disable();
        }
    }
}

impl RemoteControlHostControls for HopspotRemoteControlState {
    fn inventory_interfaces(&self) -> RemoteControlInterfaceInventory {
        hopspot::remote_control_inventory_from_snapshots(&build_snapshots(
            self.lora,
            self.usb,
            self.modes,
        ))
    }

    fn build_version(&self) -> RemoteControlBuildVersion {
        hopspot::hopspot_remote_control_build_version()
    }

    fn inventory_interface_config(&self, id: InterfaceId) -> RemoteControlInterfaceConfigOutcome {
        let ble = BluetoothAutoStatus::new(&BLE_SHARED);
        let mut group = [0u8; 32];
        let ble_group = ble.copy_group(&mut group);
        hopspot::remote_control_interface_config_from_snapshots(
            &build_snapshots(self.lora, self.usb, self.modes),
            id,
            |snapshot, card| {
                hopspot::decorate_hopspot_remote_control_card(
                    snapshot,
                    card,
                    ble_group,
                    LORA_CONTROL.current(),
                    None,
                    self.ble_identity,
                );
            },
        )
    }

    fn inventory_interface_peers(
        &self,
        id: InterfaceId,
        offset: u8,
    ) -> RemoteControlInterfacePeersOutcome {
        hopspot::remote_control_interface_peers_from_snapshots(
            &build_snapshots(self.lora, self.usb, self.modes),
            id,
            offset,
        )
    }

    fn set_interface_power(
        &self,
        id: InterfaceId,
        power: RemoteControlInterfacePower,
    ) -> RemoteControlPowerOutcome {
        let enabled = power.enabled();
        if id == self.lora.id() {
            if enabled {
                self.lora.enable();
            } else {
                self.lora.disable();
            }
            return RemoteControlPowerOutcome::Applied;
        }
        if id == self.usb.id() {
            if enabled {
                self.usb.enable();
            } else {
                self.usb.disable();
            }
            return RemoteControlPowerOutcome::Applied;
        }
        let ble = BluetoothAutoStatus::new(&BLE_SHARED);
        if id == ble.id() {
            if enabled {
                ble.enable();
            } else {
                ble.disable();
            }
            return RemoteControlPowerOutcome::Applied;
        }
        RemoteControlPowerOutcome::UnknownInterface
    }

    fn set_interface_group(
        &self,
        id: InterfaceId,
        group: RemoteControlInterfaceGroup,
    ) -> RemoteControlGroupOutcome {
        let ble = BluetoothAutoStatus::new(&BLE_SHARED);
        if id == ble.id() {
            return if ble.set_group_id(group.as_bytes()) {
                RemoteControlGroupOutcome::Applied
            } else {
                RemoteControlGroupOutcome::Failed
            };
        }
        RemoteControlGroupOutcome::UnknownInterface
    }

    fn set_interface_lora_profile(
        &self,
        id: InterfaceId,
        profile: RemoteControlLoRaProfile,
    ) -> RemoteControlLoRaOutcome {
        if id != self.lora.id() {
            return RemoteControlLoRaOutcome::UnknownInterface;
        }
        let Some(profile) = profile.profile() else {
            return RemoteControlLoRaOutcome::Failed;
        };
        if profile.validate().is_err() {
            return RemoteControlLoRaOutcome::Failed;
        }
        PENDING_REMOTE_LORA_PROFILE.lock(|cell| cell.set(Some(profile)));
        RemoteControlLoRaOutcome::Applied
    }

    fn set_interface_wifi_station(
        &self,
        _id: InterfaceId,
        _station: RemoteControlWifiStation,
    ) -> RemoteControlWifiStationOutcome {
        RemoteControlWifiStationOutcome::UnknownInterface
    }

    fn sleep_radios(&self) -> RemoteControlSleepOutcome {
        self.set_radios_enabled(false);
        RemoteControlSleepOutcome::Applied
    }

    fn wake_radios(&self) -> RemoteControlSleepOutcome {
        self.set_radios_enabled(true);
        RemoteControlSleepOutcome::Applied
    }
}

pub(super) fn on_event(event: PrnsEvent<'_>, _state: &HopspotRemoteControlState) {
    if let PrnsEvent::Message(Message::RemoteControlTargetPairingConfirmationRequired(
        confirmation,
    )) = event
    {
        let _ = REMOTE_PAIRING_EVENTS.try_send(confirmation);
    }
}

pub(super) fn take_pending_lora_profile() -> Option<RadioProfile> {
    PENDING_REMOTE_LORA_PROFILE.lock(|cell| cell.replace(None))
}

fn build_snapshots(
    lora: &dyn InterfaceStatus,
    usb: &dyn InterfaceStatus,
    modes: hopspot::InterfaceModeTable,
) -> heapless::Vec<InterfaceSnapshot, { MEMBERS + 4 }> {
    let ble = BluetoothAutoStatus::new(&BLE_SHARED);
    let mut entries: heapless::Vec<(&dyn InterfaceStatus, Membership), { MEMBERS + 4 }> =
        heapless::Vec::new();
    let _ = entries.push((lora, Membership::Independent));
    let _ = entries.push((usb, Membership::Independent));
    let supervisor_id = ble.id();
    let _ = entries.push((&ble, Membership::Independent));
    for member in ble.members() {
        let _ = entries.push((member, Membership::FleetMember { supervisor_id }));
    }
    let mut snapshots: heapless::Vec<InterfaceSnapshot, { MEMBERS + 4 }> = heapless::Vec::new();
    for (status, membership) in &entries {
        let counts = INTERFACE_STORE.counts(status.id());
        let _ = snapshots.push(InterfaceSnapshot {
            id: status.id(),
            mode: hopspot::mode_from_table(modes, status.id().kind()),
            gravity: InterfaceGravity::ZERO,
            connection: status.connection(),
            failure_reason: status.failure_reason(),
            rx_bytes: status.rx_bytes(),
            tx_bytes: status.tx_bytes(),
            transfer_rates: status.transfer_rates(),
            destinations: counts.destinations,
            links: counts.links,
            transported_links: counts.transported_links,
            membership: *membership,
            radio: status.radio(),
            details: status.details(),
        });
    }
    snapshots
}
