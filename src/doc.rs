//! Document (NoSQL) model: schemaless JSON documents grouped in collections,
//! queried with MongoDB-style filters and update operators.

use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// A collection name: `name` or `namespace.name` (a MongoDB database's collection).
fn check_name(coll: &str) -> Result<()> {
    if crate::query::plan::valid_collection(coll) {
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
        return Err(Error::Type(format!(
            "document nesting is deeper than {} levels",
            crate::value::MAX_DOC_DEPTH
        )));
    }
    Ok(())
}

pub fn decode(raw: &[u8]) -> Result<Value> {
    let s = std::str::from_utf8(raw).map_err(|_| Error::Corrupt("document is not UTF-8".into()))?;
    Value::parse(s).map_err(|e| Error::Corrupt(format!("stored document: {e}")))
}

pub fn doc_key(coll: &str, id: &Value) -> Result<Vec<u8>> {
    let mut k = keys::doc_prefix(coll);
    k.extend(keys::encode_id(id)?);
    Ok(k)
}

fn owner(coll: &str) -> String {
    format!("c:{coll}")
}

/// Extended-JSON wrappers (`{"$oid": ..}`, `{"$date": ..}`) are values, not operators.
fn is_literal_object(o: &BTreeMap<String, Value>) -> bool {
    o.len() == 1
        && matches!(
            o.keys().next().map(String::as_str),
            Some("$oid" | "$date" | "$numberLong")
        )
}

// ---------------------------------------------------------------- indexes

use crate::docindex;
pub use crate::docindex::DocIndex;

/// Indexes of a collection.
pub fn indexes(ctx: &Ctx, coll: &str) -> Result<Vec<DocIndex>> {
    docindex::load(ctx, coll)
}

fn put_doc(
    ctx: &mut Ctx,
    coll: &str,
    ixs: &mut [DocIndex],
    key: Vec<u8>,
    doc: &Value,
) -> Result<()> {
    if docindex::add(ctx, coll, ixs, &key, doc)? {
        docindex::save(ctx, coll, ixs);
    }
    ctx.put(key, doc.to_json().into_bytes());
    Ok(())
}

fn del_doc(ctx: &mut Ctx, coll: &str, ixs: &[DocIndex], key: Vec<u8>, doc: &Value) {
    docindex::remove(ctx, coll, ixs, &key, doc);
    ctx.delete(key);
}

/// Index a field path so filters on it stop scanning the collection.
/// Returns false if an index on that field already existed.
pub fn create_index(ctx: &mut Ctx, coll: &str, field: &str) -> Result<bool> {
    create_index_with(ctx, coll, vec![(field.to_string(), 1)], false, None)
}

/// Create a single-field or compound index, optionally unique.
pub fn create_index_with(
    ctx: &mut Ctx,
    coll: &str,
    fields: Vec<(String, i64)>,
    unique: bool,
    name: Option<String>,
) -> Result<bool> {
    check_name(coll)?;
    docindex::create(ctx, coll, fields, unique, name)
}

pub fn drop_index(ctx: &mut Ctx, coll: &str, name: &str) -> Result<bool> {
    check_name(coll)?;
    docindex::drop(ctx, coll, name)
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
        docindex::save(ctx, coll, &[]);
    }
    let id = match obj.get("_id") {
        Some(id) => id.clone(),
        None => {
            let n = keys::next_seq(ctx, &owner(coll))?;
            let ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let id = Value::Text(format!("{ms:012x}{n:08x}"));
            obj.insert("_id".into(), id.clone());
            id
        }
    };
    let key = doc_key(coll, &id)?;
    if ctx.get(&key).is_some() {
        return Err(Error::Constraint(format!(
            "duplicate _id {} in collection '{coll}'",
            id.to_json()
        )));
    }
    let mut ixs = indexes(ctx, coll)?;
    put_doc(ctx, coll, &mut ixs, key, &Value::Object(obj))?;
    Ok(id)
}

/// Equality operand of a filter condition on a field (`{f: v}` or `{f: {$eq: v}}`).
fn eq_operand(cond: &Value) -> Option<Value> {
    match cond {
        Value::Object(o) if o.len() == 1 && o.contains_key("$eq") => Some(o["$eq"].clone()),
        Value::Object(o)
            if !o.is_empty() && !is_literal_object(o) && o.keys().any(|k| k.starts_with('$')) =>
        {
            None
        }
        Value::Array(_) => None, // array equality is not an element match
        other => Some(other.clone()),
    }
}

