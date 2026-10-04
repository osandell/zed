#!/usr/bin/env python3
"""Build zelda-light-zed-v2.png, the zelda-light skin's atlas, from the reference.

zelda-light-reference.png is the user's reference picture of Zed in this theme:
parchment panels in plain gold rails. It is a rendered window with live text in
its panels, so the skin samples only frame margins from it plus a text-free
patch of the project panel for every panel interior. The editor's scrollbar is
painted over first, so its right rail can be repeated down the whole panel.
Below the reference this appends a gold patch (the tab bar behind the tabs) and
one row per arcoscope collection:

  x   0  the inactive tab, its face rebuilt text-free, tinted to the collection
  x 120  the bottom strip, tinted and brought to that tab face's brightness

arcoscope.json's `zelda-light` binding samples this image; its rectangles are in
the reference's pixels (reference_width 1938). The filter matches
tint-dreamweb.py: luminance onto a dark -> accent -> light ramp.

    python3 assets/images/window-skins/build-zelda-light.py

Use a new output filename when the pixels change so Zed's bitmap cache is
invalidated, and update arcoscope.json and any ~/.config/zed/arcoscope-themes.json
override that names the file.
"""

from pathlib import Path

from PIL import Image, ImageOps

HERE = Path(__file__).resolve().parent
SOURCE = HERE / "zelda-light-reference.png"
OUTPUT = HERE / "zelda-light-zed-v2.png"

# The arcoscope zelda-light collection colours (BitmapSkin.zeldaLight.accents).
ACCENTS = ["#c26402", "#4b826b", "#8e5c83", "#4a86a3", "#7b752c"]

# The editor's scrollbar (x0 y0 x1 y1), rebuilt from a text-free pixel column
# of the editor just left of it.
SCROLLBAR = (1582, 60, 1607, 788)
SCROLLBAR_COLUMN_X = 1576

# The first terminal tab and the text-free column right of its gear and name.
TAB = (14, 5, 103, 50)
TAB_FACE_COLUMN = (64, 5, 40, 50)
TAB_FACE_SPAN = (20, 111)  # absolute x rebuilt from the column
# A plain stretch of the bottom rail; the skin repeats it sideways (fill_mode
# horizontal).
STRIP = (600, 792, 60, 17)
# The rail's flat middle rows, stretched into a patch for the tab bar behind
# the tabs.
GOLD = (600, 794, 60, 9)
GOLD_SIZE = (256, 56)

GOLD_TOP = 820
ROW_TOP = GOLD_TOP + GOLD_SIZE[1] + 8
ROW_HEIGHT = 64
STRIP_X = 120
GOLD_X = 220


def crop(image: Image.Image, rect) -> Image.Image:
    x, y, w, h = rect
    return image.crop((x, y, x + w, y + h))


def clean_scrollbar(image: Image.Image) -> None:
    x0, y0, x1, y1 = SCROLLBAR
    column = image.crop((SCROLLBAR_COLUMN_X, y0, SCROLLBAR_COLUMN_X + 1, y1))
    image.paste(column.resize((x1 - x0, y1 - y0), Image.NEAREST), (x0, y0))


def clean_tab(image: Image.Image) -> Image.Image:
    """The tab with its face rebuilt from the column averaged into one pixel
    column, so no grain or blotch repeats across it."""
    tab = crop(image, TAB)
    cx, cy, cw, ch = TAB_FACE_COLUMN
    column = crop(image, TAB_FACE_COLUMN).resize((1, ch), Image.BOX)
    left, right = TAB_FACE_SPAN
    tab.paste(column.resize((right - left, ch), Image.NEAREST), (left - TAB[0], 0))
    return tab


def tint(tile: Image.Image, accent: str, midpoint: int) -> Image.Image:
    alpha = tile.getchannel("A")
    grey = ImageOps.grayscale(tile.convert("RGB"))
    out = ImageOps.colorize(grey, black="#0b1a22", mid=accent, white="#fbf6e6", midpoint=midpoint)
    out.putalpha(alpha)
    return out


def mean_luminance(image: Image.Image) -> float:
    grey = ImageOps.grayscale(image.convert("RGB"))
    return sum(grey.tobytes()) / (grey.width * grey.height)


def match_brightness(image: Image.Image, target: float) -> Image.Image:
    gain = target / max(mean_luminance(image), 1.0)
    alpha = image.getchannel("A")
    out = image.convert("RGB").point(lambda value: min(255, round(value * gain)))
    out.putalpha(alpha)
    return out


def main() -> None:
    source = Image.open(SOURCE).convert("RGBA")
    clean_scrollbar(source)
    height = ROW_TOP + ROW_HEIGHT * len(ACCENTS)
    atlas = Image.new("RGBA", (source.width, height), (0, 0, 0, 0))
    atlas.paste(source, (0, 0))
    tab, strip = clean_tab(source), crop(source, STRIP)
    atlas.paste(tab, (0, GOLD_TOP))
    atlas.paste(crop(source, GOLD).resize(GOLD_SIZE, Image.BICUBIC), (GOLD_X, GOLD_TOP))
    for index, accent in enumerate(ACCENTS):
        top = ROW_TOP + ROW_HEIGHT * index
        # Parchment sits near the top of the ramp: a high midpoint lands the face
        # on the accent itself.
        tinted_tab = tint(tab, accent, midpoint=240)
        atlas.paste(tinted_tab, (0, top))
        face = tinted_tab.crop((20, 8, 90, 44))
        atlas.paste(match_brightness(tint(strip, accent, midpoint=200), mean_luminance(face)), (STRIP_X, top))
    atlas.save(OUTPUT, optimize=True)
    print(f"{OUTPUT.name}: {atlas.width}x{atlas.height}")


if __name__ == "__main__":
    main()
