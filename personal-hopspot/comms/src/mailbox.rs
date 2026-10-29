//! A note the other person could not take is left on one mailbox.
//!
//! The mailbox is an `lxmf.propagation` address. Every N10A app uses the same
//! one. A journal `mailbox` file, or `JOURNAL_MAILBOX`, overrides the built-in
//! address. `lxmd` keeps the address only while its identity file is kept.
//! Run it with `enable_node = yes` and `autopeer = no`, and do not give it peers.
//! A stock node refuses a stamp cheaper than 13, which is also what the default
//! target of 16 accepts once its flexibility of 3 is applied. A blank stamp is
//! refused, so the stamp searched for here is 13.

use std::path::Path;

use personal_rns::crypto::{
    hkdf_sha256, sha256, sha256_chunks, Sha256PrefixState, X25519PublicKey, X25519SecretKey,
    SHA256_OUTPUT_LEN,
};
use personal_rns::identity::{PrivateIdentityMaterial, PublicIdentityMaterial, ENCRYPTION_IV_LEN};
use personal_rns::routing::announce::derive_single_destination_hash;
use personal_rns::wire::DestinationHash;

/// `lxmf.propagation` on the home node. The journal `mailbox` file overrides this.
pub const BUILT_IN_MAILBOX: &str = "8df34462733b9045380cba23c38af204";

const PROPAGATION_APP: &str = "lxmf";
const PROPAGATION_ASPECTS: &[&str] = &["delivery"];
/// `LXMRouter.PROPAGATION_COST_MIN`. Anything lower is raised to this.
const STAMP_COST: u32 = 13;
/// `LXStamper.WORKBLOCK_EXPAND_ROUNDS_PN`.
const STAMP_ROUNDS: u32 = 1000;
const STAMP_LEN: usize = 32;
const HASH_LEN: usize = 16;
const SIGNATURE_LEN: usize = 64;

/// A mailbox message opened with a retained ratchet, or the identity key.
pub struct OpenedMail {
    pub source: [u8; HASH_LEN],
    pub payload: Vec<u8>,
    pub signature: [u8; SIGNATURE_LEN],
    /// `destination || source || payload || sha256(destination || source || payload)`.
    pub signed: Vec<u8>,
}

/// The propagation address, if one is configured.
pub fn configured(home: &Path) -> Option<DestinationHash> {
    let from_env = std::env::var("JOURNAL_MAILBOX").ok();
    let from_file = std::fs::read_to_string(home.join("mailbox")).ok();
    let text = from_env
        .as_deref()
        .or(from_file.as_deref())
        .unwrap_or(BUILT_IN_MAILBOX);
    let mut hex = String::new();
    for ch in text.chars() {
        if ch.is_ascii_hexdigit() {
            hex.push(ch);
        }
    }
    if hex.len() != HASH_LEN * 2 {
        return None;
    }
    let mut bytes = [0u8; HASH_LEN];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(DestinationHash::new(bytes))
}

/// The bytes to hand the mailbox: one stamped, encrypted note, wrapped for the link.
pub fn propagation_packet(
    ours: &PrivateIdentityMaterial,
    recipient: &PublicIdentityMaterial,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let lxmf = seal_for_delivery(ours, recipient, payload, None)?;
    let transient = sha256(&lxmf);
    let stamp = stamp_for(&transient, STAMP_COST, STAMP_ROUNDS)?;
    let mut stamped = lxmf;
    stamped.extend(stamp);
    Some(wrap_propagation(&stamped, crate::lxmf::unix_now()))
}

