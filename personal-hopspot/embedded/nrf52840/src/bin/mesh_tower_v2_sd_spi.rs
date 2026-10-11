//! Hardware-SPI probe for the MeshTower V2 microSD socket.
//!
//! Build from `personal-hopspot/embedded/nrf52840`:
//!
//! ```text
//! cargo build --release --locked --no-default-features --bin mesh-tower-v2-sd-spi \
//!   --features board-mesh-tower-v2,softdevice-s140-v6 --target thumbv7em-none-eabihf
//! ```
//!
//! Flash the application the same way as the tower firmware. It links at the same
//! address and does not start the SoftDevice or the radio stack. Open the USB serial
//! port it enumerates ("MeshTower SD SPI"). Lines are kept until the port is open.

#![no_std]
#![no_main]

use core::cell::RefCell;
use core::fmt::Write;

use embassy_embedded_hal::SetConfig;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_nrf::config::HfclkSource;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::spim::{self, Spim};
use embassy_nrf::usb::vbus_detect::{HardwareVbusDetect, InterruptHandler as VbusInterruptHandler};
use embassy_nrf::usb::Driver;
use embassy_nrf::{bind_interrupts, pac, peripherals, usb};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::{Instant, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::{Builder, Config};
use panic_halt as _;
use static_cell::StaticCell;

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    SPI2 => spim::InterruptHandler<peripherals::SPI2>;
    CLOCK_POWER => VbusInterruptHandler;
});

const RING_BYTES: usize = 2048;
const CMD0: [u8; 6] = [0x40, 0x00, 0x00, 0x00, 0x00, 0x95];
const CMD8: [u8; 6] = [0x48, 0x00, 0x00, 0x01, 0xAA, 0x87];

struct Ring {
    bytes: [u8; RING_BYTES],
    head: usize,
    len: usize,
}

impl Ring {
    const fn new() -> Self {
        Self {
            bytes: [0; RING_BYTES],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, data: &[u8]) {
        for &byte in data {
            if self.len == RING_BYTES {
                self.head = (self.head + 1) % RING_BYTES;
                self.len -= 1;
            }
            let tail = (self.head + self.len) % RING_BYTES;
            self.bytes[tail] = byte;
            self.len += 1;
        }
    }

    fn take(&mut self, out: &mut [u8]) -> usize {
        let n = self.len.min(out.len());
        for slot in out.iter_mut().take(n) {
            *slot = self.bytes[self.head];
            self.head = (self.head + 1) % RING_BYTES;
        }
        self.len -= n;
        n
    }
}

static LOG: Mutex<CriticalSectionRawMutex, RefCell<Ring>> = Mutex::new(RefCell::new(Ring::new()));

#[embassy_executor::main]
async fn main(_spawner: Spawner) -> ! {
    // Pulse the external watchdog before the crystal starts. A late pet here
    // resets the board before USB can enumerate.
    pet_raw();
    let mut nrf_config = embassy_nrf::config::Config::default();
    nrf_config.hfclk_source = HfclkSource::ExternalXtal;
    let p = embassy_nrf::init(nrf_config);

    let mut watchdog = Output::new(p.P0_09, Level::Low, OutputDrive::Standard);
    pet(&mut watchdog).await;
    // Keep the GNSS rail off and the radio in reset so this image only clocks the card.
    let _gps_off = Output::new(p.P0_07, Level::High, OutputDrive::Standard);
    let _radio_reset = Output::new(p.P0_25, Level::Low, OutputDrive::Standard);
    let _fem_off = Output::new(p.P0_15, Level::Low, OutputDrive::Standard);
    let mut led = Output::new(p.P1_15, Level::Low, OutputDrive::Standard);

    // The SoftDevice is not running, so POWER events are ours. A one-shot
    // software snapshot never sees the regulator become ready.
    let driver = Driver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));

    let mut config = Config::new(0x1915, 0x520F);
    config.manufacturer = Some("Hopspot");
    config.product = Some("MeshTower SD SPI");
    config.serial_number = Some("sd-spi");
    config.max_packet_size_0 = 64;
    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static MSOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL: StaticCell<[u8; 64]> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        MSOS_DESC.init([0; 256]),
        CONTROL.init([0; 64]),
    );
    static CDC: StaticCell<State<'static>> = StaticCell::new();
    let class = CdcAcmClass::new(&mut builder, CDC.init(State::new()), 64);
    let mut usb = builder.build();

    // HT-N5262M: CS P1.00, CLK P0.06, MOSI P1.01, MISO P0.26. SPIM2 tops out at 8 MHz.
    let mut spi_config = spim::Config::default();
    spi_config.frequency = spim::Frequency::K125;
    spi_config.mode = spim::MODE_0;
    spi_config.orc = 0xFF;
    spi_config.sck_drive = OutputDrive::HighDrive;
    spi_config.mosi_drive = OutputDrive::HighDrive;
    let mut cs = Output::new(p.P1_00, Level::High, OutputDrive::HighDrive);
    // Bit-bang while the pins are still GPIO. SPIM stays enabled after this.
    bitbang_cmd0(&mut cs);
    let mut spi = Spim::new(p.SPI2, Irqs, p.P0_06, p.P0_26, p.P1_01, spi_config);
    configure_spim_pins();

    say("sd-spi: harness ready, open the MeshTower SD SPI serial port");
    embassy_futures::join::join(
        usb.run(),
        embassy_futures::join::join(
            drain(class),
            exercise(&mut spi, &mut cs, &mut led, &mut watchdog),
        ),
    )
    .await;
    loop {
        Timer::after_secs(1).await;
    }
}

