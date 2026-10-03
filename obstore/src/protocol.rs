use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

pub use crate::ipc::LocalListener;

use sha2::{Digest, Sha256};

use crate::store::{hex_encode, Claims, ObjectId, ObjectStore};

const MAGIC: &[u8; 4] = b"OBST";
const VERSION: u8 = 2;
const OP_IMPORT: u8 = 1;
const OP_FETCH: u8 = 2;
const OP_TRANSFER: u8 = 3;
const OP_RELEASE: u8 = 4;
const OP_ADMIT: u8 = 5;
const OP_WHO_HAS: u8 = 6;
const OP_CATALOG: u8 = 7;
const OP_PREVIEW: u8 = 8;
const OP_CREATE_LOD: u8 = 9;
const ADMIT_PLAIN: u8 = 0;
const ADMIT_OWNER: u8 = 1;
const ADMIT_ENVELOPE: u8 = 2;
const STATUS_OK: u8 = 0;
const STATUS_ERROR: u8 = 1;
const PREFIX_LEN: usize = 6;
const IMPORT_TAIL_LEN: usize = 8 + 4;
const ADMIT_TAIL_LEN: usize = 1 + 8 + 4;
const MAX_ENVELOPE_LEN: usize = 64 * 1024;

pub fn socket_path(config_dir: &Path) -> std::path::PathBuf {
    config_dir.join("obstore.sock")
}

pub fn bind_socket(socket: &Path) -> io::Result<LocalListener> {
    LocalListener::bind(socket)
}

#[cfg(test)]
pub fn import_file(socket: &Path, file: &Path, claims: &Claims) -> Result<String, io::Error> {
    let mut stream = connect(socket)?;
    let length = File::open(file)?.metadata()?.len();
    let mut input = File::open(file)?;
    let claims_bytes = claims.to_wire();
    let claims_len =
        u32::try_from(claims_bytes.len()).map_err(|_| io::Error::other("claims are too long"))?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_IMPORT])?;
    stream.write_all(&length.to_le_bytes())?;
    stream.write_all(&claims_len.to_le_bytes())?;
    stream.write_all(claims_bytes)?;
    let mut hasher = Sha256::new();
    copy_exact(&mut input, &mut stream, length, Some(&mut hasher))?;
    stream.flush()?;
    let returned = read_object_id(&mut stream)?;
    let sent = hex_encode(&hasher.finalize());
    if returned != sent {
        return Err(io::Error::other(format!(
            "service stored {returned}; the file hashes to {sent}"
        )));
    }
    Ok(returned)
}

/// How an import is admitted. Only `Owner` signs with this stack's LOA key.
pub enum Admission<'a> {
    Plain,
    Owner(&'a Claims),
    Envelope {
        text: &'a str,
        verifying_key: &'a [u8; 32],
    },
}

pub fn import_admission(
    socket: &Path,
    file: &Path,
    admission: Admission<'_>,
) -> Result<String, io::Error> {
    let mut stream = connect(socket)?;
    let length = File::open(file)?.metadata()?.len();
    let mut input = File::open(file)?;
    let (mode, extra) = match admission {
        Admission::Plain => (ADMIT_PLAIN, Vec::new()),
        Admission::Owner(claims) => (ADMIT_OWNER, claims.to_wire().to_vec()),
        Admission::Envelope {
            text,
            verifying_key,
        } => {
            let mut extra = Vec::with_capacity(32 + text.len());
            extra.extend_from_slice(verifying_key);
            extra.extend_from_slice(text.as_bytes());
            (ADMIT_ENVELOPE, extra)
        }
    };
    let extra_len =
        u32::try_from(extra.len()).map_err(|_| io::Error::other("import extra is too long"))?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_ADMIT])?;
    stream.write_all(&[mode])?;
    stream.write_all(&length.to_le_bytes())?;
    stream.write_all(&extra_len.to_le_bytes())?;
    stream.write_all(&extra)?;
    let mut hasher = Sha256::new();
    copy_exact(&mut input, &mut stream, length, Some(&mut hasher))?;
    stream.flush()?;
    let returned = read_object_id(&mut stream)?;
    let sent = hex_encode(&hasher.finalize());
    if returned != sent {
        return Err(io::Error::other(format!(
            "service stored {returned}; the file hashes to {sent}"
        )));
    }
    Ok(returned)
}

