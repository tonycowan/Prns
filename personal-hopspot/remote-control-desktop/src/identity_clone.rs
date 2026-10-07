//! Controller-app identity clone. The offer is a single-hop plain packet, then a
//! hop-0 link request (`AcceptDirect`) and one request endpoint. Not a prns-core
//! pairing sibling.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use personal_rns::crypto::{sha256_chunks, Ed25519Signature};
use personal_rns::identity::{
    IdentityHash, PrivateIdentityMaterial, PublicIdentityMaterial, IDENTITY_PUBLIC_KEY_LEN,
    IDENTITY_SECRET_KEY_LEN,
};
use personal_rns::prelude::{Decline, RequestContext, RequestEndpoint, RequestEndpointPolicy};
use personal_rns::routing::announce::{
    derive_destination_hash, derive_plain_destination_hash, expand_name,
};
use personal_rns::wire::DestinationHash;

use crate::backend::{InterfaceEntry, InterfacePower};

pub const IDENTITY_CLONE_APP_NAME: &str = "prns";
pub const IDENTITY_CLONE_ASPECTS: &[&str] = &["controller", "identity-clone"];
/// Well-known plain address for the one-hop adoption offer. Not announced.
pub const IDENTITY_CLONE_OFFER_ASPECTS: &[&str] = &["controller", "identity-clone", "offer"];
pub const IDENTITY_CLONE_REQUEST_ENDPOINT_ID: &str = "/prns/controller/identity-clone";

const ANNOUNCE_MAGIC: &[u8; 7] = b"PRNSCL2";
pub const IDENTITY_CLONE_NONCE_LEN: usize = 32;
const HASH_LEN: usize = 16;
const SIGNATURE_LEN: usize = 64;
const CONFIRMATION_MODULUS: u32 = 1_000_000;
const TRANSCRIPT_DOMAIN: &[u8] = b"reticulum.controller.identity.clone.transcript.v1";
const HELLO_DOMAIN: &[u8] = b"reticulum.controller.identity.clone.hello.v1";
const ACCEPT_DOMAIN: &[u8] = b"reticulum.controller.identity.clone.accept.v1";
const ANNOUNCE_LEN: usize = 7 + IDENTITY_PUBLIC_KEY_LEN + IDENTITY_CLONE_NONCE_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneTransportKind {
    Usb,
    Bluetooth,
    AutoWifi,
    Tcp,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneLocality {
    Ready,
    UsbOff,
    UsbPeerMissing,
    UsbPeerAmbiguous { peers: u8 },
    ExtraTransportOn { kind: CloneTransportKind },
}

impl CloneLocality {
    #[must_use]
    pub const fn allows_clone(self) -> bool {
        matches!(self, Self::Ready)
    }

    #[must_use]
    pub fn operator_message(self) -> String {
        match self {
            Self::Ready => {
                "USB is the only live transport and exactly one peer is present.".to_string()
            }
            Self::UsbOff => {
                "Start USB and stop BLE, Auto Wi-Fi, and TCP. Sibling adopt is USB-only for now."
                    .to_string()
            }
            Self::UsbPeerMissing => {
                "USB is on but has no peer. Plug the other Controller in over USB.".to_string()
            }
            Self::UsbPeerAmbiguous { peers } => {
                format!("USB has {peers} peers. Unplug everything except the other Controller.")
            }
            Self::ExtraTransportOn { kind } => format!(
                "Turn off {} before adopting a sibling. Sibling adopt is USB-only for now.",
                clone_kind_label(kind)
            ),
        }
    }
}

fn clone_kind_label(kind: CloneTransportKind) -> &'static str {
    match kind {
        CloneTransportKind::Usb => "USB",
        CloneTransportKind::Bluetooth => "BLE",
        CloneTransportKind::AutoWifi => "Auto Wi-Fi",
        CloneTransportKind::Tcp => "TCP",
        CloneTransportKind::Other => "the extra transport",
    }
}

#[must_use]
pub fn clone_kind_from_interface_name(name: &str) -> CloneTransportKind {
    match name {
        "usb-auto-host" | "usb-auto-device" => CloneTransportKind::Usb,
        "bluetooth-auto" | "bluetooth-peer" => CloneTransportKind::Bluetooth,
        "auto-wifi" | "wifi-peer" => CloneTransportKind::AutoWifi,
        "tcp-client" | "tcp-server" | "tcp-server-peer" => CloneTransportKind::Tcp,
        _ => CloneTransportKind::Other,
    }
}

#[must_use]
pub fn evaluate_clone_locality(interfaces: &[InterfaceEntry]) -> CloneLocality {
    let mut usb_on = false;
    let mut usb_peers = 0u8;
    for entry in interfaces {
        if entry.power != InterfacePower::On {
            continue;
        }
        match clone_kind_from_interface_name(&entry.kind) {
            CloneTransportKind::Usb => {
                usb_on = true;
                usb_peers = usb_peers.saturating_add(usb_entry_peer_count(entry));
            }
            kind => return CloneLocality::ExtraTransportOn { kind },
        }
    }
    if !usb_on {
        return CloneLocality::UsbOff;
    }
    match usb_peers {
        0 => CloneLocality::UsbPeerMissing,
        1 => CloneLocality::Ready,
        peers => CloneLocality::UsbPeerAmbiguous { peers },
    }
}

