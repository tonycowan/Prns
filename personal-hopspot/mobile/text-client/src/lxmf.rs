//! Minimal LXMF packet codec (Sideband-compatible plain texts).

use personal_rns::crypto::sha256;
use personal_rns::identity::in_memory::InMemoryNodeIdentity;
use personal_rns::identity::IdentitySigner;
use personal_rns::DestinationHash;

const DEST_LEN: usize = 16;
const SIGNATURE_LEN: usize = 64;
const FULL_HEADER_LEN: usize = DEST_LEN * 2 + SIGNATURE_LEN;
const OPPORTUNISTIC_HEADER_LEN: usize = DEST_LEN + SIGNATURE_LEN;

/// msgpack `[bin8("Personal Text"), nil]` — Sideband-style announce app_data.
pub const ANNOUNCE_APP_DATA: &[u8] = &[
    0x92, 0xc4, 0x0d, b'P', b'e', b'r', b's', b'o', b'n', b'a', b'l', b' ', b'T', b'e', b'x',
    b't', 0xc0,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMessage {
    pub source_hex: String,
    pub title: String,
    pub content: String,
}

/// Build RNS packet plaintext for opportunistic LXMF: `source ‖ signature ‖ msgpack(payload)`.
pub fn pack_opportunistic(
    destination: DestinationHash,
    source: DestinationHash,
    identity: &InMemoryNodeIdentity,
    title: &str,
    content: &str,
) -> Result<Vec<u8>, String> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let packed_payload = encode_payload(timestamp, title, content)?;

    let mut hashed_part = Vec::with_capacity(DEST_LEN * 2 + packed_payload.len());
    hashed_part.extend_from_slice(destination.as_bytes());
    hashed_part.extend_from_slice(source.as_bytes());
    hashed_part.extend_from_slice(&packed_payload);

    let message_hash = sha256(&hashed_part);
    let mut signed_part = Vec::with_capacity(hashed_part.len() + message_hash.len());
    signed_part.extend_from_slice(&hashed_part);
    signed_part.extend_from_slice(&message_hash);

    let signature = identity.sign(&signed_part);

    let mut out = Vec::with_capacity(OPPORTUNISTIC_HEADER_LEN + packed_payload.len());
    out.extend_from_slice(source.as_bytes());
    out.extend_from_slice(&signature.0);
    out.extend_from_slice(&packed_payload);
    Ok(out)
}

fn encode_payload(timestamp: f64, title: &str, content: &str) -> Result<Vec<u8>, String> {
    let mut packed_payload = Vec::with_capacity(64 + title.len() + content.len());
    rmp::encode::write_array_len(&mut packed_payload, 4).map_err(|e| e.to_string())?;
    rmp::encode::write_f64(&mut packed_payload, timestamp).map_err(|e| e.to_string())?;
    rmp::encode::write_bin(&mut packed_payload, title.as_bytes()).map_err(|e| e.to_string())?;
    rmp::encode::write_bin(&mut packed_payload, content.as_bytes()).map_err(|e| e.to_string())?;
    rmp::encode::write_map_len(&mut packed_payload, 0).map_err(|e| e.to_string())?;
    Ok(packed_payload)
}

/// Parse LXMF bytes from either wire shape Sideband uses:
/// - DIRECT / link / resource: `dest ‖ source ‖ signature ‖ msgpack`
/// - OPPORTUNISTIC single-packet: `source ‖ signature ‖ msgpack`
pub fn unpack_lxmf_bytes(data: &[u8]) -> Option<ParsedMessage> {
    if let Some(parsed) = unpack_full_packed(data) {
        return Some(parsed);
    }
    unpack_opportunistic_plaintext(data)
}

fn unpack_full_packed(data: &[u8]) -> Option<ParsedMessage> {
    if data.len() < FULL_HEADER_LEN + 1 {
        return None;
    }
    let source = &data[DEST_LEN..DEST_LEN * 2];
    let packed_payload = &data[FULL_HEADER_LEN..];
    let (title, content) = unpack_payload_fields(packed_payload)?;
    Some(ParsedMessage {
        source_hex: crate::model::hex_bytes(source),
        title,
        content,
    })
}

