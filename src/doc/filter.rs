//! Query parsing and predicate evaluation.
//!
//! A MongoDB-style filter is compiled once into a [`Pred`] tree: operator
//! names are resolved and argument shapes checked up front, instead of being
//! re-interpreted for every document. Evaluation reads fields through
//! [`Fields`], so it runs on a stored document without decoding it.

use super::codec::Fields;
use super::is_literal_object;
use crate::error::{Error, Result};
use crate::value::Value;
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CmpOp {
    Gt,
    Gte,
    Lt,
    Lte,
}

/// A condition on the value at one path.
#[derive(Debug, Clone)]
pub enum Cond {
    Eq(Value),
    Ne(Value),
    Cmp(CmpOp, Value),
    In(Vec<Value>),
    Nin(Vec<Value>),
    Exists(bool),
    /// True when not all of the inner conditions hold.
    Not(Vec<Cond>),
}

#[derive(Debug, Clone)]
pub enum Pred {
    /// Every sub-predicate holds (the top level of a filter, and `$and`).
    And(Vec<Pred>),
    Or(Vec<Pred>),
    Nor(Vec<Pred>),
    /// Every condition holds on the value at `path`.
    Field { path: String, conds: Vec<Cond> },
}

impl Pred {
    /// Compile a filter document. `null` and `{}` match everything.
    pub fn parse(filter: &Value) -> Result<Pred> {
        let obj = match filter {
            Value::Object(o) => o,
            Value::Null => return Ok(Pred::And(vec![])),
            _ => return Err(Error::Type("a filter must be a JSON object".into())),
        };
        let mut all = Vec::with_capacity(obj.len());
        for (k, cond) in obj {
            all.push(match k.as_str() {
                "$and" | "$or" | "$nor" => {
                    let Value::Array(subs) = cond else {
                        return Err(Error::Type(format!("{k} expects an array of filters")));
                    };
                    let subs = subs.iter().map(Pred::parse).collect::<Result<Vec<_>>>()?;
                    match k.as_str() {
                        "$and" => Pred::And(subs),
                        "$or" => Pred::Or(subs),
                        _ => Pred::Nor(subs),
                    }
                }
                _ if k.starts_with('$') => return Err(Error::Parse(format!("unknown filter operator {k}"))),
                _ => Pred::Field { path: k.clone(), conds: parse_conds(cond)? },
            });
        }
        Ok(if all.len() == 1 { all.pop().expect("one") } else { Pred::And(all) })
    }

    pub fn is_true(&self) -> bool {
        matches!(self, Pred::And(v) if v.is_empty())
    }

    pub fn eval(&self, doc: &dyn Fields) -> Result<bool> {
        Ok(match self {
            Pred::And(subs) => {
                for s in subs {
                    if !s.eval(doc)? {
                        return Ok(false);
                    }
                }
                true
            }
            Pred::Or(subs) => {
                for s in subs {
                    if s.eval(doc)? {
                        return Ok(true);
                    }
                }
                false
            }
            Pred::Nor(subs) => {
                for s in subs {
                    if s.eval(doc)? {
                        return Ok(false);
                    }
                }
                true
            }
            Pred::Field { path, conds } => {
                let v = doc.field(path)?;
                conds_hold(v.as_deref(), conds)
            }
        })
    }

    /// The conditions that every matching document must satisfy on top-level
    /// fields (directly or through `$and`), for index selection.
    pub fn required_fields(&self) -> Vec<(&str, &[Cond])> {
        let mut out = Vec::new();
        self.collect_required(&mut out);
        out
    }

    fn collect_required<'p>(&'p self, out: &mut Vec<(&'p str, &'p [Cond])>) {
        match self {
            Pred::And(subs) => subs.iter().for_each(|s| s.collect_required(out)),
            Pred::Field { path, conds } => out.push((path, conds)),
            Pred::Or(_) | Pred::Nor(_) => {}
        }
    }
}

/// An operator object (`{$gt: 1, $lt: 5}`) or a plain value (equality).
fn parse_conds(cond: &Value) -> Result<Vec<Cond>> {
    if let Value::Object(o) = cond {
        if !o.is_empty() && !is_literal_object(o) && o.keys().all(|k| k.starts_with('$')) {
            let mut out = Vec::with_capacity(o.len());
            for (op, arg) in o {
                out.push(match op.as_str() {
                    "$eq" => Cond::Eq(arg.clone()),
                    "$ne" => Cond::Ne(arg.clone()),
                    "$gt" => Cond::Cmp(CmpOp::Gt, arg.clone()),
                    "$gte" => Cond::Cmp(CmpOp::Gte, arg.clone()),
                    "$lt" => Cond::Cmp(CmpOp::Lt, arg.clone()),
                    "$lte" => Cond::Cmp(CmpOp::Lte, arg.clone()),
                    "$in" | "$nin" => {
                        let Value::Array(items) = arg else {
                            return Err(Error::Type(format!("{op} expects an array")));
                        };
                        if op == "$in" {
                            Cond::In(items.clone())
                        } else {
                            Cond::Nin(items.clone())
                        }
                    }
                    "$exists" => Cond::Exists(!matches!(arg, Value::Bool(false) | Value::Null | Value::Int(0))),
                    "$not" => Cond::Not(parse_conds(arg)?),
                    _ => return Err(Error::Parse(format!("unknown filter operator {op}"))),
                });
            }
            return Ok(out);
        }
    }
    Ok(vec![Cond::Eq(cond.clone())])
}

fn conds_hold(v: Option<&Value>, conds: &[Cond]) -> bool {
    conds.iter().all(|c| cond_holds(v, c))
}

/// Equality; an array field also matches when any element equals the operand.
pub fn eq_match(v: Option<&Value>, c: &Value) -> bool {
    match v {
        None => c.is_null(),
        Some(v) => {
            v.total_cmp(c) == Ordering::Equal
                || matches!(v, Value::Array(a) if a.iter().any(|x| x.total_cmp(c) == Ordering::Equal))
        }
    }
}

fn cond_holds(v: Option<&Value>, c: &Cond) -> bool {
    match c {
        Cond::Eq(arg) => eq_match(v, arg),
        Cond::Ne(arg) => !eq_match(v, arg),
        Cond::Cmp(op, arg) => match v {
            Some(v) if v.rank() == arg.rank() => {
                let o = v.total_cmp(arg);
                match op {
                    CmpOp::Gt => o == Ordering::Greater,
                    CmpOp::Gte => o != Ordering::Less,
                    CmpOp::Lt => o == Ordering::Less,
                    CmpOp::Lte => o != Ordering::Greater,
                }
            }
            _ => false,
        },
        Cond::In(items) => items.iter().any(|a| eq_match(v, a)),
        Cond::Nin(items) => !items.iter().any(|a| eq_match(v, a)),
        Cond::Exists(want) => v.is_some() == *want,
        Cond::Not(inner) => !conds_hold(v, inner),
    }
}
