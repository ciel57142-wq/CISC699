# polydb reference

Command syntax, the library API and the wire-protocol subsets. For setup and running, see the [README](../README.md); for operations, see the [runbook](RUNBOOK.md).

## Command reference

### Relational

```
CREATE TABLE [IF NOT EXISTS] t (
    col TYPE [PRIMARY KEY] [NOT NULL] [UNIQUE] [DEFAULT const] [CHECK (expr)]
             [REFERENCES p [(col)] [ON DELETE action] [ON UPDATE action]] [AUTOINCREMENT],
    ...,
    [CONSTRAINT name] PRIMARY KEY (a, b) | UNIQUE (a, b) | CHECK (expr)
                    | FOREIGN KEY (a, b) REFERENCES p (x, y) [ON DELETE action] [ON UPDATE action])
DROP TABLE [IF EXISTS] t [CASCADE | RESTRICT]
ALTER TABLE t action [, action ...]
    ADD [COLUMN] coldef | DROP [COLUMN] [IF EXISTS] c | RENAME [COLUMN] a TO b | RENAME TO t2
    ALTER [COLUMN] c SET DEFAULT const | DROP DEFAULT | SET NOT NULL | DROP NOT NULL | [SET DATA] TYPE type
    ADD [CONSTRAINT name] UNIQUE (..) | CHECK (..) | FOREIGN KEY (..) REFERENCES ..
    DROP CONSTRAINT [IF EXISTS] name
TRUNCATE [TABLE] t
CREATE [UNIQUE] INDEX [IF NOT EXISTS] name ON t (col [, col ...])
DROP INDEX [IF EXISTS] name [ON t]
INSERT INTO t [(cols)] VALUES (... | DEFAULT), ...  |  INSERT INTO t [(cols)] SELECT ...  |  INSERT INTO t DEFAULT VALUES
SELECT [DISTINCT] items FROM t [alias] | (SELECT ...) alias
       [[INNER | LEFT | RIGHT | FULL [OUTER] | CROSS] JOIN t2 [alias] [ON expr] | , t2]...
       [WHERE expr] [GROUP BY exprs] [HAVING expr]
       [UNION | INTERSECT | EXCEPT [ALL] SELECT ...]...
       [ORDER BY expr [ASC|DESC] [NULLS FIRST|LAST], ...] [LIMIT n | ALL] [OFFSET n]
UPDATE t SET col = expr | DEFAULT, ... [WHERE expr]
DELETE FROM t [WHERE expr]
EXPLAIN SELECT ...        DESCRIBE t        CHECK TABLE t | CHECK TABLES        REINDEX [TABLE t]
```

- Types: `INT`, `REAL`, `TEXT`, `BOOL` (aliases such as `INTEGER`, `DOUBLE`, `BOOLEAN` are accepted). `VARCHAR(n)` / `CHAR(n)` enforce a maximum length in characters. `SERIAL` is `INT NOT NULL AUTOINCREMENT`. Values are checked strictly: a whole `REAL` is accepted for `INT`, anything else of the wrong type is a type error; non-finite numbers are refused.
- Constraints: primary keys (one or several `INT`/`TEXT` columns), `NOT NULL`, `UNIQUE` (NULLs never conflict), `CHECK` (NULL passes), constant `DEFAULT`s, foreign keys with `RESTRICT`/`NO ACTION` (both refuse immediately), `CASCADE` and `SET NULL`. A foreign key must reference the primary key or a unique column set of the same types. Constraints are checked against the statement's result, so `UPDATE t SET id = id + 1` and swapping unique values work.
- Atomicity: a statement that fails (constraint, type, cascade, `ALTER`) leaves no change behind, inside or outside a transaction.
- Expressions: `+ - * / %`, `||`, comparisons, `AND OR NOT`, `IS [NOT] NULL`, `[NOT] LIKE`, `[NOT] IN (list | SELECT)`, `[NOT] BETWEEN`, `EXISTS (SELECT)`, scalar `(SELECT)`, `CASE`, `CAST(x AS type)` and `x::type`, with SQL three-valued NULL logic. Subqueries may be correlated; uncorrelated ones run once per statement.
- Aggregates: `COUNT(*)`, `COUNT/SUM/AVG/MIN/MAX([DISTINCT] x)`. Functions: `LOWER UPPER LENGTH TRIM LTRIM RTRIM SUBSTR REPLACE CONCAT ABS ROUND FLOOR CEIL COALESCE IFNULL NULLIF GREATEST LEAST TYPEOF`.
- `GROUP BY` and `ORDER BY` accept select aliases and 1-based positions. Columns not in `GROUP BY` take the value of the group's first row. Set operations evaluate left to right; their `ORDER BY` names output columns. NULLs sort first unless `NULLS LAST` is given.
- Limits: an expression may nest 32 levels (parentheses, `NOT`, unary minus, function calls, subqueries) and hold 100 chained operators and nested sub-expressions in total; each clause of a subquery starts from what its enclosing expression has used. Subqueries nest at most 6 deep. A foreign-key cascade stops with an error after 100,000 steps.
- Unquoted names are case-insensitive; `"Quoted"` names keep their case (`""` inside is a quote). Strings use single quotes.
- Access paths (shown by `EXPLAIN`): primary-key lookup, primary-key `IN` list, primary-key or composite-key prefix and range scans, index equality on a leftmost prefix of an index's columns plus a range on the next one, otherwise a full scan. Equality joins use a hash join, other joins a nested loop. Every candidate row is still checked against the full condition.
- Indexes are kept up to date by every write and stored with the data, so they survive restarts. `CHECK TABLE` compares every index entry with the rows and also verifies types, primary-key placement, `CHECK` constraints and foreign keys; `REINDEX` rebuilds index entries from the rows.

