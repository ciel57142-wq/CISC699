//! MongoDB-style aggregation pipelines over stored documents.
//!
//! Stages: `$match $project $addFields/$set $unset $sort $limit $skip
//! $group $unwind $count $lookup $replaceRoot`. Accumulators: `$sum $avg
//! $min $max $count $first $last $push $addToSet`. Expressions: field paths
//! (`"$a.b"`), literals, objects/arrays of expressions and the operators
//! `$add $subtract $multiply $divide $mod $concat $toUpper $toLower $ifNull
//! $cond $eq $ne $gt $gte $lt $lte $and $or $not $size $literal $arrayElemAt`.
//!
//! A leading `$match` (and `$sort`/`$limit` right after it) runs through the
//! collection's indexes like `find`; later stages work on the result.

use crate::doc::{self, FindOptions};
use crate::error::{Error, Result};
use crate::storage::Ctx;
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};

fn obj(pairs: Vec<(String, Value)>) -> Value {
    Value::Object(pairs.into_iter().collect())
}

fn perr<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Parse(m.into()))
}

/// Run a pipeline against a collection.
pub fn aggregate(ctx: &Ctx, coll: &str, pipeline: &[Value]) -> Result<Vec<Value>> {
    let empty = Value::Object(BTreeMap::new());
    let mut stages = pipeline.iter().peekable();
    // Leading $match [+ $sort] [+ $skip] [+ $limit] goes through find(), which uses indexes.
    let mut filter = &empty;
    let mut opts = FindOptions::default();
    if let Some((name, arg)) = stages.peek().and_then(|s| stage(s).ok()) {
        if name == "$match" {
            filter = arg;
            stages.next();
            let mut limit_seen = false;
            while let Some((name, arg)) = stages.peek().and_then(|s| stage(s).ok()) {
                match name {
                    "$sort" if opts.sort.is_none() && opts.skip == 0 && !limit_seen => {
                        opts.sort = Some(arg.clone())
                    }
                    "$skip" if !limit_seen => opts.skip += count(arg)?,
                    "$limit" if !limit_seen => {
                        opts.limit = Some(count(arg)?);
                        limit_seen = true;
                    }
                    _ => break,
                }
                stages.next();
            }
        }
    }
    let mut docs = doc::find(ctx, coll, filter, &opts)?;
    for s in stages {
        let (name, arg) = stage(s)?;
        docs = run_stage(ctx, name, arg, docs)?;
    }
    Ok(docs)
}

fn stage(s: &Value) -> Result<(&str, &Value)> {
    match s {
        Value::Object(o) if o.len() == 1 => {
            let (k, v) = o.iter().next().expect("one entry");
            Ok((k.as_str(), v))
        }
        _ => perr("each pipeline stage must be an object with exactly one $stage field"),
    }
}

fn count(v: &Value) -> Result<usize> {
    match v.as_f64() {
        Some(n) if n >= 0.0 && n.fract() == 0.0 => Ok(n as usize),
        _ => Err(Error::Type(format!(
            "expected a non-negative integer, got {}",
            v.to_json()
        ))),
    }
}

fn sort_docs(docs: &mut [Value], spec: &Value) -> Result<()> {
    let Value::Object(spec) = spec else {
        return Err(Error::Type(
            "$sort expects an object like {field: 1}".into(),
        ));
    };
    let keys: Vec<(&String, bool)> = spec
        .iter()
        .map(|(k, v)| (k, v.as_f64().is_some_and(|n| n < 0.0)))
        .collect();
    docs.sort_by(|a, b| {
        for (path, desc) in &keys {
            let (x, y) = (
                doc::get_path(a, path).unwrap_or(&Value::Null),
                doc::get_path(b, path).unwrap_or(&Value::Null),
            );
            let o = x.total_cmp(y);
            if o != Ordering::Equal {
                return if *desc { o.reverse() } else { o };
            }
        }
        Ordering::Equal
    });
    Ok(())
}

