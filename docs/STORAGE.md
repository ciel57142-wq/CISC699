# Storage and durability

This document describes how polydb keeps data on disk, what it guarantees, how it was audited, and what it still does not guarantee. The code is `src/storage.rs`; the tests are `tests/persistence.rs` (separate-process restart tests), `tests/recovery.rs`, `tests/integration.rs` and `tests/hardening.rs`.

## Summary

- **The default is durable.** `polydb`, `polydb DIRECTORY`, `polydb serve` and `polydb serve DIRECTORY` all use a database directory (`./polydb-data` when none is given). `Database::open` in the library does the same.
- **`--memory` is explicitly memory-only.** It creates no files at all, in the shell and in the server. Combining `--memory` with a directory in the shell is refused rather than guessed.
- **An acknowledged commit is on stable storage.** Each commit is one checksummed record appended to `data.wal` and fsynced (`fdatasync`) before the client is told it succeeded. No `CHECKPOINT` is needed for persistence.
- **All three models, and all metadata, share the one log.** Rows, documents, key-value entries, table schemas, collection markers, secondary-index definitions and entries, and id sequences are all keys in one ordered keyspace (`src/keys.rs`), so they are written, recovered and checkpointed by the same code path. There is no second storage system.

## Files in a database directory

| File | Written | Content |
|---|---|---|
| `polydb.format` | at creation, and when the first snapshot appears (atomic rename) | magic line `polydb-storage`, `version=N`, `snapshot=0/1` |
| `LOCK` | at every open | empty; holds an exclusive OS file lock while a process has the database open |
| `data.wal` | every commit (append + fsync) | write-ahead log: one record per commit |
| `data.snap` | by a checkpoint (temp file + fsync + rename + directory fsync) | full image of the keyspace, same record layout |
| `*.tmp` | only during a checkpoint or format-file rewrite | removed automatically at the next open |
| `data.wal.corrupt` | only when a damaged log is opened with `salvage` | copy of the damaged log, for inspection |

Record layout (unchanged from the first release, which is why older directories open as-is):

```
record  = len:u32le | crc32(payload):u32le | payload
payload = count:u32le | { tag:u8 (1 put, 0 delete) | klen:u32le | key | [vlen:u32le | value] } * count
```

### Format versioning

`polydb.format` records the on-disk format version (currently `1`, defined as `storage::FORMAT_VERSION`). A directory with a higher version is refused with a message to use a newer polydb. A directory written by an earlier release (it has `data.wal` but no format file) is adopted as version 1: polydb writes the format file and leaves the log and snapshot byte-for-byte unchanged.

## Lifecycle

### Opening (and creating)

