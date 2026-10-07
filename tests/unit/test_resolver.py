"""T-U1: resolver on >= 20 valid and invalid SDL fixtures (HS2 §10.1)."""

from pathlib import Path

import pytest

from engine.errors import SchemaInvalid
from engine.schema.resolver import BackendInfo, augmented_sdl, load, resolve

REPO = Path(__file__).resolve().parents[2]
B = {"pg": BackendInfo("pg", "postgres", {}), "mongo": BackendInfo("mongo", "mongodb", {})}

BASE = """
type Customer @store(backend: "pg") { id: ID! name: String!
  orders: [Order!]! @ref(localKey: "id", remoteKey: "customerId") }
type Order @store(backend: "mongo", collection: "orders") { id: ID! customerId: Int! total: Int! }
"""

VALID = {
    "two-store base": BASE,
    "default collection name": 'type A @store(backend: "pg") { id: ID! }',
    "same-store ref": """
        type V @store(backend: "pg") { id: ID! ps: [P!]! @ref(localKey: "id", remoteKey: "vId") }
        type P @store(backend: "pg") { id: ID! vId: Int! }""",
    "to-one ref to id": """
        type O @store(backend: "mongo") { id: ID! cId: Int! c: C! @ref(localKey: "cId", remoteKey: "id") }
        type C @store(backend: "pg") { id: ID! }""",
    "embedded type in document store": """
        type O @store(backend: "mongo") { id: ID! lines: [L!]! }
        type L { n: Int! q: Int! }""",
    "DateTime and Long scalars": 'type A @store(backend: "pg") { id: ID! at: DateTime! big: Long! }',
    "string keys": """
        type A @store(backend: "pg") { id: ID! code: String! bs: [B!]! @ref(localKey: "code", remoteKey: "aCode") }
        type B @store(backend: "mongo") { id: ID! aCode: String! }""",
    "two refs between the same types": BASE.replace(
        "total: Int! }", "total: Int! c: Customer! @ref(localKey: \"customerId\", remoteKey: \"id\") }"),
    "nullable scalar field": 'type A @store(backend: "pg") { id: ID! note: String }',
    "ID to Int key class": """
        type A @store(backend: "pg") { id: ID! bs: [B!]! @ref(localKey: "id", remoteKey: "aId") }
        type B @store(backend: "pg", collection: "b2") { id: ID! aId: Long! }""",
}

INVALID = {
    "unknown backend": ('type A @store(backend: "redis") { id: ID! }', "not in backends.toml"),
    "type without @store": ('type A { id: ID! }', "not embedded anywhere"),
    "missing id": ('type A @store(backend: "pg") { name: String! }', "needs `id: ID!`"),
    "id not ID type": ('type A @store(backend: "pg") { id: Int! }', "needs `id: ID!`"),
    "ref to missing local key": (BASE.replace('localKey: "id"', 'localKey: "nope"'), "localKey 'nope'"),
    "ref to missing remote key": (BASE.replace('remoteKey: "customerId"', 'remoteKey: "cid"'), "remoteKey 'cid'"),
    "key type mismatch": (BASE.replace("customerId: Int!", "customerId: String!"), "key type mismatch"),
    "entity field without @ref": (BASE.replace(' @ref(localKey: "id", remoteKey: "customerId")', ""), "needs @ref"),
    "user-written Query type": (BASE + "type Query { x: Int }", "no Query"),
    "embedded in relational store": ('type A @store(backend: "pg") { id: ID! l: [L!]! }\ntype L { n: Int! }',
                                     "document-store"),
    "duplicate collection": ('type A @store(backend: "pg", collection: "t") { id: ID! }\n'
                             'type B @store(backend: "pg", collection: "t") { id: ID! }', "already used"),
    "Float scalar": ('type A @store(backend: "pg") { id: ID! x: Float! }', "unsupported type Float"),
    "list of scalars": ('type A @store(backend: "pg") { id: ID! tags: [String!]! }', "list-of-scalar"),
    "SDL syntax error": ('type A @store(backend: "pg") { id: ID! ', "does not parse"),
    "to-one ref not to id": ("""
        type O @store(backend: "mongo") { id: ID! cId: Int! c: C! @ref(localKey: "cId", remoteKey: "k") }
        type C @store(backend: "pg") { id: ID! k: Int! }""", "must point at the target's id"),
    "ref target has no @store": ("""
        type O @store(backend: "mongo") { id: ID! l: [L!]! @ref(localKey: "id", remoteKey: "n") }
        type L { n: Int! }""", "has no @store"),
}


@pytest.mark.parametrize("name", VALID)
def test_valid_fixture_resolves(name):
    pm = resolve(VALID[name], B)
    assert pm.roots
    augmented_sdl(pm)    # must build without error


@pytest.mark.parametrize("name", INVALID)
def test_invalid_fixture_refuses_to_start(name):
    sdl, fragment = INVALID[name]
    with pytest.raises(SchemaInvalid) as e:
        resolve(sdl, B)
    assert e.value.code == "SCHEMA_INVALID"
    assert fragment in e.value.message


def test_fixture_count_meets_threshold():
    assert len(VALID) + len(INVALID) >= 20


def test_placement_map_of_base():
    pm = resolve(BASE, B)
    assert pm.types["Customer"].backend == "pg" and pm.types["Customer"].collection == "customer"
    assert pm.types["Order"].collection == "orders"
    assert pm.crosses("Customer", "orders")
    r = pm.types["Customer"].fields["orders"].ref
    assert (r.target, r.local_key, r.remote_key, r.many) == ("Order", "id", "customerId", True)
    assert pm.roots["customers"] == ("Customer", "list")
    assert pm.roots["ordersAggregate"] == ("Order", "aggregate")
    assert pm.numeric_measure_fields("Order") == ["total"]   # customerId is a join key


def test_project_schema_loads_and_derives_roots():
    schema, pm = load(REPO / "schema.graphql", REPO / "backends.toml")
    q = schema.query_type.fields
    assert {"customers", "orders", "ordersAggregate", "productsAggregate"} <= set(q)
    assert set(q["customers"].args) == {"where", "orderBy", "limit", "offset"}
    assert set(schema.get_type("Customer").fields["orders"].args) == {"where", "orderBy", "limit"}
    assert pm.crosses("Customer", "orders") and not pm.crosses("Vendor", "products")
    assert pm.types["OrderLine"].backend is None     # embedded


def test_directive_values_read_through_graphql_core():
    """R8 / ADR-0002: graphql-core exposes schema directive arguments via ast_node."""
    from graphql import build_schema
    from graphql.execution.values import get_directive_values
    from engine.schema.resolver import DIRECTIVES_SDL
    s = build_schema(DIRECTIVES_SDL + BASE)
    v = get_directive_values(s.get_directive("store"), s.get_type("Order").ast_node)
    assert v == {"backend": "mongo", "collection": "orders"}
