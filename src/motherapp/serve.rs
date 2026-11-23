//! `nebula motherapp serve`: the long-running process motherapp's core starts
//! and talks to over stdin/stdout (see `rpc.rs` for the wire format).
//!
//! It reads the database every 1.5 s, with or without a page open, because
//! it also sends the notifications for runs that end. It only reads: no
//! sweep, no expiry, no "lost" marking, no hook is claimed from here, the
//! connection cannot write, and no statement stays open between two polls,
//! so Nebula's secure delete and WAL truncation are never held up. The
//! actions that change something (`cancel`, `remove`, `removeOutput`) go
//! through the same functions as `nebula cancel` and `nebula rm`, on a
//! separate connection opened for that one call.
//!
//! Privacy: state, logs and notifications carry metadata only. Output,
//! script text and the one-liner (which holds the token) are returned only
//! by `view` and `oneLiner`, which the manifest lists in `localActions`, so
//! the core refuses them for any other machine.

use super::rpc::{self, Incoming, RpcError};
use super::state::{self, Tracker};
use crate::cli;
use crate::db::{self, DaemonInfo, Run};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{Map, Value, json};
use std::io::{BufRead, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const TITLE: &str = "Nebula";
pub const ICON: &str = "🌌";
/// As often as motherapp's tmux tool.
const POLL: Duration = Duration::from_millis(1500);
/// How many of the newest top-level runs are published (open ones always are).
const HISTORY_LIMIT: i64 = 100;
/// Most output bytes one `view` call returns; the newest are kept.
const VIEW_CAP: u64 = 256 * 1024;
/// Most script bytes one `view` call returns.
const SCRIPT_CAP: usize = 256 * 1024;

struct Snapshot {
    runs: Vec<Run>,
    /// Open at the last look, outside the published window now.
    strays: Vec<Run>,
    gone: Vec<i64>,
    total: i64,
    daemon: DaemonInfo,
}

pub struct Server<W: Write> {
    out: W,
    dir: PathBuf,
    /// Read-only connection and the file it belongs to (device, inode).
    ro: Option<(Connection, (u64, u64))>,
    state: Value,
    /// The last read failed: the runs shown are from before it.
    runs_stale: bool,
    tracker: Tracker,
    view_cap: u64,
    script_cap: usize,
    /// stdout is gone; the loop ends.
    pub closed: bool,
}

fn empty_state() -> Value {
    json!({
        "db": "missing",
        "daemon": { "running": false, "tunnel": db::TUNNEL_DOWN },
        "counts": { "open": 0, "total": 0, "limit": HISTORY_LIMIT },
        "runs": {},
    })
}

/// The messages `nebula cancel`/`rm` print, without the program's name.
fn plain(lines: &[String]) -> String {
    lines
        .iter()
        .map(|l| l.strip_prefix("nebula: ").unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n")
}

fn failed(e: anyhow::Error) -> RpcError {
    RpcError::failed(format!("{e:#}"))
}

/// `id` as the CLI takes it: `42`, or `42.2` for an execution.
fn reference(args: &Value) -> Result<String, RpcError> {
    let r = match args.get("id") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) if n.is_u64() => n.to_string(),
        _ => String::new(),
    };
    let part = |s: &str| s.parse::<i64>().is_ok_and(|n| n > 0);
    let ok = match r.split_once('.') {
        None => part(&r),
        Some((a, b)) => part(a) && part(b),
    };
    if ok {
        Ok(r)
    } else {
        Err(RpcError::invalid("id must be a run id like 42 or 42.2"))
    }
}

impl<W: Write> Server<W> {
    /// `dir` is the directory of `nebula.db`. Takes stock of what is there,
    /// so that the first `state.get` is answered with real data and nothing
    /// that ended earlier is announced.
    pub fn new(dir: PathBuf, out: W) -> Self {
        let mut s = Server {
            out,
            dir,
            ro: None,
            state: empty_state(),
            runs_stale: false,
            tracker: Tracker::default(),
            view_cap: VIEW_CAP,
            script_cap: SCRIPT_CAP,
            closed: false,
        };
        s.refresh(false);
        s
    }

    fn send(&mut self, line: String) {
        if self.closed {
            return;
        }
        if writeln!(self.out, "{line}")
            .and_then(|_| self.out.flush())
            .is_err()
        {
            self.closed = true;
        }
    }

    fn log(&mut self, level: &str, msg: String) {
        self.send(rpc::notify_line(
            "log",
            json!({ "level": level, "msg": msg }),
        ));
    }

    /// The read-only connection, reopened when the file was replaced.
    fn reader(&mut self) -> Result<Option<&Connection>> {
        let key = std::fs::metadata(self.dir.join("nebula.db"))
            .ok()
            .filter(|m| m.is_file())
            .map(|m| (m.dev(), m.ino()));
        if self.ro.as_ref().map(|(_, k)| *k) != key {
            self.ro = None;
        }
        let Some(key) = key else { return Ok(None) };
        if self.ro.is_none() {
            self.ro = db::open_readonly_in(&self.dir)?.map(|c| (c, key));
        }
        Ok(self.ro.as_ref().map(|(c, _)| c))
    }

