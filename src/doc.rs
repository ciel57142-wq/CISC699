//! Document (NoSQL) model: schemaless JSON documents grouped in collections,
//! queried with MongoDB-style filters and update operators.

use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

fn check_name(coll: &str) -> Result<()> {
    if keys::valid_name(coll) {
        Ok(())
    } else {
        Err(Error::Schema(format!("invalid collection name '{coll}'")))
    }
}

/// A document deeper than the parser's limit could be stored but never read
/// back, so refuse it on the way in (it can arise from the typed API, from
/// BSON, or from an update with a long dotted path).
fn check_depth(doc: &Value) -> Result<()> {
    if doc.deeper_than(crate::value::MAX_DOC_DEPTH) {
        return Err(Error::Type(format!("document nesting is deeper than {} levels", crate::value::MAX_DOC_DEPTH)));
    }
    Ok(())
}

fn decode(raw: &[u8]) -> Result<Value> {
    let s = std::str::from_utf8(raw).map_err(|_| Error::Corrupt("document is not UTF-8".into()))?;
    Value::parse(s).map_err(|e| Error::Corrupt(format!("stored document: {e}")))
}

fn doc_key(coll: &str, id: &Value) -> Result<Vec<u8>> {
    let mut k = keys::doc_prefix(coll);
    k.extend(keys::encode_id(id)?);
    Ok(k)
}

fn owner(coll: &str) -> String {
    format!("c:{coll}")
}

/// Extended-JSON wrappers (`{"$oid": ..}`, `{"$date": ..}`) are values, not operators.
fn is_literal_object(o: &BTreeMap<String, Value>) -> bool {
    o.len() == 1 && matches!(o.keys().next().map(String::as_str), Some("$oid" | "$date" | "$numberLong"))
}

// ---------------------------------------------------------------- indexes

/// Indexed field paths of a collection.
pub fn indexes(ctx: &Ctx, coll: &str) -> Result<Vec<String>> {
    Ok(match ctx.get(&keys::coll_meta(coll)) {
        None => vec![],
        Some(raw) => match decode(&raw)?.get("indexes") {
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => vec![],
        },
    })
}

fn write_meta(ctx: &mut Ctx, coll: &str, fields: &[String]) {
    let mut o = BTreeMap::new();
    o.insert("indexes".to_string(), Value::Array(fields.iter().map(|f| Value::Text(f.clone())).collect()));
    ctx.put(keys::coll_meta(coll), Value::Object(o).to_json().into_bytes());
}

/// Index entries for one document. Array fields get one entry per scalar element.
fn index_keys(coll: &str, fields: &[String], key: &[u8], doc: &Value) -> Vec<Vec<u8>> {
    let id = &key[keys::doc_prefix(coll).len()..];
    let mut out = Vec::new();
    for f in fields {
        let mut add = |v: &Value| {
            if let Some(enc) = keys::encode_index_value(v) {
                let mut k = keys::index_prefix(&owner(coll), f);
                k.extend(enc);
                k.extend_from_slice(id);
                out.push(k);
            }
        };
        match get_path(doc, f) {
            None => {}
            Some(Value::Array(a)) => a.iter().for_each(&mut add),
            Some(v) => add(v),
        }
    }
    out
}

fn put_doc(ctx: &mut Ctx, coll: &str, fields: &[String], key: Vec<u8>, doc: &Value) {
    for k in index_keys(coll, fields, &key, doc) {
        ctx.put(k, Vec::new());
    }
    ctx.put(key, doc.to_json().into_bytes());
}

fn del_doc(ctx: &mut Ctx, coll: &str, fields: &[String], key: Vec<u8>, doc: &Value) {
    for k in index_keys(coll, fields, &key, doc) {
        ctx.delete(k);
    }
    ctx.delete(key);
}

