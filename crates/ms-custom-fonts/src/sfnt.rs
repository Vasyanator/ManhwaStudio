/*
File: crates/ms-custom-fonts/src/sfnt.rs

Purpose:
Hand-written writer of a static TrueType (`glyf`) font. Shared by every custom font kind: a
compiler fills a `FontBuild` and gets the font file bytes back.

Key structures:
- `FontBuild`: glyphs (quantized quadratic contours + advance), cmap, kerning, metrics, names.
- `GlyphData`, `FontMetrics`, `FontNames`.

Key functions:
- `write_font`: assembles `OS/2 cmap glyf head hhea hmtx kern loca maxp name post` with a
  sorted table directory, 4-byte padding and checksums.
- `replace_kern`: re-assembles a face this writer produced with a different `kern` table
  (every other table copied byte for byte; checksums and `head.checkSumAdjustment` recomputed).

Notes:
- Output is a pure function of `FontBuild` (no timestamps: `head.created/modified` are 0), so
  equal inputs give byte-identical files.
- `cmap`: format 4 (BMP) and format 12 (full), each referenced by the Unicode (0,3)/(0,4) and
  Windows (3,1)/(3,10) records. If format 4 would exceed its 64 KiB length limit, the
  codepoint runs that do not fit are left to format 12 (readers consult both).
- `kern`: OpenType version 0, format 0, split into subtables of at most `KERN_PAIRS_PER_SUBTABLE`
  pairs (the subtable length is 16-bit); pairs are disjoint between subtables, so the additive
  application gives the same result as one table.
- Dropped in v1 (never written): GSUB, GPOS, hinting (`fpgm`/`prep`/`cvt`, instructions),
  colour and bitmap tables, variations, vertical metrics.
*/

use crate::error::CompileError;
use crate::outline::{TARGET_UPM, TtContour};

/// `glyf` simple-glyph point flags.
const ON_CURVE: u8 = 0x01;
const X_SHORT: u8 = 0x02;
const Y_SHORT: u8 = 0x04;
const REPEAT: u8 = 0x08;
const X_SAME_OR_POSITIVE: u8 = 0x10;
const Y_SAME_OR_POSITIVE: u8 = 0x20;

/// Pairs per `kern` format-0 subtable: (65535 - 14 header bytes) / 6 bytes per pair = 10920.
const KERN_PAIRS_PER_SUBTABLE: usize = 10_000;

/// One output glyph.
#[derive(Debug, Clone, Default)]
pub(crate) struct GlyphData {
    /// Horizontal advance in font units.
    pub advance: u16,
    /// Quantized quadratic contours (empty = blank glyph).
    pub contours: Vec<TtContour>,
}

/// Font-wide metrics and style facts in font units.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct FontMetrics {
    pub ascender: i16,
    pub descender: i16,
    pub line_gap: i16,
    pub x_height: i16,
    pub cap_height: i16,
    pub underline_position: i16,
    pub underline_thickness: i16,
    /// OS/2 `usWeightClass` (100..=900).
    pub weight_class: u16,
    pub italic: bool,
    /// Degrees counter-clockwise from vertical (`post.italicAngle`).
    pub italic_angle: f64,
}

/// `name` table strings (ASCII-safe names are also written for the Mac platform).
#[derive(Debug, Clone, Default)]
pub(crate) struct FontNames {
    pub family: String,
    pub subfamily: String,
    pub unique_id: String,
    pub full_name: String,
    pub version: String,
    pub postscript: String,
}

/// Everything the writer needs. Glyph 0 must be `.notdef`.
#[derive(Debug, Clone, Default)]
pub(crate) struct FontBuild {
    pub glyphs: Vec<GlyphData>,
    /// `(character, glyph id)` sorted by character, unique characters.
    pub cmap: Vec<(char, u16)>,
    /// `(left glyph, right glyph, value)` sorted by `(left, right)`, unique, non-zero.
    pub kern_pairs: Vec<(u16, u16, i16)>,
    pub metrics: FontMetrics,
    pub names: FontNames,
}

/// Big-endian byte sink.
#[derive(Default)]
struct Buf(Vec<u8>);

impl Buf {
    fn u8(&mut self, value: u8) {
        self.0.push(value);
    }
    fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn i16(&mut self, value: i16) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn i32(&mut self, value: i32) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    fn len(&self) -> usize {
        self.0.len()
    }
}

/// Converts a size or count to `u16`, naming the table on overflow.
fn to_u16(value: usize, table: &'static str) -> Result<u16, CompileError> {
    u16::try_from(value).map_err(|_| CompileError::OutputTooLarge { table })
}