/// Opens `lxmf_data` after the mailbox has stripped the stamp.
/// `ratchets` are our secrets, newest first. A token none of them opens falls back to the identity key.
pub fn open_lxmf(
    ours: &PrivateIdentityMaterial,
    our_delivery: &DestinationHash,
    lxmf_data: &[u8],
    ratchets: &[X25519SecretKey],
) -> Option<OpenedMail> {
    if lxmf_data.len() < HASH_LEN + SIGNATURE_LEN {
        return None;
    }
    let (destination, ciphertext) = lxmf_data.split_at(HASH_LEN);
    if destination != our_delivery.as_bytes() {
        return None;
    }
    let mut plain = vec![0u8; ciphertext.len()];
    let len = ours
        .decrypt_with_ratchets(ratchets, ciphertext, &mut plain)
        .ok()?;
    plain.truncate(len);
    if plain.len() < HASH_LEN + SIGNATURE_LEN {
        return None;
    }
    let mut source = [0u8; HASH_LEN];
    source.copy_from_slice(&plain[..HASH_LEN]);
    let mut signature = [0u8; SIGNATURE_LEN];
    signature.copy_from_slice(&plain[HASH_LEN..HASH_LEN + SIGNATURE_LEN]);
    let payload = plain[HASH_LEN + SIGNATURE_LEN..].to_vec();
    let message_hash = sha256_chunks(&[destination, &source, &payload]);
    let mut signed = Vec::with_capacity(HASH_LEN + HASH_LEN + payload.len() + SHA256_OUTPUT_LEN);
    signed.extend(destination);
    signed.extend(source);
    signed.extend(&payload);
    signed.extend(message_hash);
    Some(OpenedMail {
        source,
        payload,
        signature,
        signed,
    })
}

/// `[nil, nil]` asks the mailbox which notes it is holding for us.
pub fn list_query() -> Vec<u8> {
    vec![0x92, 0xc0, 0xc0]
}

/// Asks for `wants` and names nothing we already hold. The limit is kilobytes.
pub fn fetch_query(wants: &[[u8; STAMP_LEN]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x93);
    push_id_array(&mut out, wants);
    out.push(0x90);
    out.push(0xcb);
    out.extend_from_slice(&1024.0f64.to_be_bytes());
    out
}

/// `[nil, haves]` drops the notes we have taken. `haves` are hashes of the unstamped bytes.
pub fn purge_query(haves: &[[u8; STAMP_LEN]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x92);
    out.push(0xc0);
    push_id_array(&mut out, haves);
    out
}

/// A msgpack array of binary values. An error code, or anything else, is `None`.
pub fn read_bins(bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut reader = Reader { bytes };
    let count = reader.array_len()?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        items.push(reader.bin()?);
    }
    if !reader.bytes.is_empty() {
        return None;
    }
    Some(items)
}

/// The list reply: one 32-byte id per note.
pub fn read_ids(bytes: &[u8]) -> Option<Vec<[u8; STAMP_LEN]>> {
    let bins = read_bins(bytes)?;
    let mut ids = Vec::with_capacity(bins.len());
    for bin in bins {
        ids.push(bin.try_into().ok()?);
    }
    Some(ids)
}

/// The signed message a direct link carries: destination, source, signature, then the note.
pub(crate) fn sign_for_link(
    ours: &PrivateIdentityMaterial,
    recipient: &PublicIdentityMaterial,
    payload: &[u8],
) -> Option<Vec<u8>> {
    let signed = sign_note(ours, recipient, payload)?;
    let mut wire = Vec::with_capacity(HASH_LEN + HASH_LEN + SIGNATURE_LEN + signed.payload.len());
    wire.extend(signed.destination.as_bytes());
    wire.extend(signed.source);
    wire.extend(signed.signature);
    wire.extend(signed.payload);
    Some(wire)
}

/// Opens a signed message addressed to `our_delivery`. The bytes start with that address.
pub(crate) fn open_signed(our_delivery: &DestinationHash, wire: &[u8]) -> Option<OpenedMail> {
    if wire.len() < HASH_LEN + HASH_LEN + SIGNATURE_LEN
        || &wire[..HASH_LEN] != our_delivery.as_bytes()
    {
        return None;
    }
    open_body(our_delivery, &wire[HASH_LEN..])
}

/// Opens one packet addressed to `our_delivery`. The address is the packet's destination, so the bytes start with the source.
pub(crate) fn open_packet(our_delivery: &DestinationHash, plaintext: &[u8]) -> Option<OpenedMail> {
    open_body(our_delivery, plaintext)
}

