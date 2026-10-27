//! The journal's own identity and `lxmf.delivery` destination.
//!
//! This is the person's address, not a hopspot's private delivery destination.
//! The public key is what a contact scans. The destination hash is derived from
//! that key and the fixed name `lxmf.delivery`, and it is what a path request names.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use personal_rns::crypto::{sha256, Ed25519Signature};
use personal_rns::identity::in_memory::InMemoryNodeIdentity;
use personal_rns::identity::{
    IdentityHash, IdentitySigner, PrivateIdentityMaterial, PublicIdentityMaterial,
};
use personal_rns::prelude::*;
use personal_rns::routing::announce::derive_single_destination_hash;
use personal_rns::routing::delivery::Delivery;
use personal_rns::routing::links::LinkId;
use personal_rns::routing::request_handlers::RequestPathHash;
#[cfg(any(target_os = "macos", target_os = "ios"))]
use personal_rns::wifi_auto::{apple_service_discovery, AutoWifi};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use personal_rns::wifi_auto::{native_service_discovery, AutoWifi, AutoWifiDevicePolicy};
#[cfg(target_os = "android")]
use personal_rns::wifi_auto::{
    native_service_discovery_with_host_lan, AutoWifi, AutoWifiDevicePolicy,
};
use personal_rns::{load_or_create_identity_secret, IdentitySecretFileError};

const DELIVERY_APP: &str = "lxmf";
const DELIVERY_ASPECTS: &[&str] = &["delivery"];
const IDENTITY_FILE: &str = "identity";

pub static PUBLIC_KEY: OnceLock<String> = OnceLock::new();
static RUNNING: OnceLock<RunningJournal> = OnceLock::new();
static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();
static HANDLE: OnceLock<PrnsNodeHandle> = OnceLock::new();
static OURS: OnceLock<IdentityHash> = OnceLock::new();
static OUR_DELIVERY: OnceLock<DestinationHash> = OnceLock::new();
static SIGNING: OnceLock<Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>> = OnceLock::new();
static RATCHET_ROTATED: OnceLock<tokio::sync::mpsc::UnboundedSender<DestinationHash>> =
    OnceLock::new();
static OUR_RATCHETS: Mutex<Vec<personal_rns::crypto::X25519SecretKey>> = Mutex::new(Vec::new());
static MAIL: OnceLock<Mutex<Mail>> = OnceLock::new();

struct Mail {
    peers: HashMap<LinkId, IdentityHash>,
    waiting: HashMap<LinkId, Vec<crate::lxmf::NoteLetter>>,
    waiting_marks: HashMap<LinkId, Vec<crate::lxmf::ReactionLetter>>,
    inbox: Vec<ArrivedNote>,
    marks: Vec<ArrivedReaction>,
    /// Notes whose link receipt has come back.
    acked: Vec<u64>,
    /// Notes that were tried three times and will not send themselves again.
    failed: Vec<u64>,
    /// Notes the mailbox accepted, with the hash of the packed note.
    posted: Vec<(u64, [u8; 32])>,
    /// Pieces of a long note that arrived before the other person was known.
    waiting_parts: HashMap<LinkId, Vec<crate::lxmf::Fragment>>,
    /// Pieces of a long note, joined when every piece has arrived.
    assemblies: HashMap<(String, u64), crate::lxmf::Assembly>,
    /// Pieces of one resource, joined when every piece has arrived.
    resources: HashMap<(LinkId, [u8; 32]), IncomingResource>,
    /// A file name that arrived before its bytes, still owed on this link.
    open_files: Vec<OpenFile>,
    /// File names whose bytes will not arrive. The journal marks those notes failed.
    incoming_failed: Vec<FailedAttachment>,
    /// A name that arrived before we knew who sent it, on a link that then failed.
    doomed_waits: Vec<DoomedWait>,
}

struct OpenFile {
    link_id: LinkId,
    identity: String,
    written_at: f64,
    file_name: String,
}

struct DoomedWait {
    link_id: LinkId,
    written_at: f64,
    file_name: String,
}

/// A file the other person named, whose transfer then failed.
pub struct FailedAttachment {
    pub identity: String,
    pub written_at: f64,
    pub file_name: String,
}

struct IncomingResource {
    total: u64,
    parts: Vec<Option<Vec<u8>>>,
}

/// A note that arrived on a link, still waiting for the contact list.
pub struct ArrivedNote {
    pub identity: String,
    pub words: String,
    pub written_at: f64,
    pub place: Option<crate::model::Place>,
    /// The note was collected from the mailbox, so the sender is still waiting for an acknowledgement.
    pub from_mailbox: bool,
    /// sha256 of the packed note. Set when `from_mailbox` is true.
    pub digest: [u8; 32],
    /// Set when the note carried a file. Empty when it did not.
    pub file_name: String,
    pub file_bytes: Vec<u8>,
}

fn arrived_note(
    identity: String,
    letter: crate::lxmf::NoteLetter,
    from_mailbox: bool,
    digest: [u8; 32],
) -> ArrivedNote {
    let (file_name, file_bytes) = match letter.file {
        Some(file) => (file.name, file.bytes),
        None => (String::new(), Vec::new()),
    };
    ArrivedNote {
        identity,
        words: letter.words,
        written_at: letter.written_at,
        place: letter.place,
        from_mailbox,
        digest,
        file_name,
        file_bytes,
    }
}

/// A reaction that arrived on a link, still waiting for the contact list.
pub struct ArrivedReaction {
    pub identity: String,
    pub written_at: f64,
    pub words: String,
    pub reaction: Option<crate::model::Reaction>,
}

pub struct JournalIdentity {
    secret: Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>,
    public_key_hex: String,
    destination: DestinationHash,
    hash: IdentityHash,
}

#[derive(Debug)]
pub enum SessionError {
    Identity(IdentitySecretFileError),
    RemoteControl(RemoteControlFileIdentityBootstrapError),
    Destination,
    EngineStopped,
    Io(io::Error),
}

pub struct RunningJournal {
    identity: JournalIdentity,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl core::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Identity(error) => write!(formatter, "journal identity: {error}"),
            Self::RemoteControl(error) => write!(formatter, "journal remote control: {error}"),
            Self::Destination => write!(formatter, "journal delivery name is not valid"),
            Self::EngineStopped => write!(formatter, "journal engine stopped before it was ready"),
            Self::Io(error) => write!(formatter, "journal session: {error}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl JournalIdentity {
    pub fn load_or_create(directory: &Path) -> Result<Self, SessionError> {
        std::fs::create_dir_all(directory).map_err(SessionError::Io)?;
        let secret = load_or_create_identity_secret(&directory.join(IDENTITY_FILE))
            .map_err(SessionError::Identity)?;
        Self::from_secret(secret)
    }

    pub fn public_key_hex(&self) -> &str {
        &self.public_key_hex
    }

    pub fn destination(&self) -> DestinationHash {
        self.destination
    }

    fn from_secret(secret: Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>) -> Result<Self, SessionError> {
        let signer = InMemoryNodeIdentity::from_secret_key_bytes(&secret);
        let destination =
            derive_single_destination_hash(&signer.identity_hash(), DELIVERY_APP, DELIVERY_ASPECTS)
                .map_err(|_| SessionError::Destination)?;
        Ok(Self {
            public_key_hex: hex_encode(&signer.public_key_bytes()),
            secret,
            destination,
            hash: signer.identity_hash(),
        })
    }
}

impl RunningJournal {
    pub fn public_key_hex(&self) -> &str {
        self.identity.public_key_hex()
    }

    pub fn start(directory: &Path) -> Result<Self, SessionError> {
        let identity = JournalIdentity::load_or_create(directory)?;
        let secret = identity.secret.clone();
        let destination = identity.destination();
        let _ = OURS.set(identity.hash);
        let _ = OUR_DELIVERY.set(destination);
        let _ = SIGNING.set(identity.secret.clone());
        let home = directory.to_path_buf();
        let remote_directory = home.join("remote-control");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("journal".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = ready_tx.send(Err(SessionError::Io(error)));
                        return;
                    }
                };
                let _ = RUNTIME.set(runtime.handle().clone());
                runtime.block_on(async move {
                    let mut node =
                        match stand_up_live(&remote_directory, &home, secret, on_journal_event) {
                            Ok(node) => node,
                            Err(error) => {
                                let _ = ready_tx.send(Err(error));
                                return;
                            }
                        };
                    let vault_dir = home.join("vault");
                    let vault = personal_rns::identity::vault::FileVault::new(&vault_dir);
                    let restored = node.seed_self_ratchets_from_vault(&vault);
                    delivery_log(&format!(
                        "ratchets restored {} refused {} dropped {}",
                        restored.seeded_count, restored.refused_count, restored.dropped_count
                    ));
                    load_saved_ratchets(&vault, destination);
                    let (rotated_tx, rotated_rx) = tokio::sync::mpsc::unbounded_channel();
                    let _ = RATCHET_ROTATED.set(rotated_tx);
                    let flush_handle = node.handle();
                    tokio::spawn(keep_ratchets(flush_handle, vault_dir, rotated_rx));
                    let _ = HANDLE.set(node.handle());
                    let announcer = node.handle();
                    let _ = ready_tx.send(Ok(()));
                    let announce = tokio::spawn(async move {
                        // Give the interfaces time to come up, then announce on all of
                        // them. After that, announce once an hour.
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let mut ticker = tokio::time::interval(Duration::from_secs(60 * 60));
                        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        loop {
                            ticker.tick().await;
                            if announcer
                                .issue(PrnsCommand::AnnounceNow(AnnounceNow {
                                    destination,
                                    target: AnnounceTarget::AllInterfaces,
                                    app_data: AnnounceAppData::Registered,
                                }))
                                .is_none()
                            {
                                return;
                            }
                        }
                    });
                    let collector = tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(8)).await;
                        let mut ticker = tokio::time::interval(Duration::from_secs(25));
                        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        loop {
                            ticker.tick().await;
                            collect_mailbox().await;
                        }
                    });
                    let _ = node
                        .run_until(async move {
                            let _ = shutdown_rx.await;
                        })
                        .await;
                    announce.abort();
                    collector.abort();
                });
            })
            .map_err(SessionError::Io)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                identity,
                shutdown: Some(shutdown_tx),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(SessionError::EngineStopped)
            }
        }
    }
}

impl Drop for RunningJournal {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn start_default() -> Result<(), SessionError> {
    let running = RunningJournal::start(&default_directory())?;
    let _ = PUBLIC_KEY.set(running.public_key_hex().to_string());
    let _ = RUNNING.set(running);
    prepare_contacts();
    Ok(())
}

struct OpeningBook {
    people: Vec<crate::model::Person>,
    next_id: u64,
    writable: bool,
}

fn book_slot() -> &'static Mutex<OpeningBook> {
    static BOOK: OnceLock<Mutex<OpeningBook>> = OnceLock::new();
    BOOK.get_or_init(|| {
        Mutex::new(OpeningBook {
            people: Vec::new(),
            next_id: 1,
            writable: false,
        })
    })
}

/// Reads the saved contacts. A missing file is an empty book that can be written.
fn prepare_contacts() {
    let path = default_directory().join("contacts");
    let mut book = book_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match std::fs::read_to_string(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            book.people.clear();
            book.next_id = 1;
            book.writable = true;
        }
        Ok(text) => match crate::model::decode_contacts(&text) {
            Some(mut people) => {
                crate::model::note_ids_from(&people);
                mark_files_already_kept(&mut people);
                book.next_id = crate::model::next_person_id(&people);
                book.people = people;
                book.writable = true;
            }
            None => {
                book.people.clear();
                book.next_id = 1;
                book.writable = false;
            }
        },
        Err(_) => {
            book.people.clear();
            book.next_id = 1;
            book.writable = false;
        }
    }
    let saved = book.people.clone();
    drop(book);
    remember_digests(&saved);
    queue_unsent(&saved);
}

/// A file kept beside the book was already copied into Downloads by an older build.
fn mark_files_already_kept(people: &mut [crate::model::Person]) {
    for person in people {
        for note in &mut person.notes {
            if note.mine || note.downloaded || note.file_name.is_none() {
                continue;
            }
            if kept_file(note.id).is_some() {
                note.downloaded = true;
                note.failed = false;
            }
        }
    }
}

fn remember_digests(people: &[crate::model::Person]) {
    let mut sent = sent_notes();
    for person in people {
        for note in &person.notes {
            if let Some(digest) = note.digest {
                sent.insert(note.id, digest);
            }
        }
    }
}

fn queue_unsent(people: &[crate::model::Person]) {
    for person in people {
        for note in &person.notes {
            if note.mine
                && !note.settled
                && !note.failed
                && !note.posted
                && note.kind == crate::model::NoteKind::Text
            {
                let stored = note.file_name.as_ref().and_then(|_| kept_file(note.id));
                let file = match (&note.file_name, &stored) {
                    (Some(name), Some(bytes)) if !bytes.is_empty() => {
                        Some((name.as_str(), bytes.as_slice()))
                    }
                    _ => None,
                };
                if note.words.trim().is_empty() && file.is_none() {
                    continue;
                }
                send_note(
                    &person.identity,
                    &note.words,
                    note.id,
                    f64::from_bits(note.written_at),
                    note.place,
                    file,
                );
            }
        }
    }
}

pub fn opening_contacts() -> Vec<crate::model::Person> {
    book_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .people
        .clone()
}

pub fn opening_next_id() -> u64 {
    book_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .next_id
}

/// Writes the contact book next to the identity. A book that could not be read is left untouched.
pub fn save_contacts(people: &[crate::model::Person]) {
    let mut book = book_slot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !book.writable {
        return;
    }
    book.people = people.to_vec();
    book.next_id = crate::model::next_person_id(people);
    let directory = default_directory();
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let temporary = directory.join("contacts.tmp");
    let path = directory.join("contacts");
    if write_private(&temporary, crate::model::encode_contacts(people).as_bytes()).is_err() {
        return;
    }
    let _ = std::fs::rename(&temporary, &path);
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn default_directory() -> PathBuf {
    if let Ok(directory) = std::env::var("JOURNAL_HOME") {
        return PathBuf::from(directory);
    }
    #[cfg(target_os = "android")]
    {
        return android_files_dir().unwrap_or_else(|| std::env::temp_dir().join("journal"));
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join("Library/Application Support/Journal")
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var("APPDATA")
            .map(|app_data| PathBuf::from(app_data).join("Journal"))
            .unwrap_or_else(|_| PathBuf::from(".local/share/journal"))
    }
    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "windows",
        target_os = "android"
    )))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".local/share/journal")
    }
}

/// Writes the bytes of a note's file beside the contact book, so a retry can send them again.
pub fn keep_file(note_id: u64, bytes: &[u8]) {
    if note_id == 0 || bytes.is_empty() {
        return;
    }
    let dir = default_directory().join("files");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = write_private(&dir.join(note_id.to_string()), bytes);
}

/// Copies a saved file into the user's Downloads folder, keeping its name.
/// A file already there with the same bytes is left in place. A different file
/// of that name gets a numbered copy.
pub fn publish_download(note_id: u64, name: &str) -> bool {
    let name = crate::model::file_name_of(name);
    #[cfg(target_os = "android")]
    {
        save_android_download(note_id, &name).is_some()
    }
    #[cfg(not(target_os = "android"))]
    {
        download_copy(note_id, &name).is_some()
    }
}

/// Opens the Downloads copy of a saved file, writing it there first if needed.
pub fn show_kept_file(note_id: u64, name: &str) {
    let name = crate::model::file_name_of(name);
    #[cfg(target_os = "android")]
    {
        let Some(saved) = save_android_download(note_id, &name) else {
            return;
        };
        let _ = with_android_context(|env, context| {
            let class = env.get_object_class(context)?;
            let text = env.new_string(&saved)?;
            env.call_static_method(
                &class,
                "openDownload",
                "(Ljava/lang/String;)V",
                &[jni::objects::JValue::Object(&text)],
            )?;
            Ok(())
        });
    }
    #[cfg(not(target_os = "android"))]
    {
        let Some(path) = download_copy(note_id, &name) else {
            return;
        };
        open_download(&path);
    }
}

#[cfg(not(target_os = "android"))]
fn downloads_dir() -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            return std::path::PathBuf::from(profile).join("Downloads");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home).join("Downloads")
}

#[cfg(not(target_os = "android"))]
fn download_copy(note_id: u64, name: &str) -> Option<std::path::PathBuf> {
    let bytes = kept_file(note_id)?;
    let dir = downloads_dir();
    std::fs::create_dir_all(&dir).ok()?;
    for n in 1..1000 {
        let candidate = dir.join(crate::model::numbered_file_name(name, n));
        if !candidate.exists() {
            write_private(&candidate, &bytes).ok()?;
            return Some(candidate);
        }
        let same_size = std::fs::metadata(&candidate)
            .ok()
            .is_some_and(|meta| meta.len() == bytes.len() as u64);
        if same_size && std::fs::read(&candidate).ok().as_deref() == Some(bytes.as_slice()) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(not(target_os = "android"))]
fn open_download(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &path.to_string_lossy()])
            .spawn();
    }
    #[cfg(all(
        not(any(target_os = "macos", target_os = "windows")),
        not(target_arch = "wasm32")
    ))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
    }
}

#[cfg(target_os = "android")]
fn save_android_download(note_id: u64, name: &str) -> Option<String> {
    let source = default_directory().join("files").join(note_id.to_string());
    with_android_context(|env, context| {
        let class = env.get_object_class(context)?;
        let source = env.new_string(source.to_string_lossy())?;
        let name = env.new_string(name)?;
        let value = env
            .call_static_method(
                &class,
                "saveDownload",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &[
                    jni::objects::JValue::Object(&source),
                    jni::objects::JValue::Object(&name),
                ],
            )?
            .l()?;
        if value.is_null() {
            return Ok(None);
        }
        let text: String = env.get_string(&jni::objects::JString::from(value))?.into();
        Ok(Some(text).filter(|text| !text.is_empty()))
    })
    .ok()
    .flatten()
}

/// The bytes saved for a note, when they are still on disk.
pub fn kept_file(note_id: u64) -> Option<Vec<u8>> {
    if note_id == 0 {
        return None;
    }
    let bytes = std::fs::read(default_directory().join("files").join(note_id.to_string())).ok()?;
    if bytes.is_empty() {
        None
    } else {
        Some(bytes)
    }
}

#[cfg(target_os = "android")]
pub struct ClipboardContents {
    pub text: Option<String>,
    pub image: Option<Vec<u8>>,
}

#[cfg(target_os = "android")]
pub fn read_clipboard() -> Option<ClipboardContents> {
    with_process(|env, context| {
        let clipboard = system_service(env, context, "clipboard")?;
        if clipboard.is_null() {
            return Ok(ClipboardContents {
                text: None,
                image: None,
            });
        }
        let clip = env
            .call_method(
                &clipboard,
                "getPrimaryClip",
                "()Landroid/content/ClipData;",
                &[],
            )?
            .l()?;
        if clip.is_null() {
            return Ok(ClipboardContents {
                text: None,
                image: None,
            });
        }
        let count = env.call_method(&clip, "getItemCount", "()I", &[])?.i()?;
        if count <= 0 {
            return Ok(ClipboardContents {
                text: None,
                image: None,
            });
        }
        let item = env
            .call_method(
                &clip,
                "getItemAt",
                "(I)Landroid/content/ClipData$Item;",
                &[jni::objects::JValue::Int(0)],
            )?
            .l()?;
        let sequence = item_text(env, &item)?;
        let text = java_char_sequence(env, sequence)?;
        let uri = env
            .call_method(&item, "getUri", "()Landroid/net/Uri;", &[])?
            .l()?;
        let image = if uri.is_null() {
            None
        } else {
            read_uri_bytes(env, context, &uri).ok().flatten()
        };
        Ok(ClipboardContents { text, image })
    })
    .ok()
}