fn run_stage(ctx: &Ctx, name: &str, arg: &Value, mut docs: Vec<Value>) -> Result<Vec<Value>> {
    Ok(match name {
        "$match" => {
            let mut kept = Vec::new();
            for d in docs {
                if doc::matches(&d, arg)? {
                    kept.push(d);
                }
            }
            kept
        }
        "$sort" => {
            sort_docs(&mut docs, arg)?;
            docs
        }
        "$skip" => docs.into_iter().skip(count(arg)?).collect(),
        "$limit" => {
            docs.truncate(count(arg)?);
            docs
        }
        "$count" => {
            let field = arg
                .as_str()
                .filter(|f| !f.is_empty() && !f.starts_with('$'))
                .ok_or_else(|| Error::Type("$count takes a field name".into()))?;
            if docs.is_empty() {
                vec![]
            } else {
                vec![obj(vec![(
                    field.to_string(),
                    Value::Int(docs.len() as i64),
                )])]
            }
        }
        "$project" => docs
            .into_iter()
            .map(|d| project(&d, arg))
            .collect::<Result<_>>()?,
        "$addFields" | "$set" => {
            let Value::Object(fields) = arg else {
                return Err(Error::Type(format!("{name} expects an object")));
            };
            let mut out = Vec::with_capacity(docs.len());
            for mut d in docs {
                for (path, e) in fields {
                    let v = expr(&d, e)?;
                    doc::set_path(&mut d, path, v.unwrap_or(Value::Null))?;
                }
                out.push(d);
            }
            out
        }
        "$unset" => {
            let fields: Vec<String> = match arg {
                Value::Text(s) => vec![s.clone()],
                Value::Array(a) => a
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect(),
                _ => {
                    return Err(Error::Type(
                        "$unset takes a field name or an array of names".into(),
                    ))
                }
            };
            let spec = obj(fields.into_iter().map(|f| (f, Value::Int(0))).collect());
            docs.into_iter()
                .map(|d| project(&d, &spec))
                .collect::<Result<_>>()?
        }
        "$replaceRoot" | "$replaceWith" => {
            let e = if name == "$replaceRoot" {
                arg.get("newRoot")
                    .ok_or_else(|| Error::Parse("$replaceRoot needs newRoot".into()))?
            } else {
                arg
            };
            let mut out = Vec::new();
            for d in docs {
                match expr(&d, e)? {
                    Some(v @ Value::Object(_)) => out.push(v),
                    other => {
                        return Err(Error::Type(format!(
                            "{name} must produce a document, got {}",
                            other.unwrap_or(Value::Null).to_json()
                        )))
                    }
                }
            }
            out
        }
        "$unwind" => {
            let (path, keep_empty, index_field) = match arg {
                Value::Text(p) => (p.clone(), false, None),
                Value::Object(o) => (
                    o.get("path")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    o.get("preserveNullAndEmptyArrays") == Some(&Value::Bool(true)),
                    o.get("includeArrayIndex")
                        .and_then(Value::as_str)
                        .map(String::from),
                ),
                _ => {
                    return Err(Error::Type(
                        "$unwind takes \"$path\" or {path: \"$path\"}".into(),
                    ))
                }
            };
            let Some(field) = path.strip_prefix('$').filter(|f| !f.is_empty()) else {
                return Err(Error::Type("$unwind path must start with '$'".into()));
            };
            let mut out = Vec::new();
            for d in docs {
                match doc::get_path(&d, field).cloned() {
                    Some(Value::Array(items)) if !items.is_empty() => {
                        for (i, item) in items.into_iter().enumerate() {
                            let mut n = d.clone();
                            doc::set_path(&mut n, field, item)?;
                            if let Some(ix) = &index_field {
                                doc::set_path(&mut n, ix, Value::Int(i as i64))?;
                            }
                            out.push(n);
                        }
                    }
                    Some(Value::Array(_)) | None | Some(Value::Null) => {
                        if keep_empty {
                            let mut n = d.clone();
                            if let Some(ix) = &index_field {
                                doc::set_path(&mut n, ix, Value::Null)?;
                            }
                            out.push(n);
                        }
                    }
                    Some(_) => {
                        // a non-array value is treated as a one-element array
                        let mut n = d.clone();
                        if let Some(ix) = &index_field {
                            doc::set_path(&mut n, ix, Value::Null)?;
                        }
                        out.push(n);
                    }
                }
            }
            out
        }
        "$group" => group(docs, arg)?,
        "$lookup" => {
            let s = |k: &str| arg.get(k).and_then(Value::as_str).map(String::from);
            let (Some(from), Some(local), Some(foreign), Some(as_)) =
                (s("from"), s("localField"), s("foreignField"), s("as"))
            else {
                return perr("$lookup needs from, localField, foreignField and as");
            };
            let empty = Value::Object(BTreeMap::new());
            // index the foreign collection once (it may itself use an index for the filter)
            let foreign_docs = doc::find(ctx, &from, &empty, &FindOptions::default())?;
            let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, fd) in foreign_docs.iter().enumerate() {
                let vals = match doc::get_path(fd, &foreign) {
                    Some(Value::Array(a)) => a.clone(),
                    Some(v) => vec![v.clone()],
                    None => vec![Value::Null],
                };
                for v in vals {
                    by_key.entry(key_of(&v)).or_default().push(i);
                }
            }
            let mut out = Vec::with_capacity(docs.len());
            for mut d in docs {
                let locals = match doc::get_path(&d, &local) {
                    Some(Value::Array(a)) => a.clone(),
                    Some(v) => vec![v.clone()],
                    None => vec![Value::Null],
                };
                let mut hits: Vec<usize> = locals
                    .iter()
                    .flat_map(|v| by_key.get(&key_of(v)).cloned().unwrap_or_default())
                    .collect();
                hits.sort_unstable();
                hits.dedup();
                let joined =
                    Value::Array(hits.into_iter().map(|i| foreign_docs[i].clone()).collect());
                doc::set_path(&mut d, &as_, joined)?;
                out.push(d);
            }
            out
        }
        other => return perr(format!("unsupported aggregation stage {other}")),
    })
}

