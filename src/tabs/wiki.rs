/*
FILE OVERVIEW: src/tabs/wiki.rs
Wiki tab for rendering local Markdown docs from `wiki/` with file tabs and
an async image pipeline.

Main structs:
- `WikiTabState`: tab state, selected file, async receivers, and image cache.
- `WikiFileEntry`: one markdown file from `wiki/`.
- `WikiDocument`: loaded markdown file plus parsed render blocks.
- `WikiBlock`: simplified markdown blocks (headings, paragraphs, lists, image rows, code).
- `WikiImageSpec`: one image inside a row (resolved source + optional `w=NN%`).
- `WikiImageEntry`: image cache record (pending/ready/failed).
- `InlineSegment`: inline markdown fragment for plain/code/bold text.

Background work (GUI-safe):
- wiki scan (`spawn_scan_thread`) runs in a worker thread.
- markdown read+parse (`spawn_document_load_thread`) runs in a worker thread.
- image decode (`spawn_image_load_thread`) runs in worker threads per unique image.
All results are delivered via channels and polled from `draw()`.

Inline rendering notes:
- headings and body text use the same inline parser (`parse_inline_segments`),
  so markdown markers like `**bold**` are removed consistently in all block types.
- bold text prefers a dedicated bold font family (`system-ui-sans-bold`) and
  falls back to stronger color emphasis when bold font is unavailable.
- image destinations accept Markdown angle delimiters (`<path with spaces>`),
  which are stripped before local path resolution.
- relative local image paths are normalized through `PathBuf` on Windows and
  use the same invalid-character replacement as installer ZIP extraction.

Image rendering notes:
- a Markdown line made only of `![alt](src)` tags becomes one `ImageRow`; two
  tags on the same line render side by side, the shorter one vertically centered.
- each image defaults to `WIKI_DEFAULT_IMAGE_WIDTH_FRACTION` of the content
  width (so two defaults fill one row) and is never upscaled past its native
  size; a `w=NN%` token inside the alt text overrides the target width.
- the alt text itself is not drawn under the image (it only carries directives).
*/

use eframe::egui;
use egui::{ColorImage, TextureOptions};
use std::collections::HashMap;
// `Read` and `Duration` are only used by the native remote-image loader below.
#[cfg(not(target_arch = "wasm32"))]
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
#[cfg(not(target_arch = "wasm32"))]
use web_time::Duration;

#[derive(Debug, Clone)]
struct WikiFileEntry {
    title: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct WikiDocument {
    path: PathBuf,
    markdown: String,
    blocks: Vec<WikiBlock>,
}

#[derive(Debug, Clone)]
enum WikiBlock {
    Heading { level: usize, text: String },
    Paragraph(String),
    Bullet(String),
    Numbered(String),
    Code(String),
    /// One or more images sharing a single horizontal row. A one-spec row is a
    /// normal standalone image; a two-spec row renders the images side by side.
    ImageRow(Vec<WikiImageSpec>),
}

/// One image inside a [`WikiBlock::ImageRow`].
///
/// `source` is the resolved storage-relative image path or remote URL.
/// `width_percent` is the optional `w=NN%` directive parsed from the Markdown
/// alt text (percentage of the wiki content width); `None` selects the default
/// width fraction. Alt text is not rendered, so it only carries directives.
#[derive(Debug, Clone)]
struct WikiImageSpec {
    source: String,
    width_percent: Option<f32>,
}

struct WikiImageEntry {
    texture: Option<egui::TextureHandle>,
    original_size: egui::Vec2,
    pending: bool,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct InlineSegment {
    text: String,
    is_code: bool,
    is_bold: bool,
}

/// Default display width of a wiki image as a fraction of the available content
/// width when no `w=NN%` directive is present. Two default images fill one row.
const WIKI_DEFAULT_IMAGE_WIDTH_FRACTION: f32 = 0.5;

/// Owned render instruction for one slot of an image row, decoupled from the
/// image cache so the egui layout closure does not need to borrow the tab state.
enum WikiRowItem {
    Image {
        texture: egui::TextureId,
        size: egui::Vec2,
    },
    Pending(String),
    Error {
        source: String,
        error: String,
    },
    Missing(String),
}

enum WikiScanResult {
    Ok(Vec<WikiFileEntry>),
    Err(String),
}

enum WikiDocLoadResult {
    Ok(WikiDocument),
    Err(String),
}

enum WikiImageLoadResult {
    Ok {
        key: String,
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
    Err {
        key: String,
        error: String,
    },
}

pub struct WikiTabState {
    wiki_dir: PathBuf,
    files: Vec<WikiFileEntry>,
    selected_idx: Option<usize>,
    doc: Option<WikiDocument>,
    scan_rx: Option<Receiver<WikiScanResult>>,
    doc_rx: Option<Receiver<WikiDocLoadResult>>,
    image_rx: Receiver<WikiImageLoadResult>,
    image_tx: Sender<WikiImageLoadResult>,
    image_cache: HashMap<String, WikiImageEntry>,
    status: String,
}

impl WikiTabState {
    pub fn new() -> Self {
        let (image_tx, image_rx) = channel();
        let mut state = Self {
            wiki_dir: PathBuf::from("wiki"),
            files: Vec::new(),
            selected_idx: None,
            doc: None,
            scan_rx: None,
            doc_rx: None,
            image_rx,
            image_tx,
            image_cache: HashMap::new(),
            status: "Инициализация вкладки вики...".to_owned(),
        };
        state.refresh_files();
        state
    }

