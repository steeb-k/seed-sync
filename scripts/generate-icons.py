#!/usr/bin/env python3
"""Regenerate SEED Sync's committed icon deliverables from the hand-authored
per-size PNGs under ``img/``.

Source art (``img/``, one file per authored size):
  seed-appicon-<sz>px.png       -> the application icon (full-bleed tile)
  seed-trayicon-<sz>px.png      -> the system-tray glyph, standard variant
                                   (tuned for a dark panel background)
  seed-trayicon-<sz>px-light.png -> the system-tray glyph, "light" variant
                                   (tuned for a light panel background)

Deliverables written by this script:
  icon/appIcon.png          master app PNG (Linux hicolor + macOS .icns are
                            regenerated from this at package time)
  icon/appIcon.ico          multi-resolution Windows app icon, embedding the
                            hand-authored 16/32/48/64/128/256 renders
  icon/appTrayDark.png      tray master, standard variant  (decoded + rescaled
                            at runtime; shown on dark panels)
  icon/appTrayLight.png     tray master, light variant     (decoded + rescaled
                            at runtime; shown on light panels)
  android/app/src/main/res/mipmap-*dpi/ic_launcher{,_round,_foreground}.png
                            per-density Android launcher bitmaps

The build/packaging pipeline (build.rs, package-linux.sh, package-macos.sh,
tray.rs) consumes the icon/ files; nothing consumes img/ directly. Re-run this
whenever the img/ art changes:

    python3 scripts/generate-icons.py        # needs Pillow

It is intentionally idempotent and only touches the paths listed above.
"""
from __future__ import annotations

import io
import struct
import sys
from pathlib import Path

try:
    from PIL import Image
except ImportError:  # pragma: no cover
    sys.exit("generate-icons.py: requires Pillow (`pip install Pillow`)")

ROOT = Path(__file__).resolve().parent.parent
IMG = ROOT / "img"
ICON = ROOT / "icon"
ANDROID_RES = ROOT / "android" / "app" / "src" / "main" / "res"

# Windows .ico slots — every one is a distinct hand-authored render, not a
# downscale, so the small sizes keep their tuned legibility.
ICO_SIZES = [16, 32, 48, 64, 128, 256]

# Android densities: (dir suffix, dp->px scale). Launcher (legacy) art is 48dp;
# adaptive foreground/background is 108dp full-bleed.
ANDROID_DENSITIES = {
    "mdpi": 1.0,
    "hdpi": 1.5,
    "xhdpi": 2.0,
    "xxhdpi": 3.0,
    "xxxhdpi": 4.0,
}


def app_src(size: int) -> Path:
    """Nearest authored app-tile render at or above ``size`` (else the largest)."""
    authored = sorted(
        int(p.stem.split("-")[-1].removesuffix("px"))
        for p in IMG.glob("seed-appicon-*px.png")
    )
    for s in authored:
        if s >= size:
            return IMG / f"seed-appicon-{s}px.png"
    return IMG / f"seed-appicon-{authored[-1]}px.png"


def load_app(size: int) -> Image.Image:
    src = app_src(size)
    im = Image.open(src).convert("RGBA")
    if im.size != (size, size):
        im = im.resize((size, size), Image.LANCZOS)
    return im


