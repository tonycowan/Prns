use std::sync::Arc;

use prns_runtime::interfaces::IfacContext;
use prns_runtime::interfaces::{ConfiguredInterfacePolicy, EffectiveInterfacePolicy, InterfaceId};
use prns_runtime::manifold::driver::TokioInterfaceStatus;
use prns_runtime::runtime::{Attachable, AttachedInterface, PrnsNodeHandle};
use tokio::sync::Notify;

use super::{
    open_native_usb_auto_target, scan_native_usb_auto_targets, UsbAutoCandidate, UsbAutoHost,
};

pub const DEFAULT_USB_AUTO_ID: InterfaceId = InterfaceId::new([0xD0; 8]);
pub const DEFAULT_USB_BAUD: u32 = 115_200;

pub struct AutoUsb {
    baud: u32,
    policy: EffectiveInterfacePolicy,
    rescan: Arc<Notify>,
    on_status: Option<Box<dyn FnOnce(TokioInterfaceStatus) + Send>>,
}

impl Default for AutoUsb {
    fn default() -> Self {
        Self {
            baud: DEFAULT_USB_BAUD,
            policy: prns_runtime::interfaces::usb_auto::HOST_DEFAULTS
                .configured(ConfiguredInterfacePolicy::default()),
            rescan: Arc::new(Notify::new()),
            on_status: None,
        }
    }
}

impl AutoUsb {
    #[must_use]
    pub fn rescan_signal(&self) -> Arc<Notify> {
        self.rescan.clone()
    }

    #[must_use]
    pub fn with_baud(mut self, baud: u32) -> Self {
        self.baud = baud;
        self
    }

    #[must_use]
    pub fn with_policy(mut self, policy: EffectiveInterfacePolicy) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub fn on_status(
        mut self,
        on_status: impl FnOnce(TokioInterfaceStatus) + Send + 'static,
    ) -> Self {
        self.on_status = Some(Box::new(on_status));
        self
    }
}

impl Attachable for AutoUsb {
    type Attached = AttachedInterface;
    fn attach_to(self, handle: &PrnsNodeHandle) -> AttachedInterface {
        let baud = self.baud;
        let host = UsbAutoHost::with_policy(
            DEFAULT_USB_AUTO_ID,
            scan_native_usb_auto_targets,
            move |candidate: UsbAutoCandidate| async move {
                open_native_usb_auto_target(candidate, baud).await
            },
            self.rescan,
            self.policy,
        );
        if let Some(on_status) = self.on_status {
            on_status(host.status());
        }
        handle.add_interface(host)
    }

    fn attach_to_with_ifac(
        self,
        handle: &PrnsNodeHandle,
        ifac: IfacContext,
        network_name: Option<String>,
    ) -> AttachedInterface {
        let baud = self.baud;
        let host = UsbAutoHost::with_policy(
            DEFAULT_USB_AUTO_ID,
            scan_native_usb_auto_targets,
            move |candidate: UsbAutoCandidate| async move {
                open_native_usb_auto_target(candidate, baud).await
            },
            self.rescan,
            self.policy,
        );
        if let Some(on_status) = self.on_status {
            on_status(host.status());
        }
        handle.add_interface_with_ifac_name(host, ifac, network_name)
    }
}
