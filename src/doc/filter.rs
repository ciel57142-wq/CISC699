//! Query filters. A filter document is compiled once into a [`Filter`]
//! tree, which validates every operator and its argument up front (so a
//! bad filter fails even on an empty collection), then evaluated per document.
//!
//! Semantics, per field condition:
//! * The field's values are every value its path reaches ([`path_values`]);
//!   a missing field has none.
//! * Equality (`{f: v}`, `$eq`, `$in`) holds if any value equals the
//!   operand, or is an array with an element equal to it. Equality with
//!   `null` also holds for a missing field.
//! * `$ne` and `$nin` are the exact negations of `$eq` and `$in`.
//! * `$gt $gte $lt $lte` compare only values of the same type class
//!   (numbers with numbers, strings with strings, ...), against the value
//!   itself or any element of an array value.

use super::codec::Source;
use super::path::{is_literal_object, path_values, type_name, validate_path};
use crate::error::{Error, Result};
use crate::value::Value;
use std::cmp::Ordering;

#[derive(Debug, Clone)]
pub enum Filter {
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Nor(Vec<Filter>),
    Field(String, Vec<Cond>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CmpOp {
    Gt,
    Gte,
    Lt,
    Lte,
}

#[derive(Debug, Clone)]
pub enum Cond {
    Eq(Value),
    Ne(Value),
    Cmp(CmpOp, Value),
    In(Vec<Value>),
    Nin(Vec<Value>),
    Exists(bool),
    Not(Vec<Cond>),
    Type(Vec<String>),
    Size(usize),
    ElemMatchConds(Vec<Cond>),
    ElemMatchDoc(Box<Filter>),
}

const TYPE_NAMES: &[&str] =
    &["null", "bool", "int", "double", "number", "string", "array", "object", "objectId", "date", "long", "binData"];

fn is_operator_object(v: &Value) -> bool {
    matches!(v, Value::Object(o) if !o.is_empty() && !is_literal_object(o) && o.keys().all(|k| k.starts_with('$')))
}

/// Compile a filter document. `null` and `{}` match everything.
pub fn compile(filter: &Value) -> Result<Filter> {
    let obj = match filter {
        Value::Object(o) => o,
        Value::Null => return Ok(Filter::And(vec![])),
        other => return Err(Error::Type(format!("a filter must be a JSON object, got {}", type_name(other)))),
    };
    let mut parts = Vec::new();
    for (k, cond) in obj {
        parts.push(match k.as_str() {
            "$and" | "$or" | "$nor" => {
                let subs = match cond {
                    Value::Array(a) if !a.is_empty() => a,
                    _ => return Err(Error::Type(format!("{k} expects a non-empty array of filters"))),
                };
                let subs = subs.iter().map(compile).collect::<Result<Vec<_>>>()?;
                match k.as_str() {
                    "$and" => Filter::And(subs),
                    "$or" => Filter::Or(subs),
                    _ => Filter::Nor(subs),
                }
            }
            _ if k.starts_with('$') => return Err(Error::Parse(format!("unknown top-level filter operator {k}"))),
            _ => {
                validate_path(k)?;
                Filter::Field(k.clone(), compile_cond(cond)?)
            }
        });
    }
    Ok(Filter::And(parts))
}

fn compile_cond(cond: &Value) -> Result<Vec<Cond>> {
    if let Value::Object(o) = cond {
        let ops = o.keys().filter(|k| k.starts_with('$')).count();
        if ops > 0 && !is_literal_object(o) {
            if ops != o.len() {
                return Err(Error::Parse("a condition cannot mix operators ($...) with plain fields".into()));
            }
            return o.iter().map(|(op, arg)| compile_op(op, arg)).collect();
        }
    }
    Ok(vec![Cond::Eq(cond.clone())])
}

fn compile_op(op: &str, arg: &Value) -> Result<Cond> {
    let array = |what: &str| match arg {
        Value::Array(a) => Ok(a.clone()),
        _ => Err(Error::Type(format!("{what} expects an array"))),
    };
    Ok(match op {
        "$eq" => Cond::Eq(arg.clone()),
        "$ne" => Cond::Ne(arg.clone()),
        "$gt" => Cond::Cmp(CmpOp::Gt, arg.clone()),
        "$gte" => Cond::Cmp(CmpOp::Gte, arg.clone()),
        "$lt" => Cond::Cmp(CmpOp::Lt, arg.clone()),
        "$lte" => Cond::Cmp(CmpOp::Lte, arg.clone()),
        "$in" => Cond::In(array("$in")?),
        "$nin" => Cond::Nin(array("$nin")?),
        "$exists" => Cond::Exists(match arg {
            Value::Bool(b) => *b,
            Value::Int(n) => *n != 0,
            Value::Float(f) => *f != 0.0,
            _ => return Err(Error::Type("$exists expects true or false".into())),
        }),
        "$not" => {
            if !is_operator_object(arg) {
                return Err(Error::Type("$not expects an operator object, e.g. {$not: {$gt: 5}}".into()));
            }
            Cond::Not(compile_cond(arg)?)
        }
        "$type" => {
            let names: Vec<String> = match arg {
                Value::Text(s) => vec![s.clone()],
                Value::Array(a) if !a.is_empty() => a
                    .iter()
                    .map(|v| v.as_str().map(String::from).ok_or_else(|| Error::Type("$type names must be strings".into())))
                    .collect::<Result<_>>()?,
                _ => return Err(Error::Type("$type expects a type name or an array of them".into())),
            };
            if let Some(bad) = names.iter().find(|n| !TYPE_NAMES.contains(&n.as_str())) {
                return Err(Error::Type(format!("unknown $type '{bad}'; known: {}", TYPE_NAMES.join(", "))));
            }
            Cond::Type(names)
        }
        "$size" => Cond::Size(match arg {
            Value::Int(n) if *n >= 0 => *n as usize,
            Value::Float(f) if *f >= 0.0 && f.fract() == 0.0 => *f as usize,
            _ => return Err(Error::Type("$size expects a non-negative integer".into())),
        }),
        "$elemMatch" => match arg {
            Value::Object(o) if !o.is_empty() => {
                if is_operator_object(arg) && !o.keys().any(|k| matches!(k.as_str(), "$and" | "$or" | "$nor")) {
                    Cond::ElemMatchConds(compile_cond(arg)?)
                } else {
                    Cond::ElemMatchDoc(Box::new(compile(arg)?))
                }
            }
            _ => return Err(Error::Type("$elemMatch expects a non-empty object".into())),
        },
        other => return Err(Error::Parse(format!("unknown filter operator {other}"))),
    })
}

impl Filter {
    /// Does `doc` satisfy this filter?
    pub fn eval(&self, doc: &Value) -> bool {
        match self {
            Filter::And(fs) => fs.iter().all(|f| f.eval(doc)),
            Filter::Or(fs) => fs.iter().any(|f| f.eval(doc)),
            Filter::Nor(fs) => !fs.iter().any(|f| f.eval(doc)),
            Filter::Field(path, conds) => {
                let vals = path_values(doc, path);
                conds.iter().all(|c| c.eval(&vals))
            }
        }
    }

    /// [`Filter::eval`] on any field source, e.g. a stored document read
    /// lazily: only the values the filter's paths reach are decoded.
    pub fn eval_src(&self, doc: &dyn Source) -> Result<bool> {
        Ok(match self {
            Filter::And(fs) => {
                for f in fs {
                    if !f.eval_src(doc)? {
                        return Ok(false);
                    }
                }
                true
            }
            Filter::Or(fs) => {
                for f in fs {
                    if f.eval_src(doc)? {
                        return Ok(true);
                    }
                }
                false
            }
            Filter::Nor(fs) => {
                for f in fs {
                    if f.eval_src(doc)? {
                        return Ok(false);
                    }
                }
                true
            }
            Filter::Field(path, conds) => {
                let vals = doc.values(path)?;
                let refs: Vec<&Value> = vals.iter().map(|c| c.as_ref()).collect();
                conds.iter().all(|c| c.eval(&refs))
            }
        })
    }

    /// Matches every document (`{}`).
    pub fn is_empty(&self) -> bool {
        matches!(self, Filter::And(fs) if fs.is_empty())
    }

    /// Top-level conjunctive field conditions (through nested `$and`),
    /// which an access path may use to narrow the documents it reads.
    pub fn conjuncts(&self) -> Vec<(&str, &Cond)> {
        let mut out = Vec::new();
        fn go<'f>(f: &'f Filter, out: &mut Vec<(&'f str, &'f Cond)>) {
            match f {
                Filter::And(fs) => fs.iter().for_each(|f| go(f, out)),
                Filter::Field(p, cs) => out.extend(cs.iter().map(|c| (p.as_str(), c))),
                _ => {}
            }
        }
        go(self, &mut out);
        out
    }
}

fn same(a: &Value, b: &Value) -> bool {
    a.total_cmp(b) == Ordering::Equal
}

fn eq_any(vals: &[&Value], c: &Value) -> bool {
    if vals.is_empty() {
        return c.is_null();
    }
    vals.iter().any(|v| same(v, c) || matches!(v, Value::Array(a) if a.iter().any(|x| same(x, c))))
}

fn cmp_one(v: &Value, op: CmpOp, arg: &Value) -> bool {
    if v.rank() != arg.rank() {
        return false;
    }
    let o = v.total_cmp(arg);
    match op {
        CmpOp::Gt => o == Ordering::Greater,
        CmpOp::Gte => o != Ordering::Less,
        CmpOp::Lt => o == Ordering::Less,
        CmpOp::Lte => o != Ordering::Greater,
    }
}

fn has_type(v: &Value, name: &str) -> bool {
    let t = type_name(v);
    t == name || (name == "number" && matches!(v, Value::Int(_) | Value::Float(_)))
}

impl Cond {
    fn eval(&self, vals: &[&Value]) -> bool {
        match self {
            Cond::Eq(c) => eq_any(vals, c),
            Cond::Ne(c) => !eq_any(vals, c),
            Cond::Cmp(op, a) => vals
                .iter()
                .any(|v| cmp_one(v, *op, a) || matches!(v, Value::Array(xs) if xs.iter().any(|x| cmp_one(x, *op, a)))),
            Cond::In(items) => items.iter().any(|c| eq_any(vals, c)),
            Cond::Nin(items) => !items.iter().any(|c| eq_any(vals, c)),
            Cond::Exists(want) => !vals.is_empty() == *want,
            Cond::Not(cs) => !cs.iter().all(|c| c.eval(vals)),
            Cond::Type(names) => vals.iter().any(|v| {
                names.iter().any(|n| has_type(v, n))
                    || matches!(v, Value::Array(xs) if xs.iter().any(|x| names.iter().any(|n| has_type(x, n))))
            }),
            Cond::Size(n) => vals.iter().any(|v| matches!(v, Value::Array(a) if a.len() == *n)),
            Cond::ElemMatchConds(cs) => vals
                .iter()
                .any(|v| matches!(v, Value::Array(a) if a.iter().any(|e| cs.iter().all(|c| c.eval(&[e]))))),
            Cond::ElemMatchDoc(f) => vals.iter().any(|v| {
                matches!(v, Value::Array(a) if a.iter().any(|e| matches!(e, Value::Object(_)) && f.eval(e)))
            }),
        }
    }