fn open_body(our_delivery: &DestinationHash, body: &[u8]) -> Option<OpenedMail> {
    if body.len() < HASH_LEN + SIGNATURE_LEN {
        return None;
    }
    let mut source = [0u8; HASH_LEN];
    source.copy_from_slice(&body[..HASH_LEN]);
    let mut signature = [0u8; SIGNATURE_LEN];
    signature.copy_from_slice(&body[HASH_LEN..HASH_LEN + SIGNATURE_LEN]);
    let payload = crate::lxmf::payload_for_signature(&body[HASH_LEN + SIGNATURE_LEN..])?;
    let message_hash = sha256_chunks(&[our_delivery.as_bytes(), &source, &payload]);
    let mut signed = Vec::with_capacity(HASH_LEN + HASH_LEN + payload.len() + SHA256_OUTPUT_LEN);
    signed.extend(our_delivery.as_bytes());
    signed.extend(source);
    signed.extend(&payload);
    signed.extend(message_hash);
    Some(OpenedMail {
        source,
        payload,
        signature,
        signed,
    })
}

struct SignedNote {
    destination: DestinationHash,
    source: [u8; HASH_LEN],
    signature: [u8; SIGNATURE_LEN],
    payload: Vec<u8>,
}

fn sign_note(
    ours: &PrivateIdentityMaterial,
    recipient: &PublicIdentityMaterial,
    payload: &[u8],
) -> Option<SignedNote> {
    let destination = derive_single_destination_hash(
        &recipient.identity_hash(),
        PROPAGATION_APP,
        PROPAGATION_ASPECTS,
    )
    .ok()?;
    let source_destination =
        derive_single_destination_hash(&ours.identity_hash(), PROPAGATION_APP, PROPAGATION_ASPECTS)
            .ok()?;
    let source = *source_destination.as_bytes();
    let message_hash = sha256_chunks(&[destination.as_bytes(), &source, payload]);
    let mut signed = Vec::with_capacity(HASH_LEN + HASH_LEN + payload.len() + SHA256_OUTPUT_LEN);
    signed.extend(destination.as_bytes());
    signed.extend(source);
    signed.extend(payload);
    signed.extend(message_hash);
    let signature = ours.sign(&signed).0;
    Some(SignedNote {
        destination,
        source,
        signature,
        payload: payload.to_vec(),
    })
}

/// Seals a note to `ratchet` when the path announced one, otherwise to the identity encryption key.
pub(crate) fn seal_for_delivery(
    ours: &PrivateIdentityMaterial,
    recipient: &PublicIdentityMaterial,
    payload: &[u8],
    ratchet: Option<&X25519PublicKey>,
) -> Option<Vec<u8>> {
    let ephemeral = X25519SecretKey::new(random_bytes()?);
    let iv = random_bytes()?;
    seal_lxmf_with(ours, recipient, payload, &ephemeral, &iv, ratchet)
}

fn seal_lxmf_with(
    ours: &PrivateIdentityMaterial,
    recipient: &PublicIdentityMaterial,
    payload: &[u8],
    ephemeral: &X25519SecretKey,
    iv: &[u8; ENCRYPTION_IV_LEN],
    ratchet: Option<&X25519PublicKey>,
) -> Option<Vec<u8>> {
    let signed = sign_note(ours, recipient, payload)?;
    let destination = signed.destination;
    let mut plaintext = Vec::with_capacity(HASH_LEN + SIGNATURE_LEN + signed.payload.len());
    plaintext.extend(signed.source);
    plaintext.extend(signed.signature);
    plaintext.extend(&signed.payload);
    let mut ciphertext = vec![0u8; plaintext.len() + 256];
    let len = match ratchet {
        Some(ratchet) => recipient
            .encrypt_to_ratchet(ratchet, ephemeral, iv, &plaintext, &mut ciphertext)
            .ok()?,
        None => recipient
            .encrypt(ephemeral, iv, &plaintext, &mut ciphertext)
            .ok()?,
    };
    ciphertext.truncate(len);
    let mut lxmf = Vec::with_capacity(HASH_LEN + ciphertext.len());
    lxmf.extend(destination.as_bytes());
    lxmf.extend(ciphertext);
    Some(lxmf)
}

