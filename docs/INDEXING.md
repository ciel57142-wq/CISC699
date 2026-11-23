# Indexing and query optimization

This covers the relational planner: what the index subsystem looked like
before this change, why it keeps its ordered-keyspace design rather than
adding a separate B+ tree, what was added, how EXPLAIN / EXPLAIN ANALYZE
report plans, and how it was tested and measured.

Code: `src/sql/access.rs` (access paths), `src/sql/stats.rs` (statistics,
estimates), `src/sql/query.rs` (pushdown, join order and algorithms),
`src/sql/plan.rs` (plan tree). Tests: `tests/planner.rs`. Benchmarks:
`examples/planner_bench.rs`, `examples/planner_compare.rs`. Raw numbers:
`docs/evidence/planner-bench-2026-11-22.txt`.

## 1. Audit of the index subsystem before this change

| Area | What existed | Gap |
|---|---|---|
| Storage | One ordered `BTreeMap<Vec<u8>, Vec<u8>>` in memory, made durable by a checksummed WAL plus snapshots. Rows at `r\0<table>\0<pk>`, index entries at `i\0t:<table>\0<field>\0<values><row id>` with empty values. | None for indexing (see section 2). |
| Index kinds | Single-column and composite indexes (column names joined by U+001F), unique indexes, composite primary keys. Order-preserving encodings; INT and REAL share one encoding. | No `IN` lists on secondary indexes. |
| Maintenance | Every INSERT / UPDATE / DELETE / ALTER writes index entries in the same commit as the row. `CHECK TABLE` compares entries with rows; `REINDEX` rebuilds them. | No automatic check or repair when a database is opened. |
| Access paths | Fixed preference order: primary-key lookup, primary-key IN, unique-index lookup, longest equality prefix, then a range, then a full scan. | No statistics, so it could not tell a 0.1% predicate from a 90% one. It used an index even when a scan was cheaper. |
| WHERE pushdown | Only into the **first** FROM table. | Joined tables were always fully scanned. |
| Joins | Written order. Hash join on equality, otherwise nested loop. | No reordering and no index joins. A selective table written second still had the other table fully scanned. |
| Subqueries | Uncorrelated ones cached. Correlated ones re-run per outer row. | A correlated predicate such as `o.cust = c.id` could not use an index: one full scan per outer row. |
| EXPLAIN | A flat, de-duplicated list of notes (`orders: index orders_customer (customer_id =)`). | No operators, estimates, filters, sorts or measurements. No EXPLAIN ANALYZE. |

## 2. B-tree / B+ tree suitability

The storage core already **is** an ordered B-tree: Rust's `BTreeMap`, an
in-memory B-tree with up to 11 keys per node. Every table and index is a
key range in it:

* rows are **clustered by primary key** (`r\0t\0` + order-preserving key);
* a secondary index is a sorted run of `(index values, row id)` keys, the
  same layout as the leaf level of a B+ tree secondary index;
* a lookup is one O(log n) seek; a range or prefix scan is a seek followed
  by in-order iteration, which is what B+ tree leaf chaining provides.

Options considered:

| Option | Verdict |
|---|---|
| **Separate page-based B+ tree per index** (own pages, buffer pool, splits, page-level WAL) | Rejected. The data is memory-resident, and recovery replays *logical* writes (key puts and deletes) in single atomic commits. A page-level tree would duplicate the ordering the map already has. It would also need its own crash consistency, and it would bring back the risk that a table and its index disagree after a crash, which the shared keyspace rules out today. |
| **Hash indexes** | Rejected. No ranges, no leftmost-prefix use of composite indexes, no ordered iteration. |
| **Keep the ordered keyspace** (chosen) | O(log n) seeks and ordered ranges. Composite prefixes work through the order-preserving encoding. Index entries commit atomically with their rows in the same WAL record. |

The measurements back this up. The cost is in fetching and decoding rows,
not in walking the tree. In `planner_bench` (100k rows), a sequential row
read costs about 0.7 µs. A row fetched through an index costs about
1.7–2.8 µs, and most of that is the random lookup and JSON decode of the
row, not the index seek. A different tree structure would not change that.
A page-based B+ tree becomes worth it when data no longer fits in memory.
That is a storage-engine change, not a planner change.

## 3. What was implemented

