/*
File: crates/ms-text-render/src/font_ligature_patch.rs

Purpose:
Byte-level GSUB patcher that makes it IMPOSSIBLE for a font to produce the glyph
`cmap` maps U+2026 (HORIZONTAL ELLIPSIS) to through a ligature substitution.
Backs `TextRenderParams.force_remove_ellipsis_glyph`: a user who asked for
`…` -> `...` must not get the ellipsis glyph back because the font's `liga`
feature recombines the three periods.

Main responsibilities:
- resolve the ellipsis glyph id through the font's own `cmap` (swash);
- walk the GSUB `LookupList` and drop every `LigatureSubst` (type 4, including
  the ones wrapped in an Extension lookup, type 7) rule whose output is that
  glyph;
- return the ORIGINAL buffer untouched when there is nothing to remove;
- memoize the result process-wide by content id so one font is patched once.

Key types:
- `EllipsisLigatureMode`: the per-`FontSystem` partition key (`Keep`/`Remove`).
- `EllipsisPatchSkip`: the typed reason a font was left as-is.
- `EllipsisPatch`: the patched buffer plus how many rules were removed.
- `TableRange`: the GSUB's own byte range; every read, every offset and the one
  write go through it, so nothing outside the table is reachable.
- `LigatureSetWalk`: the deduplicating, step-budgeted accumulator of the walk.

Key functions:
- `remove_ellipsis_ligatures`: the pure, panic-free patch.
- `ellipsis_free_bytes`: the cached wrapper the font registry calls.

Implementation notes:
- EVERY offset is validated against the GSUB table's own `[offset, offset+length)`
  range (`TableRange`), never merely against the file: `LigatureSet` offsets
  corrupted into pointing at `cmap`, `glyf` or `name` are not followed, and the
  single write seam (`TableRange::write_field_u16`) refuses to write outside the
  table. That is what makes "everything except GSUB is byte-identical" hold for
  a MALFORMED font too, not only for a well-formed one.
- The walk is BOUNDED in time and memory (`MAX_GSUB_WALK_STEPS`,
  `MAX_DISTINCT_LIGATURE_SETS`). Several lookups may share one subtable and
  several subtables one `LigatureSet` — legal in a well-formed font, and the
  normal outcome of a corrupted offset — so the reachable set is deduplicated and
  the declared `lookupCount x subTableCount x ligatureSetCount` product is charged
  against a step budget. Without both, a 5.5 KB crafted file whose counts all
  alias the same structures made the walk allocate 64 000 000 entries (502 MB,
  237 ms) and a slightly larger one aborted the whole process on a 4 GB
  allocation. Exceeding either ceiling is a normal `EllipsisPatchSkip`: the font
  is registered unchanged, with a warning.
- The edit is IN PLACE and size-preserving: inside a `LigatureSet` the offending
  entry is removed from `ligatureOffsets` by shifting the survivors left and
  decrementing `ligatureCount`. Those offsets are measured from the start of the
  `LigatureSet`, which does not move, so the survivors stay valid; the freed tail
  slot becomes dead padding nothing reads. No table is resized, so every sfnt
  directory entry, checksum-independent offset and cross-table reference stays
  correct.
- The tempting alternatives are all WRONG with the parser we ship: blanking a
  record (`componentCount = 0xFFFF`, a zero offset) makes ttf-parser's lazy
  offset array stop at the broken entry (`ttf-parser/src/parser.rs:604-607,
  653-660`) and silently lose every LATER ligature of the same set. Verified on
  a synthetic font whose banned rule sits between two innocent siblings.
- ONLY `LigatureSubst` is touched. `SingleSubst`/`AlternateSubst` rules that emit
  the ellipsis glyph are legitimate (`fonts/ui/core/01-SourceHanSansK-Regular.otf`
  maps it to its full-width form through `aalt`/`fwid`) and removing them would be
  damage, not a fix. Contextual lookups (types 5/6) produce no glyphs themselves —
  they dispatch into the type 1-4 lookups of the same `LookupList`, which this
  walk covers in full.
- TrueType/OpenType COLLECTIONS (`ttcf`) are rejected: their faces share tables,
  so an in-place edit for one face would silently change the others.
- swash is used for the `cmap` lookup (the crate already depends on it and it
  handles every cmap format); the sfnt directory and GSUB are read directly
  because the patch needs absolute byte offsets into the file.
*/

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use crate::font_provider::{FontBytes, FontContent};

/// The codepoint whose glyph must become unreachable through ligatures.
const HORIZONTAL_ELLIPSIS: char = '\u{2026}';

/// Maximum number of ENTRIES the patch cache remembers.
///
/// An entry is either a marker ("these bytes need no patch", which costs nothing
/// beyond the map slot) or a full patched COPY of the font, so this bound alone
/// says nothing about memory — [`MAX_CACHED_PATCHED_BYTES`] is the memory
/// ceiling. It is deliberately smaller than `font_system_pool::MAX_CACHED_FILES`
/// (64, the per-`FontSystem` face bound): this cache is process-global and its
/// entries are owned buffers, while a pooled system's entries are shared `Arc`s
/// of buffers the caller already holds.
const MAX_CACHED_PATCHED_FONTS: usize = 32;

/// Maximum total size of the patched buffers the cache keeps resident.
///
/// The entry ceiling above is not a memory bound: 32 patched copies of a 16 MB
/// CJK face would be 512 MB of permanently resident memory. This one is, and it
/// is the number to change when the cache's footprint matters. A single font
/// LARGER than the ceiling is still cached — re-patching a 100 MB face on every
/// render would be worse — but it evicts everything else first.
const MAX_CACHED_PATCHED_BYTES: usize = 64 * 1024 * 1024;

/// Hard ceiling on how many DISTINCT `LigatureSet`s one GSUB walk may collect.
///
/// `2^16` is the entire domain of `ligatureSetCount`, i.e. everything a single
/// maximal `LigatureSubst` subtable can address, and it caps the collected vector
/// at ~1 MB. Measured over every font reachable from this checkout (2 267 faces:
/// the bundled `fonts/ui` stack, the project's display fonts and
/// `/usr/share/fonts`), the worst real face declares 1 792 distinct sets
/// (`FreeSerif.ttf`), so the ceiling leaves ~36x headroom over reality while
/// refusing the aliasing explosion a corrupted file produces.
const MAX_DISTINCT_LIGATURE_SETS: usize = 65_536;

/// Hard ceiling on how many offset entries one GSUB walk may traverse.
///
/// Charged for every `lookupOffsets`, `subtableOffsets`, `ligatureSetOffsets` and
/// `ligatureOffsets` entry the walk visits, so it bounds the WHOLE patch — a
/// corrupted font can otherwise declare `65535 x 65535 x 65535` visits without
/// growing by a single byte. Over the same 2 267-face corpus the worst real face
/// needs 37 915 steps (`NotoSansSignWriting-Regular.ttf`), so `2^20` leaves ~28x
/// headroom and still costs only a few milliseconds when it is actually spent.
const MAX_GSUB_WALK_STEPS: usize = 1 << 20;

