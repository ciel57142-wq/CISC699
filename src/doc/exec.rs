//! Query execution: document retrieval, predicate evaluation, sorting,
//! skip/limit and projection.
//!
//! Candidates are borrowed straight from the store. The predicate and the
//! sort keys read individual fields from the stored bytes, so a document is
//! decoded only once it is known to be part of the result (and then only
//! the projected fields).

use super::codec::{Doc, Fields};
use super::filter::Pred;
use super::plan::{Access, KeyRange, Plan};
use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::HashSet;

/// Visit candidate documents in plan order: `f(doc key, stored bytes,
/// encoded index value)` returns false to stop.
pub fn for_each_candidate<'c>(
    ctx: &'c Ctx,
    coll: &str,
    plan: &Plan,
    mut f: impl FnMut(&[u8], &'c [u8], Option<&[u8]>) -> Result<bool>,
) -> Result<()> {
    let doc_prefix = keys::doc_prefix(coll);
    match &plan.access {
        Access::Ids(ks) => {
            for k in ks {
                if let Some(raw) = ctx.get_ref(k) {
                    if !f(k, raw, None)? {
                        break;
                    }
                }
            }
        }
        Access::IdRange { start, end, rev } => {
            for (k, raw) in ctx.iter_range(&doc_prefix, start, Some(end), *rev) {
                if !f(k, raw, None)? {
                    break;
                }
            }
        }
        Access::Index { field, ranges, rev } => {
            let base = keys::index_prefix(&super::owner(coll), field);
            let mut seen: HashSet<Vec<u8>> = HashSet::new();
            let mut dk = doc_prefix.clone();
            let ordered: Box<dyn Iterator<Item = &KeyRange>> =
                if *rev { Box::new(ranges.iter().rev()) } else { Box::new(ranges.iter()) };
            'outer: for (start, end) in ordered {
                for (k, _) in ctx.iter_range(&base, start, Some(end), *rev) {
                    let rest = &k[base.len()..];
                    let Some(n) = keys::index_value_len(rest) else {
                        return Err(Error::Corrupt("malformed document index entry".into()));
                    };
                    let (enc, id) = rest.split_at(n);
                    if plan.may_repeat && !seen.insert(id.to_vec()) {
                        continue;
                    }
                    dk.truncate(doc_prefix.len());
                    dk.extend_from_slice(id);
                    // An entry without its document is reported by validate(); skip it here.
                    if let Some(raw) = ctx.get_ref(&dk) {
                        if !f(&dk, raw, Some(enc))? {
                            break 'outer;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Top-level projection, MongoDB style: `{a: 1, b: 1}` keeps only those
/// fields plus `_id` (unless `_id: 0`); `{a: 0}` drops the listed fields.
#[derive(Debug, Clone)]
pub enum Projection {
    Include { fields: Vec<String>, id: bool },
    Exclude(Vec<String>),
}

impl Projection {
    /// None for an empty projection (whole documents).
    pub fn parse(p: &Value) -> Result<Option<Projection>> {
        let Value::Object(p) = p else {
            return Err(Error::Type("a projection must be an object like {\"field\": 1}".into()));
        };
        if p.is_empty() {
            return Ok(None);
        }
        let on = |v: &Value| !matches!(v, Value::Bool(false) | Value::Int(0) | Value::Null) && v.as_f64() != Some(0.0);
        let include = p.iter().any(|(k, v)| k != "_id" && on(v)) || (p.len() == 1 && p.get("_id").map_or(false, on));
        Ok(Some(if include {
            Projection::Include {
                fields: p.iter().filter(|(k, v)| *k != "_id" && on(v)).map(|(k, _)| k.clone()).collect(),
                id: p.get("_id").map_or(true, on),
            }
        } else {
            Projection::Exclude(p.iter().filter(|(_, v)| !on(v)).map(|(k, _)| k.clone()).collect())
        }))
    }

    pub fn keeps(&self, key: &str) -> bool {
        match self {
            Projection::Include { fields, id } => {
                if key == "_id" {
                    *id
                } else {
                    fields.iter().any(|f| f == key)
                }
            }
            Projection::Exclude(fields) => !fields.iter().any(|f| f == key),
        }
    }

    pub fn apply(&self, doc: Value) -> Value {
        match doc {
            Value::Object(mut o) => {
                o.retain(|k, _| self.keeps(k));
                Value::Object(o)
            }
            other => other,
        }
    }
}

pub struct Query {
    pub pred: Pred,
    /// (path, descending)
    pub sort: Vec<(String, bool)>,
    pub skip: usize,
    pub limit: Option<usize>,
    pub projection: Option<Projection>,
}

pub fn parse_sort(sort: Option<&Value>) -> Result<Vec<(String, bool)>> {
    match sort {
        None => Ok(vec![]),
        Some(Value::Object(spec)) => Ok(spec.iter().map(|(k, v)| (k.clone(), v.as_f64().map_or(false, |n| n < 0.0))).collect()),
        Some(_) => Err(Error::Type("sort must be an object like {\"field\": 1}".into())),
    }
}

fn sort_keys(doc: &dyn Fields, sort: &[(String, bool)]) -> Result<Vec<Value>> {
    sort.iter().map(|(p, _)| Ok(doc.field(p)?.map(|c| c.into_owned()).unwrap_or(Value::Null))).collect()
}

pub fn cmp_keys(a: &[Value], b: &[Value], sort: &[(String, bool)]) -> Ordering {
    for ((x, y), (_, desc)) in a.iter().zip(b).zip(sort) {
        let o = x.total_cmp(y);
        if o != Ordering::Equal {
            return if *desc { o.reverse() } else { o };
        }
    }
    Ordering::Equal
}

fn materialize(raw: &[u8], projection: &Option<Projection>) -> Result<Value> {
    let d = Doc::from_stored(raw)?;
    match projection {
        None => d.to_value(),
        Some(p) => d.to_value_where(|k| p.keeps(k)),
    }
}

/// Matching documents' stored bytes, in result order, after skip and limit.
pub fn select<'c>(ctx: &'c Ctx, coll: &str, plan: &Plan, q: &Query) -> Result<Vec<&'c [u8]>> {
    let want = q.limit.map(|l| l.saturating_add(q.skip));
    if want == Some(0) {
        return Ok(vec![]);
    }
    let check = |raw: &[u8]| -> Result<bool> {
        if plan.covered || q.pred.is_true() {
            return Ok(true);
        }
        q.pred.eval(&Doc::from_stored(raw)?)
    };

    // Candidates already in result order: stream, stop at the limit.
    let streaming = if q.sort.is_empty() { plan.id_ordered } else { plan.sort_ordered };
    if streaming {
        // (stored bytes, encoded index value, doc key) - the last two only from an index
        type Hit<'r> = (&'r [u8], Option<Vec<u8>>, Vec<u8>);
        let mut out: Vec<Hit> = Vec::new();
        for_each_candidate(ctx, coll, plan, |k, raw, enc| {
            if let (Some(w), Some(last)) = (want, out.last()) {
                if out.len() >= w {
                    // From an index, finish the run of equal encoded values first.
                    if !plan.index_ordered || last.1.as_deref() != enc {
                        return Ok(false);
                    }
                }
            }
            if check(raw)? {
                if plan.index_ordered {
                    out.push((raw, Some(enc.unwrap_or_default().to_vec()), k.to_vec()));
                } else {
                    out.push((raw, None, Vec::new()));
                }
            }
            Ok(true)
        })?;
        let mut raws: Vec<&[u8]> = if plan.index_ordered {
            // Order inside runs that share an index encoding; ties in `_id` order.
            let mut keyed = out
                .into_iter()
                .map(|(raw, _, k)| Ok((sort_keys(&Doc::from_stored(raw)?, &q.sort)?, k, raw)))
                .collect::<Result<Vec<_>>>()?;
            keyed.sort_by(|a, b| cmp_keys(&a.0, &b.0, &q.sort).then_with(|| a.1.cmp(&b.1)));
            keyed.into_iter().map(|(_, _, r)| r).collect()
        } else {
            out.into_iter().map(|(r, _, _)| r).collect()
        };
        raws.drain(..q.skip.min(raws.len()));
        raws.truncate(q.limit.unwrap_or(usize::MAX));
        return Ok(raws);
    }

    if q.sort.is_empty() {
        // Natural (_id) order from an unordered source.
        let mut hits: Vec<(Vec<u8>, &[u8])> = Vec::new();
        for_each_candidate(ctx, coll, plan, |k, raw, _| {
            if check(raw)? {
                hits.push((k.to_vec(), raw));
            }
            Ok(true)
        })?;
        hits.sort_by(|a, b| a.0.cmp(&b.0));
        return Ok(hits.into_iter().skip(q.skip).take(q.limit.unwrap_or(usize::MAX)).map(|h| h.1).collect());
    }

    // In-memory sort on keys read from the stored bytes; ties keep `_id`
    // order. With a limit only the best `skip + limit` survive (selection,
    // not a full sort) and only those are ever decoded.
    struct Hit<'r> {
        keys: Vec<Value>,
        tie: Vec<u8>,
        seq: usize,
        raw: &'r [u8],
    }
    let mut hits: Vec<Hit> = Vec::new();
    let tie_by_key = !plan.id_ordered;
    for_each_candidate(ctx, coll, plan, |k, raw, _| {
        let d = Doc::from_stored(raw)?;
        if q.pred.is_true() || q.pred.eval(&d)? {
            let seq = hits.len();
            hits.push(Hit { keys: sort_keys(&d, &q.sort)?, tie: if tie_by_key { k.to_vec() } else { vec![] }, seq, raw });
        }
        Ok(true)
    })?;
    let cmp = |a: &Hit, b: &Hit| cmp_keys(&a.keys, &b.keys, &q.sort).then_with(|| a.tie.cmp(&b.tie)).then(a.seq.cmp(&b.seq));
    if let Some(w) = want {
        if w < hits.len() {
            hits.select_nth_unstable_by(w - 1, cmp);
            hits.truncate(w);
        }
    }
    hits.sort_unstable_by(cmp); // (keys, tie, seq) is a total order: unstable is exact
    Ok(hits.into_iter().skip(q.skip).take(q.limit.unwrap_or(usize::MAX)).map(|h| h.raw).collect())
}

pub fn run(ctx: &Ctx, coll: &str, plan: &Plan, q: &Query) -> Result<Vec<Value>> {
    select(ctx, coll, plan, q)?.into_iter().map(|raw| materialize(raw, &q.projection)).collect()
}

pub fn count(ctx: &Ctx, coll: &str, plan: &Plan, pred: &Pred) -> Result<usize> {
    let mut n = 0;
    for_each_candidate(ctx, coll, plan, |_, raw, _| {
        if plan.covered || pred.is_true() || pred.eval(&Doc::from_stored(raw)?)? {
            n += 1;
        }
        Ok(true)
    })?;
    Ok(n)
}
