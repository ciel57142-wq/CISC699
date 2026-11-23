# polydb reference

Command syntax, the library API and the wire-protocol subsets. For setup and running, see the [README](../README.md); for operations, see the [runbook](RUNBOOK.md). The full language definition (grammar, precedence, types, NULL rules, cross-model queries, EXPLAIN ANALYZE, normalization) is [PBSQL.md](PBSQL.md).

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
SELECT [DISTINCT] items FROM t [alias] | (SELECT ...) alias | DOCUMENT(c) [alias] | KV(['prefix']) [alias]
       [[INNER | LEFT | RIGHT | FULL [OUTER] | CROSS] JOIN t2 [alias] [ON expr] | , t2]...
       [WHERE expr] [GROUP BY exprs] [HAVING expr]
       [UNION | INTERSECT | EXCEPT [ALL] SELECT ...]...
       [ORDER BY expr [ASC|DESC] [NULLS FIRST|LAST], ...] [LIMIT n | ALL] [OFFSET n]
UPDATE t SET col = expr | DEFAULT, ... [WHERE expr]
DELETE FROM t [WHERE expr]
EXPLAIN [ANALYZE] SELECT | INSERT | UPDATE | DELETE ...    ANALYZE [[TABLE] t]    DESCRIBE t    CHECK TABLE t | CHECK TABLES    REINDEX [TABLE t]
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
db.<coll>.insert({...})  |  db.<coll>.insertMany([{...}, {...}])        (a list is all-or-nothing)
db.<coll>.find({filter} [, {projection}]) [.sort(spec)] [.skip(n)] [.limit(n)] [.project({...})] [.count()] [.explain()]
db.<coll>.findOne({filter} [, {projection}])
db.<coll>.update({filter}, {update} [, {upsert: true, multi: false}])  (every match unless multi: false)
db.<coll>.updateOne / updateMany({filter}, {operators} [, {upsert: true}])
db.<coll>.replaceOne({filter}, {document} [, {upsert: true}])
db.<coll>.delete({filter})  |  deleteOne({filter})  |  deleteMany({filter})
db.<coll>.bulkWrite([{insertOne: {document}}, {updateOne|updateMany: {filter, update, upsert}},
                     {replaceOne: {filter, replacement, upsert}}, {deleteOne|deleteMany: {filter}}])
