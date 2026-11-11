//! Shared building blocks for the allocators in this workspace.
//!
//! Nothing in this crate allocates. It only provides the pieces every strategy
//! needs: alignment math, the memory region an allocator owns, a spinlock to
//! guard allocator state and, behind the `stats` feature, usage counters.

#![no_std]

pub mod align;
pub mod error;
pub mod region;

pub use error::AllocError;
pub use region::{Arena, Region};
