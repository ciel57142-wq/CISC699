//! The storage core: an ordered byte-key/byte-value map, made durable by a
//! checksummed write-ahead log (WAL) plus checkpoint snapshots.
//!
//! A database directory contains:
//!
//! ```text
//! polydb.format   magic line, format version, whether a snapshot exists
//! LOCK            exclusive OS lock held while a process has the database open
//! data.wal        write-ahead log: LSN-numbered, checksummed records, fsynced before ack
//! data.snap       checkpoint snapshot: the keyspace as of one LSN
//! ```
//!
//! The ordered map is an in-memory *cache* of the files: every change reaches
//! `data.wal` (and is fsynced) before it is applied to the map, and the map is
//! rebuilt from `data.snap` + `data.wal` whenever the database is opened.
//!
//! # Log records (format version 2)
//!
//! ```text
//! record = magic:[D7 B1] | len:u32le | crc32(len ‖ body):u32le | body
//! body   = lsn:u64le | txid:u64le | kind:u8 | payload
//! ```
//!
//! Log sequence numbers (LSNs) increase by exactly one per record, so a gap
//! or a reordering is detected. A commit is the group `BEGIN`, one `PUT` or
//! `DELETE` per key, `COMMIT(op count)`, appended and fsynced as a unit before
//! the commit is acknowledged and before the map changes. Recovery applies a
//! transaction only when its intact `COMMIT` is found and the op count
//! matches, so a crash at any byte leaves all of a transaction or none of it.
//! `ABORT` records the rollback of a transaction that recovery found without
//! a `COMMIT`; `CHECKPOINT` records that a snapshot covering an LSN is durable.
//!
//! # Write ordering
//!
//! The engine is *no-steal*: uncommitted writes live only in the session that
//! made them, and the only data image written to disk is the checkpoint
//! snapshot, which is built from committed state. A snapshot is installed
//! only once the log is durable up to its LSN (WAL before data), and the log
//! is truncated only once the snapshot covering it is durable.
//!
//! # Recovery
//!
//! Opening loads the latest valid checkpoint, scans the log, skips records
//! the snapshot already covers, redoes committed transactions, rolls back
//! incomplete ones (their changes were never applied; an `ABORT` is logged so
//! the outcome is decided once) and cuts off a torn tail. It is idempotent
//! and safe to interrupt. Damage that a crash cannot explain is refused.
//!
//! Directories written by earlier releases (format version 1: one unnumbered
//! record per commit) open unchanged and are converted to version 2 by a
//! checkpoint just before their first write.

use crate::error::{Error, Result};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions as OpenOptions_};
use std::io::{self, BufWriter, Read, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};

/// Buffered changes of a transaction: `Some(v)` is a put, `None` a delete.
pub type WriteSet = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

/// Changes in log order: `Some(v)` is a put, `None` a delete.
type Ops = Vec<(Vec<u8>, Option<Vec<u8>>)>;

/// Log sequence number: the position of a record in the log history, from 1.
pub type Lsn = u64;
/// Transaction identifier, from 1. `0` marks records outside a transaction.
pub type TxId = u64;

const SNAPSHOT_FILE: &str = "data.snap";
const WAL_FILE: &str = "data.wal";
/// Entries per snapshot chunk record.
const SNAPSHOT_CHUNK: usize = 4096;

const fn make_crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}
static CRC_TABLE: [u32; 256] = make_crc_table();

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

// ------------------------------------------------------- version 1 records

/// Version 1 record layout (releases up to 0.2): `len:u32 | crc32(payload):u32 | payload`,
/// payload `count:u32 | { tag:u8 (1=put,0=del) | klen:u32 | key | [vlen:u32 | value] }*`.
/// Version 1 logs and snapshots are still read; this writer is kept for tests.
#[cfg(test)]
fn frame<'a>(ops: impl Iterator<Item = (&'a [u8], Option<&'a [u8]>)>) -> Vec<u8> {
    let mut payload = vec![0u8; 4];
    let mut n = 0u32;
    for (k, v) in ops {
        n += 1;
        payload.push(v.is_some() as u8);
        payload.extend_from_slice(&(k.len() as u32).to_le_bytes());
        payload.extend_from_slice(k);
        if let Some(v) = v {
            payload.extend_from_slice(&(v.len() as u32).to_le_bytes());
            payload.extend_from_slice(v);
        }
    }
    payload[0..4].copy_from_slice(&n.to_le_bytes());
    let mut out = Vec::with_capacity(payload.len() + 8);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32(&payload).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

fn read_u32(buf: &[u8], pos: &mut usize) -> Option<u32> {
    let b = buf.get(*pos..*pos + 4)?;
    *pos += 4;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn decode_payload(p: &[u8]) -> Option<Ops> {
    let mut pos = 0;
    let n = read_u32(p, &mut pos)?;
    let mut ops = Vec::new();
    for _ in 0..n {
        let tag = *p.get(pos)?;
        pos += 1;
        let klen = read_u32(p, &mut pos)? as usize;
        let key = p.get(pos..pos.checked_add(klen)?)?.to_vec();
        pos += klen;
        let val = if tag == 1 {
            let vlen = read_u32(p, &mut pos)? as usize;
            let v = p.get(pos..pos.checked_add(vlen)?)?.to_vec();
            pos += vlen;
            Some(v)
        } else {
            None
        };
        ops.push((key, val));
    }
    (pos == p.len()).then_some(ops)
}

/// How a log ends after its last intact record.
#[derive(Debug, Clone, PartialEq)]
pub enum LogTail {
    /// Every byte belonged to an intact record.
    Clean,
    /// The final record is incomplete or damaged: a commit that was being
    /// written when the process died. It was never acknowledged, so it is dropped.
    Torn { valid: usize },
    /// A damaged record is followed by more data. This is not a crash
    /// artefact (commits are appended one at a time), so the log is corrupt.
    Corrupt { valid: usize, reason: String },
}

/// Apply every intact record in `buf` and classify how the log ends.
fn replay(buf: &[u8], map: &mut BTreeMap<Vec<u8>, Vec<u8>>) -> LogTail {
    let mut pos = 0;
    loop {
        if pos == buf.len() {
            return LogTail::Clean;
        }
        let rest = &buf[pos..];
        if rest.len() < 8 || rest.iter().all(|b| *b == 0) {
            // A short header, or a zero-filled tail left by a crash mid-extend.
            return LogTail::Torn { valid: pos };
        }
        let mut p = pos;
        let len = read_u32(buf, &mut p).unwrap_or(0) as usize;
        let crc = read_u32(buf, &mut p).unwrap_or(0);
        let Some(end) = p.checked_add(len).filter(|e| *e <= buf.len()) else {
            // The record claims to extend past the end of the file: torn write.
            return LogTail::Torn { valid: pos };
        };
        let payload = &buf[p..end];
        let ops = if crc32(payload) == crc {
            decode_payload(payload)
        } else {
            None
        };
        let Some(ops) = ops else {
            if end == buf.len() {
                return LogTail::Torn { valid: pos };
            }
            let why = if crc32(payload) != crc {
                "checksum mismatch"
            } else {
                "invalid record framing"
            };
            return LogTail::Corrupt {
                valid: pos,
                reason: format!(
                    "{why} in the record at byte {pos}, with {} more byte(s) after it",
                    buf.len() - end
                ),
            };
        };
        for (k, v) in ops {
            match v {
                Some(v) => {
                    map.insert(k, v);
                }
                None => {
                    map.remove(&k);
                }
            }
        }
        pos = end;
    }
}

// ------------------------------------------------------- version 2 records

const MAGIC: [u8; 2] = [0xD7, 0xB1];
/// magic + len + crc
const HEADER_LEN: usize = 10;
/// lsn + txid + kind
const BODY_FIXED_LEN: usize = 17;
const SNAP_MAGIC: &[u8; 8] = b"PDBSNAP2";

const K_BEGIN: u8 = 1;
const K_PUT: u8 = 2;
const K_DELETE: u8 = 3;
const K_COMMIT: u8 = 4;
const K_ABORT: u8 = 5;
const K_CHECKPOINT: u8 = 6;
const K_SNAP_HEADER: u8 = 16;
const K_SNAP_CHUNK: u8 = 17;
const K_SNAP_FOOTER: u8 = 18;

fn crc32_parts(parts: &[&[u8]]) -> u32 {
    let mut c = !0u32;
    for part in parts {
        for &b in *part {
            c = CRC_TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
    }
    !c
}

/// The decoded body of a log or snapshot record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// A transaction starts.
    Begin,
    /// Set `key` to `value`.
    Put { key: Vec<u8>, value: Vec<u8> },
    /// Remove `key`.
    Delete { key: Vec<u8> },
    /// The transaction is committed; `ops` is how many data records it wrote.
    Commit { ops: u32 },
    /// The transaction is rolled back; none of its data records may be applied.
    Abort,
    /// A snapshot covering every record up to `snapshot_lsn` is durable.
    Checkpoint { snapshot_lsn: Lsn },
    /// First record of a snapshot file.
    SnapHeader { next_txid: TxId, entries: u64 },
    /// A run of entries in a snapshot file.
    SnapChunk { entries: Vec<(Vec<u8>, Vec<u8>)> },
    /// Last record of a snapshot file.
    SnapFooter { entries: u64 },
}

impl Body {
    fn is_snapshot(&self) -> bool {
        matches!(
            self,
            Body::SnapHeader { .. } | Body::SnapChunk { .. } | Body::SnapFooter { .. }
        )
    }
}

/// One decoded record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub lsn: Lsn,
    pub txid: TxId,
    pub body: Body,
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_u32(out, b.len() as u32);
    out.extend_from_slice(b);
}