db.<coll>.count({filter})  |  countDocuments({filter})  |  distinct('field' [, {filter}])
db.<coll>.aggregate([stage, ...])
db.<coll>.explain({filter})
db.<coll>.createIndex(spec [, {unique: true}])  |  dropIndex(name | spec)  |  getIndexes()
db.<coll>.setSchema({required: [...], fields: {...}})  |  setSchema(null)  |  getSchema()
db.<coll>.validate()  |  reindex()  |  drop()
```

**Documents.** A document is a JSON object; values may be strings, 64-bit integers, finite doubles, booleans, null, arrays and nested objects, and are returned exactly as stored (`1` and `1.0` keep their types, though they compare equal). `_id` may be an integer, a string or an ObjectId (`{"$oid": "<24 hex>"}`); one is generated if absent and can never change. Rejected on insert and update, with nothing written: a non-object document, field names containing `.` or NUL or starting with `$` (extended-JSON wrappers `$oid $date $numberLong $binary` are values and allowed), NaN or infinite numbers, nesting deeper than 100 levels. Collections are created on first write. Object keys are stored sorted, not in insertion order.

**Field paths.** `a.b.c` walks into objects; a numeric segment (`tags.0`) indexes an array; any other segment applied to an array walks into each object element (`items.sku` reaches the `sku` of every item). A field is *missing* when its path reaches nothing.

**Filters.** `{field: value}` and `$eq` hold when any value the path reaches equals the operand, or is an array with an equal element; equality with `null` also matches a missing field. `$ne` and `$nin` are the exact negations of `$eq` and `$in`. `$gt $gte $lt $lte` compare only within one type class (numbers with numbers, strings with strings, booleans with booleans), so `{age: {$gt: 5}}` never matches a string age; against an array they hold if any element qualifies, and each condition may be met by a different element. `$in $nin $exists $not` (`$not` takes an operator object), `$type` (`null bool int double number string array object objectId date long binData`), `$size`, `$elemMatch` (one element meets every condition), and `$and $or $nor` (non-empty arrays). An unknown operator or malformed argument is an error even on an empty collection.

**Updates.** `$set` (creates intermediate objects; an index past the end of an array pads with nulls), `$unset` (on an array element, sets it to null), `$inc` (numbers only; integer overflow is an error), `$push` and `$addToSet` (with optional `{$each: [...]}`; the target must be an array or missing), `$pull` (a value, an operator condition such as `{$gte: 3}`, or a filter on object elements), and `$setOnInsert` for upserts. Two paths in one update may not overlap. An update with no operators replaces the document and keeps `_id`. An upsert that matches nothing inserts the filter's top-level equality fields with the update applied. A failure on any matched document leaves every document unchanged. Not supported: positional `$` paths, `$rename $min $max $mul`, `$push` modifiers other than `$each`, pipeline updates.

**Sort and projection.** A sort spec is `{field: 1|-1}` (applied in field-name order, because objects do not keep key order) or a list `[['b', -1], ['a', 1]]` to fix the order. Missing sorts as null, and types order null < bool < number < string < array < object; an array sorts by its whole value. Projections include (`{a: 1, 'b.c': 1}`, with `_id` unless `_id: 0`), exclude (`{a: 0}`), or compute (`{name: '$person.name', k: {$literal: 1}}`); inclusion and exclusion cannot be mixed except for `_id`.

**Aggregation.** Stages `$match $project $group $sort $skip $limit $count $unwind`. `$group` takes `_id` (any expression; `null` for one group) and accumulators `$sum $avg $min $max $count $first $last $push $addToSet`; groups are returned in key order. `$sum`/`$avg` ignore non-numbers, `$min`/`$max` ignore missing and null. Expressions are field references (`'$a.b'`), `{$literal: v}`, and objects or arrays of those; other expression operators (`$add`, `$cond`, ...) and other stages (`$lookup`, `$addFields`, ...) are rejected with an error. A leading `$match` uses indexes.

**Indexes.** `createIndex({field: 1})`, nested paths (`{'addr.city': 1}`), compound indexes (`createIndex(['a', 'b'])` or `[['a', 1], ['b', 1]]`; the object form `{a: 1, b: 1}` orders fields by name), and `{unique: true}`. Booleans, numbers and strings are indexed; each value a path reaches is an entry, array elements included. A document without the field has no entry, so a unique index allows any number of documents without it. A compound index cannot hold a document where two of its fields both have several values. Every write maintains the indexes in the same atomic step, and they are stored with the data.

**Access paths** (`explain` reports them, with documents examined and matched): `id_lookup` for `_id` equality or `$in`; `index_scan` for equality or `$in` on a leftmost prefix of an index's fields, optionally followed by a range (`$gt $gte $lt $lte`) on the next field; otherwise `collection_scan`. Conditions under a top-level `$and` count; `$or` branches do not. Range bounds are scanned inclusively, and on a field that has ever held several values in one document only one bound is used, because different elements may meet different bounds. The full filter is always re-checked.

**Schemas.** `setSchema({required: ['name'], fields: {name: 'string', age: ['int', 'null'], 'addr.zip': 'string'}})` makes every insert and update check that required paths are present and that typed paths, when present, hold one of the listed types (the `$type` names plus `any`). Other fields are unconstrained. Setting a schema fails if an existing document violates it; `setSchema(null)` removes it.

**Consistency.** `validate()` checks every document (decodes, is stored under its `_id`, is a valid document, satisfies the schema) and that the index entries are exactly those the documents imply, including uniqueness; `reindex()` rebuilds the entries. All document commands run inside the current transaction, or atomically on their own, and are made durable through the write-ahead log.

- JSON is accepted in strict form or shell form (unquoted keys, single-quoted strings).
- Limits: JSON text may nest 128 levels and a stored document 100 levels, counting the innermost value. An index has at most 16 fields; an `$in` list expands to at most 256 index lookups before the planner uses fewer index fields.

### Key-value

```
SET key value...      GET key      DEL key      EXISTS key
KEYS [prefix]         INCR key [amount]      DECR key [amount]
```

Keys or values containing spaces can be quoted: `SET "my key" "my value"`. Inside quotes, `\n \t \r \0 \\ \' \"` are escapes; an unquoted value is taken verbatim to the end of the line.

