use core::cell::RefCell;
use core::sync::atomic::{compiler_fence, Ordering};

use embassy_nrf::config::HfclkSource;
use embassy_nrf::gpio::{Input, Level, Output, OutputDrive, Pull};
use embassy_nrf::interrupt::{self, InterruptExt, Priority};
use embassy_nrf::mode::Blocking;
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::pac;
use embassy_nrf::rng::Rng;
use embassy_nrf::saadc::{self, ChannelConfig, Config as SaadcConfig, Gain, Reference, Saadc};
use embassy_nrf::spim::{self, Spim};
use embassy_nrf::twim::{self, Twim};
use embassy_nrf::usb::vbus_detect::SoftwareVbusDetect;
use embassy_nrf::usb::Driver;
use embassy_nrf::{bind_interrupts, config, peripherals, usb};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::{Delay, Duration, Instant, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use personal_hopspot_core::{ChargingState, ExternalPowerState};
use personal_rns::lora::LoRaInterface;
use personal_rns::radios::sx126x::{BoardConfig, FrontendControl, Sx126x, TcxoVoltage};
use static_cell::StaticCell;

use crate::boards::status_led::StatusLed;

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    TWISPI0 => spim::InterruptHandler<peripherals::TWISPI0>;
    TWISPI1 => twim::InterruptHandler<peripherals::TWISPI1>;
    SAADC => saadc::InterruptHandler;
});

/// Hynetek HUSB238 USB-PD sink on MeshTower V2 (I²C address 0x08).
const HUSB238_ADDR: u8 = 0x08;
const HUSB238_PD_STATUS0: u8 = 0x00;
const HUSB238_PD_STATUS1: u8 = 0x01;
const HUSB238_ATTACH: u8 = 1 << 6;
/// Datasheet PD_SRC_VOLTAGE encoding for an explicit 20 V contract.
const HUSB238_PD_20V: u8 = 0b0110;

type MeshTowerV2SpiDevice = ExclusiveDevice<Spim<'static>, Output<'static>, Delay>;

type MeshTowerV2Radio =
    Sx126x<MeshTowerV2SpiDevice, Input<'static>, Input<'static>, Output<'static>, Delay>;

pub(crate) type MeshTowerV2LoraInterface = LoRaInterface<'static, MeshTowerV2Radio>;

type MeshTowerV2UsbDriver = Driver<'static, &'static SoftwareVbusDetect>;

pub(crate) struct MeshTowerV2Hardware {
    pub(crate) usb: MeshTowerV2UsbDriver,
    pub(crate) vbus: &'static SoftwareVbusDetect,
    pub(crate) radio: MeshTowerV2Radio,
    pub(crate) battery: MeshTowerV2Battery,
    pub(crate) pd_sink: MeshTowerV2PdSink,
    pub(crate) status_led: StatusLed,
    pub(crate) button: Input<'static>,
}

/// Heltec MeshTower V2 VBAT sense: gated divider into AIN2 (P0.04), ADC_CTRL on P0.21.
///
/// Sampling follows Meshtastic `AnalogBatteryLevel::getBattVoltage` for
/// `heltec_mesh_tower_v2` (Power.cpp + variant.h): enable ADC_CTRL, settle 10 ms,
/// average [`BATTERY_SENSE_SAMPLES`] reads, disable ADC_CTRL, scale with
/// AREF 3.0 V / 12-bit / [`ADC_MULTIPLIER`] 4.916.
///
/// Conversions busy-poll `EVENTS_END` (Adafruit/Meshtastic `analogRead` style) instead
/// of embassy's SAADC IRQ waiter — SoftDevice keeps that IRQ from waking, which left
/// DescribePower stuck on [`PowerSnapshot::UNKNOWN`](personal_hopspot_core::PowerSnapshot).
pub(crate) struct MeshTowerV2Battery {
    /// Keeps the embassy channel configuration / peripheral ownership alive.
    _adc: Saadc<'static, 1>,
    divider_enable: Output<'static>,
    calibrated: bool,
}

/// Meshtastic `BATTERY_SENSE_SAMPLES` default in Power.cpp.
const BATTERY_SENSE_SAMPLES: u32 = 15;
/// Meshtastic `battery_adcEnable` settle (`delay(10)`).
const ADC_CTRL_SETTLE_MS: u64 = 10;

