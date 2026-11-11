//! Error type shared by the allocators.
//!
//! `GlobalAlloc` can only report failure as a null pointer. The allocators keep
//! a richer `Result`-based path internally and in their safe API, and collapse
//! it to null at the trait boundary.

use core::fmt;

/// Why an allocation request could not be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocError {
    /// Not enough free space left in the region.
    Exhausted,
    /// The strategy does not serve this size or alignment at all.
    Unsupported,
    /// A size or address computation overflowed `usize`.
    Overflow,
}

impl fmt::Display for AllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            AllocError::Exhausted => "region exhausted",
            AllocError::Unsupported => "request not supported by this allocator",
            AllocError::Overflow => "size computation overflowed",
        })
    }
}

impl core::error::Error for AllocError {}
