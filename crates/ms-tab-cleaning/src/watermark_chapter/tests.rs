/*
File: watermark_chapter/tests.rs

Purpose:
Unit tests of the chapter watermark engine (synthetic marks, fits, detection, removal).

Key items:
- `SyntheticMark` and the per-stage test cases

Notes:
Moved out of `mod.rs` unchanged; GUI-free, no files written.
*/
use super::*;
use super::detection::{provisional_background, scan_chapter};
use super::kinds::refit_with_refined_backgrounds;

/// Synthetic mark: a rectangular "glyph" of a bright fill inside a dark outline, i.e. the
/// structure the measured 腾讯动漫 mark has (white glyph, dark outline). Per-channel alpha
/// makes it a colour mark when the three values differ; a coloured FILL makes it one the way
/// the measured chapter-two mark is (its deposit differs by up to 120 LSB between channels).
struct SyntheticMark {
    width: u32,
    height: u32,
    c: Vec<f32>,
    s: Vec<f32>,
}

impl SyntheticMark {
    /// `alpha` is per channel; the fill is bright, the outline dark, both at that alpha.
    fn new(width: u32, height: u32, alpha: [f32; 3]) -> Self {
        Self::with_colours(width, height, alpha, [235.0, 243.0, 242.0], [12.0, 14.0, 13.0])
    }

    /// The same glyph geometry with explicit fill and outline colours: two marks built this
    /// way have the SAME shape and different deposits, which is the measured case a
    /// template-keyed catalog would merge.
    fn with_colours(
        width: u32,
        height: u32,
        alpha: [f32; 3],
        fill: [f32; 3],
        outline: [f32; 3],
    ) -> Self {
        let (w, h) = (width as usize, height as usize);
        let mut c = vec![0f32; w * h * 3];
        let mut s = vec![1f32; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                // Glyph body: a centred rectangle with a 2 px outline ring around it.
                let in_glyph = x >= 2 && y >= 2 && x + 2 < w && y + 2 < h;
                let in_outline = (x >= 1 && y >= 1 && x + 1 < w && y + 1 < h) && !in_glyph;
                if !in_glyph && !in_outline {
                    continue;
                }
                let colour = if in_glyph { fill } else { outline };
                for channel in 0..3 {
                    let index = (y * w + x) * 3 + channel;
                    c[index] = alpha[channel] * colour[channel];
                    s[index] = 1.0 - alpha[channel];
                }
            }
        }
        Self {
            width,
            height,
            c,
            s,
        }
    }

    /// Composite the mark onto `page` at `rect`, optionally scaling the alpha (`k = 1` is the
    /// mark itself; `k != 1` makes a look-alike with the same SHAPE but a different
    /// amplitude, which correlation cannot tell apart).
    fn composite(&self, page: &mut RgbaImage, rect: PixelRect, alpha_scale: f32) {
        let (w, _h) = (self.width as usize, self.height as usize);
        for y in 0..self.height as usize {
            for x in 0..w {
                let index = (y * w + x) * 3;
                let pixel = page.get_pixel_mut(rect.x + x as u32, rect.y + y as u32);
                for channel in 0..3 {
                    let c = self.c[index + channel] * alpha_scale;
                    let s = 1.0 - (1.0 - self.s[index + channel]) * alpha_scale;
                    let background = f32::from(pixel[channel]);
                    pixel[channel] = (c + s * background).clamp(0.0, 255.0).round() as u8;
                }
            }
        }
    }

    /// A hollow variant: the 2 px outline ring only, no fill. The measured marks are thin
    /// glyph strokes, and a hollow mark is what lets the `s`-weighted background estimator
    /// work the way it does on real data — it needs unmarked pixels within
    /// `BACKGROUND_BLUR_RADIUS_PX` of every mark pixel, which a solid block this size does
    /// not have.
    fn ring(width: u32, height: u32, alpha: [f32; 3]) -> Self {
        let mut mark = Self::new(width, height, alpha);
        let (w, h) = (width as usize, height as usize);
        for y in 0..h {
            for x in 0..w {
                if x >= 2 && y >= 2 && x + 2 < w && y + 2 < h {
                    for channel in 0..3 {
                        let index = (y * w + x) * 3 + channel;
                        mark.c[index] = 0.0;
                        mark.s[index] = 1.0;
                    }
                }
            }
        }
        mark
    }

    fn rect_at(&self, x: u32, y: u32) -> PixelRect {
        PixelRect::new(x, y, self.width, self.height)
    }

    fn model(&self) -> WatermarkModel {
        WatermarkModel::from_parts(
            self.width,
            self.height,
            self.c.clone(),
            self.s.clone(),
            alpha_blend_operator(),
            test_provenance(),
        )
        .expect("synthetic model parameters are in range")
    }
}

// -----------------------------------------------------------------------------------
// Footprint trim
// -----------------------------------------------------------------------------------

/// `mark` sitting at `at` inside a `footprint`-sized box that is fully transparent everywhere
/// else — exactly the shape a detector's loose box produces.
fn padded_mark(footprint: (u32, u32), at: (u32, u32), mark: &SyntheticMark) -> SyntheticMark {
    let (fw, fh) = (footprint.0 as usize, footprint.1 as usize);
    let mut c = vec![0f32; fw * fh * 3];
    let mut s = vec![1f32; fw * fh * 3];
    let mw = mark.width as usize;
    for y in 0..mark.height as usize {
        for x in 0..mw {
            let from = (y * mw + x) * 3;
            let to = (((at.1 as usize + y) * fw) + at.0 as usize + x) * 3;
            c[to..to + 3].copy_from_slice(&mark.c[from..from + 3]);
            s[to..to + 3].copy_from_slice(&mark.s[from..from + 3]);
        }
    }
    SyntheticMark {
        width: footprint.0,
        height: footprint.1,
        c,
        s,
    }
}

/// A flat `level` crop with an opaque block at `rect` and, one pixel outside it, a `fringe`
/// LSB halo — the antialiased edge a real mark has.
fn template_with_block(
    size: (u32, u32),
    rect: PixelRect,
    level: u8,
    block: u8,
    fringe: u8,
) -> RgbaImage {
    RgbaImage::from_fn(size.0, size.1, |x, y| {
        let inside = x >= rect.x
            && y >= rect.y
            && u64::from(x) < rect.right()
            && u64::from(y) < rect.bottom();
        let haloed = x + 1 >= rect.x
            && y + 1 >= rect.y
            && u64::from(x) <= rect.right()
            && u64::from(y) <= rect.bottom();
        let value = if inside {
            block
        } else if haloed {
            level.saturating_sub(fringe)
        } else {
            level
        };
        image::Rgba([value, value, value, 255])
    })
}

/// The measured defect: a detector box several times the size of the mark inside it. The trim
/// must return the mark's own extent plus the safety border and nothing else.
#[test]
fn a_loose_model_footprint_trims_to_the_marks_own_extent() {
    let mark = SyntheticMark::new(24, 16, [0.18; 3]);
    let model = padded_mark((200, 120), (80, 50), &mark).model();
    let trimmed = trimmed_footprint_from_model(&model).expect("the mark deposits somewhere");
    // `SyntheticMark` leaves its outermost 1 px frame transparent, so the deposit spans
    // (81, 51)..=(102, 64); the safety border grows that by 4 on every side.
    assert_eq!(trimmed, PixelRect::new(77, 47, 30, 22));
    assert!(
        trimmed.area() * 10 < u64::from(model.width()) * u64::from(model.height()),
        "the padding is the overwhelming majority of the box and must be gone: {trimmed:?}"
    );
}

/// The user's own entry, to the pixel: a 319x236 template whose mark occupies x 182..308,
/// y 110..143. Before the trim the footprint reads as flush against the right edge of an
/// 800 px page (anchor 480 + 319 = 799); after it, it does not.
#[test]
fn the_measured_entrys_template_trims_to_its_mark() {
    let template = template_with_block(
        (319, 236),
        PixelRect::new(182, 110, 127, 34),
        255,
        40,
        0,
    );
    let trimmed =
        trimmed_footprint_from_template(&template, PixelRect::new(0, 0, 319, 236))
            .expect("the template holds a mark");
    assert_eq!(trimmed, PixelRect::new(178, 106, 135, 42));
    // The mark's real extent is covered with room to spare on every side.
    assert!(trimmed.x <= 182 && trimmed.y <= 110);
    assert!(trimmed.right() >= 309 && trimmed.bottom() >= 144);
    // The anchor moves with the footprint's left edge, and the mark stops hugging the page.
    let anchor = 480 + trimmed.x;
    assert_eq!(anchor, 658);
    assert!(
        u64::from(anchor) + u64::from(trimmed.width) + u64::from(RING_WIDTH_PX) <= 800,
        "a trimmed mark must leave room for its own measurement ring on the page"
    );
    assert!(
        480 + u64::from(319u32) + u64::from(RING_WIDTH_PX) > 800,
        "the untrimmed footprint is the one that did not"
    );
}

