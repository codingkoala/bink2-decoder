//! Bink 2 container (SPEC §2): file header, audio track table, frame index and frame packets.

use crate::error::{Error, Result};

/// Largest accepted frame count (SPEC §2.1).
pub const MAX_FRAMES: u32 = 1_000_000;
/// Largest accepted number of audio tracks (SPEC §2.1).
pub const MAX_AUDIO_TRACKS: u32 = 256;
/// Largest accepted width or height.
// SPEC-QUESTION: the spec gives no limit for width/height; 16384 is our own bound so that
// header values cannot request unbounded frame buffers.
pub const MAX_DIMENSION: u32 = 16384;

/// Video flag: the video has an alpha plane.
pub const VIDEO_FLAG_ALPHA: u32 = 0x0010_0000;

/// One audio track entry of the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioTrack {
    /// Maximum decoded audio size (not needed for decoding).
    pub max_decoded_size: u32,
    /// Sample rate in Hz.
    pub sample_rate: u16,
    /// Raw audio flags.
    pub flags: u16,
    /// Track id.
    pub id: u32,
}

impl AudioTrack {
    /// Flag 0x2000: stereo.
    pub fn is_stereo(&self) -> bool {
        self.flags & 0x2000 != 0
    }
    /// Flag 0x1000: DCT audio codec (RDFT otherwise).
    pub fn is_dct(&self) -> bool {
        self.flags & 0x1000 != 0
    }
    /// Flag 0x4000: "16-bit".
    pub fn is_16bit(&self) -> bool {
        self.flags & 0x4000 != 0
    }
}

/// One frame index entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameEntry {
    /// Offset of the frame relative to the `KB2` signature.
    pub offset: u32,
    /// Size of the frame in bytes.
    pub size: u32,
    /// Keyframe flag from the index (bit 0).
    pub keyframe: bool,
}

/// Parsed file header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Revision letter (`b'a'`, `b'd'`, `b'f'`, `b'g'` … `b'k'`).
    pub revision: u8,
    /// File size field (file size − 8).
    pub file_size_field: u32,
    /// Number of frames.
    pub num_frames: u32,
    /// Largest frame size in bytes.
    pub largest_frame: u32,
    /// Unknown field at 0x10.
    pub unknown_10: u32,
    /// Picture width.
    pub width: u32,
    /// Picture height.
    pub height: u32,
    /// Frame rate numerator.
    pub fps_num: u32,
    /// Frame rate denominator.
    pub fps_den: u32,
    /// Video flags.
    pub video_flags: u32,
    /// Unknown extra field of revisions `i`, `j`, `k`.
    pub extra_ijk: Option<u32>,
    /// Audio tracks.
    pub audio_tracks: Vec<AudioTrack>,
    /// Frame index.
    pub frames: Vec<FrameEntry>,
}

impl Header {
    /// True if the video has an alpha plane.
    pub fn has_alpha(&self) -> bool {
        self.video_flags & VIDEO_FLAG_ALPHA != 0
    }
    /// True for the float variant (revisions up to `f`).
    pub fn is_float_variant(&self) -> bool {
        is_float_revision(self.revision)
    }
}

/// True for revisions that use the float variant (`a`, `d`, `f`).
pub fn is_float_revision(rev: u8) -> bool {
    rev <= b'f'
}

/// True for revision letters this decoder accepts.
pub fn is_known_revision(rev: u8) -> bool {
    matches!(rev, b'a' | b'd' | b'f' | b'g' | b'h' | b'i' | b'j' | b'k')
}

/// A frame split into per-track audio payloads and the video packet (SPEC §2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FramePacket<'a> {
    /// Keyframe flag from the index.
    pub keyframe: bool,
    /// Audio payload of each track, in track order (without the `u32 size` prefix).
    pub audio: Vec<&'a [u8]>,
    /// The video packet.
    pub video: &'a [u8],
}

/// Returns the decoded size stored in the first `u32` of an audio payload, if it has one.
pub fn audio_decoded_size(payload: &[u8]) -> Option<u32> {
    if payload.len() >= 4 {
        Some(le32(payload, 0).ok()?)
    } else {
        None
    }
}

