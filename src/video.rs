//! Video packet decoding shared by both variants (SPEC §3, §4.6–§4.8).

use crate::bits::BitReader;
use crate::container::{is_float_revision, is_known_revision, Header, MAX_HEIGHT, MAX_WIDTH};
use crate::dsp::{block_dc_from_pixels, ceil_to, clamp, copy_block, mc_chroma, mc_luma};
use crate::edge_flags::read_edge_flags;
use crate::error::{Error, Result};
use crate::frame::{FrameBuf, Plane, VideoFrame};
use crate::tables::ZORDER;

/// Block (macroblock) types.
pub(crate) const INTRA: u8 = 0;
pub(crate) const SKIP: u8 = 1;
pub(crate) const MOTION: u8 = 2;
pub(crate) const RESIDUE: u8 = 3;

/// A motion vector (x, y).
pub(crate) type Mv = [i32; 2];

/// What to do when a motion vector points outside the picture (SPEC §4.7, open question 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutOfRangeMv {
    /// Leave the destination block as a copy of the co-located reference block.
    // SPEC-QUESTION: the reference writes nothing for such blocks (undefined content). The
    // default here copies the co-located reference block instead.
    #[default]
    CopyColocated,
    /// Clamp the source top-left into the picture and predict from there.
    ClampToPicture,
}

/// Where the inferred deblocking filter (docs/INFERRED.md) is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeblockMode {
    /// No filter (the behaviour described by SPEC.md).
    #[default]
    Off,
    /// Filter the output only; references stay unfiltered.
    Post,
    /// Filter the frame before it is output and used as the next reference.
    InLoop,
}

/// Parameters of the inferred deblocking filter. Not part of SPEC.md: a black-box model fitted
/// to RAD's own output, see docs/INFERRED.md. The defaults (other than `mode`) are the best fit
/// found: INTRA MBs only, luma and chroma, edge flags respected, no step threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deblock {
    /// Where the filter runs.
    pub mode: DeblockMode,
    /// Also filter the chroma planes (edges every 8 chroma pixels, using the luma flags of
    /// the co-located 16-pixel luma edge).
    pub chroma: bool,
    /// Filter every edge when the packet carries no edge flags.
    pub without_flags: bool,
    /// Leave edges whose step |q0 - p0| is at least this large.
    pub max_step: i32,
    /// Filter horizontal edges before vertical ones.
    pub horizontal_first: bool,
    /// Bit t set: edges are filtered in MBs of type t (bit 0 INTRA … bit 3 RESIDUE). The MB is
    /// the one holding the pixel right of / below the edge.
    pub types: u8,
}

impl Default for Deblock {
    fn default() -> Self {
        Deblock {
            mode: DeblockMode::Off,
            chroma: true,
            without_flags: false,
            max_step: 256,
            horizontal_first: false,
            types: 1,
        }
    }
}

/// Decoder options for the unverified behaviours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecoderOptions {
    /// Out-of-picture motion vector handling.
    pub out_of_range_mv: OutOfRangeMv,
    /// When a slice fails to decode, conceal the rest of it (copy from the previous frame, or
    /// mid-grey without one) and carry on with the next slice instead of failing the frame.
    /// The failure is still recorded in [`Decoder::slice_reports`].
    pub conceal_slice_errors: bool,
    /// Inferred deblocking filter (off by default).
    pub deblock: Deblock,
}

/// Side information about one decoded macroblock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MbInfo {
    /// Block type: 0 INTRA, 1 SKIP, 2 MOTION, 3 RESIDUE.
    pub mb_type: u8,
    /// Intra q (integer variant; 0 in the float variant).
    pub intra_q: i32,
    /// Inter q (integer variant; 0 in the float variant).
    pub inter_q: i32,
}

/// Where and why a slice failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceFailure {
    /// MB column and row (in MB units, frame coordinates) being decoded when the error occurred.
    pub mb: (usize, usize),
    /// Bit position in the video packet when the error was returned.
    pub bit: usize,
    /// The error.
    pub error: Error,
}

/// Bit accounting for one slice of the last decoded packet. Useful to check that the decoder
/// stays in sync with the stream: a correctly parsed slice ends at (or a few padding bytes
/// before) `end_byte`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceReport {
    /// Slice number.
    pub index: usize,
    /// MB rows `[first, end)`.
    pub mb_rows: (usize, usize),
    /// Bit position where the slice data starts (slice 0 starts after the slice table and the
    /// edge flags, so it need not be byte-aligned).
    pub start_bit: usize,
    /// Byte offset where the next slice starts, from the slice table.
    pub end_byte: usize,
    /// Bit position after the slice was decoded (before `align()`).
    pub consumed_bit: usize,
    /// Set if the slice failed.
    pub failure: Option<SliceFailure>,
}

