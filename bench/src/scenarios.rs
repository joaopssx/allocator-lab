//! Workloads. Each one runs through the global allocator, so it measures
//! whatever `switch::activate` selected, and returns how many allocator
//! operations it performed.
//!
//! Raw `alloc`/`dealloc` calls are used where the pattern itself is the
//! point; `Vec` and `HashMap` where the point is how real code behaves.
//!
//! The lists that remember which blocks are live are allocated by the system
//! allocator before the switch, with enough capacity that they never grow.
//! Otherwise the bookkeeping would show up in the numbers, the slab
//! allocator would refuse it, and a bump reset would free it.

use crate::switch::{self, Kind, Usage};
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::collections::HashMap;
use std::hint::black_box;

type Live = Vec<(*mut u8, Layout)>;

/// Capacity of each bookkeeping list. No scenario keeps more blocks.
const SCRATCH: usize = 20_000;

/// State shared with a scenario while it runs.
pub struct Ctx {
    /// Allocator under test.
    pub kind: Kind,
    /// Usage recorded at the scenario's peak, on the first run only.
    pub peak: Option<Usage>,
    first: bool,
    rng: u64,
    a: Live,
    b: Live,
}

impl Ctx {
    /// A context for `kind`. Must be created before `switch::activate`, so
    /// the bookkeeping lists come from the system allocator.
    pub fn new(kind: Kind) -> Ctx {
        Ctx {
            kind,
            peak: None,
            first: true,
            rng: 0x9e37_79b9_7f4a_7c15,
            a: Vec::with_capacity(SCRATCH),
            b: Vec::with_capacity(SCRATCH),
        }
    }

    /// Takes the two bookkeeping lists, empty. Give them back with `give`.
    fn lists(&mut self) -> (Live, Live) {
        (std::mem::take(&mut self.a), std::mem::take(&mut self.b))
    }

    fn give(&mut self, mut a: Live, mut b: Live) {
        a.clear();
        b.clear();
        self.a = a;
        self.b = b;
    }

    /// Called by a scenario at the point where the most memory is live.
    fn mark_peak(&mut self) {
        if self.first {
            self.peak = switch::usage(self.kind);
        }
    }

    /// Called by the harness between runs. Everything a scenario allocated
    /// is freed by then.
    pub fn end_run(&mut self) {
        self.first = false;
        self.rng = 0x9e37_79b9_7f4a_7c15;
        // SAFETY: scenarios free every block before returning.
        unsafe { switch::reset(self.kind) };
    }

    /// Frame boundary for allocators that release everything at once.
    fn end_frame(&mut self) {
        // SAFETY: the frame scenario frees every block of the frame first,
        // and its bookkeeping list belongs to the system allocator.
        unsafe { switch::reset(self.kind) };
    }

    fn below(&mut self, n: usize) -> usize {
        self.rng = self
            .rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.rng >> 33) % n as u64) as usize
    }
}

/// A scenario: name, what it shows, and the function that runs it.
pub type Scenario = (&'static str, &'static str, fn(&mut Ctx) -> u64);

/// Every scenario, in report order.
pub const ALL: &[Scenario] = &[
    (
        "small-fixed",
        "20 000 blocks of 32 bytes, then free in order",
        small_fixed,
    ),
    (
        "random",
        "50 000 random alloc/free steps, 8 to 2048 bytes",
        random,
    ),
    (
        "fifo",
        "10 000 random-size blocks, freed in allocation order",
        fifo,
    ),
    (
        "lifo",
        "10 000 random-size blocks, freed in reverse order",
        lifo,
    ),
    (
        "stack",
        "50 000 random push/pop steps on a stack of blocks",
        stack,
    ),
    (
        "fragment",
        "free every other block, then ask for larger ones",
        fragment,
    ),
    (
        "vec-growth",
        "20 Vec<u64> grown to 100 000 elements by push",
        vec_growth,
    ),
    (
        "hashmap",
        "HashMap<u64, u64>: 50 000 inserts, removals, reinserts",
        hashmap,
    ),
    (
        "game-frame",
        "200 frames of 1 000 short-lived allocations each",
        game_frame,
    ),
];

/// Allocates like `alloc`, but aborts on null the way `Box` would.
fn get(layout: Layout) -> *mut u8 {
    // SAFETY: scenario layouts are never zero-sized.
    let p = unsafe { alloc(layout) };
    if p.is_null() {
        handle_alloc_error(layout);
    }
    p
}

fn put(p: *mut u8, layout: Layout) {
    // SAFETY: every pointer passed here came from `get` or `alloc` with this
    // layout and is freed once.
    unsafe { dealloc(p, layout) }
}

fn layout(size: usize) -> Layout {
    Layout::from_size_align(size, 8).unwrap()
}

fn small_fixed(ctx: &mut Ctx) -> u64 {
    let (mut blocks, spare) = ctx.lists();
    let l = layout(32);
    for _ in 0..20_000 {
        blocks.push((get(l), l));
    }
    ctx.mark_peak();
    for &(p, l) in &blocks {
        put(black_box(p), l);
    }
    ctx.give(blocks, spare);
    40_000
}