impl MeshTowerV2Battery {
    /// Meshtastic-equivalent VBAT millivolts, or `None` if SAADC never completes.
    pub(crate) async fn sample_millivolts(&mut self) -> Option<u32> {
        if !self.calibrated {
            let _ = saadc_busy_calibrate(Duration::from_millis(100));
            self.calibrated = true;
        }

        // Meshtastic battery_adcEnable(): ADC_CTRL = HIGH, delay(10).
        self.divider_enable.set_high();
        Timer::after_millis(ADC_CTRL_SETTLE_MS).await;

        let mut raw_sum: i32 = 0;
        for _ in 0..BATTERY_SENSE_SAMPLES {
            let Some(raw) = saadc_busy_sample(Duration::from_millis(50)) else {
                // Meshtastic battery_adcDisable(): ADC_CTRL = !ENABLED (LOW).
                self.divider_enable.set_low();
                return None;
            };
            raw_sum += i32::from(raw);
            // Yield between samples so SoftDevice BLE can run.
            Timer::after_millis(1).await;
        }

        // Meshtastic battery_adcDisable().
        self.divider_enable.set_low();

        let raw = raw_sum / BATTERY_SENSE_SAMPLES as i32;
        let raw = if raw < 0 {
            0i16
        } else if raw > i32::from(i16::MAX) {
            i16::MAX
        } else {
            raw as i16
        };
        Some(battery_millivolts(raw))
    }
}

/// SoftDevice-safe one-shot: poll `EVENTS_*` without depending on the SAADC IRQ.
fn saadc_busy_calibrate(timeout: Duration) -> bool {
    let r = pac::SAADC;
    r.intenclr().write(|w| w.0 = 0x003F_FFFF);
    r.events_calibratedone().write_value(0);
    compiler_fence(Ordering::SeqCst);
    r.tasks_calibrateoffset().write_value(1);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if r.events_calibratedone().read() != 0 {
            r.events_calibratedone().write_value(0);
            return true;
        }
    }
    false
}

fn saadc_busy_sample(timeout: Duration) -> Option<i16> {
    let r = pac::SAADC;
    let mut buf = [0i16; 1];
    r.intenclr().write(|w| w.0 = 0x003F_FFFF);
    r.result().ptr().write_value(buf.as_mut_ptr() as u32);
    r.result().maxcnt().write(|w| w.set_maxcnt(1));
    r.events_end().write_value(0);
    compiler_fence(Ordering::SeqCst);
    r.tasks_start().write_value(1);
    r.tasks_sample().write_value(1);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if r.events_end().read() != 0 {
            compiler_fence(Ordering::SeqCst);
            r.events_end().write_value(0);
            saadc_stop_immediately();
            return Some(buf[0]);
        }
    }
    saadc_stop_immediately();
    None
}

fn saadc_stop_immediately() {
    let r = pac::SAADC;
    compiler_fence(Ordering::SeqCst);
    r.events_stopped().write_value(0);
    r.tasks_stop().write_value(1);
    let deadline = Instant::now() + Duration::from_millis(10);
    while Instant::now() < deadline {
        if r.events_stopped().read() != 0 {
            r.events_stopped().write_value(0);
            return;
        }
    }
}

/// HUSB238 on Wire0 (P0.30 SDA / P0.05 SCL). SoftDevice USBREGSTATUS is MCU 5 V only;
/// pack charge needs a USB-PD ~20 V contract through this sink.
pub(crate) struct MeshTowerV2PdSink {
    i2c: Twim<'static>,
}

impl MeshTowerV2PdSink {
    pub(crate) async fn external_power(&mut self) -> ExternalPowerState {
        let mut status0 = [0u8];
        let mut status1 = [0u8];
        // Register index must live in RAM for the TWIM TX DMA path.
        let reg0 = [HUSB238_PD_STATUS0];
        let reg1 = [HUSB238_PD_STATUS1];
        if self
            .i2c
            .write_read(HUSB238_ADDR, &reg0, &mut status0)
            .await
            .is_err()
            || self
                .i2c
                .write_read(HUSB238_ADDR, &reg1, &mut status1)
                .await
                .is_err()
        {
            return ExternalPowerState::Unknown;
        }
        husb238_external_power(status0[0], status1[0])
    }
}

/// Map HUSB238 PD_STATUS0/1 into the shared power observation.
///
/// Heltec documents that only a PD 3.0 **20 V** contract feeds the pack charger; 5 V (and
/// other non-20 V contracts) power the MCU rail only. Without BMS current we treat 20 V as
/// charging and any other attach as present-but-idle.
pub(crate) const fn husb238_external_power(status0: u8, status1: u8) -> ExternalPowerState {
    let voltage = (status0 >> 4) & 0x0F;
    let attached = (status1 & HUSB238_ATTACH) != 0;
    if !attached || voltage == 0 {
        return ExternalPowerState::Absent;
    }
    if voltage == HUSB238_PD_20V {
        ExternalPowerState::Present {
            charging: ChargingState::Charging,
        }
    } else {
        ExternalPowerState::Present {
            charging: ChargingState::Idle,
        }
    }
}

