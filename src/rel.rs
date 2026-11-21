//! Relational storage: table schemas (with constraints), row encoding,
//! index maintenance and constraint enforcement.
//!
//! Rows are JSON arrays keyed by their primary key; secondary indexes are
//! ordered entries in the shared keyspace (see [`crate::keys`]) so they
//! support equality, prefix and range scans.

use crate::error::{Error, Result};
use crate::keys;
use crate::pbsql::ast::{FkAction, Ty};
use crate::query::eval::{coerce_type, now_timestamp};
use crate::storage::Ctx;
use crate::value::Value;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Bound;

#[derive(Debug, Clone, PartialEq)]
pub enum DefaultVal {
    Lit(Value),
    /// A zero-argument time function evaluated at insert time.
    Now(String),
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: Ty,
    pub not_null: bool,
    pub default: Option<DefaultVal>,
}

/// How index keys encode values. `Legacy` is the original numeric-merging
/// encoding (kept for indexes created by earlier versions); `Exact` keeps
/// every value distinct, which UNIQUE needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Enc {
    Legacy,
    Exact,
}

#[derive(Debug, Clone)]
pub struct Index {
    pub name: String,
    pub cols: Vec<String>,
    pub unique: bool,
    /// Storage field the entries live under; fixed at creation so renaming a column needs no rebuild.
    pub field: String,
    pub enc: Enc,
}

#[derive(Debug, Clone)]
pub struct ForeignKey {
    pub cols: Vec<String>,
    pub table: String,
    pub ref_cols: Vec<String>,
    pub on_delete: FkAction,
}

#[derive(Debug, Clone)]
pub struct Schema {
    pub name: String,
    pub cols: Vec<Column>,
    /// Primary key columns; empty means rows get a hidden auto-increment id.
    pub pk: Vec<String>,
    pub indexes: Vec<Index>,
    pub fks: Vec<ForeignKey>,
    /// CHECK constraints as (name, PBSQL expression text).
    pub checks: Vec<(String, String)>,
    /// The CHECK expressions bound to this table's columns (built on load).
    pub compiled_checks: Vec<crate::pbsql::ast::Expr>,
}

/// Exact, order-preserving, self-delimiting encoding of one key component.
pub fn encode_exact(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Bool(b) => Some(vec![1, *b as u8]),
        Value::Int(i) => {
            let mut out = vec![2u8];
            out.extend_from_slice(&((*i as u64) ^ (1 << 63)).to_be_bytes());
            Some(out)
        }
        Value::Float(_) => {
            let mut out = keys::encode_index_value(v)?;
            out[0] = 3;
            Some(out)
        }
        Value::Text(_) => keys::encode_index_value(v),
        _ => None,
    }
}

pub fn encode_with(enc: Enc, v: &Value) -> Option<Vec<u8>> {
    match enc {
        Enc::Legacy => keys::encode_index_value(v),
        Enc::Exact => encode_exact(v),
    }
}

/// The key range holding every encoding with the same leading tag byte as `enc`.
pub fn tag_end(prefix: &[u8], enc: &[u8]) -> Vec<u8> {
    let mut k = prefix.to_vec();
    k.push(enc[0] + 1);
    k
}

pub fn owner(table: &str) -> String {
    format!("t:{table}")
}

impl Schema {
    pub fn col_index(&self, name: &str) -> Result<usize> {
        self.find_col(name)
            .ok_or_else(|| Error::Schema(format!("table '{}' has no column '{name}'", self.name)))
    }

    pub fn find_col(&self, name: &str) -> Option<usize> {
        self.cols.iter().position(|c| c.name == name).or_else(|| {
            let lower = name.to_lowercase();
            self.cols.iter().position(|c| c.name == lower)
        })
    }

    pub fn pk_positions(&self) -> Vec<usize> {
        self.pk.iter().filter_map(|c| self.find_col(c)).collect()
    }

