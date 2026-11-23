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
pub mod pbsql;
pub mod pgwire;
pub mod resp;
pub mod server;
pub(crate) mod sql;
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

    /// Run one relational PBSQL statement (SELECT, INSERT, UPDATE, DELETE,
    /// DDL, EXPLAIN of those). Other kinds of statement are refused; use
    /// [`Database::execute`] for those.
    pub fn sql(&mut self, statement: &str) -> Result<Output> {
        let stmt = pbsql::parse(statement)?;
        let relational = match &stmt {
            pbsql::ast::Statement::Sql(_) => true,
            pbsql::ast::Statement::Explain { stmt, .. } => matches!(**stmt, pbsql::ast::Statement::Sql(_)),
            _ => false,
        };
        if !relational {
            return Err(Error::Parse(format!(
                "sql() runs relational statements; this is a {} statement, use execute()",
                stmt.kind()
            )));
        }
        self.execute_statement(stmt)
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

    pub fn doc_find(&mut self, collection: &str, filter: &Value, opts: &FindOptions) -> Result<Vec<Value>> {
        self.run(false, |ctx| doc::find(ctx, collection, filter, opts))
    }

    /// Update every matching document; returns how many.
    pub fn doc_update(&mut self, collection: &str, filter: &Value, update: &Value) -> Result<u64> {
        self.doc_update_with(collection, filter, update, true, false).map(|r| r.0)
    }

    /// Update with control over `multi` (all matches vs first) and `upsert`.
    /// Returns (documents updated, id of an upserted document).
    pub fn doc_update_with(
        &mut self,
        collection: &str,
        filter: &Value,
        update: &Value,
        multi: bool,
        upsert: bool,
    ) -> Result<(u64, Option<Value>)> {
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

    pub fn doc_drop(&mut self, collection: &str) -> Result<bool> {
        self.run(true, |ctx| doc::drop(ctx, collection))
    }

    pub fn doc_collections(&mut self) -> Result<Vec<String>> {
        self.run(false, |ctx| Ok(doc::collections(ctx)))
    }

    /// Index a field of a collection; returns false if it was already indexed.
    pub fn doc_create_index(&mut self, collection: &str, field: &str) -> Result<bool> {
        self.run(true, |ctx| doc::create_index(ctx, collection, field))
    }

    // ---------------------------------------------------- unified commands

    /// Run one PBSQL statement (see `docs/PBSQL.md`):
    ///
    /// * relational: `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE TABLE|[UNIQUE] INDEX`,
    ///   `DROP TABLE|INDEX`, `ALTER TABLE`, `TRUNCATE`, `DESCRIBE`, `CHECK TABLE`, `REINDEX`;
    ///   a FROM clause may also read `DOCUMENT(collection)` and `KV('prefix')`
    /// * documents: `db.<collection>.<method>(...)`
    /// * key-value: `SET`, `GET`, `DEL`, `EXISTS`, `KEYS`, `INCR`, `DECR`
    /// * control: `BEGIN`, `COMMIT`, `ROLLBACK`, `EXPLAIN [ANALYZE]`, `CHECKPOINT`,
    ///   `SHOW TABLES`, `SHOW COLLECTIONS`
    ///
    /// The statement is lexed, parsed and semantically checked before it
    /// touches any data; then it runs as one atomic unit (or inside the
    /// session's open transaction).
    pub fn execute(&mut self, command: &str) -> Result<Output> {
        let stmt = pbsql::parse(command)?;
        self.execute_statement(stmt)
    }

    fn execute_statement(&mut self, stmt: pbsql::ast::Statement) -> Result<Output> {
        use pbsql::ast::{Statement, TxnOp};
        match stmt {
            Statement::Txn(TxnOp::Begin) => self.begin().map(|_| Output::Done("transaction started".into())),
            Statement::Txn(TxnOp::Commit) => self.commit().map(|_| Output::Done("committed".into())),
            Statement::Txn(TxnOp::Rollback) => self.rollback().map(|_| Output::Done("rolled back".into())),
            Statement::Checkpoint => self.checkpoint().map(|_| Output::Done("checkpoint written".into())),
            stmt => {
                let read = stmt.is_read_only();
                self.run(!read, |ctx| pbsql::exec::execute(ctx, stmt))
            }
        }
    }

    /// Check a statement against this database's catalog without running it.
    pub fn validate(&mut self, command: &str) -> Result<()> {
        let stmt = pbsql::parse(command)?;
        self.run(false, |ctx| pbsql::check(&stmt, Some(ctx)))
    }
}
