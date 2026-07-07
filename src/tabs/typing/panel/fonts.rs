/*
File: panel/fonts.rs

Purpose:
Free-function helpers extracted verbatim from panel.rs for font discovery and
loading.

Main responsibilities:
- discover and load fonts from the project fonts directory and, optionally, from
  system fonts;
- merge duplicate font files and assign disambiguating group labels;
- list font groups, compute font-file content hashes, and recurse font-file
  directories.

Notes:
Extracted verbatim from panel.rs. Free fns are pub(super) so panel.rs can use
them. use super::*; pulls in the parent module's types and imports.
*/

use super::*;

pub(super) fn resolve_fonts_dir() -> PathBuf {
    if let Ok(cwd) = env::current_dir() {
        let candidate = cwd.join("fonts");
        if candidate.is_dir() {
            return candidate;
        }
    }

    if let Ok(exe_path) = env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        let candidate = exe_dir.join("fonts");
        if candidate.is_dir() {
            return candidate;
        }
    }

    PathBuf::from("fonts")
}

pub(super) fn load_fonts(fonts_dir: &Path, use_system_fonts: bool) -> Vec<FontEntry> {
    let mut entries = load_fonts_from_dir(fonts_dir);
    if !use_system_fonts {
        return entries;
    }

    let mut known_paths: HashSet<PathBuf> = entries
        .iter()
        .flat_map(|font| std::iter::once(font.path.clone()).chain(font.alt_paths.iter().cloned()))
        .collect();
    for system_font in load_system_fonts() {
        if known_paths.insert(system_font.path.clone()) {
            entries.push(system_font);
        }
    }
    entries.sort_by_key(|font| font.label.to_lowercase());
    entries
}

pub(super) fn load_fonts_from_dir(fonts_dir: &Path) -> Vec<FontEntry> {
    let mut files = Vec::<PathBuf>::new();
    collect_font_files_recursive(fonts_dir, fonts_dir, &mut files);
    files.sort_by_key(|path| path.to_string_lossy().to_lowercase());

    // Читаем каждый файл один раз: и для перечня faces, и для хэша содержимого.
    let raws: Vec<RawFontFile> = files
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path).ok();
            let content_hash = bytes.as_deref().map_or(0, font_content_hash);
            let faces = bytes
                .as_deref()
                .map_or_else(default_single_face, font_faces_from_bytes);
            let stem = path
                .file_stem()
                .and_then(|v| v.to_str())
                .unwrap_or("font")
                .to_string();
            let group = font_group_name_for_path(fonts_dir, &path);
            let rep_face_index = faces.first().map(|face| face.face_index).unwrap_or(0);
            let coverage = bytes.as_deref().map_or_else(
                FontLanguageCoverage::default,
                |data| super::font_coverage::classify_font_bytes(data, rep_face_index),
            );
            // Original family/name from the representative face; fall back to the
            // file stem when the font file cannot be parsed.
            let original_name = bytes
                .as_deref()
                .and_then(|data| font_original_name_from_bytes(data, rep_face_index))
                .unwrap_or_else(|| stem.clone());
            RawFontFile {
                path,
                stem,
                group,
                content_hash,
                faces,
                coverage,
                original_name,
            }
        })
        .collect();

    let mut entries = merge_duplicate_fonts(raws);
    assign_font_disambiguators(&mut entries);
    entries
}

/// Объединяет копии одного шрифта (совпадает имя файла и содержимое — «тот же
/// хэш») в один пункт со списком групп; разные по содержимому остаются раздельно.
pub(super) fn merge_duplicate_fonts(raws: Vec<RawFontFile>) -> Vec<FontEntry> {
    // Кластеризация по (имя файла без регистра, хэш содержимого), с сохранением
    // порядка первого появления.
    let mut order: Vec<(String, u64)> = Vec::new();
    let mut clusters: HashMap<(String, u64), Vec<RawFontFile>> = HashMap::new();
    for raw in raws {
        let key = (raw.stem.to_lowercase(), raw.content_hash);
        if !clusters.contains_key(&key) {
            order.push(key.clone());
        }
        clusters.entry(key).or_default().push(raw);
    }

    let mut entries = Vec::with_capacity(order.len());
    for key in order {
        let mut cluster = clusters.remove(&key).unwrap_or_default();
        // Представитель — первый по пути (детерминированно).
        cluster.sort_by(|a, b| a.path.cmp(&b.path));
        let rep = &cluster[0];
        let label = rep.stem.clone();
        let faces = rep.faces.clone();
        let path = rep.path.clone();
        let coverage = rep.coverage.clone();
        let original_name = rep.original_name.clone();
        let alt_paths = cluster[1..].iter().map(|raw| raw.path.clone()).collect();
        // Объединение групп копий (без повторов, в стабильном порядке).
        let mut groups: Vec<Option<String>> = Vec::new();
        for raw in &cluster {
            if !groups.contains(&raw.group) {
                groups.push(raw.group.clone());
            }
        }
        entries.push(FontEntry {
            label,
            path,
            alt_paths,
            groups,
            disambig: None,
            faces,
            coverage,
            original_name,
        });
    }
    entries
}