In the native language `KEYS` takes a prefix (`KEYS session:`). Over the Redis port it takes a Redis glob pattern instead (`KEYS user:*`), as Redis clients expect.

### Control

```
BEGIN   COMMIT   ROLLBACK   CHECKPOINT   SHOW TABLES   SHOW COLLECTIONS
EXPLAIN [ANALYZE] <SELECT | INSERT | UPDATE | DELETE | db.c.find/count/update/delete | key-value command>
```

### Cross-model queries

```
SELECT ... FROM users u
  JOIN DOCUMENT(orders) o ON o.user = u.id          -- a collection as rows: _id plus its fields
  LEFT JOIN KV('quota:') q ON q.key = 'quota:1'     -- keys with a prefix as rows: key, value
 WHERE o.ship.city = 'Oslo'                         -- dotted names reach into documents
```

See [PBSQL.md](PBSQL.md#cross-model-queries).

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
- **MongoDB:** `insert find update delete count distinct drop createIndexes listIndexes dropIndexes listCollections` and `aggregate`, with the filter, update, index and aggregation subset described under [Document](#document); anything outside it is an error. One database (the database name is ignored), no cursors beyond the first batch, no change streams, no transactions over this port, no authentication. Because documents do not keep key order, a compound index key or multi-field sort sent as a BSON document is applied in field-name order. Error codes: duplicate keys report 11000, schema violations 121, changing `_id` 66. `mongosh` has not been tested.
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
        |      SQL       |    documents    |  key-value  |               sql/, doc/, kv.rs   
        +------------------------------------------------+
        |  transaction overlay + read tracking (Ctx)     |               storage.rs
        |  ordered map  +  write-ahead log  +  snapshot  |
        +------------------------------------------------+
```

- **One keyspace.** Every model maps onto ordered byte keys with its own prefix (`src/keys.rs`): rows under `r\0table\0pk`, documents under `d\0coll\0id`, key-value entries under `k\0key`, index entries under `i\0...`. Names cannot contain `\0`, so a table, a collection and a key with the same name never collide.
- **Transactions.** Statements write into an in-memory overlay. Commit validates the transaction's reads against commits made since it began, then logs it as `BEGIN`, one record per changed key and `COMMIT`, each with a log sequence number and a checksum, fsyncs the group, and applies it. Recovery loads the latest checkpoint, redoes transactions whose `COMMIT` is intact, rolls back (and logs an `ABORT` for) any without one, and truncates a torn tail, so every commit is all-or-nothing across models. A statement that fails halfway leaves nothing behind, inside or outside `BEGIN`.
- **Checkpoints.** `CHECKPOINT` (or a log over 64 MB) writes a full snapshot to a temp file, renames it into place, and empties the log. Any crash point in that sequence recovers correctly.
- **Files.** A database directory holds `polydb.format` (format version), `LOCK` (one process at a time), `data.wal` and, after the first checkpoint, `data.snap`. A new database is only created in a missing or empty directory; an initialised one with missing files is refused, never reopened as empty. `CHECKPOINT` is never needed for durability. See [STORAGE.md](STORAGE.md).
