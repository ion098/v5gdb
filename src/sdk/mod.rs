//! Runtime patching of SDK functions to emulate different return values or arguments.
//!
//! This module doesn't affect the actual implementations of the underlying SDK functions, but it is
//! able to add a proxy layer between VEXos and user code via the wrapper functions defined in
//! libraries like v5rt and vex_sdk_jumptable. The wrapper functions normally get inlined into their
//! call sites when LTO is on, so the functionality in this module only works when LTO is off.

use core::{arch::global_asm, ptr};

use aarch32_cpu::asm::{dsb, isb};

use crate::cpu::cache::{self, CacheTarget};

pub mod competition;

global_asm!(include_str!("./sdk_trampoline.s"), options(raw));
unsafe extern "C" {
    /// A position-independent ARM function that jumps to another (configurable) function.
    fn v5gdb_sdk_trampoline_arm();
    /// Marks the end of the code for [`v5gdb_sdk_trampoline_arm`].
    static v5gdb_sdk_trampoline_arm_end: u32;
    /// A position-independent Thumb function that jumps to another (configurable) function.
    fn v5gdb_sdk_trampoline_thumb();
    /// Marks the end of the code for [`v5gdb_sdk_trampoline_thumb`].
    static v5gdb_sdk_trampoline_thumb_end: u32;
}

/// Overwrite the target function to branch to the given proxy when called instead of performing
/// its original functionality.
///
/// The target function's instruction set is detected via its Thumb bit and a matching trampoline is
/// installed.
///
/// # Safety
///
/// The target function must be at least 3 words long, properly aligned for its instruction set, and
/// valid to write to. The destination function must be valid to call in all the same situations as
/// the target function and also have the same signature as it.
pub unsafe fn redirect_function(target: *mut (), destination: *const ()) {
    const THUMB_BIT: usize = 0b1;
    let is_thumb = (target as usize) & THUMB_BIT != 0;

    let (trampoline_fn, trampoline_end) = if is_thumb {
        (
            v5gdb_sdk_trampoline_thumb as unsafe extern "C" fn(),
            &raw const v5gdb_sdk_trampoline_thumb_end,
        )
    } else {
        (
            v5gdb_sdk_trampoline_arm as unsafe extern "C" fn(),
            &raw const v5gdb_sdk_trampoline_arm_end,
        )
    };

    // We cast to u16 since the target function may be a 2-byte aligned Thumb function.
    let trampoline_src = ((trampoline_fn as usize) & !THUMB_BIT) as *const u16;
    let write_addr = ((target as usize) & !THUMB_BIT) as *mut u16;

    let code_len = (trampoline_end as usize) - (trampoline_src as usize);
    let destination_slot = unsafe { write_addr.add(code_len) };

    unsafe {
        ptr::copy_nonoverlapping(trampoline_src, write_addr, code_len);
        // Keep the destination's Thumb bit intact so the trampoline's `bx` enters it in the
        // correct instruction set.
        ptr::write_unaligned(destination_slot.cast::<u32>(), destination as u32);
    }

    dsb();
    isb();

    // Sync both start and end, in case the function crosses a cache line.
    cache::sync_instruction(CacheTarget::Address(write_addr as u32));
    cache::sync_instruction(CacheTarget::Address(destination_slot as u32));
}

/// Directly access VEX SDK functions over the jump table without their wrappers.
///
/// This is effectively a partial re-implementation of the `vex-sdk-jumptable` crate, which we can't
/// use here because those might be the functions we are redirecting. If we were to call those
/// directly, it might cause an infinite loop.
macro_rules! jumptable {
    ($offset:literal, $ty:ty) => {{
        const JUMPTABLE_BASE: u32 = 0x037fc000;
        let ptr = (JUMPTABLE_BASE + $offset) as *const $ty;
        *ptr
    }};
}
pub(crate) use jumptable;
