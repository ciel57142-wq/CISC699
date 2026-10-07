"""Parser + validator (HS2 §4.2). graphql-core does the spec validation; this module adds
the MVA restrictions and rejects anything outside them with UNSUPPORTED_OPERATION.

MVA (HS1 §6.4): one named-or-anonymous query operation, one root field, selection depth
up to two relationship hops, at most one cross-store traversal, conjunctive comparison
predicates, limit/offset, single-level aggregation, named fragments.
Rejected: mutations/subscriptions, inline fragments, @skip/@include, introspection fields.
"""

from __future__ import annotations

from graphql import (
    DocumentNode,
    FieldNode,
    GraphQLError,
    GraphQLSchema,
    InlineFragmentNode,
    OperationDefinitionNode,
    OperationType,
    Visitor,
    parse,
    validate,
    visit,
)

from engine.errors import GRAPHQL_VALIDATION_FAILED, EngineError, Unsupported


class _MvaVisitor(Visitor):
    def __init__(self):
        super().__init__()
        self.problem: str | None = None

    def enter_inline_fragment(self, node: InlineFragmentNode, *_):
        self.problem = self.problem or "inline fragments are outside the MVA subset"

    def enter_field(self, node: FieldNode, *_):
        if node.directives:
            self.problem = self.problem or f"directive @{node.directives[0].name.value} on a field"
        if node.name.value.startswith("__"):
            self.problem = self.problem or f"introspection field {node.name.value}"


def parse_and_validate(schema: GraphQLSchema, text: str) -> DocumentNode:
    try:
        doc = parse(text)
    except GraphQLError as e:
        raise EngineError(GRAPHQL_VALIDATION_FAILED, e.message) from e
    ops = [d for d in doc.definitions if isinstance(d, OperationDefinitionNode)]
    if len(ops) != 1:
        raise Unsupported(f"exactly one operation per document (got {len(ops)})")
    if ops[0].operation != OperationType.QUERY:
        raise Unsupported(f"{ops[0].operation.value} operations: the engine is read-only")
    v = _MvaVisitor()
    visit(doc, v)
    if v.problem:
        raise Unsupported(v.problem)
    errors = validate(schema, doc)
    if errors:
        raise EngineError(GRAPHQL_VALIDATION_FAILED, "; ".join(e.message for e in errors))
    return doc