    pub fn is_pk(&self, col: &str) -> bool {
        self.pk.iter().any(|c| c == col)
    }

    pub fn to_json(&self) -> String {
        let s = |x: &str| Value::Text(x.to_string());
        let list = |v: &[String]| Value::Array(v.iter().map(|c| s(c)).collect());
        let cols = self
            .cols
            .iter()
            .map(|c| {
                let mut o = BTreeMap::new();
                o.insert("name".to_string(), s(&c.name));
                o.insert("type".to_string(), s(c.ty.name()));
                o.insert("pk".to_string(), Value::Bool(self.is_pk(&c.name)));
                o.insert("not_null".to_string(), Value::Bool(c.not_null));
                match &c.default {
                    Some(DefaultVal::Lit(v)) => {
                        o.insert("default".to_string(), v.clone());
                    }
                    Some(DefaultVal::Now(f)) => {
                        o.insert("default_fn".to_string(), s(f));
                    }
                    None => {}
                }
                Value::Object(o)
            })
            .collect();
        let indexes = self
            .indexes
            .iter()
            .map(|i| {
                let mut o = BTreeMap::new();
                o.insert("name".to_string(), s(&i.name));
                o.insert("cols".to_string(), list(&i.cols));
                o.insert("unique".to_string(), Value::Bool(i.unique));
                o.insert("field".to_string(), s(&i.field));
                o.insert(
                    "enc".to_string(),
                    s(if i.enc == Enc::Exact {
                        "exact"
                    } else {
                        "legacy"
                    }),
                );
                Value::Object(o)
            })
            .collect();
        let fks = self
            .fks
            .iter()
            .map(|f| {
                let mut o = BTreeMap::new();
                o.insert("cols".to_string(), list(&f.cols));
                o.insert("table".to_string(), s(&f.table));
                o.insert("ref_cols".to_string(), list(&f.ref_cols));
                o.insert("on_delete".to_string(), s(f.on_delete.name()));
                Value::Object(o)
            })
            .collect();
        let mut o = BTreeMap::new();
        o.insert("cols".to_string(), Value::Array(cols));
        o.insert("pk".to_string(), list(&self.pk));
        o.insert("indexes".to_string(), Value::Array(indexes));
        o.insert("fks".to_string(), Value::Array(fks));
        if !self.checks.is_empty() {
            o.insert(
                "checks".to_string(),
                Value::Array(
                    self.checks
                        .iter()
                        .map(|(n, e)| Value::Array(vec![s(n), s(e)]))
                        .collect(),
                ),
            );
        }
        Value::Object(o).to_json()
    }

    pub fn load(ctx: &Ctx, name: &str) -> Result<Schema> {
        Self::try_load(ctx, name)?.ok_or_else(|| Error::Schema(format!("no such table '{name}'")))
    }

