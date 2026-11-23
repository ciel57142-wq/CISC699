//! Access-path selection and query execution.
//!
//! For each table the planner picks, in order of preference: a primary-key
//! point lookup, a primary-key IN-list lookup, a unique-index point lookup,
//! the index (or composite primary-key prefix) with the longest leftmost
//! run of equality predicates, a range scan on the primary key or on an
//! index's first column, or a full scan. Joins use a hash join when the ON
//! clause contains an equality between the two sides, else a nested loop.
//! The chosen paths only narrow the candidate rows; every row is still
//! checked against the full WHERE / ON expression.

use super::ast::*;
use super::catalog::*;
use super::eval::*;
use super::parser::perr;
use crate::error::{Error, Result};
use crate::keys;
use crate::value::Value;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

type Rows = Vec<(Vec<u8>, Vec<Value>)>;

// ================================================================== planner

/// Predicates on one table's columns that every matching row satisfies.
#[derive(Default)]
pub(super) struct Preds {
    pub eq: HashMap<usize, Value>,
    pub ins: HashMap<usize, Vec<Value>>,
    /// Inclusive lower and upper bounds.
    pub lo: HashMap<usize, Value>,
    pub hi: HashMap<usize, Value>,
}

fn flip(op: Op) -> Op {
    match op {
        Op::Lt => Op::Gt,
        Op::Le => Op::Ge,
        Op::Gt => Op::Lt,
        Op::Ge => Op::Le,
        o => o,
    }
}

fn table_col(s: &Schema, alias: &str, e: &Expr) -> Option<usize> {
    match e {
        Expr::Col(q, n) if q.as_ref().map_or(true, |q| q == alias) => s.cols.iter().position(|c| &c.name == n),
        _ => None,
    }
}

fn konst(e: &Expr, q: &QCtx) -> Option<Value> {
    if !is_const(e) {
        return None;
    }
    const_eval(e, q).ok().filter(|v| !v.is_null())
}

fn collect_preds(e: &Expr, s: &Schema, alias: &str, q: &QCtx, p: &mut Preds) {
    match e {
        Expr::Bin(a, Op::And, b) => {
            collect_preds(a, s, alias, q, p);
            collect_preds(b, s, alias, q, p);
        }
        Expr::Bin(a, op @ (Op::Eq | Op::Lt | Op::Le | Op::Gt | Op::Ge), b) => {
            let (ci, v, op) = match (table_col(s, alias, a), table_col(s, alias, b)) {
                (Some(ci), None) => match konst(b, q) {
                    Some(v) => (ci, v, *op),
                    None => return,
                },
                (None, Some(ci)) => match konst(a, q) {
                    Some(v) => (ci, v, flip(*op)),
                    None => return,
                },
                _ => return,
            };
            match op {
                Op::Eq => {
                    p.eq.entry(ci).or_insert(v);
                }
                Op::Gt | Op::Ge => {
                    let tighter = p.lo.get(&ci).map_or(true, |old| v.total_cmp(old) == Ordering::Greater);
                    if tighter {
                        p.lo.insert(ci, v);
                    }
                }
                _ => {
                    let tighter = p.hi.get(&ci).map_or(true, |old| v.total_cmp(old) == Ordering::Less);
                    if tighter {
                        p.hi.insert(ci, v);
                    }
                }
            }
        }
        Expr::In(a, list, false) => {
            if let Some(ci) = table_col(s, alias, a) {
                let vals: Option<Vec<Value>> = list.iter().map(|x| konst(x, q)).collect();
                if let Some(vals) = vals {
                    p.ins.entry(ci).or_insert(vals);
                }
            }
        }
        _ => {}
    }
}

/// Can `v` be looked up in a key built from a column of type `ty`?
/// `encode_id` keys (one-column primary keys) need the exact type; index
/// encodings share one form for INT and REAL.
fn key_compatible(ty: Ty, v: &Value, exact: bool) -> bool {
    match (ty, v) {
        (Ty::Int, Value::Int(_)) | (Ty::Text, Value::Text(_)) | (Ty::Bool, Value::Bool(_)) => true,
        (Ty::Int | Ty::Float, Value::Int(_) | Value::Float(_)) => !exact,
        _ => false,
    }
}