async fn drain(mut class: CdcAcmClass<'static, Driver<'static, HardwareVbusDetect>>) -> ! {
    let mut packet = [0u8; 64];
    loop {
        class.wait_connection().await;
        say("sd-spi: host connected");
        loop {
            let n = LOG.lock(|log| log.borrow_mut().take(&mut packet));
            if n == 0 {
                Timer::after_millis(20).await;
                continue;
            }
            if class.write_packet(&packet[..n]).await.is_err() {
                break;
            }
        }
    }
}

async fn exercise(
    spi: &mut Spim<'static>,
    cs: &mut Output<'static>,
    led: &mut Output<'static>,
    watchdog: &mut Output<'static>,
) -> ! {
    let mut pass = 0u32;
    loop {
        pass = pass.wrapping_add(1);
        led.set_low();
        pet(watchdog).await;
        note(format_args!("sd-spi: pass {pass}"));
        let ready = init_card(spi, cs, watchdog).await;
        if ready {
            for frequency in [
                spim::Frequency::K125,
                spim::Frequency::K500,
                spim::Frequency::M1,
                spim::Frequency::M4,
                spim::Frequency::M8,
            ] {
                pet(watchdog).await;
                if !set_frequency(spi, frequency) {
                    note(format_args!(
                        "sd-spi: {} clock rejected",
                        freq_label(frequency)
                    ));
                    break;
                }
                note(format_args!(
                    "sd-spi: reading sector 0 at {}",
                    freq_label(frequency)
                ));
                Timer::after_millis(1).await;
                match read_sector0(spi, cs).await {
                    Sector::Data {
                        micros,
                        head,
                        end,
                        mark,
                    } => {
                        let bytes_per_sec = if micros == 0 {
                            0
                        } else {
                            512 * 1_000_000 / micros
                        };
                        note(format_args!(
                            "sd-spi: {} sector 0 in {micros} us ({bytes_per_sec} B/s) {}",
                            freq_label(frequency),
                            Hex(&head)
                        ));
                        match mark {
                            Some(offset) => note(format_args!(
                                "sd-spi: {} mark {offset} end {}",
                                freq_label(frequency),
                                Hex(&end)
                            )),
                            None => note(format_args!(
                                "sd-spi: {} mark missing end {}",
                                freq_label(frequency),
                                Hex(&end)
                            )),
                        }
                    }
                    Sector::NoToken { r1, sample } => {
                        note(format_args!(
                            "sd-spi: {} no data token r1 {r1:02x} {}",
                            freq_label(frequency),
                            Hex(&sample)
                        ));
                        break;
                    }
                    Sector::Command { sample } => {
                        note(format_args!(
                            "sd-spi: {} command failed {}",
                            freq_label(frequency),
                            Hex(&sample)
                        ));
                        break;
                    }
                    Sector::Dma => {
                        note(format_args!(
                            "sd-spi: {} transfer rejected",
                            freq_label(frequency)
                        ));
                        break;
                    }
                    Sector::Wedged => {
                        note(format_args!(
                            "sd-spi: {} transfer wedged",
                            freq_label(frequency)
                        ));
                        break;
                    }
                }
                Timer::after_millis(1).await;
            }
        }
        led.set_high();
        for _ in 0..20 {
            pet(watchdog).await;
            Timer::after_millis(100).await;
        }
    }
}

