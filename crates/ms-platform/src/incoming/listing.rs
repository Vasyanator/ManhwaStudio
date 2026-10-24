/*
File: incoming/listing.rs

Purpose:
The pure half of the Android incoming-images inbox: decodes the batch listings the Kotlin
`IncomingImages.takePending` answers into typed `IncomingBatch`es, and decides which claimed
batch folders must be released when an answer is empty or malformed.

Key functions:
- decode_batch(): one `"<batch dir>\n<batch.tsv text>"` entry -> `IncomingBatch`.
- decode_entries(): every entry of one take -> batches to hand out, folders to release, error.

Notes:
Pure Rust (no JNI), compiled on every target and host-tested here. The wire format is owned by
the Kotlin `IncomingImages` (file header there): lines `f\t<absolute path>` or
`s\t<name>\t<code>\t<detail>`, codes `copy_failed` / `no_stream`. Kotlin removes tabs and line
breaks from names and details, so a line with a different field count is malformed, never
"a name that contained a tab". A file path must lie inside its batch folder: the listing is
the only thing Rust trusts about the inbox, and a path outside it is rejected, not opened.
*/

use std::path::{Component, Path, PathBuf};

use super::{IncomingBatch, IncomingSkip, IncomingSkipKind};

/// Wire code of [`IncomingSkipKind::CopyFailed`].
const CODE_COPY_FAILED: &str = "copy_failed";
/// Wire code of [`IncomingSkipKind::NoStream`].
const CODE_NO_STREAM: &str = "no_stream";

/// The decoded answer of one `takePending` call.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct DecodedTake {
    /// Batches to hand to the caller, in arrival order; each has at least one file or skip.
    pub(crate) batches: Vec<IncomingBatch>,
    /// Claimed batch folders nobody will open (empty batches, or every batch of a malformed
    /// take); the caller releases them.
    pub(crate) to_release: Vec<PathBuf>,
    /// The first malformed entry's description. When set, `batches` is empty: the whole take
    /// fails, so the user hears about it instead of getting a silently partial set.
    pub(crate) error: Option<String>,
}

/// Decodes every entry of one `takePending` answer (see [`DecodedTake`] for the policy).
pub(crate) fn decode_entries(entries: &[String]) -> DecodedTake {
    let mut take = DecodedTake::default();
    for entry in entries {
        match decode_batch(entry) {
            Ok(batch) if batch.files.is_empty() && batch.skipped.is_empty() => take.to_release.extend(batch.batch_dir),
            Ok(batch) => take.batches.push(batch),
            Err(detail) => {
                take.to_release.extend(batch_dir_of(entry));
                if take.error.is_none() {
                    take.error = Some(detail);
                }
            }
        }
    }
    if take.error.is_some() {
        let dropped = std::mem::take(&mut take.batches);
        take.to_release.extend(dropped.into_iter().filter_map(|batch| batch.batch_dir));
    }
    take
}

/// Decodes one `"<batch dir>\n<batch.tsv text>"` entry.
///
/// # Errors
/// A description of the first malformed part: a relative or empty batch folder, a line with a
/// wrong field count or tag, an unknown skip code, or a file path outside the batch folder.
pub(crate) fn decode_batch(entry: &str) -> Result<IncomingBatch, String> {
    let (dir, listing) = entry.split_once('\n').unwrap_or((entry, ""));
    let batch_dir = PathBuf::from(dir);
    if dir.is_empty() || !batch_dir.is_absolute() {
        return Err(format!("the inbox batch folder '{dir}' is not an absolute path"));
    }
    let mut batch = IncomingBatch { files: Vec::new(), skipped: Vec::new(), batch_dir: Some(batch_dir.clone()) };
    for (index, line) in listing.lines().enumerate().filter(|(_, line)| !line.is_empty()) {
        let line_no = index + 1;
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["f", path] => batch.files.push(file_inside(&batch_dir, path).ok_or_else(|| format!("line {line_no}: the file '{path}' is not inside the batch folder"))?),
            ["s", name, code, detail] => {
                let kind = match *code {
                    CODE_COPY_FAILED => IncomingSkipKind::CopyFailed,
                    CODE_NO_STREAM => IncomingSkipKind::NoStream,
                    unknown => return Err(format!("line {line_no}: unknown skip code '{unknown}'")),
                };
                batch.skipped.push(IncomingSkip { name: (*name).to_owned(), kind, detail: (*detail).to_owned() });
            }
            _ => return Err(format!("line {line_no}: malformed listing line ({} fields)", fields.len())),
        }
    }
    Ok(batch)
}

/// The batch folder named by an entry's first line, when it is an absolute path.
fn batch_dir_of(entry: &str) -> Option<PathBuf> {
    let dir = entry.split_once('\n').map_or(entry, |(dir, _)| dir);
    let path = PathBuf::from(dir);
    (!dir.is_empty() && path.is_absolute()).then_some(path)
}

