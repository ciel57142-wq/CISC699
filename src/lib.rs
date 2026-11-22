//! # polydb
//!
//! One embedded database engine, three data models:
//!
//! * **relational** – typed tables, primary keys, SQL with joins and aggregates
//! * **document**   – schemaless JSON collections with MongoDB-style queries
//! * **key-value**  – string keys, byte values
//!
//! All three live in a single ordered, write-ahead-logged store, so a
//! transaction can span them and still commit atomically. Many sessions can
//! work on one database at once with serializable isolation, and the
//! [`server`] module exposes it over TCP in four protocols: its own, plus
//! the Redis, PostgreSQL and MongoDB wire protocols.
//!
//! ```no_run
//! use polydb::Database;
//! let mut db = Database::open("my-data").unwrap();
//! db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)").unwrap();
//! db.execute("INSERT INTO users VALUES (1, 'Ada')").unwrap();
//! db.execute("db.events.insert({user: 1, type: 'signup'})").unwrap();
//! db.execute("SET last_user 1").unwrap();
//! println!("{}", db.execute("SELECT * FROM users").unwrap());
//! ```

pub mod doc;
pub mod error;
mod keys;
pub mod kv;
pub mod mongo;
pub mod pgwire;
pub mod resp;
pub mod server;
mod sql;
pub mod storage;
pub mod value;

pub use doc::FindOptions;
pub use error::{Error, Result};
pub use value::Value;

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use storage::{Ctx, ReadSet, Store, WriteSet};
use value::JsonParser;

/// The result of one command.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// A status message ("3 row(s) inserted").
    Done(String),
    /// A relational result set.
    Rows { columns: Vec<String>, rows: Vec<Vec<Value>> },
    /// Documents returned by a collection query.
    Docs(Vec<Value>),
    /// A single value (key-value reads, counts, generated ids).
    Scalar(Value),
}

impl fmt::Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Output::Done(m) => write!(f, "{m}"),
            Output::Scalar(v) => write!(f, "{}", v.display()),
            Output::Docs(docs) => {
                for d in docs {
                    writeln!(f, "{}", d.to_json())?;
                }
                write!(f, "({} document(s))", docs.len())
            }
            Output::Rows { columns, rows } => {
                let cells: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(|v| v.display()).collect()).collect();
                let mut w: Vec<usize> = columns.iter().map(|c| c.chars().count()).collect();
                for r in &cells {
                    for (i, c) in r.iter().enumerate() {
                        w[i] = w[i].max(c.chars().count());
                    }
                }
                let line = |f: &mut fmt::Formatter<'_>, cols: &[String]| -> fmt::Result {
                    for (i, c) in cols.iter().enumerate() {
                        let pad = w[i] - c.chars().count();
                        write!(f, "{}{c}{}", if i == 0 { " " } else { " | " }, " ".repeat(pad))?;
                    }
                    writeln!(f)
                };
                line(f, columns)?;
                writeln!(f, "{}", w.iter().map(|n| "-".repeat(n + 2)).collect::<Vec<_>>().join("+"))?;
                for r in &cells {
                    line(f, r)?;
                }
                write!(f, "({} row(s))", rows.len())
            }
        }
    }
}

/// State shared by every session of one database.
struct Shared {
    store: Store,
    /// Number of commits so far; a transaction remembers the value it started at.
    version: u64,
    /// Keys written by recent commits, kept only while a transaction that
    /// started before them is still open.
    log: VecDeque<(u64, Vec<Vec<u8>>)>,
    /// Start versions of open transactions (version -> how many).
    active: BTreeMap<u64, usize>,
}

impl Shared {
    fn commit(&mut self, ws: WriteSet) -> Result<()> {
        if ws.is_empty() {
            return Ok(());
        }
        let keys: Option<Vec<Vec<u8>>> = (!self.active.is_empty()).then(|| ws.keys().cloned().collect());
        self.store.commit(ws)?;
        self.version += 1;
        if let Some(keys) = keys {
            self.log.push_back((self.version, keys));
        }
        Ok(())
    }

