"""Physical plan: per-backend ScanFragments plus in-engine (residual) operators.

`lower()` turns a logical plan into a physical plan in which NOTHING is pushed down:
every predicate, sort, limit and aggregate is residual and every scan reads the whole
collection. That is exactly arm A4. The rewrite rules in rules.py then move work from
the residual side into fragments, one rule at a time.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from engine.plan.logical import (
    Aggregate, AggOut, Filter, Join, Limit, Node, OutField, Pred, Project, Scan,
)
from engine.schema.resolver import PlacementMap, RefInfo


@dataclass(frozen=True)
class AggSpec:
    group_by: tuple[str, ...]
    measures: tuple[tuple[str, str], ...]


@dataclass
class ScanFragment:
    backend: str
    kind: str                         # postgres | mongodb
    collection: str
    type_name: str
    field_types: dict[str, str]
    all_fields: tuple[str, ...]       # scalar + embedded fields stored on this type
    predicates: list[Pred] = field(default_factory=list)
    projection: list[str] | None = None        # None = every stored field
    order: list[tuple[str, str]] = field(default_factory=list)
    limit: int | None = None
    offset: int | None = None
    aggregate: AggSpec | None = None
    key_filter: tuple[str, list[Any]] | None = None   # set at run time by the bind join


@dataclass
class PhysJoin:
    key: str
    ref: RefInfo
    cross: bool
    bind: bool
    child: "PhysNode"


@dataclass
class PhysNode:
    type_name: str
    fragment: ScanFragment
    outputs: tuple[OutField, ...] = ()
    residual_preds: list[Pred] = field(default_factory=list)
    residual_order: list[tuple[str, str]] = field(default_factory=list)
    residual_limit: tuple[int | None, int | None] | None = None   # (n, offset)
    residual_aggregate: AggSpec | None = None
    agg_out: tuple[AggOut, ...] = ()
    joins: list[PhysJoin] = field(default_factory=list)
    per_parent: bool = False          # order/limit apply per parent group, not globally

    def walk(self):
        yield self
        for j in self.joins:
            yield from j.child.walk()


def _fragment(pm: PlacementMap, type_name: str) -> ScanFragment:
    t = pm.types[type_name]
    b = pm.backends[t.backend]
    stored = tuple(f.name for f in t.fields.values() if f.kind in ("scalar", "embedded"))
    return ScanFragment(b.name, b.kind, t.collection, type_name,
                        {f.name: f.type_name for f in t.fields.values()}, stored)


def lower(pm: PlacementMap, plan: Node, per_parent: bool = False) -> PhysNode:
    if isinstance(plan, Aggregate):
        inner = lower(pm, plan.child, per_parent)
        inner.residual_aggregate = AggSpec(plan.group_by, plan.measures)
        inner.agg_out = plan.out
        return inner
    if isinstance(plan, Project):
        n = lower(pm, plan.child, per_parent)
        n.outputs = plan.fields
        return n
    if isinstance(plan, Limit):
        n = lower(pm, plan.child, per_parent)
        n.residual_order = list(plan.order)
        if plan.n is not None or plan.offset is not None:
            n.residual_limit = (plan.n, plan.offset)
        return n
    if isinstance(plan, Join):
        n = lower(pm, plan.left, per_parent)
        child = lower(pm, plan.right, per_parent=True)
        n.joins.append(PhysJoin(plan.key, plan.ref, plan.cross, False, child))
        return n
    if isinstance(plan, Filter):
        n = lower(pm, plan.child, per_parent)
        n.residual_preds = list(plan.preds)
        return n
    if isinstance(plan, Scan):
        return PhysNode(plan.type_name, _fragment(pm, plan.type_name), per_parent=per_parent)
    raise TypeError(plan)


def explain_physical(n: PhysNode, indent: int = 0) -> str:
    pad = "  " * indent
    f = n.fragment
    parts = [f"{pad}{n.type_name} @ {f.backend}.{f.collection}"]
    if f.predicates:
        parts.append(f"{pad}  pushed preds: " + " AND ".join(f"{p.field} {p.op} {p.value!r}" for p in f.predicates))
    if f.projection is not None:
        parts.append(f"{pad}  pushed projection: {f.projection}")
    if f.order or f.limit is not None or f.offset is not None:
        parts.append(f"{pad}  pushed order/limit: {f.order} limit={f.limit} offset={f.offset}")
    if f.aggregate:
        parts.append(f"{pad}  pushed aggregate: {f.aggregate}")
    if n.residual_preds:
        parts.append(f"{pad}  RESIDUAL preds: " + " AND ".join(f"{p.field} {p.op} {p.value!r}" for p in n.residual_preds))
    if n.residual_aggregate:
        parts.append(f"{pad}  RESIDUAL aggregate: {n.residual_aggregate}")
    if n.residual_order or n.residual_limit:
        scope = "per parent" if n.per_parent else "global"
        parts.append(f"{pad}  RESIDUAL order/limit ({scope}): {n.residual_order} {n.residual_limit}")
    for j in n.joins:
        mode = "bind join (keys pushed)" if j.bind else "full scan + hash join"
        parts.append(f"{pad}  join {j.key}: {j.ref.local_key} = {j.ref.target}.{j.ref.remote_key} "
                     f"[{'cross-store' if j.cross else 'same-store'}; {mode}]")
        parts.append(explain_physical(j.child, indent + 2))
    return "\n".join(parts)