/// Whether the faces loaded into one `FontSystem` have had their
/// ellipsis-producing ligatures removed.
///
/// This is the PARTITION KEY of the `FontSystem` pool, not a per-load flag. A
/// `FontSystem` lives in exactly one mode for its whole life, which is what keeps
/// `FontFaceCache` (keyed by content id alone) honest: the same font may be
/// loaded patched and unpatched in one process, but never into the same system,
/// so neither variant can be served to a render that asked for the other and no
/// two faces of the same `(family, weight, style, stretch)` ever meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum EllipsisLigatureMode {
    /// The font is registered exactly as the caller supplied it.
    #[default]
    Keep,
    /// Every `LigatureSubst` rule that outputs the ellipsis glyph is removed
    /// before registration.
    Remove,
}

/// Why [`remove_ellipsis_ligatures`] left a font unchanged.
///
/// Every variant is a normal, non-fatal outcome: the caller registers the
/// original bytes. [`EllipsisPatchSkip::FontCollection`],
/// [`EllipsisPatchSkip::NoTableDirectory`], [`EllipsisPatchSkip::GsubWalkTooLong`]
/// and [`EllipsisPatchSkip::TooManyLigatureSets`] describe input the renderer
/// cannot reason about and are worth a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EllipsisPatchSkip {
    /// A `ttcf` collection: its faces share tables, so no face can be edited in
    /// isolation.
    FontCollection,
    /// The bytes carry no readable sfnt table directory.
    NoTableDirectory,
    /// The font's `cmap` maps U+2026 to no glyph, so nothing can be banned.
    NoEllipsisGlyph,
    /// The font has no `GSUB` table, so it can perform no substitution at all.
    NoGsubTable,
    /// The font has a `GSUB`, but no ligature rule outputs the ellipsis glyph.
    /// The overwhelmingly common outcome and the reason the patch is a fast path.
    NoLigatureProducesEllipsis,
    /// The GSUB walk hit [`MAX_GSUB_WALK_STEPS`]. In practice this means the
    /// declared `lookupCount`/`subTableCount`/`ligatureSetCount` multiply out far
    /// beyond anything a real font needs, which is what corrupted offsets look
    /// like: many counts aliasing the same few structures.
    GsubWalkTooLong,
    /// The GSUB reaches more distinct `LigatureSet`s than
    /// [`MAX_DISTINCT_LIGATURE_SETS`]. Same cause as
    /// [`EllipsisPatchSkip::GsubWalkTooLong`], seen through the memory ceiling
    /// instead of the time one.
    TooManyLigatureSets,
}

impl EllipsisPatchSkip {
    /// Short human-readable reason, used in log lines.
    #[must_use]
    fn reason(self) -> &'static str {
        match self {
            Self::FontCollection => "font collections (ttcf) share tables between faces",
            Self::NoTableDirectory => "no readable sfnt table directory",
            Self::NoEllipsisGlyph => "cmap maps U+2026 to no glyph",
            Self::NoGsubTable => "no GSUB table",
            Self::NoLigatureProducesEllipsis => "no ligature outputs the ellipsis glyph",
            Self::GsubWalkTooLong => {
                "the GSUB lookup walk exceeded its traversal budget (corrupted offsets?)"
            }
            Self::TooManyLigatureSets => {
                "the GSUB reaches more distinct ligature sets than the ceiling allows \
                 (corrupted offsets?)"
            }
        }
    }
}

/// A successfully patched copy of a font.
#[derive(Debug)]
pub(crate) struct EllipsisPatch {
    /// The patched bytes. Always exactly as long as the input.
    pub(crate) bytes: Vec<u8>,
    /// How many ligature rules were removed (always >= 1).
    pub(crate) removed_rules: usize,
    /// The glyph id the rules used to produce, i.e. `cmap(U+2026)`.
    pub(crate) ellipsis_glyph: u16,
}

/// Removes every GSUB ligature rule that outputs the glyph `data`'s `cmap` maps
/// U+2026 to, returning a patched COPY of the whole file.
///
/// The result is a pure, deterministic function of `data`, and on a WELL-FORMED
/// font the patch is idempotent: patching a patched font yields
/// [`EllipsisPatchSkip::NoLigatureProducesEllipsis`], i.e. no second copy. (A
/// corrupted font whose `LigatureSet`s OVERLAP can still expose a rule on the
/// second pass — one set's offset array is another set's payload — which is
/// harmless: every pass stays deterministic, size-preserving and confined to the
/// GSUB table.) Only
/// `LigatureSubst` (GSUB lookup type 4, also through Extension type 7) is
/// touched; the `cmap`, the glyph count, every outline and every other lookup are
/// left byte-identical, so the real `…` a caller types still renders and every
/// unrelated ligature (`fi`, `ffl`, …) still applies.
///
/// Never panics and never allocates on the common path: malformed or truncated
/// structures make the walk stop reading rather than index out of bounds, every
/// offset is clamped to the GSUB table's own byte range, and the traversal is
/// bounded by [`MAX_GSUB_WALK_STEPS`] / [`MAX_DISTINCT_LIGATURE_SETS`] so no
/// input can make it run or allocate unboundedly.
///
/// # Errors
/// Returns the typed [`EllipsisPatchSkip`] reason when nothing was changed; the
/// caller must then use the original bytes.
pub(crate) fn remove_ellipsis_ligatures(data: &[u8]) -> Result<EllipsisPatch, EllipsisPatchSkip> {
    if data.get(0..4) == Some(b"ttcf") {
        return Err(EllipsisPatchSkip::FontCollection);
    }
    // A single-face sfnt file has its table directory at offset 0; collections,
    // the only layout where it does not, were rejected above.
    let font = swash::FontRef::from_index(data, 0).ok_or(EllipsisPatchSkip::NoTableDirectory)?;
    let ellipsis_glyph = font.charmap().map(HORIZONTAL_ELLIPSIS);
    if ellipsis_glyph == 0 {
        // 0 is `.notdef`: the font simply has no ellipsis to produce.
        return Err(EllipsisPatchSkip::NoEllipsisGlyph);
    }
    let gsub = sfnt_table_range(data, b"GSUB").ok_or(EllipsisPatchSkip::NoGsubTable)?;

    // Locate the ligature sets first and only copy the file when at least one of
    // them holds a banned rule. Walking a 16 MB CJK font takes tens of
    // microseconds, while copying it is far more expensive, so the scan is what
    // keeps the untouched-font case cheap. The list is DEDUPLICATED, so a set
    // several lookups share is counted — and edited — exactly once.
    let ligature_sets = collect_ligature_sets(data, gsub)?;
    let removed_rules: usize = ligature_sets
        .iter()
        .map(|set| count_banned_ligatures(data, gsub, *set, ellipsis_glyph))
        .sum();
    if removed_rules == 0 {
        return Err(EllipsisPatchSkip::NoLigatureProducesEllipsis);
    }

    let mut bytes = data.to_vec();
    for set in ligature_sets {
        drop_banned_ligatures(&mut bytes, gsub, set, ellipsis_glyph);
    }
    Ok(EllipsisPatch {
        bytes,
        removed_rules,
        ellipsis_glyph,
    })
}

