#!/usr/bin/env python3
"""Render share/icons/toastty.svg into the PNG and .icns files the
install scripts copy.

The rendered files are committed so `make install` needs no SVG tooling.
Re-run this after editing the SVG:

    pip install cairosvg pillow
    python3 scripts/render_icons.py
"""

import io
from pathlib import Path

import cairosvg
from PIL import Image

ICONS = Path(__file__).resolve().parent.parent / "share" / "icons"
SVG = ICONS / "toastty.svg"
# freedesktop hicolor sizes the Linux installer copies.
PNG_SIZES = (16, 24, 32, 48, 64, 128, 256, 512)


def render(size: int) -> Image.Image:
    data = cairosvg.svg2png(url=str(SVG), output_width=size, output_height=size)
    return Image.open(io.BytesIO(data)).convert("RGBA")


def main() -> None:
    png_dir = ICONS / "png"
    png_dir.mkdir(exist_ok=True)
    for size in PNG_SIZES:
        render(size).save(png_dir / f"toastty-{size}.png", optimize=True)
    # Pillow builds every icns slot (16 to 512@2x) from the 1024px master.
    render(1024).save(ICONS / "toastty.icns")


if __name__ == "__main__":
    main()
