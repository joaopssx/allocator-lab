//! Binary buddy allocator over a fixed [`Region`].
//!
//! Blocks have power-of-two sizes from [`MIN`] bytes up. A block of order `k`
//! is `MIN << k` bytes and sits at an offset from the base that is a multiple
//! of its size. Splitting a block gives two halves of the next lower order,
//! which are each other's buddies.
//!
//! Each order has its own doubly linked free list, threaded through the free
//! blocks. A bitmap at the end of the region records which `(order, offset)`
//! pairs are on a list, so checking whether a buddy is free never relies on
//! reading memory that might belong to a live block.

#![no_std]

mod raw;

use alloc_core::align::{align_down, align_up};
use alloc_core::{Region, SpinLock};
use core::alloc::Layout;
use raw::{NIL, Node};

/// Size of an order-0 block. Also the size of a free-list node.
pub const MIN: usize = 16;
const MIN_SHIFT: u32 = MIN.trailing_zeros();

/// Largest alignment served. The base of the block area is aligned to this,
/// so any block of at least this size is aligned to it too.
pub const MAX_ALIGN: usize = 4096;

/// Number of orders a `usize` address space can need.
const ORDERS: usize = (usize::BITS - MIN_SHIFT) as usize;

/// Order of the smallest block that fits `layout`, or `None` if the
/// alignment is above [`MAX_ALIGN`] or the size has no power of two.
pub fn order_of(layout: Layout) -> Option<usize> {
    if layout.align() > MAX_ALIGN {
        return None;
    }
    let need = layout.size().max(layout.align()).max(MIN);
    let bits = need.checked_next_power_of_two()?.trailing_zeros();
    Some((bits - MIN_SHIFT) as usize)
}

/// Size in bytes of a block of order `k`.
pub const fn block_size(k: usize) -> usize {
    MIN << k
}

/// A buddy allocator.
pub struct Buddy {
    region: Region,
    state: SpinLock<State>,
}

struct State {
    /// Set once the bitmap is cleared and the initial blocks are on the
    /// lists. Done lazily because `new` is a `const fn`.
    ready: bool,
    geo: Geometry,
    /// Head of the free list of each order, or `NIL`.
    heads: [usize; ORDERS],
}

/// Where things live inside the region. Computed once, from the region alone.
#[derive(Clone, Copy)]
struct Geometry {
    /// Start of the block area, aligned to `MAX_ALIGN`.
    base: usize,
    /// Size of the block area, a multiple of `MIN`.
    space: usize,
    /// Start of the bitmap, right after the block area.
    map: usize,
    /// Index of the first bit of each order in the bitmap.
    starts: [usize; ORDERS],
}

impl Geometry {
    const EMPTY: Geometry = Geometry {
        base: 0,
        space: 0,
        map: 0,
        starts: [0; ORDERS],
    };

    fn new(region: &Region) -> Geometry {
        let Some(base) = align_up(region.start(), MAX_ALIGN) else {
            return Geometry::EMPTY;
        };
        if base >= region.end() {
            return Geometry::EMPTY;
        }
        let total = region.end() - base;
        // Order k needs one bit per possible block, `space >> (MIN_SHIFT + k)`
        // bits. Summed over every order that is under `space / 8` bits, i.e.
        // `space / 64` bytes, so reserving 1/65 of the area is always enough.
        let space = align_down(total / 65 * 64, MIN);
        let mut starts = [0; ORDERS];
        let mut idx = 0;
        for (k, s) in starts.iter_mut().enumerate() {
            *s = idx;
            idx += space >> (MIN_SHIFT as usize + k);
        }
        debug_assert!(space + idx.div_ceil(8) <= total);
        Geometry {
            base,
            space,
            map: base + space,
            starts,
        }
    }

    /// Bitmap length in bytes.
    fn map_len(&self) -> usize {
        let last = ORDERS - 1;
        (self.starts[last] + (self.space >> (MIN_SHIFT as usize + last))).div_ceil(8)
    }

    /// Bit that says whether the order-`k` block at `addr` is free.
    fn bit(&self, k: usize, addr: usize) -> usize {
        self.starts[k] + ((addr - self.base) >> (MIN_SHIFT as usize + k))
    }
}

impl Buddy {
    /// Creates an allocator that owns `region`.
    pub const fn new(region: Region) -> Buddy {
        Buddy {
            region,
            state: SpinLock::new(State {
                ready: false,
                geo: Geometry::EMPTY,
                heads: [NIL; ORDERS],
            }),
        }
    }

