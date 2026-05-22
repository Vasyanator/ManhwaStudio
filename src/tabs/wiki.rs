/*
FILE OVERVIEW: src/tabs/wiki.rs
Wiki tab for rendering local Markdown docs from `wiki/` with file tabs and
an async image pipeline.

Main structs:
- `WikiTabState`: tab state, selected file, async receivers, and image cache.
- `WikiFileEntry`: one markdown file from `wiki/`.
- `WikiDocument`: loaded markdown file plus parsed render blocks.
- `WikiBlock`: simplified markdown blocks (headings, paragraphs, lists, images, code).
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
*/

use eframe::egui;
use egui::{ColorImage, TextureOptions};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::Duration;

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
    Image { alt: String, source: String },
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
            WikiBlock::Image { alt, source } => {
                self.draw_image(ui, alt, source);
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

    fn draw_image(&mut self, ui: &mut egui::Ui, alt: &str, source: &str) {
        let key = source.to_owned();
        if !self.image_cache.contains_key(&key) {
            self.image_cache.insert(
                key.clone(),
                WikiImageEntry {
                    texture: None,
                    original_size: egui::vec2(1.0, 1.0),
                    pending: true,
                    error: None,
                },
            );
            self.spawn_image_load_thread(key.clone());
        }

        let Some(entry) = self.image_cache.get(&key) else {
            return;
        };
        if let Some(texture) = entry.texture.as_ref() {
            let max_w = ui.available_width().max(1.0);
            let scale = (max_w / entry.original_size.x).min(1.0);
            let size = entry.original_size * scale;
            ui.add(egui::Image::new((texture.id(), size)));
            if !alt.is_empty() {
                ui.small(alt);
            }
        } else if entry.pending {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!("Загрузка изображения: {source}"));
            });
        } else if let Some(err) = entry.error.as_ref() {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("Не удалось загрузить изображение {source}: {err}"),
            );
        } else {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("Изображение не загружено: {source}"),
            );
        }
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
        std::thread::spawn(move || {
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
    std::thread::spawn(move || {
        if let Err(err) = fs::create_dir_all(&wiki_dir) {
            let _ = tx.send(WikiScanResult::Err(format!(
                "Не удалось создать папку {}: {err}",
                wiki_dir.display()
            )));
            return;
        }

        let read_dir = match fs::read_dir(&wiki_dir) {
            Ok(dir) => dir,
            Err(err) => {
                let _ = tx.send(WikiScanResult::Err(format!(
                    "Не удалось прочитать папку {}: {err}",
                    wiki_dir.display()
                )));
                return;
            }
        };

        let mut files = Vec::new();
        for entry in read_dir.flatten() {
            let path = entry.path();
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
    std::thread::spawn(move || {
        let markdown = match fs::read_to_string(&path) {
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
        if let Some((alt, source)) = parse_image_line(trimmed) {
            let resolved = resolve_image_source(base_dir, &source);
            blocks.push(WikiBlock::Image {
                alt,
                source: resolved,
            });
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
                || parse_image_line(n).is_some()
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

fn parse_image_line(line: &str) -> Option<(String, String)> {
    if !line.starts_with("![") {
        return None;
    }
    let alt_end = line.find("](")?;
    if !line.ends_with(')') {
        return None;
    }
    let alt = line[2..alt_end].trim().to_owned();
    let src = normalize_markdown_image_source(line[(alt_end + 2)..(line.len() - 1)].trim());
    if src.is_empty() {
        None
    } else {
        Some((alt, src))
    }
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
    base_dir.join(path).to_string_lossy().to_string()
}

fn load_local_image_rgba(path: &Path) -> Result<(usize, usize, Vec<u8>), String> {
    let image = image::open(path).map_err(|e| e.to_string())?.to_rgba8();
    let (w, h) = image.dimensions();
    Ok((w as usize, h as usize, image.into_raw()))
}

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

#[cfg(test)]
mod tests {
    use super::{parse_image_line, resolve_image_source};
    use std::path::Path;

    #[test]
    fn parse_image_line_strips_markdown_angle_delimiters() {
        let parsed =
            parse_image_line("![alt text](<images/1: Лента картинок и её параметры/image.png>)");

        assert_eq!(
            parsed,
            Some((
                "alt text".to_owned(),
                "images/1: Лента картинок и её параметры/image.png".to_owned()
            ))
        );
    }

    #[test]
    fn parse_image_line_keeps_plain_sources_unchanged() {
        let parsed = parse_image_line("![alt text](images/with spaces/image.png)");

        assert_eq!(
            parsed,
            Some((
                "alt text".to_owned(),
                "images/with spaces/image.png".to_owned()
            ))
        );
    }

    #[test]
    fn resolve_image_source_joins_angle_delimited_relative_path_after_parse() {
        let (_, source) = parse_image_line("![alt](<images/with spaces/image.png>)")
            .expect("valid image markdown must parse");

        assert_eq!(
            resolve_image_source(Path::new("wiki"), &source),
            "wiki/images/with spaces/image.png"
        );
    }
}
