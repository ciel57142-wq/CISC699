"""Executor (HS2 §4.5, §6.4): run fragments, bind hash join, residual operators, shaping.

Bind hash join: the driving side runs first; its distinct join-key values are pushed into
the other side as `key IN (...)` / `$in`, in batches of B = 1,000 (ADR-0006). Matches are
indexed in a dict on the remote key and attached to each parent. Time O(k + m), memory O(m).
Without R2 (arm A4) the other side is scanned whole and joined the same way.
"""

from __future__ import annotations

import time
from collections import defaultdict
from dataclasses import dataclass, field, replace
from datetime import datetime

from engine.adapters.base import ByteCount
from engine.errors import INTERNAL, EngineError
from engine.naming import iso_ms
from engine.plan.logical import Pred
from engine.rewrite.physical import AggSpec, PhysNode

BIND_BATCH = 1000


@dataclass
class FragmentTrace:
    type_name: str
    backend: str
    native: str
    bytes: ByteCount


@dataclass
class Trace:
    spans_ns: dict[str, int] = field(default_factory=lambda: defaultdict(int))
    fragments: list[FragmentTrace] = field(default_factory=list)
    driving_rows: int | None = None        # k: rows returned by the root scan (HS2 §6.5)

    def add(self, span: str, ns: int):
        self.spans_ns[span] += ns

    @property
    def payload_bytes(self) -> int:
        return sum(f.bytes.payload_bytes for f in self.fragments)

    def summary(self) -> dict:
        return {"spans_ms": {k: round(v / 1e6, 3) for k, v in self.spans_ns.items()},
                "payload_bytes": self.payload_bytes,
                "rows_transferred": sum(f.bytes.rows for f in self.fragments),
                "statements": sum(f.bytes.statements for f in self.fragments),
                "k": self.driving_rows}


def _match(row: dict, p: Pred) -> bool:
    v = row.get(p.field)
    if p.op == "eq":
        return v == p.value
    if p.op == "in":
        return v in p.value
    if v is None:
        return False
    return {"lt": v < p.value, "gt": v > p.value, "lte": v <= p.value, "gte": v >= p.value}[p.op]


def _sort(rows: list[dict], order: list[tuple[str, str]]) -> list[dict]:
    for f, d in reversed(order):            # stable multi-key sort
        rows.sort(key=lambda r: r[f], reverse=(d == "DESC"))
    return rows


def _slice(rows: list[dict], lim: tuple[int | None, int | None] | None) -> list[dict]:
    if not lim:
        return rows
    n, off = lim
    off = off or 0
    return rows[off:] if n is None else rows[off:off + n]


def _aggregate(rows: list[dict], spec: AggSpec) -> list[dict]:
    groups: dict[tuple, list[dict]] = defaultdict(list)
    for r in rows:
        groups[tuple(r[f] for f in spec.group_by)].append(r)
    if not groups and not spec.group_by:
        groups[()] = []
    out = []
    for key, rs in groups.items():
        o = {"key": dict(zip(spec.group_by, key)), "count": len(rs)}
        for fn, f in spec.measures:
            vals = [r[f] for r in rs]
            if not vals:
                o[f"{fn}:{f}"] = None
            elif fn == "sum":
                o[f"{fn}:{f}"] = sum(vals)
            elif fn == "avg":
                o[f"{fn}:{f}"] = sum(vals) / len(vals)
            elif fn == "min":
                o[f"{fn}:{f}"] = min(vals)
            else:
                o[f"{fn}:{f}"] = max(vals)
        out.append(o)
    return out


