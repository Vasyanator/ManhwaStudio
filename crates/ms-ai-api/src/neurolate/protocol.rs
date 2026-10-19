/*
File: crates/ms-ai-api/src/neurolate/protocol.rs

Purpose:
The pure, network-free half of the NeuroLate client: the typed request and response model of
`POST /ext/v1/ocr` and `GET /ext/v1/key`, the multipart form the OCR request is sent as, the
response parsers and the HTTP-status -> `NeurolateError` mapping.

Key structures:
- OcrMode, NeurolateLanguage, NeurolateComicType, OcrParams, OcrRequest
- OcrResponse, OcrImageResult, OcrBlock, OcrPolygon, OcrBox, TextStatus, JobError
- KeyInfo, LimitPeriod, KeyStatus

Key functions:
- ocr_form()           : the request's multipart body (every field a form field, once).
- parse_ocr_response() : status + body + sent image count -> `OcrResponse` (batch identity
                         validated, ordered by index) or the typed error.
- parse_key_info()     : status + body -> `KeyInfo` or the typed error.
- status_error()       : a non-success status + body -> `NeurolateError`.

Notes:
Target-neutral (compiled on wasm too). Wire facts verified against the live API
(`ExtOcrErrorDTO`, `OcrResponseDTO`, `ExtKeyDTO`): parameters are read from the form as well as
the query, but the query has an ~8 KB request-line limit and a field sent twice is comma-joined
and rejected, so every field goes into the FORM exactly once and the query is never used. A
missing required field answers 401 with an empty body (not 400), which is why `ocr_form` refuses
a request without images instead of letting the server call the key bad. Unknown enum values
from the server parse as `Unknown` / `None`, never as an error; `mask`, `bbox`, `text` may be
null. Renamed fields are read under both names, current first: the request id `requestId`
(formerly `id`) and a block's `maskIndex` (formerly `region`). The optional `Idempotency-Key`
header (409 `DUPLICATE_REQUEST`), `kind`, `masks` and `readingDirection` are not used. Parsing
uses `serde_json::Value` like the rest of the crate.
*/

use std::fmt;

use super::error::NeurolateError;
use crate::http::{MultipartBody, MultipartForm};

/// The API origin. A persistence-free constant; the key is sent to this origin only.
pub const BASE_URL: &str = "https://api.neurolate.com";
/// Path of the OCR / text-detection endpoint (paid).
pub(crate) const OCR_PATH: &str = "/ext/v1/ocr";
/// The `code` of a 409: a request with this `Idempotency-Key` was already made.
const DUPLICATE_REQUEST_CODE: &str = "DUPLICATE_REQUEST";
/// Path of the current-key endpoint (free).
pub(crate) const KEY_PATH: &str = "/ext/v1/key";

/// What the OCR request asks for, per image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrMode {
    /// Masks and text (the server default).
    MaskAndText,
    /// Masks only; every mask is charged.
    Mask,
    /// Text only. On a PAGE without masks the server runs (and charges) `MaskAndText` instead
    /// (see `OcrImageResult::mode_applied`).
    Text,
}

impl OcrMode {
    /// The wire value.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::MaskAndText => "MASK_AND_TEXT",
            Self::Mask => "MASK",
            Self::Text => "TEXT",
        }
    }

    /// Parses a wire value, case-insensitively; `None` for anything else.
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        [Self::MaskAndText, Self::Mask, Self::Text].into_iter().find(|mode| mode.wire().eq_ignore_ascii_case(value))
    }
}

/// The source language of the text, sent as its 2-letter ISO code: every code of the API's
/// `Language` enum except `UND` (which it rejects as a source language). It has no
/// Traditional-Chinese variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NeurolateLanguage {
    Ko,
    Ja,
    Zh,
    En,
    Ar,
    Az,
    Be,
    Bg,
    Bn,
    Bs,
    Cs,
    Da,
    De,
    El,
    Es,
    Et,
    Fa,
    Fi,
    Fr,
    He,
    Hi,
    Hr,
    Hu,
    Hy,
    Id,
    It,
    Ka,
    Kk,
    Ky,
    Lt,
    Lv,
    Mk,
    Mn,
    Mr,
    Ms,
    Ne,
    Nl,
    No,
    Pl,
    Pt,
    Ro,
    Ru,
    Sk,
    Sl,
    Sq,
    Sr,
    Sv,
    Sw,
    Ta,
    Te,
    Tg,
    Th,
    Tl,
    Tr,
    Uk,
    Ur,
    Uz,
    Vi,
}

impl NeurolateLanguage {
    /// Every language, in the order a picker lists them: the four main comic languages first,
    /// then the rest by code.
    pub const ALL: [Self; 58] = [Self::Ko, Self::Ja, Self::Zh, Self::En, Self::Ar, Self::Az, Self::Be, Self::Bg, Self::Bn, Self::Bs, Self::Cs, Self::Da, Self::De, Self::El, Self::Es, Self::Et, Self::Fa, Self::Fi, Self::Fr, Self::He, Self::Hi, Self::Hr, Self::Hu, Self::Hy, Self::Id, Self::It, Self::Ka, Self::Kk, Self::Ky, Self::Lt, Self::Lv, Self::Mk, Self::Mn, Self::Mr, Self::Ms, Self::Ne, Self::Nl, Self::No, Self::Pl, Self::Pt, Self::Ro, Self::Ru, Self::Sk, Self::Sl, Self::Sq, Self::Sr, Self::Sv, Self::Sw, Self::Ta, Self::Te, Self::Tg, Self::Th, Self::Tl, Self::Tr, Self::Uk, Self::Ur, Self::Uz, Self::Vi];

