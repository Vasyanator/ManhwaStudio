/*
FILE HEADER (cleaning/tools/watermark_library.rs)

Purpose:
On-disk LIBRARY of measured watermarks: the reusable half of the chapter-level
watermark decomposition («По главе (точное вычитание)», `watermark_removal.rs`).
A library entry is a self-contained directory under `config::watermark_library_dir()`,
so an entry can be copied, backed up or handed to another user as a folder.

Layout of one entry (`<library>/<entry-id>/`):
  entry.json        metadata: identity, name, anchors, search metadata, calibration verdict
  template.png      the RGBA crop the correlation reference is cut from; ABSENT for an EMPTY
                    entry, which is `entry.json` alone
  planes/c.png      fitted `c = alpha*W`, 16-bit RGB, `c = value/65535*255`
  planes/s.png      fitted `s = 1 - alpha`, 16-bit RGB, `s = value/65535`
  samples/NNN.png   one RGBA crop per calibration sample, in file order

Key structures:
- `EntryFile`: the serde model of `entry.json`. The format version is the LOWEST one that can
  express the document (`required_format`): 1 for an entry whose backgrounds were all measured,
  which older builds can still read, 2 once any sample carries a hand-asserted level, and 3 for
  an EMPTY entry — one with a name and an id but no template, no footprint and no samples, which
  is what «+ новый» creates and what the user then adds crops to. `validate_entry_dir` refuses
  every mixture of the two: a footprint, a sample or a plane with no template behind it
  describes geometry no image backs.
- `SaveEntryRequest` / `LoadedEntry`: the plain-data boundary this module speaks.
  Engine types (`WatermarkKind`, `WatermarkModel`, `MarkSignature`) never cross it;
  `watermark_entry.rs` maps between them, which keeps this module pure I/O.
- `EntrySummary`: what a picker needs without decoding any image.

Key functions:
- `save_entry()`, `load_entry()`, `load_entry_template()`, `load_entry_planes()`,
  `list_entries()`, `rename_entry()`, `delete_entry()`;
- `export_entry_zip()`, `export_entry_dir()`, `import_entry()`, `validate_entry_dir()`;
- `new_entry_id()`, `is_valid_entry_id()`.

Notes:
- The CALIBRATION SAMPLES are the reconstruction source, not the plane PNGs: the engine
  builds a model only through `WatermarkKind::refit`, so a loaded entry is refitted from
  its crops. The planes are written for inspection and interchange and are verified
  against a refit by this module's tests. `load_entry_planes` reads them back for DISPLAY
  only — the card's mark-on-white icon — because that costs two small PNG decodes per entry
  whatever the sample count, while a refit costs one decode per crop plus the fit. Anything
  that needs the model itself still refits.
- Every write goes through `write_atomic_bytes`: sibling temp in the SAME directory,
  `write_all` + `sync_all`, CLOSE, `rename`, then an fsync of the containing directory.
  This is the house recipe of `ms-tab-typing`'s `panel/doc_store.rs`, which is not reachable
  from here (it is crate-private to `ms-tab-typing`), so the recipe — not the code — is reused.
- An ENTRY is more than one file, so file atomicity is not enough on its own: `save_entry`
  builds the whole new directory in a staging sibling (`stage_entry`) and swaps it in at the
  end (`commit_entry_dir`). A rewrite that fails — or a process that dies — therefore leaves
  the previous entry intact and valid, instead of metadata naming crops a `samples/` wipe has
  already removed. `import_entry` uses the same stage-then-rename shape.
- Two guards hold on every metadata write, for the same reasons they hold for typing
  documents: a document of a NEWER schema is never overwritten (its unknown fields would
  be lost), and a document changed since it was read is MERGED rather than clobbered —
  the on-disk document is re-read at write time and its additive parts (creation time,
  source list, and every top-level field this build does not know) are carried forward.
- A persisted sample's background is either MEASURED from a flat ring (`Flat`) or ASSERTED by
  the user (`Manual`, no `ring_std` — nothing was measured). A `Flat` record also says how much
  of that ring existed (`ring_pixels` / `ring_full_pixels`): the measurement clips the ring at
  the page border, so a mark stamped against the edge is measured from the sides that exist, and
  the counts are what stop a reader from taking that for a ring measured all the way round.
  They are purely additive and force NO version bump — an older reader that ignores them still
  reads a genuine measurement, which is exactly why this is not the `Manual` situation. An ESTIMATED per-pixel background
  is never written: it is derived from a model rather than measured, so writing one would let a
  later fold-in overwrite a measurement with an estimate
  (`dev-docs/watermark_chapter_decomposition_plan.md`, corrections §5). The `Manual` case is
  what forces format 2; a format-1 reader fails on its unknown tag, which is the intended
  refusal, because the alternative would be reading a claim as a measurement.
- `name` is user data and is stored VERBATIM — never trimmed, cased or normalized, and
  never localized. `id` is persisted identity and stays literal too.
- The origin of a sample is recorded but not required to be a page: an entry calibrated
  from standalone reference crops (the mark on white and on black, where `B` is known
  exactly) is a first-class case of this format.
*/
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};

use image::{ColorType, ImageBuffer, Rgb, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use web_time::{SystemTime, UNIX_EPOCH};

use ms_config as config;

/// Newest on-disk entry format this build understands. A document declaring a HIGHER version is
/// refused rather than guessed at.
///
/// Version 2 added [`StoredSampleBackground::Manual`]. A format-1 reader cannot be shown that
/// document: `background` is an internally tagged enum with no catch-all, so an unknown `kind`
/// is a hard parse failure there — which is the intended outcome, because the alternative would
/// have been reading a level the user merely ASSERTED as if it had been measured.
///
/// Version 3 added the EMPTY entry: one with a name and an id but no template, no footprint and
/// no samples, created the moment the user asks for a new mark and filled in afterwards. Its
/// `template` is `null`, which an older build parses as a type error rather than as a version
/// problem, so the bump is what turns that into the refusal this module already words properly.
pub(super) const WATERMARK_LIBRARY_FORMAT: u32 = 3;

/// Lowest format version that can express an entry with these `samples` and this template.
///
/// A document is written at the LOWEST version that can hold it, not at the newest this build
/// knows: an entry whose backgrounds were all measured is exactly a format-1 document, so it
/// stays readable by older builds instead of being locked out by a version bump it does not use.
/// A hand-asserted background level forces version 2; an absent template forces version 3.
///
/// Ring coverage forces nothing: it is an additive pair of fields on a MEASURED background, so a
/// reader that ignores it still reads exactly the measurement that was made.
fn required_format(samples: &[StoredSampleRef], has_template: bool) -> u32 {
    if !has_template {
        return 3;
    }
    if samples
        .iter()
        .any(|sample| matches!(sample.background, StoredSampleBackground::Manual { .. }))
    {
        2
    } else {
        1
    }
}

/// Largest entry id this module accepts, characters. Ids are directory names.
const MAX_ENTRY_ID_LEN: usize = 64;

/// File names inside an entry directory. Literal, part of the format.
const ENTRY_METADATA_FILE: &str = "entry.json";
const ENTRY_TEMPLATE_FILE: &str = "template.png";
const ENTRY_C_PLANE_FILE: &str = "planes/c.png";
const ENTRY_S_PLANE_FILE: &str = "planes/s.png";
const ENTRY_SAMPLES_DIR: &str = "samples";

/// Encoding scale of the 16-bit plane PNGs. `c` spans 0..=255 LSB and `s` spans 0..=1,
/// so each gets its own scale and the file states which was used.
const C_PLANE_SCALE: f32 = 255.0;
const S_PLANE_SCALE: f32 = 1.0;

/// Largest member an imported archive may expand to, bytes.
///
/// An entry is a handful of small PNGs of at most `CHAPTER_MAX_TEMPLATE_SIDE` per side; this
/// bounds a hostile or corrupt archive to something the machine can hold instead of letting a
/// zip bomb decide how much memory the import job takes.
const MAX_IMPORT_MEMBER_BYTES: u64 = 64 * 1024 * 1024;
/// Largest number of members an imported entry may declare (template + planes + samples).
const MAX_IMPORT_MEMBERS: usize = 256;
/// Extension of an exported entry archive. Literal, part of the interchange format.
pub(super) const ENTRY_ARCHIVE_EXTENSION: &str = "zip";

/// True when `entry_id` is a safe single path segment: ASCII letters, digits, `-` and `_`
/// only, non-empty, at most `MAX_ENTRY_ID_LEN` characters.
///
/// This is the guard `config::watermark_library_entry_dir` documents as its precondition:
/// it is what keeps a hand-edited or imported id from escaping the library root.
#[must_use]
pub(super) fn is_valid_entry_id(entry_id: &str) -> bool {
    !entry_id.is_empty()
        && entry_id.len() <= MAX_ENTRY_ID_LEN
        && entry_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

/// A fresh entry id: `wm-<unix seconds>-<8 hex digits of a time-seeded mix>`.
///
/// Uniqueness is not cryptographic; it only has to avoid a collision between entries
/// created in the same second, which the nanosecond mix covers.
#[must_use]
pub(super) fn new_entry_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    // Cast justification: both halves are deliberately truncated into an id, the value
    // itself carries no meaning beyond being different from its neighbours.
    let seconds = (nanos / 1_000_000_000) as u64;
    let mix = ((nanos as u64) ^ 0x9E37_79B9_7F4A_7C15).wrapping_mul(0x2545_F491_4F6C_DD1D);
    format!("wm-{seconds}-{:08x}", (mix >> 32) as u32)
}

/// Unix seconds now, or 0 when the clock is before the epoch.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------
// Serde model of entry.json
// ---------------------------------------------------------------------------------------

/// How the mark's peak opacity was assumed when the samples could not pin it.
/// Persisted because it is an INPUT to the fit: a reload that ignored it would produce a
/// different model from the same crops.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum StoredAlphaAssumption {
    /// The engine anchors the assumption on the deposit's own lower bound.
    FromDeposit,
    /// A peak opacity from outside this fit, with its honest relative uncertainty.
    Stated {
        peak_alpha: f32,
        uncertainty_percent: f32,
    },
}

/// Shape-independent identity of the mark, as measured when the entry was written.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(super) struct StoredSignature {
    pub reference_level: f32,
    pub deposit_chroma: f32,
    pub mean_deposit: f32,
    pub peak_alpha: f32,
}

/// How well the alpha SCALE was pinned, and what that costs in LSB. The percentage bounds
/// the alpha SCALE only — the alpha map's shape is a separate assumption no verdict bounds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct StoredAlpha {
    /// Literal source tag (`separated_backgrounds` / `estimated_backgrounds` / `assumed`).
    pub source: String,
    pub percent: f32,
    pub rms_lsb: f32,
    pub dark_rms_lsb: f32,
    pub dark_max_lsb: f32,
    pub dark_luma: f32,
}

/// What the entry was calibrated on — the half of the metadata that tells a user whether
/// the entry is the exact case or the graded one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct StoredCalibration {
    /// Literal verdict tag (`separable` / `deposit_exact` / `not_enough_samples` /
    /// `deposit_unavailable` / `underdetermined`).
    pub verdict: String,
    /// Distinct background luma levels the calibration samples sat on, ascending.
    pub levels: Vec<f32>,
    /// Widest gap between them, LSB. Two well-separated levels (ideally black and white)
    /// is the exact case; one level is the graded case.
    pub spread: f32,
    /// Number of calibration samples the fit ran on.
    pub samples: usize,
    /// Literal fit-method tag (`closed_form_flat` / `theil_sen` / `deposit_exact`).
    pub fit_method: Option<String>,
    /// Pixels whose fitted parameters had to be clamped into the physical range.
    pub clamped_pixels: usize,
    pub alpha: Option<StoredAlpha>,
}

/// SEARCH metadata: where this entry has been seen. It is not the storage key — an entry
/// may legitimately apply to several sources, widths and layouts — so matching an open
/// chapter to an entry goes through the engine's signature first and uses these only to
/// rank and explain the candidates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct StoredSourceRef {
    /// Literal key of the source/publisher (the series folder name, in practice).
    pub source_key: String,
    /// Page width the entry was measured on, pixels.
    pub page_width: u32,
    /// Anchor SET as `MarkTemplate::anchor_key` produced it, e.g. `"48,278,523"`.
    pub anchor_key: String,
    /// Identity of this mark WITHIN its chapter catalog, for sources that carry several.
    pub variant_id: String,
    /// Chapter/project folder the measurement came from, for the user's own bookkeeping.
    pub chapter: Option<String>,
}

