#[cfg(feature = "runtime-metrics")]
use crate::engine::AnnounceOrigin;
use crate::engine::{Directive, EngineReaction, FanTarget};
use crate::interfaces::{AttachedInterfaces, InterfaceKind, InterfaceMode};

pub(in crate::engine) fn fan_frame(
    interfaces: AttachedInterfaces<'_>,
    fanout: FanTarget,
    bytes: &[u8],
    sink: &mut impl FnMut(EngineReaction<'_>),
) {
    fan(
        interfaces,
        fanout,
        bytes,
        EmissionKind::Frame,
        InterfaceWalk::Attachment,
        sink,
    );
}

/// A path request answers on the first interface that delivers it, so the heavier
/// interfaces are handed the packet first. Equal weights keep attachment order.
pub(in crate::engine) fn fan_frame_heaviest_first(
    interfaces: AttachedInterfaces<'_>,
    fanout: FanTarget,
    bytes: &[u8],
    sink: &mut impl FnMut(EngineReaction<'_>),
) {
    fan(
        interfaces,
        fanout,
        bytes,
        EmissionKind::Frame,
        InterfaceWalk::HeaviestFirst,
        sink,
    );
}

pub(in crate::engine) fn fan_announce(
    interfaces: AttachedInterfaces<'_>,
    fanout: FanTarget,
    bytes: &[u8],
    sink: &mut impl FnMut(EngineReaction<'_>),
) {
    fan(
        interfaces,
        fanout,
        bytes,
        EmissionKind::Announce,
        InterfaceWalk::Attachment,
        sink,
    );
}

#[derive(Clone, Copy)]
pub(in crate::engine) enum InterfaceWalk {
    Attachment,
    HeaviestFirst,
}

/// Visits every attached interface. `HeaviestFirst` is a stable sort on gravity:
/// a larger weight goes out first, and equal weights stay in attachment order.
pub(in crate::engine) fn walk_interfaces<'a>(
    interfaces: AttachedInterfaces<'a>,
    order: InterfaceWalk,
    mut visit: impl FnMut(&'a crate::interfaces::InterfaceDescriptor),
) {
    let descriptors = interfaces.as_slice();
    match order {
        InterfaceWalk::Attachment => {
            for descriptor in descriptors {
                visit(descriptor);
            }
        }
        InterfaceWalk::HeaviestFirst => {
            let mut started = false;
            let mut last_gravity = crate::interfaces::InterfaceGravity::ZERO;
            let mut last_index = 0usize;
            loop {
                let mut best: Option<(usize, crate::interfaces::InterfaceGravity)> = None;
                for (index, descriptor) in descriptors.iter().enumerate() {
                    if started
                        && (descriptor.gravity > last_gravity
                            || (descriptor.gravity == last_gravity && index <= last_index))
                    {
                        continue;
                    }
                    let better = best.is_none_or(|(best_index, best_gravity)| {
                        descriptor.gravity > best_gravity
                            || (descriptor.gravity == best_gravity && index < best_index)
                    });
                    if better {
                        best = Some((index, descriptor.gravity));
                    }
                }
                let Some((index, gravity)) = best else {
                    break;
                };
                started = true;
                last_gravity = gravity;
                last_index = index;
                visit(&descriptors[index]);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum EmissionKind {
    Frame,
    Announce,
}

fn fan(
    interfaces: AttachedInterfaces<'_>,
    fanout: FanTarget,
    bytes: &[u8],
    emission: EmissionKind,
    order: InterfaceWalk,
    sink: &mut impl FnMut(EngineReaction<'_>),
) {
    let mut fleets_emitted: u128 = 0;
    walk_interfaces(interfaces, order, |descriptor| {
        if !descriptor.capabilities.allows_transmit() {
            return;
        }
        let targeted = match fanout {
            FanTarget::All => true,
            FanTarget::Only(id) => descriptor.id == id,
            FanTarget::AllExcept(id) => descriptor.id != id,
        };
        if !targeted {
            return;
        }
        match descriptor.id.kind().and_then(InterfaceKind::fanout_kind) {
            Some(supervisor) => {
                debug_assert!(
                    (supervisor as u8) < 128,
                    "InterfaceKind discriminants must stay below 128 to index the fleet seen-bitmask",
                );
                let bit = 1u128 << (supervisor as u8);
                if fleets_emitted & bit == 0 {
                    fleets_emitted |= bit;
                    match emission {
                        EmissionKind::Frame => {
                            sink(EngineReaction::Directive(Directive::SendToFleet {
                                supervisor,
                                fan: fanout,
                                bytes,
                            }));
                        }
                        EmissionKind::Announce => {
                            sink(EngineReaction::Directive(Directive::SendAnnounceToFleet {
                                supervisor,
                                fan: fanout,
                                bytes,
                                hops: 0,
                                #[cfg(feature = "runtime-metrics")]
                                origin: AnnounceOrigin::Local,
                            }))
                        }
                    }
                }
            }
            None => {
                if matches!(emission, EmissionKind::Announce)
                    && descriptor.mode == InterfaceMode::AccessPoint
                {
                    return;
                }
                match emission {
                    EmissionKind::Frame => sink(EngineReaction::Directive(Directive::Send {
                        target: descriptor.id,
                        bytes,
                    })),
                    EmissionKind::Announce => {
                        sink(EngineReaction::Directive(Directive::SendAnnounce {
                            target: descriptor.id,
                            bytes,
                            hops: 0,
                            #[cfg(feature = "runtime-metrics")]
                            origin: AnnounceOrigin::Local,
                        }))
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::test_support::routable_descriptor;
    use crate::interfaces::{InterfaceDescriptor, InterfaceId};

    fn iface(byte: u8) -> InterfaceId {
        InterfaceId::new([byte; 8])
    }

    #[test]
    fn a_shared_channel_announces_before_any_peer_exists() {
        let radio = InterfaceId::from_channel_tag(InterfaceKind::WifiHaLowBroadcast, b"radio");
        let interfaces = [routable_descriptor(radio)];
        let mut count = 0;
        fan_announce(
            AttachedInterfaces::new(&interfaces),
            FanTarget::All,
            b"announce",
            &mut |reaction| {
                let EngineReaction::Directive(Directive::SendAnnounceToFleet {
                    supervisor,
                    fan,
                    ..
                }) = reaction
                else {
                    panic!("the shared channel must retain fleet fan-out intent");
                };
                assert_eq!(supervisor, InterfaceKind::WifiHaLow);
                assert_eq!(fan, FanTarget::All);
                count += 1;
            },
        );
        assert_eq!(count, 1);
    }

    #[test]
    fn local_announces_use_every_mode_except_access_point() {
        let interfaces = [
            InterfaceDescriptor {
                mode: InterfaceMode::Full,
                ..routable_descriptor(iface(0xE1))
            },
            InterfaceDescriptor {
                mode: InterfaceMode::PointToPoint,
                ..routable_descriptor(iface(0xE2))
            },
            InterfaceDescriptor {
                mode: InterfaceMode::AccessPoint,
                ..routable_descriptor(iface(0xE3))
            },
            InterfaceDescriptor {
                mode: InterfaceMode::Roaming,
                ..routable_descriptor(iface(0xE4))
            },
            InterfaceDescriptor {
                mode: InterfaceMode::Boundary,
                ..routable_descriptor(iface(0xE5))
            },
            InterfaceDescriptor {
                mode: InterfaceMode::Gateway,
                ..routable_descriptor(iface(0xE6))
            },
            InterfaceDescriptor {
                mode: InterfaceMode::Internal,
                ..routable_descriptor(iface(0xE7))
            },
        ];

        let mut targets = std::vec::Vec::new();
        fan_announce(
            AttachedInterfaces::new(&interfaces),
            FanTarget::All,
            &[0xAB],
            &mut |reaction| {
                if let EngineReaction::Directive(Directive::SendAnnounce {
                    target, hops: 0, ..
                }) = reaction
                {
                    targets.push(target);
                }
            },
        );

        assert_eq!(
            targets,
            std::vec![
                iface(0xE1),
                iface(0xE2),
                iface(0xE4),
                iface(0xE5),
                iface(0xE6),
                iface(0xE7),
            ],
        );
    }

    #[test]
    fn local_announces_use_one_announce_directive_for_a_supervised_fleet() {
        let first = InterfaceId::new([InterfaceKind::BluetoothPeer as u8, 0x41, 0, 0, 0, 0, 0, 0]);
        let second = InterfaceId::new([InterfaceKind::BluetoothPeer as u8, 0x42, 0, 0, 0, 0, 0, 0]);
        let interfaces = [routable_descriptor(first), routable_descriptor(second)];
        let mut fleets = std::vec::Vec::new();

        fan_announce(
            AttachedInterfaces::new(&interfaces),
            FanTarget::All,
            &[0xAB],
            &mut |reaction| {
                if let EngineReaction::Directive(Directive::SendAnnounceToFleet {
                    supervisor,
                    fan,
                    hops: 0,
                    ..
                }) = reaction
                {
                    fleets.push((supervisor, fan));
                }
            },
        );

        assert_eq!(
            fleets,
            std::vec![(InterfaceKind::BluetoothAuto, FanTarget::All)]
        );
    }

    #[test]
    fn frames_emit_once_for_a_supervised_interface_fleet() {
        let first = InterfaceId::new([InterfaceKind::BluetoothPeer as u8, 0x41, 0, 0, 0, 0, 0, 0]);
        let second = InterfaceId::new([InterfaceKind::BluetoothPeer as u8, 0x42, 0, 0, 0, 0, 0, 0]);
        let interfaces = [routable_descriptor(first), routable_descriptor(second)];
        let mut fleets = std::vec::Vec::new();

        fan_frame(
            AttachedInterfaces::new(&interfaces),
            FanTarget::All,
            &[0xAB],
            &mut |reaction| {
                if let EngineReaction::Directive(Directive::SendToFleet { supervisor, .. }) =
                    reaction
                {
                    fleets.push(supervisor);
                }
            },
        );

        assert_eq!(fleets, std::vec![InterfaceKind::BluetoothAuto]);
    }

    #[test]
    fn a_path_request_leaves_on_the_heaviest_interface_first() {
        use crate::interfaces::InterfaceGravity;

        let ble = InterfaceId::new([InterfaceKind::BluetoothPeer as u8, 0x01, 0, 0, 0, 0, 0, 0]);
        let tcp_first = InterfaceId::new([InterfaceKind::TcpClient as u8, 0x02, 0, 0, 0, 0, 0, 0]);
        let tcp_second = InterfaceId::new([InterfaceKind::TcpClient as u8, 0x03, 0, 0, 0, 0, 0, 0]);
        let wifi = InterfaceId::new([InterfaceKind::WifiPeer as u8, 0x04, 0, 0, 0, 0, 0, 0]);
        let interfaces = [
            InterfaceDescriptor {
                gravity: InterfaceGravity::new(700_000),
                ..routable_descriptor(ble)
            },
            InterfaceDescriptor {
                gravity: InterfaceGravity::new(500_000_000),
                ..routable_descriptor(tcp_first)
            },
            InterfaceDescriptor {
                gravity: InterfaceGravity::new(500_000_000),
                ..routable_descriptor(tcp_second)
            },
            InterfaceDescriptor {
                gravity: InterfaceGravity::new(1_000_000_000),
                ..routable_descriptor(wifi)
            },
        ];
        let mut sent = std::vec::Vec::new();
        fan_frame_heaviest_first(
            AttachedInterfaces::new(&interfaces),
            FanTarget::All,
            &[0xAB],
            &mut |reaction| match reaction {
                EngineReaction::Directive(Directive::SendToFleet { supervisor, .. }) => {
                    sent.push(format!("fleet {supervisor:?}"));
                }
                EngineReaction::Directive(Directive::Send { target, .. }) => {
                    sent.push(format!("send {target:?}"));
                }
                _ => {}
            },
        );
        assert_eq!(
            sent,
            std::vec![
                format!("fleet {:?}", InterfaceKind::AutoWifi),
                format!("send {tcp_first:?}"),
                format!("send {tcp_second:?}"),
                format!("fleet {:?}", InterfaceKind::BluetoothAuto),
            ]
        );
    }
}
