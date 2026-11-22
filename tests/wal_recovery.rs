//! Write-ahead log, checkpoint and crash-recovery tests driven by
//! deterministic fail points.
//!
//! When an armed [`FailPoint`] fires, the store stops exactly there and
//! leaves behind whatever bytes it had already written, the way a killed
//! process would. Each test then reopens the directory and checks the two
//! acceptance rules:
//!
//! * every acknowledged commit survives recovery, and
//! * no incomplete transaction leaves any of its changes behind.

use polydb::storage::{
    decode_record, encode_record, Body, Decoded, LogTail, OpenOptions, Record, Store, WriteSet,
};
use polydb::{Database, Error, FailPoint, Faults, Output, Value};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions as FileOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

const WAL: &str = "data.wal";

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("polydb-walrec-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    p
}

fn ws(ops: &[(&str, Option<&str>)]) -> WriteSet {
    ops.iter()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.map(|v| v.as_bytes().to_vec())))
        .collect()
}

fn put(s: &mut Store, k: &str, v: &str) -> polydb::Result<()> {
    s.commit(ws(&[(k, Some(v))]))
}

fn get(s: &Store, k: &str) -> Option<String> {
    s.get(k.as_bytes())
        .map(|v| String::from_utf8(v.clone()).unwrap())
}

fn dump(s: &Store) -> Model {
    s.scan(b"").map(|(k, v)| (k.clone(), v.clone())).collect()
}

fn apply(m: &mut Model, w: &WriteSet) {
    for (k, v) in w {
        match v {
            Some(v) => {
                m.insert(k.clone(), v.clone());
            }
            None => {
                m.remove(k);
            }
        }
    }
}

fn open_faulty(dir: &Path, faults: Faults) -> polydb::Result<Store> {
    Store::open_with_faults(dir, &OpenOptions::default(), faults)
}

/// Decode every valid record of a log file, stopping at the first bad one.
fn records(path: &Path) -> Vec<Record> {
    let buf = fs::read(path).unwrap_or_default();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Decoded::Valid(r, n) = decode_record(&buf[pos..]) {
        out.push(r);
        pos += n;
    }
    out
}

/// Byte offset at which each record of a log file starts.
fn offsets(path: &Path) -> Vec<usize> {
    let buf = fs::read(path).unwrap();
    let mut out = Vec::new();
    let mut pos = 0;
    while let Decoded::Valid(_, n) = decode_record(&buf[pos..]) {
        out.push(pos);
        pos += n;
    }
    out
}

fn len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn is_injected(e: &Error) -> bool {
    matches!(e, Error::Io(io) if io.to_string().contains("injected crash"))
}

fn tail(s: &Store) -> LogTail {
    s.recovery_report()
        .tail
        .clone()
        .expect("a durable store reports its log tail")
}

// ------------------------------------------------------------ log records

#[test]
fn records_round_trip_and_damage_is_detected() {
    let recs = [
        Record {
            lsn: 1,
            txid: 7,
            body: Body::Begin,
        },
        Record {
            lsn: 2,
            txid: 7,
            body: Body::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            },
        },
        Record {
            lsn: 3,
            txid: 7,
            body: Body::Delete {
                key: b"gone".to_vec(),
            },
        },
        Record {
            lsn: 4,
            txid: 7,
            body: Body::Commit { ops: 2 },
        },
        Record {
            lsn: 5,
            txid: 8,
            body: Body::Abort,
        },
        Record {
            lsn: 6,
            txid: 0,
            body: Body::Checkpoint { snapshot_lsn: 6 },
        },
    ];
    for r in &recs {
        let bytes = encode_record(r);
        assert_eq!(
            decode_record(&bytes),
            Decoded::Valid(r.clone(), bytes.len())
        );
        for cut in 0..bytes.len() {
            assert!(
                !matches!(decode_record(&bytes[..cut]), Decoded::Valid(..)),
                "prefix {cut} of {r:?} was accepted"
            );
        }
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0x10;
            assert!(
                !matches!(decode_record(&b), Decoded::Valid(..)),
                "flip at byte {i} of {r:?} went unnoticed"
            );
        }
    }
}

