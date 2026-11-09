"""Copies glyphs a font lacks from another font, for the terminal screenshots.

Usage: fonts.py <base.ttf> <donor.ttf> <output.ttf> <first>-<last>[,<first>-<last>...]

review-tui draws a few symbols, such as the half-reviewed ◐ and the changed-since-reviewed ↻,
that JetBrains Mono does not have. resvg, which renders freeze's SVG, then draws the whole line
in the fallback font, so the screenshots copy every glyph in the given ranges of hexadecimal code
points that the donor has and the base lacks into the base itself, scaled to its em and centered
in its cell.
"""

import sys

from fontTools.pens.recordingPen import DecomposingRecordingPen
from fontTools.pens.transformPen import TransformPen
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import TTFont


def main() -> None:
    base_path, donor_path, output_path, ranges = sys.argv[1:]
    # Keeping the timestamp keeps the merged font the same on every run.
    base = TTFont(base_path, recalcTimestamp=False)
    donor = TTFont(donor_path)
    scale = base["head"].unitsPerEm / donor["head"].unitsPerEm
    base_cmap = base.getBestCmap()
    donor_cmap = donor.getBestCmap()
    donor_glyphs = donor.getGlyphSet()
    # A monospaced font gives every glyph the advance of its space.
    advance = base["hmtx"][base_cmap[ord(" ")]][0]
    codes = (
        code
        for first, _, last in (span.partition("-") for span in ranges.split(","))
        for code in range(int(first, 16), int(last or first, 16) + 1)
    )
    for code in codes:
        source = donor_cmap.get(code)
        if code in base_cmap or source is None:
            continue
        name = f"uni{code:04X}.donor"
        width = donor["hmtx"][source][0] * scale
        offset = (advance - width) / 2
        # The base has none of the donor's glyphs that composites refer to, so they are drawn
        # into it as outlines.
        outline = DecomposingRecordingPen(donor_glyphs)
        donor_glyphs[source].draw(outline)
        pen = TTGlyphPen(None)
        outline.replay(TransformPen(pen, (scale, 0, 0, scale, offset, 0)))
        # Setting the glyph also appends it to the font's glyph order.
        base["glyf"][name] = pen.glyph()
        base["hmtx"][name] = (advance, 0)
        for table in base["cmap"].tables:
            if table.isUnicode():
                table.cmap[code] = name
    base.setGlyphOrder(base["glyf"].glyphOrder)
    base.save(output_path)


if __name__ == "__main__":
    main()