/// Range operands of a filter condition (`$gt $gte $lt $lte`).
fn range_operands(cond: &Value) -> (Option<(Value, bool)>, Option<(Value, bool)>) {
    let (mut lo, mut hi) = (None, None);
    if let Value::Object(o) = cond {
        if is_literal_object(o) {
            return (None, None);
        }
        for (op, v) in o {
            if matches!(v, Value::Null | Value::Array(_) | Value::Object(_)) {
                continue;
            }
            match op.as_str() {
                "$gt" => lo = Some((v.clone(), false)),
                "$gte" => lo = Some((v.clone(), true)),
                "$lt" => hi = Some((v.clone(), false)),
                "$lte" => hi = Some((v.clone(), true)),
                _ => {}
            }
        }
    }
    (lo, hi)
}

/// How `find` reaches its candidate documents.
enum Access {
    Id(Value),
    Index(docindex::Choice<Value>),
    Scan,
}

fn plan_access(ixs: &[DocIndex], filter: &Value) -> Access {
    if let Some(id) = filter.get("_id") {
        let literal = match id {
            Value::Int(_) | Value::Text(_) => true,
            Value::Object(o) => o.contains_key("$oid") && is_literal_object(o),
            _ => false,
        };
        if literal {
            return Access::Id(id.clone());
        }
    }
    let Value::Object(f) = filter else {
        return Access::Scan;
    };
    let eq = |p: &str| f.get(p).and_then(eq_operand);
    let range = |p: &str| f.get(p).map_or((None, None), range_operands);
    // a legacy index has no entries for NULL/missing, so it cannot answer `f: null`
    let usable = |ix: &DocIndex, v: &Value| !(ix.legacy && v.is_null());
    match docindex::choose(ixs, &eq, &range, &usable) {
        Some(c) => Access::Index(c),
        None => Access::Scan,
    }
}

/// Documents that could match `filter`, in key order unless an index decides
/// otherwise. The caller still applies the filter.
fn candidates(ctx: &Ctx, coll: &str, filter: &Value) -> Result<Vec<(Vec<u8>, Value)>> {
    check_name(coll)?;
    let ixs = indexes(ctx, coll)?;
    match plan_access(&ixs, filter) {
        Access::Id(id) => {
            let key = doc_key(coll, &id)?;
            Ok(match ctx.get(&key) {
                Some(raw) => vec![(key, decode(&raw)?)],
                None => vec![],
            })
        }
        Access::Index(c) => index_docs(ctx, coll, &c, false, true),
        Access::Scan => ctx
            .scan(&keys::doc_prefix(coll))
            .into_iter()
            .map(|(k, raw)| Ok((k, decode(&raw)?)))
            .collect(),
    }
}

/// Documents reached through an index range, deduplicated (multikey);
/// `key_order` re-sorts them by document key.
fn index_docs(
    ctx: &Ctx,
    coll: &str,
    c: &docindex::Choice<Value>,
    rev: bool,
    key_order: bool,
) -> Result<Vec<(Vec<u8>, Value)>> {
    let Some((lo, hi)) = docindex::key_range(coll, c) else {
        return ctx
            .scan(&keys::doc_prefix(coll))
            .into_iter()
            .map(|(k, raw)| Ok((k, decode(&raw)?)))
            .collect();
    };
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (e, _) in ctx.iter_range(lo, hi, rev) {
        let dk = docindex::doc_key_of(coll, &c.index, &e);
        if seen.insert(dk.clone()) {
            if let Some(raw) = ctx.get(&dk) {
                out.push((dk, decode(&raw)?));
            }
        }
    }
    if key_order {
        out.sort_by(|a, b| a.0.cmp(&b.0));
    }
    Ok(out)
}

