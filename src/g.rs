//! Integer variant, revisions `g` and later (SPEC §4).

use crate::bits::BitReader;
use crate::dsp::{idct_int, median, write_int_block};
use crate::error::{Error, Result};
use crate::tables::{DC_SCALE, QM_G_CHROMA_INTRA, QM_G_INTER, QM_G_LUMA_INTRA, SCAN_G};
use crate::video::{
    block_origin, is_chroma, predict_dc, refresh_neighbours, DcNeighbours, FrameCtx, Mv, StoredMb, INTRA, MOTION,
    RESIDUE, SKIP,
};
use crate::vlc;

/// Per-MB stored state (SPEC §4.1).
#[derive(Clone, Copy, Default)]
pub(crate) struct Mb {
    pub intra_q: i32,
    pub inter_q: i32,
    pub dc: [[i32; 16]; 4],
    pub ty: u8,
    pub mv: [Mv; 4],
}

/// The current and previous MB rows.
pub(crate) struct SliceRows {
    pub cur: Vec<Mb>,
    pub prev: Vec<Mb>,
}

impl SliceRows {
    pub fn new(mb_w: usize) -> Self {
        SliceRows {
            cur: vec![Mb::default(); mb_w],
            prev: vec![Mb::default(); mb_w],
        }
    }
}

/// Per-row running state.
struct RowState {
    /// Running cbp per (plane, intra = 0 / inter = 1).
    cbp: [[u32; 2]; 4],
    lru: [u8; 4],
}

const MAX_Q: i32 = 36;

pub(crate) fn decode_slice(
    ctx: &mut FrameCtx,
    br: &mut BitReader,
    rows: &mut SliceRows,
    r0: usize,
    r1: usize,
) -> Result<()> {
    for mb in rows.prev.iter_mut() {
        mb.intra_q = 0;
        mb.inter_q = 0;
        mb.mv = [[0; 2]; 4];
    }
    for mby in r0..r1 {
        for mb in rows.cur.iter_mut() {
            mb.intra_q = 0;
            mb.inter_q = 0;
            mb.mv = [[0; 2]; 4];
        }
        let mut rs = RowState {
            cbp: [[0; 2]; 4],
            lru: [MOTION, RESIDUE, SKIP, INTRA],
        };
        for mbx in 0..ctx.mb_w {
            decode_mb(ctx, br, rows, &mut rs, mbx, mby, mby == r0).inspect_err(|_| ctx.fail_mb = Some((mbx, mby)))?;
        }
        std::mem::swap(&mut rows.cur, &mut rows.prev);
    }
    Ok(())
}

/// `read_type` with the LRU list (SPEC §4.2).
pub(crate) fn read_type(br: &mut BitReader, lru: &mut [u8; 4]) -> Result<u8> {
    Ok(match br.zeros(3)? {
        0 => lru[0],
        1 => {
            let t = lru[1];
            lru.swap(0, 1);
            t
        }
        2 => {
            let t = lru[3];
            lru.swap(2, 3);
            t
        }
        _ => {
            let t = lru[2];
            lru.swap(1, 2);
            t
        }
    })
}

/// `read_dq` (SPEC §4.2).
pub(crate) fn read_dq(br: &mut BitReader) -> Result<i32> {
    let mut n = br.zeros(4)? as i32;
    if n == 3 {
        n += br.bit()? as i32;
    } else if n == 4 {
        // Only when zeros() returned 4 (SPEC draft 4; drafts 1-3 had a separate `if`).
        n += br.bits(5)? as i32 + 1;
    }
    if n != 0 && br.bit()? {
        n = -n;
    }
    Ok(n)
}

/// `pred_q` (SPEC §4.2) from the left, top and top-left values.
pub(crate) fn pred_q(top_edge: bool, left_edge: bool, l: i32, t: i32, tl: i32) -> i32 {
    match (top_edge, left_edge) {
        (true, true) => 16,
        (true, false) => l,
        (false, true) => t,
        (false, false) => median(t, l, tl),
    }
}