    fn release(&mut self, start: u64) {
        if let Some(n) = self.active.get_mut(&start) {
            *n -= 1;
            if *n == 0 {
                self.active.remove(&start);
            }
        }
        match self.active.keys().next().copied() {
            None => self.log.clear(),
            Some(oldest) => {
                while self.log.front().map_or(false, |(v, _)| *v <= oldest) {
                    self.log.pop_front();
                }
            }
        }
    }
}

struct Txn {
    writes: WriteSet,
    reads: ReadSet,
    start: u64,
}

/// A session on a database.
///
/// Each session has its own transaction state; any number of sessions (one
/// per thread or per network connection, see [`Database::session`]) share
/// the same data. Readers run in parallel; commits are serialised.
///
/// Isolation is serializable. A single statement runs atomically. An
/// explicit transaction buffers its writes and records what it read; at
/// `COMMIT`, if another session changed any of that data in the meantime,
/// the commit fails with [`Error::Conflict`] and writes nothing.
pub struct Database {
    shared: Arc<RwLock<Shared>>,
    txn: Option<Txn>,
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Some(t) = self.txn.take() {
            self.write_lock().release(t.start);
        }
    }
}

impl Database {
    fn from_store(store: Store) -> Database {
        let shared = Shared { store, version: 0, log: VecDeque::new(), active: BTreeMap::new() };
        Database { shared: Arc::new(RwLock::new(shared)), txn: None }
    }

