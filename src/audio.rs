//! Bink Audio decoder, DCT codec (AUDIO.md).
//!
//! [`AudioDecoder`] turns the per-frame audio payloads of one track into interleaved 16-bit PCM.
//! Mono and stereo tracks with the DCT codec (track flag 0x1000) are supported, in both Bink
//! Audio versions (container revision `n` uses version 2, earlier revisions version 1). Only
//! version 2 at 48 kHz has been checked against RAD's decoder (AUDIO.md, status line).
//!
//! The coefficients, transform and gain are computed in f64. AUDIO.md §3.3 says single
//! precision is enough, but on the test file an f32 transform matched RAD on 99.897 % of the
//! samples and f64 on 99.920 % (both with no difference above 1), so f64 it is; it costs little.

use crate::bits::BitReader;
use crate::container::{AudioTrack, Container};
use crate::error::{Error, Result};

/// Critical frequencies in Hz (AUDIO.md §5, `F`).
const CRITICAL_FREQS: [u32; 25] = [
    100, 200, 300, 400, 510, 630, 770, 920, 1080, 1270, 1480, 1720, 2000, 2320, 2700, 3150, 3700, 4400, 5300, 6400,
    7700, 9500, 12000, 15500, 24500,
];

/// Run lengths in units of 8 coefficients (AUDIO.md §5, `RUN`).
const RUN: [usize; 16] = [2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15, 16, 32, 64];

/// Quantiser step (AUDIO.md §2): `Q[i] = exp(i × QUANT_STEP)` (0.15289164787221953823 rounded
/// to f64).
const QUANT_STEP: f64 = 0.152_891_647_872_219_54;

/// Number of entries of the quantiser table.
const NUM_QUANTS: usize = 96;

/// Transform size `N` for a sample rate (AUDIO.md §2).
fn transform_size(sample_rate: u32) -> usize {
    if sample_rate < 22050 {
        512
    } else if sample_rate < 44100 {
        1024
    } else {
        2048
    }
}

/// Band starts `band[0 ..= NB]` for a sample rate and transform size (AUDIO.md §2).
fn band_starts(sample_rate: u32, n: usize) -> Vec<usize> {
    let half = sample_rate.div_ceil(2).max(1) as u64;
    let nb = (1..=24).find(|&nb| half <= CRITICAL_FREQS[nb - 1] as u64).unwrap_or(25);
    let mut band = Vec::with_capacity(nb + 1);
    band.push(2);
    for i in 1..nb {
        let start = (CRITICAL_FREQS[i - 1] as u64 * n as u64 / half) & !1;
        band.push(start as usize);
    }
    band.push(n);
    band
}

/// `float29()` from its three fields (AUDIO.md §3.2): `m × 2^(e − 23)`, negated if `s`.
fn float29_value(e: u32, m: u32, s: bool) -> f64 {
    let v = m as f64 * 2f64.powi(e as i32 - 23);
    if s {
        -v
    } else {
        v
    }
}

fn read_float29(r: &mut BitReader) -> Result<f64> {
    let e = r.bits(5)?;
    let m = r.bits(23)?;
    let s = r.bit()?;
    Ok(float29_value(e, m, s))
}

/// Complex number for the FFT.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Cpx {
    re: f64,
    im: f64,
}

impl Cpx {
    fn mul(self, o: Cpx) -> Cpx {
        Cpx {
            re: self.re * o.re - self.im * o.im,
            im: self.re * o.im + self.im * o.re,
        }
    }
}

/// DCT-III of AUDIO.md §3.3 in O(N log N):
/// `y[n] = X[0] + Σ_{k=1}^{N-1} X[k] cos(π k (n + ½) / N)`.
///
/// Method: with `X[N] = 0`, form `V[k] = e^{iπk/2N} (X'[k] − i X[N−k])` where `X'[0] = 2 X[0]`
/// and `X'[k] = X[k]` otherwise, take `v[n] = ½ Σ_k V[k] e^{2πikn/N}` (an inverse FFT of size
/// N without normalisation), then `y[2n] = Re v[n]` and `y[2n+1] = Re v[N−1−n]`.
#[derive(Debug, Clone)]
struct Dct3 {
    n: usize,
    /// `½ e^{iπk/2N}` for `k = 0 … N−1` (the ½ of the method folded in).
    pre: Vec<Cpx>,
    /// `e^{2πik/N}` for `k = 0 … N/2−1`.
    twiddle: Vec<Cpx>,
    /// Bit-reversal permutation of `0 … N−1`.
    bitrev: Vec<u32>,
    buf: Vec<Cpx>,
}

