/*
File: cleaning/tools/ai_editor/engines/shared/catalog_download.rs

Purpose:
The model-agnostic PURE decisions of an engine whose weights come from the app's model catalog
(`ms_sysprobe::ai_models::external_catalog`) through the shared Translation-tab download
controller (`ms_tab_translation::ocr_model_download`): which gated repositories still need the
Hugging Face token, whether a bundle is known to be incomplete, whether a controller job is
running, and why such a job keeps the engine picker closed. The engine owns its bundle (which
specs a selection needs) and its drawing; it only asks these questions.

Key functions:
- `catalog_gated_repos_missing()`, `catalog_bundle_missing()`, `catalog_job_running()`,
  `catalog_switch_block_reason()`

Notes:
None of these touches a `Ui`, a channel or the disk, so the tests at the end assert them without
a frame. The switch-block texts are `cleaning.tools.catalog_download.*` keys and name no model:
the picker tooltip is shown inside the engine that blocks it.
*/

use ms_sysprobe::ai_models::external::ExternalModelSpec;
use ms_tab_translation::ocr_model_download::{JobKind, OcrModelDownloadState, OcrModelPanelSnapshot};

/// The GATED Hugging Face repositories (`ExternalModelSpec::gated_repo`, the catalog's one
/// owner of that question) of the `bundle` specs that `installed` does not report installed, in
/// bundle order and without repeats. Non-empty exactly while the token row and its access hint
/// are drawn: a download of what is missing needs the token, and an installed gated spec needs
/// nothing. `installed` is the controller's last probed state (an unprobed spec counts as
/// missing).
#[must_use]
pub(in crate::tools::ai_editor::engines) fn catalog_gated_repos_missing(bundle: &[&'static ExternalModelSpec], installed: impl Fn(&ExternalModelSpec) -> bool) -> Vec<&'static str> {
    let mut repos: Vec<&'static str> = Vec::new();
    for spec in bundle {
        if let Some(repo) = spec.gated_repo()
            && !installed(spec)
            && !repos.contains(&repo)
        {
            repos.push(repo);
        }
    }
    repos
}

/// Whether a catalog bundle's aggregated state says files are known to be absent: nothing
/// usable, or an unfinished download. Every other state — not probed yet, probing, a job
/// running, installed, a dead status worker — claims nothing.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn catalog_bundle_missing(state: &OcrModelDownloadState) -> bool {
    match state {
        OcrModelDownloadState::Missing | OcrModelDownloadState::Partial { .. } => true,
        OcrModelDownloadState::Unknown
        | OcrModelDownloadState::Checking
        | OcrModelDownloadState::Installed
        | OcrModelDownloadState::Downloading(_)
        | OcrModelDownloadState::Verifying(_)
        | OcrModelDownloadState::Unavailable(_) => false,
    }
}

/// Whether a controller job (download, verify or adoption) is running for ANY bundle, as one
/// panel snapshot sees it: its own bundle's job, or another bundle's that blocks it. The
/// falling edge of this is when files on disk may have changed, so an engine re-asks whatever
/// it derives from them (its installed-formats probe, `.status`, the forecast) then.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn catalog_job_running(snapshot: &OcrModelPanelSnapshot) -> bool {
    snapshot.other_download.is_some()
        || match snapshot.state {
            OcrModelDownloadState::Downloading(_) | OcrModelDownloadState::Verifying(_) => true,
            OcrModelDownloadState::Unknown
            | OcrModelDownloadState::Checking
            | OcrModelDownloadState::Missing
            | OcrModelDownloadState::Partial { .. }
            | OcrModelDownloadState::Installed
            | OcrModelDownloadState::Unavailable(_) => false,
        }
}

