/*
File: crates/ms-i18n/src/catalog.rs

Purpose:
The catalog data model and the process-global active-catalog slot. A `Catalog` is
a parsed key -> value map with an optional fallback chain to English; the active
catalog lives behind an `ArcSwap` so `lookup` / `lookup_plural` (and thus `t!`)
read it wait-free, without a `Mutex` and without allocating.

Key structures:
- Catalog       — one locale's entries plus an optional `en` fallback
- Entry         — Simple(&'static str) | Plural(PluralForms)
- PluralForms   — CLDR one/few/many/other forms

Key functions:
- Catalog::from_json_str / from_json_object — build (and leak) a catalog
- install / set_locale                      — swap the active catalog
- lookup / lookup_plural                    — read the active catalog

INTENTIONAL MEMORY LEAK — the load-time contract that makes `t!` free:
Building a catalog `Box::leak`s every key and every value string, so the entry
map is `HashMap<&'static str, ...>` and lookups hand back `&'static str` that
outlive any catalog swap. The leak is bounded by the number of catalogs built
during a process lifetime, which equals the number of locale switches (a handful
per run) times the strings in a catalog (~dozens). It never grows with lookups.
This is the deliberate price for a zero-allocation, zero-lock `t!` on the render
hot path: a returned `&'static str` is valid forever, so a concurrent
`set_locale` dropping the previous `Arc<Catalog>` frees only the map, not the
strings a caller may still hold.
*/

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;

use arc_swap::ArcSwap;
use serde_json::Value;

use crate::error::I18nError;
use crate::locale::LocaleTag;
use crate::plural::{self, PluralRules};

/// The embedded English catalog source (reference / fallback locale).
const EN_JSON: &str = include_str!("../locales/en.json");
/// The embedded Russian catalog source.
const RU_JSON: &str = include_str!("../locales/ru.json");
/// The embedded Spanish catalog source.
const ES_JSON: &str = include_str!("../locales/es.json");
/// The embedded French catalog source.
const FR_JSON: &str = include_str!("../locales/fr.json");
/// The embedded Portuguese catalog source.
const PT_JSON: &str = include_str!("../locales/pt.json");

/// The reserved metadata key. It carries locale metadata (currently `name`) and is
/// never exposed as a translatable entry.
const META_KEY: &str = "_meta";

/// `(tag, json_source)` for every locale whose catalog is compiled in today.
///
/// All five shipped locales are present. A custom user-authored tag (e.g. `de`) has no
/// embedded source and must be loaded from disk. `src/locale_store.rs` uses this to
/// materialize the bundled sources into the editable `locale/` folder.
static EMBEDDED: [(&str, &str); 5] = [
    ("en", EN_JSON),
    ("ru", RU_JSON),
    ("es", ES_JSON),
    ("fr", FR_JSON),
    ("pt", PT_JSON),
];

/// Returns the embedded locale sources as `(tag, json_source)` pairs.
///
/// The slice contains every locale that ships a catalog (`en`, `ru`, `es`, `fr`, `pt`).
/// `src/locale_store.rs` materializes the bundled JSON onto disk; the catalog runtime
/// itself parses these directly.
#[must_use]
pub fn embedded_locales() -> &'static [(&'static str, &'static str)] {
    &EMBEDDED
}

/// The plural forms of a single catalog entry.
///
/// `other` is mandatory (CLDR's universal fallback category); `one`/`few`/`many`
/// are present only for locales that use them. All strings are `'static` (leaked
/// at load time — see the file header).
#[derive(Debug)]
struct PluralForms {
    one: Option<&'static str>,
    few: Option<&'static str>,
    many: Option<&'static str>,
    other: &'static str,
}

impl PluralForms {
    /// Returns the form for a CLDR `category` name, falling back to `other` when
    /// the requested category is absent or unrecognized.
    fn select(&self, category: &str) -> &'static str {
        let chosen = match category {
            plural::ONE => self.one,
            plural::FEW => self.few,
            plural::MANY => self.many,
            // `other` is always present; any unknown category collapses to it.
            _ => Some(self.other),
        };
        chosen.unwrap_or(self.other)
    }
}

