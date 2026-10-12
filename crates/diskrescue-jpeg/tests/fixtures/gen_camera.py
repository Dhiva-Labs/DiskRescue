#!/usr/bin/env python3
"""Realistic camera and phone JPEGs for the diskrescue-jpeg review tests.

Usage: gen_camera.py OUTDIR

Writes files that imitate what cameras and phones produce, built with
Pillow (libjpeg-turbo) plus hand-built metadata, and with minijpeg.py for
what Pillow cannot write (4:1:1, 4:4:0, mixed sampling, 12-bit, mozjpeg
overshoot deringing):

  cam_*    Exif (II/MM) of ~60 KB with a MakerNote holding junk IFD types,
           an embedded preview and stray FF D8 / FF D9 bytes; IFD1
           thumbnail; GPS; Interop IFD; MPF index + large preview after
           the EOI (optionally with padding before it); restart intervals.
  phone_*  Exif + XMP + ICC (Display-P3 sized, or 150 KB split over three
           APP2 segments) + MPF gain map / Samsung SEF / Google motion photo.
  ps_*     Photoshop APP13 with a 0x040C thumbnail, Adobe APP14, progressive.
  mj_*     minijpeg outputs.
  q100_*   extreme content at quality 100 (largest coefficients).

Every file is a complete, valid image: libjpeg decodes it, and the tests
expect `validate` to call it Valid and `measure` to return its exact
length. `manifest.txt` lists "name main_len" where main_len is the offset
just past the primary image's EOI.
"""
import io
import os
import random
import struct
import sys

from PIL import Image, ImageDraw, ImageFile

sys.dont_write_bytecode = True  # no __pycache__ inside the source tree
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import minijpeg  # noqa: E402

ImageFile.MAXBLOCK = 1 << 26


