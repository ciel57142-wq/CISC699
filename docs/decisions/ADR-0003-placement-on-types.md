# ADR-0003: `@store` on types, `@ref` on fields

**Status:** Implemented at HS3; pending supervisor acceptance (amendment H2-A1) · **Date:** 2026-10-07

**Decision.** `directive @store(backend: String!, collection: String) on OBJECT` and
`directive @ref(localKey: String!, remoteKey: String!) on FIELD_DEFINITION`. A field crosses a
process boundary exactly when its target type's backend differs from its owner's; the resolver
computes it (`PlacementMap.crosses`). Object types without `@store` are allowed only as embedded
types inside document-store types, may hold only scalars, and nest one level.

**Rules the resolver enforces at start-up (SCHEMA_INVALID):** unknown backend; type without
`@store` that is not a valid embedded type; entity without `id: ID!`; `@ref` key missing or not
scalar; key class mismatch (ID/Int/Long are one class, String another); to-one `@ref` not pointing at
the target's `id`; entity-typed field without `@ref`; duplicate collection on one backend; scalars
outside ID, Int, String, DateTime, Long; user-written Query type. 26 fixtures in
`tests/unit/test_resolver.py` (T-U1 threshold: 20).

**Rejected.** Field-level `@store` (duplicates placement, can disagree), a single directive carrying
join keys, placement by naming convention. See HS2 §5.2.

**Consequence.** Floats are rejected outright, which enforces HS2 §5.4 (money as integer cents) at
the schema level.
