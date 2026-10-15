/*
File: crates/ms-sysprobe/src/ai_models/easyocr.rs

Purpose:
The one owner of everything EasyOCR-specific the application decides itself: how a user's
language list is normalized, which recognizer script that list selects (a mirror of easyocr
1.7.2 `Reader.__init__`, `easyocr.py:111-167`), and the pinned download spec of every
script. The Python backend receives the normalized list and the model directory and loads
offline (`download_enabled=False`); it neither normalizes nor downloads.

Key items:
- `normalize_easyocr_langs()`: raw comma-separated setting -> canonical easyocr codes.
- `easyocr_script_for()`: canonical codes -> `EasyOcrScript`, or `EasyOcrLangError` exactly
  where easyocr would raise.
- `EasyOcrScript`: the 14 recognizers (`ALL`, `key`, `recognizer_file`).
- `easyocr_spec()`: the pinned `ExternalModelSpec` of a script.
- `EASYOCR_DIR`, `EASYOCR_DETECTOR_FILE`: the on-disk layout.

On-disk layout: ONE shared directory `<side_models>/EasyOCR/` (`DirLayout::Shared`) holds
the CRAFT detector `craft_mlt_25k.pth` once and every downloaded recognizer next to it,
because easyocr reads detector and recognizer from one `model_storage_directory`. Each
script spec lists both files; its own completion marker `.ms_model_complete.easyocr_<script>.json`
makes per-script status independent, while the detector is downloaded only by the first
script and then found verified by the others.

Notes:
- Every file comes from the official GitHub release zips of JaidedAI/EasyOCR, pinned by
  archive size + sha256 and member size + sha256 (member md5 cross-checked against the
  library's own `md5sum` when the pins were computed).
- `crates/ms-sysprobe/fixtures/easyocr_scripts.json` mirrors the library (generated from the
  installed easyocr 1.7.2; the choice cases verified against the real `Reader`). The tests
  here assert this table equals the fixture; a backend test checks the fixture against the
  installed library.
*/

use super::external::{DirLayout, ExternalFile, ExternalModelSpec, ExternalSource, FileOrigin};

/// Model directory of every EasyOCR script, relative to the `side_models` root.
pub const EASYOCR_DIR: &str = "EasyOCR";
/// File name of the shared CRAFT detector inside [`EASYOCR_DIR`].
pub const EASYOCR_DETECTOR_FILE: &str = "craft_mlt_25k.pth";
/// Release download root every archive path below is relative to.
const RELEASE_BASE_URL: &str = "https://github.com/JaidedAI/EasyOCR/releases/download";
/// Language code the backend has always defaulted to for an empty setting.
const DEFAULT_LANG: &str = "ko";

// easyocr 1.7.2 `config.py` language lists.
const LATIN: &[&str] = &[
    "af", "az", "bs", "cs", "cy", "da", "de", "en", "es", "et", "fr", "ga", "hr", "hu", "id", "is", "it", "ku", "la", "lt", "lv", "mi", "ms", "mt",
    "nl", "no", "oc", "pi", "pl", "pt", "ro", "rs_latin", "sk", "sl", "sq", "sv", "sw", "tl", "tr", "uz", "vi",
];
const ARABIC: &[&str] = &["ar", "fa", "ug", "ur"];
const BENGALI: &[&str] = &["bn", "as", "mni"];
const CYRILLIC: &[&str] = &["ru", "rs_cyrillic", "be", "bg", "uk", "mn", "abq", "ady", "kbd", "ava", "dar", "inh", "che", "lbe", "lez", "tab", "tjk"];
const DEVANAGARI: &[&str] = &["hi", "mr", "ne", "bh", "mai", "ang", "bho", "mah", "sck", "new", "gom", "sa", "bgc"];
const OTHER: &[&str] = &["th", "ch_sim", "ch_tra", "ja", "ko", "ta", "te", "kn"];
/// Listed above but shipped without a `character/<code>_char.txt`: `Reader` fails on them
/// with `FileNotFoundError`, so they are refused here as unsupported.
const UNUSABLE: &[&str] = &["sa", "bgc", "mni"];

/// One EasyOCR recognizer, in the order of easyocr's selection chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EasyOcrScript {
    /// `english_g2`: chosen only for exactly `["en"]`.
    English,
    /// `thai_g1`.
    Thai,
    /// `zh_tra_g1` (traditional Chinese).
    ChineseTra,
    /// `zh_sim_g2` (simplified Chinese).
    ChineseSim,
    /// `japanese_g2`.
    Japanese,
    /// `korean_g2`.
    Korean,
    /// `tamil_g1`.
    Tamil,
    /// `telugu_g2`.
    Telugu,
    /// `kannada_g2`.
    Kannada,
    /// `bengali_g1`.
    Bengali,
    /// `arabic_g1`.
    Arabic,
    /// `devanagari_g1`.
    Devanagari,
    /// `cyrillic_g2`.
    Cyrillic,
    /// `latin_g2`: every list no earlier branch claims.
    Latin,
}