impl SliceReport {
    /// True if the slice decoded without error and ended within the 0–3 padding bytes before
    /// `end_byte` (slices are observed to be padded to 4 bytes).
    pub fn in_sync(&self) -> bool {
        let end = self.consumed_bit.div_ceil(8);
        self.failure.is_none() && end <= self.end_byte && self.end_byte - end <= 3
    }
}

/// Slice boundaries in luma rows (SPEC §3.2). `boundary[S-1]` is the 32-aligned height.
pub fn slice_boundaries(revision: u8, height: u32, video_flags: u32) -> Vec<u32> {
    let h32 = ceil_to(height, 32);
    let two = |h: u32| vec![(h / 2 + 16) & !31, h32];
    if revision <= b'f' {
        return two(height);
    }
    if revision == b'g' {
        return if h32 < 128 { vec![h32] } else { two(height) };
    }
    let s = [2u32, 3, 4, 8][(video_flags & 3) as usize];
    let mut b = Vec::with_capacity(s as usize);
    let mut start: i64 = 0;
    let mut end: i64 = h32 as i64 + 32 * s as i64 - 1;
    for i in 0..s - 1 {
        start += ((end - start) / (s - i) as i64) & !31;
        end -= 32;
        b.push(start as u32);
    }
    b.push(h32);
    b
}

/// Parsed video packet header.
#[derive(Debug, Clone)]
pub(crate) struct PacketHeader {
    pub frame_flags: u32,
    /// Slice row ranges in MB rows `[first, end)`.
    pub slice_rows: Vec<(usize, usize)>,
    /// Byte offset where each slice ends.
    pub slice_end: Vec<usize>,
    /// Row and column edge flag lists, when present.
    pub row_flags: Option<Vec<bool>>,
    pub col_flags: Option<Vec<bool>>,
}

