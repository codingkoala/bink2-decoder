//! In-loop filter (SPEC §4.10, draft 6): DC smoothing of flat intra blocks, then the vertical
//! and horizontal 8×8 block edges.

use crate::dsp::clip8;
use crate::frame::{FrameBuf, Plane};
use crate::video::Mv;

/// What the filter needs to know about one 8×8 block of one plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct BlockInfo {
    /// The block belongs to an INTRA MB.
    pub intra: bool,
    /// Strength 0 … 3 (intra blocks only; 3 = no AC coefficients).
    pub s: u8,
    /// Final DC (intra blocks only).
    pub dc: i32,
    /// Vector of the block's 16×16 luma quadrant, coded units.
    pub mv: Mv,
}

impl BlockInfo {
    /// The packed 16-bit word, read as signed.
    pub fn word(&self) -> i16 {
        if self.intra {
            ((1u16 << 15) | ((self.s as u16 & 3) << 13) | (self.dc as u16 & 0x1FFF)) as i16
        } else {
            0
        }
    }

    /// Edge-rule strength: `s` for intra blocks, 0 otherwise.
    fn strength(&self) -> u8 {
        if self.intra {
            self.s
        } else {
            0
        }
    }
}

/// `s` of an intra block from its cbp bit and the number of AC levels read (SPEC §4.10).
pub(crate) fn intra_strength(coded: bool, n: u32) -> u8 {
    if !coded {
        3
    } else {
        (n < 8) as u8 + (n < 4) as u8 + (n == 0) as u8
    }
}

/// Block info of one plane, `bw × bh` blocks in raster order.
#[derive(Debug, Clone, Default)]
pub(crate) struct PlaneBlocks {
    pub bw: usize,
    pub bh: usize,
    pub info: Vec<BlockInfo>,
}

impl PlaneBlocks {
    pub fn new(bw: usize, bh: usize) -> Self {
        PlaneBlocks {
            bw,
            bh,
            info: vec![BlockInfo::default(); bw * bh],
        }
    }

    pub fn reset(&mut self) {
        self.info.fill(BlockInfo::default());
    }

    #[inline]
    pub fn get(&self, bx: usize, by: usize) -> &BlockInfo {
        &self.info[by * self.bw + bx]
    }

    #[inline]
    pub fn set(&mut self, bx: usize, by: usize, b: BlockInfo) {
        if bx < self.bw && by < self.bh {
            self.info[by * self.bw + bx] = b;
        }
    }
}

#[inline]
fn mulhi(d: i32, k: i32) -> i32 {
    (d * k) >> 16
}

/// The smoothing weights `K[i]`.
#[inline]
fn k(i: usize, d: i32) -> i32 {
    match i {
        0 => mulhi(d, 26624),
        1 => mulhi(d, 18432),
        2 => mulhi(d, 12288),
        3 => mulhi(d, 7168),
        4 => d >> 4,
        5 => d >> 5,
        6 => d >> 7,
        _ => 0,
    }
}

/// The 3×3 DC grid around block (bx, by) (SPEC §4.10 pass 1).
pub(crate) fn smooth_grid(blocks: &PlaneBlocks, bx: usize, by: usize, thr: i32) -> [[i32; 3]; 3] {
    let me = blocks.get(bx, by);
    let c = me.word() as i32;
    let nb = |dx: i64, dy: i64| -> Option<i32> {
        let (x, y) = (bx as i64 + dx, by as i64 + dy);
        if x < 0 || y < 0 || x >= blocks.bw as i64 || y >= blocks.bh as i64 {
            // SPEC-QUESTION: SPEC §4.10 treats neighbours outside the plane as "not accepted";
            // RAD's exact handling at the left and right plane edges is unverified.
            return None;
        }
        let n = blocks.get(x as usize, y as usize);
        let diff = ((n.word() as i32 - c) as i16 as i32).abs();
        (diff < thr).then_some(n.dc)
    };
    let mut g = [[0i32; 3]; 3];
    g[1][1] = me.dc;
    g[1][0] = nb(-1, 0).unwrap_or(g[1][1]);
    g[1][2] = nb(1, 0).unwrap_or(g[1][1]);
    g[0][1] = nb(0, -1).unwrap_or(g[1][1]);
    g[2][1] = nb(0, 1).unwrap_or(g[1][1]);
    g[0][0] = nb(-1, -1).unwrap_or(g[1][0]);
    g[2][0] = nb(-1, 1).unwrap_or(g[1][0]);
    g[0][2] = nb(1, -1).unwrap_or(g[1][2]);
    g[2][2] = nb(1, 1).unwrap_or(g[1][2]);
    g
}

