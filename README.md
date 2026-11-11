# polydb

polydb is a multi-model database engine written in Rust. One engine and one transactional store hold relational tables, JSON documents and key-value pairs, and a single transaction can change all three atomically.

| Model | Language | Example |
|---|---|---|
| Relational | SQL | `SELECT name FROM users WHERE age > 40` |
| Document | MongoDB-style calls | `db.events.find({type: 'login'})` |
| Key-value | Redis-style commands | `SET session:42 abc` |

It runs as an interactive shell, as a library inside a Rust program, or as a network server. The server speaks its own line protocol and, on separate ports, subsets of the Redis, PostgreSQL and MongoDB wire protocols, so `redis-cli`, `psql` and the standard Python drivers can connect unchanged.

**Status:** version 0.2.0, a course capstone artifact. It is a working research prototype, not a production database: all data is held in memory, and there is no authentication. See [Known limits](#known-limits).

## Prerequisites

| Need | Version verified | Required for |
|---|---|---|
| Rust toolchain (`cargo`, `rustc`) | 1.97.0 | everything |
| Python 3 with `venv` | 3.13 | optional driver check only |
| Network access to PyPI | | optional driver check only (first run) |
| Docker | not verified, see [Docker](#docker) | optional container build only |

polydb has **no third-party Rust dependencies**, so building needs no network access and no system libraries beyond a C linker (installed with Rust on macOS and by `build-essential` on Debian/Ubuntu). Any 64-bit Linux, macOS or Windows machine with 2 CPU cores and 1 GB of free memory is enough. Only Linux x86-64 has been verified; the helper scripts need `bash`, so on Windows use WSL or run the `cargo` commands directly.

Run `bash scripts/check_env.sh` to see what your machine has.

## Quick start

Five commands, about one minute. Every command is run from this directory.

```sh
bash scripts/check_env.sh          # 1. confirm Rust is installed
cargo build --release --locked     # 2. compile (about 20 s)
cargo test --release --locked      # 3. run the tests
bash scripts/smoke.sh              # 4. scripted session compared with stored output
./target/release/polydb --memory   # 5. open a shell (type .help, then .exit)
```

`make build`, `make test` and `make smoke` do the same things if you have `make`.

### Expected output

Step 3 prints one `test result` line per test target. The three that matter are:

```
test result: ok. 23 passed; 0 failed; 0 ignored; ...     (tests/hardening.rs)
test result: ok. 22 passed; 0 failed; 0 ignored; ...     (tests/integration.rs)
test result: ok. 1 passed; 0 failed; 0 ignored; ...      (Doc-tests polydb)
```

Two more lines say `0 passed` for the library and the binary; that is normal, because all tests live in `tests/`.

Step 4 ends with:

```
SMOKE OK: output matches tests/fixtures/smoke.expected (64 lines)
```

Step 5 gives a prompt. A first session looks like this:

```
polydb> CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL)
table 'users' created
polydb> INSERT INTO users VALUES (1, 'Ada')
1 row(s) inserted
polydb> db.events.insert({_id: 1, user: 1, type: 'signup'})
1
polydb> SET last_user 1
OK
polydb> SELECT * FROM users
 id | name
----+------
 1  | Ada
(1 row(s))
```

The full scripted session and its exact output are in [`tests/fixtures/smoke.commands`](tests/fixtures/smoke.commands) and [`tests/fixtures/smoke.expected`](tests/fixtures/smoke.expected).

## Ways to run it

| Goal | Command | Data kept? |
|---|---|---|
| Try it, nothing on disk | `./target/release/polydb --memory` | no |
| Shell on a database directory | `./target/release/polydb mydata` | yes, in `./mydata` |
| Run a script of commands | `./target/release/polydb --memory < file` | no |
| Network server | `./target/release/polydb serve mydata` | yes |
| Shell connected to a server | `./target/release/polydb client` | on the server |
| Server from a config file | `bash scripts/serve.sh` | per config |

With no directory argument, the shell and the server use `./polydb-data`. `./target/release/polydb --help` lists every command and option. Command syntax for all three models, the library API and the protocol details are in [docs/REFERENCE.md](docs/REFERENCE.md).

## Configuration

polydb reads no environment variables and no configuration file; everything is a command-line flag.

```sh
polydb serve [DIRECTORY] [--memory] [--host ADDR] [--port N] [--redis-port N] [--pg-port N] [--mongo-port N]
```

| Flag | Default | Meaning |
|---|---|---|
| `DIRECTORY` | `polydb-data` | where `data.wal` and `data.snap` are stored |
| `--memory` | off | keep nothing on disk |
| `--host` | `127.0.0.1` | address to bind |
| `--port` | `7878` | native protocol |
| `--redis-port` | `6380` | Redis protocol |
| `--pg-port` | `5433` | PostgreSQL protocol |
| `--mongo-port` | `27018` | MongoDB protocol |

A port value of `off` disables that listener. The default ports are one above the real products' defaults, so polydb can run beside them.

For a repeatable setup, copy [`config/polydb.env.example`](config/polydb.env.example) to `config/polydb.env`, edit it, and run `bash scripts/serve.sh`, which turns the file into the flags above and prints the exact command it runs.

## External interfaces

polydb calls no external service and needs no API key, token or account. It is itself the service; these are the interfaces it exposes.

| Port | Protocol | Connect with | Verified against |
|---|---|---|---|
| 7878 | native: one command per line in, one JSON object per line out | `polydb client`, `nc 127.0.0.1 7878` | built-in client |
| 6380 | Redis (RESP2/RESP3) | `redis-cli -p 6380` | redis-cli 7.0, redis-py 8.1.0 |
| 5433 | PostgreSQL v3, simple-query flow | `psql -h 127.0.0.1 -p 5433` | psql 16, psycopg2 2.9.13 |
| 27018 | MongoDB (OP_MSG) | `mongodb://127.0.0.1:27018/?directConnection=true` | pymongo 4.19.0 |

- **Credentials:** there are none. No port has authentication or TLS. Clients may send any user name, password or database name; all are ignored. Keep `--host 127.0.0.1` unless every machine on the network is trusted.
- **Rate limits:** none. One thread per connection; commits are applied one at a time.
- **Compatibility:** the wire protocols are real, but the languages are polydb's subsets (for example, Redis strings only, no PostgreSQL prepared statements). The exact lists are in [docs/REFERENCE.md](docs/REFERENCE.md#wire-protocol-subsets).

## Driver check (optional)

`tests/drivers.py` runs 39 checks through unmodified redis-py, psycopg2 and pymongo, including concurrent clients and one transaction read back through three protocols.

```sh
bash scripts/driver_checks.sh      # or: make drivers
```

The script creates `.venv/`, installs the pinned versions in [`tests/requirements-drivers.txt`](tests/requirements-drivers.txt), starts an in-memory server, runs the checks and stops the server. It ends with `all driver checks passed`. Ports 6380, 5433 and 27018 must be free because the test file uses them directly.

## Docker

```sh
docker build -t polydb:0.2.0 .
docker run --rm -p 7878:7878 -p 6380:6380 -p 5433:5433 -p 27018:27018 -v polydb-data:/data polydb:0.2.0
```

The build compiles polydb, runs the Rust tests and the smoke comparison, and fails if any of them fail. **The Dockerfile has not been built yet**: the environment used for the reproducibility pass had no Docker daemon. Treat it as untested until someone runs the two commands above; see [docs/REPRODUCIBILITY.md](docs/REPRODUCIBILITY.md).

## Tests

| Command | What it covers | Count |
|---|---|---|
| `cargo test --release --locked` | correct behaviour of all three models, transactions, recovery (`tests/integration.rs`) | 22 |
| | failure paths: bad arguments, damaged files, port conflicts, garbage and deeply nested input on every port, killing the server (`tests/hardening.rs`) | 23 |
| | library example in `src/lib.rs` compiles | 1 |
| `bash scripts/smoke.sh` | scripted session equals stored output | 64 lines |
| `bash scripts/driver_checks.sh` | third-party drivers | 39 checks |
| `bash scripts/clean_run.sh` | all of the above in a fresh copy, with a log in `docs/evidence/` | |

Tests are deterministic: fixed inputs, a fixed-seed byte generator for the garbage-input test, operating-system-assigned free ports, and no dependence on the clock.

## Data

polydb ships with **no dataset** and downloads none. Every test creates its own small synthetic records in the test source (names such as "Ada" and "Linus"), so there is nothing to prepare and nothing that cannot be redistributed. A database directory you create holds only what you put into it. Details, including the on-disk files and what not to commit, are in [docs/DATA.md](docs/DATA.md).

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `cargo: command not found` | Rust is not installed | install from <https://rustup.rs>, open a new terminal |
| `error: linker 'cc' not found` | no C linker | Debian/Ubuntu: `sudo apt install build-essential`; macOS: `xcode-select --install` |
| `error: the lock file ... needs to be updated but --locked was passed` | `Cargo.toml` was edited | undo the edit, or drop `--locked` |
| `cannot listen on 127.0.0.1:6380: Address already in use` | another program (often a real Redis, or a previous polydb) has the port | stop it, or pass another port, for example `--redis-port 6390`, or `--redis-port off` |
| `port 5433 is already in use` from `driver_checks.sh` | same, for the driver check | stop the other program; the driver test needs the default ports |
| `cannot connect to 127.0.0.1:7878: Connection refused` from `polydb client` | no server is running | start one with `polydb serve` in another terminal |
| `cannot open database at 'x': ...` | the path is a file, or not writable | give a directory you can write to |
| `corrupt data: snapshot ... failed its checksum` | `data.snap` is damaged | restore the directory from a backup; polydb refuses to load a damaged snapshot |
| Data missing after restart | the server was started with `--memory`, or from a different working directory (the default `polydb-data` is relative) | pass an absolute directory path |
| `psql` shows `ERROR: polydb supports the simple query protocol only` | the client used prepared statements | use `psql`, psycopg2, or a driver's simple-query mode |
| `python3 -m venv` fails with `ensurepip is not available` | Debian/Ubuntu split package | `sudo apt install python3-venv` |
| `pip` cannot reach PyPI | offline or behind a proxy | skip the driver check: `bash scripts/clean_run.sh --skip-drivers` |
| `expression is nested deeper than 32 levels` or `expression is too complex` | a SQL expression past the nesting limits | split the expression; see [Known limits](#known-limits) |
| `nesting is deeper than 128 levels` or `document nesting is deeper than 100 levels` | JSON or a document past the nesting limits | flatten the document |
| `SMOKE FAILED` with a diff | the output format changed | read the diff; if the change is intended, regenerate `tests/fixtures/smoke.expected` and review it |

## Known limits

- **All data is held in memory**; the disk is used for durability only. Startup time grows with data size.
- **No authentication, TLS or replication.** Bind to localhost or a trusted network.
- **Nesting limits.** A SQL expression may nest 32 levels (parentheses, `NOT`, unary minus, function calls) and contain 100 chained operators and nested sub-expressions in total. JSON text may nest 128 levels and a stored document 100. Anything deeper is an error. These limits are what fixed DEFECT-01, where deeply nested input overflowed the stack and stopped the process; see [docs/REPRODUCIBILITY.md](docs/REPRODUCIBILITY.md).
- **Large-message allocation (open):** the Redis and PostgreSQL listeners accept a declared message length of up to 512 MB and 256 MB and allocate it before reading.
- **Log damage:** recovery stops at the first log record that fails its checksum. A torn final record (the normal crash case) is handled correctly; damage in the middle of the log also drops the commits after it.
- Commits wait for their own `fsync`, so write throughput is bounded by the disk.
- Indexes serve equality only; range queries and joins scan.
- SQL omits subqueries, `UNION`, `ALTER TABLE`, foreign keys, `UNIQUE`, defaults, composite keys and date/time types.
- No lock file: do not open the same directory from two processes at once.
- Object keys in documents are stored sorted, not in insertion order.

## Repository layout

```
Cargo.toml, Cargo.lock      package manifest and lock file (no dependencies)
src/                        the engine (about 5,600 lines)
  storage.rs keys.rs          ordered map, write-ahead log, snapshot, key layout
  sql.rs doc.rs kv.rs         the three data models
  lib.rs value.rs error.rs    sessions, transactions, value type, errors
  server.rs resp.rs           native and Redis listeners
  pgwire.rs mongo.rs          PostgreSQL and MongoDB listeners
  main.rs                     command-line shell, server and client
tests/
  integration.rs              22 behaviour tests
  hardening.rs                23 failure-path tests
  drivers.py                  39 checks through third-party drivers
  requirements-drivers.txt    pinned driver versions
  fixtures/                   smoke script and its expected output
scripts/                    check_env, smoke, driver_checks, serve, clean_run
config/polydb.env.example   server configuration template
Dockerfile, Makefile        container spec and task shortcuts
.github/workflows/ci.yml    smoke-test workflow
docs/
  RUNBOOK.md                  how to operate it, with the architecture note
  ENVIRONMENT.md              versions, hardware, configuration map
  DATA.md                     data sources, on-disk files, redistribution
  REPRODUCIBILITY.md          clean-run record, friction points, open defects
  AUDIT.md                    what the repository audit found and changed
  EVIDENCE.md                 file manifest with checksums
  REFERENCE.md                command syntax, library API, protocol subsets
  WALKTHROUGH.md              script for the recorded walkthrough
  evidence/                   clean-run logs
```

## License

MIT. See [LICENSE](LICENSE). The driver libraries used by the optional check are installed from PyPI under their own licenses and are not redistributed here.
