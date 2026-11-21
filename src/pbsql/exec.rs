//! Execution of PBSQL statements: queries through the shared planner, DDL
//! (including ALTER TABLE and constraints), and DML on relational tables,
//! document collections and key-value namespaces.

use super::ast::*;
use super::parser;
use crate::doc;
use crate::error::{Error, Result};
use crate::keys;
use crate::kv;
use crate::query::eval::{coerce_type, eval, is_true, Env, NoSubs, SubRunner};
use crate::query::exec::{explain, run, run_analyze, Executor, Row};
use crate::query::plan::{
    self, constant, source_scope, split_conjuncts, valid_collection, Planner, Scope, SourceKind,
    SubPlan,
};
use crate::rel::{self, Column, DefaultVal, Enc, ForeignKey, Index, Schema};
use crate::storage::Ctx;
use crate::value::Value;
use crate::Output;
use std::collections::{BTreeMap, HashSet};

/// Parse and run one PBSQL (or SQL) statement.
pub fn execute(ctx: &mut Ctx, src: &str) -> Result<Output> {
    run_stmt(ctx, parser::parse(src)?)
}

/// Whether a statement only reads (so it can run under a shared lock).
pub fn is_read_only_stmt(st: &Stmt) -> bool {
    match st {
        Stmt::Query(_) | Stmt::KvGet { .. } | Stmt::KvExists { .. } => true,
        Stmt::Explain { analyze: false, .. } => true,
        Stmt::Explain {
            analyze: true,
            stmt,
        } => matches!(**stmt, Stmt::Query(_)),
        _ => false,
    }
}

/// Output columns (name, static type when certain) a statement would return,
/// planned but not executed (`None` for statements that return no rows or
/// cannot be planned yet).
pub fn describe(ctx: &Ctx, st: Stmt) -> Option<Vec<(String, Option<Ty>)>> {
    match st {
        Stmt::Query(q) => Planner::plan_query(ctx, q, vec![]).ok().map(|p| {
            if p.docs_output {
                vec![("document".to_string(), Some(Ty::Text))]
            } else {
                p.columns.into_iter().zip(p.types).collect()
            }
        }),
        Stmt::Explain { .. } => Some(vec![("QUERY PLAN".into(), Some(Ty::Text))]),
        Stmt::KvGet { .. } => Some(vec![("value".into(), Some(Ty::Text))]),
        Stmt::KvExists { .. } => Some(vec![("value".into(), Some(Ty::Bool))]),
        Stmt::KvIncr { .. } | Stmt::KvDel { .. } => Some(vec![("value".into(), Some(Ty::Int))]),
        _ => None,
    }
}

/// Whether a parsed statement only reads (so it can run under a shared lock).
pub fn is_read_only(src: &str) -> bool {
    match parser::parse(src) {
        Ok(Stmt::Query(_)) | Ok(Stmt::KvGet { .. }) | Ok(Stmt::KvExists { .. }) => true,
        Ok(Stmt::Explain { analyze: false, .. }) => true,
        Ok(Stmt::Explain {
            analyze: true,
            stmt,
        }) => matches!(*stmt, Stmt::Query(_)),
        _ => false,
    }
}

pub fn run_stmt(ctx: &mut Ctx, st: Stmt) -> Result<Output> {
    match st {
        Stmt::Query(q) => query(ctx, q),
        Stmt::Explain { analyze, stmt } => explain_stmt(ctx, *stmt, analyze),
        Stmt::CreateTable {
            name,
            cols,
            constraints,
            if_not_exists,
        } => create_table(ctx, name, cols, constraints, if_not_exists),
        Stmt::DropTable { name, if_exists } => drop_table(ctx, &name, if_exists),
        Stmt::CreateIndex {
            name,
            target,
            cols,
            unique,
            if_not_exists,
        } => create_index(ctx, name, target, cols, unique, if_not_exists),
        Stmt::DropIndex { name, target } => drop_index(ctx, &name, &target),
        Stmt::AlterTable { table, op } => alter(ctx, &table, op),
        Stmt::Insert {
            target,
            cols,
            source,
        } => insert(ctx, target, cols, source),
        Stmt::Update {
            target,
            alias,
            sets,
            filter,
        } => update(ctx, target, alias, sets, filter),
        Stmt::Delete {
            target,
            alias,
            filter,
        } => delete(ctx, target, alias, filter),
        Stmt::KvGet { ns, key } => {
            let k = kv_key(&constant(&key)?)?;
            Ok(Output::Scalar(match kv::live(ctx, &keys::kv_ns(&ns, &k)) {
                Some(v) => Value::Text(String::from_utf8_lossy(&v).into_owned()),
                None => Value::Null,
            }))
        }
        Stmt::KvExists { ns, key } => {
            let k = kv_key(&constant(&key)?)?;
            Ok(Output::Scalar(Value::Bool(
                kv::live(ctx, &keys::kv_ns(&ns, &k)).is_some(),
            )))
        }
        Stmt::KvSet {
            ns,
            key,
            value,
            ttl,
        } => {
            let k = kv_key(&constant(&key)?)?;
            let ttl_ms = match ttl {
                None => None,
                Some(e) => match constant(&e)?.as_f64() {
                    Some(secs) if secs > 0.0 => Some((secs * 1000.0) as i64),
                    _ => {
                        return Err(Error::Type(
                            "TTL must be a positive number of seconds".into(),
                        ))
                    }
                },
            };
            kv::put(
                ctx,
                keys::kv_ns(&ns, &k),
                kv_value(constant(&value)?).into_bytes(),
                ttl_ms,
            );
            Ok(Output::Done("OK".into()))
        }
        Stmt::KvDel { ns, key } => {
            let k = keys::kv_ns(&ns, &kv_key(&constant(&key)?)?);
            Ok(Output::Scalar(Value::Int(kv::remove(ctx, k) as i64)))
        }
        Stmt::KvIncr { ns, key, by } => {
            let k = kv_key(&constant(&key)?)?;
            let by = constant(&by)?
                .as_i64()
                .ok_or_else(|| Error::Type("INCREMENT ... BY needs an integer".into()))?;
            let next = kv::incr_full(ctx, keys::kv_ns(&ns, &k), &k, by)?;
            Ok(Output::Scalar(Value::Int(next)))
        }
    }
}

