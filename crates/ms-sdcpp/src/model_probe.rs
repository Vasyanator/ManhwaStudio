/*
File: crates/ms-sdcpp/src/model_probe.rs

Purpose:
Bounded, pure readers of model file HEADERS: GGUF (v2 / v3 metadata and tensor infos),
safetensors (the JSON header), Hugging Face `model.safetensors.index.json` (`weight_map`) and
`config.json` (model type and hidden size). They answer "which tensors with which shapes, and
which architecture" without reading any weight data.

Key items:
- ModelHeader / HeaderFormat : tensor shapes in PyTorch order plus GGUF metadata
- read_gguf() / read_safetensors_header() / read_index_weight_map() / read_hf_text_config()
- ProbeError, the caps GGUF_METADATA_CAP / SAFETENSORS_HEADER_CAP / INDEX_FILE_CAP /
  CONFIG_FILE_CAP and the parse bounds MAX_TENSORS / MAX_NAME_BYTES / MAX_TENSOR_DIMS

Notes:
- Every reader has a hard byte cap checked BEFORE allocating, so a corrupt or hostile file
  cannot make it allocate or read unbounded memory; GGUF arrays are skipped (strings read and
  dropped within the cap, numeric arrays seeked over).
- Shapes are normalized to PyTorch order (`[out, in]` for a linear weight): GGUF stores ggml
  order (`ne0` = innermost = input width), so its dims are reversed.
- Blocking file I/O when used on files: worker thread only.
*/

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};


/// Upper bound of the bytes read for a GGUF header (metadata + tensor infos).
pub const GGUF_METADATA_CAP: u64 = 128 * 1024 * 1024;
/// Upper bound of a safetensors JSON header. The largest real header under the app's model
/// folder measured 238 KB (2026-10-28; FLUX diffusion models 20-46 KB); large LLM checkpoints stay around
/// 1-2 MB. The parse is additionally bounded by [`MAX_TENSORS`] / [`MAX_NAME_BYTES`] /
/// [`MAX_TENSOR_DIMS`].
pub const SAFETENSORS_HEADER_CAP: u64 = 16 * 1024 * 1024;
/// Upper bound of a `model.safetensors.index.json` (largest measured: 65 KB, Qwen3-VL).
pub const INDEX_FILE_CAP: u64 = 4 * 1024 * 1024;
/// Upper bound of a `config.json` (largest measured: 2 KB).
pub const CONFIG_FILE_CAP: u64 = 1024 * 1024;
/// Most tensors (or `weight_map` entries) one header may list.
pub const MAX_TENSORS: usize = 65_536;
/// Longest tensor / shard name accepted, in bytes.
pub const MAX_NAME_BYTES: usize = 512;
/// Most dimensions a safetensors shape may have.
pub const MAX_TENSOR_DIMS: usize = 8;

/// GGUF magic bytes.
const GGUF_MAGIC: &[u8; 4] = b"GGUF";
/// Upper bound of tensor dimensions (ggml supports 4).
const GGUF_MAX_DIMS: u32 = 8;

/// Why a header could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProbeError {
    /// An I/O error other than a short read.
    #[error("I/O error: {0}")]
    Io(String),
    /// The file ended inside its header.
    #[error("the file ends inside its header")]
    Truncated,
    /// A length field exceeds its cap (`what`, announced `size`, `cap`).
    #[error("{what} of {size} bytes exceeds the limit of {cap} bytes")]
    TooLarge { what: &'static str, size: u64, cap: u64 },
    /// The bytes are not a valid header of the expected format.
    #[error("malformed header: {0}")]
    Malformed(String),
}

impl From<std::io::Error> for ProbeError {
    fn from(err: std::io::Error) -> Self {
        if err.kind() == std::io::ErrorKind::UnexpectedEof { Self::Truncated } else { Self::Io(err.to_string()) }
    }
}

/// The container format a header was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderFormat {
    /// GGUF (llama.cpp / ggml).
    Gguf,
    /// safetensors.
    Safetensors,
}

