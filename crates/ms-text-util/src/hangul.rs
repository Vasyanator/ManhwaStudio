/*
File: crates/ms-text-util/src/hangul.rs

Purpose:
Modern-Hangul syllable arithmetic: composing a precomposed syllable from its
(choseong, jungseong, jongseong) indices, splitting one back into those indices,
and the compatibility-jamo captions used to LABEL each index in UI.

The module is pure index arithmetic over the Unicode Hangul Syllables block
(U+AC00..=U+D7A3, 11 172 syllables): S = 0xAC00 + (L * 21 + V) * 28 + T.

Key constants:
- `SYLLABLE_BASE`, `CHOSEONG_COUNT`, `JUNGSEONG_COUNT`, `JONGSEONG_COUNT`.
- `CHOSEONG_COMPAT` / `JUNGSEONG_COMPAT` / `JONGSEONG_COMPAT` — compatibility
  jamo (U+3131..=U+3163) indexed by L / V / T index.

Key functions:
- `compose`, `decompose`, `is_syllable`.

Contract:
Config-free, GUI-free, dependency-free and total: no function panics and no
index arithmetic wraps — out-of-range input yields `None`. This module is
deliberately independent of `TextLanguage` / `ScriptGroup` and of the
process-global typesetting language; Hangul is not a typesetting language here,
it is character data.

Notes:
The compatibility-jamo tables are NOT contiguous with the L/T indices (the
compatibility block interleaves single and cluster consonants in a different
order), so they are written out explicitly rather than derived by offset. Only
the jungseong table happens to be contiguous (U+314F..=U+3163) and is still
written out for symmetry and reviewability.
*/

/// First code point of the Unicode Hangul Syllables block (the syllable `가`).
///
/// A modern syllable is `SYLLABLE_BASE + (L * JUNGSEONG_COUNT + V) * JONGSEONG_COUNT + T`.
pub const SYLLABLE_BASE: u32 = 0xAC00;

/// Number of lead consonants (choseong), L index range `0..19`.
pub const CHOSEONG_COUNT: usize = 19;

/// Number of vowels (jungseong), V index range `0..21`.
pub const JUNGSEONG_COUNT: usize = 21;

/// Number of final-consonant slots (jongseong), T index range `0..28`.
///
/// Index 0 means "no final consonant"; indices `1..28` are the 27 real finals.
pub const JONGSEONG_COUNT: usize = 28;

/// Total number of precomposed modern syllables (`19 * 21 * 28`).
const SYLLABLE_COUNT: u32 = 11_172;

/// Compatibility-jamo caption for each lead-consonant (choseong) index.
///
/// Indexed by the L index used by [`compose`] / [`decompose`]. These are
/// compatibility jamo (U+3131..=U+314E), not conjoining choseong (U+1100..),
/// because compatibility jamo are what a Korean keyboard shows and what renders
/// standalone in a text field.
pub const CHOSEONG_COMPAT: [char; CHOSEONG_COUNT] = [
    'ㄱ', // 0  U+3131 KIYEOK
    'ㄲ', // 1  U+3132 SSANGKIYEOK
    'ㄴ', // 2  U+3134 NIEUN
    'ㄷ', // 3  U+3137 TIKEUT
    'ㄸ', // 4  U+3138 SSANGTIKEUT
    'ㄹ', // 5  U+3139 RIEUL
    'ㅁ', // 6  U+3141 MIEUM
    'ㅂ', // 7  U+3142 PIEUP
    'ㅃ', // 8  U+3143 SSANGPIEUP
    'ㅅ', // 9  U+3145 SIOS
    'ㅆ', // 10 U+3146 SSANGSIOS
    'ㅇ', // 11 U+3147 IEUNG
    'ㅈ', // 12 U+3148 CIEUC
    'ㅉ', // 13 U+3149 SSANGCIEUC
    'ㅊ', // 14 U+314A CHIEUCH
    'ㅋ', // 15 U+314B KHIEUKH
    'ㅌ', // 16 U+314C THIEUTH
    'ㅍ', // 17 U+314D PHIEUPH
    'ㅎ', // 18 U+314E HIEUH
];