/// Where one calibration crop came from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum StoredSampleOrigin {
    /// Cut out of a scanned page at `(x, y)`.
    Page {
        page_index: usize,
        x: u32,
        y: u32,
    },
    /// A standalone reference crop of the mark over a known flat background.
    ReferenceCrop,
}

/// What is known about the background under one persisted crop.
///
/// Two cases, and the difference between them is the whole point: `Flat` was MEASURED from a
/// uniform ring, `Manual` was ASSERTED by the user for a crop whose ring the engine refused. A
/// `Manual` record carries no `ring_std` — nothing was measured, and writing `0.0` would record
/// a measurement that never happened — and its presence forces format 2 (see
/// [`required_format`]), so no older reader can mistake the claim for a measurement.
///
/// A `Flat` record additionally says how much of the ring EXISTED. A mark stamped against a page
/// border is measured from the sides that survive clipping, which the engine has always
/// permitted (`validate_calibration_sample` clips the ring and admits it on a pixel COUNT); the
/// two counts are what stop a later reader from mistaking that for a ring measured all the way
/// round. They are purely additive: an entry every one of whose rings was complete serializes
/// exactly the document it serialized before they existed, so no version bump rides on them, and
/// an older build that ignores them still reads a genuine measurement rather than a claim —
/// which is precisely why this is NOT the `Manual` situation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum StoredSampleBackground {
    Flat {
        level: [f32; 3],
        ring_std: [f32; 3],
        /// Ring pixels the level was averaged over. `None` for an entry written before the
        /// coverage was recorded — "unknown", never "complete".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ring_pixels: Option<usize>,
        /// Ring pixels an unclipped ring would have held. `None` alongside `ring_pixels`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ring_full_pixels: Option<usize>,
    },
    Manual {
        level: [f32; 3],
    },
}

impl StoredSampleBackground {
    /// The background level under the crop, per channel, however it was obtained.
    #[must_use]
    pub(super) fn level(&self) -> [f32; 3] {
        match self {
            Self::Flat { level, .. } | Self::Manual { level } => *level,
        }
    }

    /// Per-channel std of the ring the level was measured from, or `None` when the level was
    /// asserted and no ring was ever measured.
    #[must_use]
    pub(super) fn ring_std(&self) -> Option<[f32; 3]> {
        match self {
            Self::Flat { ring_std, .. } => Some(*ring_std),
            Self::Manual { .. } => None,
        }
    }

    /// True when the level was stated by the user rather than measured by the engine.
    #[must_use]
    pub(super) fn is_manual(&self) -> bool {
        matches!(self, Self::Manual { .. })
    }

    /// True when the level was measured from a ring the page edge truncated.
    ///
    /// `false` for an asserted level (there was no ring) and for a record written before the
    /// coverage was stored (it is unknown, and claiming partialness would be as wrong as
    /// claiming completeness).
    #[must_use]
    pub(super) fn has_partial_ring(&self) -> bool {
        match self {
            Self::Flat {
                ring_pixels: Some(pixels),
                ring_full_pixels: Some(full),
                ..
            } => pixels < full,
            Self::Flat { .. } | Self::Manual { .. } => false,
        }
    }
}

/// One calibration crop as recorded in `entry.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct StoredSampleRef {
    /// Path relative to the entry directory.
    pub file: String,
    pub width: u32,
    pub height: u32,
    pub origin: StoredSampleOrigin,
    pub background: StoredSampleBackground,
}

/// Where the fitted planes live and how they are encoded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct StoredPlanes {
    pub c: String,
    pub s: String,
    /// `c = pixel / 65535 * c_scale`, `s = pixel / 65535 * s_scale`.
    pub c_scale: f32,
    pub s_scale: f32,
}

/// The whole `entry.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct EntryFile {
    pub format: u32,
    /// Persisted literal identity. Equals the directory name.
    pub id: String,
    /// User-visible name, stored VERBATIM.
    pub name: String,
    pub created_unix: u64,
    pub updated_unix: u64,
    /// Compositing operator id (`CompositingOperator::id`).
    pub operator: String,
    pub width: u32,
    pub height: u32,
    pub anchors: Vec<u32>,
    pub anchor_key: String,
    pub alpha_assumption: StoredAlphaAssumption,
    pub signature: Option<StoredSignature>,
    pub calibration: StoredCalibration,
    pub sources: Vec<StoredSourceRef>,
    pub samples: Vec<StoredSampleRef>,
    /// Entry-relative path of the correlation template, or `None` for an EMPTY entry that has
    /// no mark yet. An empty entry has `width == 0`, `height == 0`, no anchors, no samples and
    /// no planes; the first calibration crop written into it is what gives it all of them.
    pub template: Option<String>,
    pub planes: Option<StoredPlanes>,
}

// ---------------------------------------------------------------------------------------
// Boundary types
// ---------------------------------------------------------------------------------------

/// One calibration crop handed in for saving, or handed back after loading.
#[derive(Debug, Clone)]
pub(super) struct LibrarySample {
    /// The observed pixels of the mark footprint, exactly as they appeared.
    pub image: RgbaImage,
    pub origin: StoredSampleOrigin,
    pub background: StoredSampleBackground,
}

/// The fitted parameter planes, interleaved RGB, `width*height*3` entries each.
#[derive(Debug, Clone)]
pub(super) struct LibraryPlanes {
    pub c: Vec<f32>,
    pub s: Vec<f32>,
}

/// One entry's stored model, read back WITHOUT decoding its calibration crops.
///
/// This is the cheap half of a stored entry: two small PNGs and the metadata that says how to
/// read them, instead of the template plus every sample plus a refit. It is what a list of
/// entries can afford per row; anything that needs the model's provenance (or exactness beyond
/// the 16-bit plane encoding) must still go through [`load_entry`] and a refit.
#[derive(Debug, Clone)]
pub(super) struct LoadedPlanes {
    pub width: u32,
    pub height: u32,
    /// `CompositingOperator::id` the model was fitted under. The caller must resolve it and
    /// refuse an id it does not implement rather than assuming alpha blending.
    pub operator: String,
    pub planes: LibraryPlanes,
}

/// Everything needed to write one entry.
#[derive(Debug, Clone)]
pub(super) struct SaveEntryRequest {
    /// `None` creates a new entry; `Some(id)` updates an existing one in place, keeping
    /// its creation time and folding the new source reference into its list.
    pub entry_id: Option<String>,
    /// Stored verbatim.
    pub name: String,
    pub operator: String,
    pub width: u32,
    pub height: u32,
    pub anchors: Vec<u32>,
    pub anchor_key: String,
    pub alpha_assumption: StoredAlphaAssumption,
    pub signature: Option<StoredSignature>,
    pub calibration: StoredCalibration,
    pub source: Option<StoredSourceRef>,
    /// The crop the correlation template is cut from, or `None` for an EMPTY entry — one the
    /// user has created but not yet given a single crop. Such a request must also carry a zero
    /// footprint, no samples and no planes; writing a template-less entry that claims a
    /// footprint would describe geometry no image backs.
    pub template: Option<RgbaImage>,
    pub samples: Vec<LibrarySample>,
    pub planes: Option<LibraryPlanes>,
}

/// One entry as read back from disk.
#[derive(Debug, Clone)]
pub(super) struct LoadedEntry {
    pub meta: EntryFile,
    /// `None` for an EMPTY entry: it has no mark yet, so there is nothing to correlate
    /// against and the first crop added to it defines both the template and the footprint.
    pub template: Option<RgbaImage>,
    pub samples: Vec<LibrarySample>,
}

/// One persisted crop whose background level was ASSERTED by the user rather than measured.
///
/// The card needs both halves: the file names WHICH sample rests on a claim, and the level is
/// what was claimed. Neither is localized — `file` is the entry-relative literal path.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ManualBackgroundRef {
    /// Entry-relative path of the crop, as `entry.json` records it.
    pub file: String,
    /// The level the user asserted, per channel, 0..=255.
    pub level: [f32; 3],
}

/// What a picker — and the library window — show without decoding any image.
///
/// It carries everything the quality column needs (`verdict`, `levels`, `spread`, `samples`,
/// `alpha`, `clamped_pixels`, `manual_backgrounds`) and everything auto-match needs
/// (`signature`, `width`, `height`), so neither has to open an entry to decide.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct EntrySummary {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub anchor_key: String,
    pub verdict: String,
    pub levels: Vec<f32>,
    pub spread: f32,
    pub samples: usize,
    /// How well the alpha SCALE was pinned, when the entry carries a model at all.
    pub alpha: Option<StoredAlpha>,
    /// Literal fit-method tag of the stored model, `None` when the entry has no model.
    pub fit_method: Option<String>,
    /// Shape-independent identity, for matching an open chapter's mark against the library.
    pub signature: Option<StoredSignature>,
    pub sources: Vec<StoredSourceRef>,
    pub updated_unix: u64,
    /// On-disk format version of this entry. 1 for a measured-only entry, 2 once any sample
    /// carries an asserted level.
    pub format: u32,
    /// Pixels whose fitted parameters had to be clamped into the physically possible range when
    /// the entry was written. A value outside that range cannot come from alpha compositing, so
    /// a large count is the recorded evidence that the samples disagree pixel for pixel.
    pub clamped_pixels: usize,
    /// Every sample whose background level the user ASSERTED. Empty for an entry built entirely
    /// from measurements; non-empty means the entry may not be described as measured.
    pub manual_backgrounds: Vec<ManualBackgroundRef>,
    /// False for an EMPTY entry: one the user created but has given no crop yet, so it has no
    /// mark to render, no footprint and nothing to match a chapter against.
    pub has_template: bool,
    /// How many samples rest on a background measured from a ring the page edge truncated.
    /// Still measurements — see [`StoredSampleBackground`] — but from fewer, one-sided pixels.
    pub partial_rings: usize,
}

// ---------------------------------------------------------------------------------------
// I/O
// ---------------------------------------------------------------------------------------

/// Writes one entry, creating or replacing its directory contents.
///
/// An update keeps the entry's creation time and unions its source references, so folding
/// a second chapter into an entry enriches it instead of resetting it. The sample list is
/// written as handed in: the caller owns the merge, backgrounds included — a sample may carry
/// either an exactly measured level or one the user ASSERTED, and the stored format version is
/// derived from that (`required_format`), never chosen by the caller. `request.name` is written
/// VERBATIM.
///
/// The write is DIRECTORY-atomic, not merely file-atomic: the new entry is built whole in a
/// staging directory beside the library and only then swapped in (see [`commit_entry_dir`]).
/// A failed or interrupted write therefore leaves the entry exactly as it was, instead of a
/// half-rewritten directory whose metadata names crops that no longer exist.
///
/// # Errors
/// Returns a user-facing message when the id is invalid, when the entry on disk carries a
/// NEWER format than this build understands (it is never overwritten), when a directory
/// cannot be created, when an image cannot be encoded, when the metadata cannot be
/// serialized or written, or when the finished staging directory cannot be swapped in.
pub(super) fn save_entry(request: &SaveEntryRequest) -> Result<String, String> {
    save_entry_in(&config::watermark_library_dir(), request)
}

