//! Planner statistics and selectivity / cardinality estimation.
//!
//! `ANALYZE [table]` reads every row of a table once and stores, under
//! `m\0a\0<table>`:
//!
//! * the row count;
//! * per column: number of distinct non-NULL values, number of NULLs, up to
//!   [`MCV_LIMIT`] most common values (seen more than once) with their
//!   frequencies, and an
//!   equi-depth histogram of up to [`HIST_POINTS`] quantile points over the
//!   non-NULL values (the minimum and maximum are its first and last point).
//!
//! Statistics only guide plan choice. Every candidate row is still checked
//! against the full condition, so a stale or missing entry can make a plan
//! slower but never makes an answer wrong. They are read without entering a
//! transaction's read set, so a concurrent ANALYZE never causes a conflict.
//!
//! Without statistics the estimator falls back to fixed defaults (a table of
//! [`DEFAULT_ROWS`] rows, an equality matching [`DEFAULT_EQ_SEL`] of them, a
//! one-sided range [`DEFAULT_RANGE_SEL`]), which keep the old preference
//! order: unique lookups, then longer index prefixes, then ranges, then a
//! full scan.

use super::catalog::{all_rows, group_key, Schema};
use crate::error::Result;
use crate::keys;
use crate::storage::Ctx;
use crate::value::Value;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;

/// Parsed statistics by table, with the stored bytes they came from.
type StatsCache = HashMap<String, (Vec<u8>, Rc<TableStats>)>;

thread_local! {
    static STATS_CACHE: RefCell<StatsCache> = RefCell::new(HashMap::new());
}

pub(super) const DEFAULT_ROWS: f64 = 1000.0;
pub(super) const DEFAULT_EQ_SEL: f64 = 0.01;
pub(super) const DEFAULT_RANGE_SEL: f64 = 1.0 / 3.0;
/// Selectivity of a condition the estimator cannot reason about.
pub(super) const DEFAULT_OTHER_SEL: f64 = 1.0 / 3.0;
const MCV_LIMIT: usize = 10;
const HIST_POINTS: usize = 101;

#[derive(Debug, Clone, Default)]
pub(super) struct ColStats {
    pub ndv: f64,
    pub nulls: f64,
    /// Most common values, most frequent first, with their fraction of all rows.
    pub mcv: Vec<(Value, f64)>,
    /// Sorted quantile points of the non-NULL values.
    pub hist: Vec<Value>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct TableStats {
    pub rows: f64,
    pub cols: HashMap<String, ColStats>,
}

impl TableStats {
    fn to_json(&self) -> String {
        let mut cols = BTreeMap::new();
        for (name, c) in &self.cols {
            let mut o = BTreeMap::new();
            o.insert("ndv".to_string(), Value::Float(c.ndv));
            o.insert("nulls".to_string(), Value::Float(c.nulls));
            o.insert(
                "mcv".to_string(),
                Value::Array(c.mcv.iter().map(|(v, f)| Value::Array(vec![v.clone(), Value::Float(*f)])).collect()),
            );
            o.insert("hist".to_string(), Value::Array(c.hist.clone()));
            cols.insert(name.clone(), Value::Object(o));
        }
        let mut o = BTreeMap::new();
        o.insert("rows".to_string(), Value::Float(self.rows));
        o.insert("cols".to_string(), Value::Object(cols));
        Value::Object(o).to_json()
    }