    pub fn try_load(ctx: &Ctx, name: &str) -> Result<Option<Schema>> {
        let Some(raw) = ctx.get(&keys::table_meta(name)) else {
            return Ok(None);
        };
        let bad = || Error::Corrupt(format!("schema of table '{name}' is unreadable"));
        let v = Value::parse(std::str::from_utf8(&raw).map_err(|_| bad())?).map_err(|_| bad())?;
        let strs = |v: Option<&Value>| -> Vec<String> {
            match v {
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
                _ => vec![],
            }
        };
        let Some(Value::Array(cols)) = v.get("cols") else {
            return Err(bad());
        };
        let mut out = Vec::new();
        let mut pk_flags = Vec::new();
        for c in cols {
            let cname = c
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(bad)?
                .to_string();
            if c.get("pk") == Some(&Value::Bool(true)) {
                pk_flags.push(cname.clone());
            }
            let default = match (c.get("default"), c.get("default_fn")) {
                (Some(v), _) => Some(DefaultVal::Lit(v.clone())),
                (None, Some(Value::Text(f))) => Some(DefaultVal::Now(f.clone())),
                _ => None,
            };
            out.push(Column {
                name: cname,
                ty: c
                    .get("type")
                    .and_then(|v| v.as_str())
                    .and_then(Ty::from_name)
                    .ok_or_else(bad)?,
                not_null: c.get("not_null") == Some(&Value::Bool(true)),
                default,
            });
        }
        let pk = if v.get("pk").is_some() {
            strs(v.get("pk"))
        } else {
            pk_flags
        };
        let mut indexes = Vec::new();
        if let Some(Value::Array(list)) = v.get("indexes") {
            for ix in list {
                match ix {
                    // format of earlier versions: [name, column]
                    Value::Array(p) => {
                        if let (Some(Value::Text(n)), Some(Value::Text(c))) = (p.first(), p.get(1))
                        {
                            indexes.push(Index {
                                name: n.clone(),
                                cols: vec![c.clone()],
                                unique: false,
                                field: c.clone(),
                                enc: Enc::Legacy,
                            });
                        }
                    }
                    Value::Object(_) => indexes.push(Index {
                        name: ix
                            .get("name")
                            .and_then(Value::as_str)
                            .ok_or_else(bad)?
                            .to_string(),
                        cols: strs(ix.get("cols")),
                        unique: ix.get("unique") == Some(&Value::Bool(true)),
                        field: ix
                            .get("field")
                            .and_then(Value::as_str)
                            .ok_or_else(bad)?
                            .to_string(),
                        enc: if ix.get("enc").and_then(Value::as_str) == Some("exact") {
                            Enc::Exact
                        } else {
                            Enc::Legacy
                        },
                    }),
                    _ => return Err(bad()),
                }
            }
        }
        let mut fks = Vec::new();
        if let Some(Value::Array(list)) = v.get("fks") {
            for f in list {
                fks.push(ForeignKey {
                    cols: strs(f.get("cols")),
                    table: f
                        .get("table")
                        .and_then(Value::as_str)
                        .ok_or_else(bad)?
                        .to_string(),
                    ref_cols: strs(f.get("ref_cols")),
                    on_delete: match f.get("on_delete").and_then(Value::as_str) {
                        Some("cascade") => FkAction::Cascade,
                        Some("set null") => FkAction::SetNull,
                        _ => FkAction::Restrict,
                    },
                });
            }
        }
        let mut checks = Vec::new();
        if let Some(Value::Array(list)) = v.get("checks") {
            for c in list {
                if let Value::Array(p) = c {
                    if let (Some(Value::Text(n)), Some(Value::Text(e))) = (p.first(), p.get(1)) {
                        checks.push((n.clone(), e.clone()));
                    }
                }
            }
        }
        let mut schema = Schema {
            name: name.to_string(),
            cols: out,
            pk,
            indexes,
            fks,
            checks,
            compiled_checks: vec![],
        };
        schema.compile_checks(ctx)?;
        Ok(Some(schema))
    }

    /// Parse and bind the CHECK expressions against this table's columns.
    pub fn compile_checks(&mut self, ctx: &Ctx) -> Result<()> {
        use crate::query::plan::{source_scope, Planner, SourceKind};
        let scope = source_scope(
            &SourceKind::Table(Schema {
                compiled_checks: vec![],
                ..self.clone()
            }),
            &self.name,
        );
        let mut out = Vec::with_capacity(self.checks.len());
        for (name, sql) in &self.checks {
            let e = crate::pbsql::parser::parse_expr(sql)?;
            let (mut bound, subs, _) = Planner::bind_single(ctx, &scope, vec![e])?;
            if !subs.is_empty() {
                return Err(Error::Schema(format!(
                    "CHECK constraint '{name}' cannot contain a subquery"
                )));
            }
            out.push(bound.remove(0));
        }
        self.compiled_checks = out;
        Ok(())
    }