#[test]
fn commits_are_logged_as_begin_data_commit_with_lsns_and_txids() {
    let dir = tmp("format");
    let mut s = Store::open(&dir).unwrap();
    s.commit(ws(&[("a", Some("1")), ("b", Some("2"))])).unwrap();
    s.commit(ws(&[("a", None)])).unwrap();
    let recs = records(&dir.join(WAL));
    assert_eq!(recs.len(), 7);
    for (i, r) in recs.iter().enumerate() {
        assert_eq!(r.lsn, i as u64 + 1, "LSNs are consecutive from 1");
    }
    assert_eq!((recs[0].txid, &recs[0].body), (1, &Body::Begin));
    assert_eq!((recs[3].txid, &recs[3].body), (1, &Body::Commit { ops: 2 }));
    assert_eq!((recs[4].txid, &recs[4].body), (2, &Body::Begin));
    assert!(matches!(recs[5].body, Body::Delete { .. }));
    assert_eq!((recs[6].txid, &recs[6].body), (2, &Body::Commit { ops: 1 }));
    assert_eq!((s.last_lsn(), s.durable_lsn()), (7, 7));
    drop(s);
    let s = Store::open(&dir).unwrap();
    assert_eq!(s.next_txid(), 3, "transaction ids continue after reopen");
    let r = s.recovery_report();
    assert_eq!((r.redone_txns, r.redone_ops, r.last_lsn), (2, 3, 7));
    assert_eq!(r.tail, Some(LogTail::Clean));
    fs::remove_dir_all(&dir).unwrap();
}

// ------------------------------------------------------ damaged log tails