    pub fn draw(&mut self, ui: &mut egui::Ui) {
        self.poll_background(ui.ctx());

        ui.horizontal(|ui| {
            if ui.button("Обновить список").clicked() {
                self.refresh_files();
            }
            ui.label(format!("Папка: {}", self.wiki_dir.display()));
        });
        ui.label(&self.status);
        ui.separator();

        self.draw_file_tabs(ui);
        ui.separator();
        self.draw_document(ui);
    }

    fn draw_file_tabs(&mut self, ui: &mut egui::Ui) {
        let mut clicked_idx = None;
        egui::ScrollArea::horizontal()
            .id_salt("wiki_files_tabs")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (idx, entry) in self.files.iter().enumerate() {
                        let selected = self.selected_idx == Some(idx);
                        if ui.selectable_label(selected, &entry.title).clicked() {
                            clicked_idx = Some(idx);
                        }
                    }
                });
            });
        if self.files.is_empty() {
            ui.label("В папке wiki нет .md файлов.");
        }
        if let Some(idx) = clicked_idx {
            self.selected_idx = Some(idx);
            self.load_selected_file();
        }
    }

    fn draw_document(&mut self, ui: &mut egui::Ui) {
        let Some(doc) = self.doc.as_ref() else {
            if self.files.is_empty() {
                ui.label("Добавьте .md файлы в папку wiki.");
            } else {
                ui.label("Выберите Markdown-файл во вкладках выше.");
            }
            return;
        };
        let path_label = doc.path.display().to_string();
        let chars_count = doc.markdown.chars().count();
        let blocks = doc.blocks.clone();

        ui.small(format!("{path_label} ({chars_count} символов)"));
        ui.separator();

        egui::ScrollArea::vertical()
            .id_salt("wiki_doc_scroll")
            .show(ui, |ui| {
                for block in &blocks {
                    self.draw_block(ui, block);
                }
            });
    }

    fn draw_block(&mut self, ui: &mut egui::Ui, block: &WikiBlock) {
        match block {
            WikiBlock::Heading { level, text } => {
                let size = match level {
                    1 => 28.0,
                    2 => 24.0,
                    3 => 20.0,
                    _ => 18.0,
                };
                ui.add_space(6.0);
                self.draw_inline_text(ui, text, None, Some(size), true);
            }
            WikiBlock::Paragraph(text) => {
                self.draw_inline_text(ui, text, None, None, false);
            }
            WikiBlock::Bullet(text) => {
                self.draw_inline_text(ui, text, Some("•"), None, false);
            }
            WikiBlock::Numbered(text) => {
                self.draw_inline_text(ui, text, Some("1."), None, false);
            }
            WikiBlock::Code(text) => {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).monospace())
                            .sense(egui::Sense::hover()),
                    );
                });
            }
            WikiBlock::ImageRow(specs) => {
                self.draw_image_row(ui, specs);
            }
        }
        ui.add_space(4.0);
    }

    fn draw_inline_text(
        &self,
        ui: &mut egui::Ui,
        text: &str,
        prefix: Option<&str>,
        base_size: Option<f32>,
        base_strong: bool,
    ) {
        for (line_idx, line) in text.lines().enumerate() {
            ui.horizontal_wrapped(|ui| {
                if line_idx == 0 {
                    if let Some(mark) = prefix {
                        ui.label(mark);
                    }
                } else if prefix.is_some() {
                    ui.label(" ");
                }

                for seg in parse_inline_segments(line) {
                    if seg.is_code {
                        let mut rich = egui::RichText::new(format!(" {} ", seg.text))
                            .monospace()
                            .background_color(ui.visuals().code_bg_color);
                        if let Some(size) = base_size {
                            rich = rich.size(size);
                        }
                        ui.label(rich);
                    } else {
                        let mut rich = egui::RichText::new(seg.text);
                        if let Some(size) = base_size {
                            rich = rich.size(size);
                        }
                        if base_strong || seg.is_bold {
                            rich = rich.strong();
                        }
                        if seg.is_bold {
                            rich = rich.color(ui.visuals().strong_text_color());
                        }
                        ui.label(rich);
                    }
                }
            });
        }
    }

    /// Draws a row of one or more images on a single horizontal line.
    ///
    /// Each image targets its `w=NN%` directive or the default width fraction of
    /// the available content width, is never upscaled past its native size, and
    /// the whole row is uniformly shrunk if the combined width (plus spacing)
    /// would overflow. Loaded images are vertically centered against the tallest
    /// one; pending or failed images render an inline placeholder in their slot.
    fn draw_image_row(&mut self, ui: &mut egui::Ui, specs: &[WikiImageSpec]) {
        for spec in specs {
            self.ensure_image_loading(&spec.source);
        }

        let available = ui.available_width().max(1.0);
        let spacing = ui.spacing().item_spacing.x;

        // First pass: resolve the display size of every already-loaded image.
        let display_sizes: Vec<Option<egui::Vec2>> = specs
            .iter()
            .map(|spec| self.image_display_size(spec, available))
            .collect();

        // Uniformly shrink the loaded images so the whole row fits the width
        // (keeps aspect ratios; only ever shrinks, never enlarges).
        let loaded_count = display_sizes.iter().filter(|size| size.is_some()).count();
        let total_w: f32 = display_sizes.iter().flatten().map(|size| size.x).sum();
        let gaps = spacing * (loaded_count.saturating_sub(1)) as f32;
        let total = total_w + gaps;
        let shrink = if total > available && total > 0.0 {
            available / total
        } else {
            1.0
        };

        // Build owned render items so the layout closure does not borrow `self`.
        let items: Vec<WikiRowItem> = specs
            .iter()
            .zip(display_sizes.iter())
            .map(|(spec, size)| self.build_row_item(spec, *size, shrink))
            .collect();

        // `ui.horizontal` uses `Align::Center`, so the shorter image is
        // vertically centered against the taller one automatically.
        ui.horizontal(|ui| {
            for item in &items {
                match item {
                    WikiRowItem::Image { texture, size } => {
                        ui.add(egui::Image::new((*texture, *size)));
                    }
                    WikiRowItem::Pending(source) => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(format!("Загрузка изображения: {source}"));
                        });
                    }
                    WikiRowItem::Error { source, error } => {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!("Не удалось загрузить изображение {source}: {error}"),
                        );
                    }
                    WikiRowItem::Missing(source) => {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            format!("Изображение не загружено: {source}"),
                        );
                    }
                }
            }
        });
    }

    /// Computes the pre-shrink display size of `spec` from its native size and
    /// width directive, or `None` if the image is not loaded yet.
    ///
    /// `available` is the row content width. The target width is `w=NN%` (or the
    /// default fraction) of `available`, clamped so it never exceeds the native
    /// width; the height follows the native aspect ratio.
    fn image_display_size(&self, spec: &WikiImageSpec, available: f32) -> Option<egui::Vec2> {
        let native = self
            .image_cache
            .get(&spec.source)
            .filter(|entry| entry.texture.is_some())
            .map(|entry| entry.original_size)
            .filter(|size| size.x > 0.0 && size.y > 0.0)?;
        let fraction = spec
            .width_percent
            .map_or(WIKI_DEFAULT_IMAGE_WIDTH_FRACTION, |percent| percent / 100.0)
            .max(0.0);
        // Target width from the directive, never upscaled past the native width.
        let target_w = (available * fraction).clamp(1.0, native.x);
        let scale = target_w / native.x;
        Some(egui::vec2(target_w, native.y * scale))
    }

    /// Translates a spec plus its resolved size into an owned [`WikiRowItem`],
    /// applying the row-wide `shrink` factor to loaded images.
    fn build_row_item(
        &self,
        spec: &WikiImageSpec,
        display_size: Option<egui::Vec2>,
        shrink: f32,
    ) -> WikiRowItem {
        let Some(entry) = self.image_cache.get(&spec.source) else {
            return WikiRowItem::Missing(spec.source.clone());
        };
        if let (Some(size), Some(texture)) = (display_size, entry.texture.as_ref()) {
            return WikiRowItem::Image {
                texture: texture.id(),
                size: size * shrink,
            };
        }
        if entry.pending {
            WikiRowItem::Pending(spec.source.clone())
        } else if let Some(error) = entry.error.as_ref() {
            WikiRowItem::Error {
                source: spec.source.clone(),
                error: error.clone(),
            }
        } else {
            WikiRowItem::Missing(spec.source.clone())
        }
    }

    /// Inserts a pending cache entry for `source` and starts its background
    /// decode if the image is not already tracked.
    fn ensure_image_loading(&mut self, source: &str) {
        if self.image_cache.contains_key(source) {
            return;
        }
        self.image_cache.insert(
            source.to_owned(),
            WikiImageEntry {
                texture: None,
                original_size: egui::vec2(1.0, 1.0),
                pending: true,
                error: None,
            },
        );
        self.spawn_image_load_thread(source.to_owned());
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        self.poll_scan_results();
        self.poll_doc_results();
        self.poll_image_results(ctx);
    }

    fn poll_scan_results(&mut self) {
        let Some(rx) = self.scan_rx.as_ref() else {
            return;
        };
        let mut events = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        for event in events {
            match event {
                WikiScanResult::Ok(files) => {
                    self.files = files;
                    self.selected_idx = if self.files.is_empty() { None } else { Some(0) };
                    self.status = format!("Найдено файлов: {}", self.files.len());
                    self.load_selected_file();
                }
                WikiScanResult::Err(err) => {
                    self.files.clear();
                    self.selected_idx = None;
                    self.doc = None;
                    self.status = err;
                }
            }
        }
    }

    fn poll_doc_results(&mut self) {
        let Some(rx) = self.doc_rx.as_ref() else {
            return;
        };
        let mut events = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        for event in events {
            match event {
                WikiDocLoadResult::Ok(doc) => {
                    self.doc = Some(doc);
                    self.status = "Файл загружен".to_owned();
                }
                WikiDocLoadResult::Err(err) => {
                    self.doc = None;
                    self.status = err;
                }
            }
        }
    }

    fn poll_image_results(&mut self, ctx: &egui::Context) {
        loop {
            match self.image_rx.try_recv() {
                Ok(WikiImageLoadResult::Ok {
                    key,
                    width,
                    height,
                    rgba,
                }) => {
                    if let Some(entry) = self.image_cache.get_mut(&key) {
                        let image = ColorImage::from_rgba_unmultiplied([width, height], &rgba);
                        let texture = ctx.load_texture(
                            format!("wiki-img-{key}"),
                            image,
                            TextureOptions::LINEAR,
                        );
                        entry.texture = Some(texture);
                        entry.original_size = egui::vec2(width as f32, height as f32);
                        entry.pending = false;
                        entry.error = None;
                    }
                    ctx.request_repaint();
                }
                Ok(WikiImageLoadResult::Err { key, error }) => {
                    if let Some(entry) = self.image_cache.get_mut(&key) {
                        entry.pending = false;
                        entry.error = Some(error);
                    }
                    ctx.request_repaint();
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn refresh_files(&mut self) {
        self.status = "Сканирование папки wiki...".to_owned();
        self.doc = None;
        self.image_cache.clear();
        let wiki_dir = self.wiki_dir.clone();
        let (tx, rx) = channel();
        self.scan_rx = Some(rx);
        spawn_scan_thread(wiki_dir, tx);
    }

    fn load_selected_file(&mut self) {
        self.doc = None;
        self.image_cache.clear();
        let Some(idx) = self.selected_idx else {
            return;
        };
        let Some(entry) = self.files.get(idx) else {
            return;
        };
        self.status = format!("Загрузка файла: {}", entry.path.display());
        let path = entry.path.clone();
        let (tx, rx) = channel();
        self.doc_rx = Some(rx);
        spawn_document_load_thread(path, tx);
    }

    fn spawn_image_load_thread(&self, key: String) {
        let tx = self.image_tx.clone();
        ms_thread::spawn(move || {
            let load = if key.starts_with("http://") || key.starts_with("https://") {
                load_remote_image_rgba(&key)
            } else {
                load_local_image_rgba(Path::new(&key))
            };
            match load {
                Ok((w, h, rgba)) => {
                    let _ = tx.send(WikiImageLoadResult::Ok {
                        key,
                        width: w,
                        height: h,
                        rgba,
                    });
                }
                Err(err) => {
                    let _ = tx.send(WikiImageLoadResult::Err { key, error: err });
                }
            }
        });
    }
}

impl Default for WikiTabState {
    fn default() -> Self {
        Self::new()
    }
}

fn spawn_scan_thread(wiki_dir: PathBuf, tx: Sender<WikiScanResult>) {
    ms_thread::spawn(move || {
        // Routed through the storage seam so the web build scans its virtual store
        // instead of the desktop filesystem.
        let store = crate::storage::storage();
        let wiki_dir_str = wiki_dir.to_string_lossy();
        if let Err(err) = store.create_dir_all(wiki_dir_str.as_ref()) {
            let _ = tx.send(WikiScanResult::Err(format!(
                "Не удалось создать папку {}: {err}",
                wiki_dir.display()
            )));
            return;
        }

        let entries = match store.read_dir(wiki_dir_str.as_ref()) {
            Ok(entries) => entries,
            Err(err) => {
                let _ = tx.send(WikiScanResult::Err(format!(
                    "Не удалось прочитать папку {}: {err}",
                    wiki_dir.display()
                )));
                return;
            }
        };

        let mut files = Vec::new();
        for entry in entries {
            // Rebuild each child path by joining the scanned dir with the entry name.
            let path = wiki_dir.join(&entry.name);
            if !is_markdown_file(&path) {
                continue;
            }
            let title = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Без имени")
                .to_owned();
            files.push(WikiFileEntry { title, path });
        }
        files.sort_by(|a, b| a.title.cmp(&b.title));

        let _ = tx.send(WikiScanResult::Ok(files));
    });
}

fn spawn_document_load_thread(path: PathBuf, tx: Sender<WikiDocLoadResult>) {
    ms_thread::spawn(move || {
        // Routed through the storage seam so the web build reads markdown from its
        // virtual store instead of the desktop filesystem.
        let path_str = path.to_string_lossy();
        let markdown = match crate::storage::storage().read_to_string(path_str.as_ref()) {
            Ok(text) => text,
            Err(err) => {
                let _ = tx.send(WikiDocLoadResult::Err(format!(
                    "Не удалось прочитать {}: {err}",
                    path.display()
                )));
                return;
            }
        };
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
        let blocks = parse_markdown_to_blocks(&markdown, base_dir);
        let _ = tx.send(WikiDocLoadResult::Ok(WikiDocument {
            path,
            markdown,
            blocks,
        }));
    });
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("md"))
        .unwrap_or(false)
}

fn parse_markdown_to_blocks(markdown: &str, base_dir: &Path) -> Vec<WikiBlock> {
    let mut blocks = Vec::new();
    let mut lines = markdown.lines().peekable();
    let mut in_code = false;
    let mut code_buf = String::new();

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            if in_code {
                if !code_buf.is_empty() {
                    blocks.push(WikiBlock::Code(code_buf.trim_end().to_owned()));
                }
                code_buf.clear();
                in_code = false;
            } else {
                in_code = true;
            }
            continue;
        }
        if in_code {
            code_buf.push_str(line);
            code_buf.push('\n');
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if let Some((level, text)) = parse_heading(trimmed) {
            blocks.push(WikiBlock::Heading { level, text });
            continue;
        }
        if let Some(images) = parse_image_row(trimmed) {
            let specs = images
                .into_iter()
                .map(|(alt, source)| WikiImageSpec {
                    source: resolve_image_source(base_dir, &source),
                    width_percent: parse_image_alt_width(&alt),
                })
                .collect();
            blocks.push(WikiBlock::ImageRow(specs));
            continue;
        }
        if let Some(text) = parse_bullet(trimmed) {
            blocks.push(WikiBlock::Bullet(text.to_owned()));
            continue;
        }
        if let Some(text) = parse_numbered(trimmed) {
            blocks.push(WikiBlock::Numbered(text));
            continue;
        }

        let mut paragraph = String::from(trimmed);
        while let Some(next) = lines.peek() {
            let n = next.trim();
            if n.is_empty()
                || n.starts_with('#')
                || n.starts_with("- ")
                || n.starts_with("```")
                || parse_numbered(n).is_some()
                || parse_image_row(n).is_some()
            {
                break;
            }
            paragraph.push('\n');
            paragraph.push_str(n);
            lines.next();
        }
        blocks.push(WikiBlock::Paragraph(paragraph));
    }

    if in_code && !code_buf.is_empty() {
        blocks.push(WikiBlock::Code(code_buf.trim_end().to_owned()));
    }
    blocks
}

fn parse_heading(line: &str) -> Option<(usize, String)> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let text = line[hashes..].trim();
    if text.is_empty() {
        None
    } else {
        Some((hashes, text.to_owned()))
    }
}