/// Font bytes for `content` with no ligature able to produce the ellipsis glyph.
///
/// Returns `content.data` itself (same buffer, no copy) whenever the font needs
/// no patch, which is the case for every bundled `fonts/ui` file and for the vast
/// majority of user fonts. A font that DOES need one is patched once per process:
/// the result is memoized under `content.content_id` and every pooled
/// `FontSystem` in `EllipsisLigatureMode::Remove` then shares that one buffer
/// instead of holding its own copy.
///
/// Concurrency: the patch itself runs OUTSIDE the cache lock (project rule — no
/// lock held across long work), so two threads racing on the same unseen font may
/// both compute it. The patch is deterministic, so the loser's identical buffer is
/// simply dropped.
pub(crate) fn ellipsis_free_bytes(content: &FontContent) -> FontBytes {
    if let Some(cached) = cached_patch(content.content_id) {
        return cached.unwrap_or_else(|| content.data.clone());
    }

    let patched = match remove_ellipsis_ligatures(content.bytes()) {
        Ok(patch) => {
            ms_log::runtime_log::log_info(format!(
                "render font '{}': removed {} ligature rule(s) producing the ellipsis glyph \
                 {} (force_remove_ellipsis_glyph)",
                content.name, patch.removed_rules, patch.ellipsis_glyph
            ));
            Some(Arc::new(patch.bytes) as FontBytes)
        }
        Err(
            skip @ (EllipsisPatchSkip::FontCollection
            | EllipsisPatchSkip::NoTableDirectory
            | EllipsisPatchSkip::GsubWalkTooLong
            | EllipsisPatchSkip::TooManyLigatureSets),
        ) => {
            ms_log::runtime_log::log_warn(format!(
                "render font '{}': cannot remove ellipsis ligatures ({}); the font is registered \
                 unchanged, so a ligature may still turn '...' back into '…'",
                content.name,
                skip.reason()
            ));
            None
        }
        // The ordinary outcomes: nothing to do, nothing to report.
        Err(
            EllipsisPatchSkip::NoEllipsisGlyph
            | EllipsisPatchSkip::NoGsubTable
            | EllipsisPatchSkip::NoLigatureProducesEllipsis,
        ) => None,
    };

    store_patch(content.content_id, patched.clone());
    patched.unwrap_or_else(|| content.data.clone())
}

/// Process-global memo of patch results, keyed by `FontContent::content_id`.
///
/// `Some(bytes)` is a patched copy, `None` records "these bytes need no patch" so
/// the scan is not repeated. Bounded by BOTH `MAX_CACHED_PATCHED_FONTS` (entries)
/// and `MAX_CACHED_PATCHED_BYTES` (resident patched bytes).
///
/// No `Debug`: `FontBytes` is `Arc<dyn AsRef<[u8]> + Send + Sync>`, which has no
/// `Debug` bound to derive from.
struct PatchCache {
    /// The memo itself.
    entries: HashMap<u64, Option<FontBytes>>,
    /// Sum of the lengths of every patched buffer currently in `entries`.
    resident_bytes: usize,
}

static PATCH_CACHE: OnceLock<Mutex<PatchCache>> = OnceLock::new();

/// The memo, created on first use.
fn patch_cache() -> &'static Mutex<PatchCache> {
    PATCH_CACHE.get_or_init(|| {
        Mutex::new(PatchCache {
            entries: HashMap::new(),
            resident_bytes: 0,
        })
    })
}

/// Length in bytes of a memoized decision (`None`, a marker, costs nothing).
fn patched_len(patched: Option<&FontBytes>) -> usize {
    patched.map_or(0, |bytes| (**bytes).as_ref().len())
}

/// The memoized decision for `content_id`: `Some(None)` = needs no patch,
/// `Some(Some(bytes))` = patched copy, `None` = not seen yet.
///
/// Recovers from a poisoned mutex instead of propagating it: the map is never
/// left structurally invalid (nothing but insert/clear runs under the lock).
fn cached_patch(content_id: u64) -> Option<Option<FontBytes>> {
    let guard = match patch_cache().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.entries.get(&content_id).cloned()
}

/// Memoizes the decision for `content_id`, bounding both the entry count
/// (`MAX_CACHED_PATCHED_FONTS`) and the resident patched bytes
/// (`MAX_CACHED_PATCHED_BYTES`).
///
/// When either bound is reached the map is CLEARED rather than evicted one entry
/// at a time: there is no access-recency information to evict by, and a cleared
/// memo is only a cost (the next render of an already-seen font re-patches it
/// into an equal buffer), never a correctness change, because the patch is
/// deterministic. Buffers already handed to a `FontSystem` stay alive through
/// their own `Arc`.
fn store_patch(content_id: u64, patched: Option<FontBytes>) {
    let mut guard = match patch_cache().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let added = patched_len(patched.as_ref());
    let over_entries =
        guard.entries.len() >= MAX_CACHED_PATCHED_FONTS && !guard.entries.contains_key(&content_id);
    let over_bytes = guard.resident_bytes.saturating_add(added) > MAX_CACHED_PATCHED_BYTES;
    if over_entries || over_bytes {
        guard.entries.clear();
        guard.resident_bytes = 0;
    }
    // A re-insert replaces a buffer that was already counted; the clear above may
    // also have dropped it, in which case `insert` reports no previous value.
    let replaced = patched_len(guard.entries.insert(content_id, patched).flatten().as_ref());
    guard.resident_bytes = guard
        .resident_bytes
        .saturating_sub(replaced)
        .saturating_add(added);
}

/// Big-endian `u16` at `offset`, or `None` when it does not fit in `data`.
fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

/// Big-endian `u32` at `offset`, or `None` when it does not fit in `data`.
fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// The half-open byte range one sfnt table occupies inside the file.
///
/// The patch validates every derived offset against the range of the TABLE it was
/// read from, not merely against the file. A `LigatureSet` offset corrupted into
/// pointing at `cmap`, `glyf` or `name` is therefore never followed and — the
/// part that matters — never written to, which is what makes the "everything
/// outside GSUB is byte-identical" contract hold for malformed input too.
#[derive(Debug, Clone, Copy)]
struct TableRange {
    /// Absolute offset of the table's first byte.
    start: usize,
    /// Absolute offset one past its last byte, clamped to the file's end.
    end: usize,
}

impl TableRange {
    /// The range of a table declared at `start` with `length` bytes, clamped to
    /// `data`.
    ///
    /// Returns `None` when the declaration leaves nothing readable inside the
    /// file (a start past the end, or a zero length).
    fn new(data: &[u8], start: usize, length: usize) -> Option<Self> {
        let end = start.checked_add(length)?.min(data.len());
        (start < end).then_some(Self { start, end })
    }

