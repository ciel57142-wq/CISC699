# PolyDB implementation status

Branch: `claude-assist` (based on `nov-11`), pushed to GitHub.
Last verified: `cargo fmt --check`, `cargo check --all-targets`, `cargo test`
(136 tests, all passing), `cargo clippy --all-targets` (no errors; style
warnings only), `scripts/smoke.sh`, `tests/drivers.py` and
`tests/clients/pg_extended.py` against a live server (all passing).

## Where things live
- `src/storage.rs` – ordered store, WAL (torn tail vs interior corruption, salvage), directory lock, streaming merge iterator, per-statement undo journal.
- `src/rel.rs` – relational schemas and constraints (PK incl. composite, UNIQUE, DEFAULT, NOT NULL, CHECK, FK RESTRICT/CASCADE/SET NULL), row and index maintenance.
- `src/doc.rs`, `src/docindex.rs`, `src/aggregate.rs` – document CRUD, document indexes, aggregation pipelines.
- `src/kv.rs` – key-value with TTL, expiry index and reaper.
- `src/pbsql/` – PBSQL lexer, AST, parser (SQL is its relational subset), statement execution, parameters.
- `src/query/` – binder/planner (`plan`), streaming executor and EXPLAIN (`exec`), expression evaluation (`eval`).
- `src/mongo.rs`, `src/resp.rs`, `src/pgwire.rs`, `src/server.rs` – wire protocols and server limits.
- `tests/` – see "Tests" below; `benches/perf.rs` – performance framework.

## Feature status

### Complete and tested
- **Storage/WAL:** format 2 log with LSNs, transaction ids and BEGIN/PUT/DELETE/COMMIT/ABORT/CHECKPOINT records; recovery redoes committed and rolls back incomplete transactions (ABORT logged), idempotent and crash-safe itself; WAL flushed before every snapshot; deterministic fail points (`FailPoint`) around log writes, commit records, snapshots, checkpoint, log truncation and recovery; format 1 directories open unchanged and convert on first write; torn final record dropped; interior corruption refused (`Error::Corrupt`) with opt-in salvage; repeated recovery stable; interrupted checkpoints (stray temp file, rename-before-truncate) recover; uncommitted transactions vanish; killing a writing server loses no acknowledged write (tested with the real binary).
- **Process safety:** exclusive directory lock, released on drop (explicit unlock) and on process death; concurrent sessions with serializable isolation (concurrent transfer test keeps totals; no lost increments).
- **Relational:** CREATE/DROP/ALTER (add/drop/rename column, rename table, add/drop CHECK constraint), INSERT/UPDATE/DELETE (incl. INSERT … SELECT, DEFAULT), SELECT with WHERE/GROUP BY/HAVING/DISTINCT/ORDER BY/LIMIT/OFFSET, INNER/LEFT/RIGHT/FULL/CROSS joins, UNION [ALL], IN/EXISTS/scalar/correlated/derived subqueries, CASE/CAST/date-time/JSON paths/scalar functions, three-valued NULL logic. Differentially tested against SQLite.
- **Constraints:** single and composite PK, single and multi-column UNIQUE, DEFAULT (literal and time functions), NOT NULL, CHECK, FOREIGN KEY with RESTRICT/CASCADE/SET NULL; enforced inside transactions, atomic per statement, persisted; property-tested invariants.
- **Relational indexes:** ordered single and composite indexes; equality, IN, ranges, LIKE prefix, PK point/prefix/range; NULL indexed (exact encoding); maintained through updates, deletes, rollback, restart; planner uses them (EXPLAIN-verified) and results equal sequential scans (property test). Indexes built by the first exact encoding on this branch are rebuilt automatically at open.
- **Document engine:** CRUD (one/many), nested paths, arrays, missing vs null, projection, sorting, pagination, database/collection isolation, persistence.
- **Document indexes:** single-field, compound, unique, nested-path, multikey (one array field); NULL/missing indexed; maintained on insert/update/delete; consistent through rollback; persisted; `validate`/`reIndex`; used by `find` (equality, ranges, index-ordered sorts with early termination) and by the PBSQL planner; results equal collection scans (property test).
- **Aggregation:** `$match $project $addFields/$set $unset $sort $limit $skip $group $unwind $count $lookup $replaceRoot` and `$sum $avg $min $max $count $first $last $push $addToSet`; empty input, null/missing, mixed types, long pipelines, invalid pipelines tested.
- **Key-value:** GET/SET/DEL/EXISTS/INCR, namespaces, TTL (SET EX/PX/NX/XX/KEEPTTL, EXPIRE/PEXPIRE, TTL/PTTL, PERSIST, PBSQL `SET KV … TTL`); expiry index ordered by time and a background reaper (no keyspace scans); expiry survives restart; expired keys never reappear (property test); TTL changes are transactional.
- **Planner/executor:** predicate pushdown (outer-join safe), index selection, lookup/hash/nested-loop joins, sort elimination (PK, exact index ranges, document `_id`/index, KV key), streaming pull-based operators with early LIMIT termination, external merge sort, spilling hash aggregation, grace hash join, spilled nested-loop inner side; spill results equal in-memory results (test with a 16 KB budget); cancellation (`Database::cancel_handle`, PostgreSQL CancelRequest).
- **EXPLAIN / EXPLAIN ANALYZE:** real physical plan; ANALYZE reports rows, loops, rows examined, time, spills.
- **PBSQL:** relational, document and KV sources, inline JSON, dotted paths incl. array positions, document DML, KV statements, cross-model joins/subqueries/unions, cross-model transactions, `$n` parameters.
- **PostgreSQL protocol:** simple query; extended protocol (Parse/Bind/Describe/Execute/Close/Sync/Flush, named/unnamed statements and portals, text/binary parameters and results, PortalSuspended, parameter count/type errors, per-connection statements); CancelRequest. Real clients: psycopg2, psycopg 3 (prepared, binary), pg8000.
- **MongoDB protocol:** CRUD, cursors (getMore, killCursors, batch sizes, expiry), aggregation, database isolation, createIndexes/listIndexes/dropIndexes; real client: pymongo.
- **Redis protocol:** strings, expiry commands, NX/XX, INCR family, errors; real client: redis-py.
- **Native protocol / CLI:** JSON-line protocol with request limits; shell with multi-line statements, `.timing`, EXPLAIN.

