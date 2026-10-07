"""Logical plan algebra (HS2 §6.2): Scan, Filter, Project, Join, Aggregate, Limit.

Shape of a relation subtree, outermost first:

    Project(fields) <- Limit(n, offset, order)? <- Join(...)* <- Filter(preds)? <- Scan(T)

A Join's right input is itself a relation subtree. A Limit inside a Join's right input is a
per-parent limit (`orders(limit: 3)`): it applies to each parent's matches, never globally.
An aggregate root is  Aggregate(group_by, measures) <- Filter? <- Scan.

All nodes are frozen dataclasses, so plans compare by value (T-U2 asserts plan equality).
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Union

from engine.schema.resolver import RefInfo


@dataclass(frozen=True)
class Pred:
    field: str
    op: str          # eq lt gt lte gte in
    value: Any


@dataclass(frozen=True)
class OutField:
    key: str                       # response key (alias or field name)
    field: str
    kind: str                      # scalar | embedded | ref
    sub: tuple["OutField", ...] = ()   # embedded sub-selection


@dataclass(frozen=True)
class Scan:
    type_name: str


@dataclass(frozen=True)
class Filter:
    child: "Node"
    preds: tuple[Pred, ...]


@dataclass(frozen=True)
class Join:
    left: "Node"
    right: "Node"
    key: str                       # response key the matches are attached under
    ref: RefInfo
    cross: bool                    # crosses a process boundary


@dataclass(frozen=True)
class Limit:
    child: "Node"
    n: int | None
    offset: int | None
    order: tuple[tuple[str, str], ...]


@dataclass(frozen=True)
class Project:
    child: "Node"
    fields: tuple[OutField, ...]


@dataclass(frozen=True)
class AggOut:
    key: str
    part: str                      # key | count | sum | avg | min | max
    subs: tuple[tuple[str, str], ...] = ()   # (response key, field)


@dataclass(frozen=True)
class Aggregate:
    child: "Node"
    group_by: tuple[str, ...]
    measures: tuple[tuple[str, str], ...]    # (fn, field), fn in sum avg min max
    out: tuple[AggOut, ...]


Node = Union[Scan, Filter, Join, Limit, Project, Aggregate]


def scan_of(node: Node) -> Scan:
    while not isinstance(node, Scan):
        node = node.left if isinstance(node, Join) else node.child
    return node


def explain(node: Node, indent: int = 0) -> str:
    """Readable plan tree, used in logs and the HS3 evidence."""
    pad = "  " * indent
    if isinstance(node, Scan):
        return f"{pad}Scan({node.type_name})"
    if isinstance(node, Filter):
        ps = " AND ".join(f"{p.field} {p.op} {p.value!r}" for p in node.preds)
        return f"{pad}Filter({ps})\n" + explain(node.child, indent + 1)
    if isinstance(node, Limit):
        return (f"{pad}Limit(n={node.n}, offset={node.offset}, order={list(node.order)})\n"
                + explain(node.child, indent + 1))
    if isinstance(node, Project):
        return (f"{pad}Project({', '.join(f.key for f in node.fields)})\n"
                + explain(node.child, indent + 1))
    if isinstance(node, Join):
        x = "cross-store" if node.cross else "same-store"
        return (f"{pad}Join({node.key}: {node.ref.local_key} = {node.ref.target}.{node.ref.remote_key}, {x})\n"
                + explain(node.left, indent + 1) + "\n" + explain(node.right, indent + 1))
    if isinstance(node, Aggregate):
        return (f"{pad}Aggregate(group_by={list(node.group_by)}, measures={list(node.measures)})\n"
                + explain(node.child, indent + 1))
    raise TypeError(node)
