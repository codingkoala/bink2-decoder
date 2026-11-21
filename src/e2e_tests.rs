//! Synthetic end-to-end tests: video packets built bit by bit from the spec.

use crate::container::tests::build_file;
use crate::container::Container;
use crate::{Decoder, Error};

/// LSB-first bit writer.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    n: usize,
}

impl BitWriter {
    /// Appends bits given in read order, e.g. "0110".
    fn put(&mut self, s: &str) -> &mut Self {
        for ch in s.bytes() {
            if self.n.is_multiple_of(8) {
                self.bytes.push(0);
            }
            if ch == b'1' {
                *self.bytes.last_mut().unwrap() |= 1 << (self.n % 8);
            }
            self.n += 1;
        }
        self
    }
    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

/// Builds a packet: frame_flags, slice_end table, then each slice's bytes.
fn packet(frame_flags: u32, slices: &[Vec<u8>]) -> Vec<u8> {
    let s = slices.len();
    let mut out = frame_flags.to_le_bytes().to_vec();
    let mut off = 4 * s;
    for sl in &slices[..s - 1] {
        off += sl.len();
        out.extend_from_slice(&(off as u32).to_le_bytes());
    }
    for sl in slices {
        out.extend_from_slice(sl);
    }
    out
}

/// One integer-variant INTRA MB with zero dq and no residuals (keyframe: no type bits).
fn g_flat_intra_mb(w: &mut BitWriter, alpha: bool) {
    w.put("1"); // read_dq: zeros(4) = 0
    w.put("1").put("0").put("0"); // Y: cbp first bit 1 (cbp 0), run-table bit 0, DC flag 0
    w.put("1").put("0"); // C1: cbp inherited, DC flag 0
    w.put("1").put("0"); // C2
    if alpha {
        w.put("1").put("0").put("0");
    }
}

fn assert_flat(v: &[u8], val: u8, what: &str) {
    assert!(
        v.iter().all(|&p| p == val),
        "{what} not flat {val}: {:?}",
        &v[..v.len().min(16)]
    );
}

#[test]
fn g_keyframe_flat_grey_two_slices() {
    // revision h, flags 0 -> 2 slices; 64x64 -> boundaries [32, 64], one MB row each.
    let mut slices = Vec::new();
    for _ in 0..2 {
        let mut w = BitWriter::default();
        g_flat_intra_mb(&mut w, false);
        g_flat_intra_mb(&mut w, false);
        slices.push(w.finish());
    }
    let pkt = packet(0, &slices);
    let mut dec = Decoder::with_params(b'h', 64, 64, 0).unwrap();
    let f = dec.decode(&pkt, true).unwrap();
    assert_eq!((f.width, f.height), (64, 64));
    assert_eq!(f.y.len(), 64 * 64);
    assert_eq!(f.u.len(), 32 * 32);
    assert_flat(&f.y, 128, "Y");
    assert_flat(&f.u, 128, "U");
    assert_flat(&f.v, 128, "V");
    assert!(f.alpha.is_none());
}

#[test]
fn g_keyframe_through_container_with_alpha_and_crop() {
    // revision g, 40x20 (H32 = 32 < 128 -> one slice), alpha; 2 MBs.
    let mut w = BitWriter::default();
    g_flat_intra_mb(&mut w, true);
    g_flat_intra_mb(&mut w, true);
    let pkt = packet(0, &[w.finish()]);
    let mut body = Vec::new();
    body.extend_from_slice(&3u32.to_le_bytes()); // audio track: 3 bytes
    body.extend_from_slice(&[1, 2, 3]);
    body.extend_from_slice(&pkt);
    let file = build_file(b'g', 40, 20, 0x0010_0000, &[(48000, 0x2000, 0)], &[(true, body)]);
    let c = Container::parse(&file).unwrap();
    let fp = c.frame(0).unwrap();
    assert_eq!(fp.audio, vec![&[1u8, 2, 3][..]]);
    let mut dec = Decoder::new(&c.header).unwrap();
    let f = dec.decode(fp.video, fp.keyframe).unwrap();
    assert_eq!(
        (f.width, f.height, f.chroma_width(), f.chroma_height()),
        (40, 20, 20, 10)
    );
    assert_eq!(f.y.len(), 800);
    assert_flat(&f.y, 128, "Y");
    assert_flat(&f.u, 128, "U");
    assert_flat(f.alpha.as_ref().unwrap(), 128, "A");
}

/// Single-MB keyframe (32x32, revision g) whose luma block 0 has DC residual c = 8.
fn g_keyframe_dc() -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put("1"); // dq 0 -> q 16
    w.put("1").put("0"); // Y cbp 0, run bit
                         // DC: flag 1; block 0: ones = 5 -> 4 + bits(2) + 2 with bits(2) = 2 -> c = 8, sign +
    w.put("1").put("111110").put("01").put("0");
    for _ in 1..16 {
        w.put("0");
    }
    w.put("1").put("0").put("1").put("0");
    packet(0, &[w.finish()])
}

