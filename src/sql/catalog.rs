//! Table schemas and how rows and index entries are laid out in the store.
//!
//! A schema is stored as JSON under `m\0t\0<table>`. The format written
//! since constraints were added is a superset of the original one, and the
//! original one (single-column indexes as `[name, col]` pairs, the primary
//! key as a per-column flag) is still read.
//!
//! Row keys:
//! * one primary-key column: `r\0<table>\0` + `keys::encode_id(value)` (unchanged)
//! * composite primary key:  `r\0<table>\0` + each component in index encoding
//! * no primary key:         `r\0<table>\0` + `encode_id(hidden row id)`
//!
//! Index entries: `i\0t:<table>\0<field>\0` + encoded values + row id bytes,
//! with an empty value. `<field>` is the column name for a one-column index
//! (unchanged) and the column names joined by U+001F for a composite one.
//! A one-column index has no entry for NULL; a composite index encodes NULL
//! as a component so that leftmost-prefix lookups still find the row.

use super::ast::*;
use super::parser::{parse_expr, to_sql};
use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use std::collections::BTreeMap;

/// Separates column names in the field of a composite index.
pub(super) const FIELD_SEP: char = '\u{1f}';

#[derive(Debug, Clone)]
pub(super) struct IndexDef {
    pub name: String,
    pub cols: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone)]
pub(super) struct Check {
    pub name: String,
    pub expr: Expr,
}