fn kv_key(v: &Value) -> Result<String> {
    match v {
        Value::Text(s) => Ok(s.clone()),
        Value::Int(i) => Ok(i.to_string()),
        other => Err(Error::Type(format!(
            "a key-value key must be text, got {}",
            other.to_json()
        ))),
    }
}

/// Values are stored as bytes: text as-is, anything else as JSON.
fn kv_value(v: Value) -> String {
    match v {
        Value::Text(s) => s,
        Value::Null => String::new(),
        other => other.to_json(),
    }
}

// ================================================================== queries

fn query(ctx: &Ctx, q: Query) -> Result<Output> {
    let plan = Planner::plan_query(ctx, q, vec![])?;
    let rows = run(ctx, &plan)?;
    Ok(rows_output(&plan, rows))
}

fn rows_output(plan: &plan::Plan, rows: Vec<Row>) -> Output {
    if plan.docs_output {
        return Output::Docs(rows.into_iter().map(|mut r| r.swap_remove(0)).collect());
    }
    Output::Rows {
        columns: plan.columns.clone(),
        rows,
    }
}

fn explain_stmt(ctx: &mut Ctx, st: Stmt, analyze: bool) -> Result<Output> {
    let lines = match st {
        Stmt::Query(q) => {
            let plan = Planner::plan_query(ctx, q, vec![])?;
            if analyze {
                let (rows, ex, nanos) = run_analyze(ctx, &plan)?;
                let mut lines = explain(&plan, Some(&ex));
                lines.push(format!("Rows returned: {}", rows.len()));
                lines.push(format!("Execution time: {:.3} ms", nanos as f64 / 1e6));
                lines
            } else {
                explain(&plan, None)
            }
        }
        Stmt::Update {
            ref target,
            ref alias,
            ref filter,
            ..
        }
        | Stmt::Delete {
            ref target,
            ref alias,
            ref filter,
        } => {
            let kind = if matches!(st, Stmt::Update { .. }) {
                "Update"
            } else {
                "Delete"
            };
            let src = resolve_source(ctx, target)?;
            let scope = source_scope(&src, alias);
            let (filters, _, _) =
                Planner::bind_single(ctx, &scope, split_conjuncts(filter.clone()))?;
            let access = Planner::access_for(ctx, &src, &filters)?;
            let node = plan::Node {
                id: 0,
                op: plan::PhysOp::Scan {
                    access,
                    alias: alias.clone(),
                    filter: plan_and(filters),
                    limit: None,
                    reverse: false,
                },
            };
            let mut lines = vec![format!("{kind} on {}", target_name(target))];
            crate::query::exec::render(&node, 0, None, &mut lines, 1);
            if analyze {
                let t = std::time::Instant::now();
                let out = run_stmt(ctx, st)?;
                lines.push(format!("Result: {out}"));
                lines.push(format!(
                    "Execution time: {:.3} ms",
                    t.elapsed().as_nanos() as f64 / 1e6
                ));
            }
            lines
        }
        other => {
            if analyze {
                return Err(Error::Parse(
                    "EXPLAIN ANALYZE supports SELECT, UPDATE and DELETE".into(),
                ));
            }
            vec![format!(
                "{} (no query plan: executes directly)",
                stmt_kind(&other)
            )]
        }
    };
    Ok(Output::Rows {
        columns: vec!["QUERY PLAN".into()],
        rows: lines.into_iter().map(|l| vec![Value::Text(l)]).collect(),
    })
}

fn plan_and(v: Vec<Expr>) -> Option<Expr> {
    let mut it = v.into_iter();
    let first = it.next()?;
    Some(it.fold(first, |a, b| Expr::Bin(Box::new(a), Op::And, Box::new(b))))
}

fn stmt_kind(s: &Stmt) -> &'static str {
    match s {
        Stmt::Insert { .. } => "Insert",
        Stmt::CreateTable { .. } => "CreateTable",
        Stmt::DropTable { .. } => "DropTable",
        Stmt::CreateIndex { .. } => "CreateIndex",
        Stmt::DropIndex { .. } => "DropIndex",
        Stmt::AlterTable { .. } => "AlterTable",
        Stmt::KvGet { .. } => "KVGet",
        Stmt::KvSet { .. } => "KVSet",
        Stmt::KvExists { .. } => "KVExists",
        Stmt::KvIncr { .. } => "KVIncrement",
        Stmt::KvDel { .. } => "KVDelete",
        _ => "Statement",
    }
}

fn target_name(o: &ObjectName) -> String {
    match o.model {
        Model::Relational => o.name.clone(),
        Model::Document => format!("DOCUMENT.{}", o.name),
        Model::Kv if o.name.is_empty() => "KV".into(),
        Model::Kv => format!("KV.{}", o.name),
    }
}