/// Converts a size or offset to `u32`, naming the table on overflow.
fn to_u32(value: usize, table: &'static str) -> Result<u32, CompileError> {
    u32::try_from(value).map_err(|_| CompileError::OutputTooLarge { table })
}

/// Per-glyph bounding box and point facts gathered while encoding `glyf`.
#[derive(Debug, Clone, Copy, Default)]
struct GlyphBox {
    empty: bool,
    x_min: i16,
    y_min: i16,
    x_max: i16,
    y_max: i16,
    points: usize,
    contours: usize,
}

/// Writes the complete font file.
///
/// # Errors
/// `OutputTooLarge` when a count or offset exceeds the format, `TooManyGlyphs` above 65 535
/// glyphs.
pub(crate) fn write_font(build: &FontBuild) -> Result<Vec<u8>, CompileError> {
    let glyph_count = u16::try_from(build.glyphs.len())
        .map_err(|_| CompileError::TooManyGlyphs { count: build.glyphs.len() })?;

    let (glyf, loca, boxes) = encode_glyf(&build.glyphs)?;
    let font_box = boxes.iter().filter(|glyph_box| !glyph_box.empty).fold(None, |acc: Option<GlyphBox>, glyph_box| {
        Some(match acc {
            None => *glyph_box,
            Some(acc) => GlyphBox {
                x_min: acc.x_min.min(glyph_box.x_min),
                y_min: acc.y_min.min(glyph_box.y_min),
                x_max: acc.x_max.max(glyph_box.x_max),
                y_max: acc.y_max.max(glyph_box.y_max),
                ..acc
            },
        })
    });
    let font_box = font_box.unwrap_or_default();

    let mut tables: Vec<([u8; 4], Vec<u8>)> = vec![
        (*b"OS/2", encode_os2(build, &font_box)),
        (*b"cmap", encode_cmap(&build.cmap)?),
        (*b"glyf", glyf),
        (*b"head", encode_head(&font_box, build.metrics)),
        (*b"hhea", encode_hhea(build, &boxes, glyph_count)),
        (*b"hmtx", encode_hmtx(build, &boxes)),
        (*b"loca", loca),
        (*b"maxp", encode_maxp(&boxes, glyph_count)?),
        (*b"name", encode_name(&build.names)?),
        (*b"post", encode_post(build.metrics)),
    ];
    if !build.kern_pairs.is_empty() {
        tables.push((*b"kern", encode_kern(&build.kern_pairs)?));
    }
    tables.sort_by_key(|(tag, _)| *tag);
    assemble(&tables)
}

/// One font table: its tag and its bytes (unpadded).
pub(crate) type Table = ([u8; 4], Vec<u8>);

/// Replaces the `kern` table of `face` (a file `write_font` produced) with `pairs` (sorted by
/// `(left, right)`, unique, non-zero; empty = no `kern` table) and re-assembles the file: every
/// other table is copied byte for byte, the directory, checksums and
/// `head.checkSumAdjustment` are recomputed exactly as `write_font` computes them.
///
/// # Errors
/// `UnsupportedFace` when the bytes are not a TrueType file with a readable table directory
/// and a `head` table; `OutputTooLarge` as in `write_font`.
pub(crate) fn replace_kern(face: &[u8], pairs: &[(u16, u16, i16)]) -> Result<Vec<u8>, CompileError> {
    let mut tables: Vec<Table> = read_tables(face)?.into_iter().filter(|(tag, _)| tag != b"kern").collect();
    let head = tables
        .iter_mut()
        .find(|(tag, _)| tag == b"head")
        .ok_or_else(|| CompileError::UnsupportedFace { reason: "no 'head' table".to_owned() })?;
    // `assemble` computes the whole-file checksum with `checkSumAdjustment` still zero, and the
    // `head` table checksum is defined over that zeroed field too.
    head.1
        .get_mut(8..12)
        .ok_or_else(|| CompileError::UnsupportedFace { reason: "the 'head' table is truncated".to_owned() })?
        .fill(0);
    if !pairs.is_empty() {
        tables.push((*b"kern", encode_kern(pairs)?));
    }
    tables.sort_by_key(|(tag, _)| *tag);
    assemble(&tables)
}

