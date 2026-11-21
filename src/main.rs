use polydb::server::{self, Config};
use polydb::Database;
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::net::TcpStream;

const HELP: &str = "\
polydb - relational, document and key-value data in one engine

USAGE
  polydb [DIRECTORY]            local shell on a database directory (default ./polydb-data)
  polydb --memory               local shell, nothing touches disk
  polydb serve [DIRECTORY]      network server for many clients (see SERVER below)
  polydb client [HOST:PORT]     shell connected to a server (default 127.0.0.1:7878)

One command per line; in the interactive shell SQL/PBSQL statements end
with ';' and may span several lines. Commands can also be piped in on stdin
(a line that is visibly unfinished, or ends with '\\', continues on the next).

RELATIONAL (SQL)
  CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT)
  CREATE INDEX users_age ON users (age)
  INSERT INTO users VALUES (1, 'Ada', 36), (2, 'Linus', 54)
  SELECT name, age FROM users WHERE age > 40 ORDER BY age DESC LIMIT 10
  SELECT u.name, COUNT(*) FROM users u JOIN orders o ON o.user_id = u.id GROUP BY u.name
  UPDATE users SET age = age + 1 WHERE id = 1
  DELETE FROM users WHERE id = 2
  DROP TABLE users

DOCUMENT (NoSQL)
  db.events.insert({user: 1, type: 'login', tags: ['web']})
  db.events.createIndex({type: 1})
  db.events.find({type: 'login', user: {$gte: 1}}).sort({user: -1}).limit(5)
  db.events.update({user: 1}, {$set: {seen: true}, $inc: {visits: 1}})
  db.events.delete({type: 'login'})
  db.events.count({})      db.events.drop()

KEY-VALUE
  SET session:42 some value      GET session:42      DEL session:42
  EXISTS session:42              KEYS session:       INCR counter 5

PBSQL (one language for all three models)
  SELECT u.name, p.profile.city, s.value
    FROM RELATIONAL.users u
    JOIN DOCUMENT.profiles p ON p.user_id = u.id
    LEFT JOIN KV.sessions s ON s.key = CONCAT('session:', u.id);
  INSERT INTO DOCUMENT.users VALUES {name: 'Alice', profile: {city: 'Berlin'}};
  UPDATE DOCUMENT.users SET profile.city = 'Paris' WHERE name = 'Alice';
  SET KV 'user:1' = 'Alice';   GET FROM KV 'user:1';   EXISTS KV 'user:1';
  INCREMENT KV 'hits' BY 2;    SELECT key, value FROM KV WHERE key LIKE 'user:%';
  EXPLAIN SELECT ...;          EXPLAIN ANALYZE SELECT ...;
  ALTER TABLE users ADD COLUMN email TEXT UNIQUE;   CREATE INDEX i ON users (city, age);
  db.events.aggregate([{$match: {type: 'login'}}, {$group: {_id: '$user', n: {$sum: 1}}}])

TRANSACTIONS (span all three models, serializable)
  BEGIN   COMMIT   ROLLBACK

OTHER
  SHOW TABLES   SHOW COLLECTIONS   CHECKPOINT   .timing on|off   .help   .exit

SERVER
  polydb serve [DIRECTORY] [--memory] [--host ADDR]
               [--port N] [--redis-port N] [--pg-port N] [--mongo-port N]
               [--max-request-bytes N] [--max-connections N] [--mongo-default-db NAME]
  Defaults: host 127.0.0.1, native 7878, Redis 6380, PostgreSQL 5433, MongoDB 27018.
  Requests larger than --max-request-bytes (default 64 MiB) close the connection.
  A port of 'off' disables that listener. There is no authentication:
  only use --host 0.0.0.0 on a network you trust.
";

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

fn open(path: Option<&str>, memory: bool) -> Database {
    if memory {
        return Database::in_memory();
    }
    let path = path.unwrap_or("polydb-data");
    Database::open(path).unwrap_or_else(|e| die(&format!("cannot open database at '{path}': {e}")))
}

/// Is the buffered text visibly unfinished (open quote/bracket, trailing
/// backslash, or ending in a keyword or operator that needs more)?
fn incomplete(buf: &str) -> bool {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut chars = buf.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' && q == '"' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth -= 1,
                '-' if chars.peek() == Some(&'-') => {
                    // comment to end of line
                    for c in chars.by_ref() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                _ => {}
            },
        }
    }
    if quote.is_some() || depth > 0 {
        return true;
    }
    let t = buf.trim_end();
    if t.ends_with('\\') {
        return true;
    }
    if t.ends_with(';') {
        return false;
    }
    let last = t.rsplit(|c: char| c.is_whitespace()).next().unwrap_or("").to_ascii_lowercase();
    matches!(
        last.as_str(),
        "select" | "from" | "where" | "and" | "or" | "not" | "join" | "on" | "by" | "set" | "values" | "into" | "as"
            | "union" | "all" | "having" | "left" | "inner" | "cross" | "explain" | "analyze" | "then" | "else" | "when"
            | "case" | "in" | "like" | "between" | "," | "=" | "+" | "-" | "*" | "/" | "||" | "<" | ">" | "<=" | ">="
            | "<>" | "!="
    ) || last.ends_with(',')
}

const SQL_WORDS: &[&str] =
    &["select", "insert", "update", "delete", "create", "drop", "alter", "explain", "with", "increment"];