    fn read(&mut self) -> Result<Option<Snapshot>> {
        let wanted = self.tracker.missing(&[]);
        let Some(conn) = self.reader()? else {
            return Ok(None);
        };
        let runs = db::tool_runs(conn, HISTORY_LIMIT)?;
        let mut strays = Vec::new();
        let mut gone = Vec::new();
        for id in wanted {
            if runs.iter().any(|r| r.id == id) {
                continue;
            }
            match db::get_run(conn, id)? {
                Some(r) => strays.push(r),
                None => gone.push(id),
            }
        }
        Ok(Some(Snapshot {
            total: db::count_runs(conn)?,
            daemon: db::daemon_info(conn)?,
            runs,
            strays,
            gone,
        }))
    }

    /// One poll: read, publish what changed, announce what ended.
    pub fn poll(&mut self) {
        self.refresh(true);
    }

    fn refresh(&mut self, announce: bool) {
        let was = self.state["db"].as_str().unwrap_or("").to_string();
        let mut new = self.state.clone();
        let mut notices = Vec::new();
        let mut problem = None;
        match self.read() {
            Ok(Some(snap)) => {
                new["db"] = json!("ok");
                new["runs"] = state::runs_json(&snap.runs);
                new["counts"] = state::counts_json(&snap.runs, snap.total, HISTORY_LIMIT);
                new["daemon"] = state::daemon_json(&snap.daemon);
                for id in snap.gone {
                    self.tracker.forget(id);
                }
                let mut seen = snap.runs;
                seen.extend(snap.strays);
                notices = self.tracker.observe(&seen);
            }
            Ok(None) => {
                new = empty_state();
                // a database made later starts counting from nothing
                self.tracker = Tracker::default();
                self.tracker.observe(&[]);
            }
            Err(e) => {
                // keep showing what was read last, marked as unconfirmed
                self.ro = None;
                new["db"] = json!("error");
                problem = Some(format!("{e:#}"));
            }
        }
        let stale = problem.is_some();
        let mut changes = Vec::new();
        state::diff("", &self.state, &new, &mut changes);
        if stale != self.runs_stale {
            // a stale mark is set and cleared on the whole `runs` value
            changes.retain(|c| {
                !c["path"]
                    .as_str()
                    .is_some_and(|p| p == "runs" || p.starts_with("runs."))
            });
            let mut c = json!({ "path": "runs", "value": new["runs"] });
            if stale {
                c["stale"] = json!(true);
            }
            changes.push(c);
        }
        self.state = new;
        self.runs_stale = stale;
        if !announce {
            return;
        }
        let now = self.state["db"].as_str().unwrap_or("").to_string();
        if now != was {
            match problem {
                Some(e) => self.log("warn", format!("cannot read Nebula's database: {e}")),
                None if was == "error" => self.log("info", "reading Nebula's database again".into()),
                None => {}
            }
        }
        if !changes.is_empty() {
            self.send(rpc::notify_line("state", json!({ "changes": changes })));
        }
        for n in notices {
            self.send(rpc::notify_line(
                "notify",
                json!({ "kind": n.kind, "title": n.title }),
            ));
        }
    }

