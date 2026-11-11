#!/usr/bin/env python3
"""Compatibility check with real client libraries.

Start a server, then run this script:

    polydb serve --memory &
    pip install redis psycopg2-binary pymongo
    python3 tests/drivers.py

Each section talks to polydb through an unmodified third-party driver, and
the last section checks that all three see the same underlying data.
"""
import sys
import threading

HOST = "127.0.0.1"
REDIS_PORT, PG_PORT, MONGO_PORT = 6380, 5433, 27018


def check(label, got, want):
    if got != want:
        print(f"FAIL {label}: got {got!r}, wanted {want!r}")
        sys.exit(1)
    print(f"ok   {label}")


# ------------------------------------------------------------------ Redis
import redis

r = redis.Redis(host=HOST, port=REDIS_PORT)
check("redis ping", r.ping(), True)
r.set("greeting", "hello")
check("redis get", r.get("greeting"), b"hello")
r.set("blob", bytes([0, 255, 10, 13]))
check("redis binary value", r.get("blob"), bytes([0, 255, 10, 13]))
check("redis missing key", r.get("nope"), None)
r.delete("hits")
check("redis incr", [r.incr("hits"), r.incrby("hits", 10), r.decr("hits")], [1, 11, 10])
r.mset({"user:1": "a", "user:2": "b"})
check("redis mget", r.mget("user:1", "user:2", "user:3"), [b"a", b"b", None])
check("redis keys glob", sorted(r.keys("user:*")), [b"user:1", b"user:2"])
check("redis exists/del", [r.exists("user:1"), r.delete("user:1", "user:2"), r.exists("user:1")], [1, 2, 0])
check("redis setnx", [r.setnx("once", "1"), r.setnx("once", "2"), r.get("once")], [True, False, b"1"])
try:
    r.execute_command("LPUSH", "list", "x")
    check("redis unsupported command", "no error", "error")
except redis.ResponseError:
    print("ok   redis unsupported command is a clean error")

# concurrent INCR from many connections must not lose updates
r.delete("race")
def bump():
    c = redis.Redis(host=HOST, port=REDIS_PORT)
    for _ in range(200):
        c.incr("race")
threads = [threading.Thread(target=bump) for _ in range(8)]
[t.start() for t in threads]
[t.join() for t in threads]
check("redis 8 clients x 200 INCR", r.get("race"), b"1600")

# ------------------------------------------------------------- PostgreSQL
import psycopg2

pg = psycopg2.connect(host=HOST, port=PG_PORT, user="app", dbname="polydb")
cur = pg.cursor()
cur.execute("DROP TABLE IF EXISTS users")
cur.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, score REAL, active BOOL)")
cur.execute("INSERT INTO users VALUES (%s, %s, %s, %s, %s)", (1, "Ada", 36, 9.5, True))
cur.executemany("INSERT INTO users VALUES (%s, %s, %s, %s, %s)", [(2, "Linus", 54, 7.25, False), (3, "O'Brien", None, None, True)])
pg.commit()
cur.execute("SELECT id, name, age, score, active FROM users ORDER BY id")
check("postgres typed rows", cur.fetchall(), [(1, "Ada", 36, 9.5, True), (2, "Linus", 54, 7.25, False), (3, "O'Brien", None, None, True)])
check("postgres column names", [d[0] for d in cur.description], ["id", "name", "age", "score", "active"])
cur.execute("SELECT COUNT(*), MAX(age) FROM users WHERE name LIKE %s", ("%i%",))
check("postgres aggregate", cur.fetchone(), (2, 54))
cur.execute("UPDATE users SET age = age + 1 WHERE id = 1")
check("postgres rowcount", cur.rowcount, 1)
pg.rollback()
cur.execute("SELECT age FROM users WHERE id = 1")
check("postgres rollback", cur.fetchone(), (36,))
pg.commit()
try:
    cur.execute("INSERT INTO users VALUES (1, 'dup', 1, 1, true)")
    check("postgres duplicate key", "no error", "error")
except psycopg2.errors.UniqueViolation:
    print("ok   postgres duplicate key raises UniqueViolation")
pg.rollback()
try:
    cur.execute("SELECT * FROM missing_table")
    check("postgres missing table", "no error", "error")
except psycopg2.errors.UndefinedTable:
    print("ok   postgres missing table raises UndefinedTable")
pg.rollback()