/// Read commands from stdin, hand each to `run`, print what comes back.
///
/// Interactively, SQL/PBSQL statements end with `;` and may span lines;
/// other commands end at the end of the line. In piped input every line is
/// a command unless it is visibly incomplete (see [`incomplete`]).
fn repl(prompt: &mut dyn FnMut() -> &'static str, run: &mut dyn FnMut(&str) -> Result<String, String>) {
    let interactive = io::stdin().is_terminal();
    if interactive {
        println!("polydb {} - type .help for commands, .exit to quit", env!("CARGO_PKG_VERSION"));
    }
    let stdin = io::stdin();
    let mut line = String::new();
    let mut buf = String::new();
    let mut timing = false;
    loop {
        if interactive {
            print!("{}", if buf.is_empty() { prompt() } else { "   ...> " });
            let _ = io::stdout().flush();
        }
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let piece = line.trim();
        if buf.is_empty() && (piece.is_empty() || piece.starts_with("--")) {
            continue;
        }
        if buf.is_empty() && piece.starts_with('.') || buf.is_empty() && matches!(piece, "exit" | "quit" | "help") {
            match piece {
                ".exit" | ".quit" | "exit" | "quit" => break,
                ".help" | "help" => print!("{HELP}"),
                ".timing on" => timing = true,
                ".timing off" => timing = false,
                other => println!("error: unknown shell command '{other}' (try .help)"),
            }
            continue;
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(piece.strip_suffix('\\').unwrap_or(piece));
        let first = buf.split(|c: char| !c.is_alphanumeric()).next().unwrap_or("").to_ascii_lowercase();
        let needs_semicolon = interactive && SQL_WORDS.contains(&first.as_str());
        let done = !incomplete(&buf) && !piece.ends_with('\\') && (!needs_semicolon || buf.trim_end().ends_with(';'));
        if !done {
            continue;
        }
        let cmd = std::mem::take(&mut buf);
        let start = std::time::Instant::now();
        match run(cmd.trim()) {
            Ok(out) => println!("{out}"),
            Err(e) => println!("error: {e}"),
        }
        if timing {
            println!("Time: {:.3} ms", start.elapsed().as_secs_f64() * 1e3);
        }
    }
    if !buf.trim().is_empty() {
        match run(buf.trim()) {
            Ok(out) => println!("{out}"),
            Err(e) => println!("error: {e}"),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let first = args.first().map(String::as_str);
    if matches!(first, Some("--help" | "-h" | "help")) {
        print!("{HELP}");
        return;
    }

    if first == Some("serve") {
        let mut cfg = Config::default();
        let mut limits = server::Limits::default();
        let mut dir = None;
        let mut memory = false;
        let mut it = args[1..].iter();
        while let Some(a) = it.next() {
            let mut value = |name: &str| it.next().cloned().unwrap_or_else(|| die(&format!("{name} needs a value")));
            let port = |v: String| -> Option<u16> {
                if v == "off" {
                    None
                } else {
                    Some(v.parse().unwrap_or_else(|_| die(&format!("'{v}' is not a port number"))))
                }
            };
            match a.as_str() {
                "--memory" => memory = true,
                "--host" => cfg.host = value("--host"),
                "--port" => cfg.native = port(value("--port")),
                "--redis-port" => cfg.redis = port(value("--redis-port")),
                "--pg-port" => cfg.postgres = port(value("--pg-port")),
                "--mongo-port" => cfg.mongo = port(value("--mongo-port")),
                "--max-request-bytes" => {
                    let v = value("--max-request-bytes");
                    limits.max_request_bytes =
                        v.parse().ok().filter(|n| *n >= 1024).unwrap_or_else(|| die(&format!("'{v}' is not a byte count >= 1024")));
                }
                "--mongo-default-db" => limits.mongo_default_db = value("--mongo-default-db"),
                "--max-connections" => {
                    let v = value("--max-connections");
                    limits.max_connections =
                        v.parse().ok().filter(|n| *n >= 1).unwrap_or_else(|| die(&format!("'{v}' is not a positive number")));
                }
                other if other.starts_with("--") => die(&format!("unknown option {other}; see polydb --help")),
                other => dir = Some(other.to_string()),
            }
        }
        let db = open(dir.as_deref(), memory);
        println!("polydb {} serving {}", env!("CARGO_PKG_VERSION"), if memory { "an in-memory database".into() } else { format!("'{}'", dir.as_deref().unwrap_or("polydb-data")) });
        if let Err(e) = server::serve_with_limits(&db, &cfg, limits) {
            die(&format!("{e}"));
        }
        return;
    }

    if first == Some("client") {
        let addr = args.get(1).map(String::as_str).unwrap_or("127.0.0.1:7878");
        let stream = TcpStream::connect(addr).unwrap_or_else(|e| die(&format!("cannot connect to {addr}: {e}")));
        let mut writer = stream.try_clone().unwrap_or_else(|e| die(&format!("{e}")));
        let mut reader = BufReader::new(stream);
        repl(&mut || "polydb> ", &mut |cmd| {
            writer.write_all(format!("{cmd}\n").as_bytes()).map_err(|e| e.to_string())?;
            let mut reply = String::new();
            match reader.read_line(&mut reply) {
                Ok(0) => Err("server closed the connection".into()),
                Err(e) => Err(e.to_string()),
                Ok(_) => server::result_from_json(reply.trim()).map(|o| o.to_string()),
            }
        });
        return;
    }

    let db = std::cell::RefCell::new(open(first.filter(|a| *a != "--memory"), first == Some("--memory")));
    repl(&mut || if db.borrow().in_transaction() { "polydb*> " } else { "polydb> " }, &mut |cmd| {
        db.borrow_mut().execute(cmd).map(|o| o.to_string()).map_err(|e| e.to_string())
    });
    if db.borrow().in_transaction() {
        eprintln!("warning: open transaction rolled back at exit");
    }
}