pub fn fetch_object(socket: &Path, object_id: &str, destination: &Path) -> Result<(), io::Error> {
    let id = ObjectId::parse(object_id).map_err(|error| io::Error::other(error.to_string()))?;
    let mut stream = connect(socket)?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_FETCH])?;
    stream.write_all(id.as_str().as_bytes())?;
    stream.flush()?;
    match read_status(&mut stream)? {
        STATUS_OK => {}
        STATUS_ERROR => return Err(read_error_message(&mut stream)),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "object store service sent an unrecognized status",
            ));
        }
    }
    let mut length_bytes = [0_u8; 8];
    stream.read_exact(&mut length_bytes)?;
    let length = u64::from_le_bytes(length_bytes);
    if let Some(parent) = destination.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let temp_path = temporary_fetch_path(destination);
    let mut temp = File::create(&temp_path)?;
    let cleanup = TempFile(temp_path.clone());
    let mut hasher = Sha256::new();
    let mut remaining = length;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = stream.read(&mut buffer[..chunk])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "object ended after {} bytes; expected {length}",
                    length - remaining
                ),
            ));
        }
        hasher.update(&buffer[..read]);
        temp.write_all(&buffer[..read])?;
        remaining -= u64::try_from(read).unwrap_or(0);
    }
    temp.sync_all()?;
    drop(temp);
    let fetched = hex_encode(&hasher.finalize());
    if fetched != id.as_str() {
        return Err(io::Error::other(format!(
            "fetched bytes hash to {fetched}, not {}",
            id.as_str()
        )));
    }
    fs::rename(&temp_path, destination)?;
    cleanup.keep();
    Ok(())
}

fn bound_remote_wait(stream: &crate::ipc::LocalStream) -> io::Result<()> {
    let timeout = std::time::Duration::from_secs(70);
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(())
}

fn clarify_remote_timeout(error: io::Error) -> io::Error {
    if error.kind() == ErrorKind::TimedOut || error.kind() == ErrorKind::WouldBlock {
        io::Error::new(
            ErrorKind::TimedOut,
            "timed out waiting for the remote object service",
        )
    } else {
        error
    }
}

fn connect(socket: &Path) -> io::Result<crate::ipc::LocalStream> {
    crate::ipc::LocalStream::connect(socket).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound
            || error.kind() == io::ErrorKind::ConnectionRefused
        {
            io::Error::new(
                error.kind(),
                format!("object service is not listening on {}", socket.display()),
            )
        } else {
            error
        }
    })
}

struct TempFile(PathBuf);

impl TempFile {
    fn keep(mut self) {
        self.0.clear();
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.0.as_os_str().is_empty() {
            return;
        }
        let _ = fs::remove_file(&self.0);
    }
}

fn temporary_fetch_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("object");
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(".{name}.obstore-fetch"))
}

pub fn request_create_lod(socket: &Path) -> Result<String, io::Error> {
    let mut stream = connect(socket)?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_CREATE_LOD])?;
    stream.flush()?;
    match read_status(&mut stream)? {
        STATUS_OK => {
            let mut public = [0_u8; 32];
            stream.read_exact(&mut public)?;
            Ok(hex_encode(&public))
        }
        STATUS_ERROR => Err(read_error_message(&mut stream)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "object store service sent an unrecognized status",
        )),
    }
}

fn serve_create_lod<S: Read + Write>(store: &ObjectStore, stream: &mut S) -> io::Result<()> {
    match store.create_lod() {
        Ok(public) => {
            stream.write_all(MAGIC)?;
            stream.write_all(&[VERSION, STATUS_OK])?;
            stream.write_all(&public)?;
            stream.flush()
        }
        Err(error) => write_error(stream, &error.to_string()),
    }
}

pub fn request_who_has(socket: &Path, object_id: &str, index: u32) -> Result<String, io::Error> {
    let id = ObjectId::parse(object_id).map_err(|error| io::Error::other(error.to_string()))?;
    let mut stream = connect(socket)?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_WHO_HAS])?;
    stream.write_all(id.as_str().as_bytes())?;
    stream.write_all(&index.to_le_bytes())?;
    stream.flush()?;
    match read_status(&mut stream)? {
        STATUS_OK => {
            let mut address = [0_u8; 32];
            stream.read_exact(&mut address)?;
            let address = std::str::from_utf8(&address)
                .map_err(|_| io::Error::other("holder address is not utf-8"))?;
            Ok(address.to_string())
        }
        STATUS_ERROR => Err(read_error_message(&mut stream)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "object store service sent an unrecognized status",
        )),
    }
}

pub fn request_release(socket: &Path, object_id: &str, hops: u8) -> Result<(), io::Error> {
    let id = ObjectId::parse(object_id).map_err(|error| io::Error::other(error.to_string()))?;
    let mut stream = connect(socket)?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_RELEASE])?;
    stream.write_all(id.as_str().as_bytes())?;
    stream.write_all(&[hops])?;
    stream.flush()?;
    match read_status(&mut stream)? {
        STATUS_OK => Ok(()),
        STATUS_ERROR => Err(read_error_message(&mut stream)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "object store service sent an unrecognized status",
        )),
    }
}

