# Bink 2 video: decoding specification

Status: draft 9 (2026-11-23). Draft 9 adds no decoding change: it states when slices are independent (§4.1) and how far the loop filter passes depend on each other (§4.10), so a decoder can run slices and filter bands in parallel. A parallel decoder built on it matches the sequential one on every frame of all five test files. Draft 8 (2026-11-23) Draft 8 covers the bottom macroblock row when the picture height is not a multiple of 32 (§4.2 SKIP, §4.7 hidden quadrants, §4.10 pass 3), checked on a 3840×2160 file (450 frames) and a 1280×720 file (3269 frames): with it the last rows match RAD on every frame of both, and the other test files decode bit-identically to draft 7. Draft 7 fixes two edge cases that only show when the picture size is not a multiple of 32 (§3.2 edge flag counts, §4.7 motion compensation at the picture edge), checked on a third `KB2n` file (720×1280, 651 frames, one keyframe): every frame matches RAD. Draft 6 was checked against two real `KB2n` files (115 and 402 frames, 2560×1440): every slice parses to its exact end, and with the loop filter of §4.10 and the corrections marked "draft 6" the decoded luma matches RAD's own output on every frame of both files (no pixel off by more than the rounding of RAD's RGB export, no drift). Chroma matches within the rounding of the colour conversion.

This document describes the Bink 2 container (`KB2*`) and video bitstream in enough detail to write a decoder. Numeric constants are in [TABLES.md](TABLES.md).

## Provenance and clean-room note

This is a functional description written by studying the unmerged LGPL-2.1+ FFmpeg `binkvideo2` decoder (`libavcodec/bink2.c`, `bink2f.c`, `bink2g.c`, and `libavformat/bink.c`, FFmpeg commit `f5b6ea3b65`, authors Konstantin Shishkov and Paul B Mahol). The document contains no code from those files. The algorithms are restated in prose and pseudocode, and some were simplified (see the DC prediction rule). The constant tables in TABLES.md are numeric data needed for interoperability, reformatted (VLC codes as bit strings in read order).

To keep the decoder implementation independent of the LGPL source:
- write it from this document and TABLES.md only, without reading the FFmpeg files;
- report anything unclear here as a spec question instead of looking at FFmpeg;
- answers come back as changes to this document.

This is a process recommendation, not legal advice.

Draft 6 adds behaviour the reference does not have: the loop filter (§4.10), the meaning of several `frame_flags` bits, the edge flag list layout (§3.3) and three corrections (inter DC range, two motion compensation details). These were established for interoperability by analysing the behaviour of RAD's own player (`binkplay.exe` from RAD Video Tools) and checked frame by frame against RAD's output. They are restated here as rules in prose and pseudocode; no code from RAD's player is reproduced.

Draft 8's rules for the bottom macroblock row were found by comparing decoder output with RAD's frames on the two files above, starting from the behaviour described in prose in the public descriptions of librempeg pull requests #217 and #218 (no code from those changes was read). The pass 3 rule of §4.10 was found by experiment only.

Where this document says "the reference" it means that FFmpeg decoder. It was never merged, its authors called it imperfect, and it has not been compared with RAD's decoder. Behaviour copied from it may contain its bugs. Those cases are marked **(unverified)**, and [Open questions](#open-questions) collects them.

---

## 1. Conventions

- All multi-byte integers are little-endian.
- `>>` on signed values is an arithmetic shift (rounds toward minus infinity).
- `clip8(v)` = `min(max(v, 0), 255)`.
- `median(a, b, c)` = the middle value of the three.
- `clamp(v, lo, hi)` = `min(max(v, lo), hi)`.
- `ceil_to(v, n)` rounds v up to a multiple of n.

### 1.1 Bit reader

The video bitstream is read **LSB-first**: the first bit is bit 0 of byte 0, then bit 1 of byte 0, …, bit 7, then bit 0 of byte 1, and so on.

- `bits(n)` reads n bits (n may be 0, which gives 0). The first bit read is bit 0 of the result.
- `bit()` = `bits(1)`.
- `ones(max)` counts 1-bits until a 0-bit is read. The 0 is consumed. If `max` 1-bits are read, it stops and returns `max` without reading a terminator.
- `zeros(max)` is the same with the roles of 0 and 1 swapped: it counts 0-bits until a 1-bit.
- `vlc(T)` reads one code from table T in TABLES.md. Bits are matched in read order and the symbol number is returned. An invalid code is a decoding error.
- `align()` skips to the next byte boundary.

## 2. Container

### 2.1 File header

| Offset | Size | Field |
|---|---|---|
| 0x00 | 3 | signature `KB2` |
| 0x03 | 1 | revision letter: `a`, `d`, `f`, `g`, `h`, `i`, `j`, `k` (known to the reference), `n` (seen in real files, see below) |
| 0x04 | 4 | file size − 8 |
| 0x08 | 4 | number of frames (sanity limit 1,000,000) |
| 0x0C | 4 | largest frame size in bytes |
| 0x10 | 4 | unknown |
| 0x14 | 4 | width (1 … 7680) |
| 0x18 | 4 | height (1 … 4800) |
| 0x1C | 4 | frame rate numerator (non-zero) |
| 0x20 | 4 | frame rate denominator (non-zero) |
| 0x24 | 4 | **video flags** (see below) |
| 0x28 | 4 | number of audio tracks N (≤ 256) |

Then, in order:
1. Revisions `i` and later: 4 bytes, unknown.
2. N × 4 bytes: maximum decoded audio size per track (not needed).
3. N × 4 bytes: per track, `u16 sample_rate` then `u16 audio flags`. In the audio flags, 0x2000 means stereo (mono otherwise) and 0x1000 means the DCT audio codec (RDFT otherwise). 0x4000 means "16-bit".
4. N × 4 bytes: audio track ids.
5. **Frame index:** one `u32` per frame. Bit 0 set means a keyframe; clearing bit 0 gives the absolute file offset of the frame. A frame ends where the next one starts; the last frame ends at the file size (header field + 8). The offsets must be strictly increasing.

