//! The benchmark binary's global allocator.
//!
//! Every lab allocator lives in a `static` with its own arena. `Switch`
//! forwards allocations to whichever one is active, and frees each pointer
//! to the allocator whose region contains it. Anything allocated before the
//! switch (by the runtime, argument parsing, and so on) came from the system
//! allocator and goes back there.

use alloc_core::{Arena, Stats};
use buddy::Buddy;
use bump::Bump;
use freelist::{Fit, FreeList};
use slab::Slab;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU8, Ordering};

/// Size of each allocator's arena.
pub const ARENA: usize = 16 << 20;

static A_BUMP: Arena<ARENA> = Arena::new();
static A_FIRST: Arena<ARENA> = Arena::new();
static A_BEST: Arena<ARENA> = Arena::new();
static A_NEXT: Arena<ARENA> = Arena::new();
static A_SLAB: Arena<ARENA> = Arena::new();
static A_BUDDY: Arena<ARENA> = Arena::new();

// SAFETY: the only region ever taken from A_BUMP.
static BUMP: Bump = Bump::new(unsafe { A_BUMP.region() });
// SAFETY: the only region ever taken from A_FIRST.
static FIRST: FreeList = FreeList::new(unsafe { A_FIRST.region() }, Fit::First);
// SAFETY: the only region ever taken from A_BEST.
static BEST: FreeList = FreeList::new(unsafe { A_BEST.region() }, Fit::Best);
// SAFETY: the only region ever taken from A_NEXT.
static NEXT: FreeList = FreeList::new(unsafe { A_NEXT.region() }, Fit::Next);
// SAFETY: the only region ever taken from A_SLAB.
static SLAB: Slab = Slab::new(unsafe { A_SLAB.region() });
// SAFETY: the only region ever taken from A_BUDDY.
static BUDDY: Buddy = Buddy::new(unsafe { A_BUDDY.region() });

/// Which allocator serves new requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// The platform allocator, as the baseline.
    System,
    /// [`Bump`].
    Bump,
    /// [`FreeList`] with first fit.
    First,
    /// [`FreeList`] with best fit.
    Best,
    /// [`FreeList`] with next fit.
    Next,
    /// [`Slab`].
    Slab,
    /// [`Buddy`].
    Buddy,
}

impl Kind {
    /// Every allocator, baseline first.
    pub const ALL: [Kind; 7] = [
        Kind::System,
        Kind::Bump,
        Kind::First,
        Kind::Best,
        Kind::Next,
        Kind::Slab,
        Kind::Buddy,
    ];

    /// Short name used on the command line and in reports.
    pub fn name(self) -> &'static str {
        match self {
            Kind::System => "system",
            Kind::Bump => "bump",
            Kind::First => "first-fit",
            Kind::Best => "best-fit",
            Kind::Next => "next-fit",
            Kind::Slab => "slab",
            Kind::Buddy => "buddy",
        }
    }

    /// Inverse of [`Kind::name`].
    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }
}

static ACTIVE: AtomicU8 = AtomicU8::new(Kind::System as u8);

/// Routes new allocations to `k` from now on.
pub fn activate(k: Kind) {
    ACTIVE.store(k as u8, Ordering::SeqCst);
}

fn active() -> Kind {
    Kind::ALL[ACTIVE.load(Ordering::Relaxed) as usize]
}

/// Runs `$body` with `$a` bound to the allocator for `$kind`.
macro_rules! with_kind {
    ($kind:expr, |$a:ident| $body:expr) => {
        match $kind {
            Kind::System => {
                let $a = &System;
                $body
            }
            Kind::Bump => {
                let $a = &BUMP;
                $body
            }
            Kind::First => {
                let $a = &FIRST;
                $body
            }
            Kind::Best => {
                let $a = &BEST;
                $body
            }
            Kind::Next => {
                let $a = &NEXT;
                $body
            }
            Kind::Slab => {
                let $a = &SLAB;
                $body
            }
            Kind::Buddy => {
                let $a = &BUDDY;
                $body
            }
        }
    };
}

/// The allocator whose region contains `ptr`, or the system allocator.
fn owner(ptr: *mut u8) -> Kind {
    let addr = ptr.addr();
    let regions = [
        (Kind::Bump, BUMP.region()),
        (Kind::First, FIRST.region()),
        (Kind::Best, BEST.region()),
        (Kind::Next, NEXT.region()),
        (Kind::Slab, SLAB.region()),
        (Kind::Buddy, BUDDY.region()),
    ];
    regions
        .into_iter()
        .find(|(_, r)| r.contains(addr, 1))
        .map_or(Kind::System, |(k, _)| k)
}

/// Forwards to the active allocator, and frees to the owner of the pointer.
pub struct Switch;

// SAFETY: every call is forwarded unchanged to a `GlobalAlloc` that upholds
// the trait's contract. A pointer is always freed or resized by the
// allocator that produced it: lab allocators only hand out pointers inside
// their own regions, the regions are disjoint statics, and the system
// allocator never returns memory inside them.
unsafe impl GlobalAlloc for Switch {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded from the caller.
        with_kind!(active(), |a| unsafe { a.alloc(layout) })
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded from the caller.
        with_kind!(active(), |a| unsafe { a.alloc_zeroed(layout) })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded to the allocator that produced `ptr`.
        with_kind!(owner(ptr), |a| unsafe { a.dealloc(ptr, layout) })
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: forwarded to the allocator that produced `ptr`.
        with_kind!(owner(ptr), |a| unsafe { a.realloc(ptr, layout, new_size) })
    }
}

/// Memory usage of a lab allocator at one point in time.
#[derive(Debug, Clone, Copy)]
pub struct Usage {
    /// Bytes requested by the caller and still live.
    pub requested: usize,
    /// Bytes the allocator has taken out of its free space for them.
    pub used: usize,
    /// Largest single block it could still hand out, if it can tell.
    pub largest: Option<usize>,
    /// Free bytes in total.
    pub free: usize,
}

/// Current usage of `k`, or `None` for the system allocator.
pub fn usage(k: Kind) -> Option<Usage> {
    let (cap, free, largest, stats): (usize, usize, Option<usize>, Stats) = match k {
        Kind::System => return None,
        Kind::Bump => {
            let free = ARENA - BUMP.used();
            (ARENA, free, Some(free), BUMP.stats())
        }
        Kind::First | Kind::Best | Kind::Next => {
            let fl = match k {
                Kind::First => &FIRST,
                Kind::Best => &BEST,
                _ => &NEXT,
            };
            let free = fl.free_bytes();
            (fl.capacity(), free, Some(fl.largest_free()), fl.stats())
        }
        Kind::Slab => (SLAB.capacity(), SLAB.free_bytes(), None, SLAB.stats()),
        Kind::Buddy => (
            BUDDY.capacity(),
            BUDDY.free_bytes(),
            Some(BUDDY.largest_free()),
            BUDDY.stats(),
        ),
    };
    Some(Usage {
        requested: stats.live_bytes,
        used: cap - free,
        largest,
        free,
    })
}

/// Releases everything at once, for allocators that can.
///
/// # Safety
///
/// Every block `k` handed out must be dead.
pub unsafe fn reset(k: Kind) {
    if k == Kind::Bump {
        // SAFETY: forwarded from the caller.
        unsafe { BUMP.reset() };
    }
}
