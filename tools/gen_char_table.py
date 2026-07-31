"""Generator for `src/tabs/typing/panel/char_table/charset.rs`.

Purpose:
Build the checked-in character set of the typing tab's "Таблица символов" window
from a curated set of Unicode ranges (see `dev-docs/char_table_plan.md` §4) and
emit it as a Rust source file holding one `&'static [char]` per group.

The generator is a BUILD-TIME tool, never a runtime dependency: its output is
committed and the application never runs this script.

Three mandatory filters are applied, in this order, and every dropped character
is reported so the resulting table is auditable:

1. unassigned      - `unicodedata.name` raises for the codepoint;
2. invisible       - general category in Cc/Cf/Zs/Zl/Zp/Mn/Me (an invisible or
                     combining cell is a cell the user cannot see or click);
3. undrawable      - no font bundled under `fonts/ui` (core + bold + ext) has a
                     glyph for it, so the cell would be a tofu box that also
                     cannot be inserted usefully.

A character claimed by two groups is kept only in the FIRST group that claims it
(group order below), so the emitted groups are disjoint by construction.

Requirements:
- `fontTools` (for reading each bundled font's cmap). The script STOPS with a
  clear message when no font-parsing library is available; it never guesses
  coverage.

Usage:
    python3 tools/gen_char_table.py               # write the Rust file
    python3 tools/gen_char_table.py --dry-run     # report only
"""

from __future__ import annotations

import argparse
import logging
import sys
import unicodedata
from dataclasses import dataclass, field
from pathlib import Path

log = logging.getLogger("gen_char_table")

# Repository root, derived from this file's location (`<root>/tools/`).
REPO_ROOT: Path = Path(__file__).resolve().parent.parent
FONTS_UI_DIR: Path = REPO_ROOT / "fonts" / "ui"
OUTPUT_FILE: Path = (
    REPO_ROOT / "src" / "tabs" / "typing" / "panel" / "char_table" / "charset.rs"
)

# General categories whose members can never be a meaningful, clickable cell.
INVISIBLE_CATEGORIES: frozenset[str] = frozenset(
    {"Cc", "Cf", "Zs", "Zl", "Zp", "Mn", "Me"}
)


@dataclass(frozen=True)
class GroupSpec:
    """One tab of the character table.

    `key` is the stable, non-localized identity persisted in the config and used
    as the i18n key suffix. `ranges` are INCLUSIVE codepoint ranges; `picks` are
    individual codepoints appended after the ranges. Order inside the group is
    the order given here (ranges first, then picks), with duplicates collapsed.
    """

    key: str
    ranges: tuple[tuple[int, int], ...] = ()
    picks: tuple[int, ...] = ()


def _r(start: int, end: int) -> tuple[int, int]:
    """Inclusive codepoint range helper (kept short so the table stays readable)."""
    return (start, end)