#[cfg(target_os = "android")]
fn item_text<'local>(
    env: &mut jni::JNIEnv<'local>,
    item: &jni::objects::JObject<'local>,
) -> jni::errors::Result<jni::objects::JObject<'local>> {
    env.call_method(item, "getText", "()Ljava/lang/CharSequence;", &[])?
        .l()
}

#[cfg(target_os = "android")]
fn java_char_sequence(
    env: &mut jni::JNIEnv,
    value: jni::objects::JObject,
) -> jni::errors::Result<Option<String>> {
    if value.is_null() {
        return Ok(None);
    }
    let text = env
        .call_method(&value, "toString", "()Ljava/lang/String;", &[])?
        .l()?;
    let text: String = env.get_string(&jni::objects::JString::from(text))?.into();
    Ok(Some(text))
}

#[cfg(target_os = "android")]
fn read_uri_bytes(
    env: &mut jni::JNIEnv,
    context: &jni::objects::JObject,
    uri: &jni::objects::JObject,
) -> jni::errors::Result<Option<Vec<u8>>> {
    let resolver = env
        .call_method(
            context,
            "getContentResolver",
            "()Landroid/content/ContentResolver;",
            &[],
        )?
        .l()?;
    let stream = env
        .call_method(
            &resolver,
            "openInputStream",
            "(Landroid/net/Uri;)Ljava/io/InputStream;",
            &[jni::objects::JValue::Object(uri)],
        )?
        .l()?;
    if stream.is_null() {
        return Ok(None);
    }
    let out = env.new_object("java/io/ByteArrayOutputStream", "()V", &[])?;
    let buffer = env.new_byte_array(8192)?;
    let mut total = 0i32;
    loop {
        let read = env
            .call_method(
                &stream,
                "read",
                "([B)I",
                &[jni::objects::JValue::Object(&buffer)],
            )?
            .i()?;
        if read <= 0 {
            break;
        }
        total += read;
        if total > 8 * 1024 * 1024 {
            let _ = env.call_method(&stream, "close", "()V", &[]);
            return Ok(None);
        }
        env.call_method(
            &out,
            "write",
            "([BII)V",
            &[
                jni::objects::JValue::Object(&buffer),
                jni::objects::JValue::Int(0),
                jni::objects::JValue::Int(read),
            ],
        )?;
    }
    let _ = env.call_method(&stream, "close", "()V", &[]);
    let bytes = env.call_method(&out, "toByteArray", "()[B", &[])?.l()?;
    Ok(Some(env.convert_byte_array(
        jni::objects::JByteArray::from(bytes),
    )?))
}

#[cfg(target_os = "android")]
fn system_service<'local>(
    env: &mut jni::JNIEnv<'local>,
    context: &jni::objects::JObject,
    name: &str,
) -> jni::errors::Result<jni::objects::JObject<'local>> {
    let name = env.new_string(name)?;
    env.call_method(
        context,
        "getSystemService",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        &[jni::objects::JValue::Object(&name)],
    )?
    .l()
}

#[cfg(target_os = "android")]
pub fn copy_to_clipboard(text: &str) -> bool {
    with_process(|env, context| {
        let clipboard_service = env.new_string("clipboard")?;
        let clipboard = env
            .call_method(
                context,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[jni::objects::JValue::Object(&clipboard_service)],
            )?
            .l()?;
        let label = env.new_string("Public key")?;
        let contents = env.new_string(text)?;
        let clip = env
            .call_static_method(
                "android/content/ClipData",
                "newPlainText",
                "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
                &[
                    jni::objects::JValue::Object(&label),
                    jni::objects::JValue::Object(&contents),
                ],
            )?
            .l()?;
        env.call_method(
            clipboard,
            "setPrimaryClip",
            "(Landroid/content/ClipData;)V",
            &[jni::objects::JValue::Object(&clip)],
        )?;
        Ok(())
    })
    .is_ok()
}

#[cfg(target_os = "android")]
fn android_files_dir() -> Option<PathBuf> {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    if let Some(directory) = DIRECTORY.get() {
        return Some(directory.clone());
    }
    let directory = with_process(|env, context| {
        let file = env
            .call_method(context, "getFilesDir", "()Ljava/io/File;", &[])?
            .l()?;
        let path = env
            .call_method(file, "getAbsolutePath", "()Ljava/lang/String;", &[])?
            .l()?;
        let path: String = env.get_string(&jni::objects::JString::from(path))?.into();
        Ok(PathBuf::from(path).join("journal"))
    })
    .ok()?;
    Some(DIRECTORY.get_or_init(|| directory).clone())
}

#[cfg(target_os = "android")]
pub fn machine_name() -> String {
    let model = with_android_context(|env, context| {
        let value = env
            .call_method(context, "machineName", "()Ljava/lang/String;", &[])?
            .l()?;
        let string = jni::objects::JString::from(value);
        let text: String = env.get_string(&string)?.into();
        Ok(text)
    })
    .unwrap_or_default();
    machine_label(&model)
}

/// The first word of a phone's model, so "Pixel 7 Pro" is "Pixel".
#[cfg(any(test, target_os = "android"))]
pub fn machine_label(model: &str) -> String {
    match model.split_whitespace().next() {
        Some(word) if !word.is_empty() => word.to_string(),
        _ => "Phone".to_string(),
    }
}

#[cfg(target_os = "android")]
struct AndroidProcess {
    vm: jni::JavaVM,
    app: jni::objects::GlobalRef,
    class: jni::objects::GlobalRef,
}

/// The process context and the activity class, taken once while the activity is alive.
/// Later calls use these owned references. The activity reference published by the
/// binding is deleted when that activity is destroyed.
#[cfg(target_os = "android")]
fn android_process() -> jni::errors::Result<&'static AndroidProcess> {
    static PROCESS: OnceLock<AndroidProcess> = OnceLock::new();
    if let Some(process) = PROCESS.get() {
        return Ok(process);
    }
    let loaded = load_android_process()?;
    Ok(PROCESS.get_or_init(|| loaded))
}

#[cfg(target_os = "android")]
fn load_android_process() -> jni::errors::Result<AndroidProcess> {
    let ctx = ndk_context::android_context();
    if ctx.vm().is_null() || ctx.context().is_null() {
        return Err(jni::errors::Error::NullPtr("android context"));
    }
    // SAFETY: The Dioxus Android binding installs this process JavaVM before `main`.
    // `JavaVM::from_raw` only wraps that pointer; dropping the wrapper does not destroy the VM.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }?;
    let (app, class) = {
        let mut env = vm.attach_current_thread()?;
        // SAFETY: The same binding publishes the activity as a live JNI context reference.
        // `JObject` does not delete that reference when dropped.
        let activity = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };
        let app = env
            .call_method(
                &activity,
                "getApplicationContext",
                "()Landroid/content/Context;",
                &[],
            )?
            .l()?;
        if app.is_null() {
            return Err(jni::errors::Error::NullPtr("application context"));
        }
        let app = env.new_global_ref(app)?;
        let class = env.get_object_class(&activity)?;
        let class = env.new_global_ref(class)?;
        (app, class)
    };
    Ok(AndroidProcess { vm, app, class })
}

#[cfg(target_os = "android")]
fn with_process<T>(
    body: impl FnOnce(&mut jni::JNIEnv, &jni::objects::JObject) -> jni::errors::Result<T>,
) -> jni::errors::Result<T> {
    let process = android_process()?;
    let mut env = process.vm.attach_current_thread()?;
    body(&mut env, process.app.as_obj())
}

#[cfg(target_os = "android")]
fn with_activity_class<T>(
    body: impl FnOnce(&mut jni::JNIEnv, &jni::objects::GlobalRef) -> jni::errors::Result<T>,
) -> jni::errors::Result<T> {
    let process = android_process()?;
    let mut env = process.vm.attach_current_thread()?;
    body(&mut env, &process.class)
}

#[cfg(target_os = "android")]
fn with_android_context<T>(
    body: impl FnOnce(&mut jni::JNIEnv, &jni::objects::JObject) -> jni::errors::Result<T>,
) -> jni::errors::Result<T> {
    let ctx = ndk_context::android_context();
    // SAFETY: The Dioxus Android binding installs this process JavaVM before `main`.
    // `JavaVM::from_raw` only wraps that pointer; dropping the wrapper does not destroy the VM.
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }?;
    let mut env = vm.attach_current_thread()?;
    // SAFETY: The same binding publishes the activity as a live JNI context reference.
    // `JObject` does not delete that reference when dropped.
    let context = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };
    body(&mut env, &context)
}

/// The journal activity is on screen. A paused or destroyed activity counts as away.
#[cfg(target_os = "android")]
pub(crate) fn screen_is_on() -> bool {
    with_activity_class(|env, class| {
        Ok(env.call_static_method(class, "onScreen", "()Z", &[])?.z()?)
    })
    .unwrap_or(false)
}

/// Posts a banner for a note that arrived while the journal was in the background.
#[cfg(target_os = "android")]
pub(crate) fn show_banner(name: &str, words: &str) {
    let _ = with_activity_class(|env, class| {
        let name = env.new_string(name)?;
        let words = env.new_string(words)?;
        env.call_static_method(
            class,
            "postNote",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            &[
                jni::objects::JValue::Object(&name),
                jni::objects::JValue::Object(&words),
            ],
        )?;
        Ok(())
    });
}

#[cfg(target_os = "android")]
pub enum PhotoCapture {
    Code(String),
    Cancelled,
    Denied,
    Unavailable,
}

#[cfg(target_os = "android")]
pub async fn capture_photo() -> PhotoCapture {
    let Some(runtime) = RUNTIME.get() else {
        return PhotoCapture::Unavailable;
    };
    runtime
        .spawn_blocking(capture_photo_blocking)
        .await
        .unwrap_or(PhotoCapture::Unavailable)
}

#[cfg(target_os = "android")]
fn capture_photo_blocking() -> PhotoCapture {
    with_android_context(|env, context| {
        // The activity object is already loaded. Looking the class up by name from this
        // worker thread uses the system loader and cannot see the app.
        let class = env.get_object_class(context)?;
        let value = env
            .call_static_method(&class, "capture", "()Ljava/lang/String;", &[])?
            .l()?;
        if !value.is_null() {
            let string = jni::objects::JString::from(value);
            let text = env.get_string(&string)?;
            let Ok(text) = text.to_str() else {
                return Ok(PhotoCapture::Unavailable);
            };
            return Ok(PhotoCapture::Code(text.to_owned()));
        }
        let status = env.get_static_field(&class, "status", "I")?.i()?;
        Ok(match status {
            0 => PhotoCapture::Cancelled,
            1 => PhotoCapture::Denied,
            _ => PhotoCapture::Unavailable,
        })
    })
    .unwrap_or(PhotoCapture::Unavailable)
}

/// A file the person chose on Android. The desktop composer uses the page's own file control.
#[cfg(target_os = "android")]
pub enum ChosenFile {
    Ready { name: String, bytes: Vec<u8> },
    Over,
    Missing,
    Cancelled,
}

#[cfg(target_os = "android")]
pub async fn choose_file(limit: Option<u64>) -> ChosenFile {
    let Some(runtime) = RUNTIME.get() else {
        return ChosenFile::Missing;
    };
    let max = limit.unwrap_or(0);
    runtime
        .spawn_blocking(move || choose_file_blocking(max))
        .await
        .unwrap_or(ChosenFile::Missing)
}

#[cfg(target_os = "android")]
fn choose_file_blocking(max_bytes: u64) -> ChosenFile {
    let max = i64::try_from(max_bytes).unwrap_or(0);
    let text = with_android_context(|env, context| {
        let class = env.get_object_class(context)?;
        let value = env
            .call_static_method(
                &class,
                "chooseFile",
                "(J)Ljava/lang/String;",
                &[jni::objects::JValue::Long(max)],
            )?
            .l()?;
        if value.is_null() {
            return Ok(None);
        }
        let string = jni::objects::JString::from(value);
        let text: String = env.get_string(&string)?.into();
        Ok(Some(text))
    });
    let Ok(Some(text)) = text else {
        return ChosenFile::Cancelled;
    };
    if text == "over" {
        return ChosenFile::Over;
    }
    if text == "missing" {
        return ChosenFile::Missing;
    }
    let Some((name, path)) = text.split_once('\n') else {
        return ChosenFile::Missing;
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return ChosenFile::Missing,
    };
    let _ = std::fs::remove_file(path);
    if bytes.is_empty() {
        return ChosenFile::Missing;
    }
    ChosenFile::Ready {
        name: name.to_string(),
        bytes,
    }
}

#[cfg(target_os = "android")]
pub fn device_location() -> Option<String> {
    with_android_context(|env, context| {
        let class = env.get_object_class(context)?;
        let value = env
            .call_static_method(&class, "whereAmI", "()Ljava/lang/String;", &[])?
            .l()?;
        if value.is_null() {
            return Ok(None);
        }
        let string = jni::objects::JString::from(value);
        let text: String = env.get_string(&string)?.into();
        Ok(Some(text))
    })
    .ok()
    .and_then(|text| text.filter(|text| !text.is_empty()))
}

#[cfg(target_os = "android")]
pub fn open_map(query: &str) {
    let _ = with_android_context(|env, context| {
        let class = env.get_object_class(context)?;
        let text = env.new_string(query)?;
        env.call_static_method(
            &class,
            "showPlace",
            "(Ljava/lang/String;)V",
            &[jni::objects::JValue::Object(&text)],
        )?;
        Ok(())
    });
}

/// A note for a contact whose code is their identity public key.
/// The words travel as an LXMF message on a link to that contact's `lxmf.delivery` address.
/// If no path exists yet, the note stays queued and is tried again until it is delivered.
pub fn send_note(
    contact_identity: &str,
    words: &str,
    note_id: u64,
    written_at: f64,
    place: Option<crate::model::Place>,
    file: Option<(&str, &[u8])>,
) {
    let words = words.trim().to_string();
    let place = place.filter(|_| words.contains(crate::model::PLACEHOLDER));
    let owned_name = file
        .as_ref()
        .map(|(name, _)| crate::model::file_name_of(name));
    let file = match (&owned_name, file) {
        (Some(name), Some((_, bytes)))
            if !bytes.is_empty() && u32::try_from(bytes.len()).is_ok() =>
        {
            Some((name.as_str(), bytes))
        }
        _ => None,
    };
    if words.is_empty() && file.is_none() {
        return;
    }
    let Some(recipient) = contact_material(contact_identity) else {
        return;
    };
    let Some(destination) = address_for(&recipient) else {
        return;
    };
    let packed = crate::lxmf::pack_file_note(&words, written_at, place, file);
    if note_id != 0 {
        sent_notes().insert(note_id, sha256(&packed));
    }
    note_queue().push(QueuedNote {
        destination,
        recipient,
        identity: contact_identity.trim().to_string(),
        packed,
        note_id,
        tries: 0,
        kind: OutboundKind::Note,
        stage: OutboundStage::Person,
        mailbox_bytes: None,
    });
    nudge_delivery();
}

/// Tells the sender of a mailbox note that it is in the journal. The hash is of the packed note.
pub fn send_ack(contact_identity: &str, digest: &[u8; 32]) {
    let Some(recipient) = contact_material(contact_identity) else {
        return;
    };
    let Some(destination) = address_for(&recipient) else {
        return;
    };
    note_queue().push(QueuedNote {
        destination,
        recipient,
        identity: contact_identity.trim().to_string(),
        packed: crate::lxmf::pack_ack(digest),
        note_id: 0,
        tries: 0,
        kind: OutboundKind::Ack,
        stage: OutboundStage::Person,
        mailbox_bytes: None,
    });
    nudge_delivery();
}

/// A reaction to a note the contact already has.
/// The time it was written matches a newer note. The words match one saved before that.
pub fn send_reaction(
    contact_identity: &str,
    written_at: f64,
    words: &str,
    reaction: Option<crate::model::Reaction>,
) {
    if written_at.to_bits() == 0 && words.trim().is_empty() {
        return;
    }
    let Some(recipient) = contact_material(contact_identity) else {
        return;
    };
    let Some(destination) = address_for(&recipient) else {
        return;
    };
    note_queue().push(QueuedNote {
        destination,
        recipient,
        identity: contact_identity.trim().to_string(),
        packed: crate::lxmf::pack_reaction(written_at, words, reaction),
        note_id: 0,
        tries: 0,
        kind: OutboundKind::Reaction,
        stage: OutboundStage::Person,
        mailbox_bytes: None,
    });
    nudge_delivery();
}

/// How many times a direct-only send is tried before it is marked failed.
const GIVE_UP_AFTER: u8 = 3;

/// A note the person cannot take is posted to the mailbox by this time.
const UNTIL_POSTED: Duration = Duration::from_secs(15);

/// Direct delivery of a link packet may use this much of [`UNTIL_POSTED`]. The rest is the mailbox.
const PERSON_SLICE: Duration = Duration::from_secs(8);

/// How long a resource may move before it is given up. A link packet stays on
/// [`PERSON_SLICE`]. A larger send gets the open-path floor (150 kbps) on top of
/// that, so a photo is not abandoned while it is still crossing.
fn resource_budget(bytes: u64) -> Duration {
    let moving_ms = bytes.saturating_mul(8_000) / crate::model::OPEN_PATH_BPS.max(1);
    PERSON_SLICE.saturating_add(Duration::from_millis(moving_ms))
}

/// How long a file send may sit with no new bytes. Before the first byte, that is the whole
/// budget. After bytes are moving, a quiet stretch of [`PERSON_SLICE`] means the transfer stopped.
fn person_quiet_limit(budget: Duration, moved: bool) -> Duration {
    if moved {
        PERSON_SLICE
    } else {
        budget
    }
}

/// Polls the outgoing file until `attempt` finishes. A transfer that is still gaining bytes keeps
/// its note. One that sits still for [`person_quiet_limit`] is the timeout.
async fn wait_for_moving_send<F>(
    label: &str,
    budget: Duration,
    mut attempt: std::pin::Pin<&mut F>,
) -> Result<Result<(), String>, ()>
where
    F: std::future::Future<Output = Result<(), String>>,
{
    let started = Instant::now();
    let mut quiet_since = Instant::now();
    let mut last = 0u64;
    let mut noted = false;
    loop {
        tokio::select! {
            biased;
            result = &mut attempt => return Ok(result),
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                let done = transfer_view().await.done;
                if done > last {
                    last = done;
                    quiet_since = Instant::now();
                }
                if !noted && last > 0 && started.elapsed() >= budget {
                    noted = true;
                    delivery_log(&format!(
                        "{label} person still moving at {last} bytes"
                    ));
                }
                let quiet = quiet_since.elapsed();
                if quiet >= person_quiet_limit(budget, last > 0) {
                    delivery_log(&format!(
                        "{label} person stopped at {last} bytes after {}ms quiet",
                        quiet.as_millis()
                    ));
                    return Err(());
                }
            }
        }
    }
}

/// The note whose file is on the wire right now. Zero when nothing large is leaving.
static SENDING_NOTE: AtomicU64 = AtomicU64::new(0);

struct SendingNote;

impl SendingNote {
    fn arm(note_id: u64) -> Option<Self> {
        if note_id == 0 {
            return None;
        }
        SENDING_NOTE.store(note_id, Ordering::Release);
        Some(Self)
    }
}

impl Drop for SendingNote {
    fn drop(&mut self) {
        SENDING_NOTE.store(0, Ordering::Release);
    }
}

/// How far one transfer has moved. `done` and `total` are bytes of the whole file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransferView {
    pub note_id: u64,
    pub done: u64,
    pub total: u64,
    pub incoming_identity: String,
    pub incoming_done: u64,
    pub incoming_total: u64,
}

struct SegmentSlice {
    index: u64,
    total_segments: u64,
    parts_done: u64,
    part_count: u64,
    whole_bytes: u64,
}

