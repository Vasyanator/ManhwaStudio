/*
File: panel/char_table/coverage.rs

Purpose:
Answer one question for the character table: "which of the currently loaded
typing fonts contain character X?" — so the expanded-symbol row can offer one
variant cell per font that can actually draw the symbol.

Main responsibilities:
- snapshot the panel font list into a thread-safe `(index, path, face_index)`
  form plus a fingerprint of that list;
- compute, on a BACKGROUND thread, a `char -> Vec<font index>` map for the whole
  character table;
- deliver the result over a channel the GUI thread polls, and skip the work
  entirely when the font list has not changed since the last successful run.

Key types:
- `FontProbe` (one font's identity for the worker)
- `CoverageResult` (a finished map plus the fingerprint it was computed for)
- `CoverageJob` (the poll-driven handle owned by `CharTableState`)

Notes:
The work is I/O bound (`std::fs::read` of every font file) plus a cmap probe per
character, so it MUST NOT run on the GUI thread; the delivery shape mirrors
`create_state::spawn_font_reload` / `FontReloadResult`. The per-font cmap is
built ONCE and every character is tested against it, exactly like
`font_coverage::classify_font_bytes_for`.

The `FontEntryKind::BundledUiStack` entry is EXCLUDED from the map on purpose:
it stands for the whole bundled `fonts/ui` fallback chain (core + bold + ~44
`ext` files), not for the single file it points at, so a per-file cmap test
would understate it — exactly the reason its language coverage is reported as
`Full` without classification (see `panel/MODULE_README.md`). The window always
offers it as the first variant instead.

Font bytes are read with raw `std::fs`, like `fonts.rs` (the app font directory
is a real directory on every supported target); only PROJECT files go through
`crate::storage::storage()`.
*/

use crate::tabs::typing::panel::FontEntry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use ms_thread as thread;

/// One font the worker must probe: its index in the panel font list, the file to
/// read, and the face inside that file.
#[derive(Debug, Clone)]
pub(super) struct FontProbe {
    /// Index into `TypingCreatePanelState.fonts` — what the map stores.
    pub(super) index: usize,
    /// Representative font FILE path.
    pub(super) path: PathBuf,
    /// Face index inside `path` (0 for single-face files).
    pub(super) face_index: usize,
}

/// A finished coverage computation.
#[derive(Debug)]
pub(super) struct CoverageResult {
    /// Token of the job that produced it; a stale token is discarded.
    token: u64,
    /// Fingerprint of the font list this map was computed for.
    fingerprint: String,
    /// Character → indices into the panel font list, ascending.
    map: HashMap<char, Vec<usize>>,
}

/// Background glyph-coverage job owned by `CharTableState`.
///
/// The GUI thread calls [`CoverageJob::ensure`] once per frame (cheap: it only
/// compares a fingerprint) and [`CoverageJob::poll`] to pick up a finished
/// result. Nothing here blocks.
#[derive(Debug, Default)]
pub(super) struct CoverageJob {
    rx: Option<Receiver<CoverageResult>>,
    /// Monotonic token; only the newest job's result is accepted.
    latest_token: u64,
    /// Fingerprint the current `map` was computed for (`None` until the first
    /// result lands).
    fingerprint: Option<String>,
    /// Fingerprint of the job currently in flight, if any.
    in_flight_fingerprint: Option<String>,
    map: HashMap<char, Vec<usize>>,
}

impl CoverageJob {
    /// Builds the stable fingerprint of a font list from identity, representative
    /// file path, and representative face index.
    #[must_use]
    fn fingerprint(fonts: &[FontEntry]) -> String {
        let mut out = String::with_capacity(fonts.len() * 16);
        out.push_str(&fonts.len().to_string());
        for font in fonts {
            append_font_fingerprint(
                &mut out,
                &font.render_identity_name(),
                font.path(),
                font.representative_face_index(),
            );
        }
        out
    }

    /// Snapshots the probe list: every REAL font file of the panel list, with the
    /// bundled-stack entry excluded (see the file header).
    #[must_use]
    fn probes(fonts: &[FontEntry]) -> Vec<FontProbe> {
        fonts
            .iter()
            .enumerate()
            .filter(|(_, font)| font.bundled_stack_font().is_none())
            .map(|(index, font)| FontProbe {
                index,
                path: font.path().to_path_buf(),
                face_index: font.representative_face_index(),
            })
            .collect()
    }

