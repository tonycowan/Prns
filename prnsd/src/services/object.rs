//! Object store, import, transfer, and release hosted by prnsd.
//!
//! Import stays on the local command socket. A transfer is a path to one
//! object-transfer destination and then a pull on a link. A release, a who-has,
//! and a claim are one-hop plain broadcasts. Transport does not carry them.
//! Each object-transfer service repeats a broadcast on every interface except
//! the one it arrived on.

use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use std::cell::Cell;
#[cfg(unix)]
use std::collections::HashMap;

use personal_rns::interfaces::{ConnectionState, InterfaceId};
use personal_rns::routing::delivery::Delivery;
use personal_rns::routing::links::LinkId;
use personal_rns::runtime::{Diagnostic, Message, PrnsEvent};
use personal_rns::wire::DestinationHash;

const INGRESS_BOUND: usize = 256;

pub(crate) struct Prepared {
    events: Events,
    rx: Receiver<Ingress>,
    #[cfg(unix)]
    stack: obstore::config::StackPaths,
    #[cfg(unix)]
    config_dir: std::path::PathBuf,
}

#[derive(Clone)]
pub(crate) struct Events {
    plain: DestinationHash,
    tx: SyncSender<Ingress>,
    known_links: Arc<Mutex<std::collections::HashSet<LinkId>>>,
}

enum Ingress {
    Plain {
        interface: InterfaceId,
        bytes: Vec<u8>,
    },
    Link {
        link: LinkId,
        bytes: Vec<u8>,
    },
    Segment {
        link: LinkId,
        hash: [u8; 32],
        index: u64,
        total: u64,
        bytes: Vec<u8>,
    },
    Closed {
        link: LinkId,
    },
}

impl Prepared {
    pub(crate) fn events(&self) -> Events {
        self.events.clone()
    }
}

impl Events {
    pub(crate) fn observe(&self, event: &PrnsEvent<'_>) {
        let ingress = match event {
            PrnsEvent::Message(Message::Delivered(Delivery::Plain(plain)))
                if plain.destination == self.plain =>
            {
                Ingress::Plain {
                    interface: plain.source_interface,
                    bytes: plain.payload.to_vec(),
                }
            }
            PrnsEvent::Message(Message::Delivered(Delivery::Link(link))) => {
                if !self.owns_link(link.link_id, link.plaintext) {
                    return;
                }
                Ingress::Link {
                    link: link.link_id,
                    bytes: link.plaintext.to_vec(),
                }
            }
            PrnsEvent::Message(Message::Resource { link_id, data, .. }) => {
                if !self.owns_link(*link_id, data) {
                    return;
                }
                Ingress::Link {
                    link: *link_id,
                    bytes: data.to_vec(),
                }
            }
            PrnsEvent::Message(Message::ResourceSegment {
                link_id,
                original_hash,
                segment_index,
                total_segments,
                data,
                ..
            }) => {
                if !self.owns_link(*link_id, data) && *segment_index != 0 {
                    return;
                }
                if !self.owns_link(*link_id, data) && !data.starts_with(b"OBXF") {
                    return;
                }
                Ingress::Segment {
                    link: *link_id,
                    hash: *original_hash.as_bytes(),
                    index: *segment_index,
                    total: *total_segments,
                    bytes: data.to_vec(),
                }
            }
            PrnsEvent::Diagnostic(Diagnostic::LinkClosed { link_id, .. }) => {
                let known = self
                    .known_links
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .contains(link_id);
                if !known {
                    return;
                }
                Ingress::Closed { link: *link_id }
            }
            _ => return,
        };
        match self.tx.try_send(ingress) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                tracing::warn!(event = "object_services_ingress_dropped");
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }

    fn owns_link(&self, link: LinkId, bytes: &[u8]) -> bool {
        let known = self
            .known_links
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(&link);
        known || bytes.starts_with(b"OBXF")
    }
}

pub(crate) fn prepare(config_dir: &std::path::Path) -> Option<Prepared> {
    #[cfg(unix)]
    {
        prepare_unix(config_dir)
    }
    #[cfg(not(unix))]
    {
        let _ = config_dir;
        None
    }
}

