/*
File: storage_mode.rs

Purpose:
The storage-mode conversion DRIVER: converts every global and title-level document into the
format of a `StorageMode` (Prod = `.db`, Dev = `.json`) in the crash-safe order the docstore's
"both files exist" rule relies on. Chapters (`layers`, `bubbles`) are never touched here.

Key structures:
- GlobalConvertOutcome : per-run tally, the failed documents, and whether the sentinel settled

Key functions:
- convert_globals()      : the driver (blocking; run it on a worker, never the GUI thread)
- global_documents()     : the documents the driver converts, in driver order (fonts, titles)

Order (binding; plan §B "Driver order"):
  (a) `General.storage_mode = target` is written into user_config in its CURRENT format, then
      `ms_docstore::set_default_format(target)` — from here a crash leaves documents the
      docstore resolves toward the target;
  (b) fonts_data, presets (application fonts directory);
  (c) every title of `project_scan::list_titles(projects_root)`: its settings, characters,
      terms, char_favorites and color_presets documents that exist;
  (d) user_config LAST, and only when (b)+(c) had no failure. user_config's format is the
      startup sentinel (`ms_config::storage_mode::probe_storage_mode`): while it differs from
      the recorded mode the next start re-runs this driver.
Idempotent: documents already in the target format are skipped, documents left in both
formats by an interrupted run are finished by `convert_document`. Per-document failures are
collected; they never abort the batch.
*/

use std::path::{Path, PathBuf};

use anyhow::Context;
use ms_config::StorageMode;
use ms_docstore::{DocFormat, DocKind, DocRef};
use ms_log::runtime_log;
use serde_json::{Map, Value};

use crate::project_scan;

/// Result of one [`convert_globals`] run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GlobalConvertOutcome {
    /// Documents rewritten (or finished) in the target format.
    pub converted: usize,
    /// Documents already in the target format.
    pub skipped: usize,
    /// Documents (or the projects root) that failed, with a technical reason. Each failed
    /// document stays readable in its previous format.
    pub failed: Vec<(PathBuf, String)>,
    /// Whether step (a) persisted the mode and switched the docstore default.
    pub mode_persisted: bool,
    /// Whether step (d) brought user_config into the target format: the conversion is
    /// complete and the next start has nothing to reconcile.
    pub sentinel_settled: bool,
}

impl GlobalConvertOutcome {
    /// Whether every document is in the target format.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.mode_persisted && self.sentinel_settled && self.failed.is_empty()
    }
}

/// Converts every global and title-level document into `target`'s format (order in the
/// file header). `fonts_dir` is the application fonts directory
/// (`ms_config::storage_mode::app_fonts_dir`), `user_config_path` the user-config document
/// (with or without extension). `progress(done, total)` is called on the calling thread
/// after each document; `total` counts the existing documents plus user_config.
///
/// Blocking file I/O proportional to the number of titles: call it from a worker thread.
/// Failures are reported in the outcome and logged, never panicked on.
#[must_use]
pub fn convert_globals(target: StorageMode, projects_root: &Path, fonts_dir: &Path, user_config_path: &Path, progress: &dyn Fn(usize, usize)) -> GlobalConvertOutcome {
    convert_globals_with(target, projects_root, fonts_dir, user_config_path, &ms_docstore::set_default_format, progress)
}

