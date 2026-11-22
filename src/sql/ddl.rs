//! Schema changes (CREATE / DROP / ALTER TABLE, CREATE / DROP INDEX) and
//! maintenance statements (DESCRIBE, CHECK TABLE, REINDEX).
//!
//! Every change that adds a constraint first verifies the rows already in
//! the table, and every change runs inside the statement's write set, so a
//! failed ALTER leaves both the schema and the data as they were.

use super::ast::*;
use super::catalog::*;
use super::dml::{coerce, exists_eq, validate_row};
use super::eval::*;
use super::parser::{parse_expr, to_sql};
use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use crate::Output;
use std::collections::{BTreeSet, HashSet};

fn valid_col_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('\0') || name.contains(FIELD_SEP) {
        return Err(Error::Schema(format!("invalid column name {name:?}")));
    }
    Ok(())
}

fn no_dups(names: &[String], what: &str) -> Result<()> {
    let mut seen = HashSet::new();
    match names.iter().find(|n| !seen.insert(n.as_str())) {
        Some(d) => Err(Error::Schema(format!("column '{d}' appears twice in {what}"))),
        None => Ok(()),
    }
}

/// Evaluate a DEFAULT expression to a constant valid for the column.
fn default_value(ctx: &Ctx, col: &Column, e: &Expr, in_pk: bool) -> Result<Option<Value>> {
    if !is_const(e) {
        return Err(Error::Schema(format!("DEFAULT of column '{}' must be a constant expression", col.name)));
    }
    let q = QCtx::new(ctx);
    let v = const_eval(e, &q)?;
    if v.is_null() {
        return Ok(None);
    }
    if col.auto {
        return Err(Error::Schema(format!("auto-increment column '{}' cannot have a DEFAULT", col.name)));
    }
    Ok(Some(coerce(col, v, in_pk)?))
}

/// Normalise a CHECK expression for storage: columns must belong to the
/// table, qualifiers are dropped, and it must survive a print/parse round trip.
fn check_expr(s: &Schema, mut e: Expr) -> Result<Expr> {
    if has_subquery(&e) || has_agg(&e) {
        return Err(Error::Schema("a CHECK constraint cannot contain subqueries or aggregates".into()));
    }
    let mut cols = Vec::new();
    columns_of(&e, &mut cols);
    for (q, n) in cols {
        if let Some(q) = q {
            if q != &s.name {
                return Err(Error::Schema(format!("CHECK constraint refers to another table '{q}'")));
            }
        }
        s.col_index(n)?;
    }
    rewrite_columns(&mut e, &|n| n.to_string());
    let text = to_sql(&e);
    parse_expr(&text).map_err(|_| Error::Schema("CHECK expression is too complex to store".into()))?;
    Ok(e)
}

/// Resolve and validate a foreign key of table `s` (which may still be under construction).
fn resolve_fk(ctx: &Ctx, s: &Schema, fk: FkDef) -> Result<ForeignKey> {
    no_dups(&fk.cols, "a FOREIGN KEY")?;
    let child_pos = s.positions(&fk.cols)?;
    let loaded;
    let parent = if fk.ref_table == s.name {
        s
    } else {
        loaded = Schema::load(ctx, &fk.ref_table)?;
        &loaded
    };
    let ref_cols = match fk.ref_cols {
        Some(c) => c,
        None if !parent.pk.is_empty() => parent.pk_names(),
        None => {
            return Err(Error::Schema(format!(
                "table '{}' has no primary key; name the referenced columns",
                parent.name
            )))
        }
    };
    if ref_cols.len() != fk.cols.len() {
        return Err(Error::Schema(format!(
            "foreign key has {} column(s) but references {}",
            fk.cols.len(),
            ref_cols.len()
        )));
    }
    let ref_pos = parent.positions(&ref_cols)?;
    if !parent.is_unique_set(&ref_cols) {
        return Err(Error::Schema(format!(
            "referenced columns {}({}) must be the primary key or have a unique constraint",
            parent.name,
            ref_cols.join(", ")
        )));
    }
    for (c, r) in child_pos.iter().zip(&ref_pos) {
        let (cc, rc) = (&s.cols[*c], &parent.cols[*r]);
        if cc.ty != rc.ty {
            return Err(Error::Schema(format!(
                "foreign key column '{}' is {} but referenced column '{}' is {}",
                cc.name,
                cc.ty.name().to_uppercase(),
                rc.name,
                rc.ty.name().to_uppercase()
            )));
        }
        if (fk.on_delete == FkAction::SetNull || fk.on_update == FkAction::SetNull) && (cc.not_null || s.pk.contains(c)) {
            return Err(Error::Schema(format!("ON ... SET NULL is impossible: column '{}' is NOT NULL", cc.name)));
        }
    }
    let name = match fk.name {
        Some(n) => n,
        None => s.fresh_name(&format!("{}_{}_fkey", s.name, fk.cols.join("_"))),
    };
    if s.names_in_use().contains(&name.as_str()) {
        return Err(Error::Schema(format!("a constraint or index named '{name}' already exists on '{}'", s.name)));
    }
    Ok(ForeignKey {
        name,
        cols: fk.cols,
        ref_table: fk.ref_table,
        ref_cols,
        on_delete: fk.on_delete,
        on_update: fk.on_update,
    })
}