/// One GGUF metadata value (arrays are skipped and only described).
#[derive(Debug, Clone, PartialEq)]
pub enum GgufValue {
    /// Any unsigned integer type.
    UInt(u64),
    /// Any signed integer type.
    Int(i64),
    /// `f32` / `f64`.
    Float(f64),
    /// `bool`.
    Bool(bool),
    /// UTF-8 string (lossy).
    String(String),
    /// An array of `len` elements of GGUF type `elem_type`, not kept.
    Array { elem_type: u32, len: u64 },
}

/// Tensor shapes (`PyTorch` order) and, for GGUF, the scalar metadata of one file.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelHeader {
    /// The container format.
    pub format: HeaderFormat,
    /// Tensor name -> shape, outermost dimension first.
    pub tensors: BTreeMap<String, Vec<u64>>,
    /// GGUF key/value metadata (empty for safetensors).
    pub metadata: BTreeMap<String, GgufValue>,
}

impl ModelHeader {
    /// The input width (last `PyTorch` dimension) of tensor `name`.
    #[must_use]
    pub fn input_width(&self, name: &str) -> Option<u64> {
        self.tensors.get(name).and_then(|shape| shape.last().copied())
    }

    /// Whether any tensor name starts with `prefix`.
    #[must_use]
    pub fn has_prefix(&self, prefix: &str) -> bool {
        self.tensors.range(prefix.to_owned()..).next().is_some_and(|(name, _)| name.starts_with(prefix))
    }

    /// GGUF string metadata `key`.
    #[must_use]
    pub fn meta_str(&self, key: &str) -> Option<&str> {
        match self.metadata.get(key) {
            Some(GgufValue::String(value)) => Some(value),
            _ => None,
        }
    }

    /// GGUF unsigned / non-negative integer metadata `key`.
    #[must_use]
    pub fn meta_u64(&self, key: &str) -> Option<u64> {
        match self.metadata.get(key) {
            Some(GgufValue::UInt(value)) => Some(*value),
            Some(GgufValue::Int(value)) => u64::try_from(*value).ok(),
            _ => None,
        }
    }
}

/// A reader that refuses to consume more than `remaining` bytes.
struct Budget<'a, R> {
    inner: &'a mut R,
    remaining: u64,
}

