/*
File: android/saf.rs

Purpose:
The pure half of the Storage Access Framework bridge: the bridge class name, the wire encoding
shared with the Kotlin `SafBridge` (dialog kind codes, result status codes, tree listings), the
registry of requests waiting for a result, and the naming rules of the local staging copies.

Key structures:
- PendingRegistry<T>: `id -> T` slots with monotonically increasing `u64` ids.
- BridgeOutcome / PickedDocument: a decoded `SafBridge.nativeOnResult` report.
- TreeEntry / PlannedCopy: a decoded `SafBridge.listTree` answer and its local copy plan.

Key functions:
- kind_code(): `DialogKind` -> the `kind` int Kotlin switches on.
- create_document_mime(): the MIME type of a created document.
- decode_outcome(), decode_tree_listing(): wire -> typed values, rejecting malformed input.
- sanitize_file_name(), unique_name(), name_with_default_extension(), staging_file_name(),
  plan_file_copies(), plan_tree_copy(): local names for provider display names.

Notes:
Pure Rust (no JNI), compiled for host tests too; the JVM half lives in
`file_dialog/android.rs`. The wire constants here and in `SafBridge.kt` must change together.
Provider display names are untrusted input: every name that becomes a local path component goes
through `sanitize_file_name` (no separators, no `.`/`..`, at most 255 bytes) and is made unique
inside its directory, so a picked document can never escape its staging directory.
*/

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::file_dialog::{DialogKind, FileDialogRequest, mime_types_for};

/// Binary name of the Kotlin SAF bridge class, loaded through the application class loader.
/// The single owner of this name; the Gradle project must declare the class under it.
#[cfg(target_os = "android")]
pub(crate) const SAF_BRIDGE_CLASS: &jni::strings::JNIStr = jni::jni_str!("io.github.vasyanator.manhwastudio.platform.SafBridge");

/// `nativeOnResult` status: the user picked at least one document.
pub(crate) const STATUS_PICKED: i32 = 0;
/// `nativeOnResult` status: the user closed the picker without a choice.
pub(crate) const STATUS_CANCELLED: i32 = 1;
/// `nativeOnResult` status: the picker could not be shown or its result not read.
pub(crate) const STATUS_FAILED: i32 = 2;

/// Longest file name (bytes) the app-storage file systems (ext4 / f2fs) accept.
const MAX_NAME_BYTES: usize = 255;
/// Longest extension kept intact when a name must be shortened.
const MAX_KEPT_EXTENSION_BYTES: usize = 16;
/// Local name for a document whose display name sanitizes to nothing.
const UNNAMED: &str = "unnamed";

/// The `kind` code `SafBridge.launch` switches on (the `DialogKind` declaration order, which is
/// append-only for this reason). `None` for `PickDirectoryPath`: SAF has no directory paths, so
/// that kind never reaches the bridge.
pub(crate) const fn kind_code(kind: DialogKind) -> Option<i32> {
    match kind {
        DialogKind::OpenFile => Some(0),
        DialogKind::OpenFiles => Some(1),
        DialogKind::OpenFolder => Some(2),
        DialogKind::SaveFile => Some(3),
        DialogKind::SaveIntoFolder => Some(4),
        DialogKind::PickDirectoryPath => None,
    }
}

/// One document the user picked: its content URI and the name the picker showed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PickedDocument {
    pub(crate) uri: String,
    pub(crate) name: String,
}

/// A decoded `nativeOnResult` report.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BridgeOutcome {
    /// At least one document (one for every kind but `OpenFiles`; a tree URI for the folder kinds).
    Picked(Vec<PickedDocument>),
    /// The user closed the picker.
    Cancelled,
    /// The bridge failed; the text is the technical detail.
    Failed(String),
}

/// Decodes the arguments of `SafBridge.nativeOnResult`. A `STATUS_PICKED` report with no
/// documents or with `uris` / `names` of different lengths is malformed and decodes to `Failed`,
/// as does an unknown status.
pub(crate) fn decode_outcome(status: i32, uris: Vec<String>, names: Vec<String>, error: Option<String>) -> BridgeOutcome {
    match status {
        STATUS_PICKED => {
            if uris.is_empty() || uris.len() != names.len() {
                return BridgeOutcome::Failed(format!("malformed picker result: {} uris, {} names", uris.len(), names.len()));
            }
            BridgeOutcome::Picked(uris.into_iter().zip(names).map(|(uri, name)| PickedDocument { uri, name }).collect())
        }
        STATUS_CANCELLED => BridgeOutcome::Cancelled,
        STATUS_FAILED => BridgeOutcome::Failed(error.unwrap_or_else(|| "the file picker reported an unspecified failure".to_owned())),
        other => BridgeOutcome::Failed(format!("unknown picker result status {other}")),
    }
}

