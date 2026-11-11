# Data

## Summary

polydb is a database engine, not a data analysis. It ships with no dataset, downloads none, and needs none to be built, tested or demonstrated.

| Question | Answer |
|---|---|
| Where does the data come from? | Every record used by the tests is written by hand in the test source. |
| How is it prepared? | It is not. Each test creates what it needs and removes it afterwards. |
| What cannot be redistributed? | Nothing in this package. See the note on driver libraries below. |
| Is any of it personal or sensitive? | No. Names such as "Ada", "Linus" and "Grace" are placeholder strings. |

## Test data, by location

| File | Data | Size |
|---|---|---|
| `tests/integration.rs` | tables `users` and `orders`, collections `events`, `p` and others, a few keys | under 20 records per test |
| `tests/hardening.rs` | small tables and keys; pseudo-random bytes from a fixed-seed generator (`noise`) for the garbage-input test; generated deeply nested SQL, JSON and BSON | up to about 1 MB per message |
| `tests/fixtures/smoke.commands` | 4 rows, 3 documents, 4 keys | 39 lines |
| `tests/drivers.py` | tables `users` and `stock`, collections `people` and `audit`, a few keys | under 20 records |

The generator in `tests/hardening.rs` is a linear congruential generator with constant seeds, so the "random" bytes are identical on every run and every machine.

## Files polydb writes

A database directory (default `./polydb-data`) holds at most three files:

| File | Content |
|---|---|
| `data.wal` | write-ahead log: one checksummed record per commit |
| `data.snap` | full snapshot, written by `CHECKPOINT` or when the log passes 64 MB |
| `data.snap.tmp` | exists only while a checkpoint is being written |

These are binary files in polydb's own format and contain whatever the user stored. They are listed in `.gitignore` and must not be committed. Tests that need files create them under the operating system's temporary directory in folders named `polydb-test-*` and `polydb-harden-*`, and delete them when the test passes. A failed test can leave its folder behind; it is safe to delete.

## Sources of nondeterminism

| Source | Where | Effect on reproducibility |
|---|---|---|
| Generated document `_id` | `src/doc.rs`: current time in milliseconds plus a sequence number | Ids differ between runs. Tests and the smoke script always supply `_id`, or check only the id's type. |
| MongoDB `ObjectId` values | created by pymongo on the client | Same as above. |
| Thread scheduling | concurrent-client tests | Tests assert only the final total, which is the same for every interleaving. |
| Free ports | chosen by the operating system (port 0) | Port numbers differ between runs; no output depends on them. |

Nothing in the engine uses a random number generator, so there is no seed to set.

## Third-party material

The optional driver check installs redis-py (MIT), psycopg2-binary (LGPL with exceptions) and pymongo (Apache 2.0) from PyPI into `.venv/`. They are used as installed, are not modified and are not included in this package; `.venv/` is ignored by git. Anyone handing the package on should leave `.venv/` out, which `scripts/clean_run.sh` and the manifest in [EVIDENCE.md](EVIDENCE.md) already do.