    /// The wire code (`"KO"`, ...), also a valid persistence key.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Ko => "KO",
            Self::Ja => "JA",
            Self::Zh => "ZH",
            Self::En => "EN",
            Self::Ar => "AR",
            Self::Az => "AZ",
            Self::Be => "BE",
            Self::Bg => "BG",
            Self::Bn => "BN",
            Self::Bs => "BS",
            Self::Cs => "CS",
            Self::Da => "DA",
            Self::De => "DE",
            Self::El => "EL",
            Self::Es => "ES",
            Self::Et => "ET",
            Self::Fa => "FA",
            Self::Fi => "FI",
            Self::Fr => "FR",
            Self::He => "HE",
            Self::Hi => "HI",
            Self::Hr => "HR",
            Self::Hu => "HU",
            Self::Hy => "HY",
            Self::Id => "ID",
            Self::It => "IT",
            Self::Ka => "KA",
            Self::Kk => "KK",
            Self::Ky => "KY",
            Self::Lt => "LT",
            Self::Lv => "LV",
            Self::Mk => "MK",
            Self::Mn => "MN",
            Self::Mr => "MR",
            Self::Ms => "MS",
            Self::Ne => "NE",
            Self::Nl => "NL",
            Self::No => "NO",
            Self::Pl => "PL",
            Self::Pt => "PT",
            Self::Ro => "RO",
            Self::Ru => "RU",
            Self::Sk => "SK",
            Self::Sl => "SL",
            Self::Sq => "SQ",
            Self::Sr => "SR",
            Self::Sv => "SV",
            Self::Sw => "SW",
            Self::Ta => "TA",
            Self::Te => "TE",
            Self::Tg => "TG",
            Self::Th => "TH",
            Self::Tl => "TL",
            Self::Tr => "TR",
            Self::Uk => "UK",
            Self::Ur => "UR",
            Self::Uz => "UZ",
            Self::Vi => "VI",
        }
    }

    /// Parses a code, case-insensitively (`"ko"` and `"KO"` are Korean); `None` for anything
    /// else.
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|language| language.code().eq_ignore_ascii_case(code.trim()))
    }
}

/// The comic type; the server derives the reading order of blocks from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NeurolateComicType {
    Webtoon,
    Manga,
}

impl NeurolateComicType {
    /// The wire value.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::Webtoon => "WEBTOON",
            Self::Manga => "MANGA",
        }
    }
}

/// The per-request recognition parameters a consumer resolves from its settings.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OcrParams {
    pub language: NeurolateLanguage,
    pub comic_type: NeurolateComicType,
}

/// How the server treats every image of a request (`kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrKind {
    /// One cropped bubble: the whole image is one block (up to 1,000,000 px).
    Block,
    /// A whole page: the server finds the blocks.
    Page,
}

impl OcrKind {
    /// The wire value.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::Block => "BLOCK",
            Self::Page => "PAGE",
        }
    }
}

/// One OCR request: the mode, the parameters, the optional `kind` and the images, each a PNG
/// (or JPEG) file. With `kind: None` the server decides per image: both sides up to 690 px is
/// a BLOCK (one crop, one block), otherwise a PAGE; a whole page that may be small sends
/// `Some(OcrKind::Page)`. `Debug` shows only the image sizes, never their bytes.
#[derive(Clone)]
pub struct OcrRequest {
    pub mode: OcrMode,
    pub kind: Option<OcrKind>,
    pub params: OcrParams,
    pub images_png: Vec<Vec<u8>>,
}

impl fmt::Debug for OcrRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sizes: Vec<usize> = self.images_png.iter().map(Vec::len).collect();
        f.debug_struct("OcrRequest").field("mode", &self.mode).field("kind", &self.kind).field("params", &self.params).field("image_bytes", &sizes).finish()
    }
}

/// The answer to one OCR request.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrResponse {
    /// `NeuroLate`'s request id (equal to the `X-Request-Id` header); for logs and support.
    pub request_id: String,
    /// One result per sent image, ordered by `index` (validated: each sent image exactly once).
    pub images: Vec<OcrImageResult>,
    /// What the request cost, in `NeuroLate` tokens (`usage.cost`); `None` when the answer
    /// carries no finite, non-negative cost (unknown, never a manufactured zero).
    pub cost: Option<f64>,
}

/// The result for one sent image.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrImageResult {
    /// Zero-based index of the image in the request.
    pub index: usize,
    /// The image size as the server decoded it, in pixels.
    pub width: u32,
    pub height: u32,
    /// The mode actually applied; `None` when the image was not processed or the server named
    /// a mode this client does not know.
    pub mode_applied: Option<OcrMode>,
    /// The blocks in the server's reading order.
    pub blocks: Vec<OcrBlock>,
    /// The failure of this image alone; its blocks are then empty.
    pub error: Option<JobError>,
}

/// One text block.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrBlock {
    /// 1-based number in reading order.
    pub number: u32,
    /// Zero-based index of the client mask the block answers, if any (wire `maskIndex`; the
    /// older API named it `region`).
    pub region: Option<u32>,
    /// The recognized text; `None` in `Mask` mode or for an empty region.
    pub text: Option<String>,
    /// The block mask in image-absolute pixels (glyph-tight, not the bubble shape).
    pub polygons: Vec<OcrPolygon>,
    /// The text envelope in image pixels.
    pub bbox: Option<OcrBox>,
    /// Client regions only: whether text was found.
    pub text_status: Option<TextStatus>,
    /// The failure of this block alone.
    pub error: Option<JobError>,
}