/// `read_luma_cbp` (SPEC §4.3).
pub(crate) fn read_luma_cbp(br: &mut BitReader, prev: u32, frame_flags: u32) -> Result<u32> {
    let mut k = (prev & 0xFFFF).count_ones();
    let mut invert = false;
    if k > 7 {
        k = 16 - k;
        invert = true;
    }
    let mut cbp = 0u32;
    if !br.bit()? {
        if k < 4 {
            for i in 0..4 {
                if !br.bit()? {
                    cbp |= br.bits(4)? << (4 * i);
                }
            }
        } else {
            cbp = br.bits(16)?;
        }
    }
    if invert {
        cbp ^= 0xFFFF;
    }
    if (frame_flags & 0x40000 == 0 || cbp != 0) && br.bit()? {
        cbp |= cbp << 16;
    }
    Ok(cbp)
}

/// `read_chroma_cbp` (SPEC §4.3).
pub(crate) fn read_chroma_cbp(br: &mut BitReader, prev: u32) -> Result<u32> {
    let mut cbp = (prev & 0xF0000) | if (prev & 0xF).count_ones() >= 2 { 0xF } else { 0 };
    if !br.bit()? {
        cbp = br.bits(4)?;
        if br.bit()? {
            cbp |= cbp << 16;
        }
    }
    Ok(cbp)
}

/// Reads the DC residuals of `n` blocks (first half of `read_dc`, SPEC §4.3).
pub(crate) fn read_dc_residuals(br: &mut BitReader, n: usize, q: i32) -> Result<[i32; 16]> {
    let scale = DC_SCALE[q.clamp(8, MAX_Q) as usize] as i64;
    let mut r = [0i32; 16];
    if br.bit()? {
        for v in r.iter_mut().take(n) {
            let mut c = br.ones(12)? as i64;
            if c > 3 {
                c = (1i64 << (c - 3)) + br.bits((c - 3) as u32)? as i64 + 2;
            }
            if c != 0 && br.bit()? {
                c = -c;
            }
            *v = ((c * scale + 512) >> 10) as i32;
        }
    }
    Ok(r)
}

/// Decodes the AC coefficients of a group of 4 blocks (SPEC §4.4).
pub(crate) fn read_ac_group(br: &mut BitReader, p: u32, q: i32, qm: &[[i32; 64]; 4]) -> Result<[[i16; 64]; 4]> {
    let mut coef = [[0i16; 64]; 4];
    if p & 0xF == 0 {
        return Ok(coef);
    }
    let table = if p >> 16 != 0 { vlc::g_skip1() } else { vlc::g_skip0() };
    let m = &qm[(q & 3) as usize];
    let shift = (q >> 2) as u32;
    for (b, blk) in coef.iter_mut().enumerate() {
        if p & (1 << b) == 0 {
            continue;
        }
        let mut pos = 1usize;
        let mut pending = 0i32;
        while pos < 64 {
            pending -= 1;
            if pending < 1 {
                let s = table.read(br)?;
                pending = if s == 13 { 7 } else { 0 };
                pos += match s {
                    0..=10 => s as usize,
                    11 => br.bits(6)? as usize,
                    12 => 64,
                    _ => 0,
                };
                if pos >= 64 {
                    break;
                }
            }
            let mut v = br.ones(12)? as i64 + 1;
            if v > 3 {
                v = br.bits((v - 3) as u32)? as i64 + (1i64 << (v - 3)) + 2;
            }
            if br.bit()? {
                v = -v;
            }
            let i = SCAN_G[pos] as usize;
            // Note: the product can overflow 32 bits only for absurd levels at high q, where
            // any wrapping is acceptable (SPEC §4.4). We compute in 64 bits and wrap to int16.
            blk[i] = ((((v * m[i] as i64) << shift) + 64) >> 7) as i16;
            pos += 1;
        }
    }
    Ok(coef)
}