/// A resource segment is numbered from 1. The slot in the assembly is one less.
fn segment_slot(index: u64, total: u64) -> Option<usize> {
    let slot = index.checked_sub(1)?;
    if total == 0 || total > 4_096 || slot >= total {
        return None;
    }
    usize::try_from(slot).ok()
}

/// Parts already across, counted in bytes of the whole file.
///
/// A segment is not a mebibyte. The last two segments of a file are split so neither is a short
/// tail, so crediting every earlier segment as [`MAX_EFFICIENT_SIZE`] shows the file almost gone
/// while half of it is still ahead. The span comes from the same plan the sender uses. A finished
/// earlier segment stays in the total when only a later segment is still live.
fn progress_of(segments: &[SegmentSlice]) -> Option<(u64, u64)> {
    let whole = segments.first()?.whole_bytes;
    if whole == 0 {
        return None;
    }
    let full = personal_rns::routing::links::resources::MAX_EFFICIENT_SIZE as u64;
    let stream = full.min(whole).max(1);
    let plan =
        personal_rns::routing::links::resources::ResourceSendPlan::new(whole, None, stream).ok()?;
    let min_index = segments.iter().map(|segment| segment.index).min()?;
    let earlier = plan
        .segment(min_index)
        .map(|span| span.data_start)
        .unwrap_or(0)
        .min(whole);
    let mut partial = 0u64;
    for segment in segments {
        if segment.index == 0 || segment.index > segment.total_segments.max(1) {
            continue;
        }
        let Some(span) = plan.segment(segment.index) else {
            continue;
        };
        let this_len = span.data_end.saturating_sub(span.data_start);
        let counted = segment.parts_done.min(segment.part_count);
        let done = if segment.part_count == 0 {
            0
        } else {
            this_len.saturating_mul(counted) / segment.part_count
        };
        partial = partial.saturating_add(done);
    }
    Some((earlier.saturating_add(partial).min(whole), whole))
}

/// The file leaving this journal, and the file arriving, if either is on the wire.
pub async fn transfer_view() -> TransferView {
    let note_id = SENDING_NOTE.load(Ordering::Acquire);
    let Some(handle) = HANDLE.get() else {
        return TransferView {
            note_id,
            ..TransferView::default()
        };
    };
    let snap = handle.resource_segments().await;
    note_wire(&snap.segments);
    note_rounds(&snap.rounds, snap.rounds_dropped);
    let mut outgoing: HashMap<[u8; 32], Vec<SegmentSlice>> = HashMap::new();
    let mut incoming: HashMap<(LinkId, [u8; 32]), Vec<SegmentSlice>> = HashMap::new();
    for row in snap.segments {
        let slice = SegmentSlice {
            index: row.segment_index,
            total_segments: row.total_segments,
            parts_done: row.parts_done,
            part_count: row.part_count,
            whole_bytes: row.whole_bytes,
        };
        let mark = *row.mark.as_bytes();
        if row.incoming {
            incoming.entry((row.link_id, mark)).or_default().push(slice);
        } else {
            outgoing.entry(mark).or_default().push(slice);
        }
    }
    let (done, total) = outgoing
        .into_values()
        .filter_map(|segments| progress_of(&segments))
        .max_by_key(|(_, total)| *total)
        .unwrap_or((0, 0));
    let incoming_pick = incoming
        .into_iter()
        .filter_map(|((link, _), segments)| {
            let (done, total) = progress_of(&segments)?;
            Some((link, done, total))
        })
        .max_by_key(|(_, _, total)| *total);
    let (incoming_identity, incoming_done, incoming_total) = match incoming_pick {
        Some((link, done, total)) => {
            let identity = mail()
                .peers
                .get(&link)
                .map(|identity| hex_encode(identity.as_bytes()))
                .unwrap_or_default();
            (identity, done, total)
        }
        None => (String::new(), 0, 0),
    };
    TransferView {
        note_id,
        done,
        total,
        incoming_identity,
        incoming_done,
        incoming_total,
    }
}

enum OutboundKind {
    Note,
    Ack,
    Reaction,
}

enum OutboundStage {
    Person,
    Mailbox,
}

enum DeliveryOutcome {
    Settled,
    Posted,
    Again,
    Failed,
}

struct QueuedNote {
    destination: personal_rns::wire::DestinationHash,
    recipient: PublicIdentityMaterial,
    identity: String,
    packed: Vec<u8>,
    note_id: u64,
    tries: u8,
    kind: OutboundKind,
    stage: OutboundStage,
    /// The sealed mailbox packet, kept so a retry does not stamp the note again.
    mailbox_bytes: Option<Vec<u8>>,
}

fn note_queue() -> std::sync::MutexGuard<'static, Vec<QueuedNote>> {
    static QUEUE: OnceLock<Mutex<Vec<QueuedNote>>> = OnceLock::new();
    QUEUE
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

static DELIVERING: AtomicBool = AtomicBool::new(false);

fn nudge_delivery() {
    let Some(runtime) = RUNTIME.get().cloned() else {
        return;
    };
    if DELIVERING.swap(true, Ordering::AcqRel) {
        return;
    }
    runtime.spawn(async move {
        loop {
            let batch = {
                let mut queue = note_queue();
                std::mem::take(&mut *queue)
            };
            if batch.is_empty() {
                DELIVERING.store(false, Ordering::Release);
                if note_queue().is_empty() || DELIVERING.swap(true, Ordering::AcqRel) {
                    break;
                }
                continue;
            }
            let mut again = Vec::new();
            for mut note in batch {
                match deliver_note(&mut note).await {
                    DeliveryOutcome::Settled => {
                        if matches!(note.kind, OutboundKind::Note) && note.note_id != 0 {
                            mail().acked.push(note.note_id);
                        }
                    }
                    DeliveryOutcome::Posted => {
                        if matches!(note.kind, OutboundKind::Note) && note.note_id != 0 {
                            mail().posted.push((note.note_id, sha256(&note.packed)));
                        }
                    }
                    DeliveryOutcome::Again => {
                        note.tries = note.tries.saturating_add(1);
                        if note.tries >= GIVE_UP_AFTER {
                            if matches!(note.kind, OutboundKind::Note) && note.note_id != 0 {
                                mail().failed.push(note.note_id);
                            }
                        } else {
                            again.push(note);
                        }
                    }
                    DeliveryOutcome::Failed => {
                        if matches!(note.kind, OutboundKind::Note) && note.note_id != 0 {
                            mail().failed.push(note.note_id);
                        }
                    }
                }
            }
            if !again.is_empty() {
                let pause = {
                    let mut queue = note_queue();
                    let fresh = std::mem::take(&mut *queue);
                    let pause = fresh.is_empty();
                    *queue = fresh;
                    queue.extend(again);
                    pause
                };
                if pause {
                    rest(5_000).await;
                }
            }
        }
    });
}

async fn deliver_note(note: &mut QueuedNote) -> DeliveryOutcome {
    let can_post = !matches!(note.kind, OutboundKind::Reaction) && mailbox_address().is_some();
    let fits = crate::lxmf::fits_one_link_packet(&note.packed);
    let _meter = (!fits).then(|| SendingNote::arm(note.note_id)).flatten();
    if !can_post {
        let budget = (!fits).then(|| resource_budget(note.packed.len() as u64));
        return if deliver_to_person(note, budget).await {
            DeliveryOutcome::Settled
        } else {
            DeliveryOutcome::Again
        };
    }
    let started = Instant::now();
    let label = note_label(note);
    if opens_at_the_mailbox(contact_is_isolated(&note.identity), true) {
        delivery_log(&format!(
            "{label} contact is isolated, posting to the mailbox"
        ));
    } else {
        let person = if fits {
            PERSON_SLICE.min(UNTIL_POSTED.saturating_sub(started.elapsed()))
        } else {
            resource_budget(note.packed.len() as u64)
        };
        delivery_log(&format!(
            "{label} trying the person for {}ms{}",
            person.as_millis(),
            if fits {
                String::new()
            } else {
                format!(" ({} bytes)", note.packed.len())
            }
        ));
        if deliver_to_person(note, Some(person)).await {
            return DeliveryOutcome::Settled;
        }
    }
    note.stage = OutboundStage::Mailbox;
    let left = if fits {
        UNTIL_POSTED.saturating_sub(started.elapsed())
    } else {
        resource_budget(note.packed.len() as u64)
    };
    if deliver_to_mailbox(note, left).await {
        DeliveryOutcome::Posted
    } else {
        delivery_log(&format!(
            "{label} failed after {}ms",
            started.elapsed().as_millis()
        ));
        DeliveryOutcome::Failed
    }
}

/// A finished search with no path. A contact that has not been measured yet is not isolated.
fn contact_is_isolated(identity: &str) -> bool {
    let identity = identity.trim();
    if identity.is_empty() {
        return false;
    }
    let map = reach_map();
    let Some(state) = map.get(identity) else {
        return false;
    };
    state.done
        && state
            .path
            .as_ref()
            .map(|path| path.trim().is_empty())
            .unwrap_or(true)
}

fn opens_at_the_mailbox(isolated: bool, can_post: bool) -> bool {
    isolated && can_post
}

fn note_label(note: &QueuedNote) -> String {
    match note.kind {
        OutboundKind::Note => format!("note {}", note.note_id),
        OutboundKind::Ack => "ack".to_string(),
        OutboundKind::Reaction => "reaction".to_string(),
    }
}

fn hash_hex(hash: &personal_rns::wire::DestinationHash) -> String {
    hash.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A line in the delivery log. Used while a file is crossing so the wire can be read afterwards.
pub fn trace(message: &str) {
    delivery_log(message);
}

fn delivery_log(message: &str) {
    let when = chrono::Local::now().format("%H:%M:%S%.3f");
    let line = format!("{when} {message}\n");
    let path = default_directory().join("delivery.log");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(line.as_bytes())
        });
}

static WIRE_AT: AtomicU64 = AtomicU64::new(0);
static WIRE_QUIET: AtomicBool = AtomicBool::new(true);

/// One line every two seconds while a file is on a link, and one line when it leaves.
fn note_wire(rows: &[personal_rns::engine::ResourceSegmentProgress]) {
    if rows.is_empty() {
        if !WIRE_QUIET.swap(true, Ordering::Relaxed) {
            delivery_log("wire quiet");
        }
        return;
    }
    WIRE_QUIET.store(false, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let last = WIRE_AT.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 2_000 {
        return;
    }
    WIRE_AT.store(now, Ordering::Relaxed);
    for row in rows {
        let way = if row.incoming { "in" } else { "out" };
        let wire = if row.interface == personal_rns::interfaces::InterfaceId::new([0; 8]) {
            "no link".to_string()
        } else {
            interface_name(row.interface)
        };
        delivery_log(&format!(
            "wire {way} {wire} rtt={}ms mtu={} window={} ceiling={} sdu={} seg={}/{} parts={}/{} bytes={}",
            row.rtt_millis,
            row.mtu,
            row.window,
            row.ceiling,
            row.sdu,
            row.segment_index,
            row.total_segments,
            row.parts_done,
            row.part_count,
            row.whole_bytes
        ));
    }
}

/// Every request since the last poll, written in one open so the log does not slow the transfer.
fn note_rounds(rounds: &[personal_rns::engine::ResourceRoundNote], dropped: u32) {
    if rounds.is_empty() && dropped == 0 {
        return;
    }
    use personal_rns::engine::ResourceRoundKind;
    let when = chrono::Local::now().format("%H:%M:%S%.3f");
    let mut body = String::new();
    for row in rounds {
        let line = match row.kind {
            ResourceRoundKind::Pull => format!(
                "pull seg={}/{} at={} asked={} win={} max={} fast={} map={} hmu={}",
                row.segment_index,
                row.total_segments,
                row.part_at,
                row.asked,
                row.window,
                row.ceiling,
                row.fast_rounds,
                row.hashmap_height,
                row.flag
            ),
            ResourceRoundKind::Round => format!(
                "round seg={}/{} at={} got={} win={} max={} first={}ms span={}ms",
                row.segment_index,
                row.total_segments,
                row.part_at,
                row.moved,
                row.window,
                row.ceiling,
                row.first_ms,
                row.span_ms
            ),
            ResourceRoundKind::Watchdog => format!(
                "watch seg={}/{} at={} win={} max={} span={}ms",
                row.segment_index,
                row.total_segments,
                row.part_at,
                row.window,
                row.ceiling,
                row.span_ms
            ),
            ResourceRoundKind::Serve => format!(
                "serve seg={}/{} at={} asked={} sent={} hmu={}",
                row.segment_index, row.total_segments, row.part_at, row.asked, row.moved, row.flag
            ),
            ResourceRoundKind::Admit => format!(
                "admit seg={}/{} win={} max={} carried={}",
                row.segment_index, row.total_segments, row.window, row.ceiling, row.flag
            ),
            ResourceRoundKind::Proof => {
                let verdict = match row.flag {
                    personal_rns::engine::ResourceRoundNote::PROOF_OUT => "out",
                    personal_rns::engine::ResourceRoundNote::PROOF_IN_ACCEPTED => "in accepted",
                    personal_rns::engine::ResourceRoundNote::PROOF_IN_INVALID => "in invalid",
                    personal_rns::engine::ResourceRoundNote::PROOF_IN_MALFORMED => "in malformed",
                    personal_rns::engine::ResourceRoundNote::PROOF_IN_MISSING => "in missing",
                    personal_rns::engine::ResourceRoundNote::PROOF_IN_OFF_WIRE => "in off-wire",
                    _ => "in",
                };
                if matches!(
                    row.flag,
                    personal_rns::engine::ResourceRoundNote::PROOF_IN_MALFORMED
                        | personal_rns::engine::ResourceRoundNote::PROOF_IN_MISSING
                ) {
                    format!("proof {verdict}")
                } else {
                    format!(
                        "proof {verdict} seg={}/{}",
                        row.segment_index, row.total_segments
                    )
                }
            }
        };
        body.push_str(&format!("{when} {line}\n"));
    }
    if dropped > 0 {
        body.push_str(&format!("{when} rounds dropped {dropped}\n"));
    }
    let path = default_directory().join("delivery.log");
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            use std::io::Write;
            file.write_all(body.as_bytes())
        });
}

fn log_up_wires(handle: &PrnsNodeHandle, label: &str) {
    let snaps = handle.interfaces();
    let mut any = false;
    for snap in &snaps {
        if !matches!(
            snap.connection,
            personal_rns::interfaces::ConnectionState::Connected
                | personal_rns::interfaces::ConnectionState::Degraded
        ) {
            continue;
        }
        any = true;
        delivery_log(&format!(
            "{label} {} is up, {} links, rx {} tx {}",
            interface_name(snap.id),
            snap.links,
            snap.rx_bytes,
            snap.tx_bytes
        ));
    }
    if !any {
        delivery_log(&format!("{label} no wire is up"));
    }
}

async fn deliver_to_person(note: &QueuedNote, budget: Option<Duration>) -> bool {
    let label = note_label(note);
    let Some(handle) = HANDLE.get().cloned() else {
        delivery_log(&format!("{label} person skipped: the node is not running"));
        return false;
    };
    if note.packed.is_empty() {
        delivery_log(&format!("{label} person skipped: nothing to send"));
        return false;
    }
    let one_packet = crate::lxmf::fits_one_link_packet(&note.packed);
    if !one_packet {
        let carry = carry_for_identity(&note.identity);
        let weight = crate::lxmf::carried_bytes(&note.packed);
        if !carry.allows(weight) {
            delivery_log(&format!(
                "{label} person skipped: {weight} bytes is more than this path will carry ({})",
                carry_label(carry)
            ));
            return false;
        }
    }
    let ours = OURS.get().copied();
    let destination = note.destination;
    let attempt = async {
        log_up_wires(&handle, &format!("{label} person"));
        match handle.request_path(destination).await {
            Ok(found) => delivery_log(&format!("{label} person path {} hops", found.hops.0)),
            Err(err) => delivery_log(&format!("{label} person path {err:?}")),
        }
        match handle.route(destination).await {
            Some(route) => delivery_log(&format!(
                "{label} person route {} {} hops",
                interface_name(route.interface),
                route.hops
            )),
            None => delivery_log(&format!("{label} person route none")),
        }
        let link = handle
            .establish_link_with_rtt(destination)
            .await
            .map_err(|err| format!("link {err:?}"))?;
        delivery_log(&format!(
            "{label} person link {:?} rtt={}ms",
            link.link_id, link.rtt_millis
        ));
        let link = link.link_id;
        if let Some(ours) = ours {
            if let Err(err) = handle.identify(link, ours).await {
                delivery_log(&format!("{label} person identify {err:?}"));
            }
        }
        let ratchet = handle.announced_ratchet(destination).await;
        let Some(material) = signing_material() else {
            return Err("no signing key".into());
        };
        let ratchet_public = ratchet.map(personal_rns::crypto::X25519PublicKey);
        let Some(sealed) = crate::mailbox::seal_for_delivery(
            &material,
            &note.recipient,
            &note.packed,
            ratchet_public.as_ref(),
        ) else {
            return Err("could not seal".into());
        };
        delivery_log(&format!(
            "{label} person sealed to {}",
            if ratchet_public.is_some() {
                "their ratchet"
            } else {
                "their identity key"
            }
        ));
        if !one_packet {
            if let Some(prelude) = crate::lxmf::file_announcement(&note.packed) {
                let Some(sealed_prelude) = crate::mailbox::seal_for_delivery(
                    &material,
                    &note.recipient,
                    &prelude,
                    ratchet_public.as_ref(),
                ) else {
                    return Err("could not seal the file announcement".into());
                };
                delivery_log(&format!("{label} person sent the words ahead of the file"));
                if crate::lxmf::fits_one_link_packet(&sealed_prelude) {
                    handle
                        .send_link_packet(link, &sealed_prelude)
                        .await
                        .map_err(|err| format!("announcement {err:?}"))?;
                } else {
                    let total = sealed_prelude.len() as u64;
                    let cursor = std::io::Cursor::new(sealed_prelude);
                    handle
                        .send_resource(link, total, cursor)
                        .await
                        .map_err(|err| format!("announcement {err:?}"))?;
                }
            }
        }
        if crate::lxmf::fits_one_link_packet(&sealed) {
            handle
                .send_link_packet(link, &sealed)
                .await
                .map_err(|err| format!("packet {err:?}"))?;
        } else {
            let total = sealed.len() as u64;
            let cursor = std::io::Cursor::new(sealed);
            handle
                .send_resource(link, total, cursor)
                .await
                .map_err(|err| format!("resource {err:?}"))?;
        }
        Ok::<(), String>(())
    };
    let budget = budget.unwrap_or(Duration::from_secs(20));
    let started = Instant::now();
    tokio::pin!(attempt);
    let waited = if one_packet {
        tokio::time::timeout(budget, attempt).await.map_err(|_| ())
    } else {
        wait_for_moving_send(&label, budget, attempt).await
    };
    match waited {
        Ok(Ok(())) => {
            delivery_log(&format!(
                "{label} person accepted in {}ms",
                started.elapsed().as_millis()
            ));
            true
        }
        Ok(Err(reason)) => {
            delivery_log(&format!(
                "{label} person missed ({reason}) in {}ms",
                started.elapsed().as_millis()
            ));
            false
        }
        Err(_) => {
            delivery_log(&format!(
                "{label} person timed out after {}ms",
                started.elapsed().as_millis()
            ));
            false
        }
    }
}

