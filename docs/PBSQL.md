# PBSQL language reference

PBSQL is the query language of PolyDB. One statement can address any of the three storage models (relational tables, document collections, the key-value store), and a single `SELECT` can join and filter across all three. Every entry point uses it: the interactive shell, `Database::execute`, the native protocol, and the PostgreSQL wire protocol. (The Redis and MongoDB listeners call the storage engines through the typed API, not through PBSQL.)

- [How a statement is processed](#how-a-statement-is-processed)
- [Lexical rules](#lexical-rules)
- [Statements](#statements)
- [Expressions and operator precedence](#expressions-and-operator-precedence)
- [Types and type checking](#types-and-type-checking)
- [NULL semantics](#null-semantics)
- [Cross-model queries](#cross-model-queries)
- [Transactions](#transactions)
- [EXPLAIN and EXPLAIN ANALYZE](#explain-and-explain-analyze)
- [Normalization](#normalization)
- [Errors](#errors)
- [Implementation map](#implementation-map)

## How a statement is processed

```
text ─► lexer ─► parser ─► semantic analysis ─► execution
        tokens   syntax     names, types,       relational / document / key-value
        + spans  tree       shapes (catalog)    engines, through the session's
                                                transaction context
```

1. **Lexing** (`src/pbsql/lexer.rs`). One lexer for the whole language. Each token carries its byte span, so every syntax error says where it is: `(at line 3, column 13)`.
2. **Parsing** (`src/pbsql/parser.rs`, `src/sql/parser.rs`). Recursive descent, no regular expressions. The first token chooses the sub-grammar; the result is one `Statement` tree.
3. **Semantic analysis** (`src/sql/sema.rs`, `pbsql::check_document`). Runs before any data is touched, against the live catalog. It resolves names, infers and checks types, enforces where aggregates may appear, and checks result shapes and document argument shapes. A statement that can never be valid is rejected even when no row would reach the bad part (an empty table, a WHERE that matches nothing).
4. **Execution** (`src/pbsql/exec.rs`). Relational statements go through the relational engine (catalog, constraints, indexes, foreign keys), document commands through the document engine, key-value commands through the key-value module. Queries reach all three through one interface, `Relation` (see [Cross-model queries](#cross-model-queries)).

A statement runs as one atomic unit, or inside the session's open transaction. If it fails, it leaves nothing behind.

## Lexical rules

| Element | Rule | Example |
|---|---|---|
| Keyword / identifier | letter or `_`, then letters, digits, `_`. **Unquoted words fold to lower case.** | `Users` and `users` are the same name |
| Quoted identifier | `"..."` or `` `...` ``, case kept, doubled quote inside stands for one quote, cannot be empty | `"userId"`, `"a""b"` |
| String literal (SQL) | `'...'`, `''` is one quote, **backslash is an ordinary character** | `'it''s'`, `'C:\dir'` |
| Integer | digits; a value over 64 bits becomes a float | `42` |
| Float | digits with `.` and/or exponent | `4.5`, `.5`, `2.5E-2` |
| Comment | `-- to end of line`, `/* block */` | |
| Operators | `+ - * / % \|\| = <> != < <= > >= ::` and `( ) , . ;` | |

A number directly followed by letters (`12abc`, `1e`) is an error, not a number then a name.

Two sub-languages have their own literal rules, because their data is not SQL text:

- **Key-value keys and values** are literal text. A bare key is any run of non-blank characters, case kept (`user:42`, `cache/a-B`). A quoted key or value (`'...'` or `"..."`) takes backslash escapes: `\n \t \r \0 \\ \' \"`; any other escaped character stands for itself. An unquoted `SET` value runs verbatim to the end of the statement (`SET greeting hello world` stores `hello world`). `--` is ordinary text here, not a comment.
- **Document arguments** are relaxed JSON: bare or quoted keys, single- or double-quoted strings with JSON escapes (`\n`, `\"`, `\u00e9`), numbers, `true`, `false`, `null`, arrays, objects.

### Identifier consistency

| Where | Rule |
|---|---|
| tables, columns, aliases, functions, `DOCUMENT(name)` and document fields in SQL | fold unquoted, keep quoted |
| collection in `db.<collection>.method(...)` | taken verbatim (the MongoDB shell convention) |
| key-value keys | literal text, never folded |

A document written with field `userId` is read in SQL as `o."userId"`; unquoted, `o.userId` means the field `userid`. A collection created as `db.Logs` is read in SQL as `DOCUMENT("Logs")`.

## Statements

```ebnf
command     = statement [ ";" ] ;
statement   = explain | transaction | utility | kv | document | sql ;

explain     = "EXPLAIN" [ "ANALYZE" ] statement ;            (* not nested; see below *)
transaction = ( "BEGIN" | "START" "TRANSACTION" ) [ "TRANSACTION" | "WORK" ]
            | ( "COMMIT" | "END" ) [ "TRANSACTION" | "WORK" ]
            | ( "ROLLBACK" | "ABORT" ) [ "TRANSACTION" | "WORK" ] ;
utility     = "SHOW" ( "TABLES" | "COLLECTIONS" ) | "CHECKPOINT" ;

kv          = "SET" key value | "GET" key | "DEL" key | "EXISTS" key
            | "KEYS" [ prefix ] | "INCR" key [ integer ] | "DECR" key [ integer ] ;

document    = "db" "." collection "." method "(" [ json { "," json } ] ")" { "." modifier "(" json ")" } ;
method      = insert | insertOne | insertMany | find | findOne | update | updateOne | updateMany
            | delete | deleteOne | deleteMany | remove | count | countDocuments | createIndex | drop ;
modifier    = sort | skip | limit ;                              (* find / findOne only *)

sql         = query | insert | update | delete | ddl | utility_sql ;
query       = select { ( "UNION" | "INTERSECT" | "EXCEPT" ) [ "ALL" | "DISTINCT" ] select }
              [ "ORDER BY" order { "," order } ] [ "LIMIT" ( integer | "ALL" ) ] [ "OFFSET" integer ] ;
select      = "SELECT" [ "DISTINCT" | "ALL" ] item { "," item }
              [ "FROM" source { join } ] [ "WHERE" expr ]
              [ "GROUP BY" expr { "," expr } ] [ "HAVING" expr ] ;
item        = "*" | name "." "*" | expr [ [ "AS" ] name ] ;
source      = name [ [ "AS" ] alias ]
            | "DOCUMENT" "(" collection ")" [ [ "AS" ] alias ]        (* also DOCUMENTS(...) *)
            | "KV" "(" [ string ] ")" [ [ "AS" ] alias ]
            | "(" query ")" [ "AS" ] alias ;
join        = "," source
            | [ "INNER" | "LEFT" [ "OUTER" ] | "RIGHT" [ "OUTER" ] | "FULL" [ "OUTER" ] ] "JOIN" source "ON" expr
            | "CROSS" "JOIN" source ;
order       = expr [ "ASC" | "DESC" ] [ "NULLS" ( "FIRST" | "LAST" ) ] ;

insert      = "INSERT" "INTO" name [ "(" name { "," name } ")" ]
              ( "VALUES" row { "," row } | query | "(" query ")" | "DEFAULT" "VALUES" ) ;
row         = "(" ( expr | "DEFAULT" ) { "," ( expr | "DEFAULT" ) } ")" ;
update      = "UPDATE" name "SET" name "=" ( expr | "DEFAULT" ) { "," name "=" ( expr | "DEFAULT" ) } [ "WHERE" expr ] ;
delete      = "DELETE" "FROM" name [ "WHERE" expr ] ;
ddl         = create_table | "DROP" "TABLE" [ "IF" "EXISTS" ] name [ "CASCADE" | "RESTRICT" ]
            | "CREATE" [ "UNIQUE" ] "INDEX" [ "IF" "NOT" "EXISTS" ] name "ON" name "(" name { "," name } ")"
            | "DROP" "INDEX" [ "IF" "EXISTS" ] name [ "ON" name ]
            | "ALTER" "TABLE" name action { "," action } | "TRUNCATE" [ "TABLE" ] name ;
utility_sql = ( "DESCRIBE" | "DESC" ) [ "TABLE" ] name | "CHECK" ( "TABLES" | "TABLE" name ) | "REINDEX" [ [ "TABLE" ] name ] ;
```

`create_table`, the column types and the `ALTER TABLE` actions are listed in [REFERENCE.md](REFERENCE.md#relational); they are unchanged.

`EXPLAIN` applies to `SELECT`, `INSERT`, `UPDATE`, `DELETE`, the document commands that read or change documents, and the key-value commands.

## Expressions and operator precedence

Loosest first. Binary operators of one level associate to the left; comparisons do not chain (`a = b = c` is a syntax error; write `(a = b) = c`).

| Level | Operators |
|---|---|
| 1 | `OR` |
| 2 | `AND` |
| 3 | `NOT` (prefix) |
| 4 | `= <> != < <= > >=`, `IS [NOT] NULL`, `[NOT] LIKE`, `[NOT] IN (...)`, `[NOT] BETWEEN ... AND ...` |
| 5 | `+ - \|\|` |
| 6 | `* / %` |
| 7 | unary `-`, unary `+` |
| 8 | `::type` (cast) |
| 9 | literals, names, `alias.field.path`, `( expr )`, `( subquery )`, `EXISTS ( subquery )`, `CASE ... END`, `CAST(expr AS type)`, function calls, aggregates |

So `NOT a = b` is `NOT (a = b)`, `-2 * 3` is `(-2) * 3`, and `a || b = c` is `(a || b) = c`.

**Functions**: `COALESCE IFNULL NULLIF GREATEST LEAST LOWER UPPER TRIM LTRIM RTRIM LENGTH SUBSTR SUBSTRING REPLACE CONCAT ABS ROUND FLOOR CEIL CEILING TYPEOF VERSION`, plus `GET_PATH(value, 'a.b.0')` (a field of a document value; text holding JSON is read as JSON) and `JSON(text)` (parse JSON text into a document value). **Aggregates**: `COUNT(*)`, `COUNT SUM AVG MIN MAX`, each with optional `DISTINCT`.

Nesting limits (unchanged, see the README): 32 levels of nesting, 100 operators and nested sub-expressions per expression, subqueries 6 deep.

## Types and type checking

Static types: `INT`, `REAL`, `TEXT`, `BOOL`, `JSON` (arrays and objects from documents), `NULL`, and `ANY` (known only at run time: document fields, derived-table columns). Table columns take their declared type from the catalog; `KV()` columns `key` and `value` are `TEXT`.

| Rule | Example rejected |
|---|---|
| `+ - * / %` and unary `-` need numbers | `name + 1` (TEXT) |
| `AND`, `OR`, `NOT` need booleans | `1 AND TRUE` |
| `WHERE`, `ON`, `HAVING`, `CASE WHEN`, `CHECK` must be boolean | `WHERE 5` |
| `LIKE` needs text on both sides | `id LIKE '1%'` (INT) |
| values of different scalar classes (number, text, boolean) are not compared | `id = 'x'` (INT vs TEXT); use `CAST` |
| an `IN` list does not mix classes | `x IN (1, 'a')` |
| function names and argument counts are checked; text functions need text, numeric ones numbers | `LOWER(id)`, `LOWER('a', 'b')`, `NOPE(1)` |
| `SUM` / `AVG` need numbers | `SUM(name)` |
| aggregates only in the select list, `HAVING`, `ORDER BY`; never nested | `WHERE COUNT(*) > 1`, `MAX(COUNT(*))` |
| scalar and `IN` subqueries return one column | `(SELECT a, b)` |
| set operations have equal widths | `SELECT 1 UNION SELECT 1, 2` |
| `VALUES` rows match the column list | `INSERT INTO t (a, b) VALUES (1)` |
| names resolve and are not ambiguous | `SELECT nope FROM t` |

`NULL` and `ANY` pass every static rule. The evaluator checks `ANY` values again at run time with the same rules. `||` accepts any operands and yields text.

## NULL semantics

PBSQL uses SQL three-valued logic for every model:

- A comparison, arithmetic or `||` with a `NULL` operand is `NULL` (unknown). `WHERE` and `ON` keep a row only when the condition is `TRUE`.
- `NULL AND FALSE` is `FALSE`, `NULL OR TRUE` is `TRUE`; otherwise `AND`/`OR` with `NULL` is `NULL`. `NOT NULL` is `NULL`.
- `x IN (..., NULL)` is `TRUE` on a match, otherwise `NULL`; so `NOT IN` over a list holding `NULL` never qualifies a row.
- `IS NULL` / `IS NOT NULL` are never unknown.
- Aggregates skip `NULL`; `COUNT(*)` counts rows. `SUM`, `AVG`, `MIN`, `MAX` of no values are `NULL`.
- `ORDER BY` puts `NULL` first ascending and last descending, unless `NULLS FIRST|LAST` says otherwise.
- **Documents:** a field a document does not have reads as `NULL`, also when no document in the collection has it (when it is named through the alias, as in `o.field`). A JSON `null` is also `NULL`. A path through a missing or non-object value (`o.a.b` where `a` is a number) is `NULL`.
- **Key-value:** a missing key is `NULL` (`GET` returns `NULL`, a `LEFT JOIN KV(...)` produces `NULL` columns).

## Cross-model queries

### Sources

| Source | Rows | Columns |
|---|---|---|
| `table [AS a]` | the table's rows | the declared columns |
| `DOCUMENT(collection) [AS a]` (default alias: the collection) | one per document | `_id`, every top-level field any matching document has (sorted), and any field the query names through the alias |
| `KV('prefix') [AS a]`, `KV()` (default alias: `kv`) | one per key starting with the prefix | `key`, `value` (text) |

Nested document values are reached with dotted names after the alias: `o.ship.address.city` means `GET_PATH(o.ship, 'address.city')`. Array elements need `GET_PATH(o.tags, '0')`, because `.0` would lex as a number. Text holding JSON (typically a key-value value) can be read the same way: `k.value.theme`.

```sql
-- table x documents x keys, outer joins keep unmatched rows
SELECT u.name, COUNT(o._id) AS orders, q.value AS quota
  FROM users u
  LEFT JOIN DOCUMENT(orders) o ON o.user = u.id
  LEFT JOIN KV('quota:') q ON q.key = 'quota:' || CAST(u.id AS TEXT)
 GROUP BY u.name, q.value;

-- filtering one model by another
SELECT name FROM users WHERE id IN (SELECT o.user FROM DOCUMENT(orders) o WHERE o.total > 10);

-- writing one model from another: the target's constraints apply
INSERT INTO order_totals SELECT o._id, o.user, o.total FROM DOCUMENT(orders) o;
UPDATE users SET vip = TRUE WHERE id IN (SELECT CAST(SUBSTR(k.key, 7) AS INT) FROM KV('quota:') k);
```

### The common execution interface

Every base source is opened as a `Relation` (`src/sql/sources.rs`):

```rust
trait Relation {
    fn model(&self) -> Model;                                       // Relational | Document | KeyValue
    fn read(&self, q: &QCtx, hint: Option<&Expr>) -> Result<RowSet>; // columns + rows
}
```

Joins, filtering, grouping, ordering and set operations run above this interface and see only rows. Each relation reads through its own engine, with the statement's transaction context:

- tables through the relational access-path planner (primary-key and index lookups, range scans);
- documents through `doc::find`, which uses `_id` lookups and field indexes;
- keys through the key-value module.

So the storage guarantees hold. Reads join the session's read set and are validated at `COMMIT`, writes into a table pass its constraints and foreign keys, and a failing statement leaves nothing behind. No path in PBSQL reads or writes the raw storage keys of another model.

### Pushdown

When a document or key-value source comes first in `FROM`, the `WHERE` clause is offered to it as a hint. The executor still applies the full `WHERE` to every row, so the hint may only narrow what is read, never drop a row the predicate would keep.

| Source | Pushed | Not pushed (and why) |
|---|---|---|
| document | `alias.field = constant`, `alias.a.b = constant`, `alias.field IN (constants)` on scalar constants. These become a document filter, which can use `_id` or a field index. | range comparisons: the document `$gt` does not compare across types, SQL `>` does |
| key-value | `key = 'k'` becomes one point lookup; `key LIKE 'abc%'` narrows the prefix scan | |

Unqualified column names are pushed only when the source is the query's only source and the query is not a subquery, because otherwise the name could belong to another table.

## Transactions

`BEGIN` opens a transaction on the session; statements of every model and cross-model queries run inside it and see its own writes; `COMMIT` validates every read against commits made since `BEGIN` (serializable isolation) and writes everything or nothing; `ROLLBACK` discards it. A document or key that a SQL query read through `DOCUMENT()` or `KV()` counts as read, so this conflicts:

```
session A: BEGIN; SELECT COUNT(*) FROM DOCUMENT(orders) o WHERE o.user = 1;
session B: db.orders.insert({_id: 15, user: 1})          -- commits
session A: SET orders_of_1 2; COMMIT                     -- serialization conflict, nothing written
```

Inside a transaction, a statement that fails is undone on its own; the transaction stays open. `SAVEPOINT` is not supported.

## EXPLAIN and EXPLAIN ANALYZE

- `EXPLAIN <statement>` lists the access path each source uses: for tables the chosen index or scan, for documents `_id lookup`, `index on <field>` or `full scan` plus the pushed filter, for key-value `key lookup` or `prefix scan`, and the join strategy (hash or nested loop). **It never writes.** For `UPDATE` and `DELETE` it shows how the target rows are found.
- `EXPLAIN ANALYZE <statement>` **runs the statement for real** (atomically, or inside the open transaction, where `ROLLBACK` undoes it). It adds the rows each source read from which engine, the rows returned or changed, documents examined, and the execution time.

```
EXPLAIN ANALYZE SELECT u.name FROM users u JOIN DOCUMENT(orders) o ON o.user = u.id
 users AS u: full scan
 u: 3 row(s) read from relational storage
 orders AS o: collection orders: full scan
 o: 4 row(s) read from document storage
 o: hash inner join
 actual: 3 row(s) returned
 execution time: 0.084 ms
```

## Normalization

`polydb::pbsql::normalize(text)` returns a statement's canonical text: keywords and function names in upper case; identifiers bare when they are lower-case non-keywords, otherwise double-quoted; the minimum parentheses precedence needs; sugar removed (`BETWEEN` becomes two comparisons, `a, b` becomes `CROSS JOIN`, `!=` becomes `<>`, an alias equal to its table is dropped, `DECR k n` becomes `INCR k -n`); document arguments as canonical JSON with sorted keys; key-value text quoted only when it would not read back unchanged.

The canonical text parses back to the same tree, so `normalize(normalize(s)) == normalize(s)`. Equivalent spellings normalize to the same text:

```
select A from T where (A != 1)        ─┐
SELECT a FROM t t WHERE ((a)) <> 1    ─┴─►  SELECT a FROM t WHERE a <> 1
```

`polydb::pbsql::validate(text)` checks syntax and every semantic rule that needs no catalog. `Database::validate(text)` also resolves names and types against the database. Neither runs anything.

## Errors

| Kind | Raised for |
|---|---|
| `parse error` | syntax, unknown function, wrong argument count, misplaced aggregate, unsupported statement; syntax errors carry `(at line L, column C)` |
| `type error` | static or run-time type mismatch, bad document argument shape |
| `schema error` | unknown table, column, collection or index; ambiguous name; wrong subquery width |
| `constraint violation` | primary key, unique, NOT NULL, CHECK, foreign key |
| `transaction error` / `serialization conflict` | misuse of `BEGIN`/`COMMIT`; a stale read at `COMMIT` |

Unsupported statements fail with a message naming the feature instead of being misread: `WITH` (CTEs), `CREATE VIEW|TRIGGER|SCHEMA|DATABASE|SEQUENCE|FUNCTION|PROCEDURE|TYPE|ROLE|USER`, `MERGE`, `GRANT`/`REVOKE`, `SAVEPOINT`/`RELEASE`, `VACUUM`/`ANALYZE`, `COPY`, `REPLACE`/`UPSERT`, `ON ... SET DEFAULT`, nested `EXPLAIN`, `EXPLAIN` of DDL or transaction control. Only one statement is accepted per command.

## Implementation map

| File | Role |
|---|---|
| `src/pbsql/lexer.rs` | tokens, spans, line/column positions, raw key-value modes |
| `src/pbsql/parser.rs` | statement dispatch; transaction, utility, key-value and document grammar |
| `src/pbsql/ast.rs` | the unified `Statement` tree |
| `src/pbsql/exec.rs` | execution against the engines; EXPLAIN for document and key-value commands |
| `src/pbsql/mod.rs` | `validate`, `normalize`, document argument checks |
| `src/sql/parser.rs` | relational grammar, including `DOCUMENT()` / `KV()` sources and dotted paths |
| `src/sql/sema.rs` | name resolution, type inference and checking, aggregate and shape rules |
| `src/sql/printer.rs` | canonical text of relational statements |
| `src/sql/sources.rs` | the `Relation` interface and its relational, document and key-value implementations |
| `src/sql/walk.rs` | syntax-tree traversal |

Tests: `src/pbsql/tests.rs` (lexer, parser and tree shapes, precedence, normalization, semantic rules, invalid syntax) and `tests/pbsql.rs` (execution against real storage: cross-model joins and filters, cross-model writes under constraints, transactions and conflicts, durability across reopen, EXPLAIN / EXPLAIN ANALYZE, NULL, identifier and escaping rules).
