//! Frame buffers with an edge-replicated margin, and the public output frame.

/// Margin around every reference plane, in pixels.
// SPEC §4.7 draft 7 (unchanged in draft 8): samples outside the coded (32-aligned) frame replicate its edge pixels.
pub const MARGIN: usize = 32;

/// One 8-bit plane covering the coded (aligned) area plus `MARGIN` on every side.
#[derive(Debug, Clone)]
pub(crate) struct Plane {
    /// Coded width (multiple of 16 for chroma, 32 for luma/alpha).
    pub w: usize,
    /// Coded height.
    pub h: usize,
    pub stride: usize,
    pub data: Vec<u8>,
}

impl Plane {
    pub fn new(w: usize, h: usize, fill: u8) -> Plane {
        let stride = w + 2 * MARGIN;
        Plane {
            w,
            h,
            stride,
            data: vec![fill; stride * (h + 2 * MARGIN)],
        }
    }

    /// True if the rectangle `[x, x+w) × [y, y+h)` (coded-area coordinates) lies inside the
    /// padded buffer.
    pub fn contains(&self, x: i64, y: i64, w: usize, h: usize) -> bool {
        let m = MARGIN as i64;
        x >= -m && y >= -m && x + w as i64 <= self.w as i64 + m && y + h as i64 <= self.h as i64 + m
    }

    /// Buffer index of coded-area position (x, y). The caller must have checked `contains`.
    #[inline]
    pub fn idx(&self, x: i64, y: i64) -> usize {
        ((y + MARGIN as i64) as usize) * self.stride + (x + MARGIN as i64) as usize
    }

    /// Fills the margin by replicating the edge pixels of the coded area.
    pub fn extend_edges(&mut self) {
        let (w, h, s) = (self.w, self.h, self.stride);
        for y in 0..h {
            let row = (y + MARGIN) * s;
            let left = self.data[row + MARGIN];
            let right = self.data[row + MARGIN + w - 1];
            self.data[row..row + MARGIN].fill(left);
            self.data[row + MARGIN + w..row + s].fill(right);
        }
        let first = MARGIN * s;
        let last = (MARGIN + h - 1) * s;
        for y in 0..MARGIN {
            self.data.copy_within(first..first + s, y * s);
            self.data.copy_within(last..last + s, (MARGIN + h + y) * s);
        }
    }

    /// Splits the coded rows into disjoint mutable bands `[0, ends[0])`, `[ends[0], ends[1])`,
    /// … (`ends` increasing, at most `h`). The top and bottom margins are not part of any band.
    /// Used to decode the slices of a frame, and to filter parts of a plane, in parallel.
    pub fn bands(&mut self, ends: &[usize]) -> Vec<PlaneBand<'_>> {
        let s = self.stride;
        let mut rest = &mut self.data[MARGIN * s..];
        let mut y0 = 0;
        let mut out = Vec::with_capacity(ends.len());
        for &y1 in ends {
            assert!(y0 <= y1 && y1 <= self.h, "bad band end {y1}");
            let (band, tail) = std::mem::take(&mut rest).split_at_mut((y1 - y0) * s);
            out.push(PlaneBand {
                stride: s,
                y0,
                y1,
                data: band,
            });
            rest = tail;
            y0 = y1;
        }
        out
    }

    /// The whole coded area as one band.
    #[cfg(test)]
    pub fn band(&mut self) -> PlaneBand<'_> {
        let h = self.h;
        self.bands(&[h]).pop().expect("one band")
    }

    /// Copies the top-left `w × h` pixels into a tightly packed vector.
    pub fn crop(&self, w: usize, h: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * h);
        for y in 0..h.min(self.h) {
            let i = self.idx(0, y as i64);
            out.extend_from_slice(&self.data[i..i + w.min(self.w)]);
        }
        out
    }
}

/// Mutable view of the coded rows `[y0, y1)` of a [`Plane`], full stride (the left and right
/// margin columns of those rows included, so offsets within a row match the plane's layout).
/// Positions are plane coordinates; only rows inside the band may be accessed.
#[derive(Debug)]
pub(crate) struct PlaneBand<'a> {
    pub stride: usize,
    /// First coded row of the band.
    pub y0: usize,
    /// One past the last coded row.
    pub y1: usize,
    pub data: &'a mut [u8],
}

impl PlaneBand<'_> {
    /// Index into `data` of plane position (x, y). `y` must lie in `[y0, y1)` and `x` in the
    /// coded width or the margin.
    #[inline]
    pub fn idx(&self, x: i64, y: i64) -> usize {
        debug_assert!(
            y >= self.y0 as i64 && y < self.y1 as i64,
            "row {y} outside band {}..{}",
            self.y0,
            self.y1
        );
        (y as usize - self.y0) * self.stride + (x + MARGIN as i64) as usize
    }
}

/// The planes of one decoded frame in coding order: Y, C1, C2 and optionally A.
#[derive(Debug, Clone)]
pub(crate) struct FrameBuf {
    pub planes: Vec<Plane>,
}

impl FrameBuf {
    pub fn new(aligned_w: usize, aligned_h: usize, alpha: bool) -> FrameBuf {
        let mut planes = vec![
            Plane::new(aligned_w, aligned_h, 0),
            Plane::new(aligned_w / 2, aligned_h / 2, 128),
            Plane::new(aligned_w / 2, aligned_h / 2, 128),
        ];
        if alpha {
            planes.push(Plane::new(aligned_w, aligned_h, 255));
        }
        FrameBuf { planes }
    }

    pub fn extend_edges(&mut self) {
        for p in &mut self.planes {
            p.extend_edges();
        }
    }
}

/// A decoded picture: 8-bit planar YUV 4:2:0 cropped to the picture size, plus optional alpha.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    /// Picture width.
    pub width: usize,
    /// Picture height.
    pub height: usize,
    /// Luma, `width × height`.
    pub y: Vec<u8>,
    /// Cb, `chroma_width() × chroma_height()`.
    pub u: Vec<u8>,
    /// Cr, `chroma_width() × chroma_height()`.
    pub v: Vec<u8>,
    /// Alpha, `width × height`, if the video has an alpha plane.
    pub alpha: Option<Vec<u8>>,
    /// Keyframe flag the frame was decoded with.
    pub keyframe: bool,
}

impl VideoFrame {
    /// Chroma plane width, `ceil(width / 2)`.
    pub fn chroma_width(&self) -> usize {
        self.width.div_ceil(2)
    }
    /// Chroma plane height, `ceil(height / 2)`.
    pub fn chroma_height(&self) -> usize {
        self.height.div_ceil(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_replication() {
        let mut p = Plane::new(32, 32, 0);
        for y in 0..32 {
            for x in 0..32 {
                let i = p.idx(x, y);
                p.data[i] = (x + 4 * y) as u8;
            }
        }
        p.extend_edges();
        assert_eq!(p.data[p.idx(-32, -32)], 0);
        assert_eq!(p.data[p.idx(63, -5)], 31);
        assert_eq!(p.data[p.idx(-1, 10)], 40);
        assert_eq!(p.data[p.idx(40, 63)], 31 + 124);
        assert!(p.contains(-32, -32, 96, 96));
        assert!(!p.contains(-33, 0, 1, 1));
        assert!(!p.contains(0, 0, 65, 1));
    }
}
