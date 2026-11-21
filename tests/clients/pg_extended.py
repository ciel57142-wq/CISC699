"""Prepared statements through real extended-protocol PostgreSQL clients.

Run against `polydb serve --memory --port off` (PostgreSQL on 5433):
    python3 tests/clients/pg_extended.py
Uses psycopg 3 (server-side binding, prepared statements, binary results)
and pg8000 (extended protocol with typed/untyped parameters).
"""
import os, sys
import psycopg, pg8000.native

HOST, PORT = os.environ.get("PG_HOST", "127.0.0.1"), int(os.environ.get("PG_PORT", "5433"))
failures = 0

def check(name, got, want):
    global failures
    if got == want:
        print(f"ok   {name}")
    else:
        failures += 1
        print(f"FAIL {name}: got {got!r}, wanted {want!r}")

# ---------------------------------------------------------------- psycopg 3
conn = psycopg.connect(host=HOST, port=PORT, user="x", dbname="x", autocommit=True)
cur = conn.cursor()
cur.execute("CREATE TABLE IF NOT EXISTS pe (id INT PRIMARY KEY, name TEXT, score REAL, ok BOOL, born DATE)")
cur.execute("DELETE FROM pe")
for i in range(20):
    cur.execute("INSERT INTO pe VALUES (%s, %s, %s, %s, %s)", (i, f"n{i}", i / 2, i % 2 == 0, "2024-01-%02d" % (i + 1)), prepare=True)
cur.execute("SELECT COUNT(*) FROM pe WHERE score >= %s", (5.0,), prepare=True)
check("psycopg prepared count", cur.fetchone(), (10,))
cur.execute("SELECT name FROM pe WHERE id = %s", (7,), prepare=True)
check("psycopg int parameter", cur.fetchone(), ("n7",))
cur.execute("SELECT id FROM pe WHERE name = %s", ("n3",), prepare=True)
check("psycopg text parameter", cur.fetchone(), (3,))
cur.execute("SELECT id FROM pe WHERE ok = %s ORDER BY id LIMIT 3", (True,), prepare=True)
check("psycopg bool parameter", cur.fetchall(), [(0,), (2,), (4,)])
cur.execute("SELECT id FROM pe WHERE name = %s", (None,), prepare=True)
check("psycopg NULL parameter matches nothing", cur.fetchall(), [])
# repeated execution of the same prepared statement
for i in (1, 2, 3):
    cur.execute("SELECT score FROM pe WHERE id = %s", (i,), prepare=True)
    check(f"psycopg repeated execution {i}", cur.fetchone(), (i / 2,))
# binary results
bcur = conn.cursor(binary=True)
bcur.execute("SELECT id, name, score, ok FROM pe WHERE id = %s", (4,))
check("psycopg binary results", bcur.fetchone(), (4, "n4", 2.0, True))
# errors come back as exceptions and the connection keeps working
try:
    cur.execute("SELECT nosuch FROM pe WHERE id = %s", (1,), prepare=True)
    check("psycopg semantic error", "no error", "error")
except psycopg.errors.Error as e:
    check("psycopg semantic error", "no such column" in str(e), True)
try:
    cur.execute("INSERT INTO pe VALUES (%s, 'dup', 0, true, NULL)", (1,), prepare=True)
    check("psycopg constraint error", "no error", "error")
except psycopg.errors.UniqueViolation:
    print("ok   psycopg constraint error is UniqueViolation")
# transactions with parameters
conn.autocommit = False
cur.execute("INSERT INTO pe VALUES (%s, %s, 0, false, NULL)", (100, "txn"))
conn.rollback()
cur.execute("SELECT COUNT(*) FROM pe WHERE id = %s", (100,))
check("psycopg rollback with parameters", cur.fetchone(), (0,))
cur.execute("INSERT INTO pe VALUES (%s, %s, 0, false, NULL)", (100, "txn"))
conn.commit()
cur.execute("SELECT name FROM pe WHERE id = %s", (100,))
check("psycopg commit with parameters", cur.fetchone(), ("txn",))
conn.commit()
conn.autocommit = True
# PBSQL through prepared statements
cur.execute("INSERT INTO DOCUMENT.pedocs VALUES {\"k\": 1, \"v\": \"a\"}")
cur.execute("SELECT v FROM DOCUMENT.pedocs WHERE k = %s", (1,), prepare=True)
check("psycopg PBSQL document query with a parameter", cur.fetchone(), ("a",))
cur.execute("SET KV 'pe:1' = %s", ("hello",))
cur.execute("GET FROM KV %s", ("pe:1",))
check("psycopg PBSQL KV with a parameter", cur.fetchone(), ("hello",))
conn.close()

# ---------------------------------------------------------------- pg8000
c = pg8000.native.Connection("x", host=HOST, port=PORT, database="x")
check("pg8000 typed int", c.run("SELECT name FROM pe WHERE id = :i", i=5), [["n5"]])
check("pg8000 text", c.run("SELECT id FROM pe WHERE name = :n", n="n6"), [[6]])
ps = c.prepare("SELECT COUNT(*) FROM pe WHERE score < :s")
# (row 100, inserted by the transaction check above, has score 0)
check("pg8000 prepared repeated", [ps.run(s=1.0), ps.run(s=3.0)], [[[3]], [[7]]])
ps.close()
c.run("UPDATE pe SET name = :n WHERE id = :i", n="renamed", i=9)
check("pg8000 update with parameters", c.run("SELECT name FROM pe WHERE id = 9"), [["renamed"]])
try:
    c.run("SELECT name FROM pe WHERE id = :i AND name = :n", i=1)
    check("pg8000 missing parameter", "no error", "error")
except Exception as e:
    print("ok   pg8000 missing parameter is an error")
check("pg8000 connection still usable", c.run("SELECT 1"), [[1]])
c.close()

print("\nall extended-protocol checks passed" if failures == 0 else f"\n{failures} check(s) FAILED")
sys.exit(1 if failures else 0)
