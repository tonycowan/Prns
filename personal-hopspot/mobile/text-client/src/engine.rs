//! Live LocalClient engine: never hosts; only joins Hopspot's TCP shared bus.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use personal_rns::engine::{
    AnnounceAppData, AnnounceNow, AnnounceTarget, DeliveryEvidence, RatchetPolicy,
    SendSinglePacketFailure,
};
use personal_rns::identity::in_memory::InMemoryNodeIdentity;
use personal_rns::identity::{Zeroizing, IDENTITY_SECRET_KEY_LEN};
use personal_rns::interfaces::{
    shared_instance::configured_policy, ConfiguredInterfacePolicy, InterfaceId,
};
use personal_rns::node_introspection::NodeIntrospection;
use personal_rns::request_endpoints;
use personal_rns::routing::{LinkRequestPolicy, ProofStrategy};
use personal_rns::runtime::{
    load_or_create_identity_secret, Diagnostic, ManuallyAttached, Message, NoPersistence,
    PreConfiguredDestination, PrnsEvent, PrnsNode, PrnsNodeRecipe, RequestPathError,
    ServeMyRequestEndpoints, SendError,
};
use personal_rns::shared_instance::{
    connect_existing_shared_instance, SharedInstanceClientIntent, SharedInstanceTransport,
};
use personal_rns::routing::delivery::Delivery;
use personal_rns::storage::GrowableHeap;
use personal_rns::DestinationHash;
use tokio::sync::mpsc;

use crate::lxmf::{self, ANNOUNCE_APP_DATA};
use crate::model::{
    hex_bytes, parse_dest_hex, AutoRangeSession, ChatDirection, ChatLine, ConnectionPhase,
    HeardAnnounce, RangePrompt, RangePromptKind, Snapshot,
};
use crate::range_check::{self, RangeRequestKind};
use crate::timeutil::format_message_time;

const BUS_PORT: u16 = 37428;
const MAX_HEARD: usize = 40;
const MAX_MESSAGES: usize = 80;
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
/// Opaque probe size matching stock `rnprobe` / `prnsd probe` default (`-s 16`).
const PROBE_PAYLOAD_LEN: usize = 16;

/// Compact label for an announce source interface: kind name + channel-tag hash.
fn format_source_interface(id: InterfaceId) -> String {
    let bytes = id.as_bytes();
    let kind = id.kind().map(|k| k.name()).unwrap_or("unknown");
    format!("{kind} · {}", hex_bytes(&bytes[1..]))
}

enum Command {
    Announce,
    Send {
        peer_hex: String,
        text: String,
        /// When set, notified after `send_single_packet` finishes (proof or error).
        /// Uses std mpsc so UI futures (Dioxus) can poll without a Tokio runtime handle.
        done: Option<std::sync::mpsc::Sender<Result<(), String>>>,
    },
    Probe { destination_hex: String },
}