/// [`save_entry`] against an explicit library root. The root is a parameter so a test can run
/// against a temporary directory instead of the installation's own library, which is why it is
/// visible to the rest of `tools` rather than private here.
pub(super) fn save_entry_in(root: &Path, request: &SaveEntryRequest) -> Result<String, String> {
    let entry_id = match request.entry_id.as_deref() {
        Some(existing) => {
            if !is_valid_entry_id(existing) {
                return Err(tf!(
                    "cleaning.tools.watermark.chapter.library_bad_id_error",
                    id = existing
                ));
            }
            existing.to_string()
        }
        None => new_entry_id(),
    };
    let dir = root.join(&entry_id);
    // Re-read the document at WRITE time, not at read time: a second instance of the
    // application may have changed it since this request was built, and its additive parts
    // must survive rather than be clobbered.
    let previous_raw = read_metadata_value(&dir).ok();
    // The version is read from the RAW document first. A newer format may carry an enum tag this
    // build cannot parse, and `from_value(..).ok()` would swallow that into `None` — which reads
    // exactly like "there is no previous document" and would overwrite it.
    if let Some(raw) = previous_raw.as_ref() {
        refuse_newer_format_value(raw)?;
    }
    let previous = previous_raw
        .as_ref()
        .and_then(|raw| serde_json::from_value::<EntryFile>(raw.clone()).ok());
    refuse_newer_format(previous.as_ref())?;

    fs::create_dir_all(root)
        .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    // The whole entry is built HERE and swapped in afterwards. Nothing below this point may
    // touch `dir`, which is still the entry the user has.
    let staging = root.join(format!(
        ".save-{}-{}",
        std::process::id(),
        now_unix_nanos_mix()
    ));
    let _ = fs::remove_dir_all(&staging);
    let outcome = stage_entry(&staging, &entry_id, request, previous.as_ref(), previous_raw.as_ref())
        .and_then(|()| commit_entry_dir(root, &dir, &staging));
    if outcome.is_err() {
        // A half-written staging directory is the only thing a failure leaves behind, and it
        // is invisible to `list_entries_in` (its name is not a valid entry id). Remove it
        // anyway so a repeated failure cannot fill the library root with debris.
        let _ = fs::remove_dir_all(&staging);
    }
    outcome.map(|()| entry_id)
}

/// Builds the complete new state of one entry inside `staging`.
///
/// `staging` must not exist yet. Every file of the entry is written here — template, crops,
/// planes and `entry.json` — so the directory that comes out is a self-contained entry that
/// [`validate_entry_dir`] accepts. The previous document is passed in rather than re-read,
/// because it belongs to the entry directory this function may not touch.
///
/// # Errors
/// A user-facing message for a directory that cannot be created, an image that cannot be
/// encoded, a plane whose length does not match the footprint, or a failed write.
fn stage_entry(
    staging: &Path,
    entry_id: &str,
    request: &SaveEntryRequest,
    previous: Option<&EntryFile>,
    previous_raw: Option<&Value>,
) -> Result<(), String> {
    // `samples/` is created even for an entry with no crops: the layout is part of the
    // format, and a reader opening the folder should not have to guess whether an absent
    // directory means "no samples" or "a broken write".
    fs::create_dir_all(staging.join(ENTRY_SAMPLES_DIR))
        .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;

    // An EMPTY entry writes no `template.png` at all rather than an invented placeholder: the
    // file's absence and the metadata's `null` are the same fact, and a 1x1 stand-in would be a
    // fake mark every reader downstream would have to learn to distrust.
    let template_file = match request.template.as_ref() {
        Some(template) => {
            save_rgba(template, &staging.join(ENTRY_TEMPLATE_FILE))?;
            Some(ENTRY_TEMPLATE_FILE.to_string())
        }
        None => None,
    };

    let mut sample_refs = Vec::with_capacity(request.samples.len());
    for (index, sample) in request.samples.iter().enumerate() {
        let file = format!("{ENTRY_SAMPLES_DIR}/{index:03}.png");
        save_rgba(&sample.image, &staging.join(&file))?;
        sample_refs.push(StoredSampleRef {
            file,
            width: sample.image.width(),
            height: sample.image.height(),
            origin: sample.origin,
            background: sample.background,
        });
    }

    let planes = match request.planes.as_ref() {
        Some(planes) => {
            save_plane(
                &planes.c,
                request.width,
                request.height,
                C_PLANE_SCALE,
                &staging.join(ENTRY_C_PLANE_FILE),
            )?;
            save_plane(
                &planes.s,
                request.width,
                request.height,
                S_PLANE_SCALE,
                &staging.join(ENTRY_S_PLANE_FILE),
            )?;
            Some(StoredPlanes {
                c: ENTRY_C_PLANE_FILE.to_string(),
                s: ENTRY_S_PLANE_FILE.to_string(),
                c_scale: C_PLANE_SCALE,
                s_scale: S_PLANE_SCALE,
            })
        }
        None => None,
    };

    let now = now_unix();
    let mut sources = previous
        .map(|meta| meta.sources.clone())
        .unwrap_or_default();
    if let Some(source) = request.source.as_ref()
        && !sources.contains(source)
    {
        sources.push(source.clone());
    }
    let meta = EntryFile {
        // The lowest version that can express these samples, so a measured-only entry stays a
        // format-1 document an older build can still read.
        format: required_format(&sample_refs, template_file.is_some()),
        id: entry_id.to_string(),
        name: request.name.clone(),
        created_unix: previous.map_or(now, |meta| meta.created_unix),
        updated_unix: now,
        operator: request.operator.clone(),
        width: request.width,
        height: request.height,
        anchors: request.anchors.clone(),
        anchor_key: request.anchor_key.clone(),
        alpha_assumption: request.alpha_assumption,
        signature: request.signature,
        calibration: request.calibration.clone(),
        sources,
        samples: sample_refs,
        template: template_file,
        planes,
    };
    write_metadata(staging, &meta, previous_raw)
}

/// Swaps a finished staging directory in as `dir`, removing whatever `dir` held before.
///
/// This is the commit point of every entry write, and it is what makes a rewrite survive a
/// crash. A new entry is a single `rename`. Replacing an existing one is two renames — the old
/// directory is moved aside first, because `rename` cannot overwrite a non-empty directory on
/// either supported platform — and the second one is rolled back if it fails, so a failure
/// never leaves the entry missing.
///
/// The remaining crash window is the instant BETWEEN those two renames, which no portable API
/// can close (there is no `RENAME_EXCHANGE` on windows-gnu). It is two directory-metadata
/// operations wide instead of the previous window, which spanned decoding and re-encoding
/// every crop of the entry; and the salvage is a directory sitting in the library root under
/// a `.replaced-*` name rather than a destroyed entry.
///
/// # Errors
/// A user-facing message naming the directory a rename failed on.
fn commit_entry_dir(root: &Path, dir: &Path, staging: &Path) -> Result<(), String> {
    if !dir.exists() {
        rename_entry_dir(staging, dir)?;
        sync_directory(root);
        return Ok(());
    }
    let backup = root.join(format!(
        ".replaced-{}-{}",
        std::process::id(),
        now_unix_nanos_mix()
    ));
    let _ = fs::remove_dir_all(&backup);
    rename_entry_dir(dir, &backup)?;
    if let Err(err) = rename_entry_dir(staging, dir) {
        // The entry must not be left missing because the second rename failed: put the
        // original back before reporting, so the user still has what they had.
        if let Err(restore) = fs::rename(&backup, dir) {
            ms_log::runtime_log::log_error(format!(
                "[cleaning] watermark library: {} could not be restored from {}: {restore}. \
                 The entry's previous contents are in that directory.",
                dir.display(),
                backup.display()
            ));
        }
        return Err(err);
    }
    // Only now is the old content unreachable, so losing this removal costs nothing.
    if let Err(err) = fs::remove_dir_all(&backup) {
        ms_log::runtime_log::log_warn(format!(
            "[cleaning] watermark library: {} was replaced but its previous contents in {} \
             could not be removed: {err}",
            dir.display(),
            backup.display()
        ));
    }
    sync_directory(root);
    Ok(())
}

/// One directory rename of the commit step, with the user-facing message on failure.
fn rename_entry_dir(from: &Path, to: &Path) -> Result<(), String> {
    fs::rename(from, to).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_write_error",
            err = format!("{} -> {}: {err}", from.display(), to.display())
        )
    })
}

/// Replaces one entry's display name, keeping everything else exactly as it is on disk.
///
/// This is a MERGE, not a rewrite: the on-disk document is re-read and only `name` and
/// `updated_unix` change, so a rename cannot destroy a re-measurement another instance of
/// the application wrote in the meantime. `name` is stored VERBATIM — no trim, no case
/// folding, no normalization.
///
/// # Errors
/// A user-facing message for an invalid id, an unreadable or unparsable `entry.json`, a
/// NEWER on-disk format, or a failed write.
pub(super) fn rename_entry(entry_id: &str, name: &str) -> Result<(), String> {
    rename_entry_in(&config::watermark_library_dir(), entry_id, name)
}

/// [`rename_entry`] against an explicit library root (see [`save_entry_in`]).
fn rename_entry_in(root: &Path, entry_id: &str, name: &str) -> Result<(), String> {
    let dir = entry_dir_in(root, entry_id)?;
    let raw = read_metadata_value(&dir)?;
    refuse_newer_format_value(&raw)?;
    let mut meta: EntryFile = parse_metadata_value(&dir, &raw)?;
    refuse_newer_format(Some(&meta))?;
    meta.name = name.to_string();
    meta.updated_unix = now_unix();
    write_metadata(&dir, &meta, Some(&raw))
}

/// Removes one entry's directory and everything in it.
///
/// # Errors
/// A user-facing message for an invalid id or a failed removal. A missing directory is
/// reported as success: the caller asked for the entry to be gone, and it is.
pub(super) fn delete_entry(entry_id: &str) -> Result<(), String> {
    delete_entry_in(&config::watermark_library_dir(), entry_id)
}

/// [`delete_entry`] against an explicit library root (see [`save_entry_in`]).
fn delete_entry_in(root: &Path, entry_id: &str) -> Result<(), String> {
    let dir = entry_dir_in(root, entry_id)?;
    if !dir.exists() {
        return Ok(());
    }
    fs::remove_dir_all(&dir).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_delete_error",
            path = dir.display(),
            err = err
        )
    })
}

/// Directory of one entry inside `root`, after validating the id as a single safe path
/// segment.
///
/// # Errors
/// A user-facing message when the id is not a safe segment — this is the guard that keeps a
/// hand-edited or imported id from escaping the library root.
fn entry_dir_in(root: &Path, entry_id: &str) -> Result<PathBuf, String> {
    if !is_valid_entry_id(entry_id) {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_bad_id_error",
            id = entry_id
        ));
    }
    Ok(root.join(entry_id))
}

/// Refuses a document written by a NEWER build of the format.
///
/// Overwriting it would silently drop the fields that build added, which is exactly the
/// kind of data loss a version number exists to prevent.
///
/// # Errors
/// The user-facing "newer format" message, naming both versions.
fn refuse_newer_format(meta: Option<&EntryFile>) -> Result<(), String> {
    match meta {
        Some(meta) if meta.format > WATERMARK_LIBRARY_FORMAT => Err(tf!(
            "cleaning.tools.watermark.chapter.library_format_error",
            format = meta.format,
            supported = WATERMARK_LIBRARY_FORMAT
        )),
        Some(_) | None => Ok(()),
    }
}

/// The same refusal read from the UNTYPED document, before it is interpreted as an
/// [`EntryFile`].
///
/// The version has to be checked first, not after: a newer format may add an enum VARIANT, and
/// every tagged enum here (`background`, `origin`, `alpha_assumption`) has no catch-all, so an
/// unknown tag fails typed parsing before the `format` field is ever looked at. Checking the raw
/// value turns "invalid JSON for this format" — which says nothing about what to do — into the
/// version message, which names both versions and tells the user to update.
///
/// A document with no `format` field, or a non-integer one, is left to typed parsing to reject:
/// this function only ever refuses a version it can read and compare.
///
/// # Errors
/// The user-facing "newer format" message.
fn refuse_newer_format_value(raw: &Value) -> Result<(), String> {
    let Some(format) = raw
        .get("format")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
    else {
        return Ok(());
    };
    if format > WATERMARK_LIBRARY_FORMAT {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_format_error",
            format = format,
            supported = WATERMARK_LIBRARY_FORMAT
        ));
    }
    Ok(())
}

/// Reads one entry back, including its template and calibration crops.
///
/// # Errors
/// Returns a user-facing message for an invalid id, a missing or unparsable `entry.json`,
/// a format version this build cannot read, a missing image, or a crop whose size does not
/// match the size its record declares.
pub(super) fn load_entry(entry_id: &str) -> Result<LoadedEntry, String> {
    load_entry_in(&config::watermark_library_dir(), entry_id)
}

