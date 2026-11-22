//! Aggregation pipelines: `$match $sort $skip $limit $project $group
//! $count $unwind $addFields/$set`.
//!
//! Execution plan:
//! * leading `$match` stages, then a `$sort` and any `$skip`/`$limit`, are
//!   pushed into the find planner, so they use indexes, early termination
//!   and top-k selection instead of materialising the collection;
//! * a `$group` or `$count` that follows reads only the fields it needs
//!   from the stored bytes (a `{$group: {_id: null, n: {$sum: 1}}}` decodes
//!   nothing at all);
//! * a simple `$project` right after the pushed stages decodes only the
//!   projected fields;
//! * the remaining stages run in memory, with `$sort` + `$limit` fused
//!   into a top-k selection.

use super::codec::{self, Doc, Fields};
use super::exec::{self, cmp_keys, parse_sort, Projection, Query};
use super::filter::Pred;
use super::update::set_path;
use super::{plan, IndexDef};
use crate::error::{Error, Result};
use crate::storage::Ctx;
use crate::value::Value;
use std::collections::{BTreeMap, HashMap};

/// An expression: a `"$field.path"` reference, an object of expressions, or a literal.
#[derive(Debug, Clone)]
enum Expr {
    Path(String),
    Object(Vec<(String, Expr)>),
    Lit(Value),
}

impl Expr {
    fn parse(v: &Value) -> Expr {
        match v {
            Value::Text(s) if s.starts_with('$') && s.len() > 1 => Expr::Path(s[1..].to_string()),
            Value::Object(o) if o.len() == 1 && o.contains_key("$literal") => Expr::Lit(o["$literal"].clone()),
            Value::Object(o) if !o.keys().any(|k| k.starts_with('$')) => {
                Expr::Object(o.iter().map(|(k, v)| (k.clone(), Expr::parse(v))).collect())
            }
            other => Expr::Lit(other.clone()),
        }
    }

    /// None when a path is missing.
    fn eval(&self, doc: &dyn Fields) -> Result<Option<Value>> {
        Ok(match self {
            Expr::Path(p) => doc.field(p)?.map(|c| c.into_owned()),
            Expr::Lit(v) => Some(v.clone()),
            Expr::Object(fields) => {
                let mut o = BTreeMap::new();
                for (k, e) in fields {
                    if let Some(v) = e.eval(doc)? {
                        o.insert(k.clone(), v);
                    }
                }
                Some(Value::Object(o))
            }
        })
    }

