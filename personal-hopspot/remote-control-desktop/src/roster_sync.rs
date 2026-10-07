//! Controller-app roster replica. Bound to the Instance destination, not Operator.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use personal_rns::crypto::{sha256_chunks, Ed25519Signature};
use personal_rns::identity::{
    IdentityHash, PrivateIdentityMaterial, PublicIdentityMaterial, Zeroizing,
    IDENTITY_PUBLIC_KEY_LEN, IDENTITY_SECRET_KEY_LEN,
};
use personal_rns::prelude::{
    Decline, RemoteControlRequestKind, RemoteControlRequestSet, RemoteControlTargetAccess,
    RemoteControlTargetIdentity, RequestContext, RequestEndpoint, RequestEndpointPolicy,
};
use personal_rns::remote_control::RemoteControlControllerAuthority;
use personal_rns::routing::announce::{derive_destination_hash, expand_name};
use personal_rns::wire::DestinationHash;

use crate::identity_clone::ControllerAppState;

pub const ROSTER_SYNC_APP_NAME: &str = "prns";
pub const ROSTER_SYNC_ASPECTS: &[&str] = &["controller", "roster-sync"];
pub const ROSTER_SYNC_REQUEST_ENDPOINT_ID: &str = "/prns/controller/roster-sync";

const HASH_LEN: usize = 16;
const SIGNATURE_LEN: usize = 64;
const REPLICA_DOMAIN: &[u8] = b"reticulum.controller.roster.replica.v3";
const LOOKING_HOLD_MS: u64 = 10 * 60 * 1000;
const PULL_DOMAIN: &[u8] = b"reticulum.controller.roster.pull.v1";
const CONTROLLER_USB_HOST_DESKTOP: [u8; 8] = [0xD0; 8];
const CONTROLLER_USB_HOST_ANDROID: [u8; 8] = [0xD1; 8];
const SIBLING_REMOVED_MARK: &str = "removed";
/// Local display name for a peer that is *this* install. Must never ride roster sync.
pub const THIS_CONTROLLER_PEER_ALIAS: &str = "This controller";
/// `peer-alias-links` value for [`THIS_CONTROLLER_PEER_ALIAS`] rows (local-only).
pub const THIS_CONTROLLER_ALIAS_LINK: &str = "__this_controller__";
/// Local display name for this install's Auto Wi-Fi dial to the default gateway.
pub const AUTO_GATEWAY_PEER_ALIAS: &str = "Auto gateway";
/// `peer-alias-links` value for [`AUTO_GATEWAY_PEER_ALIAS`] rows (local-only).
pub const AUTO_GATEWAY_ALIAS_LINK: &str = "__auto_gateway__";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RosterMessageKind {
    Pull = 1,
    Replica = 2,
    Error = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RosterErrorCode {
    NotSibling = 1,
    Malformed = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterLabelKind {
    TargetName,
    TargetAlias,
    ManagerAlias,
    PeerAlias,
    SiblingAlias,
    SiblingRemoved,
    PairingAdvertDismissed,
    TargetLooking,
    /// Instance hash → that sibling's wifi link-local (`fe80::…`). Only the signer may publish their own.
    SiblingWifiLl,
}

impl RosterLabelKind {
    const fn wire(self) -> u8 {
        match self {
            Self::TargetName => 1,
            Self::TargetAlias => 2,
            Self::ManagerAlias => 3,
            Self::PeerAlias => 4,
            Self::SiblingAlias => 5,
            Self::SiblingRemoved => 6,
            Self::PairingAdvertDismissed => 7,
            Self::TargetLooking => 8,
            Self::SiblingWifiLl => 9,
        }
    }

    fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::TargetName),
            2 => Some(Self::TargetAlias),
            3 => Some(Self::ManagerAlias),
            4 => Some(Self::PeerAlias),
            5 => Some(Self::SiblingAlias),
            6 => Some(Self::SiblingRemoved),
            7 => Some(Self::PairingAdvertDismissed),
            8 => Some(Self::TargetLooking),
            9 => Some(Self::SiblingWifiLl),
            _ => None,
        }
    }

    fn token(self) -> &'static str {
        match self {
            Self::TargetName => "target-name",
            Self::TargetAlias => "target-alias",
            Self::ManagerAlias => "manager-alias",
            Self::PeerAlias => "peer-alias",
            Self::SiblingAlias => "sibling-alias",
            Self::SiblingRemoved => "sibling-removed",
            Self::PairingAdvertDismissed => "pairing-advert",
            Self::TargetLooking => "target-looking",
            Self::SiblingWifiLl => "sibling-wifi-ll",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        match token {
            "target-name" => Some(Self::TargetName),
            "target-alias" => Some(Self::TargetAlias),
            "manager-alias" => Some(Self::ManagerAlias),
            "peer-alias" => Some(Self::PeerAlias),
            "sibling-alias" => Some(Self::SiblingAlias),
            "sibling-removed" => Some(Self::SiblingRemoved),
            "pairing-advert" => Some(Self::PairingAdvertDismissed),
            "target-looking" => Some(Self::TargetLooking),
            "sibling-wifi-ll" => Some(Self::SiblingWifiLl),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAttention {
    Free,
    HeldByThis,
    HeldBySibling,
}

#[must_use]
pub fn looking_instance<'a>(replica: &'a RosterReplica, target_id: &str) -> Option<&'a str> {
    let target_id = target_id.trim();
    let lease = replica
        .leases
        .iter()
        .find(|lease| lease.target_id.eq_ignore_ascii_case(target_id))?;
    if lease.deadline_ms <= wall_millis() || lease.holder.trim().is_empty() {
        return None;
    }
    Some(lease.holder.as_str())
}

#[must_use]
pub fn attention_for_target(
    replica: &RosterReplica,
    target_id: &str,
    instance_hex: &str,
) -> TargetAttention {
    match looking_instance(replica, target_id) {
        None => TargetAttention::Free,
        Some(holder) if holder.eq_ignore_ascii_case(instance_hex.trim()) => {
            TargetAttention::HeldByThis
        }
        Some(_) => TargetAttention::HeldBySibling,
    }
}

/// Hybrid logical clock for labels and grants. Membership dots do not use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelStamp {
    pub millis: u64,
    pub counter: u32,
    pub actor: IdentityHash,
}

impl PartialOrd for LabelStamp {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LabelStamp {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.millis
            .cmp(&other.millis)
            .then(self.counter.cmp(&other.counter))
            .then_with(|| self.actor.as_bytes().cmp(other.actor.as_bytes()))
    }
}