pub fn request_transfer(
    socket: &Path,
    object_id: &str,
    destination: &str,
) -> Result<(), io::Error> {
    let id = ObjectId::parse(object_id).map_err(|error| io::Error::other(error.to_string()))?;
    let destination = crate::transfer::Destination::parse(destination)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut stream = connect(socket)?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_TRANSFER])?;
    stream.write_all(id.as_str().as_bytes())?;
    stream.write_all(destination.as_bytes())?;
    stream.flush()?;
    match read_status(&mut stream)? {
        STATUS_OK => Ok(()),
        STATUS_ERROR => Err(read_error_message(&mut stream)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "object store service sent an unrecognized status",
        )),
    }
}

pub fn request_catalog(
    socket: &Path,
    destination: &str,
) -> Result<Vec<crate::store::PublishedObject>, io::Error> {
    request_catalog_inner(socket, destination).map_err(clarify_remote_timeout)
}

fn request_catalog_inner(
    socket: &Path,
    destination: &str,
) -> Result<Vec<crate::store::PublishedObject>, io::Error> {
    let destination = crate::transfer::Destination::parse(destination)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut stream = connect(socket)?;
    bound_remote_wait(&stream)?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_CATALOG])?;
    stream.write_all(destination.as_bytes())?;
    stream.flush()?;
    match read_status(&mut stream)? {
        STATUS_OK => read_catalog_records(&mut stream),
        STATUS_ERROR => Err(read_error_message(&mut stream)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "object store service sent an unrecognized status",
        )),
    }
}

pub fn request_preview(
    socket: &Path,
    destination: &str,
    object_id: &str,
) -> Result<(u64, Vec<u8>), io::Error> {
    request_preview_inner(socket, destination, object_id).map_err(clarify_remote_timeout)
}

fn request_preview_inner(
    socket: &Path,
    destination: &str,
    object_id: &str,
) -> Result<(u64, Vec<u8>), io::Error> {
    let destination = crate::transfer::Destination::parse(destination)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let id = ObjectId::parse(object_id).map_err(|error| io::Error::other(error.to_string()))?;
    let mut stream = connect(socket)?;
    bound_remote_wait(&stream)?;
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, OP_PREVIEW])?;
    stream.write_all(destination.as_bytes())?;
    stream.write_all(id.as_str().as_bytes())?;
    stream.flush()?;
    match read_status(&mut stream)? {
        STATUS_OK => {
            let mut length_bytes = [0_u8; 8];
            stream.read_exact(&mut length_bytes)?;
            let length = u64::from_le_bytes(length_bytes);
            let piece = read_counted_blob(&mut stream, crate::store::PIECE_SIZE)?;
            Ok((length, piece))
        }
        STATUS_ERROR => Err(read_error_message(&mut stream)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "object store service sent an unrecognized status",
        )),
    }
}

fn read_catalog_records(stream: &mut impl Read) -> io::Result<Vec<crate::store::PublishedObject>> {
    let mut count_bytes = [0_u8; 4];
    stream.read_exact(&mut count_bytes)?;
    let count = usize::try_from(u32::from_le_bytes(count_bytes)).unwrap_or(usize::MAX);
    if count > 4096 {
        return Err(io::Error::other("catalog is too large"));
    }
    let mut objects = Vec::with_capacity(count);
    for _ in 0..count {
        let mut length_bytes = [0_u8; 8];
        stream.read_exact(&mut length_bytes)?;
        let length = u64::from_le_bytes(length_bytes);
        let envelope_bytes = read_counted_blob(stream, 64 * 1024 + 512)?;
        let envelope = String::from_utf8(envelope_bytes)
            .map_err(|_| io::Error::other("LOA envelope is not utf-8"))?;
        let id = envelope
            .lines()
            .find_map(|line| line.strip_prefix("object-id "))
            .ok_or_else(|| io::Error::other("LOA envelope has no object id"))?;
        let id = ObjectId::parse(id).map_err(|error| io::Error::other(error.to_string()))?;
        objects.push(crate::store::PublishedObject {
            id,
            length,
            envelope,
        });
    }
    Ok(objects)
}

