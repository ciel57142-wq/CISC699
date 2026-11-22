//! Index selection: turn a compiled filter (and sort) into an access path.
//!
//! Index ranges are always inclusive supersets of what the filter accepts
//! and every candidate is re-checked against the full predicate, so an
//! access path can only make a query faster, never change its result.
//! (This matters for values that share an index encoding, such as `-0.0`
//! and `0.0`, or integers above 2^53 that round to the same float.)

use super::filter::{CmpOp, Cond, Pred};
use super::{owner, IndexDef};
use crate::error::Result;
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use std::collections::BTreeMap;

/// `[start, end)` of store keys.
pub type KeyRange = (Vec<u8>, Vec<u8>);

/// Where the candidate documents come from.
#[derive(Debug)]
pub enum Access {
    /// Direct lookups by `_id` (encoded keys, ascending, no duplicates).
    Ids(Vec<Vec<u8>>),
    /// A range of the collection's own keys (`_id` order), `[start, end)`.
    IdRange { start: Vec<u8>, end: Vec<u8>, rev: bool },
    /// Ranges `[start, end)` of one secondary index, ascending, disjoint.
    Index { field: String, ranges: Vec<KeyRange>, rev: bool },
}

#[derive(Debug)]
pub struct Plan {
    pub access: Access,
    /// Candidates arrive in `_id` order (so no re-sorting for natural order,
    /// and a limit can stop the scan early).
    pub id_ordered: bool,
    /// Candidates arrive in the requested sort order. For an index this is
    /// order by encoded value: ties of distinct values with one encoding
    /// still need a (cheap, small) stable sort.
    pub sort_ordered: bool,
    /// Candidates come from a secondary index in sort order: equal-key runs
    /// must be completed before a limit is applied.
    pub index_ordered: bool,
    /// A document can be produced more than once (array values in an index).
    pub may_repeat: bool,
    /// The access path alone guarantees the filter (an `_id` lookup by
    /// integer or string): candidates need no predicate check.
    pub covered: bool,
}

/// The encoded doc-key form of a literal `_id`, when it can be looked up directly.
fn literal_id(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Int(_) | Value::Text(_) => keys::encode_id(v).ok(),
        Value::Object(o) if o.contains_key("$oid") && super::is_literal_object(o) => keys::encode_id(v).ok(),
        _ => None,
    }
}

fn after(mut k: Vec<u8>, byte: u8) -> Vec<u8> {
    k.push(byte);
    k
}