### Document

```
db.<coll>.insert({...})  |  db.<coll>.insertMany([{...}, {...}])   (a batch is one atomic statement)
db.<coll>.find({filter} [, {projection}]) [.sort({field: 1|-1})] [.skip(n)] [.limit(n)] [.explain()]
db.<coll>.findOne({filter} [, {projection}])
db.<coll>.aggregate([{stage}, ...])
db.<coll>.update({filter}, {update})       (updateOne changes only the first match)
db.<coll>.delete({filter})                 (deleteOne removes only the first match)
db.<coll>.createIndex({field: 1})
db.<coll>.count({filter})
db.<coll>.validate()                       (documents and index entries agree?)
db.<coll>.reIndex()                        (rebuild every index of the collection)
db.<coll>.drop()
```

- Collections are created on first insert. `_id` may be an integer or string; one is generated if absent.
- Filters: equality, dotted paths (`"addr.city"`), `$eq $ne $gt $gte $lt $lte $in $nin $exists $not`, `$and $or $nor`. Equality against an array field matches any element.
- Updates: `$set`, `$unset`, `$inc`, `$push`, `$addToSet`, `$pull`; an update with no operators replaces the document.
- JSON is accepted in strict form or shell form (unquoted keys, single-quoted strings).
- Limits: JSON text may nest 128 levels and a stored document 100 levels, counting the innermost value.
- Projection: top-level fields, `{a: 1, b: 1}` keeps those plus `_id` (`_id: 0` drops it), `{a: 0}` drops `a`. Only projected fields are decoded.
- Sort specs are JSON objects, whose keys are kept sorted, so a multi-key sort applies its keys in alphabetical order (`{b: 1, a: -1}` sorts by `a` first).
- Aggregation stages: `$match $sort $skip $limit $project $addFields/$set $group $count $unwind`. `$group` takes `_id: null | <literal> | "$path" | {name: "$path", ...}` and the accumulators `$sum $avg $min $max $first $last $push $addToSet $count`. Leading `$match`/`$sort`/`$skip`/`$limit` stages run as one indexed query; `$group` and `$count` read only the fields they use.
- Access paths (shown by `.explain()`): `_id` equality or `$in` is a direct lookup and an `_id` range a key-range scan. An index serves equality, `$in`, ranges (`$gt $gte $lt $lte`, one type at a time), `null` / `$exists: false`, and a `sort` on the indexed field (when every document has a scalar there). Everything else scans the collection. Every candidate is still checked against the full filter, except where the lookup is exact (an integer/string `_id`, or a lone text/bool equality on an index).
- Storage: documents are stored in a compact binary layout (see `src/doc/codec.rs`) that lets filters, sort keys and index maintenance read single fields without decoding the document. Databases written by 0.2 (JSON documents) are read as they are; a document is rewritten in the binary layout when it is updated. Indexes created by 0.2 work as before; `reIndex()` adds the entries that let them also serve `null` queries and sorts.

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

- **Redis:** strings only: `GET SET MGET MSET DEL EXISTS INCR DECR INCRBY DECRBY KEYS DBSIZE STRLEN APPEND GETSET SETNX TTL PING ECHO`. No expiry, lists, hashes, sets, pub/sub or `MULTI`.
- **PostgreSQL:** the simple-query flow that psql, libpq and psycopg2 use. No server-side prepared statements (the extended protocol used by default in JDBC, asyncpg, node-postgres with parameters), no `COPY`, no `pg_catalog`, and the SQL is the subset listed below. Every polydb command works on this port, including `db.coll.find(...)` and `GET key`.
- **MongoDB:** `insert find update delete count distinct drop createIndexes listCollections` and `aggregate` with the stages listed under [Document](#document). One database (the database name is ignored), no cursors beyond the first batch, no change streams, no expression operators beyond field paths and `$literal`. `mongosh` has not been tested.
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

Equality filters on an indexed column or field (`WHERE city = 'London'`, `{city: 'London'}`, also combined with other conditions by `AND`) read only the matching entries instead of scanning. Indexes on array fields index each element. Primary keys and `_id` are always direct lookups. SQL range scans are described under [Relational](#relational) (`EXPLAIN` shows the path); document indexes also serve `$in`, ranges, `null` and sorts (`.explain()` shows the path). `LIKE` scans.

## How it works

```
  native :7878    Redis :6380    PostgreSQL :5433    MongoDB :27018      server.rs, resp.rs,
       \              |                |                 /               pgwire.rs, mongo.rs
        +---------- one session per connection ---------+               lib.rs
        |      SQL       |    documents    |  key-value  |               sql/, doc/, kv.rs    
        +------------------------------------------------+
        |  transaction overlay + read tracking (Ctx)     |               storage.rs
        |  ordered map  +  write-ahead log  +  snapshot  |
        +------------------------------------------------+
```

- **One keyspace.** Every model maps onto ordered byte keys with its own prefix (`src/keys.rs`): rows under `r\0table\0pk`, documents under `d\0coll\0id`, key-value entries under `k\0key`, index entries under `i\0...`. Names cannot contain `\0`, so a table, a collection and a key with the same name never collide.
- **Transactions.** Statements write into an in-memory overlay. Commit validates the transaction's reads against commits made since it began, then serialises the whole overlay as one checksummed log record, fsyncs it, and applies it. Recovery replays intact records and truncates a torn tail, so every commit is all-or-nothing across models. A statement that fails halfway leaves nothing behind, inside or outside `BEGIN`.
- **Checkpoints.** `CHECKPOINT` (or a log over 64 MB) writes a full snapshot to a temp file, renames it into place, and empties the log. Any crash point in that sequence recovers correctly.
- **Files.** A database directory holds `data.wal` and, after the first checkpoint, `data.snap`.
