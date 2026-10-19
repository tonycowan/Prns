//! Android BLE and USB, bridged into the journal the same way the controller does it.

mod jni_ble;
mod jni_usb;
mod usb_bridge;

use std::sync::OnceLock;

use prns_ffi::bluetooth_auto::android::AndroidBleBridge;

use usb_bridge::AndroidUsbBridge;

static BLE: OnceLock<AndroidBleBridge> = OnceLock::new();
static USB: OnceLock<AndroidUsbBridge> = OnceLock::new();

pub(crate) fn ble_bridge() -> AndroidBleBridge {
    BLE.get_or_init(AndroidBleBridge::new).clone()
}

pub(crate) fn usb_bridge() -> AndroidUsbBridge {
    USB.get_or_init(AndroidUsbBridge::new).clone()
}
