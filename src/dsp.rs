//! Arithmetic helpers, inverse transforms, motion compensation filters and block copies.

use crate::frame::Plane;

/// `clip8(v)` = `min(max(v, 0), 255)`.
#[inline]
pub fn clip8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// The middle value of three.
#[inline]
pub fn median<T: PartialOrd + Copy>(a: T, b: T, c: T) -> T {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    if c < lo {
        lo
    } else if c > hi {
        hi
    } else {
        c
    }
}

/// `clamp(v, lo, hi)` = `min(max(v, lo), hi)`, written so it never panics (unlike `Ord::clamp`
/// when `lo > hi`).
#[inline]
pub fn clamp<T: PartialOrd + Copy>(v: T, lo: T, hi: T) -> T {
    let v = if v < lo { lo } else { v };
    if v > hi {
        hi
    } else {
        v
    }
}

/// `ceil_to(v, n)`.
#[inline]
pub fn ceil_to(v: u32, n: u32) -> u32 {
    v.div_ceil(n) * n
}

// ---------------------------------------------------------------- integer IDCT (SPEC §4.9)

#[inline]
fn m_a(v: i32) -> i32 {
    v + (v >> 2)
}
#[inline]
fn m_b(v: i32) -> i32 {
    v >> 1
}
#[inline]
fn m_c(v: i32) -> i32 {
    v - (v >> 2) - (v >> 4)
}
#[inline]
fn m_d(v: i32) -> i32 {
    v + (v >> 2) - (v >> 4)
}
#[inline]
fn m_e(v: i32) -> i32 {
    v >> 2
}

fn idct_int_1d(a: &mut [i16; 64], start: usize, step: usize, sh: u32) {
    let x = |k: usize| a[start + k * step] as i32;
    let (x0, x1, x2, x3, x4, x5, x6, x7) = (x(0), x(1), x(2), x(3), x(4), x(5), x(6), x(7));
    let e0 = x0.wrapping_add(x4);
    let e1 = x0.wrapping_sub(x4);
    let p = m_a(x2) + m_b(x6);
    let q = m_b(x2) - m_a(x6);
    let (ee0, ee3, ee1, ee2) = (e0 + p, e0 - p, e1 + q, e1 - q);
    let g = x3 + x5;
    let h = x3 - x5;
    let u = x1 + g;
    let w = x1 - g;
    let r = h + x7;
    let t = h - x7;
    let o0 = m_d(u) + m_e(r);
    let o3 = m_e(u) - m_d(r);
    let o1 = w + m_c(t);
    let o2 = m_c(w) - t;
    let y = [
        ee0 + o0,
        ee1 + o1,
        ee2 + o2,
        ee3 + o3,
        ee3 - o3,
        ee2 - o2,
        ee1 - o1,
        ee0 - o0,
    ];
    for (k, v) in y.iter().enumerate() {
        a[start + k * step] = (v >> sh) as i16;
    }
}

/// Integer inverse transform in place. Afterwards image pixel (row i, column j) is `a[8j + i]`.
pub fn idct_int(a: &mut [i16; 64]) {
    for c in 0..8 {
        idct_int_1d(a, c, 8, 0);
    }
    for r in 0..8 {
        idct_int_1d(a, 8 * r, 1, 6);
    }
}

/// Writes an integer-IDCT output block (transposed layout) at (x, y), either replacing (`add`
/// false) or adding to the pixels. The caller guarantees the block lies in the coded area.
pub fn write_int_block(p: &mut Plane, x: usize, y: usize, a: &[i16; 64], add: bool) {
    for i in 0..8 {
        let base = p.idx(x as i64, (y + i) as i64);
        for j in 0..8 {
            let v = a[8 * j + i] as i32;
            let d = &mut p.data[base + j];
            *d = if add { clip8(*d as i32 + v) } else { clip8(v) };
        }
    }
}

// ---------------------------------------------------------------- float IDCT (SPEC §5.5)