/// One node of a picked document tree, as `SafBridge.listTree` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeEntry {
    pub(crate) is_dir: bool,
    /// Index of the parent directory entry; `None` = directly inside the picked tree.
    pub(crate) parent: Option<usize>,
    /// Raw provider display name (untrusted).
    pub(crate) name: String,
    pub(crate) uri: String,
}

/// Decodes a `SafBridge.listTree` answer: `["ok", (kind, parent, name, uri)*]` with kind `d` /
/// `f` and parent the decimal index of an EARLIER directory entry or `-1`; or `["err", detail]`.
///
/// # Errors
/// The bridge's own detail for `err`, or a description of the malformed encoding.
pub(crate) fn decode_tree_listing(raw: Vec<String>) -> Result<Vec<TreeEntry>, String> {
    let mut items = raw.into_iter();
    match items.next().as_deref() {
        Some("ok") => {}
        Some("err") => return Err(items.next().unwrap_or_else(|| "the folder could not be listed".to_owned())),
        other => return Err(format!("malformed folder listing header {other:?}")),
    }
    let rest: Vec<String> = items.collect();
    if !rest.len().is_multiple_of(4) {
        return Err(format!("malformed folder listing: {} fields is not a multiple of 4", rest.len()));
    }
    let mut entries: Vec<TreeEntry> = Vec::with_capacity(rest.len() / 4);
    for chunk in rest.chunks_exact(4) {
        let index = entries.len();
        let is_dir = match chunk[0].as_str() {
            "d" => true,
            "f" => false,
            other => return Err(format!("malformed folder listing: entry {index} has kind {other:?}")),
        };
        let parent = match chunk[1].as_str() {
            "-1" => None,
            text => {
                let parent: usize = text.parse().map_err(|_| format!("malformed folder listing: entry {index} has parent {text:?}"))?;
                // Parents come first, so the copy can create every directory before its children.
                if parent >= index || !entries[parent].is_dir {
                    return Err(format!("malformed folder listing: entry {index} names parent {parent}, not an earlier folder"));
                }
                Some(parent)
            }
        };
        entries.push(TreeEntry { is_dir, parent, name: chunk[2].clone(), uri: chunk[3].clone() });
    }
    Ok(entries)
}

/// One step of copying a picked tree: create `relative` (a directory) or copy `uri` to it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PlannedCopy {
    /// Path relative to the local copy of the tree root; every component is sanitized.
    pub(crate) relative: PathBuf,
    pub(crate) is_dir: bool,
    pub(crate) uri: String,
}

/// Local paths for every tree entry, in listing order (parents before children). Names are
/// sanitized and made unique per directory.
pub(crate) fn plan_tree_copy(entries: &[TreeEntry]) -> Vec<PlannedCopy> {
    let mut root_names: BTreeSet<String> = BTreeSet::new();
    // Per entry: the names taken inside it (only directories ever get children).
    let mut child_names: Vec<BTreeSet<String>> = vec![BTreeSet::new(); entries.len()];
    let mut plan: Vec<PlannedCopy> = Vec::with_capacity(entries.len());
    for entry in entries {
        let (taken, base) = match entry.parent {
            None => (&mut root_names, PathBuf::new()),
            // `decode_tree_listing` guarantees the parent is an earlier entry, so its plan exists.
            Some(parent) => (&mut child_names[parent], plan[parent].relative.clone()),
        };
        let name = unique_name(taken, &sanitize_file_name(&entry.name));
        plan.push(PlannedCopy { relative: base.join(name), is_dir: entry.is_dir, uri: entry.uri.clone() });
    }
    plan
}

/// Local file names for picked documents copied into one directory: sanitized and unique.
pub(crate) fn plan_file_copies(documents: &[PickedDocument]) -> Vec<String> {
    let mut taken: BTreeSet<String> = BTreeSet::new();
    documents.iter().map(|document| unique_name(&mut taken, &sanitize_file_name(&document.name))).collect()
}

/// A provider display name made safe as ONE local path component: path separators, NUL and
/// control characters become `_`, surrounding whitespace is trimmed, `.` / `..` / empty become
/// `unnamed`, and the name is shortened to 255 bytes, keeping a short extension.
pub(crate) fn sanitize_file_name(raw: &str) -> String {
    let replaced: String = raw.chars().map(|ch| if ch == '/' || ch == '\\' || ch.is_control() { '_' } else { ch }).collect();
    let trimmed = replaced.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return UNNAMED.to_owned();
    }
    shorten_to_limit(trimmed)
}

