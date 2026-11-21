//! Physical execution of planned queries.
//!
//! The executor is pull-based: every operator is an iterator that produces
//! rows on demand, so scans, filters, projections, unions, limits, index
//! lookup joins and the probe side of hash and nested-loop joins stream, and
//! `LIMIT` stops reading as soon as it has enough rows.
//!
//! Blocking operators keep their memory within `ExecOpts::mem_limit`:
//! sorting is an external merge sort that spills sorted runs to disk, hash
//! aggregation spills partial group states as sorted runs and merges them,
//! a hash join whose build side is too large becomes a partitioned (grace)
//! join on disk, and the inner side of a nested loop spills to a re-readable
//! buffer. DISTINCT / UNION de-duplication is bounded by the same limit and
//! fails with a clear error beyond it.
//!
//! Every operator checks the session's cancel flag on each row pulled.

use super::eval::{coerce_type, eval, is_true, Env, SubRunner};
use super::plan::*;
use crate::error::{Error, Result};
use crate::keys;
use crate::pbsql::ast::{AggFn, Expr, JoinKind, Op, Ty};
use crate::rel::{self, Enc, Schema};
use crate::storage::Ctx;
use crate::value::Value;
use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::ops::Bound as B;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Instant;

pub type Row = Vec<Value>;
type RowIter<'e> = Box<dyn Iterator<Item = Result<Row>> + 'e>;
type KeyedIter<'e> = Box<dyn Iterator<Item = Result<(Vec<u8>, Row)>> + 'e>;

/// Execution settings carried by a statement's context.
#[derive(Debug, Clone)]
pub struct ExecOpts {
    /// Memory budget for each blocking operator, in (estimated) bytes.
    pub mem_limit: usize,
    /// Set to stop the running statement with `Error::Cancelled`.
    pub cancel: Option<Arc<AtomicBool>>,
}

pub const DEFAULT_MEM_LIMIT: usize = 64 << 20;