fn unique_index(s: &Schema, name: Option<String>, cols: Vec<String>) -> Result<IndexDef> {
    no_dups(&cols, "a UNIQUE constraint")?;
    s.positions(&cols)?;
    let name = match name {
        Some(n) => n,
        None => s.fresh_name(&format!("{}_{}_key", s.name, cols.join("_"))),
    };
    if s.names_in_use().contains(&name.as_str()) {
        return Err(Error::Schema(format!("a constraint or index named '{name}' already exists on '{}'", s.name)));
    }
    Ok(IndexDef { name, cols, unique: true })
}

fn check_name(s: &Schema, name: Option<String>, col: Option<&str>) -> Result<String> {
    let name = match name {
        Some(n) => n,
        None => s.fresh_name(&match col {
            Some(c) => format!("{}_{c}_check", s.name),
            None => format!("{}_check", s.name),
        }),
    };
    if s.names_in_use().contains(&name.as_str()) {
        return Err(Error::Schema(format!("a constraint or index named '{name}' already exists on '{}'", s.name)));
    }
    Ok(name)
}

pub(super) fn create_table(ctx: &mut Ctx, def: TableDef) -> Result<Output> {
    let name = def.name;
    if !keys::valid_name(&name) {
        return Err(Error::Schema(format!("invalid table name '{name}'")));
    }
    if Schema::exists(ctx, &name) {
        return if def.if_not_exists {
            Ok(Output::Done(format!("table '{name}' already exists")))
        } else {
            Err(Error::Schema(format!("table '{name}' already exists")))
        };
    }
    let mut seen = HashSet::new();
    for c in &def.cols {
        valid_col_name(&c.col.name)?;
        if !seen.insert(c.col.name.as_str()) {
            return Err(Error::Schema(format!("duplicate column '{}'", c.col.name)));
        }
    }
    let mut s = Schema {
        name: name.clone(),
        cols: def.cols.iter().map(|c| c.col.clone()).collect(),
        pk: Vec::new(),
        indexes: Vec::new(),
        checks: Vec::new(),
        fks: Vec::new(),
    };
    // primary key first: it decides NOT NULL and the row-key layout
    let pks: Vec<&Vec<String>> =
        def.constraints.iter().filter_map(|c| if let Constraint::PrimaryKey(p) = c { Some(p) } else { None }).collect();
    if pks.len() > 1 {
        return Err(Error::Schema("a table can have only one primary key; use PRIMARY KEY (a, b) for a composite key".into()));
    }
    if let Some(cols) = pks.first() {
        no_dups(cols, "the PRIMARY KEY")?;
        s.pk = s.positions(cols).map_err(|_| {
            Error::Schema(format!("PRIMARY KEY names unknown column(s) {}", cols.join(", ")))
        })?;
        for &i in &s.pk {
            if !matches!(s.cols[i].ty, Ty::Int | Ty::Text) {
                return Err(Error::Schema("a primary key must be INT or TEXT".into()));
            }
        }
    }
    for (i, c) in def.cols.iter().enumerate() {
        if c.col.auto && c.col.ty != Ty::Int {
            return Err(Error::Schema(format!("auto-increment column '{}' must be INT", c.col.name)));
        }
        if let Some(e) = &c.default {
            s.cols[i].default = default_value(ctx, &s.cols[i], e, s.pk.contains(&i))?;
        }
    }
    let mut fks = Vec::new();
    for c in def.constraints {
        match c {
            Constraint::PrimaryKey(_) => {}
            Constraint::Unique(n, cols) => {
                let ix = unique_index(&s, n, cols)?;
                s.indexes.push(ix);
            }
            Constraint::Check(n, e) => {
                let col = single_column(&e);
                let name = check_name(&s, n, col.as_deref())?;
                let expr = check_expr(&s, e)?;
                s.checks.push(Check { name, expr });
            }
            Constraint::Foreign(fk) => fks.push(fk),
        }
    }
    // foreign keys last, so a self-reference can use this table's unique constraints
    for fk in fks {
        let f = resolve_fk(ctx, &s, fk)?;
        s.fks.push(f);
    }
    s.save(ctx);
    Ok(Output::Done(format!("table '{name}' created")))
}