impl OcrBlock {
    /// Whether the block's mask marks real text: no error, at least one polygon, and no
    /// region status other than `Ok`. An `Empty` region returns its own rectangle as the mask
    /// (verified on the live API), a `Failed` one nothing useful, and an unknown status is not
    /// trusted, so all of them are `false`.
    #[must_use]
    pub fn has_usable_mask(&self) -> bool {
        let status_ok = match &self.text_status {
            None | Some(TextStatus::Ok) => true,
            Some(TextStatus::Empty | TextStatus::Failed | TextStatus::Unknown(_)) => false,
        };
        self.error.is_none() && !self.polygons.is_empty() && status_ok
    }
}

/// One mask polygon: an outer ring and the rings cut out of it, as `[x, y]` image pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrPolygon {
    pub outer: Vec<[i32; 2]>,
    pub holes: Vec<Vec<[i32; 2]>>,
}

/// An axis-aligned box in image pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OcrBox {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// The region status of a block (`textStatus`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextStatus {
    /// Text found.
    Ok,
    /// No text in the region (still charged).
    Empty,
    /// The service failed on this region (not charged).
    Failed,
    /// A value this client does not know, kept verbatim.
    Unknown(String),
}

/// A per-image or per-block failure reported inside a successful answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobError {
    pub code: String,
    pub message: String,
}

/// The window the key's spending limit applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitPeriod {
    /// The key's whole lifetime.
    Total,
    Day,
    Week,
    /// A value this client does not know, kept verbatim.
    Unknown(String),
}

/// The state of the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyStatus {
    Active,
    Disabled,
    Expired,
    /// The limit is used up: paid operations are refused, free ones still work.
    Exhausted,
    /// A value this client does not know, kept verbatim.
    Unknown(String),
}

/// The answer of `GET /ext/v1/key`: the key's name, spending limit and use. Holds no secret
/// (`key_prefix` is the public prefix the service shows).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyInfo {
    pub name: String,
    pub key_prefix: String,
    /// The spending limit per `limit_period`, in tokens.
    pub limit_value: f64,
    pub limit_period: LimitPeriod,
    /// Spent in the current window, in tokens.
    pub used_value: f64,
    /// Still available in the current window, in tokens.
    pub remaining_value: f64,
    /// When the limit renews (an ISO date); `None` for `Total`.
    pub resets_on: Option<String>,
    /// When the key expires (an ISO date-time), if ever.
    pub expires_at: Option<String>,
    pub status: KeyStatus,
}

/// The multipart body of `request`: text fields `mode`, `language`, `type` and, when set,
/// `kind` (each exactly once, in that order), then one `images` file part per image
/// (`image.png`, `image/png`; the server sniffs PNG / JPEG itself). `masks` and
/// `readingDirection` are never sent.
///
/// # Errors
/// `RequestBuild` when the request has no image (the server would answer 401, which reads as a
/// bad key) or the form cannot be encoded.
pub(crate) fn ocr_form(request: &OcrRequest) -> Result<MultipartBody, NeurolateError> {
    if request.images_png.is_empty() {
        return Err(NeurolateError::RequestBuild { detail: "an OCR request needs at least one image".to_string() });
    }
    let form = MultipartForm::new().text("mode", request.mode.wire()).text("language", request.params.language.code()).text("type", request.params.comic_type.wire());
    let form = match request.kind {
        Some(kind) => form.text("kind", kind.wire()),
        None => form,
    };
    let form = request.images_png.iter().fold(form, |form, image| form.file("images", "image.png", "image/png", image.clone()));
    Ok(form.finish()?)
}

/// Whether `status` is a success.
pub(crate) fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Parses an OCR answer for a request of `expected_images` images: a success status with an
/// `OcrResponseDTO` body, else the status's typed error (`status_error`). The batch identity is
/// validated: exactly `expected_images` results, each `index` in `0..expected_images` exactly
/// once; they are returned ordered by `index`, so `images[i]` answers sent image `i`.
///
/// # Errors
/// `status_error` for a non-success status; `Decode` for a success body of the wrong shape or
/// a batch that does not answer each sent image exactly once.
pub fn parse_ocr_response(status: u16, body: &[u8], expected_images: usize) -> Result<OcrResponse, NeurolateError> {
    if !is_success(status) {
        return Err(status_error(status, body));
    }
    let value = json_body(body)?;
    // The live API renamed `id` to `requestId` (2026-10-19); both are read.
    let request_id = opt_str(&value, "requestId").or_else(|| opt_str(&value, "id")).ok_or_else(|| decode("missing string field `requestId` (or `id`)"))?;
    let mut images = array_field(&value, "images")?.iter().map(parse_image).collect::<Result<Vec<_>, _>>()?;
    if images.len() != expected_images {
        return Err(decode(format!("{} image results for {expected_images} sent images", images.len())));
    }
    images.sort_by_key(|image| image.index);
    // Sorted, of the right count: each index is in range and unique iff image i has index i.
    if let Some((position, image)) = images.iter().enumerate().find(|(position, image)| image.index != *position) {
        return Err(decode(format!("image result index {} where {position} was expected (duplicate, gap or out of range)", image.index)));
    }
    let cost = value.get("usage").and_then(|usage| usage.get("cost")).and_then(serde_json::Value::as_f64).filter(|cost| cost.is_finite() && *cost >= 0.0);
    Ok(OcrResponse { request_id, images, cost })
}

/// A cost for a log line: the number, or `unknown`.
pub(crate) fn cost_text(cost: Option<f64>) -> String {
    cost.map_or_else(|| "unknown".to_string(), |cost| cost.to_string())
}

