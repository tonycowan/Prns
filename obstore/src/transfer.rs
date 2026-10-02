use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::config::FetchPolicy;
use crate::store::{
    claims_from_document, hex_encode, Claims, ObjectId, ObjectStore, PublishedObject, StoreError,
    PIECE_SIZE,
};

const MAGIC: &[u8; 4] = b"OBXF";
const VERSION: u8 = 2;
const OP_PATH: u8 = 1;
const OP_OFFER: u8 = 2;
const OP_RELEASE: u8 = 3;
const OP_REQUEST: u8 = 4;
const OP_WHO_HAS: u8 = 5;
const OP_CLAIM: u8 = 6;
const OP_NEED_MANIFEST: u8 = 7;
const OP_MANIFEST: u8 = 8;
const OP_GIVE_PIECE: u8 = 9;
const OP_CATALOG: u8 = 10;
const OP_PREVIEW: u8 = 11;
const WHO_HAS_BACKOFF_MS: u64 = 30;
const WHO_HAS_RING_WAIT: Duration = Duration::from_millis(200);
const REQ_DONE: u8 = 0;
const REQ_MANIFEST: u8 = 1;
const REQ_PIECE: u8 = 2;
const REQ_REJECT: u8 = 255;
const STATUS_YES: u8 = 0;
const STATUS_NO: u8 = 1;
const STATUS_RELAY: u8 = 2;
const PATH_ASK_LOCAL: u8 = 0;
const PATH_ASK_RESOLVE: u8 = 1;
const MAX_RELEASE_HOPS: u8 = 8;
const DESTINATION_LEN: usize = 16;
const IDENTITY_HASH_LEN: usize = 16;
const NAME_HASH_LEN: usize = 10;
const IDENTITY_LEN: usize = 64;
const OBJECT_TRANSFER_NAME: &str = "reticulum.object-transfer";
const MAX_MANIFEST: usize = 1024 * 1024;
const MAX_ENVELOPE: usize = 64 * 1024;
const MAX_CATALOG_ENVELOPE: usize = MAX_ENVELOPE + 512;
const MAX_OBJECT_LEN: u64 = 32 * 1024 * 1024;
const MAX_CATALOG: usize = 4096;
const CATALOG_OK: u8 = 0;
const CATALOG_ERROR: u8 = 1;

#[derive(Debug)]
pub enum TransferError {
    Io(io::Error),
    Store(StoreError),
    Message(String),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "{source}"),
            Self::Store(source) => write!(formatter, "{source}"),
            Self::Message(message) => write!(formatter, "{message}"),
        }
    }
}

impl From<io::Error> for TransferError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<StoreError> for TransferError {
    fn from(source: StoreError) -> Self {
        Self::Store(source)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Destination([u8; DESTINATION_LEN]);

impl Destination {
    pub fn parse(text: &str) -> Result<Self, TransferError> {
        let text = text.trim();
        let bytes = decode_hex(text).ok_or_else(|| {
            TransferError::Message("destination address must be 32 hex characters".to_string())
        })?;
        let bytes: [u8; DESTINATION_LEN] = bytes.try_into().map_err(|_| {
            TransferError::Message("destination address must be 32 hex characters".to_string())
        })?;
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; DESTINATION_LEN] {
        &self.0
    }

    pub fn as_hex(&self) -> String {
        hex_encode(&self.0)
    }
}

/// The object-transfer address for a prnsd stack identity.
/// `sha256(sha256("reticulum.object-transfer")[..10] ‖ identity_hash)[..16]`,
/// the same destination derivation remote control uses for `reticulum.remote.control`.
pub fn object_transfer_address(identity_secret: &[u8; IDENTITY_LEN]) -> Destination {
    StackIdentity::from_secret(identity_secret).destination()
}

pub fn read_transport_identity(config_dir: &Path) -> Result<[u8; IDENTITY_LEN], TransferError> {
    let path = config_dir.join("storage").join("transport_identity");
    let bytes = fs::read(&path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            TransferError::Message(format!("stack identity {} is missing", path.display()))
        } else {
            TransferError::Io(error)
        }
    })?;
    if bytes.len() != IDENTITY_LEN {
        return Err(TransferError::Message(format!(
            "stack identity {} holds {} bytes, expected {IDENTITY_LEN}",
            path.display(),
            bytes.len()
        )));
    }
    let mut secret = [0_u8; IDENTITY_LEN];
    secret.copy_from_slice(&bytes);
    Ok(secret)
}

pub fn load_object_transfer_address(config_dir: &Path) -> Result<Destination, TransferError> {
    let mut secret = read_transport_identity(config_dir)?;
    let address = object_transfer_address(&secret);
    secret.zeroize();
    Ok(address)
}

/// The stack identity that owns an object-transfer address.
///
/// The address is a truncated hash of the two public keys, so a request carries
/// the public keys and an Ed25519 signature. The signing seed is the second half
/// of the transport identity.
#[derive(Clone)]
struct StackIdentity {
    encryption_public: [u8; 32],
    signing_public: [u8; 32],
    signing_seed: [u8; 32],
}

impl StackIdentity {
    fn from_secret(secret: &[u8; IDENTITY_LEN]) -> Self {
        let mut encryption_secret = [0_u8; 32];
        encryption_secret.copy_from_slice(&secret[..32]);
        let mut signing_seed = [0_u8; 32];
        signing_seed.copy_from_slice(&secret[32..]);
        let encryption_public = PublicKey::from(&StaticSecret::from(encryption_secret)).to_bytes();
        encryption_secret.zeroize();
        let signing_public = SigningKey::from_bytes(&signing_seed)
            .verifying_key()
            .to_bytes();
        Self {
            encryption_public,
            signing_public,
            signing_seed,
        }
    }

    fn destination(&self) -> Destination {
        Destination(destination_hash(
            OBJECT_TRANSFER_NAME,
            &identity_hash(&self.encryption_public, &self.signing_public),
        ))
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        SigningKey::from_bytes(&self.signing_seed)
            .sign(message)
            .to_bytes()
    }
}

impl Drop for StackIdentity {
    fn drop(&mut self) {
        self.signing_seed.zeroize();
    }
}

fn identity_hash(
    encryption_public: &[u8; 32],
    signing_public: &[u8; 32],
) -> [u8; IDENTITY_HASH_LEN] {
    let mut material = [0_u8; 64];
    material[..32].copy_from_slice(encryption_public);
    material[32..].copy_from_slice(signing_public);
    let digest = Sha256::digest(material);
    let mut hash = [0_u8; IDENTITY_HASH_LEN];
    hash.copy_from_slice(&digest[..IDENTITY_HASH_LEN]);
    hash
}

fn destination_hash(name: &str, identity: &[u8; IDENTITY_HASH_LEN]) -> [u8; DESTINATION_LEN] {
    let name_digest = Sha256::digest(name.as_bytes());
    let mut material = [0_u8; NAME_HASH_LEN + IDENTITY_HASH_LEN];
    material[..NAME_HASH_LEN].copy_from_slice(&name_digest[..NAME_HASH_LEN]);
    material[NAME_HASH_LEN..].copy_from_slice(identity);
    let digest = Sha256::digest(material);
    let mut hash = [0_u8; DESTINATION_LEN];
    hash.copy_from_slice(&digest[..DESTINATION_LEN]);
    hash
}

/// A byte stream to one neighbor, or to one object-transfer destination.
pub trait Pipe: Read + Write + Send {}

impl<T: Read + Write + Send> Pipe for T {}

/// How this stack reaches other object-transfer services.
///
/// The stand-in connects to the paths listed in `peers`. prnsd replaces that with
/// a one-hop plain broadcast for floods and a link to one destination for a pull.
pub trait Neighborhood: Send + Sync {
    fn flood(&self) -> Vec<Box<dyn Pipe>>;
    fn open(&self, destination: &Destination) -> Result<Box<dyn Pipe>, TransferError>;
}

struct PeerNeighborhood {
    root: PathBuf,
}

fn peer_list(root: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(root.join("peers")) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

impl PeerNeighborhood {
    fn peers(&self) -> Vec<PathBuf> {
        peer_list(&self.root)
    }

    fn stored_path(&self, destination: &Destination) -> Option<PathBuf> {
        let text = fs::read_to_string(self.root.join("paths").join(destination.as_hex())).ok()?;
        let path = text.trim();
        if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    }

    fn remember_path(&self, destination: &Destination, peer: &Path) -> io::Result<()> {
        fs::write(
            self.root.join("paths").join(destination.as_hex()),
            format!("{}\n", peer.display()),
        )
    }

    fn resolve(&self, destination: &Destination) -> Result<PathBuf, TransferError> {
        if let Some(path) = self.stored_path(destination) {
            return Ok(path);
        }
        for peer in self.peers() {
            match ask_path(&peer, destination, PATH_ASK_RESOLVE)? {
                PathAnswer::Here => {
                    self.remember_path(destination, &peer)?;
                    return Ok(peer);
                }
                PathAnswer::Relay(path) => {
                    self.remember_path(destination, &path)?;
                    return Ok(path);
                }
                PathAnswer::No => {}
            }
        }
        Err(TransferError::Message(format!(
            "no path to {}",
            destination.as_hex()
        )))
    }
}

impl Neighborhood for PeerNeighborhood {
    fn flood(&self) -> Vec<Box<dyn Pipe>> {
        self.peers()
            .into_iter()
            .filter_map(|peer| {
                crate::ipc::LocalStream::connect(&peer)
                    .ok()
                    .map(|stream| Box::new(stream) as Box<dyn Pipe>)
            })
            .collect()
    }

    fn open(&self, destination: &Destination) -> Result<Box<dyn Pipe>, TransferError> {
        let peer = self.resolve(destination)?;
        let stream = crate::ipc::LocalStream::connect(&peer).map_err(|error| {
            TransferError::Message(format!("connect {}: {error}", peer.display()))
        })?;
        Ok(Box::new(stream))
    }
}

#[derive(Clone)]
pub struct ObjectTransfer {
    root: PathBuf,
    destination: Destination,
    identity: StackIdentity,
    policy: FetchPolicy,
    neighborhood: Arc<dyn Neighborhood>,
}

impl ObjectTransfer {
    pub fn open(
        root: impl Into<PathBuf>,
        identity_secret: &[u8; IDENTITY_LEN],
    ) -> Result<Self, TransferError> {
        let root = root.into();
        fs::create_dir_all(root.join("paths"))?;
        let identity = StackIdentity::from_secret(identity_secret);
        let destination = identity.destination();
        let neighborhood = Arc::new(PeerNeighborhood { root: root.clone() });
        Ok(Self {
            root,
            destination,
            identity,
            policy: FetchPolicy::default(),
            neighborhood,
        })
    }

    pub fn set_fetch_policy(&mut self, policy: FetchPolicy) {
        self.policy = policy;
    }

    /// Replace the Unix peer list. Call this before the transfer is shared with listeners.
    pub fn set_neighborhood(&mut self, neighborhood: Arc<dyn Neighborhood>) {
        self.neighborhood = neighborhood;
    }

    fn own_destination(&self) -> Destination {
        self.destination
    }

    fn remember_release(
        &self,
        id: &ObjectId,
        origin: &Destination,
        hops: u8,
        envelope: &str,
    ) -> io::Result<bool> {
        let directory = self.root.join("releases");
        fs::create_dir_all(&directory)?;
        let path = directory.join(id.as_str());
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = match options.open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(error) => return Err(error),
        };
        writeln!(file, "origin {}", origin.as_hex())?;
        writeln!(file, "hops-left {hops}")?;
        write!(file, "{envelope}")?;
        Ok(true)
    }

    fn who_has_dir(&self, id: &ObjectId, index: u32, requester: &Destination) -> PathBuf {
        self.root
            .join("who-has")
            .join(id.as_str())
            .join(index.to_string())
            .join(requester.as_hex())
    }

    fn remember_manifest_ask(&self, ask: &ManifestAsk) -> io::Result<bool> {
        let directory = self
            .root
            .join("manifest-asks")
            .join(ask.id.as_str())
            .join(ask.requester.as_hex());
        fs::create_dir_all(&directory)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(format!("seen-{}", ask.hops)))
        {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn remember_who_has(
        &self,
        id: &ObjectId,
        index: u32,
        requester: &Destination,
        hops: u8,
    ) -> io::Result<bool> {
        let directory = self.who_has_dir(id, index, requester);
        fs::create_dir_all(&directory)?;
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(format!("seen-{hops}")))
        {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Record a possession claim. A later claim from the same holder replaces the
    /// stored rate only when it is faster.
    fn record_claim(
        &self,
        id: &ObjectId,
        index: u32,
        requester: &Destination,
        holder: &Destination,
        bytes_per_second: u64,
    ) -> io::Result<bool> {
        let directory = self.who_has_dir(id, index, requester).join("claims");
        fs::create_dir_all(&directory)?;
        let path = directory.join(holder.as_hex());
        if let Some(existing) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok())
        {
            if bytes_per_second <= existing {
                return Ok(false);
            }
        }
        fs::write(&path, format!("{bytes_per_second}\n"))?;
        Ok(true)
    }

    fn recorded_claims(
        &self,
        id: &ObjectId,
        index: u32,
        requester: &Destination,
    ) -> Result<Vec<(Destination, u64)>, TransferError> {
        let directory = self.who_has_dir(id, index, requester).join("claims");
        let Ok(entries) = fs::read_dir(&directory) else {
            return Ok(Vec::new());
        };
        let mut claims = Vec::new();
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(holder) = Destination::parse(&name) else {
                continue;
            };
            let text = fs::read_to_string(entry.path())?;
            let Ok(bytes_per_second) = text.trim().parse::<u64>() else {
                continue;
            };
            claims.push((holder, bytes_per_second));
        }
        Ok(claims)
    }
}

fn narrow_bytes_per_second(carried: u64, interface: u64) -> u64 {
    carried.min(interface)
}

