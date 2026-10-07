# Issue log

Severity follows HS1 SC-7: **S1** blocks a gate or makes a result wrong, **S2** degrades a result or
the schedule, **S3** cosmetic or documentation. Every entry says how it was found, because that is
part of the evidence. Opened at HS3 (Week 6, 2026-10-07); entries are never deleted, only closed.

| ID | Sev | Status | Area | One line |
|---|---|---|---|---|
| ISSUE-002 | S1 | **open** | environment | Nothing has run on the project laptop yet; all HS3 evidence so far is from a cloud sandbox |
| ISSUE-016 | S1 | **open** | comparator | Spike S-1 (Hasura DDN local build) not run; highest-risk external dependency still unverified |
| ISSUE-013 | S2 | **open** | performance | Python parse+plan costs ~3.4 ms per query, more than the whole consolidated query at small k (R18) |
| ISSUE-004 | S2 | open | reproducibility | compose.yaml pins tags, not `@sha256` digests (HS2 §8.1) |
| ISSUE-006 | S2 | open | data | `ratios.json` is provisional; UniBench SF1 row counts not yet taken |
| ISSUE-017 | S2 | open | measurement | Byte cross-check against container network counters (T-H1) not implemented |
| ISSUE-012 | S2 | open | CI | GitHub Actions workflow written but never observed running |
| ISSUE-019 | S2 | open (needs ADR) | design | R2 bind join is applied to same-store joins too; HS2 §6.3 says "across stores" |
| ISSUE-018 | S3 | open (needs ADR) | design | R3 runs after R4, not in table order, so it sees which sorts stayed residual |
| ISSUE-015 | S3 | open | protocol | Sweep point t = 10 gives k = 0 at S = 0.1; the low sweep points only mean something at S ≥ 1 |
| ISSUE-005 | S3 | open | data | Distribution checks need ≥ 500 customers, so they SKIP at the CI scale S = 0.01 |
| ISSUE-022 | S3 | open | environment | Laptop core count unknown; default `cpuset: "2,3"` needs ≥ 4 logical CPUs |
| ISSUE-001 | S3 | open | process | Code not yet pushed to github.com/ciel57142-wq/CISC699 |
| ISSUE-003 | S2 | workaround | environment | Sandbox could not pull Docker images or MongoDB binaries; FerretDB 1.24 stands in for MongoDB 7.0 |
| ISSUE-014 | S2 | workaround | environment | FerretDB `$in` lookups ignore the `customerId` index, so sandbox timings are unusable |
| ISSUE-024 | S2 | fixed | measurement | PG byte counting ran inside the timed region and inflated engine latency, mostly for A4 |
| ISSUE-010 | S1 | **fixed** | engine | Backend capability gap surfaced at run time and was mislabelled `BACKEND_UNAVAILABLE` |
| ISSUE-011 | S1 | **fixed** | oracle | False divergence on W3 at S = 0.1: integral `float8` rendered as a JSON integer broke the sort key |
| ISSUE-009 | S2 | fixed | engine | `$group` with `_id: null` returns no row on empty input; SQL returns one row with count 0 |
| ISSUE-007 | S2 | fixed (preventive) | engine | PostgreSQL locale collation vs MongoDB binary string order |
| ISSUE-008 | S3 | fixed | contract | HS2 §7.2 had no code for spec-level GraphQL validation errors |
| ISSUE-020 | S3 | fixed | tests | Two defects in the property test itself, found by Hypothesis |
| ISSUE-021 | S3 | fixed | toolchain | Sandbox system Python is 3.13; project pins 3.12 through uv |

---

## Open

**ISSUE-002 · S1 · nothing has run on the project laptop.** Every number in this package came from a
cloud sandbox (2 vCPU, 7 GB, Ubuntu 24.04, no Docker registry access) using the native fallback in
`scripts/native_up.sh`. HS3 asks for proof of repeated execution on the project's own machine, and the
compose path (`make up`) has never been executed. *Next step:* on the laptop, `make up && make
reproduce SCALE=0.1`, commit the new `bench/runs/` directory, and replace the sandbox figures in
`docs/HS3_TECHNICAL_MEMO.md` §2 with the laptop ones. This is the single item most likely to derail the
midpoint demo, because Docker Desktop on WSL2, the `cpuset` limits and the 3 GB-per-container budget
are all unverified on that machine.

**ISSUE-016 · S1 · Spike S-1 not run.** HS2 scheduled a one-day Hasura DDN spike for Week 5. It needs a
Hasura Cloud login and Docker image pulls, neither available in the sandbox. SC-6 depends on it only in
Week 11, so it does not block the midpoint demo, but it is still the highest-risk *external*
dependency. *Next step:* run it on the laptop this week; if the cross-connector PG → Mongo relationship
fails, trigger the GraphQL Mesh fallback and write ADR-0007.

**ISSUE-013 · S2 · Python planning overhead.** Pilot spans (`bench/runs/*pilot-W5/timings.csv`) show
`parse_plan` ≈ 3.4 ms median per query; arm A2 completes the whole W5 query in ≈ 1.0–1.1 ms at
t = 10 and t = 100. If this holds on the laptop, the small-k end of the crossover measures graphql-core
parsing, not federation. This is HS2 risk R18 appearing earlier than expected. *Options for the
supervisor:* (a) report as designed, with and without engine overhead (already planned); (b) add a plan
cache keyed on query text so repeated executions skip parse/validate/plan, and report both. (b) changes
what A1 measures, so it needs a decision, not a quiet fix.

