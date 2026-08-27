use personal_rns::runtime::NoPersistence;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use personal_hopspot_core::{
    load_host_ble_identity, load_host_node_identity, HopspotDestinationSet, IdentityPersistence,
    MobileEngineFailure, BLE_IDENTITY_STORAGE, NODE_IDENTITY_STORAGE,
};
use personal_rns::bluetooth_auto::BluetoothAuto;
use personal_rns::engine::{AnnounceIngressOutcome, AnnounceOrigin, AnnounceSourceKind};
use personal_rns::interfaces::bluetooth_auto::{
    AndroidHost, Endpoint, LinkCapabilities, BLE_HW_MTU,
};
use personal_rns::interfaces::wifi_direct::GoIntent;
use personal_rns::interfaces::InterfaceKind;
use personal_rns::runtime::{
    AnnounceEgressOutcome, Diagnostic, ManuallyAttached, PrnsEvent, PrnsNode, PrnsNodeHandle,
    PrnsNodeRecipe,
};
use personal_rns::shared_instance::rns_rpc::{SharedInstanceCredentials, SharedInstanceRpcServer};
use personal_rns::shared_instance::SharedInstanceServer;
use personal_rns::storage::GrowableHeap;
use personal_rns::usb_auto::{UsbAutoCandidate, UsbAutoHost};
use personal_rns::wifi_auto::AutoWifi;
use personal_rns::wifi_aware::WifiAwareAuto;
use personal_rns::wifi_direct::WifiDirectAuto;
use tokio::sync::oneshot;

use super::{
    persistence::{self, PersistenceRestore, PersistenceWorker},
    EnginePorts, EngineResources, EngineStartError, PlatformLinks, WorkerExit, ANDROID_PORT,
    ANNOUNCE_APP_DATA, NODE_ANNOUNCE_APP_DATA, USB_INTERFACE_ID, WORKER_SHUTDOWN_TIMEOUT,
};
use crate::bluetooth_auto::AndroidBleBackend;
use crate::bridge::BridgeStream;
use crate::wifi_aware::AndroidWifiAwareBackend;
use crate::wifi_direct::AndroidWifiDirectBackend;

pub(super) struct WorkerInput {
    pub(super) storage_dir: PathBuf,
    pub(super) platform: PlatformLinks,
    pub(super) ready_tx: Sender<Result<EngineResources, EngineStartError>>,
    pub(super) shutdown_rx: oneshot::Receiver<()>,
    pub(super) stopped_tx: Sender<WorkerExit>,
    pub(super) ports: EnginePorts,
}

pub(super) fn run(input: WorkerInput) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            let error = EngineStartError::RuntimeBuild;
            let _ = input.ready_tx.send(Err(error));
            let _ = input.stopped_tx.send(WorkerExit::Failed(error.failure()));
            return;
        }
    };
    let stopped_tx = input.stopped_tx.clone();
    let exit = runtime.block_on(run_engine(input));
    runtime.shutdown_timeout(WORKER_SHUTDOWN_TIMEOUT);
    let _ = stopped_tx.send(exit);
}