/// Append one framed record to `out`; `payload` writes the kind-specific part.
fn encode_with(
    out: &mut Vec<u8>,
    lsn: Lsn,
    txid: TxId,
    kind: u8,
    payload: impl FnOnce(&mut Vec<u8>),
) {
    let start = out.len();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&[0u8; 8]);
    let body = out.len();
    put_u64(out, lsn);
    put_u64(out, txid);
    out.push(kind);
    payload(out);
    let len = ((out.len() - body) as u32).to_le_bytes();
    out[start + 2..start + 6].copy_from_slice(&len);
    let crc = crc32_parts(&[&len, &out[body..]]).to_le_bytes();
    out[start + 6..start + 10].copy_from_slice(&crc);
}

/// Encode one record (for tests and tools that build logs by hand).
pub fn encode_record(rec: &Record) -> Vec<u8> {
    let kind = match &rec.body {
        Body::Begin => K_BEGIN,
        Body::Put { .. } => K_PUT,
        Body::Delete { .. } => K_DELETE,
        Body::Commit { .. } => K_COMMIT,
        Body::Abort => K_ABORT,
        Body::Checkpoint { .. } => K_CHECKPOINT,
        Body::SnapHeader { .. } => K_SNAP_HEADER,
        Body::SnapChunk { .. } => K_SNAP_CHUNK,
        Body::SnapFooter { .. } => K_SNAP_FOOTER,
    };
    let mut out = Vec::new();
    encode_with(&mut out, rec.lsn, rec.txid, kind, |p| match &rec.body {
        Body::Begin | Body::Abort => {}
        Body::Put { key, value } => {
            put_bytes(p, key);
            put_bytes(p, value);
        }
        Body::Delete { key } => put_bytes(p, key),
        Body::Commit { ops } => put_u32(p, *ops),
        Body::Checkpoint { snapshot_lsn } => put_u64(p, *snapshot_lsn),
        Body::SnapHeader { next_txid, entries } => {
            p.extend_from_slice(SNAP_MAGIC);
            put_u64(p, *next_txid);
            put_u64(p, *entries);
        }
        Body::SnapChunk { entries } => {
            put_u32(p, entries.len() as u32);
            for (k, v) in entries {
                put_bytes(p, k);
                put_bytes(p, v);
            }
        }
        Body::SnapFooter { entries } => put_u64(p, *entries),
    });
    out
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.buf.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        Some(self.take(n)?.to_vec())
    }
}

fn decode_body(body: &[u8]) -> Option<Record> {
    let mut r = Reader { buf: body, pos: 0 };
    let lsn = r.u64()?;
    let txid = r.u64()?;
    let body = match r.u8()? {
        K_BEGIN => Body::Begin,
        K_PUT => Body::Put {
            key: r.bytes()?,
            value: r.bytes()?,
        },
        K_DELETE => Body::Delete { key: r.bytes()? },
        K_COMMIT => Body::Commit { ops: r.u32()? },
        K_ABORT => Body::Abort,
        K_CHECKPOINT => Body::Checkpoint {
            snapshot_lsn: r.u64()?,
        },
        K_SNAP_HEADER => {
            if r.take(8)? != SNAP_MAGIC {
                return None;
            }
            Body::SnapHeader {
                next_txid: r.u64()?,
                entries: r.u64()?,
            }
        }
        K_SNAP_CHUNK => {
            let n = r.u32()? as usize;
            let mut entries = Vec::with_capacity(n.min(SNAPSHOT_CHUNK));
            for _ in 0..n {
                entries.push((r.bytes()?, r.bytes()?));
            }
            Body::SnapChunk { entries }
        }
        K_SNAP_FOOTER => Body::SnapFooter { entries: r.u64()? },
        _ => return None,
    };
    (r.pos == r.buf.len()).then_some(Record { lsn, txid, body })
}

/// Outcome of decoding the record at the start of a buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    /// A valid record and its total length in bytes.
    Valid(Record, usize),
    /// The buffer ends before the record does (a torn write looks like this).
    Incomplete,
    /// The bytes are not a valid record.
    Invalid(&'static str),
}

/// Where a record with a readable header claims to end, if inside `buf`.
fn declared_end(buf: &[u8]) -> Option<usize> {
    if buf.len() < HEADER_LEN || buf[0..2] != MAGIC {
        return None;
    }
    let len = u32::from_le_bytes(buf[2..6].try_into().unwrap()) as usize;
    HEADER_LEN.checked_add(len).filter(|e| *e <= buf.len())
}

/// Decode the record at the start of `buf`, validating framing and checksum.
pub fn decode_record(buf: &[u8]) -> Decoded {
    if buf.len() < 2 {
        return Decoded::Incomplete;
    }
    if buf[0..2] != MAGIC {
        return Decoded::Invalid("bad record magic");
    }
    if buf.len() < HEADER_LEN {
        return Decoded::Incomplete;
    }
    let len_bytes = &buf[2..6];
    let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
    let crc = u32::from_le_bytes(buf[6..10].try_into().unwrap());
    if len < BODY_FIXED_LEN {
        return Decoded::Invalid("record shorter than its fixed header");
    }
    let Some(end) = HEADER_LEN.checked_add(len) else {
        return Decoded::Invalid("record length overflows");
    };
    if buf.len() < end {
        return Decoded::Incomplete;
    }
    let body = &buf[HEADER_LEN..end];
    if crc32_parts(&[len_bytes, body]) != crc {
        return Decoded::Invalid("checksum mismatch");
    }
    match decode_body(body) {
        Some(rec) => Decoded::Valid(rec, end),
        None => Decoded::Invalid("malformed record body"),
    }
}

/// Does a valid log record with an LSN above `after` start anywhere in `buf`?
fn valid_record_after(buf: &[u8], after: Option<Lsn>) -> bool {
    (0..buf.len().saturating_sub(HEADER_LEN)).any(|i| {
        buf[i] == MAGIC[0]
            && buf[i + 1] == MAGIC[1]
            && matches!(decode_record(&buf[i..]),
                Decoded::Valid(r, _) if !r.body.is_snapshot() && !matches!(after, Some(a) if r.lsn <= a))
    })
}