fn resolve_source(ctx: &Ctx, o: &ObjectName) -> Result<SourceKind> {
    Ok(match o.model {
        Model::Relational => SourceKind::Table(Schema::load(ctx, &o.name)?),
        Model::Document => {
            if !valid_collection(&o.name) {
                return Err(Error::Schema(format!(
                    "invalid collection name '{}'",
                    o.name
                )));
            }
            SourceKind::Collection(o.name.clone())
        }
        Model::Kv => SourceKind::Kv(o.name.clone()),
    })
}

// ====================================================================== DDL

fn default_of(col: &ColumnDef) -> Result<Option<DefaultVal>> {
    let Some(e) = &col.default else {
        return Ok(None);
    };
    if let Expr::Func(name, args) = e {
        if args.is_empty()
            && matches!(
                name.as_str(),
                "now" | "current_timestamp" | "current_date" | "current_time" | "localtimestamp"
            )
        {
            if !matches!(col.ty, Ty::Timestamp | Ty::Date | Ty::Time | Ty::Text) {
                return Err(Error::Type(format!(
                    "DEFAULT {name} needs a date/time column, '{}' is {}",
                    col.name,
                    col.ty.name()
                )));
            }
            return Ok(Some(DefaultVal::Now(name.clone())));
        }
    }
    let v = constant(e)
        .map_err(|_| Error::Schema(format!("DEFAULT for '{}' must be a constant", col.name)))?;
    let v = coerce_type(col.ty, v)
        .map_err(|e| Error::Type(format!("DEFAULT for column '{}': {e}", col.name)))?;
    if v.is_null() && (col.not_null || col.pk) {
        return Err(Error::Schema(format!(
            "column '{}' is NOT NULL but its DEFAULT is NULL",
            col.name
        )));
    }
    Ok(Some(DefaultVal::Lit(v)))
}

fn index_name_free(s: &Schema, name: &str) -> bool {
    !s.indexes.iter().any(|i| i.name == name)
}

fn fresh_index(s: &Schema, base: &str, cols: &[String], unique: bool) -> Index {
    let mut name = base.to_string();
    let mut n = 1;
    while !index_name_free(s, &name) {
        n += 1;
        name = format!("{base}_{n}");
    }
    Index {
        field: format!("{name}\u{1}"),
        name,
        cols: cols.to_vec(),
        unique,
        enc: Enc::Exact,
    }
}

fn validate_fk(ctx: &Ctx, s: &Schema, fk: &mut ForeignKey) -> Result<()> {
    let parent = if fk.table == s.name {
        s.clone()
    } else {
        Schema::load(ctx, &fk.table)?
    };
    if fk.ref_cols.is_empty() {
        if parent.pk.is_empty() {
            return Err(Error::Schema(format!(
                "table '{}' has no primary key to reference",
                parent.name
            )));
        }
        fk.ref_cols = parent.pk.clone();
    }
    if fk.ref_cols.len() != fk.cols.len() {
        return Err(Error::Schema(
            "a foreign key must name as many columns as it references".into(),
        ));
    }
    for (c, rc) in fk.cols.iter().zip(&fk.ref_cols) {
        let ci = s.col_index(c)?;
        let ri = parent.col_index(rc)?;
        let (a, b) = (s.cols[ci].ty, parent.cols[ri].ty);
        let numeric = |t: Ty| matches!(t, Ty::Int | Ty::Float);
        if a != b && !(numeric(a) && numeric(b)) {
            return Err(Error::Schema(format!(
                "foreign key column '{c}' ({}) does not match '{}.{rc}' ({})",
                a.name(),
                parent.name,
                b.name()
            )));
        }
    }
    let is_pk =
        parent.pk.len() == fk.ref_cols.len() && parent.pk.iter().all(|p| fk.ref_cols.contains(p));
    let is_unique = parent.indexes.iter().any(|i| {
        i.unique
            && i.cols.len() == fk.ref_cols.len()
            && i.cols.iter().all(|c| fk.ref_cols.contains(c))
    });
    if !is_pk && !is_unique {
        return Err(Error::Schema(format!(
            "foreign key must reference the primary key or a UNIQUE column set of '{}'",
            parent.name
        )));
    }
    Ok(())
}

