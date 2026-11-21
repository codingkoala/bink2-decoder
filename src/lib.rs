//! Clean-room Bink 2 (`.bk2`) video decoder.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bits;
mod error;
#[allow(missing_docs)]
#[rustfmt::skip]
pub mod tables;
pub mod vlc;

pub use error::{Error, Result};
