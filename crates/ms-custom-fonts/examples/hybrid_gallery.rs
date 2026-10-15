/*
File: crates/ms-custom-fonts/examples/hybrid_gallery.rs

Purpose:
Visual and timing harness of the hybrid compiler. Compiles hybrids of the given fonts at several
weight sets and renders the sources and every hybrid one under another through the production
renderer (`ms-text-render`) into one PNG; prints compile timings.

Usage:
  cargo run --release -p ms-custom-fonts --example hybrid_gallery -- \
      <out_dir> <font_a> <font_b> [<font_c> <font_d>] \
      [--weights 50:50,80:20] [--name gallery] [--policy disable|no_influence] [--full] [--text "..."]
      [--size 52] [--save-fonts]

  `--weights` lists weight sets separated by commas, one value per font separated by colons
  (default `50:50,80:20` for two fonts, equal weights otherwise). `--full` compiles the whole
  face (prints glyph count, total and per-glyph time) instead of only the sample characters.
  The labels are drawn with the bundled Noto Sans located from this crate's manifest directory.

Notes:
A developer tool: writes only `<out_dir>/<name>.png` (and, with `--save-fonts`, the compiled
`<out_dir>/<name>-<weights>.ttf` faces); never part of the application.
*/

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use ms_custom_fonts::{
    CompileScope, CompiledFont, CustomFontKind, CustomFontRecipe, HybridParams, HybridSource, MissingGlyphPolicy, SourceFont,
    compile,
};
use ms_text_render::types::{
    AntiAliasingMode, HorizontalAlign, KerningMode, LinePlacementReference, RenderExtraInfoRequest, RenderedTextImage,
    TextDrawnLinesLayoutParams, TextFormulaLayoutParams, TextLayoutMode, TextLineMode, TextRenderParams, TextShape,
    TextVectorLinesLayoutParams, TextWrapMode, VerticalLineDirection,
};
use ms_text_render::{FontContent, FontContentSet, font_content_id, render_text_to_image};

/// Default sample: Latin, Cyrillic, Hangul, digits and punctuation.
const DEFAULT_TEXT: &str = "Hamburgefonstiv Quick Jog 0123!\nСъешь же ещё этих мягких булок\n다람쥐 헌 쳇바퀴에 타고파";
/// Working name of the label font.
const LABEL_FONT: &str = "gallery-label";
const CANVAS_WIDTH: u32 = 1700;
const MARGIN: u32 = 24;

/// Parsed command line.
struct Args {
    out_dir: PathBuf,
    fonts: Vec<PathBuf>,
    weight_sets: Vec<Vec<f32>>,
    name: String,
    policy: MissingGlyphPolicy,
    full: bool,
    text: String,
    size: f32,
    save_fonts: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut positional = Vec::new();
    let mut weights = None;
    let mut name = "gallery".to_owned();
    let mut policy = MissingGlyphPolicy::NoInfluence;
    let mut full = false;
    let mut text = DEFAULT_TEXT.to_owned();
    let mut size = 52.0_f32;
    let mut save_fonts = false;
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--weights" => weights = Some(iter.next().ok_or("--weights needs a value")?),
            "--name" => name = iter.next().ok_or("--name needs a value")?,
            "--policy" => {
                let raw = iter.next().ok_or("--policy needs a value")?;
                policy = MissingGlyphPolicy::parse(&raw).ok_or(format!("unknown policy '{raw}'"))?;
            }
            "--full" => full = true,
            "--save-fonts" => save_fonts = true,
            "--text" => text = iter.next().ok_or("--text needs a value")?,
            "--size" => {
                let raw = iter.next().ok_or("--size needs a value")?;
                size = raw.parse().map_err(|error| format!("bad size '{raw}': {error}"))?;
            }
            _ => positional.push(arg),
        }
    }
    if positional.len() < 3 {
        return Err("usage: hybrid_gallery <out_dir> <font_a> <font_b> [<font_c> <font_d>] [--weights 50:50,80:20] [--name N] [--policy P] [--full] [--text T] [--size PX] [--save-fonts]".to_owned());
    }
    let out_dir = PathBuf::from(positional.remove(0));
    let fonts: Vec<PathBuf> = positional.into_iter().map(PathBuf::from).collect();
    let weight_sets = match weights {
        Some(raw) => raw
            .split(',')
            .map(|set| set.split(':').map(|value| value.trim().parse::<f32>().map_err(|error| format!("bad weight '{value}': {error}"))).collect())
            .collect::<Result<Vec<Vec<f32>>, String>>()?,
        None if fonts.len() == 2 => vec![vec![50.0, 50.0], vec![80.0, 20.0]],
        None => vec![vec![1.0; fonts.len()]],
    };
    if weight_sets.iter().any(|set| set.len() != fonts.len()) {
        return Err("every weight set needs one value per font".to_owned());
    }
    Ok(Args { out_dir, fonts, weight_sets, name, policy, full, text, size, save_fonts })
}