fn create_table(
    ctx: &mut Ctx,
    name: String,
    defs: Vec<ColumnDef>,
    constraints: Vec<TableConstraint>,
    if_not_exists: bool,
) -> Result<Output> {
    if !keys::valid_name(&name) {
        return Err(Error::Schema(format!("invalid table name '{name}'")));
    }
    if ctx.get(&keys::table_meta(&name)).is_some() {
        return if if_not_exists {
            Ok(Output::Done(format!("table '{name}' already exists")))
        } else {
            Err(Error::Schema(format!("table '{name}' already exists")))
        };
    }
    let mut seen = HashSet::new();
    for c in &defs {
        if c.name.contains('\0') || c.name.contains('\u{1}') {
            return Err(Error::Schema(
                "column names cannot contain control characters".into(),
            ));
        }
        if !seen.insert(c.name.as_str()) {
            return Err(Error::Schema(format!("duplicate column '{}'", c.name)));
        }
    }
    let mut pk: Vec<String> = defs
        .iter()
        .filter(|c| c.pk)
        .map(|c| c.name.clone())
        .collect();
    if pk.len() > 1 {
        return Err(Error::Schema(
            "use PRIMARY KEY (a, b) for a composite primary key".into(),
        ));
    }
    let mut uniques: Vec<Vec<String>> = defs
        .iter()
        .filter(|c| c.unique)
        .map(|c| vec![c.name.clone()])
        .collect();
    let mut fk_defs: Vec<FkDef> = defs.iter().filter_map(|c| c.references.clone()).collect();
    for c in constraints {
        match c {
            TableConstraint::PrimaryKey(cols) => {
                if !pk.is_empty() {
                    return Err(Error::Schema(
                        "a table can have only one PRIMARY KEY".into(),
                    ));
                }
                pk = cols;
            }
            TableConstraint::Unique(cols) => uniques.push(cols),
            TableConstraint::ForeignKey(f) => fk_defs.push(f),
        }
    }
    let mut cols = Vec::new();
    for d in &defs {
        cols.push(Column {
            name: d.name.clone(),
            ty: d.ty,
            not_null: d.not_null || pk.contains(&d.name),
            default: default_of(d)?,
        });
    }
    let mut schema = Schema {
        name: name.clone(),
        cols,
        pk: Vec::new(),
        indexes: Vec::new(),
        fks: Vec::new(),
    };
    let mut pk_seen = HashSet::new();
    for p in &pk {
        let ci = schema
            .col_index(p)
            .map_err(|_| Error::Schema(format!("PRIMARY KEY names unknown column '{p}'")))?;
        if !pk_seen.insert(p.clone()) {
            return Err(Error::Schema(format!(
                "column '{p}' appears twice in the PRIMARY KEY"
            )));
        }
        let ty = schema.cols[ci].ty;
        let ok = if pk.len() == 1 {
            matches!(ty, Ty::Int | Ty::Text)
        } else {
            !matches!(ty, Ty::Json)
        };
        if !ok {
            return Err(Error::Schema(if pk.len() == 1 {
                "a primary key must be INT or TEXT".into()
            } else {
                "composite primary key columns cannot be JSON".to_string()
            }));
        }
    }
    schema.pk = pk;
    for u in uniques {
        for c in &u {
            schema.col_index(c)?;
        }
        let base = format!("{name}_{}_key", u.join("_"));
        let ix = fresh_index(&schema, &base, &u, true);
        schema.indexes.push(ix);
    }
    for f in fk_defs {
        let mut fk = ForeignKey {
            cols: f.cols,
            table: f.table,
            ref_cols: f.ref_cols,
            on_delete: f.on_delete,
        };
        if fk.on_delete == FkAction::SetNull {
            for c in &fk.cols {
                if schema.is_pk(c) || schema.cols[schema.col_index(c)?].not_null {
                    return Err(Error::Schema(format!(
                        "ON DELETE SET NULL needs nullable column '{c}'"
                    )));
                }
            }
        }
        validate_fk(ctx, &schema, &mut fk)?;
        // index the referencing columns so parent deletes find children without a scan
        let covered = schema
            .indexes
            .iter()
            .any(|i| i.cols.len() >= fk.cols.len() && i.cols[..fk.cols.len()] == fk.cols[..])
            || (schema.pk.len() >= fk.cols.len() && schema.pk[..fk.cols.len()] == fk.cols[..]);
        if !covered {
            let ix = fresh_index(
                &schema,
                &format!("{name}_{}_fkey", fk.cols.join("_")),
                &fk.cols,
                false,
            );
            schema.indexes.push(ix);
        }
        schema.fks.push(fk);
    }
    schema.save(ctx);
    Ok(Output::Done(format!("table '{name}' created")))
}

fn drop_table(ctx: &mut Ctx, name: &str, if_exists: bool) -> Result<Output> {
    if ctx.get(&keys::table_meta(name)).is_none() {
        return if if_exists {
            Ok(Output::Done(format!(
                "table '{name}' does not exist, nothing dropped"
            )))
        } else {
            Err(Error::Schema(format!("no such table '{name}'")))
        };
    }
    for (child, _) in rel::referencing(ctx, name)? {
        if child.name != name {
            return Err(Error::Constraint(format!(
                "cannot drop '{name}': table '{}' has a foreign key referencing it",
                child.name
            )));
        }
    }
    rel::drop_table_data(ctx, name);
    Ok(Output::Done(format!("table '{name}' dropped")))
}

fn create_index(
    ctx: &mut Ctx,
    name: String,
    target: ObjectName,
    cols: Vec<String>,
    unique: bool,
    if_not_exists: bool,
) -> Result<Output> {
    if target.model == Model::Document {
        let fields: Vec<(String, i64)> = cols.iter().map(|c| (c.clone(), 1)).collect();
        let created =
            doc::create_index_with(ctx, &target.name, fields, unique, Some(name.clone()))?;
        return Ok(Output::Done(if created {
            format!(
                "{}index '{name}' created on DOCUMENT.{}({})",
                if unique { "unique " } else { "" },
                target.name,
                cols.join(", ")
            )
        } else if if_not_exists {
            format!("index '{name}' already exists")
        } else {
            format!(
                "DOCUMENT.{}({}) is already indexed",
                target.name,
                cols.join(", ")
            )
        }));
    }
    if target.model == Model::Kv {
        return Err(Error::Schema(
            "key-value data is already ordered by key; it takes no secondary indexes".into(),
        ));
    }
    let table = target.name;
    let mut schema = Schema::load(ctx, &table)?;
    let cols: Vec<String> = cols
        .iter()
        .map(|c| {
            schema
                .find_col(c)
                .map(|i| schema.cols[i].name.clone())
                .ok_or_else(|| Error::Schema(format!("table '{table}' has no column '{c}'")))
        })
        .collect::<Result<_>>()?;
    if !index_name_free(&schema, &name) {
        return if if_not_exists {
            Ok(Output::Done(format!("index '{name}' already exists")))
        } else {
            Err(Error::Schema(format!(
                "index '{name}' already exists on table '{table}'"
            )))
        };
    }
    // A plain single-column index keeps the original entry layout, so it can
    // share entries with an existing index on the same column.
    // Exact encoding keeps every value distinct and ordered, so the index
    // can also serve ORDER BY. (Indexes from 0.2 keep their legacy layout.)
    let ix = Index {
        field: format!("{name}\u{1}"),
        name: name.clone(),
        cols: cols.clone(),
        unique,
        enc: Enc::Exact,
    };
    let shared = schema.indexes.iter().any(|i| i.field == ix.field);
    if !shared {
        rel::build_index(ctx, &schema, &ix)?;
    }
    schema.indexes.push(ix);
    schema.save(ctx);
    Ok(Output::Done(format!(
        "{}index '{name}' created on {table}({})",
        if unique { "unique " } else { "" },
        cols.join(", ")
    )))
}

