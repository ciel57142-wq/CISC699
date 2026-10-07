"""Schema loader and placement resolver (HS2 §4.1, §6.1, ADR-0002/0003/0004).

Input:  the developer's SDL (types annotated with @store / @ref) and backends.toml.
Output: a PlacementMap (where every type lives, how types join) and the augmented
        GraphQLSchema with derived root fields and where/orderBy inputs.

The resolver refuses to start (SCHEMA_INVALID) on: an unknown backend, an object type
without @store that is not a valid embedded type, an @ref naming a missing field, a key
type mismatch, a user-written Query type, or a scalar outside the supported set.
"""

from __future__ import annotations

import os
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

from graphql import (
    GraphQLError,
    GraphQLList,
    GraphQLNonNull,
    GraphQLObjectType,
    GraphQLScalarType,
    GraphQLSchema,
    build_schema,
)
from graphql.execution.values import get_directive_values

from engine.errors import SchemaInvalid

DIRECTIVES_SDL = """
directive @store(backend: String!, collection: String) on OBJECT
directive @ref(localKey: String!, remoteKey: String!) on FIELD_DEFINITION
scalar DateTime
scalar Long
"""

SCALARS = ("ID", "Int", "String", "DateTime", "Long")
KEY_CLASS = {"ID": "int", "Int": "int", "Long": "int", "String": "str", "DateTime": "ts"}
BACKEND_KINDS = {"postgres": "relational", "mongodb": "document"}
NUMERIC = ("Int", "Long")


@dataclass(frozen=True)
class RefInfo:
    target: str
    local_key: str
    remote_key: str
    many: bool


@dataclass(frozen=True)
class FieldInfo:
    name: str
    kind: str            # "scalar" | "ref" | "embedded"
    type_name: str       # named (unwrapped) type
    is_list: bool
    non_null: bool
    ref: RefInfo | None = None


@dataclass(frozen=True)
class TypeInfo:
    name: str
    backend: str | None          # None for embedded types
    collection: str | None
    fields: dict[str, FieldInfo]
    plural: str | None = None

    def scalar_fields(self) -> list[str]:
        return [f.name for f in self.fields.values() if f.kind == "scalar"]


@dataclass(frozen=True)
class BackendInfo:
    name: str
    kind: str                    # "postgres" | "mongodb"
    params: dict = field(default_factory=dict)

    @property
    def model(self) -> str:
        return BACKEND_KINDS[self.kind]


@dataclass
class PlacementMap:
    types: dict[str, TypeInfo]
    backends: dict[str, BackendInfo]
    roots: dict[str, tuple[str, str]]    # root field -> (type, "list" | "aggregate")

    def crosses(self, owner: str, field_name: str) -> bool:
        f = self.types[owner].fields[field_name]
        return f.kind == "ref" and self.types[f.ref.target].backend != self.types[owner].backend

    def numeric_measure_fields(self, type_name: str) -> list[str]:
        """Int/Long fields that are not identifiers or join keys: valid sum/avg/min/max targets."""
        keys = {"id"}
        for t in self.types.values():
            for f in t.fields.values():
                if f.ref:
                    if t.name == type_name:
                        keys.add(f.ref.local_key)
                    if f.ref.target == type_name:
                        keys.add(f.ref.remote_key)
        t = self.types[type_name]
        return [f.name for f in t.fields.values()
                if f.kind == "scalar" and f.type_name in NUMERIC and f.name not in keys]


def plural(type_name: str) -> str:
    return type_name[0].lower() + type_name[1:] + "s"


def _default_collection(type_name: str) -> str:
    return type_name.lower()


def _unwrap(t) -> tuple[str, bool, bool]:
    non_null = isinstance(t, GraphQLNonNull)
    if non_null:
        t = t.of_type
    is_list = isinstance(t, GraphQLList)
    if is_list:
        t = t.of_type
        if isinstance(t, GraphQLNonNull):
            t = t.of_type
        if isinstance(t, GraphQLList):
            raise SchemaInvalid("nested list types are not supported")
    return t.name, is_list, non_null


def load_backends(path: Path | str) -> dict[str, BackendInfo]:
    raw = tomllib.loads(Path(path).read_text())
    out = {}
    for name, cfg in raw.get("backends", {}).items():
        kind = cfg.get("kind")
        if kind not in BACKEND_KINDS:
            raise SchemaInvalid(f"backend {name!r}: unknown kind {kind!r}")
        out[name] = BackendInfo(name, kind, {k: v for k, v in cfg.items() if k != "kind"})
    if not out:
        raise SchemaInvalid("backends.toml defines no backends")
    return out


