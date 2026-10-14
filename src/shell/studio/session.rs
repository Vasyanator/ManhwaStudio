/*
File: src/shell/studio/session.rs

Purpose:
The shell half of one studio session: `StudioSession` — a project directory, or an image set
with its reserved `SingleImageScratch` — and the one shell-side owner of the session KIND
(`is_image_set`, `reloadable`). Created BY THE OPEN WORKER (`open.rs`: the scratch reservation
is I/O), owned by the studio screen (`mod.rs`) for the session's whole life, and ended by the
studio teardown (`closing.rs`) with `release`. The data half (`ProjectData::session`,
`ms_project::SessionKind`) is decided by the same open and must agree with it (pinned by the
open worker's tests); the scratch lives here because it must outlive `StudioApp`.

Key structures:
- `StudioSession`: `Project { dir }` or `ImageSet { sources, scratch }`.

Key functions:
- `StudioSession::from_target`: routing target -> session (reserves the scratch; blocking).
- `StudioSession::is_image_set` / `reloadable`: the kind questions the shell asks.
- `StudioSession::log_open`: the open's log line, written from the target when the open starts.
- `StudioSession::release`: deletes the scratch and releases the set's inbox batches
  (blocking; never on the GUI thread).

Notes:
Native-only (gated with the whole shell host off wasm).
*/

use super::super::target::{ImageSources, StartupTarget, release_inbox_batches_blocking};
use crate::project::single_image::{SingleImageError, SingleImageScratch};
use crate::runtime_log;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The shell half of one studio session. Created by the open worker, owned by the studio screen
/// for the session's whole life (plan D13) and ended with [`StudioSession::release`] by the
/// teardown worker.
#[derive(Debug)]
pub enum StudioSession {
    /// A chapter directory, loaded with the regular unsaved-detection + `ProjectData::load*`.
    Project { dir: PathBuf },
    /// An image set opened through a reserved scratch chapter. The scratch is shared (`Arc`)
    /// only while the open worker uses it; `release` deletes it once it is the sole owner.
    ImageSet { sources: ImageSources, scratch: Arc<SingleImageScratch> },
}

impl StudioSession {
    /// Logs what an open of `target` is about to open (GUI thread, when the studio screen starts,
    /// before the worker reserves anything, so the line exists even when the reservation fails).
    /// A set can hold hundreds of paths: its size and first file are logged, not the whole list.
    pub fn log_open(target: &StartupTarget) {
        match target {
            StartupTarget::Project(project_dir) => runtime_log::log_info(format!("[studio] opening project {}", project_dir.display())),
            StartupTarget::Images(sources) => runtime_log::log_info(format!(
                "[studio] opening an image set: {} file(s), {} already skipped, {} inbox batch(es), first {:?}",
                sources.files().len(),
                sources.skipped().len(),
                sources.inbox_batches().len(),
                sources.title_path()
            )),
        }
    }

    /// Maps a routing target to a session. A project maps 1:1; an image set reserves a fresh
    /// scratch session under `scratch_base` (production: `ms_config::single_image::scratch_base()`).
    /// Blocking (mkdir + lock + marker): the open worker calls it, never the GUI thread.
    ///
    /// # Errors
    /// The `SingleImageScratch::reserve` error; nothing is left behind on failure (the inbox
    /// batches of the set are the caller's to release).
    pub fn from_target(target: StartupTarget, scratch_base: &Path) -> Result<Self, SingleImageError> {
        match target {
            StartupTarget::Project(dir) => Ok(Self::Project { dir }),
            StartupTarget::Images(sources) => {
                let scratch = SingleImageScratch::reserve(scratch_base)?;
                Ok(Self::ImageSet { sources, scratch: Arc::new(scratch) })
            }
        }
    }

    /// `true` for an image set edited through a scratch chapter.
    #[must_use]
    pub fn is_image_set(&self) -> bool {
        match self {
            Self::Project { .. } => false,
            Self::ImageSet { .. } => true,
        }
    }

    /// Whether the session may be rebuilt from disk after a structural page operation
    /// (`StudioExit::Reload`). An image set never may: the reload follows a page operation and
    /// the Page Manager is hidden in image-set mode, so the request is unreachable there; it is
    /// refused (logged by the studio screen) instead of re-loading the scratch as a project.
    #[must_use]
    pub fn reloadable(&self) -> bool {
        !self.is_image_set()
    }

