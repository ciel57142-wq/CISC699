//! Unit tests of the PBSQL front end: lexer, parser and syntax trees,
//! normalization, semantic analysis, and rejection of invalid input.
//! (Execution against the storage engines is tested in `tests/pbsql.rs`.)

use super::ast::*;
use super::lexer::{line_col, tokenize, Lexer, Tok};
use super::{normalize, parse, validate};
use crate::error::Error;
use crate::sql::ast::{Expr, InsertSource, Item, JoinKind, Op, Source, Stmt};
use crate::sql::sema::{type_of, SqlType};
use crate::value::Value;

fn toks(src: &str) -> Vec<Tok> {
    tokenize(src).unwrap_or_else(|e| panic!("{src}: {e}")).into_iter().map(|t| t.tok).collect()
}

fn w(s: &str) -> Tok {
    Tok::Word(s.into())
}

fn sql(src: &str) -> Stmt {
    match parse(src).unwrap_or_else(|e| panic!("{src}: {e}")) {
        Statement::Sql(s) => s,
        other => panic!("{src}: expected SQL, got {other:?}"),
    }
}

/// The expression of `SELECT <e>`.
fn expr(e: &str) -> Expr {
    match sql(&format!("SELECT {e}")) {
        Stmt::Query(q) => match q.head.items.into_iter().next() {
            Some(Item::Expr(e, _)) => e,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn int(n: i64) -> Expr {
    Expr::Lit(Value::Int(n))
}

fn col(n: &str) -> Expr {
    Expr::Col(None, n.into())
}

fn bin(a: Expr, op: Op, b: Expr) -> Expr {
    Expr::Bin(Box::new(a), op, Box::new(b))
}

/// Structural equality of expressions (the syntax tree has no PartialEq
/// because subqueries carry process-unique ids); compared through the canonical printer.
fn same(a: &Expr, b: &Expr) -> bool {
    crate::sql::printer::expr(a) == crate::sql::printer::expr(b) && format!("{a:?}") == format!("{b:?}")
}

fn parse_err(src: &str) -> String {
    match parse(src) {
        Err(Error::Parse(m)) => m,
        other => panic!("{src:?} should be a parse error, got {other:?}"),
    }
}

fn invalid(src: &str) -> Error {
    match validate(src) {
        Err(e) => e,
        Ok(()) => panic!("{src:?} should be rejected"),
    }
}

// ===================================================================== lexer

#[test]
fn lexer_folds_words_and_keeps_quoted_identifiers() {
    assert_eq!(toks("SeLeCt Name"), vec![w("select"), w("name")]);
    assert_eq!(toks(r#""Mixed Case" `back``tick` "a""b""#), vec![
        Tok::Ident("Mixed Case".into()),
        Tok::Ident("back`tick".into()),
        Tok::Ident("a\"b".into())
    ]);
    // unicode letters are identifier characters and fold too
    assert_eq!(toks("Ärger"), vec![w("ärger")]);
}

#[test]
fn lexer_reads_strings_numbers_and_operators() {
    assert_eq!(toks("'it''s' ''"), vec![Tok::Str("it's".into()), Tok::Str(String::new())]);
    // backslashes are ordinary characters in SQL strings
    assert_eq!(toks(r"'a\nb'"), vec![Tok::Str(r"a\nb".into())]);
    assert_eq!(toks("42 4.5 .5 1e3 2.5E-2"), vec![
        Tok::Int(42),
        Tok::Float(4.5),
        Tok::Float(0.5),
        Tok::Float(1000.0),
        Tok::Float(0.025)
    ]);
    // an integer too large for 64 bits becomes a float
    assert_eq!(toks("99999999999999999999"), vec![Tok::Float(1e20)]);
    assert_eq!(toks("a<=b<>c!=d||e::f>=g"), vec![
        w("a"), Tok::Sym("<="), w("b"), Tok::Sym("<>"), w("c"), Tok::Sym("!="), w("d"),
        Tok::Sym("||"), w("e"), Tok::Sym("::"), w("f"), Tok::Sym(">="), w("g")
    ]);
}

#[test]
fn lexer_skips_both_comment_styles() {
    assert_eq!(toks("SELECT -- a comment\n 1 /* block\n comment */ + 2"), vec![
        w("select"),
        Tok::Int(1),
        Tok::Sym("+"),
        Tok::Int(2)
    ]);
}

#[test]
fn lexer_spans_and_positions_are_exact() {
    let src = "SELECT\n  naïve, 'x'";
    let t = tokenize(src).unwrap();
    assert_eq!(&src[t[1].span.start..t[1].span.end], "naïve");
    assert_eq!(line_col(src, t[1].span.start), (2, 3));
    // columns count characters, not bytes
    assert_eq!(line_col(src, t[3].span.start), (2, 10));
}

#[test]
fn lexer_errors_name_the_position() {
    for (src, msg, at) in [
        ("SELECT 'abc", "unterminated string literal", "(at line 1, column 8)"),
        ("SELECT\n\"abc", "unterminated quoted identifier", "(at line 2, column 1)"),
        ("SELECT 1 /* never closed", "unterminated /* comment", "(at line 1, column 10)"),
        ("SELECT 12abc", "invalid number '12abc'", "(at line 1, column 8)"),
        ("SELECT 1.2.3", "invalid number '1.2.3'", "(at line 1, column 8)"),
        ("SELECT 1e", "invalid number '1e'", "(at line 1, column 8)"),
        ("SELECT a ? b", "unexpected character '?'", "(at line 1, column 10)"),
        ("SELECT \"\"", "a quoted identifier cannot be empty", "(at line 1, column 8)"),
    ] {
        let m = tokenize(src).map(|_| ()).unwrap_err().to_string();
        assert!(m.contains(msg) && m.contains(at), "{src:?}: {m}");
    }
}

#[test]
fn raw_lexer_mode_keeps_key_value_text_literal() {
    let mut lx = Lexer::new("  user:42/a-B 'q\\'d\\n' rest -- of line ;");
    assert_eq!(lx.raw_word().unwrap().unwrap().0, "user:42/a-B");
    assert_eq!(lx.raw_word().unwrap().unwrap().0, "q'd\n");
    assert_eq!(lx.rest().0, "rest -- of line");
}

// ======================================================= parser: precedence

#[test]
fn arithmetic_precedence_and_associativity() {
    // * binds tighter than +
    assert!(same(&expr("1 + 2 * 3"), &bin(int(1), Op::Add, bin(int(2), Op::Mul, int(3)))));
    // parentheses override
    assert!(same(&expr("(1 + 2) * 3"), &bin(bin(int(1), Op::Add, int(2)), Op::Mul, int(3))));
    // left associative
    assert!(same(&expr("10 - 4 - 3"), &bin(bin(int(10), Op::Sub, int(4)), Op::Sub, int(3))));
    assert!(same(&expr("8 / 4 % 3"), &bin(bin(int(8), Op::Div, int(4)), Op::Mod, int(3))));
    // unary minus binds tighter than *
    assert!(same(&expr("-2 * 3"), &bin(Expr::Neg(Box::new(int(2))), Op::Mul, int(3))));
    // || sits with + and binds tighter than =
    assert!(same(&expr("a || b = c"), &bin(bin(col("a"), Op::Concat, col("b")), Op::Eq, col("c"))));
}

#[test]
fn boolean_precedence_or_and_not_comparison() {
    // AND binds tighter than OR
    assert!(same(
        &expr("a OR b AND c"),
        &bin(col("a"), Op::Or, bin(col("b"), Op::And, col("c")))
    ));
    // NOT binds looser than comparison: NOT (a = b)
    assert!(same(&expr("NOT a = b"), &Expr::Not(Box::new(bin(col("a"), Op::Eq, col("b"))))));
    // ... and tighter than AND
    assert!(same(
        &expr("NOT a AND b"),
        &bin(Expr::Not(Box::new(col("a"))), Op::And, col("b"))
    ));
    // comparison binds looser than arithmetic
    assert!(same(
        &expr("a + 1 > b * 2"),
        &bin(bin(col("a"), Op::Add, int(1)), Op::Gt, bin(col("b"), Op::Mul, int(2)))
    ));
}

#[test]
fn predicates_and_nested_expressions_parse_into_the_right_nodes() {
    // BETWEEN is sugar for two comparisons
    assert!(same(
        &expr("x BETWEEN 1 AND 5"),
        &bin(bin(col("x"), Op::Ge, int(1)), Op::And, bin(col("x"), Op::Le, int(5)))
    ));
    assert!(matches!(expr("x NOT IN (1, 2)"), Expr::In(_, l, true) if l.len() == 2));
    assert!(matches!(expr("x IS NOT NULL"), Expr::IsNull(_, true)));
    assert!(matches!(expr("x NOT LIKE 'a%'"), Expr::Like(_, _, true)));
    assert!(matches!(expr("x IN (SELECT 1)"), Expr::InSub(_, _, false)));
    assert!(matches!(expr("EXISTS (SELECT 1)"), Expr::Exists(_)));
    assert!(matches!(expr("x::text"), Expr::Cast(_, crate::sql::ast::Ty::Text)));
    // a deeply nested mix keeps its shape through a normalize round trip
    let e = "CASE WHEN (a + b) * -c > 10 AND NOT (d IS NULL OR e IN (1, 2, (SELECT MAX(f) FROM t))) \
             THEN COALESCE(g, LOWER(h || 'x')) ELSE CAST(i AS REAL) / 2 END";
    let n = normalize(&format!("SELECT {e}")).unwrap();
    assert_eq!(
        n,
        "SELECT CASE WHEN (a + b) * -c > 10 AND NOT (d IS NULL OR e IN (1, 2, (SELECT MAX(f) FROM t))) \
         THEN COALESCE(g, LOWER(h || 'x')) ELSE CAST(i AS REAL) / 2 END"
    );
    assert_eq!(normalize(&n).unwrap(), n);
}

// ========================================================= parser: statements

#[test]
fn cross_model_sources_parse_into_document_and_kv_relations() {
    let Stmt::Query(q) = sql("SELECT * FROM users u JOIN DOCUMENT(orders) o ON o.user = u.id LEFT JOIN KV('pref:') p ON p.key = 'x'")
    else {
        panic!()
    };
    assert!(matches!(&q.head.from.as_ref().unwrap().src, Source::Table(t) if t == "users"));
    assert!(matches!(&q.head.joins[0].item.src, Source::Collection(c) if c == "orders"));
    assert_eq!(q.head.joins[0].item.alias, "o");
    assert_eq!(q.head.joins[0].kind, JoinKind::Inner);
    assert!(matches!(&q.head.joins[1].item.src, Source::Kv(p) if p == "pref:"));
    assert_eq!(q.head.joins[1].kind, JoinKind::Left);
    // default aliases: the collection name, and "kv"
    let Stmt::Query(q) = sql("SELECT * FROM DOCUMENT(events), KV()") else { panic!() };
    assert_eq!(q.head.from.as_ref().unwrap().alias, "events");
    assert_eq!(q.head.joins[0].item.alias, "kv");
}

#[test]
fn dotted_names_reach_into_documents() {
    match expr("o.ship.address.city") {
        Expr::Func(f, args) => {
            assert_eq!(f, "get_path");
            assert!(matches!(&args[0], Expr::Col(Some(a), c) if a == "o" && c == "ship"));
            assert!(matches!(&args[1], Expr::Lit(Value::Text(p)) if p == "address.city"));
        }
        other => panic!("{other:?}"),
    }
    // case-sensitive document fields are quoted, like any identifier
    assert!(matches!(expr(r#"o."userId""#), Expr::Col(Some(_), c) if c == "userId"));
}

#[test]
fn document_commands_parse_into_typed_operations() {
    let Statement::Doc(d) = parse("db.Events.find({type: 'login', n: {$gt: 2}}).sort({ts: -1}).skip(5).limit(10)").unwrap() else {
        panic!()
    };
    assert_eq!(d.collection, "Events", "collection names keep their case");
    match d.op {
        DocOp::Find { filter, spec, one, end } => {
            assert_eq!(filter.get("type"), Some(&Value::Text("login".into())));
            assert!(filter.get("n").and_then(|n| n.get("$gt")).is_some());
            assert_eq!(spec.sort.unwrap().get("ts"), Some(&Value::Int(-1)));
            assert_eq!((spec.skip, spec.limit, one, end), (5, Some(10), false, None));
        }
        other => panic!("{other:?}"),
    }
    let op = |s: &str| match parse(s).unwrap_or_else(|e| panic!("{s}: {e}")) {
        Statement::Doc(d) => d.op,
        other => panic!("{other:?}"),
    };
    assert!(matches!(op("db.c.findOne()"), DocOp::Find { spec: FindSpec { limit: Some(1), .. }, one: true, .. }));
    assert!(matches!(op("db.c.find({}, {a: 1}).count()"), DocOp::Find { spec: FindSpec { projection: Some(_), .. }, end: Some(FindEnd::Count), .. }));
    assert!(matches!(op("db.c.find({a: 1}).explain()"), DocOp::Find { end: Some(FindEnd::Explain), .. }));
    assert!(matches!(op("db.c.insertMany([{a: 1}, {a: 2}])"), DocOp::Insert(Value::Array(a)) if a.len() == 2));
    assert!(matches!(op("db.c.updateOne({a: 1}, {$set: {b: 2}})"), DocOp::Update { method: UpdateMethod::UpdateOne, options: None, .. }));
    assert!(matches!(op("db.c.update({}, {$set: {b: 2}}, {multi: false, upsert: true})"), DocOp::Update { method: UpdateMethod::Update, options: Some(_), .. }));
    assert!(matches!(op("db.c.replaceOne({_id: 1}, {b: 2})"), DocOp::Update { method: UpdateMethod::ReplaceOne, .. }));
    assert!(matches!(op("db.c.deleteOne({})"), DocOp::Delete { one: true, .. }));
    assert!(matches!(op("db.c.createIndex({email: 1}, {unique: true})"), DocOp::CreateIndex { options: Some(_), .. }));
    assert!(matches!(op("db.c.distinct('city', {age: {$gt: 1}})"), DocOp::Distinct { field, .. } if field == "city"));
    assert!(matches!(op("db.c.aggregate([{$match: {}}, {$count: 'n'}])"), DocOp::Aggregate(st) if st.len() == 2));
    assert!(matches!(op("db.c.bulkWrite([])"), DocOp::BulkWrite(_)));
    assert!(matches!(op("db.c.getIndexes()"), DocOp::GetIndexes));
    assert!(matches!(op("db.c.drop();"), DocOp::Drop));
}

#[test]
fn key_value_commands_keep_their_text_exactly() {
    let kv = |s: &str| match parse(s).unwrap_or_else(|e| panic!("{s}: {e}")) {
        Statement::Kv(k) => k,
        other => panic!("{other:?}"),
    };
    assert_eq!(kv("SET greeting hello  world"), KvStmt::Set { key: "greeting".into(), value: "hello  world".into() });
    assert_eq!(kv("set Session:A {\"x\": [1, 2]}"), KvStmt::Set { key: "Session:A".into(), value: "{\"x\": [1, 2]}".into() });
    assert_eq!(kv(r#"SET "a key" 'tab\there \'q\' \\'"#), KvStmt::Set { key: "a key".into(), value: "tab\there 'q' \\".into() });
    assert_eq!(kv("SET k a--b;"), KvStmt::Set { key: "k".into(), value: "a--b".into() });
    assert_eq!(kv("GET user:1;"), KvStmt::Get("user:1".into()));
    assert_eq!(kv("KEYS"), KvStmt::Keys(String::new()));
    assert_eq!(kv("KEYS user:"), KvStmt::Keys("user:".into()));
    assert_eq!(kv("INCR hits"), KvStmt::Incr { key: "hits".into(), by: 1 });
    assert_eq!(kv("INCR hits -5"), KvStmt::Incr { key: "hits".into(), by: -5 });
    assert_eq!(kv("DECR hits 2"), KvStmt::Incr { key: "hits".into(), by: -2 });
}

#[test]
fn transactions_utilities_and_explain() {
    for (src, op) in [
        ("BEGIN", TxnOp::Begin),
        ("begin transaction", TxnOp::Begin),
        ("START TRANSACTION;", TxnOp::Begin),
        ("COMMIT WORK", TxnOp::Commit),
        ("END", TxnOp::Commit),
        ("ROLLBACK", TxnOp::Rollback),
        ("abort", TxnOp::Rollback),
    ] {
        assert!(matches!(parse(src).unwrap(), Statement::Txn(o) if o == op), "{src}");
    }
    assert!(matches!(parse("SHOW collections").unwrap(), Statement::Show(ShowWhat::Collections)));
    assert!(matches!(parse("CHECKPOINT").unwrap(), Statement::Checkpoint));
    match parse("EXPLAIN ANALYZE SELECT 1").unwrap() {
        Statement::Explain { analyze: true, stmt } => assert!(matches!(*stmt, Statement::Sql(Stmt::Query(_)))),
        other => panic!("{other:?}"),
    }
    assert!(matches!(parse("EXPLAIN db.c.find({a: 1})").unwrap(), Statement::Explain { analyze: false, .. }));
    assert!(matches!(parse("EXPLAIN DELETE FROM t WHERE a = 1").unwrap(), Statement::Explain { .. }));
}

#[test]
fn read_only_classification_follows_the_tree_not_the_first_word() {
    let ro = |s: &str| parse(s).unwrap().is_read_only();
    assert!(ro("SELECT 1"));
    assert!(ro("db.c.find({})"));
    assert!(ro("db.c.count({})"));
    assert!(ro("GET k"));
    assert!(ro("EXPLAIN DELETE FROM t"), "plain EXPLAIN writes nothing");
    assert!(!ro("EXPLAIN ANALYZE DELETE FROM t"), "EXPLAIN ANALYZE runs the statement");
    assert!(!ro("db.c.insert({a: 1})"));
    assert!(!ro("SET k v"));
    assert!(!ro("INSERT INTO t SELECT * FROM DOCUMENT(c)"));
}

#[test]
fn insert_update_create_index_and_alter_table_parse() {
    match sql("INSERT INTO t (a, b) VALUES (1, DEFAULT), (2, 'x')") {
        Stmt::Insert { cols, source: InsertSource::Values(rows), .. } => {
            assert_eq!(cols.unwrap(), vec!["a", "b"]);
            assert!(rows[0][1].is_none());
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(sql("INSERT INTO t SELECT o.a FROM DOCUMENT(c) o"), Stmt::Insert { source: InsertSource::Query(_), .. }));
    assert!(matches!(sql("CREATE UNIQUE INDEX IF NOT EXISTS i ON t (a, b)"), Stmt::CreateIndex { unique: true, if_not_exists: true, cols, .. } if cols.len() == 2));
    assert!(matches!(sql("ALTER TABLE t ADD COLUMN c INT NOT NULL DEFAULT 0, RENAME COLUMN a TO z"), Stmt::AlterTable { actions, .. } if actions.len() == 2));
    assert!(matches!(sql("UPDATE t SET a = a + 1, b = DEFAULT WHERE c"), Stmt::Update { sets, .. } if sets.len() == 2));
}

// ============================================================= normalization

#[test]
fn normalization_produces_one_canonical_spelling() {
    for (input, canonical) in [
        ("select  *  from T where a!=1", "SELECT * FROM t WHERE a <> 1"),
        ("SELECT a AS a FROM t t", "SELECT a AS a FROM t"),
        ("select x from a, b", "SELECT x FROM a CROSS JOIN b"),
        ("select ((1)) + (2 * 3)", "SELECT 1 + 2 * 3"),
        ("select (1 + 2) * 3, 1 - (2 - 3), (1 - 2) - 3", "SELECT (1 + 2) * 3, 1 - (2 - 3), 1 - 2 - 3"),
        ("select not (a and b), (not a) and b", "SELECT NOT (a AND b), NOT a AND b"),
        ("select - -1, -(-1)", "SELECT -(-1), -(-1)"),
        ("select x::int, count(distinct y)", "SELECT CAST(x AS INT), COUNT(DISTINCT y)"),
        ("select \"Weird Name\", \"select\", \"plain\"", "SELECT \"Weird Name\", \"select\", plain"),
        ("select 'it''s', 1.0, 2e3", "SELECT 'it''s', 1.0, 2000.0"),
        ("select a between 1 and 2", "SELECT a >= 1 AND a <= 2"),
        ("SELECT o.ship.city FROM documents(orders) AS o", "SELECT o.ship.city FROM DOCUMENT(orders) AS o"),
        ("select * from kv('a:') kv", "SELECT * FROM KV('a:')"),
        ("select * from t order by a desc nulls last limit 5 offset 2", "SELECT * FROM t ORDER BY a DESC NULLS LAST LIMIT 5 OFFSET 2"),
        ("select a from t union all select b from u order by 1", "SELECT a FROM t UNION ALL SELECT b FROM u ORDER BY 1"),
        ("insert into t values (1, default)", "INSERT INTO t VALUES (1, DEFAULT)"),
        ("update t set a=a+1 where id=1", "UPDATE t SET a = a + 1 WHERE id = 1"),
        ("delete from t", "DELETE FROM t"),
        ("create table t (id serial primary key, n varchar(5) unique, c real check (c > 0))",
         "CREATE TABLE t (id INT NOT NULL AUTOINCREMENT, n VARCHAR(5), c REAL, PRIMARY KEY (id), UNIQUE (n), CHECK (c > 0))"),
        ("create index i on t(a,b)", "CREATE INDEX i ON t (a, b)"),
        ("alter table t alter column a set default -1", "ALTER TABLE t ALTER COLUMN a SET DEFAULT -1"),
        ("drop table if exists t cascade", "DROP TABLE IF EXISTS t CASCADE"),
        ("db.c.find({b: 2, a: 1}).limit(3)", "db.c.find({\"a\":1,\"b\":2}).limit(3)"),
        ("db.c.remove({})", "db.c.deleteMany({})"),
        ("db.c.find({}, {a: 1}).sort({b: -1}).count()", "db.c.find({}).project({\"a\":1}).sort({\"b\":-1}).count()"),
        ("db.c.update({a: 1}, {$inc: {n: 1}}, {upsert: true})", "db.c.update({\"a\":1}, {\"$inc\":{\"n\":1}}, {\"upsert\":true})"),
        ("db.c.createIndex({b: 1, a: -1})", "db.c.createIndex({\"a\":-1,\"b\":1})"),
        ("db.c.findOne({a: 1})", "db.c.findOne({\"a\":1})"),
        ("set k  hello world ", "SET k hello world"),
        ("set k 'two\nlines'", "SET k 'two\\nlines'"),
        ("set 'a key' v", "SET 'a key' v"),
        ("decr n", "INCR n -1"),
        ("begin work", "BEGIN"),
        ("explain analyze select 1", "EXPLAIN ANALYZE SELECT 1"),
    ] {
        let n = normalize(input).unwrap_or_else(|e| panic!("{input}: {e}"));
        assert_eq!(n, canonical, "normalizing {input:?}");
        assert_eq!(normalize(&n).unwrap(), n, "canonical text must be a fixed point: {n:?}");
    }
}

#[test]
fn equivalent_spellings_normalize_identically() {
    let groups: [&[&str]; 4] = [
        &["SELECT a FROM t WHERE a <> 1", "select A from T where (A != 1)", "SELECT a FROM t t WHERE ((a)) <> 1 -- c"],
        &["SELECT * FROM a CROSS JOIN b", "select * from a, b", "SELECT * FROM a /* x */ CROSS JOIN b;"],
        &["db.c.find({\"x\":1})", "db.c.find({x: 1})", "db.c.find({'x': 1});"],
        &["SET k v", "set k v;", "SET k 'v'"],
    ];
    for g in groups {
        let first = normalize(g[0]).unwrap();
        for s in &g[1..] {
            assert_eq!(normalize(s).unwrap(), first, "{s:?} vs {:?}", g[0]);
        }
    }
}

// ========================================================= semantic checks

#[test]
fn static_types_follow_the_evaluator() {
    let ty = |e: &str| type_of(&expr(e)).unwrap_or_else(|err| panic!("{e}: {err}"));
    assert_eq!(ty("1 + 2"), SqlType::Int);
    assert_eq!(ty("1 + 2.5"), SqlType::Float);
    assert_eq!(ty("1 + NULL"), SqlType::Null);
    assert_eq!(ty("'a' || 1"), SqlType::Text);
    assert_eq!(ty("1 < 2 AND NULL"), SqlType::Bool);
    assert_eq!(ty("CAST('1' AS INT)"), SqlType::Int);
    assert_eq!(ty("LENGTH('abc')"), SqlType::Int);
    assert_eq!(ty("CASE WHEN TRUE THEN 1 ELSE 2.0 END"), SqlType::Float);
    assert_eq!(ty("COALESCE(NULL, 'x')"), SqlType::Text);
    assert_eq!(ty("(SELECT 1)"), SqlType::Int);
}

#[test]
fn semantic_errors_are_reported_before_execution() {
    type Case = (&'static str, fn(&Error) -> bool, &'static str);
    let cases: &[Case] = &[
        ("SELECT 'a' + 1", |e| matches!(e, Error::Type(_)), "cannot apply '+' to TEXT and INT in `'a' + 1`"),
        ("SELECT -'a'", |e| matches!(e, Error::Type(_)), "cannot negate TEXT"),
        ("SELECT 1 WHERE 5", |e| matches!(e, Error::Type(_)), "WHERE must be a boolean condition, but `5` is INT"),
        ("SELECT 1 AND TRUE", |e| matches!(e, Error::Type(_)), "AND needs boolean operands"),
        ("SELECT NOT 1", |e| matches!(e, Error::Type(_)), "NOT needs a boolean"),
        ("SELECT 1 LIKE 'a'", |e| matches!(e, Error::Type(_)), "LIKE needs text"),
        ("SELECT CASE WHEN 1 THEN 2 END", |e| matches!(e, Error::Type(_)), "CASE WHEN needs a boolean"),
        ("SELECT LOWER(1)", |e| matches!(e, Error::Type(_)), "lower() cannot take INT"),
        ("SELECT SUM('x')", |e| matches!(e, Error::Type(_)), "SUM needs numbers"),
        ("SELECT SUBSTR('abc', 'x')", |e| matches!(e, Error::Type(_)), "substr() takes (text, int [, int])"),
        ("SELECT NOPE(1)", |e| matches!(e, Error::Parse(_)), "unknown function 'nope'"),
        ("SELECT LOWER('a', 'b')", |e| matches!(e, Error::Parse(_)), "lower() takes 1 argument(s), got 2"),
        ("SELECT COALESCE()", |e| matches!(e, Error::Parse(_)), "coalesce() takes at least 1 argument(s), got 0"),
        ("SELECT 1 WHERE COUNT(*) > 0", |e| matches!(e, Error::Parse(_)), "aggregate functions are not allowed in WHERE"),
        ("SELECT MAX(COUNT(*))", |e| matches!(e, Error::Parse(_)), "aggregate functions cannot be nested"),
        ("SELECT (SELECT 1, 2)", |e| matches!(e, Error::Schema(_)), "subquery must return exactly one column, got 2"),
        ("SELECT 1 WHERE 1 IN (SELECT 1, 2)", |e| matches!(e, Error::Schema(_)), "exactly one column"),
        ("SELECT 1 UNION SELECT 1, 2", |e| matches!(e, Error::Schema(_)), "same number of columns (1 vs 2)"),
        ("SELECT *", |e| matches!(e, Error::Parse(_)), "SELECT * needs a FROM clause"),
        ("SELECT x", |e| matches!(e, Error::Schema(_)), "no such column 'x'"),
        ("SELECT k.nope FROM KV() k", |e| matches!(e, Error::Schema(_)), "no such column 'k.nope'"),
        ("SELECT k.key + 1 FROM KV() k", |e| matches!(e, Error::Type(_)), "cannot apply '+' to TEXT and INT"),
        ("SELECT * FROM DOCUMENT(c) a JOIN DOCUMENT(d) a ON TRUE", |e| matches!(e, Error::Schema(_)), "used twice"),
        ("UPDATE t SET a = 1, a = 2", |e| matches!(e, Error::Schema(_)), "column 'a' is assigned twice"),
        ("DELETE FROM t WHERE COUNT(*) > 1", |e| matches!(e, Error::Parse(_)), "not allowed in WHERE"),
        ("CREATE TABLE t (a INT, a TEXT)", |e| matches!(e, Error::Schema(_)), "column 'a' is defined twice"),
        ("CREATE TABLE t (a INT CHECK (a + 1))", |e| matches!(e, Error::Type(_)), "a CHECK constraint must be a boolean"),
        ("CREATE INDEX i ON t (a, a)", |e| matches!(e, Error::Schema(_)), "appears twice"),
        ("INSERT INTO t (a, b) VALUES (1)", |e| matches!(e, Error::Parse(_)), "expected 2 value(s) per row, got 1"),
        ("db.c.find(5)", |e| matches!(e, Error::Type(_)), "a filter must be a JSON object"),
        ("db.c.find({}).sort('a')", |e| matches!(e, Error::Type(_) | Error::Parse(_)), "sort"),
        ("db.c.find({a: {$bogus: 1}})", |e| matches!(e, Error::Parse(_) | Error::Type(_)), "$bogus"),
        ("db.c.updateOne({}, {b: 2})", |e| matches!(e, Error::Parse(_)), "use replaceOne"),
        ("db.c.replaceOne({}, {$set: {b: 2}})", |e| matches!(e, Error::Parse(_)), "replaceOne takes a document without operators"),
        ("db.c.insert([{a: 1}, 2])", |e| matches!(e, Error::Type(_)), "insert() needs documents"),
        ("db.c.createIndex(5)", |e| matches!(e, Error::Parse(_) | Error::Type(_)), ""),
        ("SET '' v", |e| matches!(e, Error::Parse(_)), "a key cannot be empty"),
    ];
    for (src, kind, fragment) in cases {
        let e = invalid(src);
        assert!(kind(&e), "{src:?} gave the wrong kind of error: {e:?}");
        assert!(e.to_string().contains(fragment), "{src:?}: {e}");
    }
}

#[test]
fn valid_complex_statements_pass_semantic_analysis() {
    for src in [
        "SELECT a, COUNT(*) AS n FROM t GROUP BY a HAVING COUNT(*) > 1 ORDER BY n DESC",
        "SELECT a AS x FROM t GROUP BY x ORDER BY 1",
        "SELECT * FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.id = t.id)",
        "SELECT u.name, o.total FROM users u JOIN DOCUMENT(orders) o ON o.user = u.id WHERE o.ship.city = 'Oslo'",
        "SELECT k.key, k.value.theme FROM KV('pref:') k WHERE k.key LIKE 'pref:%'",
        "SELECT * FROM (SELECT o.a FROM DOCUMENT(c) o) d WHERE d.a IS NULL",
        "INSERT INTO t SELECT o._id, o.name FROM DOCUMENT(people) o",
        "UPDATE t SET n = n + 1 WHERE id IN (SELECT CAST(k.value AS INT) FROM KV('ids:') k)",
        "SELECT NULL = 1, NULL + 1, 1 IN (NULL), CASE 1 WHEN 1 THEN 'a' END",
        "SELECT 1 = 'a', TRUE < 2, 1 IN (1, 'a')", // cross-type comparisons follow the total order
        "db.c.find({a: {$in: [1, 2]}}).sort({a: -1, b: 1})",
        "EXPLAIN ANALYZE db.c.count({x: 1})",
    ] {
        validate(src).unwrap_or_else(|e| panic!("{src:?} should be valid: {e}"));
    }
}

// ========================================================== invalid syntax

#[test]
fn syntax_errors_say_what_and_where() {
    for (src, msg, at) in [
        ("", "empty command", "(at line 1, column 1)"),
        ("SELEKT 1", "unknown command 'selekt'", "(at line 1, column 1)"),
        ("SELECT * FROM t WHERE", "unexpected end of statement", "(at line 1, column 22)"),
        ("SELECT a,\n  FROM t", "expected an expression, found keyword FROM", "(at line 2, column 3)"),
        ("SELECT (1 + 2", "expected ')', found end of statement", "(at line 1, column 14)"),
        ("SELECT 1 2", "unexpected 2 after the end of the statement", "(at line 1, column 10)"),
        ("SELECT 1; SELECT 2", "unexpected 'select' after the end of the statement", "(at line 1, column 11)"),
        ("INSERT INTO t VALUES 1", "expected '(', found 1", "(at line 1, column 22)"),
        ("SELECT * FROM foo(1)", "'foo(...)' is not a table source", ""),
        ("SELECT * FROM KV(1)", "KV() takes an optional key prefix string", ""),
        ("WITH x AS (SELECT 1) SELECT 1", "WITH (common table expressions) is not supported", "(at line 1, column 1)"),
        ("CREATE VIEW v AS SELECT 1", "CREATE VIEW is not supported", "(at line 1, column 8)"),
        ("SAVEPOINT a", "SAVEPOINT is not supported", ""),
        ("EXPLAIN EXPLAIN SELECT 1", "EXPLAIN cannot be nested", "(at line 1, column 9)"),
        ("EXPLAIN BEGIN", "EXPLAIN cannot be applied to a transaction statement", ""),
        ("EXPLAIN DROP TABLE t", "EXPLAIN cannot be applied", ""),
        ("EXPLAIN", "EXPLAIN needs a statement", ""),
        ("BEGIN NOW", "unexpected 'now' after the end of the statement", "(at line 1, column 7)"),
        ("START", "expected TRANSACTION, found end of statement", ""),
        ("SHOW USERS", "use SHOW TABLES or SHOW COLLECTIONS", "(at line 1, column 6)"),
        ("db.c.find({a: 1}", "expected ',' or ')', found end of statement", ""),
        ("db.c.find({a: })", "JSON: unexpected character", "(at line 1, column 15)"),
        ("db.c.find({}).sort({a: 1}).explode()", "unknown modifier .explode()", "(at line 1, column 28)"),
        ("db.c.find({}).count().limit(1)", "cannot follow .count() or .explain()", "(at line 1, column 23)"),
        ("db.c.distinct(5)", "distinct() takes a field name string", ""),
        ("db.c.aggregate({})", "aggregate() takes an array of stages", ""),
        ("db.c.insert({a: 1}).limit(1)", "insert() cannot be chained", "(at line 1, column 21)"),
        ("db.c.frobnicate({})", "unknown collection method 'frobnicate'", "(at line 1, column 6)"),
        ("db.c.find({}, {}, {})", "find() takes at most 2 argument(s), got 3", ""),
        ("db.c.find({}).limit(-1)", "expected a non-negative integer", ""),
        ("db.c", "expected '.', found end of statement", ""),
        ("db.c.insert()", "insert() needs a document", ""),
        ("SET k", "SET needs a key and a value", ""),
        ("GET", "GET needs a key", ""),
        ("GET a b", "unexpected extra argument 'b'", "(at line 1, column 7)"),
        ("SET k 'unterminated", "unterminated quoted string", "(at line 1, column 7)"),
        ("SET k 'v' tail", "unexpected extra argument 'tail'", ""),
        ("INCR k many", "INCR amount must be an integer", "(at line 1, column 8)"),
        ("DECR k -9223372036854775808", "DECR amount is out of range", ""),
        ("= 1", "a statement cannot start with '='", "(at line 1, column 1)"),
    ] {
        let m = parse_err(src);
        assert!(m.contains(msg) && m.contains(at), "{src:?}: {m}");
    }
}

#[test]
fn nesting_and_complexity_limits_still_apply_through_pbsql() {
    let deep = format!("SELECT {}1{}", "(".repeat(40), ")".repeat(40));
    assert!(parse_err(&deep).contains("nested deeper than 32 levels"));
    let long = format!("SELECT 0{}", "+1".repeat(150));
    assert!(parse_err(&long).contains("more than 100 operators"));
    let nested_paths = format!("SELECT o.a{}", ".b".repeat(5));
    assert!(parse(&nested_paths).is_ok());
}
