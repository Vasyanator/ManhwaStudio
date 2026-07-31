/*
File: crates/ms-text-render/src/fallback_diag.rs

Purpose:
Post-shaping font diagnostic: which characters of the text just shaped were drawn
by a font OTHER than the one the caller selected, and which characters no font in
the render base could draw at all.

Main responsibilities:
- turn a shaped `cosmic_text::Buffer` into a compact `FontFallbackReport`
  aggregated by (font -> characters), not one entry per glyph;
- stay free on the hot path: two integer comparisons per glyph and no allocation
  at all while the selected font serves the whole text.

Key functions:
- `collect_font_fallback_report` — the single entry point, called once per render
  right after `Buffer::shape_until_scroll` (every layout mode draws from that same
  shaped buffer, so one pass covers all of them).
- `expected_face_ids` — the caller's own faces, resolved once per render.

Notes:
The renderer's fallback chain is deterministic (`font_base.rs`), so a character it
serves IS rendered correctly — just not in the selected typeface. That is why the
result is INFORMATION, and only `missing` (`.notdef`, a tofu box) is a real loss.
This is a different question from `src/tabs/typing/panel/font_coverage.rs`, which
statically ranks a FONT against the typesetting LANGUAGE before any text exists.
*/

use crate::types::{FontFallbackReport, FontFallbackUse};
use cosmic_text::{Buffer, FontSystem, fontdb};
use std::collections::BTreeSet;

/// Collects the fallback/tofu diagnostic of one shaped buffer.
///
/// `expected_families` are the family names of the faces the CALLER supplied — the
/// selected font plus every inline `<font=...>` font. A glyph drawn by any face of
/// those families counts as "drawn by the font you chose"; anything else is a
/// fallback and is reported with the family name that actually drew it.
///
/// Returns an empty report (and does no work beyond the cheap per-glyph test) when
/// the whole text was served by those families. An empty `expected_families`, or a
/// set of names no registered face declares, also yields an empty report: with no
/// reference point every glyph would look like a fallback, and a wrong report is
/// worse than none.
///
/// Cost: one pass over the database faces (~50 on the bundled base) to resolve the
/// expected ids, then per glyph one `u16` comparison plus a linear scan over 1-5
/// `fontdb::ID`s. Nothing is allocated until the first glyph fails one of those.
pub(crate) fn collect_font_fallback_report(
    font_system: &FontSystem,
    buffer: &Buffer,
    expected_families: &[&str],
) -> FontFallbackReport {
    if expected_families.is_empty() {
        return FontFallbackReport::default();
    }
    let expected_ids = expected_face_ids(font_system, expected_families);
    if expected_ids.is_empty() {
        return FontFallbackReport::default();
    }

    // Aggregation by font id, in first-seen order. `BTreeSet` both deduplicates and
    // orders the characters deterministically, and allocates only once a font has
    // actually contributed something.
    let mut fallbacks: Vec<(fontdb::ID, BTreeSet<char>)> = Vec::new();
    let mut missing: BTreeSet<char> = BTreeSet::new();

    for run in buffer.layout_runs() {
        for glyph in run.glyphs {
            // `.notdef` is reported as lost whatever face produced it: the reader
            // sees a box either way, and naming the face would only mislead.
            if glyph.glyph_id == 0 {
                collect_cluster_chars(&mut missing, run.text, glyph.start, glyph.end);
                continue;
            }
            if expected_ids.contains(&glyph.font_id) {
                continue;
            }
            let idx = match fallbacks.iter().position(|(id, _)| *id == glyph.font_id) {
                Some(idx) => idx,
                None => {
                    fallbacks.push((glyph.font_id, BTreeSet::new()));
                    fallbacks.len().saturating_sub(1)
                }
            };
            if let Some((_, chars)) = fallbacks.get_mut(idx) {
                collect_cluster_chars(chars, run.text, glyph.start, glyph.end);
            }
        }
    }

    FontFallbackReport {
        fallbacks: fallbacks
            .into_iter()
            .filter(|(_, chars)| !chars.is_empty())
            .map(|(id, chars)| FontFallbackUse {
                family: family_name_of(font_system, id),
                chars: chars.into_iter().collect(),
            })
            .collect(),
        missing: missing.into_iter().collect(),
    }
}

/// Every face id in `font_system`'s database that declares one of `families`.
///
/// Family-level and not file-level on purpose: a real Bold or Italic of the SAME
/// family living in a separate file is still "the font the user chose", and the
/// family name is also the name the UI shows. Resolved once per render so the
/// per-glyph test never touches the database.
fn expected_face_ids(font_system: &FontSystem, families: &[&str]) -> Vec<fontdb::ID> {
    font_system
        .db()
        .faces()
        .filter(|face| {
            face.families
                .iter()
                .any(|(name, _)| families.contains(&name.as_str()))
        })
        .map(|face| face.id)
        .collect()
}