/// A faint antialiased edge is SIGNAL. Even where it fades to a single LSB — below the
/// detection floor, because one LSB is indistinguishable from rounding — the safety border is
/// what keeps it inside the footprint.
#[test]
fn a_faint_antialiased_edge_is_not_trimmed_away() {
    // A 2 LSB halo is above the floor and is detected outright.
    let detected = template_with_block((80, 60), PixelRect::new(30, 20, 10, 8), 255, 0, 2);
    let trimmed = trimmed_footprint_from_template(&detected, PixelRect::new(0, 0, 80, 60))
        .expect("the template holds a mark");
    assert_eq!(trimmed, PixelRect::new(25, 15, 20, 18));

    // A 1 LSB halo is at the quantization floor and is NOT detected — and is still inside
    // the trimmed footprint, because the safety border is four pixels wide.
    let faint = template_with_block((80, 60), PixelRect::new(30, 20, 10, 8), 255, 0, 1);
    let trimmed = trimmed_footprint_from_template(&faint, PixelRect::new(0, 0, 80, 60))
        .expect("the template holds a mark");
    assert!(
        trimmed.x <= 29 && trimmed.y <= 19,
        "the one-LSB halo starts one pixel outside the block and must stay covered: \
         {trimmed:?}"
    );
    assert!(trimmed.right() >= 41 && trimmed.bottom() >= 29, "{trimmed:?}");
}

/// The RAW extent states exactly what was measured; the trimmed one is that plus the safety
/// border. The reference intake needs both and they must not drift apart: the raw box is what
/// says a crop CUT the mark (it reaches the crop's border), and the grown one reaches that
/// border routinely and says nothing.
#[test]
fn the_raw_mark_extent_is_the_trimmed_one_without_its_safety_border() {
    let block = PixelRect::new(30, 20, 10, 8);
    let template = template_with_block((80, 60), block, 255, 0, 0);
    let footprint = PixelRect::new(0, 0, 80, 60);
    let raw = mark_extent_from_template(&template, footprint).expect("the template holds a mark");
    assert_eq!(raw, block, "the raw extent is the deposit itself, to the pixel");
    let trimmed = trimmed_footprint_from_template(&template, footprint)
        .expect("the template holds a mark");
    assert_eq!(
        trimmed,
        PixelRect::new(
            block.x - FOOTPRINT_TRIM_SAFETY_PX,
            block.y - FOOTPRINT_TRIM_SAFETY_PX,
            block.width + FOOTPRINT_TRIM_SAFETY_PX * 2,
            block.height + FOOTPRINT_TRIM_SAFETY_PX * 2
        ),
        "and the trimmed footprint is the same box grown by one safety border"
    );

    // A mark running into the crop's own border: the RAW box touches it, which is the signal,
    // while the grown box touches it for a mark merely NEAR the border too.
    let at_edge = template_with_block((80, 60), PixelRect::new(0, 20, 10, 8), 255, 0, 0);
    let raw = mark_extent_from_template(&at_edge, footprint).expect("the template holds a mark");
    assert_eq!(raw.x, 0, "the raw box reaches the border the mark was cut at");
    // Three pixels in: clear of the level frame the extent rule reads its background from,
    // and still inside the safety border the trimmed footprint adds.
    let near_edge = template_with_block((80, 60), PixelRect::new(3, 20, 10, 8), 255, 0, 0);
    let raw = mark_extent_from_template(&near_edge, footprint).expect("the template holds a mark");
    assert_eq!(raw.x, 3, "a mark merely near the border does not");
    assert_eq!(
        trimmed_footprint_from_template(&near_edge, footprint)
            .expect("the template holds a mark")
            .x,
        0,
        "though its trimmed footprint does, which is why the raw box is the one to test"
    );
}

/// A template holding nothing must be REFUSED, never trimmed to a zero-sized footprint.
#[test]
fn a_template_with_no_mark_is_refused_rather_than_trimmed_to_nothing() {
    let blank = RgbaImage::from_pixel(40, 30, image::Rgba([255, 255, 255, 255]));
    assert_eq!(
        trimmed_footprint_from_template(&blank, PixelRect::new(0, 0, 40, 30)),
        Err(WatermarkError::FlatTemplate {
            width: 40,
            height: 30
        })
    );
    let empty = WatermarkModel::from_parts(
        8,
        6,
        vec![0f32; 8 * 6 * 3],
        vec![1f32; 8 * 6 * 3],
        alpha_blend_operator(),
        test_provenance(),
    )
    .expect("a transparent model is still a valid one");
    assert_eq!(
        trimmed_footprint_from_model(&empty),
        Err(WatermarkError::FlatTemplate {
            width: 8,
            height: 6
        })
    );
}

/// Trimming twice must not erode the footprint: the safety border is wider than the ring the
/// level is measured from, so the second measurement sees the same background and the same
/// extent.
#[test]
fn a_second_trim_of_the_same_material_changes_nothing() {
    let template = template_with_block(
        (319, 236),
        PixelRect::new(182, 110, 127, 34),
        255,
        40,
        2,
    );
    let first = trimmed_footprint_from_template(&template, PixelRect::new(0, 0, 319, 236))
        .expect("mark");
    let cropped = RgbaImage::from_fn(first.width, first.height, |x, y| {
        *template.get_pixel(first.x + x, first.y + y)
    });
    let second =
        trimmed_footprint_from_template(&cropped, PixelRect::new(0, 0, first.width, first.height))
            .expect("mark");
    assert_eq!(
        second,
        PixelRect::new(0, 0, first.width, first.height),
        "a trimmed footprint must already be tight"
    );

    let mark = SyntheticMark::new(24, 16, [0.18; 3]);
    let model = padded_mark((200, 120), (80, 50), &mark).model();
    let trimmed = trimmed_footprint_from_model(&model).expect("mark");
    let (tw, th) = (trimmed.width as usize, trimmed.height as usize);
    let mut c = vec![0f32; tw * th * 3];
    let mut s = vec![1f32; tw * th * 3];
    for y in 0..th {
        for x in 0..tw {
            let from = (((trimmed.y as usize + y) * model.width() as usize)
                + trimmed.x as usize
                + x)
                * 3;
            let to = (y * tw + x) * 3;
            c[to..to + 3].copy_from_slice(&model.c()[from..from + 3]);
            s[to..to + 3].copy_from_slice(&model.s()[from..from + 3]);
        }
    }
    let retrimmed = WatermarkModel::from_parts(
        trimmed.width,
        trimmed.height,
        c,
        s,
        alpha_blend_operator(),
        test_provenance(),
    )
    .expect("cropped planes are in range");
    assert_eq!(
        trimmed_footprint_from_model(&retrimmed),
        Ok(PixelRect::new(0, 0, trimmed.width, trimmed.height))
    );
}

/// THE test a trim has to pass: after it, an occurrence lands at the same ABSOLUTE page
/// pixels as before. A trim moves the footprint's origin, so it moves every anchor by the
/// same amount; get that wrong and a whole chapter is subtracted in the wrong place.
#[test]
fn a_trimmed_template_finds_the_mark_at_the_same_absolute_pixels() {
    let mark = SyntheticMark::new(24, 18, [0.18; 3]);
    let padded = padded_mark((80, 60), (28, 21), &mark);
    // The mark's true position on the page, and the loose box the detector would hand over.
    let mark_at = (120u32, 70u32);
    let loose = PixelRect::new(mark_at.0 - 28, mark_at.1 - 21, 80, 60);
    let mut page = textured_page(300, 200, 150);
    mark.composite(&mut page, PixelRect::new(mark_at.0, mark_at.1, 24, 18), 1.0);

    let loose_kind = {
        let mut kind = WatermarkKind::new(
            "test.trim_anchor",
            MarkTemplate::from_page(&page, loose).expect("template"),
            alpha_blend_operator(),
        );
        kind.template_mut().set_anchors(&[loose.x]).expect("anchors");
        for sample in flat_samples(&padded, padded.rect_at(6, 6)) {
            kind.add_sample(sample).expect("matching geometry");
        }
        kind.refit().expect("separable");
        kind
    };
    let before = find_occurrences(&page, &loose_kind, &DetectionParams::default()).expect("scan");
    assert_eq!(before.len(), 1, "{before:?}");
    assert_eq!((before[0].rect.x, before[0].rect.y), (loose.x, loose.y));

    // Trim: the extent measured off the model, the template re-cut and the anchor moved by
    // exactly the horizontal offset.
    let extent =
        trimmed_footprint_from_model(loose_kind.model().expect("the fit produced a model"))
            .expect("the mark deposits somewhere");
    assert!(extent.width < 80 && extent.height < 60, "{extent:?}");
    let trimmed_rect = PixelRect::new(
        loose.x + extent.x,
        loose.y + extent.y,
        extent.width,
        extent.height,
    );
    let trimmed_padded = {
        let (tw, th) = (extent.width as usize, extent.height as usize);
        let mut c = vec![0f32; tw * th * 3];
        let mut s = vec![1f32; tw * th * 3];
        for y in 0..th {
            for x in 0..tw {
                let from = (((extent.y as usize + y) * 80) + extent.x as usize + x) * 3;
                let to = (y * tw + x) * 3;
                c[to..to + 3].copy_from_slice(&padded.c[from..from + 3]);
                s[to..to + 3].copy_from_slice(&padded.s[from..from + 3]);
            }
        }
        SyntheticMark {
            width: extent.width,
            height: extent.height,
            c,
            s,
        }
    };
    let trimmed_kind = {
        let mut kind = WatermarkKind::new(
            "test.trim_anchor",
            MarkTemplate::from_page(&page, trimmed_rect).expect("template"),
            alpha_blend_operator(),
        );
        kind.template_mut()
            .set_anchors(&[loose.x + extent.x])
            .expect("anchors");
        for sample in flat_samples(&trimmed_padded, trimmed_padded.rect_at(6, 6)) {
            kind.add_sample(sample).expect("matching geometry");
        }
        kind.refit().expect("separable");
        kind
    };
    assert_eq!(
        trimmed_kind.template().anchor_key(),
        (loose.x + extent.x).to_string()
    );

    let after =
        find_occurrences(&page, &trimmed_kind, &DetectionParams::default()).expect("scan");
    assert_eq!(after.len(), 1, "{after:?}");
    // The occurrence moved by exactly the trim offset, so the mark sits at the same absolute
    // pixels under both footprints.
    assert_eq!(after[0].rect.x, before[0].rect.x + extent.x);
    assert_eq!(after[0].rect.y, before[0].rect.y + extent.y);
    assert!(after[0].rect.x <= mark_at.0 && after[0].rect.y <= mark_at.1);
    assert!(after[0].rect.right() >= u64::from(mark_at.0 + 24));
    assert!(after[0].rect.bottom() >= u64::from(mark_at.1 + 18));
    assert!(after[0].is_removal_safe());
}

