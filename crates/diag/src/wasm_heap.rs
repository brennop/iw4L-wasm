//! The wasm heap: dlmalloc, as std uses, but growing linear memory in large
//! steps.
//!
//! std's wasm allocator grows memory by exactly what the failing request
//! needs, so an mp_rust load reaches its ~3.8 GB peak in ~5,800
//! `memory.grow` calls. On Chrome/Windows each call costs 9-16 ms, rising
//! with the heap size, which added up to ~51 s of main-thread time in one
//! trace. Growing by at least [`GROW_STEP`] cuts that to a few dozen calls.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::UnsafeCell;

use dlmalloc::{Allocator, Dlmalloc};

#[cfg(target_feature = "atomics")]
compile_error!("WasmHeap has no lock; a threaded wasm build needs one");

/// Smallest `memory.grow`, in bytes; a larger request grows by what it needs.
const GROW_STEP: usize = 64 << 20;

/// dlmalloc's system layer for wasm (after `dlmalloc::System`, which is not
/// public), with a floor on every grow. If the step does not fit under the
/// 4 GiB ceiling, the exact request is tried instead. Memory never goes back
/// to the host, as with std's. Unlike std's, the linker's spare initial
/// memory past `__heap_base` (a few MiB at most) is not used.
struct SteppedSystem;

const PAGE: usize = 64 << 10;

impl SteppedSystem {
    fn grow(bytes: usize) -> (*mut u8, usize, u32) {
        let pages = bytes.div_ceil(PAGE);
        let prev = core::arch::wasm32::memory_grow(0, pages);
        if prev == usize::MAX {
            return (std::ptr::null_mut(), 0, 0);
        }
        let base = prev * PAGE;
        let size = pages * PAGE;
        // A region ending exactly at the top of the address space would let
        // a one-past-the-end pointer wrap to 0; dlmalloc::System keeps the
        // last 16 bytes out for the same reason.
        let size = if base.wrapping_add(size) == 0 {
            size - 16
        } else {
            size
        };
        (base as *mut u8, size, 0)
    }
}

unsafe impl Allocator for SteppedSystem {
    fn alloc(&self, size: usize) -> (*mut u8, usize, u32) {
        let stepped = Self::grow(size.max(GROW_STEP));
        if stepped.0.is_null() {
            Self::grow(size)
        } else {
            stepped
        }
    }

    fn remap(&self, _ptr: *mut u8, _oldsize: usize, _newsize: usize, _can_move: bool) -> *mut u8 {
        std::ptr::null_mut()
    }

    fn free_part(&self, _ptr: *mut u8, _oldsize: usize, _newsize: usize) -> bool {
        false
    }

    fn free(&self, _ptr: *mut u8, _size: usize) -> bool {
        false
    }

    fn can_release_part(&self, _flags: u32) -> bool {
        false
    }

    fn allocates_zeros(&self) -> bool {
        true
    }

    fn page_size(&self) -> usize {
        PAGE
    }
}

pub(crate) struct WasmHeap(UnsafeCell<Dlmalloc<SteppedSystem>>);

// Sound only without wasm threads (see the compile_error above): one thread
// ever touches the heap, so the unlocked `&mut` below is never aliased.
unsafe impl Sync for WasmHeap {}

impl WasmHeap {
    pub(crate) const fn new() -> Self {
        Self(UnsafeCell::new(Dlmalloc::new_with_allocator(SteppedSystem)))
    }

    #[allow(clippy::mut_from_ref)]
    unsafe fn heap(&self) -> &mut Dlmalloc<SteppedSystem> {
        unsafe { &mut *self.0.get() }
    }
}

unsafe impl GlobalAlloc for WasmHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { self.heap().malloc(layout.size(), layout.align()) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { self.heap().calloc(layout.size(), layout.align()) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { self.heap().free(ptr, layout.size(), layout.align()) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        unsafe {
            self.heap()
                .realloc(ptr, layout.size(), layout.align(), new_size)
        }
    }
}