#[cfg(unix)]
fn prepare_unix(config_dir: &std::path::Path) -> Option<Prepared> {
    if !config_dir.join("config").is_file() {
        return None;
    }
    let stack = match obstore::config::load_stack(config_dir) {
        Ok(stack) => stack,
        Err(error) => {
            let mentioned = std::fs::read_to_string(config_dir.join("config"))
                .ok()
                .is_some_and(|text| text.contains("[object-services]"));
            if mentioned {
                tracing::error!(event = "object_services_config_invalid", error = %error);
            }
            return None;
        }
    };
    let (tx, rx) = mpsc::sync_channel(INGRESS_BOUND);
    let name =
        personal_rns::routing::announce::expand_name("reticulum", &["object-transfer"]).ok()?;
    let plain = personal_rns::routing::announce::derive_plain_destination_hash(&name);
    Some(Prepared {
        events: Events {
            plain,
            tx,
            known_links: Arc::new(Mutex::new(std::collections::HashSet::new())),
        },
        rx,
        stack,
        config_dir: config_dir.to_path_buf(),
    })
}

#[cfg(unix)]
pub(crate) fn register<St, R, F, S>(
    node: &mut personal_rns::runtime::PrnsNode<St, R, F, S>,
    identity: &personal_rns::identity::Zeroizing<
        [u8; personal_rns::identity::IDENTITY_SECRET_KEY_LEN],
    >,
) -> bool
where
    R: personal_rns::runtime::request_endpoints::RequestEndpointSet<St>,
    F: FnMut(PrnsEvent<'_>, &St),
    S: personal_rns::storage::StorageLayout,
{
    use personal_rns::engine::RatchetPolicy;
    use personal_rns::identity::in_memory::InMemoryNodeIdentity;
    use personal_rns::identity::IdentitySigner;
    use personal_rns::routing::announce::{
        derive_plain_destination_hash, derive_single_destination_hash, expand_name,
    };
    use personal_rns::routing::links::resources::ResourceStrategy;
    use personal_rns::routing::{LinkRequestPolicy, ProofStrategy};
    use personal_rns::runtime::{PreConfiguredDestination, ServeMyRequestEndpoints};

    let Ok(name) = expand_name("reticulum", &["object-transfer"]) else {
        tracing::error!(event = "object_transfer_name_invalid");
        return false;
    };
    let identity_hash = InMemoryNodeIdentity::from_secret_key_bytes(identity).identity_hash();
    let Ok(single) =
        derive_single_destination_hash(&identity_hash, "reticulum", &["object-transfer"])
    else {
        tracing::error!(event = "object_transfer_name_invalid");
        return false;
    };
    let mut secret = [0_u8; personal_rns::identity::IDENTITY_SECRET_KEY_LEN];
    secret.copy_from_slice(&identity[..]);
    let local = obstore::transfer::object_transfer_address(&secret);
    secret.fill(0);
    if single.as_bytes() != local.as_bytes() {
        tracing::error!(event = "object_transfer_address_mismatch");
        return false;
    }
    let plain = derive_plain_destination_hash(&name);
    match node.register_preconfigured_destination(PreConfiguredDestination::Plain {
        app_name: "reticulum",
        aspects: &["object-transfer"],
    }) {
        Ok(hash) if hash == plain => {}
        Ok(_) => {
            tracing::error!(event = "object_transfer_plain_destination_failed");
            return false;
        }
        Err(error) => {
            tracing::error!(event = "object_transfer_plain_destination_failed", error = ?error);
            return false;
        }
    }
    let registered = node.register_preconfigured_destination(PreConfiguredDestination::Single {
        app_name: "reticulum",
        aspects: &["object-transfer"],
        identity: identity.clone(),
        announce_app_data: &[],
        proof: ProofStrategy::ProveAll,
        link_requests: LinkRequestPolicy::AcceptAll,
        ratchet: RatchetPolicy::NoRatchets,
        resource_strategy: ResourceStrategy::Accept {
            max_uncompressed_bytes: 33 * 1024 * 1024,
            accept_compressed: true,
        },
        maximum_request_bytes: Default::default(),
        request_endpoints: ServeMyRequestEndpoints::No,
    });
    match registered {
        Ok(hash) if hash.as_bytes() == local.as_bytes() => true,
        Ok(_) => {
            tracing::error!(event = "object_transfer_address_mismatch");
            false
        }
        Err(error) => {
            tracing::error!(event = "object_transfer_destination_failed", error = ?error);
            false
        }
    }
}

#[cfg(unix)]
pub(crate) fn launch(
    handle: personal_rns::runtime::PrnsNodeHandle,
    identity: &personal_rns::identity::Zeroizing<
        [u8; personal_rns::identity::IDENTITY_SECRET_KEY_LEN],
    >,
    prepared: Prepared,
) {
    let Prepared {
        events,
        rx,
        stack,
        config_dir,
    } = prepared;
    let mut secret = [0_u8; personal_rns::identity::IDENTITY_SECRET_KEY_LEN];
    secret.copy_from_slice(&identity[..]);
    let address = obstore::transfer::object_transfer_address(&secret);
    secret.fill(0);
    let address_hex = address.as_hex();
    let store = match obstore::store::ObjectStore::open(&stack.object_store) {
        Ok(store) => Arc::new(store),
        Err(error) => {
            tracing::error!(event = "object_store_open_failed", error = %error);
            return;
        }
    };
    let (jobs_tx, jobs_rx) = tokio::sync::mpsc::channel::<Job>(32);
    let links = Arc::new(Mutex::new(HashMap::<LinkId, SyncSender<Vec<u8>>>::new()));
    let neighborhood = Arc::new(ReticulumNeighborhood {
        jobs: jobs_tx.clone(),
    });
    let transfer = match obstore::transfer::ObjectTransfer::open(&stack.object_transfer, address) {
        Ok(mut transfer) => {
            transfer.set_fetch_policy(stack.fetch);
            transfer.set_neighborhood(neighborhood);
            transfer
        }
        Err(error) => {
            tracing::error!(event = "object_transfer_open_failed", error = %error);
            return;
        }
    };
    let worker_handle = handle.clone();
    let worker_links = Arc::clone(&links);
    let worker_known = Arc::clone(&events.known_links);
    let worker_jobs = jobs_tx.clone();
    let plain = events.plain;
    tokio::spawn(async move {
        let mut jobs_rx = jobs_rx;
        while let Some(job) = jobs_rx.recv().await {
            match job {
                Job::Flood {
                    bytes,
                    exclude,
                    done,
                } => {
                    let result = send_flood(&worker_handle, plain, &bytes, exclude);
                    let _ = done.send(result);
                }
                Job::Open { destination, done } => {
                    let result = open_link(
                        &worker_handle,
                        &worker_links,
                        &worker_known,
                        worker_jobs.clone(),
                        destination,
                    )
                    .await;
                    let _ = done.send(result);
                }
                Job::Send {
                    link,
                    bytes,
                    await_proof,
                    done,
                } => {
                    let result = send_on_link(&worker_handle, link, bytes, await_proof).await;
                    let _ = done.send(result);
                }
            }
        }
    });
    let ingress_store = Arc::clone(&store);
    let ingress_transfer = transfer.clone();
    let ingress_handle = handle;
    let ingress_links = links;
    let ingress_known = events.known_links;
    let ingress_jobs = jobs_tx;
    std::thread::spawn(move || {
        run_ingress(
            rx,
            ingress_store,
            ingress_transfer,
            ingress_handle,
            ingress_links,
            ingress_known,
            ingress_jobs,
        );
    });
    let command_store = store.clone();
    let command_transfer = transfer;
    let command_dir = config_dir.clone();
    std::thread::spawn(move || run_commands(&command_dir, command_store, command_transfer));
    if store.created_loa_key() {
        eprintln!(
            "object-services: created Local Object Authority key {}",
            store.loa_key_path().display()
        );
    }
    eprintln!("object-services: object-transfer address {address_hex}");
    eprintln!(
        "object-services: listening on {} for {}",
        obstore::protocol::socket_path(&config_dir).display(),
        stack.object_store.display()
    );
}

#[cfg(unix)]
const PLAIN_HEADER_LEN: usize = 13;
#[cfg(unix)]
const MAX_PLAIN_PARTS: usize = 256;
#[cfg(unix)]
static FLOOD_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

#[cfg(unix)]
thread_local! {
    static EXCLUDE_ARRIVAL: Cell<Option<InterfaceId>> = const { Cell::new(None) };
}

#[cfg(unix)]
enum Job {
    Flood {
        bytes: Vec<u8>,
        exclude: Option<InterfaceId>,
        done: SyncSender<Result<(), String>>,
    },
    Open {
        destination: [u8; 16],
        done: SyncSender<Result<LinkPipe, String>>,
    },
    Send {
        link: LinkId,
        bytes: Vec<u8>,
        await_proof: bool,
        done: SyncSender<Result<(), String>>,
    },
}

#[cfg(unix)]
struct ReticulumNeighborhood {
    jobs: tokio::sync::mpsc::Sender<Job>,
}

#[cfg(unix)]
impl obstore::transfer::Neighborhood for ReticulumNeighborhood {
    fn flood(&self) -> Vec<Box<dyn obstore::transfer::Pipe>> {
        let exclude = EXCLUDE_ARRIVAL.with(Cell::get);
        vec![Box::new(FloodPipe {
            jobs: self.jobs.clone(),
            exclude,
            pending: Vec::new(),
        })]
    }

    fn open(
        &self,
        destination: &obstore::transfer::Destination,
    ) -> Result<Box<dyn obstore::transfer::Pipe>, obstore::transfer::TransferError> {
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        self.jobs
            .blocking_send(Job::Open {
                destination: *destination.as_bytes(),
                done: done_tx,
            })
            .map_err(|_| {
                obstore::transfer::TransferError::Message("object service stopped".to_string())
            })?;
        let pipe = done_rx
            .recv()
            .map_err(|_| {
                obstore::transfer::TransferError::Message("object service stopped".to_string())
            })?
            .map_err(obstore::transfer::TransferError::Message)?;
        Ok(Box::new(pipe))
    }
}

#[cfg(unix)]
struct FloodPipe {
    jobs: tokio::sync::mpsc::Sender<Job>,
    exclude: Option<InterfaceId>,
    pending: Vec<u8>,
}

#[cfg(unix)]
impl std::io::Write for FloodPipe {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.pending.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.pending);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        self.jobs
            .blocking_send(Job::Flood {
                bytes,
                exclude: self.exclude,
                done: done_tx,
            })
            .map_err(|_| std::io::Error::other("object service stopped"))?;
        match done_rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(std::io::Error::other(message)),
            Err(_) => Err(std::io::Error::other("object service stopped")),
        }
    }
}

