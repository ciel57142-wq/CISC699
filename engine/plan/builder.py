"""Build the logical plan from a validated operation (HS2 §6.2)."""

from __future__ import annotations

from graphql import (
    DocumentNode,
    FieldNode,
    FragmentDefinitionNode,
    FragmentSpreadNode,
    GraphQLSchema,
    OperationDefinitionNode,
    SelectionSetNode,
)
from graphql.execution.values import get_argument_values, get_variable_values

from engine.errors import GRAPHQL_VALIDATION_FAILED, EngineError, Unsupported
from engine.naming import parse_iso_ms
from engine.plan.logical import (
    Aggregate, AggOut, Filter, Join, Limit, Node, OutField, Pred, Project, Scan,
)
from engine.schema.resolver import PlacementMap

MAX_HOPS = 2
MAX_CROSSINGS = 1


class _Ctx:
    def __init__(self, schema, pm, fragments, variables):
        self.schema, self.pm, self.fragments, self.vars = schema, pm, fragments, variables
        self.crossings = 0


def _collect(ctx: _Ctx, sel: SelectionSetNode | None) -> list[tuple[str, str, FieldNode, list]]:
    """Flatten named fragments; merge repeated response keys. Returns (key, name, node, subsets)."""
    out: dict[str, tuple[str, FieldNode, list]] = {}

    def walk(s: SelectionSetNode):
        for item in s.selections:
            if isinstance(item, FragmentSpreadNode):
                walk(ctx.fragments[item.name.value].selection_set)
            elif isinstance(item, FieldNode):
                key = item.alias.value if item.alias else item.name.value
                if key in out:
                    if item.selection_set:
                        out[key][2].append(item.selection_set)
                else:
                    out[key] = (item.name.value, item, [item.selection_set] if item.selection_set else [])
            else:  # pragma: no cover - the validator rejects inline fragments first
                raise Unsupported("inline fragment")

    if sel is not None:
        walk(sel)
    return [(k, n, node, subs) for k, (n, node, subs) in out.items()]


def _merged(subsets: list[SelectionSetNode]) -> SelectionSetNode | None:
    if not subsets:
        return None
    if len(subsets) == 1:
        return subsets[0]
    return SelectionSetNode(selections=tuple(s for ss in subsets for s in ss.selections))


def _coerce(pm: PlacementMap, type_name: str, field: str, v):
    t = pm.types[type_name].fields[field].type_name
    try:
        if t == "ID" or t == "Long":
            return int(v)
        if t == "DateTime":
            return parse_iso_ms(v)
    except (ValueError, TypeError) as e:
        raise EngineError(GRAPHQL_VALIDATION_FAILED,
                          f"{type_name}.{field}: cannot read {v!r} as {t}") from e
    return v


def _preds(pm: PlacementMap, type_name: str, where: dict | None) -> tuple[Pred, ...]:
    out = []
    for field, ops in (where or {}).items():
        for op, v in ops.items():
            if v is None:
                continue
            if op == "in":
                out.append(Pred(field, "in", tuple(_coerce(pm, type_name, field, x) for x in v)))
            else:
                out.append(Pred(field, op, _coerce(pm, type_name, field, v)))
    return tuple(out)


def _order(order_by, limit, offset) -> tuple[tuple[str, str], ...]:
    order = tuple((o["field"], o.get("dir") or "ASC") for o in (order_by or []))
    if (order or limit is not None or offset is not None) and "id" not in {f for f, _ in order}:
        order += (("id", "ASC"),)       # total order, required for limit pushdown (R4)
    return order


