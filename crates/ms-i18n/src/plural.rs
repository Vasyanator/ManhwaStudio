/*
File: crates/ms-i18n/src/plural.rs

Purpose:
The CLDR plural rule set and its selection. Holds `PluralRules` — the CLOSED,
project-owned enum of the languages that have a hand-written plural rule set — and
`plural_category`, which selects the CLDR form for a count under a rule set. Also
bridges an OPEN [`crate::LocaleTag`] to its rule set via `plural_rules_for_tag`.

Why a closed enum: catalog identity is open (any `<tag>.json` loads — see
`locale.rs`), but only these five languages have hand-written rules. Keeping them
a closed enum makes `plural_category`'s `match` exhaustive with NO `_ =>` arm, so
adding a rule set forces every selection site to be reconsidered.

Key structures:
- PluralRules           — closed set {En, Ru, Es, Fr, Pt}
- PluralRulesResolution — Matched | FellBackToEnglish (an OBSERVABLE bridge result)

Key functions:
- plural_rules_for_tag(tag) -> PluralRulesResolution  (resolve once, at build time)
- plural_category(rules, n) -> &'static str           (per-lookup category select)

Notes:
- Only integer counts are modelled (operand v == 0 always), which is what the
  `tp!` call path passes. The returned string is one of the CLDR category names
  "one" / "few" / "many" / "other" and is used as a key into a catalog's plural
  forms.
- Rules follow CLDR, with two locale specifics: French treats 0 and 1 as `one`;
  Spanish/Portuguese/English use `one` only for n == 1.
- A tag with no hand-written rule set resolves to English rules, and the fallback
  is REPORTED via `PluralRulesResolution::FellBackToEnglish` (not silently) so the
  caller can log it ONCE at catalog-install time rather than on every `tp!` call.
*/

use crate::locale::LocaleTag;

/// CLDR plural category names. A catalog's plural object is keyed by these.
pub(crate) const ONE: &str = "one";
pub(crate) const FEW: &str = "few";
pub(crate) const MANY: &str = "many";
pub(crate) const OTHER: &str = "other";

/// The closed set of languages that ship a hand-written CLDR plural rule set.
///
/// This is NOT catalog identity (see [`crate::LocaleTag`]); it is only the plural
/// rule set. A catalog for any tag resolves to one of these via
/// [`plural_rules_for_tag`], defaulting to `En` for tags without dedicated rules.
///
/// Adding a variant here forces [`plural_category`] (and any other `match` on
/// `PluralRules`) to be reconsidered — that is the whole point of the closed enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluralRules {
    /// English rules (the reference and universal fallback): `one` iff n == 1.
    En,
    /// Russian rules: one/few/many by the i%10 and i%100 CLDR conditions.
    Ru,
    /// Spanish rules: `one` iff n == 1.
    Es,
    /// French rules: `one` for n in {0, 1}.
    Fr,
    /// Portuguese rules: `one` iff n == 1.
    Pt,
}

/// The result of resolving a [`LocaleTag`] to a [`PluralRules`] set.
///
/// The variant records WHETHER the tag had a dedicated rule set, so the caller can
/// surface the English fallback (log it) once at install time instead of it being
/// silent. Read the rules with [`PluralRulesResolution::rules`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluralRulesResolution {
    /// The tag's language has a dedicated hand-written rule set.
    Matched(PluralRules),
    /// The tag has no dedicated rule set; English rules are used (report this).
    FellBackToEnglish(PluralRules),
}

impl PluralRulesResolution {
    /// The resolved rule set, regardless of whether it was an exact match or the
    /// English fallback.
    #[must_use]
    pub fn rules(self) -> PluralRules {
        match self {
            PluralRulesResolution::Matched(rules)
            | PluralRulesResolution::FellBackToEnglish(rules) => rules,
        }
    }

    /// Whether the rule set came from the English fallback (a tag with no
    /// dedicated rules). The caller logs this once when installing the catalog.
    #[must_use]
    pub fn fell_back_to_english(self) -> bool {
        matches!(self, PluralRulesResolution::FellBackToEnglish(_))
    }
}

/// Resolves a [`LocaleTag`] to its plural rule set, reporting whether the English
/// fallback was used.
///
/// Matches on the tag's LANGUAGE subtag (so `pt-BR` uses Portuguese rules). A tag
/// whose language has no dedicated rule set resolves to
/// [`PluralRulesResolution::FellBackToEnglish`] — an OBSERVABLE result, so the
/// English fallback is never silent. Pure and cheap; call it once per catalog
/// build, not per lookup.
#[must_use]
pub fn plural_rules_for_tag(tag: &LocaleTag) -> PluralRulesResolution {
    // Match on `&str` (the language subtag), so a `_` arm here is required and is
    // NOT the enum catch-all the project forbids — `plural_category` below is the
    // exhaustive `match` on `PluralRules`.
    match tag.language() {
        "en" => PluralRulesResolution::Matched(PluralRules::En),
        "ru" => PluralRulesResolution::Matched(PluralRules::Ru),
        "es" => PluralRulesResolution::Matched(PluralRules::Es),
        "fr" => PluralRulesResolution::Matched(PluralRules::Fr),
        "pt" => PluralRulesResolution::Matched(PluralRules::Pt),
        _ => PluralRulesResolution::FellBackToEnglish(PluralRules::En),
    }
}