async fn deliver_to_mailbox(note: &mut QueuedNote, budget: Duration) -> bool {
    let label = note_label(note);
    let Some(mailbox) = mailbox_address() else {
        delivery_log(&format!("{label} mailbox skipped: no address"));
        return false;
    };
    let Some(handle) = HANDLE.get().cloned() else {
        delivery_log(&format!("{label} mailbox skipped: the node is not running"));
        return false;
    };
    let started = Instant::now();
    if note.mailbox_bytes.is_none() {
        let Some(ours) = signing_material() else {
            delivery_log(&format!("{label} mailbox skipped: no signing key"));
            return false;
        };
        let Some(packet) = tokio::task::spawn_blocking({
            let recipient = note.recipient;
            let packed = note.packed.clone();
            move || crate::mailbox::propagation_packet(&ours, &recipient, &packed)
        })
        .await
        .ok()
        .flatten() else {
            delivery_log(&format!(
                "{label} mailbox could not seal the note in {}ms",
                started.elapsed().as_millis()
            ));
            return false;
        };
        delivery_log(&format!(
            "{label} mailbox sealed {} bytes in {}ms",
            packet.len(),
            started.elapsed().as_millis()
        ));
        note.mailbox_bytes = Some(packet);
    }
    let Some(packet) = note.mailbox_bytes.clone() else {
        return false;
    };
    let left = budget.saturating_sub(started.elapsed());
    if left < Duration::from_millis(200) {
        delivery_log(&format!(
            "{label} mailbox had {}ms left after sealing",
            left.as_millis()
        ));
        return false;
    }
    let ours = OURS.get().copied();
    let mailbox_hex = hash_hex(&mailbox);
    let attempt = async {
        log_up_wires(&handle, &format!("{label} mailbox"));
        match handle.request_path(mailbox).await {
            Ok(found) => delivery_log(&format!("{label} mailbox path {} hops", found.hops.0)),
            Err(err) => delivery_log(&format!("{label} mailbox path {err:?}")),
        }
        let route = handle.route(mailbox).await;
        match &route {
            Some(route) => delivery_log(&format!(
                "{label} mailbox {mailbox_hex} route {} {} hops",
                interface_name(route.interface),
                route.hops
            )),
            None => delivery_log(&format!("{label} mailbox {mailbox_hex} has no route")),
        }
        if !crate::lxmf::fits_one_link_packet(&packet) {
            let carry = match &route {
                Some(route) => carry_for_path(&interface_name(route.interface), None),
                None => crate::model::resource_carry(None, false),
            };
            if !carry.allows(packet.len() as u64) {
                return Err(format!(
                    "{} bytes is more than the mailbox path will carry ({})",
                    packet.len(),
                    carry_label(carry)
                ));
            }
        }
        let link = handle
            .establish_link(mailbox)
            .await
            .map_err(|err| format!("link {err:?}"))?;
        if let Some(ours) = ours {
            if let Err(err) = handle.identify(link, ours).await {
                handle.close_link(link);
                return Err(format!("identify {err:?}"));
            }
        } else {
            handle.close_link(link);
            return Err("no identity to present".to_string());
        }
        let accepted = if crate::lxmf::fits_one_link_packet(&packet) {
            handle
                .send_link_packet(link, &packet)
                .await
                .map(|_| "link packet")
                .map_err(|err| format!("link packet {err:?}"))
        } else {
            let total = packet.len() as u64;
            let cursor = std::io::Cursor::new(packet);
            handle
                .send_resource(link, total, cursor)
                .await
                .map(|_| "resource")
                .map_err(|err| format!("resource {err:?}"))
        };
        handle.close_link(link);
        accepted
    };
    match tokio::time::timeout(left, attempt).await {
        Ok(Ok(how)) => {
            delivery_log(&format!(
                "{label} mailbox accepted the {how} in {}ms",
                started.elapsed().as_millis()
            ));
            true
        }
        Ok(Err(reason)) => {
            delivery_log(&format!(
                "{label} mailbox missed ({reason}) in {}ms",
                started.elapsed().as_millis()
            ));
            false
        }
        Err(_) => {
            delivery_log(&format!(
                "{label} mailbox timed out after {}ms",
                started.elapsed().as_millis()
            ));
            false
        }
    }
}

fn mailbox_address() -> Option<personal_rns::wire::DestinationHash> {
    crate::mailbox::configured(&default_directory())
}

fn signing_material() -> Option<PrivateIdentityMaterial> {
    let secret = SIGNING.get()?;
    PrivateIdentityMaterial::from_slice(&secret[..]).ok()
}

fn load_saved_ratchets(
    vault: &personal_rns::identity::vault::FileVault,
    destination: DestinationHash,
) {
    use personal_rns::identity::vault::IdentityVault;

    let label = personal_rns::runtime::self_ratchet_identity_label(&destination);
    let Ok(Some(len)) = vault.stored_blob_len(&label) else {
        return;
    };
    let mut buf = personal_rns::Zeroizing::new(vec![0u8; len]);
    match vault.load_blob(&label, &mut buf) {
        Ok(Some(bytes)) if remember_ratchet_secrets(bytes) => {
            let count = our_ratchet_secrets().len();
            delivery_log(&format!("ratchets loaded {count}"));
        }
        Ok(_) => delivery_log("ratchet record unreadable"),
        Err(error) => delivery_log(&format!("ratchet load failed: {error}")),
    }
}

fn remember_ratchet_secrets(sealed: &[u8]) -> bool {
    let Ok(record) = personal_rns::persistence::read_self_ratchets_snapshot(sealed) else {
        return false;
    };
    let secrets: Vec<_> = record.secrets_newest_first().collect();
    *OUR_RATCHETS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = secrets;
    true
}

fn our_ratchet_secrets() -> Vec<personal_rns::crypto::X25519SecretKey> {
    OUR_RATCHETS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .map(|secret| {
            secret.with_scalar_bytes(|bytes| personal_rns::crypto::X25519SecretKey::new(*bytes))
        })
        .collect()
}

async fn refresh_our_ratchets(handle: &PrnsNodeHandle) {
    let Some(destination) = OUR_DELIVERY.get().copied() else {
        return;
    };
    if let Ok(Some(snapshot)) = handle.snapshot_self_ratchet(destination).await {
        let _ = remember_ratchet_secrets(&snapshot.sealed);
    }
}

async fn keep_ratchets(
    handle: PrnsNodeHandle,
    vault_dir: PathBuf,
    mut rotated: tokio::sync::mpsc::UnboundedReceiver<DestinationHash>,
) {
    use personal_rns::identity::vault::IdentityVault;

    let mut vault = personal_rns::identity::vault::FileVault::new(vault_dir);
    while let Some(destination) = rotated.recv().await {
        match handle.snapshot_self_ratchet(destination).await {
            Ok(Some(snapshot)) => {
                if !remember_ratchet_secrets(&snapshot.sealed) {
                    delivery_log("ratchet record unreadable");
                    continue;
                }
                let label = personal_rns::runtime::self_ratchet_identity_label(&destination);
                match vault.store_blob(&label, &snapshot.sealed) {
                    Ok(()) => delivery_log("ratchet kept"),
                    Err(error) => delivery_log(&format!("ratchet keep failed: {error}")),
                }
            }
            Ok(None) => delivery_log("ratchet keep skipped: no record"),
            Err(error) => delivery_log(&format!("ratchet keep failed: {error:?}")),
        }
    }
}

fn unseal_for_us(bytes: &[u8]) -> Option<Vec<u8>> {
    let ours = signing_material()?;
    let delivery = OUR_DELIVERY.get()?;
    if bytes.len() < 16 || &bytes[..16] != delivery.as_bytes() {
        return None;
    }
    let secrets = our_ratchet_secrets();
    crate::mailbox::open_lxmf(&ours, delivery, bytes, &secrets).map(|opened| opened.payload)
}

async fn collect_mailbox() {
    if DELIVERING.load(Ordering::Acquire) {
        return;
    }
    let Some(mailbox) = mailbox_address() else {
        return;
    };
    let Some(handle) = HANDLE.get().cloned() else {
        return;
    };
    let Some(ours) = signing_material() else {
        return;
    };
    let Some(delivery) = OUR_DELIVERY.get().copied() else {
        return;
    };
    let Some(identity) = OURS.get().copied() else {
        return;
    };
    let attempt = async {
        let _ = seek_path(&handle, mailbox).await;
        let Ok(link) = handle.establish_link(mailbox).await else {
            return;
        };
        if handle.identify(link, identity).await.is_err() {
            handle.close_link(link);
            return;
        }
        let path = RequestPathHash::of("/get");
        let Ok((listed, _)) = handle
            .request(link, path, &crate::mailbox::list_query())
            .await
        else {
            handle.close_link(link);
            return;
        };
        let Some(ids) = crate::mailbox::read_ids(&listed) else {
            handle.close_link(link);
            return;
        };
        if ids.is_empty() {
            handle.close_link(link);
            return;
        }
        let Ok((bodies, _)) = handle
            .request(link, path, &crate::mailbox::fetch_query(&ids))
            .await
        else {
            handle.close_link(link);
            return;
        };
        let Some(messages) = crate::mailbox::read_bins(&bodies) else {
            handle.close_link(link);
            return;
        };
        refresh_our_ratchets(&handle).await;
        let secrets = our_ratchet_secrets();
        let mut haves = Vec::new();
        for message in &messages {
            haves.push(sha256(message));
            if let Some(opened) = crate::mailbox::open_lxmf(&ours, &delivery, message, &secrets) {
                stage_mailbox(&opened);
            }
        }
        if !haves.is_empty() {
            let _ = handle
                .request(link, path, &crate::mailbox::purge_query(&haves))
                .await;
        }
        handle.close_link(link);
    };
    let _ = tokio::time::timeout(Duration::from_secs(60), attempt).await;
}

fn stage_mailbox(opened: &crate::mailbox::OpenedMail) {
    if !signature_matches(opened) {
        return;
    }
    let identity = hex_encode(&opened.source);
    if let Some(digest) = crate::lxmf::open_ack(&opened.payload) {
        note_receipt_into(&mut mail(), digest);
        return;
    }
    if let Some(letter) = crate::lxmf::open_reaction(&opened.payload) {
        mail().marks.push(ArrivedReaction {
            identity,
            written_at: letter.written_at,
            words: letter.words,
            reaction: letter.reaction,
        });
        return;
    }
    if let Some(letter) = crate::lxmf::open_note(&opened.payload) {
        let digest = sha256(&opened.payload);
        let mut mail = mail();
        watch_arrived_file(&mut mail, None, &identity, &letter);
        mail.inbox
            .push(arrived_note(identity, letter, true, digest));
    }
}

fn signature_matches(opened: &crate::mailbox::OpenedMail) -> bool {
    for person in opening_contacts() {
        let Some(material) = contact_material(&person.identity) else {
            continue;
        };
        if material.identity_hash().as_bytes() != &opened.source {
            continue;
        }
        return material
            .verify(&opened.signed, &Ed25519Signature(opened.signature))
            .is_ok();
    }
    false
}

fn note_receipt_into(mail: &mut Mail, digest: [u8; 32]) {
    let id = sent_notes()
        .iter()
        .find(|(_, stored)| **stored == digest)
        .map(|(id, _)| *id);
    if let Some(id) = id {
        mail.acked.push(id);
    }
}

fn sent_notes() -> std::sync::MutexGuard<'static, HashMap<u64, [u8; 32]>> {
    static SENT: OnceLock<Mutex<HashMap<u64, [u8; 32]>>> = OnceLock::new();
    SENT.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Which of the four wires currently has a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireMarks {
    pub wifi: bool,
    pub internet: bool,
    pub ble: bool,
    pub usb: bool,
}

impl WireMarks {
    const fn quiet() -> Self {
        Self {
            wifi: false,
            internet: false,
            ble: false,
            usb: false,
        }
    }
}

/// Wi-Fi auto, the gateway, Bluetooth, and USB, read from the interfaces the node has up.
pub fn wire_marks() -> WireMarks {
    let Some(handle) = HANDLE.get() else {
        return WireMarks::quiet();
    };
    marks_from(&handle.interfaces())
}

fn marks_from(snapshots: &[personal_rns::interfaces::InterfaceSnapshot]) -> WireMarks {
    use personal_rns::interfaces::ConnectionState;

    let mut marks = WireMarks::quiet();
    for snapshot in snapshots {
        if !matches!(
            snapshot.connection,
            ConnectionState::Connected | ConnectionState::Degraded
        ) {
            continue;
        }
        match wire_of(snapshot.id) {
            Some(Wire::Wifi) => marks.wifi = true,
            Some(Wire::Internet) => marks.internet = true,
            Some(Wire::Bluetooth) => marks.ble = true,
            Some(Wire::Usb) => marks.usb = true,
            Some(Wire::Lora) | None => {}
        }
    }
    marks
}

/// The interface the stack is using for a contact, and what a sample on that path can carry.
pub use crate::model::Reach;

struct ReachState {
    generation: u64,
    path: Option<String>,
    hops: Option<u8>,
    ways: crate::model::OfferedWays,
    /// Round trip of the 400-byte sample, once one has been timed.
    round_trip_ms: Option<u64>,
    done: bool,
    /// The last time this journal read the stack's current path.
    looked_at: Instant,
    /// An announce or a removed route for this contact arrived after that read.
    due: bool,
    /// The path changed while a call was up, so the sample waits until the call is quiet.
    sample_due: bool,
    /// A read of the stack's current path, or the sample on it, is already running.
    looking: bool,
}

fn reach_map() -> std::sync::MutexGuard<'static, HashMap<String, ReachState>> {
    static MAP: OnceLock<Mutex<HashMap<String, ReachState>>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn reach_for(identity: &str) -> Reach {
    match reach_map().get(identity.trim()) {
        Some(state) => Reach {
            pending: !state.done,
            ways: state.ways,
            winner: state.path.clone(),
            hops: state.hops,
            file_limit: file_limit_of(state.path.as_deref().unwrap_or(""), state.round_trip_ms),
        },
        None => Reach {
            pending: false,
            ways: crate::model::OfferedWays::none(),
            winner: None,
            hops: None,
            file_limit: Some(crate::model::SMALLEST_ATTACHMENT),
        },
    }
}

pub fn forget(identity: &str) {
    reach_map().remove(identity.trim());
}

/// Lets the stack choose a path, names that interface, then times one sample on it.
/// One search per visit. [`watch`] follows later changes to that path.
pub fn measure(identity: &str) {
    let identity = identity.trim().to_string();
    if destination_for_contact(&identity).is_none() {
        return;
    }
    if !matches!(call_face(), CallFace::Quiet | CallFace::Failed { .. }) {
        return;
    }
    let generation = next_search();
    {
        let mut map = reach_map();
        if map.contains_key(&identity) {
            return;
        }
        map.insert(
            identity.clone(),
            ReachState {
                generation,
                path: None,
                hops: None,
                ways: crate::model::OfferedWays::none(),
                round_trip_ms: None,
                done: false,
                looked_at: Instant::now(),
                due: false,
                sample_due: false,
                looking: false,
            },
        );
    }
    let Some(runtime) = RUNTIME.get().cloned() else {
        finish_search(
            &identity,
            generation,
            crate::model::OfferedWays::none(),
            None,
        );
        return;
    };
    let Some(handle) = HANDLE.get().cloned() else {
        finish_search(
            &identity,
            generation,
            crate::model::OfferedWays::none(),
            None,
        );
        return;
    };
    let ours = OURS.get().copied();
    runtime.spawn(async move {
        let Some(destination) = destination_for_contact(&identity) else {
            finish_search(
                &identity,
                generation,
                crate::model::OfferedWays::none(),
                None,
            );
            return;
        };
        if tokio::time::timeout(STEP_WAIT, handle.request_path(destination))
            .await
            .is_ok()
        {
            if let Some(route) = handle.route(destination).await {
                remember_path(
                    &identity,
                    generation,
                    interface_name(route.interface),
                    route.hops,
                );
            }
        }
        let sample = send_probe(&handle, destination, ours).await;
        finish_search(
            &identity,
            generation,
            crate::model::ways_for_ack(sample),
            sample,
        );
    });
}

