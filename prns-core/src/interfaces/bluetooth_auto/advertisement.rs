use super::handshake::{l2cap_arrangement, AppleHost, Endpoint, L2capArrangement};
use super::identity::{default_group_tag, BleAddress, BleIdentity, GROUP_TAG_LEN};

pub const MAX_ADVERTISEMENT_LEN: usize = 31;

const fn ble_reticulum_uuid(last: u8) -> [u8; 16] {
    [
        0x37, 0x14, 0x5b, 0x00, 0x44, 0x2d, 0x4a, 0x94, 0x91, 0x7f, 0x8f, 0x42, 0xc5, 0xda, 0x28,
        last,
    ]
}

pub const BLE_SERVICE_UUID_BYTES: [u8; 16] = ble_reticulum_uuid(0xe3);
pub const BLE_SERVICE_UUID: BleUuid = BleUuid::Bit128(BLE_SERVICE_UUID_BYTES);
pub const COLUMBA_RX_UUID: BleUuid = BleUuid::Bit128(ble_reticulum_uuid(0xe5));
pub const COLUMBA_TX_UUID: BleUuid = BleUuid::Bit128(ble_reticulum_uuid(0xe4));
pub const COLUMBA_IDENTITY_UUID: BleUuid = BleUuid::Bit128(ble_reticulum_uuid(0xe6));
pub const NATIVE_CONTROL_UUID: BleUuid = BleUuid::Bit128(ble_reticulum_uuid(0xe7));
pub const NATIVE_DATA_UUID: BleUuid = BleUuid::Bit128(ble_reticulum_uuid(0xe8));

const AD_FLAGS: u8 = 0x01;
const AD_INCOMPLETE_SERVICE_UUID128: u8 = 0x06;
const AD_SERVICE_UUID128: u8 = 0x07;
pub(super) const AD_MANUFACTURER_SPECIFIC: u8 = 0xff;
const FLAGS_LE_GENERAL_DISCOVERABLE: u8 = 0x06;
const EXPERIMENTAL_ROLE_COMPANY_ID: [u8; 2] = [0xff, 0xff];
/// Oldest manufacturer payload we still parse for role flags.
pub(super) const EXPERIMENTAL_ROLE_VERSION_MIN: u8 = 0x03;
/// Manufacturer payload version that carries a discovery group tag.
pub(super) const EXPERIMENTAL_ROLE_VERSION: u8 = 0x04;
/// Retired host payload that also carried a 6-byte dial-election key. No field installations.
pub(super) const EXPERIMENTAL_ROLE_VERSION_WITH_DIAL_KEY: u8 = 0x05;
/// Host payload: node type plus discovery group. Mixed fleet is v4 + v6 only.
pub const EXPERIMENTAL_ROLE_VERSION_WITH_NODE_TYPE: u8 = 0x06;
pub(super) const EXPERIMENTAL_ROLE_PERIPHERAL_ONLY: u8 = 0x01;
pub const DIAL_KEY_LEN: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BleUuid {
    Bit16(u16),
    Bit128([u8; 16]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BleRoleCapabilities {
    DualRole,
    PeripheralOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumbaConnectionRole {
    Dial,
    Accept,
    Unavailable,
}

/// Whether this advertisement report carried our manufacturer-specific field.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManufacturerPresence {
    Present,
    Absent,
}

/// Whether a discovery should become an outbound dial sighting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DialSightingAction {
    /// Forward a sighting so policy may dial.
    Dial,
    /// Keep the peer for inbound / a later complete ADV; do not dial.
    Accept,
}

/// How DualRole manufacturer ADV without a v5 dial-key is treated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyDualRolePolicy {
    /// Host stacks (Mac): initiate toward SoftDevice / ESP.
    FailOpenDial,
    /// Embedded: keep radio-address election between firmware peers.
    AddressSort { local: BleAddress, peer: BleAddress },
}

