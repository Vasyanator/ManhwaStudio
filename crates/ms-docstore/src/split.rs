/*
File: split.rs

Purpose:
The pure half of the SQLite fragment codec: a JSON `Value` <-> the `frag` rows of plan §C.
No I/O. This is the part the Python backend mirrors byte-for-byte (`docstore.py` `split` /
`join`); the golden fixtures in `crates/ms-docstore/fixtures/` pin the contract.

Key structures:
- FragKind : `obj` / `arr` / `arr_id` / `ent` / `leaf`
- Row      : one `frag` row (path, parent, seg, kind, payload)

Key functions:
- split()           : Value -> rows sorted by path (byte order == code-point order)
- join()            : rows -> Value, strict (every contract violation is an error)
- payload_unchanged(): the diff-write equality rule (text equal OR type-strict value equal)
- compact()         : compact JSON text = `serde_json::to_string` of a BTreeMap `Value`

Notes (the binding rules; see the crate MODULE_README "Split rule" and docstore.py's
SPEC CLARIFICATIONS 1-9):
- Root row: path "", parent NULL, seg NULL. A child's parent is the parent's PATH ("" for
  top-level children), path = parent path + "/" + esc(seg), esc = RFC 6901 (`~`->`~0`
  first, then `/`->`~1`); seg holds the RAW segment text.
- Object -> `obj` (payload NULL), one child per key.
- Array: id-keyed iff non-empty, every element an object, key name `id` if the FIRST
  element has `id`, else `uid` if it has `uid`; every element carries that key as a JSON
  string or integer (not bool, not float); the compact key texts pairwise distinct. Then
  `arr_id` (payload = compact JSON array of the keys in order) + one `ent` row per element
  (seg = compact key text, payload = compact JSON of the whole element). Else, an array
  with no object/array element (including `[]`) is one `leaf`; else `arr` (payload =
  element count) with positional children "0".."n-1".
- Scalars -> `leaf` with the compact JSON of the value.
- `serde_json` is built WITHOUT `preserve_order` (Map = BTreeMap) and WITH
  `float_roundtrip`, so compact text is canonical and parsing it back is exact.
- Recursion depth is bounded by `MAX_DEPTH` in `join` (rows come from a file and may be
  hostile); `split` inputs are `Value`s the process already holds.
*/

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

/// Deepest nesting `join` rebuilds before declaring the rows malformed (a guard against a
/// hostile or corrupt parent chain overflowing the stack; real documents nest < 20 deep).
const MAX_DEPTH: usize = 512;

/// Kind of one `frag` row (the `kind` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FragKind {
    /// An object; payload NULL; children keyed by `seg`.
    Obj,
    /// A positional array; payload = element count; children segs "0".."n-1".
    Arr,
    /// An id-keyed array; payload = compact JSON array of the keys in element order.
    ArrId,
    /// One element of an id-keyed array, stored whole; seg = compact key text.
    Ent,
    /// Any value stored whole (scalar, scalar-only array, `[]`).
    Leaf,
}

impl FragKind {
    /// The `kind` column text.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Obj => "obj",
            Self::Arr => "arr",
            Self::ArrId => "arr_id",
            Self::Ent => "ent",
            Self::Leaf => "leaf",
        }
    }

    /// Parses the `kind` column; `None` for an unknown kind (a malformed row).
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "obj" => Some(Self::Obj),
            "arr" => Some(Self::Arr),
            "arr_id" => Some(Self::ArrId),
            "ent" => Some(Self::Ent),
            "leaf" => Some(Self::Leaf),
            _ => None,
        }
    }
}

/// One `frag` row. `parent`/`seg` are `None` only for the root; `payload` `None` is SQL NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    /// JSON-Pointer-style key of the fragment (root = "").
    pub(crate) path: String,
    /// The parent row's path; `None` for the root.
    pub(crate) parent: Option<String>,
    /// Raw (unescaped) segment text; `None` for the root.
    pub(crate) seg: Option<String>,
    /// Row kind.
    pub(crate) kind: FragKind,
    /// Kind-specific payload (see [`FragKind`]).
    pub(crate) payload: Option<String>,
}

/// Compact JSON text of `value` (`serde_json::to_string` of a `BTreeMap` `Value`). Serializing
/// a `Value` cannot fail: its keys are strings and it holds no non-finite floats.
pub(crate) fn compact(value: &Value) -> String {
    // `Value`'s `Display` is exactly `serde_json::to_string` and is infallible.
    value.to_string()
}