impl Default for ExecOpts {
    fn default() -> Self {
        ExecOpts {
            mem_limit: DEFAULT_MEM_LIMIT,
            cancel: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Stat {
    pub loops: u64,
    pub rows: u64,
    pub examined: u64,
    pub nanos: u128,
    /// Set when an index path could not be used at run time (e.g. a value of the wrong type).
    pub fallback: bool,
    /// Sorted runs / partitions written to disk by a blocking operator.
    pub spilled: u64,
}

// ------------------------------------------------------------ memory, spill

/// Rough heap size of a value, for memory accounting.
pub fn value_bytes(v: &Value) -> usize {
    24 + match v {
        Value::Text(s) => s.len(),
        Value::Array(a) => a.iter().map(value_bytes).sum(),
        Value::Object(o) => o.iter().map(|(k, v)| k.len() + 24 + value_bytes(v)).sum(),
        _ => 0,
    }
}

pub fn row_bytes(r: &[Value]) -> usize {
    24 + r.iter().map(value_bytes).sum::<usize>()
}

static SPILL_SEQ: AtomicUsize = AtomicUsize::new(0);

/// A temporary file of JSON lines, removed when dropped.
struct SpillFile {
    path: std::path::PathBuf,
    writer: Option<BufWriter<std::fs::File>>,
    rows: usize,
}

impl SpillFile {
    fn create() -> Result<SpillFile> {
        let n = SPILL_SEQ.fetch_add(1, AtomicOrdering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("polydb-spill-{}-{n}.jsonl", std::process::id()));
        let f = std::fs::File::create(&path)?;
        Ok(SpillFile {
            path,
            writer: Some(BufWriter::new(f)),
            rows: 0,
        })
    }

    fn push(&mut self, v: &Value) -> Result<()> {
        let w = self
            .writer
            .as_mut()
            .ok_or_else(|| Error::Corrupt("spill file already sealed".into()))?;
        w.write_all(v.to_json().as_bytes())?;
        w.write_all(b"\n")?;
        self.rows += 1;
        Ok(())
    }

    fn seal(&mut self) -> Result<()> {
        if let Some(mut w) = self.writer.take() {
            w.flush()?;
        }
        Ok(())
    }

    /// Read the records back (the file must be sealed).
    fn reader(&self) -> Result<impl Iterator<Item = Result<Value>>> {
        let f = std::fs::File::open(&self.path)?;
        Ok(BufReader::new(f).lines().map(|l| {
            let l = l?;
            Value::parse(&l).map_err(|e| Error::Corrupt(format!("spill file: {e}")))
        }))
    }
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        self.writer.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn row_of(v: Value) -> Result<Row> {
    match v {
        Value::Array(a) => Ok(a),
        _ => Err(Error::Corrupt("spill file holds a non-row record".into())),
    }
}

/// Rows kept in memory up to a budget, then on disk; can be read many times.
struct RowBuffer {
    mem: Vec<Row>,
    bytes: usize,
    limit: usize,
    file: Option<SpillFile>,
    len: usize,
}

impl RowBuffer {
    fn new(limit: usize) -> RowBuffer {
        RowBuffer {
            mem: Vec::new(),
            bytes: 0,
            limit,
            file: None,
            len: 0,
        }
    }

    fn push(&mut self, r: Row) -> Result<()> {
        self.len += 1;
        if let Some(f) = &mut self.file {
            return f.push(&Value::Array(r));
        }
        self.bytes += row_bytes(&r);
        self.mem.push(r);
        if self.bytes > self.limit {
            let mut f = SpillFile::create()?;
            for r in self.mem.drain(..) {
                f.push(&Value::Array(r))?;
            }
            self.bytes = 0;
            self.file = Some(f);
        }
        Ok(())
    }

    fn seal(&mut self) -> Result<()> {
        if let Some(f) = &mut self.file {
            f.seal()?;
        }
        Ok(())
    }

    fn spilled(&self) -> bool {
        self.file.is_some()
    }

    /// Read all rows; the iterator keeps the buffer alive on its own.
    fn iter(self: &Rc<Self>) -> Result<Box<dyn Iterator<Item = Result<Row>>>> {
        Ok(match &self.file {
            None => {
                let me = self.clone();
                let mut i = 0;
                Box::new(std::iter::from_fn(move || {
                    let r = me.mem.get(i).cloned();
                    i += 1;
                    r.map(Ok)
                }))
            }
            Some(f) => Box::new(f.reader()?.map(|v| v.and_then(row_of))),
        })
    }
}

type Cmp<'f> = Rc<dyn Fn(&[Value], &[Value]) -> Ordering + 'f>;

/// External merge sort of (keys, row) records.
struct Sorter<'f> {
    cmp: Cmp<'f>,
    buf: Vec<(Vec<Value>, Row)>,
    bytes: usize,
    limit: usize,
    runs: Vec<SpillFile>,
}

impl<'f> Sorter<'f> {
    fn new(cmp: Cmp<'f>, limit: usize) -> Self {
        Sorter {
            cmp,
            buf: Vec::new(),
            bytes: 0,
            limit,
            runs: Vec::new(),
        }
    }

    fn push(&mut self, keys: Vec<Value>, row: Row) -> Result<()> {
        self.bytes += row_bytes(&keys) + row_bytes(&row);
        self.buf.push((keys, row));
        if self.bytes > self.limit {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let cmp = self.cmp.clone();
        self.buf.sort_by(|a, b| cmp(&a.0, &b.0)); // stable: equal keys keep input order
        let mut f = SpillFile::create()?;
        for (k, r) in self.buf.drain(..) {
            f.push(&Value::Array(vec![Value::Array(k), Value::Array(r)]))?;
        }
        f.seal()?;
        self.runs.push(f);
        self.bytes = 0;
        Ok(())
    }

    /// Sorted records; merges the on-disk runs if any.
    fn finish(
        mut self,
    ) -> Result<(
        Box<dyn Iterator<Item = Result<(Vec<Value>, Row)>> + 'f>,
        u64,
    )> {
        let cmp = self.cmp.clone();
        if self.runs.is_empty() {
            self.buf.sort_by(|a, b| cmp(&a.0, &b.0));
            return Ok((Box::new(self.buf.into_iter().map(Ok)), 0));
        }
        self.flush()?;
        let n = self.runs.len() as u64;
        let split = |v: Value| -> Result<(Vec<Value>, Row)> {
            match v {
                Value::Array(mut p) if p.len() == 2 => {
                    let r = row_of(p.pop().unwrap_or(Value::Null))?;
                    let k = row_of(p.pop().unwrap_or(Value::Null))?;
                    Ok((k, r))
                }
                _ => Err(Error::Corrupt("bad sort run record".into())),
            }
        };
        let mut readers = Vec::new();
        let mut heads: Vec<Option<(Vec<Value>, Row)>> = Vec::new();
        for f in &self.runs {
            let mut r: Box<dyn Iterator<Item = Result<Value>>> = Box::new(f.reader()?);
            heads.push(match r.next() {
                Some(v) => Some(split(v?)?),
                None => None,
            });
            readers.push(r);
        }
        let runs = self.runs; // keep the files alive while merging
        let mut failed = false;
        let it = std::iter::from_fn(move || {
            let _keep = &runs;
            if failed {
                return None;
            }
            // the smallest head; ties go to the earliest run, which keeps the sort stable
            let mut best: Option<usize> = None;
            for (i, h) in heads.iter().enumerate() {
                if let Some((k, _)) = h {
                    if best.map_or(true, |b| {
                        cmp(k, &heads[b].as_ref().expect("head").0) == Ordering::Less
                    }) {
                        best = Some(i);
                    }
                }
            }
            let i = best?;
            let out = heads[i].take();
            heads[i] = match readers[i].next() {
                Some(Ok(v)) => match split(v) {
                    Ok(x) => Some(x),
                    Err(e) => {
                        failed = true;
                        return Some(Err(e));
                    }
                },
                Some(Err(e)) => {
                    failed = true;
                    return Some(Err(e));
                }
                None => None,
            };
            out.map(Ok)
        });
        Ok((Box::new(it), n))
    }
}

// ------------------------------------------------------------- accumulators

#[derive(Debug, Clone)]
enum Acc {
    CountStar(i64),
    Count(i64),
    Sum(Option<Value>),
    Avg(Option<Value>, i64),
    Min(Option<Value>),
    Max(Option<Value>),
}

impl Acc {
    fn new(a: &AggSpec) -> Acc {
        match (a.func, &a.arg) {
            (AggFn::Count, None) => Acc::CountStar(0),
            (AggFn::Count, _) => Acc::Count(0),
            (AggFn::Sum, _) => Acc::Sum(None),
            (AggFn::Avg, _) => Acc::Avg(None, 0),
            (AggFn::Min, _) => Acc::Min(None),
            (AggFn::Max, _) => Acc::Max(None),
        }
    }

    fn add(&mut self, v: &Value) -> Result<()> {
        if v.is_null() && !matches!(self, Acc::CountStar(_)) {
            return Ok(());
        }
        match self {
            Acc::CountStar(n) | Acc::Count(n) => *n += 1,
            Acc::Sum(s) => *s = Some(add(s.as_ref(), v)?),
            Acc::Avg(s, n) => {
                *s = Some(add(s.as_ref(), v)?);
                *n += 1;
            }
            Acc::Min(m) => {
                if m.as_ref()
                    .map_or(true, |c| v.total_cmp(c) == Ordering::Less)
                {
                    *m = Some(v.clone());
                }
            }
            Acc::Max(m) => {
                if m.as_ref()
                    .map_or(true, |c| v.total_cmp(c) == Ordering::Greater)
                {
                    *m = Some(v.clone());
                }
            }
        }
        Ok(())
    }

    /// Combine two partial states of the same group.
    fn merge(&mut self, o: Acc) -> Result<()> {
        match (self, o) {
            (Acc::CountStar(a), Acc::CountStar(b)) | (Acc::Count(a), Acc::Count(b)) => *a += b,
            (Acc::Sum(a), Acc::Sum(b)) => {
                if let Some(b) = b {
                    *a = Some(add(a.as_ref(), &b)?);
                }
            }
            (Acc::Avg(a, n), Acc::Avg(b, m)) => {
                if let Some(b) = b {
                    *a = Some(add(a.as_ref(), &b)?);
                }
                *n += m;
            }
            (Acc::Min(a), Acc::Min(Some(b))) => {
                if a.as_ref()
                    .map_or(true, |c| b.total_cmp(c) == Ordering::Less)
                {
                    *a = Some(b);
                }
            }
            (Acc::Max(a), Acc::Max(Some(b))) => {
                if a.as_ref()
                    .map_or(true, |c| b.total_cmp(c) == Ordering::Greater)
                {
                    *a = Some(b);
                }
            }
            (Acc::Min(_), Acc::Min(None)) | (Acc::Max(_), Acc::Max(None)) => {}
            _ => return Err(Error::Corrupt("mismatched aggregate states".into())),
        }
        Ok(())
    }

    fn finish(self) -> Value {
        match self {
            Acc::CountStar(n) | Acc::Count(n) => Value::Int(n),
            Acc::Sum(s) => s.unwrap_or(Value::Null),
            Acc::Avg(None, _) | Acc::Avg(_, 0) => Value::Null,
            Acc::Avg(Some(s), n) => Value::Float(s.as_f64().unwrap_or(0.0) / n as f64),
            Acc::Min(m) | Acc::Max(m) => m.unwrap_or(Value::Null),
        }
    }

    fn to_value(&self) -> Value {
        let o = |x: &Option<Value>| x.clone().unwrap_or(Value::Null);
        match self {
            Acc::CountStar(n) => Value::Array(vec!["c*".into(), Value::Int(*n)]),
            Acc::Count(n) => Value::Array(vec!["c".into(), Value::Int(*n)]),
            Acc::Sum(s) => Value::Array(vec!["s".into(), o(s), Value::Bool(s.is_some())]),
            Acc::Avg(s, n) => Value::Array(vec!["a".into(), o(s), Value::Int(*n)]),
            Acc::Min(m) => Value::Array(vec!["min".into(), o(m), Value::Bool(m.is_some())]),
            Acc::Max(m) => Value::Array(vec!["max".into(), o(m), Value::Bool(m.is_some())]),
        }
    }

    fn from_value(v: &Value) -> Result<Acc> {
        let bad = || Error::Corrupt("bad aggregate state in spill file".into());
        let Value::Array(a) = v else {
            return Err(bad());
        };
        let some = |i: usize| -> Option<Value> {
            (a.get(i + 1) == Some(&Value::Bool(true))).then(|| a[i].clone())
        };
        Ok(match a.first().and_then(Value::as_str).ok_or_else(bad)? {
            "c*" => Acc::CountStar(a.get(1).and_then(Value::as_i64).ok_or_else(bad)?),
            "c" => Acc::Count(a.get(1).and_then(Value::as_i64).ok_or_else(bad)?),
            "s" => Acc::Sum(some(1)),
            "a" => {
                let n = a.get(2).and_then(Value::as_i64).ok_or_else(bad)?;
                Acc::Avg((n > 0).then(|| a[1].clone()), n)
            }
            "min" => Acc::Min(some(1)),
            "max" => Acc::Max(some(1)),
            _ => return Err(bad()),
        })
    }
}

fn add(a: Option<&Value>, b: &Value) -> Result<Value> {
    match a {
        None => match b {
            Value::Int(_) | Value::Float(_) => Ok(b.clone()),
            other => Err(Error::Type(format!("cannot sum {}", other.to_json()))),
        },
        Some(a) => super::eval::arith(Op::Add, a, b),
    }
}

/// One group being aggregated.
struct Group {
    first: Row,
    accs: Vec<Acc>,
    /// Values already counted by DISTINCT aggregates (by aggregate position).
    seen: Vec<HashSet<String>>,
}

/// Hashable form of a list of values; numerically equal numbers hash alike.
/// `None` if any value is NULL and NULLs must not match (joins).
fn hash_key(vals: &[Value], nulls_equal: bool) -> Option<String> {
    let mut parts = Vec::with_capacity(vals.len());
    for v in vals {
        if v.is_null() && !nulls_equal {
            return None;
        }
        parts.push(match v {
            Value::Float(f) if f.fract() == 0.0 && f.abs() < 9e15 => Value::Int(*f as i64),
            other => other.clone(),
        });
    }
    Some(Value::Array(parts).to_json())
}

fn combine(l: &[Value], r: &[Value]) -> Row {
    let mut c = Vec::with_capacity(l.len() + r.len());
    c.extend_from_slice(l);
    c.extend_from_slice(r);
    c
}

fn nulls(n: usize) -> Row {
    vec![Value::Null; n]
}

/// How a table access reaches its rows.
enum TableKeys {
    Keys(Vec<Vec<u8>>),
    RowRange(B<Vec<u8>>, B<Vec<u8>>),
    IndexRange(B<Vec<u8>>, B<Vec<u8>>, usize),
    Fallback,
}

// ================================================================= executor

pub struct Executor<'p, 'c, 'a> {
    pub ctx: &'c Ctx<'a>,
    pub subs: &'p [SubPlan],
    pub stats: RefCell<HashMap<(usize, usize), Stat>>,
    cache: RefCell<HashMap<(usize, Option<usize>), Rc<Vec<Row>>>>,
    /// Which tree a node id belongs to: 0 = main plan, 1 + i = subplan i.
    tree: Cell<usize>,
    pub opts: ExecOpts,
}

impl SubRunner for Executor<'_, '_, '_> {
    fn run_sub(
        &self,
        id: usize,
        outer: &[Vec<Value>],
        limit: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        let sp = self
            .subs
            .get(id)
            .ok_or_else(|| Error::Parse("internal error: missing subplan".into()))?;
        if !sp.correlated {
            if let Some(rows) = self.cache.borrow().get(&(id, limit)) {
                return Ok((**rows).clone());
            }
        }
        let prev = self.tree.replace(id + 1);
        let outer: Rc<Vec<Row>> = Rc::new(if sp.correlated {
            outer.to_vec()
        } else {
            vec![]
        });
        let res = self.open(&sp.root, outer).and_then(|it| {
            it.take(limit.unwrap_or(usize::MAX))
                .collect::<Result<Vec<_>>>()
        });
        self.tree.set(prev);
        let rows = res?;
        if !sp.correlated {
            self.cache
                .borrow_mut()
                .insert((id, limit), Rc::new(rows.clone()));
        }
        Ok(rows)
    }
}

/// Counts rows and time of one operator and checks for cancellation.
struct Counted<'e> {
    inner: RowIter<'e>,
    stats: &'e RefCell<HashMap<(usize, usize), Stat>>,
    key: (usize, usize),
    cancel: Option<Arc<AtomicBool>>,
    done: bool,
}

impl Iterator for Counted<'_> {
    type Item = Result<Row>;
    fn next(&mut self) -> Option<Result<Row>> {
        if self.done {
            return None;
        }
        if self
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(AtomicOrdering::Relaxed))
        {
            self.done = true;
            return Some(Err(Error::Cancelled("statement cancelled".into())));
        }
        let t = Instant::now();
        let r = self.inner.next();
        let mut st = self.stats.borrow_mut();
        let s = st.entry(self.key).or_default();
        s.nanos += t.elapsed().as_nanos();
        match &r {
            Some(Ok(_)) => s.rows += 1,
            Some(Err(_)) | None => self.done = true,
        }
        r
    }
}

