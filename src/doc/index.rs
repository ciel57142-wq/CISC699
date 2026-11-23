//! Secondary indexes on document collections, and the access-path planner.
//!
//! Entry layout: `index_prefix(owner, name) | enc(v1) .. enc(vn) | id`,
//! with an empty value. `name` is the field path for a single-field index
//! (the original on-disk format) and `\x01f1\x01f2..` for a compound one.
//! Each `enc` is the self-delimiting order-preserving encoding from
//! [`keys::encode_index_value`]; only booleans, numbers and strings are
//! indexable. A document gets one entry per indexable value its path
//! reaches (array elements included). In a compound index a field with no
//! indexable value is stored as the one-byte [`ABSENT`] marker so the
//! document is still reachable through the other fields; a single-field
//! index simply has no entry for such a document.

use super::filter::{CmpOp, Cond, Filter};
use super::path::{path_values, validate_path};
use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Encoded stand-in for "no indexable value" in compound entries. It sorts
/// before every real encoding (whose type tags are 1, 2 and 4).
pub const ABSENT: u8 = 0;

/// The most point combinations a compound lookup expands to before the
/// planner falls back to fewer key fields.
const MAX_POINTS: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub struct IndexDef {
    pub fields: Vec<String>,
    pub unique: bool,
    /// Fields for which some document has produced more than one key value
    /// (an array, or a path through an array). On such a field, different
    /// values may satisfy different conditions of one filter, so the planner
    /// must not intersect a lower and an upper bound into a single range.
    pub multikey: Vec<String>,
}

pub fn owner(coll: &str) -> String {
    format!("c:{coll}")
}

impl IndexDef {
    pub fn new(fields: Vec<String>, unique: bool) -> Result<IndexDef> {
        if fields.is_empty() {
            return Err(Error::Schema("an index needs at least one field".into()));
        }
        if fields.len() > 16 {
            return Err(Error::Schema("an index may have at most 16 fields".into()));
        }
        for (i, f) in fields.iter().enumerate() {
            validate_path(f).map_err(|e| Error::Schema(format!("cannot index field '{f}': {e}")))?;
            if f.contains('\u{1}') {
                return Err(Error::Schema(format!("cannot index field '{f}'")));
            }
            if fields[..i].contains(f) {
                return Err(Error::Schema(format!("field '{f}' appears twice in the index")));
            }
        }
        if fields.len() == 1 && fields[0] == "_id" {
            return Err(Error::Schema("_id is already the primary key and needs no index".into()));
        }
        Ok(IndexDef { fields, unique, multikey: vec![] })
    }

    /// Same key fields and options (ignoring the multikey bookkeeping).
    pub fn same_as(&self, other: &IndexDef) -> bool {
        self.fields == other.fields && self.unique == other.unique
    }

    pub fn is_multikey(&self, field: &str) -> bool {
        self.multikey.iter().any(|f| f == field)
    }

    /// Record the fields that held several values in a document (from
    /// [`IndexDef::analyze`]). Returns true if anything new was recorded.
    pub fn note_multikey(&mut self, multi: &[bool]) -> bool {
        let mut changed = false;
        for (f, m) in self.fields.iter().zip(multi) {
            if *m && !self.multikey.contains(f) {
                self.multikey.push(f.clone());
                changed = true;
            }
        }
        changed
    }

    /// Display name in the `field_1_other_1` style.
    pub fn name(&self) -> String {
        self.fields.iter().map(|f| format!("{f}_1")).collect::<Vec<_>>().join("_")
    }

    fn storage_name(&self) -> String {
        if self.fields.len() == 1 {
            self.fields[0].clone()
        } else {
            self.fields.iter().map(|f| format!("\u{1}{f}")).collect()
        }
    }

    pub fn prefix(&self, coll: &str) -> Vec<u8> {
        keys::index_prefix(&owner(coll), &self.storage_name())
    }