fn le32(d: &[u8], off: usize) -> Result<u32> {
    let b = d.get(off..off + 4).ok_or(Error::Truncated)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Per-MB state that the DC prediction and refresh need, for both variants.
pub(crate) trait DcNum: Copy + PartialOrd + std::ops::Add<Output = Self> + std::ops::Sub<Output = Self> {
    const ZERO: Self;
    const INTRA_DEFAULT: Self;
    fn from_sum(v: i32) -> Self;
}
impl DcNum for i32 {
    const ZERO: i32 = 0;
    const INTRA_DEFAULT: i32 = 1024;
    fn from_sum(v: i32) -> i32 {
        v
    }
}
impl DcNum for f32 {
    const ZERO: f32 = 0.0;
    const INTRA_DEFAULT: f32 = 1024.0;
    fn from_sum(v: i32) -> f32 {
        v as f32
    }
}

/// Neighbour DC arrays of the same plane (None = unavailable).
#[derive(Clone, Copy)]
pub(crate) struct DcNeighbours<T> {
    pub left: Option<[T; 16]>,
    pub top: Option<[T; 16]>,
    pub top_left: Option<[T; 16]>,
}

impl<T> DcNeighbours<T> {
    pub fn none() -> Self {
        DcNeighbours {
            left: None,
            top: None,
            top_left: None,
        }
    }
}

const INV_ZORDER: [usize; 16] = {
    let mut inv = [0usize; 16];
    let mut k = 0;
    while k < 16 {
        inv[ZORDER[k] as usize] = k;
        k += 1;
    }
    inv
};

/// DC prediction (SPEC §4.8). `r` holds the decoded residuals for `n` = 16 (luma/alpha,
/// Z-order) or 4 (chroma, raster) blocks; the predicted and clamped DCs go to `out`.
pub(crate) fn predict_dc<T: DcNum>(
    n: usize,
    r: &[T; 16],
    nb: &DcNeighbours<T>,
    intra: bool,
    lo: T,
    hi: T,
    out: &mut [T; 16],
) {
    let g: i32 = if n == 16 { 4 } else { 2 };
    let idx = |row: i32, col: i32| -> usize {
        if g == 4 {
            INV_ZORDER[(row * 4 + col) as usize]
        } else {
            (row * 2 + col) as usize
        }
    };
    for k in 0..n {
        let (row, col) = if g == 4 {
            ((ZORDER[k] >> 2) as i32, (ZORDER[k] & 3) as i32)
        } else {
            ((k >> 1) as i32, (k & 1) as i32)
        };
        let get = |row: i32, col: i32, out: &[T; 16]| -> Option<T> {
            match (row < 0, col < 0) {
                (false, false) => Some(out[idx(row, col)]),
                (true, true) => nb.top_left.map(|a| a[idx(g - 1, g - 1)]),
                (true, false) => nb.top.map(|a| a[idx(g - 1, col)]),
                (false, true) => nb.left.map(|a| a[idx(row, g - 1)]),
            }
        };
        let l = get(row, col - 1, out);
        let t = get(row - 1, col, out);
        let p = match (l, t) {
            (Some(l), Some(t)) => {
                // TL is always available when L and T are; fall back to L defensively.
                let tl = get(row - 1, col - 1, out).unwrap_or(l);
                let mn = min3(l, t, tl);
                let mx = max3(l, t, tl);
                clamp(l + t - tl, mn, mx)
            }
            (Some(l), None) => l,
            (None, Some(t)) => t,
            (None, None) => {
                if intra {
                    T::INTRA_DEFAULT
                } else {
                    T::ZERO
                }
            }
        };
        out[k] = clamp(p + r[k], lo, hi);
    }
}

fn min3<T: PartialOrd + Copy>(a: T, b: T, c: T) -> T {
    let m = if b < a { b } else { a };
    if c < m {
        c
    } else {
        m
    }
}
fn max3<T: PartialOrd + Copy>(a: T, b: T, c: T) -> T {
    let m = if b > a { b } else { a };
    if c > m {
        c
    } else {
        m
    }
}

/// Recomputes the DCs of MB (mbx, mby) from the current frame's pixels for the first
/// `nplanes` planes (SPEC §4.8 neighbour refresh).
pub(crate) fn mb_dcs_from_pixels<T: DcNum>(
    f: &FrameBuf,
    mbx: usize,
    mby: usize,
    nplanes: usize,
    dc: &mut [[T; 16]; 4],
) {
    for (p, plane) in f.planes.iter().enumerate().take(nplanes) {
        if is_chroma(p) {
            for (k, d) in dc[p].iter_mut().enumerate().take(4) {
                let x = 16 * mbx + 8 * (k & 1);
                let y = 16 * mby + 8 * (k >> 1);
                *d = T::from_sum(block_dc_from_pixels(plane, x, y));
            }
        } else {
            for k in 0..16 {
                let z = ZORDER[k] as usize;
                let x = 32 * mbx + 8 * (z & 3);
                let y = 32 * mby + 8 * (z >> 2);
                dc[p][k] = T::from_sum(block_dc_from_pixels(plane, x, y));
            }
        }
    }
}

/// Stored per-MB state that the neighbour refresh needs.
pub(crate) trait StoredMb {
    type T: DcNum;
    fn ty(&self) -> u8;
    fn dc_mut(&mut self) -> &mut [[Self::T; 16]; 4];
}

/// Neighbour refresh before an INTRA MB (SPEC §4.8): recomputes the DCs of the top-left, left,
/// top-right (left edge only) and top neighbours whose type is not INTRA.
#[allow(clippy::too_many_arguments)]
pub(crate) fn refresh_neighbours<M: StoredMb>(
    f: &FrameBuf,
    cur: &mut [M],
    prev: &mut [M],
    mbx: usize,
    mby: usize,
    top_edge: bool,
    nplanes: usize,
) {
    let left_edge = mbx == 0;
    let mb_w = cur.len();
    let mut todo: [Option<(bool, usize)>; 4] = [None; 4]; // (in previous row, column)
    if !top_edge && !left_edge {
        todo[0] = Some((true, mbx - 1));
    }
    if !left_edge {
        todo[1] = Some((false, mbx - 1));
    }
    if left_edge && !top_edge && mbx + 1 < mb_w {
        todo[2] = Some((true, mbx + 1));
    }
    if !top_edge {
        todo[3] = Some((true, mbx));
    }
    for (in_prev, col) in todo.into_iter().flatten() {
        let (row, y) = if in_prev {
            (&mut *prev, mby.wrapping_sub(1))
        } else {
            (&mut *cur, mby)
        };
        let Some(mb) = row.get_mut(col) else { continue };
        if mb.ty() != INTRA {
            mb_dcs_from_pixels(f, col, y, nplanes, mb.dc_mut());
        }
    }
}

/// Plane index 1 and 2 are chroma (C1, C2); 0 is luma, 3 alpha.
#[inline]
pub(crate) fn is_chroma(p: usize) -> bool {
    p == 1 || p == 2
}

/// Pixel origin of block k of plane p in MB (mbx, mby).
#[inline]
pub(crate) fn block_origin(p: usize, k: usize, mbx: usize, mby: usize) -> (usize, usize) {
    if is_chroma(p) {
        (16 * mbx + 8 * (k & 1), 16 * mby + 8 * (k >> 1))
    } else {
        let z = ZORDER[k] as usize;
        (32 * mbx + 8 * (z & 3), 32 * mby + 8 * (z >> 2))
    }
}

/// Frame-level decoding context shared by both variants.
pub(crate) struct FrameCtx<'a> {
    pub cur: &'a mut FrameBuf,
    pub refr: Option<&'a FrameBuf>,
    pub keyframe: bool,
    pub frame_flags: u32,
    pub has_alpha: bool,
    pub pic_w: i64,
    pub pic_h: i64,
    pub mb_w: usize,
    pub opts: DecoderOptions,
    /// Set by the slice decoders to the MB that failed.
    pub fail_mb: Option<(usize, usize)>,
    /// Per-MB side information, raster order.
    pub mb_info: &'a mut [MbInfo],
}