    fn reads_fields(&self) -> bool {
        match self {
            Expr::Path(_) => true,
            Expr::Lit(_) => false,
            Expr::Object(f) => f.iter().any(|(_, e)| e.reads_fields()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum AccOp {
    Sum,
    Avg,
    Min,
    Max,
    First,
    Last,
    Push,
    AddToSet,
    Count,
}

#[derive(Debug, Clone)]
struct GroupSpec {
    id: Expr,
    accs: Vec<(String, AccOp, Expr)>,
}

#[derive(Debug, Clone)]
enum Stage {
    Match(Pred),
    Sort(Vec<(String, bool)>),
    Skip(usize),
    Limit(usize),
    Project(Projection),
    Compute(Vec<(String, Expr)>, bool),
    Group(GroupSpec),
    Count(String),
    Unwind { path: String, keep_empty: bool },
}

fn count_arg(stage: &str, v: &Value) -> Result<usize> {
    match v.as_f64() {
        Some(n) if n >= 0.0 && n.fract() == 0.0 => Ok(n as usize),
        _ => Err(Error::Type(format!("{stage} expects a non-negative integer"))),
    }
}

fn field_path(stage: &str, v: &Value) -> Result<String> {
    match v.as_str() {
        Some(s) if s.starts_with('$') && s.len() > 1 => Ok(s[1..].to_string()),
        _ => Err(Error::Type(format!("{stage} expects a field path like \"$field\""))),
    }
}

fn parse_stage(stage: &Value) -> Result<Stage> {
    let Value::Object(st) = stage else {
        return Err(Error::Parse("pipeline stages must be objects".into()));
    };
    if st.len() != 1 {
        return Err(Error::Parse("a pipeline stage must have exactly one field, like {$match: {...}}".into()));
    }
    let (name, arg) = st.iter().next().expect("one field");
    Ok(match name.as_str() {
        "$match" => Stage::Match(Pred::parse(arg)?),
        "$sort" => {
            let spec = parse_sort(Some(arg))?;
            if spec.is_empty() {
                return Err(Error::Type("$sort needs at least one field".into()));
            }
            Stage::Sort(spec)
        }
        "$skip" => Stage::Skip(count_arg("$skip", arg)?),
        "$limit" => Stage::Limit(count_arg("$limit", arg)?),
        "$count" => match arg.as_str() {
            Some(f) if !f.is_empty() && !f.starts_with('$') && !f.contains('.') => Stage::Count(f.to_string()),
            _ => return Err(Error::Type("$count expects a field name".into())),
        },
        "$unwind" => match arg {
            Value::Object(o) => Stage::Unwind {
                path: field_path("$unwind", o.get("path").unwrap_or(&Value::Null))?,
                keep_empty: matches!(o.get("preserveNullAndEmptyArrays"), Some(Value::Bool(true))),
            },
            other => Stage::Unwind { path: field_path("$unwind", other)?, keep_empty: false },
        },
        "$project" | "$addFields" | "$set" => {
            let Value::Object(p) = arg else {
                return Err(Error::Type(format!("{name} expects an object")));
            };
            let computed = p.iter().any(|(_, v)| !matches!(v, Value::Bool(_) | Value::Int(_) | Value::Float(_)));
            if name == "$project" && !computed {
                match Projection::parse(arg)? {
                    Some(proj) => Stage::Project(proj),
                    None => return Err(Error::Type("$project needs at least one field".into())),
                }
            } else {
                // $project with computed fields keeps `_id`, the computed and the included fields
                let fields = p.iter().map(|(k, v)| (k.clone(), Expr::parse(v))).collect();
                Stage::Compute(fields, name == "$project")
            }
        }
        "$group" => {
            let Value::Object(g) = arg else {
                return Err(Error::Type("$group expects an object".into()));
            };
            let id = Expr::parse(g.get("_id").ok_or_else(|| Error::Type("$group needs an _id".into()))?);
            let mut accs = Vec::new();
            for (field, acc) in g.iter().filter(|(k, _)| *k != "_id") {
                let bad = || Error::Type(format!("$group field '{field}' must be an accumulator like {{$sum: 1}}"));
                let Value::Object(a) = acc else { return Err(bad()) };
                if a.len() != 1 {
                    return Err(bad());
                }
                let (op, operand) = a.iter().next().expect("one");
                let op = match op.as_str() {
                    "$sum" => AccOp::Sum,
                    "$avg" => AccOp::Avg,
                    "$min" => AccOp::Min,
                    "$max" => AccOp::Max,
                    "$first" => AccOp::First,
                    "$last" => AccOp::Last,
                    "$push" => AccOp::Push,
                    "$addToSet" => AccOp::AddToSet,
                    "$count" => AccOp::Count,
                    other => return Err(Error::Parse(format!("unsupported accumulator {other}"))),
                };
                accs.push((field.clone(), op, Expr::parse(operand)));
            }
            Stage::Group(GroupSpec { id, accs })
        }
        other => return Err(Error::Parse(format!("unsupported aggregation stage {other}"))),
    })
}

// ------------------------------------------------------------------ $group

enum Acc {
    Sum { int: i64, float: f64, is_float: bool },
    Avg { sum: f64, n: u64 },
    Pick(Option<Value>),
    List(Vec<Value>),
    Count(i64),
}

impl Acc {
    fn new(op: AccOp) -> Acc {
        match op {
            AccOp::Sum => Acc::Sum { int: 0, float: 0.0, is_float: false },
            AccOp::Avg => Acc::Avg { sum: 0.0, n: 0 },
            AccOp::Min | AccOp::Max | AccOp::First | AccOp::Last => Acc::Pick(None),
            AccOp::Push | AccOp::AddToSet => Acc::List(Vec::new()),
            AccOp::Count => Acc::Count(0),
        }
    }

    fn add(&mut self, op: AccOp, v: Option<Value>, first_in_group: bool) {
        match (self, op) {
            (Acc::Sum { int, float, is_float }, _) => match v {
                Some(Value::Int(i)) if !*is_float => match int.checked_add(i) {
                    Some(s) => *int = s,
                    None => {
                        *is_float = true;
                        *float = *int as f64 + i as f64;
                    }
                },
                Some(Value::Int(i)) => *float += i as f64,
                Some(Value::Float(f)) => {
                    if !*is_float {
                        *is_float = true;
                        *float = *int as f64;
                    }
                    *float += f;
                }
                _ => {} // non-numeric values are ignored
            },
            (Acc::Avg { sum, n }, _) => {
                if let Some(x) = v.as_ref().and_then(Value::as_f64) {
                    *sum += x;
                    *n += 1;
                }
            }
            (Acc::Pick(cur), AccOp::First) => {
                if first_in_group {
                    *cur = Some(v.unwrap_or(Value::Null));
                }
            }
            (Acc::Pick(cur), AccOp::Last) => *cur = Some(v.unwrap_or(Value::Null)),
            (Acc::Pick(cur), _) => {
                // $min / $max ignore missing and null values
                if let Some(x) = v.filter(|x| !x.is_null()) {
                    let better = match cur {
                        None => true,
                        Some(c) => {
                            let o = x.total_cmp(c);
                            if op == AccOp::Min {
                                o == std::cmp::Ordering::Less
                            } else {
                                o == std::cmp::Ordering::Greater
                            }
                        }
                    };
                    if better {
                        *cur = Some(x);
                    }
                }
            }
            (Acc::List(items), _) => {
                if let Some(x) = v {
                    if op == AccOp::Push || !items.iter().any(|i| i.total_cmp(&x) == std::cmp::Ordering::Equal) {
                        items.push(x);
                    }
                }
            }
            (Acc::Count(n), _) => *n += 1,
        }
    }

    fn finish(self) -> Value {
        match self {
            Acc::Sum { int, float, is_float } => {
                if is_float {
                    Value::Float(float)
                } else {
                    Value::Int(int)
                }
            }
            Acc::Avg { sum, n } => {
                if n == 0 {
                    Value::Null
                } else {
                    Value::Float(sum / n as f64)
                }
            }
            Acc::Pick(v) => v.unwrap_or(Value::Null),
            Acc::List(items) => Value::Array(items),
            Acc::Count(n) => Value::Int(n),
        }
    }
}

/// Group keys compare like MongoDB: 1 and 1.0 are the same group.
fn canonical(v: &Value) -> Value {
    match v {
        Value::Float(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Value::Int(*f as i64),
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), canonical(v))).collect()),
        other => other.clone(),
    }
}

struct Grouper<'g> {
    spec: &'g GroupSpec,
    index: HashMap<Vec<u8>, usize>,
    groups: Vec<(Value, Vec<Acc>)>,
}