    /// Absolute offset `base + relative`, or `None` when it leaves the table.
    ///
    /// Every offset this module walks from goes through here, which is what makes
    /// the remaining `base + small constant` index math overflow-free on a 32-bit
    /// target (`wasm32`): a validated offset is always below `data.len()`, so
    /// adding a subtable header's fixed size can never wrap.
    fn offset(self, base: usize, relative: usize) -> Option<usize> {
        let absolute = base.checked_add(relative)?;
        (absolute >= self.start && absolute < self.end).then_some(absolute)
    }

    /// Big-endian `u16` at the absolute `offset`, or `None` when the field is not
    /// fully inside the table.
    fn read_u16(self, data: &[u8], offset: usize) -> Option<u16> {
        if offset < self.start || offset.checked_add(2)? > self.end {
            return None;
        }
        read_u16(data, offset)
    }

    /// Big-endian `u32` at the absolute `offset`, bounded like
    /// [`TableRange::read_u16`].
    fn read_u32(self, data: &[u8], offset: usize) -> Option<u32> {
        if offset < self.start || offset.checked_add(4)? > self.end {
            return None;
        }
        read_u32(data, offset)
    }

    /// Big-endian `u16` of the field at `base + delta`.
    fn field_u16(self, data: &[u8], base: usize, delta: usize) -> Option<u16> {
        let at = self.offset(base, delta)?;
        self.read_u16(data, at)
    }

    /// Reads the `u16` offset stored at `base + delta` and resolves it against
    /// `base`, keeping both the field and its target inside the table.
    ///
    /// A zero offset yields `None`: every offset the GSUB walk follows points at
    /// a structure that cannot begin at its own container's first byte (that byte
    /// holds the container's own count or format), so zero only ever means
    /// "absent" or "corrupted".
    fn follow_u16(self, data: &[u8], base: usize, delta: usize) -> Option<usize> {
        let relative = self.field_u16(data, base, delta)?;
        if relative == 0 {
            return None;
        }
        self.offset(base, usize::from(relative))
    }

    /// Writes a big-endian `u16` at `base + delta`, and ONLY when the whole field
    /// lies inside the table.
    ///
    /// This is the single write seam of the patch: a corrupted offset that
    /// escaped the table cannot reach it, so no byte outside GSUB is ever
    /// modified. An out-of-range field is silently left alone — the walk that
    /// produced it already treats such structures as unreadable.
    fn write_field_u16(self, data: &mut [u8], base: usize, delta: usize, value: u16) {
        let Some(at) = self.offset(base, delta) else {
            return;
        };
        let Some(end) = at.checked_add(2) else {
            return;
        };
        if end > self.end {
            return;
        }
        if let Some(slot) = data.get_mut(at..end) {
            slot.copy_from_slice(&value.to_be_bytes());
        }
    }
}

/// Byte range of the sfnt table `tag` in a single-face font file.
///
/// Both the offset AND the length of the directory record are honoured, so the
/// caller can keep every derived offset inside the table. Returns `None` for a
/// collection, a truncated directory, a missing table, or a record that declares
/// nothing readable.
fn sfnt_table_range(data: &[u8], tag: &[u8; 4]) -> Option<TableRange> {
    if data.get(0..4)? == b"ttcf" {
        return None;
    }
    let table_count = read_u16(data, 4)?;
    for index in 0..usize::from(table_count) {
        // Table directory: 12-byte header, then 16-byte records
        // (tag, checksum, offset, length).
        let record = 12usize.checked_add(index.checked_mul(16)?)?;
        if data.get(record..record.checked_add(4)?)? == tag {
            let offset = read_u32(data, record.checked_add(8)?)?;
            let length = read_u32(data, record.checked_add(12)?)?;
            return TableRange::new(
                data,
                usize::try_from(offset).ok()?,
                usize::try_from(length).ok()?,
            );
        }
    }
    None
}

/// One `LigatureSet` reachable from a GSUB, with its entry count already clamped
/// to what the table can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LigatureSetRef {
    /// Absolute offset of the `LigatureSet` inside the file.
    offset: usize,
    /// `ligatureCount` after clamping, so `ligatureOffsets[0..count]` is
    /// guaranteed to lie inside the GSUB table.
    count: u16,
}

/// Bounded accumulator behind [`collect_ligature_sets`].
///
/// Two properties make the walk safe on adversarial input. It DEDUPLICATES set
/// offsets — several lookups may share a subtable and several subtables a
/// `LigatureSet`, which is legal in a well-formed font and the normal outcome of
/// a corrupted offset — and it charges every offset entry it traverses against a
/// global step budget. Together they turn a declared
/// `lookupCount x subTableCount x ligatureSetCount` product of billions into an
/// early, cheap refusal instead of an out-of-memory abort.
struct LigatureSetWalk {
    /// Distinct sets, in first-seen order.
    sets: Vec<LigatureSetRef>,
    /// Membership index for `sets`, keyed by absolute offset.
    seen: HashSet<usize>,
    /// Offset entries the walk may still traverse.
    steps_left: usize,
}

impl LigatureSetWalk {
    /// An empty walk with a full [`MAX_GSUB_WALK_STEPS`] budget.
    fn new() -> Self {
        Self {
            sets: Vec::new(),
            seen: HashSet::new(),
            steps_left: MAX_GSUB_WALK_STEPS,
        }
    }

    /// Charges `steps` traversal steps up front; `false` once the budget cannot
    /// cover them, which aborts the whole patch.
    ///
    /// Charging a loop's declared length BEFORE entering it is deliberate: a
    /// corrupted `count` of 65 535 then costs one comparison rather than 65 535
    /// iterations.
    fn charge(&mut self, steps: usize) -> bool {
        match self.steps_left.checked_sub(steps) {
            Some(left) => {
                self.steps_left = left;
                true
            }
            None => false,
        }
    }

    /// Records the `LigatureSet` at `offset` unless it was already visited.
    ///
    /// # Errors
    /// [`EllipsisPatchSkip::TooManyLigatureSets`] once the distinct-set ceiling is
    /// reached, [`EllipsisPatchSkip::GsubWalkTooLong`] once the set's own ligature
    /// entries do not fit the remaining budget.
    fn push(
        &mut self,
        data: &[u8],
        gsub: TableRange,
        offset: usize,
    ) -> Result<(), EllipsisPatchSkip> {
        if !self.seen.insert(offset) {
            return Ok(());
        }
        if self.sets.len() >= MAX_DISTINCT_LIGATURE_SETS {
            return Err(EllipsisPatchSkip::TooManyLigatureSets);
        }
        let count = clamped_ligature_count(data, gsub, offset);
        // The count/drop passes below will walk exactly these entries, so they
        // are paid for here and the whole patch stays inside one budget.
        if !self.charge(usize::from(count)) {
            return Err(EllipsisPatchSkip::GsubWalkTooLong);
        }
        self.sets.push(LigatureSetRef { offset, count });
        Ok(())
    }
}

