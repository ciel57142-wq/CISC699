# ADR-0008: Adapters declare capabilities; rules consult them at plan time

**Status:** Implemented at HS3 (from ISSUE-010) · **Date:** 2026-10-07

**Context.** HS2 §4.6 says an unsupported fragment feature must raise at plan time, never at run
time. The first oracle run showed a backend (FerretDB) that rejects `$avg` in `$group` at run time.

**Decision.** Each adapter exposes `capabilities` after connecting (today: the set of aggregate
functions it can push). `rewrite()` receives them; R1b pushes an aggregate only if every measure is
supported, otherwise leaves it residual (always correct). Driver errors other than connection
failures map to `INTERNAL` (a planner bug), not `BACKEND_UNAVAILABLE`.

**Consequence.** The physical plan can differ between environments with different backends. The
manifest records the server identity of each backend so this is visible in every run.
