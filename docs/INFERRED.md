# Inferred output path (black-box findings)

Status: first measurements, 2026-11-22. Not part of [SPEC.md](SPEC.md).

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

## 2. Deblocking filter

RAD smooths 8-pixel block edges; SPEC.md describes no such filter. The model below raises the RGB PSNR of a keyframe from 44.6 to 47.6 dB. It is implemented as `DecoderOptions::deblock` (`DeblockMode::InLoop`, off by default).

### 2.1 Which edges

- Edges every 8 pixels in luma, and every 8 chroma pixels in each chroma plane.
- **Edge flags (SPEC §3.3) switch edges off.** A flag value of 1 means *not filtered*:
  - the vertical edge at x = 8k uses **column flag F[k]**;
  - the horizontal edge at y = 8k uses **row flag F[k−1]** (note the off-by-one against the columns);
  - chroma edge k (chroma x or y = 8k, luma 16k) uses the luma flag of the co-located luma edge: column F[2k], row F[2k−1]. This fits better than F[k] (47.63 against 46.97 dB on the keyframe).
- **Only INTRA macroblocks are filtered.** The MB is the one holding the pixel right of or below the edge (q0). One-step test (decoding a P frame against RAD's own previous frame, §2.3), luma MSE per MB type, frame 8:

  | filtered MB types | INTRA | SKIP | MOTION | RESIDUE |
  |---|---|---|---|---|
  | INTRA only | 0.44 | 0.05 | 0.35 | 0.60 |
  | INTRA + MOTION | 0.44 | 0.05 | 0.60 | 0.60 |
  | INTRA + SKIP | 0.44 | 0.21 | 0.35 | 0.60 |
  | INTRA + RESIDUE | 0.44 | 0.05 | 0.39 | 1.10 |
  | all | 0.44 | 0.21 | 0.64 | 1.10 |

  A weaker filter (`(7 p0 + q0 + 4) >> 3`) on RESIDUE MBs is also worse (0.52 → 0.57 on frame 2).
- **Packets without edge flags** (frame_flags bit 16 clear) are not filtered. That matches because those frames consist almost entirely of SKIP/MOTION MBs. Filtering every edge of such frames (`without_flags`) gives identical PSNR on file 001 when only INTRA MBs are filtered.
- Measured on the keyframe of file 001: edges with flag 0 show RAD moving each edge pixel by α = 0.24 of the step (median, q10–q90 0.21–0.28, vertical edges). Edges with flag 1 show α = 0.02 (q90 0.03), i.e. untouched.
- Near-black areas at the top of that frame look filtered even where the row flag is 1. Clipping makes the implied luma unreliable there, so this is treated as a measurement artefact, but it is **not verified**.

### 2.2 Filter taps

Model: on each enabled edge, with p0 | q0 the two pixels next to it,

```
p0' = (3·p0 + q0 + 2) >> 2
q0' = (3·q0 + p0 + 2) >> 2
```

all vertical edges first, then all horizontal edges (the order measured no difference).

Evidence and limits:
- A free linear fit of RAD's pixels against ours over all filtered vertical edges of the keyframe (95 369 edge rows) gives the following movements, with features (q0−p0, p0−p1, q1−q0):

  | pixel | moves by |
  |---|---|
  | p0 | +0.244·(q0−p0) − 0.10·(p0−p1) |
  | q0 | −0.242·(q0−p0) + 0.09·(q1−q0) |
  | p1, q1 | about ±0.075·(q0−p0) |
  | p2, q2 | about 0 |

  Horizontal edges are the same within ±0.04.
- The movement relative to the step falls smoothly with the step size, from 0.35 at |step| < 4 to 0.21 at |step| ≈ 48–64. Above 64 it is 0.13 (few samples). There is no sharp threshold, and no clear dependence on the MB's q (q 10–36 all similar). A hard threshold of 64 fits slightly worse.
- Candidate integer filters that also move p1/q1 or weight p1 (e.g. `(p1 + 5 p0 + 2 q0 + 4) >> 3`, `(p1 + 11 p0 + 4 q0 + 8) >> 4`, an H.263-style `d = (p1 − 4 p0 + 4 q0 − q1)/16`) all land within 0.02–0.1 RMS of each other. That is below the noise floor (RMS 1.3–1.5 on unmodified pixels), so **the exact taps are not determined**. The two-tap form is kept as the simplest adequate fit.

### 2.3 In-loop or post

**In-loop**: the filtered frame is the reference for the next frame.
- File 001, frames 0–60: post 39.96 dB, in-loop (all MB types) 39.93, in-loop INTRA only 40.26.
- With post-filtering, the SKIP MBs of frame 1 (which copy frame 0) show luma MSE 1.72. In-loop they show 0.71: RAD's frame 1 contains the filtered frame 0.

## 3. Results

RGB PSNR, mean over all frames:

| file | spec-side estimate (BT.601 limited, no filter) | BT.601 full + centred chroma | + in-loop deblock model |
|---|---|---|---|
| 001 (115 frames) | 29.34 | 37.40 | 37.73 |
| 002 (402 frames) | 29.46 | 35.04 | 34.86 |

- File 001: frame 0 goes 44.6 → 47.6 dB, the model is better on 105/115 frames, and no frame is worse by more than 0.01 dB.
- File 002: the model is **slightly worse** on average (−0.18 dB). It is better on only 31/402 frames, and the worst frame loses 0.75 dB. File 002 has one black keyframe and few INTRA MBs, so the filter rarely runs.
- On file 002 frames 0–120, every 10th: no filter 37.03, in-loop luma only 37.04, in-loop luma + chroma 36.99, post 37.03, in-loop with flag-less frames filtered too 36.82.
- So the model fits keyframes of file 001 well but does not generalise. It stays **off by default**.

Verdict: the colour conversion (§1) is the big win (+8 dB on file 001, +5.6 dB on file 002). The deblocking model is a partial fit: right about where the filter runs (flagged edges, INTRA MBs, in-loop) on file 001, but not yet good enough to enable by default.

## 4. Still unexplained

- **Drift over P frames.** PSNR falls from about 47 dB at a keyframe to about 34–36 dB after 60–100 frames, with or without the filter. One-step decoding against RAD's previous frame (bk2onestep, with the reference converted back to YUV) is at about 52 dB luma. SKIP MBs are exact there (MSE 0.05, the round-trip noise); MOTION MBs show MSE 0.26–0.35 and RESIDUE MBs 0.52–0.60. So each P frame adds a small difference in motion compensation and/or inter residual reconstruction, and it accumulates. The source was not found: swapping the IDCT pass order changes nothing measurable, and the remaining candidates (MC filter rounding or intermediate precision, inter DC bias) were not tested.
- The intra decode also differs slightly from RAD inside blocks (luma MSE about 0.4 at interior positions of the keyframe).
- The exact deblocking taps (§2.2) and the meaning of the near-black anomaly (§2.1).
- What edge flags mean on edges of non-INTRA MBs (they are present but appear unused). In a P frame of file 001, INTRA-MB edges whose flag is 1 still looked filtered (α ≈ 0.3, a few hundred samples), contradicting the keyframe rule. That may be why the model does not help file 002, where all INTRA MBs are in P frames.

## 5. Tools

- `bk2dump --report` prints per-slice bit accounting. `--side FILE` writes frame flags, edge flags, MB types and q per frame. `--deblock inloop` enables the model, and `--png` writes frames with the conversion of §1.
- `bk2onestep` decodes one P frame against an externally supplied reference.
