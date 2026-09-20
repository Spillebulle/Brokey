#!/usr/bin/env python3
"""The mark, the icons and the banners, from the palette rather than by hand.

    python3 tools/make-art.py

Writes assets/icons/brokey-{16,32,48,64,128,256,512}.png, brokey.ico,
the four files crates/brokey/icons/ that Tauri's bundler names, and, via
Design-Principles' banner.py (a copy is in tools/), docs/images/banner.png,
banner-paper.png and social.png.

The colour is not typed in: it is the dark theme's `--accent` for this
application's hue, `oklch(0.674 0.101 300)`, converted here with the same
arithmetic the browser uses, so a change to the hue in tokens.css and a rerun
of this script cannot disagree. The mark is a filled rounded square, corner
radius 22 % of the side, no glyph (STYLE-GUIDE.md §17.4).

Requires Pillow.
"""
from __future__ import annotations

import math
import re
import sys
from pathlib import Path

from PIL import Image, ImageDraw

sys.path.insert(0, str(Path(__file__).resolve().parent))
import banner  # noqa: E402  the house banner arithmetic, copied from Design-Principles

ROOT = Path(__file__).resolve().parent.parent
TOKENS = ROOT / "frontend/src/tokens.css"


def accent_hue() -> float:
    m = re.search(r"--accent-h:\s*([0-9.]+)", TOKENS.read_text(encoding="utf-8"))
    if not m:
        sys.exit("tokens.css has no --accent-h")
    return float(m.group(1))


def oklch_to_srgb(L: float, C: float, h: float) -> tuple[int, int, int]:
    """Björn Ottosson's OKLab, the transform CSS Color 4 specifies."""
    a = C * math.cos(math.radians(h))
    b = C * math.sin(math.radians(h))
    l_ = L + 0.3963377774 * a + 0.2158037573 * b
    m_ = L - 0.1055613458 * a - 0.0638541728 * b
    s_ = L - 0.0894841775 * a - 1.2914855480 * b
    l, m, s = l_**3, m_**3, s_**3
    r = 4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s
    g = -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s
    bl = -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s

    def gamma(x: float) -> int:
        x = min(1.0, max(0.0, x))
        v = 1.055 * x ** (1 / 2.4) - 0.055 if x > 0.0031308 else 12.92 * x
        return int(round(v * 255))

    return gamma(r), gamma(g), gamma(bl)


def mark(size: int, colour: tuple[int, int, int], scale: int = 8) -> Image.Image:
    """Drawn large and reduced, so the corner is smooth at 16 px too."""
    big = size * scale
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    ImageDraw.Draw(img).rounded_rectangle(
        [0, 0, big - 1, big - 1], radius=int(big * 0.22), fill=(*colour, 255)
    )
    return img.resize((size, size), Image.LANCZOS)


# WiX's stock dialog set takes exactly these two sizes and no others.
# 24-bit BMP because that is what Windows Installer's Binary table reads;
# a PNG here shows as a blank rectangle with no error anywhere.
BANNER = (493, 58)
DIALOG = (493, 312)

# Where the banner stops being light. ProgressDlg's transparent title is a
# control 330 dialog units wide starting at 20, which is 27..466 px, nearly
# the whole strip. It is left-aligned and holds "Installing Brokey", so the
# glyphs stop well short of a third of it. A judgement, not a measurement of
# the control: if a title ever did run that far the failure is text over the
# mark, which is ugly, where the sidebar's figure below would be unreadable.
BANNER_SPLIT = 300

# Where the welcome and exit field stops being dark. This is the load-bearing
# number. WelcomeEulaDlg, the whole of WixUI_Minimal's first page, draws its
# transparent title at dialog x 130, which is 173 px; ExitDialog, FatalError
# and UserExit use 135, or 180 px. 168 clears the tighter of the two by five
# pixels.
DIALOG_SIDEBAR = 168

BLOCK_MARGIN = 12      # around the brand group inside the banner's dark block
SIDEBAR_MARGIN = 24    # either side of the wordmark in the sidebar
SIDEBAR_MARK = 88      # the mark's side in the sidebar
SIDEBAR_GAP = 22       # under the mark, before the wordmark


def fitted_group(font: Path, text: str, box_w: float, box_h: float) -> float:
    """The cap height at which the mark and the wordmark fill `box`.

    `banner.fit` is not used: it measures against the banner's canvas alone,
    on purpose, so it would answer with a mark three times the height of the
    strip this is drawing.
    """
    probe = 100.0
    row_w = probe * banner.MARK_PER_CAP + probe * banner.GAP_PER_CAP + banner.ink_width(font, text, probe)
    row_h = probe * banner.MARK_PER_CAP
    return probe * min(box_w / row_w, box_h / row_h)


