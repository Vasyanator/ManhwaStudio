/*
File: settings/typesetting/font_card_psd.rs

Purpose:
Reads a "font card" PSD: a document in which every text layer is SET in some font and whose
CONTENT is that font's human-readable title. It is the reverse of `tabs/typing/psd_export.rs`
(which WRITES a font's PostScript name into a text layer) and the bulk-fill source for the
virtual font groups edited in `font_groups.rs`: one card file yields `(PostScript name, title)`
pairs the group editor turns into members with per-group aliases.

Main responsibilities:
- read the file and parse it with `ag-psd`, decoding NO raster at all (every skip flag on);
- walk the layer tree depth-first in document order, descending into groups;
- pull each text layer's font NAME (the PostScript name, per the project's font-identity
  contract) and its normalized CONTENT, skipping what cannot become an entry;
- report failure as a typed [`FontCardError`] carrying a localized user message and a separate
  technical log line.

Key types:
- `FontCardEntry` — one parsed `(post_script_name, title)` pair.
- `FontCardError` — user message + technical log message.
- `SkipCounters` — aggregated skip reasons, logged ONCE (a per-layer log would spam a card
  that legitimately holds decorative layers).

Hostile-input budget (the file is CHOSEN BY THE USER, so it is untrusted, and the whole read
runs on a worker thread where an abort would take the process with it):
- `MAX_CARD_FILE_BYTES` bounds the allocation before the file is read at all;
- the layer walk is ITERATIVE with an explicit stack and refuses to descend past
  `MAX_GROUP_DEPTH`, because a recursive walk over a file-controlled nesting depth overflows
  the stack — an abort, not a catchable error;
- `MAX_CARD_ENTRIES` bounds the result vector.
Every refusal is COUNTED and logged; none of them is silent.

Key functions:
- `read_font_card()` — the module's entry point. BLOCKING (file I/O + PSD parse).
- `parse_font_card_bytes()` — the parse half, split out so tests can feed an in-memory PSD.
- `title_from_layer_text()` — layer text -> group-member title normalization.
- `font_name_of_text()` — style-run-aware font-name extraction.
- `oversized_card_error()` — the file-size refusal, pure in the size so it is testable.
- `is_invisible_format()` — the Cf (invisible format character) predicate the title filter uses.

Notes:
- GUI-INDEPENDENT on purpose: this module draws nothing and holds no widget state. It is called
  from a worker thread; calling it on the GUI thread would violate the no-blocking-I/O contract.
- Entries are returned in TRAVERSAL order and are NOT deduplicated — two layers naming one font
  stay two entries. Deduplication is a policy of the caller (the group editor decides whether a
  repeat is an overwrite, a conflict, or a second alias), not of the reader.
- A PSD records a font by NAME only. That name is exactly what the project uses as a font's
  IDENTITY (see README_AGENT: identity = PostScript name), so a card entry can be matched
  against the font list directly — but a name contested by two byte-different files cannot be
  resolved here any better than it can on export. Resolution is the caller's job.
*/

use ag_psd::psd::{Layer, LayerTextData, ReadOptions};
use ag_psd::read_psd;
use crate::runtime_log;
use std::path::Path;

/// Maximum length of a member title, in CHARACTERS (not bytes).
///
/// The group-member table renders titles in a fixed-width column; a title longer than this
/// could not be read there anyway, and the store should not carry a whole paragraph pasted
/// into a card layer.
const MAX_TITLE_CHARS: usize = 64;

/// Largest font card accepted, in BYTES, checked before the file is read into memory.
///
/// A font card is a page of text layers, not artwork: the real ones are a few megabytes, and
/// even a card carrying a full-page background stays far below this. The cap exists because the
/// path comes from a file picker — pointing it at a multi-gigabyte production PSD would
/// otherwise make the import worker allocate the whole file before discovering it is useless.
/// 256 MiB is generous enough that no plausible card is refused, and small enough that the
/// allocation cannot exhaust a normal machine.
const MAX_CARD_FILE_BYTES: u64 = 256 * BYTES_PER_MIB;

/// Divisor turning a byte count into the megabytes the "file too large" message reports.
///
/// The message is only ever produced for a file ABOVE [`MAX_CARD_FILE_BYTES`], so the integer
/// division truncating a fraction away cannot make the reported size misleading — it is always
/// at least the reported limit.
const BYTES_PER_MIB: u64 = 1024 * 1024;

/// Deepest group nesting the layer walk descends into; the document root is level 1.
///
/// Photoshop's own UI stops the user at 10 nested groups, so this is an order of magnitude more
/// than a hand-authored card can reach. It is not a usability limit but a safety one: the tree
/// is built by `ag-psd` from untrusted bytes, and something has to bound the walk.
const MAX_GROUP_DEPTH: usize = 64;

/// Most entries collected from one card.
///
/// A card enumerates typefaces a human picked; ten thousand of them is already far past any
/// installed font base, so hitting this cap means the file is not a font card. Bounds the result
/// vector (and everything the caller then does per entry) against a file built to be huge.
const MAX_CARD_ENTRIES: usize = 10_000;