impl Dct3 {
    /// `n` must be a power of two, at least 2.
    fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && n >= 2);
        let pre = (0..n)
            .map(|k| {
                let a = std::f64::consts::PI * k as f64 / (2 * n) as f64;
                Cpx {
                    re: (0.5 * a.cos()),
                    im: (0.5 * a.sin()),
                }
            })
            .collect();
        let twiddle = (0..n / 2)
            .map(|k| {
                let a = 2.0 * std::f64::consts::PI * k as f64 / n as f64;
                Cpx {
                    re: a.cos(),
                    im: a.sin(),
                }
            })
            .collect();
        let bits = n.trailing_zeros();
        let bitrev = (0..n as u32).map(|i| i.reverse_bits() >> (32 - bits)).collect();
        Dct3 {
            n,
            pre,
            twiddle,
            bitrev,
            buf: vec![Cpx::default(); n],
        }
    }

    /// In-place radix-2 inverse FFT (positive exponent, no normalisation) of `self.buf`.
    fn ifft(&mut self) {
        let n = self.n;
        for i in 0..n {
            let j = self.bitrev[i] as usize;
            if j > i {
                self.buf.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let halfl = len / 2;
            let step = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..halfl {
                    let t = self.buf[start + k + halfl].mul(self.twiddle[k * step]);
                    let u = self.buf[start + k];
                    self.buf[start + k] = Cpx {
                        re: u.re + t.re,
                        im: u.im + t.im,
                    };
                    self.buf[start + k + halfl] = Cpx {
                        re: u.re - t.re,
                        im: u.im - t.im,
                    };
                }
            }
            len *= 2;
        }
    }

    /// Transforms `x` (length N) into `y` (length N).
    fn transform(&mut self, x: &[f64], y: &mut [f64]) {
        let n = self.n;
        debug_assert!(x.len() == n && y.len() == n);
        self.buf[0] = Cpx {
            re: 2.0 * x[0] * self.pre[0].re,
            im: 0.0,
        };
        for k in 1..n {
            let w = Cpx {
                re: x[k],
                im: -x[n - k],
            };
            self.buf[k] = w.mul(self.pre[k]);
        }
        self.ifft();
        for i in 0..n / 2 {
            y[2 * i] = self.buf[i].re;
            y[2 * i + 1] = self.buf[n - 1 - i].re;
        }
    }
}

/// Overlap step of AUDIO.md §4.5 for one channel: crossfades `s[0 … OV−1]` from `prev`.
fn overlap(s: &mut [i32], prev: &[i32], ch: usize, channels: usize) {
    let ov = prev.len();
    let w_total = (ov * channels) as i64;
    for j in 0..ov {
        let w = (ch + j * channels) as i64;
        let v = prev[j] as i64 * (w_total - w) + s[j] as i64 * w;
        s[j] = v.div_euclid(w_total) as i32;
    }
}

/// Bink Audio decoder for one track (DCT codec, 1 or 2 channels).
#[derive(Debug, Clone)]
pub struct AudioDecoder {
    sample_rate: u32,
    channels: usize,
    version2: bool,
    /// Transform size `N`.
    n: usize,
    /// Overlap length `OV = N / 16`.
    ov: usize,
    /// Band starts `band[0 ..= NB]`.
    band: Vec<usize>,
    /// Quantiser table `Q`.
    quant: [f64; NUM_QUANTS],
    /// Gain `G = 2 / √N`.
    gain: f64,
    dct: Dct3,
    /// Previous block tails `P`, per channel (`OV` values each).
    prev: Vec<Vec<i32>>,
    /// The next block is a "first block" (no overlap).
    first: bool,
    // Scratch buffers.
    coeffs: Vec<f64>,
    time: Vec<f64>,
    samples: Vec<Vec<i32>>,
    mags: Vec<u32>,
    qband: Vec<f64>,
}

