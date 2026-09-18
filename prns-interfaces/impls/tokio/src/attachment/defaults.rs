#[cfg(any(feature = "wifi-auto", feature = "usb", feature = "bluetooth-auto"))]
use prns_runtime::runtime::{AttachIntent, PrnsNodeHandle};

#[cfg(any(feature = "wifi-auto", feature = "usb", feature = "bluetooth-auto"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "bluetooth-auto"), derive(Default))]
pub struct DefaultAutoInterfaces {
    #[cfg(feature = "bluetooth-auto")]
    ble_identity: prns_core::interfaces::bluetooth_auto::BleIdentity,
}

#[cfg(feature = "bluetooth-auto")]
impl DefaultAutoInterfaces {
    pub const fn new(ble_identity: prns_core::interfaces::bluetooth_auto::BleIdentity) -> Self {
        Self { ble_identity }
    }
}

#[cfg(any(feature = "wifi-auto", feature = "usb", feature = "bluetooth-auto"))]
impl AttachIntent for DefaultAutoInterfaces {
    fn attach(self, handle: &PrnsNodeHandle) {
        #[cfg(feature = "wifi-auto")]
        {
            let wifi = crate::wifi_auto::AutoWifi::default();
            let status = wifi.status();
            let attached = handle.attach(wifi);
            if let Some(group) = prns_core::remote_control::RemoteControlInterfaceGroup::parse(
                prns_core::interfaces::wifi_auto::GROUP_NAME,
            ) {
                let _ = handle.set_interface_group(attached.id(), group);
            }
            let _ = handle.register_interface_group_apply(attached.id(), move |group| {
                status.set_group_id(group)
            });
        }
        #[cfg(all(
            feature = "usb",
            any(target_os = "linux", target_os = "macos", target_os = "windows")
        ))]
        handle.attach(crate::usb_auto::AutoUsb::default());
        #[cfg(feature = "bluetooth-auto")]
        handle.attach(crate::bluetooth_auto::AutoBle::new(self.ble_identity));
    }
}
