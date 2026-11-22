//! Error type.

use std::fmt;

/// Errors returned by the container parser and the video and audio decoders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The data ended before a complete structure could be read.
    Truncated,
    /// No `KB2` signature was found.
    BadSignature,
    /// The revision letter is not one this decoder knows.
    UnsupportedRevision(u8),
    /// A container header field is out of range or inconsistent.
    InvalidHeader(&'static str),
    /// The video or audio bitstream is malformed.
    InvalidData(&'static str),
    /// A P frame was decoded without a previous frame to predict from.
    MissingReference,
    /// A frame number past the end of the frame index was requested.
    FrameOutOfRange,
    /// The data uses a feature this decoder does not implement (e.g. the Bink Audio RDFT codec).
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => write!(f, "unexpected end of data"),
            Error::BadSignature => write!(f, "not a Bink 2 file (no KB2 signature)"),
            Error::UnsupportedRevision(r) => {
                write!(f, "unsupported Bink 2 revision {:?}", *r as char)
            }
            Error::InvalidHeader(m) => write!(f, "invalid header: {m}"),
            Error::InvalidData(m) => write!(f, "invalid data: {m}"),
            Error::MissingReference => write!(f, "P frame without a reference frame"),
            Error::FrameOutOfRange => write!(f, "frame number out of range"),
            Error::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;
