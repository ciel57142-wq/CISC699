//! Key-value model: string keys, opaque byte values, optional expiry.
//!
//! An entry with a time-to-live has a companion key (`x\0<entry key>`)
//! holding its expiry time in Unix milliseconds. Expired entries are
//! invisible to every read (point reads and scans, in every front end) and
//! are physically removed the next time the key is written or deleted.

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
    match ttl_ms {
        Some(ms) => ctx.put(
            ttl_key(&full),
            (now_ms().saturating_add(ms)).to_string().into_bytes(),
        ),
        None => {
            if ctx.get(&ttl_key(&full)).is_some() {
                ctx.delete(ttl_key(&full));
            }
        }
    }
    ctx.put(full, value);
}

/// Delete an entry and its expiry; returns whether a live entry existed.
pub fn remove(ctx: &mut Ctx, full: Vec<u8>) -> bool {
    let existed = live(ctx, &full).is_some();
    if ctx.get(&full).is_some() {
        ctx.delete(full.clone());
    }
    if ctx.get(&ttl_key(&full)).is_some() {
        ctx.delete(ttl_key(&full));
    }
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
        Some(ms) => ctx.put(
            ttl_key(full),
            (now_ms().saturating_add(ms)).to_string().into_bytes(),
        ),
        None => {
            if ctx.get(&ttl_key(full)).is_some() {
                ctx.delete(ttl_key(full));
            }
        }
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
