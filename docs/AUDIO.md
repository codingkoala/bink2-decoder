# Bink Audio: decoding specification

Status: draft 1 (2026-11-22). The DCT codec of §4 at 48 kHz stereo was checked against RAD's own decoder on a real `KB2n` file with one audio track (402 video frames, 13.4 s, 317 audio packets). Every packet parses to its exact end; the output has exactly as many samples as RAD's; 99.92 % of the 16-bit samples are identical and none differs by more than 1. Everything else is marked **(unverified)**.

This document is a companion to [SPEC.md](SPEC.md), which covers the container and the video. It uses the conventions and the bit reader of SPEC §1.

## Provenance and clean-room note

This is a functional description written by studying the LGPL-2.1+ FFmpeg Bink Audio decoder (`libavcodec/binkaudio.c`, authors Peter Ross and Daniel Verkamp) and the changes for the newer Bink Audio published in librempeg by Peter Ross. It contains no code from those files: the algorithms are restated in prose and pseudocode. The output stage (§4.5) and the gain (§4.4) were established by comparing a prototype with the WAV that RAD Video Tools (`radvideo64.exe BinkConv in.bk2 out.wav`) writes for the test file, sample by sample.

The same process applies as for SPEC.md: implement from this document only, and report anything unclear as a spec question.

---

## 1. Where the audio is

- Track table: SPEC §2.1. Per track, `sample_rate` (Hz) and `flags`: 0x2000 stereo (mono otherwise), 0x1000 DCT codec (RDFT codec otherwise), 0x4000 "16-bit" (no effect on decoding).
- Packets: SPEC §2.2. In each frame, track `t` has a payload of `size` bytes. A payload with `size < 4` is empty (no audio in that frame).
- A non-empty payload starts with `u32 decoded_bytes`: the number of bytes of 16-bit PCM it decodes to, all channels together. The rest of the payload is the **block stream** (§3).

### 1.1 Timing

Audio sample 0 plays at video time 0. A packet's first sample is the sum of the `decoded_bytes` of all earlier packets of that track, divided by `2 × channels`. The first frame usually carries much more audio than the others (the test file: 19 blocks in frame 0, then one block of 1920 samples per frame), so audio runs ahead of the video in the file, and the player buffers it.

For the test file, the `decoded_bytes` of all packets add up to `2 × channels × duration × sample_rate` exactly (13.4 s at 48 kHz).

## 2. Codec parameters

All are derived from the track header:

- `C` = channels: 2 if stereo, 1 otherwise.
- `N` = transform size: 512 if `sample_rate < 22050`, 1024 if `sample_rate < 44100`, otherwise 2048.
- `OV` = `N / 16` = overlap length.
- `L` = `N − OV` = samples per channel that one block outputs.
- `half` = `(sample_rate + 1) / 2` (integer division).
- **Bands.** With the critical frequency table `F` (§5):
  - `NB` (number of bands) = the smallest `n` in 1 … 24 for which `half ≤ F[n − 1]`, or 25 if there is none.
  - Band starts: `band[0] = 2`; for `i = 1 … NB − 1`, `band[i] = (F[i − 1] × N / half)` rounded down to an even number (integer division, then clear bit 0); `band[NB] = N`.
  - Example: 48000 Hz, `N = 2048` gives `NB = 25` and starts `2, 8, 16, 24, 34, 42, 52, 64, 78, 92, 108, 126, 146, 170, 196, 230, 268, 314, 374, 452, 546, 656, 810, 1024, 1322`, then 2048.
- **Quantiser table.** `Q[i] = exp(i × 0.15289164787221953823)` for `i = 0 … 95` (about 0.664 dB per step).
- **Version.** Container revision `n` uses **Bink Audio 2**. Earlier revisions use **Bink Audio 1**. The two differ only in the two places marked in §3.2. Only version 2 has been checked against RAD.

Mono and stereo, 1 or 2 channels, are the cases this document covers. Tracks with more than 2 channels exist in principle (they are coded as consecutive stereo blocks) but are out of scope: a decoder may reject them.

## 3. Block stream (DCT codec, flag 0x1000)

A payload holds one or more **blocks**, one after the other, until the payload ends. The bit reader starts at the first byte after `decoded_bytes`. A block ends with an alignment to a 32-bit boundary (counted from that same starting byte). In the test file, every payload ends exactly at the end of a block, so "the payload has bits left" is the loop condition.

### 3.1 Block

```
block:
    bits(2)                       // unused
    for ch in 0 .. C-1:
        channel(ch)               // fills X_ch[0 .. N-1]
    skip to the next multiple of 32 bits
```

### 3.2 Channel

```
channel:
    X[0] = float29()
    X[1] = float29()
    for b in 0 .. NB-1:
        qi[b] = bits(QB)          // QB = 7 in version 2, 8 in version 1
        q[b]  = Q[min(qi[b], 95)]
    coefficients()
```

