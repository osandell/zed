#!/usr/bin/env python3
"""Build dreamweb-zed-v5.png: v3 plus one colour-tinted copy per winman collection.

The skin draws the selected tab and the bottom strip from these tinted copies
while their side of the window holds focus (surfaces `tab_active@<n>` and
`bottom_strip@<n>` in winman.json). They are the same bitmaps as the neutral
ones, recoloured with a colour filter, so the texture, rivets and bevels stay.

The filter maps each pixel's luminance onto a black -> accent -> light ramp
(PIL's ImageOps.colorize) after a 1% autocontrast, which keeps the metal's
shading and recolours only its hue. Accents are winman's Dreamweb collection
colours (DreamwebStyle.accents in winman-gui-swift), in collection order
ö p b t g.

Rerun after changing the neutral artwork or an accent:

    python3 assets/images/window-skins/tint-dreamweb.py

and update the rectangles in winman.json if ROW_* changes. Use a new output
filename when the pixels change so Zed's bitmap cache is invalidated.
"""

from pathlib import Path

from PIL import Image, ImageOps

HERE = Path(__file__).resolve().parent
SOURCE = HERE / "dreamweb-zed-v3.png"
OUTPUT = HERE / "dreamweb-zed-v5.png"

ACCENTS = ["#c66e24", "#2c625a", "#6a4662", "#2c4a5a", "#6c6836"]

# Neutral source rectangles in v3 (x, y, width, height): the grey tab tile's
# frame rect (`tab_inactive`) and the bottom strip's fill (`bottom_strip`).
TAB = (468, 4, 438, 56)
STRIP = (38, 802, 900, 9)

# Where the tinted copies go: one row per collection below the v3 artwork.
ROW_TOP = 820
ROW_HEIGHT = 60
STRIP_X = 450


def tint(tile: Image.Image, accent: str) -> Image.Image:
    alpha = tile.getchannel("A")
    grey = ImageOps.autocontrast(ImageOps.grayscale(tile.convert("RGB")), cutoff=1)
    out = ImageOps.colorize(grey, black="#050403", mid=accent, white="#f4e6d0", midpoint=110)
    out.putalpha(alpha)
    return out


def mean_luminance(image: Image.Image) -> float:
    grey = ImageOps.grayscale(image.convert("RGB"))
    return sum(grey.getdata()) / (grey.width * grey.height)


def match_brightness(image: Image.Image, target: float) -> Image.Image:
    """Scale `image` so its mean luminance equals `target`, keeping hue and texture."""
    gain = target / max(mean_luminance(image), 1.0)
    alpha = image.getchannel("A")
    out = image.convert("RGB").point(lambda value: min(255, round(value * gain)))
    out.putalpha(alpha)
    return out


def crop(image: Image.Image, rect: tuple[int, int, int, int]) -> Image.Image:
    x, y, w, h = rect
    return image.crop((x, y, x + w, y + h))


def main() -> None:
    source = Image.open(SOURCE).convert("RGBA")
    height = ROW_TOP + ROW_HEIGHT * len(ACCENTS)
    atlas = Image.new("RGBA", (source.width, height), (0, 0, 0, 0))
    atlas.paste(source, (0, 0))
    tab, strip = crop(source, TAB), crop(source, STRIP)
    for index, accent in enumerate(ACCENTS):
        top = ROW_TOP + ROW_HEIGHT * index
        tinted_tab = tint(tab, accent)
        atlas.paste(tinted_tab, (0, top))
        # The strip is a thin, bright rail; tinted as is it glows next to the
        # tab. Bring it to the tab face's mean luminance so the two read as one
        # colour at one brightness.
        face = tinted_tab.crop((7, 6, 7 + 424, 6 + 43))
        atlas.paste(match_brightness(tint(strip, accent), mean_luminance(face)), (STRIP_X, top))
    atlas.save(OUTPUT, optimize=True)
    print(f"{OUTPUT.name}: {atlas.width}x{atlas.height}")


if __name__ == "__main__":
    main()
