# Module: crates/ms-i18n

## Purpose
GUI-free, config-free localization layer for ManhwaStudio: embedded locale
catalogs plus a wait-free, allocation-free key lookup used by the UI. This crate
owns the catalog data model and the process-global active locale; it does NOT
migrate UI strings and does NOT read files at runtime (locale JSON is embedded).
The on-disk layer — the editable `locale/` folder next to the executable, reconciled
from the embedded sources at startup (adds missing keys, never overwrites or deletes)
and loaded as the active catalog — is `ms_config::locale_store` (native only).

## Architecture
Five locales are embedded via `include_str!` (`locales/{en,ru,es,fr,pt}.json`,
`EMBEDDED` in `catalog.rs`; `en` is the reference/fallback). At load time a catalog is parsed into a
`HashMap<&'static str, Entry>` whose keys and values are `Box::leak`ed to
`&'static str`. The active catalog lives behind an `arc_swap::ArcSwap<Catalog>`,
so `lookup`/`t!` read it without a lock and return `&'static str` that outlive any
locale switch. `tf!`/`tp!` interpolate and return `String`.

Data flow: `set_locale(locale)` -> build catalog from embedded JSON (+ `en`
fallback) -> `install` swaps the `ArcSwap` -> `t!/tf!/tp!` -> `lookup`/
`lookup_plural` read the active catalog, walking the fallback chain to `en`.

## Files and submodules
- `src/lib.rs`: crate root; re-exports the public API; defines the `t!`/`tf!`/`tp!`
  macros and the `PluralCount` count-conversion trait.
- `src/locale.rs`: `LocaleTag` — the OPEN, validated catalog-identity tag (parse /
  `TryFrom` / `as_str` / `language` / `Display`). Any valid `<tag>.json` loads.
- `src/plural.rs`: `PluralRules` — the CLOSED plural rule-set enum (en/ru/es/fr/pt);
  `plural_rules_for_tag(tag)` bridge returning `PluralRulesResolution`
  (`Matched` | `FellBackToEnglish`); pure `plural_category(rules, n)` (CLDR selection).
- `src/catalog.rs`: `Catalog` (stores its `LocaleTag` + resolved `PluralRules`), entry
  model, the leak, the process-global `ArcSwap`,
  `install`/`set_locale(&LocaleTag)`/`lookup`/`lookup_plural`/`embedded_locales`/
  `active_locale()` (the installed catalog's tag — what UI code reads to load
  language-dependent assets such as the per-language `wiki/<lang>/` folder).
- `src/interpolate.rs`: `interpolate(template, args)` — named `{placeholder}` substitution.
- `src/error.rs`: `I18nError` (typed, `thiserror`) — loading-side errors only.
- `locales/`: embedded locale JSON (flat dotted keys; `_meta` reserved; values are
  strings, or plural objects with CLDR `one`/`few`/`many`/`other`, `other` required).
- `tests/key_validation.rs`: scans the repo for `t!`/`tf!`/`tp!` keys and checks them
  against `en.json`; checks `ru.json ⊆ en.json`; via `meta_holds_only_name`, that every
  locale file's `_meta` holds exactly the field `name`; and via
  `every_catalog_key_is_referenced_in_source`, that no ORPHAN key survives — every
  catalog key must appear as a bare `"key"` literal somewhere under `src/` or `crates/`
  (a permissive "quoted-key-text present" match, so runtime key-tables that hold keys as
  literals are not false-positives). It does NOT check `es`/`fr`/`pt` completeness.
- `tests/migrated_keys_resolve.rs`: proves migrated keys resolve to real text, not to
  the key itself (the `t!`-returns-key-on-miss trap).

## Contracts and invariants
- **`t!` is hot-path safe**: no lock, no allocation. It returns `&'static str`
  obtained from a load-time `Box::leak`. The leak is bounded by the number of
  locale switches per process (a handful) × catalog size; it never grows with
  lookups. `tf!`/`tp!` allocate a `String` because they interpolate.
- **No panics on miss**: an unknown key, a missing interpolation argument, or a
  malformed template returns the key/template verbatim. The crate emits no logs.
- **Fallback**: every non-`en` catalog carries an `en` fallback; lookups walk it.
- **`en` is the reference**: `ru` must not contain a key absent from `en`
  (enforced by `key_validation.rs`). Every new key is added to `en.json` AND `ru.json`
  together; `es`/`fr`/`pt` are translated separately, may lag behind, and resolve a
  missing key through the `en` fallback.
- **`_meta` holds exactly one field, `name`**: never exposed as a translatable key,
  and never stores derived state. "Untranslated" is DERIVED, not stored — a value equal
  to the Russian source is untranslated (list them with
  `tools/i18n_extract.py --untranslated <tag>`). `tests/key_validation.rs` fails the
  build on any `_meta` field other than `name`, so a write-only field cannot rot.
- **No egui/tokio/image**; deps are `serde_json`, `arc-swap`, `thiserror`.
- **Identity is OPEN, plural rules are CLOSED**. `LocaleTag` is a validated string,
  so ANY `<tag>.json` (custom languages included) loads; `set_locale(&LocaleTag)` is
  the EMBEDDED-only path (the five shipped tags) and returns `I18nError::NoCatalog` for any other
  tag. `PluralRules` is the closed enum whose `plural_category` `match` is exhaustive
  (no `_ =>`). A tag with no dedicated rule set resolves to English rules ONCE at
  catalog build; the fallback is REPORTED via `PluralRulesResolution` (the crate emits
  no logs — the binary logs it at install time), never silent.
- **`t!` never touches `LocaleTag`**: a catalog stores its resolved `PluralRules`
  (a `Copy` enum), so `lookup`/`lookup_plural` do no string hashing/allocation per call.

## Editing map
- To add a translation key, edit `locales/en.json` (reference) and `locales/ru.json`
  in the same change. Values are strings or plural objects with a required `other`.
- To ship a NEW embedded language, add its JSON and extend `EMBEDDED` in `catalog.rs`.
  Custom languages need no code change — a user `locale/<tag>.json` loads as-is.
- To add a hand-written plural rule set, add a `PluralRules` variant (this forces the
  `plural_category` match to be reconsidered) and a `plural_rules_for_tag` arm.
- To change plural rules, see `plural.rs` (and its unit tests).
- To change interpolation syntax, see `interpolate.rs`.
- To change the leak/active-catalog mechanism, see `catalog.rs`.