/// Encode a SoftDevice-sized ADV including the Prns service UUID, role flags, and discovery group tag.
///
/// Layout fills all 31 classic ADV bytes when successful.
pub fn encode_advertisement(
    out: &mut [u8],
    role_capabilities: BleRoleCapabilities,
    group_tag: [u8; GROUP_TAG_LEN],
) -> Option<usize> {
    let mut writer = AdWriter::new(out);
    writer.put(AD_FLAGS, &[FLAGS_LE_GENERAL_DISCOVERABLE])?;
    let mut little_endian = BLE_SERVICE_UUID_BYTES;
    little_endian.reverse();
    writer.put(AD_SERVICE_UUID128, &little_endian)?;
    let flags = match role_capabilities {
        BleRoleCapabilities::DualRole => 0,
        BleRoleCapabilities::PeripheralOnly => EXPERIMENTAL_ROLE_PERIPHERAL_ONLY,
    };
    writer.put(
        AD_MANUFACTURER_SPECIFIC,
        &[
            EXPERIMENTAL_ROLE_COMPANY_ID[0],
            EXPERIMENTAL_ROLE_COMPANY_ID[1],
            EXPERIMENTAL_ROLE_VERSION,
            flags,
            group_tag[0],
            group_tag[1],
            group_tag[2],
            group_tag[3],
        ],
    )?;
    Some(writer.len())
}

/// Classic ADV with a v6 type byte. Same 31-byte layout as [`encode_advertisement`];
/// group stays at the v4 offset so mixed-fleet scanners keep filtering.
pub fn encode_advertisement_with_node_type(
    out: &mut [u8],
    endpoint: Endpoint,
    group_tag: [u8; GROUP_TAG_LEN],
) -> Option<usize> {
    let mut writer = AdWriter::new(out);
    writer.put(AD_FLAGS, &[FLAGS_LE_GENERAL_DISCOVERABLE])?;
    let mut little_endian = BLE_SERVICE_UUID_BYTES;
    little_endian.reverse();
    writer.put(AD_SERVICE_UUID128, &little_endian)?;
    let payload = manufacturer_role_payload_with_node_type(endpoint, group_tag);
    writer.put(
        AD_MANUFACTURER_SPECIFIC,
        &[
            EXPERIMENTAL_ROLE_COMPANY_ID[0],
            EXPERIMENTAL_ROLE_COMPANY_ID[1],
            payload[0],
            payload[1],
            payload[2],
            payload[3],
            payload[4],
            payload[5],
        ],
    )?;
    Some(writer.len())
}

pub fn contains_service(adv: &[u8]) -> bool {
    let mut little_endian = BLE_SERVICE_UUID_BYTES;
    little_endian.reverse();
    AdReader::new(adv).any(|(ad_type, body)| {
        (ad_type == AD_SERVICE_UUID128 || ad_type == AD_INCOMPLETE_SERVICE_UUID128)
            && body == little_endian
    })
}

pub fn columba_role_capabilities(adv: &[u8]) -> Option<BleRoleCapabilities> {
    AdReader::new(adv).find_map(|(ad_type, body)| {
        if ad_type != AD_MANUFACTURER_SPECIFIC {
            return None;
        }
        let company_id: [u8; 2] = body.get(..2)?.try_into().ok()?;
        columba_role_capabilities_from_manufacturer(u16::from_le_bytes(company_id), body.get(2..)?)
    })
}

pub fn columba_role_capabilities_from_manufacturer(
    company_id: u16,
    data: &[u8],
) -> Option<BleRoleCapabilities> {
    if company_id != u16::from_le_bytes(EXPERIMENTAL_ROLE_COMPANY_ID)
        || *data.first()? < EXPERIMENTAL_ROLE_VERSION_MIN
    {
        return None;
    }
    if *data.first()? >= EXPERIMENTAL_ROLE_VERSION_WITH_NODE_TYPE {
        return Some(BleRoleCapabilities::DualRole);
    }
    if data.get(1)? & EXPERIMENTAL_ROLE_PERIPHERAL_ONLY == 0 {
        Some(BleRoleCapabilities::DualRole)
    } else {
        Some(BleRoleCapabilities::PeripheralOnly)
    }
}

/// Discovery group tag from manufacturer data, or the default group when the peer is legacy (v3).
pub fn advertisement_group_tag(adv: &[u8]) -> [u8; GROUP_TAG_LEN] {
    AdReader::new(adv)
        .find_map(|(ad_type, body)| {
            if ad_type != AD_MANUFACTURER_SPECIFIC {
                return None;
            }
            let company_id: [u8; 2] = body.get(..2)?.try_into().ok()?;
            group_tag_from_manufacturer(u16::from_le_bytes(company_id), body.get(2..)?)
        })
        .unwrap_or_else(default_group_tag)
}

