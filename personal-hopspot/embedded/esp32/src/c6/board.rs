use esp_hal::clock::CpuClock;
use esp_hal::efuse::base_mac_address;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::peripherals::{BT, WIFI};
use esp_hal::rng::TrngSource;
use esp_hal::rtc_cntl::Rtc;
use esp_hal::timer::timg::TimerGroup;
use personal_rns::engine::InstantMillis;
use personal_rns::manifold::embassy::EmbassyTimebase;

pub(crate) const ANNOUNCE_APP_DATA: &[u8] = b"\x92\xc4\x13Personal Hopspot C6\xc0";
pub(crate) const NODE_ANNOUNCE_APP_DATA: &[u8] = b"Personal Hopspot C6";

/// BLE + station Wi-Fi Auto share one allocator. Keep dram_seg modest so the residual main
/// `.stack` can absorb bring-up; put the rest in reclaimed dram2 (C6 max ~64 KiB). Isolated
/// peaks were ~60 KiB (Wi-Fi) and ~43 KiB (BLE); 128 KiB total is the tandem target.
/// USB-Serial-JTAG stays with `esp-println`.
const HEAP_BYTES: usize = 64 * 1024;
const RECLAIMED_HEAP_BYTES: usize = 64 * 1024;

pub(crate) struct C6Hardware {
    pub(crate) wifi: WIFI<'static>,
    pub(crate) bluetooth: BT<'static>,
    pub(crate) identity_entropy: TrngSource<'static>,
    pub(crate) mac: [u8; 6],
    pub(crate) timebase: EmbassyTimebase,
    pub(crate) _rtc: Rtc<'static>,
    /// Seeed FM8625H RF switch: GPIO3 low powers the switch (active-low enable).
    pub(crate) _rf_switch_power: Output<'static>,
    /// GPIO14 low selects the onboard ceramic antenna; high would select U.FL.
    pub(crate) _rf_antenna_select: Output<'static>,
    /// Yellow user LED on GPIO15 (active-low).
    pub(crate) user_led: Output<'static>,
}

pub(crate) struct XiaoEsp32C6;

impl XiaoEsp32C6 {
    pub(crate) fn bringup() -> C6Hardware {
        esp_println::logger::init_logger_from_env();
        esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: RECLAIMED_HEAP_BYTES);
        esp_alloc::heap_allocator!(size: HEAP_BYTES);
        esp_println::println!("XIAO ESP32-C6 boot {}", env!("HOPSPOT_BUILD_IDENTITY"));

        let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
        let peripherals = esp_hal::init(config);

        // Claim the antenna RF switch before radio bring-up. Undriven pins leave the switch
        // unpowered and force leaky RF; Arduino's XIAO board package does the same in initVariant.
        let rf_switch_power = Output::new(peripherals.GPIO3, Level::Low, OutputConfig::default());
        let rf_antenna_select =
            Output::new(peripherals.GPIO14, Level::Low, OutputConfig::default());
        // Active-low user LED: high = extinguished until the 1 Hz heartbeat task takes over.
        let user_led = Output::new(peripherals.GPIO15, Level::High, OutputConfig::default());
        esp_println::println!("rf: ceramic antenna selected (switch powered)");

        let timer_group = TimerGroup::new(peripherals.TIMG0);
        let software_interrupt = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
        esp_rtos::start(timer_group.timer0, software_interrupt.software_interrupt0);

        let mut rtc = Rtc::new(peripherals.LPWR);
        rtc.rwdt.disable();
        rtc.swd.disable();
        let timebase = EmbassyTimebase::start_at(InstantMillis(rtc.current_time_us() / 1000));
        let identity_entropy = TrngSource::new(peripherals.RNG, peripherals.ADC1);

        let base_mac = base_mac_address();
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&base_mac.as_bytes()[..6]);

        C6Hardware {
            wifi: peripherals.WIFI,
            bluetooth: peripherals.BT,
            identity_entropy,
            mac,
            timebase,
            _rtc: rtc,
            _rf_switch_power: rf_switch_power,
            _rf_antenna_select: rf_antenna_select,
            user_led,
        }
    }
}