/// RFC 6901 escaping of one segment: `~` -> `~0` first, then `/` -> `~1`.
fn escape_segment(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

/// `parent_path + "/" + esc(seg)`.
fn child_path(parent_path: &str, seg: &str) -> String {
    let escaped = escape_segment(seg);
    let mut path = String::with_capacity(parent_path.len() + 1 + escaped.len());
    path.push_str(parent_path);
    path.push('/');
    path.push_str(&escaped);
    path
}

/// The key field name of an id-keyed array, or `None` when the array is not id-keyed
/// (see the file header for the rule).
fn id_key_name(array: &[Value]) -> Option<&'static str> {
    let first = array.first()?.as_object()?;
    if !array.iter().all(Value::is_object) {
        return None;
    }
    let name = if first.contains_key("id") {
        "id"
    } else if first.contains_key("uid") {
        "uid"
    } else {
        return None;
    };
    let mut texts = std::collections::HashSet::with_capacity(array.len());
    for element in array {
        let key = element.get(name)?;
        // A JSON integer is a Number that is NOT a float (serde keeps `1` and `1.0` apart);
        // bools are a different variant altogether.
        let is_integer = key.as_number().is_some_and(|number| number.is_i64() || number.is_u64());
        if !(key.is_string() || is_integer) {
            return None;
        }
        if !texts.insert(compact(key)) {
            return None;
        }
    }
    Some(name)
}

/// Appends the rows of `value` at (`path`, `parent`, `seg`) to `out`.
fn split_into(value: &Value, path: String, parent: Option<String>, seg: Option<String>, out: &mut Vec<Row>) {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                split_into(nested, child_path(&path, key), Some(path.clone()), Some(key.clone()), out);
            }
            out.push(Row { path, parent, seg, kind: FragKind::Obj, payload: None });
        }
        Value::Array(items) => {
            if let Some(name) = id_key_name(items) {
                let order: Vec<Value> = items.iter().filter_map(|element| element.get(name).cloned()).collect();
                for element in items {
                    // `id_key_name` guaranteed every element carries the key.
                    let key_text = element.get(name).map(compact).unwrap_or_default();
                    out.push(Row { path: child_path(&path, &key_text), parent: Some(path.clone()), seg: Some(key_text), kind: FragKind::Ent, payload: Some(compact(element)) });
                }
                out.push(Row { path, parent, seg, kind: FragKind::ArrId, payload: Some(compact(&Value::Array(order))) });
            } else if items.iter().any(|element| element.is_object() || element.is_array()) {
                for (index, element) in items.iter().enumerate() {
                    let index_text = index.to_string();
                    split_into(element, child_path(&path, &index_text), Some(path.clone()), Some(index_text), out);
                }
                out.push(Row { path, parent, seg, kind: FragKind::Arr, payload: Some(items.len().to_string()) });
            } else {
                // Scalar-only arrays (and `[]`) stay one row.
                out.push(Row { path, parent, seg, kind: FragKind::Leaf, payload: Some(compact(value)) });
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
            out.push(Row { path, parent, seg, kind: FragKind::Leaf, payload: Some(compact(value)) });
        }
    }
}

/// Splits `value` into `frag` rows sorted by path (byte order, equal to code-point order).
pub(crate) fn split(value: &Value) -> Vec<Row> {
    let mut rows = Vec::new();
    split_into(value, String::new(), None, None, &mut rows);
    rows.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    rows
}

/// Parses a payload as JSON; `what` names the row for the error.
fn parse_payload(row: &Row) -> Result<Value, String> {
    let Some(payload) = row.payload.as_deref() else { return Err(format!("row {:?} of kind {} has a NULL payload", row.path, row.kind.as_str())) };
    serde_json::from_str(payload).map_err(|err| format!("row {:?} payload is not JSON: {err}", row.path))
}

/// Rebuilds the JSON value from `rows` (any order). Strict: exactly one root (parent NULL,
/// path ""), `obj` payload NULL, `arr` children exactly "0".."n-1", `arr_id` children exactly
/// its payload's key texts, no duplicate seg under one parent, no unreachable row.
///
/// # Errors
/// A human-readable description of the first violation; the caller wraps it as
/// `DocStoreError::Malformed` with the file path.
pub(crate) fn join(rows: &[Row]) -> Result<Value, String> {
    let mut roots = rows.iter().filter(|row| row.parent.is_none());
    let root = match (roots.next(), roots.next()) {
        (Some(root), None) if root.path.is_empty() => root,
        (Some(root), None) => return Err(format!("the root row has path {:?}, expected \"\"", root.path)),
        (None, _) => return Err("no root row (parent NULL)".to_owned()),
        (Some(_), Some(_)) => return Err("more than one root row (parent NULL)".to_owned()),
    };
    let mut children: HashMap<&str, BTreeMap<&str, &Row>> = HashMap::new();
    for row in rows {
        let Some(parent) = row.parent.as_deref() else { continue };
        let Some(seg) = row.seg.as_deref() else { return Err(format!("row {:?} has a parent but a NULL seg", row.path)) };
        if children.entry(parent).or_default().insert(seg, row).is_some() {
            return Err(format!("row {parent:?} has two children with seg {seg:?}"));
        }
    }
    let mut visited = 0usize;
    let value = build(root, &children, &mut visited, 0)?;
    if visited != rows.len() {
        return Err(format!("{} row(s) are unreachable from the root", rows.len() - visited));
    }
    Ok(value)
}

