//! Support for contracts built for the bare-metal target (`feature = "runtime"`): panic handler,
//! bump allocator and the `mem*` routines.
//!
//! The VM runs each call in a fresh memory image, so the allocator never frees. The heap is a
//! fixed zero-initialised region in `.bss`. The `mem*` routines are hand-written rv64im assembly
//! (byte loops) so no precompiled, possibly compressed or atomic, library code is needed and the
//! compiler cannot turn them into calls to themselves.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

const HEAP_SIZE: usize = 128 * 1024;

#[repr(align(16))]
struct Heap(UnsafeCell<[u8; HEAP_SIZE]>);
// SAFETY: the VM is single-threaded.
unsafe impl Sync for Heap {}
static HEAP: Heap = Heap(UnsafeCell::new([0; HEAP_SIZE]));

struct Next(UnsafeCell<usize>);
// SAFETY: the VM is single-threaded.
unsafe impl Sync for Next {}
static NEXT: Next = Next(UnsafeCell::new(0));

struct Bump;

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let next = NEXT.0.get();
        let base = HEAP.0.get() as usize;
        let start = (base + *next + layout.align() - 1) & !(layout.align() - 1);
        let end = match start.checked_add(layout.size()) {
            Some(e) if e <= base + HEAP_SIZE => e,
            _ => return core::ptr::null_mut(),
        };
        *next = end - base;
        start as *mut u8
    }
    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
}

#[global_allocator]
static ALLOC: Bump = Bump;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    crate::host::abort("panic")
}

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    r#"
    .option push
    .option norvc
    .section .text.btb_mem,"ax"
    .globl memcpy
memcpy:
    mv a3, a0
1:  beqz a2, 2f
    lbu a4, 0(a1)
    sb a4, 0(a3)
    addi a1, a1, 1
    addi a3, a3, 1
    addi a2, a2, -1
    j 1b
2:  ret

    .globl memmove
memmove:
    bgeu a0, a1, 3f
    mv a3, a0
1:  beqz a2, 2f
    lbu a4, 0(a1)
    sb a4, 0(a3)
    addi a1, a1, 1
    addi a3, a3, 1
    addi a2, a2, -1
    j 1b
2:  ret
3:  add a3, a0, a2
    add a1, a1, a2
4:  beqz a2, 5f
    addi a1, a1, -1
    addi a3, a3, -1
    lbu a4, 0(a1)
    sb a4, 0(a3)
    addi a2, a2, -1
    j 4b
5:  ret

    .globl memset
memset:
    mv a3, a0
1:  beqz a2, 2f
    sb a1, 0(a3)
    addi a3, a3, 1
    addi a2, a2, -1
    j 1b
2:  ret

    .globl memcmp
    .globl bcmp
memcmp:
bcmp:
1:  beqz a2, 2f
    lbu a3, 0(a0)
    lbu a4, 0(a1)
    bne a3, a4, 3f
    addi a0, a0, 1
    addi a1, a1, 1
    addi a2, a2, -1
    j 1b
2:  li a0, 0
    ret
3:  sub a0, a3, a4
    ret
    .option pop
"#
);