/// Parse opportunistic LXMF plaintext (`source ‖ signature ‖ msgpack`).
pub fn unpack_opportunistic_plaintext(plaintext: &[u8]) -> Option<ParsedMessage> {
    if plaintext.len() < OPPORTUNISTIC_HEADER_LEN + 1 {
        return None;
    }
    let source = &plaintext[..DEST_LEN];
    let packed_payload = &plaintext[OPPORTUNISTIC_HEADER_LEN..];
    let (title, content) = unpack_payload_fields(packed_payload)?;
    Some(ParsedMessage {
        source_hex: crate::model::hex_bytes(source),
        title,
        content,
    })
}

fn unpack_payload_fields(packed: &[u8]) -> Option<(String, String)> {
    let value = rmpv::decode::read_value(&mut &packed[..]).ok()?;
    let array = value.as_array()?;
    if array.len() < 4 {
        return None;
    }
    Some((value_to_string(&array[1]), value_to_string(&array[2])))
}

fn value_to_string(value: &rmpv::Value) -> String {
    match value {
        rmpv::Value::String(s) => s.as_str().unwrap_or("").to_string(),
        rmpv::Value::Binary(b) => String::from_utf8_lossy(b).into_owned(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use personal_rns::engine::RatchetPolicy;
    use personal_rns::identity::{Zeroizing, IDENTITY_SECRET_KEY_LEN};
    use personal_rns::routing::{LinkRequestPolicy, ProofStrategy};
    use personal_rns::runtime::{
        try_generate_identity_secret, PreConfiguredDestination, ServeMyRequestEndpoints,
    };

    fn sample_identity() -> (InMemoryNodeIdentity, DestinationHash, Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>) {
        let secret = try_generate_identity_secret().expect("entropy");
        let identity = InMemoryNodeIdentity::from_secret_key_bytes(&secret);
        let mut dest_secret = Zeroizing::new([0u8; IDENTITY_SECRET_KEY_LEN]);
        dest_secret.copy_from_slice(secret.as_ref());
        let dest = PreConfiguredDestination::Single {
            resource_strategy: personal_rns::routing::links::resources::ResourceStrategy::AcceptNone,
            app_name: "lxmf",
            aspects: &["delivery"],
            identity: dest_secret,
            announce_app_data: ANNOUNCE_APP_DATA,
            proof: ProofStrategy::ProveAll,
            link_requests: LinkRequestPolicy::AcceptAll,
            ratchet: RatchetPolicy::NoRatchets,
            maximum_request_bytes: Default::default(),
            request_endpoints: ServeMyRequestEndpoints::No,
        };
        let source_hash = dest.destination_hash().expect("name");
        // PreConfiguredDestination consumed dest_secret; rebuild for return unused.
        let mut dest_secret = Zeroizing::new([0u8; IDENTITY_SECRET_KEY_LEN]);
        dest_secret.copy_from_slice(secret.as_ref());
        (identity, source_hash, dest_secret)
    }

    #[test]
    fn pack_then_unpack_opportunistic() {
        let (identity, source_hash, _) = sample_identity();
        let peer = DestinationHash::new([0x11; 16]);
        let packed = pack_opportunistic(peer, source_hash, &identity, "", "hello mesh").unwrap();
        let parsed = unpack_lxmf_bytes(&packed).expect("parse");
        assert_eq!(parsed.content, "hello mesh");
        assert_eq!(
            parsed.source_hex,
            crate::model::hex_bytes(source_hash.as_bytes())
        );
    }

    #[test]
    fn unpack_full_direct_shape() {
        let (identity, source_hash, _) = sample_identity();
        let peer = DestinationHash::new([0x22; 16]);
        let opportunistic =
            pack_opportunistic(peer, source_hash, &identity, "hi", "direct body").unwrap();
        // DIRECT wire data prefixes the destination hash.
        let mut full = Vec::with_capacity(DEST_LEN + opportunistic.len());
        full.extend_from_slice(peer.as_bytes());
        full.extend_from_slice(&opportunistic);
        let parsed = unpack_lxmf_bytes(&full).expect("parse full");
        assert_eq!(parsed.title, "hi");
        assert_eq!(parsed.content, "direct body");
        assert_eq!(
            parsed.source_hex,
            crate::model::hex_bytes(source_hash.as_bytes())
        );
    }
}