| Task | Implementation |
|---|---|
| Range-capable indexing | Range scans on any index column after an equality prefix. Strict and inclusive bounds are tracked separately for estimates. Several bounds on one column are combined into one range. One-column primary-key ranges, composite primary-key prefixes, and composite-index prefixes plus a range all work. |
| Composite indexes | Leftmost equality prefix, then a range **or an `IN` list** on the next column (`Index IN Scan`: one seek per value). |
| Equality / range lookups | Cost-based choice among all candidate paths (section 5). A primary-key point lookup is taken at once, without loading statistics. |
| Index selection from predicates | Predicates come from WHERE and ON. Values can be constants, or columns of an enclosing query inside a correlated subquery, so `EXISTS (SELECT … WHERE o.cust = c.id)` does one index lookup per outer row instead of a full scan. |
| Predicate pushdown | WHERE is split into AND-ed parts. A part that reads one table is evaluated while that table is read and drives its access path. This applies to **every** table, not only the first, except a table on the side an outer join NULL-extends. ON conditions on the inner side of INNER and LEFT joins are pushed too. |
| Join ordering | When all joins are INNER/CROSS, greedy ordering: start from the input with the fewest estimated rows, then add the connected input that gives the smallest estimated join. Cross products are avoided while a connected input remains. `SELECT *` keeps the written column order. |
| Join algorithms | **Index Nested Loop** (one primary-key or index lookup per outer row) when its cost is below reading the inner table. Otherwise **Hash Join** on the equality conditions, or **Nested Loop** when there is none. The choice uses the real outer row count, known at that point because inputs are materialised. |
| Selectivity / cardinality | `ANALYZE` statistics: row count, distinct values, NULLs, top-10 most common values, 101-point equi-depth histogram with interpolation. Join size is `|L|·|R| / max(ndv)` per equality key. Group count is the product of the grouped columns' distinct counts, capped at the input size. |
| Fewer table scans | Everything above. See the measured before/after in section 7. |
| Index consistency | Unchanged invariant: rows and their index entries are written in one commit. Newly tested with a random workload of inserts, updates (key and indexed columns), deletes and rolled-back transactions, checked against an unindexed copy with `CHECK TABLES` after every batch. |
| Recovery | `Database::open_verified` (CLI: `--verify-indexes`) runs the full index check for every table after recovery and rebuilds, in one atomic commit, the indexes of any table with missing, orphaned or stray entries. It returns a report. |

Correctness rules the planner keeps:

* Access paths and join algorithms only narrow the candidates. Every row is
  still checked against its full condition.
* A pushed or reordered condition that hits a runtime error (say, division
  by zero) on some row keeps that row and remembers the error. The full
  WHERE and ON conditions are then evaluated again, strictly and in written
  order, on the final rows, so those errors still surface. One deliberate
  difference, shared with most SQL engines: a row removed by a *different*
  table's pushed condition is never evaluated, so its error cannot appear.
* ON clauses are only moved when every column they name resolves the same
  way in the whole FROM clause as at the written position.
* Statistics are read outside a transaction's read set: a concurrent
  `ANALYZE` never makes a transaction conflict. Statistics are only
  estimates and never change a result. `ALTER TABLE`, `TRUNCATE` and
  `DROP TABLE` discard them.

## 4. EXPLAIN and EXPLAIN ANALYZE

```
EXPLAIN SELECT ...           -- plan with estimates, no measurements
EXPLAIN ANALYZE SELECT ...   -- plan with estimates AND measurements
ANALYZE [[TABLE] name]       -- gather statistics (all tables if no name)
```

Each line is one operator. Children are indented under `->`, and details
follow on the lines below the operator:

| Operator | Details shown |
|---|---|
| `Seq Scan`, `Primary Key Lookup / IN Lookup / Range Scan / Prefix Scan`, `Index Scan`, `Index Range Scan`, `Index IN Scan` (`using <index>`) | `Index Cond` / `Key Cond`: what bounds the key range. `Filter`: the pushed conditions checked per row. |
| `Hash Join`, `Nested Loop`, `Index Nested Loop` (`inner/left/right/full/cross`) | `Hash Cond`, `Join Filter` |
| `Index Probe on t using <index>` (inner side of an index nested loop) | `Index Cond: col = outer.col`, `Filter` |
| `Filter` (what is left of WHERE above the joins) | `Filter` |
| `Aggregate`, `Hash Aggregate` | `Group Key`, `Having`, `Output` |
| `Project`, `Distinct`, `Sort` (`Sort Key`), `Limit` | |
| `Union [All]`, `Intersect`, `Except`, `Derived Table`, `Subquery Scan`, `Result` | |
| `SubPlan (correlated, evaluated per row)` / `SubPlan (uncorrelated, evaluated once)` | the subquery's own tree |

The last lines are `Statistics:` (for each table, analyzed with its row
count, or "default estimates") and, under ANALYZE, `Execution time:`.