# two connections selling the last item: one must be told to retry
cur.execute("DROP TABLE IF EXISTS stock")
cur.execute("CREATE TABLE stock (id INT PRIMARY KEY, qty INT)")
cur.execute("INSERT INTO stock VALUES (1, 1)")
pg.commit()
pg2 = psycopg2.connect(host=HOST, port=PG_PORT, user="app", dbname="polydb")
cur2 = pg2.cursor()
cur.execute("SELECT qty FROM stock WHERE id = 1"); cur.fetchall()
cur2.execute("SELECT qty FROM stock WHERE id = 1"); cur2.fetchall()
cur.execute("UPDATE stock SET qty = qty - 1 WHERE id = 1")
cur2.execute("UPDATE stock SET qty = qty - 1 WHERE id = 1")
pg.commit()
try:
    pg2.commit()
    check("postgres write conflict", "no error", "error")
except psycopg2.errors.SerializationFailure:
    print("ok   postgres conflicting commit raises SerializationFailure")
pg2.rollback()
cur.execute("SELECT qty FROM stock")
check("postgres stock never oversold", cur.fetchone(), (0,))
pg.commit()

# --------------------------------------------------------------- MongoDB
import pymongo
from bson import ObjectId

mongo = pymongo.MongoClient(f"mongodb://{HOST}:{MONGO_PORT}/?directConnection=true", serverSelectionTimeoutMS=3000)
check("mongo ping", mongo.admin.command("ping")["ok"], 1.0)
people = mongo.app.people
people.drop()
res = people.insert_one({"name": "Ada", "age": 36, "tags": ["math", "code"], "addr": {"city": "London"}})
check("mongo insert_one returns ObjectId", isinstance(res.inserted_id, ObjectId), True)
people.insert_many([{"_id": 2, "name": "Linus", "age": 54}, {"_id": 3, "name": "Grace", "age": 85, "score": 1.5}])
doc = people.find_one({"_id": res.inserted_id})
check("mongo find by ObjectId", (doc["_id"], doc["name"], doc["tags"], doc["addr"]), (res.inserted_id, "Ada", ["math", "code"], {"city": "London"}))
check("mongo count_documents", people.count_documents({"age": {"$gt": 40}}), 2)
check("mongo sort/limit", [d["name"] for d in people.find({}).sort("age", -1).limit(2)], ["Grace", "Linus"])
check("mongo nested + array filters", [people.count_documents({"addr.city": "London"}), people.count_documents({"tags": "code"})], [1, 1])
check("mongo projection", people.find_one({"_id": 3}, {"name": 1, "_id": 0}), {"name": "Grace"})
check("mongo float round trip", people.find_one({"_id": 3})["score"], 1.5)
upd = people.update_one({"_id": 2}, {"$set": {"city": "Portland"}, "$inc": {"age": 1}})
check("mongo update_one", (upd.matched_count, people.find_one({"_id": 2})["age"]), (1, 55))
check("mongo update_many", people.update_many({}, {"$set": {"seen": True}}).modified_count, 3)
up = people.update_one({"name": "Alan"}, {"$set": {"age": 41}}, upsert=True)
check("mongo upsert", (up.upserted_id is not None, people.find_one({"name": "Alan"})["age"]), (True, 41))
check("mongo distinct", sorted(people.distinct("name")), ["Ada", "Alan", "Grace", "Linus"])
people.create_index("name")
check("mongo indexed lookup", people.find_one({"name": "Grace"})["_id"], 3)
try:
    people.insert_one({"_id": 2})
    check("mongo duplicate _id", "no error", "error")
except pymongo.errors.DuplicateKeyError:
    print("ok   mongo duplicate _id raises DuplicateKeyError")
check("mongo delete_one / delete_many", [people.delete_one({"_id": 3}).deleted_count, people.delete_many({"age": {"$lt": 50}}).deleted_count], [1, 2])
check("mongo list collections", "people" in mongo.app.list_collection_names(), True)

# ----------------------------------------------- one database, three views
cur.execute("db.people.find({_id: 2})")
check("document written by pymongo, read through postgres", '"name":"Linus"' in cur.fetchone()[0], True)
cur.execute("GET greeting")
check("key written by redis, read through postgres", cur.fetchone(), ("hello",))
cur.execute("BEGIN")
cur.execute("SET audit:last 'cross-model'")
cur.execute("db.audit.insert({_id: 1, via: 'postgres'})")
cur.execute("INSERT INTO users VALUES (10, 'Txn', 1, 1, true)")
pg.commit()
check("one transaction, visible from redis and mongo", (r.get("audit:last"), mongo.app.audit.find_one({"_id": 1})["via"]), (b"cross-model", "postgres"))

print("\nall driver checks passed")
