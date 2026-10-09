#!/usr/bin/env python3
"""Compare two PNGs (pure stdlib; 8-bit, non-interlaced, any colour type).

usage: png_diff.py a.png b.png [--out heat.png]

Prints the size of each image, the mean absolute difference per channel (R, G, B),
and the share of pixels whose largest channel difference exceeds 8 and 32 (of 255).
With --out, writes a heat map (black = equal, red/yellow/white = larger difference).
Images of different sizes are reported and not compared.
"""
import struct
import sys
import zlib

CHANNELS = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}


def read_png(path):
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError(f"{path}: not a PNG")
    pos, idat, plte = 8, [], None
    while pos < len(data):
        n, kind = struct.unpack(">I4s", data[pos:pos + 8])
        body = data[pos + 8:pos + 8 + n]
        pos += 12 + n
        if kind == b"IHDR":
            w, h, depth, ctype, _, _, interlace = struct.unpack(">IIBBBBB", body)
        elif kind == b"PLTE":
            plte = body
        elif kind == b"IDAT":
            idat.append(body)
    if depth != 8 or interlace or ctype not in CHANNELS:
        raise ValueError(f"{path}: unsupported (depth {depth}, type {ctype}, interlace {interlace})")
    ch = CHANNELS[ctype]
    raw = zlib.decompress(b"".join(idat))
    stride = w * ch
    out = bytearray(h * stride)
    prev = bytearray(stride)
    p = 0
    for y in range(h):
        f = raw[p]
        line = bytearray(raw[p + 1:p + 1 + stride])
        p += 1 + stride
        if f == 1:
            for i in range(ch, stride):
                line[i] = (line[i] + line[i - ch]) & 255
        elif f == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 255
        elif f == 3:
            for i in range(stride):
                a = line[i - ch] if i >= ch else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 255
        elif f == 4:
            for i in range(stride):
                a = line[i - ch] if i >= ch else 0
                b = prev[i]
                c = prev[i - ch] if i >= ch else 0
                pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2 * c)
                pr = a if pa <= pb and pa <= pc else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 255
        out[y * stride:(y + 1) * stride] = line
        prev = line
    # normalise to RGB
    rgb = bytearray(w * h * 3)
    if ctype == 2:
        rgb = out
    elif ctype == 6:
        rgb[0::3], rgb[1::3], rgb[2::3] = out[0::4], out[1::4], out[2::4]
    elif ctype == 0:
        rgb[0::3] = rgb[1::3] = rgb[2::3] = out
    elif ctype == 4:
        g = out[0::2]
        rgb[0::3] = rgb[1::3] = rgb[2::3] = g
    elif ctype == 3:
        for i, idx in enumerate(out):
            rgb[i * 3:i * 3 + 3] = plte[idx * 3:idx * 3 + 3]
    return w, h, rgb


def write_png(path, w, h, rgb):
    raw = b"".join(b"\x00" + bytes(rgb[y * w * 3:(y + 1) * w * 3]) for y in range(h))

    def chunk(kind, body):
        c = struct.pack(">I", len(body)) + kind + body
        return c + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)

    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
                + chunk(b"IDAT", zlib.compress(raw, 6)) + chunk(b"IEND", b""))


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    a_path, b_path = argv[0], argv[1]
    out = argv[argv.index("--out") + 1] if "--out" in argv else None
    wa, ha, a = read_png(a_path)
    wb, hb, b = read_png(b_path)
    print(f"a {a_path}: {wa}x{ha}\nb {b_path}: {wb}x{hb}")
    if (wa, ha) != (wb, hb):
        print("sizes differ; not compared")
        return 1
    n = wa * ha
    sums = [0, 0, 0]
    over8 = over32 = 0
    heat = bytearray(n * 3) if out else None
    for i in range(n):
        j = i * 3
        d0, d1, d2 = abs(a[j] - b[j]), abs(a[j + 1] - b[j + 1]), abs(a[j + 2] - b[j + 2])
        sums[0] += d0
        sums[1] += d1
        sums[2] += d2
        m = max(d0, d1, d2)
        if m > 8:
            over8 += 1
            if m > 32:
                over32 += 1
        if heat is not None and m:
            v = min(255, m * 4)  # 64+ saturates
            heat[j] = v
            heat[j + 1] = max(0, v - 128) * 2
            heat[j + 2] = max(0, v - 224) * 8
    print(f"mean abs diff per channel (of 255): R {sums[0] / n:.4f}  G {sums[1] / n:.4f}  B {sums[2] / n:.4f}")
    print(f"pixels differing by >8: {100 * over8 / n:.3f}%   by >32: {100 * over32 / n:.3f}%")
    if out:
        write_png(out, wa, ha, heat)
        print(f"heat map: {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
