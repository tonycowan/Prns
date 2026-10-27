use crate::engine::EngineState;
use crate::identity::IdentityHash;
use crate::interfaces::{AttachedInterfaces, InterfaceId};
use crate::routing::links::resources::ResourceHash;
use crate::routing::links::LinkId;
use crate::routing::routes::{NextHop, RouteEntry, RouteRetention};
use crate::routing::warmth::WarmestOf;
use crate::storage::StorageLayout;
use crate::units::{InstantMillis, RttMillis};
use crate::wire::DestinationHash;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceRateState {
    pub destination: DestinationHash,
    pub last_allowed_announce_at: InstantMillis,
    pub blocked_until: InstantMillis,
    pub rate_violations: u16,
}

/// One segment of a resource that is still crossing a link.
///
/// `whole_bytes` is the full transfer, the same figure on every segment.
/// `parts_done` counts only this segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceSegmentProgress {
    pub link_id: LinkId,
    pub incoming: bool,
    pub mark: ResourceHash,
    pub segment_index: u64,
    pub total_segments: u64,
    pub parts_done: u64,
    pub part_count: u64,
    pub whole_bytes: u64,
    /// The interface this link is attached to. Zero when the link is already gone.
    pub interface: InterfaceId,
    pub rtt_millis: u64,
    pub mtu: u32,
    /// Bytes of one resource part on this link.
    pub sdu: u32,
    /// The receiver's part window. On an outgoing row this is the last window the peer asked for.
    pub window: u32,
    /// The receiver's window ceiling. Zero on an outgoing row.
    pub ceiling: u32,
}

/// One request round, drained by the host while a transfer is moving.
///
/// Times are milliseconds inside the round, not a wall clock. `span_ms` is from the request
/// to the event. `first_ms` is from the request to the first part, and only a completed round sets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceRoundNote {
    pub kind: ResourceRoundKind,
    pub incoming: bool,
    pub segment_index: u32,
    pub total_segments: u32,
    pub part_at: u32,
    pub asked: u16,
    pub moved: u16,
    pub window: u16,
    pub ceiling: u16,
    pub fast_rounds: u8,
    pub hashmap_height: u32,
    /// A pull sets this when it has run off the names it has. An admit sets this when the ceiling
    /// was kept from the previous segment of the same transfer. A proof uses [`Self::PROOF_OUT`]
    /// and the `PROOF_IN_*` values.
    pub flag: u8,
    pub first_ms: u32,
    pub span_ms: u32,
}

impl ResourceRoundNote {
    /// The receiver queued a segment proof toward the sender.
    pub const PROOF_OUT: u8 = 0;
    /// The sender accepted a segment proof.
    pub const PROOF_IN_ACCEPTED: u8 = 1;
    /// A proof reached the sender and did not match.
    pub const PROOF_IN_INVALID: u8 = 2;
    /// A proof packet reached the sender and could not be read.
    pub const PROOF_IN_MALFORMED: u8 = 3;
    /// A proof named a transfer the sender is not serving.
    pub const PROOF_IN_MISSING: u8 = 4;
    /// A proof named a segment that is not on the wire yet.
    pub const PROOF_IN_OFF_WIRE: u8 = 5;