    /// Enforce the CHECK constraints on a row (NULL results pass, as in SQL).
    pub fn check_row(&self, row: &[Value]) -> Result<()> {
        use crate::query::eval::{eval, Env, NoSubs};
        for ((name, _), e) in self.checks.iter().zip(&self.compiled_checks) {
            match eval(e, &Env::new(row, &[], &NoSubs))? {
                Value::Bool(false) => {
                    return Err(Error::Constraint(format!(
                        "row violates CHECK constraint '{name}' of table '{}'",
                        self.name
                    )))
                }
                Value::Bool(true) | Value::Null => {}
                other => {
                    return Err(Error::Type(format!(
                        "CHECK constraint '{name}' must be boolean, got {}",
                        other.to_json()
                    )))
                }
            }
        }
        Ok(())
    }

    pub fn save(&self, ctx: &mut Ctx) {
        ctx.put(keys::table_meta(&self.name), self.to_json().into_bytes());
    }
}

pub fn tables(ctx: &Ctx) -> Vec<String> {
    ctx.scan(keys::TABLE_META_PREFIX)
        .into_iter()
        .map(|(k, _)| String::from_utf8_lossy(&k[keys::TABLE_META_PREFIX.len()..]).into_owned())
        .collect()
}

pub fn all_schemas(ctx: &Ctx) -> Result<Vec<Schema>> {
    tables(ctx).iter().map(|t| Schema::load(ctx, t)).collect()
}

pub fn decode_row(raw: &[u8]) -> Result<Vec<Value>> {
    let parsed = std::str::from_utf8(raw)
        .ok()
        .and_then(|s| Value::parse(s).ok());
    match parsed {
        Some(Value::Array(a)) => Ok(a),
        _ => Err(Error::Corrupt("stored row is unreadable".into())),
    }
}

pub fn encode_row(row: &[Value]) -> Vec<u8> {
    Value::Array(row.to_vec()).to_json().into_bytes()
}

/// Coerce a value into a column, enforcing NOT NULL (and PK NOT NULL).
pub fn coerce(s: &Schema, ci: usize, v: Value) -> Result<Value> {
    let col = &s.cols[ci];
    if v.is_null() {
        if col.not_null || s.is_pk(&col.name) {
            return Err(Error::Constraint(format!(
                "column '{}' cannot be NULL",
                col.name
            )));
        }
        return Ok(Value::Null);
    }
    coerce_type(col.ty, v).map_err(|e| match e {
        Error::Type(m) => Error::Type(format!("column '{}': {m}", col.name)),
        other => other,
    })
}

pub fn default_value(col: &Column) -> Value {
    match &col.default {
        None => Value::Null,
        Some(DefaultVal::Lit(v)) => v.clone(),
        Some(DefaultVal::Now(f)) => {
            let ts = now_timestamp();
            Value::Text(match (f.as_str(), col.ty) {
                ("current_date", _) | (_, Ty::Date) => ts[..10].to_string(),
                ("current_time", _) | (_, Ty::Time) => ts[11..].to_string(),
                _ => ts,
            })
        }
    }
}

/// The key bytes after the row prefix for a primary key value list.
pub fn encode_pk(vals: &[&Value]) -> Result<Vec<u8>> {
    if vals.len() == 1 {
        return keys::encode_id(vals[0]);
    }
    let mut out = Vec::new();
    for v in vals {
        out.extend(
            encode_exact(v).ok_or_else(|| {
                Error::Type(format!("cannot use {} in a primary key", v.to_json()))
            })?,
        );
    }
    Ok(out)
}

/// Storage key of a row: its primary key, or a fresh hidden row id.
pub fn row_key(ctx: &mut Ctx, s: &Schema, row: &[Value]) -> Result<Vec<u8>> {
    let mut k = keys::row_prefix(&s.name);
    let pos = s.pk_positions();
    if pos.is_empty() {
        let id = keys::next_seq(ctx, &format!("t:{}", s.name))?;
        k.extend(keys::encode_id(&Value::Int(id))?);
    } else {
        k.extend(encode_pk(
            &pos.iter().map(|i| &row[*i]).collect::<Vec<_>>(),
        )?);
    }
    Ok(k)
}

