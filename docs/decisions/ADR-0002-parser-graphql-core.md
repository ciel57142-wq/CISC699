# ADR-0002: Parser is graphql-core 3.2

**Status:** Implemented at HS3; pending supervisor acceptance · **Date:** 2026-10-07

**Context.** The engine needs a spec-compliant GraphQL parser and validator, and must read
schema-directive arguments (`@store`, `@ref`) from SDL (risk R8).

**Decision.** Use graphql-core 3.2.x for parsing, validation, variable coercion and argument
coercion. Execution is the engine's own; graphql-core resolvers are not used.

**Evidence.** `get_directive_values(directive, type.ast_node)` returns `{"backend": "mongo",
"collection": "orders"}` (`tests/unit/test_resolver.py::test_directive_values_read_through_graphql_core`).
R8 is closed. Version in the lock: 3.2.13.