/// Index a field path so equality filters on it stop scanning the collection.
/// Returns false if the index already existed.
pub fn create_index(ctx: &mut Ctx, coll: &str, field: &str) -> Result<bool> {
    check_name(coll)?;
    if field.is_empty() || field.contains('\0') || field == "_id" {
        return Err(Error::Schema(format!("cannot index field '{field}'")));
    }
    let mut fields = indexes(ctx, coll)?;
    if fields.iter().any(|f| f == field) {
        return Ok(false);
    }
    let one = [field.to_string()];
    for (key, raw) in ctx.scan(&keys::doc_prefix(coll)) {
        for k in index_keys(coll, &one, &key, &decode(&raw)?) {
            ctx.put(k, Vec::new());
        }
    }
    fields.push(field.to_string());
    write_meta(ctx, coll, &fields);
    Ok(true)
}

// ------------------------------------------------------------------- CRUD

/// Insert a document; returns its `_id` (generated when absent).
pub fn insert(ctx: &mut Ctx, coll: &str, doc: Value) -> Result<Value> {
    check_name(coll)?;
    check_depth(&doc)?;
    let Value::Object(mut obj) = doc else {
        return Err(Error::Type("a document must be a JSON object".into()));
    };
    if ctx.get(&keys::coll_meta(coll)).is_none() {
        write_meta(ctx, coll, &[]);
    }
    let id = match obj.get("_id") {
        Some(id) => id.clone(),
        None => {
            let n = keys::next_seq(ctx, &owner(coll))?;
            let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            let id = Value::Text(format!("{ms:012x}{n:08x}"));
            obj.insert("_id".into(), id.clone());
            id
        }
    };
    let key = doc_key(coll, &id)?;
    if ctx.get(&key).is_some() {
        return Err(Error::Constraint(format!("duplicate _id {} in collection '{coll}'", id.to_json())));
    }
    let fields = indexes(ctx, coll)?;
    put_doc(ctx, coll, &fields, key, &Value::Object(obj));
    Ok(id)
}

/// Documents that could match `filter`: a direct lookup when it pins `_id`,
/// an index lookup when it pins an indexed field, otherwise a full scan.
/// The caller still applies the filter.
fn candidates(ctx: &Ctx, coll: &str, filter: &Value) -> Result<Vec<(Vec<u8>, Value)>> {
    check_name(coll)?;
    if let Some(id) = filter.get("_id") {
        let literal = match id {
            Value::Int(_) | Value::Text(_) => true,
            Value::Object(o) => o.contains_key("$oid") && is_literal_object(o),
            _ => false,
        };
        if literal {
            let key = doc_key(coll, id)?;
            return Ok(match ctx.get(&key) {
                Some(raw) => vec![(key, decode(&raw)?)],
                None => vec![],
            });
        }
    }
    if let Value::Object(f) = filter {
        let fields = indexes(ctx, coll)?;
        for (field, cond) in f {
            if !fields.contains(field) {
                continue;
            }
            let operand = match cond {
                Value::Object(o) if o.len() == 1 && o.contains_key("$eq") => &o["$eq"],
                other => other,
            };
            let Some(enc) = keys::encode_index_value(operand) else {
                continue;
            };
            let mut prefix = keys::index_prefix(&owner(coll), field);
            prefix.extend(enc);
            let mut out = Vec::new();
            for (k, _) in ctx.scan(&prefix) {
                let mut dk = keys::doc_prefix(coll);
                dk.extend_from_slice(&k[prefix.len()..]);
                if let Some(raw) = ctx.get(&dk) {
                    out.push((dk, decode(&raw)?));
                }
            }
            out.sort_by(|a, b| a.0.cmp(&b.0));
            return Ok(out);
        }
    }
    ctx.scan(&keys::doc_prefix(coll)).into_iter().map(|(k, raw)| Ok((k, decode(&raw)?))).collect()
}

#[derive(Default)]
pub struct FindOptions {
    /// `{"field": 1}` ascending, `{"field": -1}` descending.
    pub sort: Option<Value>,
    pub skip: usize,
    pub limit: Option<usize>,
}