pub fn offer(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    object_id: &str,
    destination: &str,
) -> Result<(), TransferError> {
    let id = ObjectId::parse(object_id).map_err(TransferError::Store)?;
    let destination = Destination::parse(destination)?;
    let documents = store.ensure_transfer_documents(&id)?;
    let (object_manifest, object_envelope) = store.read_object_documents(&id)?;
    let mut stream = transfer.neighborhood.open(&destination)?;
    serve_holder(
        store,
        &mut stream,
        &id,
        &object_manifest,
        &object_envelope,
        &documents.envelope,
    )
}

pub fn announce_release(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    object_id: &str,
    hops: u8,
) -> Result<(), TransferError> {
    if hops > MAX_RELEASE_HOPS {
        return Err(TransferError::Message(format!(
            "release hops must be at most {MAX_RELEASE_HOPS}"
        )));
    }
    let id = ObjectId::parse(object_id).map_err(TransferError::Store)?;
    if !store.has_object(&id) {
        return Err(TransferError::Store(StoreError::ObjectNotFound {
            id: id.as_str().to_string(),
        }));
    }
    store.ensure_transfer_documents(&id)?;
    let origin = transfer.own_destination();
    let envelope = store.read_loa_envelope(&id)?.unwrap_or_default();
    store.verify_trusted_object_envelope(&envelope, id.as_str())?;
    transfer.remember_release(&id, &origin, hops, &envelope)?;
    forward_release(transfer, &id, &origin, hops, &envelope)
}

/// Pull the LOA envelope and length of each published object at `destination`.
pub fn fetch_catalog(
    transfer: &ObjectTransfer,
    destination: &str,
) -> Result<Vec<PublishedObject>, TransferError> {
    let destination = Destination::parse(destination)?;
    let mut stream = transfer.neighborhood.open(&destination)?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_CATALOG])?;
    stream.flush()?;
    read_catalog(&mut stream)
}

/// Pull piece 0 and the object length from `destination`.
pub fn fetch_preview(
    transfer: &ObjectTransfer,
    destination: &str,
    object_id: &str,
) -> Result<(u64, Vec<u8>), TransferError> {
    let destination = Destination::parse(destination)?;
    let id = ObjectId::parse(object_id)?;
    let mut stream = transfer.neighborhood.open(&destination)?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_PREVIEW])?;
    stream.write_all(id.as_str().as_bytes())?;
    stream.flush()?;
    read_preview(&mut stream)
}

fn serve_catalog<S: Read + Write>(
    store: &ObjectStore,
    stream: &mut S,
) -> Result<(), TransferError> {
    let objects = match store.published_objects() {
        Ok(objects) if objects.len() <= MAX_CATALOG => objects,
        Ok(_) => {
            write_catalog_error(stream, "catalog is too large")?;
            return Ok(());
        }
        Err(error) => {
            write_catalog_error(stream, &error.to_string())?;
            return Ok(());
        }
    };
    stream.write_all(&[CATALOG_OK])?;
    let count = u32::try_from(objects.len()).unwrap_or(u32::MAX);
    stream.write_all(&count.to_le_bytes())?;
    for object in objects {
        stream.write_all(&object.length.to_le_bytes())?;
        write_blob(stream, object.envelope.as_bytes())?;
    }
    stream.flush()?;
    Ok(())
}

fn serve_preview<S: Read + Write>(
    store: &ObjectStore,
    stream: &mut S,
) -> Result<(), TransferError> {
    let mut id_bytes = [0_u8; 64];
    stream.read_exact(&mut id_bytes)?;
    let id_text = match std::str::from_utf8(&id_bytes) {
        Ok(id) => id,
        Err(_) => {
            write_catalog_error(stream, "object id must be 64 hex characters")?;
            return Ok(());
        }
    };
    let id = match ObjectId::parse(id_text) {
        Ok(id) => id,
        Err(error) => {
            write_catalog_error(stream, &error.to_string())?;
            return Ok(());
        }
    };
    let opened = store.open_data(&id);
    let piece = if opened.is_ok() {
        store.read_piece(&id, 0)
    } else {
        Err(StoreError::ObjectNotFound {
            id: id.as_str().to_string(),
        })
    };
    match (opened, piece) {
        (Ok((_, length)), Ok(piece)) => {
            stream.write_all(&[CATALOG_OK])?;
            stream.write_all(&length.to_le_bytes())?;
            write_blob(stream, &piece)?;
            Ok(())
        }
        (Ok((_, 0)), Err(_)) => {
            stream.write_all(&[CATALOG_OK])?;
            stream.write_all(&0_u64.to_le_bytes())?;
            write_blob(stream, &[])?;
            Ok(())
        }
        (Err(error), _) | (_, Err(error)) => {
            write_catalog_error(stream, &error.to_string())?;
            Ok(())
        }
    }
}

fn write_catalog_error<S: Write>(stream: &mut S, message: &str) -> Result<(), TransferError> {
    stream.write_all(&[CATALOG_ERROR])?;
    write_blob(stream, message.as_bytes())?;
    Ok(())
}

fn read_catalog<S: Read>(stream: &mut S) -> Result<Vec<PublishedObject>, TransferError> {
    let mut status = [0_u8; 1];
    stream.read_exact(&mut status)?;
    if status[0] != CATALOG_OK {
        let message = read_text(stream, MAX_ENVELOPE)?;
        return Err(TransferError::Message(message));
    }
    let mut count_bytes = [0_u8; 4];
    stream.read_exact(&mut count_bytes)?;
    let count = usize::try_from(u32::from_le_bytes(count_bytes)).unwrap_or(usize::MAX);
    if count > MAX_CATALOG {
        return Err(TransferError::Message("catalog is too large".to_string()));
    }
    let mut objects = Vec::with_capacity(count);
    for _ in 0..count {
        let mut length_bytes = [0_u8; 8];
        stream.read_exact(&mut length_bytes)?;
        let length = u64::from_le_bytes(length_bytes);
        let envelope = read_text(stream, MAX_CATALOG_ENVELOPE)?;
        let id = object_id_from_envelope(&envelope)?;
        objects.push(PublishedObject {
            id,
            length,
            envelope,
        });
    }
    Ok(objects)
}

fn read_preview<S: Read>(stream: &mut S) -> Result<(u64, Vec<u8>), TransferError> {
    let mut status = [0_u8; 1];
    stream.read_exact(&mut status)?;
    if status[0] != CATALOG_OK {
        let message = read_text(stream, MAX_ENVELOPE)?;
        return Err(TransferError::Message(message));
    }
    let mut length_bytes = [0_u8; 8];
    stream.read_exact(&mut length_bytes)?;
    let length = u64::from_le_bytes(length_bytes);
    let piece = read_blob(stream, PIECE_SIZE)?;
    Ok((length, piece))
}

fn object_id_from_envelope(envelope: &str) -> Result<ObjectId, TransferError> {
    let id = envelope
        .lines()
        .find_map(|line| line.strip_prefix("object-id "))
        .ok_or_else(|| TransferError::Message("LOA envelope has no object id".to_string()))?;
    ObjectId::parse(id).map_err(TransferError::Store)
}

pub fn serve_peer<S: Read + Write>(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    stream: &mut S,
    interface_bytes_per_second: u64,
) -> Result<(), TransferError> {
    let mut prefix = [0_u8; 6];
    stream.read_exact(&mut prefix)?;
    if &prefix[0..4] != MAGIC || prefix[4] != VERSION {
        return Err(TransferError::Message(
            "unrecognized object transfer message".to_string(),
        ));
    }
    match prefix[5] {
        OP_PATH => answer_path(transfer, stream),
        OP_OFFER => accept_and_pull(store, stream, None),
        OP_RELEASE => receive_release(store, transfer, stream),
        OP_REQUEST => serve_request(store, stream),
        OP_WHO_HAS => receive_who_has(store, transfer, stream, interface_bytes_per_second),
        OP_CLAIM => receive_claim(transfer, stream),
        OP_NEED_MANIFEST => receive_manifest_ask(store, transfer, stream),
        OP_MANIFEST => receive_manifest(store, stream),
        OP_GIVE_PIECE => give_piece(store, stream),
        OP_CATALOG => serve_catalog(store, stream),
        OP_PREVIEW => serve_preview(store, stream),
        _ => Err(TransferError::Message(
            "unsupported object transfer operation".to_string(),
        )),
    }
}

fn serve_holder<S: Read + Write>(
    store: &ObjectStore,
    stream: &mut S,
    id: &ObjectId,
    object_manifest: &str,
    object_envelope: &str,
    transfer_envelope: &str,
) -> Result<(), TransferError> {
    write_offer(
        stream,
        id,
        object_manifest,
        object_envelope,
        transfer_envelope,
    )?;
    loop {
        match read_request(stream)? {
            Request::Done => return Ok(()),
            Request::Manifest => {
                let documents = store.ensure_transfer_documents(id)?;
                write_blob(stream, documents.manifest.as_bytes())?;
            }
            Request::Piece(index) => {
                let piece = store.read_piece(id, index).map_err(|error| {
                    TransferError::Message(format!("piece {index} of {}: {error}", id.as_str()))
                })?;
                write_blob(stream, &piece)?;
            }
        }
    }
}

fn serve_request<S: Read + Write>(
    store: &ObjectStore,
    stream: &mut S,
) -> Result<(), TransferError> {
    let mut id_bytes = [0_u8; 64];
    stream.read_exact(&mut id_bytes)?;
    let id_text = std::str::from_utf8(&id_bytes)
        .map_err(|_| TransferError::Message("object id must be 64 hex characters".to_string()))?;
    let id = ObjectId::parse(id_text)?;
    let documents = store.ensure_transfer_documents(&id)?;
    let (object_manifest, object_envelope) = store.read_object_documents(&id)?;
    serve_holder(
        store,
        stream,
        &id,
        &object_manifest,
        &object_envelope,
        &documents.envelope,
    )
}

fn receive_release<S: Read + Write>(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    stream: &mut S,
) -> Result<(), TransferError> {
    let mut id_bytes = [0_u8; 64];
    stream.read_exact(&mut id_bytes)?;
    let id_text = std::str::from_utf8(&id_bytes)
        .map_err(|_| TransferError::Message("object id must be 64 hex characters".to_string()))?;
    let id = ObjectId::parse(id_text)?;
    let mut origin_bytes = [0_u8; DESTINATION_LEN];
    stream.read_exact(&mut origin_bytes)?;
    let mut hops = [0_u8; 1];
    stream.read_exact(&mut hops)?;
    let origin = Destination(origin_bytes);
    let hops = hops[0];
    let envelope = read_text(stream, MAX_ENVELOPE)?;
    if hops > MAX_RELEASE_HOPS {
        return Err(TransferError::Message(format!(
            "release hops must be at most {MAX_RELEASE_HOPS}"
        )));
    }
    store.verify_trusted_object_envelope(&envelope, id.as_str())?;
    if !transfer.remember_release(&id, &origin, hops, &envelope)? {
        return Ok(());
    }
    if hops > 0 {
        forward_release(transfer, &id, &origin, hops - 1, &envelope)?;
    }
    if let Err(error) = consider_release(store, transfer, &id, &envelope) {
        eprintln!("object-services: release {}: {error}", &id.as_str()[..7]);
    }
    Ok(())
}

fn consider_release(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    id: &ObjectId,
    envelope: &str,
) -> Result<(), TransferError> {
    if store.has_object(id) {
        return Ok(());
    }
    let claims = claims_from_document(envelope)?;
    if !release_matches(&transfer.policy, &claims, store) {
        if transfer.policy.interested() {
            eprintln!(
                "object-services: release {} does not match this node",
                &id.as_str()[..7]
            );
        }
        return Ok(());
    }
    eprintln!("object-services: fetching release {}", &id.as_str()[..7]);
    seek_manifest(store, transfer, id)?;
    let manifest = store
        .read_transfer_manifest(id)?
        .ok_or_else(|| TransferError::Message(format!("object {id} has no transfer manifest")))?;
    let parsed = parse_manifest(&manifest)?;
    store.note_object_length(id, parsed.length)?;
    for (index, _) in parsed.pieces.iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        take_piece(store, transfer, id, index)?;
    }
    assemble_object(store, id, &manifest)
}

fn release_matches(policy: &FetchPolicy, claims: &Claims, store: &ObjectStore) -> bool {
    cdn_matches(policy, claims) || firmware_matches(policy, claims, store)
}

fn cdn_matches(policy: &FetchPolicy, claims: &Claims) -> bool {
    if !policy.cdn {
        return false;
    }
    let Some(group) = claims.claim("CDN-group") else {
        return false;
    };
    group == "all"
        || policy
            .cdn_groups
            .iter()
            .any(|configured| configured == group)
}

fn firmware_matches(policy: &FetchPolicy, claims: &Claims, store: &ObjectStore) -> bool {
    let Some(mode) = claims.claim("mode") else {
        return false;
    };
    let enabled = match mode {
        "auto-update" => policy.auto_update,
        "auto-stage" => policy.auto_stage,
        _ => false,
    };
    if !enabled {
        return false;
    }
    if claims.claim("flash-mode") != Some("ota") || claims.claim("object-type") != Some("firmware")
    {
        return false;
    }
    if claims.claim("board") != Some(policy.board.as_str()) || policy.board.is_empty() {
        return false;
    }
    let Some(version) = claims.claim("version") else {
        return false;
    };
    !store.has_firmware_version(&policy.board, version)
}

impl FetchPolicy {
    fn interested(&self) -> bool {
        self.cdn || self.auto_update || self.auto_stage
    }
}

