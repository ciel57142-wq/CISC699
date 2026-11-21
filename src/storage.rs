//! The storage core: an ordered byte-key/byte-value map, made durable by a
//! checksummed write-ahead log plus periodic snapshots.
//!
//! Every commit is one WAL record holding the whole write set, so a commit is
//! atomic on disk: after a crash it is either fully replayed or (if the record
//! is torn) fully discarded.

use crate::error::{Error, Result};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions as OpenOptions_};
use std::io::{Read, Write};
use std::ops::Bound;
use std::path::{Path, PathBuf};

/// Buffered changes of a transaction: `Some(v)` is a put, `None` a delete.
pub type WriteSet = BTreeMap<Vec<u8>, Option<Vec<u8>>>;

const SNAPSHOT_FILE: &str = "data.snap";
const WAL_FILE: &str = "data.wal";
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

/// Record layout: `len:u32 | crc32(payload):u32 | payload`
/// Payload: `count:u32 | { tag:u8 (1=put,0=del) | klen:u32 | key | [vlen:u32 | value] }*`
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

fn decode_payload(p: &[u8]) -> Option<Vec<(Vec<u8>, Option<Vec<u8>>)>> {
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

/// How to open a store directory.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Open a log with interior corruption anyway, keeping only the commits
    /// before the damage (the damaged file is first copied to `data.wal.corrupt`).
    /// Off by default: corruption is reported as [`Error::Corrupt`].
    pub salvage: bool,
}

const LOCK_FILE: &str = "LOCK";