/// The tables of a TrueType file, in directory order, each exactly `length` bytes (padding
/// excluded).
///
/// # Errors
/// `UnsupportedFace` for a non-TrueType version tag, a truncated directory or a table record
/// that points outside the file.
pub(crate) fn read_tables(face: &[u8]) -> Result<Vec<Table>, CompileError> {
    let unsupported = |reason: &str| CompileError::UnsupportedFace { reason: reason.to_owned() };
    let read_u32 = |at: usize| -> Option<u32> {
        let bytes: [u8; 4] = face.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(u32::from_be_bytes(bytes))
    };
    if read_u32(0) != Some(0x0001_0000) {
        return Err(unsupported("not a TrueType ('glyf') font file"));
    }
    let num_tables = face.get(4..6).and_then(|bytes| <[u8; 2]>::try_from(bytes).ok()).map(u16::from_be_bytes).ok_or_else(|| unsupported("truncated header"))?;
    let mut tables = Vec::with_capacity(usize::from(num_tables));
    for index in 0..usize::from(num_tables) {
        let record = 12 + 16 * index;
        let tag: [u8; 4] = face
            .get(record..record + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| unsupported("truncated table directory"))?;
        let (Some(offset), Some(length)) = (read_u32(record + 8), read_u32(record + 12)) else {
            return Err(unsupported("truncated table directory"));
        };
        let start = usize::try_from(offset).map_err(|_| unsupported("table offset out of range"))?;
        let length = usize::try_from(length).map_err(|_| unsupported("table length out of range"))?;
        let data = start
            .checked_add(length)
            .and_then(|end| face.get(start..end))
            .ok_or_else(|| unsupported("a table lies outside the file"))?;
        tables.push((tag, data.to_vec()));
    }
    Ok(tables)
}

