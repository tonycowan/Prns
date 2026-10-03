use std::fmt;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::path::{Path, PathBuf};

use crate::config::{self, ConfigError};
use crate::store::{
    claims_from_document, claims_from_loa_envelope, manifest_length, Claims, ObjectId, StoreError,
    TRANSFER_ENVELOPE_FILE, TRANSFER_MANIFEST_FILE,
};

const MANIFEST_FILE: &str = "manifest";
const LOA_ENVELOPE_FILE: &str = "LOA-envelope";
const DATA_FILE: &str = "data";

/// Where a browser path pointed, without opening keys or creating directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreLocation {
    Stack {
        config_dir: PathBuf,
        store_root: PathBuf,
    },
    StoreRoot(PathBuf),
    DataDir(PathBuf),
}

impl StoreLocation {
    pub fn data_dir(&self) -> PathBuf {
        match self {
            Self::Stack { store_root, .. } | Self::StoreRoot(store_root) => store_root.join("data"),
            Self::DataDir(data_dir) => data_dir.clone(),
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Self::Stack {
                config_dir,
                store_root,
            } => format!(
                "stack {} · store {}",
                config_dir.display(),
                store_root.display()
            ),
            Self::StoreRoot(root) => format!("store {}", root.display()),
            Self::DataDir(dir) => format!("data {}", dir.display()),
        }
    }
}

#[derive(Debug)]
pub enum LocateError {
    NotAStore { path: PathBuf },
    Config(ConfigError),
}

impl fmt::Display for LocateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAStore { path } => write!(
                formatter,
                "{} is not a stack config directory, an object store root, or a data directory",
                path.display()
            ),
            Self::Config(source) => write!(formatter, "{source}"),
        }
    }
}

impl std::error::Error for LocateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(source) => Some(source),
            Self::NotAStore { .. } => None,
        }
    }
}

/// Resolve a path the browser can read. This does not create directories or keys.
pub fn locate_store(path: &Path) -> Result<StoreLocation, LocateError> {
    if !path.is_dir() {
        return Err(LocateError::NotAStore {
            path: path.to_path_buf(),
        });
    }
    if path.join("config").is_file() {
        let stack = config::load_stack(path).map_err(LocateError::Config)?;
        return Ok(StoreLocation::Stack {
            config_dir: path.to_path_buf(),
            store_root: stack.object_store,
        });
    }
    if path.join("data").is_dir() {
        return Ok(StoreLocation::StoreRoot(path.to_path_buf()));
    }
    if is_data_dir(path) {
        return Ok(StoreLocation::DataDir(path.to_path_buf()));
    }
    Err(LocateError::NotAStore {
        path: path.to_path_buf(),
    })
}