/// [`load_entry`] against an explicit library root (see [`save_entry_in`]).
pub(super) fn load_entry_in(root: &Path, entry_id: &str) -> Result<LoadedEntry, String> {
    let dir = entry_dir_in(root, entry_id)?;
    let meta = read_metadata(&dir)?;
    refuse_newer_format(Some(&meta))?;
    let template = match meta.template.as_deref() {
        Some(file) => Some(load_entry_image(&dir, file, meta.width, meta.height)?),
        None => None,
    };
    let mut samples = Vec::with_capacity(meta.samples.len());
    for reference in &meta.samples {
        let image = load_entry_image(&dir, &reference.file, reference.width, reference.height)?;
        samples.push(LibrarySample {
            image,
            origin: reference.origin,
            background: reference.background,
        });
    }
    Ok(LoadedEntry {
        meta,
        template,
        samples,
    })
}

/// Decodes ONLY one entry's correlation template — what a list row needs for its preview.
///
/// `Ok(None)` for an EMPTY entry, which declares no template at all. That is a normal state,
/// not a failure, and the caller has to draw something honest for it rather than an error.
///
/// # Errors
/// The same messages [`load_entry`] would produce for that file.
pub(super) fn load_entry_template(entry_id: &str) -> Result<Option<RgbaImage>, String> {
    load_entry_template_in(&config::watermark_library_dir(), entry_id)
}

/// [`load_entry_template`] against an explicit library root (see [`save_entry_in`]).
fn load_entry_template_in(root: &Path, entry_id: &str) -> Result<Option<RgbaImage>, String> {
    let dir = entry_dir_in(root, entry_id)?;
    let meta = read_metadata(&dir)?;
    refuse_newer_format(Some(&meta))?;
    match meta.template.as_deref() {
        Some(file) => load_entry_image(&dir, file, meta.width, meta.height).map(Some),
        None => Ok(None),
    }
}

/// Reads one entry's fitted parameter planes back, without touching its calibration crops.
///
/// `Ok(None)` when the entry declares no planes — i.e. `estimate_model` refused when it was
/// written, so there is no model to read and the caller has nothing to reconstruct. That is a
/// normal state, not an error.
///
/// The planes are the model to within the 16-bit encoding step (`C_PLANE_SCALE/65535` LSB for
/// `c`, `1/65535` for `s`); the exact model is only ever the refit of the crops, which is why
/// this is a reader for display and inspection and not a substitute for [`load_entry`].
///
/// # Errors
/// A user-facing message for an invalid id, a missing or unparsable `entry.json`, a format
/// version this build cannot read, a member path escaping the entry, a plane that is not 16-bit
/// RGB at the template's own size, or a plane scale that is not finite and positive.
pub(super) fn load_entry_planes(entry_id: &str) -> Result<Option<LoadedPlanes>, String> {
    load_entry_planes_in(&config::watermark_library_dir(), entry_id)
}

/// [`load_entry_planes`] against an explicit library root (see [`save_entry_in`]).
pub(super) fn load_entry_planes_in(
    root: &Path,
    entry_id: &str,
) -> Result<Option<LoadedPlanes>, String> {
    let dir = entry_dir_in(root, entry_id)?;
    let meta = read_metadata(&dir)?;
    refuse_newer_format(Some(&meta))?;
    let Some(planes) = meta.planes.as_ref() else {
        return Ok(None);
    };
    if !(planes.c_scale.is_finite()
        && planes.c_scale > 0.0
        && planes.s_scale.is_finite()
        && planes.s_scale > 0.0)
    {
        return Err(t!("cleaning.tools.watermark.chapter.library_plane_scale_error").to_string());
    }
    let c = load_plane(&dir, &planes.c, meta.width, meta.height, planes.c_scale)?;
    let s = load_plane(&dir, &planes.s, meta.width, meta.height, planes.s_scale)?;
    Ok(Some(LoadedPlanes {
        width: meta.width,
        height: meta.height,
        operator: meta.operator.clone(),
        planes: LibraryPlanes { c, s },
    }))
}

/// Decodes one 16-bit plane PNG into interleaved-RGB f32 values, `width*height*3` long.
///
/// The inverse of [`save_plane`]: `value = pixel / 65535 * scale`. The colour type is checked
/// rather than converted, because an 8-bit plane quantizes `s` to 1/255 and would silently
/// describe a different model; and the decoded length is checked against `width*height*3`
/// rather than trusted, so a mismatched file returns a message instead of an index panic.
///
/// # Errors
/// A user-facing message for a member path that escapes the entry, an undecodable file, a wrong
/// colour type, a wrong size, or a raw buffer that is not `width*height*3` long.
fn load_plane(
    dir: &Path,
    file: &str,
    width: u32,
    height: u32,
    scale: f32,
) -> Result<Vec<f32>, String> {
    validate_plane(dir, file, width, height)?;
    let path = entry_member_path(dir, file)?;
    let image = image::open(&path)
        .map_err(|err| {
            tf!(
                "cleaning.tools.watermark.chapter.library_image_read_error",
                path = path.display(),
                err = err
            )
        })?
        .to_rgb16();
    let raw = image.as_raw();
    // `validate_plane` already pinned the dimensions and the colour type, so this multiplication
    // cannot overflow for any image this format accepts; it is checked anyway because the length
    // it produces is what indexes the buffer below.
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| {
            tf!(
                "cleaning.tools.watermark.chapter.library_geometry_error",
                file = file,
                width = width,
                height = height,
                expected_width = width,
                expected_height = height
            )
        })?;
    if raw.len() != expected {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_plane_length_error",
            actual = raw.len(),
            expected = expected
        ));
    }
    Ok(raw
        .iter()
        .map(|&value| f32::from(value) / f32::from(u16::MAX) * scale)
        .collect())
}

/// Decodes one image of an entry and checks it against the size its record declares.
///
/// The relative path is validated as a path INSIDE the entry directory first: an imported
/// `entry.json` is untrusted input, and `../../etc/passwd` is a perfectly valid JSON string.
///
/// # Errors
/// A user-facing message for a path that escapes the entry, an undecodable image, or a size
/// that does not match the declared one.
fn load_entry_image(
    dir: &Path,
    file: &str,
    expected_width: u32,
    expected_height: u32,
) -> Result<RgbaImage, String> {
    let path = entry_member_path(dir, file)?;
    let image = load_rgba(&path)?;
    if image.width() != expected_width || image.height() != expected_height {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_geometry_error",
            file = file,
            width = image.width(),
            height = image.height(),
            expected_width = expected_width,
            expected_height = expected_height
        ));
    }
    Ok(image)
}

/// Lists every readable entry, newest update first, then by name.
///
/// An entry whose metadata cannot be read is skipped and logged rather than failing the
/// whole listing: one corrupt folder must not hide the rest of the library. The log line
/// distinguishes the two reasons, because they call for opposite actions: a document from a
/// NEWER format needs a newer build, while anything else needs the folder looked at.
#[must_use]
pub(super) fn list_entries() -> Vec<EntrySummary> {
    list_entries_in(&config::watermark_library_dir())
}

/// [`list_entries`] against an explicit library root (see [`save_entry_in`]).
fn list_entries_in(root: &Path) -> Vec<EntrySummary> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<EntrySummary> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !is_valid_entry_id(id) {
            continue;
        }
        match read_metadata(&path) {
            Ok(meta) => out.push(summarize(&meta)),
            Err(err) => {
                let declared = read_metadata_value(&path)
                    .ok()
                    .and_then(|raw| raw.get("format").and_then(Value::as_u64));
                match declared {
                    Some(format) if format > u64::from(WATERMARK_LIBRARY_FORMAT) => {
                        ms_log::runtime_log::log_warn(format!(
                            "[cleaning] watermark library entry {} is written in format {format}, \
                             and this build reads up to {WATERMARK_LIBRARY_FORMAT}; it is skipped \
                             rather than read with the fields this build does not know dropped. \
                             Update the application to open it. Reported as: {err}",
                            path.display()
                        ));
                    }
                    Some(_) | None => ms_log::runtime_log::log_warn(format!(
                        "[cleaning] watermark library entry {} is unreadable: {err}",
                        path.display()
                    )),
                }
            }
        }
    }
    out.sort_by(|a, b| {
        b.updated_unix
            .cmp(&a.updated_unix)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// The listing row of one parsed `entry.json`.
fn summarize(meta: &EntryFile) -> EntrySummary {
    EntrySummary {
        id: meta.id.clone(),
        name: meta.name.clone(),
        width: meta.width,
        height: meta.height,
        anchor_key: meta.anchor_key.clone(),
        verdict: meta.calibration.verdict.clone(),
        levels: meta.calibration.levels.clone(),
        spread: meta.calibration.spread,
        samples: meta.samples.len(),
        alpha: meta.calibration.alpha.clone(),
        fit_method: meta.calibration.fit_method.clone(),
        signature: meta.signature,
        sources: meta.sources.clone(),
        updated_unix: meta.updated_unix,
        format: meta.format,
        clamped_pixels: meta.calibration.clamped_pixels,
        has_template: meta.template.is_some(),
        partial_rings: meta
            .samples
            .iter()
            .filter(|sample| sample.background.has_partial_ring())
            .count(),
        manual_backgrounds: meta
            .samples
            .iter()
            .filter(|sample| sample.background.is_manual())
            .map(|sample| ManualBackgroundRef {
                file: sample.file.clone(),
                level: sample.background.level(),
            })
            .collect(),
    }
}

/// Reads and parses `entry.json` of one entry directory.
///
/// # Errors
/// A user-facing message when the file is missing, unreadable, or not valid JSON for this
/// format.
fn read_metadata(dir: &Path) -> Result<EntryFile, String> {
    let raw = read_metadata_value(dir)?;
    refuse_newer_format_value(&raw)?;
    parse_metadata_value(dir, &raw)
}

/// Reads `entry.json` as an untyped JSON value.
///
/// The untyped form is what makes the unknown-field merge possible: a field a NEWER build
/// added is invisible to [`EntryFile`], and re-serializing the typed struct alone would drop
/// it. See [`write_metadata`].
///
/// # Errors
/// A user-facing message when the file is missing, unreadable, or not valid JSON.
fn read_metadata_value(dir: &Path) -> Result<Value, String> {
    let path = dir.join(ENTRY_METADATA_FILE);
    let raw = fs::read_to_string(&path).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_read_error",
            path = path.display(),
            err = err
        )
    })?;
    serde_json::from_str(&raw).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_parse_error",
            path = path.display(),
            err = err
        )
    })
}

/// Interprets an untyped `entry.json` value as this build's [`EntryFile`].
///
/// # Errors
/// A user-facing message naming the file when a required field is missing or mistyped.
fn parse_metadata_value(dir: &Path, raw: &Value) -> Result<EntryFile, String> {
    serde_json::from_value(raw.clone()).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_parse_error",
            path = dir.join(ENTRY_METADATA_FILE).display(),
            err = err
        )
    })
}

/// Serializes `meta` and writes it atomically, carrying every top-level field of
/// `previous` this build does not know into the result.
///
/// That carry-over is the merge half of the write contract: an entry written by a newer
/// build and then renamed here keeps whatever that build recorded, instead of silently
/// losing it. Fields this build DOES know are the caller's — the caller is the newer
/// measurement.
///
/// # Errors
/// A user-facing message when the value cannot be serialized or the file cannot be written.
fn write_metadata(dir: &Path, meta: &EntryFile, previous: Option<&Value>) -> Result<(), String> {
    let mut value = serde_json::to_value(meta).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_serialize_error",
            err = err
        )
    })?;
    if let Some(previous) = previous {
        carry_unknown_fields(&mut value, previous);
    }
    let raw = serde_json::to_string_pretty(&value).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_serialize_error",
            err = err
        )
    })?;
    write_atomic_bytes(&dir.join(ENTRY_METADATA_FILE), raw.as_bytes())
}

/// Copies every top-level key of `previous` that `current` does not already carry.
///
/// Deliberately SHALLOW: a nested unknown field of a known object cannot be told apart from
/// a field this build removed on purpose, and guessing there would resurrect deleted data.
fn carry_unknown_fields(current: &mut Value, previous: &Value) {
    let (Some(current), Some(previous)) = (current.as_object_mut(), previous.as_object()) else {
        return;
    };
    for (key, value) in previous {
        if !current.contains_key(key) {
            current.insert(key.clone(), value.clone());
        }
    }
}

