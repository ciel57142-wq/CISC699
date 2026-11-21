# polydb reference

Command syntax, the library API and the wire-protocol subsets. For setup and running, see the [README](../README.md); for operations, see the [runbook](RUNBOOK.md).

## Command reference

### Relational

```
CREATE TABLE [IF NOT EXISTS] t (
    col TYPE [PRIMARY KEY] [NOT NULL] [UNIQUE] [DEFAULT expr] [REFERENCES p [(col)] [ON DELETE CASCADE|RESTRICT|SET NULL]],
    ..., [PRIMARY KEY (a, b)], [UNIQUE (a, b)], [FOREIGN KEY (a) REFERENCES p (x) [ON DELETE ...]])
DROP TABLE [IF EXISTS] t
ALTER TABLE t ADD [COLUMN] col TYPE [constraints]  |  DROP [COLUMN] col
ALTER TABLE t RENAME [COLUMN] a TO b               |  RENAME TO new_name
CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON t (col [, col ...])
DROP INDEX name ON t
INSERT INTO t [(cols)] VALUES (..., DEFAULT, ...), (...)  |  INSERT INTO t [(cols)] SELECT ...
SELECT [DISTINCT] items FROM source [alias]
       [[INNER | LEFT | CROSS] JOIN source [alias] [ON expr]]...   (also FROM a, b)
       [WHERE expr] [GROUP BY exprs] [HAVING expr]
       [UNION [ALL] SELECT ...]...
       [ORDER BY expr [ASC|DESC], ...] [LIMIT n] [OFFSET n]
UPDATE t SET col = expr, ... [WHERE expr]
DELETE FROM t [WHERE expr]
EXPLAIN [ANALYZE] statement
```

- Types: `INT`, `REAL`, `TEXT`, `BOOL`, `DATE`, `TIME`, `TIMESTAMP` (`DATETIME`), `JSON` (common aliases such as `INTEGER`, `VARCHAR(n)`, `DOUBLE`, `BOOLEAN` are accepted). Dates and times are stored as canonical ISO text (`2024-02-29`, `09:05:00`, `2024-02-29 10:00:00`), validated on write.
- Subqueries: `x [NOT] IN (SELECT ...)`, `[NOT] EXISTS (SELECT ...)`, scalar `(SELECT ...)`, correlated references to outer queries, and derived tables `FROM (SELECT ...) AS s`.
- Constraints are enforced on INSERT and UPDATE, inside transactions, and persist; foreign keys must reference a primary key or UNIQUE column set; ON UPDATE of a referenced key is RESTRICT.
- More functions: `CONCAT`, `SUBSTR`, `TRIM`, `ROUND`, `NULLIF`, `CAST(x AS type)`, `CASE`, `NOW()`/`CURRENT_TIMESTAMP`/`CURRENT_DATE`, `YEAR MONTH DAY HOUR MINUTE SECOND`, `EXTRACT(field FROM x)`, `COUNT(DISTINCT x)`.
- Expressions: `+ - * / %`, `||`, `= != <> < <= > >=`, `AND OR NOT`, `IS [NOT] NULL`, `[NOT] LIKE`, `[NOT] IN (...)`, `[NOT] BETWEEN`, with SQL three-valued NULL logic.
- Aggregates: `COUNT(*)`, `COUNT(x)`, `SUM`, `AVG`, `MIN`, `MAX`. Functions: `LOWER`, `UPPER`, `LENGTH`, `ABS`, `COALESCE`.
- Limits: an expression may nest 32 levels (parentheses, `NOT`, unary minus, function calls) and hold 100 chained operators and nested sub-expressions in total. Items of an `IN` list or of a function's arguments do not add up. Each row of `VALUES` and each `SELECT` item is its own expression.
- Unquoted names are case-insensitive; `"Quoted"` names keep their case. Strings use single quotes.
- Indexes are ordered: equality, `IN`, `<`/`<=`/`>`/`>=`/`BETWEEN` and `LIKE 'prefix%'` on an indexed column (or the leading columns of a composite index, with a range on the next one) are index scans, as are primary-key lookups and ranges. Joins use an index lookup on the inner side when one fits, otherwise a hash join (equality) or a nested loop. `EXPLAIN` shows the chosen plan; `EXPLAIN ANALYZE` runs it and reports rows, loops, rows examined and time per operator.

### PBSQL (all models)

PBSQL is the SQL above plus model-qualified sources: `RELATIONAL.t` (same as `t`), `DOCUMENT.coll` (or `DOCUMENT.db.coll`), `KV` and `KV.namespace`. A document source exposes its fields as dotted paths (`p.profile.address.city`; missing fields are NULL); a KV source has the columns `key` and `value` (a JSON value can be looked into: `value.name`). Any sources can be joined.

