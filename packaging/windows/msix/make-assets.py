#!/usr/bin/env python3
"""Renders the Microsoft Store package images (packaging/windows/msix/Assets) and
the Store listing logos (packaging/windows/msix/store-logos) from the app icon.
Needs python3-gi with librsvg and python3-cairo. Run again after changing the icon."""

from pathlib import Path

import gi

gi.require_version("Rsvg", "2.0")
from gi.repository import Rsvg  # noqa: E402
import cairo  # noqa: E402

ROOT = Path(__file__).resolve().parents[3]
ICON = ROOT / "packaging/linux/diskrescue.svg"
HERE = Path(__file__).resolve().parent
ASSETS = HERE / "Assets"
LOGOS = HERE / "store-logos"


def draw_icon(ctx: cairo.Context, x: float, y: float, size: float) -> None:
    handle = Rsvg.Handle.new_from_file(str(ICON))
    viewport = Rsvg.Rectangle()
    viewport.x, viewport.y = x, y
    viewport.width = viewport.height = size
    handle.render_document(ctx, viewport)


def render(path: Path, width: int, height: int, icon: int) -> None:
    """The icon, `icon` px square, centred on a transparent `width`×`height` image."""
    surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, width, height)
    draw_icon(cairo.Context(surface), (width - icon) / 2, (height - icon) / 2, icon)
    surface.write_to_png(str(path))


def render_art(path: Path, width: int, height: int, icon: int) -> None:
    """The icon on a soft blue backdrop with a shadow under it (Store box and poster art)."""
    surface = cairo.ImageSurface(cairo.FORMAT_RGB24, width, height)
    ctx = cairo.Context(surface)
    bg = cairo.LinearGradient(0, 0, 0, height)
    bg.add_color_stop_rgb(0, 0.94, 0.96, 1.0)
    bg.add_color_stop_rgb(1, 0.80, 0.87, 0.97)
    ctx.set_source(bg)
    ctx.paint()
    x, y = (width - icon) / 2, (height - icon) / 2
    # The card's foot sits at about 92% of the icon's height.
    cx, cy = width / 2, y + icon * 0.95
    ctx.save()
    ctx.translate(cx, cy)
    ctx.scale(icon * 0.42, icon * 0.05)
    shadow = cairo.RadialGradient(0, 0, 0, 0, 0, 1)
    shadow.add_color_stop_rgba(0, 0.15, 0.22, 0.35, 0.40)
    shadow.add_color_stop_rgba(1, 0.15, 0.22, 0.35, 0.0)
    ctx.set_source(shadow)
    ctx.arc(0, 0, 1, 0, 6.2832)
    ctx.fill()
    ctx.restore()
    draw_icon(ctx, x, y, icon)
    surface.write_to_png(str(path))


def main() -> None:
    ASSETS.mkdir(exist_ok=True)
    for old in ASSETS.glob("*.png"):
        old.unlink()
    for scale in (100, 200, 400):
        f = scale / 100
        render(ASSETS / f"StoreLogo.scale-{scale}.png", round(50 * f), round(50 * f), round(50 * f))
        render(ASSETS / f"Square44x44Logo.scale-{scale}.png", round(44 * f), round(44 * f), round(36 * f))
        render(ASSETS / f"Square150x150Logo.scale-{scale}.png", round(150 * f), round(150 * f), round(96 * f))
        render(ASSETS / f"Wide310x150Logo.scale-{scale}.png", round(310 * f), round(150 * f), round(96 * f))
    # Taskbar, Start and file icons, without the coloured tile plate.
    for size in (16, 20, 24, 30, 32, 36, 40, 48, 60, 64, 72, 80, 96, 256):
        for qualifiers in (f"targetsize-{size}", f"targetsize-{size}_altform-unplated"):
            render(ASSETS / f"Square44x44Logo.{qualifiers}.png", size, size, size)

    LOGOS.mkdir(exist_ok=True)
    render(LOGOS / "app-tile-icon-300.png", 300, 300, 300)
    render_art(LOGOS / "box-art-1080.png", 1080, 1080, 640)
    render_art(LOGOS / "poster-art-1440x2160.png", 1440, 2160, 760)


if __name__ == "__main__":
    main()
