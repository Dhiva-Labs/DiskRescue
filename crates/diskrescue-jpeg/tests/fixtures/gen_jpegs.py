#!/usr/bin/env python3
"""Generate JPEG test fixtures for diskrescue-jpeg with Pillow.

Usage: gen_jpegs.py OUTDIR [all|large|committed]

  all        the main set used by the integration tests (written at test time)
  large      one > 20 MB JPEG for the streaming test
  committed  the tiny fixtures checked into tests/fixtures/ (< 50 KB total)

Output is deterministic for a given Pillow/libjpeg version (seeded noise).
"""
import io
import os
import random
import struct
import sys

from PIL import Image, ImageDraw, ImageFile

# Progressive/optimised noise needs a bigger encoder buffer than the default.
ImageFile.MAXBLOCK = 1 << 26


def noise(w, h, seed, mode="RGB"):
    r = random.Random(seed)
    n = w * h * (3 if mode == "RGB" else 1)
    return Image.frombytes(mode, (w, h), r.randbytes(n))


def photo(w, h, seed):
    """Smooth content with shapes and mild noise: realistic, EOB-heavy."""
    r = random.Random(seed)
    g = Image.linear_gradient("L").resize((w, h))
    b = Image.radial_gradient("L").resize((w, h))
    n = Image.frombytes("L", (w, h), r.randbytes(w * h)).point(lambda v: 96 + v // 4)
    img = Image.merge("RGB", (g, b, n))
    d = ImageDraw.Draw(img)
    for _ in range(24):
        x0, y0 = r.randrange(w), r.randrange(h)
        x1 = x0 + r.randrange(1, max(2, w // 3))
        y1 = y0 + r.randrange(1, max(2, h // 3))
        col = (r.randrange(256), r.randrange(256), r.randrange(256))
        if r.random() < 0.5:
            d.ellipse([x0, y0, x1, y1], fill=col)
        else:
            d.rectangle([x0, y0, x1, y1], fill=col)
    d.text((5, 5), "DiskRescue", fill=(255, 255, 255))
    return img


def jpeg_bytes(img, **kw):
    b = io.BytesIO()
    img.save(b, "JPEG", **kw)
    return b.getvalue()


def exif_basic(dt="2024:05:06 07:08:09"):
    ex = Image.Exif()
    ex[0x010F] = "DiskRescue"
    ex[0x0110] = "FixtureCam"
    ex[0x0112] = 6
    ex[0x0132] = dt
    ex.get_ifd(0x8769)[0x9003] = dt
    return ex.tobytes()


def exif_with_thumbnail(thumb, dt="2024:07:14 10:22:31"):
    """Exif\\0\\0 + little-endian TIFF: IFD0 {Make, Model, Orientation,
    DateTime, ExifIFD}, ExifIFD {DateTimeOriginal}, IFD1 {0x0201, 0x0202}."""
    dtb = dt.encode() + b"\0"
    make = b"DiskRescue\0"
    model = b"ThumbCam\0"
    n0 = 5
    ifd0 = 8
    exif = ifd0 + 2 + n0 * 12 + 4
    ifd1 = exif + 2 + 12 + 4
    data = ifd1 + 2 + 3 * 12 + 4
    make_off = data
    model_off = make_off + len(make)
    dt_off = model_off + len(model)
    dto_off = dt_off + len(dtb)
    thumb_off = dto_off + len(dtb)
    t = b"II*\0" + struct.pack("<I", ifd0)
    t += struct.pack("<H", n0)
    t += struct.pack("<HHII", 0x010F, 2, len(make), make_off)
    t += struct.pack("<HHII", 0x0110, 2, len(model), model_off)
    t += struct.pack("<HHIHH", 0x0112, 3, 1, 1, 0)
    t += struct.pack("<HHII", 0x0132, 2, len(dtb), dt_off)
    t += struct.pack("<HHII", 0x8769, 4, 1, exif)
    t += struct.pack("<I", ifd1)
    t += struct.pack("<H", 1) + struct.pack("<HHII", 0x9003, 2, len(dtb), dto_off) + struct.pack("<I", 0)
    t += struct.pack("<H", 3)
    t += struct.pack("<HHIHH", 0x0103, 3, 1, 6, 0)
    t += struct.pack("<HHII", 0x0201, 4, 1, thumb_off)
    t += struct.pack("<HHII", 0x0202, 4, 1, len(thumb))
    t += struct.pack("<I", 0)
    assert len(t) == data
    t += make + model + dtb + dtb
    assert len(t) == thumb_off
    t += thumb
    return b"Exif\0\0" + t


def fake_mp4(payload, seed=7):
    r = random.Random(seed)
    ftyp = struct.pack(">I", 24) + b"ftypisom" + struct.pack(">I", 0x200) + b"isommp42"
    moov = struct.pack(">I", 40) + b"moov" + struct.pack(">I", 32) + b"mvhd" + bytes(24)
    mdat = struct.pack(">I", 8 + payload) + b"mdat" + r.randbytes(payload)
    return ftyp + moov + mdat


def insert_after_app0(jpg, seg):
    at = 2
    if jpg[2:4] == b"\xff\xe0":
        at = 4 + struct.unpack(">H", jpg[4:6])[0]
    return jpg[:at] + seg + jpg[at:], at


def mpf_file(main, second):
    n_entries = 3
    entries_off = 8 + 2 + n_entries * 12 + 4

    def tiff(main_size, second_offset):
        t = b"MM\0*" + struct.pack(">I", 8)
        t += struct.pack(">H", n_entries)
        t += struct.pack(">HHI", 0xB000, 7, 4) + b"0100"
        t += struct.pack(">HHII", 0xB001, 4, 1, 2)
        t += struct.pack(">HHII", 0xB002, 7, 32, entries_off)
        t += struct.pack(">I", 0)
        t += struct.pack(">IIIHH", 0x20030000, main_size, 0, 0, 0)
        t += struct.pack(">IIIHH", 0x00010001, len(second), second_offset, 0, 0)
        return t

    def seg(t):
        return b"\xff\xe2" + struct.pack(">H", 2 + 4 + len(t)) + b"MPF\0" + t

    seglen = len(seg(tiff(0, 0)))
    _, at = insert_after_app0(main, b"")
    total_main = len(main) + seglen
    tiff_start = at + 4 + 4
    out, _ = insert_after_app0(main, seg(tiff(total_main, total_main - tiff_start)))
    assert len(out) == total_main
    return out + second


def motion_google(jpg, mp4):
    xmp = (
        '<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF '
        'xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description '
        'xmlns:GCamera="http://ns.google.com/photos/1.0/camera/" GCamera:MotionPhoto="1" '
        'GCamera:MotionPhotoVersion="1" GCamera:MicroVideoOffset="%d"/></rdf:RDF></x:xmpmeta>'
        % len(mp4)
    ).encode()
    payload = b"http://ns.adobe.com/xap/1.0/\0" + xmp
    seg = b"\xff\xe1" + struct.pack(">H", 2 + len(payload)) + payload
    return jpg[:2] + seg + jpg[2:] + mp4


def motion_samsung(jpg, mp4):
    blocks = []
    name = b"MotionPhoto_Data"
    blocks.append((0x0A30, struct.pack("<HHI", 0, 0x0A30, len(name)) + name + mp4))
    name = b"Image_UTC_Data"
    blocks.append((0x0A01, struct.pack("<HHI", 0, 0x0A01, len(name)) + name + b"1700000000000"))
    body = jpg
    starts = []
    for _, b in blocks:
        starts.append(len(body))
        body += b
    h = len(body)
    d = b"SEFH" + struct.pack("<II", 101, len(blocks))
    for (typ, b), s in zip(blocks, starts):
        d += struct.pack("<HHII", 0, typ, h - s, len(b))
    return body + d + struct.pack("<I", len(d)) + b"SEFT"


def write(out, name, data):
    with open(os.path.join(out, name), "wb") as f:
        f.write(data)


def gen_all(out):
    write(out, "base_420_640x480.jpg", jpeg_bytes(photo(640, 480, 1), quality=90, exif=exif_basic()))
    write(out, "base_444_320x240.jpg", jpeg_bytes(noise(320, 240, 2), quality=85, subsampling=0))
    write(out, "base_422_200x150.jpg", jpeg_bytes(photo(200, 150, 3), quality=80, subsampling=1))
    write(out, "gray_160x120.jpg", jpeg_bytes(photo(160, 120, 4).convert("L"), quality=70))
    write(out, "prog_640x480.jpg", jpeg_bytes(photo(640, 480, 5), quality=80, progressive=True, exif=exif_basic()))
    write(out, "prog_gray_64x48.jpg", jpeg_bytes(noise(64, 48, 6, "L"), quality=90, progressive=True))
    write(out, "prog_444_320x240.jpg", jpeg_bytes(noise(320, 240, 7), quality=85, progressive=True, subsampling=0))
    write(out, "prog_rst_320x240.jpg", jpeg_bytes(photo(320, 240, 8), quality=85, progressive=True, restart_marker_blocks=2))
    write(out, "rst_rows_640x480.jpg", jpeg_bytes(noise(640, 480, 9), quality=85, restart_marker_rows=1))
    write(out, "rst_blocks_320x240.jpg", jpeg_bytes(photo(320, 240, 10), quality=85, restart_marker_blocks=3))
    write(out, "gray_rst_96x64.jpg", jpeg_bytes(photo(96, 64, 11).convert("L"), quality=85, restart_marker_blocks=5))
    write(out, "opt_640x480.jpg", jpeg_bytes(photo(640, 480, 12), quality=75, optimize=True))
    write(out, "cmyk_128x96.jpg", jpeg_bytes(photo(128, 96, 13).convert("CMYK"), quality=70))
    write(out, "tiny_64x48.jpg", jpeg_bytes(photo(64, 48, 14), quality=75))
    write(out, "odd_420_201x149.jpg", jpeg_bytes(photo(201, 149, 15), quality=88))
    write(out, "odd_prog_203x77.jpg", jpeg_bytes(photo(203, 77, 16), quality=88, progressive=True))
    write(out, "odd_prog_444_99x51.jpg", jpeg_bytes(noise(99, 51, 17), quality=88, progressive=True, subsampling=0))
    thumb = jpeg_bytes(photo(160, 120, 18), quality=50)
    write(out, "thumb_exif_1024x768.jpg",
          jpeg_bytes(noise(1024, 768, 19), quality=85, exif=exif_with_thumbnail(thumb)))
    # Truncation / corruption subjects (noise: uniform bits per MCU row).
    write(out, "noise_1024x768.jpg", jpeg_bytes(noise(1024, 768, 20), quality=90))
    write(out, "noise_rst_1024x768.jpg", jpeg_bytes(noise(1024, 768, 21), quality=90, restart_marker_blocks=4))
    write(out, "photo_1024x768.jpg", jpeg_bytes(photo(1024, 768, 22), quality=90))
    write(out, "prog_noise_1024x768.jpg", jpeg_bytes(noise(1024, 768, 23), quality=85, progressive=True))
    # Standard tables (optimize=False) for the DHT-stripping test.
    write(out, "std_640x480.jpg", jpeg_bytes(photo(640, 480, 24), quality=85, optimize=False))
    # Donor pair: same settings, different content.
    write(out, "victim_640x480.jpg", jpeg_bytes(photo(640, 480, 25), quality=90))
    write(out, "donor_640x480.jpg", jpeg_bytes(photo(640, 480, 26), quality=90))
    write(out, "donor_wrong_320x240.jpg", jpeg_bytes(photo(320, 240, 27), quality=90))
    # MPF and motion photos.
    main = jpeg_bytes(photo(640, 480, 28), quality=85)
    second = jpeg_bytes(photo(320, 240, 29), quality=80)
    write(out, "mpf2.jpg", mpf_file(main, second))
    write(out, "motion_google.jpg", motion_google(jpeg_bytes(photo(320, 240, 30), quality=85), fake_mp4(30000)))
    write(out, "motion_samsung.jpg", motion_samsung(jpeg_bytes(photo(320, 240, 31), quality=85), fake_mp4(20000, 9)))
    write(out, "fake.mp4", fake_mp4(1000, 3))


def gen_large(out):
    # Uniform noise at q100 4:4:4 compresses to roughly 3 bytes per pixel.
    w, h = 2700, 2050
    write(out, "large.jpg", jpeg_bytes(noise(w, h, 99), quality=100, subsampling=0))


def gen_committed(out):
    thumb = jpeg_bytes(photo(40, 30, 101), quality=40)
    write(out, "small_exif_thumb.jpg",
          jpeg_bytes(photo(96, 72, 102), quality=70, exif=exif_with_thumbnail(thumb)))
    write(out, "small_progressive.jpg", jpeg_bytes(photo(80, 64, 103), quality=70, progressive=True))
    write(out, "small_restart.jpg", jpeg_bytes(photo(80, 64, 104), quality=70, restart_marker_blocks=2))
    write(out, "small_gray.jpg", jpeg_bytes(photo(48, 40, 105).convert("L"), quality=70))
    write(out, "small_mpf.jpg",
          mpf_file(jpeg_bytes(photo(64, 48, 106), quality=60), jpeg_bytes(photo(32, 24, 107), quality=60)))


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    out = sys.argv[1]
    what = sys.argv[2] if len(sys.argv) > 2 else "all"
    os.makedirs(out, exist_ok=True)
    {"all": gen_all, "large": gen_large, "committed": gen_committed}[what](out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