// The constants are kept exactly as written in SPEC §5.5.
#[allow(clippy::approx_constant)]
fn idct_float_1d(a: &mut [f32; 64], start: usize, step: usize) {
    let x = |k: usize| a[start + k * step];
    let (x0, x1, x2, x3, x4, x5, x6, x7) = (x(0), x(1), x(2), x(3), x(4), x(5), x(6), x(7));
    let aa = x2 + x6;
    let b = (x2 - x6) * 1.4142135f32 - aa;
    let c = x0 + x4;
    let d = x0 - x4;
    let (e0, e3, e1, e2) = (c + aa, c - aa, d + b, d - b);
    let g = x3 + x5;
    let h = x3 - x5;
    let k = x1 + x7;
    let m = x1 - x7;
    let s = k + g;
    let z1 = (k - g) * 1.4142135f32;
    let z2 = (m - h) * 1.847759f32;
    let o1 = h * 2.613126f32 + z2 - s;
    let o2 = z1 - o1;
    let o3 = m * 1.0823922f32 - z2 + o2;
    let y = [e0 + s, e1 + o1, e2 + o2, e3 - o3, e3 + o3, e2 - o2, e1 - o1, e0 - s];
    for (k, v) in y.iter().enumerate() {
        a[start + k * step] = *v;
    }
}

/// Float inverse transform in place, including the +512.5 bias on `a[0]`. Afterwards image
/// pixel (row r, column c) is `a[8r + c]` (not transposed); the values still carry the 512 bias.
pub fn idct_float(a: &mut [f32; 64]) {
    a[0] += 512.5;
    for c in 0..8 {
        idct_float_1d(a, c, 8);
    }
    for r in 0..8 {
        idct_float_1d(a, 8 * r, 1);
    }
}

/// Writes a float-IDCT output block at (x, y): put `clip8(trunc(a − 512))` or add
/// `clip8(trunc(pixel + a − 512))`.
pub fn write_float_block(p: &mut Plane, x: usize, y: usize, a: &[f32; 64], add: bool) {
    for r in 0..8 {
        let base = p.idx(x as i64, (y + r) as i64);
        for c in 0..8 {
            let v = a[8 * r + c];
            let d = &mut p.data[base + c];
            let t = if add {
                (*d as f32 + v - 512.0) as i32
            } else {
                (v - 512.0) as i32
            };
            *d = clip8(t);
        }
    }
}

// ---------------------------------------------------------------- MC filters (SPEC §4.7)

/// Luma 6-tap half-pel filter on `s[-2] … s[3]` given as `s[0] … s[5]`.
#[inline]
pub fn luma_tap(s: [i32; 6]) -> u8 {
    let v = ((19 * (s[2] + s[3])) >> 1) - 2 * (s[1] + s[4]) + ((s[0] + s[5]) >> 1) + 8;
    clip8(v >> 4)
}

/// Chroma filter between `a` and `b` at quarter-pel fraction `f` (0 returns `a`).
#[inline]
pub fn chroma_tap(a: i32, b: i32, f: i32) -> u8 {
    match f {
        1 => ((6 * a + 2 * b + 4) >> 3) as u8,
        2 => ((a + b + 1) >> 1) as u8,
        3 => ((2 * a + 6 * b + 4) >> 3) as u8,
        _ => a as u8,
    }
}

/// Copies a `size × size` block from `src` at (sx, sy) to `dst` at (dx, dy). Returns false (and
/// writes nothing) if the source rectangle is outside the padded reference.
pub fn copy_block(dst: &mut Plane, dx: usize, dy: usize, src: &Plane, sx: i64, sy: i64, size: usize) -> bool {
    if !src.contains(sx, sy, size, size) {
        return false;
    }
    for r in 0..size {
        let si = src.idx(sx, sy + r as i64);
        let di = dst.idx(dx as i64, (dy + r) as i64);
        dst.data[di..di + size].copy_from_slice(&src.data[si..si + size]);
    }
    true
}

