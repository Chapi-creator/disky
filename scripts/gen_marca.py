#!/usr/bin/env python3
"""Genera la marca de disky: el icono de la app y el banner del README.

La marca («ticker»): una caja —el disco— con tres barras que suben y una flecha
que no para de subir. El icono, el banner y el dibujo de la cabecera de la app
salen de la misma geometría, así que la marca se reconoce igual en los tres
sitios. Si la marca cambia, cambia aquí una vez y se regenera todo.

Renderiza en grande (supersampling) y exporta:
- src-tauri/icons/: icon.ico multi-tamaño (16..256), icon.png (512), 32/128/@2x
  y los logos Windows (StoreLogo, Square*)
- docs/logo/logo.png: la marca sobre su baldosa (para el README y la web)
- docs/logo/wordmark.png: la marca + «disky ▲» + el lema, para la portada

Uso: python scripts/gen_marca.py  (desde la raíz del repo)
"""
import math
import os

from PIL import Image, ImageDraw, ImageFont

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
ICO_DIR = os.path.join(ROOT, "src-tauri", "icons")
LOGO = os.path.join(ROOT, "docs", "logo")

# Tokens de src/styles.css (fuente de verdad de la paleta). Al tocar uno, tocar
# el otro: la marca no puede decir una cosa en el icono y otra en la pantalla.
BORDER = (31, 40, 51, 255)  # --border        #1f2833
BORDER_FUERTE = (46, 58, 74, 255)  # --border-fuerte #2e3a4a
SURFACE = (14, 19, 25, 255)  # --surface       #0e1319
ACENTO = (242, 178, 60, 255)  # --acento        #f2b23c
ALZA = (47, 220, 117, 255)  # --alza          #2fdc75
TEXT = (233, 238, 246, 255)  # --text          #e9eef6
TEXT_DIM = (137, 150, 168, 255)  # --text-dim      #8996a8

S = 2048  # lienzo maestro (supersampling)

# Geometría de la marca en un cuadrado de 100×100 unidades.
CAJA = (15, 15, 85, 85)
CAJA_RADIO = 15
TRAZO = 6  # el trazo del interfaz es de 2 px; en un icono de 16 px no se ve
BARRAS = ((24, 60), (44, 50), (64, 40))  # x de cada barra y su altura superior
BARRAS_BASE = 78
BARRA_ANCHO = 12
FLECHA = ((26, 49), (41, 37), (53, 41), (72, 23))
FLECHA_GROSOR = 6.5
CABEZA_LARGO = 11
CABEZA_ANCHO = 6.5

# Banner de portada (el wordmark). Todo en px de 1x; el export lo dobla.
BANNER_W = 780
BANNER_H = 272
BANNER_RELLENO = 44
BANNER_MARCA = 176
BANNER_TITULO = 118
BANNER_LEMA = 30
BANNER_AIRE = 52  # hueco entre la marca y el texto
BANNER_ESCALA = 2

# Cascadia Mono es la fuente monoespaciada de la casa (la de la app). Si no
# está, la primera que aparezca; si no, la de PIL (el banner queda más pobre).
FUENTES = (
    "C:/Windows/Fonts/CascadiaMono.ttf",
    "C:/Windows/Fonts/consola.ttf",
    "/System/Library/Fonts/Menlo.ttc",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
)


def draw_mark(d, x, y, size, trazo=TRAZO):
    """Dibuja la marca en un cuadrado de `size` px con la esquina en (x, y).

    `trazo` y la geometría van en unidades del cuadrado de 100, no en píxeles:
    así el mismo dibujo sirve para un icono de 16 px y para un banner de 780.
    """
    u = size / 100.0

    def pt(ux, uy):
        return (x + ux * u, y + uy * u)

    def box(x0, y0, x1, y1):
        return [x + x0 * u, y + y0 * u, x + x1 * u, y + y1 * u]

    def grosor(units):
        return max(1, round(units * u))

    # La caja (el disco), con trazo ámbar.
    d.rounded_rectangle(
        box(*CAJA), radius=round(CAJA_RADIO * u), outline=ACENTO, width=grosor(trazo)
    )

    # Barras que suben: la primera en ámbar (la marca), las siguientes en verde.
    for i, (bx, top) in enumerate(BARRAS):
        d.rounded_rectangle(
            box(bx, top, bx + BARRA_ANCHO, BARRAS_BASE),
            radius=grosor(2.5),
            fill=ACENTO if i == 0 else ALZA,
        )

    # La flecha: por encima de las barras, con punta redondeada y cabeza abierta.
    d.line(
        [pt(*p) for p in FLECHA], fill=TEXT, width=grosor(FLECHA_GROSOR), joint="curve"
    )
    radio = grosor(FLECHA_GROSOR) / 2
    for ux, uy in (FLECHA[0], FLECHA[-1]):  # PIL no redondea las puntas
        cx, cy = pt(ux, uy)
        d.ellipse([cx - radio, cy - radio, cx + radio, cy + radio], fill=TEXT)

    (ax, ay), (bx, by) = FLECHA[-2], FLECHA[-1]
    dx, dy = bx - ax, by - ay
    largo = math.hypot(dx, dy) or 1
    ux, uy = dx / largo, dy / largo
    nx, ny = -uy, ux  # normal al último tramo: hacia dónde abren las barbas
    base = (bx - ux * CABEZA_LARGO, by - uy * CABEZA_LARGO)
    punta = pt(bx, by)
    for lado in (1, -1):
        d.line(
            [punta, pt(base[0] + nx * CABEZA_ANCHO * lado, base[1] + ny * CABEZA_ANCHO * lado)],
            fill=TEXT,
            width=grosor(FLECHA_GROSOR),
        )