fn drop_index(ctx: &mut Ctx, name: &str, target: &ObjectName) -> Result<Output> {
    if target.model == Model::Document {
        return if doc::drop_index(ctx, &target.name, name)? {
            Ok(Output::Done(format!("index '{name}' dropped")))
        } else {
            Err(Error::Schema(format!(
                "no index '{name}' on DOCUMENT.{}",
                target.name
            )))
        };
    }
    let table = target.name.as_str();
    let name = &name.to_lowercase();
    let mut schema = Schema::load(ctx, table)?;
    let Some(pos) = schema.indexes.iter().position(|i| i.name == *name) else {
        return Err(Error::Schema(format!(
            "no index '{name}' on table '{table}'"
        )));
    };
    let ix = schema.indexes.remove(pos);
    rel::drop_index_entries(ctx, &schema, &ix);
    schema.save(ctx);
    Ok(Output::Done(format!("index '{name}' dropped")))
}

fn alter(ctx: &mut Ctx, table: &str, op: AlterOp) -> Result<Output> {
    let mut s = Schema::load(ctx, table)?;
    match op {
        AlterOp::AddColumn(def) => {
            if s.find_col(&def.name).is_some() {
                return Err(Error::Schema(format!(
                    "table '{table}' already has a column '{}'",
                    def.name
                )));
            }
            if def.pk {
                return Err(Error::Schema(
                    "ALTER TABLE ADD COLUMN cannot add a primary key column".into(),
                ));
            }
            let col = Column {
                name: def.name.clone(),
                ty: def.ty,
                not_null: def.not_null,
                default: default_of(&def)?,
            };
            let fill = rel::default_value(&col);
            let rows = rel::scan_rows(ctx, &s)?;
            if col.not_null && fill.is_null() && !rows.is_empty() {
                return Err(Error::Constraint(format!(
                    "column '{}' is NOT NULL: give it a DEFAULT for the existing rows",
                    col.name
                )));
            }
            s.cols.push(col);
            let ci = s.cols.len() - 1;
            if def.unique {
                let ix = fresh_index(
                    &s,
                    &format!("{table}_{}_key", def.name),
                    std::slice::from_ref(&def.name),
                    true,
                );
                s.indexes.push(ix);
            }
            if let Some(f) = def.references {
                let mut fk = ForeignKey {
                    cols: f.cols,
                    table: f.table,
                    ref_cols: f.ref_cols,
                    on_delete: f.on_delete,
                };
                validate_fk(ctx, &s, &mut fk)?;
                s.fks.push(fk);
            }
            for (k, mut row) in rows {
                row.push(rel::coerce(&s, ci, fill.clone())?);
                ctx.put(k, rel::encode_row(&row));
            }
            for ix in s.indexes.iter().filter(|i| i.cols.contains(&def.name)) {
                rel::build_index(ctx, &s, ix)?;
            }
            // check new foreign key against existing rows
            if let Some(fk) = s.fks.last().filter(|f| f.cols == [def.name.clone()]) {
                if !fill.is_null() {
                    let parent = Schema::load(ctx, &fk.table)?;
                    if rel::rows_where(ctx, &parent, &fk.ref_cols, std::slice::from_ref(&fill))?
                        .is_empty()
                        && !rel::scan_rows(ctx, &s)?.is_empty()
                    {
                        return Err(Error::Constraint(format!(
                            "DEFAULT {} has no matching row in '{}'",
                            fill.to_json(),
                            fk.table
                        )));
                    }
                }
            }
            s.save(ctx);
            Ok(Output::Done(format!(
                "column '{}' added to '{table}'",
                def.name
            )))
        }
        AlterOp::DropColumn(name) => {
            let ci = s.col_index(&name)?;
            let name = s.cols[ci].name.clone();
            if s.is_pk(&name) {
                return Err(Error::Schema(format!(
                    "cannot drop primary key column '{name}'"
                )));
            }
            if s.cols.len() == 1 {
                return Err(Error::Schema(
                    "cannot drop the only column of a table".into(),
                ));
            }
            if s.fks.iter().any(|f| f.cols.contains(&name)) {
                return Err(Error::Schema(format!(
                    "cannot drop '{name}': it is part of a foreign key"
                )));
            }
            for (child, fk) in rel::referencing(ctx, table)? {
                if fk.ref_cols.contains(&name) {
                    return Err(Error::Schema(format!(
                        "cannot drop '{name}': table '{}' references it",
                        child.name
                    )));
                }
            }
            if let Some(ix) = s
                .indexes
                .iter()
                .find(|i| i.cols.len() > 1 && i.cols.contains(&name))
            {
                return Err(Error::Schema(format!(
                    "cannot drop '{name}': it is part of multi-column index '{}'",
                    ix.name
                )));
            }
            let doomed: Vec<Index> = s
                .indexes
                .iter()
                .filter(|i| i.cols == [name.clone()])
                .cloned()
                .collect();
            for ix in &doomed {
                rel::drop_index_entries(ctx, &s, ix);
                s.indexes.retain(|i| i.name != ix.name);
            }
            for (k, mut row) in rel::scan_rows(ctx, &s)? {
                if ci < row.len() {
                    row.remove(ci);
                }
                ctx.put(k, rel::encode_row(&row));
            }
            s.cols.remove(ci);
            s.save(ctx);
            Ok(Output::Done(format!(
                "column '{name}' dropped from '{table}'"
            )))
        }
        AlterOp::RenameColumn(from, to) => {
            let ci = s.col_index(&from)?;
            let from = s.cols[ci].name.clone();
            if s.find_col(&to).is_some() {
                return Err(Error::Schema(format!(
                    "table '{table}' already has a column '{to}'"
                )));
            }
            s.cols[ci].name = to.clone();
            let ren = |v: &mut Vec<String>| {
                v.iter_mut()
                    .filter(|c| **c == from)
                    .for_each(|c| *c = to.clone())
            };
            ren(&mut s.pk);
            for ix in s.indexes.iter_mut() {
                ren(&mut ix.cols); // entries are stored under the index's fixed field: no rebuild
            }
            for fk in s.fks.iter_mut() {
                ren(&mut fk.cols);
                if fk.table == table {
                    ren(&mut fk.ref_cols);
                }
            }
            s.save(ctx);
            for (mut child, _) in rel::referencing(ctx, table)? {
                if child.name == table {
                    continue;
                }
                for fk in child.fks.iter_mut().filter(|f| f.table == table) {
                    ren(&mut fk.ref_cols);
                }
                child.save(ctx);
            }
            Ok(Output::Done(format!("column '{from}' renamed to '{to}'")))
        }
        AlterOp::RenameTable(to) => {
            if !keys::valid_name(&to) {
                return Err(Error::Schema(format!("invalid table name '{to}'")));
            }
            if ctx.get(&keys::table_meta(&to)).is_some() {
                return Err(Error::Schema(format!("table '{to}' already exists")));
            }
            let old_prefix = keys::row_prefix(table);
            let new_prefix = keys::row_prefix(&to);
            for (k, v) in ctx.scan(&old_prefix) {
                let mut nk = new_prefix.clone();
                nk.extend_from_slice(&k[old_prefix.len()..]);
                ctx.delete(k);
                ctx.put(nk, v);
            }
            let old_ix = keys::index_owner_prefix(&rel::owner(table));
            let new_ix = keys::index_owner_prefix(&rel::owner(&to));
            for (k, v) in ctx.scan(&old_ix) {
                let mut nk = new_ix.clone();
                nk.extend_from_slice(&k[old_ix.len()..]);
                ctx.delete(k);
                ctx.put(nk, v);
            }
            let seq_old = keys::seq(&format!("t:{table}"));
            if let Some(v) = ctx.get(&seq_old) {
                ctx.delete(seq_old);
                ctx.put(keys::seq(&format!("t:{to}")), v);
            }
            ctx.delete(keys::table_meta(table));
            for fk in s.fks.iter_mut().filter(|f| f.table == table) {
                fk.table = to.clone();
            }
            s.name = to.clone();
            s.save(ctx);
            for (mut child, _) in rel::referencing(ctx, table)? {
                for fk in child.fks.iter_mut().filter(|f| f.table == table) {
                    fk.table = to.clone();
                }
                child.save(ctx);
            }
            Ok(Output::Done(format!("table '{table}' renamed to '{to}'")))
        }
    }
}