#[derive(Debug, Clone)]
pub(super) struct ForeignKey {
    pub name: String,
    pub cols: Vec<String>,
    pub ref_table: String,
    pub ref_cols: Vec<String>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

#[derive(Debug, Clone)]
pub(super) struct Schema {
    pub name: String,
    pub cols: Vec<Column>,
    /// Positions of the primary-key columns, in key order.
    pub pk: Vec<usize>,
    pub indexes: Vec<IndexDef>,
    pub checks: Vec<Check>,
    pub fks: Vec<ForeignKey>,
}

pub(super) type Scope = Vec<(String, String)>; // (table alias, column name)

/// Parsed schemas by table name, with the stored bytes they came from.
/// Per thread, so no locking; bounded, and only ever a cache of `parse`.
const SCHEMA_CACHE_SIZE: usize = 256;
thread_local! {
    static SCHEMA_CACHE: std::cell::RefCell<std::collections::HashMap<String, (Vec<u8>, Schema)>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Present in a stored schema exactly when it has at least one foreign key.
const FK_MARK: &[u8] = br#""fks":[{"#;

fn text_list(names: &[String]) -> Value {
    Value::Array(names.iter().map(|n| Value::Text(n.clone())).collect())
}

fn read_list(v: Option<&Value>) -> Option<Vec<String>> {
    match v {
        Some(Value::Array(a)) => a.iter().map(|x| x.as_str().map(str::to_string)).collect(),
        _ => None,
    }
}

impl Schema {
    pub fn col_index(&self, name: &str) -> Result<usize> {
        self.cols
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| Error::Schema(format!("table '{}' has no column '{name}'", self.name)))
    }

    pub fn positions(&self, names: &[String]) -> Result<Vec<usize>> {
        names.iter().map(|n| self.col_index(n)).collect()
    }

    pub fn scope(&self, alias: &str) -> Scope {
        self.cols.iter().map(|c| (alias.to_string(), c.name.clone())).collect()
    }

    pub fn pk_names(&self) -> Vec<String> {
        self.pk.iter().map(|&i| self.cols[i].name.clone()).collect()
    }

    /// Is `cols` (in any order) the primary key or covered by a unique index?
    pub fn is_unique_set(&self, cols: &[String]) -> bool {
        let same = |a: &[String], b: &[String]| a.len() == b.len() && a.iter().all(|x| b.contains(x));
        (!self.pk.is_empty() && same(&self.pk_names(), cols))
            || self.indexes.iter().any(|i| i.unique && same(&i.cols, cols))
    }

    /// Every constraint or index name in use on this table.
    pub fn names_in_use(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.indexes.iter().map(|i| i.name.as_str()).collect();
        v.extend(self.checks.iter().map(|c| c.name.as_str()));
        v.extend(self.fks.iter().map(|f| f.name.as_str()));
        v
    }

    /// `base`, or `base2`, `base3`, ... whichever is free.
    pub fn fresh_name(&self, base: &str) -> String {
        let used = self.names_in_use();
        if !used.contains(&base) {
            return base.to_string();
        }
        (2..).map(|n| format!("{base}{n}")).find(|c| !used.contains(&c.as_str())).unwrap()
    }

    pub fn to_json(&self) -> String {
        let cols = self
            .cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut o = BTreeMap::new();
                o.insert("name".to_string(), Value::Text(c.name.clone()));
                o.insert("type".to_string(), Value::Text(c.ty.name().to_string()));
                o.insert("pk".to_string(), Value::Bool(self.pk.contains(&i)));
                o.insert("not_null".to_string(), Value::Bool(c.not_null));
                if let Some(d) = &c.default {
                    o.insert("default".to_string(), d.clone());
                }
                if let Some(n) = c.max_len {
                    o.insert("max_len".to_string(), Value::Int(n as i64));
                }
                if c.auto {
                    o.insert("auto".to_string(), Value::Bool(true));
                }
                Value::Object(o)
            })
            .collect();
        let indexes = self
            .indexes
            .iter()
            .map(|ix| {
                let mut o = BTreeMap::new();
                o.insert("name".to_string(), Value::Text(ix.name.clone()));
                o.insert("cols".to_string(), text_list(&ix.cols));
                o.insert("unique".to_string(), Value::Bool(ix.unique));
                Value::Object(o)
            })
            .collect();
        let checks = self
            .checks
            .iter()
            .map(|c| {
                let mut o = BTreeMap::new();
                o.insert("name".to_string(), Value::Text(c.name.clone()));
                o.insert("sql".to_string(), Value::Text(to_sql(&c.expr)));
                Value::Object(o)
            })
            .collect();
        let fks = self
            .fks
            .iter()
            .map(|f| {
                let mut o = BTreeMap::new();
                o.insert("name".to_string(), Value::Text(f.name.clone()));
                o.insert("cols".to_string(), text_list(&f.cols));
                o.insert("ref_table".to_string(), Value::Text(f.ref_table.clone()));
                o.insert("ref_cols".to_string(), text_list(&f.ref_cols));
                o.insert("on_delete".to_string(), Value::Text(f.on_delete.name().into()));
                o.insert("on_update".to_string(), Value::Text(f.on_update.name().into()));
                Value::Object(o)
            })
            .collect();
        let mut o = BTreeMap::new();
        o.insert("cols".to_string(), Value::Array(cols));
        o.insert("primary_key".to_string(), text_list(&self.pk_names()));
        o.insert("indexes".to_string(), Value::Array(indexes));
        o.insert("checks".to_string(), Value::Array(checks));
        o.insert("fks".to_string(), Value::Array(fks));
        Value::Object(o).to_json()
    }

    pub fn save(&self, ctx: &mut Ctx) {
        ctx.put(keys::table_meta(&self.name), self.to_json().into_bytes());
    }

    pub fn exists(ctx: &Ctx, name: &str) -> bool {
        ctx.get(&keys::table_meta(name)).is_some()
    }

    /// Read a table's schema. The stored bytes are always read through
    /// `ctx` (so transactions see their own DDL and record the read); the
    /// parsed form is reused while those bytes are unchanged.
    pub fn load(ctx: &Ctx, name: &str) -> Result<Schema> {
        let raw = ctx.get(&keys::table_meta(name)).ok_or_else(|| Error::Schema(format!("no such table '{name}'")))?;
        Self::from_raw(name, raw)
    }

    fn from_raw(name: &str, raw: Vec<u8>) -> Result<Schema> {
        let hit = SCHEMA_CACHE.with(|c| {
            c.borrow().get(name).and_then(|(bytes, s)| (bytes == &raw).then(|| s.clone()))
        });
        if let Some(s) = hit {
            return Ok(s);
        }
        let s = Self::parse(name, &raw)?;
        SCHEMA_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() >= SCHEMA_CACHE_SIZE {
                c.clear();
            }
            c.insert(name.to_string(), (raw, s.clone()));
        });
        Ok(s)
    }

    fn parse(name: &str, raw: &[u8]) -> Result<Schema> {
        let bad = || Error::Corrupt(format!("schema of table '{name}' is unreadable"));
        let v = Value::parse(std::str::from_utf8(raw).map_err(|_| bad())?).map_err(|_| bad())?;
        let Some(Value::Array(cols)) = v.get("cols") else {
            return Err(bad());
        };
        let mut out = Vec::new();
        let mut pk_flags = Vec::new();
        for (i, c) in cols.iter().enumerate() {
            out.push(Column {
                name: c.get("name").and_then(|v| v.as_str()).ok_or_else(bad)?.to_string(),
                ty: c.get("type").and_then(|v| v.as_str()).and_then(Ty::from_name).ok_or_else(bad)?,
                not_null: c.get("not_null") == Some(&Value::Bool(true)),
                default: c.get("default").filter(|d| !d.is_null()).cloned(),
                max_len: c.get("max_len").and_then(|v| v.as_i64()).map(|n| n as usize),
                auto: c.get("auto") == Some(&Value::Bool(true)),
            });
            if c.get("pk") == Some(&Value::Bool(true)) {
                pk_flags.push(i);
            }
        }
        let mut s = Schema { name: name.to_string(), cols: out, pk: Vec::new(), indexes: Vec::new(), checks: Vec::new(), fks: Vec::new() };
        s.pk = match read_list(v.get("primary_key")) {
            Some(names) => s.positions(&names).map_err(|_| bad())?,
            None => pk_flags,
        };
        if let Some(Value::Array(list)) = v.get("indexes") {
            for entry in list {
                match entry {
                    // original format: [name, column]
                    Value::Array(p) => {
                        if let (Some(Value::Text(n)), Some(Value::Text(c))) = (p.first(), p.get(1)) {
                            s.indexes.push(IndexDef { name: n.clone(), cols: vec![c.clone()], unique: false });
                        }
                    }
                    Value::Object(_) => s.indexes.push(IndexDef {
                        name: entry.get("name").and_then(|v| v.as_str()).ok_or_else(bad)?.to_string(),
                        cols: read_list(entry.get("cols")).ok_or_else(bad)?,
                        unique: entry.get("unique") == Some(&Value::Bool(true)),
                    }),
                    _ => return Err(bad()),
                }
            }
        }
        if let Some(Value::Array(list)) = v.get("checks") {
            for c in list {
                let name = c.get("name").and_then(|v| v.as_str()).ok_or_else(bad)?.to_string();
                let sql = c.get("sql").and_then(|v| v.as_str()).ok_or_else(bad)?;
                let expr = parse_expr(sql).map_err(|_| bad())?;
                s.checks.push(Check { name, expr });
            }
        }
        if let Some(Value::Array(list)) = v.get("fks") {
            for f in list {
                let action = |k: &str| f.get(k).and_then(|v| v.as_str()).and_then(FkAction::from_name).ok_or_else(bad);
                s.fks.push(ForeignKey {
                    name: f.get("name").and_then(|v| v.as_str()).ok_or_else(bad)?.to_string(),
                    cols: read_list(f.get("cols")).ok_or_else(bad)?,
                    ref_table: f.get("ref_table").and_then(|v| v.as_str()).ok_or_else(bad)?.to_string(),
                    ref_cols: read_list(f.get("ref_cols")).ok_or_else(bad)?,
                    on_delete: action("on_delete")?,
                    on_update: action("on_update")?,
                });
            }
        }
        Ok(s)
    }
}

