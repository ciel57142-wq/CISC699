//! Physical execution of planned queries, with per-operator statistics that
//! EXPLAIN ANALYZE reports, and the EXPLAIN plan renderer.

use super::eval::{coerce_type, eval, is_true, Env, SubRunner};
use super::plan::*;
use crate::error::{Error, Result};
use crate::keys;
use crate::pbsql::ast::{AggFn, Expr, Ty};
use crate::rel::{self, Enc, Schema};
use crate::storage::Ctx;
use crate::value::Value;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Bound as B;
use std::rc::Rc;
use std::time::Instant;

pub type Row = Vec<Value>;

#[derive(Debug, Clone, Default)]
pub struct Stat {
    pub loops: u64,
    pub rows: u64,
    pub examined: u64,
    pub nanos: u128,
    /// Set when an index path could not be used at run time (e.g. a value of the wrong type).
    pub fallback: bool,
}

pub struct Executor<'p, 'c, 'a> {
    pub ctx: &'c Ctx<'a>,
    pub subs: &'p [SubPlan],
    pub stats: RefCell<HashMap<(usize, usize), Stat>>,
    cache: RefCell<HashMap<usize, Rc<Vec<Row>>>>,
    /// Which tree a node id belongs to: 0 = main plan, 1 + i = subplan i.
    tree: RefCell<usize>,
}

impl SubRunner for Executor<'_, '_, '_> {
    fn run_sub(&self, id: usize, outer: &[&[Value]]) -> Result<Vec<Vec<Value>>> {
        let sp = self.subs.get(id).ok_or_else(|| Error::Parse("internal error: missing subplan".into()))?;
        if !sp.correlated {
            if let Some(rows) = self.cache.borrow().get(&id) {
                return Ok((**rows).clone());
            }
        }
        let prev = self.tree.replace(id + 1);
        let res = self.exec(&sp.root, if sp.correlated { outer } else { &[] });
        self.tree.replace(prev);
        let rows = res?;
        if !sp.correlated {
            self.cache.borrow_mut().insert(id, Rc::new(rows.clone()));
        }
        Ok(rows)
    }
}

/// Hashable form of a list of values; numerically equal numbers hash alike.
/// `None` if any value is NULL (NULL never equals anything in a join).
fn hash_key(vals: &[Value], nulls_equal: bool) -> Option<String> {
    let mut parts = Vec::with_capacity(vals.len());
    for v in vals {
        if v.is_null() && !nulls_equal {
            return None;
        }
        parts.push(match v {
            Value::Float(f) if f.fract() == 0.0 && f.abs() < 9e15 => Value::Int(*f as i64),
            other => other.clone(),
        });
    }
    Some(Value::Array(parts).to_json())
}

fn combine(l: &[Value], r: &[Value]) -> Row {
    let mut c = Vec::with_capacity(l.len() + r.len());
    c.extend_from_slice(l);
    c.extend_from_slice(r);
    c
}

impl<'p, 'c, 'a> Executor<'p, 'c, 'a> {
    pub fn new(ctx: &'c Ctx<'a>, subs: &'p [SubPlan]) -> Self {
        Executor {
            ctx,
            subs,
            stats: RefCell::new(HashMap::new()),
            cache: RefCell::new(HashMap::new()),
            tree: RefCell::new(0),
        }
    }