/// Builds the table directory, pads tables, fills checksums and `head.checkSumAdjustment`.
fn assemble(tables: &[([u8; 4], Vec<u8>)]) -> Result<Vec<u8>, CompileError> {
    let num_tables = to_u16(tables.len(), "directory")?;
    let (search_range, entry_selector, range_shift) = binary_search_header(num_tables, 16);
    let mut out = Buf::default();
    out.u32(0x0001_0000);
    out.u16(num_tables);
    out.u16(search_range);
    out.u16(entry_selector);
    out.u16(range_shift);

    let mut offset = 12 + 16 * tables.len();
    let mut head_offset = None;
    for (tag, data) in tables {
        out.bytes(tag);
        out.u32(checksum(data));
        out.u32(to_u32(offset, "directory")?);
        out.u32(to_u32(data.len(), "directory")?);
        if tag == b"head" {
            head_offset = Some(offset);
        }
        offset += padded_len(data.len());
    }
    for (_, data) in tables {
        out.bytes(data);
        out.0.resize(padded_len(out.len()), 0);
    }
    if let Some(head_offset) = head_offset {
        // `checkSumAdjustment` is computed over the whole file with the field still zero.
        let adjustment = 0xB1B0_AFBA_u32.wrapping_sub(checksum(&out.0));
        out.0[head_offset + 8..head_offset + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    Ok(out.0)
}

/// Length rounded up to a multiple of 4.
fn padded_len(len: usize) -> usize {
    len.div_ceil(4) * 4
}

/// The sfnt table checksum: wrapping sum of big-endian `u32` words, zero padded.
fn checksum(data: &[u8]) -> u32 {
    data.chunks(4).fold(0_u32, |sum, chunk| {
        let mut word = [0_u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

/// `(searchRange, entrySelector, rangeShift)` for `count` items of `item_size` bytes.
fn binary_search_header(count: u16, item_size: u16) -> (u16, u16, u16) {
    if count == 0 {
        return (0, 0, 0);
    }
    let mut entry_selector = 0_u16;
    while (1_u32 << (entry_selector + 1)) <= u32::from(count) {
        entry_selector += 1;
    }
    let power = 1_u32 << entry_selector;
    let search_range = power * u32::from(item_size);
    let range_shift = u32::from(count) * u32::from(item_size) - search_range;
    // Both fit: `search_range <= count * item_size` and the callers keep that within u16.
    (
        u16::try_from(search_range).unwrap_or(u16::MAX),
        entry_selector,
        u16::try_from(range_shift).unwrap_or(u16::MAX),
    )
}

/// `glyf` bytes, long-format `loca` bytes and the per-glyph boxes.
type EncodedGlyf = (Vec<u8>, Vec<u8>, Vec<GlyphBox>);

/// Encodes `glyf` and long-format `loca`, returning per-glyph boxes.
fn encode_glyf(glyphs: &[GlyphData]) -> Result<EncodedGlyf, CompileError> {
    let mut glyf = Buf::default();
    let mut loca = Buf::default();
    let mut boxes = Vec::with_capacity(glyphs.len());
    for glyph in glyphs {
        loca.u32(to_u32(glyf.len(), "loca")?);
        let glyph_box = encode_glyph(&mut glyf, &glyph.contours)?;
        boxes.push(glyph_box);
        glyf.0.resize(padded_len(glyf.len()), 0);
    }
    loca.u32(to_u32(glyf.len(), "loca")?);
    Ok((glyf.0, loca.0, boxes))
}

/// Encodes one simple glyph (nothing for a blank glyph).
fn encode_glyph(out: &mut Buf, contours: &[TtContour]) -> Result<GlyphBox, CompileError> {
    let points: Vec<_> = contours.iter().flatten().copied().collect();
    if points.is_empty() {
        return Ok(GlyphBox { empty: true, ..GlyphBox::default() });
    }
    let glyph_box = GlyphBox {
        empty: false,
        x_min: points.iter().map(|point| point.x).min().unwrap_or(0),
        y_min: points.iter().map(|point| point.y).min().unwrap_or(0),
        x_max: points.iter().map(|point| point.x).max().unwrap_or(0),
        y_max: points.iter().map(|point| point.y).max().unwrap_or(0),
        points: points.len(),
        contours: contours.len(),
    };
    out.i16(i16::try_from(contours.len()).map_err(|_| CompileError::OutputTooLarge { table: "glyf" })?);
    out.i16(glyph_box.x_min);
    out.i16(glyph_box.y_min);
    out.i16(glyph_box.x_max);
    out.i16(glyph_box.y_max);
    let mut end = 0_usize;
    for contour in contours {
        end += contour.len();
        out.u16(to_u16(end - 1, "glyf")?);
    }
    // No instructions: the face is unhinted.
    out.u16(0);

    let mut flags = Vec::with_capacity(points.len());
    let mut xs = Buf::default();
    let mut ys = Buf::default();
    let (mut previous_x, mut previous_y) = (0_i32, 0_i32);
    for point in &points {
        let mut flag = if point.on { ON_CURVE } else { 0 };
        let dx = i32::from(point.x) - previous_x;
        let dy = i32::from(point.y) - previous_y;
        encode_delta(dx, &mut flag, X_SHORT, X_SAME_OR_POSITIVE, &mut xs);
        encode_delta(dy, &mut flag, Y_SHORT, Y_SAME_OR_POSITIVE, &mut ys);
        flags.push(flag);
        previous_x = i32::from(point.x);
        previous_y = i32::from(point.y);
    }
    let mut index = 0;
    while index < flags.len() {
        let flag = flags[index];
        let mut run = 1;
        while index + run < flags.len() && flags[index + run] == flag && run < 256 {
            run += 1;
        }
        if run > 1 {
            out.u8(flag | REPEAT);
            out.u8(u8::try_from(run - 1).unwrap_or(u8::MAX));
        } else {
            out.u8(flag);
        }
        index += run;
    }
    out.bytes(&xs.0);
    out.bytes(&ys.0);
    Ok(glyph_box)
}

/// Encodes one coordinate delta (short form when it fits a byte, nothing when zero).
fn encode_delta(delta: i32, flag: &mut u8, short_bit: u8, same_or_positive_bit: u8, out: &mut Buf) {
    if delta == 0 {
        *flag |= same_or_positive_bit;
    } else if let Ok(magnitude) = u8::try_from(delta.unsigned_abs()) {
        *flag |= short_bit;
        if delta > 0 {
            *flag |= same_or_positive_bit;
        }
        out.u8(magnitude);
    } else {
        // Deltas between two i16 coordinates may exceed i16; the format stores them modulo
        // 2^16, and readers add them back with wrapping arithmetic.
        out.0.extend_from_slice(&delta.to_be_bytes()[2..]);
    }
}

/// `head` (54 bytes); `checkSumAdjustment` is patched by `assemble`.
fn encode_head(font_box: &GlyphBox, metrics: FontMetrics) -> Vec<u8> {
    let mut out = Buf::default();
    out.u32(0x0001_0000); // version
    out.u32(0x0001_0000); // fontRevision 1.0
    out.u32(0); // checkSumAdjustment
    out.u32(0x5F0F_3CF5); // magicNumber
    out.u16(0b0000_0000_0000_0011); // baseline at y=0, left sidebearing at x=0
    out.u16(TARGET_UPM);
    out.bytes(&[0; 8]); // created (fixed for determinism)
    out.bytes(&[0; 8]); // modified
    out.i16(font_box.x_min);
    out.i16(font_box.y_min);
    out.i16(font_box.x_max);
    out.i16(font_box.y_max);
    out.u16(mac_style(metrics));
    out.u16(8); // lowestRecPPEM
    out.i16(2); // fontDirectionHint (deprecated, fixed value)
    out.i16(1); // indexToLocFormat: long offsets
    out.i16(0); // glyphDataFormat
    out.0
}

/// `head.macStyle` bits (bold, italic).
fn mac_style(metrics: FontMetrics) -> u16 {
    let mut style = 0;
    if metrics.weight_class >= 700 {
        style |= 0x01;
    }
    if metrics.italic {
        style |= 0x02;
    }
    style
}

/// `hhea` (36 bytes).
fn encode_hhea(build: &FontBuild, boxes: &[GlyphBox], glyph_count: u16) -> Vec<u8> {
    let mut advance_max = 0_u16;
    let mut min_left_bearing = i32::MAX;
    let mut min_right_bearing = i32::MAX;
    let mut max_extent = i32::MIN;
    for (glyph, glyph_box) in build.glyphs.iter().zip(boxes) {
        advance_max = advance_max.max(glyph.advance);
        if glyph_box.empty {
            continue;
        }
        let lsb = i32::from(glyph_box.x_min);
        let width = i32::from(glyph_box.x_max) - i32::from(glyph_box.x_min);
        min_left_bearing = min_left_bearing.min(lsb);
        min_right_bearing = min_right_bearing.min(i32::from(glyph.advance) - lsb - width);
        max_extent = max_extent.max(lsb + width);
    }
    let clamp = |value: i32| -> i16 {
        if value == i32::MAX || value == i32::MIN { 0 } else { i16::try_from(value).unwrap_or(if value < 0 { i16::MIN } else { i16::MAX }) }
    };
    let mut out = Buf::default();
    out.u32(0x0001_0000);
    out.i16(build.metrics.ascender);
    out.i16(build.metrics.descender);
    out.i16(build.metrics.line_gap);
    out.u16(advance_max);
    out.i16(clamp(min_left_bearing));
    out.i16(clamp(min_right_bearing));
    out.i16(clamp(max_extent));
    out.i16(1); // caretSlopeRise
    out.i16(0); // caretSlopeRun
    out.i16(0); // caretOffset
    out.bytes(&[0; 8]); // reserved
    out.i16(0); // metricDataFormat
    out.u16(glyph_count); // numberOfHMetrics: every glyph has a full record
    out.0
}

/// `hmtx`: one `(advance, lsb)` record per glyph; lsb = glyph `xMin` (0 for blank glyphs).
fn encode_hmtx(build: &FontBuild, boxes: &[GlyphBox]) -> Vec<u8> {
    let mut out = Buf::default();
    for (glyph, glyph_box) in build.glyphs.iter().zip(boxes) {
        out.u16(glyph.advance);
        out.i16(if glyph_box.empty { 0 } else { glyph_box.x_min });
    }
    out.0
}

/// `maxp` version 1.0 (32 bytes).
fn encode_maxp(boxes: &[GlyphBox], glyph_count: u16) -> Result<Vec<u8>, CompileError> {
    let max_points = boxes.iter().map(|glyph_box| glyph_box.points).max().unwrap_or(0);
    let max_contours = boxes.iter().map(|glyph_box| glyph_box.contours).max().unwrap_or(0);
    let mut out = Buf::default();
    out.u32(0x0001_0000);
    out.u16(glyph_count);
    out.u16(to_u16(max_points, "maxp")?);
    out.u16(to_u16(max_contours, "maxp")?);
    out.u16(0); // maxCompositePoints
    out.u16(0); // maxCompositeContours
    out.u16(2); // maxZones
    out.u16(0); // maxTwilightPoints
    out.u16(0); // maxStorage
    out.u16(0); // maxFunctionDefs
    out.u16(0); // maxInstructionDefs
    out.u16(0); // maxStackElements
    out.u16(0); // maxSizeOfInstructions
    out.u16(0); // maxComponentElements
    out.u16(0); // maxComponentDepth
    Ok(out.0)
}

/// `post` version 3.0 (no glyph names).
fn encode_post(metrics: FontMetrics) -> Vec<u8> {
    let mut out = Buf::default();
    out.u32(0x0003_0000);
    out.i32(fixed_16_16(metrics.italic_angle));
    out.i16(metrics.underline_position);
    out.i16(metrics.underline_thickness);
    out.u32(0); // isFixedPitch
    out.bytes(&[0; 16]); // memory hints
    out.0
}

/// A 16.16 fixed-point value (saturating).
fn fixed_16_16(value: f64) -> i32 {
    let scaled = (value * 65536.0).round();
    if !scaled.is_finite() {
        return 0;
    }
    let clamped = scaled.clamp(f64::from(i32::MIN), f64::from(i32::MAX));
    #[expect(clippy::cast_possible_truncation, reason = "clamped to the i32 range just above")]
    let fixed = clamped as i32;
    fixed
}

/// `OS/2` version 4 (96 bytes).
fn encode_os2(build: &FontBuild, font_box: &GlyphBox) -> Vec<u8> {
    let metrics = build.metrics;
    let advances: Vec<u32> = build.glyphs.iter().map(|glyph| u32::from(glyph.advance)).filter(|&advance| advance > 0).collect();
    let average_width = u32::try_from(advances.len())
        .ok()
        .filter(|&count| count > 0)
        .map_or(0, |count| (advances.iter().sum::<u32>() + count / 2) / count);
    let em = i32::from(TARGET_UPM);
    let script_size = i16::try_from(em * 65 / 100).unwrap_or(0);
    let (first_char, last_char) = build.cmap.first().zip(build.cmap.last()).map_or((0, 0), |((first, _), (last, _))| {
        (u16::try_from(u32::from(*first)).unwrap_or(0xFFFF), u16::try_from(u32::from(*last)).unwrap_or(0xFFFF))
    });
    let (unicode_ranges, code_pages) = coverage_bits(&build.cmap);

    let mut fs_selection = 0x0080_u16; // USE_TYPO_METRICS
    if metrics.italic {
        fs_selection |= 0x0001;
    }
    if metrics.weight_class >= 700 {
        fs_selection |= 0x0020;
    }
    if !metrics.italic && metrics.weight_class < 700 {
        fs_selection |= 0x0040; // REGULAR
    }

    let mut out = Buf::default();
    out.u16(4);
    out.i16(i16::try_from(average_width).unwrap_or(i16::MAX));
    out.u16(metrics.weight_class);
    out.u16(5); // usWidthClass: medium
    out.u16(0); // fsType: installable
    out.i16(script_size); // ySubscriptXSize
    out.i16(script_size); // ySubscriptYSize
    out.i16(0); // ySubscriptXOffset
    out.i16(i16::try_from(em * 14 / 100).unwrap_or(0)); // ySubscriptYOffset
    out.i16(script_size); // ySuperscriptXSize
    out.i16(script_size); // ySuperscriptYSize
    out.i16(0); // ySuperscriptXOffset
    out.i16(i16::try_from(em * 35 / 100).unwrap_or(0)); // ySuperscriptYOffset
    out.i16(metrics.underline_thickness.max(1)); // yStrikeoutSize
    out.i16(if metrics.x_height > 0 { metrics.x_height / 2 } else { i16::try_from(em / 4).unwrap_or(0) });
    out.i16(0); // sFamilyClass
    out.bytes(&[0; 10]); // panose: any
    for range in unicode_ranges {
        out.u32(range);
    }
    out.bytes(b"MSCF"); // achVendID
    out.u16(fs_selection);
    out.u16(first_char);
    out.u16(last_char);
    out.i16(metrics.ascender);
    out.i16(metrics.descender);
    out.i16(metrics.line_gap);
    // Windows clips at the win metrics: cover every outline.
    out.u16(u16::try_from(i32::from(metrics.ascender).max(i32::from(font_box.y_max)).max(0)).unwrap_or(u16::MAX));
    out.u16(u16::try_from((-i32::from(metrics.descender)).max(-i32::from(font_box.y_min)).max(0)).unwrap_or(u16::MAX));
    out.u32(code_pages[0]);
    out.u32(code_pages[1]);
    out.i16(metrics.x_height);
    out.i16(metrics.cap_height);
    out.u16(0); // usDefaultChar
    out.u16(0x20); // usBreakChar
    out.u16(2); // usMaxContext: pair kerning
    out.0
}

/// `OS/2` `ulUnicodeRange1..4` and `ulCodePageRange1..2` bits for the main scripts covered.
fn coverage_bits(cmap: &[(char, u16)]) -> ([u32; 4], [u32; 2]) {
    // (first, last, unicode range bit, optional code page bit)
    const RANGES: &[(u32, u32, u32, Option<u32>)] = &[
        (0x0020, 0x007E, 0, Some(0)),   // Basic Latin -> cp1252
        (0x0080, 0x00FF, 1, None),      // Latin-1 Supplement
        (0x0100, 0x017F, 2, Some(1)),   // Latin Extended-A -> cp1250
        (0x0180, 0x024F, 3, None),      // Latin Extended-B
        (0x0370, 0x03FF, 7, Some(3)),   // Greek -> cp1253
        (0x0400, 0x052F, 9, Some(2)),   // Cyrillic -> cp1251
        (0x1100, 0x11FF, 28, None),     // Hangul Jamo
        (0x2000, 0x206F, 31, None),     // General Punctuation
        (0x3000, 0x303F, 48, None),     // CJK Symbols and Punctuation
        (0x3040, 0x309F, 49, Some(17)), // Hiragana -> JIS
        (0x30A0, 0x30FF, 50, None),     // Katakana
        (0x3130, 0x318F, 52, None),     // Hangul Compatibility Jamo
        (0x4E00, 0x9FFF, 59, Some(18)), // CJK Unified Ideographs -> Chinese Simplified
        (0xAC00, 0xD7AF, 56, Some(19)), // Hangul Syllables -> Korean Wansung
        (0xFF00, 0xFFEF, 68, None),     // Halfwidth and Fullwidth Forms
    ];
    let mut unicode = [0_u32; 4];
    let mut code_pages = [0_u32; 2];
    for &(first, last, bit, code_page) in RANGES {
        let covered = cmap.iter().any(|(ch, _)| (first..=last).contains(&u32::from(*ch)));
        if covered {
            unicode[usize::try_from(bit / 32).unwrap_or(0)] |= 1 << (bit % 32);
            if let Some(page) = code_page {
                code_pages[usize::try_from(page / 32).unwrap_or(0)] |= 1 << (page % 32);
            }
        }
    }
    (unicode, code_pages)
}

/// `name` table, format 0: Mac Roman (ASCII names only) and Windows Unicode English records.
fn encode_name(names: &FontNames) -> Result<Vec<u8>, CompileError> {
    let entries: [(u16, &str); 6] = [
        (1, &names.family),
        (2, &names.subfamily),
        (3, &names.unique_id),
        (4, &names.full_name),
        (5, &names.version),
        (6, &names.postscript),
    ];
    let mut records: Vec<(u16, u16, u16, u16, Vec<u8>)> = Vec::new();
    for &(name_id, text) in &entries {
        if text.is_ascii() {
            records.push((1, 0, 0, name_id, text.as_bytes().to_vec()));
        }
    }
    for &(name_id, text) in &entries {
        let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        records.push((3, 1, 0x0409, name_id, utf16));
    }
    let count = to_u16(records.len(), "name")?;
    let mut out = Buf::default();
    out.u16(0);
    out.u16(count);
    out.u16(to_u16(6 + 12 * records.len(), "name")?);
    let mut storage = Buf::default();
    for (platform, encoding, language, name_id, bytes) in &records {
        out.u16(*platform);
        out.u16(*encoding);
        out.u16(*language);
        out.u16(*name_id);
        out.u16(to_u16(bytes.len(), "name")?);
        out.u16(to_u16(storage.len(), "name")?);
        storage.bytes(bytes);
    }
    out.bytes(&storage.0);
    Ok(out.0)
}

/// `cmap` with a shared format-4 and format-12 subtable.
fn encode_cmap(cmap: &[(char, u16)]) -> Result<Vec<u8>, CompileError> {
    let format4 = encode_cmap_format4(cmap)?;
    let format12 = encode_cmap_format12(cmap)?;
    let header_len = 4 + 4 * 8;
    let offset4 = to_u32(header_len, "cmap")?;
    let offset12 = to_u32(header_len + format4.len(), "cmap")?;
    let mut out = Buf::default();
    out.u16(0);
    out.u16(4);
    for (platform, encoding, offset) in [(0, 3, offset4), (0, 4, offset12), (3, 1, offset4), (3, 10, offset12)] {
        out.u16(platform);
        out.u16(encoding);
        out.u32(offset);
    }
    out.bytes(&format4);
    out.bytes(&format12);
    Ok(out.0)
}

/// One format-4 segment: codepoint range plus either a delta or explicit glyph ids.
struct Segment4 {
    start: u16,
    end: u16,
    /// `Some(delta)` when glyph = codepoint + delta over the whole range.
    delta: Option<u16>,
    glyphs: Vec<u16>,
}

/// cmap format 4 over the BMP part of `cmap` (runs that would overflow the 64 KiB length are
/// left to format 12).
fn encode_cmap_format4(cmap: &[(char, u16)]) -> Result<Vec<u8>, CompileError> {
    // Contiguous codepoint runs (U+FFFF is reserved for the terminator segment).
    let bmp: Vec<(u16, u16)> = cmap
        .iter()
        .filter_map(|&(ch, glyph)| u16::try_from(u32::from(ch)).ok().filter(|&cp| cp != 0xFFFF).map(|cp| (cp, glyph)))
        .collect();
    let mut runs: Vec<&[(u16, u16)]> = Vec::new();
    let mut start = 0;
    for index in 1..=bmp.len() {
        if index == bmp.len() || bmp[index].0 != bmp[index - 1].0 + 1 {
            runs.push(&bmp[start..index]);
            start = index;
        }
    }
    // Per run: split into delta sub-runs or keep one array segment, whichever is smaller.
    let mut segments: Vec<Segment4> = Vec::new();
    let mut length = 16 + 8; // header + terminator segment
    for run in runs {
        let mut delta_runs: Vec<&[(u16, u16)]> = Vec::new();
        let mut sub_start = 0;
        for index in 1..=run.len() {
            if index == run.len() || run[index].1 != run[index - 1].1.wrapping_add(1) {
                delta_runs.push(&run[sub_start..index]);
                sub_start = index;
            }
        }
        let delta_cost = 8 * delta_runs.len();
        let array_cost = 8 + 2 * run.len();
        let (pieces, cost): (Vec<Segment4>, usize) = if delta_cost <= array_cost {
            let pieces = delta_runs
                .iter()
                .map(|piece| Segment4 {
                    start: piece[0].0,
                    end: piece[piece.len() - 1].0,
                    delta: Some(piece[0].1.wrapping_sub(piece[0].0)),
                    glyphs: Vec::new(),
                })
                .collect();
            (pieces, delta_cost)
        } else {
            let piece = Segment4 {
                start: run[0].0,
                end: run[run.len() - 1].0,
                delta: None,
                glyphs: run.iter().map(|&(_, glyph)| glyph).collect(),
            };
            (vec![piece], array_cost)
        };
        if length + cost > usize::from(u16::MAX) {
            break;
        }
        length += cost;
        segments.extend(pieces);
    }
    segments.push(Segment4 { start: 0xFFFF, end: 0xFFFF, delta: Some(1), glyphs: Vec::new() });

    let seg_count = to_u16(segments.len(), "cmap")?;
    let (search_range, entry_selector, range_shift) = binary_search_header(seg_count, 2);
    let mut out = Buf::default();
    out.u16(4);
    out.u16(to_u16(length, "cmap")?);
    out.u16(0); // language
    out.u16(seg_count * 2);
    out.u16(search_range);
    out.u16(entry_selector);
    out.u16(range_shift);
    for segment in &segments {
        out.u16(segment.end);
    }
    out.u16(0); // reservedPad
    for segment in &segments {
        out.u16(segment.start);
    }
    for segment in &segments {
        out.u16(segment.delta.unwrap_or(0));
    }
    // idRangeOffset: bytes from this entry to the segment's first glyphIdArray element.
    let mut array_index = 0_usize;
    for (index, segment) in segments.iter().enumerate() {
        if segment.delta.is_some() {
            out.u16(0);
        } else {
            out.u16(to_u16(2 * (segments.len() - index) + 2 * array_index, "cmap")?);
            array_index += segment.glyphs.len();
        }
    }
    for segment in &segments {
        for &glyph in &segment.glyphs {
            out.u16(glyph);
        }
    }
    Ok(out.0)
}

/// cmap format 12 over the whole of `cmap`.
fn encode_cmap_format12(cmap: &[(char, u16)]) -> Result<Vec<u8>, CompileError> {
    let mut groups: Vec<(u32, u32, u32)> = Vec::new();
    for &(ch, glyph) in cmap {
        let codepoint = u32::from(ch);
        let glyph = u32::from(glyph);
        // A group maps consecutive codepoints to consecutive glyphs.
        match groups.last_mut() {
            Some((start, end, start_glyph)) if *end + 1 == codepoint && glyph == *start_glyph + (codepoint - *start) => {
                *end = codepoint;
            }
            _ => groups.push((codepoint, codepoint, glyph)),
        }
    }
    let mut out = Buf::default();
    out.u16(12);
    out.u16(0);
    out.u32(to_u32(16 + 12 * groups.len(), "cmap")?);
    out.u32(0); // language
    out.u32(to_u32(groups.len(), "cmap")?);
    for (start, end, glyph) in groups {
        out.u32(start);
        out.u32(end);
        out.u32(glyph);
    }
    Ok(out.0)
}

/// `kern` version 0 with format-0 subtables.
fn encode_kern(pairs: &[(u16, u16, i16)]) -> Result<Vec<u8>, CompileError> {
    let chunks: Vec<&[(u16, u16, i16)]> = pairs.chunks(KERN_PAIRS_PER_SUBTABLE).collect();
    let mut out = Buf::default();
    out.u16(0);
    out.u16(to_u16(chunks.len(), "kern")?);
    for chunk in chunks {
        let count = to_u16(chunk.len(), "kern")?;
        let (search_range, entry_selector, range_shift) = binary_search_header(count, 6);
        out.u16(0); // subtable version
        out.u16(to_u16(14 + 6 * chunk.len(), "kern")?);
        out.u16(0x0001); // horizontal, format 0
        out.u16(count);
        out.u16(search_range);
        out.u16(entry_selector);
        out.u16(range_shift);
        for &(left, right, value) in chunk {
            out.u16(left);
            out.u16(right);
            out.i16(value);
        }
    }
    Ok(out.0)
}
