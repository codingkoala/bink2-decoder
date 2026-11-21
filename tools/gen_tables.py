#!/usr/bin/env python3
# Regenerates src/tables.rs from docs/TABLES.md:
#   python3 tools/gen_tables.py docs/TABLES.md > src/tables.rs
import re, sys
src = open(sys.argv[1]).read()
lines = src.split('\n')
# collect code blocks with the nearest preceding non-empty text line as label
blocks = []  # (label, content)
vlcs = []    # (label, rows)
i = 0
label = None
while i < len(lines):
    l = lines[i]
    if l.startswith('```'):
        j = i + 1
        body = []
        while not lines[j].startswith('```'):
            body.append(lines[j]); j += 1
        blocks.append((label, ' '.join(body)))
        i = j + 1
        continue
    if l.startswith('#### '):
        name = re.search(r'\(`(\w+)`\)', l).group(1)
        rows = []
        j = i + 1
        while j < len(lines) and not lines[j].startswith('#'):
            m = re.match(r'\| (\d+) \| (\d+) \| `([01]+)` \|', lines[j])
            if m:
                assert int(m.group(2)) == len(m.group(3)), lines[j]
                rows.append((int(m.group(1)), m.group(3)))
            j += 1
        for k, (s, _) in enumerate(rows): assert s == k
        vlcs.append((name, [b for _, b in rows]))
        i = j
        continue
    if l.strip():
        label = l.strip()
    i += 1

def nums(s): return s.split()
out = ["// Generated mechanically from docs/TABLES.md. Do not edit by hand.", "#![allow(clippy::excessive_precision, clippy::approx_constant)]", ""]
def arr(name, ty, vals, fmt=str):
    out.append(f"pub const {name}: [{ty}; {len(vals)}] = [" + ", ".join(fmt(v) for v in vals) + "];")
def mat4(name, ty, mats):
    out.append(f"pub const {name}: [[{ty}; 64]; 4] = [")
    for m in mats:
        assert len(m) == 64
        out.append("    [" + ", ".join(m) + "],")
    out.append("];")
labels = [b[0] for b in blocks]
it = iter(blocks)
def take(prefix):
    lab, body = next(it)
    assert lab.startswith(prefix), (lab, prefix)
    return nums(body)
z = take('**Z-order'); assert len(z)==16; arr('ZORDER', 'u8', z)
s = take('**Zigzag'); assert len(s)==64; arr('SCAN_G', 'u8', s)
for nm in ['QM_G_LUMA_INTRA', 'QM_G_CHROMA_INTRA', 'QM_G_INTER']:
    mats = [take('Matrix %d' % k) for k in range(4)]
    mat4(nm, 'i32', mats)
d = take('**DC scale'); assert len(d)==37; arr('DC_SCALE', 'i32', d)
a = take('**Luma scan'); assert len(a)==64; arr('SCAN_F_LUMA', 'u8', a)
a = take('**Chroma scan'); assert len(a)==64; arr('SCAN_F_CHROMA', 'u8', a)
for nm, pre in [('F_LUMA_INTRA', '**Luma intra'), ('F_LUMA_INTER', '**Luma inter'), ('F_CHROMA', '**Chroma (intra')]:
    a = take(pre); assert len(a)==64
    arr(nm, 'f32', a, lambda v: v if '.' in v else v + '.0')
def inline(prefix):
    m = re.search(re.escape(prefix) + r'[^:]*: `([^`]*)`', src)
    return [x.strip() for x in m.group(1).split(',')]
v = inline('**DC quantiser** `F_DC_Q[q]`'); assert len(v)==16; arr('F_DC_Q', 'f32', v, lambda x: x + '.0')
v = inline('**AC quantiser** `F_AC_Q[q]`'); assert len(v)==16; arr('F_AC_Q', 'f32', v)
v = inline('**Extra DC precision bits** `F_DC_BITS[q]`'); assert len(v)==16; arr('F_DC_BITS', 'i32', v)
out.append("")
out.append("/// VLC code tables: entry `s` is the code of symbol `s`, as bits in read order.")
for name, codes in vlcs:
    out.append(f"pub const {name}_CODES: [&str; {len(codes)}] = [" + ", ".join('"%s"' % c for c in codes) + "];")
print('\n'.join(out))
