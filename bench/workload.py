"""Workload templates W1-W10 (HS2 §10.2) with their reference SQL for the oracle.

Each template: the GraphQL operation the engine runs, default variables, the reference SQL
run on pg-cons (it returns one JSON value shaped exactly like the engine's response), and
whether the root list order is part of the answer.

W7 and W10 are held out (HS2 §11.2): they must pass the oracle, but they are never timed
before M8 and were not used while writing the rewrite rules.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime, timezone


def TS(col: str) -> str:
    return f"""to_char({col} AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"""


LINES = ("coalesce((SELECT json_agg(json_build_object('lineNo', l.line_no, 'productId', l.product_id, "
         "'quantity', l.quantity, 'priceCents', l.price_cents) ORDER BY l.line_no) "
         "FROM order_line l WHERE l.order_id = o.id), '[]'::json)")


@dataclass(frozen=True)
class Template:
    id: str
    shape: str
    stores: str
    query: str
    reference_sql: str
    variables: dict = field(default_factory=dict)
    ordered: bool = False
    held_out: bool = False


def _dt(s: str) -> datetime:
    return datetime.fromisoformat(s.replace("Z", "+00:00")).astimezone(timezone.utc)


WORKLOAD: dict[str, Template] = {t.id: t for t in [
    Template(
        "W1", "Filter + sort + limit on Customer", "PG",
        """query W1($country: String!, $lo: Int!) {
          customers(where: {country: {eq: $country}, sel: {gte: $lo}},
                    orderBy: [{field: sel, dir: DESC}], limit: 10) {
            id email country sel createdAt } }""",
        f"""SELECT coalesce(json_agg(j ORDER BY rn), '[]'::json) FROM (
              SELECT row_number() OVER (ORDER BY sel DESC, id) rn,
                     json_build_object('id', id::text, 'email', email, 'country', country,
                                       'sel', sel, 'createdAt', {TS('created_at')}) j
              FROM customer WHERE country = %(country)s AND sel >= %(lo)s
              ORDER BY sel DESC, id LIMIT 10) s""",
        {"country": "DE", "lo": 3000}, ordered=True),
    Template(
        "W2", "Filter on Order totals and dates", "Mongo",
        """query W2($min: Int!, $from: DateTime!, $to: DateTime!) {
          orders(where: {totalCents: {gte: $min}, orderedAt: {gte: $from, lt: $to}}) {
            id customerId totalCents orderedAt } }""",
        f"""SELECT coalesce(json_agg(json_build_object('id', id::text, 'customerId', customer_id,
                     'totalCents', total_cents, 'orderedAt', {TS('ordered_at')})), '[]'::json)
            FROM orders WHERE total_cents >= %(min)s
              AND ordered_at >= %(from)s AND ordered_at < %(to)s""",
        {"min": 200000, "from": "2025-01-01T00:00:00.000Z", "to": "2025-07-01T00:00:00.000Z"}),
    Template(
        "W3", "Aggregate Order totals grouped by customer", "Mongo",
        """query W3($from: DateTime!) {
          ordersAggregate(where: {orderedAt: {gte: $from}}, groupBy: [customerId]) {
            key { customerId } count sum { totalCents } avg { totalCents } max { totalCents } } }""",
        """SELECT coalesce(json_agg(j), '[]'::json) FROM (
              SELECT json_build_object('key', json_build_object('customerId', customer_id),
                     'count', count(*), 'sum', json_build_object('totalCents', sum(total_cents)),
                     'avg', json_build_object('totalCents', avg(total_cents)::float8),
                     'max', json_build_object('totalCents', max(total_cents))) j
              FROM orders WHERE ordered_at >= %(from)s GROUP BY customer_id) s""",
        {"from": "2025-01-01T00:00:00.000Z"}),
    Template(
        "W4", "Aggregate Product prices grouped by vendor", "PG",
        """query W4 { productsAggregate(groupBy: [vendorId]) {
            key { vendorId } count avg { priceCents } min { priceCents } max { priceCents } } }""",
        """SELECT coalesce(json_agg(j), '[]'::json) FROM (
              SELECT json_build_object('key', json_build_object('vendorId', vendor_id), 'count', count(*),
                     'avg', json_build_object('priceCents', avg(price_cents)::float8),
                     'min', json_build_object('priceCents', min(price_cents)),
                     'max', json_build_object('priceCents', max(price_cents))) j
              FROM product GROUP BY vendor_id) s"""),
    Template(
        "W5", "Customer (filtered by sel < t) -> orders", "PG -> Mongo",
        """query W5($t: Int!) {
          customers(where: {sel: {lt: $t}}) { id email orders { id totalCents orderedAt } } }""",
        f"""SELECT coalesce(json_agg(json_build_object('id', c.id::text, 'email', c.email, 'orders',
              coalesce((SELECT json_agg(json_build_object('id', o.id::text, 'totalCents', o.total_cents,
                        'orderedAt', {TS('o.ordered_at')})) FROM orders o WHERE o.customer_id = c.id),
                       '[]'::json))), '[]'::json)
            FROM customer c WHERE c.sel < %(t)s""",
        {"t": 1000}),
    Template(
        "W6", "Order (filtered) -> customer, to-one", "Mongo -> PG",
        """query W6($min: Int!) {
          orders(where: {totalCents: {gte: $min}}) { id totalCents customer { id email country } } }""",
        """SELECT coalesce(json_agg(json_build_object('id', o.id::text, 'totalCents', o.total_cents,
              'customer', (SELECT json_build_object('id', c.id::text, 'email', c.email, 'country', c.country)
                           FROM customer c WHERE c.id = o.customer_id))), '[]'::json)
            FROM orders o WHERE o.total_cents >= %(min)s""",
        {"min": 900000}),
    Template(
        "W7", "Product -> feedback with rating filter", "PG -> Mongo",
        """query W7($p: Int!, $r: Int!) {
          products(where: {priceCents: {lt: $p}}) {
            id title priceCents feedback(where: {rating: {gte: $r}}) { id rating text postedAt } } }""",
        f"""SELECT coalesce(json_agg(json_build_object('id', p.id::text, 'title', p.title,
              'priceCents', p.price_cents, 'feedback',
              coalesce((SELECT json_agg(json_build_object('id', f.id::text, 'rating', f.rating,
                         'text', f.text, 'postedAt', {TS('f.posted_at')}))
                        FROM feedback f WHERE f.product_id = p.id AND f.rating >= %(r)s), '[]'::json))),
              '[]'::json)
            FROM product p WHERE p.price_cents < %(p)s""",
        {"p": 20000, "r": 4}, held_out=True),
    Template(
        "W8", "Vendor -> products, same store, depth 2", "PG",
        """query W8 { vendors { id name
            products(orderBy: [{field: priceCents, dir: DESC}]) { id title priceCents } } }""",
        """SELECT coalesce(json_agg(json_build_object('id', v.id::text, 'name', v.name, 'products',
              coalesce((SELECT json_agg(json_build_object('id', p.id::text, 'title', p.title,
                         'priceCents', p.price_cents) ORDER BY p.price_cents DESC, p.id)
                        FROM product p WHERE p.vendor_id = v.id), '[]'::json))), '[]'::json)
            FROM vendor v"""),
    Template(
        "W9", "Root limit/offset with nested per-parent limit", "PG -> Mongo",
        """query W9 { customers(orderBy: [{field: sel}], limit: 5, offset: 2) { id sel
            orders(orderBy: [{field: totalCents, dir: DESC}], limit: 3) { id totalCents } } }""",
        """SELECT coalesce(json_agg(j ORDER BY rn), '[]'::json) FROM (
              SELECT row_number() OVER (ORDER BY c.sel, c.id) rn,
                     json_build_object('id', c.id::text, 'sel', c.sel, 'orders',
                       coalesce((SELECT json_agg(json_build_object('id', o.id::text, 'totalCents', o.total_cents)
                                   ORDER BY o.total_cents DESC, o.id)
                                 FROM (SELECT * FROM orders o WHERE o.customer_id = c.id
                                       ORDER BY o.total_cents DESC, o.id LIMIT 3) o), '[]'::json)) j
              FROM customer c ORDER BY c.sel, c.id LIMIT 5 OFFSET 2) s""",
        ordered=True),
    Template(
        "W10", "Named fragments over W5's shape", "PG -> Mongo",
        """query W10($t: Int!) { customers(where: {sel: {lt: $t}}) { ...C orders { ...O } } }
           fragment C on Customer { id email country }
           fragment O on Order { id totalCents lines { lineNo productId quantity priceCents } }""",
        f"""SELECT coalesce(json_agg(json_build_object('id', c.id::text, 'email', c.email,
              'country', c.country, 'orders',
              coalesce((SELECT json_agg(json_build_object('id', o.id::text, 'totalCents', o.total_cents,
                        'lines', {LINES})) FROM orders o WHERE o.customer_id = c.id), '[]'::json))),
              '[]'::json)
            FROM customer c WHERE c.sel < %(t)s""",
        {"t": 500}, held_out=True),
]}


def sql_params(variables: dict) -> dict:
    """GraphQL variables -> psycopg parameters (DateTime strings become datetimes)."""
    return {k: (_dt(v) if isinstance(v, str) and v.endswith("Z") and "T" in v else v)
            for k, v in variables.items()}