impl<'g> Grouper<'g> {
    fn new(spec: &'g GroupSpec) -> Self {
        Grouper { spec, index: HashMap::new(), groups: Vec::new() }
    }

    fn add(&mut self, doc: &dyn Fields) -> Result<()> {
        let id = self.spec.id.eval(doc)?.unwrap_or(Value::Null);
        let key = codec::encode(&canonical(&id));
        let (slot, fresh) = match self.index.get(&key) {
            Some(&i) => (i, false),
            None => {
                let i = self.groups.len();
                self.groups.push((id, self.spec.accs.iter().map(|(_, op, _)| Acc::new(*op)).collect()));
                self.index.insert(key, i);
                (i, true)
            }
        };
        for (acc, (_, op, e)) in self.groups[slot].1.iter_mut().zip(&self.spec.accs) {
            let v = if *op == AccOp::Count { None } else { e.eval(doc)? };
            acc.add(*op, v, fresh);
        }
        Ok(())
    }

    fn finish(self) -> Vec<Value> {
        let names: Vec<&String> = self.spec.accs.iter().map(|(n, _, _)| n).collect();
        self.groups
            .into_iter()
            .map(|(id, accs)| {
                let mut o = BTreeMap::new();
                o.insert("_id".to_string(), id);
                for (n, a) in names.iter().zip(accs) {
                    o.insert((*n).clone(), a.finish());
                }
                Value::Object(o)
            })
            .collect()
    }
}