impl<'p, 'c, 'a> Executor<'p, 'c, 'a> {
    pub fn new(ctx: &'c Ctx<'a>, subs: &'p [SubPlan]) -> Self {
        Executor {
            ctx,
            subs,
            stats: RefCell::new(HashMap::new()),
            cache: RefCell::new(HashMap::new()),
            tree: Cell::new(0),
            opts: ctx.exec.clone(),
        }
    }

    pub fn stat(&self, tree: usize, id: usize) -> Stat {
        self.stats
            .borrow()
            .get(&(tree, id))
            .cloned()
            .unwrap_or_default()
    }

    fn bump(&self, key: (usize, usize), f: impl FnOnce(&mut Stat)) {
        f(self.stats.borrow_mut().entry(key).or_default());
    }

    fn check_cancel(&self) -> Result<()> {
        if self
            .opts
            .cancel
            .as_ref()
            .is_some_and(|c| c.load(AtomicOrdering::Relaxed))
        {
            return Err(Error::Cancelled("statement cancelled".into()));
        }
        Ok(())
    }

    /// Run a plan tree to completion.
    pub fn exec(&self, node: &Node, outer: &[Row]) -> Result<Vec<Row>> {
        self.open(node, Rc::new(outer.to_vec()))?.collect()
    }

    /// Open an operator: returns a stream of its rows.
    pub fn open<'e>(&'e self, node: &'e Node, outer: Rc<Vec<Row>>) -> Result<RowIter<'e>> {
        let key = (self.tree.get(), node.id);
        self.bump(key, |s| s.loops += 1);
        let t = Instant::now();
        let inner = self.open_op(node, outer, key);
        self.bump(key, |s| s.nanos += t.elapsed().as_nanos());
        Ok(Box::new(Counted {
            inner: inner?,
            stats: &self.stats,
            key,
            cancel: self.opts.cancel.clone(),
            done: false,
        }))
    }