/// `ligatureCount` of the `LigatureSet` at `set`, clamped to the number of
/// `ligatureOffsets` entries that actually fit inside the GSUB table.
///
/// The clamp is what bounds the later count/drop passes by the TABLE SIZE rather
/// than by a `u16` a corrupted file can set to 65 535 for every one of its sets.
fn clamped_ligature_count(data: &[u8], gsub: TableRange, set: usize) -> u16 {
    let Some(declared) = gsub.read_u16(data, set) else {
        return 0;
    };
    // `ligatureOffsets` starts at `set + 2` and each entry is 2 bytes.
    let capacity = gsub.end.saturating_sub(set.saturating_add(2)) / 2;
    u16::try_from(capacity).map_or(declared, |capacity| declared.min(capacity))
}

/// Every distinct `LigatureSet` reachable from the GSUB spanning `gsub`.
///
/// Walks the whole `LookupList`, unwrapping Extension lookups (type 7) and
/// keeping only `LigatureSubst` (type 4, format 1) subtables. Every unreadable
/// structure is skipped, so a truncated font yields a shorter list rather than an
/// error, and every offset is validated against `gsub`, so nothing outside the
/// table is ever reached.
///
/// # Errors
/// [`EllipsisPatchSkip::GsubWalkTooLong`] or
/// [`EllipsisPatchSkip::TooManyLigatureSets`] when the declared structure exceeds
/// the traversal ceilings; the caller must then register the font unchanged.
fn collect_ligature_sets(
    data: &[u8],
    gsub: TableRange,
) -> Result<Vec<LigatureSetRef>, EllipsisPatchSkip> {
    let mut walk = LigatureSetWalk::new();
    // GSUB header: majorVersion, minorVersion, scriptListOffset,
    // featureListOffset, lookupListOffset — the last at +8, relative to `gsub`.
    let Some(lookup_list) = gsub.follow_u16(data, gsub.start, 8) else {
        return Ok(walk.sets);
    };
    let Some(lookup_count) = gsub.read_u16(data, lookup_list) else {
        return Ok(walk.sets);
    };
    if !walk.charge(usize::from(lookup_count)) {
        return Err(EllipsisPatchSkip::GsubWalkTooLong);
    }
    for index in 0..usize::from(lookup_count) {
        let Some(lookup) = gsub.follow_u16(data, lookup_list, 2 + index * 2) else {
            continue;
        };
        // Lookup table: lookupType, lookupFlag, subTableCount, subtableOffsets[].
        let (Some(lookup_type), Some(subtable_count)) = (
            gsub.read_u16(data, lookup),
            gsub.field_u16(data, lookup, 4),
        ) else {
            continue;
        };
        if !walk.charge(usize::from(subtable_count)) {
            return Err(EllipsisPatchSkip::GsubWalkTooLong);
        }
        for sub_index in 0..usize::from(subtable_count) {
            let Some(subtable) = gsub.follow_u16(data, lookup, 6 + sub_index * 2) else {
                continue;
            };
            let Some((kind, subtable)) = resolve_extension(data, gsub, lookup_type, subtable)
            else {
                continue;
            };
            if kind == LIGATURE_SUBST_LOOKUP_TYPE {
                collect_ligature_sets_of_subtable(data, gsub, subtable, &mut walk)?;
            }
        }
    }
    Ok(walk.sets)
}

/// GSUB lookup type of `LigatureSubst`.
const LIGATURE_SUBST_LOOKUP_TYPE: u16 = 4;

/// GSUB lookup type of `ExtensionSubst`, the 32-bit-offset wrapper.
const EXTENSION_SUBST_LOOKUP_TYPE: u16 = 7;

/// Resolves an Extension subtable (lookup type 7) to the `(type, offset)` of the
/// subtable it wraps; any other lookup type is returned unchanged.
///
/// Returns `None` when the extension record is truncated, its 32-bit offset does
/// not fit the address space, or the target leaves the GSUB table.
fn resolve_extension(
    data: &[u8],
    gsub: TableRange,
    lookup_type: u16,
    subtable: usize,
) -> Option<(u16, usize)> {
    if lookup_type != EXTENSION_SUBST_LOOKUP_TYPE {
        return Some((lookup_type, subtable));
    }
    // ExtensionSubstFormat1: substFormat, extensionLookupType, extensionOffset
    // (32-bit, relative to the extension subtable itself).
    let inner_type = gsub.field_u16(data, subtable, 2)?;
    let at = gsub.offset(subtable, 4)?;
    let offset = gsub.read_u32(data, at)?;
    let inner = gsub.offset(subtable, usize::try_from(offset).ok()?)?;
    // Nested extensions are forbidden by the specification; refuse to recurse.
    (inner_type != EXTENSION_SUBST_LOOKUP_TYPE).then_some((inner_type, inner))
}

/// Records every `LigatureSet` of one `LigatureSubst` subtable in `walk`.
///
/// The sets are indexed by coverage index, so the coverage table itself does not
/// have to be parsed: iterating `ligatureSetOffsets` in full visits exactly the
/// same sets. Unknown subtable formats are ignored.
///
/// # Errors
/// Propagates the traversal-ceiling refusals of [`LigatureSetWalk`].
fn collect_ligature_sets_of_subtable(
    data: &[u8],
    gsub: TableRange,
    subtable: usize,
    walk: &mut LigatureSetWalk,
) -> Result<(), EllipsisPatchSkip> {
    // LigatureSubstFormat1: substFormat, coverageOffset, ligatureSetCount,
    // ligatureSetOffsets[] — all offsets relative to the subtable.
    if gsub.read_u16(data, subtable) != Some(1) {
        return Ok(());
    }
    let Some(set_count) = gsub.field_u16(data, subtable, 4) else {
        return Ok(());
    };
    if !walk.charge(usize::from(set_count)) {
        return Err(EllipsisPatchSkip::GsubWalkTooLong);
    }
    for index in 0..usize::from(set_count) {
        if let Some(set) = gsub.follow_u16(data, subtable, 6 + index * 2) {
            walk.push(data, gsub, set)?;
        }
    }
    Ok(())
}

/// How many ligatures of `set` output `banned`.
fn count_banned_ligatures(
    data: &[u8],
    gsub: TableRange,
    set: LigatureSetRef,
    banned: u16,
) -> usize {
    (0..usize::from(set.count))
        .filter(|index| ligature_outputs(data, gsub, set.offset, *index, banned))
        .count()
}

/// Whether ligature `index` of the `LigatureSet` at `set` outputs `banned`.
///
/// A record whose offset is zero, unreadable, or outside the GSUB table is
/// treated as "not banned": it is already broken, and rewriting it could only
/// make the set worse.
fn ligature_outputs(
    data: &[u8],
    gsub: TableRange,
    set: usize,
    index: usize,
    banned: u16,
) -> bool {
    // LigatureSet: ligatureCount, ligatureOffsets[] (relative to the set).
    // Ligature: ligatureGlyph, componentCount, componentGlyphIDs[].
    gsub.follow_u16(data, set, 2 + index * 2)
        .and_then(|ligature| gsub.read_u16(data, ligature))
        == Some(banned)
}