// --------------------------------------------------------------- execution

fn sort_top(docs: &mut Vec<Value>, spec: &[(String, bool)], keep: Option<usize>) -> Result<()> {
    let mut keyed: Vec<(Vec<Value>, usize, Value)> = std::mem::take(docs)
        .into_iter()
        .enumerate()
        .map(|(i, d)| {
            let k = spec.iter().map(|(p, _)| Ok(d.field(p)?.map(|c| c.into_owned()).unwrap_or(Value::Null))).collect::<Result<Vec<_>>>()?;
            Ok((k, i, d))
        })
        .collect::<Result<_>>()?;
    let cmp = |a: &(Vec<Value>, usize, Value), b: &(Vec<Value>, usize, Value)| cmp_keys(&a.0, &b.0, spec).then(a.1.cmp(&b.1));
    if let Some(k) = keep {
        if k == 0 {
            keyed.clear();
        } else if k < keyed.len() {
            keyed.select_nth_unstable_by(k - 1, cmp);
            keyed.truncate(k);
        }
    }
    keyed.sort_unstable_by(cmp);
    *docs = keyed.into_iter().map(|(_, _, d)| d).collect();
    Ok(())
}

fn compute(doc: Value, fields: &[(String, Expr)], is_project: bool) -> Result<Value> {
    // Every expression reads the input document, not the one being built.
    let mut values: Vec<(&str, Value)> = Vec::with_capacity(fields.len());
    let mut exclude_id = false;
    for (k, e) in fields {
        match (is_project, e) {
            // inclusion flags inside a $project that also computes fields
            (true, Expr::Lit(v)) if matches!(v, Value::Bool(_) | Value::Int(_) | Value::Float(_)) => {
                let on = !matches!(v, Value::Bool(false)) && v.as_f64() != Some(0.0);
                if k == "_id" {
                    exclude_id = !on;
                } else if on {
                    if let Some(x) = super::get_path(&doc, k) {
                        values.push((k, x.clone()));
                    }
                }
            }
            _ => {
                if let Some(v) = e.eval(&doc)? {
                    values.push((k, v));
                }
            }
        }
    }
    let mut out = if is_project {
        let mut o = BTreeMap::new();
        if let (false, Value::Object(mut d)) = (exclude_id, doc) {
            if let Some(id) = d.remove("_id") {
                o.insert("_id".to_string(), id);
            }
        }
        Value::Object(o)
    } else {
        doc
    };
    for (k, v) in values {
        set_path(&mut out, k, v)?;
    }
    Ok(out)
}