/// Hash key with numeric normalisation (1 and 1.0 group together).
fn key_of(v: &Value) -> String {
    match v {
        Value::Float(f) if f.fract() == 0.0 && f.abs() < 9e15 => Value::Int(*f as i64).to_json(),
        other => other.to_json(),
    }
}

fn truthy_spec(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Int(_) | Value::Float(_) => Some(v.as_f64() != Some(0.0)),
        _ => None,
    }
}

fn project(d: &Value, spec: &Value) -> Result<Value> {
    let Value::Object(spec) = spec else {
        return Err(Error::Type("$project expects an object".into()));
    };
    let inclusion = spec
        .iter()
        .any(|(k, v)| k != "_id" && truthy_spec(v) != Some(false));
    if !inclusion {
        // exclusion: copy and remove
        let mut out = d.clone();
        for (k, v) in spec {
            if truthy_spec(v) == Some(false) {
                remove_path(&mut out, k);
            }
        }
        return Ok(out);
    }
    let mut out = Value::Object(BTreeMap::new());
    if spec.get("_id").and_then(truthy_spec) != Some(false) {
        if let Some(id) = d.get("_id") {
            doc::set_path(&mut out, "_id", id.clone())?;
        }
    }
    for (k, v) in spec {
        if k == "_id" && truthy_spec(v).is_some() {
            continue;
        }
        match truthy_spec(v) {
            Some(true) => {
                if let Some(x) = doc::get_path(d, k) {
                    doc::set_path(&mut out, k, x.clone())?;
                }
            }
            Some(false) => return perr(format!("cannot exclude '{k}' in an inclusion projection")),
            None => {
                if let Some(x) = expr(d, v)? {
                    doc::set_path(&mut out, k, x)?;
                }
            }
        }
    }
    Ok(out)
}