fn seek_manifest(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    id: &ObjectId,
) -> Result<(), TransferError> {
    if store.read_transfer_manifest(id)?.is_some() {
        return Ok(());
    }
    let requester = transfer.own_destination();
    for hops in 0..=MAX_RELEASE_HOPS {
        let mut ask = ManifestAsk {
            id: id.clone(),
            requester,
            hops,
            encryption_public: transfer.identity.encryption_public,
            signing_public: transfer.identity.signing_public,
            signature: [0; 64],
        };
        ask.signature = transfer.identity.sign(&manifest_ask_statement(&ask));
        if !transfer.remember_manifest_ask(&ask)? {
            continue;
        }
        forward_manifest_ask(transfer, &ask)?;
        let deadline = Instant::now() + WHO_HAS_RING_WAIT;
        while Instant::now() < deadline {
            if store.read_transfer_manifest(id)?.is_some()
                && store.read_transfer_envelope(id)?.is_some()
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    Err(TransferError::Message(format!(
        "no transfer manifest answered for {id}"
    )))
}

fn take_piece(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    id: &ObjectId,
    index: u32,
) -> Result<(), TransferError> {
    let holder = seek_piece(store, transfer, id.as_str(), index)?;
    let manifest = store
        .read_transfer_manifest(id)?
        .ok_or_else(|| TransferError::Message(format!("object {id} has no transfer manifest")))?;
    let parsed = parse_manifest(&manifest)?;
    let hash_text = parsed
        .pieces
        .get(usize::try_from(index).unwrap_or(usize::MAX))
        .ok_or_else(|| TransferError::Message(format!("transfer manifest has no piece {index}")))?;
    let hash = decode_fixed(hash_text)?;
    let piece_length = piece_span(parsed.length, index, PIECE_SIZE)?;
    let piece_size = u32::try_from(PIECE_SIZE).unwrap_or(u32::MAX);
    let mut stream = transfer
        .neighborhood
        .open(&holder)
        .map_err(|error| TransferError::Message(format!("connect {}: {error}", holder.as_hex())))?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_GIVE_PIECE])?;
    stream.write_all(id.as_str().as_bytes())?;
    stream.write_all(&index.to_le_bytes())?;
    stream.write_all(&hash)?;
    stream.write_all(&piece_size.to_le_bytes())?;
    stream.write_all(&piece_length.to_le_bytes())?;
    stream.flush()?;
    let mut status = [0_u8; 1];
    stream.read_exact(&mut status)?;
    if status[0] != STATUS_YES {
        return Err(TransferError::Message(format!(
            "holder {} has no piece {index} of {id}",
            holder.as_hex()
        )));
    }
    let mut piece = vec![0_u8; usize::try_from(piece_length).unwrap_or(usize::MAX)];
    stream.read_exact(&mut piece)?;
    if Sha256::digest(&piece).as_slice() != hash {
        return Err(TransferError::Message(format!(
            "piece {index} of {id} does not match the transfer manifest"
        )));
    }
    store.store_piece(id, index, &piece)?;
    eprintln!(
        "object-services: received piece {} from {}",
        &hash_text[..7],
        &holder.as_hex()[..7]
    );
    Ok(())
}

fn assemble_object(
    store: &ObjectStore,
    id: &ObjectId,
    manifest: &str,
) -> Result<(), TransferError> {
    let parsed = parse_manifest(manifest)?;
    let mut assembled = Vec::new();
    for (index, hash) in parsed.pieces.iter().enumerate() {
        let piece = store.read_piece(id, u32::try_from(index).unwrap_or(u32::MAX))?;
        if hex_encode(&Sha256::digest(&piece)) != *hash {
            return Err(TransferError::Message(format!(
                "piece {index} of {id} does not match the transfer manifest"
            )));
        }
        assembled.extend_from_slice(&piece);
    }
    store.store_received_bytes(id, &assembled)?;
    Ok(())
}

fn receive_manifest_ask<S: Read + Write>(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    stream: &mut S,
) -> Result<(), TransferError> {
    let ask = read_manifest_ask(stream)?;
    if ask.hops > MAX_RELEASE_HOPS {
        return Err(TransferError::Message(format!(
            "manifest request hops must be at most {MAX_RELEASE_HOPS}"
        )));
    }
    verify_manifest_ask(&ask)?;
    if ask.requester == transfer.own_destination() {
        return Ok(());
    }
    if !transfer.remember_manifest_ask(&ask)? {
        return Ok(());
    }
    if ask.hops > 0 {
        let mut relay = ask;
        relay.hops -= 1;
        return forward_manifest_ask(transfer, &relay);
    }
    answer_manifest(store, transfer, &ask)
}

fn answer_manifest(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    ask: &ManifestAsk,
) -> Result<(), TransferError> {
    let Some(manifest) = store.read_transfer_manifest(&ask.id)? else {
        return Ok(());
    };
    let Some(envelope) = store.read_transfer_envelope(&ask.id)? else {
        return Ok(());
    };
    let wait = who_has_backoff();
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if let Ok(mut stream) = transfer.neighborhood.open(&ask.requester) {
        write_manifest(&mut stream, ask, &manifest, &envelope)?;
    }
    Ok(())
}

fn forward_manifest_ask(transfer: &ObjectTransfer, ask: &ManifestAsk) -> Result<(), TransferError> {
    for mut stream in transfer.neighborhood.flood() {
        let _ = write_manifest_ask(&mut stream, ask);
    }
    Ok(())
}

fn write_manifest_ask<S: Write>(stream: &mut S, ask: &ManifestAsk) -> Result<(), TransferError> {
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_NEED_MANIFEST])?;
    stream.write_all(ask.id.as_str().as_bytes())?;
    stream.write_all(ask.requester.as_bytes())?;
    stream.write_all(&[ask.hops])?;
    stream.write_all(&ask.encryption_public)?;
    stream.write_all(&ask.signing_public)?;
    stream.write_all(&ask.signature)?;
    stream.flush()?;
    Ok(())
}

fn read_manifest_ask<S: Read>(stream: &mut S) -> Result<ManifestAsk, TransferError> {
    let mut id_bytes = [0_u8; 64];
    stream.read_exact(&mut id_bytes)?;
    let id_text = std::str::from_utf8(&id_bytes)
        .map_err(|_| TransferError::Message("object id must be 64 hex characters".to_string()))?;
    let mut requester = [0_u8; DESTINATION_LEN];
    stream.read_exact(&mut requester)?;
    let mut hops = [0_u8; 1];
    stream.read_exact(&mut hops)?;
    let mut encryption_public = [0_u8; 32];
    stream.read_exact(&mut encryption_public)?;
    let mut signing_public = [0_u8; 32];
    stream.read_exact(&mut signing_public)?;
    let mut signature = [0_u8; 64];
    stream.read_exact(&mut signature)?;
    Ok(ManifestAsk {
        id: ObjectId::parse(id_text)?,
        requester: Destination(requester),
        hops: hops[0],
        encryption_public,
        signing_public,
        signature,
    })
}

fn write_manifest<S: Write>(
    stream: &mut S,
    ask: &ManifestAsk,
    manifest: &str,
    envelope: &str,
) -> Result<(), TransferError> {
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_MANIFEST])?;
    stream.write_all(ask.id.as_str().as_bytes())?;
    stream.write_all(ask.requester.as_bytes())?;
    write_blob(stream, manifest.as_bytes())?;
    write_blob(stream, envelope.as_bytes())?;
    Ok(())
}

fn receive_manifest<S: Read + Write>(
    store: &ObjectStore,
    stream: &mut S,
) -> Result<(), TransferError> {
    let mut id_bytes = [0_u8; 64];
    stream.read_exact(&mut id_bytes)?;
    let id_text = std::str::from_utf8(&id_bytes)
        .map_err(|_| TransferError::Message("object id must be 64 hex characters".to_string()))?;
    let id = ObjectId::parse(id_text)?;
    let mut requester = [0_u8; DESTINATION_LEN];
    stream.read_exact(&mut requester)?;
    let _requester = Destination(requester);
    let manifest = read_text(stream, MAX_MANIFEST)?;
    let envelope = read_text(stream, MAX_ENVELOPE)?;
    let parsed = parse_manifest(&manifest)?;
    if parsed.object_id != id.as_str() {
        return Err(TransferError::Message(
            "transfer manifest names a different object".to_string(),
        ));
    }
    store.store_transfer_documents(&id, &manifest, &envelope)?;
    Ok(())
}

fn give_piece<S: Read + Write>(store: &ObjectStore, stream: &mut S) -> Result<(), TransferError> {
    let mut id_bytes = [0_u8; 64];
    stream.read_exact(&mut id_bytes)?;
    let id_text = std::str::from_utf8(&id_bytes)
        .map_err(|_| TransferError::Message("object id must be 64 hex characters".to_string()))?;
    let id = ObjectId::parse(id_text)?;
    let mut index_bytes = [0_u8; 4];
    stream.read_exact(&mut index_bytes)?;
    let index = u32::from_le_bytes(index_bytes);
    let mut hash = [0_u8; 32];
    stream.read_exact(&mut hash)?;
    let mut piece_size = [0_u8; 4];
    stream.read_exact(&mut piece_size)?;
    let mut piece_length = [0_u8; 4];
    stream.read_exact(&mut piece_length)?;
    let piece_size = u32::from_le_bytes(piece_size);
    let piece_length = u32::from_le_bytes(piece_length);
    let matched = piece_matches(store, &id, index, &hash, piece_size, piece_length)?;
    if !matched {
        stream.write_all(&[STATUS_NO])?;
        stream.flush()?;
        return Ok(());
    }
    let Some(piece) = store.read_piece_window(&id, index, piece_size, piece_length)? else {
        stream.write_all(&[STATUS_NO])?;
        stream.flush()?;
        return Ok(());
    };
    stream.write_all(&[STATUS_YES])?;
    stream.write_all(&piece)?;
    stream.flush()?;
    Ok(())
}

struct ManifestAsk {
    id: ObjectId,
    requester: Destination,
    hops: u8,
    encryption_public: [u8; 32],
    signing_public: [u8; 32],
    signature: [u8; 64],
}

fn forward_release(
    transfer: &ObjectTransfer,
    id: &ObjectId,
    origin: &Destination,
    hops: u8,
    envelope: &str,
) -> Result<(), TransferError> {
    for mut stream in transfer.neighborhood.flood() {
        let _ = write_release(&mut stream, id, origin, hops, envelope);
    }
    Ok(())
}

fn write_release<S: Write>(
    stream: &mut S,
    id: &ObjectId,
    origin: &Destination,
    hops: u8,
    envelope: &str,
) -> Result<(), TransferError> {
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_RELEASE])?;
    stream.write_all(id.as_str().as_bytes())?;
    stream.write_all(origin.as_bytes())?;
    stream.write_all(&[hops])?;
    write_blob(stream, envelope.as_bytes())?;
    Ok(())
}

/// Search outward for a piece. The first send uses 0 hops, so only direct neighbors are
/// included. A later send uses the next count. A node that receives 0 and has the piece
/// answers with a claim. A node that receives a count above 0 relays and stays quiet.
///
/// The who-has carries the lowest bytes/second of the interfaces it has crossed, and
/// the claim repeats that rate. A send is one packet on every interface, so the sender
/// leaves the carried rate unchanged. The node that receives it lowers the rate when
/// the arrival interface is slower. An offer at or above the node's minimum is taken.
/// Offers below it are held, and the fastest of those is used when the search ends
/// without a faster one.
pub fn seek_piece(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    object_id: &str,
    index: u32,
) -> Result<Destination, TransferError> {
    let id = ObjectId::parse(object_id).map_err(TransferError::Store)?;
    let manifest = store
        .read_transfer_manifest(&id)?
        .ok_or_else(|| TransferError::Message(format!("object {id} has no transfer manifest")))?;
    let parsed = parse_manifest(&manifest)?;
    if parsed.object_id != id.as_str() {
        return Err(TransferError::Message(
            "transfer manifest names a different object".to_string(),
        ));
    }
    let hash_text = parsed
        .pieces
        .get(usize::try_from(index).unwrap_or(usize::MAX))
        .ok_or_else(|| TransferError::Message(format!("transfer manifest has no piece {index}")))?;
    let hash = decode_fixed(hash_text)?;
    let piece_length = piece_span(parsed.length, index, PIECE_SIZE)?;
    let requester = transfer.own_destination();
    if piece_matches(store, &id, index, &hash, PIECE_SIZE as u32, piece_length)? {
        return Ok(requester);
    }
    let mut request = WhoHas {
        id: id.clone(),
        index,
        hash,
        piece_size: u32::try_from(PIECE_SIZE).unwrap_or(u32::MAX),
        piece_length,
        requester,
        hops: 0,
        bytes_per_second: u64::MAX,
        encryption_public: transfer.identity.encryption_public,
        signing_public: transfer.identity.signing_public,
        signature: [0; 64],
    };
    request.signature = transfer.identity.sign(&who_has_statement(&request));
    let threshold = transfer.policy.minimum_bytes_per_second;
    for hops in 0..=MAX_RELEASE_HOPS {
        request.hops = hops;
        request.bytes_per_second = u64::MAX;
        if !transfer.remember_who_has(&request.id, request.index, &request.requester, hops)? {
            continue;
        }
        forward_who_has(transfer, &request)?;
        let deadline = Instant::now() + WHO_HAS_RING_WAIT;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let claims = transfer.recorded_claims(&id, index, &requester)?;
        if let Some(holder) = holder_meeting(&claims, threshold) {
            return Ok(holder);
        }
    }
    let claims = transfer.recorded_claims(&id, index, &requester)?;
    if let Some((holder, bytes_per_second)) = fastest_claim(&claims) {
        eprintln!(
            "object-services: who-has piece {index} keeping {} at {bytes_per_second} B/s, below {threshold} B/s",
            &holder.as_hex()[..7]
        );
        return Ok(holder);
    }
    Err(TransferError::Message(format!(
        "no holder answered for piece {index} of {id}"
    )))
}

fn piece_span(length: u64, index: u32, piece_size: usize) -> Result<u32, TransferError> {
    let piece_size = u64::try_from(piece_size).unwrap_or(0);
    let start = u64::from(index)
        .checked_mul(piece_size)
        .ok_or_else(|| TransferError::Message(format!("piece {index} starts past the object")))?;
    if start >= length {
        return Err(TransferError::Message(format!(
            "piece {index} starts past the object"
        )));
    }
    let span = (length - start).min(piece_size);
    u32::try_from(span).map_err(|_| TransferError::Message(format!("piece {index} is too large")))
}