async fn init_card(
    spi: &mut Spim<'static>,
    cs: &mut Output<'static>,
    watchdog: &mut Output<'static>,
) -> bool {
    if !set_frequency(spi, spim::Frequency::K125) {
        note(format_args!("sd-spi: 125 kHz clock rejected"));
        return false;
    }
    let mut idle = false;
    for try_index in 1..=4 {
        pet(watchdog).await;
        if !preamble(spi, cs).await {
            return false;
        }
        let cmd0 = command(spi, cs, &CMD0).await;
        if cmd0.wedged {
            note(format_args!("sd-spi: transfer wedged"));
            return false;
        }
        note(format_args!(
            "sd-spi: spim try {try_index} cmd0 {} r1 {}{}",
            Hex(&cmd0.sample),
            reply_label(&cmd0.sample, cmd0.r1),
            Lead(&cmd0.lead)
        ));
        if cmd0.r1 == Some(0x01) {
            idle = true;
            break;
        }
    }
    if !idle {
        return false;
    }

    let cmd8 = command(spi, cs, &CMD8).await;
    if cmd8.wedged {
        note(format_args!("sd-spi: transfer wedged"));
        return false;
    }
    note(format_args!(
        "sd-spi: cmd8 {} r1 {}",
        Hex(&cmd8.sample),
        r1_text(cmd8.r1)
    ));

    let mut tries = 0u32;
    let mut ready = false;
    while tries < 40 {
        tries += 1;
        pet(watchdog).await;
        let cmd55 = command(spi, cs, &frame(0x77, 0)).await;
        let acmd = command(spi, cs, &frame(0x69, 0x4000_0000)).await;
        if cmd55.wedged || acmd.wedged {
            note(format_args!("sd-spi: transfer wedged"));
            return false;
        }
        if tries == 1 || tries % 10 == 0 || acmd.r1 == Some(0x00) {
            note(format_args!(
                "sd-spi: acmd41 try {tries} cmd55 {} acmd {}",
                r1_text(cmd55.r1),
                r1_text(acmd.r1)
            ));
        }
        if acmd.r1 == Some(0x00) {
            ready = true;
            break;
        }
        Timer::after_millis(20).await;
    }
    if !ready {
        note(format_args!("sd-spi: card stayed idle"));
        return false;
    }
    let cmd58 = command(spi, cs, &frame(0x7A, 0)).await;
    if cmd58.wedged {
        note(format_args!("sd-spi: transfer wedged"));
        return false;
    }
    note(format_args!("sd-spi: cmd58 {}", Hex(&cmd58.sample)));
    true
}

enum Sector {
    Data {
        micros: u64,
        head: [u8; 16],
        end: [u8; 8],
        mark: Option<u16>,
    },
    NoToken {
        r1: u8,
        sample: [u8; 8],
    },
    Command {
        sample: [u8; 8],
    },
    Dma,
    Wedged,
}

