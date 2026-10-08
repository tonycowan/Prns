//! MeshTower V2 power report.
//!
//! The pack divider is P0.21 high into P0.04 (AIN2), 390 kΩ over 100 kΩ, scaled the way
//! Meshtastic does: 12-bit, 3.0 V reference, multiplier 4.916. Raw 1166 is about 4200 mV.
//! Two boots on a full pack read 4191 mV (raw 1164) and 4201 mV (raw 1167), both 100% on
//! the 4190 mV open-circuit curve.
//!
//! The HUSB238 sits on P0.30/P0.05 at address 0x08 and is powered from the battery side.
//! With the battery switch off it did not acknowledge, while the nRF USB pin still reported
//! VBUS ready from the Mac cable. With the switch on, the same cable produced
//! `raw 00 cc`: voltage nibble 0 (no PD voltage), attach bit set, response 1, and no PDO
//! offered. That is a 5 V attach, not a charge contract. A current nibble of 0 is the
//! register's zero encoding in that case, not a measured 500 mA. A 20 V contract is voltage
//! nibble 6. This Mac cannot supply that, and the board has one USB-C plug, so a tethered
//! log cannot show PD charging. Nothing on this module senses the solar input.

use embassy_nrf::gpio::Output;
use embassy_nrf::saadc::Saadc;
use embassy_nrf::twim::{self, Twim};
use embassy_time::{Duration, Timer};
use personal_hopspot_core::{BatteryPercent, ChargingState, ExternalPowerState, PowerSnapshot};

const HUSB238_ADDRESS: u8 = 0x08;
const DIVIDER_SETTLE: Duration = Duration::from_millis(10);
const REPORT_DELAY: Duration = Duration::from_secs(10);
const SAMPLES: u32 = 8;
/// Below this, the divider is treated as an open pack rather than a percentage.
const NO_CELL_MV: u32 = 2600;
const OCV_MV: [u32; 11] = [
    4190, 4050, 3990, 3890, 3800, 3720, 3630, 3530, 3420, 3300, 3100,
];

pub(crate) struct BatteryProbe {
    adc: Saadc<'static, 1>,
    divider: Output<'static>,
    i2c: Twim<'static>,
}

impl BatteryProbe {
    pub(crate) fn new(
        adc: Saadc<'static, 1>,
        divider: Output<'static>,
        i2c: Twim<'static>,
    ) -> Self {
        Self { adc, divider, i2c }
    }

    async fn sample_raw(&mut self) -> u32 {
        self.divider.set_high();
        Timer::after(DIVIDER_SETTLE).await;
        let mut total = 0u32;
        for _ in 0..SAMPLES {
            let mut sample = [0i16; 1];
            self.adc.sample(&mut sample).await;
            let raw = if sample[0] < 0 { 0 } else { sample[0] as u32 };
            total += raw;
        }
        self.divider.set_low();
        total / SAMPLES
    }
}

pub(crate) async fn report(mut probe: BatteryProbe) {
    loop {
        let snapshot = observe(&mut probe).await;
        personal_hopspot_core::publish_power_snapshot(snapshot);
        Timer::after(REPORT_DELAY).await;
    }
}

/// One pack sample plus the HUSB238 reading, logged the same way as the boot report and
/// returned as the remote-control power snapshot.
async fn observe(probe: &mut BatteryProbe) -> PowerSnapshot {
    let raw = probe.sample_raw().await;
    let millivolts = pack_millivolts(raw);
    let battery = if millivolts < NO_CELL_MV {
        console(format_args!("bat: {millivolts}mV no-cell adc {raw}"));
        None
    } else {
        let percent = state_of_charge(millivolts);
        console(format_args!("bat: {millivolts}mV {percent}% adc {raw}"));
        Some(BatteryPercent::saturating(percent))
    };
    let usb = mcu_usb();
    let external = match read_pd(&mut probe.i2c).await {
        Ok(pd) => {
            let attach = if pd.attached { "attached" } else { "open" };
            if pd.voltage == 0 {
                // The current nibble is not a contract until a PD voltage is selected.
                console(format_args!(
                    "pd: no-voltage {attach} resp {} offer {} raw {:02x} {:02x}",
                    pd.response, pd.offered, pd.status0, pd.status1,
                ));
            } else {
                console(format_args!(
                    "pd: {} {}mA {attach} resp {} offer {} raw {:02x} {:02x}",
                    voltage_label(pd.voltage),
                    pd.current_ma,
                    pd.response,
                    pd.offered,
                    pd.status0,
                    pd.status1,
                ));
            }
            console(format_args!("charge: {}", pd.charge));
            external_from_pd(&pd, usb)
        }
        Err(twim::Error::AddressNack | twim::Error::DataNack) => {
            // Seen with the battery switch off. The Mac cable can still power the nRF.
            console(format_args!("pd: nack"));
            console(format_args!("charge: pd-unpowered"));
            external_from_usb(usb)
        }
        Err(_) => {
            console(format_args!("pd: error"));
            console(format_args!("charge: pd-unread"));
            ExternalPowerState::Unknown
        }
    };
    console(format_args!("mcu-usb: {usb}"));
    console(format_args!("solar: unsensed"));
    PowerSnapshot::new(battery, external)
}

