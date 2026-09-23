#!/usr/bin/env python3
"""Genera el icono de disky: treemap con la paleta de la app.

Renderiza una maqueta vectorial en 2048 px (supersampling) y exporta:
- icon.ico multi-tamaño (16..256), icon.png (512), 32/128/@2x
- logos Windows (StoreLogo, Square*) y docs/logo/logo.png

Uso: python scripts/gen_icon.py  (desde la raíz del repo)
"""
import os
from PIL import Image, ImageDraw, ImageFont

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
ICO_DIR = os.path.join(ROOT, "src-tauri", "icons")
LOGO = os.path.join(ROOT, "docs", "logo")

BG = (15, 17, 21, 255)          # --bg #0f1115
RING = (38, 43, 54, 255)        # --border #262b36
BLUE = (79, 140, 255, 255)      # --accent #4f8cff
BLUE_LIT = (156, 192, 255, 255)
GREEN = (63, 157, 99, 255)      # creció #3f9d63
GREEN_LIT = (123, 216, 143, 255)
ORANGE = (196, 127, 46, 255)    # encogió #c47f2e
ORANGE_LIT = (224, 154, 74, 255)
NEUTRAL = (58, 65, 82, 255)     # #3a4152

S = 2048  # lienzo maestro (supersampling)


def rounded(d, box, radius, fill):
    d.rounded_rectangle(box, radius=radius, fill=fill)


def triangle(d, points, fill):
    d.polygon(points, fill=fill)


def draw_at(size):
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    k = size / S  # escala desde el maestro 2048

    def K(v):
        return round(v * k)

    m = K(104)  # margen exterior
    r_out = K(96)
    g = K(22)   # gap entre bloques
    r_blk = K(42)

    # Fondo + anillo sutil
    rounded(d, [0, 0, size, size], r_out, RING)
    rounded(d, [K(10), K(10), size - K(10), size - K(10)], r_out - K(10), BG)

    # Bloques treemap (cuadriláteros con gap)
    x0, y0 = m, m
    x1, y1 = size - m, size - m
    blocks = [
        (BLUE,  (x0, y0, x0 + (x1 - x0) * 0.40, y1)),
        (GREEN, (x0 + (x1 - x0) * 0.40 + g, y0, x1, y0 + (y1 - y0) * 0.60)),
        (ORANGE, (x0 + (x1 - x0) * 0.40 + g, y0 + (y1 - y0) * 0.60 + g,
                  x0 + (x1 - x0) * 0.40 + g + (x1 - x0) * 0.34, y1 - g)),
        (NEUTRAL, (x0 + (x1 - x0) * 0.40 + g + (x1 - x0) * 0.34 + g, y0 + (y1 - y0) * 0.60 + g,
                   x1, y1 - g)),
    ]
    for fill, box in blocks:
        rounded(d, box, r_blk, fill)

    # Flecha de crecimiento en el bloque azul
    cx = x0 + (x1 - x0) * 0.40 / 2
    cy = (y0 + y1) / 2
    tri_w = (x1 - x0) * 0.40 * 0.42
    tri_h = tri_w * 0.72
    triangle(d, [(cx - tri_w / 2, cy + tri_h / 2), (cx, cy - tri_h / 2),
                 (cx + tri_w / 2, cy + tri_h / 2)], BLUE_LIT)
    # Línea base
    line_w = tri_w * 0.34
    thick = K(16)
    d.rounded_rectangle([cx - line_w / 2, cy + tri_h / 2 + K(14),
                         cx + line_w / 2, cy + tri_h / 2 + K(14) + thick],
                        radius=thick // 2, fill=BLUE_LIT)

    # Indicadores de crecimiento en bloques verde (arriba) y naranja (abajo)
    gb = blocks[1][1]
    tw = (gb[2] - gb[0]) * 0.10
    triangle(d, [(gb[0] + tw, gb[1] + tw * 2.1), (gb[0] + tw * 2, gb[1] + tw * 2.1),
                 (gb[0] + tw * 1.5, gb[1] + tw * 0.7)], GREEN_LIT)

    ob = blocks[2][1]
    ow = (ob[2] - ob[0]) * 0.12
    triangle(d, [(ob[0] + ow, ob[1] + ow * 0.8), (ob[0] + ow * 2, ob[1] + ow * 0.8),
                 (ob[0] + ow * 1.5, ob[1] + ow * 2.2)], ORANGE_LIT)
    return img


def main():
    master = draw_at(S)
    os.makedirs(ICO_DIR, exist_ok=True)
    os.makedirs(LOGO, exist_ok=True)

    def save_png(path, px):
        master.resize((px, px), Image.LANCZOS).save(path)

    # icon.ico multi-tamaño (cada frame a su propia resolución)
    ico_sizes = [(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)]
    frames = [master.resize(s, Image.LANCZOS) for s in ico_sizes]
    frames[0].save(os.path.join(ICO_DIR, "icon.ico"),
                   format="ICO", append_images=frames[1:])

    for px, name in [(512, "icon.png"), (128, "128x128.png"),
                     (256, "128x128@2x.png"), (32, "32x32.png"),
                     (48, "StoreLogo.png"), (44, "Square44x44Logo.png"),
                     (71, "Square71x71Logo.png"), (89, "Square89x89Logo.png"),
                     (107, "Square107x107Logo.png"), (142, "Square142x142Logo.png"),
                     (150, "Square150x150Logo.png"), (284, "Square284x284Logo.png"),
                     (310, "Square310x310Logo.png"), (30, "Square30x30Logo.png")]:
        save_png(os.path.join(ICO_DIR, name), px)
    save_png(os.path.join(LOGO, "logo.png"), 1024)
    print("Iconos generados en", ICO_DIR)


if __name__ == "__main__":
    main()