fn remove_path(d: &mut Value, path: &str) {
    let (parent, last) = match path.rsplit_once('.') {
        Some((p, l)) => (Some(p), l),
        None => (None, path),
    };
    let mut cur = d;
    if let Some(p) = parent {
        for part in p.split('.') {
            cur = match cur {
                Value::Object(o) => match o.get_mut(part) {
                    Some(x) => x,
                    None => return,
                },
                _ => return,
            };
        }
    }
    if let Value::Object(o) = cur {
        o.remove(last);
    }
}

#[derive(Debug)]
enum Acc {
    Sum(Value),
    Avg(f64, u64),
    Min(Option<Value>),
    Max(Option<Value>),
    Count(i64),
    First(Option<Value>),
    Last(Value),
    Push(Vec<Value>),
    Set(Vec<Value>, std::collections::HashSet<String>),
}

fn add_num(a: &Value, b: &Value) -> Value {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x
            .checked_add(*y)
            .map(Value::Int)
            .unwrap_or(Value::Float(*x as f64 + *y as f64)),
        _ => Value::Float(a.as_f64().unwrap_or(0.0) + b.as_f64().unwrap_or(0.0)),
    }
}

fn group(docs: Vec<Value>, arg: &Value) -> Result<Vec<Value>> {
    let Value::Object(spec) = arg else {
        return Err(Error::Type("$group expects an object".into()));
    };
    let id_expr = spec.get("_id").ok_or_else(|| {
        Error::Parse("$group needs an _id expression (null for one group)".into())
    })?;
    let mut accs: Vec<(&String, &str, &Value)> = Vec::new();
    for (field, a) in spec.iter().filter(|(k, _)| *k != "_id") {
        match a {
            Value::Object(o) if o.len() == 1 => {
                let (op, e) = o.iter().next().expect("one entry");
                if !matches!(
                    op.as_str(),
                    "$sum"
                        | "$avg"
                        | "$min"
                        | "$max"
                        | "$count"
                        | "$first"
                        | "$last"
                        | "$push"
                        | "$addToSet"
                ) {
                    return perr(format!("unknown $group accumulator {op}"));
                }
                accs.push((field, op.as_str(), e));
            }
            _ => {
                return perr(format!(
                    "$group field '{field}' must be {{$accumulator: expression}}"
                ))
            }
        }
    }
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<(Value, Vec<Acc>)> = Vec::new();
    for d in &docs {
        let id = expr(d, id_expr)?.unwrap_or(Value::Null);
        let k = key_of(&id);
        let gi = match index.get(&k) {
            Some(i) => *i,
            None => {
                let fresh = accs
                    .iter()
                    .map(|(_, op, _)| match *op {
                        "$sum" => Acc::Sum(Value::Int(0)),
                        "$avg" => Acc::Avg(0.0, 0),
                        "$min" => Acc::Min(None),
                        "$max" => Acc::Max(None),
                        "$count" => Acc::Count(0),
                        "$first" => Acc::First(None),
                        "$last" => Acc::Last(Value::Null),
                        "$push" => Acc::Push(Vec::new()),
                        _ => Acc::Set(Vec::new(), Default::default()),
                    })
                    .collect();
                groups.push((id, fresh));
                index.insert(k, groups.len() - 1);
                groups.len() - 1
            }
        };
        for ((_, _, e), acc) in accs.iter().zip(groups[gi].1.iter_mut()) {
            let v = if matches!(acc, Acc::Count(_)) {
                None
            } else {
                expr(d, e)?
            };
            match acc {
                Acc::Sum(total) => {
                    if let Some(x) = v.filter(|x| matches!(x, Value::Int(_) | Value::Float(_))) {
                        *total = add_num(total, &x);
                    }
                }
                Acc::Avg(sum, n) => {
                    if let Some(x) = v.and_then(|x| x.as_f64()) {
                        *sum += x;
                        *n += 1;
                    }
                }
                Acc::Min(m) => extreme(m, v, Ordering::Less),
                Acc::Max(m) => extreme(m, v, Ordering::Greater),
                Acc::Count(n) => *n += 1,
                Acc::First(f) => {
                    if f.is_none() {
                        *f = Some(v.unwrap_or(Value::Null));
                    }
                }
                Acc::Last(l) => *l = v.unwrap_or(Value::Null),
                Acc::Push(items) => {
                    if let Some(x) = v {
                        items.push(x);
                    }
                }
                Acc::Set(items, seen) => {
                    if let Some(x) = v {
                        if seen.insert(key_of(&x)) {
                            items.push(x);
                        }
                    }
                }
            }
        }
    }
    Ok(groups
        .into_iter()
        .map(|(id, accs_v)| {
            let mut o = BTreeMap::new();
            o.insert("_id".to_string(), id);
            for ((field, _, _), acc) in accs.iter().zip(accs_v) {
                let v = match acc {
                    Acc::Sum(v) => v,
                    Acc::Avg(_, 0) => Value::Null,
                    Acc::Avg(s, n) => Value::Float(s / n as f64),
                    Acc::Min(m) | Acc::Max(m) => m.unwrap_or(Value::Null),
                    Acc::Count(n) => Value::Int(n),
                    Acc::First(f) => f.unwrap_or(Value::Null),
                    Acc::Last(l) => l,
                    Acc::Push(items) | Acc::Set(items, _) => Value::Array(items),
                };
                o.insert((*field).clone(), v);
            }
            Value::Object(o)
        })
        .collect())
}