async fn read_sector0(spi: &mut Spim<'static>, cs: &mut Output<'static>) -> Sector {
    let reply = command_inner(spi, cs, &frame(0x51, 0), false).await;
    if reply.wedged {
        return Sector::Wedged;
    }
    if reply.rejected {
        finish(spi, cs).await;
        return Sector::Dma;
    }
    let Some(0x00) = reply.r1 else {
        finish(spi, cs).await;
        return Sector::Command {
            sample: reply.sample,
        };
    };

    // The data token can arrive inside the command window. Bytes after it are
    // already the start of the sector, so the later transfer must not re-clock them.
    let mut data = [0xFFu8; 514];
    let mut filled = 0usize;
    let mut token = false;
    let mut past_r1 = false;
    if let Some(tail) = token_tail(&reply.sample, &mut past_r1) {
        data[..tail.len()].copy_from_slice(tail);
        filled = tail.len();
        token = true;
    }
    let mut last = reply.sample;
    let mut clocks = 0usize;
    while !token && clocks < 2048 {
        let tx = [0xFFu8; 16];
        let mut chunk = [0u8; 16];
        match exchange(spi, &mut chunk, &tx).await {
            Xfer::Done => {}
            Xfer::Rejected => {
                finish(spi, cs).await;
                return Sector::Dma;
            }
            Xfer::Wedged => return Sector::Wedged,
        }
        clocks += chunk.len();
        last.copy_from_slice(&chunk[..8]);
        if let Some(tail) = token_tail(&chunk, &mut past_r1) {
            let n = tail.len().min(data.len());
            data[..n].copy_from_slice(&tail[..n]);
            filled = n;
            token = true;
        }
    }
    if !token {
        finish(spi, cs).await;
        return Sector::NoToken {
            r1: 0x00,
            sample: last,
        };
    }

    let need = 514 - filled;
    let started = Instant::now();
    if need > 0 {
        let tx = [0xFFu8; 514];
        match exchange(spi, &mut data[filled..], &tx[..need]).await {
            Xfer::Done => {}
            Xfer::Rejected => {
                finish(spi, cs).await;
                return Sector::Dma;
            }
            Xfer::Wedged => return Sector::Wedged,
        }
    }
    let measured = started.elapsed().as_micros();
    let micros = if need == 0 {
        measured
    } else {
        measured.saturating_mul(514) / need as u64
    };
    finish(spi, cs).await;
    let mut head = [0u8; 16];
    head.copy_from_slice(&data[..16]);
    let mut end = [0u8; 8];
    end.copy_from_slice(&data[504..512]);
    let mut mark = None;
    let mut index = 0;
    while index + 1 < 512 {
        if data[index] == 0x55 && data[index + 1] == 0xAA {
            mark = Some(index as u16);
            break;
        }
        index += 1;
    }
    Sector::Data {
        micros,
        head,
        end,
        mark,
    }
}

fn token_tail<'a>(window: &'a [u8], past_r1: &mut bool) -> Option<&'a [u8]> {
    for (index, &byte) in window.iter().enumerate() {
        if !*past_r1 {
            if byte != 0xFF {
                *past_r1 = true;
            }
            continue;
        }
        if byte == 0xFE {
            return Some(&window[index + 1..]);
        }
    }
    None
}

struct Reply {
    sample: [u8; 8],
    lead: [u8; 4],
    r1: Option<u8>,
    rejected: bool,
    wedged: bool,
}

async fn command(spi: &mut Spim<'static>, cs: &mut Output<'static>, cmd: &[u8; 6]) -> Reply {
    command_inner(spi, cs, cmd, true).await
}

async fn command_inner(
    spi: &mut Spim<'static>,
    cs: &mut Output<'static>,
    cmd: &[u8; 6],
    release: bool,
) -> Reply {
    // One transfer: 16 clocks for a busy line, the command, then 8 response clocks.
    // Chip-select has already been low for a few microseconds, with no clock yet.
    // Both buffers stay in RAM. A flash-resident transmit slice is copied by the
    // driver into a 512-byte scratch, which panics once the sector is longer.
    let mut tx = [0xFFu8; 30];
    tx[16..22].copy_from_slice(cmd);
    let mut rx = [0u8; 30];
    cs.set_low();
    cortex_m::asm::delay(2_000);
    let transferred = exchange(spi, &mut rx, &tx).await;
    if release {
        finish(spi, cs).await;
    }
    if transferred != Xfer::Done {
        return Reply {
            sample: [0; 8],
            lead: [0; 4],
            r1: None,
            rejected: transferred == Xfer::Rejected,
            wedged: transferred == Xfer::Wedged,
        };
    }
    let mut lead = [0u8; 4];
    lead.copy_from_slice(&rx[..4]);
    let mut sample = [0u8; 8];
    sample.copy_from_slice(&rx[22..]);
    Reply {
        sample,
        lead,
        r1: sample.into_iter().find(|byte| *byte != 0xFF),
        rejected: false,
        wedged: false,
    }
}

async fn preamble(spi: &mut Spim<'static>, cs: &mut Output<'static>) -> bool {
    cs.set_high();
    // Sixteen bytes at 125 kHz is just over a millisecond of clocks.
    let clocks = [0xFFu8; 16];
    let mut idle = [0u8; 16];
    match exchange(spi, &mut idle, &clocks).await {
        Xfer::Done => {}
        Xfer::Rejected => {
            note(format_args!("sd-spi: idle clocks rejected"));
            return false;
        }
        Xfer::Wedged => {
            note(format_args!("sd-spi: transfer wedged"));
            return false;
        }
    }
    Timer::after_millis(1).await;
    true
}

