//! Query execution: FROM / JOIN planning, filtering, grouping, ordering and
//! set operations.
//!
//! Planning of the FROM clause, in order:
//!
//! 1. **Predicate pushdown.** WHERE (and ON) is split into AND-ed parts. A
//!    part that reads only one table is evaluated while that table is read,
//!    and its comparisons with constants (or with columns of an enclosing
//!    query) choose the table's access path (`access.rs`). A part is pushed
//!    only to a table whose rows an outer join cannot NULL-extend, so the
//!    result is unchanged.
//! 2. **Join ordering.** When every join is INNER or CROSS, the tables are
//!    joined greedily: first the one expected to yield the fewest rows, then
//!    repeatedly the connected table whose join is expected to be smallest
//!    (estimates from `stats.rs`). Queries with outer joins keep their
//!    written order. Output columns always keep their written order.
//! 3. **Join algorithm**, per join: an *index nested-loop join* (one index
//!    or primary-key lookup per outer row) when the inner table has a usable
//!    key or index on the join columns and that is cheaper than reading it;
//!    otherwise a *hash join* on the equality conditions; otherwise a nested
//!    loop.
//!
//! The chosen paths only narrow the candidate rows. A pushed or reordered
//! condition that fails with a runtime error on some row keeps that row, and
//! the full condition is then evaluated again, strictly, on the final rows,
//! so a reordered plan returns exactly what the written order would.

use super::access::*;
use super::ast::*;
use super::catalog::*;
use super::eval::*;
use super::parser::perr;
use super::plan::{NodeId, Work};
use super::stats::{join_rows, Estimator, TableStats, DEFAULT_OTHER_SEL};
use crate::error::{Error, Result};
use crate::value::Value;
use std::borrow::Cow;
use std::cell::{Cell, OnceCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

pub(super) use super::access::{fetch, find_eq};

/// Output of a SELECT or compound query, with its plan node and estimate.
pub(super) struct Out {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub node: Option<NodeId>,
    pub est: f64,
}

/// Run a query: one SELECT or a compound of them.
pub(super) fn run_query(q: &QCtx, query: &Query, outer: Option<&Env>, level: usize) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
    run_query_node(q, query, outer, level).map(|o| (o.cols, o.rows))
}