impl AudioDecoder {
    /// Creates a decoder for `track`. `revision` is the container revision letter
    /// ([`Header::revision`](crate::Header::revision)): `b'n'` and later use Bink Audio 2,
    /// earlier revisions Bink Audio 1.
    ///
    /// Returns [`Error::Unsupported`] for the RDFT codec (flag 0x1000 clear) and
    /// [`Error::InvalidHeader`] for a sample rate of 0.
    pub fn new(track: &AudioTrack, revision: u8) -> Result<Self> {
        if !track.is_dct() {
            return Err(Error::Unsupported("Bink Audio RDFT codec"));
        }
        if track.sample_rate == 0 {
            return Err(Error::InvalidHeader("audio sample rate 0"));
        }
        // The stereo flag gives 1 or 2 channels; more than 2 cannot be signalled here
        // (AUDIO.md §2 leaves them out of scope).
        let channels = if track.is_stereo() { 2 } else { 1 };
        let sample_rate = track.sample_rate as u32;
        let n = transform_size(sample_rate);
        let ov = n / 16;
        let mut quant = [0f64; NUM_QUANTS];
        for (i, q) in quant.iter_mut().enumerate() {
            *q = (i as f64 * QUANT_STEP).exp();
        }
        let band = band_starts(sample_rate, n);
        let nb = band.len() - 1;
        Ok(AudioDecoder {
            sample_rate,
            channels,
            version2: revision >= b'n',
            n,
            ov,
            band,
            quant,
            gain: (2.0 / (n as f64).sqrt()),
            dct: Dct3::new(n),
            prev: vec![vec![0; ov]; channels],
            first: true,
            coeffs: vec![0.0; n],
            time: vec![0.0; n],
            samples: vec![vec![0; n]; channels],
            mags: vec![0; n],
            qband: vec![0.0; nb],
        })
    }

    /// Number of channels (1 or 2).
    pub fn channels(&self) -> u16 {
        self.channels as u16
    }

    /// Sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Samples per channel that one block outputs (`L = N − N/16`).
    pub fn block_samples(&self) -> usize {
        self.n - self.ov
    }

    /// Whether this decoder uses Bink Audio 2 (revision `n` and later).
    pub fn is_version2(&self) -> bool {
        self.version2
    }

    /// Forgets the overlap state: the next block is decoded as a first block (AUDIO.md §4.6).
    pub fn reset(&mut self) {
        for p in &mut self.prev {
            p.fill(0);
        }
        self.first = true;
    }

    /// Decodes one frame's payload for this track (as in
    /// [`FramePacket::audio`](crate::FramePacket::audio), starting with the `u32`
    /// `decoded_bytes`) and appends interleaved 16-bit samples to `out`.
    ///
    /// A payload shorter than 4 bytes is empty and appends nothing. On error, `out` keeps the
    /// samples of the blocks decoded before the error, and the overlap state is that of the
    /// last complete block.
    pub fn decode(&mut self, payload: &[u8], out: &mut Vec<i16>) -> Result<()> {
        if payload.len() < 4 {
            return Ok(());
        }
        let mut r = BitReader::new(&payload[4..]);
        while r.remaining() > 0 {
            self.decode_block(&mut r, out)?;
        }
        Ok(())
    }

    /// One block (AUDIO.md §3.1), then the output stage (§4).
    fn decode_block(&mut self, r: &mut BitReader, out: &mut Vec<i16>) -> Result<()> {
        r.skip(2)?;
        for ch in 0..self.channels {
            self.read_channel(r)?;
            self.dct.transform(&self.coeffs, &mut self.time);
            let s = &mut self.samples[ch];
            for (dst, &y) in s.iter_mut().zip(&self.time) {
                *dst = (self.gain * y).round_ties_even().clamp(-32768.0, 32767.0) as i32;
            }
        }
        // Skip to the next multiple of 32 bits. SPEC-QUESTION: AUDIO.md does not say what happens
        // when that boundary lies past the end of the payload (a payload length that is not a
        // multiple of 4 after `decoded_bytes`); this takes the block as complete and the payload
        // as ended.
        let aligned = r.position().div_ceil(32) * 32;
        let skip = (aligned - r.position()).min(r.remaining());
        r.skip(skip)?;

        // Output (§4.5), only once the whole block has parsed.
        let l = self.n - self.ov;
        for ch in 0..self.channels {
            let s = &mut self.samples[ch];
            if !self.first {
                overlap(&mut s[..self.ov], &self.prev[ch], ch, self.channels);
            }
            self.prev[ch].copy_from_slice(&s[l..]);
        }
        self.first = false;
        out.reserve(l * self.channels);
        for i in 0..l {
            for ch in 0..self.channels {
                out.push(self.samples[ch][i] as i16);
            }
        }
        Ok(())
    }