fn is_data_dir(path: &Path) -> bool {
    if path.file_name().and_then(|name| name.to_str()) == Some("data") {
        return true;
    }
    if path.join(".incoming").is_dir() {
        return true;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            return false;
        };
        entry.path().is_dir() && ObjectId::parse(&name).is_ok_and(|id| id.as_str() == name)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectPresence {
    Complete,
    Partial,
    Mismatch,
    Empty,
}

/// One object directory, read without the Local Object Authority key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id: ObjectId,
    pub presence: ObjectPresence,
    pub manifest_length: Option<u64>,
    pub data_length: Option<u64>,
    pub claims: Vec<(String, String)>,
    pub has_loa_envelope: bool,
    pub has_transfer_manifest: bool,
    pub has_transfer_envelope: bool,
    pub piece_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRecord {
    pub manifest: Option<String>,
    pub loa_envelope: Option<String>,
    pub transfer_manifest: Option<String>,
    pub transfer_envelope: Option<String>,
}

/// One remote catalog record: the LOA envelope, and the length that came with it.
pub fn entry_from_envelope(envelope: &str, length: u64) -> Result<CatalogEntry, StoreError> {
    let id = envelope
        .lines()
        .find_map(|line| line.strip_prefix("object-id "))
        .ok_or_else(|| StoreError::InvalidClaim("LOA envelope has no object id".to_string()))?;
    let id = ObjectId::parse(id)?;
    let claims = claims_from_loa_envelope(envelope, id.as_str())?;
    Ok(CatalogEntry {
        id,
        presence: ObjectPresence::Complete,
        manifest_length: Some(length),
        data_length: Some(length),
        claims: claim_pairs(&claims),
        has_loa_envelope: true,
        has_transfer_manifest: false,
        has_transfer_envelope: false,
        piece_count: 0,
    })
}

pub fn read_catalog(data_dir: &Path) -> Result<Vec<CatalogEntry>, StoreError> {
    if !data_dir.is_dir() {
        return Err(StoreError::Io(io::Error::new(
            ErrorKind::NotFound,
            format!("no data directory at {}", data_dir.display()),
        )));
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(data_dir)? {
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
        entries.push(read_entry(entry.path(), id)?);
    }
    entries.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    Ok(entries)
}

pub fn read_object_record(data_dir: &Path, id: &ObjectId) -> Result<ObjectRecord, StoreError> {
    let dir = data_dir.join(id.as_str());
    if !dir.is_dir() {
        return Err(StoreError::ObjectNotFound {
            id: id.as_str().to_string(),
        });
    }
    Ok(ObjectRecord {
        manifest: read_optional_text(&dir.join(MANIFEST_FILE))?,
        loa_envelope: read_optional_text(&dir.join(LOA_ENVELOPE_FILE))?,
        transfer_manifest: read_optional_text(&dir.join(TRANSFER_MANIFEST_FILE))?,
        transfer_envelope: read_optional_text(&dir.join(TRANSFER_ENVELOPE_FILE))?,
    })
}

pub fn read_data_prefix(
    data_dir: &Path,
    id: &ObjectId,
    limit: usize,
) -> Result<Option<Vec<u8>>, StoreError> {
    let path = data_dir.join(id.as_str()).join(DATA_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let mut file = File::open(path)?;
    let mut buffer = vec![0_u8; limit];
    let read = file.read(&mut buffer)?;
    buffer.truncate(read);
    Ok(Some(buffer))
}

fn read_entry(dir: PathBuf, id: ObjectId) -> Result<CatalogEntry, StoreError> {
    let data_path = dir.join(DATA_FILE);
    let data_length = if data_path.is_file() {
        Some(fs::metadata(&data_path)?.len())
    } else {
        None
    };
    let manifest_length = fs::read_to_string(dir.join(MANIFEST_FILE))
        .ok()
        .as_deref()
        .and_then(manifest_length);
    let has_loa_envelope = dir.join(LOA_ENVELOPE_FILE).is_file();
    let has_transfer_manifest = dir.join(TRANSFER_MANIFEST_FILE).is_file();
    let has_transfer_envelope = dir.join(TRANSFER_ENVELOPE_FILE).is_file();
    let claims = read_claims(&dir);
    let piece_count = count_pieces(&dir);
    let presence = presence(
        data_length,
        manifest_length,
        piece_count,
        has_loa_envelope || has_transfer_manifest || has_transfer_envelope,
    );
    Ok(CatalogEntry {
        id,
        presence,
        manifest_length,
        data_length,
        claims,
        has_loa_envelope,
        has_transfer_manifest,
        has_transfer_envelope,
        piece_count,
    })
}

fn presence(
    data_length: Option<u64>,
    manifest_length: Option<u64>,
    piece_count: usize,
    has_documents: bool,
) -> ObjectPresence {
    if let Some(found) = data_length {
        return if manifest_length == Some(found) {
            ObjectPresence::Complete
        } else {
            ObjectPresence::Mismatch
        };
    }
    if manifest_length.is_some() || piece_count > 0 || has_documents {
        ObjectPresence::Partial
    } else {
        ObjectPresence::Empty
    }
}

fn read_claims(dir: &Path) -> Vec<(String, String)> {
    let text = fs::read_to_string(dir.join(LOA_ENVELOPE_FILE))
        .ok()
        .or_else(|| fs::read_to_string(dir.join(TRANSFER_ENVELOPE_FILE)).ok());
    let Some(text) = text else {
        return Vec::new();
    };
    claims_from_document(&text)
        .map(|claims| claim_pairs(&claims))
        .unwrap_or_default()
}

fn claim_pairs(claims: &Claims) -> Vec<(String, String)> {
    claims
        .as_text()
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(' ')?;
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

fn count_pieces(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir.join("pieces")) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry.path().is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.parse::<u32>().is_ok())
        })
        .count()
}

