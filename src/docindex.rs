//! Document indexes: single-field, compound and unique indexes on (nested)
//! field paths, kept as ordered entries in the shared keyspace.
//!
//! Entry layout: `i\0c:<coll>\0<field>\0 <enc(v1)> ... <enc(vn)> <doc id>`.
//! Every value encoding is self-delimiting and order-preserving, so an
//! index serves equality on leading fields, a range on the next field, and
//! ordered (forward or reverse) traversal for sorting.
//!
//! Indexes created by polydb 0.2 ("legacy": stored in metadata as a plain
//! field name) keep their original layout: one field, no entries for NULL or
//! missing values. Indexes created since also index NULL/missing (as NULL),
//! which lets them answer `field: null` and serve sorts over every document.

use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use std::collections::BTreeMap;
use std::ops::Bound;

#[derive(Debug, Clone, PartialEq)]
pub struct DocIndex {
    pub name: String,
    /// (field path, direction 1 / -1)
    pub fields: Vec<(String, i64)>,
    pub unique: bool,
    /// Storage field the entries live under.
    pub field: String,
    /// Original 0.2 layout (single field, NULL/missing not indexed).
    pub legacy: bool,
    /// Some document has an array in an indexed field (one entry per element).
    pub multikey: bool,
}

impl DocIndex {
    pub fn paths(&self) -> Vec<&str> {
        self.fields.iter().map(|(p, _)| p.as_str()).collect()
    }

    /// Key document as MongoDB shows it: {a: 1, b: -1}.
    pub fn key_doc(&self) -> Value {
        Value::Object(
            self.fields
                .iter()
                .map(|(p, d)| (p.clone(), Value::Int(*d)))
                .collect(),
        )
    }
}

pub fn default_name(fields: &[(String, i64)]) -> String {
    fields
        .iter()
        .map(|(p, d)| format!("{p}_{d}"))
        .collect::<Vec<_>>()
        .join("_")
}

fn owner(coll: &str) -> String {
    format!("c:{coll}")
}

fn decode_meta(raw: &[u8]) -> Result<Value> {
    let s = std::str::from_utf8(raw)
        .map_err(|_| Error::Corrupt("collection metadata is not UTF-8".into()))?;
    Value::parse(s).map_err(|e| Error::Corrupt(format!("collection metadata: {e}")))
}