/// The template rule is SELF-LIMITING: on a crop whose padding is page content rather than
/// flat background, every pixel deviates from the mean, the extent is the whole footprint and
/// nothing is trimmed. An over-tight footprint would corrupt removal; a missed trim costs
/// only crop size.
#[test]
fn a_template_on_a_busy_background_is_not_trimmed_at_all() {
    let busy = RgbaImage::from_fn(80, 60, |x, y| {
        // A gradient plus a checker: no flat level anywhere.
        let value = ((x * 3 + y * 2) % 200 + if (x + y) % 2 == 0 { 30 } else { 0 }) as u8;
        image::Rgba([value, value, value, 255])
    });
    assert_eq!(
        trimmed_footprint_from_template(&busy, PixelRect::new(0, 0, 80, 60)),
        Ok(PixelRect::new(0, 0, 80, 60))
    );
}

fn test_provenance() -> ModelProvenance {
    ModelProvenance {
        partial_rings: 0,
        samples: 2,
        manual_backgrounds: 0,
        method: FitMethod::ClosedFormFlat,
        conditioning: ModelConditioning::Separable {
            levels: vec![0.0, 255.0],
            spread: 255.0,
            min_pixel_spread: 255.0,
            alpha: AlphaUncertainty::from_flat_fit(255.0, 0.18),
        },
        clamped_pixels: 0,
    }
}

fn solid_page(width: u32, height: u32, colour: [u8; 3]) -> RgbaImage {
    RgbaImage::from_pixel(
        width,
        height,
        image::Rgba([colour[0], colour[1], colour[2], 255]),
    )
}

/// Deterministic pseudo-texture so a "busy content" page is reproducible without fixtures.
fn textured_page(width: u32, height: u32, base: u8) -> RgbaImage {
    let mut page = solid_page(width, height, [base, base, base]);
    for (x, y, pixel) in page.enumerate_pixels_mut() {
        let wave = ((x * 7 + y * 13) % 61) as i32 - 30;
        for channel in 0..3 {
            let value = i32::from(pixel[channel]) + wave / 3;
            pixel[channel] = value.clamp(0, 255) as u8;
        }
    }
    page
}

/// Build the two flat samples (over white and over black) that condition a mark.
fn flat_samples(mark: &SyntheticMark, rect: PixelRect) -> Vec<CalibrationSample> {
    let mut white = solid_page(rect.right() as u32 + 8, rect.bottom() as u32 + 8, [255; 3]);
    let mut black = solid_page(rect.right() as u32 + 8, rect.bottom() as u32 + 8, [0; 3]);
    mark.composite(&mut white, rect, 1.0);
    mark.composite(&mut black, rect, 1.0);
    let params = SampleParams::default();
    let (white_verdict, white_sample) =
        calibration_sample_from_page(&white, 0, rect, &params).expect("white sample builds");
    let (black_verdict, black_sample) =
        calibration_sample_from_page(&black, 1, rect, &params).expect("black sample builds");
    assert!(white_verdict.is_calibration(), "white ring must be flat");
    assert!(black_verdict.is_calibration(), "black ring must be flat");
    vec![
        white_sample.expect("flat white sample"),
        black_sample.expect("flat black sample"),
    ]
}

#[test]
fn closed_form_recovers_the_mark_and_removal_is_exact() {
    let mark = SyntheticMark::new(24, 18, [0.18; 3]);
    let rect = mark.rect_at(10, 12);
    let samples = flat_samples(&mark, rect);

    let model = estimate_model(&samples, alpha_blend_operator(), AlphaAssumption::default())
        .expect("well conditioned");
    assert_eq!(model.provenance().method, FitMethod::ClosedFormFlat);
    assert_eq!(model.provenance().clamped_pixels, 0);
    for (index, (&fitted_c, &true_c)) in model.c().iter().zip(mark.c.iter()).enumerate() {
        assert!(
            (fitted_c - true_c).abs() <= 1.0,
            "c[{index}] = {fitted_c}, expected {true_c}"
        );
    }
    for (index, (&fitted_s, &true_s)) in model.s().iter().zip(mark.s.iter()).enumerate() {
        assert!(
            (fitted_s - true_s).abs() <= 0.01,
            "s[{index}] = {fitted_s}, expected {true_s}"
        );
    }

    // Removal over a background level that was never calibrated on.
    let mut page = solid_page(rect.right() as u32 + 8, rect.bottom() as u32 + 8, [128; 3]);
    mark.composite(&mut page, rect, 1.0);
    let patch = remove_occurrence(&page, rect, &model, SubpixelShift::NONE)
        .expect("removal of a valid rect");
    for (index, chunk) in patch.pixels.as_chunks::<4>().0.iter().enumerate() {
        for (channel, &byte) in chunk.iter().take(3).enumerate() {
            let value = i32::from(byte);
            assert!(
                (value - 128).abs() <= 1,
                "pixel {index} channel {channel} recovered as {value}, expected 128"
            );
        }
    }
    assert_eq!(patch.residual.clipped_pixels, 0);
    assert!(patch.residual.exact_share() > 0.99);
}

/// The measured chapter-two case: every calibration sample sits on pure white. The deposit is
/// exact there, so a model IS produced and removal at that level is exact; only the alpha
/// scale is an assumption, and the verdict has to say so and name the fix.
#[test]
fn one_background_level_yields_a_graded_model_with_a_stated_alpha_uncertainty() {
    let mark = SyntheticMark::new(20, 16, [0.18; 3]);
    let rect = mark.rect_at(8, 8);
    let params = SampleParams::default();
    let page_size = (rect.right() as u32 + 8, rect.bottom() as u32 + 8);
    let mut samples = Vec::new();
    for page_index in 0..2 {
        let mut page = solid_page(page_size.0, page_size.1, [255; 3]);
        mark.composite(&mut page, rect, 1.0);
        let (_, sample) = calibration_sample_from_page(&page, page_index, rect, &params)
            .expect("sample builds");
        samples.push(sample.expect("flat sample"));
    }

    let model = estimate_model(&samples, alpha_blend_operator(), AlphaAssumption::default())
        .expect("an exact deposit is enough for a graded model");
    assert_eq!(model.provenance().method, FitMethod::DepositExact);
    let verdict = &model.provenance().conditioning;
    let uncertainty = match verdict {
        ModelConditioning::DepositExact {
            samples, alpha, ..
        } => {
            assert_eq!(*samples, 2);
            assert_eq!(alpha.source, AlphaSource::Assumed);
            *alpha
        }
        other => panic!("expected DepositExact, got {other:?}"),
    };
    assert!(!verdict.is_separable(), "the slope is not data here");
    assert!(verdict.produces_model());
    assert!(
        uncertainty.percent >= ASSUMED_ALPHA_UNCERTAINTY_PERCENT,
        "the assumption must not be quoted as better than it is: {uncertainty:?}"
    );
    assert!(uncertainty.dark_max_lsb > uncertainty.rms_lsb);
    // The observed level is white, so the fix names a DARK background.
    match verdict.suggested_background() {
        Some(SuggestedBackground::Darker { at_most }) => {
            assert!(at_most <= 255.0 - MIN_BACKGROUND_SPREAD);
        }
        other => panic!("expected a request for a darker sample, got {other:?}"),
    }

    // Removal at the calibrated level is EXACT, whatever the alpha scale turns out to be.
    let mut white = solid_page(page_size.0, page_size.1, [255; 3]);
    mark.composite(&mut white, rect, 1.0);
    let patch = remove_occurrence(&white, rect, &model, SubpixelShift::NONE).expect("removal");
    for (index, chunk) in patch.pixels.as_chunks::<4>().0.iter().enumerate() {
        for (channel, &byte) in chunk.iter().take(3).enumerate() {
            assert!(
                i32::from(byte) >= 254,
                "pixel {index} channel {channel} recovered as {byte}, expected white"
            );
        }
    }

    // Far from the calibrated level the assumed alpha shows, and the gain test is what keeps
    // that honest: the occurrence no longer regresses to unity, so detection refuses it
    // instead of removing it with a model that does not describe it.
    let mut dark = solid_page(page_size.0, page_size.1, [10; 3]);
    mark.composite(&mut dark, rect, 1.0);
    let mut dark_kind = WatermarkKind::new(
        "test.single_level.dark",
        MarkTemplate::from_page(&white, rect).expect("template"),
        alpha_blend_operator(),
    );
    for sample in &samples {
        dark_kind.add_sample(sample.clone()).expect("geometry");
    }
    dark_kind.refit().expect("graded fit succeeds");
    let dark_hits = find_occurrences(&dark, &dark_kind, &DetectionParams::default())
        .expect("scan");
    assert!(
        dark_hits.is_empty(),
        "a graded model must not vouch for an occurrence far from its calibration level: \
         {dark_hits:?}"
    );

    // The kind stores the graded verdict together with the model it licensed.
    let template = MarkTemplate::from_page(&white, rect).expect("template");
    let mut kind = WatermarkKind::new(
        "test.single_level",
        template.clone(),
        alpha_blend_operator(),
    );
    for sample in &samples {
        kind.add_sample(sample.clone()).expect("matching geometry");
    }
    kind.refit().expect("graded fit succeeds");
    assert!(kind.model().is_some());
    assert!(!kind.conditioning().is_separable());
    assert!(kind.conditioning().produces_model());
    assert!(kind.conditioning().alpha_uncertainty().is_some());

    // A caller who knows the scale from elsewhere — a persisted model of the same source, a
    // sibling kind — states it, and the engine uses that number and that uncertainty.
    let mut stated = WatermarkKind::new("test.stated", template, alpha_blend_operator());
    stated.set_alpha_assumption(AlphaAssumption::Stated {
        peak_alpha: 0.18,
        uncertainty_percent: 8.0,
    });
    for sample in samples {
        stated.add_sample(sample).expect("matching geometry");
    }
    stated.refit().expect("graded fit succeeds");
    let stated_model = stated.model().expect("model");
    assert!(
        (stated_model.peak_alpha() - 0.18).abs() < 0.01,
        "the stated peak opacity must be the one used: {}",
        stated_model.peak_alpha()
    );
    let stated_uncertainty = stated
        .conditioning()
        .alpha_uncertainty()
        .expect("graded verdict");
    assert!((stated_uncertainty.percent - 8.0).abs() < 0.01);
    assert!(stated_uncertainty.rms_lsb < uncertainty.rms_lsb);
}