fn receive_who_has<S: Read + Write>(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    stream: &mut S,
    interface_bytes_per_second: u64,
) -> Result<(), TransferError> {
    let mut request = read_who_has(stream)?;
    if request.hops > MAX_RELEASE_HOPS {
        return Err(TransferError::Message(format!(
            "who-has hops must be at most {MAX_RELEASE_HOPS}"
        )));
    }
    verify_who_has(&request)?;
    request.bytes_per_second =
        narrow_bytes_per_second(request.bytes_per_second, interface_bytes_per_second);
    if request.requester == transfer.own_destination() {
        return Ok(());
    }
    if !transfer.remember_who_has(&request.id, request.index, &request.requester, request.hops)? {
        return Ok(());
    }
    if request.hops > 0 {
        let mut relay = request;
        relay.hops -= 1;
        return forward_who_has(transfer, &relay);
    }
    answer_who_has(store, transfer, &request)
}

fn answer_who_has(
    store: &ObjectStore,
    transfer: &ObjectTransfer,
    request: &WhoHas,
) -> Result<(), TransferError> {
    if request.piece_size == 0
        || request.piece_length == 0
        || request.piece_length > request.piece_size
    {
        return Ok(());
    }
    if !piece_matches(
        store,
        &request.id,
        request.index,
        &request.hash,
        request.piece_size,
        request.piece_length,
    )? {
        return Ok(());
    }
    let wait = who_has_backoff();
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let holder = transfer.own_destination();
    transfer.record_claim(
        &request.id,
        request.index,
        &request.requester,
        &holder,
        request.bytes_per_second,
    )?;
    let mut claim = Claim {
        id: request.id.clone(),
        index: request.index,
        hash: request.hash,
        requester: request.requester,
        holder,
        bytes_per_second: request.bytes_per_second,
        encryption_public: transfer.identity.encryption_public,
        signing_public: transfer.identity.signing_public,
        signature: [0; 64],
    };
    claim.signature = transfer.identity.sign(&claim_statement(&claim));
    forward_claim(transfer, &claim)?;
    if let Ok(mut stream) = transfer.neighborhood.open(&request.requester) {
        let _ = write_claim(&mut stream, &claim);
    }
    Ok(())
}

fn piece_matches(
    store: &ObjectStore,
    id: &ObjectId,
    index: u32,
    hash: &[u8; 32],
    piece_size: u32,
    piece_length: u32,
) -> Result<bool, TransferError> {
    let Some(bytes) = store.read_piece_window(id, index, piece_size, piece_length)? else {
        return Ok(false);
    };
    Ok(
        bytes.len() == usize::try_from(piece_length).unwrap_or(usize::MAX)
            && Sha256::digest(&bytes).as_slice() == hash,
    )
}

fn who_has_backoff() -> Duration {
    let mut byte = [0_u8; 1];
    let millis = match getrandom::getrandom(&mut byte) {
        Ok(()) => u64::from(byte[0]) % (WHO_HAS_BACKOFF_MS + 1),
        Err(_) => WHO_HAS_BACKOFF_MS,
    };
    Duration::from_millis(millis)
}

/// Send this one who-has on every interface. The carried rate stays as the
/// receiver left it; the next node applies the interface it arrives on.
fn forward_who_has(transfer: &ObjectTransfer, request: &WhoHas) -> Result<(), TransferError> {
    for mut stream in transfer.neighborhood.flood() {
        let _ = write_who_has(&mut stream, request);
    }
    Ok(())
}

fn forward_claim(transfer: &ObjectTransfer, claim: &Claim) -> Result<(), TransferError> {
    for mut stream in transfer.neighborhood.flood() {
        let _ = write_claim(&mut stream, claim);
    }
    Ok(())
}

fn write_who_has<S: Write>(stream: &mut S, request: &WhoHas) -> Result<(), TransferError> {
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_WHO_HAS])?;
    stream.write_all(request.id.as_str().as_bytes())?;
    stream.write_all(&request.index.to_le_bytes())?;
    stream.write_all(&request.hash)?;
    stream.write_all(&request.piece_size.to_le_bytes())?;
    stream.write_all(&request.piece_length.to_le_bytes())?;
    stream.write_all(request.requester.as_bytes())?;
    stream.write_all(&[request.hops])?;
    stream.write_all(&request.bytes_per_second.to_le_bytes())?;
    stream.write_all(&request.encryption_public)?;
    stream.write_all(&request.signing_public)?;
    stream.write_all(&request.signature)?;
    stream.flush()?;
    Ok(())
}

fn read_who_has<S: Read>(stream: &mut S) -> Result<WhoHas, TransferError> {
    let mut id_bytes = [0_u8; 64];
    stream.read_exact(&mut id_bytes)?;
    let id_text = std::str::from_utf8(&id_bytes)
        .map_err(|_| TransferError::Message("object id must be 64 hex characters".to_string()))?;
    let id = ObjectId::parse(id_text)?;
    let mut index_bytes = [0_u8; 4];
    stream.read_exact(&mut index_bytes)?;
    let mut hash = [0_u8; 32];
    stream.read_exact(&mut hash)?;
    let mut piece_size = [0_u8; 4];
    stream.read_exact(&mut piece_size)?;
    let mut piece_length = [0_u8; 4];
    stream.read_exact(&mut piece_length)?;
    let mut requester = [0_u8; DESTINATION_LEN];
    stream.read_exact(&mut requester)?;
    let mut hops = [0_u8; 1];
    stream.read_exact(&mut hops)?;
    let mut bytes_per_second = [0_u8; 8];
    stream.read_exact(&mut bytes_per_second)?;
    let mut encryption_public = [0_u8; 32];
    stream.read_exact(&mut encryption_public)?;
    let mut signing_public = [0_u8; 32];
    stream.read_exact(&mut signing_public)?;
    let mut signature = [0_u8; 64];
    stream.read_exact(&mut signature)?;
    Ok(WhoHas {
        id,
        index: u32::from_le_bytes(index_bytes),
        hash,
        piece_size: u32::from_le_bytes(piece_size),
        piece_length: u32::from_le_bytes(piece_length),
        requester: Destination(requester),
        hops: hops[0],
        bytes_per_second: u64::from_le_bytes(bytes_per_second),
        encryption_public,
        signing_public,
        signature,
    })
}

fn receive_claim<S: Read + Write>(
    transfer: &ObjectTransfer,
    stream: &mut S,
) -> Result<(), TransferError> {
    let claim = read_claim(stream)?;
    verify_claim(&claim)?;
    let fresh = transfer.record_claim(
        &claim.id,
        claim.index,
        &claim.requester,
        &claim.holder,
        claim.bytes_per_second,
    )?;
    if fresh && transfer.own_destination() == claim.requester {
        eprintln!(
            "object-services: who-has piece {} answered by {} at {} B/s",
            claim.index,
            &claim.holder.as_hex()[..7],
            claim.bytes_per_second
        );
    }
    Ok(())
}

fn write_claim<S: Write>(stream: &mut S, claim: &Claim) -> Result<(), TransferError> {
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_CLAIM])?;
    stream.write_all(claim.id.as_str().as_bytes())?;
    stream.write_all(&claim.index.to_le_bytes())?;
    stream.write_all(&claim.hash)?;
    stream.write_all(claim.requester.as_bytes())?;
    stream.write_all(claim.holder.as_bytes())?;
    stream.write_all(&claim.bytes_per_second.to_le_bytes())?;
    stream.write_all(&claim.encryption_public)?;
    stream.write_all(&claim.signing_public)?;
    stream.write_all(&claim.signature)?;
    stream.flush()?;
    Ok(())
}

fn read_claim<S: Read>(stream: &mut S) -> Result<Claim, TransferError> {
    let mut id_bytes = [0_u8; 64];
    stream.read_exact(&mut id_bytes)?;
    let id_text = std::str::from_utf8(&id_bytes)
        .map_err(|_| TransferError::Message("object id must be 64 hex characters".to_string()))?;
    let id = ObjectId::parse(id_text)?;
    let mut index_bytes = [0_u8; 4];
    stream.read_exact(&mut index_bytes)?;
    let mut hash = [0_u8; 32];
    stream.read_exact(&mut hash)?;
    let mut requester = [0_u8; DESTINATION_LEN];
    stream.read_exact(&mut requester)?;
    let mut holder = [0_u8; DESTINATION_LEN];
    stream.read_exact(&mut holder)?;
    let mut bytes_per_second = [0_u8; 8];
    stream.read_exact(&mut bytes_per_second)?;
    let mut encryption_public = [0_u8; 32];
    stream.read_exact(&mut encryption_public)?;
    let mut signing_public = [0_u8; 32];
    stream.read_exact(&mut signing_public)?;
    let mut signature = [0_u8; 64];
    stream.read_exact(&mut signature)?;
    Ok(Claim {
        id,
        index: u32::from_le_bytes(index_bytes),
        hash,
        requester: Destination(requester),
        holder: Destination(holder),
        bytes_per_second: u64::from_le_bytes(bytes_per_second),
        encryption_public,
        signing_public,
        signature,
    })
}

/// The fastest offer whose path is at least `threshold` bytes/second.
fn holder_meeting(claims: &[(Destination, u64)], threshold: u64) -> Option<Destination> {
    let mut best: Option<(Destination, u64)> = None;
    for (holder, bytes_per_second) in claims {
        if *bytes_per_second < threshold {
            continue;
        }
        let replace = match best {
            Some((_, rate)) => *bytes_per_second > rate,
            None => true,
        };
        if replace {
            best = Some((*holder, *bytes_per_second));
        }
    }
    best.map(|(holder, _)| holder)
}

fn fastest_claim(claims: &[(Destination, u64)]) -> Option<(Destination, u64)> {
    let mut best: Option<(Destination, u64)> = None;
    for (holder, bytes_per_second) in claims {
        let replace = match best {
            Some((_, rate)) => *bytes_per_second > rate,
            None => true,
        };
        if replace {
            best = Some((*holder, *bytes_per_second));
        }
    }
    best
}

fn decode_fixed(text: &str) -> Result<[u8; 32], TransferError> {
    let bytes = decode_hex(text)
        .filter(|bytes| bytes.len() == 32)
        .ok_or_else(|| {
            TransferError::Message("piece hash must be 64 hex characters".to_string())
        })?;
    let mut hash = [0_u8; 32];
    hash.copy_from_slice(&bytes);
    Ok(hash)
}

#[derive(Clone)]
struct WhoHas {
    id: ObjectId,
    index: u32,
    hash: [u8; 32],
    piece_size: u32,
    piece_length: u32,
    requester: Destination,
    hops: u8,
    bytes_per_second: u64,
    encryption_public: [u8; 32],
    signing_public: [u8; 32],
    signature: [u8; 64],
}

fn who_has_statement(request: &WhoHas) -> Vec<u8> {
    let mut message = Vec::with_capacity(7 + 64 + 4 + 32 + 4 + 4 + DESTINATION_LEN + 32 + 32);
    message.extend_from_slice(b"who-has");
    message.extend_from_slice(request.id.as_str().as_bytes());
    message.extend_from_slice(&request.index.to_le_bytes());
    message.extend_from_slice(&request.hash);
    message.extend_from_slice(&request.piece_size.to_le_bytes());
    message.extend_from_slice(&request.piece_length.to_le_bytes());
    message.extend_from_slice(request.requester.as_bytes());
    message.extend_from_slice(&request.encryption_public);
    message.extend_from_slice(&request.signing_public);
    message
}

fn manifest_ask_statement(ask: &ManifestAsk) -> Vec<u8> {
    let mut message = Vec::with_capacity(13 + 64 + DESTINATION_LEN + 32 + 32);
    message.extend_from_slice(b"need-manifest");
    message.extend_from_slice(ask.id.as_str().as_bytes());
    message.extend_from_slice(ask.requester.as_bytes());
    message.extend_from_slice(&ask.encryption_public);
    message.extend_from_slice(&ask.signing_public);
    message
}

fn verify_who_has(request: &WhoHas) -> Result<(), TransferError> {
    verify_requester_signature(
        &request.requester,
        &request.encryption_public,
        &request.signing_public,
        &request.signature,
        &who_has_statement(request),
        "who-has",
        "requester",
    )
}

fn verify_manifest_ask(ask: &ManifestAsk) -> Result<(), TransferError> {
    verify_requester_signature(
        &ask.requester,
        &ask.encryption_public,
        &ask.signing_public,
        &ask.signature,
        &manifest_ask_statement(ask),
        "manifest request",
        "requester",
    )
}

fn verify_requester_signature(
    signer: &Destination,
    encryption_public: &[u8; 32],
    signing_public: &[u8; 32],
    signature: &[u8; 64],
    message: &[u8],
    what: &str,
    role: &str,
) -> Result<(), TransferError> {
    let derived = Destination(destination_hash(
        OBJECT_TRANSFER_NAME,
        &identity_hash(encryption_public, signing_public),
    ));
    if &derived != signer {
        return Err(TransferError::Message(format!(
            "{what} {role} address does not match the identity"
        )));
    }
    let key = VerifyingKey::from_bytes(signing_public).map_err(|_| {
        TransferError::Message(format!(
            "{what} identity is not a valid Ed25519 verifying key"
        ))
    })?;
    let signature = Signature::from_slice(signature)
        .map_err(|_| TransferError::Message(format!("{what} signature is not 64 bytes")))?;
    key.verify(message, &signature).map_err(|_| {
        TransferError::Message(format!("{what} signature does not match the {role}"))
    })?;
    Ok(())
}

struct Claim {
    id: ObjectId,
    index: u32,
    hash: [u8; 32],
    requester: Destination,
    holder: Destination,
    bytes_per_second: u64,
    encryption_public: [u8; 32],
    signing_public: [u8; 32],
    signature: [u8; 64],
}

fn claim_statement(claim: &Claim) -> Vec<u8> {
    let mut message =
        Vec::with_capacity(5 + 64 + 4 + 32 + DESTINATION_LEN + DESTINATION_LEN + 8 + 32 + 32);
    message.extend_from_slice(b"claim");
    message.extend_from_slice(claim.id.as_str().as_bytes());
    message.extend_from_slice(&claim.index.to_le_bytes());
    message.extend_from_slice(&claim.hash);
    message.extend_from_slice(claim.requester.as_bytes());
    message.extend_from_slice(claim.holder.as_bytes());
    message.extend_from_slice(&claim.bytes_per_second.to_le_bytes());
    message.extend_from_slice(&claim.encryption_public);
    message.extend_from_slice(&claim.signing_public);
    message
}

