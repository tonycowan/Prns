use embassy_futures::select::{select, select6, Either, Either6};
use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Receiver;
use embassy_sync::signal::Signal;
use heapless::Vec as HeaplessVec;

use crate::engine::{
    ClassifiedInboundPacket, Departure, EngineState, IngestIo, IssuedCommand, Journaled,
    ProofRequest,
};
use crate::interfaces::rns_management::{
    write_route_snapshots, RNS_PATH_TABLE_MAX_ENCODED_ENTRY_BYTES,
};
use crate::interfaces::InterfaceIfac;
use crate::interfaces::{
    AttachedInterfaces, IfacUnmaskError, InboundPacket, InterfaceDescriptor, InterfaceId,
    InterfaceMode,
};
use crate::manifold::grant::{FrameTarget, ManifoldLaneReader};
use crate::manifold::interface_seam::{EMBEDDED_MAX_LINK_MTU, EMBEDDED_MAX_WIRE_FRAME_LEN};
use crate::manifold::timers::{wait_for_due_reason, wait_for_pacer};
use crate::manifold::wake_schedule::{fire_due_reason, merge_wake_schedules_delta};
use crate::manifold::{AppDeciders, Host};
use crate::remote_control::RemoteControlPathInventory;
#[cfg(feature = "remote-control-path-table")]
use crate::remote_control::{
    RemoteControlPathEntry, RemoteControlPathPage, RemoteControlPathPageBuilder,
};
use crate::routing::links::request::{response_envelope_prefix, RESPONSE_WIRE_OVERHEAD};
use crate::routing::links::resources::ResourceOffer;
use crate::routing::links::resources::{
    ResourceBody, ResourceCorrelation, ResourceMetadata, ResourceSend,
};
use crate::runtime::{
    InterfaceInspectionStore, ManifoldPersistence, ResourceResponse, ResourceResponsePayload,
};
use crate::storage::{DirtyInterfaceSet, StorageLayout};

use super::egress::{
    flush_due_pacers, ifac_for, route_reaction, soonest_pacer_release, InterfacePacer,
    ManifoldEgress, PooledEgress,
};
use super::inline_work::{
    fulfill_owed_work_inline, route_and_capture_owed_work, InlineOwedWorkQueue,
};
use super::interface_status::account_protocol_violation;
use super::packet_phy::retain_packet_phy;
use super::EmbassyInterfaceStatus;

// Use one callback type for ingress and command continuations so their large
// inline-work dispatcher is shared in firmware instead of monomorphized twice.
fn persist_and_notify<'a, S: StorageLayout>(
    persistence: &'a mut impl ManifoldPersistence<S>,
    on_journaled: &'a mut impl FnMut(Journaled<'_>),
    now: crate::engine::InstantMillis,
) -> impl FnMut(Journaled<'_>) + 'a {
    move |journaled| {
        persistence.observe(&journaled, now);
        on_journaled(journaled);
    }
}

const RNS_PATH_TABLE_MAX_ENTRIES: usize = 8;
pub const RNS_PATH_TABLE_RESPONSE_BYTES: usize = RESPONSE_WIRE_OVERHEAD
    + 1
    + RNS_PATH_TABLE_MAX_ENTRIES * RNS_PATH_TABLE_MAX_ENCODED_ENTRY_BYTES;

#[derive(Debug, PartialEq, Eq)]
pub enum InterfacePublicationOutcome {
    Published,
    UnknownInterface,
    IdentityConflict,
}

/// Changes the live descriptor set without reallocating the fixed lane pool.
#[repr(C)]
pub enum InterfaceLifecycle<'a> {
    Publish {
        old_id: InterfaceId,
        descriptor: InterfaceDescriptor,
        completion: &'a embassy_sync::signal::Signal<
            embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex,
            InterfacePublicationOutcome,
        >,
    },
    Add {
        descriptor: InterfaceDescriptor,
    },
    Remove {
        id: InterfaceId,
    },
    Update {
        descriptor: InterfaceDescriptor,
    },
    Retag {
        old_id: InterfaceId,
        new_id: InterfaceId,
        descriptor: InterfaceDescriptor,
    },
    SetMode {
        id: InterfaceId,
        mode: InterfaceMode,
    },
}