def installer_art(images: dict[int, "Image.Image"]) -> None:
    """The WiX banner and dialog bitmaps.

    Light where MSI writes its black transparent titles, dark everywhere else.
    See BANNER_SPLIT and DIALOG_SIDEBAR above: this is the whole reason the
    two pictures are shaped the way they are.
    """
    out = ROOT / "packaging" / "windows"
    out.mkdir(parents=True, exist_ok=True)
    font = ROOT / "assets/fonts/Archivo.ttf"
    paper, _ = banner.GROUNDS["banner-paper.png"]
    dark, ink = banner.GROUNDS["banner.png"]

    # The banner: paper across the strip, a dark block on the right carrying
    # the brand group as a row.
    strip = Image.new("RGB", BANNER, paper)
    block_w = BANNER[0] - BANNER_SPLIT
    cap = fitted_group(font, "BROKEY", block_w - BLOCK_MARGIN * 2, BANNER[1] - BLOCK_MARGIN * 2)
    block = Image.new("RGB", (block_w, BANNER[1]), dark)
    mark_h = int(round(cap * banner.MARK_PER_CAP))
    word, baseline_in_word = banner.word_layer(font, "BROKEY", cap, ink)
    group_w = mark_h + cap * banner.GAP_PER_CAP + word.width
    left = (block_w - group_w) / 2
    middle = BANNER[1] / 2
    glyph = images[512].resize((mark_h, mark_h), Image.LANCZOS)
    block.paste(glyph, (int(round(left)), int(round(middle - mark_h / 2))), glyph)
    block.paste(word, (int(round(left + mark_h + cap * banner.GAP_PER_CAP)),
                       int(round(middle + cap / 2 - baseline_in_word))), word)
    strip.paste(block, (BANNER_SPLIT, 0))
    strip.save(out / "banner.bmp")

    # The field: paper, with a dark sidebar down the left. The row does not
    # fit a 168 px column at any size worth reading, so the sidebar stacks the
    # mark over the wordmark. It is the only second arrangement of the brand
    # group in Brokey and it is stated here, once.
    field = Image.new("RGB", DIALOG, paper)
    field.paste(Image.new("RGB", (DIALOG_SIDEBAR, DIALOG[1]), dark), (0, 0))
    column = DIALOG_SIDEBAR
    usable = column - SIDEBAR_MARGIN * 2
    probe = 100.0
    cap = probe * usable / banner.ink_width(font, "BROKEY", probe)
    word, baseline_in_word = banner.word_layer(font, "BROKEY", cap, ink)
    group_h = SIDEBAR_MARK + SIDEBAR_GAP + cap
    top = (DIALOG[1] - group_h) / 2
    glyph = images[512].resize((SIDEBAR_MARK, SIDEBAR_MARK), Image.LANCZOS)
    field.paste(glyph, (int(round((column - SIDEBAR_MARK) / 2)), int(round(top))), glyph)
    field.paste(word, (int(round((column - word.width) / 2)),
                       int(round(top + SIDEBAR_MARK + SIDEBAR_GAP + cap - baseline_in_word))), word)
    field.save(out / "dialog.bmp")
    print(f"wrote packaging/windows/banner.bmp and dialog.bmp, split {BANNER_SPLIT} and {DIALOG_SIDEBAR}")

    readable(out / "banner.bmp", BANNER, (0, 0, BANNER_SPLIT, BANNER[1]))
    readable(out / "dialog.bmp", DIALOG, (DIALOG_SIDEBAR, 0, DIALOG[0], DIALOG[1]))


def readable(path: Path, size: tuple[int, int], light: tuple[int, int, int, int]) -> None:
    """Refuse to leave a bitmap MSI would write black text onto illegibly.

    `light` is the box, in pixels, that must stay pale: MSI draws the dialog
    titles there and the colour is not ours to change.
    """
    img = Image.open(path)
    if img.size != size or img.mode != "RGB":
        sys.exit(
            f"{path.name} is {img.size} {img.mode}, and WiX takes {size} 24-bit. "
            f"Check BANNER and DIALOG against the sizes WixUI asks for, then run this script again."
        )
    x0, y0, x1, y1 = light
    px = img.load()
    darkest = min(
        (0.2126 * px[x, y][0] + 0.7152 * px[x, y][1] + 0.0722 * px[x, y][2]) / 255
        for y in range(y0, y1) for x in range(x0, x1)
    )
    if darkest <= 0.6:
        sys.exit(
            f"{path.name} is too dark at {darkest:.2f} where MSI writes its titles in black, "
            f"so the installer's headings would not be readable. Move the light part of the "
            f"picture to cover that box, or widen it: see BANNER_SPLIT and DIALOG_SIDEBAR."
        )


def main() -> int:
    hue = accent_hue()
    colour = oklch_to_srgb(0.674, 0.101, hue)
    print(f"accent hue {hue:g} -> #{colour[0]:02X}{colour[1]:02X}{colour[2]:02X}")

    icons = ROOT / "assets/icons"
    icons.mkdir(parents=True, exist_ok=True)
    sizes = [16, 32, 48, 64, 128, 256, 512]
    images = {s: mark(s, colour) for s in sizes}
    for s, img in images.items():
        img.save(icons / f"brokey-{s}.png")
    images[256].save(icons / "brokey.ico", sizes=[(s, s) for s in (16, 32, 48, 64, 128, 256)])

    tauri = ROOT / "crates/brokey/icons"
    tauri.mkdir(parents=True, exist_ok=True)
    images[32].save(tauri / "32x32.png")
    images[128].save(tauri / "128x128.png")
    images[256].save(tauri / "128x128@2x.png")
    images[512].save(tauri / "icon.png")
    print(f"wrote {len(sizes)} icon sizes, the .ico and Tauri's four")

    installer_art(images)

    out = ROOT / "docs/images"
    out.mkdir(parents=True, exist_ok=True)
    font = ROOT / "assets/fonts/Archivo.ttf"
    for name, (ground, ink) in banner.GROUNDS.items():
        img = banner.compose(images[512], font, "BROKEY", ground, ink, banner.WIDTH, banner.HEIGHT)
        img.save(out / name)
        print(f"wrote {out / name}")
    ground, ink = banner.GROUNDS["banner.png"]
    banner.compose(images[512], font, "BROKEY", ground, ink, *banner.SOCIAL).save(out / "social.png")
    print(f"wrote {out / 'social.png'}")
    return 0



if __name__ == "__main__":
    sys.exit(main())