/// Rebuilds the subtree rooted at `row`; counts every visited row in `visited`.
fn build(row: &Row, children: &HashMap<&str, BTreeMap<&str, &Row>>, visited: &mut usize, depth: usize) -> Result<Value, String> {
    if depth > MAX_DEPTH {
        return Err(format!("row {:?} nests deeper than {MAX_DEPTH} levels", row.path));
    }
    *visited += 1;
    let empty = BTreeMap::new();
    let kids = children.get(row.path.as_str()).unwrap_or(&empty);
    match row.kind {
        FragKind::Obj => {
            if row.payload.is_some() {
                return Err(format!("obj row {:?} has a non-NULL payload", row.path));
            }
            let mut map = serde_json::Map::new();
            for (seg, child) in kids {
                map.insert((*seg).to_owned(), build(child, children, visited, depth + 1)?);
            }
            Ok(Value::Object(map))
        }
        FragKind::Arr => {
            let count_text = row.payload.as_deref().unwrap_or_default();
            if count_text.is_empty() || !count_text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(format!("arr row {:?} has a bad count {:?}", row.path, row.payload));
            }
            let count: usize = count_text.parse().map_err(|_| format!("arr row {:?} count {count_text:?} is out of range", row.path))?;
            if count != kids.len() {
                return Err(format!("arr row {:?} declares {count} elements but has {} children", row.path, kids.len()));
            }
            let mut items = Vec::with_capacity(count);
            for index in 0..count {
                let Some(child) = kids.get(index.to_string().as_str()) else { return Err(format!("arr row {:?} children do not match 0..{count}", row.path)) };
                items.push(build(child, children, visited, depth + 1)?);
            }
            Ok(Value::Array(items))
        }
        FragKind::ArrId => {
            let Value::Array(order) = parse_payload(row)? else { return Err(format!("arr_id row {:?} payload is not an array", row.path)) };
            if order.len() != kids.len() {
                return Err(format!("arr_id row {:?} children do not match its key order", row.path));
            }
            let mut seen = std::collections::HashSet::with_capacity(order.len());
            let mut items = Vec::with_capacity(order.len());
            for key in &order {
                let text = compact(key);
                let Some(child) = kids.get(text.as_str()) else { return Err(format!("arr_id row {:?} children do not match its key order", row.path)) };
                if !seen.insert(text) {
                    return Err(format!("arr_id row {:?} lists a key twice", row.path));
                }
                items.push(build(child, children, visited, depth + 1)?);
            }
            Ok(Value::Array(items))
        }
        // A whole value; any child row stays unvisited and fails the orphan check.
        FragKind::Ent | FragKind::Leaf => parse_payload(row),
    }
}