fn single_column(e: &Expr) -> Option<String> {
    let mut cols = Vec::new();
    columns_of(e, &mut cols);
    let names: BTreeSet<&str> = cols.iter().map(|(_, n)| *n).collect();
    (names.len() == 1).then(|| names.into_iter().next().unwrap().to_string())
}

fn delete_sequences(ctx: &mut Ctx, table: &str) {
    ctx.delete(keys::seq(&rowid_seq(table)));
    for (k, _) in ctx.scan(&keys::seq(&format!("t:{table}."))) {
        ctx.delete(k);
    }
}

pub(super) fn drop_table(ctx: &mut Ctx, name: &str, if_exists: bool, cascade: bool) -> Result<Output> {
    if !Schema::exists(ctx, name) {
        return if if_exists {
            Ok(Output::Done(format!("table '{name}' does not exist, nothing dropped")))
        } else {
            Err(Error::Schema(format!("no such table '{name}'")))
        };
    }
    for (mut child, fk) in referencing(ctx, name)? {
        if child.name == name {
            continue;
        }
        if !cascade {
            return Err(Error::Constraint(format!(
                "cannot drop table '{name}': foreign key '{}' on table '{}' references it (use DROP TABLE {name} CASCADE to drop the constraint too)",
                fk.name, child.name
            )));
        }
        child.fks.retain(|f| f.ref_table != name);
        child.save(ctx);
    }
    for prefix in [keys::row_prefix(name), keys::index_owner_prefix(&owner(name))] {
        for (k, _) in ctx.scan(&prefix) {
            ctx.delete(k);
        }
    }
    ctx.delete(keys::table_meta(name));
    super::stats::forget(ctx, name);
    delete_sequences(ctx, name);
    Ok(Output::Done(format!("table '{name}' dropped")))
}

pub(super) fn create_index(
    ctx: &mut Ctx,
    name: &str,
    table: &str,
    cols: Vec<String>,
    unique: bool,
    if_not_exists: bool,
) -> Result<Output> {
    let mut s = Schema::load(ctx, table)?;
    no_dups(&cols, "the index")?;
    s.positions(&cols)?;
    if name.contains('\0') {
        return Err(Error::Schema("index names cannot contain NUL".into()));
    }
    if s.names_in_use().contains(&name) {
        return if if_not_exists {
            Ok(Output::Done(format!("index '{name}' already exists")))
        } else {
            Err(Error::Schema(format!("index '{name}' already exists on table '{table}'")))
        };
    }
    let ix = IndexDef { name: name.to_string(), cols, unique };
    build_index(ctx, &s, &ix)?;
    let msg = format!("index '{name}' created on {table}({})", ix.cols.join(", "));
    s.indexes.push(ix);
    s.save(ctx);
    Ok(Output::Done(msg))
}

/// Remove index `pos` from `s` (not saved), refusing if a foreign key needs it.
fn remove_index(ctx: &mut Ctx, s: &mut Schema, pos: usize) -> Result<()> {
    let ix = s.indexes.remove(pos);
    if ix.unique {
        for (child, fk) in referencing(ctx, &s.name)? {
            if !s.is_unique_set(&fk.ref_cols) {
                return Err(Error::Constraint(format!(
                    "cannot drop '{}': foreign key '{}' on table '{}' depends on it",
                    ix.name, fk.name, child.name
                )));
            }
        }
    }
    if !s.indexes.iter().any(|i| i.cols == ix.cols) {
        clear_index_field(ctx, &s.name, &ix.cols);
    }
    Ok(())
}

pub(super) fn drop_index(ctx: &mut Ctx, name: &str, table: Option<&str>, if_exists: bool) -> Result<Output> {
    let candidates: Vec<String> = match table {
        Some(t) => {
            Schema::load(ctx, t)?;
            vec![t.to_string()]
        }
        None => {
            let mut v = Vec::new();
            for t in tables(ctx) {
                if Schema::load(ctx, &t)?.indexes.iter().any(|i| i.name == name) {
                    v.push(t);
                }
            }
            v
        }
    };
    let mut found = Vec::new();
    for t in &candidates {
        let s = Schema::load(ctx, t)?;
        if let Some(p) = s.indexes.iter().position(|i| i.name == name) {
            found.push((s, p));
        }
    }
    match found.len() {
        0 => {
            if if_exists {
                Ok(Output::Done(format!("index '{name}' does not exist, nothing dropped")))
            } else if let Some(t) = table {
                Err(Error::Schema(format!("no index '{name}' on table '{t}'")))
            } else {
                Err(Error::Schema(format!("no such index '{name}'")))
            }
        }
        1 => {
            let (mut s, p) = found.pop().unwrap();
            remove_index(ctx, &mut s, p)?;
            s.save(ctx);
            Ok(Output::Done(format!("index '{name}' dropped")))
        }
        _ => Err(Error::Schema(format!("index name '{name}' is used on several tables; use DROP INDEX {name} ON <table>"))),
    }
}

