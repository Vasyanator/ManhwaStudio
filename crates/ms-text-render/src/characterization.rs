/*
File: crates/ms-text-render/src/characterization.rs

Purpose:
Test-only support for the RECORDED characterization matrices of the advance between two
adjacent glyphs: the horizontal pen (`pipeline` tests) and the formula seed advances
(`formula::render` tests). Each matrix is a bit-exact (`f32::to_bits`) snapshot of today's
layout over a fixed grid of texts x kerning modes x tracking x override tables x em sizes,
stored as a tracked JSON fixture under `tests/fixtures/`. A refactor of the pair decision must
reproduce it to the bit; a deliberate behaviour change regenerates it and says which rows moved.

Main responsibilities:
- the matrix axes (`MATRIX_TEXTS`, `MATRIX_MODES`, `MATRIX_TRACKING`, `MATRIX_TABLES`,
  `MATRIX_EMS`), shared by both matrices so they describe the same grid;
- `TrackedFont` / `LoadedFont`: the fixture fonts, taken ONLY from the tracked `fonts/ui` stack
  (the matrices pin the SHIPPED stack; the other unit tests load the tracked Arimo caller
  font of `test_fixtures.rs` instead);
- `MatrixRow` + `check_against_fixture`: compare a computed matrix with its fixture, or rewrite
  the fixture on the explicit opt-in `MS_TEXT_RENDER_REGEN_CHARACTERIZATION=1`.

Notes:
Without the opt-in the check only READS the fixture; it never writes anything (global test
hygiene). A mismatch panics with the full list of differing rows (case, expected, actual as
f32) so the diff is readable without a debugger.
*/

use crate::types::KerningMode;
use cosmic_text::{FontSystem, fontdb};
use std::path::PathBuf;

/// Environment variable that switches [`check_against_fixture`] from comparing to rewriting.
pub(crate) const REGEN_ENV: &str = "MS_TEXT_RENDER_REGEN_CHARACTERIZATION";

/// A tracked font of the shipped `fonts/ui` stack used as a matrix fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackedFont {
    /// `fonts/ui/core/00-NotoSans-Regular.ttf`: Latin with GPOS pair kerning (`AV`, `To`, ...).
    Latin,
    /// `fonts/ui/ext/30-NotoSansHebrew-Regular.ttf`: a right-to-left script for the direction rows.
    Hebrew,
}

impl TrackedFont {
    /// Repository-relative path of the font file (the crate sits two levels below the root).
    fn relative_path(self) -> &'static str {
        match self {
            Self::Latin => "../../fonts/ui/core/00-NotoSans-Regular.ttf",
            Self::Hebrew => "../../fonts/ui/ext/30-NotoSansHebrew-Regular.ttf",
        }
    }

    /// Short label used in matrix case names.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Latin => "latin",
            Self::Hebrew => "hebrew",
        }
    }

    /// The font file's bytes. Panics when the tracked file is missing (see [`Self::load`]).
    pub(crate) fn bytes(self) -> Vec<u8> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(self.relative_path());
        std::fs::read(&path)
            .unwrap_or_else(|err| panic!("tracked fixture font {} unreadable: {err}", path.display()))
    }

    /// Loads the font into a fresh single-face `FontSystem` (fixed `en-US` locale, no OS fonts).
    ///
    /// Panics when the tracked file is missing: it ships with every clone, so its absence is a
    /// broken checkout, not a skippable environment.
    pub(crate) fn load(self) -> LoadedFont {
        let mut db = fontdb::Database::new();
        db.load_font_data(self.bytes());
        let face = db.faces().next().expect("the tracked fixture font registers one face");
        let face_id = face.id;
        let family = face
            .families
            .first()
            .map(|(name, _language)| name.clone())
            .expect("the tracked fixture face has a family name");
        let font_system = FontSystem::new_with_locale_and_db("en-US".to_string(), db);
        LoadedFont {
            font_system,
            face_id,
            family,
        }
    }
}