impl FrameCtx<'_> {
    /// SKIP: copies the co-located MB from the reference for all planes.
    pub fn copy_mb(&mut self, mbx: usize, mby: usize) -> Result<()> {
        let r = self.refr.ok_or(Error::MissingReference)?;
        for (p, (dst, src)) in self.cur.planes.iter_mut().zip(r.planes.iter()).enumerate() {
            let n = if is_chroma(p) { 16 } else { 32 };
            copy_block(dst, n * mbx, n * mby, src, (n * mbx) as i64, (n * mby) as i64, n);
        }
        Ok(())
    }

    /// Copies one plane's co-located MB from the reference.
    pub fn copy_mb_plane(&mut self, p: usize, mbx: usize, mby: usize) -> Result<()> {
        let r = self.refr.ok_or(Error::MissingReference)?;
        let n = if is_chroma(p) { 16 } else { 32 };
        if let (Some(dst), Some(src)) = (self.cur.planes.get_mut(p), r.planes.get(p)) {
            copy_block(dst, n * mbx, n * mby, src, (n * mbx) as i64, (n * mby) as i64, n);
        }
        Ok(())
    }

    /// Motion-compensates the planes listed in `planes` of MB (mbx, mby) (SPEC §4.7).
    pub fn mc_mb(&mut self, mbx: usize, mby: usize, mv: &[Mv; 4], planes: &[usize]) -> Result<()> {
        let r = self.refr.ok_or(Error::MissingReference)?;
        let policy = self.opts.out_of_range_mv;
        for &p in planes {
            let (Some(dst), Some(src)) = (self.cur.planes.get_mut(p), r.planes.get(p)) else {
                continue;
            };
            let chroma = is_chroma(p);
            for (j, v) in mv.iter().enumerate() {
                let (bs, x, y) = if chroma {
                    (8, 16 * mbx + 8 * (j & 1), 16 * mby + 8 * (j >> 1))
                } else {
                    (16, 32 * mbx + 16 * (j & 1), 32 * mby + 16 * (j >> 1))
                };
                let (w, h, sx, sy, fx, fy) = if chroma {
                    (
                        self.pic_w / 2,
                        self.pic_h / 2,
                        x as i64 + (v[0] >> 2) as i64,
                        y as i64 + (v[1] >> 2) as i64,
                        v[0] & 3,
                        v[1] & 3,
                    )
                } else {
                    (
                        self.pic_w,
                        self.pic_h,
                        x as i64 + (v[0] >> 1) as i64,
                        y as i64 + (v[1] >> 1) as i64,
                        v[0] & 1,
                        v[1] & 1,
                    )
                };
                mc_block(dst, src, x, y, bs, sx, sy, fx, fy, w, h, chroma, policy);
            }
        }
        Ok(())
    }
}