/// Split a version 2 log into its valid records and classify how it ends.
///
/// A bad record is a *torn tail* when nothing intact can follow it: it is
/// incomplete, or it ends exactly at end-of-file, or only zeros follow, or
/// its extent is unknown and no valid record appears later. That is all a
/// crash during an append can leave. Anything else (an intact record after
/// the damage, a complete bad record with data after it, an LSN gap) is
/// corruption.
fn scan_log(buf: &[u8]) -> (Vec<Record>, LogTail) {
    let mut records: Vec<Record> = Vec::new();
    let mut pos = 0;
    loop {
        if pos == buf.len() {
            return (records, LogTail::Clean);
        }
        let rest = &buf[pos..];
        if rest.iter().all(|b| *b == 0) {
            // A zero-filled tail left by a crash while the file was extended.
            return (records, LogTail::Torn { valid: pos });
        }
        let prev = records.last().map(|r| r.lsn);
        let why = match decode_record(rest) {
            Decoded::Valid(rec, n) => {
                if rec.body.is_snapshot() {
                    "snapshot record inside the log"
                } else if prev.is_some_and(|p| rec.lsn != p + 1) {
                    let reason = format!(
                        "log sequence number {} follows {} in the record at byte {pos}",
                        rec.lsn,
                        prev.unwrap()
                    );
                    return (records, LogTail::Corrupt { valid: pos, reason });
                } else {
                    records.push(rec);
                    pos += n;
                    continue;
                }
            }
            Decoded::Incomplete => return (records, LogTail::Torn { valid: pos }),
            Decoded::Invalid(why) => why,
        };
        let end = declared_end(rest).map(|e| pos + e);
        let more_after = end.is_some_and(|e| buf[e..].iter().any(|b| *b != 0));
        if more_after || valid_record_after(&buf[pos + 1..], prev) {
            let reason = match end {
                Some(e) if e < buf.len() => format!(
                    "{why} in the record at byte {pos}, with {} more byte(s) after it",
                    buf.len() - e
                ),
                _ => format!("{why} in the record at byte {pos}, followed by intact records"),
            };
            return (records, LogTail::Corrupt { valid: pos, reason });
        }
        return (records, LogTail::Torn { valid: pos });
    }
}

// --------------------------------------------------------------- snapshots

/// A loaded checkpoint image.
struct Image {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Every log record up to this LSN is reflected in `map`.
    redo_lsn: Lsn,
    next_txid: TxId,
}

/// Load a snapshot in either format; any damage is an error, never a partial load.
fn load_snapshot(path: &Path) -> Result<Image> {
    let buf = fs::read(path)?;
    let bad = |why: &str| {
        Error::Corrupt(format!(
            "snapshot {} failed its checksum ({why})",
            path.display()
        ))
    };
    if !buf.starts_with(&MAGIC) {
        // Format version 1: a sequence of unnumbered records.
        let mut map = BTreeMap::new();
        if replay(&buf, &mut map) != LogTail::Clean {
            return Err(bad("version 1 record"));
        }
        return Ok(Image {
            map,
            redo_lsn: 0,
            next_txid: 1,
        });
    }
    let mut pos = 0;
    let next = |pos: &mut usize| match decode_record(&buf[*pos..]) {
        Decoded::Valid(r, n) => {
            *pos += n;
            Ok(r)
        }
        Decoded::Incomplete => Err(bad("truncated")),
        Decoded::Invalid(why) => Err(bad(why)),
    };
    let head = next(&mut pos)?;
    let Body::SnapHeader { next_txid, entries } = head.body else {
        return Err(bad("missing header"));
    };
    let mut map = BTreeMap::new();
    let mut seen = 0u64;
    loop {
        let rec = next(&mut pos)?;
        if rec.lsn != head.lsn {
            return Err(bad("mixed checkpoint LSNs"));
        }
        match rec.body {
            Body::SnapChunk { entries } => {
                seen += entries.len() as u64;
                map.extend(entries);
            }
            Body::SnapFooter { entries: n } => {
                if n != entries
                    || seen != entries
                    || map.len() as u64 != entries
                    || pos != buf.len()
                {
                    return Err(bad("entry count or length mismatch"));
                }
                return Ok(Image {
                    map,
                    redo_lsn: head.lsn,
                    next_txid,
                });
            }
            _ => return Err(bad("unexpected record")),
        }
    }
}

fn apply(
    map: &mut BTreeMap<Vec<u8>, Vec<u8>>,
    ops: impl IntoIterator<Item = (Vec<u8>, Option<Vec<u8>>)>,
) {
    for (k, v) in ops {
        match v {
            Some(v) => {
                map.insert(k, v);
            }
            None => {
                map.remove(&k);
            }
        }
    }
}

// --------------------------------------------------------- failure injection

/// A place where a crash can be simulated deterministically.
///
/// When an armed point fires, the store behaves like a process that died at
/// that instant: bytes already handed to the OS stay in the files, nothing is
/// cleaned up, and every later write through that store is refused. Reopen
/// the directory to run recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailPoint {
    /// A commit is about to append its log records; nothing is written yet.
    WalWrite,
    /// Half of a commit's `BEGIN` and data records reach the log.
    WalTornWrite,
    /// `BEGIN` and data records are written; the `COMMIT` record is not.
    BeforeCommitRecord,
    /// Half of the `COMMIT` record reaches the log.
    TornCommitRecord,
    /// The `COMMIT` record is durable, but the commit was neither applied nor
    /// acknowledged.
    AfterCommitRecord,
    /// The checkpoint snapshot (the data image) is partially written.
    SnapshotWrite,
    /// The snapshot is complete and synced but not renamed into place.
    BeforeSnapshotRename,
    /// The snapshot is installed; no `CHECKPOINT` record is logged yet.
    AfterSnapshotRename,
    /// The `CHECKPOINT` record is durable; the log is not yet truncated.
    BeforeWalTruncate,
    /// The log is truncated; the format file is not yet updated.
    AfterWalTruncate,
    /// Recovery cut off the log's torn tail but has not logged its aborts.
    RecoveryAfterTruncate,
    /// Recovery logged its aborts but has not cleared obsolete log records.
    RecoveryAfterAbort,
}

impl fmt::Display for FailPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Which fail points are armed, and on which hit each one fires.
#[derive(Debug, Default, Clone)]
pub struct Faults {
    armed: Vec<(FailPoint, u64)>,
}

impl Faults {
    /// Nothing armed.
    pub fn none() -> Faults {
        Faults::default()
    }

    /// Fire the `nth` time (1-based) execution reaches `point`.
    pub fn at(point: FailPoint, nth: u64) -> Faults {
        Faults::none().and(point, nth)
    }

    /// Also arm `point`.
    pub fn and(mut self, point: FailPoint, nth: u64) -> Faults {
        self.armed.push((point, nth.max(1)));
        self
    }

    fn hit(&mut self, point: FailPoint) -> bool {
        let Some(i) = self.armed.iter().position(|(p, _)| *p == point) else {
            return false;
        };
        self.armed[i].1 -= 1;
        if self.armed[i].1 == 0 {
            self.armed.remove(i);
            return true;
        }
        false
    }
}

fn injected(point: FailPoint) -> Error {
    Error::Io(io::Error::other(format!("injected crash at {point}")))
}

/// What the most recent open found and did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecoveryReport {
    /// LSN covered by the checkpoint snapshot that was loaded (0 = none).
    pub checkpoint_lsn: Lsn,
    /// Highest LSN after recovery.
    pub last_lsn: Lsn,
    /// Valid log records read.
    pub records_scanned: u64,
    /// Records the snapshot already reflects, so not replayed.
    pub records_skipped: u64,
    /// Committed transactions redone from the log.
    pub redone_txns: u64,
    /// Puts and deletes redone from the log.
    pub redone_ops: u64,
    /// Transactions with no `COMMIT` that this recovery rolled back (an
    /// `ABORT` record was logged for each).
    pub rolled_back: Vec<TxId>,
    /// `ABORT` records found in the log.
    pub aborts_seen: u64,
    /// How the log ended before repair.
    pub tail: Option<LogTail>,
    /// Bytes of a torn tail that were cut off.
    pub discarded_bytes: u64,
    /// The damaged log was opened with `salvage` (a copy is in `data.wal.corrupt`).
    pub salvaged: bool,
    /// A half-written snapshot or format file of an interrupted checkpoint was removed.
    pub removed_temp_files: bool,
    /// Log records the snapshot already covered were cleared (an interrupted
    /// checkpoint was finished).
    pub cleared_obsolete_records: u64,
    /// The directory is in format version 1; it is converted on the first write.
    pub legacy_format: bool,
}

/// How to open a store directory.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Open a log with interior corruption anyway, keeping only the commits
    /// before the damage (the damaged file is first copied to `data.wal.corrupt`).
    /// Off by default: corruption is reported as [`Error::Corrupt`].
    pub salvage: bool,
}

const LOCK_FILE: &str = "LOCK";

/// Marks a directory as an initialised polydb database and records the
/// on-disk format version. Written at creation (and rewritten when the first
/// snapshot appears), always by atomic rename.
const FORMAT_FILE: &str = "polydb.format";
const FORMAT_MAGIC: &str = "polydb-storage";
/// Version 2: LSN-numbered log records and LSN-stamped snapshots (see the
/// module documentation). Version 1 is the unnumbered layout earlier releases
/// wrote (directories without a format file are version 1); it is read as is
/// and converted to version 2 by a checkpoint before the first write.
pub const FORMAT_VERSION: u32 = 2;

