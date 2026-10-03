use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

const LOA_DIR: &str = "loa";
const LOA_LOCAL_DIR: &str = "local";
const LOA_TRUSTED_DIR: &str = "trusted";
const LOA_PRIVATE_FILE: &str = "private";
const LOA_PUBLIC_FILE: &str = "public";
const SIGNING_SEED_LEN: usize = 32;
pub(crate) const PIECE_SIZE: usize = 4096;
pub(crate) const TRANSFER_MANIFEST_FILE: &str = "transfer-manifest";
pub(crate) const TRANSFER_ENVELOPE_FILE: &str = "LOA-transfer-envelope";

pub struct TransferDocuments {
    pub manifest: String,
    pub envelope: String,
}

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectId(String);

impl ObjectId {
    pub fn parse(text: &str) -> Result<Self, StoreError> {
        let text = text.trim();
        if text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(Self(text.to_ascii_lowercase()));
        }
        Err(StoreError::InvalidObjectId)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    ShortRead {
        expected: u64,
        read: u64,
    },
    ExistingObjectDiffers {
        id: String,
        expected: u64,
        found: u64,
    },
    MalformedLoaKey {
        path: PathBuf,
        found: u64,
    },
    Entropy(String),
    InvalidClaim(String),
    InvalidEnvelope(String),
    ExistingEnvelopeDiffers {
        id: String,
    },
    InvalidObjectId,
    ObjectNotFound {
        id: String,
    },
    ManifestMismatch {
        id: String,
    },
    MissingPiece {
        id: String,
        index: u32,
    },
    TransferDocumentDiffers {
        id: String,
        name: String,
    },
    NotInLod,
    LodAlreadyExists,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "{source}"),
            Self::ShortRead { expected, read } => {
                write!(
                    formatter,
                    "import ended after {read} bytes; expected {expected}"
                )
            }
            Self::ExistingObjectDiffers {
                id,
                expected,
                found,
            } => write!(
                formatter,
                "object {id} is already stored with length {found}, not {expected}"
            ),
            Self::MalformedLoaKey { path, found } => write!(
                formatter,
                "LOA private key {} holds {found} bytes, expected {SIGNING_SEED_LEN}",
                path.display()
            ),
            Self::Entropy(message) => write!(
                formatter,
                "could not create a Local Object Authority key: {message}"
            ),
            Self::InvalidClaim(message) => write!(formatter, "{message}"),
            Self::InvalidEnvelope(message) => write!(formatter, "{message}"),
            Self::ExistingEnvelopeDiffers { id } => write!(
                formatter,
                "object {id} is already stored with a different LOA envelope"
            ),
            Self::InvalidObjectId => write!(formatter, "object id must be 64 hex characters"),
            Self::ObjectNotFound { id } => write!(formatter, "object {id} is not in the store"),
            Self::ManifestMismatch { id } => {
                write!(formatter, "object {id} manifest does not match its data")
            }
            Self::NotInLod => {
                write!(formatter, "This node is not part of a local object domain")
            }
            Self::LodAlreadyExists => {
                write!(
                    formatter,
                    "This node already belongs to a local object domain"
                )
            }
            Self::MissingPiece { id, index } => {
                write!(formatter, "object {id} has no piece {index}")
            }
            Self::TransferDocumentDiffers { id, name } => {
                write!(formatter, "object {id} already has a different {name}")
            }
        }
    }
}

impl std::error::Error for StoreError {}

const MAX_CLAIMS: usize = 32;
const MAX_CLAIM_BYTES: usize = 64 * 1024;
const RESERVED_CLAIM_NAMES: &[&str] = &["object-id", "signature", "length"];

/// Claims signed into an object's LOA envelope, in the order they were given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    text: String,
}

impl Claims {
    pub const MAX_WIRE_LEN: usize = MAX_CLAIM_BYTES;

    pub fn none() -> Self {
        Self {
            text: String::new(),
        }
    }

    pub fn parse<I, S>(entries: I) -> Result<Self, StoreError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut names = Vec::new();
        let mut text = String::new();
        for entry in entries {
            let entry = entry.as_ref().trim();
            if entry.is_empty() {
                continue;
            }
            let Some((name, value)) = entry.split_once('=') else {
                return Err(StoreError::InvalidClaim(format!(
                    "claim {entry} is not name=value"
                )));
            };
            let name = name.trim();
            let value = value.trim();
            if !valid_claim_name(name) {
                return Err(StoreError::InvalidClaim(format!(
                    "claim name {name} is not allowed"
                )));
            }
            if value.is_empty() {
                return Err(StoreError::InvalidClaim(format!(
                    "claim {name} has an empty value"
                )));
            }
            if value.chars().any(|character| character.is_control()) {
                return Err(StoreError::InvalidClaim(format!(
                    "claim {name} contains a control character"
                )));
            }
            if names.iter().any(|seen| seen == name) {
                return Err(StoreError::InvalidClaim(format!(
                    "claim {name} is repeated"
                )));
            }
            names.push(name.to_string());
            text.push_str(name);
            text.push(' ');
            text.push_str(value);
            text.push('\n');
            if names.len() > MAX_CLAIMS || text.len() > MAX_CLAIM_BYTES {
                return Err(StoreError::InvalidClaim("claims are too long".to_string()));
            }
        }
        Ok(Self { text })
    }

    pub fn from_wire(bytes: &[u8]) -> Result<Self, StoreError> {
        if bytes.len() > MAX_CLAIM_BYTES {
            return Err(StoreError::InvalidClaim("claims are too long".to_string()));
        }
        if bytes.is_empty() {
            return Ok(Self::none());
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| StoreError::InvalidClaim("claims are not utf-8".to_string()))?;
        if !text.ends_with('\n') {
            return Err(StoreError::InvalidClaim("claims are truncated".to_string()));
        }
        let mut entries = Vec::new();
        for line in text.lines() {
            let Some((name, value)) = line.split_once(' ') else {
                return Err(StoreError::InvalidClaim(format!(
                    "claim {line} is not name value"
                )));
            };
            entries.push(format!("{name}={value}"));
        }
        let claims = Self::parse(entries)?;
        if claims.text != text {
            return Err(StoreError::InvalidClaim(
                "claims are not in canonical form".to_string(),
            ));
        }
        Ok(claims)
    }

    pub fn to_wire(&self) -> &[u8] {
        self.text.as_bytes()
    }

    pub(crate) fn as_text(&self) -> &str {
        &self.text
    }

    pub fn claim<'a>(&'a self, name: &str) -> Option<&'a str> {
        self.text.lines().find_map(|line| {
            let (key, value) = line.split_once(' ')?;
            (key == name).then_some(value)
        })
    }
}

fn valid_claim_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    if RESERVED_CLAIM_NAMES.contains(&name) {
        return false;
    }
    characters
        .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
}

impl From<io::Error> for StoreError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