/// Group tag from a parsed manufacturer payload (`version | flags | tag…`), if present.
pub fn group_tag_from_manufacturer(company_id: u16, data: &[u8]) -> Option<[u8; GROUP_TAG_LEN]> {
    if company_id != u16::from_le_bytes(EXPERIMENTAL_ROLE_COMPANY_ID) {
        return None;
    }
    if *data.first()? < EXPERIMENTAL_ROLE_VERSION {
        return None;
    }
    data.get(2..2 + GROUP_TAG_LEN)?.try_into().ok()
}

/// Effective discovery group for a manufacturer payload (legacy/missing → default group).
pub fn manufacturer_discovery_group_tag(company_id: u16, data: &[u8]) -> [u8; GROUP_TAG_LEN] {
    group_tag_from_manufacturer(company_id, data).unwrap_or_else(default_group_tag)
}

/// True when the advertisement's discovery group matches `local_tag`.
///
/// Legacy (v3) advertisements are treated as the default group, so custom-group
/// nodes do not peer with untagged firmware.
pub fn discovery_groups_match(local_tag: [u8; GROUP_TAG_LEN], adv: &[u8]) -> bool {
    advertisement_group_tag(adv) == local_tag
}

/// True when a manufacturer payload's discovery group matches `local_tag`.
pub fn manufacturer_discovery_groups_match(
    local_tag: [u8; GROUP_TAG_LEN],
    company_id: u16,
    data: &[u8],
) -> bool {
    manufacturer_discovery_group_tag(company_id, data) == local_tag
}

/// Manufacturer-specific body for a DualRole advertisement in the local discovery group.
///
/// SoftDevice primary ADV is capped at [`MAX_ADVERTISEMENT_LEN`]; this v4 shape fits beside the
/// 128-bit service UUID. Prefer [`manufacturer_role_payload_with_node_type`] on every dual-role
/// advertiser (including SoftDevice) so typed dial can see Esp32 / Nrf52.
pub fn manufacturer_role_payload(
    role_capabilities: BleRoleCapabilities,
    group_tag: [u8; GROUP_TAG_LEN],
) -> [u8; 2 + GROUP_TAG_LEN] {
    let flags = match role_capabilities {
        BleRoleCapabilities::DualRole => 0,
        BleRoleCapabilities::PeripheralOnly => EXPERIMENTAL_ROLE_PERIPHERAL_ONLY,
    };
    [
        EXPERIMENTAL_ROLE_VERSION,
        flags,
        group_tag[0],
        group_tag[1],
        group_tag[2],
        group_tag[3],
    ]
}

/// Host manufacturer payload: advertised node type plus discovery group.
///
/// Same size as v4, so it still sits in classic ADV_IND next to the 128-bit UUID.
/// Group stays at the v4 offset so un-upgraded scanners keep filtering. Byte 1 is
/// [`Endpoint::advertisement_type_byte`] instead of DualRole flags.
pub fn manufacturer_role_payload_with_node_type(
    endpoint: Endpoint,
    group_tag: [u8; GROUP_TAG_LEN],
) -> [u8; 2 + GROUP_TAG_LEN] {
    [
        EXPERIMENTAL_ROLE_VERSION_WITH_NODE_TYPE,
        endpoint.advertisement_type_byte(),
        group_tag[0],
        group_tag[1],
        group_tag[2],
        group_tag[3],
    ]
}

/// Node type from a manufacturer role body, when the peer advertised v6+.
pub fn node_type_from_role_payload(data: &[u8]) -> Option<Endpoint> {
    if *data.first()? < EXPERIMENTAL_ROLE_VERSION_WITH_NODE_TYPE {
        return None;
    }
    Endpoint::from_advertisement_type_byte(*data.get(1)?)
}

/// Node type from a parsed manufacturer payload, when the peer advertised v6+.
pub fn node_type_from_manufacturer(company_id: u16, data: &[u8]) -> Option<Endpoint> {
    if company_id != u16::from_le_bytes(EXPERIMENTAL_ROLE_COMPANY_ID) {
        return None;
    }
    node_type_from_role_payload(data)
}

