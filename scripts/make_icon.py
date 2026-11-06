#!/usr/bin/env python3
"""Gera o ícone do app (icns) a partir de um desenho simples.

Uso: scripts/make_icon.py [saida.icns]
Requer Pillow (pip) e iconutil (macOS).
"""
import os
import shutil
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw, ImageFont

SIZE = 1024
BG = (13, 17, 23, 255)
BORDER = (52, 63, 79, 255)
ACCENT = (96, 165, 250, 255)
TEXT = (200, 210, 220, 255)

FONTS = [
    "/System/Library/Fonts/Menlo.ttc",
    "/System/Library/Fonts/SFNSMono.ttf",
    "/System/Library/Fonts/Monaco.ttf",
]


def base_image() -> Image.Image:
    img = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)
    margin, radius = 96, 190
    draw.rounded_rectangle(
        [margin, margin, SIZE - margin, SIZE - margin],
        radius=radius,
        fill=BG,
        outline=BORDER,
        width=8,
    )
    font = None
    for path in FONTS:
        if os.path.exists(path):
            font = ImageFont.truetype(path, 520)
            break
    prompt = ">"
    if font is not None:
        draw.text((250, 240), prompt, font=font, fill=ACCENT)
    else:  # fallback sem fonte: dois traços
        draw.line([(280, 380), (460, 512), (280, 644)], fill=ACCENT, width=48)
    # cursor
    draw.rounded_rectangle([590, 600, 760, 700], radius=16, fill=TEXT)
    return img


def write_icns(target: str) -> None:
    base = base_image()
    iconset = tempfile.mkdtemp(suffix=".iconset")
    for size in (16, 32, 64, 128, 256, 512, 1024):
        img = base.resize((size, size), Image.LANCZOS)
        img.save(os.path.join(iconset, f"icon_{size}x{size}.png"))
        if size <= 512:
            img2 = base.resize((size * 2, size * 2), Image.LANCZOS)
            img2.save(os.path.join(iconset, f"icon_{size}x{size}@2x.png"))
    os.makedirs(os.path.dirname(os.path.abspath(target)), exist_ok=True)
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", target], check=True)
    shutil.rmtree(iconset, ignore_errors=True)


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else "assets/icon.icns"
    write_icns(out)
    print(out)