/// An index that returns documents already in `sort` order: not multikey,
/// covers NULL/missing (so no document is skipped), and its leading fields
/// after the equality prefix match the sort fields with one consistent direction.
fn sort_index(
    ixs: &[DocIndex],
    access: &Access,
    sort: &[(String, bool)],
) -> Option<(docindex::Choice<Value>, bool)> {
    let fits = |ix: &DocIndex, skip: usize| -> Option<bool> {
        if ix.legacy || ix.multikey || ix.fields.len() < skip + sort.len() {
            return None;
        }
        let mut rev = None;
        for ((p, d), (sp, desc)) in ix.fields[skip..].iter().zip(sort) {
            if p != sp {
                return None;
            }
            // entries are stored ascending whatever the declared direction
            let _ = d;
            let r = *desc;
            if rev.is_some_and(|x| x != r) {
                return None;
            }
            rev = Some(r);
        }
        rev
    };
    match access {
        Access::Index(c) if c.lo.is_none() && c.hi.is_none() => {
            fits(&c.index, c.eq.len()).map(|r| (c.clone(), r))
        }
        Access::Index(c) if c.eq.is_empty() => fits(&c.index, 0).map(|r| (c.clone(), r)),
        Access::Scan => ixs.iter().find_map(|ix| {
            fits(ix, 0).map(|r| {
                (
                    docindex::Choice {
                        index: ix.clone(),
                        eq: vec![],
                        lo: None,
                        hi: None,
                    },
                    r,
                )
            })
        }),
        _ => None,
    }
}

#[derive(Default)]
pub struct FindOptions {
    /// `{"field": 1}` ascending, `{"field": -1}` descending.
    pub sort: Option<Value>,
    pub skip: usize,
    pub limit: Option<usize>,
}

/// A sort specification as (path, descending) in priority order. Accepts
/// `{field: 1, ...}` (keys in name order, since objects are stored sorted) or
/// the order-preserving `[[field, 1], [field2, -1]]`.
pub fn sort_spec(v: &Value) -> Result<Vec<(String, bool)>> {
    let desc = |d: &Value| d.as_f64().is_some_and(|n| n < 0.0);
    match v {
        Value::Object(spec) => Ok(spec.iter().map(|(k, d)| (k.clone(), desc(d))).collect()),
        Value::Array(pairs) => pairs
            .iter()
            .map(|p| match p {
                Value::Array(kv) if kv.len() == 2 => match &kv[0] {
                    Value::Text(k) => Ok((k.clone(), desc(&kv[1]))),
                    _ => Err(Error::Type("sort field names must be strings".into())),
                },
                _ => Err(Error::Type(
                    "sort must be {field: 1} or [[field, 1], ...]".into(),
                )),
            })
            .collect(),
        _ => Err(Error::Type(
            "sort must be an object like {\"field\": 1}".into(),
        )),
    }
}

pub fn find(ctx: &Ctx, coll: &str, filter: &Value, opts: &FindOptions) -> Result<Vec<Value>> {
    let spec: Vec<(String, bool)> = match &opts.sort {
        None => vec![],
        Some(v) => sort_spec(v)?,
    };
    check_name(coll)?;
    if !spec.is_empty() {
        let ixs = indexes(ctx, coll)?;
        let access = plan_access(&ixs, filter);
        if let Some((choice, rev)) = sort_index(&ixs, &access, &spec) {
            // the index yields documents in sort order: stop as soon as skip + limit match
            let want = opts.limit.map(|l| l.saturating_add(opts.skip));
            let mut out = Vec::new();
            let Some((lo, hi)) = docindex::key_range(coll, &choice) else {
                return find_sorted_in_memory(ctx, coll, filter, opts, &spec);
            };
            for (e, _) in ctx.iter_range(lo, hi, rev) {
                if want.is_some_and(|w| out.len() >= w) {
                    break;
                }
                let dk = docindex::doc_key_of(coll, &choice.index, &e);
                if let Some(raw) = ctx.get(&dk) {
                    let d = decode(&raw)?;
                    if matches(&d, filter)? {
                        out.push(d);
                    }
                }
            }
            return Ok(out.into_iter().skip(opts.skip).collect());
        }
    }
    find_sorted_in_memory(ctx, coll, filter, opts, &spec)
}

