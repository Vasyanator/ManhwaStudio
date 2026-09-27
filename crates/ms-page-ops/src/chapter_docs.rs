/*
File: crates/ms-page-ops/src/chapter_docs.rs

Purpose:
The chapter's two OWNED documents (`layers/layers` and the bubbles document) named as
`ms_docstore::DocRef`s, and the one place that applies rule B.3 of the document store to
them: a NEW chapter document is created in the format the chapter already uses (first
existing sibling in the order committed bubbles, committed layers, staging bubbles, staging
layers), and only a chapter with no document at all takes `default_format()`. A JSON chapter
therefore stays JSON in Prod mode until the user converts it.

Key functions:
- chapter_trees(): committed + `_unsaved` staging tree dirs of the chapter a tree dir belongs to.
- chapter_doc_siblings(): the four sibling documents in rule-B.3 order.
- chapter_doc_for_write(): the `DocRef` every writer of a chapter document uses (carries the
  new-document format hint); derives the chapter from the document path alone.
- chapter_doc_durability(): the one staging-vs-committed durability decision for such a write
  (`_unsaved` staging → `Durability::None`, committed → the caller's durability).
- ProjectPaths::chapter_doc_siblings(): the same four documents from a loaded chapter.

Notes:
Lives in this crate because `ProjectPaths` (the chapter layout) is declared here and both
`ms-project` (the save merge) and `ms-models` (the staging savers) sit above it. The
`{chapter}_unsaved` naming is the layout `ms-project` builds `ProjectPaths` with.
Pure path logic plus `ms_docstore::chapter_new_format` (a few `stat`s); no parsing.
*/

use crate::ProjectPaths;
use ms_docstore::{DocKind, DocRef, Durability};
use std::path::{Path, PathBuf};

/// Suffix of a chapter's staging mirror directory: `{title}/{chapter}_unsaved/`.
pub const UNSAVED_DIR_SUFFIX: &str = "_unsaved";

/// File name (with the `.json` extension; the store strips it) of the layer manifest inside
/// a tree's `layers/` directory.
const LAYERS_MANIFEST_FILE: &str = "layers.json";

/// The layer manifest document of the chapter tree `tree_dir` (committed or staging).
#[must_use]
pub fn layers_doc(tree_dir: &Path) -> DocRef {
    DocRef::new(tree_dir.join(ms_config::LAYERS_DIR).join(LAYERS_MANIFEST_FILE), DocKind::Layers)
}

/// The bubbles document of the chapter tree `tree_dir` (committed or staging).
#[must_use]
pub fn bubbles_doc(tree_dir: &Path) -> DocRef {
    DocRef::new(tree_dir.join(ms_config::BUBBLES_FILE), DocKind::Bubbles)
}

/// `(committed_dir, unsaved_dir)` of the chapter that the tree directory `tree_dir` belongs
/// to: a `{chapter}_unsaved` dir is the staging tree of its sibling `{chapter}`, any other
/// dir is a committed tree whose staging mirror is the sibling `{name}_unsaved`. `None` when
/// `tree_dir` has no parent or no UTF-8 file name (no chapter layout can be derived).
#[must_use]
pub fn chapter_trees(tree_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let parent = tree_dir.parent()?;
    let name = tree_dir.file_name()?.to_str()?;
    match name.strip_suffix(UNSAVED_DIR_SUFFIX) {
        Some(chapter) if !chapter.is_empty() => Some((parent.join(chapter), tree_dir.to_path_buf())),
        _ => Some((tree_dir.to_path_buf(), parent.join(format!("{name}{UNSAVED_DIR_SUFFIX}")))),
    }
}

/// The chapter's four owned documents in the rule-B.3 sibling order: committed bubbles,
/// committed layers, staging bubbles, staging layers. Pass the result to
/// `ms_docstore::chapter_new_format`.
#[must_use]
pub fn chapter_doc_siblings(committed_dir: &Path, unsaved_dir: &Path) -> [DocRef; 4] {
    [bubbles_doc(committed_dir), layers_doc(committed_dir), bubbles_doc(unsaved_dir), layers_doc(unsaved_dir)]
}