/// Alpha measured channel-neutral on both chapters, so per-channel alpha is NOT required by a
/// colour mark — but it stays supported, and a source that does vary per channel must not be
/// collapsed onto one value.
#[test]
fn per_channel_alpha_is_supported_although_measured_marks_are_channel_neutral() {
    let alpha = [0.30f32, 0.10, 0.20];
    let mark = SyntheticMark::new(22, 16, alpha);
    let rect = mark.rect_at(9, 7);
    let samples = flat_samples(&mark, rect);
    let model = estimate_model(&samples, alpha_blend_operator(), AlphaAssumption::default())
        .expect("well conditioned");

    // Pick a pixel inside the glyph body and check the three channels stayed distinct.
    let pixel = (8 * model.width() as usize) + 8;
    let fitted: Vec<f32> = (0..3).map(|ch| model.params_at(pixel, ch).1).collect();
    for channel in 0..3 {
        assert!(
            (fitted[channel] - (1.0 - alpha[channel])).abs() <= 0.01,
            "channel {channel} fitted s = {}, expected {}",
            fitted[channel],
            1.0 - alpha[channel]
        );
    }
    assert!(
        (fitted[0] - fitted[1]).abs() > 0.1 && (fitted[1] - fitted[2]).abs() > 0.05,
        "channels were collapsed: {fitted:?}"
    );
}

#[test]
fn non_uniform_ring_is_refused_for_calibration_but_kept_as_template() {
    let mark = SyntheticMark::new(20, 16, [0.18; 3]);
    let rect = mark.rect_at(12, 12);
    let mut page = solid_page(rect.right() as u32 + 12, rect.bottom() as u32 + 12, [255; 3]);
    // A hard edge crossing the ring: half the surroundings are dark.
    for y in 0..page.height() {
        for x in 0..page.width() {
            if x > rect.x + rect.width / 2 {
                page.put_pixel(x, y, image::Rgba([20, 20, 20, 255]));
            }
        }
    }
    mark.composite(&mut page, rect, 1.0);

    let verdict = validate_calibration_sample(&page, rect, &SampleParams::default());
    assert!(!verdict.is_calibration(), "structured ring must be refused");
    assert!(
        verdict.usable_as_template(),
        "a refused calibration target is still a valid detection template"
    );
    match verdict {
        SampleVerdict::TemplateOnly {
            ring_std,
            std_limit,
            ..
        } => assert!(
            ring_std.iter().any(|&value| value > std_limit),
            "the refusal must report the measured std that caused it: {ring_std:?}"
        ),
        other => panic!("expected TemplateOnly, got {other:?}"),
    }

    // And the calibration helper must not hand back a sample for it.
    let (_, sample) =
        calibration_sample_from_page(&page, 0, rect, &SampleParams::default()).expect("valid rect");
    assert!(sample.is_none());
}

#[test]
fn subpixel_shift_more_than_halves_the_residual() {
    let mark = SyntheticMark::new(24, 20, [0.18; 3]);
    let rect = mark.rect_at(10, 10);
    let model = mark.model();
    let shift = SubpixelShift::new(0.5, 0.5);

    // Compose an occurrence that landed half a pixel off the grid: the model itself is
    // resampled, exactly as a resampled strip would carry it.
    let background = 128u8;
    let mut page = solid_page(
        rect.right() as u32 + 8,
        rect.bottom() as u32 + 8,
        [background; 3],
    );
    for y in 0..rect.height as usize {
        for x in 0..rect.width as usize {
            let pixel = page.get_pixel_mut(rect.x + x as u32, rect.y + y as u32);
            for channel in 0..3 {
                let (c, s) = model.params_bilinear(
                    x as f32 - shift.dx,
                    y as f32 - shift.dy,
                    channel,
                );
                pixel[channel] = (c + s * f32::from(background)).clamp(0.0, 255.0).round() as u8;
            }
        }
    }

    let deviation = |patch: &RegionPatch| -> f32 {
        let mut sum = 0.0f64;
        let mut count = 0usize;
        for chunk in patch.pixels.as_chunks::<4>().0 {
            for &byte in chunk.iter().take(3) {
                let error = f64::from(i32::from(byte) - i32::from(background));
                sum += error * error;
                count += 1;
            }
        }
        (sum / count.max(1) as f64).sqrt() as f32
    };

    let unshifted = remove_occurrence(&page, rect, &model, SubpixelShift::NONE).expect("removal");
    let shifted = remove_occurrence(&page, rect, &model, shift).expect("removal");
    let (before, after) = (deviation(&unshifted), deviation(&shifted));
    assert!(before > 1.0, "the misaligned removal should leave a residual");
    assert!(
        after < before * 0.5,
        "subpixel model shift must cut the residual: {before} -> {after}"
    );
}

#[test]
fn quantization_is_clamped_once_and_only_real_clipping_is_reported() {
    // Hand-built model whose recovery legitimately overshoots the byte range purely because
    // the observation was rounded: c has a fractional part and s halves the range.
    let (width, height) = (8u32, 6u32);
    let pixels = (width as usize) * (height as usize);
    let model = WatermarkModel::from_parts(
        width,
        height,
        vec![10.3; pixels * 3],
        vec![0.5; pixels * 3],
        alpha_blend_operator(),
        test_provenance(),
    )
    .expect("in-range parameters");

    // Two true backgrounds at the extremes of the range.
    let mut page = solid_page(width + 4, height + 4, [0; 3]);
    let rect = PixelRect::new(2, 2, width, height);
    for y in 0..height {
        for x in 0..width {
            let truth = if x.is_multiple_of(2) { 0.0f32 } else { 255.0f32 };
            let observed = (10.3 + 0.5 * truth).round().clamp(0.0, 255.0) as u8;
            page.put_pixel(rect.x + x, rect.y + y, image::Rgba([observed; 4]));
        }
    }

    let patch = remove_occurrence(&page, rect, &model, SubpixelShift::NONE).expect("removal");
    for (index, chunk) in patch.pixels.as_chunks::<4>().0.iter().enumerate() {
        let x = index % width as usize;
        let expected = if x.is_multiple_of(2) { 0u8 } else { 255u8 };
        for (channel, &byte) in chunk.iter().take(3).enumerate() {
            assert_eq!(
                byte, expected,
                "pixel {index} channel {channel} lost its value to intermediate clamping"
            );
        }
    }
    assert_eq!(
        patch.residual.clipped_pixels, 0,
        "an excursion explainable by one quantization step is not clipping"
    );
    assert!(patch.residual.rms().iter().all(|&value| value <= 0.5));

    // A genuinely irrecoverable pixel: the observation cannot come from any in-range
    // background under this model.
    let mut saturated = solid_page(width + 4, height + 4, [250; 3]);
    for y in 0..height {
        for x in 0..width {
            saturated.put_pixel(rect.x + x, rect.y + y, image::Rgba([250, 250, 250, 255]));
        }
    }
    let clipped =
        remove_occurrence(&saturated, rect, &model, SubpixelShift::NONE).expect("removal");
    assert_eq!(clipped.residual.clipped_pixels, clipped.residual.mark_pixels);
    assert!(clipped.residual.clipped_share() > 0.99);
}

