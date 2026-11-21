# Bink 2 video: decoding specification

Status: draft 3 (2026-11-22). Container checked against two real `KB2n` files; video not yet verified.

This document describes the Bink 2 container (`KB2*`) and video bitstream in enough detail to write a decoder. Numeric constants are in [TABLES.md](TABLES.md).

## Provenance and clean-room note

This is a functional description written by studying the unmerged LGPL-2.1+ FFmpeg `binkvideo2` decoder (`libavcodec/bink2.c`, `bink2f.c`, `bink2g.c`, and `libavformat/bink.c`, FFmpeg commit `f5b6ea3b65`, authors Konstantin Shishkov and Paul B Mahol). The document contains no code from those files. The algorithms are restated in prose and pseudocode, and some were simplified (see the DC prediction rule). The constant tables in TABLES.md are numeric data needed for interoperability, reformatted (VLC codes as bit strings in read order).

To keep the decoder implementation independent of the LGPL source:
- write it from this document and TABLES.md only, without reading the FFmpeg files;
- report anything unclear here as a spec question instead of looking at FFmpeg;
- answers come back as changes to this document.

This is a process recommendation, not legal advice.

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
1. For each audio track: `u32 size`, then `size` bytes of audio. When `size ≥ 4`, the first `u32` of the audio data is the decoded size in bytes. Audio is a separate codec ("Bink Audio", DCT/RDFT) and is not covered here.
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
   - if bit 15 (0x8000) is clear: read **row flags** with `count = ceil_to(height, 16) / 8 − 1` (§3.3);
   - then, if bit 14 (0x4000) is clear: read **column flags** with `count = ceil_to(width, 16) / 8 − 1`.
   The reference parses these flags but never uses them. They probably mark 8-pixel edges for an edge or deblocking filter **(unverified; see open questions)**. A decoder must still parse them to stay in sync.
6. For each slice i: decode its MB rows (§4 or §5) starting at the current bit position, which is not byte-aligned for slice 0. Then `align()` and move the reader to byte `slice_end[i]`. The reference only warns about over- or under-reads.

### 3.3 Edge flag lists

This produces a list of `count` + 1 flags F[0] … F[count].

```
if bit() == 0:                         // raw form
    F[0 .. count-1] = next count bits (one per flag, in order)
    F[count] = 0
    done
F[0] = 0; pos = 1; left = count; mode = NONE; flag = 0
while left > 0:
    if bit() == 0:                     // literal group
        flag = (mode == TOGGLE) ? 1 - flag : bit()
        mode = LITERAL
        n = min(left, 5)
        F[pos] = flag
        F[pos+1 .. pos+n-1] = next n-1 bits
        pos += n; left -= n
    else:                              // run
        w = (left < 4) ? 2 : (left < 16) ? 4 : 5
        if mode == TOGGLE:
            flag = 1 - flag; run = w + 1
        else:
            flag = bit();    run = w + 2
        run = min(run, left); left -= run
        if left > 0:
            extra = bits(w); left -= extra; run += extra
            mode = (extra == 2^w - 1) ? EXPLICIT : TOGGLE
        F[pos .. pos+run-1] = flag; pos += run   // may run past count, see below
```
`mode` starts as "not TOGGLE". After a literal group the next flag value is read explicitly. After a run that ended early (extra < max), the next segment's first flag is the opposite of this run's flag.

If `extra` is larger than what was left, `left` goes negative. That is not an error: the whole run is still produced and the loop ends. Flags past F[count] are discarded, and no further bits are read.

Note the asymmetry: the raw form starts at F[0], the coded form at F[1]. The intended meaning is unknown, so keep both exactly as written.

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

`read_dq`:
```
n = zeros(4)
if n == 3: n += bit()
if n == 4: n += bits(5) + 1
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
2. Copy the co-located 32×32 luma (and alpha) area and the 16×16 area of each chroma plane from the reference frame.
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
| DC range | 0 … 2047 | −1023 … 1023 |
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

If the source top-left is outside `[0, W) × [0, H)` (W and H = the **visible** picture width and height, not the 32-aligned size, halved with integer division for chroma), the reference **writes nothing** for that block **(unverified; the block keeps undefined content)**. Because of this, even zero-vector blocks that start in the padding right of or below the picture count as out of range.

**Luma filter.** For six samples `s[-2] … s[3]` around the half-pel position between `s[0]` and `s[1]`:
`h = clip8(( ((19·(s0 + s1)) >> 1) − 2·(s−1 + s2) + ((s−2 + s3) >> 1) + 8 ) >> 4)`

By fraction:
- (0,0): copy;
- (1,0): horizontal filter;
- (0,1): vertical filter;
- (1,1): run the horizontal filter on the 21 source rows −2 … +18 (results clipped), then the vertical filter on that intermediate.

**Chroma filter.** Between samples a and b, at fraction f:
- f = 1: `(6a + 2b + 4) >> 3`;
- f = 2: `(a + b + 1) >> 1`;
- f = 3: `(2a + 6b + 4) >> 3`.

If fy = 0, filter horizontally only (fx = 0 means copy). If fx = 0, filter vertically only. Otherwise filter 9 rows horizontally (clipped), then filter that result vertically.

The filters and blocks can read past the picture and past the 32-aligned frame. The reference relies on unspecified buffer padding there **(unverified)**. Suggestion: keep reference planes with a margin of at least 32 pixels, filled by edge replication.

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

## 5. Float variant (revisions ≤ `f`)

Same structure as §4 (MBs, slices, Z-order, edges, DC prediction rule, neighbour refresh, MC filters and vector units), with these differences.

### 5.1 State and macroblock

- Block type: `bits(2)` with the numbering of §4.1, no LRU. On keyframes no type bits are read and every MB is INTRA, as in §4.2.
- No spatial q prediction. Each (plane, intra/inter) pair has a running q, set to 8 at the start of every MB row. Each coded plane reads `dq = vlc(F_DQ)`, and if `dq != 0 and bit()`, `dq = -dq`. Then `q = running + dq`, which must be in 0 … 15 and becomes the new running value.
- Only previous-row `mv` is reset at slice start. Per row, current `mv` and the 8 running cbps are reset to 0.
- Neighbour refresh covers Y, C1 and C2 only (no alpha).
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

1. **Row and column flags (§3.3).** What are they for? Probably an 8-pixel edge or deblocking filter that the reference does not implement. If RAD applies a filter, output will differ along those edges.
2. **Out-of-picture motion vectors (§4.7).** RAD's behaviour is unknown: skip, clamp or edge-extend. The reference leaves the block undefined.
3. **Reference padding.** Pixels outside the picture are read by the luma 6-tap filter and by blocks that cross the right or bottom edge.
4. **Plane order.** Is C1 really Cr?
5. **Float variant:** the alpha MC omission and the `c2` prediction difference (§5.1, §5.4).
6. **Revisions `a` and `d`.** Accepted by the reference but probably never tested.
7. **Unknown header fields:** 0x10, the extra field in `i`/`j`/`k`, the other video flags, and the `frame_flags` bits other than 14, 15, 16 and 18.
8. **Colour matrix and range.** Not signalled anywhere known. BT.601 limited range is a guess.
9. **Float bit-exactness.** It depends on IEEE float32 evaluation (no fused multiply-add, no extended precision).
10. **Bink Audio** (DCT/RDFT) still needs its own spec. It is a separate codec already in mainline FFmpeg (`binkaudio.c`, LGPL), so the same clean-room process applies.
