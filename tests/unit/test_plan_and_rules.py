"""T-U2 (planner output per operator shape) and T-U3 (each rule fires when it should and
not when it must not). No databases needed: the engine is built with connect=False."""

from datetime import datetime, timezone

import pytest

from engine.api import Engine
from engine.errors import EngineError
from engine.plan.logical import Aggregate, AggOut, Filter, Join, Limit, OutField, Pred, Project, Scan
from engine.rewrite.physical import lower
from engine.rewrite.rules import (
    RuleConfig, r1_predicate_pushdown, r1b_aggregate_pushdown, r2_bind_join,
    r3_projection_pushdown, r4_limit_pushdown, rewrite,
)


@pytest.fixture(scope="module")
def eng():
    return Engine(connect=False)


def logical(eng, q, v=None):
    return eng.plan(q, v)[0]


def phys(eng, q, v=None):
    return lower(eng.pm, logical(eng, q, v))


# ------------------------------------------------------------------ T-U2 planner

def test_plan_single_store_filter_sort_limit(eng):
    p = logical(eng, '{ customers(where: {country: {eq: "FI"}, sel: {lt: 10}}, '
                     'orderBy: [{field: sel, dir: DESC}], limit: 5) { id sel } }')
    assert p == Project(
        Limit(Filter(Scan("Customer"), (Pred("country", "eq", "FI"), Pred("sel", "lt", 10))),
              5, None, (("sel", "DESC"), ("id", "ASC"))),
        (OutField("id", "id", "scalar"), OutField("sel", "sel", "scalar")))


def test_plan_cross_store_join(eng):
    p = logical(eng, "{ customers(where: {sel: {lt: 3}}) { id orders { id } } }")
    assert isinstance(p, Project) and isinstance(p.child, Join)
    j = p.child
    assert j.cross and j.key == "orders" and j.ref.remote_key == "customerId"
    assert j.left == Filter(Scan("Customer"), (Pred("sel", "lt", 3),))
    assert j.right == Project(Scan("Order"), (OutField("id", "id", "scalar"),))


def test_plan_same_store_join_is_not_cross(eng):
    p = logical(eng, "{ vendors { id products { id } } }")
    assert isinstance(p.child, Join) and not p.child.cross


def test_plan_no_limit_means_no_limit_node(eng):
    assert logical(eng, "{ vendors { id } }") == Project(Scan("Vendor"), (OutField("id", "id", "scalar"),))


def test_plan_aliases_and_embedded(eng):
    p = logical(eng, "{ orders { oid: id lines { lineNo q: quantity } } }")
    assert p.fields == (OutField("oid", "id", "scalar"),
                        OutField("lines", "lines", "embedded",
                                 (OutField("lineNo", "lineNo", "scalar"), OutField("q", "quantity", "scalar"))))


def test_plan_named_fragments_flatten(eng):
    q = "query { customers { ...C } } fragment C on Customer { id email }"
    assert [f.key for f in logical(eng, q).fields] == ["id", "email"]


def test_plan_aggregate(eng):
    p = logical(eng, "{ ordersAggregate(groupBy: [customerId]) { key { customerId } count s: sum { totalCents } } }")
    assert p == Aggregate(Scan("Order"), ("customerId",), (("sum", "totalCents"),),
                          (AggOut("key", "key", (("customerId", "customerId"),)), AggOut("count", "count"),
                           AggOut("s", "sum", (("totalCents", "totalCents"),))))


def test_plan_coerces_id_and_datetime(eng):
    p = logical(eng, '{ orders(where: {id: {in: ["1", "2"]}, orderedAt: {gte: "2025-01-01T00:00:00.000Z"}}) { id } }')
    preds = p.child.preds
    assert preds[0] == Pred("id", "in", (1, 2))
    assert preds[1] == Pred("orderedAt", "gte", datetime(2025, 1, 1, tzinfo=timezone.utc))


