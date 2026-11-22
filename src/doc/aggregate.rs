//! Projection, sorting and the aggregation pipeline.
//!
//! Pipeline stages: `$match $project $group $sort $skip $limit $count
//! $unwind`. `$group` accumulators: `$sum $avg $min $max $count $first
//! $last $push $addToSet`. Expressions are field references (`"$a.b"`),
//! `{$literal: v}`, and objects / arrays built from those; other
//! expression operators are rejected rather than ignored.

use super::filter::compile;
use super::path::{get_path, set_path, validate_path};
use crate::error::{Error, Result};
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::BTreeMap;

// ------------------------------------------------------------- expressions

/// Evaluate an expression against a document; `None` means "missing".
pub fn eval_expr(doc: &Value, e: &Value) -> Result<Option<Value>> {
    Ok(match e {
        Value::Text(s) if s.starts_with("$$") => {
            return Err(Error::Parse(format!("variables such as '{s}' are not supported")))
        }
        Value::Text(s) if s.starts_with('$') => {
            let path = &s[1..];
            validate_path(path)?;
            let parts: Vec<&str> = path.split('.').collect();
            ref_path(doc, &parts)
        }
        Value::Object(o) if o.keys().any(|k| k.starts_with('$')) => {
            if o.len() == 1 {
                if let Some(v) = o.get("$literal") {
                    return Ok(Some(v.clone()));
                }
            }
            if super::path::is_literal_object(o) {
                return Ok(Some(e.clone()));
            }
            let op = o.keys().find(|k| k.starts_with('$')).cloned().unwrap_or_default();
            return Err(Error::Parse(format!("expression operator {op} is not supported (use field references or $literal)")));
        }
        Value::Object(o) => {
            let mut out = BTreeMap::new();
            for (k, v) in o {
                if let Some(x) = eval_expr(doc, v)? {
                    out.insert(k.clone(), x);
                }
            }
            Some(Value::Object(out))
        }
        Value::Array(a) => {
            Some(Value::Array(a.iter().map(|v| Ok(eval_expr(doc, v)?.unwrap_or(Value::Null))).collect::<Result<_>>()?))
        }
        other => Some(other.clone()),
    })
}

/// A field reference: through an array it yields the array of the values
/// found in its elements.
fn ref_path(cur: &Value, parts: &[&str]) -> Option<Value> {
    let Some((p, rest)) = parts.split_first() else {
        return Some(cur.clone());
    };
    match cur {
        Value::Object(o) => ref_path(o.get(*p)?, rest),
        Value::Array(a) => Some(Value::Array(a.iter().filter_map(|e| ref_path(e, parts)).collect())),
        _ => None,
    }
}

// --------------------------------------------------------------- sorting

/// A sort specification: `{"a": 1, "b": -1}` (keys apply in name order,
/// since objects do not keep field order) or, to choose the order,
/// `[["b", -1], ["a", 1]]` / `["b", "a"]`. Returns (path, descending).
pub fn parse_sort(spec: &Value) -> Result<Vec<(String, bool)>> {
    let dir = |v: &Value| match v.as_f64() {
        Some(n) if n == 1.0 => Ok(false),
        Some(n) if n == -1.0 => Ok(true),
        _ => Err(Error::Type(format!("sort direction must be 1 or -1, got {}", v.to_json()))),
    };
    let keys: Vec<(String, bool)> = match spec {
        Value::Object(o) => o.iter().map(|(k, v)| Ok((k.clone(), dir(v)?))).collect::<Result<_>>()?,
        Value::Array(items) => items
            .iter()
            .map(|it| match it {
                Value::Text(k) => Ok((k.clone(), false)),
                Value::Array(pair) if pair.len() == 2 => match &pair[0] {
                    Value::Text(k) => Ok((k.clone(), dir(&pair[1])?)),
                    _ => Err(Error::Type("sort pairs look like [\"field\", 1]".into())),
                },
                _ => Err(Error::Type("sort list items are \"field\" or [\"field\", 1|-1]".into())),
            })
            .collect::<Result<_>>()?,
        _ => return Err(Error::Type("sort must be an object like {\"field\": 1} or a list of [field, dir] pairs".into())),
    };
    for (k, _) in &keys {
        validate_path(k)?;
    }
    Ok(keys)
}

/// Stable sort; a missing field sorts as null. Values of different types
/// order null < bool < number < string < array < object.
pub fn sort_docs(docs: &mut [Value], spec: &[(String, bool)]) {
    docs.sort_by(|a, b| {
        for (path, desc) in spec {
            let x = get_path(a, path).unwrap_or(&Value::Null);
            let y = get_path(b, path).unwrap_or(&Value::Null);
            let o = x.total_cmp(y);
            if o != Ordering::Equal {
                return if *desc { o.reverse() } else { o };
            }
        }
        Ordering::Equal
    });
}

