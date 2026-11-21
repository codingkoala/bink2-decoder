//! Clean-room Bink 2 (`.bk2`) video decoder.
//!
//! - [`container`]: file header, audio track table, frame index and frame packets. Audio
//!   payloads are returned as bytes; Bink Audio is not decoded.
//! - [`Decoder`]: video decoding to planar 8-bit YUV 4:2:0 (+ optional alpha) for both the
//!   float variant (revisions `a`, `d`, `f`) and the integer variant (`g` and later).
//!
//! ```no_run
//! let data = std::fs::read("movie.bk2")?;
//! let file = bink2::Container::parse(&data)?;
//! let mut dec = bink2::Decoder::new(&file.header)?;
//! for i in 0..file.num_frames() {
//!     let pkt = file.frame(i)?;
//!     let frame = dec.decode(pkt.video, pkt.keyframe)?;
//!     println!("{}x{} y[0] = {}", frame.width, frame.height, frame.y[0]);
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bits;
pub mod container;
mod dsp;
pub mod edge_flags;
mod error;
mod f;
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
pub use video::{slice_boundaries, Decoder, DecoderOptions, OutOfRangeMv, SliceFailure, SliceReport};

#[cfg(test)]
mod e2e_tests;