async fn finish(spi: &mut Spim<'static>, cs: &mut Output<'static>) {
    cs.set_high();
    let tx = [0xFFu8; 2];
    let mut ignored = [0u8; 2];
    let _ = exchange(spi, &mut ignored, &tx).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Xfer {
    Done,
    Rejected,
    Wedged,
}

async fn exchange(spi: &mut Spim<'static>, rx: &mut [u8], tx: &[u8]) -> Xfer {
    match select(spi.transfer(rx, tx), Timer::after_millis(500)).await {
        Either::First(Ok(())) => Xfer::Done,
        Either::First(Err(_)) => Xfer::Rejected,
        Either::Second(_) => {
            stop_spim();
            Xfer::Wedged
        }
    }
}

fn stop_spim() {
    let spi = pac::SPIM2;
    spi.events_stopped().write_value(0);
    spi.tasks_stop().write_value(1);
    let mut spins = 0u32;
    while spi.events_stopped().read() == 0 && spins < 100_000 {
        spins += 1;
        cortex_m::asm::nop();
    }
    spi.events_end().write_value(0);
    spi.events_stopped().write_value(0);
    spi.intenclr().write(|w| w.set_end(true));
}

fn set_frequency(spi: &mut Spim<'static>, frequency: spim::Frequency) -> bool {
    let mut config = spim::Config::default();
    config.frequency = frequency;
    config.mode = spim::MODE_0;
    config.orc = 0xFF;
    config.sck_drive = OutputDrive::HighDrive;
    config.mosi_drive = OutputDrive::HighDrive;
    spi.set_config(&config).is_ok()
}

fn frame(index: u8, arg: u32) -> [u8; 6] {
    let mut body = [0u8; 6];
    body[0] = index;
    body[1] = (arg >> 24) as u8;
    body[2] = (arg >> 16) as u8;
    body[3] = (arg >> 8) as u8;
    body[4] = arg as u8;
    body[5] = crc7(&body[..5]);
    body
}

const _: () = assert!(crc7(&[0x40, 0, 0, 0, 0]) == 0x95);
const _: () = assert!(crc7(&[0x48, 0, 0, 0x01, 0xAA]) == 0x87);

const fn crc7(message: &[u8]) -> u8 {
    let mut crc = 0u8;
    let mut index = 0;
    while index < message.len() {
        crc ^= message[index];
        let mut bit = 0;
        while bit < 8 {
            if crc & 0x80 != 0 {
                crc = crc.wrapping_shl(1) ^ 0x12;
            } else {
                crc = crc.wrapping_shl(1);
            }
            bit += 1;
        }
        index += 1;
    }
    crc | 1
}

fn reply_label(sample: &[u8], r1: Option<u8>) -> &'static str {
    if sample.iter().all(|byte| *byte == 0) {
        "stuck"
    } else {
        r1_text(r1)
    }
}

struct Lead<'a>(&'a [u8]);

impl core::fmt::Display for Lead<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.iter().all(|byte| *byte == 0xFF) {
            return Ok(());
        }
        write!(formatter, " lead {}", Hex(self.0))
    }
}

fn r1_text(r1: Option<u8>) -> &'static str {
    match r1 {
        Some(0x00) => "ready",
        Some(0x01) => "idle",
        Some(_) => "other",
        None => "none",
    }
}

fn freq_label(frequency: spim::Frequency) -> &'static str {
    match frequency {
        spim::Frequency::K125 => "125 kHz",
        spim::Frequency::K500 => "500 kHz",
        spim::Frequency::M1 => "1 MHz",
        spim::Frequency::M4 => "4 MHz",
        spim::Frequency::M8 => "8 MHz",
        _ => "other",
    }
}

struct Hex<'a>(&'a [u8]);

impl core::fmt::Display for Hex<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (index, byte) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str(" ")?;
            }
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

fn note(args: core::fmt::Arguments<'_>) {
    let mut line = heapless::String::<160>::new();
    let _ = write!(line, "{args}\r\n");
    LOG.lock(|log| log.borrow_mut().push(line.as_bytes()));
}

fn say(line: &str) {
    note(format_args!("{line}"));
}