/// A stored object the transfer service can list: its LOA envelope and the length of `data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedObject {
    pub id: ObjectId,
    pub length: u64,
    pub envelope: String,
}

pub struct ObjectStore {
    data: PathBuf,
    loa_key_path: PathBuf,
    signing_seed: Mutex<Option<[u8; SIGNING_SEED_LEN]>>,
}

impl ObjectStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        let data = root.join("data");
        fs::create_dir_all(data.join(".incoming"))?;
        let loa_key_path = loa_private_path(&root);
        let signing_seed = if loa_key_path.is_file() {
            Some(read_loa_signing_seed(&loa_key_path)?)
        } else {
            None
        };
        load_trusted_loas(&loa_key_path)?;
        Ok(Self {
            data,
            loa_key_path,
            signing_seed: Mutex::new(signing_seed),
        })
    }

    /// Create a local object domain (LOD). Writes this node's LOA credentials once.
    ///
    /// A second call fails. The returned bytes are the Ed25519 verifying key.
    pub fn create_lod(&self) -> Result<[u8; 32], StoreError> {
        let mut slot = self
            .signing_seed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_some() || self.loa_key_path.is_file() {
            return Err(StoreError::LodAlreadyExists);
        }
        let mut seed = [0_u8; SIGNING_SEED_LEN];
        if let Err(error) = getrandom::getrandom(&mut seed) {
            seed.zeroize();
            return Err(StoreError::Entropy(error.to_string()));
        }
        let public = SigningKey::from_bytes(&seed).verifying_key().to_bytes();
        if let Err(error) = install_local_loa(&self.loa_key_path, &seed, &public) {
            seed.zeroize();
            return Err(error);
        }
        *slot = Some(seed);
        Ok(public)
    }

    /// Copy a LOA verifying key into this store's trusted set.
    ///
    /// The same key may be installed again. A file that already holds different
    /// bytes is left untouched and reported as an error.
    pub fn trust_loa(&self, public_key: &[u8; 32]) -> Result<(), StoreError> {
        if VerifyingKey::from_bytes(public_key).is_err() {
            return Err(StoreError::InvalidEnvelope(
                "trusted LOA public key is not a valid Ed25519 verifying key".to_string(),
            ));
        }
        ensure_loa_dirs(&self.loa_key_path)?;
        write_trusted(&self.loa_key_path, public_key)
    }

    /// Check an object envelope against this node's trusted LOA public keys.
    ///
    /// The signed bytes are every line before `signature`, including `object-id`.
    pub fn verify_trusted_object_envelope(
        &self,
        envelope: &str,
        object_id: &str,
    ) -> Result<(), StoreError> {
        let (statement, signature_hex) = split_signature(envelope)?;
        if !statement.ends_with(&format!("object-id {object_id}\n")) {
            return Err(StoreError::InvalidEnvelope(
                "object envelope names a different object".to_string(),
            ));
        }
        verify_statement(
            statement,
            signature_hex,
            &load_trusted_loas(&self.loa_key_path)?,
            "object envelope signature does not match a trusted LOA",
        )
    }

    /// Check a transfer envelope against this node's trusted LOA public keys.
    ///
    /// The signed bytes are the claims in the envelope followed by the transfer manifest.
    pub fn verify_trusted_transfer_envelope(
        &self,
        envelope: &str,
        manifest: &str,
    ) -> Result<(), StoreError> {
        let (claims, signature_hex) = split_signature(envelope)?;
        let statement = format!("{claims}{manifest}");
        verify_statement(
            &statement,
            signature_hex,
            &load_trusted_loas(&self.loa_key_path)?,
            "transfer envelope signature does not match a trusted LOA",
        )
    }

    pub fn loa_key_path(&self) -> &Path {
        &self.loa_key_path
    }

    fn signing_seed(&self) -> Result<[u8; SIGNING_SEED_LEN], StoreError> {
        self.signing_seed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .ok_or(StoreError::NotInLod)
    }

    /// A store opened in tests can sign. Create a local object domain (LOD) when this directory has none.
    #[cfg(test)]
    pub fn open_with_lod(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let store = Self::open(root)?;
        if store.signing_seed().is_ok() {
            return Ok(store);
        }
        store.create_lod()?;
        Ok(store)
    }

    pub fn import_reader(
        &self,
        reader: impl Read,
        length: u64,
        claims: &Claims,
    ) -> Result<ObjectId, StoreError> {
        let seed = self.signing_seed()?;
        let staged = self.stage_import(reader, length)?;
        let id = staged.id.clone();
        let destination = self.data.join(&id);
        if destination.exists() {
            require_same_length(&destination, &id, length)?;
            write_loa_envelope(&destination, &id, &seed, claims)?;
            drop(staged.cleanup);
            return Ok(ObjectId(id));
        }
        write_manifest(&staged.staging, &id, length)?;
        write_loa_envelope(&staged.staging, &id, &seed, claims)?;
        self.commit_staged(&staged, &destination)?;
        Ok(ObjectId(id))
    }

    /// Store the bytes and the inventory manifest. This does not write an LOA envelope.
    pub fn import_plain(&self, reader: impl Read, length: u64) -> Result<ObjectId, StoreError> {
        let staged = self.stage_import(reader, length)?;
        let id = staged.id.clone();
        let destination = self.data.join(&id);
        if destination.exists() {
            require_same_length(&destination, &id, length)?;
            drop(staged.cleanup);
            return Ok(ObjectId(id));
        }
        write_manifest(&staged.staging, &id, length)?;
        self.commit_staged(&staged, &destination)?;
        Ok(ObjectId(id))
    }

    /// Store the bytes with an envelope some other authority already signed.
    ///
    /// The signature is checked against `verifying_key` over the claims and object id.
    /// The envelope text is stored unchanged. This stack does not sign a new one.
    pub fn import_envelope(
        &self,
        reader: impl Read,
        length: u64,
        envelope: &str,
        verifying_key: &[u8; 32],
    ) -> Result<ObjectId, StoreError> {
        let staged = self.stage_import(reader, length)?;
        let id = staged.id.clone();
        verify_loa_envelope(envelope, &id, verifying_key)?;
        let destination = self.data.join(&id);
        if destination.exists() {
            require_same_length(&destination, &id, length)?;
            install_envelope_text(&destination, &id, envelope)?;
            drop(staged.cleanup);
            return Ok(ObjectId(id));
        }
        write_manifest(&staged.staging, &id, length)?;
        install_envelope_text(&staged.staging, &id, envelope)?;
        self.commit_staged(&staged, &destination)?;
        Ok(ObjectId(id))
    }

    fn stage_import(&self, mut reader: impl Read, length: u64) -> Result<StagedImport, StoreError> {
        let staging = self.staging_dir()?;
        let cleanup = StagingDir(staging.clone());
        let data_path = staging.join("data");
        let mut data = File::create(&data_path)?;
        let mut hasher = Sha256::new();
        let mut remaining = length;
        let mut buffer = [0_u8; 64 * 1024];
        while remaining > 0 {
            let chunk = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
            let read = reader.read(&mut buffer[..chunk])?;
            if read == 0 {
                return Err(StoreError::ShortRead {
                    expected: length,
                    read: length - remaining,
                });
            }
            hasher.update(&buffer[..read]);
            data.write_all(&buffer[..read])?;
            remaining -= u64::try_from(read).unwrap_or(0);
        }
        data.sync_all()?;
        drop(data);
        Ok(StagedImport {
            cleanup,
            staging,
            id: hex_encode(&hasher.finalize()),
        })
    }

    fn commit_staged(&self, staged: &StagedImport, destination: &Path) -> Result<(), StoreError> {
        fs::rename(&staged.staging, destination)?;
        sync_directory(&self.data)?;
        Ok(())
    }

    pub fn open_data(&self, id: &ObjectId) -> Result<(File, u64), StoreError> {
        let data_path = self.data.join(id.as_str()).join("data");
        let manifest_path = self.data.join(id.as_str()).join("manifest");
        if !data_path.is_file() {
            return Err(StoreError::ObjectNotFound {
                id: id.as_str().to_string(),
            });
        }
        let file = File::open(&data_path)?;
        let length = file.metadata()?.len();
        let manifest = fs::read_to_string(&manifest_path)?;
        if manifest_length(&manifest) != Some(length)
            || !manifest.starts_with(&format!("object-id {}\n", id.as_str()))
        {
            return Err(StoreError::ManifestMismatch {
                id: id.as_str().to_string(),
            });
        }
        Ok((file, length))
    }

    pub fn ensure_transfer_documents(
        &self,
        id: &ObjectId,
    ) -> Result<TransferDocuments, StoreError> {
        let dir = self.data.join(id.as_str());
        if !dir.join("data").is_file() {
            return Err(StoreError::ObjectNotFound {
                id: id.as_str().to_string(),
            });
        }
        let manifest_path = dir.join(TRANSFER_MANIFEST_FILE);
        let manifest = if manifest_path.is_file() {
            fs::read_to_string(&manifest_path)?
        } else {
            let text = build_transfer_manifest(id.as_str(), &dir.join("data"))?;
            write_atomic(&manifest_path, text.as_bytes())?;
            text
        };
        let envelope_path = dir.join(TRANSFER_ENVELOPE_FILE);
        let envelope = if envelope_path.is_file() {
            fs::read_to_string(&envelope_path)?
        } else {
            let loa = fs::read_to_string(dir.join("LOA-envelope"))?;
            let claims = claims_from_loa_envelope(&loa, id.as_str())?;
            let seed = self.signing_seed()?;
            let text = transfer_envelope_text(&claims, &manifest, &seed);
            write_atomic(&envelope_path, text.as_bytes())?;
            text
        };
        Ok(TransferDocuments { manifest, envelope })
    }

    pub fn has_object(&self, id: &ObjectId) -> bool {
        self.data.join(id.as_str()).join("data").is_file()
    }

    pub fn read_piece(&self, id: &ObjectId, index: u32) -> Result<Vec<u8>, StoreError> {
        let piece_path = self
            .data
            .join(id.as_str())
            .join("pieces")
            .join(index.to_string());
        if piece_path.is_file() {
            return Ok(fs::read(piece_path)?);
        }
        let mut file = File::open(self.data.join(id.as_str()).join("data"))?;
        let length = file.metadata()?.len();
        let start = u64::from(index) * u64::try_from(PIECE_SIZE).unwrap_or(0);
        if start >= length {
            return Err(StoreError::MissingPiece {
                id: id.as_str().to_string(),
                index,
            });
        }
        let want =
            usize::try_from((length - start).min(u64::try_from(PIECE_SIZE).unwrap_or(length)))
                .unwrap_or(PIECE_SIZE);
        file.seek(SeekFrom::Start(start))?;
        let mut buffer = vec![0_u8; want];
        file.read_exact(&mut buffer)?;
        Ok(buffer)
    }

    /// Bytes for a who-has request. A stored piece file wins. Otherwise the slice is
    /// `piece_length` bytes of `data` at `index * piece_size`, so a holder can answer
    /// without a transfer manifest.
    pub fn read_piece_window(
        &self,
        id: &ObjectId,
        index: u32,
        piece_size: u32,
        piece_length: u32,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let piece_path = self
            .data
            .join(id.as_str())
            .join("pieces")
            .join(index.to_string());
        if piece_path.is_file() {
            return Ok(Some(fs::read(piece_path)?));
        }
        let data_path = self.data.join(id.as_str()).join("data");
        if !data_path.is_file() {
            return Ok(None);
        }
        let mut file = File::open(data_path)?;
        let file_length = file.metadata()?.len();
        let start = u64::from(index).saturating_mul(u64::from(piece_size));
        let end = start.saturating_add(u64::from(piece_length));
        if end > file_length {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(start))?;
        let mut buffer = vec![0_u8; usize::try_from(piece_length).unwrap_or(usize::MAX)];
        file.read_exact(&mut buffer)?;
        Ok(Some(buffer))
    }

    pub fn read_transfer_manifest(&self, id: &ObjectId) -> Result<Option<String>, StoreError> {
        read_optional_text(&self.data.join(id.as_str()).join(TRANSFER_MANIFEST_FILE))
    }

    pub fn read_loa_envelope(&self, id: &ObjectId) -> Result<Option<String>, StoreError> {
        read_optional_text(&self.data.join(id.as_str()).join("LOA-envelope"))
    }

    pub fn read_transfer_envelope(&self, id: &ObjectId) -> Result<Option<String>, StoreError> {
        read_optional_text(&self.data.join(id.as_str()).join(TRANSFER_ENVELOPE_FILE))
    }

    /// Objects with a data file, a matching manifest, and an LOA envelope.
    ///
    /// Plain imports and partial objects are left out. A remote catalog is the
    /// envelopes of objects this store can preview.
    pub fn published_objects(&self) -> Result<Vec<PublishedObject>, StoreError> {
        let mut objects = Vec::new();
        for entry in fs::read_dir(&self.data)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let Ok(id) = ObjectId::parse(&name) else {
                continue;
            };
            if id.as_str() != name || !entry.path().is_dir() {
                continue;
            }
            let dir = entry.path();
            let envelope_path = dir.join("LOA-envelope");
            let data_path = dir.join("data");
            if !envelope_path.is_file() || !data_path.is_file() {
                continue;
            }
            let envelope = fs::read_to_string(&envelope_path)?;
            if claims_from_loa_envelope(&envelope, id.as_str()).is_err() {
                continue;
            }
            let length = fs::metadata(&data_path)?.len();
            let Some(manifest) = fs::read_to_string(dir.join("manifest"))
                .ok()
                .as_deref()
                .and_then(manifest_length)
            else {
                continue;
            };
            if manifest != length {
                continue;
            }
            objects.push(PublishedObject {
                id,
                length,
                envelope,
            });
        }
        objects.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        Ok(objects)
    }

    /// Inventory line for an object learned from a transfer manifest, before `data` exists.
    pub fn note_object_length(&self, id: &ObjectId, length: u64) -> Result<(), StoreError> {
        let directory = self.data.join(id.as_str());
        fs::create_dir_all(&directory)?;
        let path = directory.join("manifest");
        let text = manifest_text(id.as_str(), length);
        if path.is_file() {
            let existing = fs::read_to_string(&path)?;
            if existing != text {
                return Err(StoreError::ManifestMismatch {
                    id: id.as_str().to_string(),
                });
            }
            return Ok(());
        }
        write_atomic(&path, text.as_bytes())
    }

    /// True when a complete object already carries this firmware version for the board.
    pub fn has_firmware_version(&self, board: &str, version: &str) -> bool {
        let Ok(entries) = fs::read_dir(&self.data) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.join("data").is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if ObjectId::parse(name).is_err() {
                continue;
            }
            let Some(text) = fs::read_to_string(path.join("LOA-envelope"))
                .ok()
                .or_else(|| fs::read_to_string(path.join(TRANSFER_ENVELOPE_FILE)).ok())
            else {
                continue;
            };
            let Ok(claims) = claims_from_document(&text) else {
                continue;
            };
            if claims.claim("object-type") == Some("firmware")
                && claims.claim("board") == Some(board)
                && claims.claim("version") == Some(version)
            {
                return true;
            }
        }
        false
    }

    pub fn read_object_documents(&self, id: &ObjectId) -> Result<(String, String), StoreError> {
        let dir = self.data.join(id.as_str());
        if !dir.join("data").is_file() {
            return Err(StoreError::ObjectNotFound {
                id: id.as_str().to_string(),
            });
        }
        Ok((
            fs::read_to_string(dir.join("manifest"))?,
            fs::read_to_string(dir.join("LOA-envelope"))?,
        ))
    }

    /// Keep the object manifest and LOA-envelope from the transfer initiation.
    pub fn store_object_documents(
        &self,
        id: &ObjectId,
        object_manifest: &str,
        object_envelope: &str,
    ) -> Result<(), StoreError> {
        if !object_manifest.starts_with(&format!("object-id {}\n", id.as_str()))
            || manifest_length(object_manifest).is_none()
        {
            return Err(StoreError::ManifestMismatch {
                id: id.as_str().to_string(),
            });
        }
        claims_from_loa_envelope(object_envelope, id.as_str())?;
        self.verify_trusted_object_envelope(object_envelope, id.as_str())?;
        let destination = self.data.join(id.as_str());
        fs::create_dir_all(&destination)?;
        install_document(&destination.join("manifest"), object_manifest, id.as_str())?;
        install_document(
            &destination.join("LOA-envelope"),
            object_envelope,
            id.as_str(),
        )?;
        Ok(())
    }

    /// Keep the transfer manifest and its envelope as soon as the manifest arrives.
    pub fn store_transfer_documents(
        &self,
        id: &ObjectId,
        transfer_manifest: &str,
        transfer_envelope: &str,
    ) -> Result<(), StoreError> {
        self.verify_trusted_transfer_envelope(transfer_envelope, transfer_manifest)?;
        let destination = self.data.join(id.as_str());
        fs::create_dir_all(&destination)?;
        install_document(
            &destination.join(TRANSFER_MANIFEST_FILE),
            transfer_manifest,
            id.as_str(),
        )?;
        install_document(
            &destination.join(TRANSFER_ENVELOPE_FILE),
            transfer_envelope,
            id.as_str(),
        )?;
        Ok(())
    }

    /// Keep one piece after its manifest hash has matched.
    pub fn store_piece(&self, id: &ObjectId, index: u32, bytes: &[u8]) -> Result<(), StoreError> {
        let directory = self.data.join(id.as_str()).join("pieces");
        fs::create_dir_all(&directory)?;
        let path = directory.join(index.to_string());
        if path.is_file() {
            let existing = fs::read(&path)?;
            if existing != bytes {
                return Err(StoreError::TransferDocumentDiffers {
                    id: id.as_str().to_string(),
                    name: format!("piece {index}"),
                });
            }
            return Ok(());
        }
        let staging = directory.join(format!(".{index}.staging"));
        write_bytes(&staging, bytes)?;
        fs::rename(&staging, &path)?;
        Ok(())
    }

    /// Write the whole object after every piece is present and the object id matches.
    pub fn store_received_bytes(&self, id: &ObjectId, bytes: &[u8]) -> Result<(), StoreError> {
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if hex_encode(&Sha256::digest(bytes)) != id.as_str() {
            return Err(StoreError::ManifestMismatch {
                id: id.as_str().to_string(),
            });
        }
        let destination = self.data.join(id.as_str());
        let object_manifest = fs::read_to_string(destination.join("manifest"))?;
        if manifest_length(&object_manifest) != Some(length) {
            return Err(StoreError::ManifestMismatch {
                id: id.as_str().to_string(),
            });
        }
        let data_path = destination.join("data");
        if data_path.is_file() {
            let found = fs::metadata(&data_path)?.len();
            if found != length {
                return Err(StoreError::ExistingObjectDiffers {
                    id: id.as_str().to_string(),
                    expected: length,
                    found,
                });
            }
        } else {
            let staging = destination.join(".data.staging");
            write_bytes(&staging, bytes)?;
            fs::rename(&staging, &data_path)?;
            sync_directory(&destination)?;
        }
        let pieces = destination.join("pieces");
        if pieces.is_dir() {
            fs::remove_dir_all(pieces)?;
        }
        Ok(())
    }

    fn staging_dir(&self) -> Result<PathBuf, StoreError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = self
            .data
            .join(".incoming")
            .join(format!("{nanos}-{sequence}"));
        fs::create_dir(&path)?;
        Ok(path)
    }
}