// ====================================================================== DML

struct Subs<'p, 'c, 'a> {
    ex: Executor<'p, 'c, 'a>,
}

impl SubRunner for Subs<'_, '_, '_> {
    fn run_sub(
        &self,
        id: usize,
        outer: &[Vec<Value>],
        limit: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        self.ex.run_sub(id, outer, limit)
    }
}

fn insert(
    ctx: &mut Ctx,
    target: ObjectName,
    cols: Option<Vec<String>>,
    source: InsertSource,
) -> Result<Output> {
    // untyped parameters take the type of the column they are inserted into
    let mut source = source;
    if let (Model::Relational, InsertSource::Values(rows)) = (target.model, &mut source) {
        if let Ok(schema) = Schema::load(ctx, &target.name) {
            let tys: Vec<Option<Ty>> = match &cols {
                Some(names) => names
                    .iter()
                    .map(|n| schema.find_col(n).map(|i| schema.cols[i].ty))
                    .collect(),
                None => schema.cols.iter().map(|c| Some(c.ty)).collect(),
            };
            for row in rows.iter_mut() {
                for (e, ty) in row.iter_mut().zip(&tys) {
                    if let (Expr::Untyped(t), Some(ty)) = (&*e, ty) {
                        *e = Expr::Lit(crate::query::eval::untyped_as(*ty, t));
                    }
                }
            }
        }
    }
    // 1. compute the rows to insert (reads only)
    // `None` marks a DEFAULT written in a VALUES list
    let rows: Vec<Vec<Option<Value>>> = match source {
        InsertSource::Values(rows) => {
            let flat: Vec<Expr> = rows
                .iter()
                .flatten()
                .filter(|e| !matches!(e, Expr::Default))
                .cloned()
                .collect();
            let (bound, subs, _) = Planner::bind_single(ctx, &Scope::new(), flat)?;
            let runner = Subs {
                ex: Executor::new(ctx, &subs),
            };
            let mut it = bound.into_iter();
            let mut out = Vec::with_capacity(rows.len());
            for r in &rows {
                let mut vals = Vec::with_capacity(r.len());
                for e in r {
                    vals.push(match e {
                        Expr::Default => None,
                        _ => Some(eval(
                            &it.next().expect("bound in order"),
                            &Env::new(&[], &[], &runner),
                        )?),
                    });
                }
                out.push(vals);
            }
            out
        }
        InsertSource::Query(q) => {
            let plan = Planner::plan_query(ctx, *q, vec![])?;
            run(ctx, &plan)?
                .into_iter()
                .map(|r| r.into_iter().map(Some).collect())
                .collect()
        }
    };
    let n = rows.len();
    match target.model {
        Model::Relational => {
            let schema = Schema::load(ctx, &target.name)?;
            let targets: Vec<usize> = match &cols {
                Some(names) => {
                    let t = names
                        .iter()
                        .map(|c| schema.col_index(c))
                        .collect::<Result<Vec<_>>>()?;
                    let mut seen = HashSet::new();
                    if !t.iter().all(|i| seen.insert(*i)) {
                        return Err(Error::Schema("a column is named twice in INSERT".into()));
                    }
                    t
                }
                None => (0..schema.cols.len()).collect(),
            };
            for vals in rows {
                if vals.len() != targets.len() {
                    return Err(Error::Parse(format!(
                        "expected {} value(s) per row, got {}",
                        targets.len(),
                        vals.len()
                    )));
                }
                let mut row: Vec<Option<Value>> = vec![None; schema.cols.len()];
                for (v, &ci) in vals.into_iter().zip(&targets) {
                    row[ci] = v;
                }
                let mut full = Vec::with_capacity(row.len());
                for (ci, v) in row.into_iter().enumerate() {
                    let v = v.unwrap_or_else(|| rel::default_value(&schema.cols[ci]));
                    full.push(rel::coerce(&schema, ci, v)?);
                }
                rel::insert_row(ctx, &schema, full)?;
            }
            Ok(Output::Done(format!("{n} row(s) inserted")))
        }
        Model::Document => {
            if !valid_collection(&target.name) {
                return Err(Error::Schema(format!(
                    "invalid collection name '{}'",
                    target.name
                )));
            }
            let mut ids = Vec::new();
            for vals in rows {
                let d = match (&cols, vals.as_slice()) {
                    (None, [Some(d @ Value::Object(_))]) => d.clone(),
                    (None, [Some(Value::Text(s))]) => match Value::parse(s) {
                        Ok(d @ Value::Object(_)) => d,
                        _ => return Err(Error::Type("a document must be a JSON object".into())),
                    },
                    (Some(names), vals) if names.len() == vals.len() => {
                        let mut o = BTreeMap::new();
                        for (k, v) in names.iter().zip(vals) {
                            if let Some(v) = v {
                                o.insert(k.clone(), v.clone());
                            }
                        }
                        Value::Object(o)
                    }
                    _ => {
                        return Err(Error::Type(
                            "INSERT INTO DOCUMENT needs one JSON object per row, or a column list"
                                .into(),
                        ))
                    }
                };
                ids.push(doc::insert(ctx, &target.name, d)?);
            }
            Ok(Output::Done(format!("{n} document(s) inserted")))
        }
        Model::Kv => {
            let (ki, vi) = match &cols {
                None => (0, 1),
                Some(names) => {
                    let pos = |w: &str| names.iter().position(|c| c.eq_ignore_ascii_case(w));
                    match (pos("key"), pos("value")) {
                        (Some(k), Some(v)) if names.len() == 2 => (k, v),
                        _ => {
                            return Err(Error::Schema(
                                "INSERT INTO KV takes the columns (key, value)".into(),
                            ))
                        }
                    }
                }
            };
            for vals in rows {
                if vals.len() != 2 {
                    return Err(Error::Parse(
                        "INSERT INTO KV needs (key, value) rows".into(),
                    ));
                }
                let k = kv_key(vals[ki].as_ref().unwrap_or(&Value::Null))?;
                kv::put(
                    ctx,
                    keys::kv_ns(&target.name, &k),
                    kv_value(vals[vi].clone().unwrap_or(Value::Null)).into_bytes(),
                    None,
                );
            }
            Ok(Output::Done(format!("{n} key(s) set")))
        }
    }
}

