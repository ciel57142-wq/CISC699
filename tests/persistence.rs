//! Durability across process restarts.
//!
//! Every test here writes through one `polydb` process, lets it exit (or
//! kills it), and reads back through a *different* process, so nothing can
//! survive in the test's own memory. Library calls are used only to set up
//! damaged directories or to inspect files.

use polydb::Database;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_polydb");

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("polydb-persist-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    let _ = fs::remove_file(&p);
    p
}

struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Run {
    /// stdout lines with trailing padding removed.
    fn lines(&self) -> Vec<String> {
        self.stdout
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect()
    }

    fn has(&self, line: &str) -> bool {
        self.lines().iter().any(|l| l.trim() == line)
    }

    fn assert_ok(&self) -> &Self {
        assert_eq!(
            self.code,
            Some(0),
            "polydb exited with {:?}\nstdout:\n{}\nstderr:\n{}",
            self.code,
            self.stdout,
            self.stderr
        );
        self
    }

    fn assert_no_errors(&self) -> &Self {
        self.assert_ok();
        assert!(
            !self.stdout.contains("error:"),
            "a command failed:\n{}",
            self.stdout
        );
        self
    }
}

/// Run the polydb binary as a new process with `args` in `cwd`, piping `script` to it.
fn polydb_in(cwd: &Path, args: &[&str], script: &str) -> Run {
    let mut child = Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn polydb");
    let mut input = child.stdin.take().unwrap();
    let data = script.to_string();
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(data.as_bytes());
    });
    let out = child.wait_with_output().expect("wait for polydb");
    let _ = writer.join();
    Run {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// Run the shell on database directory `dir` in a fresh process.
fn polydb(dir: &Path, script: &str) -> Run {
    polydb_in(&std::env::temp_dir(), &[dir.to_str().unwrap()], script)
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

// --------------------------------------------------------- the three models

#[test]
fn relational_rows_survive_process_restart() {
    let dir = tmp("rel");
    polydb(
        &dir,
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT)\n\
         INSERT INTO users VALUES (1, 'Ada', 36), (2, 'Linus', 54), (3, 'Grace', 85)\n\
         UPDATE users SET age = age + 1 WHERE id = 1\n\
         DELETE FROM users WHERE id = 2\n",
    )
    .assert_no_errors();

    let r = polydb(&dir, "SELECT id, name, age FROM users ORDER BY id\n");
    r.assert_no_errors();
    assert_eq!(
        r.lines(),
        vec![
            " id | name  | age",
            "----+-------+-----",
            " 1  | Ada   | 37",
            " 3  | Grace | 85",
            "(2 row(s))"
        ],
        "stdout:\n{}",
        r.stdout
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn documents_survive_process_restart() {
    let dir = tmp("doc");
    polydb(
        &dir,
        "db.events.insert({_id: 1, user: 1, type: 'login', tags: ['web']})\n\
         db.events.insert({_id: 2, user: 2, type: 'signup'})\n\
         db.events.insert({user: 3, type: 'generated-id'})\n\
         db.events.update({_id: 1}, {$set: {seen: true}, $inc: {visits: 2}})\n\
         db.events.delete({_id: 2})\n",
    )
    .assert_no_errors();

    let r = polydb(
        &dir,
        "db.events.find({_id: 1})\ndb.events.count({})\ndb.events.count({type: 'generated-id'})\nSHOW COLLECTIONS\n",
    );
    r.assert_no_errors();
    assert!(
        r.has(r#"{"_id":1,"seen":true,"tags":["web"],"type":"login","user":1,"visits":2}"#),
        "stdout:\n{}",
        r.stdout
    );
    let l = r.lines();
    assert_eq!(l[2], "2", "two documents remain: {:?}", l);
    assert_eq!(
        l[3], "1",
        "the document with a generated _id survived: {:?}",
        l
    );
    assert!(r.has("events"));

    // Generated ids keep counting after a restart instead of reusing values.
    polydb(&dir, "db.events.insert({user: 4, type: 'generated-id'})\n").assert_no_errors();
    let r = polydb(&dir, "db.events.count({type: 'generated-id'})\n");
    assert_eq!(r.lines(), vec!["2"]);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn key_value_entries_survive_process_restart() {
    let dir = tmp("kv");
    polydb(
        &dir,
        "SET session:42 abc\nSET greeting \"hello\\tworld\"\nINCR visits 5\nINCR visits\nSET gone 1\nDEL gone\n",
    )
    .assert_no_errors();

    let r = polydb(
        &dir,
        "GET session:42\nGET greeting\nGET visits\nEXISTS gone\nKEYS\n",
    );
    r.assert_no_errors();
    let l = r.lines();
    assert_eq!(
        &l[..4],
        &["abc", "hello\tworld", "6", "false"],
        "stdout:\n{}",
        r.stdout
    );
    assert!(r.has("session:42") && r.has("visits") && !r.has("gone"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn schemas_survive_process_restart() {
    let dir = tmp("schema");
    polydb(&dir, "CREATE TABLE items (id INT PRIMARY KEY, name TEXT NOT NULL, qty INT)\nCREATE TABLE empty_one (k TEXT PRIMARY KEY)\n")
        .assert_no_errors();

    let r = polydb(
        &dir,
        "SHOW TABLES\n\
         CREATE TABLE items (id INT PRIMARY KEY)\n\
         INSERT INTO items VALUES (1, NULL, 1)\n\
         INSERT INTO items VALUES (1, 'ok', 'not a number')\n\
         INSERT INTO items VALUES (1, 'ok', 3)\n\
         INSERT INTO items VALUES (1, 'dup', 4)\n\
         SELECT * FROM empty_one\n",
    );
    r.assert_ok();
    let l = r.lines();
    assert!(
        r.has("items") && r.has("empty_one"),
        "both tables listed:\n{}",
        r.stdout
    );
    let errors: Vec<&String> = l.iter().filter(|l| l.starts_with("error:")).collect();
    assert_eq!(
        errors.len(),
        4,
        "expected 4 schema-driven errors:\n{}",
        r.stdout
    );
    assert!(errors[0].contains("already exists"), "{}", errors[0]);
    assert!(
        errors[1].contains("constraint"),
        "NOT NULL kept: {}",
        errors[1]
    );
    assert!(
        errors[2].contains("type"),
        "column type kept: {}",
        errors[2]
    );
    assert!(
        errors[3].contains("duplicate primary key"),
        "primary key kept: {}",
        errors[3]
    );
    assert!(r.has("1 row(s) inserted"));
    assert!(
        r.has(" k") || r.has("k"),
        "empty table keeps its columns:\n{}",
        r.stdout
    );

    // DROP TABLE is durable too.
    polydb(&dir, "DROP TABLE empty_one\n").assert_no_errors();
    let r = polydb(&dir, "SHOW TABLES\n");
    assert!(r.has("items") && !r.has("empty_one"), "{}", r.stdout);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn indexes_survive_process_restart_and_stay_correct() {
    let dir = tmp("index");
    polydb(
        &dir,
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, city TEXT)\n\
         INSERT INTO users VALUES (1, 'Ada', 'London'), (2, 'Linus', 'Portland'), (3, 'Grace', 'London')\n\
         CREATE INDEX users_city ON users (city)\n\
         db.people.insert({_id: 1, email: 'a@x', team: 'red'})\n\
         db.people.insert({_id: 2, email: 'b@x', team: 'blue'})\n\
         db.people.createIndex({team: 1})\n",
    )
    .assert_no_errors();

    // The index definitions are persisted metadata ...
    let r = polydb(
        &dir,
        "CREATE INDEX users_city ON users (city)\ndb.people.createIndex({team: 1})\n",
    );
    assert!(
        r.stdout.contains("index 'users_city' already exists"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("team) is already indexed"),
        "{}",
        r.stdout
    );

    // ... and index entries are maintained by writes made in a later process.
    polydb(
        &dir,
        "UPDATE users SET city = 'Paris' WHERE id = 3\n\
         INSERT INTO users VALUES (4, 'Alan', 'London')\n\
         db.people.update({_id: 2}, {$set: {team: 'red'}})\n\
         db.people.insert({_id: 3, email: 'c@x', team: 'green'})\n",
    )
    .assert_no_errors();

    let r = polydb(
        &dir,
        "SELECT id FROM users WHERE city = 'London' ORDER BY id\n\
         SELECT id FROM users WHERE city = 'Paris'\n\
         db.people.count({team: 'red'})\n\
         db.people.count({team: 'blue'})\n\
         db.people.count({team: 'green'})\n",
    );
    r.assert_no_errors();
    assert_eq!(
        r.lines(),
        vec![
            " id",
            "----",
            " 1",
            " 4",
            "(2 row(s))", //
            " id",
            "----",
            " 3",
            "(1 row(s))", //
            "2",
            "0",
            "1",
        ],
        "stdout:\n{}",
        r.stdout
    );

    // DROP INDEX is durable.
    polydb(&dir, "DROP INDEX users_city ON users\n").assert_no_errors();
    let r = polydb(&dir, "CREATE INDEX users_city ON users (city)\nSELECT COUNT(*) FROM users WHERE city = 'London'\n");
    r.assert_no_errors();
    assert!(
        r.has("index 'users_city' created on users(city)"),
        "{}",
        r.stdout
    );
    assert!(r.has("2"), "rebuilt index is complete:\n{}", r.stdout);
    fs::remove_dir_all(&dir).unwrap();
}

// ------------------------------------------------------- reopening, modes

#[test]
fn reopening_many_times_preserves_state() {
    let dir = tmp("reopen");
    polydb(&dir, "CREATE TABLE log (n INT PRIMARY KEY)\n").assert_no_errors();
    for i in 1..=6 {
        // Process i sees exactly the i-1 rows of earlier processes, then adds one.
        let script = format!(
            "SELECT COUNT(*) FROM log\nINSERT INTO log VALUES ({i})\nINCR runs\ndb.runs.insert({{_id: {i}}})\n{}",
            if i == 3 { "CHECKPOINT\n" } else { "" }
        );
        let r = polydb(&dir, &script);
        r.assert_no_errors();
        assert_eq!(
            r.lines()[2],
            format!(" {}", i - 1),
            "run {i}:\n{}",
            r.stdout
        );
    }
    let r = polydb(
        &dir,
        "SELECT COUNT(*) FROM log\nGET runs\ndb.runs.count({})\n",
    );
    let l = r.lines();
    assert_eq!(
        (l[2].trim(), l[4].as_str(), l[5].as_str()),
        ("6", "6", "6"),
        "{}",
        r.stdout
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn committed_transactions_survive_and_open_ones_do_not() {
    let dir = tmp("txn");
    polydb(
        &dir,
        "CREATE TABLE t (id INT PRIMARY KEY)\n\
         BEGIN\nINSERT INTO t VALUES (1)\ndb.c.insert({_id: 1})\nSET k committed\nCOMMIT\n\
         BEGIN\nINSERT INTO t VALUES (2)\nSET k rolled-back\nROLLBACK\n\
         BEGIN\nINSERT INTO t VALUES (3)\ndb.c.insert({_id: 3})\nSET k never-committed\n",
    )
    .assert_no_errors();
    let r = polydb(&dir, "SELECT id FROM t\ndb.c.count({})\nGET k\n");
    assert_eq!(
        r.lines(),
        vec![" id", "----", " 1", "(1 row(s))", "1", "committed"],
        "{}",
        r.stdout
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn default_launch_is_disk_backed() {
    let cwd = tmp("default-cwd");
    fs::create_dir_all(&cwd).unwrap();
    polydb_in(
        &cwd,
        &[],
        "SET location disk\nCREATE TABLE t (id INT PRIMARY KEY)\nINSERT INTO t VALUES (7)\n",
    )
    .assert_no_errors();
    let data = cwd.join("polydb-data");
    let files = file_names(&data);
    assert!(
        files.contains(&"data.wal".to_string()) && files.contains(&"polydb.format".to_string()),
        "{files:?}"
    );
    assert!(
        fs::metadata(data.join("data.wal")).unwrap().len() > 0,
        "the commits are in the log"
    );

    let r = polydb_in(&cwd, &[], "GET location\nSELECT * FROM t\n");
    r.assert_no_errors();
    assert!(r.has("disk") && r.has("7"), "{}", r.stdout);
    fs::remove_dir_all(&cwd).unwrap();
}

#[test]
fn memory_mode_creates_no_files() {
    let cwd = tmp("memory-cwd");
    fs::create_dir_all(&cwd).unwrap();
    let r = polydb_in(
        &cwd,
        &["--memory"],
        "CREATE TABLE t (id INT PRIMARY KEY)\nINSERT INTO t VALUES (1)\ndb.c.insert({_id: 1})\nSET k v\nCHECKPOINT\nGET k\n",
    );
    r.assert_no_errors();
    assert!(r.has("v"));
    assert_eq!(
        file_names(&cwd),
        Vec::<String>::new(),
        "--memory wrote files"
    );

    // Nothing carries over between memory-mode processes.
    let r = polydb_in(&cwd, &["--memory"], "GET k\nSHOW TABLES\n");
    assert_eq!(r.lines()[0], "NULL");
    assert_eq!(file_names(&cwd), Vec::<String>::new());

    // The server's --memory mode is file-free too.
    let (mut child, addr) = spawn_server(&cwd, &["--memory"]);
    assert!(ask(addr, "SET k v").contains("\"ok\":true"));
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        file_names(&cwd),
        Vec::<String>::new(),
        "serve --memory wrote files"
    );

    // --memory with a directory is ambiguous and refused, not silently volatile.
    let r = polydb_in(&cwd, &["somewhere", "--memory"], "");
    assert_eq!(r.code, Some(1));
    assert!(!cwd.join("somewhere").exists());
    fs::remove_dir_all(&cwd).unwrap();
}

#[test]
fn automatic_checkpoints_keep_data_without_manual_checkpoint() {
    let dir = tmp("autockpt");
    {
        let mut db = Database::open(&dir).unwrap();
        db.set_checkpoint_threshold(4096);
        db.execute("CREATE TABLE t (id INT PRIMARY KEY, pad TEXT)")
            .unwrap();
        for i in 0..300 {
            db.execute(&format!("INSERT INTO t VALUES ({i}, '{}')", "x".repeat(64)))
                .unwrap();
        }
        db.execute("SET last 299").unwrap();
    }
    assert!(
        dir.join("data.snap").exists(),
        "the log passed the threshold, so a snapshot was written"
    );
    assert!(
        fs::metadata(dir.join("data.wal")).unwrap().len() < 8192,
        "and the log was emptied"
    );
    let r = polydb(&dir, "SELECT COUNT(*) FROM t\nGET last\n");
    r.assert_no_errors();
    assert_eq!(
        (r.lines()[2].trim(), r.lines()[4].as_str()),
        ("300", "299"),
        "{}",
        r.stdout
    );
    fs::remove_dir_all(&dir).unwrap();
}

// --------------------------------------------------- crash and kill -9

/// Start `polydb serve` with the native port on 0 (chosen by the OS, so
/// parallel tests cannot collide) and return the address it reports.
// The child is handed to the caller, which kills and waits for it.
#[allow(clippy::zombie_processes)]
fn spawn_server(cwd: &Path, args: &[&str]) -> (Child, SocketAddr) {
    let mut child = Command::new(BIN)
        .arg("serve")
        .args(args)
        .args([
            "--port",
            "0",
            "--redis-port",
            "off",
            "--pg-port",
            "off",
            "--mongo-port",
            "off",
        ])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        if out.read_line(&mut line).unwrap_or(0) == 0 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("server exited before listening");
        }
        if let Some(rest) = line.trim().strip_prefix("native") {
            let addr = rest
                .trim()
                .trim_start_matches("listening on")
                .trim()
                .parse()
                .unwrap();
            // Keep draining stdout so the server can never block on a full pipe.
            std::thread::spawn(move || std::io::copy(&mut out, &mut std::io::sink()));
            return (child, addr);
        }
    }
}

fn serve(dir: &Path) -> (Child, SocketAddr) {
    spawn_server(&std::env::temp_dir(), &[dir.to_str().unwrap()])
}

fn ask(addr: SocketAddr, cmd: &str) -> String {
    let stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut w = stream.try_clone().unwrap();
    w.write_all(format!("{cmd}\n").as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    line
}

#[test]
fn killed_server_keeps_every_acknowledged_write_of_all_models() {
    let dir = tmp("kill9");
    for round in 0..3 {
        let (mut child, addr) = serve(&dir);
        if round == 0 {
            assert!(
                ask(addr, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").contains("\"ok\":true")
            );
            assert!(ask(addr, "CREATE INDEX t_v ON t (v)").contains("\"ok\":true"));
            assert!(ask(addr, "db.c.createIndex({tag: 1})").contains("\"ok\":true"));
        }
        for i in 0..20 {
            let id = round * 100 + i;
            assert!(
                ask(addr, &format!("INSERT INTO t VALUES ({id}, 'r{round}')"))
                    .contains("\"ok\":true")
            );
            assert!(ask(
                addr,
                &format!("db.c.insert({{_id: {id}, tag: 'r{round}'}})")
            )
            .contains("\"ok\":true"));
            assert!(ask(addr, &format!("SET k{id} {id}")).contains("\"ok\":true"));
        }
        child.kill().unwrap(); // SIGKILL: no shutdown code runs
        child.wait().unwrap();
    }
    let r = polydb(
        &dir,
        "SELECT COUNT(*) FROM t\nSELECT COUNT(*) FROM t WHERE v = 'r1'\ndb.c.count({tag: 'r2'})\nGET k219\nKEYS k\n",
    );
    r.assert_no_errors();
    let l = r.lines();
    assert_eq!(
        (l[2].trim(), l[6].trim(), l[8].as_str(), l[9].as_str()),
        ("60", "20", "20", "219"),
        "{}",
        r.stdout
    );
    assert!(r.has("(60 row(s))"), "{}", r.stdout);
    fs::remove_dir_all(&dir).unwrap();
}

// ------------------------------------------- safe initialisation and opening

#[test]
fn a_second_process_cannot_open_a_database_in_use() {
    let dir = tmp("locked");
    let (mut child, addr) = serve(&dir);
    assert!(ask(addr, "SET owner server").contains("\"ok\":true"));

    let r = polydb(&dir, "SET owner intruder\n");
    assert_eq!(r.code, Some(1), "{}", r.stdout);
    assert!(
        r.stderr.contains("locked by another process"),
        "{}",
        r.stderr
    );
    assert!(Database::open(&dir).is_err());

    child.kill().unwrap();
    child.wait().unwrap();
    // The lock dies with the process; the data is the server's.
    let r = polydb(&dir, "GET owner\n");
    assert_eq!(r.lines(), vec!["server"]);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn never_initialises_over_a_directory_with_other_files() {
    let dir = tmp("foreign");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("notes.txt"), b"precious").unwrap();
    let r = polydb(&dir, "SET k v\n");
    assert_eq!(r.code, Some(1));
    assert!(r.stderr.contains("not a polydb database"), "{}", r.stderr);
    assert_eq!(file_names(&dir), vec!["LOCK", "notes.txt"]);
    assert_eq!(fs::read(dir.join("notes.txt")).unwrap(), b"precious");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_missing_log_is_an_error_not_an_empty_database() {
    let dir = tmp("nowal");
    polydb(&dir, "SET k v\n").assert_no_errors();
    fs::remove_file(dir.join("data.wal")).unwrap();
    let r = polydb(&dir, "GET k\n");
    assert_eq!(r.code, Some(1));
    assert!(
        r.stderr.contains("data.wal") && r.stderr.contains("missing"),
        "{}",
        r.stderr
    );
    assert!(
        !dir.join("data.wal").exists(),
        "a refused open must not create a fresh log"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_missing_snapshot_after_checkpoint_is_an_error() {
    let dir = tmp("nosnap");
    polydb(&dir, "SET k v\nCHECKPOINT\n").assert_no_errors();
    assert!(fs::read_to_string(dir.join("polydb.format"))
        .unwrap()
        .contains("snapshot=1"));
    fs::remove_file(dir.join("data.snap")).unwrap();
    let r = polydb(&dir, "GET k\n");
    assert_eq!(r.code, Some(1));
    assert!(r.stderr.contains("data.snap"), "{}", r.stderr);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_newer_format_version_is_refused() {
    let dir = tmp("future");
    polydb(&dir, "SET k v\n").assert_no_errors();
    fs::write(
        dir.join("polydb.format"),
        "polydb-storage\nversion=99\nsnapshot=0\n",
    )
    .unwrap();
    let r = polydb(&dir, "GET k\n");
    assert_eq!(r.code, Some(1));
    assert!(r.stderr.contains("format 99"), "{}", r.stderr);
    fs::remove_dir_all(&dir).unwrap();
}

/// CRC-32 (IEEE), as used by both on-disk formats.
fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// One commit in the format-1 record layout that polydb 0.2.0 wrote:
/// `len | crc32(payload) | count | { tag | klen | key | vlen | value }*`.
fn v1_record(entries: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut payload = (entries.len() as u32).to_le_bytes().to_vec();
    for (k, v) in entries {
        payload.push(1);
        payload.extend_from_slice(&(k.len() as u32).to_le_bytes());
        payload.extend_from_slice(k);
        payload.extend_from_slice(&(v.len() as u32).to_le_bytes());
        payload.extend_from_slice(v);
    }
    let mut out = (payload.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&crc32(&payload).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

#[test]
fn directories_from_polydb_0_2_0_open_unchanged() {
    // 0.2.0 wrote only data.wal / data.snap in the format-1 record layout and
    // no format file. Build such a directory byte for byte, holding exactly
    // the entries the current engine stores for two keys.
    let src = tmp("legacy-src");
    polydb(&src, "SET before checkpoint\nSET after checkpoint\n").assert_no_errors();
    let entries: Vec<(Vec<u8>, Vec<u8>)> = {
        let store = polydb::storage::Store::open(&src).unwrap();
        store
            .scan(b"")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    let is_after = |k: &[u8]| k.windows(5).any(|w| w == b"after");
    let (logged, snapped): (Vec<_>, Vec<_>) = entries.into_iter().partition(|(k, _)| is_after(k));
    assert!(!logged.is_empty() && !snapped.is_empty());
    let dir = tmp("legacy");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("data.snap"), v1_record(&snapped)).unwrap();
    fs::write(dir.join("data.wal"), v1_record(&logged)).unwrap();
    assert_eq!(file_names(&dir), vec!["data.snap", "data.wal"]);
    let wal_before = fs::read(dir.join("data.wal")).unwrap();

    let r = polydb(&dir, "GET before\nGET after\n");
    assert_eq!(r.lines(), vec!["checkpoint", "checkpoint"], "{}", r.stdout);
    assert_eq!(
        fs::read(dir.join("data.wal")).unwrap(),
        wal_before,
        "the log is not rewritten"
    );
    let format = fs::read_to_string(dir.join("polydb.format")).unwrap();
    assert!(
        format.starts_with("polydb-storage\nversion=1\n") && format.contains("snapshot=1"),
        "{format}"
    );

    // The first write converts the directory to the current format.
    polydb(&dir, "SET third write\n").assert_no_errors();
    let format = fs::read_to_string(dir.join("polydb.format")).unwrap();
    assert!(
        format.starts_with("polydb-storage\nversion=2\n"),
        "{format}"
    );
    for f in ["data.snap", "data.wal"] {
        assert_eq!(
            &fs::read(dir.join(f)).unwrap()[..2],
            &[0xD7, 0xB1],
            "{f} is in format 2"
        );
    }
    let r = polydb(&dir, "GET before\nGET after\nGET third\n");
    assert_eq!(
        r.lines(),
        vec!["checkpoint", "checkpoint", "write"],
        "{}",
        r.stdout
    );
    fs::remove_dir_all(&dir).unwrap();
    fs::remove_dir_all(&src).unwrap();
}

#[test]
fn an_interrupted_final_commit_is_discarded_and_reported() {
    let dir = tmp("torn");
    polydb(&dir, "SET a 1\nSET b 2\n").assert_no_errors();
    let wal = dir.join("data.wal");
    let good = fs::read(&wal).unwrap();
    // Half of a third commit record, as a crash during the append would leave it.
    let mut torn = good.clone();
    torn.extend_from_slice(&[40, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef, 1, 0, 0]);
    fs::write(&wal, &torn).unwrap();

    let r = polydb(&dir, "GET a\nGET b\n");
    r.assert_no_errors();
    assert_eq!(r.lines(), vec!["1", "2"]);
    assert!(r.stderr.contains("discarded 11 byte(s)"), "{}", r.stderr);
    assert_eq!(fs::read(&wal).unwrap(), good);
    fs::remove_dir_all(&dir).unwrap();
}

/// SIGKILL the server while a client is in the middle of a stream of writes.
/// Every write that was acknowledged must be there after restart; at most the
/// one in flight at the moment of the kill may be missing or present.
#[test]
fn kill_during_a_write_stream_loses_no_acknowledged_write() {
    let dir = tmp("kill-mid");
    let (mut child, addr) = serve(&dir);
    assert!(ask(addr, "CREATE TABLE t (id INT PRIMARY KEY)").contains("\"ok\":true"));

    let writer = std::thread::spawn(move || {
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut w = stream.try_clone().unwrap();
        let mut r = BufReader::new(stream);
        let mut acked = 0u32;
        for id in 1.. {
            if w.write_all(format!("INSERT INTO t VALUES ({id})\n").as_bytes())
                .is_err()
            {
                break;
            }
            let mut line = String::new();
            match r.read_line(&mut line) {
                Ok(n) if n > 0 && line.contains("\"ok\":true") => acked = id,
                _ => break,
            }
        }
        acked
    });
    std::thread::sleep(Duration::from_millis(400));
    child.kill().unwrap();
    child.wait().unwrap();
    let acked = writer.join().unwrap();
    assert!(acked > 0, "no write was acknowledged before the kill");

    let r = polydb(&dir, "SELECT COUNT(*) FROM t\nSELECT MAX(id) FROM t\n");
    r.assert_no_errors();
    let count: u32 = r.lines()[2].trim().parse().unwrap();
    let max: u32 = r.lines()[6].trim().parse().unwrap();
    assert_eq!(
        count, max,
        "ids 1..=max are all present, no holes: {}",
        r.stdout
    );
    assert!(
        count == acked || count == acked + 1,
        "acked {acked}, recovered {count}"
    );
    fs::remove_dir_all(&dir).unwrap();
}