    /// Starts a background computation when the font list changed since the last
    /// completed (or in-flight) job. A no-op otherwise, so it is safe to call
    /// every frame.
    ///
    /// `chars` is the full character set of the table; it is copied into the
    /// worker so the caller keeps no borrow across the thread boundary.
    pub(super) fn ensure(&mut self, fonts: &[FontEntry], chars: &[char]) {
        let fingerprint = Self::fingerprint(fonts);
        if self.fingerprint.as_deref() == Some(fingerprint.as_str())
            || self.in_flight_fingerprint.as_deref() == Some(fingerprint.as_str())
        {
            return;
        }
        let probes = Self::probes(fonts);
        let chars = chars.to_vec();
        self.latest_token = self.latest_token.wrapping_add(1);
        let token = self.latest_token;
        let (tx, rx) = mpsc::channel::<CoverageResult>();
        let worker_fingerprint = fingerprint.clone();
        let spawn_result = thread::Builder::new()
            .name("typing-char-table-coverage".to_string())
            .spawn(move || {
                let map = compute_coverage(&probes, &chars);
                // The receiver is dropped when the panel is dropped or a newer
                // job replaced this one; a failed send is then expected.
                if tx
                    .send(CoverageResult {
                        token,
                        fingerprint: worker_fingerprint,
                        map,
                    })
                    .is_err()
                {
                    crate::runtime_log::log_warn(
                        "typing: char table coverage result dropped (window closed or superseded)",
                    );
                }
            });
        match spawn_result {
            Ok(_handle) => {
                self.rx = Some(rx);
                self.in_flight_fingerprint = Some(fingerprint);
            }
            Err(err) => {
                // Without the worker the table still works — every font simply
                // stays unlisted for every symbol — but the reason must be
                // diagnosable rather than silent.
                crate::runtime_log::log_error(format!(
                    "typing: failed to spawn the character-table coverage worker; the per-font \
                     variants will be unavailable: {err}"
                ));
            }
        }
    }

    /// Picks up a finished result without blocking. Call once per frame.
    pub(super) fn poll(&mut self) {
        let Some(rx) = self.rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(result) => {
                // A result from a superseded job is dropped: its font indices
                // refer to a font list that is no longer current.
                if result.token == self.latest_token {
                    self.map = result.map;
                    self.fingerprint = Some(result.fingerprint);
                }
                self.rx = None;
                self.in_flight_fingerprint = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                self.in_flight_fingerprint = None;
            }
        }
    }

    /// Whether a computation is currently running (the window shows the variants
    /// row as "still loading" meanwhile).
    #[must_use]
    pub(super) fn in_flight(&self) -> bool {
        self.rx.is_some()
    }

    /// Indices (into the panel font list) of the fonts that can draw `ch`.
    ///
    /// Empty before the first result lands and for a character no loaded font
    /// covers. The bundled-stack entry is never listed here — the window offers
    /// it unconditionally as the first variant.
    #[must_use]
    pub(super) fn fonts_for(&self, ch: char) -> &[usize] {
        self.map.get(&ch).map_or(&[], Vec::as_slice)
    }
}

/// Appends every font attribute that can change the computed glyph map.
fn append_font_fingerprint(out: &mut String, identity: &str, path: &std::path::Path, face: usize) {
    out.push('\u{1f}');
    out.push_str(identity);
    out.push('\u{1e}');
    out.push_str(&path.to_string_lossy());
    out.push('\u{1e}');
    out.push_str(&face.to_string());
}