pub struct Store {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Held for the lifetime of the store: an exclusive OS lock on `dir/LOCK`
    /// so two processes can never write the same database directory.
    _lock: Option<File>,
    dir: Option<PathBuf>,
    wal: Option<File>,
    wal_len: u64,
    /// fsync the WAL on every commit (default true). Turning this off trades
    /// durability of the last few commits on power loss for speed.
    pub sync: bool,
    /// The WAL is folded into a snapshot once it grows past this many bytes.
    pub checkpoint_bytes: u64,
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
            sync: true,
            checkpoint_bytes: 64 << 20,
        }
    }

    /// Open (or create) a store in `dir`, recovering from the snapshot and WAL.
    pub fn open(dir: impl AsRef<Path>) -> Result<Store> {
        Store::open_with(dir, &OpenOptions::default())
    }

    pub fn open_with(dir: impl AsRef<Path>, opts: &OpenOptions) -> Result<Store> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let lock = OpenOptions_::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(LOCK_FILE))?;
        match lock.try_lock() {
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
        let mut map = BTreeMap::new();

        let snap = dir.join(SNAPSHOT_FILE);
        if snap.exists() {
            let buf = fs::read(&snap)?;
            if replay(&buf, &mut map) != LogTail::Clean {
                return Err(Error::Corrupt(format!(
                    "snapshot {} failed its checksum",
                    snap.display()
                )));
            }
        }

        let wal_path = dir.join(WAL_FILE);
        let mut wal = OpenOptions_::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&wal_path)?;
        let mut buf = Vec::new();
        wal.read_to_end(&mut buf)?;
        let valid = match replay(&buf, &mut map) {
            LogTail::Clean => buf.len(),
            // A torn final record from a crash mid-commit: that commit never
            // completed (or was never acknowledged), so drop it.
            LogTail::Torn { valid } => valid,
            LogTail::Corrupt { valid, reason } => {
                if !opts.salvage {
                    return Err(Error::Corrupt(format!(
                        "write-ahead log {}: {reason}; refusing to open (open with salvage to keep the {valid} intact byte(s) before it)",
                        wal_path.display()
                    )));
                }
                fs::write(dir.join(format!("{WAL_FILE}.corrupt")), &buf)?;
                valid
            }
        } as u64;
        if valid < buf.len() as u64 {
            wal.set_len(valid)?;
            wal.sync_all()?;
        }
        Ok(Store {
            map,
            _lock: Some(lock),
            dir: Some(dir),
            wal: Some(wal),
            wal_len: valid,
            sync: true,
            checkpoint_bytes: 64 << 20,
        })
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
    ) -> impl Iterator<Item = (&'a Vec<u8>, &'a Vec<u8>)> + 'a {
        self.map.range::<[u8], _>((lo, hi))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Atomically and durably apply a write set.
    pub fn commit(&mut self, ws: WriteSet) -> Result<()> {
        if ws.is_empty() {
            return Ok(());
        }
        if let Some(wal) = self.wal.as_mut() {
            let rec = frame(ws.iter().map(|(k, v)| (k.as_slice(), v.as_deref())));
            let res =
                wal.write_all(&rec)
                    .and_then(|_| if self.sync { wal.sync_data() } else { Ok(()) });
            if let Err(e) = res {
                // Roll the log back so a half-written record cannot sit in front of later commits.
                let _ = wal.set_len(self.wal_len);
                return Err(e.into());
            }
            self.wal_len += rec.len() as u64;
        }
        for (k, v) in ws {
            match v {
                Some(v) => {
                    self.map.insert(k, v);
                }
                None => {
                    self.map.remove(&k);
                }
            }
        }
        if self.wal.is_some() && self.wal_len > self.checkpoint_bytes {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Write a full snapshot and empty the WAL. Safe at any crash point: the
    /// snapshot is swapped in by atomic rename, and replaying a stale WAL over
    /// a newer snapshot is idempotent.
    pub fn checkpoint(&mut self) -> Result<()> {
        let Some(dir) = self.dir.clone() else {
            return Ok(());
        };
        let tmp = dir.join(format!("{SNAPSHOT_FILE}.tmp"));
        {
            let mut f = File::create(&tmp)?;
            let mut chunk: Vec<(&[u8], Option<&[u8]>)> = Vec::with_capacity(SNAPSHOT_CHUNK);
            for (k, v) in &self.map {
                chunk.push((k.as_slice(), Some(v.as_slice())));
                if chunk.len() == SNAPSHOT_CHUNK {
                    f.write_all(&frame(chunk.drain(..)))?;
                }
            }
            if !chunk.is_empty() {
                f.write_all(&frame(chunk.drain(..)))?;
            }
            f.sync_all()?;
        }
        fs::rename(&tmp, dir.join(SNAPSHOT_FILE))?;
        if let Ok(d) = File::open(&dir) {
            let _ = d.sync_all(); // persist the rename where the platform allows it
        }
        if let Some(wal) = self.wal.as_mut() {
            wal.set_len(0)?;
            wal.sync_all()?;
        }
        self.wal_len = 0;
        Ok(())
    }
}

/// Everything a transaction has looked at: exact keys and scanned prefixes.
/// Used at commit time to detect that another session changed that data.
#[derive(Default, Clone)]
pub struct ReadSet {
    keys: BTreeSet<Vec<u8>>,
    prefixes: Vec<Vec<u8>>,
    ranges: Vec<(Bound<Vec<u8>>, Bound<Vec<u8>>)>,
}

fn in_range(key: &[u8], lo: &Bound<Vec<u8>>, hi: &Bound<Vec<u8>>) -> bool {
    let above = match lo {
        Bound::Included(l) => key >= l.as_slice(),
        Bound::Excluded(l) => key > l.as_slice(),
        Bound::Unbounded => true,
    };
    let below = match hi {
        Bound::Included(h) => key <= h.as_slice(),
        Bound::Excluded(h) => key < h.as_slice(),
        Bound::Unbounded => true,
    };
    above && below
}

impl ReadSet {
    pub fn touches(&self, key: &[u8]) -> bool {
        self.keys.contains(key)
            || self.prefixes.iter().any(|p| key.starts_with(p))
            || self.ranges.iter().any(|(lo, hi)| in_range(key, lo, hi))
    }
}

/// A statement's view of the database: committed data overlaid with the
/// uncommitted writes of the current transaction.
pub struct Ctx<'a> {
    pub(crate) store: &'a Store,
    pub(crate) writes: WriteSet,
    /// Present inside explicit transactions, where reads must be validated at commit.
    pub(crate) reads: Option<RefCell<ReadSet>>,
    /// Execution settings (memory budget, cancel flag) for queries run in this context.
    pub(crate) exec: crate::query::exec::ExecOpts,
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

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.writes.insert(key, Some(value));
    }

    pub fn delete(&mut self, key: Vec<u8>) {
        self.writes.insert(key, None);
    }

    /// All live entries whose key starts with `prefix`, in key order.
    pub fn scan(&self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        if let Some(r) = &self.reads {
            let mut r = r.borrow_mut();
            if !r.prefixes.iter().any(|p| prefix.starts_with(p)) {
                r.prefixes.push(prefix.to_vec());
            }
        }
        let mut out: BTreeMap<Vec<u8>, Vec<u8>> = self
            .store
            .scan(prefix)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in self
            .writes
            .range::<[u8], _>((Bound::Included(prefix), Bound::Unbounded))
        {
            if !k.starts_with(prefix) {
                break;
            }
            match v {
                Some(v) => {
                    out.insert(k.clone(), v.clone());
                }
                None => {
                    out.remove(k);
                }
            }
        }
        out.into_iter().collect()
    }

    /// All live entries with keys inside the bounds, in key order. Inside a
    /// transaction the whole range is recorded, so a concurrent insert into
    /// it (a phantom) makes the commit fail.
    pub fn range(&self, lo: Bound<&[u8]>, hi: Bound<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
        if let Some(r) = &self.reads {
            r.borrow_mut()
                .ranges
                .push((lo.map(<[u8]>::to_vec), hi.map(<[u8]>::to_vec)));
        }
        let mut out: BTreeMap<Vec<u8>, Vec<u8>> = self
            .store
            .range(lo, hi)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (k, v) in self.writes.range::<[u8], _>((lo, hi)) {
            match v {
                Some(v) => {
                    out.insert(k.clone(), v.clone());
                }
                None => {
                    out.remove(k);
                }
            }
        }
        out.into_iter().collect()
    }

    /// Stream the live entries inside the bounds without materialising them,
    /// in ascending key order (or descending with `rev`). Committed data is
    /// merged with this transaction's uncommitted writes on the fly. Inside a
    /// transaction the whole range is recorded for conflict detection.
    pub fn iter_range<'c>(
        &'c self,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
        rev: bool,
    ) -> MergeIter<'c> {
        if let Some(r) = &self.reads {
            r.borrow_mut().ranges.push((lo.clone(), hi.clone()));
        }
        let r = (lo, hi);
        let store: Box<dyn Iterator<Item = (&'c Vec<u8>, &'c Vec<u8>)> + 'c> = if rev {
            Box::new(self.store.map.range::<Vec<u8>, _>(r.clone()).rev())
        } else {
            Box::new(self.store.map.range::<Vec<u8>, _>(r.clone()))
        };
        let writes: Box<dyn Iterator<Item = (&'c Vec<u8>, &'c Option<Vec<u8>>)> + 'c> = if rev {
            Box::new(self.writes.range::<Vec<u8>, _>(r).rev())
        } else {
            Box::new(self.writes.range::<Vec<u8>, _>(r))
        };
        MergeIter {
            store: store.peekable(),
            writes: writes.peekable(),
            rev,
        }
    }

    /// Stream the live entries whose key starts with `prefix`.
    pub fn iter_prefix<'c>(&'c self, prefix: &[u8], rev: bool) -> MergeIter<'c> {
        let hi = prefix_successor(prefix).map_or(Bound::Unbounded, Bound::Excluded);
        self.iter_range(Bound::Included(prefix.to_vec()), hi, rev)
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

