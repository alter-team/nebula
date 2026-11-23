//! Drives `nebula motherapp serve` over stdio the way motherapp's core does,
//! against a temporary NEBULA_HOME: describe, state.get, a run that appears,
//! its output through `view`, the notification when it finishes, and the
//! exit when stdin closes. Runs are made with the real CLI (`nebula run
//! --local --detach`, `nebula go`); no daemon is needed for local runs.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const NEBULA: &str = env!("CARGO_BIN_EXE_nebula");
const OUTPUT_MARK: &str = "e2e-output-4711";
const SCRIPT_MARK: &str = "e2e-script-marker";

struct Serve {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    /// Everything the tool sent that was not a reply to a request.
    pushed: Vec<Value>,
    next_id: u64,
}

impl Serve {
    fn start(home: &Path) -> Serve {
        let mut child = Command::new(NEBULA)
            .args(["motherapp", "serve"])
            .env("NEBULA_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("cannot start nebula motherapp serve");
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Serve {
            stdin: child.stdin.take(),
            child,
            lines,
            pushed: Vec::new(),
            next_id: 0,
        }
    }

    /// The next message, which must be one JSON-RPC 2.0 object on one line.
    fn next(&mut self, wait: Duration) -> Option<Value> {
        let line = self.lines.recv_timeout(wait).ok()?;
        let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert_eq!(v["jsonrpc"], json!("2.0"), "{line}");
        Some(v)
    }

    fn request(&mut self, method: &str, params: Option<Value>) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let mut msg = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(p) = params {
            msg["params"] = p;
        }
        writeln!(self.stdin.as_mut().unwrap(), "{msg}").unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let v = self
                .next(left)
                .unwrap_or_else(|| panic!("no reply to {method}"));
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
            assert!(v.get("id").is_none(), "a reply nobody asked for: {v}");
            self.pushed.push(v);
        }
    }

    fn action(&mut self, name: &str, args: Value) -> Value {
        let v = self.request("action", Some(json!({"name": name, "args": args})));
        assert!(v.get("error").is_none(), "{name} failed: {v}");
        v["result"].clone()
    }

    /// Wait for a pushed message that `want` accepts (also among those already seen).
    fn wait_for(&mut self, what: &str, want: impl Fn(&Value) -> bool) -> Value {
        let mut seen = 0;
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(v) = self.pushed[seen..].iter().find(|v| want(v)) {
                return v.clone();
            }
            seen = self.pushed.len();
            let left = end.saturating_duration_since(Instant::now());
            match self.next(left) {
                Some(v) => self.pushed.push(v),
                None => panic!("timed out waiting for {what}; got {:#?}", self.pushed),
            }
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn nebula(home: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(NEBULA)
        .args(args)
        .env("NEBULA_HOME", home)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .output()
        .expect("cannot run nebula");
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn changes(v: &Value) -> Vec<Value> {
    if v["method"] != json!("state") {
        return Vec::new();
    }
    v["params"]["changes"].as_array().cloned().unwrap_or_default()
}

fn temp_home(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nebula-ma-e2e-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn serve_over_stdio() {
    let home = temp_home("stdio");
    let script = home.join("script.sh");
    std::fs::write(
        &script,
        format!("# {SCRIPT_MARK}\necho {OUTPUT_MARK}\nexit 0\n"),
    )
    .unwrap();
    let mut s = Serve::start(&home);

    // the handshake the core does: describe, then state.get
    let d = s.request("describe", None);
    assert_eq!(d["result"]["title"], json!("Nebula"));
    assert_eq!(d["result"]["page"], json!("custom"));
    let st = s.request("state.get", None);
    assert_eq!(st["result"]["state"]["db"], json!("missing"));
    assert_eq!(st["result"]["state"]["runs"], json!({}));
    assert_eq!(st["result"]["stale"], json!([]));
    assert!(
        !home.join("nebula.db").exists(),
        "serve does not create the database"
    );
    // what the core sends when a page opens; no answer, no effect
    writeln!(
        s.stdin.as_mut().unwrap(),
        r#"{{"jsonrpc":"2.0","method":"watch","params":{{"watching":true}}}}"#
    )
    .unwrap();
    let e = s.request("action", Some(json!({"name": "nope"})));
    assert_eq!(e["error"]["code"], json!(-32601));

    // a run appears
    let (code, out) = nebula(
        &home,
        &[
            "run",
            "--local",
            "--detach",
            "--no-clipboard",
            "--name",
            "e2e-job",
            script.to_str().unwrap(),
        ],
    );
    assert_eq!(code, 0, "{out}");
    let appeared = s.wait_for("the run to appear", |v| {
        changes(v)
            .iter()
            .any(|c| c["value"]["name"] == json!("e2e-job") && c["value"]["state"] == json!("pending"))
    });
    let change = changes(&appeared)
        .into_iter()
        .find(|c| c["value"]["name"] == json!("e2e-job"))
        .unwrap();
    let path = change["path"].as_str().unwrap().to_string();
    let key = path.strip_prefix("runs.").expect("runs.<row id>").to_string();
    let label = change["value"]["label"].as_str().unwrap().to_string();
    assert_eq!(change["value"]["target"], json!("local"));
    let st = s.request("state.get", None);
    assert_eq!(st["result"]["state"]["db"], json!("ok"));
    assert_eq!(st["result"]["state"]["counts"]["open"], json!(1));
    assert_eq!(st["result"]["state"]["runs"][&key]["label"], json!(label));

    // local-only actions: the one-liner, and the script before anything ran
    assert_eq!(
        s.action("oneLiner", json!({"id": label})),
        json!({"text": format!("nebula go {label}"), "target": "local"})
    );
    let v = s.action("view", json!({"id": label, "script": true}));
    assert!(v["script"].as_str().unwrap().contains(SCRIPT_MARK));
    assert_eq!((&v["text"], &v["next"]), (&json!(""), &json!(0)));

    // the human runs it; the tool notices the end and says so, once
    let (code, out) = nebula(&home, &["go", &label]);
    assert_eq!(code, 0, "{out}");
    let n = s.wait_for("the notification", |v| v["method"] == json!("notify"));
    assert_eq!(
        n["params"],
        json!({"kind": "finished", "title": "e2e-job finished"})
    );
    s.wait_for("the final state", |v| {
        changes(v)
            .iter()
            .any(|c| c["path"] == json!(format!("runs.{key}.state")) && c["value"] == json!("done"))
    });
    let st = s.request("state.get", None);
    let run = &st["result"]["state"]["runs"][&key];
    assert_eq!((&run["state"], &run["exit"]), (&json!("done"), &json!(0)));
    assert_eq!(run["bytes"], json!(OUTPUT_MARK.len() + 1));

    // what was recorded about where it ran: only in the details of `view`
    let v = s.action("view", json!({"id": label, "script": true}));
    let d = &v["details"];
    assert_eq!(d["via"], json!("go"));
    assert!(d["os"].as_str().unwrap().starts_with("Linux "), "{d}");
    assert!(d.get("fetchAddr").is_none() && d.get("fetchAgent").is_none(), "{d}");
    assert!(d.get("tty").is_none(), "stdin was no terminal: {d}");

    // the output, whole and then from an offset
    let v = s.action("view", json!({"id": label}));
    assert_eq!(v["text"], json!(format!("{OUTPUT_MARK}\n")));
    assert_eq!((&v["from"], &v["state"]), (&json!(0), &json!("done")));
    let next = v["next"].as_u64().unwrap();
    assert_eq!(next, OUTPUT_MARK.len() as u64 + 1);
    let v = s.action("view", json!({"id": label, "offset": 4}));
    assert_eq!(v["text"], json!(format!("{}\n", &OUTPUT_MARK[4..])));
    let v = s.action("view", json!({"id": label, "offset": next}));
    assert_eq!(v["text"], json!(""));
    // page by page from the start, as "copy output" reads it
    let v = s.action("view", json!({"id": label, "offset": 0, "forward": true}));
    assert_eq!(v["text"], json!(format!("{OUTPUT_MARK}\n")));
    assert_eq!(v["next"], json!(next));

    // a failing run, then the same deletion `nebula rm` does
    std::fs::write(&script, "echo failing; exit 3\n").unwrap();
    let (code, out) = nebula(
        &home,
        &["run", "--local", "--detach", "--no-clipboard", script.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{out}");
    let second = s.wait_for("the second run", |v| {
        changes(v)
            .iter()
            .any(|c| c["value"]["state"] == json!("pending") && c["value"]["label"] != json!(label))
    });
    let label2 = changes(&second)
        .into_iter()
        .find(|c| c["value"]["state"] == json!("pending"))
        .unwrap()["value"]["label"]
        .as_str()
        .unwrap()
        .to_string();
    let (code, _) = nebula(&home, &["go", &label2]);
    assert_eq!(code, 3);
    let n = s.wait_for("the failure notification", |v| {
        v["method"] == json!("notify") && v["params"]["kind"] == json!("failed")
    });
    assert_eq!(
        n["params"]["title"],
        json!(format!("#{label2} failed (exit 3)"))
    );
    let r = s.action("remove", json!({"id": label}));
    assert_eq!(r["message"], json!(format!("run {label} removed")));
    let (code, out) = nebula(&home, &["show", &label]);
    assert_ne!(code, 0, "{out}");
    let st = s.request("state.get", None);
    assert!(st["result"]["state"]["runs"].get(&key).is_none());

    // nothing that was pushed (state, log, notify) carries output or script text
    let pushed = serde_json::to_string(&s.pushed).unwrap();
    for secret in [OUTPUT_MARK, SCRIPT_MARK, "failing", "nebula go", "Linux"] {
        assert!(!pushed.contains(secret), "{secret} in {pushed}");
    }
    let kinds: Vec<&str> = s
        .pushed
        .iter()
        .filter(|v| v["method"] == json!("notify"))
        .map(|v| v["params"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["finished", "failed"]);

    // stdin closes: the tool exits by itself
    drop(s.stdin.take());
    let end = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(st) = s.child.try_wait().unwrap() {
            break st;
        }
        assert!(Instant::now() < end, "serve did not exit after stdin closed");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
    drop(s);
    let _ = std::fs::remove_dir_all(&home);
}

/// A database with runs that ended before the tool started: all of them are
/// in the first state, and none is announced.
#[test]
fn history_is_shown_but_not_announced() {
    let home = temp_home("history");
    let script = home.join("script.sh");
    std::fs::write(&script, "echo old; exit 5\n").unwrap();
    let (code, out) = nebula(
        &home,
        &["run", "--local", "--detach", "--no-clipboard", "--name", "old", script.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(nebula(&home, &["go", "1"]).0, 5);
    let mut s = Serve::start(&home);
    let st = s.request("state.get", None);
    let run = &st["result"]["state"]["runs"]["1"];
    assert_eq!((&run["state"], &run["exit"], &run["name"]), (&json!("done"), &json!(5), &json!("old")));
    // two polls later still nothing was pushed
    assert!(s.next(Duration::from_millis(3500)).is_none());
    drop(s);
    let _ = std::fs::remove_dir_all(&home);
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A run fetched over HTTP from a private daemon: the address, the client
/// and the far side's OS are recorded, shown by `nebula show` and by `view`,
/// and appear nowhere else in what the tool sends.
#[test]
fn fetch_details_only_in_view() {
    if Command::new("curl").arg("--version").output().is_err() {
        println!("SKIPPED: curl is not installed");
        return;
    }
    let home = temp_home("fetch");
    let port = free_port();
    let mut daemon = Command::new(NEBULA)
        .arg("daemon")
        .env("NEBULA_HOME", &home)
        .env("NEBULA_PORT", port.to_string())
        .env("NEBULA_NO_TUNNEL", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("cannot start a private daemon");
    let up = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < up, "the daemon did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut s = Serve::start(&home);
    s.request("describe", None);
    s.request("state.get", None);

    let script = home.join("script.sh");
    std::fs::write(&script, "echo fetched-run\n").unwrap();
    let (code, out) = nebula(
        &home,
        &["run", "--local", "--detach", "--no-clipboard", "--name", "over-http", script.to_str().unwrap()],
    );
    assert_eq!(code, 0, "{out}");
    let url = out
        .split_whitespace()
        .find(|w| w.starts_with(&format!("http://127.0.0.1:{port}/r/")))
        .unwrap_or_else(|| panic!("no link in {out}"))
        .to_string();
    let far = Command::new("sh")
        .arg("-c")
        .arg(format!("curl -fsS -A 'curl/e2e <i>agent</i>' '{url}' | sh"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(far.status.success(), "{}", String::from_utf8_lossy(&far.stderr));
    s.wait_for("the notification", |v| {
        v["method"] == json!("notify") && v["params"]["title"] == json!("over-http finished")
    });

    let v = s.action("view", json!({"id": "1", "script": true}));
    let d = &v["details"];
    assert_eq!(d["via"], json!("http"));
    assert_eq!(d["fetchAddr"], json!("127.0.0.1"));
    assert_eq!(d["fetchAgent"], json!("curl/e2e <i>agent</i>"));
    assert!(d.get("fetchCountry").is_none(), "{d}");
    let os = d["os"].as_str().unwrap();
    assert!(os.starts_with("Linux ") && os.split(' ').count() >= 3, "{os}");
    assert!(d.get("tty").is_none());
    assert_eq!(v["text"], json!("fetched-run\n"));

    let (_, show) = nebula(&home, &["show", "1"]);
    assert!(show.contains("\nfetched from: 127.0.0.1\n"), "{show}");
    assert!(show.contains("\nfetched with: curl/e2e <i>agent</i>\n"), "{show}");
    assert!(show.contains(&format!("\nos:           {os}\n")), "{show}");

    let st = s.request("state.get", None);
    let public = format!("{}{}", st, serde_json::to_string(&s.pushed).unwrap());
    assert!(public.contains("over-http"));
    for secret in ["127.0.0.1", "curl/e2e", "agent", "Linux", os, "fetched-run"] {
        assert!(!public.contains(secret), "{secret} in {public}");
    }
    drop(s);
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_dir_all(&home);
}