#[test]
fn detection_rejects_a_shape_alike_with_the_wrong_amplitude() {
    let mark = SyntheticMark::new(24, 18, [0.18; 3]);
    let model = mark.model();
    let anchor = 60u32;
    let true_rect = mark.rect_at(anchor, 30);

    let mut page = textured_page(140, 260, 150);
    mark.composite(&mut page, true_rect, 1.0);
    // Same glyph, twice the alpha: correlation is scale invariant and scores it 1.0.
    mark.composite(&mut page, mark.rect_at(anchor, 120), 2.0);
    // An exact copy far off the anchor column.
    mark.composite(&mut page, mark.rect_at(10, 200), 1.0);

    let template_page = {
        let mut clean = solid_page(140, 260, [150; 3]);
        mark.composite(&mut clean, true_rect, 1.0);
        clean
    };
    let template = MarkTemplate::from_page(&template_page, true_rect).expect("template");
    let mut kind = WatermarkKind::new("test.mark", template, alpha_blend_operator());
    for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
        kind.add_sample(sample).expect("matching geometry");
    }
    kind.refit().expect("two separated levels");

    let found = find_occurrences(&page, &kind, &DetectionParams::default()).expect("scan");
    assert_eq!(
        found.len(),
        1,
        "exactly the true occurrence must survive, got {found:?}"
    );
    let hit = &found[0];
    assert_eq!(hit.rect, true_rect);
    assert!(hit.is_removal_safe());
    match hit.evidence {
        AcceptanceEvidence::Gain { gain, snr } => {
            assert!((gain - 1.0).abs() < 0.1, "gain {gain} should be near unity");
            assert!(snr >= MIN_DETECTION_T);
        }
        AcceptanceEvidence::Correlation => panic!("a kind with a model must use the gain test"),
    }

    // Without a model the same page can only produce correlation-only accepts, and those are
    // explicitly not removal-safe.
    let mut blind = WatermarkKind::new(
        "test.blind",
        MarkTemplate::from_page(&template_page, true_rect).expect("template"),
        alpha_blend_operator(),
    );
    assert!(blind.refit().is_err());
    let blind_hits = find_occurrences(&page, &blind, &DetectionParams::default()).expect("scan");
    assert!(blind_hits.iter().all(|hit| !hit.is_removal_safe()));
    assert!(
        blind_hits.len() >= 2,
        "correlation alone cannot tell the look-alike apart: {blind_hits:?}"
    );
    let error = remove_occurrences_on_page(&page, &model, &blind_hits)
        .expect_err("unverified occurrences must be refused");
    assert!(matches!(error, WatermarkError::UnverifiedOccurrence { .. }));
}

#[test]
fn overlapping_kinds_are_resolved_by_score() {
    let mark = SyntheticMark::new(24, 18, [0.18; 3]);
    let anchor = 50u32;
    let true_rect = mark.rect_at(anchor, 40);
    let mut page = textured_page(120, 140, 150);
    mark.composite(&mut page, true_rect, 1.0);

    let template_page = {
        let mut clean = solid_page(120, 140, [150; 3]);
        mark.composite(&mut clean, true_rect, 1.0);
        clean
    };
    let template = MarkTemplate::from_page(&template_page, true_rect).expect("template");

    let mut exact = WatermarkKind::new("test.exact", template.clone(), alpha_blend_operator());
    for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
        exact.add_sample(sample).expect("matching geometry");
    }
    exact.refit().expect("separable");

    // A second kind claiming the same spot with a slightly wrong alpha: still inside the gain
    // window, but a worse fit, so it must lose the arbitration instead of both being applied.
    let mut approximate =
        WatermarkKind::new("test.approximate", template, alpha_blend_operator());
    let weak = SyntheticMark::new(24, 18, [0.18 * 0.92; 3]);
    for sample in flat_samples(&weak, weak.rect_at(4, 4)) {
        approximate.add_sample(sample).expect("matching geometry");
    }
    approximate.refit().expect("separable");

    let catalog = [exact, approximate];
    let hits = scan_page(&page, 3, &catalog, &DetectionParams::default()).expect("scan");
    assert_eq!(hits.len(), 1, "overlapping kinds must not both apply: {hits:?}");
    assert_eq!(hits[0].kind_index, 0, "the better-fitting kind must win");
    assert_eq!(hits[0].page_index, 3);
}

#[test]
fn scan_chapter_is_deterministic_and_page_ordered() {
    let mark = SyntheticMark::new(24, 18, [0.18; 3]);
    let anchor = 40u32;
    let rect = mark.rect_at(anchor, 30);
    let mut first = textured_page(100, 120, 150);
    mark.composite(&mut first, rect, 1.0);
    let mut second = textured_page(100, 120, 90);
    mark.composite(&mut second, mark.rect_at(anchor, 60), 1.0);

    let template_page = {
        let mut clean = solid_page(100, 120, [150; 3]);
        mark.composite(&mut clean, rect, 1.0);
        clean
    };
    let mut kind = WatermarkKind::new(
        "test.chapter",
        MarkTemplate::from_page(&template_page, rect).expect("template"),
        alpha_blend_operator(),
    );
    for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
        kind.add_sample(sample).expect("matching geometry");
    }
    kind.refit().expect("separable");

    let pages = [&first, &second];
    let catalog = [kind];
    let params = DetectionParams::default();
    let first_run = scan_chapter(&pages, &catalog, &params).expect("scan");
    let second_run = scan_chapter(&pages, &catalog, &params).expect("scan");
    assert_eq!(first_run, second_run, "the scan must be deterministic");
    assert_eq!(first_run.len(), 2);
    assert_eq!(first_run[0].page_index, 0);
    assert_eq!(first_run[1].page_index, 1);
}

#[test]
fn public_entry_points_reject_bad_geometry_without_panicking() {
    let mark = SyntheticMark::new(16, 12, [0.18; 3]);
    let model = mark.model();
    let page = solid_page(40, 30, [128; 3]);

    let outside = PixelRect::new(30, 25, 16, 12);
    assert!(matches!(
        remove_occurrence(&page, outside, &model, SubpixelShift::NONE),
        Err(WatermarkError::RectOutOfPage { .. })
    ));
    assert!(matches!(
        remove_occurrence(&page, PixelRect::new(0, 0, 0, 12), &model, SubpixelShift::NONE),
        Err(WatermarkError::EmptyRect { .. })
    ));
    assert!(matches!(
        remove_occurrence(&page, PixelRect::new(0, 0, 8, 8), &model, SubpixelShift::NONE),
        Err(WatermarkError::GeometryMismatch { .. })
    ));
    assert!(matches!(
        MarkTemplate::from_page(&page, PixelRect::new(0, 0, 8, 8)),
        Err(WatermarkError::FlatTemplate { .. })
    ));
    assert!(matches!(
        estimate_model(&[], alpha_blend_operator(), AlphaAssumption::default()),
        Err(ModelFitError::Invalid(WatermarkError::NoSamples))
    ));
    assert!(matches!(
        WatermarkModel::from_parts(
            4,
            4,
            vec![0.0; 3],
            vec![1.0; 48],
            alpha_blend_operator(),
            test_provenance()
        ),
        Err(WatermarkError::BufferLength { .. })
    ));
    assert!(matches!(
        WatermarkModel::from_parts(
            2,
            2,
            vec![0.0; 12],
            vec![0.0; 12],
            alpha_blend_operator(),
            test_provenance()
        ),
        Err(WatermarkError::ParameterOutOfRange { what: "s", .. })
    ));
    assert!(matches!(
        validate_calibration_sample(&page, outside, &SampleParams::default()),
        SampleVerdict::Unusable {
            reason: SampleRejection::BadRect
        }
    ));
}

#[test]
fn theil_sen_path_fits_from_estimated_backgrounds() {
    let mark = SyntheticMark::new(24, 18, [0.18; 3]);
    let rect = mark.rect_at(10, 10);
    let bootstrap = mark.model();
    let params = DetectionParams::default();

    // Three occurrences over well-separated levels, with the background supplied per pixel by
    // the engine's own estimator instead of a flat-ring reading. No sample is `Flat`, so the
    // robust path has to carry the fit.
    let mut samples = Vec::new();
    for (page_index, level) in [20u8, 128, 240].into_iter().enumerate() {
        let mut page = solid_page(
            rect.right() as u32 + 12,
            rect.bottom() as u32 + 12,
            [level; 3],
        );
        mark.composite(&mut page, rect, 1.0);
        let estimated = provisional_background(&page, rect, &bootstrap, &params)
            .expect("background estimate");
        for (index, &value) in estimated.iter().enumerate() {
            assert!(
                (value - f32::from(level)).abs() <= 3.0,
                "estimated background[{index}] = {value}, expected ~{level}"
            );
        }
        samples.push(
            CalibrationSample::from_page(
                &page,
                page_index,
                rect,
                SampleBackground::Estimated { values: estimated },
            )
            .expect("sample builds"),
        );
    }

    let model = estimate_model(&samples, alpha_blend_operator(), AlphaAssumption::default())
        .expect("well conditioned");
    assert_eq!(model.provenance().method, FitMethod::TheilSen);
    // A slope fitted against ESTIMATES is never quoted as well as one fitted against exact
    // backgrounds: the plan measured that path at +-10-20% and the verdict must say so.
    let uncertainty = model
        .provenance()
        .conditioning
        .alpha_uncertainty()
        .expect("a model always carries one");
    assert_eq!(uncertainty.source, AlphaSource::EstimatedBackgrounds);
    assert!(
        (uncertainty.percent - ESTIMATED_BACKGROUND_ALPHA_UNCERTAINTY_PERCENT).abs() < 0.01
    );
    for (index, (&fitted_s, &true_s)) in model.s().iter().zip(mark.s.iter()).enumerate() {
        assert!(
            (fitted_s - true_s).abs() <= 0.05,
            "s[{index}] = {fitted_s}, expected {true_s}"
        );
    }
    for (index, (&fitted_c, &true_c)) in model.c().iter().zip(mark.c.iter()).enumerate() {
        assert!(
            (fitted_c - true_c).abs() <= 8.0,
            "c[{index}] = {fitted_c}, expected {true_c}"
        );
    }
}