/// Parses a key answer: a success status with an `ExtKeyDTO` body, else the status's typed
/// error.
///
/// # Errors
/// `status_error` for a non-success status; `Decode` for a success body of the wrong shape.
pub fn parse_key_info(status: u16, body: &[u8]) -> Result<KeyInfo, NeurolateError> {
    if !is_success(status) {
        return Err(status_error(status, body));
    }
    let value = json_body(body)?;
    let limit_period = match str_field(&value, "limitPeriod")?.to_ascii_uppercase().as_str() {
        "TOTAL" => LimitPeriod::Total,
        "DAY" => LimitPeriod::Day,
        "WEEK" => LimitPeriod::Week,
        other => LimitPeriod::Unknown(other.to_string()),
    };
    let status = match str_field(&value, "status")?.to_ascii_uppercase().as_str() {
        "ACTIVE" => KeyStatus::Active,
        "DISABLED" => KeyStatus::Disabled,
        "EXPIRED" => KeyStatus::Expired,
        "EXHAUSTED" => KeyStatus::Exhausted,
        other => KeyStatus::Unknown(other.to_string()),
    };
    Ok(KeyInfo {
        name: opt_str(&value, "name").unwrap_or_default(),
        key_prefix: opt_str(&value, "keyPrefix").unwrap_or_default(),
        limit_value: quota_field(&value, "limitValue")?,
        limit_period,
        used_value: quota_field(&value, "usedValue")?,
        remaining_value: quota_field(&value, "remainingValue")?,
        resets_on: opt_str(&value, "resetsOn"),
        expires_at: opt_str(&value, "expiresAt"),
        status,
    })
}

/// The typed error of a non-success answer, from its status and its `ExtOcrErrorDTO` body
/// (`{code, message, index}`; any field may be missing, a non-JSON body reads as empty).
#[must_use]
pub fn status_error(status: u16, body: &[u8]) -> NeurolateError {
    let value = serde_json::from_slice::<serde_json::Value>(body).ok();
    let text = |name: &str| value.as_ref().and_then(|value| opt_str(value, name)).unwrap_or_default();
    match status {
        400 => NeurolateError::InvalidRequest { code: text("code"), message: text("message"), index: value.as_ref().and_then(|value| value.get("index")).and_then(serde_json::Value::as_i64).and_then(|index| i32::try_from(index).ok()) },
        401 | 403 => NeurolateError::KeyRejected,
        409 if text("code") == DUPLICATE_REQUEST_CODE => NeurolateError::DuplicateRequest { first_request_id: value.as_ref().and_then(|value| opt_str(value, "requestId")) },
        402 => NeurolateError::OutOfCredits { code: text("code") },
        413 => NeurolateError::TooLarge,
        502 => NeurolateError::ServiceFailed { message: text("message") },
        status => NeurolateError::Http { status, message: text("message") },
    }
}

fn decode(detail: impl Into<String>) -> NeurolateError {
    NeurolateError::Decode { detail: detail.into() }
}

fn json_body(body: &[u8]) -> Result<serde_json::Value, NeurolateError> {
    serde_json::from_slice(body).map_err(|error| decode(format!("the answer is not JSON: {error}")))
}

fn str_field<'v>(value: &'v serde_json::Value, name: &str) -> Result<&'v str, NeurolateError> {
    value.get(name).and_then(serde_json::Value::as_str).ok_or_else(|| decode(format!("missing string field `{name}`")))
}

fn array_field<'v>(value: &'v serde_json::Value, name: &str) -> Result<&'v [serde_json::Value], NeurolateError> {
    value.get(name).and_then(serde_json::Value::as_array).map(Vec::as_slice).ok_or_else(|| decode(format!("missing array field `{name}`")))
}

/// A quota amount: a finite, non-negative number (the fields are not cross-checked).
fn quota_field(value: &serde_json::Value, name: &str) -> Result<f64, NeurolateError> {
    value.get(name).and_then(serde_json::Value::as_f64).filter(|number| number.is_finite() && *number >= 0.0).ok_or_else(|| decode(format!("missing or negative number field `{name}`")))
}

/// A string field, `None` when absent or null (or not a string).
fn opt_str(value: &serde_json::Value, name: &str) -> Option<String> {
    value.get(name).and_then(serde_json::Value::as_str).map(str::to_string)
}

/// A non-negative integer field that must fit `T`.
fn uint_field<T: TryFrom<u64>>(value: &serde_json::Value, name: &str) -> Result<T, NeurolateError> {
    value.get(name).and_then(serde_json::Value::as_u64).and_then(|number| T::try_from(number).ok()).ok_or_else(|| decode(format!("missing or out-of-range integer field `{name}`")))
}

/// An optional non-negative integer field; null or absent is `None`, anything else must fit.
fn opt_uint_field<T: TryFrom<u64>>(value: &serde_json::Value, name: &str) -> Result<Option<T>, NeurolateError> {
    match value.get(name) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(_) => uint_field(value, name).map(Some),
    }
}

/// A `{code, message}` error object; null or absent is `None`.
fn job_error(value: &serde_json::Value) -> Option<JobError> {
    let error = value.get("error").filter(|error| !error.is_null())?;
    Some(JobError { code: opt_str(error, "code").unwrap_or_default(), message: opt_str(error, "message").unwrap_or_default() })
}

fn parse_image(value: &serde_json::Value) -> Result<OcrImageResult, NeurolateError> {
    let blocks = match value.get("blocks") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(_) => array_field(value, "blocks")?.iter().map(parse_block).collect::<Result<Vec<_>, _>>()?,
    };
    Ok(OcrImageResult {
        index: uint_field(value, "index")?,
        width: uint_field(value, "width")?,
        height: uint_field(value, "height")?,
        mode_applied: opt_str(value, "modeApplied").as_deref().and_then(OcrMode::from_wire),
        blocks,
        error: job_error(value),
    })
}