pub fn tables(ctx: &Ctx) -> Vec<String> {
    ctx.scan(keys::TABLE_META_PREFIX)
        .into_iter()
        .map(|(k, _)| String::from_utf8_lossy(&k[keys::TABLE_META_PREFIX.len()..]).into_owned())
        .collect()
}

/// Every table whose foreign keys point at `table` (including `table` itself).
/// Scans every schema, so that inside a transaction a concurrent change to
/// any table's foreign keys is a conflict.
pub(super) fn referencing(ctx: &Ctx, table: &str) -> Result<Vec<(Schema, ForeignKey)>> {
    let mut out = Vec::new();
    for (k, raw) in ctx.scan(keys::TABLE_META_PREFIX) {
        // most tables have no foreign keys at all: skip them without parsing
        if !raw.windows(FK_MARK.len()).any(|w| w == FK_MARK) {
            continue;
        }
        let t = String::from_utf8_lossy(&k[keys::TABLE_META_PREFIX.len()..]).into_owned();
        let s = Schema::from_raw(&t, raw)?;
        for fk in &s.fks {
            if fk.ref_table == table {
                out.push((s.clone(), fk.clone()));
            }
        }
    }
    Ok(out)
}

// ================================================================== encoding

pub(super) fn decode_row(raw: &[u8]) -> Result<Vec<Value>> {
    let parsed = std::str::from_utf8(raw).ok().and_then(|s| Value::parse(s).ok());
    match parsed {
        Some(Value::Array(a)) => Ok(a),
        _ => Err(Error::Corrupt("stored row is unreadable".into())),
    }
}