/// Removes, in place and without resizing, every ligature of `set` that outputs
/// `banned`.
///
/// The survivors' offsets are shifted down inside `ligatureOffsets` and
/// `ligatureCount` is decremented. Those offsets are measured from the start of
/// the `LigatureSet`, which does not move, so they remain valid; the freed tail
/// slots become dead padding no reader reaches. See the file header for why the
/// obvious alternatives corrupt the set.
///
/// Both the reads and the writes stay inside `gsub`, and `set.count` was clamped
/// to the table when the set was collected, so a corrupted `ligatureCount` cannot
/// turn this into a long loop or an edit of a neighbouring table.
fn drop_banned_ligatures(data: &mut [u8], gsub: TableRange, set: LigatureSetRef, banned: u16) {
    let mut kept = 0usize;
    for index in 0..usize::from(set.count) {
        if ligature_outputs(data, gsub, set.offset, index, banned) {
            continue;
        }
        let Some(offset) = gsub.field_u16(data, set.offset, 2 + index * 2) else {
            continue;
        };
        gsub.write_field_u16(data, set.offset, 2 + kept * 2, offset);
        kept += 1;
    }
    if kept < usize::from(set.count) {
        // `kept <= count <= u16::MAX`, so the conversion cannot fail; keep it
        // fallible anyway rather than widening a panic path into the renderer.
        if let Ok(kept) = u16::try_from(kept) {
            gsub.write_field_u16(data, set.offset, 0, kept);
        }
    }
}

/// The committed ellipsis-ligature test fixture, shared by every test in the
/// crate that needs a font which actually carries the rule.
///
/// The bundled `fonts/ui` stack has none and the display fonts that do are not
/// tracked by Git, so the fixture is generated and committed; see
/// `tools/make_ellipsis_ligature_fixture.py` for its exact contents.
#[cfg(test)]
pub(crate) mod test_fixture {
    use std::path::PathBuf;