/// The smooth 8×8 surface through a DC grid, as `out[row][column]`.
pub(crate) fn smooth_surface(g: &[[i32; 3]; 3]) -> [[u8; 8]; 8] {
    let h: [[i32; 8]; 3] = std::array::from_fn(|r| {
        std::array::from_fn(|i| g[r][1] + 4 + k(i, g[r][0] - g[r][1]) + k(7 - i, g[r][2] - g[r][1]))
    });
    let mut out = [[0u8; 8]; 8];
    for (j, row) in out.iter_mut().enumerate() {
        for (i, px) in row.iter_mut().enumerate() {
            let v = h[1][i] + k(j, h[0][i] - h[1][i]) + k(7 - j, h[2][i] - h[1][i]);
            *px = clip8(v >> 3);
        }
    }
    out
}

/// Pass 1: replaces every intra block with `s = 3` by its smooth surface.
pub(crate) fn smooth_plane(p: &mut Plane, blocks: &PlaneBlocks, thr: i32) {
    for by in 0..blocks.bh.min(p.h / 8) {
        for bx in 0..blocks.bw.min(p.w / 8) {
            let b = blocks.get(bx, by);
            if !(b.intra && b.s == 3) {
                continue;
            }
            let surf = smooth_surface(&smooth_grid(blocks, bx, by, thr));
            for (j, row) in surf.iter().enumerate() {
                let i = p.idx((8 * bx) as i64, (8 * by + j) as i64);
                p.data[i..i + 8].copy_from_slice(row);
            }
        }
    }
}

/// Edge strengths (sA, sB) for blocks A (left/above) and B (right/below), or None if the edge
/// is skipped.
pub(crate) fn edge_strength(a: &BlockInfo, b: &BlockInfo) -> Option<(u8, u8)> {
    let (mut sa, mut sb) = (a.strength(), b.strength());
    if !a.intra && !b.intra && ((a.mv[0] - b.mv[0]).abs() >= 2 || (a.mv[1] - b.mv[1]).abs() >= 2) {
        sa |= 2;
        sb |= 2;
    }
    if sa == sb && (sa == 0 || sa == 3) {
        None
    } else {
        Some((sa, sb))
    }
}

/// The edge filter on `[p1, p0, q0, q1]`.
pub(crate) fn filter_edge(px: [u8; 4], sa: u8, sb: u8) -> [u8; 4] {
    let [p1, p0, q0, q1] = px.map(|v| v as i32);
    let d = q0 - p0;
    let a = (d + 2) >> 2;
    let b = (d + 4) >> 3;
    let mut out = px;
    if sa >= 1 {
        out[1] = clip8(p0 + a);
    }
    if sa >= 2 {
        out[0] = clip8(p1 + b);
    }
    if sb >= 1 {
        out[2] = clip8(q0 - a);
    }
    if sb >= 2 {
        out[3] = clip8(q1 - b);
    }
    out
}

/// Pass 2: vertical edges. `off(bx)` is true if the edge at plane x = 8·bx is switched off.
pub(crate) fn vertical_pass(p: &mut Plane, blocks: &PlaneBlocks, off: &dyn Fn(usize) -> bool) {
    let (bw, bh) = (blocks.bw.min(p.w / 8), blocks.bh.min(p.h / 8));
    for by in 0..bh {
        for bx in 1..bw {
            if off(bx) {
                continue;
            }
            let Some((sa, sb)) = edge_strength(blocks.get(bx - 1, by), blocks.get(bx, by)) else {
                continue;
            };
            for y in 8 * by..8 * by + 8 {
                let i = p.idx((8 * bx) as i64 - 2, y as i64);
                let px = [p.data[i], p.data[i + 1], p.data[i + 2], p.data[i + 3]];
                p.data[i..i + 4].copy_from_slice(&filter_edge(px, sa, sb));
            }
        }
    }
}

