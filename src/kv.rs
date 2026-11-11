//! Key-value model: string keys, opaque byte values.

use crate::error::{Error, Result};
use crate::keys;
use crate::storage::Ctx;

pub fn set(ctx: &mut Ctx, key: &str, value: &[u8]) {
    ctx.put(keys::kv(key), value.to_vec());
}

pub fn get(ctx: &Ctx, key: &str) -> Option<Vec<u8>> {
    ctx.get(&keys::kv(key))
}

/// Returns whether the key existed.
pub fn del(ctx: &mut Ctx, key: &str) -> bool {
    let k = keys::kv(key);
    let existed = ctx.get(&k).is_some();
    if existed {
        ctx.delete(k);
    }
    existed
}

/// Keys starting with `prefix`, in sorted order.
pub fn keys(ctx: &Ctx, prefix: &str) -> Vec<String> {
    ctx.scan(&keys::kv(prefix))
        .into_iter()
        .map(|(k, _)| String::from_utf8_lossy(&k[keys::KV_PREFIX.len()..]).into_owned())
        .collect()
}

/// Add `by` to an integer value (missing keys count as 0) and return the result.
pub fn incr(ctx: &mut Ctx, key: &str, by: i64) -> Result<i64> {
    let cur = match get(ctx, key) {
        Some(raw) => std::str::from_utf8(&raw)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .ok_or_else(|| Error::Type(format!("value at '{key}' is not an integer")))?,
        None => 0,
    };
    let next = cur.checked_add(by).ok_or_else(|| Error::Type("integer overflow".into()))?;
    set(ctx, key, next.to_string().as_bytes());
    Ok(next)
}