#[test]
fn g_keyframe_dc_residual_and_prediction() {
    // r = (8 * DC_SCALE[16] + 512) >> 10 = 128 -> dc 1152 for block 0, and every other block
    // predicts 1152 -> pixel (1152 * 8 + 32) >> 6 = 144.
    let mut dec = Decoder::with_params(b'g', 32, 32, 0).unwrap();
    let f = dec.decode(&g_keyframe_dc(), true).unwrap();
    assert_flat(&f.y, 144, "Y");
    assert_flat(&f.u, 128, "U");
}

#[test]
fn g_p_frames_skip_motion_residue() {
    let mut dec = Decoder::with_params(b'g', 32, 32, 0).unwrap();
    // P frame before any keyframe
    assert_eq!(
        dec.decode(&packet(0, &[vec![0]]), false).unwrap_err(),
        Error::MissingReference
    );
    dec.decode(&g_keyframe_dc(), true).unwrap();

    // SKIP: LRU [MOTION, RESIDUE, SKIP, INTRA] -> SKIP is n = 3 ("000")
    let mut w = BitWriter::default();
    w.put("000");
    let f = dec.decode(&packet(0, &[w.finish()]), false).unwrap();
    assert_flat(&f.y, 144, "Y after SKIP");

    // MOTION, one vector (x = 2 half-pels = 1 pixel, y = 0): flat stays flat
    let mut w = BitWriter::default();
    w.put("1").put("1").put("00110").put("1");
    let f = dec.decode(&packet(0, &[w.finish()]), false).unwrap();
    assert_flat(&f.y, 144, "Y after MOTION");
    assert_flat(&f.v, 128, "V after MOTION");

    // RESIDUE ("01"), zero vector, dq 0, luma DC residual c = 8 on block 0 -> inter dc 128 for
    // all blocks -> adds (128 * 8 + 32) >> 6 = 16. Chroma bit 0.
    let mut w = BitWriter::default();
    w.put("01").put("1").put("1").put("1"); // type, n = 1, x 0, y 0
    w.put("1"); // dq
    w.put("1").put("0"); // luma cbp 0, run bit
    w.put("1").put("111110").put("01").put("0");
    for _ in 1..16 {
        w.put("0");
    }
    w.put("0"); // no chroma
    let f = dec.decode(&packet(0, &[w.finish()]), false).unwrap();
    assert_flat(&f.y, 160, "Y after RESIDUE");
    assert_flat(&f.u, 128, "U after RESIDUE");
}

#[test]
fn g_out_of_range_motion_copies_colocated() {
    let mut dec = Decoder::with_params(b'g', 32, 32, 0).unwrap();
    dec.decode(&g_keyframe_dc(), true).unwrap();
    // MOTION, one vector: x = escape. zeros(12) = 0 -> w = 4, u = bits(4) + 15 = 30 -> 15
    // half-pels... use a large one: zeros = 12 -> w = 16, u = 65535 + bits(16) -> far outside.
    let mut w = BitWriter::default();
    w.put("1")
        .put("1")
        .put("0000")
        .put("000000000000")
        .put("0000000000000000")
        .put("1");
    let f = dec.decode(&packet(0, &[w.finish()]), false).unwrap();
    assert_flat(&f.y, 144, "Y");
}