**Revision `n`** (not known to the reference). Two real 2560×1440 files show the same container layout as `i`/`j`/`k`: the extra field at 0x2C is present (both files: 0x87E90500), and with no audio the frame index starts at 0x30 and ends exactly where frame 0 starts. Treat `l`, `m` and `n` like `k` (integer variant, §3.2 rule for `h` and later) until a difference shows up. Whether the video bitstream is unchanged is **unverified**; the observed slice tables look consistent (8 slices for `video_flags & 3 = 3`, increasing offsets inside the frame).

The frame index has `frames + 1` entries; the last one is the end of the last frame. Using the file size for that is equivalent.

Video flags:
- bit 20 (0x00100000): the video has an alpha plane.
- bits 0–1: slice count selector for revision `h` and later (§3.2).
- Other bits: unknown.

Some files are wrapped in 512-byte "SMUS" blocks: the file starts with the 4 bytes `SMUS`. Step forward in 512-byte units until the first 3 bytes at that position are a Bink signature; that position is the start of the Bink data, and every file offset in the header and index is relative to it. The data from there on is contiguous. The reference only recognises Bink 1 (`BIK`) signatures inside SMUS, so SMUS-wrapped Bink 2 may not exist in practice.

### 2.2 Frame packets

Each frame (from the index) holds, in order:
1. For each audio track: `u32 size`, then `size` bytes of audio. When `size ≥ 4`, the first `u32` of the audio data is the decoded size in bytes. Audio is a separate codec ("Bink Audio", DCT/RDFT), specified in [AUDIO.md](AUDIO.md).
2. The rest of the frame is the **video packet**.

A frame's keyframe status comes from the index bit, not from the video packet.

## 3. Video packet

### 3.1 Overview

- Output: 8-bit planar YUV 4:2:0. Chroma planes are half size in both directions. With alpha there is also a full-resolution alpha plane.
- Coding happens on a frame whose width and height are rounded up to multiples of 32. The visible picture is the top-left `width × height`.
- **Variants:**
  - revisions `a`, `d`, `f` use the **float variant** (§5);
  - revisions `g` and later use the **integer variant** (§4).
- A **macroblock (MB)** covers 32×32 luma pixels, 16×16 of each chroma plane, and 32×32 alpha. Within a 32×32 luma (or alpha) area there are sixteen 8×8 blocks, numbered in Z-order with `ZORDER` (TABLES.md): block k sits at row `ZORDER[k] >> 2`, column `ZORDER[k] & 3`. In chroma, the four 8×8 blocks are in raster order (0 top-left, 1 top-right, 2 bottom-left, 3 bottom-right).
- **Plane coding order** in every MB: Y, C1, C2, then A (only if alpha is present). The reference writes C1 into the Cr (V) plane and C2 into the Cb (U) plane **(unverified; check on a real file that colours are not swapped)**.
- Frames are I (keyframe) or P. P frames predict from the previous decoded frame only.

### 3.2 Packet header and slices

1. `frame_flags` = the `u32` at byte 0 of the video packet.
2. Slice count S and slice boundaries, with `H32 = ceil_to(height, 32)`:
   - revision ≤ `f`: S = 2, `boundary[0] = (height / 2 + 16) & ~31` (integer division);
   - revision `g`: if H32 < 128, S = 1; otherwise as for ≤ `f`;
   - revision ≥ `h`: S = `[2, 3, 4, 8][video_flags & 3]`. Then:
     ```
     start = 0; end = H32 + 32*S - 1
     for i in 0 .. S-2:
         start += ((end - start) / (S - i)) & ~31     // integer division
         end   -= 32
         boundary[i] = start
     ```
   - always: `boundary[S-1] = H32`.
   Slice i covers luma rows `[boundary[i-1], boundary[i])`, with `boundary[-1] = 0`.
3. Bytes 4 … 4·(S−1)+3: S−1 `u32` values. `slice_end[i]` is the byte offset, from the start of the video packet, where slice i+1 starts. `slice_end[S-1]` = the packet length.
4. The bit reader starts at byte `4·S`.
5. If `frame_flags` bit 16 (0x10000) is set:
   - if bit 15 (0x8000) is clear: read the **row flags** R with `count = ceil_to(height, 16) / 8 − 1` (§3.3);
   - then, if bit 14 (0x4000) is clear: read the **column flags** C with `count = ceil_to(width, 32) / 8 − 1`.

   *(draft 7: the column count uses 32, not 16; on a 720-wide file it must be 91, and 89 desynchronises slice 0 of every frame. The row count uses 16: on a 720-high file it must be 89, and 91 desynchronises every frame. 2160-high files need 269, not 271. The two roundings agree whenever the size is a multiple of 32, and at 1080.)*
   If bit 16 is clear, nothing is read and every flag of a list that is in use (bit 15 resp. bit 14 clear) is 0 (that case does not occur in the test files: there, frames without lists always have bits 14 and 15 set). The flags switch individual edges of the loop filter off (§4.10). *(draft 6)*

   Other `frame_flags` bits used by this document *(draft 6)*:

   | bits | meaning |
   |---|---|
   | 0–7 | `smooth_thr = (frame_flags & 0xFF) · 8`, the DC smoothing threshold (§4.10) |
   | 14 | set: no vertical-edge filtering in this frame (§4.10) |
   | 15 | set: no horizontal-edge filtering in this frame (§4.10) |
   | 16 | edge flag lists are coded |
   | 18 | inter DC range ±2047 instead of ±1023 (§4.3); also used by `read_luma_cbp` (§4.3) |
6. For each slice i: decode its MB rows (§4 or §5) starting at the current bit position, which is not byte-aligned for slice 0. Then `align()` and move the reader to byte `slice_end[i]`. The reference only warns about over- or under-reads.

### 3.3 Edge flag lists

This produces a list of `count` flags B[0] … B[count−1] *(draft 6: corrected, see the note at the end)*.