/// One catalog entry: either a plain translated string or a set of plural forms.
#[derive(Debug)]
enum Entry {
    Simple(&'static str),
    Plural(PluralForms),
}

/// A parsed locale catalog: key -> [`Entry`] plus an optional fallback catalog.
///
/// Lookups first consult this catalog's own entries and, on a miss, walk the
/// fallback chain (which terminates at English). All contained strings are
/// `'static`; see the file header for the leak contract.
#[derive(Debug)]
pub struct Catalog {
    tag: LocaleTag,
    rules: PluralRules,
    entries: HashMap<&'static str, Entry>,
    fallback: Option<Arc<Catalog>>,
}

impl Catalog {
    /// The tag (catalog identity) this catalog was built for.
    #[must_use]
    pub fn tag(&self) -> &LocaleTag {
        &self.tag
    }

    /// The CLDR plural rule set resolved from this catalog's tag at build time
    /// (English rules for a tag with no dedicated rule set).
    #[must_use]
    pub fn plural_rules(&self) -> PluralRules {
        self.rules
    }

    /// Builds a catalog for `tag` from a JSON source string.
    ///
    /// The root must be a JSON object of flat, dotted keys. A value is either a
    /// string (simple entry) or an object of CLDR plural forms (`one`/`few`/
    /// `many`/`other`, `other` required). The reserved `_meta` key is ignored.
    /// The built catalog has no fallback; attach one with [`Catalog::with_fallback`].
    ///
    /// Leaks every key and value string (`'static`); see the file header.
    ///
    /// # Errors
    /// - [`I18nError::Parse`] if `source` is not valid JSON;
    /// - [`I18nError::NotObject`] if the root is not an object;
    /// - [`I18nError::InvalidValue`] if a value is neither a string nor a valid
    ///   plural object.
    pub fn from_json_str(tag: &LocaleTag, source: &str) -> Result<Catalog, I18nError> {
        let value: Value = serde_json::from_str(source).map_err(|source| I18nError::Parse {
            tag: tag.as_str().to_owned(),
            source,
        })?;
        Catalog::from_json_object(tag, &value)
    }

    /// Builds a catalog for `tag` from an already-parsed JSON value.
    ///
    /// Same contract as [`Catalog::from_json_str`] except the JSON is supplied as
    /// a [`serde_json::Value`]. Leaks every key and value string. The plural rule
    /// set is resolved from `tag` ONCE here (English rules for a tag with no
    /// dedicated rule set); the resolution is not re-run per lookup.
    ///
    /// # Errors
    /// [`I18nError::NotObject`] if `value` is not an object, or
    /// [`I18nError::InvalidValue`] for a malformed entry.
    pub fn from_json_object(tag: &LocaleTag, value: &Value) -> Result<Catalog, I18nError> {
        let object = value
            .as_object()
            .ok_or_else(|| I18nError::NotObject(tag.as_str().to_owned()))?;

        let mut entries: HashMap<&'static str, Entry> = HashMap::with_capacity(object.len());
        for (key, raw) in object {
            // `_meta` is reserved metadata, never a translatable entry.
            if key == META_KEY {
                continue;
            }
            let entry = build_entry(tag, key, raw)?;
            // Leak the key so the map key type is `&'static str`; bounded by the
            // number of catalogs built per process (see file header).
            let leaked_key: &'static str = Box::leak(key.clone().into_boxed_str());
            entries.insert(leaked_key, entry);
        }

        Ok(Catalog {
            tag: tag.clone(),
            // Resolve the plural rule set once, at build time; per the observability
            // contract the English fallback for a custom tag is REPORTED by the
            // caller via `plural_rules_for_tag`, not here (this crate emits no logs).
            rules: plural::plural_rules_for_tag(tag).rules(),
            entries,
            fallback: None,
        })
    }

    /// Returns this catalog with `fallback` attached as its fallback chain.
    ///
    /// Lookups that miss in this catalog consult `fallback` next. Conventionally
    /// `fallback` is the English catalog.
    #[must_use]
    pub fn with_fallback(mut self, fallback: Arc<Catalog>) -> Catalog {
        self.fallback = Some(fallback);
        self
    }