    /// One channel (AUDIO.md §3.2): fills `self.coeffs`.
    fn read_channel(&mut self, r: &mut BitReader) -> Result<()> {
        let n = self.n;
        let nb = self.band.len() - 1;
        let qbits = if self.version2 { 7 } else { 8 };
        let x = &mut self.coeffs;
        x[0] = read_float29(r)?;
        x[1] = read_float29(r)?;
        for b in 0..nb {
            let qi = r.bits(qbits)? as usize;
            self.qband[b] = self.quant[qi.min(NUM_QUANTS - 1)];
        }
        let band = &self.band;
        let q = &self.qband;
        let mut k = 0usize;
        let mut cq = q[0];
        let mut i = 2usize;
        while i < n {
            let len = if r.bit()? { 8 * RUN[r.bits(4)? as usize] } else { 8 };
            let end = (i + len).min(n);
            let w = r.bits(4)?;
            if w == 0 {
                x[i..end].fill(0.0);
                i = end;
                // band[NB] = N >= i, so k stays <= NB.
                while band[k] < i {
                    cq = q[k];
                    k += 1;
                }
            } else {
                if self.version2 {
                    for m in &mut self.mags[i..end] {
                        *m = r.bits(w)?;
                    }
                }
                for (p, xp) in x.iter_mut().enumerate().take(end).skip(i) {
                    // band[NB] = N > p, so q[k] is never read past NB − 1.
                    if band[k] == p {
                        cq = q[k];
                        k += 1;
                    }
                    let mag = if self.version2 { self.mags[p] } else { r.bits(w)? };
                    *xp = if mag == 0 {
                        0.0
                    } else if r.bit()? {
                        -(mag as f64) * cq
                    } else {
                        mag as f64 * cq
                    };
                }
                i = end;
            }
        }
        Ok(())
    }
}

