"""Deterministic still-image fixtures for the diskrescue-stills tests.

Usage: python3 gen_images.py OUTDIR

Needs Pillow. Exits with status 3 when Pillow is missing so the tests can
skip with an explicit message. Files are small; nothing is committed.
"""
import io
import json
import os
import random
import struct
import sys

try:
    from PIL import Image, ImageDraw, TiffImagePlugin
except ImportError:
    sys.exit(3)

out = sys.argv[1]
os.makedirs(out, exist_ok=True)
rnd = random.Random(20241012)
facts = {}


def noise(w, h, mode="RGB"):
    n = {"RGB": 3, "RGBA": 4, "L": 1}[mode]
    return Image.frombytes(mode, (w, h), bytes(rnd.randrange(256) for _ in range(w * h * n)))


def picture(w, h):
    img = Image.new("RGB", (w, h), (30, 60, 90))
    d = ImageDraw.Draw(img)
    for i in range(0, w, 8):
        d.line([(i, 0), (w - i, h)], fill=(255 - i % 256, (i * 3) % 256, 128), width=2)
    d.rectangle([1, 1, max(2, w // 2), max(2, h // 2)], outline=(250, 250, 10))
    return img


def save(name, img, fmt, **kw):
    path = os.path.join(out, name)
    img.save(path, fmt, **kw)
    return path


def exif_bytes(dt="2024:05:06 07:08:09", make="DiskRescue", model="FixtureCam", orient=6):
    ex = Image.Exif()
    ex[0x010F] = make
    ex[0x0110] = model
    ex[0x0112] = orient
    ex[0x0132] = dt
    ex.get_ifd(0x8769)[0x9003] = dt
    return ex.tobytes()


# GIF ---------------------------------------------------------------------
base = picture(64, 48)
save("gif_static.gif", base.quantize(64), "GIF")
frames = [picture(64, 48).rotate(a).quantize(32) for a in (0, 90, 180, 270, 45)]
save("gif_anim.gif", frames[0], "GIF", save_all=True, append_images=frames[1:], duration=40, loop=0)
# Random content forces many dictionary resets (clear codes) and sub-blocks.
save("gif_noise.gif", noise(200, 150).quantize(256), "GIF")
facts["gif"] = {"static": [64, 48], "anim": [64, 48, 5], "noise": [200, 150]}

# BMP ---------------------------------------------------------------------
save("bmp_rgb24.bmp", picture(13, 7), "BMP")
save("bmp_rgba32.bmp", noise(17, 9, "RGBA"), "BMP")
save("bmp_pal8.bmp", picture(23, 11).quantize(200), "BMP")
save("bmp_mono.bmp", picture(37, 5).convert("1"), "BMP")
raw = open(os.path.join(out, "bmp_rgb24.bmp"), "rb").read()
off = struct.unpack("<I", raw[10:14])[0]
w, h = struct.unpack("<ii", raw[18:26])
stride = ((w * 24 + 31) // 32) * 4
rows = [raw[off + i * stride: off + (i + 1) * stride] for i in range(abs(h))]
topdown = bytearray(raw[:off] + b"".join(reversed(rows)))
topdown[22:26] = struct.pack("<i", -h)
open(os.path.join(out, "bmp_topdown.bmp"), "wb").write(topdown)
zero = bytearray(raw)
zero[2:6] = b"\0\0\0\0"
open(os.path.join(out, "bmp_size0.bmp"), "wb").write(zero)
facts["bmp"] = {"rgb24": [13, 7], "rgba32": [17, 9], "pal8": [23, 11], "mono": [37, 5]}

# WebP --------------------------------------------------------------------
save("webp_lossy.webp", picture(70, 50), "WEBP", quality=80)
save("webp_lossless.webp", picture(70, 50), "WEBP", lossless=True)
save("webp_exif.webp", picture(70, 50), "WEBP", quality=70, exif=exif_bytes())
fr = [picture(40, 30).rotate(a) for a in (0, 90, 180)]
save("webp_anim.webp", fr[0], "WEBP", save_all=True, append_images=fr[1:], duration=50, lossless=True)
facts["webp"] = {"size": [70, 50], "anim": [40, 30]}

# TIFF --------------------------------------------------------------------
save("tiff_strips.tif", picture(64, 48), "TIFF", compression=None)
save("tiff_lzw.tif", noise(64, 48), "TIFF", compression="tiff_lzw")
ifd = TiffImagePlugin.ImageFileDirectory_v2()
ifd[271] = "DiskRescue"
ifd[272] = "FixtureCam"
ifd[274] = 6
ifd[306] = "2024:05:06 07:08:09"
save("tiff_meta.tif", picture(64, 48), "TIFF", compression=None, tiffinfo=ifd)
pages = [picture(64, 48), picture(32, 24), picture(16, 12)]
save("tiff_pages.tif", pages[0], "TIFF", save_all=True, append_images=pages[1:], compression=None)
save("tiff_gray.tif", picture(50, 40).convert("L"), "TIFF", compression=None)
facts["tiff"] = {"strips": [64, 48], "pages": 3}

# JPEG with Exif (embedded in RAF by the tests) ---------------------------
save("jpeg_exif.jpg", picture(96, 64), "JPEG", quality=85, exif=exif_bytes(make="FUJIFILM", model="X-T3"))
save("jpeg_plain.jpg", picture(32, 24), "JPEG", quality=60)

json.dump(facts, open(os.path.join(out, "facts.json"), "w"))