/// Writes an RGBA image as PNG, creating the parent directory when needed.
///
/// The PNG is encoded in memory and then written atomically, so a crash mid-write leaves
/// the previous crop intact instead of a truncated one the reader would refuse.
fn save_rgba(image: &RgbaImage, path: &Path) -> Result<(), String> {
    let bytes = encode_png(|cursor| image.write_to(cursor, image::ImageFormat::Png), path)?;
    write_atomic_bytes(path, &bytes)
}

/// Runs one `image` encoder into an in-memory PNG buffer.
///
/// # Errors
/// The user-facing image-write message, naming `path`, when the encoder fails.
fn encode_png<F>(encode: F, path: &Path) -> Result<Vec<u8>, String>
where
    F: FnOnce(&mut Cursor<Vec<u8>>) -> image::ImageResult<()>,
{
    let mut cursor = Cursor::new(Vec::new());
    encode(&mut cursor).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_image_write_error",
            path = path.display(),
            err = err
        )
    })?;
    Ok(cursor.into_inner())
}

/// Replaces `path` crash-safely: sibling temp file in the SAME directory, `write_all`,
/// `sync_all`, CLOSE the handle, `rename`, then fsync the containing directory.
///
/// The recipe is the one `ms-tab-typing`'s `panel/doc_store.rs` documents; that module is
/// crate-private to `ms-tab-typing` and therefore not reachable from here, so the recipe is
/// reused rather than the code. The temp name carries the process id so two instances of
/// the application never collide on it, and the handle is dropped before any rename or
/// cleanup (deleting an open file fails on Windows).
///
/// # Errors
/// A user-facing message when the parent directory cannot be created, the temp file cannot
/// be written or fsynced, or the rename fails.
fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("entry");
    let temp = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
        // The handle is dropped here, BEFORE the rename below.
    })();
    if let Err(err) = written {
        let _ = fs::remove_file(&temp);
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_write_error",
            err = err
        ));
    }
    if let Err(err) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_write_error",
            err = err
        ));
    }
    sync_directory(parent);
    Ok(())
}

/// Fsyncs `dir` so the rename inside it is on stable storage.
///
/// Best effort: a failure is logged and not propagated, because the data is already
/// renamed into place and refusing the write afterwards would be a lie.
#[cfg(unix)]
fn sync_directory(dir: &Path) {
    match fs::File::open(dir).and_then(|handle| handle.sync_all()) {
        Ok(()) => {}
        Err(err) => ms_log::runtime_log::log_warn(format!(
            "[cleaning] watermark library: cannot fsync {}: {err}",
            dir.display()
        )),
    }
}

/// Windows/wasm: opening a directory handle needs `FILE_FLAG_BACKUP_SEMANTICS`, which
/// `std::fs` does not expose, so the rename's durability rests on the filesystem. Documented
/// no-op, exactly as in `doc_store.rs`.
#[cfg(not(unix))]
fn sync_directory(_dir: &Path) {}

/// Resolves one entry-relative member path, refusing anything that leaves the entry.
///
/// `entry.json` is untrusted whenever it came from an import, and a member path is a plain
/// JSON string: absolute paths, drive prefixes and `..` all have to be rejected here rather
/// than handed to `fs`.
///
/// # Errors
/// A user-facing message naming the offending path.
fn entry_member_path(dir: &Path, file: &str) -> Result<PathBuf, String> {
    let relative = Path::new(file);
    let safe = !file.is_empty()
        && relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if !safe {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_member_path_error",
            file = file
        ));
    }
    Ok(dir.join(relative))
}

/// Decodes an RGBA image of the entry.
fn load_rgba(path: &Path) -> Result<RgbaImage, String> {
    image::open(path)
        .map(|image| image.to_rgba8())
        .map_err(|err| {
            tf!(
                "cleaning.tools.watermark.chapter.library_image_read_error",
                path = path.display(),
                err = err
            )
        })
}

/// Writes one interleaved-RGB f32 plane as a 16-bit PNG scaled by `scale`.
///
/// # Errors
/// [`String`] message when the plane length does not match `width*height*3` or the file
/// cannot be written.
fn save_plane(
    values: &[f32],
    width: u32,
    height: u32,
    scale: f32,
    path: &Path,
) -> Result<(), String> {
    let expected = (width as usize) * (height as usize) * 3;
    if values.len() != expected {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_plane_length_error",
            actual = values.len(),
            expected = expected
        ));
    }
    let raw: Vec<u16> = values
        .iter()
        .map(|&value| {
            // Cast justification: the expression is clamped to 0..=65535 and rounded, so it
            // is exactly representable as u16.
            ((value / scale).clamp(0.0, 1.0) * f32::from(u16::MAX)).round() as u16
        })
        .collect();
    let buffer: ImageBuffer<Rgb<u16>, Vec<u16>> =
        ImageBuffer::from_raw(width, height, raw).ok_or_else(|| {
            tf!(
                "cleaning.tools.watermark.chapter.library_plane_length_error",
                actual = values.len(),
                expected = expected
            )
        })?;
    let bytes = encode_png(
        |cursor| buffer.write_to(cursor, image::ImageFormat::Png),
        path,
    )?;
    write_atomic_bytes(path, &bytes)
}

// ---------------------------------------------------------------------------------------
// Validation, export and import
// ---------------------------------------------------------------------------------------

/// Every file one entry consists of, as entry-relative paths in a stable order.
///
/// This is the interchange manifest: export writes exactly these members and import
/// extracts exactly these members, so a foreign archive cannot smuggle in a file the format
/// does not describe.
fn entry_member_files(meta: &EntryFile) -> Vec<String> {
    // An EMPTY entry has no template file, so its manifest starts with nothing. It is still a
    // complete entry: `entry.json` alone describes it exactly.
    let mut files: Vec<String> = meta.template.clone().into_iter().collect();
    if let Some(planes) = meta.planes.as_ref() {
        files.push(planes.c.clone());
        files.push(planes.s.clone());
    }
    files.extend(meta.samples.iter().map(|sample| sample.file.clone()));
    files
}

/// Structural validation of one entry directory, as strict as the interchange boundary
/// needs: this is what an imported entry has to survive before it is allowed into the
/// library.
///
/// Checks, in order: the metadata parses; the format is not NEWER than this build (a newer
/// entry is refused rather than accepted with its unknown fields dropped); the id is a safe
/// path segment; an entry that HAS a template has a non-degenerate footprint, and one that has
/// none declares no footprint, no samples and no planes either; the member list is bounded;
/// every member path stays inside the entry; the template and every calibration crop decode at
/// exactly the size their record declares; and the plane PNGs, when present, are 16-bit RGB of
/// the template's own size with usable scales.
///
/// # Errors
/// A user-facing message naming the first violated condition.
pub(super) fn validate_entry_dir(dir: &Path) -> Result<EntryFile, String> {
    let raw = read_metadata_value(dir)?;
    refuse_newer_format_value(&raw)?;
    let meta = parse_metadata_value(dir, &raw)?;
    refuse_newer_format(Some(&meta))?;
    if !is_valid_entry_id(&meta.id) {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_bad_id_error",
            id = meta.id.clone()
        ));
    }
    match meta.template.clone() {
        Some(template) => {
            if meta.width == 0 || meta.height == 0 {
                return Err(tf!(
                    "cleaning.tools.watermark.chapter.library_geometry_error",
                    file = template,
                    width = meta.width,
                    height = meta.height,
                    expected_width = 1,
                    expected_height = 1
                ));
            }
        }
        // An EMPTY entry is only empty if it is empty all the way through: a footprint, a
        // sample or a plane with no template behind it describes geometry no image backs, and
        // accepting it would let an import invent a mark this build could never render.
        None => {
            if meta.width != 0
                || meta.height != 0
                || !meta.samples.is_empty()
                || meta.planes.is_some()
            {
                return Err(tf!(
                    "cleaning.tools.watermark.chapter.library_empty_entry_error",
                    id = meta.id.clone(),
                    width = meta.width,
                    height = meta.height,
                    samples = meta.samples.len()
                ));
            }
        }
    }
    let members = entry_member_files(&meta);
    if members.len() > MAX_IMPORT_MEMBERS {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_too_many_members_error",
            count = members.len(),
            limit = MAX_IMPORT_MEMBERS
        ));
    }
    if let Some(template) = meta.template.as_deref() {
        load_entry_image(dir, template, meta.width, meta.height)?;
    }
    for sample in &meta.samples {
        load_entry_image(dir, &sample.file, sample.width, sample.height)?;
    }
    if let Some(planes) = meta.planes.as_ref() {
        if !(planes.c_scale.is_finite()
            && planes.c_scale > 0.0
            && planes.s_scale.is_finite()
            && planes.s_scale > 0.0)
        {
            return Err(t!("cleaning.tools.watermark.chapter.library_plane_scale_error").to_string());
        }
        validate_plane(dir, &planes.c, meta.width, meta.height)?;
        validate_plane(dir, &planes.s, meta.width, meta.height)?;
    }
    Ok(meta)
}

/// Checks that one plane PNG is 16-bit RGB at the template's own size.
///
/// The bit depth is part of the format, not an implementation detail: an 8-bit plane would
/// quantize `s` to 1/255 and silently change the model an entry describes.
///
/// # Errors
/// A user-facing message for an undecodable file, a wrong colour type, or a wrong size.
fn validate_plane(dir: &Path, file: &str, width: u32, height: u32) -> Result<(), String> {
    let path = entry_member_path(dir, file)?;
    let image = image::open(&path).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_image_read_error",
            path = path.display(),
            err = err
        )
    })?;
    if image.color() != ColorType::Rgb16 {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_plane_format_error",
            file = file
        ));
    }
    if image.width() != width || image.height() != height {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_geometry_error",
            file = file,
            width = image.width(),
            height = image.height(),
            expected_width = width,
            expected_height = height
        ));
    }
    Ok(())
}

/// Writes one entry as a single zip archive at `dest`.
///
/// The archive IS the entry directory: members are stored at the archive root under the
/// same relative names `entry.json` uses, so unpacking it anywhere produces a directory
/// [`import_entry`] accepts. The entry is validated before it is packed — exporting a
/// broken entry only spreads it.
///
/// # Errors
/// A user-facing message for an invalid id, an entry that fails validation, or a failed
/// write.
pub(super) fn export_entry_zip(entry_id: &str, dest: &Path) -> Result<(), String> {
    export_entry_zip_in(&config::watermark_library_dir(), entry_id, dest)
}

/// [`export_entry_zip`] against an explicit library root (see [`save_entry_in`]).
fn export_entry_zip_in(root: &Path, entry_id: &str, dest: &Path) -> Result<(), String> {
    let dir = entry_dir_in(root, entry_id)?;
    let meta = validate_entry_dir(&dir)?;
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let mut members = vec![ENTRY_METADATA_FILE.to_string()];
        members.extend(entry_member_files(&meta));
        for member in members {
            let path = entry_member_path(&dir, &member)?;
            let bytes = fs::read(&path).map_err(|err| {
                tf!(
                    "cleaning.tools.watermark.chapter.library_read_error",
                    path = path.display(),
                    err = err
                )
            })?;
            zip.start_file(&member, options)
                .and_then(|()| zip.write_all(&bytes).map_err(zip::result::ZipError::from))
                .map_err(|err| {
                    tf!(
                        "cleaning.tools.watermark.chapter.library_export_error",
                        path = dest.display(),
                        err = err
                    )
                })?;
        }
        zip.finish().map_err(|err| {
            tf!(
                "cleaning.tools.watermark.chapter.library_export_error",
                path = dest.display(),
                err = err
            )
        })?;
    }
    write_atomic_bytes(dest, &buffer.into_inner())
}

/// Copies one entry into `dest_parent` as a plain directory named after its id.
///
/// # Errors
/// A user-facing message for an invalid id, an entry that fails validation, a destination
/// that already exists (it is never overwritten), or a failed copy.
pub(super) fn export_entry_dir(entry_id: &str, dest_parent: &Path) -> Result<PathBuf, String> {
    export_entry_dir_in(&config::watermark_library_dir(), entry_id, dest_parent)
}