#[test]
fn g_bad_data_errors_do_not_panic() {
    let mut dec = Decoder::with_params(b'h', 64, 64, 3).unwrap();
    // too short for the slice table
    assert!(dec.decode(&[0, 0, 0, 0, 1], true).is_err());
    // slice offsets past the end
    let mut pkt = vec![0u8; 4];
    for _ in 0..7 {
        pkt.extend_from_slice(&1000u32.to_le_bytes());
    }
    assert!(matches!(dec.decode(&pkt, true), Err(Error::InvalidData(_))));
    // Every truncation and many corruptions of a valid packet must give Ok or Err, never panic.
    let mut slices = Vec::new();
    for _ in 0..2 {
        let mut w = BitWriter::default();
        g_flat_intra_mb(&mut w, false);
        g_flat_intra_mb(&mut w, false);
        slices.push(w.finish());
    }
    let good = packet(0x10000, &slices);
    let mut dec = Decoder::with_params(b'h', 64, 64, 0).unwrap();
    for n in 0..good.len() {
        let _ = dec.decode(&good[..n], true);
    }
    let mut seed = 12345u32;
    for _ in 0..2000 {
        let mut p = good.clone();
        p.resize(64, 0);
        for b in p.iter_mut().skip(4) {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            *b = (seed >> 16) as u8;
        }
        let key = seed & 1 == 0;
        let _ = dec.decode(&p, key);
    }
}

/// One float-variant INTRA MB with no residuals. `corner` = MB is at both edges.
fn f_flat_intra_mb(w: &mut BitWriter, corner: bool, alpha: bool, luma_extra: u32) {
    let planes = if alpha { 4 } else { 3 };
    for p in 0..planes {
        w.put("10"); // cbp: lo = prev & 0xFFFF = 0, no further bits
        w.put("1"); // dq = 0 -> q = 8
        w.put("000"); // DC width 0
        if corner {
            // e = F_DC_BITS[8] - 1 + 0 = 2 -> bits(8)
            let v = if p == 0 { luma_extra } else { 0 };
            let s: String = (0..8).map(|i| if v >> i & 1 != 0 { '1' } else { '0' }).collect();
            w.put(&s);
            if v != 0 {
                w.put("0");
            }
        }
    }
}

#[test]
fn f_keyframe_flat_grey() {
    // revision f, 64x32: boundaries [32, 32] -> slice 1 is empty.
    let mut w = BitWriter::default();
    f_flat_intra_mb(&mut w, true, true, 0);
    f_flat_intra_mb(&mut w, false, true, 0);
    let pkt = packet(0, &[w.finish(), vec![]]);
    let mut dec = Decoder::with_params(b'f', 64, 32, 0x0010_0000).unwrap();
    let f = dec.decode(&pkt, true).unwrap();
    assert_flat(&f.y, 128, "Y");
    assert_flat(&f.u, 128, "U");
    assert_flat(&f.v, 128, "V");
    assert_flat(f.alpha.as_ref().unwrap(), 128, "A");
}

#[test]
fn f_keyframe_corner_dc_and_skip() {
    // Corner DC: v = 16 -> r[0] = 16 * F_DC_Q[8] = 160 -> dc 1184 everywhere in luma -> 148.
    let mut w = BitWriter::default();
    f_flat_intra_mb(&mut w, true, false, 16);
    let pkt = packet(0, &[w.finish(), vec![]]);
    let mut dec = Decoder::with_params(b'd', 32, 32, 0).unwrap();
    let f = dec.decode(&pkt, true).unwrap();
    assert_flat(&f.y, 148, "Y");
    assert_flat(&f.u, 128, "U");
    // P frame: SKIP = bits(2) = 1
    let mut w = BitWriter::default();
    w.put("10");
    let f = dec.decode(&packet(0, &[w.finish(), vec![]]), false).unwrap();
    assert_flat(&f.y, 148, "Y after SKIP");
    // MOTION = 2: x and y widths 0, corner offsets 0
    let mut w = BitWriter::default();
    w.put("01").put("000").put("00000").put("000").put("00000");
    let f = dec.decode(&packet(0, &[w.finish(), vec![]]), false).unwrap();
    assert_flat(&f.y, 148, "Y after MOTION");
}