```
if bit() == 0:                         // raw form
    B[0 .. count-1] = next count bits (one per flag, in order)
    done
pos = 0; left = count; mode = NONE; flag = 0
while left > 0:
    if bit() == 0:                     // literal group
        flag = (mode == TOGGLE) ? 1 - flag : bit()
        mode = LITERAL
        n = min(left, 5)
        B[pos] = flag
        B[pos+1 .. pos+n-1] = next n-1 bits
        pos += n; left -= n
    else:                              // run
        w = (left < 4) ? 2 : (left < 16) ? 4 : 5
        if mode == TOGGLE:
            flag = 1 - flag; run = w + 1
        else:
            flag = bit();    run = w + 2
        run = min(run, left); left -= run
        if left > 0:
            extra = min(bits(w), left); left -= extra; run += extra
            mode = (extra == 2^w - 1) ? EXPLICIT : TOGGLE
        B[pos .. pos+run-1] = flag; pos += run
```
`mode` starts as "not TOGGLE". After a literal group the next flag value is read explicitly. After a run that ended early (extra < max), the next segment's first flag is the opposite of this run's flag. A coded `extra` larger than what is left is not an error: it is cut to `left` and the list ends.

Note: the reference writes the coded form from position 1 (with an extra leading 0) but the raw form from position 0. RAD's decoder writes both from position 0, as above; with the reference's layout the flags of the coded form are off by one edge. Only the layout changed; the bits read are the same.

## 4. Integer variant (revisions `g`+)

### 4.1 Per-slice and per-row state

Each MB column `m` = x/32 keeps the following for two MB rows, "current" and "previous" (the row above). The roles swap at the end of each MB row.
- `intra_q`, `inter_q`: small integers;
- `dc[plane][16]`: DC values (planes Y, C1, C2, A);
- `type`: block type;
- `mv[4]`: four motion vectors (x, y).

Resets:
- at slice start: previous-row `q` and `mv` are set to 0 (DC values are not reset);
- at the start of every MB row: current-row `q` and `mv` are set to 0. Running coded-block patterns (`cbp`) are set to 0, one per (plane, intra/inter), so eight in total. The block-type list `LRU` is set to `[MOTION, RESIDUE, SKIP, INTRA]`.

Neighbours of MB (m, row): left = (m−1, current), top = (m, previous), top-left = (m−1, previous), top-right = (m+1, previous). Edge conditions:
- `TOP_EDGE`: this MB row is the first row of the slice;
- `LEFT_EDGE`: m = 0.
Neighbours across an edge are unavailable.

**Slice independence (draft 9).** In the integer variant a slice can be decoded without the slices before it: its bits start at a known byte (§3.2), it reads only the reference frame and its own MB rows, it writes only its own rows (slice boundaries are multiples of 32), and the loop filter only runs after all slices (§4.10). The DC values left over from the previous slice are never observed. In a slice's first row the MBs above are unavailable (`TOP_EDGE`), and every DC that a later INTRA MB can predict from is either written by its own MB or recomputed by the neighbour refresh (§4.8), which covers all planes. Slices can therefore be decoded in parallel, each starting from fresh row state, with the same result. In the float variant this holds only without alpha: see §5.1.

Block types: `INTRA` = 0, `SKIP` = 1, `MOTION` = 2, `RESIDUE` = 3.

### 4.2 Macroblock

```
type = keyframe ? INTRA : read_type()
store type in current row
```
`read_type`: `n = zeros(3)`, then:

| n | result | then swap |
|---|---|---|
| 0 | LRU[0] | none |
| 1 | LRU[1] | LRU[0] ↔ LRU[1] |
| 2 | LRU[3] | LRU[2] ↔ LRU[3] |
| 3 | LRU[2] | LRU[1] ↔ LRU[2] |

`pred_q(kind)` (kind = intra or inter):
- both edges: 16;
- `TOP_EDGE` only: the left MB's value;
- `LEFT_EDGE` only: the top MB's value;
- otherwise: median(top, left, top-left).

`read_dq` (draft 1–3 wrongly wrote the second test as a separate `if`, so 3 + 1 also took the 5-bit branch; that desynchronised almost every content slice):
```
n = zeros(4)
if n == 3:   n += bit()            // gives 3 or 4
elif n == 4: n += bits(5) + 1      // only when zeros() returned 4
if n != 0 and bit(): n = -n
```

The four MB types are decoded as follows:

**INTRA:**
1. Refresh neighbour DCs (§4.8).
2. Store motion vectors predicted with zero deltas, using the four-vector rule (§4.6). Intra MBs therefore carry predicted vectors, not zero.
3. `inter_q = pred_q(inter)`.
4. `intra_q = pred_q(intra) + read_dq()`. It must be in 0 … 36.
5. Decode intra Y, intra C1, intra C2, and intra A if alpha is present (§4.3), all with `q = intra_q`.

**SKIP:**
1. `intra_q = pred_q(intra)`, `inter_q = pred_q(inter)`.
2. Copy the co-located 32×32 luma (and alpha) area and the 16×16 area of each chroma plane from the reference frame. Exception *(draft 8)*: a **hidden quadrant** (§4.7) is not copied from its own position but motion-compensated with the vector (0, 0) adjusted as in §4.7, i.e. it is copied from the last 16 luma rows (8 chroma rows) above `H16`.
3. No bits are read. Motion vectors stay 0.

**MOTION:**
1. Both q values from `pred_q`.
2. Read the motion vectors (§4.5) and predict them (§4.6).
3. Motion-compensate Y, C1, C2 and A (§4.7).

**RESIDUE:**
1. `intra_q = pred_q(intra)`.
2. Read the motion vectors and predict them.
3. `inter_q = pred_q(inter) + read_dq()`. It must be in 0 … 36.
4. Motion-compensate all planes.
5. Decode inter Y (§4.3).
6. Read one bit. If it is 1, decode inter C1 then inter C2. If it is 0, set the C1 and C2 running inter cbp to 0.
7. Decode inter A if alpha is present.

### 4.3 Coding a plane of one MB