/// The refinement loop runs a fixed, named number of iterations because nothing in the data
/// selects a stopping point, and it never downgrades an exactly measured background.
#[test]
fn background_refinement_iterates_a_fixed_number_of_times_and_keeps_exact_samples() {
    let mark = SyntheticMark::ring(24, 18, [0.18; 3]);
    let rect = mark.rect_at(10, 10);
    let (page_w, page_h) = (rect.right() as u32 + 12, rect.bottom() as u32 + 12);
    let template_page = {
        let mut clean = solid_page(page_w, page_h, [150; 3]);
        mark.composite(&mut clean, rect, 1.0);
        clean
    };
    let mut kind = WatermarkKind::new(
        "test.refine",
        MarkTemplate::from_page(&template_page, rect).expect("template"),
        alpha_blend_operator(),
    );
    for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
        kind.add_sample(sample).expect("matching geometry");
    }
    kind.refit().expect("separable");

    // With only exactly measured backgrounds there is nothing to refine, and replacing them
    // with estimates would throw away the only hard evidence in the set.
    let pages: Vec<RgbaImage> = Vec::new();
    let page_refs: Vec<&RgbaImage> = pages.iter().collect();
    assert!(matches!(
        refit_with_refined_backgrounds(&page_refs, &mut kind, &DetectionParams::default()),
        Err(ModelFitError::Invalid(WatermarkError::NothingToRefine))
    ));

    // Add occurrences over busy content, with backgrounds the model itself estimated.
    let bootstrap = kind.model().expect("fitted").clone();
    let params = DetectionParams::default();
    let mut busy_pages = Vec::new();
    for base in [40u8, 200] {
        let mut page = textured_page(page_w, page_h, base);
        mark.composite(&mut page, rect, 1.0);
        busy_pages.push(page);
    }
    for (page_index, page) in busy_pages.iter().enumerate() {
        let values =
            provisional_background(page, rect, &bootstrap, &params).expect("estimate");
        kind.add_sample(
            CalibrationSample::from_page(
                page,
                page_index,
                rect,
                SampleBackground::Estimated { values },
            )
            .expect("sample builds"),
        )
        .expect("matching geometry");
    }

    let refs: Vec<&RgbaImage> = busy_pages.iter().collect();
    refit_with_refined_backgrounds(&refs, &mut kind, &params).expect("refinement");
    let model = kind.model().expect("refinement keeps a model");
    for (index, (&fitted_s, &true_s)) in model.s().iter().zip(mark.s.iter()).enumerate() {
        assert!(
            (fitted_s - true_s).abs() <= 0.1,
            "s[{index}] = {fitted_s}, expected {true_s}"
        );
    }
    assert!(
        kind.samples().iter().filter(|s| s.is_flat()).count() == 2,
        "the exactly measured samples must survive refinement"
    );
}

/// A background level the USER asserted is not a measurement, but the refinement loop must
/// leave it alone all the same: overwriting it with an estimate would silently replace an
/// instruction with a guess, and the loop is what would do it every iteration.
#[test]
fn refinement_never_overwrites_a_hand_asserted_level() {
    let mark = SyntheticMark::new(16, 12, [0.18; 3]);
    let rect = mark.rect_at(6, 6);
    let template_page = {
        let mut page = solid_page(rect.right() as u32 + 8, rect.bottom() as u32 + 8, [150; 3]);
        mark.composite(&mut page, rect, 1.0);
        page
    };
    let mut kind = WatermarkKind::new(
        "test.manual-refine",
        MarkTemplate::from_page(&template_page, rect).expect("template"),
        alpha_blend_operator(),
    );
    for (page_index, level) in [0u8, 255].into_iter().enumerate() {
        let mut page =
            solid_page(rect.right() as u32 + 8, rect.bottom() as u32 + 8, [level; 3]);
        mark.composite(&mut page, rect, 1.0);
        kind.add_sample(
            CalibrationSample::from_page(
                &page,
                page_index,
                rect,
                SampleBackground::Manual {
                    level: [f32::from(level); 3],
                },
            )
            .expect("sample builds"),
        )
        .expect("matching geometry");
    }
    kind.refit().expect("two asserted levels still fit");
    assert_eq!(kind.model().expect("model").provenance().manual_backgrounds, 2);

    // Nothing here is refinable: every sample already states its own `B`.
    let pages: Vec<&RgbaImage> = Vec::new();
    assert!(matches!(
        refit_with_refined_backgrounds(&pages, &mut kind, &DetectionParams::default()),
        Err(ModelFitError::Invalid(WatermarkError::NothingToRefine))
    ));
    assert!(
        kind.samples().iter().all(CalibrationSample::is_manual),
        "an asserted level must survive the refinement path untouched"
    );
}

#[test]
fn a_pixel_that_never_saw_two_backgrounds_is_reported_as_underdetermined() {
    let mark = SyntheticMark::new(16, 12, [0.18; 3]);
    let rect = mark.rect_at(6, 6);
    let pixels = (rect.width as usize) * (rect.height as usize);
    let mut samples = Vec::new();
    for (page_index, level) in [10u8, 250].into_iter().enumerate() {
        let mut page = solid_page(
            rect.right() as u32 + 8,
            rect.bottom() as u32 + 8,
            [level; 3],
        );
        mark.composite(&mut page, rect, 1.0);
        let mut values = vec![f32::from(level); pixels * 3];
        // One pixel is pinned to the same background in every sample: its `c`/`s` would be a
        // guess, and the engine must say so instead of fitting it.
        values[..3].fill(64.0);
        samples.push(
            CalibrationSample::from_page(
                &page,
                page_index,
                rect,
                SampleBackground::Estimated { values },
            )
            .expect("sample builds"),
        );
    }

    let error = estimate_model(&samples, alpha_blend_operator(), AlphaAssumption::default())
        .expect_err("one underdetermined pixel blocks the fit");
    match error.conditioning() {
        Some(ModelConditioning::Underdetermined {
            underdetermined_pixels,
            total_pixels,
            ..
        }) => {
            assert_eq!(*underdetermined_pixels, 1);
            assert_eq!(*total_pixels, pixels);
        }
        other => panic!("expected Underdetermined, got {other:?}"),
    }
}

#[test]
fn conditioning_names_the_missing_background() {
    let assumed = AlphaUncertainty::from_percent(
        AlphaSource::Assumed,
        ASSUMED_ALPHA_UNCERTAINTY_PERCENT,
    );
    let dark = ModelConditioning::DepositExact {
        levels: vec![12.0],
        spread: 1.0,
        samples: 3,
        alpha: assumed,
    };
    assert!(matches!(
        dark.suggested_background(),
        Some(SuggestedBackground::Brighter { .. })
    ));
    let bright = ModelConditioning::DepositExact {
        levels: vec![240.0],
        spread: 1.0,
        samples: 3,
        alpha: assumed,
    };
    match bright.suggested_background() {
        Some(SuggestedBackground::Darker { at_most }) => {
            assert!((at_most - (240.0 - MIN_BACKGROUND_SPREAD)).abs() < 0.01);
        }
        other => panic!("expected a request for a darker sample, got {other:?}"),
    }
    assert!(
        ModelConditioning::Separable {
            levels: vec![0.0, 255.0],
            spread: 255.0,
            min_pixel_spread: 255.0,
            alpha: AlphaUncertainty::from_flat_fit(255.0, 0.18),
        }
        .suggested_background()
        .is_none()
    );
}

/// The measured chapter-two case a template-keyed catalog gets wrong: a colour mark and a
/// greyscale one sharing the same artwork. Here they literally share the template, so only the
/// deposit can tell them apart — and it must, in both directions.
#[test]
fn identical_templates_with_different_deposits_stay_two_marks() {
    // Deposit chroma ~127 LSB on white; the measured colour mark reaches 120.
    let colour = SyntheticMark::with_colours(
        24,
        18,
        [0.50; 3],
        [255.0, 30.0, 0.0],
        [12.0, 14.0, 13.0],
    );
    // Achromatic twin depositing ~0.42 of the colour one — the measured bimodal opacity gain
    // (pale 0.365-0.515 against the colour model).
    let pale = SyntheticMark::with_colours(
        24,
        18,
        [0.34; 3],
        [128.0, 128.0, 128.0],
        [13.0, 13.0, 13.0],
    );

    let anchor = 40u32;
    let colour_rect = colour.rect_at(anchor, 20);
    let pale_rect = pale.rect_at(anchor, 80);
    let mut page = textured_page(120, 140, 150);
    colour.composite(&mut page, colour_rect, 1.0);
    pale.composite(&mut page, pale_rect, 1.0);

    // ONE template, cloned into both kinds: shape cannot possibly tell them apart.
    let template_page = {
        let mut clean = solid_page(120, 140, [150; 3]);
        colour.composite(&mut clean, colour_rect, 1.0);
        clean
    };
    let template = MarkTemplate::from_page(&template_page, colour_rect).expect("template");
    let mut colour_kind =
        WatermarkKind::new("test.colour", template.clone(), alpha_blend_operator());
    for sample in flat_samples(&colour, colour.rect_at(4, 4)) {
        colour_kind.add_sample(sample).expect("matching geometry");
    }
    colour_kind.refit().expect("separable");

    // Identity has to work at PICK time as well, before any fit: a flat sample measures the
    // deposit, and that is all the discriminator needs.
    let mut unfitted =
        WatermarkKind::new("test.unfitted", template.clone(), alpha_blend_operator());
    for sample in flat_samples(&pale, pale.rect_at(4, 4)) {
        unfitted.add_sample(sample).expect("matching geometry");
    }
    assert!(unfitted.model().is_none());
    let unfitted_signature = unfitted
        .signature()
        .expect("a flat sample is enough for an identity");
    assert!(unfitted_signature.is_achromatic());

    let mut pale_kind = WatermarkKind::new("test.pale", template, alpha_blend_operator());
    for sample in flat_samples(&pale, pale.rect_at(4, 4)) {
        pale_kind.add_sample(sample).expect("matching geometry");
    }
    pale_kind.refit().expect("separable");

    let colour_signature = colour_kind.signature().expect("a fitted kind has an identity");
    let pale_signature = pale_kind.signature().expect("a fitted kind has an identity");
    assert!(
        !colour_signature.is_achromatic(),
        "colour deposit chroma {} should exceed the achromatic limit",
        colour_signature.deposit_chroma
    );
    assert!(
        pale_signature.is_achromatic(),
        "greyscale deposit chroma {} should be under the achromatic limit",
        pale_signature.deposit_chroma
    );
    let gain = pale_signature
        .opacity_gain_against(&colour_signature)
        .expect("both deposits are measured against white");
    assert!(
        (0.30..0.60).contains(&gain),
        "expected the measured bimodal gain around 0.42, got {gain}"
    );
    assert!(!colour_kind.is_same_mark_as(&pale_kind));
    assert!(!pale_kind.is_same_mark_as(&colour_kind));
    assert!(colour_signature.is_same_mark_as(&colour_signature));
    // The sample-measured identity agrees with the fitted one, and still refuses the twin.
    assert!(unfitted_signature.is_same_mark_as(&pale_signature));
    assert!(!unfitted_signature.is_same_mark_as(&colour_signature));

    // A catalog lookup keyed on the signature keeps them apart; a shape-keyed one could not.
    let catalog = [colour_kind, pale_kind];
    assert_eq!(find_matching_kind(&catalog, &colour_signature), Some(0));
    assert_eq!(find_matching_kind(&catalog, &pale_signature), Some(1));

    // And each occurrence is attributed to its own kind, never removed with the other model.
    let hits = scan_page(&page, 0, &catalog, &DetectionParams::default()).expect("scan");
    assert_eq!(hits.len(), 2, "both marks must be found: {hits:?}");
    for hit in &hits {
        let expected_kind = if hit.occurrence.rect == colour_rect {
            0
        } else {
            assert_eq!(hit.occurrence.rect, pale_rect, "unexpected hit {hit:?}");
            1
        };
        assert_eq!(
            hit.kind_index, expected_kind,
            "occurrence matched to the wrong kind: {hit:?}"
        );
        assert!(hit.occurrence.is_removal_safe());
    }
}