# Group order is significant: a character claimed by several groups stays in the
# first one listed here (see `collect_group_codepoints`).
GROUP_SPECS: tuple[GroupSpec, ...] = (
    GroupSpec(
        key="arrows",
        ranges=(_r(0x2190, 0x21FF), _r(0x27F0, 0x27FF), _r(0x2794, 0x27BE), _r(0x2B00, 0x2B11)),
    ),
    GroupSpec(
        key="lines",
        # Box Drawing + Block Elements.
        ranges=(_r(0x2500, 0x257F), _r(0x2580, 0x259F)),
    ),
    GroupSpec(
        key="shapes",
        # Geometric Shapes + the shape part of Miscellaneous Symbols and Arrows.
        ranges=(_r(0x25A0, 0x25FF), _r(0x2B12, 0x2B4F)),
    ),
    GroupSpec(
        key="math",
        # Mathematical Operators + Superscripts/Subscripts, plus the handful of
        # everyday math signs that live outside those blocks.
        ranges=(_r(0x2200, 0x22FF), _r(0x2070, 0x209F)),
        picks=(
            0x00B1,  # ±
            0x00D7,  # ×
            0x00F7,  # ÷
            0x2032,  # ′
            0x2033,  # ″
            0x2044,  # ⁄
        ),
    ),
    GroupSpec(
        key="typography",
        # Curated from General Punctuation (U+2000-206F), Latin-1 Punctuation
        # (U+00A1-00BF) and Supplemental Punctuation (U+2E00-2E4F): the marks a
        # typesetter actually reaches for. Whole ranges are deliberately NOT used
        # here - most of those blocks is invisible spacing or scholarly notation.
        picks=(
            # Dashes and hyphens.
            0x2010, 0x2011, 0x2012, 0x2013, 0x2014, 0x2015,
            # Quotation marks.
            0x2018, 0x2019, 0x201A, 0x201B, 0x201C, 0x201D, 0x201E, 0x201F,
            0x2039, 0x203A, 0x00AB, 0x00BB,
            # Reference and list marks.
            0x2020, 0x2021, 0x2022, 0x2023, 0x2027, 0x2030, 0x2031,
            0x00B6, 0x00A7, 0x00A9, 0x00AE, 0x2122, 0x00B0, 0x00B7,
            # Interrogation and exclamation.
            0x00A1, 0x00BF, 0x203C, 0x203D, 0x2047, 0x2048, 0x2049,
            # Miscellaneous marks.
            0x2026, 0x2025, 0x203B, 0x2042, 0x00A6, 0x00AC, 0x2E2E, 0x2E18,
        ),
    ),
    GroupSpec(
        key="currency",
        ranges=(_r(0x20A0, 0x20BF),),
        picks=(
            0x0024,  # $
            0x00A2,  # ¢
            0x00A3,  # £
            0x00A5,  # ¥
            0x00A4,  # ¤
        ),
    ),
    GroupSpec(
        key="music",
        # The four everyday note/accidental signs, plus a curated part of the
        # Musical Symbols block (clefs, accidentals, rests, note values,
        # augmentation dots and the common ornaments).
        ranges=(
            _r(0x2669, 0x266F),
            _r(0x1D100, 0x1D107),
            _r(0x1D10B, 0x1D113),
            _r(0x1D11E, 0x1D122),
            _r(0x1D12A, 0x1D135),
            _r(0x1D13B, 0x1D142),
            _r(0x1D15D, 0x1D164),
            _r(0x1D16A, 0x1D16C),
            _r(0x1D183, 0x1D184),
            _r(0x1D19B, 0x1D1A9),
        ),
    ),
    GroupSpec(
        key="technical",
        # Miscellaneous Technical, plus the check/cross/pen/scissors picks from
        # Dingbats that belong with editing marks rather than with decoration.
        ranges=(_r(0x2300, 0x23FF),),
        picks=(
            0x2701, 0x2702, 0x2703, 0x2704, 0x2706, 0x2707, 0x2708, 0x2709,
            0x270D, 0x270E, 0x270F, 0x2710, 0x2711, 0x2712,
            0x2713, 0x2714, 0x2715, 0x2716, 0x2717, 0x2718,
            0x274C, 0x274E,
        ),
    ),
    GroupSpec(
        key="game",
        # Chess, card suits, dice, plus the curated playing-card block (the four
        # suits Ace..King and the three jokers; the reserved gaps are skipped).
        ranges=(
            _r(0x2654, 0x265F),
            _r(0x2660, 0x2667),
            _r(0x2680, 0x2685),
            _r(0x1F0A0, 0x1F0AE),
            _r(0x1F0B1, 0x1F0BF),
            _r(0x1F0C1, 0x1F0CF),
            _r(0x1F0D1, 0x1F0DF),
        ),
    ),
    GroupSpec(
        key="stars_weather",
        ranges=(
            _r(0x2600, 0x2606),
            _r(0x263C, 0x263F),
            _r(0x2721, 0x2734),
            _r(0x26C4, 0x26C8),
        ),
        picks=(0x2609, 0x2744, 0x2B50),
    ),
    GroupSpec(
        key="emoji",
        # Curated sub-ranges of the three emoji blocks named in the spec. Whole
        # blocks are not used: they contain long runs of regional indicators,
        # skin-tone modifiers and flag components, none of which is a meaningful
        # single-cell pick.
        ranges=(
            _r(0x1F300, 0x1F321),  # sky, weather, globes, moon phases
            _r(0x1F332, 0x1F343),  # plants
            _r(0x1F345, 0x1F36F),  # food and drink
            _r(0x1F380, 0x1F393),  # celebration
            _r(0x1F3A0, 0x1F3C4),  # activities and sports
            _r(0x1F400, 0x1F43F),  # animals
            _r(0x1F600, 0x1F64F),  # faces and gestures
            _r(0x1F910, 0x1F92F),  # more faces
        ),
        picks=(0x2764,),  # ❤ (⭐ U+2B50 is claimed by `stars_weather`)
    ),
)


