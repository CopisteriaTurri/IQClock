#!/usr/bin/env python3
"""Genera le icone di IQClock (serve Pillow: `pip install pillow`).

`assets/iqclock.png` è l'immagine usata a runtime per la finestra e la taskbar,
`assets/iqclock.ico` è la stessa immagine in 16–256 px incorporata nell'exe
Windows da `build.rs`. Generandole insieme non possono divergere.
"""

import io
import math
import struct
from pathlib import Path

from PIL import Image, ImageDraw

S = 4096  # disegno in grande e riduco: bordi puliti a ogni misura
BLU_SU = (0, 150, 214)  # blu HP in alto
BLU_GIU = (0, 90, 152)  # blu più scuro in basso
BIANCO = (255, 255, 255, 255)
MISURE = (16, 24, 32, 48, 64, 128, 256)
# Sotto i 48 px l'anello e le lancette fini si impastano: uso tratti più grossi
# e niente tacche.
PICCOLE = 48

CARTELLA = Path(__file__).resolve().parent / "assets"


def disegna(semplice: bool) -> Image.Image:
    """Icona su 1024: quadrante bianco su quadrato blu arrotondato."""

    def px(x: float) -> int:
        return int(round(x * S / 1024.0))

    img = Image.new("RGBA", (S, S), (0, 0, 0, 0))

    # Sfondo: quadrato arrotondato con gradiente verticale.
    maschera = Image.new("L", (S, S), 0)
    ImageDraw.Draw(maschera).rounded_rectangle(
        [0, 0, S - 1, S - 1], radius=px(230), fill=255
    )
    sfondo = Image.new("RGBA", (S, S))
    sd = ImageDraw.Draw(sfondo)
    for y in range(S):
        t = y / (S - 1)
        sd.line(
            [(0, y), (S, y)],
            fill=tuple(int(a + (b - a) * t) for a, b in zip(BLU_SU, BLU_GIU)) + (255,),
        )
    img.paste(sfondo, (0, 0), maschera)

    d = ImageDraw.Draw(img)
    cx, cy = px(512), px(500)

    def barra(x0: float, y0: float, x1: float, y1: float, largo: int) -> None:
        """Segmento bianco con estremità arrotondate."""
        d.line([(x0, y0), (x1, y1)], fill=BIANCO, width=largo)
        r = largo / 2
        for x, y in ((x0, y0), (x1, y1)):
            d.ellipse([x - r, y - r, x + r, y + r], fill=BIANCO)

    # Quadrante: anello bianco.
    r, spessore = (300, 100) if semplice else (292, 62)
    d.ellipse([cx - px(r), cy - px(r), cx + px(r), cy + px(r)], outline=BIANCO, width=px(spessore))

    # Tacche a 12, 3, 6 e 9.
    if not semplice:
        for ang in (0, 90, 180, 270):
            a = math.radians(ang)
            barra(
                cx + math.sin(a) * px(196),
                cy - math.cos(a) * px(196),
                cx + math.sin(a) * px(248),
                cy - math.cos(a) * px(248),
                px(30),
            )

    # Lancette: ore alle 10, minuti alle 2.
    lancette = ((-60, 150, 96), (60, 196, 84)) if semplice else ((-60, 152, 64), (60, 210, 54))
    for ang, lungo, largo in lancette:
        a = math.radians(ang)
        barra(cx, cy, cx + math.sin(a) * px(lungo), cy - math.cos(a) * px(lungo), px(largo))

    # Perno centrale.
    r = px(70 if semplice else 48)
    d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=BIANCO)
    return img


def in_ico(misura: int, img: Image.Image) -> bytes:
    """Un fotogramma per l'ICO: PNG per 256 px, BMP a 32 bit per gli altri
    (i fotogrammi BMP li leggono anche le API e gli strumenti più vecchi)."""
    im = img.resize((misura, misura), Image.LANCZOS)
    if misura == 256:
        buf = io.BytesIO()
        im.save(buf, format="PNG", optimize=True)
        return buf.getvalue()
    w = h = misura
    pixel = im.load()
    xor = bytearray()
    for y in range(h - 1, -1, -1):  # dal basso verso l'alto
        for x in range(w):
            r, g, b, a = pixel[x, y]
            xor += bytes((b, g, r, a))
    # Maschera AND: un bit a 1 dove il pixel è trasparente, righe da 4 byte.
    riga = ((w + 31) // 32) * 4
    and_mask = bytearray(riga * h)
    for y in range(h):
        for x in range(w):
            if pixel[x, y][3] < 128:
                and_mask[(h - 1 - y) * riga + x // 8] |= 0x80 >> (x % 8)
    testa = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, len(xor), 0, 0, 0, 0)
    return testa + bytes(xor) + bytes(and_mask)


grande = disegna(semplice=False)
piccola = disegna(semplice=True)
grande.resize((256, 256), Image.LANCZOS).save(CARTELLA / "iqclock.png", optimize=True)

fotogrammi = [
    (m, in_ico(m, piccola if m < PICCOLE else grande)) for m in MISURE
]
voci = b""
spostamento = 6 + 16 * len(fotogrammi)
for misura, dati in fotogrammi:
    voci += struct.pack(
        "<BBBBHHII",
        0 if misura == 256 else misura,  # 0 significa 256
        0 if misura == 256 else misura,
        0,  # colori nella tavolozza
        0,  # riservato
        1,  # piani
        32,  # bit per pixel
        len(dati),
        spostamento,
    )
    spostamento += len(dati)
(CARTELLA / "iqclock.ico").write_bytes(
    struct.pack("<HHH", 0, 1, len(fotogrammi)) + voci + b"".join(d for _, d in fotogrammi)
)
print(f"scritte {CARTELLA / 'iqclock.png'} e {CARTELLA / 'iqclock.ico'}")