fn identity_of(path: &Path) -> String {
    path.file_stem().map_or_else(|| path.display().to_string(), |stem| stem.to_string_lossy().into_owned())
}

fn content(name: &str, bytes: Vec<u8>) -> FontContent {
    FontContent {
        name: name.to_owned(),
        original_name: name.to_owned(),
        content_id: font_content_id(&bytes),
        data: Arc::new(bytes),
        face_index: 0,
        custom_kerning: None,
    }
}

fn params(text: &str, font: &str, size: f32) -> TextRenderParams {
    TextRenderParams {
        text: text.to_owned(),
        text_color: [10, 10, 10, 255],
        font_name: font.to_owned(),
        font_size_px: size,
        line_spacing_px: 0.0,
        line_spacing_percent: 20.0,
        kerning_mode: KerningMode::Auto,
        kerning_px: 0.0,
        kerning_percent: 0.0,
        glyph_height_percent: 100.0,
        glyph_width_percent: 100.0,
        width_px: CANVAS_WIDTH - 2 * MARGIN,
        align: HorizontalAlign::LEFT,
        selected_face_index: 0,
        force_bold: false,
        force_italic: false,
        faux_bold: None,
        faux_italic_slant_deg: None,
        uppercase_text: false,
        trim_extra_spaces: true,
        replace_ellipsis_with_dots: true,
        force_remove_ellipsis_glyph: false,
        hanging_punctuation: 0.0,
        new_line_after_sentence: false,
        enable_inline_style_tags: false,
        text_wrap_mode: TextWrapMode::WholeWords,
        text_shape: TextShape::Free,
        shape_min_width_percent: 55.0,
        shape_variant: 5,
        compare_shape_with: None,
        allow_moderate_trees: false,
        text_line_mode: TextLineMode::Horizontal,
        vertical_line_direction: VerticalLineDirection::RightToLeft,
        text_layout_mode: TextLayoutMode::Normal,
        formula_layout: TextFormulaLayoutParams::default(),
        drawn_lines_layout: TextDrawnLinesLayoutParams::default(),
        vector_lines_layout: TextVectorLinesLayoutParams::default(),
        effects_json: String::new(),
        anti_aliasing: AntiAliasingMode::Strong,
        global_rotation_deg: 0.0,
        line_placement_percent: 0.0,
        line_placement_reference: LinePlacementReference::GlyphHeight,
        raster_transform: None,
        extra_info: RenderExtraInfoRequest::default(),
    }
}

