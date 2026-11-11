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

One command per line. Commands can also be piped in on stdin.

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

TRANSACTIONS (span all three models, serializable)
  BEGIN   COMMIT   ROLLBACK

OTHER
  SHOW TABLES   SHOW COLLECTIONS   CHECKPOINT   .help   .exit

SERVER
  polydb serve [DIRECTORY] [--memory] [--host ADDR]
               [--port N] [--redis-port N] [--pg-port N] [--mongo-port N]
  Defaults: host 127.0.0.1, native 7878, Redis 6380, PostgreSQL 5433, MongoDB 27018.
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

/// Read commands from stdin, hand each to `run`, print what comes back.
fn repl(prompt: &mut dyn FnMut() -> &'static str, run: &mut dyn FnMut(&str) -> Result<String, String>) {
    let interactive = io::stdin().is_terminal();
    if interactive {
        println!("polydb {} - type .help for commands, .exit to quit", env!("CARGO_PKG_VERSION"));
    }
    let stdin = io::stdin();
    let mut line = String::new();
    loop {
        if interactive {
            print!("{}", prompt());
            let _ = io::stdout().flush();
        }
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let cmd = line.trim();
        if cmd.is_empty() || cmd.starts_with("--") {
            continue;
        }
        match cmd {
            ".exit" | ".quit" | "exit" | "quit" => break,
            ".help" | "help" => print!("{HELP}"),
            _ => match run(cmd) {
                Ok(out) => println!("{out}"),
                Err(e) => println!("error: {e}"),
            },
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
                other if other.starts_with("--") => die(&format!("unknown option {other}; see polydb --help")),
                other => dir = Some(other.to_string()),
            }
        }
        let db = open(dir.as_deref(), memory);
        println!("polydb {} serving {}", env!("CARGO_PKG_VERSION"), if memory { "an in-memory database".into() } else { format!("'{}'", dir.as_deref().unwrap_or("polydb-data")) });
        if let Err(e) = server::serve(&db, &cfg) {
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