/// Parses a Markdown line that consists only of `![alt](source)` image tags.
///
/// Returns the `(alt, source)` pairs in order (one entry for a standalone image,
/// two for a side-by-side row). Returns `None` when the line is not purely made
/// of image tags (mixed text or no image), so it can fall back to a paragraph.
fn parse_image_row(line: &str) -> Option<Vec<(String, String)>> {
    let mut rest = line.trim();
    if !rest.starts_with("![") {
        return None;
    }
    let mut images = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        // A non-empty, non-image remainder means the line mixes text with images
        // and is not a pure image row, so the whole line is rejected here.
        let (alt, source, consumed) = parse_leading_image_tag(rest)?;
        images.push((alt, source));
        rest = &rest[consumed..];
    }
    if images.is_empty() {
        None
    } else {
        Some(images)
    }
}

/// Parses a single leading `![alt](source)` image tag from `text`.
///
/// Returns the trimmed alt text, the normalized image source, and the number of
/// bytes consumed up to and including the closing `)`. Returns `None` if `text`
/// does not start with a well-formed image tag or the source is empty.
fn parse_leading_image_tag(text: &str) -> Option<(String, String, usize)> {
    if !text.starts_with("![") {
        return None;
    }
    let alt_end = text.find("](")?;
    let src_start = alt_end + 2;
    let close_rel = text[src_start..].find(')')?;
    let close_idx = src_start + close_rel;
    let alt = text[2..alt_end].trim().to_owned();
    let source = normalize_markdown_image_source(text[src_start..close_idx].trim());
    if source.is_empty() {
        return None;
    }
    Some((alt, source, close_idx + 1))
}

