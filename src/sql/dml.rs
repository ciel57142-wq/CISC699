//! INSERT, UPDATE, DELETE and TRUNCATE, and the row-change engine that
//! enforces every constraint.
//!
//! A statement turns into a batch of row changes (old row -> new row, either
//! side optional) that is applied in phases, so constraints are checked
//! against the statement's result rather than its intermediate states:
//!
//! 1. every old row and its index entries are removed;
//! 2. every new row is type-checked, CHECK-ed and written, refusing a
//!    duplicate primary key or unique-index value;
//! 3. new foreign-key values must exist in the referenced table;
//! 4. referenced keys that disappeared trigger each referencing foreign
//!    key's action (RESTRICT, CASCADE, SET NULL). Cascades are queued and
//!    applied in turn, each reading the current rows, so a row touched by
//!    two cascades is never written back from a stale copy.
//!
//! All writes go to the statement's private write set, which the caller
//! discards on any error: a failing statement changes nothing.

use super::ast::*;
use super::catalog::*;
use super::eval::*;
use super::query::{fetch, find_eq, run_query};
use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use crate::Output;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};

/// Upper bound on queued cascade steps for one statement.
const MAX_CASCADE_STEPS: usize = 100_000;

pub(super) struct Change {
    pub old: Option<(Vec<u8>, Vec<Value>)>,
    pub new: Option<Vec<Value>>,
}

/// Check and convert a value for a column.
pub(super) fn coerce(col: &Column, v: Value, in_pk: bool) -> Result<Value> {
    match (col.ty, v) {
        (_, Value::Null) => {
            if col.not_null || in_pk {
                Err(Error::Constraint(format!("column '{}' cannot be NULL", col.name)))
            } else {
                Ok(Value::Null)
            }
        }
        (Ty::Int, Value::Int(i)) => Ok(Value::Int(i)),
        (Ty::Int, Value::Float(f)) if f.fract() == 0.0 && f.abs() < 9e18 => Ok(Value::Int(f as i64)),
        (Ty::Float, Value::Int(i)) => Ok(Value::Float(i as f64)),
        (Ty::Float, Value::Float(f)) if f.is_finite() => Ok(Value::Float(f)),
        (Ty::Text, Value::Text(s)) => {
            if let Some(n) = col.max_len {
                if s.chars().count() > n {
                    return Err(Error::Type(format!(
                        "value too long for column '{}' (VARCHAR({n})): {} characters",
                        col.name,
                        s.chars().count()
                    )));
                }
            }
            Ok(Value::Text(s))
        }
        (Ty::Bool, Value::Bool(b)) => Ok(Value::Bool(b)),
        (ty, v) => Err(Error::Type(format!("column '{}' expects {}, got {}", col.name, ty.name().to_uppercase(), v.to_json()))),
    }
}

/// Coerce every value of a row and evaluate the table's CHECK constraints.
pub(super) fn validate_row(q: &QCtx, s: &Schema, row: Vec<Value>) -> Result<Vec<Value>> {
    if row.len() != s.cols.len() {
        return Err(Error::Corrupt(format!("row of table '{}' has {} values, expected {}", s.name, row.len(), s.cols.len())));
    }
    let row = row
        .into_iter()
        .enumerate()
        .map(|(i, v)| coerce(&s.cols[i], v, s.pk.contains(&i)))
        .collect::<Result<Vec<_>>>()?;
    if !s.checks.is_empty() {
        let scope = s.scope(&s.name);
        let env = Env::new(q, &scope, &row, None, 0);
        for c in &s.checks {
            if tri(&eval(&c.expr, &env)?)? == Some(false) {
                return Err(Error::Constraint(format!(
                    "row violates check constraint '{}' on table '{}'",
                    c.name, s.name
                )));
            }
        }
    }
    Ok(row)
}

fn show(vals: &[Value]) -> String {
    vals.iter().map(|v| v.to_json()).collect::<Vec<_>>().join(", ")
}

fn same(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.total_cmp(y) == Ordering::Equal)
}