/// Node type from a full advertisement report, when the peer advertised v6+.
pub fn node_type_from_advertisement(adv: &[u8]) -> Option<Endpoint> {
    AdReader::new(adv).find_map(|(ad_type, body)| {
        if ad_type != AD_MANUFACTURER_SPECIFIC {
            return None;
        }
        let company_id: [u8; 2] = body.get(..2)?.try_into().ok()?;
        node_type_from_manufacturer(u16::from_le_bytes(company_id), body.get(2..)?)
    })
}

/// Parsed manufacturer role fields from one ADV or SCAN_RSP report.
///
/// v6 is `[ver, type, group4]`. There is no v5 dial-key tie on the wire; [`Self::dial_key`]
/// is only set for historical v5 payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdvertisedRoleView {
    pub version: Option<u8>,
    pub type_byte: Option<u8>,
    pub node_type: Option<Endpoint>,
    pub group: [u8; GROUP_TAG_LEN],
    pub dial_key: Option<BleAddress>,
    pub has_service: bool,
    pub manufacturer: ManufacturerPresence,
}

/// Walk one report and surface the v6 type / group (and a v5 dial-key if present).
pub fn advertised_role_view(adv: &[u8]) -> AdvertisedRoleView {
    let (version, type_byte) = AdReader::new(adv)
        .find_map(|(ad_type, body)| {
            if ad_type != AD_MANUFACTURER_SPECIFIC {
                return None;
            }
            if body.get(..2) != Some(EXPERIMENTAL_ROLE_COMPANY_ID.as_slice()) {
                return None;
            }
            Some((*body.get(2)?, body.get(3).copied()))
        })
        .map(|(version, type_byte)| (Some(version), type_byte))
        .unwrap_or((None, None));
    AdvertisedRoleView {
        version,
        type_byte,
        node_type: node_type_from_advertisement(adv),
        group: advertisement_group_tag(adv),
        dial_key: dial_key_from_advertisement(adv),
        has_service: contains_service(adv),
        manufacturer: advertisement_manufacturer_presence(adv),
    }
}

/// CoreBluetooth `startAdvertising` never puts manufacturer data on the air.
/// A Prns UUID with no manufacturer is treated as MacOs for the arrangement table.
pub fn implied_macos_without_manufacturer() -> Endpoint {
    Endpoint::CoreBluetooth(AppleHost::MacOs)
}

/// Peer type from this report, or MacOs when our service is present and manufacturer is absent.
pub fn advertised_or_implied_node_type(adv: &[u8]) -> Option<Endpoint> {
    if let Some(peer) = node_type_from_advertisement(adv) {
        return Some(peer);
    }
    if contains_service(adv)
        && advertisement_manufacturer_presence(adv) == ManufacturerPresence::Absent
    {
        return Some(implied_macos_without_manufacturer());
    }
    None
}

/// Use the CoC arrangement table as the pre-connect dial decision when ADV carries a type.
///
/// `Opens(E)` means E dials and opens CoC. `GattOnly` / `EitherOpens` return `None` so the
/// caller keeps the v4 C′ path (Mac/Android first; same-type ties are later).
pub fn typed_dial_override(
    local: Endpoint,
    advertised_peer: Option<Endpoint>,
) -> Option<DialSightingAction> {
    let peer = advertised_peer?;
    match l2cap_arrangement(local, peer) {
        L2capArrangement::Opens(opener) if opener == local => Some(DialSightingAction::Dial),
        L2capArrangement::Opens(_) => Some(DialSightingAction::Accept),
        L2capArrangement::GattOnly | L2capArrangement::EitherOpens => None,
    }
}

/// JNI / host adapter code: `1` dial, `0` accept, `-1` keep the v4 C′ path.
///
/// An empty payload means no manufacturer: look up implied MacOs.
pub fn typed_dial_override_code(local: Endpoint, role_payload: &[u8]) -> i8 {
    let peer = if role_payload.is_empty() {
        Some(implied_macos_without_manufacturer())
    } else {
        node_type_from_role_payload(role_payload)
    };
    match typed_dial_override(local, peer) {
        Some(DialSightingAction::Dial) => 1,
        Some(DialSightingAction::Accept) => 0,
        None => -1,
    }
}