    /// One line from the core.
    pub fn handle_line(&mut self, line: &str) {
        for msg in rpc::parse_line(line) {
            match msg {
                Incoming::Request { id, method, params } => {
                    let reply = match self.request(&method, &params) {
                        Ok(result) => rpc::result_line(&id, result),
                        Err(e) => rpc::error_line(&id, &e),
                    };
                    self.send(reply);
                }
                // `watch` says whether a page is looking; this tool polls either way
                Incoming::Notification { .. } | Incoming::Response => {}
                Incoming::Invalid(reason) => {
                    let start: String = line.chars().take(200).collect();
                    eprintln!("nebula: ignoring input ({reason}): {start}");
                }
            }
        }
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, RpcError> {
        let name = || params.get("name").and_then(Value::as_str);
        match method {
            "describe" => Ok(json!({ "title": TITLE, "icon": ICON, "page": "custom" })),
            "state.get" => Ok(json!({
                "state": self.state,
                "stale": if self.runs_stale { vec!["runs"] } else { vec![] },
            })),
            "action" => {
                let Some(name) = name() else {
                    return Err(RpcError::invalid("action: missing name"));
                };
                let args = match params.get("args") {
                    Some(a @ Value::Object(_)) => a.clone(),
                    _ => json!({}),
                };
                self.action(name, &args)
            }
            "stream.open" => match name() {
                Some(n) => Err(RpcError::new(
                    rpc::METHOD_NOT_FOUND,
                    format!("unknown stream: {n}"),
                )),
                None => Err(RpcError::invalid("stream.open: missing name")),
            },
            "stream.close" => match name() {
                Some(_) => Ok(Value::Null),
                None => Err(RpcError::invalid("stream.close: missing name")),
            },
            _ => Err(RpcError::new(
                rpc::METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            )),
        }
    }

    fn action(&mut self, name: &str, args: &Value) -> Result<Value, RpcError> {
        match name {
            "view" => self.view(args),
            "oneLiner" => self.one_liner(args),
            "cancel" | "remove" | "removeOutput" => self.change(name, args),
            _ => Err(RpcError::new(
                rpc::METHOD_NOT_FOUND,
                format!("unknown action: {name}"),
            )),
        }
    }

    fn lookup(&mut self, reference: &str) -> Result<(&Connection, Run), RpcError> {
        let conn = self
            .reader()
            .map_err(failed)?
            .ok_or_else(|| RpcError::failed("Nebula has no database yet"))?;
        let run = db::resolve(conn, reference)
            .map_err(failed)?
            .ok_or_else(|| RpcError::failed(format!("no run {reference}")))?;
        Ok((conn, run))
    }

    /// Local only. The output from `offset` on (at most `view_cap` bytes,
    /// the newest; with `forward: true` the next ones, to read all of it page
    /// by page), decoded lossily; with `script: true` also the start of the
    /// script and the details `nebula show` prints.
    fn view(&mut self, args: &Value) -> Result<Value, RpcError> {
        let reference = reference(args)?;
        let offset = match args.get("offset") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .ok_or_else(|| RpcError::invalid("offset must be a byte count"))?,
            ),
        };
        let with_script = args.get("script") == Some(&json!(true));
        let forward = args.get("forward") == Some(&json!(true));
        let script_offset = match args.get("scriptOffset") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_u64()
                    .ok_or_else(|| RpcError::invalid("scriptOffset must be a byte count"))?
                    as usize,
            ),
        };
        let (cap, script_cap) = (self.view_cap, self.script_cap);
        let (conn, r) = self.lookup(&reference)?;
        let total = if r.output_deleted {
            0
        } else {
            r.output_bytes.max(0) as u64
        };
        let (mut from, to) = state::view_range(total, offset, cap, forward);
        let mut data = db::output_slice(conn, r.id, from, to).map_err(failed)?;
        if offset != Some(from) && from > 0 {
            // started in the middle of the output, maybe in the middle of a character
            let cut = state::leading_continuation(&data);
            data.drain(..cut);
            from += cut as u64;
        }
        if !r.is_finished() || to < total {
            // the rest of a character cut off at the end comes with the next call
            data.truncate(data.len() - state::incomplete_tail(&data));
        }
        let mut m = Map::new();
        m.insert("id".into(), json!(r.label()));
        m.insert("state".into(), json!(r.state));
        m.insert("bytes".into(), json!(total));
        // `from` above 0 on a call without `offset`, or different from the
        // `offset` given: everything before it was skipped (newest kept)
        m.insert("from".into(), json!(from));
        m.insert("next".into(), json!(from + data.len() as u64));
        m.insert("cap".into(), json!(cap));
        m.insert("text".into(), json!(String::from_utf8_lossy(&data)));
        if r.output_deleted {
            m.insert("deleted".into(), json!(true));
        }
        if r.output_truncated {
            m.insert("truncated".into(), json!(true));
        }
        // `script: true`: the start of the script and the details. `scriptOffset`:
        // the script from there on, `script_cap` bytes a call, for copying a long one.
        if with_script || script_offset.is_some() {
            match db::get_script(conn, &r.script_sha).map_err(failed)? {
                Some(body) => {
                    let start = script_offset.unwrap_or(0).min(body.len());
                    let mut end = start.saturating_add(script_cap).min(body.len());
                    if end < body.len() {
                        end -= state::incomplete_tail(&body[start..end]);
                        m.insert("scriptCut".into(), json!(true));
                    }
                    let shown = &body[start..end];
                    m.insert("script".into(), json!(String::from_utf8_lossy(shown)));
                    m.insert("scriptNext".into(), json!(end));
                    m.insert("scriptBytes".into(), json!(body.len()));
                }
                None => {
                    m.insert("scriptMissing".into(), json!(true));
                }
            }
        }
        if with_script {
            let mut d = Map::new();
            d.insert("sha256".into(), json!(r.script_sha));
            d.insert("echo".into(), json!(r.echo));
            d.insert("deleteAfter".into(), json!(r.delete_after));
            d.insert("ttlSecs".into(), json!(r.ttl_secs));
            d.insert("staleSecs".into(), json!(r.stale_secs));
            for (key, v) in [
                ("note", &r.note),
                ("via", &r.via),
                ("onDone", &r.on_done),
                ("tmux", &r.tmux_target),
                ("hook", &r.hook_result),
                // where it was fetched from and what ran it: as recorded by the
                // daemon from request headers and the wrapper's report, untrusted
                ("fetchAddr", &r.fetch_addr),
                ("fetchCountry", &r.fetch_country),
                ("fetchAgent", &r.fetch_agent),
                ("os", &r.far_os),
                ("tty", &r.tty),
            ] {
                if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
                    d.insert(key.into(), json!(v));
                }
            }
            m.insert("details".into(), Value::Object(d));
        }
        Ok(Value::Object(m))
    }

    /// Local only: what the human pastes for a pending run, as `nebula show`
    /// prints it. For a remote run it holds the link with the token.
    fn one_liner(&mut self, args: &Value) -> Result<Value, RpcError> {
        let reference = reference(args)?;
        let (conn, r) = self.lookup(&reference)?;
        if r.state != db::ST_PENDING {
            return Err(RpcError::failed(format!(
                "run {} is {}; only a pending run has a one-liner",
                r.label(),
                r.state
            )));
        }
        let info = db::daemon_info(conn).map_err(failed)?;
        match cli::current_one_liner(&r, &info) {
            Some(text) => Ok(json!({ "text": text, "target": r.target })),
            None => Err(RpcError::failed(
                "no tunnel address right now (is the Nebula daemon running?)",
            )),
        }
    }

    /// `cancel`, `remove` (`nebula rm`) and `removeOutput` (`nebula rm
    /// --output-only`), through the CLI's own functions.
    fn change(&mut self, name: &str, args: &Value) -> Result<Value, RpcError> {
        let reference = reference(args)?;
        if !self.dir.join("nebula.db").is_file() {
            return Err(RpcError::failed("Nebula has no database yet"));
        }
        let done = {
            let mut conn = db::open_in(&self.dir).map_err(failed)?;
            match name {
                "cancel" => cli::cancel(&conn, &reference),
                _ => cli::remove(&mut conn, &reference, name == "removeOutput"),
            }
            .map_err(failed)?
        };
        let message = plain(&done.lines);
        if done.code != 0 {
            return Err(RpcError::failed(message));
        }
        // the run's id and what was done, nothing else
        self.log("info", format!("{name}: run {reference}"));
        // publish the result before the reply, so the page shows it at once
        self.refresh(true);
        Ok(json!({ "message": message }))
    }
}

