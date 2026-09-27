/*
File: convert.rs

Purpose:
Format conversion of documents (plan §B "Conversion of one document") and the chapter
format report. Title/global enumeration, the mode key and driver ORDER live above this
crate (ms-config / the binary); this module converts the documents it is handed.

Key structures:
- ConvertStep / ConvertHook : the five protocol steps and an observer called after each
                              (tests inject "crashes" by returning an error)
- ConvertOutcome            : Converted / Reconciled / Skipped / Absent
- BatchOutcome              : aggregate of `convert_many`
- ChapterFormatReport       : which formats a chapter's committed/staging documents use

Key functions:
- convert_document() : one document, under its lock for the whole protocol
- convert_many()     : a list, continuing past per-document failures, with progress
- chapter_format_report()

Protocol (lock held throughout; see the crate MODULE_README "Conversion"):
  0. resolve under the lock (runs the B.4 repair when both files exist);
     already in `target` -> Skipped (Reconciled when the repair finished a crashed run)
  1. read the source document (Malformed -> abort, nothing touched)       -> ReadSource
  2. write it whole into a fresh temp in the target format                 -> TempWritten
  3. reopen the temp, require a Value-equal document (else delete temp)    -> TempValidated
  4. fsync + rename the temp over `<stem>.<target ext>` + directory fsync  -> Renamed
  5. delete the source file (and a `.db` source's journal)                 -> SourceRemoved
A crash before 4 leaves only the source (+ a temp the next run replaces); a crash between
4 and 5 leaves both files, which rule B.4 repairs on the next locked access PROVIDED
`default_format()` already equals `target` (drivers persist the mode first). The source
is never deleted before the target is durable and validated.
*/

use std::path::PathBuf;

use crate::{DocFormat, DocKind, DocRef, DocStoreError, Result, lock, resolve};

/// One step of the conversion protocol, reported to a [`ConvertHook`] after it completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertStep {
    /// The source document was read and parsed.
    ReadSource,
    /// The temp file in the target format is written and closed.
    TempWritten,
    /// The temp file was reopened and holds a Value-equal document.
    TempValidated,
    /// The temp file was renamed to the target name (both formats now exist).
    Renamed,
    /// The source file was deleted; the conversion is complete.
    SourceRemoved,
}

/// Observer of [`convert_document`]'s steps. Returning `Err` aborts the conversion right
/// there WITHOUT any cleanup, exactly like a crash at that point (test failpoints); the
/// error is reported as `DocStoreError::Io` with kind `Interrupted`.
pub trait ConvertHook {
    /// Called after `step` completed for `doc`.
    ///
    /// # Errors
    /// A message; the conversion stops as if the process died after `step`.
    fn after_step(&self, doc: &DocRef, step: ConvertStep) -> Result<(), String>;
}

/// The hook of production callers: observes nothing, never interrupts.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoHook;

impl ConvertHook for NoHook {
    fn after_step(&self, _doc: &DocRef, _step: ConvertStep) -> Result<(), String> {
        Ok(())
    }
}

/// Result of converting one document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvertOutcome {
    /// The document was rewritten in the target format and the source removed.
    Converted,
    /// Both formats existed (an interrupted earlier run); the B.4 repair kept the target
    /// file and removed the Value-equal leftover.
    Reconciled,
    /// Already in the target format; nothing was written.
    Skipped,
    /// The document does not exist in any format; nothing was written.
    Absent,
}

/// Aggregate of [`convert_many`].
#[derive(Debug, Default)]
pub struct BatchOutcome {
    /// Documents rewritten or reconciled into the target format.
    pub converted: usize,
    /// Documents already in the target format, or absent.
    pub skipped: usize,
    /// Documents that failed, with the file that failed and why; each is left readable in
    /// its previous format (or both formats, when `Ambiguous`).
    pub failed: Vec<(PathBuf, DocStoreError)>,
}

/// Converts `doc` into `target` following the protocol in the file header, holding the
/// document lock throughout. Idempotent: a document already in `target` is `Skipped`, and
/// a document left in both formats by an interrupted run is finished (`Reconciled`) when
/// `default_format() == target`.
///
/// # Errors
/// `Malformed` (source does not parse; nothing touched), `Ambiguous` (both exist and
/// differ; nothing deleted), `Unsupported` (`Db` on wasm32), `Write`/`Io`/`Storage` on I/O
/// failure (source untouched unless the error is `Write(DirSync)` after the rename), and
/// `Io` with kind `Interrupted` when `hook` stopped it.
pub fn convert_document(doc: &DocRef, target: DocFormat, hook: &dyn ConvertHook) -> Result<ConvertOutcome> {
    resolve::ensure_supported(doc, target)?;
    lock::with_document_lock(doc.stem(), || convert_locked(doc, target, hook))
}