/// The anchor set is DATA: chapter two stamps at three columns, chapter one at one. Detection
/// must accept at every column of the set and refuse the same mark anywhere else.
#[test]
fn an_anchor_set_accepts_at_every_column_and_rejects_off_anchor() {
    let mark = SyntheticMark::new(24, 18, [0.18; 3]);
    let anchors = [20u32, 60, 100];
    let mut page = textured_page(140, 200, 150);
    for (index, &anchor) in anchors.iter().enumerate() {
        let y = 20 + 40 * u32::try_from(index).expect("three anchors");
        mark.composite(&mut page, mark.rect_at(anchor, y), 1.0);
    }
    // The same mark at full amplitude, far from every anchor column.
    let off_anchor = mark.rect_at(45, 160);
    mark.composite(&mut page, off_anchor, 1.0);

    let template_rect = mark.rect_at(anchors[0], 20);
    let template_page = {
        let mut clean = solid_page(140, 200, [150; 3]);
        mark.composite(&mut clean, template_rect, 1.0);
        clean
    };
    let mut kind = WatermarkKind::new(
        "test.anchor_set",
        MarkTemplate::from_page(&template_page, template_rect).expect("template"),
        alpha_blend_operator(),
    );
    for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
        kind.add_sample(sample).expect("matching geometry");
    }
    kind.refit().expect("separable");
    assert_eq!(
        kind.template().anchors(),
        &[anchors[0]],
        "a picked sample knows only its own column"
    );
    kind.template_mut()
        .set_anchors(&anchors)
        .expect("non-empty set");
    assert_eq!(kind.template().anchor_key(), "20,60,100");

    let found = find_occurrences(&page, &kind, &DetectionParams::default()).expect("scan");
    assert_eq!(found.len(), 3, "every anchor column must be scanned: {found:?}");
    for (occurrence, &anchor) in found.iter().zip(anchors.iter()) {
        assert_eq!(occurrence.rect.x, anchor);
        assert!(occurrence.is_removal_safe());
    }
    assert!(
        found.iter().all(|hit| hit.rect != off_anchor),
        "an off-anchor occurrence must not be accepted: {found:?}"
    );
    assert!(matches!(
        kind.template_mut().set_anchors(&[]),
        Err(WatermarkError::EmptyAnchorSet)
    ));
}

/// The two guards chapter two measured: 9 of 147 candidates were false, all off-anchor and/or
/// under a gain of 0.35, and removing at one injects an inverse mark into content.
#[test]
fn the_anchor_and_gain_guards_reject_what_the_looser_rule_accepted() {
    let mark = SyntheticMark::ring(24, 18, [0.18; 3]);
    let anchor = 50u32;
    let mut page = textured_page(140, 220, 150);
    let true_rect = mark.rect_at(anchor, 20);
    mark.composite(&mut page, true_rect, 1.0);
    // 3 px off the anchor column: inside the old +-3 band, outside the measured +-2 one.
    let off_anchor = mark.rect_at(anchor + 3, 80);
    mark.composite(&mut page, off_anchor, 1.0);
    // On the anchor at a fifth of the amplitude: a gain of ~0.2, under the measured floor.
    let weak = mark.rect_at(anchor, 140);
    mark.composite(&mut page, weak, 0.2);

    let template_page = {
        let mut clean = solid_page(140, 220, [150; 3]);
        mark.composite(&mut clean, true_rect, 1.0);
        clean
    };
    let mut kind = WatermarkKind::new(
        "test.guards",
        MarkTemplate::from_page(&template_page, true_rect).expect("template"),
        alpha_blend_operator(),
    );
    for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
        kind.add_sample(sample).expect("matching geometry");
    }
    kind.refit().expect("separable");

    let found = find_occurrences(&page, &kind, &DetectionParams::default()).expect("scan");
    assert_eq!(found.len(), 1, "only the true occurrence survives: {found:?}");
    assert_eq!(found[0].rect, true_rect);

    // The looser band the engine used before this measurement accepted the off-anchor one.
    let loose = DetectionParams {
        anchor_tolerance: 3,
        ..DetectionParams::default()
    };
    let loose_found = find_occurrences(&page, &kind, &loose).expect("scan");
    assert!(
        loose_found.iter().any(|hit| hit.rect == off_anchor),
        "the +-3 px band is what let the measured false accepts through: {loose_found:?}"
    );

    // No configuration may lower the gain floor under the measured 0.35, so the fifth-
    // amplitude look-alike stays rejected even with a hand-widened window.
    let wide = DetectionParams {
        gain_min: 0.05,
        gain_max: 1.5,
        anchor_tolerance: 3,
        ..DetectionParams::default()
    };
    assert!((wide.normalized().gain_min - FALSE_ACCEPT_GAIN_FLOOR).abs() < f32::EPSILON);
    let wide_found = find_occurrences(&page, &kind, &wide).expect("scan");
    assert!(
        wide_found.iter().all(|hit| hit.rect != weak),
        "a gain of ~0.2 must never be accepted: {wide_found:?}"
    );
}

/// The anchor set has to come from the data — nobody knows the other columns from the one
/// occurrence the user pointed at.
#[test]
fn anchor_discovery_finds_every_stamped_column() {
    let mark = SyntheticMark::new(24, 18, [0.18; 3]);
    let columns = [12u32, 60, 104];
    let mut page = textured_page(140, 260, 150);
    let mut stamped = Vec::new();
    for (index, &column) in columns.iter().enumerate() {
        let offset = 32 * u32::try_from(index).expect("three columns");
        for base in [20u32, 160] {
            let rect = mark.rect_at(column, base + offset);
            mark.composite(&mut page, rect, 1.0);
            stamped.push(rect);
        }
    }

    let template_rect = mark.rect_at(columns[0], 20);
    let template_page = {
        let mut clean = solid_page(140, 260, [150; 3]);
        mark.composite(&mut clean, template_rect, 1.0);
        clean
    };
    let mut kind = WatermarkKind::new(
        "test.discovery",
        MarkTemplate::from_page(&template_page, template_rect).expect("template"),
        alpha_blend_operator(),
    );
    for sample in flat_samples(&mark, mark.rect_at(4, 4)) {
        kind.add_sample(sample).expect("matching geometry");
    }
    kind.refit().expect("separable");

    let params = DetectionParams::default();
    let discovered = discover_anchors(&[&page], kind.template(), &params).expect("discovery");
    for &column in &columns {
        assert!(
            discovered
                .iter()
                .any(|&found| found.abs_diff(column) <= ANCHOR_CLUSTER_RADIUS_PX),
            "column {column} was stamped twice and must be discovered: {discovered:?}"
        );
    }

    // Installing the discovered set finds every occurrence, and the gain test is what keeps a
    // spurious column from turning into a spurious removal.
    kind.template_mut()
        .set_anchors(&discovered)
        .expect("non-empty set");
    let found = find_occurrences(&page, &kind, &params).expect("scan");
    assert_eq!(found.len(), stamped.len(), "{found:?}");
    for rect in stamped {
        assert!(found.iter().any(|hit| hit.rect == rect), "missing {rect:?}");
    }
}

