//! Field paths and document shape: resolving dotted paths, writing through
//! them, and validating what may be stored.

use crate::error::{Error, Result};
use crate::value::{Value, MAX_DOC_DEPTH};
use std::collections::BTreeMap;

/// Extended-JSON wrappers (`{"$oid": ..}`, `{"$date": ..}`, ...) are values,
/// not operators. They come from BSON over the MongoDB port or from the user.
pub fn is_literal_object(o: &BTreeMap<String, Value>) -> bool {
    o.len() == 1 && matches!(o.keys().next().map(String::as_str), Some("$oid" | "$date" | "$numberLong" | "$binary"))
}

/// A dotted field path must have non-empty segments, no `\0` and no
/// segment starting with `$` (positional operators are not supported).
pub fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Err(Error::Parse("empty field path".into()));
    }
    for seg in path.split('.') {
        if seg.is_empty() {
            return Err(Error::Parse(format!("field path '{path}' has an empty segment")));
        }
        if seg.starts_with('$') {
            return Err(Error::Parse(format!(
                "field path '{path}': segment '{seg}' starts with '$' (positional and operator paths are not supported)"
            )));
        }
        if seg.contains('\0') {
            return Err(Error::Parse(format!("field path '{path}' contains a NUL character")));
        }
    }
    Ok(())
}

/// Resolve a dotted path such as `address.city` or `tags.0` exactly: each
/// segment names an object field, or an array position when it is numeric.
pub fn get_path<'v>(doc: &'v Value, path: &str) -> Option<&'v Value> {
    let mut cur = doc;
    for part in path.split('.') {
        cur = match cur {
            Value::Object(o) => o.get(part)?,
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Every value a path reaches, for query matching and indexing. Like
/// [`get_path`], but a non-numeric segment applied to an array descends
/// into each object element, so `items.sku` reaches the `sku` of every
/// object in `items`. An empty result means the field is missing.
pub fn path_values<'v>(doc: &'v Value, path: &str) -> Vec<&'v Value> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut out = Vec::new();
    walk(doc, &parts, &mut out);
    out
}

fn walk<'v>(cur: &'v Value, parts: &[&str], out: &mut Vec<&'v Value>) {
    let Some((p, rest)) = parts.split_first() else {
        out.push(cur);
        return;
    };
    match cur {
        Value::Object(o) => {
            if let Some(v) = o.get(*p) {
                walk(v, rest, out);
            }
        }
        Value::Array(a) => match p.parse::<usize>() {
            Ok(i) => {
                if let Some(v) = a.get(i) {
                    walk(v, rest, out);
                }
            }
            Err(_) => {
                for e in a {
                    if let Value::Object(_) = e {
                        walk(e, parts, out);
                    }
                }
            }
        },
        _ => {}
    }
}

pub fn get_path_mut<'v>(doc: &'v mut Value, path: &str) -> Option<&'v mut Value> {
    let mut cur = doc;
    for part in path.split('.') {
        cur = match cur {
            Value::Object(o) => o.get_mut(part)?,
            Value::Array(a) => a.get_mut(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// How far past the end of an array `$set` may write (the gap is padded with nulls).
const MAX_ARRAY_PAD: usize = 10_000;

fn array_slot<'v>(a: &'v mut Vec<Value>, seg: &str, path: &str, fill: impl FnOnce() -> Value) -> Result<&'v mut Value> {
    let i = seg
        .parse::<usize>()
        .map_err(|_| Error::Type(format!("cannot use field name '{seg}' on an array in path '{path}'")))?;
    if i >= a.len() {
        if i - a.len() > MAX_ARRAY_PAD {
            return Err(Error::Type(format!("array index {i} in path '{path}' is too far past the end")));
        }
        a.resize(i, Value::Null);
        a.push(fill());
    }
    Ok(&mut a[i])
}

/// Write `v` at `path`, creating intermediate objects. Numeric segments
/// index arrays; writing past the end pads with nulls.
pub fn set_path(doc: &mut Value, path: &str, v: Value) -> Result<()> {
    let parts: Vec<&str> = path.split('.').collect();
    let (last, parents) = parts.split_last().expect("split always yields one part");
    let mut cur = doc;
    for p in parents {
        let empty = || Value::Object(BTreeMap::new());
        cur = match cur {
            Value::Object(o) => o.entry(p.to_string()).or_insert_with(empty),
            Value::Array(a) => array_slot(a, p, path, empty)?,
            other => {
                return Err(Error::Type(format!(
                    "cannot create field '{p}' in path '{path}': the parent is {}",
                    type_name(other)
                )))
            }
        };
    }
    match cur {
        Value::Object(o) => {
            o.insert(last.to_string(), v);
            Ok(())
        }
        Value::Array(a) => {
            *array_slot(a, last, path, || Value::Null)? = v;
            Ok(())
        }
        other => Err(Error::Type(format!("cannot set '{last}' in path '{path}': the parent is {}", type_name(other)))),
    }
}

/// Remove the field at `path`. An array element is set to null instead of
/// removed, so the positions of later elements do not shift. A missing
/// path is a no-op.
pub fn unset_path(doc: &mut Value, path: &str) {
    let (parent, last) = match path.rsplit_once('.') {
        Some((p, l)) => (get_path_mut(doc, p), l),
        None => (Some(doc), path),
    };
    match parent {
        Some(Value::Object(o)) => {
            o.remove(last);
        }
        Some(Value::Array(a)) => {
            if let Some(slot) = last.parse::<usize>().ok().and_then(|i| a.get_mut(i)) {
                *slot = Value::Null;
            }
        }
        _ => {}
    }
}

/// Lower-case type name used in error messages, `$type` and schemas.
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "double",
        Value::Text(_) => "string",
        Value::Array(_) => "array",
        Value::Object(o) if is_literal_object(o) => match o.keys().next().map(String::as_str) {
            Some("$oid") => "objectId",
            Some("$date") => "date",
            Some("$numberLong") => "long",
            _ => "binData",
        },
        Value::Object(_) => "object",
    }
}

/// Check that a document can be stored and read back unchanged: nesting
/// within the limit, finite numbers, and field names that a dotted path
/// can address (no `.`, no leading `$` outside extended-JSON wrappers, no NUL).
pub fn validate_doc(doc: &Value) -> Result<()> {
    if doc.deeper_than(MAX_DOC_DEPTH) {
        return Err(Error::Type(format!("document nesting is deeper than {MAX_DOC_DEPTH} levels")));
    }
    check_shape(doc, "")
}

fn check_shape(v: &Value, at: &str) -> Result<()> {
    let here = |k: &str| if at.is_empty() { k.to_string() } else { format!("{at}.{k}") };
    match v {
        Value::Float(f) if !f.is_finite() => {
            Err(Error::Type(format!("field '{at}' holds a non-finite number, which cannot be stored")))
        }
        Value::Array(a) => {
            for (i, e) in a.iter().enumerate() {
                check_shape(e, &here(&i.to_string()))?;
            }
            Ok(())
        }
        Value::Object(o) if is_literal_object(o) => Ok(()),
        Value::Object(o) => {
            for (k, e) in o {
                if k.starts_with('$') {
                    return Err(Error::Type(format!("field name '{}' may not start with '$'", here(k))));
                }
                if k.contains('.') {
                    return Err(Error::Type(format!("field name '{}' may not contain '.'", here(k))));
                }
                if k.contains('\0') {
                    return Err(Error::Type(format!("field name '{}' may not contain NUL", here(k))));
                }
                check_shape(e, &here(k))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