fn parse_block(value: &serde_json::Value) -> Result<OcrBlock, NeurolateError> {
    let polygons = match value.get("mask") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(_) => array_field(value, "mask")?.iter().map(parse_polygon).collect::<Result<Vec<_>, _>>()?,
    };
    let bbox = match value.get("bbox") {
        None | Some(serde_json::Value::Null) => None,
        Some(bbox) => Some(OcrBox { x: i32_field(bbox, "x")?, y: i32_field(bbox, "y")?, width: i32_field(bbox, "width")?, height: i32_field(bbox, "height")? }),
    };
    let text_status = opt_str(value, "textStatus").map(|status| match status.to_ascii_uppercase().as_str() {
        "OK" => TextStatus::Ok,
        "EMPTY" => TextStatus::Empty,
        "FAILED" => TextStatus::Failed,
        _ => TextStatus::Unknown(status),
    });
    Ok(OcrBlock { number: uint_field(value, "number")?, region: match opt_uint_field(value, "maskIndex")? {
        Some(index) => Some(index),
        None => opt_uint_field(value, "region")?,
    }, text: opt_str(value, "text"), polygons, bbox, text_status, error: job_error(value) })
}

fn i32_field(value: &serde_json::Value, name: &str) -> Result<i32, NeurolateError> {
    value.get(name).and_then(serde_json::Value::as_i64).and_then(|number| i32::try_from(number).ok()).ok_or_else(|| decode(format!("missing or out-of-range integer field `{name}`")))
}

fn parse_polygon(value: &serde_json::Value) -> Result<OcrPolygon, NeurolateError> {
    let outer = parse_ring(value.get("outer").ok_or_else(|| decode("a mask polygon has no `outer` ring"))?)?;
    let holes = match value.get("exclude") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(_) => array_field(value, "exclude")?.iter().map(parse_ring).collect::<Result<Vec<_>, _>>()?,
    };
    Ok(OcrPolygon { outer, holes })
}