`(est. rows=N)` is the planner's estimate of rows produced per execution.
EXPLAIN ANALYZE adds `(actual rows=… loops=… time=… examined=… index_entries=…)`:

* **actual rows**: rows the operator produced, **totalled over all loops**;
* **loops**: how many times it ran (per outer row for a correlated
  subquery or an index probe);
* **time**: wall-clock time of the operator's own work, excluding its inputs
  but including any subquery it evaluated;
* **examined**: rows read from storage (scans and probes), or rows / pairs
  tested (filters, joins, aggregates);
* **index_entries**: index entries read.

A measurement is only printed for something that ran. An operator that
never ran says `(never executed)`, and nothing is filled in from estimates.
Plain EXPLAIN executes the query too, because subquery plans are only known
once they run, but it prints no measurements. EXPLAIN of a slow query
therefore takes as long as the query.

Example (`cargo run --release --example explain_demo`, after `ANALYZE`):

```
> EXPLAIN ANALYZE SELECT * FROM customers c LEFT JOIN orders o ON o.customer_id = c.id AND o.status = 'open' WHERE c.id IN (1, 2, 3)
Project  (est. rows=3)  (actual rows=12 loops=1 time=0.008 ms examined=12)
  Output: id, name, city, vip, id, customer_id, total, status
   -> Index Nested Loop (left)  (est. rows=3)  (actual rows=12 loops=1 time=0.044 ms examined=10)
        Join Filter: (o.customer_id = c.id) AND (o.status = 'open')
      -> Primary Key IN Lookup on customers AS c  (est. rows=3)  (actual rows=3 loops=1 time=0.008 ms examined=3)
           Key Cond: id IN (3 keys)
           Filter: c.id IN (1, 2, 3)
      -> Index Probe on orders AS o using orders_customer  (est. rows=10)  (actual rows=10 loops=3 time=0.036 ms examined=30 index_entries=30)
           Index Cond: customer_id = c.id
           Filter: o.status = 'open'
Statistics: customers (analyzed, 2000 rows), orders (analyzed, 20000 rows)
Execution time: 0.114 ms (12 row(s) returned)
```

Reading it: the three customers were found by key (3 rows examined).
Each was probed in the `orders_customer` index (3 loops, 30 entries and
30 rows read, 10 kept by the pushed `status = 'open'` filter). The
estimate of 3 joined rows against 12 actual is a real estimation error.
The estimator assumes the 2,000 open orders are spread evenly over all
2,000 customers (column independence). In this data they all belong to
customers whose id ends in 1: customer 1 has 10 and customers 2 and 3
have none, so those two are NULL-extended, 10 + 2 = 12. Estimate and
measurement are printed side by side so errors like this are visible.

## 5. Statistics and cost model

Without statistics: a table is assumed to hold 1000 rows, an equality on
a non-unique column matches 1%, a one-sided range a third, and a two-sided
range a ninth. A unique column matches one row. These defaults reproduce
the old preference order, so behaviour without `ANALYZE` stays predictable.

With statistics:

* `col = v`: the value's frequency if it is a most-common value; 0 if it
  is outside the histogram's range; otherwise the remaining fraction
  spread evenly over the remaining distinct values.
* ranges: fraction of histogram points below each bound, interpolated
  linearly between points for numbers. Exact counts are used when the
  histogram holds every value (101 or fewer non-NULL values).
* `IN`: sum of the equalities. `IS NULL`: NULL fraction. `OR`, `NOT`,
  `<>` are combined by the usual rules. Anything else: 1/3.

Costs are in units of one sequential row read. They were calibrated with
`planner_bench` (100k rows, release build):

| constant | value | measured basis |
|---|---|---|
| sequential row | 1.0 | about 0.7 µs per row in a full scan |
| index entry | 0.1 | |
| row fetched by key | 1.7 | about 1.7–2.8 µs per row through an index |
| seek | 0.5 | |
| hash build / probe | 0.1 / 0.05 | |

So an index wins up to roughly 55% of the table. Measured: at 50% the
index was 1.5× faster than a scan, at 60% the scan was 1.45× faster.

## 6. Tests

`tests/planner.rs` (13 tests) checks correctness before performance:

* **Datasets with different cardinalities and distributions**: unique,
  uniform with 100 values, geometric (half the rows one value), a 2%
  boolean, 10 text categories, and a uniform REAL with 10% NULLs. An
  identical copy has no secondary indexes and is the oracle.
* **Indexed vs full scan**: 32 predicates covering equality, ranges with
  strict and inclusive bounds, IN lists (with duplicates and misses),
  composite prefix plus range or IN, NULLs, cross-type values (`id = 17.0`,
  `grp = 'x'`), OR and NOT. Results must equal the unindexed copy's,
  before and after ANALYZE.