/// Compatibility-jamo caption for each vowel (jungseong) index.
///
/// Indexed by the V index used by [`compose`] / [`decompose`]. This range is
/// contiguous in the compatibility block (U+314F..=U+3163).
pub const JUNGSEONG_COMPAT: [char; JUNGSEONG_COUNT] = [
    'ㅏ', // 0  U+314F A
    'ㅐ', // 1  U+3150 AE
    'ㅑ', // 2  U+3151 YA
    'ㅒ', // 3  U+3152 YAE
    'ㅓ', // 4  U+3153 EO
    'ㅔ', // 5  U+3154 E
    'ㅕ', // 6  U+3155 YEO
    'ㅖ', // 7  U+3156 YE
    'ㅗ', // 8  U+3157 O
    'ㅘ', // 9  U+3158 WA
    'ㅙ', // 10 U+3159 WAE
    'ㅚ', // 11 U+315A OE
    'ㅛ', // 12 U+315B YO
    'ㅜ', // 13 U+315C U
    'ㅝ', // 14 U+315D WEO
    'ㅞ', // 15 U+315E WE
    'ㅟ', // 16 U+315F WI
    'ㅠ', // 17 U+3160 YU
    'ㅡ', // 18 U+3161 EU
    'ㅢ', // 19 U+3162 YI
    'ㅣ', // 20 U+3163 I
];

/// Compatibility-jamo caption for each final-consonant (jongseong) index.
///
/// Indexed by the T index used by [`compose`] / [`decompose`]. Entry `[0]` is
/// `None`: T index 0 is the "no final consonant" slot and has no jamo to show,
/// so the caller must render its own placeholder rather than a character.
pub const JONGSEONG_COMPAT: [Option<char>; JONGSEONG_COUNT] = [
    None,      // 0  no final consonant
    Some('ㄱ'), // 1  U+3131 KIYEOK
    Some('ㄲ'), // 2  U+3132 SSANGKIYEOK
    Some('ㄳ'), // 3  U+3133 KIYEOK-SIOS
    Some('ㄴ'), // 4  U+3134 NIEUN
    Some('ㄵ'), // 5  U+3135 NIEUN-CIEUC
    Some('ㄶ'), // 6  U+3136 NIEUN-HIEUH
    Some('ㄷ'), // 7  U+3137 TIKEUT
    Some('ㄹ'), // 8  U+3139 RIEUL
    Some('ㄺ'), // 9  U+313A RIEUL-KIYEOK
    Some('ㄻ'), // 10 U+313B RIEUL-MIEUM
    Some('ㄼ'), // 11 U+313C RIEUL-PIEUP
    Some('ㄽ'), // 12 U+313D RIEUL-SIOS
    Some('ㄾ'), // 13 U+313E RIEUL-THIEUTH
    Some('ㄿ'), // 14 U+313F RIEUL-PHIEUPH
    Some('ㅀ'), // 15 U+3140 RIEUL-HIEUH
    Some('ㅁ'), // 16 U+3141 MIEUM
    Some('ㅂ'), // 17 U+3142 PIEUP
    Some('ㅄ'), // 18 U+3144 PIEUP-SIOS
    Some('ㅅ'), // 19 U+3145 SIOS
    Some('ㅆ'), // 20 U+3146 SSANGSIOS
    Some('ㅇ'), // 21 U+3147 IEUNG
    Some('ㅈ'), // 22 U+3148 CIEUC
    Some('ㅊ'), // 23 U+314A CHIEUCH
    Some('ㅋ'), // 24 U+314B KHIEUKH
    Some('ㅌ'), // 25 U+314C THIEUTH
    Some('ㅍ'), // 26 U+314D PHIEUPH
    Some('ㅎ'), // 27 U+314E HIEUH
];