/// Pastes `image` onto `canvas` at `(x, y)` with source-over blending.
fn paste(canvas: &mut image::RgbaImage, image: &RenderedTextImage, x: u32, y: u32) {
    for (index, pixel) in image.rgba.chunks_exact(4).enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        let (px, py) = (x + index % image.width.max(1), y + index / image.width.max(1));
        if px >= canvas.width() || py >= canvas.height() {
            continue;
        }
        let alpha = u32::from(pixel[3]);
        let target = canvas.get_pixel_mut(px, py);
        for channel in 0..3 {
            let blended = (u32::from(pixel[channel]) * alpha + u32::from(target[channel]) * (255 - alpha)) / 255;
            target[channel] = u8::try_from(blended).unwrap_or(u8::MAX);
        }
    }
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    std::fs::create_dir_all(&args.out_dir).map_err(|error| format!("cannot create {}: {error}", args.out_dir.display()))?;
    let datas: Vec<Vec<u8>> = args
        .fonts
        .iter()
        .map(|path| std::fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display())))
        .collect::<Result<_, _>>()?;
    let identities: Vec<String> = args.fonts.iter().map(|path| identity_of(path)).collect();
    let sources: Vec<SourceFont<'_>> = identities
        .iter()
        .zip(&datas)
        .map(|(identity, data)| SourceFont { identity, data, face_index: 0, user_kerning: &[] })
        .collect();
    let sample: BTreeSet<char> = args.text.chars().filter(|ch| !ch.is_control()).collect();
    let never = AtomicBool::new(false);

    let mut hybrids: Vec<(String, CompiledFont)> = Vec::new();
    for weights in &args.weight_sets {
        let recipe = CustomFontRecipe {
            kind: CustomFontKind::Hybrid(HybridParams {
                sources: identities
                    .iter()
                    .zip(weights)
                    .map(|(identity, &weight)| HybridSource { identity: identity.clone(), weight, missing_override: None })
                    .collect(),
                missing_default: args.policy,
            }),
        };
        let started = Instant::now();
        let preview = compile(&recipe, &sources, CompileScope::Subset(&sample), &|_, _| {}, &never).map_err(|error| error.to_string())?;
        println!(
            "weights {weights:?}: subset of {} chars compiled in {:.1} ms ({} bytes)",
            sample.len(),
            started.elapsed().as_secs_f64() * 1000.0,
            preview.bytes.len()
        );
        let compiled = if args.full {
            let started = Instant::now();
            let total = std::sync::atomic::AtomicUsize::new(0);
            let full = compile(&recipe, &sources, CompileScope::Full, &|_, all| total.store(all, std::sync::atomic::Ordering::Relaxed), &never)
                .map_err(|error| error.to_string())?;
            let elapsed = started.elapsed().as_secs_f64();
            let glyphs = total.load(std::sync::atomic::Ordering::Relaxed);
            println!(
                "weights {weights:?}: FULL {} glyphs in {:.2} s = {:.3} ms/glyph wall ({} threads), {} bytes, {} chars, {} disabled",
                glyphs,
                elapsed,
                elapsed * 1000.0 / f64::from(u32::try_from(glyphs.max(1)).unwrap_or(u32::MAX)),
                rayon::current_num_threads(),
                full.bytes.len(),
                full.report.total_chars,
                full.report.disabled
            );
            full
        } else {
            preview
        };
        let label = weights.iter().map(|weight| format!("{weight}")).collect::<Vec<_>>().join(":");
        if args.save_fonts {
            let path = args.out_dir.join(format!("{}-{}.ttf", args.name, label.replace(':', "_")));
            std::fs::write(&path, &compiled.bytes).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            println!("saved {}", path.display());
        }
        hybrids.push((label, compiled));
    }

    let label_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fonts/ui/core/00-NotoSans-Regular.ttf");
    let label_bytes = std::fs::read(&label_path).map_err(|error| format!("cannot read label font {}: {error}", label_path.display()))?;
    let mut fonts = vec![content(LABEL_FONT, label_bytes)];
    fonts.extend(identities.iter().zip(&datas).map(|(identity, data)| content(identity, data.clone())));
    fonts.extend(hybrids.iter().map(|(_, compiled)| content(&compiled.postscript_name, compiled.bytes.clone())));
    let provider = FontContentSet::new(fonts);

    let mut rows: Vec<(String, String)> = identities.iter().map(|identity| (format!("source: {identity}"), identity.clone())).collect();
    rows.extend(hybrids.iter().map(|(label, compiled)| (format!("hybrid {label} ({})", compiled.postscript_name), compiled.postscript_name.clone())));
    let mut rendered = Vec::new();
    for (label, font) in &rows {
        let label_image = render_text_to_image(&params(label, LABEL_FONT, 22.0), &provider, None)?;
        let text_image = render_text_to_image(&params(&args.text, font, args.size), &provider, None)?;
        if !text_image.font_fallbacks.is_empty() {
            println!("{label}: fallback {:?}", text_image.font_fallbacks);
        }
        rendered.push((label_image, text_image));
    }
    let height: u32 = rendered.iter().map(|(label, text)| label.height + text.height + 3 * MARGIN / 2).sum::<u32>() + MARGIN;
    let mut canvas = image::RgbaImage::from_pixel(CANVAS_WIDTH, height, image::Rgba([255, 255, 255, 255]));
    let mut y = MARGIN;
    for (label, text) in &rendered {
        paste(&mut canvas, label, MARGIN, y);
        y += label.height + MARGIN / 2;
        paste(&mut canvas, text, MARGIN, y);
        y += text.height + MARGIN;
    }
    let out = args.out_dir.join(format!("{}.png", args.name));
    canvas.save(&out).map_err(|error| format!("cannot write {}: {error}", out.display()))?;
    println!("wrote {}", out.display());
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hybrid_gallery: {error}");
            ExitCode::FAILURE
        }
    }
}