/// Extracts an optional display-width directive from Markdown image alt text.
///
/// Recognizes a `w=NN` or `w=NN%` token as a percentage of the wiki content
/// width. Returns `None` when no positive, valid directive is present.
fn parse_image_alt_width(alt: &str) -> Option<f32> {
    alt.split_whitespace().find_map(|token| {
        let value = token.strip_prefix("w=")?;
        let value = value.strip_suffix('%').unwrap_or(value);
        value.parse::<f32>().ok().filter(|percent| *percent > 0.0)
    })
}

fn normalize_markdown_image_source(source: &str) -> String {
    source
        .strip_prefix('<')
        .and_then(|inner| inner.strip_suffix('>'))
        .unwrap_or(source)
        .trim()
        .to_owned()
}

fn parse_bullet(line: &str) -> Option<&str> {
    line.strip_prefix("- ").map(str::trim)
}

fn parse_numbered(line: &str) -> Option<String> {
    let mut chars = line.chars().peekable();
    while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
        chars.next();
    }
    if chars.next() != Some('.') {
        return None;
    }
    let rest: String = chars.collect();
    let rest = rest.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_owned())
    }
}

fn parse_inline_segments(line: &str) -> Vec<InlineSegment> {
    let mut segments = Vec::new();
    let mut is_code = false;
    for part in line.split('`') {
        if !part.is_empty() {
            if is_code {
                segments.push(InlineSegment {
                    text: part.to_owned(),
                    is_code: true,
                    is_bold: false,
                });
            } else {
                segments.extend(parse_bold_segments(part));
            }
        }
        is_code = !is_code;
    }
    segments
}