/// A candidate access path over an ordered run of encoded keys.
struct Prefixed {
    desc: String,
    /// Key prefix shared by every candidate entry (field prefix plus equality values).
    base: Vec<u8>,
    /// Entries are index entries pointing at rows, rather than the rows themselves.
    is_index: bool,
    /// Encoded values that follow `base` in an index entry, before the row id.
    total_components: usize,
    lo: Option<Vec<u8>>,
    hi: Option<Vec<u8>>,
}

fn get_row(q: &QCtx, key: Vec<u8>) -> Result<Option<(Vec<u8>, Vec<Value>)>> {
    Ok(match q.ctx.get(&key) {
        Some(raw) => Some((key, decode_row(&raw)?)),
        None => None,
    })
}

/// Candidate rows for `preds`. The caller still applies the full filter.
pub(super) fn fetch_with(q: &QCtx, s: &Schema, alias: &str, p: &Preds) -> Result<Rows> {
    let label = if alias == s.name { s.name.clone() } else { format!("{} AS {alias}", s.name) };
    let exact_pk = s.pk.len() == 1;
    // 1. primary-key point lookup
    if !s.pk.is_empty()
        && s.pk.iter().all(|i| p.eq.get(i).map_or(false, |v| key_compatible(s.cols[*i].ty, v, exact_pk)))
    {
        let mut row = vec![Value::Null; s.cols.len()];
        for &i in &s.pk {
            row[i] = p.eq[&i].clone();
        }
        q.note(|| format!("{label}: primary key lookup"));
        return Ok(get_row(q, pk_key(s, &row)?)?.into_iter().collect());
    }
    // 2. primary-key IN list
    if exact_pk {
        let pi = s.pk[0];
        if let Some(vals) = p.ins.get(&pi) {
            if vals.iter().all(|v| key_compatible(s.cols[pi].ty, v, true)) {
                let mut keys_ = HashSet::new();
                let mut out = Vec::new();
                for v in vals {
                    let mut k = keys::row_prefix(&s.name);
                    k.extend(keys::encode_id(v)?);
                    if keys_.insert(k.clone()) {
                        out.extend(get_row(q, k)?);
                    }
                }
                out.sort_by(|a, b| a.0.cmp(&b.0));
                q.note(|| format!("{label}: primary key IN lookup ({} keys)", keys_.len()));
                return Ok(out);
            }
        }
    }
    // 3. index or composite-key prefix with the most leading equalities, then ranges
    let mut best: Option<(usize, Prefixed)> = None;
    let mut consider = |score: usize, cand: Prefixed| {
        if score > 0 && best.as_ref().map_or(true, |(b, _)| score > *b) {
            best = Some((score, cand));
        }
    };
    for ix in &s.indexes {
        let Ok(pos) = s.positions(&ix.cols) else { continue };
        let mut k = 0;
        while k < pos.len() && p.eq.get(&pos[k]).map_or(false, |v| key_compatible(s.cols[pos[k]].ty, v, false)) {
            k += 1;
        }
        let mut base = index_base(&s.name, &ix.cols);
        let eq_vals: Vec<&Value> = pos[..k].iter().map(|i| &p.eq[i]).collect();
        if k > 0 {
            match index_values(&eq_vals) {
                Some(enc) => base.extend(enc),
                None => continue,
            }
        }
        let (mut lo, mut hi) = (None, None);
        if k < pos.len() {
            let ty = s.cols[pos[k]].ty;
            let single = pos.len() == 1;
            let enc = |v: &Value| if single { keys::encode_index_value(v) } else { Some(enc_component(v)) };
            lo = p.lo.get(&pos[k]).filter(|v| key_compatible(ty, v, false)).and_then(enc);
            hi = p.hi.get(&pos[k]).filter(|v| key_compatible(ty, v, false)).and_then(enc);
        }
        let ranged = lo.is_some() || hi.is_some();
        let score = if k == pos.len() && ix.unique {
            800
        } else {
            100 * k + if ranged { 10 } else { 0 }
        };
        let mut desc = format!("{label}: index {}", ix.name);
        let parts: Vec<String> = ix.cols[..k].iter().map(|c| format!("{c} =")).collect();
        let mut parts = parts;
        if ranged && k < ix.cols.len() {
            parts.push(format!("{} range", ix.cols[k]));
        }
        desc += &format!(" ({})", parts.join(", "));
        consider(score, Prefixed { desc, base, is_index: true, total_components: pos.len() - k, lo, hi });
    }
    if s.pk.len() > 1 {
        let mut k = 0;
        while k < s.pk.len() && p.eq.get(&s.pk[k]).map_or(false, |v| key_compatible(s.cols[s.pk[k]].ty, v, false)) {
            k += 1;
        }
        let mut base = keys::row_prefix(&s.name);
        for &i in &s.pk[..k] {
            base.extend(enc_component(&p.eq[&i]));
        }
        let (mut lo, mut hi) = (None, None);
        if k < s.pk.len() {
            let ci = s.pk[k];
            let ty = s.cols[ci].ty;
            lo = p.lo.get(&ci).filter(|v| key_compatible(ty, v, false)).map(enc_component);
            hi = p.hi.get(&ci).filter(|v| key_compatible(ty, v, false)).map(enc_component);
        }
        let ranged = lo.is_some() || hi.is_some();
        let score = 100 * k + if ranged { 11 } else { 0 };
        consider(score, Prefixed { desc: format!("{label}: primary key prefix scan"), base, is_index: false, total_components: 0, lo, hi });
    } else if exact_pk {
        let ci = s.pk[0];
        let ty = s.cols[ci].ty;
        let enc = |v: &Value| keys::encode_id(v).ok();
        let lo = p.lo.get(&ci).filter(|v| key_compatible(ty, v, true)).and_then(enc);
        let hi = p.hi.get(&ci).filter(|v| key_compatible(ty, v, true)).and_then(enc);
        if lo.is_some() || hi.is_some() {
            let base = keys::row_prefix(&s.name);
            consider(11, Prefixed { desc: format!("{label}: primary key range scan"), base, is_index: false, total_components: 0, lo, hi });
        }
    }
    if let Some((_, cand)) = best {
        q.note(|| cand.desc.clone());
        return scan_prefixed(q, s, cand);
    }
    q.note(|| format!("{label}: full scan"));
    all_rows(q.ctx, s)
}