    /// Open or create a durable database in directory `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        Ok(Database::from_store(Store::open(path)?))
    }

    /// Open a durable database and, after recovery, verify every relational
    /// index entry against the rows, rebuilding the indexes of any table
    /// where they disagree. Costs one pass over the data; returns what it
    /// found. Rows and index entries are written in the same atomic commit,
    /// so on an undamaged database this finds nothing; it is the repair path
    /// for data written by other tools or damaged outside the engine.
    pub fn open_verified(path: impl AsRef<Path>) -> Result<(Database, sql::RepairReport)> {
        let mut db = Database::open(path)?;
        let report = db.repair_indexes()?;
        Ok((db, report))
    }

    /// Verify every relational index against its rows and rebuild the
    /// indexes of tables where they disagree, as one atomic commit.
    pub fn repair_indexes(&mut self) -> Result<sql::RepairReport> {
        self.run(true, sql::repair_indexes)
    }

    /// A database that lives only in memory.
    pub fn in_memory() -> Database {
        Database::from_store(Store::memory())
    }

    /// Another session on the same data, with its own transaction state.
    /// Sessions are `Send`: give one to each thread.
    pub fn session(&self) -> Database {
        Database { shared: self.shared.clone(), txn: None }
    }

    fn read_lock(&self) -> RwLockReadGuard<'_, Shared> {
        self.shared.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_lock(&self) -> RwLockWriteGuard<'_, Shared> {
        self.shared.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Every commit is fsynced before it is acknowledged (the default, and
    /// what you want for accuracy). `false` skips the fsync: faster, but the
    /// most recent commits can be lost on power failure (never corrupted).
    pub fn set_sync(&mut self, sync: bool) {
        self.write_lock().store.sync = sync;
    }

    /// Run `f` inside this session's open transaction, or as its own atomic
    /// statement. A failing statement leaves no partial writes behind.
    fn run<T>(&mut self, writes: bool, f: impl FnOnce(&mut Ctx) -> Result<T>) -> Result<T> {
        if let Some(mut t) = self.txn.take() {
            let guard = self.shared.read().unwrap_or_else(|e| e.into_inner());
            let savepoint = t.writes.clone();
            let mut ctx = Ctx { store: &guard.store, writes: t.writes, reads: Some(RefCell::new(t.reads)) };
            let res = f(&mut ctx);
            t.reads = ctx.reads.take().map(RefCell::into_inner).unwrap_or_default();
            t.writes = if res.is_ok() { ctx.writes } else { savepoint };
            drop(guard);
            self.txn = Some(t);
            return res;
        }
        if writes {
            // Hold the write lock across read-modify-write so the statement is atomic.
            let mut guard = self.shared.write().unwrap_or_else(|e| e.into_inner());
            let mut ctx = Ctx { store: &guard.store, writes: WriteSet::new(), reads: None };
            let out = f(&mut ctx)?;
            let ws = ctx.writes;
            guard.commit(ws)?;
            Ok(out)
        } else {
            let guard = self.shared.read().unwrap_or_else(|e| e.into_inner());
            let mut ctx = Ctx { store: &guard.store, writes: WriteSet::new(), reads: None };
            f(&mut ctx)
        }
    }

    // ------------------------------------------------------- transactions

    pub fn begin(&mut self) -> Result<()> {
        if self.txn.is_some() {
            return Err(Error::Txn("a transaction is already open".into()));
        }
        let mut g = self.write_lock();
        let start = g.version;
        *g.active.entry(start).or_insert(0) += 1;
        drop(g);
        self.txn = Some(Txn { writes: WriteSet::new(), reads: ReadSet::default(), start });
        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        let t = self.txn.take().ok_or_else(|| Error::Txn("no transaction is open".into()))?;
        let mut g = self.write_lock();
        let stale = g.log.iter().any(|(v, keys)| *v > t.start && keys.iter().any(|k| t.reads.touches(k)));
        g.release(t.start);
        if stale {
            return Err(Error::Conflict(
                "another session changed data this transaction read; nothing was written, retry it".into(),
            ));
        }
        g.commit(t.writes)
    }

    pub fn rollback(&mut self) -> Result<()> {
        let t = self.txn.take().ok_or_else(|| Error::Txn("no transaction is open".into()))?;
        self.write_lock().release(t.start);
        Ok(())
    }

    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// Fold the write-ahead log into a snapshot file.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.write_lock().store.checkpoint()
    }

    /// Number of stored entries across all models (rows, documents, keys, metadata, index entries).
    pub fn entry_count(&self) -> usize {
        self.read_lock().store.len()
    }

    // ------------------------------------------------------------ typed API

    /// Run one SQL statement.
    pub fn sql(&mut self, statement: &str) -> Result<Output> {
        let read = sql::is_read_only(statement);
        self.run(!read, |ctx| sql::execute(ctx, statement))
    }

    pub fn kv_set(&mut self, key: &str, value: &[u8]) -> Result<()> {
        self.run(true, |ctx| {
            kv::set(ctx, key, value);
            Ok(())
        })
    }

    pub fn kv_get(&mut self, key: &str) -> Result<Option<Vec<u8>>> {
        self.run(false, |ctx| Ok(kv::get(ctx, key)))
    }

    pub fn kv_del(&mut self, key: &str) -> Result<bool> {
        self.run(true, |ctx| Ok(kv::del(ctx, key)))
    }

    pub fn kv_keys(&mut self, prefix: &str) -> Result<Vec<String>> {
        self.run(false, |ctx| Ok(kv::keys(ctx, prefix)))
    }

    pub fn kv_incr(&mut self, key: &str, by: i64) -> Result<i64> {
        self.run(true, |ctx| kv::incr(ctx, key, by))
    }

    /// Insert a document and return its `_id`.
    pub fn doc_insert(&mut self, collection: &str, doc: Value) -> Result<Value> {
        self.run(true, |ctx| doc::insert(ctx, collection, doc))
    }

    /// Insert several documents atomically (all or none); returns their ids.
    pub fn doc_insert_many(&mut self, collection: &str, docs: Vec<Value>) -> Result<Vec<Value>> {
        self.run(true, |ctx| doc::insert_many(ctx, collection, docs))
    }

    pub fn doc_find(&mut self, collection: &str, filter: &Value, opts: &FindOptions) -> Result<Vec<Value>> {
        self.run(false, |ctx| doc::find(ctx, collection, filter, opts))
    }

    /// Number of documents matching `filter`.
    pub fn doc_count(&mut self, collection: &str, filter: &Value) -> Result<u64> {
        self.run(false, |ctx| doc::count(ctx, collection, filter))
    }

    /// Distinct values of `field` among matching documents, sorted.
    pub fn doc_distinct(&mut self, collection: &str, field: &str, filter: &Value) -> Result<Vec<Value>> {
        self.run(false, |ctx| doc::distinct(ctx, collection, field, filter))
    }

    /// The access path a filter uses and how many documents it reads.
    pub fn doc_explain(&mut self, collection: &str, filter: &Value) -> Result<Value> {
        self.run(false, |ctx| doc::explain(ctx, collection, filter))
    }

    /// Run an aggregation pipeline (see [`doc::aggregate`](crate::doc::aggregate) for the stages).
    pub fn doc_aggregate(&mut self, collection: &str, pipeline: &[Value]) -> Result<Vec<Value>> {
        self.run(false, |ctx| doc::aggregate(ctx, collection, pipeline))
    }

    /// Update every matching document; returns how many matched.
    pub fn doc_update(&mut self, collection: &str, filter: &Value, update: &Value) -> Result<u64> {
        self.doc_update_with(collection, filter, update, true, false).map(|r| r.0)
    }

    /// Update with control over `multi` (all matches vs first) and `upsert`.
    /// Returns (documents matched, id of an upserted document).
    pub fn doc_update_with(
        &mut self,
        collection: &str,
        filter: &Value,
        update: &Value,
        multi: bool,
        upsert: bool,
    ) -> Result<(u64, Option<Value>)> {
        self.doc_update_full(collection, filter, update, multi, upsert).map(|r| (r.matched, r.upserted_id))
    }

    /// Update returning matched and modified counts and any upserted id.
    pub fn doc_update_full(
        &mut self,
        collection: &str,
        filter: &Value,
        update: &Value,
        multi: bool,
        upsert: bool,
    ) -> Result<doc::UpdateResult> {
        self.run(true, |ctx| doc::update(ctx, collection, filter, update, multi, upsert))
    }

    /// Delete every matching document; returns how many.
    pub fn doc_delete(&mut self, collection: &str, filter: &Value) -> Result<u64> {
        self.doc_delete_with(collection, filter, None)
    }

    /// Delete at most `limit` matching documents.
    pub fn doc_delete_with(&mut self, collection: &str, filter: &Value, limit: Option<u64>) -> Result<u64> {
        self.run(true, |ctx| doc::delete(ctx, collection, filter, limit))
    }

    /// Several write operations as one atomic unit (see [`doc::bulk_write`](crate::doc::bulk_write)).
    pub fn doc_bulk_write(&mut self, collection: &str, ops: &[Value]) -> Result<Value> {
        self.run(true, |ctx| doc::bulk_write(ctx, collection, ops))
    }

    pub fn doc_drop(&mut self, collection: &str) -> Result<bool> {
        self.run(true, |ctx| doc::drop(ctx, collection))
    }

    pub fn doc_collections(&mut self) -> Result<Vec<String>> {
        self.run(false, |ctx| Ok(doc::collections(ctx)))
    }

    /// Index a field of a collection; returns false if it was already indexed.
    pub fn doc_create_index(&mut self, collection: &str, field: &str) -> Result<bool> {
        self.doc_create_index_on(collection, &[field], false)
    }

    /// Create a single-field or compound index, optionally unique.
    pub fn doc_create_index_on(&mut self, collection: &str, fields: &[&str], unique: bool) -> Result<bool> {
        let fields = fields.iter().map(|f| f.to_string()).collect();
        self.run(true, |ctx| doc::create_index(ctx, collection, fields, unique))
    }

    /// Drop an index by name (`a_1_b_1`), field, or key spec.
    pub fn doc_drop_index(&mut self, collection: &str, spec: &Value) -> Result<bool> {
        self.run(true, |ctx| doc::drop_index(ctx, collection, spec))
    }

    pub fn doc_indexes(&mut self, collection: &str) -> Result<Vec<Value>> {
        self.run(false, |ctx| doc::list_indexes(ctx, collection))
    }

    /// Set or clear (`null`) a collection's document schema.
    pub fn doc_set_schema(&mut self, collection: &str, schema: &Value) -> Result<()> {
        self.run(true, |ctx| doc::set_schema(ctx, collection, schema))
    }

    /// Consistency report for a collection's documents and index entries.
    pub fn doc_validate(&mut self, collection: &str) -> Result<Value> {
        self.run(false, |ctx| doc::validate(ctx, collection))
    }

    /// Rebuild a collection's index entries; returns how many were written.
    pub fn doc_reindex(&mut self, collection: &str) -> Result<u64> {
        self.run(true, |ctx| doc::reindex(ctx, collection))
    }

    // ---------------------------------------------------- unified commands

    /// Run one command in any of the three languages:
    ///
    /// * SQL: `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE|[UNIQUE] INDEX`, `DROP TABLE|INDEX`,
    ///   `ALTER TABLE`, `TRUNCATE`, `EXPLAIN [ANALYZE]`, `ANALYZE`, `DESCRIBE`, `CHECK TABLE`, `REINDEX`
    /// * documents: `db.<collection>.<method>(...)`
    /// * key-value: `SET`, `GET`, `DEL`, `EXISTS`, `KEYS`, `INCR`
    /// * control: `BEGIN`, `COMMIT`, `ROLLBACK`, `CHECKPOINT`, `SHOW TABLES`, `SHOW COLLECTIONS`
    pub fn execute(&mut self, command: &str) -> Result<Output> {
        let s = command.trim().trim_end_matches(';').trim_end();
        if let Some(rest) = s.strip_prefix("db.") {
            let method = rest.split_once('.').map_or("", |(_, m)| m.split('(').next().unwrap_or("")).to_ascii_lowercase();
            let read = matches!(
                method.trim(),
                "find" | "findone" | "count" | "countdocuments" | "distinct" | "aggregate" | "getindexes" | "getschema"
                    | "validate" | "explain"
            );
            return self.run(!read, |ctx| doc_command(ctx, s));
        }
        let (word, rest) = match s.find(char::is_whitespace) {
            Some(i) => (&s[..i], s[i..].trim_start()),
            None => (s, ""),
        };
        match word.to_ascii_lowercase().as_str() {
            "" => Err(Error::Parse("empty command".into())),
            "begin" => self.begin().map(|_| Output::Done("transaction started".into())),
            "commit" => self.commit().map(|_| Output::Done("committed".into())),
            "rollback" => self.rollback().map(|_| Output::Done("rolled back".into())),
            "checkpoint" => self.checkpoint().map(|_| Output::Done("checkpoint written".into())),
            "show" => {
                let what = rest.to_ascii_lowercase();
                let (header, names) = match what.as_str() {
                    "tables" => ("table", self.run(false, |ctx| Ok(sql::tables(ctx)))?),
                    "collections" => ("collection", self.run(false, |ctx| Ok(doc::collections(ctx)))?),
                    _ => return Err(Error::Parse("use SHOW TABLES or SHOW COLLECTIONS".into())),
                };
                Ok(Output::Rows {
                    columns: vec![header.to_string()],
                    rows: names.into_iter().map(|n| vec![Value::Text(n)]).collect(),
                })
            }
            w @ ("get" | "exists" | "keys") => self.run(false, |ctx| kv_command(ctx, w, rest)),
            w @ ("set" | "del" | "incr") => self.run(true, |ctx| kv_command(ctx, w, rest)),
            "select" | "insert" | "update" | "delete" | "create" | "drop" | "alter" | "truncate" | "explain"
            | "describe" | "desc" | "check" | "reindex" | "analyze" => self.sql(s),
            other => Err(Error::Parse(format!(
                "unknown command '{other}': expected SQL, a db.<collection>.<method>(...) call, or SET/GET/DEL/EXISTS/KEYS/INCR"
            ))),
        }
    }
}