impl EasyOcrScript {
    /// Every script, in easyocr's selection order.
    pub const ALL: [Self; 14] = [
        Self::English,
        Self::Thai,
        Self::ChineseTra,
        Self::ChineseSim,
        Self::Japanese,
        Self::Korean,
        Self::Tamil,
        Self::Telugu,
        Self::Kannada,
        Self::Bengali,
        Self::Arabic,
        Self::Devanagari,
        Self::Cyrillic,
        Self::Latin,
    ];

    /// Stable key (spec id suffix, logs): easyocr's `model_lang` name.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::English => "english",
            Self::Thai => "thai",
            Self::ChineseTra => "chinese_tra",
            Self::ChineseSim => "chinese_sim",
            Self::Japanese => "japanese",
            Self::Korean => "korean",
            Self::Tamil => "tamil",
            Self::Telugu => "telugu",
            Self::Kannada => "kannada",
            Self::Bengali => "bengali",
            Self::Arabic => "arabic",
            Self::Devanagari => "devanagari",
            Self::Cyrillic => "cyrillic",
            Self::Latin => "latin",
        }
    }

    /// File name of the recognizer weights inside [`EASYOCR_DIR`] (easyocr's `filename`).
    #[must_use]
    pub fn recognizer_file(self) -> &'static str {
        easyocr_spec(self).files[1].path
    }

    /// The codes this script accepts besides its own trigger (easyocr's `setModelLanguage`
    /// list); `None` for Latin, which easyocr never restricts.
    fn compatible(self) -> Option<&'static [&'static str]> {
        match self {
            Self::English => Some(&["en"]),
            Self::Thai => Some(&["th", "en"]),
            Self::ChineseTra => Some(&["ch_tra", "en"]),
            Self::ChineseSim => Some(&["ch_sim", "en"]),
            Self::Japanese => Some(&["ja", "en"]),
            Self::Korean => Some(&["ko", "en"]),
            Self::Tamil => Some(&["ta", "en"]),
            Self::Telugu => Some(&["te", "en"]),
            Self::Kannada => Some(&["kn", "en"]),
            Self::Bengali => Some(&["bn", "as", "mni", "en"]),
            Self::Arabic => Some(&["ar", "fa", "ug", "ur", "en"]),
            Self::Devanagari => Some(&["hi", "mr", "ne", "bh", "mai", "ang", "bho", "mah", "sck", "new", "gom", "sa", "bgc", "en"]),
            Self::Cyrillic => {
                Some(&["ru", "rs_cyrillic", "be", "bg", "uk", "mn", "abq", "ady", "kbd", "ava", "dar", "inh", "che", "lbe", "lez", "tab", "tjk", "en"])
            }
            Self::Latin => None,
        }
    }
}

/// Why a language list cannot be read by EasyOCR; mirrors the `ValueError`s of
/// `Reader.__init__` (plus the codes it lists but cannot load).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EasyOcrLangError {
    /// The list is empty.
    #[error("no EasyOCR language selected")]
    Empty,
    /// These codes are not EasyOCR languages (or are listed but ship no character set).
    #[error("EasyOCR does not support the language code(s): {}", codes.join(", "))]
    Unsupported { codes: Vec<String> },
    /// `script`'s recognizer is selected, but the list contains codes it cannot read
    /// together with it (`compatible` is what it accepts).
    #[error("EasyOCR's {} recognizer only reads {} together", script.key(), compatible.join(", "))]
    Incompatible { script: EasyOcrScript, compatible: &'static [&'static str] },
}

/// Normalizes the raw comma-separated EasyOCR language setting into the canonical codes the
/// backend passes to `easyocr.Reader`: pieces trimmed and lowercased, empty ones dropped,
/// the aliases the application has always accepted mapped (`kor`/`korean` -> `ko`,
/// `jpn`/`jp`/`japan`/`japanese` -> `ja`, `zh`/`ch`/`chinese`/`zh-cn`/`zh-hans` -> `ch_sim`,
/// `zh-tw`/`zh-hant`/`chinese_cht` -> `ch_tra`), duplicates removed (first one wins); an
/// empty result is `["ko"]`. Validity is NOT checked here (see [`easyocr_script_for`]).
#[must_use]
pub fn normalize_easyocr_langs(raw: &str) -> Vec<String> {
    let mut langs: Vec<String> = Vec::new();
    for piece in raw.split(',') {
        let code = canonical_code(&piece.trim().to_lowercase());
        if !code.is_empty() && !langs.contains(&code) {
            langs.push(code);
        }
    }
    if langs.is_empty() {
        langs.push(DEFAULT_LANG.to_owned());
    }
    langs
}