Each (plane, intra/inter) pair has its own running `cbp`, which is updated to the newly decoded pattern.

**Luma or alpha (16 blocks):**
1. `cbp = read_luma_cbp(running)`.
2. `dc = read_dc(16, q, range, ...)`.
3. For each group g = 0 … 3: decode the AC coefficients of blocks 4g … 4g+3 (§4.4), using the 4 cbp bits `4g … 4g+3`. For each block k in the group: set coefficient 0 to `dc[k]·8 + 32`, inverse-transform it, and write it at the block's Z-order position.

**Chroma (4 blocks):**
1. `cbp = read_chroma_cbp(running)`.
2. `dc = read_dc(4, ...)`.
3. Decode one AC group with cbp bits 0–3.
4. For each block k: coefficient 0 = `dc[k]·8 + 32`, inverse-transform, write it in raster position k.

Intra vs inter:

| | intra | inter |
|---|---|---|
| DC range | 0 … 2047 | −1023 … 1023, or −2047 … 2047 if `frame_flags` bit 18 (0x40000) is set *(draft 6)* |
| DC prediction | uses neighbour MBs (§4.8) | as if both edges were set (only within this MB, starting from 0) |
| matrices | luma: `G luma intra`; chroma: `G chroma intra` | `G inter` for both |
| output | overwrite pixels (put) | add to the motion-compensated pixels |

**`read_luma_cbp(prev)`:**
```
k = popcount(prev & 0xFFFF); invert = false
if k > 7: k = 16 - k; invert = true
cbp = 0
if bit() == 0:
    if k < 4:
        for nibble i in 0..3: if bit() == 0: cbp |= bits(4) << 4i
    else:
        cbp = bits(16)
if invert: cbp ^= 0xFFFF
if (frame_flags & 0x40000) == 0 or cbp != 0:
    if bit(): cbp |= cbp << 16          // "second run table" flag
```

**`read_chroma_cbp(prev)`:**
```
cbp = (prev & 0xF0000) | ((popcount(prev & 0xF) >= 2) ? 0xF : 0)
if bit() == 0:
    cbp = bits(4)
    if bit(): cbp |= cbp << 16
```
When the first bit is 1, the upper bits (the run-table flag) are inherited from `prev`.

**`read_dc(n, q, lo, hi, edges)`:**
```
scale = DC_SCALE[max(q, 8)]
r[0..n-1] = 0
if bit():
    for i in 0..n-1:
        c = ones(12)
        if c > 3: c = 2^(c-3) + bits(c-3) + 2
        if c != 0 and bit(): c = -c
        r[i] = (c * scale + 512) >> 10
predict (§4.8) and clamp each dc[i] to [lo, hi]
```

### 4.4 AC coefficients

One call handles a group of 4 blocks, given `p` = the cbp shifted so that the group's 4 bits are at bits 0–3.
- Clear all four 64-entry coefficient arrays (signed 16-bit).
- If `p & 0xF == 0`, stop; no bits are read.
- Run table: `G_SKIP1` if `p` has any bit set in 16–31, else `G_SKIP0`. For the patterns the cbp readers produce, this is just the "second run table" flag.

For each block b = 0 … 3 with bit b of `p` set:
```
pos = 1; pending = 0
while pos < 64:
    pending -= 1
    if pending < 1:
        s = vlc(run table)
        pending = (s == 13) ? 7 : 0
        adv = s for s <= 10; bits(6) for s == 11; 64 for s == 12; 0 for s == 13
        pos += adv
        if pos >= 64: break
    v = ones(12) + 1
    if v > 3: v = bits(v-3) + 2^(v-3) + 2
    if bit(): v = -v
    i = SCAN_G[pos]
    coef[i] = (v * QM[q & 3][i] * 2^(q >> 2) + 64) >> 7      // stored as int16
```
The reference computes the product in 32-bit signed arithmetic; it only overflows on streams with absurd levels at high q. Any wrapping behaviour there is acceptable.
```
    pos += 1
```

### 4.5 Motion vector syntax

```
n = bit() ? 1 : 4
for component c in (x, y):
    for j in 0..n-1:
        s = vlc(G_MV)
        if s <= 7:      d = s
        elif s <= 14:   d = s - 15
        else:           w = zeros(12) + 4
                        u = bits(w) + 2^w - 1
                        d = (u odd) ? -(u >> 1) - 1 : (u >> 1)
        delta[j].c = d
```
All x components come first, then all y components.

### 4.6 Motion vector prediction

Vectors are numbered by quadrant: 0 top-left, 1 top-right, 2 bottom-left, 3 bottom-right. `L`, `T` and `TL` are the stored vectors of the left, top and top-left MBs. Each formula applies to x and y separately. SKIP MBs store zero vectors; INTRA MBs store predicted vectors (§4.2).

**One vector (n = 1):** all four stored vectors are `delta[0] + P`, where P is:

| edges | P |
|---|---|
| top and left | 0 |
| top only | median(L0, L1, L3) |
| left only | median(T0, T2, T3) |
| none | median(TL3, T2, L1) |

**Four vectors** (n = 4, and INTRA with zero deltas); `c` is the result and `d` the delta:

| edges | formulas |
|---|---|
| none | c0 = d0 + median(T2, TL3, L1); c1 = d1 + median(T2, T3, c0); c2 = d2 + median(L1, L3, c0); c3 = d3 + median(c0, c1, c2) |
| left only | c0 = d0 + median(T0, T2, T3); c1 = d1 + median(T2, T3, c0); c2 = d2 + median(T2, c0, c1); c3 = d3 + median(c0, c1, c2) |
| top only | c0 = d0 + median(L0, L1, L3); c2 = d2 + median(L1, L3, c0); c1 = d1 + median(L1, c0, c2); c3 = d3 + median(c0, c1, c2) |
| top and left | c0 = d0; c1 = d1 + d0; c2 = d2 + d0; c3 = d3 + median(c0, c1, c2) |

### 4.7 Motion compensation