**ISSUE-004 · S2.** `make lock-digests` exists but needs a machine that can pull images.
**ISSUE-006 · S2.** Ratios are round numbers chosen to fit memory. Replace before any reported run.
**ISSUE-017 · S2.** T-H1 needs container network counters; the native setup has no containers. Implement
on the laptop with `docker exec <svc> cat /sys/class/net/eth0/statistics/tx_bytes`.
**ISSUE-012 · S2.** `.github/workflows/ci.yml` follows HS2 §8.4 but has not run. First push will show.

**ISSUE-019 · S2 · R2 on same-store joins.** HS2 §6.3 defines R2 for "Equi-Join across stores". The
engine applies it to every declared join because same-store join pushdown is out of scope, so without
R2 a same-store traversal (W8) would scan the child table whole. Results are unaffected (oracle green),
but the A1/A4 contrast now includes same-store joins. Needs an ADR-0006 amendment.

**ISSUE-018 · S3 · rule order.** R3 (projection) runs last so it can add the fields of any residual
sort or filter. HS2 lists rules in the order R1, R1b, R2, R3, R4. No result changes; record in ADR.

**ISSUE-015 · S3.** At S = 0.1 there are 1,000 customers, so `sel < 10` selects about 1 customer in
expectation and selected 0 for seed 1. The nine HS2 sweep points assume S = 1 (10,000 customers).

**ISSUE-005 · S3.** χ² with 100 bins needs ≥ 5 expected per bin. The checks report SKIPPED, never PASS,
below that. CI runs at S = 0.01, so CI never exercises them; `make validate` at S ≥ 0.05 does.

**ISSUE-022 · S3.** Set `DB_CPUSET`/`DB_CPUS` in `.env` after running `lscpu` on the laptop (HS2 §8.2).

**ISSUE-001 · S3.** GitHub was not linked to the build session. Push from the laptop.

## Workarounds in place

**ISSUE-003 · S2 · no Docker images in the sandbox.** Docker Hub, `fastdl.mongodb.org` and
`repo.mongodb.org` were refused by the sandbox egress policy; PyPI and GitHub were reachable.
Workaround: two native PostgreSQL 16 clusters plus FerretDB 1.24 (a MongoDB wire-protocol server
backed by PostgreSQL, downloaded from GitHub releases). The Mongo adapter talks to it with the same
pymongo code it will use against MongoDB 7.0. Consequences: correctness evidence (oracle, property
tests) is meaningful because it compares answers; latency evidence is not (ISSUE-014). The manifest
records `setup: sandbox-native` so no sandbox run can be mistaken for a measurement.

**ISSUE-014 · S2.** In the pilot, the A1 Mongo span is ≈ 1.57 s at both k = 4 and k = 96. A bind join
looking up 4 keys through an index should not cost the same as 96. FerretDB 1.x evaluates this `$match`
without the index. Bytes and row counts are unaffected and are reported; timings are not.

## Fixed

**ISSUE-010 · S1 · capability gap at run time.** *Found by:* first oracle run (W3). FerretDB rejects
`$avg` inside `$group`. Two defects showed up: the error surfaced at run time, which HS2 §4.6 forbids
(capability gaps must be caught at plan time), and it was reported as `BACKEND_UNAVAILABLE`, which is
wrong (the backend was up). *Fix:* adapters now declare capabilities at connect time (`agg_fns`);
R1b consults them and leaves an unsupported aggregate residual, which is always safe. Driver errors
other than connection failures now map to `INTERNAL`. Tests: `test_r1b_respects_declared_capabilities`.

**ISSUE-011 · S1 · oracle false divergence.** *Found by:* oracle at S = 0.1 (green at S = 0.01). Two W3
groups had averages 415482 and 415481.67. PostgreSQL renders an integral `float8` as the JSON integer
`415482`; the engine holds `415482.0`; the order-insensitive sort key serialised them differently, so
rows were paired with the wrong partner and every field "differed". The engine was right. *Fix:* the
sort key normalises floats to 9 significant digits and integral floats to int; values are still compared
with relative tolerance 1e-9. Regression test: `test_compare_integral_float_vs_int_regression`. Lesson:
an oracle that only ever ran at the smallest scale had a blind spot; the oracle now runs at two scales.

**ISSUE-024 · S2 · byte counting inside the timed region.** *Found by:* reviewing the adapter while
writing this log, not by a test. The PostgreSQL adapter counted payload bytes by reading every cell
after the fetch; that was outside the `backend`/`decode` spans but inside end-to-end latency, inflating
A1/A4 in proportion to rows returned. The first pilot run (`20261007T211147Z-43aea8bf`) has this
defect and is superseded. *Fix:* counting is switchable (`PostgresAdapter.count_bytes`); the bench
turns it off for timed repetitions and takes bytes from the untimed oracle-gate pass at each point.

**ISSUE-009 · S2.** Normalised in the Mongo adapter. Not yet exercised against real MongoDB 7.0.
**ISSUE-007 · S2.** All string comparisons and sorts in PostgreSQL use `COLLATE "C"`. No workload
template sorts by a string yet, so add one before calling this verified.
**ISSUE-008 · S3.** Added `GRAPHQL_VALIDATION_FAILED` to the error contract (`engine/errors.py`).
**ISSUE-020 · S3.** Hypothesis found two bugs in `tests/db/test_property.py`: a variable declared but
unused when no country filter was drawn, and an ordered comparison of a nested list that had no defined
order. Both were test defects; the engine answers were correct in both cases.
**ISSUE-021 · S3.** `.python-version` and `requires-python = ">=3.12,<3.13"`; `uv` installs 3.12.
