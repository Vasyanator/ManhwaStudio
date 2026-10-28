/*
File: crates/ms-sdcpp/src/error.rs

Purpose:
`SdCppError`, the typed failure of every `ms-sdcpp` operation, with a localized `Display`
(`sdcpp.error.*`, one exhaustive match) ready for a status line, and `PackageFailure`, the
coarse class of a package install / remove failure.

Notes:
- `Debug` (derived, English) is the log form; `Display` is the user-facing text.
- `SdCppError` is `Clone` because the process-global status snapshot carries probe errors;
  every variant must stay `Clone`.
- The worker-side variants (`LibraryLoad` … `Protocol`) also cross the worker pipe as
  `protocol::WireError`; a new worker-side variant needs a wire twin there.
- `PackageError` is mapped once, in `from_package_error`, keeping its English diagnostic text
  as `detail` (no secrets: URLs and paths only).
*/

use ms_native_package::PackageError;
use ms_platform::Unavailable;

use crate::catalog::SdCppBuild;
use crate::families::ModelFilesError;
use crate::platform_check::RequirementProblem;

/// Why loading the sd.cpp libraries failed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum LoadProblem {
    /// A system library the package needs is not installed (`library` names it, e.g.
    /// `libgomp.so.1`, or a too-old `GLIBCXX_3.4.32` symbol version).
    MissingSystemLibrary { library: String },
    /// Any other load failure (see the detail).
    Other,
}

/// Coarse class of a failed package install or remove, chosen for the user-facing text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageFailure {
    /// The archive could not be downloaded (HTTP, network, retries exhausted, size mismatch).
    Download,
    /// The downloaded archive does not hash to its pinned sha256 (it was deleted).
    Checksum,
    /// The verified archive could not be unpacked, or a member name is invalid.
    Unpack,
    /// A filesystem step in the package directory failed.
    Io,
}

/// Failure of an `ms-sdcpp` operation.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SdCppError {
    /// The platform has no sd.cpp runtime (`PlatformFeature::SdCppRuntime`).
    #[error("{0}")]
    Unavailable(Unavailable),
    /// The catalog has no package of `build` for this OS / architecture.
    #[error("{}", tf!("sdcpp.error.no_build_here_error", build = build.label()))]
    NoBuildHere { build: SdCppBuild },
    /// The package of `build` is not installed (or its marker is not trusted).
    #[error("{}", tf!("sdcpp.error.not_installed_error", build = build.label()))]
    NotInstalled { build: SdCppBuild },
    /// The host does not meet a requirement of the selected build.
    #[error("{0}")]
    Requirement(RequirementProblem),
    /// Installing or removing the package failed; `detail` is the English diagnostic text.
    #[error("{}", package_message(*kind, detail))]
    Package { kind: PackageFailure, detail: String },
    /// The package download (or a probe) was cancelled by the user; staged download bytes are
    /// kept.
    #[error("{}", t!("sdcpp.error.cancelled_error"))]
    Cancelled,
    /// The package is being installed (or removed) by another task right now.
    #[error("{}", t!("sdcpp.error.busy_error"))]
    Busy,
    /// A generation was cancelled (by the user, or by an unload / package removal).
    #[error("{}", t!("sdcpp.error.generation_cancelled_error"))]
    GenerationCancelled,
    /// Another generation is already running in this process (one at a time).
    #[error("{}", t!("sdcpp.error.generation_busy_error"))]
    GenerationBusy,
    /// The catalog has no package at all for this OS / architecture.
    #[error("{}", t!("sdcpp.error.no_build_for_platform_error"))]
    NoBuildForPlatform,
    /// A worker using the package could not be stopped in time, so the package was not
    /// removed (its libraries may still be loaded).
    #[error("{}", t!("sdcpp.error.worker_still_running_error"))]
    WorkerStillRunning,
    /// The model files cannot run under sd.cpp.
    #[error("{0}")]
    Files(ModelFilesError),
    /// The sd.cpp or ggml library could not be loaded; `detail` is the loader's message.
    #[error("{}", load_message(problem, detail))]
    LibraryLoad { problem: LoadProblem, detail: String },
    /// The loaded library is not the pinned release (`sd_commit()` = `found`).
    #[error("{}", tf!("sdcpp.error.abi_mismatch_error", expected = expected, found = found))]
    AbiMismatch { expected: String, found: String },
    /// The chosen compute device `name` is not among the devices sd.cpp reports.
    #[error("{}", tf!("sdcpp.error.device_not_found_error", name = name))]
    DeviceNotFound { name: String },
    /// sd.cpp could not create the context (load the model); `detail` holds its last errors.
    #[error("{}", tf!("sdcpp.error.context_error", detail = detail))]
    ContextCreate { detail: String },
    /// The generation failed; `detail` holds sd.cpp's last errors.
    #[error("{}", tf!("sdcpp.error.generation_error", detail = detail))]
    Generation { detail: String },
    /// The library returned an image of another size than requested.
    #[error("{}", tf!("sdcpp.error.output_size_error", expected = format!("{}x{}", expected.0, expected.1), found = format!("{}x{}", found.0, found.1)))]
    OutputSize { expected: (u32, u32), found: (u32, u32) },
    /// The worker pipe carried something unexpected; `detail` is English diagnostic text.
    #[error("{}", tf!("sdcpp.error.protocol_error", detail = detail))]
    Protocol { detail: String },
    /// The worker process could not be started.
    #[error("{}", tf!("sdcpp.error.worker_spawn_error", detail = detail))]
    WorkerSpawn { detail: String },
    /// The worker process ended unexpectedly (`exit`: "signal 6 (SIGABRT)", "code 134", …);
    /// `stderr_tail` holds its last diagnostic lines (logged, not shown).
    #[error("{}", tf!("sdcpp.error.worker_crashed_error", exit = exit))]
    WorkerCrashed { exit: String, stderr_tail: String },
}

