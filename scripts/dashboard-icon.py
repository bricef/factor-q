"""Build the fq dashboard icon: a dark tile with a bold monospace "fq".

Glyph outlines come from DejaVu Sans Mono Bold via fontTools, so the SVG
carries paths, not <text> — the mark renders identically in every
browser tab and in the one-off rasterisation for the PNG sizes.
"""
import sys
from fontTools.ttLib import TTFont
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.boundsPen import BoundsPen

FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
BG, FG = "#14161a", "#7aa2e8"
SIZE = 64          # viewBox edge
INNER = 0.62       # the mark's width as a share of the tile (maskable-safe)

font = TTFont(FONT)
glyphs = font.getGlyphSet()
cmap = font.getBestCmap()
upm = font["head"].unitsPerEm

names = [cmap[ord(c)] for c in "fq"]
paths, x = [], 0
xmin = ymin = float("inf")
xmax = ymax = float("-inf")
for n in names:
    g = glyphs[n]
    bp = BoundsPen(glyphs)
    g.draw(bp)
    gx0, gy0, gx1, gy1 = bp.bounds
    xmin, xmax = min(xmin, x + gx0), max(xmax, x + gx1)
    ymin, ymax = min(ymin, gy0), max(ymax, gy1)
    sp = SVGPathPen(glyphs)
    g.draw(sp)
    paths.append((x, sp.getCommands()))
    x += g.width

w, h = xmax - xmin, ymax - ymin
scale = SIZE * INNER / w
tx = (SIZE - w * scale) / 2 - xmin * scale
# y flips: font y-up, SVG y-down. Centre the ink box vertically.
ty = (SIZE + h * scale) / 2 + ymin * scale

out = [
    f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {SIZE} {SIZE}">',
    f'  <rect width="{SIZE}" height="{SIZE}" fill="{BG}"/>',
    f'  <g fill="{FG}" transform="translate({tx:.3f} {ty:.3f}) scale({scale:.5f} {-scale:.5f})">',
]
for off, d in paths:
    out.append(f'    <path transform="translate({off} 0)" d="{d}"/>')
out += ["  </g>", "</svg>", ""]
sys.stdout.write("\n".join(out))