fn adopt_stored_mode(
    store: &impl InterfaceInspectionStore,
    mut descriptor: InterfaceDescriptor,
) -> InterfaceDescriptor {
    if let Some(mode) = store.interface_mode(descriptor.id) {
        descriptor.mode = mode;
    } else {
        store.set_interface_mode(descriptor.id, descriptor.mode);
    }
    descriptor
}

fn clamp_to_embedded_ceiling(mut descriptor: InterfaceDescriptor) -> InterfaceDescriptor {
    if let Some(mtu) = descriptor.hardware_mtu {
        descriptor.hardware_mtu = Some(mtu.min(EMBEDDED_MAX_LINK_MTU));
    }
    descriptor
}

fn publish_interface<const LANES: usize, const INTERFACES: usize>(
    old_id: InterfaceId,
    descriptor: InterfaceDescriptor,
    descriptors: &mut HeaplessVec<InterfaceDescriptor, INTERFACES>,
    egress: &mut PooledEgress<LANES>,
    inbound: &mut HeaplessVec<(InterfaceId, &'static mut dyn ManifoldLaneReader), LANES>,
    ifacs: &mut HeaplessVec<InterfaceIfac, LANES>,
    pacers: &mut HeaplessVec<InterfacePacer, LANES>,
) -> InterfacePublicationOutcome {
    let Some(slot) = descriptors
        .iter()
        .position(|existing| existing.id == old_id)
    else {
        return InterfacePublicationOutcome::UnknownInterface;
    };
    let Some(old_lane) = egress.lane_for(old_id) else {
        return InterfacePublicationOutcome::UnknownInterface;
    };
    let new_id = descriptor.id;
    if old_id != new_id && descriptors.iter().any(|existing| existing.id == new_id) {
        return InterfacePublicationOutcome::IdentityConflict;
    }
    let descriptor = clamp_to_embedded_ceiling(descriptor);
    descriptors[slot] = descriptor;
    egress.retag(old_id, new_id);
    if let Some(entry) = inbound.iter_mut().find(|(id, _)| *id == old_id) {
        entry.0 = new_id;
    }
    if let Some(entry) = ifacs.iter_mut().find(|entry| entry.id == old_id) {
        entry.id = new_id;
    }
    if let Some(pos) = pacers.iter().position(|pacer| pacer.id == old_lane) {
        // A shared fleet lane retains its lane identity; a dedicated lane takes the new id.
        let new_lane = if old_lane == old_id { new_id } else { old_lane };
        pacers[pos] = InterfacePacer::from_descriptor(new_lane, &descriptor);
    }
    InterfacePublicationOutcome::Published
}

fn inbound_source(
    lane_id: InterfaceId,
    stamped_source: InterfaceId,
    descriptors: &[InterfaceDescriptor],
) -> InterfaceId {
    if descriptors
        .iter()
        .any(|descriptor| descriptor.id == lane_id)
    {
        lane_id
    } else {
        stamped_source
    }
}

// Both variants stay inline because embedded runtimes cannot rely on heap indirection.
#[allow(clippy::large_enum_variant)]
enum MaterializedResourceResponse<const N: usize> {
    Ready(HeaplessVec<u8, N>),
    RnsPathTable(HeaplessVec<u8, RNS_PATH_TABLE_RESPONSE_BYTES>),
}

impl<const N: usize> MaterializedResourceResponse<N> {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Ready(data) => data,
            Self::RnsPathTable(data) => data,
        }
    }
}