/// Refuse a row whose values for a unique index already belong to another row.
pub(super) fn check_unique(ctx: &Ctx, s: &Schema, id: &[u8], row: &[Value]) -> Result<()> {
    for ix in s.indexes.iter().filter(|i| i.unique) {
        let pos = s.positions(&ix.cols)?;
        let vals: Vec<&Value> = pos.iter().map(|&p| &row[p]).collect();
        if vals.iter().any(|v| v.is_null()) {
            continue; // NULLs never conflict
        }
        let mut prefix = index_base(&s.name, &ix.cols);
        prefix.extend(index_values(&vals).unwrap_or_default());
        if ctx.scan(&prefix).iter().any(|(k, _)| &k[prefix.len()..] != id) {
            return Err(Error::Constraint(format!(
                "duplicate value for unique index '{}' on table '{}': ({}) = ({})",
                ix.name,
                s.name,
                ix.cols.join(", "),
                vals.iter().map(|v| v.to_json()).collect::<Vec<_>>().join(", ")
            )));
        }
    }
    Ok(())
}

pub(super) fn exists_eq(ctx: &Ctx, s: &Schema, cols: &[usize], vals: &[Value]) -> Result<bool> {
    let q = QCtx::new(ctx);
    Ok(!find_eq(&q, s, cols, vals)?.is_empty())
}

/// Can rows be looked up by `cols` without a full scan?
fn has_lookup(s: &Schema, cols: &[usize]) -> bool {
    let leading = |pos: &[usize]| pos.len() >= cols.len() && cols.iter().all(|c| pos[..cols.len()].contains(c));
    leading(&s.pk) || s.indexes.iter().any(|ix| s.positions(&ix.cols).map_or(false, |p| leading(&p)))
}

/// Rows of `s` whose `cols` equal one of `keys_`; returns (key, row, index into keys_).
fn find_matching(ctx: &Ctx, s: &Schema, cols: &[usize], keys_: &[Vec<Value>]) -> Result<Vec<(Vec<u8>, Vec<Value>, usize)>> {
    let mut out = Vec::new();
    if has_lookup(s, cols) || keys_.len() <= 4 {
        let q = QCtx::new(ctx);
        for (i, vals) in keys_.iter().enumerate() {
            for (k, r) in find_eq(&q, s, cols, vals)? {
                out.push((k, r, i));
            }
        }
    } else {
        let wanted: HashMap<Vec<u8>, usize> = keys_.iter().enumerate().map(|(i, v)| (group_key(v.iter()), i)).collect();
        for (k, r) in all_rows(ctx, s)? {
            if cols.iter().any(|&c| r[c].is_null()) {
                continue;
            }
            if let Some(&i) = wanted.get(&group_key(cols.iter().map(|&c| &r[c]))) {
                out.push((k, r, i));
            }
        }
    }
    Ok(out)
}

enum Work {
    Rows(Schema, Vec<Change>),
    /// Referenced key values of `parent` that changed or disappeared:
    /// (old values, new values or None when deleted).
    Refs { parent: Schema, child: Schema, fk: ForeignKey, removed: Vec<(Vec<Value>, Option<Vec<Value>>)> },
}

/// Apply a batch of row changes to one table, with every constraint and cascade.
pub(super) fn apply_all(ctx: &mut Ctx, s: Schema, changes: Vec<Change>) -> Result<()> {
    let mut queue = VecDeque::from([Work::Rows(s, changes)]);
    let mut steps = 0;
    while let Some(w) = queue.pop_front() {
        steps += 1;
        if steps > MAX_CASCADE_STEPS {
            return Err(Error::Constraint(format!("foreign-key cascade did not finish within {MAX_CASCADE_STEPS} steps")));
        }
        match w {
            Work::Rows(s, changes) => apply_one(ctx, &s, changes, &mut queue)?,
            Work::Refs { parent, child, fk, removed } => {
                let changes = referencing_changes(ctx, &parent, &child, &fk, removed)?;
                if !changes.is_empty() {
                    queue.push_back(Work::Rows(child, changes));
                }
            }
        }
    }
    Ok(())
}

