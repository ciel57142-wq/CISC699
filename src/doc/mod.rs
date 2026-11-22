//! Document (NoSQL) model: schemaless JSON documents grouped in collections,
//! queried with MongoDB-style filters, update operators and aggregation
//! pipelines.
//!
//! Query execution is split into stages, one module each:
//!
//! | stage                       | module        |
//! |-----------------------------|---------------|
//! | query parsing, predicates   | [`filter`]    |
//! | index selection / lookup    | [`plan`]      |
//! | retrieval, sort, projection | [`exec`]      |
//! | aggregation                 | [`aggregate`] |
//! | update operators            | [`update`]    |
//! | storage layout              | [`codec`]     |

mod aggregate;
pub mod codec;
mod exec;
mod filter;
mod plan;
mod update;

use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use codec::{Doc, Fields};
use exec::{parse_sort, Projection, Query};
use filter::Pred;
use std::collections::{BTreeMap, BTreeSet};
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

pub(crate) fn owner(coll: &str) -> String {
    format!("c:{coll}")
}

/// Extended-JSON wrappers (`{"$oid": ..}`, `{"$date": ..}`) are values, not operators.
pub(crate) fn is_literal_object(o: &BTreeMap<String, Value>) -> bool {
    o.len() == 1 && matches!(o.keys().next().map(String::as_str), Some("$oid" | "$date" | "$numberLong"))
}

// ---------------------------------------------------------------- indexes

/// One secondary index of a collection.
#[derive(Debug, Clone)]
pub struct IndexDef {
    pub path: String,
    /// The index also has an entry for every document without an indexable
    /// scalar at `path` (missing, null, array or object). Such an index can
    /// answer `null` / `$exists: false` queries and, when no such entry
    /// exists, deliver a sort. Indexes built before 0.3 lack these entries
    /// until `reIndex()`.
    pub nulls: bool,
}

/// The collection's indexes and their key prefixes, resolved once per statement.
struct Indexes {
    defs: Vec<IndexDef>,
    bases: Vec<Vec<u8>>,
}

impl Indexes {
    fn new(coll: &str, defs: Vec<IndexDef>) -> Indexes {
        let own = owner(coll);
        let bases = defs.iter().map(|d| keys::index_prefix(&own, &d.path)).collect();
        Indexes { defs, bases }
    }

