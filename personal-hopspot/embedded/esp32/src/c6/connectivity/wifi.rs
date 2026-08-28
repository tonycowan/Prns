use super::super::*;
use super::station::{
    net_task, network_ready_task, wifi_connect_task, wifi_radio_keepalive_task, StationCredentials,
};
use alloc::string::String;
use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{
    Config as NetConfig, ConfigV6, DhcpConfig, Ipv6Cidr, Stack, StackResources, StaticConfigV6,
};
use esp_hal::rng::Rng;
use esp_radio::wifi::ControllerConfig;
use personal_rns::interfaces::wifi_auto as wifi_auto_contract;
use personal_rns::interfaces::MacAddress;
use personal_rns::wifi_auto::{
    AutoWifi, AutoWifiSegment, AutoWifiStatus, AutoWifiTopology, MdnsMulticastFamily,
    UdpServiceDiscovery, UdpServiceDiscoveryStorage, UDP_SERVICE_DISCOVERY_RX_SOCKET_BYTES,
    UDP_SERVICE_DISCOVERY_RX_SOCKET_METADATA, UDP_SERVICE_DISCOVERY_SOCKET_COUNT,
    UDP_SERVICE_DISCOVERY_TX_SOCKET_BYTES, UDP_SERVICE_DISCOVERY_TX_SOCKET_METADATA,
};
use static_cell::StaticCell;

// STA + BLE coex: leaner than the Wi-Fi-only bring-up profile.
const WIFI_STATIC_RX_BUFFERS: u8 = 4;
const WIFI_DYNAMIC_RX_BUFFERS: u16 = 12;
const WIFI_RX_BA_WINDOW: u8 = 3;
const WIFI_RX_QUEUE_FRAMES: usize = 12;
const WIFI_TX_QUEUE_FRAMES: usize = 3;
const WIFI_STATIC_TX_BUFFERS: u8 = 0;
const WIFI_DYNAMIC_TX_BUFFERS: u16 = 6;

const WIFI_DATA_SOCKET_BUFFER_BYTES: usize = 2 * 1_024;
const WIFI_DATA_SOCKET_METADATA: usize = 4;
const WIFI_AUTO_DISCOVERY_SOCKET_METADATA: usize = 4;
const WIFI_AUTO_DISCOVERY_SOCKET_BYTES: usize = 128;
const WIFI_AUTO_UNICAST_DISCOVERY_TX_QUEUED_PACKETS: usize = 2;
const WIFI_AUTO_UNICAST_DISCOVERY_TX_SOCKET_METADATA: usize =
    WIFI_AUTO_UNICAST_DISCOVERY_TX_QUEUED_PACKETS + 1;
const WIFI_AUTO_UNICAST_DISCOVERY_TX_SOCKET_BYTES: usize =
    wifi_auto_contract::PEERING_TOKEN_BYTES * WIFI_AUTO_UNICAST_DISCOVERY_TX_QUEUED_PACKETS;

const DHCP_CLIENT_SOCKET_COUNT: usize = 1;
const WIFI_AUTO_DATAGRAM_SOCKET_COUNT: usize = 3;
const STATION_STACK_SOCKET_CAPACITY: usize = DHCP_CLIENT_SOCKET_COUNT
    + WIFI_AUTO_DATAGRAM_SOCKET_COUNT
    + UDP_SERVICE_DISCOVERY_SOCKET_COUNT as usize;

const _: () = assert!(WIFI_STATIC_RX_BUFFERS >= WIFI_RX_BA_WINDOW);
const _: () = assert!(WIFI_DYNAMIC_RX_BUFFERS as usize >= WIFI_RX_QUEUE_FRAMES);
const _: () = assert!(WIFI_AUTO_DISCOVERY_SOCKET_BYTES >= wifi_auto_contract::PEERING_TOKEN_BYTES);