pub fn pk_key(s: &Schema, vals: &[&Value]) -> Result<Vec<u8>> {
    let mut k = keys::row_prefix(&s.name);
    k.extend(encode_pk(vals)?);
    Ok(k)
}

/// Entry prefix for an index and a list of leading values (None if a value is not indexable).
pub fn index_value_prefix(s: &Schema, ix: &Index, vals: &[Value]) -> Option<Vec<u8>> {
    let mut k = keys::index_prefix(&owner(&s.name), &ix.field);
    for v in vals {
        k.extend(encode_with(ix.enc, v)?);
    }
    Some(k)
}

fn index_entry(s: &Schema, ix: &Index, id: &[u8], row: &[Value]) -> Option<Vec<u8>> {
    let vals: Vec<Value> = ix
        .cols
        .iter()
        .map(|c| s.find_col(c).map(|i| row[i].clone()))
        .collect::<Option<_>>()?;
    let mut k = index_value_prefix(s, ix, &vals)?;
    k.extend_from_slice(id);
    Some(k)
}

/// Secondary-index entries pointing at one row. Rows with a NULL (or
/// otherwise unindexable) value in an indexed column get no entry.
pub fn index_entries(s: &Schema, key: &[u8], row: &[Value]) -> Vec<Vec<u8>> {
    let id = &key[keys::row_prefix(&s.name).len()..];
    let mut out: Vec<Vec<u8>> = s
        .indexes
        .iter()
        .filter_map(|ix| index_entry(s, ix, id, row))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Row keys found through an index entry prefix.
pub fn ids_from_index(
    ctx: &Ctx,
    s: &Schema,
    lo: Bound<&[u8]>,
    hi: Bound<&[u8]>,
    field_prefix_len: usize,
    ix: &Index,
) -> Vec<Vec<u8>> {
    let _ = ix;
    let rp = keys::row_prefix(&s.name);
    ctx.range(lo, hi)
        .into_iter()
        .map(|(k, _)| {
            let mut rk = rp.clone();
            rk.extend_from_slice(id_suffix(&k[field_prefix_len..], ix));
            rk
        })
        .collect()
}

/// Skip the encoded index values at the front of an entry, leaving the row id.
fn id_suffix<'a>(mut rest: &'a [u8], ix: &Index) -> &'a [u8] {
    for _ in 0..ix.cols.len() {
        let n = encoded_len(rest);
        rest = &rest[n.min(rest.len())..];
    }
    rest
}

/// Length of one self-delimiting encoded value.
pub fn encoded_len(b: &[u8]) -> usize {
    match b.first() {
        Some(0) => 1,
        Some(1) => 2,
        Some(2) | Some(3) => 9,
        Some(4) | Some(5) => {
            let mut i = 1;
            while i < b.len() {
                if b[i] == 0 {
                    if b.get(i + 1) == Some(&0xff) {
                        i += 2;
                        continue;
                    }
                    return i + 2;
                }
                i += 1;
            }
            b.len()
        }
        _ => b.len(),
    }
}

fn pk_text(vals: Vec<String>) -> String {
    if vals.len() == 1 {
        vals.into_iter().next().unwrap_or_default()
    } else {
        format!("({})", vals.join(", "))
    }
}

fn get_row(ctx: &Ctx, key: &[u8]) -> Result<Option<Vec<Value>>> {
    match ctx.get(key) {
        Some(raw) => Ok(Some(decode_row(&raw)?)),
        None => Ok(None),
    }
}

fn values_of(s: &Schema, row: &[Value], cols: &[String]) -> Vec<Value> {
    cols.iter()
        .map(|c| s.find_col(c).map_or(Value::Null, |i| row[i].clone()))
        .collect()
}

