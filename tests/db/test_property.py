"""T-P1: Hypothesis generates predicates, limits and thresholds; the engine must agree with
SQL written independently from the same parameters. Shrunk counterexamples get copied into
test_regressions.py. Examples per run: PROPERTY_EXAMPLES (default 100; HS2 target 500 in CI).
"""

import os

from hypothesis import HealthCheck, given, settings, strategies as st

from engine.config import settings as db_settings
from oracle.compare import compare

N = int(os.getenv("PROPERTY_EXAMPLES", "100"))
COUNTRIES = ["DE", "FI", "US", "JP", "BR", "ZZ"]
OPS = {"lt": "<", "gt": ">", "lte": "<=", "gte": ">=", "eq": "="}

params = st.fixed_dictionaries({
    "sel_op": st.sampled_from(list(OPS)),
    "sel": st.integers(-5, 10_005),
    "countries": st.one_of(st.none(), st.lists(st.sampled_from(COUNTRIES), min_size=1, max_size=3, unique=True)),
    "tot_op": st.one_of(st.none(), st.sampled_from(list(OPS))),
    "tot": st.integers(0, 1_000_000),
    "limit": st.one_of(st.none(), st.integers(0, 12)),
    "offset": st.one_of(st.none(), st.integers(0, 5)),
    "child_limit": st.one_of(st.none(), st.integers(0, 4)),
    "desc": st.booleans(),
})


def gql(p) -> tuple[str, dict]:
    where = [f"sel: {{{p['sel_op']}: $sel}}"]
    if p["countries"]:
        where.append("country: {in: $countries}")
    args = [f"where: {{{', '.join(where)}}}", f"orderBy: [{{field: sel, dir: {'DESC' if p['desc'] else 'ASC'}}}]"]
    if p["limit"] is not None:
        args.append(f"limit: {p['limit']}")
    if p["offset"] is not None:
        args.append(f"offset: {p['offset']}")
    cargs = ["orderBy: [{field: totalCents, dir: DESC}]"]   # nested order must be defined to compare ordered
    if p["tot_op"]:
        cargs.append(f"where: {{totalCents: {{{p['tot_op']}: {p['tot']}}}}}")
    if p["child_limit"] is not None:
        cargs.append(f"limit: {p['child_limit']}")
    child = f"orders({', '.join(cargs)})"
    decl = "$sel: Int!" + (", $countries: [String!]" if p["countries"] else "")
    q = (f"query P({decl}) {{ customers({', '.join(args)}) "
         f"{{ id sel country {child} {{ id totalCents }} }} }}")
    v = {"sel": p["sel"]}
    if p["countries"]:
        v["countries"] = p["countries"]
    return q, v


def ref_sql(p) -> tuple[str, list]:
    params, cw = [p["sel"]], [f"c.sel {OPS[p['sel_op']]} %s"]
    if p["countries"]:
        cw.append('c.country = ANY(%s)')
        params.append(p["countries"])
    ow = ["o.customer_id = c.id"]
    sub_params = []
    if p["tot_op"]:
        ow.append(f"o.total_cents {OPS[p['tot_op']]} %s")
        sub_params.append(p["tot"])
    lim = " ORDER BY o.total_cents DESC, o.id"
    if p["child_limit"] is not None:
        lim += f" LIMIT {p['child_limit']}"
    d = "DESC" if p["desc"] else "ASC"
    root_lim = (f" LIMIT {p['limit']}" if p["limit"] is not None else "") + \
               (f" OFFSET {p['offset']}" if p["offset"] else "")
    sql = f"""SELECT coalesce(json_agg(j ORDER BY rn), '[]'::json) FROM (
      SELECT row_number() OVER (ORDER BY c.sel {d}, c.id) rn,
        json_build_object('id', c.id::text, 'sel', c.sel, 'country', c.country, 'orders',
          coalesce((SELECT json_agg(json_build_object('id', o.id::text, 'totalCents', o.total_cents)
                                    ORDER BY o.total_cents DESC, o.id)
                    FROM (SELECT * FROM orders o WHERE {' AND '.join(ow)}{lim}) o), '[]'::json)) j
      FROM customer c WHERE {' AND '.join(cw)} ORDER BY c.sel {d}, c.id{root_lim}) s"""
    # subquery params appear (textually) before the outer WHERE params
    return sql, sub_params + params


@settings(max_examples=N, deadline=None, suppress_health_check=[HealthCheck.function_scoped_fixture])
@given(p=params)
def test_engine_agrees_with_sql(engines, cons, p):
    q, v = gql(p)
    sql, sp = ref_sql(p)
    ref = cons.execute(sql, sp).fetchone()[0]
    for arm, eng in engines.items():
        r = eng.execute(q, v)
        assert not r.errors, r.errors
        d = compare(r.data["customers"], ref, ordered=True)   # root order is always defined
        assert d == [], f"{arm}: {d[:3]}\nquery: {q}\nvars: {v}"


def test_db_settings_point_at_separate_instances():
    s = db_settings()
    assert s.pg_fed_dsn != s.pg_cons_dsn
