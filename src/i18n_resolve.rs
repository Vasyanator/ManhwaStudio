/*
File: src/i18n_resolve.rs

Purpose:
Resolve catalog keys that are chosen at RUNTIME (not compile-time literals) into
their active-locale text. The `t!` macro cannot be used for these because its
argument must be a string literal; a GUI-free logic crate (e.g. `ms-text-util`,
`ms-text-render`) hands the binary a `&'static str` catalog key it computed from
an enum variant, and the binary resolves it here.

Key items:
- none. `resolve_key` is declared in `ms_i18n` (a wait-free, allocation-free catalog read,
  safe on the egui paint path) and called there directly. This module exists only to host
  the cross-crate tests that guard it — see the note below.

Notes:
This mirrors the `SocketSpec::display_label` / `reline_models::resolve_key`
pattern. The key/label split contract lives in `docs/i18n_exclusions.md` §F: a
GUI-free crate never carries localized text, but any label it hands the UI is a
catalog key resolved here.
*/

// `resolve_key` is DECLARED in `ms_i18n`; every caller now names it there directly (the
// launcher and the settings panes became crates of their own, and both sit above `ms-i18n`).
// This module is therefore no longer a re-export — it exists ONLY to host the tests below.
//
// They stay HERE, not in `ms-i18n`: they assert against the key sets of `ms-text-util` and
// against `ms_config::locale_store::GLOBAL_LOCALE_LOCK`, and `ms-i18n` sits BELOW both —
// importing them there would invert the dependency. The binary is the lowest place that
// can see all three at once.

#[cfg(test)]
mod tests {
    use crate::locale_store::GLOBAL_LOCALE_LOCK;
    use ms_i18n::resolve_key;
    use ms_i18n::LocaleTag;
    use ms_text_util::language::{ScriptGroup, TextLanguage};
    use ms_text_util::segmentation::Conservatism;

    /// Installs the embedded catalog for `tag`. Serialized by the caller under
    /// `GLOBAL_LOCALE_LOCK` because the active catalog is a process-global `ArcSwap`.
    fn install(tag: &str) {
        let tag = LocaleTag::parse(tag).expect("valid embedded tag");
        ms_i18n::set_locale(&tag).expect("embedded catalog installs");
    }

    #[test]
    fn script_group_label_follows_active_locale() {
        let _guard = GLOBAL_LOCALE_LOCK.lock().expect("lock");
        install("ru");
        assert_eq!(
            resolve_key(ScriptGroup::CyrillicSlavic.name_key()),
            "Славянские (кириллица)"
        );
        install("en");
        assert_eq!(
            resolve_key(ScriptGroup::CyrillicSlavic.name_key()),
            "Slavic (Cyrillic)"
        );
    }

    #[test]
    fn every_runtime_key_exists_in_active_catalog() {
        let _guard = GLOBAL_LOCALE_LOCK.lock().expect("lock");
        // With English installed every key returned by the converted crate methods
        // must resolve to a real value (not the key echoed back on a miss).
        install("en");
        for language in TextLanguage::all() {
            let key = language.name_key();
            assert_ne!(resolve_key(key), key, "language key {key:?} missing from en.json");
        }
        for group in ScriptGroup::all() {
            let name = group.name_key();
            assert_ne!(resolve_key(name), name, "group key {name:?} missing from en.json");
            let script = group.script_name_key();
            assert_ne!(resolve_key(script), script, "script key {script:?} missing from en.json");
        }
        for level in Conservatism::all() {
            let key = level.label_key();
            assert_ne!(resolve_key(key), key, "conservatism key {key:?} missing from en.json");
        }
        // The single prose form-preset key resolves too (shape presets carry no key).
        assert_ne!(
            resolve_key("typing.advanced.form_preset_free_no_tree"),
            "typing.advanced.form_preset_free_no_tree",
            "form-preset key missing from en.json"
        );
    }
}
