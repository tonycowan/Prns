//! Copy `/firmware.bin` from the card into the application flash.
#![cfg_attr(feature = "image-battery-sense", allow(dead_code))]
//!
//! The copier runs from RAM. It leaves the SoftDevice, the saved identity pages, and the UF2
//! bootloader in place. A failed copy sits in a watchdog loop so a double-tap can reach UF2.

use core::ptr;

pub(crate) const MAX_FIRMWARE_CLUSTERS: usize = 64;

pub(crate) struct FirmwareImage {
    pub(crate) clusters: [u32; MAX_FIRMWARE_CLUSTERS],
    pub(crate) cluster_count: u32,
    pub(crate) sectors_per_cluster: u32,
    pub(crate) heap_lba: u32,
    pub(crate) bytes: u32,
}

const APP_BASE: u32 = 0x26000;
const APP_END: u32 = 0xE2000;
const FLASH_PAGE: u32 = 4096;
const BLOCK: usize = 512;

pub(crate) fn commit(image: &FirmwareImage) -> Result<(), ()> {
    if image.bytes < 8
        || image.bytes > APP_END - APP_BASE
        || image.cluster_count == 0
        || image.sectors_per_cluster == 0
    {
        return Err(());
    }
    // SoftDevice disable is an SVC and must run from thread mode before the RAM copier erases
    // the application. The SVC stub lives in the SoftDevice, which this update does not touch.
    let disabled = unsafe { nrf_softdevice::raw::sd_softdevice_disable() };
    if disabled != 0 {
        return Err(());
    }
    cortex_m::interrupt::disable();
    let ram: unsafe fn(&FirmwareImage) -> ! = ram_commit;
    unsafe { ram(image) }
}

#[link_section = ".data.firmware_update"]
#[inline(never)]
unsafe fn ram_commit(image: &FirmwareImage) -> ! {
    configure_pins();
    pet_watchdog();
    // SoftDevice shutdown resets the SD pins. A card left mid-command will not answer CMD17
    // until it is idled and started again. Fail that before erasing any application page.
    if image.sectors_per_cluster == 0 || !wake_card() {
        system_reset();
    }
    let sectors = image.bytes.div_ceil(BLOCK as u32);
    let chunk = unsafe { &mut *ptr::addr_of_mut!(STAGING) };
    let mut sector_index = 0u32;
    while sector_index < sectors {
        let address = APP_BASE + sector_index * BLOCK as u32;
        if address < APP_BASE || address.saturating_add(BLOCK as u32) > APP_END {
            halt();
        }
        fill_chunk(chunk);
        let Some(lba) = file_lba(image, sector_index) else {
            halt();
        };
        if !read_block(lba, &mut chunk.0) {
            if sector_index == 0 {
                system_reset();
            }
            halt();
        }
        if sector_index % 8 == 0 && !erase_page(address) {
            halt();
        }
        let sector_base = sector_index * BLOCK as u32;
        if sector_base + BLOCK as u32 > image.bytes {
            let mut index = (image.bytes - sector_base) as usize;
            while index < BLOCK {
                chunk.0[index] = 0xFF;
                index += 1;
            }
        }
        if !program_chunk(address, chunk) {
            halt();
        }
        pet_watchdog();
        sector_index += 1;
    }
    system_reset()
}

#[repr(C, align(4))]
struct Chunk([u8; BLOCK]);