fn parse_bold_segments(text: &str) -> Vec<InlineSegment> {
    let mut segments = Vec::new();
    let mut cursor = 0usize;

    while let Some((open_idx, marker_len)) = find_next_bold_marker(text, cursor) {
        if open_idx > cursor {
            segments.push(InlineSegment {
                text: text[cursor..open_idx].to_owned(),
                is_code: false,
                is_bold: false,
            });
        }

        let content_start = open_idx + marker_len;
        let marker = if marker_len == 3 { "***" } else { "**" };
        let Some(close_rel) = text[content_start..].find(marker) else {
            // Unclosed marker: keep it as plain text.
            segments.push(InlineSegment {
                text: text[open_idx..].to_owned(),
                is_code: false,
                is_bold: false,
            });
            cursor = text.len();
            break;
        };

        let close_idx = content_start + close_rel;
        if close_idx > content_start {
            segments.push(InlineSegment {
                text: text[content_start..close_idx].to_owned(),
                is_code: false,
                is_bold: true,
            });
        }
        cursor = close_idx + marker_len;
    }

    if cursor < text.len() {
        segments.push(InlineSegment {
            text: text[cursor..].to_owned(),
            is_code: false,
            is_bold: false,
        });
    }

    segments
}

fn find_next_bold_marker(text: &str, from: usize) -> Option<(usize, usize)> {
    let rel = text[from..].find("**")?;
    let idx = from + rel;
    let marker_len = if text[idx..].starts_with("***") { 3 } else { 2 };
    Some((idx, marker_len))
}

