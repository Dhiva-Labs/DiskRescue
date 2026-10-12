#!/usr/bin/env python3
"""A small pure-Python sequential JPEG encoder for test fixtures Pillow
cannot write: arbitrary sampling factors (4:1:1, 4:4:0, 2x1/1x2 mixes),
12-bit precision (SOF1), optimised Huffman tables, restart intervals and
mozjpeg-style overshoot deringing (samples above 255 near white edges).

Only what the tests need; it is slow (float DCT in Python), so keep the
images small. Every output is a standard-conforming baseline/extended
sequential JPEG (libjpeg-turbo decodes the 8-bit ones).
"""
import math
import struct

ZIGZAG = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
]

STD_LUMA_Q = [
    16, 11, 12, 14, 12, 10, 16, 14, 13, 14, 18, 17, 16, 19, 24, 40, 26, 24, 22, 22, 24, 49, 35, 37,
    29, 40, 58, 51, 61, 60, 57, 51, 56, 55, 64, 72, 92, 78, 64, 68, 87, 69, 55, 56, 80, 109, 81, 87,
    95, 98, 103, 104, 103, 62, 77, 113, 121, 112, 100, 120, 92, 101, 103, 99,
]  # zigzag order
STD_CHROMA_Q = [
    17, 18, 18, 24, 21, 24, 47, 26, 26, 47, 99, 66, 56, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
]

_C = [
    [(math.sqrt(1 / 8) if u == 0 else math.sqrt(2 / 8)) * math.cos((2 * x + 1) * u * math.pi / 16)
     for x in range(8)]
    for u in range(8)
]