#[cfg(unix)]
impl std::io::Read for FloodPipe {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Write::flush(self)?;
        Ok(0)
    }
}

#[cfg(unix)]
impl Drop for FloodPipe {
    fn drop(&mut self) {
        let _ = std::io::Write::flush(self);
    }
}

#[cfg(unix)]
struct LinkPipe {
    jobs: tokio::sync::mpsc::Sender<Job>,
    link: LinkId,
    /// The peer proves data on a link this node opened. A reply on a link the
    /// peer opened is delivered, and this node does not prove it.
    await_proof: bool,
    incoming: Receiver<Vec<u8>>,
    pending_out: Vec<u8>,
    pending_in: Vec<u8>,
    pos: usize,
}

#[cfg(unix)]
impl std::io::Write for LinkPipe {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.pending_out.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.pending_out.is_empty() {
            return Ok(());
        }
        let bytes = std::mem::take(&mut self.pending_out);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        self.jobs
            .blocking_send(Job::Send {
                link: self.link,
                bytes,
                await_proof: self.await_proof,
                done: done_tx,
            })
            .map_err(|_| std::io::Error::other("object service stopped"))?;
        match done_rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(std::io::Error::other(message)),
            Err(_) => Err(std::io::Error::other("object service stopped")),
        }
    }
}