pub(super) fn encode_row(row: &[Value]) -> Vec<u8> {
    let mut out = String::with_capacity(16 * row.len() + 2);
    out.push('[');
    for (i, v) in row.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        v.write_json(&mut out);
    }
    out.push(']');
    out.into_bytes()
}

/// Self-delimiting, order-preserving encoding of any value, NULL included
/// (NULL sorts first, as in `Value::total_cmp`).
pub(super) fn enc_component(v: &Value) -> Vec<u8> {
    match v {
        Value::Null => vec![0],
        _ => keys::encode_index_value(v).unwrap_or_else(|| {
            let mut out = vec![9u8];
            out.extend(v.to_json().bytes().filter(|b| *b != 0));
            out.extend_from_slice(&[0, 0]);
            out
        }),
    }
}

/// Byte key that groups values the way SQL equality does (1 = 1.0).
pub(super) fn group_key<'v>(vals: impl IntoIterator<Item = &'v Value>) -> Vec<u8> {
    let mut k = Vec::new();
    for v in vals {
        k.extend(enc_component(v));
    }
    k
}

/// Length of one component written by `enc_component` / `encode_index_value`.
pub(super) fn component_len(b: &[u8]) -> Option<usize> {
    match b.first()? {
        0 => Some(1),
        1 => (b.len() >= 2).then_some(2),
        2 => (b.len() >= 9).then_some(9),
        4 | 9 => {
            let mut i = 1;
            while i + 1 < b.len() {
                if b[i] == 0 {
                    if b[i + 1] == 0 {
                        return Some(i + 2);
                    }
                    i += 2; // escaped NUL
                } else {
                    i += 1;
                }
            }
            None
        }
        _ => None,
    }
}

/// The smallest key greater than every key that starts with `prefix`.
pub(super) fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut p = prefix.to_vec();
    while let Some(last) = p.pop() {
        if last < 0xff {
            p.push(last + 1);
            return Some(p);
        }
    }
    None
}

pub(super) fn owner(table: &str) -> String {
    format!("t:{table}")
}

pub(super) fn index_field(cols: &[String]) -> String {
    cols.join(&FIELD_SEP.to_string())
}

pub(super) fn index_base(table: &str, cols: &[String]) -> Vec<u8> {
    keys::index_prefix(&owner(table), &index_field(cols))
}

/// Encoded index values for a row, or None when a one-column index skips it (NULL).
pub(super) fn index_values(vals: &[&Value]) -> Option<Vec<u8>> {
    if vals.len() == 1 {
        keys::encode_index_value(vals[0])
    } else {
        Some(group_key(vals.iter().copied()))
    }
}

/// Sequence name for the hidden row id of a table without a primary key.
pub(super) fn rowid_seq(table: &str) -> String {
    format!("t:{table}")
}

/// Sequence name of an auto-increment column. Table names cannot contain
/// '.', so this never collides with another table's sequences.
pub(super) fn auto_seq(table: &str, col: &str) -> String {
    format!("t:{table}.{col}")
}

/// Storage key of a row from its primary-key values.
pub(super) fn pk_key(s: &Schema, row: &[Value]) -> Result<Vec<u8>> {
    let mut k = keys::row_prefix(&s.name);
    if s.pk.len() == 1 {
        k.extend(keys::encode_id(&row[s.pk[0]])?);
    } else {
        for &i in &s.pk {
            k.extend(enc_component(&row[i]));
        }
    }
    Ok(k)
}

/// Storage key for a new row: its primary key, or a fresh hidden row id.
pub(super) fn new_row_key(ctx: &mut Ctx, s: &Schema, row: &[Value]) -> Result<Vec<u8>> {
    if s.pk.is_empty() {
        let id = keys::next_seq(ctx, &rowid_seq(&s.name))?;
        let mut k = keys::row_prefix(&s.name);
        k.extend(keys::encode_id(&Value::Int(id))?);
        Ok(k)
    } else {
        pk_key(s, row)
    }
}