fn read_optional_text(path: &Path) -> Result<Option<String>, StoreError> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(fs::read_to_string(path)?))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::{locate_store, read_catalog, read_data_prefix, read_object_record, ObjectPresence};
    use crate::store::{Claims, ObjectId, ObjectStore};

    #[test]
    fn catalog_lists_a_stored_object_and_a_partial_one() {
        let root = tempfile::tempdir().expect("temp dir");
        let store = ObjectStore::open_with_lod(root.path()).expect("store");
        let bytes = b"hello object";
        let claims = Claims::parse(["object-type=note", "name=field-note"]).expect("claims");
        let id = store
            .import_reader(Cursor::new(bytes), bytes.len() as u64, &claims)
            .expect("import");
        let partial =
            ObjectId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .expect("partial id");
        store.note_object_length(&partial, 99).expect("note");

        let data_dir = root.path().join("data");
        let entries = read_catalog(&data_dir).expect("catalog");
        assert_eq!(entries.len(), 2);

        let stored = entries
            .iter()
            .find(|entry| entry.id == id)
            .expect("stored object");
        assert_eq!(stored.presence, ObjectPresence::Complete);
        assert_eq!(stored.manifest_length, Some(bytes.len() as u64));
        assert_eq!(stored.data_length, Some(bytes.len() as u64));
        assert_eq!(
            stored.claims,
            vec![
                ("object-type".to_string(), "note".to_string()),
                ("name".to_string(), "field-note".to_string()),
            ]
        );
        assert!(stored.has_loa_envelope);
        assert!(!stored.has_transfer_manifest);

        let waiting = entries
            .iter()
            .find(|entry| entry.id == partial)
            .expect("partial object");
        assert_eq!(waiting.presence, ObjectPresence::Partial);
        assert_eq!(waiting.manifest_length, Some(99));
        assert_eq!(waiting.data_length, None);

        let prefix = read_data_prefix(&data_dir, &id, 5)
            .expect("prefix")
            .expect("bytes");
        assert_eq!(prefix, b"hello");
        let record = read_object_record(&data_dir, &id).expect("record");
        let manifest = record.manifest.expect("manifest");
        assert!(manifest.contains(id.as_str()));
        assert!(manifest.contains("length 12"));
        assert!(record.loa_envelope.is_some());
        assert!(record.transfer_manifest.is_none());
    }

    #[test]
    fn locate_store_reads_a_stack_config_directory() {
        let root = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            root.path().join("config"),
            "\
[object-services]
  [[object-service]]
    object-store-directory = store

  [[object-transfer]]
    object-transfer-directory = transfer
",
        )
        .expect("config");
        let located = locate_store(root.path()).expect("locate");
        assert_eq!(located.data_dir(), root.path().join("store").join("data"));
    }

    #[test]
    fn locate_store_accepts_a_store_root_and_its_data_directory() {
        let root = tempfile::tempdir().expect("temp dir");
        let _store = ObjectStore::open_with_lod(root.path()).expect("store");
        let root_location = locate_store(root.path()).expect("root");
        assert_eq!(root_location.data_dir(), root.path().join("data"));
        let data_location = locate_store(&root.path().join("data")).expect("data");
        assert_eq!(data_location.data_dir(), root.path().join("data"));
    }

    #[test]
    fn locate_store_rejects_an_unrelated_directory() {
        let root = tempfile::tempdir().expect("temp dir");
        let error = locate_store(root.path()).expect_err("reject");
        let message = error.to_string();
        assert!(message.contains("not a stack config directory"));
    }
}
