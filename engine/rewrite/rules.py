"""Rewrite rules (HS2 §6.3). Each rule is a pure function PhysNode -> PhysNode applied once,
in a fixed order. A rule that cannot prove its rewrite safe leaves the plan unchanged.

  R1   predicate pushdown      residual conjunctive predicates -> fragment.predicates
  R1b  aggregate pushdown      residual aggregate over a fully pushed filter -> GROUP BY / $group
  R2   bind join               join keys of the driving side pushed into the other side
  R3   projection pushdown     only needed fields requested (extension E-1)
  R4   limit pushdown          root order + limit/offset pushed when no residual work sits above

Arm A1 = all rules on. Arm A4 = all rules off (= the output of lower()).
"""

from __future__ import annotations

import copy
from dataclasses import dataclass

from engine.rewrite.physical import PhysNode

SUPPORTED_OPS = {
    "postgres": {"eq", "lt", "gt", "lte", "gte", "in"},
    "mongodb": {"eq", "lt", "gt", "lte", "gte", "in"},
}


@dataclass(frozen=True)
class RuleConfig:
    predicate: bool = True     # R1
    aggregate: bool = True     # R1b
    bind: bool = True          # R2
    projection: bool = True    # R3
    limit: bool = True         # R4

    @classmethod
    def a1(cls) -> "RuleConfig":
        return cls()

    @classmethod
    def a4(cls) -> "RuleConfig":
        return cls(False, False, False, False, False)


def _each(node: PhysNode, fn) -> PhysNode:
    node = copy.deepcopy(node)

    def rec(n: PhysNode):
        fn(n)
        for j in n.joins:
            rec(j.child)

    rec(node)
    return node


def r1_predicate_pushdown(node: PhysNode) -> PhysNode:
    def fn(n: PhysNode):
        ok = SUPPORTED_OPS[n.fragment.kind]
        if n.residual_preds and all(p.op in ok and p.field in n.fragment.field_types
                                    for p in n.residual_preds):
            n.fragment.predicates = n.fragment.predicates + n.residual_preds
            n.residual_preds = []
    return _each(node, fn)


def r1b_aggregate_pushdown(node: PhysNode, caps: dict | None = None) -> PhysNode:
    def fn(n: PhysNode):
        fns = (caps or {}).get(n.fragment.backend, {}).get("agg_fns", {"sum", "avg", "min", "max"})
        if (n.residual_aggregate and not n.residual_preds and not n.joins
                and all(f in fns for f, _ in n.residual_aggregate.measures)):
            n.fragment.aggregate = n.residual_aggregate
            n.residual_aggregate = None
    return _each(node, fn)


def r2_bind_join(node: PhysNode) -> PhysNode:
    def fn(n: PhysNode):
        for j in n.joins:
            j.bind = True     # equi-join on declared keys: semi-join reduction is always safe
    return _each(node, fn)


def r3_projection_pushdown(node: PhysNode) -> PhysNode:
    def fn(n: PhysNode):
        if n.residual_aggregate or n.fragment.aggregate:
            return
        need: list[str] = []
        for o in n.outputs:
            if o.kind in ("scalar", "embedded"):
                need.append(o.field)
        need += [p.field for p in n.residual_preds]
        need += [f for f, _ in n.residual_order] + [f for f, _ in n.fragment.order]
        need += [j.ref.local_key for j in n.joins]
        need.append("id")
        n.fragment.projection = list(dict.fromkeys(need))

    out = _each(node, fn)

    def add_remote(n: PhysNode):   # the child must return the key it is matched on
        for j in n.joins:
            if j.child.fragment.projection is not None and j.ref.remote_key not in j.child.fragment.projection:
                j.child.fragment.projection.append(j.ref.remote_key)
            add_remote(j.child)
    add_remote(out)
    return out


def r4_limit_pushdown(node: PhysNode) -> PhysNode:
    node = copy.deepcopy(node)
    n = node          # root only: a per-parent limit is never pushed across a bind join
    if n.per_parent or n.residual_preds or n.residual_aggregate:
        return node
    if not n.residual_order or n.residual_order[-1][0] != "id":
        return node   # needs a total order (planner appends id)
    if n.residual_limit is None:
        return node
    n.fragment.order = list(n.residual_order)
    n.fragment.limit, n.fragment.offset = n.residual_limit
    n.residual_order, n.residual_limit = [], None
    return node


def rewrite(node: PhysNode, cfg: RuleConfig, caps: dict | None = None) -> PhysNode:
    """caps: backend name -> declared capabilities (from the connected adapters)."""
    if cfg.predicate:
        node = r1_predicate_pushdown(node)
    if cfg.aggregate:
        node = r1b_aggregate_pushdown(node, caps)
    if cfg.bind:
        node = r2_bind_join(node)
    if cfg.limit:
        node = r4_limit_pushdown(node)
    if cfg.projection:         # last, so it sees which sorts/filters stayed residual
        node = r3_projection_pushdown(node)
    return node