def resolve(sdl: str, backends: dict[str, BackendInfo]) -> PlacementMap:
    try:
        base = build_schema(DIRECTIVES_SDL + sdl)
    except (GraphQLError, TypeError) as e:
        raise SchemaInvalid(f"SDL does not parse: {e}") from e
    if base.query_type is not None or base.mutation_type is not None:
        raise SchemaInvalid("define no Query or Mutation type; root fields are derived (ADR-0004)")

    store_d, ref_d = base.get_directive("store"), base.get_directive("ref")
    objects = {n: t for n, t in base.type_map.items()
               if isinstance(t, GraphQLObjectType) and not n.startswith("__")}
    for n, t in base.type_map.items():
        if n.startswith("__") or n in SCALARS or n in ("String", "Boolean", "Int", "Float", "ID"):
            continue
        if isinstance(t, GraphQLScalarType) or not isinstance(t, GraphQLObjectType):
            raise SchemaInvalid(f"type {n}: only object types and the scalars {SCALARS} are supported")

    stores = {n: get_directive_values(store_d, t.ast_node) for n, t in objects.items()}
    entities = {n for n, s in stores.items() if s}

    types: dict[str, TypeInfo] = {}
    seen_collections: dict[tuple[str, str], str] = {}
    for n, t in objects.items():
        s = stores[n]
        if s:
            if s["backend"] not in backends:
                raise SchemaInvalid(f"type {n}: @store backend {s['backend']!r} is not in backends.toml")
            coll = s.get("collection") or _default_collection(n)
            if (s["backend"], coll) in seen_collections:
                raise SchemaInvalid(f"type {n}: collection {coll!r} on {s['backend']!r} "
                                    f"is already used by {seen_collections[(s['backend'], coll)]}")
            seen_collections[(s["backend"], coll)] = n
        fields: dict[str, FieldInfo] = {}
        for fname, f in t.fields.items():
            tname, is_list, nn = _unwrap(f.type)
            rv = get_directive_values(ref_d, f.ast_node)
            if rv:
                if tname not in entities:
                    raise SchemaInvalid(f"{n}.{fname}: @ref target {tname} has no @store")
                fields[fname] = FieldInfo(fname, "ref", tname, is_list, nn,
                                          RefInfo(tname, rv["localKey"], rv["remoteKey"], is_list))
            elif tname in SCALARS:
                if is_list:
                    raise SchemaInvalid(f"{n}.{fname}: list-of-scalar fields are not supported")
                fields[fname] = FieldInfo(fname, "scalar", tname, False, nn)
            elif tname in entities:
                raise SchemaInvalid(f"{n}.{fname}: field of entity type {tname} needs @ref")
            elif tname in objects:
                fields[fname] = FieldInfo(fname, "embedded", tname, is_list, nn)
            else:
                raise SchemaInvalid(f"{n}.{fname}: unsupported type {tname}")
        types[n] = TypeInfo(n, s["backend"] if s else None,
                            (s.get("collection") or _default_collection(n)) if s else None,
                            fields, plural(n) if s else None)

    # Entities need an integer id; embedded types must be used only inside document types.
    for n in entities:
        idf = types[n].fields.get("id")
        if not idf or idf.type_name != "ID" or not idf.non_null:
            raise SchemaInvalid(f"type {n}: an entity needs `id: ID!`")
    embedded_users: dict[str, list[str]] = {n: [] for n in objects if n not in entities}
    for t in types.values():
        for f in t.fields.values():
            if f.kind == "embedded":
                embedded_users[f.type_name].append(t.name)
    for emb, users in embedded_users.items():
        if not users:
            raise SchemaInvalid(f"type {emb} has no @store and is not embedded anywhere")
        for u in users:
            ut = types[u]
            if ut.backend is None:
                raise SchemaInvalid(f"embedded type {emb} nested in embedded type {u}: depth > 1")
            if backends[ut.backend].model != "document":
                raise SchemaInvalid(f"type {emb} has no @store; it can only be embedded in a "
                                    f"document-store type, but {u} is on {ut.backend!r}")
        if any(f.kind != "scalar" for f in types[emb].fields.values()):
            raise SchemaInvalid(f"embedded type {emb} may only contain scalar fields")

    # @ref keys must exist, be scalar, and have the same key class.
    for t in types.values():
        for f in t.fields.values():
            if f.kind != "ref":
                continue
            r, tgt = f.ref, types[f.ref.target]
            lk, rk = t.fields.get(r.local_key), tgt.fields.get(r.remote_key)
            if lk is None or lk.kind != "scalar":
                raise SchemaInvalid(f"{t.name}.{f.name}: localKey {r.local_key!r} is not a scalar field of {t.name}")
            if rk is None or rk.kind != "scalar":
                raise SchemaInvalid(f"{t.name}.{f.name}: remoteKey {r.remote_key!r} is not a scalar field of {tgt.name}")
            if KEY_CLASS[lk.type_name] != KEY_CLASS[rk.type_name]:
                raise SchemaInvalid(f"{t.name}.{f.name}: key type mismatch "
                                    f"{t.name}.{lk.name}: {lk.type_name} vs {tgt.name}.{rk.name}: {rk.type_name}")
            if not r.many and r.remote_key != "id":
                raise SchemaInvalid(f"{t.name}.{f.name}: a to-one @ref must point at the target's id")

    roots = {}
    for n in sorted(entities):
        roots[plural(n)] = (n, "list")
        roots[plural(n) + "Aggregate"] = (n, "aggregate")
    return PlacementMap(types, backends, roots)