    /// Index entries for one document, sorted and without duplicates.
    /// Array fields get one entry per scalar element.
    fn keys_for(&self, id: &[u8], doc: &dyn Fields) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for (def, base) in self.defs.iter().zip(&self.bases) {
            let entry = |enc: &[u8]| [base.as_slice(), enc, id].concat();
            let v = doc.field(&def.path)?;
            let scalar = match v.as_deref() {
                Some(Value::Array(a)) => {
                    out.extend(a.iter().filter_map(keys::encode_index_value).map(|e| entry(&e)));
                    None
                }
                Some(v) => keys::encode_index_value(v),
                None => None,
            };
            match scalar {
                Some(enc) => out.push(entry(&enc)),
                None if def.nulls => out.push(entry(&[keys::INDEX_OTHER])),
                None => {}
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

/// None when the collection does not exist.
fn read_meta(ctx: &Ctx, coll: &str) -> Result<Option<Vec<IndexDef>>> {
    let Some(raw) = ctx.get_ref(&keys::coll_meta(coll)) else {
        return Ok(None);
    };
    let s = std::str::from_utf8(raw).map_err(|_| Error::Corrupt("collection metadata is not UTF-8".into()))?;
    let meta = Value::parse(s).map_err(|e| Error::Corrupt(format!("collection metadata: {e}")))?;
    let names = |key: &str| -> Vec<String> {
        match meta.get(key) {
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => vec![],
        }
    };
    let nulls = names("nulls");
    Ok(Some(names("indexes").into_iter().map(|path| IndexDef { nulls: nulls.contains(&path), path }).collect()))
}

fn write_meta(ctx: &mut Ctx, coll: &str, defs: &[IndexDef]) {
    let list = |f: &dyn Fn(&IndexDef) -> bool| Value::Array(defs.iter().filter(|d| f(d)).map(|d| Value::Text(d.path.clone())).collect());
    let mut o = BTreeMap::new();
    o.insert("indexes".to_string(), list(&|_| true));
    o.insert("nulls".to_string(), list(&|d| d.nulls));
    ctx.put(keys::coll_meta(coll), Value::Object(o).to_json().into_bytes());
}

/// Indexed field paths of a collection.
pub fn indexes(ctx: &Ctx, coll: &str) -> Result<Vec<String>> {
    Ok(read_meta(ctx, coll)?.unwrap_or_default().into_iter().map(|d| d.path).collect())
}

/// Index a field path so queries on it (equality, `$in`, ranges, `null`,
/// sorts) stop scanning the collection. Returns false if it already existed.
pub fn create_index(ctx: &mut Ctx, coll: &str, field: &str) -> Result<bool> {
    check_name(coll)?;
    if field.is_empty() || field.contains('\0') || field == "_id" {
        return Err(Error::Schema(format!("cannot index field '{field}'")));
    }
    let mut defs = read_meta(ctx, coll)?.unwrap_or_default();
    if defs.iter().any(|d| d.path == field) {
        return Ok(false);
    }
    let one = Indexes::new(coll, vec![IndexDef { path: field.to_string(), nulls: true }]);
    let entries = build_entries(ctx, coll, &one)?;
    for k in entries {
        ctx.put(k, Vec::new());
    }
    defs.push(IndexDef { path: field.to_string(), nulls: true });
    write_meta(ctx, coll, &defs);
    Ok(true)
}

/// Every index entry the collection's documents call for.
fn build_entries(ctx: &Ctx, coll: &str, ix: &Indexes) -> Result<Vec<Vec<u8>>> {
    let prefix = keys::doc_prefix(coll);
    let mut out = Vec::new();
    for (k, raw) in ctx.iter_prefix(&prefix) {
        out.extend(ix.keys_for(&k[prefix.len()..], &Doc::from_stored(raw)?)?);
    }
    Ok(out)
}

/// Drop and rebuild every index of a collection (also upgrading indexes
/// built before 0.3 to the form with `null` entries). Returns how many.
pub fn reindex(ctx: &mut Ctx, coll: &str) -> Result<usize> {
    check_name(coll)?;
    let defs: Vec<IndexDef> =
        read_meta(ctx, coll)?.unwrap_or_default().into_iter().map(|d| IndexDef { path: d.path, nulls: true }).collect();
    let old: Vec<Vec<u8>> = ctx.iter_prefix(&keys::index_owner_prefix(&owner(coll))).map(|(k, _)| k.to_vec()).collect();
    for k in old {
        ctx.delete(k);
    }
    let ix = Indexes::new(coll, defs);
    for k in build_entries(ctx, coll, &ix)? {
        ctx.put(k, Vec::new());
    }
    if ctx.get_ref(&keys::coll_meta(coll)).is_some() {
        write_meta(ctx, coll, &ix.defs);
    }
    Ok(ix.defs.len())
}

/// Check that documents and index entries agree: every document decodes,
/// sits under the key of its own `_id`, and has exactly the index entries
/// its values call for, and no index entry points elsewhere.
pub fn validate(ctx: &Ctx, coll: &str) -> Result<Value> {
    check_name(coll)?;
    let ix = Indexes::new(coll, read_meta(ctx, coll)?.unwrap_or_default());
    let prefix = keys::doc_prefix(coll);
    let mut expected = BTreeSet::new();
    let (mut docs, mut bad_docs) = (0i64, 0i64);
    for (k, raw) in ctx.iter_prefix(&prefix) {
        docs += 1;
        let ok = (|| -> Result<bool> {
            let d = Doc::from_stored(raw)?;
            let id = d.field("_id")?.map(|c| c.into_owned()).unwrap_or(Value::Null);
            let same_key = keys::encode_id(&id).map_or(false, |e| e == k[prefix.len()..]);
            expected.extend(ix.keys_for(&k[prefix.len()..], &d)?);
            Ok(same_key && d.to_value().is_ok())
        })();
        if !matches!(ok, Ok(true)) {
            bad_docs += 1;
        }
    }
    // Entries of indexes the metadata lists (stale entries of others count as extra).
    let actual: BTreeSet<Vec<u8>> = ctx.iter_prefix(&keys::index_owner_prefix(&owner(coll))).map(|(k, _)| k.to_vec()).collect();
    let missing = expected.difference(&actual).count() as i64;
    let extra = actual.difference(&expected).count() as i64;
    let mut o = BTreeMap::new();
    o.insert("ok".to_string(), Value::Bool(missing == 0 && extra == 0 && bad_docs == 0));
    o.insert("documents".to_string(), Value::Int(docs));
    o.insert("invalid_documents".to_string(), Value::Int(bad_docs));
    o.insert("index_entries".to_string(), Value::Int(actual.len() as i64));
    o.insert("missing_entries".to_string(), Value::Int(missing));
    o.insert("extra_entries".to_string(), Value::Int(extra));
    Ok(Value::Object(o))
}

// ------------------------------------------------------------------- CRUD

/// Insert a document; returns its `_id` (generated when absent).
pub fn insert(ctx: &mut Ctx, coll: &str, doc: Value) -> Result<Value> {
    Ok(insert_many(ctx, coll, vec![doc])?.pop().expect("one id per document"))
}

/// Insert documents in order; returns their `_id`s. The collection's
/// metadata, indexes and id sequence are read and written once for the
/// whole batch, not once per document.
pub fn insert_many(ctx: &mut Ctx, coll: &str, docs: Vec<Value>) -> Result<Vec<Value>> {
    check_name(coll)?;
    let defs = match read_meta(ctx, coll)? {
        Some(d) => d,
        None => {
            write_meta(ctx, coll, &[]);
            vec![]
        }
    };
    let ix = Indexes::new(coll, defs);
    let prefix = keys::doc_prefix(coll);
    let seq_key = keys::seq(&owner(coll));
    let mut seq: Option<i64> = None;
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let mut ids = Vec::with_capacity(docs.len());
    for doc in docs {
        check_depth(&doc)?;
        let Value::Object(mut obj) = doc else {
            return Err(Error::Type("a document must be a JSON object".into()));
        };
        let id = match obj.get("_id") {
            Some(id) => id.clone(),
            None => {
                let cur = match seq {
                    Some(n) => n,
                    None => match ctx.get_ref(&seq_key) {
                        Some(raw) => std::str::from_utf8(raw)
                            .ok()
                            .and_then(|s| s.parse::<i64>().ok())
                            .ok_or_else(|| Error::Corrupt(format!("sequence '{}' is not a number", owner(coll))))?,
                        None => 0,
                    },
                };
                let n = cur + 1;
                seq = Some(n);
                let id = Value::Text(format!("{ms:012x}{n:08x}"));
                obj.insert("_id".into(), id.clone());
                id
            }
        };
        let enc_id = keys::encode_id(&id)?;
        let key = [prefix.as_slice(), &enc_id].concat();
        if ctx.get_ref(&key).is_some() {
            return Err(Error::Constraint(format!("duplicate _id {} in collection '{coll}'", id.to_json())));
        }
        let doc = Value::Object(obj);
        for k in ix.keys_for(&enc_id, &doc)? {
            ctx.put(k, Vec::new());
        }
        ctx.put(key, codec::encode(&doc));
        ids.push(id);
    }
    if let Some(n) = seq {
        ctx.put(seq_key, n.to_string().into_bytes());
    }
    Ok(ids)
}

#[derive(Default, Debug, Clone)]
pub struct FindOptions {
    /// `{"field": 1}` ascending, `{"field": -1}` descending.
    pub sort: Option<Value>,
    pub skip: usize,
    pub limit: Option<usize>,
    /// `{"a": 1}` keeps only `a` (and `_id`); `{"a": 0}` drops `a`.
    pub projection: Option<Value>,
}

/// Index definitions, read only when the plan could use them: a query
/// that only constrains (or sorts by) `_id` never needs the metadata.
fn plan_indexes(ctx: &Ctx, coll: &str, pred: &Pred, sort: &[(String, bool)]) -> Result<Vec<IndexDef>> {
    let only_id = pred.required_fields().iter().all(|(p, _)| *p == "_id") && sort.iter().take(1).all(|(p, _)| p == "_id");
    if only_id {
        return Ok(vec![]);
    }
    Ok(read_meta(ctx, coll)?.unwrap_or_default())
}

fn build_query(filter: &Value, opts: &FindOptions) -> Result<Query> {
    Ok(Query {
        pred: Pred::parse(filter)?,
        sort: parse_sort(opts.sort.as_ref())?,
        skip: opts.skip,
        limit: opts.limit,
        projection: match &opts.projection {
            Some(p) => Projection::parse(p)?,
            None => None,
        },
    })
}

pub fn find(ctx: &Ctx, coll: &str, filter: &Value, opts: &FindOptions) -> Result<Vec<Value>> {
    check_name(coll)?;
    let q = build_query(filter, opts)?;
    let defs = plan_indexes(ctx, coll, &q.pred, &q.sort)?;
    let p = plan::plan(ctx, coll, &q.pred, &q.sort, &defs)?;
    exec::run(ctx, coll, &p, &q)
}

/// Number of documents matching `filter`. Nothing is decoded beyond the
/// fields the filter reads.
pub fn count(ctx: &Ctx, coll: &str, filter: &Value) -> Result<usize> {
    check_name(coll)?;
    let pred = Pred::parse(filter)?;
    let defs = plan_indexes(ctx, coll, &pred, &[])?;
    let p = plan::plan(ctx, coll, &pred, &[], &defs)?;
    exec::count(ctx, coll, &p, &pred)
}

/// How `find(filter, opts)` would run: access path, index, sort strategy.
pub fn explain(ctx: &Ctx, coll: &str, filter: &Value, opts: &FindOptions) -> Result<Value> {
    check_name(coll)?;
    let q = build_query(filter, opts)?;
    let defs = read_meta(ctx, coll)?.unwrap_or_default();
    let p = plan::plan(ctx, coll, &q.pred, &q.sort, &defs)?;
    Ok(plan::describe(&p, &q.pred, &q.sort))
}

/// Run an aggregation pipeline (see [`aggregate`] for the stages).
pub fn aggregate(ctx: &Ctx, coll: &str, pipeline: &[Value]) -> Result<Vec<Value>> {
    check_name(coll)?;
    let defs = read_meta(ctx, coll)?.unwrap_or_default();
    aggregate::aggregate(ctx, coll, pipeline, &defs)
}

/// Keys of the documents matching `pred`, in `_id` order, at most `limit`.
fn matching_keys(ctx: &Ctx, coll: &str, pred: &Pred, defs: &[IndexDef], limit: Option<usize>) -> Result<Vec<Vec<u8>>> {
    let p = plan::plan(ctx, coll, pred, &[], defs)?;
    let mut out = Vec::new();
    let early = p.id_ordered;
    exec::for_each_candidate(ctx, coll, &p, |k, raw, _| {
        if early && limit.map_or(false, |l| out.len() >= l) {
            return Ok(false);
        }
        if p.covered || pred.is_true() || pred.eval(&Doc::from_stored(raw)?)? {
            out.push(k.to_vec());
        }
        Ok(true)
    })?;
    if !early {
        out.sort();
    }
    out.truncate(limit.unwrap_or(usize::MAX));
    Ok(out)
}

/// Apply index changes: delete entries only the old version had, add those only the new one has.
fn diff_index(ctx: &mut Ctx, old: Vec<Vec<u8>>, new: Vec<Vec<u8>>) {
    let (mut i, mut j) = (0, 0);
    while i < old.len() || j < new.len() {
        match (old.get(i), new.get(j)) {
            (Some(a), Some(b)) if a == b => {
                i += 1;
                j += 1;
            }
            (Some(a), Some(b)) if a < b => {
                ctx.delete(a.clone());
                i += 1;
            }
            (Some(a), None) => {
                ctx.delete(a.clone());
                i += 1;
            }
            (_, Some(b)) => {
                ctx.put(b.clone(), Vec::new());
                j += 1;
            }
            (None, None) => break,
        }
    }
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
    check_name(coll)?;
    let pred = Pred::parse(filter)?;
    let defs = read_meta(ctx, coll)?.unwrap_or_default();
    let targets = matching_keys(ctx, coll, &pred, &defs, (!multi).then_some(1))?;
    let ix = Indexes::new(coll, defs);
    let prefix_len = keys::doc_prefix(coll).len();
    let mut n = 0;
    for key in targets {
        let raw = ctx.get_ref(&key).ok_or_else(|| Error::Corrupt("matched document vanished".into()))?;
        // Decode once and change in place: no copy of the old document is kept.
        let mut doc = Doc::from_stored(raw)?.to_value()?;
        let old_keys = ix.keys_for(&key[prefix_len..], &doc)?;
        let old_id = doc.get("_id").cloned();
        update::apply_update(&mut doc, upd, false)?;
        check_depth(&doc)?;
        if doc.get("_id") != old_id.as_ref() {
            return Err(Error::Constraint("_id cannot be changed".into()));
        }
        let new_keys = ix.keys_for(&key[prefix_len..], &doc)?;
        diff_index(ctx, old_keys, new_keys);
        ctx.put(key, codec::encode(&doc));
        n += 1;
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
                    update::set_path(&mut base, k, cond.clone())?;
                }
            }
        }
        update::apply_update(&mut base, upd, true)?;
        return Ok((0, Some(insert(ctx, coll, base)?)));
    }
    Ok((n, None))
}