/// One trimmed, lowercased code with the legacy aliases mapped.
fn canonical_code(code: &str) -> String {
    match code {
        "kor" | "korean" => "ko",
        "jpn" | "jp" | "japan" | "japanese" => "ja",
        "zh" | "ch" | "chinese" | "zh-cn" | "zh-hans" => "ch_sim",
        "zh-tw" | "zh-hant" | "chinese_cht" => "ch_tra",
        other => other,
    }
    .to_owned()
}

/// The recognizer easyocr 1.7.2 selects for `langs` (canonical codes, normally the output of
/// [`normalize_easyocr_langs`]; duplicates are ignored), following `Reader.__init__`
/// (`easyocr.py:111-167`) branch for branch: exactly `["en"]` -> English; else the first of
/// Thai, Chinese-traditional, Chinese-simplified, Japanese, Korean, Tamil, Telugu, Kannada
/// whose code is present; else Bengali, Arabic, Devanagari, Cyrillic when a code of that
/// list is present; else Latin. A selected script other than Latin must accept every code.
///
/// # Errors
/// `Empty`; `Unsupported` for codes easyocr does not list or cannot load; `Incompatible`
/// where easyocr raises "… is only compatible with English".
pub fn easyocr_script_for<S: AsRef<str>>(langs: &[S]) -> Result<EasyOcrScript, EasyOcrLangError> {
    let mut list: Vec<&str> = Vec::new();
    for lang in langs {
        let code = lang.as_ref();
        if !list.contains(&code) {
            list.push(code);
        }
    }
    if list.is_empty() {
        return Err(EasyOcrLangError::Empty);
    }
    let known = |code: &&str| [LATIN, ARABIC, BENGALI, CYRILLIC, DEVANAGARI, OTHER].iter().any(|group| group.contains(code));
    let unsupported: Vec<String> = list.iter().filter(|code| !known(code) || UNUSABLE.contains(code)).map(|code| (*code).to_owned()).collect();
    if !unsupported.is_empty() {
        return Err(EasyOcrLangError::Unsupported { codes: unsupported });
    }
    let has = |code: &str| list.contains(&code);
    let any_of = |group: &[&str]| list.iter().any(|code| group.contains(code));
    let script = if list == ["en"] {
        EasyOcrScript::English
    } else if has("th") {
        EasyOcrScript::Thai
    } else if has("ch_tra") {
        EasyOcrScript::ChineseTra
    } else if has("ch_sim") {
        EasyOcrScript::ChineseSim
    } else if has("ja") {
        EasyOcrScript::Japanese
    } else if has("ko") {
        EasyOcrScript::Korean
    } else if has("ta") {
        EasyOcrScript::Tamil
    } else if has("te") {
        EasyOcrScript::Telugu
    } else if has("kn") {
        EasyOcrScript::Kannada
    } else if any_of(BENGALI) {
        EasyOcrScript::Bengali
    } else if any_of(ARABIC) {
        EasyOcrScript::Arabic
    } else if any_of(DEVANAGARI) {
        EasyOcrScript::Devanagari
    } else if any_of(CYRILLIC) {
        EasyOcrScript::Cyrillic
    } else {
        EasyOcrScript::Latin
    };
    match script.compatible() {
        Some(compatible) if list.iter().any(|code| !compatible.contains(code)) => Err(EasyOcrLangError::Incompatible { script, compatible }),
        Some(_) | None => Ok(script),
    }
}

/// The pinned download spec of `script`: the shared detector plus its recognizer, in
/// [`EASYOCR_DIR`].
#[must_use]
pub fn easyocr_spec(script: EasyOcrScript) -> &'static ExternalModelSpec {
    match script {
        EasyOcrScript::English => &ENGLISH,
        EasyOcrScript::Thai => &THAI,
        EasyOcrScript::ChineseTra => &CHINESE_TRA,
        EasyOcrScript::ChineseSim => &CHINESE_SIM,
        EasyOcrScript::Japanese => &JAPANESE,
        EasyOcrScript::Korean => &KOREAN,
        EasyOcrScript::Tamil => &TAMIL,
        EasyOcrScript::Telugu => &TELUGU,
        EasyOcrScript::Kannada => &KANNADA,
        EasyOcrScript::Bengali => &BENGALI_SPEC,
        EasyOcrScript::Arabic => &ARABIC_SPEC,
        EasyOcrScript::Devanagari => &DEVANAGARI_SPEC,
        EasyOcrScript::Cyrillic => &CYRILLIC_SPEC,
        EasyOcrScript::Latin => &LATIN_SPEC,
    }
}