    /// Resolves a simple (non-plural) key through this catalog and its fallback.
    ///
    /// Returns `None` if the key is absent, or if it exists only as a plural
    /// entry in this catalog (a plural key is not a simple string).
    fn lookup_simple(&self, key: &str) -> Option<&'static str> {
        match self.entries.get(key) {
            Some(Entry::Simple(text)) => Some(text),
            // Present but plural: this catalog "owns" the key as a plural, so do
            // not cross into the fallback looking for a simple form.
            Some(Entry::Plural(_)) => None,
            None => self.fallback.as_ref().and_then(|f| f.lookup_simple(key)),
        }
    }

    /// Resolves a plural key for count `n` through this catalog and its fallback.
    ///
    /// Selects the CLDR category from *this* catalog's locale, then returns that
    /// form (falling back to `other`). Returns `None` if the key is absent, or if
    /// it exists only as a simple entry here.
    fn lookup_plural(&self, key: &str, n: i64) -> Option<&'static str> {
        match self.entries.get(key) {
            Some(Entry::Plural(forms)) => {
                let category = plural::plural_category(self.rules, n);
                Some(forms.select(category))
            }
            Some(Entry::Simple(_)) => None,
            None => self
                .fallback
                .as_ref()
                .and_then(|f| f.lookup_plural(key, n)),
        }
    }
}

/// Builds one [`Entry`] from a raw JSON value, leaking its strings.
///
/// A string becomes [`Entry::Simple`]; an object becomes [`Entry::Plural`]. Any
/// other JSON type is rejected.
fn build_entry(tag: &LocaleTag, key: &str, raw: &Value) -> Result<Entry, I18nError> {
    match raw {
        Value::String(text) => Ok(Entry::Simple(leak_str(text))),
        Value::Object(map) => {
            // A plural object: read the CLDR category forms. `other` is required.
            let read = |name: &str| map.get(name).and_then(Value::as_str).map(leak_str);
            let other = read(plural::OTHER).ok_or_else(|| I18nError::InvalidValue {
                tag: tag.as_str().to_owned(),
                key: key.to_owned(),
                reason: "plural object is missing the required \"other\" form",
            })?;
            Ok(Entry::Plural(PluralForms {
                one: read(plural::ONE),
                few: read(plural::FEW),
                many: read(plural::MANY),
                other,
            }))
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => {
            Err(I18nError::InvalidValue {
                tag: tag.as_str().to_owned(),
                key: key.to_owned(),
                reason: "value must be a string or a plural object",
            })
        }
    }
}

/// Leaks a string slice into a `'static` reference (bounded load-time leak).
fn leak_str(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}

/// The process-global active catalog. Wait-free reads via `ArcSwap`.
static ACTIVE: OnceLock<ArcSwap<Catalog>> = OnceLock::new();

/// Returns the global active-catalog slot, initializing it to an empty English
/// catalog on first use so lookups before `install`/`set_locale` return `None`
/// (and `t!` returns the key verbatim) instead of panicking.
fn active() -> &'static ArcSwap<Catalog> {
    ACTIVE.get_or_init(|| {
        ArcSwap::from_pointee(Catalog {
            tag: LocaleTag::english(),
            rules: PluralRules::En,
            entries: HashMap::new(),
            fallback: None,
        })
    })
}

/// Installs `catalog` as the process-global active catalog.
///
/// Wait-free for concurrent readers: an in-flight `lookup` either sees the old or
/// the new catalog, and any `&'static str` it already returned stays valid.
pub fn install(catalog: Catalog) {
    active().store(Arc::new(catalog));
}

/// Loads the embedded catalog for `tag` and installs it (with an English
/// fallback, unless `tag` is English itself).
///
/// This is the EMBEDDED-only install path (no disk access): the five shipped locales
/// (`en`, `ru`, `es`, `fr`, `pt`) have an embedded source. The on-disk layer
/// (`locale_store` in the binary) is what loads custom `<tag>.json` files; this is its
/// embedded fallback and the wasm path.
///
/// # Errors
/// [`I18nError::NoCatalog`] if `tag` has no embedded catalog (any tag outside the five
/// shipped locales), or any [`Catalog::from_json_str`] error if a bundled source fails
/// to parse (which would be a build-time bug in the shipped JSON).
pub fn set_locale(tag: &LocaleTag) -> Result<(), I18nError> {
    let mut catalog = load_embedded_catalog(tag)?;
    if !tag.is_english() {
        let fallback = load_embedded_catalog(&LocaleTag::english())?;
        catalog = catalog.with_fallback(Arc::new(fallback));
    }
    install(catalog);
    Ok(())
}