```
SELECT u.name, p.profile_data, s.value
FROM RELATIONAL.users AS u
JOIN DOCUMENT.profiles AS p ON p.user_id = u.id
LEFT JOIN KV.user_sessions AS s ON s.key = CONCAT('session:', u.id)
INSERT INTO DOCUMENT.users VALUES {"name": "Alice", "age": 30}, {...}
INSERT INTO DOCUMENT.users (name, age) VALUES ('Bob', 25)
UPDATE DOCUMENT.users SET profile.city = 'Paris' WHERE name = 'Alice'
DELETE FROM DOCUMENT.users WHERE age < 18
CREATE INDEX users_age ON DOCUMENT.users (age)
SET KV[.ns] 'key' = value [TTL seconds]   GET FROM KV[.ns] 'key'        EXISTS KV[.ns] 'key'
INCREMENT KV[.ns] 'key' [BY n]   SELECT key, value FROM KV WHERE key LIKE 'user:%'
DELETE FROM KV WHERE key = 'k'   INSERT INTO KV (key, value) VALUES ('k', 'v')
```

PBSQL statements join the session's transaction, so one `BEGIN ... COMMIT` can change all three models atomically. Ordering comparisons between values of different types (a number and a text) are NULL.

### Document

```
db.<coll>.insert({...})  |  db.<coll>.insert([{...}, {...}])
db.<coll>.find({filter}) [.sort({field: 1|-1})] [.skip(n)] [.limit(n)]
db.<coll>.findOne({filter})
db.<coll>.update({filter}, {update})       (updateOne changes only the first match)
db.<coll>.delete({filter})                 (deleteOne removes only the first match)
db.<coll>.createIndex({field: 1})
db.<coll>.count({filter})
db.<coll>.drop()
```

- Collections are created on first insert. `_id` may be an integer or string; one is generated if absent.
- Filters: equality, dotted paths (`"addr.city"`), `$eq $ne $gt $gte $lt $lte $in $nin $exists $not`, `$and $or $nor`. Equality against an array field matches any element.
- Updates: `$set`, `$unset`, `$inc`, `$push`, `$addToSet`, `$pull`; an update with no operators replaces the document.
- JSON is accepted in strict form or shell form (unquoted keys, single-quoted strings).
- Limits: JSON text may nest 128 levels and a stored document 100 levels, counting the innermost value.
- A filter on `_id` is a direct lookup and equality on an indexed field an index lookup; everything else scans the collection.

### Key-value

```
SET key value...      GET key      DEL key      EXISTS key
KEYS [prefix]         INCR key [amount]
```

Keys or values containing spaces can be quoted: `SET "my key" "my value"`.

In the native language `KEYS` takes a prefix (`KEYS session:`). Over the Redis port it takes a Redis glob pattern instead (`KEYS user:*`), as Redis clients expect.

### Control

```
BEGIN   COMMIT   ROLLBACK   CHECKPOINT   SHOW TABLES   SHOW COLLECTIONS
```

## Use as a library

```rust
use polydb::{Database, Output, Value, FindOptions};

let mut db = Database::open("mydata")?;

// one entry point understands all three languages
db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL)")?;
db.execute("INSERT INTO users VALUES (1, 'Ada')")?;
db.execute("db.events.insert({user: 1, type: 'signup'})")?;
db.execute("SET last_user 1")?;

// or use the typed API
db.kv_set("blob", &[0, 1, 2])?;                       // values are binary-safe
let id = db.doc_insert("events", Value::parse(r#"{"user": 1}"#)?)?;
let hits = db.doc_find("events", &Value::parse(r#"{"user": 1}"#)?, &FindOptions::default())?;

// a transaction can span all three models
db.begin()?;
db.sql("UPDATE users SET name = 'Ada L.' WHERE id = 1")?;
db.doc_insert("audit", Value::parse(r#"{"what": "rename"}"#)?)?;
db.kv_incr("renames", 1)?;
db.commit()?;                                         // all or nothing
```

A `Database` value is one session. Call `db.session()` to get another session on the same data and move it to another thread; sessions are `Send`. To serve the network protocols from your own program:

```rust
let db = Database::open("mydata")?;
polydb::server::serve(&db, &polydb::server::Config::default())?;   // blocks
```

## Native protocol replies

The native protocol replies look like this:

```
> SELECT id, name FROM users
{"columns":["id","name"],"ok":true,"rows":[[1,"Ada"]],"type":"rows"}
> SELEKT
{"error":"parse error: unknown command 'selekt': ...","kind":"parse","ok":false}
```

`type` is `rows`, `docs` (with `docs`), `value` (with `value`) or `done` (with `message`).

