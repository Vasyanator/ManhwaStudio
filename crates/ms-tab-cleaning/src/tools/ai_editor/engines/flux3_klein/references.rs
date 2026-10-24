/*
File: cleaning/tools/ai_editor/engines/flux3_klein/references.rs

Purpose:
The extra reference images of the FLUX.3 klein engine: the session list the user builds with
«Добавить референс», its capacity rule, and the worker-side read of a picked file into the
wire PNG the run sends.

Main responsibilities:
- own `Flux3Reference` (display name, pixel size, wire PNG) and `Flux3References` (the list);
- decide how many more may be added and whether the marks may still travel as a separate
  reference (`free_slots`, `allows_marks_reference`);
- read one picked file on a WORKER (`read_reference_file`): decode, re-encode as an RGBA PNG
  through the cleaning tab's one wire codec, refuse an unreadable file with a reason.

Key structures:
- `Flux3Reference`, `Flux3References`, `Flux3ReferenceLoad`

Key functions:
- `read_reference_file()`, `Flux3References::{push_loaded, remove, clear, free_slots,
  allows_marks_reference, sizes}`

Notes:
SESSION state, never persisted: a reference is a picture for this page, and a settings file
that named image paths would break silently as soon as a file moved. The list is capped at
`FLUX3_MAX_EXTRA_REFERENCES` (the model takes ten images and the region is always #0). A
marks reference is ONE MORE extra reference, so while the list is full the engine offers only
«На редактируемую картинку» for the marks (`marks_support`).
*/

use super::*;

/// One extra reference image, ready for the wire.
#[derive(Debug, Clone)]
pub(super) struct Flux3Reference {
    /// The file name the user picked (display only).
    pub(super) name: String,
    /// `[width, height]` in pixels — what the forecast's `shape.references` carries.
    pub(super) size: [u32; 2],
    /// The image as an RGBA8 PNG (the wire form). Shared, so a run takes it without copying.
    pub(super) png: Arc<Vec<u8>>,
}

/// What the read of one picked file answered: the reference, or a localized refusal naming
/// the file.
pub(super) type Flux3ReferenceLoad = Result<Flux3Reference, String>;

/// The session's extra reference images, in the order the model receives them (#1 and up).
#[derive(Debug, Clone, Default)]
pub(super) struct Flux3References {
    pub(super) items: Vec<Flux3Reference>,
}

impl Flux3References {
    /// How many more references may be added.
    pub(super) fn free_slots(&self) -> usize {
        FLUX3_MAX_EXTRA_REFERENCES.saturating_sub(self.items.len())
    }

    /// Whether a marks reference still fits beside the user's references: it is one more extra
    /// reference, so a full list leaves the marks only the composited-onto-the-region mode.
    pub(super) fn allows_marks_reference(&self) -> bool {
        self.free_slots() > 0
    }

    /// Appends the references a worker read, up to the capacity, and returns the refusals —
    /// the read failures plus one line when files had to be left out because the list is full.
    /// `left_out` counts picked files the worker did not even read (no slot was free for them).
    pub(super) fn push_loaded(&mut self, loads: Vec<Flux3ReferenceLoad>, left_out: usize) -> Vec<String> {
        let mut refusals = Vec::new();
        let mut dropped = left_out;
        for load in loads {
            match load {
                Ok(reference) if self.free_slots() > 0 => self.items.push(reference),
                Ok(_) => dropped += 1,
                Err(refusal) => refusals.push(refusal),
            }
        }
        if dropped > 0 {
            refusals.push(tf!("cleaning.tools.flux3_klein.references_full_error", count = dropped, max = FLUX3_MAX_EXTRA_REFERENCES));
        }
        refusals
    }

    /// Removes the reference at `index`; an index past the end is a no-op.
    pub(super) fn remove(&mut self, index: usize) {
        if index < self.items.len() {
            self.items.remove(index);
        }
    }

    /// Drops every reference.
    pub(super) fn clear(&mut self) {
        self.items.clear();
    }

    /// The pixel sizes, in order (the forecast's references after the region).
    pub(super) fn sizes(&self) -> Vec<[u32; 2]> {
        self.items.iter().map(|reference| reference.size).collect()
    }
}