/// Reads motion vector deltas (SPEC §4.5). Returns (n, deltas).
pub(crate) fn read_mv_deltas(br: &mut BitReader) -> Result<(usize, [Mv; 4])> {
    let n = if br.bit()? { 1 } else { 4 };
    let mut d = [[0i32; 2]; 4];
    for c in 0..2 {
        for dj in d.iter_mut().take(n) {
            let s = vlc::g_mv().read(br)? as i32;
            dj[c] = if s <= 7 {
                s
            } else if s <= 14 {
                s - 15
            } else {
                let w = br.zeros(12)? + 4;
                let u = br.bits(w)? as i32 + (1i32 << w) - 1;
                if u & 1 != 0 {
                    -(u >> 1) - 1
                } else {
                    u >> 1
                }
            };
        }
    }
    Ok((n, d))
}

/// Motion vector prediction (SPEC §4.6). `n` = 1 or 4 deltas.
pub(crate) fn predict_mv(
    n: usize,
    d: &[Mv; 4],
    top_edge: bool,
    left_edge: bool,
    l: &[Mv; 4],
    t: &[Mv; 4],
    tl: &[Mv; 4],
) -> [Mv; 4] {
    let mut out = [[0i32; 2]; 4];
    for c in 0..2 {
        let (l0, l1, l3) = (l[0][c], l[1][c], l[3][c]);
        let (t0, t2, t3) = (t[0][c], t[2][c], t[3][c]);
        let tl3 = tl[3][c];
        let dd = |j: usize| d[j][c];
        let add = |a: i32, b: i32| a.wrapping_add(b);
        if n == 1 {
            let p = match (top_edge, left_edge) {
                (true, true) => 0,
                (true, false) => median(l0, l1, l3),
                (false, true) => median(t0, t2, t3),
                (false, false) => median(tl3, t2, l1),
            };
            let v = add(dd(0), p);
            for o in out.iter_mut() {
                o[c] = v;
            }
            continue;
        }
        let (c0, c1, c2);
        match (top_edge, left_edge) {
            (false, false) => {
                c0 = add(dd(0), median(t2, tl3, l1));
                c1 = add(dd(1), median(t2, t3, c0));
                c2 = add(dd(2), median(l1, l3, c0));
            }
            (false, true) => {
                c0 = add(dd(0), median(t0, t2, t3));
                c1 = add(dd(1), median(t2, t3, c0));
                c2 = add(dd(2), median(t2, c0, c1));
            }
            (true, false) => {
                c0 = add(dd(0), median(l0, l1, l3));
                c2 = add(dd(2), median(l1, l3, c0));
                c1 = add(dd(1), median(l1, c0, c2));
            }
            (true, true) => {
                c0 = dd(0);
                c1 = add(dd(1), dd(0));
                c2 = add(dd(2), dd(0));
            }
        }
        let c3 = add(dd(3), median(c0, c1, c2));
        out[0][c] = c0;
        out[1][c] = c1;
        out[2][c] = c2;
        out[3][c] = c3;
    }
    out
}

impl StoredMb for Mb {
    type T = i32;
    fn ty(&self) -> u8 {
        self.ty
    }
    fn dc_mut(&mut self) -> &mut [[i32; 16]; 4] {
        &mut self.dc
    }
}

