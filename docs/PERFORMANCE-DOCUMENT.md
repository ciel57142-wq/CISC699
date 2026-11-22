# Document engine: profiling, changes and measurements

This records the document-engine optimisation pass (indexing, query
execution, aggregation, storage layout). Every change below was made in
response to a measurement, and each is checked by `tests/document.rs`.

## Method

* **Benchmark:** `examples/doc_bench.rs`, run with
  `cargo run --release --example doc_bench` (20,000 user documents with
  nested objects and arrays, 3 indexes, plus 200 documents of about 50 KB).
  It uses only calls the previous version already had, so the identical
  file runs against both versions. Every query checks its result size, so
  a version cannot win by returning less.
* **Before / after:** the previous commit (`367985b`) was checked out in a
  separate worktree and built with the same benchmark file. The two binaries
  were run alternately, 5 times each, and the table reports the minimum per
  line. Run-to-run noise on this machine is about ±10% on lines under
  ~150 ms, so differences that small are not claimed as wins or losses.
* **Profiles and instruction counts:** `valgrind --tool=callgrind` on a
  release build with debug info. Instruction counts are deterministic and
  were used to settle the lines where wall-clock noise was larger than the
  difference.
* Machine: 4 vCPU Intel Xeon @ 2.10 GHz, Linux, rustc 1.97.0, in-memory
  database unless stated (no fsync).

## What the profile showed (before)

Callgrind over the whole benchmark (2,000-document run):

| where the instructions went | share |
|---|---|
| `doc::decode` → `JsonParser` (parsing stored documents) | 65% |
| of which `JsonParser::string` building strings byte by byte | 40% |
| `malloc` / `realloc` from those string buffers | 18% |
| `Ctx::scan` copying every scanned key and value into a new map | most of the rest of `candidates` |

Every candidate document was parsed in full before the filter looked at it,
and only `_id` equality and equality on an indexed field avoided a full
scan: ranges, `$in`, `null`, sorts and counts all parsed every document in
the collection. Aggregation materialised the whole collection, and there
was no general `$group`.

## Changes

| # | change | why (measurement) |
|---|---|---|
| 1 | `Ctx::get_ref` / `Ctx::iter_range`: reads borrow from the store and the write set (a merge iterator) instead of copying | scans copied every value; indexed equality 141 → 93 ms and large-document scan 2,138 → 1,337 ms from this alone |
| 2 | JSON string fast path: copy unescaped runs in one go and skip UTF-8 re-validation | `JsonParser::string` was 40% of the profile |
| 3 | **Binary document layout** (`src/doc/codec.rs`): length-prefixed containers, sorted keys, zigzag-varint integers | reading one field costs 0.10 µs instead of a 1.1–1.3 µs full parse; encoding is 2× faster than JSON; a typical document is 11% smaller (154 vs 173 bytes) |
| 4 | Filters compiled once into a predicate tree (`filter.rs`) and evaluated on the stored bytes | non-matching documents are never decoded |
| 5 | Planner (`plan.rs`): `_id` `$in` and ranges; index `$in`, ranges, `null` / `$exists: false`; sorts delivered by an index; equality preferred over ranges | ranges and `$in` scanned the collection |
| 6 | Executor (`exec.rs`): streaming with early stop for `limit`, top-k selection instead of a full sort, sort keys read from stored bytes, only result documents decoded, projection decodes only kept fields | sort + limit decoded and sorted all 20,000 documents |
| 7 | Exact lookups skip the re-check: integer/string `_id` lookups, and a lone text/bool equality on an index | the re-check was ~20% of indexed-equality instructions |
| 8 | Aggregation pipeline (`aggregate.rs`): leading `$match/$sort/$skip/$limit` pushed into the planner, `$group`/`$count` read only referenced fields, `$sort`+`$limit` fused into top-k | the old pipeline loaded the whole collection; no `$group` |
| 9 | Bulk insert: metadata, index list and id sequence read/written once per batch; command documents moved instead of cloned | per-document metadata parse and a full clone + drop of every inserted document |
| 10 | Updates decode once and change the document in place, then write only the index entries that changed (old/new key diff); deletes compute index keys from the stored bytes | an update cloned the old document and rewrote every index entry |
| 11 | Planner reads index metadata only when a plan could use it, and allocates no maps for simple filters | `_id` point lookups were 7% *slower* in instructions after change 3–6 because of planning overhead; fixed |
| 12 | Object decoding with in-order inserts rather than `collect()` | `collect()` buffers, sorts and bulk-builds: 2 extra allocations per small object |

## Results

Before = commit `367985b`, after = this change; minimum of 5 alternating runs.