class Executor:
    def __init__(self, adapters: dict, batch: int = BIND_BATCH):
        self.adapters = adapters
        self.batch = batch

    # -------------------------------------------------------------- data movement
    def _scan(self, node: PhysNode, trace: Trace, key_filter=None) -> list[dict]:
        frag = replace(node.fragment, key_filter=key_filter)
        ad = self.adapters[frag.backend]
        t = time.perf_counter_ns()
        res = ad.scan(frag)
        trace.add(f"backend:{frag.backend}", res.bytes.fetch_ns)
        trace.add("decode", res.bytes.decode_ns)
        trace.add("engine_other", time.perf_counter_ns() - t - res.bytes.fetch_ns - res.bytes.decode_ns)
        trace.fragments.append(FragmentTrace(frag.type_name, frag.backend, res.native, res.bytes))
        return res.rows

    def run(self, node: PhysNode, trace: Trace, key_filter=None) -> list[dict]:
        rows = self._scan(node, trace, key_filter)
        t = time.perf_counter_ns()
        if node.residual_preds:
            rows = [r for r in rows if all(_match(r, p) for p in node.residual_preds)]
        if node.residual_aggregate:
            rows = _aggregate(rows, node.residual_aggregate)
        if not node.per_parent:
            if node.residual_order:
                rows = _sort(rows, node.residual_order)
            rows = _slice(rows, node.residual_limit)
        trace.add("residual", time.perf_counter_ns() - t)
        if trace.driving_rows is None:
            trace.driving_rows = len(rows)
        for j in node.joins:
            self._join(rows, j, trace)
        return rows

    def _join(self, rows: list[dict], j, trace: Trace) -> None:
        lk, rk = j.ref.local_key, j.ref.remote_key
        child = j.child
        if j.bind:
            keys = list(dict.fromkeys(r[lk] for r in rows if r.get(lk) is not None))
            matches: list[dict] = []
            for i in range(0, len(keys), self.batch):
                matches += self.run(child, trace, key_filter=(rk, keys[i:i + self.batch]))
        else:
            matches = self.run(child, trace)
        t = time.perf_counter_ns()
        index: dict = defaultdict(list)
        for m in matches:
            index[m[rk]].append(m)
        for r in rows:
            ms = index.get(r.get(lk), [])
            if j.ref.many:
                if child.residual_order:
                    ms = _sort(list(ms), child.residual_order)
                r[j.key] = _slice(ms, child.residual_limit)
            else:
                if len(ms) > 1:
                    raise EngineError(INTERNAL, f"to-one {j.key} matched {len(ms)} rows")
                r[j.key] = ms[0] if ms else None
        trace.add("join", time.perf_counter_ns() - t)

    # -------------------------------------------------------------- shaping
    def shape(self, node: PhysNode, rows: list[dict]) -> list[dict]:
        if node.agg_out:
            return [self._shape_agg(node, r) for r in rows]
        joins = {j.key: j for j in node.joins}
        types = node.fragment.field_types
        out = []
        for r in rows:
            o = {}
            for f in node.outputs:
                if f.kind == "scalar":
                    o[f.key] = _ser(types[f.field], r[f.field])
                elif f.kind == "embedded":
                    o[f.key] = [{s.key: x[s.field] for s in f.sub} for x in r[f.field]]
                else:
                    j = joins[f.key]
                    v = r[f.key]
                    if j.ref.many:
                        o[f.key] = self.shape(j.child, v)
                    else:
                        o[f.key] = self.shape(j.child, [v])[0] if v is not None else None
            out.append(o)
        return out

    def _shape_agg(self, node: PhysNode, r: dict) -> dict:
        types = node.fragment.field_types
        o = {}
        for a in node.agg_out:
            if a.part == "count":
                o[a.key] = r["count"]
            elif a.part == "key":
                o[a.key] = {k: _ser(types[f], r["key"].get(f)) for k, f in a.subs}
            else:
                o[a.key] = {k: (float(r[f"{a.part}:{f}"]) if a.part == "avg" and r[f"{a.part}:{f}"] is not None
                                else r[f"{a.part}:{f}"]) for k, f in a.subs}
        return o


def _ser(gql_type: str, v):
    if v is None:
        return None
    if gql_type == "ID":
        return str(v)
    if gql_type == "DateTime" and isinstance(v, datetime):
        return iso_ms(v)
    return v