async fn run_engine(input: WorkerInput) -> WorkerExit {
    let WorkerInput {
        storage_dir,
        platform,
        ready_tx,
        shutdown_rx,
        ports,
        ..
    } = input;

    let node_bootstrap = load_host_node_identity(&storage_dir.join(NODE_IDENTITY_STORAGE.as_str()));
    if log_identity_persistence("node", node_bootstrap.persistence()).is_err() {
        return fail_start(ready_tx, EngineStartError::StorageConfiguration);
    }
    let node_identity = node_bootstrap.into_identity();
    let local_rpc_key = match super::local_rpc_key::load_or_create_local_rpc_key(&storage_dir) {
        Ok(key) => key,
        Err(error) => {
            log::error!("hopspot local RPC key unavailable: {error}");
            return fail_start(ready_tx, EngineStartError::StorageConfiguration);
        }
    };
    let credentials = SharedInstanceCredentials::from_identity_secret(node_identity.secret())
        .with_rpc_key(local_rpc_key.to_vec());
    let node_identity_hash = credentials.transport_identity_hash();
    let rpc_key = credentials.rpc_key().clone();
    let transport_secret = node_identity.transport_secret();

    let ble_bootstrap = load_host_ble_identity(&storage_dir.join(BLE_IDENTITY_STORAGE.as_str()));
    if log_identity_persistence("Bluetooth", ble_bootstrap.persistence()).is_err() {
        return fail_start(ready_tx, EngineStartError::StorageConfiguration);
    }
    let ble_identity = ble_bootstrap.into_identity();
    platform.ble.set_local_identity(ble_identity);

    let destinations = HopspotDestinationSet::new(
        node_identity.into_destination_secret(),
        ANNOUNCE_APP_DATA,
        NODE_ANNOUNCE_APP_DATA,
    );
    let destination_hashes = destinations
        .destination_hashes()
        .expect("the hopspot destination names are valid");

    let persistence_store = match persistence::open(&storage_dir) {
        Ok(persistence_store) => persistence_store,
        Err(error) => {
            log::error!("hopspot persistence storage unavailable: {error}");
            return fail_start(ready_tx, EngineStartError::StorageConfiguration);
        }
    };
    let timeline_origin = persistence_store.timeline_origin();
    let (rotated_tx, rotated_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut node = PrnsNode::new(PrnsNodeRecipe {
        transport_identity: Some(transport_secret),
        pre_configured_destinations: destinations.into_preconfigured_destinations(),
        app_state: (),
        storage: GrowableHeap,
        request_endpoints: personal_hopspot_core::node_pages::NodePageRoutes,
        interfaces: ManuallyAttached,
        persistence: NoPersistence,
        on_event: move |event, _state: &()| {
            match event {
                PrnsEvent::Diagnostic(Diagnostic::SelfRatchetRotated { destination }) => {
                    let _ = rotated_tx.send(destination);
                }
                PrnsEvent::Diagnostic(Diagnostic::AnnounceHeard {
                    destination,
                    hops,
                    source_interface,
                    app_data,
                }) => {
                    log_announce_relay(
                        source_interface.kind(),
                        hops,
                        app_data.len(),
                        destination.as_bytes(),
                        source_interface.as_bytes(),
                    );
                }
                PrnsEvent::Diagnostic(Diagnostic::AnnounceHeldDropped {
                    destination,
                    source_interface,
                    cause,
                }) => {
                    if is_sideband_or_ble(source_interface.kind()) {
                        let dest = destination.as_bytes();
                        log::warn!(
                            "hopspot: announce held-dropped from={:?} cause={cause:?} dest={:02x}{:02x}{:02x}{:02x}",
                            source_interface.kind(),
                            dest[0],
                            dest[1],
                            dest[2],
                            dest[3],
                        );
                    }
                }
                PrnsEvent::Diagnostic(Diagnostic::AnnounceIngestRejected {
                    destination,
                    source_interface,
                    reason,
                }) => {
                    if is_sideband_or_ble(source_interface.kind()) {
                        let dest = destination.as_bytes();
                        log::info!(
                            "hopspot: sideband announce REJECTED reason={reason:?} dest={:02x}{:02x}{:02x}{:02x} iface={:02x}{:02x}",
                            dest[0],
                            dest[1],
                            dest[2],
                            dest[3],
                            source_interface.as_bytes()[0],
                            source_interface.as_bytes()[1],
                        );
                    } else {
                        let dest = destination.as_bytes();
                        log::debug!(
                            "hopspot: announce REJECTED kind={:?} reason={reason:?} dest={:02x}{:02x}{:02x}{:02x}",
                            source_interface.kind(),
                            dest[0],
                            dest[1],
                            dest[2],
                            dest[3],
                        );
                    }
                }
                PrnsEvent::Diagnostic(Diagnostic::PacketForwarded {
                    source_interface,
                    fire_on,
                    destination,
                    hops,
                    packet_type,
                }) => {
                    if is_sideband_or_ble(source_interface.kind())
                        || is_sideband_or_ble(fire_on.kind())
                    {
                        let dest = destination.as_bytes();
                        log::info!(
                            "hopspot: packet FORWARD src={:?} -> dst_iface={:?} hops={hops} type={} dest={:02x}{:02x}{:02x}{:02x}",
                            source_interface.kind(),
                            fire_on.kind(),
                            packet_type_name(packet_type),
                            dest[0],
                            dest[1],
                            dest[2],
                            dest[3],
                        );
                    }
                }
                PrnsEvent::Diagnostic(Diagnostic::PacketForwardBlocked {
                    source_interface,
                    fire_on,
                    destination,
                    hops,
                    packet_type,
                }) => {
                    if is_sideband_or_ble(source_interface.kind())
                        || is_sideband_or_ble(fire_on.kind())
                    {
                        let dest = destination.as_bytes();
                        log::warn!(
                            "hopspot: packet FORWARD-BLOCKED src={:?} -> dst_iface={:?} hops={hops} type={} dest={:02x}{:02x}{:02x}{:02x} (egress ineligible)",
                            source_interface.kind(),
                            fire_on.kind(),
                            packet_type_name(packet_type),
                            dest[0],
                            dest[1],
                            dest[2],
                            dest[3],
                        );
                    }
                }
                PrnsEvent::Diagnostic(Diagnostic::PacketIgnored {
                    source_interface,
                    reason,
                }) => {
                    if is_sideband_or_ble(source_interface.kind()) {
                        log::info!(
                            "hopspot: packet IGNORED from={:?} reason={reason:?}",
                            source_interface.kind(),
                        );
                    }
                }
                PrnsEvent::Diagnostic(Diagnostic::PacketReceived {
                    source_interface,
                    packet_type,
                    destination,
                    bytes,
                }) => {
                    if source_interface.kind() == Some(InterfaceKind::LocalClient) {
                        match destination {
                            Some(dest) => {
                                let d = dest.as_bytes();
                                log::info!(
                                    "hopspot: LocalClient RX type={} bytes={bytes} dest={:02x}{:02x}{:02x}{:02x}",
                                    packet_type_name(packet_type),
                                    d[0],
                                    d[1],
                                    d[2],
                                    d[3],
                                );
                            }
                            None => {
                                log::info!(
                                    "hopspot: LocalClient RX type={} bytes={bytes} dest=?",
                                    packet_type_name(packet_type),
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        },
    })
    .with_timeline_origin(timeline_origin);
    let restored = persistence_store.restore(&mut node);
    let restore = PersistenceRestore::from_report(&restored);
    let handle = node.handle();
    let persistence =
        PersistenceWorker::new(handle.clone(), persistence_store, rotated_rx, restore);
    let persistence_health = persistence.health();

    let scan = {
        let bridge = platform.usb.clone();
        move || {
            if bridge.is_connected() {
                vec![UsbAutoCandidate::prns_specific(ANDROID_PORT)]
            } else {
                Vec::new()
            }
        }
    };
    let open = {
        let bridge = platform.usb.clone();
        move |_candidate: UsbAutoCandidate| {
            let bridge = bridge.clone();
            async move { Ok::<BridgeStream, io::Error>(bridge.open_stream()) }
        }
    };
    let usb = UsbAutoHost::new(USB_INTERFACE_ID, scan, open, platform.usb.rescan());
    let usb_status = usb.status();
    handle.add_interface(usb);

    let local_server = SharedInstanceServer::with_port(ports.local);
    #[cfg(target_os = "android")]
    let local_server = if ports.local != 0 {
        // Stock Sideband on Android uses AF_UNIX `\0rns/default`, not TCP 37428.
        local_server.also_listen_on_abstract_unix("default")
    } else {
        local_server
    };
    let local = match local_server.bind().await {
        Ok(local) => {
            #[cfg(target_os = "android")]
            if ports.local != 0 {
                if local.listens_on_abstract_unix() {
                    log::info!(
                        "hopspot: shared instance bus on TCP 127.0.0.1:{} and abstract unix rns/default",
                        ports.local
                    );
                } else {
                    log::warn!(
                        "hopspot: abstract unix rns/default unavailable; stock Sideband will not join this instance"
                    );
                }
            }
            local
        }
        Err(_) => return fail_start(ready_tx, EngineStartError::LocalListenerBind),
    };
    handle.supervise(local);

    let rpc = match SharedInstanceRpcServer::tcp(credentials.clone(), ports.rpc, handle.clone())
        .bind()
        .await
    {
        Ok(rpc) => rpc,
        Err(_) => return fail_start(ready_tx, EngineStartError::RpcListenerBind),
    };
    tokio::spawn(rpc.run());
    spawn_android_abstract_unix_rpc(credentials, ports.rpc, handle.clone());

    let service_discovery = match platform.service_discovery.take_service_discovery() {
        Ok(service_discovery) => service_discovery,
        Err(_service_discovery_unavailable) => {
            return fail_start(ready_tx, EngineStartError::WorkerStopped);
        }
    };
    let wifi = AutoWifi::new().with_platform_discovery(service_discovery);
    let wifi_status = wifi.status();
    handle.supervise(wifi);

    let bluetooth = BluetoothAuto::<_, { AndroidBleBackend::MAX_PEERS }>::new(
        AndroidBleBackend::new(platform.ble),
        ble_identity,
        Endpoint::Android(AndroidHost::Android),
        LinkCapabilities {
            l2cap: None,
            link_mtu: BLE_HW_MTU as u16,
        },
    );
    let ble_status = bluetooth.status();
    handle.supervise(bluetooth);

    let wifi_direct = WifiDirectAuto::new(
        AndroidWifiDirectBackend::new(platform.wifi_direct),
        GoIntent::PREFER_OWNER,
    );
    let wd_status = wifi_direct.status();
    handle.supervise(wifi_direct);

    let wifi_aware = WifiAwareAuto::new(AndroidWifiAwareBackend::new(platform.wifi_aware));
    let wa_status = wifi_aware.status();
    handle.supervise(wifi_aware);

    let resources = EngineResources {
        started_at: Instant::now(),
        node_identity_hash,
        ble_identity,
        usb_status,
        wifi_status,
        ble_status,
        wd_status,
        wa_status,
        handle: handle.clone(),
        destination: destination_hashes.delivery,
        node_page_destination: destination_hashes.node_page,
        rpc_key,
        ports,
        persistence: persistence_health,
    };
    let mut node_run = Box::pin(node.run());
    let initialized = {
        let initialization = persistence.initialize();
        tokio::pin!(initialization);
        tokio::select! {
            result = &mut node_run => {
                match result {
                    Ok(()) => {
                        log::error!("hopspot engine stopped during persistence initialization")
                    }
                    Err(error) => {
                        log::error!(
                            "hopspot engine stopped during persistence initialization: {error}"
                        )
                    }
                }
                return fail_start(ready_tx, EngineStartError::WorkerStopped);
            }
            result = &mut initialization => result,
        }
    };
    if initialized.is_err() {
        return fail_start(ready_tx, EngineStartError::PersistenceWrite);
    }
    if ready_tx.send(Ok(resources)).is_err() {
        return WorkerExit::Stopped;
    }

    spawn_sideband_announce_diag(handle.clone());

    let persistence_run = persistence.run(shutdown_rx);
    tokio::pin!(persistence_run);
    tokio::select! {
        result = &mut node_run => {
            match result {
                Ok(()) => log::error!("hopspot engine stopped"),
                Err(error) => log::error!("hopspot engine stopped: {error}"),
            }
            WorkerExit::Failed(MobileEngineFailure::WorkerStopped)
        }
        result = &mut persistence_run => {
            match result {
                Ok(()) => WorkerExit::Stopped,
                Err(()) => WorkerExit::Failed(MobileEngineFailure::PersistenceWrite),
            }
        }
    }
}

fn spawn_sideband_announce_diag(handle: PrnsNodeHandle) {
    tokio::spawn(async move {
        log::info!(
            "hopspot: sideband/BLE packet diagnostics enabled (adb logcat -s HopspotRust:I)"
        );
        let mut prev = SidebandAnnounceDiag::default();
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let Some(snapshot) = handle.metrics_snapshot().await else {
                continue;
            };
            let next = SidebandAnnounceDiag::from_metrics(&snapshot);
            if next == prev {
                continue;
            }
            log::info!(
                "hopspot: sideband announce metrics ingress accepted={} ignored={} held={} blackholed={} sched_reject={} | egress enqueued={} unavailable={} lane_full={} lane_missing={} ifac_reject={} pacer_reject={} | pacer_depth={} scheduled_depth={} held_depth={}",
                next.ingress_accepted,
                next.ingress_ignored,
                next.ingress_held,
                next.ingress_blackholed,
                next.ingress_sched_reject,
                next.egress_enqueued,
                next.egress_unavailable,
                next.egress_lane_full,
                next.egress_lane_missing,
                next.egress_ifac_reject,
                next.egress_pacer_reject,
                next.pacer_depth,
                next.scheduled_depth,
                next.held_depth,
            );
            if !next.interface_lines.is_empty() {
                for line in &next.interface_lines {
                    log::info!("hopspot: announce iface {line}");
                }
            }
            prev = next;
        }
    });
}

#[derive(Default, Clone, PartialEq, Eq)]
struct SidebandAnnounceDiag {
    ingress_accepted: u64,
    ingress_ignored: u64,
    ingress_held: u64,
    ingress_blackholed: u64,
    ingress_sched_reject: u64,
    egress_enqueued: u64,
    egress_unavailable: u64,
    egress_lane_full: u64,
    egress_lane_missing: u64,
    egress_ifac_reject: u64,
    egress_pacer_reject: u64,
    pacer_depth: u32,
    scheduled_depth: u32,
    held_depth: u32,
    interface_lines: std::vec::Vec<String>,
}

impl SidebandAnnounceDiag {
    fn from_metrics(snapshot: &personal_rns::runtime::RuntimeMetricsSnapshot) -> Self {
        let ingress = &snapshot.engine.announces.ingress;
        let shared = AnnounceSourceKind::SharedClient;
        let egress = &snapshot.egress.announces;
        let origin = AnnounceOrigin::SharedClient;
        let mut interface_lines = std::vec::Vec::new();
        for entry in &egress.interfaces {
            let depth = entry.pacer_queue_depth;
            let enqueued = entry
                .outcomes
                .get(origin, AnnounceEgressOutcome::Enqueued);
            let unavailable = entry
                .outcomes
                .get(origin, AnnounceEgressOutcome::InterfaceUnavailable);
            let full = entry.outcomes.get(origin, AnnounceEgressOutcome::LaneFull);
            let missing = entry
                .outcomes
                .get(origin, AnnounceEgressOutcome::LaneMissing);
            if depth == 0 && enqueued == 0 && unavailable == 0 && full == 0 && missing == 0 {
                continue;
            }
            let id = entry.interface.as_bytes();
            interface_lines.push(format!(
                "kind={:?} id={:02x}{:02x} pacer={depth} enq={enqueued} unavail={unavailable} full={full} missing={missing}",
                entry.interface.kind(),
                id[0],
                id[1],
            ));
        }
        for entry in &snapshot.engine.announces.interfaces {
            if entry.held_depth == 0 && entry.scheduled_depth == 0 {
                continue;
            }
            let id = entry.interface.as_bytes();
            interface_lines.push(format!(
                "kind={:?} id={:02x}{:02x} held_depth={} scheduled_depth={}",
                entry.interface.kind(),
                id[0],
                id[1],
                entry.held_depth,
                entry.scheduled_depth,
            ));
        }
        interface_lines.sort();
        Self {
            ingress_accepted: ingress.get(shared, AnnounceIngressOutcome::Accepted),
            ingress_ignored: ingress.get(shared, AnnounceIngressOutcome::Ignored),
            ingress_held: ingress.get(shared, AnnounceIngressOutcome::Held),
            ingress_blackholed: ingress.get(shared, AnnounceIngressOutcome::Blackholed),
            ingress_sched_reject: ingress.get(
                shared,
                AnnounceIngressOutcome::AcceptedScheduleRejectedQueueFull,
            ),
            egress_enqueued: egress.outcomes.get(origin, AnnounceEgressOutcome::Enqueued),
            egress_unavailable: egress
                .outcomes
                .get(origin, AnnounceEgressOutcome::InterfaceUnavailable),
            egress_lane_full: egress.outcomes.get(origin, AnnounceEgressOutcome::LaneFull),
            egress_lane_missing: egress
                .outcomes
                .get(origin, AnnounceEgressOutcome::LaneMissing),
            egress_ifac_reject: egress
                .outcomes
                .get(origin, AnnounceEgressOutcome::IfacRejected),
            egress_pacer_reject: egress
                .outcomes
                .get(origin, AnnounceEgressOutcome::PacerRejected),
            pacer_depth: egress.pacer_queue_depth,
            scheduled_depth: snapshot.engine.announces.scheduled_depth,
            held_depth: snapshot.engine.announces.held_depth,
            interface_lines,
        }
    }
}

fn spawn_android_abstract_unix_rpc(
    credentials: SharedInstanceCredentials,
    rpc_port: u16,
    handle: PrnsNodeHandle,
) {
    #[cfg(target_os = "android")]
    {
        if rpc_port == 0 {
            return;
        }
        tokio::spawn(async move {
            match SharedInstanceRpcServer::abstract_unix(credentials, "default", handle)
                .bind()
                .await
            {
                Ok(rpc) => {
                    log::info!("hopspot: shared instance RPC on abstract unix rns/default/rpc");
                    rpc.run().await;
                }
                Err(error) => {
                    log::warn!(
                        "hopspot: abstract unix RPC rns/default/rpc unavailable: {error:?}"
                    )
                }
            }
        });
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (credentials, rpc_port, handle);
    }
}

fn fail_start(
    ready_tx: Sender<Result<EngineResources, EngineStartError>>,
    error: EngineStartError,
) -> WorkerExit {
    let _ = ready_tx.send(Err(error));
    WorkerExit::Failed(error.failure())
}

fn is_sideband_or_ble(kind: Option<InterfaceKind>) -> bool {
    matches!(
        kind,
        Some(InterfaceKind::LocalClient | InterfaceKind::BluetoothAuto)
    )
}

fn packet_type_name(packet_type: u8) -> &'static str {
    match packet_type {
        0 => "data",
        1 => "announce",
        2 => "link_request",
        3 => "proof",
        _ => "unknown",
    }
}

fn log_announce_relay(
    source_kind: Option<InterfaceKind>,
    hops: u8,
    app_data_bytes: usize,
    dest: &[u8],
    iface: &[u8],
) {
    if !is_sideband_or_ble(source_kind) {
        log::debug!(
            "hopspot: announce heard kind={source_kind:?} hops={hops} app_data_bytes={app_data_bytes} dest={:02x}{:02x}{:02x}{:02x}",
            dest.first().copied().unwrap_or(0),
            dest.get(1).copied().unwrap_or(0),
            dest.get(2).copied().unwrap_or(0),
            dest.get(3).copied().unwrap_or(0),
        );
        return;
    }
    let label = match source_kind {
        Some(InterfaceKind::LocalClient) => "sideband",
        Some(InterfaceKind::BluetoothAuto) => "ble",
        _ => "peer",
    };
    log::info!(
        "hopspot: {label} announce heard hops={hops} app_data_bytes={app_data_bytes} dest={:02x}{:02x}{:02x}{:02x} iface={:02x}{:02x}",
        dest.first().copied().unwrap_or(0),
        dest.get(1).copied().unwrap_or(0),
        dest.get(2).copied().unwrap_or(0),
        dest.get(3).copied().unwrap_or(0),
        iface.first().copied().unwrap_or(0),
        iface.get(1).copied().unwrap_or(0),
    );
}

fn log_identity_persistence(
    identity: &str,
    persistence: &IdentityPersistence<impl core::fmt::Display>,
) -> Result<(), ()> {
    match persistence {
        IdentityPersistence::Loaded => Ok(()),
        IdentityPersistence::Created => {
            log::info!("{identity} identity created");
            Ok(())
        }
        IdentityPersistence::Recovered(error) => {
            log::warn!("{identity} identity recovered after corruption: {error}");
            Ok(())
        }
        IdentityPersistence::Ephemeral(error) => {
            log::error!("{identity} identity is ephemeral: {error}");
            Err(())
        }
    }
}