/// The localized text of a [`SdCppError::LibraryLoad`].
fn load_message(problem: &LoadProblem, detail: &str) -> String {
    match problem {
        LoadProblem::MissingSystemLibrary { library } => tf!("sdcpp.error.missing_system_library_error", library = library, detail = detail),
        LoadProblem::Other => tf!("sdcpp.error.library_load_error", detail = detail),
    }
}

/// The localized text of a [`SdCppError::Package`].
fn package_message(kind: PackageFailure, detail: &str) -> String {
    match kind {
        PackageFailure::Download => tf!("sdcpp.error.download_error", detail = detail),
        PackageFailure::Checksum => tf!("sdcpp.error.checksum_error", detail = detail),
        PackageFailure::Unpack => tf!("sdcpp.error.unpack_error", detail = detail),
        PackageFailure::Io => tf!("sdcpp.error.io_error", detail = detail),
    }
}

impl SdCppError {
    /// Maps a package transaction failure: cancel stays [`SdCppError::Cancelled`], everything
    /// else becomes [`SdCppError::Package`] with its class and the English diagnostic text.
    /// Invalid or colliding file names are catalog defects (the catalog tests rule them out);
    /// they are classed as `Unpack` (member names) or `Io` (package file names).
    #[must_use]
    pub fn from_package_error(err: &PackageError) -> Self {
        let kind = match err {
            PackageError::Cancelled => return Self::Cancelled,
            PackageError::ChecksumMismatch { .. } => PackageFailure::Checksum,
            PackageError::Download { .. } => PackageFailure::Download,
            PackageError::Unpack { .. } | PackageError::InvalidMemberPath { .. } => PackageFailure::Unpack,
            PackageError::Io { .. }
            | PackageError::Publish { .. }
            | PackageError::SyncDir { .. }
            | PackageError::WriteMarker { .. }
            | PackageError::InvalidFileName { .. }
            | PackageError::DuplicateFileName { .. } => PackageFailure::Io,
        };
        Self::Package { kind, detail: err.to_string() }
    }
}

impl From<Unavailable> for SdCppError {
    fn from(value: Unavailable) -> Self {
        Self::Unavailable(value)
    }
}

impl From<ModelFilesError> for SdCppError {
    fn from(value: ModelFilesError) -> Self {
        Self::Files(value)
    }
}