/// [`export_entry_dir`] against an explicit library root (see [`save_entry_in`]).
fn export_entry_dir_in(
    root: &Path,
    entry_id: &str,
    dest_parent: &Path,
) -> Result<PathBuf, String> {
    let dir = entry_dir_in(root, entry_id)?;
    let meta = validate_entry_dir(&dir)?;
    let target = dest_parent.join(entry_id);
    if target.exists() {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_export_exists_error",
            path = target.display()
        ));
    }
    let mut members = vec![ENTRY_METADATA_FILE.to_string()];
    members.extend(entry_member_files(&meta));
    for member in members {
        let from = entry_member_path(&dir, &member)?;
        let to = entry_member_path(&target, &member)?;
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
        }
        fs::copy(&from, &to).map_err(|err| {
            tf!(
                "cleaning.tools.watermark.chapter.library_export_error",
                path = to.display(),
                err = err
            )
        })?;
    }
    Ok(target)
}

/// Imports one entry from a zip archive or a plain directory and returns the id it landed
/// under.
///
/// The import is staged: members are unpacked into a private directory beside the library,
/// the whole entry is validated there, and only then is it renamed into place. A foreign
/// entry therefore never half-exists in the library, and a NEWER format is refused with its
/// version named instead of being read with its unknown fields dropped.
///
/// The original id is kept when it is free; otherwise a fresh one is minted, so importing
/// never silently replaces a local entry. The display name is preserved VERBATIM.
///
/// # Errors
/// A user-facing message for an unreadable source, a member that escapes the entry, a
/// member larger than [`MAX_IMPORT_MEMBER_BYTES`], a failed validation, or a failed write.
pub(super) fn import_entry(source: &Path) -> Result<String, String> {
    import_entry_in(&config::watermark_library_dir(), source)
}

/// [`import_entry`] against an explicit library root (see [`save_entry_in`]).
fn import_entry_in(root: &Path, source: &Path) -> Result<String, String> {
    fs::create_dir_all(root)
        .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    let staging = root.join(format!(
        ".import-{}-{}",
        std::process::id(),
        now_unix_nanos_mix()
    ));
    let _ = fs::remove_dir_all(&staging);
    let outcome = stage_import(source, &staging).and_then(|()| finish_import(root, &staging));
    if outcome.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    outcome
}

/// Unpacks `source` (a zip file or a directory) into `staging`.
///
/// Only the members `entry.json` itself declares are unpacked; anything else in the archive
/// is ignored rather than trusted.
///
/// # Errors
/// A user-facing message for an unreadable source, a malformed archive, a member path that
/// escapes the entry, or an oversized member.
fn stage_import(source: &Path, staging: &Path) -> Result<(), String> {
    fs::create_dir_all(staging)
        .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    if source.is_dir() {
        let raw = read_metadata_value(source)?;
        refuse_newer_format_value(&raw)?;
        let meta = parse_metadata_value(source, &raw)?;
        refuse_newer_format(Some(&meta))?;
        let mut members = vec![ENTRY_METADATA_FILE.to_string()];
        members.extend(entry_member_files(&meta));
        if members.len() > MAX_IMPORT_MEMBERS {
            return Err(tf!(
                "cleaning.tools.watermark.chapter.library_too_many_members_error",
                count = members.len(),
                limit = MAX_IMPORT_MEMBERS
            ));
        }
        for member in members {
            let from = entry_member_path(source, &member)?;
            let to = entry_member_path(staging, &member)?;
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
            }
            fs::copy(&from, &to).map_err(|err| {
                tf!(
                    "cleaning.tools.watermark.chapter.library_read_error",
                    path = from.display(),
                    err = err
                )
            })?;
        }
        return Ok(());
    }
    stage_import_zip(source, staging)
}

/// Unpacks the declared members of a zip archive into `staging`.
///
/// # Errors
/// See [`stage_import`].
fn stage_import_zip(source: &Path, staging: &Path) -> Result<(), String> {
    let file = fs::File::open(source).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_read_error",
            path = source.display(),
            err = err
        )
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_import_error",
            path = source.display(),
            err = err
        )
    })?;
    let metadata_bytes = read_zip_member(&mut archive, ENTRY_METADATA_FILE, source)?;
    let raw: Value = serde_json::from_slice(&metadata_bytes).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_parse_error",
            path = source.display(),
            err = err
        )
    })?;
    // Before the typed parse, for the reason `refuse_newer_format_value` documents: a newer
    // archive would otherwise fail with a JSON error instead of the version message.
    refuse_newer_format_value(&raw)?;
    let meta: EntryFile = serde_json::from_value(raw).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_parse_error",
            path = source.display(),
            err = err
        )
    })?;
    refuse_newer_format(Some(&meta))?;
    let members = entry_member_files(&meta);
    if members.len() + 1 > MAX_IMPORT_MEMBERS {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_too_many_members_error",
            count = members.len() + 1,
            limit = MAX_IMPORT_MEMBERS
        ));
    }
    write_staged_member(staging, ENTRY_METADATA_FILE, &metadata_bytes)?;
    for member in members {
        let bytes = read_zip_member(&mut archive, &member, source)?;
        write_staged_member(staging, &member, &bytes)?;
    }
    Ok(())
}

/// Reads one archive member whole, refusing anything above [`MAX_IMPORT_MEMBER_BYTES`].
///
/// # Errors
/// A user-facing message for a missing, oversized or unreadable member.
fn read_zip_member(
    archive: &mut zip::ZipArchive<fs::File>,
    member: &str,
    source: &Path,
) -> Result<Vec<u8>, String> {
    let mut entry = archive.by_name(member).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_import_member_error",
            file = member,
            path = source.display(),
            err = err
        )
    })?;
    if entry.size() > MAX_IMPORT_MEMBER_BYTES {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_member_too_large_error",
            file = member,
            size = entry.size(),
            limit = MAX_IMPORT_MEMBER_BYTES
        ));
    }
    let mut bytes = Vec::new();
    // The declared size is untrusted, so the read is bounded independently of it.
    entry
        .by_ref()
        .take(MAX_IMPORT_MEMBER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| {
            tf!(
                "cleaning.tools.watermark.chapter.library_import_member_error",
                file = member,
                path = source.display(),
                err = err
            )
        })?;
    if bytes.len() as u64 > MAX_IMPORT_MEMBER_BYTES {
        return Err(tf!(
            "cleaning.tools.watermark.chapter.library_member_too_large_error",
            file = member,
            size = bytes.len(),
            limit = MAX_IMPORT_MEMBER_BYTES
        ));
    }
    Ok(bytes)
}

/// Writes one staged member, validating its relative path first.
///
/// # Errors
/// A user-facing message for a path that escapes the staging directory or a failed write.
fn write_staged_member(staging: &Path, member: &str, bytes: &[u8]) -> Result<(), String> {
    let path = entry_member_path(staging, member)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| tf!("cleaning.settings_io.create_dir_error", err = err))?;
    }
    fs::write(&path, bytes).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_write_error",
            err = format!("{}: {err}", path.display())
        )
    })
}

/// Validates a staged import, gives it a free id and moves it into the library.
///
/// # Errors
/// A user-facing message for a failed validation or a failed move.
fn finish_import(root: &Path, staging: &Path) -> Result<String, String> {
    let raw = read_metadata_value(staging)?;
    refuse_newer_format_value(&raw)?;
    let mut meta = validate_entry_dir(staging)?;
    let entry_id = if root.join(&meta.id).exists() {
        // Never replace a local entry silently: an import that collides gets its own id and
        // the user decides which of the two to keep.
        new_entry_id()
    } else {
        meta.id.clone()
    };
    if entry_id != meta.id {
        meta.id.clone_from(&entry_id);
        write_metadata(staging, &meta, Some(&raw))?;
    }
    let target = entry_dir_in(root, &entry_id)?;
    fs::rename(staging, &target).map_err(|err| {
        tf!(
            "cleaning.tools.watermark.chapter.library_import_error",
            path = target.display(),
            err = err
        )
    })?;
    sync_directory(root);
    Ok(entry_id)
}