/// Find the target rows of an UPDATE / DELETE through the planner's access
/// path selection, evaluating the WHERE clause (subqueries included).
#[allow(clippy::type_complexity)]
fn targets(
    ctx: &Ctx,
    src: &SourceKind,
    alias: &str,
    filter: Option<Expr>,
    extra: Vec<Expr>,
) -> Result<(Vec<(Vec<u8>, Row)>, Vec<Vec<Value>>)> {
    let scope = source_scope(src, alias);
    let cs = split_conjuncts(filter);
    let nf = cs.len();
    let mut all = cs;
    all.extend(extra);
    let (bound, subs, _): (Vec<Expr>, Vec<SubPlan>, usize) =
        Planner::bind_single(ctx, &scope, all)?;
    let (filters, extra) = bound.split_at(nf);
    let access = Planner::access_for(ctx, src, filters)?;
    let ex = Executor::new(ctx, &subs);
    let pred = plan_and(filters.to_vec());
    let mut out = Vec::new();
    let (mut examined, mut fb) = (0, false);
    ex.scan(
        &access,
        pred.as_ref(),
        None,
        &[],
        &mut examined,
        &mut fb,
        &mut |k, r| {
            out.push((k, r));
            Ok(())
        },
    )?;
    let mut computed = Vec::with_capacity(out.len());
    for (_, r) in &out {
        computed.push(
            extra
                .iter()
                .map(|e| eval(e, &Env::new(r, &[], &ex)))
                .collect::<Result<Vec<_>>>()?,
        );
    }
    Ok((out, computed))
}