    fn open_op<'e>(
        &'e self,
        node: &'e Node,
        outer: Rc<Vec<Row>>,
        key: (usize, usize),
    ) -> Result<RowIter<'e>> {
        Ok(match &node.op {
            PhysOp::Unit => Box::new(std::iter::once(Ok(vec![]))),
            PhysOp::Scan {
                access,
                filter,
                limit,
                reverse,
                ..
            } => {
                let it = self.scan_stream(access, filter.as_ref(), *limit, *reverse, outer, key)?;
                Box::new(it.map(|r| r.map(|(_, row)| row)))
            }
            PhysOp::Derived { input, .. } => self.open(input, outer)?,
            PhysOp::Filter { input, pred } => {
                let input = self.open(input, outer.clone())?;
                Box::new(input.filter_map(move |r| {
                    self.bump(key, |s| s.examined += 1);
                    match r.and_then(|row| {
                        eval(pred, &Env::new(&row, &outer, self))
                            .and_then(|v| is_true(&v))
                            .map(|k| (k, row))
                    }) {
                        Ok((true, row)) => Some(Ok(row)),
                        Ok((false, _)) => None,
                        Err(e) => Some(Err(e)),
                    }
                }))
            }
            PhysOp::Project { input, exprs } => {
                let input = self.open(input, outer.clone())?;
                Box::new(input.map(move |r| {
                    let row = r?;
                    exprs
                        .iter()
                        .map(|e| eval(e, &Env::new(&row, &outer, self)))
                        .collect()
                }))
            }
            PhysOp::Limit {
                input,
                limit,
                offset,
            } => {
                let input = self.open(input, outer)?;
                Box::new(input.skip(*offset).take(limit.unwrap_or(usize::MAX)))
            }
            PhysOp::Union { left, right, all } => {
                let l = self.open(left, outer.clone())?;
                let rnode: &'e Node = right;
                let mut right_it: Option<RowIter<'e>> = None;
                let mut left_it = Some(l);
                let chained = std::iter::from_fn(move || loop {
                    if let Some(l) = &mut left_it {
                        match l.next() {
                            Some(r) => return Some(r),
                            None => left_it = None,
                        }
                    }
                    if right_it.is_none() {
                        match self.open(rnode, outer.clone()) {
                            Ok(it) => right_it = Some(it),
                            Err(e) => return Some(Err(e)),
                        }
                    }
                    return right_it.as_mut()?.next();
                });
                if *all {
                    Box::new(chained)
                } else {
                    self.dedupe(Box::new(chained), "UNION")
                }
            }
            PhysOp::Distinct { input } => {
                let input = self.open(input, outer)?;
                self.dedupe(input, "DISTINCT")
            }
            PhysOp::Sort { input, keys } => self.sort(input, keys, outer, key)?,
            PhysOp::HashAggregate {
                input,
                group,
                aggs,
                width,
            } => self.aggregate(input, group, aggs, *width, outer, key)?,
            PhysOp::NestedLoopJoin {
                left,
                right,
                on,
                kind,
                right_width,
            } => self.nested_loop(left, right, on.as_ref(), *kind, *right_width, outer, key)?,
            PhysOp::HashJoin {
                left,
                right,
                lkeys,
                rkeys,
                on,
                kind,
                right_width,
            } => self.hash_join(
                left,
                right,
                lkeys,
                rkeys,
                on.as_ref(),
                *kind,
                *right_width,
                outer,
                key,
            )?,
            PhysOp::LookupJoin {
                left,
                right,
                on,
                kind,
                right_width,
            } => {
                let l = self.open(left, outer.clone())?;
                let left_outer = matches!(kind, JoinKind::Left);
                let rw = *right_width;
                let mut left_it = l;
                let mut pending: std::collections::VecDeque<Row> = Default::default();
                Box::new(std::iter::from_fn(move || loop {
                    if let Some(r) = pending.pop_front() {
                        return Some(Ok(r));
                    }
                    let lr = match left_it.next()? {
                        Ok(r) => r,
                        Err(e) => return Some(Err(e)),
                    };
                    let mut stack = Vec::with_capacity(outer.len() + 1);
                    stack.push(lr.clone());
                    stack.extend(outer.iter().cloned());
                    let rights = match self.open(right, Rc::new(stack)) {
                        Ok(it) => it,
                        Err(e) => return Some(Err(e)),
                    };
                    let mut matched = false;
                    for rr in rights {
                        let rr = match rr {
                            Ok(r) => r,
                            Err(e) => return Some(Err(e)),
                        };
                        self.bump(key, |s| s.examined += 1);
                        let c = combine(&lr, &rr);
                        match on.as_ref().map_or(Ok(true), |p| {
                            eval(p, &Env::new(&c, &outer, self)).and_then(|v| is_true(&v))
                        }) {
                            Ok(true) => {
                                matched = true;
                                pending.push_back(c);
                            }
                            Ok(false) => {}
                            Err(e) => return Some(Err(e)),
                        }
                    }
                    if !matched && left_outer {
                        pending.push_back(combine(&lr, &nulls(rw)));
                    }
                }))
            }
        })
    }

    /// Stream unique rows; the set of rows seen so far is bounded by the memory limit.
    fn dedupe<'e>(&'e self, input: RowIter<'e>, what: &'static str) -> RowIter<'e> {
        let mut seen = HashSet::new();
        let mut bytes = 0usize;
        let limit = self.opts.mem_limit;
        let mut failed = false;
        Box::new(input.filter_map(move |r| {
            if failed {
                return None;
            }
            match r {
                Err(e) => Some(Err(e)),
                Ok(row) => {
                    let k = hash_key(&row, true).unwrap_or_default();
                    if seen.contains(&k) {
                        return None;
                    }
                    bytes += k.len() + 48;
                    if bytes > limit {
                        failed = true;
                        return Some(Err(Error::Cancelled(format!(
                            "{what} needs more than the {limit}-byte memory limit to remove duplicates"
                        ))));
                    }
                    seen.insert(k);
                    Some(Ok(row))
                }
            }
        }))
    }

    fn sort<'e>(
        &'e self,
        input: &'e Node,
        keys: &'e [(Expr, bool)],
        outer: Rc<Vec<Row>>,
        key: (usize, usize),
    ) -> Result<RowIter<'e>> {
        let cmp: Cmp<'e> = Rc::new(move |a: &[Value], b: &[Value]| {
            for (i, (_, desc)) in keys.iter().enumerate() {
                let o = a[i].total_cmp(&b[i]);
                if o != Ordering::Equal {
                    return if *desc { o.reverse() } else { o };
                }
            }
            Ordering::Equal
        });
        let mut sorter = Sorter::new(cmp, self.opts.mem_limit);
        for r in self.open(input, outer.clone())? {
            let r = r?;
            let k = keys
                .iter()
                .map(|(e, _)| eval(e, &Env::new(&r, &outer, self)))
                .collect::<Result<Vec<_>>>()?;
            sorter.push(k, r)?;
        }
        let (it, runs) = sorter.finish()?;
        self.bump(key, |s| s.spilled += runs);
        Ok(Box::new(it.map(|r| r.map(|(_, row)| row))))
    }

    #[allow(clippy::too_many_arguments)]
    fn aggregate<'e>(
        &'e self,
        input: &'e Node,
        group: &'e [Expr],
        aggs: &'e [AggSpec],
        width: usize,
        outer: Rc<Vec<Row>>,
        key: (usize, usize),
    ) -> Result<RowIter<'e>> {
        let limit = self.opts.mem_limit;
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut groups: Vec<Group> = Vec::new();
        let mut bytes = 0usize;
        let by_key: Cmp<'e> = Rc::new(|a: &[Value], b: &[Value]| a[0].total_cmp(&b[0]));
        let mut sorter: Option<Sorter<'e>> = None;
        let has_distinct = aggs.iter().any(|a| a.distinct);
        // spill: write each group's partial state as a sorted record, start afresh
        let spill = |index: &mut HashMap<String, usize>,
                     groups: &mut Vec<Group>,
                     sorter: &mut Option<Sorter<'e>>|
         -> Result<()> {
            let s = sorter.get_or_insert_with(|| Sorter::new(by_key.clone(), 0));
            let mut keyed: Vec<(String, usize)> = index.drain().collect();
            keyed.sort();
            for (k, gi) in keyed {
                let g = &groups[gi];
                let state = Value::Array(vec![
                    Value::Array(g.first.clone()),
                    Value::Array(g.accs.iter().map(Acc::to_value).collect()),
                ]);
                s.buf.push((vec![Value::Text(k)], vec![state]));
            }
            groups.clear();
            s.flush()
        };
        for r in self.open(input, outer.clone())? {
            let r = r?;
            self.bump(key, |s| s.examined += 1);
            let env = Env::new(&r, &outer, self);
            let gk = group
                .iter()
                .map(|e| eval(e, &env))
                .collect::<Result<Vec<_>>>()?;
            let h = hash_key(&gk, true).unwrap_or_default();
            let gi = match index.get(&h) {
                Some(i) => *i,
                None => {
                    bytes += h.len() + row_bytes(&r) + 64 * aggs.len().max(1);
                    groups.push(Group {
                        first: r.clone(),
                        accs: aggs.iter().map(Acc::new).collect(),
                        seen: vec![HashSet::new(); aggs.len()],
                    });
                    index.insert(h, groups.len() - 1);
                    groups.len() - 1
                }
            };
            for (ai, a) in aggs.iter().enumerate() {
                let v = match &a.arg {
                    Some(e) => eval(e, &env)?,
                    None => Value::Bool(true),
                };
                if a.distinct && !v.is_null() {
                    let k = hash_key(std::slice::from_ref(&v), true).unwrap_or_default();
                    if !groups[gi].seen[ai].insert(k.clone()) {
                        continue;
                    }
                    bytes += k.len() + 48;
                }
                groups[gi].accs[ai].add(&v)?;
            }
            if bytes > limit {
                if has_distinct {
                    return Err(Error::Cancelled(format!(
                        "aggregate with DISTINCT needs more than the {limit}-byte memory limit"
                    )));
                }
                spill(&mut index, &mut groups, &mut sorter)?;
                bytes = 0;
            }
        }
        let finish_group = move |first: Row, accs: Vec<Acc>| -> Row {
            let mut row = first;
            row.resize(width, Value::Null);
            row.extend(accs.into_iter().map(Acc::finish));
            row
        };
        if sorter.is_none() {
            if groups.is_empty() && group.is_empty() {
                return Ok(Box::new(std::iter::once(Ok(finish_group(
                    nulls(width),
                    aggs.iter().map(Acc::new).collect(),
                )))));
            }
            return Ok(Box::new(
                groups
                    .into_iter()
                    .map(move |g| Ok(finish_group(g.first, g.accs))),
            ));
        }
        spill(&mut index, &mut groups, &mut sorter)?;
        let (merged, runs) = sorter.expect("spilled").finish()?;
        self.bump(key, |s| s.spilled += runs);
        // merge partial states of equal group keys (adjacent after the merge)
        let mut merged = merged.peekable();
        let decode = |payload: Row| -> Result<(Row, Vec<Acc>)> {
            match payload.into_iter().next() {
                Some(Value::Array(mut p)) if p.len() == 2 => {
                    let accs = match p.pop() {
                        Some(Value::Array(a)) => {
                            a.iter().map(Acc::from_value).collect::<Result<Vec<_>>>()?
                        }
                        _ => return Err(Error::Corrupt("bad group state".into())),
                    };
                    Ok((row_of(p.pop().unwrap_or(Value::Null))?, accs))
                }
                _ => Err(Error::Corrupt("bad group state".into())),
            }
        };
        Ok(Box::new(std::iter::from_fn(move || {
            let (k, payload) = match merged.next()? {
                Ok(x) => x,
                Err(e) => return Some(Err(e)),
            };
            let (first, mut accs) = match decode(payload) {
                Ok(x) => x,
                Err(e) => return Some(Err(e)),
            };
            while let Some(Ok((k2, _))) = merged.peek() {
                if k2 != &k {
                    break;
                }
                let Some(Ok((_, p2))) = merged.next() else {
                    break;
                };
                match decode(p2) {
                    Ok((_, more)) => {
                        for (a, b) in accs.iter_mut().zip(more) {
                            if let Err(e) = a.merge(b) {
                                return Some(Err(e));
                            }
                        }
                    }
                    Err(e) => return Some(Err(e)),
                }
            }
            Some(Ok(finish_group(first, accs)))
        })))
    }

    #[allow(clippy::too_many_arguments)]
    fn nested_loop<'e>(
        &'e self,
        left: &'e Node,
        right: &'e Node,
        on: Option<&'e Expr>,
        kind: JoinKind,
        rw: usize,
        outer: Rc<Vec<Row>>,
        key: (usize, usize),
    ) -> Result<RowIter<'e>> {
        let mut buf = RowBuffer::new(self.opts.mem_limit);
        for r in self.open(right, outer.clone())? {
            buf.push(r?)?;
        }
        buf.seal()?;
        if buf.spilled() {
            self.bump(key, |s| s.spilled += 1);
        }
        let buf = Rc::new(buf);
        let left_outer = matches!(kind, JoinKind::Left | JoinKind::Full);
        let right_outer = matches!(kind, JoinKind::Right | JoinKind::Full);
        let mut matched_right = vec![false; if right_outer { buf.len } else { 0 }];
        let mut left_it = Some(self.open(left, outer.clone())?);
        let mut lw: Option<usize> = None;
        let mut pending: std::collections::VecDeque<Row> = Default::default();
        let mut tail: Option<Box<dyn Iterator<Item = Result<Row>>>> = None;
        Ok(Box::new(std::iter::from_fn(move || loop {
            if let Some(r) = pending.pop_front() {
                return Some(Ok(r));
            }
            if let Some(t) = &mut tail {
                return t.next();
            }
            let Some(lit) = &mut left_it else { return None };
            match lit.next() {
                Some(Ok(lr)) => {
                    lw.get_or_insert(lr.len());
                    let mut matched = false;
                    let rows = match buf.iter() {
                        Ok(it) => it,
                        Err(e) => return Some(Err(e)),
                    };
                    for (i, rr) in rows.enumerate() {
                        let rr = match rr {
                            Ok(r) => r,
                            Err(e) => return Some(Err(e)),
                        };
                        self.bump(key, |s| s.examined += 1);
                        let c = combine(&lr, &rr);
                        match on.map_or(Ok(true), |p| {
                            eval(p, &Env::new(&c, &outer, self)).and_then(|v| is_true(&v))
                        }) {
                            Ok(true) => {
                                matched = true;
                                if right_outer {
                                    matched_right[i] = true;
                                }
                                pending.push_back(c);
                            }
                            Ok(false) => {}
                            Err(e) => return Some(Err(e)),
                        }
                    }
                    if !matched && left_outer {
                        pending.push_back(combine(&lr, &nulls(rw)));
                    }
                }
                Some(Err(e)) => return Some(Err(e)),
                None => {
                    left_it = None;
                    if !right_outer {
                        return None;
                    }
                    let width = lw.unwrap_or_else(|| left_width_hint(left));
                    let flags = std::mem::take(&mut matched_right);
                    let rows = match buf.iter() {
                        Ok(it) => it,
                        Err(e) => return Some(Err(e)),
                    };
                    tail = Some(Box::new(rows.zip(flags).filter_map(
                        move |(r, m)| match r {
                            Ok(_) if m => None,
                            Ok(r) => Some(Ok(combine(&nulls(width), &r))),
                            Err(e) => Some(Err(e)),
                        },
                    )));
                }
            }
        })))
    }

    #[allow(clippy::too_many_arguments)]
    fn hash_join<'e>(
        &'e self,
        left: &'e Node,
        right: &'e Node,
        lkeys: &'e [Expr],
        rkeys: &'e [Expr],
        on: Option<&'e Expr>,
        kind: JoinKind,
        rw: usize,
        outer: Rc<Vec<Row>>,
        key: (usize, usize),
    ) -> Result<RowIter<'e>> {
        let limit = self.opts.mem_limit;
        let keys_of = move |row: &[Value], ks: &[Expr], outer: &[Row]| -> Result<Option<String>> {
            let k = ks
                .iter()
                .map(|e| eval(e, &Env::new(row, outer, self)))
                .collect::<Result<Vec<_>>>()?;
            Ok(hash_key(&k, false))
        };
        // build side, with a fallback to disk partitions when it outgrows the budget
        const PARTS: usize = 16;
        let part_of = |h: &Option<String>| -> usize {
            match h {
                None => 0,
                Some(s) => {
                    (s.bytes()
                        .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64))
                        % PARTS as u64) as usize
                }
            }
        };
        let mut build: Vec<Row> = Vec::new();
        let mut bytes = 0;
        let mut rparts: Option<Vec<SpillFile>> = None;
        for r in self.open(right, outer.clone())? {
            let r = r?;
            if let Some(parts) = &mut rparts {
                let h = keys_of(&r, rkeys, &outer)?;
                parts[part_of(&h)].push(&Value::Array(r))?;
                continue;
            }
            bytes += row_bytes(&r);
            build.push(r);
            if bytes > limit {
                let mut parts = (0..PARTS)
                    .map(|_| SpillFile::create())
                    .collect::<Result<Vec<_>>>()?;
                for r in build.drain(..) {
                    let h = keys_of(&r, rkeys, &outer)?;
                    parts[part_of(&h)].push(&Value::Array(r))?;
                }
                rparts = Some(parts);
            }
        }
        let left_outer = matches!(kind, JoinKind::Left | JoinKind::Full);
        let right_outer = matches!(kind, JoinKind::Right | JoinKind::Full);
        let probe_outer = outer.clone();
        let probe = move |table: &HashMap<String, Vec<usize>>,
                          build: &[Row],
                          matched: &mut [bool],
                          lr: &Row,
                          out: &mut std::collections::VecDeque<Row>|
              -> Result<()> {
            let mut hit = false;
            if let Some(idx) = keys_of(lr, lkeys, &probe_outer)?.and_then(|h| table.get(&h)) {
                for &i in idx {
                    self.bump(key, |s| s.examined += 1);
                    let c = combine(lr, &build[i]);
                    if on.map_or(Ok(true), |p| {
                        eval(p, &Env::new(&c, &probe_outer, self)).and_then(|v| is_true(&v))
                    })? {
                        hit = true;
                        if right_outer {
                            matched[i] = true;
                        }
                        out.push_back(c);
                    }
                }
            }
            if !hit && left_outer {
                out.push_back(combine(lr, &nulls(rw)));
            }
            Ok(())
        };
        let index_rows =
            move |rows: &[Row], outer: &[Row]| -> Result<HashMap<String, Vec<usize>>> {
                let mut t: HashMap<String, Vec<usize>> = HashMap::new();
                for (i, r) in rows.iter().enumerate() {
                    if let Some(h) = keys_of(r, rkeys, outer)? {
                        t.entry(h).or_default().push(i);
                    }
                }
                Ok(t)
            };
        let unmatched = move |build: &[Row],
                              matched: &[bool],
                              lw: usize,
                              out: &mut std::collections::VecDeque<Row>| {
            for (r, m) in build.iter().zip(matched) {
                if !m {
                    out.push_back(combine(&nulls(lw), r));
                }
            }
        };
        let lw_hint = left_width_hint(left);
        match rparts {
            None => {
                let table = index_rows(&build, &outer)?;
                let mut matched = vec![false; if right_outer { build.len() } else { 0 }];
                let mut left_it = Some(self.open(left, outer.clone())?);
                let mut pending = std::collections::VecDeque::new();
                Ok(Box::new(std::iter::from_fn(move || loop {
                    if let Some(r) = pending.pop_front() {
                        return Some(Ok(r));
                    }
                    let lit = left_it.as_mut()?;
                    match lit.next() {
                        Some(Ok(lr)) => {
                            if let Err(e) = probe(&table, &build, &mut matched, &lr, &mut pending) {
                                return Some(Err(e));
                            }
                        }
                        Some(Err(e)) => return Some(Err(e)),
                        None => {
                            left_it = None;
                            if right_outer {
                                unmatched(&build, &matched, lw_hint, &mut pending);
                            }
                            if pending.is_empty() {
                                return None;
                            }
                        }
                    }
                })))
            }
            Some(mut rparts) => {
                // grace join: partition the probe side the same way, then join partition by partition
                self.bump(key, |s| s.spilled += PARTS as u64);
                let mut lparts = (0..PARTS)
                    .map(|_| SpillFile::create())
                    .collect::<Result<Vec<_>>>()?;
                for r in self.open(left, outer.clone())? {
                    let r = r?;
                    let h = keys_of(&r, lkeys, &outer)?;
                    lparts[part_of(&h)].push(&Value::Array(r))?;
                }
                for p in rparts.iter_mut().chain(lparts.iter_mut()) {
                    p.seal()?;
                }
                let mut part = 0usize;
                let mut pending = std::collections::VecDeque::new();
                Ok(Box::new(std::iter::from_fn(move || loop {
                    if let Some(r) = pending.pop_front() {
                        return Some(Ok(r));
                    }
                    if part >= PARTS {
                        return None;
                    }
                    let res: Result<()> = (|| {
                        self.check_cancel()?;
                        let build: Vec<Row> = rparts[part]
                            .reader()?
                            .map(|v| v.and_then(row_of))
                            .collect::<Result<_>>()?;
                        let table = index_rows(&build, &outer)?;
                        let mut matched = vec![false; if right_outer { build.len() } else { 0 }];
                        for lr in lparts[part].reader()? {
                            let lr = row_of(lr?)?;
                            probe(&table, &build, &mut matched, &lr, &mut pending)?;
                        }
                        if right_outer {
                            unmatched(&build, &matched, lw_hint, &mut pending);
                        }
                        Ok(())
                    })();
                    part += 1;
                    if let Err(e) = res {
                        part = PARTS;
                        return Some(Err(e));
                    }
                })))
            }
        }
    }

    // ------------------------------------------------------------- scans

    /// Stream (storage key, row) pairs of an access path that pass `filter`,
    /// stopping after `limit` rows.
    #[allow(clippy::too_many_arguments)]
    fn scan_stream<'e>(
        &'e self,
        access: &'e Access,
        filter: Option<&'e Expr>,
        limit: Option<usize>,
        reverse: bool,
        outer: Rc<Vec<Row>>,
        key: (usize, usize),
    ) -> Result<KeyedIter<'e>> {
        let fallback = Cell::new(false);
        let raw = self.keyed(access, reverse, &outer, &fallback)?;
        if fallback.get() {
            self.bump(key, |s| s.fallback = true);
        }
        let filtered = raw.filter_map(move |r| {
            self.bump(key, |s| s.examined += 1);
            match r {
                Err(e) => Some(Err(e)),
                Ok((k, row)) => match filter {
                    None => Some(Ok((k, row))),
                    Some(f) => {
                        match eval(f, &Env::new(&row, &outer, self)).and_then(|v| is_true(&v)) {
                            Ok(true) => Some(Ok((k, row))),
                            Ok(false) => None,
                            Err(e) => Some(Err(e)),
                        }
                    }
                },
            }
        });
        Ok(match limit {
            Some(l) => Box::new(filtered.take(l)),
            None => Box::new(filtered),
        })
    }

    /// Run an access path for DML: `sink` receives (storage key, row) of each match.
    #[allow(clippy::too_many_arguments)]
    pub fn scan(
        &self,
        access: &Access,
        filter: Option<&Expr>,
        limit: Option<usize>,
        outer: &[Row],
        examined: &mut u64,
        fallback: &mut bool,
        sink: &mut dyn FnMut(Vec<u8>, Row) -> Result<()>,
    ) -> Result<()> {
        let fb = Cell::new(false);
        let outer = Rc::new(outer.to_vec());
        let raw = self.keyed(access, false, &outer, &fb)?;
        *fallback |= fb.get();
        let mut n = 0usize;
        for r in raw {
            self.check_cancel()?;
            if limit.is_some_and(|l| n >= l) {
                break;
            }
            let (k, row) = r?;
            *examined += 1;
            if let Some(f) = filter {
                if !is_true(&eval(f, &Env::new(&row, &outer, self))?)? {
                    continue;
                }
            }
            sink(k, row)?;
            n += 1;
        }
        Ok(())
    }

    /// The raw (key, row) stream of an access path, before filtering.
    fn keyed<'e>(
        &'e self,
        access: &'e Access,
        rev: bool,
        outer: &Rc<Vec<Row>>,
        fallback: &Cell<bool>,
    ) -> Result<KeyedIter<'e>> {
        let ctx = self.ctx;
        let env_outer = outer.clone();
        let ev = move |e: &Expr| eval(e, &Env::new(&[], &env_outer, self));
        Ok(match access {
            Access::Table { schema, path } => {
                let decode = |(k, raw): (Vec<u8>, Vec<u8>)| -> Result<(Vec<u8>, Row)> {
                    Ok((k, rel::decode_row(&raw)?))
                };
                match self.table_keys(schema, path, &ev)? {
                    TableKeys::Fallback | TableKeys::RowRange(..)
                        if matches!(path, TablePath::FullScan) =>
                    {
                        Box::new(
                            ctx.iter_prefix(&keys::row_prefix(&schema.name), rev)
                                .map(decode),
                        )
                    }
                    TableKeys::Fallback => {
                        fallback.set(true);
                        Box::new(
                            ctx.iter_prefix(&keys::row_prefix(&schema.name), rev)
                                .map(decode),
                        )
                    }
                    TableKeys::Keys(ks) => Box::new(
                        ks.into_iter()
                            .filter_map(move |k| ctx.get(&k).map(|raw| decode((k, raw)))),
                    ),
                    TableKeys::RowRange(lo, hi) => {
                        Box::new(ctx.iter_range(lo, hi, rev).map(decode))
                    }
                    TableKeys::IndexRange(lo, hi, field_len) => {
                        let TablePath::Index { ix, .. } = path else {
                            unreachable!()
                        };
                        let rp = keys::row_prefix(&schema.name);
                        Box::new(ctx.iter_range(lo, hi, rev).filter_map(move |(e, _)| {
                            let mut rk = rp.clone();
                            let mut rest = &e[field_len..];
                            for _ in 0..ix.cols.len() {
                                let n = rel::encoded_len(rest);
                                rest = &rest[n.min(rest.len())..];
                            }
                            rk.extend_from_slice(rest);
                            ctx.get(&rk).map(|raw| decode((rk, raw)))
                        }))
                    }
                }
            }
            Access::Doc { coll, path } => {
                let decode = |(k, raw): (Vec<u8>, Vec<u8>)| -> Result<(Vec<u8>, Row)> {
                    Ok((k, vec![crate::doc::decode(&raw)?]))
                };
                match path {
                    DocPath::FullScan => {
                        Box::new(ctx.iter_prefix(&keys::doc_prefix(coll), rev).map(decode))
                    }
                    DocPath::Ids(list) => {
                        let mut out = Vec::new();
                        for e in list {
                            if let Ok(enc) = keys::encode_id(&ev(e)?) {
                                let mut k = keys::doc_prefix(coll);
                                k.extend(enc);
                                out.push(k);
                            }
                        }
                        out.sort();
                        out.dedup();
                        Box::new(
                            out.into_iter()
                                .filter_map(move |k| ctx.get(&k).map(|raw| decode((k, raw)))),
                        )
                    }
                    DocPath::Index(c) => {
                        let mut eq = Vec::with_capacity(c.eq.len());
                        for e in &c.eq {
                            eq.push(ev(e)?);
                        }
                        let bound = |b: &Option<(Expr, bool)>| -> Result<Option<(Value, bool)>> {
                            Ok(match b {
                                Some((e, i)) => Some((ev(e)?, *i)),
                                None => None,
                            })
                        };
                        let choice = crate::docindex::Choice {
                            index: c.index.clone(),
                            eq,
                            lo: bound(&c.lo)?,
                            hi: bound(&c.hi)?,
                        };
                        let any_null = choice
                            .eq
                            .iter()
                            .chain(choice.lo.iter().map(|x| &x.0))
                            .chain(choice.hi.iter().map(|x| &x.0))
                            .any(Value::is_null);
                        if any_null {
                            return Ok(Box::new(std::iter::empty())); // SQL: nothing equals or orders against NULL
                        }
                        match crate::docindex::key_range(coll, &choice) {
                            None => {
                                fallback.set(true);
                                Box::new(ctx.iter_prefix(&keys::doc_prefix(coll), rev).map(decode))
                            }
                            Some((lo, hi)) => {
                                let mut seen = HashSet::new();
                                let ix = choice.index;
                                Box::new(ctx.iter_range(lo, hi, rev).filter_map(move |(e, _)| {
                                    let dk = crate::docindex::doc_key_of(coll, &ix, &e);
                                    if ix.multikey && !seen.insert(dk.clone()) {
                                        return None;
                                    }
                                    ctx.get(&dk).map(|raw| decode((dk, raw)))
                                }))
                            }
                        }
                    }
                }
            }
            Access::Kv { ns, path } => {
                let base = keys::kv_ns_prefix(ns);
                let blen = base.len();
                let to_row = move |(k, v): (Vec<u8>, Vec<u8>)| -> Option<Result<(Vec<u8>, Row)>> {
                    if crate::kv::is_expired(ctx, &k) {
                        return None;
                    }
                    let row = vec![
                        Value::Text(String::from_utf8_lossy(&k[blen..]).into_owned()),
                        Value::Text(String::from_utf8_lossy(&v).into_owned()),
                    ];
                    Some(Ok((k, row)))
                };
                let text = |v: Value| -> Option<String> {
                    match v {
                        Value::Text(s) => Some(s),
                        Value::Int(i) => Some(i.to_string()),
                        _ => None,
                    }
                };
                match path {
                    KvPath::Keys(list) => {
                        let mut ks = Vec::new();
                        for e in list {
                            if let Some(s) = text(ev(e)?) {
                                let mut k = base.clone();
                                k.extend_from_slice(s.as_bytes());
                                ks.push(k);
                            }
                        }
                        ks.sort();
                        ks.dedup();
                        Box::new(
                            ks.into_iter()
                                .filter_map(move |k| ctx.get(&k).and_then(|v| to_row((k, v)))),
                        )
                    }
                    KvPath::Prefix(p) => {
                        let mut k = base.clone();
                        k.extend_from_slice(p.as_bytes());
                        Box::new(ctx.iter_prefix(&k, rev).filter_map(to_row))
                    }
                    KvPath::Range { lo, hi } => {
                        let end = crate::storage::prefix_successor(&base)
                            .map_or(B::Unbounded, B::Excluded);
                        let bound =
                            |b: &Option<Bound>, lower: bool| -> Result<Option<B<Vec<u8>>>> {
                                Ok(match b {
                                    None => Some(if lower {
                                        B::Included(base.clone())
                                    } else {
                                        end.clone()
                                    }),
                                    Some(b) => match ev(&b.expr)? {
                                        Value::Text(s) => {
                                            let mut k = base.clone();
                                            k.extend_from_slice(s.as_bytes());
                                            Some(if b.inclusive {
                                                B::Included(k)
                                            } else {
                                                B::Excluded(k)
                                            })
                                        }
                                        _ => None,
                                    },
                                })
                            };
                        match (bound(lo, true)?, bound(hi, false)?) {
                            (Some(l), Some(h)) => {
                                Box::new(ctx.iter_range(l, h, rev).filter_map(to_row))
                            }
                            _ => {
                                fallback.set(true);
                                Box::new(ctx.iter_prefix(&base, rev).filter_map(to_row))
                            }
                        }
                    }
                    KvPath::FullScan => Box::new(ctx.iter_prefix(&base, rev).filter_map(to_row)),
                }
            }
        })
    }

    /// How a table path reaches its rows (keys or a key range).
    fn table_keys(
        &self,
        s: &Schema,
        path: &TablePath,
        ev: &dyn Fn(&Expr) -> Result<Value>,
    ) -> Result<TableKeys> {
        let pk = s.pk_positions();
        let typed = |ci: usize, v: Value| -> Option<Value> {
            let ty = s.cols[ci].ty;
            let c = coerce_type(ty, v.clone()).ok()?;
            (!c.is_null() && (c.rank() == v.rank() || matches!(ty, Ty::Json))).then_some(c)
        };
        let succ = |p: &[u8]| crate::storage::prefix_successor(p).map_or(B::Unbounded, B::Excluded);
        Ok(match path {
            TablePath::FullScan => TableKeys::Fallback,
            TablePath::PkPoints(points) => {
                let mut out = Vec::new();
                'points: for p in points {
                    let mut vals = Vec::new();
                    for (e, ci) in p.iter().zip(&pk) {
                        match typed(*ci, ev(e)?) {
                            Some(v) => vals.push(v),
                            None => continue 'points, // can never be equal
                        }
                    }
                    out.push(rel::pk_key(s, &vals.iter().collect::<Vec<_>>())?);
                }
                out.sort();
                out.dedup();
                TableKeys::Keys(out)
            }
            TablePath::PkRange { eq, lo, hi } => {
                let base = keys::row_prefix(&s.name);
                let single = pk.len() == 1;
                let mut prefix = base.clone();
                for (e, ci) in eq.iter().zip(&pk) {
                    match typed(*ci, ev(e)?).and_then(|v| rel::encode_exact(&v)) {
                        Some(enc) => prefix.extend(enc),
                        None => return Ok(TableKeys::Keys(vec![])),
                    }
                }
                let ci = pk[eq.len()];
                let enc = |v: Value| -> Option<Vec<u8>> {
                    if single {
                        keys::encode_id(&v).ok()
                    } else {
                        rel::encode_exact(&v)
                    }
                };
                let bound = |b: &Option<Bound>, lower: bool| -> Result<Option<B<Vec<u8>>>> {
                    Ok(match b {
                        None => Some(if lower {
                            B::Included(prefix.clone())
                        } else {
                            succ(&prefix)
                        }),
                        Some(b) => {
                            let v = ev(&b.expr)?;
                            if v.is_null() {
                                return Ok(Some(B::Excluded(prefix.clone()))); // nothing compares with NULL
                            }
                            match typed(ci, v).and_then(enc) {
                                None => None,
                                Some(e) => {
                                    let mut k = prefix.clone();
                                    k.extend(e);
                                    Some(if lower || single {
                                        B::Included(k)
                                    } else {
                                        succ(&k)
                                    })
                                }
                            }
                        }
                    })
                };
                match (bound(lo, true)?, bound(hi, false)?) {
                    (Some(l), Some(h)) => TableKeys::RowRange(l, h),
                    _ => TableKeys::Fallback,
                }
            }
            TablePath::Index { ix, eq, lo, hi } => {
                let field_prefix = keys::index_prefix(&rel::owner(&s.name), &ix.field);
                let mut prefix = field_prefix.clone();
                let cols: Vec<usize> = ix.cols.iter().filter_map(|c| s.find_col(c)).collect();
                for (e, ci) in eq.iter().zip(&cols) {
                    let v = ev(e)?;
                    let v = if ix.enc == Enc::Exact {
                        typed(*ci, v)
                    } else {
                        Some(v).filter(|v| !v.is_null())
                    };
                    match v.and_then(|v| rel::encode_with(ix.enc, &v)) {
                        Some(enc) => prefix.extend(enc),
                        None => return Ok(TableKeys::Keys(vec![])),
                    }
                }
                let next = cols.get(eq.len()).copied();
                let enc_bound = |v: Value| -> Option<Vec<u8>> {
                    let v = match (ix.enc, next) {
                        (Enc::Exact, Some(ci)) => typed(ci, v)?,
                        _ => v,
                    };
                    rel::encode_with(ix.enc, &v)
                };
                match index_bounds(&prefix, lo, hi, ev, &enc_bound)? {
                    Some((l, h)) => TableKeys::IndexRange(l, h, field_prefix.len()),
                    None => TableKeys::Fallback,
                }
            }
            TablePath::IndexIn { ix, vals } => {
                let field_prefix = keys::index_prefix(&rel::owner(&s.name), &ix.field);
                let ci = s.find_col(&ix.cols[0]);
                let mut out = Vec::new();
                for e in vals {
                    let v = ev(e)?;
                    let v = match (ix.enc, ci) {
                        (Enc::Exact, Some(ci)) => typed(ci, v),
                        _ => Some(v).filter(|v| !v.is_null()),
                    };
                    let Some(enc) = v.and_then(|v| rel::encode_with(ix.enc, &v)) else {
                        continue;
                    };
                    let mut p = field_prefix.clone();
                    p.extend(enc);
                    let end = succ(&p);
                    out.extend(rel::ids_from_index(
                        self.ctx,
                        s,
                        B::Included(&p),
                        as_ref(&end),
                        field_prefix.len(),
                        ix,
                    ));
                }
                out.sort();
                out.dedup();
                TableKeys::Keys(out)
            }
        })
    }
}