#[derive(Clone, Copy)]
struct Format {
    version: u32,
    /// A snapshot has been written, so `data.snap` must exist from now on.
    snapshot: bool,
}

impl Format {
    fn text(self) -> String {
        format!(
            "{FORMAT_MAGIC}\nversion={}\nsnapshot={}\n",
            self.version, self.snapshot as u8
        )
    }

    fn read(path: &Path) -> Result<Format> {
        let text = fs::read_to_string(path)?;
        let bad = || {
            Error::Corrupt(format!(
                "{} is not a valid polydb format file",
                path.display()
            ))
        };
        let mut lines = text.lines();
        if lines.next() != Some(FORMAT_MAGIC) {
            return Err(bad());
        }
        let (mut version, mut snapshot) = (None, false);
        for line in lines.filter(|l| !l.trim().is_empty()) {
            match line.split_once('=') {
                Some(("version", v)) => version = Some(v.trim().parse::<u32>().map_err(|_| bad())?),
                Some(("snapshot", v)) => snapshot = v.trim() == "1",
                Some(_) => {} // keys added by later minor revisions are ignored
                None => return Err(bad()),
            }
        }
        match version {
            Some(v) if v >= 1 => Ok(Format {
                version: v,
                snapshot,
            }),
            _ => Err(bad()),
        }
    }
}

/// Make directory entries (new files, renames) durable. Only Unix lets a
/// directory be opened and fsynced; elsewhere the file system's own metadata
/// journaling is relied on.
fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