/// Luma (and alpha) 16×16 half-pel prediction from integer source (sx, sy) with fraction
/// (fx, fy) ∈ {0,1}². Returns false (nothing written) if the reads leave the padded reference.
#[allow(clippy::too_many_arguments)]
pub fn mc_luma(dst: &mut Plane, dx: usize, dy: usize, src: &Plane, sx: i64, sy: i64, fx: i32, fy: i32) -> bool {
    const N: usize = 16;
    if !src.contains(sx - 2, sy - 2, N + 5, N + 5) {
        return false;
    }
    let px = |x: i64, y: i64| src.data[src.idx(x, y)] as i32;
    let hf = |x: i64, y: i64| {
        luma_tap([
            px(x - 2, y),
            px(x - 1, y),
            px(x, y),
            px(x + 1, y),
            px(x + 2, y),
            px(x + 3, y),
        ])
    };
    match (fx, fy) {
        (0, 0) => return copy_block(dst, dx, dy, src, sx, sy, N),
        (1, 0) => {
            for r in 0..N {
                let di = dst.idx(dx as i64, (dy + r) as i64);
                for c in 0..N {
                    dst.data[di + c] = hf(sx + c as i64, sy + r as i64);
                }
            }
        }
        (0, 1) => {
            for r in 0..N {
                let di = dst.idx(dx as i64, (dy + r) as i64);
                for c in 0..N {
                    let (x, y) = (sx + c as i64, sy + r as i64);
                    dst.data[di + c] = luma_tap([
                        px(x, y - 2),
                        px(x, y - 1),
                        px(x, y),
                        px(x, y + 1),
                        px(x, y + 2),
                        px(x, y + 3),
                    ]);
                }
            }
        }
        _ => {
            // 21 intermediate rows, source rows -2 … +18.
            let mut tmp = [[0i32; N]; N + 5];
            for (r, row) in tmp.iter_mut().enumerate() {
                for (c, v) in row.iter_mut().enumerate() {
                    *v = hf(sx + c as i64, sy + r as i64 - 2) as i32;
                }
            }
            for r in 0..N {
                let di = dst.idx(dx as i64, (dy + r) as i64);
                let rows = &tmp[r..r + 6];
                for (c, d) in dst.data[di..di + N].iter_mut().enumerate() {
                    *d = luma_tap(std::array::from_fn(|k| rows[k][c]));
                }
            }
        }
    }
    true
}

/// Chroma 8×8 quarter-pel prediction from integer source (sx, sy) with fraction (fx, fy) ∈ 0..4.
/// Returns false (nothing written) if the reads leave the padded reference.
#[allow(clippy::too_many_arguments)]
pub fn mc_chroma(dst: &mut Plane, dx: usize, dy: usize, src: &Plane, sx: i64, sy: i64, fx: i32, fy: i32) -> bool {
    const N: usize = 8;
    if !src.contains(sx, sy, N + 1, N + 1) {
        return false;
    }
    let px = |x: i64, y: i64| src.data[src.idx(x, y)] as i32;
    if fx == 0 && fy == 0 {
        return copy_block(dst, dx, dy, src, sx, sy, N);
    }
    let mut out = [[0u8; N]; N];
    if fy == 0 {
        for (r, row) in out.iter_mut().enumerate() {
            for (c, v) in row.iter_mut().enumerate() {
                let (x, y) = (sx + c as i64, sy + r as i64);
                *v = chroma_tap(px(x, y), px(x + 1, y), fx);
            }
        }
    } else if fx == 0 {
        for (r, row) in out.iter_mut().enumerate() {
            for (c, v) in row.iter_mut().enumerate() {
                let (x, y) = (sx + c as i64, sy + r as i64);
                *v = chroma_tap(px(x, y), px(x, y + 1), fy);
            }
        }
    } else {
        let mut tmp = [[0i32; N]; N + 1];
        for (r, row) in tmp.iter_mut().enumerate() {
            for (c, v) in row.iter_mut().enumerate() {
                let (x, y) = (sx + c as i64, sy + r as i64);
                *v = chroma_tap(px(x, y), px(x + 1, y), fx) as i32;
            }
        }
        for (r, row) in out.iter_mut().enumerate() {
            for (c, v) in row.iter_mut().enumerate() {
                *v = chroma_tap(tmp[r][c], tmp[r + 1][c], fy);
            }
        }
    }
    for (r, row) in out.iter().enumerate() {
        let di = dst.idx(dx as i64, (dy + r) as i64);
        dst.data[di..di + N].copy_from_slice(row);
    }
    true
}