fn next_search() -> u64 {
    static SEARCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    SEARCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn remember_path(identity: &str, generation: u64, path: String, hops: u8) {
    let mut map = reach_map();
    let Some(state) = map.get_mut(identity) else {
        return;
    };
    if state.generation != generation || state.done {
        return;
    }
    state.path = Some(path);
    state.hops = Some(hops);
}

fn finish_search(
    identity: &str,
    generation: u64,
    ways: crate::model::OfferedWays,
    round_trip_ms: Option<u64>,
) {
    let mut map = reach_map();
    let Some(state) = map.get_mut(identity) else {
        return;
    };
    if state.generation != generation {
        return;
    }
    state.ways = ways;
    state.round_trip_ms = round_trip_ms;
    state.done = true;
    state.looked_at = Instant::now();
    state.due = false;
    state.sample_due = false;
    state.looking = false;
}

/// How often an open journal re-reads the path the stack is using, when no announce has arrived.
const ROUTE_LOOK: Duration = Duration::from_secs(2);

/// Re-reads the path the stack is using for an open journal.
/// A different path that is still a path gets a new sample, so note, voice, and video follow it.
pub fn watch(identity: &str) {
    let identity = identity.trim().to_string();
    let quiet = call_is_quiet();
    let generation = {
        let mut map = reach_map();
        let Some(state) = map.get_mut(&identity) else {
            return;
        };
        if !state.done || state.looking {
            return;
        }
        let route_due = state.due || state.looked_at.elapsed() >= ROUTE_LOOK;
        if !route_due && !(state.sample_due && quiet) {
            return;
        }
        state.due = false;
        state.looking = true;
        state.looked_at = Instant::now();
        state.generation
    };
    let Some(runtime) = RUNTIME.get().cloned() else {
        apply_watched_route(&identity, generation, RouteLook::Skipped, true);
        return;
    };
    let Some(handle) = HANDLE.get().cloned() else {
        apply_watched_route(&identity, generation, RouteLook::Skipped, true);
        return;
    };
    let ours = OURS.get().copied();
    runtime.spawn(async move {
        let look = match destination_for_contact(&identity) {
            Some(destination) => match handle.route(destination).await {
                Some(route) => RouteLook::Known(interface_name(route.interface), route.hops),
                None => RouteLook::Gone,
            },
            None => RouteLook::Skipped,
        };
        let follow = apply_watched_route(&identity, generation, look, call_is_quiet());
        if !matches!(follow, SampleNext::Now) {
            return;
        }
        if !call_is_quiet() {
            postpone_sample(&identity, generation);
            return;
        }
        let sample = match destination_for_contact(&identity) {
            Some(destination) => send_probe(&handle, destination, ours).await,
            None => None,
        };
        finish_sample(
            &identity,
            generation,
            crate::model::ways_for_ack(sample),
            sample,
        );
    });
}

enum RouteLook {
    Known(String, u8),
    Gone,
    Skipped,
}

enum SampleNext {
    Now,
    AfterCall,
    None,
}

/// A known path that is not the one already shown needs a new sample. An isolated path does not.
fn path_needs_sample(path: Option<&str>, hops: Option<u8>, look: &RouteLook) -> bool {
    let RouteLook::Known(next, next_hops) = look else {
        return false;
    };
    path != Some(next.as_str()) || hops != Some(*next_hops)
}

fn apply_watched_route(
    identity: &str,
    generation: u64,
    look: RouteLook,
    quiet: bool,
) -> SampleNext {
    let mut map = reach_map();
    let Some(state) = map.get_mut(identity) else {
        return SampleNext::None;
    };
    if state.generation != generation || !state.done {
        return SampleNext::None;
    }
    let changed = path_needs_sample(state.path.as_deref(), state.hops, &look);
    let wanted = changed || (state.sample_due && matches!(look, RouteLook::Known(_, _)));
    match look {
        RouteLook::Known(path, hops) => {
            if state.path.as_deref() != Some(path.as_str()) {
                state.round_trip_ms = None;
            }
            state.path = Some(path);
            state.hops = Some(hops);
        }
        RouteLook::Gone => {
            state.path = None;
            state.hops = None;
            state.round_trip_ms = None;
            state.sample_due = false;
            state.looking = false;
            return SampleNext::None;
        }
        RouteLook::Skipped => {
            state.looking = false;
            return SampleNext::None;
        }
    }
    if !wanted {
        state.looking = false;
        return SampleNext::None;
    }
    if !quiet {
        state.sample_due = true;
        state.looking = false;
        return SampleNext::AfterCall;
    }
    state.sample_due = false;
    state.ways = crate::model::OfferedWays::none();
    state.looking = true;
    SampleNext::Now
}

fn postpone_sample(identity: &str, generation: u64) {
    let mut map = reach_map();
    let Some(state) = map.get_mut(identity) else {
        return;
    };
    if state.generation != generation || !state.done {
        return;
    }
    state.sample_due = true;
    state.looking = false;
}

fn finish_sample(
    identity: &str,
    generation: u64,
    ways: crate::model::OfferedWays,
    round_trip_ms: Option<u64>,
) {
    let mut map = reach_map();
    let Some(state) = map.get_mut(identity) else {
        return;
    };
    if state.generation != generation || !state.done {
        return;
    }
    state.ways = ways;
    state.round_trip_ms = round_trip_ms;
    state.looking = false;
    state.looked_at = Instant::now();
}

fn call_is_quiet() -> bool {
    matches!(call_face(), CallFace::Quiet | CallFace::Failed { .. })
}

fn note_route_churn(destination: DestinationHash) {
    let mut map = reach_map();
    for (identity, state) in map.iter_mut() {
        if !state.done {
            continue;
        }
        let Some(theirs) = destination_for_contact(identity) else {
            continue;
        };
        if theirs == destination {
            state.due = true;
        }
    }
}

/// How long the journal waits on a path request, and on the sample, before it moves on.
const STEP_WAIT: Duration = Duration::from_secs(5);

async fn seek_path(handle: &PrnsNodeHandle, destination: personal_rns::wire::DestinationHash) {
    let _ = handle.request_path(destination).await;
}

fn interface_name(interface: personal_rns::interfaces::InterfaceId) -> String {
    match wire_of(interface) {
        Some(Wire::Wifi) => "Wi-Fi",
        Some(Wire::Internet) => "Internet",
        Some(Wire::Bluetooth) => "Bluetooth",
        Some(Wire::Usb) => "USB",
        Some(Wire::Lora) => "LoRa",
        None => "Link",
    }
    .to_string()
}

fn file_limit_of(path: &str, round_trip_ms: Option<u64>) -> Option<u64> {
    match carry_for_path(path, round_trip_ms) {
        crate::model::ResourceCarry::Open => None,
        crate::model::ResourceCarry::Limited(bytes) => Some(bytes),
    }
}

fn carry_for_identity(identity: &str) -> crate::model::ResourceCarry {
    let map = reach_map();
    let Some(state) = map.get(identity) else {
        return crate::model::resource_carry(None, false);
    };
    let path = state.path.clone().unwrap_or_default();
    carry_for_path(&path, state.round_trip_ms)
}

fn carry_for_hash(identity: &IdentityHash) -> crate::model::ResourceCarry {
    let hex = hex_encode(identity.as_bytes());
    let found = reach_map().iter().find_map(|(stored, state)| {
        same_contact(stored, &hex)
            .then(|| (state.path.clone().unwrap_or_default(), state.round_trip_ms))
    });
    match found {
        Some((path, sample)) => carry_for_path(&path, sample),
        None => crate::model::resource_carry(None, false),
    }
}

/// A measured sample wins. Without one, a fast wire is treated as open until the sample returns, Bluetooth takes a middle step, and anything else stays small. LoRa never leaves the smallest attachment.
fn carry_for_path(path: &str, round_trip_ms: Option<u64>) -> crate::model::ResourceCarry {
    if path == "LoRa" {
        return crate::model::resource_carry(None, true);
    }
    if let Some(round_trip_ms) = round_trip_ms {
        return crate::model::resource_carry(crate::model::bits_per_second(round_trip_ms), false);
    }
    match path {
        "Wi-Fi" | "Internet" | "USB" => crate::model::ResourceCarry::Open,
        "Bluetooth" => crate::model::resource_carry(Some(32_000), false),
        _ => crate::model::resource_carry(None, false),
    }
}

fn carry_label(carry: crate::model::ResourceCarry) -> String {
    match carry {
        crate::model::ResourceCarry::Open => "no limit".to_string(),
        crate::model::ResourceCarry::Limited(bytes) => format!("{bytes} bytes"),
    }
}

fn admit_link(link_id: LinkId, carry: crate::model::ResourceCarry) {
    let Some(handle) = HANDLE.get() else {
        return;
    };
    let max_uncompressed_bytes = match carry {
        crate::model::ResourceCarry::Open => u64::MAX,
        crate::model::ResourceCarry::Limited(bytes) => bytes,
    };
    let _ = handle.admit_resource_offers(
        link_id,
        personal_rns::runtime::ResourceOfferAdmission {
            peer: personal_rns::runtime::ResourceAdmissionPeer::Any,
            max_uncompressed_bytes,
            accept_compressed: true,
        },
    );
}

enum Wire {
    Wifi,
    Internet,
    Bluetooth,
    Usb,
    Lora,
}

/// The USB cable is attached under a fixed id that carries no kind byte.
const MAC_USB: personal_rns::interfaces::InterfaceId =
    personal_rns::interfaces::InterfaceId::new([0xD0; 8]);
const PHONE_USB: personal_rns::interfaces::InterfaceId =
    personal_rns::interfaces::InterfaceId::new([0xD1; 8]);

/// Wi-Fi auto's own sockets, including the TCP sessions it dials and accepts, are Wi-Fi.
/// The gateway client is the one Internet socket. The USB cable is the fixed id above.
fn wire_of(interface: personal_rns::interfaces::InterfaceId) -> Option<Wire> {
    use personal_rns::interfaces::InterfaceKind;
    if interface == MAC_USB || interface == PHONE_USB {
        return Some(Wire::Usb);
    }
    match interface.kind() {
        Some(InterfaceKind::AutoWifi | InterfaceKind::WifiPeer | InterfaceKind::TcpServerPeer) => {
            Some(Wire::Wifi)
        }
        Some(InterfaceKind::TcpClient) if is_gateway_client(interface) => Some(Wire::Internet),
        Some(InterfaceKind::TcpClient) => Some(Wire::Wifi),
        Some(InterfaceKind::TcpServer) => Some(Wire::Internet),
        Some(InterfaceKind::BluetoothAuto | InterfaceKind::BluetoothPeer) => Some(Wire::Bluetooth),
        Some(InterfaceKind::UsbAutoHost | InterfaceKind::UsbAutoDevice) => Some(Wire::Usb),
        Some(InterfaceKind::LoRa | InterfaceKind::Rnode) => Some(Wire::Lora),
        _ => None,
    }
}

fn is_gateway_client(interface: personal_rns::interfaces::InterfaceId) -> bool {
    gateway_client_id().is_some_and(|gateway| interface == gateway)
}

fn gateway_client_id() -> Option<personal_rns::interfaces::InterfaceId> {
    let target = tcp_dial_target(&default_directory());
    Some(personal_rns::interfaces::InterfaceId::from_channel_tag(
        personal_rns::interfaces::InterfaceKind::TcpClient,
        target.as_bytes(),
    ))
}

async fn send_probe(
    handle: &PrnsNodeHandle,
    destination: personal_rns::wire::DestinationHash,
    ours: Option<personal_rns::identity::IdentityHash>,
) -> Option<u64> {
    let opened = std::sync::Mutex::new(None);
    let measured = async {
        let link = handle.establish_link(destination).await.ok()?;
        *opened
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(link);
        if let Some(ours) = ours {
            let _ = handle.identify(link, ours).await;
        }
        let sent = handle.send_link_packet(link, &probe_packet()).await;
        sent.ok().map(|receipt| receipt.rtt.millis())
    };
    let measured = tokio::time::timeout(STEP_WAIT, measured)
        .await
        .ok()
        .flatten();
    if let Some(link) = opened
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        handle.close_link(link);
    }
    measured
}

fn probe_packet() -> Vec<u8> {
    let mut packet = vec![0u8; 400];
    packet[0] = b'P';
    packet
}

/// Notes that arrived on a link. The contact list decides which ones stay.
pub fn drain_inbox() -> Vec<ArrivedNote> {
    std::mem::take(&mut mail().inbox)
}

/// Reactions that arrived on a link. The contact list matches them to a note.
pub fn drain_reactions() -> Vec<ArrivedReaction> {
    std::mem::take(&mut mail().marks)
}

/// Notes the other side has acknowledged.
pub fn drain_acks() -> Vec<u64> {
    std::mem::take(&mut mail().acked)
}

/// Notes the mailbox accepted, still waiting for the other person.
pub fn drain_posted() -> Vec<(u64, [u8; 32])> {
    std::mem::take(&mut mail().posted)
}

/// Notes that were tried three times and will not send themselves again.
pub fn drain_failures() -> Vec<u64> {
    std::mem::take(&mut mail().failed)
}

/// Files whose transfer failed after the name had already arrived.
pub fn drain_incoming_failures() -> Vec<FailedAttachment> {
    std::mem::take(&mut mail().incoming_failed)
}

pub fn same_contact(stored_identity: &str, identity_hash_hex: &str) -> bool {
    let stored = stored_identity.trim();
    if stored.eq_ignore_ascii_case(identity_hash_hex) {
        return true;
    }
    let Some(bytes) = decode_hex(stored) else {
        return false;
    };
    let Ok(material) = PublicIdentityMaterial::from_slice(&bytes) else {
        return false;
    };
    hex_encode(material.identity_hash().as_bytes()).eq_ignore_ascii_case(identity_hash_hex)
}

/// Yields without parking the interface thread. The journal runtime owns the timer.
pub async fn pause() {
    rest(250).await;
}