impl Drop for ObjectStore {
    fn drop(&mut self) {
        if let Some(seed) = self
            .signing_seed
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            seed.zeroize();
        }
    }
}

fn loa_private_path(root: &Path) -> PathBuf {
    root.join(LOA_DIR)
        .join(LOA_LOCAL_DIR)
        .join(LOA_PRIVATE_FILE)
}

fn loa_public_path(private: &Path) -> PathBuf {
    private.with_file_name(LOA_PUBLIC_FILE)
}

fn trusted_loa_path(private: &Path, public_key: &[u8; 32]) -> PathBuf {
    private
        .parent()
        .and_then(Path::parent)
        .expect("loa directory")
        .join(LOA_TRUSTED_DIR)
        .join(hex_encode(public_key))
}

fn read_loa_signing_seed(path: &Path) -> Result<[u8; SIGNING_SEED_LEN], StoreError> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length != SIGNING_SEED_LEN as u64 {
        return Err(StoreError::MalformedLoaKey {
            path: path.to_path_buf(),
            found: length,
        });
    }
    let mut seed = [0_u8; SIGNING_SEED_LEN];
    file.read_exact(&mut seed)?;
    Ok(seed)
}

fn install_local_loa(
    private_path: &Path,
    seed: &[u8; SIGNING_SEED_LEN],
    public: &[u8; 32],
) -> Result<(), StoreError> {
    ensure_loa_dirs(private_path)?;
    let staging = private_path.with_file_name(format!(
        ".{LOA_PRIVATE_FILE}.{}.staging",
        std::process::id()
    ));
    if let Err(error) = write_secret(&staging, seed) {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    let created = match fs::hard_link(&staging, private_path) {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::AlreadyExists => false,
        Err(error) => {
            let _ = fs::remove_file(&staging);
            return Err(error.into());
        }
    };
    let _ = fs::remove_file(&staging);
    if !created {
        return Err(StoreError::LodAlreadyExists);
    }
    if let Err(error) = write_public(&loa_public_path(private_path), public) {
        let _ = fs::remove_file(private_path);
        return Err(error);
    }
    if let Err(error) = write_trusted(private_path, public) {
        let _ = fs::remove_file(private_path);
        let _ = fs::remove_file(loa_public_path(private_path));
        return Err(error);
    }
    Ok(())
}

fn write_trusted(private_path: &Path, public: &[u8; 32]) -> Result<(), StoreError> {
    let path = trusted_loa_path(private_path, public);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        fs::set_permissions(parent, fs::Permissions::from_mode(0o755))?;
    }
    write_public(&path, public)
}

