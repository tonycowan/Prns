use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dioxus::prelude::spawn;
#[cfg(target_os = "android")]
use personal_rns::bluetooth_auto::BluetoothAuto;
use personal_rns::bluetooth_auto::BluetoothAutoStatus;
use personal_rns::engine::{
    AdmitRemoteControlControllerPairingResponseOutcome, ApproveRemoteControlControllerPairing,
    RejectRemoteControlControllerPairing, RouteSnapshot,
};
use personal_rns::identity::{IdentityHash, IDENTITY_PUBLIC_KEY_LEN};
use personal_rns::interfaces::bluetooth_auto::BleIdentity;
#[cfg(target_os = "android")]
use personal_rns::interfaces::bluetooth_auto::{
    AndroidHost, Endpoint, LinkCapabilities, BLE_HW_MTU,
};
use personal_rns::interfaces::lora::{ModemPreset, Modulation, RadioProfile};
use personal_rns::interfaces::{
    BluetoothIndication, ConnectionState, InterfaceId, InterfaceKind, InterfaceMode,
    InterfaceSnapshot, InterfaceStatus, LoRaIndication, Membership, RadioIndication, RssiDbm,
    SignalQualityTenthsPercent, SnrQuarterDb, WifiIndication,
};
use personal_rns::load_or_create_ble_identity;
use personal_rns::manifold::tokio::TokioInterfaceStatus;
use personal_rns::node_introspection::{logical_interface_inventory, FrameAccountingCoverage};
use personal_rns::prelude::*;
use personal_rns::remote_control::{
    parse_controller_public_keys, ReceiveRemoteControlControllerPairingOfferOutcome,
    RemoteControlAuthorizeControllerOutcome, RemoteControlGroupOutcome, RemoteControlInterfaceCard,
    RemoteControlInterfaceConfigOutcome, RemoteControlInterfaceEntry, RemoteControlInterfaceGroup,
    RemoteControlInterfacePeer, RemoteControlInterfacePeersOutcome, RemoteControlInterfacePower,
    RemoteControlLoRaOutcome, RemoteControlLoRaProfile, RemoteControlModeOutcome,
    RemoteControlPairingEndpoint, RemoteControlPairingInvitationCode, RemoteControlPowerOutcome,
    RemoteControlRequestKind, RemoteControlRevokeControllerOutcome, RemoteControlSleepOutcome,
    RemoteControlWifiStation, RemoteControlWifiStationOutcome,
};
use personal_rns::routing::NextHop;
use personal_rns::runtime::RequestPathError;
use personal_rns::units::InstantMillis;
#[cfg(target_os = "android")]
use personal_rns::usb_auto::{UsbAutoCandidate, UsbAutoHost};
#[cfg(target_os = "macos")]
use personal_rns::wifi_auto::apple_service_discovery;
use personal_rns::wifi_auto::AutoWifiStatus;
#[cfg(not(target_os = "android"))]
use personal_rns::AutoBle;
#[cfg(target_os = "android")]
use prns_ffi::bluetooth_auto::android::AndroidBleBackend;
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
use tokio::sync::Notify;

use crate::identity_clone::{
    evaluate_clone_locality, identity_clone_destination_hash, parse_clone_siblings,
    parse_identity_clone_message, usb_supervisor_link_is_up, CloneLocality, ControllerAppState,
    DestCloneSession, IdentityClone, IdentityCloneAccept, IdentityCloneAnnounce,
    IdentityCloneHello, IdentityCloneInbound, IdentityClonePayload, IdentityCloneShared,
    IdentityCloneView, SourceCloneSession, IDENTITY_CLONE_APP_NAME, IDENTITY_CLONE_ASPECTS,
    IDENTITY_CLONE_NONCE_LEN, IDENTITY_CLONE_REQUEST_ENDPOINT_ID,
};
use crate::roster_sync::{
    adopt_sibling, forget_sibling_locally, forget_target_locally, import_seed_labels, load_replica,
    merge_roster, next_sibling_alias, note_local_label, note_local_upsert, parse_replica_reply,
    peer_alias_is_syncable, persist_replica, replica_message, replica_path,
    roster_sync_destination_hash, sibling_alias_is_syncable, strip_local_sibling_alias, write_pull,
    RosterLabel, RosterLabelKind, RosterShared, RosterSync, ROSTER_SYNC_APP_NAME,
    ROSTER_SYNC_ASPECTS, ROSTER_SYNC_REQUEST_ENDPOINT_ID,
};

