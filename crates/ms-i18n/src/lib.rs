/*
File: crates/ms-i18n/src/lib.rs

Purpose:
Crate root of `ms-i18n` — the ManhwaStudio localization catalog and lookup layer.
It owns the embedded locale JSON, the parsed [`Catalog`] model, the process-global
active-catalog slot (behind `ArcSwap`), and the `t!` / `tf!` / `tp!` macros.

Main responsibilities:
- expose `LocaleTag` (open catalog identity), `PluralRules` (closed rule set) and
  the `plural_rules_for_tag` bridge, `Catalog`, `install`/`set_locale`,
  `lookup`/`lookup_plural`, `plural_category`, `interpolate`, and `embedded_locales`;
- export the `t!` / `tf!` / `tp!` translation macros at the crate root.

Type split — identity vs. plural rules:
- `LocaleTag` is an OPEN validated string: any `<tag>.json` a user drops into the
  on-disk `locale/` folder loads (custom languages included). It identifies a
  catalog and maps 1:1 to a filename.
- `PluralRules` is a CLOSED enum (En/Ru/Es/Fr/Pt): only these have hand-written
  CLDR rules, and `plural_category` matches on it exhaustively. A tag with no
  dedicated rule set resolves (once, at catalog-build time) to English rules via
  `plural_rules_for_tag`, whose `PluralRulesResolution` result REPORTS the
  fallback so the binary can log it rather than it being silent.

Design contracts (details in the module headers):
- `t!` is on the egui render hot path: it neither locks nor allocates. It reads
  the active catalog through `ArcSwap` and returns a `&'static str`. Catalog
  strings are `Box::leak`ed at load time so the reference is valid forever; the
  leak is bounded by the number of locale switches per process (see
  `catalog.rs`). `tf!` and `tp!` interpolate and therefore return `String`.
- The crate is GUI-free and config-free: locale JSON is embedded via
  `include_str!`, and it emits no logs (a miss returns the key/template verbatim
  so callers decide how to react).
*/

#![warn(clippy::all)]
#![warn(clippy::pedantic)]
// `module_name_repetitions` fires on `I18nError` / the module layout the project
// convention uses elsewhere; not applicable here.
#![allow(clippy::module_name_repetitions)]

pub mod catalog;
pub mod error;
pub mod interpolate;
pub mod locale;
pub mod plural;

pub use catalog::{
    Catalog, embedded_locales, install, lookup, lookup_plural, set_locale,
};
pub use error::I18nError;
pub use interpolate::interpolate;
pub use locale::LocaleTag;
pub use plural::{PluralRules, PluralRulesResolution, plural_category, plural_rules_for_tag};

/// Converts an integer count into the `i64` operand used for plural selection.
///
/// Implemented for the standard integer types. Out-of-range magnitudes saturate
/// to `i64::MIN` / `i64::MAX` rather than wrapping or panicking; real UI counts
/// never approach that range, so the saturation is a safety net, not a lossy
/// path callers are expected to hit. Used by the [`tp!`] macro so it accepts any
/// integer count type.
pub trait PluralCount {
    /// Returns this count as an `i64`, saturating on overflow.
    fn as_i64(&self) -> i64;
}

// Types that always fit in i64 via a lossless `From`.
macro_rules! impl_plural_count_from {
    ($($t:ty),*) => { $(
        impl PluralCount for $t {
            fn as_i64(&self) -> i64 { i64::from(*self) }
        }
    )* };
}
impl_plural_count_from!(i8, i16, i32, u8, u16, u32);

impl PluralCount for i64 {
    fn as_i64(&self) -> i64 {
        *self
    }
}

// Unsigned-wide types: only the > i64::MAX case fails, and it saturates high.
macro_rules! impl_plural_count_saturating_high {
    ($($t:ty),*) => { $(
        impl PluralCount for $t {
            fn as_i64(&self) -> i64 { i64::try_from(*self).unwrap_or(i64::MAX) }
        }
    )* };
}
impl_plural_count_saturating_high!(u64, usize);

impl PluralCount for isize {
    fn as_i64(&self) -> i64 {
        // Signed-wide: saturate toward the end the value overflowed past.
        i64::try_from(*self).unwrap_or(if *self < 0 { i64::MIN } else { i64::MAX })
    }
}

/// Translates a static key to a `&'static str`, or returns the key itself on a miss.
///
/// Reads the active catalog wait-free and without allocating (safe on the render
/// hot path). The argument must be a string literal.
#[macro_export]
macro_rules! t {
    ($key:literal $(,)?) => {
        $crate::lookup($key).unwrap_or($key)
    };
}