#[cfg(unix)]
impl std::io::Read for LinkPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Write::flush(self)?;
        if self.pos >= self.pending_in.len() {
            self.pending_in = self
                .incoming
                .recv_timeout(std::time::Duration::from_secs(120))
                .map_err(|error| match error {
                    std::sync::mpsc::RecvTimeoutError::Timeout => std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "timed out waiting for the remote object service",
                    ),
                    std::sync::mpsc::RecvTimeoutError::Disconnected => {
                        std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "link closed")
                    }
                })?;
            self.pos = 0;
            if self.pending_in.is_empty() {
                return Ok(0);
            }
        }
        let available = self.pending_in.len() - self.pos;
        let amount = available.min(buf.len());
        buf[..amount].copy_from_slice(&self.pending_in[self.pos..self.pos + amount]);
        self.pos += amount;
        Ok(amount)
    }
}

#[cfg(unix)]
impl Drop for LinkPipe {
    fn drop(&mut self) {
        let _ = std::io::Write::flush(self);
    }
}

#[cfg(unix)]
struct MemoryPipe {
    cursor: std::io::Cursor<Vec<u8>>,
}

#[cfg(unix)]
impl std::io::Read for MemoryPipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut self.cursor, buf)
    }
}

#[cfg(unix)]
impl std::io::Write for MemoryPipe {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
fn send_flood(
    handle: &personal_rns::runtime::PrnsNodeHandle,
    plain: DestinationHash,
    bytes: &[u8],
    exclude: Option<InterfaceId>,
) -> Result<(), String> {
    use personal_rns::engine::{
        EgressTarget, PrnsCommand, SendPlainPacket, SendPlainPacketPayload,
    };

    let chunk_len = personal_rns::wire::BROADCAST_MDU.saturating_sub(PLAIN_HEADER_LEN);
    if chunk_len == 0 || bytes.is_empty() {
        return Err("object broadcast does not fit".to_string());
    }
    let count = bytes.len().div_ceil(chunk_len);
    let count_u16 =
        u16::try_from(count).map_err(|_| "object broadcast is too large".to_string())?;
    let id = FLOOD_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let interfaces = handle.interfaces();
    for (index, chunk) in bytes.chunks(chunk_len).enumerate() {
        let mut packet = Vec::with_capacity(PLAIN_HEADER_LEN + chunk.len());
        packet.extend_from_slice(b"OBPL");
        packet.push(1);
        packet.extend_from_slice(&id.to_le_bytes());
        let index_u16 =
            u16::try_from(index).map_err(|_| "object broadcast is too large".to_string())?;
        packet.extend_from_slice(&index_u16.to_le_bytes());
        packet.extend_from_slice(&count_u16.to_le_bytes());
        packet.extend_from_slice(chunk);
        let payload = SendPlainPacketPayload::from_slice(&packet)
            .map_err(|()| "object broadcast does not fit".to_string())?;
        for interface in &interfaces {
            if Some(interface.id) == exclude {
                continue;
            }
            if !matches!(
                interface.connection,
                ConnectionState::Connected | ConnectionState::Degraded
            ) {
                continue;
            }
            let _ = handle.issue(PrnsCommand::SendPlainPacket(SendPlainPacket {
                destination: plain,
                target: EgressTarget::Interface(interface.id),
                payload: payload.clone(),
            }));
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn open_link(
    handle: &personal_rns::runtime::PrnsNodeHandle,
    links: &Mutex<HashMap<LinkId, SyncSender<Vec<u8>>>>,
    known: &Mutex<std::collections::HashSet<LinkId>>,
    jobs: tokio::sync::mpsc::Sender<Job>,
    destination: [u8; 16],
) -> Result<LinkPipe, String> {
    let hash = DestinationHash::new(destination);
    let address = hex_bytes(&destination);
    match tokio::time::timeout(
        std::time::Duration::from_secs(20),
        handle.request_path(hash),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return Err(format!("no path to {address}: {error:?}")),
        Err(_) => return Err(format!("no path to {address}: timed out")),
    }
    let link = match tokio::time::timeout(
        std::time::Duration::from_secs(20),
        handle.establish_link(hash),
    )
    .await
    {
        Ok(Ok(link)) => link,
        Ok(Err(error)) => return Err(format!("link to {address} failed: {error:?}")),
        Err(_) => return Err(format!("link to {address} timed out")),
    };
    let (tx, rx) = mpsc::sync_channel(32);
    known
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(link);
    links
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(link, tx);
    handle
        .set_link_resource_strategy(
            link,
            personal_rns::routing::links::resources::ResourceStrategy::Accept {
                max_uncompressed_bytes: 33 * 1024 * 1024,
                accept_compressed: true,
            },
        )
        .await
        .map_err(|error| format!("link to {address} refused resources: {error:?}"))?;
    Ok(LinkPipe {
        jobs,
        link,
        await_proof: true,
        incoming: rx,
        pending_out: Vec::new(),
        pending_in: Vec::new(),
        pos: 0,
    })
}

#[cfg(unix)]
async fn send_on_link(
    handle: &personal_rns::runtime::PrnsNodeHandle,
    link: LinkId,
    bytes: Vec<u8>,
    await_proof: bool,
) -> Result<(), String> {
    if !await_proof {
        return issue_unproven_link_bytes(handle, link, &bytes);
    }
    if bytes.len() <= personal_rns::engine::MAX_SEND_TO_LINK_PLAINTEXT_LEN {
        return handle
            .send_link_packet(link, &bytes)
            .await
            .map(|_| ())
            .map_err(|error| format!("object transfer send failed: {error:?}"));
    }
    let length =
        u64::try_from(bytes.len()).map_err(|_| "object transfer is too large".to_string())?;
    let cursor = std::io::Cursor::new(bytes);
    handle
        .send_resource(link, length, cursor)
        .await
        .map_err(|error| format!("object transfer send failed: {error:?}"))
}

#[cfg(unix)]
fn issue_unproven_link_bytes(
    handle: &personal_rns::runtime::PrnsNodeHandle,
    link: LinkId,
    bytes: &[u8],
) -> Result<(), String> {
    use personal_rns::engine::{PrnsCommand, SendToLink, SendToLinkPayload};

    let max = personal_rns::engine::MAX_SEND_TO_LINK_PLAINTEXT_LEN;
    for chunk in bytes.chunks(max) {
        let payload = SendToLinkPayload::from_slice(chunk)
            .map_err(|()| "object transfer does not fit in a link packet".to_string())?;
        if handle
            .issue(PrnsCommand::SendToLink(SendToLink {
                link_id: link,
                payload,
            }))
            .is_none()
        {
            return Err("object service stopped".to_string());
        }
    }
    Ok(())
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(HEX[usize::from(byte >> 4)] as char);
        text.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    text
}

#[cfg(unix)]
fn run_commands(
    config_dir: &std::path::Path,
    store: Arc<obstore::store::ObjectStore>,
    transfer: obstore::transfer::ObjectTransfer,
) {
    let socket = obstore::protocol::socket_path(config_dir);
    let listener = match obstore::protocol::bind_socket(&socket) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("object-services: {error}");
            return;
        }
    };
    struct SocketGuard(std::path::PathBuf);
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _guard = SocketGuard(socket);
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                let store = Arc::clone(&store);
                let transfer = transfer.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    if let Err(error) = obstore::protocol::serve_connection(
                        store.as_ref(),
                        Some(&transfer),
                        &mut stream,
                    ) {
                        eprintln!("object-services: {error}");
                    }
                });
            }
            Err(error) => eprintln!("object-services: {error}"),
        }
    }
}

