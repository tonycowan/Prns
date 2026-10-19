//! Android Wi-Fi inventory for AutoWifi.
//!
//! The phone publishes its LAN addresses here. The discovery side reads the same
//! inventory and asks the activity to hold a multicast lock while it is listening.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use jni::objects::{JByteArray, JClass, JIntArray, JObjectArray, JString};
use jni::sys::{jboolean, jlong};
use jni::JNIEnv;
use personal_rns::wifi_auto::{
    AutoWifiStatus, HostLanAddress, HostLanInterface, HostLanInventory, HostLanReplaceOutcome,
};

const JNI_TRUE: jboolean = 1;
const JNI_FALSE: jboolean = 0;

struct LanBridge {
    inventory: HostLanInventory,
    wifi_status: Mutex<Option<AutoWifiStatus>>,
    wait: Condvar,
}

impl LanBridge {
    fn new() -> Self {
        Self {
            inventory: HostLanInventory::new(),
            wifi_status: Mutex::new(None),
            wait: Condvar::new(),
        }
    }

    fn should_hold_multicast_lock(&self) -> bool {
        self.wifi_status
            .lock()
            .ok()
            .and_then(|status| status.as_ref().map(AutoWifiStatus::is_enabled))
            .unwrap_or(false)
    }

    fn wait_for_work(&self, timeout: Duration) {
        let Ok(guard) = self.wifi_status.lock() else {
            return;
        };
        let _ = self.wait.wait_timeout(guard, timeout);
    }
}

fn bridge() -> &'static LanBridge {
    static BRIDGE: OnceLock<LanBridge> = OnceLock::new();
    BRIDGE.get_or_init(LanBridge::new)
}

pub fn inventory() -> HostLanInventory {
    bridge().inventory.clone()
}

pub fn remember_status(status: AutoWifiStatus) {
    let lan = bridge();
    if let Ok(mut stored) = lan.wifi_status.lock() {
        *stored = Some(status);
    }
    lan.wait.notify_all();
}

#[no_mangle]
pub extern "system" fn Java_dev_dioxus_main_WifiLan_nativeShouldHoldMulticastLock(
    _env: JNIEnv,
    _class: JClass,
) -> jboolean {
    if bridge().should_hold_multicast_lock() {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[no_mangle]
pub extern "system" fn Java_dev_dioxus_main_WifiLan_nativeWaitForWork(
    _env: JNIEnv,
    _class: JClass,
    timeout_millis: jlong,
) {
    let timeout = u64::try_from(timeout_millis.max(0)).unwrap_or(0);
    bridge().wait_for_work(Duration::from_millis(timeout));
}

#[no_mangle]
pub extern "system" fn Java_dev_dioxus_main_WifiLan_nativeSetInterfaces(
    mut env: JNIEnv,
    _class: JClass,
    names: JObjectArray,
    indexes: JIntArray,
    addresses: JObjectArray,
    prefix_lengths: JObjectArray,
) -> jboolean {
    match parse_interfaces(&mut env, &names, &indexes, &addresses, &prefix_lengths) {
        Ok(interfaces) => match bridge().inventory.replace(interfaces) {
            HostLanReplaceOutcome::Replaced | HostLanReplaceOutcome::Unchanged => JNI_TRUE,
            HostLanReplaceOutcome::Unavailable => JNI_FALSE,
        },
        Err(()) => JNI_FALSE,
    }
}

fn parse_interfaces(
    env: &mut JNIEnv,
    names: &JObjectArray,
    indexes: &JIntArray,
    addresses: &JObjectArray,
    prefix_lengths: &JObjectArray,
) -> Result<Vec<HostLanInterface>, ()> {
    let name_count = array_len(env, names)?;
    let address_groups = array_len(env, addresses)?;
    let prefix_groups = array_len(env, prefix_lengths)?;
    let index_count = env
        .get_array_length(indexes)
        .ok()
        .and_then(|length| usize::try_from(length).ok())
        .ok_or(())?;
    if name_count != index_count || name_count != address_groups || name_count != prefix_groups {
        return Err(());
    }
    let mut index_values = vec![0; index_count];
    env.get_int_array_region(indexes, 0, &mut index_values)
        .map_err(|_| ())?;
    let mut interfaces = Vec::with_capacity(name_count);
    for slot in 0..name_count {
        let name_object = env
            .get_object_array_element(names, slot as i32)
            .map_err(|_| ())?;
        let name = java_string(env, &JString::from(name_object))?;
        let index = u32::try_from(index_values[slot]).map_err(|_| ())?;
        let address_group = JObjectArray::from(
            env.get_object_array_element(addresses, slot as i32)
                .map_err(|_| ())?,
        );
        let prefix_group = JIntArray::from(
            env.get_object_array_element(prefix_lengths, slot as i32)
                .map_err(|_| ())?,
        );
        let parsed = parse_addresses(env, &address_group, &prefix_group)?;
        if let Ok(interface) = HostLanInterface::new(name, index, parsed) {
            interfaces.push(interface);
        }
    }
    Ok(interfaces)
}

fn parse_addresses(
    env: &mut JNIEnv,
    addresses: &JObjectArray,
    prefix_lengths: &JIntArray,
) -> Result<Vec<HostLanAddress>, ()> {
    let address_count = array_len(env, addresses)?;
    let prefix_count = env
        .get_array_length(prefix_lengths)
        .ok()
        .and_then(|length| usize::try_from(length).ok())
        .ok_or(())?;
    if address_count != prefix_count {
        return Err(());
    }
    let mut prefix_values = vec![0; prefix_count];
    env.get_int_array_region(prefix_lengths, 0, &mut prefix_values)
        .map_err(|_| ())?;
    let mut parsed = Vec::with_capacity(address_count);
    for slot in 0..address_count {
        let address_object = env
            .get_object_array_element(addresses, slot as i32)
            .map_err(|_| ())?;
        let octets = env
            .convert_byte_array(JByteArray::from(address_object))
            .map_err(|_| ())?;
        let prefix_len = u8::try_from(prefix_values[slot]).map_err(|_| ())?;
        let Some(addr) = ip_addr(&octets) else {
            continue;
        };
        if let Ok(address) = HostLanAddress::new(addr, prefix_len) {
            parsed.push(address);
        }
    }
    Ok(parsed)
}

fn ip_addr(octets: &[u8]) -> Option<IpAddr> {
    match octets.len() {
        4 => Some(IpAddr::V4(Ipv4Addr::new(
            octets[0], octets[1], octets[2], octets[3],
        ))),
        16 => {
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(octets);
            Some(IpAddr::V6(Ipv6Addr::from(bytes)))
        }
        _ => None,
    }
}

fn array_len(env: &mut JNIEnv, array: &JObjectArray) -> Result<usize, ()> {
    env.get_array_length(array)
        .ok()
        .and_then(|length| usize::try_from(length).ok())
        .ok_or(())
}

fn java_string(env: &mut JNIEnv, value: &JString) -> Result<String, ()> {
    if value.is_null() {
        return Err(());
    }
    env.get_string(value)
        .map_err(|_| ())?
        .to_str()
        .map(str::to_owned)
        .map_err(|_| ())
}