struct Shared {
    phase: Mutex<ConnectionPhase>,
    destination_hex: Mutex<Option<String>>,
    announce_count: Mutex<u64>,
    last_announce: Mutex<Option<String>>,
    heard: Mutex<VecDeque<HeardAnnounce>>,
    heard_seq: Mutex<u64>,
    messages: Mutex<VecDeque<ChatLine>>,
    message_seq: Mutex<u64>,
    pending_range_prompt: Mutex<Option<RangePrompt>>,
    pending_auto_reply: Mutex<Option<RangePrompt>>,
    auto_range: Mutex<Option<AutoRangeSession>>,
    probe_toast: Mutex<Option<String>>,
    commands: Mutex<Option<mpsc::UnboundedSender<Command>>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            phase: Mutex::new(ConnectionPhase::Starting),
            destination_hex: Mutex::new(None),
            announce_count: Mutex::new(0),
            last_announce: Mutex::new(None),
            heard: Mutex::new(VecDeque::new()),
            heard_seq: Mutex::new(0),
            messages: Mutex::new(VecDeque::new()),
            message_seq: Mutex::new(0),
            pending_range_prompt: Mutex::new(None),
            pending_auto_reply: Mutex::new(None),
            auto_range: Mutex::new(None),
            probe_toast: Mutex::new(None),
            commands: Mutex::new(None),
        }
    }

    fn set_phase(&self, phase: ConnectionPhase) {
        if let Ok(mut guard) = self.phase.lock() {
            *guard = phase;
        }
    }

    fn push_heard(&self, destination: DestinationHash, hops: u8, source_interface: InterfaceId) {
        let seq = {
            let Ok(mut seq) = self.heard_seq.lock() else {
                return;
            };
            *seq += 1;
            *seq
        };
        let at = format_message_time();
        let interface = format_source_interface(source_interface);
        let entry = HeardAnnounce {
            destination_hex: hex_bytes(destination.as_bytes()),
            hops,
            interface,
            at: at.clone(),
            seq,
        };
        if let Ok(mut heard) = self.heard.lock() {
            if let Some(existing) = heard
                .iter_mut()
                .find(|h| h.destination_hex == entry.destination_hex)
            {
                existing.hops = entry.hops;
                existing.interface = entry.interface;
                existing.at = at;
                existing.seq = entry.seq;
                return;
            }
            heard.push_front(entry);
            while heard.len() > MAX_HEARD {
                heard.pop_back();
            }
        }
    }

    fn push_message(
        &self,
        direction: ChatDirection,
        peer_hex: String,
        text: String,
        status: String,
    ) {
        let seq = {
            let Ok(mut seq) = self.message_seq.lock() else {
                return;
            };
            *seq += 1;
            *seq
        };
        let line = ChatLine {
            direction,
            peer_hex,
            text,
            status,
            at: format_message_time(),
            seq,
        };
        if let Ok(mut messages) = self.messages.lock() {
            messages.push_front(line);
            while messages.len() > MAX_MESSAGES {
                messages.pop_back();
            }
        }
    }

    fn push_probed(&self, destination: DestinationHash, hops: u8, rtt_ms: u64) {
        let seq = {
            let Ok(mut seq) = self.heard_seq.lock() else {
                return;
            };
            *seq += 1;
            *seq
        };
        let at = format_message_time();
        let interface = format!("probe · {rtt_ms} ms RTT");
        let entry = HeardAnnounce {
            destination_hex: hex_bytes(destination.as_bytes()),
            hops,
            interface,
            at: at.clone(),
            seq,
        };
        if let Ok(mut heard) = self.heard.lock() {
            if let Some(existing) = heard
                .iter_mut()
                .find(|h| h.destination_hex == entry.destination_hex)
            {
                existing.hops = entry.hops;
                existing.interface = entry.interface;
                existing.at = at;
                existing.seq = entry.seq;
                return;
            }
            heard.push_front(entry);
            while heard.len() > MAX_HEARD {
                heard.pop_back();
            }
        }
    }

    fn set_probe_toast(&self, message: String) {
        if let Ok(mut toast) = self.probe_toast.lock() {
            *toast = Some(message);
        }
    }
}

static STARTED: AtomicBool = AtomicBool::new(false);
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();

fn shared() -> Arc<Shared> {
    SHARED.get_or_init(|| Arc::new(Shared::new())).clone()
}

pub fn ensure_started() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let state = shared();
    std::thread::Builder::new()
        .name("personal-text-rns".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("personal-text-tokio")
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    state.set_phase(ConnectionPhase::Failed(format!(
                        "tokio runtime: {error}"
                    )));
                    return;
                }
            };
            runtime.block_on(run_forever(state));
        })
        .expect("spawn personal-text engine thread");
}

pub fn snapshot() -> Snapshot {
    let state = shared();
    let phase = state
        .phase
        .lock()
        .map(|g| g.clone())
        .unwrap_or(ConnectionPhase::Failed("lock poisoned".into()));
    let destination_hex = state.destination_hex.lock().ok().and_then(|g| g.clone());
    let announce_count = state.announce_count.lock().map(|g| *g).unwrap_or(0);
    let last_announce = state.last_announce.lock().ok().and_then(|g| g.clone());
    let heard = state
        .heard
        .lock()
        .map(|g| g.iter().cloned().collect())
        .unwrap_or_default();
    let messages = state
        .messages
        .lock()
        .map(|g| g.iter().cloned().collect())
        .unwrap_or_default();
    let pending_range_prompt = state
        .pending_range_prompt
        .lock()
        .ok()
        .and_then(|g| g.clone());
    let pending_auto_reply = state
        .pending_auto_reply
        .lock()
        .ok()
        .and_then(|g| g.clone());
    let auto_range = state.auto_range.lock().ok().and_then(|g| g.clone());
    Snapshot {
        phase,
        destination_hex,
        bus: format!("127.0.0.1:{BUS_PORT}"),
        announce_count,
        last_announce,
        heard,
        messages,
        pending_range_prompt,
        pending_auto_reply,
        auto_range,
        live: true,
    }
}