/// The chapter tree directory holding the chapter document at `path` (a file path or stem):
/// the parent of the bubbles document, the parent of the `layers/` dir for the manifest.
/// `None` for a path outside the chapter layout (e.g. a manifest not inside a `layers/` dir,
/// as unit tests of the savers use) and for non-chapter kinds.
fn tree_dir_of(path: &Path, kind: DocKind) -> Option<&Path> {
    match kind {
        DocKind::Bubbles => path.parent(),
        DocKind::Layers => {
            let layers_dir = path.parent()?;
            if layers_dir.file_name()? != std::ffi::OsStr::new(ms_config::LAYERS_DIR) {
                return None;
            }
            layers_dir.parent()
        }
        DocKind::UserConfig
        | DocKind::FontsData
        | DocKind::Presets
        | DocKind::ProjectSettings
        | DocKind::Characters
        | DocKind::Terms
        | DocKind::CharFavorites
        | DocKind::ColorPresets => None,
    }
}

/// The `DocRef` a WRITER of the chapter document at `path` (either tree, either format's file
/// name or the bare stem) must use: an existing document keeps its format (the store ignores
/// the hint then), a NEW one is created in the chapter's format per rule B.3
/// (`ms_docstore::chapter_new_format` over [`chapter_doc_siblings`]). When the chapter layout
/// cannot be derived from `path` (see [`tree_dir_of`]) the plain `DocRef` is returned and a
/// new document takes `default_format()`.
///
/// Costs up to eight `stat`s; call it on a worker thread like every other document write.
#[must_use]
pub fn chapter_doc_for_write(path: &Path, kind: DocKind) -> DocRef {
    let doc = DocRef::new(path, kind);
    let Some((committed, unsaved)) = tree_dir_of(path, kind).and_then(chapter_trees) else { return doc };
    doc.with_new_format(ms_docstore::chapter_new_format(&chapter_doc_siblings(&committed, &unsaved)))
}