# ---------------------------------------------------------------- augmented schema

_OPS = ("eq", "lt", "gt", "lte", "gte")


def augmented_sdl(pm: PlacementMap) -> str:
    out = ["scalar DateTime", "scalar Long", "enum SortDir { ASC DESC }"]
    for s in SCALARS:
        ops = " ".join(f"{o}: {s}" for o in _OPS)
        out.append(f"input {s}Filter {{ {ops} in: [{s}!] }}")
    for t in pm.types.values():
        if t.backend is None:
            out.append(f"type {t.name} {{ " + " ".join(
                f"{f.name}: {f.type_name}{'!' if f.non_null else ''}" for f in t.fields.values()) + " }")
            continue
        sc = t.scalar_fields()
        out.append(f"enum {t.name}Field {{ {' '.join(sc)} }}")
        out.append(f"input {t.name}Where {{ " + " ".join(
            f"{f}: {t.fields[f].type_name}Filter" for f in sc) + " }")
        out.append(f"input {t.name}OrderBy {{ field: {t.name}Field! dir: SortDir = ASC }}")
        body = []
        for f in t.fields.values():
            if f.kind == "scalar":
                body.append(f"{f.name}: {f.type_name}{'!' if f.non_null else ''}")
            elif f.kind == "embedded":
                body.append(f"{f.name}: [{f.type_name}!]!" if f.is_list else f"{f.name}: {f.type_name}")
            elif f.ref.many:
                tg = f.ref.target
                body.append(f"{f.name}(where: {tg}Where, orderBy: [{tg}OrderBy!], limit: Int): [{tg}!]!")
            else:
                body.append(f"{f.name}: {f.ref.target}{'!' if f.non_null else ''}")
        out.append(f"type {t.name} {{ " + " ".join(body) + " }")
        # aggregate row
        meas = pm.numeric_measure_fields(t.name)
        out.append(f"type {t.name}GroupKey {{ " + " ".join(
            f"{f}: {t.fields[f].type_name}" for f in sc) + " }")
        agg = [f"key: {t.name}GroupKey!", "count: Int!"]
        if meas:
            out.append(f"type {t.name}Sums {{ " + " ".join(f"{m}: Long" for m in meas) + " }")
            out.append(f"type {t.name}Avgs {{ " + " ".join(f"{m}: Float" for m in meas) + " }")
            out.append(f"type {t.name}Extremes {{ " + " ".join(
                f"{m}: {t.fields[m].type_name}" for m in meas) + " }")
            agg += [f"sum: {t.name}Sums", f"avg: {t.name}Avgs",
                    f"min: {t.name}Extremes", f"max: {t.name}Extremes"]
        out.append(f"type {t.name}AggregateRow {{ " + " ".join(agg) + " }")
    q = []
    for root, (tn, kind) in pm.roots.items():
        if kind == "list":
            q.append(f"{root}(where: {tn}Where, orderBy: [{tn}OrderBy!], limit: Int, offset: Int): [{tn}!]!")
        else:
            q.append(f"{root}(where: {tn}Where, groupBy: [{tn}Field!]): [{tn}AggregateRow!]!")
    out.append("type Query { " + " ".join(q) + " }")
    return "\n".join(out) + "\n"


def load(schema_path: Path | str, backends_path: Path | str) -> tuple[GraphQLSchema, PlacementMap]:
    backends = load_backends(backends_path)
    pm = resolve(Path(schema_path).read_text(), backends)
    return build_schema(augmented_sdl(pm)), pm


def backend_params(b: BackendInfo) -> dict:
    """Resolve *_env indirections to actual values at connect time."""
    out = {}
    for k, v in b.params.items():
        if k.endswith("_env"):
            out[k[:-4]] = os.getenv(v)
        else:
            out[k] = v
    return out