#[cfg(unix)]
fn run_ingress(
    rx: Receiver<Ingress>,
    store: Arc<obstore::store::ObjectStore>,
    transfer: obstore::transfer::ObjectTransfer,
    handle: personal_rns::runtime::PrnsNodeHandle,
    links: Arc<Mutex<HashMap<LinkId, SyncSender<Vec<u8>>>>>,
    known: Arc<Mutex<std::collections::HashSet<LinkId>>>,
    jobs: tokio::sync::mpsc::Sender<Job>,
) {
    let mut fragments: HashMap<(u32, InterfaceId), PlainAssembly> = HashMap::new();
    let mut segments: HashMap<(LinkId, [u8; 32]), SegmentAssembly> = HashMap::new();
    while let Ok(event) = rx.recv() {
        match event {
            Ingress::Plain { interface, bytes } => {
                let Some(message) = assemble_plain(&mut fragments, interface, &bytes) else {
                    continue;
                };
                let store = Arc::clone(&store);
                let transfer = transfer.clone();
                let handle = handle.clone();
                std::thread::spawn(move || {
                    EXCLUDE_ARRIVAL.with(|cell| cell.set(Some(interface)));
                    let mut pipe = MemoryPipe {
                        cursor: std::io::Cursor::new(message),
                    };
                    let rate = interface_rate(&handle, interface);
                    if let Err(error) =
                        obstore::transfer::serve_peer(store.as_ref(), &transfer, &mut pipe, rate)
                    {
                        eprintln!("object-services: {error}");
                    }
                    EXCLUDE_ARRIVAL.with(|cell| cell.set(None));
                });
            }
            Ingress::Link { link, bytes } => {
                deliver_link(&store, &transfer, &links, &known, &jobs, link, bytes);
            }
            Ingress::Segment {
                link,
                hash,
                index,
                total,
                bytes,
            } => {
                let Some(message) =
                    assemble_segment(&mut segments, link, hash, index, total, bytes)
                else {
                    continue;
                };
                deliver_link(&store, &transfer, &links, &known, &jobs, link, message);
            }
            Ingress::Closed { link } => {
                known
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&link);
                links
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&link);
            }
        }
    }
}

