// mdns-sd snapshots on a multicast-broken AP often have an A and no usable
// AAAA. TCP A already becomes a peer; UDP A is still dropped.

#[test]
fn ipv4_only_tcp_record_is_a_peer_without_aaaa() {
    let central_publications = central_publications();
    let android = resolved_service(
        DiscoveryTransport::Tcp,
        "prns-androidv4",
        &["192.168.1.40".parse().unwrap()],
        contract::TCP_RENDEZVOUS_PORT,
        &[(contract::TXT_VERSION_KEY, contract::TXT_VERSION_VALUE)],
    );
    let advertisement = build_service_advertisement(&android, &central_publications, &[])
        .expect("IPv4-only Android publication is a TCP peer");
    assert_eq!(advertisement.endpoints().len(), 1);
    assert_eq!(
        advertisement.endpoints()[0].socket_addr(),
        "192.168.1.40:42699".parse().unwrap()
    );
}

#[test]
fn ipv4_interface_id_fills_missing_link_local_scope_on_aaaa() {
    let central_publications = central_publications();
    let mixed = resolved_service(
        DiscoveryTransport::Udp,
        "prns-mixed",
        &[
            "fe80::12bd:a3ff:fe9d:f90c".parse().unwrap(),
            "192.168.1.62".parse().unwrap(),
        ],
        contract::UNICAST_DISCOVERY_PORT,
        &[(contract::TXT_VERSION_KEY, contract::TXT_VERSION_VALUE)],
    );
    let advertisement = build_service_advertisement(&mixed, &central_publications, &[14])
        .expect("scoped AAAA stays eligible");
    let sockets: Vec<_> = advertisement
        .endpoints()
        .iter()
        .map(|endpoint| endpoint.socket_addr())
        .collect();
    assert!(sockets.contains(&"[fe80::12bd:a3ff:fe9d:f90c%14]:29717".parse().unwrap()));
}

#[test]
#[ignore = "DiscoveryEndpoint::udp still requires IPv6; IPv4-only UDP records are the blocked-multicast contract"]
fn ipv4_only_udp_record_is_a_peer_without_aaaa() {
    let central_publications = central_publications();
    let android = resolved_service(
        DiscoveryTransport::Udp,
        "prns-androidudp",
        &["192.168.1.40".parse().unwrap()],
        contract::UNICAST_DISCOVERY_PORT,
        &[(contract::TXT_VERSION_KEY, contract::TXT_VERSION_VALUE)],
    );
    let advertisement = build_service_advertisement(&android, &central_publications, &[])
        .expect("IPv4-only Android publication is a UDP peer");
    assert_eq!(advertisement.endpoints().len(), 1);
    assert_eq!(
        advertisement.endpoints()[0].socket_addr(),
        "192.168.1.40:29717".parse().unwrap()
    );
}