/// Host manufacturer payload including a dial-election key shared across address spaces.
///
/// Retired wire format (v5). Kept so tests can still parse historical payloads.
pub fn manufacturer_role_payload_with_dial_key(
    role_capabilities: BleRoleCapabilities,
    group_tag: [u8; GROUP_TAG_LEN],
    dial_key: BleAddress,
) -> [u8; 2 + GROUP_TAG_LEN + DIAL_KEY_LEN] {
    let flags = match role_capabilities {
        BleRoleCapabilities::DualRole => 0,
        BleRoleCapabilities::PeripheralOnly => EXPERIMENTAL_ROLE_PERIPHERAL_ONLY,
    };
    let key = *dial_key.octets();
    [
        EXPERIMENTAL_ROLE_VERSION_WITH_DIAL_KEY,
        flags,
        group_tag[0],
        group_tag[1],
        group_tag[2],
        group_tag[3],
        key[0],
        key[1],
        key[2],
        key[3],
        key[4],
        key[5],
    ]
}

/// First six bytes of a Bluetooth Auto identity, used as a cross-platform dial sort key.
pub fn dial_key_from_identity(identity: BleIdentity) -> BleAddress {
    let bytes = identity.as_bytes();
    BleAddress::new([bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]])
}

/// Dial-election key from a manufacturer payload, when the peer advertised v5+.
pub fn dial_key_from_manufacturer(company_id: u16, data: &[u8]) -> Option<BleAddress> {
    if company_id != u16::from_le_bytes(EXPERIMENTAL_ROLE_COMPANY_ID) {
        return None;
    }
    if *data.first()? < EXPERIMENTAL_ROLE_VERSION_WITH_DIAL_KEY {
        return None;
    }
    let key: [u8; DIAL_KEY_LEN] = data
        .get(2 + GROUP_TAG_LEN..2 + GROUP_TAG_LEN + DIAL_KEY_LEN)?
        .try_into()
        .ok()?;
    Some(BleAddress::new(key))
}

/// Pre-dial election (option C′).
///
/// Dial-key (v5) peers elect with [`columba_connection_role`]. DualRole reports
/// without manufacturer Accept so UUID-only primary ADV does not race a phone
/// inbound dial. Legacy DualRole with manufacturer and no dial key follows
/// [`LegacyDualRolePolicy`].
pub fn dial_sighting_action(
    local_dial_key: BleAddress,
    peer_dial_key: Option<BleAddress>,
    peer_capabilities: BleRoleCapabilities,
    manufacturer: ManufacturerPresence,
    legacy: LegacyDualRolePolicy,
) -> DialSightingAction {
    match peer_capabilities {
        BleRoleCapabilities::PeripheralOnly => DialSightingAction::Dial,
        BleRoleCapabilities::DualRole => match peer_dial_key {
            Some(peer) => match columba_connection_role(
                local_dial_key,
                BleRoleCapabilities::DualRole,
                peer,
                BleRoleCapabilities::DualRole,
            ) {
                ColumbaConnectionRole::Dial => DialSightingAction::Dial,
                ColumbaConnectionRole::Accept | ColumbaConnectionRole::Unavailable => {
                    DialSightingAction::Accept
                }
            },
            None => match manufacturer {
                ManufacturerPresence::Absent => DialSightingAction::Accept,
                ManufacturerPresence::Present => match legacy {
                    LegacyDualRolePolicy::FailOpenDial => DialSightingAction::Dial,
                    LegacyDualRolePolicy::AddressSort { local, peer } => {
                        match columba_connection_role(
                            local,
                            BleRoleCapabilities::DualRole,
                            peer,
                            BleRoleCapabilities::DualRole,
                        ) {
                            ColumbaConnectionRole::Dial => DialSightingAction::Dial,
                            ColumbaConnectionRole::Accept | ColumbaConnectionRole::Unavailable => {
                                DialSightingAction::Accept
                            }
                        }
                    }
                },
            },
        },
    }
}

/// Manufacturer presence for our experimental company ID on this ADV report.
pub fn advertisement_manufacturer_presence(adv: &[u8]) -> ManufacturerPresence {
    if AdReader::new(adv).any(|(ad_type, body)| {
        ad_type == AD_MANUFACTURER_SPECIFIC
            && body.get(..2) == Some(EXPERIMENTAL_ROLE_COMPANY_ID.as_slice())
    }) {
        ManufacturerPresence::Present
    } else {
        ManufacturerPresence::Absent
    }
}