/// Delete matching documents, at most `limit` of them when given.
pub fn delete(ctx: &mut Ctx, coll: &str, filter: &Value, limit: Option<u64>) -> Result<u64> {
    check_name(coll)?;
    let pred = Pred::parse(filter)?;
    let defs = read_meta(ctx, coll)?.unwrap_or_default();
    let targets = matching_keys(ctx, coll, &pred, &defs, limit.map(|l| l as usize))?;
    let ix = Indexes::new(coll, defs);
    let prefix_len = keys::doc_prefix(coll).len();
    let mut n = 0;
    for key in targets {
        // Index keys come from the stored bytes: only indexed fields are read.
        let entries = match ctx.get_ref(&key) {
            Some(raw) => ix.keys_for(&key[prefix_len..], &Doc::from_stored(raw)?)?,
            None => continue,
        };
        for k in entries {
            ctx.delete(k);
        }
        ctx.delete(key);
        n += 1;
    }
    Ok(n)
}

/// Remove a collection, its documents and its indexes. Returns whether it existed.
pub fn drop(ctx: &mut Ctx, coll: &str) -> Result<bool> {
    check_name(coll)?;
    let meta = keys::coll_meta(coll);
    let existed = ctx.get_ref(&meta).is_some();
    for prefix in [keys::doc_prefix(coll), keys::index_owner_prefix(&owner(coll))] {
        let doomed: Vec<Vec<u8>> = ctx.iter_prefix(&prefix).map(|(k, _)| k.to_vec()).collect();
        for k in doomed {
            ctx.delete(k);
        }
    }
    ctx.delete(meta);
    ctx.delete(keys::seq(&owner(coll)));
    Ok(existed)
}

pub fn collections(ctx: &Ctx) -> Vec<String> {
    ctx.iter_prefix(keys::COLL_META_PREFIX)
        .map(|(k, _)| String::from_utf8_lossy(&k[keys::COLL_META_PREFIX.len()..]).into_owned())
        .collect()
}

/// MongoDB-style projection on top-level fields: `{a: 1, b: 1}` keeps only
/// those (plus `_id` unless `_id: 0`), `{a: 0}` removes them.
pub fn project(doc: Value, projection: &Value) -> Value {
    match Projection::parse(projection) {
        Ok(Some(p)) => p.apply(doc),
        _ => doc,
    }
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
    Pred::parse(filter)?.eval(doc)
}