fn require_connected_commands() -> Result<mpsc::UnboundedSender<Command>, String> {
    let state = shared();
    let phase = state
        .phase
        .lock()
        .map(|g| g.clone())
        .unwrap_or(ConnectionPhase::Failed("lock poisoned".into()));
    if !matches!(phase, ConnectionPhase::Connected) {
        return Err("Not connected to Hopspot yet.".into());
    }
    state
        .commands
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .ok_or_else(|| "Engine command channel not ready.".to_string())
}

pub fn request_announce() -> Result<(), String> {
    require_connected_commands()?
        .send(Command::Announce)
        .map_err(|_| "Engine stopped.".to_string())
}

fn prepare_send(peer_hex: &str, text: &str) -> Result<String, String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("Message is empty.".into());
    }
    if text.len() > 240 {
        return Err("Keep messages under ~240 characters for opportunistic LXMF.".into());
    }
    let _ = parse_dest_hex(peer_hex)?;
    let state = shared();
    if range_check::is_stop(&text) && auto_peer_matches(&state, peer_hex) {
        clear_auto_session(&state);
    }
    Ok(text)
}

pub fn request_send(peer_hex: String, text: String) -> Result<(), String> {
    let text = prepare_send(&peer_hex, &text)?;
    require_connected_commands()?
        .send(Command::Send {
            peer_hex,
            text,
            done: None,
        })
        .map_err(|_| "Engine stopped.".to_string())
}

/// Queue a send and wait until delivery proof (or send failure) is recorded.
pub async fn request_send_wait(peer_hex: String, text: String) -> Result<(), String> {
    let text = prepare_send(&peer_hex, &text)?;
    let (tx, rx) = std::sync::mpsc::channel();
    require_connected_commands()?
        .send(Command::Send {
            peer_hex,
            text,
            done: Some(tx),
        })
        .map_err(|_| "Engine stopped.".to_string())?;
    loop {
        match rx.try_recv() {
            Ok(result) => return result,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                crate::timeutil::sleep_ms(50).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err("Engine stopped.".into());
            }
        }
    }
}

fn auto_peer_matches(state: &Shared, peer_hex: &str) -> bool {
    state
        .auto_range
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .is_some_and(|session| session.peer_hex == peer_hex)
}

fn clear_auto_session(state: &Shared) {
    if let Ok(mut session) = state.auto_range.lock() {
        *session = None;
    }
    if let Ok(mut pending) = state.pending_auto_reply.lock() {
        *pending = None;
    }
}

fn maybe_handle_range_or_stop(state: &Shared, peer_hex: &str, text: &str) {
    if range_check::is_stop(text) {
        if auto_peer_matches(state, peer_hex) {
            clear_auto_session(state);
        }
        return;
    }

    let Ok((kind, point)) = range_check::parse_request(text) else {
        return;
    };

    let prompt = RangePrompt {
        peer_hex: peer_hex.to_string(),
        latitude: point.latitude,
        longitude: point.longitude,
        kind: match kind {
            RangeRequestKind::OneShot => RangePromptKind::OneShot,
            RangeRequestKind::Auto => RangePromptKind::Auto,
        },
    };

    if auto_peer_matches(state, peer_hex) {
        if let Ok(mut pending) = state.pending_auto_reply.lock() {
            *pending = Some(prompt);
        }
        return;
    }

    if let Ok(mut pending) = state.pending_range_prompt.lock() {
        // Keep an unanswered Auto prompt if the initiator's 10s cycle already
        // started sending one-shot Range checks before Accept/Deny.
        if let Some(existing) = pending.as_ref() {
            if existing.peer_hex == peer_hex
                && existing.kind == RangePromptKind::Auto
                && prompt.kind == RangePromptKind::OneShot
            {
                return;
            }
        }
        *pending = Some(prompt);
    }
}

pub fn clear_range_prompt() {
    let state = shared();
    if let Ok(mut pending) = state.pending_range_prompt.lock() {
        *pending = None;
    };
}

