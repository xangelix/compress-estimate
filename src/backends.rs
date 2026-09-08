//! Compression-algorithm backends. Currently only zstd; the
//! [`crate::backend::Backend`] trait is the extension point.

pub mod zstd;