/// A ring of `[x, y]` integer points.
fn parse_ring(value: &serde_json::Value) -> Result<Vec<[i32; 2]>, NeurolateError> {
    let points = value.as_array().ok_or_else(|| decode("a mask ring is not an array"))?;
    points
        .iter()
        .map(|point| match point.as_array().map(Vec::as_slice) {
            Some([x, y]) => {
                let coordinate = |value: &serde_json::Value| value.as_i64().and_then(|number| i32::try_from(number).ok());
                coordinate(x).zip(coordinate(y)).map(|(x, y)| [x, y]).ok_or_else(|| decode("a mask point is not two integers"))
            }
            _ => Err(decode("a mask point is not an [x, y] pair")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{KeyStatus, LimitPeriod, NeurolateComicType, NeurolateLanguage, OcrKind, OcrMode, OcrParams, OcrRequest, TextStatus, ocr_form, parse_key_info, parse_ocr_response, status_error};
    use crate::neurolate::error::NeurolateError;

    const OCR_PAGE: &str = include_str!("../../fixtures/neurolate/ocr_page.json");
    const OCR_CROPS: &str = include_str!("../../fixtures/neurolate/ocr_crops.json");
    const OCR_REGIONS_EMPTY: &str = include_str!("../../fixtures/neurolate/ocr_regions_empty.json");
    const OCR_BLANK_TEXT: &str = include_str!("../../fixtures/neurolate/ocr_blank_text.json");
    const KEY: &str = include_str!("../../fixtures/neurolate/key.json");
    const ERROR_INVALID_LANGUAGE: &str = include_str!("../../fixtures/neurolate/error_invalid_language.json");
    /// A live answer in the current format (`requestId`, `maskIndex`), trimmed to three blocks.
    const OCR_PAGE_REQUEST_ID: &str = include_str!("../../fixtures/neurolate/ocr_page_request_id.json");

    fn request(mode: OcrMode, images: usize) -> OcrRequest {
        OcrRequest { mode, kind: None, params: OcrParams { language: NeurolateLanguage::Ko, comic_type: NeurolateComicType::Webtoon }, images_png: (0..images).map(|index| vec![0x89, b'P', b'N', b'G', u8::try_from(index).unwrap_or(u8::MAX)]).collect() }
    }

    /// The `name="..."` of every part of a multipart body, in order.
    fn part_names(bytes: &[u8]) -> Vec<String> {
        let text = String::from_utf8_lossy(bytes);
        text.split("Content-Disposition: form-data; name=\"").skip(1).filter_map(|rest| rest.split('"').next().map(str::to_string)).collect()
    }

    /// The value of the text part `name`.
    fn text_value(bytes: &[u8], name: &str) -> Option<String> {
        let text = String::from_utf8_lossy(bytes);
        let marker = format!("name=\"{name}\"\r\n\r\n");
        let start = text.find(&marker)? + marker.len();
        text[start..].split("\r\n").next().map(str::to_string)
    }

    #[test]
    fn the_form_sends_every_field_once_then_the_images() {
        let body = ocr_form(&request(OcrMode::MaskAndText, 3)).unwrap_or_else(|error| panic!("form: {error:?}"));
        assert_eq!(part_names(&body.bytes), ["mode", "language", "type", "images", "images", "images"]);
        assert_eq!(text_value(&body.bytes, "mode").as_deref(), Some("MASK_AND_TEXT"));
        assert_eq!(text_value(&body.bytes, "language").as_deref(), Some("KO"));
        assert_eq!(text_value(&body.bytes, "type").as_deref(), Some("WEBTOON"));
        assert_eq!(String::from_utf8_lossy(&body.bytes).matches("filename=\"image.png\"\r\nContent-Type: image/png").count(), 3);
        assert!(body.content_type.starts_with("multipart/form-data; boundary="));
        let mask = ocr_form(&OcrRequest { params: OcrParams { language: NeurolateLanguage::Ja, comic_type: NeurolateComicType::Manga }, ..request(OcrMode::Mask, 1) }).unwrap_or_else(|error| panic!("form: {error:?}"));
        assert_eq!(text_value(&mask.bytes, "mode").as_deref(), Some("MASK"));
        assert_eq!(text_value(&mask.bytes, "language").as_deref(), Some("JA"));
        assert_eq!(text_value(&mask.bytes, "type").as_deref(), Some("MANGA"));
        assert_eq!(text_value(&ocr_form(&request(OcrMode::Text, 1)).map(|body| body.bytes).unwrap_or_default(), "mode").as_deref(), Some("TEXT"));
    }

    #[test]
    fn kind_is_sent_only_when_set() {
        let page = ocr_form(&OcrRequest { kind: Some(OcrKind::Page), ..request(OcrMode::Mask, 1) }).unwrap_or_else(|error| panic!("form: {error:?}"));
        assert_eq!(part_names(&page.bytes), ["mode", "language", "type", "kind", "images"]);
        assert_eq!(text_value(&page.bytes, "kind").as_deref(), Some("PAGE"));
        let block = ocr_form(&OcrRequest { kind: Some(OcrKind::Block), ..request(OcrMode::Text, 1) }).unwrap_or_else(|error| panic!("form: {error:?}"));
        assert_eq!(text_value(&block.bytes, "kind").as_deref(), Some("BLOCK"));
        let default = ocr_form(&request(OcrMode::Mask, 1)).unwrap_or_else(|error| panic!("form: {error:?}"));
        assert!(!part_names(&default.bytes).contains(&"kind".to_string()));
    }

    #[test]
    fn a_request_without_images_is_refused_before_sending() {
        assert!(matches!(ocr_form(&request(OcrMode::Mask, 0)), Err(NeurolateError::RequestBuild { .. })));
    }

    #[test]
    fn debug_never_shows_image_bytes() {
        let debug = format!("{:?}", request(OcrMode::Mask, 2));
        assert!(debug.contains("image_bytes: [5, 5]"), "{debug}");
        assert!(!debug.contains("137"), "{debug}");
    }

    #[test]
    fn enum_codes_round_trip_case_insensitively() {
        for language in NeurolateLanguage::ALL {
            assert_eq!(NeurolateLanguage::from_code(language.code()), Some(language));
            assert_eq!(NeurolateLanguage::from_code(&language.code().to_ascii_lowercase()), Some(language));
            assert_eq!(language.code().len(), 2);
        }
        assert_eq!(NeurolateLanguage::ALL.len(), 58);
        let unique: std::collections::HashSet<&str> = NeurolateLanguage::ALL.iter().map(|language| language.code()).collect();
        assert_eq!(unique.len(), 58, "every code once");
        assert_eq!(NeurolateLanguage::from_code("und"), None, "UND is not a source language");
        assert_eq!(NeurolateLanguage::from_code("uk"), Some(NeurolateLanguage::Uk));
        assert_eq!(NeurolateLanguage::from_code("zh-TW"), None);
        assert_eq!(NeurolateLanguage::from_code("auto"), None);
        for mode in [OcrMode::MaskAndText, OcrMode::Mask, OcrMode::Text] {
            assert_eq!(OcrMode::from_wire(&mode.wire().to_ascii_lowercase()), Some(mode));
        }
        assert_eq!(OcrMode::from_wire("BBOX"), None);
    }

    #[test]
    fn parses_a_page_answer() {
        let response = parse_ocr_response(200, OCR_PAGE.as_bytes(), 1).unwrap_or_else(|error| panic!("page: {error:?}"));
        assert_eq!(response.request_id, "00000000-0000-0000-0000-000000000000");
        assert!(response.cost.is_some_and(|cost| (cost - 0.142_613).abs() < 1e-9));
        let [image] = response.images.as_slice() else { panic!("one image: {response:?}") };
        assert_eq!((image.index, image.width, image.height), (0, 690, 1500));
        assert_eq!(image.mode_applied, Some(OcrMode::MaskAndText));
        assert_eq!(image.blocks.iter().map(|block| block.number).collect::<Vec<_>>(), [1, 2, 3]);
        let first = &image.blocks[0];
        assert_eq!(first.text.as_deref(), Some("안녕하세요! 오늘 날씨가 정말 좋네요."));
        assert_eq!(first.bbox.map(|bbox| (bbox.x, bbox.y, bbox.width, bbox.height)), Some((147, 133, 185, 133)));
        assert_eq!(first.polygons.len(), 1);
        assert_eq!(first.polygons[0].outer.len(), 231);
        assert!(first.polygons[0].holes.is_empty());
        assert_eq!((first.region, &first.text_status), (None, &None));
        assert!(image.blocks.iter().all(super::OcrBlock::has_usable_mask));
    }

    #[test]
    fn parses_the_current_answer_format() {
        let response = parse_ocr_response(200, OCR_PAGE_REQUEST_ID.as_bytes(), 1).unwrap_or_else(|error| panic!("current format: {error:?}"));
        assert_eq!(response.request_id, "00000000-0000-0000-0000-000000000000");
        assert!(response.cost.is_some_and(|cost| (cost - 0.504_631).abs() < 1e-9));
        let image = &response.images[0];
        assert_eq!((image.width, image.height, image.mode_applied), (690, 14700, Some(OcrMode::MaskAndText)));
        assert_eq!(image.blocks.iter().map(|block| (block.number, block.region, block.polygons.len())).collect::<Vec<_>>(), [(1, None, 1), (2, None, 3), (3, None, 2)]);
        assert_eq!(image.blocks[0].text.as_deref(), Some("테스트"));
        assert!(image.blocks.iter().all(super::OcrBlock::has_usable_mask));
    }

    #[test]
    fn the_request_id_and_mask_index_are_read_under_both_names() {
        let new = br#"{"requestId":"new","id":"old","images":[{"index":0,"width":1,"height":1,"blocks":[{"number":1,"maskIndex":4,"region":9}]}]}"#;
        let new = parse_ocr_response(200, new, 1).unwrap_or_else(|error| panic!("new names: {error:?}"));
        assert_eq!((new.request_id.as_str(), new.images[0].blocks[0].region), ("new", Some(4)), "the current names win");
        let old = br#"{"id":"old","images":[{"index":0,"width":1,"height":1,"blocks":[{"number":1,"maskIndex":null,"region":2}]}]}"#;
        let old = parse_ocr_response(200, old, 1).unwrap_or_else(|error| panic!("old names: {error:?}"));
        assert_eq!((old.request_id.as_str(), old.images[0].blocks[0].region), ("old", Some(2)));
        assert!(matches!(parse_ocr_response(200, br#"{"images":[]}"#, 0), Err(NeurolateError::Decode { .. })), "neither id is present");
    }

    // Recorded from the older server, whose crop rule was "up to 500x500": its 501x500 image
    // was a page that happened to hold one block. Kept as recorded data.
    #[test]
    fn parses_crops_as_one_block_each() {
        let response = parse_ocr_response(200, OCR_CROPS.as_bytes(), 3).unwrap_or_else(|error| panic!("crops: {error:?}"));
        assert_eq!(response.images.iter().map(|image| (image.width, image.height, image.blocks.len())).collect::<Vec<_>>(), [(300, 200, 1), (500, 500, 1), (501, 500, 1)]);
    }

    #[test]
    fn an_empty_region_has_no_usable_mask() {
        let response = parse_ocr_response(200, OCR_REGIONS_EMPTY.as_bytes(), 1).unwrap_or_else(|error| panic!("regions: {error:?}"));
        let blocks = &response.images[0].blocks;
        assert_eq!(blocks.iter().map(|block| block.region).collect::<Vec<_>>(), [Some(0), Some(1), Some(2)]);
        assert_eq!(blocks[0].text_status, Some(TextStatus::Ok));
        assert!(blocks[0].has_usable_mask() && blocks[1].has_usable_mask());
        let empty = &blocks[2];
        assert_eq!(empty.text_status, Some(TextStatus::Empty));
        assert_eq!(empty.text, None);
        assert_eq!(empty.polygons[0].outer.len(), 4, "the region rectangle comes back as the mask");
        assert!(!empty.has_usable_mask());
    }

    #[test]
    fn blank_text_answers_have_no_blocks_and_the_applied_mode() {
        let response = parse_ocr_response(200, OCR_BLANK_TEXT.as_bytes(), 4).unwrap_or_else(|error| panic!("blank: {error:?}"));
        assert_eq!(response.images.iter().map(|image| image.mode_applied).collect::<Vec<_>>(), [Some(OcrMode::Text), Some(OcrMode::MaskAndText), Some(OcrMode::MaskAndText), Some(OcrMode::Text)]);
        assert!(response.images.iter().all(|image| image.blocks.is_empty() && image.error.is_none()));
        assert_eq!(response.cost, Some(0.0), "a free answer states its zero cost");
    }

    #[test]
    fn tolerates_null_fields_unknown_values_and_per_image_errors() {
        let body = br#"{"id":"r","images":[{"index":0,"width":10,"height":10,"modeApplied":"SOMETHING_NEW","blocks":[{"number":1,"region":null,"text":null,"mask":null,"bbox":null,"textStatus":"PARTIAL","error":null}],"error":null},{"index":1,"width":5,"height":5,"modeApplied":null,"blocks":[],"error":{"code":"OCR_FAILED","message":"boom"}}]}"#;
        let response = parse_ocr_response(200, body, 2).unwrap_or_else(|error| panic!("tolerant: {error:?}"));
        assert_eq!(response.cost, None, "no usage reads as an unknown cost, never zero");
        let block = &response.images[0].blocks[0];
        assert_eq!(response.images[0].mode_applied, None);
        assert_eq!(block.text_status, Some(TextStatus::Unknown("PARTIAL".to_string())));
        assert!(block.polygons.is_empty() && block.bbox.is_none() && !block.has_usable_mask());
        assert_eq!(response.images[1].error.as_ref().map(|error| error.code.as_str()), Some("OCR_FAILED"));
    }

    #[test]
    fn malformed_geometry_is_a_decode_error() {
        for body in [&br#"{"id":"r","images":[{"index":0,"width":1,"height":1,"blocks":[{"number":1,"mask":[{"outer":[[1]]}]}]}]}"#[..], br#"{"id":"r","images":[{"index":-1,"width":1,"height":1,"blocks":[]}]}"#, br#"{"images":[]}"#, b"<html>"] {
            assert!(matches!(parse_ocr_response(200, body, 1), Err(NeurolateError::Decode { .. })), "{}", String::from_utf8_lossy(body));
        }
    }

    /// An answer with one empty result per `indices` entry.
    fn batch(indices: &[i64], usage: &str) -> String {
        let images: Vec<String> = indices.iter().map(|index| format!(r#"{{"index":{index},"width":1,"height":1,"blocks":[]}}"#)).collect();
        format!(r#"{{"id":"r","images":[{}]{usage}}}"#, images.join(","))
    }

    #[test]
    fn the_batch_must_answer_each_sent_image_exactly_once() {
        let ordered = parse_ocr_response(200, batch(&[2, 0, 1], "").as_bytes(), 3).unwrap_or_else(|error| panic!("reordered: {error:?}"));
        assert_eq!(ordered.images.iter().map(|image| image.index).collect::<Vec<_>>(), [0, 1, 2], "returned ordered by index");
        for (indices, expected) in [(&[0, 0, 1][..], 3), (&[0, 2][..], 2), (&[0, 1, 7][..], 3), (&[0, 1][..], 3), (&[0, 1, 2][..], 2), (&[4_294_967_296][..], 1)] {
            assert!(matches!(parse_ocr_response(200, batch(indices, "").as_bytes(), expected), Err(NeurolateError::Decode { .. })), "{indices:?} for {expected}");
        }
    }

    #[test]
    fn a_cost_that_is_not_a_finite_non_negative_number_is_unknown() {
        for usage in [r#","usage":null"#, r#","usage":{"cost":null}"#, r#","usage":{"cost":"0.1"}"#, r#","usage":{"cost":-0.5}"#, r#","usage":{}"#] {
            assert_eq!(parse_ocr_response(200, batch(&[0], usage).as_bytes(), 1).map(|response| response.cost).ok(), Some(None), "{usage}");
        }
        assert_eq!(parse_ocr_response(200, batch(&[0], r#","usage":{"cost":0.25}"#).as_bytes(), 1).map(|response| response.cost).ok(), Some(Some(0.25)));
    }

    #[test]
    fn parses_key_info() {
        let info = parse_key_info(200, KEY.as_bytes()).unwrap_or_else(|error| panic!("key: {error:?}"));
        assert_eq!((info.name.as_str(), info.key_prefix.as_str()), ("test", "nlk_TEST0000"));
        assert_eq!(info.limit_period, LimitPeriod::Week);
        assert_eq!(info.status, KeyStatus::Active);
        assert!((info.limit_value - 100.0).abs() < 1e-9 && (info.used_value - 12.5).abs() < 1e-9 && (info.remaining_value - 87.5).abs() < 1e-9);
        assert_eq!((info.resets_on.as_deref(), info.expires_at.as_deref()), (Some("2000-01-08"), None));
        let odd = br#"{"name":"k","keyPrefix":"nlk_x","limitValue":1,"limitPeriod":"MONTH","usedValue":0,"remainingValue":1,"resetsOn":null,"expiresAt":null,"status":"PAUSED"}"#;
        let odd = parse_key_info(200, odd).unwrap_or_else(|error| panic!("odd: {error:?}"));
        assert_eq!((odd.limit_period, odd.status), (LimitPeriod::Unknown("MONTH".to_string()), KeyStatus::Unknown("PAUSED".to_string())));
        for (field, bad) in [("limitValue", "-1"), ("usedValue", "-0.5"), ("remainingValue", "-3"), ("remainingValue", "\"9\""), ("limitValue", "null")] {
            let body = KEY.replacen(&format!("\"{field}\":"), &format!("\"{field}\":{bad},\"was\":"), 1);
            assert!(matches!(parse_key_info(200, body.as_bytes()), Err(NeurolateError::Decode { .. })), "{field}={bad}");
        }
    }

    #[test]
    fn statuses_map_onto_typed_errors() {
        match parse_ocr_response(400, ERROR_INVALID_LANGUAGE.as_bytes(), 1) {
            Err(NeurolateError::InvalidRequest { code, message, index }) => {
                assert_eq!(code, "INVALID_LANGUAGE");
                assert!(message.starts_with("'language' must be"));
                assert_eq!(index, None);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(status_error(400, br#"{"code":"INVALID_IMAGE","message":"x","index":2}"#), NeurolateError::InvalidRequest { index: Some(2), .. }));
        // A missing form field answers 401 with an empty body; this client always sends them.
        assert!(matches!(parse_ocr_response(401, b"", 1), Err(NeurolateError::KeyRejected)));
        assert!(matches!(parse_key_info(403, b""), Err(NeurolateError::KeyRejected)));
        assert!(matches!(status_error(402, br#"{"code":"API_KEY_LIMIT_EXCEEDED","message":"x"}"#), NeurolateError::OutOfCredits { code } if code == "API_KEY_LIMIT_EXCEEDED"));
        assert!(matches!(status_error(413, b""), NeurolateError::TooLarge));
        assert!(matches!(status_error(409, br#"{"code":"DUPLICATE_REQUEST","message":"x","requestId":"00000000-0000-0000-0000-000000000001"}"#), NeurolateError::DuplicateRequest { first_request_id: Some(id) } if id == "00000000-0000-0000-0000-000000000001"));
        assert!(matches!(status_error(409, br#"{"code":"DUPLICATE_REQUEST","message":"x","requestId":null}"#), NeurolateError::DuplicateRequest { first_request_id: None }));
        assert!(matches!(status_error(409, br#"{"code":"INVALID_IDEMPOTENCY_KEY","message":"x"}"#), NeurolateError::Http { status: 409, .. }));
        assert!(matches!(status_error(502, br#"{"code":"OCR_FAILED","message":"all failed"}"#), NeurolateError::ServiceFailed { message } if message == "all failed"));
        assert!(matches!(status_error(500, b"<html>oops</html>"), NeurolateError::Http { status: 500, message } if message.is_empty()));
    }
}
