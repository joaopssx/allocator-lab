//! Edge cases for alignment math and region bounds.

use alloc_core::Region;
use alloc_core::align::{align_down, align_up, is_aligned};
use std::alloc::{Layout, alloc, dealloc};
use std::ptr::NonNull;

#[test]
fn align_up_rounds_to_next_multiple() {
    assert_eq!(align_up(0, 1), Some(0));
    assert_eq!(align_up(0, 4096), Some(0));
    assert_eq!(align_up(1, 4096), Some(4096));
    assert_eq!(align_up(4096, 4096), Some(4096));
    assert_eq!(align_up(4097, 4096), Some(8192));
    assert_eq!(align_up(7, 1), Some(7));
}

#[test]
fn align_up_reports_overflow() {
    assert_eq!(align_up(usize::MAX, 2), None);
    assert_eq!(align_up(usize::MAX - 4094, 4096), None);
    // The last multiple of 4096 that fits is still reachable.
    let top = usize::MAX & !4095;
    assert_eq!(align_up(top - 1, 4096), Some(top));
    assert_eq!(align_up(top, 4096), Some(top));
    // Already aligned values never overflow, even at the top.
    assert_eq!(align_up(usize::MAX, 1), Some(usize::MAX));
}

#[test]
fn align_down_and_is_aligned_agree() {
    for addr in [0, 1, 15, 16, 17, 4095, 4096, 4097, usize::MAX] {
        for shift in 0..=12 {
            let a = 1usize << shift;
            let down = align_down(addr, a);
            assert!(is_aligned(down, a));
            assert!(down <= addr && addr - down < a);
            assert_eq!(is_aligned(addr, a), down == addr);
        }
    }
}

/// Heap buffer that lives as long as the test, aligned to a page.
struct Buf {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl Buf {
    fn new(size: usize) -> Buf {
        let layout = Layout::from_size_align(size, 4096).unwrap();
        // SAFETY: size is non-zero in every caller.
        let ptr = NonNull::new(unsafe { alloc(layout) }).expect("test buffer");
        Buf { ptr, layout }
    }

    fn region(&self) -> Region {
        // SAFETY: the buffer outlives the region in every test and nothing
        // else touches it.
        unsafe { Region::new(self.ptr, self.layout.size()) }
    }
}

impl Drop for Buf {
    fn drop(&mut self) {
        // SAFETY: allocated in `Buf::new` with the same layout.
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

#[test]
fn region_bounds() {
    let buf = Buf::new(4096);
    let r = buf.region();
    let (start, end) = (r.start(), r.end());

    assert_eq!(end - start, 4096);
    assert!(is_aligned(start, 4096));
    assert!(r.contains(start, 4096));
    assert!(r.contains(start, 0));
    assert!(r.contains(end, 0));
    assert!(!r.contains(start, 4097));
    assert!(!r.contains(start + 1, 4096));
    assert!(!r.contains(start - 1, 1));

    assert_eq!(r.offset(start), Some(0));
    assert_eq!(r.offset(end), Some(4096));
    assert_eq!(r.offset(end + 1), None);
    assert_eq!(r.offset(start - 1), None);
}

#[test]
fn region_contains_rejects_wrapping_ranges() {
    let buf = Buf::new(64);
    let r = buf.region();
    assert!(!r.contains(r.start(), usize::MAX));
    assert!(!r.contains(usize::MAX, 2));
}

#[test]
fn region_pointers_are_usable() {
    let buf = Buf::new(4096);
    let r = buf.region();
    let p = r.ptr(r.start() + 100);
    // SAFETY: offset 100..104 is inside the buffer, and p has the region's
    // provenance, which Miri checks.
    unsafe {
        p.cast::<u32>().write_unaligned(0xdead_beef);
        assert_eq!(
            r.ptr(r.start() + 100).cast::<u32>().read_unaligned(),
            0xdead_beef
        );
    }
}

#[test]
fn empty_region() {
    let buf = Buf::new(64);
    // SAFETY: zero bytes are trivially valid and exclusive.
    let r = unsafe { Region::new(buf.ptr, 0) };
    assert_eq!(r.size(), 0);
    assert!(r.contains(r.start(), 0));
    assert!(!r.contains(r.start(), 1));
}
