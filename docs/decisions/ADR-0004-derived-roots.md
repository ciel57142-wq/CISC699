# ADR-0004: Derived root fields and inputs

**Status:** Implemented at HS3; pending supervisor acceptance · **Date:** 2026-10-07

**Decision.** For every `@store` type `T` the resolver derives, and the developer writes none of:
`ts(where: TWhere, orderBy: [TOrderBy!], limit: Int, offset: Int): [T!]!` and
`tsAggregate(where: TWhere, groupBy: [TField!]): [TAggregateRow!]!`, where `ts` is `T` with a
lower-case first letter plus `s` (so `Feedback` becomes `feedbacks`, which is ugly but predictable).
`TWhere` has one `<Scalar>Filter {eq lt gt lte gte in}` per scalar field; conjunction only.
To-many `@ref` fields get `(where, orderBy, limit)`. Aggregate measures (`sum` as Long, `avg` as
Float, `min`/`max`) are offered on Int/Long fields that are not `id` and not join keys.

**Totality.** When an ordering, limit or offset is requested, `id ASC` is appended as a final sort
key so every ordered result is a total order. R4 relies on this.