/// Add a constraint to an existing table after verifying its rows (schema not saved).
fn add_constraint(ctx: &mut Ctx, s: &mut Schema, c: Constraint) -> Result<()> {
    match c {
        Constraint::PrimaryKey(_) => {
            Err(Error::Schema("adding a primary key to an existing table is not supported; create a new table".into()))
        }
        Constraint::Unique(n, cols) => {
            let ix = unique_index(s, n, cols)?;
            build_index(ctx, s, &ix)?;
            s.indexes.push(ix);
            Ok(())
        }
        Constraint::Check(n, e) => {
            let col = single_column(&e);
            let name = check_name(s, n, col.as_deref())?;
            let expr = check_expr(s, e)?;
            let scope = s.scope(&s.name);
            let q = QCtx::new(ctx);
            for (_, row) in all_rows(ctx, s)? {
                if tri(&eval(&expr, &Env::new(&q, &scope, &row, None, 0))?)? == Some(false) {
                    return Err(Error::Constraint(format!(
                        "cannot add check constraint '{name}': existing rows of '{}' violate it",
                        s.name
                    )));
                }
            }
            s.checks.push(Check { name, expr });
            Ok(())
        }
        Constraint::Foreign(fk) => {
            let f = resolve_fk(ctx, s, fk)?;
            let pos = s.positions(&f.cols)?;
            let parent = if f.ref_table == s.name { s.clone() } else { Schema::load(ctx, &f.ref_table)? };
            let ref_pos = parent.positions(&f.ref_cols)?;
            for (_, row) in all_rows(ctx, s)? {
                let vals: Vec<Value> = pos.iter().map(|&p| row[p].clone()).collect();
                if !vals.iter().any(Value::is_null) && !exists_eq(ctx, &parent, &ref_pos, &vals)? {
                    return Err(Error::Constraint(format!(
                        "cannot add foreign key '{}': no row in '{}' has ({}) = ({})",
                        f.name,
                        f.ref_table,
                        f.ref_cols.join(", "),
                        vals.iter().map(|v| v.to_json()).collect::<Vec<_>>().join(", ")
                    )));
                }
            }
            s.fks.push(f);
            Ok(())
        }
    }
}

fn rewrite_rows(ctx: &mut Ctx, s: &Schema, mut f: impl FnMut(&mut Ctx, Vec<Value>) -> Result<Vec<Value>>) -> Result<()> {
    for (k, row) in all_rows(ctx, s)? {
        let new = f(ctx, row)?;
        ctx.put(k, encode_row(&new));
    }
    Ok(())
}