impl<R: Read + Seek> Budget<'_, R> {
    fn take(&mut self, len: u64, what: &'static str) -> Result<(), ProbeError> {
        if len > self.remaining {
            return Err(ProbeError::TooLarge { what, size: len, cap: GGUF_METADATA_CAP });
        }
        self.remaining -= len;
        Ok(())
    }

    fn bytes(&mut self, len: u64, what: &'static str) -> Result<Vec<u8>, ProbeError> {
        self.take(len, what)?;
        let len = usize::try_from(len).map_err(|_| ProbeError::TooLarge { what, size: len, cap: GGUF_METADATA_CAP })?;
        let mut buf = vec![0_u8; len];
        self.inner.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ProbeError> {
        self.take(u64::try_from(N).unwrap_or(u64::MAX), "a GGUF field")?;
        let mut buf = [0_u8; N];
        self.inner.read_exact(&mut buf)?;
        Ok(buf)
    }

    fn u32(&mut self) -> Result<u32, ProbeError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, ProbeError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn string(&mut self) -> Result<String, ProbeError> {
        let len = self.u64()?;
        let bytes = self.bytes(len, "a GGUF string")?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Skips `len` bytes by seeking (no allocation); the skip still counts against the cap.
    fn skip(&mut self, len: u64) -> Result<(), ProbeError> {
        self.take(len, "a GGUF array")?;
        let offset = i64::try_from(len).map_err(|_| ProbeError::Malformed("array too long".to_owned()))?;
        self.inner.seek(SeekFrom::Current(offset))?;
        Ok(())
    }
}

/// Byte size of a scalar GGUF value type, `None` for string / array / unknown.
fn gguf_scalar_size(value_type: u32) -> Option<u64> {
    match value_type {
        0 | 1 | 7 => Some(1),
        2 | 3 => Some(2),
        4..=6 => Some(4),
        10..=12 => Some(8),
        _ => None,
    }
}

/// Reads one GGUF value of `value_type`.
fn gguf_value<R: Read + Seek>(budget: &mut Budget<'_, R>, value_type: u32) -> Result<GgufValue, ProbeError> {
    Ok(match value_type {
        0 => GgufValue::UInt(u64::from(budget.array::<1>()?[0])),
        1 => GgufValue::Int(i64::from(i8::from_le_bytes(budget.array()?))),
        2 => GgufValue::UInt(u64::from(u16::from_le_bytes(budget.array()?))),
        3 => GgufValue::Int(i64::from(i16::from_le_bytes(budget.array()?))),
        4 => GgufValue::UInt(u64::from(budget.u32()?)),
        5 => GgufValue::Int(i64::from(i32::from_le_bytes(budget.array()?))),
        6 => GgufValue::Float(f64::from(f32::from_le_bytes(budget.array()?))),
        7 => GgufValue::Bool(budget.array::<1>()?[0] != 0),
        8 => GgufValue::String(budget.string()?),
        9 => {
            let elem_type = budget.u32()?;
            let len = budget.u64()?;
            if elem_type == 8 {
                for _ in 0..len {
                    budget.string()?;
                }
            } else {
                let size = gguf_scalar_size(elem_type).ok_or_else(|| ProbeError::Malformed(format!("unsupported GGUF array element type {elem_type}")))?;
                let total = size.checked_mul(len).ok_or(ProbeError::TooLarge { what: "a GGUF array", size: u64::MAX, cap: GGUF_METADATA_CAP })?;
                budget.skip(total)?;
            }
            GgufValue::Array { elem_type, len }
        }
        10 => GgufValue::UInt(budget.u64()?),
        11 => GgufValue::Int(i64::from_le_bytes(budget.array()?)),
        12 => GgufValue::Float(f64::from_le_bytes(budget.array()?)),
        other => return Err(ProbeError::Malformed(format!("unknown GGUF value type {other}"))),
    })
}

/// Reads the GGUF (v2 / v3) header: every scalar metadata value (arrays described, not kept)
/// and every tensor's name and shape. At most [`GGUF_METADATA_CAP`] bytes are consumed.
///
/// # Errors
/// [`ProbeError`]: not GGUF or an unsupported version (`Malformed`), a cap exceeded
/// (`TooLarge`), a short file (`Truncated`), I/O.
pub fn read_gguf<R: Read + Seek>(reader: &mut R) -> Result<ModelHeader, ProbeError> {
    let mut budget = Budget { inner: reader, remaining: GGUF_METADATA_CAP };
    if &budget.array::<4>()? != GGUF_MAGIC {
        return Err(ProbeError::Malformed("not a GGUF file".to_owned()));
    }
    let version = budget.u32()?;
    if !(2..=3).contains(&version) {
        return Err(ProbeError::Malformed(format!("unsupported GGUF version {version}")));
    }
    let tensor_count = budget.u64()?;
    let kv_count = budget.u64()?;
    let mut metadata = BTreeMap::new();
    for _ in 0..kv_count {
        let key = budget.string()?;
        let value_type = budget.u32()?;
        metadata.insert(key, gguf_value(&mut budget, value_type)?);
    }
    let mut tensors = BTreeMap::new();
    for _ in 0..tensor_count {
        let name = budget.string()?;
        let dims = budget.u32()?;
        if dims > GGUF_MAX_DIMS {
            return Err(ProbeError::Malformed(format!("tensor {name} has {dims} dimensions")));
        }
        let mut shape = (0..dims).map(|_| budget.u64()).collect::<Result<Vec<u64>, ProbeError>>()?;
        // ggml order (innermost first) -> PyTorch order (outermost first).
        shape.reverse();
        // Element type and data offset are not needed.
        budget.u32()?;
        budget.u64()?;
        tensors.insert(name, shape);
    }
    Ok(ModelHeader { format: HeaderFormat::Gguf, tensors, metadata })
}

/// Reads the safetensors JSON header: every tensor's name and shape (`__metadata__` and the
/// other entry fields are skipped without being materialized). The header length is capped
/// at [`SAFETENSORS_HEADER_CAP`]; the parse is typed and bounded ([`MAX_TENSORS`] entries,
/// [`MAX_NAME_BYTES`] per name, [`MAX_TENSOR_DIMS`] per shape).
///
/// # Errors
/// [`ProbeError`]: cap or bound exceeded, short file, invalid JSON or shape, I/O.
pub fn read_safetensors_header<R: Read>(reader: &mut R) -> Result<ModelHeader, ProbeError> {
    let mut len = [0_u8; 8];
    reader.read_exact(&mut len)?;
    let len = u64::from_le_bytes(len);
    if len > SAFETENSORS_HEADER_CAP {
        return Err(ProbeError::TooLarge { what: "the safetensors header", size: len, cap: SAFETENSORS_HEADER_CAP });
    }
    let len = usize::try_from(len).map_err(|_| ProbeError::TooLarge { what: "the safetensors header", size: len, cap: SAFETENSORS_HEADER_CAP })?;
    let mut bytes = vec![0_u8; len];
    reader.read_exact(&mut bytes)?;
    let header: bounded::SafetensorsHeader = serde_json::from_slice(&bytes).map_err(|err| ProbeError::Malformed(format!("safetensors header: {err}")))?;
    Ok(ModelHeader { format: HeaderFormat::Safetensors, tensors: header.0, metadata: BTreeMap::new() })
}

/// Reads at most `cap` bytes of a JSON file and parses them as `T` (a bounded typed shape).
fn read_capped_json<T: serde::de::DeserializeOwned, R: Read>(reader: &mut R, cap: u64, what: &'static str) -> Result<T, ProbeError> {
    let mut bytes = Vec::new();
    reader.take(cap + 1).read_to_end(&mut bytes)?;
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if len > cap {
        return Err(ProbeError::TooLarge { what, size: len, cap });
    }
    serde_json::from_slice(&bytes).map_err(|err| ProbeError::Malformed(format!("{what}: {err}")))
}

/// Reads the `weight_map` (tensor name -> shard file name) of a Hugging Face
/// `model.safetensors.index.json` (other fields skipped; at most [`MAX_TENSORS`] entries of
/// at most [`MAX_NAME_BYTES`] each).
///
/// # Errors
/// [`ProbeError`]: over [`INDEX_FILE_CAP`] or a bound, invalid JSON, no `weight_map` object of
/// strings.
pub fn read_index_weight_map<R: Read>(reader: &mut R) -> Result<BTreeMap<String, String>, ProbeError> {
    let index: bounded::IndexFile = read_capped_json(reader, INDEX_FILE_CAP, "the safetensors index")?;
    Ok(index.weight_map.0)
}

/// The text-model facts of a Hugging Face `config.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HfTextConfig {
    /// Top-level `model_type` (`qwen3`, `qwen3_vl`).
    pub model_type: Option<String>,
    /// The text model's hidden size: `text_config.hidden_size` when present (vision-language
    /// configs nest the text model there and have a different top-level / vision size), else
    /// the top-level `hidden_size`.
    pub hidden_size: Option<u64>,
}

