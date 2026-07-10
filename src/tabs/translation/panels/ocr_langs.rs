/*
File: ocr_langs.rs

Purpose:
Static catalogs of OCR language options for the EasyOCR and PaddleOCR engines.

Each entry is `(wire_code, display_key)`: the wire code is the stable engine
identifier persisted in project settings and sent to the backend, and the
display key is a stable i18n catalog key resolved to a localized label at
render time via `lang_label`. Only the wire code is identity; the label is
free to localize (see `docs/i18n_exclusions.md` §A5).

Key structures:
- EASYOCR_FULL_LANGUAGES / EASYOCR_MAIN_LANGUAGES
- PADDLEOCR_FULL_LANGUAGES / PADDLEOCR_MAIN_LANGUAGES

Key functions:
- lang_label(): resolves a display key to its localized label.
*/

/// Resolves an OCR-language display key to its localized label, falling back to
/// the key on a catalog miss. Runtime (not `const`) because `t!` is not const.
#[must_use]
pub fn lang_label(display_key: &'static str) -> &'static str {
    ms_i18n::lookup(display_key).unwrap_or(display_key)
}

pub const EASYOCR_FULL_LANGUAGES: &[(&str, &str)] = &[
    ("abq", "translation.ocr_langs.abaza"),
    ("ady", "translation.ocr_langs.adyghe"),
    ("af", "translation.ocr_langs.afrikaans"),
    ("ang", "translation.ocr_langs.angika"),
    ("ar", "translation.ocr_langs.arabic"),
    ("as", "translation.ocr_langs.assamese"),
    ("ava", "translation.ocr_langs.avar"),
    ("az", "translation.ocr_langs.azerbaijani"),
    ("be", "translation.ocr_langs.belarusian"),
    ("bg", "translation.ocr_langs.bulgarian"),
    ("bh", "translation.ocr_langs.bihari"),
    ("bho", "translation.ocr_langs.bhojpuri"),
    ("bn", "translation.ocr_langs.bengali"),
    ("bs", "translation.ocr_langs.bosnian"),
    ("ch_sim", "translation.ocr_langs.chinese_simplified"),
    ("ch_tra", "translation.ocr_langs.chinese_traditional"),
    ("che", "translation.ocr_langs.chechen"),
    ("cs", "translation.ocr_langs.czech"),
    ("cy", "translation.ocr_langs.welsh"),
    ("da", "translation.ocr_langs.danish"),
    ("dar", "translation.ocr_langs.dargwa"),
    ("de", "translation.ocr_langs.german"),
    ("en", "translation.ocr_langs.english"),
    ("es", "translation.ocr_langs.spanish"),
    ("et", "translation.ocr_langs.estonian"),
    ("fa", "translation.ocr_langs.persian"),
    ("fr", "translation.ocr_langs.french"),
    ("ga", "translation.ocr_langs.irish"),
    ("gom", "translation.ocr_langs.goan_konkani"),
    ("hi", "translation.ocr_langs.hindi"),
    ("hr", "translation.ocr_langs.croatian"),
    ("hu", "translation.ocr_langs.hungarian"),
    ("id", "translation.ocr_langs.indonesian"),
    ("inh", "translation.ocr_langs.ingush"),
    ("is", "translation.ocr_langs.icelandic"),
    ("it", "translation.ocr_langs.italian"),
    ("ja", "translation.ocr_langs.japanese"),
    ("kbd", "translation.ocr_langs.kabardian"),
    ("kn", "translation.ocr_langs.kannada"),
    ("ko", "translation.ocr_langs.korean"),
    ("ku", "translation.ocr_langs.kurdish"),
    ("la", "translation.ocr_langs.latin"),
    ("lbe", "translation.ocr_langs.lak"),
    ("lez", "translation.ocr_langs.lezgian"),
    ("lt", "translation.ocr_langs.lithuanian"),
    ("lv", "translation.ocr_langs.latvian"),
    ("mah", "translation.ocr_langs.magahi"),
    ("mai", "translation.ocr_langs.maithili"),
    ("mi", "translation.ocr_langs.maori"),
    ("mn", "translation.ocr_langs.mongolian"),
    ("mr", "translation.ocr_langs.marathi"),
    ("ms", "translation.ocr_langs.malay"),
    ("mt", "translation.ocr_langs.maltese"),
    ("ne", "translation.ocr_langs.nepali"),
    ("new", "translation.ocr_langs.newari"),
    ("nl", "translation.ocr_langs.dutch"),
    ("no", "translation.ocr_langs.norwegian"),
    ("oc", "translation.ocr_langs.occitan"),
    ("pi", "translation.ocr_langs.pali"),
    ("pl", "translation.ocr_langs.polish"),
    ("pt", "translation.ocr_langs.portuguese"),
    ("ro", "translation.ocr_langs.romanian"),
    ("ru", "translation.ocr_langs.russian"),
    ("rs_cyrillic", "translation.ocr_langs.serbian_cyrillic"),
    ("rs_latin", "translation.ocr_langs.serbian_latin"),
    ("sck", "translation.ocr_langs.nagpuri"),
    ("sk", "translation.ocr_langs.slovak"),
    ("sl", "translation.ocr_langs.slovenian"),
    ("sq", "translation.ocr_langs.albanian"),
    ("sv", "translation.ocr_langs.swedish"),
    ("sw", "translation.ocr_langs.swahili"),
    ("ta", "translation.ocr_langs.tamil"),
    ("tab", "translation.ocr_langs.tabasaran"),
    ("te", "translation.ocr_langs.telugu"),
    ("th", "translation.ocr_langs.thai"),
    ("tjk", "translation.ocr_langs.tajik"),
    ("tl", "translation.ocr_langs.tagalog"),
    ("tr", "translation.ocr_langs.turkish"),
    ("ug", "translation.ocr_langs.uyghur"),
    ("uk", "translation.ocr_langs.ukrainian"),
    ("ur", "translation.ocr_langs.urdu"),
    ("uz", "translation.ocr_langs.uzbek"),
    ("vi", "translation.ocr_langs.vietnamese"),
];

pub const EASYOCR_MAIN_LANGUAGES: &[(&str, &str)] = &[
    ("ch_sim", "translation.ocr_langs.chinese_simplified"),
    ("ch_tra", "translation.ocr_langs.chinese_traditional"),
    ("ja", "translation.ocr_langs.japanese"),
    ("ko", "translation.ocr_langs.korean"),
    ("en", "translation.ocr_langs.english"),
];

pub const PADDLEOCR_FULL_LANGUAGES: &[(&str, &str)] = &[
    ("english_v5", "translation.ocr_langs.english"),
    ("latin_v5", "translation.ocr_langs.latin_languages"),
    ("eslav_v5", "translation.ocr_langs.slavic_languages"),
    ("korean_v5", "translation.ocr_langs.korean"),
    ("chinese_v5", "translation.ocr_langs.chinese_japanese_english"),
    ("thai_v5", "translation.ocr_langs.thai"),
    ("greek_v5", "translation.ocr_langs.greek"),
    ("arabic_v3", "translation.ocr_langs.arabic"),
    ("hindi_v3", "translation.ocr_langs.devanagari"),
    ("telugu_v3", "translation.ocr_langs.telugu"),
    ("tamil_v3", "translation.ocr_langs.tamil"),
];

pub const PADDLEOCR_MAIN_LANGUAGES: &[(&str, &str)] = &[
    ("korean_v5", "translation.ocr_langs.korean"),
    ("chinese_v5", "translation.ocr_langs.chinese_japanese_english"),
    ("english_v5", "translation.ocr_langs.english"),
    ("latin_v5", "translation.ocr_langs.latin_languages"),
    ("eslav_v5", "translation.ocr_langs.slavic_languages"),
];