fn random(ctx: &mut Ctx) -> u64 {
    let (mut live, spare) = ctx.lists();
    let mut ops = 0;
    let mut peaked = false;
    for _ in 0..50_000 {
        if live.len() < 2000 && (live.is_empty() || ctx.below(10) < 6) {
            let l = layout(8 + ctx.below(2041));
            live.push((get(l), l));
        } else {
            let (p, l) = live.swap_remove(ctx.below(live.len()));
            put(p, l);
        }
        ops += 1;
        if !peaked && live.len() == 2000 {
            peaked = true;
            ctx.mark_peak();
        }
    }
    ops += live.len() as u64;
    for &(p, l) in &live {
        put(p, l);
    }
    ctx.give(live, spare);
    ops
}

fn fill(ctx: &mut Ctx, blocks: &mut Live, n: usize) {
    for _ in 0..n {
        let l = layout(64 + ctx.below(449));
        blocks.push((get(l), l));
    }
}

fn fifo(ctx: &mut Ctx) -> u64 {
    let (mut blocks, spare) = ctx.lists();
    fill(ctx, &mut blocks, 10_000);
    ctx.mark_peak();
    for &(p, l) in &blocks {
        put(p, l);
    }
    ctx.give(blocks, spare);
    20_000
}

fn lifo(ctx: &mut Ctx) -> u64 {
    let (mut blocks, spare) = ctx.lists();
    fill(ctx, &mut blocks, 10_000);
    ctx.mark_peak();
    for &(p, l) in blocks.iter().rev() {
        put(p, l);
    }
    ctx.give(blocks, spare);
    20_000
}

fn stack(ctx: &mut Ctx) -> u64 {
    let (mut st, spare) = ctx.lists();
    let mut deepest = 0;
    for _ in 0..50_000 {
        if st.is_empty() || (st.len() < 4000 && ctx.below(2) == 0) {
            let l = layout(16 + ctx.below(241));
            st.push((get(l), l));
        } else if let Some((p, l)) = st.pop() {
            put(p, l);
        }
        if st.len() > deepest {
            deepest = st.len();
        }
    }
    ctx.mark_peak();
    let ops = 50_000 + st.len() as u64;
    while let Some((p, l)) = st.pop() {
        put(p, l);
    }
    ctx.give(st, spare);
    ops
}

/// The interesting output of this one is the usage snapshot: half the
/// blocks are freed, so the free space is plentiful but cut into holes too
/// small for the follow-up requests.
fn fragment(ctx: &mut Ctx) -> u64 {
    let (small, big) = (layout(256), layout(512));
    let (mut keep, mut holes) = ctx.lists();
    for i in 0..12_000 {
        let l = if i % 2 == 0 { small } else { big };
        let p = get(l);
        if i % 2 == 0 {
            holes.push((p, l));
        } else {
            keep.push((p, l));
        }
    }
    for (p, l) in holes.drain(..) {
        put(p, l);
    }
    ctx.mark_peak();
    // Requests larger than any hole: they have to come from fresh space.
    let wide = layout(1024);
    let mut extra = holes;
    for _ in 0..2000 {
        // SAFETY: non-zero layout; null is handled below.
        let p = unsafe { alloc(wide) };
        if p.is_null() {
            break;
        }
        extra.push((p, wide));
    }
    let ops = 12_000 + 6000 + 2 * extra.len() as u64 + 6000;
    for &(p, l) in extra.iter().chain(&keep) {
        put(p, l);
    }
    ctx.give(keep, extra);
    ops
}

fn vec_growth(ctx: &mut Ctx) -> u64 {
    let mut ops = 0;
    for round in 0..20 {
        let mut v: Vec<u64> = Vec::new();
        for i in 0..100_000u64 {
            v.push(i);
        }
        ops += 100_000;
        if round == 0 {
            ctx.mark_peak();
        }
        black_box(&v);
    }
    ops
}

fn hashmap(ctx: &mut Ctx) -> u64 {
    let mut m: HashMap<u64, u64> = HashMap::new();
    for _ in 0..50_000 {
        let k = ctx.below(1 << 30) as u64;
        m.insert(k, k);
    }
    ctx.mark_peak();
    let before = m.len();
    m.retain(|k, _| k % 2 == 1);
    let removed = (before - m.len()) as u64;
    for _ in 0..25_000 {
        let k = ctx.below(1 << 30) as u64;
        m.insert(k, k);
    }
    black_box(&m);
    50_000 + removed + 25_000
}

fn game_frame(ctx: &mut Ctx) -> u64 {
    let (mut frame, spare) = ctx.lists();
    for f in 0..200 {
        for _ in 0..1000 {
            let l = layout(16 + ctx.below(241));
            frame.push((get(l), l));
        }
        if f == 0 {
            ctx.mark_peak();
        }
        for (p, l) in frame.drain(..) {
            put(p, l);
        }
        ctx.end_frame();
    }
    ctx.give(frame, spare);
    400_000
}
