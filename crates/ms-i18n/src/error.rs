/*
File: crates/ms-i18n/src/error.rs

Purpose:
Typed public error surface for the `ms-i18n` catalog layer. Every fallible
public entry point (`LocaleTag::parse`, `Catalog::from_json_str`, `set_locale`)
returns `I18nError`, so callers can distinguish "syntactically invalid tag", "no
catalog for this tag yet", and "the catalog JSON is malformed" without string
matching.

Notes:
- Uses `thiserror` for the `Display`/`Error` impls (project contract for public
  library errors).
- Lookup (`lookup` / `lookup_plural` / `t!` / `tf!` / `tp!`) is intentionally
  infallible: on a miss it returns the key/template verbatim rather than an
  error, because it runs on the render hot path. Only *loading* a catalog can
  fail, which is what this type covers.
*/

/// Errors produced while parsing tags or building/installing locale catalogs.
///
/// Returned only by the loading side of the API. The lookup side never errors;
/// see the module header for the rationale.
#[derive(Debug, thiserror::Error)]
pub enum I18nError {
    /// The string is not a syntactically valid locale tag (empty, an uppercase
    /// language subtag, a path separator, a dot, …). `reason` names the specific
    /// rule that was violated. See [`crate::LocaleTag::parse`] for the grammar.
    #[error("invalid locale tag {tag:?}: {reason}")]
    InvalidTag { tag: String, reason: &'static str },

    /// The tag is syntactically valid but ships no embedded catalog (any tag
    /// other than the compiled-in `en`/`ru`, including a custom on-disk tag that
    /// has no disk file to load from).
    #[error("no embedded catalog for locale tag {0}")]
    NoCatalog(String),

    /// The catalog source is not valid JSON. `tag` is the tag being loaded.
    #[error("failed to parse catalog JSON for locale tag {tag}: {source}")]
    Parse {
        tag: String,
        #[source]
        source: serde_json::Error,
    },

    /// The catalog JSON root is not a JSON object (`{ ... }`).
    #[error("catalog JSON root for locale tag {0} is not an object")]
    NotObject(String),

    /// A key maps to a value the catalog format does not allow. `reason` names
    /// the specific violation (e.g. a non-string simple value, or a plural
    /// object missing its required `other` form).
    #[error("invalid catalog value for key {key:?} in locale tag {tag}: {reason}")]
    InvalidValue {
        tag: String,
        key: String,
        reason: &'static str,
    },
}