/// Conceals MBs from `from` (inclusive) to the end of MB row `end_row` (exclusive): copies the
/// co-located MB from the reference, or fills mid-grey without one.
fn conceal(ctx: &mut FrameCtx, from: (usize, usize), end_row: usize) {
    let (mut x, mut y) = from;
    while y < end_row {
        if ctx.copy_mb(x, y).is_err() {
            for (p, plane) in ctx.cur.planes.iter_mut().enumerate() {
                let n = if is_chroma(p) { 16 } else { 32 };
                for r in 0..n {
                    let i = plane.idx((n * x) as i64, (n * y + r) as i64);
                    plane.data[i..i + n].fill(if p == 3 { 255 } else { 128 });
                }
            }
        }
        x += 1;
        if x == ctx.mb_w {
            x = 0;
            y += 1;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn mc_block(
    dst: &mut Plane,
    src: &Plane,
    x: usize,
    y: usize,
    bs: usize,
    mut sx: i64,
    mut sy: i64,
    fx: i32,
    fy: i32,
    w: i64,
    h: i64,
    chroma: bool,
    policy: OutOfRangeMv,
) {
    // W and H are the visible picture size, not the 32-aligned size (SPEC §4.7), so even
    // zero-vector blocks starting in the padding right of or below the picture are outside.
    let outside = sx < 0 || sx >= w || sy < 0 || sy >= h;
    if outside {
        match policy {
            OutOfRangeMv::CopyColocated => {
                copy_block(dst, x, y, src, x as i64, y as i64, bs);
                return;
            }
            OutOfRangeMv::ClampToPicture => {
                sx = clamp(sx, 0, (w - 1).max(0));
                sy = clamp(sy, 0, (h - 1).max(0));
            }
        }
    }
    let ok = if chroma {
        mc_chroma(dst, x, y, src, sx, sy, fx, fy)
    } else {
        mc_luma(dst, x, y, src, sx, sy, fx, fy)
    };
    if !ok {
        copy_block(dst, x, y, src, x as i64, y as i64, bs);
    }
}

/// Stateful Bink 2 video decoder.
#[derive(Debug, Clone)]
pub struct Decoder {
    revision: u8,
    width: usize,
    height: usize,
    video_flags: u32,
    has_alpha: bool,
    aligned_w: usize,
    aligned_h: usize,
    opts: DecoderOptions,
    reference: Option<FrameBuf>,
    spare: Option<FrameBuf>,
    reports: Vec<SliceReport>,
    last_frame_flags: u32,
    last_edge_flags: (Option<Vec<bool>>, Option<Vec<bool>>),
    mb_info: Vec<MbInfo>,
}

impl Decoder {
    /// Creates a decoder for the video described by a container header.
    pub fn new(header: &Header) -> Result<Decoder> {
        Decoder::with_params(header.revision, header.width, header.height, header.video_flags)
    }

    /// Creates a decoder from the raw stream parameters.
    pub fn with_params(revision: u8, width: u32, height: u32, video_flags: u32) -> Result<Decoder> {
        if !is_known_revision(revision) {
            return Err(Error::UnsupportedRevision(revision));
        }
        if !(1..=MAX_WIDTH).contains(&width) || !(1..=MAX_HEIGHT).contains(&height) {
            return Err(Error::InvalidHeader("bad picture size"));
        }
        Ok(Decoder {
            revision,
            width: width as usize,
            height: height as usize,
            video_flags,
            has_alpha: video_flags & crate::container::VIDEO_FLAG_ALPHA != 0,
            aligned_w: ceil_to(width, 32) as usize,
            aligned_h: ceil_to(height, 32) as usize,
            opts: DecoderOptions::default(),
            reference: None,
            spare: None,
            reports: Vec::new(),
            last_frame_flags: 0,
            last_edge_flags: (None, None),
            mb_info: Vec::new(),
        })
    }

    /// Sets the options for unverified behaviours.
    pub fn set_options(&mut self, opts: DecoderOptions) {
        self.opts = opts;
    }

    /// True if the video has an alpha plane.
    pub fn has_alpha(&self) -> bool {
        self.has_alpha
    }

    /// `frame_flags` of the last packet whose header parsed.
    pub fn last_frame_flags(&self) -> u32 {
        self.last_frame_flags
    }

    /// Row and column edge flag lists (SPEC §3.3) of the last packet, when present.
    pub fn last_edge_flags(&self) -> (Option<&[bool]>, Option<&[bool]>) {
        (self.last_edge_flags.0.as_deref(), self.last_edge_flags.1.as_deref())
    }

    /// Per-MB side information of the last decoded frame, raster order (`ceil(w/32)` per row).
    pub fn last_mb_info(&self) -> &[MbInfo] {
        &self.mb_info
    }

    /// Per-slice bit accounting of the last [`Decoder::decode`] call.
    pub fn slice_reports(&self) -> &[SliceReport] {
        &self.reports
    }

    /// Replaces the reference frame with the given planar 4:2:0 picture (`width × height` luma,
    /// `ceil(w/2) × ceil(h/2)` chroma, Cb then Cr). Analysis hook: lets a P frame be decoded
    /// against an externally supplied reference, e.g. to measure one-step errors.
    #[doc(hidden)]
    pub fn debug_set_reference_yuv(&mut self, y: &[u8], u: &[u8], v: &[u8]) {
        let mut f = FrameBuf::new(self.aligned_w, self.aligned_h, self.has_alpha);
        let (w, h) = (self.width, self.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let put = |p: &mut Plane, src: &[u8], sw: usize, sh: usize| {
            for yy in 0..p.h {
                for xx in 0..p.w {
                    let sx = xx.min(sw - 1);
                    let sy = yy.min(sh - 1);
                    let i = p.idx(xx as i64, yy as i64);
                    p.data[i] = src.get(sy * sw + sx).copied().unwrap_or(128);
                }
            }
        };
        put(&mut f.planes[0], y, w, h);
        put(&mut f.planes[1], v, cw, ch);
        put(&mut f.planes[2], u, cw, ch);
        f.extend_edges();
        self.reference = Some(f);
    }

    /// Drops the reference frame (e.g. before seeking to a keyframe).
    pub fn reset(&mut self) {
        self.reference = None;
    }

    fn parse_packet_header(&self, packet: &[u8], br: &mut BitReader) -> Result<PacketHeader> {
        let frame_flags = le32(packet, 0)?;
        let bounds = slice_boundaries(self.revision, self.height as u32, self.video_flags);
        let s = bounds.len();
        if packet.len() < 4 * s {
            return Err(Error::Truncated);
        }
        let mut slice_end = Vec::with_capacity(s);
        let mut prev = 4 * s;
        for i in 0..s {
            let e = if i + 1 < s {
                le32(packet, 4 + 4 * i)? as usize
            } else {
                packet.len()
            };
            if e < prev || e > packet.len() {
                return Err(Error::InvalidData("bad slice offset"));
            }
            prev = e;
            slice_end.push(e);
        }
        let mut slice_rows = Vec::with_capacity(s);
        let mut start = 0u32;
        for &b in &bounds {
            if b < start || b as usize > self.aligned_h {
                return Err(Error::InvalidData("bad slice boundary"));
            }
            slice_rows.push(((start / 32) as usize, (b / 32) as usize));
            start = b;
        }
        br.seek_byte(4 * s)?;
        let (mut row_flags, mut col_flags) = (None, None);
        if frame_flags & 0x10000 != 0 {
            // Edge flags (SPEC §3.2 step 5, open question 1).
            if frame_flags & 0x8000 == 0 {
                row_flags = Some(read_edge_flags(br, ceil_to(self.height as u32, 16) as usize / 8 - 1)?);
            }
            if frame_flags & 0x4000 == 0 {
                col_flags = Some(read_edge_flags(br, ceil_to(self.width as u32, 16) as usize / 8 - 1)?);
            }
        }
        Ok(PacketHeader {
            frame_flags,
            slice_rows,
            slice_end,
            row_flags,
            col_flags,
        })
    }

    /// Decodes one video packet. `keyframe` comes from the frame index.
    // SPEC-QUESTION: SPEC §2.2/§3 do not say what an empty (or shorter than the slice table)
    // video packet means; we report it as an error rather than repeating the previous frame.
    pub fn decode(&mut self, packet: &[u8], keyframe: bool) -> Result<VideoFrame> {
        if !keyframe && self.reference.is_none() {
            return Err(Error::MissingReference);
        }
        let mut br = BitReader::new(packet);
        self.reports.clear();
        let hdr = self.parse_packet_header(packet, &mut br)?;
        self.last_frame_flags = hdr.frame_flags;
        self.last_edge_flags = (hdr.row_flags.clone(), hdr.col_flags.clone());
        self.mb_info.clear();
        self.mb_info
            .resize((self.aligned_w / 32) * (self.aligned_h / 32), MbInfo::default());
        let mut cur = self
            .spare
            .take()
            .unwrap_or_else(|| FrameBuf::new(self.aligned_w, self.aligned_h, self.has_alpha));
        {
            let mut ctx = FrameCtx {
                cur: &mut cur,
                refr: if keyframe { None } else { self.reference.as_ref() },
                keyframe,
                frame_flags: hdr.frame_flags,
                has_alpha: self.has_alpha,
                pic_w: self.width as i64,
                pic_h: self.height as i64,
                mb_w: self.aligned_w / 32,
                opts: self.opts,
                fail_mb: None,
                mb_info: &mut self.mb_info,
            };
            let float = is_float_revision(self.revision);
            let mut gstate = crate::g::SliceRows::new(ctx.mb_w);
            let mut fstate = crate::f::SliceRows::new(ctx.mb_w);
            for (i, &(r0, r1)) in hdr.slice_rows.iter().enumerate() {
                // SPEC-QUESTION: the reference only warns when a slice over- or under-reads; we
                // allow reading into the next slice but fail on reading past the packet end.
                let start_bit = br.position();
                ctx.fail_mb = None;
                let res = if float {
                    crate::f::decode_slice(&mut ctx, &mut br, &mut fstate, r0, r1)
                } else {
                    crate::g::decode_slice(&mut ctx, &mut br, &mut gstate, r0, r1)
                };
                let failure = res.err().map(|error| SliceFailure {
                    mb: ctx.fail_mb.unwrap_or((0, r0)),
                    bit: br.position(),
                    error,
                });
                self.reports.push(SliceReport {
                    index: i,
                    mb_rows: (r0, r1),
                    start_bit,
                    end_byte: hdr.slice_end[i],
                    consumed_bit: br.position(),
                    failure: failure.clone(),
                });
                if let Some(f) = failure {
                    if !self.opts.conceal_slice_errors {
                        return Err(f.error);
                    }
                    conceal(&mut ctx, f.mb, r1);
                }
                br.align();
                br.seek_byte(hdr.slice_end[i])?;
            }
        }
        let out = match self.opts.deblock.mode {
            DeblockMode::Off => {
                cur.extend_edges();
                self.output(&cur, keyframe)
            }
            DeblockMode::InLoop => {
                self.apply_deblock(&mut cur);
                cur.extend_edges();
                self.output(&cur, keyframe)
            }
            DeblockMode::Post => {
                cur.extend_edges();
                let mut f = cur.clone();
                self.apply_deblock(&mut f);
                self.output(&f, keyframe)
            }
        };
        self.spare = self.reference.replace(cur);
        Ok(out)
    }

    fn apply_deblock(&self, f: &mut FrameBuf) {
        let d = self.opts.deblock;
        let (rows, cols) = (self.last_edge_flags.0.as_deref(), self.last_edge_flags.1.as_deref());
        // Vertical edge at x = 8k uses column flag F[k]; horizontal edge at y = 8k uses row
        // flag F[k-1] (measured, docs/INFERRED.md). A set flag switches the edge off.
        let off = |flags: Option<&[bool]>, i: Option<usize>| match (flags, i) {
            (Some(fl), Some(i)) => fl.get(i).copied().unwrap_or(false),
            (Some(_), None) => false,
            (None, _) => !d.without_flags,
        };
        let mb_w = self.aligned_w / 32;
        let info = &self.mb_info;
        for (p, plane) in f.planes.iter_mut().enumerate() {
            let chroma = is_chroma(p);
            let mbs = if chroma { 16 } else { 32 };
            let pix_on = |x: usize, y: usize| {
                let t = info.get((y / mbs) * mb_w + x / mbs).map(|m| m.mb_type).unwrap_or(0);
                d.types & (1 << t) != 0
            };
            if chroma && !d.chroma {
                continue;
            }
            let s = if chroma { 2 } else { 1 };
            let col_off = |k: usize| off(cols, Some(s * k));
            let row_off = |k: usize| off(rows, (s * k).checked_sub(1));
            if d.horizontal_first {
                crate::deblock::filter_plane(plane, false, true, &col_off, &row_off, d.max_step, &pix_on);
                crate::deblock::filter_plane(plane, true, false, &col_off, &row_off, d.max_step, &pix_on);
            } else {
                crate::deblock::filter_plane(plane, true, true, &col_off, &row_off, d.max_step, &pix_on);
            }
        }
    }

    fn output(&self, f: &FrameBuf, keyframe: bool) -> VideoFrame {
        let (w, h) = (self.width, self.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        // SPEC-QUESTION: SPEC §3.1 says the reference writes C1 into Cr (V) and C2 into Cb (U);
        // we follow that, unverified.
        VideoFrame {
            width: w,
            height: h,
            y: f.planes[0].crop(w, h),
            v: f.planes[1].crop(cw, ch),
            u: f.planes[2].crop(cw, ch),
            alpha: f.planes.get(3).map(|p| p.crop(w, h)),
            keyframe,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_up_to_f() {
        // H32 = 736, boundary[0] = (360 + 16) & ~31 = 352
        assert_eq!(slice_boundaries(b'f', 720, 0), vec![352, 736]);
        assert_eq!(slice_boundaries(b'a', 32, 3), vec![32, 32]);
        assert_eq!(slice_boundaries(b'd', 20, 0), vec![0, 32]);
    }

    #[test]
    fn slices_g() {
        assert_eq!(slice_boundaries(b'g', 96, 3), vec![96]); // H32 < 128: one slice
        assert_eq!(slice_boundaries(b'g', 100, 3), vec![64, 128]); // H32 = 128
    }

    #[test]
    fn slices_h_and_later() {
        // 1080: H32 = 1088
        assert_eq!(slice_boundaries(b'h', 1080, 0), vec![544, 1088]);
        assert_eq!(slice_boundaries(b'k', 1080, 1), vec![384, 736, 1088]);
        // 720: H32 = 736, S = 4: end 863 -> +192, end 831 -> +192, end 799 -> +192
        assert_eq!(slice_boundaries(b'i', 720, 2), vec![192, 384, 576, 736]);
        // tiny picture with 8 slices: all but the first are empty
        assert_eq!(slice_boundaries(b'j', 32, 3), vec![32; 8]);
        // only bits 0-1 select the count
        assert_eq!(slice_boundaries(b'h', 1080, 0x0010_0004), vec![544, 1088]);
    }

    fn arr16(v: &[i32]) -> [i32; 16] {
        let mut a = [0; 16];
        a[..v.len()].copy_from_slice(v);
        a
    }

    #[test]
    fn dc_prediction_chroma() {
        let r = arr16(&[0, 1, 2, 3]);
        let mut out = [0; 16];
        // no neighbours, intra: block 0 = 1024; block 1 = L = 1024 + 1; block 2 = T = 1024 + 2;
        // block 3: L + T - TL = 1026 + 1025 - 1024 = 1027, clamped to max(L, T, TL) = 1026, + 3
        predict_dc(4, &r, &DcNeighbours::none(), true, 0, 2047, &mut out);
        assert_eq!(&out[..4], &[1024, 1025, 1026, 1029]);
        // inter starts at 0: block 3: 2 + 1 - 0 = 3 clamped to 2, + 3
        predict_dc(4, &r, &DcNeighbours::none(), false, -1023, 1023, &mut out);
        assert_eq!(&out[..4], &[0, 1, 2, 5]);
        // with neighbours: left MB blocks 1 and 3, top MB blocks 2 and 3, top-left block 3
        let nb = DcNeighbours {
            left: Some(arr16(&[0, 100, 0, 300])),
            top: Some(arr16(&[0, 0, 200, 400])),
            top_left: Some(arr16(&[0, 0, 0, 150])),
        };
        predict_dc(4, &[0; 16], &nb, true, 0, 2047, &mut out);
        // block 0: L 100, T 200, TL 150 -> 150 (in range); block 1: L 150, T 400, TL 200 -> 350;
        // block 2: L 300, T 150, TL 100 -> 300 (clamped to max); block 3: L 300, T 350, TL 150 -> 350
        assert_eq!(&out[..4], &[150, 350, 300, 350]);
        // top only (left edge)
        let nb2 = DcNeighbours {
            left: None,
            top: nb.top,
            top_left: None,
        };
        predict_dc(4, &[0; 16], &nb2, true, 0, 2047, &mut out);
        assert_eq!(&out[..4], &[200, 400, 200, 400]);
        // clamping of the result
        predict_dc(
            4,
            &arr16(&[5000, -9000, 0, 0]),
            &DcNeighbours::none(),
            true,
            0,
            2047,
            &mut out,
        );
        assert_eq!(&out[..2], &[2047, 0]);
    }

    #[test]
    fn dc_prediction_luma_zorder() {
        // Left neighbour only, residuals zero: every block in a row copies the left MB's
        // rightmost block of that row. Left MB values by raster position r*4+c are stored at
        // index INV_ZORDER.
        let mut left = [0i32; 16];
        for row in 0..4 {
            left[INV_ZORDER[row * 4 + 3]] = 10 * (row as i32 + 1);
        }
        let nb = DcNeighbours {
            left: Some(left),
            top: None,
            top_left: None,
        };
        let mut out = [0; 16];
        predict_dc(16, &[0; 16], &nb, true, 0, 2047, &mut out);
        for k in 0..16 {
            let row = (ZORDER[k] >> 2) as i32;
            assert_eq!(out[k], 10 * (row + 1), "block {k}");
        }
    }

    #[test]
    fn float_dc_prediction() {
        let r = [0.5f32; 16];
        let mut out = [0f32; 16];
        predict_dc(4, &r, &DcNeighbours::none(), true, 0.0, 2047.0, &mut out);
        assert_eq!(&out[..4], &[1024.5, 1025.0, 1025.0, 1025.5]);
    }
}