fn le32(d: &[u8], off: usize) -> Result<u32> {
    let b = d
        .get(off..off.checked_add(4).ok_or(Error::Truncated)?)
        .ok_or(Error::Truncated)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn has_signature(d: &[u8]) -> bool {
    d.len() >= 4 && &d[0..3] == b"KB2" && is_known_revision(d[3])
}

/// Finds the start of the Bink data: offset 0, or the first 512-byte boundary holding the
/// signature for SMUS-wrapped files.
// SPEC-QUESTION: SPEC §2.1 does not say how a SMUS wrapper is recognised or whether the data
// after the signature is contiguous; we scan 512-byte boundaries for `KB2` + a known revision
// and treat everything from there as one contiguous Bink file.
pub fn find_signature(data: &[u8]) -> Option<usize> {
    let mut off = 0;
    while off < data.len() {
        if has_signature(&data[off..]) {
            return Some(off);
        }
        off += 512;
    }
    None
}

/// A parsed container borrowing the file bytes.
#[derive(Debug, Clone)]
pub struct Container<'a> {
    /// The file bytes starting at the `KB2` signature.
    data: &'a [u8],
    /// Offset of the signature in the input (non-zero for SMUS-wrapped files).
    pub base_offset: usize,
    /// The parsed header.
    pub header: Header,
}

impl<'a> Container<'a> {
    /// Parses the header and frame index of a whole file held in memory.
    pub fn parse(file: &'a [u8]) -> Result<Container<'a>> {
        let base = find_signature(file).ok_or_else(|| {
            if file.len() >= 4 && &file[0..3] == b"KB2" {
                Error::UnsupportedRevision(file[3])
            } else {
                Error::BadSignature
            }
        })?;
        let d = &file[base..];
        let revision = d[3];
        let file_size_field = le32(d, 0x04)?;
        let num_frames = le32(d, 0x08)?;
        let largest_frame = le32(d, 0x0C)?;
        let unknown_10 = le32(d, 0x10)?;
        let width = le32(d, 0x14)?;
        let height = le32(d, 0x18)?;
        let fps_num = le32(d, 0x1C)?;
        let fps_den = le32(d, 0x20)?;
        let video_flags = le32(d, 0x24)?;
        let n_audio = le32(d, 0x28)?;
        if num_frames > MAX_FRAMES {
            return Err(Error::InvalidHeader("too many frames"));
        }
        if n_audio > MAX_AUDIO_TRACKS {
            return Err(Error::InvalidHeader("too many audio tracks"));
        }
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(Error::InvalidHeader("bad picture size"));
        }
        if fps_num == 0 || fps_den == 0 {
            return Err(Error::InvalidHeader("zero frame rate"));
        }
        let mut off = 0x2C;
        let extra_ijk = if matches!(revision, b'i' | b'j' | b'k') {
            let v = le32(d, off)?;
            off += 4;
            Some(v)
        } else {
            None
        };
        let n = n_audio as usize;
        // Check the whole table fits before allocating.
        let tables_end = off + 12 * n + 4 * num_frames as usize;
        if tables_end > d.len() {
            return Err(Error::Truncated);
        }
        let mut audio_tracks = Vec::with_capacity(n);
        for i in 0..n {
            let max_decoded_size = le32(d, off + 4 * i)?;
            let rf = le32(d, off + 4 * n + 4 * i)?;
            let id = le32(d, off + 8 * n + 4 * i)?;
            audio_tracks.push(AudioTrack {
                max_decoded_size,
                sample_rate: (rf & 0xFFFF) as u16,
                flags: (rf >> 16) as u16,
                id,
            });
        }
        off += 12 * n;
        let end = file_size_field as u64 + 8;
        let mut frames = Vec::with_capacity(num_frames as usize);
        let mut prev: Option<u32> = None;
        for i in 0..num_frames as usize {
            let v = le32(d, off + 4 * i)?;
            let o = v & !1;
            if (o as usize) < tables_end {
                return Err(Error::InvalidHeader("frame offset inside the header"));
            }
            if let Some(p) = prev {
                if o <= p {
                    return Err(Error::InvalidHeader("frame offsets not increasing"));
                }
            }
            prev = Some(o);
            frames.push(FrameEntry {
                offset: o,
                size: 0,
                keyframe: v & 1 != 0,
            });
        }
        for i in 0..frames.len() {
            let next = if i + 1 < frames.len() {
                frames[i + 1].offset as u64
            } else {
                end
            };
            if next < frames[i].offset as u64 {
                return Err(Error::InvalidHeader("frame offset past the end of the file"));
            }
            frames[i].size = (next - frames[i].offset as u64) as u32;
        }
        Ok(Container {
            data: d,
            base_offset: base,
            header: Header {
                revision,
                file_size_field,
                num_frames,
                largest_frame,
                unknown_10,
                width,
                height,
                fps_num,
                fps_den,
                video_flags,
                extra_ijk,
                audio_tracks,
                frames,
            },
        })
    }