#[cfg(unix)]
struct PlainAssembly {
    parts: Vec<Option<Vec<u8>>>,
}

#[cfg(unix)]
fn assemble_plain(
    fragments: &mut HashMap<(u32, InterfaceId), PlainAssembly>,
    interface: InterfaceId,
    bytes: &[u8],
) -> Option<Vec<u8>> {
    if bytes.len() < PLAIN_HEADER_LEN || &bytes[..4] != b"OBPL" || bytes[4] != 1 {
        return None;
    }
    let id = u32::from_le_bytes(bytes[5..9].try_into().ok()?);
    let index = u16::from_le_bytes(bytes[9..11].try_into().ok()?) as usize;
    let count = u16::from_le_bytes(bytes[11..13].try_into().ok()?) as usize;
    if count == 0 || count > MAX_PLAIN_PARTS || index >= count {
        return None;
    }
    let key = (id, interface);
    let assembly = fragments.entry(key).or_insert_with(|| PlainAssembly {
        parts: vec![None; count],
    });
    if assembly.parts.len() != count {
        *assembly = PlainAssembly {
            parts: vec![None; count],
        };
    }
    assembly.parts[index] = Some(bytes[PLAIN_HEADER_LEN..].to_vec());
    if assembly.parts.iter().any(Option::is_none) {
        return None;
    }
    let finished = fragments.remove(&key)?;
    let mut message = Vec::new();
    for part in finished.parts.into_iter().flatten() {
        message.extend(part);
    }
    Some(message)
}