/// Rows of `s` whose `cols` equal `vals`, using the primary key or an index when possible.
pub fn rows_where(
    ctx: &Ctx,
    s: &Schema,
    cols: &[String],
    vals: &[Value],
) -> Result<Vec<(Vec<u8>, Vec<Value>)>> {
    if !s.pk.is_empty() && s.pk.len() == cols.len() && s.pk.iter().all(|p| cols.contains(p)) {
        let ordered: Vec<&Value> =
            s.pk.iter()
                .map(|p| &vals[cols.iter().position(|c| c == p).unwrap()])
                .collect();
        let key = pk_key(s, &ordered)?;
        return Ok(get_row(ctx, &key)?
            .map(|r| vec![(key, r)])
            .unwrap_or_default());
    }
    // an index whose leading columns are exactly these
    for ix in &s.indexes {
        if ix.cols.len() < cols.len() || !cols.iter().all(|c| ix.cols[..cols.len()].contains(c)) {
            continue;
        }
        let ordered: Vec<Value> = ix.cols[..cols.len()]
            .iter()
            .map(|ic| vals[cols.iter().position(|c| c == ic).unwrap()].clone())
            .collect();
        let Some(prefix) = index_value_prefix(s, ix, &ordered) else {
            continue;
        };
        let field_len = keys::index_prefix(&owner(&s.name), &ix.field).len();
        let mut hi = prefix.clone();
        hi.push(0xff);
        let mut out = Vec::new();
        for rk in ids_from_index(
            ctx,
            s,
            Bound::Included(&prefix),
            Bound::Excluded(&hi),
            field_len,
            ix,
        ) {
            if let Some(r) = get_row(ctx, &rk)? {
                if values_of(s, &r, cols)
                    .iter()
                    .zip(vals)
                    .all(|(a, b)| a.total_cmp(b) == Ordering::Equal)
                {
                    out.push((rk, r));
                }
            }
        }
        return Ok(out);
    }
    let mut out = Vec::new();
    for (k, raw) in ctx.scan(&keys::row_prefix(&s.name)) {
        let r = decode_row(&raw)?;
        if values_of(s, &r, cols)
            .iter()
            .zip(vals)
            .all(|(a, b)| a.total_cmp(b) == Ordering::Equal)
        {
            out.push((k, r));
        }
    }
    Ok(out)
}

fn check_unique(ctx: &Ctx, s: &Schema, key: &[u8], row: &[Value]) -> Result<()> {
    for ix in s.indexes.iter().filter(|i| i.unique) {
        let vals = values_of(s, row, &ix.cols);
        if vals.iter().any(Value::is_null) {
            continue; // NULLs are never equal to each other
        }
        for (other, _) in rows_where(ctx, s, &ix.cols, &vals)? {
            if other != key {
                let shown: Vec<String> = vals.iter().map(Value::to_json).collect();
                return Err(Error::Constraint(format!(
                    "duplicate value ({}) for UNIQUE ({}) in table '{}'",
                    shown.join(", "),
                    ix.cols.join(", "),
                    s.name
                )));
            }
        }
    }
    Ok(())
}

fn check_parents(
    ctx: &Ctx,
    s: &Schema,
    row: &[Value],
    only_changed: Option<&[Value]>,
) -> Result<()> {
    for fk in &s.fks {
        let vals = values_of(s, row, &fk.cols);
        if vals.iter().any(Value::is_null) {
            continue;
        }
        if let Some(old) = only_changed {
            let before = values_of(s, old, &fk.cols);
            if before
                .iter()
                .zip(&vals)
                .all(|(a, b)| a.total_cmp(b) == Ordering::Equal)
            {
                continue;
            }
        }
        let parent = if fk.table == s.name {
            s.clone()
        } else {
            Schema::load(ctx, &fk.table)?
        };
        // coerce to the parent's column types so 1 and 1.0 find the same row
        let mut typed = Vec::new();
        for (v, rc) in vals.iter().zip(&fk.ref_cols) {
            let ci = parent.col_index(rc)?;
            typed.push(coerce_type(parent.cols[ci].ty, v.clone()).unwrap_or_else(|_| v.clone()));
        }
        if rows_where(ctx, &parent, &fk.ref_cols, &typed)?.is_empty() {
            let shown: Vec<String> = vals.iter().map(Value::to_json).collect();
            return Err(Error::Constraint(format!(
                "foreign key ({}) = ({}) has no matching row in '{}'",
                fk.cols.join(", "),
                shown.join(", "),
                fk.table
            )));
        }
    }
    Ok(())
}