pub(super) fn alter_table(ctx: &mut Ctx, table: &str, actions: Vec<Alter>) -> Result<Output> {
    let mut table = table.to_string();
    let original = table.clone();
    // column names, types and keys may change: the statistics no longer apply
    super::stats::forget(ctx, &table);
    for action in actions {
        let mut s = Schema::load(ctx, &table)?;
        match action {
            Alter::AddColumn(cdef, cons) => {
                let mut col = cdef.col;
                valid_col_name(&col.name)?;
                if s.cols.iter().any(|c| c.name == col.name) {
                    return Err(Error::Schema(format!("table '{table}' already has a column '{}'", col.name)));
                }
                if cons.iter().any(|c| matches!(c, Constraint::PrimaryKey(_))) {
                    return Err(Error::Schema("cannot add a PRIMARY KEY column to an existing table".into()));
                }
                if col.auto && col.ty != Ty::Int {
                    return Err(Error::Schema(format!("auto-increment column '{}' must be INT", col.name)));
                }
                if let Some(e) = &cdef.default {
                    col.default = default_value(ctx, &col, e, false)?;
                }
                let has_rows = !all_rows(ctx, &s)?.is_empty();
                if col.not_null && col.default.is_none() && !col.auto && has_rows {
                    return Err(Error::Constraint(format!(
                        "cannot add NOT NULL column '{}' without a DEFAULT: table '{table}' has rows",
                        col.name
                    )));
                }
                let seq = auto_seq(&table, &col.name);
                let (fill, auto) = (col.default.clone().unwrap_or(Value::Null), col.auto);
                rewrite_rows(ctx, &s, |ctx, mut row| {
                    row.push(if auto { Value::Int(keys::next_seq(ctx, &seq)?) } else { fill.clone() });
                    Ok(row)
                })?;
                s.cols.push(col);
                for c in cons {
                    add_constraint(ctx, &mut s, c)?;
                }
            }
            Alter::DropColumn { name, if_exists } => {
                let Some(ci) = s.cols.iter().position(|c| c.name == name) else {
                    if if_exists {
                        continue;
                    }
                    return Err(Error::Schema(format!("table '{table}' has no column '{name}'")));
                };
                if s.pk.contains(&ci) {
                    return Err(Error::Schema(format!("cannot drop column '{name}': it is part of the primary key")));
                }
                if s.cols.len() == 1 {
                    return Err(Error::Schema(format!("cannot drop '{name}', the only column of '{table}'")));
                }
                for (child, fk) in referencing(ctx, &table)? {
                    if fk.ref_cols.contains(&name) && !(child.name == table && fk.cols.contains(&name)) {
                        return Err(Error::Constraint(format!(
                            "cannot drop column '{name}': foreign key '{}' on table '{}' references it",
                            fk.name, child.name
                        )));
                    }
                }
                s.fks.retain(|f| !f.cols.contains(&name));
                while let Some(p) = s.indexes.iter().position(|i| i.cols.contains(&name)) {
                    remove_index(ctx, &mut s, p)?;
                }
                s.checks.retain(|c| {
                    let mut cols = Vec::new();
                    columns_of(&c.expr, &mut cols);
                    !cols.iter().any(|(_, n)| *n == name)
                });
                rewrite_rows(ctx, &s, |_, mut row| {
                    row.remove(ci);
                    Ok(row)
                })?;
                ctx.delete(keys::seq(&auto_seq(&table, &name)));
                s.cols.remove(ci);
                for p in s.pk.iter_mut() {
                    if *p > ci {
                        *p -= 1;
                    }
                }
            }
            Alter::RenameColumn { from, to } => {
                let ci = s.col_index(&from)?;
                valid_col_name(&to)?;
                if s.cols.iter().any(|c| c.name == to) {
                    return Err(Error::Schema(format!("table '{table}' already has a column '{to}'")));
                }
                let rn = |c: &mut String| {
                    if *c == from {
                        *c = to.clone();
                    }
                };
                for (mut child, _) in referencing(ctx, &table)? {
                    if child.name != table {
                        for f in child.fks.iter_mut().filter(|f| f.ref_table == table) {
                            f.ref_cols.iter_mut().for_each(rn);
                        }
                        child.save(ctx);
                    }
                }
                for f in s.fks.iter_mut() {
                    f.cols.iter_mut().for_each(rn);
                    if f.ref_table == table {
                        f.ref_cols.iter_mut().for_each(rn);
                    }
                }
                let affected: Vec<usize> = (0..s.indexes.len()).filter(|&i| s.indexes[i].cols.contains(&from)).collect();
                for &i in &affected {
                    clear_index_field(ctx, &table, &s.indexes[i].cols.clone());
                }
                for &i in &affected {
                    s.indexes[i].cols.iter_mut().for_each(rn);
                }
                for c in s.checks.iter_mut() {
                    rewrite_columns(&mut c.expr, &|n| if n == from { to.clone() } else { n.to_string() });
                }
                if let Some(v) = ctx.get(&keys::seq(&auto_seq(&table, &from))) {
                    ctx.delete(keys::seq(&auto_seq(&table, &from)));
                    ctx.put(keys::seq(&auto_seq(&table, &to)), v);
                }
                s.cols[ci].name = to.clone();
                for &i in &affected {
                    build_index(ctx, &s, &s.indexes[i].clone())?;
                }
            }
            Alter::RenameTable(new) => {
                if !keys::valid_name(&new) {
                    return Err(Error::Schema(format!("invalid table name '{new}'")));
                }
                if Schema::exists(ctx, &new) {
                    return Err(Error::Schema(format!("table '{new}' already exists")));
                }
                let (old_rows, new_rows) = (keys::row_prefix(&table), keys::row_prefix(&new));
                for (k, v) in ctx.scan(&old_rows) {
                    let mut nk = new_rows.clone();
                    nk.extend_from_slice(&k[old_rows.len()..]);
                    ctx.delete(k);
                    ctx.put(nk, v);
                }
                let (old_ix, new_ix) = (keys::index_owner_prefix(&owner(&table)), keys::index_owner_prefix(&owner(&new)));
                for (k, v) in ctx.scan(&old_ix) {
                    let mut nk = new_ix.clone();
                    nk.extend_from_slice(&k[old_ix.len()..]);
                    ctx.delete(k);
                    ctx.put(nk, v);
                }
                let mut seqs = vec![(rowid_seq(&table), rowid_seq(&new))];
                for c in &s.cols {
                    seqs.push((auto_seq(&table, &c.name), auto_seq(&new, &c.name)));
                }
                for (a, b) in seqs {
                    if let Some(v) = ctx.get(&keys::seq(&a)) {
                        ctx.delete(keys::seq(&a));
                        ctx.put(keys::seq(&b), v);
                    }
                }
                for (mut child, _) in referencing(ctx, &table)? {
                    if child.name != table {
                        for f in child.fks.iter_mut().filter(|f| f.ref_table == table) {
                            f.ref_table = new.clone();
                        }
                        child.save(ctx);
                    }
                }
                for f in s.fks.iter_mut().filter(|f| f.ref_table == table) {
                    f.ref_table = new.clone();
                }
                ctx.delete(keys::table_meta(&table));
                s.name = new.clone();
                table = new;
            }
            Alter::SetDefault(col, e) => {
                let ci = s.col_index(&col)?;
                s.cols[ci].default = match e {
                    Some(e) => default_value(ctx, &s.cols[ci], &e, s.pk.contains(&ci))?,
                    None => None,
                };
            }
            Alter::SetNotNull(col, on) => {
                let ci = s.col_index(&col)?;
                if on {
                    if all_rows(ctx, &s)?.iter().any(|(_, r)| r[ci].is_null()) {
                        return Err(Error::Constraint(format!("cannot set NOT NULL: column '{col}' contains NULLs")));
                    }
                } else if s.pk.contains(&ci) {
                    return Err(Error::Schema(format!("column '{col}' is part of the primary key and must stay NOT NULL")));
                }
                s.cols[ci].not_null = on;
            }
            Alter::SetType(col, ty, max_len) => {
                let ci = s.col_index(&col)?;
                if s.pk.contains(&ci) {
                    return Err(Error::Schema(format!("cannot change the type of primary-key column '{col}'")));
                }
                let in_fk = s.fks.iter().any(|f| f.cols.contains(&col))
                    || referencing(ctx, &table)?.iter().any(|(_, f)| f.ref_cols.contains(&col));
                if in_fk {
                    return Err(Error::Schema(format!("cannot change the type of column '{col}': a foreign key uses it")));
                }
                if s.cols[ci].auto && ty != Ty::Int {
                    return Err(Error::Schema(format!("auto-increment column '{col}' must stay INT")));
                }
                let mut newcol = s.cols[ci].clone();
                newcol.ty = ty;
                newcol.max_len = if ty == Ty::Text { max_len } else { None };
                newcol.default = match newcol.default.take() {
                    Some(d) => Some(coerce(&newcol, cast(d, ty)?, false)?),
                    None => None,
                };
                let affected: Vec<IndexDef> = s.indexes.iter().filter(|i| i.cols.contains(&col)).cloned().collect();
                for ix in &affected {
                    clear_index_field(ctx, &table, &ix.cols);
                }
                rewrite_rows(ctx, &s, |_, mut row| {
                    row[ci] = coerce(&newcol, cast(std::mem::replace(&mut row[ci], Value::Null), ty)?, false)?;
                    Ok(row)
                })?;
                s.cols[ci] = newcol;
                if !s.checks.is_empty() {
                    let q = QCtx::new(ctx);
                    for (_, row) in all_rows(ctx, &s)? {
                        validate_row(&q, &s, row)?;
                    }
                }
                for ix in &affected {
                    build_index(ctx, &s, ix)?;
                }
            }
            Alter::AddConstraint(c) => add_constraint(ctx, &mut s, c)?,
            Alter::DropConstraint { name, if_exists } => {
                if let Some(p) = s.indexes.iter().position(|i| i.name == name) {
                    remove_index(ctx, &mut s, p)?;
                } else if let Some(p) = s.checks.iter().position(|c| c.name == name) {
                    s.checks.remove(p);
                } else if let Some(p) = s.fks.iter().position(|f| f.name == name) {
                    s.fks.remove(p);
                } else if !if_exists {
                    return Err(Error::Schema(format!("table '{table}' has no constraint '{name}'")));
                }
            }
        }
        s.save(ctx);
    }
    Ok(Output::Done(if table == original {
        format!("table '{table}' altered")
    } else {
        format!("table '{original}' altered (now '{table}')")
    }))
}