type StoreIter<'c> = std::iter::Peekable<Box<dyn Iterator<Item = (&'c Vec<u8>, &'c Vec<u8>)> + 'c>>;
type WriteIter<'c> =
    std::iter::Peekable<Box<dyn Iterator<Item = (&'c Vec<u8>, &'c Option<Vec<u8>>)> + 'c>>;

/// Merge of committed entries and uncommitted writes (writes win; deletes hide).
pub struct MergeIter<'c> {
    store: StoreIter<'c>,
    writes: WriteIter<'c>,
    rev: bool,
}

impl Iterator for MergeIter<'_> {
    type Item = (Vec<u8>, Vec<u8>);
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let order = match (self.store.peek(), self.writes.peek()) {
                (None, None) => return None,
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (Some((a, _)), Some((b, _))) => {
                    let o = a.as_slice().cmp(b.as_slice());
                    if self.rev {
                        o.reverse()
                    } else {
                        o
                    }
                }
            };
            match order {
                std::cmp::Ordering::Less => {
                    let (k, v) = self.store.next()?;
                    return Some((k.clone(), v.clone()));
                }
                std::cmp::Ordering::Equal => {
                    self.store.next();
                    let (k, v) = self.writes.next()?;
                    if let Some(v) = v {
                        return Some((k.clone(), v.clone()));
                    }
                }
                std::cmp::Ordering::Greater => {
                    let (k, v) = self.writes.next()?;
                    if let Some(v) = v {
                        return Some((k.clone(), v.clone()));
                    }
                }
            }
        }
    }
}