#[inline(never)]
fn resource_response_data<S: StorageLayout, const N: usize>(
    engine: &EngineState<S>,
    descriptors: &[InterfaceDescriptor],
    request_id: crate::routing::links::request::RequestId,
    payload: ResourceResponsePayload<N>,
) -> Option<MaterializedResourceResponse<N>> {
    match payload {
        ResourceResponsePayload::Ready(data) => Some(MaterializedResourceResponse::Ready(data)),
        ResourceResponsePayload::RnsPathTable(selection) => {
            let entries = engine.bounded_route_snapshots::<RNS_PATH_TABLE_MAX_ENTRIES>(
                AttachedInterfaces::new(descriptors),
                |entry| selection.includes(entry.destination, entry.hops),
            );
            let mut data = HeaplessVec::new();
            data.resize(RNS_PATH_TABLE_RESPONSE_BYTES, 0).ok()?;
            data.get_mut(..RESPONSE_WIRE_OVERHEAD)?
                .copy_from_slice(&response_envelope_prefix(&request_id));
            let written = write_route_snapshots(
                entries.entries(),
                data.get_mut(RESPONSE_WIRE_OVERHEAD..RNS_PATH_TABLE_RESPONSE_BYTES)?,
            )
            .ok()?;
            data.truncate(RESPONSE_WIRE_OVERHEAD + written);
            Some(MaterializedResourceResponse::RnsPathTable(data))
        }
        #[cfg(feature = "remote-control-path-table")]
        ResourceResponsePayload::RemoteControlPathPage(_) => None,
    }
}

#[cfg(feature = "remote-control-path-table")]
#[inline(never)]
fn remote_control_path_inventory<S: StorageLayout>(
    engine: &EngineState<S>,
    descriptors: &[InterfaceDescriptor],
    page: RemoteControlPathPage,
) -> RemoteControlPathInventory {
    let mut builder = RemoteControlPathPageBuilder::new(page);
    engine.visit_route_snapshots(AttachedInterfaces::new(descriptors), |snapshot| {
        builder.observe(RemoteControlPathEntry::new(
            snapshot.destination,
            snapshot.hops,
            snapshot.via,
            snapshot.interface,
            snapshot.learned_at.0,
            snapshot.expires_at.0,
        ));
    });
    builder.finish()
}

/// Borrowed lanes and channels for one pooled-topology manifold run.
pub struct PooledWiring<
    'run,
    'publication,
    M: RawMutex + 'static,
    const LANE_COUNT: usize,
    const INTERFACE_CAPACITY: usize,
    const NOTIFY: usize,
    const COMMANDS: usize,
    const RESPONSE_BYTES: usize,
    const LIFECYCLE: usize,
> {
    pub descriptors: &'run mut HeaplessVec<InterfaceDescriptor, INTERFACE_CAPACITY>,
    pub ifacs: &'run mut HeaplessVec<InterfaceIfac, LANE_COUNT>,
    pub inbound:
        &'run mut HeaplessVec<(InterfaceId, &'static mut dyn ManifoldLaneReader), LANE_COUNT>,
    pub frame_accounting_statuses: &'run [&'static EmbassyInterfaceStatus],
    pub egress: &'run mut PooledEgress<LANE_COUNT>,
    pub notify: Receiver<'run, M, InterfaceId, NOTIFY>,
    pub commands: Receiver<'run, M, IssuedCommand, COMMANDS>,
    pub resource_responses: Receiver<'run, M, ResourceResponse<RESPONSE_BYTES>, 1>,
    pub path_page_reply: &'run Signal<M, RemoteControlPathInventory>,
    pub lifecycle: Receiver<'run, M, InterfaceLifecycle<'publication>, LIFECYCLE>,
}

/// Runs a mutable descriptor set over a fixed lane pool; `LANE_COUNT` bounds pacers.
pub(crate) async fn run_pooled<
    S,
    H,
    M,
    Store,
    const LANE_COUNT: usize,
    const INTERFACE_CAPACITY: usize,
    const NOTIFY: usize,
    const COMMANDS: usize,
    const RESPONSE_BYTES: usize,
    const LIFECYCLE: usize,