/// Why the engine picker must stay closed for a running catalog job of the engine's download
/// controller (`OcrModelDownloadController::running_job`), or `None` when none runs.
///
/// Every kind blocks: the host polls only the
/// SELECTED engine, so a job left behind by a switch would run with no progress, no cancel
/// and no repaint, and a download would share the disk with the other checkpoint's download.
/// A download names itself; a verify and an automatic adoption both hash files on disk and
/// share one message.
#[must_use]
pub(in crate::tools::ai_editor::engines) fn catalog_switch_block_reason(running_job: Option<JobKind>) -> Option<String> {
    match running_job? {
        JobKind::Download => Some(t!("cleaning.tools.catalog_download.switch_blocked_download").to_string()),
        JobKind::Verify | JobKind::Adopt => Some(t!("cleaning.tools.catalog_download.switch_blocked_verify").to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// Gated specs not installed are named once each, in bundle order; open specs never; an
    /// installed gated spec drops its repository. Built over the real catalog so the
    /// `gated_repo` answer is the catalog's own.
    #[test]
    fn the_gated_repositories_are_the_missing_gated_specs_once_each_in_order() {
        let all = ms_sysprobe::ai_models::external_catalog::all_specs();
        let gated: Vec<&'static ExternalModelSpec> = all.iter().copied().filter(|spec| spec.gated_repo().is_some()).collect();
        let open: Vec<&'static ExternalModelSpec> = all.iter().copied().filter(|spec| spec.gated_repo().is_none()).collect();
        assert!(!gated.is_empty() && !open.is_empty(), "the catalog has both gated and open specs");
        let nothing_installed = |_: &ExternalModelSpec| false;
        assert!(catalog_gated_repos_missing(&open, nothing_installed).is_empty(), "an open spec needs no token");
        let mut expected: Vec<&'static str> = Vec::new();
        for spec in &gated {
            if let Some(repo) = spec.gated_repo()
                && !expected.contains(&repo)
            {
                expected.push(repo);
            }
        }
        // Every gated spec twice, interleaved with open ones: still one entry per repository.
        let mut bundle: Vec<&'static ExternalModelSpec> = Vec::new();
        for spec in gated.iter().chain(gated.iter()) {
            bundle.push(*spec);
            bundle.extend(open.first().copied());
        }
        assert_eq!(catalog_gated_repos_missing(&bundle, nothing_installed), expected);
        assert!(catalog_gated_repos_missing(&bundle, |_| true).is_empty(), "installed gated specs need no token");
        let first_id = gated[0].id;
        let first_repo = gated[0].gated_repo();
        let only_first_missing = |spec: &ExternalModelSpec| spec.id != first_id;
        assert_eq!(catalog_gated_repos_missing(&bundle, only_first_missing), first_repo.into_iter().collect::<Vec<_>>());
    }

    /// Only a state that says files are absent counts as a missing bundle; a job runs exactly
    /// while a download / verify of this bundle, or any job of another one, is in flight.
    #[test]
    fn the_bundle_states_that_claim_missing_files_and_a_running_job() {
        assert!(catalog_bundle_missing(&OcrModelDownloadState::Missing));
        assert!(catalog_bundle_missing(&OcrModelDownloadState::Partial { bytes_present: 1 }));
        for state in [OcrModelDownloadState::Unknown, OcrModelDownloadState::Checking, OcrModelDownloadState::Installed, OcrModelDownloadState::Unavailable("x".to_string())] {
            assert!(!catalog_bundle_missing(&state), "{state:?}");
        }
        let snapshot = |state: OcrModelDownloadState| OcrModelPanelSnapshot { bundle_key: String::new(), state, missing_bytes: 0, notice: None, other_download: None, access: Vec::new() };
        assert!(!catalog_job_running(&snapshot(OcrModelDownloadState::Installed)));
        assert!(!catalog_job_running(&snapshot(OcrModelDownloadState::Missing)));
    }

    /// Every catalog job kind closes the engine picker: a download with the download message,
    /// a verify and an automatic adoption with the file-check message; no job, no block.
    #[test]
    fn every_catalog_job_kind_blocks_the_engine_switch() {
        assert_eq!(catalog_switch_block_reason(None), None);
        assert_eq!(
            catalog_switch_block_reason(Some(JobKind::Download)).as_deref(),
            Some(t!("cleaning.tools.catalog_download.switch_blocked_download"))
        );
        for kind in [JobKind::Verify, JobKind::Adopt] {
            assert_eq!(
                catalog_switch_block_reason(Some(kind)).as_deref(),
                Some(t!("cleaning.tools.catalog_download.switch_blocked_verify")),
                "{kind:?}"
            );
        }
    }

    /// The picker's disabled tooltips (a closed control that says nothing is a defect): the
    /// download one in every shipped locale, the file-check one in the two catalogs maintained
    /// by hand (`en`, the reference, and `ru`; `es` / `fr` / `pt` fall back to `en`). Neither
    /// takes a placeholder.
    #[test]
    fn the_catalogs_carry_the_switch_block_texts() {
        for (tag, source) in ms_i18n::embedded_locales() {
            let catalog: Value = serde_json::from_str(source).unwrap_or_else(|error| panic!("locale `{tag}` is not valid JSON: {error}"));
            let mut keys = vec!["cleaning.tools.catalog_download.switch_blocked_download"];
            if matches!(*tag, "en" | "ru") {
                keys.push("cleaning.tools.catalog_download.switch_blocked_verify");
            }
            for key in keys {
                let text = catalog.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("locale `{tag}` lacks the key `{key}`"));
                assert!(!text.trim().is_empty(), "locale `{tag}`: `{key}` is empty");
                assert!(!text.contains('{'), "locale `{tag}`: `{key}` takes no placeholder, `{text}` has one");
            }
        }
    }
}