/// A pinned zip-member file of the EasyOCR releases.
const fn zip_member(path: &'static str, size: u64, sha256: &'static str, archive: &'static str, archive_size: u64, archive_sha256: &'static str) -> ExternalFile {
    // The member name inside every EasyOCR release zip is the weight file name itself.
    ExternalFile { path, size, sha256, origin: FileOrigin::ZipMember { archive, archive_size, archive_sha256, member: path } }
}

/// The spec of one script in the shared EasyOCR directory.
const fn spec(id: &'static str, files: &'static [ExternalFile]) -> ExternalModelSpec {
    ExternalModelSpec { id, source: ExternalSource::Url { base_url: RELEASE_BASE_URL }, dir: EASYOCR_DIR, layout: DirLayout::Shared, files }
}

// Pins computed by `tools/model_pins/compute_pins.py` from the release assets.
/// The shared CRAFT text detector (`craft_mlt_25k.pth`), stored once in [`EASYOCR_DIR`].
const CRAFT: ExternalFile = zip_member("craft_mlt_25k.pth", 83_152_330, "4a5efbfb48b4081100544e75e1e2b57f8de3d84f213004b14b85fd4b3748db17", "pre-v1.1.6/craft_mlt_25k.zip", 77_251_756, "8dc6a1c703a89ed56308ef742d26ebd45c656248cbbbda6e7fe60e569f873e65");
static ENGLISH_FILES: [ExternalFile; 2] = [CRAFT, zip_member("english_g2.pth", 15_143_997, "e2272681d9d67a04e2dff396b6e95077bc19001f8f6d3593c307b9852e1c29e8", "v1.3/english_g2.zip", 14_040_947, "1b5eaebf1c062de6205560c97ffcfa8dc0e6f413c340e8adc5cfc57e159f61ff")];
static ENGLISH: ExternalModelSpec = spec("easyocr_english", &ENGLISH_FILES);
static THAI_FILES: [ExternalFile; 2] = [CRAFT, zip_member("thai.pth", 215_384_298, "3ebbb9321b9c7c694e169cd23cc37f3c0bd2a160f5b539602bbef80e663e0a17", "pre-v1.1.6/thai.zip", 199_881_630, "4f26ffd98dd28cca89ff6fe0bc761ab865c2b38fabd76a3ce2143d1d88c0d7f1")];
static THAI: ExternalModelSpec = spec("easyocr_thai", &THAI_FILES);
static CHINESE_TRA_FILES: [ExternalFile; 2] = [CRAFT, zip_member("chinese.pth", 226_062_909, "19ecc60f4e079eb2cd7f25f3513fc996cf80819fc1f1d0aee24c58402cc4776d", "pre-v1.1.6/chinese.zip", 209_821_039, "71cb95f601ce0c062860224ff3e1d2e807d784f2be92bf53ba500bb1736dabd5")];
static CHINESE_TRA: ExternalModelSpec = spec("easyocr_chinese_tra", &CHINESE_TRA_FILES);
static CHINESE_SIM_FILES: [ExternalFile; 2] = [CRAFT, zip_member("zh_sim_g2.pth", 21_951_421, "cb678fdef09d651e7763ca551ad790dc89f0b2e3d2a640484330e338fb574c7a", "v1.3/zh_sim_g2.zip", 20_288_076, "eed38bdd9e612bd150f13d2f70fd8e5b24c79c2e2fe621a02603b38d385503e9")];
static CHINESE_SIM: ExternalModelSpec = spec("easyocr_chinese_sim", &CHINESE_SIM_FILES);
static JAPANESE_FILES: [ExternalFile; 2] = [CRAFT, zip_member("japanese_g2.pth", 17_321_277, "afbb32b0485297de5a7c1f59aeecac12c69f32b15cedd3c3d8c2e8529de85af3", "v1.3/japanese_g2.zip", 16_065_475, "7b45d23da9b001e296590ed202a45bc1fcaf06ab661f457c463ed502de766fb5")];
static JAPANESE: ExternalModelSpec = spec("easyocr_japanese", &JAPANESE_FILES);
static KOREAN_FILES: [ExternalFile; 2] = [CRAFT, zip_member("korean_g2.pth", 16_081_533, "ecdead53c909748aad74bc2f2beeab3e3ad751134db6335e97ea19f879ca407e", "v1.3/korean_g2.zip", 14_910_151, "3cdaad22676723040ceb5c6fe72089d656b3bc1f630889c373ade86f3ab38c2e")];
static KOREAN: ExternalModelSpec = spec("easyocr_korean", &KOREAN_FILES);
static TAMIL_FILES: [ExternalFile; 2] = [CRAFT, zip_member("tamil.pth", 215_314_530, "1e5e3336a4d4923da6246a76338c02cf9b366cfb3d1a84182bfc3d11df7d5a4d", "v1.1.7/tamil.zip", 199_783_937, "a6f372175127ab106c620b1d6af184e7075bd629d1e2aa22a88ac05bae68050d")];
static TAMIL: ExternalModelSpec = spec("easyocr_tamil", &TAMIL_FILES);
static TELUGU_FILES: [ExternalFile; 2] = [CRAFT, zip_member("telugu.pth", 15_209_558, "a73ba1c65f8b3e7b18df3104c1f137970510ce51e3e1d9341506292730fa3c43", "v1.2/telugu.zip", 14_108_882, "cbcb5f570dc3cc36e9187f6d407610dadbc0dc88e214757b9182d45043a11d1f")];
static TELUGU: ExternalModelSpec = spec("easyocr_telugu", &TELUGU_FILES);
static KANNADA_FILES: [ExternalFile; 2] = [CRAFT, zip_member("kannada.pth", 15_211_614, "3cc308972bccb2c483741e90e87f2941c1b40d84cf548115f48372d288deb0f0", "v1.2/kannada.zip", 14_105_056, "408ef9d7b68b99366d1dafc9b1275db80ed529252c2e1fa1038d4114cb9ef1d2")];
static KANNADA: ExternalModelSpec = spec("easyocr_kannada", &KANNADA_FILES);
static BENGALI_FILES: [ExternalFile; 2] = [CRAFT, zip_member("bengali.pth", 215_369_934, "28d10d7dab96bf38c1d190d09a65a732130f5f34b28dcbd7e51aba3366146896", "v1.1.8/bengali.zip", 199_852_959, "b9f88f7beef9aa5480c62e9f84c50be82214e5b209d0115465c30175eca4f944")];
static BENGALI_SPEC: ExternalModelSpec = spec("easyocr_bengali", &BENGALI_FILES);
static ARABIC_FILES: [ExternalFile; 2] = [CRAFT, zip_member("arabic.pth", 215_400_714, "2a9afd42c374deb98aed0b53c9b77d75e1d00d4e0501f3b0276c54190c89b1a8", "pre-v1.1.6/arabic.zip", 199_916_962, "90b08b8e456d435bcdd96141221db5f116ef03ab3efc2d3239cfb24520ad7756")];
static ARABIC_SPEC: ExternalModelSpec = spec("easyocr_arabic", &ARABIC_FILES);
static DEVANAGARI_FILES: [ExternalFile; 2] = [CRAFT, zip_member("devanagari.pth", 215_406_870, "1a8d0abb2032c0ec9d06639bc078b7686c10cf3b8193fe42a6382bc6846ae4a3", "pre-v1.1.6/devanagari.zip", 199_886_824, "462a75e10de1ed1f34fbdc4761ccee341d59664fc46ecb3463f60be4862e01f9")];
static DEVANAGARI_SPEC: ExternalModelSpec = spec("easyocr_devanagari", &DEVANAGARI_FILES);
static CYRILLIC_FILES: [ExternalFile; 2] = [CRAFT, zip_member("cyrillic_g2.pth", 15_256_523, "48d0f3b58f28aa64651ab1032cc2d498c4de25135829668e87c14e7a07529f29", "v1.6.1/cyrillic_g2.zip", 14_140_353, "34060353accfadd4e429f28d392f88cf70c2fc9d2633710a3b00c21412dc627b")];
static CYRILLIC_SPEC: ExternalModelSpec = spec("easyocr_cyrillic", &CYRILLIC_FILES);
static LATIN_FILES: [ExternalFile; 2] = [CRAFT, zip_member("latin_g2.pth", 15_406_141, "aaa95be1c4a9cb3496879bed7c520886ce1164f89e026f0c54488394e74e8c55", "v1.3/latin_g2.zip", 14_284_936, "29f1920c493378da65a59793fb70e7e190504662b6bed57ab26f4067eb5f3769")];
static LATIN_SPEC: ExternalModelSpec = spec("easyocr_latin", &LATIN_FILES);


#[cfg(test)]
mod tests;