struct HeldIo {
    fem_enable: Output<'static>,
    fem_tx_rx: Output<'static>,
    watchdog_done: Output<'static>,
    _watchdog_wake: Input<'static>,
    _gps_en: Output<'static>,
    _pa_detect: Input<'static>,
}

static HELD_IO: Mutex<CriticalSectionRawMutex, RefCell<Option<HeldIo>>> =
    Mutex::new(RefCell::new(None));

pub(crate) struct MeshTowerV2Board;

impl MeshTowerV2Board {
    pub(crate) async fn initialize<R>(
        bootstrap: impl FnOnce(&mut Nvmc<'static>, Rng<'static, Blocking>) -> R,
    ) -> (R, MeshTowerV2Hardware) {
        let mut nrf_config = config::Config::default();
        nrf_config.hfclk_source = HfclkSource::ExternalXtal;
        nrf_config.gpiote_interrupt_priority = Priority::P2;
        nrf_config.time_interrupt_priority = Priority::P2;
        let peripherals = embassy_nrf::init(nrf_config);

        let identity = {
            let mut nvmc = Nvmc::new(peripherals.NVMC);
            let rng = Rng::new_blocking(peripherals.RNG);
            bootstrap(&mut nvmc, rng)
        };

        // SoftDevice reserves P0/P1; keep app interrupts off those. USB at P2, SPI/TWIM/SAADC at P3
        // so a BLE radio event can preempt LoRa SPI, PD I²C, and battery conversion.
        interrupt::USBD.set_priority(Priority::P2);
        interrupt::TWISPI0.set_priority(Priority::P3);
        interrupt::TWISPI1.set_priority(Priority::P3);
        interrupt::SAADC.set_priority(Priority::P3);
        static SOFTWARE_VBUS: StaticCell<SoftwareVbusDetect> = StaticCell::new();
        let vbus = crate::runtime::software_vbus::initialize(&SOFTWARE_VBUS);
        let usb = Driver::new(peripherals.USBD, Irqs, vbus);

        // AIN2 sees VBAT through Heltec's gated 390k/100k divider (ADC_CTRL high enables it).
        // Gain 1/5 against the 0.6 V internal reference yields the 3.0 V ADC range Heltec uses.
        let mut battery_channel = ChannelConfig::single_ended(peripherals.P0_04);
        battery_channel.reference = Reference::INTERNAL;
        battery_channel.gain = Gain::GAIN1_5;
        let battery_adc = Saadc::new(
            peripherals.SAADC,
            Irqs,
            SaadcConfig::default(),
            [battery_channel],
        );
        let battery = MeshTowerV2Battery {
            _adc: battery_adc,
            divider_enable: Output::new(peripherals.P0_21, Level::Low, OutputDrive::Standard),
            calibrated: false,
        };

        // HUSB238 PD sink on the only Wire bus Meshtastic documents for MeshTower V2.
        static TWIM_TX_BUF: StaticCell<[u8; 4]> = StaticCell::new();
        let twim_tx_buf = TWIM_TX_BUF.init([0; 4]);
        let mut twim_config = twim::Config::default();
        twim_config.frequency = twim::Frequency::K100;
        let pd_sink = MeshTowerV2PdSink {
            i2c: Twim::new(
                peripherals.TWISPI1,
                Irqs,
                peripherals.P0_30,
                peripherals.P0_05,
                twim_config,
                twim_tx_buf,
            ),
        };

        let mut radio_spim_config = spim::Config::default();
        radio_spim_config.frequency = spim::Frequency::M4;
        let radio_sck = peripherals.P0_19;
        let radio_miso = peripherals.P0_23;
        let radio_mosi = peripherals.P0_22;
        let radio_bus = Spim::new(
            peripherals.TWISPI0,
            Irqs,
            radio_sck,
            radio_miso,
            radio_mosi,
            radio_spim_config,
        );
        let radio_cs = Output::new(peripherals.P0_24, Level::High, OutputDrive::Standard);
        let radio_spi = ExclusiveDevice::new(radio_bus, radio_cs, Delay).unwrap();
        let radio_busy = Input::new(peripherals.P0_17, Pull::None);
        let radio_dio1 = Input::new(peripherals.P0_20, Pull::None);
        let mut radio_reset = Output::new(peripherals.P0_25, Level::Low, OutputDrive::Standard);
        Timer::after_millis(2).await;
        radio_reset.set_high();

        let pa_detect = Input::new(peripherals.P0_13, Pull::None);
        let mut fem_enable = Output::new(peripherals.P0_15, Level::High, OutputDrive::Standard);
        fem_enable.set_high();
        Timer::after_millis(1).await;
        let mut fem_tx_rx = Output::new(peripherals.P0_16, Level::Low, OutputDrive::Standard);
        fem_tx_rx.set_low();

        // P0.09/P0.10 are NFC by default; board-mesh-tower-v2 enables nfc-pins-as-gpio.
        let mut watchdog_done = Output::new(peripherals.P0_09, Level::Low, OutputDrive::Standard);
        let watchdog_wake = Input::new(peripherals.P0_10, Pull::None);
        watchdog_done.set_low();
        Timer::after_millis(1).await;
        watchdog_done.set_high();
        Timer::after_millis(1).await;
        watchdog_done.set_low();

        // GPS_EN is active-low; hold it high so the L76K stays off until a GPS face exists.
        let gps_en = Output::new(peripherals.P0_07, Level::High, OutputDrive::Standard);

        HELD_IO.lock(|held| {
            *held.borrow_mut() = Some(HeldIo {
                fem_enable,
                fem_tx_rx,
                watchdog_done,
                _watchdog_wake: watchdog_wake,
                _gps_en: gps_en,
                _pa_detect: pa_detect,
            });
        });

        let radio = Sx126x::new(
            radio_spi,
            radio_busy,
            radio_dio1,
            radio_reset,
            Delay,
            BoardConfig {
                tcxo_voltage: Some(TcxoVoltage::V1_8),
                use_dcdc: true,
                rx_boost: true,
                dio2_as_rf_switch: true,
                external_rx_gain_db: 0,
                external_power_amplifier: None,
                frontend_control: FrontendControl::TxRx {
                    enter_transmit,
                    enter_receive,
                },
            },
        );

        let status_led = StatusLed::active_low(Output::new(
            peripherals.P1_15,
            Level::High,
            OutputDrive::Standard,
        ));
        let button = Input::new(peripherals.P1_10, Pull::Up);

        (
            identity,
            MeshTowerV2Hardware {
                usb,
                vbus,
                radio,
                battery,
                pd_sink,
                status_led,
                button,
            },
        )
    }
}