/// Replace `dir/name` with `contents` so that a crash leaves either the old
/// or the new file, never a mix: temp file, fsync, rename, fsync directory.
fn write_atomic(dir: &Path, name: &str, contents: &[u8]) -> Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    let res = (|| -> Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = res {
        // Best effort only: a leftover temp file is harmless and is removed at the next open.
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(&tmp, dir.join(name))?;
    sync_dir(dir)
}

pub struct Store {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Held for the lifetime of the store: an exclusive OS lock on `dir/LOCK`
    /// so two processes can never write the same database directory.
    _lock: Option<File>,
    dir: Option<PathBuf>,
    wal: Option<File>,
    wal_len: u64,
    /// The format file already records that a snapshot exists.
    snapshot_recorded: bool,
    /// The directory is still in format version 1 (converted before the first write).
    legacy: bool,
    /// Set when a write failed in a way that leaves the log's on-disk state
    /// unknown (failed fsync, failed rollback), or after a simulated crash.
    /// All further writes are refused until the database is reopened.
    failed: Option<String>,
    /// After a failed automatic checkpoint, do not retry before the log reaches this size.
    checkpoint_retry_at: u64,
    last_checkpoint_error: Option<String>,
    next_lsn: Lsn,
    /// Every log record up to this LSN is known to be fsynced.
    durable_lsn: Lsn,
    next_txid: TxId,
    checkpoint_lsn: Lsn,
    faults: Faults,
    report: RecoveryReport,
    /// fsync the WAL on every commit (default true). Turning this off trades
    /// durability of the last few commits on power loss for speed.
    pub sync: bool,
    /// The WAL is folded into a snapshot once it grows past this many bytes.
    pub checkpoint_bytes: u64,
}

impl Drop for Store {
    fn drop(&mut self) {
        // Release the directory lock explicitly. Closing the descriptor is not
        // enough: a child process forked by another thread at that moment
        // briefly shares the open file description (until its exec closes it),
        // and an flock lives as long as any descriptor of it does.
        if let Some(l) = &self._lock {
            let _ = l.unlock();
        }
    }
}

impl Store {
    /// A volatile store with no files behind it.
    pub fn memory() -> Store {
        Store {
            map: BTreeMap::new(),
            _lock: None,
            dir: None,
            wal: None,
            wal_len: 0,
            snapshot_recorded: false,
            legacy: false,
            failed: None,
            checkpoint_retry_at: 0,
            last_checkpoint_error: None,
            next_lsn: 1,
            durable_lsn: 0,
            next_txid: 1,
            checkpoint_lsn: 0,
            faults: Faults::none(),
            report: RecoveryReport::default(),
            sync: true,
            checkpoint_bytes: 64 << 20,
        }
    }

    /// Open the database in `dir`, or create one if `dir` does not exist or
    /// is an empty directory, recovering from the snapshot and WAL.
    ///
    /// Refuses (rather than starting empty) when the directory holds other
    /// files, is locked, has a newer format version, is missing a file it is
    /// known to have had, or has damage recovery cannot attribute to an
    /// interrupted commit.
    pub fn open(dir: impl AsRef<Path>) -> Result<Store> {
        Store::open_with(dir, &OpenOptions::default())
    }

    pub fn open_with(dir: impl AsRef<Path>, opts: &OpenOptions) -> Result<Store> {
        Store::open_with_faults(dir, opts, Faults::none())
    }

    /// [`Store::open_with`] with fail points armed. They also cover the
    /// recovery that runs while opening. For crash testing.
    pub fn open_with_faults(
        dir: impl AsRef<Path>,
        opts: &OpenOptions,
        faults: Faults,
    ) -> Result<Store> {
        let dir = dir.as_ref().to_path_buf();
        match fs::metadata(&dir) {
            Ok(m) if !m.is_dir() => {
                return Err(Error::Storage(format!(
                    "'{}' exists and is not a directory",
                    dir.display()
                )))
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&dir)?;
                let parent = match dir.parent() {
                    Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                    _ => PathBuf::from("."),
                };
                sync_dir(&parent)?;
            }
            Err(e) => return Err(e.into()),
        }
        let lock = OpenOptions_::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LOCK_FILE))?;
        // A handle being dropped in this or another thread releases the lock a
        // moment later: wait briefly before reporting the directory as locked.
        let mut tries = 0;
        let locked = loop {
            match lock.try_lock() {
                Err(fs::TryLockError::WouldBlock) if tries < 25 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                other => break other,
            }
        };
        match locked {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!(
                        "database directory {} is locked by another process",
                        dir.display()
                    ),
                )))
            }
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }

        let wal_path = dir.join(WAL_FILE);
        let snap = dir.join(SNAPSHOT_FILE);
        let format_path = dir.join(FORMAT_FILE);
        let has_wal = wal_path.try_exists()?;
        let has_snap = snap.try_exists()?;
        let format = if format_path.try_exists()? {
            let f = Format::read(&format_path)?;
            if f.version > FORMAT_VERSION {
                return Err(Error::Storage(format!(
                    "database '{}' uses storage format {}, but this polydb supports up to {FORMAT_VERSION}; use a newer polydb",
                    dir.display(),
                    f.version
                )));
            }
            if !has_wal {
                return Err(Error::Corrupt(format!(
                    "{} is missing from an initialised database; refusing to open it as empty (restore the directory from a backup)",
                    wal_path.display()
                )));
            }
            if f.snapshot && !has_snap {
                return Err(Error::Corrupt(format!(
                    "{} is missing although a checkpoint was written; refusing to open a partial database (restore the directory from a backup)",
                    snap.display()
                )));
            }
            f
        } else if has_wal {
            // Written by a release without format files, whose record layout
            // is version 1: record that, and leave the files as they are.
            // (Version 2 content whose format file was lost is recognised by
            // its record magic and kept as version 2.)
            let starts_v2 = |p: &Path| -> Result<bool> {
                let mut head = [0u8; 2];
                Ok(p.try_exists()? && File::open(p)?.read(&mut head)? == 2 && head == MAGIC)
            };
            let version = if starts_v2(&wal_path)? || starts_v2(&snap)? {
                2
            } else {
                1
            };
            let f = Format {
                version,
                snapshot: has_snap,
            };
            write_atomic(&dir, FORMAT_FILE, f.text().as_bytes())?;
            f
        } else if has_snap {
            return Err(Error::Corrupt(format!(
                "{} exists but {} is missing; refusing to open a partial database",
                snap.display(),
                wal_path.display()
            )));
        } else {
            // A new database. Only initialise into an empty directory (our own
            // lock file, leftovers of an interrupted initialisation and a mount
            // point's `lost+found` aside), so pointing polydb at the wrong
            // directory cannot bury files there.
            let tmp_format = format!("{FORMAT_FILE}.tmp");
            for entry in fs::read_dir(&dir)? {
                let name = entry?.file_name();
                if name != LOCK_FILE && name != tmp_format.as_str() && name != "lost+found" {
                    return Err(Error::Storage(format!(
                        "'{}' is not empty and is not a polydb database (found '{}'); refusing to create a database there",
                        dir.display(),
                        name.to_string_lossy()
                    )));
                }
            }
            // The log first, then the format file: a crash in between leaves
            // a log without a format file, which the branch above adopts.
            OpenOptions_::new()
                .write(true)
                .create_new(true)
                .open(&wal_path)?
                .sync_all()?;
            sync_dir(&dir)?;
            let f = Format {
                version: FORMAT_VERSION,
                snapshot: false,
            };
            write_atomic(&dir, FORMAT_FILE, f.text().as_bytes())?;
            f
        };

        let mut store = Store::memory();
        store._lock = Some(lock);
        store.dir = Some(dir);
        store.faults = faults;
        store.legacy = format.version < 2;
        store.recover(format, has_snap, opts)?;
        Ok(store)
    }

    /// Arm a fail point on an open store.
    pub fn inject(&mut self, point: FailPoint, nth: u64) {
        self.faults = std::mem::take(&mut self.faults).and(point, nth);
    }

    /// What recovery found and did when this store was opened.
    pub fn recovery_report(&self) -> &RecoveryReport {
        &self.report
    }

    /// LSN of the newest record in the log history (0 when there is none).
    pub fn last_lsn(&self) -> Lsn {
        self.next_lsn - 1
    }

    /// Every record up to this LSN has been fsynced.
    pub fn durable_lsn(&self) -> Lsn {
        self.durable_lsn
    }

    /// LSN covered by the current checkpoint snapshot.
    pub fn checkpoint_lsn(&self) -> Lsn {
        self.checkpoint_lsn
    }

    /// The identifier the next commit will get.
    pub fn next_txid(&self) -> TxId {
        self.next_txid
    }

    fn fault(&mut self, point: FailPoint) -> Result<()> {
        if self.faults.hit(point) {
            return Err(self.crash(point));
        }
        Ok(())
    }

    fn crash(&mut self, point: FailPoint) -> Error {
        self.failed = Some(format!("a simulated crash at {point}"));
        injected(point)
    }

    // ------------------------------------------------------------ recovery

    fn recover(&mut self, format: Format, has_snap: bool, opts: &OpenOptions) -> Result<()> {
        let dir = self.dir.clone().expect("recovery needs a directory");
        let wal_path = dir.join(WAL_FILE);
        let snap = dir.join(SNAPSHOT_FILE);
        let mut report = RecoveryReport {
            legacy_format: self.legacy,
            ..RecoveryReport::default()
        };

        // 1. Temp files of an interrupted checkpoint or format rewrite carry
        //    nothing that is not elsewhere; a snapshot that was never renamed
        //    into place is never trusted.
        for name in [format!("{SNAPSHOT_FILE}.tmp"), format!("{FORMAT_FILE}.tmp")] {
            let p = dir.join(name);
            if p.try_exists()? {
                fs::remove_file(&p)?;
                report.removed_temp_files = true;
            }
        }
        if report.removed_temp_files {
            sync_dir(&dir)?;
        }

        // 2. The latest valid checkpoint.
        let (redo, snap_txid) = if has_snap {
            let img = load_snapshot(&snap)?;
            self.map = img.map;
            (img.redo_lsn, img.next_txid)
        } else {
            (0, 1)
        };
        report.checkpoint_lsn = redo;

        let mut wal = OpenOptions_::new()
            .read(true)
            .append(true)
            .open(&wal_path)?;
        let mut buf = Vec::new();
        wal.read_to_end(&mut buf)?;

        // 3. Scan the log. Nothing on disk changes until it is known to be
        //    recoverable.
        let corrupt_log = |valid: usize, reason: &str| {
            Error::Corrupt(format!(
                "write-ahead log {}: {reason}; refusing to open (open with salvage to keep the {valid} intact byte(s) before it)",
                wal_path.display()
            ))
        };
        let mut open: BTreeMap<TxId, Ops> = BTreeMap::new();
        let mut last = redo;
        let mut max_txid = 0;
        let mut obsolete = 0u64;
        let tail = if self.legacy {
            // Version 1: whole-commit records without LSNs. Replaying one over
            // a snapshot that already contains it is harmless (records hold
            // whole values), which also covers a conversion that crashed after
            // installing its snapshot.
            replay(&buf, &mut self.map)
        } else {
            let (records, tail) = scan_log(&buf);
            if let LogTail::Corrupt { valid, reason } = &tail {
                if !opts.salvage {
                    return Err(corrupt_log(*valid, reason));
                }
            }
            let bad = |why: String| {
                Error::Corrupt(format!("write-ahead log {}: {why}", wal_path.display()))
            };
            for rec in &records {
                report.records_scanned += 1;
                max_txid = max_txid.max(rec.txid);
                if let Body::Checkpoint { snapshot_lsn } = rec.body {
                    if snapshot_lsn > redo {
                        return Err(bad(format!(
                            "a checkpoint at LSN {snapshot_lsn} completed, but {} only covers LSN {redo}; refusing to open a stale snapshot",
                            snap.display()
                        )));
                    }
                }
                if rec.lsn <= redo {
                    // Already in the snapshot: left over from a checkpoint
                    // that crashed before truncating the log.
                    report.records_skipped += 1;
                    obsolete += 1;
                    continue;
                }
                if rec.lsn != last + 1 {
                    return Err(bad(format!(
                        "records {}..{} are missing after the checkpoint at LSN {redo}",
                        last + 1,
                        rec.lsn - 1
                    )));
                }
                last = rec.lsn;
                let unknown = || {
                    bad(format!(
                        "LSN {}: no BEGIN for transaction {}",
                        rec.lsn, rec.txid
                    ))
                };
                match &rec.body {
                    Body::Begin => {
                        if open.insert(rec.txid, Vec::new()).is_some() {
                            return Err(bad(format!("transaction {} begins twice", rec.txid)));
                        }
                    }
                    Body::Put { key, value } => open
                        .get_mut(&rec.txid)
                        .ok_or_else(unknown)?
                        .push((key.clone(), Some(value.clone()))),
                    Body::Delete { key } => open
                        .get_mut(&rec.txid)
                        .ok_or_else(unknown)?
                        .push((key.clone(), None)),
                    Body::Commit { ops } => {
                        // Redo: a transaction's changes are applied only here.
                        let writes = open.remove(&rec.txid).ok_or_else(unknown)?;
                        if writes.len() != *ops as usize {
                            return Err(bad(format!(
                                "transaction {} commits {ops} write(s) but logged {}",
                                rec.txid,
                                writes.len()
                            )));
                        }
                        report.redone_txns += 1;
                        report.redone_ops += writes.len() as u64;
                        apply(&mut self.map, writes);
                    }
                    Body::Abort => {
                        open.remove(&rec.txid);
                        report.aborts_seen += 1;
                    }
                    Body::Checkpoint { .. } => {}
                    Body::SnapHeader { .. } | Body::SnapChunk { .. } | Body::SnapFooter { .. } => {
                        unreachable!("scan_log rejects snapshot records")
                    }
                }
            }
            tail
        };

        // 4. Repair the tail: a torn final write is cut off; a corrupt log is
        //    only cut back when salvage was asked for, after keeping a copy.
        let valid = match &tail {
            LogTail::Clean => buf.len(),
            LogTail::Torn { valid } => {
                report.discarded_bytes = (buf.len() - valid) as u64;
                *valid
            }
            LogTail::Corrupt { valid, reason } => {
                if !opts.salvage {
                    return Err(corrupt_log(*valid, reason));
                }
                fs::write(dir.join(format!("{WAL_FILE}.corrupt")), &buf)?;
                report.salvaged = true;
                *valid
            }
        };
        report.tail = Some(tail);
        if valid < buf.len() {
            wal.set_len(valid as u64)?;
            wal.sync_all()?;
        }
        self.snapshot_recorded = format.snapshot;
        if has_snap && !format.snapshot {
            // A checkpoint finished its rename but crashed before recording it.
            let f = Format {
                snapshot: true,
                ..format
            };
            write_atomic(&dir, FORMAT_FILE, f.text().as_bytes())?;
            self.snapshot_recorded = true;
        }
        self.wal = Some(wal);
        self.wal_len = valid as u64;
        self.next_lsn = last + 1;
        self.durable_lsn = last;
        self.next_txid = snap_txid.max(max_txid + 1).max(1);
        self.checkpoint_lsn = redo;
        report.last_lsn = last;
        self.report = report;
        self.fault(FailPoint::RecoveryAfterTruncate)?;

        // 5. Undo. An incomplete transaction's changes were never applied (no
        //    COMMIT, and snapshots only hold committed state), so rolling it
        //    back means logging its ABORT: later recoveries then see the
        //    outcome instead of deciding again.
        if !open.is_empty() {
            let mut abort = Vec::new();
            let mut lsn = self.next_lsn;
            for txid in open.keys() {
                encode_with(&mut abort, lsn, *txid, K_ABORT, |_| {});
                lsn += 1;
            }
            self.append(&abort, true)?;
            self.next_lsn = lsn;
            self.durable_lsn = lsn - 1;
            self.report.rolled_back = open.into_keys().collect();
            self.report.last_lsn = self.last_lsn();
        }
        self.fault(FailPoint::RecoveryAfterAbort)?;

        // 6. A log holding only records the snapshot covers is what a
        //    checkpoint leaves when it crashes before truncating: finish it.
        if !self.legacy && obsolete > 0 && obsolete == self.report.records_scanned {
            self.truncate_wal()?;
            self.report.cleared_obsolete_records = obsolete;
        }
        Ok(())
    }

    /// Whether this store is backed by files.
    pub fn is_durable(&self) -> bool {
        self.dir.is_some()
    }

    /// Bytes of an interrupted final commit that recovery removed when opening.
    pub fn discarded_tail_bytes(&self) -> u64 {
        self.report.discarded_bytes
    }

    /// Why the most recent automatic checkpoint failed, if it did. Commits
    /// stay durable in the log meanwhile; the log just keeps growing.
    pub fn last_checkpoint_error(&self) -> Option<&str> {
        self.last_checkpoint_error.as_deref()
    }

    fn check_writable(&self) -> Result<()> {
        match &self.failed {
            None => Ok(()),
            Some(why) => Err(Error::Storage(format!(
                "writes are disabled after an earlier storage failure ({why}); reopen the database to recover"
            ))),
        }
    }

    pub fn get(&self, key: &[u8]) -> Option<&Vec<u8>> {
        self.map.get(key)
    }

    pub fn scan<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (&'a Vec<u8>, &'a Vec<u8>)> + 'a {
        self.map
            .range::<[u8], _>((Bound::Included(prefix), Bound::Unbounded))
            .take_while(move |(k, _)| k.starts_with(prefix))
    }

    /// Committed entries with keys in `[lo, hi)` style bounds, in key order.
    pub fn range<'a>(
        &'a self,
        lo: Bound<&'a [u8]>,
        hi: Bound<&'a [u8]>,
    ) -> Box<dyn Iterator<Item = (&'a Vec<u8>, &'a Vec<u8>)> + 'a> {
        if !range_is_valid(&lo, &hi) {
            return Box::new(std::iter::empty());
        }
        Box::new(self.map.range::<[u8], _>((lo, hi)))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    // --------------------------------------------------------------- writes

    /// Append to the log, fsyncing if asked. On failure the partial append is
    /// cut off again; if that, or the fsync, fails the log's on-disk state is
    /// unknown and the store refuses all further writes.
    fn append(&mut self, bytes: &[u8], sync: bool) -> Result<()> {
        let wal = self.wal.as_mut().expect("append needs a log");
        if let Err(e) = wal.write_all(bytes) {
            // Cut off whatever part of the record reached the file, so it
            // cannot sit in front of later commits.
            if let Err(e2) = wal.set_len(self.wal_len).and_then(|_| wal.sync_data()) {
                self.failed = Some(format!(
                    "log write failed ({e}) and could not be rolled back ({e2})"
                ));
            }
            return Err(e.into());
        }
        if sync {
            if let Err(e) = wal.sync_data() {
                // After a failed fsync the kernel may have dropped the dirty
                // pages, so the log's on-disk content is unknown. The write is
                // reported as failed, cut off, and nothing may build on it.
                let rollback = match wal.set_len(self.wal_len) {
                    Ok(()) => String::new(),
                    Err(e2) => format!("; rollback also failed: {e2}"),
                };
                self.failed = Some(format!("fsync of {WAL_FILE} failed: {e}{rollback}"));
                return Err(e.into());
            }
        }
        self.wal_len += bytes.len() as u64;
        Ok(())
    }

    /// Atomically and durably apply a write set. When this returns `Ok` (and
    /// `sync` is on, the default) the commit is on stable storage.
    pub fn commit(&mut self, ws: WriteSet) -> Result<()> {
        if ws.is_empty() {
            return Ok(());
        }
        self.check_writable()?;
        if self.wal.is_some() {
            if self.legacy {
                // Convert a version 1 directory before writing the first
                // version 2 record into it.
                self.checkpoint()?;
            }
            self.log_commit(&ws)?;
        } else {
            self.next_txid += 1;
        }
        apply(&mut self.map, ws);
        if self.wal.is_some() && self.wal_len > self.checkpoint_bytes.max(self.checkpoint_retry_at)
        {
            if let Err(e) = self.checkpoint() {
                // The commit itself is already durable in the log, so it has
                // succeeded. Report the checkpoint failure and retry later.
                eprintln!(
                    "polydb: warning: automatic checkpoint failed, will retry (commits remain durable in {WAL_FILE}): {e}"
                );
                self.last_checkpoint_error = Some(e.to_string());
                self.checkpoint_retry_at = self.wal_len.saturating_add(self.checkpoint_bytes);
            }
        }
        Ok(())
    }

    /// Log one transaction as BEGIN, its data records and COMMIT.
    fn log_commit(&mut self, ws: &WriteSet) -> Result<()> {
        let txid = self.next_txid;
        let mut lsn = self.next_lsn;
        let mut body = Vec::new();
        encode_with(&mut body, lsn, txid, K_BEGIN, |_| {});
        lsn += 1;
        for (k, v) in ws {
            match v {
                Some(v) => encode_with(&mut body, lsn, txid, K_PUT, |p| {
                    put_bytes(p, k);
                    put_bytes(p, v);
                }),
                None => encode_with(&mut body, lsn, txid, K_DELETE, |p| put_bytes(p, k)),
            }
            lsn += 1;
        }
        let mut commit = Vec::new();
        encode_with(&mut commit, lsn, txid, K_COMMIT, |p| {
            put_u32(p, ws.len() as u32)
        });

        self.fault(FailPoint::WalWrite)?;
        if self.faults.hit(FailPoint::WalTornWrite) {
            let _ = self
                .wal
                .as_mut()
                .unwrap()
                .write_all(&body[..body.len() / 2]);
            return Err(self.crash(FailPoint::WalTornWrite));
        }
        if self.faults.hit(FailPoint::BeforeCommitRecord) {
            let _ = self.wal.as_mut().unwrap().write_all(&body);
            return Err(self.crash(FailPoint::BeforeCommitRecord));
        }
        if self.faults.hit(FailPoint::TornCommitRecord) {
            let wal = self.wal.as_mut().unwrap();
            let _ = wal
                .write_all(&body)
                .and_then(|_| wal.write_all(&commit[..commit.len() / 2]));
            return Err(self.crash(FailPoint::TornCommitRecord));
        }
        body.extend_from_slice(&commit);
        let sync = self.sync;
        self.append(&body, sync)?;
        self.next_lsn = lsn + 1;
        self.next_txid += 1;
        if sync {
            self.durable_lsn = lsn;
        }
        self.fault(FailPoint::AfterCommitRecord)
    }

    /// Make every logged record durable.
    fn flush_wal(&mut self) -> Result<()> {
        if self.durable_lsn < self.last_lsn() {
            if let Some(wal) = &self.wal {
                if let Err(e) = wal.sync_data() {
                    self.failed = Some(format!("fsync of {WAL_FILE} failed: {e}"));
                    return Err(e.into());
                }
            }
            self.durable_lsn = self.last_lsn();
        }
        Ok(())
    }

    /// WAL before data: an image may reach disk only once every change it
    /// contains is durable in the log.
    fn ensure_logged(&self, image_lsn: Lsn) -> Result<()> {
        if self.durable_lsn.saturating_add(1) < image_lsn {
            return Err(Error::Storage(format!(
                "refusing to write a snapshot at LSN {image_lsn}: the log is durable only up to LSN {}",
                self.durable_lsn
            )));
        }
        Ok(())
    }

    /// Empty the log once a durable snapshot covers all of it.
    fn truncate_wal(&mut self) -> Result<()> {
        if let Some(wal) = self.wal.as_mut() {
            let res = wal.set_len(0).and_then(|_| wal.sync_all());
            if let Err(e) = res {
                self.failed = Some(format!("truncating {WAL_FILE} failed: {e}"));
                return Err(e.into());
            }
            self.wal_len = 0;
        }
        Ok(())
    }

    /// Write a full snapshot, log the checkpoint and empty the log. Never
    /// needed for durability, only to bound the log's size and the time
    /// recovery takes.
    ///
    /// Safe at every crash point: the log is flushed before the snapshot is
    /// written, the snapshot is swapped in by an atomic rename followed by a
    /// directory fsync, and only then is the log truncated. A crash before
    /// the rename leaves the old snapshot and the full log; a crash after it
    /// leaves log records the new snapshot already covers, which recovery
    /// recognises by LSN, skips and clears.
    pub fn checkpoint(&mut self) -> Result<()> {
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        self.check_writable()?;
        self.flush_wal()?;
        // The checkpoint takes the next LSN. The image holds every change up
        // to it (the checkpoint record itself changes nothing).
        let c = self.next_lsn;
        self.ensure_logged(c)?;
        let tmp = dir.join(format!("{SNAPSHOT_FILE}.tmp"));
        if let Err(e) = self.write_snapshot(&tmp, c) {
            if self.failed.is_none() {
                // Best effort only: a leftover temp file is harmless and is removed at the next open.
                let _ = fs::remove_file(&tmp);
            }
            return Err(e);
        }
        self.fault(FailPoint::BeforeSnapshotRename)?;
        fs::rename(&tmp, dir.join(SNAPSHOT_FILE))?;
        sync_dir(&dir)?;
        self.checkpoint_lsn = c;
        self.next_lsn = c + 1;
        self.fault(FailPoint::AfterSnapshotRename)?;

        if !self.legacy {
            // The checkpoint's commit point in the log. (A version 1 log
            // cannot hold version 2 records; it is about to be emptied.)
            let mut rec = Vec::new();
            encode_with(&mut rec, c, 0, K_CHECKPOINT, |p| put_u64(p, c));
            if let Err(e) = self.append(&rec, true) {
                // The snapshot is in place but the log now ends one LSN short:
                // let recovery reconcile them rather than appending after it.
                self.failed
                    .get_or_insert_with(|| format!("logging the checkpoint failed: {e}"));
                return Err(e);
            }
            self.durable_lsn = c;
        }
        self.fault(FailPoint::BeforeWalTruncate)?;
        // Only now, with the snapshot durable, may the log be emptied.
        self.truncate_wal()?;
        self.fault(FailPoint::AfterWalTruncate)?;
        if !self.snapshot_recorded || self.legacy {
            let f = Format {
                version: FORMAT_VERSION,
                snapshot: true,
            };
            write_atomic(&dir, FORMAT_FILE, f.text().as_bytes())?;
            self.snapshot_recorded = true;
            self.legacy = false;
        }
        self.checkpoint_retry_at = 0;
        self.last_checkpoint_error = None;
        Ok(())
    }

    fn write_snapshot(&mut self, tmp: &Path, lsn: Lsn) -> Result<()> {
        let mut w = BufWriter::new(File::create(tmp)?);
        w.write_all(&encode_record(&Record {
            lsn,
            txid: 0,
            body: Body::SnapHeader {
                next_txid: self.next_txid,
                entries: self.map.len() as u64,
            },
        }))?;
        if self.faults.hit(FailPoint::SnapshotWrite) {
            w.flush()?;
            return Err(self.crash(FailPoint::SnapshotWrite));
        }
        let mut buf = Vec::new();
        let mut entries: Vec<(&[u8], &[u8])> = Vec::with_capacity(SNAPSHOT_CHUNK);
        let mut it = self.map.iter().peekable();
        while it.peek().is_some() {
            entries.clear();
            entries.extend(
                it.by_ref()
                    .take(SNAPSHOT_CHUNK)
                    .map(|(k, v)| (k.as_slice(), v.as_slice())),
            );
            buf.clear();
            encode_with(&mut buf, lsn, 0, K_SNAP_CHUNK, |p| {
                put_u32(p, entries.len() as u32);
                for (k, v) in &entries {
                    put_bytes(p, k);
                    put_bytes(p, v);
                }
            });
            w.write_all(&buf)?;
        }
        w.write_all(&encode_record(&Record {
            lsn,
            txid: 0,
            body: Body::SnapFooter {
                entries: self.map.len() as u64,
            },
        }))?;
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        Ok(())
    }
}