fn write_entries(ctx: &mut Ctx, s: &Schema, key: Vec<u8>, row: &[Value]) {
    for k in index_entries(s, &key, row) {
        ctx.put(k, Vec::new());
    }
    ctx.put(key, encode_row(row));
}

fn remove_entries(ctx: &mut Ctx, s: &Schema, key: &[u8], row: &[Value]) {
    for k in index_entries(s, key, row) {
        ctx.delete(k);
    }
    ctx.delete(key.to_vec());
}

/// Insert a fully coerced row, enforcing PK, UNIQUE and FOREIGN KEY constraints.
pub fn insert_row(ctx: &mut Ctx, s: &Schema, row: Vec<Value>) -> Result<()> {
    s.check_row(&row)?;
    let key = row_key(ctx, s, &row)?;
    if ctx.get(&key).is_some() {
        let pk: Vec<String> = s.pk_positions().iter().map(|i| row[*i].to_json()).collect();
        return Err(Error::Constraint(format!(
            "duplicate primary key {} in table '{}'",
            pk_text(pk),
            s.name
        )));
    }
    check_unique(ctx, s, &key, &row)?;
    check_parents(ctx, s, &row, None)?;
    write_entries(ctx, s, key, &row);
    Ok(())
}

/// Replace a row, enforcing constraints on the new values and referential
/// integrity for rows of other tables that reference the old ones.
pub fn update_row(
    ctx: &mut Ctx,
    s: &Schema,
    key: Vec<u8>,
    old: &[Value],
    new: Vec<Value>,
) -> Result<()> {
    let pos = s.pk_positions();
    let new_key = if pos
        .iter()
        .any(|i| old[*i].total_cmp(&new[*i]) != Ordering::Equal)
    {
        let k = pk_key(s, &pos.iter().map(|i| &new[*i]).collect::<Vec<_>>())?;
        if ctx.get(&k).is_some() {
            let pk: Vec<String> = pos.iter().map(|i| new[*i].to_json()).collect();
            return Err(Error::Constraint(format!(
                "duplicate primary key {} in table '{}'",
                pk_text(pk),
                s.name
            )));
        }
        k
    } else {
        key.clone()
    };
    remove_entries(ctx, s, &key, old);
    s.check_row(&new)?;
    check_unique(ctx, s, &new_key, &new)?;
    check_parents(ctx, s, &new, Some(old))?;
    // children referencing values this update changes
    for (child, fk) in referencing(ctx, &s.name)? {
        let before = values_of(s, old, &fk.ref_cols);
        let after = values_of(s, &new, &fk.ref_cols);
        if before.iter().any(Value::is_null)
            || before
                .iter()
                .zip(&after)
                .all(|(a, b)| a.total_cmp(b) == Ordering::Equal)
        {
            continue;
        }
        let child = if child.name == s.name {
            s.clone()
        } else {
            child
        };
        if !rows_where(ctx, &child, &fk.cols, &before)?.is_empty() {
            return Err(Error::Constraint(format!(
                "cannot change ({}) of '{}': rows in '{}' still reference it",
                fk.ref_cols.join(", "),
                s.name,
                child.name
            )));
        }
    }
    write_entries(ctx, s, new_key, &new);
    Ok(())
}