/// Photoshop's invisible-glyph sentinel font, always index 0 of a document's `FontSet`.
///
/// It is not a real typeface: Photoshop assigns it to invisible characters, so a style run
/// naming it says nothing about how the layer is set. Treated as "no font".
const INVISIBLE_FONT_SENTINEL: &str = "AdobeInvisFont";

/// One entry of a font card: the font's PostScript name as recorded in the PSD, paired with
/// the human-readable title the layer's text spells out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FontCardEntry {
    /// PostScript name EXACTLY as written in the PSD (case preserved; only surrounding
    /// whitespace is stripped, which a spec-valid PostScript name cannot contain anyway).
    pub(super) post_script_name: String,
    /// The member title: the layer's text normalized by [`title_from_layer_text`], at most
    /// [`MAX_TITLE_CHARS`] characters and never empty.
    pub(super) title: String,
}

/// A font-card read failure: what to show the user and what to write to the log.
///
/// The two halves are deliberately separate — `user_message` is localized and actionable,
/// `log_message` carries the path and the underlying error and is never shown in the UI.
#[derive(Debug, Clone)]
pub(super) struct FontCardError {
    /// Localized, user-facing explanation.
    pub(super) user_message: String,
    /// Technical detail for `runtime_log` (path + underlying error). Not localized.
    pub(super) log_message: String,
}

/// How many layers were skipped, by reason. Aggregated so one card produces ONE log line.
#[derive(Debug, Default, Clone, Copy)]
struct SkipCounters {
    /// Hidden layers (and hidden groups, whose whole subtree is skipped with them).
    hidden: usize,
    /// Non-group layers carrying no text data at all.
    no_text: usize,
    /// Text layers whose content normalizes to an empty title.
    empty_title: usize,
    /// Text layers naming no usable font (absent, blank, or the invisible sentinel).
    no_font: usize,
    /// Groups NOT descended into because [`MAX_GROUP_DEPTH`] was already reached. Counts the
    /// group layers themselves, not the layers inside them (which are never visited).
    too_deep: usize,
    /// Candidate layers passed over because [`MAX_CARD_ENTRIES`] was already collected. They are
    /// not examined at all, so this counts LAYERS that could have become entries, not entries.
    over_capacity: usize,
}

impl SkipCounters {
    /// Total number of skipped layers, used to decide whether the skip line is worth logging.
    fn total(self) -> usize {
        self.hidden
            + self.no_text
            + self.empty_title
            + self.no_font
            + self.too_deep
            + self.over_capacity
    }

    /// Whether a safety cap fired, i.e. the card was truncated rather than merely filtered.
    ///
    /// Distinguished from ordinary skips because it means the RESULT IS INCOMPLETE, which is
    /// worth a warning rather than the informational per-card line.
    fn hit_a_cap(self) -> bool {
        self.too_deep > 0 || self.over_capacity > 0
    }

    /// The counters as one log fragment, so the three call sites cannot drift apart.
    fn log_fragment(self) -> String {
        format!(
            "hidden={}, no_text={}, empty_title={}, no_font={}, too_deep={}, over_capacity={}",
            self.hidden,
            self.no_text,
            self.empty_title,
            self.no_font,
            self.too_deep,
            self.over_capacity
        )
    }
}

/// Reads a font-card PSD and returns its entries in layer-traversal order.
///
/// `path` must point at a `.psd` file. BLOCKING: it reads the whole file and parses the
/// document, so it must be called OFF the GUI thread.
///
/// Raster data is never decoded (layer images, the composite, the thumbnail and linked files
/// are all skipped), so the cost is bounded by the file size and the layer count.
///
/// The file is SIZE-CHECKED before it is read: `path` comes from a file picker, and reading a
/// mistakenly chosen multi-gigabyte PSD would allocate all of it on the import worker. See
/// [`MAX_CARD_FILE_BYTES`].
///
/// # Errors
/// Returns [`FontCardError`] when the file cannot be stat'ed or read, when it is larger than
/// [`MAX_CARD_FILE_BYTES`], when the bytes are not a PSD this crate can parse (a damaged file,
/// or an unsupported color mode), or when the document contains no usable text layer at all.
pub(super) fn read_font_card(path: &Path) -> Result<Vec<FontCardEntry>, FontCardError> {
    // Stat before read: the point is to refuse the allocation, so the size must be known
    // without having performed it. A stat failure is the same user-facing problem as a read
    // failure (missing file, no permission), hence the shared message.
    let size = std::fs::metadata(path)
        .map_err(|err| FontCardError {
            user_message: t!("typing.font_settings.font_card_read_error").to_string(),
            log_message: format!("failed to stat font card '{}': {err}", path.display()),
        })?
        .len();
    if let Some(err) = oversized_card_error(path, size) {
        return Err(err);
    }

    let bytes = std::fs::read(path).map_err(|err| FontCardError {
        user_message: t!("typing.font_settings.font_card_read_error").to_string(),
        log_message: format!("failed to read font card '{}': {err}", path.display()),
    })?;
    parse_font_card_bytes(&bytes, path)
}