// ------------------------------------------------------------- projection

enum Proj {
    Include,
    Exclude,
    Computed(Value),
}

/// Apply a projection. `{a: 1, "b.c": 1}` keeps only those paths (plus
/// `_id` unless `_id: 0`), `{a: 0}` removes paths, and a field reference
/// or `{$literal: v}` computes a new field. Inclusion and exclusion cannot
/// be mixed, except for `_id`. An empty projection returns the document.
pub fn project(doc: Value, projection: &Value) -> Result<Value> {
    let Value::Object(spec) = projection else {
        return Err(Error::Type("a projection must be an object".into()));
    };
    if spec.is_empty() {
        return Ok(doc);
    }
    let mut items = Vec::new();
    for (path, v) in spec {
        validate_path(path)?;
        let p = match v {
            Value::Bool(b) => if *b { Proj::Include } else { Proj::Exclude },
            Value::Int(_) | Value::Float(_) => if v.as_f64() == Some(0.0) { Proj::Exclude } else { Proj::Include },
            other => Proj::Computed(other.clone()),
        };
        items.push((path.as_str(), p));
    }
    let excluding = items.iter().any(|(p, x)| *p != "_id" && matches!(x, Proj::Exclude));
    let including = items.iter().any(|(p, x)| *p != "_id" && !matches!(x, Proj::Exclude));
    if excluding && including {
        return Err(Error::Parse("a projection cannot mix inclusion and exclusion (except for _id)".into()));
    }
    if !including {
        let mut d = doc;
        for (path, _) in &items {
            remove_path(&mut d, &path.split('.').collect::<Vec<_>>());
        }
        return Ok(d);
    }
    let mut out = Value::Object(BTreeMap::new());
    let keep_id = !items.iter().any(|(p, x)| *p == "_id" && matches!(x, Proj::Exclude));
    if keep_id {
        if let Some(id) = doc.get("_id") {
            set_path(&mut out, "_id", id.clone())?;
        }
    }
    for (path, p) in &items {
        match p {
            Proj::Include => include_path(&doc, &path.split('.').collect::<Vec<_>>(), &mut out),
            Proj::Computed(e) => {
                if let Some(v) = eval_expr(&doc, e)? {
                    set_path(&mut out, path, v)?;
                }
            }
            Proj::Exclude => {}
        }
    }
    Ok(out)
}

/// Copy `src`'s value at `parts` into `dst`, keeping its nesting. Through
/// an array, the path applies to each object element.
fn include_path(src: &Value, parts: &[&str], dst: &mut Value) {
    let (Value::Object(s), Value::Object(d)) = (src, &mut *dst) else { return };
    let Some((p, rest)) = parts.split_first() else { return };
    let Some(child) = s.get(*p) else { return };
    if rest.is_empty() {
        d.insert(p.to_string(), child.clone());
        return;
    }
    match child {
        Value::Object(_) => {
            let slot = d.entry(p.to_string()).or_insert_with(|| Value::Object(BTreeMap::new()));
            include_path(child, rest, slot);
        }
        Value::Array(a) => {
            let objs: Vec<&Value> = a.iter().filter(|e| matches!(e, Value::Object(_))).collect();
            let slot = d.entry(p.to_string()).or_insert_with(|| {
                Value::Array(objs.iter().map(|_| Value::Object(BTreeMap::new())).collect())
            });
            if let Value::Array(out) = slot {
                for (e, o) in objs.into_iter().zip(out.iter_mut()) {
                    include_path(e, rest, o);
                }
            }
        }
        _ => {}
    }
}

fn remove_path(v: &mut Value, parts: &[&str]) {
    let Some((p, rest)) = parts.split_first() else { return };
    match v {
        Value::Object(o) => {
            if rest.is_empty() {
                o.remove(*p);
            } else if let Some(c) = o.get_mut(*p) {
                remove_path(c, rest);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|e| remove_path(e, parts)),
        _ => {}
    }
}

// ----------------------------------------------------------------- $group

/// Values compare as in queries (so 1 and 1.0 land in one group).
#[derive(Clone)]
struct Key(Value);
impl PartialEq for Key {
    fn eq(&self, o: &Self) -> bool {
        self.0.total_cmp(&o.0) == Ordering::Equal
    }
}
impl Eq for Key {}
impl PartialOrd for Key {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Key {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0)
    }
}