/// Every (table, foreign key) that references `table`.
pub fn referencing(ctx: &Ctx, table: &str) -> Result<Vec<(Schema, ForeignKey)>> {
    let mut out = Vec::new();
    for sc in all_schemas(ctx)? {
        for fk in sc.fks.clone() {
            if fk.table == table {
                out.push((sc.clone(), fk));
            }
        }
    }
    Ok(out)
}

/// Delete a row, applying ON DELETE actions of referencing foreign keys.
pub fn delete_row(ctx: &mut Ctx, s: &Schema, key: &[u8], row: &[Value]) -> Result<()> {
    delete_row_depth(ctx, s, key, row, 0)
}

fn delete_row_depth(
    ctx: &mut Ctx,
    s: &Schema,
    key: &[u8],
    row: &[Value],
    depth: usize,
) -> Result<()> {
    if depth > 64 {
        return Err(Error::Constraint(
            "ON DELETE CASCADE chain is deeper than 64 levels".into(),
        ));
    }
    remove_entries(ctx, s, key, row);
    for (child, fk) in referencing(ctx, &s.name)? {
        let vals = values_of(s, row, &fk.ref_cols);
        if vals.iter().any(Value::is_null) {
            continue;
        }
        let refs = rows_where(ctx, &child, &fk.cols, &vals)?;
        if refs.is_empty() {
            continue;
        }
        match fk.on_delete {
            FkAction::Restrict => {
                return Err(Error::Constraint(format!(
                    "cannot delete from '{}': {} row(s) in '{}' reference it (foreign key {})",
                    s.name,
                    refs.len(),
                    child.name,
                    fk.cols.join(", ")
                )))
            }
            FkAction::Cascade => {
                for (k, r) in refs {
                    if ctx.get(&k).is_some() {
                        delete_row_depth(ctx, &child, &k, &r, depth + 1)?;
                    }
                }
            }
            FkAction::SetNull => {
                for (k, r) in refs {
                    let mut new = r.clone();
                    for c in &fk.cols {
                        let ci = child.col_index(c)?;
                        new[ci] = coerce(&child, ci, Value::Null)?;
                    }
                    update_row(ctx, &child, k, &r, new)?;
                }
            }
        }
    }
    Ok(())
}

/// All rows of a table, in primary-key order.
pub fn scan_rows(ctx: &Ctx, s: &Schema) -> Result<Vec<(Vec<u8>, Vec<Value>)>> {
    ctx.scan(&keys::row_prefix(&s.name))
        .into_iter()
        .map(|(k, raw)| Ok((k, decode_row(&raw)?)))
        .collect()
}

/// Write (or rewrite) every index entry of one index.
pub fn build_index(ctx: &mut Ctx, s: &Schema, ix: &Index) -> Result<()> {
    let one = Schema {
        indexes: vec![ix.clone()],
        ..s.clone()
    };
    for (key, row) in scan_rows(ctx, s)? {
        if ix.unique {
            check_unique(ctx, &one, &key, &row)?;
        }
        for k in index_entries(&one, &key, &row) {
            ctx.put(k, Vec::new());
        }
    }
    Ok(())
}

pub fn drop_index_entries(ctx: &mut Ctx, s: &Schema, ix: &Index) {
    if s.indexes
        .iter()
        .any(|o| o.name != ix.name && o.field == ix.field)
    {
        return; // another index shares these entries
    }
    for (k, _) in ctx.scan(&keys::index_prefix(&owner(&s.name), &ix.field)) {
        ctx.delete(k);
    }
}

/// Remove every key belonging to a table (rows, index entries, sequence, schema).
pub fn drop_table_data(ctx: &mut Ctx, name: &str) {
    for prefix in [
        keys::row_prefix(name),
        keys::index_owner_prefix(&owner(name)),
    ] {
        for (k, _) in ctx.scan(&prefix) {
            ctx.delete(k);
        }
    }
    ctx.delete(keys::table_meta(name));
    ctx.delete(keys::seq(&format!("t:{name}")));
}
