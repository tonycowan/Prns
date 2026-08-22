//! Smoke: attach as LocalClient to whatever is on :37428 and announce once.

use std::time::Duration;

use personal_rns::engine::{AnnounceAppData, AnnounceNow, AnnounceTarget, RatchetPolicy};
use personal_rns::interfaces::shared_instance::configured_policy;
use personal_rns::interfaces::ConfiguredInterfacePolicy;
use personal_rns::request_endpoints;
use personal_rns::routing::{LinkRequestPolicy, ProofStrategy};
use personal_rns::runtime::{
    try_generate_identity_secret, Diagnostic, ManuallyAttached, NoPersistence,
    PreConfiguredDestination, PrnsEvent, PrnsNode, PrnsNodeRecipe, ServeMyRequestEndpoints,
};
use personal_rns::shared_instance::{
    connect_existing_shared_instance, SharedInstanceClientIntent, SharedInstanceTransport,
};
use personal_rns::storage::GrowableHeap;

#[tokio::test]
async fn connect_and_announce_against_local_bus() {
    if std::env::var_os("PERSONAL_TEXT_LIVE_BUS").is_none() {
        eprintln!("skip: set PERSONAL_TEXT_LIVE_BUS=1 with a host on :37428");
        return;
    }
    let destination = PreConfiguredDestination::Single {
        resource_strategy: personal_rns::routing::links::resources::ResourceStrategy::AcceptNone,
        app_name: "lxmf",
        aspects: &["delivery"],
        identity: try_generate_identity_secret().expect("entropy"),
        announce_app_data: b"personal-text-smoke",
        proof: ProofStrategy::ProveAll,
        link_requests: LinkRequestPolicy::AcceptAll,
        ratchet: RatchetPolicy::NoRatchets,
        maximum_request_bytes: Default::default(),
        request_endpoints: ServeMyRequestEndpoints::No,
    };
    let destination_hash = destination.destination_hash().expect("name");

    let (heard_tx, mut heard_rx) = tokio::sync::mpsc::unbounded_channel();
    let node = PrnsNode::new(PrnsNodeRecipe {
        transport_identity: None,
        pre_configured_destinations: [destination],
        app_state: (),
        storage: GrowableHeap,
        request_endpoints: request_endpoints![],
        interfaces: ManuallyAttached,
        persistence: NoPersistence,
        on_event: move |event, _| {
            if let PrnsEvent::Diagnostic(Diagnostic::AnnounceHeard { destination, .. }) = event {
                let _ = heard_tx.send(destination);
            }
        },
    });
    let handle = node.handle();

    let intent = SharedInstanceClientIntent {
        bus_port: 37428,
        transport: SharedInstanceTransport::Tcp,
        policy: configured_policy(ConfiguredInterfacePolicy::default()),
    };

    connect_existing_shared_instance(&handle, intent)
        .await
        .expect("Hopspot/prnsd must be listening on 127.0.0.1:37428 for this smoke test");

    let announcer = handle.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        announcer
            .announce_now(AnnounceNow {
                destination: destination_hash,
                target: AnnounceTarget::AllInterfaces,
                app_data: AnnounceAppData::Registered,
            })
            .await
            .expect("announce_now");
    });

    tokio::select! {
        result = node.run() => {
            result.expect("node");
            panic!("node stopped before announce settled");
        }
        _ = tokio::time::sleep(Duration::from_secs(3)) => {
            // Wire success is host-side; LocalClient announce_now returning Ok is enough here.
            // Drain any heard events without requiring a second peer.
            let _ = heard_rx.try_recv();
        }
    }
}