def tile(size):
    """La marca sobre su baldosa: así se despega de cualquier fondo."""
    img = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    u = size / 100.0
    d.rounded_rectangle([0, 0, size, size], radius=round(22 * u), fill=BORDER_FUERTE)
    d.rounded_rectangle(
        [2.5 * u, 2.5 * u, 97.5 * u, 97.5 * u], radius=round(20 * u), fill=SURFACE
    )
    draw_mark(d, 0, 0, size)
    return img


def load_font(size):
    """Cascadia Mono si está; si no, la primera monoespaciada que exista."""
    for path in FUENTES:
        if os.path.exists(path):
            return ImageFont.truetype(path, size)
    print("aviso: sin fuente monoespaciada del sistema; el banner usará la de PIL")
    return ImageFont.load_default(size)


def triangle(d, left, bottom, ancho, alto, fill):
    d.polygon(
        [(left, bottom), (left + ancho, bottom), (left + ancho / 2, bottom - alto)],
        fill=fill,
    )


def wordmark():
    """Banner de portada: la marca + «disky ▲» + el lema, sobre la superficie."""
    k = BANNER_ESCALA
    w, h = BANNER_W * k, BANNER_H * k
    img = Image.new("RGBA", (w, h), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle(
        [0, 0, w - 1, h - 1], radius=24 * k, fill=SURFACE, outline=BORDER, width=2 * k
    )

    marca = BANNER_MARCA * k
    draw_mark(d, BANNER_RELLENO * k, (h - marca) // 2, marca)

    titulo = load_font(BANNER_TITULO * k)
    lema = load_font(BANNER_LEMA * k)
    x = (BANNER_RELLENO + BANNER_MARCA + BANNER_AIRE) * k

    # Centrado por la tinta real: la fuente reserva huecos de ascendente y
    # descendente que el texto no ocupa, y centrar por ellos deja el bloque
    # visible descentrado. `getbbox` mide desde el origen ascendente, que es
    # justo el que usa `text` por defecto (anchor «la»).
    caja_t = titulo.getbbox("disky")
    caja_l = lema.getbbox("El disco como bolsa")
    alto_t, alto_l = caja_t[3] - caja_t[1], caja_l[3] - caja_l[1]
    hueco = 18 * k
    y = (h - (alto_t + hueco + alto_l)) // 2
    origen_t = y - caja_t[1]
    origen_l = y + alto_t + hueco - caja_l[1]

    d.text(
        (x, origen_t),
        "disky",
        font=titulo,
        fill=TEXT,
        stroke_fill=TEXT,
        stroke_width=2 * k,
    )

    # La flecha verde del wordmark, algo levantada sobre la línea base del
    # título: la misma que el `h1::after` de la app.
    base_t = origen_t + titulo.getmetrics()[0]
    triangle(
        d,
        x + titulo.getlength("disky") + 16 * k,
        base_t - round(BANNER_TITULO * 0.10) * k,
        42 * k,
        38 * k,
        ALZA,
    )

    d.text((x, origen_l), "El disco como bolsa", font=lema, fill=TEXT_DIM)
    return img


def main():
    os.makedirs(ICO_DIR, exist_ok=True)
    os.makedirs(LOGO, exist_ok=True)
    master = tile(S)

    def save_png(path, px):
        master.resize((px, px), Image.LANCZOS).save(path)

    # icon.ico multi-tamaño. Pillow genera los frames él solo a partir del
    # maestro: `append_images` lo ignora el plugin de ICO y saldría uno solo.
    master.save(
        os.path.join(ICO_DIR, "icon.ico"),
        format="ICO",
        sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)],
    )

    for px, name in [
        (512, "icon.png"),
        (128, "128x128.png"),
        (256, "128x128@2x.png"),
        (32, "32x32.png"),
        (48, "StoreLogo.png"),
        (44, "Square44x44Logo.png"),
        (71, "Square71x71Logo.png"),
        (89, "Square89x89Logo.png"),
        (107, "Square107x107Logo.png"),
        (142, "Square142x142Logo.png"),
        (150, "Square150x150Logo.png"),
        (284, "Square284x284Logo.png"),
        (310, "Square310x310Logo.png"),
        (30, "Square30x30Logo.png"),
    ]:
        save_png(os.path.join(ICO_DIR, name), px)
    save_png(os.path.join(LOGO, "logo.png"), 1024)
    wordmark().save(os.path.join(LOGO, "wordmark.png"))
    print("Marca generada en", ICO_DIR, "y", LOGO)


if __name__ == "__main__":
    main()
