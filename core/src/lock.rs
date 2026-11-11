//! A minimal test-and-test-and-set spinlock.
//!
//! An allocator cannot use `std::sync::Mutex`: it lives in `no_std` code, and
//! a lock that might allocate or park the thread is a bad fit for the code path
//! that every allocation goes through. Critical sections here are short (a few
//! pointer updates), so spinning is acceptable.
//!
//! Known limitations, by design:
//!
//! - Not fair. Under heavy contention a thread can starve.
//! - Not reentrant. Locking twice from the same thread deadlocks. The
//!   allocators never allocate while holding their own lock, so this does not
//!   come up in practice.
//! - No poisoning. A panic while the guard is held releases the lock on unwind
//!   and leaves the state as it was at the panic point.
//! - Not safe to take from a signal handler or interrupt that may have
//!   interrupted the holder: that also deadlocks.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// Mutual exclusion through busy waiting.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the lock hands out at most one `&mut T` at a time, so sharing the
// lock between threads is fine as long as `T` itself may move between them.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Creates an unlocked spinlock.
    pub const fn new(value: T) -> Self {
        SpinLock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Spins until the lock is acquired.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            // Wait on a plain load so contended cores do not keep bouncing
            // the cache line with failed compare-exchanges.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    /// Acquires the lock if it is free, without spinning.
    pub fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        // Strong exchange on purpose: the weak one may fail spuriously, and
        // then `try_lock` would report a free lock as taken.
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinGuard { lock: self })
    }

    /// Mutable access without locking, through exclusive ownership.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }
}

/// Proof that the lock is held. Releases it on drop.
pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the guard exists only while `locked` is true and was set by
        // this guard's acquire, so no other reference to the value is live.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: same as `deref`; `&mut self` also rules out another borrow
        // through this guard.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::SpinLock;
    use std::thread;

    #[test]
    fn excludes_concurrent_writers() {
        static COUNTER: SpinLock<usize> = SpinLock::new(0);
        let iters = if cfg!(miri) { 50 } else { 10_000 };

        let handles: std::vec::Vec<_> = (0..4)
            .map(|_| {
                thread::spawn(move || {
                    for _ in 0..iters {
                        // Read and write in two steps so a missing lock would
                        // actually lose updates.
                        let mut g = COUNTER.lock();
                        let v = *g;
                        *g = v + 1;
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(*COUNTER.lock(), 4 * iters);
    }

    #[test]
    fn try_lock_fails_while_held() {
        let lock = SpinLock::new(());
        let g = lock.lock();
        assert!(lock.try_lock().is_none());
        drop(g);
        assert!(lock.try_lock().is_some());
    }
}
