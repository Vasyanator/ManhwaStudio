/*
File: src/i18n_resolve.rs

Purpose:
Resolve catalog keys that are chosen at RUNTIME (not compile-time literals) into
their active-locale text. The `t!` macro cannot be used for these because its
argument must be a string literal; a GUI-free logic crate (e.g. `ms-text-util`,
`ms-text-render`) hands the binary a `&'static str` catalog key it computed from
an enum variant, and the binary resolves it here.

Key functions:
- `resolve_key` — `ms_i18n::lookup(key).unwrap_or(key)`, a wait-free,
  allocation-free catalog read safe on the egui paint path.

Notes:
This mirrors the `SocketSpec::display_label` / `reline_models::resolve_key`
pattern. The key/label split contract lives in `docs/i18n_exclusions.md` §F: a
GUI-free crate never carries localized text, but any label it hands the UI is a
catalog key resolved here.
*/

/// Resolves a catalog `key` to its active-locale text, falling back to the key
/// itself on a catalog miss (never panics, never allocates).
///
/// Use for keys chosen at runtime — e.g. returned by `ScriptGroup::name_key`,
/// `TextLanguage::name_key`, or `Conservatism::label_key` — where `t!` (which
/// requires a literal) does not apply. The returned `&'static str` is a pointer
/// into the leaked active catalog, so it is safe to paint every frame.
#[must_use]
pub fn resolve_key(key: &'static str) -> &'static str {
    ms_i18n::lookup(key).unwrap_or(key)
}

#[cfg(test)]
mod tests {
    use super::resolve_key;
    use crate::locale_store::GLOBAL_LOCALE_LOCK;
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
