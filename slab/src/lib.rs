//! Slab allocator over a fixed [`Region`](alloc_core::Region).
//!
//! Requests are rounded up to one of eight power-of-two size classes, from 16
//! to 2048 bytes. Anything larger, or aligned to more than 2048 bytes, is not
//! served: allocation returns null.

#![no_std]

pub mod class;