/// Whether the chapter document at `path` lives in a `{chapter}_unsaved` staging tree (see
/// [`chapter_trees`] for the naming rule). `false` outside the chapter layout.
fn is_staging_doc(path: &Path, kind: DocKind) -> bool {
    tree_dir_of(path, kind)
        .and_then(Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .and_then(|name| name.strip_suffix(UNSAVED_DIR_SUFFIX))
        .is_some_and(|chapter| !chapter.is_empty())
}

/// The ONE durability decision for a write of the chapter document at `path` (same path forms as
/// [`chapter_doc_for_write`]): a document in the `{chapter}_unsaved` staging tree is written with
/// `Durability::None` — still an atomic temp+rename, never torn, but not fsynced: staging is scratch
/// data rewritten on every edit, and a crash can lose at most its last write. Any other document
/// (the committed tree, or a path outside the chapter layout) gets `committed`, the caller's
/// durability for the committed tree. Only JSON honours it; a `.db` document ignores `Durability`
/// by design (ms-docstore), so this never weakens a `.db` write.
#[must_use]
pub fn chapter_doc_durability(path: &Path, kind: DocKind, committed: Durability) -> Durability {
    if is_staging_doc(path, kind) { Durability::None } else { committed }
}

impl ProjectPaths {
    /// The loaded chapter's four owned documents in rule-B.3 order (see
    /// [`chapter_doc_siblings`]).
    #[must_use]
    pub fn chapter_doc_siblings(&self) -> [DocRef; 4] {
        [
            DocRef::new(&self.bubbles_file, DocKind::Bubbles),
            DocRef::new(self.layers_dir.join(LAYERS_MANIFEST_FILE), DocKind::Layers),
            DocRef::new(&self.unsaved_bubbles_file, DocKind::Bubbles),
            DocRef::new(self.unsaved_layers_dir.join(LAYERS_MANIFEST_FILE), DocKind::Layers),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ms_docstore::DocFormat;

    /// Creates the `format` file of `doc` (content is irrelevant: B.3 only stats files).
    fn touch(doc: &DocRef, format: DocFormat) {
        let path = doc.path_for(format);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, b"[]").expect("write");
    }

    #[test]
    fn chapter_trees_maps_both_directions() {
        let (committed, unsaved) = chapter_trees(Path::new("/t/ch1")).expect("layout");
        assert_eq!((committed.as_path(), unsaved.as_path()), (Path::new("/t/ch1"), Path::new("/t/ch1_unsaved")));
        let (committed, unsaved) = chapter_trees(Path::new("/t/ch1_unsaved")).expect("layout");
        assert_eq!((committed.as_path(), unsaved.as_path()), (Path::new("/t/ch1"), Path::new("/t/ch1_unsaved")));
        // A dir literally named `_unsaved` is a committed chapter of that name.
        let (committed, unsaved) = chapter_trees(Path::new("/t/_unsaved")).expect("layout");
        assert_eq!((committed.as_path(), unsaved.as_path()), (Path::new("/t/_unsaved"), Path::new("/t/_unsaved_unsaved")));
    }

    #[test]
    fn new_staging_doc_joins_a_json_chapter() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let committed = tmp.path().join("ch1");
        touch(&layers_doc(&committed), DocFormat::Json);
        let staging_bubbles = tmp.path().join("ch1_unsaved").join(ms_config::BUBBLES_FILE);
        let doc = chapter_doc_for_write(&staging_bubbles, DocKind::Bubbles);
        // The hint overrides whatever the process default is (Prod = Db included).
        assert_eq!(doc.new_format_hint(), Some(DocFormat::Json));
    }

    #[test]
    fn new_staging_doc_joins_a_db_chapter_and_is_created_as_db() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let committed = tmp.path().join("ch1");
        touch(&bubbles_doc(&committed), DocFormat::Db);
        let staging_layers = tmp.path().join("ch1_unsaved").join("layers").join("layers.json");
        let doc = chapter_doc_for_write(&staging_layers, DocKind::Layers);
        assert_eq!(doc.new_format_hint(), Some(DocFormat::Db));
        ms_docstore::write_value(&doc, &serde_json::json!({"pages": []}), ms_docstore::WriteOptions::default()).expect("write");
        assert!(doc.path_for(DocFormat::Db).is_file());
        assert!(!doc.path_for(DocFormat::Json).exists());
    }

    #[test]
    fn sibling_order_committed_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let committed = tmp.path().join("ch1");
        let unsaved = tmp.path().join("ch1_unsaved");
        touch(&layers_doc(&committed), DocFormat::Db);
        touch(&bubbles_doc(&unsaved), DocFormat::Json);
        let doc = chapter_doc_for_write(&bubbles_doc(&committed).path_for(DocFormat::Json), DocKind::Bubbles);
        assert_eq!(doc.new_format_hint(), Some(DocFormat::Db));
    }

    #[test]
    fn brand_new_chapter_takes_the_default_format() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let doc = chapter_doc_for_write(&tmp.path().join("ch1").join(ms_config::BUBBLES_FILE), DocKind::Bubbles);
        assert_eq!(doc.new_format_hint(), Some(ms_docstore::default_format()));
    }

    #[test]
    fn staging_docs_are_not_fsynced_committed_docs_keep_their_durability() {
        let staging_layers = Path::new("/t/ch1_unsaved/layers/layers.json");
        let staging_bubbles = Path::new("/t/ch1_unsaved").join(ms_config::BUBBLES_FILE);
        assert_eq!(chapter_doc_durability(staging_layers, DocKind::Layers, Durability::Contents), Durability::None);
        assert_eq!(chapter_doc_durability(&staging_bubbles, DocKind::Bubbles, Durability::ContentsAndDirectory), Durability::None);
        let committed_layers = Path::new("/t/ch1/layers/layers.db");
        assert_eq!(chapter_doc_durability(committed_layers, DocKind::Layers, Durability::Contents), Durability::Contents);
        // A chapter literally named `_unsaved` is committed; a manifest outside `layers/` is not a chapter doc.
        assert_eq!(chapter_doc_durability(Path::new("/t/_unsaved/layers/layers.json"), DocKind::Layers, Durability::Contents), Durability::Contents);
        assert_eq!(chapter_doc_durability(Path::new("/t/ch1_unsaved/layers.json"), DocKind::Layers, Durability::Contents), Durability::Contents);
    }

    #[test]
    fn manifest_outside_a_layers_dir_gets_no_hint() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let doc = chapter_doc_for_write(&tmp.path().join("layers.json"), DocKind::Layers);
        assert_eq!(doc.new_format_hint(), None);
    }
}