`float29()` reads 5 bits `e`, then 23 bits `m`, then 1 sign bit `s`. The value is `m × 2^(e − 23)`, negated if `s = 1`.

**Coefficients 2 … N−1** are coded in **runs**. They use a band pointer `k` (starting at 0) and a current quantiser `cq` (starting at `q[0]`):

```
coefficients:
    i = 2
    while i < N:
        if bit() == 1: len = 8 × RUN[bits(4)]
        else:          len = 8
        end = min(i + len, N)
        w = bits(4)
        if w == 0:
            X[i .. end-1] = 0
            i = end
            while band[k] < i:
                cq = q[k];  k = k + 1
        else:
            (version 2) read the magnitudes first:
                for p in i .. end-1:  mag[p] = bits(w)
            then, for p in i .. end-1, in order:
                if band[k] == p:  cq = q[k];  k = k + 1
                (version 1) mag[p] = bits(w)
                if mag[p] != 0:
                    if bit() == 1: X[p] = −mag[p] × cq
                    else:          X[p] = +mag[p] × cq
                else:
                    X[p] = 0
            i = end
```

`RUN` is in §5. So in version 2 a run is all of its magnitudes (`w` bits each), then one sign bit for each non-zero magnitude, in position order. In version 1 each magnitude is directly followed by its sign bit (when non-zero).

The two band rules are as written: after a zero run, `cq` is set to the quantiser of the last band whose start is below the new position. In a coded run, `cq` changes exactly at a band start.

### 3.3 Inverse transform

For each channel, the block's samples before the gain are

```
y[n] = X[0] + Σ_{k=1}^{N-1} X[k] × cos(π × k × (n + ½) / N),     n = 0 … N-1
```

This is a DCT-III without a factor ½ on the DC term. Use a fast algorithm (an FFT-based DCT-III, O(N log N)). The direct sum costs N² per channel and block, which is too slow for real time at `N = 2048`. Single-precision float is enough: the ±1 differences against RAD come from float rounding in any case.

## 4. Output

### 4.4 Gain and conversion

`s[n] = clamp(round(G × y[n]), −32768, 32767)` with `G = 2 / √N`. `round` is round-half-to-even. Clip to 16 bits **before** the overlap step of §4.5.

`G` was measured only at `N = 2048` (**unverified** for 512 and 1024, but it follows from the same normalisation).

### 4.5 Overlap

For each channel, keep the last `OV` values `s[L … N-1]` of the previous block as `P[0 … OV-1]`.

- The **first block** after the start of the track, or after a reset (§4.6), outputs `s[0 … L-1]` unchanged.
- Every later block replaces its first `OV` values before output, for `j = 0 … OV-1`:
  ```
  W = OV × C
  w = ch + j × C
  s[j] = floor((P[j] × (W − w) + s[j] × w) / W)        // integers, floor division
  ```
  Then it outputs `s[0 … L-1]`.
- In both cases the block then saves `s[L … N-1]` as the new `P` (those values are never changed by the overlap step, because `L ≥ OV`).

The weight starts at `ch / W`, not 0, and steps by `C / W`. This is how RAD crossfades, and it differs slightly between the left and right channels.

The output of a block is `L` samples per channel. Interleave the channels for a PCM stream (left first).

### 4.6 Reset and seeking

A reset clears `P` and makes the next block a "first block". To start playback at a video frame other than 0, reset, then decode from a packet at or before the target. The test file has no audio-only keyframes. Starting at an arbitrary packet only loses the crossfade of the first block, and the next block is normal again, because blocks don't depend on each other except through `P`. Decoding a whole track up front is cheap (the test track takes a fraction of a second) and avoids the question.

## 5. Tables

**`RUN`** (index from `bits(4)`, then multiplied by 8):

```
2, 3, 4, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15, 16, 32, 64
```

**`F`**, the critical frequencies in Hz (the same 25 band edges as several other audio codecs):

```
100, 200, 300, 400, 510, 630, 770, 920, 1080, 1270, 1480, 1720, 2000,
2320, 2700, 3150, 3700, 4400, 5300, 6400, 7700, 9500, 12000, 15500, 24500
```

## 6. Not covered

1. **RDFT codec** (flag 0x1000 clear). It interleaves the channels into one transform and uses a different transform and gain. There is no test file. A decoder should report it as unsupported.
2. **More than 2 channels.**
3. **Bink Audio 1 against RAD.** The version-1 rules come from the FFmpeg decoder only.
4. **`N` = 512 or 1024** (sample rates below 44100 Hz): the gain of §4.4.
5. The remaining ±1 differences (0.08 % of samples) are rounding inside RAD's transform. Matching them would need RAD's exact FFT, which isn't worth it for audio.
