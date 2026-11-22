//! Inferred deblocking filter (not in SPEC.md; see docs/INFERRED.md). Black-box fit against
//! RAD's own output: an edge at a multiple of 8 pixels is smoothed unless its edge flag
//! (SPEC §3.3) is set.

use crate::frame::Plane;

/// Smooths the two pixels next to every 8-pixel edge of `p` that is enabled:
/// `p0' = (3·p0 + q0 + 2) >> 2`, `q0' = (3·q0 + p0 + 2) >> 2`.
///
/// `col_off(k)` / `row_off(k)` return true if the vertical edge at x = 8k / the horizontal edge
/// at y = 8k is switched off. `step` is the edge spacing in this plane (8).
pub(crate) fn filter_plane(
    p: &mut Plane,
    vertical: bool,
    horizontal: bool,
    col_off: &dyn Fn(usize) -> bool,
    row_off: &dyn Fn(usize) -> bool,
    max_step: i32,
    pix_on: &dyn Fn(usize, usize) -> bool,
) {
    let (w, h) = (p.w, p.h);
    if vertical {
        for k in 1..w / 8 {
            if col_off(k) {
                continue;
            }
            let x = 8 * k;
            for y in 0..h {
                if !pix_on(x, y) {
                    continue;
                }
                let i = p.idx(x as i64, y as i64);
                let (a, b) = (p.data[i - 1] as i32, p.data[i] as i32);
                if (b - a).abs() >= max_step {
                    continue;
                }
                p.data[i - 1] = ((3 * a + b + 2) >> 2) as u8;
                p.data[i] = ((3 * b + a + 2) >> 2) as u8;
            }
        }
    }
    if horizontal {
        for k in 1..h / 8 {
            if row_off(k) {
                continue;
            }
            let y = 8 * k;
            for x in 0..w {
                if !pix_on(x, y) {
                    continue;
                }
                let i = p.idx(x as i64, y as i64);
                let j = i - p.stride;
                let (a, b) = (p.data[j] as i32, p.data[i] as i32);
                if (b - a).abs() >= max_step {
                    continue;
                }
                p.data[j] = ((3 * a + b + 2) >> 2) as u8;
                p.data[i] = ((3 * b + a + 2) >> 2) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smooths_enabled_edges_only() {
        let mut p = Plane::new(16, 16, 0);
        for y in 0..16 {
            for x in 8..16 {
                let i = p.idx(x, y);
                p.data[i] = 100;
            }
        }
        let mut q = p.clone();
        filter_plane(&mut p, true, true, &|_| false, &|_| false, 256, &|_, _| true);
        assert_eq!(p.data[p.idx(7, 3)], 25);
        assert_eq!(p.data[p.idx(8, 3)], 75);
        assert_eq!(p.data[p.idx(6, 3)], 0);
        filter_plane(&mut q, true, true, &|k| k == 1, &|_| false, 256, &|_, _| true);
        assert_eq!(q.data[q.idx(7, 3)], 0);
    }
}