/// Composes a modern Hangul syllable from its jamo indices.
///
/// `lead` is the choseong index (`0..CHOSEONG_COUNT`), `vowel` the jungseong
/// index (`0..JUNGSEONG_COUNT`), `tail` the jongseong index
/// (`0..JONGSEONG_COUNT`, where `0` means "no final consonant").
///
/// Returns the precomposed syllable in U+AC00..=U+D7A3, or `None` if any index
/// is out of range. Never panics and never wraps: every in-range combination
/// maps to a valid scalar value, so the result is always a real character.
#[must_use]
pub fn compose(lead: usize, vowel: usize, tail: usize) -> Option<char> {
    if lead >= CHOSEONG_COUNT || vowel >= JUNGSEONG_COUNT || tail >= JONGSEONG_COUNT {
        return None;
    }
    // Indices are bounded above by the counts, so each conversion is lossless
    // even on a 16-bit `usize`; `try_from` keeps that guaranteed rather than assumed.
    let lead = u32::try_from(lead).ok()?;
    let vowel = u32::try_from(vowel).ok()?;
    let tail = u32::try_from(tail).ok()?;
    let jungseong_count = u32::try_from(JUNGSEONG_COUNT).ok()?;
    let jongseong_count = u32::try_from(JONGSEONG_COUNT).ok()?;

    let offset = (lead * jungseong_count + vowel) * jongseong_count + tail;
    char::from_u32(SYLLABLE_BASE + offset)
}

/// Splits a precomposed modern Hangul syllable into its `(lead, vowel, tail)` indices.
///
/// Accepts only the Hangul Syllables block (U+AC00..=U+D7A3). Returns `None` for
/// anything else — including compatibility jamo (U+3131..=U+3163) and conjoining
/// jamo (U+1100 block), which are jamo but not syllables.
///
/// The returned indices always satisfy `compose(l, v, t) == Some(c)`.
#[must_use]
pub fn decompose(c: char) -> Option<(usize, usize, usize)> {
    let offset = u32::from(c).checked_sub(SYLLABLE_BASE)?;
    if offset >= SYLLABLE_COUNT {
        return None;
    }
    let jungseong_count = u32::try_from(JUNGSEONG_COUNT).ok()?;
    let jongseong_count = u32::try_from(JONGSEONG_COUNT).ok()?;

    let tail = offset % jongseong_count;
    let rest = offset / jongseong_count;
    let vowel = rest % jungseong_count;
    let lead = rest / jungseong_count;

    Some((
        usize::try_from(lead).ok()?,
        usize::try_from(vowel).ok()?,
        usize::try_from(tail).ok()?,
    ))
}

