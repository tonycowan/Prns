//! Station-only Wi-Fi Auto bring-up for the SRAM-only XIAO ESP32-C6 (BLE + Wi-Fi Auto profile).

mod station;
mod wifi;

pub(super) use wifi::build_wifi;