fn read_counted_blob(stream: &mut impl Read, max: usize) -> io::Result<Vec<u8>> {
    let mut size = [0_u8; 4];
    stream.read_exact(&mut size)?;
    let size = usize::try_from(u32::from_le_bytes(size)).unwrap_or(usize::MAX);
    if size > max {
        return Err(io::Error::other(
            "object store service sent a message that is too long",
        ));
    }
    let mut bytes = vec![0_u8; size];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub fn serve_connection<S: Read + Write>(
    store: &ObjectStore,
    transfer: Option<&crate::transfer::ObjectTransfer>,
    stream: &mut S,
) -> io::Result<()> {
    let mut prefix = [0_u8; PREFIX_LEN];
    if let Err(error) = stream.read_exact(&mut prefix) {
        return write_error(stream, &error.to_string());
    }
    if &prefix[0..4] != MAGIC {
        return write_error(stream, "unrecognized object store message");
    }
    if prefix[4] != VERSION {
        return write_error(stream, "unsupported object store protocol version");
    }
    match prefix[5] {
        OP_IMPORT => serve_import(store, stream),
        OP_ADMIT => serve_admit(store, stream),
        OP_FETCH => serve_fetch(store, stream),
        OP_TRANSFER => serve_transfer(store, transfer, stream),
        OP_RELEASE => serve_release(store, transfer, stream),
        OP_WHO_HAS => serve_who_has(store, transfer, stream),
        OP_CATALOG => serve_remote_catalog(transfer, stream),
        OP_PREVIEW => serve_remote_preview(transfer, stream),
        OP_CREATE_LOD => serve_create_lod(store, stream),
        _ => write_error(stream, "unsupported object store operation"),
    }
}

fn serve_remote_catalog<S: Read + Write>(
    transfer: Option<&crate::transfer::ObjectTransfer>,
    stream: &mut S,
) -> io::Result<()> {
    let Some(transfer) = transfer else {
        return write_error(stream, "object transfer is not available");
    };
    let mut destination = [0_u8; 16];
    if let Err(error) = stream.read_exact(&mut destination) {
        return write_error(stream, &error.to_string());
    }
    let destination = crate::store::hex_encode(&destination);
    match crate::transfer::fetch_catalog(transfer, &destination) {
        Ok(objects) => {
            stream.write_all(MAGIC)?;
            stream.write_all(&[VERSION, STATUS_OK])?;
            let count = u32::try_from(objects.len()).unwrap_or(u32::MAX);
            stream.write_all(&count.to_le_bytes())?;
            for object in objects {
                stream.write_all(&object.length.to_le_bytes())?;
                let bytes = object.envelope.as_bytes();
                let size = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                stream.write_all(&size.to_le_bytes())?;
                stream.write_all(bytes)?;
            }
            stream.flush()
        }
        Err(error) => write_error(stream, &error.to_string()),
    }
}

fn serve_remote_preview<S: Read + Write>(
    transfer: Option<&crate::transfer::ObjectTransfer>,
    stream: &mut S,
) -> io::Result<()> {
    let Some(transfer) = transfer else {
        return write_error(stream, "object transfer is not available");
    };
    let mut destination = [0_u8; 16];
    if let Err(error) = stream.read_exact(&mut destination) {
        return write_error(stream, &error.to_string());
    }
    let mut id_bytes = [0_u8; 64];
    if let Err(error) = stream.read_exact(&mut id_bytes) {
        return write_error(stream, &error.to_string());
    }
    let destination = crate::store::hex_encode(&destination);
    let id_text = match std::str::from_utf8(&id_bytes) {
        Ok(id) => id,
        Err(_) => return write_error(stream, "object id must be 64 hex characters"),
    };
    match crate::transfer::fetch_preview(transfer, &destination, id_text) {
        Ok((length, piece)) => {
            stream.write_all(MAGIC)?;
            stream.write_all(&[VERSION, STATUS_OK])?;
            stream.write_all(&length.to_le_bytes())?;
            let size = u32::try_from(piece.len()).unwrap_or(u32::MAX);
            stream.write_all(&size.to_le_bytes())?;
            stream.write_all(&piece)?;
            stream.flush()
        }
        Err(error) => write_error(stream, &error.to_string()),
    }
}

fn serve_who_has<S: Read + Write>(
    store: &ObjectStore,
    transfer: Option<&crate::transfer::ObjectTransfer>,
    stream: &mut S,
) -> io::Result<()> {
    let Some(transfer) = transfer else {
        return write_error(stream, "object transfer is not available");
    };
    let mut id_bytes = [0_u8; 64];
    if let Err(error) = stream.read_exact(&mut id_bytes) {
        return write_error(stream, &error.to_string());
    }
    let mut index_bytes = [0_u8; 4];
    if let Err(error) = stream.read_exact(&mut index_bytes) {
        return write_error(stream, &error.to_string());
    }
    let id_text = match std::str::from_utf8(&id_bytes) {
        Ok(id) => id,
        Err(_) => return write_error(stream, "object id must be 64 hex characters"),
    };
    match crate::transfer::seek_piece(store, transfer, id_text, u32::from_le_bytes(index_bytes)) {
        Ok(holder) => {
            stream.write_all(MAGIC)?;
            stream.write_all(&[VERSION, STATUS_OK])?;
            stream.write_all(holder.as_hex().as_bytes())?;
            stream.flush()
        }
        Err(error) => write_error(stream, &error.to_string()),
    }
}

fn serve_release<S: Read + Write>(
    store: &ObjectStore,
    transfer: Option<&crate::transfer::ObjectTransfer>,
    stream: &mut S,
) -> io::Result<()> {
    let Some(transfer) = transfer else {
        return write_error(stream, "object transfer is not available");
    };
    let mut id_bytes = [0_u8; 64];
    if let Err(error) = stream.read_exact(&mut id_bytes) {
        return write_error(stream, &error.to_string());
    }
    let mut hops = [0_u8; 1];
    if let Err(error) = stream.read_exact(&mut hops) {
        return write_error(stream, &error.to_string());
    }
    let id_text = match std::str::from_utf8(&id_bytes) {
        Ok(id) => id,
        Err(_) => return write_error(stream, "object id must be 64 hex characters"),
    };
    match crate::transfer::announce_release(store, transfer, id_text, hops[0]) {
        Ok(()) => {
            stream.write_all(MAGIC)?;
            stream.write_all(&[VERSION, STATUS_OK])?;
            stream.flush()
        }
        Err(error) => write_error(stream, &error.to_string()),
    }
}

fn serve_transfer<S: Read + Write>(
    store: &ObjectStore,
    transfer: Option<&crate::transfer::ObjectTransfer>,
    stream: &mut S,
) -> io::Result<()> {
    let Some(transfer) = transfer else {
        return write_error(stream, "object transfer is not available");
    };
    let mut id_bytes = [0_u8; 64];
    if let Err(error) = stream.read_exact(&mut id_bytes) {
        return write_error(stream, &error.to_string());
    }
    let mut destination = [0_u8; 16];
    if let Err(error) = stream.read_exact(&mut destination) {
        return write_error(stream, &error.to_string());
    }
    let id_text = match std::str::from_utf8(&id_bytes) {
        Ok(id) => id,
        Err(_) => return write_error(stream, "object id must be 64 hex characters"),
    };
    let destination = crate::store::hex_encode(&destination);
    match crate::transfer::offer(store, transfer, id_text, &destination) {
        Ok(()) => {
            stream.write_all(MAGIC)?;
            stream.write_all(&[VERSION, STATUS_OK])?;
            stream.flush()
        }
        Err(error) => write_error(stream, &error.to_string()),
    }
}

fn serve_admit<S: Read + Write>(store: &ObjectStore, stream: &mut S) -> io::Result<()> {
    let mut tail = [0_u8; ADMIT_TAIL_LEN];
    if let Err(error) = stream.read_exact(&mut tail) {
        return write_error(stream, &error.to_string());
    }
    let mode = tail[0];
    let length = u64::from_le_bytes(tail[1..9].try_into().expect("length"));
    let extra_len = u32::from_le_bytes(tail[9..13].try_into().expect("extra"));
    let extra_cap = match mode {
        ADMIT_OWNER => Claims::MAX_WIRE_LEN,
        ADMIT_ENVELOPE => 32 + MAX_ENVELOPE_LEN,
        ADMIT_PLAIN => 0,
        _ => return write_error(stream, "unsupported import admission"),
    };
    if usize::try_from(extra_len).unwrap_or(usize::MAX) > extra_cap {
        return write_error(stream, "import extra is too long");
    }
    let mut extra = vec![0_u8; usize::try_from(extra_len).unwrap_or(0)];
    if let Err(error) = stream.read_exact(&mut extra) {
        return write_error(stream, &error.to_string());
    }
    let reader = ReadExact::new(&mut *stream, length);
    let admitted = match mode {
        ADMIT_PLAIN => {
            if !extra.is_empty() {
                return write_error(stream, "a plain import has no envelope");
            }
            store.import_plain(reader, length)
        }
        ADMIT_OWNER => {
            let claims = match Claims::from_wire(&extra) {
                Ok(claims) => claims,
                Err(error) => return write_error(stream, &error.to_string()),
            };
            store.import_reader(reader, length, &claims)
        }
        ADMIT_ENVELOPE => {
            if extra.len() < 32 {
                return write_error(stream, "an existing envelope needs its authority key");
            }
            let mut verifying_key = [0_u8; 32];
            verifying_key.copy_from_slice(&extra[..32]);
            let text = match std::str::from_utf8(&extra[32..]) {
                Ok(text) => text,
                Err(_) => return write_error(stream, "object envelope must be utf-8"),
            };
            store.import_envelope(reader, length, text, &verifying_key)
        }
        _ => return write_error(stream, "unsupported import admission"),
    };
    match admitted {
        Ok(id) => write_import_ok(stream, id.as_str()),
        Err(error) => write_error(stream, &error.to_string()),
    }
}

fn write_import_ok<S: Write>(stream: &mut S, id: &str) -> io::Result<()> {
    let mut response = Vec::with_capacity(6 + id.len());
    response.extend_from_slice(MAGIC);
    response.push(VERSION);
    response.push(STATUS_OK);
    response.extend_from_slice(id.as_bytes());
    stream.write_all(&response)?;
    stream.flush()
}

fn serve_import<S: Read + Write>(store: &ObjectStore, stream: &mut S) -> io::Result<()> {
    let mut tail = [0_u8; IMPORT_TAIL_LEN];
    if let Err(error) = stream.read_exact(&mut tail) {
        return write_error(stream, &error.to_string());
    }
    let mut length_bytes = [0_u8; 8];
    length_bytes.copy_from_slice(&tail[0..8]);
    let length = u64::from_le_bytes(length_bytes);
    let mut claims_len_bytes = [0_u8; 4];
    claims_len_bytes.copy_from_slice(&tail[8..12]);
    let claims_len = u32::from_le_bytes(claims_len_bytes);
    if usize::try_from(claims_len).unwrap_or(usize::MAX) > Claims::MAX_WIRE_LEN {
        return write_error(stream, "claims are too long");
    }
    let mut claims_bytes = vec![0_u8; usize::try_from(claims_len).unwrap_or(0)];
    if let Err(error) = stream.read_exact(&mut claims_bytes) {
        return write_error(stream, &error.to_string());
    }
    let claims = match Claims::from_wire(&claims_bytes) {
        Ok(claims) => claims,
        Err(error) => return write_error(stream, &error.to_string()),
    };
    match store.import_reader(ReadExact::new(&mut *stream, length), length, &claims) {
        Ok(id) => write_import_ok(stream, id.as_str()),
        Err(error) => write_error(stream, &error.to_string()),
    }
}

fn serve_fetch<S: Read + Write>(store: &ObjectStore, stream: &mut S) -> io::Result<()> {
    let mut id_bytes = [0_u8; 64];
    if let Err(error) = stream.read_exact(&mut id_bytes) {
        return write_error(stream, &error.to_string());
    }
    let id_text = match std::str::from_utf8(&id_bytes) {
        Ok(id) => id,
        Err(_) => return write_error(stream, "object id must be 64 hex characters"),
    };
    let id = match ObjectId::parse(id_text) {
        Ok(id) => id,
        Err(error) => return write_error(stream, &error.to_string()),
    };
    let (mut file, length) = match store.open_data(&id) {
        Ok(opened) => opened,
        Err(error) => return write_error(stream, &error.to_string()),
    };
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, STATUS_OK])?;
    stream.write_all(&length.to_le_bytes())?;
    copy_exact(&mut file, stream, length, None)?;
    stream.flush()
}