/// A 20 V PD contract is the only attach this board treats as charging. Any other attach is
/// external power that is not charging the pack. With no attach, the nRF USB pin is the
/// remaining evidence.
fn external_from_pd(pd: &PdReading, usb: &'static str) -> ExternalPowerState {
    if pd.attached && pd.voltage == 6 {
        ExternalPowerState::Present {
            charging: ChargingState::Charging,
        }
    } else if pd.attached {
        ExternalPowerState::Present {
            charging: ChargingState::Idle,
        }
    } else {
        external_from_usb(usb)
    }
}

fn external_from_usb(usb: &'static str) -> ExternalPowerState {
    match usb {
        "vbus" | "vbus-ready" => ExternalPowerState::Present {
            charging: ChargingState::Unknown,
        },
        "absent" => ExternalPowerState::Absent,
        _ => ExternalPowerState::Unknown,
    }
}

struct PdReading {
    status0: u8,
    status1: u8,
    voltage: u8,
    current_ma: u16,
    attached: bool,
    response: u8,
    offered: &'static str,
    charge: &'static str,
}

async fn read_pd(i2c: &mut Twim<'_>) -> Result<PdReading, twim::Error> {
    let mut regs = [0u8; 8];
    let register = [0u8];
    i2c.write_read(HUSB238_ADDRESS, &register, &mut regs)
        .await?;
    let status0 = regs[0];
    let status1 = regs[1];
    let voltage = status0 >> 4;
    let attached = status1 & (1 << 6) != 0;
    let response = (status1 >> 3) & 0x07;
    // Voltage nibble 6 is the 20 V PD contract. Any other attach, including the
    // battery-on Mac capture (nibble 0, attach set), is not that contract.
    let charge = if !attached {
        "no-pd-attach"
    } else if voltage == 6 {
        "usb-pd-20v"
    } else {
        "usb-not-20v"
    };
    Ok(PdReading {
        status0,
        status1,
        voltage,
        current_ma: current_ma(status0 & 0x0f),
        attached,
        response,
        offered: offered_label(&regs[2..8]),
        charge,
    })
}

fn offered_label(pdos: &[u8]) -> &'static str {
    let mask = pdos.iter().enumerate().fold(0u8, |mask, (index, byte)| {
        if byte & 0x80 != 0 {
            mask | (1 << index)
        } else {
            mask
        }
    });
    match mask {
        0 => "none",
        0b000001 => "5V",
        0b000011 => "5V 9V",
        0b000111 => "5V 9V 12V",
        0b001111 => "5V 9V 12V 15V",
        0b011111 => "5V 9V 12V 15V 18V",
        0b111111 => "5V 9V 12V 15V 18V 20V",
        0b100001 => "5V 20V",
        0b100000 => "20V",
        _ => "mixed",
    }
}

fn voltage_label(code: u8) -> &'static str {
    match code {
        0 => "no-voltage",
        1 => "5V",
        2 => "9V",
        3 => "12V",
        4 => "15V",
        5 => "18V",
        6 => "20V",
        _ => "other",
    }
}

fn current_ma(code: u8) -> u16 {
    match code {
        0 => 500,
        1 => 700,
        2 => 1000,
        3 => 1250,
        4 => 1500,
        5 => 1750,
        6 => 2000,
        7 => 2250,
        8 => 2500,
        9 => 2750,
        10 => 3000,
        _ => 0,
    }
}

fn mcu_usb() -> &'static str {
    let mut status = 0u32;
    // SoftDevice owns POWER. This SVC is the allowed read of USBREGSTATUS, and it writes one
    // word through the pointer.
    let result = unsafe { nrf_softdevice::raw::sd_power_usbregstatus_get(&mut status) };
    if result != 0 {
        return "unread";
    }
    match (status & 1 != 0, status & 2 != 0) {
        (false, _) => "absent",
        (true, false) => "vbus",
        (true, true) => "vbus-ready",
    }
}

const fn pack_millivolts(raw: u32) -> u32 {
    raw * 14_748 / 4_096
}

fn state_of_charge(millivolts: u32) -> u8 {
    if millivolts >= OCV_MV[0] {
        return 100;
    }
    if millivolts <= OCV_MV[OCV_MV.len() - 1] {
        return 0;
    }
    let mut index = 1;
    while index < OCV_MV.len() && millivolts < OCV_MV[index] {
        index += 1;
    }
    let above = millivolts - OCV_MV[index];
    let span = OCV_MV[index - 1] - OCV_MV[index];
    let floor = (10 - index as u32) * 10;
    (floor + above * 10 / span) as u8
}

fn console(args: core::fmt::Arguments<'_>) {
    #[cfg(feature = "usb-debug-log")]
    log::info!("{args}");
    #[cfg(not(feature = "usb-debug-log"))]
    let _ = args;
}

#[embassy_executor::task]
pub(crate) async fn report_battery(probe: BatteryProbe) {
    report(probe).await;
}

/// One line a second, from the first second, so a host can see that this port is the console.
#[embassy_executor::task]
pub(crate) async fn console_tick() {
    let mut second = 0u32;
    loop {
        console(format_args!("tick: {second}"));
        second = second.wrapping_add(1);
        Timer::after(Duration::from_secs(1)).await;
    }
}

const _: () = {
    assert!(pack_millivolts(0) == 0);
    assert!(pack_millivolts(1_166) >= 4_195);
    assert!(pack_millivolts(1_166) <= 4_205);
};