/// The alpha-uncertainty figures are the MEASURED calibration, not a re-derivation: +-10% on
/// the alpha scale costs 3.2 LSB rms overall and 4.7 LSB (worst 13) on backgrounds under
/// luma 80. A change to the constants must show up here.
#[test]
fn reported_uncertainty_matches_the_measured_alpha_calibration() {
    let ten = AlphaUncertainty::from_percent(AlphaSource::Assumed, 10.0);
    assert!((ten.rms_lsb - 3.2).abs() < 0.05, "{ten:?}");
    assert!((ten.dark_rms_lsb - 4.7).abs() < 0.05, "{ten:?}");
    assert!((ten.dark_max_lsb - 13.0).abs() < 0.05, "{ten:?}");
    assert!((ten.dark_luma - DARK_BACKGROUND_LUMA).abs() < f32::EPSILON);

    // Linear in the alpha error, as `delta_alpha * (B - B0) / (1 - alpha)` requires.
    let five = AlphaUncertainty::from_percent(AlphaSource::Assumed, 5.0);
    let twenty = AlphaUncertainty::from_percent(AlphaSource::Assumed, 20.0);
    assert!((five.rms_lsb - 1.6).abs() < 0.05, "{five:?}");
    assert!((twenty.rms_lsb - 6.4).abs() < 0.05, "{twenty:?}");

    // A fitted slope over well-separated exact backgrounds is worth far more than an
    // assumption, and the measured 2.3-2.6 LSB leave-one-out residual is what it predicts.
    let fitted = AlphaUncertainty::from_flat_fit(255.0, 0.183);
    assert_eq!(fitted.source, AlphaSource::SeparatedBackgrounds);
    assert!(
        fitted.percent < ASSUMED_ALPHA_UNCERTAINTY_PERCENT,
        "a measured slope must beat an assumed one: {fitted:?}"
    );
    assert!(
        (2.0..3.0).contains(&fitted.rms_lsb),
        "expected the measured ~2.5 LSB floor, got {fitted:?}"
    );
    // A degenerate spread cannot be quoted as certain.
    assert!((AlphaUncertainty::from_flat_fit(0.0, 0.18).percent - 100.0).abs() < 0.01);
}

// -----------------------------------------------------------------------------------
// Partial rings: a mark stamped against the page border
// -----------------------------------------------------------------------------------

/// A page with a flat background and one semi-transparent mark at `(x, y)`.
///
/// The mark is a deliberately structured block — the ring must never be able to read it —
/// so a footprint placed exactly on it makes the ring the pure background.
fn page_with_mark(
    width: u32,
    height: u32,
    background: [u8; 3],
    mark: PixelRect,
) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, y| {
        let inside = x >= mark.x
            && y >= mark.y
            && u64::from(x) < mark.right()
            && u64::from(y) < mark.bottom();
        if !inside {
            return image::Rgba([background[0], background[1], background[2], 255]);
        }
        // Cast justification: synthetic coordinates of a page bounded by the literals the
        // callers pass, far inside u8 after the modulo.
        let shade = ((x * 7 + y * 13) % 200) as u8;
        image::Rgba([shade, shade, shade, 255])
    })
}

/// The user's own geometry: a 319x236 mark whose left edge sits at x=480 on an 800-px page,
/// so only ONE pixel of background is left to its right while the ring wants three.
///
/// This used to be unmeasurable through the library capture lane, which demanded the full
/// ring on every side. The measurement itself never did: it clips and judges the result on
/// a pixel count, and that is what this pins.
#[test]
fn a_mark_flush_against_the_page_edge_still_measures_and_says_the_ring_was_partial() {
    let mark = PixelRect::new(480, 200, 319, 236);
    let page = page_with_mark(800, 1200, [240, 240, 240], mark);
    let verdict = validate_calibration_sample(&page, mark, &SampleParams::default());
    let SampleVerdict::Calibration {
        level,
        ring_std,
        ring,
    } = verdict
    else {
        panic!("a flat background one pixel wide is still a flat background: {verdict:?}");
    };
    assert!((level[0] - 240.0).abs() < 0.01, "level {level:?}");
    assert!(ring_std.iter().all(|&v| v < 0.01), "a uniform ring has no spread");
    assert!(ring.is_partial(), "the page edge took pixels: {ring:?}");
    assert!(
        ring.pixels >= SampleParams::default().min_ring_pixels,
        "the surviving ring is what admits the sample: {ring:?}"
    );
    assert!(
        ring.pixels < ring.full_pixels,
        "a truncated ring must not report itself complete: {ring:?}"
    );
    assert!(ring.share() < 1.0 && ring.share() > 0.5, "{:?}", ring.share());
}

/// The same mark away from every border is UNCHANGED by the partial-ring work: the same
/// level, the same verdict, and a coverage that says the ring was complete. This is the
/// regression pin — the measurement path every existing entry was written through.
#[test]
fn a_full_ring_sample_is_untouched_and_reports_complete_coverage() {
    let mark = PixelRect::new(100, 200, 319, 236);
    let page = page_with_mark(800, 1200, [240, 240, 240], mark);
    let verdict = validate_calibration_sample(&page, mark, &SampleParams::default());
    let SampleVerdict::Calibration {
        level,
        ring_std,
        ring,
    } = verdict
    else {
        panic!("a flat ring all the way round is the ordinary accepted case: {verdict:?}");
    };
    assert!((level[0] - 240.0).abs() < 0.01);
    assert!(ring_std.iter().all(|&v| v < 0.01));
    assert!(!ring.is_partial(), "nothing clipped this ring: {ring:?}");
    assert_eq!(ring.pixels, ring.full_pixels);
    // `(w+2r)(h+2r) - w*h` for w=319, h=236, r=3.
    assert_eq!(ring.full_pixels, 325 * 242 - 319 * 236);
}

/// The floor is a PIXEL COUNT, not a per-side margin: a ring the border starves below
/// `min_ring_pixels` is refused, and the refusal names the count so the message can too.
#[test]
fn a_ring_starved_below_the_pixel_floor_is_still_refused() {
    let mark = PixelRect::new(0, 0, 20, 19);
    let page = page_with_mark(20, 20, [240, 240, 240], mark);
    let verdict = validate_calibration_sample(&page, mark, &SampleParams::default());
    let SampleVerdict::Unusable {
        reason: SampleRejection::RingTooSmall { pixels, needed },
    } = verdict
    else {
        panic!("20 ring pixels cannot support a flatness claim: {verdict:?}");
    };
    assert_eq!(pixels, 20);
    assert_eq!(needed, MIN_RING_PIXELS);
}

/// Partial and NOT FLAT are different failures and stay different: a truncated ring that
/// carries real structure is refused as a template-only sample, with the flatness numbers
/// that refused it, and never admitted because it was short of pixels anyway.
#[test]
fn a_partial_ring_that_is_not_flat_is_refused_as_not_flat() {
    let mark = PixelRect::new(480, 200, 319, 236);
    // A horizontal gradient across the whole page: the surviving ring sides sit on
    // different parts of it, which is exactly the bias a truncated ring could otherwise
    // introduce into the level.
    let mut page = page_with_mark(800, 1200, [240, 240, 240], mark);
    for y in 0..1200 {
        for x in 0..800 {
            let inside = x >= mark.x
                && y >= mark.y
                && u64::from(x) < mark.right()
                && u64::from(y) < mark.bottom();
            if inside {
                continue;
            }
            // Cast justification: `x` is bounded by 800, so `x / 4` fits u8 exactly.
            let shade = (x / 4) as u8;
            page.put_pixel(x, y, image::Rgba([shade, shade, shade, 255]));
        }
    }
    let verdict = validate_calibration_sample(&page, mark, &SampleParams::default());
    let SampleVerdict::TemplateOnly { ring, ring_std, .. } = verdict else {
        panic!("a structured ring is a template, not a calibration target: {verdict:?}");
    };
    assert!(
        ring_std.iter().any(|&v| v > FLAT_RING_STD_LIMIT),
        "the flatness test is what refused it, not the coverage: {ring_std:?}"
    );
    assert!(ring.is_partial(), "and the coverage is still reported: {ring:?}");
}

/// The coverage travels all the way into a fitted model's provenance, so a report can say
/// the measurement was one-sided without re-deriving it from the samples.
#[test]
fn a_partial_ring_is_counted_in_the_model_provenance() {
    let footprint = PixelRect::new(0, 0, 8, 8);
    let mut samples = Vec::new();
    for (level, ring) in [
        (0.0f32, RingCoverage::full(120)),
        (
            255.0f32,
            RingCoverage {
                pixels: 70,
                full_pixels: 120,
            },
        ),
    ] {
        let page = RgbaImage::from_fn(8, 8, |x, y| {
            // A mark that actually varies, so the fit has something to separate.
            let alpha = 0.2 + 0.1 * f32::from(u8::try_from((x + y) % 4).unwrap_or(0));
            // Cast justification: an alpha composite of two 0..=255 values, rounded.
            let value = (alpha * 30.0 + (1.0 - alpha) * level).clamp(0.0, 255.0).round() as u8;
            image::Rgba([value, value, value, 255])
        });
        samples.push(
            CalibrationSample::from_page(
                &page,
                0,
                footprint,
                SampleBackground::Flat {
                    level: [level, level, level],
                    ring_std: [0.1, 0.1, 0.1],
                    ring,
                },
            )
            .expect("synthetic sample"),
        );
    }
    let model = estimate_model(&samples, alpha_blend_operator(), AlphaAssumption::FromDeposit).expect("a fit");
    assert_eq!(
        model.provenance().partial_rings,
        1,
        "one of the two samples rests on a truncated ring"
    );
    assert_eq!(
        model.provenance().manual_backgrounds,
        0,
        "a partial ring is a MEASUREMENT and must never be counted as an assertion"
    );
}