/// Pass 3: horizontal edges. `off(by)` is true if the edge at plane y = 8·by is switched off.
pub(crate) fn horizontal_pass(p: &mut Plane, blocks: &PlaneBlocks, off: &dyn Fn(usize) -> bool) {
    let (bw, bh) = (blocks.bw.min(p.w / 8), blocks.bh.min(p.h / 8));
    let s = p.stride;
    for by in 1..bh {
        if off(by) {
            continue;
        }
        for bx in 0..bw {
            let Some((sa, sb)) = edge_strength(blocks.get(bx, by - 1), blocks.get(bx, by)) else {
                continue;
            };
            for x in 8 * bx..8 * bx + 8 {
                let i = p.idx(x as i64, (8 * by) as i64 - 2);
                let px = [p.data[i], p.data[i + s], p.data[i + 2 * s], p.data[i + 3 * s]];
                let o = filter_edge(px, sa, sb);
                for (r, v) in o.iter().enumerate() {
                    p.data[i + r * s] = *v;
                }
            }
        }
    }
}

/// Runs the whole filter on a decoded frame (planes in coding order Y, C1, C2, A).
pub(crate) fn filter_frame(
    f: &mut FrameBuf,
    blocks: &[PlaneBlocks],
    frame_flags: u32,
    row_flags: Option<&[bool]>,
    col_flags: Option<&[bool]>,
) {
    let thr = ((frame_flags & 0xFF) * 8) as i32;
    // A flag that is not coded is 0 (SPEC §3.2): the edge is filtered.
    // SPEC-QUESTION: luma edges past the end of a list (coded width/height rounded to 32 but the
    // list sized from the size rounded to 16) are treated as flag 0.
    let flag = |l: Option<&[bool]>, i: usize| l.and_then(|l| l.get(i)).copied().unwrap_or(false);
    for (pi, (plane, pb)) in f.planes.iter_mut().zip(blocks).enumerate() {
        if pi == 3 {
            // Alpha: DC smoothing only, not with frame_flags bit 19.
            // SPEC-QUESTION: §4.10 also names alpha in the edge flag indexing of pass 2; we
            // follow the statement that alpha gets DC smoothing only (unverified, no test file).
            if frame_flags & 0x80000 == 0 {
                smooth_plane(plane, pb, thr);
            }
            continue;
        }
        let chroma = pi != 0;
        smooth_plane(plane, pb, thr);
        if frame_flags & 0x4000 == 0 {
            let off = |bx: usize| flag(col_flags, if chroma { 2 * bx - 1 } else { bx - 1 });
            vertical_pass(plane, pb, &off);
        }
        if frame_flags & 0x8000 == 0 {
            let off = |by: usize| flag(row_flags, if chroma { 2 * by - 1 } else { by - 1 });
            horizontal_pass(plane, pb, &off);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intra(s: u8, dc: i32) -> BlockInfo {
        BlockInfo {
            intra: true,
            s,
            dc,
            mv: [0, 0],
        }
    }

    fn inter(mv: Mv) -> BlockInfo {
        BlockInfo {
            mv,
            ..Default::default()
        }
    }

    #[test]
    fn strength_from_level_count() {
        assert_eq!(intra_strength(false, 0), 3);
        assert_eq!(intra_strength(true, 0), 3);
        assert_eq!(intra_strength(true, 1), 2);
        assert_eq!(intra_strength(true, 3), 2);
        assert_eq!(intra_strength(true, 4), 1);
        assert_eq!(intra_strength(true, 7), 1);
        assert_eq!(intra_strength(true, 8), 0);
        assert_eq!(intra_strength(true, 63), 0);
    }

    #[test]
    fn packed_word() {
        assert_eq!(intra(3, 1024).word() as u16, 0x8000 | 0x6000 | 1024);
        assert_eq!(intra(0, 2047).word() as u16, 0x8000 | 2047);
        assert_eq!(inter([5, 5]).word(), 0);
    }

    /// 3×3 blocks with the given DCs, all intra with s = 3 unless listed in `other`.
    fn grid(dcs: [i32; 9], other: &[(usize, BlockInfo)]) -> PlaneBlocks {
        let mut b = PlaneBlocks::new(3, 3);
        for (i, &dc) in dcs.iter().enumerate() {
            b.info[i] = intra(3, dc);
        }
        for &(i, v) in other {
            b.info[i] = v;
        }
        b
    }

    #[test]
    fn dc_smoothing_grid_selection() {
        let b = grid(
            [1000, 1010, 1020, 990, 1000, 1100, 980, 1000, 1005],
            &[(7, intra(2, 1000))],
        );
        // thr 40: right neighbour (1100) rejected -> its slot and its corners' fallback become
        // the centre; bottom neighbour is s = 2 (word differs by 1 << 13) -> rejected.
        let g = smooth_grid(&b, 1, 1, 40);
        assert_eq!(g, [[1000, 1010, 1020], [990, 1000, 1000], [980, 1000, 1005]]);
        // Corners fall back to the left/right slot of their row, not to the centre.
        let b2 = grid([1500, 1010, 1500, 990, 1000, 1030, 1500, 1000, 1500], &[]);
        assert_eq!(
            smooth_grid(&b2, 1, 1, 40),
            [[990, 1010, 1030], [990, 1000, 1030], [990, 1000, 1030]]
        );
        // thr 0: nothing accepted, even equal DCs -> every slot is the centre.
        let flat = grid([1000; 9], &[]);
        assert_eq!(smooth_grid(&flat, 1, 1, 0), [[1000; 3]; 3]);
        // Plane edge: the corner block has no neighbours above/left.
        let g = smooth_grid(&flat, 0, 0, 2040);
        assert_eq!(g, [[1000; 3]; 3]);
        // Inter neighbours never qualify.
        let b3 = grid([1000; 9], &[(3, inter([0, 0]))]);
        assert_eq!(smooth_grid(&b3, 1, 1, 2040)[1][0], 1000);
        let b4 = grid(
            [1000, 1000, 1000, 1200, 1000, 1000, 1000, 1000, 1000],
            &[(3, inter([0, 0]))],
        );
        assert_eq!(smooth_grid(&b4, 1, 1, 2040)[1][0], 1000);
    }

    #[test]
    fn dc_smoothing_surface() {
        // flat grid: (dc + 4) >> 3 everywhere, the same as a DC-only block
        for dc in [0, 1, 1000, 1027, 2047] {
            let s = smooth_surface(&[[dc; 3]; 3]);
            assert!(s.iter().flatten().all(|&v| v == clip8((dc + 4) >> 3)), "dc {dc}");
        }
        // left neighbour 800, everything else 1000 (rows identical): column i of each row is
        // (1000 + 4 + K[i](-200)) >> 3, K[7-i] term zero.
        let g = [[800, 1000, 1000]; 3];
        let s = smooth_surface(&g);
        let want: [i32; 8] = [
            1004 + ((-200 * 26624) >> 16),
            1004 + ((-200 * 18432) >> 16),
            1004 + ((-200 * 12288) >> 16),
            1004 + ((-200 * 7168) >> 16),
            1004 + (-200 >> 4),
            1004 + (-200 >> 5),
            1004 + (-200 >> 7),
            1004,
        ];
        for row in &s {
            for i in 0..8 {
                assert_eq!(row[i] as i32, want[i] >> 3, "column {i}");
            }
        }
        // hand-computed corner: g all 1000 except below = 1400. Pixel row 7 uses K[0] on the
        // (zero) upper difference and K[7-7] = K[0] on the lower: 1004 + mulhi(400, 26624).
        let mut g = [[1000; 3]; 3];
        g[2] = [1400; 3];
        let s = smooth_surface(&g);
        assert_eq!(s[7][3] as i32, (1004 + ((400 * 26624) >> 16)) >> 3);
        assert_eq!(s[0][3] as i32, 1004 >> 3);
    }

    #[test]
    fn dc_smoothing_writes_only_flat_intra_blocks() {
        let mut p = Plane::new(24, 24, 7);
        let mut b = grid([800, 800, 800, 800, 1600, 800, 800, 800, 800], &[(0, intra(1, 800))]);
        b.info[8] = inter([0, 0]);
        smooth_plane(&mut p, &b, 0);
        assert_eq!(p.data[p.idx(0, 0)], 7, "s = 1 block untouched");
        assert_eq!(p.data[p.idx(16, 16)], 7, "inter block untouched");
        assert_eq!(p.data[p.idx(8, 8)], 200, "thr 0: flat at own DC");
        assert_eq!(p.data[p.idx(15, 15)], 200);
        assert_eq!(p.data[p.idx(8, 0)], 100);
    }

    #[test]
    fn edge_strength_rules() {
        // intra/intra: skipped when equal and 0 or 3
        assert_eq!(edge_strength(&intra(3, 0), &intra(3, 9)), None);
        assert_eq!(edge_strength(&intra(0, 0), &intra(0, 9)), None);
        assert_eq!(edge_strength(&intra(1, 0), &intra(1, 0)), Some((1, 1)));
        assert_eq!(edge_strength(&intra(2, 0), &intra(2, 0)), Some((2, 2)));
        assert_eq!(edge_strength(&intra(3, 0), &intra(1, 0)), Some((3, 1)));
        // intra next to inter: the inter side is 0, and the mv rule does not apply
        assert_eq!(edge_strength(&intra(3, 0), &inter([10, 0])), Some((3, 0)));
        assert_eq!(edge_strength(&inter([10, 0]), &intra(0, 0)), None);
        // inter/inter: mv difference of 2 or more in either component
        assert_eq!(edge_strength(&inter([0, 0]), &inter([1, -1])), None);
        assert_eq!(edge_strength(&inter([0, 0]), &inter([2, 0])), Some((2, 2)));
        assert_eq!(edge_strength(&inter([0, 3]), &inter([0, 1])), Some((2, 2)));
    }

    #[test]
    fn edge_filter_pixels() {
        // d = 100 - 20 = 80: a = 20, b = 10
        assert_eq!(filter_edge([10, 20, 100, 110], 3, 3), [20, 40, 80, 100]);
        assert_eq!(filter_edge([10, 20, 100, 110], 1, 1), [10, 40, 80, 110]);
        assert_eq!(filter_edge([10, 20, 100, 110], 0, 2), [10, 20, 80, 100]);
        assert_eq!(filter_edge([10, 20, 100, 110], 2, 0), [20, 40, 100, 110]);
        // negative step and arithmetic shifts: d = -5: a = (-3) >> 2 = -1, b = (-1) >> 3 = -1
        assert_eq!(filter_edge([50, 50, 45, 45], 2, 2), [49, 49, 46, 46]);
        // d = -250: a = -248 >> 2 = -62, b = -246 >> 3 = -31
        assert_eq!(filter_edge([255, 250, 0, 0], 2, 2), [224, 188, 62, 31]);
        // p1 and q1 clip
        assert_eq!(filter_edge([255, 0, 255, 0], 2, 2), [255, 64, 191, 0]);
        assert_eq!(filter_edge([0, 0, 255, 255], 3, 3), [32, 64, 191, 223]);
        assert_eq!(filter_edge([254, 255, 255, 0], 3, 3), [254, 255, 255, 0]);
    }

    #[test]
    fn passes_respect_flags_and_order() {
        // 2×2 blocks: left column intra s = 1, right column intra s = 2.
        let mut b = PlaneBlocks::new(2, 2);
        b.info = vec![intra(1, 0), intra(2, 0), intra(1, 0), intra(2, 0)];
        let mut p = Plane::new(16, 16, 0);
        for y in 0..16 {
            for x in 8..16 {
                let i = p.idx(x, y);
                p.data[i] = 80;
            }
        }
        let base = p.clone();
        vertical_pass(&mut p, &b, &|_| true);
        assert_eq!(p.data, base.data, "flag 1 switches the edge off");
        vertical_pass(&mut p, &b, &|_| false);
        // sA = 1: p0 += 20; sB = 2: q0 -= 20, q1 -= 10
        let row: Vec<u8> = (5..11).map(|x| p.data[p.idx(x, 3)]).collect();
        assert_eq!(row, [0, 0, 20, 60, 70, 80]);
        // horizontal: (left, top) s = 1 vs (left, bottom) s = 1 -> filtered, but no step
        let before = p.clone();
        horizontal_pass(&mut p, &b, &|_| false);
        assert_eq!(p.data, before.data);
    }

    #[test]
    fn frame_flag_bits_switch_passes() {
        let mut f = FrameBuf::new(32, 32, false);
        let mut blocks: Vec<PlaneBlocks> = f.planes.iter().map(|p| PlaneBlocks::new(p.w / 8, p.h / 8)).collect();
        // luma: block (0,0) intra s=1, block (1,0) intra s=1, step 0 -> 100 at x = 8
        for y in 0..32 {
            for x in 8..32 {
                let i = f.planes[0].idx(x, y);
                f.planes[0].data[i] = 100;
            }
        }
        blocks[0].set(0, 0, intra(1, 0));
        blocks[0].set(1, 0, intra(1, 0));
        let orig = f.clone();
        // bit 14: no vertical edges
        filter_frame(&mut f, &blocks, 0x4000, None, None);
        assert_eq!(f.planes[0].data, orig.planes[0].data);
        // lists in use but not coded (bit 16 clear): all flags 0 -> filtered
        filter_frame(&mut f, &blocks, 0, None, None);
        assert_eq!(f.planes[0].data[f.planes[0].idx(7, 0)], 25);
        assert_eq!(f.planes[0].data[f.planes[0].idx(8, 0)], 75);
        // column flag C[0] = 1 switches the luma edge at x = 8 off
        let mut g = orig.clone();
        filter_frame(&mut g, &blocks, 0x18000, None, Some(&[true, false, false]));
        assert_eq!(g.planes[0].data, orig.planes[0].data);
    }

    #[test]
    fn smoothing_threshold_from_frame_flags() {
        // Two flat intra luma blocks side by side, DCs 1000 and 1016 (pixels 125 and 127).
        let mut blocks: Vec<PlaneBlocks> = vec![PlaneBlocks::new(4, 4), PlaneBlocks::new(2, 2), PlaneBlocks::new(2, 2)];
        blocks[0].set(0, 0, intra(3, 1000));
        blocks[0].set(1, 0, intra(3, 1016));
        let run = |flags: u32| {
            let mut f = FrameBuf::new(32, 32, false);
            filter_frame(&mut f, &blocks, flags, None, None);
            (0..16)
                .map(|x| f.planes[0].data[f.planes[0].idx(x, 0)])
                .collect::<Vec<u8>>()
        };
        // thr = 2·8 = 16: |1016 - 1000| is not < 16 -> both blocks flat; the edge between two
        // s = 3 blocks is skipped.
        let flat = run(2 | 0xC000);
        assert_eq!(flat, [vec![125u8; 8], vec![127u8; 8]].concat());
        // thr = 3·8 = 24: accepted -> a ramp across the edge
        let smooth = run(3 | 0xC000);
        assert_ne!(smooth, flat);
        assert_eq!(smooth[0], 125);
        assert_eq!(smooth[15], 127);
        assert!(smooth.windows(2).all(|w| w[0] <= w[1]), "{smooth:?}");
        // without bits 14/15 the edge passes run: the edge between the two s = 3 blocks is
        // skipped, the one to the (non-intra, black) block at x = 16 gets sA = 3:
        // d = -127, p0 += (-125) >> 2 = -32, p1 += (-123) >> 3 = -16
        let edges = run(3);
        assert_eq!(edges[..14], smooth[..14]);
        assert_eq!(edges[14..], [127 - 16, 127 - 32]);
    }
}