fn copy_exact(
    reader: &mut impl Read,
    writer: &mut impl Write,
    length: u64,
    mut hasher: Option<&mut Sha256>,
) -> io::Result<()> {
    let mut remaining = length;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = reader.read(&mut buffer[..chunk])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "file ended after {} bytes; expected {length}",
                    length - remaining
                ),
            ));
        }
        if let Some(hasher) = hasher.as_mut() {
            hasher.update(&buffer[..read]);
        }
        writer.write_all(&buffer[..read])?;
        remaining -= u64::try_from(read).unwrap_or(0);
    }
    Ok(())
}

fn read_object_id(stream: &mut impl Read) -> io::Result<String> {
    match read_status(stream)? {
        STATUS_OK => {
            let mut id = [0_u8; 64];
            stream.read_exact(&mut id)?;
            let id = std::str::from_utf8(&id)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "object id is not hex"))?;
            Ok(id.to_string())
        }
        STATUS_ERROR => Err(read_error_message(stream)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "object store service sent an unrecognized status",
        )),
    }
}

fn read_status(stream: &mut impl Read) -> io::Result<u8> {
    let mut prefix = [0_u8; PREFIX_LEN];
    stream.read_exact(&mut prefix)?;
    if &prefix[0..4] != MAGIC || prefix[4] != VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "object store service sent an unrecognized reply",
        ));
    }
    Ok(prefix[5])
}

