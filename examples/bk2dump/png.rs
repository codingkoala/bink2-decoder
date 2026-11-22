//! Tiny dependency-free PNG writer (RGB8) with a fixed-Huffman LZ77 deflate encoder.
//! Used by the bk2dump example only.

use std::io::{self, Write};

fn crc32(data: &[&[u8]]) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *t = c;
    }
    let mut c = 0xFFFF_FFFFu32;
    for d in data {
        for &b in *d {
            c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
    }
    !c
}

struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, v: u32, len: u32) {
        self.acc |= (v as u64) << self.n;
        self.n += len;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }
    /// Huffman codes are sent most-significant bit first.
    fn put_rev(&mut self, code: u32, len: u32) {
        let mut r = 0;
        for i in 0..len {
            r |= ((code >> i) & 1) << (len - 1 - i);
        }
        self.put(r, len);
    }
    fn lit(&mut self, v: u32) {
        match v {
            0..=143 => self.put_rev(0x30 + v, 8),
            144..=255 => self.put_rev(0x190 + v - 144, 9),
            256..=279 => self.put_rev(v - 256, 7),
            _ => self.put_rev(0xC0 + v - 280, 8),
        }
    }
}

const LEN_BASE: [u32; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258,
];
const LEN_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u32; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13,
];

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut b = Bits {
        out: vec![0x78, 0x01],
        acc: 0,
        n: 0,
    };
    b.put(1, 1); // final block
    b.put(1, 2); // fixed Huffman
    const HBITS: u32 = 15;
    let mut head = vec![usize::MAX; 1 << HBITS];
    let hash = |i: usize| {
        let v = (data[i] as u32) | (data[i + 1] as u32) << 8 | (data[i + 2] as u32) << 16;
        (v.wrapping_mul(2_654_435_761) >> (32 - HBITS)) as usize
    };
    let mut i = 0;
    while i < data.len() {
        let mut best = (0usize, 0usize);
        if i + 3 <= data.len() {
            let h = hash(i);
            let cand = head[h];
            head[h] = i;
            if cand != usize::MAX && i - cand <= 32768 {
                let max = (data.len() - i).min(258);
                let mut l = 0;
                while l < max && data[cand + l] == data[i + l] {
                    l += 1;
                }
                if l >= 3 {
                    best = (l, i - cand);
                }
            }
        }
        if best.0 >= 3 {
            let (len, dist) = best;
            let li = LEN_BASE.iter().rposition(|&x| x as usize <= len).unwrap();
            b.lit(257 + li as u32);
            b.put(len as u32 - LEN_BASE[li], LEN_EXTRA[li]);
            let di = DIST_BASE.iter().rposition(|&x| x as usize <= dist).unwrap();
            b.put_rev(di as u32, 5);
            b.put(dist as u32 - DIST_BASE[di], DIST_EXTRA[di]);
            for k in i + 1..(i + len).min(data.len().saturating_sub(2)) {
                let h = hash(k);
                head[h] = k;
            }
            i += len;
        } else {
            b.lit(data[i] as u32);
            i += 1;
        }
    }
    b.lit(256);
    b.put(0, 7);
    let mut out = b.out;
    let (mut s1, mut s2) = (1u32, 0u32);
    for &x in data {
        s1 = (s1 + x as u32) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    out.extend_from_slice(&((s2 << 16) | s1).to_be_bytes());
    out
}

/// Writes an RGB8 image (`rgb.len() == 3 * w * h`) as PNG, with the "Sub" filter on every row.
pub fn write_png<W: Write>(mut w: W, width: usize, height: usize, rgb: &[u8]) -> io::Result<()> {
    let mut raw = Vec::with_capacity((3 * width + 1) * height);
    for row in rgb.chunks_exact(3 * width) {
        raw.push(1);
        for (i, &v) in row.iter().enumerate() {
            raw.push(if i >= 3 { v.wrapping_sub(row[i - 3]) } else { v });
        }
    }
    let chunk = |w: &mut W, ty: &[u8], data: &[u8]| -> io::Result<()> {
        w.write_all(&(data.len() as u32).to_be_bytes())?;
        w.write_all(ty)?;
        w.write_all(data)?;
        w.write_all(&crc32(&[ty, data]).to_be_bytes())
    };
    w.write_all(b"\x89PNG\r\n\x1a\n")?;
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut w, b"IHDR", &ihdr)?;
    chunk(&mut w, b"IDAT", &deflate(&raw))?;
    chunk(&mut w, b"IEND", &[])
}