fn verify_claim(claim: &Claim) -> Result<(), TransferError> {
    verify_requester_signature(
        &claim.holder,
        &claim.encryption_public,
        &claim.signing_public,
        &claim.signature,
        &claim_statement(claim),
        "claim",
        "holder",
    )
}

enum PathAnswer {
    Here,
    Relay(PathBuf),
    No,
}

fn ask_path(
    peer: &Path,
    destination: &Destination,
    depth: u8,
) -> Result<PathAnswer, TransferError> {
    let mut stream = match crate::ipc::LocalStream::connect(peer) {
        Ok(stream) => stream,
        Err(_) => return Ok(PathAnswer::No),
    };
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_PATH])?;
    stream.write_all(destination.as_bytes())?;
    stream.write_all(&[depth])?;
    stream.flush()?;
    let mut status = [0_u8; 1];
    stream.read_exact(&mut status)?;
    match status[0] {
        STATUS_YES => Ok(PathAnswer::Here),
        STATUS_RELAY => {
            let mut length_bytes = [0_u8; 2];
            stream.read_exact(&mut length_bytes)?;
            let length = usize::from(u16::from_le_bytes(length_bytes));
            if length > 4096 {
                return Err(TransferError::Message(
                    "relayed path is too long".to_string(),
                ));
            }
            let mut bytes = vec![0_u8; length];
            stream.read_exact(&mut bytes)?;
            let path = String::from_utf8(bytes)
                .map_err(|_| TransferError::Message("relayed path is not utf-8".to_string()))?;
            Ok(PathAnswer::Relay(PathBuf::from(path)))
        }
        _ => Ok(PathAnswer::No),
    }
}

fn peer_serves(peer: &Path, destination: &Destination) -> Result<bool, TransferError> {
    Ok(matches!(
        ask_path(peer, destination, PATH_ASK_LOCAL)?,
        PathAnswer::Here
    ))
}

fn answer_path<S: Read + Write>(
    transfer: &ObjectTransfer,
    stream: &mut S,
) -> Result<(), TransferError> {
    let mut requested = [0_u8; DESTINATION_LEN];
    stream.read_exact(&mut requested)?;
    let mut depth = [0_u8; 1];
    stream.read_exact(&mut depth)?;
    if transfer.own_destination().as_bytes() == &requested {
        stream.write_all(&[STATUS_YES])?;
        stream.flush()?;
        return Ok(());
    }
    if depth[0] == PATH_ASK_RESOLVE {
        for peer in peer_list(&transfer.root) {
            if peer_serves(&peer, &Destination(requested))? {
                let path = peer.display().to_string();
                let length = u16::try_from(path.len())
                    .map_err(|_| TransferError::Message("relayed path is too long".to_string()))?;
                stream.write_all(&[STATUS_RELAY])?;
                stream.write_all(&length.to_le_bytes())?;
                stream.write_all(path.as_bytes())?;
                stream.flush()?;
                return Ok(());
            }
        }
    }
    stream.write_all(&[STATUS_NO])?;
    stream.flush()?;
    Ok(())
}

fn write_offer<S: Write>(
    stream: &mut S,
    id: &ObjectId,
    object_manifest: &str,
    object_envelope: &str,
    transfer_envelope: &str,
) -> Result<(), TransferError> {
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_OFFER])?;
    stream.write_all(id.as_str().as_bytes())?;
    write_blob(stream, object_manifest.as_bytes())?;
    write_blob(stream, object_envelope.as_bytes())?;
    write_blob(stream, transfer_envelope.as_bytes())?;
    Ok(())
}

fn accept_and_pull<S: Read + Write>(
    store: &ObjectStore,
    stream: &mut S,
    source: Option<&Destination>,
) -> Result<(), TransferError> {
    let mut id_bytes = [0_u8; 64];
    stream.read_exact(&mut id_bytes)?;
    let id_text = std::str::from_utf8(&id_bytes)
        .map_err(|_| TransferError::Message("object id must be 64 hex characters".to_string()))?;
    let id = ObjectId::parse(id_text)?;
    let object_manifest = read_text(stream, MAX_ENVELOPE)?;
    let object_envelope = read_text(stream, MAX_ENVELOPE)?;
    let envelope = read_text(stream, MAX_ENVELOPE)?;
    if !object_envelope
        .lines()
        .any(|line| line.starts_with("signature "))
        || !envelope.lines().any(|line| line.starts_with("signature "))
    {
        write_reject(stream, "transfer declined")?;
        return Ok(());
    }
    if let Err(error) = store.store_object_documents(&id, &object_manifest, &object_envelope) {
        write_reject(stream, &error.to_string())?;
        return Ok(());
    }
    stream.write_all(&[REQ_MANIFEST])?;
    stream.flush()?;
    let manifest = read_text(stream, MAX_MANIFEST)?;
    let parsed = parse_manifest(&manifest)?;
    if parsed.object_id != id.as_str() {
        write_reject(stream, "transfer manifest names a different object")?;
        return Ok(());
    }
    if let Err(error) = store.store_transfer_documents(&id, &manifest, &envelope) {
        write_reject(stream, &error.to_string())?;
        return Ok(());
    }
    let mut assembled = Vec::new();
    for (index, hash) in parsed.pieces.iter().enumerate() {
        stream.write_all(&[REQ_PIECE])?;
        stream.write_all(&u32::try_from(index).unwrap_or(u32::MAX).to_le_bytes())?;
        stream.flush()?;
        let piece = read_blob(stream, PIECE_SIZE)?;
        if hex_encode(&Sha256::digest(&piece)) != *hash {
            write_reject(stream, "piece hash does not match the transfer manifest")?;
            return Ok(());
        }
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        if let Err(error) = store.store_piece(&id, index, &piece) {
            write_reject(stream, &error.to_string())?;
            return Ok(());
        }
        if let Some(source) = source {
            eprintln!(
                "object-services: received piece {} from {}",
                &hash[..7],
                &source.as_hex()[..7]
            );
        }
        assembled.extend_from_slice(&piece);
    }
    if u64::try_from(assembled.len()).unwrap_or(u64::MAX) != parsed.length {
        write_reject(stream, "transfer length does not match the manifest")?;
        return Ok(());
    }
    if hex_encode(&Sha256::digest(&assembled)) != id.as_str() {
        write_reject(stream, "stored bytes do not match the object id")?;
        return Ok(());
    }
    if let Err(error) = store.store_received_bytes(&id, &assembled) {
        write_reject(stream, &error.to_string())?;
        return Ok(());
    }
    stream.write_all(&[REQ_DONE])?;
    stream.flush()?;
    Ok(())
}

struct ParsedManifest {
    object_id: String,
    length: u64,
    pieces: Vec<String>,
}

fn parse_manifest(text: &str) -> Result<ParsedManifest, TransferError> {
    let mut lines = text.lines();
    let object_id = lines
        .next()
        .and_then(|line| line.strip_prefix("object-id "))
        .filter(|id| ObjectId::parse(id).is_ok())
        .ok_or_else(|| TransferError::Message("transfer manifest has no object id".to_string()))?
        .to_string();
    let length = lines
        .next()
        .and_then(|line| line.strip_prefix("length "))
        .and_then(|length| length.parse::<u64>().ok())
        .ok_or_else(|| TransferError::Message("transfer manifest has no length".to_string()))?;
    if length > MAX_OBJECT_LEN {
        return Err(TransferError::Message(
            "transfer manifest length is too large".to_string(),
        ));
    }
    let piece_size = lines
        .next()
        .and_then(|line| line.strip_prefix("piece-size "))
        .and_then(|size| size.parse::<usize>().ok())
        .ok_or_else(|| TransferError::Message("transfer manifest has no piece size".to_string()))?;
    if piece_size != PIECE_SIZE {
        return Err(TransferError::Message(format!(
            "transfer manifest piece size is {piece_size}, expected {PIECE_SIZE}"
        )));
    }
    let mut pieces = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some(rest) = line.strip_prefix("piece ") else {
            return Err(TransferError::Message(format!(
                "transfer manifest line {line} is not a piece"
            )));
        };
        let Some((index, hash)) = rest.split_once(' ') else {
            return Err(TransferError::Message(format!(
                "transfer manifest line {line} is not a piece"
            )));
        };
        if index.parse::<usize>().ok() != Some(pieces.len())
            || decode_hex(hash).is_none_or(|bytes| bytes.len() != 32)
        {
            return Err(TransferError::Message(format!(
                "transfer manifest piece {index} is not valid"
            )));
        }
        pieces.push(hash.to_string());
    }
    let expected = if length == 0 {
        0
    } else {
        usize::try_from(length.div_ceil(u64::try_from(PIECE_SIZE).unwrap_or(1)))
            .unwrap_or(usize::MAX)
    };
    if pieces.len() != expected {
        return Err(TransferError::Message(
            "transfer manifest piece count does not match its length".to_string(),
        ));
    }
    Ok(ParsedManifest {
        object_id,
        length,
        pieces,
    })
}

enum Request {
    Done,
    Manifest,
    Piece(u32),
}

fn read_request<S: Read>(stream: &mut S) -> Result<Request, TransferError> {
    let mut opcode = [0_u8; 1];
    stream.read_exact(&mut opcode)?;
    match opcode[0] {
        REQ_DONE => Ok(Request::Done),
        REQ_MANIFEST => Ok(Request::Manifest),
        REQ_PIECE => {
            let mut index = [0_u8; 4];
            stream.read_exact(&mut index)?;
            Ok(Request::Piece(u32::from_le_bytes(index)))
        }
        REQ_REJECT => {
            let message = read_blob(stream, MAX_ENVELOPE)?;
            Err(TransferError::Message(
                String::from_utf8_lossy(&message).into_owned(),
            ))
        }
        _ => Err(TransferError::Message(
            "unrecognized transfer request".to_string(),
        )),
    }
}

fn write_reject<S: Write>(stream: &mut S, message: &str) -> Result<(), TransferError> {
    stream.write_all(&[REQ_REJECT])?;
    write_blob(stream, message.as_bytes())?;
    Ok(())
}

fn write_blob<S: Write>(stream: &mut S, bytes: &[u8]) -> Result<(), TransferError> {
    let length = u32::try_from(bytes.len())
        .map_err(|_| TransferError::Message("transfer message is too long".to_string()))?;
    stream.write_all(&length.to_le_bytes())?;
    stream.write_all(bytes)?;
    stream.flush()?;
    Ok(())
}

fn read_text<S: Read>(stream: &mut S, max: usize) -> Result<String, TransferError> {
    let bytes = read_blob(stream, max)?;
    String::from_utf8(bytes)
        .map_err(|_| TransferError::Message("transfer message is not utf-8".to_string()))
}

