//! Video packet decoding shared by both variants (SPEC §3, §4.6–§4.8).

use crate::bits::BitReader;
use crate::container::{is_float_revision, is_known_revision, Header, MAX_HEIGHT, MAX_WIDTH};
use crate::dsp::{block_dc_from_pixels, ceil_to, clamp, copy_block, mc_chroma, mc_luma};
use crate::edge_flags::read_edge_flags;
use crate::error::{Error, Result};
use crate::frame::{FrameBuf, Plane, PlaneBand, VideoFrame};
use crate::loop_filter::{filter_frame, BlocksBand, PlaneBlocks};
use crate::par::par_map;
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
    /// Predict normally from the coded (32-aligned) reference frame with edge replication, as RAD does (SPEC §4.7 draft 7;
    /// unchanged in draft 8, which only adjusts the vectors of hidden quadrants beforehand).
    #[default]
    Predict,
    /// Alternative to RAD's behaviour, kept for experiments: leave the destination block as a
    /// copy of the co-located reference block.
    CopyColocated,
    /// Alternative to RAD's behaviour, kept for experiments: clamp the source top-left into the
    /// picture and predict from there.
    ClampToPicture,
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
    /// Switch the in-loop filter of SPEC §4.10 off (for debugging; the output then no longer
    /// matches RAD's decoder, and P frames drift). The filter runs by default for the integer
    /// variant; the float variant never runs it (see [`Decoder`]).
    pub disable_loop_filter: bool,
    /// Number of threads that decode the slices of a frame, and then run the loop filter, in
    /// parallel. 0 (the default) uses [`std::thread::available_parallelism`]; 1 decodes on the
    /// calling thread only, slice after slice. The output is the same either way. Slice
    /// decoding uses at most one thread per slice, and float-variant files with alpha are
    /// always decoded slice after slice (see [`Decoder::decode`]).
    pub threads: usize,
}