pub(super) fn describe(ctx: &Ctx, table: &str) -> Result<Output> {
    let s = Schema::load(ctx, table)?;
    let text = |v: String| Value::Text(v);
    let mut rows = Vec::new();
    for (i, c) in s.cols.iter().enumerate() {
        let ty = match (c.ty, c.max_len) {
            (Ty::Text, Some(n)) => format!("varchar({n})"),
            (t, _) => t.name().to_string(),
        };
        let key = if s.pk.contains(&i) {
            "PRI"
        } else if s.indexes.iter().any(|ix| ix.unique && ix.cols == [c.name.clone()]) {
            "UNI"
        } else if s.indexes.iter().any(|ix| ix.cols.first() == Some(&c.name)) {
            "MUL"
        } else {
            ""
        };
        let mut extra = Vec::new();
        if c.auto {
            extra.push("auto_increment".to_string());
        }
        for f in s.fks.iter().filter(|f| f.cols.contains(&c.name)) {
            extra.push(format!("references {}({})", f.ref_table, f.ref_cols.join(", ")));
        }
        rows.push(vec![
            text(c.name.clone()),
            text(ty),
            text(if c.not_null || s.pk.contains(&i) { "NO" } else { "YES" }.into()),
            text(key.into()),
            c.default.clone().unwrap_or(Value::Null),
            text(extra.join("; ")),
        ]);
    }
    let columns = ["column", "type", "nullable", "key", "default", "extra"].iter().map(|s| s.to_string()).collect();
    Ok(Output::Rows { columns, rows })
}