Vector j moves one quadrant:
- luma (and alpha): a 16×16 block at `(X, Y) = (x + 16·(j&1), y + 16·(j>>1))`;
- chroma: an 8×8 block at `(x/2 + 8·(j&1), y/2 + 8·(j>>1))`.

The same vector values serve both:
- **Luma:** half-pel units. Source = `(X + (vx >> 1), Y + (vy >> 1))`, fraction = `(vx & 1, vy & 1)`.
- **Chroma:** quarter-pel units in the chroma plane. Source = `(X + (vx >> 2), Y + (vy >> 2))`, fraction = `(vx & 3, vy & 3)`.

Every block is predicted normally, wherever its vector points *(draft 7)*. The reference frame is the whole **coded** frame (32-aligned, including the decoded pixels right of and below the visible picture), and any sample outside it takes the value of the nearest coded sample (edge replication). There is no "out of picture" case. (The reference skips blocks whose source top-left is outside the *visible* picture, including zero-vector blocks in the padding. RAD doesn't: on a 720×1280 file that difference drifts the right edge within 13 frames, while predicting normally matches RAD on all 651 frames.)

**Hidden quadrants** *(draft 8)*. Let `H16 = ceil_to(height, 16)`. When the height is not a multiple of 32, the bottom MB row reaches 16 rows past `H16`, and its two bottom quadrants (j = 2, 3) lie entirely at or below `H16`. Call a quadrant **hidden** if its luma top `Y ≥ H16`. For a hidden quadrant, the vertical vector component is adjusted before motion compensation:
```
iy  = vy >> 1;  fy = vy & 1
lim = H16 - 16 - 3·fy - Y          // keeps the source block (plus the 3 rows the half-pel filter reads below it) above H16
if iy > lim:  vy = 2·lim + fy      // the fraction is kept
```
The adjusted `vy` is used for the luma block and for the co-located chroma blocks (whose fraction and offset come from it as above), and for alpha **(unverified: no test file with alpha)**. `vx` is never adjusted, and quadrants that are not hidden are never adjusted. The adjustment only affects the prediction: motion vector prediction (§4.6) and the loop filter (§4.10) use the vectors as decoded. SKIP MBs treat their hidden quadrants the same way, with vector (0, 0) (§4.2). Hidden pixels are never shown, but they reach the picture through DC prediction, neighbour refresh and DC smoothing of the last visible block row. (Without the three draft 8 rules, a 3840×2160 file goes wrong in its last 4 rows from the first frame with horizontal edge filtering on, and stays wrong; without only the SKIP rule, 9 of its 450 frames still differ.)

**Luma filter.** For six samples `s[-2] … s[3]` around the half-pel position between `s[0]` and `s[1]`:
`h = clip8(( ((19·(s0 + s1)) >> 1) − 2·(s−1 + s2) + ((s−2 + s3) >> 1) + 8 ) >> 4)`

By fraction:
- (0,0): copy;
- (1,0): horizontal filter;
- (0,1): vertical filter;
- (1,1): run the horizontal filter on the 21 source rows −2 … +18, then the vertical filter on that intermediate. The intermediate values are **not** clipped: they are `(… + 8) >> 4` as in the formula, without `clip8`, and may be below 0 or above 255. Only the final vertical result is clipped. *(draft 6; the reference clips the intermediate)*

**Chroma filter.** Between samples a and b, at fraction f:
- f = 1: `(6a + 2b + 4) >> 3`;
- f = 2: `(a + b + 1) >> 1`;
- f = 3: `(2a + 6b + 4) >> 3`.

If fy = 0, filter horizontally only (fx = 0 means copy). If fx = 0, filter vertically only. Otherwise interpolate in one step, with no rounding in between *(draft 6; the reference rounds after the horizontal pass)*:
```
wa(f) = 8 - 2f;  wb(f) = 2f
h(r)  = wa(fx)·p(x, r) + wb(fx)·p(x+1, r)          // for rows r = y and y+1
out   = (wa(fy)·h(y) + wb(fy)·h(y+1) + 32) >> 6
```

The filters and blocks can read past the 32-aligned frame. A margin of 32 replicated pixels around each reference plane covers the vectors seen in the test files; a decoder may clamp coordinates instead, which is the same thing. *(draft 7)*

### 4.8 DC prediction and neighbour refresh

**Prediction rule** (both variants, luma and chroma). Go through the blocks in coding order (Z-order for luma, raster for chroma). For each block take its DC neighbours L (left), T (above) and TL (above-left). They can be in this MB or in the left, top or top-left MB, using those MBs' stored `dc` of the same plane. A neighbour in another MB is available unless an edge excludes it, and for inter blocks all outside neighbours count as unavailable. Then:
- L and T both available (TL then always is): `P = clamp(L + T − TL, min(L, T, TL), max(L, T, TL))`;
- only L: P = L;
- only T: P = T;
- neither: P = 1024 for intra, 0 for inter.

The result is `dc = clamp(P + r, lo, hi)`, stored in the current row's `dc[plane]`. Inter blocks also store their (inter) DCs there.

**Neighbour refresh** (before decoding an INTRA MB). For each of these neighbours that exists and whose stored type is **not** INTRA:
- top-left, if neither edge;
- left, if not `LEFT_EDGE`;
- top-right, if `LEFT_EDGE` and not `TOP_EDGE`;
- top, if not `TOP_EDGE`;

recompute that MB's stored DCs for Y, C1, C2 (and A) from the **already reconstructed pixels of the current frame**. For each 8×8 block, and for each of its 8 columns:
```
avg(a, b) = (a + b + 1) >> 1
col = avg( avg(avg(p0,p1), avg(p2,p3)), avg(avg(p4,p5), avg(p6,p7)) )
```
where p0 … p7 are the column's pixels top to bottom. The block's DC is the sum of `col` over its 8 columns. Luma index k uses the block at `ZORDER[k]`.

The top-right refresh does not affect any prediction in the current MB. It is listed only for exactness.

### 4.9 Inverse transform (integer)

The 8×8 coefficients are in a 64-entry array `a` (index = 8·row + col of the array). Helpers:
`mA(v) = v + (v>>2)`, `mB(v) = v>>1`, `mC(v) = v − (v>>2) − (v>>4)`, `mD(v) = v + (v>>2) − (v>>4)`, `mE(v) = v>>2`.

**1-D transform** of x0 … x7 with output shift `sh`:
```
e0 = x0 + x4;          e1 = x0 - x4
p  = mA(x2) + mB(x6);  q  = mB(x2) - mA(x6)
E0 = e0 + p; E3 = e0 - p; E1 = e1 + q; E2 = e1 - q
g  = x3 + x5;  h = x3 - x5
u  = x1 + g;   w = x1 - g
r  = h + x7;   t = h - x7
o0 = mD(u) + mE(r);    o3 = mE(u) - mD(r)
o1 = w + mC(t);        o2 = mC(w) - t
y0 = E0+o0  y1 = E1+o1  y2 = E2+o2  y3 = E3+o3
y4 = E3-o3  y5 = E2-o2  y6 = E1-o1  y7 = E0-o0      (each >> sh)
```
Work in 32-bit integers and store the results back as signed 16-bit after each pass.

Steps:
1. For each array column c: transform `a[c], a[8+c], …, a[56+c]` with `sh = 0`.
2. For each array row r: transform `a[8r … 8r+7]` with `sh = 6`.
3. Pixel at image (row i, column j) = `a[8j + i]`. The array is **transposed** relative to the image.
4. Put: `clip8(value)`. Add: `clip8(pixel + value)`.

### 4.10 Loop filter *(draft 6)*

After all slices of a frame are decoded, the frame is filtered **in place**, and the filtered frame is both the output and the reference for the next frame. The filter has three passes per plane, run in this order over the whole plane: DC smoothing, vertical edges, horizontal edges. The planes are Y, C1 and C2. For alpha, only DC smoothing runs, and not at all when `frame_flags` bit 19 (0x80000) is set **(unverified: no test file with alpha)**. The filter works on the whole coded (32-aligned) frame, on 8×8 blocks of each plane (a chroma block is 8×8 chroma pixels).

**Block info.** While decoding, record for every 8×8 block of each plane:
- `intra`: the block belongs to an INTRA MB;
- `s`, for intra blocks only: 3 if the block's cbp bit is 0 (no AC coefficients coded); otherwise, with n = the number of AC coefficient values read for the block in §4.4 (each level counts once, the DC does not), `s = (n < 8) + (n < 4) + (n == 0)`, so 0, 1, 2 or 3;
- `dc`: the block's final DC value from §4.3 (after prediction and clamping), intra blocks only;
- `mv`: the motion vector of the block's 16×16 luma quadrant (§4.7; the same vector for the co-located chroma blocks), in the coded units (no scaling). SKIP and INTRA MBs have mv (0, 0).

Blocks of SKIP, MOTION and RESIDUE MBs are not intra. For non-intra blocks `s = 0` in the edge rules below.

It helps to pack the info of a block in a 16-bit word, because the smoothing threshold test below compares whole words:
`word = (intra << 15) | (s << 13) | (dc & 0x1FFF)` for intra blocks, and 0 for the others. Read `word` as a signed 16-bit value where a sign is needed.

**Pass 1: DC smoothing.** For every intra block with `s = 3` (no AC coefficients), replace its 8×8 pixels by a smooth surface through the DCs of its 3×3 neighbourhood:
```
c = word of this block (signed 16-bit)
accept(n) = | wrap16(n - c) | < smooth_thr       // n: neighbour word, signed; wrap16: to signed 16 bits
g[1][1] = dc of this block
L, R, T, B (g[1][0], g[1][2], g[0][1], g[2][1]): neighbour's dc if accept(neighbour word) else g[1][1]
corners g[0][0], g[2][0]: neighbour's dc if accepted else g[1][0]
corners g[0][2], g[2][2]: neighbour's dc if accepted else g[1][2]
```
Because the test compares whole words, only neighbours that are intra with `s = 3` and a DC within `smooth_thr` of this block's DC are used. With `smooth_thr = 0` nothing is accepted and the block becomes flat. Neighbours outside the plane: use this block itself (equivalent to "not accepted") **(observed to match; RAD's exact handling at the left and right plane edges unverified)**.

