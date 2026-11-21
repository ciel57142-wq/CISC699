//! The shell: multi-line statements, PBSQL and EXPLAIN through the binary.

use std::io::Write;
use std::process::{Command, Stdio};

fn run(stdin: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_polydb"))
        .arg("--memory")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn polydb");
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn multiline_statements_pbsql_and_explain() {
    let out = run(concat!(
        "CREATE TABLE t (id INT PRIMARY KEY,\n",
        "  v TEXT)\n",
        "INSERT INTO t VALUES (1, 'a'),\n",
        "  (2, 'b')\n",
        "SELECT v FROM t WHERE\n",
        "  id = 2\n",
        "SELECT COUNT(*) \\\n",
        "FROM t\n",
        "INSERT INTO DOCUMENT.d VALUES {\"_id\": 1,\n",
        "  \"x\": {\"y\": 5}}\n",
        "SELECT x.y FROM DOCUMENT.d\n",
        "EXPLAIN SELECT v FROM t WHERE id = 1\n",
        "SELEKT 1\n",
        "GET FROM KV 'missing'\n",
    ));
    assert!(out.contains("table 't' created"), "{out}");
    assert!(out.contains("2 row(s) inserted"), "{out}");
    assert!(out.contains("\n b\n"), "{out}");
    assert!(out.contains("1 document(s) inserted"), "{out}");
    assert!(out.contains("\n 5  \n"), "{out}");
    assert!(out.contains("PrimaryKeyLookup on t"), "{out}");
    assert!(out.contains("error: parse error"), "{out}");
    assert!(out.trim_end().ends_with("NULL"), "{out}");
}
