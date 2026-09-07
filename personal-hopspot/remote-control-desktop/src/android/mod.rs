mod jni_ble;
mod jni_usb;
mod usb_bridge;

use std::sync::OnceLock;

use prns_ffi::bluetooth_auto::android::AndroidBleBridge;

use usb_bridge::AndroidUsbBridge;

const ANDROID_USB_PORT: &str = "android-usb";

pub struct AndroidPlatform {
    pub ble: AndroidBleBridge,
    pub usb: AndroidUsbBridge,
}

impl AndroidPlatform {
    fn new() -> Self {
        Self {
            ble: AndroidBleBridge::new(),
            usb: AndroidUsbBridge::new(),
        }
    }
}

static PLATFORM: OnceLock<AndroidPlatform> = OnceLock::new();

pub fn platform() -> &'static AndroidPlatform {
    PLATFORM.get_or_init(AndroidPlatform::new)
}

pub fn ble_bridge() -> AndroidBleBridge {
    platform().ble.clone()
}

pub fn usb_bridge() -> AndroidUsbBridge {
    platform().usb.clone()
}

pub fn android_usb_port() -> &'static str {
    ANDROID_USB_PORT
}