/// Selects the CLDR plural category for `n` under `rules`.
///
/// Returns one of `"one"` / `"few"` / `"many"` / `"other"`. `n` is treated by
/// absolute value (CLDR operand `n`), so a negative count selects the same form
/// as its magnitude. Pure function; no allocation, no global state. The `match`
/// on `PluralRules` is exhaustive by design.
///
/// Rule sets (integer counts only, operand `v == 0`):
/// - `Ru`: `one` for i%10==1 && i%100!=11; `few` for i%10 in 2..=4 && i%100 not in
///   12..=14; otherwise `many`.
/// - `En`/`Es`/`Pt`: `one` iff n == 1, else `other`.
/// - `Fr`: `one` for n in {0, 1}, else `other`.
#[must_use]
pub fn plural_category(rules: PluralRules, n: i64) -> &'static str {
    // CLDR operand `i` is the integer absolute value; `unsigned_abs` avoids the
    // i64::MIN overflow that plain negation would hit.
    let i = n.unsigned_abs();
    match rules {
        PluralRules::Ru => {
            let rem10 = i % 10;
            let rem100 = i % 100;
            if rem10 == 1 && rem100 != 11 {
                ONE
            } else if (2..=4).contains(&rem10) && !(12..=14).contains(&rem100) {
                FEW
            } else {
                // Covers rem10 == 0, rem10 in 5..=9, and the 11..=14 exceptions.
                MANY
            }
        }
        // English, Spanish and Portuguese: `one` only for exactly 1.
        PluralRules::En | PluralRules::Es | PluralRules::Pt => {
            if i == 1 {
                ONE
            } else {
                OTHER
            }
        }
        // French: 0 and 1 both take `one`.
        PluralRules::Fr => {
            if i == 0 || i == 1 {
                ONE
            } else {
                OTHER
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(s: &str) -> LocaleTag {
        LocaleTag::parse(s).expect("test tag is valid")
    }

    #[test]
    fn russian_categories() {
        assert_eq!(plural_category(PluralRules::Ru, 1), ONE);
        assert_eq!(plural_category(PluralRules::Ru, 2), FEW);
        assert_eq!(plural_category(PluralRules::Ru, 5), MANY);
        assert_eq!(plural_category(PluralRules::Ru, 11), MANY);
        assert_eq!(plural_category(PluralRules::Ru, 21), ONE);
        assert_eq!(plural_category(PluralRules::Ru, 22), FEW);
        assert_eq!(plural_category(PluralRules::Ru, 25), MANY);
    }

    #[test]
    fn english_categories() {
        assert_eq!(plural_category(PluralRules::En, 0), OTHER);
        assert_eq!(plural_category(PluralRules::En, 1), ONE);
        assert_eq!(plural_category(PluralRules::En, 2), OTHER);
    }

    #[test]
    fn french_categories() {
        assert_eq!(plural_category(PluralRules::Fr, 0), ONE);
        assert_eq!(plural_category(PluralRules::Fr, 1), ONE);
        assert_eq!(plural_category(PluralRules::Fr, 2), OTHER);
    }

    #[test]
    fn portuguese_categories() {
        assert_eq!(plural_category(PluralRules::Pt, 0), OTHER);
        assert_eq!(plural_category(PluralRules::Pt, 1), ONE);
        assert_eq!(plural_category(PluralRules::Pt, 2), OTHER);
    }

    #[test]
    fn spanish_categories() {
        assert_eq!(plural_category(PluralRules::Es, 0), OTHER);
        assert_eq!(plural_category(PluralRules::Es, 1), ONE);
        assert_eq!(plural_category(PluralRules::Es, 2), OTHER);
    }

    #[test]
    fn known_tags_match_their_rule_set() {
        assert_eq!(
            plural_rules_for_tag(&tag("ru")),
            PluralRulesResolution::Matched(PluralRules::Ru)
        );
        // Regional tag resolves by language subtag.
        assert_eq!(
            plural_rules_for_tag(&tag("pt-BR")),
            PluralRulesResolution::Matched(PluralRules::Pt)
        );
        assert!(!plural_rules_for_tag(&tag("ru")).fell_back_to_english());
    }

    #[test]
    fn custom_tag_falls_back_to_english_observably() {
        let resolution = plural_rules_for_tag(&tag("de"));
        // The fallback is a distinct, assertable variant — not a silent default.
        assert_eq!(
            resolution,
            PluralRulesResolution::FellBackToEnglish(PluralRules::En)
        );
        assert!(resolution.fell_back_to_english());
        assert_eq!(resolution.rules(), PluralRules::En);
    }
}