/// Builds the catalog for `tag` from its embedded JSON source.
///
/// # Errors
/// [`I18nError::NoCatalog`] if no source is embedded for `tag`; otherwise a
/// parse/validation error from [`Catalog::from_json_str`].
fn load_embedded_catalog(tag: &LocaleTag) -> Result<Catalog, I18nError> {
    let source = embedded_locales()
        .iter()
        .find(|(embedded_tag, _)| *embedded_tag == tag.as_str())
        .map(|(_, source)| *source)
        .ok_or_else(|| I18nError::NoCatalog(tag.as_str().to_owned()))?;
    Catalog::from_json_str(tag, source)
}

/// The tag of the currently installed catalog.
///
/// Returns the English reference tag before any `install`/`set_locale` call (the
/// same empty-catalog default `lookup` reads from), so callers never observe an
/// "unset" state. Wait-free; clones the tag so the result outlives later locale
/// switches.
#[must_use]
pub fn active_locale() -> LocaleTag {
    active().load().tag().clone()
}

/// Looks up a simple key in the active catalog, walking its fallback chain.
///
/// Returns a `'static` reference valid regardless of later locale switches, or
/// `None` if the key is unknown (or is a plural-only key). Wait-free and
/// allocation-free; this is what `t!` calls.
#[must_use]
pub fn lookup(key: &str) -> Option<&'static str> {
    active().load().lookup_simple(key)
}

