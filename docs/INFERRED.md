# Inferred output path (black-box findings)

Status: first measurements, 2026-11-22; §2–§4 updated for SPEC draft 6. Not part of [SPEC.md](SPEC.md).

This file records what was **inferred from data only**: two real `KB2n` files (2560×1440, 115 and 402 frames) decoded with this crate, compared with frames exported by RAD's own converter (RGB PNG, treated as ground truth). No decoder source or outside description was used. Everything here is a fitted model with measured residuals, not a specification.

Measurement notes:
- "Implied luma" means `Y' = 0.299 R + 0.587 G + 0.114 B` computed from RAD's RGB. With the conversion below, that is RAD's decoded luma up to RGB rounding (about ±0.2).
- "Interior" pixels are rows and columns 2–5 of an 8×8 block, where an edge filter that touches at most two pixels each side does not reach.
- PSNR is over the whole RGB frame (8 bit, all three channels).

## 1. YUV → RGB conversion

Best fit: **BT.601 matrix, full range, centred bilinear chroma upsampling, chroma centred on 128.5, rounded to nearest.**

```
Cb', Cr' = chroma upsampled 2× with weights 3/4 (nearest sample) and 1/4 (next sample) in
           each direction, i.e. (9·c00 + 3·c10 + 3·c01 + c11) / 16, chroma sited at the centre
           of each 2×2 luma group (MPEG-1/JPEG siting)
R = Y + 1.402    · (Cr' − 128.5)
G = Y − 0.344136 · (Cb' − 128.5) − 0.714136 · (Cr' − 128.5)
B = Y + 1.772    · (Cb' − 128.5)
round to nearest, clip to 0…255
```

Evidence (file 001 frame 0, a keyframe):

| conversion | PSNR (RGB) |
|---|---|
| BT.601 limited range, nearest chroma (the earlier guess) | 30.0 dB |
| BT.709 limited | 30.2 dB |
| BT.709 full | 38.2 dB |
| BT.601 full, nearest chroma | 42.4 dB |
| BT.601 full, centred bilinear chroma, chroma centre 128.5 | 44.6 dB |

- Black (letterbox) areas decode to Y = 0, and RAD outputs RGB 0 there: full range.
- A free least-squares fit of RGB on (Y, Cb, Cr, 1) over interior pixels gives R = 0.998 Y + 1.395 Cr, G = 0.998 Y − 0.352 Cb − 0.716 Cr, B = 0.996 Y + 1.751 Cb (BT.601: 1.402 / 0.344 / 0.714 / 1.772). The residual RMS is about 1.1 per channel.
- The upsampling kernel was fitted freely per output phase over a 3×3 chroma neighbourhood. It comes out separable, ¼/¾ in each direction (0.058, 0.185, 0.197, 0.566 against the ideal 0.0625, 0.1875, 0.1875, 0.5625), with no weight on other taps.
- Implied Cb/Cr from RAD's RGB are on average 0.54 lower than our decoded chroma, in both planes and both inside and on block edges. A chroma centre of 128.5 (equivalently, chroma 0.5 lower) removes the RGB bias: the mean RGB error goes from (+0.96, −0.32, +1.25) to (+0.3, +0.2, +0.4) or less.
- An integer upsampler `(9a+3b+3c+d+r) >> 4` fits worse than the float form for both r = 0 and r = 8, so RAD probably upsamples and converts at higher precision.
- **Saturation / colour cast.** With BT.601 *limited* range, our output looks more saturated and more orange-red than RAD's. That comes from the conversion, not from the decoder's chroma levels:
  - Regressing RAD's implied chroma on our decoded chroma (interior pixels, file 001 frame 0) gives slopes of 0.992 (Cb) and 0.989 (Cr), with offsets −0.7 and −0.3. Luma gives 0.999 / −0.14. So the decoded chroma amplitude matches RAD's to about 1 %.
  - Mean RGB of that frame: ours with BT.601 limited (59.7, 35.7, 18.6); ours with the conversion above (62.1, 40.7, 23.3); RAD (61.9, 40.4, 23.0). Limited-range expansion stretches the dark, warm content (Y·1.164 − 18.6, chroma ×1.14), pulling G and B down relative to R.
  - Side by side (`/home/wizy/Videos/bk2/decoded/colour_compare_f001_0.png`: limited, inferred, RAD), the inferred conversion is visually indistinguishable from RAD's.
  - On file 002 frame 40 the slopes are 0.985 / 0.977, which includes accumulated P-frame drift (§4).