/// Key ranges of a field's index that contain every document satisfying
/// `conds`, with a selectivity class (lower is better), or None when the
/// index cannot narrow the search.
fn index_ranges(base: &[u8], nulls: bool, conds: &[&Cond]) -> Option<(u8, Vec<KeyRange>)> {
    let point = |v: &Value| -> Option<KeyRange> {
        match keys::encode_index_value(v) {
            Some(enc) => {
                let start = [base, &enc].concat();
                // index entries are enc + id, and id bytes are always < 0xff
                Some((start.clone(), after(start, 0xff)))
            }
            // null equality also matches missing fields and arrays holding null:
            // exactly the documents with an "other" entry
            None if v.is_null() && nulls => Some((after(base.to_vec(), keys::INDEX_OTHER), after(base.to_vec(), 1))),
            None => None,
        }
    };
    let mut best: Option<(u8, Vec<KeyRange>)> = None;
    let consider = |class: u8, ranges: Vec<KeyRange>, best: &mut Option<(u8, Vec<KeyRange>)>| {
        if best.as_ref().map_or(true, |(c, _)| class < *c) {
            *best = Some((class, ranges));
        }
    };
    let (mut lo, mut hi): (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
    let mut tag: Option<u8> = None;
    let mut mixed = false;
    for &c in conds {
        match c {
            Cond::Eq(v) => {
                if let Some(r) = point(v) {
                    consider(0, vec![r], &mut best);
                }
            }
            Cond::In(items) if !items.is_empty() => {
                if let Some(mut rs) = items.iter().map(point).collect::<Option<Vec<_>>>() {
                    rs.sort();
                    rs.dedup();
                    consider(1, rs, &mut best);
                }
            }
            Cond::Exists(false) if nulls => {
                consider(1, vec![(after(base.to_vec(), keys::INDEX_OTHER), after(base.to_vec(), 1))], &mut best)
            }
            Cond::Cmp(op, v) => {
                let Some(enc) = keys::encode_index_value(v) else {
                    mixed = true; // a comparison with null/array/object: leave it to the scan
                    continue;
                };
                if tag.map_or(false, |t| t != enc[0]) {
                    mixed = true;
                }
                tag = Some(enc[0]);
                let at = [base, &enc].concat();
                match op {
                    CmpOp::Gt | CmpOp::Gte => {
                        if lo.as_ref().map_or(true, |l| at > *l) {
                            lo = Some(at);
                        }
                    }
                    CmpOp::Lt | CmpOp::Lte => {
                        let end = after(at, 0xff);
                        if hi.as_ref().map_or(true, |h| end < *h) {
                            hi = Some(end);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if let (Some(t), false) = (tag, mixed) {
        // comparisons only match values of the same type: stay inside its tag
        let class = if lo.is_some() && hi.is_some() { 2 } else { 3 };
        let start = lo.unwrap_or_else(|| after(base.to_vec(), t));
        let end = hi.unwrap_or_else(|| after(base.to_vec(), t + 1));
        let ranges = if start < end { vec![(start, end)] } else { vec![] };
        consider(class, ranges, &mut best);
    }
    best
}

/// `_id` conditions: direct lookups or a key range.
fn id_access(doc_prefix: &[u8], conds: &[&Cond]) -> Option<(u8, Access)> {
    let mut lo: Option<Vec<u8>> = None;
    let mut hi: Option<Vec<u8>> = None;
    let mut tag: Option<u8> = None;
    let mut mixed = false;
    for &c in conds {
        match c {
            Cond::Eq(v) => {
                if let Some(k) = literal_id(v) {
                    return Some((0, Access::Ids(vec![[doc_prefix, &k].concat()])));
                }
            }
            Cond::In(items) if !items.is_empty() => {
                if let Some(mut ks) = items.iter().map(|v| literal_id(v).map(|k| [doc_prefix, &k].concat())).collect::<Option<Vec<_>>>() {
                    ks.sort();
                    ks.dedup();
                    return Some((1, Access::Ids(ks)));
                }
            }
            Cond::Cmp(op, v @ (Value::Int(_) | Value::Text(_))) => {
                let k = [doc_prefix, &keys::encode_id(v).ok()?].concat();
                if tag.map_or(false, |t| t != k[doc_prefix.len()]) {
                    mixed = true;
                }
                tag = Some(k[doc_prefix.len()]);
                match op {
                    CmpOp::Gt | CmpOp::Gte => {
                        if lo.as_ref().map_or(true, |l| k > *l) {
                            lo = Some(k);
                        }
                    }
                    CmpOp::Lt | CmpOp::Lte => {
                        let end = after(k, 0);
                        if hi.as_ref().map_or(true, |h| end < *h) {
                            hi = Some(end);
                        }
                    }
                }
            }
            Cond::Cmp(..) => mixed = true,
            _ => {}
        }
    }
    let t = tag.filter(|_| !mixed)?;
    let class = if lo.is_some() && hi.is_some() { 2 } else { 3 };
    let start = lo.unwrap_or_else(|| after(doc_prefix.to_vec(), t));
    let end = hi.unwrap_or_else(|| after(doc_prefix.to_vec(), t + 1));
    let end = if end < start { start.clone() } else { end };
    Some((class, Access::IdRange { start, end, rev: false }))
}

/// True when no document lacks an indexable scalar at the index path, so the
/// index holds every document exactly once and in value order.
fn index_is_total(ctx: &Ctx, base: &[u8]) -> bool {
    let other = after(base.to_vec(), keys::INDEX_OTHER);
    ctx.iter_range(base, &other, Some(&after(base.to_vec(), 1)), false).next().is_none()
}

pub fn plan(ctx: &Ctx, coll: &str, pred: &Pred, sort: &[(String, bool)], indexes: &[IndexDef]) -> Result<Plan> {
    let doc_prefix = keys::doc_prefix(coll);
    let mut best: Option<(u8, Access)> = None;
    let take = |class: u8, access: Access, best: &mut Option<(u8, Access)>| {
        if best.as_ref().map_or(true, |(c, _)| class < *c) {
            *best = Some((class, access));
        }
    };
    // Gather the conditions on each path (top level and $and), by reference.
    let mut by_path: Vec<(&str, Vec<&Cond>)> = Vec::new();
    for (path, conds) in pred.required_fields() {
        match by_path.iter_mut().find(|(p, _)| *p == path) {
            Some((_, cs)) => cs.extend(conds.iter()),
            None => by_path.push((path, conds.iter().collect())),
        }
    }
    let own = if indexes.is_empty() { String::new() } else { owner(coll) };
    for (path, conds) in &by_path {
        if *path == "_id" {
            if let Some((class, a)) = id_access(&doc_prefix, conds) {
                take(class, a, &mut best);
            }
            continue;
        }
        if let Some(ix) = indexes.iter().find(|i| i.path == *path) {
            let base = keys::index_prefix(&own, path);
            if let Some((class, ranges)) = index_ranges(&base, ix.nulls, conds) {
                // prefer _id access on ties
                take(class + 1, Access::Index { field: path.to_string(), ranges, rev: false }, &mut best);
            }
        }
    }
    let covered = match (pred, &best) {
        (Pred::Field { path, conds }, Some((_, Access::Ids(_)))) if path == "_id" && conds.len() == 1 => match &conds[0] {
            Cond::Eq(v) => matches!(v, Value::Int(_) | Value::Text(_)),
            Cond::In(items) => items.iter().all(|v| matches!(v, Value::Int(_) | Value::Text(_))),
            _ => false,
        },
        _ => false,
    };

    let sort_key = sort.first();
    let mut p = match best.map(|b| b.1) {
        Some(Access::Ids(ks)) => Plan { access: Access::Ids(ks), id_ordered: true, sort_ordered: false, index_ordered: false, may_repeat: false, covered },
        Some(a @ Access::IdRange { .. }) => Plan { access: a, id_ordered: true, sort_ordered: false, index_ordered: false, may_repeat: false, covered: false },
        Some(Access::Index { field, ranges, .. }) => {
            let base = keys::index_prefix(&own, &field);
            let nulls = indexes.iter().any(|i| i.path == field && i.nulls);
            let total = nulls && index_is_total(ctx, &base);
            let single_eq = ranges.len() == 1 && {
                // one equality value: entries are value + id, so id-ordered and unique
                let (s, e) = &ranges[0];
                e.len() == s.len() + 1 && e.ends_with(&[0xff]) && e[..s.len()] == s[..]
            };
            let sorted = total && sort_key.map_or(false, |(k, _)| *k == field);
            // Text and bool encodings are exact (unlike numbers, where 1 and
            // 1.0, -0.0 and 0.0, or ints past 2^53 share an encoding), and
            // index entries mirror equality's array-element rule: a lone
            // equality on such a value needs no re-check.
            let exact_eq = single_eq
                && matches!(pred, Pred::Field { path, conds } if *path == field && conds.len() == 1
                    && matches!(&conds[0], Cond::Eq(Value::Text(_) | Value::Bool(_))));
            Plan {
                access: Access::Index { field, ranges, rev: sorted && sort_key.map_or(false, |s| s.1) },
                id_ordered: single_eq && !sorted,
                sort_ordered: sorted,
                index_ordered: sorted,
                may_repeat: !total && !single_eq,
                covered: exact_eq,
            }
        }
        None => {
            // No index narrows the filter. An index on the sort key can still
            // deliver documents in order, if it holds every document.
            let ordered = sort_key.and_then(|(k, desc)| {
                let ix = indexes.iter().find(|i| i.path == *k && i.nulls)?;
                let base = keys::index_prefix(&own, &ix.path);
                index_is_total(ctx, &base).then(|| {
                    let ranges = vec![(after(base.clone(), 1), after(base, 0xff))];
                    Access::Index { field: k.clone(), ranges, rev: *desc }
                })
            });
            match ordered {
                Some(a) => Plan { access: a, id_ordered: false, sort_ordered: true, index_ordered: true, may_repeat: false, covered: false },
                None => Plan {
                    access: Access::IdRange { start: doc_prefix.clone(), end: keys::doc_prefix_end(coll), rev: false },
                    id_ordered: true,
                    sort_ordered: false,
                    index_ordered: false,
                    may_repeat: false,
                    covered: false,
                },
            }
        }
    };
    // `_id` is unique, so `_id` order is a total order: a sort on `_id`
    // (first key) is satisfied by walking the keys forwards or backwards.
    // (Every id-ordered access walks a single key range or a key list, so
    // reversing it gives descending `_id` order.)
    if let (Some((k, desc)), true) = (sort_key, p.id_ordered) {
        if k == "_id" {
            match &mut p.access {
                Access::IdRange { rev, .. } => *rev = *desc,
                Access::Ids(ks) if *desc => ks.reverse(),
                Access::Ids(_) => {}
                Access::Index { ranges, rev, .. } => {
                    debug_assert_eq!(ranges.len(), 1, "only a single equality range is id-ordered");
                    *rev = *desc;
                }
            }
            p.sort_ordered = true;
            p.id_ordered = !*desc;
        }
    }
    Ok(p)
}

/// A description of a plan, for `explain()`.
pub fn describe(p: &Plan, pred: &Pred, sort: &[(String, bool)]) -> Value {
    let mut o = BTreeMap::new();
    let (kind, extra): (&str, Vec<(&str, Value)>) = match &p.access {
        Access::Ids(ks) => ("id_lookup", vec![("keys", Value::Int(ks.len() as i64))]),
        Access::IdRange { start, end, .. } => {
            // the full scan runs from `d\0coll\0` to `d\0coll\x01`
            let full = start.len() == end.len() && start.ends_with(&[0]) && end.ends_with(&[1]) && start[..start.len() - 1] == end[..end.len() - 1];
            (if full { "collection_scan" } else { "id_range" }, vec![])
        }
        Access::Index { field, ranges, .. } => (
            "index_scan",
            vec![("index", Value::Text(field.clone())), ("ranges", Value::Int(ranges.len() as i64))],
        ),
    };
    o.insert("access".to_string(), Value::Text(kind.into()));
    for (k, v) in extra {
        o.insert(k.to_string(), v);
    }
    let rev = matches!(p.access, Access::IdRange { rev: true, .. } | Access::Index { rev: true, .. });
    o.insert("reverse".into(), Value::Bool(rev));
    o.insert("filter".into(), Value::Bool(!pred.is_true()));
    o.insert("sort".into(), Value::Text(if sort.is_empty() { "none" } else if p.sort_ordered { "from_access" } else { "in_memory" }.into()));
    Value::Object(o)
}