/// A tracked fixture font loaded into its own `FontSystem`.
#[derive(Debug)]
pub(crate) struct LoadedFont {
    pub(crate) font_system: FontSystem,
    /// The registered face id — what an override table binds to (`CustomKerningMap::insert`).
    pub(crate) face_id: fontdb::ID,
    /// The family name to select the face with (`Family::Name`).
    pub(crate) family: String,
}

/// One text row of the matrix: a label for case names, the text, and the font that draws it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MatrixText {
    pub(crate) label: &'static str,
    pub(crate) text: &'static str,
    pub(crate) font: TrackedFont,
}

/// The texts: a bare kerned pair, a line with several font-kerned and overridden pairs, a line
/// with narrow glyphs (the tracking basis floor), and a right-to-left run.
pub(crate) const MATRIX_TEXTS: [MatrixText; 4] = [
    MatrixText { label: "AV", text: "AV", font: TrackedFont::Latin },
    MatrixText { label: "AVAWAY_To", text: "AVAWAY To", font: TrackedFont::Latin },
    MatrixText { label: "iWorld_lil", text: "iWorld lil", font: TrackedFont::Latin },
    MatrixText { label: "hebrew_abg", text: "\u{05D0}\u{05D1}\u{05D2}", font: TrackedFont::Hebrew },
];

/// Every kerning mode with its case-name label.
pub(crate) const MATRIX_MODES: [(&str, KerningMode); 3] = [
    ("auto", KerningMode::Auto),
    ("fixed", KerningMode::Fixed),
    ("optical", KerningMode::Optical),
];

/// Uniform tracking settings: `(label, kerning_px, kerning_percent)` as `TextRenderParams` holds them.
pub(crate) const MATRIX_TRACKING: [(&str, f32, f32); 3] =
    [("none", 0.0, 0.0), ("px3.5", 3.5, 0.0), ("pct-20", 0.0, -20.0)];

/// One user-authored pair: `(left, right, offset_per_mille)`.
pub(crate) type AuthoredPair = (char, char, f32);

/// One recorded fixture row's named series, as `f32` bit patterns.
type RecordedFields = Vec<(String, Vec<u32>)>;

/// Override tables as `(label, (left, right, offset_per_mille) triples)`. `none` binds no table at
/// all; `unlisted` binds a table that names no pair of any text (fast path off, nothing matched).
/// The Hebrew pair sits in the same tables so the right-to-left rows are overridden too.
pub(crate) const MATRIX_TABLES: [(&str, &[AuthoredPair]); 4] = [
    ("none", &[]),
    ("tighten", &[('A', 'V', -100.0), ('\u{05D0}', '\u{05D1}', -100.0)]),
    ("cancel", &[('A', 'V', 0.0), ('\u{05D0}', '\u{05D1}', 0.0)]),
    ("unlisted", &[('Q', 'z', -200.0)]),
];

/// The em sizes in px: a typical overlay size and the round 100 the unit tests reason in.
pub(crate) const MATRIX_EMS: [f32; 2] = [36.0, 100.0];