    /// Does an element (or scalar) satisfy this condition on its own? Used by `$pull`.
    pub fn eval_value(&self, v: &Value) -> bool {
        self.eval(&[v])
    }
}

/// Compile and evaluate in one step.
pub fn matches(doc: &Value, filter: &Value) -> Result<bool> {
    Ok(compile(filter)?.eval(doc))
}

/// A condition used by `$pull` to select array elements: an operator
/// object applies to the element itself, a plain object is a filter on
/// object elements, anything else is equality.
pub enum ElementTest {
    Conds(Vec<Cond>),
    Doc(Filter),
    Equal(Value),
}

impl ElementTest {
    pub fn compile(arg: &Value) -> Result<ElementTest> {
        Ok(match arg {
            v if is_operator_object(v) => ElementTest::Conds(compile_cond(v)?),
            Value::Object(o) if !is_literal_object(o) && !o.is_empty() => ElementTest::Doc(compile(arg)?),
            other => ElementTest::Equal(other.clone()),
        })
    }

    pub fn test(&self, e: &Value) -> bool {
        match self {
            ElementTest::Conds(cs) => cs.iter().all(|c| c.eval_value(e)),
            ElementTest::Doc(f) => matches!(e, Value::Object(_)) && f.eval(e),
            ElementTest::Equal(v) => same(e, v),
        }
    }
}