fn configure_spim_pins() {
    use embassy_nrf::pac::gpio::vals::{Dir, Drive, Input, Pull};

    // SCK stays an output whose input buffer is connected. MOSI idles low.
    // MISO is an input with the pull-up the card needs while it is deselected.
    pac::P0.pin_cnf(6).write(|w| {
        w.set_dir(Dir::OUTPUT);
        w.set_input(Input::CONNECT);
        w.set_pull(Pull::DISABLED);
        w.set_drive(Drive::H0H1);
    });
    pac::P0.outclr().write(|w| w.set_pin(6, true));
    pac::P1.pin_cnf(1).write(|w| {
        w.set_dir(Dir::OUTPUT);
        w.set_input(Input::DISCONNECT);
        w.set_pull(Pull::DISABLED);
        w.set_drive(Drive::H0H1);
    });
    pac::P1.outclr().write(|w| w.set_pin(1, true));
    pac::P0.pin_cnf(26).write(|w| {
        w.set_dir(Dir::INPUT);
        w.set_input(Input::CONNECT);
        w.set_pull(Pull::PULLUP);
    });
}

fn bitbang_cmd0(cs: &mut Output<'_>) {
    use embassy_nrf::pac::gpio::vals::{Dir, Input, Pull};

    // Same edges as the production bit-bang, on the same pins, before SPIM is armed.
    pac::P0.pin_cnf(6).write(|w| {
        w.set_dir(Dir::OUTPUT);
        w.set_input(Input::DISCONNECT);
        w.set_pull(Pull::DISABLED);
    });
    pac::P0.outclr().write(|w| w.set_pin(6, true));
    pac::P1.pin_cnf(1).write(|w| {
        w.set_dir(Dir::OUTPUT);
        w.set_input(Input::DISCONNECT);
        w.set_pull(Pull::DISABLED);
    });
    pac::P1.outset().write(|w| w.set_pin(1, true));
    pac::P0.pin_cnf(26).write(|w| {
        w.set_dir(Dir::INPUT);
        w.set_input(Input::CONNECT);
        w.set_pull(Pull::PULLUP);
    });

    cs.set_high();
    for _ in 0..10 {
        bb_byte(0xFF);
    }
    cs.set_low();
    cortex_m::asm::delay(2_000);
    bb_byte(0xFF);
    for byte in CMD0 {
        bb_byte(byte);
    }
    let mut sample = [0u8; 8];
    for slot in &mut sample {
        *slot = bb_byte(0xFF);
    }
    cs.set_high();
    bb_byte(0xFF);
    let r1 = sample.into_iter().find(|byte| *byte != 0xFF);
    note(format_args!(
        "sd-spi: bitbang cmd0 {} r1 {}",
        Hex(&sample),
        reply_label(&sample, r1)
    ));
}

fn bb_byte(out: u8) -> u8 {
    let mut input = 0u8;
    for bit in (0..8).rev() {
        if out & (1 << bit) == 0 {
            pac::P1.outclr().write(|w| w.set_pin(1, true));
        } else {
            pac::P1.outset().write(|w| w.set_pin(1, true));
        }
        cortex_m::asm::delay(320);
        pac::P0.outset().write(|w| w.set_pin(6, true));
        cortex_m::asm::delay(320);
        input <<= 1;
        if pac::P0.in_().read().pin(26) {
            input |= 1;
        }
        pac::P0.outclr().write(|w| w.set_pin(6, true));
    }
    pac::P1.outset().write(|w| w.set_pin(1, true));
    input
}

fn pet_raw() {
    const P0: usize = 0x5000_0000;
    // SAFETY: P0.09 is the board watchdog. This runs before embassy owns the pin.
    unsafe {
        (P0 as *mut u32).add(0x700 / 4 + 9).write_volatile(0x3);
        (P0 as *mut u32).add(0x508 / 4).write_volatile(1 << 9);
        cortex_m::asm::delay(64_000);
        (P0 as *mut u32).add(0x50C / 4).write_volatile(1 << 9);
    }
}

async fn pet(watchdog: &mut Output<'_>) {
    watchdog.set_high();
    Timer::after_millis(1).await;
    watchdog.set_low();
}

// This image does not link the SoftDevice, which is what normally provides these.
#[no_mangle]
unsafe fn _critical_section_1_0_acquire() -> bool {
    let nested = !cortex_m::register::primask::read().is_active();
    cortex_m::interrupt::disable();
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    nested
}

#[no_mangle]
unsafe fn _critical_section_1_0_release(nested: bool) {
    core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    if !nested {
        // SAFETY: acquire masked interrupts, and only the outermost release unmasks them.
        unsafe { cortex_m::interrupt::enable() };
    }
}