#[test]
fn g_lru_across_macroblocks() {
    // 64x32, two MBs. P frame: SKIP ("000", LRU becomes [MOTION, SKIP, RESIDUE, INTRA]), then
    // SKIP again, now LRU[1] ("01").
    let mut w = BitWriter::default();
    g_flat_intra_mb(&mut w, false);
    g_flat_intra_mb(&mut w, false);
    let mut dec = Decoder::with_params(b'g', 64, 32, 0).unwrap();
    dec.decode(&packet(0, &[w.finish()]), true).unwrap();
    let mut w = BitWriter::default();
    w.put("000").put("01");
    let pkt = packet(0, &[w.finish()]);
    let f = dec.decode(&pkt, false).unwrap();
    assert_flat(&f.y, 128, "Y");
    // "000" twice would select RESIDUE for the second MB and run out of data.
    let mut w = BitWriter::default();
    w.put("000").put("000");
    assert!(dec.decode(&packet(0, &[w.finish()]), false).is_err());
}

#[test]
fn fuzz_all_variants_never_panic() {
    let mut seed = 987_654_321u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    for (rev, w, h, flags) in [
        (b'f', 64u32, 64u32, 0x0010_0000u32),
        (b'a', 40, 72, 0),
        (b'g', 96, 160, 0x0010_0000),
        (b'k', 70, 70, 0x0010_0003),
    ] {
        let mut dec = Decoder::with_params(rev, w, h, flags).unwrap();
        let s = crate::slice_boundaries(rev, h, flags).len();
        for it in 0..1500 {
            let len = 4 * s + (rnd() % 200) as usize;
            let mut p: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
            // keep the slice table mostly valid so the MB decoders get exercised
            let mut off = 4 * s;
            for i in 0..s - 1 {
                off = (off + (rnd() as usize % 40)).min(len);
                p[4 + 4 * i..8 + 4 * i].copy_from_slice(&(off as u32).to_le_bytes());
            }
            if it % 3 != 0 {
                p[0..4].copy_from_slice(&(rnd() & 0x4_0000).to_le_bytes());
            }
            let _ = dec.decode(&p, it % 4 == 0);
        }
    }
    // container parser on random and mutated headers
    let base = build_file(
        b'i',
        64,
        64,
        0,
        &[(44100, 0, 1)],
        &[(true, vec![0; 8]), (false, vec![0; 8])],
    );
    for _ in 0..3000 {
        let mut f = base.clone();
        let n = 1 + rnd() as usize % 4;
        for _ in 0..n {
            let i = rnd() as usize % f.len();
            f[i] = rnd() as u8;
        }
        f.truncate(rnd() as usize % (f.len() + 1));
        if let Ok(c) = Container::parse(&f) {
            for i in 0..c.num_frames() {
                let _ = c.frame(i);
            }
        }
    }
}

#[test]
fn slice_reports_and_concealment() {
    // Two slices (revision h, 64x64). Slice 0 is valid; slice 1 is a single byte of ones, which
    // reads dq = 0 then runs out of data inside the MB.
    let mut w = BitWriter::default();
    g_flat_intra_mb(&mut w, false);
    g_flat_intra_mb(&mut w, false);
    let pkt = packet(0, &[w.finish(), vec![0xFF]]);
    let mut dec = Decoder::with_params(b'h', 64, 64, 0).unwrap();
    assert_eq!(dec.decode(&pkt, true).unwrap_err(), Error::Truncated);
    let r = dec.slice_reports();
    assert_eq!(r.len(), 2);
    assert!(r[0].in_sync());
    assert_eq!(r[1].failure.as_ref().unwrap().mb, (0, 1));

    dec.set_options(crate::DecoderOptions { conceal_slice_errors: true, ..Default::default() });
    let f = dec.decode(&pkt, true).unwrap();
    assert!(dec.slice_reports()[0].in_sync());
    assert!(!dec.slice_reports()[1].in_sync());
    assert_flat(&f.y[..64 * 32], 128, "decoded slice");
    assert_flat(&f.y[64 * 32..], 128, "concealed slice (grey, no reference)");
    assert_eq!(dec.last_frame_flags(), 0);
}