/// DC of an 8×8 block recomputed from pixels (SPEC §4.8 neighbour refresh).
pub fn block_dc_from_pixels(p: &Plane, x: usize, y: usize) -> i32 {
    let avg = |a: i32, b: i32| (a + b + 1) >> 1;
    let mut sum = 0;
    for c in 0..8 {
        let v: [i32; 8] = std::array::from_fn(|r| p.data[p.idx((x + c) as i64, (y + r) as i64)] as i32);
        sum += avg(
            avg(avg(v[0], v[1]), avg(v[2], v[3])),
            avg(avg(v[4], v[5]), avg(v[6], v[7])),
        );
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_clamp() {
        assert_eq!(median(1, 2, 3), 2);
        assert_eq!(median(3, 1, 2), 2);
        assert_eq!(median(2, 3, 1), 2);
        assert_eq!(median(5, 5, -1), 5);
        assert_eq!(median(-4, 7, 7), 7);
        assert_eq!(median(0.5f32, -1.0, 0.25), 0.25);
        assert_eq!(clamp(5, 0, 3), 3);
        assert_eq!(clamp(-5, 0, 3), 0);
        assert_eq!(clamp(2, 0, 3), 2);
        assert_eq!(clamp(1, 4, 2), 2); // lo > hi does not panic
        assert_eq!(clip8(-3), 0);
        assert_eq!(clip8(300), 255);
        assert_eq!(ceil_to(33, 32), 64);
        assert_eq!(ceil_to(32, 32), 32);
    }

    #[test]
    fn int_idct_dc_only_is_flat() {
        for dc in [0, 1, 1024, 2047] {
            let mut a = [0i16; 64];
            a[0] = (dc * 8 + 32) as i16;
            idct_int(&mut a);
            let want = ((dc * 8 + 32) >> 6) as i16;
            assert!(a.iter().all(|&v| v == want), "dc {dc}");
        }
        let mut a = [0i16; 64];
        a[0] = 1024 * 8 + 32;
        idct_int(&mut a);
        assert_eq!(a[0], 128);
    }

    #[test]
    fn int_idct_hand_computed() {
        // a[8] = 64 (array row 1, column 0). Column pass on column 0 gives
        // [76, 64, 44, 16, -16, -44, -64, -76]; the row pass then spreads each value along its
        // row with >> 6: [1, 1, 0, 0, -1, -1, -1, -2]. Image pixel (i, j) = a[8j + i], so
        // the image varies along columns.
        let mut a = [0i16; 64];
        a[8] = 64;
        idct_int(&mut a);
        let want = [1, 1, 0, 0, -1, -1, -1, -2];
        for j in 0..8 {
            for i in 0..8 {
                assert_eq!(a[8 * j + i], want[j], "row {j} col {i}");
            }
        }
        let mut p = Plane::new(32, 32, 128);
        write_int_block(&mut p, 8, 0, &a, true);
        let row0: Vec<u8> = (0..8).map(|c| p.data[p.idx(8 + c, 0)]).collect();
        assert_eq!(row0, [129, 129, 128, 128, 127, 127, 127, 126]);
        let row5: Vec<u8> = (0..8).map(|c| p.data[p.idx(8 + c, 5)]).collect();
        assert_eq!(row0, row5);
        write_int_block(&mut p, 8, 0, &a, false);
        assert_eq!(p.data[p.idx(8, 0)], 1);
        assert_eq!(p.data[p.idx(15, 0)], 0);
    }

    #[test]
    fn float_idct_dc_only_is_flat() {
        for dc in [0.0f32, 1024.0, 1027.0, 2047.0] {
            let mut a = [0f32; 64];
            a[0] = dc * 0.125;
            idct_float(&mut a);
            let mut p = Plane::new(16, 16, 0);
            write_float_block(&mut p, 0, 0, &a, false);
            let want = clip8((dc * 0.125 + 0.5) as i32);
            for r in 0..8 {
                for c in 0..8 {
                    assert_eq!(p.data[p.idx(c, r)], want, "dc {dc}");
                }
            }
        }
        // inter: DC 0 adds nothing
        let mut a = [0f32; 64];
        idct_float(&mut a);
        let mut p = Plane::new(16, 16, 77);
        write_float_block(&mut p, 0, 0, &a, true);
        assert_eq!(p.data[p.idx(3, 3)], 77);
    }

    #[test]
    fn luma_filter_known_inputs() {
        assert_eq!(luma_tap([100; 6]), 100);
        // step between s0 and s1: 0,0,0,160,160,160 -> (19*160/2 - 2*160 + 160/2 + 8) >> 4
        assert_eq!(luma_tap([0, 0, 0, 160, 160, 160]), ((1520 - 320 + 80 + 8) >> 4) as u8);
        assert_eq!(luma_tap([0, 0, 0, 160, 160, 160]), 80);
        assert_eq!(luma_tap([255, 0, 255, 255, 0, 255]), 255); // clipped high
        assert_eq!(luma_tap([0, 255, 0, 0, 255, 0]), 0); // clipped low
        assert_eq!(luma_tap([10, 20, 30, 40, 50, 60]), ((665 - 140 + 35 + 8) >> 4) as u8);
    }

    #[test]
    fn chroma_filter_known_inputs() {
        assert_eq!(chroma_tap(0, 80, 1), 20);
        assert_eq!(chroma_tap(0, 80, 2), 40);
        assert_eq!(chroma_tap(0, 80, 3), 60);
        assert_eq!(chroma_tap(1, 2, 2), 2);
        assert_eq!(chroma_tap(9, 200, 0), 9);
    }

    fn ramp_plane() -> Plane {
        let mut p = Plane::new(32, 32, 0);
        for y in 0..32 {
            for x in 0..32 {
                let i = p.idx(x, y);
                p.data[i] = (4 * x + 2 * y) as u8;
            }
        }
        p.extend_edges();
        p
    }

    #[test]
    fn mc_luma_on_ramp() {
        // On a linear ramp the 6-tap filter gives the exact midpoint (+ rounding).
        let src = ramp_plane();
        let mut dst = Plane::new(32, 32, 0);
        assert!(mc_luma(&mut dst, 0, 0, &src, 4, 4, 0, 0));
        assert_eq!(dst.data[dst.idx(0, 0)], 24);
        assert!(mc_luma(&mut dst, 0, 0, &src, 4, 4, 1, 0));
        assert_eq!(dst.data[dst.idx(0, 0)], 26);
        assert!(mc_luma(&mut dst, 0, 0, &src, 4, 4, 0, 1));
        assert_eq!(dst.data[dst.idx(0, 0)], 25);
        assert!(mc_luma(&mut dst, 0, 0, &src, 4, 4, 1, 1));
        assert_eq!(dst.data[dst.idx(0, 0)], 27);
        assert_eq!(dst.data[dst.idx(1, 2)], 27 + 4 + 4);
        // reads beyond the margin are refused
        assert!(!mc_luma(&mut dst, 0, 0, &src, 60, 0, 1, 0));
    }

    #[test]
    fn mc_chroma_on_ramp() {
        let src = ramp_plane();
        let mut dst = Plane::new(16, 16, 0);
        assert!(mc_chroma(&mut dst, 0, 0, &src, 2, 2, 1, 0));
        assert_eq!(dst.data[dst.idx(0, 0)], 13); // (6*12 + 2*16 + 4) >> 3
        assert!(mc_chroma(&mut dst, 0, 0, &src, 2, 2, 0, 2));
        assert_eq!(dst.data[dst.idx(0, 0)], 13); // (12 + 14 + 1) >> 1
        assert!(mc_chroma(&mut dst, 0, 0, &src, 2, 2, 2, 2));
        assert_eq!(dst.data[dst.idx(0, 0)], 15); // h: 14 and 16, then (14+16+1)>>1
    }

    #[test]
    fn dc_from_pixels() {
        let mut p = Plane::new(16, 16, 100);
        assert_eq!(block_dc_from_pixels(&p, 0, 0), 800);
        let i = p.idx(0, 0);
        p.data[i] = 101; // avg(101,100)=101, avg(101,100)=101, avg(101,100)=101
        assert_eq!(block_dc_from_pixels(&p, 0, 0), 801);
    }
}