fn write_secret(path: &Path, secret: &[u8; SIGNING_SEED_LEN]) -> Result<(), StoreError> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(secret)?;
    file.sync_all()?;
    Ok(())
}

fn write_public(path: &Path, public: &[u8; 32]) -> Result<(), StoreError> {
    if path.is_file() {
        let existing = fs::read(path)?;
        if existing == public {
            return Ok(());
        }
        return Err(StoreError::InvalidEnvelope(
            "LOA public key already exists with different bytes".to_string(),
        ));
    }
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o644);
    let mut file = options.open(path)?;
    file.write_all(public)?;
    file.sync_all()?;
    Ok(())
}

fn ensure_loa_dirs(private_path: &Path) -> Result<(), StoreError> {
    let Some(local) = private_path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(local)?;
    #[cfg(unix)]
    fs::set_permissions(local, fs::Permissions::from_mode(0o700))?;
    let Some(loa) = local.parent() else {
        return Ok(());
    };
    #[cfg(unix)]
    fs::set_permissions(loa, fs::Permissions::from_mode(0o700))?;
    let trusted = loa.join(LOA_TRUSTED_DIR);
    fs::create_dir_all(&trusted)?;
    #[cfg(unix)]
    fs::set_permissions(&trusted, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn write_loa_envelope(
    dir: &Path,
    object_id: &str,
    signing_seed: &[u8; SIGNING_SEED_LEN],
    claims: &Claims,
) -> Result<(), StoreError> {
    let text = envelope_text(object_id, signing_seed, claims);
    install_envelope_text(dir, object_id, &text)
}

fn install_envelope_text(dir: &Path, object_id: &str, text: &str) -> Result<(), StoreError> {
    let path = dir.join("LOA-envelope");
    if path.is_file() {
        let existing = fs::read_to_string(&path)?;
        if existing == text {
            return Ok(());
        }
        return Err(StoreError::ExistingEnvelopeDiffers {
            id: object_id.to_string(),
        });
    }
    let staging = dir.join(".LOA-envelope.staging");
    let mut file = File::create(&staging)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&staging, &path)?;
    Ok(())
}

fn write_manifest(dir: &Path, object_id: &str, length: u64) -> Result<(), StoreError> {
    let mut manifest_file = File::create(dir.join("manifest"))?;
    manifest_file.write_all(manifest_text(object_id, length).as_bytes())?;
    manifest_file.sync_all()?;
    Ok(())
}

fn require_same_length(destination: &Path, object_id: &str, length: u64) -> Result<(), StoreError> {
    let found = fs::metadata(destination.join("data"))?.len();
    if found != length {
        return Err(StoreError::ExistingObjectDiffers {
            id: object_id.to_string(),
            expected: length,
            found,
        });
    }
    Ok(())
}

/// Ed25519 verifying key, or the signing half of a 64-byte identity public key.
pub fn parse_verifying_key(text: &str) -> Result<[u8; 32], StoreError> {
    let bytes = hex_decode(text.trim()).ok_or_else(|| {
        StoreError::InvalidEnvelope(
            "authority must be an Ed25519 verifying key (64 hex characters) or an identity public key (128 hex characters)".to_string(),
        )
    })?;
    let signing = match bytes.len() {
        32 => bytes,
        64 => bytes[32..].to_vec(),
        _ => {
            return Err(StoreError::InvalidEnvelope(
                "authority must be an Ed25519 verifying key (64 hex characters) or an identity public key (128 hex characters)".to_string(),
            ));
        }
    };
    let key: [u8; 32] = signing.as_slice().try_into().expect("32 bytes");
    if VerifyingKey::from_bytes(&key).is_err() {
        return Err(StoreError::InvalidEnvelope(
            "authority is not a valid Ed25519 verifying key".to_string(),
        ));
    }
    Ok(key)
}

fn load_trusted_loas(private_path: &Path) -> Result<Vec<[u8; 32]>, StoreError> {
    let Some(loa) = private_path.parent().and_then(Path::parent) else {
        return Ok(Vec::new());
    };
    let trusted = loa.join(LOA_TRUSTED_DIR);
    if !trusted.is_dir() {
        return Ok(Vec::new());
    }
    let mut keys = Vec::new();
    for entry in fs::read_dir(&trusted)? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path)?;
        if bytes.len() != 32 {
            return Err(StoreError::InvalidEnvelope(format!(
                "trusted LOA public key {} holds {} bytes, expected 32",
                path.display(),
                bytes.len()
            )));
        }
        let mut key = [0_u8; 32];
        key.copy_from_slice(&bytes);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name != hex_encode(&key) {
            return Err(StoreError::InvalidEnvelope(format!(
                "trusted LOA public key {} is not named with its hex encoding",
                path.display()
            )));
        }
        if VerifyingKey::from_bytes(&key).is_err() {
            return Err(StoreError::InvalidEnvelope(format!(
                "trusted LOA public key {} is not a valid Ed25519 verifying key",
                path.display()
            )));
        }
        keys.push(key);
    }
    Ok(keys)
}