    /// Path of the committed fixture, addressed from the crate manifest (a test
    /// binary's working directory is the package root, not the repository root).
    pub(crate) fn path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ellipsis_ligature.ttf")
    }

    /// The fixture bytes. The file is committed, so a missing one is a real
    /// failure, not a reason to skip.
    pub(crate) fn bytes() -> Vec<u8> {
        let path = path();
        match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => panic!(
                "the committed ellipsis-ligature fixture must be readable at {}: {error}",
                path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixture::bytes as fixture_bytes;
    use super::{
        EllipsisPatchSkip, HORIZONTAL_ELLIPSIS, LIGATURE_SUBST_LOOKUP_TYPE, ellipsis_free_bytes,
        read_u16, remove_ellipsis_ligatures, sfnt_table_range,
    };
    use crate::font_provider::{FontContent, font_content_id};
    use std::path::PathBuf;
    use std::sync::Arc;

    /// The patched bytes, or an owned copy of the input when nothing was removed.
    fn patched_or_copy(data: &[u8]) -> Vec<u8> {
        remove_ellipsis_ligatures(data).map_or_else(|_| data.to_vec(), |patch| patch.bytes)
    }

    /// `numGlyphs` from the `maxp` table, read straight from the bytes.
    fn glyph_count(data: &[u8]) -> Option<u16> {
        read_u16(data, sfnt_table_range(data, b"maxp")?.start + 4)
    }

    /// Every `(codepoint, glyph)` pair of the font's character map.
    fn charmap_pairs(data: &[u8]) -> Vec<(u32, u16)> {
        let Some(font) = swash::FontRef::from_index(data, 0) else {
            return Vec::new();
        };
        let mut pairs = Vec::new();
        font.charmap().enumerate(|codepoint, glyph| {
            pairs.push((codepoint, glyph));
        });
        pairs
    }

    /// Asserts the invariants the patch must hold for ANY font: the result still
    /// parses, keeps its size, character map and glyph count, and the patch is
    /// deterministic and idempotent. Shared by the fixture test and the two
    /// corpus sweeps.
    pub(crate) fn assert_patch_invariants(label: &str, original: &[u8]) {
        let patched = patched_or_copy(original);
        assert_eq!(
            patched.len(),
            original.len(),
            "{label}: the patch must not resize the file"
        );
        assert!(
            swash::FontRef::from_index(&patched, 0).is_some(),
            "{label}: the patched font must still parse"
        );
        assert_eq!(
            charmap_pairs(&patched),
            charmap_pairs(original),
            "{label}: the patch must not touch the character map"
        );
        assert_eq!(
            glyph_count(&patched),
            glyph_count(original),
            "{label}: the patch must not change the glyph count"
        );
        assert_eq!(
            patched_or_copy(original),
            patched,
            "{label}: the patch must be deterministic"
        );
        // Idempotency. A font that WAS patched must report the specific "nothing
        // left" reason on the second pass; a font that was skipped for another
        // reason (no GSUB, no ellipsis glyph) simply keeps reporting that reason.
        if remove_ellipsis_ligatures(original).is_ok() {
            assert_eq!(
                remove_ellipsis_ligatures(&patched).err(),
                Some(EllipsisPatchSkip::NoLigatureProducesEllipsis),
                "{label}: patching a patched font must find nothing left to remove"
            );
        } else {
            assert!(
                remove_ellipsis_ligatures(&patched).is_err(),
                "{label}: a font the patcher skipped must stay skipped"
            );
        }
        assert_eq!(
            patched_or_copy(&patched),
            patched,
            "{label}: the patch must be idempotent"
        );
    }

    #[test]
    fn the_fixture_loses_exactly_the_ellipsis_rule() {
        let original = fixture_bytes();
        let patch = remove_ellipsis_ligatures(&original)
            .expect("the fixture ships a `period period period -> ellipsis` rule");
        assert_eq!(
            patch.removed_rules, 1,
            "exactly one rule of the fixture outputs the ellipsis glyph"
        );
        let font = swash::FontRef::from_index(&original, 0).expect("the fixture must parse");
        assert_eq!(
            patch.ellipsis_glyph,
            font.charmap().map(HORIZONTAL_ELLIPSIS),
            "the banned glyph must be the one cmap maps U+2026 to"
        );
        assert_patch_invariants("fixture", &original);
    }

    #[test]
    fn a_font_without_the_rule_is_returned_untouched() {
        // Liberation Sans has a GSUB but no ligature producing the ellipsis, so
        // it must take the no-copy fast path.
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/PanelCleaner/pcleaner/data/LiberationSans-Regular.ttf");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!(
                "skipping a_font_without_the_rule_is_returned_untouched: {} is missing",
                path.display()
            );
            return;
        };
        assert_eq!(
            remove_ellipsis_ligatures(&bytes).err(),
            Some(EllipsisPatchSkip::NoLigatureProducesEllipsis)
        );
        assert_patch_invariants("LiberationSans-Regular", &bytes);
    }

    #[test]
    fn a_collection_is_rejected_instead_of_edited() {
        // Faces of a `ttcf` container share tables; a hand-built minimal header
        // is enough to pin the refusal.
        let mut collection = b"ttcf".to_vec();
        collection.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            remove_ellipsis_ligatures(&collection).err(),
            Some(EllipsisPatchSkip::FontCollection)
        );
    }

    #[test]
    fn garbage_is_refused_without_panicking() {
        for data in [
            Vec::new(),
            vec![0u8; 3],
            vec![0u8; 12],
            b"OTTO".to_vec(),
            vec![0xFFu8; 512],
        ] {
            assert!(
                remove_ellipsis_ligatures(&data).is_err(),
                "malformed input must be refused, never patched"
            );
        }
    }

    #[test]
    fn the_cache_returns_the_same_buffer_for_the_same_content() {
        let bytes = fixture_bytes();
        let content = FontContent {
            name: "ellipsis-fixture".to_string(),
            original_name: "ellipsis-fixture".to_string(),
            data: Arc::new(bytes.clone()),
            face_index: 0,
            // A key OF THIS TEST ONLY. The memo is process-global, and tests run
            // in parallel: sharing the fixture's real content id with the loader
            // tests would let a concurrent first-time patch of the same font
            // overwrite the entry between the two calls below — the benign race
            // documented on `ellipsis_free_bytes` — and the pointer check would
            // fail for a reason that is not a bug.
            content_id: font_content_id(&bytes) ^ 0x5f3a_9c11_0000_0001,
        };
        let first = ellipsis_free_bytes(&content);
        let second = ellipsis_free_bytes(&content);
        assert!(
            std::ptr::eq((*first).as_ref().as_ptr(), (*second).as_ref().as_ptr()),
            "a second request for the same content must reuse the cached buffer"
        );
        assert_ne!(
            (*first).as_ref(),
            bytes.as_slice(),
            "the fixture must actually be patched"
        );
    }

    #[test]
    fn an_unpatchable_font_keeps_its_own_buffer_identity() {
        // The buffer identity of a font that needs no patch must survive the
        // call: `font_registry::resident_ids_in` recognises the bundled `fonts/ui`
        // buffers by ADDRESS, so a gratuitous copy here would break it.
        let bytes = b"not a font at all".to_vec();
        let content = FontContent {
            name: "not-a-font".to_string(),
            original_name: "not-a-font".to_string(),
            data: Arc::new(bytes.clone()),
            face_index: 0,
            content_id: font_content_id(&bytes),
        };
        let returned = ellipsis_free_bytes(&content);
        assert!(
            std::ptr::eq(
                (*returned).as_ref().as_ptr(),
                (*content.data).as_ref().as_ptr()
            ),
            "an unpatched font must be handed back as the very same buffer"
        );
    }

    /// The glyph the fixture's `cmap` maps U+2026 to — the one a crafted rule
    /// must output to be banned.
    fn fixture_ellipsis_glyph() -> u16 {
        swash::FontRef::from_index(&fixture_bytes(), 0)
            .expect("the fixture must parse")
            .charmap()
            .map(HORIZONTAL_ELLIPSIS)
    }

    /// The fixture with its `GSUB` directory record repointed at `blob`, appended
    /// to the end of the file and DECLARED as `declared_len` bytes long.
    ///
    /// `declared_len` may deliberately be shorter than `blob`: the remainder then
    /// lies inside the FILE but outside the TABLE, which is exactly the region
    /// the patch must never read from or write to.
    fn font_with_gsub(blob: &[u8], declared_len: usize) -> Vec<u8> {
        let mut out = fixture_bytes();
        let table_count = read_u16(&out, 4).expect("the fixture has a table directory");
        let mut record = None;
        for index in 0..usize::from(table_count) {
            let at = 12 + index * 16;
            if out.get(at..at + 4) == Some(b"GSUB".as_slice()) {
                record = Some(at);
            }
        }
        let record = record.expect("the fixture ships a GSUB record to repoint");
        let start = u32::try_from(out.len()).expect("the fixture is a few KB");
        let length = u32::try_from(declared_len).expect("the crafted blob is a few KB");
        out[record + 8..record + 12].copy_from_slice(&start.to_be_bytes());
        out[record + 12..record + 16].copy_from_slice(&length.to_be_bytes());
        out.extend_from_slice(blob);
        out
    }

    /// Writes the big-endian `u16` `value` at `at`.
    fn put_u16(blob: &mut [u8], at: usize, value: u16) {
        blob[at..at + 2].copy_from_slice(&value.to_be_bytes());
    }

    /// A GSUB whose `n` lookups each declare `n` subtables which each declare `n`
    /// ligature sets — all ALIASING the same three structures, so the declared
    /// product is `n^3` visits over a few dozen real bytes.
    ///
    /// Legal per the specification (sharing subtables between lookups is a real
    /// tool-chain output) and the shape random corruption of a real font produces
    /// constantly. The single reachable `LigatureSet` holds one banned rule.
    fn aliasing_gsub_blob(n: u16, ellipsis: u16) -> Vec<u8> {
        let count = usize::from(n);
        let lookup_list = 10usize;
        let lookup = lookup_list + 2 + count * 2;
        let subtable = lookup + 6 + count * 2;
        let set = subtable + 6 + count * 2;
        let ligature = set + 4;
        let mut blob = vec![0u8; ligature + 6];

        put_u16(&mut blob, 0, 1); // majorVersion
        put_u16(&mut blob, 8, u16::try_from(lookup_list).expect("small blob"));

        put_u16(&mut blob, lookup_list, n); // lookupCount
        let lookup_rel = u16::try_from(lookup - lookup_list).expect("small blob");
        for index in 0..count {
            put_u16(&mut blob, lookup_list + 2 + index * 2, lookup_rel);
        }

        put_u16(&mut blob, lookup, LIGATURE_SUBST_LOOKUP_TYPE);
        put_u16(&mut blob, lookup + 4, n); // subTableCount
        let subtable_rel = u16::try_from(subtable - lookup).expect("small blob");
        for index in 0..count {
            put_u16(&mut blob, lookup + 6 + index * 2, subtable_rel);
        }

        put_u16(&mut blob, subtable, 1); // substFormat
        put_u16(&mut blob, subtable + 4, n); // ligatureSetCount
        let set_rel = u16::try_from(set - subtable).expect("small blob");
        for index in 0..count {
            put_u16(&mut blob, subtable + 6 + index * 2, set_rel);
        }

        put_u16(&mut blob, set, 1); // ligatureCount
        put_u16(&mut blob, set + 2, u16::try_from(ligature - set).expect("small blob"));
        put_u16(&mut blob, ligature, ellipsis); // ligatureGlyph
        put_u16(&mut blob, ligature + 2, 3); // componentCount
        blob
    }

    /// A GSUB with TWO independent `LigatureSet`s, each holding one banned rule,
    /// plus the offset at which the second set begins.
    ///
    /// Declaring the table only `second_set` bytes long leaves the second set
    /// inside the file but outside the table.
    fn two_set_gsub_blob(ellipsis: u16) -> (Vec<u8>, usize) {
        let lookup_list = 10usize;
        let lookup = lookup_list + 4;
        let subtable = lookup + 8;
        let first_set = subtable + 10;
        let first_ligature = first_set + 4;
        let second_set = first_ligature + 8;
        let second_ligature = second_set + 4;
        let mut blob = vec![0u8; second_ligature + 8];

        put_u16(&mut blob, 0, 1);
        put_u16(&mut blob, 8, u16::try_from(lookup_list).expect("small blob"));
        put_u16(&mut blob, lookup_list, 1); // lookupCount
        put_u16(&mut blob, lookup_list + 2, u16::try_from(lookup - lookup_list).expect("small"));
        put_u16(&mut blob, lookup, LIGATURE_SUBST_LOOKUP_TYPE);
        put_u16(&mut blob, lookup + 4, 1); // subTableCount
        put_u16(&mut blob, lookup + 6, u16::try_from(subtable - lookup).expect("small"));
        put_u16(&mut blob, subtable, 1); // substFormat
        put_u16(&mut blob, subtable + 4, 2); // ligatureSetCount
        put_u16(&mut blob, subtable + 6, u16::try_from(first_set - subtable).expect("small"));
        put_u16(&mut blob, subtable + 8, u16::try_from(second_set - subtable).expect("small"));
        for (set, ligature) in [(first_set, first_ligature), (second_set, second_ligature)] {
            put_u16(&mut blob, set, 1); // ligatureCount
            put_u16(&mut blob, set + 2, u16::try_from(ligature - set).expect("small"));
            put_u16(&mut blob, ligature, ellipsis);
            put_u16(&mut blob, ligature + 2, 3); // componentCount
        }
        (blob, second_set)
    }

    /// H1: a GSUB whose declared counts multiply out to billions of visits must
    /// be REFUSED in bounded time and memory, not walked.
    ///
    /// Before the traversal ceilings this exact input (a 5.5 KB file) collected
    /// 64 000 000 aliased set offsets in 502 MB of RSS, and a slightly larger `n`
    /// aborted the process on a 4 GB allocation.
    #[test]
    fn an_aliasing_gsub_is_refused_instead_of_walked() {
        let font = font_with_gsub_of_full_length(&aliasing_gsub_blob(400, fixture_ellipsis_glyph()));
        let started = std::time::Instant::now();
        let outcome = remove_ellipsis_ligatures(&font);
        let elapsed = started.elapsed();
        assert_eq!(
            outcome.err(),
            Some(EllipsisPatchSkip::GsubWalkTooLong),
            "an aliasing GSUB must be refused, so the caller registers the font unchanged"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "the refusal must be bounded in time, took {elapsed:?}"
        );
    }

    /// L1: a `LigatureSet` several lookups reach is one rule, not one per path.
    #[test]
    fn a_ligature_set_reached_many_times_is_counted_once() {
        // 2 lookups x 2 subtables x 2 set offsets, all aliasing one set that
        // holds a single banned rule: eight paths, one rule.
        let font = font_with_gsub_of_full_length(&aliasing_gsub_blob(2, fixture_ellipsis_glyph()));
        let patch = remove_ellipsis_ligatures(&font).expect("the crafted rule must be removable");
        assert_eq!(
            patch.removed_rules, 1,
            "a shared LigatureSet must be counted once, not once per path reaching it"
        );
        assert_eq!(
            remove_ellipsis_ligatures(&patch.bytes).err(),
            Some(EllipsisPatchSkip::NoLigatureProducesEllipsis),
            "the single edit must actually have removed the rule"
        );
    }

    /// M1: an offset that leaves the GSUB table must be neither followed nor
    /// written to, even though it is still inside the file.
    #[test]
    fn the_patch_never_reaches_outside_the_declared_gsub() {
        let ellipsis = fixture_ellipsis_glyph();
        let (blob, second_set) = two_set_gsub_blob(ellipsis);

        let whole = font_with_gsub_of_full_length(&blob);
        assert_eq!(
            remove_ellipsis_ligatures(&whole)
                .expect("both crafted rules are removable")
                .removed_rules,
            2,
            "with the whole blob declared as GSUB both sets are legitimately reachable"
        );

        let clamped = font_with_gsub(&blob, second_set);
        let gsub_start = clamped.len() - blob.len();
        let patch = remove_ellipsis_ligatures(&clamped).expect("the first rule is removable");
        assert_eq!(
            patch.removed_rules, 1,
            "only the set inside the declared GSUB may be edited"
        );
        assert_eq!(
            &patch.bytes[..gsub_start],
            &clamped[..gsub_start],
            "no byte before the GSUB table may change"
        );
        let outside = gsub_start + second_set;
        assert_eq!(
            &patch.bytes[outside..],
            &clamped[outside..],
            "no byte after the GSUB table may change"
        );
    }

    /// `font_with_gsub` with the record declaring the blob's real length.
    fn font_with_gsub_of_full_length(blob: &[u8]) -> Vec<u8> {
        font_with_gsub(blob, blob.len())
    }

    /// Property sweep over the SHIPPED bundle: the patch must hold its invariants
    /// on every file the renderer's own base is built from, and — because none of
    /// them ships an ellipsis ligature — must leave every one of them untouched.
    #[test]
    fn the_shipped_bundle_survives_the_patch_unchanged() {
        use crate::font_base::test_bundle;
        use ms_fonts::Tier;

        let mut checked = 0usize;
        for tier in [Tier::Core, Tier::Bold, Tier::Ext] {
            for path in test_bundle::tier_paths(tier) {
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                let label = path.display().to_string();
                assert_patch_invariants(&label, &bytes);
                assert!(
                    remove_ellipsis_ligatures(&bytes).is_err(),
                    "{label}: no bundled font may carry an ellipsis ligature — if one now does, \
                     the 'never patch the bundle' decision in font_registry needs revisiting"
                );
                checked += 1;
            }
        }
        if checked == 0 {
            eprintln!(
                "skipping the_shipped_bundle_survives_the_patch_unchanged: fonts/ui is not \
                 present next to this checkout"
            );
        }
    }

    /// The same sweep over the project's `fonts/` directory (display fonts). Those
    /// files are not tracked by Git, so an empty directory is a skip, not a
    /// failure — unlike `fonts/ui`, some of them DO carry the rule.
    #[test]
    fn the_display_fonts_survive_the_patch() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fonts");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            eprintln!("skipping the_display_fonts_survive_the_patch: {} is missing", dir.display());
            return;
        };
        let mut checked = 0usize;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let is_font = path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| {
                matches!(ext.to_ascii_lowercase().as_str(), "ttf" | "otf" | "ttc" | "otc")
            });
            if !path.is_file() || !is_font {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            assert_patch_invariants(&path.display().to_string(), &bytes);
            checked += 1;
        }
        if checked == 0 {
            eprintln!("skipping the_display_fonts_survive_the_patch: no font files in fonts/");
        }
    }
}