/// Everything a transaction has looked at: exact keys and scanned prefixes.
/// Used at commit time to detect that another session changed that data.
#[derive(Default, Clone)]
pub struct ReadSet {
    keys: BTreeSet<Vec<u8>>,
    prefixes: Vec<Vec<u8>>,
}

impl ReadSet {
    pub fn touches(&self, key: &[u8]) -> bool {
        self.keys.contains(key) || self.prefixes.iter().any(|p| key.starts_with(p))
    }
}

/// A statement's view of the database: committed data overlaid with the
/// uncommitted writes of the current transaction.
pub struct Ctx<'a> {
    pub(crate) store: &'a Store,
    pub(crate) writes: WriteSet,
    /// Present inside explicit transactions, where reads must be validated at commit.
    pub(crate) reads: Option<RefCell<ReadSet>>,
}

impl<'a> Ctx<'a> {
    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        if let Some(v) = self.writes.get(key) {
            return v.clone();
        }
        if let Some(r) = &self.reads {
            r.borrow_mut().keys.insert(key.to_vec());
        }
        self.store.get(key).cloned()
    }

    /// Like `get`, but not recorded in a transaction's read set. Only for
    /// data that cannot change a statement's result (planner statistics),
    /// so that refreshing it never makes a concurrent transaction conflict.
    pub fn get_untracked(&self, key: &[u8]) -> Option<Vec<u8>> {
        if let Some(v) = self.writes.get(key) {
            return v.clone();
        }
        self.store.get(key).cloned()
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.writes.insert(key, Some(value));
    }

    pub fn delete(&mut self, key: Vec<u8>) {
        self.writes.insert(key, None);
    }

    /// The live value of `key`, borrowed: no copy of the stored bytes.
    pub fn get_ref(&self, key: &[u8]) -> Option<&[u8]> {
        if let Some(v) = self.writes.get(key) {
            return v.as_deref();
        }
        if let Some(r) = &self.reads {
            r.borrow_mut().keys.insert(key.to_vec());
        }
        self.store.get(key).map(Vec::as_slice)
    }

    fn note_prefix_read(&self, prefix: &[u8]) {
        if let Some(r) = &self.reads {
            let mut r = r.borrow_mut();
            if !r.prefixes.iter().any(|p| prefix.starts_with(p)) {
                r.prefixes.push(prefix.to_vec());
            }
        }
    }

    /// Live entries whose key starts with `prefix`, is `>= start` and, when
    /// `end` is given, `< end`, merged from the store and this statement's
    /// uncommitted writes and borrowed from both: nothing is copied. Ascending
    /// key order, or descending when `rev`. Inside a transaction the whole
    /// prefix is recorded as read.
    pub fn iter_range<'s>(&'s self, prefix: &[u8], start: &[u8], end: Option<&[u8]>, rev: bool) -> Merged<'s> {
        self.note_prefix_read(prefix);
        let start = if start < prefix { prefix.to_vec() } else { start.to_vec() };
        // The first key past every key starting with `prefix`.
        let mut pend = prefix.to_vec();
        while let Some(&last) = pend.last() {
            if last == 0xff {
                pend.pop();
            } else {
                *pend.last_mut().unwrap() += 1;
                break;
            }
        }
        let end: Option<Vec<u8>> = match (end, pend.is_empty()) {
            (Some(e), false) => Some(e.min(pend.as_slice()).to_vec()),
            (Some(e), true) => Some(e.to_vec()),
            (None, false) => Some(pend),
            (None, true) => None,
        };
        let hi = match &end {
            Some(e) => Bound::Excluded(e.as_slice()),
            None => Bound::Unbounded,
        };
        if end.as_deref().map_or(false, |e| e <= start.as_slice()) {
            return Merged { store: Box::new(std::iter::empty()), writes: Box::new(std::iter::empty()), sh: None, wh: None, rev };
        }
        let lo = Bound::Included(start.as_slice());
        let s = self.store.map.range::<[u8], _>((lo, hi)).map(|(k, v)| (k.as_slice(), v.as_slice()));
        let w = self.writes.range::<[u8], _>((lo, hi)).map(|(k, v)| (k.as_slice(), v.as_deref()));
        let (store, writes): (Box<dyn Iterator<Item = _> + 's>, Box<dyn Iterator<Item = _> + 's>) =
            if rev { (Box::new(s.rev()), Box::new(w.rev())) } else { (Box::new(s), Box::new(w)) };
        let mut m = Merged { store, writes, sh: None, wh: None, rev };
        m.sh = m.store.next();
        m.wh = m.writes.next();
        m
    }

    /// [`Ctx::iter_range`] over a whole prefix, ascending.
    pub fn iter_prefix<'s>(&'s self, prefix: &[u8]) -> Merged<'s> {
        self.iter_range(prefix, prefix, None, false)
    }

    /// All live entries whose key starts with `prefix`, in key order.
    pub fn scan(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.iter_prefix(prefix).map(|(k, v)| (k.to_vec(), v.to_vec())).collect()
    }

    /// Live entries whose key starts with `prefix`, is `>= start` and, when
    /// `end` is given, `< end`, in key order. `start` must itself begin with
    /// `prefix`. Inside a transaction the whole prefix is recorded as read,
    /// which is conservative but keeps conflict detection simple and sound.
    pub fn scan_range(&self, prefix: &[u8], start: &[u8], end: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.iter_range(prefix, start, end, false).map(|(k, v)| (k.to_vec(), v.to_vec())).collect()
    }
}

