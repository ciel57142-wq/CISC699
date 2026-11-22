//! Update operators: `$set $unset $inc $push $addToSet $pull $setOnInsert`
//! and whole-document replacement. They change a document in place.

use super::get_path;
use crate::error::{Error, Result};
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::BTreeMap;

pub fn set_path(doc: &mut Value, path: &str, v: Value) -> Result<()> {
    let parts: Vec<&str> = path.split('.').collect();
    let (last, parents) = parts.split_last().expect("split always yields one part");
    let mut cur = doc;
    for p in parents {
        cur = match cur {
            Value::Object(o) => o.entry(p.to_string()).or_insert_with(|| Value::Object(BTreeMap::new())),
            Value::Array(a) => p
                .parse::<usize>()
                .ok()
                .and_then(|i| a.get_mut(i))
                .ok_or_else(|| Error::Type(format!("no array element '{p}' in path '{path}'")))?,
            _ => return Err(Error::Type(format!("cannot descend into a scalar at '{p}' in path '{path}'"))),
        };
    }
    match cur {
        Value::Object(o) => {
            o.insert(last.to_string(), v);
            Ok(())
        }
        Value::Array(a) => match last.parse::<usize>() {
            Ok(i) if i < a.len() => {
                a[i] = v;
                Ok(())
            }
            Ok(i) if i == a.len() => {
                a.push(v);
                Ok(())
            }
            _ => Err(Error::Type(format!("bad array index '{last}' in path '{path}'"))),
        },
        _ => Err(Error::Type(format!("cannot set '{last}' on a scalar in path '{path}'"))),
    }
}

fn unset_path(doc: &mut Value, path: &str) {
    let (parent, last) = match path.rsplit_once('.') {
        Some((p, l)) => (Some(p), l),
        None => (None, path),
    };
    let target = match parent {
        None => Some(doc),
        Some(p) => get_path_mut(doc, p),
    };
    if let Some(Value::Object(o)) = target {
        o.remove(last);
    }
}

fn get_path_mut<'v>(doc: &'v mut Value, path: &str) -> Option<&'v mut Value> {
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

/// Apply `$set $unset $inc $push $addToSet $pull` (and `$setOnInsert` when
/// `inserting`); an update with no operators replaces the whole document,
/// keeping `_id`.
pub fn apply_update(doc: &mut Value, upd: &Value, inserting: bool) -> Result<()> {
    let Value::Object(ops) = upd else {
        return Err(Error::Type("an update must be a JSON object".into()));
    };
    if !ops.keys().any(|k| k.starts_with('$')) {
        let id = doc.get("_id").cloned();
        let mut new = ops.clone();
        if let Value::Object(old) = doc {
            if inserting {
                for (k, v) in old.iter() {
                    new.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
        if let Some(id) = id {
            new.entry("_id".into()).or_insert(id);
        }
        *doc = Value::Object(new);
        return Ok(());
    }
    for (op, fields) in ops {
        let Value::Object(fields) = fields else {
            return Err(Error::Type(format!("{op} expects an object of fields")));
        };
        for (path, v) in fields {
            match op.as_str() {
                "$set" => set_path(doc, path, v.clone())?,
                "$setOnInsert" => {
                    if inserting {
                        set_path(doc, path, v.clone())?
                    }
                }
                "$unset" => unset_path(doc, path),
                "$inc" => {
                    let cur = get_path(doc, path).cloned().unwrap_or(Value::Int(0));
                    let new = match (&cur, v) {
                        (Value::Int(a), Value::Int(b)) => {
                            Value::Int(a.checked_add(*b).ok_or_else(|| Error::Type("integer overflow".into()))?)
                        }
                        _ => match (cur.as_f64(), v.as_f64()) {
                            (Some(a), Some(b)) => Value::Float(a + b),
                            _ => return Err(Error::Type(format!("$inc needs numbers at '{path}'"))),
                        },
                    };
                    set_path(doc, path, new)?;
                }
                "$push" | "$addToSet" => match get_path_mut(doc, path) {
                    Some(Value::Array(a)) => {
                        if op == "$push" || !a.iter().any(|x| x.total_cmp(v) == Ordering::Equal) {
                            a.push(v.clone())
                        }
                    }
                    Some(_) => return Err(Error::Type(format!("{op} target '{path}' is not an array"))),
                    None => set_path(doc, path, Value::Array(vec![v.clone()]))?,
                },
                "$pull" => {
                    if let Some(Value::Array(a)) = get_path_mut(doc, path) {
                        a.retain(|x| x.total_cmp(v) != Ordering::Equal);
                    }
                }
                _ => return Err(Error::Parse(format!("unknown update operator {op}"))),
            }
        }
    }
    Ok(())
}