impl From<RequirementProblem> for SdCppError {
    fn from(value: RequirementProblem) -> Self {
        Self::Requirement(value)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ms_platform::PlatformFeature;

    use super::*;

    /// One value of every variant (the match keeps the list exhaustive).
    fn every_variant() -> Vec<SdCppError> {
        let all = vec![
            SdCppError::Unavailable(Unavailable::new(PlatformFeature::SdCppRuntime, ms_platform::UnavailableReason::NotOnThisPlatform)),
            SdCppError::NoBuildHere { build: SdCppBuild::Vulkan },
            SdCppError::NotInstalled { build: SdCppBuild::Cpu },
            SdCppError::Requirement(RequirementProblem::NotGlibc),
            SdCppError::Package { kind: PackageFailure::Download, detail: "d".to_owned() },
            SdCppError::Package { kind: PackageFailure::Checksum, detail: "d".to_owned() },
            SdCppError::Package { kind: PackageFailure::Unpack, detail: "d".to_owned() },
            SdCppError::Package { kind: PackageFailure::Io, detail: "d".to_owned() },
            SdCppError::Cancelled,
            SdCppError::Busy,
            SdCppError::GenerationCancelled,
            SdCppError::GenerationBusy,
            SdCppError::NoBuildForPlatform,
            SdCppError::WorkerStillRunning,
            SdCppError::Files(ModelFilesError::EncoderMismatch { expected_width: 7680, found_width: 12288 }),
            SdCppError::LibraryLoad { problem: LoadProblem::MissingSystemLibrary { library: "libgomp.so.1".into() }, detail: "d".into() },
            SdCppError::LibraryLoad { problem: LoadProblem::Other, detail: "d".into() },
            SdCppError::AbiMismatch { expected: "be48649".into(), found: "0000000".into() },
            SdCppError::DeviceNotFound { name: "Vulkan1".into() },
            SdCppError::ContextCreate { detail: "d".into() },
            SdCppError::Generation { detail: "d".into() },
            SdCppError::OutputSize { expected: (64, 64), found: (32, 32) },
            SdCppError::Protocol { detail: "d".into() },
            SdCppError::WorkerSpawn { detail: "d".into() },
            SdCppError::WorkerCrashed { exit: "signal 6 (SIGABRT)".into(), stderr_tail: "GGML_ASSERT".into() },
        ];
        for err in &all {
            match err {
                SdCppError::Unavailable(_)
                | SdCppError::NoBuildHere { .. }
                | SdCppError::NotInstalled { .. }
                | SdCppError::Requirement(_)
                | SdCppError::Package { .. }
                | SdCppError::Cancelled
                | SdCppError::Busy
                | SdCppError::GenerationCancelled
                | SdCppError::GenerationBusy
                | SdCppError::NoBuildForPlatform
                | SdCppError::WorkerStillRunning
                | SdCppError::Files(_)
                | SdCppError::LibraryLoad { .. }
                | SdCppError::AbiMismatch { .. }
                | SdCppError::DeviceNotFound { .. }
                | SdCppError::ContextCreate { .. }
                | SdCppError::Generation { .. }
                | SdCppError::OutputSize { .. }
                | SdCppError::Protocol { .. }
                | SdCppError::WorkerSpawn { .. }
                | SdCppError::WorkerCrashed { .. } => {}
            }
        }
        all
    }

    #[test]
    fn every_variant_displays_text() {
        for err in every_variant() {
            let text = err.to_string();
            assert!(!text.trim().is_empty(), "{err:?}");
        }
    }

    #[test]
    fn package_errors_map_to_their_class() {
        let class = |err: &PackageError| match SdCppError::from_package_error(err) {
            SdCppError::Package { kind, detail } => {
                assert_ne!(detail, "");
                Some(kind)
            }
            SdCppError::Cancelled => None,
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(class(&PackageError::Cancelled), None);
        assert_eq!(class(&PackageError::ChecksumMismatch { url: "u".into(), expected: "a".into(), actual: "b".into() }), Some(PackageFailure::Checksum));
        assert_eq!(class(&PackageError::InvalidMemberPath { member: "/".into() }), Some(PackageFailure::Unpack));
        assert_eq!(class(&PackageError::DuplicateFileName { name: "x".into() }), Some(PackageFailure::Io));
        assert_eq!(
            class(&PackageError::Io { op: ms_native_package::PackageIoOp::CreateDir, path: PathBuf::from("/tmp/x"), source: std::io::Error::other("boom") }),
            Some(PackageFailure::Io)
        );
    }
}