    pub fn to_meta(&self) -> Value {
        let mut o = BTreeMap::new();
        o.insert("fields".into(), Value::Array(self.fields.iter().map(|f| Value::Text(f.clone())).collect()));
        o.insert("unique".into(), Value::Bool(self.unique));
        if !self.multikey.is_empty() {
            o.insert("multikey".into(), Value::Array(self.multikey.iter().map(|f| Value::Text(f.clone())).collect()));
        }
        Value::Object(o)
    }

    /// Read one entry of the collection's index list: a bare field name
    /// (the format written before compound indexes existed) or an object.
    pub fn from_meta(v: &Value) -> Result<IndexDef> {
        let bad = || Error::Corrupt(format!("bad index definition {}", v.to_json()));
        match v {
            // Old entries carry no multikey information, so assume every field may be.
            Value::Text(f) => Ok(IndexDef { fields: vec![f.clone()], unique: false, multikey: vec![f.clone()] }),
            Value::Object(_) => {
                let Some(Value::Array(fs)) = v.get("fields") else { return Err(bad()) };
                let fields = fs.iter().map(|f| f.as_str().map(String::from).ok_or_else(bad)).collect::<Result<_>>()?;
                let multikey = match v.get("multikey") {
                    None => vec![],
                    Some(Value::Array(m)) => m.iter().map(|f| f.as_str().map(String::from).ok_or_else(bad)).collect::<Result<_>>()?,
                    Some(_) => return Err(bad()),
                };
                Ok(IndexDef { fields, unique: v.get("unique") == Some(&Value::Bool(true)), multikey })
            }
            _ => Err(bad()),
        }
    }

    /// The encoded key parts (everything between prefix and id) this
    /// document contributes, deduplicated.
    pub fn key_parts(&self, doc: &Value) -> Result<Vec<Vec<u8>>> {
        Ok(self.analyze(doc)?.0)
    }

    /// Key parts plus, per field, whether the document holds several values there.
    pub fn analyze(&self, doc: &Value) -> Result<(Vec<Vec<u8>>, Vec<bool>)> {
        let per_field: Vec<BTreeSet<Vec<u8>>> = self.fields.iter().map(|f| indexable(doc, f)).collect();
        let multi: Vec<bool> = per_field.iter().map(|s| s.len() > 1).collect();
        if self.fields.len() == 1 {
            return Ok((per_field.into_iter().next().unwrap_or_default().into_iter().collect(), multi));
        }
        if per_field.iter().filter(|s| s.len() > 1).count() > 1 {
            return Err(Error::Constraint(format!(
                "compound index {} cannot index a document where more than one of its fields holds several values (parallel arrays)",
                self.name()
            )));
        }
        let mut out: Vec<Vec<u8>> = vec![vec![]];
        for set in per_field {
            if set.is_empty() {
                out.iter_mut().for_each(|k| k.push(ABSENT));
            } else {
                out = out.into_iter().flat_map(|k| set.iter().map(move |e| [k.as_slice(), e].concat())).collect();
            }
        }
        Ok((out, multi))
    }

    /// Full index keys for a document stored under `id` (encoded id bytes).
    pub fn entries(&self, coll: &str, doc: &Value, id: &[u8]) -> Result<Vec<Vec<u8>>> {
        let prefix = self.prefix(coll);
        Ok(self.key_parts(doc)?.into_iter().map(|p| [prefix.as_slice(), &p, id].concat()).collect())
    }

    /// For a unique index, fail if another document already holds one of
    /// these key parts. A compound key with an absent field is not checked.
    pub fn check_unique(&self, ctx: &Ctx, coll: &str, parts: &[Vec<u8>], id: &[u8]) -> Result<()> {
        if !self.unique {
            return Ok(());
        }
        let prefix = self.prefix(coll);
        for p in parts {
            if self.fields.len() > 1 && has_absent(p, self.fields.len()) {
                continue;
            }
            let full = [prefix.as_slice(), p].concat();
            if ctx.iter_prefix(&full).any(|(k, _)| &k[full.len()..] != id) {
                return Err(Error::Constraint(format!(
                    "duplicate key for unique index {} on collection '{coll}'",
                    self.name()
                )));
            }
        }
        Ok(())
    }