/// Child rows affected by keys that left `parent`, turned into changes per the FK's action.
fn referencing_changes(
    ctx: &Ctx,
    parent: &Schema,
    child: &Schema,
    fk: &ForeignKey,
    removed: Vec<(Vec<Value>, Option<Vec<Value>>)>,
) -> Result<Vec<Change>> {
    let ref_pos = parent.positions(&fk.ref_cols)?;
    let child_pos = child.positions(&fk.cols)?;
    // A key another parent row provides again is not gone.
    let mut gone = Vec::new();
    for (old, new) in removed {
        if !exists_eq(ctx, parent, &ref_pos, &old)? {
            gone.push((old, new));
        }
    }
    if gone.is_empty() {
        return Ok(Vec::new());
    }
    let olds: Vec<Vec<Value>> = gone.iter().map(|(o, _)| o.clone()).collect();
    let mut changes = Vec::new();
    for (key, row, i) in find_matching(ctx, child, &child_pos, &olds)? {
        let (old, new) = &gone[i];
        let action = if new.is_some() { fk.on_update } else { fk.on_delete };
        let mut updated = row.clone();
        match (action, new) {
            (FkAction::Restrict, _) => {
                return Err(Error::Constraint(format!(
                    "{} on table '{}' violates foreign key '{}' on table '{}': key ({}) = ({}) is still referenced",
                    if new.is_some() { "update" } else { "delete" },
                    parent.name,
                    fk.name,
                    child.name,
                    fk.ref_cols.join(", "),
                    show(old)
                )));
            }
            (FkAction::Cascade, None) => {
                changes.push(Change { old: Some((key, row)), new: None });
                continue;
            }
            (FkAction::Cascade, Some(new)) => {
                for (j, &p) in child_pos.iter().enumerate() {
                    updated[p] = new[j].clone();
                }
            }
            (FkAction::SetNull, _) => {
                for &p in &child_pos {
                    updated[p] = Value::Null;
                }
            }
        }
        changes.push(Change { old: Some((key, row)), new: Some(updated) });
    }
    Ok(changes)
}

fn apply_one(ctx: &mut Ctx, s: &Schema, mut changes: Vec<Change>, queue: &mut VecDeque<Work>) -> Result<()> {
    let layout = Layout::new(s);
    // 1. remove old rows
    for c in &changes {
        if let Some((k, r)) = &c.old {
            layout.delete(ctx, k, r);
        }
    }
    // 2. validate and write new rows
    let mut new_rows: Vec<Option<Vec<Value>>> = Vec::with_capacity(changes.len());
    for c in changes.iter_mut() {
        let Some(new) = c.new.take() else {
            new_rows.push(None);
            continue;
        };
        let row = {
            let q = QCtx::new(ctx);
            validate_row(&q, s, new)?
        };
        let key = match &c.old {
            Some((k, _)) if s.pk.is_empty() => k.clone(), // keep the hidden row id
            _ => new_row_key(ctx, s, &row)?,
        };
        if ctx.get(&key).is_some() {
            let vals: Vec<Value> = s.pk.iter().map(|&i| row[i].clone()).collect();
            let shown = if vals.len() == 1 {
                vals[0].to_json()
            } else {
                format!("({}) = ({})", s.pk_names().join(", "), show(&vals))
            };
            return Err(Error::Constraint(format!("duplicate primary key {shown} in table '{}'", s.name)));
        }
        check_unique(ctx, s, row_id(s, &key), &row)?;
        layout.put(ctx, key, &row);
        new_rows.push(Some(row));
    }
    // 3. new foreign-key values must point at existing rows
    for fk in &s.fks {
        let pos = s.positions(&fk.cols)?;
        let parent = if fk.ref_table == s.name { s.clone() } else { Schema::load(ctx, &fk.ref_table)? };
        let ref_pos = parent.positions(&fk.ref_cols)?;
        let mut checked = HashSet::new();
        for (c, new) in changes.iter().zip(&new_rows) {
            let Some(new) = new else { continue };
            let vals: Vec<Value> = pos.iter().map(|&p| new[p].clone()).collect();
            if vals.iter().any(Value::is_null) {
                continue; // MATCH SIMPLE: a NULL part means no reference
            }
            if let Some((_, old)) = &c.old {
                if same(&vals, &pos.iter().map(|&p| old[p].clone()).collect::<Vec<_>>()) {
                    continue;
                }
            }
            if checked.insert(group_key(vals.iter())) && !exists_eq(ctx, &parent, &ref_pos, &vals)? {
                return Err(Error::Constraint(format!(
                    "foreign key '{}' on table '{}': no row in '{}' has ({}) = ({})",
                    fk.name,
                    s.name,
                    fk.ref_table,
                    fk.ref_cols.join(", "),
                    show(&vals)
                )));
            }
        }
    }
    // 4. referenced keys that changed or disappeared
    if changes.iter().all(|c| c.old.is_none()) {
        return Ok(());
    }
    for (child, fk) in referencing(ctx, &s.name)? {
        let ref_pos = s.positions(&fk.ref_cols)?;
        let mut seen = HashSet::new();
        let mut removed = Vec::new();
        for (c, new) in changes.iter().zip(&new_rows) {
            let Some((_, old)) = &c.old else { continue };
            let ov: Vec<Value> = ref_pos.iter().map(|&p| old[p].clone()).collect();
            if ov.iter().any(Value::is_null) {
                continue;
            }
            let nv: Option<Vec<Value>> = new.as_ref().map(|n| ref_pos.iter().map(|&p| n[p].clone()).collect());
            if nv.as_ref().map_or(false, |nv| same(nv, &ov)) {
                continue;
            }
            if seen.insert(group_key(ov.iter())) {
                removed.push((ov, nv));
            }
        }
        if !removed.is_empty() {
            let child = if child.name == s.name { s.clone() } else { child };
            queue.push_back(Work::Refs { parent: s.clone(), child, fk, removed });
        }
    }
    Ok(())
}