pub fn find(ctx: &Ctx, coll: &str, filter: &Value, opts: &FindOptions) -> Result<Vec<Value>> {
    let mut docs = Vec::new();
    for (_, d) in candidates(ctx, coll, filter)? {
        if matches(&d, filter)? {
            docs.push(d);
        }
    }
    if let Some(sort) = &opts.sort {
        let Value::Object(spec) = sort else {
            return Err(Error::Type("sort must be an object like {\"field\": 1}".into()));
        };
        let spec: Vec<(&String, bool)> =
            spec.iter().map(|(k, v)| (k, v.as_f64().map_or(false, |n| n < 0.0))).collect();
        docs.sort_by(|a, b| {
            for (path, desc) in &spec {
                let (x, y) = (get_path(a, path).unwrap_or(&Value::Null), get_path(b, path).unwrap_or(&Value::Null));
                let o = x.total_cmp(y);
                if o != Ordering::Equal {
                    return if *desc { o.reverse() } else { o };
                }
            }
            Ordering::Equal
        });
    }
    Ok(docs.into_iter().skip(opts.skip).take(opts.limit.unwrap_or(usize::MAX)).collect())
}

/// Update matching documents (all of them, or only the first when `multi`
/// is false). With `upsert`, a document is created when nothing matches.
/// Returns (documents updated, id of the upserted document).
pub fn update(
    ctx: &mut Ctx,
    coll: &str,
    filter: &Value,
    upd: &Value,
    multi: bool,
    upsert: bool,
) -> Result<(u64, Option<Value>)> {
    let fields = indexes(ctx, coll)?;
    let mut n = 0;
    for (key, old) in candidates(ctx, coll, filter)? {
        if !matches(&old, filter)? {
            continue;
        }
        let mut new = old.clone();
        apply_update(&mut new, upd, false)?;
        check_depth(&new)?;
        if new.get("_id") != old.get("_id") {
            return Err(Error::Constraint("_id cannot be changed".into()));
        }
        del_doc(ctx, coll, &fields, key.clone(), &old);
        put_doc(ctx, coll, &fields, key, &new);
        n += 1;
        if !multi {
            break;
        }
    }
    if n == 0 && upsert {
        // Seed the new document from the filter's plain equality fields.
        let mut base = Value::Object(BTreeMap::new());
        if let Value::Object(f) = filter {
            for (k, cond) in f {
                let plain = match cond {
                    Value::Object(o) => is_literal_object(o) || !o.keys().any(|k| k.starts_with('$')),
                    _ => true,
                };
                if !k.starts_with('$') && plain {
                    set_path(&mut base, k, cond.clone())?;
                }
            }
        }
        apply_update(&mut base, upd, true)?;
        return Ok((0, Some(insert(ctx, coll, base)?)));
    }
    Ok((n, None))
}

/// Delete matching documents, at most `limit` of them when given.
pub fn delete(ctx: &mut Ctx, coll: &str, filter: &Value, limit: Option<u64>) -> Result<u64> {
    let fields = indexes(ctx, coll)?;
    let mut n = 0;
    for (key, d) in candidates(ctx, coll, filter)? {
        if limit.map_or(false, |l| n >= l) {
            break;
        }
        if matches(&d, filter)? {
            del_doc(ctx, coll, &fields, key, &d);
            n += 1;
        }
    }
    Ok(n)
}

/// Remove a collection, its documents and its indexes. Returns whether it existed.
pub fn drop(ctx: &mut Ctx, coll: &str) -> Result<bool> {
    check_name(coll)?;
    let meta = keys::coll_meta(coll);
    let existed = ctx.get(&meta).is_some();
    for prefix in [keys::doc_prefix(coll), keys::index_owner_prefix(&owner(coll))] {
        for (k, _) in ctx.scan(&prefix) {
            ctx.delete(k);
        }
    }
    ctx.delete(meta);
    ctx.delete(keys::seq(&owner(coll)));
    Ok(existed)
}

pub fn collections(ctx: &Ctx) -> Vec<String> {
    ctx.scan(keys::COLL_META_PREFIX)
        .into_iter()
        .map(|(k, _)| String::from_utf8_lossy(&k[keys::COLL_META_PREFIX.len()..]).into_owned())
        .collect()
}

