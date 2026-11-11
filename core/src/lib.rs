//! Shared building blocks for the allocators in this workspace.
//!
//! Nothing in this crate allocates. It only provides the pieces every strategy
//! needs: alignment math, the memory region an allocator owns, a spinlock to
//! guard allocator state and, behind the `stats` feature, usage counters.

#![no_std]

pub mod align;
pub mod error;
pub mod fail;
pub mod lock;
pub mod region;
#[cfg(feature = "stats")]
pub mod stats;

pub use error::AllocError;
pub use fail::broken;
pub use lock::SpinLock;
pub use region::{Arena, Region};
#[cfg(feature = "stats")]
pub use stats::Stats;
