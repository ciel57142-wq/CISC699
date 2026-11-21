# Repository audit

Audit of the package as received on 2026-11-11 (`polydb.zip`, 18 files), against the Hard Stop 5 checklist: missing files, undocumented dependencies, broken links, inconsistent naming. Each finding lists what was done. The audit itself changed nothing under `src/`; one defect found later by the hardening tests was then fixed there (DEFECT-01, see REPRODUCIBILITY.md).

## Baseline before any change

| Check | Result |
|---|---|
| `cargo build --release` | passed, 17 s |
| `cargo test --release` | 22 integration tests and 1 doc test passed |
| `tests/drivers.py` | could not be run as received: client libraries not installed, no pinned versions, server had to be started by hand |
| `cargo clippy --all-targets` | 13 style warnings, no errors |
| `cargo fmt --check` | formatting differences in most source files |

## Findings

### Missing files

| ID | Finding | Action |
|---|---|---|
| A-01 | `Cargo.toml` declares `license = "MIT"` but there was no `LICENSE` file. | Added `LICENSE` (MIT). The copyright holder is a placeholder to fill in. |
| A-02 | No `.gitignore`. `target/`, data directories and a Python environment would be committed or zipped by accident. | Added `.gitignore` and `.dockerignore`. |
| A-03 | No container spec, task runner or CI workflow. | Added `Dockerfile`, `Makefile`, `.github/workflows/ci.yml`. The Dockerfile and workflow are not yet executed; see REPRODUCIBILITY.md. |
| A-04 | No script for any multi-step procedure. The driver check needed three manual steps in two terminals. | Added `scripts/check_env.sh`, `smoke.sh`, `driver_checks.sh`, `serve.sh`, `clean_run.sh`. |
| A-05 | No configuration template. | Added `config/polydb.env.example`, read by `scripts/serve.sh`. |
| A-06 | No documentation beyond the README: no runbook, environment record, data statement or reproducibility record. | Added `docs/RUNBOOK.md`, `ENVIRONMENT.md`, `DATA.md`, `REPRODUCIBILITY.md`, `EVIDENCE.md`, this file, and `docs/REFERENCE.md`. |
| A-07 | No tests for failure paths (bad arguments, damaged files, hostile network input, process kill). | Added `tests/hardening.rs` (23 tests) and `tests/fixtures/`. |
| A-08 | No version history in the package (the zip has no `.git` directory). | Recorded a file manifest with SHA-256 checksums in `docs/EVIDENCE.md`. |

### Undocumented dependencies

| ID | Finding | Action |
|---|---|---|
| A-09 | The Rust version was not stated anywhere. | Verified version (1.97.0) and the practical minimum (1.78, from the lock-file format) recorded in README and `docs/ENVIRONMENT.md`; checked by `scripts/check_env.sh`. |
| A-10 | Python driver versions appeared only as prose in the README ("redis-py 8.1", "psycopg2 2.9", "pymongo 4.19"); the install line in `tests/drivers.py` was unpinned. | Added `tests/requirements-drivers.txt` with exact versions; the script installs from it into `.venv/`. |
| A-11 | The need for a C linker, for `bash`, and for free ports 6380, 5433 and 27018 was not stated. | Added to README prerequisites and troubleshooting. |
| A-12 | `tests/drivers.py` hard-codes its three ports. | Left as is (kept the received test unchanged); `scripts/driver_checks.sh` checks the ports first and explains the conflict. Listed as an open item. |

### Broken or missing links and references

| ID | Finding | Action |
|---|---|---|
| A-13 | The received README had no links at all, so nothing was broken, but nothing pointed to the test files, the help text or the limits. | New README links every document and fixture; all relative links were checked by script and resolve. |
| A-14 | The README server usage line omitted the `--memory` flag that `polydb --help` and `tests/drivers.py` both use. | Added to the README configuration section. |
| A-15 | The README build section gave commands but no expected output and no troubleshooting. | Added both. |

### Inconsistent naming

| ID | Finding | Action |
|---|---|---|
| A-16 | The project is called "polystore" in course submissions; the crate, binary, zip and README say "polydb". | Documented: "polystore" is the capstone project name, "polydb" is the artifact name. No rename, since renaming the crate would change `src/`. |
| A-17 | Key-value `KEYS` takes a prefix in the native language (`KEYS session:`) but a glob over the Redis port (`KEYS user:*`). | Documented in the reference; behaviour unchanged. |
| A-18 | The PostgreSQL listener reports `server_version` 14.0 while the README describes it only as "PostgreSQL v3" (the protocol version). | Noted here; cosmetic, unchanged. |
| A-19 | Test temp folders were named `polydb-test-*`. | New tests use `polydb-harden-*` so the two files cannot collide; both documented in DATA.md. |

### Claims checked against the code

| Claim in the received README | Check | Result |
|---|---|---|
| "22 integration tests + 1 doc test" | ran `cargo test` | correct |
| "`tests/drivers.py` runs 39 checks" | ran it, counted `ok` lines | correct (39) |
| "about 5,500 lines" | `wc -l src/*.rs` | correct (5,465 as received; 5,599 after the DEFECT-01 fix) |
| "No dependencies" | `Cargo.toml`, `Cargo.lock` | correct |
| "Nothing acknowledged is lost on a crash" | new test kills the server process and reopens the directory | holds for process kill; power loss not testable here |
| "a client cannot exhaust memory" (comment in `server.rs`, native port cap of 64 MB) | read the other listeners | native port only; Redis and PostgreSQL allow 512 MB and 256 MB declared lengths (open item) |

## Not changed, by decision

- `src/`, apart from the DEFECT-01 fix (145 lines added, 11 changed, in `value.rs`, `sql.rs`, `doc.rs` and `mongo.rs`). The two observations in REPRODUCIBILITY.md are reported, not fixed.
- `tests/integration.rs` and `tests/drivers.py`.
- `Cargo.toml` and `Cargo.lock`.
- Formatting and lint warnings in `src/` (reported by `make lint`, advisory).
