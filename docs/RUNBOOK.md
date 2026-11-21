# polydb runbook

How to build, start, check, stop, back up and recover polydb. Commands are run from the repository root. For first-time setup, follow the [README quick start](../README.md#quick-start) first.

## Architecture note

```
  clients          polydb client / nc      redis-cli        psql           pymongo
                          |                    |              |               |
  listeners         native :7878         Redis :6380   PostgreSQL :5433  MongoDB :27018
  (one thread             \                    |              |               /
   per connection)         +-------------- one session per connection -------+
                                                   |
  data models                    SQL (sql.rs) | documents (doc.rs) | key-value (kv.rs)
                                                   |
  transactions                 write overlay + read tracking, validated at commit
                                                   |
  storage (storage.rs)         ordered in-memory map + write-ahead log + snapshot
                                                   |
  files in DIRECTORY                     data.wal            data.snap
```

One process, no external services. All four listeners share one in-memory ordered map. Each model stores its entries under its own key prefix, so a table, a collection and a key with the same name never collide. A commit is validated against concurrent commits, written to `data.wal` as one checksummed record, flushed to disk with `fsync`, then applied. `CHECKPOINT` (or a log over 64 MB) writes the whole map to `data.snap` and empties the log.

## Packaging flow

```
source (src/, tests/, Cargo.toml, Cargo.lock)
   |  cargo build --release --locked
   v
target/release/polydb            one self-contained executable, about 1.5 MB
   |  cargo test --release --locked      46 Rust tests
   |  scripts/smoke.sh                   golden-output comparison
   |  scripts/driver_checks.sh           39 third-party driver checks (optional)
   v
scripts/clean_run.sh             repeats all of the above in a fresh copy, writes docs/evidence/clean-run-<date>.log
   |
   v
hand-over: zip of the repository without target/, .venv/ and data directories
```

The executable has no runtime dependencies beyond the system C library, so it can be copied to another machine with the same operating system and CPU architecture and run there.

## Configuration map

| Setting | Flag | `config/polydb.env` key | Default |
|---|---|---|---|
| data directory | positional `DIRECTORY` | `POLYDB_DATA_DIR` | `polydb-data` |
| in-memory mode | `--memory` | empty `POLYDB_DATA_DIR` | off |
| bind address | `--host` | `POLYDB_HOST` | `127.0.0.1` |
| native port | `--port` | `POLYDB_PORT` | `7878` |
| Redis port | `--redis-port` | `POLYDB_REDIS_PORT` | `6380` |
| PostgreSQL port | `--pg-port` | `POLYDB_PG_PORT` | `5433` |
| MongoDB port | `--mongo-port` | `POLYDB_MONGO_PORT` | `27018` |

Two settings exist only in the library API, not on the command line: `Database::set_sync(false)` (skip `fsync` per commit) and the 64 MB checkpoint threshold.

## Procedures

### Build

```sh
cargo build --release --locked
```

Result: `target/release/polydb`. Takes 10 to 20 seconds on two cores.

### Start the server

```sh
./target/release/polydb serve /absolute/path/to/data
```

Expected output:

```
polydb 0.2.0 serving '/absolute/path/to/data'
native    listening on 127.0.0.1:7878
redis     listening on 127.0.0.1:6380
postgres  listening on 127.0.0.1:5433
mongodb   listening on 127.0.0.1:27018
```

Use an absolute path. A relative path is resolved against the current directory, so starting the server from a different directory silently opens a different, empty database.

### Check that it is healthy

```sh
echo "SHOW TABLES" | ./target/release/polydb client       # native port
redis-cli -p 6380 PING                                     # expect PONG
psql -h 127.0.0.1 -p 5433 -c "SHOW TABLES"                 # expect a table list
```

Any reply, including an empty list, means the server is up.

### Stop the server

Press Ctrl+C, or send `SIGTERM` (`kill <pid>`). There is no shutdown command and none is needed: every acknowledged commit is already on disk. Transactions that were open are discarded.

### Back up and restore

1. Stop the server (the files must not change while they are copied).
2. Copy the whole data directory: `cp -r /path/to/data /path/to/backup`.
3. To restore, stop the server, replace the directory with the copy, start the server.

Running `CHECKPOINT` before stopping makes the directory smaller and the next start faster.

### Recover after a crash or power loss

Start the server again on the same directory. Recovery is automatic: the snapshot is loaded, intact log records are replayed, and a half-written final record is removed. No command is needed.

If start-up fails with `corrupt data: snapshot ... failed its checksum`, the snapshot file is damaged. Restore the directory from a backup.

### Run the checks

| Check | Command | Passing output ends with |
|---|---|---|
| Rust tests | `cargo test --release --locked` | `test result: ok.` for each target |
| Smoke | `bash scripts/smoke.sh` | `SMOKE OK` |
| Drivers | `bash scripts/driver_checks.sh` | `all driver checks passed` |
| Everything, fresh copy | `bash scripts/clean_run.sh` | `RESULT: CLEAN RUN PASSED` |

### Change a default port because of a conflict

```sh
./target/release/polydb serve data --redis-port 6390     # move one listener
./target/release/polydb serve data --mongo-port off      # or turn it off
```

## Operating limits to know

- Memory use is roughly the size of the data; there is no limit setting.
- One process per data directory. There is no lock file to stop a second one.
- No authentication on any port. See [README, External interfaces](../README.md#external-interfaces).
- Input nesting is limited (SQL 32 levels and 100 operators per expression, JSON 128 levels, stored documents 100); deeper input is rejected with an error. See [REPRODUCIBILITY.md](REPRODUCIBILITY.md), DEFECT-01.