fn split_signature(envelope: &str) -> Result<(&str, &str), StoreError> {
    let Some((statement, signature_line)) = envelope.rsplit_once("signature ") else {
        return Err(StoreError::InvalidEnvelope(
            "object envelope has no signature".to_string(),
        ));
    };
    let signature_hex = signature_line.strip_suffix('\n').unwrap_or(signature_line);
    if signature_hex.is_empty() || signature_hex.contains('\n') {
        return Err(StoreError::InvalidEnvelope(
            "object envelope has no signature".to_string(),
        ));
    }
    Ok((statement, signature_hex))
}

fn verify_statement(
    statement: &str,
    signature_hex: &str,
    keys: &[[u8; 32]],
    mismatch: &str,
) -> Result<(), StoreError> {
    if keys.is_empty() {
        return Err(StoreError::NotInLod);
    }
    let signature_bytes = hex_decode(signature_hex)
        .filter(|bytes| bytes.len() == 64)
        .ok_or_else(|| {
            StoreError::InvalidEnvelope("object envelope signature is not 64 bytes".to_string())
        })?;
    let signature = Signature::from_slice(&signature_bytes).map_err(|_| {
        StoreError::InvalidEnvelope("object envelope signature is not 64 bytes".to_string())
    })?;
    for key in keys {
        let verifying = VerifyingKey::from_bytes(key).map_err(|_| {
            StoreError::InvalidEnvelope(
                "authority is not a valid Ed25519 verifying key".to_string(),
            )
        })?;
        if verifying.verify(statement.as_bytes(), &signature).is_ok() {
            return Ok(());
        }
    }
    Err(StoreError::InvalidEnvelope(mismatch.to_string()))
}