def write_png(im: Image.Image, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    im.save(path, "PNG")
    print(f"  {path.relative_to(ROOT)}  ({im.size[0]}x{im.size[1]})")


# ---------------------------------------------------------------------------
# Windows multi-resolution .ico
#
# Pillow's ICO save downscales from a single frame, which would throw away the
# hand-tuned small renders. So we pack the directory by hand: a real 32bpp BMP
# (BGRA, bottom-up, empty AND mask) per authored size < 256, and a PNG payload
# for 256. Both forms are decoded by every Windows >= 7 and by the winresource
# resource compiler.
# ---------------------------------------------------------------------------
def _bmp_entry(im: Image.Image) -> bytes:
    w, h = im.size
    px = im.load()
    header = struct.pack(
        "<IiiHHIIiiII",
        40,        # biSize
        w,         # biWidth
        h * 2,     # biHeight (XOR image + AND mask)
        1,         # biPlanes
        32,        # biBitCount
        0,         # biCompression (BI_RGB)
        w * h * 4, # biSizeImage
        0, 0, 0, 0,
    )
    body = bytearray()
    for y in range(h - 1, -1, -1):  # bottom-up
        for x in range(w):
            r, g, b, a = px[x, y]
            body += bytes((b, g, r, a))
    # AND mask: 1bpp, rows padded to 32 bits. Left all-zero — transparency is
    # carried by the BGRA alpha channel.
    mask_row = ((w + 31) // 32) * 4
    body += b"\x00" * (mask_row * h)
    return header + bytes(body)


def build_ico(path: Path) -> None:
    entries = []  # (width, height, payload)
    for size in ICO_SIZES:
        im = load_app(size)
        if size >= 256:
            buf = io.BytesIO()
            im.save(buf, "PNG")
            payload = buf.getvalue()
        else:
            payload = _bmp_entry(im)
        entries.append((size, size, payload))

    out = bytearray()
    out += struct.pack("<HHH", 0, 1, len(entries))  # ICONDIR: reserved, type=1, count
    offset = 6 + 16 * len(entries)
    for w, h, payload in entries:
        out += struct.pack(
            "<BBBBHHII",
            w & 0xFF,          # width  (0 == 256)
            h & 0xFF,          # height (0 == 256)
            0,                 # palette count
            0,                 # reserved
            1,                 # color planes
            32,                # bits per pixel
            len(payload),
            offset,
        )
        offset += len(payload)
    for _, _, payload in entries:
        out += payload

    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(out)
    print(f"  {path.relative_to(ROOT)}  (sizes: {', '.join(map(str, ICO_SIZES))})")


def circular(im: Image.Image) -> Image.Image:
    """Apply an antialiased circular mask (for the legacy round launcher)."""
    size = im.size[0]
    ss = size * 4
    mask = Image.new("L", (ss, ss), 0)
    from PIL import ImageDraw

    ImageDraw.Draw(mask).ellipse((0, 0, ss - 1, ss - 1), fill=255)
    mask = mask.resize((size, size), Image.LANCZOS)
    out = im.copy()
    out.putalpha(mask)
    return out


def build_android() -> None:
    for suffix, scale in ANDROID_DENSITIES.items():
        d = ANDROID_RES / f"mipmap-{suffix}"
        launcher_px = round(48 * scale)
        fg_px = round(108 * scale)
        tile = load_app(launcher_px)
        write_png(tile, d / "ic_launcher.png")
        write_png(circular(tile), d / "ic_launcher_round.png")
        # Adaptive foreground is full-bleed (108dp); the solid-colour background
        # layer (@color/ic_launcher_background) fills the mask's corners.
        write_png(load_app(fg_px), d / "ic_launcher_foreground.png")


def _tray_size(path: Path) -> int:
    """Parse the pixel size out of a tray render's filename (``...-<sz>px[-light]``)."""
    return int(path.stem.replace("-light", "").split("-")[-1].removesuffix("px"))


def tray_src(light: bool) -> Path:
    """Largest authored tray render for the given variant (light or standard).

    The "-light" files are the light-panel variant; everything else is the
    standard (dark-panel) glyph. The plain ``*px.png`` glob would also catch the
    ``*px-light.png`` names, so filter those out for the standard variant.
    """
    if light:
        cands = list(IMG.glob("seed-trayicon-*px-light.png"))
    else:
        cands = [
            p for p in IMG.glob("seed-trayicon-*px.png")
            if not p.stem.endswith("-light")
        ]
    return max(cands, key=_tray_size)


def main() -> None:
    print("app master + tray masters:")
    ICON.mkdir(parents=True, exist_ok=True)
    write_png(load_app(1024), ICON / "appIcon.png")
    write_png(Image.open(tray_src(light=False)).convert("RGBA"),
              ICON / "appTrayDark.png")
    write_png(Image.open(tray_src(light=True)).convert("RGBA"),
              ICON / "appTrayLight.png")
    print("windows .ico:")
    build_ico(ICON / "appIcon.ico")
    print("android launcher bitmaps:")
    build_android()
    print("done.")


if __name__ == "__main__":
    main()