pub(super) fn run_query_node(q: &QCtx, query: &Query, outer: Option<&Env>, level: usize) -> Result<Out> {
    let mut out = select(q, &query.head, outer, level)?;
    if query.rest.is_empty() {
        return Ok(out);
    }
    for (op, all, s) in &query.rest {
        let o2 = select(q, s, outer, level)?;
        if o2.cols.len() != out.cols.len() {
            return Err(Error::Schema(format!(
                "each side of a set operation must have the same number of columns ({} vs {})",
                out.cols.len(),
                o2.cols.len()
            )));
        }
        let name = match op {
            SetOp::Union => "Union",
            SetOp::Intersect => "Intersect",
            SetOp::Except => "Except",
        };
        let est = match op {
            SetOp::Union => out.est + o2.est,
            SetOp::Intersect => out.est.min(o2.est),
            SetOp::Except => out.est,
        };
        let run = q.begin(|| format!("{name}{}", if *all { " All" } else { "" }), est, &[out.node, o2.node]);
        let examined = (out.rows.len() + o2.rows.len()) as u64;
        out.rows = combine(*op, *all, std::mem::take(&mut out.rows), o2.rows);
        out.node = q.done(run, out.rows.len(), Work { examined, ..Work::default() });
        out.est = est;
    }
    let cols = &out.cols;
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
        let run = q.begin(|| "Sort".into(), out.est, &[out.node]);
        q.detail(run.id, || format!("Sort Key: {}", order.iter().map(|(i, o)| sort_key_text(&cols[*i], o)).collect::<Vec<_>>().join(", ")));
        out.rows.sort_by(|a, b| {
            for (i, o) in &order {
                let c = cmp_order(&a[*i], &b[*i], o);
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
        out.node = q.done(run, out.rows.len(), Work { examined: out.rows.len() as u64, ..Work::default() });
    }
    if query.limit.is_some() || query.offset > 0 {
        let (rows, node, est) = limit(q, std::mem::take(&mut out.rows), out.node, out.est, query.limit, query.offset);
        out.rows = rows;
        out.node = node;
        out.est = est;
    }
    Ok(out)
}

fn limit(
    q: &QCtx,
    rows: Vec<Vec<Value>>,
    node: Option<NodeId>,
    est: f64,
    lim: Option<usize>,
    offset: usize,
) -> (Vec<Vec<Value>>, Option<NodeId>, f64) {
    let est = (est - offset as f64).max(0.0).min(lim.map_or(f64::INFINITY, |l| l as f64));
    let run = q.begin(|| "Limit".into(), est, &[node]);
    q.detail(run.id, || match lim {
        Some(l) if offset > 0 => format!("Limit: {l} Offset: {offset}"),
        Some(l) => format!("Limit: {l}"),
        None => format!("Offset: {offset}"),
    });
    let examined = rows.len() as u64;
    let rows: Vec<Vec<Value>> = rows.into_iter().skip(offset).take(lim.unwrap_or(usize::MAX)).collect();
    let node = q.done(run, rows.len(), Work { examined, ..Work::default() });
    (rows, node, est)
}

fn sort_key_text(name: &str, o: &OrderItem) -> String {
    let mut s = name.to_string();
    if o.desc {
        s += " DESC";
    }
    match o.nulls_first {
        Some(true) => s += " NULLS FIRST",
        Some(false) => s += " NULLS LAST",
        None => {}
    }
    s
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

// ============================================================ FROM / JOIN

/// One FROM item being planned.
struct Input<'f> {
    alias: &'f str,
    base: Option<Schema>,
    /// Statistics of the base table, loaded when first needed.
    stats: OnceCell<Option<Rc<TableStats>>>,
    /// Rows of a derived table (taken when it is read) and its plan node.
    derived: Option<Vec<Vec<Value>>>,
    derived_node: Option<NodeId>,
    /// Its columns: this range of the planner's `full` scope.
    cols: std::ops::Range<usize>,
    /// Conditions on this input alone, applied while it is read.
    pushed: Vec<&'f Expr>,
    /// Estimated rows after the pushed conditions.
    est: f64,
}

/// What the FROM clause produced.
struct Joined<'f> {
    scope: Scope,
    rows: Vec<Vec<Value>>,
    node: Option<NodeId>,
    est: f64,
    /// Conditions still to check, strictly and in this order, on every row.
    residual: Vec<&'f Expr>,
    residual_sel: f64,
    /// Estimated distinct values per output column (for GROUP BY estimates).
    ndv: Vec<f64>,
}

/// How a join step checks candidate pairs.
enum Cond<'f> {
    None,
    /// The join's ON clause, with exact SQL semantics.
    On(&'f Expr),
    /// Conditions of an inner join; one that fails with an error keeps the
    /// pair, and the final strict check decides.
    Pool(Vec<&'f Expr>),
}

/// Evaluate AND-ed conditions; an error keeps the row and is remembered.
fn tolerant(list: &[&Expr], env: &Env, swallowed: &Cell<bool>) -> bool {
    let mut unknown = false;
    for e in list {
        match eval(e, env).and_then(|v| tri(&v)) {
            Ok(Some(true)) => {}
            Ok(Some(false)) => return false,
            Ok(None) => unknown = true,
            Err(_) => {
                swallowed.set(true);
                return true;
            }
        }
    }
    !unknown
}

/// Evaluate AND-ed conditions exactly as `c1 AND c2 AND ...` would.
fn strict_all(list: &[&Expr], env: &Env) -> Result<bool> {
    let mut acc = Some(true);
    for e in list {
        if acc == Some(false) {
            return Ok(false);
        }
        let r = tri(&eval(e, env)?)?;
        acc = match (acc, r) {
            (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        };
    }
    Ok(acc == Some(true))
}

/// Inputs (FROM-item positions) whose columns an expression reads, or None
/// when it cannot be placed (subquery, aggregate, ambiguous column).
fn refs(e: &Expr, scope: &[(String, String)], owner: &[usize]) -> Option<Vec<usize>> {
    if has_subquery(e) || has_agg(e) {
        return None;
    }
    let mut cols = Vec::new();
    columns_of(e, &mut cols);
    let mut out = Vec::new();
    for (qq, n) in cols {
        match find_col(scope, qq, n) {
            Ok(Some(i)) => out.push(owner[i]),
            Ok(None) => {} // a column of an enclosing query (or an error found later)
            Err(_) => return None,
        }
    }
    out.sort_unstable();
    out.dedup();
    Some(out)
}

fn show_list(list: &[&Expr]) -> String {
    list.iter().map(|e| show(e)).collect::<Vec<_>>().join(" AND ")
}

fn kind_name(k: JoinKind) -> &'static str {
    match k {
        JoinKind::Inner => "inner",
        JoinKind::Left => "left",
        JoinKind::Right => "right",
        JoinKind::Full => "full",
        JoinKind::Cross => "cross",
    }
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

struct Planner<'q, 'f> {
    q: &'q QCtx<'q>,
    outer: Option<&'q Env<'q>>,
    level: usize,
    /// Every column of the FROM clause, in written order.
    full: Scope,
    swallowed: Cell<bool>,
    inputs: Vec<Input<'f>>,
}

impl<'f> Input<'f> {
    /// "table" or "table AS alias", for plan output.
    fn label(&self) -> String {
        match &self.base {
            Some(s) if s.name != self.alias => format!("{} AS {}", s.name, self.alias),
            _ => self.alias.to_string(),
        }
    }
}

impl<'q, 'f> Planner<'q, 'f> {
    /// The columns of input `i`.
    fn sc(&self, i: usize) -> &[(String, String)] {
        &self.full[self.inputs[i].cols.clone()]
    }

    fn est<'s>(&self, i: usize, s: &'s Schema) -> Estimator<'s> {
        let st = self.inputs[i].stats.get_or_init(|| Estimator::stats_for(self.q.ctx, s)).clone();
        Estimator { s, st }
    }

    fn consts(&self) -> Consts<'_> {
        Consts { local: &self.full, outer: self.outer, level: self.level }
    }

    fn preds(&self, i: usize) -> Preds {
        let inp = &self.inputs[i];
        let mut p = Preds::default();
        if let Some(s) = &inp.base {
            for e in &inp.pushed {
                collect_preds(e, s, inp.alias, self.q, self.consts(), &mut p);
            }
        }
        p
    }

    /// Estimated rows of an input after its pushed conditions.
    fn estimate(&mut self, i: usize) {
        let inp = &self.inputs[i];
        let est = match (&inp.base, &inp.derived) {
            (Some(s), _) => {
                let e = self.est(i, s);
                let sel = conjunction_selectivity(&inp.pushed, s, inp.alias, self.q, self.consts(), &e);
                e.rows() * sel
            }
            (None, Some(rows)) => rows.len() as f64 * DEFAULT_OTHER_SEL.powi(inp.pushed.len() as i32),
            _ => 0.0,
        };
        self.inputs[i].est = est;
    }

    /// Distinct values of column `c` of input `i`, bounded by its row estimate.
    fn col_ndv(&self, i: usize, c: usize) -> f64 {
        let inp = &self.inputs[i];
        let n = match &inp.base {
            Some(s) => self.est(i, s).ndv(c),
            None => inp.est,
        };
        n.min(inp.est.max(1.0)).max(1.0)
    }

    /// Plan and read one input, applying its pushed conditions.
    fn scan(&mut self, i: usize) -> Result<(Vec<Vec<Value>>, Option<NodeId>)> {
        let q = self.q;
        if let Some(rows) = self.inputs[i].derived.take() {
            let inp = &self.inputs[i];
            if inp.pushed.is_empty() {
                return Ok((rows, inp.derived_node));
            }
            let run = q.begin(|| format!("Subquery Scan on {}", inp.alias), inp.est, &[inp.derived_node]);
            q.detail(run.id, || format!("Filter: {}", show_list(&inp.pushed)));
            let n = rows.len();
            let kept: Vec<Vec<Value>> = rows
                .into_iter()
                .filter(|r| tolerant(&inp.pushed, &Env::new(q, &self.full[inp.cols.clone()], r, self.outer, self.level), &self.swallowed))
                .collect();
            let node = q.done(run, kept.len(), Work { examined: n as u64, ..Work::default() });
            return Ok((kept, node));
        }
        let p = self.preds(i);
        let inp = &self.inputs[i];
        let s = inp.base.as_ref().expect("a FROM item is a table or a derived table");
        // a primary-key lookup needs no statistics
        let pk_point = !s.pk.is_empty() && s.pk.iter().all(|c| p.eq.contains_key(c));
        let est = if pk_point && !q.recording() { Estimator { s, st: None } } else { self.est(i, s) };
        let a = plan_access(q, s, &p, &est)?;
        let run = q.begin(
            || format!("{} on {}{}", a.op, inp.label(), a.index.as_ref().map_or(String::new(), |i| format!(" using {i}"))),
            inp.est,
            &[],
        );
        if let Some(c) = &a.cond {
            q.detail(run.id, || format!("{}: {c}", if a.index.is_some() { "Index Cond" } else { "Key Cond" }));
        }
        if !inp.pushed.is_empty() {
            q.detail(run.id, || format!("Filter: {}", show_list(&inp.pushed)));
        }
        let (rows, w) = exec_access(q, s, &a)?;
        let kept: Vec<Vec<Value>> = rows
            .into_iter()
            .map(|(_, r)| r)
            .filter(|r| inp.pushed.is_empty() || tolerant(&inp.pushed, &Env::new(q, &self.full[inp.cols.clone()], r, self.outer, self.level), &self.swallowed))
            .collect();
        let node = q.done(run, kept.len(), w);
        Ok((kept, node))
    }

    /// Cost of reading input `i` in full (with its pushed conditions).
    fn scan_cost(&self, i: usize) -> f64 {
        let inp = &self.inputs[i];
        match &inp.base {
            Some(s) => {
                let est = self.est(i, s);
                plan_access(self.q, s, &self.preds(i), &est).map_or(f64::INFINITY, |a| a.cost)
            }
            None => 0.0,
        }
    }

    /// Join the rows built so far (`left`, over scope `lscope`) with input `i`.
    #[allow(clippy::too_many_arguments)]
    fn join(
        &mut self,
        left: Vec<Vec<Value>>,
        lnode: Option<NodeId>,
        lscope: &Scope,
        i: usize,
        kind: JoinKind,
        cond: Cond<'f>,
        est_out: f64,
    ) -> Result<(Vec<Vec<Value>>, Option<NodeId>)> {
        let q = self.q;
        let (outer, level) = (self.outer, self.level);
        let rscope: Scope = self.sc(i).to_vec();
        let (lw, rw) = (lscope.len(), rscope.len());
        let mut scope: Scope = lscope.clone();
        scope.extend(rscope.iter().cloned());
        let mut keys = Vec::new();
        match &cond {
            Cond::On(e) => equi_keys(e, lscope, &rscope, &mut keys),
            Cond::Pool(list) => list.iter().for_each(|e| equi_keys(e, lscope, &rscope, &mut keys)),
            Cond::None => {}
        }
        let filter_text = match &cond {
            Cond::None => None,
            Cond::On(e) => Some(show(e)),
            Cond::Pool(list) if list.is_empty() => None,
            Cond::Pool(list) => Some(show_list(list)),
        };
        let keep_left = matches!(kind, JoinKind::Left | JoinKind::Full);
        let keep_right = matches!(kind, JoinKind::Right | JoinKind::Full);
        let swallowed = &self.swallowed;
        let test = |combined: &[Value]| -> Result<bool> {
            let env = Env::new(q, &scope, combined, outer, level);
            match &cond {
                Cond::None => Ok(true),
                Cond::On(e) => is_true(&eval(e, &env)?),
                Cond::Pool(list) => Ok(tolerant(list, &env, swallowed)),
            }
        };
        let key_text = |keys: &[(usize, usize)]| {
            keys.iter()
                .map(|(l, r)| format!("{}.{} = {}.{}", lscope[*l].0, lscope[*l].1, rscope[*r].0, rscope[*r].1))
                .collect::<Vec<_>>()
                .join(" AND ")
        };

        // index nested loop: look the inner rows up per outer row
        if !keys.is_empty() && matches!(kind, JoinKind::Inner | JoinKind::Left) {
            if let Some(s) = &self.inputs[i].base {
                let est = self.est(i, s);
                if let Some(probe) = choose_probe(s, &est, &keys) {
                    let hash_cost = self.scan_cost(i) + self.inputs[i].est * HASH_BUILD + left.len() as f64 * HASH_PROBE;
                    if left.len() as f64 * probe.cost < hash_cost {
                        let inp = &self.inputs[i];
                        let pnode = q.register(|| format!("Index Probe on {} using {}", inp.label(), probe.index), probe.est, &[]);
                        q.detail(pnode, || {
                            let parts: Vec<String> = probe
                                .cols
                                .iter()
                                .zip(&probe.outer_pos)
                                .map(|(c, p)| format!("{} = {}.{}", s.cols[*c].name, lscope[*p].0, lscope[*p].1))
                                .collect();
                            format!("Index Cond: {}", parts.join(" AND "))
                        });
                        if !inp.pushed.is_empty() {
                            q.detail(pnode, || format!("Filter: {}", show_list(&inp.pushed)));
                        }
                        let run = q.begin(|| format!("Index Nested Loop ({})", kind_name(kind)), est_out, &[lnode, pnode]);
                        if let Some(t) = &filter_text {
                            q.detail(run.id, || format!("Join Filter: {t}"));
                        }
                        let mut out = Vec::new();
                        let mut tested = 0u64;
                        for l in &left {
                            let t = q.timer();
                            let mut w = Work::default();
                            let cands: Vec<Vec<Value>> = probe
                                .fetch(q, s, l, &mut w)?
                                .into_iter()
                                .map(|(_, r)| r)
                                .filter(|r| inp.pushed.is_empty() || tolerant(&inp.pushed, &Env::new(q, &rscope, r, outer, level), swallowed))
                                .collect();
                            q.record(pnode, cands.len(), w, t);
                            let mut matched = false;
                            for r in cands {
                                tested += 1;
                                let mut c = l.clone();
                                c.extend(r);
                                if test(&c)? {
                                    out.push(c);
                                    matched = true;
                                }
                            }
                            if !matched && keep_left {
                                let mut c = l.clone();
                                c.resize(lw + rw, Value::Null);
                                out.push(c);
                            }
                        }
                        let node = q.done(run, out.len(), Work { examined: tested, ..Work::default() });
                        return Ok((out, node));
                    }
                }
            }
        }

        let (right, rnode) = self.scan(i)?;
        let swallowed = &self.swallowed;
        let test = |combined: &[Value]| -> Result<bool> {
            let env = Env::new(q, &scope, combined, outer, level);
            match &cond {
                Cond::None => Ok(true),
                Cond::On(e) => is_true(&eval(e, &env)?),
                Cond::Pool(list) => Ok(tolerant(list, &env, swallowed)),
            }
        };
        let label = if matches!(cond, Cond::None) || (filter_text.is_none() && keys.is_empty()) {
            format!("Nested Loop ({})", if kind == JoinKind::Inner { "cross" } else { kind_name(kind) })
        } else if keys.is_empty() {
            format!("Nested Loop ({})", kind_name(kind))
        } else {
            format!("Hash Join ({})", kind_name(kind))
        };
        let run = q.begin(|| label, est_out, &[lnode, rnode]);
        if !keys.is_empty() {
            q.detail(run.id, || format!("Hash Cond: {}", key_text(&keys)));
        }
        if let Some(t) = &filter_text {
            q.detail(run.id, || format!("Join Filter: {t}"));
        }
        let mut out = Vec::new();
        let mut tested = 0u64;
        let mut right_matched = vec![false; right.len()];
        let mut probe_rows = |l: &Vec<Value>, cands: &mut dyn Iterator<Item = usize>, tested: &mut u64| -> Result<()> {
            let mut matched = false;
            for ri in cands {
                *tested += 1;
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
        if keys.is_empty() {
            for l in &left {
                probe_rows(l, &mut (0..right.len()), &mut tested)?;
            }
        } else {
            let mut table: HashMap<Vec<u8>, Vec<usize>> = HashMap::new();
            for (ri, r) in right.iter().enumerate() {
                if let Some(k) = join_key(keys.iter().map(|(_, rc)| &r[*rc])) {
                    table.entry(k).or_default().push(ri);
                }
            }
            let empty = Vec::new();
            for l in &left {
                let cands = join_key(keys.iter().map(|(lc, _)| &l[*lc])).and_then(|k| table.get(&k)).unwrap_or(&empty);
                probe_rows(l, &mut cands.iter().copied(), &mut tested)?;
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
        let node = q.done(run, out.len(), Work { examined: tested, ..Work::default() });
        Ok((out, node))
    }

    /// Estimated rows of joining `l` rows (columns `lcols`: (input, column)
    /// per position) with input `i` on `keys`.
    fn join_estimate(&self, l: f64, lcols: &[(usize, usize)], i: usize, keys: &[(usize, usize)], other_conds: usize) -> f64 {
        let ndvs: Vec<(f64, f64)> = keys
            .iter()
            .map(|(lp, rc)| {
                let (li, lc) = lcols[*lp];
                (self.col_ndv(li, lc), self.col_ndv(i, *rc))
            })
            .collect();
        join_rows(l, self.inputs[i].est, &ndvs) * DEFAULT_OTHER_SEL.powi(other_conds as i32)
    }
}

fn from_clause<'f>(q: &QCtx, s: &'f Select, outer: Option<&Env>, level: usize) -> Result<Joined<'f>> {
    let mut wconj: Vec<&Expr> = Vec::new();
    if let Some(f) = &s.filter {
        conjuncts(f, &mut wconj);
    }
    let Some(from) = &s.from else {
        let run = q.begin(|| "Result".into(), 1.0, &[]);
        let node = q.done(run, 1, Work::default());
        return Ok(Joined { scope: Scope::new(), rows: vec![vec![]], node, est: 1.0, residual_sel: DEFAULT_OTHER_SEL.powi(wconj.len() as i32), residual: wconj, ndv: Vec::new() });
    };

    // 1. the FROM items
    let mut items: Vec<(&'f FromItem, JoinKind, Option<&'f Expr>)> = vec![(from, JoinKind::Inner, None)];
    items.extend(s.joins.iter().map(|j| (&j.item, j.kind, j.on.as_ref())));
    let mut inputs: Vec<Input> = Vec::with_capacity(items.len());
    let mut full = Scope::new();
    let mut owner = Vec::new();
    let mut ends = Vec::new();
    for (k, (item, _, _)) in items.iter().enumerate() {
        if items[..k].iter().any(|(it, _, _)| it.alias == item.alias) {
            return Err(Error::Schema(format!("table name '{}' used twice; give it an alias", item.alias)));
        }
        let inp = match &item.src {
            Source::Table(name) => {
                let sch = Schema::load(q.ctx, name)?;
                let stats = OnceCell::new();
                if q.recording() {
                    let st = stats.get_or_init(|| Estimator::stats_for(q.ctx, &sch));
                    q.table_stats(&sch.name, st.as_ref().map(|st| st.rows));
                }
                let start = full.len();
                full.extend(sch.cols.iter().map(|c| (item.alias.clone(), c.name.clone())));
                Input {
                    alias: &item.alias,
                    cols: start..full.len(),
                    est: 0.0, // set by `estimate`
                    stats,
                    base: Some(sch),
                    derived: None,
                    derived_node: None,
                    pushed: Vec::new(),
                }
            }
            Source::Derived(sub) => {
                let o = run_query_node(q, sub, outer, level)?;
                let run = q.begin(|| format!("Derived Table {}", item.alias), o.est, &[o.node]);
                let node = q.done(run, o.rows.len(), Work::default());
                let start = full.len();
                full.extend(o.cols.iter().map(|c| (item.alias.clone(), c.clone())));
                Input {
                    alias: &item.alias,
                    cols: start..full.len(),
                    est: o.rows.len() as f64,
                    stats: OnceCell::new(),
                    base: None,
                    derived: Some(o.rows),
                    derived_node: node,
                    pushed: Vec::new(),
                }
            }
        };
        owner.extend(std::iter::repeat_n(k, inp.cols.len()));
        ends.push(full.len());
        inputs.push(inp);
    }
    let n = inputs.len();
    let kinds: Vec<JoinKind> = items.iter().map(|(_, k, _)| *k).collect();
    let all_inner = kinds[1..].iter().all(|k| matches!(k, JoinKind::Inner | JoinKind::Cross));
    // ON may be moved only if every column it names means the same thing in
    // the whole FROM clause as at its written position
    let on_movable = items.iter().enumerate().all(|(i, (_, _, on))| match on {
        None => true,
        Some(e) if has_subquery(e) => false,
        Some(e) => {
            let mut cols = Vec::new();
            columns_of(e, &mut cols);
            cols.iter().all(|(qq, nm)| match (find_col(&full[..ends[i]], qq, nm), find_col(&full, qq, nm)) {
                (Ok(a), Ok(b)) => a == b,
                _ => false,
            })
        }
    });
    let reorder = n >= 2 && all_inner && on_movable;
    let mut p = Planner { q, outer, level, full, swallowed: Cell::new(false), inputs };

    let (scope, rows, node, est, residual, residual_sel);
    if reorder {
        // ---- inner joins: one pool of conditions, greedy order
        let mut pool: Vec<(&'f Expr, Option<Vec<usize>>)> = Vec::new();
        for (_, _, on) in &items {
            if let Some(e) = on {
                let mut c = Vec::new();
                conjuncts(e, &mut c);
                pool.extend(c.into_iter().map(|c| (c, refs(c, &p.full, &owner))));
            }
        }
        pool.extend(wconj.iter().map(|c| (*c, refs(c, &p.full, &owner))));
        let mut used = vec![false; pool.len()];
        for (k, (e, r)) in pool.iter().enumerate() {
            if let Some(r) = r {
                if r.len() <= 1 {
                    p.inputs[r.first().copied().unwrap_or(0)].pushed.push(e);
                    used[k] = true;
                }
            }
        }
        for i in 0..n {
            p.estimate(i);
        }
        let start = (0..n).min_by(|a, b| p.inputs[*a].est.total_cmp(&p.inputs[*b].est).then(a.cmp(b))).unwrap();
        let mut order = vec![start];
        let mut joined = vec![false; n];
        joined[start] = true;
        let (mut cur, mut cur_node) = p.scan(start)?;
        let mut cur_scope = p.sc(start).to_vec();
        let mut cur_cols: Vec<(usize, usize)> = (0..cur_scope.len()).map(|c| (start, c)).collect();
        let mut cur_est = p.inputs[start].est;
        while order.len() < n {
            // pick the next input
            let mut best: Option<(bool, f64, usize)> = None;
            for r in (0..n).filter(|r| !joined[*r]) {
                let applicable: Vec<usize> = (0..pool.len())
                    .filter(|k| !used[*k])
                    .filter(|k| pool[*k].1.as_ref().map_or(false, |rs| rs.contains(&r) && rs.iter().all(|x| *x == r || joined[*x])))
                    .collect();
                let mut keys = Vec::new();
                for k in &applicable {
                    equi_keys(pool[*k].0, &cur_scope, p.sc(r), &mut keys);
                }
                let connected = !applicable.is_empty();
                let e = p.join_estimate(cur_est, &cur_cols, r, &keys, applicable.len().saturating_sub(keys.len()));
                let better = match best {
                    None => true,
                    Some((bc, be, _)) => (connected && !bc) || (connected == bc && e < be),
                };
                if better {
                    best = Some((connected, e, r));
                }
            }
            let (_, e, r) = best.unwrap();
            let picked: Vec<usize> = (0..pool.len())
                .filter(|k| !used[*k] && pool[*k].1.as_ref().map_or(false, |rs| rs.contains(&r) && rs.iter().all(|x| *x == r || joined[*x])))
                .collect();
            let conds: Vec<&'f Expr> = picked.iter().map(|k| pool[*k].0).collect();
            for k in picked {
                used[k] = true;
            }
            let kind = if conds.is_empty() { JoinKind::Cross } else { JoinKind::Inner };
            let cond = if conds.is_empty() { Cond::None } else { Cond::Pool(conds) };
            let (rows2, node2) = p.join(cur, cur_node, &cur_scope, r, if kind == JoinKind::Cross { JoinKind::Inner } else { kind }, cond, e)?;
            cur = rows2;
            cur_node = node2;
            cur_scope.extend(p.sc(r).iter().cloned());
            cur_cols.extend((0..p.inputs[r].cols.len()).map(|c| (r, c)));
            cur_est = e;
            joined[r] = true;
            order.push(r);
        }
        // back to the written column order
        if order.windows(2).any(|w| w[0] > w[1]) {
            let mut at = HashMap::new();
            for (pos, (i, c)) in cur_cols.iter().enumerate() {
                at.insert((*i, *c), pos);
            }
            let perm: Vec<usize> = (0..n).flat_map(|i| (0..p.inputs[i].cols.len()).map(move |c| (i, c))).map(|k| at[&k]).collect();
            for r in cur.iter_mut() {
                let old = std::mem::take(r);
                *r = perm.iter().map(|&k| old[k].clone()).collect();
            }
        }
        let mut res: Vec<&'f Expr> = Vec::new();
        if p.swallowed.get() {
            // a condition failed on some row: check everything again, as written
            for (_, _, on) in &items {
                if let Some(e) = on {
                    conjuncts(e, &mut res);
                }
            }
            res.extend(wconj.iter().copied());
        } else {
            res.extend(pool.iter().zip(&used).filter(|(_, u)| !**u).map(|((e, _), _)| *e));
        }
        residual_sel = DEFAULT_OTHER_SEL.powi(pool.iter().zip(&used).filter(|(_, u)| !**u).count() as i32);
        residual = res;
        scope = std::mem::take(&mut p.full);
        rows = cur;
        node = cur_node;
        est = cur_est;
    } else {
        // ---- written order; push only where no outer join NULL-extends
        let nullable = |i: usize| -> bool {
            let later_right = kinds[i + 1..].iter().any(|k| matches!(k, JoinKind::Right | JoinKind::Full));
            later_right || (i > 0 && matches!(kinds[i], JoinKind::Left | JoinKind::Full))
        };
        let mut all_pushed = true;
        for c in &wconj {
            match refs(c, &p.full, &owner) {
                Some(r) if r.len() <= 1 && !nullable(r.first().copied().unwrap_or(0)) => {
                    p.inputs[r.first().copied().unwrap_or(0)].pushed.push(c);
                }
                _ => all_pushed = false,
            }
        }
        for (i, (_, kind, on)) in items.iter().enumerate().skip(1) {
            if let (Some(e), JoinKind::Inner | JoinKind::Left) = (on, kind) {
                let mut c = Vec::new();
                conjuncts(e, &mut c);
                for c in c {
                    if refs(c, &p.full[..ends[i]], &owner[..ends[i]]).as_deref() == Some(&[i]) {
                        p.inputs[i].pushed.push(c);
                    }
                }
            }
        }
        // estimates steer join decisions and are shown by EXPLAIN; a lone
        // table's access path is costed when it is read
        if n > 1 || q.recording() {
            for i in 0..n {
                p.estimate(i);
            }
        }
        let (mut cur, mut cur_node) = p.scan(0)?;
        let mut cur_scope = if n > 1 { p.sc(0).to_vec() } else { Scope::new() };
        let mut cur_cols: Vec<(usize, usize)> = (0..p.inputs[0].cols.len()).map(|c| (0, c)).collect();
        let mut cur_est = p.inputs[0].est;
        for (i, (_, kind, on)) in items.iter().enumerate().skip(1) {
            let mut keys = Vec::new();
            if let Some(e) = on {
                equi_keys(e, &cur_scope, p.sc(i), &mut keys);
            }
            let mut e = match on {
                Some(_) => p.join_estimate(cur_est, &cur_cols, i, &keys, usize::from(keys.is_empty())),
                None => cur_est * p.inputs[i].est,
            };
            match kind {
                JoinKind::Left => e = e.max(cur_est),
                JoinKind::Right => e = e.max(p.inputs[i].est),
                JoinKind::Full => e = e.max(cur_est).max(p.inputs[i].est),
                _ => {}
            }
            let cond = match on {
                Some(e) => Cond::On(e),
                None => Cond::None,
            };
            let k = if *kind == JoinKind::Cross { JoinKind::Inner } else { *kind };
            let (rows2, node2) = p.join(cur, cur_node, &cur_scope, i, k, cond, e)?;
            cur = rows2;
            cur_node = node2;
            cur_scope.extend(p.sc(i).iter().cloned());
            cur_cols.extend((0..p.inputs[i].cols.len()).map(|c| (i, c)));
            cur_est = e;
        }
        let recheck = p.swallowed.get() || !all_pushed;
        residual_sel = if recheck && !p.swallowed.get() { DEFAULT_OTHER_SEL } else { 1.0 };
        residual = if recheck { wconj } else { Vec::new() };
        drop(cur_scope);
        scope = std::mem::take(&mut p.full); // written order: the same columns
        rows = cur;
        node = cur_node;
        est = cur_est;
    }
    let ndv = if s.group_by.is_empty() {
        Vec::new()
    } else {
        (0..n).flat_map(|i| (0..p.inputs[i].cols.len()).map(move |c| (i, c))).map(|(i, c)| p.col_ndv(i, c)).collect()
    };
    Ok(Joined { scope, rows, node, est, residual, residual_sel, ndv })
}

pub(super) fn select(q: &QCtx, s: &Select, outer: Option<&Env>, level: usize) -> Result<Out> {
    if let Some(f) = &s.filter {
        if has_agg(f) {
            return perr("aggregate functions are not allowed in WHERE");
        }
    }
    // 1. FROM, JOINs and the pushed-down parts of WHERE
    let j = from_clause(q, s, outer, level)?;
    let scope = j.scope;
    let mut rows = j.rows;
    let mut node = j.node;
    let mut est = j.est;
    macro_rules! env {
        ($row:expr) => {
            Env::new(q, &scope, $row, outer, level)
        };
    }

    // 2. what is left of WHERE
    if !j.residual.is_empty() {
        let e2 = est * j.residual_sel;
        let run = q.begin(|| "Filter".into(), e2, &[node]);
        q.detail(run.id, || format!("Filter: {}", show_list(&j.residual)));
        let examined = rows.len() as u64;
        let mut kept = Vec::new();
        for r in rows {
            if strict_all(&j.residual, &env!(&r))? {
                kept.push(r);
            }
        }
        rows = kept;
        est = e2;
        node = q.done(run, rows.len(), Work { examined, ..Work::default() });
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
    let output_text = || format!("Output: {}", items.iter().map(|i| i.1.clone()).collect::<Vec<_>>().join(", "));

    let mut out: Vec<(Vec<Value>, Vec<Value>)> = Vec::new();
    let examined = rows.len() as u64;
    if aggregated {
        let groups_est = if group_by.is_empty() {
            1.0
        } else {
            let mut g = 1.0f64;
            for e in &group_by {
                g *= match &**e {
                    Expr::Col(q_, n) => match find_col(&scope, q_, n) {
                        Ok(Some(i)) => j.ndv.get(i).copied().unwrap_or(est / 10.0),
                        _ => est / 10.0,
                    },
                    _ => est / 10.0,
                };
            }
            g.min(est).max(if est > 0.0 { 1.0 } else { 0.0 })
        };
        let groups_est = if s.having.is_some() { groups_est * DEFAULT_OTHER_SEL } else { groups_est };
        let run = q.begin(|| if group_by.is_empty() { "Aggregate".into() } else { "Hash Aggregate".into() }, groups_est, &[node]);
        if !group_by.is_empty() {
            q.detail(run.id, || format!("Group Key: {}", group_by.iter().map(|g| show(g)).collect::<Vec<_>>().join(", ")));
        }
        if let Some(h) = &s.having {
            q.detail(run.id, || format!("Having: {}", show(h)));
        }
        q.detail(run.id, output_text);
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
        est = groups_est;
        node = q.done(run, out.len(), Work { examined, ..Work::default() });
    } else {
        if s.having.is_some() {
            return perr("HAVING needs GROUP BY or an aggregate");
        }
        let run = q.begin(|| "Project".into(), est, &[node]);
        q.detail(run.id, output_text);
        for r in &rows {
            out.push(project(&env!(r))?);
        }
        node = q.done(run, out.len(), Work { examined, ..Work::default() });
    }

    // 5. DISTINCT, ORDER BY, OFFSET / LIMIT
    if s.distinct {
        let run = q.begin(|| "Distinct".into(), est, &[node]);
        let examined = out.len() as u64;
        let mut seen = HashSet::new();
        out.retain(|(_, row)| seen.insert(group_key(row.iter())));
        node = q.done(run, out.len(), Work { examined, ..Work::default() });
    }
    if !order.is_empty() {
        let run = q.begin(|| "Sort".into(), est, &[node]);
        q.detail(run.id, || format!("Sort Key: {}", order.iter().map(|(_, o)| sort_key_text(&show(&o.expr), o)).collect::<Vec<_>>().join(", ")));
        out.sort_by(|a, b| {
            for (i, (_, o)) in order.iter().enumerate() {
                let c = cmp_order(&a.0[i], &b.0[i], o);
                if c != Ordering::Equal {
                    return c;
                }
            }
            Ordering::Equal
        });
        node = q.done(run, out.len(), Work { examined: out.len() as u64, ..Work::default() });
    }
    let mut rows: Vec<Vec<Value>> = out.into_iter().map(|(_, r)| r).collect();
    if s.limit.is_some() || s.offset > 0 {
        let (r, n2, e2) = limit(q, rows, node, est, s.limit, s.offset);
        rows = r;
        node = n2;
        est = e2;
    }
    Ok(Out { cols: items.into_iter().map(|i| i.1).collect(), rows, node, est })
}