fn verify_loa_envelope(
    envelope: &str,
    object_id: &str,
    verifying_key: &[u8; 32],
) -> Result<(), StoreError> {
    let (statement, signature_hex) = split_signature(envelope)?;
    if !statement.ends_with(&format!("object-id {object_id}\n")) {
        return Err(StoreError::InvalidEnvelope(
            "object envelope names a different object".to_string(),
        ));
    }
    verify_statement(
        statement,
        signature_hex,
        &[*verifying_key],
        "object envelope signature does not match the authority",
    )
}

struct StagedImport {
    cleanup: StagingDir,
    staging: PathBuf,
    id: String,
}

fn envelope_text(
    object_id: &str,
    signing_seed: &[u8; SIGNING_SEED_LEN],
    claims: &Claims,
) -> String {
    let statement = signed_statement(object_id, claims);
    let signature = SigningKey::from_bytes(signing_seed).sign(statement.as_bytes());
    format!(
        "{statement}signature {}\n",
        hex_encode(&signature.to_bytes())
    )
}

fn signed_statement(object_id: &str, claims: &Claims) -> String {
    format!("{}object-id {object_id}\n", claims.as_text())
}

fn transfer_envelope_text(
    claims: &Claims,
    manifest: &str,
    signing_seed: &[u8; SIGNING_SEED_LEN],
) -> String {
    let statement = format!("{}{manifest}", claims.as_text());
    let signature = SigningKey::from_bytes(signing_seed).sign(statement.as_bytes());
    format!(
        "{}signature {}\n",
        claims.as_text(),
        hex_encode(&signature.to_bytes())
    )
}

fn read_optional_text(path: &Path) -> Result<Option<String>, StoreError> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(fs::read_to_string(path)?))
}

pub(crate) fn claims_from_document(text: &str) -> Result<Claims, StoreError> {
    let mut entries = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with("signature ") || line.starts_with("object-id ") {
            break;
        }
        let Some((name, value)) = line.split_once(' ') else {
            return Err(StoreError::InvalidClaim(format!(
                "claim {line} is not name value"
            )));
        };
        entries.push(format!("{name}={value}"));
    }
    Claims::parse(entries)
}

pub(crate) fn claims_from_loa_envelope(text: &str, object_id: &str) -> Result<Claims, StoreError> {
    let mut entries = Vec::new();
    let mut saw_object = false;
    for line in text.lines() {
        if let Some(id) = line.strip_prefix("object-id ") {
            if id != object_id {
                return Err(StoreError::ManifestMismatch {
                    id: object_id.to_string(),
                });
            }
            saw_object = true;
            break;
        }
        if line.starts_with("signature ") || line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(' ') else {
            return Err(StoreError::InvalidClaim(format!(
                "claim {line} is not name value"
            )));
        };
        entries.push(format!("{name}={value}"));
    }
    if !saw_object {
        return Err(StoreError::InvalidClaim(
            "LOA envelope has no object id".to_string(),
        ));
    }
    Claims::parse(entries)
}