    fn parse(raw: &[u8]) -> Option<TableStats> {
        let v = Value::parse(std::str::from_utf8(raw).ok()?).ok()?;
        let mut st = TableStats {
            rows: v.get("rows")?.as_f64()?,
            cols: HashMap::new(),
        };
        if let Some(Value::Object(cols)) = v.get("cols") {
            for (name, c) in cols {
                let mcv = match c.get("mcv") {
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(|p| match p {
                            Value::Array(p) if p.len() == 2 => Some((p[0].clone(), p[1].as_f64()?)),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let hist = match c.get("hist") {
                    Some(Value::Array(a)) => a.clone(),
                    _ => Vec::new(),
                };
                st.cols.insert(
                    name.clone(),
                    ColStats {
                        ndv: c.get("ndv").and_then(Value::as_f64).unwrap_or(0.0),
                        nulls: c.get("nulls").and_then(Value::as_f64).unwrap_or(0.0),
                        mcv,
                        hist,
                    },
                );
            }
        }
        Some(st)
    }

    /// The stored statistics of a table, parsed once per thread while the
    /// stored bytes stay the same.
    pub fn load(ctx: &Ctx, table: &str) -> Option<Rc<TableStats>> {
        let raw = ctx.get_untracked(&keys::table_stats(table))?;
        let hit = STATS_CACHE.with(|c| c.borrow().get(table).and_then(|(b, st)| (b == &raw).then(|| st.clone())));
        if hit.is_some() {
            return hit;
        }
        let st = Rc::new(TableStats::parse(&raw)?);
        STATS_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() >= 256 {
                c.clear();
            }
            c.insert(table.to_string(), (raw, st.clone()));
        });
        Some(st)
    }
}

/// Compute statistics of one table from its rows.
pub(super) fn analyze_table(ctx: &Ctx, s: &Schema) -> Result<TableStats> {
    let rows = all_rows(ctx, s)?;
    let n = rows.len();
    let mut st = TableStats {
        rows: n as f64,
        cols: HashMap::new(),
    };
    for (ci, col) in s.cols.iter().enumerate() {
        let mut counts: HashMap<Vec<u8>, (usize, &Value)> = HashMap::new();
        let mut nulls = 0usize;
        let mut vals: Vec<&Value> = Vec::with_capacity(n);
        for (_, r) in &rows {
            let v = &r[ci];
            if v.is_null() {
                nulls += 1;
                continue;
            }
            counts.entry(group_key([v])).or_insert((0, v)).0 += 1;
            vals.push(v);
        }
        let ndv = counts.len();
        let mut freq: Vec<(usize, &Value)> = counts.into_values().filter(|(c, _)| *c > 1).collect();
        freq.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.total_cmp(b.1)));
        let mcv = freq.into_iter().take(MCV_LIMIT).map(|(c, v)| (v.clone(), c as f64 / n as f64)).collect();
        vals.sort_by(|a, b| a.total_cmp(b));
        let hist = if vals.len() <= HIST_POINTS {
            vals.iter().map(|v| (*v).clone()).collect()
        } else {
            (0..HIST_POINTS).map(|i| vals[i * (vals.len() - 1) / (HIST_POINTS - 1)].clone()).collect()
        };
        st.cols.insert(
            col.name.clone(),
            ColStats {
                ndv: ndv as f64,
                nulls: nulls as f64,
                mcv,
                hist,
            },
        );
    }
    Ok(st)
}

pub(super) fn save(ctx: &mut Ctx, table: &str, st: &TableStats) {
    ctx.put(keys::table_stats(table), st.to_json().into_bytes());
}

pub(super) fn forget(ctx: &mut Ctx, table: &str) {
    if ctx.get_untracked(&keys::table_stats(table)).is_some() {
        ctx.delete(keys::table_stats(table));
    }
}

/// A bound of a range predicate: value and whether it is inclusive.
pub(super) type Bound<'a> = Option<(&'a Value, bool)>;

/// Estimates for one table, from its statistics or from the defaults.
pub(super) struct Estimator<'a> {
    pub s: &'a Schema,
    pub st: Option<Rc<TableStats>>,
}