/// Verify a table against its schema: row encoding and types, primary keys,
/// every index entry (missing and orphaned), uniqueness, CHECK and FOREIGN KEY.
fn check_one(ctx: &Ctx, s: &Schema) -> Result<Vec<Vec<Value>>> {
    let mut out = Vec::new();
    let mut report = |check: String, problems: Vec<String>, ok_detail: String| {
        let ok = problems.is_empty();
        out.push(vec![
            Value::Text(s.name.clone()),
            Value::Text(check),
            Value::Text(if ok { "ok" } else { "error" }.into()),
            Value::Text(if ok { ok_detail } else { problems.join("; ") }),
        ]);
    };
    let prefix = keys::row_prefix(&s.name);
    let mut rows = Vec::new();
    let mut problems = Vec::new();
    for (k, raw) in ctx.scan(&prefix) {
        match decode_row(&raw) {
            Ok(r) if r.len() == s.cols.len() => rows.push((k, r)),
            Ok(r) => problems.push(format!("a row has {} values, expected {}", r.len(), s.cols.len())),
            Err(_) => problems.push("a row is unreadable".into()),
        }
    }
    let mut bad_values = 0;
    let mut bad_keys = 0;
    for (k, r) in &rows {
        if r.iter().enumerate().any(|(i, v)| coerce(&s.cols[i], v.clone(), s.pk.contains(&i)).ok().as_ref() != Some(v)) {
            bad_values += 1;
        }
        if !s.pk.is_empty() && pk_key(s, r).ok().as_deref() != Some(k.as_slice()) {
            bad_keys += 1;
        }
    }
    if bad_values > 0 {
        problems.push(format!("{bad_values} row(s) have values that violate a column type or NOT NULL"));
    }
    if bad_keys > 0 {
        problems.push(format!("{bad_keys} row(s) are stored under a key that does not match their primary key"));
    }
    report("rows".into(), problems, format!("{} row(s)", rows.len()));

    for ix in &s.indexes {
        let mut problems = Vec::new();
        let Ok(pos) = s.positions(&ix.cols) else {
            report(format!("index {}", ix.name), vec!["names a missing column".into()], String::new());
            continue;
        };
        let expected: BTreeSet<Vec<u8>> =
            rows.iter().filter_map(|(k, r)| index_entry(s, ix, row_id(s, k), r)).collect();
        let actual: BTreeSet<Vec<u8>> = ctx.scan(&index_base(&s.name, &ix.cols)).into_iter().map(|(k, _)| k).collect();
        let missing = expected.difference(&actual).count();
        let orphan = actual.difference(&expected).count();
        if missing > 0 {
            problems.push(format!("{missing} missing entr{}", if missing == 1 { "y" } else { "ies" }));
        }
        if orphan > 0 {
            problems.push(format!("{orphan} orphaned entr{}", if orphan == 1 { "y" } else { "ies" }));
        }
        if ix.unique {
            let mut seen = HashSet::new();
            let dups = rows
                .iter()
                .filter(|(_, r)| pos.iter().all(|&p| !r[p].is_null()))
                .filter(|(_, r)| !seen.insert(group_key(pos.iter().map(|&p| &r[p]))))
                .count();
            if dups > 0 {
                problems.push(format!("{dups} duplicate value(s)"));
            }
        }
        report(format!("index {}", ix.name), problems, format!("{} entr{}", expected.len(), if expected.len() == 1 { "y" } else { "ies" }));
    }

    let q = QCtx::new(ctx);
    let scope = s.scope(&s.name);
    for c in &s.checks {
        let mut bad = 0;
        for (_, r) in &rows {
            if matches!(eval(&c.expr, &Env::new(&q, &scope, r, None, 0)).and_then(|v| tri(&v)), Ok(Some(false)) | Err(_)) {
                bad += 1;
            }
        }
        let problems = if bad > 0 { vec![format!("{bad} row(s) violate it")] } else { vec![] };
        report(format!("check {}", c.name), problems, "satisfied".into());
    }
    for f in &s.fks {
        let mut problems = Vec::new();
        match (s.positions(&f.cols), Schema::load(ctx, &f.ref_table)) {
            (Ok(pos), Ok(parent)) => match parent.positions(&f.ref_cols) {
                Ok(ref_pos) => {
                    let mut orphans = 0;
                    for (_, r) in &rows {
                        let vals: Vec<Value> = pos.iter().map(|&p| r[p].clone()).collect();
                        if !vals.iter().any(Value::is_null) && !exists_eq(ctx, &parent, &ref_pos, &vals)? {
                            orphans += 1;
                        }
                    }
                    if orphans > 0 {
                        problems.push(format!("{orphans} row(s) reference missing keys in '{}'", f.ref_table));
                    }
                }
                Err(_) => problems.push(format!("referenced columns are missing from '{}'", f.ref_table)),
            },
            (_, Err(_)) => problems.push(format!("referenced table '{}' is missing", f.ref_table)),
            (Err(_), _) => problems.push("names a missing column".into()),
        }
        report(format!("foreign key {}", f.name), problems, format!("references {}({})", f.ref_table, f.ref_cols.join(", ")));
    }
    // index entries under this table that belong to no declared index
    let declared: HashSet<Vec<u8>> = s.indexes.iter().map(|ix| index_base(&s.name, &ix.cols)).collect();
    let stray = ctx
        .scan(&keys::index_owner_prefix(&owner(&s.name)))
        .into_iter()
        .filter(|(k, _)| !declared.iter().any(|d| k.starts_with(d)))
        .count();
    if stray > 0 {
        report("index entries".into(), vec![format!("{stray} entr(ies) belong to no index")], String::new());
    }
    Ok(out)
}

