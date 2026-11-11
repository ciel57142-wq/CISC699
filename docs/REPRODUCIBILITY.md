# Reproducibility record

What was run, in what environment, what went wrong along the way, and what is still unverified. The raw log is [`evidence/clean-run-2026-11-11.log`](evidence/clean-run-2026-11-11.log).

## Method

A peer-style simulation, automated in `scripts/clean_run.sh`:

1. Copy the package to a new temporary directory, leaving out `target/`, `.venv/`, data directories and earlier logs, so nothing built earlier can help.
2. In that copy, follow the README quick start in order: check prerequisites, build, test, smoke test, driver check.
3. Record every command, its full output, its duration and its exit status.
4. Report `CLEAN RUN PASSED` only if every step succeeded.

The same script is the way to repeat the pass on another machine: `bash scripts/clean_run.sh` (or `--skip-drivers` when offline).

**Limits of this method.** The copy is fresh, but the machine is not: the Rust toolchain and Python were already installed, and pip may have used its download cache. A different machine was not available, and the container build could not be run. This is a clean directory on a prepared machine, not a clean machine.

## Result

Run of 2026-11-11 on Ubuntu 24.04.5, x86-64, 2 cores, 7.8 GB, Rust 1.97.0, Python 3.13.18.

| Step | Command | Result | Time |
|---|---|---|---|
| 0 | fresh copy has no `target/` or `.venv/` | ok | 0 s |
| 1 | `bash scripts/check_env.sh` | ok | under 1 s |
| 2 | `cargo build --release --locked` | ok | 10 s |
| 3 | `cargo test --release --locked` | 23 + 22 + 1 passed, none ignored | 4 s |
| 4 | `bash scripts/smoke.sh` | output identical to `smoke.expected` (64 lines) | under 1 s |
| 5 | `bash scripts/driver_checks.sh` | 39 of 39 checks passed | 5 s |
| 6 | `cargo test --locked` (unoptimised build) | 23 + 22 + 1 passed | 7 s |

**RESULT: CLEAN RUN PASSED.** This is the run after the DEFECT-01 fix. In addition, the Rust test suite was run 10 times in a row after the fix (and 15 times before it) to look for timing-dependent failures; every run passed.

Step 6 repeats the tests without optimisation because stack frames are several times larger there, which makes it the stricter check of the nesting limits.

Durations are from the log of the final run and vary by a few seconds between runs.

## Friction points and fixes

Problems met while preparing and running the pass, in the order they appeared.

| ID | Friction or missing assumption | Fix |
|---|---|---|
| F-01 | **The driver check could not be run from the instructions as received.** They said "start a server, `pip install redis psycopg2-binary pymongo`, run the script": two terminals, unpinned versions, and a system-wide pip install that current Debian/Ubuntu Python refuses. | `scripts/driver_checks.sh` creates `.venv/`, installs pinned versions, starts and stops the server itself. |
| F-02 | **Fixed ports in `tests/drivers.py`.** If a real Redis, PostgreSQL on 5433, or MongoDB on 27018 is running, the checks talk to the wrong server or fail confusingly. | The script tests the three ports first and stops with a plain message. The test file itself is unchanged (open item O-3). |
| F-03 | **No expected output anywhere.** A newcomer could not tell whether `cargo test` output was a pass, and nothing showed what a session should print. | README "Expected output"; `tests/fixtures/smoke.expected`; `scripts/smoke.sh` compares automatically. |
| F-04 | **First draft of "Expected output" was wrong.** It said `cargo test` prints three result lines; the real run prints five (two say `0 passed` for the library and binary targets). Found by comparing the README with the log. | README corrected to explain the two extra lines. |
| F-05 | **`scripts/check_env.sh` printed an error for `nc`.** The first version asked every tool for `--version`; netcat rejects that flag. | `nc` is now only reported as installed or not. |
| F-06 | **Pinning the Rust toolchain broke the offline case.** A `rust-toolchain.toml` with an exact version makes cargo download that toolchain; the verification environment could not download toolchains at all, so the pin would have made a working machine fail. | No toolchain file. The version is pinned in the Dockerfile and CI, and checked (with a note, not a failure) by `check_env.sh`. |
| F-07 | **Rust version floor was unknown.** Nothing stated a minimum version. | `Cargo.lock` format 4 sets the floor at Rust 1.78; recorded in ENVIRONMENT.md. Older versions could not be tested (same download limit as F-06). |
| F-08 | **No Docker daemon in the verification environment.** The client is installed but nothing can be built. | Dockerfile written but marked unverified everywhere it is mentioned (open item O-1). |
| F-09 | **Relative default data directory.** `polydb serve` with no directory uses `./polydb-data`, so starting from another folder shows an empty database. Noticed when `scripts/serve.sh` created `polydb-data/` in the repository root. | Runbook and troubleshooting say to use an absolute path; `polydb-data/` added to `.gitignore`. |
| F-10 | **Deeply nested input stopped the process** (found by the new hardening tests; see DEFECT-01). | Fixed with nesting limits in the parsers; five regression tests. The first choice of limits was itself too generous for unoptimised builds and was lowered after measurement. |
| F-11 | **Formatting and lint are not clean.** `cargo fmt --check` and `cargo clippy` report differences and 13 style warnings in `src/`. A CI job that enforced them would fail on day one. | CI does not gate on them; `make lint` reports them as advisory (open item O-5). |
| F-12 | **Project name mismatch.** Course documents say "polystore", every file says "polydb". | Explained in AUDIT.md (A-16) and the report. |

