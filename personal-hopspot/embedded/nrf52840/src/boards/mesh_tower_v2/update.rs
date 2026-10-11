//! Copy `/firmware.bin` from the card into the application flash.
#![cfg_attr(feature = "image-battery-sense", allow(dead_code))]
//!
//! The copy loop is linked in flash, then copied into the unused runtime-stack reservation and
//! run from there. Static RAM cannot hold it. The SoftDevice is shut down first, so the flash
//! controller can be programmed directly and the radio does not have to stay up for progress.
//! Application interrupts are masked so the image being replaced cannot run. The saved identity
//! pages and the UF2 bootloader stay in place. A failed copy sits in a watchdog loop so a
//! double-tap can reach UF2.

use core::mem::MaybeUninit;
use core::ptr;
use core::sync::atomic::{compiler_fence, Ordering};

pub(crate) const MAX_FIRMWARE_CLUSTERS: usize = 64;

pub(crate) struct FirmwareImage {
    pub(crate) clusters: [u32; MAX_FIRMWARE_CLUSTERS],
    /// First cluster of a `NO_FAT_CHAIN` run. Zero means [`Self::clusters`] lists every cluster.
    pub(crate) contiguous_first: u32,
    pub(crate) cluster_count: u32,
    pub(crate) sectors_per_cluster: u32,
    pub(crate) heap_lba: u32,
    pub(crate) bytes: u32,
}

const APP_BASE: u32 = 0x26000;
const APP_END: u32 = 0xE2000;
const FLASH_PAGE: u32 = 4096;
const BLOCK: usize = 512;
/// Bottom of the unused runtime-stack reservation. The copy loop is placed here, above static RAM.
const COPIER_RAM: usize = 0x2002_F000;
/// The loop must end before the BLE event scratch that follows it.
const COPIER_RAM_LIMIT: usize = 0x2003_2000;

struct CopyHooks {
    pet: fn(),
    configure: fn(),
    wake: fn() -> bool,
    stage: fn(&FirmwareImage, u32) -> bool,
    delay: fn(u32),
    halt: fn() -> !,
}

#[link_section = ".data.firmware_update"]
static HOOKS: CopyHooks = CopyHooks {
    pet: pet_watchdog,
    configure: configure_pins,
    wake: wake_card,
    stage: stage_sector,
    delay: delay_cycles,
    halt: halt,
};

unsafe extern "C" {
    static _stack_end: u8;
    static __firmware_copier_start: u8;
    static __firmware_copier_end: u8;
}

pub(crate) fn commit(image: &FirmwareImage) -> Result<(), ()> {
    if image.bytes < 8
        || image.bytes > APP_END - APP_BASE
        || image.cluster_count == 0
        || image.sectors_per_cluster == 0
        || (image.contiguous_first < 2 && image.cluster_count > MAX_FIRMWARE_CLUSTERS as u32)
    {
        return Err(());
    }
    // The SoftDevice owns the flash controller until this returns. The copy then
    // uses the hardware controller and does not send progress.
    if unsafe { nrf_softdevice::raw::sd_softdevice_disable() } != 0 {
        return Err(());
    }
    enter_copier(image)
}

fn enter_copier(image: &FirmwareImage) -> ! {
    unsafe {
        let stack_end = ptr::addr_of!(_stack_end) as usize;
        let start = ptr::addr_of!(__firmware_copier_start);
        let end = ptr::addr_of!(__firmware_copier_end);
        let len = (end as usize).wrapping_sub(start as usize);
        if stack_end > COPIER_RAM || len == 0 || len > COPIER_RAM_LIMIT - COPIER_RAM {
            system_reset();
        }
        ptr::copy_nonoverlapping(start, COPIER_RAM as *mut u8, len);
        core::arch::asm!("dsb", "isb", options(nostack));
        let flash_entry = ram_commit as *const () as usize & !1;
        let offset = flash_entry.wrapping_sub(start as usize);
        if offset >= len {
            system_reset();
        }
        let entry = (COPIER_RAM + offset) | 1;
        let ram: unsafe fn(&FirmwareImage) -> ! = core::mem::transmute(entry);
        ram(image)
    }
}