    fn env<'e>(&'e self, row: &'e [Value], outer: &'e [&'e [Value]]) -> Env<'e> {
        Env::new(row, outer, self)
    }

    pub fn stat(&self, tree: usize, id: usize) -> Stat {
        self.stats.borrow().get(&(tree, id)).cloned().unwrap_or_default()
    }

    pub fn exec(&self, node: &Node, outer: &[&[Value]]) -> Result<Vec<Row>> {
        let start = Instant::now();
        let mut examined = 0u64;
        let mut fallback = false;
        let rows = self.exec_op(node, outer, &mut examined, &mut fallback)?;
        let key = (*self.tree.borrow(), node.id);
        let mut st = self.stats.borrow_mut();
        let s = st.entry(key).or_default();
        s.loops += 1;
        s.rows += rows.len() as u64;
        s.examined += examined;
        s.nanos += start.elapsed().as_nanos();
        s.fallback |= fallback;
        Ok(rows)
    }

    fn exec_op(&self, node: &Node, outer: &[&[Value]], examined: &mut u64, fallback: &mut bool) -> Result<Vec<Row>> {
        Ok(match &node.op {
            PhysOp::Unit => vec![vec![]],
            PhysOp::Scan { access, filter, limit, .. } => {
                let mut out = Vec::new();
                self.scan(access, filter.as_ref(), *limit, outer, examined, fallback, &mut |_, r| {
                    out.push(r);
                    Ok(())
                })?;
                out
            }
            PhysOp::Derived { input, .. } => self.exec(input, outer)?,
            PhysOp::Filter { input, pred } => {
                let rows = self.exec(input, outer)?;
                *examined = rows.len() as u64;
                let mut kept = Vec::new();
                for r in rows {
                    if is_true(&eval(pred, &self.env(&r, outer))?)? {
                        kept.push(r);
                    }
                }
                kept
            }
            PhysOp::NestedLoopJoin { left, right, on, outer: lo, right_width } => {
                let l = self.exec(left, outer)?;
                let r = self.exec(right, outer)?;
                let mut out = Vec::new();
                for lr in &l {
                    let mut matched = false;
                    for rr in &r {
                        *examined += 1;
                        let c = combine(lr, rr);
                        if on.as_ref().map_or(Ok(true), |p| is_true(&eval(p, &self.env(&c, outer))?))? {
                            out.push(c);
                            matched = true;
                        }
                    }
                    if !matched && *lo {
                        let mut c = lr.clone();
                        c.resize(lr.len() + right_width, Value::Null);
                        out.push(c);
                    }
                }
                out
            }
            PhysOp::HashJoin { left, right, lkeys, rkeys, on, outer: lo, right_width } => {
                let r = self.exec(right, outer)?;
                let mut table: HashMap<String, Vec<usize>> = HashMap::new();
                for (i, rr) in r.iter().enumerate() {
                    let k = rkeys.iter().map(|e| eval(e, &self.env(rr, outer))).collect::<Result<Vec<_>>>()?;
                    if let Some(h) = hash_key(&k, false) {
                        table.entry(h).or_default().push(i);
                    }
                }
                let l = self.exec(left, outer)?;
                let mut out = Vec::new();
                for lr in &l {
                    let k = lkeys.iter().map(|e| eval(e, &self.env(lr, outer))).collect::<Result<Vec<_>>>()?;
                    let mut matched = false;
                    if let Some(idx) = hash_key(&k, false).and_then(|h| table.get(&h)) {
                        for i in idx {
                            *examined += 1;
                            let c = combine(lr, &r[*i]);
                            if on.as_ref().map_or(Ok(true), |p| is_true(&eval(p, &self.env(&c, outer))?))? {
                                out.push(c);
                                matched = true;
                            }
                        }
                    }
                    if !matched && *lo {
                        let mut c = lr.clone();
                        c.resize(lr.len() + right_width, Value::Null);
                        out.push(c);
                    }
                }
                out
            }
            PhysOp::LookupJoin { left, right, on, outer: lo, right_width } => {
                let l = self.exec(left, outer)?;
                let mut out = Vec::new();
                for lr in &l {
                    let mut stack: Vec<&[Value]> = Vec::with_capacity(outer.len() + 1);
                    stack.push(lr);
                    stack.extend_from_slice(outer);
                    let mut matched = false;
                    for rr in self.exec(right, &stack)? {
                        *examined += 1;
                        let c = combine(lr, &rr);
                        if on.as_ref().map_or(Ok(true), |p| is_true(&eval(p, &self.env(&c, outer))?))? {
                            out.push(c);
                            matched = true;
                        }
                    }
                    if !matched && *lo {
                        let mut c = lr.clone();
                        c.resize(lr.len() + right_width, Value::Null);
                        out.push(c);
                    }
                }
                out
            }
            PhysOp::HashAggregate { input, group, aggs, width } => {
                let rows = self.exec(input, outer)?;
                *examined = rows.len() as u64;
                self.aggregate(rows, group, aggs, *width, outer)?
            }
            PhysOp::Project { input, exprs } => {
                let rows = self.exec(input, outer)?;
                let mut out = Vec::with_capacity(rows.len());
                for r in &rows {
                    out.push(exprs.iter().map(|e| eval(e, &self.env(r, outer))).collect::<Result<Row>>()?);
                }
                out
            }
            PhysOp::Distinct { input } => {
                let mut rows = self.exec(input, outer)?;
                let mut seen = HashSet::new();
                rows.retain(|r| seen.insert(hash_key(r, true).unwrap_or_default()));
                rows
            }
            PhysOp::Sort { input, keys } => {
                let rows = self.exec(input, outer)?;
                let mut keyed = Vec::with_capacity(rows.len());
                for r in rows {
                    let k = keys.iter().map(|(e, _)| eval(e, &self.env(&r, outer))).collect::<Result<Vec<_>>>()?;
                    keyed.push((k, r));
                }
                keyed.sort_by(|a, b| {
                    for (i, (_, desc)) in keys.iter().enumerate() {
                        let o = a.0[i].total_cmp(&b.0[i]);
                        if o != std::cmp::Ordering::Equal {
                            return if *desc { o.reverse() } else { o };
                        }
                    }
                    std::cmp::Ordering::Equal
                });
                keyed.into_iter().map(|(_, r)| r).collect()
            }
            PhysOp::Limit { input, limit, offset } => {
                let rows = self.exec(input, outer)?;
                rows.into_iter().skip(*offset).take(limit.unwrap_or(usize::MAX)).collect()
            }
            PhysOp::Union { left, right, all } => {
                let mut rows = self.exec(left, outer)?;
                rows.extend(self.exec(right, outer)?);
                if !*all {
                    let mut seen = HashSet::new();
                    rows.retain(|r| seen.insert(hash_key(r, true).unwrap_or_default()));
                }
                rows
            }
        })
    }

    fn aggregate(&self, rows: Vec<Row>, group: &[Expr], aggs: &[AggSpec], width: usize, outer: &[&[Value]]) -> Result<Vec<Row>> {
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut groups: Vec<(Row, Vec<Vec<Value>>, u64)> = Vec::new();
        for r in rows {
            let env = self.env(&r, outer);
            let key = group.iter().map(|e| eval(e, &env)).collect::<Result<Vec<_>>>()?;
            let mut vals = Vec::with_capacity(aggs.len());
            for a in aggs {
                vals.push(match &a.arg {
                    Some(e) => eval(e, &env)?,
                    None => Value::Bool(true),
                });
            }
            let h = hash_key(&key, true).unwrap_or_default();
            let gi = match index.get(&h) {
                Some(i) => *i,
                None => {
                    groups.push((r.clone(), vec![Vec::new(); aggs.len()], 0));
                    index.insert(h, groups.len() - 1);
                    groups.len() - 1
                }
            };
            let g = &mut groups[gi];
            g.2 += 1;
            for (i, v) in vals.into_iter().enumerate() {
                if !v.is_null() {
                    g.1[i].push(v);
                }
            }
        }
        if groups.is_empty() && group.is_empty() {
            groups.push((vec![Value::Null; width], vec![Vec::new(); aggs.len()], 0));
        }
        let mut out = Vec::with_capacity(groups.len());
        for (first, mut vals, n) in groups {
            let mut row = first;
            row.resize(width, Value::Null);
            for (a, v) in aggs.iter().zip(vals.iter_mut()) {
                if a.distinct {
                    let mut seen = HashSet::new();
                    v.retain(|x| seen.insert(hash_key(std::slice::from_ref(x), true).unwrap_or_default()));
                }
                row.push(finish_agg(a, v, n)?);
            }
            out.push(row);
        }
        Ok(out)
    }

    /// Run an access path; `sink` receives (storage key, row) for each row
    /// that passes `filter`, until `limit` rows have been produced.
    #[allow(clippy::too_many_arguments)]
    pub fn scan(
        &self,
        access: &Access,
        filter: Option<&Expr>,
        limit: Option<usize>,
        outer: &[&[Value]],
        examined: &mut u64,
        fallback: &mut bool,
        sink: &mut dyn FnMut(Vec<u8>, Row) -> Result<()>,
    ) -> Result<()> {
        let mut produced = 0usize;
        let mut feed = |key: Vec<u8>, row: Row, examined: &mut u64| -> Result<bool> {
            *examined += 1;
            if let Some(f) = filter {
                if !is_true(&eval(f, &self.env(&row, outer))?)? {
                    return Ok(true);
                }
            }
            sink(key, row)?;
            produced += 1;
            Ok(limit.is_none_or(|l| produced < l))
        };
        if limit == Some(0) {
            return Ok(());
        }
        let ev = |e: &Expr| eval(e, &self.env(&[], outer));
        match access {
            Access::Table { schema, path } => {
                let keys = match self.table_keys(schema, path, &ev)? {
                    Some(k) => k,
                    None => {
                        *fallback = !matches!(path, TablePath::FullScan);
                        for (k, raw) in self.ctx.scan(&keys::row_prefix(&schema.name)) {
                            if !feed(k, rel::decode_row(&raw)?, examined)? {
                                break;
                            }
                        }
                        return Ok(());
                    }
                };
                for k in keys {
                    if let Some(raw) = self.ctx.get(&k) {
                        if !feed(k, rel::decode_row(&raw)?, examined)? {
                            break;
                        }
                    }
                }
            }
            Access::Doc { coll, path } => {
                let keys = match self.doc_keys(coll, path, &ev)? {
                    Some(k) => k,
                    None => {
                        *fallback = !matches!(path, DocPath::FullScan);
                        for (k, raw) in self.ctx.scan(&keys::doc_prefix(coll)) {
                            if !feed(k, vec![crate::doc::decode(&raw)?], examined)? {
                                break;
                            }
                        }
                        return Ok(());
                    }
                };
                for k in keys {
                    if let Some(raw) = self.ctx.get(&k) {
                        if !feed(k, vec![crate::doc::decode(&raw)?], examined)? {
                            break;
                        }
                    }
                }
            }
            Access::Kv { ns, path } => {
                let base = keys::kv_ns_prefix(ns);
                let text = |v: Value| -> Option<String> {
                    match v {
                        Value::Text(s) => Some(s),
                        Value::Int(i) => Some(i.to_string()),
                        _ => None,
                    }
                };
                let row = |k: &[u8], v: Vec<u8>| -> Row {
                    vec![
                        Value::Text(String::from_utf8_lossy(&k[base.len()..]).into_owned()),
                        Value::Text(String::from_utf8_lossy(&v).into_owned()),
                    ]
                };
                let entries: Vec<(Vec<u8>, Vec<u8>)> = match path {
                    KvPath::Keys(list) => {
                        let mut ks = Vec::new();
                        for e in list {
                            if let Some(s) = text(ev(e)?) {
                                let mut k = base.clone();
                                k.extend_from_slice(s.as_bytes());
                                ks.push(k);
                            }
                        }
                        ks.sort();
                        ks.dedup();
                        ks.into_iter().filter_map(|k| self.ctx.get(&k).map(|v| (k, v))).collect()
                    }
                    KvPath::Prefix(p) => {
                        let mut k = base.clone();
                        k.extend_from_slice(p.as_bytes());
                        self.ctx.scan(&k)
                    }
                    KvPath::Range { lo, hi } => {
                        let bound = |b: &Option<Bound>, lower: bool| -> Result<Option<B<Vec<u8>>>> {
                            Ok(match b {
                                None => Some(if lower { B::Included(base.clone()) } else { B::Excluded(prefix_end(&base)) }),
                                Some(b) => match ev(&b.expr)? {
                                    Value::Text(s) => {
                                        let mut k = base.clone();
                                        k.extend_from_slice(s.as_bytes());
                                        Some(if b.inclusive { B::Included(k) } else { B::Excluded(k) })
                                    }
                                    _ => None,
                                },
                            })
                        };
                        match (bound(lo, true)?, bound(hi, false)?) {
                            (Some(l), Some(h)) => self.ctx.range(as_ref(&l), as_ref(&h)),
                            _ => {
                                *fallback = true;
                                self.ctx.scan(&base)
                            }
                        }
                    }
                    KvPath::FullScan => self.ctx.scan(&base),
                };
                for (k, v) in entries {
                    let r = row(&k, v);
                    if !feed(k, r, examined)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Storage keys a table path visits, or `None` to fall back to a full scan.
    fn table_keys(&self, s: &Schema, path: &TablePath, ev: &dyn Fn(&Expr) -> Result<Value>) -> Result<Option<Vec<Vec<u8>>>> {
        let pk = s.pk_positions();
        let typed = |ci: usize, v: Value| -> Option<Value> {
            let ty = s.cols[ci].ty;
            let c = coerce_type(ty, v.clone()).ok()?;
            (!c.is_null() && (c.rank() == v.rank() || matches!(ty, Ty::Json))).then_some(c)
        };
        Ok(Some(match path {
            TablePath::FullScan => return Ok(None),
            TablePath::PkPoints(points) => {
                let mut out = Vec::new();
                'points: for p in points {
                    let mut vals = Vec::new();
                    for (e, ci) in p.iter().zip(&pk) {
                        match typed(*ci, ev(e)?) {
                            Some(v) => vals.push(v),
                            None => continue 'points, // can never be equal
                        }
                    }
                    out.push(rel::pk_key(s, &vals.iter().collect::<Vec<_>>())?);
                }
                out.sort();
                out.dedup();
                out
            }
            TablePath::PkRange { eq, lo, hi } => {
                let base = keys::row_prefix(&s.name);
                let single = pk.len() == 1;
                let mut prefix = base.clone();
                for (e, ci) in eq.iter().zip(&pk) {
                    match typed(*ci, ev(e)?).and_then(|v| rel::encode_exact(&v)) {
                        Some(enc) => prefix.extend(enc),
                        None => return Ok(Some(vec![])),
                    }
                }
                let ci = pk[eq.len()];
                let enc = |v: Value| -> Option<Vec<u8>> {
                    if single {
                        keys::encode_id(&v).ok()
                    } else {
                        rel::encode_exact(&v)
                    }
                };
                let bound = |b: &Option<Bound>, lower: bool| -> Result<Option<B<Vec<u8>>>> {
                    Ok(match b {
                        None => Some(if lower { B::Included(prefix.clone()) } else { B::Excluded(prefix_end(&prefix)) }),
                        Some(b) => {
                            let v = ev(&b.expr)?;
                            if v.is_null() {
                                return Ok(Some(B::Excluded(prefix.clone()))); // nothing compares with NULL
                            }
                            match typed(ci, v).and_then(enc) {
                                None => None,
                                Some(e) => {
                                    let mut k = prefix.clone();
                                    k.extend(e);
                                    Some(if lower || single { B::Included(k) } else { B::Excluded(prefix_end(&k)) })
                                }
                            }
                        }
                    })
                };
                match (bound(lo, true)?, bound(hi, false)?) {
                    (Some(l), Some(h)) => self.ctx.range(as_ref(&l), as_ref(&h)).into_iter().map(|(k, _)| k).collect(),
                    _ => return Ok(None),
                }
            }
            TablePath::Index { ix, eq, lo, hi } => {
                let field_prefix = keys::index_prefix(&rel::owner(&s.name), &ix.field);
                let mut prefix = field_prefix.clone();
                let cols: Vec<usize> = ix.cols.iter().filter_map(|c| s.find_col(c)).collect();
                for (e, ci) in eq.iter().zip(&cols) {
                    let v = ev(e)?;
                    let v = if ix.enc == Enc::Exact { typed(*ci, v) } else { Some(v).filter(|v| !v.is_null()) };
                    match v.and_then(|v| rel::encode_with(ix.enc, &v)) {
                        Some(enc) => prefix.extend(enc),
                        None => return Ok(Some(vec![])),
                    }
                }
                let next = cols.get(eq.len()).copied();
                let enc_bound = |v: Value| -> Option<Vec<u8>> {
                    let v = match (ix.enc, next) {
                        (Enc::Exact, Some(ci)) => typed(ci, v)?,
                        _ => v,
                    };
                    rel::encode_with(ix.enc, &v)
                };
                let (l, h) = match index_bounds(&prefix, lo, hi, ev, &enc_bound)? {
                    Some(b) => b,
                    None => return Ok(None),
                };
                let mut ks = rel::ids_from_index(self.ctx, s, as_ref(&l), as_ref(&h), field_prefix.len(), ix);
                if lo.is_none() && hi.is_none() {
                    ks.sort();
                }
                ks.dedup();
                ks
            }
            TablePath::IndexIn { ix, vals } => {
                let field_prefix = keys::index_prefix(&rel::owner(&s.name), &ix.field);
                let ci = s.find_col(&ix.cols[0]);
                let mut out = Vec::new();
                for e in vals {
                    let v = ev(e)?;
                    let v = match (ix.enc, ci) {
                        (Enc::Exact, Some(ci)) => typed(ci, v),
                        _ => Some(v).filter(|v| !v.is_null()),
                    };
                    let Some(enc) = v.and_then(|v| rel::encode_with(ix.enc, &v)) else { continue };
                    let mut p = field_prefix.clone();
                    p.extend(enc);
                    let end = prefix_end(&p);
                    out.extend(rel::ids_from_index(self.ctx, s, B::Included(&p), B::Excluded(&end), field_prefix.len(), ix));
                }
                out.sort();
                out.dedup();
                out
            }
        }))
    }

    fn doc_keys(&self, coll: &str, path: &DocPath, ev: &dyn Fn(&Expr) -> Result<Value>) -> Result<Option<Vec<Vec<u8>>>> {
        Ok(Some(match path {
            DocPath::FullScan => return Ok(None),
            DocPath::Ids(list) => {
                let mut out = Vec::new();
                for e in list {
                    let v = ev(e)?;
                    if let Ok(enc) = keys::encode_id(&v) {
                        let mut k = keys::doc_prefix(coll);
                        k.extend(enc);
                        out.push(k);
                    }
                }
                out.sort();
                out.dedup();
                out
            }
            DocPath::Index { field, eq, lo, hi } => {
                let field_prefix = keys::index_prefix(&format!("c:{coll}"), field);
                let enc = |v: Value| keys::encode_index_value(&v);
                let (l, h) = match eq {
                    Some(e) => match ev(e)?.clone() {
                        v if v.is_null() => return Ok(Some(vec![])),
                        v => match enc(v) {
                            Some(x) => {
                                let mut p = field_prefix.clone();
                                p.extend(x);
                                let end = prefix_end(&p);
                                (B::Included(p), B::Excluded(end))
                            }
                            None => return Ok(None),
                        },
                    },
                    None => match index_bounds(&field_prefix, lo, hi, ev, &enc)? {
                        Some(b) => b,
                        None => return Ok(None),
                    },
                };
                let dp = keys::doc_prefix(coll);
                let mut out: Vec<Vec<u8>> = self
                    .ctx
                    .range(as_ref(&l), as_ref(&h))
                    .into_iter()
                    .map(|(k, _)| {
                        let rest = &k[field_prefix.len()..];
                        let n = rel::encoded_len(rest);
                        let mut d = dp.clone();
                        d.extend_from_slice(&rest[n.min(rest.len())..]);
                        d
                    })
                    .collect();
                out.sort();
                out.dedup();
                out
            }
        }))
    }
}

fn as_ref(b: &B<Vec<u8>>) -> B<&[u8]> {
    match b {
        B::Included(v) => B::Included(v.as_slice()),
        B::Excluded(v) => B::Excluded(v.as_slice()),
        B::Unbounded => B::Unbounded,
    }
}

/// The first key after every key that starts with `p`.
fn prefix_end(p: &[u8]) -> Vec<u8> {
    let mut k = p.to_vec();
    k.push(0xff);
    k
}

/// Key bounds for a range on an index column after `prefix`. Bounds are
/// widened to inclusive and kept inside the value's type tag; the scan's
/// filter makes the result exact. `None` means "cannot use the index".
type KeyBounds = (B<Vec<u8>>, B<Vec<u8>>);
fn index_bounds(
    prefix: &[u8],
    lo: &Option<Bound>,
    hi: &Option<Bound>,
    ev: &dyn Fn(&Expr) -> Result<Value>,
    enc: &dyn Fn(Value) -> Option<Vec<u8>>,
) -> Result<Option<KeyBounds>> {
    let mut tag: Option<u8> = None;
    let mut one = |b: &Option<Bound>| -> Result<Option<Option<Vec<u8>>>> {
        Ok(match b {
            None => Some(None),
            Some(b) => {
                let v = ev(&b.expr)?;
                if v.is_null() {
                    return Ok(None);
                }
                match enc(v) {
                    None => None,
                    Some(e) => {
                        if tag.is_some_and(|t| t != e[0]) {
                            return Ok(None);
                        }
                        tag = Some(e[0]);
                        Some(Some(e))
                    }
                }
            }
        })
    };
    let (Some(l), Some(h)) = (one(lo)?, one(hi)?) else {
        return Ok(None);
    };
    let Some(tag) = tag else {
        // no bound at all: the whole prefix
        return Ok(Some((B::Included(prefix.to_vec()), B::Excluded(prefix_end(prefix)))));
    };
    let lo_key = match l {
        Some(e) => [prefix, &e].concat(),
        None => [prefix, &[tag]].concat(),
    };
    let hi_key = match h {
        Some(e) => prefix_end(&[prefix, &e].concat()),
        None => [prefix, &[tag + 1]].concat(),
    };
    Ok(Some((B::Included(lo_key), B::Excluded(hi_key))))
}

fn finish_agg(a: &AggSpec, vals: &mut Vec<Value>, n: u64) -> Result<Value> {
    Ok(match a.func {
        AggFn::Count if a.arg.is_none() => Value::Int(n as i64),
        AggFn::Count => Value::Int(vals.len() as i64),
        AggFn::Min => vals.drain(..).min_by(|a, b| a.total_cmp(b)).unwrap_or(Value::Null),
        AggFn::Max => vals.drain(..).max_by(|a, b| a.total_cmp(b)).unwrap_or(Value::Null),
        AggFn::Sum | AggFn::Avg => {
            if vals.is_empty() {
                return Ok(Value::Null);
            }
            let count = vals.len() as f64;
            let mut total = Value::Int(0);
            for v in vals.iter() {
                total = super::eval::arith(crate::pbsql::ast::Op::Add, &total, v)?;
            }
            if a.func == AggFn::Avg {
                Value::Float(total.as_f64().unwrap_or(0.0) / count)
            } else {
                total
            }
        }
    })
}

// =================================================================== EXPLAIN

pub fn fmt_expr(e: &Expr) -> String {
    use crate::pbsql::ast::SubRef;
    let sub = |s: &SubRef| match s {
        SubRef::Bound(i) => format!("SubPlan {i}"),
        SubRef::Parsed(_) => "subquery".into(),
    };
    match e {
        Expr::Lit(Value::Text(s)) => format!("'{}'", s.replace('\'', "''")),
        Expr::Lit(v) => v.to_json(),
        Expr::Col(p) => p.join("."),
        Expr::Slot { level: 0, name, .. } => name.clone(),
        Expr::Slot { level, name, .. } => format!("{}{name}", "outer.".repeat(*level)),
        Expr::Bin(a, op, b) => format!("({} {} {})", fmt_expr(a), op.text(), fmt_expr(b)),
        Expr::Not(a) => format!("NOT {}", fmt_expr(a)),
        Expr::Neg(a) => format!("-{}", fmt_expr(a)),
        Expr::IsNull(a, n) => format!("{} IS {}NULL", fmt_expr(a), if *n { "NOT " } else { "" }),
        Expr::Like(a, p, n) => format!("{} {}LIKE {}", fmt_expr(a), if *n { "NOT " } else { "" }, fmt_expr(p)),
        Expr::In(a, l, n) => format!(
            "{} {}IN ({})",
            fmt_expr(a),
            if *n { "NOT " } else { "" },
            l.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
        ),
        Expr::InQuery(a, s, n) => format!("{} {}IN ({})", fmt_expr(a), if *n { "NOT " } else { "" }, sub(s)),
        Expr::Exists(s, n) => format!("{}EXISTS ({})", if *n { "NOT " } else { "" }, sub(s)),
        Expr::ScalarQuery(s) => format!("({})", sub(s)),
        Expr::Agg(f, a, d) => format!(
            "{}({}{})",
            f.name(),
            if *d { "DISTINCT " } else { "" },
            a.as_ref().map_or("*".into(), |x| fmt_expr(x))
        ),
        Expr::Func(n, a) => format!("{n}({})", a.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")),
        Expr::Case { .. } => "CASE ...".into(),
        Expr::Cast(a, t) => format!("CAST({} AS {})", fmt_expr(a), t.name().to_uppercase()),
        Expr::Default => "DEFAULT".into(),
    }
}

fn fmt_bound(name: &str, lo: &Option<Bound>, hi: &Option<Bound>) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(b) = lo {
        v.push(format!("{name} {} {}", if b.inclusive { ">=" } else { ">" }, fmt_expr(&b.expr)));
    }
    if let Some(b) = hi {
        v.push(format!("{name} {} {}", if b.inclusive { "<=" } else { "<" }, fmt_expr(&b.expr)));
    }
    v
}

fn describe_access(access: &Access, alias: &str) -> (String, Option<String>) {
    let on = |kind: &str, what: &str| if alias == what || what.ends_with(&format!(".{alias}")) {
        format!("{kind} on {what}")
    } else {
        format!("{kind} on {what} {alias}")
    };
    match access {
        Access::Table { schema, path } => {
            let pk = &schema.pk;
            match path {
                TablePath::FullScan => (on("TableScan", &schema.name), None),
                TablePath::PkPoints(points) => {
                    let conds: Vec<String> = points
                        .iter()
                        .map(|p| pk.iter().zip(p).map(|(c, e)| format!("{c} = {}", fmt_expr(e))).collect::<Vec<_>>().join(" AND "))
                        .collect();
                    (on("PrimaryKeyLookup", &schema.name), Some(conds.join(" OR ")))
                }
                TablePath::PkRange { eq, lo, hi } => {
                    let mut c: Vec<String> = pk.iter().zip(eq).map(|(c, e)| format!("{c} = {}", fmt_expr(e))).collect();
                    c.extend(fmt_bound(&pk[eq.len()], lo, hi));
                    (on("PrimaryKeyRangeScan", &schema.name), Some(c.join(" AND ")))
                }
                TablePath::Index { ix, eq, lo, hi } => {
                    let mut c: Vec<String> = ix.cols.iter().zip(eq).map(|(c, e)| format!("{c} = {}", fmt_expr(e))).collect();
                    let ranged = lo.is_some() || hi.is_some();
                    if let Some(next) = ix.cols.get(eq.len()) {
                        c.extend(fmt_bound(next, lo, hi));
                    }
                    let kind = if ranged || eq.len() < ix.cols.len() { "IndexRangeScan" } else { "IndexScan" };
                    (format!("{} using {}", on(kind, &schema.name), ix.name), Some(c.join(" AND ")))
                }
                TablePath::IndexIn { ix, vals } => (
                    format!("{} using {}", on("IndexScan", &schema.name), ix.name),
                    Some(format!("{} IN ({})", ix.cols[0], vals.iter().map(fmt_expr).collect::<Vec<_>>().join(", "))),
                ),
            }
        }
        Access::Doc { coll, path } => {
            let what = format!("DOCUMENT.{coll}");
            match path {
                DocPath::FullScan => (on("DocumentScan", &what), None),
                DocPath::Ids(ids) => (
                    on("DocumentIdLookup", &what),
                    Some(ids.iter().map(|e| format!("_id = {}", fmt_expr(e))).collect::<Vec<_>>().join(" OR ")),
                ),
                DocPath::Index { field, eq, lo, hi } => {
                    let (kind, cond) = match eq {
                        Some(e) => ("DocumentIndexScan", vec![format!("{field} = {}", fmt_expr(e))]),
                        None => ("DocumentIndexRangeScan", fmt_bound(field, lo, hi)),
                    };
                    (format!("{} using index on {field}", on(kind, &what)), Some(cond.join(" AND ")))
                }
            }
        }
        Access::Kv { ns, path } => {
            let what = if ns.is_empty() { "KV".to_string() } else { format!("KV.{ns}") };
            match path {
                KvPath::FullScan => (on("KVScan", &what), None),
                KvPath::Keys(k) => (
                    on("KVGet", &what),
                    Some(k.iter().map(|e| format!("key = {}", fmt_expr(e))).collect::<Vec<_>>().join(" OR ")),
                ),
                KvPath::Prefix(p) => (on("KVPrefixScan", &what), Some(format!("key LIKE '{p}%'"))),
                KvPath::Range { lo, hi } => (on("KVRangeScan", &what), Some(fmt_bound("key", lo, hi).join(" AND "))),
            }
        }
    }
}

fn model_of(n: &Node) -> Option<&'static str> {
    match &n.op {
        PhysOp::Scan { access, .. } => Some(match access {
            Access::Table { .. } => "relational",
            Access::Doc { .. } => "document",
            Access::Kv { .. } => "kv",
        }),
        PhysOp::Filter { input, .. } | PhysOp::Project { input, .. } => model_of(input),
        _ => None,
    }
}

fn models(n: &Node, out: &mut HashSet<&'static str>) {
    if let Some(m) = model_of(n) {
        out.insert(m);
    }
    match &n.op {
        PhysOp::NestedLoopJoin { left, right, .. } | PhysOp::HashJoin { left, right, .. } | PhysOp::LookupJoin { left, right, .. } => {
            models(left, out);
            models(right, out);
        }
        PhysOp::Filter { input, .. } | PhysOp::Derived { input, .. } => models(input, out),
        _ => {}
    }
}

pub struct ExplainOpts {
    pub analyze: bool,
}

pub fn render(root: &Node, tree: usize, ex: Option<&Executor>, out: &mut Vec<String>, depth: usize) {
    let pad = if depth == 0 { String::new() } else { format!("{}-> ", "   ".repeat(depth - 1)) };
    let detail_pad = "   ".repeat(depth) + "   ";
    let mut details: Vec<String> = Vec::new();
    let title = match &root.op {
        PhysOp::Unit => "Result (one row)".to_string(),
        PhysOp::Scan { access, alias, filter, limit } => {
            let (t, cond) = describe_access(access, alias);
            if let Some(c) = cond {
                details.push(format!("Index Cond: {c}"));
            }
            if let Some(f) = filter {
                details.push(format!("Filter: {}", fmt_expr(f)));
            }
            if let Some(l) = limit {
                details.push(format!("Stops after: {l} row(s)"));
            }
            t
        }
        PhysOp::Derived { alias, .. } => format!("SubqueryScan {alias}"),
        PhysOp::Filter { pred, .. } => {
            details.push(format!("Filter: {}", fmt_expr(pred)));
            "Filter".into()
        }
        PhysOp::NestedLoopJoin { on, outer, .. } | PhysOp::HashJoin { on, outer, .. } | PhysOp::LookupJoin { on, outer, .. } => {
            let kind = match &root.op {
                PhysOp::NestedLoopJoin { .. } => "NestedLoopJoin",
                PhysOp::HashJoin { .. } => "HashJoin",
                _ => "IndexLookupJoin",
            };
            if let PhysOp::HashJoin { lkeys, rkeys, .. } = &root.op {
                details.push(format!(
                    "Hash Keys: ({}) = ({})",
                    lkeys.iter().map(fmt_expr).collect::<Vec<_>>().join(", "),
                    rkeys.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
                ));
            }
            if let Some(c) = on {
                details.push(format!("Join Cond: {}", fmt_expr(c)));
            }
            let mut ms = HashSet::new();
            models(root, &mut ms);
            let cross = if ms.len() > 1 {
                let mut v: Vec<&str> = ms.into_iter().collect();
                v.sort();
                format!(" [cross-model: {}]", v.join(" + "))
            } else {
                String::new()
            };
            format!("{kind}{}{cross}", if *outer { " (LEFT)" } else { "" })
        }
        PhysOp::HashAggregate { group, aggs, .. } => {
            if !group.is_empty() {
                details.push(format!("Group Key: {}", group.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")));
            }
            if !aggs.is_empty() {
                details.push(format!(
                    "Aggregates: {}",
                    aggs.iter()
                        .map(|a| fmt_expr(&Expr::Agg(a.func, a.arg.clone().map(Box::new), a.distinct)))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            "HashAggregate".into()
        }
        PhysOp::Project { exprs, .. } => format!("Projection ({})", exprs.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")),
        PhysOp::Distinct { .. } => "Distinct".into(),
        PhysOp::Sort { keys, .. } => format!(
            "Sort ({})",
            keys.iter().map(|(e, d)| format!("{}{}", fmt_expr(e), if *d { " DESC" } else { "" })).collect::<Vec<_>>().join(", ")
        ),
        PhysOp::Limit { limit, offset, .. } => match (limit, offset) {
            (Some(l), 0) => format!("Limit ({l})"),
            (Some(l), o) => format!("Limit ({l} offset {o})"),
            (None, o) => format!("Offset ({o})"),
        },
        PhysOp::Union { all, .. } => if *all { "UnionAll" } else { "Union" }.into(),
    };
    let stats = ex.map(|e| e.stat(tree, root.id));
    let suffix = match &stats {
        Some(s) if s.loops == 0 => "  (never executed)".to_string(),
        Some(s) => {
            let mut t = format!(
                "  (actual rows={} loops={} time={:.3} ms",
                s.rows,
                s.loops,
                s.nanos as f64 / 1e6
            );
            if matches!(root.op, PhysOp::Scan { .. }) || s.examined > 0 {
                t.push_str(&format!(" examined={}", s.examined));
            }
            t.push(')');
            if s.fallback {
                t.push_str(" [index not usable for these values: scanned]");
            }
            t
        }
        None => String::new(),
    };
    out.push(format!("{pad}{title}{suffix}"));
    for d in details {
        out.push(format!("{detail_pad}{d}"));
    }
    let kids: Vec<&Node> = match &root.op {
        PhysOp::Unit | PhysOp::Scan { .. } => vec![],
        PhysOp::Derived { input, .. } | PhysOp::Filter { input, .. } | PhysOp::HashAggregate { input, .. }
        | PhysOp::Project { input, .. } | PhysOp::Distinct { input } | PhysOp::Sort { input, .. }
        | PhysOp::Limit { input, .. } => vec![input],
        PhysOp::NestedLoopJoin { left, right, .. } | PhysOp::HashJoin { left, right, .. }
        | PhysOp::LookupJoin { left, right, .. } | PhysOp::Union { left, right, .. } => vec![left, right],
    };
    for k in kids {
        render(k, tree, ex, out, depth + 1);
    }
}

pub fn explain(plan: &Plan, ex: Option<&Executor>) -> Vec<String> {
    let mut out = Vec::new();
    render(&plan.root, 0, ex, &mut out, 0);
    for (i, sp) in plan.subs.iter().enumerate() {
        out.push(format!("SubPlan {i}{}", if sp.correlated { " (correlated: runs once per outer row)" } else { " (runs once)" }));
        render(&sp.root, i + 1, ex, &mut out, 1);
    }
    out
}

/// Run a planned query.
pub fn run(ctx: &Ctx, plan: &Plan) -> Result<Vec<Row>> {
    let ex = Executor::new(ctx, &plan.subs);
    ex.exec(&plan.root, &[])
}

/// Run a planned query and return its rows together with the executor (for EXPLAIN ANALYZE).
pub fn run_analyze<'p, 'c, 'a>(ctx: &'c Ctx<'a>, plan: &'p Plan) -> Result<(Vec<Row>, Executor<'p, 'c, 'a>, u128)> {
    let ex = Executor::new(ctx, &plan.subs);
    let t = Instant::now();
    let rows = ex.exec(&plan.root, &[])?;
    let nanos = t.elapsed().as_nanos();
    Ok((rows, ex, nanos))
}