def photo(w, h, seed):
    r = random.Random(seed)
    g = Image.linear_gradient("L").resize((w, h))
    b = Image.radial_gradient("L").resize((w, h))
    n = Image.frombytes("L", (w, h), r.randbytes(w * h)).point(lambda v: 96 + v // 4)
    img = Image.merge("RGB", (g, b, n))
    d = ImageDraw.Draw(img)
    for _ in range(30):
        x0, y0 = r.randrange(w), r.randrange(h)
        x1 = x0 + r.randrange(1, max(2, w // 3))
        y1 = y0 + r.randrange(1, max(2, h // 3))
        col = (r.randrange(256), r.randrange(256), r.randrange(256))
        if r.random() < 0.5:
            d.ellipse([x0, y0, x1, y1], fill=col)
        else:
            d.rectangle([x0, y0, x1, y1], fill=col)
    d.text((5, 5), "DiskRescue camera", fill=(255, 255, 255))
    return img


def text_page(w, h, seed):
    """Black text and lines on white: the worst case for deringing."""
    r = random.Random(seed)
    img = Image.new("RGB", (w, h), (255, 255, 255))
    d = ImageDraw.Draw(img)
    for y in range(2, h - 8, 10):
        d.text((r.randrange(4), y), "".join(r.choice("AWMXQ#@%") for _ in range(w // 6)), fill=(0, 0, 0))
    for _ in range(6):
        d.line([r.randrange(w), r.randrange(h), r.randrange(w), r.randrange(h)], fill=(0, 0, 0), width=2)
    d.rectangle([w // 2, h // 2, w // 2 + 9, h // 2 + 9], fill=(255, 0, 0))
    return img


def jpeg_bytes(img, **kw):
    b = io.BytesIO()
    img.save(b, "JPEG", **kw)
    return b.getvalue()


# ---------------------------------------------------------------------------
# Segment surgery
# ---------------------------------------------------------------------------

def split_jpeg(jpg):
    """(header segments [(marker, payload)], rest from SOS on)."""
    assert jpg[:2] == b"\xff\xd8"
    p = 2
    segs = []
    while True:
        assert jpg[p] == 0xFF, p
        m = jpg[p + 1]
        if m == 0xDA:
            return segs, jpg[p:]
        ln = struct.unpack(">H", jpg[p + 2:p + 4])[0]
        segs.append((m, jpg[p + 4:p + 2 + ln]))
        p += 2 + ln


def seg(marker, payload):
    assert len(payload) + 2 <= 0xFFFF, len(payload)
    return bytes([0xFF, marker]) + struct.pack(">H", len(payload) + 2) + payload


def assemble(front, segs, rest, drop=(0xE0,)):
    """SOI + `front` segments + Pillow's segments (minus `drop`) + scan."""
    out = bytearray(b"\xff\xd8")
    for m, p in front:
        out += seg(m, p)
    for m, p in segs:
        if m not in drop:
            out += seg(m, p)
    out += rest
    return bytes(out)


# ---------------------------------------------------------------------------
# TIFF / Exif builder
# ---------------------------------------------------------------------------

class Tiff:
    """Builds a TIFF structure; offsets are relative to its start."""

    def __init__(self, big):
        self.e = ">" if big else "<"
        self.big = big

    def build(self, ifds, blobs):
        """ifds: list of (name, entries, next_name); entries are
        (tag, type, count, value) where value is int, bytes, ('ifd', name)
        or ('blob', name). blobs: dict name -> bytes."""
        e = self.e
        layout = {}
        pos = 8
        for name, entries, _ in ifds:
            layout[name] = pos
            pos += 2 + 12 * len(entries) + 4
        data_area = []
        out_of_line = {}
        for name, entries, _ in ifds:
            for (tag, typ, count, value) in entries:
                if isinstance(value, bytes) and len(value) > 4:
                    out_of_line[(name, tag)] = pos
                    data_area.append(value + (b"\0" if len(value) % 2 else b""))
                    pos += len(data_area[-1])
        for bname, b in blobs.items():
            layout[bname] = pos
            data_area.append(b + (b"\0" if len(b) % 2 else b""))
            pos += len(data_area[-1])
        t = bytearray((b"MM\0*" if self.big else b"II*\0") + struct.pack(e + "I", 8))
        for name, entries, nxt in ifds:
            t += struct.pack(e + "H", len(entries))
            for (tag, typ, count, value) in entries:
                t += struct.pack(e + "HHI", tag, typ, count)
                if isinstance(value, tuple):
                    t += struct.pack(e + "I", layout[value[1]])
                elif isinstance(value, bytes):
                    if len(value) > 4:
                        t += struct.pack(e + "I", out_of_line[(name, tag)])
                    else:
                        t += value.ljust(4, b"\0")
                elif typ == 3:
                    t += struct.pack(e + "HH", value, 0)
                else:
                    t += struct.pack(e + "I", value)
            t += struct.pack(e + "I", layout[nxt] if nxt else 0)
        for b in data_area:
            t += b
        return bytes(t), layout


def rational(e, pairs):
    return b"".join(struct.pack(e + "II", n, d) for n, d in pairs)


def makernote(kind, big, size, seed):
    """A MakerNote blob with junk types, absurd counts, an embedded
    preview JPEG and stray marker bytes."""
    r = random.Random(seed)
    e = ">" if big else "<"
    preview = jpeg_bytes(photo(48, 32, seed), quality=60)
    if kind == "nikon":
        head = b"Nikon\0\x02\x10\0\0" + (b"MM\0*" if big else b"II*\0") + struct.pack(e + "I", 8)
    elif kind == "apple":
        head = b"Apple iOS\0\0\x01MM"
    elif kind == "fuji":
        head = b"FUJIFILM" + struct.pack("<I", 12)
    else:
        head = b""
    n = 24
    body = struct.pack(e + "H", n)
    for i in range(n):
        typ = r.choice([1, 2, 3, 4, 5, 7, 0, 99, 0x1234, 13])
        count = r.choice([1, 4, 0xFFFFFFFF, r.randrange(1 << 20)])
        body += struct.pack(e + "HHII", 0x0001 + i, typ, count, r.randrange(1 << 32))
    body += struct.pack(e + "I", r.randrange(1 << 32))
    junk = bytearray(r.randbytes(max(0, size - len(head) - len(body) - len(preview))))
    for k in range(0, len(junk) - 4, 997):
        junk[k:k + 4] = r.choice([b"\xff\xd9\xff\xd8", b"\xff\xd8\xff\xe1", b"\xff\xda\x00\x0c", b"\xff\xc4\xff\xff"])
    return head + body + preview + bytes(junk)


def camera_exif(thumb, make, model, big, mn_kind, mn_size, w, h, seed, dt="2023:11:05 14:03:27"):
    e = ">" if big else "<"
    t = Tiff(big)
    dtb = dt.encode() + b"\0"
    ifd0 = [
        (0x010F, 2, len(make) + 1, make.encode() + b"\0"),
        (0x0110, 2, len(model) + 1, model.encode() + b"\0"),
        (0x0112, 3, 1, 1),
        (0x011A, 5, 1, rational(e, [(300, 1)])),
        (0x011B, 5, 1, rational(e, [(300, 1)])),
        (0x0128, 3, 1, 2),
        (0x0131, 2, 12, b"Firmware 1.2\0"[:12]),
        (0x0132, 2, 20, dtb),
        (0x0213, 3, 1, 2),
        (0x8769, 4, 1, ("ifd", "exif")),
        (0x8825, 4, 1, ("ifd", "gps")),
    ]
    exif = [
        (0x829A, 5, 1, rational(e, [(1, 250)])),
        (0x829D, 5, 1, rational(e, [(28, 10)])),
        (0x8827, 3, 1, 200),
        (0x9000, 7, 4, b"0232"),
        (0x9003, 2, 20, dtb),
        (0x9004, 2, 20, dtb),
        (0x9011, 2, 7, b"+01:00\0"),
        (0x9101, 7, 4, b"\x01\x02\x03\x00"),
        (0x927C, 7, mn_size, ("blob", "makernote")),
        (0x9286, 7, 264, b"ASCII\0\0\0" + b" " * 256),
        (0x9291, 2, 3, b"42\0"),
        (0xA000, 7, 4, b"0100"),
        (0xA001, 3, 1, 1),
        (0xA002, 4, 1, w),
        (0xA003, 4, 1, h),
        (0xA005, 4, 1, ("ifd", "interop")),
        (0xA420, 2, 33, b"0123456789abcdef0123456789abcdef\0"),
        (0xA434, 2, 14, b"Zoom 24-70mm\0\0"),
    ]
    interop = [(0x0001, 2, 4, b"R98\0"), (0x0002, 7, 4, b"0100")]
    gps = [
        (0x0000, 1, 4, b"\x02\x03\x00\x00"),
        (0x0001, 2, 2, b"N\0"),
        (0x0002, 5, 3, rational(e, [(48, 1), (51, 1), (2964, 100)])),
        (0x0003, 2, 2, b"E\0"),
        (0x0004, 5, 3, rational(e, [(2, 1), (17, 1), (4020, 100)])),
    ]
    ifd1 = [
        (0x0103, 3, 1, 6),
        (0x011A, 5, 1, rational(e, [(72, 1)])),
        (0x011B, 5, 1, rational(e, [(72, 1)])),
        (0x0128, 3, 1, 2),
        (0x0201, 4, 1, ("blob", "thumb")),
        (0x0202, 4, 1, len(thumb)),
    ]
    mn = makernote(mn_kind, big, mn_size, seed)
    tiff, _ = t.build(
        [("ifd0", ifd0, "ifd1"), ("exif", exif, None), ("interop", interop, None),
         ("gps", gps, None), ("ifd1", ifd1, None)],
        {"makernote": mn, "thumb": thumb},
    )
    return b"Exif\0\0" + tiff


def mpf_index(big, entries):
    """APP2 MPF payload; entries: (attribute, size, offset)."""
    e = ">" if big else "<"
    n = len(entries)
    off_entries = 8 + 2 + 3 * 12 + 4
    t = (b"MM\0*" if big else b"II*\0") + struct.pack(e + "I", 8)
    t += struct.pack(e + "H", 3)
    t += struct.pack(e + "HHI", 0xB000, 7, 4) + b"0100"
    t += struct.pack(e + "HHII", 0xB001, 4, 1, n)
    t += struct.pack(e + "HHII", 0xB002, 7, 16 * n, off_entries)
    t += struct.pack(e + "I", 0)
    for attr, size, off in entries:
        t += struct.pack(e + "IIIHH", attr, size, off, 0, 0)
    return b"MPF\0" + t


def mpf_attr(big, num):
    """APP2 MPF payload of a secondary image (MP Attribute IFD only)."""
    e = ">" if big else "<"
    t = (b"MM\0*" if big else b"II*\0") + struct.pack(e + "I", 8)
    t += struct.pack(e + "H", 2)
    t += struct.pack(e + "HHI", 0xB000, 7, 4) + b"0100"
    t += struct.pack(e + "HHII", 0xB101, 4, 1, num)
    t += struct.pack(e + "I", 0)
    return b"MPF\0" + t


def with_mpf(front_before, front_after, segs, rest, extras, big, pad=b"", drop=(0xE0,)):
    """Primary image with an MPF index (placed after `front_before`) listing
    `extras` (list of (attribute, jpeg bytes)) appended after its EOI
    (with `pad` before the first one)."""
    placeholder = mpf_index(big, [(0x20030000, 0, 0)] + [(a, 0, 0) for a, _ in extras])
    front = list(front_before) + [(0xE2, placeholder)] + list(front_after)
    main = assemble(front, segs, rest, drop)
    # Offset of the MPF TIFF header: SOI + segments before it + marker/len + "MPF\0".
    t_off = 2 + sum(4 + len(p) for _, p in front_before) + 4 + 4
    entries = [(0x20030000, len(main), 0)]
    pos = len(main) + len(pad)
    for attr, b in extras:
        entries.append((attr, len(b), pos - t_off))
        pos += len(b)
    real = mpf_index(big, entries)
    assert len(real) == len(placeholder)
    front = list(front_before) + [(0xE2, real)] + list(front_after)
    main = assemble(front, segs, rest, drop)
    return main, main + pad + b"".join(b for _, b in extras)


def secondary(img, quality, big, num, **kw):
    segs, rest = split_jpeg(jpeg_bytes(img, quality=quality, **kw))
    return assemble([(0xE2, mpf_attr(big, num))], segs, rest)


def xmp_payload(body):
    return b"http://ns.adobe.com/xap/1.0/\0" + body.encode()


def fake_icc(size, seed):
    r = random.Random(seed)
    head = struct.pack(">I", size) + b"appl" + b"\x04\x00\x00\x00mntrRGB XYZ " + bytes(12) + b"acspAPPL"
    body = bytearray(r.randbytes(size - len(head)))
    for k in range(0, len(body) - 4, 1499):
        body[k:k + 4] = b"\xff\xd9\xff\xd8"
    return head + bytes(body)


def fake_mp4(payload, seed):
    r = random.Random(seed)
    ftyp = struct.pack(">I", 24) + b"ftypmp42" + struct.pack(">I", 0) + b"isommp42"
    moov = struct.pack(">I", 40) + b"moov" + struct.pack(">I", 32) + b"mvhd" + bytes(24)
    mdat = struct.pack(">I", 8 + payload) + b"mdat" + r.randbytes(payload)
    return ftyp + mdat + moov


def sef_trailer(start, blocks):
    """Samsung SEF blocks + SEFH directory + SEFT; `start` is the offset of
    the first block in the final file. blocks: (type, name, payload)."""
    body = b""
    starts = []
    for typ, name, payload in blocks:
        starts.append(start + len(body))
        body += struct.pack("<HHI", 0, typ, len(name)) + name + payload
    h = start + len(body)
    d = b"SEFH" + struct.pack("<II", 106, len(blocks))
    for (typ, name, payload), s in zip(blocks, starts):
        d += struct.pack("<HHII", 0, typ, h - s, 8 + len(name) + len(payload))
    return body + d + struct.pack("<I", len(d)) + b"SEFT"


def photoshop_app13(thumb):
    res = b"8BIM" + struct.pack(">H", 0x0404) + b"\0\0" + struct.pack(">I", 6) + b"\x1c\x02\x00\x00\x02\x00"
    th = struct.pack(">IIIIIIHH", 1, 160, 120, 480, 57600, len(thumb), 24, 1) + thumb
    res += b"8BIM" + struct.pack(">H", 0x040C) + b"\0\0" + struct.pack(">I", len(th)) + th
    if len(th) % 2:
        res += b"\0"
    return b"Photoshop 3.0\0" + res


# ---------------------------------------------------------------------------
# The set
# ---------------------------------------------------------------------------

def gen(out):
    manifest = []

    def write(name, data, main_len=None):
        with open(os.path.join(out, name), "wb") as f:
            f.write(data)
        manifest.append("%s %d" % (name, main_len if main_len is not None else len(data)))

    # Canon-like: II Exif with a 40 KB MakerNote, thumbnail, MPF VGA preview,
    # 4:2:2, no restart markers.
    img = photo(1024, 680, 1)
    thumb = jpeg_bytes(photo(160, 106, 2), quality=70)
    ex = camera_exif(thumb, "Canon", "Canon EOS R6", False, "canon", 40000, 1024, 680, 3)
    segs, rest = split_jpeg(jpeg_bytes(img, quality=95, subsampling=1))
    prev = secondary(photo(640, 425, 4), 85, False, 2)
    main, full = with_mpf([(0xE1, ex)], [], segs, rest, [(0x00010001, prev)], False)
    write("cam_canon_422.jpg", full, len(main))

    # Nikon-like: MM Exif with Nikon MakerNote (inner TIFF + preview), DRI,
    # MPF preview after 16 bytes of padding.
    img = photo(960, 640, 5)
    thumb = jpeg_bytes(photo(160, 120, 6), quality=75)
    ex = camera_exif(thumb, "NIKON CORPORATION", "NIKON Z 6_2", True, "nikon", 55000, 960, 640, 7)
    segs, rest = split_jpeg(jpeg_bytes(img, quality=93, subsampling=1, restart_marker_blocks=4))
    prev = secondary(photo(640, 426, 8), 80, True, 2)
    main, full = with_mpf([(0xE1, ex)], [], segs, rest, [(0x00010001, prev)], True, pad=b"\0" * 16)
    write("cam_nikon_rst_pad.jpg", full, len(main))

    # Fuji-like: II Exif + FUJIFILM MakerNote, 4:2:0, restart every MCU row,
    # two MPF images (VGA + full HD previews).
    img = photo(800, 600, 9)
    thumb = jpeg_bytes(photo(160, 120, 10), quality=80)
    ex = camera_exif(thumb, "FUJIFILM", "X-T5", False, "fuji", 30000, 800, 600, 11)
    segs, rest = split_jpeg(jpeg_bytes(img, quality=90, restart_marker_rows=1))
    p1 = secondary(photo(640, 480, 12), 80, False, 2)
    p2 = secondary(photo(400, 300, 13), 75, False, 3)
    main, full = with_mpf([(0xE1, ex)], [], segs, rest, [(0x00010001, p1), (0x00010002, p2)], False)
    write("cam_fuji_420_rows.jpg", full, len(main))

    # Old point-and-shoot: JFIF APP0 kept + Exif, 4:2:2, standard tables, no MPF.
    img = photo(640, 480, 14)
    thumb = jpeg_bytes(photo(160, 120, 15), quality=60)
    ex = camera_exif(thumb, "OLYMPUS IMAGING CORP.", "u-7000", False, "canon", 8000, 640, 480, 16)
    segs, rest = split_jpeg(jpeg_bytes(img, quality=88, subsampling=1))
    write("cam_compact_jfif_exif.jpg", assemble([], segs[:1] + [(0xE1, ex)] + segs[1:], rest, drop=()))

    # Samsung-like phone: Exif + XMP + ICC + MPF? no; motion photo with MP4
    # straight after the EOI and a SEF directory (Image_UTC_Data,
    # MotionPhoto_Data), 4:2:0 q95.
    img = photo(1008, 756, 17)
    thumb = jpeg_bytes(photo(320, 240, 18), quality=80)
    ex = camera_exif(thumb, "samsung", "SM-S918B", False, "canon", 4000, 1008, 756, 19)
    xmp = xmp_payload(
        '<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">'
        '<rdf:Description xmlns:GCamera="http://ns.google.com/photos/1.0/camera/" '
        'GCamera:MotionPhoto="1" GCamera:MotionPhotoVersion="1" GCamera:MotionPhotoPresentationTimestampUs="0"/>'
        "</rdf:RDF></x:xmpmeta>")
    icc = fake_icc(548, 20)
    segs, rest = split_jpeg(jpeg_bytes(img, quality=95))
    main = assemble([(0xE1, ex), (0xE1, xmp), (0xE2, b"ICC_PROFILE\0\x01\x01" + icc)], segs, rest)
    mp4 = fake_mp4(60000, 21)
    tail = sef_trailer(len(main) + len(mp4), [(0x0A01, b"Image_UTC_Data", b"1699193007000"),
                                               (0x0AA1, b"MCC_Data", b"310"),
                                               (0x0A30, b"MotionPhoto_Data", b"")])
    write("phone_samsung_motion.jpg", main + mp4 + tail, len(main))

    # Older Samsung: classic SEF where MotionPhoto_Data holds the MP4.
    img = photo(800, 600, 22)
    segs, rest = split_jpeg(jpeg_bytes(img, quality=92, exif=camera_exif(
        thumb, "samsung", "SM-G973F", False, "canon", 3000, 800, 600, 23)))
    main = assemble([], segs, rest)
    tail = sef_trailer(len(main), [(0x0A30, b"MotionPhoto_Data", fake_mp4(40000, 24)),
                                   (0x0A01, b"Image_UTC_Data", b"1599193007000")])
    write("phone_samsung_sef.jpg", main + tail, len(main))

    # Apple-like: MM Exif with Apple MakerNote, 150 KB ICC split over three
    # APP2 segments, XMP, MPF with an HDR gain map.
    img = photo(1008, 756, 25)
    thumb = jpeg_bytes(photo(160, 120, 26), quality=70)
    ex = camera_exif(thumb, "Apple", "iPhone 15 Pro", True, "apple", 2500, 1008, 756, 27)
    segs, rest = split_jpeg(jpeg_bytes(img, quality=92, icc_profile=fake_icc(150000, 28),
                                       xmp=b'<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF/></x:xmpmeta>'))
    gain = secondary(photo(504, 378, 29).convert("L"), 80, True, 2,
                     xmp=b'<x:xmpmeta xmlns:x="adobe:ns:meta/" HDRGainMap:HDRGainMapVersion="65536"/>')
    main, full = with_mpf([(0xE1, ex)], [], segs, rest, [(0x00000000, gain)], True)
    write("phone_apple_gainmap_icc3.jpg", full, len(main))

    # Pixel-like Ultra HDR + motion photo: MPF gain map, then the MP4.
    img = photo(1000, 750, 30)
    mp4 = fake_mp4(50000, 31)
    gain_img = photo(250, 188, 32).convert("L")
    gain = secondary(gain_img, 85, False, 2)
    xmp = xmp_payload(
        '<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">'
        '<rdf:Description xmlns:Container="http://ns.google.com/photos/1.0/container/" '
        'xmlns:Item="http://ns.google.com/photos/1.0/container/item/" '
        'xmlns:GCamera="http://ns.google.com/photos/1.0/camera/" GCamera:MotionPhoto="1" '
        'GCamera:MotionPhotoVersion="1"><Container:Directory><rdf:Seq>'
        '<rdf:li rdf:parseType="Resource"><Container:Item Item:Mime="image/jpeg" Item:Semantic="Primary"/></rdf:li>'
        '<rdf:li rdf:parseType="Resource"><Container:Item Item:Mime="image/jpeg" Item:Semantic="GainMap" Item:Length="%d"/></rdf:li>'
        '<rdf:li rdf:parseType="Resource"><Container:Item Item:Mime="video/mp4" Item:Semantic="MotionPhoto" Item:Length="%d"/></rdf:li>'
        "</rdf:Seq></Container:Directory></rdf:Description></rdf:RDF></x:xmpmeta>" % (len(gain), len(mp4)))
    ex = camera_exif(thumb, "Google", "Pixel 8", False, "canon", 1500, 1000, 750, 33)
    segs, rest = split_jpeg(jpeg_bytes(img, quality=95))
    main, full = with_mpf([(0xE1, ex), (0xE1, xmp)], [], segs, rest, [(0x00000000, gain)], False)
    write("phone_pixel_ultrahdr_motion.jpg", full + mp4, len(main))

    # Photoshop-saved: APP13 with a 0x040C thumbnail, Adobe APP14, ICC in
    # three segments, progressive with optimised tables.
    img = photo(900, 600, 34)
    th = jpeg_bytes(photo(160, 107, 35), quality=60)
    segs, rest = split_jpeg(jpeg_bytes(img, quality=85, progressive=True, optimize=True,
                                       icc_profile=fake_icc(140000, 36)))
    adobe = b"Adobe" + struct.pack(">HHHB", 100, 0, 0, 1)
    write("ps_progressive_app13.jpg", assemble([(0xED, photoshop_app13(th)), (0xEE, adobe)], segs, rest))

    # CMYK with ICC (Pillow writes Adobe APP14 transform 0, inverted CMYK).
    write("ps_cmyk_icc.jpg", jpeg_bytes(photo(320, 240, 37).convert("CMYK"), quality=90,
                                        icc_profile=fake_icc(70000, 38)))
    write("ps_cmyk_progressive.jpg", jpeg_bytes(photo(240, 180, 39).convert("CMYK"), quality=80, progressive=True))
    # RGB without colour transform (Adobe transform 0).
    write("ps_keep_rgb.jpg", jpeg_bytes(photo(256, 192, 40), quality=90, keep_rgb=True))
    write("ps_keep_rgb_prog.jpg", jpeg_bytes(photo(256, 192, 41), quality=90, keep_rgb=True, progressive=True))

    # Progressive variants with Exif thumbnails and restart intervals.
    img = photo(1024, 768, 42)
    thumb = jpeg_bytes(photo(160, 120, 43), quality=70)
    ex = camera_exif(thumb, "SONY", "ILCE-7M4", False, "canon", 20000, 1024, 768, 44)
    write("cam_sony_progressive.jpg", jpeg_bytes(img, quality=90, progressive=True, exif=ex))
    write("cam_progressive_444_rst.jpg", jpeg_bytes(photo(400, 300, 45), quality=95, progressive=True,
                                                    subsampling=0, restart_marker_blocks=3))
    write("cam_progressive_rst1.jpg", jpeg_bytes(photo(320, 200, 46), quality=85, progressive=True,
                                                 restart_marker_blocks=1))
    write("cam_rst1_444.jpg", jpeg_bytes(photo(320, 200, 47), quality=97, subsampling=0, restart_marker_blocks=1))

    # Extreme content at quality 100: the largest coefficients there are.
    w, h = 256, 192
    r = random.Random(48)
    noise = Image.frombytes("RGB", (w, h), r.randbytes(w * h * 3))
    write("q100_noise_444.jpg", jpeg_bytes(noise, quality=100, subsampling=0))
    write("q100_noise_420_prog.jpg", jpeg_bytes(noise, quality=100, progressive=True))
    cb = Image.new("L", (w, h))
    cb.putdata([255 if ((x + y) % 2) else 0 for y in range(h) for x in range(w)])
    write("q100_checker_gray.jpg", jpeg_bytes(cb, quality=100))
    bars = Image.new("RGB", (w, h))
    bars.putdata([(255, 0, 255) if (x // 4 + y // 4) % 2 else (0, 255, 0) for y in range(h) for x in range(w)])
    write("q100_bars_444.jpg", jpeg_bytes(bars, quality=100, subsampling=0))
    for q in (40, 75, 95):
        write("text_q%d.jpg" % q, jpeg_bytes(text_page(320, 240, 49 + q), quality=q))

    # minijpeg: sampling factors Pillow cannot write, 12-bit, deringing.
    small = photo(150, 100, 60)
    pl = minijpeg.rgb_planes(small)
    write("mj_411.jpg", minijpeg.encode(pl, 150, 100, [(4, 1), (1, 1), (1, 1)], quality=90))
    write("mj_440_rst.jpg", minijpeg.encode(pl, 150, 100, [(1, 2), (1, 1), (1, 1)], quality=85, restart=5))
    write("mj_mixed_2x1_1x2.jpg", minijpeg.encode(pl, 150, 100, [(2, 1), (1, 2), (1, 1)], quality=88))
    write("mj_422_vert_odd.jpg", minijpeg.encode(pl, 150, 100, [(2, 2), (2, 1), (1, 1)], quality=80))
    g12 = [[v * 16 + (x * 7 + y * 3) % 16 for x, v in enumerate(row)] for y, row in enumerate(minijpeg.gray_plane(small))]
    write("mj_12bit_gray.jpg", minijpeg.encode([g12], 150, 100, [(1, 1)], precision=12, quality=92))
    p12 = [[[v * 16 + 8 for v in row] for row in plane] for plane in pl]
    write("mj_12bit_420_rst.jpg", minijpeg.encode(p12, 150, 100, [(2, 2), (1, 1), (1, 1)], precision=12,
                                                    quality=95, restart=4))
    tp = text_page(160, 120, 61)
    tpl = minijpeg.rgb_planes(tp)
    for q in (50, 60, 70, 80, 85, 90, 95):
        write("mj_dering_q%d.jpg" % q, minijpeg.encode(tpl, 160, 120, [(2, 2), (1, 1), (1, 1)],
                                                       quality=q, deringing=True))
    write("mj_dering_gray_q75.jpg", minijpeg.encode([minijpeg.gray_plane(tp)], 160, 120, [(1, 1)],
                                                    quality=75, deringing=True))

    # Donor transplant: a victim and donors that do or do not fit it.
    vex = camera_exif(thumb, "Canon", "Canon EOS 5D", False, "canon", 6000, 320, 240, 90)
    base = dict(quality=90, subsampling=1, restart_marker_blocks=4, exif=vex)
    donors = {
        "dv_victim_422_rst4.jpg": (photo(320, 240, 91), base),
        "dv_ok.jpg": (photo(320, 240, 92), base),
        "dv_quality75.jpg": (photo(320, 240, 93), dict(base, quality=75)),
        "dv_sub420.jpg": (photo(320, 240, 94), dict(base, subsampling=2)),
        "dv_sub444.jpg": (photo(320, 240, 95), dict(base, subsampling=0)),
        "dv_larger_480x360.jpg": (photo(480, 360, 96), base),
        "dv_smaller_320x200.jpg": (photo(320, 200, 97), base),
        "dv_progressive.jpg": (photo(320, 240, 98), dict(base, progressive=True)),
        "dv_no_dri.jpg": (photo(320, 240, 99), dict(base, restart_marker_blocks=0)),
        "dv_dri8.jpg": (photo(320, 240, 100), dict(base, restart_marker_blocks=8)),
        "dv_optimised.jpg": (photo(320, 240, 101), dict(base, optimize=True)),
        "dv_gray.jpg": (photo(320, 240, 102).convert("L"), dict(base)),
        # Victim without restart markers and donors for it.
        "dw_victim_420.jpg": (photo(320, 240, 103), dict(quality=85)),
        "dw_ok.jpg": (photo(320, 240, 104), dict(quality=85)),
        "dw_optimised.jpg": (photo(320, 240, 105), dict(quality=85, optimize=True)),
        "dw_larger_352x240.jpg": (photo(352, 240, 106), dict(quality=85)),
        "dw_sub422.jpg": (photo(320, 240, 107), dict(quality=85, subsampling=1)),
    }
    for name, (img, kw) in donors.items():
        b = io.BytesIO()
        img.save(b, "JPEG", **kw)
        with open(os.path.join(out, name), "wb") as f:
            f.write(b.getvalue())

    # Small images with full-size metadata, for the mutation tests.
    thumb = jpeg_bytes(photo(80, 60, 70), quality=60)
    ex = camera_exif(thumb, "NIKON CORPORATION", "NIKON D850", True, "nikon", 20000, 160, 120, 71)
    segs, rest = split_jpeg(jpeg_bytes(photo(160, 120, 72), quality=90, subsampling=1, restart_marker_blocks=2))
    prev = secondary(photo(64, 48, 73), 70, True, 2)
    main, full = with_mpf([(0xE1, ex)], [], segs, rest, [(0x00010001, prev)], True, pad=b"\0" * 8)
    write("small_cam_mpf_rst.jpg", full, len(main))
    gain = secondary(photo(40, 30, 74).convert("L"), 70, False, 2)
    mp4 = fake_mp4(3000, 75)
    xmp = xmp_payload('<x:xmpmeta xmlns:x="adobe:ns:meta/" GCamera:MotionPhoto="1" GCamera:MicroVideoOffset="%d"/>'
                      % len(mp4))
    segs, rest = split_jpeg(jpeg_bytes(photo(120, 90, 76), quality=85, icc_profile=fake_icc(70000, 77)))
    ex = camera_exif(thumb, "Google", "Pixel 7", False, "canon", 1200, 120, 90, 78)
    main, full = with_mpf([(0xE1, ex), (0xE1, xmp)], [], segs, rest, [(0x00000000, gain)], False)
    write("small_phone_gain_motion.jpg", full + mp4, len(main))
    th = jpeg_bytes(photo(40, 30, 79), quality=50)
    segs, rest = split_jpeg(jpeg_bytes(photo(100, 75, 80), quality=80, progressive=True, restart_marker_blocks=3))
    adobe = b"Adobe" + struct.pack(">HHHB", 100, 0, 0, 1)
    write("small_ps_prog_rst.jpg", assemble([(0xED, photoshop_app13(th)), (0xEE, adobe)], segs, rest))

    with open(os.path.join(out, "manifest.txt"), "w") as f:
        f.write("\n".join(manifest) + "\n")


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    os.makedirs(sys.argv[1], exist_ok=True)
    gen(sys.argv[1])
    return 0


if __name__ == "__main__":
    sys.exit(main())