@pytest.mark.parametrize("q,code", [
    ("mutation { x }", "UNSUPPORTED_OPERATION"),
    ("{ customers { ... on Customer { id } } }", "UNSUPPORTED_OPERATION"),
    ("{ customers { id @include(if: true) } }", "UNSUPPORTED_OPERATION"),
    ("{ customers { __typename } }", "UNSUPPORTED_OPERATION"),
    ("{ customers { id } vendors { id } }", "UNSUPPORTED_OPERATION"),
    ("{ products { feedback { product { vendor { id } } } } }", "UNSUPPORTED_OPERATION"),   # depth 3
    ("{ customers { orders { customer { feedback { id } } } } }", "UNSUPPORTED_OPERATION"),  # 2 crossings
    ("{ customers { nope } }", "GRAPHQL_VALIDATION_FAILED"),
    ("{ customers(limit: -1) { id } }", "GRAPHQL_VALIDATION_FAILED"),
    ('{ orders(where: {orderedAt: {gt: "yesterday"}}) { id } }', "GRAPHQL_VALIDATION_FAILED"),
    ("{ customers { id ", "GRAPHQL_VALIDATION_FAILED"),
])
def test_errors_have_stable_codes(eng, q, code):
    with pytest.raises(EngineError) as e:
        eng.plan(q)
    assert e.value.code == code


# ------------------------------------------------------------------ T-U3 rules

W5 = "{ customers(where: {sel: {lt: 10}}) { id orders(where: {totalCents: {gt: 5}}) { id } } }"


def test_lowered_plan_pushes_nothing(eng):
    n = phys(eng, W5)
    f = n.fragment
    assert f.predicates == [] and f.projection is None and f.limit is None and f.aggregate is None
    assert n.residual_preds == [Pred("sel", "lt", 10)]
    assert not n.joins[0].bind


def test_r1_fires_on_both_sides(eng):
    n = r1_predicate_pushdown(phys(eng, W5))
    assert n.fragment.predicates == [Pred("sel", "lt", 10)] and n.residual_preds == []
    assert n.joins[0].child.fragment.predicates == [Pred("totalCents", "gt", 5)]


def test_r1_is_pure(eng):
    n = phys(eng, W5)
    r1_predicate_pushdown(n)
    assert n.residual_preds == [Pred("sel", "lt", 10)]


def test_r2_marks_bind_join(eng):
    assert r2_bind_join(phys(eng, W5)).joins[0].bind


def test_r3_projection_includes_join_keys(eng):
    n = r3_projection_pushdown(phys(eng, W5))
    assert set(n.fragment.projection) == {"id", "sel"}           # sel: residual predicate
    assert set(n.joins[0].child.fragment.projection) == {"id", "totalCents", "customerId"}


def test_r4_fires_on_root_with_total_order(eng):
    n = rewrite(phys(eng, '{ customers(where: {sel: {lt: 9}}, orderBy: [{field: sel}], limit: 3, offset: 1) { id } }'),
                RuleConfig(predicate=True, aggregate=False, bind=False, projection=False, limit=True))
    assert n.fragment.limit == 3 and n.fragment.offset == 1
    assert n.fragment.order == [("sel", "ASC"), ("id", "ASC")] and n.residual_limit is None


def test_r4_does_not_fire_with_residual_filter(eng):
    n = r4_limit_pushdown(phys(eng, "{ customers(where: {sel: {lt: 9}}, limit: 3) { id } }"))
    assert n.fragment.limit is None and n.residual_limit == (3, None)


def test_r4_never_pushes_per_parent_limit(eng):
    n = rewrite(phys(eng, "{ customers(limit: 2) { id orders(limit: 1) { id } } }"), RuleConfig.a1())
    assert n.fragment.limit == 2
    child = n.joins[0].child
    assert child.per_parent and child.fragment.limit is None and child.residual_limit == (1, None)


def test_r1b_requires_pushed_filter(eng):
    q = "{ ordersAggregate(where: {totalCents: {gt: 1}}, groupBy: [customerId]) { count } }"
    assert r1b_aggregate_pushdown(phys(eng, q)).fragment.aggregate is None      # filter still residual
    n = r1b_aggregate_pushdown(r1_predicate_pushdown(phys(eng, q)))
    assert n.fragment.aggregate is not None and n.residual_aggregate is None


def test_r1b_respects_declared_capabilities(eng):
    q = "{ ordersAggregate(groupBy: [customerId]) { avg { totalCents } } }"
    n = r1b_aggregate_pushdown(phys(eng, q), {"mongo": {"agg_fns": {"sum"}}})
    assert n.fragment.aggregate is None and n.residual_aggregate is not None
    n = r1b_aggregate_pushdown(phys(eng, q), {"mongo": {"agg_fns": {"sum", "avg"}}})
    assert n.fragment.aggregate is not None


def test_a4_config_leaves_lowered_plan_unchanged(eng):
    n = phys(eng, W5)
    m = rewrite(n, RuleConfig.a4())
    assert m.fragment == n.fragment and m.residual_preds == n.residual_preds