@dataclass
class FilterReport:
    """Per-filter record of what was dropped, for the audit printout."""

    unassigned: list[tuple[str, int]] = field(default_factory=list)
    invisible: list[tuple[str, int, str]] = field(default_factory=list)
    undrawable: list[tuple[str, int]] = field(default_factory=list)
    duplicate: list[tuple[str, int, str]] = field(default_factory=list)


def collect_group_codepoints() -> list[tuple[str, list[int]]]:
    """Expand every group spec into its ordered codepoint list.

    Duplicates inside one group collapse; a codepoint already claimed by an
    EARLIER group is left out here (reported separately by the caller), so the
    emitted groups are disjoint.
    """
    claimed: dict[int, str] = {}
    groups: list[tuple[str, list[int]]] = []
    for spec in GROUP_SPECS:
        codepoints: list[int] = []
        seen: set[int] = set()
        for start, end in spec.ranges:
            codepoints.extend(range(start, end + 1))
        codepoints.extend(spec.picks)
        ordered: list[int] = []
        for codepoint in codepoints:
            if codepoint in seen:
                continue
            seen.add(codepoint)
            if codepoint in claimed:
                continue
            claimed[codepoint] = spec.key
            ordered.append(codepoint)
        groups.append((spec.key, ordered))
    return groups


def duplicate_claims() -> list[tuple[str, int, str]]:
    """Codepoints a later group asked for that an earlier group already owns."""
    owner: dict[int, str] = {}
    duplicates: list[tuple[str, int, str]] = []
    for spec in GROUP_SPECS:
        requested: list[int] = []
        for start, end in spec.ranges:
            requested.extend(range(start, end + 1))
        requested.extend(spec.picks)
        for codepoint in requested:
            if codepoint in owner and owner[codepoint] != spec.key:
                duplicates.append((spec.key, codepoint, owner[codepoint]))
            else:
                owner.setdefault(codepoint, spec.key)
    return duplicates


def load_bundled_coverage() -> set[int]:
    """Union of the cmaps of every font bundled under `fonts/ui`.

    Reads the `core`, `bold` and `ext` tiers. Raises `SystemExit` when no font
    parser is installed or no bundled font can be read: guessing coverage would
    silently change what the table offers.
    """
    try:
        from fontTools.ttLib import TTFont  # noqa: PLC0415 - optional build-time dep
    except ImportError as err:
        raise SystemExit(
            "gen_char_table: no font-parsing library available "
            f"(fontTools import failed: {err}).\n"
            "Install it into the project venv before regenerating the charset:\n"
            "    venv/bin/python -m pip install fonttools\n"
            "The generator refuses to guess glyph coverage."
        ) from err

    if not FONTS_UI_DIR.is_dir():
        raise SystemExit(
            f"gen_char_table: bundled font directory not found: {FONTS_UI_DIR}"
        )

    font_files: list[Path] = []
    for tier in ("core", "bold", "ext"):
        tier_dir = FONTS_UI_DIR / tier
        if not tier_dir.is_dir():
            log.warning("bundled font tier missing: %s", tier_dir)
            continue
        font_files.extend(sorted(p for p in tier_dir.iterdir() if p.suffix.lower() in {".ttf", ".otf", ".ttc"}))

    if not font_files:
        raise SystemExit(
            f"gen_char_table: no font files found under {FONTS_UI_DIR} "
            "(expected the core/bold/ext tiers). Cannot compute glyph coverage."
        )

    covered: set[int] = set()
    for path in font_files:
        try:
            with TTFont(path, fontNumber=0, lazy=True) as font:
                covered.update(font.getBestCmap().keys())
        except Exception as err:  # noqa: BLE001 - any parser failure must be visible
            log.error("cannot read cmap of %s: %s", path, err)
    if not covered:
        raise SystemExit(
            "gen_char_table: every bundled font failed to parse; refusing to emit "
            "an empty charset."
        )
    log.info(
        "bundled coverage: %d codepoints from %d font files",
        len(covered),
        len(font_files),
    )
    return covered