/// [`convert_globals`] with the default-format switch injected: tests pass a recorder so
/// they never flip the process-global docstore default under their parallel neighbours.
fn convert_globals_with(target: StorageMode, projects_root: &Path, fonts_dir: &Path, user_config_path: &Path, set_default: &dyn Fn(DocFormat), progress: &dyn Fn(usize, usize)) -> GlobalConvertOutcome {
    let format = target.doc_format();
    let mut outcome = GlobalConvertOutcome::default();
    runtime_log::log_info(format!("[storage-mode] converting global documents to .{} (mode {})", format.extension(), target.as_config_str()));

    // (a) Persist the mode FIRST, in user_config's current format. Without it a crash
    // mid-run would restart in the old mode and the "both files" rule would pick the old
    // files, so nothing is converted when this step fails.
    if let Err(err) = persist_mode(user_config_path, target) {
        runtime_log::log_error(format!(
            "[storage-mode] could not record the new storage mode; nothing was converted.\nDocument: {}\nMode: {}\nError: {err:#}",
            user_config_path.display(),
            target.as_config_str()
        ));
        outcome.failed.push((user_config_path.to_path_buf(), format!("{err:#}")));
        return outcome;
    }
    set_default(format);
    outcome.mode_persisted = true;

    // (b) + (c): enumerate first so progress has a stable total.
    let (docs, scan_error) = global_documents(projects_root, fonts_dir);
    if let Some(err) = scan_error {
        runtime_log::log_error(format!("[storage-mode] could not list the titles of the projects folder; their documents stay in their previous format.\nPath: {}\nError: {err}", projects_root.display()));
        outcome.failed.push((projects_root.to_path_buf(), err));
    }
    let docs: Vec<DocRef> = docs.into_iter().filter(ms_docstore::exists).collect();
    let total = docs.len() + 1;
    let batch = ms_docstore::convert_many(&docs, format, &|done, _| progress(done, total));
    outcome.converted += batch.converted;
    outcome.skipped += batch.skipped;
    outcome.failed.extend(batch.failed.into_iter().map(|(path, err)| (path, err.to_string())));

    // (d) user_config LAST, and only after a clean (b)+(c): it is the startup sentinel.
    if outcome.failed.is_empty() {
        let user_config = DocRef::new(user_config_path, DocKind::UserConfig);
        match ms_docstore::convert_document(&user_config, format, &ms_docstore::NoHook) {
            Ok(ms_docstore::ConvertOutcome::Converted | ms_docstore::ConvertOutcome::Reconciled) => {
                outcome.converted += 1;
                outcome.sentinel_settled = true;
            }
            Ok(ms_docstore::ConvertOutcome::Skipped | ms_docstore::ConvertOutcome::Absent) => {
                outcome.skipped += 1;
                outcome.sentinel_settled = true;
            }
            Err(err) => {
                runtime_log::log_error(format!("[storage-mode] user config conversion failed; it stays in its previous format and the conversion re-runs at the next start.\nDocument: {}\nError: {err}", user_config_path.display()));
                outcome.failed.push((user_config.stem().to_path_buf(), err.to_string()));
            }
        }
    } else {
        runtime_log::log_warn(format!("[storage-mode] {} document(s) failed; user config is left in its previous format so the conversion re-runs at the next start", outcome.failed.len()));
    }
    progress(total, total);
    runtime_log::log_info(format!(
        "[storage-mode] conversion to .{} finished: converted={} skipped={} failed={} complete={}",
        format.extension(),
        outcome.converted,
        outcome.skipped,
        outcome.failed.len(),
        outcome.is_complete()
    ));
    outcome
}

/// Writes `General.storage_mode = target` into the user-config document (one serialized
/// read-modify-write in its current format; a missing document is created).
fn persist_mode(user_config_path: &Path, target: StorageMode) -> anyhow::Result<()> {
    ms_config::update_user_config_file(user_config_path, |root| {
        // `update_user_config_file` guarantees an object root before the mutator runs.
        let root_obj = root.as_object_mut().context("user config root is not an object")?;
        let general = root_obj.entry("General").or_insert_with(|| Value::Object(Map::new()));
        if !general.is_object() {
            *general = Value::Object(Map::new());
        }
        if let Value::Object(general) = general {
            general.insert(ms_config::GENERAL_STORAGE_MODE_KEY.to_owned(), Value::String(target.as_config_str().to_owned()));
        }
        Ok(())
    })
}

/// The documents steps (b) and (c) convert, in driver order: `fonts_data`, `presets`, then
/// the five title-level documents of every title (existing or not; the driver filters).
/// A projects root that does not exist has no titles; any other listing failure is
/// returned as the second element (the fonts documents are still listed).
#[must_use]
pub fn global_documents(projects_root: &Path, fonts_dir: &Path) -> (Vec<DocRef>, Option<String>) {
    let mut docs = vec![ms_config::storage_mode::fonts_data_doc(fonts_dir), ms_config::storage_mode::fonts_presets_doc(fonts_dir)];
    let titles = match project_scan::list_titles(projects_root) {
        Ok(titles) => titles,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return (docs, Some(err.to_string())),
    };
    for title in titles {
        docs.extend(title_documents(&projects_root.join(title)));
    }
    (docs, None)
}

