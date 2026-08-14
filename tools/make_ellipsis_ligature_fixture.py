"""Regenerate the ellipsis-ligature test fixture of `ms-text-render`.

Why this script exists
----------------------
`crates/ms-text-render/src/font_ligature_patch.rs` removes every GSUB ligature
rule whose OUTPUT is the glyph `cmap` maps U+2026 to, so that a font cannot turn
three typed periods back into a single ellipsis glyph. The bundled `fonts/ui`
stack contains no such rule (checked over all 56 files), and the display fonts
that do have one are not tracked by Git, so the negative case has no natural
fixture in a clean clone. This script builds a minimal, reproducible one.

The fixture
-----------
A ~3 KB subset of `fonts/ui/core/00-NotoSans-Regular.ttf` holding only
`.notdef period ellipsis a b f i`, renamed to a private family so it can never
collide with the bundled "Noto Sans" faces, with a hand-written `liga` feature:

    sub period period period by ellipsis;   # the rule the patcher must remove
    sub period a          by b;             # SAME LigatureSet: must survive
    sub f i               by b;             # other LigatureSet: must survive

`period period period -> ellipsis` has three components and `period a` two, so
feaLib emits them in that order inside the shared `period` LigatureSet: removing
the first entry shifts the second one down, which is exactly the offset-array
edit the patcher performs and the case a naive "blank the record" patch corrupts.

Usage
-----
    venv/bin/python tools/make_ellipsis_ligature_fixture.py

Writes `crates/ms-text-render/tests/fixtures/ellipsis_ligature.ttf`. The output
is committed (see the allowlist entry in `.gitignore`); rerun this script only
when the fixture contract changes, and commit the regenerated file with it.
"""

from __future__ import annotations

import logging
from pathlib import Path

from fontTools.feaLib.builder import addOpenTypeFeatures
from fontTools.subset import Options, Subsetter
from fontTools.ttLib import TTFont

log = logging.getLogger(__name__)

REPO_ROOT = Path(__file__).resolve().parent.parent
SOURCE = REPO_ROOT / "fonts/ui/core/00-NotoSans-Regular.ttf"
OUTPUT = REPO_ROOT / "crates/ms-text-render/tests/fixtures/ellipsis_ligature.ttf"

# Only what the tests shape: the dots, the ellipsis they must not collapse into,
# and two more pairs that prove unrelated ligatures survive the patch.
KEEP_UNICODES = [0x002E, 0x2026, 0x0061, 0x0062, 0x0066, 0x0069]

# A private family name: the fixture is loaded next to the bundled base in tests,
# and reusing "Noto Sans" would trigger `displace_bundled_faces`.
FAMILY_NAME = "MS Ellipsis Ligature Fixture"
STYLE_NAME = "Regular"
POSTSCRIPT_NAME = "MSEllipsisLigatureFixture-Regular"

FEATURES = """
feature liga {
    sub period period period by ellipsis;
    sub period a by b;
    sub f i by b;
} liga;
"""

# `head.created` / `head.modified` are seconds since 1904-01-01, and fontTools
# stamps `modified` with the CURRENT time on every save unless it is told not to.
# That single field is the only thing that used to make two runs of this script
# differ (byte 115 onward), which contradicts both the docstring above and
# `tools/MODULE_README.md`. Pinning both fields to one constant makes the output
# byte-identical run to run, machine to machine.
#
# 3 786 912 000 = 2024-01-01T00:00:00Z in the sfnt `LONGDATETIME` epoch
# (1 704 067 200 Unix + 2 082 844 800 between 1904-01-01 and 1970-01-01).
FIXED_TIMESTAMP = 3_786_912_000


def build_fixture(source: Path, output: Path) -> None:
    """Subset `source`, rename it, install the `liga` feature and write `output`.

    Raises FileNotFoundError if `source` is missing and ValueError if the subset
    lost a glyph the feature file references.
    """
    if not source.is_file():
        raise FileNotFoundError(f"source font not found: {source}")

    # `recalcTimestamp=False` keeps fontTools from stamping `head.modified` with
    # the current time; `_pin_timestamps` then replaces whatever the source font
    # carried, so the output does not depend on the source's own stamp either.
    font = TTFont(str(source), recalcTimestamp=False)

    options = Options()
    # Drop every layout table: the fixture's whole point is that its GSUB holds
    # exactly the three rules below and nothing else.
    options.layout_features = []
    options.name_IDs = ["*"]
    options.name_legacy = True
    options.notdef_outline = True
    options.recalc_bounds = True
    subsetter = Subsetter(options=options)
    subsetter.populate(unicodes=KEEP_UNICODES)
    subsetter.subset(font)

    for table in ("GSUB", "GPOS", "GDEF"):
        if table in font:
            del font[table]

    missing = {
        name
        for name in ("period", "ellipsis", "a", "b", "f", "i")
        if name not in font.getGlyphOrder()
    }
    if missing:
        raise ValueError(f"the subset lost glyphs the feature file needs: {sorted(missing)}")

    _rename(font)

    fea_path = output.parent / "ellipsis_ligature.fea"
    output.parent.mkdir(parents=True, exist_ok=True)
    fea_path.write_text(FEATURES, encoding="utf-8")
    try:
        addOpenTypeFeatures(font, str(fea_path))
    finally:
        fea_path.unlink(missing_ok=True)

    _pin_timestamps(font)

    font.save(str(output))
    log.info("wrote %s (%d bytes)", output, output.stat().st_size)


def _pin_timestamps(font: TTFont) -> None:
    """Freeze `head.created`/`head.modified` so the output is byte-reproducible."""
    head = font["head"]
    head.created = FIXED_TIMESTAMP
    head.modified = FIXED_TIMESTAMP


def _rename(font: TTFont) -> None:
    """Overwrite the family/style/PostScript name records with the fixture's own."""
    name_table = font["name"]
    for name_id, value in (
        (1, FAMILY_NAME),
        (2, STYLE_NAME),
        (3, f"{FAMILY_NAME}; fixture"),
        (4, f"{FAMILY_NAME} {STYLE_NAME}"),
        (6, POSTSCRIPT_NAME),
        (16, FAMILY_NAME),
        (17, STYLE_NAME),
    ):
        # Windows/Unicode and Macintosh/Roman records, the two fontdb reads.
        name_table.setName(value, name_id, 3, 1, 0x409)
        name_table.setName(value, name_id, 1, 0, 0)


def main() -> None:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
    build_fixture(SOURCE, OUTPUT)


if __name__ == "__main__":
    main()