def apply_filters(
    groups: list[tuple[str, list[int]]], covered: set[int]
) -> tuple[list[tuple[str, list[int]]], FilterReport]:
    """Apply the three mandatory filters and return the surviving groups."""
    report = FilterReport(duplicate=duplicate_claims())
    filtered: list[tuple[str, list[int]]] = []
    for key, codepoints in groups:
        kept: list[int] = []
        for codepoint in codepoints:
            char = chr(codepoint)
            try:
                unicodedata.name(char)
            except ValueError:
                report.unassigned.append((key, codepoint))
                continue
            category = unicodedata.category(char)
            if category in INVISIBLE_CATEGORIES:
                report.invisible.append((key, codepoint, category))
                continue
            if codepoint not in covered:
                report.undrawable.append((key, codepoint))
                continue
            kept.append(codepoint)
        filtered.append((key, kept))
    return filtered, report


def _format_codepoints(items: list[int], limit: int = 40) -> str:
    """Compact `U+XXXX` listing, truncated so the audit log stays readable."""
    shown = ", ".join(f"U+{cp:04X}" for cp in items[:limit])
    if len(items) > limit:
        shown += f", ... (+{len(items) - limit} more)"
    return shown


def report_drops(report: FilterReport) -> None:
    """Print what each filter removed, grouped by table group."""
    for title, entries in (
        ("filter 1 (unassigned)", [(k, cp) for k, cp in report.unassigned]),
        ("filter 2 (invisible/combining)", [(k, cp) for k, cp, _ in report.invisible]),
        ("filter 3 (no bundled font draws it)", [(k, cp) for k, cp in report.undrawable]),
        ("pre-filter (claimed by an earlier group)", [(k, cp) for k, cp, _ in report.duplicate]),
    ):
        by_group: dict[str, list[int]] = {}
        for key, codepoint in entries:
            by_group.setdefault(key, []).append(codepoint)
        total = sum(len(v) for v in by_group.values())
        log.info("%s dropped %d characters", title, total)
        for key in sorted(by_group):
            log.info("  %-14s %3d: %s", key, len(by_group[key]), _format_codepoints(by_group[key]))