enum Acc {
    Sum(Value),
    Avg(f64, u64),
    Min(Option<Value>),
    Max(Option<Value>),
    Count(i64),
    First(Option<Value>),
    Last(Value),
    Push(Vec<Value>),
    AddToSet(Vec<Value>),
}

fn new_acc(op: &str) -> Result<Acc> {
    Ok(match op {
        "$sum" => Acc::Sum(Value::Int(0)),
        "$avg" => Acc::Avg(0.0, 0),
        "$min" => Acc::Min(None),
        "$max" => Acc::Max(None),
        "$count" => Acc::Count(0),
        "$first" => Acc::First(None),
        "$last" => Acc::Last(Value::Null),
        "$push" => Acc::Push(vec![]),
        "$addToSet" => Acc::AddToSet(vec![]),
        other => return Err(Error::Parse(format!("unsupported $group accumulator {other}"))),
    })
}

fn add(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => match x.checked_add(*y) {
            Some(s) => Value::Int(s),
            None => Value::Float(*x as f64 + *y as f64),
        },
        _ => Value::Float(a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0)),
    }
}

impl Acc {
    fn feed(&mut self, v: Option<Value>) {
        let num = |v: &Option<Value>| matches!(v, Some(Value::Int(_) | Value::Float(_)));
        match self {
            Acc::Sum(s) => {
                if num(&v) {
                    *s = add(s, v.as_ref().expect("checked"));
                }
            }
            Acc::Avg(s, n) => {
                if let Some(x) = v.as_ref().and_then(Value::as_f64) {
                    *s += x;
                    *n += 1;
                }
            }
            Acc::Min(m) => {
                if let Some(x) = v.filter(|x| !x.is_null()) {
                    if m.as_ref().map_or(true, |c| x.total_cmp(c) == Ordering::Less) {
                        *m = Some(x);
                    }
                }
            }
            Acc::Max(m) => {
                if let Some(x) = v.filter(|x| !x.is_null()) {
                    if m.as_ref().map_or(true, |c| x.total_cmp(c) == Ordering::Greater) {
                        *m = Some(x);
                    }
                }
            }
            Acc::Count(n) => *n += 1,
            Acc::First(f) => {
                if f.is_none() {
                    *f = Some(v.unwrap_or(Value::Null));
                }
            }
            Acc::Last(l) => *l = v.unwrap_or(Value::Null),
            Acc::Push(items) => {
                if let Some(x) = v {
                    items.push(x)
                }
            }
            Acc::AddToSet(items) => {
                if let Some(x) = v {
                    if !items.iter().any(|y| y.total_cmp(&x) == Ordering::Equal) {
                        items.push(x)
                    }
                }
            }
        }
    }

    fn finish(self) -> Value {
        match self {
            Acc::Sum(s) => s,
            Acc::Avg(_, 0) => Value::Null,
            Acc::Avg(s, n) => Value::Float(s / n as f64),
            Acc::Min(m) | Acc::Max(m) => m.unwrap_or(Value::Null),
            Acc::Count(n) => Value::Int(n),
            Acc::First(f) => f.unwrap_or(Value::Null),
            Acc::Last(l) => l,
            Acc::Push(v) | Acc::AddToSet(v) => Value::Array(v),
        }
    }
}

/// `{$group: {_id: <expr>, field: {<accumulator>: <expr>}, ...}}`. Output
/// documents are ordered by group key.
fn group(docs: Vec<Value>, spec: &Value) -> Result<Vec<Value>> {
    let Value::Object(spec) = spec else {
        return Err(Error::Type("$group expects an object".into()));
    };
    let key_expr = spec.get("_id").ok_or_else(|| Error::Parse("$group needs an _id (use null for one group)".into()))?;
    let mut accs: Vec<(&String, &String, &Value)> = Vec::new();
    for (field, a) in spec.iter().filter(|(k, _)| *k != "_id") {
        if field.contains('.') || field.starts_with('$') {
            return Err(Error::Parse(format!("$group output field '{field}' may not contain '.' or start with '$'")));
        }
        match a {
            Value::Object(o) if o.len() == 1 => {
                let (op, arg) = o.iter().next().expect("one entry");
                new_acc(op)?;
                if op == "$count" && !matches!(arg, Value::Object(x) if x.is_empty()) {
                    return Err(Error::Type("$count takes an empty object: {$count: {}}".into()));
                }
                accs.push((field, op, arg));
            }
            _ => return Err(Error::Parse(format!("$group field '{field}' needs one accumulator, e.g. {{$sum: 1}}"))),
        }
    }
    let mut groups: BTreeMap<Key, Vec<Acc>> = BTreeMap::new();
    for d in &docs {
        let key = eval_expr(d, key_expr)?.unwrap_or(Value::Null);
        let states = match groups.entry(Key(key)) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(accs.iter().map(|(_, op, _)| new_acc(op)).collect::<Result<_>>()?)
            }
        };
        for (state, (_, _, arg)) in states.iter_mut().zip(&accs) {
            state.feed(eval_expr(d, arg)?);
        }
    }
    Ok(groups
        .into_iter()
        .map(|(k, states)| {
            let mut o = BTreeMap::new();
            o.insert("_id".to_string(), k.0);
            for (state, (field, _, _)) in states.into_iter().zip(&accs) {
                o.insert((*field).clone(), state.finish());
            }
            Value::Object(o)
        })
        .collect())
}

