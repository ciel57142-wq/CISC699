# PolyDB implementation status

Working branch: `claude-assist` (based on `nov-11`), pushed to GitHub.

## Current stage
Core engine expansion done for storage, relational, indexes, planner,
EXPLAIN, PBSQL, document aggregation, Mongo cursors/namespaces, CLI.

## Architecture (where things live)
- `src/storage.rs` – ordered KV store, WAL (torn-tail vs interior-corruption
  classification, salvage option), directory LOCK, range scans with phantom tracking.
- `src/rel.rs` – relational schemas (composite PK, UNIQUE, DEFAULT, FK with
  RESTRICT/CASCADE/SET NULL), row/index maintenance, constraint enforcement.
- `src/pbsql/` – PBSQL lexer, AST, parser (SQL is its relational subset), statement executor (DDL, DML for all models, KV statements, EXPLAIN).
- `src/query/` – shared `eval` (3-valued logic, dates, functions), `plan`
  (binder -> logical plan -> pushdown -> physical plan), `exec` (operators,
  stats, EXPLAIN rendering).
- `src/sql.rs` – thin SQL entry point onto PBSQL.
- `src/doc.rs`, `src/kv.rs` – native document / key-value APIs (unchanged interfaces).

## Completed
- WAL recovery distinguishes torn tail / interior corruption; `OpenOptions{salvage}`.
- Exclusive directory lock (second process or handle gets an error).
- Network limits (`server::Limits`, `--max-request-bytes`, `--max-connections`).
- ALTER TABLE ADD/DROP/RENAME COLUMN, RENAME TO (persisted, row rewrite).
- UNIQUE (single/multi), DEFAULT (literal + CURRENT_TIMESTAMP etc.), composite PK, FOREIGN KEY.
- DATE / TIME / TIMESTAMP (DATETIME) / JSON column types; CAST, CASE, EXTRACT, date funcs.
- Subqueries: IN, NOT IN, EXISTS, scalar, correlated, derived tables; UNION / UNION ALL.
- Ordered secondary indexes incl. composite; equality, IN, range, LIKE-prefix scans;
  PK point/prefix/range scans; document index range scans; KV get/prefix/range.
- Planner: predicate pushdown (LEFT JOIN safe), index selection, IndexLookupJoin /
  HashJoin / NestedLoopJoin, hash aggregation, limit pushdown.
- EXPLAIN / EXPLAIN ANALYZE (real per-operator rows, loops, examined, time).
- PBSQL: RELATIONAL./DOCUMENT./KV[.ns] sources, document paths, JSON literals,
  document INSERT/UPDATE/DELETE, KV GET/SET/EXISTS/INCREMENT, KV namespaces,
  cross-model joins, transactions across models.

- Aggregation pipeline (`src/aggregate.rs`): $match $project $addFields/$set $unset
  $sort $limit $skip $group $unwind $count $lookup $replaceRoot + accumulators;
  native `db.c.aggregate([...])`, `Database::doc_aggregate`, Mongo `aggregate`.
- Mongo cursors (getMore/killCursors, batchSize, 10 min expiry, per-connection cap),
  database isolation (`--mongo-default-db`, `db.coll` storage names), listDatabases, dropDatabase.
- CLI: multiline statements (`;` interactively, visibly-incomplete lines when piped), `.timing`.
- KV TTL: `SET KV 'k' = v TTL s`, Redis SET EX/PX/NX/XX/KEEPTTL, EXPIRE, PEXPIRE, TTL, PTTL, PERSIST;
  expired entries hidden from all reads, removed on next write.
- Real-driver checks (`tests/drivers.py`, redis-py/psycopg2/pymongo) pass, incl. new cursor/aggregate/PBSQL checks.

## Remaining / next tasks
1. Composite / unique document indexes; use document indexes for `$lookup`.
2. Streaming (non-materialising) executor; simple statistics for join ordering.
3. Extended PostgreSQL protocol (prepared statements) is still unsupported.

## Known limitations
- Cross-model atomicity holds because all models share one WAL/transaction.
- Document indexes: single field, non-unique. Expired KV entries stay in storage until the key is rewritten.
- Planner is rule-based (no statistics / cost model).
- Executor materialises intermediate results per operator (limit pushdown into scans only).
- Ordering comparisons between different types (number vs text) evaluate to NULL.

## Tests
`cargo test` – hardening (26), integration (22), engine (20), documents (4), cli (1), doctest (1): all passing.
`python3 tests/drivers.py` against `polydb serve --memory --port off --mongo-default-db app`: all passing.
`cargo fmt --all -- --check` passes (whole crate formatted in a4257d9). Clippy: only minor style warnings.