/// Advance a sequence so it never hands out `v` or anything below it.
fn bump_seq(ctx: &mut Ctx, name: &str, v: i64) -> Result<()> {
    let k = keys::seq(name);
    let cur = match ctx.get(&k) {
        Some(raw) => String::from_utf8_lossy(&raw)
            .parse::<i64>()
            .map_err(|_| Error::Corrupt(format!("sequence '{name}' is not a number")))?,
        None => 0,
    };
    if v > cur {
        ctx.put(k, v.to_string().into_bytes());
    }
    Ok(())
}

/// Fill in defaults and auto-increment values. `None` means "not given / DEFAULT".
fn fill_defaults(ctx: &mut Ctx, s: &Schema, given: Vec<Option<Value>>) -> Result<Vec<Value>> {
    let mut row = Vec::with_capacity(s.cols.len());
    for (col, v) in s.cols.iter().zip(given) {
        let mut v = match v {
            Some(v) => v,
            None => col.default.clone().unwrap_or(Value::Null),
        };
        if col.auto {
            match &v {
                Value::Null => v = Value::Int(keys::next_seq(ctx, &auto_seq(&s.name, &col.name))?),
                Value::Int(i) => bump_seq(ctx, &auto_seq(&s.name, &col.name), *i)?,
                _ => {}
            }
        }
        row.push(v);
    }
    Ok(row)
}

pub(super) fn insert(ctx: &mut Ctx, table: &str, cols: Option<Vec<String>>, source: InsertSource) -> Result<Output> {
    let s = Schema::load(ctx, table)?;
    let targets: Vec<usize> = match (&cols, &source) {
        (_, InsertSource::DefaultValues) => Vec::new(),
        (Some(names), _) => {
            let t = s.positions(names)?;
            let mut seen = HashSet::new();
            if let Some(dup) = names.iter().find(|n| !seen.insert(n.as_str())) {
                return Err(Error::Schema(format!("column '{dup}' is listed twice")));
            }
            t
        }
        (None, _) => (0..s.cols.len()).collect(),
    };
    // Evaluate every value against the database as it was before the statement.
    let provided: Vec<Vec<Option<Value>>> = {
        let q = QCtx::new(ctx);
        let scope = Scope::new();
        let env = Env::new(&q, &scope, &[], None, 0);
        match source {
            InsertSource::DefaultValues => vec![Vec::new()],
            InsertSource::Values(rows) => {
                let mut out = Vec::with_capacity(rows.len());
                for exprs in rows {
                    if exprs.len() != targets.len() {
                        return Err(Error::Parse(format!(
                            "expected {} value(s) per row, got {}",
                            targets.len(),
                            exprs.len()
                        )));
                    }
                    out.push(exprs.iter().map(|e| e.as_ref().map(|e| eval(e, &env)).transpose()).collect::<Result<Vec<_>>>()?);
                }
                out
            }
            InsertSource::Query(query) => {
                let (cols_, rows) = run_query(&q, &query, None, 0)?;
                if cols_.len() != targets.len() {
                    return Err(Error::Parse(format!(
                        "INSERT ... SELECT: the query returns {} column(s), {} expected",
                        cols_.len(),
                        targets.len()
                    )));
                }
                rows.into_iter().map(|r| r.into_iter().map(Some).collect()).collect()
            }
        }
    };
    let mut changes = Vec::with_capacity(provided.len());
    for vals in provided {
        let mut given: Vec<Option<Value>> = vec![None; s.cols.len()];
        for (v, &ci) in vals.into_iter().zip(&targets) {
            given[ci] = v;
        }
        changes.push(Change { old: None, new: Some(fill_defaults(ctx, &s, given)?) });
    }
    let n = changes.len();
    apply_all(ctx, s, changes)?;
    Ok(Output::Done(format!("{n} row(s) inserted")))
}