fn build_transfer_manifest(object_id: &str, data: &Path) -> Result<String, StoreError> {
    let mut file = File::open(data)?;
    let mut length = 0_u64;
    let mut index = 0_u32;
    let mut pieces = String::new();
    let mut buffer = [0_u8; PIECE_SIZE];
    loop {
        let mut filled = 0;
        while filled < buffer.len() {
            let read = file.read(&mut buffer[filled..])?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            break;
        }
        let hash = hex_encode(&Sha256::digest(&buffer[..filled]));
        pieces.push_str(&format!("piece {index} {hash}\n"));
        length += u64::try_from(filled).unwrap_or(0);
        index = index.saturating_add(1);
    }
    Ok(format!(
        "object-id {object_id}\nlength {length}\npiece-size {PIECE_SIZE}\n{pieces}"
    ))
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("document");
    let staging = path.with_file_name(format!(".{name}.staging"));
    let mut file = File::create(&staging)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    match fs::rename(&staging, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound && path.is_file() => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn install_document(path: &Path, text: &str, object_id: &str) -> Result<(), StoreError> {
    if path.is_file() {
        let existing = fs::read_to_string(path)?;
        if existing == text {
            return Ok(());
        }
        return Err(StoreError::TransferDocumentDiffers {
            id: object_id.to_string(),
            name: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("document")
                .to_string(),
        });
    }
    write_atomic(path, text.as_bytes())
}

fn manifest_text(object_id: &str, length: u64) -> String {
    format!("object-id {object_id}\nlength {length}\n")
}

pub(crate) fn manifest_length(text: &str) -> Option<u64> {
    let length_line = text.lines().nth(1)?;
    length_line.strip_prefix("length ")?.parse().ok()
}

fn sync_directory(path: &Path) -> Result<(), StoreError> {
    open_directory(path)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn open_directory(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Windows flushes a directory only through a writable handle opened with
/// backup semantics. A read-only handle makes `FlushFileBuffers` return
/// access denied.
#[cfg(windows)]
fn open_directory(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

pub(crate) fn hex_decode(text: &str) -> Option<Vec<u8>> {
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

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

struct StagingDir(PathBuf);

impl Drop for StagingDir {
    fn drop(&mut self) {
        if self.0.as_os_str().is_empty() {
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Cursor, Read};

    use ed25519_dalek::{Signer, Verifier};

    use super::{hex_decode, hex_encode, manifest_text, Claims, ObjectId, ObjectStore};

    #[test]
    fn import_writes_data_and_manifest_and_repeats_the_same_id() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open_with_lod(root.path()).expect("store");
        let bytes = b"abc";
        let first = store
            .import_reader(Cursor::new(bytes), bytes.len() as u64, &Claims::none())
            .expect("import");
        let envelope_before = fs::read(
            root.path()
                .join("data")
                .join(first.as_str())
                .join("LOA-envelope"),
        )
        .expect("envelope");
        let second = store
            .import_reader(Cursor::new(bytes), bytes.len() as u64, &Claims::none())
            .expect("repeat");
        assert_eq!(first, second);
        assert_eq!(
            fs::read(
                root.path()
                    .join("data")
                    .join(first.as_str())
                    .join("LOA-envelope")
            )
            .expect("envelope"),
            envelope_before
        );
        assert_eq!(
            first.as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let object = root.path().join("data").join(first.as_str());
        assert!(root.path().join("loa").join("local").is_dir());
        assert!(!root.path().join(first.as_str()).exists());
        assert_eq!(fs::read(object.join("data")).expect("data"), bytes);
        assert_eq!(
            fs::read_to_string(object.join("manifest")).expect("manifest"),
            manifest_text(first.as_str(), bytes.len() as u64)
        );
        let envelope = fs::read_to_string(object.join("LOA-envelope")).expect("envelope");
        let statement = format!("object-id {}\n", first.as_str());
        assert!(envelope.starts_with(&statement));
        let signature = envelope
            .strip_prefix(&statement)
            .and_then(|rest| rest.strip_prefix("signature "))
            .and_then(|rest| rest.strip_suffix('\n'))
            .expect("signature line");
        let signature_bytes = hex_decode(signature).expect("signature hex");
        let key = fs::read(root.path().join("loa").join("local").join("private")).expect("key");
        assert_eq!(key.len(), 32);
        let signing =
            ed25519_dalek::SigningKey::from_bytes(key.as_slice().try_into().expect("seed"));
        let signature = ed25519_dalek::Signature::from_bytes(
            signature_bytes.as_slice().try_into().expect("sig"),
        );
        signing
            .verifying_key()
            .verify(statement.as_bytes(), &signature)
            .expect("signature verifies");
        let again = ObjectStore::open(root.path()).expect("reopen");
        assert_eq!(fs::read(again.loa_key_path()).expect("same key"), key);
    }

    #[test]
    fn open_does_not_create_loa_credentials() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open(root.path()).expect("store");
        assert!(!store.loa_key_path().is_file());
        let error = store
            .import_reader(Cursor::new(b"abc"), 3, &Claims::none())
            .expect_err("signing import");
        assert_eq!(
            error.to_string(),
            "This node is not part of a local object domain"
        );

        let bytes = b"abc";
        let id = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let signing = ed25519_dalek::SigningKey::from_bytes(&[9_u8; 32]);
        let statement = format!("object-id {id}\n");
        let signature = signing.sign(statement.as_bytes());
        let envelope = format!(
            "{statement}signature {}\n",
            hex_encode(&signature.to_bytes())
        );
        store
            .import_envelope(
                Cursor::new(bytes),
                bytes.len() as u64,
                &envelope,
                &signing.verifying_key().to_bytes(),
            )
            .expect("stored envelope");
        let stored = ObjectId::parse(id).expect("id");
        match store.ensure_transfer_documents(&stored) {
            Err(error) => assert_eq!(
                error.to_string(),
                "This node is not part of a local object domain"
            ),
            Ok(_) => panic!("transfer signature must require LOA credentials"),
        }

        let public = store.create_lod().expect("create lod");
        assert_eq!(public.len(), 32);
        assert!(store.loa_key_path().is_file());
        assert_eq!(
            fs::read(root.path().join("loa").join("local").join("public")).expect("public"),
            public
        );
        assert_eq!(
            fs::read(
                root.path()
                    .join("loa")
                    .join("trusted")
                    .join(hex_encode(&public))
            )
            .expect("trusted"),
            public
        );
        store
            .ensure_transfer_documents(&stored)
            .expect("signed after create");
        let again = store.create_lod().expect_err("second create");
        assert_eq!(
            again.to_string(),
            "This node already belongs to a local object domain"
        );
        let reopened = ObjectStore::open(root.path()).expect("reopen");
        let still = reopened.create_lod().expect_err("create after reopen");
        assert_eq!(
            still.to_string(),
            "This node already belongs to a local object domain"
        );
    }

    #[test]
    fn trust_loa_installs_a_public_key_without_a_local_secret() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open(root.path()).expect("store");
        let public = ed25519_dalek::SigningKey::from_bytes(&[7_u8; 32])
            .verifying_key()
            .to_bytes();
        store.trust_loa(&public).expect("trust");
        store.trust_loa(&public).expect("same key again");
        assert!(!store.loa_key_path().is_file());
        assert!(root.path().join("loa").join("local").is_dir());
        assert_eq!(
            fs::read(
                root.path()
                    .join("loa")
                    .join("trusted")
                    .join(hex_encode(&public))
            )
            .expect("trusted"),
            public
        );
    }

    #[test]
    fn an_offer_envelope_must_match_a_trusted_loa() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open_with_lod(root.path()).expect("store");
        let id = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let object_id = ObjectId::parse(id).expect("id");
        let manifest = format!("object-id {id}\nlength 3\n");
        let statement = format!("object-id {id}\n");
        let foreign = ed25519_dalek::SigningKey::from_bytes(&[9_u8; 32]);
        let foreign_envelope = format!(
            "{statement}signature {}\n",
            hex_encode(&foreign.sign(statement.as_bytes()).to_bytes())
        );
        let error = store
            .store_object_documents(&object_id, &manifest, &foreign_envelope)
            .expect_err("foreign signature");
        assert!(error.to_string().contains("does not match a trusted LOA"));
        assert!(!root
            .path()
            .join("data")
            .join(id)
            .join("LOA-envelope")
            .exists());

        let local = fs::read(store.loa_key_path()).expect("private");
        let signing =
            ed25519_dalek::SigningKey::from_bytes(local.as_slice().try_into().expect("seed"));
        let envelope = format!(
            "{statement}signature {}\n",
            hex_encode(&signing.sign(statement.as_bytes()).to_bytes())
        );
        store
            .store_object_documents(&object_id, &manifest, &envelope)
            .expect("trusted signature");

        let transfer_manifest =
            format!("object-id {id}\nlength 3\npiece-size 4096\npiece 0 {id}\n");
        let foreign_transfer = format!(
            "signature {}\n",
            hex_encode(&foreign.sign(transfer_manifest.as_bytes()).to_bytes())
        );
        let error = store
            .store_transfer_documents(&object_id, &transfer_manifest, &foreign_transfer)
            .expect_err("foreign transfer signature");
        assert!(error.to_string().contains("does not match a trusted LOA"));
        let transfer_envelope = format!(
            "signature {}\n",
            hex_encode(&signing.sign(transfer_manifest.as_bytes()).to_bytes())
        );
        store
            .store_transfer_documents(&object_id, &transfer_manifest, &transfer_envelope)
            .expect("trusted transfer signature");

        let bare_root = tempfile::tempdir().expect("temp dir");
        let bare = ObjectStore::open(bare_root.path()).expect("bare");
        let missing = bare
            .store_object_documents(&object_id, &manifest, &envelope)
            .expect_err("no trusted key");
        assert_eq!(
            missing.to_string(),
            "This node is not part of a local object domain"
        );
    }

    #[test]
    fn open_refuses_a_short_loa_key() {
        let root = tempfile::tempdir().expect("temp dir");
        let local = root.path().join("loa").join("local");
        fs::create_dir_all(&local).expect("local");
        fs::write(local.join("private"), [1, 2, 3]).expect("short key");
        match ObjectStore::open(root.path()) {
            Err(error) => assert!(error.to_string().contains("3 bytes")),
            Ok(_store) => panic!("a short Local Object Authority key must not open"),
        }
    }

    #[test]
    fn import_signs_the_claims_into_the_loa_envelope() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open_with_lod(root.path()).expect("store");
        let bytes = b"abc";
        let claims = Claims::parse([
            "board=heltec-v4-r8",
            "version=1.2.3",
            "object-type=firmware",
            "flash-mode=ota",
            "provenance=local-build",
        ])
        .expect("claims");
        let id = store
            .import_reader(Cursor::new(bytes), bytes.len() as u64, &claims)
            .expect("import");
        let object = root.path().join("data").join(id.as_str());
        let envelope = fs::read_to_string(object.join("LOA-envelope")).expect("envelope");
        let statement = format!(
            "board heltec-v4-r8\nversion 1.2.3\nobject-type firmware\nflash-mode ota\nprovenance local-build\nobject-id {}\n",
            id.as_str()
        );
        assert!(envelope.starts_with(&statement));
        let signature = envelope
            .strip_prefix(&statement)
            .and_then(|rest| rest.strip_prefix("signature "))
            .and_then(|rest| rest.strip_suffix('\n'))
            .expect("signature line");
        let signature_bytes = hex_decode(signature).expect("signature hex");
        let key = fs::read(root.path().join("loa").join("local").join("private")).expect("key");
        let signing =
            ed25519_dalek::SigningKey::from_bytes(key.as_slice().try_into().expect("seed"));
        let signature = ed25519_dalek::Signature::from_bytes(
            signature_bytes.as_slice().try_into().expect("sig"),
        );
        signing
            .verifying_key()
            .verify(statement.as_bytes(), &signature)
            .expect("signature verifies");

        let again = store
            .import_reader(Cursor::new(bytes), bytes.len() as u64, &claims)
            .expect("same claims");
        assert_eq!(again, id);
        let changed = Claims::parse(["board=heltec-v4-r8", "provenance=imported"]).expect("claims");
        let error = store
            .import_reader(Cursor::new(bytes), bytes.len() as u64, &changed)
            .expect_err("different claims");
        assert!(error.to_string().contains("different LOA envelope"));
        assert_eq!(
            fs::read_to_string(object.join("LOA-envelope")).expect("envelope"),
            envelope
        );
    }

    #[test]
    fn a_plain_import_writes_data_and_manifest_without_an_envelope() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open(root.path()).expect("store");
        let bytes = b"abc";
        let id = store
            .import_plain(Cursor::new(bytes), bytes.len() as u64)
            .expect("import");
        let object = root.path().join("data").join(id.as_str());
        assert_eq!(fs::read(object.join("data")).expect("data"), bytes);
        assert!(object.join("manifest").is_file());
        assert!(!object.join("LOA-envelope").exists());
    }

    #[test]
    fn an_existing_envelope_is_stored_when_its_signature_matches() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open(root.path()).expect("store");
        let bytes = b"abc";
        let id = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let signing = ed25519_dalek::SigningKey::from_bytes(&[7_u8; 32]);
        let statement = format!("board heltec-v4-r8\nobject-id {id}\n");
        let signature = signing.sign(statement.as_bytes());
        let envelope = format!(
            "{statement}signature {}\n",
            hex_encode(&signature.to_bytes())
        );
        let key = signing.verifying_key().to_bytes();
        let stored = store
            .import_envelope(Cursor::new(bytes), bytes.len() as u64, &envelope, &key)
            .expect("import");
        assert_eq!(stored.as_str(), id);
        let object = root.path().join("data").join(id);
        assert_eq!(
            fs::read_to_string(object.join("LOA-envelope")).expect("envelope"),
            envelope
        );

        let other = ed25519_dalek::SigningKey::from_bytes(&[8_u8; 32])
            .verifying_key()
            .to_bytes();
        let error = store
            .import_envelope(Cursor::new(bytes), bytes.len() as u64, &envelope, &other)
            .expect_err("wrong authority");
        assert!(error.to_string().contains("does not match"));
        assert_eq!(
            fs::read_to_string(object.join("LOA-envelope")).expect("unchanged"),
            envelope
        );

        let tampered = envelope.replacen("board", "boarX", 1);
        let fresh = tempfile::tempdir().expect("temp dir");
        let fresh_store = ObjectStore::open(fresh.path()).expect("store");
        let error = fresh_store
            .import_envelope(Cursor::new(bytes), bytes.len() as u64, &tampered, &key)
            .expect_err("tampered");
        assert!(error.to_string().contains("does not match"));
        assert!(!fresh.path().join("data").join(id).exists());
    }

    #[test]
    fn a_claim_must_be_name_equals_value() {
        let error = Claims::parse(["board"]).expect_err("claim");
        assert!(error.to_string().contains("name=value"));
        let error = Claims::parse(["board=heltec-v4-r8", "board=other"]).expect_err("repeat");
        assert!(error.to_string().contains("repeated"));
    }

    #[test]
    fn open_data_returns_the_stored_bytes() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open_with_lod(root.path()).expect("store");
        let id = store
            .import_reader(Cursor::new(b"abc"), 3, &Claims::none())
            .expect("import");
        let (mut file, length) = store.open_data(&id).expect("open");
        assert_eq!(length, 3);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).expect("read");
        assert_eq!(bytes, b"abc");
        let missing =
            ObjectId::parse("0000000000000000000000000000000000000000000000000000000000000000")
                .expect("id");
        let error = store.open_data(&missing).expect_err("missing");
        assert!(error.to_string().contains("not in the store"));
        assert!(ObjectId::parse("../data").is_err());
    }
}