/// Width of a node's rows when no row has been seen (for NULL-extending).
fn left_width_hint(n: &Node) -> usize {
    match &n.op {
        PhysOp::Unit => 0,
        PhysOp::Scan { access, .. } => match access {
            Access::Table { schema, .. } => schema.cols.len(),
            Access::Doc { .. } => 1,
            Access::Kv { .. } => 2,
        },
        PhysOp::Derived { input, .. }
        | PhysOp::Filter { input, .. }
        | PhysOp::Distinct { input }
        | PhysOp::Sort { input, .. }
        | PhysOp::Limit { input, .. } => left_width_hint(input),
        PhysOp::NestedLoopJoin {
            left, right_width, ..
        }
        | PhysOp::HashJoin {
            left, right_width, ..
        }
        | PhysOp::LookupJoin {
            left, right_width, ..
        } => left_width_hint(left) + right_width,
        PhysOp::HashAggregate { width, aggs, .. } => width + aggs.len(),
        PhysOp::Project { exprs, .. } => exprs.len(),
        PhysOp::Union { left, .. } => left_width_hint(left),
    }
}

fn as_ref(b: &B<Vec<u8>>) -> B<&[u8]> {
    match b {
        B::Included(v) => B::Included(v.as_slice()),
        B::Excluded(v) => B::Excluded(v.as_slice()),
        B::Unbounded => B::Unbounded,
    }
}