pub(super) fn update(ctx: &mut Ctx, table: &str, sets: Vec<(String, Option<Expr>)>, filter: Option<Expr>) -> Result<Output> {
    let s = Schema::load(ctx, table)?;
    let mut seen = HashSet::new();
    let mut targets = Vec::with_capacity(sets.len());
    for (n, e) in sets {
        if !seen.insert(n.clone()) {
            return Err(Error::Schema(format!("column '{n}' is assigned twice")));
        }
        targets.push((s.col_index(&n)?, e));
    }
    if let Some(f) = &filter {
        if has_agg(f) {
            return Err(Error::Parse("aggregate functions are not allowed in WHERE".into()));
        }
    }
    let scope = s.scope(table);
    let mut changes = Vec::new();
    let mut n = 0u64;
    {
        let q = QCtx::new(ctx);
        for (key, row) in fetch(&q, &s, table, filter.as_ref())? {
            let env = Env::new(&q, &scope, &row, None, 0);
            if let Some(f) = &filter {
                if !is_true(&eval(f, &env)?)? {
                    continue;
                }
            }
            // every SET expression sees the row as it was before the statement
            let mut new = row.clone();
            for (ci, e) in &targets {
                new[*ci] = match e {
                    Some(e) => eval(e, &env)?,
                    None => s.cols[*ci].default.clone().unwrap_or(Value::Null),
                };
            }
            n += 1;
            let unchanged = new.iter().zip(&row).all(|(a, b)| a.total_cmp(b) == Ordering::Equal && a.rank() == b.rank());
            if !unchanged {
                changes.push(Change { old: Some((key, row)), new: Some(new) });
            }
        }
    }
    apply_all(ctx, s, changes)?;
    Ok(Output::Done(format!("{n} row(s) updated")))
}

pub(super) fn delete(ctx: &mut Ctx, table: &str, filter: Option<Expr>) -> Result<Output> {
    let s = Schema::load(ctx, table)?;
    if let Some(f) = &filter {
        if has_agg(f) {
            return Err(Error::Parse("aggregate functions are not allowed in WHERE".into()));
        }
    }
    let scope = s.scope(table);
    let mut changes = Vec::new();
    {
        let q = QCtx::new(ctx);
        for (key, row) in fetch(&q, &s, table, filter.as_ref())? {
            if let Some(f) = &filter {
                if !is_true(&eval(f, &Env::new(&q, &scope, &row, None, 0))?)? {
                    continue;
                }
            }
            changes.push(Change { old: Some((key, row)), new: None });
        }
    }
    let n = changes.len();
    apply_all(ctx, s, changes)?;
    Ok(Output::Done(format!("{n} row(s) deleted")))
}

pub(super) fn truncate(ctx: &mut Ctx, table: &str) -> Result<Output> {
    let s = Schema::load(ctx, table)?;
    for (child, fk) in referencing(ctx, table)? {
        if child.name != table && !ctx.scan(&keys::row_prefix(&child.name)).is_empty() {
            return Err(Error::Constraint(format!(
                "cannot truncate '{table}': foreign key '{}' on table '{}' references it and that table has rows",
                fk.name, child.name
            )));
        }
    }
    for prefix in [keys::row_prefix(&s.name), keys::index_owner_prefix(&owner(&s.name))] {
        for (k, _) in ctx.scan(&prefix) {
            ctx.delete(k);
        }
    }
    super::stats::forget(ctx, &s.name);
    Ok(Output::Done(format!("table '{table}' truncated")))
}