| benchmark | before (ms) | after (ms) | speedup |
|---|---:|---:|---:|
| bulk insertMany (batches of 500, 3 indexes), 20,000 docs | 145.3 | 105.1 | 1.38× |
| single doc_insert (typed API, own commit) ×5,000 | 16.0 | 12.9 | 1.24× |
| `_id` point lookup (typed API) ×5,000 | 11.0 | 9.8 | 1.12× |
| `_id` point lookup (command text) ×5,000 | 10.1 | 9.3 | 1.09× |
| indexed equality (city, 200 docs returned) ×200 | 103.6 | 95.3 | 1.09× |
| indexed nested equality (`addr.zip`) ×500 | 54.3 | 37.8 | 1.44× |
| indexed equality + residual predicate ×200 | 107.2 | 70.5 | 1.52× |
| indexed range (`age >= 40 and < 42`) ×50 | 4,027.5 | 62.9 | 64× |
| indexed `$in` (3 cities) ×100 | 7,738.9 | 151.4 | 51× |
| unindexed nested scan (`addr.geo.lat`) ×20 | 1,501.3 | 57.9 | 26× |
| full scan, no match ×20 | 1,461.2 | 56.0 | 26× |
| sort by score desc, limit 10 (no index) ×20 | 1,950.3 | 125.9 | 15× |
| sort by indexed age, limit 10 ×20 | 2,278.6 | 2.4 | 949× |
| count (indexed equality) ×200 | 96.0 | 11.0 | 8.7× |
| aggregate `$match/$group/$sort/$limit` ×10 | not supported | 51.3 | |
| same result computed in the application (find + fold) ×10 | 806.3 | 387.0 | 2.1× |
| update `$inc` through index (city) ×100 | 170.8 | 64.8 | 2.6× |
| update indexed field (`$set age`) by `_id` ×2,000 | 15.3 | 10.7 | 1.43× |
| delete by `_id` ×1,000 | 5.5 | 5.0 | 1.10× |
| large doc insert (~50 KB JSON each) ×200 | 96.4 | 53.7 | 1.80× |
| large doc `_id` lookup ×2,000 | 436.7 | 335.8 | 1.30× |
| large doc scan on a top-level field ×20 | 1,960.8 | 288.7 | 6.8× |
| large doc `$set` one field by `_id` ×500 | 234.0 | 114.8 | 2.04× |
| durable insertMany (no fsync), 20,000 docs | 142.4 | 117.7 | 1.21× |
| reopen (WAL replay of 20,000 docs + one query) ×5 | 110.5 | 123.6 | 0.89× |

Aggregation on the server (51 ms) is 16× faster than the old way of getting
the same answer (806 ms: fetch every matching document and fold in the
application).

### Instruction counts for the decode-bound lines

Fetch-heavy lines return whole documents, and building a `Value` tree is
allocation-bound in any format (a plain `Value::clone` of the test document
costs 0.6 µs; a full binary decode 1.0 µs; a JSON parse 1.1–1.3 µs). Their
wall-clock differences are close to the noise, so here are callgrind
instruction counts for the same work (20,000 documents, city and zip
indexes):

| workload | before | after | change |
|---|---:|---:|---:|
| 5,000 `_id` lookups | 120.7 M | 119.6 M | −1% |
| 200 city-equality queries (200 docs each) | 1,006.9 M | 777.9 M | −23% |
| 500 `addr.zip` queries (40 docs each) | 491.9 M | 393.5 M | −20% |
| 5 × `find({})` decoding all 20,000 docs | 2,377.9 M | 1,874.8 M | −21% |
| reopen (WAL replay) + one indexed query | 222.3 M | 218.3 M | −2% |

**Reopen** is the one line that measured slower on the wall clock (0.89×).
It does less work (−2% instructions) on a WAL that is 8% smaller
(4.38 MB vs 4.77 MB), and it is a single allocation- and I/O-bound pass, so
this reads as noise rather than a regression; it is recorded here rather
than left out.

## Correctness

Speed changes are only accepted with these checks (`tests/document.rs`):

* **Indexed vs unindexed differential test:** the same random documents
  (values chosen to hit the encoding edges: `-0.0`/`0.0`, `1`/`1.0`, integers
  above 2^53 that share a float encoding, text with NUL bytes, arrays,
  objects, null, missing fields) in an indexed and an unindexed collection;
  7,200 random filters with random sorts, skips and limits must give
  identical ordered results, also after random updates, deletes and inserts.
  It found one real bug during development (equality on an index combined
  with `sort({_id: -1})` returned ascending order), fixed before commit. It
  was also run locally with 60 seeds (108,000 queries).
* **Index maintenance:** `validate()` recomputes every index entry from the
  documents and compares, after each kind of update (`$set` on indexed and
  nested fields, `$inc`, `$push`, `$pull`, `$unset`, replacement, failed
  updates), deletes, index creation and `reIndex()`.
* **Crash consistency:** a WAL holding an update, a bulk insert and a delete
  is cut at every byte offset; each recovered database must pass `validate()`
  and be in exactly one of the four commit-boundary states.
* **Old databases:** a WAL written in the 0.2 format (JSON documents, index
  without null entries) is opened, queried through its index, updated
  (documents rewritten in the binary layout), re-indexed and reopened.
* **Transactions:** index range reads still conflict with a concurrent
  insert into the range; uncommitted writes are visible to index scans.
* **Codec:** round trips, path lookups equal to `get_path`, and truncated or
  bit-flipped encodings return errors instead of panicking.

## Known limits

* A sort spec is a JSON object and its keys are stored sorted, so a
  multi-key sort applies keys alphabetically (unchanged from before).
* Numeric equality through an index is always re-checked (encodings of
  `1`/`1.0`, `-0.0`/`0.0` and integers above 2^53 collide by design).
* An index delivers a sort only when every document has a scalar at the
  indexed path; otherwise the sort is done in memory (still top-k).
* Indexes built by 0.2 serve equality, `$in` and ranges immediately; they
  serve `null` queries and sorts after `db.<coll>.reIndex()`.
* Projection is top-level only. Aggregation supports field paths and
  `$literal` as expressions, not arithmetic or string operators.