fn run_stage(docs: Vec<Value>, stage: &Stage, next: Option<&Stage>) -> Result<(Vec<Value>, bool)> {
    let mut docs = docs;
    let mut consumed_next = false;
    match stage {
        Stage::Match(p) => {
            let mut kept = Vec::with_capacity(docs.len());
            for d in docs {
                if p.eval(&d)? {
                    kept.push(d);
                }
            }
            docs = kept;
        }
        Stage::Sort(spec) => {
            let keep = match next {
                Some(Stage::Limit(n)) => {
                    consumed_next = true;
                    Some(*n)
                }
                _ => None,
            };
            sort_top(&mut docs, spec, keep)?;
        }
        Stage::Skip(n) => {
            docs.drain(..(*n).min(docs.len()));
        }
        Stage::Limit(n) => docs.truncate(*n),
        Stage::Project(p) => docs = docs.into_iter().map(|d| p.apply(d)).collect(),
        Stage::Compute(fields, is_project) => {
            docs = docs.into_iter().map(|d| compute(d, fields, *is_project)).collect::<Result<_>>()?
        }
        Stage::Group(spec) => {
            let mut g = Grouper::new(spec);
            for d in &docs {
                g.add(d)?;
            }
            docs = g.finish();
        }
        Stage::Count(field) => {
            docs = if docs.is_empty() {
                vec![]
            } else {
                let mut o = BTreeMap::new();
                o.insert(field.clone(), Value::Int(docs.len() as i64));
                vec![Value::Object(o)]
            }
        }
        Stage::Unwind { path, keep_empty } => {
            let mut out = Vec::with_capacity(docs.len());
            for d in docs {
                match super::get_path(&d, path) {
                    Some(Value::Array(a)) if !a.is_empty() => {
                        for x in a.clone() {
                            let mut c = d.clone();
                            set_path(&mut c, path, x)?;
                            out.push(c);
                        }
                    }
                    Some(Value::Array(_)) | Some(Value::Null) | None => {
                        if *keep_empty {
                            out.push(d);
                        }
                    }
                    Some(_) => out.push(d),
                }
            }
            docs = out;
        }
    }
    Ok((docs, consumed_next))
}

pub fn aggregate(ctx: &Ctx, coll: &str, pipeline: &[Value], indexes: &[IndexDef]) -> Result<Vec<Value>> {
    let stages = pipeline.iter().map(parse_stage).collect::<Result<Vec<_>>>()?;

    // Push the leading $match / $sort / $skip / $limit (and a simple $project) into the query.
    let mut i = 0;
    let mut preds = Vec::new();
    while let Some(Stage::Match(p)) = stages.get(i) {
        preds.push(p.clone());
        i += 1;
    }
    let mut q = Query {
        pred: if preds.len() == 1 { preds.pop().expect("one") } else { Pred::And(preds) },
        sort: vec![],
        skip: 0,
        limit: None,
        projection: None,
    };
    if let Some(Stage::Sort(spec)) = stages.get(i) {
        q.sort = spec.clone();
        i += 1;
    }
    loop {
        match stages.get(i) {
            Some(Stage::Skip(n)) => {
                q.skip = q.skip.saturating_add(*n);
                q.limit = q.limit.map(|l| l.saturating_sub(*n));
            }
            Some(Stage::Limit(n)) => q.limit = Some(q.limit.map_or(*n, |l| l.min(*n))),
            _ => break,
        }
        i += 1;
    }
    let p = plan::plan(ctx, coll, &q.pred, &q.sort, indexes)?;

    // $group / $count straight off the stored bytes.
    let mut docs = match stages.get(i) {
        Some(Stage::Count(field)) => {
            i += 1;
            let n = if q.sort.is_empty() && q.skip == 0 && q.limit.is_none() {
                exec::count(ctx, coll, &p, &q.pred)?
            } else {
                exec::select(ctx, coll, &p, &q)?.len()
            };
            if n == 0 {
                vec![]
            } else {
                let mut o = BTreeMap::new();
                o.insert(field.clone(), Value::Int(n as i64));
                vec![Value::Object(o)]
            }
        }
        Some(Stage::Group(spec)) => {
            i += 1;
            let mut g = Grouper::new(spec);
            let needs_fields = spec.id.reads_fields() || spec.accs.iter().any(|(_, op, e)| *op != AccOp::Count && e.reads_fields());
            let empty = Value::Null;
            for raw in exec::select(ctx, coll, &p, &q)? {
                if needs_fields {
                    g.add(&Doc::from_stored(raw)?)?;
                } else {
                    g.add(&empty)?;
                }
            }
            g.finish()
        }
        Some(Stage::Project(proj)) => {
            i += 1;
            q.projection = Some(proj.clone());
            exec::run(ctx, coll, &p, &q)?
        }
        _ => exec::run(ctx, coll, &p, &q)?,
    };

    while i < stages.len() {
        let (out, consumed_next) = run_stage(docs, &stages[i], stages.get(i + 1))?;
        docs = out;
        i += if consumed_next { 2 } else { 1 };
    }
    Ok(docs)
}