fn wrap_propagation(stamped: &[u8], unix: f64) -> Vec<u8> {
    let mut out = Vec::with_capacity(stamped.len() + 24);
    out.push(0x92);
    out.push(0xcb);
    out.extend_from_slice(&unix.to_be_bytes());
    out.push(0x91);
    push_bin(&mut out, stamped);
    out
}

fn stamp_for(material: &[u8; STAMP_LEN], cost: u32, rounds: u32) -> Option<[u8; STAMP_LEN]> {
    let block = workblock(material, rounds);
    let prefix = Sha256PrefixState::absorb(&[&block]);
    let tries = 1u32.checked_shl(cost.saturating_add(4)).unwrap_or(u32::MAX);
    for _ in 0..tries {
        let stamp = random_bytes()?;
        let hash = prefix.digest_with_suffix(&stamp);
        if hash_meets(&hash, cost) {
            return Some(stamp);
        }
    }
    None
}

fn workblock(material: &[u8; STAMP_LEN], rounds: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(rounds as usize * 256);
    for n in 0..rounds {
        let (packed, len) = msgpack_uint(n);
        let salt = sha256_chunks(&[material, &packed[..len]]);
        out.extend(hkdf_sha256::<256>(material, &salt, b""));
    }
    out
}

fn hash_meets(hash: &[u8; STAMP_LEN], cost: u32) -> bool {
    if cost == 0 {
        return true;
    }
    if cost > 256 {
        return false;
    }
    let mut limit = [0u8; STAMP_LEN];
    let from_msb = (cost - 1) as usize;
    limit[from_msb / 8] = 1 << (7 - (from_msb % 8));
    hash <= &limit
}

fn msgpack_uint(n: u32) -> ([u8; 5], usize) {
    if n <= 0x7f {
        ([n as u8, 0, 0, 0, 0], 1)
    } else if n <= 0xff {
        ([0xcc, n as u8, 0, 0, 0], 2)
    } else if n <= 0xffff {
        let value = (n as u16).to_be_bytes();
        ([0xcd, value[0], value[1], 0, 0], 3)
    } else {
        let value = n.to_be_bytes();
        ([0xce, value[0], value[1], value[2], value[3]], 5)
    }
}

fn random_bytes<const N: usize>() -> Option<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::getrandom(&mut bytes).ok()?;
    Some(bytes)
}

fn push_id_array(out: &mut Vec<u8>, ids: &[[u8; STAMP_LEN]]) {
    let count = ids.len();
    if count <= 15 {
        out.push(0x90 | count as u8);
    } else if count <= u16::MAX as usize {
        out.push(0xdc);
        out.extend_from_slice(&(count as u16).to_be_bytes());
    } else {
        out.push(0xdd);
        out.extend_from_slice(&(count as u32).to_be_bytes());
    }
    for id in ids {
        push_bin(out, id);
    }
}