type StoreIter<'s> = Box<dyn Iterator<Item = (&'s [u8], &'s [u8])> + 's>;
type WriteIter<'s> = Box<dyn Iterator<Item = (&'s [u8], Option<&'s [u8]>)> + 's>;

/// Committed entries overlaid with a write set, merged in key order.
pub struct Merged<'s> {
    store: StoreIter<'s>,
    writes: WriteIter<'s>,
    sh: Option<(&'s [u8], &'s [u8])>,
    wh: Option<(&'s [u8], Option<&'s [u8]>)>,
    rev: bool,
}

impl<'s> Iterator for Merged<'s> {
    type Item = (&'s [u8], &'s [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let order = match (self.sh, self.wh) {
                (None, None) => return None,
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some((sk, _)), Some((wk, _))) => {
                    let o = sk.cmp(wk);
                    if self.rev {
                        o.reverse()
                    } else {
                        o
                    }
                }
            };
            match order {
                std::cmp::Ordering::Less => {
                    let out = self.sh;
                    self.sh = self.store.next();
                    return out;
                }
                std::cmp::Ordering::Equal | std::cmp::Ordering::Greater => {
                    if order == std::cmp::Ordering::Equal {
                        self.sh = self.store.next(); // shadowed by the write
                    }
                    let (k, v) = self.wh.expect("checked above");
                    self.wh = self.writes.next();
                    if let Some(v) = v {
                        return Some((k, v));
                    }
                }
            }
        }
    }
}