Then, with these weights (`mulhi(d, k) = (d · k) >> 16`, arithmetic shifts):
```
K[0](d) = mulhi(d, 26624)   K[1](d) = mulhi(d, 18432)   K[2](d) = mulhi(d, 12288)   K[3](d) = mulhi(d, 7168)
K[4](d) = d >> 4            K[5](d) = d >> 5            K[6](d) = d >> 7            K[7](d) = 0

for r in 0, 1, 2:                 // grid row: above, this, below
    for i in 0 .. 7:
        H[r][i] = g[r][1] + 4 + K[i](g[r][0] - g[r][1]) + K[7-i](g[r][2] - g[r][1])
for j in 0 .. 7:                  // pixel row
    for i in 0 .. 7:              // pixel column
        v = H[1][i] + K[j](H[0][i] - H[1][i]) + K[7-j](H[2][i] - H[1][i])
        pixel(i, j) = clip8(v >> 3)
```

**Edge strength.** For two adjacent blocks A (left or above) and B (right or below):
```
sA = A.intra ? A.s : 0;  sB = B.intra ? B.s : 0
if not A.intra and not B.intra and (|A.mv.x - B.mv.x| >= 2 or |A.mv.y - B.mv.y| >= 2):
    sA |= 2; sB |= 2
skip the edge if sA == sB and (sA == 0 or sA == 3)
```

**Edge filter.** Across the edge, take p1, p0 on A's side (p0 next to the edge) and q0, q1 on B's side, all from the pixels before this edge is filtered:
```
d = q0 - p0;  a = (d + 2) >> 2;  b = (d + 4) >> 3
if sA >= 1: p0' = clip8(p0 + a)
if sA >= 2: p1' = clip8(p1 + b)
if sB >= 1: q0' = clip8(q0 - a)
if sB >= 2: q1' = clip8(q1 - b)
```
(Pixels not listed keep their value.) There is no threshold on the step size.