def _relation(ctx: _Ctx, type_name: str, args: dict, sel, hops: int) -> Node:
    pm = ctx.pm
    t = pm.types[type_name]
    gql_type = ctx.schema.get_type(type_name)
    outs: list[OutField] = []
    joins: list[tuple[str, object, Node, bool]] = []
    for key, name, node, subsets in _collect(ctx, sel):
        f = t.fields[name]
        if f.kind == "scalar":
            outs.append(OutField(key, name, "scalar"))
        elif f.kind == "embedded":
            subs = tuple(OutField(k, n, "scalar") for k, n, _, _ in _collect(ctx, _merged(subsets)))
            outs.append(OutField(key, name, "embedded", subs))
        else:
            if hops + 1 > MAX_HOPS:
                raise Unsupported(f"depth {hops + 1} traversal at {type_name}.{name} (MVA allows {MAX_HOPS})")
            cross = pm.crosses(type_name, name)
            if cross:
                ctx.crossings += 1
                if ctx.crossings > MAX_CROSSINGS:
                    raise Unsupported("more than one cross-store traversal in one operation")
            cargs = get_argument_values(gql_type.fields[name], node, ctx.vars)
            child = _relation(ctx, f.ref.target, cargs, _merged(subsets), hops + 1)
            joins.append((key, f.ref, child, cross))
            outs.append(OutField(key, name, "ref"))

    plan: Node = Scan(type_name)
    preds = _preds(pm, type_name, args.get("where"))
    if preds:
        plan = Filter(plan, preds)
    for key, ref, child, cross in joins:
        plan = Join(plan, child, key, ref, cross)
    limit, offset = args.get("limit"), args.get("offset")
    if limit is not None and limit < 0 or offset is not None and offset < 0:
        raise EngineError(GRAPHQL_VALIDATION_FAILED, "limit and offset must be >= 0")
    order = _order(args.get("orderBy"), limit, offset)
    if order or limit is not None or offset is not None:
        plan = Limit(plan, limit, offset, order)
    return Project(plan, tuple(outs))


def _aggregate(ctx: _Ctx, type_name: str, args: dict, sel) -> Node:
    pm = ctx.pm
    allowed = set(pm.numeric_measure_fields(type_name))
    measures: list[tuple[str, str]] = []
    out: list[AggOut] = []
    for key, part, _node, subsets in _collect(ctx, sel):
        subs = tuple((k, n) for k, n, _, _ in _collect(ctx, _merged(subsets)))
        if part in ("sum", "avg", "min", "max"):
            for _, f in subs:
                assert f in allowed
                if (part, f) not in measures:
                    measures.append((part, f))
        out.append(AggOut(key, part, subs))
    plan: Node = Scan(type_name)
    preds = _preds(pm, type_name, args.get("where"))
    if preds:
        plan = Filter(plan, preds)
    group_by = tuple(dict.fromkeys(args.get("groupBy") or []))
    return Aggregate(plan, group_by, tuple(measures), tuple(out))


def build(schema: GraphQLSchema, pm: PlacementMap, doc: DocumentNode,
          variables: dict | None = None) -> Node:
    op = next(d for d in doc.definitions if isinstance(d, OperationDefinitionNode))
    fragments = {d.name.value: d for d in doc.definitions if isinstance(d, FragmentDefinitionNode)}
    coerced = get_variable_values(schema, op.variable_definitions or (), variables or {})
    if isinstance(coerced, list):
        raise EngineError(GRAPHQL_VALIDATION_FAILED, "; ".join(e.message for e in coerced))
    ctx = _Ctx(schema, pm, fragments, coerced)
    roots = _collect(ctx, op.selection_set)
    if len(roots) != 1:
        raise Unsupported(f"exactly one root field per operation (got {len(roots)})")
    _key, name, node, subsets = roots[0]
    type_name, kind = pm.roots[name]
    args = get_argument_values(schema.query_type.fields[name], node, coerced)
    if kind == "list":
        return _relation(ctx, type_name, args, _merged(subsets), 0)
    return _aggregate(ctx, type_name, args, _merged(subsets))


def root_key(doc: DocumentNode) -> str:
    op = next(d for d in doc.definitions if isinstance(d, OperationDefinitionNode))
    f = op.selection_set.selections[0]
    if isinstance(f, FragmentSpreadNode):  # pragma: no cover
        raise Unsupported("fragment spread at the root")
    return f.alias.value if f.alias else f.name.value