#[link_section = ".firmware_copier"]
#[inline(never)]
unsafe fn ram_commit(image: &FirmwareImage) -> ! {
    // The SoftDevice is already off. Mask every interrupt so handlers in the
    // application flash cannot run while those pages are erased.
    unsafe {
        core::arch::asm!("cpsid i", options(nomem, nostack, preserves_flags));
        // Drop cached instructions from the pages about to be rewritten.
        ptr::write_volatile(0x4001_E540u32 as *mut u32, 0);
    }
    (HOOKS.configure)();
    (HOOKS.pet)();
    if image.sectors_per_cluster == 0 || !(HOOKS.wake)() {
        system_reset();
    }
    let pages = image.bytes.div_ceil(FLASH_PAGE);
    let mut page_index = 0u32;
    while page_index < pages {
        let address = APP_BASE + page_index * FLASH_PAGE;
        if address < APP_BASE || address.saturating_add(FLASH_PAGE) > APP_END {
            (HOOKS.halt)();
        }
        // Read the first sector before erasing. A failure there, on the first page, can still
        // reset into the firmware that is running.
        if !(HOOKS.stage)(image, page_index * (FLASH_PAGE / BLOCK as u32)) {
            if page_index == 0 {
                system_reset();
            }
            (HOOKS.halt)();
        }
        if !flash_erase(address / FLASH_PAGE) {
            if page_index == 0 {
                system_reset();
            }
            (HOOKS.halt)();
        }
        let chunk = unsafe { &*ptr::addr_of!(STAGING) };
        if !flash_program(address, chunk) {
            (HOOKS.halt)();
        }
        let mut sector = 1u32;
        while sector < FLASH_PAGE / BLOCK as u32 {
            let sector_index = page_index * (FLASH_PAGE / BLOCK as u32) + sector;
            if sector_index * BLOCK as u32 >= image.bytes {
                break;
            }
            if !(HOOKS.stage)(image, sector_index) {
                (HOOKS.halt)();
            }
            let chunk = unsafe { &*ptr::addr_of!(STAGING) };
            if !flash_program(address + sector * BLOCK as u32, chunk) {
                (HOOKS.halt)();
            }
            sector += 1;
        }
        (HOOKS.pet)();
        page_index += 1;
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
    if cluster_index >= image.cluster_count {
        return None;
    }
    let cluster = if image.contiguous_first >= 2 {
        image.contiguous_first.checked_add(cluster_index)?
    } else {
        *image.clusters.get(cluster_index as usize)?
    };
    if cluster < 2 {
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
    // SCK input buffer stays connected and SCK/MOSI use high drive. That is the
    // pin setup that completed a sector read at 8 MHz on this socket.
    pin_cnf(0x5000_0000, 6, 0x301);
    pin_cnf(0x5000_0000, 9, 0x3);
    pin_cnf(0x5000_0000, 26, 0xC);
    pin_cnf(0x5000_0300, 0, 0x303);
    pin_cnf(0x5000_0300, 1, 0x303);
    out_clr(0x5000_0000, 6);
    out_set(0x5000_0000, 9);
    out_set(0x5000_0300, 0);
    out_clr(0x5000_0300, 1);
    arm_spim();
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
fn arm_spim() {
    // SPIM2. Stay enabled for the copy. Disconnecting it between commands made
    // the next transfer hang on this board.
    spim_reg(0x508, 6);
    spim_reg(0x50C, 33);
    spim_reg(0x510, 26);
    spim_reg(0x554, 0);
    spim_reg(0x5C0, 0xFF);
    spim_reg(0x524, 0x0200_0000);
    spim_reg(0x500, 7);
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn spim_reg(offset: u32, value: u32) {
    unsafe {
        ptr::write_volatile((0x4002_3000 + offset) as *mut u32, value);
    }
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn spim_exchange(tx: *const u8, rx: *mut u8, len: u32) -> bool {
    compiler_fence(Ordering::SeqCst);
    spim_reg(0x534, rx as u32);
    spim_reg(0x538, len);
    spim_reg(0x544, tx as u32);
    spim_reg(0x548, len);
    spim_reg(0x118, 0);
    spim_reg(0x010, 1);
    let mut spins = 0u32;
    while unsafe { ptr::read_volatile((0x4002_3000 + 0x118) as *const u32) } == 0 {
        spins += 1;
        if spins & 0xFFFF == 0 {
            pet_watchdog();
        }
        if spins > 2_000_000 {
            spim_reg(0x104, 0);
            spim_reg(0x014, 1);
            return false;
        }
    }
    compiler_fence(Ordering::SeqCst);
    true
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn read_payload(dest: &mut [u8]) -> bool {
    if dest.len() > BLOCK {
        return false;
    }
    let mut tx = MaybeUninit::<[u8; BLOCK]>::uninit();
    let tx_ptr = tx.as_mut_ptr() as *mut u8;
    let mut index = 0;
    while index < dest.len() {
        unsafe {
            tx_ptr.add(index).write(0xFF);
        }
        index += 1;
    }
    if !spim_exchange(tx_ptr, dest.as_mut_ptr(), dest.len() as u32) {
        return false;
    }
    let crc_tx = [0xFFu8; 2];
    let mut crc_rx = [0u8; 2];
    spim_exchange(crc_tx.as_ptr(), crc_rx.as_mut_ptr(), 2)
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
    let mut tx = [0u8; 1];
    tx[0] = out;
    let mut rx = [0u8; 1];
    if !spim_exchange(tx.as_ptr(), rx.as_mut_ptr(), 1) {
        return 0xFF;
    }
    rx[0]
}

#[inline(never)]
#[link_section = ".data.firmware_update"]
fn sync_card() {
    out_set(0x5000_0300, 0);
    let mut clocks = 0;
    while clocks < 16 {
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
    delay_cycles(2_000);
    let _ = transfer(0xFF);
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
    if status == Some(0) && ocr & 0x40 != 0 {
        // Same clock the sector-0 read kept through 8 MHz.
        spim_reg(0x524, 0x8000_0000);
        true
    } else {
        false
    }
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
            if token == 0xFE && read_payload(dest) {
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
fn stage_sector(image: &FirmwareImage, sector_index: u32) -> bool {
    let sector_base = sector_index * BLOCK as u32;
    let chunk = unsafe { &mut *ptr::addr_of_mut!(STAGING) };
    fill_chunk(chunk);
    let Some(lba) = file_lba(image, sector_index) else {
        return false;
    };
    if !read_block(lba, &mut chunk.0) {
        return false;
    }
    if sector_base + BLOCK as u32 > image.bytes {
        let mut index = (image.bytes - sector_base) as usize;
        while index < BLOCK {
            chunk.0[index] = 0xFF;
            index += 1;
        }
    }
    true
}

#[inline(never)]
#[link_section = ".firmware_copier"]
fn flash_erase(page: u32) -> bool {
    let address = page.saturating_mul(FLASH_PAGE);
    if address < APP_BASE || address.saturating_add(FLASH_PAGE) > APP_END {
        return false;
    }
    if !nvmc_wait() {
        return false;
    }
    unsafe {
        ptr::write_volatile(0x4001_E504u32 as *mut u32, 2);
        ptr::write_volatile(0x4001_E508u32 as *mut u32, address);
    }
    if !nvmc_wait() {
        return false;
    }
    unsafe {
        ptr::write_volatile(0x4001_E504u32 as *mut u32, 0);
    }
    true
}

#[inline(never)]
#[link_section = ".firmware_copier"]
fn flash_program(address: u32, chunk: &Chunk) -> bool {
    if !nvmc_wait() {
        return false;
    }
    unsafe {
        ptr::write_volatile(0x4001_E504u32 as *mut u32, 1);
    }
    let mut index = 0usize;
    while index < BLOCK {
        let word = u32::from_le_bytes([
            chunk.0[index],
            chunk.0[index + 1],
            chunk.0[index + 2],
            chunk.0[index + 3],
        ]);
        unsafe {
            ptr::write_volatile((address + index as u32) as *mut u32, word);
        }
        if !nvmc_wait() {
            unsafe {
                ptr::write_volatile(0x4001_E504u32 as *mut u32, 0);
            }
            return false;
        }
        index += 4;
    }
    unsafe {
        ptr::write_volatile(0x4001_E504u32 as *mut u32, 0);
    }
    flash_matches(address, chunk)
}

#[inline(never)]
#[link_section = ".firmware_copier"]
fn nvmc_wait() -> bool {
    let mut spins = 0u32;
    while unsafe { ptr::read_volatile(0x4001_E400u32 as *const u32) } == 0 {
        spins = spins.saturating_add(1);
        if spins & 0xFFFF == 0 {
            (HOOKS.pet)();
        }
        if spins > 20_000_000 {
            return false;
        }
    }
    true
}

#[inline(never)]
#[link_section = ".firmware_copier"]
fn flash_matches(address: u32, chunk: &Chunk) -> bool {
    let mut index = 0;
    while index < BLOCK {
        let stored = unsafe { ptr::read_volatile((address + index as u32) as *const u8) };
        let expected = unsafe { *chunk.0.as_ptr().add(index) };
        if stored != expected {
            return false;
        }
        index += 1;
    }
    true
}

#[inline(never)]
#[link_section = ".firmware_copier"]
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
