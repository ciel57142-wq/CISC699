//! Key-value model: string keys, opaque byte values, optional expiry.
//!
//! An entry with a time-to-live has a companion key (`x\0<entry key>`)
//! holding its expiry time in Unix milliseconds, and an entry in the expiry
//! index (`X\0<expiry as big-endian u64><entry key>`), which is ordered by
//! time. Expired entries are invisible to every read (point reads and scans,
//! in every front end) from the moment they expire; [`purge_expired`] (run
//! by the database's background reaper, and on open) deletes them by
//! reading only the due prefix of the expiry index, never the whole keyspace.

use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ttl_key(full: &[u8]) -> Vec<u8> {
    let mut k = b"x\0".to_vec();
    k.extend_from_slice(full);
    k
}

const EXPIRY_INDEX: &[u8] = b"X\0";

fn index_key(at: i64, full: &[u8]) -> Vec<u8> {
    let mut k = EXPIRY_INDEX.to_vec();
    k.extend_from_slice(&(at.max(0) as u64).to_be_bytes());
    k.extend_from_slice(full);
    k
}

/// Record (or clear, with `None`) the expiry of an entry, keeping the index in step.
fn set_expiry(ctx: &mut Ctx, full: &[u8], at: Option<i64>) {
    if let Some(old) = expiry(ctx, full) {
        ctx.delete(index_key(old, full));
    }
    match at {
        Some(at) => {
            ctx.put(ttl_key(full), at.to_string().into_bytes());
            ctx.put(index_key(at, full), Vec::new());
        }
        None => {
            if ctx.get(&ttl_key(full)).is_some() {
                ctx.delete(ttl_key(full));
            }
        }
    }
}

/// Delete up to `max` entries that expired at or before `now_ms`; returns how many.
/// Reads only the due part of the expiry index.
pub fn purge_expired(ctx: &mut Ctx, now: i64, max: usize) -> usize {
    let hi = index_key(now.saturating_add(1), b"");
    let due: Vec<Vec<u8>> = ctx
        .iter_range(
            std::ops::Bound::Included(EXPIRY_INDEX.to_vec()),
            std::ops::Bound::Excluded(hi),
            false,
        )
        .take(max)
        .map(|(k, _)| k)
        .collect();
    let mut n = 0;
    for ik in due {
        let at = u64::from_be_bytes(ik[2..10].try_into().unwrap_or([0; 8])) as i64;
        let full = ik[10..].to_vec();
        ctx.delete(ik);
        // only if this index entry is still the key's current expiry
        if expiry(ctx, &full) == Some(at) {
            ctx.delete(ttl_key(&full));
            if ctx.get(&full).is_some() {
                ctx.delete(full);
                n += 1;
            }
        }
    }
    n
}

/// Expiry time of the earliest indexed entry, if any.
pub fn next_expiry(ctx: &Ctx) -> Option<i64> {
    let hi = crate::storage::prefix_successor(EXPIRY_INDEX)
        .map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
    ctx.iter_range(std::ops::Bound::Included(EXPIRY_INDEX.to_vec()), hi, false)
        .next()
        .map(|(k, _)| u64::from_be_bytes(k[2..10].try_into().unwrap_or([0; 8])) as i64)
}

/// Give TTL records written by an earlier format their expiry-index entry.
pub fn index_untracked_expiries(ctx: &mut Ctx) -> usize {
    let mut n = 0;
    for (k, v) in ctx.scan(b"x\0") {
        let full = &k[2..];
        if let Some(at) = std::str::from_utf8(&v)
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
        {
            let ik = index_key(at, full);
            if ctx.get(&ik).is_none() {
                ctx.put(ik, Vec::new());
                n += 1;
            }
        }
    }
    n
}

/// Expiry (Unix ms) of an entry, if it has one.
pub fn expiry(ctx: &Ctx, full: &[u8]) -> Option<i64> {
    ctx.get(&ttl_key(full))
        .and_then(|raw| std::str::from_utf8(&raw).ok()?.parse().ok())
}

