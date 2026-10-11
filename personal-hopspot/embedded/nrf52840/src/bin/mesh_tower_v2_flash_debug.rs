//! Read the firmware file already on the MeshTower card at 8 MHz, then program
//! the first pages into spare flash. The running image stays at the start of
//! the application region, and this loop yields so the USB log keeps moving.
//!
//! Build from `personal-hopspot/embedded/nrf52840`:
//!
//! ```text
//! cargo build --release --locked --no-default-features --bin mesh-tower-v2-flash-debug \
//!   --features board-mesh-tower-v2,softdevice-s140-v6 --target thumbv7em-none-eabihf
//! ```

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
const BLOCK: usize = 512;
const PAGE: usize = 4096;
/// Spare application flash. The test image is linked at 0x26000 and stays well below this.
const SCRATCH: u32 = 0xC0000;
const SCRATCH_END: u32 = 0xE0000;
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
    pet_raw();
    let mut nrf_config = embassy_nrf::config::Config::default();
    nrf_config.hfclk_source = HfclkSource::ExternalXtal;
    let p = embassy_nrf::init(nrf_config);

    let mut watchdog = Output::new(p.P0_09, Level::Low, OutputDrive::Standard);
    pet(&mut watchdog).await;
    let _gps_off = Output::new(p.P0_07, Level::High, OutputDrive::Standard);
    let _radio_reset = Output::new(p.P0_25, Level::Low, OutputDrive::Standard);
    let _fem_off = Output::new(p.P0_15, Level::Low, OutputDrive::Standard);
    let mut led = Output::new(p.P1_15, Level::Low, OutputDrive::Standard);

    let driver = Driver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));
    let mut config = Config::new(0x1915, 0x520F);
    config.manufacturer = Some("Hopspot");
    config.product = Some("MeshTower flash");
    config.serial_number = Some("flash");
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

    let mut spi_config = spim::Config::default();
    spi_config.frequency = spim::Frequency::K125;
    spi_config.mode = spim::MODE_0;
    spi_config.orc = 0xFF;
    spi_config.sck_drive = OutputDrive::HighDrive;
    spi_config.mosi_drive = OutputDrive::HighDrive;
    let mut cs = Output::new(p.P1_00, Level::High, OutputDrive::HighDrive);
    let mut spi = Spim::new(p.SPI2, Irqs, p.P0_06, p.P0_26, p.P1_01, spi_config);
    configure_pins();

    say("flash: ready, firmware file is expected on the card");
    embassy_futures::join::join(
        usb.run(),
        embassy_futures::join::join(drain(class), run(&mut spi, &mut cs, &mut led, &mut watchdog)),
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
        say("flash: host connected");
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

async fn run(
    spi: &mut Spim<'static>,
    cs: &mut Output<'static>,
    led: &mut Output<'static>,
    watchdog: &mut Output<'static>,
) -> ! {
    led.set_low();
    pet(watchdog).await;
    if !init_card(spi, cs, watchdog).await {
        say("flash: card init failed");
        halt(led, watchdog).await;
    }
    if !set_frequency(spi, spim::Frequency::M8) {
        say("flash: 8 MHz rejected");
        halt(led, watchdog).await;
    }
    say("flash: 8 MHz");
    let Some(volume) = find_volume(spi, cs).await else {
        say("flash: exfat not found");
        halt(led, watchdog).await;
    };
    note(format_args!(
        "flash: root {} spc {}",
        volume.root_cluster, volume.sectors_per_cluster
    ));
    let Some(file) = find_firmware(spi, cs, &volume).await else {
        say("flash: firmware.old and firmware.new are absent");
        halt(led, watchdog).await;
    };
    note(format_args!(
        "flash: using {} {} bytes cluster {}",
        file.name, file.bytes, file.cluster
    ));
    copy_file(spi, cs, watchdog, &volume, &file).await;
    say("flash: done");
    led.set_high();
    halt(led, watchdog).await;
}

async fn halt(led: &mut Output<'static>, watchdog: &mut Output<'static>) -> ! {
    loop {
        led.toggle();
        pet(watchdog).await;
        Timer::after_millis(500).await;
    }
}

struct Volume {
    fat_lba: u32,
    heap_lba: u32,
    sectors_per_cluster: u32,
    root_cluster: u32,
}

struct FirmwareFile {
    name: &'static str,
    cluster: u32,
    bytes: u32,
    contiguous: bool,
}

async fn copy_file(
    spi: &mut Spim<'static>,
    cs: &mut Output<'static>,
    watchdog: &mut Output<'static>,
    volume: &Volume,
    file: &FirmwareFile,
) {
    static PAGE_BUF: StaticCell<[u8; PAGE]> = StaticCell::new();
    let page_buf = PAGE_BUF.init([0; PAGE]);
    let pages = file.bytes.div_ceil(PAGE as u32);
    let mut page_index = 0u32;
    let mut cluster = file.cluster;
    let mut sector_in_cluster = 0u32;
    while page_index < pages {
        pet(watchdog).await;
        let page_bytes = ((file.bytes - page_index * PAGE as u32) as usize).min(PAGE);
        let started = Instant::now();
        let mut filled = 0usize;
        while filled < page_bytes {
            let Some(lba) = volume.lba(cluster, sector_in_cluster) else {
                note(format_args!("flash: lba rejected at page {page_index}"));
                return;
            };
            let take = (page_bytes - filled).min(BLOCK);
            match read_block(spi, cs, lba).await {
                Block::Data(data) => {
                    page_buf[filled..filled + take].copy_from_slice(&data[..take]);
                }
                Block::Fail(why) => {
                    note(format_args!(
                        "flash: read failed page {page_index} lba {lba} {why}"
                    ));
                    return;
                }
            }
            filled += take;
            sector_in_cluster += 1;
            if sector_in_cluster >= volume.sectors_per_cluster {
                sector_in_cluster = 0;
                if file.contiguous {
                    cluster = cluster.saturating_add(1);
                } else {
                    match fat_next(spi, cs, volume, cluster).await {
                        Some(next) => cluster = next,
                        None => {
                            note(format_args!("flash: fat ended at page {page_index}"));
                            return;
                        }
                    }
                }
            }
        }
        let micros = started.elapsed().as_micros();
        if page_index == 0 {
            let sp = u32::from_le_bytes(page_buf[0..4].try_into().unwrap_or([0; 4]));
            let reset = u32::from_le_bytes(page_buf[4..8].try_into().unwrap_or([0; 4]));
            note(format_args!(
                "flash: vector sp {sp:08x} reset {reset:08x} in {micros} us"
            ));
        } else if page_index % 8 == 0 {
            note(format_args!(
                "flash: read page {page_index}/{pages} in {micros} us"
            ));
        }
        let address = SCRATCH + page_index * PAGE as u32;
        if address.saturating_add(PAGE as u32) <= SCRATCH_END {
            note(format_args!("flash: erase {address:08x}"));
            Timer::after_millis(1).await;
            if !erase_page(address) {
                note(format_args!("flash: erase stalled {address:08x}"));
                return;
            }
            note(format_args!("flash: program {address:08x}"));
            Timer::after_millis(1).await;
            if !program_page(address, &page_buf[..page_bytes]) {
                note(format_args!("flash: program mismatch {address:08x}"));
                return;
            }
            note(format_args!("flash: page {page_index} ok"));
        }
        Timer::after_millis(1).await;
        page_index += 1;
    }
    note(format_args!(
        "flash: read {pages} pages, programmed {}",
        pages.min((SCRATCH_END - SCRATCH) / PAGE as u32)
    ));
}

async fn find_firmware(
    spi: &mut Spim<'static>,
    cs: &mut Output<'static>,
    volume: &Volume,
) -> Option<FirmwareFile> {
    let mut old = None;
    let mut new = None;
    let mut cluster = volume.root_cluster;
    let mut guard = 0u32;
    while guard < 4 {
        let mut sector = 0u32;
        while sector < volume.sectors_per_cluster {
            let lba = volume.lba(cluster, sector)?;
            let data = match read_block(spi, cs, lba).await {
                Block::Data(data) => data,
                Block::Fail(why) => {
                    note(format_args!("flash: root read {lba} {why}"));
                    return None;
                }
            };
            if sector == 0 && guard == 0 {
                let mut types = heapless::String::<48>::new();
                for index in 0..16 {
                    let _ = write!(types, "{:02x}", data[index * 32]);
                }
                note(format_args!("flash: dir {types}"));
            }
            let mut index = 0;
            let mut ended = false;
            while index + 32 <= BLOCK {
                if data[index] == 0 {
                    ended = true;
                    break;
                }
                if data[index] == 0x85
                    && index + 96 <= BLOCK
                    && data[index + 32] == 0xC0
                    && data[index + 64] == 0xC1
                {
                    if let Some(found) = describe_file(&data[index..index + 96]) {
                        note(format_args!(
                            "flash: saw {} {} bytes",
                            found.name, found.bytes
                        ));
                        if found.name == "firmware.old" {
                            old = Some(found);
                        } else if found.name == "firmware.new" {
                            new = Some(found);
                        }
                    }
                }
                index += 32;
            }
            if ended {
                return choose(old, new);
            }
            sector += 1;
        }
        match fat_next(spi, cs, volume, cluster).await {
            Some(next) => cluster = next,
            None => break,
        }
        guard += 1;
    }
    choose(old, new)
}

fn choose(old: Option<FirmwareFile>, new: Option<FirmwareFile>) -> Option<FirmwareFile> {
    match (old, new) {
        (Some(old), _) if old.bytes >= 8 => Some(old),
        (_, Some(new)) if new.bytes >= 8 => Some(new),
        (Some(old), _) => Some(old),
        (_, found) => found,
    }
}

fn describe_file(set: &[u8]) -> Option<FirmwareFile> {
    let stream = &set[32..64];
    let name_entry = &set[64..96];
    let declared = stream[3] as usize;
    if declared == 0 || declared > 15 {
        return None;
    }
    let mut name = heapless::String::<16>::new();
    for unit in 0..declared {
        let raw = u16::from_le_bytes([name_entry[2 + unit * 2], name_entry[3 + unit * 2]]);
        if raw > 0x7F {
            return None;
        }
        name.push(fold(raw as u8) as char).ok()?;
    }
    let label = if name == "firmware.old" {
        "firmware.old"
    } else if name == "firmware.new" {
        "firmware.new"
    } else {
        return None;
    };
    let bytes = u32::try_from(u64::from_le_bytes(stream[8..16].try_into().ok()?)).ok()?;
    let cluster = u32::from_le_bytes(stream[20..24].try_into().ok()?);
    if bytes > 0 && cluster < 2 {
        return None;
    }
    Some(FirmwareFile {
        name: label,
        cluster,
        bytes,
        contiguous: stream[1] & 0x02 != 0,
    })
}

fn fold(byte: u8) -> u8 {
    match byte {
        b'A'..=b'Z' => byte + 32,
        other => other,
    }
}

impl Volume {
    fn lba(&self, cluster: u32, sector: u32) -> Option<u32> {
        if cluster < 2 || sector >= self.sectors_per_cluster {
            return None;
        }
        self.heap_lba
            .checked_add((cluster - 2).checked_mul(self.sectors_per_cluster)?)?
            .checked_add(sector)
    }
}

async fn fat_next(
    spi: &mut Spim<'static>,
    cs: &mut Output<'static>,
    volume: &Volume,
    cluster: u32,
) -> Option<u32> {
    let byte = cluster.checked_mul(4)?;
    let lba = volume.fat_lba.checked_add(byte / BLOCK as u32)?;
    let data = match read_block(spi, cs, lba).await {
        Block::Data(data) => data,
        Block::Fail(_) => return None,
    };
    let offset = (byte as usize) % BLOCK;
    let next = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
    if next < 2 || next >= 0xFFFF_FFF8 {
        None
    } else {
        Some(next)
    }
}

async fn find_volume(spi: &mut Spim<'static>, cs: &mut Output<'static>) -> Option<Volume> {
    let sector0 = match read_block(spi, cs, 0).await {
        Block::Data(data) => data,
        Block::Fail(why) => {
            note(format_args!("flash: sector 0 {why}"));
            return None;
        }
    };
    if is_exfat(&sector0) {
        return volume_from(&sector0, 0);
    }
    if sector0[510] != 0x55 || sector0[511] != 0xAA {
        return None;
    }
    let mut protective = false;
    for index in 0..4 {
        let base = 446 + index * 16;
        let kind = sector0[base + 4];
        let lba = u32::from_le_bytes(sector0[base + 8..base + 12].try_into().ok()?);
        if kind == 0 || lba == 0 {
            continue;
        }
        if kind == 0xEE {
            protective = true;
            continue;
        }
        if let Block::Data(boot) = read_block(spi, cs, lba).await {
            if is_exfat(&boot) {
                return volume_from(&boot, lba);
            }
        }
    }
    if protective {
        return find_gpt(spi, cs).await;
    }
    None
}

async fn find_gpt(spi: &mut Spim<'static>, cs: &mut Output<'static>) -> Option<Volume> {
    let header = match read_block(spi, cs, 1).await {
        Block::Data(data) => data,
        Block::Fail(_) => return None,
    };
    if &header[..8] != b"EFI PART" {
        return None;
    }
    let entry_lba = u32::try_from(u64::from_le_bytes(header[72..80].try_into().ok()?)).ok()?;
    let entry_size = u32::from_le_bytes(header[84..88].try_into().ok()?) as usize;
    if !(40..=128).contains(&entry_size) {
        return None;
    }
    let count = (u32::from_le_bytes(header[80..84].try_into().ok()?) as usize).min(32);
    for index in 0..count {
        let byte = index * entry_size;
        let lba = entry_lba.checked_add((byte / BLOCK) as u32)?;
        let offset = byte % BLOCK;
        let Block::Data(entries) = read_block(spi, cs, lba).await else {
            continue;
        };
        if offset + 40 > BLOCK || entries[offset..offset + 16].iter().all(|b| *b == 0) {
            continue;
        }
        let start = u32::try_from(u64::from_le_bytes(
            entries[offset + 32..offset + 40].try_into().ok()?,
        ))
        .ok()?;
        if let Block::Data(boot) = read_block(spi, cs, start).await {
            if is_exfat(&boot) {
                return volume_from(&boot, start);
            }
        }
    }
    None
}

fn is_exfat(block: &[u8]) -> bool {
    block.len() >= 512 && &block[3..11] == b"EXFAT   " && block[510] == 0x55 && block[511] == 0xAA
}

fn volume_from(block: &[u8], volume_lba: u32) -> Option<Volume> {
    if block[108] != 9 || block[109] > 16 {
        return None;
    }
    let sectors_per_cluster = 1u32 << block[109];
    let fat_lba = volume_lba.checked_add(u32::from_le_bytes(block[80..84].try_into().ok()?))?;
    let heap_lba = volume_lba.checked_add(u32::from_le_bytes(block[88..92].try_into().ok()?))?;
    let root_cluster = u32::from_le_bytes(block[96..100].try_into().ok()?);
    if root_cluster < 2 || sectors_per_cluster == 0 {
        return None;
    }
    Some(Volume {
        fat_lba,
        heap_lba,
        sectors_per_cluster,
        root_cluster,
    })
}

enum Block {
    Data([u8; BLOCK]),
    Fail(&'static str),
}

async fn read_block(spi: &mut Spim<'static>, cs: &mut Output<'static>, lba: u32) -> Block {
    let reply = command_inner(spi, cs, &frame(0x51, lba), false).await;
    if reply.wedged {
        return Block::Fail("wedged");
    }
    if reply.rejected {
        finish(spi, cs).await;
        return Block::Fail("dma");
    }
    if reply.r1 != Some(0x00) {
        finish(spi, cs).await;
        return Block::Fail("cmd17");
    }
    let mut data = [0xFFu8; 514];
    let mut filled = 0usize;
    let mut token = false;
    let mut past_r1 = false;
    if let Some(tail) = token_tail(&reply.sample, &mut past_r1) {
        let n = tail.len().min(data.len());
        data[..n].copy_from_slice(&tail[..n]);
        filled = n;
        token = true;
    }
    let mut clocks = 0usize;
    while !token && clocks < 2048 {
        let tx = [0xFFu8; 16];
        let mut chunk = [0u8; 16];
        match exchange(spi, &mut chunk, &tx).await {
            Xfer::Done => {}
            Xfer::Rejected => {
                finish(spi, cs).await;
                return Block::Fail("dma");
            }
            Xfer::Wedged => return Block::Fail("wedged"),
        }
        clocks += chunk.len();
        if let Some(tail) = token_tail(&chunk, &mut past_r1) {
            let n = tail.len().min(data.len());
            data[..n].copy_from_slice(&tail[..n]);
            filled = n;
            token = true;
        }
    }
    if !token {
        finish(spi, cs).await;
        return Block::Fail("no token");
    }
    let need = 514 - filled;
    if need > 0 {
        let tx = [0xFFu8; 514];
        match exchange(spi, &mut data[filled..], &tx[..need]).await {
            Xfer::Done => {}
            Xfer::Rejected => {
                finish(spi, cs).await;
                return Block::Fail("dma");
            }
            Xfer::Wedged => return Block::Fail("wedged"),
        }
    }
    finish(spi, cs).await;
    let mut block = [0u8; BLOCK];
    block.copy_from_slice(&data[..BLOCK]);
    Block::Data(block)
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

fn erase_page(address: u32) -> bool {
    if address < SCRATCH || address.saturating_add(PAGE as u32) > SCRATCH_END {
        return false;
    }
    nvmc_wait();
    unsafe {
        core::ptr::write_volatile(0x4001_E504 as *mut u32, 2);
        core::ptr::write_volatile(0x4001_E508 as *mut u32, address);
    }
    if !nvmc_wait_limited() {
        return false;
    }
    unsafe {
        core::ptr::write_volatile(0x4001_E504 as *mut u32, 0);
    }
    true
}

fn program_page(address: u32, data: &[u8]) -> bool {
    nvmc_wait();
    unsafe {
        core::ptr::write_volatile(0x4001_E504 as *mut u32, 1);
    }
    let mut offset = 0usize;
    while offset + 4 <= data.len() {
        let word = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        unsafe {
            core::ptr::write_volatile((address + offset as u32) as *mut u32, word);
        }
        if !nvmc_wait_limited() {
            unsafe {
                core::ptr::write_volatile(0x4001_E504 as *mut u32, 0);
            }
            return false;
        }
        offset += 4;
    }
    unsafe {
        core::ptr::write_volatile(0x4001_E504 as *mut u32, 0);
    }
    for index in 0..data.len() {
        let stored = unsafe { core::ptr::read_volatile((address + index as u32) as *const u8) };
        if stored != data[index] {
            return false;
        }
    }
    true
}

fn nvmc_wait() {
    while unsafe { core::ptr::read_volatile(0x4001_E400 as *const u32) } == 0 {
        cortex_m::asm::nop();
    }
}

fn nvmc_wait_limited() -> bool {
    let mut spins = 0u32;
    while unsafe { core::ptr::read_volatile(0x4001_E400 as *const u32) } == 0 {
        spins = spins.saturating_add(1);
        if spins & 0xFFFF == 0 {
            pet_raw();
        }
        if spins > 20_000_000 {
            return false;
        }
    }
    true
}

async fn init_card(
    spi: &mut Spim<'static>,
    cs: &mut Output<'static>,
    watchdog: &mut Output<'static>,
) -> bool {
    if !set_frequency(spi, spim::Frequency::K125) {
        return false;
    }
    let mut idle = false;
    for try_index in 1..=8 {
        pet(watchdog).await;
        if !preamble(spi, cs).await {
            return false;
        }
        let cmd0 = command(spi, cs, &CMD0).await;
        note(format_args!(
            "flash: cmd0 try {try_index} {}",
            Hex(&cmd0.sample)
        ));
        if cmd0.wedged {
            return false;
        }
        if cmd0.r1 == Some(0x01) {
            idle = true;
            break;
        }
    }
    if !idle {
        return false;
    }
    let cmd8 = command(spi, cs, &CMD8).await;
    note(format_args!("flash: cmd8 {}", Hex(&cmd8.sample)));
    if cmd8.wedged || cmd8.r1 != Some(0x01) {
        return false;
    }
    let mut tries = 0u32;
    let mut ready = false;
    while tries < 40 {
        tries += 1;
        pet(watchdog).await;
        let cmd55 = command(spi, cs, &frame(0x77, 0)).await;
        let acmd = command(spi, cs, &frame(0x69, 0x4000_0000)).await;
        if cmd55.wedged || acmd.wedged {
            return false;
        }
        if acmd.r1 == Some(0x00) {
            ready = true;
            note(format_args!("flash: acmd41 ready try {tries}"));
            break;
        }
        Timer::after_millis(20).await;
    }
    if !ready {
        return false;
    }
    let cmd58 = command(spi, cs, &frame(0x7A, 0)).await;
    note(format_args!("flash: cmd58 {}", Hex(&cmd58.sample)));
    !cmd58.wedged && cmd58.r1 == Some(0x00)
}

struct Reply {
    sample: [u8; 8],
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
            r1: None,
            rejected: transferred == Xfer::Rejected,
            wedged: transferred == Xfer::Wedged,
        };
    }
    let mut sample = [0u8; 8];
    sample.copy_from_slice(&rx[22..]);
    Reply {
        sample,
        r1: sample.into_iter().find(|byte| *byte != 0xFF),
        rejected: false,
        wedged: false,
    }
}

async fn preamble(spi: &mut Spim<'static>, cs: &mut Output<'static>) -> bool {
    cs.set_high();
    let clocks = [0xFFu8; 16];
    let mut idle = [0u8; 16];
    if exchange(spi, &mut idle, &clocks).await != Xfer::Done {
        return false;
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

const _: () = assert!(crc7(&[0x40, 0, 0, 0, 0]) == 0x95);
const _: () = assert!(crc7(&[0x48, 0, 0, 0x01, 0xAA]) == 0x87);

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

fn configure_pins() {
    use embassy_nrf::pac::gpio::vals::{Dir, Drive, Input, Pull};

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
    // Chip-select stays a GPIO output. A drive-only write would clear that bit.
    pac::P1.pin_cnf(0).write(|w| {
        w.set_dir(Dir::OUTPUT);
        w.set_input(Input::DISCONNECT);
        w.set_pull(Pull::DISABLED);
        w.set_drive(Drive::H0H1);
    });
    pac::P1.outset().write(|w| w.set_pin(0, true));
}

fn pet_raw() {
    const P0: usize = 0x5000_0000;
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
        unsafe { cortex_m::interrupt::enable() };
    }
}
