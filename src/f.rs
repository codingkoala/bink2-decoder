//! Float variant, revisions `a`, `d`, `f` (SPEC §5).

use crate::bits::BitReader;
use crate::dsp::{idct_float, median, write_float_block};
use crate::error::{Error, Result};
use crate::tables::{F_AC_Q, F_CHROMA, F_DC_BITS, F_DC_Q, F_LUMA_INTER, F_LUMA_INTRA, SCAN_F_CHROMA, SCAN_F_LUMA};
use crate::video::{
    block_origin, is_chroma, predict_dc, refresh_neighbours, DcNeighbours, FrameCtx, Mv, StoredMb, INTRA, SKIP,
};
use crate::vlc;

/// Per-MB stored state.
#[derive(Clone, Copy, Default)]
pub(crate) struct Mb {
    pub dc: [[f32; 16]; 4],
    pub ty: u8,
    pub mv: [Mv; 4],
}

impl StoredMb for Mb {
    type T = f32;
    fn ty(&self) -> u8 {
        self.ty
    }
    fn dc_mut(&mut self) -> &mut [[f32; 16]; 4] {
        &mut self.dc
    }
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

struct RowState {
    /// Running q per (plane, intra = 0 / inter = 1).
    q: [[i32; 2]; 4],
    /// Running cbp per (plane, intra / inter).
    cbp: [[u32; 2]; 4],
}

const MAX_Q: i32 = 15;

pub(crate) fn decode_slice(
    ctx: &mut FrameCtx,
    br: &mut BitReader,
    rows: &mut SliceRows,
    r0: usize,
    r1: usize,
) -> Result<()> {
    for mb in rows.prev.iter_mut() {
        mb.mv = [[0; 2]; 4];
    }
    for mby in r0..r1 {
        for mb in rows.cur.iter_mut() {
            mb.mv = [[0; 2]; 4];
        }
        let mut rs = RowState {
            q: [[8; 2]; 4],
            cbp: [[0; 2]; 4],
        };
        for mbx in 0..ctx.mb_w {
            decode_mb(ctx, br, rows, &mut rs, mbx, mby, mby == r0).inspect_err(|_| ctx.fail_mb = Some((mbx, mby)))?;
        }
        std::mem::swap(&mut rows.cur, &mut rows.prev);
    }
    Ok(())
}

/// Luma coded-block pattern (SPEC §5.2).
// Note: `h |=` ORs in the bit's mask (1, 2, 4 or 8), and the initial `nib` and `h` seeds really
// take nibble 1 of `prev` (SPEC §5.2, draft 2; matches the reference, possibly a reference bug).
pub(crate) fn read_luma_cbp(br: &mut BitReader, prev: u32) -> Result<u32> {
    let lo = if br.bit()? {
        if br.bit()? {
            return Ok(prev);
        }
        prev & 0xFFFF
    } else {
        let mut l = 0u32;
        let mut nib = (prev >> 4) & 0xF;
        for i in 0..4 {
            if !br.bit()? {
                nib = br.bits(4)?;
            }
            l |= nib << (4 * i);
        }
        l
    };
    let mut hi = 0u32;
    let mut h = (prev >> 20) & 0xF;
    for i in 0..4 {
        let m = (lo >> (4 * i)) & 0xF;
        if m == 0 {
            h = 0;
        } else if m.count_ones() == 1 || !br.bit()? {
            h = 0;
            for j in 0..4 {
                if m & (1 << j) != 0 && br.bit()? {
                    h |= 1 << j;
                }
            }
        }
        h &= m;
        hi |= h << (4 * i);
    }
    Ok(lo | hi << 16)
}

/// Chroma coded-block pattern (SPEC §5.2).
pub(crate) fn read_chroma_cbp(br: &mut BitReader, prev: u32) -> Result<u32> {
    let lo = if br.bit()? {
        if br.bit()? {
            return Ok(prev);
        }
        prev & 0xF
    } else {
        br.bits(4)?
    };
    if lo == 0 {
        return Ok(lo);
    }
    if lo.count_ones() >= 2 {
        let h = (prev >> 16) & lo;
        if br.bit()? {
            return Ok(lo | h << 16);
        }
    }
    let mut h = 0;
    for j in 0..4 {
        if lo & (1 << j) != 0 && br.bit()? {
            h |= 1 << j;
        }
    }
    Ok(lo | h << 16)
}

/// Reads a `bits(3)` (+ `bits(2)` when 7) width.
fn read_width(br: &mut BitReader) -> Result<u32> {
    let mut w = br.bits(3)?;
    if w == 7 {
        w += br.bits(2)?;
    }
    Ok(w)
}

/// Reads four `bits(w)` magnitudes followed by sign bits for the non-zero ones.
fn read_four(br: &mut BitReader, w: u32, out: &mut [i32]) -> Result<()> {
    for v in out.iter_mut() {
        *v = br.bits(w)? as i32;
    }
    for v in out.iter_mut() {
        if *v != 0 && br.bit()? {
            *v = -*v;
        }
    }
    Ok(())
}

/// DC residuals (SPEC §5.3), already multiplied by `F_DC_Q[q]`.
pub(crate) fn read_dc_residuals(br: &mut BitReader, n: usize, q: i32, intra_corner: bool) -> Result<[f32; 16]> {
    let w = read_width(br)?;
    let mut r = [0i32; 16];
    if w > 0 {
        for g in 0..n / 4 {
            read_four(br, w, &mut r[4 * g..4 * g + 4])?;
        }
    }
    if intra_corner {
        let e = F_DC_BITS[q as usize] - 1 + w as i32;
        if e < 10 {
            let mut v = br.bits((10 - e) as u32)? as i32;
            if v != 0 {
                v <<= w;
                if br.bit()? {
                    v = -v;
                }
            }
            r[0] += v;
        }
    }
    let scale = F_DC_Q[q as usize];
    let mut out = [0f32; 16];
    for (o, v) in out.iter_mut().zip(r.iter()).take(n) {
        *o = *v as f32 * scale;
    }
    Ok(out)
}

/// AC coefficients of one block (SPEC §5.3).
pub(crate) fn read_ac(
    br: &mut BitReader,
    second_tables: bool,
    q: i32,
    scan: &[u8; 64],
    m: &[f32; 64],
    coef: &mut [f32; 64],
) -> Result<()> {
    let (level, run) = if second_tables {
        (vlc::f_level1(), vlc::f_skip1())
    } else {
        (vlc::f_level0(), vlc::f_skip0())
    };
    let aq = F_AC_Q[q as usize];
    let mut pos = 1usize;
    let mut pending = 0i32;
    loop {
        let s = level.read(br)?;
        let v = if s == 0 {
            0
        } else {
            let mut v = if s <= 3 {
                s as i32
            } else {
                br.bits(s - 3)? as i32 + (1 << (s - 3)) + 2
            };
            if br.bit()? {
                v = -v;
            }
            v
        };
        let i = scan[pos] as usize;
        coef[i] = (v as f32 * aq) * m[i];
        if pos > 62 {
            break;
        }
        pos += 1;
        pending -= 1;
        if pending < 1 {
            let s = run.read(br)?;
            pending = if s == 13 { 7 } else { 0 };
            pos += match s {
                0..=10 => s as usize,
                11 => br.bits(6)? as usize,
                12 => 62,
                _ => 0,
            };
        }
        if pos >= 64 {
            break;
        }
    }
    Ok(())
}

/// Motion vector deltas (SPEC §5.4).
pub(crate) fn read_mv_deltas(br: &mut BitReader, both_edges: bool) -> Result<[Mv; 4]> {
    let mut d = [[0i32; 2]; 4];
    for c in 0..2 {
        let w = read_width(br)?;
        let mut v = [0i32; 4];
        if w > 0 {
            read_four(br, w, &mut v)?;
        }
        if both_edges {
            let mut o = br.bits(5)? as i32 * 16;
            if o != 0 && br.bit()? {
                o = -o;
            }
            for x in v.iter_mut() {
                *x += o;
            }
        }
        for (dj, x) in d.iter_mut().zip(v) {
            dj[c] = x;
        }
    }
    Ok(d)
}

/// Four-vector prediction of the float variant (SPEC §5.4).
pub(crate) fn predict_mv(
    d: &[Mv; 4],
    top_edge: bool,
    left_edge: bool,
    l: &[Mv; 4],
    t: &[Mv; 4],
    tl: &[Mv; 4],
) -> [Mv; 4] {
    if top_edge && left_edge {
        return *d;
    }
    let mut out = [[0i32; 2]; 4];
    for c in 0..2 {
        let (l0, l1, l3) = (l[0][c], l[1][c], l[3][c]);
        let (t0, t2, t3) = (t[0][c], t[2][c], t[3][c]);
        let tl3 = tl[3][c];
        let dd = |j: usize| d[j][c];
        let add = |a: i32, b: i32| a.wrapping_add(b);
        let (c0, c1, c2);
        if !top_edge && !left_edge {
            c0 = add(dd(0), median(t2, tl3, l1));
            c1 = add(dd(1), median(t2, t3, c0));
            // SPEC-QUESTION: SPEC §5.4 (unverified) uses median(T2, c0, c1) here, unlike the
            // integer variant's median(L1, L3, c0); we follow the float rule as written.
            c2 = add(dd(2), median(t2, c0, c1));
        } else if left_edge {
            c0 = add(dd(0), median(t0, t2, t3));
            c1 = add(dd(1), median(t2, t3, c0));
            c2 = add(dd(2), median(t2, c0, c1));
        } else {
            c0 = add(dd(0), median(l0, l1, l3));
            c2 = add(dd(2), median(l1, l3, c0));
            c1 = add(dd(1), median(l1, c0, c2));
        }
        out[0][c] = c0;
        out[1][c] = c1;
        out[2][c] = c2;
        out[3][c] = add(dd(3), median(c0, c1, c2));
    }
    out
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
    // Keyframes read no type bits and every MB is INTRA; otherwise bits(2) uses the §4.1
    // numbering (0 INTRA, 1 SKIP, 2 MOTION, 3 RESIDUE) (SPEC §5.1).
    let ty = if ctx.keyframe { INTRA } else { br.bits(2)? as u8 };
    rows.cur[mbx].ty = ty;
    if ty == INTRA {
        // No alpha in the float variant's refresh (SPEC §5.1).
        refresh_neighbours(&ctx.cur, &mut rows.cur, &mut rows.prev, mbx, mby, top_edge, 3);
    }
    let zero = Mb::default();
    let l = if left_edge { zero } else { rows.cur[mbx - 1] };
    let t = if top_edge { zero } else { rows.prev[mbx] };
    let tl = if top_edge || left_edge {
        zero
    } else {
        rows.prev[mbx - 1]
    };
    let mut me = rows.cur[mbx];
    let nplanes = if ctx.has_alpha { 4 } else { 3 };
    match ty {
        INTRA => {
            me.mv = predict_mv(&[[0; 2]; 4], top_edge, left_edge, &l.mv, &t.mv, &tl.mv);
            for p in 0..nplanes {
                let nb = DcNeighbours {
                    left: (!left_edge).then_some(l.dc[p]),
                    top: (!top_edge).then_some(t.dc[p]),
                    top_left: (!top_edge && !left_edge).then_some(tl.dc[p]),
                };
                decode_plane(
                    ctx,
                    br,
                    rs,
                    p,
                    true,
                    top_edge && left_edge,
                    &nb,
                    mbx,
                    mby,
                    &mut me.dc[p],
                )?;
            }
        }
        SKIP => ctx.copy_mb(mbx, mby)?,
        _ => {
            let d = read_mv_deltas(br, top_edge && left_edge)?;
            me.mv = predict_mv(&d, top_edge, left_edge, &l.mv, &t.mv, &tl.mv);
            ctx.mc_mb(mbx, mby, &me.mv, &[0, 1, 2])?;
            if ctx.has_alpha {
                // Note: the reference does not motion-compensate alpha in the float variant and
                // leaves it undefined (SPEC §5.1, unverified). Copying the co-located reference
                // alpha is the documented stand-in; RESIDUE then adds its alpha residual to it.
                ctx.copy_mb_plane(3, mbx, mby)?;
            }
            if ty != crate::video::MOTION {
                let nb = DcNeighbours::none();
                for p in 0..nplanes {
                    decode_plane(ctx, br, rs, p, false, false, &nb, mbx, mby, &mut me.dc[p])?;
                }
            }
        }
    }
    ctx.set_mb_info(
        mbx,
        mby,
        crate::video::MbInfo {
            mb_type: ty,
            intra_q: 0,
            inter_q: 0,
        },
    );
    rows.cur[mbx] = me;
    Ok(())
}

/// Codes one plane of one MB: cbp, dq, DC, AC (SPEC §5.1, §5.3).
#[allow(clippy::too_many_arguments)]
fn decode_plane(
    ctx: &mut FrameCtx,
    br: &mut BitReader,
    rs: &mut RowState,
    p: usize,
    intra: bool,
    both_edges: bool,
    nb: &DcNeighbours<f32>,
    mbx: usize,
    mby: usize,
    dc: &mut [f32; 16],
) -> Result<()> {
    let chroma = is_chroma(p);
    let kind = if intra { 0 } else { 1 };
    let cbp = if chroma {
        read_chroma_cbp(br, rs.cbp[p][kind])?
    } else {
        read_luma_cbp(br, rs.cbp[p][kind])?
    };
    rs.cbp[p][kind] = cbp;
    let mut dq = vlc::f_dq().read(br)? as i32;
    if dq != 0 && br.bit()? {
        dq = -dq;
    }
    let q = rs.q[p][kind] + dq;
    if !(0..=MAX_Q).contains(&q) {
        return Err(Error::InvalidData("float q out of range"));
    }
    rs.q[p][kind] = q;
    let n = if chroma { 4 } else { 16 };
    let r = read_dc_residuals(br, n, q, intra && both_edges)?;
    let (lo, hi) = if intra { (0.0, 2047.0) } else { (-1023.0, 1023.0) };
    predict_dc(n, &r, nb, intra, lo, hi, dc);
    let (scan, m) = if chroma {
        (&SCAN_F_CHROMA, &F_CHROMA)
    } else if intra {
        (&SCAN_F_LUMA, &F_LUMA_INTRA)
    } else {
        (&SCAN_F_LUMA, &F_LUMA_INTER)
    };
    for (k, &dck) in dc.iter().enumerate().take(n) {
        let mut coef = [0f32; 64];
        if cbp & (1 << k) != 0 {
            read_ac(br, cbp & (1 << (16 + k)) != 0, q, scan, m, &mut coef)?;
        }
        coef[0] = dck * 0.125;
        idct_float(&mut coef);
        let (x, y) = block_origin(p, k, mbx, mby);
        if let Some(plane) = ctx.cur.get_mut(p) {
            write_float_block(plane, x, y, &coef, !intra);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vlc::tests::pack;

    #[test]
    fn luma_cbp_float() {
        // "11": return prev
        let d = pack(&["1", "1"]);
        assert_eq!(
            read_luma_cbp(&mut BitReader::new(&d), 0x1234_5678).unwrap(),
            0x1234_5678
        );
        // "0": nibbles: re-read nibble 0 = 0b0001, keep it for nibble 1, re-read 2 = 0, keep 3.
        // Upper: nibble 0 has one bit -> per-bit flags: 1 -> h = 1. nibble 1 same: flag 0 -> 0.
        // nibbles 2, 3: zero.
        let d = pack(&["0", "0", "1000", "1", "0", "0000", "1", "1", "0"]);
        assert_eq!(read_luma_cbp(&mut BitReader::new(&d), 0).unwrap(), 0x11 | (0x1 << 16));
        // "10": lo = prev low half 0x3; two bits set, bit 1 -> h keeps (prev >> 20) & 0xF & m
        let d = pack(&["1", "0", "1"]);
        assert_eq!(
            read_luma_cbp(&mut BitReader::new(&d), 0x0030_0003).unwrap(),
            0x3 | (0x3 << 16)
        );
    }

    #[test]
    fn chroma_cbp_float() {
        let d = pack(&["0", "0000"]);
        assert_eq!(read_chroma_cbp(&mut BitReader::new(&d), 0xF_000F).unwrap(), 0);
        // lo = 0b0101, inherit upper (bit 1): h = prev>>16 & lo
        let d = pack(&["0", "1010", "1"]);
        assert_eq!(read_chroma_cbp(&mut BitReader::new(&d), 0xF_0000).unwrap(), 0x5_0005);
        // lo = 0b0101, explicit: bit 0, then per set bit: 1, 0
        let d = pack(&["0", "1010", "0", "1", "0"]);
        assert_eq!(read_chroma_cbp(&mut BitReader::new(&d), 0).unwrap(), 0x1_0005);
    }

    #[test]
    fn dc_float() {
        // w = 2: values 1, 0, 3, 0; signs for the two non-zero: +, -
        let d = pack(&["010", "10", "00", "11", "00", "0", "1"]);
        let r = read_dc_residuals(&mut BitReader::new(&d), 4, 0, false).unwrap();
        assert_eq!(&r[..4], &[4.0, 0.0, -12.0, 0.0]);
        // corner: w = 0, q 0: e = 2 - 1 + 0 = 1 -> bits(9) = 5, << 0, sign + ; * 4
        let d = pack(&["000", "101000000", "0"]);
        let r = read_dc_residuals(&mut BitReader::new(&d), 4, 0, true).unwrap();
        assert_eq!(r[0], 20.0);
    }

    #[test]
    fn ac_float() {
        // F_LEVEL0 symbol 1 ("1"), sign 0 at pos 1; run symbol 12 ("11") -> pos 64 stops.
        let d = pack(&["1", "0", "11"]);
        let mut c = [0f32; 64];
        let mut r = BitReader::new(&d);
        read_ac(&mut r, false, 1, &SCAN_F_LUMA, &F_LUMA_INTRA, &mut c).unwrap();
        let i = SCAN_F_LUMA[1] as usize;
        assert_eq!(c[i], (1.0f32 * 2.0) * F_LUMA_INTRA[i]);
        assert_eq!(r.position(), 4);
    }

    #[test]
    fn mv_float_deltas_and_prediction() {
        // x: w=1, values 1,0,1,1, signs -,+,+ ; y: w=0. Both edges: offsets x: 1*16 +, y: 0
        let d = pack(&["100", "1", "0", "1", "1", "1", "0", "0", "10000", "0", "000", "00000"]);
        let dd = read_mv_deltas(&mut BitReader::new(&d), true).unwrap();
        assert_eq!(dd, [[15, 0], [16, 0], [17, 0], [17, 0]]);
        let z = [[0; 2]; 4];
        assert_eq!(predict_mv(&dd, true, true, &z, &z, &z), dd);
        // no edges: c2 uses median(T2, c0, c1)
        let l = [[1, 0], [5, 0], [0, 0], [9, 0]];
        let t = [[2, 0], [0, 0], [7, 0], [3, 0]];
        let tl = [[0, 0], [0, 0], [0, 0], [4, 0]];
        let d = [[1, 0], [2, 0], [3, 0], [4, 0]];
        let p = predict_mv(&d, false, false, &l, &t, &tl);
        // c0 = 1 + 5 = 6; c1 = 2 + med(7,3,6) = 8; c2 = 3 + med(7,6,8) = 10; c3 = 4 + 8
        assert_eq!([p[0][0], p[1][0], p[2][0], p[3][0]], [6, 8, 10, 12]);
    }
}