### Implemented but partially tested
- Memory limit for DISTINCT / UNION de-duplication: bounded with a clear error, not spilled.
- Mongo `listIndexes` shows key documents in field-name order (the index itself keeps the written order).
- Binary PostgreSQL parameter formats beyond int2/4/8, float4/8, bool, text, json are rejected (text format works for all).

### Not implemented (out of scope or blocked)
- Authentication and TLS on every protocol.
- PostgreSQL `COPY` and `pg_catalog`; Redis lists/hashes/sets/pub-sub/MULTI; Mongo change streams.
- Cost-based optimisation (planner is rule-based, no statistics).
- Disk-resident data: the whole dataset is held in memory (the disk is used for durability).

## Known limitations
- Object keys in stored documents are kept in name order.
- Ordering comparisons between different types (number vs text) are NULL in SQL/PBSQL.
- Expired keys can make a concurrent transaction that read them fail with a serialization conflict when the reaper deletes them.

## Tests (136, all passing; `cargo test`)
| File | What it covers |
|---|---|
| `tests/integration.rs` (22), `tests/hardening.rs` (26) | original suites (CLI contract, damaged files, hostile input) |
| `tests/engine.rs` (20) | relational extensions, indexes, planner, EXPLAIN, PBSQL, TTL |
| `tests/relational.rs` (12) | DDL/DML/joins/subqueries/NULL/types/functions/errors, constraint interactions, persistence |
| `tests/property.rs` (7) | index vs scan (tables, documents), rollback, restart, UNIQUE/FK invariants, expiry, PBSQL vs SQL vs document |
| `tests/differential.rs` (1) | random queries vs SQLite (python3 sqlite3; skips with a message if absent) |
| `tests/recovery.rs` (8) | WAL/recovery edge cases, kill test, locking, concurrency, large data, limits, malformed input |
| `tests/wal_recovery.rs` (17), `src/storage.rs` unit tests (3) | fail-point crash tests: torn/corrupt records, crashes around the commit record, every checkpoint step, during recovery, crash storm; format 1 conversion |
| `tests/streaming.rs` (6) | document indexes, early termination, spilling, RIGHT/FULL joins, cancellation |
| `tests/kv.rs` (7), `tests/protocols.rs` (5) | KV and TTL cleanup; raw-wire PostgreSQL extended/cancel, Redis, Mongo indexes, native |
| `tests/crossmodel.rs` (5), `tests/docsuite.rs` (6), `tests/documents.rs` (4) | cross-model integration; document CRUD and aggregation |
| `tests/regressions.rs` (5), `tests/cli.rs` (1), doctest (1) | bugs found by the property/differential tests; shell |

Scale knobs: `POLYDB_CASES` (property/differential cases), `POLYDB_SEED`, `POLYDB_DATASET=small|medium|large|stress|<n>`.
Real-client checks (not part of `cargo test`): `bash scripts/driver_checks.sh` runs `tests/drivers.py` (redis-py, psycopg2, pymongo) and `tests/clients/pg_extended.py` (psycopg 3, pg8000).

Bugs found and fixed by the new tests: inverted index ranges panicking, composite indexes missing rows with NULLs, descending document indexes scanned backwards, array positions in dotted paths, missing Mongo command names, flaky lock release when another thread forks, quadratic statement savepoints in long transactions.

## Performance (measured, `POLYDB_DATASET=medium` = 100,000 records, this container)
| workload | ops/s | p50 µs | p99 µs |
|---|---:|---:|---:|
| insert, batched transaction | 256,197 | – | – |
| insert, autocommit (fsync each) | 5,280 | 177 | 276 |
| point lookup by PK (SQL text each time) | 50,938 | 19 | 48 |
| range scan via index (10% of rows) | 77 queries/s | 12,452 | 17,744 |
| sequential scan + filter | 17 queries/s | 55,885 | 82,002 |
| ORDER BY indexed column LIMIT 10 | 1,353 | 723 | 857 |
| GROUP BY aggregate | 27 queries/s | 36,480 | 36,928 |
| hash join 100k × 200k + group | 3 queries/s | 331,232 | 346,120 |
| document insert, batched | 181,120 | – | – |
| document find via index | 215,329 | 4.3 | 8.9 |
| document `$group` over 100k | 4 queries/s | 266,649 | 271,387 |
| KV set (fsync) / no fsync / get | 8,112 / 745,011 / 2,111,320 | | |

Peak RSS 277 MB; data directory 31.6 MB (WAL only, before checkpoint).
SQLite (in-memory, through Python) for the same relational workloads is
roughly 5–15× faster on scans, aggregates and joins and ~6× faster on batched
inserts: PolyDB re-parses and plans each statement, decodes JSON rows, and
builds string hash keys. Point lookups and document/KV reads are where it is
closest.

## Next tasks (if work resumes)
1. Faster row/document encoding (binary instead of JSON) and typed hash keys for joins/aggregation.
2. Spill DISTINCT/UNION de-duplication instead of failing past the memory limit.
3. Plan cache for repeated statements (prepared statements already parse once on the client side only).
4. Simple statistics (row counts) for join ordering.
