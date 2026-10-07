#[cfg(feature = "usb")]
use std::sync::Arc;

#[cfg(feature = "usb")]
use crate::usb_auto::AutoUsb;

#[cfg(feature = "usb")]
use super::{AttachmentResult, InterfaceConstruction, PlanRuntimeContext};

#[cfg(feature = "usb")]
pub(super) fn stand_up(
    construction: InterfaceConstruction<'_>,
    context: &PlanRuntimeContext,
) -> AttachmentResult {
    let reported = Arc::new(std::sync::Mutex::new(None));
    let mut interface = AutoUsb::default().with_policy(construction.interface.policy);
    if context.interface_controls().is_some() {
        let reported = Arc::clone(&reported);
        interface = interface.on_status(move |status| {
            if let Ok(mut slot) = reported.lock() {
                *slot = Some(status);
            }
        });
    }
    let attached = construction.attach(interface);
    if let Some(controls) = context.interface_controls() {
        if let Some(status) = reported.lock().ok().and_then(|mut slot| slot.take()) {
            controls(super::RegisteredInterfaceControl::UsbAuto {
                id: attached.id(),
                status,
            });
        }
    }
    Ok(attached.id())
}