fn update(
    ctx: &mut Ctx,
    target: ObjectName,
    alias: String,
    sets: Vec<(Vec<String>, Expr)>,
    filter: Option<Expr>,
) -> Result<Output> {
    let src = resolve_source(ctx, &target)?;
    let exprs: Vec<Expr> = sets
        .iter()
        .map(|(path, e)| match (e, &src) {
            // SET col = <untyped parameter>: use the column's type
            (Expr::Untyped(t), SourceKind::Table(schema)) => {
                let col = path.last().and_then(|c| schema.find_col(c));
                match col {
                    Some(ci) => Expr::Lit(crate::query::eval::untyped_as(schema.cols[ci].ty, t)),
                    None => e.clone(),
                }
            }
            (Expr::Untyped(t), SourceKind::Kv(_)) => Expr::Lit(Value::Text(t.clone())),
            _ => e.clone(),
        })
        .collect();
    let (hits, new_vals) = targets(ctx, &src, &alias, filter, exprs)?;
    let n = hits.len();
    match src {
        SourceKind::Table(schema) => {
            let mut cols = Vec::new();
            for (path, _) in &sets {
                let path: Vec<&String> = if path.len() > 1 && path[0].eq_ignore_ascii_case(&alias) {
                    path[1..].iter().collect()
                } else {
                    path.iter().collect()
                };
                if path.len() != 1 {
                    return Err(Error::Schema(format!(
                        "cannot SET a path inside column '{}'",
                        path[0]
                    )));
                }
                cols.push(schema.col_index(path[0])?);
            }
            for ((key, row), vals) in hits.into_iter().zip(new_vals) {
                // the row may have been changed by an earlier cascade in this statement
                let Some(raw) = ctx.get(&key) else { continue };
                let cur = rel::decode_row(&raw)?;
                let mut new = cur.clone();
                for (ci, v) in cols.iter().zip(vals) {
                    new[*ci] = rel::coerce(&schema, *ci, v)?;
                }
                let _ = row;
                rel::update_row(ctx, &schema, key, &cur, new)?;
            }
            Ok(Output::Done(format!("{n} row(s) updated")))
        }
        SourceKind::Collection(coll) => {
            for ((key, row), vals) in hits.into_iter().zip(new_vals) {
                let old = row.into_iter().next().unwrap_or(Value::Null);
                let mut new = old.clone();
                for ((path, _), v) in sets.iter().zip(vals) {
                    let path: Vec<&str> = path.iter().map(String::as_str).collect();
                    let p = if path.len() > 1 && path[0].eq_ignore_ascii_case(&alias) {
                        &path[1..]
                    } else {
                        &path[..]
                    };
                    doc::set_path(&mut new, &p.join("."), v)?;
                }
                doc::replace(ctx, &coll, key, &old, new)?;
            }
            Ok(Output::Done(format!("{n} document(s) updated")))
        }
        SourceKind::Kv(ns) => {
            for ((key, row), vals) in hits.into_iter().zip(new_vals) {
                let mut k = kv_key(&row[0])?;
                let mut v = row[1].clone();
                for ((path, _), val) in sets.iter().zip(vals) {
                    match path.last().map(|s| s.to_lowercase()).as_deref() {
                        Some("value") => v = val,
                        Some("key") => k = kv_key(&val)?,
                        _ => {
                            return Err(Error::Schema(
                                "a key-value entry has only the columns key and value".into(),
                            ))
                        }
                    }
                }
                // an UPDATE keeps the entry's remaining time-to-live
                let left = kv::ttl_left(ctx, &key).filter(|ms| *ms >= 0);
                kv::remove(ctx, key);
                kv::put(ctx, keys::kv_ns(&ns, &k), kv_value(v).into_bytes(), left);
            }
            Ok(Output::Done(format!("{n} key(s) updated")))
        }
    }
}

fn delete(
    ctx: &mut Ctx,
    target: ObjectName,
    alias: String,
    filter: Option<Expr>,
) -> Result<Output> {
    let src = resolve_source(ctx, &target)?;
    let (hits, _) = targets(ctx, &src, &alias, filter, vec![])?;
    let mut n = 0u64;
    match src {
        SourceKind::Table(schema) => {
            for (key, _) in hits {
                // a cascade earlier in this statement may already have removed it
                let Some(raw) = ctx.get(&key) else { continue };
                let row = rel::decode_row(&raw)?;
                rel::delete_row(ctx, &schema, &key, &row)?;
                n += 1;
            }
            Ok(Output::Done(format!("{n} row(s) deleted")))
        }
        SourceKind::Collection(coll) => {
            for (key, row) in hits {
                doc::remove(ctx, &coll, key, &row[0])?;
                n += 1;
            }
            Ok(Output::Done(format!("{n} document(s) deleted")))
        }
        SourceKind::Kv(_) => {
            for (key, _) in hits {
                kv::remove(ctx, key);
                n += 1;
            }
            Ok(Output::Done(format!("{n} key(s) deleted")))
        }
    }
}

/// Evaluate a predicate with no subqueries against a row (used by callers outside the planner).
pub fn matches_row(e: &Expr, row: &[Value]) -> Result<bool> {
    is_true(&eval(e, &Env::new(row, &[], &NoSubs))?)
}