/// Reports whether `c` is a precomposed modern Hangul syllable (U+AC00..=U+D7A3).
///
/// Compatibility jamo and conjoining jamo are NOT syllables and yield `false`.
#[must_use]
pub fn is_syllable(c: char) -> bool {
    (SYLLABLE_BASE..SYLLABLE_BASE + SYLLABLE_COUNT).contains(&u32::from(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn counts_match_the_syllable_block() {
        assert_eq!(
            u32::try_from(CHOSEONG_COUNT * JUNGSEONG_COUNT * JONGSEONG_COUNT),
            Ok(SYLLABLE_COUNT)
        );
    }

    #[test]
    fn composes_known_syllables() {
        assert_eq!(compose(0, 0, 0), Some('가'));
        assert_eq!(compose(18, 20, 27), Some('힣'));
        // ㅇ + ㅏ + ㅇ
        assert_eq!(compose(11, 0, 21), Some('앙'));
    }

    #[test]
    fn round_trips_every_syllable() {
        // The acceptance gate: all 11 172 syllables, in block order.
        let mut index = 0u32;
        for lead in 0..CHOSEONG_COUNT {
            for vowel in 0..JUNGSEONG_COUNT {
                for tail in 0..JONGSEONG_COUNT {
                    let composed =
                        compose(lead, vowel, tail).expect("in-range indices must compose");
                    let expected = char::from_u32(SYLLABLE_BASE + index)
                        .expect("syllable block contains no surrogates");
                    assert_eq!(composed, expected, "at L={lead} V={vowel} T={tail}");
                    assert!(is_syllable(composed));
                    assert_eq!(decompose(composed), Some((lead, vowel, tail)));
                    index += 1;
                }
            }
        }
        assert_eq!(index, SYLLABLE_COUNT);
    }

    #[test]
    fn out_of_range_indices_return_none() {
        assert_eq!(compose(CHOSEONG_COUNT, 0, 0), None);
        assert_eq!(compose(0, JUNGSEONG_COUNT, 0), None);
        assert_eq!(compose(0, 0, JONGSEONG_COUNT), None);
        assert_eq!(compose(usize::MAX, usize::MAX, usize::MAX), None);
        // The last valid combination still composes — the bound is exclusive, not off by one.
        assert!(compose(CHOSEONG_COUNT - 1, JUNGSEONG_COUNT - 1, JONGSEONG_COUNT - 1).is_some());
    }

    #[test]
    fn decompose_rejects_non_syllables() {
        // Compatibility jamo (the whole block, U+3131..=U+3163).
        for code in 0x3131..=0x3163u32 {
            let c = char::from_u32(code).expect("compatibility jamo block is valid");
            assert_eq!(decompose(c), None, "U+{code:04X}");
            assert!(!is_syllable(c), "U+{code:04X}");
        }
        // Conjoining jamo (U+1100 block): choseong, jungseong and jongseong leads.
        for code in [0x1100u32, 0x1161, 0x11A8, 0x11FF] {
            let c = char::from_u32(code).expect("conjoining jamo block is valid");
            assert_eq!(decompose(c), None, "U+{code:04X}");
            assert!(!is_syllable(c), "U+{code:04X}");
        }
        // Latin, digits, punctuation and the block's immediate neighbours.
        for c in ['A', 'z', '0', ' ', '.', '\u{ABFF}', '\u{D7A4}'] {
            assert_eq!(decompose(c), None, "{c:?}");
            assert!(!is_syllable(c), "{c:?}");
        }
        // The block edges themselves ARE syllables.
        assert_eq!(decompose('가'), Some((0, 0, 0)));
        assert_eq!(decompose('힣'), Some((18, 20, 27)));
    }

    #[test]
    fn compat_tables_are_distinct_compatibility_jamo() {
        let mut seen = HashSet::new();
        let entries = CHOSEONG_COMPAT
            .iter()
            .copied()
            .chain(JUNGSEONG_COMPAT.iter().copied());
        for c in entries {
            let code = u32::from(c);
            assert!(
                (0x3131..=0x3163).contains(&code),
                "{c:?} (U+{code:04X}) is outside the compatibility-jamo block"
            );
        }
        // Within one table, every caption must be distinct — a duplicate would make
        // two different indices indistinguishable in the UI.
        for c in CHOSEONG_COMPAT {
            assert!(seen.insert(c), "duplicate choseong caption {c:?}");
        }
        seen.clear();
        for c in JUNGSEONG_COMPAT {
            assert!(seen.insert(c), "duplicate jungseong caption {c:?}");
        }
        seen.clear();
        assert_eq!(JONGSEONG_COMPAT[0], None, "T index 0 has no jamo caption");
        for (index, entry) in JONGSEONG_COMPAT.iter().enumerate().skip(1) {
            let c = entry.unwrap_or_else(|| panic!("missing jongseong caption at index {index}"));
            let code = u32::from(c);
            assert!(
                (0x3131..=0x3163).contains(&code),
                "{c:?} (U+{code:04X}) is outside the compatibility-jamo block"
            );
            assert!(seen.insert(c), "duplicate jongseong caption {c:?}");
        }
    }

    #[test]
    fn compat_captions_agree_with_composed_syllables() {
        // Cross-check the tables against real syllables: a caption table that is
        // merely self-consistent could still be permuted, so anchor a few entries
        // to syllables whose spelling is unambiguous.
        assert_eq!(CHOSEONG_COMPAT[11], 'ㅇ');
        assert_eq!(JUNGSEONG_COMPAT[0], 'ㅏ');
        assert_eq!(JONGSEONG_COMPAT[21], Some('ㅇ'));
        assert_eq!(compose(11, 0, 21), Some('앙'));

        assert_eq!(CHOSEONG_COMPAT[5], 'ㄹ');
        assert_eq!(JUNGSEONG_COMPAT[18], 'ㅡ');
        assert_eq!(JONGSEONG_COMPAT[16], Some('ㅁ'));
        // ㄹ + ㅡ + ㅁ = 름
        assert_eq!(compose(5, 18, 16), Some('름'));

        assert_eq!(CHOSEONG_COMPAT[0], 'ㄱ');
        assert_eq!(JONGSEONG_COMPAT[1], Some('ㄱ'));
        // ㄱ + ㅏ + ㄱ = 각
        assert_eq!(compose(0, 0, 1), Some('각'));
    }
}