/// `path` when it is absolute, free of `..` / `.` components, and strictly inside `batch_dir`.
fn file_inside(batch_dir: &Path, path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    let plain = path.components().all(|component| matches!(component, Component::RootDir | Component::Prefix(_) | Component::Normal(_)));
    (plain && path.is_absolute() && path != batch_dir && path.starts_with(batch_dir)).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIR: &str = "/data/user/0/app/cache/incoming/0001760000000000-000000";

    fn entry(listing: &str) -> String {
        format!("{DIR}\n{listing}")
    }

    #[test]
    fn files_and_skips_keep_listing_order() {
        let batch = decode_batch(&entry(&format!("f\t{DIR}/00000/b.png\ns\tx.webp\tcopy_failed\tIOException: gone\nf\t{DIR}/00002/a.png\ns\t4\tno_stream\t\n")))
            .unwrap_or_else(|err| panic!("valid listing rejected: {err}"));
        assert_eq!(batch.files, vec![PathBuf::from(format!("{DIR}/00000/b.png")), PathBuf::from(format!("{DIR}/00002/a.png"))]);
        assert_eq!(
            batch.skipped,
            vec![
                IncomingSkip { name: "x.webp".to_owned(), kind: IncomingSkipKind::CopyFailed, detail: "IOException: gone".to_owned() },
                IncomingSkip { name: "4".to_owned(), kind: IncomingSkipKind::NoStream, detail: String::new() },
            ]
        );
        assert_eq!(batch.batch_dir, Some(PathBuf::from(DIR)));
    }

    #[test]
    fn a_tab_inside_a_name_is_rejected_not_misread() {
        let err = decode_batch(&entry("s\tbad\tname.png\tcopy_failed\tdetail\n")).err();
        assert!(err.is_some_and(|err| err.contains("malformed")));
        assert!(decode_batch(&entry(&format!("f\t{DIR}/00000/a\tb.png\n"))).is_err());
    }

    #[test]
    fn an_unknown_skip_code_is_rejected() {
        let err = decode_batch(&entry("s\ta.png\ttoo_big\t\n")).err();
        assert!(err.is_some_and(|err| err.contains("too_big")));
    }

    #[test]
    fn unknown_tags_and_wrong_field_counts_are_rejected() {
        assert!(decode_batch(&entry("x\t/a\n")).is_err());
        assert!(decode_batch(&entry("f\n")).is_err());
        assert!(decode_batch(&entry("s\ta.png\tcopy_failed\n")).is_err());
    }

    #[test]
    fn files_outside_the_batch_folder_are_rejected() {
        assert!(decode_batch(&entry("f\t/etc/passwd\n")).is_err());
        assert!(decode_batch(&entry(&format!("f\t{DIR}/../other/a.png\n"))).is_err());
        assert!(decode_batch(&entry(&format!("f\t{DIR}\n"))).is_err());
        assert!(decode_batch(&entry("f\t00000/a.png\n")).is_err());
    }

    #[test]
    fn the_batch_folder_must_be_absolute() {
        assert!(decode_batch("relative/dir\n").is_err());
        assert!(decode_batch("").is_err());
    }

    #[test]
    fn an_empty_listing_decodes_to_an_empty_batch() {
        let batch = decode_batch(DIR).unwrap_or_else(|err| panic!("empty listing rejected: {err}"));
        assert!(batch.files.is_empty() && batch.skipped.is_empty());
        let batch = decode_batch(&entry("\n\n")).unwrap_or_else(|err| panic!("blank lines rejected: {err}"));
        assert!(batch.files.is_empty() && batch.skipped.is_empty());
    }

    #[test]
    fn a_take_hands_out_batches_and_releases_empty_ones() {
        let second = "/data/user/0/app/cache/incoming/0001760000000001-000001";
        let take = decode_entries(&[entry(&format!("f\t{DIR}/00000/a.png\n")), format!("{second}\n")]);
        assert_eq!(take.error, None);
        assert_eq!(take.batches.len(), 1);
        assert_eq!(take.to_release, vec![PathBuf::from(second)]);
    }

    #[test]
    fn a_malformed_entry_fails_the_whole_take_and_releases_every_batch() {
        let second = "/data/user/0/app/cache/incoming/0001760000000001-000001";
        let take = decode_entries(&[entry(&format!("f\t{DIR}/00000/a.png\n")), format!("{second}\ns\ta.png\tweird\t\n")]);
        assert!(take.error.is_some());
        assert_eq!(take.batches, [] as [IncomingBatch; 0]);
        assert_eq!(take.to_release, vec![PathBuf::from(second), PathBuf::from(DIR)]);
    }

    #[test]
    fn no_entries_is_an_empty_take() {
        assert_eq!(decode_entries(&[]), DecodedTake::default());
    }
}