/// Indexes of a collection (both layouts).
pub fn load(ctx: &Ctx, coll: &str) -> Result<Vec<DocIndex>> {
    let Some(raw) = ctx.get(&keys::coll_meta(coll)) else {
        return Ok(vec![]);
    };
    let meta = decode_meta(&raw)?;
    let mut out = Vec::new();
    if let Some(Value::Array(list)) = meta.get("indexes") {
        for ix in list {
            match ix {
                Value::Text(f) => out.push(DocIndex {
                    name: format!("{f}_1"),
                    fields: vec![(f.clone(), 1)],
                    unique: false,
                    field: f.clone(),
                    legacy: true,
                    multikey: true, // unknown for old indexes: never used for sorting
                }),
                Value::Object(o) => {
                    let bad =
                        || Error::Corrupt(format!("index metadata of '{coll}' is unreadable"));
                    let mut fields = Vec::new();
                    if let Some(Value::Array(k)) = o.get("key") {
                        for pair in k {
                            match pair {
                                Value::Array(p) if p.len() == 2 => fields.push((
                                    p[0].as_str().ok_or_else(bad)?.to_string(),
                                    p[1].as_i64().unwrap_or(1),
                                )),
                                _ => return Err(bad()),
                            }
                        }
                    }
                    out.push(DocIndex {
                        name: o
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or_else(bad)?
                            .to_string(),
                        fields,
                        unique: o.get("unique") == Some(&Value::Bool(true)),
                        field: o
                            .get("field")
                            .and_then(Value::as_str)
                            .ok_or_else(bad)?
                            .to_string(),
                        legacy: false,
                        multikey: o.get("multikey") == Some(&Value::Bool(true)),
                    });
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

pub fn save(ctx: &mut Ctx, coll: &str, indexes: &[DocIndex]) {
    let list = indexes
        .iter()
        .map(|ix| {
            if ix.legacy {
                return Value::Text(ix.field.clone());
            }
            let mut o = BTreeMap::new();
            o.insert("name".to_string(), Value::Text(ix.name.clone()));
            o.insert(
                "key".to_string(),
                Value::Array(
                    ix.fields
                        .iter()
                        .map(|(p, d)| Value::Array(vec![Value::Text(p.clone()), Value::Int(*d)]))
                        .collect(),
                ),
            );
            o.insert("unique".to_string(), Value::Bool(ix.unique));
            o.insert("field".to_string(), Value::Text(ix.field.clone()));
            o.insert("multikey".to_string(), Value::Bool(ix.multikey));
            Value::Object(o)
        })
        .collect();
    let mut o = BTreeMap::new();
    o.insert("indexes".to_string(), Value::Array(list));
    ctx.put(
        keys::coll_meta(coll),
        Value::Object(o).to_json().into_bytes(),
    );
}

/// Order-preserving, self-delimiting encoding of one indexed value.
/// NULL sorts first; objects (rare in indexes) compare by their JSON text.
pub fn encode(v: &Value) -> Vec<u8> {
    match v {
        Value::Null => vec![0],
        Value::Object(_) | Value::Array(_) => {
            let mut out = keys::encode_index_value(&Value::Text(v.to_json())).unwrap_or_default();
            out[0] = 5;
            out
        }
        other => keys::encode_index_value(other).unwrap_or_else(|| vec![0]),
    }
}

/// Encoding for an index layout (`None`: not indexed in that layout).
pub fn encode_for(ix: &DocIndex, v: &Value) -> Option<Vec<u8>> {
    if ix.legacy {
        keys::encode_index_value(v)
    } else {
        Some(encode(v))
    }
}

/// The value tuples a document contributes to an index (several when a
/// field holds an array). Errors on arrays in two fields of one index.
fn tuples(ix: &DocIndex, doc: &Value) -> Result<(Vec<Vec<Value>>, bool)> {
    let mut per_field: Vec<Vec<Value>> = Vec::with_capacity(ix.fields.len());
    let mut array_fields = 0;
    for (path, _) in &ix.fields {
        let vals = match crate::doc::get_path(doc, path) {
            None | Some(Value::Null) => vec![Value::Null],
            Some(Value::Array(a)) => {
                array_fields += 1;
                if a.is_empty() {
                    vec![Value::Null]
                } else {
                    let mut v: Vec<Value> = Vec::with_capacity(a.len());
                    for x in a {
                        if !v
                            .iter()
                            .any(|y| y.total_cmp(x) == std::cmp::Ordering::Equal)
                        {
                            v.push(x.clone());
                        }
                    }
                    v
                }
            }
            Some(v) => vec![v.clone()],
        };
        per_field.push(vals);
    }
    if array_fields > 1 {
        return Err(Error::Constraint(format!(
            "cannot index parallel arrays: document has arrays in more than one field of index '{}'",
            ix.name
        )));
    }
    let mut out: Vec<Vec<Value>> = vec![vec![]];
    for vals in per_field {
        let mut next = Vec::with_capacity(out.len() * vals.len());
        for prefix in &out {
            for v in &vals {
                let mut t = prefix.clone();
                t.push(v.clone());
                next.push(t);
            }
        }
        out = next;
    }
    Ok((out, array_fields > 0))
}

pub fn field_prefix(coll: &str, ix: &DocIndex) -> Vec<u8> {
    keys::index_prefix(&owner(coll), &ix.field)
}

fn tuple_prefix(coll: &str, ix: &DocIndex, vals: &[Value]) -> Option<Vec<u8>> {
    let mut k = field_prefix(coll, ix);
    for v in vals {
        k.extend(encode_for(ix, v)?);
    }
    Some(k)
}

/// Index entries for one document, and whether it made the index multikey.
pub fn entries(
    coll: &str,
    ix: &DocIndex,
    doc_key: &[u8],
    doc: &Value,
) -> Result<(Vec<Vec<u8>>, bool)> {
    let id = &doc_key[keys::doc_prefix(coll).len()..];
    let (ts, multi) = tuples(ix, doc)?;
    let mut out = Vec::with_capacity(ts.len());
    for t in ts {
        if let Some(mut k) = tuple_prefix(coll, ix, &t) {
            k.extend_from_slice(id);
            out.push(k);
        }
    }
    Ok((out, multi))
}

/// The document key an index entry points at.
pub fn doc_key_of(coll: &str, ix: &DocIndex, entry: &[u8]) -> Vec<u8> {
    let mut rest = &entry[field_prefix(coll, ix).len()..];
    for _ in 0..ix.fields.len() {
        let n = crate::rel::encoded_len(rest);
        rest = &rest[n.min(rest.len())..];
    }
    let mut k = keys::doc_prefix(coll);
    k.extend_from_slice(rest);
    k
}

/// Add a document's entries to every index, enforcing UNIQUE. Returns the
/// indexes whose multikey flag must be persisted.
pub fn add(
    ctx: &mut Ctx,
    coll: &str,
    indexes: &mut [DocIndex],
    doc_key: &[u8],
    doc: &Value,
) -> Result<bool> {
    let mut meta_changed = false;
    for ix in indexes.iter_mut() {
        let (es, multi) = entries(coll, ix, doc_key, doc)?;
        if ix.unique {
            let (ts, _) = tuples(ix, doc)?;
            for t in ts {
                let Some(p) = tuple_prefix(coll, ix, &t) else {
                    continue;
                };
                let hi =
                    crate::storage::prefix_successor(&p).map_or(Bound::Unbounded, Bound::Excluded);
                for (e, _) in ctx.iter_range(Bound::Included(p.clone()), hi, false) {
                    if doc_key_of(coll, ix, &e) != doc_key {
                        let shown: Vec<String> = t.iter().map(Value::to_json).collect();
                        return Err(Error::Constraint(format!(
                            "duplicate key ({}) for unique index '{}' on collection '{coll}'",
                            shown.join(", "),
                            ix.name
                        )));
                    }
                }
            }
        }
        for e in es {
            ctx.put(e, Vec::new());
        }
        if multi && !ix.multikey && !ix.legacy {
            ix.multikey = true;
            meta_changed = true;
        }
    }
    Ok(meta_changed)
}

pub fn remove(ctx: &mut Ctx, coll: &str, indexes: &[DocIndex], doc_key: &[u8], doc: &Value) {
    for ix in indexes {
        // a stored document always produced valid entries; ignore the parallel-array case
        if let Ok((es, _)) = entries(coll, ix, doc_key, doc) {
            for e in es {
                ctx.delete(e);
            }
        }
    }
}

/// Build (or rebuild) every entry of one index from the stored documents.
pub fn build(ctx: &mut Ctx, coll: &str, ix: &mut DocIndex) -> Result<()> {
    let docs: Vec<(Vec<u8>, Vec<u8>)> = ctx.scan(&keys::doc_prefix(coll));
    let mut one = vec![ix.clone()];
    for (k, raw) in docs {
        let d = crate::doc::decode(&raw)?;
        add(ctx, coll, &mut one, &k, &d)?;
    }
    *ix = one.remove(0);
    Ok(())
}

pub fn drop_entries(ctx: &mut Ctx, coll: &str, ix: &DocIndex) {
    for (k, _) in ctx.scan(&field_prefix(coll, ix)) {
        ctx.delete(k);
    }
}

/// Create an index. Returns false if an index with the same key already exists.
pub fn create(
    ctx: &mut Ctx,
    coll: &str,
    fields: Vec<(String, i64)>,
    unique: bool,
    name: Option<String>,
) -> Result<bool> {
    if fields.is_empty() {
        return Err(Error::Schema("an index needs at least one field".into()));
    }
    for (f, d) in &fields {
        if f.is_empty()
            || f.contains('\0')
            || f.contains('\u{1}')
            || f.split('.').any(str::is_empty)
        {
            return Err(Error::Schema(format!("cannot index field '{f}'")));
        }
        if *d != 1 && *d != -1 {
            return Err(Error::Schema(format!(
                "index direction for '{f}' must be 1 or -1"
            )));
        }
    }
    if fields.len() == 1 && fields[0].0 == "_id" {
        return Ok(false); // documents are already keyed by _id
    }
    let mut all = load(ctx, coll)?;
    if let Some(existing) = all.iter().find(|i| i.fields == fields) {
        if existing.unique != unique {
            return Err(Error::Schema(format!(
                "an index on these fields already exists: '{}'",
                existing.name
            )));
        }
        return Ok(false);
    }
    let name = name.unwrap_or_else(|| default_name(&fields));
    if all.iter().any(|i| i.name == name) {
        return Err(Error::Schema(format!(
            "index '{name}' already exists on collection '{coll}'"
        )));
    }
    if ctx.get(&keys::coll_meta(coll)).is_none() {
        save(ctx, coll, &[]); // creating an index creates the collection
    }
    let mut ix = DocIndex {
        field: format!("{name}\u{1}"),
        name,
        fields,
        unique,
        legacy: false,
        multikey: false,
    };
    build(ctx, coll, &mut ix)?;
    all.push(ix);
    save(ctx, coll, &all);
    Ok(true)
}

/// Drop an index by name (or by key document). Returns whether it existed.
pub fn drop(ctx: &mut Ctx, coll: &str, name: &str) -> Result<bool> {
    let mut all = load(ctx, coll)?;
    let Some(pos) = all
        .iter()
        .position(|i| i.name == name || (i.legacy && i.field == name))
    else {
        return Ok(false);
    };
    let ix = all.remove(pos);
    drop_entries(ctx, coll, &ix);
    save(ctx, coll, &all);
    Ok(true)
}

/// Compare every index with the entries its documents should produce.
/// Returns a list of problems (empty when consistent).
pub fn validate(ctx: &Ctx, coll: &str) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    let docs: Vec<(Vec<u8>, Value)> = ctx
        .scan(&keys::doc_prefix(coll))
        .into_iter()
        .map(|(k, raw)| Ok((k, crate::doc::decode(&raw)?)))
        .collect::<Result<_>>()?;
    for ix in load(ctx, coll)? {
        let mut expected = std::collections::BTreeSet::new();
        for (k, d) in &docs {
            match entries(coll, &ix, k, d) {
                Ok((es, _)) => expected.extend(es),
                Err(e) => problems.push(format!("index '{}': {e}", ix.name)),
            }
        }
        let actual: std::collections::BTreeSet<Vec<u8>> = ctx
            .scan(&field_prefix(coll, &ix))
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        let missing = expected.difference(&actual).count();
        let extra = actual.difference(&expected).count();
        if missing > 0 || extra > 0 {
            problems.push(format!(
                "index '{}': {missing} missing and {extra} stray entries",
                ix.name
            ));
        }
    }
    Ok(problems)
}

/// Drop and rebuild every index of a collection.
pub fn reindex(ctx: &mut Ctx, coll: &str) -> Result<usize> {
    let mut all = load(ctx, coll)?;
    for ix in all.iter_mut() {
        drop_entries(ctx, coll, ix);
        if !ix.legacy {
            ix.multikey = false;
        }
        build(ctx, coll, ix)?;
    }
    save(ctx, coll, &all);
    Ok(all.len())
}

/// A chosen index access: equality values on leading fields, an optional
/// range on the next one.
#[derive(Debug, Clone)]
pub struct Choice<T> {
    pub index: DocIndex,
    pub eq: Vec<T>,
    pub lo: Option<(T, bool)>,
    pub hi: Option<(T, bool)>,
}

/// Pick the best index for the predicates: a unique index fully pinned by
/// equality, then the longest equality prefix (plus a range), then a range.
/// `eq(path)` / `range(path)` describe the predicates on a field; `usable`
/// rejects values an index layout cannot answer for (e.g. NULL on legacy).
pub fn choose<T: Clone>(
    indexes: &[DocIndex],
    eq: &dyn Fn(&str) -> Option<T>,
    range: &dyn Fn(&str) -> (Option<(T, bool)>, Option<(T, bool)>),
    usable: &dyn Fn(&DocIndex, &T) -> bool,
) -> Option<Choice<T>> {
    let mut best: Option<(usize, Choice<T>)> = None;
    for ix in indexes {
        let mut eqs = Vec::new();
        for (p, _) in &ix.fields {
            match eq(p).filter(|v| usable(ix, v)) {
                Some(v) => eqs.push(v),
                None => break,
            }
        }
        let k = eqs.len();
        let (lo, hi) = match ix.fields.get(k) {
            Some((p, _)) => {
                let (l, h) = range(p);
                (
                    l.filter(|v| usable(ix, &v.0)),
                    h.filter(|v| usable(ix, &v.0)),
                )
            }
            None => (None, None),
        };
        let score = if k == ix.fields.len() {
            if ix.unique {
                100
            } else {
                80 + k
            }
        } else if k > 0 {
            50 + 2 * k + usize::from(lo.is_some() || hi.is_some())
        } else if lo.is_some() || hi.is_some() {
            40
        } else {
            continue;
        };
        if best.as_ref().map_or(true, |b| b.0 < score) {
            best = Some((
                score,
                Choice {
                    index: ix.clone(),
                    eq: eqs,
                    lo,
                    hi,
                },
            ));
        }
    }
    best.map(|b| b.1)
}

/// Key range for a chosen access with concrete values. Bounds are inclusive
/// (the caller re-applies the filter) and a range stays within the value's
/// type. `None` if the values cannot be encoded for this index.
pub fn key_range(coll: &str, c: &Choice<Value>) -> Option<(Bound<Vec<u8>>, Bound<Vec<u8>>)> {
    let prefix = tuple_prefix(coll, &c.index, &c.eq)?;
    let succ =
        |p: &[u8]| crate::storage::prefix_successor(p).map_or(Bound::Unbounded, Bound::Excluded);
    if c.lo.is_none() && c.hi.is_none() {
        return Some((Bound::Included(prefix.clone()), succ(&prefix)));
    }
    let enc = |v: &Value| encode_for(&c.index, v);
    let lo_e = match &c.lo {
        Some((v, _)) => Some(enc(v)?),
        None => None,
    };
    let hi_e = match &c.hi {
        Some((v, _)) => Some(enc(v)?),
        None => None,
    };
    let tag = lo_e.as_ref().or(hi_e.as_ref())?[0];
    if hi_e.as_ref().is_some_and(|h| h[0] != tag) || lo_e.as_ref().is_some_and(|l| l[0] != tag) {
        return Some((Bound::Included(prefix.clone()), Bound::Excluded(prefix)));
        // empty: incomparable types
    }
    let lo = match lo_e {
        Some(e) => [prefix.as_slice(), &e].concat(),
        None => [prefix.as_slice(), &[tag]].concat(),
    };
    let hi = match hi_e {
        Some(e) => succ(&[prefix.as_slice(), &e].concat()),
        None => Bound::Excluded([prefix.as_slice(), &[tag + 1]].concat()),
    };
    Some((Bound::Included(lo), hi))
}