/// The diff-write equality rule (clarification 7): a stored payload already represents the
/// new one when the texts are equal, or — for kinds whose payload is JSON (`leaf`, `ent`,
/// `arr_id`) — when both parse to type-strictly equal values (`serde_json::Value` equality
/// keeps int != float and bool != int; -0.0 == 0.0). This keeps either implementation from
/// rewriting a row only because the other one spells a float differently.
pub(crate) fn payload_unchanged(kind: FragKind, old: Option<&str>, new: Option<&str>) -> bool {
    if old == new {
        return true;
    }
    let (Some(old), Some(new)) = (old, new) else { return false };
    match kind {
        FragKind::Leaf | FragKind::Ent | FragKind::ArrId => match (serde_json::from_str::<Value>(old), serde_json::from_str::<Value>(new)) {
            (Ok(old), Ok(new)) => old == new,
            _ => false,
        },
        FragKind::Obj | FragKind::Arr => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn row(path: &str, parent: Option<&str>, seg: Option<&str>, kind: FragKind, payload: Option<&str>) -> Row {
        Row { path: path.into(), parent: parent.map(Into::into), seg: seg.map(Into::into), kind, payload: payload.map(Into::into) }
    }

    #[test]
    fn keys_are_escaped_in_path_and_raw_in_seg() {
        let rows = split(&json!({"a/b": 1, "t~x": 2, "": 3}));
        let paths: Vec<(&str, &str)> = rows.iter().skip(1).map(|row| (row.path.as_str(), row.seg.as_deref().unwrap_or_default())).collect();
        assert_eq!(paths, vec![("/", ""), ("/a~1b", "a/b"), ("/t~0x", "t~x")]);
    }

    #[test]
    fn id_rules_bool_float_and_mixed_keys_fall_back_to_positional_or_leaf() {
        assert_eq!(split(&json!([{"id": true}, {"id": false}]))[0].kind, FragKind::Arr);
        assert_eq!(split(&json!([{"id": 1.0}]))[0].kind, FragKind::Arr);
        assert_eq!(split(&json!([{"id": 1}, {"uid": 2}]))[0].kind, FragKind::Arr, "every element must carry the first element's key");
        assert_eq!(split(&json!([{"uid": "a", "id": 3}]))[0].kind, FragKind::ArrId, "id wins over uid");
        assert_eq!(split(&json!([{"id": 12}, {"id": "12"}]))[0].kind, FragKind::ArrId, "12 and \"12\" are distinct");
        assert_eq!(split(&json!([]))[0], row("", None, None, FragKind::Leaf, Some("[]")));
    }

    #[test]
    fn join_rejects_contract_violations() {
        let root = row("", None, None, FragKind::Obj, None);
        let cases: Vec<Vec<Row>> = vec![
            vec![],
            vec![root.clone(), row("/x", None, None, FragKind::Leaf, Some("1"))],
            vec![row("/", None, None, FragKind::Obj, None)],
            vec![row("", None, None, FragKind::Obj, Some("x"))],
            vec![root.clone(), row("/a", Some(""), Some("a"), FragKind::Leaf, Some("1")), row("/b", Some(""), Some("a"), FragKind::Leaf, Some("2"))],
            vec![root.clone(), row("/o", Some("/nowhere"), Some("o"), FragKind::Leaf, Some("1"))],
            vec![row("", None, None, FragKind::Arr, Some("2")), row("/0", Some(""), Some("0"), FragKind::Obj, None)],
            vec![row("", None, None, FragKind::Arr, Some("1")), row("/1", Some(""), Some("1"), FragKind::Obj, None)],
            vec![row("", None, None, FragKind::Arr, Some("x"))],
            vec![row("", None, None, FragKind::ArrId, Some("[1,1]")), row("/1", Some(""), Some("1"), FragKind::Ent, Some("{\"id\":1}"))],
            vec![row("", None, None, FragKind::ArrId, Some("[1]")), row("/2", Some(""), Some("2"), FragKind::Ent, Some("{\"id\":2}"))],
            vec![row("", None, None, FragKind::Leaf, None)],
            vec![row("", None, None, FragKind::Leaf, Some("{"))],
            vec![row("", None, None, FragKind::Leaf, Some("1")), row("/c", Some(""), Some("c"), FragKind::Leaf, Some("1"))],
            vec![root.clone(), row("/a", Some(""), None, FragKind::Leaf, Some("1"))],
        ];
        for (index, rows) in cases.iter().enumerate() {
            assert!(join(rows).is_err(), "case {index} must be malformed: {rows:?}");
        }
    }

    #[test]
    fn join_bounds_the_depth_of_a_hostile_parent_chain() {
        let mut rows = vec![row("", None, None, FragKind::Obj, None)];
        let mut parent = String::new();
        for _ in 0..(MAX_DEPTH + 10) {
            let path = format!("{parent}/k");
            rows.push(row(&path, Some(&parent), Some("k"), FragKind::Obj, None));
            parent = path;
        }
        assert!(join(&rows).expect_err("too deep").contains("deeper"));
    }

    #[test]
    fn payload_rule_is_text_or_type_strict_value_equality() {
        assert!(payload_unchanged(FragKind::Leaf, Some("1e-05"), Some("0.00001")));
        assert!(payload_unchanged(FragKind::Leaf, Some("-0.0"), Some("0.0")));
        assert!(!payload_unchanged(FragKind::Leaf, Some("1"), Some("1.0")));
        assert!(!payload_unchanged(FragKind::Leaf, Some("true"), Some("1")));
        assert!(payload_unchanged(FragKind::Ent, Some("{\"id\":1,\"x\":1e+16}"), Some("{\"id\":1,\"x\":10000000000000000.0}")));
        assert!(!payload_unchanged(FragKind::Arr, Some("02"), Some("2")), "counts compare as text");
        assert!(!payload_unchanged(FragKind::Leaf, None, Some("1")));
    }

    /// With `float_roundtrip`, every finite f64 survives compact text -> parse exactly (the
    /// losslessness contract of the codec). Deterministic pseudo-random bit patterns.
    #[test]
    fn floats_round_trip_exactly_through_compact_text() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..200_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let float = f64::from_bits(state);
            if !float.is_finite() {
                continue;
            }
            let value = json!(float);
            let back: Value = serde_json::from_str(&compact(&value)).expect("parse");
            assert_eq!(back, value, "bits {state:#x}");
        }
    }
}