/// One computed row of a matrix: a case name and named `f32` series.
#[derive(Debug, Clone)]
pub(crate) struct MatrixRow {
    pub(crate) case: String,
    pub(crate) fields: Vec<(&'static str, Vec<f32>)>,
}

/// The case name of one grid point; both matrices name their rows the same way.
pub(crate) fn case_name(
    text: &MatrixText,
    mode_label: &str,
    tracking_label: &str,
    table_label: &str,
    em: f32,
) -> String {
    format!(
        "text={}|font={}|mode={mode_label}|tracking={tracking_label}|table={table_label}|em={em}",
        text.label,
        text.font.label()
    )
}

/// Absolute path of a fixture file under the crate's `tests/fixtures/`.
fn fixture_path(file_name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(file_name)
}

/// Serializes `rows` one per line (so a regenerated fixture diffs row by row in review).
fn render_fixture(about: &str, rows: &[MatrixRow]) -> String {
    let mut out = String::new();
    out.push_str("{\n  \"about\": ");
    out.push_str(&serde_json::Value::String(about.to_string()).to_string());
    out.push_str(",\n  \"rows\": [\n");
    for (idx, row) in rows.iter().enumerate() {
        let mut object = serde_json::Map::new();
        object.insert("case".to_string(), serde_json::Value::String(row.case.clone()));
        for (name, values) in &row.fields {
            let bits = values
                .iter()
                .map(|value| serde_json::Value::from(value.to_bits()))
                .collect::<Vec<_>>();
            object.insert((*name).to_string(), serde_json::Value::Array(bits));
        }
        out.push_str("    ");
        out.push_str(&serde_json::Value::Object(object).to_string());
        if idx + 1 < rows.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("  ]\n}\n");
    out
}

/// Reads a fixture back as `case -> field -> bits`.
fn parse_fixture(text: &str) -> Vec<(String, RecordedFields)> {
    let value: serde_json::Value = serde_json::from_str(text).expect("characterization fixture is JSON");
    let rows = value
        .get("rows")
        .and_then(serde_json::Value::as_array)
        .expect("characterization fixture has a `rows` array");
    rows.iter()
        .map(|row| {
            let object = row.as_object().expect("a fixture row is an object");
            let case = object
                .get("case")
                .and_then(serde_json::Value::as_str)
                .expect("a fixture row has a `case` name")
                .to_string();
            let fields = object
                .iter()
                .filter(|(key, _)| key.as_str() != "case")
                .map(|(key, values)| {
                    let bits = values
                        .as_array()
                        .expect("a fixture field is an array")
                        .iter()
                        .map(|bit| {
                            bit.as_u64()
                                .and_then(|bit| u32::try_from(bit).ok())
                                .expect("a fixture value is an f32 bit pattern")
                        })
                        .collect::<Vec<_>>();
                    (key.clone(), bits)
                })
                .collect::<Vec<_>>();
            (case, fields)
        })
        .collect()
}

/// Compares `rows` bit for bit with the fixture `file_name`, or rewrites the fixture when
/// [`REGEN_ENV`] is `1`.
///
/// Panics on any difference — a missing or extra case, a different field set, or a single
/// differing bit — listing every differing row with expected and actual values as `f32`.
pub(crate) fn check_against_fixture(file_name: &str, about: &str, rows: &[MatrixRow]) {
    let path = fixture_path(file_name);
    if std::env::var(REGEN_ENV).is_ok_and(|value| value == "1") {
        std::fs::write(&path, render_fixture(about, rows))
            .unwrap_or_else(|err| panic!("cannot rewrite {}: {err}", path.display()));
        eprintln!("characterization fixture rewritten: {}", path.display());
        return;
    }
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "characterization fixture {} unreadable: {err} (it is tracked; regenerate with \
             {REGEN_ENV}=1 only when a behaviour change is intended)",
            path.display()
        )
    });
    let expected = parse_fixture(&text);
    let mut report = String::new();
    if expected.len() != rows.len() {
        report.push_str(&format!("row count: expected {}, computed {}\n", expected.len(), rows.len()));
    }
    for (row, (expected_case, expected_fields)) in rows.iter().zip(expected.iter()) {
        if &row.case != expected_case {
            report.push_str(&format!("case order: expected {expected_case}, computed {}\n", row.case));
            continue;
        }
        let mut computed = row
            .fields
            .iter()
            .map(|(name, values)| {
                ((*name).to_string(), values.iter().map(|value| value.to_bits()).collect::<Vec<_>>())
            })
            .collect::<Vec<_>>();
        computed.sort_by(|a, b| a.0.cmp(&b.0));
        let mut wanted = expected_fields.clone();
        wanted.sort_by(|a, b| a.0.cmp(&b.0));
        if computed != wanted {
            let as_f32 = |fields: &[(String, Vec<u32>)]| {
                fields
                    .iter()
                    .map(|(name, bits)| {
                        let values = bits.iter().map(|bit| f32::from_bits(*bit)).collect::<Vec<_>>();
                        format!("{name}={values:?}")
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            report.push_str(&format!(
                "{}\n  expected: {}\n  actual:   {}\n",
                row.case,
                as_f32(&wanted),
                as_f32(&computed)
            ));
        }
    }
    assert!(
        report.is_empty(),
        "layout differs from the recorded characterization {}:\n{report}",
        path.display()
    );
}