    /// Split an entry key (after the index prefix) into the document id bytes.
    pub fn id_of<'k>(&self, rest: &'k [u8]) -> Option<&'k [u8]> {
        let mut pos = 0;
        for _ in 0..self.fields.len() {
            pos += enc_len(&rest[pos..])?;
        }
        rest.get(pos..)
    }
}

pub(crate) fn has_absent(parts: &[u8], n: usize) -> bool {
    let mut pos = 0;
    for _ in 0..n {
        if parts.get(pos) == Some(&ABSENT) {
            return true;
        }
        match enc_len(&parts[pos..]) {
            Some(l) => pos += l,
            None => return false,
        }
    }
    false
}

/// Encodings of every indexable value a path reaches (array elements included).
fn indexable(doc: &Value, path: &str) -> BTreeSet<Vec<u8>> {
    let mut out = BTreeSet::new();
    for v in path_values(doc, path) {
        match v {
            Value::Array(a) => out.extend(a.iter().filter_map(keys::encode_index_value)),
            v => out.extend(keys::encode_index_value(v)),
        }
    }
    out
}

/// Length of one encoded value at the start of `b`.
fn enc_len(b: &[u8]) -> Option<usize> {
    match *b.first()? {
        ABSENT => Some(1),
        1 => Some(2),
        2 => Some(9),
        4 => {
            let mut i = 1;
            loop {
                if *b.get(i)? == 0 {
                    if *b.get(i + 1)? == 0 {
                        return Some(i + 2);
                    }
                    i += 2; // escaped NUL inside the string
                } else {
                    i += 1;
                }
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- planner

/// What a filter lets an access path narrow on, for one field.
#[derive(Default, Debug)]
struct FieldPred {
    /// Equality / `$in` points (encoded), when every operand is indexable.
    points: Option<Vec<Vec<u8>>>,
    /// Range bounds within one type tag: (tag, lowest lower bound, highest upper bound).
    range: Option<(u8, Option<Vec<u8>>, Option<Vec<u8>>)>,
}

fn field_preds(filter: &Filter) -> BTreeMap<&str, FieldPred> {
    let mut preds: BTreeMap<&str, FieldPred> = BTreeMap::new();
    let mut tags: BTreeMap<&str, Option<u8>> = BTreeMap::new();
    for (path, cond) in filter.conjuncts() {
        let p = preds.entry(path).or_default();
        match cond {
            Cond::Eq(v) => {
                if let Some(e) = keys::encode_index_value(v) {
                    p.points = Some(vec![e]);
                }
            }
            Cond::In(items) if !items.is_empty() && p.points.is_none() => {
                let encs: Option<BTreeSet<Vec<u8>>> = items.iter().map(keys::encode_index_value).collect();
                p.points = encs.map(|s| s.into_iter().collect());
            }
            Cond::Cmp(op, v) => {
                let Some(e) = keys::encode_index_value(v) else { continue };
                let tag = e[0];
                let t = tags.entry(path).or_insert(Some(tag));
                if *t != Some(tag) {
                    *t = None; // mixed types: no single range
                    p.range = None;
                    continue;
                }
                let (_, lo, hi) = p.range.get_or_insert((tag, None, None));
                match op {
                    CmpOp::Gt | CmpOp::Gte => {
                        if lo.as_ref().map_or(true, |l| &e > l) {
                            *lo = Some(e)
                        }
                    }
                    CmpOp::Lt | CmpOp::Lte => {
                        if hi.as_ref().map_or(true, |h| &e < h) {
                            *hi = Some(e)
                        }
                    }
                }
            }
            _ => {}
        }
    }
    preds
}

#[derive(Debug)]
pub enum Plan {
    /// Read exactly these documents by `_id`.
    Ids(Vec<Value>),
    /// Scan `[start, end)` key ranges of one index.
    Index { index: IndexDef, eq_fields: usize, range: bool, ranges: Vec<(Vec<u8>, Vec<u8>)> },
    /// Read the whole collection.
    Scan,
}

impl Plan {
    pub fn describe(&self) -> Value {
        let mut o = BTreeMap::new();
        match self {
            Plan::Ids(ids) => {
                o.insert("access".into(), Value::Text("id_lookup".into()));
                o.insert("ids".into(), Value::Int(ids.len() as i64));
            }
            Plan::Index { index, eq_fields, range, ranges } => {
                o.insert("access".into(), Value::Text("index_scan".into()));
                o.insert("index".into(), Value::Text(index.name()));
                o.insert("equalityFields".into(), Value::Int(*eq_fields as i64));
                o.insert("range".into(), Value::Bool(*range));
                o.insert("keyRanges".into(), Value::Int(ranges.len() as i64));
            }
            Plan::Scan => {
                o.insert("access".into(), Value::Text("collection_scan".into()));
            }
        }
        Value::Object(o)
    }
}

fn is_id_value(v: &Value) -> bool {
    keys::encode_id(v).is_ok()
}

/// Choose how to find candidate documents. The chosen path only narrows
/// the documents read; the caller still evaluates the full filter.
pub fn plan(coll: &str, filter: &Filter, indexes: &[IndexDef]) -> Plan {
    for (path, cond) in filter.conjuncts() {
        if path != "_id" {
            continue;
        }
        match cond {
            Cond::Eq(v) if is_id_value(v) => return Plan::Ids(vec![v.clone()]),
            Cond::In(items) if items.iter().all(is_id_value) => {
                let mut ids: Vec<Value> = Vec::new();
                for it in items {
                    if !ids.iter().any(|x| x == it) {
                        ids.push(it.clone());
                    }
                }
                return Plan::Ids(ids);
            }
            _ => {}
        }
    }
    let preds = field_preds(filter);
    let mut best: Option<(usize, bool, &IndexDef, Vec<Vec<u8>>)> = None;
    for ix in indexes {
        let mut combos: Vec<Vec<u8>> = vec![vec![]];
        let mut eq = 0;
        for f in &ix.fields {
            let Some(points) = preds.get(f.as_str()).and_then(|p| p.points.as_ref()) else { break };
            if combos.len() * points.len() > MAX_POINTS {
                break;
            }
            combos = combos.into_iter().flat_map(|c| points.iter().map(move |p| [c.as_slice(), p].concat())).collect();
            eq += 1;
        }
        let range = eq < ix.fields.len() && preds.get(ix.fields[eq].as_str()).map_or(false, |p| p.range.is_some());
        if eq == 0 && !range {
            continue;
        }
        let better = match &best {
            None => true,
            Some((be, br, _, _)) => (eq, range) > (*be, *br),
        };
        if better {
            best = Some((eq, range, ix, combos));
        }
    }
    let Some((eq, range, ix, combos)) = best else { return Plan::Scan };
    let prefix = ix.prefix(coll);
    let mut ranges = Vec::new();
    for c in combos {
        let base = [prefix.as_slice(), &c].concat();
        if range {
            let (tag, lo, mut hi) = preds[ix.fields[eq].as_str()].range.clone().expect("range checked above");
            if ix.is_multikey(&ix.fields[eq]) && lo.is_some() && hi.is_some() {
                // One value may meet the lower bound and another the upper.
                hi = None;
            }
            let start = [base.as_slice(), &lo.unwrap_or_else(|| vec![tag])].concat();
            // Bounds are scanned inclusively; the filter applies strictness.
            let end = match hi {
                Some(h) => [base.as_slice(), &h, &[0xff]].concat(),
                None => [base.as_slice(), &[tag + 1]].concat(),
            };
            ranges.push((start, end));
        } else {
            // Everything after the points is another encoding or the id;
            // both start with a byte below 0xff.
            let end = [base.as_slice(), &[0xff]].concat();
            ranges.push((base, end));
        }
    }
    Plan::Index { index: ix.clone(), eq_fields: eq, range, ranges }
}