// --------------------------------------------------------------- pipeline

fn count_arg(stage: &str, v: &Value, allow_zero: bool) -> Result<usize> {
    let n = match v {
        Value::Int(n) => *n as f64,
        Value::Float(f) if f.fract() == 0.0 => *f,
        _ => -1.0,
    };
    if n < 0.0 || (!allow_zero && n == 0.0) {
        return Err(Error::Type(format!(
            "{stage} expects a {} integer",
            if allow_zero { "non-negative" } else { "positive" }
        )));
    }
    Ok(n as usize)
}

fn unwind(docs: Vec<Value>, arg: &Value) -> Result<Vec<Value>> {
    let (path, preserve) = match arg {
        Value::Text(s) => (s.as_str(), false),
        Value::Object(o) => (
            o.get("path").and_then(Value::as_str).ok_or_else(|| Error::Parse("$unwind needs a path".into()))?,
            o.get("preserveNullAndEmptyArrays") == Some(&Value::Bool(true)),
        ),
        _ => return Err(Error::Type("$unwind expects \"$field\" or {path: \"$field\"}".into())),
    };
    let path = path.strip_prefix('$').ok_or_else(|| Error::Parse("$unwind path must start with '$'".into()))?;
    validate_path(path)?;
    let mut out = Vec::new();
    for d in docs {
        match get_path(&d, path).cloned() {
            Some(Value::Array(items)) if !items.is_empty() => {
                for it in items {
                    let mut c = d.clone();
                    set_path(&mut c, path, it)?;
                    out.push(c);
                }
            }
            Some(Value::Array(_)) | Some(Value::Null) | None => {
                if preserve {
                    out.push(d);
                }
            }
            Some(_) => out.push(d),
        }
    }
    Ok(out)
}

/// Run pipeline stages over `docs`. The caller may already have applied
/// a leading `$match` (through an index) and passes the remaining stages.
pub fn run(mut docs: Vec<Value>, stages: &[Value]) -> Result<Vec<Value>> {
    for stage in stages {
        let (name, arg) = stage_parts(stage)?;
        docs = match name {
            "$match" => {
                let f = compile(arg)?;
                docs.into_iter().filter(|d| f.eval(d)).collect()
            }
            "$project" => {
                if matches!(arg, Value::Object(o) if o.is_empty()) {
                    return Err(Error::Parse("$project needs at least one field".into()));
                }
                docs.into_iter().map(|d| project(d, arg)).collect::<Result<_>>()?
            }
            "$group" => group(docs, arg)?,
            "$sort" => {
                let spec = parse_sort(arg)?;
                if spec.is_empty() {
                    return Err(Error::Parse("$sort needs at least one field".into()));
                }
                sort_docs(&mut docs, &spec);
                docs
            }
            "$skip" => docs.into_iter().skip(count_arg("$skip", arg, true)?).collect(),
            "$limit" => {
                docs.truncate(count_arg("$limit", arg, false)?);
                docs
            }
            "$count" => {
                let field = match arg {
                    Value::Text(s) if !s.is_empty() && !s.starts_with('$') && !s.contains('.') => s.clone(),
                    _ => return Err(Error::Type("$count expects a field name".into())),
                };
                if docs.is_empty() {
                    vec![]
                } else {
                    let mut o = BTreeMap::new();
                    o.insert(field, Value::Int(docs.len() as i64));
                    vec![Value::Object(o)]
                }
            }
            "$unwind" => unwind(docs, arg)?,
            other => return Err(Error::Parse(format!("unsupported aggregation stage {other}"))),
        };
    }
    Ok(docs)
}

pub fn stage_parts(stage: &Value) -> Result<(&str, &Value)> {
    match stage {
        Value::Object(o) if o.len() == 1 => {
            let (k, v) = o.iter().next().expect("one entry");
            Ok((k.as_str(), v))
        }
        _ => Err(Error::Parse("each pipeline stage must be an object with exactly one stage name".into())),
    }
}
