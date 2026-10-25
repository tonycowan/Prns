//! The journal's own identity and `lxmf.delivery` destination.
//!
//! This is the person's address, not a hopspot's private delivery destination.
//! The public key is what a contact scans. The destination hash is derived from
//! that key and the fixed name `lxmf.delivery`, and it is what a path request names.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use personal_rns::identity::in_memory::InMemoryNodeIdentity;
use personal_rns::identity::{IdentityHash, IdentitySigner, PublicIdentityMaterial};
use personal_rns::prelude::*;
use personal_rns::routing::announce::derive_single_destination_hash;
use personal_rns::routing::delivery::Delivery;
use personal_rns::routing::links::LinkId;
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
    /// Pieces of a long note that arrived before the other person was known.
    waiting_parts: HashMap<LinkId, Vec<crate::lxmf::Fragment>>,
    /// Pieces of a long note, joined when every piece has arrived.
    assemblies: HashMap<(String, u64), crate::lxmf::Assembly>,
}

/// A note that arrived on a link, still waiting for the contact list.
pub struct ArrivedNote {
    pub identity: String,
    pub words: String,
    pub written_at: f64,
    pub place: Option<crate::model::Place>,
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
                    let node =
                        match stand_up_live(&remote_directory, &home, secret, on_journal_event) {
                            Ok(node) => node,
                            Err(error) => {
                                let _ = ready_tx.send(Err(error));
                                return;
                            }
                        };
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
                    let _ = node
                        .run_until(async move {
                            let _ = shutdown_rx.await;
                        })
                        .await;
                    announce.abort();
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
            Some(people) => {
                crate::model::note_ids_from(&people);
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
    queue_unsent(&saved);
}

fn queue_unsent(people: &[crate::model::Person]) {
    for person in people {
        for note in &person.notes {
            if note.mine
                && !note.settled
                && !note.failed
                && note.kind == crate::model::NoteKind::Text
            {
                send_note(
                    &person.identity,
                    &note.words,
                    note.id,
                    f64::from_bits(note.written_at),
                    note.place,
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
) {
    let words = words.trim().to_string();
    let place = place.filter(|_| words.contains(crate::model::PLACEHOLDER));
    if words.is_empty() {
        return;
    }
    let Some(destination) = destination_for_contact(contact_identity) else {
        return;
    };
    let packed = match place {
        Some(place) => crate::lxmf::pack_placed_note(&words, written_at, Some(place)),
        None => crate::lxmf::pack_note(&words, written_at),
    };
    note_queue().push(QueuedNote {
        destination,
        packed,
        note_id,
        tries: 0,
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
    let Some(destination) = destination_for_contact(contact_identity) else {
        return;
    };
    note_queue().push(QueuedNote {
        destination,
        packed: crate::lxmf::pack_reaction(written_at, words, reaction),
        note_id: 0,
        tries: 0,
    });
    nudge_delivery();
}

/// How many times a note is sent before it is marked failed.
const GIVE_UP_AFTER: u8 = 3;

struct QueuedNote {
    destination: personal_rns::wire::DestinationHash,
    packed: Vec<u8>,
    note_id: u64,
    tries: u8,
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
                if deliver_note(&note).await {
                    if note.note_id != 0 {
                        mail().acked.push(note.note_id);
                    }
                } else {
                    note.tries = note.tries.saturating_add(1);
                    if note.tries >= GIVE_UP_AFTER {
                        if note.note_id != 0 {
                            mail().failed.push(note.note_id);
                        }
                    } else {
                        again.push(note);
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

async fn deliver_note(note: &QueuedNote) -> bool {
    let Some(handle) = HANDLE.get().cloned() else {
        return false;
    };
    let packets = crate::lxmf::link_packets(&note.packed);
    if packets.is_empty() {
        return false;
    }
    let ours = OURS.get().copied();
    let attempt = async {
        let _ = seek_path(&handle, note.destination).await;
        let Ok(link) = handle.establish_link(note.destination).await else {
            return false;
        };
        if let Some(ours) = ours {
            let _ = handle.identify(link, ours).await;
        }
        for packet in &packets {
            if handle.send_link_packet(link, packet).await.is_err() {
                return false;
            }
        }
        true
    };
    let budget = Duration::from_secs(20u64.saturating_mul(packets.len() as u64));
    tokio::time::timeout(budget, attempt).await.unwrap_or(false)
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
            None => {}
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
        },
        None => Reach {
            pending: false,
            ways: crate::model::OfferedWays::none(),
            winner: None,
            hops: None,
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
                done: false,
                looked_at: Instant::now(),
                due: false,
                sample_due: false,
                looking: false,
            },
        );
    }
    let Some(runtime) = RUNTIME.get().cloned() else {
        finish_search(&identity, generation, crate::model::OfferedWays::none());
        return;
    };
    let Some(handle) = HANDLE.get().cloned() else {
        finish_search(&identity, generation, crate::model::OfferedWays::none());
        return;
    };
    let ours = OURS.get().copied();
    runtime.spawn(async move {
        let Some(destination) = destination_for_contact(&identity) else {
            finish_search(&identity, generation, crate::model::OfferedWays::none());
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
        let ways = crate::model::ways_for_ack(send_probe(&handle, destination, ours).await);
        finish_search(&identity, generation, ways);
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

fn finish_search(identity: &str, generation: u64, ways: crate::model::OfferedWays) {
    let mut map = reach_map();
    let Some(state) = map.get_mut(identity) else {
        return;
    };
    if state.generation != generation {
        return;
    }
    state.ways = ways;
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
        let ways = match destination_for_contact(&identity) {
            Some(destination) => {
                crate::model::ways_for_ack(send_probe(&handle, destination, ours).await)
            }
            None => crate::model::OfferedWays::none(),
        };
        finish_sample(&identity, generation, ways);
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
            state.path = Some(path);
            state.hops = Some(hops);
        }
        RouteLook::Gone => {
            state.path = None;
            state.hops = None;
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

fn finish_sample(identity: &str, generation: u64, ways: crate::model::OfferedWays) {
    let mut map = reach_map();
    let Some(state) = map.get_mut(identity) else {
        return;
    };
    if state.generation != generation || !state.done {
        return;
    }
    state.ways = ways;
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
        None => "Link",
    }
    .to_string()
}

enum Wire {
    Wifi,
    Internet,
    Bluetooth,
    Usb,
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

/// Notes that were tried three times and will not send themselves again.
pub fn drain_failures() -> Vec<u64> {
    std::mem::take(&mut mail().failed)
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
        PrnsEvent::Diagnostic(Diagnostic::PeerIdentified { link_id, identity }) => {
            remember_peer(link_id, identity);
        }
        PrnsEvent::Diagnostic(Diagnostic::AnnounceHeard { destination, .. })
        | PrnsEvent::Diagnostic(Diagnostic::RouteRemoved { destination, .. }) => {
            note_route_churn(destination);
        }
        PrnsEvent::Message(Message::Delivered(Delivery::Link(delivery))) => {
            accept_words(delivery.link_id, delivery.plaintext);
        }
        _ => {}
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
            waiting_parts: HashMap::new(),
            assemblies: HashMap::new(),
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
                mail.inbox.push(ArrivedNote {
                    identity: identity_hex.clone(),
                    words: letter.words,
                    written_at: letter.written_at,
                    place: letter.place,
                });
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
                stage_message(&mut mail, &identity_hex, &whole);
            }
        }
    }
    note_peer_on_call(link_id, &identity_hex);
}

fn accept_words(link_id: LinkId, bytes: &[u8]) {
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
    stage_message(&mut mail, &identity_hex, &whole);
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

fn stage_message(mail: &mut Mail, identity_hex: &str, bytes: &[u8]) {
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
        mail.inbox.push(ArrivedNote {
            identity: identity_hex.to_string(),
            words: letter.words,
            written_at: letter.written_at,
            place: letter.place,
        });
    }
}

fn accept_message(link_id: LinkId, bytes: &[u8]) {
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
        return;
    };
    let mut mail = mail();
    if let Some(identity) = mail.peers.get(&link_id).copied() {
        mail.inbox.push(ArrivedNote {
            identity: hex_encode(identity.as_bytes()),
            words: letter.words,
            written_at: letter.written_at,
            place: letter.place,
        });
    } else {
        mail.waiting.entry(link_id).or_default().push(letter);
    }
}

fn destination_for_contact(stored_identity: &str) -> Option<DestinationHash> {
    let bytes = decode_hex(stored_identity.trim())?;
    let material = PublicIdentityMaterial::from_slice(&bytes).ok()?;
    derive_single_destination_hash(&material.identity_hash(), DELIVERY_APP, DELIVERY_ASPECTS).ok()
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
        resource_strategy: ResourceStrategy::AcceptNone,
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
        apply_watched_route, call_can_continue, delivery_destination, finish_sample, forget,
        gateway_client_id, interface_name, machine_label, marks_from, parse_tcp_target, reach_for,
        reach_map, stand_up, JournalIdentity, ReachState, RouteLook, SampleNext, WireMarks,
        BUILT_IN_TCP, MAC_USB, PHONE_USB, ROUTE_LOOK, STEP_WAIT,
    };

    #[test]
    fn a_search_step_moves_on_after_five_seconds() {
        assert_eq!(STEP_WAIT, Duration::from_secs(5));
        assert_eq!(ROUTE_LOOK, Duration::from_secs(2));
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
        finish_sample(identity, 4, measured);
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