/// The refusal for a card larger than [`MAX_CARD_FILE_BYTES`], or `None` when `size` fits.
///
/// A pure function of the SIZE, not of the file: that is what lets the cap be tested without
/// producing a 256 MiB fixture. `path` only labels the messages.
fn oversized_card_error(path: &Path, size: u64) -> Option<FontCardError> {
    if size <= MAX_CARD_FILE_BYTES {
        return None;
    }
    Some(FontCardError {
        user_message: tf!(
            "typing.font_settings.font_card_too_large",
            size = size / BYTES_PER_MIB,
            limit = MAX_CARD_FILE_BYTES / BYTES_PER_MIB
        ),
        log_message: format!(
            "font card '{}' is {size} bytes, over the {MAX_CARD_FILE_BYTES}-byte cap",
            path.display()
        ),
    })
}

/// Parses font-card entries out of already-loaded PSD `bytes`.
///
/// Split from [`read_font_card`] so the parse contract can be exercised against an in-memory
/// document. `path` is used for LOG CONTEXT only — nothing is read from disk here.
///
/// # Errors
/// Returns [`FontCardError`] when `bytes` do not parse as a supported PSD, or when the parsed
/// document yields no entry.
///
/// The COLOR MODE is part of "supported", and the raster skip flags do not exempt it: `ag-psd`
/// rejects anything but Bitmap/Grayscale/Indexed/RGB while reading the FILE HEADER, before any
/// option is consulted (`ag-psd-0.1.0/src/reader.rs:678`, testing `is_supported_color_mode` at
/// `:566`). A CMYK, Lab, duotone or multichannel card therefore fails here even though we
/// decode none of its pixels — which is what the user message says, and what
/// `cmyk_color_mode_is_rejected_despite_raster_skips` pins down.
fn parse_font_card_bytes(bytes: &[u8], path: &Path) -> Result<Vec<FontCardEntry>, FontCardError> {
    // Every raster skip flag is on: a font card is read for its TEXT, and decoding page-sized
    // layer images would dominate the cost for data we discard.
    let options = ReadOptions {
        skip_layer_image_data: Some(true),
        skip_composite_image_data: Some(true),
        skip_thumbnail: Some(true),
        skip_linked_files_data: Some(true),
        ..Default::default()
    };
    let psd = read_psd(bytes, &options).map_err(|err| FontCardError {
        user_message: t!("typing.font_settings.font_card_parse_error").to_string(),
        log_message: format!(
            "failed to parse font card '{}' ({} bytes): {err}",
            path.display(),
            bytes.len()
        ),
    })?;

    let mut entries = Vec::new();
    let mut skipped = SkipCounters::default();
    if let Some(children) = psd.children.as_ref() {
        collect_card_entries(children, &mut entries, &mut skipped);
    }

    if entries.is_empty() {
        return Err(FontCardError {
            user_message: t!("typing.font_settings.font_card_empty_error").to_string(),
            log_message: format!(
                "font card '{}' yielded no entries; skipped layers: {}",
                path.display(),
                skipped.log_fragment()
            ),
        });
    }

    // A safety cap firing means the returned list is INCOMPLETE, which the counters alone would
    // understate among ordinary skips — so it gets its own warning naming the caps.
    if skipped.hit_a_cap() {
        runtime_log::log_warn(format!(
            "[font-card] '{}': truncated by a safety cap (depth>{MAX_GROUP_DEPTH} groups \
             skipped: {}, layers past the {MAX_CARD_ENTRIES}-entry cap: {}); the file is \
             unlikely to be a font card",
            path.display(),
            skipped.too_deep,
            skipped.over_capacity
        ));
    }

    // One aggregated line per card: a per-layer log would flood the runtime log for a card
    // that legitimately mixes text layers with decorative or hidden ones.
    if skipped.total() > 0 {
        runtime_log::log_info(format!(
            "[font-card] '{}': {} entries; skipped layers: {}",
            path.display(),
            entries.len(),
            skipped.log_fragment()
        ));
    }

    Ok(entries)
}

