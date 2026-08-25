mod board;
mod connectivity;

use esp_backtrace as _;
use esp_bootloader_esp_idf::esp_app_desc;
use esp_hal::peripherals::BT;
use esp_hal::rng::Rng;

use embassy_executor::Spawner;
use embassy_net::{Runner, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use static_cell::StaticCell;

use personal_rns::bluetooth_auto::BluetoothAutoShared;
use personal_rns::engine::IssuedCommand;
use personal_rns::interfaces::bluetooth_auto::{BleIdentity, BLE_HW_MTU};
use personal_rns::interfaces::wifi_auto as wifi_auto_contract;
use personal_rns::interfaces::{InterfaceId, InterfaceKind};
use personal_rns::manifold::embassy::{EmbassyHost, InterfaceLifecycle};
use personal_rns::runtime::{
    minimum_interface_store_capacity, minimum_manifold_notification_capacity, CompletionPool,
    EmbassyInterfaceStore, Fleet, ManifoldLaneSet, PrnsEvent, PrnsNode, PrnsNodeHandle,
    PrnsNodeRecipe, StaticManifoldLane,
};
use personal_rns::wifi_auto::AutoWifiShared;
use prns_interfaces_embassy::bluetooth_auto::PEER_CAPACITY as EMBEDDED_BLE_PEER_CAPACITY;

use crate::storage::{C6Storage, EngineStorageType};

use esp_radio::wifi::sta::StationConfig;
use esp_radio::wifi::{Config as WifiConfig, Interface as WifiStaDevice, WifiController};

esp_app_desc!();

use board::{C6Hardware, XiaoEsp32C6, ANNOUNCE_APP_DATA, NODE_ANNOUNCE_APP_DATA};

const WIFI_SSID: &str = match option_env!("HOPSPOT_WIFI_SSID") {
    Some(ssid) => ssid,
    None => "",
};
const WIFI_PASSWORD: &str = match option_env!("HOPSPOT_WIFI_PASSWORD") {
    Some(password) => password,
    None => "",
};

const BLE_LANE: usize = 1;
const WIFI_LANE: usize = 1;
const LANE_COUNT: usize = BLE_LANE + WIFI_LANE;
const LANE_DEPTH: usize = 1;
const OUTBOUND_BURST_DEPTH: usize = C6Storage::MAX_OUTGOING_RESOURCE_REACTION_FRAMES;
/// AutoWifi peer slots. No PSRAM — keep modest, but room for several LAN hosts + phones.
pub const WIFI_MEMBERS: usize = 10;
pub const BLE_PEER_CAPACITY: usize = EMBEDDED_BLE_PEER_CAPACITY;
const INTERFACE_CAPACITY: usize = LANE_COUNT + WIFI_MEMBERS + BLE_PEER_CAPACITY;
pub const NOTIFY_CAP: usize = minimum_manifold_notification_capacity(LANE_COUNT, LANE_DEPTH);
const COMMANDS_CAP: usize = 8;
pub const LIFECYCLE_CAP: usize = 32;
const COMPLETIONS_CAP: usize = 4;
const INTERFACE_STORE_CAP: usize = minimum_interface_store_capacity(INTERFACE_CAPACITY);
const PACKET_PHY_RETENTION_CAPACITY: usize = 32;
const PACKET_PHY_INDEX_BUCKETS: usize =
    personal_rns::routing::dedup::dedup_index_buckets(PACKET_PHY_RETENTION_CAPACITY);
const _: () = assert!(C6Storage::LINK_SESSIONS > WIFI_MEMBERS + BLE_PEER_CAPACITY);

/// Fallback if DHCP never arrives; prefer waiting on [`WIFI_LINK_UP`].
const BLE_START_FALLBACK: Duration = Duration::from_secs(15);
fn c6_ble_config() -> esp_radio::ble::Config {
    esp_radio::ble::Config::default()
        .with_task_priority(0)
        .with_task_stack_size(4096)
        .with_max_connections(BLE_PEER_CAPACITY as u16)
        .with_default_tx_power(esp_radio::ble::TxPower::P20)
}

const BLE_SUPERVISOR_ID: InterfaceId =
    InterfaceId::new([InterfaceKind::BluetoothAuto as u8, 0, 0, 0, 0, 0, 0, 0]);
const WIFI_SUPERVISOR_ID: InterfaceId =
    InterfaceId::new([InterfaceKind::AutoWifi as u8, 0, 0, 0, 0, 0, 0, 0]);

type Mtx = CriticalSectionRawMutex;
type InterfaceStore = EmbassyInterfaceStore<
    Mtx,
    INTERFACE_STORE_CAP,
    PACKET_PHY_RETENTION_CAPACITY,
    PACKET_PHY_INDEX_BUCKETS,
>;
type C6BleFleet = Fleet<Mtx, BLE_HW_MTU, NOTIFY_CAP, LIFECYCLE_CAP>;
type C6WifiFleet = Fleet<Mtx, { wifi_auto_contract::HARDWARE_MTU }, NOTIFY_CAP, LIFECYCLE_CAP>;
type Node = PrnsNode<
    (),
    personal_hopspot_core::node_pages::NodePageRoutes,
    for<'a> fn(PrnsEvent<'a>, &()),
    EngineStorageType,
    EmbassyHost<fn(&mut [u8])>,
    Mtx,
    LANE_COUNT,
    INTERFACE_CAPACITY,
    NOTIFY_CAP,
    COMMANDS_CAP,
    LIFECYCLE_CAP,
    COMPLETIONS_CAP,