// ------------------------------------------------------- key-value commands

/// Take one argument: a bare word, or a quoted string with backslash escapes.
fn take_arg(s: &str) -> Result<(String, &str)> {
    let s = s.trim_start();
    let mut chars = s.char_indices();
    match chars.next() {
        None => Err(Error::Parse("missing argument".into())),
        Some((_, q @ ('"' | '\''))) => {
            let mut out = String::new();
            let mut escaped = false;
            for (i, c) in chars {
                if escaped {
                    out.push(match c {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    return Ok((out, &s[i + 1..]));
                } else {
                    out.push(c);
                }
            }
            Err(Error::Parse("unterminated quoted argument".into()))
        }
        Some(_) => {
            let end = s.find(char::is_whitespace).unwrap_or(s.len());
            Ok((s[..end].to_string(), &s[end..]))
        }
    }
}

fn no_more(rest: &str) -> Result<()> {
    if rest.trim().is_empty() {
        Ok(())
    } else {
        Err(Error::Parse(format!("unexpected extra argument '{}'", rest.trim())))
    }
}

fn kv_command(ctx: &mut Ctx, word: &str, rest: &str) -> Result<Output> {
    if word == "keys" {
        let prefix = if rest.trim().is_empty() { String::new() } else { take_arg(rest)?.0 };
        let rows = kv::keys(ctx, &prefix).into_iter().map(|k| vec![Value::Text(k)]).collect();
        return Ok(Output::Rows { columns: vec!["key".into()], rows });
    }
    let (key, rest) = take_arg(rest)?;
    match word {
        "set" => {
            let rest = rest.trim();
            if rest.is_empty() {
                return Err(Error::Parse("SET needs a key and a value".into()));
            }
            // A quoted value is unescaped; anything else is taken verbatim to the end of the line.
            let value = if rest.starts_with('"') || rest.starts_with('\'') {
                let (v, tail) = take_arg(rest)?;
                no_more(tail)?;
                v
            } else {
                rest.to_string()
            };
            kv::set(ctx, &key, value.as_bytes());
            Ok(Output::Done("OK".into()))
        }
        "get" => {
            no_more(rest)?;
            Ok(Output::Scalar(match kv::get(ctx, &key) {
                Some(v) => Value::Text(String::from_utf8_lossy(&v).into_owned()),
                None => Value::Null,
            }))
        }
        "del" => {
            no_more(rest)?;
            Ok(Output::Scalar(Value::Int(kv::del(ctx, &key) as i64)))
        }
        "exists" => {
            no_more(rest)?;
            Ok(Output::Scalar(Value::Bool(kv::get(ctx, &key).is_some())))
        }
        _ => {
            let by = if rest.trim().is_empty() {
                1
            } else {
                rest.trim().parse::<i64>().map_err(|_| Error::Parse("INCR amount must be an integer".into()))?
            };
            Ok(Output::Scalar(Value::Int(kv::incr(ctx, &key, by)?)))
        }
    }
}

// -------------------------------------------------------- document commands

/// Parse `method(args...).method(args...)` where each argument is JSON.
fn parse_calls(s: &str) -> Result<Vec<(String, Vec<Value>)>> {
    let b = s.as_bytes();
    let mut pos = 0;
    let mut calls = Vec::new();
    let skip_ws = |pos: &mut usize| {
        while *pos < b.len() && b[*pos].is_ascii_whitespace() {
            *pos += 1;
        }
    };
    loop {
        let start = pos;
        while pos < b.len() && (b[pos].is_ascii_alphanumeric() || b[pos] == b'_') {
            pos += 1;
        }
        let name = s[start..pos].to_ascii_lowercase();
        if name.is_empty() || b.get(pos) != Some(&b'(') {
            return Err(Error::Parse("expected db.<collection>.<method>(...)".into()));
        }
        pos += 1;
        let mut args = Vec::new();
        loop {
            skip_ws(&mut pos);
            match b.get(pos) {
                None => return Err(Error::Parse("missing ')'".into())),
                Some(b')') => {
                    pos += 1;
                    break;
                }
                Some(b',') => pos += 1,
                Some(_) => {
                    let mut jp = JsonParser::at(s, pos);
                    args.push(jp.value()?);
                    pos = jp.pos;
                }
            }
        }
        calls.push((name, args));
        skip_ws(&mut pos);
        match b.get(pos) {
            None => return Ok(calls),
            Some(b'.') => pos += 1,
            Some(_) => return Err(Error::Parse(format!("unexpected text after ')': '{}'", &s[pos..]))),
        }
    }
}

fn doc_command(ctx: &mut Ctx, s: &str) -> Result<Output> {
    let rest = &s[3..];
    let dot = rest.find('.').ok_or_else(|| Error::Parse("expected db.<collection>.<method>(...)".into()))?;
    let coll = &rest[..dot];
    let calls = parse_calls(&rest[dot + 1..])?;
    let (method, args) = &calls[0];
    let empty = Value::Object(Default::default());
    let arg = |i: usize| args.get(i).unwrap_or(&empty);
    let need = |i: usize, what: &str| {
        args.get(i).ok_or_else(|| Error::Parse(format!("{method}() needs {what}")))
    };
    let as_count = |v: &Value| {
        v.as_i64()
            .filter(|n| *n >= 0)
            .map(|n| n as usize)
            .ok_or_else(|| Error::Parse("expected a non-negative integer".into()))
    };
    let option = |i: usize, key: &str| args.get(i).and_then(|o| o.get(key)) == Some(&Value::Bool(true));

    let is_find = matches!(method.as_str(), "find" | "findone");
    if !is_find && calls.len() > 1 {
        return Err(Error::Parse(format!("{method}() cannot be chained")));
    }
    match method.as_str() {
        "insert" | "insertone" | "insertmany" => match need(0, "a document")? {
            Value::Array(docs) => Ok(Output::Scalar(Value::Array(doc::insert_many(ctx, coll, docs.clone())?))),
            d => Ok(Output::Scalar(doc::insert(ctx, coll, d.clone())?)),
        },
        "find" | "findone" => {
            let mut opts = FindOptions { projection: args.get(1).cloned(), ..FindOptions::default() };
            let mut finish: Option<&str> = None;
            for (name, a) in &calls[1..] {
                if finish.is_some() {
                    return Err(Error::Parse(format!(".{name}() cannot follow .count() or .explain()")));
                }
                if matches!(name.as_str(), "count" | "explain") {
                    finish = Some(name);
                    continue;
                }
                let first = a.first().ok_or_else(|| Error::Parse(format!("{name}() needs an argument")))?;
                match name.as_str() {
                    "sort" => opts.sort = Some(first.clone()),
                    "limit" => opts.limit = Some(as_count(first)?),
                    "skip" => opts.skip = as_count(first)?,
                    "project" | "projection" => opts.projection = Some(first.clone()),
                    _ => {
                        return Err(Error::Parse(format!(
                            "unknown modifier .{name}(); use sort, skip, limit, project, count or explain"
                        )))
                    }
                }
            }
            if method == "findone" {
                opts.limit = Some(1);
            }
            match finish {
                Some("count") => Ok(Output::Scalar(Value::Int(doc::count(ctx, coll, arg(0))? as i64))),
                Some(_) => Ok(Output::Scalar(doc::explain(ctx, coll, arg(0))?)),
                None => Ok(Output::Docs(doc::find(ctx, coll, arg(0), &opts)?)),
            }
        }
        "update" | "updatemany" | "updateone" | "replaceone" => {
            let what = "a filter and an update";
            let (filter, upd) = (need(0, what)?, need(1, what)?);
            match method.as_str() {
                "replaceone" if !doc::update::is_replacement(upd)? => {
                    return Err(Error::Parse("replaceOne takes a document without operators".into()))
                }
                "updateone" | "updatemany" if doc::update::is_replacement(upd)? => {
                    return Err(Error::Parse(format!("{method}() takes update operators such as $set; use replaceOne")))
                }
                _ => {}
            }
            // Plain update() changes every match unless given {multi: false}.
            let multi = match method.as_str() {
                "updatemany" => true,
                "update" => args.get(2).and_then(|o| o.get("multi")).map_or(true, |m| *m == Value::Bool(true)),
                _ => false,
            };
            let r = doc::update(ctx, coll, filter, upd, multi, option(2, "upsert"))?;
            let mut msg = format!("{} document(s) matched, {} modified", r.matched, r.modified);
            if let Some(id) = r.upserted_id {
                msg.push_str(&format!(", upserted _id {}", id.to_json()));
            }
            Ok(Output::Done(msg))
        }
        "delete" | "deletemany" | "deleteone" | "remove" => {
            let limit = (method == "deleteone").then_some(1);
            let n = doc::delete(ctx, coll, need(0, "a filter ({} deletes everything)")?, limit)?;
            Ok(Output::Done(format!("{n} document(s) deleted")))
        }
        "bulkwrite" => match need(0, "a list of operations")? {
            Value::Array(ops) => Ok(Output::Scalar(doc::bulk_write(ctx, coll, ops)?)),
            _ => Err(Error::Parse("bulkWrite() takes an array of operations".into())),
        },
        "createindex" => {
            let fields = doc::index_fields(need(0, "a key spec, e.g. createIndex({email: 1})")?)?;
            let unique = option(1, "unique");
            let created = doc::create_index(ctx, coll, fields.clone(), unique)?;
            let label = fields.join(", ");
            Ok(Output::Done(if created {
                format!("{}index created on {coll}({label})", if unique { "unique " } else { "" })
            } else {
                format!("{coll}({label}) is already indexed")
            }))
        }
        "dropindex" => {
            let dropped = doc::drop_index(ctx, coll, need(0, "an index name or key spec")?)?;
            Ok(Output::Done(if dropped { "index dropped".into() } else { "no such index, nothing dropped".into() }))
        }
        "getindexes" => Ok(Output::Docs(doc::list_indexes(ctx, coll)?)),
        "count" | "countdocuments" => Ok(Output::Scalar(Value::Int(doc::count(ctx, coll, arg(0))? as i64))),
        "distinct" => {
            let field = need(0, "a field name")?
                .as_str()
                .ok_or_else(|| Error::Parse("distinct() takes a field name string".into()))?;
            Ok(Output::Scalar(Value::Array(doc::distinct(ctx, coll, field, arg(1))?)))
        }
        "aggregate" => match need(0, "a pipeline array")? {
            Value::Array(stages) => Ok(Output::Docs(doc::aggregate(ctx, coll, stages)?)),
            _ => Err(Error::Parse("aggregate() takes an array of stages".into())),
        },
        "explain" => Ok(Output::Scalar(doc::explain(ctx, coll, arg(0))?)),
        "setschema" => {
            doc::set_schema(ctx, coll, need(0, "a schema (or null to remove it)")?)?;
            Ok(Output::Done(format!("schema of '{coll}' updated")))
        }
        "getschema" => Ok(Output::Scalar(doc::get_schema(ctx, coll)?)),
        "validate" => Ok(Output::Scalar(doc::validate(ctx, coll)?)),
        "reindex" => Ok(Output::Done(format!("{} index entr(y/ies) rebuilt", doc::reindex(ctx, coll)?))),
        "drop" => {
            let existed = doc::drop(ctx, coll)?;
            Ok(Output::Done(if existed {
                format!("collection '{coll}' dropped")
            } else {
                format!("collection '{coll}' does not exist, nothing dropped")
            }))
        }
        other => Err(Error::Parse(format!(
            "unknown collection method '{other}': use insert, find, findOne, update, replaceOne, delete, bulkWrite, count, distinct, aggregate, explain, createIndex, dropIndex, getIndexes, setSchema, getSchema, validate, reindex or drop"
        ))),
    }
}
