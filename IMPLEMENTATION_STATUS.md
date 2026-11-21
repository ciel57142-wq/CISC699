# PolyDB implementation status

Working branch: `claude-assist` (based on `nov-11`). GitHub push access was
unavailable (HTTP 403) in the first session; commits are local until access is fixed.

## Current stage
Core engine expansion: storage hardening, relational constraints, ordered
indexes, shared planner/executor, EXPLAIN, PBSQL. Next: document
aggregation pipeline, Mongo cursors/namespaces, CLI multiline.

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

## Remaining / next tasks
1. Document aggregation pipeline (`$group`, `$unwind`, `$project`, `$lookup`, accumulators) in `doc.rs`, used by Mongo protocol and `db.c.aggregate([...])`.
2. Mongo cursors (`getMore`, `killCursors`, batch sizes) and database namespaces (`db.coll` collection names).
3. CLI: multiline statements, PBSQL help text; pgwire command tags for ALTER/EXPLAIN.
4. KV TTL (not implemented).
5. Update README/REFERENCE for PBSQL syntax.

## Known limitations
- Cross-model atomicity holds because all models share one WAL/transaction.
- Document indexes: single field, non-unique. KV has no TTL.
- Planner is rule-based (no statistics / cost model).
- Executor materialises intermediate results per operator (limit pushdown into scans only).
- Ordering comparisons between different types (number vs text) evaluate to NULL.

## Tests
`cargo test` – hardening (26), integration (22), engine (19), doctest (1): all passing.
`cargo fmt --check` was already failing on the base branch (pre-existing formatting).