/// Yields for `millis` without parking the interface thread.
pub async fn rest(millis: u64) {
    let Some(runtime) = RUNTIME.get() else {
        std::thread::sleep(Duration::from_millis(millis));
        return;
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    runtime.spawn(async move {
        tokio::time::sleep(Duration::from_millis(millis)).await;
        let _ = tx.send(());
    });
    let _ = rx.await;
}

fn on_journal_event(event: PrnsEvent<'_>, _: &NoRemoteControlHostControls) {
    match event {
        PrnsEvent::Diagnostic(Diagnostic::LinkEstablished(link)) => {
            delivery_log(&format!(
                "link up {:?} rtt={}ms",
                link.link_id, link.rtt_millis
            ));
            admit_link(
                link.link_id,
                crate::model::ResourceCarry::Limited(crate::model::SMALLEST_ATTACHMENT),
            );
        }
        PrnsEvent::Diagnostic(Diagnostic::LinkClosed { link_id, reason }) => {
            let held = {
                let mail = mail();
                let parts = mail
                    .resources
                    .iter()
                    .filter(|((id, _), _)| *id == link_id)
                    .map(|(_, incoming)| {
                        let got = incoming.parts.iter().filter(|part| part.is_some()).count();
                        format!("{got}/{}", incoming.parts.len())
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let waiting = mail
                    .waiting
                    .get(&link_id)
                    .map(|notes| notes.len())
                    .unwrap_or(0);
                (parts, waiting)
            };
            delivery_log(&format!(
                "link down {link_id:?} {reason:?} parts={} waiting_notes={}",
                if held.0.is_empty() {
                    "-".to_string()
                } else {
                    held.0
                },
                held.1
            ));
            if let Some(handle) = HANDLE.get() {
                handle.deny_resource_offers(link_id);
            }
            {
                let mut mail = mail();
                fail_open_files(&mut mail, link_id);
                mail.resources.retain(|(id, _), _| *id != link_id);
            }
        }
        PrnsEvent::Diagnostic(Diagnostic::LinkInterfaceMismatch {
            link_id,
            attached_interface,
            arrived_on,
        }) => {
            delivery_log(&format!(
                "link {link_id:?} packet arrived on {} but the link is on {}",
                interface_name(arrived_on),
                interface_name(attached_interface)
            ));
        }
        PrnsEvent::Diagnostic(Diagnostic::ResourceFailed { link_id, cause, .. }) => {
            delivery_log(&format!("resource failed on {link_id:?} {cause:?}"));
            fail_open_files(&mut mail(), link_id);
        }
        PrnsEvent::Diagnostic(Diagnostic::ResourceAssembled {
            link_id,
            total_size_bytes,
            ..
        }) => {
            delivery_log(&format!(
                "resource assembled on {link_id:?} {total_size_bytes} bytes"
            ));
        }
        PrnsEvent::Diagnostic(Diagnostic::PeerIdentified { link_id, identity }) => {
            remember_peer(link_id, identity);
        }
        PrnsEvent::Diagnostic(Diagnostic::SelfRatchetRotated { destination }) => {
            if let Some(sender) = RATCHET_ROTATED.get() {
                let _ = sender.send(destination);
            }
        }
        PrnsEvent::Diagnostic(Diagnostic::AnnounceHeard { destination, .. })
        | PrnsEvent::Diagnostic(Diagnostic::RouteRemoved { destination, .. }) => {
            note_route_churn(destination);
        }
        PrnsEvent::Message(Message::Delivered(Delivery::Link(delivery))) => {
            accept_words(delivery.link_id, delivery.plaintext);
        }
        PrnsEvent::Message(Message::Resource { link_id, data, .. }) => {
            accept_words(link_id, data);
        }
        PrnsEvent::Message(Message::ResourceSegment {
            link_id,
            original_hash,
            segment_index,
            total_segments,
            data,
            ..
        }) => {
            accept_resource_piece(link_id, original_hash, segment_index, total_segments, data);
        }
        _ => {}
    }
}

#[cfg(test)]
fn empty_mail() -> Mail {
    Mail {
        peers: HashMap::new(),
        waiting: HashMap::new(),
        waiting_marks: HashMap::new(),
        inbox: Vec::new(),
        marks: Vec::new(),
        acked: Vec::new(),
        failed: Vec::new(),
        posted: Vec::new(),
        waiting_parts: HashMap::new(),
        assemblies: HashMap::new(),
        resources: HashMap::new(),
        open_files: Vec::new(),
        incoming_failed: Vec::new(),
        doomed_waits: Vec::new(),
    }
}

fn mail() -> std::sync::MutexGuard<'static, Mail> {
    MAIL.get_or_init(|| {
        Mutex::new(Mail {
            peers: HashMap::new(),
            waiting: HashMap::new(),
            waiting_marks: HashMap::new(),
            inbox: Vec::new(),
            marks: Vec::new(),
            acked: Vec::new(),
            failed: Vec::new(),
            posted: Vec::new(),
            waiting_parts: HashMap::new(),
            assemblies: HashMap::new(),
            resources: HashMap::new(),
            open_files: Vec::new(),
            incoming_failed: Vec::new(),
            doomed_waits: Vec::new(),
        })
    })
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn remember_peer(link_id: LinkId, identity: IdentityHash) {
    let identity_hex = hex_encode(identity.as_bytes());
    {
        let mut mail = mail();
        mail.peers.insert(link_id, identity);
        if let Some(waiting) = mail.waiting.remove(&link_id) {
            for letter in waiting {
                if take_doomed_wait(&mut mail, link_id, &letter) {
                    if let Some(file) = letter.file.as_ref().filter(|file| file.bytes.is_empty()) {
                        mail.incoming_failed.push(FailedAttachment {
                            identity: identity_hex.clone(),
                            written_at: letter.written_at,
                            file_name: file.name.clone(),
                        });
                    }
                } else {
                    watch_arrived_file(&mut mail, Some(link_id), &identity_hex, &letter);
                }
                mail.inbox
                    .push(arrived_note(identity_hex.clone(), letter, false, [0; 32]));
            }
        }
        if let Some(waiting) = mail.waiting_marks.remove(&link_id) {
            for letter in waiting {
                mail.marks.push(ArrivedReaction {
                    identity: identity_hex.clone(),
                    written_at: letter.written_at,
                    words: letter.words,
                    reaction: letter.reaction,
                });
            }
        }
        if let Some(parts) = mail.waiting_parts.remove(&link_id) {
            let mut wholes = Vec::new();
            for fragment in parts {
                if let Some(whole) = offer_piece(&mut mail, &identity_hex, fragment) {
                    wholes.push(whole);
                }
            }
            for whole in wholes {
                stage_message(&mut mail, &identity_hex, &whole, link_id);
            }
        }
    }
    note_peer_on_call(link_id, &identity_hex);
    admit_link(link_id, carry_for_hash(&identity));
}

fn accept_words(link_id: LinkId, bytes: &[u8]) {
    if let Some(plain) = unseal_for_us(bytes) {
        accept_message(link_id, &plain);
        return;
    }
    if bytes.first() == Some(&b'V') {
        accept_voice(link_id, bytes);
        return;
    }
    if bytes.first() == Some(&b'F') {
        #[cfg(any(target_os = "macos", target_os = "android"))]
        accept_film(link_id, bytes);
        return;
    }
    if let Some(fragment) = crate::lxmf::open_fragment(bytes) {
        accept_fragment(link_id, fragment);
        return;
    }
    accept_message(link_id, bytes);
}

fn accept_resource_piece(
    link_id: LinkId,
    hash: personal_rns::routing::links::resources::ResourceHash,
    index: u64,
    total: u64,
    data: &[u8],
) {
    let Some(slot_index) = segment_slot(index, total) else {
        return;
    };
    let key = (link_id, *hash.as_bytes());
    let mut mail = mail();
    let incoming = mail
        .resources
        .entry(key)
        .or_insert_with(|| IncomingResource {
            total,
            parts: vec![None; total as usize],
        });
    if incoming.total != total {
        return;
    }
    if let Some(slot) = incoming.parts.get_mut(slot_index) {
        if slot.is_none() {
            *slot = Some(data.to_vec());
        }
    }
    if incoming.parts.iter().any(|part| part.is_none()) {
        return;
    }
    let Some(done) = mail.resources.remove(&key) else {
        return;
    };
    drop(mail);
    let mut whole = Vec::new();
    for part in done.parts.into_iter().flatten() {
        whole.extend(part);
    }
    delivery_log(&format!(
        "joined {} bytes on {link_id:?} from {total} segments",
        whole.len()
    ));
    accept_words(link_id, &whole);
}

fn accept_fragment(link_id: LinkId, fragment: crate::lxmf::Fragment) {
    let mut mail = mail();
    let Some(identity) = mail.peers.get(&link_id).copied() else {
        mail.waiting_parts
            .entry(link_id)
            .or_default()
            .push(fragment);
        return;
    };
    let identity_hex = hex_encode(identity.as_bytes());
    let Some(whole) = offer_piece(&mut mail, &identity_hex, fragment) else {
        return;
    };
    stage_message(&mut mail, &identity_hex, &whole, link_id);
}

fn offer_piece(
    mail: &mut Mail,
    identity: &str,
    fragment: crate::lxmf::Fragment,
) -> Option<Vec<u8>> {
    let key = (identity.to_string(), fragment.group);
    let whole = mail
        .assemblies
        .entry(key.clone())
        .or_default()
        .offer(fragment)?;
    mail.assemblies.remove(&key);
    Some(whole)
}

fn stage_message(mail: &mut Mail, identity_hex: &str, bytes: &[u8], link_id: LinkId) {
    if let Some(plain) = unseal_for_us(bytes) {
        stage_plain(mail, identity_hex, &plain, link_id);
        return;
    }
    stage_plain(mail, identity_hex, bytes, link_id);
}

fn stage_plain(mail: &mut Mail, identity_hex: &str, bytes: &[u8], link_id: LinkId) {
    if let Some(digest) = crate::lxmf::open_ack(bytes) {
        note_receipt_into(mail, digest);
        return;
    }
    if let Some(letter) = crate::lxmf::open_reaction(bytes) {
        mail.marks.push(ArrivedReaction {
            identity: identity_hex.to_string(),
            written_at: letter.written_at,
            words: letter.words,
            reaction: letter.reaction,
        });
        return;
    }
    if let Some(letter) = crate::lxmf::open_note(bytes) {
        watch_arrived_file(mail, Some(link_id), identity_hex, &letter);
        mail.inbox.push(arrived_note(
            identity_hex.to_string(),
            letter,
            false,
            [0; 32],
        ));
    }
}

fn accept_message(link_id: LinkId, bytes: &[u8]) {
    if let Some(digest) = crate::lxmf::open_ack(bytes) {
        note_receipt_into(&mut mail(), digest);
        return;
    }
    if let Some(letter) = crate::lxmf::open_reaction(bytes) {
        let mut mail = mail();
        if let Some(identity) = mail.peers.get(&link_id).copied() {
            mail.marks.push(ArrivedReaction {
                identity: hex_encode(identity.as_bytes()),
                written_at: letter.written_at,
                words: letter.words,
                reaction: letter.reaction,
            });
        } else {
            mail.waiting_marks.entry(link_id).or_default().push(letter);
        }
        return;
    }
    let Some(letter) = crate::lxmf::open_note(bytes) else {
        if bytes.len() > 500 {
            delivery_log(&format!(
                "arrived {} bytes on {link_id:?} and it was not a note",
                bytes.len()
            ));
        }
        return;
    };
    let file = letter
        .file
        .as_ref()
        .map(|file| (file.name.clone(), file.bytes.len()));
    let mut mail = mail();
    if let Some(identity) = mail.peers.get(&link_id).copied() {
        if let Some((name, len)) = &file {
            delivery_log(&format!(
                "arrived note file {name} {len} bytes on {link_id:?}"
            ));
        }
        let identity_hex = hex_encode(identity.as_bytes());
        watch_arrived_file(&mut mail, Some(link_id), &identity_hex, &letter);
        mail.inbox
            .push(arrived_note(identity_hex, letter, false, [0; 32]));
    } else {
        delivery_log(&format!(
            "arrived note on {link_id:?} waiting for who they are{}",
            file.map(|(name, len)| format!(", file {name} {len} bytes"))
                .unwrap_or_default()
        ));
        mail.waiting.entry(link_id).or_default().push(letter);
    }
}

fn watch_arrived_file(
    mail: &mut Mail,
    link_id: Option<LinkId>,
    identity: &str,
    letter: &crate::lxmf::NoteLetter,
) {
    let Some(file) = &letter.file else {
        return;
    };
    let same = |open: &OpenFile| {
        open.identity == identity
            && open.written_at.to_bits() == letter.written_at.to_bits()
            && open.file_name == file.name
    };
    if !file.bytes.is_empty() {
        mail.open_files.retain(|open| !same(open));
        mail.incoming_failed.retain(|failed| {
            !(failed.identity == identity
                && failed.written_at.to_bits() == letter.written_at.to_bits()
                && failed.file_name == file.name)
        });
        return;
    }
    let Some(link_id) = link_id else {
        return;
    };
    mail.incoming_failed.retain(|failed| {
        !(failed.identity == identity
            && failed.written_at.to_bits() == letter.written_at.to_bits()
            && failed.file_name == file.name)
    });
    if let Some(open) = mail.open_files.iter_mut().find(|open| same(open)) {
        open.link_id = link_id;
        return;
    }
    mail.open_files.push(OpenFile {
        link_id,
        identity: identity.to_string(),
        written_at: letter.written_at,
        file_name: file.name.clone(),
    });
}

fn fail_open_files(mail: &mut Mail, link_id: LinkId) {
    let doomed: Vec<DoomedWait> = mail
        .waiting
        .get(&link_id)
        .into_iter()
        .flatten()
        .filter_map(|letter| {
            let file = letter.file.as_ref().filter(|file| file.bytes.is_empty())?;
            Some(DoomedWait {
                link_id,
                written_at: letter.written_at,
                file_name: file.name.clone(),
            })
        })
        .collect();
    mail.doomed_waits.extend(doomed);
    let mut kept = Vec::new();
    for open in mail.open_files.drain(..) {
        if open.link_id == link_id {
            mail.incoming_failed.push(FailedAttachment {
                identity: open.identity,
                written_at: open.written_at,
                file_name: open.file_name,
            });
        } else {
            kept.push(open);
        }
    }
    mail.open_files = kept;
}

fn take_doomed_wait(mail: &mut Mail, link_id: LinkId, letter: &crate::lxmf::NoteLetter) -> bool {
    let Some(file) = letter.file.as_ref() else {
        return false;
    };
    let before = mail.doomed_waits.len();
    mail.doomed_waits.retain(|wait| {
        !(wait.link_id == link_id
            && wait.written_at.to_bits() == letter.written_at.to_bits()
            && wait.file_name == file.name)
    });
    mail.doomed_waits.len() != before
}

fn contact_material(stored_identity: &str) -> Option<PublicIdentityMaterial> {
    let bytes = decode_hex(stored_identity.trim())?;
    PublicIdentityMaterial::from_slice(&bytes).ok()
}

fn address_for(material: &PublicIdentityMaterial) -> Option<DestinationHash> {
    derive_single_destination_hash(&material.identity_hash(), DELIVERY_APP, DELIVERY_ASPECTS).ok()
}

fn destination_for_contact(stored_identity: &str) -> Option<DestinationHash> {
    address_for(&contact_material(stored_identity)?)
}

/// What the open journal shows for the voice button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallFace {
    Quiet,
    /// `outgoing` is the side that placed the call. The other side is answering.
    Placing {
        identity: String,
        outgoing: bool,
    },
    Live {
        identity: String,
    },
    Failed {
        identity: String,
        words: &'static str,
    },
    /// A contact is calling, and nobody has answered yet.
    Ringing {
        identity: String,
        ticket: u64,
    },
}

/// A voice call that ended, ready to be written into the thread.
pub struct FinishedCall {
    pub identity: String,
    pub seconds: u64,
    pub outgoing: bool,
}

/// Someone in the contacts is asking to talk.
pub struct IncomingCall {
    pub ticket: u64,
    pub identity: String,
}

struct Call {
    face: CallFace,
    /// The open call is a picture, not voice.
    video: bool,
    generation: u64,
    outgoing: bool,
    /// The other person has answered. Audio stays quiet until then.
    answered: bool,
    started: Option<Instant>,
    link: Option<LinkId>,
    finished: Option<FinishedCall>,
    invites: Vec<IncomingCall>,
    next_ticket: u64,
    tickets: HashMap<u64, LinkId>,
    held: HashMap<LinkId, Held>,
}

#[derive(Default)]
struct Held {
    identity: Option<String>,
    hello: bool,
    invited: bool,
    /// The buffered packets are picture frames, not voice.
    video: bool,
    packets: Vec<Vec<u8>>,
}

static CALL: OnceLock<Mutex<Call>> = OnceLock::new();

fn calls() -> std::sync::MutexGuard<'static, Call> {
    CALL.get_or_init(|| {
        Mutex::new(Call {
            face: CallFace::Quiet,
            video: false,
            generation: 0,
            outgoing: false,
            answered: false,
            started: None,
            link: None,
            finished: None,
            invites: Vec::new(),
            next_ticket: 1,
            tickets: HashMap::new(),
            held: HashMap::new(),
        })
    })
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn call_face() -> CallFace {
    calls().face.clone()
}

pub fn call_is_video() -> bool {
    calls().video
}

#[cfg(any(target_os = "macos", target_os = "android"))]
pub fn video_picture() -> Option<String> {
    crate::video::picture()
}

/// The outbound side is still waiting for the other person to accept or reject.
pub fn caller_is_waiting() -> bool {
    let call = calls();
    !call.answered && matches!(call.face, CallFace::Placing { outgoing: true, .. })
}

pub fn drain_invites() -> Vec<IncomingCall> {
    std::mem::take(&mut calls().invites)
}

pub fn take_finished() -> Option<FinishedCall> {
    calls().finished.take()
}

/// Opens the microphone and a link to this contact, then keeps sending voice.
pub fn place_call(identity: &str) {
    let identity = identity.trim().to_string();
    let Some(destination) = destination_for_contact(&identity) else {
        return;
    };
    let Some(runtime) = RUNTIME.get().cloned() else {
        fail_now(&identity, crate::voice::DIDNT_CONNECT);
        return;
    };
    let Some(handle) = HANDLE.get().cloned() else {
        fail_now(&identity, crate::voice::DIDNT_CONNECT);
        return;
    };
    let Some(generation) = begin_call(&identity, true, false) else {
        return;
    };
    let ours = OURS.get().copied();
    runtime.spawn(async move {
        if !prepare_voice(generation, &identity, false).await {
            return;
        }
        let _ = seek_path(&handle, destination).await;
        if !still(generation) {
            stop_voice();
            return;
        }
        let Ok(link) = handle.establish_link(destination).await else {
            stop_voice();
            fail_if(generation, &identity, crate::voice::DIDNT_CONNECT);
            return;
        };
        if let Some(ours) = ours {
            let _ = handle.identify(link, ours).await;
        }
        if !bind_link(generation, link) {
            stop_voice();
            let _ = handle
                .send_link_packet(link, &crate::voice::bye_packet())
                .await;
            return;
        }
        let hello = handle
            .send_link_packet(link, &crate::voice::hello_packet())
            .await;
        if !call_can_continue(hello) {
            stop_voice();
            fail_if(generation, &identity, crate::voice::DIDNT_CONNECT);
            let _ = handle
                .send_link_packet(link, &crate::voice::bye_packet())
                .await;
            return;
        }
        loop {
            if !still(generation) {
                stop_voice();
                let _ = handle
                    .send_link_packet(link, &crate::voice::bye_packet())
                    .await;
                return;
            }
            if crate::voice::stopped() {
                fail_if(generation, &identity, crate::voice::DIDNT_CONNECT);
                let _ = handle
                    .send_link_packet(link, &crate::voice::bye_packet())
                    .await;
                return;
            }
            if peer_answered(generation) {
                break;
            }
            rest(50).await;
        }
        release_held(link);
        if !mark_live(generation, &identity, link) {
            stop_voice();
            let _ = handle
                .send_link_packet(link, &crate::voice::bye_packet())
                .await;
            return;
        }
        speak(handle, link, generation, identity, true).await;
    });
}

#[cfg(any(target_os = "macos", target_os = "android"))]
pub fn place_video(identity: &str) {
    let identity = identity.trim().to_string();
    let Some(destination) = destination_for_contact(&identity) else {
        return;
    };
    let Some(runtime) = RUNTIME.get().cloned() else {
        fail_now(&identity, crate::voice::DIDNT_CONNECT);
        return;
    };
    let Some(handle) = HANDLE.get().cloned() else {
        fail_now(&identity, crate::voice::DIDNT_CONNECT);
        return;
    };
    let Some(generation) = begin_call(&identity, true, true) else {
        return;
    };
    let ours = OURS.get().copied();
    runtime.spawn(async move {
        if !prepare_voice(generation, &identity, true).await {
            return;
        }
        if !prepare_video(generation, &identity).await {
            return;
        }
        let _ = seek_path(&handle, destination).await;
        if !still(generation) {
            stop_voice();
            return;
        }
        let Ok(link) = handle.establish_link(destination).await else {
            stop_voice();
            fail_if(generation, &identity, crate::voice::DIDNT_CONNECT);
            return;
        };
        if let Some(ours) = ours {
            let _ = handle.identify(link, ours).await;
        }
        if !bind_link(generation, link) {
            stop_voice();
            let _ = handle
                .send_link_packet(link, &crate::video::bye_packet())
                .await;
            return;
        }
        let hello = handle
            .send_link_packet(link, &crate::video::hello_packet())
            .await;
        if !call_can_continue(hello) {
            stop_voice();
            fail_if(generation, &identity, crate::voice::DIDNT_CONNECT);
            let _ = handle
                .send_link_packet(link, &crate::video::bye_packet())
                .await;
            return;
        }
        loop {
            if !still(generation) {
                stop_voice();
                let _ = handle
                    .send_link_packet(link, &crate::video::bye_packet())
                    .await;
                return;
            }
            if crate::video::stopped() {
                fail_if(generation, &identity, crate::voice::DIDNT_CONNECT);
                let _ = handle
                    .send_link_packet(link, &crate::video::bye_packet())
                    .await;
                return;
            }
            if peer_answered(generation) {
                break;
            }
            rest(50).await;
        }
        release_held(link);
        if !mark_live(generation, &identity, link) {
            stop_voice();
            let _ = handle
                .send_link_packet(link, &crate::video::bye_packet())
                .await;
            return;
        }
        crate::voice::stage_hello();
        show_and_speak(handle, link, generation, identity, true).await;
    });
}

pub fn answer(ticket: u64) {
    let taken = {
        let mut call = calls();
        let Some(link) = call.tickets.remove(&ticket) else {
            return;
        };
        call.invites.retain(|invite| invite.ticket != ticket);
        if !matches!(
            call.face,
            CallFace::Quiet | CallFace::Failed { .. } | CallFace::Ringing { .. }
        ) {
            Some(Err(link))
        } else {
            let held = call.held.remove(&link).unwrap_or_default();
            let identity = held.identity.clone().unwrap_or_default();
            let video = held.video;
            call.generation = call.generation.wrapping_add(1);
            let generation = call.generation;
            call.outgoing = false;
            call.video = video;
            call.answered = true;
            call.started = None;
            call.link = Some(link);
            call.face = CallFace::Placing {
                identity: identity.clone(),
                outgoing: false,
            };
            Some(Ok((link, identity, generation, video)))
        }
    };
    match taken {
        Some(Err(link)) => send_bye(link),
        Some(Ok((link, identity, generation, video))) => {
            let Some(runtime) = RUNTIME.get().cloned() else {
                fail_if(generation, &identity, crate::voice::DIDNT_CONNECT);
                return;
            };
            let Some(handle) = HANDLE.get().cloned() else {
                fail_if(generation, &identity, crate::voice::DIDNT_CONNECT);
                return;
            };
            runtime.spawn(async move {
                if video {
                    #[cfg(any(target_os = "macos", target_os = "android"))]
                    answer_film(handle, link, generation, identity).await;
                    #[cfg(not(any(target_os = "macos", target_os = "android")))]
                    {
                        let _ = (handle, generation, identity);
                        send_bye(link);
                    }
                    return;
                }
                if !prepare_voice(generation, &identity, false).await {
                    send_bye(link);
                    return;
                }
                if !still(generation) {
                    stop_voice();
                    send_bye(link);
                    return;
                }
                release_held(link);
                if !mark_live(generation, &identity, link) {
                    stop_voice();
                    send_bye(link);
                    return;
                }
                // The hello goes out as the first voice packet. Waiting for its
                // receipt here kept this side silent while the caller was already talking.
                crate::voice::stage_hello();
                speak(handle, link, generation, identity, false).await;
            });
        }
        None => {}
    }
}

pub fn decline(ticket: u64) {
    let link = {
        let mut call = calls();
        let Some(link) = call.tickets.remove(&ticket) else {
            return;
        };
        call.held.remove(&link);
        call.invites.retain(|invite| invite.ticket != ticket);
        if let CallFace::Ringing {
            ticket: ringing, ..
        } = call.face
        {
            if ringing == ticket {
                call.face = CallFace::Quiet;
            }
        }
        link
    };
    send_bye(link);
}

/// Keeps a call from a known contact on the page until someone answers it.
pub fn offer_incoming(invite: IncomingCall) {
    let busy = {
        let mut call = calls();
        if matches!(call.face, CallFace::Quiet | CallFace::Failed { .. }) {
            call.face = CallFace::Ringing {
                identity: invite.identity,
                ticket: invite.ticket,
            };
            false
        } else {
            true
        }
    };
    if busy {
        decline(invite.ticket);
    }
}

pub fn hang_up() {
    let mut call = calls();
    if let CallFace::Live { identity } = &call.face {
        let seconds = call
            .started
            .map(|start| start.elapsed().as_secs())
            .unwrap_or(0);
        call.finished = Some(FinishedCall {
            identity: identity.clone(),
            seconds,
            outgoing: call.outgoing,
        });
    }
    call.generation = call.generation.wrapping_add(1);
    call.video = false;
    call.face = CallFace::Quiet;
    call.link = None;
    drop(call);
    stop_voice();
}

fn stop_voice() {
    crate::voice::stop();
    #[cfg(any(target_os = "macos", target_os = "android"))]
    crate::video::stop();
    #[cfg(target_os = "android")]
    hold_to_ear(false);
}

#[cfg(target_os = "android")]
fn hold_to_ear(on: bool) {
    route_call(on, false);
}

/// A video call stays on the loudspeaker, with the screen left on.
#[cfg(target_os = "android")]
fn play_aloud(on: bool) {
    route_call(on, true);
}

#[cfg(target_os = "android")]
fn route_call(on: bool, aloud: bool) {
    let method = if !on {
        "endCall"
    } else if aloud {
        "beginVideoCall"
    } else {
        "beginCall"
    };
    let _ = with_android_context(|env, context| {
        env.call_method(context, method, "()V", &[])?;
        Ok(())
    });
}

async fn prepare_voice(generation: u64, identity: &str, aloud: bool) -> bool {
    if !still(generation) {
        return false;
    }
    #[cfg(target_os = "android")]
    if !microphone_allowed().await {
        fail_if(generation, identity, crate::voice::NEEDS_MICROPHONE);
        return false;
    }
    let Some(runtime) = RUNTIME.get() else {
        fail_if(generation, identity, crate::voice::DIDNT_CONNECT);
        return false;
    };
    #[cfg(target_os = "android")]
    if aloud {
        play_aloud(true);
    } else {
        hold_to_ear(true);
    }
    #[cfg(not(target_os = "android"))]
    let _ = aloud;
    let opened = runtime.spawn_blocking(crate::voice::start).await;
    if !still(generation) {
        stop_voice();
        return false;
    }
    match opened {
        Ok(Ok(())) if !crate::voice::stopped() => true,
        Ok(Err(words)) => {
            fail_if(generation, identity, words);
            false
        }
        _ => {
            stop_voice();
            fail_if(generation, identity, crate::voice::DIDNT_CONNECT);
            false
        }
    }
}

async fn speak(
    handle: PrnsNodeHandle,
    link: LinkId,
    generation: u64,
    identity: String,
    outgoing: bool,
) {
    send_voice(&handle, link, generation).await;
    let _ = handle
        .send_link_packet(link, &crate::voice::bye_packet())
        .await;
    finish_call(generation, &identity, outgoing);
}

async fn send_voice(handle: &PrnsNodeHandle, link: LinkId, generation: u64) {
    loop {
        if !still(generation) || crate::voice::stopped() {
            break;
        }
        let Some(packet) = crate::voice::take_one() else {
            rest(5).await;
            continue;
        };
        if !still(generation) || crate::voice::stopped() {
            break;
        }
        // The side that opened the link never proves data that comes back, so a
        // receipt for the return voice does not arrive until it times out. Waiting
        // for that receipt held the responder to one moment per timeout.
        if let Some(sent) = hand_to_link(handle, link, &packet).await {
            if !call_can_continue(sent) {
                stop_voice();
                break;
            }
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "android"))]
async fn prepare_video(generation: u64, identity: &str) -> bool {
    if !still(generation) {
        return false;
    }
    let Some(runtime) = RUNTIME.get() else {
        fail_if(generation, identity, crate::voice::DIDNT_CONNECT);
        return false;
    };
    let opened = runtime.spawn_blocking(crate::video::start).await;
    if !still(generation) {
        stop_voice();
        return false;
    }
    match opened {
        Ok(Ok(())) if !crate::video::stopped() => true,
        Ok(Err(words)) => {
            fail_if(generation, identity, words);
            false
        }
        _ => {
            stop_voice();
            fail_if(generation, identity, crate::voice::DIDNT_CONNECT);
            false
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "android"))]
async fn answer_film(handle: PrnsNodeHandle, link: LinkId, generation: u64, identity: String) {
    if !prepare_video(generation, &identity).await {
        let _ = handle
            .send_link_packet(link, &crate::video::bye_packet())
            .await;
        return;
    }
    if !prepare_voice(generation, &identity, true).await {
        let _ = handle
            .send_link_packet(link, &crate::video::bye_packet())
            .await;
        return;
    }
    if !still(generation) {
        stop_voice();
        let _ = handle
            .send_link_packet(link, &crate::video::bye_packet())
            .await;
        return;
    }
    release_held(link);
    if !mark_live(generation, &identity, link) {
        stop_voice();
        let _ = handle
            .send_link_packet(link, &crate::video::bye_packet())
            .await;
        return;
    }
    crate::video::stage_hello();
    crate::voice::stage_hello();
    show_and_speak(handle, link, generation, identity, false).await;
}

#[cfg(any(target_os = "macos", target_os = "android"))]
async fn show_and_speak(
    handle: PrnsNodeHandle,
    link: LinkId,
    generation: u64,
    identity: String,
    outgoing: bool,
) {
    tokio::join!(
        send_voice(&handle, link, generation),
        send_picture(&handle, link, generation, outgoing),
    );
    let _ = handle
        .send_link_packet(link, &crate::voice::bye_packet())
        .await;
    let _ = handle
        .send_link_packet(link, &crate::video::bye_packet())
        .await;
    finish_call(generation, &identity, outgoing);
}

#[cfg(any(target_os = "macos", target_os = "android"))]
async fn send_picture(handle: &PrnsNodeHandle, link: LinkId, generation: u64, outgoing: bool) {
    loop {
        if !still(generation) || crate::video::stopped() {
            break;
        }
        // Sound is one short moment at a time. A picture piece waits while a
        // moment is already queued, so the microphone is not stuck behind it.
        if crate::voice::waiting() {
            rest(5).await;
            continue;
        }
        let Some(packet) = crate::video::take_one() else {
            rest(15).await;
            continue;
        };
        if !still(generation) || crate::video::stopped() {
            break;
        }
        // The opener is proved for each packet, so the next piece waits until
        // this one is accepted. A burst of a key frame was arriving with its
        // tail missing, and the picture cannot start without that first frame.
        // The side that answered gets no proof back; waiting there stalls the
        // call for a whole timeout per piece, so that side still sends ahead,
        // one piece at a time, with a gap a voice moment can use.
        if outgoing {
            match handle.send_link_packet(link, &packet).await {
                Ok(_) => {}
                Err(personal_rns::SendError::NodeStopped) => {
                    stop_voice();
                    break;
                }
                Err(_) => {}
            }
        } else {
            match hand_to_link(handle, link, &packet).await {
                Some(sent) if !call_can_continue(sent) => {
                    stop_voice();
                    break;
                }
                Some(_) | None => rest(20).await,
            }
        }
    }
}

fn begin_call(identity: &str, outgoing: bool, video: bool) -> Option<u64> {
    let mut call = calls();
    if !matches!(call.face, CallFace::Quiet | CallFace::Failed { .. }) {
        return None;
    }
    call.generation = call.generation.wrapping_add(1);
    call.video = video;
    call.outgoing = outgoing;
    call.answered = false;
    call.started = None;
    call.link = None;
    call.face = CallFace::Placing {
        identity: identity.to_string(),
        outgoing,
    };
    Some(call.generation)
}

fn still(generation: u64) -> bool {
    calls().generation == generation
}

fn peer_answered(generation: u64) -> bool {
    let call = calls();
    call.generation == generation && call.answered
}

/// Queues one voice packet and returns at once.
///
/// `Some` means the link answered before the packet was queued, usually because
/// the link is already gone. `None` means the packet is on its way.
async fn hand_to_link(
    handle: &PrnsNodeHandle,
    link: LinkId,
    packet: &[u8],
) -> Option<
    Result<
        personal_rns::PacketReceiptDelivered,
        personal_rns::SendError<personal_rns::engine::SendToLinkFailure>,
    >,
> {
    let sending = handle.send_link_packet(link, packet);
    tokio::pin!(sending);
    tokio::select! {
        biased;
        result = &mut sending => Some(result),
        _ = std::future::ready(()) => None,
    }
}

/// A voice packet that never got its receipt can be skipped. The call ends
/// when the link itself is gone.
fn call_can_continue(
    result: Result<
        personal_rns::PacketReceiptDelivered,
        personal_rns::SendError<personal_rns::engine::SendToLinkFailure>,
    >,
) -> bool {
    match result {
        Ok(_) => true,
        Err(personal_rns::SendError::Failed(failure)) => matches!(
            failure,
            personal_rns::engine::SendToLinkFailure::Timeout
                | personal_rns::engine::SendToLinkFailure::Culled
                | personal_rns::engine::SendToLinkFailure::WriteFailed(_)
        ),
        Err(personal_rns::SendError::Busy | personal_rns::SendError::PayloadTooLarge) => true,
        Err(personal_rns::SendError::NodeStopped) => false,
    }
}

fn bind_link(generation: u64, link: LinkId) -> bool {
    let mut call = calls();
    if call.generation != generation {
        return false;
    }
    call.link = Some(link);
    true
}

fn mark_live(generation: u64, identity: &str, link: LinkId) -> bool {
    let mut call = calls();
    if call.generation != generation {
        return false;
    }
    call.face = CallFace::Live {
        identity: identity.to_string(),
    };
    call.link = Some(link);
    call.started = Some(Instant::now());
    true
}

fn finish_call(generation: u64, identity: &str, outgoing: bool) {
    let mut call = calls();
    if call.generation != generation {
        return;
    }
    if call.finished.is_none() {
        if let CallFace::Live { .. } = call.face {
            let seconds = call
                .started
                .map(|start| start.elapsed().as_secs())
                .unwrap_or(0);
            call.finished = Some(FinishedCall {
                identity: identity.to_string(),
                seconds,
                outgoing,
            });
        }
    }
    call.generation = call.generation.wrapping_add(1);
    call.video = false;
    call.face = CallFace::Quiet;
    call.link = None;
    stop_voice();
}

fn fail_now(identity: &str, words: &'static str) {
    let mut call = calls();
    if !matches!(call.face, CallFace::Quiet | CallFace::Failed { .. }) {
        return;
    }
    call.face = CallFace::Failed {
        identity: identity.to_string(),
        words,
    };
}

fn fail_if(generation: u64, identity: &str, words: &'static str) {
    let mut call = calls();
    if call.generation != generation {
        return;
    }
    stop_voice();
    call.video = false;
    call.face = CallFace::Failed {
        identity: identity.to_string(),
        words,
    };
    call.link = None;
}

fn send_bye(link: LinkId) {
    let Some(runtime) = RUNTIME.get() else {
        return;
    };
    let Some(handle) = HANDLE.get().cloned() else {
        return;
    };
    runtime.spawn(async move {
        let _ = handle
            .send_link_packet(link, &crate::voice::bye_packet())
            .await;
    });
}

fn release_held(link: LinkId) {
    crate::voice::ringback(false);
    let taken = {
        let mut call = calls();
        call.held.remove(&link)
    };
    for packet in taken.map(|held| held.packets).unwrap_or_default() {
        match packet.first().copied() {
            Some(b'F') => {
                #[cfg(any(target_os = "macos", target_os = "android"))]
                crate::video::hear(&packet);
            }
            Some(b'V') => crate::voice::hear(&packet),
            _ => {}
        }
    }
}

fn accept_voice(link_id: LinkId, bytes: &[u8]) {
    let peer = mail().peers.get(&link_id).copied();
    match crate::voice::kind(bytes) {
        crate::voice::VoiceKind::Hello => note_hello(link_id, peer),
        crate::voice::VoiceKind::Audio => note_audio(link_id, bytes, peer),
        crate::voice::VoiceKind::Bye => note_bye(link_id),
        crate::voice::VoiceKind::Other => {}
    }
}

#[cfg(any(target_os = "macos", target_os = "android"))]
fn accept_film(link_id: LinkId, bytes: &[u8]) {
    let peer = mail().peers.get(&link_id).copied();
    match crate::video::kind(bytes) {
        crate::video::FilmKind::Hello => {
            note_hello(link_id, peer);
            let mut call = calls();
            if let Some(held) = call.held.get_mut(&link_id) {
                held.video = true;
            }
            if call.link == Some(link_id) {
                call.video = true;
            }
        }
        crate::video::FilmKind::Frame => note_film(link_id, bytes, peer),
        crate::video::FilmKind::Bye => note_bye(link_id),
        crate::video::FilmKind::Other => {}
    }
}

#[cfg(any(target_os = "macos", target_os = "android"))]
fn note_film(link_id: LinkId, bytes: &[u8], peer: Option<IdentityHash>) {
    let play_now = {
        let mut call = calls();
        if call.link == Some(link_id) && crate::video::running() && call.answered {
            true
        } else {
            let held = call.held.entry(link_id).or_default();
            held.video = true;
            if held.packets.len() >= 64 {
                held.packets.remove(0);
            }
            held.packets.push(bytes.to_vec());
            if held.identity.is_none() {
                if let Some(peer) = peer {
                    held.identity = Some(hex_encode(peer.as_bytes()));
                }
            }
            false
        }
    };
    if play_now {
        crate::video::hear(bytes);
    }
}

fn note_hello(link_id: LinkId, peer: Option<IdentityHash>) {
    let play_held = {
        let mut call = calls();
        if call.link == Some(link_id) {
            call.answered = true;
            true
        } else {
            false
        }
    };
    if play_held {
        release_held(link_id);
        return;
    }
    let mut call = calls();
    if let Some(identity) = placing_identity(&call.face) {
        if peer.is_some_and(|peer| same_contact(&identity, &hex_encode(peer.as_bytes())))
            && call.link.is_none()
        {
            call.link = Some(link_id);
            call.answered = true;
        }
        return;
    }
    {
        let held = call.held.entry(link_id).or_default();
        held.hello = true;
        if held.identity.is_none() {
            if let Some(peer) = peer {
                held.identity = Some(hex_encode(peer.as_bytes()));
            }
        }
    }
    invite_held(&mut call, link_id);
}

fn note_audio(link_id: LinkId, bytes: &[u8], peer: Option<IdentityHash>) {
    let play_now = {
        let mut call = calls();
        if call.link == Some(link_id) && crate::voice::running() && call.answered {
            true
        } else {
            let held = call.held.entry(link_id).or_default();
            if held.packets.len() >= 64 {
                held.packets.remove(0);
            }
            held.packets.push(bytes.to_vec());
            if held.identity.is_none() {
                if let Some(peer) = peer {
                    held.identity = Some(hex_encode(peer.as_bytes()));
                }
            }
            false
        }
    };
    if play_now {
        crate::voice::hear(bytes);
    }
}

fn note_bye(link_id: LinkId) {
    let ending = {
        let call = calls();
        if call.link == Some(link_id) {
            let identity = match &call.face {
                CallFace::Placing { identity, .. } | CallFace::Live { identity } => {
                    identity.clone()
                }
                _ => String::new(),
            };
            let unanswered = matches!(call.face, CallFace::Placing { outgoing: true, .. });
            Some((call.generation, unanswered, identity))
        } else {
            None
        }
    };
    if let Some((generation, unanswered, identity)) = ending {
        if unanswered {
            fail_if(generation, &identity, crate::voice::WASNT_ANSWERED);
        } else {
            hang_up();
        }
        return;
    }
    let mut call = calls();
    if let CallFace::Ringing { ticket, .. } = call.face {
        if call.tickets.get(&ticket) == Some(&link_id) {
            call.face = CallFace::Quiet;
        }
    }
    call.held.remove(&link_id);
    call.tickets.retain(|_, link| *link != link_id);
    let live: Vec<u64> = call.tickets.keys().copied().collect();
    call.invites.retain(|invite| live.contains(&invite.ticket));
}

fn note_peer_on_call(link_id: LinkId, identity_hex: &str) {
    let mut call = calls();
    if let Some(identity) = placing_identity(&call.face) {
        if same_contact(&identity, identity_hex) && call.link.is_none() {
            call.link = Some(link_id);
        }
        return;
    }
    {
        let Some(held) = call.held.get_mut(&link_id) else {
            return;
        };
        if held.identity.is_none() {
            held.identity = Some(identity_hex.to_string());
        }
    }
    invite_held(&mut call, link_id);
}

fn invite_held(call: &mut Call, link_id: LinkId) {
    let Some(held) = call.held.get_mut(&link_id) else {
        return;
    };
    if held.invited || !held.hello {
        return;
    }
    let Some(identity) = held.identity.clone() else {
        return;
    };
    held.invited = true;
    let ticket = call.next_ticket;
    call.next_ticket = call.next_ticket.wrapping_add(1);
    call.tickets.insert(ticket, link_id);
    call.invites.push(IncomingCall { ticket, identity });
}

fn placing_identity(face: &CallFace) -> Option<String> {
    match face {
        CallFace::Placing { identity, .. } | CallFace::Live { identity } => Some(identity.clone()),
        _ => None,
    }
}

#[cfg(target_os = "android")]
async fn microphone_allowed() -> bool {
    let Some(runtime) = RUNTIME.get() else {
        return false;
    };
    runtime
        .spawn_blocking(microphone_allowed_blocking)
        .await
        .unwrap_or(false)
}

#[cfg(target_os = "android")]
fn microphone_allowed_blocking() -> bool {
    with_android_context(|env, context| {
        let allowed = env
            .call_method(context, "allowMicrophone", "()I", &[])?
            .i()?;
        Ok(allowed == 0)
    })
    .unwrap_or(false)
}

fn stand_up_live<F>(
    remote_directory: &Path,
    home: &Path,
    secret: Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>,
    on_event: F,
) -> Result<PrnsNode<NoRemoteControlHostControls, (), F, GrowableHeap>, SessionError>
where
    F: FnMut(PrnsEvent<'_>, &NoRemoteControlHostControls),
{
    stand_up_on_lan(remote_directory, home, secret, on_event)
}

fn stand_up_on_lan<F>(
    remote_directory: &Path,
    home: &Path,
    secret: Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>,
    on_event: F,
) -> Result<PrnsNode<NoRemoteControlHostControls, (), F, GrowableHeap>, SessionError>
where
    F: FnMut(PrnsEvent<'_>, &NoRemoteControlHostControls),
{
    let announce = Vec::new();
    let remote_control = remote_control_service(remote_directory)?;
    let home = home.to_path_buf();
    Ok(PrnsNode::new(PrnsNodeRecipe {
        transport_identity: None,
        remote_control: remote_control.into(),
        pre_configured_destinations: [delivery_destination(secret, &announce)],
        app_state: NoRemoteControlHostControls,
        storage: GrowableHeap,
        request_endpoints: request_endpoints![],
        interfaces: move |handle: &PrnsNodeHandle| attach_interfaces(handle, &home),
        persistence: NoPersistence,
        on_event,
    }))
}

fn attach_interfaces(handle: &PrnsNodeHandle, home: &Path) {
    attach_wifi(handle);
    attach_internet(handle, home);
    attach_ble(handle, home);
    #[cfg(not(target_os = "ios"))]
    attach_usb(handle);
}

fn attach_wifi(handle: &PrnsNodeHandle) {
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let wifi = AutoWifi::default().with_host_discovery(apple_service_discovery());
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let wifi = AutoWifi::default()
        .with_native_host_discovery(native_service_discovery(AutoWifiDevicePolicy::default()));
    #[cfg(target_os = "android")]
    let wifi =
        AutoWifi::default().with_native_host_discovery(native_service_discovery_with_host_lan(
            AutoWifiDevicePolicy::default(),
            crate::lan::inventory(),
        ));
    #[cfg(target_os = "android")]
    crate::lan::remember_status(wifi.status());
    let _attached = handle.supervise(wifi);
}

/// The built-in internet gateway. A saved `tcp-target`, or `JOURNAL_TCP`, can name another.
/// Every build dials the home node.
const BUILT_IN_TCP: &str = "touchbase.viewdns.net:4242";

/// A TCP client dials the built-in gateway unless `JOURNAL_TCP` or a saved `tcp-target` names another `host:port`.
fn attach_internet(handle: &PrnsNodeHandle, home: &Path) {
    let target = tcp_dial_target(home);
    handle.attach(TcpClientInterface::new(target));
}

fn tcp_dial_target(home: &Path) -> String {
    if let Some(target) = std::env::var("JOURNAL_TCP")
        .ok()
        .and_then(|value| parse_tcp_target(&value))
    {
        let _ = std::fs::write(home.join("tcp-target"), format!("{target}\n"));
        return target;
    }
    if let Some(target) = std::fs::read_to_string(home.join("tcp-target"))
        .ok()
        .and_then(|text| parse_tcp_target(&text))
    {
        return target;
    }
    BUILT_IN_TCP.to_string()
}

fn parse_tcp_target(value: &str) -> Option<String> {
    let value = value.trim();
    let (host, port) = value.rsplit_once(':')?;
    let host = host.trim();
    let port = port.trim().parse::<u16>().ok()?;
    if host.is_empty() || port == 0 {
        return None;
    }
    Some(format!("{host}:{port}"))
}

fn attach_ble(handle: &PrnsNodeHandle, home: &Path) {
    let Ok(identity) = personal_rns::load_or_create_ble_identity(&home.join("ble")) else {
        return;
    };
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "linux",
        target_os = "windows"
    ))]
    {
        let _attached = handle.attach(personal_rns::AutoBle::new(identity));
    }
    #[cfg(target_os = "android")]
    {
        use personal_rns::bluetooth_auto::BluetoothAuto;
        use personal_rns::interfaces::bluetooth_auto::{
            AndroidHost, Endpoint, LinkCapabilities, BLE_HW_MTU,
        };
        use prns_ffi::bluetooth_auto::android::AndroidBleBackend;

        let bridge = crate::links::ble_bridge();
        bridge.set_local_identity(identity);
        let bluetooth = BluetoothAuto::<_, { AndroidBleBackend::MAX_PEERS }>::new(
            AndroidBleBackend::new(bridge),
            identity,
            Endpoint::Android(AndroidHost::Android),
            LinkCapabilities {
                l2cap: None,
                link_mtu: BLE_HW_MTU as u16,
            },
        );
        let _attached = handle.supervise(bluetooth);
    }
}

#[cfg(not(target_os = "ios"))]
fn attach_usb(handle: &PrnsNodeHandle) {
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        use std::sync::Arc;

        use tokio::sync::Notify;

        if std::env::var_os("PRNS_USB_AUTO_ANDROID_ACCESSORY").is_none() {
            std::env::set_var("PRNS_USB_AUTO_ANDROID_ACCESSORY", "1");
        }
        let usb = personal_rns::usb_auto::UsbAutoHost::new(
            MAC_USB,
            personal_rns::usb_auto::scan_native_usb_auto_targets,
            open_usb_target,
            Arc::new(Notify::new()),
        );
        handle.add_interface(usb);
    }
    #[cfg(target_os = "android")]
    {
        use std::sync::Arc;

        use personal_rns::usb_auto::{UsbAutoCandidate, UsbAutoHost};

        let bridge = crate::links::usb_bridge();
        let scan = {
            let bridge = bridge.clone();
            move || {
                if bridge.is_connected() {
                    vec![UsbAutoCandidate::prns_specific("android-usb")]
                } else {
                    Vec::new()
                }
            }
        };
        let open = {
            let bridge = bridge.clone();
            move |_candidate: UsbAutoCandidate| {
                let bridge = bridge.clone();
                async move { Ok(bridge.open_stream()) }
            }
        };
        let usb = UsbAutoHost::new(PHONE_USB, scan, open, Arc::clone(&bridge.rescan()));
        handle.add_interface(usb);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
async fn open_usb_target(
    candidate: personal_rns::usb_auto::UsbAutoCandidate,
) -> std::io::Result<personal_rns::usb_auto::NativeUsbAutoStream> {
    personal_rns::usb_auto::open_native_usb_auto_target(
        candidate,
        personal_rns::usb_auto::DEFAULT_USB_BAUD,
    )
    .await
}

#[cfg(test)]
fn stand_up<F>(
    remote_directory: &Path,
    secret: Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>,
    on_event: F,
) -> Result<PrnsNode<NoRemoteControlHostControls, (), F, GrowableHeap>, SessionError>
where
    F: FnMut(PrnsEvent<'_>, &NoRemoteControlHostControls),
{
    let announce = Vec::new();
    let remote_control = remote_control_service(remote_directory)?;
    Ok(PrnsNode::new(PrnsNodeRecipe {
        transport_identity: None,
        remote_control: remote_control.into(),
        pre_configured_destinations: [delivery_destination(secret, &announce)],
        app_state: NoRemoteControlHostControls,
        storage: GrowableHeap,
        request_endpoints: request_endpoints![],
        interfaces: ManuallyAttached,
        persistence: NoPersistence,
        on_event,
    }))
}

fn delivery_destination<'a>(
    secret: Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>,
    announce: &'a [u8],
) -> PreConfiguredDestination<'a> {
    PreConfiguredDestination::Single {
        app_name: DELIVERY_APP,
        aspects: DELIVERY_ASPECTS,
        identity: secret,
        announce_app_data: announce,
        proof: ProofStrategy::ProveAll,
        link_requests: LinkRequestPolicy::AcceptAll,
        ratchet: RatchetPolicy::Ratcheted,
        resource_strategy: ResourceStrategy::AcceptIf,
        maximum_request_bytes: Default::default(),
        request_endpoints: ServeMyRequestEndpoints::No,
    }
}

fn remote_control_service(directory: &Path) -> Result<RemoteControlService<'static>, SessionError> {
    let bootstrap = RemoteControlIdentityDirectory::new(directory)
        .load_or_generate()
        .map_err(SessionError::RemoteControl)?;
    let (secrets, _) = bootstrap.into_parts();
    Ok(RemoteControlService::new(
        secrets,
        RemoteControlInitialControllerGrants::Nobody,
        RemoteControlSelfAnnouncement::Unavailable,
    ))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use personal_rns::prelude::*;
    use tokio::sync::mpsc;

    use personal_rns::interfaces::{
        ConnectionState, InterfaceGravity, InterfaceId, InterfaceMode, InterfaceSnapshot,
        Membership, PeerDetails, RadioIndication,
    };

    use super::{
        apply_watched_route, call_can_continue, contact_is_isolated, delivery_destination,
        finish_sample, forget, gateway_client_id, interface_name, machine_label, marks_from,
        opens_at_the_mailbox, parse_tcp_target, person_quiet_limit, progress_of, reach_for,
        reach_map, resource_budget, stand_up, JournalIdentity, ReachState, RouteLook, SampleNext,
        SegmentSlice, WireMarks, BUILT_IN_TCP, MAC_USB, PERSON_SLICE, PHONE_USB, ROUTE_LOOK,
        STEP_WAIT, UNTIL_POSTED,
    };

    #[test]
    fn a_search_step_moves_on_after_five_seconds() {
        assert_eq!(STEP_WAIT, Duration::from_secs(5));
        assert_eq!(ROUTE_LOOK, Duration::from_secs(2));
    }

    #[test]
    fn a_resource_segment_is_numbered_from_one() {
        assert_eq!(super::segment_slot(1, 2), Some(0));
        assert_eq!(super::segment_slot(2, 2), Some(1));
        assert_eq!(super::segment_slot(1, 1), Some(0));
        assert_eq!(super::segment_slot(0, 1), None);
    }

    #[test]
    fn a_named_file_is_failed_when_its_transfer_dies() {
        use personal_rns::routing::links::LinkId;
        use personal_rns::wire::TRUNCATED_HASH_BYTE_LEN;

        let link = LinkId::new([0x44; TRUNCATED_HASH_BYTE_LEN]);
        let other = LinkId::new([0x45; TRUNCATED_HASH_BYTE_LEN]);
        let packed = crate::lxmf::pack_file_note(
            "this photo",
            1_700_000_000.0,
            None,
            Some(("Pears.jpg", b"jpeg-bytes")),
        );
        let named = crate::lxmf::open_note(&crate::lxmf::file_announcement(&packed).expect("name"))
            .expect("announcement");
        let whole = crate::lxmf::open_note(&packed).expect("file");
        let mut mail = super::empty_mail();
        super::watch_arrived_file(&mut mail, Some(link), "ab", &named);
        super::watch_arrived_file(&mut mail, Some(other), "cd", &named);
        super::fail_open_files(&mut mail, link);
        assert!(mail.open_files.iter().all(|open| open.identity == "cd"));
        assert_eq!(mail.incoming_failed.len(), 1);
        assert_eq!(mail.incoming_failed[0].identity, "ab");
        assert_eq!(mail.incoming_failed[0].file_name, "Pears.jpg");
        super::watch_arrived_file(&mut mail, Some(link), "ab", &whole);
        assert!(mail.incoming_failed.is_empty());
        super::watch_arrived_file(&mut mail, Some(link), "ab", &named);
        super::fail_open_files(&mut mail, link);
        assert_eq!(mail.incoming_failed.len(), 1);
        super::watch_arrived_file(&mut mail, Some(link), "ab", &named);
        assert!(
            mail.incoming_failed.is_empty(),
            "a fresh announcement is another try"
        );
    }

    #[test]
    fn an_isolated_contact_is_posted_without_waiting_on_them() {
        assert_eq!(UNTIL_POSTED, Duration::from_secs(15));
        assert!(PERSON_SLICE < UNTIL_POSTED);
        assert_eq!(resource_budget(0), PERSON_SLICE);
        let photo = resource_budget(1_231_490);
        assert_eq!(person_quiet_limit(photo, false), photo);
        assert_eq!(person_quiet_limit(photo, true), PERSON_SLICE);
        assert!(photo > Duration::from_secs(60));
        assert!(photo < Duration::from_secs(90));
        let whole = 1_000u64;
        let (done, total) = progress_of(&[SegmentSlice {
            index: 1,
            total_segments: 1,
            parts_done: 40,
            part_count: 80,
            whole_bytes: whole,
        }])
        .expect("a live segment");
        assert_eq!((done, total), (500, whole));
        let span = personal_rns::routing::links::resources::MAX_EFFICIENT_SIZE as u64;
        let photo_bytes = span + 200;
        let plan =
            personal_rns::routing::links::resources::ResourceSendPlan::new(photo_bytes, None, span)
                .expect("plan");
        let first = plan.segment(1).expect("first").data_end;
        let (done, total) = progress_of(&[SegmentSlice {
            index: 2,
            total_segments: 2,
            parts_done: 0,
            part_count: 10,
            whole_bytes: photo_bytes,
        }])
        .expect("the second segment");
        assert_eq!((done, total), (first, photo_bytes));
        assert!(first < span, "the short tail is folded into both segments");
        let (done, _) = progress_of(&[
            SegmentSlice {
                index: 1,
                total_segments: 2,
                parts_done: 1,
                part_count: 2,
                whole_bytes: photo_bytes,
            },
            SegmentSlice {
                index: 2,
                total_segments: 2,
                parts_done: 0,
                part_count: 4,
                whole_bytes: photo_bytes,
            },
        ])
        .expect("both segments");
        assert_eq!(done, first / 2);
        assert!(!opens_at_the_mailbox(false, true));
        assert!(opens_at_the_mailbox(true, true));
        assert!(!opens_at_the_mailbox(true, false));

        let identity = "isolated-delivery-test";
        reach_map().insert(
            identity.to_string(),
            ReachState {
                generation: 1,
                path: None,
                hops: None,
                ways: crate::model::OfferedWays::none(),
                round_trip_ms: None,
                done: false,
                looked_at: std::time::Instant::now(),
                due: false,
                sample_due: false,
                looking: true,
            },
        );
        assert!(!contact_is_isolated(identity));
        reach_map().get_mut(identity).unwrap().done = true;
        assert!(contact_is_isolated(identity));
        reach_map().get_mut(identity).unwrap().path = Some("Internet".to_string());
        assert!(!contact_is_isolated(identity));
        reach_map().remove(identity);
        assert!(!contact_is_isolated("never-measured"));
    }

    #[test]
    fn an_open_journal_follows_a_changed_path() {
        let identity = "watch-test-contact";
        reach_map().insert(
            identity.to_string(),
            ReachState {
                generation: 7,
                path: Some("Bluetooth".to_string()),
                hops: Some(1),
                ways: crate::model::OfferedWays::none(),
                round_trip_ms: None,
                done: true,
                looked_at: std::time::Instant::now(),
                due: false,
                sample_due: false,
                looking: true,
            },
        );
        let follow =
            apply_watched_route(identity, 7, RouteLook::Known("Wi-Fi".to_string(), 3), true);
        assert!(matches!(follow, SampleNext::Now));
        let reach = reach_for(identity);
        assert_eq!(reach.winner.as_deref(), Some("Wi-Fi"));
        assert_eq!(reach.hops, Some(3));
        assert_eq!(reach.ways, crate::model::OfferedWays::none());
        assert!(!reach.pending);

        let follow = apply_watched_route(
            identity,
            6,
            RouteLook::Known("Internet".to_string(), 9),
            true,
        );
        assert!(matches!(follow, SampleNext::None));
        let reach = reach_for(identity);
        assert_eq!(reach.winner.as_deref(), Some("Wi-Fi"));
        assert_eq!(reach.hops, Some(3));

        let same = apply_watched_route(identity, 7, RouteLook::Known("Wi-Fi".to_string(), 3), true);
        assert!(matches!(same, SampleNext::None));

        let follow = apply_watched_route(identity, 7, RouteLook::Gone, true);
        assert!(matches!(follow, SampleNext::None));
        let reach = reach_for(identity);
        assert_eq!(reach.winner, None);
        assert_eq!(reach.hops, None);
        assert!(!reach.pending);

        reach_map().get_mut(identity).expect("the contact").done = false;
        let follow = apply_watched_route(identity, 7, RouteLook::Known("USB".to_string(), 2), true);
        assert!(matches!(follow, SampleNext::None));
        let reach = reach_for(identity);
        assert!(reach.pending);
        assert_eq!(reach.winner, None);

        forget(identity);
    }

    #[test]
    fn a_changed_path_asks_for_another_sample() {
        let identity = "sample-again-contact";
        let voice = crate::model::OfferedWays {
            note: true,
            voice: true,
            video: false,
        };
        reach_map().insert(
            identity.to_string(),
            ReachState {
                generation: 4,
                path: Some("Bluetooth".to_string()),
                hops: Some(1),
                ways: voice,
                round_trip_ms: None,
                done: true,
                looked_at: std::time::Instant::now(),
                due: false,
                sample_due: false,
                looking: true,
            },
        );

        let follow = apply_watched_route(
            identity,
            4,
            RouteLook::Known("Internet".to_string(), 2),
            false,
        );
        assert!(matches!(follow, SampleNext::AfterCall));
        let reach = reach_for(identity);
        assert_eq!(reach.winner.as_deref(), Some("Internet"));
        assert_eq!(reach.hops, Some(2));
        assert_eq!(reach.ways, voice);
        assert!(reach_map().get(identity).expect("the contact").sample_due);

        let follow = apply_watched_route(
            identity,
            4,
            RouteLook::Known("Internet".to_string(), 2),
            true,
        );
        assert!(matches!(follow, SampleNext::Now));
        let reach = reach_for(identity);
        assert_eq!(reach.ways, crate::model::OfferedWays::none());
        assert!(!reach_map().get(identity).expect("the contact").sample_due);

        let measured = crate::model::OfferedWays {
            note: true,
            voice: true,
            video: true,
        };
        finish_sample(identity, 4, measured, Some(20));
        assert_eq!(reach_for(identity).ways, measured);
        assert!(!reach_map().get(identity).expect("the contact").looking);

        forget(identity);
    }

    #[test]
    fn a_slow_voice_packet_keeps_the_call() {
        use personal_rns::engine::{SendToLinkFailure, SendToLinkRejection};
        use personal_rns::SendError;

        assert!(call_can_continue(Err(SendError::Failed(
            SendToLinkFailure::Timeout
        ))));
        assert!(call_can_continue(Err(SendError::Busy)));
        assert!(!call_can_continue(Err(SendError::Failed(
            SendToLinkFailure::LinkClosed
        ))));
        assert!(!call_can_continue(Err(SendError::Failed(
            SendToLinkFailure::Rejected(SendToLinkRejection::LinkNotActive)
        ))));
        assert!(!call_can_continue(Err(SendError::NodeStopped)));
    }

    #[test]
    fn a_phone_name_is_the_first_word_of_its_model() {
        assert_eq!(machine_label("Pixel 7 Pro"), "Pixel");
        assert_eq!(machine_label("  Pixel Fold "), "Pixel");
        assert_eq!(machine_label(""), "Phone");
        assert_eq!(machine_label("   "), "Phone");
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
    #[test]
    fn a_tcp_target_is_a_host_and_port() {
        assert_eq!(
            parse_tcp_target("  example.com:4242\n"),
            Some("example.com:4242".to_string())
        );
        assert_eq!(
            parse_tcp_target(BUILT_IN_TCP),
            Some(BUILT_IN_TCP.to_string())
        );
        assert_eq!(parse_tcp_target("missing"), None);
        assert_eq!(parse_tcp_target("host:0"), None);
    }

    #[test]
    fn wire_letters_follow_a_live_peer_on_each_interface() {
        let quiet = marks_from(&[]);
        assert_eq!(quiet, WireMarks::quiet());

        let mut snapshots = vec![
            snapshot(InterfaceKind::AutoWifi, ConnectionState::Disconnected),
            snapshot(InterfaceKind::WifiPeer, ConnectionState::Connected),
            snapshot(InterfaceKind::TcpClient, ConnectionState::Connected),
            snapshot(InterfaceKind::TcpServerPeer, ConnectionState::Connected),
            snapshot(InterfaceKind::BluetoothPeer, ConnectionState::Connected),
            snapshot(InterfaceKind::UsbAutoHost, ConnectionState::Initializing),
            snapshot(InterfaceKind::UsbAutoDevice, ConnectionState::Connected),
        ];
        if let Some(id) = gateway_client_id() {
            let mut gateway = snapshot(InterfaceKind::TcpClient, ConnectionState::Degraded);
            gateway.id = id;
            snapshots.push(gateway);
        }
        assert_eq!(
            marks_from(&snapshots),
            WireMarks {
                wifi: true,
                internet: true,
                ble: true,
                usb: true,
            }
        );

        let down = [snapshot(
            InterfaceKind::BluetoothAuto,
            ConnectionState::Disconnected,
        )];
        assert!(!marks_from(&down).ble);

        let mut cable = snapshot(InterfaceKind::Loopback, ConnectionState::Connected);
        cable.id = MAC_USB;
        assert!(marks_from(&[cable]).usb);
        cable.id = PHONE_USB;
        assert!(marks_from(&[cable]).usb);
        assert_eq!(interface_name(MAC_USB), "USB");
        assert_eq!(interface_name(PHONE_USB), "USB");
    }

    fn snapshot(kind: InterfaceKind, connection: ConnectionState) -> InterfaceSnapshot {
        InterfaceSnapshot {
            id: InterfaceId::from_channel_tag(kind, b"journal"),
            mode: InterfaceMode::Full,
            gravity: InterfaceGravity::ZERO,
            connection,
            failure_reason: None,
            rx_bytes: 0,
            tx_bytes: 0,
            transfer_rates: None,
            destinations: 0,
            links: 0,
            transported_links: 0,
            membership: Membership::Independent,
            radio: RadioIndication::for_kind(Some(kind)),
            details: PeerDetails::NotApplicable,
        }
    }

    #[test]
    fn a_journal_keeps_its_public_key_and_answers_as_lxmf_delivery() {
        let directory = temp_journal();
        let first = JournalIdentity::load_or_create(&directory).expect("identity");
        let second = JournalIdentity::load_or_create(&directory).expect("the same identity");
        assert_eq!(first.public_key_hex(), second.public_key_hex());
        assert_eq!(first.public_key_hex().len(), 128);
        assert!(first
            .public_key_hex()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(first.destination(), second.destination());
        let again = delivery_destination(first.secret.clone(), b"");
        assert_eq!(
            again.destination_hash().expect("delivery name"),
            first.destination()
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_journals_open_a_link_to_lxmf_delivery() {
        let listener_dir = temp_journal();
        let dialer_dir = temp_journal();
        let listener_identity =
            JournalIdentity::load_or_create(&listener_dir).expect("listener identity");
        let destination = listener_identity.destination();
        let tcp = TcpServer::bind("127.0.0.1:0")
            .await
            .expect("localhost listener");
        let address = tcp.local_addr().expect("bound address").to_string();
        let listener = stand_up(
            &listener_dir.join("remote-control"),
            listener_identity.secret.clone(),
            |_event, _state| {},
        )
        .expect("listener");
        let listener_handle = listener.handle();
        let _server = listener_handle.supervise(tcp);

        let (heard_tx, mut heard_rx) = mpsc::unbounded_channel();
        let dialer_identity =
            JournalIdentity::load_or_create(&dialer_dir).expect("dialer identity");
        let client = TcpClientInterface::new(address);
        let dialer = stand_up(
            &dialer_dir.join("remote-control"),
            dialer_identity.secret,
            move |event, _state| {
                if let PrnsEvent::Diagnostic(Diagnostic::AnnounceHeard {
                    destination: heard, ..
                }) = event
                {
                    let _ = heard_tx.send(heard);
                }
            },
        )
        .expect("dialer");
        // The client has to be attached while the node is built. stand_up uses ManuallyAttached,
        // so attach it now, before either node runs.
        dialer.handle().attach(client);

        let announcer = listener_handle.clone();
        let announce = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(200));
            loop {
                ticker.tick().await;
                if announcer
                    .issue(PrnsCommand::AnnounceNow(AnnounceNow {
                        destination,
                        target: AnnounceTarget::AllInterfaces,
                        app_data: AnnounceAppData::Registered,
                    }))
                    .is_none()
                {
                    return;
                }
            }
        });

        let dialer_handle = dialer.handle();
        let exchange = async {
            loop {
                let heard = heard_rx.recv().await.expect("announce stream closed");
                if heard == destination {
                    break;
                }
            }
            dialer_handle
                .establish_link(destination)
                .await
                .expect("link to lxmf.delivery");
        };

        tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(10), exchange) => {
                result.expect("the journals did not link within 10 seconds");
            }
            result = listener.run() => {
                result.expect("listener failed");
                panic!("listener stopped before the link");
            }
            result = dialer.run() => {
                result.expect("dialer failed");
                panic!("dialer stopped before the link");
            }
        }
        announce.abort();
        let _ = std::fs::remove_dir_all(&listener_dir);
        let _ = std::fs::remove_dir_all(&dialer_dir);
    }

    #[test]
    fn a_public_key_matches_its_identity_hash() {
        let directory = temp_journal();
        let identity = super::JournalIdentity::load_or_create(&directory).expect("identity");
        let hash = super::hex_encode(identity.hash.as_bytes());
        assert!(super::same_contact(identity.public_key_hex(), &hash));
        assert!(!super::same_contact("not-a-key", &hash));
        let _ = std::fs::remove_dir_all(&directory);
    }

    fn temp_journal() -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "journal-session-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).expect("temp journal directory");
        directory
    }
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = Vec::with_capacity(text.len() / 2);
    let text = text.as_bytes();
    let mut index = 0;
    while index < text.len() {
        let high = hex_value(text[index])?;
        let low = hex_value(text[index + 1])?;
        bytes.push((high << 4) | low);
        index += 2;
    }
    Some(bytes)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}
