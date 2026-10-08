//! Side stack for sealing and opening Wi-Fi configuration records.
//!
//! Core 0's main stack is the embassy poll stack. Opening one record adds the
//! `decode_record` frame (0x6d0) and the `token_open` frame (0x590) on top of
//! a poll that already sits on the stack guard. Those two frames run here, in
//! the external PSRAM heap, which [`crate::storage::reinit_private_psram_heap`]
//! does not reset.

use core::alloc::Layout;
use core::sync::atomic::{AtomicPtr, Ordering};

use allocator_api2::alloc::Allocator;

use personal_hopspot_core::set_wifi_record_runner;

const RECORD_STACK_BYTES: usize = 16 * 1024;
const STACK_CANARY: u32 = 0xA5A5_5A5A;

static RECORD_STACK_TOP: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());

pub(super) fn install() -> bool {
    let Some(top) = allocate_record_stack() else {
        return false;
    };
    RECORD_STACK_TOP.store(top, Ordering::Release);
    set_wifi_record_runner(run_job);
    true
}

static POLL_STACK_BASE: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());

/// Allocate a stack in external RAM and return its top pointer.
///
/// The allocation is kept for the life of the runtime. `run_core`'s poll frame
/// subtracts `0x8290` bytes on entry, and core 0 has about 36 KiB free above a
/// 4 KiB guard. A canary at the base is checked after each [`call`].
pub(super) fn allocate_external_stack(bytes: usize) -> *mut u8 {
    let bytes = bytes.max(64).next_multiple_of(16);
    let layout = Layout::from_size_align(bytes, 16).expect("external stack layout");
    let block = Allocator::allocate(&esp_alloc::ExternalMemory, layout)
        .unwrap_or_else(|_| panic!("external stack allocation of {bytes} bytes failed"));
    let start = block.as_ptr() as *mut u8;
    // SAFETY: `start` is the base of the allocation and is writable for `layout`.
    unsafe { start.cast::<u32>().write_volatile(STACK_CANARY) };
    POLL_STACK_BASE.store(start, Ordering::Release);
    let top = unsafe { start.add(layout.size()) };
    esp_rtos::allow_extra_stack(start as usize, top as usize);
    top
}

/// Run `body` with the stack pointer at `stack_top`, then restore this frame.
pub(super) fn call(stack_top: *mut u8, body: &mut dyn FnMut()) {
    let mut body = body as *mut dyn FnMut();
    // SAFETY: `stack_top` came from [`allocate_external_stack`] and `body` stays
    // alive for the call.
    unsafe {
        call_on_stack(
            stack_top,
            trampoline,
            (&mut body as *mut *mut dyn FnMut()).cast(),
        )
    };
    let base = POLL_STACK_BASE.load(Ordering::Acquire);
    if !base.is_null() {
        // SAFETY: `base` is the canary word written at allocation.
        let canary = unsafe { base.cast::<u32>().read_volatile() };
        if canary != STACK_CANARY {
            panic!("run_core poll overflowed the external stack");
        }
    }
}

fn allocate_record_stack() -> Option<*mut u8> {
    let layout = Layout::from_size_align(RECORD_STACK_BYTES, 16).ok()?;
    let block = Allocator::allocate(&esp_alloc::ExternalMemory, layout).ok()?;
    let start = block.as_ptr() as *mut u8;
    // `allocate` returns the base. The stack grows down from this top.
    Some(unsafe { start.add(layout.size()) })
}

fn run_job(body: &mut dyn FnMut()) {
    let top = RECORD_STACK_TOP.load(Ordering::Acquire);
    if top.is_null() {
        body();
        return;
    }
    let mut body = body as *mut dyn FnMut();
    unsafe { call_on_stack(top, trampoline, (&mut body as *mut *mut dyn FnMut()).cast()) };
}

unsafe extern "C" fn trampoline(arg: *mut u8) {
    let body = unsafe { *arg.cast::<*mut dyn FnMut()>() };
    unsafe { (*body)() };
}

/// Calls `func(arg)` with the stack pointer set to `stack_top`.
///
/// `stack_top` is one past a writable, 16-byte-aligned region. The call runs
/// with that as the callee stack and restores this frame's pointer from `a5`
/// afterwards.
///
/// # Safety
///
/// `stack_top` must be 16-byte aligned and the bytes below it, for the whole
/// call, must be a writable stack that outlives the call. `func` must be a
/// windowed-ABI function.
///
/// After `entry`, the incoming arguments are in `a2`/`a3`/`a4`. A `callx8`
/// rotates by eight, so the outgoing target goes in `a8` and the first
/// argument in `a10` — the same placement rustc uses. Putting the argument in
/// `a2` delivers a null pointer to `func`.
///
/// `a2`–`a7` stay in this window across `callx8`, so the caller's stack
/// pointer is kept in `a5`. `movsp` is required: a plain `mov` into `a1`
/// leaves the register-window spill area on the old stack, and `retw` then
/// returns onto the guard.
#[unsafe(naked)]
unsafe extern "C" fn call_on_stack(
    stack_top: *mut u8,
    func: unsafe extern "C" fn(*mut u8),
    arg: *mut u8,
) {
    core::arch::naked_asm!(
        "entry a1, 32",
        "mov a5, a1",
        "addi a2, a2, -16",
        "movsp a1, a2",
        "mov a10, a4",
        "mov a8, a3",
        "callx8 a8",
        "movsp a1, a5",
        "retw",
    );
}