**Pass 2: vertical edges** (skipped if `frame_flags` bit 14 is set). For every block row and every block column bx ≥ 1, filter the edge between blocks bx−1 (A) and bx (B) on all 8 pixel rows of the block, unless the column flag says off: the edge at plane x = 8·bx uses `C[bx − 1]` in luma (and alpha) and `C[2·bx − 1]` in chroma (the flag of the co-located luma edge). A flag value of 1 switches the edge off.

**Pass 3: horizontal edges** (skipped if `frame_flags` bit 15 is set). Same, for every block row by ≥ 1 between blocks above (A) and below (B), on all 8 pixel columns, using `R[by − 1]` in luma and `R[2·by − 1]` in chroma. Edges at or below `H16` (§4.7) are **never** filtered *(draft 8)*: in luma the edges with `8·by ≥ H16`, in chroma those with `8·by ≥ H16 / 2`. These are exactly the edges whose index would be past the end of the row flag list (§3.2 counts it from `H16`). Filtering them, as a flag of 0 would, changes the last 2 visible rows. (When a frame has no lists, bit 16 clear, the same rule is assumed **(unverified: does not occur in the test files)**.)

Each edge reads the pixels as left by the edges filtered before it in the same pass order (block rows top to bottom, columns left to right). Edges of the same pass never share pixels, so this order only matters between the passes. *(draft 9)* More precisely: pass 1 (DC smoothing) depends only on the block information, not on pixels written by the other passes. Pass 2 (vertical edges) stays inside one block row. So passes 1 and 2 can run together per block row, and the plane can be split into bands of block rows for them. Pass 3 at the edge above block row `by` touches pixel rows 8·by−2 … 8·by+1, so it can run in bands as well, once passes 1 and 2 are done for the whole plane, as long as each edge lies inside one band (for example bands cut a few rows above an edge). The three planes are independent of each other.

**Float variant.** Only checked on integer-variant files. Whether revisions ≤ `f` use the same filter is **unverified**.

## 5. Float variant (revisions ≤ `f`)

Same structure as §4 (MBs, slices, Z-order, edges, DC prediction rule, neighbour refresh, MC filters and vector units), with these differences.

### 5.1 State and macroblock

- Block type: `bits(2)` with the numbering of §4.1, no LRU. On keyframes no type bits are read and every MB is INTRA, as in §4.2.
- No spatial q prediction. Each (plane, intra/inter) pair has a running q, set to 8 at the start of every MB row. Each coded plane reads `dq = vlc(F_DQ)`, and if `dq != 0 and bit()`, `dq = -dq`. Then `q = running + dq`, which must be in 0 … 15 and becomes the new running value.
- Only previous-row `mv` is reset at slice start. Per row, current `mv` and the 8 running cbps are reset to 0.
- Neighbour refresh covers Y, C1 and C2 only (no alpha).
  *(draft 9)* So with alpha, slices are **not** independent here: an INTRA MB in a slice's first row can predict its alpha DC from a left neighbour (SKIP or MOTION) whose alpha DC was never written in this slice and still holds a value from the previous slice (DC values are not reset at slice start, §4.1). Decode such files sequentially, carrying the row state from slice to slice. What RAD's decoder stores there is **unverified** (no test file with alpha).
- MB types:
  - **INTRA:** refresh neighbour DCs, store the four-vector prediction with zero deltas (§5.4), then intra Y, C1, C2, A.
  - **SKIP:** copy, as in §4.
  - **MOTION:** read the vectors (§5.4), predict them, MC Y, C1, C2. The reference does **not** motion-compensate alpha here, leaving that alpha area undefined **(unverified)**. Copying the co-located reference alpha is a reasonable stand-in.
  - **RESIDUE:** vectors and prediction, MC Y, C1, C2 (again no alpha MC in the reference), then inter Y, C1, C2, A. There is no chroma-present bit.
- Per coded plane, the order is: cbp, dq, DC, AC groups.

### 5.2 Coded-block patterns

The upper half of the result (bits 16 + k) selects, per block k, the second level and run tables.

**Luma:**
```
if bit():
    if bit(): return prev
    lo = prev & 0xFFFF
else:
    lo = 0; nib = (prev >> 4) & 0xF
    for i in 0..3:
        if bit() == 0: nib = bits(4)
        lo |= nib << 4i                  // nib carries over if not re-read
hi = 0; h = (prev >> 20) & 0xF
for i in 0..3:
    m = (lo >> 4i) & 0xF
    if popcount(m) == 0: h = 0
    elif popcount(m) == 1 or bit() == 0:
        h = 0
        for each set bit of m, lowest first: if bit(): h |= that bit's mask (1, 2, 4 or 8)
    // else (2+ bits set and the bit read was 1): h keeps its previous value
    h &= m
    hi |= h << 4i
return lo | hi << 16
```
**Chroma:**
```
if bit():
    if bit(): return prev
    lo = prev & 0xF
else:
    lo = bits(4)
if popcount(lo) == 0: return lo
if popcount(lo) >= 2:
    h = (prev >> 16) & lo
    if bit(): return lo | h << 16
h = 0
for each set bit of lo, lowest first: if bit(): h |= that bit's mask (1, 2, 4 or 8)
return lo | h << 16
```
The initial `nib = (prev >> 4) & 0xF` and `h = (prev >> 20) & 0xF` really take nibble 1 of `prev`, not nibble 0. That is what the reference does; it may be a reference bug **(unverified)**.

### 5.3 DC and AC