/// Key bounds for a range on an index column after `prefix`. Bounds are
/// widened to inclusive and kept inside the value's type tag; the scan's
/// filter makes the result exact. `None` means "cannot use the index".
type KeyBounds = (B<Vec<u8>>, B<Vec<u8>>);
fn index_bounds(
    prefix: &[u8],
    lo: &Option<Bound>,
    hi: &Option<Bound>,
    ev: &dyn Fn(&Expr) -> Result<Value>,
    enc: &dyn Fn(Value) -> Option<Vec<u8>>,
) -> Result<Option<KeyBounds>> {
    let succ = |p: &[u8]| crate::storage::prefix_successor(p).map_or(B::Unbounded, B::Excluded);
    let mut tag: Option<u8> = None;
    let mut one = |b: &Option<Bound>| -> Result<Option<Option<Vec<u8>>>> {
        Ok(match b {
            None => Some(None),
            Some(b) => {
                let v = ev(&b.expr)?;
                if v.is_null() {
                    return Ok(None);
                }
                match enc(v) {
                    None => None,
                    Some(e) => {
                        if tag.is_some_and(|t| t != e[0]) {
                            return Ok(None);
                        }
                        tag = Some(e[0]);
                        Some(Some(e))
                    }
                }
            }
        })
    };
    let (Some(l), Some(h)) = (one(lo)?, one(hi)?) else {
        return Ok(None);
    };
    let Some(tag) = tag else {
        // no bound at all: the whole prefix
        return Ok(Some((B::Included(prefix.to_vec()), succ(prefix))));
    };
    let lo_key = match l {
        Some(e) => [prefix, &e].concat(),
        None => [prefix, &[tag]].concat(),
    };
    let hi_key = match h {
        Some(e) => succ(&[prefix, &e].concat()),
        None => B::Excluded([prefix, &[tag + 1]].concat()),
    };
    Ok(Some((B::Included(lo_key), hi_key)))
}