impl<'a> Estimator<'a> {
    pub fn new(ctx: &Ctx, s: &'a Schema) -> Estimator<'a> {
        Estimator {
            s,
            st: Self::stats_for(ctx, s),
        }
    }

    /// The statistics usable for `s` (None when missing or for other columns).
    pub fn stats_for(ctx: &Ctx, s: &Schema) -> Option<Rc<TableStats>> {
        TableStats::load(ctx, &s.name).filter(|st| s.cols.iter().all(|c| st.cols.contains_key(&c.name)))
    }

    pub fn rows(&self) -> f64 {
        self.st.as_ref().map_or(DEFAULT_ROWS, |st| st.rows)
    }

    fn col(&self, ci: usize) -> Option<&ColStats> {
        self.st.as_ref()?.cols.get(&self.s.cols[ci].name)
    }

    fn null_frac(&self, ci: usize) -> f64 {
        match (self.col(ci), self.rows()) {
            (Some(c), r) if r > 0.0 => c.nulls / r,
            _ => 0.0,
        }
    }

    /// Is column `ci` alone the primary key or a unique index?
    fn unique_col(&self, ci: usize) -> bool {
        self.s.is_unique_set(std::slice::from_ref(&self.s.cols[ci].name))
    }

    /// Distinct non-NULL values of a column.
    pub fn ndv(&self, ci: usize) -> f64 {
        if let Some(c) = self.col(ci) {
            return c.ndv.max(1.0);
        }
        if self.unique_col(ci) {
            self.rows().max(1.0)
        } else {
            (1.0 / DEFAULT_EQ_SEL).min(self.rows()).max(1.0)
        }
    }

    /// Fraction of rows with `col = v`.
    pub fn eq_sel(&self, ci: usize, v: &Value) -> f64 {
        let rows = self.rows();
        if rows <= 0.0 {
            return 0.0;
        }
        let Some(c) = self.col(ci) else {
            return if self.unique_col(ci) { 1.0 / rows } else { DEFAULT_EQ_SEL };
        };
        if let Some((_, f)) = c.mcv.iter().find(|(m, _)| m.total_cmp(v) == Ordering::Equal) {
            return *f;
        }
        if let (Some(lo), Some(hi)) = (c.hist.first(), c.hist.last()) {
            if v.total_cmp(lo) == Ordering::Less || v.total_cmp(hi) == Ordering::Greater {
                return 0.0;
            }
        }
        let mcv_total: f64 = c.mcv.iter().map(|(_, f)| f).sum();
        let rest = (1.0 - mcv_total - self.null_frac(ci)).max(0.0);
        let others = (c.ndv - c.mcv.len() as f64).max(1.0);
        (rest / others).min(1.0)
    }

    /// Fraction of rows with `col = x` for a value not known in advance.
    pub fn eq_sel_unknown(&self, ci: usize) -> f64 {
        if self.rows() <= 0.0 {
            return 0.0;
        }
        if self.col(ci).is_none() && !self.unique_col(ci) {
            return DEFAULT_EQ_SEL;
        }
        ((1.0 - self.null_frac(ci)) / self.ndv(ci)).min(1.0)
    }

    pub fn in_sel(&self, ci: usize, vals: &[Value]) -> f64 {
        let mut seen = std::collections::HashSet::new();
        vals.iter()
            .filter(|v| seen.insert(group_key([*v])))
            .map(|v| self.eq_sel(ci, v))
            .sum::<f64>()
            .min(1.0)
    }

    /// Fraction of the non-NULL values strictly below `v` (`inclusive`
    /// false) or up to `v`. When the histogram holds every value it counts
    /// them; otherwise its points are quantiles, and a value between two of
    /// them is placed by linear interpolation (numbers) or in the middle.
    fn frac_below(c: &ColStats, nonnull: f64, v: &Value, inclusive: bool) -> f64 {
        let h = &c.hist;
        let n = h.len();
        if n == 0 {
            return 0.0;
        }
        let lt = h.partition_point(|x| x.total_cmp(v) == Ordering::Less);
        let le = h.partition_point(|x| x.total_cmp(v) != Ordering::Greater);
        if (n as f64 - nonnull).abs() < 0.5 {
            return (if inclusive { le } else { lt }) as f64 / n as f64;
        }
        if n == 1 {
            // one point: everything at or below it, nothing below it
            return if le == 0 {
                0.0
            } else if lt == n || inclusive {
                1.0
            } else {
                0.0
            };
        }
        let span = (n - 1) as f64;
        if le > lt {
            // v is a quantile point (possibly repeated): its run spans lt..le-1
            return if inclusive { (le - 1) as f64 / span } else { lt as f64 / span };
        }
        if lt == 0 {
            return 0.0;
        }
        if lt == n {
            return 1.0;
        }
        let (a, b) = (&h[lt - 1], &h[lt]);
        let t = match (a.as_f64(), b.as_f64(), v.as_f64()) {
            (Some(a), Some(b), Some(x)) if b > a => ((x - a) / (b - a)).clamp(0.0, 1.0),
            _ => 0.5,
        };
        ((lt - 1) as f64 + t) / span
    }

    /// Fraction of rows with `lo <= col <= hi` (each bound optional).
    pub fn range_sel(&self, ci: usize, lo: Bound, hi: Bound) -> f64 {
        if lo.is_none() && hi.is_none() {
            return 1.0;
        }
        let Some(c) = self.col(ci) else {
            return if lo.is_some() && hi.is_some() {
                DEFAULT_RANGE_SEL * DEFAULT_RANGE_SEL
            } else {
                DEFAULT_RANGE_SEL
            };
        };
        let nonnull = (self.rows() - c.nulls).max(0.0);
        let top = hi.map_or(1.0, |(v, inc)| Self::frac_below(c, nonnull, v, inc));
        let bottom = lo.map_or(0.0, |(v, inc)| Self::frac_below(c, nonnull, v, !inc));
        ((top - bottom).max(0.0)) * (1.0 - self.null_frac(ci))
    }

    pub fn null_sel(&self, ci: usize, is_null: bool) -> f64 {
        let f = if self.col(ci).is_some() { self.null_frac(ci) } else { DEFAULT_EQ_SEL };
        if is_null {
            f
        } else {
            1.0 - f
        }
    }
}

/// Estimated number of rows of a join of `l` and `r` rows whose equality
/// keys have the given distinct counts (the larger one bounds the matches).
pub(super) fn join_rows(l: f64, r: f64, key_ndvs: &[(f64, f64)]) -> f64 {
    let mut rows = l * r;
    for (a, b) in key_ndvs {
        rows /= a.max(*b).max(1.0);
    }
    rows
}