    /// Number of frames.
    pub fn num_frames(&self) -> usize {
        self.header.frames.len()
    }

    /// Raw bytes of frame `i`.
    pub fn frame_bytes(&self, i: usize) -> Result<&'a [u8]> {
        let e = self.header.frames.get(i).ok_or(Error::FrameOutOfRange)?;
        let start = e.offset as usize;
        let end = start + e.size as usize;
        self.data.get(start..end).ok_or(Error::Truncated)
    }

    /// Frame `i` split into audio payloads and the video packet.
    pub fn frame(&self, i: usize) -> Result<FramePacket<'a>> {
        let keyframe = self.header.frames.get(i).ok_or(Error::FrameOutOfRange)?.keyframe;
        let bytes = self.frame_bytes(i)?;
        split_frame(bytes, self.header.audio_tracks.len(), keyframe)
    }
}

/// Splits a frame into `n_audio` audio payloads and the video packet (SPEC §2.2).
pub fn split_frame(bytes: &[u8], n_audio: usize, keyframe: bool) -> Result<FramePacket<'_>> {
    let mut pos = 0usize;
    let mut audio = Vec::with_capacity(n_audio.min(MAX_AUDIO_TRACKS as usize));
    for _ in 0..n_audio {
        let size = le32(bytes, pos)? as usize;
        pos += 4;
        let payload = bytes
            .get(pos..pos.checked_add(size).ok_or(Error::Truncated)?)
            .ok_or(Error::Truncated)?;
        audio.push(payload);
        pos += size;
    }
    Ok(FramePacket {
        keyframe,
        audio,
        video: &bytes[pos..],
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Builds a synthetic file. `frames` are the raw frame bodies, `key` their keyframe flags.
    pub fn build_file(
        rev: u8,
        width: u32,
        height: u32,
        video_flags: u32,
        tracks: &[(u16, u16, u32)],
        frames: &[(bool, Vec<u8>)],
    ) -> Vec<u8> {
        let mut h = Vec::new();
        let n = tracks.len();
        let mut header_len = 0x2C + 12 * n + 4 * frames.len();
        if matches!(rev, b'i' | b'j' | b'k') {
            header_len += 4;
        }
        let total: usize = header_len + frames.iter().map(|f| f.1.len()).sum::<usize>();
        let largest = frames.iter().map(|f| f.1.len()).max().unwrap_or(0) as u32;
        h.extend_from_slice(b"KB2");
        h.push(rev);
        for v in [
            (total - 8) as u32,
            frames.len() as u32,
            largest,
            0,
            width,
            height,
            30,
            1,
            video_flags,
            n as u32,
        ] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        if matches!(rev, b'i' | b'j' | b'k') {
            h.extend_from_slice(&0xDEADu32.to_le_bytes());
        }
        for _ in tracks {
            h.extend_from_slice(&4096u32.to_le_bytes());
        }
        for t in tracks {
            h.extend_from_slice(&t.0.to_le_bytes());
            h.extend_from_slice(&t.1.to_le_bytes());
        }
        for t in tracks {
            h.extend_from_slice(&t.2.to_le_bytes());
        }
        let mut off = header_len as u32;
        for f in frames {
            h.extend_from_slice(&(off | f.0 as u32).to_le_bytes());
            off += f.1.len() as u32;
        }
        assert_eq!(h.len(), header_len);
        for f in frames {
            h.extend_from_slice(&f.1);
        }
        h
    }

    fn frame_with_audio(audio: &[&[u8]], video: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        for a in audio {
            v.extend_from_slice(&(a.len() as u32).to_le_bytes());
            v.extend_from_slice(a);
        }
        v.extend_from_slice(video);
        v
    }

    #[test]
    fn parse_synthetic_header() {
        let tracks = [(44100u16, 0x2000u16, 7u32), (22050, 0x1000, 9)];
        let f0 = frame_with_audio(&[&[8, 0, 0, 0, 1, 2], &[]], &[0xAA, 0xBB]);
        let f1 = frame_with_audio(&[&[1, 2], &[3]], &[0xCC]);
        let file = build_file(b'j', 100, 50, VIDEO_FLAG_ALPHA | 2, &tracks, &[(true, f0), (false, f1)]);
        let c = Container::parse(&file).unwrap();
        let h = &c.header;
        assert_eq!(h.revision, b'j');
        assert_eq!((h.width, h.height, h.fps_num, h.fps_den), (100, 50, 30, 1));
        assert!(h.has_alpha());
        assert!(!h.is_float_variant());
        assert_eq!(h.extra_ijk, Some(0xDEAD));
        assert_eq!(h.audio_tracks.len(), 2);
        assert_eq!(h.audio_tracks[0].sample_rate, 44100);
        assert!(h.audio_tracks[0].is_stereo() && !h.audio_tracks[0].is_dct());
        assert!(h.audio_tracks[1].is_dct() && !h.audio_tracks[1].is_stereo());
        assert_eq!(h.audio_tracks[1].id, 9);
        assert_eq!(c.num_frames(), 2);
        let p0 = c.frame(0).unwrap();
        assert!(p0.keyframe);
        assert_eq!(p0.audio, vec![&[8u8, 0, 0, 0, 1, 2][..], &[][..]]);
        assert_eq!(audio_decoded_size(p0.audio[0]), Some(8));
        assert_eq!(p0.video, &[0xAA, 0xBB]);
        let p1 = c.frame(1).unwrap();
        assert!(!p1.keyframe);
        assert_eq!(p1.video, &[0xCC]);
        assert_eq!(c.frame(2), Err(Error::FrameOutOfRange));
    }

    #[test]
    fn revision_g_has_no_extra_field_and_smus_offset() {
        let file = build_file(b'g', 32, 32, 0, &[], &[(true, vec![1, 2, 3])]);
        let mut wrapped = b"SMUS".to_vec();
        wrapped.resize(1024, 0);
        wrapped.extend_from_slice(&file);
        let c = Container::parse(&wrapped).unwrap();
        assert_eq!(c.base_offset, 1024);
        assert_eq!(c.header.extra_ijk, None);
        assert_eq!(c.frame(0).unwrap().video, &[1, 2, 3]);
    }

    #[test]
    fn rejects_bad_headers() {
        assert_eq!(Container::parse(b"RIFF....").unwrap_err(), Error::BadSignature);
        let mut f = build_file(b'g', 32, 32, 0, &[], &[(true, vec![1])]);
        f[3] = b'z';
        assert_eq!(Container::parse(&f).unwrap_err(), Error::UnsupportedRevision(b'z'));
        let mut f = build_file(b'g', 32, 32, 0, &[], &[(true, vec![1])]);
        f[8..12].copy_from_slice(&2_000_000u32.to_le_bytes());
        assert!(matches!(Container::parse(&f), Err(Error::InvalidHeader(_))));
        let mut f = build_file(b'g', 32, 32, 0, &[], &[(true, vec![1])]);
        f[0x28..0x2C].copy_from_slice(&300u32.to_le_bytes());
        assert!(matches!(Container::parse(&f), Err(Error::InvalidHeader(_))));
        let mut f = build_file(b'g', 32, 32, 0, &[], &[(true, vec![1])]);
        f[0x20..0x24].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(Container::parse(&f), Err(Error::InvalidHeader(_))));
        // non-increasing offsets
        let mut f = build_file(b'g', 32, 32, 0, &[], &[(true, vec![1]), (false, vec![2])]);
        let o0 = u32::from_le_bytes(f[0x2C..0x30].try_into().unwrap());
        f[0x30..0x34].copy_from_slice(&o0.to_le_bytes());
        assert!(matches!(Container::parse(&f), Err(Error::InvalidHeader(_))));
        // truncated index
        let f = build_file(b'g', 32, 32, 0, &[], &[(true, vec![1])]);
        assert_eq!(Container::parse(&f[..0x2E]).unwrap_err(), Error::Truncated);
        // audio size larger than the frame
        assert_eq!(split_frame(&[9, 0, 0, 0, 1], 1, true).unwrap_err(), Error::Truncated);
        // truncated file: frame data missing
        let f = build_file(b'g', 32, 32, 0, &[], &[(true, vec![1, 2, 3, 4])]);
        let c = Container::parse(&f[..f.len() - 2]).unwrap();
        assert_eq!(c.frame(0).unwrap_err(), Error::Truncated);
    }
}