// =================================================================== EXPLAIN

pub fn fmt_expr(e: &Expr) -> String {
    use crate::pbsql::ast::SubRef;
    let sub = |s: &SubRef| match s {
        SubRef::Bound(i) => format!("SubPlan {i}"),
        SubRef::Parsed(_) => "subquery".into(),
    };
    match e {
        Expr::Lit(Value::Text(s)) => format!("'{}'", s.replace('\'', "''")),
        Expr::Lit(v) => v.to_json(),
        Expr::Col(p) => p.join("."),
        Expr::Slot { level: 0, name, .. } => name.clone(),
        Expr::Slot { level, name, .. } => format!("{}{name}", "outer.".repeat(*level)),
        Expr::Bin(a, op, b) => format!("({} {} {})", fmt_expr(a), op.text(), fmt_expr(b)),
        Expr::Not(a) => format!("NOT {}", fmt_expr(a)),
        Expr::Neg(a) => format!("-{}", fmt_expr(a)),
        Expr::IsNull(a, n) => format!("{} IS {}NULL", fmt_expr(a), if *n { "NOT " } else { "" }),
        Expr::Like(a, p, n) => format!(
            "{} {}LIKE {}",
            fmt_expr(a),
            if *n { "NOT " } else { "" },
            fmt_expr(p)
        ),
        Expr::In(a, l, n) => format!(
            "{} {}IN ({})",
            fmt_expr(a),
            if *n { "NOT " } else { "" },
            l.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
        ),
        Expr::InQuery(a, s, n) => format!(
            "{} {}IN ({})",
            fmt_expr(a),
            if *n { "NOT " } else { "" },
            sub(s)
        ),
        Expr::Exists(s, n) => format!("{}EXISTS ({})", if *n { "NOT " } else { "" }, sub(s)),
        Expr::ScalarQuery(s) => format!("({})", sub(s)),
        Expr::Agg(f, a, d) => format!(
            "{}({}{})",
            f.name(),
            if *d { "DISTINCT " } else { "" },
            a.as_ref().map_or("*".into(), |x| fmt_expr(x))
        ),
        Expr::Func(n, a) => format!(
            "{n}({})",
            a.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
        ),
        Expr::Case { .. } => "CASE ...".into(),
        Expr::Cast(a, t) => format!("CAST({} AS {})", fmt_expr(a), t.name().to_uppercase()),
        Expr::Default => "DEFAULT".into(),
        Expr::Untyped(t) => format!("'{}'", t.replace('\'', "''")),
    }
}

fn fmt_bound(name: &str, lo: &Option<Bound>, hi: &Option<Bound>) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(b) = lo {
        v.push(format!(
            "{name} {} {}",
            if b.inclusive { ">=" } else { ">" },
            fmt_expr(&b.expr)
        ));
    }
    if let Some(b) = hi {
        v.push(format!(
            "{name} {} {}",
            if b.inclusive { "<=" } else { "<" },
            fmt_expr(&b.expr)
        ));
    }
    v
}

fn describe_access(access: &Access, alias: &str) -> (String, Option<String>) {
    let on = |kind: &str, what: &str| {
        if alias == what || what.ends_with(&format!(".{alias}")) {
            format!("{kind} on {what}")
        } else {
            format!("{kind} on {what} {alias}")
        }
    };
    match access {
        Access::Table { schema, path } => {
            let pk = &schema.pk;
            match path {
                TablePath::FullScan => (on("TableScan", &schema.name), None),
                TablePath::PkPoints(points) => {
                    let conds: Vec<String> = points
                        .iter()
                        .map(|p| {
                            pk.iter()
                                .zip(p)
                                .map(|(c, e)| format!("{c} = {}", fmt_expr(e)))
                                .collect::<Vec<_>>()
                                .join(" AND ")
                        })
                        .collect();
                    (
                        on("PrimaryKeyLookup", &schema.name),
                        Some(conds.join(" OR ")),
                    )
                }
                TablePath::PkRange { eq, lo, hi } => {
                    let mut c: Vec<String> = pk
                        .iter()
                        .zip(eq)
                        .map(|(c, e)| format!("{c} = {}", fmt_expr(e)))
                        .collect();
                    c.extend(fmt_bound(&pk[eq.len()], lo, hi));
                    (
                        on("PrimaryKeyRangeScan", &schema.name),
                        Some(c.join(" AND ")),
                    )
                }
                TablePath::Index { ix, eq, lo, hi } => {
                    let mut c: Vec<String> = ix
                        .cols
                        .iter()
                        .zip(eq)
                        .map(|(c, e)| format!("{c} = {}", fmt_expr(e)))
                        .collect();
                    let ranged = lo.is_some() || hi.is_some();
                    if let Some(next) = ix.cols.get(eq.len()) {
                        c.extend(fmt_bound(next, lo, hi));
                    }
                    let kind = if ranged || eq.len() < ix.cols.len() {
                        "IndexRangeScan"
                    } else {
                        "IndexScan"
                    };
                    (
                        format!("{} using {}", on(kind, &schema.name), ix.name),
                        Some(c.join(" AND ")),
                    )
                }
                TablePath::IndexIn { ix, vals } => (
                    format!("{} using {}", on("IndexScan", &schema.name), ix.name),
                    Some(format!(
                        "{} IN ({})",
                        ix.cols[0],
                        vals.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
                    )),
                ),
            }
        }
        Access::Doc { coll, path } => {
            let what = format!("DOCUMENT.{coll}");
            match path {
                DocPath::FullScan => (on("DocumentScan", &what), None),
                DocPath::Ids(ids) => (
                    on("DocumentIdLookup", &what),
                    Some(
                        ids.iter()
                            .map(|e| format!("_id = {}", fmt_expr(e)))
                            .collect::<Vec<_>>()
                            .join(" OR "),
                    ),
                ),
                DocPath::Index(c) => {
                    let mut cond: Vec<String> = c
                        .index
                        .fields
                        .iter()
                        .zip(&c.eq)
                        .map(|((p, _), e)| format!("{p} = {}", fmt_expr(e)))
                        .collect();
                    let ranged = c.lo.is_some() || c.hi.is_some();
                    if let Some((p, _)) = c.index.fields.get(c.eq.len()) {
                        let b = |x: &Option<(Expr, bool)>| {
                            x.as_ref().map(|(e, i)| Bound {
                                expr: e.clone(),
                                inclusive: *i,
                            })
                        };
                        cond.extend(fmt_bound(p, &b(&c.lo), &b(&c.hi)));
                    }
                    let kind = if ranged || c.eq.len() < c.index.fields.len() {
                        "DocumentIndexRangeScan"
                    } else {
                        "DocumentIndexScan"
                    };
                    let fields: Vec<&str> = c.index.paths();
                    (
                        format!(
                            "{} using index {} ({})",
                            on(kind, &what),
                            c.index.name,
                            fields.join(", ")
                        ),
                        (!cond.is_empty()).then(|| cond.join(" AND ")),
                    )
                }
            }
        }
        Access::Kv { ns, path } => {
            let what = if ns.is_empty() {
                "KV".to_string()
            } else {
                format!("KV.{ns}")
            };
            match path {
                KvPath::FullScan => (on("KVScan", &what), None),
                KvPath::Keys(k) => (
                    on("KVGet", &what),
                    Some(
                        k.iter()
                            .map(|e| format!("key = {}", fmt_expr(e)))
                            .collect::<Vec<_>>()
                            .join(" OR "),
                    ),
                ),
                KvPath::Prefix(p) => (on("KVPrefixScan", &what), Some(format!("key LIKE '{p}%'"))),
                KvPath::Range { lo, hi } => (
                    on("KVRangeScan", &what),
                    Some(fmt_bound("key", lo, hi).join(" AND ")),
                ),
            }
        }
    }
}