/// Family name a face declares, or an empty string for a face with none.
///
/// `fontdb` rejects unnamed fonts on load, so the empty case is unreachable in
/// practice; it is handled rather than asserted because the renderer must not panic
/// on a malformed font.
fn family_name_of(font_system: &FontSystem, id: fontdb::ID) -> String {
    font_system
        .db()
        .face(id)
        .and_then(|face| face.families.first().map(|(name, _)| name.clone()))
        .unwrap_or_default()
}

/// Adds the characters of one shaped cluster to `out`.
///
/// `start`/`end` are byte offsets into the ORIGINAL line text, which is exactly
/// `LayoutRun::text` (`cosmic-text-0.14.2/src/layout.rs:13-16`,
/// `src/buffer.rs:19-20`). A cluster is normally a single character; a ligature or a
/// base+mark cluster spans several, and all of them were drawn by the same font, so
/// all of them are recorded. Control characters are skipped — they have no visible
/// shape, so naming them would be noise in the user-facing list. An out-of-bounds or
/// non-boundary range is ignored instead of panicking.
fn collect_cluster_chars(out: &mut BTreeSet<char>, line_text: &str, start: usize, end: usize) {
    let Some(cluster) = line_text.get(start..end) else {
        return;
    };
    for ch in cluster.chars().filter(|ch| !ch.is_control()) {
        out.insert(ch);
    }
}

#[cfg(test)]
mod tests {
    use super::collect_font_fallback_report;
    use crate::font_base::test_bundle;
    use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping};

    /// Shapes `text` in `family` on the SHIPPED bundle and returns the report the
    /// production collector produces for it.
    ///
    /// Returns `None` when this checkout has no `fonts/ui` bundle, so a test can
    /// skip instead of asserting against an empty database.
    fn report_for(
        text: &str,
        family: &str,
    ) -> Option<(crate::types::FontFallbackReport, FontSystem)> {
        let mut font_system = test_bundle::font_system()?;
        let attrs = Attrs::new().family(Family::Name(family));
        let mut buffer = Buffer::new(&mut font_system, Metrics::new(32.0, 32.0));
        buffer.set_size(&mut font_system, None, None);
        buffer.set_text(&mut font_system, text, &attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut font_system, false);
        let report = collect_font_fallback_report(&font_system, &buffer, &[family]);
        Some((report, font_system))
    }

    #[test]
    fn text_fully_served_by_the_selected_font_reports_nothing() {
        let Some((report, _system)) = report_for("Latin text, 1234.", "Noto Sans") else {
            return;
        };
        assert!(
            report.is_empty(),
            "plain Latin in Noto Sans must produce no diagnostic, got {report:?}"
        );
        // Empty means "no allocation happened": both vectors are still the
        // capacity-0 ones `Vec::new()` produced.
        assert_eq!(report.fallbacks.capacity(), 0);
        assert_eq!(report.missing.capacity(), 0);
    }

    #[test]
    fn a_han_character_on_a_latin_font_is_reported_with_the_font_that_drew_it() {
        let Some((report, _system)) = report_for("a漢b", "Noto Sans") else {
            return;
        };
        assert!(
            report.missing.is_empty(),
            "the bundle covers this Han character, so nothing may be lost: {report:?}"
        );
        assert_eq!(
            report.fallbacks.len(),
            1,
            "exactly one fallback font expected, got {report:?}"
        );
        let entry = match report.fallbacks.first() {
            Some(entry) => entry,
            None => panic!("fallback entry asserted above must exist"),
        };
        assert_eq!(entry.chars, vec!['漢']);
        assert!(
            !entry.family.is_empty() && entry.family != "Noto Sans",
            "the fallback must name a REAL other family, got {:?}",
            entry.family
        );
        assert!(
            entry.family.contains("Han") || entry.family.contains("Plangothic"),
            "a Han character must be served by a CJK family, got {:?}",
            entry.family
        );
    }

    #[test]
    fn a_character_no_bundled_font_ships_is_reported_as_not_rendered() {
        // U+E000 is a Private Use Area codepoint: by definition no shipped font
        // assigns it, so it can only come out as `.notdef`.
        let Some((report, _system)) = report_for("a\u{e000}b", "Noto Sans") else {
            return;
        };
        assert_eq!(
            report.missing,
            vec!['\u{e000}'],
            "an unassigned codepoint must land in `missing`, got {report:?}"
        );
    }

    #[test]
    fn an_unknown_family_name_yields_an_empty_report_instead_of_a_wrong_one() {
        let Some(mut font_system) = test_bundle::font_system() else {
            return;
        };
        let attrs = Attrs::new();
        let mut buffer = Buffer::new(&mut font_system, Metrics::new(32.0, 32.0));
        buffer.set_size(&mut font_system, None, None);
        buffer.set_text(&mut font_system, "text", &attrs, Shaping::Advanced);
        buffer.shape_until_scroll(&mut font_system, false);

        let no_reference =
            collect_font_fallback_report(&font_system, &buffer, &["No Such Family At All"]);
        assert!(
            no_reference.is_empty(),
            "an unresolvable reference family must silence the diagnostic, got {no_reference:?}"
        );
        let no_families = collect_font_fallback_report(&font_system, &buffer, &[]);
        assert!(no_families.is_empty());
    }
}