pub fn take_range_prompt() -> Option<RangePrompt> {
    let state = shared();
    state
        .pending_range_prompt
        .lock()
        .ok()
        .and_then(|mut g| g.take())
}

pub fn restore_range_prompt(prompt: RangePrompt) {
    let state = shared();
    if let Ok(mut pending) = state.pending_range_prompt.lock() {
        *pending = Some(prompt);
    };
}

pub fn take_auto_reply() -> Option<RangePrompt> {
    let state = shared();
    state
        .pending_auto_reply
        .lock()
        .ok()
        .and_then(|mut g| g.take())
}

pub fn restore_auto_reply(prompt: RangePrompt) {
    let state = shared();
    if let Ok(mut pending) = state.pending_auto_reply.lock() {
        *pending = Some(prompt);
    };
}

pub fn set_auto_range_session(session: Option<AutoRangeSession>) {
    let state = shared();
    let clearing = session.is_none();
    if let Ok(mut guard) = state.auto_range.lock() {
        *guard = session;
    }
    if clearing {
        if let Ok(mut pending) = state.pending_auto_reply.lock() {
            *pending = None;
        }
    }
}

pub fn request_probe(destination_hex: String) -> Result<(), String> {
    let _ = parse_dest_hex(&destination_hex)?;
    require_connected_commands()?
        .send(Command::Probe { destination_hex })
        .map_err(|_| "Engine stopped.".to_string())
}

pub fn take_probe_toast() -> Option<String> {
    shared()
        .probe_toast
        .lock()
        .ok()
        .and_then(|mut guard| guard.take())
}

async fn run_probe(
    handle: &personal_rns::PrnsNodeHandle,
    state: &Shared,
    destination: DestinationHash,
) -> String {
    let hops = if let Some(route) = handle.route(destination).await {
        route.hops
    } else {
        match tokio::time::timeout(PROBE_TIMEOUT, handle.request_path(destination)).await {
            Ok(Ok(path)) => path.hops.0,
            Ok(Err(RequestPathError::NodeStopped)) => return "Engine stopped.".into(),
            Ok(Err(RequestPathError::Failed(_))) => return "Path lookup failed.".into(),
            Ok(Err(RequestPathError::EntropyUnavailable)) => {
                return "Path lookup unavailable.".into();
            }
            Err(_) => return "Path lookup timed out.".into(),
        }
    };

    // Opaque payload, same size as stock rnprobe default; contents are not meaningful.
    let payload = [0x01u8; PROBE_PAYLOAD_LEN];
    let delivery = tokio::time::timeout(
        PROBE_TIMEOUT,
        handle.send_single_packet(destination, &payload),
    )
    .await;

    match delivery {
        Err(_) => "Probe timed out.".into(),
        Ok(Err(SendError::NodeStopped)) => "Engine stopped.".into(),
        Ok(Err(SendError::PayloadTooLarge)) => "Probe payload too large.".into(),
        Ok(Err(SendError::Busy)) => "Engine busy — try again.".into(),
        Ok(Err(SendError::Failed(
            SendSinglePacketFailure::Timeout | SendSinglePacketFailure::Culled,
        ))) => "Probe timed out.".into(),
        Ok(Err(SendError::Failed(error))) => format!("Probe failed: {error:?}"),
        Ok(Ok(delivered)) => match delivered.evidence {
            DeliveryEvidence::Proof(_) => {
                let rtt_ms = delivered.rtt.millis();
                let hops = handle
                    .route(destination)
                    .await
                    .map(|route| route.hops)
                    .unwrap_or(hops);
                state.push_probed(destination, hops, rtt_ms);
                format!("Found peer — {rtt_ms} ms RTT, {hops} hop(s)")
            }
            DeliveryEvidence::Response => "Unexpected probe response.".into(),
        },
    }
}

fn ingest_delivery(state: &Shared, delivery: Delivery<'_>) {
    let plaintext = match &delivery {
        Delivery::Single(single) => single.plaintext,
        Delivery::Link(link) => link.plaintext,
        Delivery::Plain(plain) => plain.payload,
        Delivery::Group(group) => group.plaintext,
    };
    // Opaque probes (and other non-LXMF singles) must not spam Chats as "unparsed".
    // Without a sender address in the payload we also cannot list the prober on Others.
    if matches!(delivery, Delivery::Single(_))
        && plaintext.len() <= PROBE_PAYLOAD_LEN
        && lxmf::unpack_lxmf_bytes(plaintext).is_none()
    {
        return;
    }
    ingest_lxmf_bytes(state, plaintext, "packet");
}