def render_rust(groups: list[tuple[str, list[int]]], report: FilterReport) -> str:
    """Render the complete `charset.rs` source text."""
    total = sum(len(codepoints) for _, codepoints in groups)
    lines: list[str] = []
    lines.append("/*")
    lines.append("File: panel/char_table/charset.rs")
    lines.append("")
    lines.append("Purpose:")
    lines.append("The character set of the typing tab's character-table window: one")
    lines.append("`&'static [char]` per group plus the `CharGroup` table that names them.")
    lines.append("")
    lines.append("GENERATED FILE - do not edit by hand.")
    lines.append("Regenerate with `python3 tools/gen_char_table.py` after changing the group")
    lines.append("ranges in that script; the output is checked in and the generator is never")
    lines.append("a runtime dependency.")
    lines.append("")
    lines.append("Filters applied by the generator (all three mandatory, see the script):")
    lines.append(f"  1. unassigned codepoints                    - dropped {len(report.unassigned)}")
    lines.append(f"  2. Cc/Cf/Zs/Zl/Zp/Mn/Me (invisible)         - dropped {len(report.invisible)}")
    lines.append(f"  3. not drawable by any bundled `fonts/ui`   - dropped {len(report.undrawable)}")
    lines.append(f"  plus {len(report.duplicate)} codepoints claimed by an earlier group.")
    lines.append("")
    lines.append("Key types:")
    lines.append("- `CharGroup` (a group's stable key + its characters)")
    lines.append("")
    lines.append("Key functions:")
    lines.append("- `groups` (the whole table, in tab order)")
    lines.append("- `group_by_key` (lookup by the persisted group key)")
    lines.append("")
    lines.append("Notes:")
    lines.append("`key` is the stable NON-LOCALIZED group identity: it is persisted in")
    lines.append("`TextTab.char_table_last_group` and forms the i18n key suffix")
    lines.append("(`typing.char_table.group.<key>_label`). The favorites tab is NOT a group")
    lines.append("here - it is a UI concept backed by `favorites.rs`, not by a character list.")
    lines.append("Characters are emitted as `\\u{...}` escapes so the file stays ASCII and")
    lines.append("unambiguous regardless of the reader's font.")
    lines.append("*/")
    lines.append("")
    lines.append("/// One tab of the character table: a stable key plus its characters.")
    lines.append("///")
    lines.append("/// `key` is persisted (`TextTab.char_table_last_group`) and is the i18n key")
    lines.append("/// suffix; it must never be localized or renamed without a migration.")
    lines.append("#[derive(Debug, Clone, Copy)]")
    lines.append("pub(in crate::tabs::typing::panel) struct CharGroup {")
    lines.append("    /// Stable, non-localized group identity (`\"arrows\"`, `\"lines\"`, ...).")
    lines.append("    pub(in crate::tabs::typing::panel) key: &'static str,")
    lines.append("    /// The group's characters, in display order. Never empty.")
    lines.append("    pub(in crate::tabs::typing::panel) chars: &'static [char],")
    lines.append("}")
    lines.append("")

    const_names: list[tuple[str, str]] = []
    for key, codepoints in groups:
        const_name = f"{key.upper()}_CHARS"
        const_names.append((key, const_name))
        lines.append(f"/// Characters of the `{key}` group ({len(codepoints)} entries).")
        lines.append(f"const {const_name}: &[char] = &[")
        for chunk_start in range(0, len(codepoints), 8):
            chunk = codepoints[chunk_start : chunk_start + 8]
            body = " ".join(f"'\\u{{{cp:04X}}}'," for cp in chunk)
            lines.append(f"    {body}")
        lines.append("];")
        lines.append("")

    lines.append(f"/// The whole character table in tab order ({total} characters total).")
    lines.append("const GROUPS: &[CharGroup] = &[")
    for key, const_name in const_names:
        lines.append(f'    CharGroup {{ key: "{key}", chars: {const_name} }},')
    lines.append("];")
    lines.append("")
    lines.append("/// All character groups, in the order their tabs are shown.")
    lines.append("#[must_use]")
    lines.append("pub(super) fn groups() -> &'static [CharGroup] {")
    lines.append("    GROUPS")
    lines.append("}")
    lines.append("")
    lines.append("/// Looks a group up by its stable `key`, or `None` when no group has that key")
    lines.append("/// (e.g. a persisted key from a newer build, or the favorites tab's own key).")
    lines.append("#[must_use]")
    lines.append("pub(super) fn group_by_key(key: &str) -> Option<&'static CharGroup> {")
    lines.append("    GROUPS.iter().find(|group| group.key == key)")
    lines.append("}")
    lines.append("")
    lines.append("#[cfg(test)]")
    lines.append("mod tests {")
    lines.append("    use super::*;")
    lines.append("    use std::collections::HashMap;")
    lines.append("")
    lines.append("    #[test]")
    lines.append("    fn no_group_is_empty() {")
    lines.append("        assert!(!groups().is_empty(), \"the table must have groups\");")
    lines.append("        for group in groups() {")
    lines.append("            assert!(")
    lines.append("                !group.chars.is_empty(),")
    lines.append("                \"group {} must not be empty\",")
    lines.append("                group.key")
    lines.append("            );")
    lines.append("        }")
    lines.append("    }")
    lines.append("")
    lines.append("    #[test]")
    lines.append("    fn no_invisible_or_combining_characters() {")
    lines.append("        // Filter 2 of the generator: an invisible or combining cell is a cell")
    lines.append("        // the user can neither see nor click meaningfully.")
    lines.append("        for group in groups() {")
    lines.append("            for &ch in group.chars {")
    lines.append("                assert!(")
    lines.append("                    !ch.is_control(),")
    lines.append("                    \"control character U+{:04X} in group {}\",")
    lines.append("                    u32::from(ch),")
    lines.append("                    group.key")
    lines.append("                );")
    lines.append("                assert!(")
    lines.append("                    !ch.is_whitespace(),")
    lines.append("                    \"whitespace character U+{:04X} in group {}\",")
    lines.append("                    u32::from(ch),")
    lines.append("                    group.key")
    lines.append("                );")
    lines.append("                // The generator drops Cf/Mn/Me; these are the ranges that")
    lines.append("                // would have carried them into the curated blocks.")
    lines.append("                let cp = u32::from(ch);")
    lines.append("                let combining = (0x0300..=0x036F).contains(&cp)")
    lines.append("                    || (0x1AB0..=0x1AFF).contains(&cp)")
    lines.append("                    || (0x20D0..=0x20FF).contains(&cp)")
    lines.append("                    || (0xFE00..=0xFE0F).contains(&cp)")
    lines.append("                    || (0x1D165..=0x1D169).contains(&cp)")
    lines.append("                    || (0x1D16D..=0x1D182).contains(&cp)")
    lines.append("                    || (0x1D185..=0x1D18B).contains(&cp)")
    lines.append("                    || (0x1D1AA..=0x1D1AD).contains(&cp);")
    lines.append("                assert!(")
    lines.append("                    !combining,")
    lines.append("                    \"combining character U+{cp:04X} in group {}\",")
    lines.append("                    group.key")
    lines.append("                );")
    lines.append("                // Format characters (Cf) are neither control nor whitespace in")
    lines.append("                // Rust's classification, so guard the two ranges the curated")
    lines.append("                // blocks touch explicitly.")
    lines.append("                let format_char = (0x200B..=0x200F).contains(&cp)")
    lines.append("                    || (0x2028..=0x202E).contains(&cp)")
    lines.append("                    || (0x2060..=0x2064).contains(&cp);")
    lines.append("                assert!(")
    lines.append("                    !format_char,")
    lines.append("                    \"format character U+{cp:04X} in group {}\",")
    lines.append("                    group.key")
    lines.append("                );")
    lines.append("            }")
    lines.append("        }")
    lines.append("    }")
    lines.append("")
    lines.append("    #[test]")
    lines.append("    fn characters_are_unique_across_groups() {")
    lines.append("        let mut owner: HashMap<char, &str> = HashMap::new();")
    lines.append("        for group in groups() {")
    lines.append("            for &ch in group.chars {")
    lines.append("                if let Some(previous) = owner.insert(ch, group.key) {")
    lines.append("                    panic!(")
    lines.append("                        \"U+{:04X} appears in both {previous} and {}\",")
    lines.append("                        u32::from(ch),")
    lines.append("                        group.key")
    lines.append("                    );")
    lines.append("                }")
    lines.append("            }")
    lines.append("        }")
    lines.append("    }")
    lines.append("")
    lines.append("    #[test]")
    lines.append("    fn group_lookup_matches_the_table() {")
    lines.append("        for group in groups() {")
    lines.append("            let found = group_by_key(group.key).map(|found| found.key);")
    lines.append("            assert_eq!(found, Some(group.key));")
    lines.append("        }")
    lines.append("        assert!(group_by_key(\"favorites\").is_none());")
    lines.append("    }")
    lines.append("}")
    return "\n".join(lines) + "\n"


def main() -> int:
    """Entry point: build the charset, report the drops, write the Rust file."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="report the filters without writing the Rust file",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=OUTPUT_FILE,
        help=f"destination file (default: {OUTPUT_FILE})",
    )
    args = parser.parse_args()

    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
    log.info("unicodedata version: %s", unicodedata.unidata_version)

    covered = load_bundled_coverage()
    groups = collect_group_codepoints()
    filtered, report = apply_filters(groups, covered)
    report_drops(report)
    for key, codepoints in filtered:
        log.info("group %-14s kept %4d characters", key, len(codepoints))
    total = sum(len(codepoints) for _, codepoints in filtered)
    log.info("total kept: %d characters", total)

    empty = [key for key, codepoints in filtered if not codepoints]
    if empty:
        raise SystemExit(
            f"gen_char_table: groups became empty after filtering: {', '.join(empty)}"
        )

    text = render_rust(filtered, report)
    if args.dry_run:
        log.info("dry run: %s not written (%d bytes)", args.output, len(text))
        return 0
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(text, encoding="utf-8")
    log.info("wrote %s (%d bytes)", args.output, len(text))
    return 0


if __name__ == "__main__":
    sys.exit(main())