/// Walks `layers` in document order, appending every usable entry to `out` and counting what
/// was skipped into `counters`.
///
/// A layer with `children` is a GROUP: it is entered at its own position, so the result order is
/// the depth-first document order. HIDDEN layers are skipped, and a hidden group takes its whole
/// subtree with it — the user's decision: what is invisible in the card is not part of the card.
///
/// The walk is ITERATIVE and bounded. It must not recurse: `ag-psd` builds this tree with its
/// own explicit stack, so the nesting depth is limited by nothing but the file size, and a
/// recursive walk over a hostile card would overflow the worker's stack — a process abort that
/// no `Result` and no `catch_unwind` can intercept. Descent stops at [`MAX_GROUP_DEPTH`] and
/// collection stops at [`MAX_CARD_ENTRIES`]; both refusals are counted into `counters`.
fn collect_card_entries(layers: &[Layer], out: &mut Vec<FontCardEntry>, counters: &mut SkipCounters) {
    // Stack of "where we are inside each open level". Iterators (rather than index cursors)
    // keep the traversal order identical to the recursive form it replaces: a level is resumed
    // exactly where it was left when its group child was entered.
    let mut stack: Vec<std::slice::Iter<'_, Layer>> = vec![layers.iter()];

    while !stack.is_empty() {
        // The borrow of `stack` ends with this statement — the yielded reference borrows the
        // LAYER TREE, not the iterator — which is what lets the body push a deeper level.
        let Some(layer) = stack.last_mut().and_then(Iterator::next) else {
            stack.pop();
            continue;
        };

        if matches!(layer.hidden, Some(true)) {
            counters.hidden += 1;
            continue;
        }
        if let Some(children) = layer.children.as_ref() {
            // `stack.len()` IS the depth of the level this group lives in (the root is 1), so
            // entering it would create level `len() + 1`.
            if stack.len() >= MAX_GROUP_DEPTH {
                counters.too_deep += 1;
                continue;
            }
            stack.push(children.iter());
            continue;
        }
        // Past the cap the walk keeps running only to COUNT what it is dropping; examining the
        // layers would cost their title normalization for a result that cannot be returned.
        if out.len() >= MAX_CARD_ENTRIES {
            counters.over_capacity += 1;
            continue;
        }
        let Some(text_data) = layer.additional_info.text.as_ref() else {
            counters.no_text += 1;
            continue;
        };
        let title = title_from_layer_text(&text_data.text);
        if title.is_empty() {
            counters.empty_title += 1;
            continue;
        }
        let Some(post_script_name) = font_name_of_text(text_data) else {
            counters.no_font += 1;
            continue;
        };
        out.push(FontCardEntry {
            post_script_name: post_script_name.to_string(),
            title,
        });
    }
}

/// Picks the PostScript name a text layer is set in, or `None` when it names no usable font.
///
/// STYLE RUNS WIN over the layer's base style: Photoshop stores the real per-character font in
/// the runs and commonly leaves a placeholder in the base style. The first run naming a USABLE
/// font decides — not merely the first run that has a font at all, because run 0 of a real
/// Photoshop text layer is frequently the invisible sentinel (see [`INVISIBLE_FONT_SENTINEL`]),
/// and honoring it would discard a perfectly good card layer.
///
/// The returned slice is trimmed; a spec-valid PostScript name contains no whitespace, so
/// trimming cannot change the identity while it does make a sloppily authored card usable.
fn font_name_of_text(text: &LayerTextData) -> Option<&str> {
    let from_runs = text.style_runs.as_ref().and_then(|runs| {
        runs.iter()
            .filter_map(|run| run.style.font.as_ref())
            .map(|font| font.name.trim())
            .find(|name| is_usable_font_name(name))
    });
    from_runs.or_else(|| {
        text.style
            .as_ref()
            .and_then(|style| style.font.as_ref())
            .map(|font| font.name.trim())
            .filter(|name| is_usable_font_name(name))
    })
}

/// Whether `name` (already trimmed) can stand for a real typeface.
///
/// Rejects the empty name and Photoshop's invisible-glyph sentinel. `MyriadPro-Regular` is
/// deliberately NOT rejected: it is a real font that happens to be our export's last-resort
/// fallback, and a card legitimately built with it must still import.
fn is_usable_font_name(name: &str) -> bool {
    !name.is_empty() && !name.eq_ignore_ascii_case(INVISIBLE_FONT_SENTINEL)
}

/// Whether `ch` is an invisible Unicode FORMAT character (general category Cf).
///
/// These are the characters that change how the text around them is laid out without occupying
/// space themselves: the bidi overrides and isolates, the zero-width space/joiners, the
/// word joiner, the byte-order mark, the tag characters. A title is a short label drawn in a
/// table cell, so none of them can do anything there except misrepresent it — U+202E makes the
/// rest of a row read backwards, U+200B makes a word look like two, and a title made of nothing
/// but these would render blank while comparing unequal to every other title.
///
/// Spelled out as ranges rather than taken from a Unicode-property crate on purpose: this file's
/// only interest in the character database is this one category, and none of the crates already
/// in the dependency graph exposes it (`unicode-properties` and friends are transitive
/// dependencies of the shaper, not ours to call). The list is the Cf block set of Unicode 16.
/// Combining marks are deliberately NOT here: variation selectors (U+FE00..U+FE0F, category Mn)
/// are what make an emoji render as an emoji, and dropping them would corrupt visible text.
fn is_invisible_format(ch: char) -> bool {
    matches!(
        ch,
        '\u{00AD}'                              // SOFT HYPHEN
        | '\u{0600}'..='\u{0605}'               // Arabic subtending marks
        | '\u{061C}'                            // ARABIC LETTER MARK
        | '\u{06DD}' | '\u{070F}'               // Arabic/Syriac number & abbreviation marks
        | '\u{0890}'..='\u{0891}' | '\u{08E2}'  // Arabic pound/piastre & disputed-end-of-ayah
        | '\u{180E}'                            // MONGOLIAN VOWEL SEPARATOR
        | '\u{200B}'..='\u{200F}'               // ZWSP, ZWNJ, ZWJ, LRM, RLM
        | '\u{202A}'..='\u{202E}'               // bidi embeddings and OVERRIDES
        | '\u{2060}'..='\u{2064}'               // word joiner, invisible operators
        | '\u{2066}'..='\u{206F}'               // bidi isolates + deprecated format controls
        | '\u{FEFF}'                            // ZERO WIDTH NO-BREAK SPACE (BOM)
        | '\u{FFF9}'..='\u{FFFB}'               // interlinear annotation
        | '\u{110BD}' | '\u{110CD}'             // Kaithi number signs
        | '\u{13430}'..='\u{1343F}'             // Egyptian hieroglyph format controls
        | '\u{1BCA0}'..='\u{1BCA3}'             // shorthand format controls
        | '\u{1D173}'..='\u{1D17A}'             // musical format controls
        | '\u{E0001}' | '\u{E0020}'..='\u{E007F}' // language tag + tag characters
    )
}