/// Index entry of one row in one index, or None if the index skips the row.
pub(super) fn index_entry(s: &Schema, ix: &IndexDef, id: &[u8], row: &[Value]) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(ix.cols.len());
    for c in &ix.cols {
        vals.push(&row[s.cols.iter().position(|x| &x.name == c)?]);
    }
    let mut k = index_base(&s.name, &ix.cols);
    k.extend(index_values(&vals)?);
    k.extend_from_slice(id);
    Some(k)
}

/// The row-id part of a row key.
pub(super) fn row_id<'k>(s: &Schema, key: &'k [u8]) -> &'k [u8] {
    &key[keys::row_prefix(&s.name).len()..]
}

/// Where a table's rows and index entries go, worked out once per batch of
/// writes instead of once per row.
pub(super) struct Layout {
    prefix_len: usize,
    /// (entry prefix, column positions) per index.
    indexes: Vec<(Vec<u8>, Vec<usize>)>,
}

impl Layout {
    pub fn new(s: &Schema) -> Layout {
        let indexes = s
            .indexes
            .iter()
            .filter_map(|ix| Some((index_base(&s.name, &ix.cols), s.positions(&ix.cols).ok()?)))
            .collect();
        Layout { prefix_len: keys::row_prefix(&s.name).len(), indexes }
    }

    fn entries<'a>(&'a self, key: &'a [u8], row: &'a [Value]) -> impl Iterator<Item = Vec<u8>> + 'a {
        let id = &key[self.prefix_len..];
        self.indexes.iter().filter_map(move |(base, pos)| {
            let vals: Vec<&Value> = pos.iter().map(|&p| &row[p]).collect();
            let enc = index_values(&vals)?;
            let mut k = Vec::with_capacity(base.len() + enc.len() + id.len());
            k.extend_from_slice(base);
            k.extend(enc);
            k.extend_from_slice(id);
            Some(k)
        })
    }

    pub fn put(&self, ctx: &mut Ctx, key: Vec<u8>, row: &[Value]) {
        let entries: Vec<Vec<u8>> = self.entries(&key, row).collect();
        for k in entries {
            ctx.put(k, Vec::new());
        }
        ctx.put(key, encode_row(row));
    }

    pub fn delete(&self, ctx: &mut Ctx, key: &[u8], row: &[Value]) {
        let entries: Vec<Vec<u8>> = self.entries(key, row).collect();
        for k in entries {
            ctx.delete(k);
        }
        ctx.delete(key.to_vec());
    }
}

/// Every stored row of a table, in key order.
pub(super) fn all_rows(ctx: &Ctx, s: &Schema) -> Result<Vec<(Vec<u8>, Vec<Value>)>> {
    ctx.scan(&keys::row_prefix(&s.name)).into_iter().map(|(k, raw)| Ok((k, decode_row(&raw)?))).collect()
}

/// Remove every entry of an index field (shared by indexes on the same columns).
pub(super) fn clear_index_field(ctx: &mut Ctx, table: &str, cols: &[String]) {
    for (k, _) in ctx.scan(&index_base(table, cols)) {
        ctx.delete(k);
    }
}

/// Write the entries of one index for every row; refuse duplicates if it is unique.
pub(super) fn build_index(ctx: &mut Ctx, s: &Schema, ix: &IndexDef) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    let pos = s.positions(&ix.cols)?;
    for (key, row) in all_rows(ctx, s)? {
        if ix.unique && pos.iter().all(|&p| !row[p].is_null()) && !seen.insert(group_key(pos.iter().map(|&p| &row[p]))) {
            let vals: Vec<String> = pos.iter().map(|&p| row[p].to_json()).collect();
            return Err(Error::Constraint(format!(
                "cannot create unique index '{}': duplicate value ({}) = ({}) in table '{}'",
                ix.name,
                ix.cols.join(", "),
                vals.join(", "),
                s.name
            )));
        }
        let id = row_id(s, &key).to_vec();
        if let Some(k) = index_entry(s, ix, &id, &row) {
            ctx.put(k, Vec::new());
        }
    }
    Ok(())
}
