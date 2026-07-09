/*
File: crates/ms-i18n/src/locale.rs

Purpose:
The `LocaleTag` type: the OPEN, validated language-tag that identifies a catalog.
Any `<tag>.json` a user drops into the on-disk `locale/` folder is loadable, so
catalog identity must not be a closed set. `LocaleTag` is the owned, validated
string form of that identity.

This is deliberately separate from the CLDR plural-rule set (`plural::PluralRules`),
which IS a closed enum: catalog identity is open (any valid tag loads), but only a
handful of languages have hand-written plural rules. The bridge from a tag to its
rule set is `plural::plural_rules_for_tag`.

Key structures:
- LocaleTag (owned, validated: language subtag + optional region suffix)

Grammar / validation (see `LocaleTag::parse`):
- non-empty;
- a language subtag of ASCII-lowercase letters only (uppercase is REJECTED, not
  normalized, so a tag maps 1:1 to a `<tag>.json` filename on case-sensitive
  filesystems);
- an OPTIONAL region suffix after a single `-` or `_` separator, of ASCII
  alphanumeric characters (so `pt-BR` / `pt_BR` / `es-419` are accepted);
- anything else — a path separator, a dot, `..`, a second separator — is rejected
  with a typed [`I18nError::InvalidTag`], never silently accepted.
*/

use crate::error::I18nError;

/// A validated, owned interface-language tag identifying a catalog.
///
/// Open set: any string that satisfies [`LocaleTag::parse`]'s grammar is a valid
/// tag, so a user-authored `locale/<tag>.json` (e.g. `de.json`) loads. The tag is
/// NOT tied to the closed [`crate::PluralRules`] enum; use
/// [`crate::plural_rules_for_tag`] to resolve a tag to its plural rule set.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocaleTag(String);

impl LocaleTag {
    /// Parses and validates a locale tag.
    ///
    /// Accepts a non-empty ASCII-lowercase language subtag with an optional
    /// region suffix (`pt-BR`, `pt_BR`, `es-419`). The language subtag is
    /// rejected (not normalized) when it contains uppercase, so `EN` is an error
    /// while `pt-BR`'s uppercase REGION is allowed.
    ///
    /// # Errors
    /// [`I18nError::InvalidTag`] with a `reason` describing the violated rule for
    /// an empty tag, an uppercase/non-letter language subtag, a bad region
    /// suffix, or any tag containing a path separator or dot.
    pub fn parse(tag: &str) -> Result<LocaleTag, I18nError> {
        let invalid = |reason: &'static str| {
            Err(I18nError::InvalidTag {
                tag: tag.to_owned(),
                reason,
            })
        };
        if tag.is_empty() {
            return invalid("tag is empty");
        }
        // Split into a language subtag and an optional region suffix on the FIRST
        // '-' or '_'. Only one separator is supported; a second one lands in the
        // region part and is rejected there (it is not ASCII-alphanumeric).
        let (lang, region) = match tag.find(['-', '_']) {
            Some(idx) => (&tag[..idx], Some(&tag[idx + 1..])),
            None => (tag, None),
        };
        // Language subtag: non-empty, ASCII-lowercase letters only. Rejecting
        // (rather than lowercasing) keeps the tag byte-identical to its
        // `<tag>.json` filename on case-sensitive filesystems.
        if lang.is_empty() || !lang.bytes().all(|b| b.is_ascii_lowercase()) {
            return invalid("language subtag must be non-empty ASCII lowercase letters");
        }
        if let Some(region) = region {
            // Region suffix: non-empty ASCII alphanumeric — letters of any case or
            // digits (`BR`, `br`, `419`). This is the one place uppercase is
            // allowed. A path separator or dot here is not alphanumeric, so
            // `../etc` and `en.json` are rejected via this or the language check.
            if region.is_empty() || !region.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return invalid("region subtag must be non-empty ASCII alphanumeric");
            }
        }
        Ok(LocaleTag(tag.to_owned()))
    }

    /// The English reference tag (`"en"`), the universal fallback locale.
    ///
    /// Infallible constructor for the compile-time-known reference tag, so
    /// internal callers never parse-and-handle a constant that cannot fail.
    #[must_use]
    pub fn english() -> LocaleTag {
        // `"en"` satisfies the grammar by construction.
        LocaleTag("en".to_owned())
    }

    /// The raw tag string (`"en"`, `"ru"`, `"pt-BR"`, a custom `"de"`, …).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The language subtag: the part before any region separator (`"pt"` for
    /// `"pt-BR"`). Used to resolve the plural rule set.
    #[must_use]
    pub fn language(&self) -> &str {
        match self.0.find(['-', '_']) {
            Some(idx) => &self.0[..idx],
            None => &self.0,
        }
    }

    /// Whether this is exactly the English reference tag (`"en"`).
    ///
    /// Used to decide whether to attach the English fallback catalog (English
    /// must not fall back to itself). A regional English such as `"en-US"` is not
    /// the reference tag and does get the `en` fallback.
    #[must_use]
    pub fn is_english(&self) -> bool {
        self.0 == "en"
    }
}

impl TryFrom<&str> for LocaleTag {
    type Error = I18nError;

    /// Same contract as [`LocaleTag::parse`].
    fn try_from(tag: &str) -> Result<LocaleTag, I18nError> {
        LocaleTag::parse(tag)
    }
}

impl core::fmt::Display for LocaleTag {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_language_tags() {
        assert_eq!(LocaleTag::parse("en").unwrap().as_str(), "en");
        assert_eq!(LocaleTag::parse("ru").unwrap().as_str(), "ru");
        assert_eq!(LocaleTag::parse("de").unwrap().as_str(), "de");
    }

    #[test]
    fn accepts_region_suffix_with_both_separators() {
        assert_eq!(LocaleTag::parse("pt-BR").unwrap().as_str(), "pt-BR");
        assert_eq!(LocaleTag::parse("pt_BR").unwrap().as_str(), "pt_BR");
        // Numeric UN M.49 region.
        assert_eq!(LocaleTag::parse("es-419").unwrap().as_str(), "es-419");
    }

    #[test]
    fn language_subtag_is_the_part_before_the_region() {
        assert_eq!(LocaleTag::parse("pt-BR").unwrap().language(), "pt");
        assert_eq!(LocaleTag::parse("en").unwrap().language(), "en");
    }

    #[test]
    fn rejects_empty() {
        assert!(matches!(
            LocaleTag::parse(""),
            Err(I18nError::InvalidTag { .. })
        ));
    }

    #[test]
    fn rejects_uppercase_language() {
        // Decision: uppercase language subtags are REJECTED (not normalized), so a
        // tag maps 1:1 to a case-sensitive filename.
        assert!(matches!(
            LocaleTag::parse("EN"),
            Err(I18nError::InvalidTag { .. })
        ));
    }

    #[test]
    fn rejects_path_separators_and_dots() {
        for bad in ["../etc/passwd", "en/ru", "en\\ru", "en.json", ".."] {
            assert!(
                matches!(LocaleTag::parse(bad), Err(I18nError::InvalidTag { .. })),
                "tag {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn english_helper_is_the_reference_tag() {
        assert!(LocaleTag::english().is_english());
        assert!(!LocaleTag::parse("ru").unwrap().is_english());
        // Regional English is not the bare reference tag.
        assert!(!LocaleTag::parse("en-US").unwrap().is_english());
    }
}