/// The five title-level documents of `title_dir`.
fn title_documents(title_dir: &Path) -> [DocRef; 5] {
    [
        ms_config::project_settings_doc(title_dir),
        DocRef::new(title_dir.join(ms_config::CHARACTERS_DIR).join(ms_config::CHARACTERS_FILE), DocKind::Characters),
        DocRef::new(title_dir.join(ms_config::TERMS_FILE), DocKind::Terms),
        DocRef::new(title_dir.join(ms_config::CHAR_FAVORITES_FILE), DocKind::CharFavorites),
        DocRef::new(title_dir.join(ms_config::COLOR_PRESETS_FILE), DocKind::ColorPresets),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::{Cell, RefCell};

    /// Counts calls of an injected callback.
    #[derive(Debug, Default)]
    struct CallCount(Cell<usize>);

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    /// A data root with fonts docs, two titles and a user config, all JSON.
    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        fonts: PathBuf,
        user_config: PathBuf,
    }

    fn fixture() -> Result<Fixture, Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("projects");
        let fonts = temp.path().join("fonts");
        std::fs::create_dir_all(root.join("Title A").join("characters"))?;
        std::fs::create_dir_all(root.join("Title B").join("Chapter 1"))?;
        std::fs::create_dir_all(&fonts)?;
        let write = |path: PathBuf, value: Value| std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap_or_default());
        write(fonts.join("fonts_data.json"), json!({"fonts": {"A": {"size": 12}}}))?;
        write(fonts.join("presets.json"), json!({"presets": [{"id": "p1"}]}))?;
        write(root.join("Title A").join("settings.json"), json!({"canvas": {"x": 1}}))?;
        write(root.join("Title A").join("characters").join("characters.json"), json!([{"name": "Hero"}]))?;
        write(root.join("Title A").join("terms.json"), json!([{"term": "a"}]))?;
        write(root.join("Title B").join("color_presets.json"), json!([[1, 2, 3]]))?;
        write(root.join("Title B").join("char_favorites.json"), json!(["★"]))?;
        // A chapter document: must never be converted by the global driver.
        write(root.join("Title B").join("Chapter 1").join("bubbles.json"), json!({"bubbles": []}))?;
        let user_config = temp.path().join("user_config.json");
        write(user_config.clone(), json!({"General": {"theme": "dark"}}))?;
        Ok(Fixture { _temp: temp, root, fonts, user_config })
    }

    fn doc_files(fx: &Fixture) -> Vec<PathBuf> {
        vec![
            fx.fonts.join("fonts_data"),
            fx.fonts.join("presets"),
            fx.root.join("Title A").join("settings"),
            fx.root.join("Title A").join("characters").join("characters"),
            fx.root.join("Title A").join("terms"),
            fx.root.join("Title B").join("color_presets"),
            fx.root.join("Title B").join("char_favorites"),
        ]
    }

    fn with_ext(stem: &Path, ext: &str) -> PathBuf {
        stem.with_extension(ext)
    }

    #[test]
    fn converts_everything_with_user_config_last() -> TestResult {
        let fx = fixture()?;
        let defaults = RefCell::new(Vec::new());
        let json_seen_at_progress = RefCell::new(Vec::new());
        let outcome = convert_globals_with(StorageMode::Prod, &fx.root, &fx.fonts, &fx.user_config, &|format| defaults.borrow_mut().push(format), &|done, total| {
            // The default switch precedes every conversion.
            assert_eq!(defaults.borrow().len(), 1);
            json_seen_at_progress.borrow_mut().push((done, total, fx.user_config.is_file()));
        });
        assert!(outcome.is_complete(), "{outcome:?}");
        assert_eq!(*defaults.borrow(), vec![DocFormat::Db]);
        assert_eq!(outcome.converted, 8);
        for stem in doc_files(&fx) {
            assert!(with_ext(&stem, "db").is_file(), "{} not converted", stem.display());
            assert!(!with_ext(&stem, "json").exists(), "{} left behind", stem.display());
        }
        // user_config stayed JSON through every document progress tick, and only the final
        // tick sees it converted.
        let seen = json_seen_at_progress.borrow();
        let (last, rest) = seen.split_last().ok_or("no progress")?;
        assert_eq!((last.0, last.1, last.2), (8, 8, false));
        assert!(rest.iter().all(|(_, total, json)| *total == 8 && *json));
        let user_config = DocRef::new(&fx.user_config, DocKind::UserConfig);
        assert_eq!(ms_docstore::actual_format(&user_config)?, Some(DocFormat::Db));
        let value = ms_docstore::read_value(&user_config)?.ok_or("user config missing")?;
        assert_eq!(value["General"]["storage_mode"], json!("prod"));
        assert_eq!(value["General"]["theme"], json!("dark"));
        // Chapters are untouched.
        assert!(fx.root.join("Title B").join("Chapter 1").join("bubbles.json").is_file());
        Ok(())
    }

    #[test]
    fn a_failed_document_keeps_the_sentinel_pending_and_a_rerun_converges() -> TestResult {
        let fx = fixture()?;
        let broken = fx.root.join("Title A").join("terms.json");
        std::fs::write(&broken, b"{ broken")?;
        let outcome = convert_globals_with(StorageMode::Prod, &fx.root, &fx.fonts, &fx.user_config, &|_| {}, &|_, _| {});
        assert!(outcome.mode_persisted);
        assert!(!outcome.sentinel_settled);
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, fx.root.join("Title A").join("terms"));
        // The mode is recorded, but user_config is still JSON: the next start re-runs.
        assert!(fx.user_config.is_file());
        let probe = ms_config::storage_mode::probe_storage_mode(&DocRef::new(&fx.user_config, DocKind::UserConfig));
        assert_eq!(probe.mode, StorageMode::Prod);
        assert!(probe.reconciliation_pending);
        // The broken document is untouched; the others are converted.
        assert_eq!(std::fs::read(&broken)?, b"{ broken");
        assert!(fx.fonts.join("fonts_data.db").is_file());

        std::fs::write(&broken, b"[]")?;
        let rerun = convert_globals_with(StorageMode::Prod, &fx.root, &fx.fonts, &fx.user_config, &|_| {}, &|_, _| {});
        assert!(rerun.is_complete(), "{rerun:?}");
        assert_eq!(rerun.converted, 2, "only terms + user_config are left");
        assert!(!fx.user_config.exists());
        Ok(())
    }

    #[test]
    fn is_idempotent_and_converts_back() -> TestResult {
        let fx = fixture()?;
        let first = convert_globals_with(StorageMode::Prod, &fx.root, &fx.fonts, &fx.user_config, &|_| {}, &|_, _| {});
        assert!(first.is_complete());
        let second = convert_globals_with(StorageMode::Prod, &fx.root, &fx.fonts, &fx.user_config, &|_| {}, &|_, _| {});
        assert!(second.is_complete());
        assert_eq!((second.converted, second.skipped), (0, 8));

        let back = convert_globals_with(StorageMode::Dev, &fx.root, &fx.fonts, &fx.user_config, &|_| {}, &|_, _| {});
        assert!(back.is_complete(), "{back:?}");
        for stem in doc_files(&fx) {
            assert!(with_ext(&stem, "json").is_file());
            assert!(!with_ext(&stem, "db").exists());
        }
        let value: Value = serde_json::from_slice(&std::fs::read(&fx.user_config)?)?;
        assert_eq!(value["General"]["storage_mode"], json!("dev"));
        Ok(())
    }

    #[test]
    fn an_unwritable_mode_converts_nothing() -> TestResult {
        let fx = fixture()?;
        std::fs::write(&fx.user_config, b"{ broken")?;
        let calls = CallCount::default();
        let outcome = convert_globals_with(StorageMode::Prod, &fx.root, &fx.fonts, &fx.user_config, &|_| calls.0.set(calls.0.get() + 1), &|_, _| {});
        assert!(!outcome.mode_persisted);
        assert_eq!(calls.0.get(), 0, "the default must not switch");
        assert_eq!(outcome.failed.len(), 1);
        for stem in doc_files(&fx) {
            assert!(with_ext(&stem, "json").is_file());
        }
        Ok(())
    }

    #[test]
    fn a_missing_projects_root_has_no_titles() -> TestResult {
        let temp = tempfile::tempdir()?;
        let (docs, err) = global_documents(&temp.path().join("absent"), &temp.path().join("fonts"));
        assert!(err.is_none());
        assert_eq!(docs.iter().map(DocRef::kind).collect::<Vec<_>>(), vec![DocKind::FontsData, DocKind::Presets]);
        Ok(())
    }
}