/// Looks up a plural key in the active catalog for count `n`, walking its
/// fallback chain.
///
/// Selects the CLDR category from the active locale and returns the matching
/// form (falling back to `other`), or `None` if the key is unknown (or is a
/// simple-only key). The returned string is a template still containing any
/// `{n}` placeholder; `tp!` interpolates it.
#[must_use]
pub fn lookup_plural(key: &str, n: i64) -> Option<&'static str> {
    active().load().lookup_plural(key, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses a tag for tests (all literals here are valid).
    fn tag(s: &str) -> LocaleTag {
        LocaleTag::parse(s).expect("test tag is valid")
    }

    #[test]
    fn embedded_catalogs_build_and_are_ru_subset_of_en() {
        let en = Catalog::from_json_str(&tag("en"), EN_JSON).unwrap();
        let ru = Catalog::from_json_str(&tag("ru"), RU_JSON).unwrap();
        for key in ru.entries.keys() {
            assert!(
                en.entries.contains_key(key),
                "ru key {key:?} absent from en reference catalog"
            );
        }
    }

    #[test]
    fn meta_key_is_not_exposed() {
        let en = Catalog::from_json_str(&tag("en"), EN_JSON).unwrap();
        assert!(!en.entries.contains_key(META_KEY));
        assert!(en.lookup_simple(META_KEY).is_none());
    }

    #[test]
    fn fallback_to_en_when_ru_missing_key() {
        // ru omits `only.in.en`; en provides it -> ru lookup returns the en value.
        let en = Catalog::from_json_str(
            &tag("en"),
            r#"{ "only.in.en": "english only", "shared": "en shared" }"#,
        )
        .unwrap();
        let ru = Catalog::from_json_str(&tag("ru"), r#"{ "shared": "ru shared" }"#)
            .unwrap()
            .with_fallback(Arc::new(en));
        assert_eq!(ru.lookup_simple("shared"), Some("ru shared"));
        assert_eq!(ru.lookup_simple("only.in.en"), Some("english only"));
        assert_eq!(ru.lookup_simple("nowhere"), None);
    }

    #[test]
    fn custom_tag_catalog_falls_back_to_en_and_uses_en_plurals() {
        // A custom `de` catalog: a missing key resolves through the en fallback,
        // and its plural rule set is English (resolved from the tag at build time).
        let en = Catalog::from_json_str(
            &tag("en"),
            r#"{ "shared": "en shared", "wiki.chars": { "one": "{n} character", "other": "{n} characters" } }"#,
        )
        .unwrap();
        let de = Catalog::from_json_str(&tag("de"), r#"{ "greeting": "Hallo" }"#)
            .unwrap()
            .with_fallback(Arc::new(en));
        assert_eq!(de.plural_rules(), PluralRules::En);
        assert_eq!(de.lookup_simple("greeting"), Some("Hallo"));
        // Missing key falls back to the English value.
        assert_eq!(de.lookup_simple("shared"), Some("en shared"));
        // Plural resolves through the fallback with English categories.
        assert_eq!(de.lookup_plural("wiki.chars", 1), Some("{n} character"));
        assert_eq!(de.lookup_plural("wiki.chars", 5), Some("{n} characters"));
    }

    #[test]
    fn unknown_key_returns_none() {
        let en = Catalog::from_json_str(&tag("en"), EN_JSON).unwrap();
        assert_eq!(en.lookup_simple("does.not.exist"), None);
    }

    #[test]
    fn plural_object_requires_other() {
        let err = Catalog::from_json_str(&tag("en"), r#"{ "k": { "one": "1" } }"#).unwrap_err();
        assert!(matches!(err, I18nError::InvalidValue { .. }));
    }

    #[test]
    fn plural_selection_uses_ru_categories() {
        // Self-contained ru fixture (no shipped-catalog dependency): the ru plural
        // rule set — resolved from the tag at build time — must select one/few/many
        // by count. Deleting a product plural key can never break this.
        let ru = Catalog::from_json_str(
            &tag("ru"),
            r#"{ "fixture.chars": { "one": "{n} символ", "few": "{n} символа", "many": "{n} символов", "other": "{n} символа" } }"#,
        )
        .unwrap();
        assert_eq!(ru.lookup_plural("fixture.chars", 1), Some("{n} символ"));
        assert_eq!(ru.lookup_plural("fixture.chars", 2), Some("{n} символа"));
        assert_eq!(ru.lookup_plural("fixture.chars", 5), Some("{n} символов"));
    }

    #[test]
    fn install_drives_global_lookup_with_fallback() {
        let _guard = crate::lock_active_catalog_for_test();
        // Self-contained fixtures (no shipped-catalog dependency): a ru catalog with
        // an en fallback. install() must swap the global slot and lookup() must read
        // it, resolving a ru-missing key through the en fallback.
        let en = Catalog::from_json_str(
            &tag("en"),
            r#"{ "fixture.k": "en value", "fixture.only_en": "en only" }"#,
        )
        .unwrap();
        let ru = Catalog::from_json_str(&tag("ru"), r#"{ "fixture.k": "ru value" }"#)
            .unwrap()
            .with_fallback(Arc::new(en));
        install(ru);
        assert_eq!(lookup("fixture.k"), Some("ru value"));
        assert_eq!(lookup("fixture.only_en"), Some("en only"));

        // Swapping to an en-only catalog is observed by the next lookup.
        let en_only =
            Catalog::from_json_str(&tag("en"), r#"{ "fixture.k": "en value" }"#).unwrap();
        install(en_only);
        assert_eq!(lookup("fixture.k"), Some("en value"));
    }

    #[test]
    fn every_shipped_locale_has_an_embedded_catalog() {
        // Guards the `EMBEDDED` table against a locale JSON being added to `locales/`
        // but never registered (or vice versa).
        let _guard = crate::lock_active_catalog_for_test();
        for shipped in ["en", "ru", "es", "fr", "pt"] {
            assert!(
                set_locale(&tag(shipped)).is_ok(),
                "shipped locale {shipped} has no embedded catalog"
            );
        }
    }

    #[test]
    fn set_locale_without_embedded_catalog_errors() {
        let _guard = crate::lock_active_catalog_for_test();
        // A syntactically valid tag that ships no catalog: only a user-authored
        // `<tag>.json` on disk can serve it, which this embedded-only path cannot see.
        assert!(matches!(
            set_locale(&tag("nl")),
            Err(I18nError::NoCatalog(t)) if t == "nl"
        ));
        assert!(matches!(
            set_locale(&tag("de")),
            Err(I18nError::NoCatalog(t)) if t == "de"
        ));
    }
}