fn ingest_lxmf_bytes(state: &Shared, data: &[u8], via: &str) {
    if let Some(parsed) = lxmf::unpack_lxmf_bytes(data) {
        let text = if parsed.title.is_empty() {
            parsed.content
        } else {
            format!("{} — {}", parsed.title, parsed.content)
        };
        let peer_hex = parsed.source_hex.clone();
        maybe_handle_range_or_stop(state, &peer_hex, &text);
        state.push_message(
            ChatDirection::In,
            peer_hex,
            text,
            format!("received ({via})"),
        );
        return;
    }
    // Still surface the delivery so inbound path bugs are visible in Chats.
    let preview = hex_bytes(&data[..data.len().min(12)]);
    state.push_message(
        ChatDirection::In,
        "unknown".into(),
        format!("({via} {} bytes) {preview}…", data.len()),
        "unparsed".into(),
    );
}

fn identity_path() -> PathBuf {
    #[cfg(target_os = "android")]
    {
        return PathBuf::from("/data/data/org.personal.textclient/files/lxmf_identity");
    }
    #[cfg(not(target_os = "android"))]
    {
        let base = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join(".personal-text-client").join("lxmf_identity")
    }
}

fn lxmf_destination(
    identity: Zeroizing<[u8; IDENTITY_SECRET_KEY_LEN]>,
) -> PreConfiguredDestination<'static> {
    PreConfiguredDestination::Single {
        // Sideband often delivers short LXMs as link packets and larger ones as resources.
        resource_strategy: personal_rns::routing::links::resources::ResourceStrategy::Accept {
            max_uncompressed_bytes: 256 * 1024,
            accept_compressed: true,
        },
        app_name: "lxmf",
        aspects: &["delivery"],
        identity,
        announce_app_data: ANNOUNCE_APP_DATA,
        proof: ProofStrategy::ProveAll,
        link_requests: LinkRequestPolicy::AcceptAll,
        ratchet: RatchetPolicy::NoRatchets,
        maximum_request_bytes: Default::default(),
        request_endpoints: ServeMyRequestEndpoints::No,
    }
}

