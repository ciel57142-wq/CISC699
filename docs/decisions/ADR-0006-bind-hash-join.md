# ADR-0006: Bind hash join, B = 1,000

**Status:** Implemented at HS3; **amendment pending** (ISSUE-019) · **Date:** 2026-10-07

**Decision.** The driving side runs first. Its distinct non-null join-key values are pushed into
the other side as `col = ANY(%s)` (PostgreSQL) or `$in` (MongoDB) in batches of B = 1,000. Matches
are indexed in a dict on the remote key and attached per parent; per-parent order/limit is applied
after grouping and never pushed. If the driving side returns no keys, the other backend is not
contacted.

**Evidence.** Oracle green with B ∈ {1, 2, 7, 1000} (`test_bind_join_batch_boundaries`); with B = 1
the statement count is exactly k + 1.

**Proposed amendment.** Apply R2 to every declared join, including same-store ones, because
same-store join pushdown is out of scope (HS2 §12.4) and without R2 a same-store child table is
scanned whole. Needs supervisor agreement because it widens what the A1/A4 contrast covers.