/// Dial-election key from a full advertisement report, when the peer advertised v5+.
pub fn dial_key_from_advertisement(adv: &[u8]) -> Option<BleAddress> {
    AdReader::new(adv).find_map(|(ad_type, body)| {
        if ad_type != AD_MANUFACTURER_SPECIFIC {
            return None;
        }
        let company_id: [u8; 2] = body.get(..2)?.try_into().ok()?;
        dial_key_from_manufacturer(u16::from_le_bytes(company_id), body.get(2..)?)
    })
}

/// Embedded scan-path election: typed table when ADV has a type or implies Mac, else address sort.
///
/// Trouble / SoftDevice funnels only forward Dial sightings. Without the typed
/// override, `Opens(Esp32)` vs Mac never fires because C′ address-sort can drop
/// the Mac ADV before policy sees it.
pub fn embedded_scan_dial_action(
    local: Endpoint,
    local_radio: BleAddress,
    peer_radio: BleAddress,
    adv: &[u8],
) -> DialSightingAction {
    typed_dial_override(local, advertised_or_implied_node_type(adv)).unwrap_or_else(|| {
        match columba_connection_role(
            local_radio,
            BleRoleCapabilities::DualRole,
            peer_radio,
            columba_role_capabilities(adv).unwrap_or(BleRoleCapabilities::DualRole),
        ) {
            ColumbaConnectionRole::Dial => DialSightingAction::Dial,
            ColumbaConnectionRole::Accept | ColumbaConnectionRole::Unavailable => {
                DialSightingAction::Accept
            }
        }
    })
}

/// Embedded scan-path election over one ADV or scan-response report.
pub fn embedded_dial_sighting_action(
    local_dial_key: BleAddress,
    local_radio: BleAddress,
    peer_radio: BleAddress,
    adv: &[u8],
) -> DialSightingAction {
    dial_sighting_action(
        local_dial_key,
        dial_key_from_advertisement(adv),
        columba_role_capabilities(adv).unwrap_or(BleRoleCapabilities::DualRole),
        advertisement_manufacturer_presence(adv),
        LegacyDualRolePolicy::AddressSort {
            local: local_radio,
            peer: peer_radio,
        },
    )
}

pub fn columba_connection_role(
    local_address: BleAddress,
    local_capabilities: BleRoleCapabilities,
    peer_address: BleAddress,
    peer_capabilities: BleRoleCapabilities,
) -> ColumbaConnectionRole {
    match (local_capabilities, peer_capabilities) {
        (BleRoleCapabilities::DualRole, BleRoleCapabilities::PeripheralOnly) => {
            ColumbaConnectionRole::Dial
        }
        (BleRoleCapabilities::PeripheralOnly, BleRoleCapabilities::DualRole) => {
            ColumbaConnectionRole::Accept
        }
        (BleRoleCapabilities::PeripheralOnly, BleRoleCapabilities::PeripheralOnly) => {
            ColumbaConnectionRole::Unavailable
        }
        (BleRoleCapabilities::DualRole, BleRoleCapabilities::DualRole) => {
            match local_address.cmp(&peer_address) {
                core::cmp::Ordering::Less => ColumbaConnectionRole::Dial,
                core::cmp::Ordering::Greater => ColumbaConnectionRole::Accept,
                core::cmp::Ordering::Equal => ColumbaConnectionRole::Unavailable,
            }
        }
    }
}

struct AdWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> AdWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn put(&mut self, ad_type: u8, body: &[u8]) -> Option<()> {
        let field_len = 1 + body.len();
        let end = self.pos + 1 + field_len;
        let slot = self.buf.get_mut(self.pos..end)?;
        slot[0] = u8::try_from(field_len).ok()?;
        slot[1] = ad_type;
        slot[2..].copy_from_slice(body);
        self.pos = end;
        Some(())
    }

    fn len(&self) -> usize {
        self.pos
    }
}

struct AdReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> AdReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
}

impl<'a> Iterator for AdReader<'a> {
    type Item = (u8, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        let field_len = *self.buf.get(self.pos)? as usize;
        if field_len == 0 {
            return None;
        }
        let ad_type = *self.buf.get(self.pos + 1)?;
        let body = self.buf.get(self.pos + 2..self.pos + 1 + field_len)?;
        self.pos += 1 + field_len;
        Some((ad_type, body))
    }
}