/// MongoDB-style projection on top-level fields: `{a: 1, b: 1}` keeps only
/// those (plus `_id`), `{a: 0}` removes them.
pub fn project(doc: Value, projection: &Value) -> Value {
    let (Value::Object(mut d), Value::Object(p)) = (doc.clone(), projection) else {
        return doc;
    };
    let on = |v: &Value| !matches!(v, Value::Bool(false) | Value::Int(0) | Value::Null) && v.as_f64() != Some(0.0);
    if p.is_empty() {
        return doc;
    }
    if p.iter().any(|(k, v)| k != "_id" && on(v)) {
        let keep_id = p.get("_id").map_or(true, on);
        d.retain(|k, _| (k == "_id" && keep_id) || (k != "_id" && p.get(k).map_or(false, on)));
    } else {
        d.retain(|k, _| !p.contains_key(k));
    }
    Value::Object(d)
}

// ---------------------------------------------------------------- filters

/// Resolve a dotted path such as `address.city` or `tags.0`.
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

/// Does `doc` satisfy a MongoDB-style `filter`?
///
/// Supported: field equality, dotted paths, `$eq $ne $gt $gte $lt $lte $in
/// $nin $exists $not`, and the logical `$and $or $nor`.
pub fn matches(doc: &Value, filter: &Value) -> Result<bool> {
    let obj = match filter {
        Value::Object(o) => o,
        Value::Null => return Ok(true),
        _ => return Err(Error::Type("a filter must be a JSON object".into())),
    };
    for (k, cond) in obj {
        let ok = match k.as_str() {
            "$and" | "$or" | "$nor" => {
                let Value::Array(subs) = cond else {
                    return Err(Error::Type(format!("{k} expects an array of filters")));
                };
                let mut all = true;
                let mut any = false;
                for s in subs {
                    let m = matches(doc, s)?;
                    all &= m;
                    any |= m;
                }
                match k.as_str() {
                    "$and" => all,
                    "$or" => any,
                    _ => !any,
                }
            }
            _ if k.starts_with('$') => return Err(Error::Parse(format!("unknown filter operator {k}"))),
            _ => field_matches(get_path(doc, k), cond)?,
        };
        if !ok {
            return Ok(false);
        }
    }
    Ok(true)
}

fn field_matches(v: Option<&Value>, cond: &Value) -> Result<bool> {
    if let Value::Object(o) = cond {
        if !o.is_empty() && !is_literal_object(o) && o.keys().all(|k| k.starts_with('$')) {
            for (op, arg) in o {
                if !op_matches(v, op, arg)? {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
    }
    Ok(eq_match(v, cond))
}

/// Equality; an array field also matches when any element equals the operand.
fn eq_match(v: Option<&Value>, c: &Value) -> bool {
    match v {
        None => c.is_null(),
        Some(v) => {
            v.total_cmp(c) == Ordering::Equal
                || matches!(v, Value::Array(a) if a.iter().any(|x| x.total_cmp(c) == Ordering::Equal))
        }
    }
}

fn op_matches(v: Option<&Value>, op: &str, arg: &Value) -> Result<bool> {
    Ok(match op {
        "$eq" => eq_match(v, arg),
        "$ne" => !eq_match(v, arg),
        "$gt" | "$gte" | "$lt" | "$lte" => match v {
            Some(v) if v.rank() == arg.rank() => {
                let o = v.total_cmp(arg);
                match op {
                    "$gt" => o == Ordering::Greater,
                    "$gte" => o != Ordering::Less,
                    "$lt" => o == Ordering::Less,
                    _ => o != Ordering::Greater,
                }
            }
            _ => false,
        },
        "$in" | "$nin" => {
            let Value::Array(items) = arg else {
                return Err(Error::Type(format!("{op} expects an array")));
            };
            items.iter().any(|a| eq_match(v, a)) == (op == "$in")
        }
        "$exists" => v.is_some() == !matches!(arg, Value::Bool(false) | Value::Null | Value::Int(0)),
        "$not" => !field_matches(v, arg)?,
        _ => return Err(Error::Parse(format!("unknown filter operator {op}"))),
    })
}

// ---------------------------------------------------------------- updates

fn set_path(doc: &mut Value, path: &str, v: Value) -> Result<()> {
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
fn apply_update(doc: &mut Value, upd: &Value, inserting: bool) -> Result<()> {
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