#[test]
fn incomplete_final_record_drops_only_that_transaction() {
    let dir = tmp("torn");
    {
        let mut s = Store::open(&dir).unwrap();
        put(&mut s, "a", "1").unwrap();
        put(&mut s, "b", "2").unwrap();
        s.inject(FailPoint::WalTornWrite, 1);
        let e = s
            .commit(ws(&[("c", Some("3")), ("d", Some("4"))]))
            .unwrap_err();
        assert!(is_injected(&e), "{e}");
        assert!(
            put(&mut s, "e", "5").is_err(),
            "a crashed store accepts nothing more"
        );
    }
    let wal = dir.join(WAL);
    let s = Store::open(&dir).unwrap();
    assert!(matches!(tail(&s), LogTail::Torn { .. }), "{:?}", tail(&s));
    assert!(s.recovery_report().discarded_bytes > 0);
    assert_eq!(
        (get(&s, "a"), get(&s, "b")),
        (Some("1".into()), Some("2".into()))
    );
    assert_eq!((get(&s, "c"), get(&s, "d")), (None, None));
    let settled = len(&wal);
    drop(s);
    let s = Store::open(&dir).unwrap();
    assert_eq!(
        tail(&s),
        LogTail::Clean,
        "the second recovery finds a clean log"
    );
    assert!(s.recovery_report().rolled_back.is_empty());
    assert_eq!(len(&wal), settled);
    assert_eq!(dump(&s).len(), 2);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn corrupted_final_record_keeps_every_earlier_commit() {
    let dir = tmp("corrupt-tail");
    {
        let mut s = Store::open(&dir).unwrap();
        for i in 0..5 {
            put(&mut s, &format!("k{i}"), "v").unwrap();
        }
    }
    let wal = dir.join(WAL);
    let mut bytes = fs::read(&wal).unwrap();
    let last = *offsets(&wal).last().unwrap();
    bytes[last + 12] ^= 0x01; // inside the final COMMIT record
    fs::write(&wal, &bytes).unwrap();
    let s = Store::open(&dir).unwrap();
    assert!(matches!(tail(&s), LogTail::Torn { .. }), "{:?}", tail(&s));
    assert_eq!(
        s.recovery_report().rolled_back,
        vec![5],
        "the last transaction lost its COMMIT and is rolled back"
    );
    assert_eq!(dump(&s).len(), 4);
    assert_eq!(get(&s, "k4"), None);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn damaged_unrelated_trailing_record_does_not_discard_committed_data() {
    let dir = tmp("trailing-junk");
    let mut model = Model::new();
    {
        let mut s = Store::open(&dir).unwrap();
        for i in 0..20 {
            let w = ws(&[(&format!("k{i}"), Some(&format!("v{i}")))]);
            apply(&mut model, &w);
            s.commit(w).unwrap();
        }
    }
    // A damaged record that belongs to no committed transaction: a mangled
    // ABORT for an unknown transaction, cut short.
    let mut junk = encode_record(&Record {
        lsn: 61,
        txid: 99,
        body: Body::Abort,
    });
    junk[14] ^= 0xff;
    junk.truncate(junk.len() - 3);
    let wal = dir.join(WAL);
    FileOptions::new()
        .append(true)
        .open(&wal)
        .unwrap()
        .write_all(&junk)
        .unwrap();
    let s = Store::open(&dir).unwrap();
    assert_eq!(dump(&s), model, "all 20 committed transactions survive");
    assert!(matches!(tail(&s), LogTail::Torn { .. }));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn mid_log_damage_is_refused_and_salvage_keeps_the_prefix() {
    let dir = tmp("mid-damage");
    {
        let mut s = Store::open(&dir).unwrap();
        put(&mut s, "a", "1").unwrap();
        put(&mut s, "b", "2").unwrap();
        put(&mut s, "c", "3").unwrap();
    }
    let wal = dir.join(WAL);
    let offs = offsets(&wal);
    let mut bytes = fs::read(&wal).unwrap();
    bytes[offs[4] + 30] ^= 0xff; // the PUT of transaction 2, with intact records after it
    fs::write(&wal, &bytes).unwrap();

    match Store::open(&dir) {
        Err(Error::Corrupt(m)) => assert!(m.contains("checksum mismatch"), "{m}"),
        Err(e) => panic!("expected Corrupt, got {e}"),
        Ok(_) => panic!("damage followed by intact commits was silently cut off"),
    }
    assert_eq!(
        fs::read(&wal).unwrap(),
        bytes,
        "a refused open changes nothing"
    );

    let opts = OpenOptions { salvage: true };
    let mut s = Store::open_with(&dir, &opts).unwrap();
    let r = s.recovery_report().clone();
    assert!(r.salvaged && matches!(r.tail, Some(LogTail::Corrupt { .. })));
    assert_eq!(get(&s, "a"), Some("1".into()));
    assert_eq!((get(&s, "b"), get(&s, "c")), (None, None));
    assert_eq!(
        r.rolled_back,
        vec![2],
        "transaction 2's BEGIN survived, so it is rolled back"
    );
    assert_eq!(fs::read(dir.join("data.wal.corrupt")).unwrap(), bytes);
    put(&mut s, "d", "4").unwrap();
    drop(s);
    let s = Store::open(&dir).unwrap();
    assert_eq!(tail(&s), LogTail::Clean);
    assert_eq!(
        (get(&s, "a"), get(&s, "d")),
        (Some("1".into()), Some("4".into()))
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn lsn_gaps_and_missing_history_are_reported() {
    let txn = |first: u64, txid: u64, key: &str| -> Vec<u8> {
        [
            Record {
                lsn: first,
                txid,
                body: Body::Begin,
            },
            Record {
                lsn: first + 1,
                txid,
                body: Body::Put {
                    key: key.into(),
                    value: b"v".to_vec(),
                },
            },
            Record {
                lsn: first + 2,
                txid,
                body: Body::Commit { ops: 1 },
            },
        ]
        .iter()
        .flat_map(encode_record)
        .collect()
    };
    let dir = tmp("gap");
    drop(Store::open(&dir).unwrap());
    // a hole inside the log
    let mut log = txn(1, 1, "a");
    log.extend(txn(7, 3, "c"));
    fs::write(dir.join(WAL), &log).unwrap();
    match Store::open(&dir) {
        Err(Error::Corrupt(m)) => assert!(m.contains("log sequence number 7 follows 3"), "{m}"),
        other => panic!("expected Corrupt, got {:?}", other.err()),
    }
    // a log that does not start right after the checkpoint
    fs::write(dir.join(WAL), txn(4, 2, "b")).unwrap();
    match Store::open(&dir) {
        Err(Error::Corrupt(m)) => assert!(m.contains("missing"), "{m}"),
        other => panic!("expected Corrupt, got {:?}", other.err()),
    }
    // a contiguous log recovers
    let mut log = txn(1, 1, "a");
    log.extend(txn(4, 2, "b"));
    fs::write(dir.join(WAL), &log).unwrap();
    let s = Store::open(&dir).unwrap();
    assert_eq!(dump(&s).len(), 2);
    assert_eq!(s.next_txid(), 3);
    drop(s);
    fs::remove_dir_all(&dir).unwrap();
}

// ----------------------------------------------------------- commit crashes

#[test]
fn crash_before_commit_record_leaves_no_partial_transaction() {
    for point in [
        FailPoint::WalWrite,
        FailPoint::WalTornWrite,
        FailPoint::BeforeCommitRecord,
        FailPoint::TornCommitRecord,
    ] {
        let dir = tmp(&format!("before-commit-{point}"));
        {
            let mut s = Store::open(&dir).unwrap();
            s.commit(ws(&[("keep", Some("old")), ("drop", Some("me"))]))
                .unwrap();
            s.inject(point, 1);
            let multi = ws(&[
                ("keep", Some("new")),
                ("drop", None),
                ("x", Some("1")),
                ("y", Some("2")),
            ]);
            assert!(is_injected(&s.commit(multi).unwrap_err()));
        }
        let wal = dir.join(WAL);
        let s = Store::open(&dir).unwrap();
        let expect: Model = [
            (b"keep".to_vec(), b"old".to_vec()),
            (b"drop".to_vec(), b"me".to_vec()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            dump(&s),
            expect,
            "{point}: none of the unfinished transaction may survive"
        );
        let rolled_back = s.recovery_report().rolled_back.clone();
        if point == FailPoint::WalWrite {
            assert!(
                rolled_back.is_empty(),
                "nothing was written, so nothing to roll back"
            );
        } else {
            assert_eq!(rolled_back, vec![2], "{point}");
            let last = records(&wal).pop().unwrap();
            assert_eq!(
                (last.txid, last.body),
                (2, Body::Abort),
                "{point}: the rollback is logged"
            );
        }
        let settled = len(&wal);
        drop(s);
        let s = Store::open(&dir).unwrap();
        assert!(
            s.recovery_report().rolled_back.is_empty(),
            "{point}: rolled back only once"
        );
        assert_eq!(len(&wal), settled, "{point}");
        assert_eq!(dump(&s), expect);
        drop(s);
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn durable_commit_survives_a_crash_before_it_was_acknowledged() {
    let dir = tmp("after-commit");
    {
        let mut s = Store::open(&dir).unwrap();
        put(&mut s, "acked", "yes").unwrap();
        s.inject(FailPoint::AfterCommitRecord, 1);
        assert!(is_injected(&put(&mut s, "durable", "yes").unwrap_err()));
    }
    let s = Store::open(&dir).unwrap();
    assert_eq!(get(&s, "acked"), Some("yes".into()));
    assert_eq!(
        get(&s, "durable"),
        Some("yes".into()),
        "a COMMIT on disk is a commit, acknowledged or not"
    );
    assert!(s.recovery_report().rolled_back.is_empty());
    drop(s);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn database_level_crash_keeps_acknowledged_rows_documents_and_keys() {
    let dir = tmp("db-crash");
    {
        let mut db = Database::open(&dir).unwrap();
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)")
            .unwrap();
        db.execute("INSERT INTO t VALUES (1, 'one')").unwrap();
        db.execute("db.c.insert({_id: 1, v: 'doc'})").unwrap();
        db.execute("SET k1 v1").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO t VALUES (2, 'two')").unwrap();
        db.execute("db.c.insert({_id: 2, v: 'doc'})").unwrap();
        db.execute("SET k2 v2").unwrap();
        db.inject_fault(FailPoint::TornCommitRecord, 1);
        assert!(db.execute("COMMIT").is_err());
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.recovery_report().rolled_back.len(), 1);
    let rows = db.execute("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(
        rows,
        Output::Rows {
            columns: vec!["id".into()],
            rows: vec![vec![Value::Int(1)]]
        }
    );
    assert_eq!(
        db.execute("db.c.count({})").unwrap(),
        Output::Scalar(Value::Int(1))
    );
    assert_eq!(
        db.execute("GET k1").unwrap(),
        Output::Scalar(Value::Text("v1".into()))
    );
    assert_eq!(db.execute("GET k2").unwrap(), Output::Scalar(Value::Null));
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

// ------------------------------------------------------------- checkpoints

#[test]
fn checkpoint_flushes_the_log_first_and_then_truncates_it() {
    let dir = tmp("ckpt-order");
    let mut s = Store::open(&dir).unwrap();
    s.sync = false;
    for i in 0..10 {
        put(&mut s, &format!("k{i}"), "v").unwrap();
    }
    assert!(
        s.durable_lsn() < s.last_lsn(),
        "without sync, commits are not yet durable"
    );
    s.checkpoint().unwrap();
    // WAL before data: the log was flushed before the snapshot went in
    assert_eq!(s.durable_lsn(), s.last_lsn());
    assert_eq!(
        s.checkpoint_lsn(),
        31,
        "the checkpoint takes the LSN after the 30 logged records"
    );
    assert_eq!(len(&dir.join(WAL)), 0, "the covered log is truncated");
    assert!(!dir.join("data.snap.tmp").exists());
    put(&mut s, "after", "1").unwrap();
    assert_eq!(
        records(&dir.join(WAL))[0].lsn,
        32,
        "LSNs continue after the checkpoint"
    );
    drop(s);
    let s = Store::open(&dir).unwrap();
    let r = s.recovery_report();
    assert_eq!((r.checkpoint_lsn, r.redone_txns, r.last_lsn), (31, 1, 34));
    assert_eq!(dump(&s).len(), 11);
    assert_eq!(s.next_txid(), 12);
    drop(s);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn crash_at_every_checkpoint_step_recovers_the_committed_state() {
    for point in [
        FailPoint::SnapshotWrite,
        FailPoint::BeforeSnapshotRename,
        FailPoint::AfterSnapshotRename,
        FailPoint::BeforeWalTruncate,
        FailPoint::AfterWalTruncate,
    ] {
        let dir = tmp(&format!("ckpt-{point}"));
        let mut model = Model::new();
        {
            let mut s = Store::open(&dir).unwrap();
            for i in 0..10 {
                let w = ws(&[(&format!("k{i}"), Some("a"))]);
                apply(&mut model, &w);
                s.commit(w).unwrap();
            }
            s.checkpoint().unwrap();
            for i in 5..15 {
                let w = ws(&[(&format!("k{i}"), Some("b")), ("k0", None)]);
                apply(&mut model, &w);
                s.commit(w).unwrap();
            }
            s.inject(point, 1);
            assert!(is_injected(&s.checkpoint().unwrap_err()), "{point}");
        }
        if point == FailPoint::BeforeWalTruncate {
            let last = records(&dir.join(WAL)).pop().unwrap();
            assert!(
                matches!(last.body, Body::Checkpoint { .. }),
                "the checkpoint is logged before truncation"
            );
        }
        let mut s = Store::open(&dir).unwrap();
        assert_eq!(dump(&s), model, "{point}");
        assert!(
            !dir.join("data.snap.tmp").exists(),
            "{point}: partial snapshot left behind"
        );
        let r = s.recovery_report().clone();
        match point {
            FailPoint::SnapshotWrite | FailPoint::BeforeSnapshotRename => {
                assert!(r.removed_temp_files, "{point}");
                assert_eq!(r.redone_txns, 10, "{point}: the old snapshot plus the log");
            }
            FailPoint::AfterSnapshotRename | FailPoint::BeforeWalTruncate => {
                assert!(
                    r.cleared_obsolete_records > 0,
                    "{point}: recovery finishes the checkpoint"
                );
                assert_eq!(r.redone_txns, 0, "{point}: nothing is applied twice");
                assert_eq!(len(&dir.join(WAL)), 0, "{point}");
            }
            _ => assert_eq!(r.records_scanned, 0, "{point}"),
        }
        // keep going: write, checkpoint, write, reopen
        let w = ws(&[("later", Some("x"))]);
        apply(&mut model, &w);
        s.commit(w).unwrap();
        s.checkpoint().unwrap();
        let w = ws(&[("last", Some("y")), ("k14", None)]);
        apply(&mut model, &w);
        s.commit(w).unwrap();
        drop(s);
        let s = Store::open(&dir).unwrap();
        assert_eq!(dump(&s), model, "{point}: after more work");
        assert_eq!(tail(&s), LogTail::Clean);
        drop(s);
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn uncommitted_changes_never_reach_a_checkpoint() {
    let dir = tmp("no-steal");
    {
        let mut db = Database::open(&dir).unwrap();
        let mut other = db.session();
        other.execute("BEGIN").unwrap();
        other.execute("SET pending 1").unwrap();
        db.execute("SET committed 1").unwrap();
        db.checkpoint().unwrap();
        // the process "dies" with `other` still open
    }
    {
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(
            db.execute("GET pending").unwrap(),
            Output::Scalar(Value::Null)
        );
        assert_eq!(
            db.execute("GET committed").unwrap(),
            Output::Scalar(Value::Text("1".into()))
        );
    }
    // an incomplete transaction in the log is not folded into a snapshot either
    {
        let mut s = Store::open(&dir).unwrap();
        s.inject(FailPoint::BeforeCommitRecord, 1);
        assert!(put(&mut s, "torn", "1").is_err());
    }
    {
        let mut s = Store::open(&dir).unwrap();
        assert_eq!(s.recovery_report().rolled_back.len(), 1);
        s.checkpoint().unwrap();
    }
    let s = Store::open(&dir).unwrap();
    assert_eq!(get(&s, "torn"), None);
    assert_eq!(
        s.recovery_report().records_scanned,
        0,
        "all state came from the snapshot"
    );
    drop(s);
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(
        db.execute("GET committed").unwrap(),
        Output::Scalar(Value::Text("1".into()))
    );
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_snapshot_older_than_a_logged_checkpoint_is_refused() {
    let dir = tmp("stale-snap");
    {
        let mut s = Store::open(&dir).unwrap();
        put(&mut s, "a", "1").unwrap();
        s.checkpoint().unwrap();
        fs::copy(dir.join("data.snap"), dir.join("..stale")).unwrap();
        put(&mut s, "b", "2").unwrap();
        s.inject(FailPoint::BeforeWalTruncate, 1);
        assert!(s.checkpoint().is_err());
    }
    fs::rename(dir.join("..stale"), dir.join("data.snap")).unwrap();
    match Store::open(&dir) {
        Err(Error::Corrupt(m)) => assert!(m.contains("stale snapshot"), "{m}"),
        other => panic!("expected Corrupt, got {:?}", other.err()),
    }
    fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------- recovery crashes

#[test]
fn recovery_survives_crashing_during_recovery() {
    let dir = tmp("recovery-crash");
    {
        let mut s = Store::open(&dir).unwrap();
        put(&mut s, "a", "1").unwrap();
        s.inject(FailPoint::BeforeCommitRecord, 1);
        assert!(put(&mut s, "b", "2").is_err());
    }
    let wal = dir.join(WAL);
    FileOptions::new()
        .append(true)
        .open(&wal)
        .unwrap()
        .write_all(&[0xD7, 0xB1, 9, 9])
        .unwrap();

    for point in [
        FailPoint::RecoveryAfterTruncate,
        FailPoint::RecoveryAfterTruncate,
        FailPoint::RecoveryAfterAbort,
    ] {
        let e = open_faulty(&dir, Faults::at(point, 1)).err().unwrap();
        assert!(is_injected(&e), "{point}: {e}");
    }
    let s = Store::open(&dir).unwrap();
    assert_eq!((get(&s, "a"), get(&s, "b")), (Some("1".into()), None));
    assert_eq!(
        s.recovery_report().aborts_seen,
        1,
        "the abort logged by the crashed recovery is honoured"
    );
    assert!(s.recovery_report().rolled_back.is_empty());
    let aborts = records(&wal)
        .iter()
        .filter(|r| r.body == Body::Abort)
        .count();
    assert_eq!(
        aborts, 1,
        "the rollback is logged once, however many recoveries ran"
    );
    drop(s);
    let settled = len(&wal);
    for _ in 0..3 {
        let s = Store::open(&dir).unwrap();
        assert_eq!(dump(&s).len(), 1);
        assert_eq!(len(&wal), settled, "recovery is idempotent");
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn reopening_after_recovery_continues_the_log() {
    let dir = tmp("reopen");
    {
        let mut s = Store::open(&dir).unwrap();
        put(&mut s, "a", "1").unwrap();
        s.inject(FailPoint::TornCommitRecord, 1);
        assert!(put(&mut s, "b", "2").is_err());
    }
    for round in 0..3 {
        let mut s = Store::open(&dir).unwrap();
        put(&mut s, &format!("r{round}"), "x").unwrap();
    }
    let s = Store::open(&dir).unwrap();
    assert_eq!(dump(&s).len(), 4);
    let recs = records(&dir.join(WAL));
    for w in recs.windows(2) {
        assert_eq!(
            w[1].lsn,
            w[0].lsn + 1,
            "LSNs stay consecutive across recoveries"
        );
    }
    let committed: Vec<_> = recs
        .iter()
        .filter(|r| matches!(r.body, Body::Commit { .. }))
        .map(|r| r.txid)
        .collect();
    assert_eq!(
        committed,
        vec![1, 3, 4, 5],
        "txid 2 was rolled back and never reused"
    );
    drop(s);
    fs::remove_dir_all(&dir).unwrap();
}

// -------------------------------------------------------------- crash storm

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Many rounds of: reopen, verify, do random work, crash at a random fail
/// point (sometimes also during the next recovery). After every recovery the
/// store must equal a model holding exactly the acknowledged commits, plus a
/// commit whose record was already durable when the crash hit.
#[test]
fn acknowledged_commits_survive_a_deterministic_crash_storm() {
    let dir = tmp("storm");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut model = Model::new();
    let points = [
        FailPoint::WalWrite,
        FailPoint::WalTornWrite,
        FailPoint::BeforeCommitRecord,
        FailPoint::TornCommitRecord,
        FailPoint::AfterCommitRecord,
        FailPoint::SnapshotWrite,
        FailPoint::BeforeSnapshotRename,
        FailPoint::AfterSnapshotRename,
        FailPoint::BeforeWalTruncate,
        FailPoint::AfterWalTruncate,
    ];
    for round in 0..80 {
        if rng.below(4) == 0 {
            let p = if rng.below(2) == 0 {
                FailPoint::RecoveryAfterTruncate
            } else {
                FailPoint::RecoveryAfterAbort
            };
            assert!(
                open_faulty(&dir, Faults::at(p, 1)).is_err(),
                "round {round}"
            );
        }
        let mut s = Store::open(&dir).unwrap();
        assert_eq!(
            dump(&s),
            model,
            "round {round}: recovered state differs from acknowledged commits"
        );
        let point = points[rng.below(points.len() as u64) as usize];
        s.inject(point, 1 + rng.below(6));
        loop {
            if rng.below(5) == 0 {
                match s.checkpoint() {
                    Ok(()) => continue,
                    Err(e) => {
                        assert!(is_injected(&e), "round {round}: {e}");
                        break;
                    }
                }
            }
            let mut w = WriteSet::new();
            for _ in 0..1 + rng.below(4) {
                let k = format!("key{}", rng.below(24)).into_bytes();
                let v = (rng.below(3) != 0)
                    .then(|| format!("r{round}-{}", rng.below(1000)).into_bytes());
                w.insert(k, v);
            }
            match s.commit(w.clone()) {
                Ok(()) => apply(&mut model, &w),
                Err(e) => {
                    assert!(is_injected(&e), "round {round}: {e}");
                    if point == FailPoint::AfterCommitRecord {
                        apply(&mut model, &w);
                    }
                    break;
                }
            }
        }
    }
    let s = Store::open(&dir).unwrap();
    assert_eq!(dump(&s), model);
    drop(s);
    fs::remove_dir_all(&dir).unwrap();
}