fn extreme(m: &mut Option<Value>, v: Option<Value>, want: Ordering) {
    if let Some(x) = v.filter(|x| !x.is_null()) {
        if m.as_ref().map_or(true, |cur| x.total_cmp(cur) == want) {
            *m = Some(x);
        }
    }
}

/// Evaluate an aggregation expression; `None` means "missing" (not the same as null).
pub fn expr(d: &Value, e: &Value) -> Result<Option<Value>> {
    Ok(match e {
        Value::Text(s) if s.starts_with("$$ROOT") => Some(d.clone()),
        Value::Text(s) if s.starts_with('$') && s.len() > 1 => doc::get_path(d, &s[1..]).cloned(),
        Value::Array(a) => Some(Value::Array(
            a.iter()
                .map(|x| expr(d, x).map(|v| v.unwrap_or(Value::Null)))
                .collect::<Result<_>>()?,
        )),
        Value::Object(o)
            if o.len() == 1
                && o.keys().next().is_some_and(|k| k.starts_with('$'))
                && !matches!(
                    o.keys().next().map(String::as_str),
                    Some("$oid" | "$date" | "$numberLong")
                ) =>
        {
            let (op, a) = o.iter().next().expect("one entry");
            Some(operator(d, op, a)?)
        }
        Value::Object(o) => {
            let mut out = BTreeMap::new();
            for (k, v) in o {
                if let Some(x) = expr(d, v)? {
                    out.insert(k.clone(), x);
                }
            }
            Some(Value::Object(out))
        }
        other => Some(other.clone()),
    })
}

fn args(d: &Value, a: &Value) -> Result<Vec<Value>> {
    match a {
        Value::Array(items) => items
            .iter()
            .map(|x| expr(d, x).map(|v| v.unwrap_or(Value::Null)))
            .collect(),
        other => Ok(vec![expr(d, other)?.unwrap_or(Value::Null)]),
    }
}