fn discovery_socket(stack: Stack<'static>) -> UdpSocket<'static> {
    static RX_META: StaticCell<[PacketMetadata; WIFI_AUTO_DISCOVERY_SOCKET_METADATA]> =
        StaticCell::new();
    static RX_BYTES: StaticCell<[u8; WIFI_AUTO_DISCOVERY_SOCKET_BYTES]> = StaticCell::new();
    static TX_META: StaticCell<[PacketMetadata; WIFI_AUTO_DISCOVERY_SOCKET_METADATA]> =
        StaticCell::new();
    static TX_BYTES: StaticCell<[u8; WIFI_AUTO_DISCOVERY_SOCKET_BYTES]> = StaticCell::new();
    UdpSocket::new(
        stack,
        RX_META.init([PacketMetadata::EMPTY; WIFI_AUTO_DISCOVERY_SOCKET_METADATA]),
        RX_BYTES.init([0; WIFI_AUTO_DISCOVERY_SOCKET_BYTES]),
        TX_META.init([PacketMetadata::EMPTY; WIFI_AUTO_DISCOVERY_SOCKET_METADATA]),
        TX_BYTES.init([0; WIFI_AUTO_DISCOVERY_SOCKET_BYTES]),
    )
}

fn unicast_discovery_socket(stack: Stack<'static>) -> UdpSocket<'static> {
    static RX_META: StaticCell<[PacketMetadata; WIFI_AUTO_DISCOVERY_SOCKET_METADATA]> =
        StaticCell::new();
    static RX_BYTES: StaticCell<[u8; WIFI_AUTO_DISCOVERY_SOCKET_BYTES]> = StaticCell::new();
    static TX_META: StaticCell<[PacketMetadata; WIFI_AUTO_UNICAST_DISCOVERY_TX_SOCKET_METADATA]> =
        StaticCell::new();
    static TX_BYTES: StaticCell<[u8; WIFI_AUTO_UNICAST_DISCOVERY_TX_SOCKET_BYTES]> =
        StaticCell::new();
    UdpSocket::new(
        stack,
        RX_META.init([PacketMetadata::EMPTY; WIFI_AUTO_DISCOVERY_SOCKET_METADATA]),
        RX_BYTES.init([0; WIFI_AUTO_DISCOVERY_SOCKET_BYTES]),
        TX_META.init([PacketMetadata::EMPTY; WIFI_AUTO_UNICAST_DISCOVERY_TX_SOCKET_METADATA]),
        TX_BYTES.init([0; WIFI_AUTO_UNICAST_DISCOVERY_TX_SOCKET_BYTES]),
    )
}

fn data_socket(stack: Stack<'static>) -> UdpSocket<'static> {
    static RX_META: StaticCell<[PacketMetadata; WIFI_DATA_SOCKET_METADATA]> = StaticCell::new();
    static RX_BYTES: StaticCell<[u8; WIFI_DATA_SOCKET_BUFFER_BYTES]> = StaticCell::new();
    static TX_META: StaticCell<[PacketMetadata; WIFI_DATA_SOCKET_METADATA]> = StaticCell::new();
    static TX_BYTES: StaticCell<[u8; WIFI_DATA_SOCKET_BUFFER_BYTES]> = StaticCell::new();
    UdpSocket::new(
        stack,
        RX_META.init([PacketMetadata::EMPTY; WIFI_DATA_SOCKET_METADATA]),
        RX_BYTES.init([0; WIFI_DATA_SOCKET_BUFFER_BYTES]),
        TX_META.init([PacketMetadata::EMPTY; WIFI_DATA_SOCKET_METADATA]),
        TX_BYTES.init([0; WIFI_DATA_SOCKET_BUFFER_BYTES]),
    )
}

fn udp_service_discovery_socket(stack: Stack<'static>) -> UdpSocket<'static> {
    static RX_META: StaticCell<[PacketMetadata; UDP_SERVICE_DISCOVERY_RX_SOCKET_METADATA]> =
        StaticCell::new();
    static RX_BYTES: StaticCell<[u8; UDP_SERVICE_DISCOVERY_RX_SOCKET_BYTES]> = StaticCell::new();
    static TX_META: StaticCell<[PacketMetadata; UDP_SERVICE_DISCOVERY_TX_SOCKET_METADATA]> =
        StaticCell::new();
    static TX_BYTES: StaticCell<[u8; UDP_SERVICE_DISCOVERY_TX_SOCKET_BYTES]> = StaticCell::new();
    UdpSocket::new(
        stack,
        RX_META.init([PacketMetadata::EMPTY; UDP_SERVICE_DISCOVERY_RX_SOCKET_METADATA]),
        RX_BYTES.init([0; UDP_SERVICE_DISCOVERY_RX_SOCKET_BYTES]),
        TX_META.init([PacketMetadata::EMPTY; UDP_SERVICE_DISCOVERY_TX_SOCKET_METADATA]),
        TX_BYTES.init([0; UDP_SERVICE_DISCOVERY_TX_SOCKET_BYTES]),
    )
}

pub(in crate::c6) fn build_wifi(
    spawner: &Spawner,
    wifi: esp_hal::peripherals::WIFI<'static>,
    mac: [u8; 6],
) -> Option<AutoWifi<'static, WIFI_MEMBERS>> {
    let wifi_config = ControllerConfig::default()
        .with_static_rx_buf_num(WIFI_STATIC_RX_BUFFERS)
        .with_dynamic_rx_buf_num(WIFI_DYNAMIC_RX_BUFFERS)
        .with_rx_ba_win(WIFI_RX_BA_WINDOW)
        .with_rx_queue_size(WIFI_RX_QUEUE_FRAMES)
        .with_tx_queue_size(WIFI_TX_QUEUE_FRAMES)
        .with_static_tx_buf_num(WIFI_STATIC_TX_BUFFERS)
        .with_dynamic_tx_buf_num(WIFI_DYNAMIC_TX_BUFFERS);
    let Ok((controller, interfaces)) = esp_radio::wifi::new(wifi, wifi_config) else {
        return None;
    };
    log::info!(
        "wifi: c6 rx profile static={} dynamic={} ba={} queue={}",
        WIFI_STATIC_RX_BUFFERS,
        WIFI_DYNAMIC_RX_BUFFERS,
        WIFI_RX_BA_WINDOW,
        WIFI_RX_QUEUE_FRAMES
    );

    let Some(credentials) = station_credentials() else {
        spawner
            .spawn(wifi_radio_keepalive_task(controller).expect("wifi radio keepalive task fits"));
        return None;
    };

    let link_local = wifi_auto_contract::link_local_from_mac(MacAddress::new(mac));
    let mut net_config = NetConfig::dhcpv4(DhcpConfig::default());
    net_config.ipv6 = ConfigV6::Static(StaticConfigV6 {
        address: Ipv6Cidr::new(link_local, 64),
        gateway: None,
        dns_servers: Default::default(),
    });
    static RESOURCES: StaticCell<StackResources<STATION_STACK_SOCKET_CAPACITY>> = StaticCell::new();
    let resources = RESOURCES.init(StackResources::new());
    let seed = {
        let mut bytes = [0u8; 8];
        Rng::new().read(&mut bytes);
        u64::from_le_bytes(bytes)
    };
    let (stack, runner) = embassy_net::new(interfaces.station, net_config, resources, seed);
    let discovery = discovery_socket(stack);
    let unicast_discovery = unicast_discovery_socket(stack);
    let data = data_socket(stack);
    let wifi_status = AutoWifiStatus::new(&WIFI_SHARED);
    start_udp_service_discovery(spawner, stack, link_local, wifi_status);
    spawner.spawn(net_task(runner).expect("net task fits"));
    spawner.spawn(network_ready_task(stack).expect("network readiness task fits"));
    spawner.spawn(wifi_connect_task(controller, credentials).expect("wifi connect task fits"));

    Some(AutoWifi::new(
        AutoWifiTopology {
            primary: AutoWifiSegment {
                stack,
                discovery,
                unicast_discovery,
                data,
                mac,
            },
            secondary: None,
            rendezvous: None,
        },
        &WIFI_SHARED,
    ))
}

fn start_udp_service_discovery(
    spawner: &Spawner,
    stack: Stack<'static>,
    address: core::net::Ipv6Addr,
    status: AutoWifiStatus<WIFI_MEMBERS>,
) {
    let socket = udp_service_discovery_socket(stack);
    static STORAGE: StaticCell<UdpServiceDiscoveryStorage<WIFI_MEMBERS>> = StaticCell::new();
    let storage = STORAGE.init(UdpServiceDiscoveryStorage::new());
    // IPv4 mDNS (224.0.0.251) — same path Android used successfully on APs that block IPv6 LL
    // multicast. Publication carries LL AAAA plus station IPv4 A when DHCP is up.
    let service_discovery = match UdpServiceDiscovery::with_multicast(
        socket,
        stack,
        address,
        status,
        storage,
        super::super::hardware_entropy,
        MdnsMulticastFamily::Ipv4,
    ) {
        Ok(service_discovery) => service_discovery,
        Err(error) => {
            log::error!("wifi-auto: UDP DNS-SD construction failed: {error:?}");
            return;
        }
    };
    match udp_service_discovery_task(service_discovery) {
        Ok(task) => {
            spawner.spawn(task);
            log::info!("wifi-auto: UDP DNS-SD task started (IPv4 mDNS)");
        }
        Err(_) => log::error!("wifi-auto: UDP DNS-SD task capacity exhausted"),
    }
}

#[embassy_executor::task]
async fn udp_service_discovery_task(
    service_discovery: UdpServiceDiscovery<'static, WIFI_MEMBERS>,
) -> ! {
    service_discovery.run().await
}

fn station_credentials() -> Option<StationCredentials> {
    if WIFI_SSID.is_empty() {
        log::warn!("wifi: no HOPSPOT_WIFI_SSID at build time; Wi-Fi Auto station disabled");
        return None;
    }
    log::info!(
        "wifi: station SSID from build env (ssid_len={} password_len={})",
        WIFI_SSID.len(),
        WIFI_PASSWORD.len()
    );
    Some(StationCredentials {
        ssid: String::from(WIFI_SSID),
        password: String::from(WIFI_PASSWORD),
    })
}