/// Translates a static key and interpolates named arguments, returning a `String`.
///
/// Placeholders are named (`{err}`, ...). A missing catalog key falls back to the
/// key text; a missing argument leaves its placeholder verbatim. Never panics.
///
/// Usage: `tf!("app.save_error", err = e)`.
#[macro_export]
macro_rules! tf {
    ($key:literal $(, $name:ident = $val:expr)* $(,)?) => {
        $crate::interpolate(
            $crate::lookup($key).unwrap_or($key),
            &[$( (::core::stringify!($name), &($val) as &dyn ::core::fmt::Display) ),*],
        )
    };
}

/// Translates a plural key for count `n` and interpolates `{n}`, returning a `String`.
///
/// Selects the CLDR plural form for the active locale, then substitutes `{n}`
/// with the count. On a missing key it returns the key text. Accepts any integer
/// count type (via [`PluralCount`]). Never panics.
///
/// Usage: `tp!("wiki.chars", count)`.
#[macro_export]
macro_rules! tp {
    ($key:literal, $n:expr $(,)?) => {{
        let __ms_i18n_n: i64 = $crate::PluralCount::as_i64(&$n);
        match $crate::lookup_plural($key, __ms_i18n_n) {
            ::core::option::Option::Some(__ms_i18n_tpl) => $crate::interpolate(
                __ms_i18n_tpl,
                &[("n", &__ms_i18n_n as &dyn ::core::fmt::Display)],
            ),
            ::core::option::Option::None => ::std::string::String::from($key),
        }
    }};
}

/// Process-global lock serializing every test in this crate that mutates the
/// active-catalog slot (`ACTIVE` in `catalog.rs`). The macro tests here and the
/// `install`/`set_locale` tests in `catalog.rs` share one `ArcSwap`, so they must
/// not run concurrently or one test's assertion could observe another's catalog.
#[cfg(test)]
pub(crate) static ACTIVE_CATALOG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquires [`ACTIVE_CATALOG_TEST_LOCK`], ignoring poisoning.
///
/// A failing assertion inside the critical section poisons the mutex, which would then
/// fail every other test that shares it with an unrelated `PoisonError` and bury the one
/// real failure. The guarded data is `()`, so there is no invariant a panic could have
/// broken — recovering is safe and keeps failures reported at their true site.
#[cfg(test)]
pub(crate) fn lock_active_catalog_for_test() -> std::sync::MutexGuard<'static, ()> {
    ACTIVE_CATALOG_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use crate::catalog::Catalog;
    use crate::locale::LocaleTag;
    use std::sync::Arc;

    /// Parses a tag for tests (all literals here are valid).
    fn tag(s: &str) -> LocaleTag {
        LocaleTag::parse(s).expect("test tag is valid")
    }

    /// Installs a self-contained English fixture catalog so these macro tests never
    /// depend on the shipped locale JSON — deleting or renaming a product key can
    /// never break them.
    fn install_en_fixture() {
        let en = Catalog::from_json_str(
            &tag("en"),
            r#"{
                "fixture.greeting": "Hello",
                "fixture.save_error": "Save failed: {err}",
                "fixture.chars": { "one": "{n} character", "other": "{n} characters" }
            }"#,
        )
        .expect("en fixture parses");
        crate::install(en);
    }

    /// Installs a self-contained Russian fixture catalog (with an English fallback)
    /// exercising the CLDR one/few/many/other plural forms.
    fn install_ru_fixture() {
        let en = Catalog::from_json_str(
            &tag("en"),
            r#"{ "fixture.chars": { "one": "{n} character", "other": "{n} characters" } }"#,
        )
        .expect("en fixture parses");
        let ru = Catalog::from_json_str(
            &tag("ru"),
            r#"{ "fixture.chars": { "one": "{n} символ", "few": "{n} символа", "many": "{n} символов", "other": "{n} символа" } }"#,
        )
        .expect("ru fixture parses")
        .with_fallback(Arc::new(en));
        crate::install(ru);
    }

    #[test]
    fn t_returns_translation_or_key() {
        let _guard = crate::lock_active_catalog_for_test();
        install_en_fixture();
        assert_eq!(t!("fixture.greeting"), "Hello");
        // Unknown key falls back to the key text.
        assert_eq!(t!("no.such.key"), "no.such.key");
    }

    #[test]
    fn tf_interpolates_and_never_panics_on_missing_arg() {
        let _guard = crate::lock_active_catalog_for_test();
        install_en_fixture();
        assert_eq!(
            tf!("fixture.save_error", err = "disk full"),
            "Save failed: disk full"
        );
        // Missing argument leaves the placeholder intact, no panic.
        assert_eq!(tf!("fixture.save_error"), "Save failed: {err}");
    }

    #[test]
    fn tp_selects_plural_form_and_interpolates() {
        let _guard = crate::lock_active_catalog_for_test();
        install_en_fixture();
        assert_eq!(tp!("fixture.chars", 1_usize), "1 character");
        assert_eq!(tp!("fixture.chars", 5_i32), "5 characters");
        install_ru_fixture();
        assert_eq!(tp!("fixture.chars", 2_u64), "2 символа");
    }
}