    /// The region this allocator manages.
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// Size of the block area: the region minus the alignment of its start
    /// and the bitmap at its end.
    pub fn capacity(&self) -> usize {
        Geometry::new(&self.region).space
    }

    /// Total size of all free blocks.
    pub fn free_bytes(&self) -> usize {
        let mut st = self.state.lock();
        self.prepare(&mut st);
        let mut total = 0;
        for (k, &head) in st.heads.iter().enumerate() {
            let mut at = head;
            while at != NIL {
                total += block_size(k);
                // SAFETY: `at` is on free list `k`.
                at = unsafe { raw::node(&self.region, at) }.next;
            }
        }
        total
    }

    /// Puts the order-`k` block at `addr` at the head of list `k`.
    ///
    /// # Safety
    ///
    /// The block must lie in the block area, be aligned to its size, and be
    /// free: not live and not on any list.
    unsafe fn push(&self, st: &mut State, k: usize, addr: usize) {
        let head = st.heads[k];
        // SAFETY: the block is free (caller contract) and so is `head`.
        unsafe {
            raw::set_node(
                &self.region,
                addr,
                Node {
                    next: head,
                    prev: NIL,
                },
            );
            if head != NIL {
                let mut h = raw::node(&self.region, head);
                h.prev = addr;
                raw::set_node(&self.region, head, h);
            }
            raw::set_bit(&self.region, st.geo.map, st.geo.bit(k, addr), true);
        }
        st.heads[k] = addr;
    }

    fn prepare(&self, st: &mut State) {
        if st.ready {
            return;
        }
        st.ready = true;
        st.geo = Geometry::new(&self.region);
        let geo = st.geo;
        // SAFETY: `map..map + map_len` is the bitmap area that `Geometry`
        // reserved past the block area, inside the region.
        unsafe { raw::clear(&self.region, geo.map, geo.map_len()) };

        // Cover the block area with the largest blocks that fit. Taking sizes
        // in decreasing order keeps every block aligned to its own size. The
        // buddy of one of these roots is either past the end of the area or
        // the start of a smaller root, so it can never be a free block of the
        // root's order, and merging never crosses from one root into another.
        let mut off = 0;
        while geo.space - off >= MIN {
            let rest = geo.space - off;
            let k = (rest.ilog2() - MIN_SHIFT) as usize;
            let k = k.min(ORDERS - 1);
            // SAFETY: the block is inside the area, aligned (see above), and
            // nothing has been handed out yet.
            unsafe { self.push(st, k, geo.base + off) };
            off += block_size(k);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use core::ptr::NonNull;
    use std::alloc::{alloc, dealloc};

    fn l(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).unwrap()
    }

    #[test]
    fn orders() {
        assert_eq!(order_of(l(0, 1)), Some(0));
        assert_eq!(order_of(l(16, 1)), Some(0));
        assert_eq!(order_of(l(17, 1)), Some(1));
        assert_eq!(order_of(l(1, 4096)), Some(8));
        assert_eq!(order_of(l(4097, 8)), Some(9));
        assert_eq!(order_of(l(1, 8192)), None);
        assert_eq!(order_of(l(isize::MAX as usize, 1)), Some(59));
        assert_eq!(block_size(8), 4096);
    }

    #[test]
    fn odd_sized_area_is_tiled_by_decreasing_blocks() {
        let size = 3 * 4096;
        let layout = Layout::from_size_align(size, 4096).unwrap();
        // SAFETY: non-zero layout.
        let base = NonNull::new(unsafe { alloc(layout) }).unwrap();
        // SAFETY: the buffer outlives `b` and only `b` touches it.
        let b = Buddy::new(unsafe { Region::new(base, size) });

        // 12288 / 65 * 64, rounded to 16: 8192 + 2048 + 1024 + 512 + 256 + 64.
        assert_eq!(b.capacity(), 12096);
        assert_eq!(b.free_bytes(), 12096);
        let st = b.state.lock();
        let roots: std::vec::Vec<_> = (0..ORDERS).filter(|&k| st.heads[k] != NIL).collect();
        assert_eq!(roots, [2, 4, 5, 6, 7, 9]);
        drop(st);

        // SAFETY: allocated above with this layout.
        unsafe { dealloc(base.as_ptr(), layout) };
    }
}