const IDENTITY_HASH_BYTES: usize = 16;
const INTERFACE_ID_BYTES: usize = 8;
#[cfg(not(target_os = "android"))]
const DEFAULT_DATA_DIRECTORY: &str = ".local/share/hopspot-remote-control";
const TARGET_NAMES_FILE: &str = "target-names";
const TARGET_ALIASES_FILE: &str = "target-aliases";
const PEER_ALIASES_FILE: &str = "peer-aliases";
const MANAGER_ALIASES_FILE: &str = "manager-aliases";
const SIBLING_ALIASES_FILE: &str = "sibling-aliases";
const CONTROLLER_IDENTITY_FILE: &str = "controller";
const INSTANCE_IDENTITY_FILE: &str = "instance";
const CONTROL_ANNOUNCE_POLL: Duration = Duration::from_millis(500);
const CONTROL_ANNOUNCE_WAIT: Duration = Duration::from_secs(15 * 60);
const CONNECT_AFTER_ANNOUNCE_ATTEMPTS: u8 = 2;
const CONNECT_AFTER_ANNOUNCE_GAP: Duration = Duration::from_secs(1);
const INVENTORY_RECOVERY_WAIT: Duration = Duration::from_secs(120);
const INVENTORY_RECOVERY_GAP: Duration = Duration::from_secs(2);
const ACTIVITY_CONTROLLER_SCOPE: &str = "controller";
const DEFAULT_TCP_TARGET: &str = "127.0.0.1:4242";
const ROSTER_ANNOUNCE_GAP: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteControlAnnounceWait {
    UntilHeard,
    UntilRefreshed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControllerIdentity {
    pub operator_hash: String,
    pub allow_list_key: String,
    pub operator_secret_path: String,
    pub instance_hash: String,
    pub instance_secret_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetAccess {
    pub id: String,
    pub name: String,
    pub status: TargetStatus,
    pub path: Option<TargetPath>,
    pub build_version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetPath {
    pub hops: u8,
    pub via: String,
    pub interface: String,
    pub announced_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetStatus {
    Online,
    Sleeping,
    Offline,
    AwaitingPairing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterfaceFact {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterfaceEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub power: InterfacePower,
    pub mode: InterfaceMode,
    pub connection: String,
    pub group: Option<String>,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub tx_bps: Option<u32>,
    pub rx_bps: Option<u32>,
    pub links: u32,
    pub transported_links: u32,
    pub destinations: u32,
    pub rate_bytes_per_sec: u32,
    pub gravity: Option<i64>,
    pub ifac_bytes: Option<usize>,
    pub last_activity_secs: Option<u32>,
    pub detail: Option<String>,
    pub failure: Option<String>,
    pub extras: Vec<InterfaceFact>,
    pub shows_peers: bool,
    pub peers: Vec<InterfacePeer>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterfacePeer {
    pub id: String,
    pub name: String,
    pub alias: Option<String>,
    pub connection: String,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub links: u32,
    pub destinations: u32,
    pub rate_bytes_per_sec: u32,
    pub last_activity_secs: Option<u32>,
    pub radio: RadioIndication,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterfacePower {
    On,
    Off,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairingState {
    Idle,
    /// Invitation accepted; establishing the pairing link / waiting for offer.
    Connecting,
    AwaitingConfirmation {
        digits: String,
    },
    /// Local Approve was pressed; waiting for the target to approve and return completion.
    WaitingForTarget,
    Approved,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BackendError {
    #[error("controller startup failed: {0}")]
    Startup(String),
    #[error(
        "that pairing announcement is missing or expired; open pairing on the target again while this app is running"
    )]
    NoPairingAdvertisement,
    #[error("invalid invitation code; enter the eight hexadecimal digits shown by the target")]
    InvalidInvitationCode,
    #[error(
        "the target never answered Begin. Pairing mints a new destination each time Pair remote opens (and after every flash), and a wrong invitation is dropped silently. Leave this app running, open Pair remote again, use the newest awaiting row (not an older Hopspot line), and type the eight digits on the screen now"
    )]
    PairingOfferTimeout,
    #[error("invalid target identity hash: {0}")]
    InvalidTargetId(String),
    #[error("invalid interface id: {0}")]
    InvalidInterfaceId(String),
    #[error("{operation} failed: {detail}")]
    Operation {
        operation: &'static str,
        detail: String,
    },
    #[error("no controller pairing confirmation is pending")]
    NoPendingPairing,
    #[error("{0}")]
    Clone(String),
}

#[derive(Clone)]
pub struct RemoteControlBackend {
    session: Result<Arc<ControllerSession>, BackendError>,
}

impl RemoteControlBackend {
    pub fn new() -> Self {
        Self {
            session: ControllerSession::start().map(Arc::new),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.session.is_ok()
    }

    pub fn controller_identity(&self) -> Result<ControllerIdentity, BackendError> {
        let session = self.session()?;
        let mut identity = session.controller_identity.clone();
        if let Ok(secret) = load_controller_secret(&session.identities_dir) {
            let material = personal_rns::identity::PrivateIdentityMaterial::from_bytes(*secret);
            identity.operator_hash = encode_hex(material.identity_hash().as_bytes());
            identity.allow_list_key = encode_hex(material.public().as_bytes());
        }
        Ok(identity)
    }

    pub fn identity_clone(&self) -> Result<IdentityCloneView, BackendError> {
        let session = self.session()?;
        let locality = match self.local_interfaces() {
            Ok(interfaces) => evaluate_clone_locality(&interfaces),
            Err(_) => CloneLocality::UsbOff,
        };
        let shared = session
            .clone
            .lock()
            .map_err(|_| BackendError::Clone("clone state lock was poisoned".to_string()))?;
        let self_hash = session.controller_identity.instance_hash.clone();
        let siblings = session
            .roster
            .lock()
            .ok()
            .map(|roster| {
                let hashes = roster
                    .replica
                    .siblings
                    .iter()
                    .map(|keys| {
                        (
                            encode_hex(keys.identity_hash().as_bytes()),
                            roster.heard_sibling(keys.identity_hash()),
                        )
                    })
                    .collect::<Vec<_>>();
                drop(roster);
                ensure_missing_sibling_aliases(
                    session,
                    hashes
                        .iter()
                        .map(|(hash, _)| hash.as_str())
                        .filter(|hash| sibling_alias_is_syncable(hash, &self_hash)),
                );
                let aliases = session
                    .sibling_aliases
                    .lock()
                    .expect("sibling aliases mutex poisoned");
                let mut siblings = hashes
                    .into_iter()
                    .map(|(instance_hash, heard)| {
                        let is_self = instance_hash == self_hash;
                        crate::identity_clone::SiblingControllerView {
                            alias: aliases
                                .get(&instance_hash)
                                .cloned()
                                .unwrap_or_else(|| next_sibling_alias(&aliases)),
                            instance_hash,
                            heard,
                            is_self,
                        }
                    })
                    .collect::<Vec<_>>();
                siblings.sort_by(|left, right| left.instance_hash.cmp(&right.instance_hash));
                siblings
            })
            .unwrap_or_default();
        Ok(IdentityCloneView {
            locality,
            dest_waiting: shared.dest_waiting,
            dest_accepted: shared.dest.as_ref().is_some_and(|dest| dest.accepted),
            incoming_source: shared
                .dest
                .as_ref()
                .map(|dest| encode_hex(dest.announce.source().as_bytes())),
            incoming_digits: shared
                .dest
                .as_ref()
                .and_then(|dest| dest.transcript)
                .map(|transcript| transcript.confirmation_code().to_string()),
            outgoing_dest: shared.source.as_ref().and_then(|source| {
                source
                    .peer_keys
                    .map(|keys| encode_hex(keys.identity_hash().as_bytes()))
            }),
            outgoing_digits: shared
                .source
                .as_ref()
                .and_then(|source| source.transcript)
                .map(|transcript| transcript.confirmation_code().to_string()),
            source_accepted: shared.source.as_ref().is_some_and(|source| source.accepted),
            in_progress: shared.in_progress(),
            siblings,
            notice: shared.notice.clone(),
            error: shared.error.clone(),
        })
    }

    pub fn arm_clone_dest(&self) -> Result<(), BackendError> {
        let locality = evaluate_clone_locality(&self.local_interfaces()?);
        if !locality.allows_clone() {
            return Err(BackendError::Clone(locality.operator_message()));
        }
        let session = self.session()?;
        let mut shared = session
            .clone
            .lock()
            .map_err(|_| BackendError::Clone("clone state lock was poisoned".to_string()))?;
        shared.dest_waiting = true;
        shared.dest = None;
        shared.error = None;
        shared.notice = Some(
            "Waiting to be adopted over USB. Approving replaces this app's Operator identity. The Instance id stays."
                .to_string(),
        );
        Ok(())
    }

    pub fn cancel_clone(&self) -> Result<(), BackendError> {
        let session = self.session()?;
        let mut shared = session
            .clone
            .lock()
            .map_err(|_| BackendError::Clone("clone state lock was poisoned".to_string()))?;
        *shared = IdentityCloneShared::default();
        Ok(())
    }

    pub async fn start_clone_source(&self) -> Result<(), BackendError> {
        let locality = evaluate_clone_locality(&self.local_interfaces()?);
        if !locality.allows_clone() {
            return Err(BackendError::Clone(locality.operator_message()));
        }
        let session = self.session()?;
        let instance = load_instance_secret(&session.identities_dir)?;
        let instance_material =
            personal_rns::identity::PrivateIdentityMaterial::from_bytes(*instance);
        let source_hash = instance_material.identity_hash();
        let destination = identity_clone_destination_hash(source_hash)
            .ok_or_else(|| BackendError::Clone("clone destination name is invalid".to_string()))?;
        let mut source_nonce = [0u8; IDENTITY_CLONE_NONCE_LEN];
        let mut entropy = OsRuntimeEntropy::try_new()
            .map_err(|error| BackendError::Clone(format!("{error:?}")))?;
        entropy.fill_random(&mut source_nonce);
        let announce = IdentityCloneAnnounce::new(instance_material.public(), source_nonce);
        let mut app_data = [0u8; 128];
        let app_data_len = announce
            .write(&mut app_data)
            .ok_or_else(|| BackendError::Clone("clone announce is too large".to_string()))?;
        let bytes = personal_rns::routing::announce::emit::AnnounceAppDataBytes::from_slice(
            &app_data[..app_data_len],
        )
        .map_err(|_| BackendError::Clone("clone announce app data is too large".to_string()))?;
        session
            .handle
            .announce_now(AnnounceNow {
                destination,
                target: AnnounceTarget::AllInterfaces,
                app_data: AnnounceAppData::Data(bytes),
            })
            .await
            .map_err(|error| BackendError::Clone(format!("{error:?}")))?;
        let mut shared = session
            .clone
            .lock()
            .map_err(|_| BackendError::Clone("clone state lock was poisoned".to_string()))?;
        shared.source = Some(SourceCloneSession {
            source_hash,
            source_nonce,
            peer_keys: None,
            transcript: None,
            accepted: false,
            payload: None,
            last_announce: Instant::now(),
        });
        shared.notice = Some(
            "Adoption announce sent on USB. The other app must already have pressed I am up for adoption."
                .to_string(),
        );
        shared.error = None;
        Ok(())
    }

    pub async fn advance_clone(&self) -> Result<(), BackendError> {
        if let Err(error) = self.refresh_clone_source_announce().await {
            let Ok(session) = self.session() else {
                return Err(error);
            };
            if let Ok(mut shared) = session.clone.lock() {
                shared.error = Some(error.to_string());
            }
            return Err(error);
        }
        if let Err(error) = self.send_pending_clone_hello().await {
            let Ok(session) = self.session() else {
                return Err(error);
            };
            if let Ok(mut shared) = session.clone.lock() {
                shared.error = Some(error.to_string());
            }
            return Err(error);
        }
        Ok(())
    }

    async fn refresh_clone_source_announce(&self) -> Result<(), BackendError> {
        let session = self.session()?;
        let (destination, source_nonce) = {
            let mut shared = session
                .clone
                .lock()
                .map_err(|_| BackendError::Clone("clone state lock was poisoned".to_string()))?;
            let Some(source) = shared.source.as_mut() else {
                return Ok(());
            };
            if source.peer_keys.is_some() || source.last_announce.elapsed() < Duration::from_secs(3)
            {
                return Ok(());
            }
            source.last_announce = Instant::now();
            (
                identity_clone_destination_hash(source.source_hash).ok_or_else(|| {
                    BackendError::Clone("clone destination name is invalid".to_string())
                })?,
                source.source_nonce,
            )
        };
        let instance = load_instance_secret(&session.identities_dir)?;
        let instance_material =
            personal_rns::identity::PrivateIdentityMaterial::from_bytes(*instance);
        let announce = IdentityCloneAnnounce::new(instance_material.public(), source_nonce);
        let mut app_data = [0u8; 128];
        let app_data_len = announce
            .write(&mut app_data)
            .ok_or_else(|| BackendError::Clone("clone announce is too large".to_string()))?;
        let bytes = personal_rns::routing::announce::emit::AnnounceAppDataBytes::from_slice(
            &app_data[..app_data_len],
        )
        .map_err(|_| BackendError::Clone("clone announce app data is too large".to_string()))?;
        session
            .handle
            .announce_now(AnnounceNow {
                destination,
                target: AnnounceTarget::AllInterfaces,
                app_data: AnnounceAppData::Data(bytes),
            })
            .await
            .map_err(|error| BackendError::Clone(format!("{error:?}")))?;
        Ok(())
    }

    async fn send_pending_clone_hello(&self) -> Result<(), BackendError> {
        let session = self.session()?;
        let should_hello = {
            let mut shared = session
                .clone
                .lock()
                .map_err(|_| BackendError::Clone("clone state lock was poisoned".to_string()))?;
            if !shared.dest_waiting {
                return Ok(());
            }
            let Some(dest) = shared.dest.as_mut() else {
                return Ok(());
            };
            if dest.transcript.is_some() || dest.hello_in_flight {
                return Ok(());
            }
            dest.hello_in_flight = true;
            true
        };
        if !should_hello {
            return Ok(());
        }
        match self.send_clone_hello().await {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Ok(mut shared) = session.clone.lock() {
                    if let Some(dest) = shared.dest.as_mut() {
                        dest.hello_in_flight = false;
                    }
                }
                Err(error)
            }
        }
    }

    async fn send_clone_hello(&self) -> Result<(), BackendError> {
        let session = self.session()?;
        let dest_secret = load_instance_secret(&session.identities_dir)?;
        let dest_material =
            personal_rns::identity::PrivateIdentityMaterial::from_bytes(*dest_secret);
        let (destination, announce) = {
            let shared = session
                .clone
                .lock()
                .map_err(|_| BackendError::Clone("clone state lock was poisoned".to_string()))?;
            let dest = shared.dest.as_ref().ok_or_else(|| {
                BackendError::Clone("no sibling push has been heard on USB".to_string())
            })?;
            (dest.destination, dest.announce)
        };
        let mut dest_nonce = [0u8; IDENTITY_CLONE_NONCE_LEN];
        let mut entropy = OsRuntimeEntropy::try_new()
            .map_err(|error| BackendError::Clone(format!("{error:?}")))?;
        entropy.fill_random(&mut dest_nonce);
        let hello = IdentityCloneHello::sign(
            &dest_material,
            announce.source(),
            announce.source_nonce(),
            dest_nonce,
        );
        let mut hello_bytes = [0u8; 256];
        let hello_len = hello
            .write(&mut hello_bytes)
            .ok_or_else(|| BackendError::Clone("clone hello is too large".to_string()))?;
        let link_id = session
            .handle
            .establish_link(destination)
            .await
            .map_err(|error| BackendError::Clone(format!("{error:?}")))?;
        let (hello_reply, _) = session
            .handle
            .request(
                link_id,
                RequestEndpointId::of(IDENTITY_CLONE_REQUEST_ENDPOINT_ID),
                &hello_bytes[..hello_len],
            )
            .await
            .map_err(|error| BackendError::Clone(format!("{error:?}")))?;
        match parse_identity_clone_message(&hello_reply) {
            Some(IdentityCloneInbound::HelloAck) => {}
            other => {
                return Err(BackendError::Clone(format!(
                    "the other app rejected hello: {other:?}"
                )))
            }
        }
        let transcript = hello
            .verify(announce.source(), announce.source_nonce())
            .ok_or_else(|| BackendError::Clone("clone hello did not verify locally".to_string()))?;
        let mut shared = session
            .clone
            .lock()
            .map_err(|_| BackendError::Clone("clone state lock was poisoned".to_string()))?;
        if let Some(dest) = shared.dest.as_mut() {
            dest.dest_nonce = Some(dest_nonce);
            dest.transcript = Some(transcript);
            dest.hello_in_flight = false;
        }
        shared.notice = Some(
            "USB sibling session is live. Confirm the six-digit adoption code, then Approve."
                .to_string(),
        );
        shared.error = None;
        Ok(())
    }

    pub async fn accept_clone_source(&self, alias: &str) -> Result<(), BackendError> {
        let locality = evaluate_clone_locality(&self.local_interfaces()?);
        if !locality.allows_clone() {
            return Err(BackendError::Clone(locality.operator_message()));
        }
        let session = self.session()?;
        let secret = load_controller_secret(&session.identities_dir)?;
        let accesses = session
            .handle
            .snapshot_remote_control_target_accesses()
            .await
            .map_err(|error| BackendError::Clone(format!("{error:?}")))?
            .unwrap_or_default();
        let mut shared = session
            .clone
            .lock()
            .map_err(|_| BackendError::Clone("clone state lock was poisoned".to_string()))?;
        let Some(source) = shared.source.as_mut() else {
            return Err(BackendError::Clone(
                "share Operator before accepting".to_string(),
            ));
        };
        if source.transcript.is_none() {
            return Err(BackendError::Clone(
                "the other app has not joined this sibling session yet".to_string(),
            ));
        }
        let mut siblings = {
            let roster = session
                .roster
                .lock()
                .map_err(|_| BackendError::Clone("roster lock was poisoned".to_string()))?;
            roster.replica.siblings.clone()
        };
        if let Some(dest_keys) = source.peer_keys {
            adopt_sibling(
                &mut session
                    .roster
                    .lock()
                    .map_err(|_| BackendError::Clone("roster lock was poisoned".to_string()))?
                    .replica,
                dest_keys,
            );
            remember_sibling_alias(
                session,
                &encode_hex(dest_keys.identity_hash().as_bytes()),
                alias,
            );
            if !siblings
                .iter()
                .any(|sibling| sibling.identity_hash() == dest_keys.identity_hash())
            {
                siblings.push(dest_keys);
            }
        }
        let clock = session
            .roster
            .lock()
            .map_err(|_| BackendError::Clone("roster lock was poisoned".to_string()))?
            .replica
            .clock;
        persist_session_replica(session);
        source.payload = Some(IdentityClonePayload {
            operator_secret: secret,
            accesses,
            clock,
            siblings,
        });
        source.accepted = true;
        shared.notice = Some("You accepted. Waiting for the other app to Accept.".to_string());
        Ok(())
    }

    pub async fn accept_clone_dest(&self, alias: &str) -> Result<(), BackendError> {
        let locality = evaluate_clone_locality(&self.local_interfaces()?);
        if !locality.allows_clone() {
            return Err(BackendError::Clone(locality.operator_message()));
        }
        let session = self.session()?;
        let dest_secret = load_instance_secret(&session.identities_dir)?;
        let dest_material =
            personal_rns::identity::PrivateIdentityMaterial::from_bytes(*dest_secret);
        if session
            .clone
            .lock()
            .map_err(|_| BackendError::Clone("clone state lock was poisoned".to_string()))?
            .dest
            .as_ref()
            .and_then(|dest| dest.transcript)
            .is_none()
        {
            self.send_clone_hello().await?;
        }
        let (destination, announce, transcript) = {
            let mut shared = session
                .clone
                .lock()
                .map_err(|_| BackendError::Clone("clone state lock was poisoned".to_string()))?;
            let dest = shared.dest.as_mut().ok_or_else(|| {
                BackendError::Clone("no sibling push has been heard on USB".to_string())
            })?;
            let transcript = dest.transcript.ok_or_else(|| {
                BackendError::Clone(
                    "the USB sibling session has not finished hello yet".to_string(),
                )
            })?;
            dest.accepted = true;
            (dest.destination, dest.announce, transcript)
        };
        let accept = IdentityCloneAccept::sign(&dest_material, transcript);
        let mut accept_bytes = [0u8; 256];
        let accept_len = accept
            .write(&mut accept_bytes)
            .ok_or_else(|| BackendError::Clone("clone accept is too large".to_string()))?;
        let link_id = session
            .handle
            .establish_link(destination)
            .await
            .map_err(|error| BackendError::Clone(format!("{error:?}")))?;
        for _ in 0..30 {
            let (reply, _) = session
                .handle
                .request(
                    link_id,
                    RequestEndpointId::of(IDENTITY_CLONE_REQUEST_ENDPOINT_ID),
                    &accept_bytes[..accept_len],
                )
                .await
                .map_err(|error| BackendError::Clone(format!("{error:?}")))?;
            match parse_identity_clone_message(&reply) {
                Some(IdentityCloneInbound::WaitingForSource) => {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Some(IdentityCloneInbound::Payload {
                    secret,
                    accesses,
                    clock,
                    siblings,
                }) => {
                    let siblings = parse_clone_siblings(siblings).ok_or_else(|| {
                        BackendError::Clone("clone sibling list was malformed".to_string())
                    })?;
                    adopt_cloned_identity(
                        &session.identities_dir,
                        &session.persist_dir,
                        secret,
                        accesses,
                    )?;
                    {
                        let mut roster = session.roster.lock().map_err(|_| {
                            BackendError::Clone("roster lock was poisoned".to_string())
                        })?;
                        roster.replica.clock = clock;
                        adopt_sibling(&mut roster.replica, announce.source_keys());
                        for sibling in &siblings {
                            if sibling.identity_hash() != dest_material.identity_hash() {
                                adopt_sibling(&mut roster.replica, *sibling);
                            }
                        }
                    }
                    remember_sibling_alias(
                        session,
                        &encode_hex(announce.source().as_bytes()),
                        alias,
                    );
                    let extra = siblings
                        .iter()
                        .filter(|sibling| sibling.identity_hash() != dest_material.identity_hash())
                        .map(|sibling| encode_hex(sibling.identity_hash().as_bytes()))
                        .collect::<Vec<_>>();
                    ensure_missing_sibling_aliases(session, extra.iter().map(String::as_str));
                    persist_session_replica(session);
                    let mut shared = session.clone.lock().map_err(|_| {
                        BackendError::Clone("clone state lock was poisoned".to_string())
                    })?;
                    shared.notice = Some(
                        "Operator identity adopted. Instance is unchanged. Quit and reopen this app so the live node uses the new Operator."
                            .to_string(),
                    );
                    shared.dest_waiting = false;
                    shared.dest = None;
                    shared.error = None;
                    return Ok(());
                }
                other => {
                    return Err(BackendError::Clone(format!(
                        "the other app rejected accept: {other:?}"
                    )))
                }
            }
        }
        Err(BackendError::Clone(
            "the source did not Accept in time".to_string(),
        ))
    }

    pub async fn targets(&self) -> Result<Vec<TargetAccess>, BackendError> {
        let session = self.session()?;
        let inventory = session
            .handle
            .remote_control_target_inventory()
            .await
            .map_err(|error| operation("read target inventory", error))?;
        let mut pairing = session
            .pairing
            .lock()
            .expect("pairing state mutex poisoned");
        if let Some((id, name)) = pairing.remember_unnamed_targets(inventory.targets()) {
            drop(pairing);
            self.record_local_label(RosterLabelKind::TargetName, &id, Some(&name));
            pairing = session
                .pairing
                .lock()
                .expect("pairing state mutex poisoned");
        }
        let mut items = Vec::new();
        for target in inventory.targets() {
            let id = encode_hex(target.identity_hash().as_bytes());
            items.push(TargetAccess {
                name: pairing.paired_announce_name(&id).unwrap_or_default(),
                status: pairing.reachability(&id),
                path: None,
                build_version: session.cached_build_version(&id),
                id,
            });
        }
        let announcements = pairing.active_announcements();
        drop(pairing);
        for item in items.iter_mut() {
            item.path = self.target_path(&item.id).await;
        }
        for announcement in announcements {
            items.push(TargetAccess {
                name: announcement.name,
                status: TargetStatus::AwaitingPairing,
                path: announcement.path,
                build_version: None,
                id: announcement.id,
            });
        }
        Ok(items)
    }

    pub async fn forget_target(&self, target_id: &str) -> Result<(), BackendError> {
        let target = IdentityHash::new(
            parse_hex::<IDENTITY_HASH_BYTES>(target_id)
                .map_err(|_| BackendError::InvalidTargetId(target_id.to_string()))?,
        );
        let session = self.session()?;
        let outcome = session
            .handle
            .forget_remote_control_target(target)
            .await
            .map_err(|error| operation("forget target", error))?;
        session
            .pairing
            .lock()
            .expect("pairing state mutex poisoned")
            .forget_stored(target_id);
        session.forget_build_version(target_id);
        {
            let mut aliases = session
                .target_aliases
                .lock()
                .expect("target aliases mutex poisoned");
            aliases.remove(target_id);
            persist_target_names(&session.target_aliases_path, &aliases);
        }
        match outcome {
            ForgetRemoteControlTargetOutcome::Forgotten { .. }
            | ForgetRemoteControlTargetOutcome::NotFound => {
                if let Ok(mut roster) = session.roster.lock() {
                    forget_target_locally(&mut roster.replica, target);
                }
                persist_session_replica(session);
                spawn({
                    let backend = self.clone();
                    async move {
                        let _ = backend.push_roster_to_siblings().await;
                    }
                });
                Ok(())
            }
        }
    }

    pub async fn interfaces(&self, target_id: &str) -> Result<Vec<InterfaceEntry>, BackendError> {
        self.interfaces_after_announce(target_id, RemoteControlAnnounceWait::UntilHeard)
            .await
    }

    pub async fn interfaces_after_announce(
        &self,
        target_id: &str,
        wait: RemoteControlAnnounceWait,
    ) -> Result<Vec<InterfaceEntry>, BackendError> {
        let announce_deadline = Instant::now() + CONTROL_ANNOUNCE_WAIT;
        let inventory_deadline = Instant::now() + INVENTORY_RECOVERY_WAIT;
        let baseline = match wait {
            RemoteControlAnnounceWait::UntilHeard => None,
            RemoteControlAnnounceWait::UntilRefreshed => self
                .session()?
                .pairing
                .lock()
                .expect("pairing state mutex poisoned")
                .control_announce_baseline
                .get(target_id)
                .copied(),
        };
        loop {
            self.wait_for_control_announce(target_id, wait, baseline, announce_deadline)
                .await?;
            match self.inventory_after_announce(target_id).await {
                Ok(entries) => return Ok(entries),
                Err(error) => {
                    eprintln!("connect to target {target_id} failed: {error}");
                    if !inventory_recovery_continues(Instant::now(), inventory_deadline) {
                        return Err(error);
                    }
                    tokio::time::sleep(INVENTORY_RECOVERY_GAP).await;
                }
            }
        }
    }

    pub async fn begin_pairing(
        &self,
        announcement_id: &str,
        invitation_code: &str,
    ) -> Result<PairingState, BackendError> {
        let session = self.session()?;
        let invitation_code = parse_invitation_code(invitation_code)?;
        let (endpoint, expires_at) = {
            let mut pairing = session
                .pairing
                .lock()
                .expect("pairing state mutex poisoned");
            pairing.pending = None;
            pairing.pin_announcement(announcement_id);
            let heard = pairing
                .announcement(announcement_id)
                .ok_or(BackendError::NoPairingAdvertisement)?;
            pairing.pending_announce_name = pairing.active_announcement_name();
            heard
        };
        self.record_local_label(
            RosterLabelKind::PairingAdvertDismissed,
            announcement_id,
            Some("taken"),
        );
        let control_announce_baseline = self.snapshot_control_announces().await?;
        session
            .pairing
            .lock()
            .expect("pairing state mutex poisoned")
            .control_announce_baseline = control_announce_baseline;
        let received = match session
            .handle
            .initiate_remote_control_controller_pairing(InitiateRemoteControlControllerPairing {
                endpoint,
                invitation_code,
                expires_at,
            })
            .await
        {
            Ok(received) => received,
            Err(error) => {
                return Err(begin_pairing_error(error));
            }
        };

        // Prefer the settled offer outcome (always available when initiate
        // returns Ok). The confirmation journal is a side channel that some
        // desktop executors can miss relative to this await.
        if let AdmitRemoteControlControllerPairingResponseOutcome::Offer(
            ReceiveRemoteControlControllerPairingOfferOutcome::ConfirmationRequired { attempt_id },
        ) = received.admission
        {
            let digits = attempt_id.confirmation_code().to_string();
            let pending = PendingPairing {
                digits: digits.clone(),
                approval: ApproveRemoteControlControllerPairing { attempt_id },
                rejection: RejectRemoteControlControllerPairing { attempt_id },
            };
            session
                .pairing
                .lock()
                .expect("pairing state mutex poisoned")
                .set_pending(pending);
            return Ok(PairingState::AwaitingConfirmation { digits });
        }

        for _ in 0..100 {
            if let Some(digits) = session
                .pairing
                .lock()
                .expect("pairing state mutex poisoned")
                .pending
                .as_ref()
                .map(|pending| pending.digits.clone())
            {
                return Ok(PairingState::AwaitingConfirmation { digits });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(BackendError::Operation {
            operation: "begin controller pairing",
            detail: format!(
                "pairing offer returned without confirmation (admission={:?})",
                received.admission
            ),
        })
    }

    pub async fn approve_pairing(&self) -> Result<PairingState, BackendError> {
        let session = self.session()?;
        let approval = session
            .pairing
            .lock()
            .expect("pairing state mutex poisoned")
            .pending
            .as_ref()
            .map(|pending| pending.approval)
            .ok_or(BackendError::NoPendingPairing)?;
        session
            .handle
            .approve_remote_control_controller_pairing(approval)
            .await
            .map_err(|error| operation("approve controller pairing", error))?;
        let (pending_name, pending_target_id) = {
            let mut pairing = session
                .pairing
                .lock()
                .expect("pairing state mutex poisoned");
            pairing.pending = None;
            let name = pairing
                .active_announcement_name()
                .or_else(|| pairing.pending_announce_name.clone());
            pairing.pending_announce_name = name.clone();
            (name, pairing.pending_target_id.clone())
        };
        let paired = self
            .wait_for_paired_inventory_target(pending_target_id.as_deref())
            .await;
        if let Some(target) = paired {
            let id = encode_hex(target.as_bytes());
            {
                let mut pairing = session
                    .pairing
                    .lock()
                    .expect("pairing state mutex poisoned");
                if let Some(name) = pending_name.as_deref() {
                    pairing.set_announce_name(&id, name);
                }
                pairing.finish_pairing_announcement();
            }
            if let Ok(mut roster) = session.roster.lock() {
                note_local_upsert(&mut roster.replica, target);
                if let Some(name) = pending_name.as_deref() {
                    note_local_label(
                        &mut roster.replica,
                        RosterLabelKind::TargetName,
                        &id,
                        Some(name),
                    );
                }
            }
            persist_session_replica(session);
            self.refresh_cached_snapshot().await;
        }
        spawn({
            let backend = self.clone();
            async move {
                let _ = backend.push_roster_to_siblings().await;
            }
        });
        Ok(PairingState::Approved)
    }

    async fn wait_for_paired_inventory_target(
        &self,
        pending_id: Option<&str>,
    ) -> Option<IdentityHash> {
        let session = self.session().ok()?;
        for _ in 0..20 {
            if let Ok(inventory) = session.handle.remote_control_target_inventory().await {
                let hashes: Vec<IdentityHash> = inventory
                    .targets()
                    .iter()
                    .map(|target| target.identity_hash())
                    .collect();
                if let Some(hash) = resolve_paired_target_hash(&hashes, pending_id) {
                    return Some(hash);
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let inventory = session
            .handle
            .remote_control_target_inventory()
            .await
            .ok()?;
        let hashes: Vec<IdentityHash> = inventory
            .targets()
            .iter()
            .map(|target| target.identity_hash())
            .collect();
        resolve_paired_target_hash(&hashes, pending_id)
    }

    async fn refresh_cached_snapshot(&self) {
        let Ok(session) = self.session() else {
            return;
        };
        if let Ok(snapshot) = session
            .handle
            .snapshot_remote_control_target_accesses()
            .await
        {
            if let Ok(mut roster) = session.roster.lock() {
                roster.cached_snapshot = snapshot.unwrap_or_default();
            }
        }
    }

    pub async fn reject_pairing(&self) -> Result<PairingState, BackendError> {
        let session = self.session()?;
        let rejection = session
            .pairing
            .lock()
            .expect("pairing state mutex poisoned")
            .pending
            .as_ref()
            .map(|pending| pending.rejection)
            .ok_or(BackendError::NoPendingPairing)?;
        session
            .handle
            .reject_remote_control_controller_pairing(rejection)
            .await
            .map_err(|error| operation("reject controller pairing", error))?;
        let mut pairing = session
            .pairing
            .lock()
            .expect("pairing state mutex poisoned");
        pairing.pending = None;
        if let Some(id) = pairing.active_announcement_id.clone() {
            pairing.pinned.remove(&id);
        }
        Ok(PairingState::Rejected)
    }

    pub fn target_announce_name(&self, target_id: &str) -> Option<String> {
        self.session().ok().and_then(|session| {
            session
                .pairing
                .lock()
                .expect("pairing state mutex poisoned")
                .paired_announce_name(target_id)
        })
    }

    pub fn target_aliases(&self) -> HashMap<String, String> {
        self.session()
            .map(|session| {
                session
                    .target_aliases
                    .lock()
                    .expect("target aliases mutex poisoned")
                    .clone()
            })
            .unwrap_or_default()
    }

    pub fn set_target_alias(&self, target_id: &str, alias: &str) -> Result<(), BackendError> {
        let session = self.session()?;
        let id = target_id.trim();
        if id.is_empty() {
            return Ok(());
        }
        let name = alias.trim();
        let mut aliases = session
            .target_aliases
            .lock()
            .expect("target aliases mutex poisoned");
        if name.is_empty() {
            aliases.remove(id);
        } else {
            aliases.insert(id.to_owned(), name.to_owned());
        }
        persist_target_names(&session.target_aliases_path, &aliases);
        self.record_local_label(
            RosterLabelKind::TargetAlias,
            id,
            (!name.is_empty()).then_some(name),
        );
        Ok(())
    }

    pub fn peer_aliases(&self) -> HashMap<String, String> {
        self.session()
            .map(|session| {
                session
                    .peer_aliases
                    .lock()
                    .expect("peer aliases mutex poisoned")
                    .clone()
            })
            .unwrap_or_default()
    }

    pub fn manager_aliases(&self) -> HashMap<String, String> {
        self.session()
            .map(|session| {
                session
                    .manager_aliases
                    .lock()
                    .expect("manager aliases mutex poisoned")
                    .clone()
            })
            .unwrap_or_default()
    }

    pub fn set_manager_alias(&self, hash: &str, alias: &str) -> Result<(), BackendError> {
        let session = self.session()?;
        let id = hash.trim();
        if id.is_empty() {
            return Ok(());
        }
        let name = alias.trim();
        let mut aliases = session
            .manager_aliases
            .lock()
            .expect("manager aliases mutex poisoned");
        if name.is_empty() {
            aliases.remove(id);
        } else {
            aliases.insert(id.to_owned(), name.to_owned());
        }
        persist_target_names(&session.manager_aliases_path, &aliases);
        self.record_local_label(
            RosterLabelKind::ManagerAlias,
            id,
            (!name.is_empty()).then_some(name),
        );
        Ok(())
    }

    pub fn set_peer_alias(&self, peer_id: &str, alias: &str) -> Result<(), BackendError> {
        let session = self.session()?;
        let id = peer_id.trim();
        if id.is_empty() {
            return Ok(());
        }
        let name = alias.trim();
        let mut aliases = session
            .peer_aliases
            .lock()
            .expect("peer aliases mutex poisoned");
        if name.is_empty() {
            aliases.remove(id);
        } else {
            aliases.insert(id.to_owned(), name.to_owned());
        }
        persist_target_names(&session.peer_aliases_path, &aliases);
        if peer_alias_is_syncable(id) {
            self.record_local_label(
                RosterLabelKind::PeerAlias,
                id,
                (!name.is_empty()).then_some(name),
            );
        }
        Ok(())
    }

    pub fn set_sibling_alias(&self, instance_hash: &str, alias: &str) -> Result<(), BackendError> {
        let session = self.session()?;
        let id = instance_hash.trim();
        if id.is_empty()
            || !sibling_alias_is_syncable(id, &session.controller_identity.instance_hash)
        {
            return Ok(());
        }
        remember_sibling_alias(session, id, alias);
        let backend = self.clone();
        spawn(async move {
            let _ = backend.push_roster_to_siblings().await;
        });
        Ok(())
    }

    pub fn sibling_aliases(&self) -> HashMap<String, String> {
        self.session()
            .ok()
            .map(|session| {
                session
                    .sibling_aliases
                    .lock()
                    .expect("sibling aliases mutex poisoned")
                    .clone()
            })
            .unwrap_or_default()
    }

    pub fn forget_sibling(&self, instance_hash: &str) -> Result<(), BackendError> {
        let session = self.session()?;
        let id = instance_hash.trim();
        if id.is_empty()
            || !sibling_alias_is_syncable(id, &session.controller_identity.instance_hash)
        {
            return Ok(());
        }
        let hash = IdentityHash::new(parse_hex::<IDENTITY_HASH_BYTES>(id).map_err(|_| {
            BackendError::Operation {
                operation: "remove sibling",
                detail: "sibling Instance hash must be 32 hex characters".to_string(),
            }
        })?);
        {
            let mut roster = session.roster.lock().map_err(|_| BackendError::Operation {
                operation: "remove sibling",
                detail: "roster lock was poisoned".to_string(),
            })?;
            forget_sibling_locally(&mut roster.replica, hash);
            roster.applied_generation = roster.applied_generation.saturating_add(1);
        }
        remove_alias_map_key(&session.sibling_aliases, &session.sibling_aliases_path, id);
        persist_session_replica(session);
        let backend = self.clone();
        spawn(async move {
            let _ = backend.push_roster_to_siblings().await;
        });
        Ok(())
    }

    pub fn suggested_sibling_alias(&self) -> String {
        self.session()
            .ok()
            .map(|session| {
                next_sibling_alias(
                    &session
                        .sibling_aliases
                        .lock()
                        .expect("sibling aliases mutex poisoned"),
                )
            })
            .unwrap_or_else(|| "Sibling 1".to_string())
    }

    pub fn roster_apply_generation(&self) -> u64 {
        self.session()
            .ok()
            .and_then(|session| session.roster.lock().ok())
            .map(|roster| roster.applied_generation)
            .unwrap_or(0)
    }

    fn record_local_label(&self, kind: RosterLabelKind, key: &str, value: Option<&str>) {
        let Ok(session) = self.session() else {
            return;
        };
        if let Ok(mut roster) = session.roster.lock() {
            note_local_label(&mut roster.replica, kind, key, value);
        }
        persist_session_replica(session);
        let backend = self.clone();
        spawn(async move {
            let _ = backend.push_roster_to_siblings().await;
        });
    }

    pub fn local_interfaces(&self) -> Result<Vec<InterfaceEntry>, BackendError> {
        let session = self.session()?;
        let tcp_target = session.tcp_target.as_deref();
        let aliases = session
            .peer_aliases
            .lock()
            .expect("peer aliases mutex poisoned")
            .clone();
        let raw = session.handle.interface_inventory();
        let members = raw.clone();
        let mut items: Vec<InterfaceEntry> = logical_interface_inventory(raw)
            .into_iter()
            .filter(operator_local_interface)
            .map(|entry| {
                let mut peers: Vec<InterfacePeer> = members
                    .iter()
                    .filter_map(|member| match member.snapshot.membership {
                        Membership::FleetMember { supervisor_id }
                            if supervisor_id == entry.snapshot.id =>
                        {
                            Some(interface_peer(&member.snapshot))
                        }
                        Membership::Independent | Membership::FleetMember { .. } => None,
                    })
                    .collect();
                apply_peer_aliases(&mut peers, &aliases);
                local_interface_entry(
                    &entry.snapshot,
                    entry.name.as_deref(),
                    Some(session.ble_identity),
                    entry.group.as_deref(),
                    entry.ifac.as_ref().map(|ifac| ifac.size.bytes()),
                    local_interface_config(
                        &entry.snapshot,
                        entry.ifac.as_ref().map(|ifac| ifac.size.bytes()),
                        tcp_target,
                    )
                    .as_deref(),
                    entry.snapshot.failure_reason,
                    local_frame_drops(&entry.frame_accounting),
                    peers,
                )
            })
            .collect();
        for item in &mut items {
            attach_usb_link_peer(item);
        }
        session.stamp_activity(ACTIVITY_CONTROLLER_SCOPE, &mut items);
        Ok(items)
    }

    pub fn set_local_interface_power(
        &self,
        interface_id: &str,
        power: InterfacePower,
    ) -> Result<(), BackendError> {
        let id = InterfaceId::new(
            parse_hex::<INTERFACE_ID_BYTES>(interface_id)
                .map_err(|_| BackendError::InvalidInterfaceId(interface_id.to_string()))?,
        );
        self.session()?.local_power.set(id, power)
    }

    pub fn set_local_interface_mode(
        &self,
        interface_id: &str,
        mode: InterfaceMode,
    ) -> Result<(), BackendError> {
        let id = InterfaceId::new(
            parse_hex::<INTERFACE_ID_BYTES>(interface_id)
                .map_err(|_| BackendError::InvalidInterfaceId(interface_id.to_string()))?,
        );
        match self.session()?.handle.set_interface_mode(id, mode) {
            RemoteControlModeOutcome::Applied => Ok(()),
            RemoteControlModeOutcome::UnknownInterface => Err(BackendError::Operation {
                operation: "set local interface mode",
                detail: "this controller does not know that interface".to_string(),
            }),
            RemoteControlModeOutcome::Failed => Err(BackendError::Operation {
                operation: "set local interface mode",
                detail: "the controller could not apply the requested mode".to_string(),
            }),
        }
    }

    pub fn set_local_interface_group(
        &self,
        interface_id: &str,
        group: &str,
    ) -> Result<(), BackendError> {
        let id = InterfaceId::new(
            parse_hex::<INTERFACE_ID_BYTES>(interface_id)
                .map_err(|_| BackendError::InvalidInterfaceId(interface_id.to_string()))?,
        );
        let Some(group) = RemoteControlInterfaceGroup::parse(group.trim()) else {
            return Err(BackendError::Operation {
                operation: "set local interface group",
                detail: "group id must be 1 to 32 UTF-8 bytes".to_string(),
            });
        };
        match self.session()?.handle.set_interface_group(id, group) {
            RemoteControlGroupOutcome::Applied => Ok(()),
            RemoteControlGroupOutcome::UnknownInterface => Err(BackendError::Operation {
                operation: "set local interface group",
                detail: "this controller does not know that interface".to_string(),
            }),
            RemoteControlGroupOutcome::Failed => Err(BackendError::Operation {
                operation: "set local interface group",
                detail: "the controller could not apply the requested group".to_string(),
            }),
        }
    }

    pub async fn set_interface_power(
        &self,
        target_id: &str,
        interface_id: &str,
        power: InterfacePower,
    ) -> Result<(), BackendError> {
        let id = InterfaceId::new(
            parse_hex::<INTERFACE_ID_BYTES>(interface_id)
                .map_err(|_| BackendError::InvalidInterfaceId(interface_id.to_string()))?,
        );
        let remote = self.connect_target(target_id).await?;
        let (outcome, _) = remote
            .set_interface_power(
                id,
                match power {
                    InterfacePower::On => RemoteControlInterfacePower::On,
                    InterfacePower::Off => RemoteControlInterfacePower::Off,
                },
            )
            .await
            .map_err(|error| operation("set interface power", error))?;
        remote.close();
        match outcome {
            RemoteControlPowerOutcome::Applied => Ok(()),
            RemoteControlPowerOutcome::UnknownInterface => Err(BackendError::Operation {
                operation: "set interface power",
                detail: "the target does not know that interface".to_string(),
            }),
            RemoteControlPowerOutcome::Failed => Err(BackendError::Operation {
                operation: "set interface power",
                detail: "the target could not apply the requested state".to_string(),
            }),
        }
    }

    pub async fn set_interface_mode(
        &self,
        target_id: &str,
        interface_id: &str,
        mode: InterfaceMode,
    ) -> Result<(), BackendError> {
        let id = InterfaceId::new(
            parse_hex::<INTERFACE_ID_BYTES>(interface_id)
                .map_err(|_| BackendError::InvalidInterfaceId(interface_id.to_string()))?,
        );
        let remote = self.connect_target(target_id).await?;
        let (outcome, _) = remote
            .set_interface_mode(id, mode)
            .await
            .map_err(|error| operation("set interface mode", error))?;
        remote.close();
        match outcome {
            RemoteControlModeOutcome::Applied => Ok(()),
            RemoteControlModeOutcome::UnknownInterface => Err(BackendError::Operation {
                operation: "set interface mode",
                detail: "the target does not know that interface".to_string(),
            }),
            RemoteControlModeOutcome::Failed => Err(BackendError::Operation {
                operation: "set interface mode",
                detail: "the target could not apply the requested mode".to_string(),
            }),
        }
    }

    pub async fn set_interface_group(
        &self,
        target_id: &str,
        interface_id: &str,
        group: &str,
    ) -> Result<(), BackendError> {
        let id = InterfaceId::new(
            parse_hex::<INTERFACE_ID_BYTES>(interface_id)
                .map_err(|_| BackendError::InvalidInterfaceId(interface_id.to_string()))?,
        );
        let Some(group) = RemoteControlInterfaceGroup::parse(group.trim()) else {
            return Err(BackendError::Operation {
                operation: "set interface group",
                detail: "group id must be 1 to 32 UTF-8 bytes".to_string(),
            });
        };
        let remote = self.connect_target(target_id).await?;
        let (outcome, _) = remote
            .set_interface_group(id, group)
            .await
            .map_err(|error| operation("set interface group", error))?;
        remote.close();
        match outcome {
            RemoteControlGroupOutcome::Applied => Ok(()),
            RemoteControlGroupOutcome::UnknownInterface => Err(BackendError::Operation {
                operation: "set interface group",
                detail: "the target does not know that interface".to_string(),
            }),
            RemoteControlGroupOutcome::Failed => Err(BackendError::Operation {
                operation: "set interface group",
                detail: "the target could not apply the requested group".to_string(),
            }),
        }
    }

    pub async fn set_interface_lora_profile(
        &self,
        target_id: &str,
        interface_id: &str,
        profile: RadioProfile,
    ) -> Result<(), BackendError> {
        let id = InterfaceId::new(
            parse_hex::<INTERFACE_ID_BYTES>(interface_id)
                .map_err(|_| BackendError::InvalidInterfaceId(interface_id.to_string()))?,
        );
        let Some(profile) = RemoteControlLoRaProfile::from_profile(profile) else {
            return Err(BackendError::Operation {
                operation: "set interface LoRa profile",
                detail: "the requested radio settings are outside the selected region's limits"
                    .to_string(),
            });
        };
        let remote = self.connect_target(target_id).await?;
        let (outcome, _) = remote
            .set_interface_lora_profile(id, profile)
            .await
            .map_err(lora_profile_exchange_error)?;
        remote.close();
        match outcome {
            RemoteControlLoRaOutcome::Applied => Ok(()),
            RemoteControlLoRaOutcome::UnknownInterface => Err(BackendError::Operation {
                operation: "set interface LoRa profile",
                detail: "the target does not know that LoRa interface".to_string(),
            }),
            RemoteControlLoRaOutcome::Failed => Err(BackendError::Operation {
                operation: "set interface LoRa profile",
                detail: "the target could not apply the requested radio settings".to_string(),
            }),
        }
    }

    pub async fn set_interface_wifi_station(
        &self,
        target_id: &str,
        interface_id: &str,
        ssid: &str,
        password: &str,
    ) -> Result<(), BackendError> {
        let id = InterfaceId::new(
            parse_hex::<INTERFACE_ID_BYTES>(interface_id)
                .map_err(|_| BackendError::InvalidInterfaceId(interface_id.to_string()))?,
        );
        let Some(station) = RemoteControlWifiStation::parse(ssid.trim(), password) else {
            return Err(BackendError::Operation {
                operation: "set interface Wi-Fi station",
                detail: "SSID must be 1 to 32 UTF-8 bytes; password may be empty or up to 64 UTF-8 bytes".to_string(),
            });
        };
        let remote = self.connect_target(target_id).await?;
        let (outcome, _) = remote
            .set_interface_wifi_station(id, station)
            .await
            .map_err(wifi_station_exchange_error)?;
        remote.close();
        match outcome {
            RemoteControlWifiStationOutcome::Applied => Ok(()),
            RemoteControlWifiStationOutcome::UnknownInterface => Err(BackendError::Operation {
                operation: "set interface Wi-Fi station",
                detail: "the target does not know that Auto Wi-Fi interface".to_string(),
            }),
            RemoteControlWifiStationOutcome::Failed => Err(BackendError::Operation {
                operation: "set interface Wi-Fi station",
                detail: "the target has no live station radio to join. Flash Wi-Fi once so the board starts a station stack, then you can change SSID and password here".to_string(),
            }),
        }
    }

    pub async fn inventory_controllers(
        &self,
        target_id: &str,
    ) -> Result<Vec<String>, BackendError> {
        let remote = self.connect_target(target_id).await?;
        let (inventory, _) = remote
            .inventory_controllers()
            .await
            .map_err(controller_whitelist_exchange_error)?;
        remote.close();
        Ok(inventory
            .hashes()
            .iter()
            .map(|hash| encode_hex(hash.as_bytes()))
            .collect())
    }

    pub async fn authorize_controller(
        &self,
        target_id: &str,
        allow_list_key: &str,
    ) -> Result<(), BackendError> {
        let controller = parse_allow_list_key(allow_list_key)?;
        let remote = self.connect_target(target_id).await?;
        let (outcome, _) = remote
            .authorize_controller(controller)
            .await
            .map_err(controller_whitelist_exchange_error)?;
        remote.close();
        match outcome {
            RemoteControlAuthorizeControllerOutcome::Applied => Ok(()),
            RemoteControlAuthorizeControllerOutcome::CapacityExhausted => {
                Err(BackendError::Operation {
                    operation: "authorize controller",
                    detail: "the target allow-list is full".to_string(),
                })
            }
            RemoteControlAuthorizeControllerOutcome::Failed => Err(BackendError::Operation {
                operation: "authorize controller",
                detail: "the target could not add that controller".to_string(),
            }),
        }
    }

    pub async fn revoke_controller(&self, target_id: &str, hash: &str) -> Result<(), BackendError> {
        let hash = IdentityHash::new(parse_hex::<IDENTITY_HASH_BYTES>(hash).map_err(|_| {
            BackendError::Operation {
                operation: "revoke controller",
                detail: "controller hash must be 32 hex characters".to_string(),
            }
        })?);
        let remote = self.connect_target(target_id).await?;
        let (outcome, _) = remote
            .revoke_controller(hash)
            .await
            .map_err(controller_whitelist_exchange_error)?;
        remote.close();
        match outcome {
            RemoteControlRevokeControllerOutcome::Applied => Ok(()),
            RemoteControlRevokeControllerOutcome::NotFound => Err(BackendError::Operation {
                operation: "revoke controller",
                detail: "that controller is not on the allow-list".to_string(),
            }),
            RemoteControlRevokeControllerOutcome::Forbidden => Err(BackendError::Operation {
                operation: "revoke controller",
                detail: "this controller cannot remove itself from the allow-list".to_string(),
            }),
            RemoteControlRevokeControllerOutcome::Failed => Err(BackendError::Operation {
                operation: "revoke controller",
                detail: "the target could not remove that controller".to_string(),
            }),
        }
    }

    pub async fn refresh_build_version(
        &self,
        target_id: &str,
    ) -> Result<Option<String>, BackendError> {
        if let Some(cached) = self.session()?.cached_build_version(target_id) {
            return Ok(Some(cached));
        }
        let remote = self.connect_target(target_id).await?;
        let result = remote.describe_build().await;
        remote.close();
        match result {
            Ok((version, _)) => {
                let text = version
                    .as_str()
                    .filter(|text| !text.is_empty())
                    .map(ToOwned::to_owned);
                if let Some(text) = text.clone() {
                    self.session()?.remember_build_version(target_id, text);
                }
                Ok(text)
            }
            Err(_) => Ok(None),
        }
    }

    pub async fn probe_target(&self, target_id: &str) -> Result<TargetPath, BackendError> {
        self.request_control_path(target_id).await?;
        match self.target_path(target_id).await {
            Some(path) => {
                self.session()?
                    .mark_reachable(target_id, TargetStatus::Online);
                Ok(path)
            }
            None => {
                self.session()?
                    .mark_reachable(target_id, TargetStatus::Offline);
                Err(BackendError::Operation {
                    operation: "find path to target",
                    detail: "the target did not answer a path request for its remote-control destination"
                        .to_string(),
                })
            }
        }
    }

    pub async fn announce(&self, target_id: &str) -> Result<(), BackendError> {
        let remote = self.connect_target(target_id).await?;
        remote
            .announce_self()
            .await
            .map_err(|error| operation("announce target", error))?;
        remote.close();
        self.session()?
            .mark_reachable(target_id, TargetStatus::Online);
        Ok(())
    }

    pub async fn sleep(&self, target_id: &str) -> Result<(), BackendError> {
        self.set_radio_sleep(target_id, RadioAction::Sleep).await
    }

    pub async fn wake(&self, target_id: &str) -> Result<(), BackendError> {
        self.set_radio_sleep(target_id, RadioAction::Wake).await
    }

    fn session(&self) -> Result<&Arc<ControllerSession>, BackendError> {
        self.session.as_ref().map_err(Clone::clone)
    }

    async fn target_path(&self, target_id: &str) -> Option<TargetPath> {
        let session = self.session().ok()?;
        let target = IdentityHash::new(parse_hex::<IDENTITY_HASH_BYTES>(target_id).ok()?);
        let resolved = session
            .handle
            .resolve_remote_control_target(target)
            .await
            .ok()?;
        let route = session
            .handle
            .route(resolved.endpoint().destination_hash())
            .await?;
        Some(path_from_route(&route))
    }

    async fn snapshot_control_announces(
        &self,
    ) -> Result<HashMap<String, InstantMillis>, BackendError> {
        let session = self.session()?;
        let inventory = session
            .handle
            .remote_control_target_inventory()
            .await
            .map_err(|error| operation("read target inventory", error))?;
        let mut baselines = HashMap::new();
        for target in inventory.targets() {
            let id = encode_hex(target.identity_hash().as_bytes());
            if let Some(learned_at) = self.control_announce_learned_at(&id).await? {
                baselines.insert(id, learned_at);
            }
        }
        Ok(baselines)
    }

    async fn wait_for_control_announce(
        &self,
        target_id: &str,
        wait: RemoteControlAnnounceWait,
        baseline: Option<InstantMillis>,
        deadline: Instant,
    ) -> Result<(), BackendError> {
        let _ = self.request_control_path(target_id).await;
        loop {
            let heard = self.control_announce_learned_at(target_id).await?;
            if control_announce_satisfies(heard, wait, baseline) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BackendError::Operation {
                    operation: "wait for remote control announce",
                    detail: "the controller has not heard the target's remote-control destination"
                        .to_string(),
                });
            }
            tokio::time::sleep(CONTROL_ANNOUNCE_POLL).await;
        }
    }

    async fn control_destination(
        &self,
        target_id: &str,
    ) -> Result<Option<DestinationHash>, BackendError> {
        let session = self.session()?;
        let target = IdentityHash::new(
            parse_hex::<IDENTITY_HASH_BYTES>(target_id)
                .map_err(|_| BackendError::InvalidTargetId(target_id.to_string()))?,
        );
        let Ok(resolved) = session.handle.resolve_remote_control_target(target).await else {
            return Ok(None);
        };
        Ok(Some(resolved.endpoint().destination_hash()))
    }

    async fn request_control_path(&self, target_id: &str) -> Result<(), BackendError> {
        let Some(destination) = self.control_destination(target_id).await? else {
            return Err(BackendError::Operation {
                operation: "find path to target",
                detail: "the controller has not stored that target yet".to_string(),
            });
        };
        self.session()?
            .handle
            .request_path(destination)
            .await
            .map(|_| ())
            .map_err(|error: RequestPathError| operation("find path to target", error))
    }

    async fn control_announce_learned_at(
        &self,
        target_id: &str,
    ) -> Result<Option<InstantMillis>, BackendError> {
        let session = self.session()?;
        let Some(destination) = self.control_destination(target_id).await? else {
            return Ok(None);
        };
        Ok(session
            .handle
            .route(destination)
            .await
            .map(|route| route.learned_at))
    }

    async fn inventory_after_announce(
        &self,
        target_id: &str,
    ) -> Result<Vec<InterfaceEntry>, BackendError> {
        let mut last_error = None;
        for attempt in 0..CONNECT_AFTER_ANNOUNCE_ATTEMPTS {
            match self.inventory_connected_target(target_id).await {
                Ok(entries) => return Ok(entries),
                Err(error) => {
                    last_error = Some(error);
                    if attempt + 1 < CONNECT_AFTER_ANNOUNCE_ATTEMPTS {
                        tokio::time::sleep(CONNECT_AFTER_ANNOUNCE_GAP).await;
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| operation("inventory target interfaces", "unknown error")))
    }

    async fn inventory_connected_target(
        &self,
        target_id: &str,
    ) -> Result<Vec<InterfaceEntry>, BackendError> {
        let remote = self.connect_target(target_id).await?;
        let (inventory, _) = remote
            .inventory_interfaces()
            .await
            .map_err(|error| operation("inventory target interfaces", error))?;
        self.session()?
            .mark_reachable(target_id, TargetStatus::Online);
        let mut items = inventory
            .entries()
            .iter()
            .enumerate()
            .filter(|(_, entry)| operator_interface_kind(entry.kind))
            .map(|(index, entry)| remote_interface_entry(entry, inventory.card(index)))
            .collect::<Vec<_>>();
        if inventory.cards().is_empty() {
            for item in items.iter_mut() {
                fetch_interface_config(&remote, item).await;
            }
        }
        if let Ok((version, _)) = remote.describe_build().await {
            if let Some(text) = version.as_str().filter(|text| !text.is_empty()) {
                self.session()?
                    .remember_build_version(target_id, text.to_owned());
            }
        }
        for item in items.iter_mut() {
            if !item.shows_peers {
                continue;
            }
            let Ok(id) = parse_hex::<INTERFACE_ID_BYTES>(&item.id) else {
                continue;
            };
            item.peers = fetch_interface_peers(&remote, InterfaceId::new(id)).await;
        }
        remote.close();
        let aliases = self
            .session()?
            .peer_aliases
            .lock()
            .expect("peer aliases mutex poisoned")
            .clone();
        for item in items.iter_mut() {
            apply_peer_aliases(&mut item.peers, &aliases);
        }
        self.session()?.stamp_activity(target_id, &mut items);
        Ok(items)
    }

    async fn connect_target(
        &self,
        target_id: &str,
    ) -> Result<RemoteControlTargetHandle<'_>, BackendError> {
        let target = IdentityHash::new(
            parse_hex::<IDENTITY_HASH_BYTES>(target_id)
                .map_err(|_| BackendError::InvalidTargetId(target_id.to_string()))?,
        );
        match self
            .session()?
            .handle
            .connect_remote_control_target(target)
            .await
        {
            Ok(remote) => Ok(remote),
            Err(error) => {
                self.session()?
                    .mark_reachable(target_id, TargetStatus::Offline);
                Err(operation("connect to target", error))
            }
        }
    }

    async fn set_radio_sleep(
        &self,
        target_id: &str,
        action: RadioAction,
    ) -> Result<(), BackendError> {
        let remote = self.connect_target(target_id).await?;
        let result = match action {
            RadioAction::Sleep => remote.sleep_radios().await,
            RadioAction::Wake => remote.wake_radios().await,
        }
        .map_err(|error| operation(action.operation(), error))?;
        remote.close();
        match result.0 {
            RemoteControlSleepOutcome::Applied => {
                self.session()?.mark_reachable(
                    target_id,
                    match action {
                        RadioAction::Sleep => TargetStatus::Sleeping,
                        RadioAction::Wake => TargetStatus::Online,
                    },
                );
                Ok(())
            }
            RemoteControlSleepOutcome::Unavailable => Err(BackendError::Operation {
                operation: action.operation(),
                detail: "the target does not expose radio sleep control".to_string(),
            }),
            RemoteControlSleepOutcome::Failed => Err(BackendError::Operation {
                operation: action.operation(),
                detail: "the target could not apply the requested radio state".to_string(),
            }),
        }
    }

    pub async fn maintain_roster(&self) {
        let Ok(session) = self.session() else {
            return;
        };
        if let Ok(snapshot) = session
            .handle
            .snapshot_remote_control_target_accesses()
            .await
        {
            if let Ok(mut roster) = session.roster.lock() {
                roster.cached_snapshot = snapshot.unwrap_or_default();
            }
        }
        let pending = session
            .roster
            .lock()
            .ok()
            .and_then(|mut roster| roster.pending.take());
        if let Some(delta) = pending {
            let plan = {
                let Ok(mut roster) = session.roster.lock() else {
                    return;
                };
                let (merged, plan) = merge_roster(&roster.replica, &delta);
                roster.replica = merged;
                roster.applied_generation = roster.applied_generation.saturating_add(1);
                plan
            };
            persist_session_replica(session);
            for access in plan.upserts {
                let _ = session
                    .handle
                    .set_remote_control_target_access(access)
                    .await;
            }
            for hash in plan.forgets {
                let id = encode_hex(hash.as_bytes());
                let _ = session.handle.forget_remote_control_target(hash).await;
                session
                    .pairing
                    .lock()
                    .expect("pairing state mutex poisoned")
                    .forget_stored(&id);
                session.forget_build_version(&id);
                remove_alias_map_key(&session.target_aliases, &session.target_aliases_path, &id);
            }
            apply_roster_labels(session, &plan.labels);
        }
        let cloning = session
            .clone
            .lock()
            .ok()
            .is_some_and(|shared| shared.in_progress());
        if cloning {
            return;
        }
        let has_siblings = session
            .roster
            .lock()
            .ok()
            .is_some_and(|roster| !roster.replica.siblings.is_empty());
        if !has_siblings {
            return;
        }
        let due = session
            .last_roster_sync
            .lock()
            .ok()
            .is_none_or(|last| last.is_none_or(|at| at.elapsed() >= ROSTER_ANNOUNCE_GAP));
        if due {
            if let Ok(mut last) = session.last_roster_sync.lock() {
                *last = Some(Instant::now());
            }
            let _ = self.announce_roster_destination().await;
        }
        let pull_due = session
            .roster
            .lock()
            .ok()
            .map(|mut roster| roster.take_pull_due())
            .unwrap_or_default();
        if !pull_due.is_empty() {
            let _ = self
                .exchange_roster_with_siblings(RosterExchange::Pull, Some(&pull_due))
                .await;
        }
    }

    async fn push_roster_to_siblings(&self) -> Result<(), BackendError> {
        let cloning = self
            .session()?
            .clone
            .lock()
            .ok()
            .is_some_and(|shared| shared.in_progress());
        if cloning {
            return Ok(());
        }
        let _ = self.announce_roster_destination().await;
        self.exchange_roster_with_siblings(RosterExchange::Push, None)
            .await
    }

    async fn announce_roster_destination(&self) -> Result<(), BackendError> {
        let session = self.session()?;
        let instance_hash = IdentityHash::new(
            parse_hex::<IDENTITY_HASH_BYTES>(&session.controller_identity.instance_hash)
                .unwrap_or([0u8; IDENTITY_HASH_BYTES]),
        );
        let Some(destination) = roster_sync_destination_hash(instance_hash) else {
            return Ok(());
        };
        session
            .handle
            .announce_now(AnnounceNow {
                destination,
                target: AnnounceTarget::AllInterfaces,
                app_data: AnnounceAppData::Registered,
            })
            .await
            .map_err(|error| BackendError::Clone(format!("{error:?}")))?;
        Ok(())
    }

    async fn exchange_roster_with_siblings(
        &self,
        exchange: RosterExchange,
        only: Option<&[IdentityHash]>,
    ) -> Result<(), BackendError> {
        let session = self.session()?;
        let (siblings, instance, replica, snapshot) = {
            let roster = session
                .roster
                .lock()
                .map_err(|_| BackendError::Clone("roster lock was poisoned".to_string()))?;
            let instance = roster
                .instance_secret
                .as_ref()
                .map(|secret| personal_rns::identity::PrivateIdentityMaterial::from_bytes(**secret))
                .ok_or_else(|| BackendError::Clone("instance secret is missing".to_string()))?;
            let siblings = roster
                .replica
                .siblings
                .iter()
                .copied()
                .filter(|sibling| roster.heard_sibling(sibling.identity_hash()))
                .filter(|sibling| {
                    only.is_none_or(|wanted| wanted.contains(&sibling.identity_hash()))
                })
                .collect::<Vec<_>>();
            (
                siblings,
                instance,
                roster.replica.clone(),
                roster.cached_snapshot.clone(),
            )
        };
        if siblings.is_empty() {
            return Ok(());
        }
        let request = match exchange {
            RosterExchange::Pull => {
                let mut request = vec![0u8; 1 + IDENTITY_PUBLIC_KEY_LEN + 64];
                let written = write_pull(&instance, &mut request).ok_or_else(|| {
                    BackendError::Clone("roster message is too large".to_string())
                })?;
                request.truncate(written);
                request
            }
            RosterExchange::Push => replica_message(&instance, &replica, &snapshot)
                .ok_or_else(|| BackendError::Clone("roster message is too large".to_string()))?,
        };
        for sibling in siblings {
            let Some(destination) = roster_sync_destination_hash(sibling.identity_hash()) else {
                continue;
            };
            let Ok(link_id) = session.handle.establish_link(destination).await else {
                continue;
            };
            let Ok((reply, _)) = session
                .handle
                .request(
                    link_id,
                    RequestEndpointId::of(ROSTER_SYNC_REQUEST_ENDPOINT_ID),
                    &request,
                )
                .await
            else {
                continue;
            };
            if let Some(delta) = parse_replica_reply(&reply) {
                if let Ok(mut roster) = session.roster.lock() {
                    roster.pending = Some(delta);
                }
            }
        }
        Ok(())
    }
}

enum RosterExchange {
    Pull,
    Push,
}

struct ControllerSession {
    handle: PrnsNodeHandle,
    pairing: Arc<Mutex<PairingEvents>>,
    local_power: Arc<LocalInterfacePower>,
    tcp_target: Option<String>,
    controller_identity: ControllerIdentity,
    activity: Mutex<HashMap<String, InterfaceActivityStamp>>,
    peer_aliases: Mutex<HashMap<String, String>>,
    peer_aliases_path: PathBuf,
    manager_aliases: Mutex<HashMap<String, String>>,
    manager_aliases_path: PathBuf,
    target_aliases: Mutex<HashMap<String, String>>,
    target_aliases_path: PathBuf,
    sibling_aliases: Mutex<HashMap<String, String>>,
    sibling_aliases_path: PathBuf,
    build_versions: Mutex<HashMap<String, String>>,
    clone: Arc<Mutex<IdentityCloneShared>>,
    roster: Arc<Mutex<RosterShared>>,
    ble_identity: BleIdentity,
    identities_dir: PathBuf,
    persist_dir: PathBuf,
    data_dir: PathBuf,
    last_roster_sync: Mutex<Option<Instant>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InterfaceActivitySignature {
    connection: u8,
    tx_bytes: u64,
    rx_bytes: u64,
    links: u32,
    destinations: u32,
    rate_bytes_per_sec: u32,
    radio: RadioIndication,
}

struct InterfaceActivityStamp {
    signature: InterfaceActivitySignature,
    last_at: Option<Instant>,
}

struct LocalInterfacePower {
    controls: Mutex<HashMap<InterfaceId, LocalPowerControl>>,
}

enum LocalPowerControl {
    Tokio(TokioInterfaceStatus),
    Wifi(AutoWifiStatus),
    Ble(BluetoothAutoStatus),
}

impl LocalInterfacePower {
    fn new() -> Self {
        Self {
            controls: Mutex::new(HashMap::new()),
        }
    }

    fn register(&self, control: LocalPowerControl) {
        let id = match &control {
            LocalPowerControl::Tokio(status) => status.id(),
            LocalPowerControl::Wifi(status) => status.id(),
            LocalPowerControl::Ble(status) => status.id(),
        };
        self.controls
            .lock()
            .expect("local interface power mutex poisoned")
            .insert(id, control);
    }

    fn set(&self, id: InterfaceId, power: InterfacePower) -> Result<(), BackendError> {
        let controls = self
            .controls
            .lock()
            .expect("local interface power mutex poisoned");
        let Some(control) = controls.get(&id) else {
            return Err(BackendError::Operation {
                operation: "set controller interface power",
                detail: "this controller does not own that interface".to_string(),
            });
        };
        match (control, power) {
            (LocalPowerControl::Tokio(status), InterfacePower::On) => status.enable(),
            (LocalPowerControl::Tokio(status), InterfacePower::Off) => status.disable(),
            (LocalPowerControl::Wifi(status), InterfacePower::On) => status.enable(),
            (LocalPowerControl::Wifi(status), InterfacePower::Off) => status.disable(),
            (LocalPowerControl::Ble(status), InterfacePower::On) => status.enable(),
            (LocalPowerControl::Ble(status), InterfacePower::Off) => status.disable(),
        }
        Ok(())
    }
}

impl InterfaceActivitySignature {
    fn observed_active(self) -> bool {
        matches!(self.connection, 1 | 2) || self.links > 0 || self.rate_bytes_per_sec > 0
    }
}

impl ControllerSession {
    fn start() -> Result<Self, BackendError> {
        let tcp = std::env::var("HOPSPOT_RC_TCP")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let start_tcp = tcp.is_some();
        let tcp_target = tcp.unwrap_or_else(|| DEFAULT_TCP_TARGET.to_string());
        let auto_wifi = env_enabled("HOPSPOT_RC_AUTO_WIFI");
        let start_ble = !env_started_off("HOPSPOT_RC_BLE");
        #[cfg(any(
            target_os = "android",
            target_os = "linux",
            target_os = "macos",
            target_os = "windows"
        ))]
        let start_usb = env_enabled("HOPSPOT_RC_USB");
        let local_power = Arc::new(LocalInterfacePower::new());
        let attach_power = local_power.clone();
        let attach_tcp = tcp_target.clone();

        let data_dir = controller_data_dir();
        std::fs::create_dir_all(&data_dir)
            .map_err(|error| BackendError::Startup(error.to_string()))?;
        let identities_dir = data_dir.join("identities");
        let ble_identity = load_or_create_ble_identity(&identities_dir.join("ble"))
            .map_err(|error| BackendError::Startup(error.to_string()))?;
        let (identity_secrets, _) = RemoteControlIdentityDirectory::new(&identities_dir)
            .load_or_generate()
            .map_err(|error| BackendError::Startup(error.to_string()))?
            .into_parts();
        let identities = identity_secrets.identities();
        let controller = identities.controller();
        let controller_secret = load_controller_secret(&identities_dir)?;
        let instance_secret = load_or_create_instance_secret(&identities_dir, &controller_secret)?;
        let instance_material =
            personal_rns::identity::PrivateIdentityMaterial::from_bytes(*instance_secret);
        let controller_identity = ControllerIdentity {
            operator_hash: encode_hex(controller.identity_hash().as_bytes()),
            allow_list_key: encode_hex(&controller.public_keys().public_key_bytes()),
            operator_secret_path: controller_identity_secret_path(&identities_dir)
                .display()
                .to_string(),
            instance_hash: encode_hex(instance_material.identity_hash().as_bytes()),
            instance_secret_path: instance_identity_secret_path(&identities_dir)
                .display()
                .to_string(),
        };
        let remote_control = RemoteControlService::new(
            identity_secrets,
            RemoteControlInitialControllerGrants::Nobody,
            RemoteControlSelfAnnouncement::Unavailable,
        );
        let pairing = Arc::new(Mutex::new(PairingEvents::load(
            data_dir.join(TARGET_NAMES_FILE),
        )));
        let clone = Arc::new(Mutex::new(IdentityCloneShared::default()));
        let clone_events = clone.clone();
        let persist_dir = data_dir.join("node");
        let peer_aliases_path = data_dir.join(PEER_ALIASES_FILE);
        let peer_aliases_map = load_target_names(&peer_aliases_path);
        let manager_aliases_path = data_dir.join(MANAGER_ALIASES_FILE);
        let manager_aliases_map = load_target_names(&manager_aliases_path);
        let target_aliases_path = data_dir.join(TARGET_ALIASES_FILE);
        let target_aliases_map = load_target_names(&target_aliases_path);
        let sibling_aliases_path = data_dir.join(SIBLING_ALIASES_FILE);
        let mut sibling_aliases_map = load_target_names(&sibling_aliases_path);
        let target_names = pairing
            .lock()
            .expect("pairing state mutex poisoned")
            .target_names
            .clone();
        let mut roster_state = RosterShared {
            replica: load_replica(&replica_path(&data_dir)),
            instance_secret: Some(personal_rns::identity::Zeroizing::new(*instance_secret)),
            ..RosterShared::default()
        };
        let instance_hex = encode_hex(instance_material.identity_hash().as_bytes());
        roster_state
            .replica
            .siblings
            .retain(|sibling| sibling.identity_hash() != instance_material.identity_hash());
        strip_local_sibling_alias(&mut roster_state.replica, &instance_hex);
        sibling_aliases_map.retain(|key, _| sibling_alias_is_syncable(key, &instance_hex));
        persist_target_names(&sibling_aliases_path, &sibling_aliases_map);
        import_seed_labels(
            &mut roster_state.replica,
            RosterLabelKind::TargetName,
            &target_names,
        );
        import_seed_labels(
            &mut roster_state.replica,
            RosterLabelKind::TargetAlias,
            &target_aliases_map,
        );
        import_seed_labels(
            &mut roster_state.replica,
            RosterLabelKind::ManagerAlias,
            &manager_aliases_map,
        );
        import_seed_labels(
            &mut roster_state.replica,
            RosterLabelKind::PeerAlias,
            &peer_aliases_map,
        );
        import_seed_labels(
            &mut roster_state.replica,
            RosterLabelKind::SiblingAlias,
            &sibling_aliases_map,
        );
        persist_replica(&replica_path(&data_dir), &roster_state.replica);
        let roster = Arc::new(Mutex::new(roster_state));
        let roster_events = roster.clone();
        let app_state = ControllerAppState {
            clone: clone.clone(),
            roster: roster.clone(),
        };
        let peer_aliases = Mutex::new(peer_aliases_map);
        let manager_aliases = Mutex::new(manager_aliases_map);
        let target_aliases = Mutex::new(target_aliases_map);
        let sibling_aliases = Mutex::new(sibling_aliases_map);
        let pairing_events = pairing.clone();
        let node = PrnsNode::new(PrnsNodeRecipe {
            transport_identity: None,
            remote_control,
            pre_configured_destinations: [
                PreConfiguredDestination::Single {
                    app_name: IDENTITY_CLONE_APP_NAME,
                    aspects: IDENTITY_CLONE_ASPECTS,
                    identity: personal_rns::identity::Zeroizing::new(*instance_secret),
                    announce_app_data: b"",
                    proof: ProofStrategy::ProveNone,
                    link_requests: LinkRequestPolicy::AcceptDirect,
                    ratchet: RatchetPolicy::NoRatchets,
                    resource_strategy: ResourceStrategy::AcceptNone,
                    maximum_request_bytes: Default::default(),
                    request_endpoints: ServeMyRequestEndpoints::Yes,
                },
                PreConfiguredDestination::Single {
                    app_name: ROSTER_SYNC_APP_NAME,
                    aspects: ROSTER_SYNC_ASPECTS,
                    identity: personal_rns::identity::Zeroizing::new(*instance_secret),
                    announce_app_data: b"",
                    proof: ProofStrategy::ProveNone,
                    link_requests: LinkRequestPolicy::AcceptAll,
                    ratchet: RatchetPolicy::NoRatchets,
                    resource_strategy: ResourceStrategy::AcceptNone,
                    maximum_request_bytes: Default::default(),
                    request_endpoints: ServeMyRequestEndpoints::Yes,
                },
            ],
            app_state,
            storage: GrowableHeap,
            request_endpoints: request_endpoints![IdentityClone, RosterSync],
            on_event: move |event, _state| match event {
                PrnsEvent::Diagnostic(Diagnostic::AnnounceHeard {
                    destination,
                    hops,
                    source_interface,
                    app_data,
                }) => {
                    if let Ok(mut shared) = roster_events.lock() {
                        for sibling in &shared.replica.siblings.clone() {
                            if Some(destination)
                                == roster_sync_destination_hash(sibling.identity_hash())
                            {
                                shared.note_heard_sibling(sibling.identity_hash());
                            }
                        }
                    }
                    if !clone_announce_is_usb_local(hops, source_interface) {
                        return;
                    }
                    let Some(announce) = IdentityCloneAnnounce::parse(app_data) else {
                        return;
                    };
                    let Some(expected) = identity_clone_destination_hash(announce.source()) else {
                        return;
                    };
                    if expected != destination {
                        return;
                    }
                    let Ok(mut shared) = clone_events.lock() else {
                        return;
                    };
                    if !shared.dest_waiting {
                        return;
                    }
                    shared.dest = Some(DestCloneSession {
                        announce,
                        destination,
                        dest_nonce: None,
                        transcript: None,
                        accepted: false,
                        hello_in_flight: false,
                    });
                    shared.error = None;
                    shared.notice = Some(
                        "Heard a USB sibling adoption. Confirm the six-digit adoption code, then Approve."
                            .to_string(),
                    );
                }
                PrnsEvent::Message(Message::RemoteControlPairingAvailable(observation)) => {
                    let id = encode_hex(observation.endpoint().destination_hash().as_bytes());
                    let label = String::from_utf8_lossy(observation.public_app_data().as_bytes())
                        .trim()
                        .to_owned();
                    let name = if label.is_empty() {
                        format!("Pairing {}", short_id(&id))
                    } else {
                        label
                    };
                    let remaining_ms = observation
                        .expires_at()
                        .0
                        .saturating_sub(observation.observed_at().0);
                    pairing_events
                        .lock()
                        .expect("pairing state mutex poisoned")
                        .announcements
                        .insert(
                            id,
                            HeardAnnouncement {
                                endpoint: observation.endpoint(),
                                expires_at: observation.expires_at(),
                                wall_expires: Instant::now() + Duration::from_millis(remaining_ms),
                                name,
                                hops: observation.hops().0,
                                interface: observation.source_interface(),
                                observed_at: observation.observed_at(),
                            },
                        );
                }
                PrnsEvent::Message(
                    Message::RemoteControlControllerPairingConfirmationRequired(confirmation),
                ) => {
                    let target_id = encode_hex(
                        confirmation
                            .confirmation()
                            .target()
                            .identity_hash()
                            .as_bytes(),
                    );
                    let pending = PendingPairing {
                        digits: confirmation.confirmation().confirmation_code().to_string(),
                        approval: confirmation.approval(),
                        rejection: confirmation.rejection(),
                    };
                    let mut pairing = pairing_events.lock().expect("pairing state mutex poisoned");
                    pairing.pending_target_id = Some(target_id.clone());
                    pairing.remember_active_name(&target_id);
                    pairing.set_pending(pending);
                }
                PrnsEvent::Message(_) | PrnsEvent::Diagnostic(_) => {}
            },
            interfaces: move |handle: &PrnsNodeHandle| {
                let client = TcpClientInterface::new(attach_tcp);
                let tcp_status = client.status();
                if !start_tcp {
                    tcp_status.disable();
                }
                attach_power.register(LocalPowerControl::Tokio(tcp_status));
                handle.attach(client);
                #[cfg(target_os = "macos")]
                let wifi = AutoWifi::default().with_host_discovery(apple_service_discovery());
                #[cfg(not(target_os = "macos"))]
                let wifi = AutoWifi::default();
                let wifi_status = wifi.status();
                if !auto_wifi {
                    wifi_status.disable();
                }
                attach_power.register(LocalPowerControl::Wifi(wifi_status.clone()));
                let attached = handle.supervise(wifi);
                if let Some(group) = RemoteControlInterfaceGroup::parse("reticulum") {
                    let _ = handle.set_interface_group(attached.id(), group);
                }
                let _ = handle.register_interface_group_apply(attached.id(), move |group| {
                    wifi_status.set_group_id(group)
                });

                #[cfg(not(target_os = "android"))]
                {
                    let attached_ble = handle.attach(AutoBle::new(ble_identity));
                    let ble_status = attached_ble.status();
                    if !start_ble {
                        ble_status.disable();
                    }
                    attach_power.register(LocalPowerControl::Ble(ble_status.clone()));
                    if let Some(group) = RemoteControlInterfaceGroup::parse("reticulum") {
                        let _ = handle.set_interface_group(attached_ble.id(), group);
                    }
                    let _ = handle
                        .register_interface_group_apply(attached_ble.id(), move |group| {
                            ble_status.set_group_id(group)
                        });
                }

                #[cfg(target_os = "android")]
                {
                    let platform = crate::android::platform();
                    platform.ble.set_local_identity(ble_identity);
                    let bluetooth = BluetoothAuto::<_, { AndroidBleBackend::MAX_PEERS }>::new(
                        AndroidBleBackend::new(platform.ble.clone()),
                        ble_identity,
                        Endpoint::Android(AndroidHost::Android),
                        LinkCapabilities {
                            l2cap: None,
                            link_mtu: BLE_HW_MTU as u16,
                        },
                    );
                    let ble_status = bluetooth.status();
                    if !start_ble {
                        ble_status.disable();
                    }
                    attach_power.register(LocalPowerControl::Ble(ble_status.clone()));
                    let attached_ble = handle.supervise(bluetooth);
                    if let Some(group) = RemoteControlInterfaceGroup::parse("reticulum") {
                        let _ = handle.set_interface_group(attached_ble.id(), group);
                    }
                    let _ = handle
                        .register_interface_group_apply(attached_ble.id(), move |group| {
                            ble_status.set_group_id(group)
                        });
                }

                #[cfg(target_os = "android")]
                {
                    let usb_bridge = crate::android::usb_bridge();
                    let scan = {
                        let bridge = usb_bridge.clone();
                        move || {
                            if bridge.is_connected() {
                                vec![UsbAutoCandidate::prns_specific(
                                    crate::android::android_usb_port(),
                                )]
                            } else {
                                Vec::new()
                            }
                        }
                    };
                    let open = {
                        let bridge = usb_bridge.clone();
                        move |_candidate: UsbAutoCandidate| {
                            let bridge = bridge.clone();
                            async move { Ok(bridge.open_stream()) }
                        }
                    };
                    let usb = UsbAutoHost::new(
                        InterfaceId::new([0xD1; 8]),
                        scan,
                        open,
                        usb_bridge.rescan(),
                    );
                    let usb_status = usb.status();
                    if !start_usb {
                        usb_status.disable();
                    }
                    attach_power.register(LocalPowerControl::Tokio(usb_status));
                    handle.add_interface(usb);
                }

                #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
                {
                    enable_controller_android_accessory_switch();
                    let rescan = Arc::new(Notify::new());
                    let usb = personal_rns::usb_auto::UsbAutoHost::new(
                        personal_rns::usb_auto::DEFAULT_USB_AUTO_ID,
                        personal_rns::usb_auto::scan_native_usb_auto_targets,
                        open_native_usb_auto_target_with_baud,
                        rescan,
                    );
                    let usb_status = usb.status();
                    if !start_usb {
                        usb_status.disable();
                    }
                    attach_power.register(LocalPowerControl::Tokio(usb_status));
                    handle.add_interface(usb);
                }
            },
            persistence: NodePersistence::custom_dir(data_dir.join("node"))
                .map_err(|error| BackendError::Startup(error.to_string()))?,
        });
        let handle = node.handle();
        spawn(async move {
            if let Err(error) = node.run().await {
                eprintln!("remote-control controller node stopped: {error}");
            }
        });
        Ok(Self {
            handle,
            pairing,
            local_power,
            tcp_target: Some(tcp_target),
            controller_identity,
            activity: Mutex::new(HashMap::new()),
            peer_aliases,
            peer_aliases_path,
            manager_aliases,
            manager_aliases_path,
            target_aliases,
            target_aliases_path,
            sibling_aliases,
            sibling_aliases_path,
            build_versions: Mutex::new(HashMap::new()),
            clone,
            roster,
            ble_identity,
            identities_dir,
            persist_dir,
            data_dir,
            last_roster_sync: Mutex::new(None),
        })
    }

    fn cached_build_version(&self, target_id: &str) -> Option<String> {
        self.build_versions
            .lock()
            .expect("build versions mutex poisoned")
            .get(target_id)
            .cloned()
    }

    fn remember_build_version(&self, target_id: &str, version: String) {
        self.build_versions
            .lock()
            .expect("build versions mutex poisoned")
            .insert(target_id.to_owned(), version);
    }

    fn forget_build_version(&self, target_id: &str) {
        self.build_versions
            .lock()
            .expect("build versions mutex poisoned")
            .remove(target_id);
    }

    fn stamp_activity(&self, scope: &str, items: &mut [InterfaceEntry]) {
        let mut tracker = self
            .activity
            .lock()
            .expect("interface activity mutex poisoned");
        let now = Instant::now();
        for item in items {
            let key = format!("{scope}:{}", item.id);
            let signature = InterfaceActivitySignature {
                connection: activity_connection_code(&item.connection),
                tx_bytes: item.tx_bytes,
                rx_bytes: item.rx_bytes,
                links: item.links,
                destinations: item.destinations,
                rate_bytes_per_sec: item.rate_bytes_per_sec,
                radio: RadioIndication::NotRadio,
            };
            let last_at = match tracker.get_mut(&key) {
                Some(stamp) => {
                    if stamp.signature != signature {
                        stamp.signature = signature;
                        stamp.last_at = Some(now);
                    }
                    stamp.last_at
                }
                None => {
                    let last_at = signature.observed_active().then_some(now);
                    tracker.insert(key, InterfaceActivityStamp { signature, last_at });
                    last_at
                }
            };
            item.last_activity_secs =
                last_at.map(|then| now.saturating_duration_since(then).as_secs() as u32);
            for peer in &mut item.peers {
                let peer_key = format!("{scope}:{}:{}", item.id, peer.id);
                let signature = InterfaceActivitySignature {
                    connection: activity_connection_code(&peer.connection),
                    tx_bytes: peer.tx_bytes,
                    rx_bytes: peer.rx_bytes,
                    links: peer.links,
                    destinations: peer.destinations,
                    rate_bytes_per_sec: peer.rate_bytes_per_sec,
                    radio: peer.radio,
                };
                let last_at = match tracker.get_mut(&peer_key) {
                    Some(stamp) => {
                        if stamp.signature != signature {
                            stamp.signature = signature;
                            stamp.last_at = Some(now);
                        }
                        stamp.last_at
                    }
                    None => {
                        let last_at = signature.observed_active().then_some(now);
                        tracker.insert(peer_key, InterfaceActivityStamp { signature, last_at });
                        last_at
                    }
                };
                peer.last_activity_secs =
                    last_at.map(|then| now.saturating_duration_since(then).as_secs() as u32);
            }
        }
    }

    fn mark_reachable(&self, target_id: &str, status: TargetStatus) {
        self.pairing
            .lock()
            .expect("pairing state mutex poisoned")
            .set_reachability(target_id, status);
    }
}

struct PairingEvents {
    announcements: HashMap<String, HeardAnnouncement>,
    /// Announcement ids kept visible while a pairing attempt is in progress.
    pinned: HashMap<String, HeardAnnouncement>,
    active_announcement_id: Option<String>,
    pending: Option<PendingPairing>,
    pending_target_id: Option<String>,
    target_names: HashMap<String, String>,
    reachability: HashMap<String, TargetStatus>,
    pending_announce_name: Option<String>,
    names_path: PathBuf,
    control_announce_baseline: HashMap<String, InstantMillis>,
}

impl PairingEvents {
    fn load(names_path: PathBuf) -> Self {
        let target_names = load_target_names(&names_path);
        Self {
            announcements: HashMap::new(),
            pinned: HashMap::new(),
            active_announcement_id: None,
            pending: None,
            pending_target_id: None,
            target_names,
            reachability: HashMap::new(),
            pending_announce_name: None,
            names_path,
            control_announce_baseline: HashMap::new(),
        }
    }

    fn paired_announce_name(&self, id: &str) -> Option<String> {
        stored_alias(self.target_names.get(id).map(String::as_str))
    }

    fn set_announce_name(&mut self, target_id: &str, name: &str) -> bool {
        let id = target_id.trim();
        if id.is_empty() {
            return false;
        }
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let changed = self.target_names.get(id).map(String::as_str) != Some(name);
        self.target_names.insert(id.to_owned(), name.to_owned());
        persist_target_names(&self.names_path, &self.target_names);
        changed
    }

    fn active_announcement_name(&self) -> Option<String> {
        let id = self.active_announcement_id.as_ref()?;
        self.announcements
            .get(id)
            .or_else(|| self.pinned.get(id))
            .map(|announcement| announcement.name.clone())
    }

    fn remember_active_name(&mut self, target_id: &str) {
        if let Some(name) = self.active_announcement_name() {
            self.set_announce_name(target_id, &name);
            self.pending_announce_name = None;
        }
    }

    fn remember_unnamed_targets(
        &mut self,
        targets: &[AuthorizedRemoteControlTarget],
    ) -> Option<(String, String)> {
        let pending = self.pending_announce_name.clone();
        self.remember_unnamed_targets_with(targets, pending)
    }

    fn remember_unnamed_targets_with(
        &mut self,
        targets: &[AuthorizedRemoteControlTarget],
        name: Option<String>,
    ) -> Option<(String, String)> {
        let unnamed = targets
            .iter()
            .map(|target| encode_hex(target.identity_hash().as_bytes()))
            .filter(|id| !self.target_names.contains_key(id))
            .collect::<Vec<_>>();
        let name = name.filter(|name| !name.is_empty())?;
        if unnamed.len() == 1 {
            if let Some(id) = unnamed.into_iter().next() {
                if self.set_announce_name(&id, &name) {
                    self.pending_announce_name = None;
                    return Some((id, name));
                }
                self.pending_announce_name = None;
            }
        }
        None
    }

    fn reachability(&self, id: &str) -> TargetStatus {
        self.reachability
            .get(id)
            .copied()
            .unwrap_or(TargetStatus::Offline)
    }

    fn set_reachability(&mut self, id: &str, status: TargetStatus) {
        self.reachability.insert(id.to_owned(), status);
    }

    fn forget_stored(&mut self, target_id: &str) {
        self.target_names.remove(target_id);
        self.reachability.remove(target_id);
        persist_target_names(&self.names_path, &self.target_names);
    }

    fn clear_announce_name(&mut self, target_id: &str) {
        if self.target_names.remove(target_id).is_some() {
            persist_target_names(&self.names_path, &self.target_names);
        }
    }

    fn prune_expired(&mut self) {
        let now = Instant::now();
        let keep = self
            .pending
            .is_some()
            .then(|| self.active_announcement_id.clone())
            .flatten();
        self.announcements.retain(|id, announcement| {
            announcement.wall_expires > now || keep.as_ref() == Some(id)
        });
        self.pinned.retain(|id, announcement| {
            announcement.wall_expires > now || keep.as_ref() == Some(id)
        });
    }

    fn pin_announcement(&mut self, announcement_id: &str) {
        if let Some(announcement) = self.announcements.get(announcement_id).cloned() {
            self.pinned.insert(announcement_id.to_owned(), announcement);
        }
    }

    #[allow(dead_code)]
    fn forget_announcement(&mut self, announcement_id: &str) {
        self.announcements.remove(announcement_id);
        self.pinned.remove(announcement_id);
        if self.active_announcement_id.as_deref() == Some(announcement_id) {
            self.active_announcement_id = None;
        }
    }

    fn finish_pairing_announcement(&mut self) {
        if let Some(id) = self.active_announcement_id.take() {
            self.announcements.remove(&id);
            self.pinned.remove(&id);
        }
        self.pending_target_id = None;
    }

    fn dismiss_heard_advert(&mut self, announcement_id: &str) {
        let id = announcement_id.trim();
        if id.is_empty() {
            return;
        }
        if self.active_announcement_id.as_deref() == Some(id) || self.pinned.contains_key(id) {
            return;
        }
        self.announcements.remove(id);
    }

    fn set_pending(&mut self, pending: PendingPairing) {
        self.pending = Some(pending);
    }

    fn active_announcements(&mut self) -> Vec<AnnouncementSummary> {
        self.prune_expired();
        let mut items = HashMap::new();
        for (id, announcement) in self.announcements.iter().chain(self.pinned.iter()) {
            items.insert(
                id.clone(),
                (
                    announcement.observed_at,
                    AnnouncementSummary {
                        id: id.clone(),
                        name: announcement.name.clone(),
                        path: Some(path_from_announce(
                            announcement.hops,
                            announcement.interface,
                            announcement.observed_at,
                        )),
                    },
                ),
            );
        }
        let mut items: Vec<(InstantMillis, AnnouncementSummary)> = items.into_values().collect();
        items.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.id.cmp(&right.1.id)));
        items.into_iter().map(|(_, item)| item).collect()
    }

    fn announcement(
        &mut self,
        announcement_id: &str,
    ) -> Option<(RemoteControlPairingEndpoint, InstantMillis)> {
        self.prune_expired();
        let announcement = self
            .announcements
            .get(announcement_id)
            .or_else(|| self.pinned.get(announcement_id))?;
        self.active_announcement_id = Some(announcement_id.to_owned());
        Some((announcement.endpoint, announcement.expires_at))
    }
}

#[derive(Clone)]
struct HeardAnnouncement {
    endpoint: RemoteControlPairingEndpoint,
    expires_at: InstantMillis,
    wall_expires: Instant,
    name: String,
    hops: u8,
    interface: InterfaceId,
    observed_at: InstantMillis,
}

struct AnnouncementSummary {
    id: String,
    name: String,
    path: Option<TargetPath>,
}

struct PendingPairing {
    digits: String,
    approval: ApproveRemoteControlControllerPairing,
    rejection: RejectRemoteControlControllerPairing,
}

#[derive(Clone, Copy)]
enum RadioAction {
    Sleep,
    Wake,
}

impl RadioAction {
    const fn operation(self) -> &'static str {
        match self {
            Self::Sleep => "sleep target radios",
            Self::Wake => "wake target radios",
        }
    }
}

fn load_controller_secret(
    identities_dir: &Path,
) -> Result<
    personal_rns::identity::Zeroizing<[u8; personal_rns::IDENTITY_SECRET_KEY_LEN]>,
    BackendError,
> {
    let vault = personal_rns::identity::vault::FileVault::new(identities_dir.to_path_buf());
    let label = personal_rns::identity::vault::IdentityLabel::new(CONTROLLER_IDENTITY_FILE)
        .map_err(|error| BackendError::Startup(format!("{error:?}")))?;
    personal_rns::identity::vault::IdentityVault::load(&vault, &label)
        .map_err(|error| BackendError::Startup(error.to_string()))?
        .ok_or_else(|| BackendError::Startup("controller identity is missing".to_string()))
}

fn adopt_cloned_identity(
    identities_dir: &Path,
    persist_dir: &Path,
    secret: &[u8; personal_rns::IDENTITY_SECRET_KEY_LEN],
    accesses: &[u8],
) -> Result<(), BackendError> {
    let mut vault = personal_rns::identity::vault::FileVault::new(identities_dir.to_path_buf());
    let label = personal_rns::identity::vault::IdentityLabel::new(CONTROLLER_IDENTITY_FILE)
        .map_err(|error| BackendError::Clone(format!("{error:?}")))?;
    let stored = personal_rns::identity::Zeroizing::new(*secret);
    personal_rns::identity::vault::IdentityVault::store(&mut vault, &label, &stored)
        .map_err(|error| BackendError::Clone(error.to_string()))?;
    if !accesses.is_empty() {
        let mut store = personal_rns::persistence::FileStore::new(persist_dir.to_path_buf());
        personal_rns::persistence::PersistedStore::store(
            &mut store,
            personal_rns::persistence::SnapshotRegion::RemoteControlTargetAccesses,
            accesses,
        )
        .map_err(|error| BackendError::Clone(error.to_string()))?;
    }
    Ok(())
}

fn controller_identity_secret_path(identities_dir: impl AsRef<Path>) -> PathBuf {
    identities_dir.as_ref().join(CONTROLLER_IDENTITY_FILE)
}

fn instance_identity_secret_path(identities_dir: impl AsRef<Path>) -> PathBuf {
    identities_dir.as_ref().join(INSTANCE_IDENTITY_FILE)
}

fn load_instance_secret(
    identities_dir: &Path,
) -> Result<
    personal_rns::identity::Zeroizing<[u8; personal_rns::IDENTITY_SECRET_KEY_LEN]>,
    BackendError,
> {
    personal_rns::load_or_create_identity_secret(&instance_identity_secret_path(identities_dir))
        .map_err(|error| BackendError::Startup(error.to_string()))
}

fn load_or_create_instance_secret(
    identities_dir: &Path,
    operator: &[u8; personal_rns::IDENTITY_SECRET_KEY_LEN],
) -> Result<
    personal_rns::identity::Zeroizing<[u8; personal_rns::IDENTITY_SECRET_KEY_LEN]>,
    BackendError,
> {
    let path = instance_identity_secret_path(identities_dir);
    let secret = personal_rns::load_or_create_identity_secret(&path)
        .map_err(|error| BackendError::Startup(error.to_string()))?;
    if *secret != *operator {
        return Ok(secret);
    }
    let fresh = personal_rns::try_generate_identity_secret()
        .map_err(|error| BackendError::Startup(error.to_string()))?;
    std::fs::write(&path, &*fresh).map_err(|error| BackendError::Startup(error.to_string()))?;
    Ok(fresh)
}

fn persist_session_replica(session: &ControllerSession) {
    if let Ok(mut roster) = session.roster.lock() {
        strip_local_sibling_alias(
            &mut roster.replica,
            &session.controller_identity.instance_hash,
        );
        persist_replica(&replica_path(&session.data_dir), &roster.replica);
    }
}

fn remember_sibling_alias(session: &ControllerSession, instance_hash: &str, alias: &str) {
    let id = instance_hash.trim();
    if id.is_empty() || !sibling_alias_is_syncable(id, &session.controller_identity.instance_hash) {
        return;
    }
    let name = {
        let aliases = session
            .sibling_aliases
            .lock()
            .expect("sibling aliases mutex poisoned");
        let trimmed = alias.trim();
        if trimmed.is_empty() {
            let mut considered = aliases.clone();
            considered.remove(id);
            next_sibling_alias(&considered)
        } else {
            trimmed.to_owned()
        }
    };
    {
        let mut aliases = session
            .sibling_aliases
            .lock()
            .expect("sibling aliases mutex poisoned");
        aliases.insert(id.to_owned(), name.clone());
        persist_target_names(&session.sibling_aliases_path, &aliases);
    }
    if let Ok(mut roster) = session.roster.lock() {
        note_local_label(
            &mut roster.replica,
            RosterLabelKind::SiblingAlias,
            id,
            Some(&name),
        );
    }
    persist_session_replica(session);
}

fn ensure_missing_sibling_aliases<'a>(
    session: &ControllerSession,
    hashes: impl IntoIterator<Item = &'a str>,
) {
    let mut aliases = session
        .sibling_aliases
        .lock()
        .expect("sibling aliases mutex poisoned");
    let mut changed = false;
    for hash in hashes {
        let hash = hash.trim();
        if hash.is_empty()
            || !sibling_alias_is_syncable(hash, &session.controller_identity.instance_hash)
        {
            continue;
        }
        if aliases
            .get(hash)
            .is_some_and(|name| !name.trim().is_empty())
        {
            continue;
        }
        let next = next_sibling_alias(&aliases);
        aliases.insert(hash.to_owned(), next);
        changed = true;
    }
    if !changed {
        return;
    }
    persist_target_names(&session.sibling_aliases_path, &aliases);
    drop(aliases);
    if let Ok(mut roster) = session.roster.lock() {
        let aliases = session
            .sibling_aliases
            .lock()
            .expect("sibling aliases mutex poisoned");
        import_seed_labels(&mut roster.replica, RosterLabelKind::SiblingAlias, &aliases);
    }
    persist_session_replica(session);
}

fn remove_alias_map_key(aliases: &Mutex<HashMap<String, String>>, path: &PathBuf, key: &str) {
    let mut aliases = aliases.lock().expect("alias map mutex poisoned");
    if aliases.remove(key).is_some() {
        persist_target_names(path, &aliases);
    }
}

fn apply_map_label(
    aliases: &Mutex<HashMap<String, String>>,
    path: &PathBuf,
    key: &str,
    value: Option<&str>,
) {
    let mut aliases = aliases.lock().expect("alias map mutex poisoned");
    match value {
        Some(name) if !name.is_empty() => {
            aliases.insert(key.to_owned(), name.to_owned());
        }
        _ => {
            aliases.remove(key);
        }
    }
    persist_target_names(path, &aliases);
}

fn apply_roster_labels(session: &ControllerSession, labels: &[RosterLabel]) {
    for label in labels {
        match label.kind {
            RosterLabelKind::TargetName => {
                let mut pairing = session
                    .pairing
                    .lock()
                    .expect("pairing state mutex poisoned");
                match &label.value {
                    Some(name) if !name.is_empty() => {
                        pairing.set_announce_name(&label.key, name);
                    }
                    _ => {
                        pairing.clear_announce_name(&label.key);
                    }
                }
            }
            RosterLabelKind::TargetAlias => apply_map_label(
                &session.target_aliases,
                &session.target_aliases_path,
                &label.key,
                label.value.as_deref(),
            ),
            RosterLabelKind::ManagerAlias => apply_map_label(
                &session.manager_aliases,
                &session.manager_aliases_path,
                &label.key,
                label.value.as_deref(),
            ),
            RosterLabelKind::PeerAlias => {
                if peer_alias_is_syncable(&label.key) {
                    apply_map_label(
                        &session.peer_aliases,
                        &session.peer_aliases_path,
                        &label.key,
                        label.value.as_deref(),
                    );
                }
            }
            RosterLabelKind::SiblingAlias => {
                if sibling_alias_is_syncable(&label.key, &session.controller_identity.instance_hash)
                {
                    apply_map_label(
                        &session.sibling_aliases,
                        &session.sibling_aliases_path,
                        &label.key,
                        label.value.as_deref(),
                    );
                }
            }
            RosterLabelKind::SiblingRemoved => {
                if label
                    .value
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                {
                    if let Ok(bytes) = parse_hex::<IDENTITY_HASH_BYTES>(&label.key) {
                        if let Ok(mut roster) = session.roster.lock() {
                            roster.replica.siblings.retain(|sibling| {
                                sibling.identity_hash() != IdentityHash::new(bytes)
                            });
                        }
                    }
                    remove_alias_map_key(
                        &session.sibling_aliases,
                        &session.sibling_aliases_path,
                        &label.key,
                    );
                }
            }
            RosterLabelKind::PairingAdvertDismissed => {
                if label
                    .value
                    .as_deref()
                    .is_some_and(|value| !value.is_empty())
                {
                    session
                        .pairing
                        .lock()
                        .expect("pairing state mutex poisoned")
                        .dismiss_heard_advert(&label.key);
                }
            }
        }
    }
}

fn controller_data_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("HOPSPOT_RC_DATA_DIR") {
        return PathBuf::from(path);
    }
    #[cfg(target_os = "android")]
    {
        return android_files_dir().unwrap_or_else(std::env::temp_dir);
    }
    #[cfg(not(target_os = "android"))]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(DEFAULT_DATA_DIRECTORY)
    }
}

#[cfg(target_os = "android")]
fn android_files_dir() -> Option<PathBuf> {
    use jni::objects::{JObject, JString};
    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.ok()?;
    let mut env = vm.attach_current_thread().ok()?;
    let context = unsafe { JObject::from_raw(ctx.context().cast()) };
    let file = env
        .call_method(&context, "getFilesDir", "()Ljava/io/File;", &[])
        .ok()?
        .l()
        .ok()?;
    let path = env
        .call_method(file, "getAbsolutePath", "()Ljava/lang/String;", &[])
        .ok()?
        .l()
        .ok()?;
    let path: String = env.get_string(&JString::from(path)).ok()?.into();
    Some(PathBuf::from(path).join("hopspot-remote-control"))
}

fn env_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn env_started_off(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
        .unwrap_or(false)
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn enable_controller_android_accessory_switch() {
    // Native scan only switches a Pixel into AOA when this is present.
    // Controller-to-Controller USB is that phone-as-device path.
    if std::env::var_os("PRNS_USB_AUTO_ANDROID_ACCESSORY").is_none() {
        std::env::set_var("PRNS_USB_AUTO_ANDROID_ACCESSORY", "1");
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
async fn open_native_usb_auto_target_with_baud(
    candidate: personal_rns::usb_auto::UsbAutoCandidate,
) -> std::io::Result<personal_rns::usb_auto::NativeUsbAutoStream> {
    personal_rns::usb_auto::open_native_usb_auto_target(
        candidate,
        personal_rns::usb_auto::DEFAULT_USB_BAUD,
    )
    .await
}

fn parse_invitation_code(code: &str) -> Result<RemoteControlPairingInvitationCode, BackendError> {
    let normalized: String = code
        .chars()
        .filter(|character| *character != '-' && !character.is_whitespace())
        .collect();
    if normalized.len() != 8
        || !normalized
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(BackendError::InvalidInvitationCode);
    }
    u32::from_str_radix(&normalized, 16)
        .map(RemoteControlPairingInvitationCode::from_value)
        .map_err(|_| BackendError::InvalidInvitationCode)
}

fn begin_pairing_error(error: InitiateRemoteControlControllerPairingError) -> BackendError {
    match error {
        InitiateRemoteControlControllerPairingError::Request { failure, .. }
            if matches!(
                failure.cause,
                personal_rns::engine::RemoteControlControllerPairingRequestFailureCause::Request(
                    personal_rns::engine::SendRequestFailure::Timeout
                )
            ) =>
        {
            BackendError::PairingOfferTimeout
        }
        error => operation("begin controller pairing", error),
    }
}

fn parse_hex<const N: usize>(input: &str) -> Result<[u8; N], ()> {
    let input = input.trim();
    if input.len() != N.saturating_mul(2) {
        return Err(());
    }
    let mut output = [0u8; N];
    for (index, byte) in output.iter_mut().enumerate() {
        let start = index.saturating_mul(2);
        *byte = u8::from_str_radix(&input[start..start + 2], 16).map_err(|_| ())?;
    }
    Ok(output)
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn resolve_paired_target_hash(
    hashes: &[IdentityHash],
    pending_id: Option<&str>,
) -> Option<IdentityHash> {
    if let Some(pending_id) = pending_id {
        let pending_id = pending_id.trim();
        if pending_id.is_empty() {
            return None;
        }
        return hashes
            .iter()
            .copied()
            .find(|hash| encode_hex(hash.as_bytes()).eq_ignore_ascii_case(pending_id));
    }
    (hashes.len() == 1).then_some(hashes[0])
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub fn target_label(id: &str) -> String {
    format!("T {}", short_id(id))
}

fn operator_interface_kind(kind: InterfaceKind) -> bool {
    kind.supervisor_kind().is_none()
}

fn operator_local_interface(
    entry: &personal_rns::node_introspection::InterfaceInventoryEntry,
) -> bool {
    operator_local_kind(&entry.snapshot).is_some()
}

/// This app's USB Auto host id is `[0xD0; 8]`, a cookie, not `kind ++ hash`.
/// Settings still reports it as `UsbAutoHost`.
///
/// Any independent supervisor whose first id byte is not a known
/// `InterfaceKind` gets that same stamp. Fleet members stay omitted; a
/// decodable member kind is dropped, not relabeled USB.
/// Ingress stores a direct USB announce as hop 1 (`header.hops + 1`).
/// Hop 0 is kept for any path that does not increment.
fn clone_announce_is_usb_local(hops: u8, source_interface: InterfaceId) -> bool {
    hops <= 1
        && matches!(
            source_interface.kind(),
            Some(InterfaceKind::UsbAutoHost | InterfaceKind::UsbAutoDevice) | None
        )
}

fn operator_local_kind(snapshot: &InterfaceSnapshot) -> Option<InterfaceKind> {
    if matches!(snapshot.membership, Membership::FleetMember { .. }) {
        return None;
    }
    match snapshot.id.kind() {
        Some(kind) if operator_interface_kind(kind) => Some(kind),
        None => Some(InterfaceKind::UsbAutoHost),
        Some(_) => None,
    }
}

fn operator_remote_kind(id: InterfaceId) -> Option<InterfaceKind> {
    match id.kind() {
        Some(kind) if operator_interface_kind(kind) => Some(kind),
        None => Some(InterfaceKind::UsbAutoDevice),
        Some(_) => None,
    }
}

/// USB Auto never publishes fleet-member peers. When the host handshake
/// is up, Settings still needs one row so clone and the accordion agree.
fn attach_usb_link_peer(entry: &mut InterfaceEntry) {
    if !entry.peers.is_empty() || !usb_supervisor_link_is_up(entry) {
        return;
    }
    entry.shows_peers = true;
    entry.peers.push(InterfacePeer {
        id: format!("{}:link", entry.id),
        name: "USB link".to_string(),
        alias: None,
        connection: entry.connection.clone(),
        tx_bytes: entry.tx_bytes,
        rx_bytes: entry.rx_bytes,
        links: entry.links,
        destinations: entry.destinations,
        rate_bytes_per_sec: entry.rate_bytes_per_sec,
        last_activity_secs: entry.last_activity_secs,
        radio: RadioIndication::NotRadio,
    });
}

fn local_interface_entry(
    snapshot: &InterfaceSnapshot,
    name: Option<&str>,
    ble_identity: Option<BleIdentity>,
    group: Option<&str>,
    ifac_bytes: Option<usize>,
    detail: Option<&str>,
    failure: Option<&str>,
    drops: Option<(u32, u32)>,
    peers: Vec<InterfacePeer>,
) -> InterfaceEntry {
    let id = encode_hex(snapshot.id.as_bytes());
    let kind = operator_local_kind(snapshot);
    let kind_name = kind
        .map(InterfaceKind::name)
        .unwrap_or("unknown")
        .to_string();
    let label = ble_identity
        .filter(|_| kind == Some(InterfaceKind::BluetoothAuto))
        .map(bluetooth_auto_title)
        .or_else(|| {
            name.map(str::trim)
                .filter(|name| !name.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| format!("{} {}", kind_name, short_id(&id)));
    let extras = hopspot_extra_facts(detail, kind, drops);
    InterfaceEntry {
        name: label,
        id,
        kind: kind_name,
        power: interface_power_from_connection(snapshot.connection),
        mode: snapshot.mode,
        connection: connection_label(kind, snapshot.connection),
        group: group
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        tx_bytes: snapshot.tx_bytes,
        rx_bytes: snapshot.rx_bytes,
        tx_bps: snapshot.transfer_rates.map(|rates| rates.tx_bps),
        rx_bps: snapshot.transfer_rates.map(|rates| rates.rx_bps),
        links: snapshot.links,
        transported_links: snapshot.transported_links,
        destinations: snapshot.destinations,
        rate_bytes_per_sec: snapshot
            .transfer_rates
            .map(|rates| rates.rx_bps.saturating_add(rates.tx_bps) / 8)
            .unwrap_or(0),
        gravity: (snapshot.gravity.get() != 0).then_some(snapshot.gravity.get()),
        ifac_bytes,
        last_activity_secs: None,
        detail: detail
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        failure: failure
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned),
        extras,
        shows_peers: kind.is_some_and(|kind| kind.member_kind().is_some()) || !peers.is_empty(),
        peers,
    }
}

async fn fetch_interface_config(
    remote: &RemoteControlTargetHandle<'_>,
    entry: &mut InterfaceEntry,
) {
    let Ok(id) = parse_hex::<INTERFACE_ID_BYTES>(&entry.id) else {
        return;
    };
    match remote
        .inventory_interface_config(InterfaceId::new(id))
        .await
    {
        Ok((RemoteControlInterfaceConfigOutcome::Card(card), _)) => {
            apply_remote_card(entry, &card);
        }
        Ok((RemoteControlInterfaceConfigOutcome::UnknownInterface, _)) | Err(_) => {}
    }
}

fn apply_remote_card(entry: &mut InterfaceEntry, card: &RemoteControlInterfaceCard) {
    let card_name = card.name.as_str().trim();
    if !card_name.is_empty() {
        entry.name = card_name.to_owned();
    }
    entry.group = Some(card.group.as_str().trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    entry.detail = Some(card.config.as_str().trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    entry.failure = Some(card.failure.as_str().trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    entry.destinations = card.destinations;
    entry.transported_links = card.transported_links;
    let kind = parse_hex::<INTERFACE_ID_BYTES>(&entry.id)
        .ok()
        .and_then(|bytes| operator_remote_kind(InterfaceId::new(bytes)));
    entry.extras = hopspot_extra_facts(entry.detail.as_deref(), kind, None);
}

async fn fetch_interface_peers(
    remote: &RemoteControlTargetHandle<'_>,
    id: InterfaceId,
) -> Vec<InterfacePeer> {
    let mut peers = Vec::new();
    let mut offset = 0u8;
    loop {
        match remote.inventory_interface_peers(id, offset).await {
            Ok((RemoteControlInterfacePeersOutcome::Page(page), _)) => {
                let received = u8::try_from(page.peers.len()).unwrap_or(u8::MAX);
                for peer in page.peers {
                    peers.push(interface_peer_from_wire(&peer));
                }
                let Some(next) = offset.checked_add(received) else {
                    break;
                };
                if received == 0 || next >= page.total {
                    break;
                }
                offset = next;
            }
            Ok((RemoteControlInterfacePeersOutcome::UnknownInterface, _)) | Err(_) => break,
        }
    }
    peers
}

fn interface_peer_from_wire(peer: &RemoteControlInterfacePeer) -> InterfacePeer {
    InterfacePeer {
        id: encode_hex(peer.id.as_bytes()),
        name: peer_label(peer.id),
        alias: None,
        connection: connection_label(peer.id.kind(), peer.connection),
        tx_bytes: peer.tx_bytes,
        rx_bytes: peer.rx_bytes,
        links: peer.links,
        destinations: peer.destinations,
        rate_bytes_per_sec: peer.rate_bytes_per_sec,
        last_activity_secs: None,
        radio: peer.radio,
    }
}

fn remote_interface_entry(
    entry: &RemoteControlInterfaceEntry,
    card: Option<&RemoteControlInterfaceCard>,
) -> InterfaceEntry {
    let id = encode_hex(entry.id.as_bytes());
    let kind = entry.kind.name().to_string();
    let card_name = card
        .map(|card| card.name.as_str().trim())
        .filter(|name| !name.is_empty());
    let name = card_name
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("{} {}", kind, short_id(&id)));
    let group = card
        .map(|card| card.group.as_str().trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let detail = card
        .map(|card| card.config.as_str().trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let failure = card
        .map(|card| card.failure.as_str().trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let peers: Vec<InterfacePeer> = card
        .map(|card| card.peers.iter().map(interface_peer_from_wire).collect())
        .unwrap_or_default();
    let extras = hopspot_extra_facts(detail.as_deref(), Some(entry.kind), None);
    InterfaceEntry {
        name,
        id,
        kind,
        power: if entry.enabled {
            InterfacePower::On
        } else {
            InterfacePower::Off
        },
        mode: entry.mode,
        connection: connection_label(Some(entry.kind), entry.connection),
        group,
        tx_bytes: entry.tx_bytes,
        rx_bytes: entry.rx_bytes,
        tx_bps: None,
        rx_bps: None,
        links: entry.links,
        transported_links: card.map(|card| card.transported_links).unwrap_or(0),
        destinations: card.map(|card| card.destinations).unwrap_or(0),
        rate_bytes_per_sec: entry.rate_bytes_per_sec,
        gravity: None,
        ifac_bytes: None,
        last_activity_secs: None,
        detail,
        failure,
        extras,
        shows_peers: entry.kind.member_kind().is_some() || !peers.is_empty(),
        peers,
    }
}

fn local_interface_config(
    snapshot: &InterfaceSnapshot,
    ifac_bytes: Option<usize>,
    tcp_target: Option<&str>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(size) = ifac_bytes {
        parts.push(format!("IFAC {size}"));
    }
    match operator_local_kind(snapshot) {
        Some(InterfaceKind::TcpClient) => {
            if let Some(target) = tcp_target.map(str::trim).filter(|value| !value.is_empty()) {
                parts.push(target.to_string());
            } else {
                parts.push("TCP dial".to_string());
            }
        }
        Some(InterfaceKind::AutoWifi) => parts.push("Auto discovery".to_string()),
        Some(InterfaceKind::LocalServer) => parts.push("Shared instance".to_string()),
        Some(InterfaceKind::TcpServer) => parts.push("TCP listen".to_string()),
        Some(InterfaceKind::BluetoothAuto) => parts.push("BLE supervisor".to_string()),
        Some(InterfaceKind::UsbAutoHost | InterfaceKind::UsbAutoDevice) => {
            parts.push("USB".to_string())
        }
        Some(InterfaceKind::LoRa | InterfaceKind::Rnode) => parts.push("LoRa".to_string()),
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
            | InterfaceKind::BackboneServer
            | InterfaceKind::BackboneServerPeer
            | InterfaceKind::BackboneClient
            | InterfaceKind::EspNow
            | InterfaceKind::WebSocketClient
            | InterfaceKind::WebSocketServer
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
        | None => {}
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn interface_peer(snapshot: &InterfaceSnapshot) -> InterfacePeer {
    InterfacePeer {
        id: encode_hex(snapshot.id.as_bytes()),
        name: peer_label(snapshot.id),
        alias: None,
        connection: connection_label(snapshot.id.kind(), snapshot.connection),
        tx_bytes: snapshot.tx_bytes,
        rx_bytes: snapshot.rx_bytes,
        links: snapshot.links,
        destinations: snapshot.destinations,
        rate_bytes_per_sec: snapshot
            .transfer_rates
            .map(|rates| rates.rx_bps.saturating_add(rates.tx_bps) / 8)
            .unwrap_or(0),
        last_activity_secs: None,
        radio: snapshot.radio,
    }
}

/// Short display label from the first two hash bytes of an `InterfaceId`.
/// The full id is `kind ++ SHA-256(channel_tag)[:7]`. BLE tags the persisted
/// BleIdentity; that id (and this prefix) come back the same after a reboot
/// when the peer identity is unchanged. Aliases key on the full hex id.
fn peer_label(id: InterfaceId) -> String {
    format!("P {}", appearance_prefix(id))
}

fn appearance_prefix(id: InterfaceId) -> String {
    let bytes = id.as_bytes();
    match (bytes.get(1), bytes.get(2)) {
        (Some(first), Some(second)) => format!("{first:02x}{second:02x}"),
        (Some(_) | None, Some(_) | None) => "----".to_string(),
    }
}

fn bluetooth_auto_title(identity: BleIdentity) -> String {
    let id = InterfaceId::from_channel_tag(InterfaceKind::BluetoothPeer, identity.as_bytes());
    format!("bluetooth-auto {}", appearance_prefix(id))
}

fn apply_peer_aliases(peers: &mut [InterfacePeer], aliases: &HashMap<String, String>) {
    for peer in peers {
        peer.alias = aliases
            .get(&peer.id)
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned);
    }
}

pub fn interface_mode_label(mode: InterfaceMode) -> &'static str {
    mode.label()
}

fn connection_label(kind: Option<InterfaceKind>, connection: ConnectionState) -> String {
    match connection {
        ConnectionState::Initializing => "Initializing".to_string(),
        ConnectionState::Connected => "Connected".to_string(),
        ConnectionState::Degraded => "Degraded".to_string(),
        ConnectionState::Reconnecting => "Retrying".to_string(),
        ConnectionState::Failed => "Failed".to_string(),
        ConnectionState::Disconnected => match kind {
            Some(
                InterfaceKind::AutoWifi
                | InterfaceKind::WifiDirect
                | InterfaceKind::WifiAware
                | InterfaceKind::UsbAutoHost
                | InterfaceKind::UsbAutoDevice,
            ) => "Waiting".to_string(),
            Some(InterfaceKind::BluetoothAuto) => "No Peers".to_string(),
            Some(_) | None => "Disconnected".to_string(),
        },
        ConnectionState::Disabled => "Off".to_string(),
        ConnectionState::Unknown => "Unknown".to_string(),
    }
}

pub(crate) fn apply_wifi_station_to_entry(entry: &mut InterfaceEntry, ssid: &str) {
    let detail = personal_rns::remote_control::wifi_station_inventory_config(ssid).to_string();
    let kind = InterfaceKind::ALL
        .into_iter()
        .find(|kind| kind.name() == entry.kind);
    entry.detail = Some(detail.clone());
    entry.extras = hopspot_extra_facts(Some(&detail), kind, None);
}

pub(crate) fn apply_lora_profile_to_entry(entry: &mut InterfaceEntry, profile: RadioProfile) {
    let detail = profile.inventory_config().to_string();
    let kind = InterfaceKind::ALL
        .into_iter()
        .find(|kind| kind.name() == entry.kind);
    entry.detail = Some(detail.clone());
    entry.extras = hopspot_extra_facts(Some(&detail), kind, None);
}

fn hopspot_extra_facts(
    detail: Option<&str>,
    kind: Option<InterfaceKind>,
    drops: Option<(u32, u32)>,
) -> Vec<InterfaceFact> {
    let mut extras = Vec::new();
    if let Some((egress, ingress)) = drops {
        if egress > 0 {
            extras.push(interface_fact("Egress drops", egress.to_string()));
        }
        if ingress > 0 {
            extras.push(interface_fact("RX drops", ingress.to_string()));
        }
    }
    if let Some(detail) = detail.map(str::trim).filter(|value| !value.is_empty()) {
        if let Some(profile) = RadioProfile::parse_inventory_config(detail) {
            extras.push(interface_fact("Config", "LoRa".to_string()));
            extras.extend(lora_tune_facts(&profile));
        } else if let Some(ssid) = personal_rns::remote_control::parse_wifi_station_ssid(detail) {
            extras.push(interface_fact(
                "SSID",
                if ssid.is_empty() {
                    "not set".to_string()
                } else {
                    ssid.to_string()
                },
            ));
        } else {
            for part in detail.split(" · ") {
                let part = part.trim();
                if let Some(size) = part.strip_prefix("IFAC ") {
                    extras.push(interface_fact("IFAC", size.to_string()));
                } else if let Some((host, port)) = split_host_port(part) {
                    extras.push(interface_fact("Host", host.to_string()));
                    extras.push(interface_fact("Port", port.to_string()));
                } else if part == "LoRa" {
                    extras.push(interface_fact("Config", part.to_string()));
                } else if !part.is_empty() {
                    extras.push(interface_fact("Role", part.to_string()));
                }
            }
        }
    } else if matches!(kind, Some(InterfaceKind::LoRa | InterfaceKind::Rnode)) {
        extras.push(interface_fact("Config", "LoRa".to_string()));
    } else if let Some(role) = kind.and_then(interface_role_label) {
        extras.push(interface_fact("Role", role.to_string()));
    }
    extras
}

fn lora_tune_facts(profile: &RadioProfile) -> Vec<InterfaceFact> {
    let Modulation::Lora {
        spreading_factor,
        bandwidth,
        coding_rate,
    } = profile.modulation;
    let preset = ModemPreset::matching(profile.modulation)
        .map(ModemPreset::label)
        .unwrap_or("Custom");
    vec![
        interface_fact("Region", profile.region.label().to_string()),
        interface_fact(
            "Frequency",
            format_lora_frequency_mhz(profile.frequency.hz()),
        ),
        interface_fact("Preset", preset.to_string()),
        interface_fact("SF", (spreading_factor as u8).to_string()),
        interface_fact("Bandwidth", format!("{} kHz", bandwidth.hz() / 1_000)),
        interface_fact("CR", format!("4/{}", coding_rate.denominator())),
        interface_fact("TX power", format!("{} dBm", profile.tx_power.dbm())),
        interface_fact("Preamble", profile.preamble.count().to_string()),
    ]
}

fn format_lora_frequency_mhz(hz: u32) -> String {
    format!("{}.{:03} MHz", hz / 1_000_000, (hz % 1_000_000) / 1_000)
}

fn interface_fact(label: &str, value: String) -> InterfaceFact {
    InterfaceFact {
        label: label.to_string(),
        value,
    }
}

fn split_host_port(value: &str) -> Option<(&str, &str)> {
    let (host, port) = value.rsplit_once(':')?;
    if host.is_empty() || port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((host, port))
}

fn interface_role_label(kind: InterfaceKind) -> Option<&'static str> {
    match kind {
        InterfaceKind::AutoWifi => Some("Auto discovery"),
        InterfaceKind::LocalServer => Some("Shared instance"),
        InterfaceKind::TcpServer => Some("TCP listen"),
        InterfaceKind::TcpClient => Some("TCP dial"),
        InterfaceKind::WebSocketServer => Some("WebSocket listen"),
        InterfaceKind::WebSocketClient => Some("WebSocket dial"),
        InterfaceKind::BackboneServer => Some("Backbone listen"),
        InterfaceKind::BackboneClient => Some("Backbone dial"),
        InterfaceKind::BluetoothAuto => Some("BLE supervisor"),
        InterfaceKind::UsbAutoHost | InterfaceKind::UsbAutoDevice => Some("USB"),
        InterfaceKind::LoRa | InterfaceKind::Rnode => Some("LoRa"),
        InterfaceKind::EspNow => Some("ESP-NOW"),
        _ => None,
    }
}

fn local_frame_drops(coverage: &FrameAccountingCoverage) -> Option<(u32, u32)> {
    let accounting = coverage.complete()?;
    let ingress = accounting
        .malformed
        .saturating_add(accounting.undecodable)
        .min(u64::from(u32::MAX)) as u32;
    let egress = 0;
    (ingress > 0).then_some((egress, ingress))
}

fn activity_connection_code(connection: &str) -> u8 {
    match connection {
        "Connected" => 1,
        "Degraded" => 2,
        "Initializing" => 3,
        "Retrying" => 4,
        "Failed" => 5,
        "Waiting" | "No Peers" | "Disconnected" => 6,
        "Off" => 7,
        _ => 0,
    }
}

const RSSI_LABEL: &str = "RSSI";
const SNR_LABEL: &str = "SNR";
const QUALITY_LABEL: &str = "Quality";
const SIGNAL_LABEL: &str = "Signal";

pub fn radio_facts(radio: RadioIndication) -> Vec<InterfaceFact> {
    match radio {
        RadioIndication::NotRadio => Vec::new(),
        RadioIndication::Bluetooth(BluetoothIndication::Pending)
        | RadioIndication::Wifi(WifiIndication::Pending)
        | RadioIndication::LoRa(LoRaIndication::Pending) => vec![InterfaceFact {
            label: SIGNAL_LABEL.to_string(),
            value: "pending".to_string(),
        }],
        RadioIndication::Wifi(WifiIndication::Unavailable) => vec![InterfaceFact {
            label: SIGNAL_LABEL.to_string(),
            value: "not measured on this medium".to_string(),
        }],
        RadioIndication::Bluetooth(BluetoothIndication::Rssi(rssi))
        | RadioIndication::Wifi(WifiIndication::Rssi(rssi)) => vec![rssi_fact(rssi)],
        RadioIndication::LoRa(LoRaIndication::Sample { rssi, snr, quality }) => {
            let mut facts = vec![rssi_fact(rssi)];
            if let Some(snr) = snr {
                facts.push(InterfaceFact {
                    label: SNR_LABEL.to_string(),
                    value: format_snr(snr),
                });
            }
            if let Some(quality) = quality {
                facts.push(InterfaceFact {
                    label: QUALITY_LABEL.to_string(),
                    value: format_quality(quality),
                });
            }
            facts
        }
    }
}

fn rssi_fact(rssi: RssiDbm) -> InterfaceFact {
    InterfaceFact {
        label: RSSI_LABEL.to_string(),
        value: format!("{} dBm", rssi.get()),
    }
}

fn format_snr(snr: SnrQuarterDb) -> String {
    let tenths = i32::from(snr.quarters()).saturating_mul(10) / 4;
    let magnitude = tenths.unsigned_abs();
    let sign = if tenths < 0 { "-" } else { "" };
    format!("{sign}{}.{} dB", magnitude / 10, magnitude % 10)
}

fn format_quality(quality: SignalQualityTenthsPercent) -> String {
    let tenths = quality.tenths_percent();
    format!("{}.{}%", tenths / 10, tenths % 10)
}

pub fn format_activity_age(age_secs: Option<u32>) -> String {
    match age_secs {
        None => "—".to_string(),
        Some(0) => "now".to_string(),
        Some(seconds) if seconds < 60 => format!("{seconds}s"),
        Some(seconds) if seconds < 3600 => format!("{}m", seconds / 60),
        Some(seconds) => format!("{}h", seconds / 3600),
    }
}

fn interface_power_from_connection(connection: ConnectionState) -> InterfacePower {
    match connection {
        ConnectionState::Disabled => InterfacePower::Off,
        ConnectionState::Connected
        | ConnectionState::Degraded
        | ConnectionState::Initializing
        | ConnectionState::Reconnecting
        | ConnectionState::Failed
        | ConnectionState::Disconnected
        | ConnectionState::Unknown => InterfacePower::On,
    }
}

fn stored_alias(stored: Option<&str>) -> Option<String> {
    stored
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn load_target_names(path: &PathBuf) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    parse_target_names(&text)
}

fn persist_target_names(path: &PathBuf, names: &HashMap<String, String>) {
    let _ = std::fs::write(path, render_target_names(names));
}

fn parse_target_names(text: &str) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for line in text.lines() {
        let Some((id, name)) = line.split_once('\t') else {
            continue;
        };
        let id = id.trim();
        let name = name.trim();
        if id.is_empty() || name.is_empty() {
            continue;
        }
        names.insert(id.to_owned(), name.to_owned());
    }
    names
}

fn render_target_names(names: &HashMap<String, String>) -> String {
    let mut lines = names
        .iter()
        .map(|(id, name)| format!("{id}\t{name}"))
        .collect::<Vec<_>>();
    lines.sort();
    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

fn parse_allow_list_key(
    input: &str,
) -> Result<personal_rns::remote_control::RemoteControlControllerIdentity, BackendError> {
    let input = input.trim();
    if parse_hex::<IDENTITY_HASH_BYTES>(input).is_ok() {
        return Err(BackendError::Operation {
            operation: "authorize controller",
            detail: "paste the allow-list key from that controller's Settings, not the 32-character identity hash".to_string(),
        });
    }
    let bytes =
        parse_hex::<IDENTITY_PUBLIC_KEY_LEN>(input).map_err(|_| BackendError::Operation {
            operation: "authorize controller",
            detail: "allow-list key must be 128 hex characters from that controller's Settings"
                .to_string(),
        })?;
    parse_controller_public_keys(&bytes).ok_or(BackendError::Operation {
        operation: "authorize controller",
        detail: "that allow-list key is not a valid controller public key".to_string(),
    })
}

fn controller_whitelist_exchange_error(error: RemoteControlTargetOperationError) -> BackendError {
    match error {
        RemoteControlTargetOperationError::Exchange(RemoteControlError::Remote(
            RemoteControlProtocolError::UnknownRequestKind { found },
        )) if found == RemoteControlRequestKind::InventoryControllers.wire_value()
            || found == RemoteControlRequestKind::AuthorizeController.wire_value()
            || found == RemoteControlRequestKind::RevokeController.wire_value() =>
        {
            BackendError::Operation {
                operation: "node management whitelist",
                detail: "the target firmware does not recognize controller allow-list edits yet. Flash the current Hopspot build onto that board; an existing pair already has the grant".to_string(),
            }
        }
        error => operation("node management whitelist", error),
    }
}

fn wifi_station_exchange_error(error: RemoteControlTargetOperationError) -> BackendError {
    match error {
        RemoteControlTargetOperationError::Exchange(RemoteControlError::Remote(
            RemoteControlProtocolError::UnknownRequestKind { found },
        )) if found == RemoteControlRequestKind::SetInterfaceWifiStation.wire_value() => {
            BackendError::Operation {
                operation: "set interface Wi-Fi station",
                detail: "the target firmware does not recognize Wi-Fi station edits yet. Flash the current Hopspot build onto that board; an existing pair already has the grant".to_string(),
            }
        }
        error => operation("set interface Wi-Fi station", error),
    }
}

fn lora_profile_exchange_error(error: RemoteControlTargetOperationError) -> BackendError {
    match error {
        RemoteControlTargetOperationError::Exchange(RemoteControlError::Remote(
            RemoteControlProtocolError::UnknownRequestKind { found },
        )) if found == RemoteControlRequestKind::SetInterfaceLoRaProfile.wire_value() => {
            BackendError::Operation {
                operation: "set interface LoRa profile",
                detail: "the target firmware does not recognize LoRa tune yet. Flash the current Hopspot build onto that board; an existing pair already has the grant".to_string(),
            }
        }
        error => operation("set interface LoRa profile", error),
    }
}

fn operation(operation: &'static str, error: impl std::fmt::Debug) -> BackendError {
    BackendError::Operation {
        operation,
        detail: format!("{error:?}"),
    }
}

fn inventory_recovery_continues(now: Instant, deadline: Instant) -> bool {
    now < deadline
}

fn control_announce_satisfies(
    heard: Option<InstantMillis>,
    wait: RemoteControlAnnounceWait,
    baseline: Option<InstantMillis>,
) -> bool {
    let Some(learned_at) = heard else {
        return false;
    };
    match wait {
        RemoteControlAnnounceWait::UntilHeard => true,
        RemoteControlAnnounceWait::UntilRefreshed => match baseline {
            None => true,
            Some(prior) => learned_at != prior,
        },
    }
}

fn path_from_route(route: &RouteSnapshot) -> TargetPath {
    TargetPath {
        hops: route.hops,
        via: format_next_hop(route.via),
        interface: format_interface(route.interface),
        announced_at: format_announce_millis(route.learned_at.0),
    }
}

fn path_from_announce(hops: u8, interface: InterfaceId, observed_at: InstantMillis) -> TargetPath {
    TargetPath {
        hops,
        via: if hops == 0 {
            "direct".to_string()
        } else {
            "relayed".to_string()
        },
        interface: format_interface(interface),
        announced_at: format_announce_millis(observed_at.0),
    }
}

fn format_next_hop(via: NextHop) -> String {
    match via {
        NextHop::Direct => "direct".to_string(),
        NextHop::Via(transport) => format!("via {}", short_id(&encode_hex(transport.as_bytes()))),
    }
}

fn format_interface(interface: InterfaceId) -> String {
    route_interface_kind(interface).name().to_string()
}

/// USB Auto host ids are a `[0xD0; 8]` cookie, not `kind ++ hash`, so
/// `InterfaceId::kind` is `None`. Settings already stamps those as
/// `UsbAutoHost`; route lines use the same override.
fn route_interface_kind(interface: InterfaceId) -> InterfaceKind {
    match interface.kind() {
        Some(kind) => kind,
        None => InterfaceKind::UsbAutoHost,
    }
}

fn format_hop_count(hops: u8) -> String {
    if hops == 1 {
        "1 hop".to_string()
    } else {
        format!("{hops} hops")
    }
}

pub fn format_target_announce(path: Option<&TargetPath>) -> String {
    match path {
        Some(path) => format!("Last announce {}", path.announced_at),
        None => "Last announce not heard yet".to_string(),
    }
}

pub fn format_target_route(path: Option<&TargetPath>) -> String {
    match path {
        Some(path) => format!(
            "{} {} on {}",
            format_hop_count(path.hops),
            path.via,
            path.interface
        ),
        None => "No path heard yet".to_string(),
    }
}

fn format_announce_millis(millis: u64) -> String {
    let unix_secs = i64::try_from(millis / 1_000).unwrap_or(0);
    match local_civil(unix_secs) {
        Some((year, month, day, hour, minute, second, offset_secs)) => format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} {}",
            format_utc_offset(offset_secs)
        ),
        None => format_utc_millis(millis),
    }
}

#[cfg(unix)]
fn local_civil(unix_secs: i64) -> Option<(i32, u32, u32, u32, u32, u32, i32)> {
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    let time = libc::time_t::try_from(unix_secs).ok()?;
    let ptr = unsafe { libc::localtime_r(&time, tm.as_mut_ptr()) };
    if ptr.is_null() {
        return None;
    }
    let tm = unsafe { tm.assume_init() };
    Some((
        tm.tm_year.checked_add(1900)?,
        u32::try_from(tm.tm_mon.checked_add(1)?).ok()?,
        u32::try_from(tm.tm_mday).ok()?,
        u32::try_from(tm.tm_hour).ok()?,
        u32::try_from(tm.tm_min).ok()?,
        u32::try_from(tm.tm_sec).ok()?,
        i32::try_from(tm.tm_gmtoff).ok()?,
    ))
}

#[cfg(not(unix))]
fn local_civil(_unix_secs: i64) -> Option<(i32, u32, u32, u32, u32, u32, i32)> {
    None
}

fn format_utc_offset(offset_secs: i32) -> String {
    let sign = if offset_secs >= 0 { '+' } else { '-' };
    let abs = offset_secs.unsigned_abs();
    format!("{sign}{:02}:{:02}", abs / 3_600, (abs % 3_600) / 60)
}

fn format_utc_millis(millis: u64) -> String {
    let (year, month, day, hour, minute, second) = utc_date_time(millis / 1_000);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn utc_date_time(seconds: u64) -> (i32, u32, u32, u32, u32, u32) {
    let seconds = i64::try_from(seconds).unwrap_or(i64::MAX);
    let days = seconds.div_euclid(86_400);
    let time_of_day = u32::try_from(seconds.rem_euclid(86_400)).unwrap_or(0);
    let hour = time_of_day / 3_600;
    let minute = (time_of_day % 3_600) / 60;
    let second = time_of_day % 60;
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = u32::try_from(shifted - era * 146_097).unwrap_or(0);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = i32::try_from(year_of_era).unwrap_or(0) + i32::try_from(era).unwrap_or(0) * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_period = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_period + 2) / 5 + 1;
    let month = if month_period < 10 {
        month_period + 3
    } else {
        month_period - 9
    };
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::{
        apply_peer_aliases, attach_usb_link_peer, bluetooth_auto_title,
        clone_announce_is_usb_local, control_announce_satisfies, controller_identity_secret_path,
        encode_hex, format_activity_age, format_announce_millis, format_hop_count,
        format_interface, format_target_announce, format_target_route, format_utc_millis,
        format_utc_offset, instance_identity_secret_path, interface_peer_from_wire,
        interface_power_from_connection, inventory_recovery_continues, local_interface_config,
        local_interface_entry, operator_interface_kind, operator_local_kind, parse_invitation_code,
        parse_target_names, peer_label, radio_facts, remote_interface_entry, render_target_names,
        resolve_paired_target_hash, route_interface_kind, stored_alias, target_label, BackendError,
        InterfaceFact, InterfacePower, RemoteControlAnnounceWait, TargetPath,
        CONTROLLER_IDENTITY_FILE, INSTANCE_IDENTITY_FILE, MANAGER_ALIASES_FILE,
    };
    use personal_rns::identity::IdentityHash;
    use personal_rns::interfaces::bluetooth_auto::BleIdentity;
    use personal_rns::interfaces::{
        ConnectionState, InterfaceGravity, InterfaceId, InterfaceKind, InterfaceMode,
        InterfaceSnapshot, Membership, PacketPhyStats, RadioIndication, RssiDbm,
        SignalQualityTenthsPercent, SnrQuarterDb, WifiIndication,
    };
    use personal_rns::remote_control::{
        RemoteControlInterfaceCard, RemoteControlInterfaceEntry, RemoteControlInterfacePeer,
    };
    use personal_rns::units::InstantMillis;
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn target_label_is_the_short_dest_prefix_and_alias_is_optional() {
        assert_eq!(
            target_label("b5ae6f59f1febf48dd3623181fb7a9f1"),
            "T b5ae6f59"
        );
        assert_eq!(stored_alias(Some("prnsd")).as_deref(), Some("prnsd"));
        assert_eq!(stored_alias(None), None);
        assert_eq!(stored_alias(Some("   ")), None);
    }

    #[test]
    fn resolve_paired_target_hash_prefers_the_pending_identity() {
        let wanted = IdentityHash::new([0x11; 16]);
        let other = IdentityHash::new([0x22; 16]);
        let listed = [other, wanted];
        assert_eq!(
            resolve_paired_target_hash(&listed, Some(&encode_hex(wanted.as_bytes()))),
            Some(wanted)
        );
        assert_eq!(resolve_paired_target_hash(&listed, None), None);
        assert_eq!(resolve_paired_target_hash(&[wanted], None), Some(wanted));
        assert_eq!(
            resolve_paired_target_hash(
                &listed,
                Some(&encode_hex(IdentityHash::new([0x33; 16]).as_bytes()))
            ),
            None
        );
    }

    #[test]
    fn peer_label_uses_the_first_two_hash_bytes_and_is_stable_for_the_same_tag() {
        let first = InterfaceId::from_channel_tag(InterfaceKind::BluetoothPeer, b"stable-identity");
        let again = InterfaceId::from_channel_tag(InterfaceKind::BluetoothPeer, b"stable-identity");
        assert_eq!(first, again);
        assert_eq!(peer_label(first), peer_label(again));
        assert!(peer_label(first).starts_with("P "));
        assert_eq!(peer_label(first).len(), 6);
        let identity = BleIdentity::new(*b"stable-identity!");
        let seen = InterfaceId::from_channel_tag(InterfaceKind::BluetoothPeer, identity.as_bytes());
        assert_eq!(
            bluetooth_auto_title(identity),
            format!("bluetooth-auto {}", &peer_label(seen)[2..])
        );
    }

    #[test]
    fn peer_aliases_attach_to_the_full_interface_id() {
        let id = InterfaceId::from_channel_tag(InterfaceKind::BluetoothPeer, b"kitchen");
        let mut peer = interface_peer_from_wire(&RemoteControlInterfacePeer {
            id,
            connection: ConnectionState::Connected,
            tx_bytes: 0,
            rx_bytes: 0,
            links: 0,
            destinations: 0,
            rate_bytes_per_sec: 0,
            radio: RadioIndication::for_kind(Some(InterfaceKind::BluetoothPeer)),
        });
        let mut aliases = HashMap::new();
        aliases.insert(encode_hex(id.as_bytes()), "Kitchen HV4".to_string());
        apply_peer_aliases(std::slice::from_mut(&mut peer), &aliases);
        assert_eq!(peer.alias.as_deref(), Some("Kitchen HV4"));
        apply_peer_aliases(std::slice::from_mut(&mut peer), &HashMap::new());
        assert_eq!(peer.alias, None);
    }

    #[test]
    fn target_name_file_round_trips() {
        let parsed = parse_target_names("b5ae6f59f1febf48dd3623181fb7a9f1\tprnsd\n");
        assert_eq!(
            parsed
                .get("b5ae6f59f1febf48dd3623181fb7a9f1")
                .map(String::as_str),
            Some("prnsd")
        );
        assert_eq!(
            render_target_names(&parsed),
            "b5ae6f59f1febf48dd3623181fb7a9f1\tprnsd\n"
        );
    }

    #[test]
    fn fleet_peer_kinds_are_not_operator_interfaces() {
        assert!(!operator_interface_kind(InterfaceKind::BluetoothPeer));
        assert!(!operator_interface_kind(InterfaceKind::WifiPeer));
        assert!(operator_interface_kind(InterfaceKind::BluetoothAuto));
        assert!(operator_interface_kind(InterfaceKind::TcpClient));
        assert!(operator_interface_kind(InterfaceKind::LocalServer));
        assert!(operator_interface_kind(InterfaceKind::AutoWifi));
    }

    #[test]
    fn hand_rolled_local_usb_ids_leave_as_usb_auto_host() {
        let id = InterfaceId::new([0xD0; 8]);
        let snapshot = InterfaceSnapshot {
            id,
            mode: InterfaceMode::Full,
            gravity: InterfaceGravity::ZERO,
            connection: ConnectionState::Disconnected,
            failure_reason: None,
            rx_bytes: 0,
            tx_bytes: 0,
            transfer_rates: None,
            destinations: 0,
            links: 0,
            transported_links: 0,
            membership: Membership::Independent,
            radio: RadioIndication::for_kind(None),
        };
        assert_eq!(
            operator_local_kind(&snapshot),
            Some(InterfaceKind::UsbAutoHost)
        );
        let entry = local_interface_entry(
            &snapshot,
            None,
            None,
            None,
            None,
            local_interface_config(&snapshot, None, None).as_deref(),
            None,
            None,
            Vec::new(),
        );
        assert_eq!(entry.kind, "usb-auto-host");
        assert_eq!(entry.detail.as_deref(), Some("USB"));
        assert_eq!(entry.connection, "Waiting");
        let mut waiting = entry.clone();
        attach_usb_link_peer(&mut waiting);
        assert!(waiting.peers.is_empty());
        let mut connected = entry;
        connected.connection = "Connected".to_string();
        attach_usb_link_peer(&mut connected);
        assert_eq!(connected.peers.len(), 1);
        assert_eq!(connected.peers[0].name, "USB link");
    }

    #[test]
    fn a_direct_usb_announce_is_local_at_hop_one() {
        let usb = InterfaceId::new([0xD0; 8]);
        assert!(clone_announce_is_usb_local(0, usb));
        assert!(clone_announce_is_usb_local(1, usb));
        assert!(!clone_announce_is_usb_local(2, usb));
        assert!(!clone_announce_is_usb_local(
            1,
            InterfaceId::from_channel_tag(InterfaceKind::BluetoothAuto, b"ble")
        ));
    }

    #[test]
    fn utc_millis_format_known_instants() {
        assert_eq!(format_utc_millis(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(
            format_utc_millis(1_704_067_200_000),
            "2024-01-01 00:00:00 UTC"
        );
        assert_eq!(
            format_utc_millis(1_704_067_261_000),
            "2024-01-01 00:01:01 UTC"
        );
        assert_eq!(format_utc_offset(-14_400), "-04:00");
        assert_eq!(format_utc_offset(0), "+00:00");
        assert_eq!(format_utc_offset(5_400), "+01:30");
        let local = format_announce_millis(1_704_067_200_000);
        assert!(!local.ends_with(" UTC"));
        assert!(
            local.ends_with("+00:00") || local.contains(" -") || local.contains(" +"),
            "{local}"
        );
    }

    #[test]
    fn format_interface_stamps_undecodable_ids_as_usb_auto_host() {
        let usb = InterfaceId::new([0xD0; 8]);
        assert_eq!(usb.kind(), None);
        assert_eq!(route_interface_kind(usb), InterfaceKind::UsbAutoHost);
        assert_eq!(format_interface(usb), "usb-auto-host");
        assert_eq!(
            format_interface(InterfaceId::from_channel_tag(InterfaceKind::LoRa, b"lab")),
            "lora"
        );
    }

    #[test]
    fn disabled_connection_is_powered_off() {
        assert_eq!(
            interface_power_from_connection(ConnectionState::Disabled),
            InterfacePower::Off
        );
        assert_eq!(
            interface_power_from_connection(ConnectionState::Connected),
            InterfacePower::On
        );
    }

    #[test]
    fn local_interface_entry_uses_kind_name_and_tcp_detail() {
        let id = InterfaceId::from_channel_tag(InterfaceKind::TcpClient, b"tcp");
        let entry = local_interface_entry(
            &InterfaceSnapshot {
                id,
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
                radio: personal_rns::interfaces::RadioIndication::for_kind(id.kind()),
            },
            None,
            None,
            Some("home"),
            None,
            Some("127.0.0.1:4242"),
            None,
            None,
            Vec::new(),
        );
        assert_eq!(entry.kind, "tcp-client");
        assert_eq!(entry.detail.as_deref(), Some("127.0.0.1:4242"));
        assert_eq!(entry.group.as_deref(), Some("home"));
        assert_eq!(entry.power, InterfacePower::On);
        assert_eq!(entry.mode, InterfaceMode::Full);
        assert_eq!(entry.connection, "Connected");
        assert_eq!(
            entry.extras,
            vec![
                InterfaceFact {
                    label: "Host".to_string(),
                    value: "127.0.0.1".to_string(),
                },
                InterfaceFact {
                    label: "Port".to_string(),
                    value: "4242".to_string(),
                },
            ]
        );
        assert!(!entry.shows_peers);
    }

    #[test]
    fn local_ble_auto_title_uses_the_peer_facing_identity_prefix() {
        let identity = BleIdentity::new(*b"stable-identity!");
        let id = InterfaceId::from_channel_tag(InterfaceKind::BluetoothAuto, b"bluetooth-auto");
        let entry = local_interface_entry(
            &InterfaceSnapshot {
                id,
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
                radio: personal_rns::interfaces::RadioIndication::for_kind(id.kind()),
            },
            None,
            Some(identity),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
        );
        assert_eq!(entry.name, bluetooth_auto_title(identity));
        assert_eq!(
            entry.name,
            format!(
                "bluetooth-auto {}",
                &peer_label(InterfaceId::from_channel_tag(
                    InterfaceKind::BluetoothPeer,
                    identity.as_bytes()
                ))[2..]
            )
        );
    }

    #[test]
    fn remote_interface_entry_uses_card_group_peers_and_config() {
        let id = InterfaceId::from_channel_tag(InterfaceKind::BluetoothAuto, b"ble");
        let peer_id = InterfaceId::from_channel_tag(InterfaceKind::BluetoothPeer, b"peer");
        let mut card = RemoteControlInterfaceCard::empty();
        card.set_name("BLE");
        card.set_group("home");
        card.set_config("BLE supervisor");
        card.set_failure("radio timeout");
        card.destinations = 4;
        card.push_peer(RemoteControlInterfacePeer {
            id: peer_id,
            connection: ConnectionState::Degraded,
            tx_bytes: 4,
            rx_bytes: 6,
            links: 1,
            destinations: 2,
            rate_bytes_per_sec: 8,
            radio: RadioIndication::from_bluetooth_rssi(Some(-62)),
        })
        .expect("one peer fits");
        let entry = remote_interface_entry(
            &RemoteControlInterfaceEntry {
                id,
                kind: InterfaceKind::BluetoothAuto,
                mode: InterfaceMode::Full,
                connection: ConnectionState::Connected,
                enabled: true,
                tx_bytes: 8,
                rx_bytes: 16,
                links: 2,
                rate_bytes_per_sec: 32,
            },
            Some(&card),
        );
        assert_eq!(entry.name, "BLE");
        assert_eq!(entry.group.as_deref(), Some("home"));
        assert_eq!(entry.detail.as_deref(), Some("BLE supervisor"));
        assert_eq!(entry.failure.as_deref(), Some("radio timeout"));
        assert_eq!(entry.destinations, 4);
        assert_eq!(
            entry.extras,
            vec![InterfaceFact {
                label: "Role".to_string(),
                value: "BLE supervisor".to_string(),
            }]
        );
        assert!(entry.shows_peers);
        assert_eq!(entry.peers.len(), 1);
        assert_eq!(entry.peers[0].connection, "Degraded");
        assert_eq!(entry.peers[0].tx_bytes, 4);
        assert_eq!(entry.peers[0].rx_bytes, 6);
        assert_eq!(entry.peers[0].links, 1);
        assert_eq!(entry.peers[0].destinations, 2);
        assert_eq!(entry.peers[0].rate_bytes_per_sec, 8);
        assert_eq!(
            entry.peers[0].radio,
            RadioIndication::from_bluetooth_rssi(Some(-62))
        );
    }

    #[test]
    fn remote_lora_entry_lists_tune_facts_under_config() {
        let id = InterfaceId::from_channel_tag(InterfaceKind::LoRa, b"lora");
        let mut card = RemoteControlInterfaceCard::empty();
        card.set_name("LoRa");
        card.set_config(
            personal_rns::interfaces::lora::DEFAULT_915_PROFILE
                .inventory_config()
                .as_str(),
        );
        let entry = remote_interface_entry(
            &RemoteControlInterfaceEntry {
                id,
                kind: InterfaceKind::LoRa,
                mode: InterfaceMode::Full,
                connection: ConnectionState::Connected,
                enabled: true,
                tx_bytes: 0,
                rx_bytes: 0,
                links: 0,
                rate_bytes_per_sec: 0,
            },
            Some(&card),
        );
        assert_eq!(
            entry.extras,
            vec![
                InterfaceFact {
                    label: "Config".to_string(),
                    value: "LoRa".to_string(),
                },
                InterfaceFact {
                    label: "Region".to_string(),
                    value: "US915".to_string(),
                },
                InterfaceFact {
                    label: "Frequency".to_string(),
                    value: "915.000 MHz".to_string(),
                },
                InterfaceFact {
                    label: "Preset".to_string(),
                    value: "MediumFast".to_string(),
                },
                InterfaceFact {
                    label: "SF".to_string(),
                    value: "9".to_string(),
                },
                InterfaceFact {
                    label: "Bandwidth".to_string(),
                    value: "250 kHz".to_string(),
                },
                InterfaceFact {
                    label: "CR".to_string(),
                    value: "4/5".to_string(),
                },
                InterfaceFact {
                    label: "TX power".to_string(),
                    value: "22 dBm".to_string(),
                },
                InterfaceFact {
                    label: "Preamble".to_string(),
                    value: "18".to_string(),
                },
            ]
        );
    }

    #[test]
    fn target_path_lines_split_announce_from_route() {
        let path = TargetPath {
            hops: 2,
            via: "via a1b2c3d4".to_string(),
            interface: "auto-wifi".to_string(),
            announced_at: "2026-09-05 18:59:12 UTC".to_string(),
        };
        assert_eq!(format_hop_count(0), "0 hops");
        assert_eq!(format_hop_count(1), "1 hop");
        assert_eq!(
            format_target_announce(Some(&path)),
            "Last announce 2026-09-05 18:59:12 UTC"
        );
        assert_eq!(
            format_target_route(Some(&path)),
            "2 hops via a1b2c3d4 on auto-wifi"
        );
        assert_eq!(format_target_announce(None), "Last announce not heard yet");
        assert_eq!(format_target_route(None), "No path heard yet");
    }

    #[test]
    fn inventory_waits_for_the_control_destination_announce() {
        let first = InstantMillis(1_000);
        let newer = InstantMillis(2_000);
        assert!(!control_announce_satisfies(
            None,
            RemoteControlAnnounceWait::UntilHeard,
            None
        ));
        assert!(control_announce_satisfies(
            Some(first),
            RemoteControlAnnounceWait::UntilHeard,
            Some(first)
        ));
        assert!(!control_announce_satisfies(
            Some(first),
            RemoteControlAnnounceWait::UntilRefreshed,
            Some(first)
        ));
        assert!(control_announce_satisfies(
            Some(newer),
            RemoteControlAnnounceWait::UntilRefreshed,
            Some(first)
        ));
        assert!(control_announce_satisfies(
            Some(first),
            RemoteControlAnnounceWait::UntilRefreshed,
            None
        ));
        let now = Instant::now();
        assert!(inventory_recovery_continues(
            now,
            now + Duration::from_secs(1)
        ));
        assert!(!inventory_recovery_continues(now, now));
        assert_eq!(format_activity_age(None), "—");
        assert_eq!(format_activity_age(Some(0)), "now");
        assert_eq!(format_activity_age(Some(12)), "12s");
        assert_eq!(format_activity_age(Some(120)), "2m");
    }

    #[test]
    fn radio_facts_name_the_measurement_the_medium_actually_has() {
        assert_eq!(radio_facts(RadioIndication::NotRadio), Vec::new());
        assert_eq!(
            radio_facts(RadioIndication::Wifi(WifiIndication::Unavailable)),
            vec![InterfaceFact {
                label: "Signal".to_string(),
                value: "not measured on this medium".to_string(),
            }]
        );
        assert_eq!(
            radio_facts(RadioIndication::from_bluetooth_rssi(Some(-62))),
            vec![InterfaceFact {
                label: "RSSI".to_string(),
                value: "-62 dBm".to_string(),
            }]
        );
        assert_eq!(
            radio_facts(RadioIndication::from_lora_phy(PacketPhyStats {
                rssi: Some(RssiDbm::new(-103)),
                snr: Some(SnrQuarterDb::new(-11)),
                quality: SignalQualityTenthsPercent::new(812),
            })),
            vec![
                InterfaceFact {
                    label: "RSSI".to_string(),
                    value: "-103 dBm".to_string(),
                },
                InterfaceFact {
                    label: "SNR".to_string(),
                    value: "-2.7 dB".to_string(),
                },
                InterfaceFact {
                    label: "Quality".to_string(),
                    value: "81.2%".to_string(),
                },
            ]
        );
    }

    #[test]
    fn controller_identity_secret_lives_under_the_identities_vault() {
        assert_eq!(
            controller_identity_secret_path(Path::new("/tmp/hopspot-remote-control/identities")),
            Path::new("/tmp/hopspot-remote-control/identities").join(CONTROLLER_IDENTITY_FILE)
        );
        assert_eq!(
            instance_identity_secret_path(Path::new("/tmp/hopspot-remote-control/identities")),
            Path::new("/tmp/hopspot-remote-control/identities").join(INSTANCE_IDENTITY_FILE)
        );
        assert_eq!(CONTROLLER_IDENTITY_FILE, "controller");
        assert_eq!(INSTANCE_IDENTITY_FILE, "instance");
    }

    #[test]
    fn manager_aliases_use_the_controller_hash_file() {
        assert_eq!(MANAGER_ALIASES_FILE, "manager-aliases");
        let parsed = parse_target_names("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\tlaptop\n");
        assert_eq!(
            parsed
                .get("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .map(String::as_str),
            Some("laptop")
        );
    }

    #[test]
    fn invitation_codes_are_case_insensitive_eight_digit_hex() {
        let upper = parse_invitation_code("ABCD1234").expect("uppercase hex is valid");
        let lower = parse_invitation_code("abcd1234").expect("lowercase hex is valid");
        let dashed = parse_invitation_code("AbCd-1234").expect("dashes are ignored");
        assert_eq!(upper, lower);
        assert_eq!(upper, dashed);
        assert_eq!(upper.value(), 0xABCD_1234);
        assert_eq!(
            parse_invitation_code("ABCD123"),
            Err(BackendError::InvalidInvitationCode)
        );
        assert_eq!(
            parse_invitation_code("ABCD12345"),
            Err(BackendError::InvalidInvitationCode)
        );
        assert_eq!(
            parse_invitation_code("ABCD12GH"),
            Err(BackendError::InvalidInvitationCode)
        );
    }
}