/// Decodes audio track `track` of `container` from frame 0 to the end.
///
/// Returns the interleaved samples, the sample rate and the number of channels.
pub fn decode_track(container: &Container, track: usize) -> Result<(Vec<i16>, u32, u16)> {
    let info = container
        .header
        .audio_tracks
        .get(track)
        .ok_or(Error::InvalidHeader("no such audio track"))?;
    let mut dec = AudioDecoder::new(info, container.header.revision)?;
    let mut out = Vec::new();
    for i in 0..container.num_frames() {
        let pkt = container.frame(i)?;
        if let Some(payload) = pkt.audio.get(track) {
            dec.decode(payload, &mut out)?;
        }
    }
    Ok((out, dec.sample_rate(), dec.channels()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LSB-first bit writer.
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        n: usize,
    }

    impl BitWriter {
        fn put(&mut self, v: u32, bits: u32) {
            for b in 0..bits {
                if self.n.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if (v >> b) & 1 != 0 {
                    *self.bytes.last_mut().unwrap() |= 1 << (self.n % 8);
                }
                self.n += 1;
            }
        }
        fn align32(&mut self) {
            while !self.n.is_multiple_of(32) {
                self.put(0, 1);
            }
        }
    }

    fn direct_dct3(x: &[f64]) -> Vec<f64> {
        let n = x.len();
        (0..n)
            .map(|i| {
                let mut s = x[0];
                for (k, &xk) in x.iter().enumerate().skip(1) {
                    s += xk * (std::f64::consts::PI * k as f64 * (i as f64 + 0.5) / n as f64).cos();
                }
                s
            })
            .collect()
    }

    /// Small deterministic pseudo-random generator.
    fn lcg(seed: &mut u64) -> f64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((*seed >> 40) as f64 / (1u64 << 24) as f64) * 2.0 - 1.0
    }

    #[test]
    fn dct3_matches_direct_sum() {
        let mut seed = 1;
        for &n in &[2usize, 4, 8, 16, 64, 2048] {
            let x: Vec<f64> = (0..n).map(|_| lcg(&mut seed) * 1000.0).collect();
            let mut y = vec![0f64; n];
            let mut dct = Dct3::new(n);
            dct.transform(&x, &mut y);
            let want = direct_dct3(&x);
            let scale = x.iter().map(|v| v.abs()).sum::<f64>();
            let max_err = y.iter().zip(&want).map(|(&a, &b)| (a - b).abs()).fold(0.0, f64::max);
            assert!(max_err <= 1e-12 * scale, "n={n}: max error {max_err} (scale {scale})");
        }
    }

    #[test]
    fn dct3_dc_and_single_cosine() {
        let n = 16;
        let mut dct = Dct3::new(n);
        let mut y = vec![0f64; n];
        let mut x = vec![0f64; n];
        x[0] = 3.0;
        dct.transform(&x, &mut y);
        assert!(y.iter().all(|&v| (v - 3.0).abs() < 1e-12), "{y:?}");
        x[0] = 0.0;
        x[5] = 2.0;
        dct.transform(&x, &mut y);
        for (i, &v) in y.iter().enumerate() {
            let want = 2.0 * (std::f64::consts::PI * 5.0 * (i as f64 + 0.5) / n as f64).cos();
            assert!((v - want).abs() < 1e-12);
        }
    }

    #[test]
    fn bands_48k() {
        let band = band_starts(48000, transform_size(48000));
        assert_eq!(
            band,
            [
                2, 8, 16, 24, 34, 42, 52, 64, 78, 92, 108, 126, 146, 170, 196, 230, 268, 314, 374, 452, 546, 656, 810,
                1024, 1322, 2048
            ]
        );
        assert_eq!(band.len() - 1, 25);
        // Low rates: fewer bands, all below N, non-decreasing.
        for rate in [1u32, 2, 199, 200, 8000, 11025, 22050, 32000, 44100, 65535] {
            let n = transform_size(rate);
            let b = band_starts(rate, n);
            assert_eq!(*b.last().unwrap(), n);
            assert!(b.windows(2).all(|w| w[0] <= w[1]), "rate {rate}: {b:?}");
        }
        assert_eq!(band_starts(8000, 512).len() - 1, 18); // half = 4000 <= F[17] = 4400
    }

    #[test]
    fn float29() {
        assert_eq!(float29_value(23, 5, false), 5.0);
        assert_eq!(float29_value(23, 5, true), -5.0);
        assert_eq!(float29_value(0, 1 << 22, false), 0.5);
        assert_eq!(float29_value(31, 3, false), 3.0 * 256.0);
        assert_eq!(float29_value(10, 0, true), 0.0);
        // Field order in the stream: e (5), m (23), s (1).
        let mut w = BitWriter::default();
        w.put(24, 5);
        w.put(3, 23);
        w.put(1, 1);
        let mut r = BitReader::new(&w.bytes);
        assert_eq!(read_float29(&mut r).unwrap(), -6.0);
        assert_eq!(r.position(), 29);
    }

    #[test]
    fn overlap_hand_made() {
        // OV = 2, stereo: W = 4, weights w = ch + 2j.
        let prev = [100, -7];
        let mut s = [0, 9, 55];
        overlap(&mut s, &prev, 0, 2);
        // j=0: w=0 -> (100*4 + 0) / 4 = 100; j=1: w=2 -> (-7*2 + 9*2) / 4 = 4/4 = 1
        assert_eq!(s, [100, 1, 55]);
        let mut s = [0, 9];
        overlap(&mut s, &prev, 1, 2);
        // j=0: w=1 -> (100*3 + 0) / 4 = 75; j=1: w=3 -> (-7*1 + 9*3) / 4 = 20/4 = 5
        assert_eq!(s, [75, 5]);
        // Floor division of negative values.
        let mut s = [-1, -1];
        overlap(&mut s, &[-2, -2], 0, 1);
        // W = 2: j=0: w=0 -> -4/2 = -2; j=1: w=1 -> (-2 - 1) / 2 = -1.5 -> -2
        assert_eq!(s, [-2, -2]);
    }

    fn mono_track(rate: u16) -> AudioTrack {
        AudioTrack {
            max_decoded_size: 0,
            sample_rate: rate,
            flags: 0x1000 | 0x4000,
            id: 0,
        }
    }

    /// Writes one mono block: DC `dc` (as float29 with e = 23), all bands at quantiser 0, and
    /// all coefficients 2 … N−1 zero via 64-coefficient zero runs.
    fn write_dc_block(w: &mut BitWriter, n: usize, nb: usize, dc: u32, qbits: u32) {
        w.put(0, 2);
        w.put(23, 5);
        w.put(dc, 23);
        w.put(0, 1);
        w.put(0, 29); // X[1] = 0
        for _ in 0..nb {
            w.put(0, qbits);
        }
        let mut i = 2;
        while i < n {
            w.put(1, 1);
            w.put(15, 4); // RUN[15] = 64 -> 512 coefficients
            w.put(0, 4); // w = 0: zeros
            i += 512;
        }
        w.align32();
    }

    #[test]
    fn decode_dc_blocks_and_overlap() {
        let mut dec = AudioDecoder::new(&mono_track(48000), b'n').unwrap();
        assert_eq!(
            (dec.channels(), dec.sample_rate(), dec.block_samples()),
            (1, 48000, 1920)
        );
        // DC 4545 -> y = 4545, G = 2/sqrt(2048) -> 200.87... -> 201.
        let mut w = BitWriter::default();
        w.put(0, 32); // decoded_bytes (not checked)
        write_dc_block(&mut w, 2048, 25, 4545, 7);
        write_dc_block(&mut w, 2048, 25, 0, 7);
        let mut out = Vec::new();
        dec.decode(&w.bytes, &mut out).unwrap();
        assert_eq!(out.len(), 2 * 1920);
        assert!(out[..1920].iter().all(|&v| v == 201));
        // Second block: crossfade from 201 to 0 over OV = 128 samples, then 0.
        for j in 0..128 {
            assert_eq!(out[1920 + j] as i64, (201 * (128 - j as i64)).div_euclid(128));
        }
        assert!(out[1920 + 128..].iter().all(|&v| v == 0));
        // After a reset the first block is output unchanged.
        dec.reset();
        let mut w = BitWriter::default();
        w.put(0, 32);
        write_dc_block(&mut w, 2048, 25, 0, 7);
        out.clear();
        dec.decode(&w.bytes, &mut out).unwrap();
        assert!(out.iter().all(|&v| v == 0));
        // Version 1 reads 8-bit quantiser indices.
        let mut dec1 = AudioDecoder::new(&mono_track(48000), b'k').unwrap();
        assert!(!dec1.is_version2());
        let mut w = BitWriter::default();
        w.put(0, 32);
        write_dc_block(&mut w, 2048, 25, 4545, 8);
        out.clear();
        dec1.decode(&w.bytes, &mut out).unwrap();
        assert!(out.len() == 1920 && out.iter().all(|&v| v == 201));
    }

    #[test]
    fn rejects_unsupported_and_malformed() {
        let mut t = mono_track(48000);
        t.flags = 0x2000;
        assert!(matches!(AudioDecoder::new(&t, b'n'), Err(Error::Unsupported(_))));
        assert!(AudioDecoder::new(&mono_track(0), b'n').is_err());
        let mut dec = AudioDecoder::new(&mono_track(22050), b'n').unwrap();
        let mut out = Vec::new();
        dec.decode(&[], &mut out).unwrap();
        dec.decode(&[1, 2, 3], &mut out).unwrap();
        assert!(out.is_empty());
        // Truncated block.
        assert_eq!(dec.decode(&[0, 0, 0, 0, 0xFF], &mut out), Err(Error::Truncated));
        // Random garbage never panics.
        let mut seed = 7u64;
        for len in 0..300 {
            let data: Vec<u8> = (0..len).map(|_| (lcg(&mut seed) * 127.0 + 128.0) as u8).collect();
            for rate in [8000u16, 32000, 48000] {
                for rev in *b"kn" {
                    let mut d = AudioDecoder::new(&mono_track(rate), rev).unwrap();
                    let _ = d.decode(&data, &mut out);
                }
            }
        }
    }

    /// Finds the format and `data` chunk of a 16-bit PCM RIFF WAV.
    fn parse_wav(d: &[u8]) -> (u16, u32, Vec<i16>) {
        assert!(&d[0..4] == b"RIFF" && &d[8..12] == b"WAVE", "not a WAV file");
        let mut pos = 12;
        let mut fmt = None;
        while pos + 8 <= d.len() {
            let id = &d[pos..pos + 4];
            let size = u32::from_le_bytes(d[pos + 4..pos + 8].try_into().unwrap()) as usize;
            let body = &d[pos + 8..(pos + 8 + size).min(d.len())];
            if id == b"fmt " {
                let tag = u16::from_le_bytes([body[0], body[1]]);
                let ch = u16::from_le_bytes([body[2], body[3]]);
                let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                let bits = u16::from_le_bytes([body[14], body[15]]);
                assert!((tag == 1 || tag == 0xFFFE) && bits == 16, "not 16-bit PCM");
                fmt = Some((ch, rate));
            } else if id == b"data" {
                let (ch, rate) = fmt.expect("data before fmt");
                let samples = body
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| i16::from_le_bytes([b[0], b[1]]))
                    .collect();
                return (ch, rate, samples);
            }
            pos += 8 + size + (size & 1);
        }
        panic!("no data chunk");
    }

    #[test]
    #[ignore = "needs BINK2_AUDIO_BK2 and BINK2_AUDIO_REF_WAV"]
    fn audio_matches_reference_wav() {
        let (Ok(bk2), Ok(wav)) = (std::env::var("BINK2_AUDIO_BK2"), std::env::var("BINK2_AUDIO_REF_WAV")) else {
            eprintln!("BINK2_AUDIO_BK2 / BINK2_AUDIO_REF_WAV not set, skipping");
            return;
        };
        let data = std::fs::read(bk2).unwrap();
        let file = Container::parse(&data).unwrap();
        let t0 = std::time::Instant::now();
        let (pcm, rate, channels) = decode_track(&file, 0).unwrap();
        let elapsed = t0.elapsed();
        let (ref_ch, ref_rate, reference) = parse_wav(&std::fs::read(wav).unwrap());
        assert_eq!((channels, rate), (ref_ch, ref_rate));

        // decoded_bytes against the actual output, per packet.
        let mut dec = AudioDecoder::new(&file.header.audio_tracks[0], file.header.revision).unwrap();
        let mut mismatched = 0;
        for i in 0..file.num_frames() {
            let pkt = file.frame(i).unwrap();
            let mut out = Vec::new();
            dec.decode(pkt.audio[0], &mut out).unwrap();
            if let Some(db) = crate::container::audio_decoded_size(pkt.audio[0]) {
                if db as usize != out.len() * 2 {
                    mismatched += 1;
                }
            }
        }

        let same = pcm.iter().zip(&reference).filter(|(a, b)| a == b).count();
        let max_diff = pcm
            .iter()
            .zip(&reference)
            .map(|(&a, &b)| (a as i32 - b as i32).abs())
            .max()
            .unwrap_or(0);
        let pct = 100.0 * same as f64 / reference.len().max(1) as f64;
        println!(
            "audio: {} Hz, {} ch, decoded {} samples ({} per channel), reference {}; identical {same} ({pct:.4} %), \
             max |diff| {max_diff}; packets with decoded_bytes != output: {mismatched}; decode time {:.1} ms",
            rate,
            channels,
            pcm.len(),
            pcm.len() / channels as usize,
            reference.len(),
            elapsed.as_secs_f64() * 1000.0
        );
        assert_eq!(pcm.len(), reference.len());
        assert!(pct >= 99.9);
        assert!(max_diff <= 1);
    }
}