fn find_sorted_in_memory(
    ctx: &Ctx,
    coll: &str,
    filter: &Value,
    opts: &FindOptions,
    spec: &[(String, bool)],
) -> Result<Vec<Value>> {
    let mut docs = Vec::new();
    let want = opts
        .limit
        .map(|l| l.saturating_add(opts.skip))
        .filter(|_| spec.is_empty());
    for (_, d) in candidates(ctx, coll, filter)? {
        if want.is_some_and(|w| docs.len() >= w) {
            break;
        }
        if matches(&d, filter)? {
            docs.push(d);
        }
    }
    if !spec.is_empty() {
        docs.sort_by(|a, b| {
            for (path, desc) in spec {
                let (x, y) = (
                    get_path(a, path).unwrap_or(&Value::Null),
                    get_path(b, path).unwrap_or(&Value::Null),
                );
                let o = x.total_cmp(y);
                if o != Ordering::Equal {
                    return if *desc { o.reverse() } else { o };
                }
            }
            Ordering::Equal
        });
    }
    Ok(docs
        .into_iter()
        .skip(opts.skip)
        .take(opts.limit.unwrap_or(usize::MAX))
        .collect())
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
    let mut ixs = indexes(ctx, coll)?;
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
        del_doc(ctx, coll, &ixs, key.clone(), &old);
        put_doc(ctx, coll, &mut ixs, key, &new)?;
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
                    Value::Object(o) => {
                        is_literal_object(o) || !o.keys().any(|k| k.starts_with('$'))
                    }
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

/// Replace a stored document (found by key), keeping indexes in step.
pub fn replace(ctx: &mut Ctx, coll: &str, key: Vec<u8>, old: &Value, new: Value) -> Result<()> {
    check_depth(&new)?;
    if new.get("_id") != old.get("_id") {
        return Err(Error::Constraint("_id cannot be changed".into()));
    }
    let mut ixs = indexes(ctx, coll)?;
    del_doc(ctx, coll, &ixs, key.clone(), old);
    put_doc(ctx, coll, &mut ixs, key, &new)
}

/// Delete a stored document (found by key) and its index entries.
pub fn remove(ctx: &mut Ctx, coll: &str, key: Vec<u8>, old: &Value) -> Result<()> {
    let fields = indexes(ctx, coll)?;
    del_doc(ctx, coll, &fields, key, old);
    Ok(())
}

/// Delete matching documents, at most `limit` of them when given.
pub fn delete(ctx: &mut Ctx, coll: &str, filter: &Value, limit: Option<u64>) -> Result<u64> {
    let fields = indexes(ctx, coll)?;
    let mut n = 0;
    for (key, d) in candidates(ctx, coll, filter)? {
        if limit.is_some_and(|l| n >= l) {
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
    for prefix in [
        keys::doc_prefix(coll),
        keys::index_owner_prefix(&owner(coll)),
    ] {
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
    let on = |v: &Value| {
        !matches!(v, Value::Bool(false) | Value::Int(0) | Value::Null) && v.as_f64() != Some(0.0)
    };
    if p.is_empty() {
        return doc;
    }
    if p.iter().any(|(k, v)| k != "_id" && on(v)) {
        let keep_id = p.get("_id").is_none_or(on);
        d.retain(|k, _| (k == "_id" && keep_id) || (k != "_id" && p.get(k).is_some_and(on)));
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
            _ if k.starts_with('$') => {
                return Err(Error::Parse(format!("unknown filter operator {k}")))
            }
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
        "$exists" => {
            v.is_some() == !matches!(arg, Value::Bool(false) | Value::Null | Value::Int(0))
        }
        "$not" => !field_matches(v, arg)?,
        _ => return Err(Error::Parse(format!("unknown filter operator {op}"))),
    })
}

// ---------------------------------------------------------------- updates

pub fn set_path(doc: &mut Value, path: &str, v: Value) -> Result<()> {
    let parts: Vec<&str> = path.split('.').collect();
    let (last, parents) = parts.split_last().expect("split always yields one part");
    let mut cur = doc;
    for p in parents {
        cur = match cur {
            Value::Object(o) => o
                .entry(p.to_string())
                .or_insert_with(|| Value::Object(BTreeMap::new())),
            Value::Array(a) => p
                .parse::<usize>()
                .ok()
                .and_then(|i| a.get_mut(i))
                .ok_or_else(|| Error::Type(format!("no array element '{p}' in path '{path}'")))?,
            _ => {
                return Err(Error::Type(format!(
                    "cannot descend into a scalar at '{p}' in path '{path}'"
                )))
            }
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
            _ => Err(Error::Type(format!(
                "bad array index '{last}' in path '{path}'"
            ))),
        },
        _ => Err(Error::Type(format!(
            "cannot set '{last}' on a scalar in path '{path}'"
        ))),
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
                        (Value::Int(a), Value::Int(b)) => Value::Int(
                            a.checked_add(*b)
                                .ok_or_else(|| Error::Type("integer overflow".into()))?,
                        ),
                        _ => match (cur.as_f64(), v.as_f64()) {
                            (Some(a), Some(b)) => Value::Float(a + b),
                            _ => {
                                return Err(Error::Type(format!("$inc needs numbers at '{path}'")))
                            }
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
                    Some(_) => {
                        return Err(Error::Type(format!("{op} target '{path}' is not an array")))
                    }
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