static mut STAGING: Chunk = Chunk([0; BLOCK]);

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn fill_chunk(chunk: &mut Chunk) {
    let mut index = 0;
    while index < BLOCK {
        chunk.0[index] = 0xFF;
        index += 1;
    }
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn file_lba(image: &FirmwareImage, sector_index: u32) -> Option<u32> {
    if image.sectors_per_cluster == 0 {
        return None;
    }
    let cluster_index = sector_index / image.sectors_per_cluster;
    let within = sector_index % image.sectors_per_cluster;
    let cluster = *image.clusters.get(cluster_index as usize)?;
    if cluster_index >= image.cluster_count || cluster < 2 {
        return None;
    }
    image
        .heap_lba
        .checked_add((cluster - 2).checked_mul(image.sectors_per_cluster)?)?
        .checked_add(within)
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn delay_cycles(cycles: u32) {
    let left = cycles;
    unsafe {
        core::arch::asm!(
            "1:",
            "subs {left}, {left}, #1",
            "bne 1b",
            left = inout(reg) left => _,
            options(nomem, nostack),
        );
    }
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn configure_pins() {
    // P0.06 clock, P0.26 data in, P1.01 data out, P1.00 select, P0.09 watchdog.
    pin_cnf(0x5000_0000, 6, 0x3);
    pin_cnf(0x5000_0000, 9, 0x3);
    pin_cnf(0x5000_0000, 26, 0xC);
    pin_cnf(0x5000_0300, 0, 0x3);
    pin_cnf(0x5000_0300, 1, 0x3);
    out_clr(0x5000_0000, 6);
    out_set(0x5000_0000, 9);
    out_set(0x5000_0300, 0);
    out_set(0x5000_0300, 1);
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn pin_cnf(port: u32, pin: u32, value: u32) {
    unsafe {
        ptr::write_volatile((port + 0x700 + pin * 4) as *mut u32, value);
    }
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn out_set(port: u32, pin: u32) {
    unsafe {
        ptr::write_volatile((port + 0x508) as *mut u32, 1 << pin);
    }
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn out_clr(port: u32, pin: u32) {
    unsafe {
        ptr::write_volatile((port + 0x50C) as *mut u32, 1 << pin);
    }
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn miso_high() -> bool {
    unsafe { ptr::read_volatile(0x5000_0510u32 as *const u32) & (1 << 26) != 0 }
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn pet_watchdog() {
    out_set(0x5000_0000, 9);
    delay_cycles(64_000);
    out_clr(0x5000_0000, 9);
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn transfer(out: u8) -> u8 {
    let mut input = 0u8;
    let mut bit = 8;
    while bit > 0 {
        bit -= 1;
        if out & (1 << bit) == 0 {
            out_clr(0x5000_0300, 1);
        } else {
            out_set(0x5000_0300, 1);
        }
        delay_cycles(320);
        out_set(0x5000_0000, 6);
        delay_cycles(320);
        input <<= 1;
        if miso_high() {
            input |= 1;
        }
        out_clr(0x5000_0000, 6);
    }
    out_set(0x5000_0300, 1);
    input
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn sync_card() {
    out_set(0x5000_0300, 0);
    let mut clocks = 0;
    while clocks < 10 {
        let _ = transfer(0xFF);
        clocks += 1;
    }
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
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
const _: () = assert!(crc7(&[0x48, 0, 0, 1, 0xAA]) == 0x87);

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn deselect() {
    out_set(0x5000_0300, 0);
    let _ = transfer(0xFF);
}

/// Send one SPI command and return R1. Chip select stays low so the caller can read a response tail.
#[inline(never)]
#[link_section = ".data.firmware_update"]
fn command_r1(index: u8, arg: u32) -> Option<u8> {
    out_set(0x5000_0300, 0);
    let _ = transfer(0xFF);
    out_clr(0x5000_0300, 0);
    let mut frame = [0u8; 6];
    frame[0] = 0x40 | index;
    frame[1] = (arg >> 24) as u8;
    frame[2] = (arg >> 16) as u8;
    frame[3] = (arg >> 8) as u8;
    frame[4] = arg as u8;
    frame[5] = crc7(&frame[..5]);
    let mut sent = 0;
    while sent < 6 {
        let _ = transfer(frame[sent]);
        sent += 1;
    }
    let mut tries = 0;
    while tries < 16 {
        let status = transfer(0xFF);
        if status != 0xFF {
            return Some(status);
        }
        tries += 1;
    }
    None
}

/// CMD0, CMD8, ACMD41, and CMD58. Requires the block-addressed v2 card this board already mounted.
#[inline(never)]
#[link_section = ".data.firmware_update"]
fn wake_card() -> bool {
    sync_card();
    let mut idle_try = 0;
    let mut idle = false;
    while idle_try < 32 {
        pet_watchdog();
        idle = command_r1(0, 0) == Some(0x01);
        deselect();
        if idle {
            break;
        }
        idle_try += 1;
    }
    if !idle {
        return false;
    }
    let version = command_r1(8, 0x1AA);
    let echo0 = transfer(0xFF);
    let echo1 = transfer(0xFF);
    let echo2 = transfer(0xFF);
    let echo3 = transfer(0xFF);
    deselect();
    if version != Some(0x01) || echo0 != 0 || echo1 != 0 || echo2 != 0x01 || echo3 != 0xAA {
        return false;
    }
    let mut ready_try = 0;
    let mut ready = false;
    while ready_try < 200 {
        pet_watchdog();
        let app = command_r1(55, 0);
        deselect();
        if !matches!(app, Some(status) if status & !0x01 == 0) {
            return false;
        }
        let status = command_r1(41, 0x4000_0000);
        deselect();
        if status == Some(0) {
            ready = true;
            break;
        }
        if status != Some(0x01) {
            return false;
        }
        let mut pause = 0;
        while pause < 10 {
            pet_watchdog();
            pause += 1;
        }
        ready_try += 1;
    }
    if !ready {
        return false;
    }
    let status = command_r1(58, 0);
    let ocr = transfer(0xFF);
    let _ = transfer(0xFF);
    let _ = transfer(0xFF);
    let _ = transfer(0xFF);
    deselect();
    status == Some(0) && ocr & 0x40 != 0
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn read_block(lba: u32, dest: &mut [u8]) -> bool {
    let mut attempt = 0;
    while attempt < 4 {
        pet_watchdog();
        if command_r1(17, lba) == Some(0) {
            let mut wait = 0;
            let mut token = 0xFFu8;
            while wait < 250_000 {
                if wait & 1023 == 0 {
                    pet_watchdog();
                }
                token = transfer(0xFF);
                if token != 0xFF {
                    break;
                }
                wait += 1;
            }
            if token == 0xFE {
                let mut index = 0;
                while index < dest.len() {
                    dest[index] = transfer(0xFF);
                    index += 1;
                }
                let _ = transfer(0xFF);
                let _ = transfer(0xFF);
                out_set(0x5000_0300, 0);
                let _ = transfer(0xFF);
                return true;
            }
        }
        out_set(0x5000_0300, 0);
        let _ = transfer(0xFF);
        attempt += 1;
    }
    false
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn nvmc_wait() {
    while unsafe { ptr::read_volatile(0x4001_E400u32 as *const u32) } == 0 {}
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn nvmc_config(value: u32) {
    unsafe { ptr::write_volatile(0x4001_E504u32 as *mut u32, value) };
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn erase_page(address: u32) -> bool {
    if address & (FLASH_PAGE - 1) != 0 {
        return false;
    }
    nvmc_wait();
    nvmc_config(2);
    unsafe { ptr::write_volatile(0x4001_E508u32 as *mut u32, address) };
    nvmc_wait();
    nvmc_config(0);
    true
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn program_chunk(address: u32, chunk: &Chunk) -> bool {
    nvmc_wait();
    nvmc_config(1);
    let dest = address as *mut u32;
    let src = chunk.0.as_ptr() as *const u32;
    let mut word = 0;
    while word < BLOCK / 4 {
        unsafe {
            ptr::write_volatile(dest.add(word), ptr::read_volatile(src.add(word)));
        }
        nvmc_wait();
        word += 1;
    }
    nvmc_config(0);
    nvmc_wait();
    word = 0;
    while word < BLOCK / 4 {
        let stored = unsafe { ptr::read_volatile(dest.add(word)) };
        let expected = unsafe { ptr::read_volatile(src.add(word)) };
        if stored != expected {
            return false;
        }
        word += 1;
    }
    true
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn system_reset() -> ! {
    unsafe {
        ptr::write_volatile(0xE000_ED0Cu32 as *mut u32, 0x05FA_0004);
    }
    loop {
        core::hint::spin_loop();
    }
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn halt() -> ! {
    loop {
        pet_watchdog();
    }
}
