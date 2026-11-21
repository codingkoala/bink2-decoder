//! Clean-room Bink 2 (`.bk2`) video decoder.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bits;
pub mod container;
mod dsp;
pub mod edge_flags;
mod error;
mod frame;
mod g;
#[allow(missing_docs)]
#[rustfmt::skip]
pub mod tables;
mod video;
pub mod vlc;

pub use container::{AudioTrack, Container, FrameEntry, FramePacket, Header};
pub use error::{Error, Result};
pub use frame::{VideoFrame, MARGIN};
pub use video::{slice_boundaries, Decoder, DecoderOptions, OutOfRangeMv};

#[cfg(test)]
mod e2e_tests;