fn read_error_message(stream: &mut impl Read) -> io::Error {
    let mut size = [0_u8; 4];
    if stream.read_exact(&mut size).is_err() {
        return io::Error::other("object store service sent an empty error");
    }
    let size = usize::try_from(u32::from_le_bytes(size)).unwrap_or(0);
    let mut message = vec![0_u8; size];
    if stream.read_exact(&mut message).is_err() {
        return io::Error::other("object store service sent an empty error");
    }
    io::Error::other(String::from_utf8_lossy(&message).to_string())
}

fn write_error(stream: &mut impl Write, message: &str) -> io::Result<()> {
    let message = message.as_bytes();
    let size = u32::try_from(message.len()).unwrap_or(u32::MAX);
    let message = &message[..usize::try_from(size).unwrap_or(message.len())];
    stream.write_all(MAGIC)?;
    stream.write_all(&[VERSION, STATUS_ERROR])?;
    stream.write_all(&size.to_le_bytes())?;
    stream.write_all(message)?;
    stream.flush()
}

struct ReadExact<R> {
    inner: R,
    remaining: u64,
}

impl<R> ReadExact<R> {
    fn new(inner: R, remaining: u64) -> Self {
        Self { inner, remaining }
    }
}

impl<R: Read> Read for ReadExact<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let chunk =
            usize::try_from(self.remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = self.inner.read(&mut buffer[..chunk])?;
        self.remaining -= u64::try_from(read).unwrap_or(0);
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Read, Write};
    use std::thread;

    use ed25519_dalek::Signer;

    use std::io::Cursor;

    use super::{
        fetch_object, import_admission, import_file, serve_connection, Admission, MAGIC, STATUS_OK,
        VERSION,
    };
    use crate::store::{hex_encode, Claims, ObjectStore};

    #[test]
    fn import_streams_the_file_to_the_service() {
        let root = tempfile::tempdir().expect("temp dir");
        let socket = root.path().join("s.sock");
        let listener = crate::ipc::LocalListener::bind(&socket).expect("bind");
        let store_root = root.path().join("store");
        let server = thread::spawn(move || {
            let store = ObjectStore::open_with_lod(&store_root).expect("store");
            let (mut stream, _) = listener.accept().expect("accept");
            serve_connection(&store, None, &mut stream).expect("serve");
            let mut extra = [0_u8; 1];
            let _ = stream.read(&mut extra);
        });

        let file_dir = tempfile::tempdir().expect("file dir");
        let file = file_dir.path().join("sample.txt");
        fs::write(&file, b"abc").expect("sample");
        let id = import_file(&socket, &file, &Claims::none()).expect("import");
        server.join().expect("server thread");
        assert_eq!(
            id,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            fs::read(
                root.path()
                    .join("store")
                    .join("data")
                    .join(&id)
                    .join("data")
            )
            .expect("stored"),
            b"abc"
        );
    }

    #[test]
    fn import_sends_the_claims_list() {
        let root = tempfile::tempdir().expect("temp dir");
        let socket = root.path().join("s.sock");
        let listener = crate::ipc::LocalListener::bind(&socket).expect("bind");
        let store_root = root.path().join("store");
        let server = thread::spawn(move || {
            let store = ObjectStore::open_with_lod(&store_root).expect("store");
            let (mut stream, _) = listener.accept().expect("accept");
            serve_connection(&store, None, &mut stream).expect("serve");
        });
        let file = root.path().join("sample.txt");
        fs::write(&file, b"abc").expect("sample");
        let claims =
            Claims::parse(["board=heltec-v4-r8", "provenance=local-build"]).expect("claims");
        let id = import_file(&socket, &file, &claims).expect("import");
        server.join().expect("server thread");
        let envelope = fs::read_to_string(
            root.path()
                .join("store")
                .join("data")
                .join(id)
                .join("LOA-envelope"),
        )
        .expect("envelope");
        assert!(envelope.starts_with("board heltec-v4-r8\nprovenance local-build\nobject-id "));
    }

    #[test]
    fn admit_plain_stores_no_envelope_and_admit_owner_signs_one() {
        let root = tempfile::tempdir().expect("temp dir");
        let socket = root.path().join("s.sock");
        let listener = crate::ipc::LocalListener::bind(&socket).expect("bind");
        let store_root = root.path().join("store");
        let server = thread::spawn(move || {
            let store = ObjectStore::open_with_lod(&store_root).expect("store");
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept");
                serve_connection(&store, None, &mut stream).expect("serve");
            }
        });
        let file = root.path().join("sample.txt");
        fs::write(&file, b"abc").expect("sample");
        let plain = import_admission(&socket, &file, Admission::Plain).expect("plain");
        assert!(!root
            .path()
            .join("store")
            .join("data")
            .join(&plain)
            .join("LOA-envelope")
            .exists());
        let claims = Claims::parse(["board=heltec-v4-r8"]).expect("claims");
        let owned = import_admission(&socket, &file, Admission::Owner(&claims)).expect("owner");
        assert_eq!(plain, owned);
        let envelope = fs::read_to_string(
            root.path()
                .join("store")
                .join("data")
                .join(&owned)
                .join("LOA-envelope"),
        )
        .expect("envelope");
        assert!(envelope.starts_with("board heltec-v4-r8\nobject-id "));
        server.join().expect("server thread");
    }

    #[test]
    fn admit_envelope_stores_the_foreign_envelope_unchanged() {
        let root = tempfile::tempdir().expect("temp dir");
        let socket = root.path().join("s.sock");
        let listener = crate::ipc::LocalListener::bind(&socket).expect("bind");
        let store_root = root.path().join("store");
        let server = thread::spawn(move || {
            let store = ObjectStore::open_with_lod(&store_root).expect("store");
            let (mut stream, _) = listener.accept().expect("accept");
            serve_connection(&store, None, &mut stream).expect("serve");
        });
        let file = root.path().join("sample.txt");
        fs::write(&file, b"abc").expect("sample");
        let id = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let signing = ed25519_dalek::SigningKey::from_bytes(&[7_u8; 32]);
        let statement = format!("provenance imported\nobject-id {id}\n");
        let signature = signing.sign(statement.as_bytes());
        let envelope = format!(
            "{statement}signature {}\n",
            hex_encode(&signature.to_bytes())
        );
        let key = signing.verifying_key().to_bytes();
        let stored = import_admission(
            &socket,
            &file,
            Admission::Envelope {
                text: &envelope,
                verifying_key: &key,
            },
        )
        .expect("envelope");
        server.join().expect("server thread");
        assert_eq!(stored, id);
        assert_eq!(
            fs::read_to_string(
                root.path()
                    .join("store")
                    .join("data")
                    .join(id)
                    .join("LOA-envelope")
            )
            .expect("stored"),
            envelope
        );
    }

    #[test]
    fn a_missing_service_is_reported() {
        let root = tempfile::tempdir().expect("temp dir");
        let file = root.path().join("sample.txt");
        let mut sample = fs::File::create(&file).expect("sample");
        sample.write_all(b"abc").expect("write");
        let error = import_file(&root.path().join("missing.sock"), &file, &Claims::none())
            .expect_err("connect");
        assert!(error
            .to_string()
            .contains("object service is not listening"));
    }

    #[test]
    fn import_rejects_when_the_service_stores_different_bytes() {
        let root = tempfile::tempdir().expect("temp dir");
        let socket = root.path().join("s.sock");
        let listener = crate::ipc::LocalListener::bind(&socket).expect("bind");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut prefix = [0_u8; 6];
            stream.read_exact(&mut prefix).expect("prefix");
            let mut tail = [0_u8; 12];
            stream.read_exact(&mut tail).expect("tail");
            let length = u64::from_le_bytes(tail[0..8].try_into().expect("length"));
            let claims_len = u32::from_le_bytes(tail[8..12].try_into().expect("claims"));
            let mut rest = vec![0_u8; claims_len as usize + length as usize];
            stream.read_exact(&mut rest).expect("body");
            stream.write_all(MAGIC).expect("magic");
            stream.write_all(&[VERSION, STATUS_OK]).expect("status");
            stream.write_all(&[b'a'; 64]).expect("id");
        });
        let file = root.path().join("sample.txt");
        fs::write(&file, b"abc").expect("sample");
        let error = import_file(&socket, &file, &Claims::none()).expect_err("mismatch");
        server.join().expect("server thread");
        let message = error.to_string();
        assert!(message.contains(
            "service stored aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        assert!(message.contains(
            "the file hashes to ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        ));
    }

    #[test]
    fn fetch_streams_the_object_back() {
        let root = tempfile::tempdir().expect("temp dir");
        let store_root = root.path().join("store");
        let id = {
            let store = ObjectStore::open_with_lod(&store_root).expect("store");
            store
                .import_reader(Cursor::new(b"abc"), 3, &Claims::none())
                .expect("import")
        };
        let socket = root.path().join("s.sock");
        let listener = crate::ipc::LocalListener::bind(&socket).expect("bind");
        let server = thread::spawn(move || {
            let store = ObjectStore::open_with_lod(&store_root).expect("store");
            let (mut stream, _) = listener.accept().expect("accept");
            serve_connection(&store, None, &mut stream).expect("serve");
        });
        let fetched = root.path().join("out.bin");
        fetch_object(&socket, id.as_str(), &fetched).expect("fetch");
        server.join().expect("server thread");
        assert_eq!(fs::read(&fetched).expect("fetched"), b"abc");
    }

    #[test]
    fn fetch_reports_a_missing_object() {
        let root = tempfile::tempdir().expect("temp dir");
        let socket = root.path().join("s.sock");
        let listener = crate::ipc::LocalListener::bind(&socket).expect("bind");
        let store_root = root.path().join("store");
        let server = thread::spawn(move || {
            let store = ObjectStore::open_with_lod(&store_root).expect("store");
            let (mut stream, _) = listener.accept().expect("accept");
            serve_connection(&store, None, &mut stream).expect("serve");
        });
        let fetched = root.path().join("out.bin");
        let error = fetch_object(
            &socket,
            "0000000000000000000000000000000000000000000000000000000000000000",
            &fetched,
        )
        .expect_err("missing");
        server.join().expect("server thread");
        assert!(error.to_string().contains("not in the store"));
        assert!(!fetched.exists());
    }
}