/// Reads one picked image file into a wire reference. BLOCKING (file read + decode + encode):
/// worker threads only.
///
/// Any format the `image` crate decodes is accepted and re-encoded as an RGBA8 PNG through the
/// tab's one wire codec (`encode_color_image_png_rgba`), so the backend always receives what
/// its decoder expects; the alpha is kept and flattened backend-side like the region's.
///
/// # Errors
/// A localized refusal naming the file: it cannot be read or decoded, it has no pixels, or it
/// cannot be encoded. The technical detail is logged.
pub(super) fn read_reference_file(path: &Path, name: &str) -> Flux3ReferenceLoad {
    let refuse = |detail: String| {
        ms_log::runtime_log::log_warn(format!("[cleaning] {FLUX3_LOG_LABEL} reference {} not usable: {detail}", path.display()));
        tf!("cleaning.tools.flux3_klein.reference_unreadable_error", name = name, err = detail)
    };
    let decoded = image::open(path).map_err(|err| refuse(err.to_string()))?.to_rgba8();
    let (width, height) = decoded.dimensions();
    if width == 0 || height == 0 {
        return Err(refuse(format!("{width}x{height} image")));
    }
    let size = [width, height];
    let side = |px: u32| usize::try_from(px).map_err(|err| refuse(err.to_string()));
    let image = egui::ColorImage::from_rgba_unmultiplied([side(width)?, side(height)?], decoded.as_raw());
    let png = encode_color_image_png_rgba(&image).map_err(refuse)?;
    Ok(Flux3Reference { name: name.to_string(), size, png: Arc::new(png) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(name: &str) -> Flux3Reference {
        Flux3Reference { name: name.to_string(), size: [64, 32], png: Arc::new(vec![1, 2, 3]) }
    }

    /// Nine fit; a tenth is refused with one line, a read failure is reported verbatim, and a
    /// full list closes the separate-reference marks mode.
    #[test]
    fn the_list_is_capped_at_nine_and_a_full_list_closes_the_marks_reference() {
        let mut list = Flux3References::default();
        assert!(list.allows_marks_reference());
        let loads: Vec<Flux3ReferenceLoad> = (0..8).map(|i| Ok(reference(&format!("r{i}")))).collect();
        assert!(list.push_loaded(loads, 0).is_empty());
        assert_eq!(list.free_slots(), 1);
        assert!(list.allows_marks_reference(), "eight user references leave room for the marks");
        let refusals = list.push_loaded(vec![Ok(reference("r8")), Ok(reference("r9")), Err("broken.png".to_string())], 0);
        assert_eq!(list.items.len(), FLUX3_MAX_EXTRA_REFERENCES);
        assert_eq!(refusals.len(), 2, "the read failure and one line for the left-out file: {refusals:?}");
        assert_eq!(refusals[0], "broken.png");
        assert!(!list.allows_marks_reference());
        assert_eq!(list.sizes(), vec![[64, 32]; 9]);
        list.remove(0);
        list.remove(42);
        assert_eq!(list.items.len(), 8);
        assert_eq!(list.items[0].name, "r1", "order is kept");
        list.clear();
        assert_eq!(list.free_slots(), FLUX3_MAX_EXTRA_REFERENCES);
        // Files the worker never read count as left out, and a failed read frees no slot for them.
        let refusals = list.push_loaded(vec![Err("bad.png".to_string())], 3);
        assert!(list.items.is_empty());
        assert_eq!(refusals.len(), 2);
    }

    /// A real file becomes an RGBA PNG of its own size; garbage is refused, not sent.
    #[test]
    fn a_picked_file_is_read_into_a_wire_png() {
        let dir = std::env::temp_dir().join(format!("ms_flux3_refs_{}_{}", std::process::id(), unix_now_secs()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let good = dir.join("ref.png");
        image::RgbImage::from_pixel(40, 24, image::Rgb([10, 20, 30])).save(&good).expect("write a png");
        let reference = read_reference_file(&good, "ref.png").expect("a readable image");
        assert_eq!(reference.size, [40, 24]);
        let decoded = image::load_from_memory(&reference.png).expect("the wire bytes are an image").to_rgba8();
        assert_eq!(decoded.dimensions(), (40, 24));
        assert_eq!(decoded.get_pixel(3, 3).0, [10, 20, 30, 255]);
        let bad = dir.join("bad.png");
        std::fs::write(&bad, b"not an image").expect("write");
        assert!(read_reference_file(&bad, "bad.png").is_err());
        assert!(read_reference_file(&dir.join("missing.png"), "missing.png").is_err());
        std::fs::remove_dir_all(&dir).expect("clean up");
    }
}