fn resolve_image_source(base_dir: &Path, source: &str) -> String {
    if source.starts_with("http://") || source.starts_with("https://") {
        return source.to_owned();
    }
    let path = Path::new(source);
    if path.is_absolute() {
        return path.to_string_lossy().to_string();
    }
    base_dir
        .join(relative_local_image_source_path(source))
        .to_string_lossy()
        .to_string()
}

fn relative_local_image_source_path(source: &str) -> PathBuf {
    #[cfg(windows)]
    {
        source
            .split(['/', '\\'])
            .filter(|part| !part.is_empty())
            .map(sanitize_windows_local_path_component)
            .collect()
    }

    #[cfg(not(windows))]
    {
        PathBuf::from(source)
    }
}

#[cfg(windows)]
fn sanitize_windows_local_path_component(component: &str) -> String {
    let mut sanitized: String = component
        .chars()
        .map(|ch| match ch {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect();

    while sanitized.ends_with([' ', '.']) {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        sanitized.push('_');
    }
    let stem = sanitized
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let is_reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .and_then(|suffix| suffix.parse::<u8>().ok())
            .is_some_and(|n| (1..=9).contains(&n))
        || stem
            .strip_prefix("LPT")
            .and_then(|suffix| suffix.parse::<u8>().ok())
            .is_some_and(|n| (1..=9).contains(&n));
    if is_reserved {
        sanitized.push('_');
    }
    sanitized
}

fn load_local_image_rgba(path: &Path) -> Result<(usize, usize, Vec<u8>), String> {
    // Routed through the storage seam so the web build decodes image bytes from its
    // virtual store instead of reading the desktop filesystem directly.
    let path_str = path.to_string_lossy();
    let bytes = crate::storage::storage()
        .read(path_str.as_ref())
        .map_err(|e| e.to_string())?;
    let image = image::load_from_memory(&bytes)
        .map_err(|e| e.to_string())?
        .to_rgba8();
    let (w, h) = image.dimensions();
    Ok((w as usize, h as usize, image.into_raw()))
}

/// Fetches a remote image over HTTP and decodes it to RGBA.
///
/// Native builds use `ureq`. On wasm there is no synchronous HTTP client here, so
/// remote image loading is unavailable and this returns an error instead of a
/// blank image.
#[cfg(not(target_arch = "wasm32"))]
fn load_remote_image_rgba(url: &str) -> Result<(usize, usize, Vec<u8>), String> {
    let response = ureq::get(url)
        .timeout(Duration::from_secs(20))
        .call()
        .map_err(|e: ureq::Error| e.to_string())?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let image = image::load_from_memory(&bytes)
        .map_err(|e: image::ImageError| e.to_string())?
        .to_rgba8();
    let (w, h) = image.dimensions();
    Ok((w as usize, h as usize, image.into_raw()))
}

#[cfg(target_arch = "wasm32")]
fn load_remote_image_rgba(_url: &str) -> Result<(usize, usize, Vec<u8>), String> {
    Err("загрузка изображений по сети недоступна в веб-версии".to_string())
}

#[cfg(test)]
mod tests {
    use super::{parse_image_alt_width, parse_image_row, resolve_image_source};
    use std::path::Path;

    #[test]
    fn parse_image_row_strips_markdown_angle_delimiters() {
        let parsed =
            parse_image_row("![alt text](<images/1: Лента картинок и её параметры/image.png>)");

        assert_eq!(
            parsed,
            Some(vec![(
                "alt text".to_owned(),
                "images/1: Лента картинок и её параметры/image.png".to_owned()
            )])
        );
    }

    #[test]
    fn parse_image_row_keeps_plain_sources_unchanged() {
        let parsed = parse_image_row("![alt text](images/with spaces/image.png)");

        assert_eq!(
            parsed,
            Some(vec![(
                "alt text".to_owned(),
                "images/with spaces/image.png".to_owned()
            )])
        );
    }

    #[test]
    fn parse_image_row_parses_two_images_on_one_line() {
        let parsed = parse_image_row("![a](images/1_1.png) ![b w=70%](images/1_2.png)");

        assert_eq!(
            parsed,
            Some(vec![
                ("a".to_owned(), "images/1_1.png".to_owned()),
                ("b w=70%".to_owned(), "images/1_2.png".to_owned()),
            ])
        );
    }

    #[test]
    fn parse_image_row_rejects_mixed_text_or_no_image() {
        assert_eq!(parse_image_row("![a](images/1.png) trailing text"), None);
        assert_eq!(parse_image_row("plain paragraph"), None);
    }

    #[test]
    fn parse_image_alt_width_reads_percent_directive() {
        assert_eq!(parse_image_alt_width("image w=70%"), Some(70.0));
        assert_eq!(parse_image_alt_width("w=50"), Some(50.0));
        assert_eq!(parse_image_alt_width("image"), None);
        assert_eq!(parse_image_alt_width("w=0%"), None);
    }

    #[test]
    fn resolve_image_source_joins_angle_delimited_relative_path_after_parse() {
        let row = parse_image_row("![alt](<images/with spaces/image.png>)")
            .expect("valid image markdown must parse");
        let (_, source) = &row[0];

        assert_eq!(
            resolve_image_source(Path::new("wiki"), source),
            "wiki/images/with spaces/image.png"
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolve_image_source_normalizes_markdown_slashes_on_windows() {
        assert_eq!(
            resolve_image_source(Path::new("wiki"), "images/Вкладка-Термины/1.png"),
            r"wiki\images\Вкладка-Термины\1.png"
        );
    }

    #[cfg(windows)]
    #[test]
    fn resolve_image_source_uses_installer_safe_names_on_windows() {
        assert_eq!(
            resolve_image_source(
                Path::new("wiki"),
                "images/1: Лента картинок и её параметры/1.png"
            ),
            r"wiki\images\1_ Лента картинок и её параметры\1.png"
        );
    }
}
