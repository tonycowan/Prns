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

static RECORD_STACK_TOP: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());

pub(super) fn install() {
    let Some(top) = allocate_record_stack() else {
        return;
    };
    RECORD_STACK_TOP.store(top, Ordering::Release);
    set_wifi_record_runner(run_job);
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
/// `stack_top` is one past a writable, 16-byte-aligned region. The previous
/// stack pointer is stored in the top 16 bytes of that region and restored
/// after `func` returns.
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
#[unsafe(naked)]
unsafe extern "C" fn call_on_stack(
    stack_top: *mut u8,
    func: unsafe extern "C" fn(*mut u8),
    arg: *mut u8,
) {
    core::arch::naked_asm!(
        "entry a1, 32",
        "addi a2, a2, -16",
        "s32i a1, a2, 0",
        "mov a10, a4",
        "mov a8, a3",
        "mov a1, a2",
        "callx8 a8",
        "l32i a1, a1, 0",
        "retw",
    );
}