    /// Ends the session: deletes an image-set scratch session (blocking
    /// recursive delete — a worker or the process-shutdown hook, never a regular frame), then
    /// releases the set's inbox batches (`ms_platform::incoming::release_batch_blocking`, whose
    /// files were copied into the scratch at open). A delete failure, or a scratch still shared
    /// elsewhere, is logged; the next startup sweep removes it.
    pub fn release(self) {
        match self {
            Self::Project { .. } => {}
            Self::ImageSet { sources, scratch } => {
                match Arc::try_unwrap(scratch) {
                    Ok(scratch) => {
                        if let Err(err) = scratch.remove() {
                            runtime_log::log_warn(format!(
                                "[studio] could not delete the image-set scratch for {} file(s) starting with '{}': {err}; the next startup sweep retries",
                                sources.files().len(),
                                sources.title_path().map_or_else(String::new, |path| path.display().to_string())
                            ));
                        }
                    }
                    Err(shared) => runtime_log::log_warn(format!(
                        "[studio] image-set scratch {} is still shared; left for the next startup sweep",
                        shared.root().display()
                    )),
                }
                release_inbox_batches_blocking(sources.inbox_batches());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{images, temp_base};
    use super::*;

    #[test]
    fn project_target_maps_one_to_one_without_touching_the_scratch_base() {
        let base = temp_base("project");
        let dir = PathBuf::from("/home/u/titles/Title/Chapter 1");
        let target = StartupTarget::Project(dir.clone());
        assert!(!target.is_image_set());
        assert_eq!(target.window_title("1.2.3"), "ManhwaStudio v1.2.3 - /home/u/titles/Title/Chapter 1");
        let session = StudioSession::from_target(target, &base).expect("project maps");
        assert!(matches!(&session, StudioSession::Project { dir: mapped } if *mapped == dir));
        assert!(!session.is_image_set());
        assert!(session.reloadable(), "a project session reloads after a page operation");
        assert!(!base.exists(), "a project start must not create the scratch base");
        session.release();
    }

    #[test]
    fn image_target_reserves_a_scratch_and_release_deletes_it() {
        let base = temp_base("image");
        let source = PathBuf::from("/home/u/Картинки/page one.jpg");
        let target = images(&["/home/u/Картинки/page one.jpg"]);
        assert!(target.is_image_set());
        assert_eq!(target.window_title("1.2.3"), "ManhwaStudio v1.2.3 - page one.jpg", "a one-file set keeps the single-image title");
        let session = StudioSession::from_target(target, &base).expect("scratch reserved");
        assert!(session.is_image_set());
        assert!(!session.reloadable(), "an image-set session never reloads");
        let root = match &session {
            StudioSession::ImageSet { sources, scratch } => {
                assert_eq!(sources.files(), [source].as_slice());
                scratch.root().to_path_buf()
            }
            StudioSession::Project { dir } => panic!("image mapped to project {}", dir.display()),
        };
        assert!(root.starts_with(&base) && root.is_dir(), "scratch root {} must live under the base", root.display());
        session.release();
        assert!(!root.exists(), "release must delete the scratch session");
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }

    #[test]
    fn release_while_the_scratch_is_still_shared_leaves_it_for_the_sweep() {
        let base = temp_base("shared");
        let session = StudioSession::from_target(images(&["/home/u/a.png"]), &base).expect("scratch reserved");
        let other_share = match &session {
            StudioSession::ImageSet { scratch, .. } => Arc::clone(scratch),
            StudioSession::Project { dir } => panic!("image mapped to project {}", dir.display()),
        };
        let root = other_share.root().to_path_buf();
        session.release();
        assert!(root.is_dir(), "a scratch still held elsewhere must not be deleted under its holder");
        // The last holder dropping the scratch releases the lock without I/O; the startup sweep
        // then owns the cleanup.
        drop(other_share);
        let report = crate::project::single_image::sweep_stale_sessions(&base);
        assert_eq!(report.removed, 1);
        assert!(!root.exists());
        std::fs::remove_dir_all(&base).expect("remove test scratch base");
    }
}
