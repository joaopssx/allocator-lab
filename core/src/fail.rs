//! Failing from inside an allocator.
//!
//! A `GlobalAlloc` method must not unwind, so a panic is not an option when
//! an allocator notices its own state is broken. [`broken`] prints the
//! message the way a panic does and then aborts.

/// Aborts the process, reporting that an allocator invariant does not hold.
#[cold]
#[inline(never)]
pub fn broken(what: &'static str) -> ! {
    // A panic cannot leave an `extern "C"` function: it aborts at that
    // boundary instead, after the panic hook has printed the message.
    extern "C" fn stop(what: &&'static str) -> ! {
        panic!("allocator invariant broken: {what}")
    }
    stop(&what)
}