## Wire-protocol subsets

**What "compatible" means here.** The wire protocols are real, so drivers connect, authenticate nothing, and exchange data in their native formats. The *languages* are polydb's subsets, not the full products:

- **Redis:** strings only: `GET SET MGET MSET DEL EXISTS INCR DECR INCRBY DECRBY KEYS DBSIZE STRLEN APPEND GETSET SETNX TTL PTTL EXPIRE PEXPIRE PERSIST PING ECHO`, and `SET` with `EX PX NX XX KEEPTTL`. Expired keys are invisible to every front end. No lists, hashes, sets, pub/sub or `MULTI`.
- **PostgreSQL:** the simple-query flow (psql, libpq, psycopg2) and the extended protocol: Parse/Bind/Describe/Execute/Close/Sync, `$n` parameters in text or binary format (untyped parameters take the type of the column they meet), binary results, row-limited Execute, and CancelRequest. Tested with psycopg 3 and pg8000. No `COPY`, no `pg_catalog`, and the SQL is the subset listed below. Every polydb command works on this port, including `db.coll.find(...)` and `GET key`.
- **MongoDB:** `insert find getMore killCursors update delete count distinct drop dropDatabase createIndexes listCollections listDatabases` and `aggregate` with `$match $project $addFields $set $unset $sort $skip $limit $group $unwind $count $lookup $replaceRoot` (accumulators `$sum $avg $min $max $count $first $last $push $addToSet`). Results come in batches through server-side cursors (idle cursors expire after 10 minutes). Compound, unique and nested-field indexes (`createIndexes`, `listIndexes`, `dropIndexes`) are used for equality, ranges and index-ordered sorts. Databases are isolated; the default database (`--mongo-default-db`, `test` unless set) holds the collections the native `db.<coll>` API sees, and database `d` is visible natively as `db.d.<coll>` / `DOCUMENT.d.coll`. No change streams. `mongosh` has not been tested.
- **No authentication or TLS on any port.** The server binds to 127.0.0.1 by default; only expose it on a network you trust.

## Concurrency and correctness

- Any number of clients at once. Reads run in parallel under a shared lock; commits are applied one at a time.
- **Serializable isolation.** A single statement is atomic. A `BEGIN ... COMMIT` transaction buffers its writes and records everything it read (rows, and whole ranges it scanned); if another client committed a change to any of that in the meantime, `COMMIT` fails with a conflict error and writes nothing, so the application retries. Over PostgreSQL this is SQLSTATE `40001`, over MongoDB `WriteConflict`. Lost updates, dirty reads and phantoms cannot occur.
- **Every commit is on disk before it is acknowledged** (checksummed write-ahead log, fsync per commit). Nothing acknowledged is lost on a crash or power failure.

## Indexes (for read-heavy use)

```
CREATE INDEX users_city ON users (city)        DROP INDEX users_city ON users
db.people.createIndex({city: 1})
```

Equality filters on an indexed column or field (`WHERE city = 'London'`, `{city: 'London'}`, also combined with other conditions by `AND`) read only the matching entries instead of scanning. Indexes on array fields index each element. Primary keys and `_id` are always direct lookups. Range conditions (`>`, `<`, `LIKE`) still scan.

## How it works

```
  native :7878    Redis :6380    PostgreSQL :5433    MongoDB :27018      server.rs, resp.rs,
       \              |                |                 /               pgwire.rs, mongo.rs
        +---------- one session per connection ---------+               lib.rs
        |      SQL       |    documents    |  key-value  |               sql.rs, doc.rs, kv.rs
        +------------------------------------------------+
        |  transaction overlay + read tracking (Ctx)     |               storage.rs
        |  ordered map  +  write-ahead log  +  snapshot  |
        +------------------------------------------------+
```

- **One keyspace.** Every model maps onto ordered byte keys with its own prefix (`src/keys.rs`): rows under `r\0table\0pk`, documents under `d\0coll\0id`, key-value entries under `k\0key`, index entries under `i\0...`. Names cannot contain `\0`, so a table, a collection and a key with the same name never collide.
- **Transactions.** Statements write into an in-memory overlay. Commit validates the transaction's reads against commits made since it began, then serialises the whole overlay as one checksummed log record, fsyncs it, and applies it. Recovery replays intact records and truncates a torn tail, so every commit is all-or-nothing across models. A statement that fails halfway leaves nothing behind, inside or outside `BEGIN`.
- **Checkpoints.** `CHECKPOINT` (or a log over 64 MB) writes a full snapshot to a temp file, renames it into place, and empties the log. Any crash point in that sequence recovers correctly.
- **Files.** A database directory holds `data.wal` and, after the first checkpoint, `data.snap`.