fn decode_mb(
    ctx: &mut FrameCtx,
    br: &mut BitReader,
    rows: &mut SliceRows,
    rs: &mut RowState,
    mbx: usize,
    mby: usize,
    top_edge: bool,
) -> Result<()> {
    let left_edge = mbx == 0;
    let ty = if ctx.keyframe {
        INTRA
    } else {
        read_type(br, &mut rs.lru)?
    };
    rows.cur[mbx].ty = ty;
    let nplanes = if ctx.has_alpha { 4 } else { 3 };
    if ty == INTRA {
        refresh_neighbours(ctx.cur, &mut rows.cur, &mut rows.prev, mbx, mby, top_edge, nplanes);
    }
    let zero = Mb::default();
    let l = if left_edge { zero } else { rows.cur[mbx - 1] };
    let t = if top_edge { zero } else { rows.prev[mbx] };
    let tl = if top_edge || left_edge {
        zero
    } else {
        rows.prev[mbx - 1]
    };
    let pq_intra = pred_q(top_edge, left_edge, l.intra_q, t.intra_q, tl.intra_q);
    let pq_inter = pred_q(top_edge, left_edge, l.inter_q, t.inter_q, tl.inter_q);
    let mut me = rows.cur[mbx];
    match ty {
        INTRA => {
            me.mv = predict_mv(4, &[[0; 2]; 4], top_edge, left_edge, &l.mv, &t.mv, &tl.mv);
            me.inter_q = pq_inter;
            me.intra_q = pq_intra + read_dq(br)?;
            if !(0..=MAX_Q).contains(&me.intra_q) {
                return Err(Error::InvalidData("intra q out of range"));
            }
            for p in 0..nplanes {
                let nb = DcNeighbours {
                    left: (!left_edge).then_some(l.dc[p]),
                    top: (!top_edge).then_some(t.dc[p]),
                    top_left: (!top_edge && !left_edge).then_some(tl.dc[p]),
                };
                decode_plane(
                    ctx,
                    br,
                    p,
                    true,
                    me.intra_q,
                    &mut rs.cbp[p][0],
                    &nb,
                    mbx,
                    mby,
                    &mut me.dc[p],
                )?;
            }
        }
        SKIP => {
            me.intra_q = pq_intra;
            me.inter_q = pq_inter;
            ctx.copy_mb(mbx, mby)?;
        }
        _ => {
            me.intra_q = pq_intra;
            let (n, d) = read_mv_deltas(br)?;
            me.mv = predict_mv(n, &d, top_edge, left_edge, &l.mv, &t.mv, &tl.mv);
            me.inter_q = if ty == RESIDUE {
                pq_inter + read_dq(br)?
            } else {
                pq_inter
            };
            if !(0..=MAX_Q).contains(&me.inter_q) {
                return Err(Error::InvalidData("inter q out of range"));
            }
            let planes: &[usize] = if ctx.has_alpha { &[0, 1, 2, 3] } else { &[0, 1, 2] };
            ctx.mc_mb(mbx, mby, &me.mv, planes)?;
            if ty == RESIDUE {
                let nb = DcNeighbours::none();
                let q = me.inter_q;
                decode_plane(ctx, br, 0, false, q, &mut rs.cbp[0][1], &nb, mbx, mby, &mut me.dc[0])?;
                if br.bit()? {
                    for p in 1..3 {
                        decode_plane(ctx, br, p, false, q, &mut rs.cbp[p][1], &nb, mbx, mby, &mut me.dc[p])?;
                    }
                } else {
                    rs.cbp[1][1] = 0;
                    rs.cbp[2][1] = 0;
                }
                if ctx.has_alpha {
                    decode_plane(ctx, br, 3, false, q, &mut rs.cbp[3][1], &nb, mbx, mby, &mut me.dc[3])?;
                }
            }
        }
    }
    if let Some(info) = ctx.mb_info.get_mut(mby * ctx.mb_w + mbx) {
        *info = crate::video::MbInfo {
            mb_type: ty,
            intra_q: me.intra_q,
            inter_q: me.inter_q,
        };
    }
    rows.cur[mbx] = me;
    Ok(())
}