impl Default for LabelStamp {
    fn default() -> Self {
        Self {
            millis: 0,
            counter: 0,
            actor: IdentityHash::new([0; HASH_LEN]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dot {
    pub counter: u64,
    pub actor: IdentityHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterLabel {
    pub kind: RosterLabelKind,
    pub key: String,
    pub value: Option<String>,
    pub stamp: LabelStamp,
}

/// Who is on the ten-minute Connect hold. Expiry does not remove a managed node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookingLease {
    pub target_id: String,
    pub holder: String,
    pub deadline_ms: u64,
    pub stamp: LabelStamp,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Member {
    pub access: RemoteControlTargetAccess,
    pub access_stamp: LabelStamp,
    pub adds: Vec<Dot>,
    pub removes: Vec<Dot>,
}

impl Clone for Member {
    fn clone(&self) -> Self {
        Self {
            access: clone_access(&self.access),
            access_stamp: self.access_stamp,
            adds: self.adds.clone(),
            removes: self.removes.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RosterReplica {
    /// This install's instance hash. New dots are minted under it.
    pub actor: IdentityHash,
    /// Per-install dot counter. Not a shared membership clock.
    pub clock: u64,
    pub hlc_millis: u64,
    pub hlc_counter: u32,
    pub members: Vec<Member>,
    pub siblings: Vec<PublicIdentityMaterial>,
    pub labels: Vec<RosterLabel>,
    pub leases: Vec<LookingLease>,
}

impl Default for RosterReplica {
    fn default() -> Self {
        Self {
            actor: IdentityHash::new([0; HASH_LEN]),
            clock: 0,
            hlc_millis: 0,
            hlc_counter: 0,
            members: Vec::new(),
            siblings: Vec::new(),
            labels: Vec::new(),
            leases: Vec::new(),
        }
    }
}

impl Clone for RosterReplica {
    fn clone(&self) -> Self {
        Self {
            actor: self.actor,
            clock: self.clock,
            hlc_millis: self.hlc_millis,
            hlc_counter: self.hlc_counter,
            members: self.members.clone(),
            siblings: self.siblings.clone(),
            labels: self.labels.clone(),
            leases: self.leases.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RosterDelta {
    pub signer: PublicIdentityMaterial,
    pub members: Vec<Member>,
    pub siblings: Vec<PublicIdentityMaterial>,
    pub labels: Vec<RosterLabel>,
    pub leases: Vec<LookingLease>,
}

impl Clone for RosterDelta {
    fn clone(&self) -> Self {
        Self {
            signer: self.signer,
            members: self.members.clone(),
            siblings: self.siblings.clone(),
            labels: self.labels.clone(),
            leases: self.leases.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct MergePlan {
    pub clock: u64,
    pub upserts: Vec<RemoteControlTargetAccess>,
    pub forgets: Vec<IdentityHash>,
    pub labels: Vec<RosterLabel>,
    pub leases: Vec<LookingLease>,
}

pub struct RosterShared {
    pub replica: RosterReplica,
    pub pending: Vec<RosterDelta>,
    pub instance_secret: Option<Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>>,
    pub heard_siblings: Vec<IdentityHash>,
    pub pull_due: Vec<IdentityHash>,
    pub applied_generation: u64,
    /// Bumps when a sync changes the managed-node rows the list paints from a snapshot.
    pub managed_list_generation: u64,
    pub sync_round: u64,
    pub force_sync: bool,
    pub sync_notify: std::sync::Arc<tokio::sync::Notify>,
    /// Unix milliseconds of the last completed roster exchange, keyed by sibling identity hash.
    pub last_synced_ms: HashMap<String, u64>,
}

impl Default for RosterShared {
    fn default() -> Self {
        Self {
            replica: RosterReplica::default(),
            pending: Vec::new(),
            instance_secret: None,
            heard_siblings: Vec::new(),
            pull_due: Vec::new(),
            applied_generation: 0,
            managed_list_generation: 0,
            sync_round: 0,
            force_sync: false,
            sync_notify: std::sync::Arc::new(tokio::sync::Notify::new()),
            last_synced_ms: HashMap::new(),
        }
    }
}

impl RosterShared {
    pub fn note_heard_sibling(&mut self, hash: IdentityHash) {
        push_unique_hash(&mut self.heard_siblings, hash);
        push_unique_hash(&mut self.pull_due, hash);
        self.sync_notify.notify_one();
    }

    #[allow(dead_code)] // read by the paused automatic roster sync
    pub fn take_pull_due(&mut self) -> Vec<IdentityHash> {
        std::mem::take(&mut self.pull_due)
    }

    #[must_use]
    pub fn heard_sibling(&self, hash: IdentityHash) -> bool {
        self.heard_siblings.contains(&hash)
    }

    pub fn note_roster_sync(&mut self, peer: IdentityHash, self_hash: IdentityHash) {
        let now = unix_millis_now();
        self.last_synced_ms.insert(encode_hex(peer.as_bytes()), now);
        self.last_synced_ms
            .insert(encode_hex(self_hash.as_bytes()), now);
    }
}

fn unix_millis_now() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

/// Destination other siblings path-request to reach this sibling. `None` when that
/// address is the identity hash itself.
pub fn roster_sync_address_hex(identity: IdentityHash) -> Option<String> {
    let address = encode_hex(roster_sync_destination_hash(identity)?.as_bytes());
    let identity_hex = encode_hex(identity.as_bytes());
    (address != identity_hex).then_some(address)
}

fn push_unique_hash(list: &mut Vec<IdentityHash>, hash: IdentityHash) {
    if !list.contains(&hash) {
        list.push(hash);
    }
}

fn instance_material(shared: &RosterShared) -> Option<PrivateIdentityMaterial> {
    shared
        .instance_secret
        .as_ref()
        .map(|secret| PrivateIdentityMaterial::from_bytes(**secret))
}

pub fn roster_sync_destination_hash(identity: IdentityHash) -> Option<DestinationHash> {
    let name = expand_name(ROSTER_SYNC_APP_NAME, ROSTER_SYNC_ASPECTS).ok()?;
    Some(derive_destination_hash(&identity, &name))
}

pub fn pin_sibling(replica: &mut RosterReplica, keys: PublicIdentityMaterial) {
    if replica
        .siblings
        .iter()
        .any(|sibling| sibling.identity_hash() == keys.identity_hash())
    {
        return;
    }
    replica.siblings.push(keys);
}

pub fn adopt_sibling(replica: &mut RosterReplica, keys: PublicIdentityMaterial) {
    let key = encode_hex(keys.identity_hash().as_bytes());
    note_local_label(replica, RosterLabelKind::SiblingRemoved, &key, None);
    pin_sibling(replica, keys);
}

pub fn forget_sibling_locally(replica: &mut RosterReplica, hash: IdentityHash) {
    replica
        .siblings
        .retain(|sibling| sibling.identity_hash() != hash);
    let key = encode_hex(hash.as_bytes());
    note_local_label(
        replica,
        RosterLabelKind::SiblingRemoved,
        &key,
        Some(SIBLING_REMOVED_MARK),
    );
    note_local_label(replica, RosterLabelKind::SiblingAlias, &key, None);
    note_local_label(replica, RosterLabelKind::SiblingWifiLl, &key, None);
}

fn sibling_is_removed(replica: &RosterReplica, hash: IdentityHash) -> bool {
    let key = encode_hex(hash.as_bytes());
    replica.labels.iter().any(|label| {
        label.kind == RosterLabelKind::SiblingRemoved
            && label.key == key
            && label
                .value
                .as_deref()
                .is_some_and(|value| !value.is_empty())
    })
}

pub fn bind_actor(replica: &mut RosterReplica, actor: IdentityHash) {
    replica.actor = actor;
}

pub fn forget_target_locally(replica: &mut RosterReplica, target: IdentityHash) {
    if let Some(member) = member_mut(replica, target) {
        let adds = member.adds.clone();
        for dot in adds {
            if !member.removes.contains(&dot) {
                member.removes.push(dot);
            }
        }
    }
    let key = encode_hex(target.as_bytes());
    note_local_label(replica, RosterLabelKind::TargetName, &key, None);
    note_local_label(replica, RosterLabelKind::TargetAlias, &key, None);
    release_looking_lease(replica, &key);
}

#[must_use]
pub fn replica_forgets_target(replica: &RosterReplica, hash: IdentityHash) -> bool {
    match member(replica, hash) {
        Some(member) => !member_visible(member),
        None => false,
    }
}

#[must_use]
pub fn replica_known_targets(replica: &RosterReplica) -> Vec<IdentityHash> {
    let mut ids = replica
        .members
        .iter()
        .filter(|member| member_visible(member))
        .map(|member| member.access.target().identity_hash())
        .collect::<Vec<_>>();
    ids.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    ids.dedup();
    ids
}

/// Record or refresh managed-target authorization. A new dot is not covered by an older remove.
pub fn note_local_upsert(replica: &mut RosterReplica, access: RemoteControlTargetAccess) {
    replica.clock = replica.clock.saturating_add(1);
    let dot = Dot {
        counter: replica.clock,
        actor: replica.actor,
    };
    let stamp = bump_stamp(replica);
    let hash = access.target().identity_hash();
    if let Some(member) = member_mut(replica, hash) {
        member.access = access;
        member.access_stamp = stamp;
        member.adds.push(dot);
    } else {
        replica.members.push(Member {
            access,
            access_stamp: stamp,
            adds: vec![dot],
            removes: Vec::new(),
        });
    }
}

#[must_use]
pub fn access_for(
    replica: &RosterReplica,
    hash: IdentityHash,
) -> Option<&RemoteControlTargetAccess> {
    let member = member(replica, hash)?;
    member_visible(member).then_some(&member.access)
}

/// Import engine snapshot rows into the roster (migration from AssembledRemoteControl storage).
pub fn hydrate_accesses_from_snapshot(replica: &mut RosterReplica, snapshot: &[u8]) -> bool {
    if snapshot.is_empty() {
        return false;
    }
    let Ok(persisted) =
        personal_rns::persistence::read_remote_control_target_accesses_snapshot(snapshot)
    else {
        return false;
    };
    let mut changed = false;
    for access in persisted {
        let hash = access.target().identity_hash();
        if replica_forgets_target(replica, hash) {
            continue;
        }
        if member(replica, hash).is_some_and(member_visible) {
            continue;
        }
        note_local_upsert(replica, access);
        changed = true;
    }
    changed
}

/// Returns whether the replica changed. An unchanged fact does not bump the clock.
pub fn note_local_label(
    replica: &mut RosterReplica,
    kind: RosterLabelKind,
    key: &str,
    value: Option<&str>,
) -> bool {
    let key = key.trim();
    if key.is_empty() {
        return false;
    }
    if kind == RosterLabelKind::PeerAlias && !peer_alias_is_syncable(key) {
        return false;
    }
    let next_value = value
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned);
    // Refuse to *publish* the local-only "This Controller" string. Clears (None) stay allowed
    // so a prior mistaken seed can be retracted to siblings.
    if kind == RosterLabelKind::PeerAlias && !peer_alias_value_is_syncable(next_value.as_deref()) {
        return false;
    }
    if kind == RosterLabelKind::TargetLooking {
        match next_value.as_deref() {
            Some(holder) => {
                note_looking_lease(replica, key, holder, wall_millis() + LOOKING_HOLD_MS)
            }
            None => release_looking_lease(replica, key),
        }
        return true;
    }
    if replica
        .labels
        .iter()
        .any(|label| label.kind == kind && label.key == key && label.value == next_value)
    {
        return false;
    }
    let stamp = bump_stamp(replica);
    upsert_label(
        replica,
        RosterLabel {
            kind,
            key: key.to_owned(),
            value: next_value,
            stamp,
        },
    );
    true
}

pub fn import_seed_labels(
    replica: &mut RosterReplica,
    kind: RosterLabelKind,
    values: &std::collections::HashMap<String, String>,
) {
    for (key, value) in values {
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || value.is_empty() {
            continue;
        }
        if kind == RosterLabelKind::PeerAlias
            && (!peer_alias_is_syncable(key) || !peer_alias_value_is_syncable(Some(value)))
        {
            continue;
        }
        if label_stamp(replica, kind, key).is_some() {
            continue;
        }
        note_local_label(replica, kind, key, Some(value));
    }
}

#[must_use]
pub fn peer_alias_is_syncable(peer_id: &str) -> bool {
    match parse_hex::<8>(peer_id) {
        Ok(bytes) => bytes != CONTROLLER_USB_HOST_DESKTOP && bytes != CONTROLLER_USB_HOST_ANDROID,
        Err(()) => true,
    }
}

/// "This controller" and "Auto gateway" name paths on *this* install — never shared facts.
#[must_use]
pub fn peer_alias_value_is_syncable(value: Option<&str>) -> bool {
    !value
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .is_some_and(|name| {
            name.eq_ignore_ascii_case(THIS_CONTROLLER_PEER_ALIAS)
                || name.eq_ignore_ascii_case(AUTO_GATEWAY_PEER_ALIAS)
        })
}

/// Links owned by this-controller / sibling-controller auto-fill stay off the roster.
#[must_use]
pub fn peer_alias_link_is_local_only(
    link: &str,
    sibling_instance_hashes: impl IntoIterator<Item = impl AsRef<str>>,
) -> bool {
    let link = link.trim();
    if link.is_empty() {
        return false;
    }
    if link == THIS_CONTROLLER_ALIAS_LINK || link == AUTO_GATEWAY_ALIAS_LINK {
        return true;
    }
    sibling_instance_hashes
        .into_iter()
        .any(|hash| link.eq_ignore_ascii_case(hash.as_ref().trim()))
}

/// Retract any PeerAlias rows whose value is local-only so siblings stop showing them.
pub fn retract_unsyncable_peer_alias_values(replica: &mut RosterReplica) {
    let bad_keys = replica
        .labels
        .iter()
        .filter(|label| {
            label.kind == RosterLabelKind::PeerAlias
                && !peer_alias_value_is_syncable(label.value.as_deref())
        })
        .map(|label| label.key.clone())
        .collect::<Vec<_>>();
    for key in bad_keys {
        note_local_label(replica, RosterLabelKind::PeerAlias, &key, None);
    }
}

/// An install's name for itself is local display only ("This controller").
#[must_use]
pub fn sibling_alias_is_syncable(key: &str, instance_hex: &str) -> bool {
    let key = key.trim();
    let instance_hex = instance_hex.trim();
    !key.is_empty() && !instance_hex.is_empty() && !key.eq_ignore_ascii_case(instance_hex)
}

pub fn strip_local_sibling_alias(replica: &mut RosterReplica, instance_hex: &str) {
    replica.labels.retain(|label| {
        label.kind != RosterLabelKind::SiblingAlias
            || sibling_alias_is_syncable(&label.key, instance_hex)
    });
}

#[must_use]
pub fn next_sibling_alias(aliases: &HashMap<String, String>) -> String {
    let mut taken = aliases
        .values()
        .filter_map(|value| sibling_alias_number(value))
        .collect::<Vec<_>>();
    taken.sort_unstable();
    taken.dedup();
    let mut number = 1u32;
    for used in taken {
        if used == number {
            number = number.saturating_add(1);
        }
    }
    format!("Sibling {number}")
}

fn sibling_alias_number(value: &str) -> Option<u32> {
    let rest = value.trim().strip_prefix("Sibling ")?;
    if rest.is_empty() || !rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// Lowest unused `Alias <n>` among current managed-node aliases (custom names are ignored).
#[must_use]
pub fn next_target_alias(aliases: &HashMap<String, String>) -> String {
    let mut taken = aliases
        .values()
        .filter_map(|value| target_alias_number(value))
        .collect::<Vec<_>>();
    taken.sort_unstable();
    taken.dedup();
    let mut number = 1u32;
    for used in taken {
        if used == number {
            number = number.saturating_add(1);
        }
    }
    format!("Alias {number}")
}

fn target_alias_number(value: &str) -> Option<u32> {
    let rest = value.trim().strip_prefix("Alias ")?;
    if rest.is_empty() || !rest.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

fn wall_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn bump_stamp(replica: &mut RosterReplica) -> LabelStamp {
    let millis = wall_millis();
    if millis > replica.hlc_millis {
        replica.hlc_millis = millis;
        replica.hlc_counter = 0;
    } else {
        replica.hlc_counter = replica.hlc_counter.saturating_add(1);
    }
    LabelStamp {
        millis: replica.hlc_millis,
        counter: replica.hlc_counter,
        actor: replica.actor,
    }
}

fn member(replica: &RosterReplica, hash: IdentityHash) -> Option<&Member> {
    replica
        .members
        .iter()
        .find(|member| member.access.target().identity_hash() == hash)
}

fn member_mut(replica: &mut RosterReplica, hash: IdentityHash) -> Option<&mut Member> {
    replica
        .members
        .iter_mut()
        .find(|member| member.access.target().identity_hash() == hash)
}

fn member_visible(member: &Member) -> bool {
    member.adds.iter().any(|dot| !member.removes.contains(dot))
}

fn note_looking_lease(
    replica: &mut RosterReplica,
    target_id: &str,
    holder: &str,
    deadline_ms: u64,
) {
    let stamp = bump_stamp(replica);
    upsert_lease(
        replica,
        LookingLease {
            target_id: target_id.to_owned(),
            holder: holder.to_owned(),
            deadline_ms,
            stamp,
        },
    );
}

fn release_looking_lease(replica: &mut RosterReplica, target_id: &str) {
    let stamp = bump_stamp(replica);
    upsert_lease(
        replica,
        LookingLease {
            target_id: target_id.to_owned(),
            holder: String::new(),
            deadline_ms: 0,
            stamp,
        },
    );
}

fn join_member(replica: &mut RosterReplica, remote: &Member) {
    let hash = remote.access.target().identity_hash();
    if let Some(existing) = member_mut(replica, hash) {
        for dot in &remote.adds {
            if !existing.adds.contains(dot) {
                existing.adds.push(*dot);
            }
        }
        for dot in &remote.removes {
            if !existing.removes.contains(dot) {
                existing.removes.push(*dot);
            }
        }
        if remote.access_stamp > existing.access_stamp {
            existing.access = clone_access(&remote.access);
            existing.access_stamp = remote.access_stamp;
        }
        return;
    }
    replica.members.push(remote.clone());
}

#[must_use]
/// Membership, names, looking leases, and dismissed pairing rows live on the
/// managed-node snapshot. Alias-only labels are painted from their own maps.
pub fn plan_changes_managed_nodes(plan: &MergePlan) -> bool {
    !plan.upserts.is_empty()
        || !plan.forgets.is_empty()
        || !plan.leases.is_empty()
        || plan.labels.iter().any(|label| {
            matches!(
                label.kind,
                RosterLabelKind::TargetName
                    | RosterLabelKind::PairingAdvertDismissed
                    | RosterLabelKind::TargetLooking
            )
        })
}

pub fn merge_roster(local: &RosterReplica, delta: &RosterDelta) -> (RosterReplica, MergePlan) {
    let before = replica_known_targets(local);
    let mut merged = local.clone();
    for remote in &delta.members {
        join_member(&mut merged, remote);
    }
    let after = replica_known_targets(&merged);
    if !before.is_empty() && after.is_empty() {
        return (
            local.clone(),
            MergePlan {
                clock: local.clock,
                upserts: Vec::new(),
                forgets: Vec::new(),
                labels: Vec::new(),
                leases: Vec::new(),
            },
        );
    }
    let mut upserts = Vec::new();
    for hash in &after {
        let changed = !before.contains(hash)
            || access_for(local, *hash).map(clone_access)
                != access_for(&merged, *hash).map(clone_access);
        if changed {
            if let Some(access) = access_for(&merged, *hash) {
                upserts.push(clone_access(access));
            }
        }
    }
    let mut forgets = before
        .into_iter()
        .filter(|hash| !after.contains(hash))
        .collect::<Vec<_>>();
    forgets.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    forgets.dedup();
    let mut labels = Vec::new();
    for label in &delta.labels {
        if label.kind == RosterLabelKind::TargetLooking {
            continue;
        }
        if label.kind == RosterLabelKind::PeerAlias
            && (!peer_alias_is_syncable(&label.key)
                || !peer_alias_value_is_syncable(label.value.as_deref()))
        {
            continue;
        }
        if label.kind == RosterLabelKind::SiblingAlias {
            let signer_hex = encode_hex(delta.signer.identity_hash().as_bytes());
            if !sibling_alias_is_syncable(&label.key, &signer_hex) {
                continue;
            }
        }
        if label.kind == RosterLabelKind::SiblingWifiLl {
            let signer_hex = encode_hex(delta.signer.identity_hash().as_bytes());
            if !label.key.eq_ignore_ascii_case(&signer_hex) {
                continue;
            }
        }
        if label_stamp(&merged, label.kind, &label.key)
            .is_some_and(|current| current >= label.stamp)
        {
            continue;
        }
        upsert_label(&mut merged, label.clone());
        labels.push(label.clone());
    }
    let mut leases = Vec::new();
    for lease in &delta.leases {
        if lease_stamp(&merged, &lease.target_id).is_some_and(|current| current >= lease.stamp) {
            continue;
        }
        upsert_lease(&mut merged, lease.clone());
        leases.push(lease.clone());
    }
    for sibling in &delta.siblings {
        // The peer's pin of this install is not a sibling of this install.
        // Pinning it makes the next exchange teach the peer to pin itself.
        if sibling.identity_hash() == merged.actor
            || sibling_is_removed(&merged, sibling.identity_hash())
        {
            continue;
        }
        pin_sibling(&mut merged, *sibling);
    }
    let removed = merged
        .siblings
        .iter()
        .filter(|sibling| {
            sibling.identity_hash() == merged.actor
                || sibling_is_removed(&merged, sibling.identity_hash())
        })
        .map(|sibling| sibling.identity_hash())
        .collect::<Vec<_>>();
    merged
        .siblings
        .retain(|sibling| !removed.contains(&sibling.identity_hash()));
    let plan = MergePlan {
        clock: merged.clock,
        upserts,
        forgets,
        labels,
        leases,
    };
    (merged, plan)
}

fn label_stamp(replica: &RosterReplica, kind: RosterLabelKind, key: &str) -> Option<LabelStamp> {
    replica
        .labels
        .iter()
        .find(|label| label.kind == kind && label.key == key)
        .map(|label| label.stamp)
}

fn lease_stamp(replica: &RosterReplica, target_id: &str) -> Option<LabelStamp> {
    replica
        .leases
        .iter()
        .find(|lease| lease.target_id.eq_ignore_ascii_case(target_id))
        .map(|lease| lease.stamp)
}

fn upsert_label(replica: &mut RosterReplica, label: RosterLabel) {
    if let Some(existing) = replica
        .labels
        .iter_mut()
        .find(|candidate| candidate.kind == label.kind && candidate.key == label.key)
    {
        if existing.stamp <= label.stamp {
            *existing = label;
        }
        return;
    }
    replica.labels.push(label);
}

fn upsert_lease(replica: &mut RosterReplica, lease: LookingLease) {
    if let Some(existing) = replica
        .leases
        .iter_mut()
        .find(|candidate| candidate.target_id.eq_ignore_ascii_case(&lease.target_id))
    {
        if existing.stamp <= lease.stamp {
            *existing = lease;
        }
        return;
    }
    replica.leases.push(lease);
}

fn authority_for(requests: &RemoteControlRequestSet) -> RemoteControlControllerAuthority {
    if requests
        .iter()
        .any(|request| request.requires_administrator())
    {
        RemoteControlControllerAuthority::Administrator
    } else {
        RemoteControlControllerAuthority::Operator
    }
}

pub(crate) fn clone_access(access: &RemoteControlTargetAccess) -> RemoteControlTargetAccess {
    RemoteControlTargetAccess::new(
        RemoteControlTargetIdentity::new(*access.target().public_keys()),
        access.authority(),
        *access.permitted_requests(),
    )
    .expect("persisted access already has a request")
}

#[cfg(test)]
pub(crate) fn tests_access(fill: u8) -> RemoteControlTargetAccess {
    use personal_rns::identity::{PrivateIdentityMaterial, IDENTITY_SECRET_KEY_LEN};
    let keys = PrivateIdentityMaterial::from_bytes([fill; IDENTITY_SECRET_KEY_LEN])
        .public()
        .public_keys();
    RemoteControlTargetAccess::new(
        RemoteControlTargetIdentity::new(keys),
        RemoteControlControllerAuthority::Operator,
        RemoteControlRequestSet::only(RemoteControlRequestKind::Describe),
    )
    .unwrap()
}

pub fn load_replica(path: &Path) -> RosterReplica {
    let Ok(text) = std::fs::read_to_string(path) else {
        return RosterReplica::default();
    };
    if !text.starts_with("v3\n") {
        return RosterReplica::default();
    }
    parse_replica(&text)
}

pub fn persist_replica(path: &Path, replica: &RosterReplica) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(existing) = std::fs::read_to_string(path) {
        if !existing.is_empty() && !existing.starts_with("v3\n") {
            return;
        }
    }
    let tmp = path.with_file_name("roster-replica.tmp");
    if std::fs::write(&tmp, format_replica(replica)).is_err() {
        return;
    }
    if std::fs::rename(&tmp, path).is_err() {
        return;
    }
    let bak = path.with_file_name("roster-replica.bak");
    let _ = std::fs::copy(path, bak);
}

pub fn replica_path(data_dir: impl AsRef<Path>) -> PathBuf {
    data_dir.as_ref().join("roster-replica")
}

fn format_replica(replica: &RosterReplica) -> String {
    let mut out = format!(
        "v3\nactor {}\nclock {}\nhlc {} {}\n",
        encode_hex(replica.actor.as_bytes()),
        replica.clock,
        replica.hlc_millis,
        replica.hlc_counter
    );
    for sibling in &replica.siblings {
        out.push_str("sibling ");
        out.push_str(&encode_hex(sibling.as_bytes()));
        out.push('\n');
    }
    let mut members = replica.members.iter().collect::<Vec<_>>();
    members.sort_by(|left, right| {
        left.access
            .target()
            .identity_hash()
            .as_bytes()
            .cmp(right.access.target().identity_hash().as_bytes())
    });
    for member in members {
        out.push_str("member ");
        out.push_str(&encode_hex(
            &member.access.target().public_keys().public_key_bytes(),
        ));
        out.push(' ');
        let requests: Vec<_> = member
            .access
            .permitted_requests()
            .iter()
            .map(|kind| kind.wire_value().to_string())
            .collect();
        out.push_str(&requests.join(","));
        out.push(' ');
        out.push_str(&member.access_stamp.millis.to_string());
        out.push(' ');
        out.push_str(&member.access_stamp.counter.to_string());
        out.push(' ');
        out.push_str(&encode_hex(member.access_stamp.actor.as_bytes()));
        out.push_str(" adds ");
        out.push_str(&format_dots(&member.adds));
        out.push_str(" removes ");
        out.push_str(&format_dots(&member.removes));
        out.push('\n');
    }
    let mut labels = replica.labels.clone();
    labels.sort_by(|left, right| {
        left.kind
            .token()
            .cmp(right.kind.token())
            .then_with(|| left.key.cmp(&right.key))
    });
    for label in labels {
        out.push_str("label ");
        out.push_str(label.kind.token());
        out.push(' ');
        out.push_str(&label.stamp.millis.to_string());
        out.push(' ');
        out.push_str(&label.stamp.counter.to_string());
        out.push(' ');
        out.push_str(&encode_hex(label.stamp.actor.as_bytes()));
        out.push(' ');
        out.push_str(&label.key);
        if let Some(value) = &label.value {
            out.push('\t');
            out.push_str(value);
        }
        out.push('\n');
    }
    let mut leases = replica.leases.clone();
    leases.sort_by(|left, right| left.target_id.cmp(&right.target_id));
    for lease in leases {
        out.push_str("lease ");
        out.push_str(&lease.target_id);
        out.push(' ');
        if lease.holder.is_empty() {
            out.push('-');
        } else {
            out.push_str(&lease.holder);
        }
        out.push(' ');
        out.push_str(&lease.deadline_ms.to_string());
        out.push(' ');
        out.push_str(&lease.stamp.millis.to_string());
        out.push(' ');
        out.push_str(&lease.stamp.counter.to_string());
        out.push(' ');
        out.push_str(&encode_hex(lease.stamp.actor.as_bytes()));
        out.push('\n');
    }
    out
}

fn format_dots(dots: &[Dot]) -> String {
    if dots.is_empty() {
        return "-".to_string();
    }
    dots.iter()
        .map(|dot| format!("{}:{}", encode_hex(dot.actor.as_bytes()), dot.counter))
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_replica(text: &str) -> RosterReplica {
    let mut replica = RosterReplica::default();
    for line in text.lines() {
        let line = line.trim();
        if line == "v3" {
            continue;
        } else if let Some(hex) = line.strip_prefix("actor ") {
            if let Ok(bytes) = parse_hex::<HASH_LEN>(hex) {
                replica.actor = IdentityHash::new(bytes);
            }
        } else if let Some(clock) = line.strip_prefix("clock ") {
            if let Ok(value) = clock.parse() {
                replica.clock = value;
            }
        } else if let Some(rest) = line.strip_prefix("hlc ") {
            let mut parts = rest.split_whitespace();
            if let (Some(millis), Some(counter)) = (
                parts.next().and_then(|value| value.parse().ok()),
                parts.next().and_then(|value| value.parse().ok()),
            ) {
                replica.hlc_millis = millis;
                replica.hlc_counter = counter;
            }
        } else if let Some(hex) = line.strip_prefix("sibling ") {
            if let Ok(bytes) = parse_hex::<IDENTITY_PUBLIC_KEY_LEN>(hex) {
                if let Ok(keys) = PublicIdentityMaterial::from_slice(&bytes) {
                    pin_sibling(&mut replica, keys);
                }
            }
        } else if let Some(rest) = line.strip_prefix("member ") {
            if let Some(member) = parse_member(rest) {
                replica.members.push(member);
            }
        } else if let Some(rest) = line.strip_prefix("label ") {
            if let Some(label) = parse_label_line(rest) {
                upsert_label(&mut replica, label);
            }
        } else if let Some(rest) = line.strip_prefix("lease ") {
            if let Some(lease) = parse_lease_line(rest) {
                upsert_lease(&mut replica, lease);
            }
        }
    }
    replica
}

fn parse_member(rest: &str) -> Option<Member> {
    let (head, dots) = rest.split_once(" adds ")?;
    let (adds, removes) = dots.split_once(" removes ")?;
    let mut parts = head.split_whitespace();
    let keys = PublicIdentityMaterial::from_slice(
        &parse_hex::<IDENTITY_PUBLIC_KEY_LEN>(parts.next()?).ok()?,
    )
    .ok()?;
    let mut requests = RemoteControlRequestSet::empty();
    for token in parts.next()?.split(',') {
        let value = token.parse::<u8>().ok()?;
        let kind = request_kind_from_wire(value)?;
        let _ = requests.insert(kind);
    }
    if requests.is_empty() {
        return None;
    }
    let millis = parts.next()?.parse().ok()?;
    let counter = parts.next()?.parse().ok()?;
    let actor = IdentityHash::new(parse_hex::<HASH_LEN>(parts.next()?).ok()?);
    let access = RemoteControlTargetAccess::new(
        RemoteControlTargetIdentity::new(keys.public_keys()),
        authority_for(&requests),
        requests,
    )
    .ok()?;
    Some(Member {
        access,
        access_stamp: LabelStamp {
            millis,
            counter,
            actor,
        },
        adds: parse_dots(adds)?,
        removes: parse_dots(removes)?,
    })
}

fn parse_dots(text: &str) -> Option<Vec<Dot>> {
    if text == "-" {
        return Some(Vec::new());
    }
    let mut dots = Vec::new();
    for token in text.split(',') {
        let (actor, counter) = token.split_once(':')?;
        dots.push(Dot {
            actor: IdentityHash::new(parse_hex::<HASH_LEN>(actor).ok()?),
            counter: counter.parse().ok()?,
        });
    }
    Some(dots)
}

fn parse_label_line(rest: &str) -> Option<RosterLabel> {
    let (kind_token, rest) = rest.split_once(' ')?;
    let kind = RosterLabelKind::from_token(kind_token)?;
    let (millis, rest) = rest.split_once(' ')?;
    let (counter, rest) = rest.split_once(' ')?;
    let (actor, rest) = rest.split_once(' ')?;
    let (key, value) = match rest.split_once('\t') {
        Some((key, value)) => (
            key.trim(),
            Some(value.trim())
                .filter(|name| !name.is_empty())
                .map(ToOwned::to_owned),
        ),
        None => (rest.trim(), None),
    };
    if key.is_empty() {
        return None;
    }
    Some(RosterLabel {
        kind,
        key: key.to_owned(),
        value,
        stamp: LabelStamp {
            millis: millis.parse().ok()?,
            counter: counter.parse().ok()?,
            actor: IdentityHash::new(parse_hex::<HASH_LEN>(actor).ok()?),
        },
    })
}

fn parse_lease_line(rest: &str) -> Option<LookingLease> {
    let mut parts = rest.split_whitespace();
    let target_id = parts.next()?.to_owned();
    let holder = parts.next().unwrap_or("").to_owned();
    let deadline_ms = parts.next()?.parse().ok()?;
    let millis = parts.next()?.parse().ok()?;
    let counter = parts.next()?.parse().ok()?;
    let actor = IdentityHash::new(parse_hex::<HASH_LEN>(parts.next()?).ok()?);
    if target_id.is_empty() {
        return None;
    }
    Some(LookingLease {
        target_id,
        holder: if holder == "-" { String::new() } else { holder },
        deadline_ms,
        stamp: LabelStamp {
            millis,
            counter,
            actor,
        },
    })
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn parse_hex<const N: usize>(input: &str) -> Result<[u8; N], ()> {
    let cleaned: String = input.chars().filter(|ch| ch.is_ascii_hexdigit()).collect();
    if cleaned.len() != N * 2 {
        return Err(());
    }
    let mut out = [0u8; N];
    for (index, chunk) in cleaned.as_bytes().chunks_exact(2).enumerate() {
        let value = core::str::from_utf8(chunk).map_err(|_| ())?;
        out[index] = u8::from_str_radix(value, 16).map_err(|_| ())?;
    }
    Ok(out)
}

pub fn write_pull(signer: &PrivateIdentityMaterial, out: &mut [u8]) -> Option<usize> {
    let required = 1 + IDENTITY_PUBLIC_KEY_LEN + SIGNATURE_LEN;
    if out.len() < required {
        return None;
    }
    out[0] = RosterMessageKind::Pull as u8;
    out[1..1 + IDENTITY_PUBLIC_KEY_LEN].copy_from_slice(signer.public().as_bytes());
    let material = sha256_chunks(&[PULL_DOMAIN, signer.identity_hash().as_bytes()]);
    out[1 + IDENTITY_PUBLIC_KEY_LEN..required].copy_from_slice(&signer.sign(&material).0);
    Some(required)
}

pub fn write_replica_message(
    signer: &PrivateIdentityMaterial,
    replica: &RosterReplica,
    out: &mut [u8],
) -> Option<usize> {
    let body = encode_replica_body(replica, signer.public())?;
    let required = 1 + 8 + IDENTITY_PUBLIC_KEY_LEN + SIGNATURE_LEN + body.len();
    if out.len() < required {
        return None;
    }
    out[0] = RosterMessageKind::Replica as u8;
    out[1..9].copy_from_slice(&replica.clock.to_le_bytes());
    out[9..9 + IDENTITY_PUBLIC_KEY_LEN].copy_from_slice(signer.public().as_bytes());
    let material = replica_material(replica.clock, signer.public(), &body);
    let signature = signer.sign(&material);
    let sig_at = 9 + IDENTITY_PUBLIC_KEY_LEN;
    out[sig_at..sig_at + SIGNATURE_LEN].copy_from_slice(&signature.0);
    out[sig_at + SIGNATURE_LEN..required].copy_from_slice(&body);
    Some(required)
}

pub fn replica_message(
    signer: &PrivateIdentityMaterial,
    replica: &RosterReplica,
) -> Option<Vec<u8>> {
    let body = encode_replica_body(replica, signer.public())?;
    let required = 1 + 8 + IDENTITY_PUBLIC_KEY_LEN + SIGNATURE_LEN + body.len();
    let mut out = vec![0u8; required];
    write_replica_message(signer, replica, &mut out)?;
    Some(out)
}

fn replica_material(clock: u64, signer: PublicIdentityMaterial, body: &[u8]) -> [u8; 32] {
    sha256_chunks(&[
        REPLICA_DOMAIN,
        &clock.to_le_bytes(),
        signer.as_bytes(),
        body,
    ])
}

fn encode_replica_body(replica: &RosterReplica, signer: PublicIdentityMaterial) -> Option<Vec<u8>> {
    let siblings: Vec<_> = replica
        .siblings
        .iter()
        .filter(|sibling| sibling.identity_hash() != signer.identity_hash())
        .collect();
    let sibling_count = u16::try_from(siblings.len()).ok()?;
    let member_count = u16::try_from(replica.members.len()).ok()?;
    let mut body = Vec::new();
    body.push(3);
    body.extend_from_slice(&member_count.to_le_bytes());
    for member in &replica.members {
        encode_member(&mut body, member)?;
    }
    body.extend_from_slice(&sibling_count.to_le_bytes());
    for sibling in siblings {
        body.extend_from_slice(sibling.as_bytes());
    }
    let signer_hex = encode_hex(signer.identity_hash().as_bytes());
    let labels: Vec<_> = replica
        .labels
        .iter()
        .filter(|label| {
            label.kind != RosterLabelKind::SiblingAlias
                || sibling_alias_is_syncable(&label.key, &signer_hex)
        })
        .cloned()
        .collect();
    encode_labels(&mut body, &labels)?;
    encode_leases(&mut body, &replica.leases)?;
    Some(body)
}

pub(crate) fn encode_labels(body: &mut Vec<u8>, labels: &[RosterLabel]) -> Option<()> {
    let count = u16::try_from(labels.len()).ok()?;
    body.extend_from_slice(&count.to_le_bytes());
    for label in labels {
        body.push(label.kind.wire());
        let key = label.key.as_bytes();
        let key_len = u8::try_from(key.len()).ok()?;
        body.push(key_len);
        body.extend_from_slice(key);
        encode_stamp(body, label.stamp);
        match &label.value {
            Some(value) => {
                let bytes = value.as_bytes();
                let value_len = u16::try_from(bytes.len()).ok()?;
                body.extend_from_slice(&value_len.to_le_bytes());
                body.extend_from_slice(bytes);
            }
            None => body.extend_from_slice(&0u16.to_le_bytes()),
        }
    }
    Some(())
}

fn encode_stamp(body: &mut Vec<u8>, stamp: LabelStamp) {
    body.extend_from_slice(&stamp.millis.to_le_bytes());
    body.extend_from_slice(&stamp.counter.to_le_bytes());
    body.extend_from_slice(stamp.actor.as_bytes());
}

pub(crate) fn encode_member_list(members: &[Member]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    let count = u16::try_from(members.len()).ok()?;
    body.extend_from_slice(&count.to_le_bytes());
    for member in members {
        encode_member(&mut body, member)?;
    }
    Some(body)
}

pub(crate) fn decode_member_list(bytes: &[u8]) -> Option<Vec<Member>> {
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    let mut at = 0usize;
    let count = u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as usize;
    at += 2;
    let mut members = Vec::with_capacity(count);
    for _ in 0..count {
        let (member, consumed) = decode_member(bytes.get(at..)?)?;
        members.push(member);
        at += consumed;
    }
    if at != bytes.len() {
        return None;
    }
    Some(members)
}

fn encode_member(body: &mut Vec<u8>, member: &Member) -> Option<()> {
    body.extend_from_slice(&member.access.target().public_keys().public_key_bytes());
    encode_stamp(body, member.access_stamp);
    let requests = member.access.permitted_requests();
    let count = u8::try_from(requests.len()).ok()?;
    body.push(count);
    for kind in requests.iter() {
        body.push(kind.wire_value());
    }
    encode_dot_list(body, &member.adds)?;
    encode_dot_list(body, &member.removes)?;
    Some(())
}

fn encode_dot_list(body: &mut Vec<u8>, dots: &[Dot]) -> Option<()> {
    let count = u16::try_from(dots.len()).ok()?;
    body.extend_from_slice(&count.to_le_bytes());
    for dot in dots {
        body.extend_from_slice(dot.actor.as_bytes());
        body.extend_from_slice(&dot.counter.to_le_bytes());
    }
    Some(())
}

fn encode_leases(body: &mut Vec<u8>, leases: &[LookingLease]) -> Option<()> {
    let count = u16::try_from(leases.len()).ok()?;
    body.extend_from_slice(&count.to_le_bytes());
    for lease in leases {
        let target = lease.target_id.as_bytes();
        let holder = lease.holder.as_bytes();
        let target_len = u8::try_from(target.len()).ok()?;
        let holder_len = u8::try_from(holder.len()).ok()?;
        body.push(target_len);
        body.extend_from_slice(target);
        body.push(holder_len);
        body.extend_from_slice(holder);
        body.extend_from_slice(&lease.deadline_ms.to_le_bytes());
        encode_stamp(body, lease.stamp);
    }
    Some(())
}

fn parse_roster_message(bytes: &[u8]) -> Option<RosterInbound<'_>> {
    let (kind, rest) = bytes.split_first()?;
    match *kind {
        1 => parse_pull(rest),
        2 => parse_replica_message(rest),
        3 => {
            let (code, rest) = rest.split_first()?;
            if !rest.is_empty() {
                return None;
            }
            Some(RosterInbound::Error(*code))
        }
        _ => None,
    }
}

enum RosterInbound<'a> {
    Pull { signer: PublicIdentityMaterial },
    Replica { delta: RosterDelta, _body: &'a [u8] },
    Error(u8),
}

fn parse_pull(rest: &[u8]) -> Option<RosterInbound<'_>> {
    if rest.len() != IDENTITY_PUBLIC_KEY_LEN + SIGNATURE_LEN {
        return None;
    }
    let signer = PublicIdentityMaterial::from_slice(&rest[..IDENTITY_PUBLIC_KEY_LEN]).ok()?;
    let mut signature = [0u8; SIGNATURE_LEN];
    signature.copy_from_slice(&rest[IDENTITY_PUBLIC_KEY_LEN..]);
    let material = sha256_chunks(&[PULL_DOMAIN, signer.identity_hash().as_bytes()]);
    signer
        .verify(&material, &Ed25519Signature(signature))
        .ok()?;
    Some(RosterInbound::Pull { signer })
}

fn parse_replica_message(rest: &[u8]) -> Option<RosterInbound<'_>> {
    if rest.len() < 8 + IDENTITY_PUBLIC_KEY_LEN + SIGNATURE_LEN {
        return None;
    }
    let clock = u64::from_le_bytes(rest[..8].try_into().ok()?);
    let signer = PublicIdentityMaterial::from_slice(&rest[8..8 + IDENTITY_PUBLIC_KEY_LEN]).ok()?;
    let sig_at = 8 + IDENTITY_PUBLIC_KEY_LEN;
    let mut signature = [0u8; SIGNATURE_LEN];
    signature.copy_from_slice(&rest[sig_at..sig_at + SIGNATURE_LEN]);
    let body = &rest[sig_at + SIGNATURE_LEN..];
    let material = replica_material(clock, signer, body);
    signer
        .verify(&material, &Ed25519Signature(signature))
        .ok()?;
    let delta = decode_replica_body(clock, signer, body)?;
    Some(RosterInbound::Replica { delta, _body: body })
}

fn decode_replica_body(
    _clock: u64,
    signer: PublicIdentityMaterial,
    body: &[u8],
) -> Option<RosterDelta> {
    let mut at = 0;
    if *body.get(at)? != 3 {
        return None;
    }
    at += 1;
    let member_count = u16::from_le_bytes(body.get(at..at + 2)?.try_into().ok()?) as usize;
    at += 2;
    let mut members = Vec::with_capacity(member_count);
    for _ in 0..member_count {
        let (member, consumed) = decode_member(body.get(at..)?)?;
        members.push(member);
        at += consumed;
    }
    let sibling_count = u16::from_le_bytes(body.get(at..at + 2)?.try_into().ok()?) as usize;
    at += 2;
    let mut siblings = Vec::with_capacity(sibling_count);
    for _ in 0..sibling_count {
        siblings.push(
            PublicIdentityMaterial::from_slice(body.get(at..at + IDENTITY_PUBLIC_KEY_LEN)?).ok()?,
        );
        at += IDENTITY_PUBLIC_KEY_LEN;
    }
    let (labels, consumed) = decode_labels_prefix(body.get(at..)?)?;
    at += consumed;
    let leases = decode_leases(body.get(at..)?)?;
    Some(RosterDelta {
        signer,
        members,
        siblings,
        labels,
        leases,
    })
}

fn decode_member(body: &[u8]) -> Option<(Member, usize)> {
    let mut at = 0;
    let keys =
        PublicIdentityMaterial::from_slice(body.get(at..at + IDENTITY_PUBLIC_KEY_LEN)?).ok()?;
    at += IDENTITY_PUBLIC_KEY_LEN;
    let (access_stamp, consumed) = decode_stamp(body.get(at..)?)?;
    at += consumed;
    let request_count = *body.get(at)? as usize;
    at += 1;
    let mut requests = RemoteControlRequestSet::empty();
    for _ in 0..request_count {
        let kind = request_kind_from_wire(*body.get(at)?)?;
        let _ = requests.insert(kind);
        at += 1;
    }
    let access = RemoteControlTargetAccess::new(
        RemoteControlTargetIdentity::new(keys.public_keys()),
        authority_for(&requests),
        requests,
    )
    .ok()?;
    let (adds, consumed) = decode_dot_list(body.get(at..)?)?;
    at += consumed;
    let (removes, consumed) = decode_dot_list(body.get(at..)?)?;
    at += consumed;
    Some((
        Member {
            access,
            access_stamp,
            adds,
            removes,
        },
        at,
    ))
}

fn decode_stamp(body: &[u8]) -> Option<(LabelStamp, usize)> {
    let millis = u64::from_le_bytes(body.get(..8)?.try_into().ok()?);
    let counter = u32::from_le_bytes(body.get(8..12)?.try_into().ok()?);
    let mut actor = [0u8; HASH_LEN];
    actor.copy_from_slice(body.get(12..12 + HASH_LEN)?);
    Some((
        LabelStamp {
            millis,
            counter,
            actor: IdentityHash::new(actor),
        },
        12 + HASH_LEN,
    ))
}

fn decode_dot_list(body: &[u8]) -> Option<(Vec<Dot>, usize)> {
    let count = u16::from_le_bytes(body.get(..2)?.try_into().ok()?) as usize;
    let mut at = 2;
    let mut dots = Vec::with_capacity(count);
    for _ in 0..count {
        let mut actor = [0u8; HASH_LEN];
        actor.copy_from_slice(body.get(at..at + HASH_LEN)?);
        at += HASH_LEN;
        let counter = u64::from_le_bytes(body.get(at..at + 8)?.try_into().ok()?);
        at += 8;
        dots.push(Dot {
            actor: IdentityHash::new(actor),
            counter,
        });
    }
    Some((dots, at))
}

fn decode_leases(body: &[u8]) -> Option<Vec<LookingLease>> {
    let mut at = 0;
    let count = u16::from_le_bytes(body.get(at..at + 2)?.try_into().ok()?) as usize;
    at += 2;
    let mut leases = Vec::with_capacity(count);
    for _ in 0..count {
        let target_len = *body.get(at)? as usize;
        at += 1;
        let target_id = std::str::from_utf8(body.get(at..at + target_len)?)
            .ok()?
            .to_owned();
        at += target_len;
        let holder_len = *body.get(at)? as usize;
        at += 1;
        let holder = std::str::from_utf8(body.get(at..at + holder_len)?)
            .ok()?
            .to_owned();
        at += holder_len;
        let deadline_ms = u64::from_le_bytes(body.get(at..at + 8)?.try_into().ok()?);
        at += 8;
        let (stamp, consumed) = decode_stamp(body.get(at..)?)?;
        at += consumed;
        leases.push(LookingLease {
            target_id,
            holder,
            deadline_ms,
            stamp,
        });
    }
    if at != body.len() {
        return None;
    }
    Some(leases)
}

pub(crate) fn decode_labels(body: &[u8]) -> Option<Vec<RosterLabel>> {
    let (labels, consumed) = decode_labels_prefix(body)?;
    if consumed != body.len() {
        return None;
    }
    Some(labels)
}

fn decode_labels_prefix(body: &[u8]) -> Option<(Vec<RosterLabel>, usize)> {
    if body.is_empty() {
        return Some((Vec::new(), 0));
    }
    let mut at = 0;
    let count = u16::from_le_bytes(body.get(at..at + 2)?.try_into().ok()?) as usize;
    at += 2;
    let mut labels = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = RosterLabelKind::from_wire(*body.get(at)?);
        at += 1;
        let key_len = *body.get(at)? as usize;
        at += 1;
        let key = std::str::from_utf8(body.get(at..at + key_len)?)
            .ok()?
            .to_owned();
        at += key_len;
        let (stamp, consumed) = decode_stamp(body.get(at..)?)?;
        at += consumed;
        let value_len = u16::from_le_bytes(body.get(at..at + 2)?.try_into().ok()?) as usize;
        at += 2;
        let value = if value_len == 0 {
            None
        } else {
            Some(
                std::str::from_utf8(body.get(at..at + value_len)?)
                    .ok()?
                    .to_owned(),
            )
        };
        at += value_len;
        let Some(kind) = kind else {
            continue;
        };
        labels.push(RosterLabel {
            kind,
            key,
            value,
            stamp,
        });
    }
    Some((labels, at))
}

fn request_kind_from_wire(value: u8) -> Option<RemoteControlRequestKind> {
    RemoteControlRequestKind::ALL
        .into_iter()
        .find(|kind| kind.wire_value() == value)
}

fn is_sibling(replica: &RosterReplica, keys: &PublicIdentityMaterial) -> bool {
    replica
        .siblings
        .iter()
        .any(|sibling| sibling.identity_hash() == keys.identity_hash())
}

pub struct RosterSync;

impl RequestEndpoint<ControllerAppState> for RosterSync {
    const ENDPOINT_ID: &'static str = ROSTER_SYNC_REQUEST_ENDPOINT_ID;
    const POLICY: RequestEndpointPolicy = RequestEndpointPolicy::AllowAll;

    async fn handle(
        mut context: RequestContext<'_, ControllerAppState>,
        _node: &impl personal_rns::runtime::PrnsNodeApi,
    ) -> Result<(), Decline> {
        let inbound = parse_roster_message(context.data);
        let Ok(mut shared) = context.state.roster.lock() else {
            return context.respond([
                RosterMessageKind::Error as u8,
                RosterErrorCode::Malformed as u8,
            ]);
        };
        let Some(instance) = instance_material(&shared) else {
            return context.respond([
                RosterMessageKind::Error as u8,
                RosterErrorCode::Malformed as u8,
            ]);
        };
        match inbound {
            Some(RosterInbound::Pull { signer }) => {
                if !is_sibling(&shared.replica, &signer) {
                    drop(shared);
                    return context.respond([
                        RosterMessageKind::Error as u8,
                        RosterErrorCode::NotSibling as u8,
                    ]);
                }
                shared.note_roster_sync(signer.identity_hash(), instance.identity_hash());
                let reply = replica_message(&instance, &shared.replica);
                drop(shared);
                let Some(reply) = reply else {
                    return context.respond([
                        RosterMessageKind::Error as u8,
                        RosterErrorCode::Malformed as u8,
                    ]);
                };
                context.respond(reply)
            }
            Some(RosterInbound::Replica { delta, .. }) => {
                if !is_sibling(&shared.replica, &delta.signer) {
                    drop(shared);
                    return context.respond([
                        RosterMessageKind::Error as u8,
                        RosterErrorCode::NotSibling as u8,
                    ]);
                }
                let peer = delta.signer.identity_hash();
                shared.note_roster_sync(peer, instance.identity_hash());
                shared.pending.push(delta);
                shared.sync_notify.notify_one();
                let reply = replica_message(&instance, &shared.replica);
                drop(shared);
                let Some(reply) = reply else {
                    return context.respond([
                        RosterMessageKind::Error as u8,
                        RosterErrorCode::Malformed as u8,
                    ]);
                };
                context.respond(reply)
            }
            Some(RosterInbound::Error(code)) => {
                drop(shared);
                context.respond([RosterMessageKind::Error as u8, code])
            }
            None => {
                drop(shared);
                context.respond([
                    RosterMessageKind::Error as u8,
                    RosterErrorCode::Malformed as u8,
                ])
            }
        }
    }
}

pub fn parse_replica_reply(bytes: &[u8]) -> Option<RosterDelta> {
    match parse_roster_message(bytes)? {
        RosterInbound::Replica { delta, .. } => Some(delta),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use personal_rns::identity::{PrivateIdentityMaterial, IDENTITY_SECRET_KEY_LEN};
    use personal_rns::remote_control::RemoteControlRequestKind;

    fn secret(fill: u8) -> PrivateIdentityMaterial {
        PrivateIdentityMaterial::from_bytes([fill; IDENTITY_SECRET_KEY_LEN])
    }

    #[test]
    fn the_roster_sync_address_is_not_the_sibling_identity_hash() {
        let identity = secret(0x11).identity_hash();
        let address = roster_sync_address_hex(identity).expect("sync address");
        assert_ne!(address, encode_hex(identity.as_bytes()));
    }

    #[test]
    fn managed_node_membership_and_names_mark_the_list_stale() {
        let membership = MergePlan {
            clock: 1,
            upserts: vec![access(0x21)],
            forgets: Vec::new(),
            labels: Vec::new(),
            leases: Vec::new(),
        };
        assert!(plan_changes_managed_nodes(&membership));

        let renamed = MergePlan {
            clock: 1,
            upserts: Vec::new(),
            forgets: Vec::new(),
            labels: vec![label(
                RosterLabelKind::TargetName,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                Some("Mt2A"),
                stamp(0x22, 1),
            )],
            leases: Vec::new(),
        };
        assert!(plan_changes_managed_nodes(&renamed));

        let sibling_alias = MergePlan {
            clock: 1,
            upserts: Vec::new(),
            forgets: Vec::new(),
            labels: vec![label(
                RosterLabelKind::SiblingAlias,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                Some("Android"),
                stamp(0x22, 1),
            )],
            leases: Vec::new(),
        };
        assert!(!plan_changes_managed_nodes(&sibling_alias));
    }

    fn access(fill: u8) -> RemoteControlTargetAccess {
        RemoteControlTargetAccess::new(
            RemoteControlTargetIdentity::new(secret(fill).public().public_keys()),
            RemoteControlControllerAuthority::Operator,
            RemoteControlRequestSet::only(RemoteControlRequestKind::Describe),
        )
        .unwrap()
    }

    fn stamp(fill: u8, millis: u64) -> LabelStamp {
        LabelStamp {
            millis,
            counter: 0,
            actor: secret(fill).identity_hash(),
        }
    }

    fn label(
        kind: RosterLabelKind,
        key: &str,
        value: Option<&str>,
        stamp: LabelStamp,
    ) -> RosterLabel {
        RosterLabel {
            kind,
            key: key.to_string(),
            value: value.map(str::to_string),
            stamp,
        }
    }

    fn delta_with(signer: PublicIdentityMaterial, member: Option<Member>) -> RosterDelta {
        RosterDelta {
            signer,
            members: member.into_iter().collect(),
            siblings: Vec::new(),
            labels: Vec::new(),
            leases: Vec::new(),
        }
    }

    #[test]
    fn union_adds_the_missing_row() {
        let mut remote = RosterReplica::default();
        bind_actor(&mut remote, secret(0x11).identity_hash());
        note_local_upsert(&mut remote, access(0x21));
        let local = RosterReplica::default();
        let delta = RosterDelta {
            signer: secret(0x11).public(),
            members: remote.members.clone(),
            siblings: vec![secret(0x11).public()],
            labels: Vec::new(),
            leases: Vec::new(),
        };
        let (merged, plan) = merge_roster(&local, &delta);
        assert_eq!(plan.upserts.len(), 1);
        assert!(plan.forgets.is_empty());
        assert_eq!(merged.siblings.len(), 1);
        assert_eq!(replica_known_targets(&merged).len(), 1);
    }

    #[test]
    fn cloned_member_list_joins_the_source_dots() {
        let mut source = RosterReplica::default();
        bind_actor(&mut source, secret(0x11).identity_hash());
        note_local_upsert(&mut source, access(0x21));
        let encoded = encode_member_list(&source.members).unwrap();
        let members = decode_member_list(&encoded).unwrap();
        assert!(decode_member_list(b"snap").is_none());
        let dest = RosterReplica::default();
        let (merged, _) = merge_roster(
            &dest,
            &RosterDelta {
                signer: secret(0x11).public(),
                members,
                siblings: Vec::new(),
                labels: Vec::new(),
                leases: Vec::new(),
            },
        );
        assert_eq!(
            replica_known_targets(&merged),
            replica_known_targets(&source)
        );
        assert_eq!(merged.members[0].adds, source.members[0].adds);
        assert_eq!(
            merged.members[0].access_stamp,
            source.members[0].access_stamp
        );
    }

    #[test]
    fn remove_that_names_the_dot_drops_the_node() {
        let hash = access(0x21).target().identity_hash();
        let mut local = RosterReplica::default();
        bind_actor(&mut local, secret(0x11).identity_hash());
        note_local_upsert(&mut local, access(0x21));
        let dot = local.members[0].adds[0];
        let remote = Member {
            access: clone_access(&local.members[0].access),
            access_stamp: local.members[0].access_stamp,
            adds: vec![dot],
            removes: vec![dot],
        };
        let (_merged, plan) =
            merge_roster(&local, &delta_with(secret(0x22).public(), Some(remote)));
        assert!(
            plan.forgets.is_empty(),
            "dropping the only node is rejected"
        );
        assert_eq!(replica_known_targets(&local), vec![hash]);
    }

    #[test]
    fn remove_of_one_node_leaves_the_other() {
        let mut local = RosterReplica::default();
        bind_actor(&mut local, secret(0x11).identity_hash());
        note_local_upsert(&mut local, access(0x21));
        note_local_upsert(&mut local, access(0x22));
        let gone = local.members[0].access.target().identity_hash();
        let dot = local.members[0].adds[0];
        let remote = Member {
            access: clone_access(&local.members[0].access),
            access_stamp: local.members[0].access_stamp,
            adds: vec![dot],
            removes: vec![dot],
        };
        let (merged, plan) = merge_roster(&local, &delta_with(secret(0x33).public(), Some(remote)));
        assert_eq!(plan.forgets, vec![gone]);
        assert_eq!(replica_known_targets(&merged).len(), 1);
    }

    #[test]
    fn concurrent_add_survives_a_remove_that_did_not_observe_it() {
        let mut local = RosterReplica::default();
        bind_actor(&mut local, secret(0x11).identity_hash());
        note_local_upsert(&mut local, access(0x21));
        let observed = local.members[0].adds[0];
        note_local_upsert(&mut local, access(0x21));
        let fresh = local.members[0].adds[1];
        let remote = Member {
            access: clone_access(&local.members[0].access),
            access_stamp: local.members[0].access_stamp,
            adds: vec![observed],
            removes: vec![observed],
        };
        let (merged, plan) = merge_roster(&local, &delta_with(secret(0x22).public(), Some(remote)));
        assert!(plan.forgets.is_empty());
        assert!(merged.members[0].adds.contains(&fresh));
        assert!(!merged.members[0].removes.contains(&fresh));
        assert_eq!(replica_known_targets(&merged).len(), 1);
    }

    #[test]
    fn repair_after_remove_brings_the_node_back() {
        let hash = access(0x21).target().identity_hash();
        let mut local = RosterReplica::default();
        bind_actor(&mut local, secret(0x11).identity_hash());
        note_local_upsert(&mut local, access(0x21));
        forget_target_locally(&mut local, hash);
        assert!(replica_known_targets(&local).is_empty());
        note_local_upsert(&mut local, access(0x21));
        assert_eq!(replica_known_targets(&local), vec![hash]);
    }

    #[test]
    fn empty_sibling_and_a_far_ahead_counter_keep_local_nodes() {
        let mut local = RosterReplica::default();
        bind_actor(&mut local, secret(0x11).identity_hash());
        note_local_upsert(&mut local, access(0x21));
        let hash = access(0x21).target().identity_hash();
        let empty = RosterDelta {
            signer: secret(0x22).public(),
            members: Vec::new(),
            siblings: Vec::new(),
            labels: Vec::new(),
            leases: Vec::new(),
        };
        let (merged, plan) = merge_roster(&local, &empty);
        assert!(plan.forgets.is_empty());
        assert_eq!(replica_known_targets(&merged), vec![hash]);
        let mut other = RosterReplica::default();
        bind_actor(&mut other, secret(0x22).identity_hash());
        other.clock = 9_000;
        note_local_upsert(&mut other, access(0x33));
        let (merged, plan) = merge_roster(
            &local,
            &RosterDelta {
                signer: secret(0x22).public(),
                members: other.members,
                siblings: Vec::new(),
                labels: Vec::new(),
                leases: Vec::new(),
            },
        );
        assert!(plan.forgets.is_empty());
        assert!(replica_known_targets(&merged).contains(&hash));
    }

    #[test]
    fn label_only_replica_does_not_change_membership() {
        let mut local = RosterReplica::default();
        bind_actor(&mut local, secret(0x11).identity_hash());
        note_local_upsert(&mut local, access(0x21));
        let before = replica_known_targets(&local);
        let delta = RosterDelta {
            signer: secret(0x22).public(),
            members: Vec::new(),
            siblings: Vec::new(),
            labels: vec![label(
                RosterLabelKind::TargetAlias,
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                Some("kitchen"),
                stamp(0x22, 50),
            )],
            leases: Vec::new(),
        };
        let (merged, plan) = merge_roster(&local, &delta);
        assert_eq!(replica_known_targets(&merged), before);
        assert_eq!(plan.labels.len(), 1);
        assert!(plan.forgets.is_empty());
    }

    #[test]
    fn expired_looking_lease_is_free_and_forget_clears_it() {
        let target = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let this_instance = encode_hex(secret(0x11).identity_hash().as_bytes());
        let mut replica = RosterReplica::default();
        bind_actor(&mut replica, secret(0x11).identity_hash());
        note_local_label(
            &mut replica,
            RosterLabelKind::TargetLooking,
            target,
            Some(&this_instance),
        );
        assert_eq!(
            attention_for_target(&replica, target, &this_instance),
            TargetAttention::HeldByThis
        );
        replica.leases[0].deadline_ms = 0;
        assert_eq!(
            attention_for_target(&replica, target, &this_instance),
            TargetAttention::Free
        );
        let hash = IdentityHash::new(parse_hex::<HASH_LEN>(target).expect("hash"));
        note_local_upsert(&mut replica, access(0x21));
        forget_target_locally(&mut replica, hash);
        assert_eq!(looking_instance(&replica, target), None);
    }

    #[test]
    fn two_sibling_replies_both_contribute() {
        let mut first = RosterReplica::default();
        bind_actor(&mut first, secret(0x11).identity_hash());
        note_local_upsert(&mut first, access(0x21));
        let mut second = RosterReplica::default();
        bind_actor(&mut second, secret(0x22).identity_hash());
        note_local_upsert(&mut second, access(0x22));
        let local = RosterReplica::default();
        let (once, _) = merge_roster(
            &local,
            &RosterDelta {
                signer: secret(0x11).public(),
                members: first.members,
                siblings: Vec::new(),
                labels: Vec::new(),
                leases: Vec::new(),
            },
        );
        let (twice, _) = merge_roster(
            &once,
            &RosterDelta {
                signer: secret(0x22).public(),
                members: second.members,
                siblings: Vec::new(),
                labels: Vec::new(),
                leases: Vec::new(),
            },
        );
        assert_eq!(replica_known_targets(&twice).len(), 2);
    }

    #[test]
    fn non_v3_file_is_left_unread_and_unreplaced() {
        let dir = std::env::temp_dir().join(format!("roster-v2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("roster-replica");
        std::fs::write(&path, "clock 4\nupsert leftover\n").expect("write v2");
        assert!(load_replica(&path).members.is_empty());
        persist_replica(&path, &RosterReplica::default());
        let kept = std::fs::read_to_string(&path).expect("still there");
        assert!(kept.starts_with("clock 4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v3_file_round_trips_and_writes_a_bak() {
        let mut replica = RosterReplica::default();
        bind_actor(&mut replica, secret(0x11).identity_hash());
        note_local_upsert(&mut replica, access(0x21));
        note_local_label(
            &mut replica,
            RosterLabelKind::SiblingAlias,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some("Kitchen"),
        );
        let dir = std::env::temp_dir().join(format!("roster-v3-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("roster-replica");
        persist_replica(&path, &replica);
        let loaded = load_replica(&path);
        assert_eq!(
            replica_known_targets(&loaded),
            replica_known_targets(&replica)
        );
        assert_eq!(loaded.labels[0].value.as_deref(), Some("Kitchen"));
        assert!(dir.join("roster-replica.bak").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newer_label_wins_and_older_does_not() {
        let mut local = RosterReplica::default();
        bind_actor(&mut local, secret(0x11).identity_hash());
        note_local_label(
            &mut local,
            RosterLabelKind::TargetAlias,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some("old"),
        );
        let newer = label(
            RosterLabelKind::TargetAlias,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            None,
            stamp(0x22, local.labels[0].stamp.millis.saturating_add(5)),
        );
        let (merged, plan) = merge_roster(
            &local,
            &RosterDelta {
                signer: secret(0x22).public(),
                members: Vec::new(),
                siblings: Vec::new(),
                labels: vec![newer],
                leases: Vec::new(),
            },
        );
        assert_eq!(plan.labels[0].value, None);
        let older = label(
            RosterLabelKind::TargetAlias,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some("porch"),
            LabelStamp::default(),
        );
        let (_merged, plan) = merge_roster(
            &merged,
            &RosterDelta {
                signer: secret(0x22).public(),
                members: Vec::new(),
                siblings: Vec::new(),
                labels: vec![older],
                leases: Vec::new(),
            },
        );
        assert!(plan.labels.is_empty());
    }

    #[test]
    fn note_local_label_skips_unchanged_values() {
        let mut local = RosterReplica::default();
        note_local_label(
            &mut local,
            RosterLabelKind::TargetAlias,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some("hv4C"),
        );
        let stamp = local.labels[0].stamp;
        note_local_label(
            &mut local,
            RosterLabelKind::TargetAlias,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some("hv4C"),
        );
        assert_eq!(local.labels[0].stamp, stamp);
    }

    #[test]
    fn sibling_alias_for_the_signer_is_not_merged() {
        let signer = secret(0x11).public();
        let signer_hex = encode_hex(signer.identity_hash().as_bytes());
        let other = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let local = RosterReplica::default();
        let delta = RosterDelta {
            signer,
            members: Vec::new(),
            siblings: Vec::new(),
            labels: vec![
                label(
                    RosterLabelKind::SiblingAlias,
                    &signer_hex,
                    Some("Sibling 2"),
                    stamp(0x11, 2),
                ),
                label(
                    RosterLabelKind::SiblingAlias,
                    other,
                    Some("Laptop"),
                    stamp(0x11, 2),
                ),
            ],
            leases: Vec::new(),
        };
        let (merged, plan) = merge_roster(&local, &delta);
        assert_eq!(plan.labels.len(), 1);
        assert_eq!(plan.labels[0].key, other);
        assert_eq!(merged.labels.len(), 1);
    }

    #[test]
    fn replica_message_omits_the_signer_sibling_alias() {
        let signer = secret(0x44);
        let self_hex = encode_hex(signer.identity_hash().as_bytes());
        let mut replica = RosterReplica::default();
        bind_actor(&mut replica, signer.identity_hash());
        note_local_label(
            &mut replica,
            RosterLabelKind::SiblingAlias,
            &self_hex,
            Some("This controller"),
        );
        note_local_label(
            &mut replica,
            RosterLabelKind::SiblingAlias,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            Some("Phone"),
        );
        let delta = parse_replica_reply(&replica_message(&signer, &replica).unwrap()).unwrap();
        assert_eq!(delta.labels.len(), 1);
        assert_eq!(delta.labels[0].value.as_deref(), Some("Phone"));
    }

    #[test]
    fn controller_usb_host_ids_are_not_syncable_peer_aliases() {
        assert!(!peer_alias_is_syncable("d0d0d0d0d0d0d0d0"));
        assert!(!peer_alias_is_syncable("d1d1d1d1d1d1d1d1"));
        assert!(peer_alias_is_syncable("aabbccddeeff0011"));
        assert!(!peer_alias_value_is_syncable(Some("This Controller")));
        assert!(!peer_alias_value_is_syncable(Some("Auto gateway")));
        assert!(peer_alias_value_is_syncable(Some("Hv4A")));
    }

    #[test]
    fn seed_import_skips_local_only_peer_aliases() {
        let mut replica = RosterReplica::default();
        let mut values = std::collections::HashMap::new();
        values.insert("d0d0d0d0d0d0d0d0".to_string(), "cable".to_string());
        values.insert(
            "08530421d2659610".to_string(),
            THIS_CONTROLLER_PEER_ALIAS.to_string(),
        );
        values.insert("aabbccddeeff0011".to_string(), "tower".to_string());
        import_seed_labels(&mut replica, RosterLabelKind::PeerAlias, &values);
        assert_eq!(replica.labels.len(), 1);
        assert_eq!(replica.labels[0].key, "aabbccddeeff0011");
    }

    #[test]
    fn next_aliases_fill_the_lowest_unused_number() {
        let mut aliases = HashMap::new();
        assert_eq!(next_sibling_alias(&aliases), "Sibling 1");
        aliases.insert("aa".to_string(), "Sibling 1".to_string());
        aliases.insert("cc".to_string(), "Sibling 3".to_string());
        assert_eq!(next_sibling_alias(&aliases), "Sibling 2");
        assert_eq!(next_target_alias(&HashMap::new()), "Alias 1");
    }

    #[test]
    fn forgotten_sibling_is_not_re_pinned_and_adopt_clears_removal() {
        let sibling = secret(0x77).public();
        let mut local = RosterReplica::default();
        pin_sibling(&mut local, sibling);
        forget_sibling_locally(&mut local, sibling.identity_hash());
        let (merged, _) = merge_roster(
            &local,
            &RosterDelta {
                signer: secret(0x11).public(),
                members: Vec::new(),
                siblings: vec![sibling],
                labels: Vec::new(),
                leases: Vec::new(),
            },
        );
        assert!(merged.siblings.is_empty());
        adopt_sibling(&mut local, sibling);
        assert_eq!(local.siblings, vec![sibling]);
    }

    #[test]
    fn sibling_wifi_ll_must_be_the_signer() {
        let signer = secret(0x11).public();
        let signer_hex = encode_hex(signer.identity_hash().as_bytes());
        let delta = RosterDelta {
            signer,
            members: Vec::new(),
            siblings: Vec::new(),
            labels: vec![
                label(
                    RosterLabelKind::SiblingWifiLl,
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    Some("fe80::1"),
                    stamp(0x11, 2),
                ),
                label(
                    RosterLabelKind::SiblingWifiLl,
                    &signer_hex,
                    Some("fe80::494:446c:eb84:e48b"),
                    stamp(0x11, 2),
                ),
            ],
            leases: Vec::new(),
        };
        let (merged, plan) = merge_roster(&RosterReplica::default(), &delta);
        assert_eq!(plan.labels.len(), 1);
        assert_eq!(merged.labels[0].key, signer_hex);
    }

    #[test]
    fn pull_and_replica_messages_verify() {
        let signer = secret(0x44);
        let mut replica = RosterReplica::default();
        bind_actor(&mut replica, signer.identity_hash());
        replica.clock = 1;
        pin_sibling(&mut replica, secret(0x55).public());
        let mut pull = [0u8; 256];
        let pull_len = write_pull(&signer, &mut pull).unwrap();
        assert!(matches!(
            parse_roster_message(&pull[..pull_len]),
            Some(RosterInbound::Pull { .. })
        ));
        let delta = parse_replica_reply(&replica_message(&signer, &replica).unwrap()).unwrap();
        assert_eq!(delta.siblings.len(), 1);
    }
}