/// Pure mapping core: reads every probe's font file and returns the
/// character → font-index map.
///
/// Runs on the worker thread. Per font the charmap is built ONCE and every
/// character is tested against it (`charmap.map(ch) != 0`, glyph id 0 being
/// `.notdef`), mirroring `font_coverage::classify_font_bytes_for`. An unreadable
/// or unparseable file is logged once and contributes nothing — the alternative,
/// claiming coverage it cannot deliver, would offer a variant that renders tofu.
///
/// Indices in each value are ascending because `probes` is walked in order.
#[must_use]
pub(super) fn compute_coverage(probes: &[FontProbe], chars: &[char]) -> HashMap<char, Vec<usize>> {
    let mut map: HashMap<char, Vec<usize>> = HashMap::new();
    for probe in probes {
        let bytes = match std::fs::read(&probe.path) {
            Ok(bytes) => bytes,
            Err(err) => {
                crate::runtime_log::log_warn(format!(
                    "typing: char table coverage: cannot read font file. Path: {} Error: {err}",
                    probe.path.display()
                ));
                continue;
            }
        };
        let Some(font) = swash::FontRef::from_index(&bytes, probe.face_index) else {
            crate::runtime_log::log_warn(format!(
                "typing: char table coverage: cannot parse face {} of font file. Path: {}",
                probe.face_index,
                probe.path.display()
            ));
            continue;
        };
        // Build the charmap once per font, then loop the characters against it.
        let charmap = font.charmap();
        for &ch in chars {
            if charmap.map(ch) != 0 {
                map.entry(ch).or_default().push(probe.index);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Bundled font used as a real-file fixture. `NotoSansMath` covers the math
    /// operators the test probes and is part of the checked-in `fonts/ui/ext`.
    const MATH_FONT: &str = "fonts/ui/ext/10-NotoSansMath-Regular.ttf";
    /// The core Latin/Cyrillic font: it covers `A` but not the astral-plane
    /// musical symbols, so it separates the two probes below.
    const CORE_FONT: &str = "fonts/ui/core/00-NotoSans-Regular.ttf";

    /// Absolute path of an in-repo fixture, resolved from the crate manifest dir
    /// so the test does not depend on the working directory.
    fn repo_path(relative: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    #[test]
    fn maps_characters_to_the_fonts_that_cover_them() {
        let core = repo_path(CORE_FONT);
        let math = repo_path(MATH_FONT);
        assert!(core.is_file(), "missing fixture {}", core.display());
        assert!(math.is_file(), "missing fixture {}", math.display());

        let probes = vec![
            FontProbe {
                index: 3,
                path: core,
                face_index: 0,
            },
            FontProbe {
                index: 7,
                path: math,
                face_index: 0,
            },
        ];
        // 'A' is in the core font; '∑' (U+2211) is a math operator the math font
        // certainly carries; U+E000 is a private-use codepoint neither has.
        let map = compute_coverage(&probes, &['A', '\u{2211}', '\u{E000}']);

        let a = map.get(&'A').map(Vec::as_slice).unwrap_or_default();
        assert!(a.contains(&3), "the core font must cover 'A': {a:?}");

        let sigma = map.get(&'\u{2211}').map(Vec::as_slice).unwrap_or_default();
        assert!(
            sigma.contains(&7),
            "the math font must cover U+2211: {sigma:?}"
        );

        assert!(
            !map.contains_key(&'\u{E000}'),
            "a private-use codepoint must map to no font"
        );
    }

    #[test]
    fn indices_are_ascending_and_probe_order_is_preserved() {
        let core = repo_path(CORE_FONT);
        let probes = vec![
            FontProbe {
                index: 2,
                path: core.clone(),
                face_index: 0,
            },
            FontProbe {
                index: 5,
                path: core,
                face_index: 0,
            },
        ];
        let map = compute_coverage(&probes, &['A']);
        assert_eq!(map.get(&'A').map(Vec::as_slice), Some(&[2usize, 5][..]));
    }

    #[test]
    fn an_unreadable_font_file_contributes_nothing() {
        let probes = vec![FontProbe {
            index: 0,
            path: PathBuf::from("/nonexistent/char_table_coverage_fixture.ttf"),
            face_index: 0,
        }];
        // A missing file must be skipped, not panic and not claim coverage.
        assert!(compute_coverage(&probes, &['A']).is_empty());
    }

    #[test]
    fn fingerprint_changes_with_path_or_representative_face() {
        let mut original = String::new();
        append_font_fingerprint(&mut original, "Family", Path::new("/fonts/a.ttc"), 0);
        let mut changed_path = String::new();
        append_font_fingerprint(
            &mut changed_path,
            "Family",
            Path::new("/fonts/b.ttc"),
            0,
        );
        let mut changed_face = String::new();
        append_font_fingerprint(
            &mut changed_face,
            "Family",
            Path::new("/fonts/a.ttc"),
            1,
        );
        assert_ne!(original, changed_path);
        assert_ne!(original, changed_face);
    }
}