/// Reads the ORIGINAL family/name of `face_index` from font `bytes` via fontdb.
/// Falls back to the face's post_script_name; returns `None` only when the file
/// cannot be parsed or yields no non-empty name.
#[must_use]
pub(super) fn font_original_name_from_bytes(bytes: &[u8], face_index: usize) -> Option<String> {
    let mut db = fontdb::Database::new();
    let ids = db.load_font_source(fontdb::Source::Binary(Arc::new(bytes.to_vec())));
    let id = ids.get(face_index).or_else(|| ids.first())?;
    let face = db.face(*id)?;
    face.families
        .first()
        .map(|(name, _)| name.clone())
        .filter(|name| !name.is_empty())
        .or_else(|| {
            Some(face.post_script_name.clone()).filter(|name| !name.is_empty())
        })
}

/// Проставляет скобочное уточнение (по группам) тем пунктам, у которых базовое
/// имя совпадает с другим пунктом.
pub(super) fn assign_font_disambiguators(entries: &mut [FontEntry]) {
    let mut label_counts: HashMap<String, usize> = HashMap::new();
    for entry in entries.iter() {
        *label_counts.entry(entry.label.to_lowercase()).or_insert(0) += 1;
    }
    // Уникальное имя — уточнение не нужно.
    let mut used: HashMap<String, usize> = HashMap::new();
    for entry in entries.iter_mut() {
        if label_counts.get(&entry.label.to_lowercase()).copied().unwrap_or(0) <= 1 {
            entry.disambig = None;
            continue;
        }
        let mut suffix = font_groups_label(&entry.groups);
        // Если уточнения совпали (например, два корневых) — добавим индекс.
        let key = format!("{}\u{0}{}", entry.label.to_lowercase(), suffix.to_lowercase());
        let n = used.entry(key).or_insert(0);
        *n += 1;
        if *n > 1 {
            suffix = format!("{suffix} {n}");
        }
        entry.disambig = Some(suffix);
    }
}

/// Отображаемое имя группы для уточнения: имя группы или «корень».
pub(super) fn font_groups_label(groups: &[Option<String>]) -> String {
    let parts: Vec<&str> = groups
        .iter()
        .map(|group| group.as_deref().unwrap_or("корень"))
        .collect();
    if parts.is_empty() {
        "корень".to_string()
    } else {
        parts.join(", ")
    }
}