async fn run_forever(state: Arc<Shared>) {
    loop {
        match run_session(state.clone()).await {
            SessionEnd::HopspotGone => {
                state.set_phase(ConnectionPhase::WaitingForHopspot);
            }
            SessionEnd::Failed(message) => {
                state.set_phase(ConnectionPhase::Failed(message));
            }
        }
        if let Ok(mut commands) = state.commands.lock() {
            *commands = None;
        }
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

enum SessionEnd {
    HopspotGone,
    Failed(String),
}

async fn run_session(state: Arc<Shared>) -> SessionEnd {
    state.set_phase(ConnectionPhase::WaitingForHopspot);

    let path = identity_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let identity_secret = match load_or_create_identity_secret(&path) {
        Ok(identity) => identity,
        Err(error) => {
            return SessionEnd::Failed(format!("identity file: {error}"));
        }
    };
    let signer = InMemoryNodeIdentity::from_secret_key_bytes(&identity_secret);
    let mut dest_secret = Zeroizing::new([0u8; IDENTITY_SECRET_KEY_LEN]);
    dest_secret.copy_from_slice(identity_secret.as_ref());

    let destination = lxmf_destination(dest_secret);
    let destination_hash = match destination.destination_hash() {
        Ok(hash) => hash,
        Err(error) => {
            return SessionEnd::Failed(format!("destination name: {error:?}"));
        }
    };
    if let Ok(mut hex) = state.destination_hex.lock() {
        *hex = Some(hex_bytes(destination_hash.as_bytes()));
    }

    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    if let Ok(mut commands) = state.commands.lock() {
        *commands = Some(command_tx);
    }

    let event_state = state.clone();
    let node = PrnsNode::new(PrnsNodeRecipe {
        transport_identity: None,
        remote_control: personal_rns::remote_control::RemoteControlService::Unavailable,
        pre_configured_destinations: [destination],
        app_state: (),
        storage: GrowableHeap,
        request_endpoints: request_endpoints![],
        interfaces: ManuallyAttached,
        persistence: NoPersistence,
        on_event: move |event, _app| match event {
            PrnsEvent::Diagnostic(Diagnostic::AnnounceHeard {
                destination,
                hops,
                source_interface,
                ..
            }) => {
                event_state.push_heard(destination, hops, source_interface);
            }
            PrnsEvent::Message(Message::Delivered(delivery)) => {
                ingest_delivery(&event_state, delivery);
            }
            PrnsEvent::Message(Message::Resource { data, .. }) => {
                ingest_lxmf_bytes(&event_state, data, "resource");
            }
            _ => {}
        },
    });
    let handle = node.handle();

    let intent = SharedInstanceClientIntent {
        bus_port: BUS_PORT,
        transport: SharedInstanceTransport::Tcp,
        policy: configured_policy(ConfiguredInterfacePolicy::default()),
    };

    match connect_existing_shared_instance(&handle, intent).await {
        Ok(_) => {
            state.set_phase(ConnectionPhase::Connected);
        }
        Err(_) => {
            return SessionEnd::HopspotGone;
        }
    }

    let cmd_handle = handle.clone();
    let cmd_state = state.clone();
    let command_task = tokio::spawn(async move {
        while let Some(command) = command_rx.recv().await {
            match command {
                Command::Announce => {
                    let result = cmd_handle
                        .announce_now(AnnounceNow {
                            destination: destination_hash,
                            target: AnnounceTarget::AllInterfaces,
                            app_data: AnnounceAppData::Registered,
                        })
                        .await;
                    match result {
                        Ok(()) => {
                            if let Ok(mut count) = cmd_state.announce_count.lock() {
                                *count += 1;
                            }
                            if let Ok(mut last) = cmd_state.last_announce.lock() {
                                *last = Some("ok".into());
                            }
                        }
                        Err(error) => {
                            if let Ok(mut last) = cmd_state.last_announce.lock() {
                                *last = Some(format!("error: {error:?}"));
                            }
                        }
                    }
                }
                Command::Send {
                    peer_hex,
                    text,
                    done,
                } => {
                    let finish = |result: Result<(), String>| {
                        if let Some(done) = done {
                            let _ = done.send(result);
                        }
                    };
                    let peer_bytes = match parse_dest_hex(&peer_hex) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            let status = format!("bad peer: {error}");
                            cmd_state.push_message(
                                ChatDirection::Out,
                                peer_hex,
                                text,
                                status.clone(),
                            );
                            finish(Err(status));
                            continue;
                        }
                    };
                    let peer = DestinationHash::new(peer_bytes);
                    let packed = match lxmf::pack_opportunistic(
                        peer,
                        destination_hash,
                        &signer,
                        "",
                        &text,
                    ) {
                        Ok(packed) => packed,
                        Err(error) => {
                            let status = format!("pack: {error}");
                            cmd_state.push_message(
                                ChatDirection::Out,
                                peer_hex,
                                text,
                                status.clone(),
                            );
                            finish(Err(status));
                            continue;
                        }
                    };
                    match cmd_handle.send_single_packet(peer, &packed).await {
                        Ok(_) => {
                            cmd_state.push_message(
                                ChatDirection::Out,
                                peer_hex,
                                text,
                                "sent".into(),
                            );
                            finish(Ok(()));
                        }
                        Err(error) => {
                            let status = format!("send: {error:?}");
                            cmd_state.push_message(
                                ChatDirection::Out,
                                peer_hex,
                                text,
                                status.clone(),
                            );
                            finish(Err(status));
                        }
                    }
                }
                Command::Probe { destination_hex } => {
                    let peer_bytes = match parse_dest_hex(&destination_hex) {
                        Ok(bytes) => bytes,
                        Err(error) => {
                            cmd_state.set_probe_toast(error);
                            continue;
                        }
                    };
                    let destination = DestinationHash::new(peer_bytes);
                    let message = run_probe(&cmd_handle, &cmd_state, destination).await;
                    cmd_state.set_probe_toast(message);
                }
            }
        }
    });

    let run_result = node.run().await;
    command_task.abort();
    match run_result {
        Ok(()) => SessionEnd::HopspotGone,
        Err(error) => SessionEnd::Failed(format!("node: {error:?}")),
    }
}