fn scan_prefixed(q: &QCtx, s: &Schema, c: Prefixed) -> Result<Rows> {
    let mut start = c.base.clone();
    if let Some(lo) = &c.lo {
        start.extend_from_slice(lo);
    }
    let end = match &c.hi {
        Some(hi) => {
            let mut e = c.base.clone();
            e.extend_from_slice(hi);
            prefix_end(&e)
        }
        None => prefix_end(&c.base),
    };
    let entries = q.ctx.scan_range(&c.base, &start, end.as_deref());
    if !c.is_index {
        return entries.into_iter().map(|(k, raw)| Ok((k, decode_row(&raw)?))).collect();
    }
    let mut out = Vec::new();
    for (ik, _) in entries {
        let mut pos = c.base.len();
        for _ in 0..c.total_components {
            pos += component_len(&ik[pos..]).ok_or_else(|| Error::Corrupt("index entry is unreadable".into()))?;
        }
        let mut k = keys::row_prefix(&s.name);
        k.extend_from_slice(&ik[pos..]);
        out.extend(get_row(q, k)?);
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    Ok(out)
}

/// Candidate rows of a table for a WHERE clause.
pub(super) fn fetch(q: &QCtx, s: &Schema, alias: &str, filter: Option<&Expr>) -> Result<Rows> {
    let mut p = Preds::default();
    if let Some(f) = filter {
        collect_preds(f, s, alias, q, &mut p);
    }
    fetch_with(q, s, alias, &p)
}

/// Rows whose columns `cols` equal `vals` exactly (all non-NULL).
pub(super) fn find_eq(q: &QCtx, s: &Schema, cols: &[usize], vals: &[Value]) -> Result<Rows> {
    let mut p = Preds::default();
    for (c, v) in cols.iter().zip(vals) {
        p.eq.insert(*c, v.clone());
    }
    let mut rows = fetch_with(q, s, &s.name, &p)?;
    rows.retain(|(_, r)| cols.iter().zip(vals).all(|(c, v)| r[*c].total_cmp(v) == Ordering::Equal));
    Ok(rows)
}

// ================================================================= execution

/// Run a query: one SELECT or a compound of them.
pub(super) fn run_query(q: &QCtx, query: &Query, outer: Option<&Env>, level: usize) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    let (cols, mut rows) = select(q, &query.head, outer, level)?;
    if query.rest.is_empty() {
        return Ok((cols, rows));
    }
    for (op, all, s) in &query.rest {
        let (c2, r2) = select(q, s, outer, level)?;
        if c2.len() != cols.len() {
            return Err(Error::Schema(format!(
                "each side of a set operation must have the same number of columns ({} vs {})",
                cols.len(),
                c2.len()
            )));
        }
        rows = combine(*op, *all, rows, r2);
    }
    // ORDER BY of a compound query names output columns, by name or position.
    let mut order = Vec::new();
    for o in &query.order_by {
        let idx = match &o.expr {
            Expr::Lit(Value::Int(k)) if *k >= 1 && (*k as usize) <= cols.len() => *k as usize - 1,
            Expr::Col(_, n) => match cols.iter().position(|c| c == n) {
                Some(i) => i,
                None => return Err(Error::Schema(format!("ORDER BY '{n}' is not an output column of the set operation"))),
            },
            _ => return perr("ORDER BY of a set operation must name an output column or position"),
        };
        order.push((idx, o));
    }
    if !order.is_empty() {
        rows.sort_by(|a, b| {
            for (i, o) in &order {
                let c = cmp_order(&a[*i], &b[*i], o);
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
    }
    let rows = rows.into_iter().skip(query.offset).take(query.limit.unwrap_or(usize::MAX)).collect();
    Ok((cols, rows))
}

fn combine(op: SetOp, all: bool, left: Vec<Vec<Value>>, right: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let key = |r: &Vec<Value>| group_key(r.iter());
    match op {
        SetOp::Union => {
            let mut out = left;
            out.extend(right);
            if !all {
                let mut seen = HashSet::new();
                out.retain(|r| seen.insert(key(r)));
            }
            out
        }
        SetOp::Intersect | SetOp::Except => {
            let mut counts: HashMap<Vec<u8>, usize> = HashMap::new();
            for r in &right {
                *counts.entry(key(r)).or_insert(0) += 1;
            }
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for r in left {
                let k = key(&r);
                let present = counts.get(&k).copied().unwrap_or(0) > 0;
                let keep = if op == SetOp::Intersect { present } else { !present };
                if all {
                    if op == SetOp::Intersect && present {
                        *counts.get_mut(&k).unwrap() -= 1;
                        out.push(r);
                    } else if op == SetOp::Except {
                        if present {
                            *counts.get_mut(&k).unwrap() -= 1;
                        } else {
                            out.push(r);
                        }
                    }
                } else if keep && seen.insert(k) {
                    out.push(r);
                }
            }
            out
        }
    }
}

fn cmp_order(x: &Value, y: &Value, o: &OrderItem) -> Ordering {
    match (x.is_null(), y.is_null(), o.nulls_first) {
        (true, false, Some(nf)) => {
            if nf {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true, Some(nf)) => {
            if nf {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        _ => {
            let c = x.total_cmp(y);
            if o.desc {
                c.reverse()
            } else {
                c
            }
        }
    }
}

/// Rows and scope of one FROM item. Base sources of every storage model are
/// read through the common [`Relation`](super::sources::Relation) interface.
fn source_rows(
    q: &QCtx,
    item: &FromItem,
    filter: Option<&Expr>,
    fields: Vec<String>,
    sole: bool,
    outer: Option<&Env>,
    level: usize,
) -> Result<(Scope, Vec<Vec<Value>>)> {
    let (columns, rows) = match super::sources::open(item, fields, sole) {
        Some(rel) => {
            let set = rel.read(q, filter)?;
            if q.analyze {
                let n = set.rows.len();
                q.note(|| format!("{}: {n} row(s) read from {} storage", item.alias, rel.model().name()));
            }
            (set.columns, set.rows)
        }
        None => {
            let Source::Derived(sub) = &item.src else { unreachable!("base sources are relations") };
            let (cols, rows) = run_query(q, sub, outer, level)?;
            q.note(|| format!("{}: derived table", item.alias));
            (cols, rows)
        }
    };
    Ok((columns.into_iter().map(|c| (item.alias.clone(), c)).collect(), rows))
}

/// Equality conjuncts of an ON clause between a left column and a right column.
fn equi_keys(e: &Expr, left: &[(String, String)], right: &[(String, String)], out: &mut Vec<(usize, usize)>) {
    match e {
        Expr::Bin(a, Op::And, b) => {
            equi_keys(a, left, right, out);
            equi_keys(b, left, right, out);
        }
        Expr::Bin(a, Op::Eq, b) => {
            if let (Expr::Col(qa, na), Expr::Col(qb, nb)) = (&**a, &**b) {
                let side = |q: &Option<String>, n: &str| -> Option<(bool, usize)> {
                    let l = find_col(left, q, n).ok()?;
                    let r = find_col(right, q, n).ok()?;
                    match (l, r) {
                        (Some(i), None) => Some((true, i)),
                        (None, Some(i)) => Some((false, i)),
                        _ => None,
                    }
                };
                match (side(qa, na), side(qb, nb)) {
                    (Some((true, l)), Some((false, r))) | (Some((false, r)), Some((true, l))) => out.push((l, r)),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn join(
    q: &QCtx,
    scope: &mut Scope,
    left: Vec<Vec<Value>>,
    rscope: Scope,
    right: Vec<Vec<Value>>,
    j: &Join,
    outer: Option<&Env>,
    level: usize,
) -> Result<Vec<Vec<Value>>> {
    let (lw, rw) = (scope.len(), rscope.len());
    scope.extend(rscope);
    let scope: &Scope = scope;
    let mut out = Vec::new();
    let Some(on) = &j.on else {
        q.note(|| format!("{}: cross join", j.item.alias));
        for l in &left {
            for r in &right {
                let mut c = l.clone();
                c.extend(r.iter().cloned());
                out.push(c);
            }
        }
        return Ok(out);
    };
    let keep_left = matches!(j.kind, JoinKind::Left | JoinKind::Full);
    let keep_right = matches!(j.kind, JoinKind::Right | JoinKind::Full);
    let mut right_matched = vec![false; right.len()];
    let test = |combined: &[Value]| -> Result<bool> { is_true(&eval(on, &Env::new(q, scope, combined, outer, level))?) };
    let mut keys_ = Vec::new();
    equi_keys(on, &scope[..lw], &scope[lw..], &mut keys_);
    let kind = match j.kind {
        JoinKind::Inner => "inner",
        JoinKind::Left => "left",
        JoinKind::Right => "right",
        JoinKind::Full => "full",
        JoinKind::Cross => "cross",
    };
    let mut probe = |l: &Vec<Value>, cands: &mut dyn Iterator<Item = usize>| -> Result<()> {
        let mut matched = false;
        for ri in cands {
            let mut c = l.clone();
            c.extend(right[ri].iter().cloned());
            if test(&c)? {
                out.push(c);
                matched = true;
                right_matched[ri] = true;
            }
        }
        if !matched && keep_left {
            let mut c = l.clone();
            c.resize(lw + rw, Value::Null);
            out.push(c);
        }
        Ok(())
    };
    if keys_.is_empty() {
        q.note(|| format!("{}: nested loop {kind} join", j.item.alias));
        for l in &left {
            probe(l, &mut (0..right.len()))?;
        }
    } else {
        q.note(|| format!("{}: hash {kind} join", j.item.alias));
        let mut table: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
        for (ri, r) in right.iter().enumerate() {
            if let Some(k) = join_key(keys_.iter().map(|(_, rc)| &r[*rc])) {
                table.entry(k).or_default().push(ri);
            }
        }
        let empty = Vec::new();
        for l in &left {
            let cands = join_key(keys_.iter().map(|(lc, _)| &l[*lc])).and_then(|k| table.get(&k)).unwrap_or(&empty);
            probe(l, &mut cands.iter().copied())?;
        }
    }
    if keep_right {
        for (ri, r) in right.iter().enumerate() {
            if !right_matched[ri] {
                let mut c = vec![Value::Null; lw];
                c.extend(r.iter().cloned());
                out.push(c);
            }
        }
    }
    Ok(out)
}

pub(super) fn select(q: &QCtx, s: &Select, outer: Option<&Env>, level: usize) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    // 1. FROM and JOINs build the working rows and the column scope.
    let mut scope = Scope::new();
    let mut rows: Vec<Vec<Value>> = vec![vec![]];
    if let Some(from) = &s.from {
        // WHERE may narrow the first table's scan unless an outer join can
        // put NULLs into its columns.
        let preserves_first = s.joins.iter().all(|j| matches!(j.kind, JoinKind::Inner | JoinKind::Left | JoinKind::Cross));
        let pushdown = if preserves_first { s.filter.as_ref() } else { None };
        let fields = |item: &FromItem| match item.src {
            Source::Collection(_) => super::walk::fields_of(s, &item.alias),
            _ => Vec::new(),
        };
        let sole = s.joins.is_empty() && outer.is_none();
        let (sc, r) = source_rows(q, from, pushdown, fields(from), sole, outer, level)?;
        scope = sc;
        rows = r;
        for j in &s.joins {
            if scope.iter().any(|(a, _)| a == &j.item.alias) {
                return Err(Error::Schema(format!("table name '{}' used twice; give it an alias", j.item.alias)));
            }
            let (rscope, right) = source_rows(q, &j.item, None, fields(&j.item), false, outer, level)?;
            rows = join(q, &mut scope, rows, rscope, right, j, outer, level)?;
        }
    }
    macro_rules! env {
        ($row:expr) => {
            Env::new(q, &scope, $row, outer, level)
        };
    }

    // 2. WHERE
    if let Some(f) = &s.filter {
        if has_agg(f) {
            return perr("aggregate functions are not allowed in WHERE");
        }
        let mut kept = Vec::new();
        for r in rows {
            if is_true(&eval(f, &env!(&r))?)? {
                kept.push(r);
            }
        }
        rows = kept;
    }

    // 3. Expand the select list.
    let mut items: Vec<(Cow<Expr>, String, bool)> = Vec::new(); // (expr, header, has explicit alias)
    for it in &s.items {
        match it {
            Item::Star => {
                if scope.is_empty() {
                    return perr("SELECT * needs a FROM clause");
                }
                items.extend(scope.iter().map(|(a, c)| (Cow::Owned(Expr::Col(Some(a.clone()), c.clone())), c.clone(), false)));
            }
            Item::TableStar(t) => {
                let before = items.len();
                items.extend(
                    scope
                        .iter()
                        .filter(|(a, _)| a == t)
                        .map(|(a, c)| (Cow::Owned(Expr::Col(Some(a.clone()), c.clone())), c.clone(), false)),
                );
                if items.len() == before {
                    return Err(Error::Schema(format!("no table '{t}' in this query")));
                }
            }
            Item::Expr(e, alias) => items.push((Cow::Borrowed(e), alias.clone().unwrap_or_else(|| label(e)), alias.is_some())),
        }
    }

    // GROUP BY and ORDER BY may name a select alias or a 1-based output position.
    let by_alias = |e: &Expr| -> Option<Expr> {
        match e {
            Expr::Lit(Value::Int(k)) if *k >= 1 && (*k as usize) <= items.len() => Some(items[*k as usize - 1].0.clone().into_owned()),
            Expr::Col(None, n) => items.iter().find(|(_, name, aliased)| *aliased && name == n).map(|i| i.0.clone().into_owned()),
            _ => None,
        }
    };
    let group_by: Vec<Cow<Expr>> = s
        .group_by
        .iter()
        .map(|g| {
            // an input column wins over a select alias of the same name
            let is_input = matches!(g, Expr::Col(q_, n) if matches!(find_col(&scope, q_, n), Ok(Some(_))));
            match (is_input, by_alias(g)) {
                (false, Some(e)) => Cow::Owned(e),
                _ => Cow::Borrowed(g),
            }
        })
        .collect();
    let order: Vec<(Cow<Expr>, &OrderItem)> =
        s.order_by.iter().map(|o| (by_alias(&o.expr).map_or(Cow::Borrowed(&o.expr), Cow::Owned), o)).collect();
    for g in &group_by {
        if has_agg(g) {
            return perr("aggregate functions are not allowed in GROUP BY");
        }
    }

    // 4. Project, grouping first if the query aggregates.
    let aggregated = !group_by.is_empty()
        || items.iter().any(|i| has_agg(&i.0))
        || s.having.as_ref().map_or(false, has_agg)
        || order.iter().any(|o| has_agg(&o.0));

    let project = |env: &Env| -> Result<(Vec<Value>, Vec<Value>)> {
        let out = items.iter().map(|i| eval(&i.0, env)).collect::<Result<Vec<_>>>()?;
        let keys_ = order.iter().map(|o| eval(&o.0, env)).collect::<Result<Vec<_>>>()?;
        Ok((keys_, out))
    };

    let mut out: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    if aggregated {
        let groups: Vec<Vec<Vec<Value>>> = if group_by.is_empty() {
            vec![rows]
        } else {
            let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
            let mut groups: Vec<Vec<Vec<Value>>> = Vec::new();
            for r in rows {
                let key = group_by.iter().map(|e| eval(e, &env!(&r))).collect::<Result<Vec<_>>>()?;
                let gi = *index.entry(group_key(key.iter())).or_insert_with(|| {
                    groups.push(Vec::new());
                    groups.len() - 1
                });
                groups[gi].push(r);
            }
            groups
        };
        let nulls = vec![Value::Null; scope.len()];
        for g in &groups {
            let genv = Env { group: Some(g), ..env!(g.first().unwrap_or(&nulls)) };
            if let Some(h) = &s.having {
                if !is_true(&eval(h, &genv)?)? {
                    continue;
                }
            }
            out.push(project(&genv)?);
        }
    } else {
        if s.having.is_some() {
            return perr("HAVING needs GROUP BY or an aggregate");
        }
        for r in &rows {
            out.push(project(&env!(r))?);
        }
    }

    // 5. DISTINCT, ORDER BY, OFFSET / LIMIT
    if s.distinct {
        let mut seen = HashSet::new();
        out.retain(|(_, row)| seen.insert(group_key(row.iter())));
    }
    if !order.is_empty() {
        out.sort_by(|a, b| {
            for (i, (_, o)) in order.iter().enumerate() {
                let c = cmp_order(&a.0[i], &b.0[i], o);
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
    }
    let rows = out.into_iter().map(|(_, r)| r).skip(s.offset).take(s.limit.unwrap_or(usize::MAX)).collect();
    Ok((items.into_iter().map(|i| i.1).collect(), rows))
}
