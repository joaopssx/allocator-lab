//! Allocation counters, compiled in only with the `stats` feature.
//!
//! The counters are plain integers, not atomics: each allocator keeps its
//! `Stats` inside the state already guarded by its lock, so updating them adds
//! no synchronization of its own. Arithmetic saturates because a panic inside
//! the allocator would abort the process with no useful message.

/// Counters for one allocator instance.
///
/// Byte counts are the sizes the caller asked for, not what the allocator
/// actually reserved. Comparing `peak_bytes` with the allocator's own usage
/// figures is how the benchmarks measure internal fragmentation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Successful allocations, including the new block of a moving realloc.
    pub allocs: u64,
    /// Blocks returned through `dealloc`.
    pub deallocs: u64,
    /// Calls to `realloc` that succeeded, in place or not.
    pub reallocs: u64,
    /// Requests that returned null.
    pub failures: u64,
    /// Bytes currently held by the caller.
    pub live_bytes: usize,
    /// Highest value `live_bytes` reached.
    pub peak_bytes: usize,
    /// Sum of all sizes ever requested.
    pub total_bytes: u64,
}

impl Stats {
    /// All counters at zero.
    pub const fn new() -> Self {
        Stats {
            allocs: 0,
            deallocs: 0,
            reallocs: 0,
            failures: 0,
            live_bytes: 0,
            peak_bytes: 0,
            total_bytes: 0,
        }
    }

    /// Records a successful allocation of `size` bytes.
    pub fn on_alloc(&mut self, size: usize) {
        self.allocs = self.allocs.saturating_add(1);
        self.total_bytes = self.total_bytes.saturating_add(size as u64);
        self.grow(size);
    }

    /// Records the release of a block of `size` bytes.
    pub fn on_dealloc(&mut self, size: usize) {
        self.deallocs = self.deallocs.saturating_add(1);
        self.live_bytes = self.live_bytes.saturating_sub(size);
    }

    /// Records a successful resize from `old` to `new` bytes.
    pub fn on_realloc(&mut self, old: usize, new: usize) {
        self.reallocs = self.reallocs.saturating_add(1);
        if new > old {
            self.total_bytes = self.total_bytes.saturating_add((new - old) as u64);
            self.grow(new - old);
        } else {
            self.live_bytes = self.live_bytes.saturating_sub(old - new);
        }
    }

    /// Records a request that could not be served.
    pub fn on_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    fn grow(&mut self, by: usize) {
        self.live_bytes = self.live_bytes.saturating_add(by);
        self.peak_bytes = self.peak_bytes.max(self.live_bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::Stats;

    #[test]
    fn tracks_peak_across_realloc() {
        let mut s = Stats::new();
        s.on_alloc(100);
        s.on_alloc(50);
        s.on_realloc(100, 300);
        s.on_dealloc(50);
        s.on_realloc(300, 10);

        assert_eq!(s.live_bytes, 10);
        assert_eq!(s.peak_bytes, 350);
        assert_eq!(s.total_bytes, 350);
        assert_eq!((s.allocs, s.deallocs, s.reallocs), (2, 1, 2));
    }

    #[test]
    fn dealloc_never_underflows() {
        let mut s = Stats::new();
        s.on_dealloc(10);
        assert_eq!(s.live_bytes, 0);
    }
}