/// Whether a key range can contain anything. `BTreeMap::range` panics on an
/// inverted range (e.g. from `x BETWEEN 10 AND 5`), so callers check first.
pub fn range_is_valid<T: AsRef<[u8]>>(lo: &Bound<T>, hi: &Bound<T>) -> bool {
    match (lo, hi) {
        (Bound::Included(a), Bound::Included(b)) => a.as_ref() <= b.as_ref(),
        (Bound::Included(a), Bound::Excluded(b)) | (Bound::Excluded(a), Bound::Included(b)) => {
            a.as_ref() < b.as_ref()
        }
        (Bound::Excluded(a), Bound::Excluded(b)) => a.as_ref() < b.as_ref(),
        _ => true,
    }
}

/// The smallest key greater than every key starting with `p` (None if p is all 0xff).
pub fn prefix_successor(p: &[u8]) -> Option<Vec<u8>> {
    let mut k = p.to_vec();
    while let Some(last) = k.pop() {
        if last < 0xff {
            k.push(last + 1);
            return Some(k);
        }
    }
    None
}

#[cfg(test)]
mod wal_tests {
    use super::*;

    fn legacy_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("polydb-unit-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let put = |k: &'static [u8], v: &'static [u8]| (k, Some(v));
        fs::write(
            dir.join(SNAPSHOT_FILE),
            frame([put(b"a", b"1"), put(b"b", b"2")].into_iter()),
        )
        .unwrap();
        let mut log = frame([(&b"b"[..], None), put(b"c", b"3")].into_iter());
        log.extend(frame([put(b"d", b"4")].into_iter()));
        fs::write(dir.join(WAL_FILE), &log).unwrap();
        dir
    }

    fn contents(s: &Store) -> Vec<(Vec<u8>, Vec<u8>)> {
        s.scan(b"").map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(Vec<u8>, Vec<u8>)> {
        pairs
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect()
    }

    #[test]
    fn version_1_directory_is_converted_on_first_write_even_across_crashes() {
        let before = kv(&[("a", "1"), ("c", "3"), ("d", "4")]);
        let after = kv(&[("a", "1"), ("c", "3"), ("d", "4"), ("e", "5")]);
        let points = [
            FailPoint::SnapshotWrite,
            FailPoint::BeforeSnapshotRename,
            FailPoint::AfterSnapshotRename,
            FailPoint::BeforeWalTruncate,
            FailPoint::AfterWalTruncate,
        ];
        for point in points {
            let dir = legacy_dir(&format!("{point}"));
            let wal_v1 = fs::read(dir.join(WAL_FILE)).unwrap();
            {
                let mut s = Store::open(&dir).unwrap();
                assert!(s.recovery_report().legacy_format);
                assert_eq!(contents(&s), before);
                assert_eq!(
                    fs::read(dir.join(WAL_FILE)).unwrap(),
                    wal_v1,
                    "opening changes nothing"
                );
                s.inject(point, 1);
                let ws: WriteSet = [(b"e".to_vec(), Some(b"5".to_vec()))].into_iter().collect();
                assert!(s.commit(ws).is_err(), "{point}");
            }
            // The conversion crashed: the commit that triggered it was never
            // logged, and everything before it is intact.
            let mut s = Store::open(&dir).unwrap();
            assert_eq!(contents(&s), before, "{point}");
            let ws: WriteSet = [(b"e".to_vec(), Some(b"5".to_vec()))].into_iter().collect();
            s.commit(ws).unwrap();
            assert!(!s.legacy, "{point}: converted");
            drop(s);
            let s = Store::open(&dir).unwrap();
            assert!(!s.recovery_report().legacy_format, "{point}");
            assert_eq!(contents(&s), after, "{point}");
            assert!(fs::read_to_string(dir.join(FORMAT_FILE))
                .unwrap()
                .contains("version=2"));
            drop(s);
            fs::remove_dir_all(&dir).unwrap();
        }
    }

    #[test]
    fn wal_before_data_is_enforced() {
        let mut s = Store::memory();
        s.next_lsn = 10;
        s.durable_lsn = 5;
        assert!(
            s.ensure_logged(10).is_err(),
            "image at LSN 10 with the log durable only to 5"
        );
        s.durable_lsn = 9;
        assert!(s.ensure_logged(10).is_ok());
    }

    #[test]
    fn torn_and_corrupt_tails_are_told_apart() {
        let rec = |lsn| {
            encode_record(&Record {
                lsn,
                txid: 1,
                body: Body::Begin,
            })
        };
        let mut log = rec(1);
        log.extend(rec(2));
        let n = log.len();
        assert_eq!(scan_log(&log).1, LogTail::Clean);
        assert_eq!(scan_log(&log[..n - 4]).1, LogTail::Torn { valid: n / 2 });
        let mut zeros = log.clone();
        zeros.extend([0u8; 32]);
        assert_eq!(scan_log(&zeros).1, LogTail::Torn { valid: n });
        let mut flipped = log.clone();
        flipped[n - 1] ^= 1; // the final record, ending at end-of-file
        assert_eq!(scan_log(&flipped).1, LogTail::Torn { valid: n / 2 });
        let mut mid = log.clone();
        mid[12] ^= 1; // the first record, with an intact one after it
        assert!(matches!(
            scan_log(&mid).1,
            LogTail::Corrupt { valid: 0, .. }
        ));
    }
}
