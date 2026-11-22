//! Document (NoSQL) model: schemaless JSON documents grouped in
//! collections, queried with MongoDB-style filters, update operators and
//! an aggregation pipeline.
//!
//! Every function here works on a [`Ctx`], so each call runs inside the
//! caller's statement or transaction: a failing call leaves no partial
//! writes, and committed changes go through the write-ahead log.
//!
//! * [`path`] — dotted field paths and document validation
//! * [`filter`] — query filter compiler and evaluator
//! * [`update`] — update operators
//! * [`index`] — secondary indexes and the access-path planner
//! * [`aggregate`] — projection, sorting and the aggregation pipeline

pub mod aggregate;
pub mod filter;
pub mod index;
pub mod path;
pub mod update;

pub use aggregate::project;
pub use filter::matches;
pub use path::get_path;

use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use filter::Filter;
use index::{IndexDef, Plan};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

fn check_name(coll: &str) -> Result<()> {
    if keys::valid_name(coll) {
        Ok(())
    } else {
        Err(Error::Schema(format!("invalid collection name '{coll}'")))
    }
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

fn obj(fields: Vec<(&str, Value)>) -> Value {
    Value::Object(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

/// Prefix an error message with context, keeping its kind.
fn context(e: Error, what: &str) -> Error {
    match e {
        Error::Parse(m) => Error::Parse(format!("{what}: {m}")),
        Error::Schema(m) => Error::Schema(format!("{what}: {m}")),
        Error::Constraint(m) => Error::Constraint(format!("{what}: {m}")),
        Error::Type(m) => Error::Type(format!("{what}: {m}")),
        other => other,
    }
}

// ------------------------------------------------------------- metadata

/// Per-collection metadata, stored as JSON under `m\0c\0<coll>`.
#[derive(Default, Clone)]
struct Meta {
    indexes: Vec<IndexDef>,
    schema: Option<Schema>,
}

fn load_meta(ctx: &Ctx, coll: &str) -> Result<Option<Meta>> {
    let Some(raw) = ctx.get(&keys::coll_meta(coll)) else { return Ok(None) };
    let v = decode(&raw)?;
    let indexes = match v.get("indexes") {
        Some(Value::Array(a)) => a.iter().map(IndexDef::from_meta).collect::<Result<_>>()?,
        _ => vec![],
    };
    let schema = match v.get("schema") {
        Some(s) => Some(Schema::parse(s).map_err(|e| Error::Corrupt(format!("stored schema of '{coll}': {e}")))?),
        None => None,
    };
    Ok(Some(Meta { indexes, schema }))
}

fn meta_or_default(ctx: &Ctx, coll: &str) -> Result<Meta> {
    Ok(load_meta(ctx, coll)?.unwrap_or_default())
}

fn save_meta(ctx: &mut Ctx, coll: &str, meta: &Meta) {
    let mut o = BTreeMap::new();
    o.insert("indexes".to_string(), Value::Array(meta.indexes.iter().map(IndexDef::to_meta).collect()));
    if let Some(s) = &meta.schema {
        o.insert("schema".to_string(), s.raw.clone());
    }
    ctx.put(keys::coll_meta(coll), Value::Object(o).to_json().into_bytes());
}

// --------------------------------------------------------------- schema

const SCHEMA_TYPES: &[&str] =
    &["null", "bool", "int", "double", "number", "string", "array", "object", "objectId", "date", "long", "binData", "any"];

/// An optional per-collection document schema:
/// `{required: ["name"], fields: {name: "string", age: ["int", "null"], "addr.zip": "string"}}`.
/// A required path must be present; a typed path, when present, must hold
/// one of the listed types at every value it reaches. Other fields are free.
#[derive(Clone, Debug)]
struct Schema {
    raw: Value,
    required: Vec<String>,
    fields: Vec<(String, Vec<String>)>,
}

impl Schema {
    fn parse(v: &Value) -> Result<Schema> {
        let Value::Object(o) = v else {
            return Err(Error::Type("a schema must be an object like {required: [...], fields: {...}}".into()));
        };
        if let Some(k) = o.keys().find(|k| !matches!(k.as_str(), "required" | "fields")) {
            return Err(Error::Parse(format!("unknown schema key '{k}' (use required and fields)")));
        }
        let required = match o.get("required") {
            None => vec![],
            Some(Value::Array(a)) => a
                .iter()
                .map(|p| match p {
                    Value::Text(s) => path::validate_path(s).map(|_| s.clone()),
                    _ => Err(Error::Type("schema 'required' lists field paths as strings".into())),
                })
                .collect::<Result<_>>()?,
            Some(_) => return Err(Error::Type("schema 'required' must be an array".into())),
        };
        let mut fields = Vec::new();
        match o.get("fields") {
            None => {}
            Some(Value::Object(f)) => {
                for (p, t) in f {
                    path::validate_path(p)?;
                    let types: Vec<String> = match t {
                        Value::Text(s) => vec![s.clone()],
                        Value::Array(a) if !a.is_empty() => a
                            .iter()
                            .map(|x| x.as_str().map(String::from).ok_or_else(|| Error::Type("type names are strings".into())))
                            .collect::<Result<_>>()?,
                        _ => return Err(Error::Type(format!("schema type of '{p}' must be a type name or a list of them"))),
                    };
                    if let Some(bad) = types.iter().find(|t| !SCHEMA_TYPES.contains(&t.as_str())) {
                        return Err(Error::Type(format!("unknown schema type '{bad}'; known: {}", SCHEMA_TYPES.join(", "))));
                    }
                    fields.push((p.clone(), types));
                }
            }
            Some(_) => return Err(Error::Type("schema 'fields' must be an object".into())),
        }
        Ok(Schema { raw: v.clone(), required, fields })
    }

    fn check(&self, doc: &Value) -> Result<()> {
        for r in &self.required {
            if path::path_values(doc, r).is_empty() {
                return Err(Error::Constraint(format!("schema: required field '{r}' is missing")));
            }
        }
        for (p, types) in &self.fields {
            for v in path::path_values(doc, p) {
                let t = path::type_name(v);
                let ok = types.iter().any(|want| {
                    want == "any" || want == t || (want == "number" && matches!(v, Value::Int(_) | Value::Float(_)))
                });
                if !ok {
                    return Err(Error::Constraint(format!(
                        "schema: field '{p}' must be {}, found {t}",
                        types.join(" or ")
                    )));
                }
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------- document writes

/// Validate and store a document plus its index entries.
/// Records newly multikey index fields in the metadata (written once per field).
fn write_doc(ctx: &mut Ctx, coll: &str, meta: &mut Meta, key: Vec<u8>, doc: &Value) -> Result<()> {
    path::validate_doc(doc)?;
    if let Some(s) = &meta.schema {
        s.check(doc)?;
    }
    let id = key[keys::doc_prefix(coll).len()..].to_vec();
    let mut meta_changed = false;
    for ix in &mut meta.indexes {
        let (parts, multi) = ix.analyze(doc)?;
        ix.check_unique(ctx, coll, &parts, &id)?;
        meta_changed |= ix.note_multikey(&multi);
        let prefix = ix.prefix(coll);
        for p in parts {
            ctx.put([prefix.as_slice(), &p, &id].concat(), Vec::new());
        }
    }
    if meta_changed {
        save_meta(ctx, coll, meta);
    }
    ctx.put(key, doc.to_json().into_bytes());
    Ok(())
}

fn remove_doc(ctx: &mut Ctx, coll: &str, meta: &Meta, key: Vec<u8>, doc: &Value) {
    let id = key[keys::doc_prefix(coll).len()..].to_vec();
    for ix in &meta.indexes {
        // A stored document always produced valid entries when written.
        if let Ok(entries) = ix.entries(coll, doc, &id) {
            for k in entries {
                ctx.delete(k);
            }
        }
    }
    ctx.delete(key);
}

fn insert_with(ctx: &mut Ctx, coll: &str, meta: &mut Meta, doc: Value) -> Result<Value> {
    let Value::Object(mut obj) = doc else {
        return Err(Error::Type("a document must be a JSON object".into()));
    };
    let id = match obj.get("_id") {
        Some(id) => id.clone(),
        None => {
            let n = keys::next_seq(ctx, &index::owner(coll))?;
            let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
            let id = Value::Text(format!("{ms:012x}{n:08x}"));
            obj.insert("_id".into(), id.clone());
            id
        }
    };
    let key = doc_key(coll, &id).map_err(|e| context(e, "_id"))?;
    if ctx.get(&key).is_some() {
        return Err(Error::Constraint(format!("duplicate _id {} in collection '{coll}'", id.to_json())));
    }
    write_doc(ctx, coll, meta, key, &Value::Object(obj))?;
    Ok(id)
}

/// Metadata of a collection, creating the collection if needed.
fn ensure(ctx: &mut Ctx, coll: &str) -> Result<Meta> {
    check_name(coll)?;
    match load_meta(ctx, coll)? {
        Some(m) => Ok(m),
        None => {
            let m = Meta::default();
            save_meta(ctx, coll, &m);
            Ok(m)
        }
    }
}

/// Insert a document; returns its `_id` (generated when absent).
pub fn insert(ctx: &mut Ctx, coll: &str, doc: Value) -> Result<Value> {
    let mut meta = ensure(ctx, coll)?;
    insert_with(ctx, coll, &mut meta, doc)
}

/// Insert several documents, all or none; returns their ids in order.
pub fn insert_many(ctx: &mut Ctx, coll: &str, docs: Vec<Value>) -> Result<Vec<Value>> {
    let mut meta = ensure(ctx, coll)?;
    docs.into_iter()
        .enumerate()
        .map(|(i, d)| insert_with(ctx, coll, &mut meta, d).map_err(|e| context(e, &format!("document {i}"))))
        .collect()
}

// ------------------------------------------------------------------ reads

/// Candidate documents for a filter through the planned access path, in
/// key order. The caller still evaluates the filter.
fn candidates(ctx: &Ctx, coll: &str, f: &Filter, meta: &Meta) -> Result<(Vec<(Vec<u8>, Value)>, Plan)> {
    let plan = index::plan(coll, f, &meta.indexes);
    let mut out = Vec::new();
    match &plan {
        Plan::Ids(ids) => {
            let mut keys: Vec<Vec<u8>> = ids.iter().map(|id| doc_key(coll, id)).collect::<Result<_>>()?;
            keys.sort();
            for k in keys {
                if let Some(raw) = ctx.get(&k) {
                    out.push((k, decode(&raw)?));
                }
            }
        }
        Plan::Index { index, ranges, .. } => {
            let prefix = index.prefix(coll);
            let mut found: BTreeSet<Vec<u8>> = BTreeSet::new();
            for (start, end) in ranges {
                for (k, _) in ctx.scan_range(&prefix, start, Some(end)) {
                    let id = index
                        .id_of(&k[prefix.len()..])
                        .ok_or_else(|| Error::Corrupt(format!("malformed entry in index {}", index.name())))?;
                    found.insert([keys::doc_prefix(coll).as_slice(), id].concat());
                }
            }
            for k in found {
                if let Some(raw) = ctx.get(&k) {
                    out.push((k, decode(&raw)?));
                }
            }
        }
        Plan::Scan => {
            for (k, raw) in ctx.scan(&keys::doc_prefix(coll)) {
                out.push((k, decode(&raw)?));
            }
        }
    }
    Ok((out, plan))
}

fn matching(ctx: &Ctx, coll: &str, filter: &Value) -> Result<(Vec<(Vec<u8>, Value)>, Meta)> {
    check_name(coll)?;
    let f = filter::compile(filter)?;
    let meta = meta_or_default(ctx, coll)?;
    let (cands, _) = candidates(ctx, coll, &f, &meta)?;
    Ok((cands.into_iter().filter(|(_, d)| f.eval(d)).collect(), meta))
}

#[derive(Default, Clone, Debug)]
pub struct FindOptions {
    /// `{"field": 1}` ascending, `{"field": -1}` descending, or
    /// `[["a", -1], ["b", 1]]` to fix the key order.
    pub sort: Option<Value>,
    pub skip: usize,
    pub limit: Option<usize>,
    /// Applied after sort, skip and limit; see [`project`].
    pub projection: Option<Value>,
}

pub fn find(ctx: &Ctx, coll: &str, filter: &Value, opts: &FindOptions) -> Result<Vec<Value>> {
    let sort = opts.sort.as_ref().map(aggregate::parse_sort).transpose()?;
    let (found, _) = matching(ctx, coll, filter)?;
    let mut docs: Vec<Value> = found.into_iter().map(|(_, d)| d).collect();
    if let Some(spec) = sort {
        aggregate::sort_docs(&mut docs, &spec);
    }
    let docs = docs.into_iter().skip(opts.skip).take(opts.limit.unwrap_or(usize::MAX));
    match &opts.projection {
        Some(p) => docs.map(|d| project(d, p)).collect(),
        None => Ok(docs.collect()),
    }
}

pub fn count(ctx: &Ctx, coll: &str, filter: &Value) -> Result<u64> {
    Ok(matching(ctx, coll, filter)?.0.len() as u64)
}

/// Distinct values of a field among matching documents (array fields
/// contribute their elements), sorted.
pub fn distinct(ctx: &Ctx, coll: &str, field: &str, filter: &Value) -> Result<Vec<Value>> {
    path::validate_path(field)?;
    let mut seen: BTreeSet<SortKey> = BTreeSet::new();
    for (_, d) in matching(ctx, coll, filter)?.0 {
        for v in path::path_values(&d, field) {
            match v {
                Value::Array(a) => seen.extend(a.iter().cloned().map(SortKey)),
                v => {
                    seen.insert(SortKey(v.clone()));
                }
            }
        }
    }
    Ok(seen.into_iter().map(|k| k.0).collect())
}

struct SortKey(Value);
impl PartialEq for SortKey {
    fn eq(&self, o: &Self) -> bool {
        self.0.total_cmp(&o.0).is_eq()
    }
}
impl Eq for SortKey {}
impl PartialOrd for SortKey {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for SortKey {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&o.0)
    }
}

/// How a filter would be answered: the access path, how many documents it
/// read and how many matched.
pub fn explain(ctx: &Ctx, coll: &str, filter: &Value) -> Result<Value> {
    check_name(coll)?;
    let f = filter::compile(filter)?;
    let meta = meta_or_default(ctx, coll)?;
    let (cands, plan) = candidates(ctx, coll, &f, &meta)?;
    let matched = cands.iter().filter(|(_, d)| f.eval(d)).count();
    let mut out = plan.describe();
    if let Value::Object(o) = &mut out {
        o.insert("docsExamined".into(), Value::Int(cands.len() as i64));
        o.insert("matched".into(), Value::Int(matched as i64));
    }
    Ok(out)
}

/// Run an aggregation pipeline. A leading `$match` uses the planner.
pub fn aggregate(ctx: &Ctx, coll: &str, pipeline: &[Value]) -> Result<Vec<Value>> {
    let (docs, rest) = match pipeline.first().map(aggregate::stage_parts).transpose()? {
        Some(("$match", f)) => (find(ctx, coll, f, &FindOptions::default())?, &pipeline[1..]),
        _ => (find(ctx, coll, &Value::Null, &FindOptions::default())?, pipeline),
    };
    aggregate::run(docs, rest)
}

// ---------------------------------------------------------- update/delete

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpdateResult {
    /// Documents that matched the filter.
    pub matched: u64,
    /// Matched documents that the update actually changed.
    pub modified: u64,
    /// `_id` of a document created by an upsert.
    pub upserted_id: Option<Value>,
}

/// Update matching documents (all of them, or only the first when `multi`
/// is false). With `upsert`, a document is created when nothing matches,
/// seeded from the filter's top-level equality conditions.
pub fn update(ctx: &mut Ctx, coll: &str, filter: &Value, upd: &Value, multi: bool, upsert: bool) -> Result<UpdateResult> {
    check_name(coll)?;
    let f = filter::compile(filter)?;
    update::validate(upd)?;
    let mut meta = meta_or_default(ctx, coll)?;
    let mut res = UpdateResult::default();
    let (cands, _) = candidates(ctx, coll, &f, &meta)?;
    for (key, old) in cands {
        if !f.eval(&old) {
            continue;
        }
        res.matched += 1;
        let mut new = old.clone();
        update::apply(&mut new, upd, false)?;
        if new.get("_id") != old.get("_id") {
            return Err(Error::Constraint("_id cannot be changed".into()));
        }
        if new != old {
            remove_doc(ctx, coll, &meta, key.clone(), &old);
            write_doc(ctx, coll, &mut meta, key, &new)?;
            res.modified += 1;
        }
        if !multi {
            break;
        }
    }
    if res.matched == 0 && upsert {
        let mut base = Value::Object(BTreeMap::new());
        for (p, cond) in f.conjuncts() {
            if let filter::Cond::Eq(v) = cond {
                path::set_path(&mut base, p, v.clone())?;
            }
        }
        update::apply(&mut base, upd, true)?;
        let mut meta = ensure(ctx, coll)?;
        res.upserted_id = Some(insert_with(ctx, coll, &mut meta, base)?);
    }
    Ok(res)
}

/// Delete matching documents, at most `limit` of them when given.
pub fn delete(ctx: &mut Ctx, coll: &str, filter: &Value, limit: Option<u64>) -> Result<u64> {
    let (found, meta) = matching(ctx, coll, filter)?;
    let mut n = 0;
    for (key, d) in found {
        if limit.map_or(false, |l| n >= l) {
            break;
        }
        remove_doc(ctx, coll, &meta, key, &d);
        n += 1;
    }
    Ok(n)
}

/// Run a list of write operations as one atomic unit: if any fails,
/// nothing is written. Operations:
/// `{insertOne: {document}}`, `{updateOne|updateMany: {filter, update, upsert?}}`,
/// `{replaceOne: {filter, replacement, upsert?}}`, `{deleteOne|deleteMany: {filter}}`.
pub fn bulk_write(ctx: &mut Ctx, coll: &str, ops: &[Value]) -> Result<Value> {
    check_name(coll)?;
    let (mut inserted, mut matched, mut modified, mut deleted) = (Vec::new(), 0i64, 0i64, 0i64);
    let mut upserted = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let mut run = |ctx: &mut Ctx| -> Result<()> {
            let (name, body) = match op {
                Value::Object(o) if o.len() == 1 => o.iter().next().expect("one entry"),
                _ => return Err(Error::Parse("each operation is an object with one key, e.g. {insertOne: {...}}".into())),
            };
            let field = |k: &str| body.get(k).ok_or_else(|| Error::Parse(format!("{name} needs '{k}'")));
            let upsert = body.get("upsert") == Some(&Value::Bool(true));
            match name.as_str() {
                "insertOne" => {
                    inserted.push(insert(ctx, coll, field("document")?.clone())?);
                }
                "updateOne" | "updateMany" | "replaceOne" => {
                    let (upd, replacing) = match name.as_str() {
                        "replaceOne" => (field("replacement")?, true),
                        _ => (field("update")?, false),
                    };
                    if update::is_replacement(upd)? != replacing {
                        return Err(Error::Parse(if replacing {
                            "replaceOne takes a document without operators".into()
                        } else {
                            format!("{name} takes update operators such as $set")
                        }));
                    }
                    let r = update(ctx, coll, field("filter")?, upd, name == "updateMany", upsert)?;
                    matched += r.matched as i64;
                    modified += r.modified as i64;
                    if let Some(id) = r.upserted_id {
                        upserted.push(obj(vec![("index", Value::Int(i as i64)), ("_id", id)]));
                    }
                }
                "deleteOne" | "deleteMany" => {
                    let limit = (name == "deleteOne").then_some(1);
                    deleted += delete(ctx, coll, field("filter")?, limit)? as i64;
                }
                other => return Err(Error::Parse(format!("unknown bulk operation {other}"))),
            }
            Ok(())
        };
        run(ctx).map_err(|e| context(e, &format!("operation {i}")))?;
    }
    Ok(obj(vec![
        ("insertedCount", Value::Int(inserted.len() as i64)),
        ("insertedIds", Value::Array(inserted)),
        ("matchedCount", Value::Int(matched)),
        ("modifiedCount", Value::Int(modified)),
        ("deletedCount", Value::Int(deleted)),
        ("upsertedCount", Value::Int(upserted.len() as i64)),
        ("upserted", Value::Array(upserted)),
    ]))
}

// ------------------------------------------------------------ collections

/// Remove a collection, its documents and its indexes. Returns whether it existed.
pub fn drop(ctx: &mut Ctx, coll: &str) -> Result<bool> {
    check_name(coll)?;
    let meta = keys::coll_meta(coll);
    let existed = ctx.get(&meta).is_some();
    for prefix in [keys::doc_prefix(coll), keys::index_owner_prefix(&index::owner(coll))] {
        for (k, _) in ctx.scan(&prefix) {
            ctx.delete(k);
        }
    }
    ctx.delete(meta);
    ctx.delete(keys::seq(&index::owner(coll)));
    Ok(existed)
}

pub fn collections(ctx: &Ctx) -> Vec<String> {
    ctx.scan(keys::COLL_META_PREFIX)
        .into_iter()
        .map(|(k, _)| String::from_utf8_lossy(&k[keys::COLL_META_PREFIX.len()..]).into_owned())
        .collect()
}

/// Field list of an index specification: `"field"`, `{field: 1, other: -1}`
/// (fields in name order, since objects do not keep key order; directions
/// are accepted but entries are always stored ascending), or
/// `["field", "other"]` / `[["field", 1], ["other", 1]]` to fix the order.
pub fn index_fields(spec: &Value) -> Result<Vec<String>> {
    let dir_ok = |v: &Value| matches!(v.as_f64(), Some(n) if n == 1.0 || n == -1.0);
    let fields: Vec<String> = match spec {
        Value::Text(f) => vec![f.clone()],
        Value::Object(o) if !o.is_empty() => {
            if let Some((k, _)) = o.iter().find(|(_, v)| !dir_ok(v)) {
                return Err(Error::Parse(format!("index key '{k}' must be 1 or -1 (only ascending/descending b-tree indexes exist)")));
            }
            o.keys().cloned().collect()
        }
        Value::Array(a) if !a.is_empty() => a
            .iter()
            .map(|it| match it {
                Value::Text(f) => Ok(f.clone()),
                Value::Array(p) if p.len() == 2 && dir_ok(&p[1]) => {
                    p[0].as_str().map(String::from).ok_or_else(|| Error::Parse("index field names are strings".into()))
                }
                _ => Err(Error::Parse("index list items are \"field\" or [\"field\", 1]".into())),
            })
            .collect::<Result<_>>()?,
        _ => return Err(Error::Parse("an index spec is a field, {field: 1, ...} or [\"field\", ...]".into())),
    };
    Ok(fields)
}

/// Create an index over one or more field paths and build its entries
/// from the existing documents. Returns false if an identical index exists.
pub fn create_index(ctx: &mut Ctx, coll: &str, fields: Vec<String>, unique: bool) -> Result<bool> {
    let mut def = IndexDef::new(fields, unique)?;
    let mut meta = ensure(ctx, coll)?;
    if let Some(existing) = meta.indexes.iter().find(|i| i.fields == def.fields) {
        if existing.same_as(&def) {
            return Ok(false);
        }
        return Err(Error::Schema(format!("index {} already exists with different options", def.name())));
    }
    let prefix = def.prefix(coll);
    for (key, raw) in ctx.scan(&keys::doc_prefix(coll)) {
        let doc = decode(&raw)?;
        let id = key[keys::doc_prefix(coll).len()..].to_vec();
        let (parts, multi) = def.analyze(&doc).map_err(|e| context(e, "cannot build index"))?;
        def.check_unique(ctx, coll, &parts, &id).map_err(|e| context(e, "cannot build index"))?;
        def.note_multikey(&multi);
        for p in parts {
            ctx.put([prefix.as_slice(), &p, &id].concat(), Vec::new());
        }
    }
    meta.indexes.push(def);
    save_meta(ctx, coll, &meta);
    Ok(true)
}

/// Drop an index by display name (`a_1_b_1`), by field (`"a"`), or by spec.
pub fn drop_index(ctx: &mut Ctx, coll: &str, spec: &Value) -> Result<bool> {
    check_name(coll)?;
    let Some(mut meta) = load_meta(ctx, coll)? else { return Ok(false) };
    let pos = match spec {
        Value::Text(name) => meta.indexes.iter().position(|i| i.name() == *name || i.fields == [name.clone()]),
        other => {
            let fields = index_fields(other)?;
            meta.indexes.iter().position(|i| i.fields == fields)
        }
    };
    let Some(pos) = pos else { return Ok(false) };
    let def = meta.indexes.remove(pos);
    for (k, _) in ctx.scan(&def.prefix(coll)) {
        ctx.delete(k);
    }
    save_meta(ctx, coll, &meta);
    Ok(true)
}

/// Index descriptions, starting with the implicit `_id` primary key.
pub fn list_indexes(ctx: &Ctx, coll: &str) -> Result<Vec<Value>> {
    check_name(coll)?;
    let Some(meta) = load_meta(ctx, coll)? else { return Ok(vec![]) };
    let describe = |name: String, fields: &[String], unique: bool| {
        obj(vec![
            ("name", Value::Text(name)),
            ("key", Value::Object(fields.iter().map(|f| (f.clone(), Value::Int(1))).collect())),
            ("fields", Value::Array(fields.iter().map(|f| Value::Text(f.clone())).collect())),
            ("unique", Value::Bool(unique)),
        ])
    };
    let mut out = vec![describe("_id_".into(), &["_id".to_string()], true)];
    out.extend(meta.indexes.iter().map(|i| describe(i.name(), &i.fields, i.unique)));
    Ok(out)
}

/// Field paths of a collection's secondary indexes (compound ones joined with ',').
pub fn indexes(ctx: &Ctx, coll: &str) -> Result<Vec<String>> {
    Ok(meta_or_default(ctx, coll)?.indexes.iter().map(|i| i.fields.join(",")).collect())
}

/// Set (or with `null` / `{}`, clear) the collection's schema. Every
/// existing document must already satisfy it.
pub fn set_schema(ctx: &mut Ctx, coll: &str, schema: &Value) -> Result<()> {
    let mut meta = ensure(ctx, coll)?;
    let clear = matches!(schema, Value::Null) || matches!(schema, Value::Object(o) if o.is_empty());
    meta.schema = if clear { None } else { Some(Schema::parse(schema)?) };
    if let Some(s) = &meta.schema {
        for (_, raw) in ctx.scan(&keys::doc_prefix(coll)) {
            let d = decode(&raw)?;
            s.check(&d).map_err(|e| context(e, &format!("existing document {}", d.get("_id").map_or("?".into(), Value::to_json))))?;
        }
    }
    save_meta(ctx, coll, &meta);
    Ok(())
}

pub fn get_schema(ctx: &Ctx, coll: &str) -> Result<Value> {
    check_name(coll)?;
    Ok(meta_or_default(ctx, coll)?.schema.map_or(Value::Null, |s| s.raw))
}

/// Check a collection's stored state: every document decodes, sits under
/// its own `_id`, is a valid document and satisfies the schema, and the
/// index entries are exactly those the documents imply.
pub fn validate(ctx: &Ctx, coll: &str) -> Result<Value> {
    check_name(coll)?;
    let meta = meta_or_default(ctx, coll)?;
    let dp = keys::doc_prefix(coll);
    let mut problems: Vec<Value> = Vec::new();
    let mut expected: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut ndocs = 0i64;
    for (key, raw) in ctx.scan(&dp) {
        ndocs += 1;
        let id = &key[dp.len()..];
        let doc = match decode(&raw) {
            Ok(d) => d,
            Err(e) => {
                problems.push(Value::Text(format!("document key {id:?}: {e}")));
                continue;
            }
        };
        let label = doc.get("_id").map_or("(no _id)".to_string(), Value::to_json);
        match doc.get("_id").map(keys::encode_id) {
            Some(Ok(enc)) if enc == id => {}
            _ => problems.push(Value::Text(format!("document {label} is stored under the wrong key"))),
        }
        if let Err(e) = path::validate_doc(&doc) {
            problems.push(Value::Text(format!("document {label}: {e}")));
        }
        if let Some(s) = &meta.schema {
            if let Err(e) = s.check(&doc) {
                problems.push(Value::Text(format!("document {label}: {e}")));
            }
        }
        for ix in &meta.indexes {
            match ix.analyze(&doc) {
                Ok((parts, multi)) => {
                    let prefix = ix.prefix(coll);
                    expected.extend(parts.into_iter().map(|p| [prefix.as_slice(), &p, id].concat()));
                    if ix.clone().note_multikey(&multi) {
                        problems.push(Value::Text(format!(
                            "document {label} holds several values for a field index {} does not mark multikey",
                            ix.name()
                        )));
                    }
                }
                Err(e) => problems.push(Value::Text(format!("document {label}: {e}"))),
            }
        }
    }
    let mut actual: BTreeSet<Vec<u8>> = BTreeSet::new();
    let known: Vec<Vec<u8>> = meta.indexes.iter().map(|i| i.prefix(coll)).collect();
    for (k, _) in ctx.scan(&keys::index_owner_prefix(&index::owner(coll))) {
        if !known.iter().any(|p| k.starts_with(p)) {
            problems.push(Value::Text("index entry belongs to no defined index".into()));
        }
        actual.insert(k);
    }
    let missing = expected.difference(&actual).count();
    let dangling = actual.difference(&expected).count();
    if missing > 0 {
        problems.push(Value::Text(format!("{missing} index entr(y/ies) missing")));
    }
    if dangling > 0 {
        problems.push(Value::Text(format!("{dangling} index entr(y/ies) point at nothing")));
    }
    // Unique indexes: no two documents share a full key.
    for ix in meta.indexes.iter().filter(|i| i.unique) {
        let prefix = ix.prefix(coll);
        let mut owners: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for (k, _) in ctx.scan(&prefix) {
            let rest = &k[prefix.len()..];
            let Some(id) = ix.id_of(rest) else { continue };
            let part = rest[..rest.len() - id.len()].to_vec();
            if ix.fields.len() > 1 && index::has_absent(&part, ix.fields.len()) {
                continue;
            }
            if let Some(other) = owners.insert(part, id.to_vec()) {
                if other != id {
                    problems.push(Value::Text(format!("unique index {} has a duplicate key", ix.name())));
                }
            }
        }
    }
    Ok(obj(vec![
        ("collection", Value::Text(coll.to_string())),
        ("documents", Value::Int(ndocs)),
        ("indexes", Value::Int(meta.indexes.len() as i64)),
        ("indexEntries", Value::Int(actual.len() as i64)),
        ("ok", Value::Bool(problems.is_empty())),
        ("problems", Value::Array(problems)),
    ]))
}

/// Rebuild every index entry of a collection from its documents.
/// Returns the number of entries written.
pub fn reindex(ctx: &mut Ctx, coll: &str) -> Result<u64> {
    check_name(coll)?;
    let Some(mut meta) = load_meta(ctx, coll)? else { return Ok(0) };
    for ix in &mut meta.indexes {
        ix.multikey.clear();
    }
    for (k, _) in ctx.scan(&keys::index_owner_prefix(&index::owner(coll))) {
        ctx.delete(k);
    }
    let mut n = 0;
    for (key, raw) in ctx.scan(&keys::doc_prefix(coll)) {
        let doc = decode(&raw)?;
        let id = key[keys::doc_prefix(coll).len()..].to_vec();
        for ix in &mut meta.indexes {
            let (parts, multi) = ix.analyze(&doc)?;
            ix.check_unique(ctx, coll, &parts, &id)?;
            ix.note_multikey(&multi);
            let prefix = ix.prefix(coll);
            for p in parts {
                ctx.put([prefix.as_slice(), &p, &id].concat(), Vec::new());
                n += 1;
            }
        }
    }
    save_meta(ctx, coll, &meta);
    Ok(n)
}