/// Normalizes a text layer's content into a group-member title.
///
/// Control characters (line breaks included) become spaces, invisible FORMAT characters are
/// dropped, runs of whitespace collapse to a single space, the result is trimmed and then cut to
/// [`MAX_TITLE_CHARS`] CHARACTERS — never bytes, so a Cyrillic title is cut at the same visible
/// length as a Latin one. The cut can leave a trailing space, which is trimmed away again.
///
/// A control becomes a SPACE (it separated words) while a format character is REMOVED (it did
/// not — U+200B sits inside a word that was drawn as one). Everything the card actually shows
/// survives, emoji and non-Latin scripts included; the one visible consequence is that an emoji
/// ZWJ sequence falls apart into the emoji it is built from, which is a fair price for not
/// letting a whole class of invisible controls into a stored label. See [`is_invisible_format`].
///
/// Returns an empty string when the layer carries nothing but whitespace and invisibles; the
/// caller treats that as "no title" and skips the layer.
pub(super) fn title_from_layer_text(text: &str) -> String {
    // Map controls to spaces FIRST so `\n`/`\r`/`\t` act as word separators rather than being
    // dropped and gluing two words together; format characters go the other way and vanish.
    let flattened: String = text
        .chars()
        .filter(|ch| !is_invisible_format(*ch))
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();

    // `split_whitespace` handles the trim and the collapse in one pass, and covers Unicode
    // spaces (NBSP among them) that a naive `== ' '` test would keep.
    let mut collapsed = String::with_capacity(flattened.len());
    for word in flattened.split_whitespace() {
        if !collapsed.is_empty() {
            collapsed.push(' ');
        }
        collapsed.push_str(word);
    }

    let truncated: String = collapsed.chars().take(MAX_TITLE_CHARS).collect();
    truncated.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ag_psd::psd::{
        BlendMode, ColorMode, Font, LayerAdditionalInfo, PixelData, Psd, TextStyle, TextStyleRun,
        WriteOptions,
    };
    use ag_psd::write_psd;

    /// Builds a text layer whose content is `text`, set in `base_font` (when `Some`) and
    /// carrying `run_fonts` as style runs. `hidden` mirrors the PSD flag.
    ///
    /// Every layer gets a 1x1 opaque pixel: the writer serializes layer image data, and an
    /// image-less layer is not what a real card looks like.
    fn text_layer(
        name: &str,
        text: Option<&str>,
        base_font: Option<&str>,
        run_fonts: &[Option<&str>],
        hidden: bool,
    ) -> Layer {
        let text_data = text.map(|body| LayerTextData {
            text: body.to_string(),
            style: base_font.map(|font| TextStyle {
                font: Some(Font {
                    name: font.to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            style_runs: (!run_fonts.is_empty()).then(|| {
                run_fonts
                    .iter()
                    .map(|font| TextStyleRun {
                        length: 1.0,
                        style: TextStyle {
                            font: font.map(|name| Font {
                                name: name.to_string(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    })
                    .collect()
            }),
            ..Default::default()
        });
        Layer {
            additional_info: LayerAdditionalInfo {
                name: Some(name.to_string()),
                text: text_data,
                ..Default::default()
            },
            top: Some(0.0),
            left: Some(0.0),
            bottom: Some(1.0),
            right: Some(1.0),
            blend_mode: Some(BlendMode::Normal),
            opacity: Some(1.0),
            hidden: Some(hidden),
            image_data: Some(PixelData {
                width: 1,
                height: 1,
                data: vec![0, 0, 0, 255],
            }),
            ..Default::default()
        }
    }

    /// Wraps `children` into a visible group layer.
    fn group_layer(name: &str, children: Vec<Layer>) -> Layer {
        Layer {
            additional_info: LayerAdditionalInfo {
                name: Some(name.to_string()),
                ..Default::default()
            },
            blend_mode: Some(BlendMode::PassThrough),
            opacity: Some(1.0),
            hidden: Some(false),
            children: Some(children),
            opened: Some(true),
            ..Default::default()
        }
    }

    /// Serializes a one-page RGB document holding `children` into PSD bytes.
    ///
    /// `invalidate_text_layers: Some(false)` is required: the default asks Photoshop to redraw
    /// text layers, which drops the very data this module reads back.
    fn card_psd_bytes(children: Vec<Layer>) -> Vec<u8> {
        let psd = Psd {
            width: 4.0,
            height: 4.0,
            color_mode: Some(ColorMode::Rgb),
            channels: Some(4.0),
            bits_per_channel: Some(8.0),
            children: Some(children),
            image_data: Some(PixelData {
                width: 4,
                height: 4,
                data: vec![255; 4 * 4 * 4],
            }),
            ..Default::default()
        };
        write_psd(
            &psd,
            &WriteOptions {
                invalidate_text_layers: Some(false),
                ..Default::default()
            },
        )
    }

    #[test]
    fn title_collapses_line_breaks_and_spaces() {
        assert_eq!(
            title_from_layer_text("Comic\nSans   MS"),
            "Comic Sans MS",
            "line breaks and runs of spaces collapse to single spaces"
        );
        assert_eq!(
            title_from_layer_text("  \t Плакатный \r\n шрифт \n "),
            "Плакатный шрифт",
            "leading/trailing whitespace and control characters are stripped"
        );
    }

    #[test]
    fn title_of_blank_text_is_empty() {
        assert_eq!(title_from_layer_text(""), "");
        assert_eq!(title_from_layer_text("   \n\t\r\n  "), "");
    }

    #[test]
    fn title_truncates_by_characters_not_bytes() {
        // 64 Cyrillic characters = 128 bytes: a byte-wise cut would mangle this.
        let exactly_64 = "я".repeat(MAX_TITLE_CHARS);
        let title = title_from_layer_text(&exactly_64);
        assert_eq!(title, exactly_64, "a title of exactly the cap survives whole");
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);

        let too_long = "я".repeat(MAX_TITLE_CHARS + 6);
        let title = title_from_layer_text(&too_long);
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS, "cut is by characters");
        assert_eq!(title, exactly_64);

        // A cut landing on a space must not leave the title ending in one.
        let with_space_at_cut = format!("{} хвост", "a".repeat(MAX_TITLE_CHARS - 1));
        let title = title_from_layer_text(&with_space_at_cut);
        assert_eq!(title, "a".repeat(MAX_TITLE_CHARS - 1));
    }

    #[test]
    fn title_drops_invisible_format_characters() {
        assert_eq!(
            title_from_layer_text("Comic\u{200b}Sans"),
            "ComicSans",
            "a zero-width space is removed, not turned into a word break"
        );
        assert_eq!(
            title_from_layer_text("\u{202e}Плакатный\u{202c} шрифт"),
            "Плакатный шрифт",
            "a bidi override cannot reach the title and reverse the row"
        );
        assert_eq!(
            title_from_layer_text("\u{2066}A\u{2069}\u{feff}B"),
            "AB",
            "isolates and the BOM are removed too"
        );
        assert_eq!(
            title_from_layer_text("\u{200b}\u{202e}\u{2069}"),
            "",
            "a layer holding only invisibles has no title, so the caller skips it"
        );

        // Visible text survives whole: Cyrillic, guillemets, and an emoji whose variation
        // selector (U+FE0F, category Mn) must NOT be mistaken for a format character.
        let visible = "Шрифт ❤\u{fe0f} «Комикс»";
        assert_eq!(title_from_layer_text(visible), visible);

        // Invisibles are dropped BEFORE the cut, so they cannot consume the visible budget.
        let padded = "я\u{200b}".repeat(MAX_TITLE_CHARS);
        assert_eq!(title_from_layer_text(&padded), "я".repeat(MAX_TITLE_CHARS));
    }

    #[test]
    fn oversized_card_is_refused_by_size_alone() {
        assert!(
            oversized_card_error(Path::new("card.psd"), MAX_CARD_FILE_BYTES).is_none(),
            "a file exactly at the cap is still read"
        );
        let err = oversized_card_error(Path::new("card.psd"), MAX_CARD_FILE_BYTES + 1)
            .expect("a file past the cap is refused");
        assert!(!err.user_message.is_empty());
        assert!(
            err.log_message.contains("card.psd") && err.log_message.contains("cap"),
            "the log line must name the file and the cap: {}",
            err.log_message
        );
    }

    #[test]
    fn deep_nesting_is_walked_without_recursion_and_stops_at_the_depth_cap() {
        // 300 levels: far past `MAX_GROUP_DEPTH`, yet shallow enough that BUILDING and DROPPING
        // this tree (both of which recurse over the nested `Vec<Layer>`) stay inside a test
        // thread's stack. The walk under test is the part that must survive any depth.
        const DEPTH: usize = 300;

        let mut node = group_layer(
            "deepest",
            vec![text_layer(
                "beyond",
                Some("За пределом"),
                Some("Beyond-Regular"),
                &[],
                false,
            )],
        );
        for level in (0..DEPTH).rev() {
            let title = format!("Уровень {level}");
            let font = format!("L{level}-Regular");
            // Each level holds its own text layer FIRST and the deeper group second, so the
            // entries must come back in ascending level order.
            node = group_layer(
                &format!("g{level}"),
                vec![
                    text_layer(&format!("t{level}"), Some(&title), Some(&font), &[], false),
                    node,
                ],
            );
        }

        let mut entries = Vec::new();
        let mut counters = SkipCounters::default();
        collect_card_entries(std::slice::from_ref(&node), &mut entries, &mut counters);

        // `g0` sits at level 1 and every level contributes one text layer before its nested
        // group, so the levels actually entered are 1..=MAX_GROUP_DEPTH and the last group
        // encountered at level MAX_GROUP_DEPTH is refused.
        assert_eq!(entries.len(), MAX_GROUP_DEPTH - 1);
        assert_eq!(counters.too_deep, 1, "exactly one group was refused entry");
        assert!(counters.hit_a_cap());
        assert_eq!(entries[0].title, "Уровень 0", "order is still document order");
        assert_eq!(
            entries[entries.len() - 1].title,
            format!("Уровень {}", MAX_GROUP_DEPTH - 2)
        );
        assert!(
            entries
                .iter()
                .all(|entry| entry.post_script_name != "Beyond-Regular"),
            "nothing below the cap is collected"
        );
    }

    #[test]
    fn entry_count_is_capped_and_the_overflow_is_counted() {
        const EXTRA: usize = 5;
        let layers: Vec<Layer> = (0..MAX_CARD_ENTRIES + EXTRA)
            .map(|index| {
                text_layer(
                    "layer",
                    Some(&format!("Шрифт {index}")),
                    Some(&format!("F{index}-Regular")),
                    &[],
                    false,
                )
            })
            .collect();

        let mut entries = Vec::new();
        let mut counters = SkipCounters::default();
        collect_card_entries(&layers, &mut entries, &mut counters);

        assert_eq!(entries.len(), MAX_CARD_ENTRIES);
        assert_eq!(
            counters.over_capacity, EXTRA,
            "the layers past the cap are counted, not silently dropped"
        );
        assert!(counters.hit_a_cap());
        assert_eq!(entries[0].title, "Шрифт 0", "the kept entries are the first ones");
    }

    #[test]
    fn cmyk_color_mode_is_rejected_despite_raster_skips() {
        let mut bytes = card_psd_bytes(vec![text_layer(
            "a",
            Some("Имя"),
            Some("A-Regular"),
            &[],
            false,
        )]);
        assert!(
            parse_font_card_bytes(&bytes, Path::new("<memory>")).is_ok(),
            "the RGB baseline parses, so the rejection below is about the color mode alone"
        );

        // PSD file header: "8BPS"(4) + version(2) + reserved(6) + channels(2) + height(4) +
        // width(4) + depth(2), then the color mode as a big-endian u16 at [24..26]; 4 = CMYK.
        // Patching the header (rather than writing a CMYK document) keeps the test about the
        // READER: everything after the header is byte-identical to a document that parses.
        const COLOR_MODE_OFFSET: usize = 24;
        bytes[COLOR_MODE_OFFSET] = 0;
        bytes[COLOR_MODE_OFFSET + 1] = 4;

        let err = parse_font_card_bytes(&bytes, Path::new("<memory>"))
            .expect_err("a CMYK document must be refused");
        // ag-psd checks the color mode while reading the HEADER, before any ReadOptions flag is
        // consulted, so our raster skips do not let a CMYK card through
        // (ag-psd-0.1.0/src/reader.rs:678, `is_supported_color_mode` at :566). This is what the
        // localized parse-error message promises.
        assert!(
            err.log_message.contains("Color mode not supported"),
            "unexpected failure reason: {}",
            err.log_message
        );
    }

    /// Builds bare text data (no round trip) so the font-name preference can be checked on
    /// exactly the shape given, without the writer's run-filling in the way.
    fn text_data(base_font: Option<&str>, run_fonts: &[Option<&str>]) -> LayerTextData {
        let Layer {
            additional_info: LayerAdditionalInfo { text, .. },
            ..
        } = text_layer("probe", Some("x"), base_font, run_fonts, false);
        text.expect("text data")
    }

    #[test]
    fn font_name_prefers_the_first_usable_style_run() {
        // No font anywhere.
        assert_eq!(font_name_of_text(&text_data(None, &[])), None);
        // Base style only.
        assert_eq!(
            font_name_of_text(&text_data(Some("Base-Regular"), &[])),
            Some("Base-Regular")
        );
        // A base style naming only the sentinel is as good as no font.
        assert_eq!(
            font_name_of_text(&text_data(Some(INVISIBLE_FONT_SENTINEL), &[])),
            None
        );
        assert_eq!(font_name_of_text(&text_data(Some("   "), &[])), None);
        // Runs win over the base style, and unusable run entries are stepped over.
        assert_eq!(
            font_name_of_text(&text_data(
                Some("Base-Regular"),
                &[None, Some(INVISIBLE_FONT_SENTINEL), Some("Run-Bold")]
            )),
            Some("Run-Bold")
        );
        // Runs that name nothing usable fall back to the base style.
        assert_eq!(
            font_name_of_text(&text_data(
                Some("Base-Regular"),
                &[Some(INVISIBLE_FONT_SENTINEL), None]
            )),
            Some("Base-Regular")
        );
        // Surrounding whitespace is stripped; the case is preserved verbatim.
        assert_eq!(
            font_name_of_text(&text_data(Some(" ComicSansMS \n"), &[])),
            Some("ComicSansMS")
        );
    }

    #[test]
    fn reads_entries_in_traversal_order_skipping_unusable_layers() {
        // Order inside the group and at the top level is the expected result order.
        let bytes = card_psd_bytes(vec![
            group_layer(
                "Группа",
                vec![
                    text_layer("visible", Some("Комик\nСанс"), Some("ComicSansMS"), &[], false),
                    text_layer("hidden", Some("Скрытый"), Some("Hidden-Font"), &[], true),
                    // Run 0 is the invisible sentinel, exactly as Photoshop writes it; the
                    // first USABLE run decides, and it beats the base style. (The writer
                    // appends a filler run in the base font for the uncovered tail, so
                    // "runs win" is genuinely exercised here.)
                    text_layer(
                        "runs",
                        Some("Из ранов"),
                        Some("Base-Regular"),
                        &[Some(INVISIBLE_FONT_SENTINEL), Some("Runs-Bold")],
                        false,
                    ),
                ],
            ),
            text_layer("no text", None, None, &[], false),
            text_layer("blank title", Some("   \n  "), Some("Blank-Font"), &[], false),
            text_layer("no font", Some("Без шрифта"), None, &[], false),
            text_layer("last", Some("Последний"), Some("Last-Regular"), &[], false),
        ]);

        let entries =
            parse_font_card_bytes(&bytes, Path::new("<memory>")).expect("card parses");
        assert_eq!(
            entries,
            vec![
                FontCardEntry {
                    post_script_name: "ComicSansMS".to_string(),
                    title: "Комик Санс".to_string(),
                },
                FontCardEntry {
                    post_script_name: "Runs-Bold".to_string(),
                    title: "Из ранов".to_string(),
                },
                FontCardEntry {
                    post_script_name: "Last-Regular".to_string(),
                    title: "Последний".to_string(),
                },
            ]
        );
    }

    #[test]
    fn hidden_group_skips_its_whole_subtree() {
        let mut hidden_group = group_layer(
            "Скрытая группа",
            vec![text_layer("inside", Some("Внутри"), Some("Inside-Font"), &[], false)],
        );
        hidden_group.hidden = Some(true);
        let bytes = card_psd_bytes(vec![
            hidden_group,
            text_layer("visible", Some("Снаружи"), Some("Outside-Font"), &[], false),
        ]);

        let entries =
            parse_font_card_bytes(&bytes, Path::new("<memory>")).expect("card parses");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].post_script_name, "Outside-Font");
    }

    #[test]
    fn duplicates_are_kept_for_the_caller_to_resolve() {
        let bytes = card_psd_bytes(vec![
            text_layer("a", Some("Первое имя"), Some("Same-Regular"), &[], false),
            text_layer("b", Some("Второе имя"), Some("Same-Regular"), &[], false),
        ]);

        let entries =
            parse_font_card_bytes(&bytes, Path::new("<memory>")).expect("card parses");
        assert_eq!(entries.len(), 2, "the reader never deduplicates");
        assert_eq!(entries[0].title, "Первое имя");
        assert_eq!(entries[1].title, "Второе имя");
    }

    #[test]
    fn card_without_usable_text_layers_is_an_error() {
        let bytes = card_psd_bytes(vec![text_layer("no text", None, None, &[], false)]);
        let err = parse_font_card_bytes(&bytes, Path::new("<memory>"))
            .expect_err("a card with no text layer must fail");
        assert!(!err.user_message.is_empty());
        assert!(
            err.log_message.contains("no_text=1"),
            "the log line reports why layers were skipped: {}",
            err.log_message
        );
    }

    #[test]
    fn non_psd_bytes_produce_an_error_not_a_panic() {
        let err = parse_font_card_bytes(b"this is definitely not a PSD file", Path::new("x.psd"))
            .expect_err("garbage must not parse");
        assert!(!err.user_message.is_empty());
        assert!(!err.log_message.is_empty());

        // An empty buffer is the degenerate case of the same path.
        assert!(parse_font_card_bytes(&[], Path::new("x.psd")).is_err());
    }

    #[test]
    fn missing_file_reports_a_read_error() {
        let err = read_font_card(Path::new("no-such-font-card-file.psd"))
            .expect_err("a missing file must fail");
        // The size check stats the file first, so a missing file is reported by THAT step; both
        // steps name the file and carry the same localized message, which is the contract here.
        assert!(
            err.log_message.contains("font card 'no-such-font-card-file.psd'"),
            "unexpected log message: {}",
            err.log_message
        );
        assert!(!err.user_message.is_empty());
    }
}