/// Reads [`HfTextConfig`] from a `config.json` (every other field skipped).
///
/// # Errors
/// [`ProbeError`]: over [`CONFIG_FILE_CAP`], invalid JSON, a non-integer hidden size.
pub fn read_hf_text_config<R: Read>(reader: &mut R) -> Result<HfTextConfig, ProbeError> {
    let config: bounded::HfConfig = read_capped_json(reader, CONFIG_FILE_CAP, "config.json")?;
    let hidden_size = config.text_config.and_then(|text| text.hidden_size).or(config.hidden_size);
    Ok(HfTextConfig { model_type: config.model_type.map(|name| name.0), hidden_size })
}

/// Typed, allocation-bounded shapes of the JSON headers: names and shapes are checked against
/// their bounds before they are stored, and every unneeded value is skipped (`IgnoredAny`,
/// nothing materialized).
mod bounded {
    use std::collections::BTreeMap;
    use std::fmt;

    use serde::Deserialize;
    use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

    use super::{MAX_NAME_BYTES, MAX_TENSOR_DIMS, MAX_TENSORS};

    /// A string of at most [`MAX_NAME_BYTES`] bytes.
    pub(super) struct Name(pub(super) String);

    impl<'de> Deserialize<'de> for Name {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct NameVisitor;
            impl Visitor<'_> for NameVisitor {
                type Value = Name;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, "a string of at most {MAX_NAME_BYTES} bytes")
                }
                fn visit_str<E: de::Error>(self, value: &str) -> Result<Name, E> {
                    if value.len() > MAX_NAME_BYTES {
                        return Err(E::custom(format!("a name of {} bytes exceeds {MAX_NAME_BYTES}", value.len())));
                    }
                    Ok(Name(value.to_owned()))
                }
            }
            deserializer.deserialize_str(NameVisitor)
        }
    }

    /// A tensor shape of at most [`MAX_TENSOR_DIMS`] non-negative integers.
    pub(super) struct Shape(pub(super) Vec<u64>);

    impl<'de> Deserialize<'de> for Shape {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct ShapeVisitor;
            impl<'de> Visitor<'de> for ShapeVisitor {
                type Value = Shape;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    write!(f, "an array of at most {MAX_TENSOR_DIMS} dimensions")
                }
                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Shape, A::Error> {
                    let mut dims = Vec::new();
                    while let Some(dim) = seq.next_element::<u64>()? {
                        if dims.len() == MAX_TENSOR_DIMS {
                            return Err(de::Error::custom(format!("a shape with more than {MAX_TENSOR_DIMS} dimensions")));
                        }
                        dims.push(dim);
                    }
                    Ok(Shape(dims))
                }
            }
            deserializer.deserialize_seq(ShapeVisitor)
        }
    }

    /// One safetensors header entry: only the shape is kept.
    #[derive(Deserialize)]
    struct TensorEntry {
        shape: Shape,
    }

    /// The safetensors header: tensor name -> shape.
    pub(super) struct SafetensorsHeader(pub(super) BTreeMap<String, Vec<u64>>);

    impl<'de> Deserialize<'de> for SafetensorsHeader {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct HeaderVisitor;
            impl<'de> Visitor<'de> for HeaderVisitor {
                type Value = SafetensorsHeader;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a safetensors header object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<SafetensorsHeader, A::Error> {
                    let mut tensors = BTreeMap::new();
                    while let Some(Name(name)) = map.next_key::<Name>()? {
                        if name == "__metadata__" {
                            map.next_value::<IgnoredAny>()?;
                            continue;
                        }
                        if tensors.len() == MAX_TENSORS {
                            return Err(de::Error::custom(format!("more than {MAX_TENSORS} tensors")));
                        }
                        let entry = map.next_value::<TensorEntry>()?;
                        tensors.insert(name, entry.shape.0);
                    }
                    Ok(SafetensorsHeader(tensors))
                }
            }
            deserializer.deserialize_map(HeaderVisitor)
        }
    }

    /// A `weight_map`: tensor name -> shard file name.
    pub(super) struct WeightMap(pub(super) BTreeMap<String, String>);

    impl<'de> Deserialize<'de> for WeightMap {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct MapVisitor;
            impl<'de> Visitor<'de> for MapVisitor {
                type Value = WeightMap;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a weight_map object of strings")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<WeightMap, A::Error> {
                    let mut entries = BTreeMap::new();
                    while let Some(Name(name)) = map.next_key::<Name>()? {
                        if entries.len() == MAX_TENSORS {
                            return Err(de::Error::custom(format!("more than {MAX_TENSORS} weight_map entries")));
                        }
                        let Name(shard) = map.next_value::<Name>()?;
                        entries.insert(name, shard);
                    }
                    Ok(WeightMap(entries))
                }
            }
            deserializer.deserialize_map(MapVisitor)
        }
    }

    /// `model.safetensors.index.json` (only `weight_map`).
    #[derive(Deserialize)]
    pub(super) struct IndexFile {
        pub(super) weight_map: WeightMap,
    }

    /// The nested text config of a vision-language `config.json`.
    #[derive(Deserialize)]
    pub(super) struct TextConfig {
        pub(super) hidden_size: Option<u64>,
    }

    /// The fields of a `config.json` the probe needs.
    #[derive(Deserialize)]
    pub(super) struct HfConfig {
        pub(super) model_type: Option<Name>,
        pub(super) hidden_size: Option<u64>,
        pub(super) text_config: Option<TextConfig>,
    }
}