fn enter_transmit() {
    HELD_IO.lock(|held| {
        if let Some(io) = held.borrow_mut().as_mut() {
            io.fem_enable.set_high();
            io.fem_tx_rx.set_high();
        }
    });
}

fn enter_receive() {
    HELD_IO.lock(|held| {
        if let Some(io) = held.borrow_mut().as_mut() {
            io.fem_enable.set_high();
            io.fem_tx_rx.set_low();
        }
    });
}

pub(crate) fn pet_watchdog() {
    HELD_IO.lock(|held| {
        if let Some(io) = held.borrow_mut().as_mut() {
            io.watchdog_done.set_high();
        }
    });
}

pub(crate) fn release_watchdog() {
    HELD_IO.lock(|held| {
        if let Some(io) = held.borrow_mut().as_mut() {
            io.watchdog_done.set_low();
        }
    });
}

/// Meshtastic / Heltec scale:
/// `ADC_MULTIPLIER * ((1000 * AREF_VOLTAGE) / 2^BATTERY_SENSE_RESOLUTION_BITS) * raw`
/// with AREF=3.0, bits=12, multiplier=4.916 → integer `(raw * 3000 * 4916) / (4096 * 1000)`.
const fn battery_millivolts(raw: i16) -> u32 {
    let clamped = if raw < 0 { 0u32 } else { raw as u32 };
    (clamped * 3_000 * 4_916) / (4_096 * 1_000)
}

#[cfg(test)]
mod tests {
    use super::{battery_millivolts, husb238_external_power, HUSB238_ATTACH, HUSB238_PD_20V};
    use personal_hopspot_core::{ChargingState, ExternalPowerState};

    #[test]
    fn full_cell_maps_near_4200_mv() {
        assert!(battery_millivolts(0) == 0);
        assert!(battery_millivolts(1_166) >= 4_195);
        assert!(battery_millivolts(1_166) <= 4_205);
    }

    #[test]
    fn husb238_absent_when_unattached() {
        assert_eq!(
            husb238_external_power(0, 0),
            ExternalPowerState::Absent
        );
        // Attached flag without a negotiated voltage still means no contract.
        assert_eq!(
            husb238_external_power(0, HUSB238_ATTACH),
            ExternalPowerState::Absent
        );
    }

    #[test]
    fn husb238_five_volt_is_present_idle() {
        let five_volt = 0b0001 << 4;
        assert_eq!(
            husb238_external_power(five_volt, HUSB238_ATTACH),
            ExternalPowerState::Present {
                charging: ChargingState::Idle,
            }
        );
    }

    #[test]
    fn husb238_twenty_volt_is_present_charging() {
        let twenty_volt = HUSB238_PD_20V << 4;
        assert_eq!(
            husb238_external_power(twenty_volt, HUSB238_ATTACH),
            ExternalPowerState::Present {
                charging: ChargingState::Charging,
            }
        );
    }
}