- Residual: 47 % of interior RGB values match exactly, RMS 1.16. Inside blocks our decoded luma differs from RAD's implied luma by RMS 0.64 and mean −0.22, more than RGB rounding explains. Part of that is a period-2 pattern (higher error at even row and column positions inside the block), most likely chroma error leaking into the implied luma because the matrix fits only approximately. Its exact source is **not explained**.

## 2. Deblocking filter: superseded

The black-box deblocking model that was here (a two-tap smoothing of flagged 8-pixel edges in INTRA MBs, `DecoderOptions::deblock`) is **superseded by SPEC §4.10** (draft 6) and was removed. The decoder now runs the specified in-loop filter by default (`src/loop_filter.rs`; `DecoderOptions::disable_loop_filter` / `bk2dump --no-loop-filter` switch it off for debugging).

The model got some things right (in-loop; edge flag 1 = edge off; chroma uses the co-located luma flag) and others wrong (the taps, which depend on per-block strength and also move p1/q1; DC smoothing of flat intra blocks; inter edges with differing vectors are filtered too). Its "row flag off by one" was an artefact of the old edge flag layout (SPEC §3.3).

## 3. Results with SPEC draft 6

Luma against RAD's implied luma (`0.299 R + 0.587 G + 0.114 B`), ignoring pixels where any RGB channel is 0 or 255:

| file | mean luma PSNR | pixels off by > 1.5 (worst frame) | max \|difference\| |
|---|---|---|---|
| 001 (115 frames) | 56.73 dB | 0 | 1.05 |
| 002 (402 frames, 397 with unclipped pixels) | 56.74 dB | 0 | 1.05 |

That is the RGB rounding floor on every frame, with no drift. Without the loop filter, file 001 drops to 38.0 dB (43 % of pixels off by > 1.5 on the worst frame).

## 4. Still unexplained

- The P-frame drift of earlier measurements is resolved: it came from the missing loop filter, the edge flag layout, the inter DC range and two motion compensation details (SPEC draft 6).
- The chroma offset and period-2 luma residual of §1 were measured before draft 6 and may have been partly the missing filter; they were not re-measured.

## 5. Tools

- `bk2dump --report` prints per-slice bit accounting. `--side FILE` writes frame flags, edge flags, MB types and q per frame, `--no-loop-filter` switches the SPEC §4.10 filter off, and `--png` writes frames with the conversion of §1.
- `bk2onestep` decodes one P frame against an externally supplied reference.

## 6. Audio implementation: spec questions (AUDIO.md draft 1)

Raised while implementing `src/audio.rs` from AUDIO.md only.

1. **Precision (§3.3).** AUDIO.md says single precision is enough. On the test file (`movies_video_002`, track 0) an f32 transform gives 99.897 % identical samples (max difference 1), just under the 99.92 % of the status line; computing coefficients, transform and gain in f64 gives 99.920 % (1,285,374 of 1,286,400), max difference 1. The crate uses f64. Should §3.3 say double precision, or name 99.9 % as the f32 expectation?
2. **Final alignment (§3.1).** What if the 32-bit boundary after the last block lies past the end of the payload (payload length after `decoded_bytes` not a multiple of 4)? The crate treats the block as complete and stops. A block that runs out of bits before its end is an error.
3. **Trailing bits (§3).** "The payload has bits left" is the loop condition, so any non-zero number of trailing bits starts a new block (and usually fails as truncated). Is zero padding of 32 bits or more possible after the last block?
4. **`decoded_bytes` (§1).** Not used for decoding. On the test file every packet's output matches it exactly. Should a decoder check it, or trim or pad its output to it?
5. **Version selection (§2).** Implemented as revision ≥ `n` → version 2. Revisions `l`/`m` (no samples) would be version 1 by the literal reading.
6. **More than 2 channels (§2).** The track flags only give mono or stereo, so it is unclear how a track with more channels would be signalled; the crate cannot see one and does not check for it.