**DC:**
```
w = bits(3); if w == 7: w += bits(2)
r[0..n-1] = 0
if w > 0:
    for each group of 4 (i = 0,4,8,12 up to n):
        r[i..i+3] = four values of bits(w)
        for each of those four that is non-zero: if bit(): negate it
if intra and both edges:
    e = F_DC_BITS[q] - 1 + w
    if e < 10:
        v = bits(10 - e)
        if v != 0: v <<= w; if bit(): v = -v
        r[0] += v
r[i] = r[i] * F_DC_Q[q]          // float32
```
Prediction and clamping follow §4.8 in float32, with the same ranges.

Coefficient 0 of each block = `dc[k] · 0.125`.

**AC** for block k with its cbp bit set. Tables: `F_LEVEL1`/`F_SKIP1` if cbp bit 16+k is set, else `F_LEVEL0`/`F_SKIP0`. Scan: `SCAN_F_LUMA` for luma and alpha, `SCAN_F_CHROMA` for chroma. Matrix (TABLES.md):
- luma intra: `F luma intra`;
- luma inter: `F luma inter`;
- chroma (intra and inter): `F chroma`;
- alpha uses the luma matrices.
```
pos = 1; pending = 0
loop:
    s = vlc(level table)
    if s == 0: v = 0
    else:
        v = s if s <= 3 else bits(s-3) + 2^(s-3) + 2
        if bit(): v = -v
    i = SCAN[pos]
    coef[i] = (v * F_AC_Q[q]) * M[i]           // float32, in this order
    if pos > 62: stop
    pos += 1; pending -= 1
    if pending < 1:
        s = vlc(run table); pending = (s == 13) ? 7 : 0
        pos += s for s <= 10; bits(6) for 11; 62 for 12; 0 for 13
    if pos >= 64: stop
```

### 5.4 Motion vectors

Always four vectors:
```
for component c in (x, y):
    w = bits(3); if w == 7: w += bits(2)
    if w > 0:
        d[0..3].c = four values of bits(w)
        for each non-zero one: if bit(): negate
    if both edges:
        o = bits(5) * 16
        if o != 0 and bit(): o = -o
        add o to d[0..3].c
```
The prediction matches the integer four-vector table (§4.6), except:
- no edges: `c2 = d2 + median(T2, c0, c1)` (not `median(L1, L3, c0)`) **(unverified)**;
- both edges: `cj = dj`, no prediction at all.

### 5.5 Inverse transform (float)

Use float32 arithmetic throughout; the order of operations matters for bit-exact output.

**1-D transform** of x0 … x7:
```
a = x2 + x6;  b = (x2 - x6) * 1.4142135 - a
c = x0 + x4;  d = x0 - x4
E0 = c + a; E3 = c - a; E1 = d + b; E2 = d - b
g = x3 + x5;  h = x3 - x5
k = x1 + x7;  m = x1 - x7
s  = k + g
z1 = (k - g) * 1.4142135
z2 = (m - h) * 1.847759
o1 = h * 2.613126 + z2 - s
o2 = z1 - o1
o3 = m * 1.0823922 - z2 + o2
y0 = E0+s  y1 = E1+o1  y2 = E2+o2  y3 = E3-o3
y4 = E3+o3 y5 = E2-o2  y6 = E1-o1  y7 = E0-s
```
Steps:
1. Add 512.5 to `a[0]`.
2. Transform each array column (stride 8), then each array row.
3. Pixel at (row r, column c) = `a[8r + c]`. Here the array is **not** transposed.
4. Put: `clip8(trunc(a − 512.0))`. Add: `clip8(trunc(pixel + a − 512.0))`. `trunc` converts to integer toward zero; the 512.5 bias makes it round to nearest.

## 6. Open questions

Things the reference does not settle, to resolve by testing against real files and, where possible, RAD's own player output:

1. **Deblocking filter: closed (draft 6).** It is an in-loop filter with DC smoothing and edge filtering, see §4.10. The edge flags of §3.3 switch single edges off. Still open: the float variant, the alpha plane, and the DC smoothing rule at the left and right plane edges.
2. **Out-of-picture motion vectors (§4.7): closed (draft 7).** Predict normally from the coded frame with edge replication.
3. **Reference padding: closed (draft 7).** See §4.7.
4. **Plane order: closed.** C1 is Cr (V) and C2 is Cb (U); checked against RAD's output (swapping gives a blue/cyan cast).
5. **Float variant:** the alpha MC omission and the `c2` prediction difference (§5.1, §5.4).
6. **Revisions `a` and `d`.** Accepted by the reference but probably never tested.
7. **Unknown header fields:** 0x10, the extra field in `i`/`j`/`k`, the other video flags, and the `frame_flags` bits not listed in §3.2 (in particular 17, 19 and 24–31, which RAD's player reads but which have no visible effect on the test files).
8. **Colour conversion: answered for RAD's converter.** RAD Video Tools converts with the BT.601 matrix at **full** range, centred bilinear chroma upsampling and chroma centred on 128.5 (black-box fit, see INFERRED.md on the implementation branch). Using BT.601 limited range instead costs about 7 dB and makes warm content look too saturated.
11. **P-frame drift: closed (draft 6).** It came from the missing loop filter (§4.10), the edge flag layout (§3.3), the inter DC range (§4.3) and the two motion compensation details (§4.7). With all of them, both test files match RAD on every frame.
12. **Bottom macroblock row: closed (draft 8)** for the integer variant: §4.2 SKIP, §4.7 hidden quadrants, §4.10 pass 3. Not checked: the float variant and alpha (no test files). The librempeg descriptions (#218) also report behaviour for older streams without `frame_flags` bit 18 (0x40000: a residual filter inside RESIDUE MBs) and for half-pel vectors at the very end of the reference in `KB2g`. That is not covered here (no test file).
13. **Float variant, alpha DC across slices (draft 9, open).** See §5.1: a SKIP or MOTION MB's alpha DC is never written or refreshed, so the first row of a slice can see a value from the previous slice. Is that what RAD does, or does RAD write or refresh it? No test file.
9. **Float bit-exactness.** It depends on IEEE float32 evaluation (no fused multiply-add, no extended precision).
10. **Bink Audio: see [AUDIO.md](AUDIO.md)** (draft 1, DCT codec checked against RAD).