#[must_use]
pub(super) fn font_content_hash(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

#[must_use]
pub(super) fn default_single_face() -> Vec<FontFaceEntry> {
    vec![FontFaceEntry {
        label: "Face 0".to_string(),
        face_index: 0,
    }]
}

pub(super) fn load_font_groups(fonts_dir: &Path) -> Vec<String> {
    let groups_dir = fonts_dir.join("groups");
    let Ok(read_dir) = fs::read_dir(groups_dir) else {
        return Vec::new();
    };

    let mut groups = read_dir
        .filter_map(|entry_result| {
            let entry = entry_result.ok()?;
            let path = entry.path();
            if !path.is_dir() {
                return None;
            }
            path.file_name()
                .and_then(|value| value.to_str())
                .map(ToOwned::to_owned)
        })
        .collect::<Vec<_>>();
    groups.sort_by_key(|group_name| group_name.to_lowercase());
    groups
}

pub(super) fn load_system_fonts() -> Vec<FontEntry> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();

    let mut by_path = HashMap::<PathBuf, Vec<FontFaceEntry>>::new();
    // Track the fontdb id per (path, face_index) so the representative face's
    // coverage can be read back via `db.with_face_data` (memory-mapped) below.
    let mut ids_by_path: HashMap<PathBuf, Vec<(usize, fontdb::ID)>> = HashMap::new();
    // Original family name per (path, face_index), used to pick the representative
    // face's real name for `FontEntry.original_name`.
    let mut families_by_path: HashMap<PathBuf, Vec<(usize, String)>> = HashMap::new();
    for face in db.faces() {
        let path = match &face.source {
            fontdb::Source::File(path) => path.clone(),
            _ => continue,
        };
        let family = face
            .families
            .first()
            .map(|(name, _)| name.as_str())
            .unwrap_or("Unknown");
        let style = match face.style {
            fontdb::Style::Normal => "Normal",
            fontdb::Style::Italic => "Italic",
            fontdb::Style::Oblique => "Oblique",
        };
        let face_index = face.index as usize;
        ids_by_path
            .entry(path.clone())
            .or_default()
            .push((face_index, face.id));
        families_by_path
            .entry(path.clone())
            .or_default()
            .push((face_index, family.to_string()));
        by_path.entry(path).or_default().push(FontFaceEntry {
            label: format!(
                "#{face_index} {family} | {style} | w{} | {}",
                face.weight.0, face.post_script_name
            ),
            face_index,
        });
    }

    let mut files: Vec<PathBuf> = by_path.keys().cloned().collect();
    files.sort_by_key(|path| path.to_string_lossy().to_lowercase());

    let mut used_labels = HashMap::<String, usize>::new();
    let mut entries = Vec::<FontEntry>::with_capacity(files.len());
    for path in files {
        let mut faces = by_path.remove(&path).unwrap_or_default();
        faces.sort_by_key(|face| face.face_index);
        if faces.is_empty() {
            faces.push(FontFaceEntry {
                label: "Face 0".to_string(),
                face_index: 0,
            });
        }

        let stem = path
            .file_stem()
            .and_then(|v| v.to_str())
            .unwrap_or("system font");
        let base_label = format!("{stem} [system]");
        let count = used_labels.entry(base_label.clone()).or_insert(0);
        *count += 1;
        let label = if *count > 1 {
            format!("{base_label} ({count})")
        } else {
            base_label
        };
        let rep_face_index = faces.first().map(|face| face.face_index).unwrap_or(0);
        let coverage = ids_by_path
            .get(&path)
            .and_then(|ids| {
                ids.iter()
                    .find(|(idx, _)| *idx == rep_face_index)
                    .map(|(_, id)| *id)
            })
            .and_then(|id| {
                db.with_face_data(id, |data, index| {
                    super::font_coverage::classify_font_bytes(data, index as usize)
                })
            })
            .unwrap_or_default();
        // Representative face's real family name; fall back to the file stem.
        let original_name = families_by_path
            .get(&path)
            .and_then(|fams| {
                fams.iter()
                    .find(|(idx, _)| *idx == rep_face_index)
                    .map(|(_, name)| name.clone())
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| stem.to_string());
        entries.push(FontEntry {
            label,
            path,
            alt_paths: Vec::new(),
            groups: vec![None],
            disambig: None,
            faces,
            coverage,
            original_name,
        });
    }

    entries
}

pub(super) fn font_faces_from_bytes(bytes: &[u8]) -> Vec<FontFaceEntry> {
    let mut db = fontdb::Database::new();
    let ids = db.load_font_source(fontdb::Source::Binary(Arc::new(bytes.to_vec())));
    if ids.is_empty() {
        return default_single_face();
    }

    let mut faces = Vec::with_capacity(ids.len());
    for (idx, id) in ids.iter().enumerate() {
        let label = if let Some(face) = db.face(*id) {
            let family = face
                .families
                .first()
                .map(|(name, _)| name.as_str())
                .unwrap_or("Unknown");
            let style = match face.style {
                fontdb::Style::Normal => "Normal",
                fontdb::Style::Italic => "Italic",
                fontdb::Style::Oblique => "Oblique",
            };
            format!(
                "#{idx} {family} | {style} | w{} | {}",
                face.weight.0, face.post_script_name
            )
        } else {
            format!("#{idx} Face")
        };
        faces.push(FontFaceEntry {
            label,
            face_index: idx,
        });
    }

    if faces.is_empty() {
        faces.push(FontFaceEntry {
            label: "Face 0".to_string(),
            face_index: 0,
        });
    }
    faces
}

pub(super) fn font_group_name_for_path(fonts_dir: &Path, path: &Path) -> Option<String> {
    let mut components = path.strip_prefix(fonts_dir).ok()?.components();
    let first = components.next()?.as_os_str().to_str()?;
    if !first.eq_ignore_ascii_case("groups") {
        return None;
    }
    components
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .map(ToOwned::to_owned)
}

pub(super) fn collect_font_files_recursive(root_dir: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };

    for entry_result in read_dir {
        let Ok(entry) = entry_result else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            if should_skip_font_dir(root_dir, &path) {
                continue;
            }
            collect_font_files_recursive(root_dir, &path, out);
            continue;
        }

        let ext = path
            .extension()
            .and_then(|v| v.to_str())
            .map(|v| v.to_ascii_lowercase())
            .unwrap_or_default();
        if matches!(ext.as_str(), "ttf" | "otf" | "ttc") {
            out.push(path);
        }
    }
}

pub(super) fn should_skip_font_dir(root_dir: &Path, dir: &Path) -> bool {
    dir.strip_prefix(root_dir)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .is_some_and(|component| component.eq_ignore_ascii_case("ui"))
}