/// Test-only writers of tiny synthetic GGUF / safetensors headers (shared with `families`).
#[cfg(test)]
pub(crate) mod fixtures {
    use std::io::Write;

    /// A GGUF v3 file with string metadata `kv` and tensors `(name, ggml-order dims)`.
    pub(crate) fn gguf(kv: &[(&str, &str)], uint_kv: &[(&str, u32)], tensors: &[(&str, &[u64])]) -> Vec<u8> {
        let mut out = Vec::new();
        let string = |out: &mut Vec<u8>, text: &str| {
            out.extend_from_slice(&u64::try_from(text.len()).expect("len fits u64").to_le_bytes());
            out.extend_from_slice(text.as_bytes());
        };
        out.extend_from_slice(b"GGUF");
        out.extend_from_slice(&3_u32.to_le_bytes());
        out.extend_from_slice(&u64::try_from(tensors.len()).expect("count").to_le_bytes());
        out.extend_from_slice(&u64::try_from(kv.len() + uint_kv.len() + 1).expect("count").to_le_bytes());
        for (key, value) in kv {
            string(&mut out, key);
            out.extend_from_slice(&8_u32.to_le_bytes());
            string(&mut out, value);
        }
        for (key, value) in uint_kv {
            string(&mut out, key);
            out.extend_from_slice(&4_u32.to_le_bytes());
            out.extend_from_slice(&value.to_le_bytes());
        }
        // One array of each kind, so the readers' skipping is exercised.
        string(&mut out, "tokenizer.ggml.tokens");
        out.extend_from_slice(&9_u32.to_le_bytes());
        out.extend_from_slice(&8_u32.to_le_bytes());
        out.extend_from_slice(&2_u64.to_le_bytes());
        string(&mut out, "a");
        string(&mut out, "bc");
        for (name, dims) in tensors {
            string(&mut out, name);
            out.extend_from_slice(&u32::try_from(dims.len()).expect("dims").to_le_bytes());
            for dim in *dims {
                out.extend_from_slice(&dim.to_le_bytes());
            }
            out.extend_from_slice(&0_u32.to_le_bytes());
            out.extend_from_slice(&0_u64.to_le_bytes());
        }
        out
    }