>(
    engine: &mut EngineState<S>,
    host: &mut H,
    wiring: PooledWiring<
        '_,
        '_,
        M,
        LANE_COUNT,
        INTERFACE_CAPACITY,
        NOTIFY,
        COMMANDS,
        RESPONSE_BYTES,
        LIFECYCLE,
    >,
    mut on_journaled: impl FnMut(Journaled<'_>),
    deciders: AppDeciders<impl FnMut(&ProofRequest) -> bool, impl FnMut(&ResourceOffer) -> bool>,
    store: &Store,
    persistence: &mut impl ManifoldPersistence<S>,
) where
    S: StorageLayout,
    H: Host,
    M: RawMutex + 'static,
    Store: InterfaceInspectionStore,
{
    engine.use_inline_resource_work();
    let AppDeciders {
        mut should_prove,
        mut should_accept_resource,
    } = deciders;
    let PooledWiring {
        descriptors,
        ifacs,
        inbound,
        frame_accounting_statuses,
        egress,
        notify,
        commands,
        resource_responses,
        path_page_reply,
        lifecycle,
    } = wiring;
    #[cfg(not(feature = "remote-control-path-table"))]
    let _ = path_page_reply;
    let mut pacers: HeaplessVec<InterfacePacer, LANE_COUNT> = HeaplessVec::new();
    for descriptor in descriptors.iter_mut() {
        *descriptor = adopt_stored_mode(store, clamp_to_embedded_ceiling(*descriptor));
        engine.interface_attached(descriptor.id, host.now());
        if let Some(lane) = egress.lane_for(descriptor.id) {
            if !pacers.iter().any(|pacer| pacer.id == lane) {
                let _ = pacers.push(InterfacePacer::from_descriptor(lane, descriptor));
            }
        }
    }
    let mut wake_schedules = engine.wake_schedules(AttachedInterfaces::new(&*descriptors));
    loop {
        let wake = wake_schedules.soonest(host.now());
        let pacer_wake = soonest_pacer_release(&pacers);

        let persistence_deadline = persistence.deadline(host.now());
        match select6(
            notify.receive(),
            select(commands.receive(), resource_responses.receive()),
            wait_for_due_reason(&*host, wake),
            wait_for_pacer(&*host, pacer_wake),
            lifecycle.receive(),
            wait_for_persistence_or_work(&*host, persistence_deadline, &*persistence),
        )
        .await
        {
            Either6::First(_) => {
                while notify.try_receive().is_ok() {}
                for (lane_id, lane) in inbound.iter_mut() {
                    while let Some((target, packet_phy, frame)) = lane.try_read() {
                        let FrameTarget::Direct(stamped_source) = target else {
                            lane.release();
                            continue;
                        };
                        // A dedicated lane's live key is authoritative. Runtime retagging updates
                        // that key atomically with the descriptor, while the already-constructed
                        // interface seam can still have stamped a queued frame with its prior id.
                        // Fleet lanes have no descriptor of their own, so their per-member stamp
                        // remains authoritative instead.
                        let source = inbound_source(*lane_id, stamped_source, descriptors);
                        #[cfg(feature = "log")]
                        {
                            log::info!("lane {}b", frame.len());
                            yield_now().await;
                        }
                        let mut unmasked = [0u8; EMBEDDED_MAX_WIRE_FRAME_LEN];
                        let bytes = match ifac_for(ifacs, *lane_id) {
                            Some(entry) => {
                                match entry.context.try_unmask_inbound(frame, &mut unmasked) {
                                    Ok(clean_len) => &mut unmasked[..clean_len],
                                    Err(IfacUnmaskError::PacketTooShort) => {
                                        account_protocol_violation(
                                            frame_accounting_statuses,
                                            source,
                                            Some(
                                                crate::engine::ProtocolViolationKind::InvalidIfacEnvelope,
                                            ),
                                        );
                                        lane.release();
                                        continue;
                                    }
                                    Err(
                                        IfacUnmaskError::MissingFlag
                                        | IfacUnmaskError::InvalidSignature
                                        | IfacUnmaskError::OutputTooSmall { .. },
                                    ) => {
                                        lane.release();
                                        continue;
                                    }
                                }
                            }
                            None => frame,
                        };
                        let now = host.now();
                        #[cfg(feature = "log")]
                        let rx_len = bytes.len();
                        #[cfg(feature = "log")]
                        {
                            note_inbound(bytes, source);
                            yield_now().await;
                        }
                        let mut packet = ClassifiedInboundPacket::classify(InboundPacket {
                            arrived_at: now,
                            source_interface: source,
                            bytes,
                        });
                        retain_packet_phy(store, &mut packet, packet_phy);
                        let mut owed_work = InlineOwedWorkQueue::new();
                        let report = engine.ingest_classified_into_report_with_request_diagnostics::<
                            { cfg!(feature = "log") }, _, _, _, _,
                        >(
                            packet,
                            IngestIo {
                                interfaces: AttachedInterfaces::new(&*descriptors),
                                now,
                                fill_random: &mut |entropy| host.fill_random(entropy),
                                should_prove: &mut should_prove,
                                should_accept_resource: &mut should_accept_resource,
                                sink: &mut |reaction| {
                                    route_and_capture_owed_work(
                                        reaction,
                                        &mut *egress,
                                        ifacs,
                                        &mut pacers,
                                        now,
                                        &mut persist_and_notify(persistence, &mut on_journaled, now),
                                        &mut owed_work,
                                    )
                                },
                            },
                        );
                        #[cfg(feature = "log")]
                        {
                            log::info!("rx back {rx_len}b");
                            yield_now().await;
                        }
                        let completion_delta = fulfill_owed_work_inline(
                            owed_work,
                            &mut *engine,
                            &mut *host,
                            AttachedInterfaces::new(&*descriptors),
                            &mut *egress,
                            ifacs,
                            &mut pacers,
                            frame_accounting_statuses,
                            now,
                            &mut should_prove,
                            &mut persist_and_notify(persistence, &mut on_journaled, now),
                        );
                        #[cfg(feature = "log")]
                        {
                            log::info!("owed back");
                            yield_now().await;
                        }
                        account_protocol_violation(
                            frame_accounting_statuses,
                            source,
                            report.protocol_violation,
                        );
                        #[cfg(feature = "log")]
                        if let Some(request) = report.request {
                            log::debug!(target: "prns::request", "request ingress: {request:?}");
                        }
                        lane.release();
                        let mut step_delta = report.wake_schedules;
                        step_delta.compose(completion_delta);
                        merge_wake_schedules_delta(
                            &mut wake_schedules,
                            step_delta,
                            &*engine,
                            AttachedInterfaces::new(&*descriptors),
                        );
                    }
                }
            }
            Either6::Second(input) => {
                let now = host.now();
                let mut owed_work = InlineOwedWorkQueue::new();
                let mut delta = match input {
                    Either::First(issued) => engine.ingest_command_into_with_work(
                        issued,
                        AttachedInterfaces::new(&*descriptors),
                        now,
                        &mut |entropy| host.fill_random(entropy),
                        &mut |reaction| {
                            route_and_capture_owed_work(
                                reaction,
                                &mut *egress,
                                ifacs,
                                &mut pacers,
                                now,
                                &mut persist_and_notify(persistence, &mut on_journaled, now),
                                &mut owed_work,
                            )
                        },
                    ),
                    Either::Second(response) => {
                        #[cfg(feature = "remote-control-path-table")]
                        if let ResourceResponsePayload::RemoteControlPathPage(page) =
                            response.payload
                        {
                            path_page_reply.signal(remote_control_path_inventory(
                                engine,
                                descriptors,
                                page,
                            ));
                            continue;
                        }
                        let Some(data) = resource_response_data(
                            engine,
                            descriptors,
                            response.request_id,
                            response.payload,
                        ) else {
                            continue;
                        };
                        engine.ingest_send_resource_into(
                            &ResourceSend {
                                id: response.id,
                                link_id: response.link_id,
                                body: ResourceBody {
                                    data: data.as_slice(),
                                    compressed_candidate: None,
                                    metadata: ResourceMetadata::None,
                                },
                                correlation: ResourceCorrelation::Response(response.request_id),
                            },
                            now,
                            &mut |entropy| host.fill_random(entropy),
                            &mut |reaction| {
                                route_reaction(
                                    reaction,
                                    &mut *egress,
                                    ifacs,
                                    &mut pacers,
                                    now,
                                    &mut persist_and_notify(persistence, &mut on_journaled, now),
                                )
                            },
                        )
                    }
                };
                delta.merge(fulfill_owed_work_inline(
                    owed_work,
                    &mut *engine,
                    &mut *host,
                    AttachedInterfaces::new(&*descriptors),
                    &mut *egress,
                    ifacs,
                    &mut pacers,
                    frame_accounting_statuses,
                    now,
                    &mut should_prove,
                    &mut persist_and_notify(persistence, &mut on_journaled, now),
                ));
                merge_wake_schedules_delta(
                    &mut wake_schedules,
                    delta,
                    &*engine,
                    AttachedInterfaces::new(&*descriptors),
                );
            }
            Either6::Third(reason) => {
                let now = host.now();
                let delta = fire_due_reason(
                    &mut *engine,
                    reason,
                    now,
                    AttachedInterfaces::new(&*descriptors),
                    &mut |bytes| host.fill_random(bytes),
                    &mut |reaction| {
                        route_reaction(
                            reaction,
                            &mut *egress,
                            ifacs,
                            &mut pacers,
                            now,
                            &mut persist_and_notify(persistence, &mut on_journaled, now),
                        )
                    },
                );
                merge_wake_schedules_delta(
                    &mut wake_schedules,
                    delta,
                    &*engine,
                    AttachedInterfaces::new(&*descriptors),
                );
            }
            Either6::Fourth(()) => {
                let now = host.now();
                flush_due_pacers(&mut pacers, now, &mut *egress, ifacs);
            }
            Either6::Fifth(message) => match message {
                InterfaceLifecycle::Publish {
                    old_id,
                    descriptor,
                    completion,
                } => {
                    let outcome = publish_interface(
                        old_id,
                        descriptor,
                        descriptors,
                        egress,
                        inbound,
                        ifacs,
                        &mut pacers,
                    );
                    if outcome == InterfacePublicationOutcome::Published {
                        wake_schedules =
                            engine.wake_schedules(AttachedInterfaces::new(&*descriptors));
                    }
                    completion.signal(outcome);
                }
                InterfaceLifecycle::Add { descriptor } => {
                    let descriptor =
                        adopt_stored_mode(store, clamp_to_embedded_ceiling(descriptor));
                    let id = descriptor.id;
                    let present = descriptors.iter().any(|existing| existing.id == id);
                    if !present {
                        engine.interface_attached(id, host.now());
                        let _ = descriptors.push(descriptor);
                        if let Some(lane) = egress.lane_for(id) {
                            if !pacers.iter().any(|pacer| pacer.id == lane) {
                                let _ =
                                    pacers.push(InterfacePacer::from_descriptor(lane, &descriptor));
                            }
                        }
                        wake_schedules =
                            engine.wake_schedules(AttachedInterfaces::new(&*descriptors));
                    }
                    #[cfg(feature = "log")]
                    log::info!(
                        target: "personal_hopspot_esp32",
                        "manifold: Add kind={:?} present={present} descriptors={}",
                        id.kind(),
                        descriptors.len()
                    );
                }
                InterfaceLifecycle::Remove { id } => {
                    let now = host.now();
                    let departed_lane = egress.lane_for(id);
                    engine.interface_departed(id, Departure::Forgotten, now);
                    let found = descriptors
                        .iter()
                        .position(|descriptor| descriptor.id == id);
                    if let Some(pos) = found {
                        let _ = descriptors.swap_remove(pos);
                    }
                    #[cfg(feature = "log")]
                    log::info!(
                        target: "personal_hopspot_esp32",
                        "manifold: Remove kind={:?} found={} descriptors={}",
                        id.kind(),
                        found.is_some(),
                        descriptors.len()
                    );
                    if let Some(lane) = departed_lane {
                        let lane_still_serves_a_descriptor = descriptors
                            .iter()
                            .any(|descriptor| egress.lane_for(descriptor.id) == Some(lane));
                        if !lane_still_serves_a_descriptor {
                            if let Some(pos) = pacers.iter().position(|pacer| pacer.id == lane) {
                                let _ = pacers.swap_remove(pos);
                            }
                        }
                    }
                    engine.cull_expired_routes(
                        now,
                        AttachedInterfaces::new(&*descriptors),
                        &mut |reaction| {
                            route_reaction(
                                reaction,
                                &mut *egress,
                                ifacs,
                                &mut pacers,
                                now,
                                &mut persist_and_notify(persistence, &mut on_journaled, now),
                            )
                        },
                    );
                    wake_schedules = engine.wake_schedules(AttachedInterfaces::new(&*descriptors));
                }
                InterfaceLifecycle::Update { descriptor } => {
                    let descriptor =
                        adopt_stored_mode(store, clamp_to_embedded_ceiling(descriptor));
                    if let Some(slot) = descriptors
                        .iter()
                        .position(|existing| existing.id == descriptor.id)
                    {
                        descriptors[slot] = descriptor;
                        if let Some(lane) = egress.lane_for(descriptor.id) {
                            if let Some(pos) = pacers.iter().position(|pacer| pacer.id == lane) {
                                pacers[pos] = InterfacePacer::from_descriptor(lane, &descriptor);
                            }
                        }
                        wake_schedules =
                            engine.wake_schedules(AttachedInterfaces::new(&*descriptors));
                    }
                }
                InterfaceLifecycle::Retag {
                    old_id,
                    new_id,
                    descriptor,
                } => {
                    if descriptor.id == new_id
                        && publish_interface(
                            old_id,
                            descriptor,
                            descriptors,
                            egress,
                            inbound,
                            ifacs,
                            &mut pacers,
                        ) == InterfacePublicationOutcome::Published
                    {
                        wake_schedules =
                            engine.wake_schedules(AttachedInterfaces::new(&*descriptors));
                    }
                }
                InterfaceLifecycle::SetMode { id, mode } => {
                    store.set_interface_mode(id, mode);
                    if let Some(slot) = descriptors.iter().position(|existing| existing.id == id) {
                        descriptors[slot].mode = mode;
                        if let Some(lane) = egress.lane_for(id) {
                            if let Some(pos) = pacers.iter().position(|pacer| pacer.id == lane) {
                                pacers[pos] =
                                    InterfacePacer::from_descriptor(lane, &descriptors[slot]);
                            }
                        }
                        wake_schedules =
                            engine.wake_schedules(AttachedInterfaces::new(&*descriptors));
                    }
                }
            },
            Either6::Sixth(()) => {}
        }
        let now = host.now();
        if persistence
            .deadline(now)
            .is_some_and(|deadline| deadline.0 <= now.0)
        {
            persistence.progress(engine, now).await;
            yield_now().await;
        }
        if Store::RETAINS_COUNTS {
            let mut dirty = engine.take_dirty_interfaces();
            let mut changed = false;
            dirty.drain(|interface| {
                if descriptors
                    .iter()
                    .any(|descriptor| descriptor.id == interface)
                {
                    store.set_interface_counts(interface, engine.interface_counts(interface));
                } else {
                    store.forget_interface(interface);
                }
                changed = true;
            });
            if changed {
                store.signal_interface_counts_changed();
            }
        }
    }
}

#[cfg(feature = "log")]
fn note_inbound(bytes: &[u8], source: crate::interfaces::InterfaceId) {
    let header = crate::wire::WirePacketHeader::parse(bytes)
        .ok()
        .map(|(header, _)| header);
    let id = source.as_bytes();
    log::info!(
        "rx {}b {:?} {:?} {:02x}{:02x}{:02x}{:02x}",
        bytes.len(),
        header.map(|header| header.packet_type),
        header.map(|header| header.context),
        id[0],
        id[1],
        id[2],
        id[3]
    );
}

#[cfg(feature = "log")]
fn note_journaled(journaled: &Journaled<'_>) {
    match journaled {
        Journaled::LinkEstablished(_) => log::info!("journal link-up"),
        Journaled::RequestReceived { data, .. } => log::info!(
            "journal request kind={:?} bytes={}",
            data.get(1).copied(),
            data.len()
        ),
        Journaled::LinkClosed { reason, .. } => log::info!("journal link-close {reason:?}"),
        Journaled::Delivered(_) => log::info!("journal delivered"),
        _ => {}
    }
}

async fn wait_for_persistence(host: &impl Host, deadline: Option<crate::engine::InstantMillis>) {
    match deadline {
        Some(deadline) => host.sleep_until(deadline).await,
        None => core::future::pending().await,
    }
}

async fn wait_for_persistence_or_work<S: StorageLayout>(
    host: &impl Host,
    deadline: Option<crate::engine::InstantMillis>,
    persistence: &impl ManifoldPersistence<S>,
) {
    let _completed = select(
        wait_for_persistence(host, deadline),
        persistence.wait_for_work(),
    )
    .await;
}

#[cfg(test)]
mod tests;