#[cfg(unix)]
struct SegmentAssembly {
    total: u64,
    parts: HashMap<u64, Vec<u8>>,
}

#[cfg(unix)]
fn assemble_segment(
    segments: &mut HashMap<(LinkId, [u8; 32]), SegmentAssembly>,
    link: LinkId,
    hash: [u8; 32],
    index: u64,
    total: u64,
    bytes: Vec<u8>,
) -> Option<Vec<u8>> {
    if total == 0 || index >= total {
        return None;
    }
    if total == 1 {
        return Some(bytes);
    }
    let assembly = segments
        .entry((link, hash))
        .or_insert_with(|| SegmentAssembly {
            total,
            parts: HashMap::new(),
        });
    if assembly.total != total {
        assembly.total = total;
        assembly.parts.clear();
    }
    assembly.parts.insert(index, bytes);
    if u64::try_from(assembly.parts.len()).ok()? != total {
        return None;
    }
    let finished = segments.remove(&(link, hash))?;
    let mut message = Vec::new();
    for index in 0..finished.total {
        message.extend(finished.parts.get(&index)?.clone());
    }
    Some(message)
}

#[cfg(unix)]
fn deliver_link(
    store: &Arc<obstore::store::ObjectStore>,
    transfer: &obstore::transfer::ObjectTransfer,
    links: &Mutex<HashMap<LinkId, SyncSender<Vec<u8>>>>,
    known: &Mutex<std::collections::HashSet<LinkId>>,
    jobs: &tokio::sync::mpsc::Sender<Job>,
    link: LinkId,
    bytes: Vec<u8>,
) {
    let mut senders = links
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(sender) = senders.get(&link) {
        let _ = sender.send(bytes);
        return;
    }
    if !bytes.starts_with(b"OBXF") {
        return;
    }
    let (tx, rx) = mpsc::sync_channel(32);
    if tx.send(bytes).is_err() {
        return;
    }
    senders.insert(link, tx);
    drop(senders);
    known
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(link);
    let store = Arc::clone(store);
    let transfer = transfer.clone();
    let jobs = jobs.clone();
    std::thread::spawn(move || {
        let mut pipe = LinkPipe {
            jobs,
            link,
            await_proof: false,
            incoming: rx,
            pending_out: Vec::new(),
            pending_in: Vec::new(),
            pos: 0,
        };
        if let Err(error) =
            obstore::transfer::serve_peer(store.as_ref(), &transfer, &mut pipe, u64::MAX)
        {
            eprintln!("object-services: {error}");
        }
    });
}

#[cfg(unix)]
fn interface_rate(handle: &personal_rns::runtime::PrnsNodeHandle, id: InterfaceId) -> u64 {
    let Some(snapshot) = handle.interfaces().into_iter().find(|item| item.id == id) else {
        return u64::MAX;
    };
    let Some(rates) = snapshot.transfer_rates else {
        return u64::MAX;
    };
    let rate = u64::from(rates.rx_bps.min(rates.tx_bps));
    if rate == 0 {
        u64::MAX
    } else {
        rate
    }
}