/// Run serve mode until stdin closes (the core went away or stopped us).
pub fn run(home: Option<&Path>) -> Result<i32> {
    let dir = match home {
        Some(d) => d.to_path_buf(),
        None => db::data_dir()?,
    };
    let (tx, rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match input.read_until(b'\n', &mut buf) {
                // an unterminated tail at the end of input is dropped, as the SDK does
                Ok(n) if n > 0 && buf.ends_with(b"\n") => {
                    let line = String::from_utf8_lossy(&buf);
                    let line = line.trim_end_matches(['\n', '\r']);
                    if !line.trim().is_empty() && tx.send(Some(line.to_string())).is_err() {
                        return;
                    }
                }
                _ => {
                    let _ = tx.send(None);
                    return;
                }
            }
        }
    });
    let mut server = Server::new(dir, std::io::stdout());
    let mut next = Instant::now() + POLL;
    while !server.closed {
        match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(Some(line)) => server.handle_line(&line),
            Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= next {
            server.poll();
            next = Instant::now() + POLL;
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: PathBuf,
        conn: Connection,
        s: Server<Vec<u8>>,
        next_id: i64,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("nebula-ma-{}", crate::util::new_token().unwrap()))
    }

    fn fixture() -> Fixture {
        let dir = temp_dir();
        let conn = db::open_in(&dir).unwrap();
        let s = Server::new(dir.clone(), Vec::new());
        Fixture {
            dir,
            conn,
            s,
            next_id: 0,
        }
    }

    impl Fixture {
        /// Everything written to "stdout" since the last call, one value per line.
        fn output(&mut self) -> Vec<Value> {
            let raw = String::from_utf8(std::mem::take(&mut self.s.out)).unwrap();
            raw.lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }
        fn raw_output(&mut self) -> String {
            String::from_utf8(std::mem::take(&mut self.s.out)).unwrap()
        }
        /// Send a request, return its reply (`result` or `error` inside).
        fn call(&mut self, method: &str, params: Value) -> Value {
            self.next_id += 1;
            let id = self.next_id;
            self.s.handle_line(
                &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
                    .to_string(),
            );
            let mut replies: Vec<Value> = self
                .output()
                .into_iter()
                .filter(|m| m.get("id") == Some(&json!(id)))
                .collect();
            assert_eq!(replies.len(), 1, "one reply for {method}");
            replies.remove(0)
        }
        fn action(&mut self, name: &str, args: Value) -> Value {
            self.call("action", json!({"name": name, "args": args}))
        }
        fn state(&mut self) -> Value {
            self.call("state.get", Value::Null)["result"]["state"].clone()
        }
        fn started(&mut self, name: Option<&str>) -> Run {
            let r = state::test_run(&self.conn, name, None);
            db::claim_fetch(&self.conn, r.id, "https://tunnel-secret.example").unwrap();
            db::mark_started(&self.conn, r.id, "web-1", "root").unwrap();
            r
        }
    }

    #[test]
    fn describe_state_and_errors() {
        let mut f = fixture();
        assert!(f.output().is_empty(), "nothing is written unasked");
        assert_eq!(
            f.call("describe", Value::Null)["result"],
            json!({"title": "Nebula", "icon": ICON, "page": "custom"})
        );
        let r = f.call("state.get", Value::Null);
        assert_eq!(r["jsonrpc"], json!("2.0"));
        assert_eq!(r["result"]["stale"], json!([]));
        assert_eq!(
            r["result"]["state"],
            json!({
                "db": "ok",
                "daemon": {"running": false, "tunnel": "down"},
                "counts": {"open": 0, "total": 0, "limit": 100},
                "runs": {},
            })
        );
        let code = |v: &Value| v["error"]["code"].as_i64().unwrap();
        let e = f.call("nope", Value::Null);
        assert_eq!(code(&e), -32601);
        assert_eq!(e["error"]["message"], json!("method not found: nope"));
        assert_eq!(code(&f.call("action", Value::Null)), -32602);
        assert_eq!(code(&f.call("action", json!({"args": {}}))), -32602);
        let e = f.call("action", json!({"name": "explode"}));
        assert_eq!(code(&e), -32601);
        assert_eq!(e["error"]["message"], json!("unknown action: explode"));
        assert_eq!(code(&f.call("stream.open", json!({"name": "x"}))), -32601);
        assert_eq!(code(&f.call("stream.open", json!({}))), -32602);
        assert_eq!(
            f.call("stream.close", json!({"name": "x"}))["result"],
            Value::Null
        );
        for bad in [json!({}), json!({"id": "x"}), json!({"id": "1.2.3"}), json!({"id": -4})] {
            for a in ["view", "oneLiner", "cancel", "remove", "removeOutput"] {
                assert_eq!(code(&f.action(a, bad.clone())), -32602, "{a} {bad}");
            }
        }
        let e = f.action("view", json!({"id": 99}));
        assert_eq!(code(&e), -32000);
        assert_eq!(e["error"]["message"], json!("no run 99"));
        // a string id is echoed as a string; notifications and replies get no answer
        f.s.handle_line(r#"{"jsonrpc":"2.0","id":"abc","method":"describe"}"#);
        f.s.handle_line(r#"{"jsonrpc":"2.0","method":"watch","params":{"watching":true}}"#);
        f.s.handle_line(r#"{"jsonrpc":"2.0","id":5,"result":null}"#);
        f.s.handle_line("not json");
        let out = f.output();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], json!("abc"));
    }

    #[test]
    fn a_run_appears_changes_and_is_announced() {
        let mut f = fixture();
        let r = state::test_run(&f.conn, Some("disk-check"), None);
        let key = r.id.to_string();
        f.s.poll();
        let out = f.output();
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0]["method"], json!("state"));
        let changes = out[0]["params"]["changes"].as_array().unwrap();
        let whole = changes
            .iter()
            .find(|c| c["path"] == json!(format!("runs.{key}")))
            .expect("the new run arrives as one value");
        assert_eq!(whole["value"]["state"], json!("pending"));
        assert_eq!(whole["value"]["name"], json!("disk-check"));
        assert!(changes.contains(&json!({"path": "counts.open", "value": 1})));
        // nothing changed: nothing is sent
        f.s.poll();
        assert!(f.output().is_empty());

        db::claim_fetch(&f.conn, r.id, "https://tunnel-secret.example").unwrap();
        db::mark_started(&f.conn, r.id, "web-1", "root").unwrap();
        db::append_output(&mut f.conn, r.id, 0, b"OUTPUT-SECRET\n").unwrap();
        f.s.poll();
        let out = f.output();
        assert_eq!(out.len(), 1);
        let changes = out[0]["params"]["changes"].as_array().unwrap();
        assert!(changes.contains(&json!({"path": format!("runs.{key}.state"), "value": "running"})));
        assert!(changes.contains(&json!({"path": format!("runs.{key}.bytes"), "value": 14})));
        assert!(changes.contains(&json!({"path": format!("runs.{key}.host"), "value": "web-1"})));
        assert!(changes.iter().all(|c| c["path"] != json!(format!("runs.{key}"))));

        db::mark_done(&f.conn, r.id, 0, Some(14)).unwrap();
        f.s.poll();
        let out = f.output();
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0]["method"], json!("state"));
        assert_eq!(
            out[1],
            json!({"jsonrpc": "2.0", "method": "notify",
                   "params": {"kind": "finished", "title": "disk-check finished"}})
        );
        f.s.poll();
        assert!(f.output().is_empty(), "announced once");
        assert_eq!(f.state()["runs"][&key]["exit"], json!(0));
    }

    #[test]
    fn nothing_is_announced_for_what_ended_before_the_start() {
        let dir = temp_dir();
        let conn = db::open_in(&dir).unwrap();
        let old = state::test_run(&conn, Some("old"), None);
        db::claim_fetch(&conn, old.id, "http://x").unwrap();
        db::mark_done(&conn, old.id, 2, None).unwrap();
        let mut s = Server::new(dir.clone(), Vec::new());
        s.poll();
        s.poll();
        assert!(s.out.is_empty(), "{}", String::from_utf8_lossy(&s.out));
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The whole point of `localActions`: everything outside the results of
    /// `view` and `oneLiner` is free of output, script, note, token, tunnel address, and of
    /// where a link was fetched from (address, country, user agent), the far OS and the tty.
    #[test]
    fn secrets_only_in_local_action_results() {
        let mut f = fixture();
        let pending = state::test_run(&f.conn, Some("waiting"), None);
        let r = f.started(Some("job"));
        db::append_output(&mut f.conn, r.id, 0, b"OUTPUT-SECRET line\n").unwrap();
        f.conn
            .execute(
                "UPDATE runs SET on_done = 'echo HOOK-SECRET', fetch_addr = '203.0.113.77', \
                 fetch_country = 'ZQ', fetch_agent = 'agent-SECRET/1', \
                 far_os = 'os-SECRET 1 x', tty = '/dev/pts/SECRET'",
                [],
            )
            .unwrap();
        db::set_kv(&f.conn, "tunnel_url", "https://tunnel-secret.example").unwrap();
        f.s.poll();
        db::mark_done(&f.conn, r.id, 4, None).unwrap();
        f.s.poll();
        f.s.handle_line(r#"{"jsonrpc":"2.0","id":900,"method":"state.get"}"#);
        f.s.handle_line(r#"{"jsonrpc":"2.0","id":901,"method":"describe"}"#);
        f.s.handle_line(
            &json!({"jsonrpc": "2.0", "id": 902, "method": "action",
                    "params": {"name": "removeOutput", "args": {"id": r.id}}})
            .to_string(),
        );
        let public = f.raw_output();
        assert!(public.contains("\"notify\"") && public.contains("job failed (exit 4)"));
        assert!(public.contains("removeOutput: run"));
        let tokens = [
            db::get_run(&f.conn, r.id).unwrap().unwrap().token,
            pending.token.clone(),
        ];
        for secret in [
            "SECRET",
            "tunnel-secret",
            "203.0.113",
            "ZQ",
            &tokens[0],
            &tokens[1],
            &r.script_sha,
        ] {
            assert!(!public.contains(secret), "{secret} leaked: {public}");
        }
        // and the local actions do return them
        let v = f.action("view", json!({"id": pending.id, "script": true}));
        assert_eq!(v["result"]["script"], json!("echo SCRIPT-SECRET\n"));
        assert_eq!(v["result"]["details"]["note"], json!("NOTE-SECRET"));
        assert_eq!(v["result"]["details"]["onDone"], json!("echo HOOK-SECRET"));
        let d = &v["result"]["details"];
        assert_eq!(d["fetchAddr"], json!("203.0.113.77"));
        assert_eq!(d["fetchCountry"], json!("ZQ"));
        assert_eq!(d["fetchAgent"], json!("agent-SECRET/1"));
        assert_eq!(d["os"], json!("os-SECRET 1 x"));
        assert_eq!(d["tty"], json!("/dev/pts/SECRET"));
    }

    #[test]
    fn view_follows_the_output() {
        let mut f = fixture();
        let r = f.started(None);
        f.s.view_cap = 16;
        let view = |f: &mut Fixture, args: Value| f.action("view", args)["result"].clone();
        let v = view(&mut f, json!({"id": r.id}));
        assert_eq!(
            (&v["text"], &v["from"], &v["next"], &v["bytes"], &v["cap"]),
            (&json!(""), &json!(0), &json!(0), &json!(0), &json!(16))
        );
        assert!(v.get("script").is_none(), "only when asked for");
        // chunks of any size, and a character split across two of them
        let euro = "€".as_bytes();
        db::append_output(&mut f.conn, r.id, 0, b"one\n").unwrap();
        db::append_output(&mut f.conn, r.id, 4, &[b'2', euro[0]]).unwrap();
        let v = view(&mut f, json!({"id": r.id, "offset": 0}));
        assert_eq!(v["text"], json!("one\n2"), "the half character waits");
        assert_eq!((&v["from"], &v["next"], &v["bytes"]), (&json!(0), &json!(5), &json!(6)));
        db::append_output(&mut f.conn, r.id, 6, &euro[1..]).unwrap();
        let v = view(&mut f, json!({"id": r.id, "offset": 5}));
        assert_eq!(v["text"], json!("€"));
        assert_eq!((&v["from"], &v["next"]), (&json!(5), &json!(8)));
        let v = view(&mut f, json!({"id": r.id, "offset": 8}));
        assert_eq!((&v["text"], &v["next"]), (&json!(""), &json!(8)));

        // more than the cap arrives at once: the newest 16 bytes, on a character boundary
        db::append_output(&mut f.conn, r.id, 8, "0123456789€€€€€€".as_bytes()).unwrap();
        let v = view(&mut f, json!({"id": r.id, "offset": 8}));
        assert_eq!(v["bytes"], json!(36));
        assert_eq!(v["text"], json!("€€€€€"), "the newest 16 bytes start inside a character, which is dropped");
        assert_eq!((&v["from"], &v["next"]), (&json!(21), &json!(36)));
        // a first call gets the same newest part
        let first = view(&mut f, json!({"id": r.id}));
        assert_eq!((&first["text"], &first["from"]), (&v["text"], &v["from"]));

        // bytes that are not UTF-8 are replaced, and once the run is over nothing is held back
        db::append_output(&mut f.conn, r.id, 36, &[0xFF, b'x', euro[0]]).unwrap();
        db::mark_done(&f.conn, r.id, 0, None).unwrap();
        let v = view(&mut f, json!({"id": r.id, "offset": 36}));
        assert_eq!(v["text"], json!("\u{FFFD}x\u{FFFD}"));
        assert_eq!((&v["next"], &v["state"]), (&json!(39), &json!("done")));

        // paging forward reads all of it, 16 bytes a call, never splitting a character
        let whole = String::from_utf8_lossy(
            &db::output_slice(&f.conn, r.id, 0, 39).unwrap(),
        )
        .into_owned();
        let (mut at, mut text, mut calls) = (0u64, String::new(), 0);
        loop {
            let v = view(&mut f, json!({"id": r.id, "offset": at, "forward": true}));
            assert_eq!(v["from"], json!(at));
            let part = v["text"].as_str().unwrap();
            assert!(part.len() <= 16);
            text.push_str(part);
            at = v["next"].as_u64().unwrap();
            calls += 1;
            if at >= v["bytes"].as_u64().unwrap() {
                break;
            }
            assert!(calls < 20, "no progress");
        }
        assert_eq!(text, whole);
        assert!(calls >= 3 && !text[..36].contains('\u{FFFD}'), "{calls} {text}");

        // a long script comes in pieces too
        f.s.script_cap = 8;
        let v = view(&mut f, json!({"id": r.id, "script": true}));
        assert_eq!((&v["script"], &v["scriptNext"], &v["scriptCut"]), (&json!("echo SCR"), &json!(8), &json!(true)));
        assert!(v.get("details").is_some());
        let v = view(&mut f, json!({"id": r.id, "scriptOffset": 8}));
        assert_eq!((&v["script"], &v["scriptNext"]), (&json!("IPT-SECR"), &json!(16)));
        assert!(v.get("details").is_none());
        let v = view(&mut f, json!({"id": r.id, "scriptOffset": 16}));
        assert_eq!((&v["script"], &v["scriptNext"], &v["scriptBytes"]), (&json!("ET\n"), &json!(19), &json!(19)));
        assert!(v.get("scriptCut").is_none());
        f.s.script_cap = SCRIPT_CAP;

        // deleted output: empty, flagged, and an old offset starts over
        f.s.view_cap = VIEW_CAP;
        assert!(f.action("removeOutput", json!({"id": r.id})).get("result").is_some());
        let v = view(&mut f, json!({"id": r.id, "offset": 39}));
        assert_eq!(
            (&v["text"], &v["from"], &v["next"], &v["bytes"], &v["deleted"]),
            (&json!(""), &json!(0), &json!(0), &json!(0), &json!(true))
        );
    }

    #[test]
    fn one_liner_only_for_a_pending_run() {
        let mut f = fixture();
        let local = {
            let sha = db::put_script(&f.conn, b"true\n").unwrap();
            let token = crate::util::new_token().unwrap();
            db::create_run(
                &f.conn,
                &db::NewRun {
                    token: &token,
                    target: "local",
                    script_sha: &sha,
                    ttl_secs: 60,
                    ..Default::default()
                },
            )
            .unwrap()
        };
        assert_eq!(
            f.action("oneLiner", json!({"id": local}))["result"],
            json!({"text": format!("nebula go {local}"), "target": "local"})
        );
        // remote, no daemon: no address to build the link from
        let remote = state::test_run(&f.conn, None, None);
        let e = f.action("oneLiner", json!({"id": remote.id}));
        assert_eq!(e["error"]["code"], json!(-32000));
        assert!(e["error"]["message"].as_str().unwrap().contains("no tunnel address"));
        db::cancel_run(&f.conn, local).unwrap();
        let e = f.action("oneLiner", json!({"id": local.to_string()}));
        assert_eq!(
            e["error"]["message"],
            json!(format!("run {local} is cancelled; only a pending run has a one-liner"))
        );
    }

    #[test]
    fn cancel_and_remove_behave_like_the_cli() {
        let mut f = fixture();
        let r = f.started(Some("job"));
        let key = r.id.to_string();
        db::append_output(&mut f.conn, r.id, 0, b"OUTPUT-SECRET\n").unwrap();
        f.s.poll();
        f.output();
        // a run in progress cannot be removed, with the CLI's own words
        let e = f.action("remove", json!({"id": r.id}));
        assert_eq!(e["error"]["code"], json!(-32000));
        assert_eq!(
            e["error"]["message"],
            json!(format!("run {key} is running; cancel it first (nebula cancel {key})"))
        );
        // cancel: the state change is published before the reply, and no notification follows
        f.s.handle_line(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "action",
                    "params": {"name": "cancel", "args": {"id": key}}})
            .to_string(),
        );
        let out = f.output();
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[0]["method"], json!("log"));
        assert_eq!(out[0]["params"]["msg"], json!(format!("cancel: run {key}")));
        assert!(
            out[1]["params"]["changes"]
                .as_array()
                .unwrap()
                .contains(&json!({"path": format!("runs.{key}.state"), "value": "cancelled"}))
        );
        let msg = out[2]["result"]["message"].as_str().unwrap();
        assert!(msg.starts_with(&format!("run {key} cancelled (was running)\nnote: ")), "{msg}");
        let e = f.action("cancel", json!({"id": key}));
        assert_eq!(
            e["error"]["message"],
            json!(format!("run {key} is already cancelled, nothing to cancel"))
        );

        let v = f.action("removeOutput", json!({"id": r.id}));
        assert_eq!(
            v["result"]["message"],
            json!(format!("output of run {key} deleted (14 bytes); metadata kept"))
        );
        assert_eq!(f.state()["runs"][&key]["deleted"], json!(true));
        assert!(db::chunks_from(&f.conn, r.id, 0).unwrap().is_empty());

        let v = f.action("remove", json!({"id": r.id}));
        assert_eq!(v["result"]["message"], json!(format!("run {key} removed")));
        assert!(db::get_run(&f.conn, r.id).unwrap().is_none());
        assert_eq!(f.state()["runs"], json!({}));
        f.s.poll();
        assert!(f.output().is_empty());
    }

    #[test]
    fn executions_are_addressed_by_label() {
        let mut f = fixture();
        let p = state::test_run(&f.conn, Some("fleet"), Some(0));
        let a = db::claim_execution(&mut f.conn, p.id, "http", Some("http://x"), None)
            .unwrap()
            .unwrap();
        let b = db::claim_execution(&mut f.conn, p.id, "http", Some("http://x"), None)
            .unwrap()
            .unwrap();
        db::mark_started(&f.conn, a.id, "web-1", "root").unwrap();
        db::append_output(&mut f.conn, a.id, 0, b"from a\n").unwrap();
        db::mark_done(&f.conn, a.id, 0, None).unwrap();
        f.s.poll();
        let st = f.state();
        assert_eq!(st["runs"][p.id.to_string()]["multi"], json!(true));
        assert_eq!(st["runs"][a.id.to_string()]["label"], json!(format!("{}.1", p.id)));
        assert_eq!(st["runs"][b.id.to_string()]["parent"], json!(p.id));
        assert_eq!(st["counts"], json!({"open": 2, "total": 1, "limit": 100}));
        let v = f.action("view", json!({"id": format!("{}.1", p.id)}));
        assert_eq!(v["result"]["text"], json!("from a\n"));
        assert_eq!(v["result"]["id"], json!(format!("{}.1", p.id)));
        // an execution's row id is not a run id, as in the CLI
        let e = f.action("view", json!({"id": a.id}));
        assert_eq!(e["error"]["message"], json!(format!("no run {}", a.id)));
        // cancelling the run cancels the execution still in progress
        assert!(f.action("cancel", json!({"id": p.id})).get("result").is_some());
        let st = f.state();
        assert_eq!(st["runs"][b.id.to_string()]["state"], json!("cancelled"));
        assert_eq!(st["runs"][a.id.to_string()]["state"], json!("done"));
    }

    #[test]
    fn it_never_writes_while_polling() {
        let mut f = fixture();
        // overdue in every way: a sweep would expire one and lose the other
        let p = state::test_run(&f.conn, Some("late"), None);
        let r = f.started(Some("silent"));
        // and a finished one whose hook nobody has claimed yet
        let h = f.started(Some("hooked"));
        db::mark_done(&f.conn, h.id, 0, None).unwrap();
        f.conn
            .execute("UPDATE runs SET on_done = 'true' WHERE id = ?1", [h.id])
            .unwrap();
        f.conn
            .execute("UPDATE runs SET expires_at = 1 WHERE id = ?1", [p.id])
            .unwrap();
        f.conn
            .execute("UPDATE runs SET last_seen_at = 1, started_at = 1 WHERE id = ?1", [r.id])
            .unwrap();
        for _ in 0..3 {
            f.s.poll();
        }
        f.action("view", json!({"id": r.id, "script": true}));
        f.action("oneLiner", json!({"id": p.id}));
        let state_of = |id| db::get_run(&f.conn, id).unwrap().unwrap();
        assert_eq!(state_of(p.id).state, db::ST_PENDING);
        assert_eq!(state_of(r.id).state, db::ST_RUNNING);
        assert!(state_of(h.id).hook_claimed_at.is_none());
        assert_eq!(db::pending_hooks(&f.conn).unwrap(), vec![h.id]);
        // and no read is left open: the WAL can be truncated right away
        assert!(db::checkpoint_truncate(&f.conn));
        let wal = std::fs::metadata(f.dir.join("nebula.db-wal")).unwrap().len();
        assert_eq!(wal, 0);
    }

    #[test]
    fn no_database_then_one_and_a_broken_one() {
        let dir = temp_dir();
        let mut s = Server::new(dir.clone(), Vec::new());
        assert!(!dir.exists(), "serve creates nothing");
        assert_eq!(s.state["db"], json!("missing"));
        s.handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"action","params":{"name":"cancel","args":{"id":1}}}"#,
        );
        let out = String::from_utf8(std::mem::take(&mut s.out)).unwrap();
        assert!(out.contains("Nebula has no database yet"), "{out}");
        assert!(!dir.exists());

        let conn = db::open_in(&dir).unwrap();
        let r = state::test_run(&conn, Some("first"), None);
        s.poll();
        assert_eq!(s.state["db"], json!("ok"));
        assert_eq!(s.state["runs"][r.id.to_string()]["state"], json!("pending"));
        db::claim_fetch(&conn, r.id, "http://x").unwrap();
        db::mark_done(&conn, r.id, 0, None).unwrap();
        s.poll();
        let out = String::from_utf8(std::mem::take(&mut s.out)).unwrap();
        assert!(out.contains("first finished"), "{out}");

        // the file is replaced by something that is no database: the runs stay, unconfirmed
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("nebula.db"), vec![b'x'; 5000]).unwrap();
        s.poll();
        let lines: Vec<Value> = String::from_utf8(std::mem::take(&mut s.out))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(s.state["db"], json!("error"));
        assert_eq!(lines[0]["method"], json!("log"));
        assert_eq!(lines[0]["params"]["level"], json!("warn"));
        let changes = lines[1]["params"]["changes"].as_array().unwrap();
        let runs = changes.iter().find(|c| c["path"] == json!("runs")).unwrap();
        assert_eq!(runs["stale"], json!(true));
        assert_eq!(runs["value"][r.id.to_string()]["name"], json!("first"));
        s.handle_line(r#"{"jsonrpc":"2.0","id":2,"method":"state.get"}"#);
        let reply: Value =
            serde_json::from_slice(&std::mem::take(&mut s.out)).unwrap();
        assert_eq!(reply["result"]["stale"], json!(["runs"]));

        // gone again: empty and confirmed
        std::fs::remove_dir_all(&dir).unwrap();
        s.poll();
        assert_eq!(s.state["db"], json!("missing"));
        let out = String::from_utf8(std::mem::take(&mut s.out)).unwrap();
        assert!(out.contains(r#"{"path":"runs","value":{}}"#), "{out}");
    }

    #[test]
    fn a_closed_stdout_ends_it() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let dir = temp_dir();
        let mut s = Server::new(dir, Broken);
        assert!(!s.closed);
        s.handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"describe"}"#);
        assert!(s.closed);
    }
}
