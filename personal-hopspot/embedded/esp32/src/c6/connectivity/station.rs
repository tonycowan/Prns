use super::super::*;
use alloc::string::String;
use esp_radio::wifi::sta::ScanMethod;
use esp_radio::wifi::AuthenticationMethod;

pub(super) struct StationCredentials {
    pub(super) ssid: String,
    pub(super) password: String,
}

#[embassy_executor::task]
pub(super) async fn net_task(mut runner: Runner<'static, WifiStaDevice<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
pub(super) async fn network_ready_task(stack: Stack<'static>) -> ! {
    let mut announced_link_up = false;
    loop {
        if stack.is_link_up() {
            if !announced_link_up {
                announced_link_up = true;
                // Unblock BLE as soon as the STA link is up; DHCP can lag past the fallback.
                WIFI_LINK_UP.signal(());
            }
            if let Some(config) = stack.config_v4() {
                log::info!("wifi: station ready ipv4={config:?}");
            } else {
                log::info!("wifi: station link up (waiting for DHCP)");
            }
        }
        Timer::after(Duration::from_secs(5)).await;
    }
}

#[embassy_executor::task]
pub(super) async fn wifi_connect_task(
    mut controller: WifiController<'static>,
    credentials: StationCredentials,
) -> ! {
    // Threshold authmode is taken from `auth_method`. Wpa2Wpa3Personal as threshold
    // rejects common "WPA2/WPA3" beacons that still advertise as WPA2-PSK
    // (NoAccessPointFoundInAuthmodeThreshold). Wpa2Personal is the ESP-IDF-recommended
    // floor: allows WPA2 and stronger, including mixed BSS, while SAE stays available
    // in the vendor stack. AllChannels helps when Fast scan misses the 2.4 GHz BSS.
    let station = StationConfig::default()
        .with_ssid(credentials.ssid)
        .with_password(credentials.password)
        .with_auth_method(AuthenticationMethod::Wpa2Personal)
        .with_scan_method(ScanMethod::AllChannels)
        .with_failure_retry_cnt(3);
    let config = WifiConfig::Station(station);
    loop {
        if let Err(error) = controller.set_config(&config) {
            log::warn!("wifi: station configuration failed: {error:?}");
            Timer::after(Duration::from_secs(2)).await;
            continue;
        }
        match controller.connect_async().await {
            Ok(_info) => {
                log::info!("wifi: associated");
                match controller.wait_for_disconnect_async().await {
                    Ok(disconnected) => {
                        log::warn!(
                            "wifi: station disconnected ({:?}, rssi {})",
                            disconnected.reason,
                            disconnected.rssi
                        );
                    }
                    Err(error) => {
                        log::warn!("wifi: disconnect monitor failed: {error:?}");
                    }
                }
            }
            Err(error) => {
                log::warn!("wifi: connect failed: {error:?}");
                Timer::after(Duration::from_secs(3)).await;
            }
        }
    }
}

#[embassy_executor::task]
pub(super) async fn wifi_radio_keepalive_task(_controller: WifiController<'static>) -> ! {
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}