/// Body of [`convert_document`]; caller holds the lock.
fn convert_locked(doc: &DocRef, target: DocFormat, hook: &dyn ConvertHook) -> Result<ConvertOutcome> {
    let both_before = doc_has_both(doc);
    let Some(source) = resolve::detect(doc)? else { return Ok(ConvertOutcome::Absent) };
    // Resolution under the lock runs the B.4 repair when both files exist.
    let source = if both_before { resolve::resolve_locked(doc)? } else { source };
    if source == target {
        return Ok(if both_before { ConvertOutcome::Reconciled } else { ConvertOutcome::Skipped });
    }
    run_protocol(doc, source, target, hook)?;
    ms_log::runtime_log::log_info(format!("docstore: converted {} from .{} to .{}", doc.stem().display(), source.extension(), target.extension()));
    Ok(ConvertOutcome::Converted)
}

/// Whether both formats' files of `doc` exist (stat only).
fn doc_has_both(doc: &DocRef) -> bool {
    resolve::known_formats().iter().all(|format| crate::fsio::exists(&doc.path_for(*format)))
}

/// Steps 1-5 (native).
#[cfg(not(target_arch = "wasm32"))]
fn run_protocol(doc: &DocRef, source: DocFormat, target: DocFormat, hook: &dyn ConvertHook) -> Result<()> {
    let step = |step: ConvertStep| {
        hook.after_step(doc, step).map_err(|message| DocStoreError::Io {
            path: doc.path_for(target),
            source: std::io::Error::new(std::io::ErrorKind::Interrupted, format!("conversion interrupted after {step:?}: {message}")),
        })
    };
    let value = crate::codec::read_value_at(doc, source)?.ok_or_else(|| DocStoreError::Storage(crate::StorageError::NotFound(doc.path_for(source).display().to_string())))?;
    step(ConvertStep::ReadSource)?;
    let temp = crate::whole::write_temp(doc, &value, target, false)?;
    step(ConvertStep::TempWritten)?;
    crate::whole::validate_temp(&temp, target, &value)?;
    step(ConvertStep::TempValidated)?;
    crate::whole::commit_temp(doc, &temp, target)?;
    step(ConvertStep::Renamed)?;
    crate::codec::remove_format(doc, source)?;
    step(ConvertStep::SourceRemoved)
}

/// wasm32: only `Json -> Json` could get here, and that is `Skipped` above.
#[cfg(target_arch = "wasm32")]
fn run_protocol(doc: &DocRef, _source: DocFormat, target: DocFormat, _hook: &dyn ConvertHook) -> Result<()> {
    Err(DocStoreError::Unsupported { path: doc.path_for(target), format: target })
}

/// Converts every document of `docs` into `target` with [`NoHook`], continuing past
/// per-document failures. `progress(done, total)` is called after each document (on the
/// calling thread; run this on a worker, never on the GUI thread).
#[must_use]
pub fn convert_many(docs: &[DocRef], target: DocFormat, progress: &dyn Fn(usize, usize)) -> BatchOutcome {
    let mut outcome = BatchOutcome::default();
    for (index, doc) in docs.iter().enumerate() {
        match convert_document(doc, target, &NoHook) {
            Ok(ConvertOutcome::Converted | ConvertOutcome::Reconciled) => outcome.converted += 1,
            Ok(ConvertOutcome::Skipped | ConvertOutcome::Absent) => outcome.skipped += 1,
            Err(err) => {
                ms_log::runtime_log::log_error(format!("docstore: format conversion failed; the document stays in its previous format.\nDocument: {}\nTarget: .{}\nError: {err}", doc.stem().display(), target.extension()));
                outcome.failed.push((doc.stem().to_path_buf(), err));
            }
        }
        progress(index + 1, docs.len());
    }
    outcome
}

/// Which format each EXISTING document of a chapter's committed and staging trees has.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ChapterFormatReport {
    /// `(kind, format)` of every existing committed document, in the order given.
    pub committed: Vec<(DocKind, DocFormat)>,
    /// `(kind, format)` of every existing staging (`_unsaved`) document.
    pub staging: Vec<(DocKind, DocFormat)>,
    /// Documents whose format could not be determined (e.g. a `.db` failing the header
    /// sniff), with the reason; they are not in the two lists above.
    pub unreadable: Vec<(PathBuf, String)>,
}

impl ChapterFormatReport {
    /// Whether any existing document is not in `target` (or could not be probed), i.e.
    /// whether offering a conversion to `target` makes sense.
    #[must_use]
    pub fn needs_conversion(&self, target: DocFormat) -> bool {
        !self.unreadable.is_empty() || self.committed.iter().chain(&self.staging).any(|(_, format)| *format != target)
    }
}

/// Probes the formats of a chapter's documents (stat + header sniff only; no parse, no
/// lock). Absent documents are omitted.
#[must_use]
pub fn chapter_format_report(committed: &[DocRef], staging: &[DocRef]) -> ChapterFormatReport {
    let mut report = ChapterFormatReport::default();
    for (docs, into_staging) in [(committed, false), (staging, true)] {
        for doc in docs {
            match resolve::detect(doc) {
                Ok(Some(format)) if into_staging => report.staging.push((doc.kind(), format)),
                Ok(Some(format)) => report.committed.push((doc.kind(), format)),
                Ok(None) => {}
                Err(err) => report.unreadable.push((doc.stem().to_path_buf(), err.to_string())),
            }
        }
    }
    report
}