fn model_of(n: &Node) -> Option<&'static str> {
    match &n.op {
        PhysOp::Scan { access, .. } => Some(match access {
            Access::Table { .. } => "relational",
            Access::Doc { .. } => "document",
            Access::Kv { .. } => "kv",
        }),
        PhysOp::Filter { input, .. } | PhysOp::Project { input, .. } => model_of(input),
        _ => None,
    }
}

fn models(n: &Node, out: &mut HashSet<&'static str>) {
    if let Some(m) = model_of(n) {
        out.insert(m);
    }
    match &n.op {
        PhysOp::NestedLoopJoin { left, right, .. }
        | PhysOp::HashJoin { left, right, .. }
        | PhysOp::LookupJoin { left, right, .. } => {
            models(left, out);
            models(right, out);
        }
        PhysOp::Filter { input, .. } | PhysOp::Derived { input, .. } => models(input, out),
        _ => {}
    }
}

pub struct ExplainOpts {
    pub analyze: bool,
}

pub fn render(
    root: &Node,
    tree: usize,
    ex: Option<&Executor>,
    out: &mut Vec<String>,
    depth: usize,
) {
    let pad = if depth == 0 {
        String::new()
    } else {
        format!("{}-> ", "   ".repeat(depth - 1))
    };
    let detail_pad = "   ".repeat(depth) + "   ";
    let mut details: Vec<String> = Vec::new();
    let title = match &root.op {
        PhysOp::Unit => "Result (one row)".to_string(),
        PhysOp::Scan {
            access,
            alias,
            filter,
            limit,
            reverse,
        } => {
            let (t, cond) = describe_access(access, alias);
            if *reverse {
                details.push("Scan Direction: backward".into());
            }
            if let Some(c) = cond {
                details.push(format!("Index Cond: {c}"));
            }
            if let Some(f) = filter {
                details.push(format!("Filter: {}", fmt_expr(f)));
            }
            if let Some(l) = limit {
                details.push(format!("Stops after: {l} row(s)"));
            }
            t
        }
        PhysOp::Derived { alias, .. } => format!("SubqueryScan {alias}"),
        PhysOp::Filter { pred, .. } => {
            details.push(format!("Filter: {}", fmt_expr(pred)));
            "Filter".into()
        }
        PhysOp::NestedLoopJoin { on, kind: jk, .. }
        | PhysOp::HashJoin { on, kind: jk, .. }
        | PhysOp::LookupJoin { on, kind: jk, .. } => {
            let kind = match &root.op {
                PhysOp::NestedLoopJoin { .. } => "NestedLoopJoin",
                PhysOp::HashJoin { .. } => "HashJoin",
                _ => "IndexLookupJoin",
            };
            if let PhysOp::HashJoin { lkeys, rkeys, .. } = &root.op {
                details.push(format!(
                    "Hash Keys: ({}) = ({})",
                    lkeys.iter().map(fmt_expr).collect::<Vec<_>>().join(", "),
                    rkeys.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
                ));
            }
            if let Some(c) = on {
                details.push(format!("Join Cond: {}", fmt_expr(c)));
            }
            let mut ms = HashSet::new();
            models(root, &mut ms);
            let cross = if ms.len() > 1 {
                let mut v: Vec<&str> = ms.into_iter().collect();
                v.sort();
                format!(" [cross-model: {}]", v.join(" + "))
            } else {
                String::new()
            };
            let side = match jk {
                JoinKind::Left => " (LEFT)",
                JoinKind::Right => " (RIGHT)",
                JoinKind::Full => " (FULL)",
                _ => "",
            };
            format!("{kind}{side}{cross}")
        }
        PhysOp::HashAggregate { group, aggs, .. } => {
            if !group.is_empty() {
                details.push(format!(
                    "Group Key: {}",
                    group.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
                ));
            }
            if !aggs.is_empty() {
                details.push(format!(
                    "Aggregates: {}",
                    aggs.iter()
                        .map(|a| fmt_expr(&Expr::Agg(
                            a.func,
                            a.arg.clone().map(Box::new),
                            a.distinct
                        )))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            "HashAggregate".into()
        }
        PhysOp::Project { exprs, .. } => format!(
            "Projection ({})",
            exprs.iter().map(fmt_expr).collect::<Vec<_>>().join(", ")
        ),
        PhysOp::Distinct { .. } => "Distinct".into(),
        PhysOp::Sort { keys, .. } => format!(
            "Sort ({})",
            keys.iter()
                .map(|(e, d)| format!("{}{}", fmt_expr(e), if *d { " DESC" } else { "" }))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        PhysOp::Limit { limit, offset, .. } => match (limit, offset) {
            (Some(l), 0) => format!("Limit ({l})"),
            (Some(l), o) => format!("Limit ({l} offset {o})"),
            (None, o) => format!("Offset ({o})"),
        },
        PhysOp::Union { all, .. } => if *all { "UnionAll" } else { "Union" }.into(),
    };
    let stats = ex.map(|e| e.stat(tree, root.id));
    let suffix = match &stats {
        Some(s) if s.loops == 0 => "  (never executed)".to_string(),
        Some(s) => {
            let mut t = format!(
                "  (actual rows={} loops={} time={:.3} ms",
                s.rows,
                s.loops,
                s.nanos as f64 / 1e6
            );
            if matches!(root.op, PhysOp::Scan { .. }) || s.examined > 0 {
                t.push_str(&format!(" examined={}", s.examined));
            }
            t.push(')');
            if s.fallback {
                t.push_str(" [index not usable for these values: scanned]");
            }
            if s.spilled > 0 {
                t.push_str(&format!(
                    " [spilled to disk: {} run(s)/partition(s)]",
                    s.spilled
                ));
            }
            t
        }
        None => String::new(),
    };
    out.push(format!("{pad}{title}{suffix}"));
    for d in details {
        out.push(format!("{detail_pad}{d}"));
    }
    let kids: Vec<&Node> = match &root.op {
        PhysOp::Unit | PhysOp::Scan { .. } => vec![],
        PhysOp::Derived { input, .. }
        | PhysOp::Filter { input, .. }
        | PhysOp::HashAggregate { input, .. }
        | PhysOp::Project { input, .. }
        | PhysOp::Distinct { input }
        | PhysOp::Sort { input, .. }
        | PhysOp::Limit { input, .. } => vec![input],
        PhysOp::NestedLoopJoin { left, right, .. }
        | PhysOp::HashJoin { left, right, .. }
        | PhysOp::LookupJoin { left, right, .. }
        | PhysOp::Union { left, right, .. } => vec![left, right],
    };
    for k in kids {
        render(k, tree, ex, out, depth + 1);
    }
}

pub fn explain(plan: &Plan, ex: Option<&Executor>) -> Vec<String> {
    let mut out = Vec::new();
    render(&plan.root, 0, ex, &mut out, 0);
    for (i, sp) in plan.subs.iter().enumerate() {
        out.push(format!(
            "SubPlan {i}{}",
            if sp.correlated {
                " (correlated: runs once per outer row)"
            } else {
                " (runs once)"
            }
        ));
        render(&sp.root, i + 1, ex, &mut out, 1);
    }
    out
}

/// Run a planned query.

/// Run a planned query.
pub fn run(ctx: &Ctx, plan: &Plan) -> Result<Vec<Row>> {
    let ex = Executor::new(ctx, &plan.subs);
    ex.exec(&plan.root, &[])
}

/// Run a planned query and return its rows together with the executor (for EXPLAIN ANALYZE).
pub fn run_analyze<'p, 'c, 'a>(
    ctx: &'c Ctx<'a>,
    plan: &'p Plan,
) -> Result<(Vec<Row>, Executor<'p, 'c, 'a>, u128)> {
    let ex = Executor::new(ctx, &plan.subs);
    let t = Instant::now();
    let rows = ex.exec(&plan.root, &[])?;
    let nanos = t.elapsed().as_nanos();
    Ok((rows, ex, nanos))
}
