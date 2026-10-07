# ADR-0005: Seeded generator on the UniBench schema

**Status:** Implemented at HS3 (amendment H2-A2) · **Date:** 2026-10-07

**Decision.** `bench/data/gen.py`, numpy PCG64. Five entities; `sel` uniform 0..9,999 on
customers; orders assigned to customers by a bounded Zipf over ranks (exponent 1.0) with ranks
randomly permuted so fan-out is independent of `sel`; 1–5 embedded order lines; integer cents;
millisecond UTC timestamps; synthetic vocabulary for all text; nothing null or omitted.

**Evidence.** T-D1: two runs, same seed, byte-identical files (`make data`). T-D2: generated,
federated and consolidated checksums identical at S = 0.01 and S = 0.1. Zipf exponent recovered by
MLE within 1.1% at S ≥ 0.1 for seeds 1–3 (the 5% check in HS2 §11.1 is therefore meaningful).

**Open.** Entity ratios are provisional (ISSUE-006).
