//! Frame buffers with an edge-replicated margin, and the public output frame.

/// Margin around every reference plane, in pixels.
// SPEC-QUESTION: SPEC §4.7 leaves reference padding undefined. We keep a 32-pixel margin on
// every side, filled by replicating the edge pixels of the coded (32-aligned) frame.
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