/// `name` shortened to [`MAX_NAME_BYTES`] at a char boundary, keeping its extension when that
/// is at most [`MAX_KEPT_EXTENSION_BYTES`] long.
fn shorten_to_limit(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_owned();
    }
    let (stem, extension) = split_extension(name);
    let suffix = match extension {
        Some(extension) if extension.len() < MAX_KEPT_EXTENSION_BYTES => format!(".{extension}"),
        Some(_) | None => String::new(),
    };
    let source = if suffix.is_empty() { name } else { stem };
    let budget = MAX_NAME_BYTES - suffix.len();
    let mut end = budget.min(source.len());
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &source[..end])
}

/// `(stem, extension)` of a file name; a leading dot (`.hidden`) or a trailing dot is not an
/// extension separator.
fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot + 1 < name.len() => (&name[..dot], Some(&name[dot + 1..])),
        Some(_) | None => (name, None),
    }
}

/// The extension of `name`, if it has one (see `split_extension`).
pub(crate) fn extension_of(name: &str) -> Option<&str> {
    split_extension(name).1
}

/// `name` itself when it is free in `taken`, else `stem (2).ext`, `stem (3).ext`, ...; the
/// returned name is recorded in `taken`.
pub(crate) fn unique_name(taken: &mut BTreeSet<String>, name: &str) -> String {
    let mut candidate = name.to_owned();
    let mut counter: u64 = 2;
    while taken.contains(&candidate) {
        let (stem, extension) = split_extension(name);
        candidate = match extension {
            Some(extension) => format!("{stem} ({counter}).{extension}"),
            None => format!("{stem} ({counter})"),
        };
        counter += 1;
    }
    taken.insert(candidate.clone());
    candidate
}

/// The MIME type of a document created for a name with `extension`: its well-known type
/// (`mime_types_for`), else `application/octet-stream`, which providers store under the exact
/// name given (a known type may make a provider append its own extension).
pub(crate) fn create_document_mime(extension: Option<&str>) -> &'static str {
    const OCTET_STREAM: &str = "application/octet-stream";
    let Some(extension) = extension.filter(|extension| !extension.trim().is_empty()) else {
        return OCTET_STREAM;
    };
    let request = FileDialogRequest::new(DialogKind::SaveFile).filter("", &[extension]);
    match mime_types_for(request.filters()).first() {
        Some(&"*/*") | None => OCTET_STREAM,
        Some(&mime) => mime,
    }
}

/// The suggested `ACTION_CREATE_DOCUMENT` name: `name` with `.default_extension` appended when
/// the name has no extension at all (the desktop `with_default_extension` rule). An empty name
/// stays empty: the picker then asks the user for one.
pub(crate) fn name_with_default_extension(name: &str, default_extension: Option<&str>) -> String {
    match default_extension {
        Some(extension) if !name.trim().is_empty() && extension_of(name).is_none() => format!("{name}.{extension}"),
        Some(_) | None => name.to_owned(),
    }
}

/// The local staging file name for a created document. It is the sanitized name the provider
/// actually gave the document (it may differ from the suggestion: renamed by the user,
/// de-duplicated as `x (1).png`, or extended to match the MIME type); when that name has no
/// extension, `fallback_extension` (the requested one) is appended, because callers choose
/// the output format from the extension of the path they write to.
pub(crate) fn staging_file_name(display_name: &str, fallback_extension: Option<&str>) -> String {
    let sanitized = sanitize_file_name(display_name);
    match fallback_extension {
        Some(extension) if extension_of(&sanitized).is_none() => shorten_to_limit(&format!("{sanitized}.{extension}")),
        Some(_) | None => sanitized,
    }
}

/// Requests waiting for an asynchronous result, keyed by a process-unique id.
pub(crate) struct PendingRegistry<T> {
    next_id: AtomicU64,
    slots: Mutex<std::collections::BTreeMap<u64, T>>,
}

impl<T> PendingRegistry<T> {
    /// An empty registry; `const` so it can back a `static`.
    pub(crate) const fn new() -> Self {
        Self { next_id: AtomicU64::new(1), slots: Mutex::new(std::collections::BTreeMap::new()) }
    }

    /// Stores `value` and returns its id (never 0, never reused within the process).
    pub(crate) fn register(&self, value: T) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(id, value);
        id
    }

    /// Removes and returns the value registered under `id`; `None` when it was already taken
    /// or withdrawn (a late result).
    pub(crate) fn take(&self, id: u64) -> Option<T> {
        self.lock().remove(&id)
    }

    /// Whether `id` is still registered (not yet taken or withdrawn).
    pub(crate) fn contains(&self, id: u64) -> bool {
        self.lock().contains_key(&id)
    }

    /// Number of waiting requests.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<u64, T>> {
        // A panic while holding the lock can only interrupt a single map insert/remove, which
        // leaves the map itself consistent, so a poisoned lock is still safe to use.
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests;
