//! Read-only traversal of the relational syntax tree, including the bodies
//! of subqueries. Used by semantic analysis and by the cross-model sources
//! (which need to know which document fields a query names).

use super::ast::*;

/// Visit `e` and every expression below it. `f` returns whether to
/// descend into the node's children (including subquery bodies).
pub(crate) fn expr(e: &Expr, f: &mut dyn FnMut(&Expr) -> bool) {
    if !f(e) {
        return;
    }
    match e {
        Expr::Lit(_) | Expr::Col(..) => {}
        Expr::Not(a) | Expr::Neg(a) | Expr::IsNull(a, _) | Expr::Cast(a, _) => expr(a, f),
        Expr::Bin(a, _, b) | Expr::Like(a, b, _) => {
            expr(a, f);
            expr(b, f);
        }
        Expr::In(a, list, _) => {
            expr(a, f);
            list.iter().for_each(|x| expr(x, f));
        }
        Expr::InSub(a, sq, _) => {
            expr(a, f);
            query(&sq.q, f);
        }
        Expr::Exists(sq) | Expr::Subquery(sq) => query(&sq.q, f),
        Expr::Agg(_, a, _) => {
            if let Some(a) = a {
                expr(a, f);
            }
        }
        Expr::Func(_, args) => args.iter().for_each(|x| expr(x, f)),
        Expr::Case(op, whens, els) => {
            if let Some(o) = op {
                expr(o, f);
            }
            for (w, t) in whens {
                expr(w, f);
                expr(t, f);
            }
            if let Some(x) = els {
                expr(x, f);
            }
        }
    }
}

/// Visit every expression of a query, derived tables included.
pub(crate) fn query(q: &Query, f: &mut dyn FnMut(&Expr) -> bool) {
    select(&q.head, f);
    for (_, _, s) in &q.rest {
        select(s, f);
    }
    for o in &q.order_by {
        expr(&o.expr, f);
    }
}

pub(crate) fn select(s: &Select, f: &mut dyn FnMut(&Expr) -> bool) {
    for it in &s.items {
        if let Item::Expr(e, _) = it {
            expr(e, f);
        }
    }
    let from_item = |item: &FromItem, f: &mut dyn FnMut(&Expr) -> bool| {
        if let Source::Derived(q) = &item.src {
            query(q, f);
        }
    };
    if let Some(item) = &s.from {
        from_item(item, f);
    }
    for j in &s.joins {
        from_item(&j.item, f);
        if let Some(on) = &j.on {
            expr(on, f);
        }
    }
    for e in s.filter.iter().chain(&s.group_by).chain(s.having.iter()) {
        expr(e, f);
    }
    for o in &s.order_by {
        expr(&o.expr, f);
    }
}

/// Names of the fields a SELECT reads through `alias` (`alias.field` or
/// `alias.field.path...`), looking inside subqueries too.
pub(crate) fn fields_of(s: &Select, alias: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    select(s, &mut |e| {
        if let Expr::Col(Some(q), n) = e {
            if q == alias && !out.contains(n) {
                out.push(n.clone());
            }
        }
        true
    });
    out
}
