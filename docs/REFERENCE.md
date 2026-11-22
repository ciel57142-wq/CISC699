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
EXPLAIN [ANALYZE] SELECT ...    ANALYZE [[TABLE] t]    DESCRIBE t    CHECK TABLE t | CHECK TABLES    REINDEX [TABLE t]
```

- Types: `INT`, `REAL`, `TEXT`, `BOOL` (aliases such as `INTEGER`, `DOUBLE`, `BOOLEAN` are accepted). `VARCHAR(n)` / `CHAR(n)` enforce a maximum length in characters. `SERIAL` is `INT NOT NULL AUTOINCREMENT`. Values are checked strictly: a whole `REAL` is accepted for `INT`, anything else of the wrong type is a type error; non-finite numbers are refused.
- Constraints: primary keys (one or several `INT`/`TEXT` columns), `NOT NULL`, `UNIQUE` (NULLs never conflict), `CHECK` (NULL passes), constant `DEFAULT`s, foreign keys with `RESTRICT`/`NO ACTION` (both refuse immediately), `CASCADE` and `SET NULL`. A foreign key must reference the primary key or a unique column set of the same types. Constraints are checked against the statement's result, so `UPDATE t SET id = id + 1` and swapping unique values work.
- Atomicity: a statement that fails (constraint, type, cascade, `ALTER`) leaves no change behind, inside or outside a transaction.
- Expressions: `+ - * / %`, `||`, comparisons, `AND OR NOT`, `IS [NOT] NULL`, `[NOT] LIKE`, `[NOT] IN (list | SELECT)`, `[NOT] BETWEEN`, `EXISTS (SELECT)`, scalar `(SELECT)`, `CASE`, `CAST(x AS type)` and `x::type`, with SQL three-valued NULL logic. Subqueries may be correlated; uncorrelated ones run once per statement.
- Aggregates: `COUNT(*)`, `COUNT/SUM/AVG/MIN/MAX([DISTINCT] x)`. Functions: `LOWER UPPER LENGTH TRIM LTRIM RTRIM SUBSTR REPLACE CONCAT ABS ROUND FLOOR CEIL COALESCE IFNULL NULLIF GREATEST LEAST TYPEOF`.
- `GROUP BY` and `ORDER BY` accept select aliases and 1-based positions. Columns not in `GROUP BY` take the value of the group's first row. Set operations evaluate left to right; their `ORDER BY` names output columns. NULLs sort first unless `NULLS LAST` is given.
- Limits: an expression may nest 32 levels (parentheses, `NOT`, unary minus, function calls, subqueries) and hold 100 chained operators and nested sub-expressions in total; each clause of a subquery starts from what its enclosing expression has used. Subqueries nest at most 6 deep. A foreign-key cascade stops with an error after 100,000 steps.
- Unquoted names are case-insensitive; `"Quoted"` names keep their case (`""` inside is a quote). Strings use single quotes.
- Access paths (chosen by cost, shown by `EXPLAIN`): `Primary Key Lookup`, `Primary Key IN Lookup`, `Primary Key Range Scan`, `Primary Key Prefix Scan` (composite keys), `Index Scan` (equality on a leftmost prefix of an index's columns, plus an optional range on the next column), `Index Range Scan`, `Index IN Scan` (an `IN` list on an index column), `Seq Scan`. Conditions may compare a column with a constant or, inside a correlated subquery, with a column of the enclosing query. Every candidate row is still checked against the full condition.
- Pushdown and joins: each table's own conditions filter it while it is read, except on the side an outer join NULL-extends. Queries whose joins are all `INNER`/`CROSS` are reordered (greedy, smallest estimated result first; output columns keep their written order); outer joins keep their written order. Each join is an `Index Nested Loop` (a key or index lookup per outer row) when that is cheaper than reading the inner table, a `Hash Join` on equality conditions otherwise, or a `Nested Loop` when there is no equality.
- Statistics: `ANALYZE [t]` stores per-table row counts and per-column distinct counts, NULL counts, most common values and an equi-depth histogram, used for selectivity and cardinality estimates. Without them fixed defaults apply. `ALTER TABLE`, `TRUNCATE` and `DROP TABLE` discard them; they never affect results and never cause transaction conflicts.
- `EXPLAIN` prints the operator tree with estimated rows (it runs the query to discover subquery plans, but prints no measurements). `EXPLAIN ANALYZE` adds, per operator, the measured rows (total over all loops), loops, time (the operator's own work, excluding its inputs), rows examined and index entries read, and the statement's execution time. Details: [INDEXING.md](INDEXING.md).
- Indexes are kept up to date by every write and stored with the data, so they survive restarts. `CHECK TABLE` compares every index entry with the rows and also verifies types, primary-key placement, `CHECK` constraints and foreign keys; `REINDEX` rebuilds index entries from the rows. `Database::open_verified` (and `polydb --verify-indexes`) runs that comparison for every table when a database is opened and rebuilds the indexes of any table that disagrees.

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

- **Redis:** strings only: `GET SET MGET MSET DEL EXISTS INCR DECR INCRBY DECRBY KEYS DBSIZE STRLEN APPEND GETSET SETNX TTL PING ECHO`. No expiry, lists, hashes, sets, pub/sub or `MULTI`.
- **PostgreSQL:** the simple-query flow that psql, libpq and psycopg2 use. No server-side prepared statements (the extended protocol used by default in JDBC, asyncpg, node-postgres with parameters), no `COPY`, no `pg_catalog`, and the SQL is the subset listed below. Every polydb command works on this port, including `db.coll.find(...)` and `GET key`.
- **MongoDB:** `insert find update delete count distinct drop createIndexes listCollections` and `aggregate` with `$match $sort $skip $limit $count`. One database (the database name is ignored), no cursors beyond the first batch, no change streams, no general `$group`. `mongosh` has not been tested.
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
        |      SQL       |    documents    |  key-value  |               sql/, doc.rs, kv.rs  
        +------------------------------------------------+
        |  transaction overlay + read tracking (Ctx)     |               storage.rs
        |  ordered map  +  write-ahead log  +  snapshot  |
        +------------------------------------------------+
```

- **One keyspace.** Every model maps onto ordered byte keys with its own prefix (`src/keys.rs`): rows under `r\0table\0pk`, documents under `d\0coll\0id`, key-value entries under `k\0key`, index entries under `i\0...`. Names cannot contain `\0`, so a table, a collection and a key with the same name never collide.
- **Transactions.** Statements write into an in-memory overlay. Commit validates the transaction's reads against commits made since it began, then serialises the whole overlay as one checksummed log record, fsyncs it, and applies it. Recovery replays intact records and truncates a torn tail, so every commit is all-or-nothing across models. A statement that fails halfway leaves nothing behind, inside or outside `BEGIN`.
- **Checkpoints.** `CHECKPOINT` (or a log over 64 MB) writes a full snapshot to a temp file, renames it into place, and empties the log. Any crash point in that sequence recovers correctly.
- **Files.** A database directory holds `data.wal` and, after the first checkpoint, `data.snap`.