* **Access-path choice**: follows the predicates without statistics and
  the data with them. Wide predicates switch to Seq Scan. Of two usable
  indexes the narrower wins.
* **Estimates vs actual**: after ANALYZE, scan estimates must be within
  5–60% of measured rows, depending on the predicate.
* **Joins**: 18 queries (inner, multi-way, cross, LEFT/RIGHT/FULL, ON with
  non-equality parts, derived tables, EXISTS / NOT EXISTS / IN subqueries).
  Results must equal the same queries over copies with no keys or indexes.
  Plan checks cover index nested loop vs hash join, greedy join order, no
  cross products, and pushdown only where safe.
* **EXPLAIN vs EXPLAIN ANALYZE**: EXPLAIN shows no measurements. ANALYZE
  counts match the data exactly (a full scan examines N rows, a
  primary-key miss reads none, an index scan reads one entry per match,
  the top node's rows equal the rows returned, correlated subplan loops
  equal the outer rows). Identical UNION branches are separate operators.
  Unexecuted operators say so.
* **Runtime-error semantics** of pushed and reordered conditions.
* **Index consistency** under a random workload with rolled-back
  transactions.
* **Recovery**: delete every third index entry and add an orphan directly
  in the store files. `CHECK TABLE` sees the damage and queries go wrong.
  `open_verified` rebuilds exactly that table, the indexes are consistent
  afterwards, and the repair persists.

The existing 89 tests still pass. Their EXPLAIN assertions were updated to
the new operator names.

## 7. Measurements

From `docs/evidence/planner-bench-2026-11-22.txt` (100,000-row fact table,
1,000-row dimension table, in memory, release build, median of 5 runs).
Each result was compared with the no-index form before timing.

| query | rows | planned | no index | plan chosen |
|---|---:|---:|---:|---|
| pk equality | 1 | 0.01 ms | 69 ms | Primary Key Lookup |
| index eq, 0.1% | 97 | 0.13 ms | 68 ms | Index Scan |
| index eq, 5% | 5040 | 11.8 ms | 75 ms | Index Scan (composite prefix) |
| index eq, skewed common value (50%) | 50088 | 100 ms | 122 ms | Index Scan |
| IN list on index | 413 | 0.64 ms | 71 ms | Index IN Scan |
| index range, 0.5% | 481 | 0.71 ms | 63 ms | Index Range Scan |
| index range, 10% | 10120 | 22.9 ms | 79 ms | Index Range Scan |
| index range, 60% | 59669 | 132 ms | 139 ms | **Seq Scan** (stats) |
| composite eq + range | 234 | 0.37 ms | 69 ms | Index Scan |
| join, selective dimension | 4973 | 11.7 ms | 75 ms | Index Nested Loop |
| join, large table written first | 1 | 2.1 ms | 70 ms | Index Nested Loop |
| join, both large | 1 | 302 ms | 317 ms | Hash Join |

Before and after, with the same queries on the previous engine (commit
`367985b`):

| query | previous | new, no ANALYZE | new, ANALYZE |
|---|---:|---:|---:|
| IN list on index | 86.1 ms | 0.70 ms | 0.62 ms |
| index range, 60% | 249 ms | 172 ms | 92.7 ms |
| join, selective dimension | 166 ms | 7.7 ms | 7.3 ms |
| join, point lookup on dimension | 71.5 ms | 0.18 ms | 0.18 ms |
| join, large table written first | 145 ms | 1.75 ms | 1.41 ms |
| join, both large (hash) | 245 ms | 233 ms | 236 ms |

The existing `sql_bench` is unchanged within noise except for single-row
primary-key SELECTs: 2.98 → 3.23 µs per statement in the recorded run.
That is the fixed cost of planning (conjunct analysis and pushdown
bookkeeping) on a statement that otherwise does almost nothing.

## 8. Limitations

* Join ordering is greedy, not exhaustive, and only for INNER/CROSS joins.
  Outer joins keep their written order. There is no merge join.
* No index-only scans. ORDER BY never uses index order, and LIMIT does not
  stop a scan early, because the executor materialises every operator.
* OR across different columns cannot use indexes (no index union).
* Statistics are gathered only by `ANALYZE` and are not refreshed
  automatically. After heavy changes they go stale until the next
  `ANALYZE` (plans may get slower; results stay correct).
* Column-independence is assumed between different columns.
* `open_verified` reads every row and index entry, so it costs one full
  pass over the data and is opt-in.
