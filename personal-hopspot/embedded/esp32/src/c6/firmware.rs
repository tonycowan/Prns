use super::*;
use connectivity::build_wifi;
use esp_hal::gpio::Output;

pub async fn run(spawner: Spawner) {
    let C6Hardware {
        wifi,
        bluetooth,
        identity_entropy,
        mac,
        timebase,
        _rtc,
        _rf_switch_power,
        _rf_antenna_select,
        user_led,
    } = XiaoEsp32C6::bringup();

    let node_bootstrap = crate::identity::bootstrap_node_identity();
    crate::identity::log_persistence("node", node_bootstrap.persistence());
    let ble_bootstrap = crate::identity::bootstrap_ble_identity();
    crate::identity::log_persistence("Bluetooth", ble_bootstrap.persistence());
    drop(identity_entropy);

    let auto_wifi = build_wifi(&spawner, wifi, mac);

    let node_identity = node_bootstrap.into_identity();
    let transport_secret = node_identity.transport_secret();
    let destination_secret = node_identity.into_destination_secret();
    let destinations = personal_hopspot_core::HopspotDestinationSet::new(
        destination_secret,
        ANNOUNCE_APP_DATA,
        NODE_ANNOUNCE_APP_DATA,
    );
    let ble_identity = ble_bootstrap.into_identity();

    let mut manifold_lanes = ManifoldLanes::new();
    let ble_supervisor_lane = manifold_lanes
        .claim_supervisor(&BLE_MANIFOLD_LANE, BLE_SUPERVISOR_ID, &BLE_OUTBOUND_WAKE)
        .expect("Bluetooth supervisor lane is available");
    let wifi_supervisor_lane = auto_wifi.as_ref().map(|_| {
        manifold_lanes
            .claim_supervisor(&WIFI_MANIFOLD_LANE, WIFI_SUPERVISOR_ID, &WIFI_OUTBOUND_WAKE)
            .expect("Wi-Fi Auto supervisor lane is available")
    });

    let handle = PrnsNodeHandle::new(COMMANDS.sender(), &COMPLETION);
    let manifold_wiring = manifold_lanes.into_manifold_wiring(
        NOTIFY.receiver(),
        COMMANDS.receiver(),
        LIFECYCLE.receiver(),
        handle,
    );

    let ble_fleet: C6BleFleet = ble_supervisor_lane.into_fleet(NOTIFY.sender(), LIFECYCLE.sender());
    let wifi = auto_wifi
        .zip(wifi_supervisor_lane)
        .map(|(interface, lane)| {
            let fleet: C6WifiFleet = lane.into_fleet(NOTIFY.sender(), LIFECYCLE.sender());
            (interface, fleet)
        });

    let host = EmbassyHost::new_with_timebase(timebase, hardware_entropy as fn(&mut [u8]));
    let recipe = PrnsNodeRecipe {
        transport_identity: Some(transport_secret),
        pre_configured_destinations: destinations.into_preconfigured_destinations(),
        app_state: (),
        storage: C6Storage,
        request_endpoints: personal_hopspot_core::node_pages::NodePageRoutes,
        interfaces: personal_rns::runtime::ManuallyAttached,
        persistence: crate::persistence::c6(),
        on_event: ignore_events as for<'a> fn(PrnsEvent<'a>, &()),
    };

    static NODE: StaticCell<Node> = StaticCell::new();
    let (node, persistence) =
        PrnsNode::init_static_with_persistence(&NODE, recipe, manifold_wiring, host);
    node.set_protocol_policy(personal_hopspot_core::EMBEDDED_HOPSPOT_PROTOCOL_POLICY);
    static PERSISTENCE: StaticCell<crate::persistence::C6Persistence> = StaticCell::new();
    let persistence = PERSISTENCE.init(persistence);
    spawner.spawn(manifold_task(node, persistence).expect("manifold task fits"));
    spawner.spawn(status_task(user_led).expect("status task fits"));
    spawner.spawn(
        ble_task(
            spawner,
            bluetooth,
            mac,
            ble_identity,
            ble_fleet,
            &BLE_SHARED,
        )
        .expect("ble task fits"),
    );
    if let Some((interface, fleet)) = wifi {
        // Primary data path only (no SoftAP secondary): keep the unused secondary buffer tiny.
        let data_buf = alloc::vec![0u8; wifi_auto_contract::HARDWARE_MTU].leak();
        let secondary_data_buf = alloc::vec![0u8; 1].leak();
        let run = alloc::boxed::Box::leak(alloc::boxed::Box::new(interface.run(
            fleet,
            data_buf,
            secondary_data_buf,
        )));
        let run: core::pin::Pin<&'static mut dyn core::future::Future<Output = ()>> =
            // SAFETY: leaked allocation cannot move or be freed.
            unsafe { core::pin::Pin::new_unchecked(run) };
        spawner.spawn(wifi_task(run).expect("Wi-Fi Auto task fits"));
    } else {
        // No station credentials: unblock BLE immediately so isolation still works.
        WIFI_LINK_UP.signal(());
    }
    core::future::pending().await
}

#[embassy_executor::task]
async fn wifi_task(run: core::pin::Pin<&'static mut dyn core::future::Future<Output = ()>>) {
    run.await
}

/// 1 Hz active-low LED pulse (100 ms on) plus heap/stack stats every 10 s.
#[embassy_executor::task]
async fn status_task(mut led: Output<'static>) -> ! {
    const ILLUMINATED_MS: u64 = 100;
    const PERIOD_MS: u64 = 1_000;
    const HEAP_EVERY_TICKS: u32 = 10;
    let mut ticks = 0u32;
    loop {
        led.set_low();
        Timer::after(Duration::from_millis(ILLUMINATED_MS)).await;
        led.set_high();
        Timer::after(Duration::from_millis(PERIOD_MS - ILLUMINATED_MS)).await;
        ticks = ticks.wrapping_add(1);
        if ticks % HEAP_EVERY_TICKS == 0 {
            let heap = esp_alloc::HEAP.stats();
            let (stack_size, stack_free) = main_stack_stats();
            match stack_free {
                Some(free) => log::info!(
                    "ram: heap used={} free={} high={} size={} stack_size={} stack_free={}",
                    heap.current_usage,
                    heap.size.saturating_sub(heap.current_usage),
                    heap.max_usage,
                    heap.size,
                    stack_size,
                    free
                ),
                None => log::info!(
                    "ram: heap used={} free={} high={} size={} stack_size={} stack_free=off_main",
                    heap.current_usage,
                    heap.size.saturating_sub(heap.current_usage),
                    heap.max_usage,
                    heap.size,
                    stack_size
                ),
            }
        }
    }
}

fn main_stack_stats() -> (usize, Option<usize>) {
    unsafe extern "C" {
        static _stack_end_cpu0: u32;
        static _stack_start_cpu0: u32;
    }
    let bottom = core::ptr::addr_of!(_stack_end_cpu0) as usize;
    let top = core::ptr::addr_of!(_stack_start_cpu0) as usize;
    let size = top.saturating_sub(bottom);
    let sp: usize;
    // SAFETY: reading the stack pointer does not touch memory.
    unsafe {
        core::arch::asm!("mv {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    let free = if sp > bottom && sp <= top {
        Some(sp - bottom)
    } else {
        None
    };
    (size, free)
}