    pub const fn empty() -> Self {
        Self {
            kind: ResourceRoundKind::Pull,
            incoming: false,
            segment_index: 0,
            total_segments: 0,
            part_at: 0,
            asked: 0,
            moved: 0,
            window: 0,
            ceiling: 0,
            fast_rounds: 0,
            hashmap_height: 0,
            flag: 0,
            first_ms: 0,
            span_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResourceRoundKind {
    Pull = 1,
    Round = 2,
    Watchdog = 3,
    Serve = 4,
    Admit = 5,
    Proof = 6,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSnapshot {
    pub destination: DestinationHash,
    pub hops: u8,
    pub via: NextHop,
    pub learned_at: InstantMillis,
    pub last_route_activity_at: InstantMillis,
    pub expires_at: InstantMillis,
    pub interface: InterfaceId,
    pub retention: RouteRetention,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedRouteSnapshots<const N: usize> {
    entries: heapless::Vec<RouteSnapshot, N>,
    truncated: bool,
}

impl<const N: usize> BoundedRouteSnapshots<N> {
    pub const fn new() -> Self {
        Self {
            entries: heapless::Vec::new(),
            truncated: false,
        }
    }

    pub fn entries(&self) -> &[RouteSnapshot] {
        &self.entries
    }

    pub const fn is_truncated(&self) -> bool {
        self.truncated
    }

    fn push(&mut self, entry: RouteSnapshot) {
        if self.entries.push(entry).is_err() {
            self.truncated = true;
        }
    }
}

impl<const N: usize> Default for BoundedRouteSnapshots<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveLinkSnapshot {
    pub link_id: LinkId,
    pub rtt: RttMillis,
    pub remote_identity: Option<IdentityHash>,
}

fn route_snapshot(
    destination: DestinationHash,
    entry: RouteEntry,
    expires_at: InstantMillis,
) -> RouteSnapshot {
    RouteSnapshot {
        destination,
        hops: entry.hops,
        via: entry.next_hop,
        learned_at: entry.learned_at,
        last_route_activity_at: entry.last_route_activity_at,
        expires_at,
        interface: entry.receiving_interface,
        retention: entry.retention,
    }
}

impl<S: StorageLayout> EngineState<S> {
    #[must_use]
    pub fn link_count(&self) -> u32 {
        u32::try_from(self.links.active_link_count()).unwrap_or(u32::MAX)
    }

    pub fn visit_announce_rate_states(&self, mut visit: impl FnMut(AnnounceRateState)) {
        for (destination, entry) in self.destination_announce_limits.entries() {
            visit(AnnounceRateState {
                destination,
                last_allowed_announce_at: entry.last_allowed_announce_at,
                blocked_until: entry.blocked_until,
                rate_violations: entry.rate_violations,
            });
        }
    }

    pub fn visit_active_link_snapshots(&self, mut visit: impl FnMut(ActiveLinkSnapshot)) {
        self.links
            .visit_active_links(|link_id, rtt, remote_identity| {
                visit(ActiveLinkSnapshot {
                    link_id,
                    rtt,
                    remote_identity,
                });
            });
    }

    pub fn visit_route_snapshots(
        &self,
        interfaces: AttachedInterfaces<'_>,
        mut visit: impl FnMut(RouteSnapshot),
    ) {
        let warmth = WarmestOf(&self.tunnels, &self.departed_interfaces);
        for (destination, entry, expires_at) in self
            .routing_table
            .path_rows_with_expiry(interfaces, &warmth)
        {
            visit(route_snapshot(destination, entry, expires_at));
        }
    }

    pub fn bounded_route_snapshots<const N: usize>(
        &self,
        interfaces: AttachedInterfaces<'_>,
        mut include: impl FnMut(&RouteSnapshot) -> bool,
    ) -> BoundedRouteSnapshots<N> {
        let mut snapshots = BoundedRouteSnapshots::new();
        self.visit_route_snapshots(interfaces, |snapshot| {
            if include(&snapshot) {
                snapshots.push(snapshot);
            }
        });
        snapshots
    }

    #[must_use]
    pub fn route_snapshot(
        &self,
        destination: DestinationHash,
        interfaces: AttachedInterfaces<'_>,
    ) -> Option<RouteSnapshot> {
        let warmth = WarmestOf(&self.tunnels, &self.departed_interfaces);
        self.routing_table
            .path_row_with_expiry(&destination, interfaces, &warmth)
            .map(|(entry, expires_at)| route_snapshot(destination, entry, expires_at))
    }

    /// The ratchet on the announce that installed this path. Absent when the path is unknown or the announce carried none.
    #[must_use]
    pub fn announced_ratchet(&self, destination: &DestinationHash) -> Option<[u8; 32]> {
        self.routing_table
            .stored_announce_for(destination)?
            .announce
            .ratchet
            .map(|ratchet| *ratchet.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_support::TestStorageLayout;
    use crate::interfaces::AnnounceRateLimit;

    #[test]
    fn announce_rate_introspection_projects_engine_state() {
        let mut engine = EngineState::<TestStorageLayout>::default();
        let destination = DestinationHash::new([0x42; 16]);
        let limit = AnnounceRateLimit {
            target_ms: 100,
            grace: 3,
            penalty_ms: 1_000,
        };
        engine
            .destination_announce_limits
            .observe(destination, InstantMillis(10), limit);
        engine
            .destination_announce_limits
            .observe(destination, InstantMillis(20), limit);
        let mut inspected = None;

        engine.visit_announce_rate_states(|state| inspected = Some(state));

        assert_eq!(
            inspected,
            Some(AnnounceRateState {
                destination,
                last_allowed_announce_at: InstantMillis(20),
                blocked_until: InstantMillis(0),
                rate_violations: 1,
            })
        );
    }
}