fn push_bin(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = bytes.len();
    if len <= u8::MAX as usize {
        out.push(0xc4);
        out.push(len as u8);
    } else if len <= u16::MAX as usize {
        out.push(0xc5);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0xc6);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    }
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn array_len(&mut self) -> Option<usize> {
        let marker = self.u8()?;
        if (0x90..=0x9f).contains(&marker) {
            return Some((marker & 0x0f) as usize);
        }
        match marker {
            0xdc => Some(self.u16()? as usize),
            0xdd => usize::try_from(self.u32()?).ok(),
            _ => None,
        }
    }

    fn bin(&mut self) -> Option<Vec<u8>> {
        let marker = self.u8()?;
        let len = match marker {
            0xc4 => self.u8()? as usize,
            0xc5 => self.u16()? as usize,
            0xc6 => usize::try_from(self.u32()?).ok()?,
            _ => return None,
        };
        Some(self.take(len)?.to_vec())
    }

    fn u8(&mut self) -> Option<u8> {
        let (first, rest) = self.bytes.split_first()?;
        self.bytes = rest;
        Some(*first)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_be_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        if self.bytes.len() < len {
            return None;
        }
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Some(head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use personal_rns::crypto::Ed25519Signature;
    use personal_rns::identity::IDENTITY_SECRET_KEY_LEN;

    fn secret(encryption: u8, signing: u8) -> PrivateIdentityMaterial {
        let mut bytes = [0u8; IDENTITY_SECRET_KEY_LEN];
        bytes[..32].fill(encryption);
        bytes[32..].fill(signing);
        PrivateIdentityMaterial::from_bytes(bytes)
    }

    #[test]
    fn the_first_stamp_block_matches_the_hkdf_the_mailbox_uses() {
        let material = [7u8; 32];
        let block = workblock(&material, 1);
        assert_eq!(
            &block[..32],
            &hex_32("21aa27279ac6de772957f4d308c8bedf2eaf97c661003cf31728355e613e1102")
        );
    }

    #[test]
    fn a_hash_meets_the_stamp_cost_when_it_is_at_or_under_the_limit() {
        assert!(hash_meets(&[0u8; 32], 13));
        let mut at = [0u8; 32];
        at[1] = 0x08;
        assert!(hash_meets(&at, 13));
        at[31] = 1;
        assert!(!hash_meets(&at, 13));
        assert!(hash_meets(
            &[
                0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0
            ],
            1
        ));
        assert!(!hash_meets(
            &[
                0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 1
            ],
            1
        ));
    }

    #[test]
    fn a_cheap_stamp_meets_its_cost() {
        let material = [9u8; 32];
        let stamp = stamp_for(&material, 1, 1).expect("stamp");
        let block = workblock(&material, 1);
        let hash = Sha256PrefixState::absorb(&[&block]).digest_with_suffix(&stamp);
        assert!(hash_meets(&hash, 1));
    }

    #[test]
    fn a_sealed_note_opens_with_the_recipients_identity_key() {
        let sender = secret(0x44, 0x33);
        let recipient = secret(0x22, 0x11);
        let payload = crate::lxmf::pack_note("The pears are ready.", 1_700_000_000.0);
        let ephemeral = X25519SecretKey::new([0x55; 32]);
        let iv = [0x66; ENCRYPTION_IV_LEN];
        let lxmf = seal_lxmf_with(
            &sender,
            &recipient.public(),
            &payload,
            &ephemeral,
            &iv,
            None,
        )
        .expect("sealed");
        let delivery = derive_single_destination_hash(
            &recipient.identity_hash(),
            PROPAGATION_APP,
            PROPAGATION_ASPECTS,
        )
        .expect("delivery address");
        let opened = open_lxmf(&recipient, &delivery, &lxmf, &[]).expect("opened");
        assert_eq!(opened.payload, payload);
        let sender_delivery = derive_single_destination_hash(
            &sender.identity_hash(),
            PROPAGATION_APP,
            PROPAGATION_ASPECTS,
        )
        .expect("sender address");
        assert_eq!(opened.source, *sender_delivery.as_bytes());
        sender
            .public()
            .verify(&opened.signed, &Ed25519Signature(opened.signature))
            .expect("signature");
        assert!(open_lxmf(&sender, &delivery, &lxmf, &[]).is_none());
    }

    #[test]
    fn a_signed_note_opens_on_a_link_and_as_a_packet() {
        let sender = secret(0x44, 0x33);
        let recipient = secret(0x22, 0x11);
        let payload = crate::lxmf::pack_note("The pears are ready.", 1_700_000_000.0);
        let wire = sign_for_link(&sender, &recipient.public(), &payload).expect("signed");
        let delivery = derive_single_destination_hash(
            &recipient.identity_hash(),
            PROPAGATION_APP,
            PROPAGATION_ASPECTS,
        )
        .expect("delivery address");
        let opened = open_signed(&delivery, &wire).expect("link note");
        assert_eq!(opened.payload, payload);
        sender
            .public()
            .verify(&opened.signed, &Ed25519Signature(opened.signature))
            .expect("signature");
        let packet = open_packet(&delivery, &wire[HASH_LEN..]).expect("packet");
        assert_eq!(packet.payload, payload);
        assert_eq!(packet.signature, opened.signature);
        assert!(open_signed(&delivery, &wire[HASH_LEN..]).is_none());
        let mut stamped = wire.clone();
        let payload_at = HASH_LEN + HASH_LEN + SIGNATURE_LEN;
        assert_eq!(stamped[payload_at], 0x94);
        stamped[payload_at] = 0x95;
        stamped.extend_from_slice(&[0xc4, 0x01, 0xab]);
        let opened = open_signed(&delivery, &stamped).expect("stamp left out");
        assert_eq!(opened.payload, payload);
        sender
            .public()
            .verify(&opened.signed, &Ed25519Signature(opened.signature))
            .expect("signature over the unstamped note");
    }

    #[test]
    fn a_note_sealed_to_a_ratchet_opens_only_with_that_secret() {
        use personal_rns::crypto::x25519_public_key;

        let sender = secret(0x44, 0x33);
        let recipient = secret(0x22, 0x11);
        let ratchet = X25519SecretKey::new([0x77; 32]);
        let ratchet_public = x25519_public_key(&ratchet);
        let payload = crate::lxmf::pack_note("The pears are ready.", 1_700_000_000.0);
        let ephemeral = X25519SecretKey::new([0x55; 32]);
        let iv = [0x66; ENCRYPTION_IV_LEN];
        let lxmf = seal_lxmf_with(
            &sender,
            &recipient.public(),
            &payload,
            &ephemeral,
            &iv,
            Some(&ratchet_public),
        )
        .expect("sealed");
        let delivery = derive_single_destination_hash(
            &recipient.identity_hash(),
            PROPAGATION_APP,
            PROPAGATION_ASPECTS,
        )
        .expect("delivery address");
        assert!(open_lxmf(&recipient, &delivery, &lxmf, &[]).is_none());
        let opened = open_lxmf(&recipient, &delivery, &lxmf, &[ratchet]).expect("opened");
        assert_eq!(opened.payload, payload);
    }

    #[test]
    fn an_identity_key_seal_still_opens_beside_a_retained_ratchet() {
        let sender = secret(0x44, 0x33);
        let recipient = secret(0x22, 0x11);
        let payload = crate::lxmf::pack_note("Left on the identity key.", 1_700_000_000.0);
        let ephemeral = X25519SecretKey::new([0x55; 32]);
        let iv = [0x66; ENCRYPTION_IV_LEN];
        let lxmf = seal_lxmf_with(
            &sender,
            &recipient.public(),
            &payload,
            &ephemeral,
            &iv,
            None,
        )
        .expect("sealed");
        let delivery = derive_single_destination_hash(
            &recipient.identity_hash(),
            PROPAGATION_APP,
            PROPAGATION_ASPECTS,
        )
        .expect("delivery address");
        let other = X25519SecretKey::new([0x77; 32]);
        let opened = open_lxmf(&recipient, &delivery, &lxmf, &[other]).expect("opened");
        assert_eq!(opened.payload, payload);
    }

    #[test]
    fn list_and_fetch_requests_are_the_arrays_the_mailbox_reads() {
        assert_eq!(list_query(), [0x92, 0xc0, 0xc0]);
        let id = [0xab; 32];
        let fetch = fetch_query(&[id]);
        assert_eq!(&fetch[..3], &[0x93, 0x91, 0xc4]);
        assert_eq!(fetch[3], 32);
        let purge = purge_query(&[id]);
        assert_eq!(&purge[..2], &[0x92, 0xc0]);
        let listed = read_ids(&{
            let mut bytes = vec![0x91, 0xc4, 32];
            bytes.extend(id);
            bytes
        })
        .expect("ids");
        assert_eq!(listed, vec![id]);
        assert!(read_ids(&[0xcc, 0xf0]).is_none());
    }

    #[test]
    fn the_mailbox_file_overrides_the_built_in_address() {
        let directory =
            std::env::temp_dir().join(format!("optio-mailbox-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("dir");
        let built_in = configured(&directory).expect("built-in address");
        assert_eq!(built_in.as_bytes()[0], 0x8d);
        std::fs::write(directory.join("mailbox"), "ab".repeat(16) + "\n").expect("file");
        let hash = configured(&directory).expect("hash");
        assert_eq!(hash.as_bytes()[0], 0xab);
        let _ = std::fs::remove_dir_all(&directory);
    }

    fn hex_32(text: &str) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (index, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex");
        }
        out
    }
}