def scaled_table(base, quality, maxval=255):
    q = max(1, min(100, quality))
    scale = 5000 // q if q < 50 else 200 - 2 * q
    return [max(1, min(maxval, (v * scale + 50) // 100)) for v in base]


def fdct(block):
    """Orthonormal 2-D DCT-II of 64 samples (natural order)."""
    tmp = [[sum(_C[u][x] * block[y * 8 + x] for x in range(8)) for u in range(8)] for y in range(8)]
    out = [0.0] * 64
    for v in range(8):
        cv = _C[v]
        for u in range(8):
            out[v * 8 + u] = sum(cv[y] * tmp[y][u] for y in range(8))
    return out


def _catmull_rom(v1, v2, v3, v4, t, size):
    tan1 = (v3 - v1) * size
    tan2 = (v4 - v2) * size
    t2 = t * t
    t3 = t2 * t
    f1 = 2 * t3 - 3 * t2 + 1
    f2 = -2 * t3 + 3 * t2
    f3 = t3 - 2 * t2 + t
    f4 = t3 - t2
    return v2 * f1 + tan1 * f3 + v3 * f2 + tan2 * f4


def dering(data, q0, maxsample):
    """mozjpeg preprocess_deringing (jcdctmgr.c), on level-shifted samples."""
    size = 64
    s = sum(data)
    count = sum(1 for v in data if v >= maxsample)
    if count == 0 or count == size:
        return data
    maxover = maxsample + min(min(31, 2 * q0), int((maxsample * size - s) / count))
    data = list(data)
    n = 0
    while n < size:
        if data[ZIGZAG[n]] < maxsample:
            n += 1
            continue
        start = n
        n += 1
        while n < size and data[ZIGZAG[n]] >= maxsample:
            n += 1
        end = n
        f1 = data[ZIGZAG[start - 1 if start >= 1 else 0]]
        f2 = data[ZIGZAG[start - 2 if start >= 2 else 0]]
        l1 = data[ZIGZAG[end if end < size - 1 else size - 1]]
        l2 = data[ZIGZAG[end + 1 if end < size - 2 else size - 1]]
        fslope = max(f1 - f2, maxsample - f1)
        lslope = max(l1 - l2, maxsample - l1)
        if start == 0:
            fslope = lslope
        if end == size:
            lslope = fslope
        length = end - start
        step = 1.0 / (length + 1)
        pos = step
        for i in range(start, end):
            tmp = math.ceil(_catmull_rom(maxsample - fslope, maxsample, maxsample, maxsample - lslope, pos, length))
            data[ZIGZAG[i]] = min(tmp, maxover)
            pos += step
        n += 1
    return data


def gen_optimal_table(freq):
    """libjpeg jpeg_gen_optimal_table: (bits[16], huffval) for 256 symbols."""
    freq = list(freq) + [1]
    codesize = [0] * 257
    others = [-1] * 257
    while True:
        c1, v = -1, 1 << 62
        for i in range(257):
            if freq[i] and freq[i] <= v:
                v, c1 = freq[i], i
        c2, v = -1, 1 << 62
        for i in range(257):
            if freq[i] and freq[i] <= v and i != c1:
                v, c2 = freq[i], i
        if c2 < 0:
            break
        freq[c1] += freq[c2]
        freq[c2] = 0
        codesize[c1] += 1
        while others[c1] >= 0:
            c1 = others[c1]
            codesize[c1] += 1
        others[c1] = c2
        codesize[c2] += 1
        while others[c2] >= 0:
            c2 = others[c2]
            codesize[c2] += 1
    bits = [0] * 33
    for i in range(257):
        if codesize[i]:
            bits[codesize[i]] += 1
    for i in range(32, 16, -1):
        while bits[i] > 0:
            j = i - 2
            while bits[j] == 0:
                j -= 1
            bits[i] -= 2
            bits[i - 1] += 1
            bits[j + 1] += 2
            bits[j] -= 1
    i = 16
    while bits[i] == 0:
        i -= 1
    bits[i] -= 1
    huffval = [j for i in range(1, 33) for j in range(256) if codesize[j] == i]
    return bits[1:17], huffval


def canonical_codes(bits, vals):
    codes = {}
    code = 0
    k = 0
    for length in range(1, 17):
        for _ in range(bits[length - 1]):
            codes[vals[k]] = (code, length)
            code += 1
            k += 1
        code <<= 1
    return codes


class BitWriter:
    def __init__(self):
        self.out = bytearray()
        self.acc = 0
        self.n = 0

    def put(self, value, length):
        for i in range(length - 1, -1, -1):
            self.acc = (self.acc << 1) | ((value >> i) & 1)
            self.n += 1
            if self.n == 8:
                self.out.append(self.acc)
                if self.acc == 0xFF:
                    self.out.append(0)
                self.acc = 0
                self.n = 0

    def flush(self):
        if self.n:
            self.put((1 << (8 - self.n)) - 1, 8 - self.n)


def _category(v):
    v = abs(v)
    s = 0
    while v:
        s += 1
        v >>= 1
    return s


def _bits_of(v, s):
    return v if v >= 0 else v + (1 << s) - 1


def encode(planes, width, height, sampling, precision=8, quality=85, restart=0,
           deringing=False, ids=None, app=b"", qtables=None):
    """Encode component sample planes.

    planes: list of 2-D lists [rows][cols] of samples (0..2^precision-1) at
    full resolution; they are box-downsampled to each component's size.
    sampling: list of (h, v) per component.
    """
    maxv = (1 << precision) - 1
    center = 1 << (precision - 1)
    nc = len(planes)
    ids = ids or list(range(1, nc + 1))
    hmax = max(h for h, _ in sampling)
    vmax = max(v for _, v in sampling)
    mx = -(-width // (8 * hmax))
    my = -(-height // (8 * vmax))
    if qtables is None:
        qmax = 255 if precision == 8 else 32767
        qtables = [scaled_table(STD_LUMA_Q, quality, qmax), scaled_table(STD_CHROMA_Q, quality, qmax)]
    tq = [0] + [1] * (nc - 1)
    # Component sample grids padded to whole MCUs (edge replication).
    comp_blocks = []
    for ci in range(nc):
        h, v = sampling[ci]
        fx, fy = hmax // h, vmax // v
        cw, ch = -(-width * h // hmax), -(-height * v // vmax)
        src = planes[ci]
        grid_w, grid_h = mx * h * 8, my * v * 8
        grid = []
        for y in range(grid_h):
            yy = min(y, ch - 1)
            row = []
            for x in range(grid_w):
                xx = min(x, cw - 1)
                acc = 0
                cnt = 0
                for dy in range(fy):
                    sy = min(yy * fy + dy, height - 1)
                    for dx in range(fx):
                        sx = min(xx * fx + dx, width - 1)
                        acc += src[sy][sx]
                        cnt += 1
                row.append((acc + cnt // 2) // cnt)
            grid.append(row)
        comp_blocks.append(grid)
    # Quantised coefficients per MCU, in scan order.
    q_nat = []
    for t in qtables:
        nat = [0] * 64
        for k in range(64):
            nat[ZIGZAG[k]] = t[k]
        q_nat.append(nat)
    mcus = []
    for my_i in range(my):
        for mx_i in range(mx):
            mcu = []
            for ci in range(nc):
                h, v = sampling[ci]
                grid = comp_blocks[ci]
                for by in range(v):
                    for bx in range(h):
                        x0 = (mx_i * h + bx) * 8
                        y0 = (my_i * v + by) * 8
                        blk = [grid[y0 + y][x0 + x] - center for y in range(8) for x in range(8)]
                        qn = q_nat[tq[ci]]
                        if deringing and precision == 8:
                            blk = dering(blk, qn[0], maxv - center)
                        coef = fdct(blk)
                        zz = [int(round(coef[ZIGZAG[k]] / qn[ZIGZAG[k]])) for k in range(64)]
                        mcu.append((ci, zz))
            mcus.append(mcu)
    # Symbols, then optimal tables per class/table id.
    nt = 2 if nc > 1 else 1
    dc_freq = [[0] * 256 for _ in range(nt)]
    ac_freq = [[0] * 256 for _ in range(nt)]
    syms = []  # (kind, table, symbol, extra, nbits) or ("rst", n)
    pred = [0] * nc
    for i, mcu in enumerate(mcus):
        if restart and i and i % restart == 0:
            syms.append(("rst", (i // restart - 1) & 7))
            pred = [0] * nc
        for ci, zz in mcu:
            t = min(ci, nt - 1)
            diff = zz[0] - pred[ci]
            pred[ci] = zz[0]
            s = _category(diff)
            dc_freq[t][s] += 1
            syms.append(("dc", t, s, _bits_of(diff, s), s))
            r = 0
            for k in range(1, 64):
                c = zz[k]
                if c == 0:
                    r += 1
                    continue
                while r > 15:
                    ac_freq[t][0xF0] += 1
                    syms.append(("ac", t, 0xF0, 0, 0))
                    r -= 16
                s = _category(c)
                rs = (r << 4) | s
                ac_freq[t][rs] += 1
                syms.append(("ac", t, rs, _bits_of(c, s), s))
                r = 0
            if r > 0:
                ac_freq[t][0] += 1
                syms.append(("ac", t, 0, 0, 0))
    dc_tabs = [gen_optimal_table(f) for f in dc_freq]
    ac_tabs = [gen_optimal_table(f) for f in ac_freq]
    dc_codes = [canonical_codes(b, v) for b, v in dc_tabs]
    ac_codes = [canonical_codes(b, v) for b, v in ac_tabs]
    bw = BitWriter()
    for sym in syms:
        if sym[0] == "rst":
            bw.flush()
            bw.out += bytes([0xFF, 0xD0 + sym[1]])
            continue
        kind, t, s, extra, nb = sym
        code, length = (dc_codes if kind == "dc" else ac_codes)[t][s]
        bw.put(code, length)
        if nb:
            bw.put(extra, nb)
    bw.flush()

    def seg(marker, payload):
        return bytes([0xFF, marker]) + struct.pack(">H", len(payload) + 2) + payload

    out = bytearray(b"\xff\xd8")
    out += app
    for i, t in enumerate(qtables[: 2 if nc > 1 else 1]):
        if max(t) > 255:
            out += seg(0xDB, bytes([0x10 | i]) + b"".join(struct.pack(">H", x) for x in t))
        else:
            out += seg(0xDB, bytes([i]) + bytes(t))
    sof = 0xC0 if precision == 8 else 0xC1
    p = struct.pack(">BHHB", precision, height, width, nc)
    for ci in range(nc):
        h, v = sampling[ci]
        p += bytes([ids[ci], (h << 4) | v, tq[ci]])
    out += seg(sof, p)
    for t in range(nt):
        b, v = dc_tabs[t]
        out += seg(0xC4, bytes([t]) + bytes(b) + bytes(v))
        b, v = ac_tabs[t]
        out += seg(0xC4, bytes([0x10 | t]) + bytes(b) + bytes(v))
    if restart:
        out += seg(0xDD, struct.pack(">H", restart))
    p = bytes([nc])
    for ci in range(nc):
        t = min(ci, nt - 1)
        p += bytes([ids[ci], (t << 4) | t])
    p += bytes([0, 63, 0])
    out += seg(0xDA, p)
    out += bw.out
    out += b"\xff\xd9"
    return bytes(out)


def rgb_planes(img):
    """Y, Cb, Cr planes (8-bit) of a Pillow RGB image."""
    ycc = img.convert("YCbCr")
    w, h = ycc.size
    planes = []
    for band in ycc.split():
        data = band.tobytes()
        planes.append([list(data[y * w:(y + 1) * w]) for y in range(h)])
    return planes


def gray_plane(img):
    g = img.convert("L")
    w, h = g.size
    data = g.tobytes()
    return [list(data[y * w:(y + 1) * w]) for y in range(h)]