impl DecoderOptions {
    /// The thread count `threads` stands for (at least 1).
    fn resolved_threads(&self) -> usize {
        match self.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n,
        }
    }
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
/// `nplanes` planes (SPEC §4.8 neighbour refresh). `f` holds the current slice's rows, which
/// contain every MB the refresh can reach (it never looks above the slice's first row).
pub(crate) fn mb_dcs_from_pixels<T: DcNum>(
    f: &[PlaneBand],
    mbx: usize,
    mby: usize,
    nplanes: usize,
    dc: &mut [[T; 16]; 4],
) {
    for (p, plane) in f.iter().enumerate().take(nplanes) {
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
    f: &[PlaneBand],
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

/// Decoding context of one slice, shared by both variants.
pub(crate) struct FrameCtx<'a> {
    /// The slice's rows of the current frame, one band per plane (Y, C1, C2, A). A slice
    /// writes only its own MB rows (SPEC §3.2) and reads only those and the reference, so the
    /// slices of a frame can be decoded in parallel (see [`Decoder::decode`]).
    pub cur: Vec<PlaneBand<'a>>,
    pub refr: Option<&'a FrameBuf>,
    pub keyframe: bool,
    pub frame_flags: u32,
    pub has_alpha: bool,
    pub pic_w: i64,
    pub pic_h: i64,
    /// `H16 = ceil_to(height, 16)` for the hidden quadrant rules of SPEC §4.7 / §4.2 (draft 8).
    /// `None` for the float variant (revisions <= f, SPEC §5): draft 8 was only checked on
    /// integer-variant files, so the float variant keeps the plain co-located copy and the
    /// decoded vectors.
    pub hidden_h16: Option<i64>,
    pub mb_w: usize,
    pub opts: DecoderOptions,
    /// Set by the slice decoders to the MB that failed.
    pub fail_mb: Option<(usize, usize)>,
    /// Per-MB side information of the slice's MB rows, raster order, from MB row `mb_row0`.
    pub mb_info: &'a mut [MbInfo],
    pub mb_row0: usize,
    /// Per-plane 8×8 block info of the slice's rows, for the loop filter (SPEC §4.10).
    pub blocks: Vec<BlocksBand<'a>>,
}

impl FrameCtx<'_> {
    /// Stores the side information of MB (mbx, mby).
    pub fn set_mb_info(&mut self, mbx: usize, mby: usize, info: MbInfo) {
        let i = (mby - self.mb_row0) * self.mb_w + mbx;
        if let Some(slot) = self.mb_info.get_mut(i) {
            *slot = info;
        }
    }

    /// Copies the co-located MB from the reference for all planes: SKIP in the float variant
    /// (unchanged by SPEC draft 8, which was checked on the integer variant only; see
    /// [`FrameCtx::hidden_h16`]) and slice error concealment. The integer variant's SKIP uses
    /// [`FrameCtx::skip_mb`].
    pub fn copy_mb(&mut self, mbx: usize, mby: usize) -> Result<()> {
        let r = self.refr.ok_or(Error::MissingReference)?;
        for (p, (dst, src)) in self.cur.iter_mut().zip(r.planes.iter()).enumerate() {
            let n = if is_chroma(p) { 16 } else { 32 };
            copy_block(dst, n * mbx, n * mby, src, (n * mbx) as i64, (n * mby) as i64, n);
        }
        Ok(())
    }

    /// Copies one plane's co-located MB from the reference. Only used for float-variant alpha
    /// (SPEC §5.1 stand-in), so it does not apply the draft 8 hidden quadrant rule; the integer
    /// variant motion-compensates alpha through [`FrameCtx::mc_mb`] / [`FrameCtx::skip_mb`].
    pub fn copy_mb_plane(&mut self, p: usize, mbx: usize, mby: usize) -> Result<()> {
        let r = self.refr.ok_or(Error::MissingReference)?;
        let n = if is_chroma(p) { 16 } else { 32 };
        if let (Some(dst), Some(src)) = (self.cur.get_mut(p), r.planes.get(p)) {
            copy_block(dst, n * mbx, n * mby, src, (n * mbx) as i64, (n * mby) as i64, n);
        }
        Ok(())
    }

    /// SKIP for the integer variant (SPEC §4.2 step 2, draft 8): copies the co-located MB from
    /// the reference, except that hidden quadrants (§4.7) are motion-compensated with the vector
    /// (0, 0) adjusted as in §4.7, i.e. copied from the last 16 luma (8 chroma) rows above H16.
    /// This covers every plane, alpha included (§4.7 says alpha too, unverified).
    pub fn skip_mb(&mut self, mbx: usize, mby: usize) -> Result<()> {
        self.copy_mb(mbx, mby)?;
        let Some(h16) = self.hidden_h16 else { return Ok(()) };
        let planes: &[usize] = if self.has_alpha { &[0, 1, 2, 3] } else { &[0, 1, 2] };
        for j in 0..4 {
            if quadrant_y(mby, j) >= h16 {
                self.mc_quadrant(mbx, mby, j, [0, 0], planes)?;
            }
        }
        Ok(())
    }

    /// Motion-compensates the planes listed in `planes` of MB (mbx, mby) (SPEC §4.7).
    /// `mv` are the decoded vectors; hidden quadrants get their vertical component adjusted
    /// here for the prediction only (draft 8), the caller keeps the decoded vectors for vector
    /// prediction and the loop filter.
    pub fn mc_mb(&mut self, mbx: usize, mby: usize, mv: &[Mv; 4], planes: &[usize]) -> Result<()> {
        self.refr.ok_or(Error::MissingReference)?;
        for (j, &v) in mv.iter().enumerate() {
            self.mc_quadrant(mbx, mby, j, v, planes)?;
        }
        Ok(())
    }

    /// Motion-compensates quadrant j (16×16 luma, 8×8 chroma) of the listed planes with the
    /// decoded vector `v`, adjusted first if the quadrant is hidden (SPEC §4.7 draft 8).
    fn mc_quadrant(&mut self, mbx: usize, mby: usize, j: usize, v: Mv, planes: &[usize]) -> Result<()> {
        let r = self.refr.ok_or(Error::MissingReference)?;
        let policy = self.opts.out_of_range_mv;
        let v = match self.hidden_h16 {
            Some(h16) => [v[0], adjust_hidden_vy(v[1], quadrant_y(mby, j), h16)],
            None => v,
        };
        for &p in planes {
            let (Some(dst), Some(src)) = (self.cur.get_mut(p), r.planes.get(p)) else {
                continue;
            };
            let chroma = is_chroma(p);
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
        Ok(())
    }
}

/// Luma top row of quadrant j of MB row `mby`.
#[inline]
fn quadrant_y(mby: usize, j: usize) -> i64 {
    (32 * mby + 16 * (j >> 1)) as i64
}

/// SPEC §4.7 draft 8, hidden quadrants: the vertical vector (luma half-pel units) of the
/// quadrant whose luma top row is `y`, as used for motion compensation. Quadrants with
/// `y < h16` are never adjusted; for hidden ones the integer part is limited so that the source
/// block, plus the 3 rows the half-pel filter reads below it, stays above H16. The fraction is
/// kept.
pub(crate) fn adjust_hidden_vy(vy: i32, y: i64, h16: i64) -> i32 {
    if y < h16 {
        return vy;
    }
    let (iy, fy) = (vy >> 1, vy & 1);
    let lim = h16 - 16 - 3 * fy as i64 - y;
    if iy as i64 > lim {
        (2 * lim) as i32 + fy
    } else {
        vy
    }
}

/// Conceals MBs from `from` (inclusive) to the end of MB row `end_row` (exclusive): copies the
/// co-located MB from the reference, or fills mid-grey without one.
fn conceal(ctx: &mut FrameCtx, from: (usize, usize), end_row: usize) {
    let (mut x, mut y) = from;
    while y < end_row {
        if ctx.copy_mb(x, y).is_err() {
            for (p, plane) in ctx.cur.iter_mut().enumerate() {
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

/// What every slice of a frame reads besides its own bits: the reference frame and the
/// frame-level parameters.
struct SliceShared<'a> {
    refr: Option<&'a FrameBuf>,
    keyframe: bool,
    frame_flags: u32,
    has_alpha: bool,
    pic_w: i64,
    pic_h: i64,
    hidden_h16: Option<i64>,
    mb_w: usize,
    opts: DecoderOptions,
    float: bool,
}

/// One slice to decode: where its bits start, and the parts of the frame it writes.
struct SliceJob<'a> {
    index: usize,
    /// MB rows `[first, end)`.
    rows: (usize, usize),
    /// Positioned at the slice's first bit.
    reader: BitReader<'a>,
    end_byte: usize,
    /// The slice's rows of each plane.
    planes: Vec<PlaneBand<'a>>,
    /// The slice's block rows of each plane's loop filter info.
    blocks: Vec<BlocksBand<'a>>,
    /// The slice's rows of the per-MB side information.
    mb_info: &'a mut [MbInfo],
}

/// The per-slice MB row state of either variant (SPEC §4.1, §5.1).
enum SliceState {
    G(crate::g::SliceRows),
    F(crate::f::SliceRows),
}

impl SliceState {
    fn new(float: bool, mb_w: usize) -> SliceState {
        if float {
            SliceState::F(crate::f::SliceRows::new(mb_w))
        } else {
            SliceState::G(crate::g::SliceRows::new(mb_w))
        }
    }
}

/// Splits the frame into one [`SliceJob`] per slice. Slice 0 starts where `header` stopped
/// (after the edge flag lists), slice i > 0 at byte `slice_end[i-1]` (SPEC §3.2 step 6). Slice
/// boundaries are multiples of 32 luma rows, so every plane splits at whole MB rows.
fn split_slices<'a>(
    hdr: &PacketHeader,
    header: &BitReader<'a>,
    packet: &'a [u8],
    cur: &'a mut FrameBuf,
    blocks: &'a mut [PlaneBlocks],
    mb_info: &'a mut [MbInfo],
    mb_w: usize,
) -> Result<Vec<SliceJob<'a>>> {
    // Rows per MB row: in pixels 32 (luma, alpha) or 16 (chroma), in 8×8 blocks 4 or 2.
    let ends = |chroma: bool, unit: usize| -> Vec<usize> {
        let unit = if chroma { unit / 2 } else { unit };
        hdr.slice_rows.iter().map(|&(_, r1)| r1 * unit).collect()
    };
    let mut planes: Vec<_> = (cur.planes.iter_mut().enumerate())
        .map(|(p, plane)| plane.bands(&ends(is_chroma(p), 32)).into_iter())
        .collect();
    let mut block_bands: Vec<_> = (blocks.iter_mut().enumerate())
        .map(|(p, b)| b.bands(&ends(is_chroma(p), 4)).into_iter())
        .collect();
    let mut info = mb_info;
    let mut jobs = Vec::with_capacity(hdr.slice_rows.len());
    for (i, &(r0, r1)) in hdr.slice_rows.iter().enumerate() {
        let (mine, rest) = std::mem::take(&mut info).split_at_mut((r1 - r0) * mb_w);
        info = rest;
        let reader = if i == 0 {
            header.clone()
        } else {
            let mut br = BitReader::new(packet);
            br.seek_byte(hdr.slice_end[i - 1])?;
            br
        };
        jobs.push(SliceJob {
            index: i,
            rows: (r0, r1),
            reader,
            end_byte: hdr.slice_end[i],
            planes: planes
                .iter_mut()
                .map(|b| b.next().expect("one band per slice"))
                .collect(),
            blocks: block_bands
                .iter_mut()
                .map(|b| b.next().expect("one band per slice"))
                .collect(),
            mb_info: mine,
        });
    }
    Ok(jobs)
}

/// Decodes one slice (SPEC §3.2 step 6), concealing the rest of it on failure if the options
/// say so.
fn run_slice(shared: &SliceShared, job: SliceJob, state: &mut SliceState) -> SliceReport {
    let SliceJob {
        index,
        rows: (r0, r1),
        mut reader,
        end_byte,
        planes,
        blocks,
        mb_info,
    } = job;
    let mut ctx = FrameCtx {
        cur: planes,
        refr: shared.refr,
        keyframe: shared.keyframe,
        frame_flags: shared.frame_flags,
        has_alpha: shared.has_alpha,
        pic_w: shared.pic_w,
        pic_h: shared.pic_h,
        hidden_h16: shared.hidden_h16,
        mb_w: shared.mb_w,
        opts: shared.opts,
        fail_mb: None,
        mb_info,
        mb_row0: r0,
        blocks,
    };
    // SPEC-QUESTION: the reference only warns when a slice over- or under-reads; we allow
    // reading into the next slice but fail on reading past the packet end.
    let start_bit = reader.position();
    let res = match state {
        SliceState::G(rows) => crate::g::decode_slice(&mut ctx, &mut reader, rows, r0, r1),
        SliceState::F(rows) => crate::f::decode_slice(&mut ctx, &mut reader, rows, r0, r1),
    };
    let failure = res.err().map(|error| SliceFailure {
        mb: ctx.fail_mb.unwrap_or((0, r0)),
        bit: reader.position(),
        error,
    });
    if let Some(f) = &failure {
        if shared.opts.conceal_slice_errors {
            conceal(&mut ctx, f.mb, r1);
        }
    }
    SliceReport {
        index,
        mb_rows: (r0, r1),
        start_bit,
        end_byte,
        consumed_bit: reader.position(),
        failure,
    }
}

#[allow(clippy::too_many_arguments)]
fn mc_block(
    dst: &mut PlaneBand,
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
    // SPEC §4.7 draft 7 (still so in draft 8): RAD predicts every block from the coded frame; the visible-size check only
    // applies to the experimental policies. The draft 8 hidden quadrant adjustment is applied by the caller.
    let outside = policy != OutOfRangeMv::Predict && (sx < 0 || sx >= w || sy < 0 || sy >= h);
    if outside {
        match policy {
            OutOfRangeMv::Predict => {} // unreachable: `outside` is false for Predict
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
    // SPEC-QUESTION: the draft 6 corrections of §4.7 (unclipped luma (1,1) intermediate,
    // one-step chroma bilinear) were checked on integer-variant files only; §5 says the float
    // variant shares the MC filters, so they are applied to both.
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
///
/// For the integer variant every frame goes through the in-loop filter of SPEC §4.10 before it
/// is output and kept as the next reference ([`DecoderOptions::disable_loop_filter`] switches it
/// off). The float variant is decoded without it.
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
    /// `opts.threads` resolved (at least 1).
    threads: usize,
    reference: Option<FrameBuf>,
    spare: Option<FrameBuf>,
    reports: Vec<SliceReport>,
    last_frame_flags: u32,
    last_edge_flags: (Option<Vec<bool>>, Option<Vec<bool>>),
    mb_info: Vec<MbInfo>,
    blocks: Vec<PlaneBlocks>,
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
            threads: DecoderOptions::default().resolved_threads(),
            reference: None,
            spare: None,
            reports: Vec::new(),
            last_frame_flags: 0,
            last_edge_flags: (None, None),
            mb_info: Vec::new(),
            blocks: Vec::new(),
        })
    }

    /// Sets the options for unverified behaviours.
    pub fn set_options(&mut self, opts: DecoderOptions) {
        self.opts = opts;
        self.threads = opts.resolved_threads();
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
            // Edge flags (SPEC §3.2 step 5, §3.3); used by the loop filter (§4.10).
            // SPEC §3.2 draft 7 (unchanged in draft 8): the row count uses the 16-aligned height,
            // the column count the 32-aligned width. Row edges past the list end lie at or below
            // H16 and are never filtered (§4.10 pass 3, draft 8; see `filter_frame`).
            if frame_flags & 0x8000 == 0 {
                row_flags = Some(read_edge_flags(br, ceil_to(self.height as u32, 16) as usize / 8 - 1)?);
            }
            if frame_flags & 0x4000 == 0 {
                col_flags = Some(read_edge_flags(br, ceil_to(self.width as u32, 32) as usize / 8 - 1)?);
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
    ///
    /// The slices are decoded in parallel (see [`DecoderOptions::threads`]), except for
    /// float-variant files with alpha. The result is the same as decoding them in order.
    // SPEC-QUESTION: SPEC §5.1 leaves alpha out of the float variant's neighbour refresh, so an
    // INTRA MB can predict alpha from the DC a SKIP or MOTION neighbour never wrote; in the
    // first row of a slice that stale value comes from the previous slice (a sequential decoder
    // keeps the row state across slices; a randomised test shows the parallel decode would
    // differ). We decode those files sequentially. Whether such files exist is unknown.
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
        if self.blocks.len() != cur.planes.len() {
            self.blocks = cur.planes.iter().map(|p| PlaneBlocks::new(p.w / 8, p.h / 8)).collect();
        }
        for b in &mut self.blocks {
            b.reset();
        }
        let shared = SliceShared {
            refr: if keyframe { None } else { self.reference.as_ref() },
            keyframe,
            frame_flags: hdr.frame_flags,
            has_alpha: self.has_alpha,
            pic_w: self.width as i64,
            pic_h: self.height as i64,
            hidden_h16: (!is_float_revision(self.revision)).then(|| ceil_to(self.height as u32, 16) as i64),
            mb_w: self.aligned_w / 32,
            opts: self.opts,
            float: is_float_revision(self.revision),
        };
        let jobs = split_slices(
            &hdr,
            &br,
            packet,
            &mut cur,
            &mut self.blocks,
            &mut self.mb_info,
            shared.mb_w,
        )?;
        // SPEC §3.2 step 6 gives every slice's start up front, and each slice writes only its
        // own MB rows (see `FrameCtx::cur`), so the slices can be decoded in any order. The
        // one exception is the float variant with alpha: its neighbour refresh skips alpha
        // (SPEC §5.1), so an INTRA MB in a slice's first row can predict alpha from a SKIP or
        // MOTION neighbour's stale stored DC, which the sequential decoder carries over from the
        // previous slice. Such files are decoded sequentially.
        let threads = self.threads.min(jobs.len());
        let reports = if threads <= 1 || (shared.float && self.has_alpha) {
            let mut rows = SliceState::new(shared.float, shared.mb_w);
            let mut reports = Vec::with_capacity(jobs.len());
            for job in jobs {
                let rep = run_slice(&shared, job, &mut rows);
                let stop = rep.failure.is_some() && !self.opts.conceal_slice_errors;
                reports.push(rep);
                if stop {
                    break;
                }
            }
            reports
        } else {
            // Every slice starts from a fresh state: SPEC §4.1 resets, at slice start, all the
            // state its first row reads (q and vectors of the row above; `TOP_EDGE` makes the
            // DCs and types above unavailable and keeps the neighbour refresh off that row).
            // The DCs that SKIP and MOTION MBs (and RESIDUE MBs without chroma) leave stored are
            // never read before the refresh recomputes them (SPEC §4.8), so whatever a
            // sequential decoder carries over from the previous slice cannot change the output.
            par_map(jobs, threads, |job| {
                run_slice(&shared, job, &mut SliceState::new(shared.float, shared.mb_w))
            })
        };
        self.reports = reports;
        if !self.opts.conceal_slice_errors {
            // As in a sequential decode: the first failing slice fails the frame, and the
            // reports end with it.
            if let Some(i) = self.reports.iter().position(|r| r.failure.is_some()) {
                self.reports.truncate(i + 1);
                // Slices after the failing one were not decoded sequentially; drop what the
                // parallel decode stored for them.
                let r1 = self.reports[i].mb_rows.1;
                self.mb_info[r1 * shared.mb_w..].fill(MbInfo::default());
                return Err(self.reports[i]
                    .failure
                    .as_ref()
                    .map(|f| f.error.clone())
                    .expect("failure"));
            }
        }
        // SPEC-QUESTION: SPEC §4.10 says it is unverified whether the float variant (revisions
        // <= f) uses the same loop filter; we leave it off there.
        if !is_float_revision(self.revision) && !self.opts.disable_loop_filter {
            filter_frame(
                &mut cur,
                &self.blocks,
                self.height,
                hdr.frame_flags,
                hdr.row_flags.as_deref(),
                hdr.col_flags.as_deref(),
                self.threads,
            );
        }
        cur.extend_edges();
        let out = self.output(&cur, keyframe);
        self.spare = self.reference.replace(cur);
        Ok(out)
    }

    fn output(&self, f: &FrameBuf, keyframe: bool) -> VideoFrame {
        let (w, h) = (self.width, self.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        // C1 is Cr (V) and C2 is Cb (U) (SPEC §3.1; open question 4 closed).
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
    #[test]
    fn hidden_quadrant_vector_adjustment() {
        // height 2160 -> H16 = 2160; the bottom MB row starts at 2144, its quadrants j = 2, 3
        // at Y = 2160 are hidden.
        let h16 = ceil_to(2160, 16) as i64;
        assert_eq!(h16, 2160);
        assert_eq!(adjust_hidden_vy(0, 2160, h16), 2 * (2160 - 16 - 2160));
        assert_eq!(adjust_hidden_vy(0, 2160, h16), -32);
        // half-pel: lim = 2160 - 16 - 3 - 2160 = -19 -> vy = 2·(-19) + 1
        assert_eq!(adjust_hidden_vy(1, 2160, h16), -37);
        // already far enough up: unchanged
        assert_eq!(adjust_hidden_vy(-40, 2160, h16), -40);
        assert_eq!(adjust_hidden_vy(-39, 2160, h16), -39);
        // quadrants above H16 (the top row of the bottom MB) are never adjusted
        assert_eq!(adjust_hidden_vy(0, 2144, h16), 0);
        assert_eq!(adjust_hidden_vy(57, 2144, h16), 57);
        // a height that is a multiple of 32: no quadrant of the coded frame reaches H16
        for height in [64u32, 1088, 2176] {
            let h16 = ceil_to(height, 16) as i64;
            for y in (0..ceil_to(height, 32) as i64).step_by(16) {
                for vy in -70..70 {
                    assert_eq!(adjust_hidden_vy(vy, y, h16), vy);
                }
            }
        }
    }
    #[test]
    fn skip_hidden_quadrants_copy_from_above_h16() {
        // Coded 32×64 (two MB rows), visible height 40 -> H16 = 48: quadrants at Y = 48 are hidden.
        let mut refr = FrameBuf::new(32, 64, false);
        for plane in refr.planes.iter_mut() {
            for y in 0..plane.h {
                for x in 0..plane.w {
                    let i = plane.idx(x as i64, y as i64);
                    plane.data[i] = y as u8;
                }
            }
        }
        refr.extend_edges();
        let run = |hidden_h16: Option<i64>| {
            let mut cur = FrameBuf::new(32, 64, false);
            let mut mb_info = vec![MbInfo::default(); 2];
            let mut ctx = FrameCtx {
                cur: cur.planes.iter_mut().map(|p| p.band()).collect(),
                refr: Some(&refr),
                keyframe: false,
                frame_flags: 0,
                has_alpha: false,
                pic_w: 32,
                pic_h: 40,
                hidden_h16,
                mb_w: 1,
                opts: DecoderOptions::default(),
                fail_mb: None,
                mb_info: &mut mb_info,
                mb_row0: 0,
                blocks: Vec::new(),
            };
            ctx.skip_mb(0, 1).unwrap();
            drop(ctx);
            let col = |p: &Plane| (0..p.h).map(|y| p.data[p.idx(3, y as i64)]).collect::<Vec<u8>>();
            (col(&cur.planes[0]), col(&cur.planes[1]))
        };
        let (y, c) = run(Some(48));
        // luma: rows 32..48 co-located, hidden rows 48..64 from 32..48 (H16 - 16)
        assert_eq!(y[32..48], (32..48).collect::<Vec<u8>>()[..]);
        assert_eq!(y[48..64], (32..48).collect::<Vec<u8>>()[..]);
        // chroma: rows 16..24 co-located, hidden rows 24..32 from 16..24
        assert_eq!(c[16..24], (16..24).collect::<Vec<u8>>()[..]);
        assert_eq!(c[24..32], (16..24).collect::<Vec<u8>>()[..]);
        // float variant (no H16): plain co-located copy
        let (y, c) = run(None);
        assert_eq!(y[32..64], (32..64).collect::<Vec<u8>>()[..]);
        assert_eq!(c[16..32], (16..32).collect::<Vec<u8>>()[..]);
    }
}