fn read_blob<S: Read>(stream: &mut S, max: usize) -> Result<Vec<u8>, TransferError> {
    let mut length_bytes = [0_u8; 4];
    stream.read_exact(&mut length_bytes)?;
    let length = usize::try_from(u32::from_le_bytes(length_bytes)).unwrap_or(usize::MAX);
    if length > max {
        return Err(TransferError::Message(
            "transfer message is too long".to_string(),
        ));
    }
    let mut bytes = vec![0_u8; length];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(text.len() / 2);
    let mut index = 0;
    while index < bytes.len() {
        let high = hex_value(bytes[index])?;
        let low = hex_value(bytes[index + 1])?;
        decoded.push((high << 4) | low);
        index += 2;
    }
    Some(decoded)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use ed25519_dalek::{Signer, Verifier};

    use super::{
        announce_release, destination_hash, fastest_claim, fetch_catalog, fetch_preview,
        holder_meeting, identity_hash, narrow_bytes_per_second, object_transfer_address, offer,
        read_claim, read_who_has, receive_claim, receive_manifest_ask, receive_release,
        receive_who_has, release_matches, seek_piece, serve_peer, write_blob, write_claim,
        write_who_has, Destination, Neighborhood, ObjectTransfer, Pipe, TransferError, WhoHas,
        DESTINATION_LEN,
    };
    use crate::config::FetchPolicy;
    use crate::store::{
        hex_decode, Claims, ObjectId, ObjectStore, TRANSFER_ENVELOPE_FILE, TRANSFER_MANIFEST_FILE,
    };
    use sha2::{Digest, Sha256};

    #[test]
    fn a_transfer_creates_the_manifest_and_envelope_from_the_loa_claims() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open_with_load(root.path()).expect("store");
        let claims =
            Claims::parse(["board=heltec-v4-r8", "provenance=local-build"]).expect("claims");
        let id = store
            .import_reader(Cursor::new(b"abc"), 3, &claims)
            .expect("import");
        let first = store.ensure_transfer_documents(&id).expect("documents");
        let second = store.ensure_transfer_documents(&id).expect("again");
        assert_eq!(first.manifest, second.manifest);
        assert_eq!(first.envelope, second.envelope);
        assert!(first.manifest.starts_with(&format!(
            "object-id {}\nlength 3\npiece-size 4096\npiece 0 ",
            id.as_str()
        )));
        assert!(first
            .envelope
            .starts_with("board heltec-v4-r8\nprovenance local-build\nsignature "));
        let statement = format!(
            "board heltec-v4-r8\nprovenance local-build\n{}",
            first.manifest
        );
        let signature = first
            .envelope
            .lines()
            .find_map(|line| line.strip_prefix("signature "))
            .expect("signature");
        let signature_bytes = hex_decode(signature).expect("hex");
        let key =
            std::fs::read(root.path().join("loa").join("local").join("private")).expect("key");
        let signing =
            ed25519_dalek::SigningKey::from_bytes(key.as_slice().try_into().expect("seed"));
        let signature = ed25519_dalek::Signature::from_bytes(
            signature_bytes.as_slice().try_into().expect("sig"),
        );
        signing
            .verifying_key()
            .verify(statement.as_bytes(), &signature)
            .expect("signature verifies");
    }

    #[test]
    fn the_receiver_pulls_the_manifest_and_then_the_pieces() {
        let root = tempfile::tempdir().expect("temp dir");
        let source_root = root.path().join("source-store");
        let source = ObjectStore::open_with_load(&source_root).expect("source");
        let bytes = vec![7_u8; 4097];
        let id = source
            .import_reader(
                Cursor::new(bytes.clone()),
                bytes.len() as u64,
                &Claims::none(),
            )
            .expect("import");
        let destination_root = root.path().join("destination-store");
        let destination_store =
            ObjectStore::open_with_load(&destination_root).expect("destination");
        trust_authority(&destination_store, &source);
        let transfer_root = root.path().join("transfer");
        let identity = [0x5a_u8; 64];
        let address = object_transfer_address(&identity);
        let transfer = ObjectTransfer::open(&transfer_root, &identity).expect("transfer");
        let socket = root.path().join("transfer.sock");
        let listener = crate::ipc::LocalListener::bind(&socket).expect("bind");
        std::fs::write(
            transfer_root.join("peers"),
            format!("{}\n", socket.display()),
        )
        .expect("peers");
        let peer_store = ObjectStore::open_with_load(&destination_root).expect("peer store");
        let peer_transfer = transfer.clone();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                serve_peer(&peer_store, &peer_transfer, &mut stream, u64::MAX).expect("peer");
            }
        });
        offer(&source, &transfer, id.as_str(), &address.as_hex()).expect("offer");
        server.join().expect("server");
        let object_dir = destination_root.join("data").join(id.as_str());
        let stored = std::fs::read(object_dir.join("data")).expect("stored");
        assert_eq!(stored, bytes);
        assert!(!object_dir.join("pieces").exists());
        assert_eq!(
            destination_store
                .read_piece(&id, 0)
                .expect("piece from data"),
            &bytes[..4096]
        );
        assert_eq!(
            destination_store
                .read_piece(&id, 1)
                .expect("tail from data"),
            &bytes[4096..]
        );
        let source_dir = source_root.join("data").join(id.as_str());
        for name in [
            "manifest",
            "LOA-envelope",
            TRANSFER_MANIFEST_FILE,
            TRANSFER_ENVELOPE_FILE,
        ] {
            assert_eq!(
                std::fs::read_to_string(object_dir.join(name)).expect(name),
                std::fs::read_to_string(source_dir.join(name)).expect(name),
                "{name}"
            );
        }
        assert!(transfer_root.join("paths").join(address.as_hex()).is_file());
    }

    #[test]
    fn a_piece_that_does_not_match_the_manifest_is_not_stored() {
        let root = tempfile::tempdir().expect("temp dir");
        let source = ObjectStore::open_with_load(root.path().join("source-store")).expect("source");
        let id = source
            .import_reader(Cursor::new(b"abc"), 3, &Claims::none())
            .expect("import");
        std::fs::write(
            root.path()
                .join("source-store")
                .join("data")
                .join(id.as_str())
                .join(TRANSFER_MANIFEST_FILE),
            format!(
                "object-id {}\nlength 3\npiece-size 4096\npiece 0 {}\n",
                id.as_str(),
                "00".repeat(32)
            ),
        )
        .expect("bad manifest");
        let transfer_root = root.path().join("transfer");
        let identity = [0x5a_u8; 64];
        let address = object_transfer_address(&identity);
        let transfer = ObjectTransfer::open(&transfer_root, &identity).expect("transfer");
        let socket = root.path().join("transfer.sock");
        let listener = crate::ipc::LocalListener::bind(&socket).expect("bind");
        std::fs::write(
            transfer_root.join("peers"),
            format!("{}\n", socket.display()),
        )
        .expect("peers");
        let peer_store =
            ObjectStore::open_with_load(root.path().join("destination-store")).expect("peer");
        trust_authority(&peer_store, &source);
        let peer_transfer = transfer.clone();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                serve_peer(&peer_store, &peer_transfer, &mut stream, u64::MAX).expect("peer");
            }
        });
        let error = offer(&source, &transfer, id.as_str(), &address.as_hex()).expect_err("reject");
        server.join().expect("server");
        assert!(error.to_string().contains("piece hash"));
        let object_dir = root
            .path()
            .join("destination-store")
            .join("data")
            .join(id.as_str());
        assert!(object_dir.join("manifest").is_file());
        assert!(object_dir.join("LOA-envelope").is_file());
        assert!(object_dir.join(TRANSFER_MANIFEST_FILE).is_file());
        assert!(object_dir.join(TRANSFER_ENVELOPE_FILE).is_file());
        assert!(!object_dir.join("pieces").join("0").exists());
        assert!(!object_dir.join("data").exists());
    }

    #[test]
    fn a_release_with_a_bad_signature_is_not_remembered_or_forwarded() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open_with_load(root.path().join("store")).expect("store");
        let mut transfer =
            ObjectTransfer::open(root.path().join("transfer"), &[4; 64]).expect("transfer");
        let flooded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        struct Counting {
            flooded: Arc<std::sync::atomic::AtomicBool>,
        }
        impl Neighborhood for Counting {
            fn flood(&self) -> Vec<Box<dyn Pipe>> {
                self.flooded
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Vec::new()
            }
            fn open(&self, destination: &Destination) -> Result<Box<dyn Pipe>, TransferError> {
                Err(TransferError::Message(destination.as_hex()))
            }
        }
        transfer.set_neighborhood(Arc::new(Counting {
            flooded: Arc::clone(&flooded),
        }));
        let id = ObjectId::parse(&"ab".repeat(32)).expect("id");
        let foreign = ed25519_dalek::SigningKey::from_bytes(&[9_u8; 32]);
        let statement = format!("object-id {}\n", id.as_str());
        let forged = format!(
            "{statement}signature {}\n",
            crate::store::hex_encode(&foreign.sign(statement.as_bytes()).to_bytes())
        );
        let mut payload = Vec::new();
        payload.extend_from_slice(id.as_str().as_bytes());
        payload.extend_from_slice(&[9_u8; DESTINATION_LEN]);
        payload.push(2);
        write_blob(&mut payload, forged.as_bytes()).expect("blob");
        let error =
            receive_release(&store, &transfer, &mut Cursor::new(payload)).expect_err("reject");
        assert!(error.to_string().contains("does not match a trusted LOA"));
        assert!(!flooded.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!transfer.root.join("releases").join(id.as_str()).exists());

        let seed = std::fs::read(store.loa_key_path()).expect("seed");
        let signing =
            ed25519_dalek::SigningKey::from_bytes(seed.as_slice().try_into().expect("seed"));
        let statement = format!("object-id {}\n", id.as_str());
        let envelope = format!(
            "{statement}signature {}\n",
            crate::store::hex_encode(&signing.sign(statement.as_bytes()).to_bytes())
        );
        let mut payload = Vec::new();
        payload.extend_from_slice(id.as_str().as_bytes());
        payload.extend_from_slice(&[9_u8; DESTINATION_LEN]);
        payload.push(2);
        write_blob(&mut payload, envelope.as_bytes()).expect("blob");
        receive_release(&store, &transfer, &mut Cursor::new(payload)).expect("accept");
        assert!(flooded.load(std::sync::atomic::Ordering::Relaxed));
        assert!(transfer.root.join("releases").join(id.as_str()).is_file());
    }

    #[test]
    fn a_release_propagates_from_a_to_b_to_c_without_a_fetch() {
        let root = tempfile::tempdir().expect("temp dir");
        let bytes = b"release-bytes";
        let mut listeners = Vec::new();
        let mut stacks = Vec::new();
        for (name, identity) in [("a", 0x11_u8), ("b", 0x22_u8), ("c", 0x33_u8)] {
            let store_root = root.path().join(name);
            let store = Arc::new(ObjectStore::open_with_load(&store_root).expect(name));
            let transfer_root = root.path().join(format!("{name}-transfer"));
            let secret = [identity; 64];
            let address = object_transfer_address(&secret);
            let transfer = ObjectTransfer::open(&transfer_root, &secret).expect(name);
            let socket = root.path().join(format!("{name}.sock"));
            let listener = crate::ipc::LocalListener::bind(&socket).expect(name);
            let accept_store = Arc::clone(&store);
            let accept_transfer = transfer.clone();
            listeners.push(thread::spawn(move || {
                for connection in listener.incoming() {
                    let store = Arc::clone(&accept_store);
                    let transfer = accept_transfer.clone();
                    thread::spawn(move || {
                        if let Ok(mut stream) = connection {
                            if let Err(error) = serve_peer(&store, &transfer, &mut stream, u64::MAX)
                            {
                                eprintln!("serve_peer: {error:?}");
                            }
                        }
                    });
                }
            }));
            stacks.push((store, transfer, socket, address));
        }
        let (a_store, a_transfer, a_socket, a_address) = &stacks[0];
        let (_b_store, b_transfer, b_socket, _) = &stacks[1];
        let (_c_store, c_transfer, c_socket, _) = &stacks[2];
        std::fs::write(
            a_transfer.root.join("peers"),
            format!("{}\n", b_socket.display()),
        )
        .expect("a peers");
        std::fs::write(
            b_transfer.root.join("peers"),
            format!("{}\n{}\n", a_socket.display(), c_socket.display()),
        )
        .expect("b peers");
        std::fs::write(
            c_transfer.root.join("peers"),
            format!("{}\n", b_socket.display()),
        )
        .expect("c peers");
        trust_authority(stacks[1].0.as_ref(), stacks[0].0.as_ref());
        trust_authority(stacks[2].0.as_ref(), stacks[0].0.as_ref());
        let id = a_store
            .import_reader(Cursor::new(&bytes[..]), bytes.len() as u64, &Claims::none())
            .expect("import");
        announce_release(a_store, a_transfer, id.as_str(), 8).expect("release");
        let c_object = root.path().join("c").join("data").join(id.as_str());
        let c_release = c_transfer.root.join("releases").join(id.as_str());
        let ready = (0..50).any(|_| {
            if c_release.is_file() {
                true
            } else {
                thread::sleep(Duration::from_millis(50));
                false
            }
        });
        assert!(ready, "c did not receive the release");
        thread::sleep(Duration::from_millis(400));
        assert!(!c_object.join("data").exists());
        assert!(!root
            .path()
            .join("b")
            .join("data")
            .join(id.as_str())
            .join("data")
            .exists());
        let envelope = std::fs::read_to_string(
            root.path()
                .join("a")
                .join("data")
                .join(id.as_str())
                .join("LOA-envelope"),
        )
        .expect("a envelope");
        let b_release = std::fs::read_to_string(b_transfer.root.join("releases").join(id.as_str()))
            .expect("b release");
        let c_notice = std::fs::read_to_string(&c_release).expect("c release");
        assert_eq!(
            b_release,
            format!("origin {}\nhops-left 8\n{envelope}", a_address.as_hex())
        );
        assert_eq!(
            c_notice,
            format!("origin {}\nhops-left 7\n{envelope}", a_address.as_hex())
        );
        drop(listeners);
    }

    #[test]
    fn a_catalog_lists_signed_objects_and_a_preview_is_the_first_piece() {
        let root = tempfile::tempdir().expect("temp dir");
        let (holder_store, _holder_transfer, holder_socket, holder_address, holder_listen) =
            listen_stack(
                root.path(),
                "holder",
                0x41,
                FetchPolicy::default(),
                u64::MAX,
            );
        let (seeker_store, seeker_transfer, _, _, seeker_listen) = listen_stack(
            root.path(),
            "seeker",
            0x42,
            FetchPolicy::default(),
            u64::MAX,
        );
        std::fs::write(
            seeker_transfer.root.join("peers"),
            format!("{}\n", holder_socket.display()),
        )
        .expect("peers");
        let mut bytes = vec![0x11_u8; crate::store::PIECE_SIZE + 10];
        bytes[0] = b'h';
        let claims = Claims::parse(["object-type=firmware", "board=heltec-v4-r8"]).expect("claims");
        let id = holder_store
            .import_reader(Cursor::new(bytes.clone()), bytes.len() as u64, &claims)
            .expect("import");
        let plain = b"plain-object";
        holder_store
            .import_plain(Cursor::new(&plain[..]), plain.len() as u64)
            .expect("plain");

        let catalog = fetch_catalog(&seeker_transfer, &holder_address.as_hex()).expect("catalog");
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, id);
        assert_eq!(catalog[0].length, bytes.len() as u64);
        assert!(catalog[0].envelope.contains("object-type firmware"));
        assert!(catalog[0].envelope.contains(&id.as_str().to_string()));

        let (length, piece) =
            fetch_preview(&seeker_transfer, &holder_address.as_hex(), id.as_str())
                .expect("preview");
        assert_eq!(length, bytes.len() as u64);
        assert_eq!(piece, bytes[..crate::store::PIECE_SIZE]);

        let local_socket = root.path().join("local.sock");
        let listener = crate::ipc::LocalListener::bind(&local_socket).expect("local");
        let relay_store = Arc::clone(&seeker_store);
        let relay_transfer = seeker_transfer.clone();
        let relay = thread::spawn(move || {
            for connection in listener.incoming().take(2) {
                let Ok(mut stream) = connection else {
                    continue;
                };
                let _ = crate::protocol::serve_connection(
                    &relay_store,
                    Some(&relay_transfer),
                    &mut stream,
                );
            }
        });
        let listed = crate::protocol::request_catalog(&local_socket, &holder_address.as_hex())
            .expect("local catalog");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        let (relayed_length, relayed_piece) =
            crate::protocol::request_preview(&local_socket, &holder_address.as_hex(), id.as_str())
                .expect("local preview");
        assert_eq!(relayed_length, bytes.len() as u64);
        assert_eq!(relayed_piece, bytes[..crate::store::PIECE_SIZE]);
        relay.join().expect("relay");
        drop((holder_listen, seeker_listen));
    }

    fn signed_claim(secret: &[u8; 64], mut claim: super::Claim) -> super::Claim {
        let identity = super::StackIdentity::from_secret(secret);
        claim.encryption_public = identity.encryption_public;
        claim.signing_public = identity.signing_public;
        claim.holder = identity.destination();
        claim.signature = identity.sign(&super::claim_statement(&claim));
        claim
    }

    fn signed_who_has(secret: &[u8; 64], mut request: WhoHas) -> WhoHas {
        let identity = super::StackIdentity::from_secret(secret);
        request.encryption_public = identity.encryption_public;
        request.signing_public = identity.signing_public;
        request.signature = identity.sign(&super::who_has_statement(&request));
        request
    }

    fn trust_authority(member: &ObjectStore, authority: &ObjectStore) {
        let public = std::fs::read(authority.loa_key_path().with_file_name("public"))
            .expect("loa public key");
        let key: [u8; 32] = public.as_slice().try_into().expect("32-byte public key");
        member.trust_loa(&key).expect("trust loa");
    }

    fn listen_stack(
        root: &std::path::Path,
        name: &str,
        identity: u8,
        policy: crate::config::FetchPolicy,
        interface_bytes_per_second: u64,
    ) -> (
        Arc<ObjectStore>,
        ObjectTransfer,
        std::path::PathBuf,
        super::Destination,
        std::thread::JoinHandle<()>,
    ) {
        let store_root = root.join(name);
        let store = Arc::new(ObjectStore::open_with_load(&store_root).expect(name));
        let transfer_root = root.join(format!("{name}-transfer"));
        let secret = [identity; 64];
        let address = object_transfer_address(&secret);
        let mut transfer = ObjectTransfer::open(&transfer_root, &secret).expect(name);
        transfer.set_fetch_policy(policy);
        let socket = root.join(format!("{name}.sock"));
        let listener = crate::ipc::LocalListener::bind(&socket).expect(name);
        let accept_store = Arc::clone(&store);
        let accept_transfer = transfer.clone();
        let handle = thread::spawn(move || {
            for connection in listener.incoming() {
                let store = Arc::clone(&accept_store);
                let transfer = accept_transfer.clone();
                thread::spawn(move || {
                    if let Ok(mut stream) = connection {
                        let _ =
                            serve_peer(&store, &transfer, &mut stream, interface_bytes_per_second);
                    }
                });
            }
        });
        (store, transfer, socket, address, handle)
    }

    #[test]
    fn a_who_has_at_zero_hops_is_answered_by_the_neighbor_and_stops() {
        let root = tempfile::tempdir().expect("temp dir");
        let (holder_store, holder_transfer, holder_socket, holder_address, holder_listen) =
            listen_stack(
                root.path(),
                "holder",
                0x22,
                FetchPolicy::default(),
                u64::MAX,
            );
        let (_far_store, far_transfer, far_socket, _, far_listen) =
            listen_stack(root.path(), "far", 0x11, FetchPolicy::default(), u64::MAX);
        let (seeker_store, seeker_transfer, seeker_socket, seeker_address, seeker_listen) =
            listen_stack(
                root.path(),
                "seeker",
                0x33,
                FetchPolicy::default(),
                u64::MAX,
            );
        std::fs::write(
            holder_transfer.root.join("peers"),
            format!("{}\n{}\n", seeker_socket.display(), far_socket.display()),
        )
        .expect("holder peers");
        std::fs::write(
            far_transfer.root.join("peers"),
            format!("{}\n", holder_socket.display()),
        )
        .expect("far peers");
        std::fs::write(
            seeker_transfer.root.join("peers"),
            format!("{}\n", holder_socket.display()),
        )
        .expect("seeker peers");
        let bytes = b"who-has-neighbor";
        let id = holder_store
            .import_reader(Cursor::new(&bytes[..]), bytes.len() as u64, &Claims::none())
            .expect("import");
        let documents = holder_store
            .ensure_transfer_documents(&id)
            .expect("manifest");
        trust_authority(&seeker_store, &holder_store);
        seeker_store
            .store_transfer_documents(&id, &documents.manifest, &documents.envelope)
            .expect("share manifest");
        let found = seek_piece(&seeker_store, &seeker_transfer, id.as_str(), 0).expect("who-has");
        assert_eq!(found.as_hex(), holder_address.as_hex());
        assert!(!far_transfer
            .root
            .join("who-has")
            .join(id.as_str())
            .join("0")
            .join(seeker_address.as_hex())
            .join("seen-0")
            .exists());
        drop((holder_listen, far_listen, seeker_listen));
    }

    #[test]
    fn a_who_has_widens_until_the_edge_that_has_the_piece() {
        let root = tempfile::tempdir().expect("temp dir");
        let (holder_store, holder_transfer, holder_socket, holder_address, holder_listen) =
            listen_stack(
                root.path(),
                "holder",
                0x11,
                FetchPolicy::default(),
                u64::MAX,
            );
        let (_middle_store, middle_transfer, middle_socket, _, middle_listen) = listen_stack(
            root.path(),
            "middle",
            0x22,
            FetchPolicy::default(),
            u64::MAX,
        );
        let (seeker_store, seeker_transfer, seeker_socket, seeker_address, seeker_listen) =
            listen_stack(
                root.path(),
                "seeker",
                0x33,
                FetchPolicy::default(),
                u64::MAX,
            );
        std::fs::write(
            holder_transfer.root.join("peers"),
            format!("{}\n", middle_socket.display()),
        )
        .expect("holder peers");
        std::fs::write(
            middle_transfer.root.join("peers"),
            format!("{}\n{}\n", holder_socket.display(), seeker_socket.display()),
        )
        .expect("middle peers");
        std::fs::write(
            seeker_transfer.root.join("peers"),
            format!("{}\n", middle_socket.display()),
        )
        .expect("seeker peers");
        let bytes = b"who-has-far";
        let id = holder_store
            .import_reader(Cursor::new(&bytes[..]), bytes.len() as u64, &Claims::none())
            .expect("import");
        let documents = holder_store
            .ensure_transfer_documents(&id)
            .expect("manifest");
        trust_authority(&seeker_store, &holder_store);
        seeker_store
            .store_transfer_documents(&id, &documents.manifest, &documents.envelope)
            .expect("share manifest");
        let found = seek_piece(&seeker_store, &seeker_transfer, id.as_str(), 0).expect("who-has");
        assert_eq!(found.as_hex(), holder_address.as_hex());
        let search = middle_transfer
            .root
            .join("who-has")
            .join(id.as_str())
            .join("0")
            .join(seeker_address.as_hex());
        assert!(search.join("seen-0").is_file());
        assert!(search.join("seen-1").is_file());
        let claim = std::fs::read_to_string(search.join("claims").join(holder_address.as_hex()))
            .expect("middle heard the claim");
        assert_eq!(claim.trim(), u64::MAX.to_string());
        drop((holder_listen, middle_listen, seeker_listen));
    }

    #[test]
    fn a_node_inside_the_ring_relays_a_piece_it_has_and_does_not_claim() {
        let root = tempfile::tempdir().expect("temp dir");
        let (inside_store, inside_transfer, inside_socket, _, inside_listen) = listen_stack(
            root.path(),
            "inside",
            0x22,
            FetchPolicy::default(),
            u64::MAX,
        );
        let (_edge_store, edge_transfer, edge_socket, _, edge_listen) =
            listen_stack(root.path(), "edge", 0x11, FetchPolicy::default(), u64::MAX);
        std::fs::write(
            inside_transfer.root.join("peers"),
            format!("{}\n", edge_socket.display()),
        )
        .expect("inside peers");
        std::fs::write(
            edge_transfer.root.join("peers"),
            format!("{}\n", inside_socket.display()),
        )
        .expect("edge peers");
        let bytes = b"inside-has-it";
        let id = inside_store
            .import_reader(Cursor::new(&bytes[..]), bytes.len() as u64, &Claims::none())
            .expect("import");
        let documents = inside_store
            .ensure_transfer_documents(&id)
            .expect("manifest");
        let hash_line = documents
            .manifest
            .lines()
            .find_map(|line| line.strip_prefix("piece 0 "))
            .expect("piece line");
        let hash = hex_decode(hash_line).expect("hash");
        let mut hash_bytes = [0_u8; 32];
        hash_bytes.copy_from_slice(&hash);
        let secret = [0x33_u8; 64];
        let requester = object_transfer_address(&secret);
        let socket = crate::ipc::LocalStream::connect(&inside_socket).expect("connect");
        write_who_has(
            &mut &socket,
            &signed_who_has(
                &secret,
                WhoHas {
                    id: id.clone(),
                    index: 0,
                    hash: hash_bytes,
                    piece_size: u32::try_from(super::PIECE_SIZE).unwrap_or(u32::MAX),
                    piece_length: u32::try_from(bytes.len()).unwrap_or(u32::MAX),
                    requester,
                    hops: 1,
                    bytes_per_second: u64::MAX,
                    encryption_public: [0; 32],
                    signing_public: [0; 32],
                    signature: [0; 64],
                },
            ),
        )
        .expect("who-has");
        drop(socket);
        let edge_seen = edge_transfer
            .root
            .join("who-has")
            .join(id.as_str())
            .join("0")
            .join(requester.as_hex())
            .join("seen-0");
        let forwarded = (0..40).any(|_| {
            if edge_seen.is_file() {
                true
            } else {
                thread::sleep(Duration::from_millis(10));
                false
            }
        });
        assert!(forwarded, "the inside node did not relay the who-has");
        let claim = inside_transfer
            .root
            .join("who-has")
            .join(id.as_str())
            .join("0")
            .join(requester.as_hex())
            .join("claim");
        assert!(!claim.exists());
        drop((inside_listen, edge_listen));
    }

    #[test]
    fn a_slow_claim_is_held_while_a_faster_path_is_sought() {
        let root = tempfile::tempdir().expect("temp dir");
        let (lora_store, lora_transfer, lora_socket, lora_address, lora_listen) =
            listen_stack(root.path(), "lora", 0x11, FetchPolicy::default(), 50);
        let (_middle_store, middle_transfer, middle_socket, _, middle_listen) = listen_stack(
            root.path(),
            "middle",
            0x22,
            FetchPolicy::default(),
            1_000_000,
        );
        let (wifi_store, wifi_transfer, wifi_socket, wifi_address, wifi_listen) =
            listen_stack(root.path(), "wifi", 0x44, FetchPolicy::default(), 1_000_000);
        let (seeker_store, seeker_transfer, seeker_socket, seeker_address, seeker_listen) =
            listen_stack(
                root.path(),
                "seeker",
                0x33,
                FetchPolicy {
                    minimum_bytes_per_second: 1_000,
                    ..FetchPolicy::default()
                },
                u64::MAX,
            );
        std::fs::write(
            seeker_transfer.root.join("peers"),
            format!("{}\n{}\n", lora_socket.display(), middle_socket.display()),
        )
        .expect("seeker peers");
        std::fs::write(
            lora_transfer.root.join("peers"),
            format!("{}\n", seeker_socket.display()),
        )
        .expect("lora peers");
        std::fs::write(
            middle_transfer.root.join("peers"),
            format!("{}\n{}\n", seeker_socket.display(), wifi_socket.display()),
        )
        .expect("middle peers");
        std::fs::write(
            wifi_transfer.root.join("peers"),
            format!("{}\n", middle_socket.display()),
        )
        .expect("wifi peers");
        let bytes = b"who-has-fast-path";
        let id = wifi_store
            .import_reader(Cursor::new(&bytes[..]), bytes.len() as u64, &Claims::none())
            .expect("wifi import");
        lora_store
            .import_reader(Cursor::new(&bytes[..]), bytes.len() as u64, &Claims::none())
            .expect("lora import");
        let documents = wifi_store.ensure_transfer_documents(&id).expect("manifest");
        trust_authority(&seeker_store, &wifi_store);
        seeker_store
            .store_transfer_documents(&id, &documents.manifest, &documents.envelope)
            .expect("share manifest");
        let found = seek_piece(&seeker_store, &seeker_transfer, id.as_str(), 0).expect("who-has");
        assert_eq!(found.as_hex(), wifi_address.as_hex());
        let claims = seeker_transfer
            .root
            .join("who-has")
            .join(id.as_str())
            .join("0")
            .join(seeker_address.as_hex())
            .join("claims");
        assert_eq!(
            std::fs::read_to_string(claims.join(lora_address.as_hex()))
                .expect("slow claim")
                .trim(),
            "50"
        );
        assert_eq!(
            std::fs::read_to_string(claims.join(wifi_address.as_hex()))
                .expect("fast claim")
                .trim(),
            "1000000"
        );
        drop((lora_listen, middle_listen, wifi_listen, seeker_listen));
    }

    #[test]
    fn the_slowest_link_is_what_a_later_hop_hears() {
        assert_eq!(narrow_bytes_per_second(u64::MAX, 50), 50);
        assert_eq!(narrow_bytes_per_second(1_000_000, 50), 50);
        assert_eq!(narrow_bytes_per_second(50, 1_000_000), 50);
        let slow = object_transfer_address(&[0x11_u8; 64]);
        let medium = object_transfer_address(&[0x22_u8; 64]);
        let fast = object_transfer_address(&[0x44_u8; 64]);
        let below = [(slow, 50_u64), (medium, 400)];
        assert!(holder_meeting(&below, 1_000).is_none());
        assert_eq!(
            fastest_claim(&below).expect("fallback").0.as_hex(),
            medium.as_hex()
        );
        let both = [(slow, 50_u64), (fast, 1_000_000)];
        assert_eq!(
            holder_meeting(&both, 1_000).expect("wifi").as_hex(),
            fast.as_hex()
        );
    }

    #[test]
    fn who_has_and_its_claim_carry_the_path_rate() {
        let id = ObjectId::parse(&"ab".repeat(32)).expect("id");
        let secret = [0x33_u8; 64];
        let requester = object_transfer_address(&secret);
        let holder_secret = [0x11_u8; 64];
        let request = signed_who_has(
            &secret,
            WhoHas {
                id: id.clone(),
                index: 0,
                hash: [7_u8; 32],
                piece_size: 4096,
                piece_length: 3,
                requester,
                hops: 1,
                bytes_per_second: 50,
                encryption_public: [0; 32],
                signing_public: [0; 32],
                signature: [0; 64],
            },
        );
        let mut bytes = Vec::new();
        write_who_has(&mut bytes, &request).expect("write who-has");
        let read = read_who_has(&mut Cursor::new(bytes[6..].to_vec())).expect("read who-has");
        assert_eq!(read.bytes_per_second, 50);
        assert_eq!(read.hops, 1);
        let claim = signed_claim(
            &holder_secret,
            super::Claim {
                id,
                index: 0,
                hash: [7_u8; 32],
                requester,
                holder: object_transfer_address(&holder_secret),
                bytes_per_second: 50,
                encryption_public: [0; 32],
                signing_public: [0; 32],
                signature: [0; 64],
            },
        );
        let mut claim_bytes = Vec::new();
        write_claim(&mut claim_bytes, &claim).expect("write claim");
        let read = read_claim(&mut Cursor::new(&claim_bytes[6..])).expect("read claim");
        assert_eq!(read.bytes_per_second, 50);
        assert_eq!(read.holder.as_hex(), claim.holder.as_hex());
    }

    #[test]
    fn a_claim_that_names_another_holder_is_dropped() {
        let root = tempfile::tempdir().expect("temp dir");
        let transfer =
            ObjectTransfer::open(root.path().join("transfer"), &[4; 64]).expect("transfer");
        let id = ObjectId::parse(&"ab".repeat(32)).expect("id");
        let holder_secret = [0x11_u8; 64];
        let mut claim = signed_claim(
            &holder_secret,
            super::Claim {
                id: id.clone(),
                index: 0,
                hash: [7_u8; 32],
                requester: object_transfer_address(&[0x33_u8; 64]),
                holder: object_transfer_address(&holder_secret),
                bytes_per_second: 50,
                encryption_public: [0; 32],
                signing_public: [0; 32],
                signature: [0; 64],
            },
        );
        claim.holder = object_transfer_address(&[0x44_u8; 64]);
        let mut bytes = Vec::new();
        write_claim(&mut bytes, &claim).expect("write claim");
        let error =
            receive_claim(&transfer, &mut Cursor::new(bytes[6..].to_vec())).expect_err("reject");
        assert!(error.to_string().contains("does not match the identity"));
        assert!(!transfer.root.join("who-has").exists());

        claim.holder = object_transfer_address(&holder_secret);
        claim.bytes_per_second = 1_000_000;
        let mut bytes = Vec::new();
        write_claim(&mut bytes, &claim).expect("write claim");
        let error = receive_claim(&transfer, &mut Cursor::new(bytes[6..].to_vec()))
            .expect_err("altered rate");
        assert!(error
            .to_string()
            .contains("signature does not match the holder"));
        assert!(!transfer.root.join("who-has").exists());

        let claim = signed_claim(
            &holder_secret,
            super::Claim {
                id: id.clone(),
                index: 0,
                hash: [7_u8; 32],
                requester: object_transfer_address(&[0x33_u8; 64]),
                holder: object_transfer_address(&holder_secret),
                bytes_per_second: 50,
                encryption_public: [0; 32],
                signing_public: [0; 32],
                signature: [0; 64],
            },
        );
        let mut bytes = Vec::new();
        write_claim(&mut bytes, &claim).expect("write claim");
        receive_claim(&transfer, &mut Cursor::new(bytes[6..].to_vec())).expect("accept");
        let recorded = transfer
            .root
            .join("who-has")
            .join(id.as_str())
            .join("0")
            .join(claim.requester.as_hex())
            .join("claims")
            .join(claim.holder.as_hex());
        assert_eq!(
            std::fs::read_to_string(recorded).expect("claim").trim(),
            "50"
        );
    }

    #[test]
    fn a_request_that_names_another_address_is_dropped() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open(root.path().join("store")).expect("store");
        let mut transfer =
            ObjectTransfer::open(root.path().join("transfer"), &[4; 64]).expect("transfer");
        let flooded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        struct Counting {
            flooded: Arc<std::sync::atomic::AtomicBool>,
        }
        impl Neighborhood for Counting {
            fn flood(&self) -> Vec<Box<dyn Pipe>> {
                self.flooded
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Vec::new()
            }
            fn open(&self, destination: &Destination) -> Result<Box<dyn Pipe>, TransferError> {
                Err(TransferError::Message(destination.as_hex()))
            }
        }
        transfer.set_neighborhood(Arc::new(Counting {
            flooded: Arc::clone(&flooded),
        }));
        let id = ObjectId::parse(&"ab".repeat(32)).expect("id");
        let asker = [0x33_u8; 64];
        let mut request = signed_who_has(
            &asker,
            WhoHas {
                id: id.clone(),
                index: 0,
                hash: [7_u8; 32],
                piece_size: 4096,
                piece_length: 3,
                requester: object_transfer_address(&asker),
                hops: 1,
                bytes_per_second: u64::MAX,
                encryption_public: [0; 32],
                signing_public: [0; 32],
                signature: [0; 64],
            },
        );
        request.requester = object_transfer_address(&[0x44_u8; 64]);
        let mut bytes = Vec::new();
        write_who_has(&mut bytes, &request).expect("write who-has");
        let error = receive_who_has(
            &store,
            &transfer,
            &mut Cursor::new(bytes[6..].to_vec()),
            u64::MAX,
        )
        .expect_err("reject");
        assert!(error.to_string().contains("does not match the identity"));
        assert!(!flooded.load(std::sync::atomic::Ordering::Relaxed));
        assert!(!transfer.root.join("who-has").exists());

        request.requester = object_transfer_address(&asker);
        request.signature[0] ^= 0xff;
        let mut bytes = Vec::new();
        write_who_has(&mut bytes, &request).expect("write who-has");
        let error = receive_who_has(
            &store,
            &transfer,
            &mut Cursor::new(bytes[6..].to_vec()),
            u64::MAX,
        )
        .expect_err("bad signature");
        assert!(error
            .to_string()
            .contains("signature does not match the requester"));
        assert!(!transfer.root.join("who-has").exists());

        let request = signed_who_has(
            &asker,
            WhoHas {
                id: id.clone(),
                index: 0,
                hash: [7_u8; 32],
                piece_size: 4096,
                piece_length: 3,
                requester: object_transfer_address(&asker),
                hops: 1,
                bytes_per_second: u64::MAX,
                encryption_public: [0; 32],
                signing_public: [0; 32],
                signature: [0; 64],
            },
        );
        let mut bytes = Vec::new();
        write_who_has(&mut bytes, &request).expect("write who-has");
        receive_who_has(
            &store,
            &transfer,
            &mut Cursor::new(bytes[6..].to_vec()),
            u64::MAX,
        )
        .expect("accept");
        assert!(flooded.load(std::sync::atomic::Ordering::Relaxed));
        assert!(transfer
            .root
            .join("who-has")
            .join(id.as_str())
            .join("0")
            .join(request.requester.as_hex())
            .join("seen-1")
            .is_file());

        let identity = super::StackIdentity::from_secret(&asker);
        let mut ask = super::ManifestAsk {
            id: id.clone(),
            requester: object_transfer_address(&[0x44_u8; 64]),
            hops: 0,
            encryption_public: identity.encryption_public,
            signing_public: identity.signing_public,
            signature: [0; 64],
        };
        ask.signature = identity.sign(&super::manifest_ask_statement(&ask));
        let mut bytes = Vec::new();
        super::write_manifest_ask(&mut bytes, &ask).expect("write ask");
        let error = receive_manifest_ask(&store, &transfer, &mut Cursor::new(bytes[6..].to_vec()))
            .expect_err("reject manifest ask");
        assert!(error.to_string().contains("does not match the identity"));
        assert!(!transfer.root.join("manifest-asks").exists());
    }

    #[test]
    fn a_release_matches_a_cdn_group_or_a_new_firmware_version() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open_with_load(root.path()).expect("store");
        let cdn = FetchPolicy {
            cdn: true,
            cdn_groups: vec!["site-a".to_string()],
            ..FetchPolicy::default()
        };
        let all = Claims::parse(["CDN-group=all"]).expect("claims");
        assert!(release_matches(&cdn, &all, &store));
        let other = Claims::parse(["CDN-group=site-b"]).expect("claims");
        assert!(!release_matches(&cdn, &other, &store));
        let site = Claims::parse(["CDN-group=site-a"]).expect("claims");
        assert!(release_matches(&cdn, &site, &store));
        assert!(!release_matches(&FetchPolicy::default(), &all, &store));

        let firmware = Claims::parse([
            "mode=auto-stage",
            "flash-mode=ota",
            "object-type=firmware",
            "board=heltec-v4-r8",
            "version=1.2.3",
        ])
        .expect("claims");
        let stage = FetchPolicy {
            auto_stage: true,
            board: "heltec-v4-r8".to_string(),
            ..FetchPolicy::default()
        };
        assert!(release_matches(&stage, &firmware, &store));
        let update_only = FetchPolicy {
            auto_update: true,
            board: "heltec-v4-r8".to_string(),
            ..FetchPolicy::default()
        };
        assert!(!release_matches(&update_only, &firmware, &store));
        let wrong_board = FetchPolicy {
            auto_stage: true,
            board: "other".to_string(),
            ..FetchPolicy::default()
        };
        assert!(!release_matches(&wrong_board, &firmware, &store));
        store
            .import_reader(Cursor::new(b"held"), 4, &firmware)
            .expect("held version");
        assert!(!release_matches(&stage, &firmware, &store));
    }

    #[test]
    fn a_cdn_node_fetches_a_release_whose_group_is_all() {
        let root = tempfile::tempdir().expect("temp dir");
        let (holder_store, holder_transfer, holder_socket, _, holder_listen) = listen_stack(
            root.path(),
            "holder",
            0x11,
            FetchPolicy::default(),
            u64::MAX,
        );
        let (_middle_store, middle_transfer, middle_socket, _, middle_listen) = listen_stack(
            root.path(),
            "middle",
            0x22,
            FetchPolicy::default(),
            u64::MAX,
        );
        let cdn = FetchPolicy {
            cdn: true,
            ..FetchPolicy::default()
        };
        let (_seeker_store, seeker_transfer, seeker_socket, _, seeker_listen) =
            listen_stack(root.path(), "seeker", 0x33, cdn, u64::MAX);
        std::fs::write(
            holder_transfer.root.join("peers"),
            format!("{}\n", middle_socket.display()),
        )
        .expect("holder peers");
        std::fs::write(
            middle_transfer.root.join("peers"),
            format!("{}\n{}\n", holder_socket.display(), seeker_socket.display()),
        )
        .expect("middle peers");
        std::fs::write(
            seeker_transfer.root.join("peers"),
            format!("{}\n", middle_socket.display()),
        )
        .expect("seeker peers");
        let bytes = b"cdn-object";
        let claims = Claims::parse(["CDN-group=all"]).expect("claims");
        let id = holder_store
            .import_reader(Cursor::new(&bytes[..]), bytes.len() as u64, &claims)
            .expect("import");
        trust_authority(&_middle_store, &holder_store);
        trust_authority(&_seeker_store, &holder_store);
        announce_release(&holder_store, &holder_transfer, id.as_str(), 8).expect("release");
        let data = root
            .path()
            .join("seeker")
            .join("data")
            .join(id.as_str())
            .join("data");
        let ready = (0..80).any(|_| {
            if data.is_file() {
                true
            } else {
                thread::sleep(Duration::from_millis(50));
                false
            }
        });
        assert!(ready, "cdn node did not fetch the object");
        assert_eq!(std::fs::read(&data).expect("data"), bytes);
        drop((holder_listen, middle_listen, seeker_listen));
    }

    #[test]
    fn a_release_floods_through_the_neighborhood_without_a_peers_file() {
        struct Capture {
            bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
            pending: Vec<u8>,
        }
        impl std::io::Write for Capture {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.pending.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                let mut bytes = self.bytes.lock().expect("capture");
                bytes.extend_from_slice(&self.pending);
                self.pending.clear();
                Ok(())
            }
        }
        impl std::io::Read for Capture {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                std::io::Write::flush(self)?;
                Ok(0)
            }
        }
        struct Recording {
            bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        }
        impl Neighborhood for Recording {
            fn flood(&self) -> Vec<Box<dyn Pipe>> {
                vec![Box::new(Capture {
                    bytes: std::sync::Arc::clone(&self.bytes),
                    pending: Vec::new(),
                })]
            }
            fn open(&self, destination: &Destination) -> Result<Box<dyn Pipe>, TransferError> {
                Err(TransferError::Message(format!(
                    "no path to {}",
                    destination.as_hex()
                )))
            }
        }

        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open_with_load(root.path()).expect("store");
        let mut transfer =
            ObjectTransfer::open(root.path().join("transfer"), &[7; 64]).expect("transfer");
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        transfer.set_neighborhood(std::sync::Arc::new(Recording {
            bytes: std::sync::Arc::clone(&captured),
        }));
        let id = store
            .import_reader(Cursor::new(b"flood"), 5, &Claims::none())
            .expect("import");
        announce_release(&store, &transfer, id.as_str(), 1).expect("release");
        let bytes = captured.lock().expect("capture").clone();
        assert!(bytes.windows(4).any(|window| window == b"OBXF"));
        assert!(bytes
            .windows(id.as_str().len())
            .any(|window| window == id.as_str().as_bytes()));
        assert!(!root.path().join("transfer").join("peers").exists());
    }

    #[test]
    fn the_address_uses_the_remote_control_destination_derivation() {
        let name_hash = Sha256::digest(b"reticulum.remote.control");
        assert_eq!(
            &name_hash[..10],
            &[0xfc, 0xce, 0x4e, 0xf8, 0x4b, 0x57, 0xc8, 0xe9, 0xb2, 0xe3]
        );
        let identity = identity_hash(&[0x41; 32], &[0x42; 32]);
        assert_eq!(
            destination_hash("reticulum.remote.control", &identity),
            [
                0xb3, 0x6f, 0x62, 0x54, 0x71, 0x3a, 0xf4, 0x56, 0x2e, 0x66, 0x42, 0xb3, 0x90, 0x20,
                0xf5, 0xb0
            ]
        );
    }
}