fn operator(d: &Value, op: &str, a: &Value) -> Result<Value> {
    if op == "$literal" {
        return Ok(a.clone());
    }
    let v = args(d, a)?;
    let two = || -> Result<(&Value, &Value)> {
        match v.as_slice() {
            [x, y] => Ok((x, y)),
            _ => perr(format!("{op} takes two arguments")),
        }
    };
    let truthy =
        |x: &Value| !matches!(x, Value::Null | Value::Bool(false)) && x.as_f64() != Some(0.0);
    Ok(match op {
        "$add" | "$multiply" => {
            if v.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            let mut acc = if op == "$add" {
                Value::Int(0)
            } else {
                Value::Int(1)
            };
            for x in &v {
                let o = if op == "$add" {
                    crate::pbsql::ast::Op::Add
                } else {
                    crate::pbsql::ast::Op::Mul
                };
                acc = crate::query::eval::arith(o, &acc, x)?;
            }
            acc
        }
        "$subtract" | "$divide" | "$mod" => {
            let (x, y) = two()?;
            let o = match op {
                "$subtract" => crate::pbsql::ast::Op::Sub,
                "$divide" => {
                    return Ok(match (x.as_f64(), y.as_f64()) {
                        (_, Some(z)) if z == 0.0 => {
                            return Err(Error::Type("$divide by zero".into()))
                        }
                        (Some(p), Some(q)) => Value::Float(p / q),
                        _ => Value::Null,
                    })
                }
                _ => crate::pbsql::ast::Op::Mod,
            };
            crate::query::eval::arith(o, x, y)?
        }
        "$concat" => {
            if v.iter().any(Value::is_null) {
                Value::Null
            } else {
                Value::Text(v.iter().map(Value::display).collect())
            }
        }
        "$toUpper" | "$toLower" => match v.first() {
            Some(Value::Text(s)) => Value::Text(if op == "$toUpper" {
                s.to_uppercase()
            } else {
                s.to_lowercase()
            }),
            _ => Value::Text(String::new()),
        },
        "$ifNull" => v
            .iter()
            .find(|x| !x.is_null())
            .cloned()
            .unwrap_or(Value::Null),
        "$cond" => {
            let (c, t, f) = match a {
                Value::Object(o) => (
                    expr(d, o.get("if").unwrap_or(&Value::Null))?.unwrap_or(Value::Null),
                    o.get("then").unwrap_or(&Value::Null),
                    o.get("else").unwrap_or(&Value::Null),
                ),
                Value::Array(items) if items.len() == 3 => (
                    expr(d, &items[0])?.unwrap_or(Value::Null),
                    &items[1],
                    &items[2],
                ),
                _ => return perr("$cond takes {if, then, else} or [if, then, else]"),
            };
            expr(d, if truthy(&c) { t } else { f })?.unwrap_or(Value::Null)
        }
        "$eq" | "$ne" | "$gt" | "$gte" | "$lt" | "$lte" => {
            let (x, y) = two()?;
            let o = x.total_cmp(y);
            Value::Bool(match op {
                "$eq" => o == Ordering::Equal,
                "$ne" => o != Ordering::Equal,
                "$gt" => o == Ordering::Greater,
                "$gte" => o != Ordering::Less,
                "$lt" => o == Ordering::Less,
                _ => o != Ordering::Greater,
            })
        }
        "$and" => Value::Bool(v.iter().all(truthy)),
        "$or" => Value::Bool(v.iter().any(truthy)),
        "$not" => Value::Bool(!v.first().is_some_and(truthy)),
        "$size" => match v.first() {
            Some(Value::Array(x)) => Value::Int(x.len() as i64),
            _ => return Err(Error::Type("$size needs an array".into())),
        },
        "$arrayElemAt" => {
            let (arr, idx) = two()?;
            match (arr, idx.as_i64()) {
                (Value::Array(x), Some(i)) => {
                    let i = if i < 0 { x.len() as i64 + i } else { i };
                    x.get(i as usize).cloned().unwrap_or(Value::Null)
                }
                _ => Value::Null,
            }
        }
        other => return perr(format!("unsupported aggregation operator {other}")),
    })
}