1. If the path does not exist it is created and its parent directory is fsynced. If it exists and is not a directory, opening fails.
2. `LOCK` is locked exclusively (`File::try_lock`, retried for up to half a second so a handle being dropped in another thread can finish). If another process or handle still holds it, opening fails with `io error: database directory ... is locked by another process`. The lock is released explicitly when the store is dropped, and by the OS when the process exits or is killed.
3. The directory is classified:
   - **Initialised** (`polydb.format` present): the version must be supported, `data.wal` must exist, and `data.snap` must exist if a snapshot was recorded. Anything missing is a `corrupt data` error: polydb never silently starts an empty database in place of one that failed to load.
   - **Earlier release** (`data.wal`, no format file): adopted as described above.
   - **New**: only allowed if the directory is empty (apart from the lock file, leftovers of an interrupted initialisation, and a mount point's `lost+found`). A directory with any other file is refused, so pointing polydb at the wrong place cannot bury files or "initialise over" anything. Creation writes and fsyncs an empty `data.wal`, fsyncs the directory, then writes `polydb.format` atomically. A crash between those steps leaves a log without a format file, which the next open adopts.
4. `data.snap` (if any) is loaded and must be entirely intact; one bad checksum is a `corrupt data` error.
5. `data.wal` is replayed. Its ending is classified:
   - *clean*: every byte was an intact record;
   - *torn tail*: the last record is incomplete, or ends exactly at end-of-file with bad contents, or the tail is zero-filled. That is what a crash during an append leaves. The commit was never acknowledged, so the tail is truncated and fsynced. The shell and server print `recovery: discarded N byte(s) ...` on stderr; the library reports it through `Database::recovered_torn_bytes()`;
   - *corrupt*: a bad record followed by further non-zero data. A crash cannot cause this (commits are appended and fsynced one at a time), and truncating would drop acknowledged commits, so opening fails with `corrupt data: write-ahead log ...: checksum mismatch in the record at byte N ...` and the file is left untouched. `Database::open_with(dir, &OpenOptions { salvage: true })` deliberately keeps the intact prefix, after saving the damaged log as `data.wal.corrupt`.

### Committing

1. The transaction's write set is framed as one record and appended to `data.wal`.
2. The log is fsynced (`sync_data`) unless the embedder turned that off with `Database::set_sync(false)`.
3. Only then is the change applied to the in-memory map and the commit acknowledged.

Failure handling:

- If the append fails (for example the disk is full), the partial record is cut off with `set_len` + fsync and the commit returns the I/O error. The database stays usable.
- If that rollback also fails, or if the fsync fails, the on-disk state of the log is unknown (after a failed fsync Linux may drop the dirty pages and report success later). The commit returns an error and the database **refuses all further writes** (`storage error: writes are disabled after an earlier storage failure ...`) until it is reopened, when recovery re-reads what is really on disk. Reads keep working.

### Checkpoints

A checkpoint writes the whole keyspace to `data.snap.tmp`, fsyncs it, renames it over `data.snap`, fsyncs the directory, records `snapshot=1` in `polydb.format` (first time only), and only then truncates and fsyncs `data.wal`. A crash at any point recovers correctly: before the rename the old snapshot plus the full log are used; after it, replaying the not-yet-truncated log over the new snapshot is idempotent because records hold whole values.

Checkpoints run automatically when the log passes 64 MB (`Database::set_checkpoint_threshold` changes this). They exist only to bound the log size and the start-up time; durability never depends on them. If an automatic checkpoint fails, the commit that triggered it still succeeds (it is already durable in the log), a warning is printed to stderr, the error is kept (`Store::last_checkpoint_error`), and the checkpoint is retried after another threshold's worth of log.

## Audit findings

The audit was made against the store as it stood before the durability work. S-2 and S-3 were fixed independently on the same branch (commit `d6b0f63`, "classify WAL tails, directory lock") while this work was in progress; the rest are fixed by this change.

| # | Finding | Severity | Resolution |
|---|---|---|---|
| S-1 | The keyspace lives in RAM as a `BTreeMap`; disk is the log + snapshot. That design was already durable (fsync per commit), so it was kept rather than replaced. | design | documented: the map is a cache rebuilt from disk at open |
| S-2 | Damage in the middle of `data.wal` was treated as a torn tail: everything after it was truncated, silently losing acknowledged commits. | high | refused with a `corrupt data` error; explicit `salvage` option (`d6b0f63`) |
| S-3 | No lock: two processes could open one directory and interleave appends, corrupting the log. | high | exclusive OS lock on `LOCK` (`d6b0f63`) |
| S-4 | Opening any directory created `data.wal` in it, including non-empty directories that were not polydb databases. | medium | only initialise into a missing or empty directory |
| S-5 | A database whose `data.wal` or `data.snap` had gone missing opened as empty (or partial), with no error. | high | `polydb.format` marks initialisation and snapshot presence; missing files are errors |
| S-6 | No format version on disk. | medium | `polydb.format` with `version=1`; older directories adopted unchanged |
| S-7 | Creating `data.wal` and the database directory was not followed by a directory fsync, so the new file could vanish on power loss. | medium | parent and database directories fsynced after creation |
| S-8 | The directory fsync after the snapshot rename ignored errors (`let _ =`). | medium | error propagated (on Unix; other platforms cannot fsync a directory) |
| S-9 | A failed rollback after a failed log write was ignored (`let _ = set_len`), so later commits could be appended after a half-written record. A failed fsync left the database writable. | high | the database becomes read-only until reopened |
| S-10 | An automatic checkpoint failing inside `commit` made an already-durable commit report failure. | medium | the commit succeeds; the checkpoint failure is reported and retried |
| S-11 | `polydb DIRECTORY --memory` silently used the directory (only a *first* argument of `--memory` was honoured). | low | `--memory` is honoured anywhere and refused together with a directory |

Components holding authoritative state only in RAM after this change: none for committed data. The per-process commit counter and the list of open transactions (used for serializable conflict detection) are in RAM by design; open transactions are discarded when a process exits.

## Tests

`tests/persistence.rs` writes with one `polydb` process and reads back with another, so nothing can survive in the test's own memory:

| Test | Proves |
|---|---|
| `relational_rows_survive_process_restart` | inserts, updates and deletes of rows |
| `documents_survive_process_restart` | documents, updates, deletes, generated ids keep counting |
| `key_value_entries_survive_process_restart` | SET, INCR, DEL, escaped values |
| `schemas_survive_process_restart` | table list, column types, NOT NULL, primary keys, DROP TABLE |
| `indexes_survive_process_restart_and_stay_correct` | SQL and document index definitions persist and are maintained by writes in later processes; DROP INDEX persists; a recreated index is rebuilt completely |
| `reopening_many_times_preserves_state` | six consecutive processes, with a checkpoint in the middle |
| `committed_transactions_survive_and_open_ones_do_not` | multi-model COMMIT persists; ROLLBACK and an unfinished transaction do not |
| `default_launch_is_disk_backed` | `polydb` with no arguments writes `./polydb-data` and reads it back |
| `memory_mode_creates_no_files` | shell and server `--memory` create no files; nothing carries over |
| `automatic_checkpoints_keep_data_without_manual_checkpoint` | auto-checkpoint writes a snapshot, empties the log, data survives |
| `killed_server_keeps_every_acknowledged_write_of_all_models` | SIGKILL three times; rows, documents, keys and indexes intact |
| `kill_during_a_write_stream_loses_no_acknowledged_write` | SIGKILL mid-stream; every acknowledged insert present, no holes |
| `a_second_process_cannot_open_a_database_in_use` | lock refusal, and release when the holder dies |
| `never_initialises_over_a_directory_with_other_files` | foreign files are left alone, no database created |
| `a_missing_log_is_an_error_not_an_empty_database`, `a_missing_snapshot_after_checkpoint_is_an_error` | no silent empty database after a failed load |
| `a_newer_format_version_is_refused` | format versioning |
| `directories_from_polydb_0_2_0_open_unchanged` | directories without a format file open; their log is not rewritten |
| `an_interrupted_final_commit_is_discarded_and_reported` | torn-tail recovery and its stderr report |

`tests/recovery.rs` and `tests/hardening.rs` additionally cover interrupted checkpoints, mid-log damage being refused (and salvageable on request), a damaged snapshot being refused, and the lock being released when a killed server dies.

## Remaining durability limitations

1. **Power loss is not tested on real hardware.** Process kills (SIGKILL) are tested; crash consistency under power failure relies on the file system honouring `fsync`/`fdatasync` and directory fsync. Network file systems, some container volume drivers, and disks with volatile write caches that ignore flush requests can break the guarantee.
2. **Non-Unix directory fsync.** On Windows a directory cannot be fsynced, so the creation of new files and the snapshot rename rely on NTFS metadata journaling. Only Linux has been verified.
3. **The whole dataset must fit in memory.** The keyspace is held in RAM and rebuilt at open; start-up time grows with the snapshot plus log size. There is no page cache or eviction.
4. **Checkpoints are full rewrites** that hold the database's write lock while the snapshot is written, so writers stall for the duration on large databases.
5. **One fsync per commit, no group commit.** Throughput is bounded by the device's flush latency. `set_sync(false)` (library only) trades the last commits on power loss for speed; it is never the default.
6. **Ambiguous outcome after a failed fsync.** The client gets an error, but the record may already be on disk and reappear after restart (this is inherent to fsync semantics). The database refuses writes until reopened so it never builds on an unknown state.
7. **A corrupted length field can look like a torn tail.** If damage to the *last* record's header makes it claim to extend past end-of-file, it is indistinguishable from an interrupted append and is truncated. Damage anywhere else is detected and refused.
8. **No repair tool beyond salvage.** A damaged snapshot is refused; a damaged log is refused unless opened with `salvage`, which keeps only the commits before the damage. Otherwise recovery means restoring the directory from a backup. There is no online backup command: copy the directory while no process has it open (the lock file makes concurrent opening impossible, not copying).
9. **CRC32 detects accidental damage only**, not deliberate tampering.
10. **The lock needs file-lock support.** On file systems without it, opening fails with an explanatory error rather than running unprotected.
