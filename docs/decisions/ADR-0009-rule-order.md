# ADR-0009: Rule application order R1, R1b, R2, R4, R3

**Status:** Implemented at HS3 (from ISSUE-018) · **Date:** 2026-10-07

Projection pushdown (R3) runs last so that it can include the fields of any filter or sort that the
earlier rules left residual. Each rule is still applied exactly once and is a pure function; results
are unchanged (oracle green under all-on and all-off configurations).