/// Codes one plane of one MB (SPEC §4.3).
#[allow(clippy::too_many_arguments)]
fn decode_plane(
    ctx: &mut FrameCtx,
    br: &mut BitReader,
    p: usize,
    intra: bool,
    q: i32,
    cbp_run: &mut u32,
    nb: &DcNeighbours<i32>,
    mbx: usize,
    mby: usize,
    dc: &mut [i32; 16],
) -> Result<()> {
    let chroma = is_chroma(p);
    let cbp = if chroma {
        read_chroma_cbp(br, *cbp_run)?
    } else {
        read_luma_cbp(br, *cbp_run, ctx.frame_flags)?
    };
    *cbp_run = cbp;
    let n = if chroma { 4 } else { 16 };
    let r = read_dc_residuals(br, n, q)?;
    let (lo, hi) = if intra { (0, 2047) } else { (-1023, 1023) };
    predict_dc(n, &r, nb, intra, lo, hi, dc);
    let qm = if !intra {
        &QM_G_INTER
    } else if chroma {
        &QM_G_CHROMA_INTRA
    } else {
        &QM_G_LUMA_INTRA
    };
    let Some(plane) = ctx.cur.planes.get_mut(p) else {
        return Ok(());
    };
    for g in 0..n / 4 {
        let mut coef = read_ac_group(br, cbp >> (4 * g), q, qm)?;
        for (b, blk) in coef.iter_mut().enumerate() {
            let k = 4 * g + b;
            blk[0] = (dc[k] * 8 + 32) as i16;
            idct_int(blk);
            let (x, y) = block_origin(p, k, mbx, mby);
            write_int_block(plane, x, y, blk, !intra);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vlc::tests::pack;

    #[test]
    fn type_lru() {
        let d = pack(&["1", "01", "001", "000"]);
        let mut r = BitReader::new(&d);
        let mut lru = [MOTION, RESIDUE, SKIP, INTRA];
        assert_eq!(read_type(&mut r, &mut lru).unwrap(), MOTION);
        assert_eq!(read_type(&mut r, &mut lru).unwrap(), RESIDUE);
        assert_eq!(lru, [RESIDUE, MOTION, SKIP, INTRA]);
        assert_eq!(read_type(&mut r, &mut lru).unwrap(), INTRA);
        assert_eq!(lru, [RESIDUE, MOTION, INTRA, SKIP]);
        assert_eq!(read_type(&mut r, &mut lru).unwrap(), INTRA);
        assert_eq!(lru, [RESIDUE, INTRA, MOTION, SKIP]);
    }

    #[test]
    fn dq_values() {
        let cases: &[(&[&str], i32)] = &[
            (&["1"], 0),
            (&["01", "0"], 1),
            (&["01", "1"], -1),
            (&["001", "1"], -2),
            (&["0001", "0", "0"], 3),
            (&["0000", "00000", "0"], 5),
            (&["0000", "11111", "1"], -36),
        ];
        for (bits, want) in cases {
            let d = pack(bits);
            assert_eq!(read_dq(&mut BitReader::new(&d)).unwrap(), *want, "{bits:?}");
        }
    }

    #[test]
    fn dq_three_plus_one_is_four_without_escape() {
        // Regression (SPEC draft 4): "0001 1 0" is zeros = 3, +1 = 4, sign 0: +4 in 6 bits.
        // Drafts 1-3 also read the 5-bit escape here, which desynchronised real streams.
        let d = pack(&["0001", "1", "0", "11111"]);
        let mut r = BitReader::new(&d);
        assert_eq!(read_dq(&mut r).unwrap(), 4);
        assert_eq!(r.position(), 6);
        let d = pack(&["0001", "1", "1"]);
        assert_eq!(read_dq(&mut BitReader::new(&d)).unwrap(), -4);
    }

    #[test]
    fn pred_q_rule() {
        assert_eq!(pred_q(true, true, 1, 2, 3), 16);
        assert_eq!(pred_q(true, false, 1, 2, 3), 1);
        assert_eq!(pred_q(false, true, 1, 2, 3), 2);
        assert_eq!(pred_q(false, false, 9, 2, 5), 5);
    }

    #[test]
    fn luma_cbp() {
        // prev 0: k = 0 < 4; first bit 0, nibble 0: bit 0 + 1010, nibbles 1-3 skipped (bit 1),
        // run-table bit 1
        let d = pack(&["0", "0", "0101", "1", "1", "1", "1"]);
        assert_eq!(read_luma_cbp(&mut BitReader::new(&d), 0, 0).unwrap(), 0xA | (0xA << 16));
        // prev with 9 bits set: invert; first bit 1 -> 0 ^ 0xFFFF; flag 0
        let d = pack(&["1", "0"]);
        assert_eq!(read_luma_cbp(&mut BitReader::new(&d), 0x1FF, 0).unwrap(), 0xFFFF);
        // k = 4: 16 raw bits
        let d = pack(&["0", "1000000000000001", "0"]);
        assert_eq!(read_luma_cbp(&mut BitReader::new(&d), 0xF, 0).unwrap(), 0x8001);
        // frame flag 0x40000 with cbp 0: no run-table bit read
        let d = pack(&["1"]);
        let mut r = BitReader::new(&d);
        assert_eq!(read_luma_cbp(&mut r, 0, 0x40000).unwrap(), 0);
        assert_eq!(r.position(), 1);
    }

    #[test]
    fn chroma_cbp() {
        let d = pack(&["1"]);
        assert_eq!(read_chroma_cbp(&mut BitReader::new(&d), 0x3_0003).unwrap(), 0x3_000F);
        assert_eq!(read_chroma_cbp(&mut BitReader::new(&d), 0x1).unwrap(), 0);
        let d = pack(&["0", "1100", "1"]);
        assert_eq!(read_chroma_cbp(&mut BitReader::new(&d), 0).unwrap(), 0x3_0003);
    }

    #[test]
    fn dc_residuals() {
        // q 16 -> scale 16384: c = 1 -> (16384 + 512) >> 10 = 16
        let d = pack(&["1", "10", "0", "110", "1"]);
        let r = read_dc_residuals(&mut BitReader::new(&d), 2, 16).unwrap();
        assert_eq!(&r[..2], &[16, -32]);
        // escape: c = ones = 4 -> 2 + bits(1) + 2; q 0 uses DC_SCALE[8] = 4096
        let d = pack(&["1", "11110", "1", "0"]);
        let r = read_dc_residuals(&mut BitReader::new(&d), 1, 0).unwrap();
        assert_eq!(r[0], (5 * 4096 + 512) >> 10);
    }

    #[test]
    fn ac_group_end_of_block_and_run() {
        // block 0 only, G_SKIP0. Run symbol 0 ("1"): pos 1. Level ones=0 -> v = 1, sign 0.
        // Then run symbol 12 ("01") ends the block.
        let d = pack(&["1", "0", "0", "01"]);
        let mut r = BitReader::new(&d);
        let c = read_ac_group(&mut r, 1, 0, &QM_G_INTER).unwrap();
        let i = SCAN_G[1] as usize;
        assert_eq!(c[0][i], ((QM_G_INTER[0][i] + 64) >> 7) as i16);
        assert_eq!(c[0].iter().filter(|&&v| v != 0).count(), 1);
        assert_eq!(r.position(), 5);
        // nothing read when no cbp bit
        let mut r = BitReader::new(&d);
        read_ac_group(&mut r, 0x1_0000, 0, &QM_G_INTER).unwrap();
        assert_eq!(r.position(), 0);
    }

    #[test]
    fn ac_group_back_to_back() {
        // run 13 ("00111"): 7 coefficients at pos 1..7 without run codes, then EOB.
        let mut parts = vec!["00111"];
        for _ in 0..7 {
            parts.extend(["0", "1"]); // v = 1, negative
        }
        parts.push("01");
        let d = pack(&parts);
        let mut r = BitReader::new(&d);
        let c = read_ac_group(&mut r, 1, 4, &QM_G_INTER).unwrap();
        for &i in &SCAN_G[1..8] {
            let i = i as usize;
            assert_eq!(c[0][i], ((-(QM_G_INTER[0][i] as i64) * 2 + 64) >> 7) as i16);
        }
        assert_eq!(r.position(), 5 + 7 * 2 + 2);
    }

    #[test]
    fn mv_deltas() {
        // n = 1: x = symbol 1 ("011") = 1, y = symbol 14 ("010") = -1
        let d = pack(&["1", "011", "010"]);
        let (n, dd) = read_mv_deltas(&mut BitReader::new(&d)).unwrap();
        assert_eq!((n, dd[0]), (1, [1, -1]));
        // escape: zeros(12) = 0 -> w = 4, u = bits(4) + 15; bits 3 -> u = 18 -> 9; then u = 19 -> -10
        let mut parts = vec!["0"];
        parts.extend(["0000", "1", "1100"]);
        parts.extend(["0000", "1", "0010"]);
        parts.extend(["1", "1"]);
        parts.extend(["1"; 4]);
        let d = pack(&parts);
        let (n, dd) = read_mv_deltas(&mut BitReader::new(&d)).unwrap();
        assert_eq!(n, 4);
        assert_eq!([dd[0][0], dd[1][0], dd[2][0], dd[3][0]], [9, -10, 0, 0]);
    }

    fn mvs(a: [i32; 4]) -> [Mv; 4] {
        [[a[0], -a[0]], [a[1], -a[1]], [a[2], -a[2]], [a[3], -a[3]]]
    }

    #[test]
    fn mv_prediction_one_vector() {
        let l = mvs([1, 5, 0, 9]);
        let t = mvs([2, 0, 7, 3]);
        let tl = mvs([0, 0, 0, 4]);
        let d = mvs([10, 0, 0, 0]);
        let get = |te, le| predict_mv(1, &d, te, le, &l, &t, &tl);
        assert_eq!(get(true, true), [[10, -10]; 4]);
        assert_eq!(get(true, false), [[10 + 5, -15]; 4]); // median(1, 5, 9)
        assert_eq!(get(false, true), [[10 + 3, -13]; 4]); // median(2, 7, 3)
        assert_eq!(get(false, false), [[10 + 5, -15]; 4]); // median(4, 7, 5)
    }

    #[test]
    fn mv_prediction_four_vectors() {
        let l = mvs([1, 5, 0, 9]);
        let t = mvs([2, 0, 7, 3]);
        let tl = mvs([0, 0, 0, 4]);
        let d = mvs([1, 2, 3, 4]);
        let x = |v: [Mv; 4]| [v[0][0], v[1][0], v[2][0], v[3][0]];
        // none: c0 = 1 + med(7,4,5)=6; c1 = 2 + med(7,3,6)=8; c2 = 3 + med(5,9,6)=9; c3 = 4 + 8
        assert_eq!(x(predict_mv(4, &d, false, false, &l, &t, &tl)), [6, 8, 9, 12]);
        // left only: c0 = 1 + med(2,7,3)=4; c1 = 2 + med(7,3,4)=6; c2 = 3 + med(7,4,6)=9; c3 = 4 + 6
        assert_eq!(x(predict_mv(4, &d, false, true, &l, &t, &tl)), [4, 6, 9, 10]);
        // top only: c0 = 1 + med(1,5,9)=6; c2 = 3 + med(5,9,6)=9; c1 = 2 + med(5,6,9)=8; c3 = 4 + 8
        assert_eq!(x(predict_mv(4, &d, true, false, &l, &t, &tl)), [6, 8, 9, 12]);
        // both: c0 = 1; c1 = 3; c2 = 4; c3 = 4 + 3
        assert_eq!(x(predict_mv(4, &d, true, true, &l, &t, &tl)), [1, 3, 4, 7]);
        // y components are the negations, and median commutes with negation
        let p = predict_mv(4, &d, false, false, &l, &t, &tl);
        assert_eq!([p[0][1], p[1][1], p[2][1], p[3][1]], [-6, -8, -9, -12]);
    }
}
