//! Update documents: `$set $unset $inc $push $pull $addToSet` (and
//! `$setOnInsert` for upserts), or a plain replacement document.

use super::filter::ElementTest;
use super::path::{get_path, get_path_mut, set_path, type_name, unset_path, validate_path};
use crate::error::{Error, Result};
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// True if `upd` is a replacement document (no operator keys). Mixing
/// operators and plain fields is an error.
pub fn is_replacement(upd: &Value) -> Result<bool> {
    let Value::Object(ops) = upd else {
        return Err(Error::Type(format!("an update must be a JSON object, got {}", type_name(upd))));
    };
    let n = ops.keys().filter(|k| k.starts_with('$')).count();
    if n > 0 && n != ops.len() {
        return Err(Error::Parse("an update cannot mix operators ($set, ...) with plain fields".into()));
    }
    Ok(n == 0)
}

/// Two paths conflict if one is the other or lies inside it.
fn conflicts(a: &str, b: &str) -> bool {
    let inside = |x: &str, y: &str| x.len() > y.len() && x.starts_with(y) && x.as_bytes()[y.len()] == b'.';
    a == b || inside(a, b) || inside(b, a)
}

/// Values for `$push` / `$addToSet`: a single value, or `{$each: [...]}`.
fn each_values(op: &str, v: &Value) -> Result<Vec<Value>> {
    if let Value::Object(o) = v {
        if o.keys().any(|k| k.starts_with('$')) && !super::path::is_literal_object(o) {
            return match (o.len(), o.get("$each")) {
                (1, Some(Value::Array(items))) => Ok(items.clone()),
                (1, Some(_)) => Err(Error::Type(format!("{op} $each expects an array"))),
                _ => Err(Error::Parse(format!("{op} supports only the $each modifier"))),
            };
        }
    }
    Ok(vec![v.clone()])
}

fn add_numbers(a: &Value, b: &Value, path: &str) -> Result<Value> {
    Ok(match (a, b) {
        (Value::Int(x), Value::Int(y)) => {
            Value::Int(x.checked_add(*y).ok_or_else(|| Error::Type(format!("$inc overflows a 64-bit integer at '{path}'")))?)
        }
        _ => {
            let r = a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0);
            if !r.is_finite() {
                return Err(Error::Type(format!("$inc produced a non-finite number at '{path}'")));
            }
            Value::Float(r)
        }
    })
}

fn equal(a: &Value, b: &Value) -> bool {
    a.total_cmp(b) == Ordering::Equal
}

/// Apply an update to `doc` in place. A replacement keeps `_id` (and, when
/// `inserting`, the fields seeded from the filter). The caller checks that
/// `_id` did not change and that the result is a valid document.
pub fn apply(doc: &mut Value, upd: &Value, inserting: bool) -> Result<()> {
    if is_replacement(upd)? {
        let Value::Object(fields) = upd else { unreachable!("checked by is_replacement") };
        let mut new = fields.clone();
        if let Value::Object(old) = doc {
            if inserting {
                for (k, v) in old.iter() {
                    new.entry(k.clone()).or_insert_with(|| v.clone());
                }
            } else if let Some(id) = old.get("_id") {
                new.entry("_id".into()).or_insert_with(|| id.clone());
            }
        }
        *doc = Value::Object(new);
        return Ok(());
    }
    let Value::Object(ops) = upd else { unreachable!("checked by is_replacement") };
    if ops.is_empty() {
        return Ok(());
    }
    // Validate every operator and path before touching the document.
    let mut seen: Vec<&str> = Vec::new();
    for (op, fields) in ops {
        if !matches!(op.as_str(), "$set" | "$unset" | "$inc" | "$push" | "$addToSet" | "$pull" | "$setOnInsert") {
            return Err(Error::Parse(format!("unknown update operator {op}")));
        }
        let Value::Object(fields) = fields else {
            return Err(Error::Type(format!("{op} expects an object of fields")));
        };
        for path in fields.keys() {
            validate_path(path)?;
            if op != "$setOnInsert" || inserting {
                if let Some(other) = seen.iter().find(|p| conflicts(p, path)) {
                    return Err(Error::Parse(format!("update paths '{other}' and '{path}' conflict")));
                }
                seen.push(path);
            }
        }
    }
    for (op, fields) in ops {
        let Value::Object(fields) = fields else { unreachable!("validated above") };
        for (path, v) in fields {
            apply_one(doc, op, path, v, inserting)?;
        }
    }
    Ok(())
}

fn apply_one(doc: &mut Value, op: &str, path: &str, v: &Value, inserting: bool) -> Result<()> {
    match op {
        "$set" => set_path(doc, path, v.clone()),
        "$setOnInsert" if inserting => set_path(doc, path, v.clone()),
        "$setOnInsert" => Ok(()),
        "$unset" => {
            if path == "_id" {
                return Err(Error::Constraint("_id cannot be removed".into()));
            }
            unset_path(doc, path);
            Ok(())
        }
        "$inc" => {
            if !matches!(v, Value::Int(_) | Value::Float(_)) {
                return Err(Error::Type(format!("$inc at '{path}' needs a number, got {}", type_name(v))));
            }
            let new = match get_path(doc, path) {
                None => v.clone(),
                Some(cur @ (Value::Int(_) | Value::Float(_))) => add_numbers(cur, v, path)?,
                Some(cur) => {
                    return Err(Error::Type(format!("$inc at '{path}': the field holds {}, not a number", type_name(cur))))
                }
            };
            set_path(doc, path, new)
        }
        "$push" | "$addToSet" => {
            let items = each_values(op, v)?;
            if get_path(doc, path).is_none() {
                set_path(doc, path, Value::Array(vec![]))?;
            }
            match get_path_mut(doc, path) {
                Some(Value::Array(a)) => {
                    for it in items {
                        if op == "$push" || !a.iter().any(|x| equal(x, &it)) {
                            a.push(it);
                        }
                    }
                    Ok(())
                }
                Some(other) => Err(Error::Type(format!("{op} at '{path}': the field holds {}, not an array", type_name(other)))),
                None => Err(Error::Type(format!("{op} cannot create '{path}'"))),
            }
        }
        "$pull" => {
            let test = ElementTest::compile(v)?;
            match get_path_mut(doc, path) {
                None => Ok(()),
                Some(Value::Array(a)) => {
                    a.retain(|x| !test.test(x));
                    Ok(())
                }
                Some(other) => Err(Error::Type(format!("$pull at '{path}': the field holds {}, not an array", type_name(other)))),
            }
        }
        _ => Err(Error::Parse(format!("unknown update operator {op}"))),
    }
}

/// Check an update without a document to apply it to, so a malformed
/// update fails even when nothing matches.
pub fn validate(upd: &Value) -> Result<()> {
    apply(&mut Value::Object(BTreeMap::new()), upd, true)
}