/// Whether an entry (known to exist) has expired.
pub fn is_expired(ctx: &Ctx, full: &[u8]) -> bool {
    expiry(ctx, full).is_some_and(|at| at <= now_ms())
}

/// The value of a live (present, unexpired) entry.
pub fn live(ctx: &Ctx, full: &[u8]) -> Option<Vec<u8>> {
    let v = ctx.get(full)?;
    (!is_expired(ctx, full)).then_some(v)
}

/// Write an entry; `ttl_ms` sets a time-to-live, `None` makes it persistent.
pub fn put(ctx: &mut Ctx, full: Vec<u8>, value: Vec<u8>, ttl_ms: Option<i64>) {
    set_expiry(ctx, &full, ttl_ms.map(|ms| now_ms().saturating_add(ms)));
    ctx.put(full, value);
}

/// Delete an entry and its expiry; returns whether a live entry existed.
pub fn remove(ctx: &mut Ctx, full: Vec<u8>) -> bool {
    let existed = live(ctx, &full).is_some();
    if ctx.get(&full).is_some() {
        ctx.delete(full.clone());
    }
    set_expiry(ctx, &full, None);
    existed
}

/// Set or clear the time-to-live of a live entry; returns whether it exists.
pub fn set_ttl(ctx: &mut Ctx, full: &[u8], ttl_ms: Option<i64>) -> bool {
    if live(ctx, full).is_none() {
        return false;
    }
    match ttl_ms {
        Some(ms) if ms <= 0 => {
            remove(ctx, full.to_vec());
        }
        Some(ms) => set_expiry(ctx, full, Some(now_ms().saturating_add(ms))),
        None => set_expiry(ctx, full, None),
    }
    true
}

/// Remaining time-to-live in ms: `None` if the key does not exist, `Some(-1)` if it never expires.
pub fn ttl_left(ctx: &Ctx, full: &[u8]) -> Option<i64> {
    live(ctx, full)?;
    Some(expiry(ctx, full).map_or(-1, |at| (at - now_ms()).max(0)))
}

pub fn set(ctx: &mut Ctx, key: &str, value: &[u8]) {
    put(ctx, keys::kv(key), value.to_vec(), None);
}

pub fn set_with_ttl(ctx: &mut Ctx, key: &str, value: &[u8], ttl_ms: Option<i64>) {
    put(ctx, keys::kv(key), value.to_vec(), ttl_ms);
}

pub fn get(ctx: &Ctx, key: &str) -> Option<Vec<u8>> {
    live(ctx, &keys::kv(key))
}

/// Returns whether the key existed.
pub fn del(ctx: &mut Ctx, key: &str) -> bool {
    remove(ctx, keys::kv(key))
}

/// Keys starting with `prefix`, in sorted order.
pub fn keys(ctx: &Ctx, prefix: &str) -> Vec<String> {
    ctx.scan(&keys::kv(prefix))
        .into_iter()
        .filter(|(k, _)| !is_expired(ctx, k))
        .map(|(k, _)| String::from_utf8_lossy(&k[keys::KV_PREFIX.len()..]).into_owned())
        .collect()
}

/// Add `by` to an integer value (missing keys count as 0) and return the result.
/// An existing time-to-live is kept.
pub fn incr(ctx: &mut Ctx, key: &str, by: i64) -> Result<i64> {
    incr_full(ctx, keys::kv(key), key, by)
}

pub fn incr_full(ctx: &mut Ctx, full: Vec<u8>, key: &str, by: i64) -> Result<i64> {
    let cur = match live(ctx, &full) {
        Some(raw) => std::str::from_utf8(&raw)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .ok_or_else(|| Error::Type(format!("value at '{key}' is not an integer")))?,
        None => {
            remove(ctx, full.clone()); // drop an expired entry and its expiry
            0
        }
    };
    let next = cur
        .checked_add(by)
        .ok_or_else(|| Error::Type("integer overflow".into()))?;
    ctx.put(full, next.to_string().into_bytes());
    Ok(next)
}