pub(super) fn check_tables(ctx: &Ctx, table: Option<&str>) -> Result<Output> {
    let names = match table {
        Some(t) => vec![Schema::load(ctx, t)?.name],
        None => tables(ctx),
    };
    let mut rows = Vec::new();
    for t in names {
        rows.extend(check_one(ctx, &Schema::load(ctx, &t)?)?);
    }
    let columns = ["table", "check", "status", "detail"].iter().map(|s| s.to_string()).collect();
    Ok(Output::Rows { columns, rows })
}

/// Rebuild every index entry of one or all tables from the rows.
pub(super) fn reindex(ctx: &mut Ctx, table: Option<&str>) -> Result<Output> {
    let names = match table {
        Some(t) => vec![Schema::load(ctx, t)?.name],
        None => tables(ctx),
    };
    let mut n = 0;
    for t in &names {
        let s = Schema::load(ctx, t)?;
        for (k, _) in ctx.scan(&keys::index_owner_prefix(&owner(t))) {
            ctx.delete(k);
        }
        for ix in &s.indexes {
            build_index(ctx, &s, ix)?;
            n += 1;
        }
    }
    Ok(Output::Done(format!("{n} index(es) rebuilt on {} table(s)", names.len())))
}

/// Gather planner statistics for one or all tables.
pub(super) fn analyze(ctx: &mut Ctx, table: Option<&str>) -> Result<Output> {
    let names = match table {
        Some(t) => vec![Schema::load(ctx, t)?.name],
        None => tables(ctx),
    };
    let mut rows = Vec::new();
    for t in &names {
        let s = Schema::load(ctx, t)?;
        let st = super::stats::analyze_table(ctx, &s)?;
        rows.push(vec![Value::Text(t.clone()), Value::Int(st.rows as i64), Value::Int(s.cols.len() as i64)]);
        super::stats::save(ctx, t, &st);
    }
    let columns = ["table", "rows", "columns"].iter().map(|s| s.to_string()).collect();
    Ok(Output::Rows { columns, rows })
}

/// What `repair_indexes` found and did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RepairReport {
    pub tables_checked: usize,
    /// Tables whose index entries did not match their rows and were rebuilt.
    pub rebuilt: Vec<String>,
    /// The problems found, as `table: check: detail`.
    pub problems: Vec<String>,
}

/// Verify every index entry of every table against the rows and rebuild the
/// indexes of any table where they disagree (missing, orphaned or stray
/// entries). Used after recovery; a no-op on a consistent database.
pub fn repair_indexes(ctx: &mut Ctx) -> Result<RepairReport> {
    let mut report = RepairReport::default();
    for t in tables(ctx) {
        let s = Schema::load(ctx, &t)?;
        report.tables_checked += 1;
        let mut bad = false;
        for r in check_one(ctx, &s)? {
            let check = r[1].display();
            let detail = r[3].display();
            if r[2] != Value::Text("ok".into()) && check.starts_with("index") && !detail.contains("duplicate") {
                bad = true;
                report.problems.push(format!("{t}: {check}: {detail}"));
            }
        }
        if bad {
            reindex(ctx, Some(&t))?;
            report.rebuilt.push(t);
        }
    }
    Ok(report)
}