>;
type ManifoldLanes = ManifoldLaneSet<Mtx, LANE_COUNT, NOTIFY_CAP>;

static NOTIFY: Channel<Mtx, InterfaceId, NOTIFY_CAP> = Channel::new();
static COMMANDS: Channel<Mtx, IssuedCommand, COMMANDS_CAP> = Channel::new();
static LIFECYCLE: Channel<Mtx, InterfaceLifecycle, LIFECYCLE_CAP> = Channel::new();
static COMPLETION: CompletionPool<Mtx, COMPLETIONS_CAP> = CompletionPool::new();
static INTERFACE_STORE: InterfaceStore = EmbassyInterfaceStore::new();
static BLE_MANIFOLD_LANE: StaticManifoldLane<Mtx, BLE_HW_MTU, LANE_DEPTH, OUTBOUND_BURST_DEPTH> =
    StaticManifoldLane::new();
static WIFI_MANIFOLD_LANE: StaticManifoldLane<
    Mtx,
    { wifi_auto_contract::HARDWARE_MTU },
    LANE_DEPTH,
    2,
> = StaticManifoldLane::new();
static BLE_SHARED: BluetoothAutoShared<BLE_PEER_CAPACITY> =
    BluetoothAutoShared::new(BLE_SUPERVISOR_ID);
static BLE_OUTBOUND_WAKE: Signal<Mtx, ()> = Signal::new();
static WIFI_SHARED: AutoWifiShared<WIFI_MEMBERS> = AutoWifiShared::new(WIFI_SUPERVISOR_ID);
static WIFI_OUTBOUND_WAKE: Signal<Mtx, ()> = Signal::new();
/// Fired once when the station gets an IPv4 address so BLE can start after Wi-Fi settle.
static WIFI_LINK_UP: Signal<Mtx, ()> = Signal::new();

#[embassy_executor::task]
async fn manifold_task(
    node: &'static mut Node,
    persistence: &'static mut crate::persistence::C6Persistence,
) {
    let _ = node.restore_embedded_persistence(persistence).await;
    node.run_manifold_with_persistence_and_interface_store(&INTERFACE_STORE, persistence)
        .await
}

fn hardware_entropy(bytes: &mut [u8]) {
    Rng::new().read(bytes);
}

fn ignore_events(_event: PrnsEvent<'_>, _state: &()) {}

#[embassy_executor::task]
async fn ble_task(
    spawner: Spawner,
    bt: BT<'static>,
    mac: [u8; 6],
    identity: BleIdentity,
    fleet: C6BleFleet,
    shared: &'static BluetoothAutoShared<BLE_PEER_CAPACITY>,
) {
    // Let station associate + DHCP before BLE/coex contention; fall back if SSID is missing.
    match embassy_futures::select::select(WIFI_LINK_UP.wait(), Timer::after(BLE_START_FALLBACK))
        .await
    {
        embassy_futures::select::Either::First(()) => {
            log::info!("ble: starting after wifi link-up");
        }
        embassy_futures::select::Either::Second(()) => {
            log::warn!("ble: starting after wifi wait timeout");
        }
    }
    // Let STA/DHCP and coex settle before the controller comes up.
    Timer::after(Duration::from_secs(2)).await;
    let connector =
        esp_radio::ble::controller::BleConnector::new(bt, c6_ble_config()).expect("ble connector");
    crate::bluetooth_auto::run(connector, mac, identity, fleet, shared, spawner).await;
}

mod firmware;
pub use firmware::run;