## Defects found

### DEFECT-01: unbounded recursion stopped the process (fixed)

- **What happened.** A SQL expression or JSON value nested thousands of levels deep overflowed the stack. Rust aborts the whole process on a stack overflow (exit code 134, message `has overflowed its stack`), so in server mode one client sending one line stopped the server for every client. Committed data was never at risk: the write-ahead log was intact and the next start recovered everything.
- **Measured before the fix.** SQL with 5,000 nested parentheses and JSON with 100,000 nested arrays aborted the shell; 1,000 levels of either were handled. A 100,000-level JSON line sent to the native port stopped a running server.
- **Cause.** Recursion without a depth limit in five places: the JSON parser (`src/value.rs`), the SQL expression parser and evaluator (`src/sql.rs`, where a long chain such as `1+1+1+...` builds a tree as deep as the chain is long), the BSON decoder on the MongoDB port (`src/mongo.rs`), and document updates with a long dotted path (`src/doc.rs`), which could build a document too deep to read back.
- **Fix.** Explicit limits, each returning an ordinary error:

| Limit | Value | Error |
|---|---|---|
| SQL nesting (parentheses, `NOT`, unary minus, function calls) | 32 levels | `parse error: expression is nested deeper than 32 levels` |
| SQL chained operators plus nested sub-expressions, per expression | 100 | `parse error: expression is too complex: ...` |
| JSON text | 128 levels | `parse error: JSON: nesting is deeper than 128 levels` |
| Stored document | 100 levels | `type error: document nesting is deeper than 100 levels` |
| BSON from a MongoDB client | 100 levels | connection closed |

  Items of an `IN` list or a function's arguments sit side by side and do not add up, and each `VALUES` row is its own expression, so long lists and large inserts are unaffected (tested with 5,000 and 1,998 items).
- **How the values were chosen.** A server connection thread has a 2 MB stack. The deepest inputs the limits still accept were run with the stack restricted: the unoptimised build needs less than 1 MB and the optimised build less than 256 KB. A first attempt (64 levels, 400 operators) passed in the optimised build but overflowed in the unoptimised one, and stored documents at the JSON limit could not be read back through the native protocol, whose reply adds two levels. Both were caught by the new tests and corrected.
- **Regression tests** (`tests/hardening.rs`): `extremely_nested_json_does_not_crash_the_process`, `extremely_nested_sql_does_not_crash_the_process` (six shapes, 200,000 levels each), `nesting_limits_are_enforced_at_their_documented_values`, `updates_cannot_create_documents_too_deep_to_read_back`, `deeply_nested_input_over_the_network_does_not_stop_the_server` (native and MongoDB ports).
- **Size of the change.** 145 lines added and 11 changed in four source files. All 22 original tests and all 39 driver checks still pass.

### Observation: large declared message lengths (open)

The Redis listener accepts a declared argument length of up to 512 MB and the PostgreSQL listener a message length of up to 256 MB, and each allocates that much before reading. A few connections could exhaust memory. Found by reading `src/resp.rs` and `src/pgwire.rs`; not exercised by a test because the test would itself need that memory. Suggested fix: lower the limits, or read in chunks.

### Observation: damage in the middle of the log (open)

Recovery treats the first record that fails its checksum as the end of the log. That is correct for a crash (only the last record can be torn) but means damage in the middle of `data.wal` silently drops later commits. `damage_in_the_middle_of_the_log_keeps_everything_before_it` records the current behaviour. Suggested improvement: if valid records follow a bad one, refuse to open and report corruption instead of truncating.

## What was not verified

| ID | Item | How to close it |
|---|---|---|
| O-1 | `Dockerfile` has never been built. The image tag `rust:1.97` is assumed to exist. | `docker build -t polydb:0.2.0 .` on a machine with Docker; fix the tag if needed. |
| O-2 | `.github/workflows/ci.yml` has never run. | Push to a GitHub repository and check the first run. |
| O-3 | `tests/drivers.py` ports are fixed. | Read the ports from environment variables with the current values as defaults. |
| O-4 | Only Linux x86-64 and Rust 1.97.0 were tested. | Run `bash scripts/clean_run.sh` on macOS, on arm64, and with an older toolchain. |
| O-5 | `cargo fmt` and `cargo clippy` are not clean. | Run `cargo fmt` and `cargo clippy --fix` once, review, then make CI enforce both. |
| O-6 | A second person has not followed the README. | Give the zip to a classmate with only the README and note where they stop. |
| O-7 | Durability under real power loss. | Out of reach without hardware; process kill is tested. |
| O-9 | The two open observations above (large message lengths, mid-log damage). | Apply the suggested fixes. |
| O-10 | A document built through the Rust library API deeper than the stack allows is the embedding program's responsibility; the limits protect the parsers and the network ports. | Document for library users, or make the remaining walkers iterative. |
| O-8 | `LICENSE` has a placeholder copyright holder. | Fill in the name. |

## If this were handed to another graduate student next week

They could build, test and run it from the README without help; that path is the one the log records. What would still slow them down: the untested container (O-1), the driver test's fixed ports if they already run Redis or MongoDB (O-3), and the memory observation (O-9) if they expose the server to untrusted clients.