    /// A safetensors file (header only, no data) with tensors `(name, PyTorch-order shape)`.
    pub(crate) fn safetensors(tensors: &[(&str, &[u64])]) -> Vec<u8> {
        let mut header = serde_json::Map::new();
        header.insert("__metadata__".to_owned(), serde_json::json!({"format": "pt"}));
        for (name, shape) in tensors {
            header.insert((*name).to_owned(), serde_json::json!({"dtype": "BF16", "shape": shape, "data_offsets": [0, 0]}));
        }
        let json = serde_json::to_vec(&header).expect("serialize header");
        let mut out = Vec::new();
        out.write_all(&u64::try_from(json.len()).expect("len").to_le_bytes()).expect("write len");
        out.write_all(&json).expect("write header");
        out
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::fixtures::{gguf, safetensors};
    use super::*;

    #[test]
    fn gguf_header_is_read_and_reversed_to_pytorch_order() {
        let bytes = gguf(&[("general.architecture", "qwen3")], &[("qwen3.embedding_length", 2560)], &[("txt_in.weight", &[7680, 3072]), ("blk.0.attn_k.weight", &[2560, 1024])]);
        let header = read_gguf(&mut Cursor::new(bytes)).expect("gguf");
        assert_eq!(header.format, HeaderFormat::Gguf);
        assert_eq!(header.meta_str("general.architecture"), Some("qwen3"));
        assert_eq!(header.meta_u64("qwen3.embedding_length"), Some(2560));
        assert_eq!(header.tensors["txt_in.weight"], vec![3072, 7680]);
        assert_eq!(header.input_width("txt_in.weight"), Some(7680));
        assert!(header.has_prefix("blk."));
        assert!(!header.has_prefix("txt_mode_blocks."));
        assert!(matches!(header.metadata.get("tokenizer.ggml.tokens"), Some(GgufValue::Array { elem_type: 8, len: 2 })));
    }

    #[test]
    fn gguf_rejects_bad_magic_version_truncation_and_huge_strings() {
        assert!(matches!(read_gguf(&mut Cursor::new(b"GGML\x03\0\0\0".to_vec())), Err(ProbeError::Malformed(_))));
        let mut v1 = gguf(&[], &[], &[]);
        v1[4] = 1;
        assert!(matches!(read_gguf(&mut Cursor::new(v1)), Err(ProbeError::Malformed(_))));
        let full = gguf(&[("general.architecture", "flux")], &[], &[("txt_in.weight", &[7680, 3072])]);
        for cut in [3, 10, 30, full.len() - 1] {
            assert_eq!(read_gguf(&mut Cursor::new(full[..cut].to_vec())), Err(ProbeError::Truncated), "cut at {cut}");
        }
        // A string announcing 1 TiB is refused before any allocation.
        let mut huge = b"GGUF".to_vec();
        huge.extend_from_slice(&3_u32.to_le_bytes());
        huge.extend_from_slice(&0_u64.to_le_bytes());
        huge.extend_from_slice(&1_u64.to_le_bytes());
        huge.extend_from_slice(&(1_u64 << 40).to_le_bytes());
        assert!(matches!(read_gguf(&mut Cursor::new(huge)), Err(ProbeError::TooLarge { .. })));
    }

    #[test]
    fn safetensors_header_is_read_and_capped() {
        let bytes = safetensors(&[("txt_in.weight", &[4096, 12288])]);
        let header = read_safetensors_header(&mut Cursor::new(bytes.clone())).expect("safetensors");
        assert_eq!(header.input_width("txt_in.weight"), Some(12288));
        assert_eq!(header.tensors.len(), 1);
        assert_eq!(read_safetensors_header(&mut Cursor::new(bytes[..bytes.len() - 2].to_vec())), Err(ProbeError::Truncated));
        let mut over = (SAFETENSORS_HEADER_CAP + 1).to_le_bytes().to_vec();
        over.extend_from_slice(b"{}");
        assert!(matches!(read_safetensors_header(&mut Cursor::new(over)), Err(ProbeError::TooLarge { .. })));
        let mut not_json = 3_u64.to_le_bytes().to_vec();
        not_json.extend_from_slice(b"abc");
        assert!(matches!(read_safetensors_header(&mut Cursor::new(not_json)), Err(ProbeError::Malformed(_))));
    }

    #[test]
    fn index_and_config_readers() {
        let index = br#"{"metadata":{},"weight_map":{"model.embed_tokens.weight":"model-00001-of-00002.safetensors"}}"#;
        let map = read_index_weight_map(&mut Cursor::new(index.to_vec())).expect("index");
        assert_eq!(map["model.embed_tokens.weight"], "model-00001-of-00002.safetensors");
        assert!(read_index_weight_map(&mut Cursor::new(b"{}".to_vec())).is_err());

        let vl = br#"{"model_type":"qwen3_vl","text_config":{"hidden_size":2560},"vision_config":{"hidden_size":1024}}"#;
        assert_eq!(read_hf_text_config(&mut Cursor::new(vl.to_vec())).expect("vl"), HfTextConfig { model_type: Some("qwen3_vl".into()), hidden_size: Some(2560) });
        let text = br#"{"model_type":"qwen3","hidden_size":4096}"#;
        assert_eq!(read_hf_text_config(&mut Cursor::new(text.to_vec())).expect("text").hidden_size, Some(4096));

        let mut big = b"[".to_vec();
        big.resize(usize::try_from(CONFIG_FILE_CAP).expect("cap fits") + 10, b' ');
        assert!(matches!(read_hf_text_config(&mut Cursor::new(big)), Err(ProbeError::TooLarge { .. })));
        assert!(matches!(read_hf_text_config(&mut Cursor::new(br#"{"hidden_size":"big"}"#.to_vec())), Err(ProbeError::Malformed(_))));
    }

    /// A safetensors file whose header is `json`.
    fn raw_safetensors(json: &str) -> Vec<u8> {
        let mut out = u64::try_from(json.len()).expect("len").to_le_bytes().to_vec();
        out.extend_from_slice(json.as_bytes());
        out
    }

    #[test]
    fn hostile_header_shapes_are_refused() {
        let long_name = format!(r#"{{"{}":{{"shape":[1]}}}}"#, "n".repeat(MAX_NAME_BYTES + 1));
        assert!(matches!(read_safetensors_header(&mut Cursor::new(raw_safetensors(&long_name))), Err(ProbeError::Malformed(_))));
        let many_dims = r#"{"t":{"shape":[1,1,1,1,1,1,1,1,1]}}"#;
        assert!(matches!(read_safetensors_header(&mut Cursor::new(raw_safetensors(many_dims))), Err(ProbeError::Malformed(_))));
        let negative = r#"{"t":{"shape":[-1]}}"#;
        assert!(matches!(read_safetensors_header(&mut Cursor::new(raw_safetensors(negative))), Err(ProbeError::Malformed(_))));
        let mut entries: Vec<String> = (0..=MAX_TENSORS).map(|index| format!(r#""t{index}":{{"shape":[1]}}"#)).collect();
        let too_many = format!("{{{}}}", entries.join(","));
        assert!(matches!(read_safetensors_header(&mut Cursor::new(raw_safetensors(&too_many))), Err(ProbeError::Malformed(_))));
        // Exactly the limit is fine; skipped fields (metadata, dtype, offsets) cost nothing.
        entries.pop();
        let at_limit = format!(r#"{{"__metadata__":{{"x":[[[{{}}]]]}},{}}}"#, entries.join(","));
        assert_eq!(read_safetensors_header(&mut Cursor::new(raw_safetensors(&at_limit))).expect("at the limit").tensors.len(), MAX_TENSORS);

        let long_shard = format!(r#"{{"weight_map":{{"a":"{}"}}}}"#, "s".repeat(MAX_NAME_BYTES + 1));
        assert!(matches!(read_index_weight_map(&mut Cursor::new(long_shard.into_bytes())), Err(ProbeError::Malformed(_))));
        let not_strings = br#"{"weight_map":{"a":1}}"#;
        assert!(matches!(read_index_weight_map(&mut Cursor::new(not_strings.to_vec())), Err(ProbeError::Malformed(_))));
        let mut over = b"{".to_vec();
        over.resize(usize::try_from(INDEX_FILE_CAP).expect("cap fits") + 2, b' ');
        assert!(matches!(read_index_weight_map(&mut Cursor::new(over)), Err(ProbeError::TooLarge { .. })));
    }
}