/// USB Auto is a single supervisor: live ports update that host's
/// connection, they do not appear as fleet-member peers.
#[must_use]
pub fn usb_entry_peer_count(entry: &InterfaceEntry) -> u8 {
    if !entry.peers.is_empty() {
        return u8::try_from(entry.peers.len()).unwrap_or(u8::MAX);
    }
    u8::from(usb_supervisor_link_is_up(entry))
}

#[must_use]
pub fn usb_supervisor_link_is_up(entry: &InterfaceEntry) -> bool {
    matches!(
        clone_kind_from_interface_name(&entry.kind),
        CloneTransportKind::Usb
    ) && matches!(entry.connection.as_str(), "Connected" | "Degraded")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityCloneConfirmationCode(u32);

impl IdentityCloneConfirmationCode {
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl core::fmt::Display for IdentityCloneConfirmationCode {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{:06}", self.value())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityCloneTranscript {
    source: IdentityHash,
    dest: IdentityHash,
    source_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
    dest_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
}

impl IdentityCloneTranscript {
    #[must_use]
    pub const fn new(
        source: IdentityHash,
        dest: IdentityHash,
        source_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
        dest_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
    ) -> Self {
        Self {
            source,
            dest,
            source_nonce,
            dest_nonce,
        }
    }

    #[must_use]
    pub fn confirmation_code(self) -> IdentityCloneConfirmationCode {
        let digest = sha256_chunks(&[
            TRANSCRIPT_DOMAIN,
            self.source.as_bytes(),
            self.dest.as_bytes(),
            &self.source_nonce,
            &self.dest_nonce,
        ]);
        let [first, second, third, fourth, ..] = digest;
        IdentityCloneConfirmationCode(
            u32::from_le_bytes([first, second, third, fourth]).wrapping_rem(CONFIRMATION_MODULUS),
        )
    }

    fn hello_material(self) -> [u8; 32] {
        sha256_chunks(&[
            HELLO_DOMAIN,
            self.source.as_bytes(),
            self.dest.as_bytes(),
            &self.source_nonce,
            &self.dest_nonce,
        ])
    }

    fn accept_material(self) -> [u8; 32] {
        sha256_chunks(&[
            ACCEPT_DOMAIN,
            self.source.as_bytes(),
            self.dest.as_bytes(),
            &self.source_nonce,
            &self.dest_nonce,
        ])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityCloneAnnounce {
    source_keys: PublicIdentityMaterial,
    source_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
}

impl IdentityCloneAnnounce {
    #[must_use]
    pub const fn new(
        source_keys: PublicIdentityMaterial,
        source_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
    ) -> Self {
        Self {
            source_keys,
            source_nonce,
        }
    }

    #[must_use]
    pub fn source(self) -> IdentityHash {
        self.source_keys.identity_hash()
    }

    #[must_use]
    pub const fn source_keys(self) -> PublicIdentityMaterial {
        self.source_keys
    }

    #[must_use]
    pub const fn source_nonce(self) -> [u8; IDENTITY_CLONE_NONCE_LEN] {
        self.source_nonce
    }

    pub fn write(self, out: &mut [u8]) -> Option<usize> {
        if out.len() < ANNOUNCE_LEN {
            return None;
        }
        out[..7].copy_from_slice(ANNOUNCE_MAGIC);
        out[7..7 + IDENTITY_PUBLIC_KEY_LEN].copy_from_slice(self.source_keys.as_bytes());
        out[7 + IDENTITY_PUBLIC_KEY_LEN..ANNOUNCE_LEN].copy_from_slice(&self.source_nonce);
        Some(ANNOUNCE_LEN)
    }

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != ANNOUNCE_LEN || !bytes.starts_with(ANNOUNCE_MAGIC) {
            return None;
        }
        let source_keys =
            PublicIdentityMaterial::from_slice(&bytes[7..7 + IDENTITY_PUBLIC_KEY_LEN]).ok()?;
        let mut source_nonce = [0u8; IDENTITY_CLONE_NONCE_LEN];
        source_nonce.copy_from_slice(&bytes[7 + IDENTITY_PUBLIC_KEY_LEN..ANNOUNCE_LEN]);
        Some(Self {
            source_keys,
            source_nonce,
        })
    }
}

pub fn identity_clone_destination_hash(identity: IdentityHash) -> Option<DestinationHash> {
    let name = expand_name(IDENTITY_CLONE_APP_NAME, IDENTITY_CLONE_ASPECTS).ok()?;
    Some(derive_destination_hash(&identity, &name))
}

/// Address of the single-hop plain offer. Every controller registers it; the
/// engine drops the packet if it has traveled more than one hop.
pub fn identity_clone_offer_destination_hash() -> Option<DestinationHash> {
    let name = expand_name(IDENTITY_CLONE_APP_NAME, IDENTITY_CLONE_OFFER_ASPECTS).ok()?;
    Some(derive_plain_destination_hash(&name))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityCloneMessageKind {
    Hello = 1,
    HelloAck = 2,
    Accept = 3,
    Payload = 4,
    WaitingForSource = 5,
    Error = 6,
    /// One slice of the operator copy. Stays inside a single link packet.
    PayloadChunk = 7,
    /// Adoptee has stored the operator copy.
    Done = 8,
}

/// Small enough to fit a direct USB link packet without a resource transfer.
pub const CLONE_PAYLOAD_CHUNK_LEN: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityCloneErrorCode {
    NotOffering = 1,
    SessionMismatch = 2,
    InvalidSignature = 3,
    Malformed = 4,
}

const HELLO_LEN: usize = 1 + IDENTITY_PUBLIC_KEY_LEN + IDENTITY_CLONE_NONCE_LEN + SIGNATURE_LEN;
const ACCEPT_BODY_LEN: usize = HASH_LEN + IDENTITY_CLONE_NONCE_LEN + SIGNATURE_LEN;
const ACCEPT_LEN: usize = 1 + ACCEPT_BODY_LEN + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityCloneHello {
    dest_keys: PublicIdentityMaterial,
    dest_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
    signature: Ed25519Signature,
}

impl IdentityCloneHello {
    pub fn sign(
        dest_secret: &PrivateIdentityMaterial,
        source: IdentityHash,
        source_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
        dest_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
    ) -> Self {
        let transcript = IdentityCloneTranscript::new(
            source,
            dest_secret.identity_hash(),
            source_nonce,
            dest_nonce,
        );
        Self {
            dest_keys: dest_secret.public(),
            dest_nonce,
            signature: dest_secret.sign(&transcript.hello_material()),
        }
    }

    pub fn verify(
        &self,
        source: IdentityHash,
        source_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
    ) -> Option<IdentityCloneTranscript> {
        let transcript = IdentityCloneTranscript::new(
            source,
            self.dest_keys.identity_hash(),
            source_nonce,
            self.dest_nonce,
        );
        self.dest_keys
            .verify(&transcript.hello_material(), &self.signature)
            .ok()?;
        Some(transcript)
    }

    pub fn dest_keys(self) -> PublicIdentityMaterial {
        self.dest_keys
    }

    pub fn write(self, out: &mut [u8]) -> Option<usize> {
        if out.len() < HELLO_LEN {
            return None;
        }
        out[0] = IdentityCloneMessageKind::Hello as u8;
        out[1..1 + IDENTITY_PUBLIC_KEY_LEN].copy_from_slice(self.dest_keys.as_bytes());
        let nonce_at = 1 + IDENTITY_PUBLIC_KEY_LEN;
        out[nonce_at..nonce_at + IDENTITY_CLONE_NONCE_LEN].copy_from_slice(&self.dest_nonce);
        out[nonce_at + IDENTITY_CLONE_NONCE_LEN..HELLO_LEN].copy_from_slice(&self.signature.0);
        Some(HELLO_LEN)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentityCloneAccept {
    dest: IdentityHash,
    dest_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
    signature: Ed25519Signature,
    offset: u32,
}

impl IdentityCloneAccept {
    pub fn sign(
        dest_secret: &PrivateIdentityMaterial,
        transcript: IdentityCloneTranscript,
    ) -> Self {
        Self {
            dest: dest_secret.identity_hash(),
            dest_nonce: transcript.dest_nonce,
            signature: dest_secret.sign(&transcript.accept_material()),
            offset: 0,
        }
    }

    #[must_use]
    pub const fn at_offset(mut self, offset: u32) -> Self {
        self.offset = offset;
        self
    }

    #[must_use]
    pub const fn offset(self) -> u32 {
        self.offset
    }

    pub fn verify(
        &self,
        dest_keys: &PublicIdentityMaterial,
        transcript: IdentityCloneTranscript,
    ) -> bool {
        dest_keys.identity_hash() == self.dest
            && transcript.dest_nonce == self.dest_nonce
            && dest_keys
                .verify(&transcript.accept_material(), &self.signature)
                .is_ok()
    }

    pub fn write(self, out: &mut [u8]) -> Option<usize> {
        if out.len() < ACCEPT_LEN {
            return None;
        }
        out[0] = IdentityCloneMessageKind::Accept as u8;
        out[1..1 + HASH_LEN].copy_from_slice(self.dest.as_bytes());
        let nonce_at = 1 + HASH_LEN;
        out[nonce_at..nonce_at + IDENTITY_CLONE_NONCE_LEN].copy_from_slice(&self.dest_nonce);
        out[nonce_at + IDENTITY_CLONE_NONCE_LEN
            ..nonce_at + IDENTITY_CLONE_NONCE_LEN + SIGNATURE_LEN]
            .copy_from_slice(&self.signature.0);
        let offset_at = nonce_at + IDENTITY_CLONE_NONCE_LEN + SIGNATURE_LEN;
        out[offset_at..ACCEPT_LEN].copy_from_slice(&self.offset.to_le_bytes());
        Some(ACCEPT_LEN)
    }

    pub fn write_done(self, out: &mut [u8]) -> Option<usize> {
        let len = self.write(out)?;
        out.first_mut()
            .map(|byte| *byte = IdentityCloneMessageKind::Done as u8)?;
        Some(len)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityCloneInbound<'a> {
    Hello(IdentityCloneHello),
    HelloAck,
    Accept(IdentityCloneAccept),
    Payload {
        secret: &'a [u8; IDENTITY_SECRET_KEY_LEN],
        accesses: &'a [u8],
        clock: u64,
        siblings: &'a [u8],
        labels: &'a [u8],
    },
    WaitingForSource,
    Error(IdentityCloneErrorCode),
    PayloadChunk {
        offset: u32,
        total: u32,
        data: &'a [u8],
    },
    Done(IdentityCloneAccept),
}

pub fn parse_identity_clone_message(bytes: &[u8]) -> Option<IdentityCloneInbound<'_>> {
    let (kind, rest) = bytes.split_first()?;
    match *kind {
        1 => parse_hello(rest),
        2 if rest.is_empty() => Some(IdentityCloneInbound::HelloAck),
        3 => parse_accept(rest),
        4 => parse_payload(rest),
        5 if rest.is_empty() => Some(IdentityCloneInbound::WaitingForSource),
        7 => parse_payload_chunk(rest),
        8 => parse_accept(rest).and_then(|inbound| match inbound {
            IdentityCloneInbound::Accept(accept) => Some(IdentityCloneInbound::Done(accept)),
            _ => None,
        }),
        6 => {
            let (code, rest) = rest.split_first()?;
            if !rest.is_empty() {
                return None;
            }
            let code = match *code {
                1 => IdentityCloneErrorCode::NotOffering,
                2 => IdentityCloneErrorCode::SessionMismatch,
                3 => IdentityCloneErrorCode::InvalidSignature,
                4 => IdentityCloneErrorCode::Malformed,
                _ => return None,
            };
            Some(IdentityCloneInbound::Error(code))
        }
        _ => None,
    }
}

fn parse_hello(rest: &[u8]) -> Option<IdentityCloneInbound<'_>> {
    if rest.len() != HELLO_LEN - 1 {
        return None;
    }
    let dest_keys = PublicIdentityMaterial::from_slice(&rest[..IDENTITY_PUBLIC_KEY_LEN]).ok()?;
    let mut dest_nonce = [0u8; IDENTITY_CLONE_NONCE_LEN];
    dest_nonce.copy_from_slice(
        &rest[IDENTITY_PUBLIC_KEY_LEN..IDENTITY_PUBLIC_KEY_LEN + IDENTITY_CLONE_NONCE_LEN],
    );
    let mut signature = [0u8; SIGNATURE_LEN];
    signature.copy_from_slice(&rest[IDENTITY_PUBLIC_KEY_LEN + IDENTITY_CLONE_NONCE_LEN..]);
    Some(IdentityCloneInbound::Hello(IdentityCloneHello {
        dest_keys,
        dest_nonce,
        signature: Ed25519Signature(signature),
    }))
}

fn parse_accept(rest: &[u8]) -> Option<IdentityCloneInbound<'_>> {
    if rest.len() != ACCEPT_LEN - 1 {
        return None;
    }
    let mut dest = [0u8; HASH_LEN];
    dest.copy_from_slice(&rest[..HASH_LEN]);
    let mut dest_nonce = [0u8; IDENTITY_CLONE_NONCE_LEN];
    dest_nonce.copy_from_slice(&rest[HASH_LEN..HASH_LEN + IDENTITY_CLONE_NONCE_LEN]);
    let mut signature = [0u8; SIGNATURE_LEN];
    let signature_at = HASH_LEN + IDENTITY_CLONE_NONCE_LEN;
    signature.copy_from_slice(&rest[signature_at..signature_at + SIGNATURE_LEN]);
    let mut offset_bytes = [0u8; 4];
    offset_bytes.copy_from_slice(&rest[signature_at + SIGNATURE_LEN..]);
    Some(IdentityCloneInbound::Accept(IdentityCloneAccept {
        dest: IdentityHash::new(dest),
        dest_nonce,
        signature: Ed25519Signature(signature),
        offset: u32::from_le_bytes(offset_bytes),
    }))
}

fn parse_payload_chunk(rest: &[u8]) -> Option<IdentityCloneInbound<'_>> {
    if rest.len() < 8 {
        return None;
    }
    let offset = u32::from_le_bytes(rest[..4].try_into().ok()?);
    let total = u32::from_le_bytes(rest[4..8].try_into().ok()?);
    Some(IdentityCloneInbound::PayloadChunk {
        offset,
        total,
        data: &rest[8..],
    })
}

pub fn write_payload_chunk(
    offset: u32,
    total: u32,
    payload: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let start = usize::try_from(offset).ok()?;
    let rest = payload.get(start..)?;
    let data = if rest.len() > CLONE_PAYLOAD_CHUNK_LEN {
        &rest[..CLONE_PAYLOAD_CHUNK_LEN]
    } else {
        rest
    };
    let len = 1 + 4 + 4 + data.len();
    if out.len() < len {
        return None;
    }
    out[0] = IdentityCloneMessageKind::PayloadChunk as u8;
    out[1..5].copy_from_slice(&offset.to_le_bytes());
    out[5..9].copy_from_slice(&total.to_le_bytes());
    out[9..len].copy_from_slice(data);
    Some(len)
}

fn parse_payload(rest: &[u8]) -> Option<IdentityCloneInbound<'_>> {
    if rest.len() < IDENTITY_SECRET_KEY_LEN + 4 + 8 + 1 {
        return None;
    }
    let secret = rest.get(..IDENTITY_SECRET_KEY_LEN)?.try_into().ok()?;
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&rest[IDENTITY_SECRET_KEY_LEN..IDENTITY_SECRET_KEY_LEN + 4]);
    let access_len = u32::from_le_bytes(len_bytes) as usize;
    let start = IDENTITY_SECRET_KEY_LEN + 4;
    let accesses = rest.get(start..start + access_len)?;
    let clock_at = start + access_len;
    let clock_bytes = rest.get(clock_at..clock_at + 8)?;
    let clock = u64::from_le_bytes(clock_bytes.try_into().ok()?);
    let sibling_at = clock_at + 8;
    let count = *rest.get(sibling_at)?;
    let sibling_bytes = 1 + usize::from(count).saturating_mul(IDENTITY_PUBLIC_KEY_LEN);
    let siblings = rest.get(sibling_at..sibling_at + sibling_bytes)?;
    let labels = rest.get(sibling_at + sibling_bytes..).unwrap_or(&[]);
    Some(IdentityCloneInbound::Payload {
        secret,
        accesses,
        clock,
        siblings,
        labels,
    })
}

pub fn parse_clone_siblings(bytes: &[u8]) -> Option<Vec<PublicIdentityMaterial>> {
    let (count, rest) = bytes.split_first()?;
    let expected = usize::from(*count).saturating_mul(IDENTITY_PUBLIC_KEY_LEN);
    if rest.len() < expected {
        return None;
    }
    let rest = &rest[..expected];
    let mut siblings = Vec::with_capacity(usize::from(*count));
    for chunk in rest.chunks_exact(IDENTITY_PUBLIC_KEY_LEN) {
        siblings.push(PublicIdentityMaterial::from_slice(chunk).ok()?);
    }
    Some(siblings)
}

pub fn write_hello_ack(out: &mut [u8]) -> Option<usize> {
    out.first_mut()
        .map(|byte| *byte = IdentityCloneMessageKind::HelloAck as u8)
        .map(|()| 1)
}

pub fn write_waiting_for_source(out: &mut [u8]) -> Option<usize> {
    out.first_mut()
        .map(|byte| *byte = IdentityCloneMessageKind::WaitingForSource as u8)
        .map(|()| 1)
}

pub fn write_clone_error(code: IdentityCloneErrorCode, out: &mut [u8]) -> Option<usize> {
    if out.len() < 2 {
        return None;
    }
    out[0] = IdentityCloneMessageKind::Error as u8;
    out[1] = code as u8;
    Some(2)
}

#[derive(Clone, Default)]
pub struct ControllerAppState {
    pub clone: Arc<Mutex<IdentityCloneShared>>,
    pub roster: Arc<Mutex<crate::roster_sync::RosterShared>>,
}

impl personal_rns::runtime::RemoteControlHostControls for ControllerAppState {
    async fn execute_remote_control(
        &self,
        _command: personal_rns::runtime::RemoteControlHostCommand,
    ) -> Result<
        personal_rns::runtime::RemoteControlHostResponse,
        personal_rns::runtime::RemoteControlHostCommandError,
    > {
        Err(personal_rns::runtime::RemoteControlHostCommandError::Unsupported)
    }
}

#[derive(Default)]
pub struct IdentityCloneShared {
    pub dest_waiting: bool,
    pub source: Option<SourceCloneSession>,
    pub dest: Option<DestCloneSession>,
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl IdentityCloneShared {
    #[must_use]
    pub fn in_progress(&self) -> bool {
        self.dest_waiting || self.dest.is_some() || self.source.is_some()
    }
}

pub struct SourceCloneSession {
    pub source_hash: IdentityHash,
    pub source_nonce: [u8; IDENTITY_CLONE_NONCE_LEN],
    pub peer_keys: Option<PublicIdentityMaterial>,
    pub transcript: Option<IdentityCloneTranscript>,
    pub accepted: bool,
    pub payload: Option<IdentityClonePayload>,
    pub alias: String,
    /// The adoptee stored the operator copy. The adopting app still has to
    /// record the sibling; that happens off the request handler.
    pub handoff_complete: bool,
    pub last_announce: Instant,
}

pub struct IdentityClonePayload {
    pub encoded: Vec<u8>,
}

pub struct DestCloneSession {
    pub announce: IdentityCloneAnnounce,
    pub destination: DestinationHash,
    pub dest_nonce: Option<[u8; IDENTITY_CLONE_NONCE_LEN]>,
    pub transcript: Option<IdentityCloneTranscript>,
    pub accepted: bool,
    pub hello_in_flight: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SiblingControllerView {
    pub instance_hash: String,
    /// Set when other siblings find this one at a destination other than the identity hash.
    pub sync_address: Option<String>,
    pub last_synced: String,
    pub alias: String,
    pub heard: bool,
    pub is_self: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityCloneView {
    pub locality: CloneLocality,
    pub dest_waiting: bool,
    pub dest_accepted: bool,
    pub incoming_source: Option<String>,
    pub incoming_digits: Option<String>,
    pub outgoing_dest: Option<String>,
    pub outgoing_digits: Option<String>,
    pub source_accepted: bool,
    pub in_progress: bool,
    pub siblings: Vec<SiblingControllerView>,
    pub notice: Option<String>,
    pub error: Option<String>,
}

pub struct IdentityClone;

impl RequestEndpoint<ControllerAppState> for IdentityClone {
    const ENDPOINT_ID: &'static str = IDENTITY_CLONE_REQUEST_ENDPOINT_ID;
    const POLICY: RequestEndpointPolicy = RequestEndpointPolicy::AllowAll;

    async fn handle(
        mut context: RequestContext<'_, ControllerAppState>,
        _node: &impl personal_rns::runtime::PrnsNodeApi,
    ) -> Result<(), Decline> {
        let inbound = parse_identity_clone_message(context.data);
        let mut reply = [0u8; 8];
        let Ok(mut shared) = context.state.clone.lock() else {
            return context.respond([
                IdentityCloneMessageKind::Error as u8,
                IdentityCloneErrorCode::Malformed as u8,
            ]);
        };
        match inbound {
            Some(IdentityCloneInbound::Hello(hello)) => {
                let written = match shared.source.as_mut() {
                    Some(source) => match hello.verify(source.source_hash, source.source_nonce) {
                        Some(transcript) => {
                            source.peer_keys = Some(hello.dest_keys());
                            source.transcript = Some(transcript);
                            write_hello_ack(&mut reply)
                        }
                        None => {
                            write_clone_error(IdentityCloneErrorCode::InvalidSignature, &mut reply)
                        }
                    },
                    None => write_clone_error(IdentityCloneErrorCode::NotOffering, &mut reply),
                };
                drop(shared);
                return respond_bytes(&mut context, written, &reply);
            }
            Some(IdentityCloneInbound::Accept(accept)) => {
                let Some(source) = shared.source.as_mut() else {
                    drop(shared);
                    return respond_error(&mut context, IdentityCloneErrorCode::NotOffering);
                };
                let ready = source
                    .peer_keys
                    .as_ref()
                    .zip(source.transcript)
                    .is_some_and(|(keys, transcript)| accept.verify(keys, transcript));
                if !ready {
                    drop(shared);
                    return respond_error(&mut context, IdentityCloneErrorCode::SessionMismatch);
                }
                if !source.accepted {
                    let written = write_waiting_for_source(&mut reply);
                    drop(shared);
                    return respond_bytes(&mut context, written, &reply);
                }
                let Some(payload) = source.payload.as_ref() else {
                    let written = write_waiting_for_source(&mut reply);
                    drop(shared);
                    return respond_bytes(&mut context, written, &reply);
                };
                let total = u32::try_from(payload.encoded.len()).unwrap_or(u32::MAX);
                let mut chunk = vec![0u8; 9 + CLONE_PAYLOAD_CHUNK_LEN];
                let Some(len) =
                    write_payload_chunk(accept.offset(), total, &payload.encoded, &mut chunk)
                else {
                    drop(shared);
                    return respond_error(&mut context, IdentityCloneErrorCode::Malformed);
                };
                drop(shared);
                chunk.truncate(len);
                return context.respond(chunk);
            }
            Some(IdentityCloneInbound::Done(accept)) => {
                let Some(source) = shared.source.as_mut() else {
                    drop(shared);
                    return respond_error(&mut context, IdentityCloneErrorCode::NotOffering);
                };
                let ready = source
                    .peer_keys
                    .as_ref()
                    .zip(source.transcript)
                    .is_some_and(|(keys, transcript)| accept.verify(keys, transcript));
                if !ready || !source.accepted {
                    drop(shared);
                    return respond_error(&mut context, IdentityCloneErrorCode::SessionMismatch);
                }
                source.handoff_complete = true;
                shared.notice = Some(
                    "The other install has the Operator. It should quit and reopen.".to_string(),
                );
                shared.error = None;
                drop(shared);
                return context.respond([IdentityCloneMessageKind::Done as u8]);
            }
            Some(_) | None => {
                drop(shared);
                respond_error(&mut context, IdentityCloneErrorCode::Malformed)
            }
        }
    }
}

fn respond_bytes(
    context: &mut RequestContext<'_, ControllerAppState>,
    written: Option<usize>,
    reply: &[u8],
) -> Result<(), Decline> {
    let Some(len) = written else {
        return respond_error(context, IdentityCloneErrorCode::Malformed);
    };
    context.respond(&reply[..len])
}

fn respond_error(
    context: &mut RequestContext<'_, ControllerAppState>,
    code: IdentityCloneErrorCode,
) -> Result<(), Decline> {
    context.respond([IdentityCloneMessageKind::Error as u8, code as u8])
}

pub fn write_clone_payload(
    secret: &[u8; IDENTITY_SECRET_KEY_LEN],
    accesses: &[u8],
    clock: u64,
    siblings: &[PublicIdentityMaterial],
    labels: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    let access_len = u32::try_from(accesses.len()).ok()?;
    let sibling_count = u8::try_from(siblings.len()).ok()?;
    let sibling_bytes = siblings.len().saturating_mul(IDENTITY_PUBLIC_KEY_LEN);
    let required =
        1 + IDENTITY_SECRET_KEY_LEN + 4 + accesses.len() + 8 + 1 + sibling_bytes + labels.len();
    if out.len() < required {
        return None;
    }
    out[0] = IdentityCloneMessageKind::Payload as u8;
    out[1..1 + IDENTITY_SECRET_KEY_LEN].copy_from_slice(secret);
    let mut at = 1 + IDENTITY_SECRET_KEY_LEN;
    out[at..at + 4].copy_from_slice(&access_len.to_le_bytes());
    at += 4;
    out[at..at + accesses.len()].copy_from_slice(accesses);
    at += accesses.len();
    out[at..at + 8].copy_from_slice(&clock.to_le_bytes());
    at += 8;
    out[at] = sibling_count;
    at += 1;
    for sibling in siblings {
        out[at..at + IDENTITY_PUBLIC_KEY_LEN].copy_from_slice(sibling.as_bytes());
        at += IDENTITY_PUBLIC_KEY_LEN;
    }
    out[at..at + labels.len()].copy_from_slice(labels);
    Some(required)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::InterfacePeer;
    use personal_rns::interfaces::{InterfaceMode, PeerDetails, RadioIndication};

    fn interface(kind: &str, power: InterfacePower, peers: usize) -> InterfaceEntry {
        InterfaceEntry {
            id: "00".repeat(8),
            name: kind.to_string(),
            kind: kind.to_string(),
            power,
            mode: InterfaceMode::Full,
            connection: String::new(),
            group: None,
            tx_bytes: 0,
            rx_bytes: 0,
            tx_bps: None,
            rx_bps: None,
            links: 0,
            transported_links: 0,
            destinations: 0,
            rate_bytes_per_sec: 0,
            gravity: None,
            ifac_bytes: None,
            last_activity_secs: None,
            detail: None,
            failure: None,
            extras: Vec::new(),
            shows_peers: peers > 0,
            peers_pending: false,
            peers_error: None,
            arrived_at: None,
            peers: (0..peers)
                .map(|index| InterfacePeer {
                    id: format!("{index:016x}"),
                    name: format!("peer {index}"),
                    role: "Peer".to_string(),
                    detail: String::new(),
                    endpoint: None,
                    endpoint_label: None,
                    local_endpoint: None,
                    local_endpoint_label: None,
                    alias: None,
                    connection: String::new(),
                    health: None,
                    tx_bytes: 0,
                    rx_bytes: 0,
                    links: 0,
                    destinations: 0,
                    rate_bytes_per_sec: 0,
                    last_activity_secs: None,
                    radio: RadioIndication::NotRadio,
                    details: PeerDetails::NotApplicable,
                })
                .collect(),
        }
    }

    fn secret(fill: u8) -> PrivateIdentityMaterial {
        PrivateIdentityMaterial::from_bytes([fill; IDENTITY_SECRET_KEY_LEN])
    }

    #[test]
    fn usb_only_with_one_peer_is_ready() {
        assert_eq!(
            evaluate_clone_locality(&[
                interface("usb-auto-host", InterfacePower::On, 1),
                interface("bluetooth-auto", InterfacePower::Off, 2),
            ]),
            CloneLocality::Ready,
        );
    }

    #[test]
    fn ble_on_blocks_clone() {
        assert_eq!(
            evaluate_clone_locality(&[
                interface("usb-auto-host", InterfacePower::On, 1),
                interface("bluetooth-auto", InterfacePower::On, 0),
            ]),
            CloneLocality::ExtraTransportOn {
                kind: CloneTransportKind::Bluetooth
            },
        );
    }

    #[test]
    fn two_usb_peers_are_ambiguous() {
        assert_eq!(
            evaluate_clone_locality(&[interface("usb-auto-host", InterfacePower::On, 2)]),
            CloneLocality::UsbPeerAmbiguous { peers: 2 },
        );
    }

    #[test]
    fn connected_usb_host_without_fleet_members_is_ready() {
        let mut usb = interface("usb-auto-host", InterfacePower::On, 0);
        usb.connection = "Connected".to_string();
        assert_eq!(evaluate_clone_locality(&[usb]), CloneLocality::Ready);
    }

    #[test]
    fn waiting_usb_host_without_fleet_members_is_missing() {
        let mut usb = interface("usb-auto-host", InterfacePower::On, 0);
        usb.connection = "Waiting".to_string();
        assert_eq!(
            evaluate_clone_locality(&[usb]),
            CloneLocality::UsbPeerMissing
        );
    }

    #[test]
    fn clone_session_is_idle_after_dest_clears() {
        let mut shared = IdentityCloneShared {
            dest_waiting: true,
            notice: Some("waiting".to_string()),
            ..IdentityCloneShared::default()
        };
        assert!(shared.in_progress());
        shared.dest_waiting = false;
        shared.dest = None;
        shared.notice = Some("adopted".to_string());
        assert!(!shared.in_progress());
    }

    #[test]
    fn hello_accept_and_payload_round_trip() {
        let source = secret(0x11);
        let dest = secret(0x22);
        let source_nonce = [0x31; IDENTITY_CLONE_NONCE_LEN];
        let dest_nonce = [0x32; IDENTITY_CLONE_NONCE_LEN];
        let announce = IdentityCloneAnnounce::new(source.public(), source_nonce);
        let mut announce_bytes = [0u8; 128];
        let announce_len = announce.write(&mut announce_bytes).unwrap();
        let parsed_announce =
            IdentityCloneAnnounce::parse(&announce_bytes[..announce_len]).unwrap();
        assert_eq!(parsed_announce.source_keys(), source.public());
        let hello = IdentityCloneHello::sign(
            &dest,
            parsed_announce.source(),
            parsed_announce.source_nonce(),
            dest_nonce,
        );
        let mut hello_bytes = [0u8; 256];
        let hello_len = hello.write(&mut hello_bytes).unwrap();
        let IdentityCloneInbound::Hello(parsed) =
            parse_identity_clone_message(&hello_bytes[..hello_len]).unwrap()
        else {
            panic!("hello");
        };
        let transcript = parsed
            .verify(parsed_announce.source(), parsed_announce.source_nonce())
            .unwrap();
        let accept = IdentityCloneAccept::sign(&dest, transcript);
        let mut accept_bytes = [0u8; 256];
        let accept_len = accept.write(&mut accept_bytes).unwrap();
        let IdentityCloneInbound::Accept(parsed_accept) =
            parse_identity_clone_message(&accept_bytes[..accept_len]).unwrap()
        else {
            panic!("accept");
        };
        assert!(parsed_accept.verify(&dest.public(), transcript));
        assert_eq!(transcript.confirmation_code().to_string().len(), 6);
        let secret_bytes = [0x55; IDENTITY_SECRET_KEY_LEN];
        let sibling = secret(0x66).public();
        let mut payload = [0u8; 256];
        let payload_len =
            write_clone_payload(&secret_bytes, b"snap", 7, &[sibling], &[], &mut payload).unwrap();
        match parse_identity_clone_message(&payload[..payload_len]).unwrap() {
            IdentityCloneInbound::Payload {
                secret: parsed_secret,
                accesses,
                clock,
                siblings,
                labels,
            } => {
                assert_eq!(parsed_secret, &secret_bytes);
                assert_eq!(accesses, b"snap");
                assert_eq!(clock, 7);
                assert_eq!(parse_clone_siblings(siblings).unwrap(), vec![sibling]);
                assert!(labels.is_empty());
            }
            _ => panic!("payload"),
        }
    }

    #[test]
    fn clone_payload_keeps_trailing_roster_labels() {
        let secret_bytes = [0x55; IDENTITY_SECRET_KEY_LEN];
        let sibling = secret(0x66).public();
        let mut labels = Vec::new();
        crate::roster_sync::encode_labels(
            &mut labels,
            &[crate::roster_sync::RosterLabel {
                kind: crate::roster_sync::RosterLabelKind::TargetName,
                key: "aabbccddeeff00112233445566778899".to_string(),
                value: Some("Heltec".to_string()),
                stamp: crate::roster_sync::LabelStamp {
                    millis: 4,
                    counter: 0,
                    actor: personal_rns::identity::IdentityHash::new([0; 16]),
                },
            }],
        )
        .unwrap();
        let mut payload = vec![0u8; 512];
        let payload_len =
            write_clone_payload(&secret_bytes, b"snap", 7, &[sibling], &labels, &mut payload)
                .unwrap();
        match parse_identity_clone_message(&payload[..payload_len]).unwrap() {
            IdentityCloneInbound::Payload {
                siblings,
                labels: parsed_labels,
                ..
            } => {
                assert_eq!(parse_clone_siblings(siblings).unwrap(), vec![sibling]);
                let decoded = crate::roster_sync::decode_labels(parsed_labels).unwrap();
                assert_eq!(decoded.len(), 1);
                assert_eq!(decoded[0].value.as_deref(), Some("Heltec"));
            }
            _ => panic!("payload"),
        }
    }
}