/// A time-seeded 32-bit mix, for a staging directory name that cannot collide with a
/// concurrent import of the same process.
fn now_unix_nanos_mix() -> u32 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    // Cast justification: the value is a uniqueness token, deliberately truncated; it
    // carries no meaning beyond being different from its neighbours.
    let mix = ((nanos as u64) ^ 0x9E37_79B9_7F4A_7C15).wrapping_mul(0x2545_F491_4F6C_DD1D);
    (mix >> 32) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_request(dir_name: &str) -> SaveEntryRequest {
        let template = RgbaImage::from_fn(4, 3, |x, y| {
            image::Rgba([(x * 40) as u8, (y * 50) as u8, 30, 255])
        });
        SaveEntryRequest {
            entry_id: Some(dir_name.to_string()),
            name: "  Пробел по краям  ".to_string(),
            operator: "alpha_blend".to_string(),
            width: 4,
            height: 3,
            anchors: vec![48, 278, 523],
            anchor_key: "48,278,523".to_string(),
            alpha_assumption: StoredAlphaAssumption::FromDeposit,
            signature: Some(StoredSignature {
                reference_level: 255.0,
                deposit_chroma: 4.0,
                mean_deposit: 60.0,
                peak_alpha: 0.38,
            }),
            calibration: StoredCalibration {
                verdict: "deposit_exact".to_string(),
                levels: vec![255.0],
                spread: 0.0,
                samples: 1,
                fit_method: Some("deposit_exact".to_string()),
                clamped_pixels: 0,
                alpha: Some(StoredAlpha {
                    source: "assumed".to_string(),
                    percent: 30.0,
                    rms_lsb: 9.6,
                    dark_rms_lsb: 14.1,
                    dark_max_lsb: 39.0,
                    dark_luma: 80.0,
                }),
            },
            source: Some(StoredSourceRef {
                source_key: "series".to_string(),
                page_width: 690,
                anchor_key: "48,278,523".to_string(),
                variant_id: "mark-1".to_string(),
                chapter: Some("ch42".to_string()),
            }),
            template: Some(template.clone()),
            samples: vec![LibrarySample {
                image: template,
                origin: StoredSampleOrigin::Page {
                    page_index: 2,
                    x: 48,
                    y: 900,
                },
                background: StoredSampleBackground::Flat {
                    level: [255.0, 255.0, 255.0],
                    ring_std: [0.4, 0.5, 0.6],
                    ring_pixels: Some(180),
                    ring_full_pixels: Some(180),
                },
            }],
            // 4x3 pixels, interleaved RGB: the planes must be exactly `width*height*3`.
            planes: Some(LibraryPlanes {
                c: (0..36).map(|index| (index as f32 * 7.0) % 256.0).collect(),
                s: (0..36).map(|index| 0.05 + index as f32 * 0.02).collect(),
            }),
        }
    }

    #[test]
    fn entry_id_validation_rejects_path_escapes() {
        assert!(is_valid_entry_id("wm-1755-0000abcd"));
        assert!(is_valid_entry_id("A_b-9"));
        assert!(!is_valid_entry_id(""));
        assert!(!is_valid_entry_id(".."));
        assert!(!is_valid_entry_id("a/b"));
        assert!(!is_valid_entry_id("a\\b"));
        assert!(!is_valid_entry_id("проб ел"));
        assert!(!is_valid_entry_id(&"x".repeat(MAX_ENTRY_ID_LEN + 1)));
    }

    #[test]
    fn new_entry_id_is_valid_and_prefixed() {
        let id = new_entry_id();
        assert!(id.starts_with("wm-"), "unexpected id shape: {id}");
        assert!(is_valid_entry_id(&id));
    }

    #[test]
    fn metadata_roundtrips_through_json() {
        let request = sample_request("wm-test-json");
        let meta = EntryFile {
            format: WATERMARK_LIBRARY_FORMAT,
            id: "wm-test-json".to_string(),
            name: request.name.clone(),
            created_unix: 1,
            updated_unix: 2,
            operator: request.operator.clone(),
            width: request.width,
            height: request.height,
            anchors: request.anchors.clone(),
            anchor_key: request.anchor_key.clone(),
            alpha_assumption: request.alpha_assumption,
            signature: request.signature,
            calibration: request.calibration.clone(),
            sources: request.source.clone().into_iter().collect(),
            samples: vec![StoredSampleRef {
                file: "samples/000.png".to_string(),
                width: 4,
                height: 3,
                origin: StoredSampleOrigin::ReferenceCrop,
                background: StoredSampleBackground::Flat {
                    level: [0.0, 0.0, 0.0],
                    ring_std: [0.1, 0.1, 0.1],
                    ring_pixels: Some(90),
                    ring_full_pixels: Some(180),
                },
            }],
            template: Some("template.png".to_string()),
            planes: None,
        };
        let raw = serde_json::to_string(&meta).expect("serialize entry metadata");
        let parsed: EntryFile = serde_json::from_str(&raw).expect("deserialize entry metadata");
        assert_eq!(parsed, meta);
        // The name is user data: whatever the user typed comes back byte for byte.
        assert_eq!(parsed.name, "  Пробел по краям  ");
    }

    /// A private library root for one test, so nothing is written into the installation's
    /// own library (and nothing is left behind in the working tree).
    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("manhwastudio-wm-library-test-{name}"));
        let _ = fs::remove_dir_all(&root);
        root
    }

    /// Full disk round trip against a temporary library root.
    #[test]
    fn entry_roundtrips_through_disk() {
        let id = "wm-selftest-roundtrip";
        let root = temp_root("roundtrip");
        let dir = root.join(id);
        let request = sample_request(id);
        let written = save_entry_in(&root, &request).expect("entry saves");
        assert_eq!(written, id);

        let loaded = load_entry_in(&root, id).expect("entry loads");
        assert_eq!(loaded.meta.id, id);
        assert_eq!(loaded.meta.name, request.name);
        assert_eq!(loaded.meta.anchors, request.anchors);
        assert_eq!(loaded.meta.calibration, request.calibration);
        assert_eq!(loaded.meta.sources.len(), 1);
        assert_eq!(loaded.samples.len(), 1);
        assert_eq!(loaded.samples[0].background, request.samples[0].background);
        assert_eq!(loaded.template.as_ref().expect("template").dimensions(), (4, 3));
        assert_eq!(
            loaded.samples[0].image.as_raw(),
            request.samples[0].image.as_raw()
        );

        // The planes on disk must describe the same model the crops reconstruct, within the
        // 16-bit encoding step (255/65535 LSB for c, 1/65535 for s).
        let planes = request.planes.as_ref().expect("request carries planes");
        // Read through the PRODUCTION reader, not a test-local decoder: the reader is what the
        // card's icon goes through, so this is also its round-trip test.
        let read_back = load_entry_planes_in(&root, id)
            .expect("planes read")
            .expect("a fitted entry declares planes");
        assert_eq!((read_back.width, read_back.height), (4, 3));
        assert_eq!(read_back.operator, "alpha_blend");
        let c = read_back.planes.c;
        let s = read_back.planes.s;
        let _ = &dir;
        assert_eq!(c.len(), planes.c.len());
        for (read, written) in c.iter().zip(planes.c.iter()) {
            assert!((read - written).abs() <= 0.01, "c drifted: {read} vs {written}");
        }
        for (read, written) in s.iter().zip(planes.s.iter()) {
            assert!(
                (read - written).abs() <= 0.0001,
                "s drifted: {read} vs {written}"
            );
        }

        // An update keeps the creation time and does not duplicate a known source.
        let mut again = sample_request(id);
        again.name = "Второе имя".to_string();
        save_entry_in(&root, &again).expect("entry updates");
        let updated = load_entry_in(&root, id).expect("updated entry loads");
        assert_eq!(updated.meta.created_unix, loaded.meta.created_unix);
        assert_eq!(updated.meta.name, "Второе имя");
        assert_eq!(updated.meta.sources.len(), 1);

        let summaries = list_entries_in(&root);
        assert!(
            summaries.iter().any(|summary| summary.id == id),
            "the saved entry must be listed"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_shorter_sample_list_leaves_no_stale_crop() {
        let id = "wm-selftest-shrink";
        let root = temp_root("shrink");
        let mut request = sample_request(id);
        let extra = request.samples[0].clone();
        request.samples.push(extra);
        save_entry_in(&root, &request).expect("entry saves");
        assert!(root.join(id).join("samples/001.png").exists());

        request.samples.truncate(1);
        save_entry_in(&root, &request).expect("entry saves again");
        assert!(!root.join(id).join("samples/001.png").exists());
        assert_eq!(
            load_entry_in(&root, id).expect("entry loads").samples.len(),
            1
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A rewrite that fails part way through must leave the entry EXACTLY as it was.
    ///
    /// This is the whole point of staging the directory: the previous shape wiped `samples/`
    /// first and wrote `entry.json` last, so a failure in between left metadata naming crops
    /// that no longer existed — an entry `validate_entry_dir` refuses, i.e. a lost entry. The
    /// failure is injected through a plane whose length does not match the footprint, which
    /// `save_plane` rejects AFTER the template and the crops have already been written.
    #[test]
    fn an_interrupted_rewrite_leaves_the_original_entry_intact() {
        let id = "wm-selftest-atomic";
        let root = temp_root("atomic");
        let dir = root.join(id);
        let mut request = sample_request(id);
        let extra = request.samples[0].clone();
        request.samples.push(extra);
        save_entry_in(&root, &request).expect("the first write succeeds");
        let before = fs::read_to_string(dir.join(ENTRY_METADATA_FILE)).expect("metadata reads");

        let mut broken = sample_request(id);
        broken.name = "replacement that must never land".to_string();
        broken.samples.clear();
        broken.planes = Some(LibraryPlanes {
            c: vec![0.0; 3],
            s: vec![0.0; 3],
        });
        let err = save_entry_in(&root, &broken).expect_err("a bad plane length must be refused");
        assert!(!err.is_empty(), "the refusal must carry a reason");

        // Everything the failed write would have destroyed is still there.
        let meta = validate_entry_dir(&dir).expect("the original entry must still validate");
        assert_eq!(meta.samples.len(), 2);
        assert_eq!(meta.name, request.name);
        assert!(dir.join("samples/000.png").exists());
        assert!(dir.join("samples/001.png").exists());
        assert_eq!(
            fs::read_to_string(dir.join(ENTRY_METADATA_FILE)).expect("metadata reads"),
            before,
            "a refused rewrite must not touch the document at all"
        );
        // The staging directory is cleaned up, and it could never be mistaken for an entry
        // anyway: its name is not a valid entry id.
        let leftovers: Vec<String> = fs::read_dir(&root)
            .expect("library root reads")
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name != id)
            .collect();
        assert!(leftovers.is_empty(), "staging debris left behind: {leftovers:?}");
        let _ = fs::remove_dir_all(&root);
    }

    /// A successful rewrite replaces the directory whole: the crops of the new sample list are
    /// the ones on disk, and nothing of the previous one survives.
    #[test]
    fn a_staged_rewrite_replaces_the_directory_and_keeps_the_creation_time() {
        let id = "wm-selftest-swap";
        let root = temp_root("swap");
        let dir = root.join(id);
        let mut request = sample_request(id);
        let extra = request.samples[0].clone();
        request.samples.push(extra);
        save_entry_in(&root, &request).expect("the first write succeeds");
        let created = read_metadata(&dir).expect("metadata reads").created_unix;

        request.samples.truncate(1);
        request.planes = None;
        save_entry_in(&root, &request).expect("the rewrite succeeds");

        let meta = validate_entry_dir(&dir).expect("the rewritten entry validates");
        assert_eq!(meta.samples.len(), 1);
        assert!(meta.planes.is_none());
        assert_eq!(
            meta.created_unix, created,
            "a rewrite keeps the entry's creation time"
        );
        assert!(!dir.join("samples/001.png").exists());
        assert!(
            !dir.join(ENTRY_C_PLANE_FILE).exists(),
            "an entry that lost its model must not keep the planes of the model it had"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Deleting the last hand-ASSERTED crop takes the document back to format 1.
    ///
    /// The version is derived from the samples actually written (`required_format`), never
    /// latched: an entry that no longer rests on a claim is a format-1 document again and must
    /// become readable by an older build, instead of staying locked out by a bump it no longer
    /// uses.
    #[test]
    fn deleting_the_last_manual_crop_demotes_the_document_to_format_1() {
        let id = "wm-selftest-demote";
        let root = temp_root("demote");
        let mut request = sample_request(id);
        let mut asserted = request.samples[0].clone();
        asserted.background = StoredSampleBackground::Manual {
            level: [12.0, 12.0, 12.0],
        };
        request.samples.push(asserted);
        save_entry_in(&root, &request).expect("entry saves");
        assert_eq!(
            read_metadata(&root.join(id)).expect("metadata reads").format,
            2,
            "a hand-asserted level forces format 2"
        );

        request.samples.truncate(1);
        save_entry_in(&root, &request).expect("entry saves again");
        let meta = read_metadata(&root.join(id)).expect("metadata reads");
        assert_eq!(meta.format, 1);
        assert!(
            meta.samples
                .iter()
                .all(|sample| !sample.background.is_manual())
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_refuses_a_newer_format() {
        let id = "wm-selftest-format";
        let root = temp_root("format");
        save_entry_in(&root, &sample_request(id)).expect("entry saves");
        let path = root.join(id).join(ENTRY_METADATA_FILE);
        let raw = fs::read_to_string(&path).expect("metadata reads");
        let mut meta: EntryFile = serde_json::from_str(&raw).expect("metadata parses");
        meta.format = WATERMARK_LIBRARY_FORMAT + 1;
        fs::write(
            &path,
            serde_json::to_string(&meta).expect("metadata serializes"),
        )
        .expect("metadata writes");
        assert!(load_entry_in(&root, id).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_rejects_an_id_that_escapes_the_root() {
        assert!(load_entry("../secrets").is_err());
        assert!(save_entry(&sample_request("../secrets")).is_err());
    }

    /// Rewrites `entry.json` of one entry through a closure, bypassing the writer's own
    /// guards, so a test can plant the state a hostile or newer document would have.
    fn patch_metadata(dir: &Path, patch: impl FnOnce(&mut serde_json::Map<String, Value>)) {
        let path = dir.join(ENTRY_METADATA_FILE);
        let raw = fs::read_to_string(&path).expect("metadata reads");
        let mut value: Value = serde_json::from_str(&raw).expect("metadata parses");
        patch(value.as_object_mut().expect("metadata is an object"));
        fs::write(
            &path,
            serde_json::to_string_pretty(&value).expect("metadata serializes"),
        )
        .expect("metadata writes");
    }

    /// A full export -> import round trip through a zip, across two separate library roots.
    ///
    /// The point of the assertion on `name` is that it is USER DATA: the interchange path
    /// must not trim, case-fold or normalize it anywhere.
    #[test]
    fn export_import_roundtrip_keeps_the_name_byte_for_byte() {
        let id = "wm-selftest-export";
        let source_root = temp_root("export-source");
        let target_root = temp_root("export-target");
        let request = sample_request(id);
        save_entry_in(&source_root, &request).expect("entry saves");

        let archive = source_root.join("exported.zip");
        export_entry_zip_in(&source_root, id, &archive).expect("entry exports");
        assert!(archive.is_file(), "the archive must exist after an export");

        let imported = import_entry_in(&target_root, &archive).expect("entry imports");
        assert_eq!(imported, id, "a free id is preserved across the transfer");
        let loaded = load_entry_in(&target_root, &imported).expect("imported entry loads");
        assert_eq!(loaded.meta.name, request.name);
        assert_eq!(loaded.meta.name.as_bytes(), request.name.as_bytes());
        assert_eq!(loaded.samples.len(), request.samples.len());
        assert_eq!(loaded.template.as_ref().expect("template").dimensions(), (4, 3));
        assert_eq!(loaded.meta.calibration, request.calibration);

        // A second import of the same archive must not replace the first one.
        let again = import_entry_in(&target_root, &archive).expect("entry imports twice");
        assert_ne!(again, imported, "a colliding id must be minted afresh");
        assert_eq!(list_entries_in(&target_root).len(), 2);

        // The directory form of the export is accepted by the same importer.
        let dir_export = source_root.join("as-folder");
        let exported_dir =
            export_entry_dir_in(&source_root, id, &dir_export).expect("entry exports as a folder");
        let from_dir = import_entry_in(&target_root, &exported_dir).expect("folder imports");
        assert_eq!(
            load_entry_in(&target_root, &from_dir)
                .expect("folder entry loads")
                .meta
                .name,
            request.name
        );

        let _ = fs::remove_dir_all(&source_root);
        let _ = fs::remove_dir_all(&target_root);
    }

    #[test]
    fn import_refuses_a_newer_format() {
        let id = "wm-selftest-import-format";
        let source_root = temp_root("import-format-source");
        let target_root = temp_root("import-format-target");
        save_entry_in(&source_root, &sample_request(id)).expect("entry saves");
        patch_metadata(&source_root.join(id), |meta| {
            meta.insert(
                "format".to_string(),
                Value::from(WATERMARK_LIBRARY_FORMAT + 1),
            );
            meta.insert("future_field".to_string(), Value::from("kept"));
        });
        let archive = source_root.join("newer.zip");
        // Exporting validates too, so the archive is built by hand from the raw folder.
        assert!(
            export_entry_zip_in(&source_root, id, &archive).is_err(),
            "a newer entry must not even be exportable through the validating path"
        );
        assert!(
            import_entry_in(&target_root, &source_root.join(id)).is_err(),
            "a newer entry directory must be refused on import"
        );
        assert!(
            list_entries_in(&target_root).is_empty(),
            "a refused import must leave nothing behind"
        );
        let _ = fs::remove_dir_all(&source_root);
        let _ = fs::remove_dir_all(&target_root);
    }

    #[test]
    fn a_newer_document_is_never_overwritten_and_a_rename_merges() {
        let id = "wm-selftest-guards";
        let root = temp_root("guards");
        save_entry_in(&root, &sample_request(id)).expect("entry saves");
        let dir = root.join(id);
        patch_metadata(&dir, |meta| {
            meta.insert("future_field".to_string(), Value::from("kept"));
        });

        // A rename is a merge: the unknown field of a newer writer survives it.
        rename_entry_in(&root, id, "  Новое имя  ").expect("entry renames");
        let raw = read_metadata_value(&dir).expect("metadata reads");
        assert_eq!(
            raw.get("future_field").and_then(Value::as_str),
            Some("kept"),
            "an unknown top-level field must survive a rename"
        );
        assert_eq!(
            raw.get("name").and_then(Value::as_str),
            Some("  Новое имя  "),
            "the display name is stored verbatim"
        );

        // A full save merges the same way.
        let mut again = sample_request(id);
        again.name = "Третье имя".to_string();
        save_entry_in(&root, &again).expect("entry saves again");
        assert_eq!(
            read_metadata_value(&dir)
                .expect("metadata reads")
                .get("future_field")
                .and_then(Value::as_str),
            Some("kept")
        );

        // A NEWER schema is refused outright by every writer.
        patch_metadata(&dir, |meta| {
            meta.insert(
                "format".to_string(),
                Value::from(WATERMARK_LIBRARY_FORMAT + 1),
            );
        });
        assert!(save_entry_in(&root, &again).is_err());
        assert!(rename_entry_in(&root, id, "x").is_err());
        assert!(validate_entry_dir(&dir).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_member_path_that_escapes_the_entry_is_refused() {
        let id = "wm-selftest-escape";
        let root = temp_root("escape");
        save_entry_in(&root, &sample_request(id)).expect("entry saves");
        let dir = root.join(id);
        patch_metadata(&dir, |meta| {
            meta.insert(
                "template".to_string(),
                Value::from("../../outside/template.png"),
            );
        });
        assert!(validate_entry_dir(&dir).is_err());
        assert!(load_entry_in(&root, id).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    /// A crop whose background the user ASSERTED, for the format-2 tests.
    fn manual_sample(template: &RgbaImage) -> LibrarySample {
        LibrarySample {
            image: template.clone(),
            origin: StoredSampleOrigin::ReferenceCrop,
            background: StoredSampleBackground::Manual {
                level: [12.0, 13.0, 14.0],
            },
        }
    }

    /// The format is the LOWEST one that can express the document, so a measured-only entry
    /// stays readable by a build that predates the manual-background variant.
    #[test]
    fn the_format_written_is_the_lowest_that_fits_the_document() {
        let id = "wm-selftest-format-floor";
        let root = temp_root("format-floor");
        let mut request = sample_request(id);
        save_entry_in(&root, &request).expect("measured entry saves");
        assert_eq!(
            load_entry_in(&root, id).expect("entry loads").meta.format,
            1,
            "an entry whose backgrounds were all measured is a format-1 document"
        );

        let template = request.samples[0].image.clone();
        request.samples.push(manual_sample(&template));
        save_entry_in(&root, &request).expect("entry with an asserted level saves");
        assert_eq!(
            load_entry_in(&root, id).expect("entry loads").meta.format,
            2,
            "one asserted level forces the version older builds must refuse"
        );

        // Dropping the asserted sample takes the document back to a version older builds read.
        request.samples.truncate(1);
        save_entry_in(&root, &request).expect("entry saves again");
        assert_eq!(load_entry_in(&root, id).expect("entry loads").meta.format, 1);
        let _ = fs::remove_dir_all(&root);
    }

    /// A manual background survives write -> read -> export -> import byte for byte, and the
    /// listing names which sample carries it without decoding an image.
    #[test]
    fn a_manual_background_roundtrips_through_disk_and_interchange() {
        let id = "wm-selftest-manual";
        let source_root = temp_root("manual-source");
        let target_root = temp_root("manual-target");
        let mut request = sample_request(id);
        let template = request.samples[0].image.clone();
        request.samples.push(manual_sample(&template));
        save_entry_in(&source_root, &request).expect("entry saves");

        let loaded = load_entry_in(&source_root, id).expect("entry loads");
        assert_eq!(loaded.meta.format, 2);
        assert_eq!(
            loaded.samples[1].background,
            StoredSampleBackground::Manual {
                level: [12.0, 13.0, 14.0]
            },
            "the asserted level comes back as an assertion, never as a measurement"
        );
        assert_eq!(loaded.samples[1].background.ring_std(), None);
        assert!(loaded.samples[0].background.ring_std().is_some());

        let summary = list_entries_in(&source_root)
            .into_iter()
            .find(|summary| summary.id == id)
            .expect("the entry is listed");
        assert_eq!(summary.format, 2);
        assert_eq!(summary.manual_backgrounds.len(), 1);
        assert_eq!(summary.manual_backgrounds[0].file, "samples/001.png");
        assert_eq!(summary.manual_backgrounds[0].level, [12.0, 13.0, 14.0]);

        let archive = source_root.join("manual.zip");
        export_entry_zip_in(&source_root, id, &archive).expect("entry exports");
        let imported = import_entry_in(&target_root, &archive).expect("entry imports");
        let round_tripped = load_entry_in(&target_root, &imported).expect("imported entry loads");
        assert_eq!(round_tripped.meta.format, 2);
        assert_eq!(round_tripped.samples[1].background, loaded.samples[1].background);
        assert_eq!(round_tripped.meta.samples.len(), 2);

        let _ = fs::remove_dir_all(&source_root);
        let _ = fs::remove_dir_all(&target_root);
    }

    /// What a build that predates a background variant does with a document carrying it.
    ///
    /// The variant is internally tagged and serde offers no catch-all, so an unknown `kind` is a
    /// hard parse failure — every reader REFUSES the entry rather than reading the level it does
    /// not understand as a measured one, and the listing skips it with a logged reason instead
    /// of half-loading it. Planting an unknown tag is the faithful way to reproduce that from
    /// this build: to a format-1 reader, `"manual"` is exactly such a tag.
    #[test]
    fn an_unknown_background_tag_is_refused_by_every_reader() {
        let id = "wm-selftest-unknown-background";
        let root = temp_root("unknown-background");
        save_entry_in(&root, &sample_request(id)).expect("entry saves");
        let dir = root.join(id);
        patch_metadata(&dir, |meta| {
            let samples = meta
                .get_mut("samples")
                .and_then(Value::as_array_mut)
                .expect("samples is an array");
            samples[0]
                .get_mut("background")
                .and_then(Value::as_object_mut)
                .expect("background is an object")
                .insert("kind".to_string(), Value::from("from_a_newer_build"));
        });
        assert!(load_entry_in(&root, id).is_err(), "a reader must refuse it");
        assert!(validate_entry_dir(&dir).is_err());
        assert!(rename_entry_in(&root, id, "x").is_err());
        assert!(
            list_entries_in(&root).is_empty(),
            "the listing skips it rather than showing a half-read entry"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The version is checked BEFORE the document is interpreted, so a newer entry is refused
    /// with the version message even when it also carries a tag this build cannot parse.
    #[test]
    fn a_newer_format_is_refused_before_typed_parsing() {
        let id = "wm-selftest-newer-first";
        let root = temp_root("newer-first");
        let target_root = temp_root("newer-first-target");
        save_entry_in(&root, &sample_request(id)).expect("entry saves");
        let dir = root.join(id);
        patch_metadata(&dir, |meta| {
            meta.insert(
                "format".to_string(),
                Value::from(WATERMARK_LIBRARY_FORMAT + 1),
            );
            let samples = meta
                .get_mut("samples")
                .and_then(Value::as_array_mut)
                .expect("samples is an array");
            samples[0]
                .get_mut("background")
                .and_then(Value::as_object_mut)
                .expect("background is an object")
                .insert("kind".to_string(), Value::from("from_a_newer_build"));
        });
        let expected = tf!(
            "cleaning.tools.watermark.chapter.library_format_error",
            format = WATERMARK_LIBRARY_FORMAT + 1,
            supported = WATERMARK_LIBRARY_FORMAT
        );
        assert_eq!(
            load_entry_in(&root, id).err(),
            Some(expected.clone()),
            "the user must be told to update, not shown a JSON error"
        );
        assert_eq!(validate_entry_dir(&dir).err(), Some(expected.clone()));
        assert_eq!(
            import_entry_in(&target_root, &dir).err(),
            Some(expected.clone()),
            "import refuses a newer schema than this build"
        );
        // And the newer document is never overwritten by a write that cannot understand it.
        assert_eq!(save_entry_in(&root, &sample_request(id)).err(), Some(expected));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&target_root);
    }

    /// An entry without a fitted model declares no planes, and the reader says so rather than
    /// inventing an empty pair.
    #[test]
    fn an_entry_without_planes_reads_back_as_no_model() {
        let id = "wm-selftest-no-planes";
        let root = temp_root("no-planes");
        let mut request = sample_request(id);
        request.planes = None;
        save_entry_in(&root, &request).expect("entry saves");
        assert!(
            load_entry_planes_in(&root, id)
                .expect("planes read")
                .is_none()
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A plane file that is not what the format declares is refused with a message, never
    /// decoded into a buffer the caller would then index.
    #[test]
    fn a_malformed_plane_is_refused_rather_than_decoded() {
        let id = "wm-selftest-bad-plane";
        let root = temp_root("bad-plane");
        save_entry_in(&root, &sample_request(id)).expect("entry saves");
        let dir = root.join(id);

        // 8-bit instead of 16-bit: the bit depth is part of the format, not a detail.
        let eight_bit: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(4, 3, Rgb([1, 2, 3]));
        eight_bit
            .save(dir.join(ENTRY_C_PLANE_FILE))
            .expect("plane writes");
        assert!(load_entry_planes_in(&root, id).is_err());

        // The right depth at the wrong size.
        let wrong_size: ImageBuffer<Rgb<u16>, Vec<u16>> =
            ImageBuffer::from_pixel(5, 3, Rgb([1, 2, 3]));
        wrong_size
            .save(dir.join(ENTRY_C_PLANE_FILE))
            .expect("plane writes");
        assert!(load_entry_planes_in(&root, id).is_err());

        // A scale the decoder cannot use.
        let good: ImageBuffer<Rgb<u16>, Vec<u16>> = ImageBuffer::from_pixel(4, 3, Rgb([1, 2, 3]));
        good.save(dir.join(ENTRY_C_PLANE_FILE)).expect("plane writes");
        patch_metadata(&dir, |meta| {
            meta.get_mut("planes")
                .and_then(Value::as_object_mut)
                .expect("planes is an object")
                .insert("c_scale".to_string(), Value::from(0.0));
        });
        assert!(load_entry_planes_in(&root, id).is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_removes_the_entry_and_tolerates_a_missing_one() {
        let id = "wm-selftest-delete";
        let root = temp_root("delete");
        save_entry_in(&root, &sample_request(id)).expect("entry saves");
        delete_entry_in(&root, id).expect("entry deletes");
        assert!(!root.join(id).exists());
        delete_entry_in(&root, id).expect("deleting a missing entry is not an error");
        assert!(delete_entry_in(&root, "../escape").is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
