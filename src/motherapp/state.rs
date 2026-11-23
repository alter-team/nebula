//! What the motherapp tool publishes: the state object, the deltas between
//! two of them, and the notifications for runs that ended.
//!
//! motherapp's core caches state and sends it to every page, also to other
//! machines on the LAN. So state holds metadata only. Output, script text,
//! the token and the one-liner never get in here (they exist only in the
//! result of the local-only actions, see `serve.rs`).

use crate::db::{self, DaemonInfo, Run};
use serde_json::{Map, Value, json};
use std::collections::HashSet;

/// One run as the page sees it. Absent values are left out: state never
/// holds `null` (a `null` in a delta deletes the key).
pub fn run_json(r: &Run) -> Value {
    let mut m = Map::new();
    m.insert("label".into(), json!(r.label()));
    m.insert("target".into(), json!(r.target));
    m.insert("state".into(), json!(r.state));
    m.insert("bytes".into(), json!(r.output_bytes));
    m.insert("created".into(), json!(r.created_at));
    m.insert("expires".into(), json!(r.expires_at));
    let mut num = |key: &str, v: Option<i64>| {
        if let Some(v) = v {
            m.insert(key.into(), json!(v));
        }
    };
    num("parent", r.parent_id);
    num("seq", r.seq);
    num("exit", r.exit_code);
    num("reported", r.reported_bytes);
    num("fetched", r.fetched_at);
    num("started", r.started_at);
    num("seen", r.last_seen_at);
    num("finished", r.finished_at);
    let mut text = |key: &str, v: &Option<String>| {
        if let Some(v) = v.as_deref().filter(|v| !v.is_empty()) {
            m.insert(key.into(), json!(v));
        }
    };
    text("name", &r.name);
    text("host", &r.host);
    text("user", &r.user);
    if r.output_truncated {
        m.insert("truncated".into(), json!(true));
    }
    if r.output_deleted {
        m.insert("deleted".into(), json!(true));
    }
    if r.is_multi() {
        // `uses` 0 means any number of fetches until the link expires
        m.insert("multi".into(), json!(true));
        m.insert("uses".into(), json!(r.uses));
        m.insert("fetches".into(), json!(r.fetch_count));
    }
    Value::Object(m)
}

/// Runs keyed by row id (a key must not contain a dot, so not by `42.2`).
pub fn runs_json(runs: &[Run]) -> Value {
    Value::Object(
        runs.iter()
            .map(|r| (r.id.to_string(), run_json(r)))
            .collect(),
    )
}

/// Whether the daemon runs and how its tunnel is, but never the tunnel's
/// address: with it and a token anyone could fetch a script.
pub fn daemon_json(info: &DaemonInfo) -> Value {
    if info.alive {
        json!({ "running": true, "tunnel": info.tunnel_state })
    } else {
        json!({ "running": false, "tunnel": db::TUNNEL_DOWN })
    }
}

/// `total` is the number of top-level runs in the database, `limit` how many
/// of the newest ones are published.
pub fn counts_json(runs: &[Run], total: i64, limit: i64) -> Value {
    json!({
        "open": runs.iter().filter(|r| !r.is_finished()).count(),
        "total": total,
        "limit": limit,
    })
}

/// The changes that turn `old` into `new`, as motherapp `state` deltas:
/// `{ path, value }`, where `value: null` deletes. Objects are compared key
/// by key, so a run that only got more output sends one small change.
pub fn diff(path: &str, old: &Value, new: &Value, out: &mut Vec<Value>) {
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let join = |k: &str| {
                if path.is_empty() {
                    k.to_string()
                } else {
                    format!("{path}.{k}")
                }
            };
            for (k, v) in b {
                match a.get(k) {
                    Some(o) => diff(&join(k), o, v, out),
                    None => out.push(json!({ "path": join(k), "value": v })),
                }
            }
            for k in a.keys().filter(|k| !b.contains_key(*k)) {
                out.push(json!({ "path": join(k), "value": null }));
            }
        }
        _ if old != new => out.push(json!({ "path": path, "value": new })),
        _ => {}
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Notice {
    pub kind: &'static str,
    pub title: String,
}

/// How a run is named in a notification: its name, else `#42`. Never
/// anything from the output. The name is the agent's own text; control
/// characters are dropped and it is clamped.
fn who(r: &Run) -> String {
    let name: String = r
        .name
        .as_deref()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .take(60)
        .collect();
    let name = name.trim();
    match (name.is_empty(), r.parent_id.is_some()) {
        (true, _) => format!("#{}", r.label()),
        (false, false) => name.to_string(),
        // executions of one run share its name
        (false, true) => format!("{name} #{}", r.label()),
    }
}

/// The notification for a run that reached this end state, if it gets one.
/// Cancelling is the user's own doing, and `closed` only says that a link
/// with several uses takes no more fetches.
pub fn notice(r: &Run) -> Option<Notice> {
    let who = who(r);
    let (kind, title) = match r.state.as_str() {
        db::ST_DONE => match r.exit_code {
            Some(0) | None => ("finished", format!("{who} finished")),
            Some(code) => ("failed", format!("{who} failed (exit {code})")),
        },
        db::ST_LOST => ("lost", format!("{who} was lost")),
        db::ST_EXPIRED => ("expired", format!("{who} expired")),
        _ => return None,
    };
    Some(Notice { kind, title })
}

/// Remembers which runs were open, to notice the ones that ended.
#[derive(Default)]
pub struct Tracker {
    primed: bool,
    /// Highest row id seen so far; a higher one is a run made since.
    max_id: i64,
    open: HashSet<i64>,
}

impl Tracker {
    /// Runs that were open at the last look and are not in `runs` now: the
    /// caller looks each one up (it may have ended and left the published
    /// window in the same moment) and passes what it finds to `observe`,
    /// or calls `forget` for a run that is gone.
    pub fn missing(&self, runs: &[Run]) -> Vec<i64> {
        let present: HashSet<i64> = runs.iter().map(|r| r.id).collect();
        let mut ids: Vec<i64> = self.open.difference(&present).copied().collect();
        ids.sort_unstable();
        ids
    }

    pub fn forget(&mut self, id: i64) {
        self.open.remove(&id);
    }

    /// The notifications for what ended since the last call. The first call
    /// only takes stock: nothing is announced for what had already ended
    /// when the tool started.
    pub fn observe(&mut self, runs: &[Run]) -> Vec<Notice> {
        let mut out = Vec::new();
        let mut sorted: Vec<&Run> = runs.iter().collect();
        sorted.sort_by_key(|r| r.id);
        for r in sorted {
            if !r.is_finished() {
                self.open.insert(r.id);
            } else if self.open.remove(&r.id) || (self.primed && r.id > self.max_id) {
                // seen open before, or new since the last look and already over
                // (an execution can start and end between two polls)
                out.extend(notice(r));
            }
        }
        self.max_id = runs.iter().map(|r| r.id).fold(self.max_id, i64::max);
        if !std::mem::replace(&mut self.primed, true) {
            out.clear();
        }
        out
    }
}

/// Hold back bytes at the end of `data` that are the start of a UTF-8
/// character whose rest has not arrived yet: returns how many.
pub fn incomplete_tail(data: &[u8]) -> usize {
    for back in 1..=data.len().min(3) {
        let b = data[data.len() - back];
        if b & 0xC0 == 0x80 {
            continue; // continuation byte, keep looking for its lead byte
        }
        let need = match b {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        return if need > back { back } else { 0 };
    }
    0
}

/// Continuation bytes at the start of `data`: the tail of a character cut
/// off by starting in the middle of the output.
pub fn leading_continuation(data: &[u8]) -> usize {
    data.iter().take(3).take_while(|b| *b & 0xC0 == 0x80).count()
}

/// Where a `view` call reads: `[from, to)` of an output of `total` bytes,
/// at most `cap` bytes, for a page that already has everything before
/// `offset` (None: nothing yet). When there is more than `cap`, the newest
/// bytes are kept and the gap is skipped. With `forward` nothing is skipped:
/// the range starts at `offset` and ends `cap` bytes on (for reading the
/// whole output page by page).
pub fn view_range(total: u64, offset: Option<u64>, cap: u64, forward: bool) -> (u64, u64) {
    if forward {
        let from = offset.unwrap_or(0).min(total);
        return (from, from.saturating_add(cap).min(total));
    }
    let newest = total.saturating_sub(cap);
    let from = match offset {
        // an offset beyond the end: the output was replaced; start over
        Some(o) if o <= total => o.max(newest),
        _ => newest,
    };
    (from, total)
}

/// Keys that may appear in a published run. The test below checks every
/// state against it, so a new field is a decision, not an accident.
#[cfg(test)]
pub const RUN_KEYS: [&str; 22] = [
    "label",
    "target",
    "state",
    "bytes",
    "created",
    "expires",
    "parent",
    "seq",
    "exit",
    "reported",
    "fetched",
    "started",
    "seen",
    "finished",
    "name",
    "host",
    "user",
    "truncated",
    "deleted",
    "multi",
    "uses",
    "fetches",
];

/// For tests in this module and in `serve.rs`. No completion hook: a test
/// that cancels would otherwise start `hook-run` with the test binary.
#[cfg(test)]
pub fn test_run(conn: &rusqlite::Connection, name: Option<&str>, uses: Option<i64>) -> Run {
    let sha = db::put_script(conn, b"echo SCRIPT-SECRET\n").unwrap();
    let token = crate::util::new_token().unwrap();
    let id = db::create_run(
        conn,
        &db::NewRun {
            token: &token,
            target: "remote",
            name,
            note: Some("NOTE-SECRET"),
            script_sha: &sha,
            ttl_secs: 60,
            uses,
            ..Default::default()
        },
    )
    .unwrap();
    db::get_run(conn, id).unwrap().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reread(conn: &rusqlite::Connection, r: &Run) -> Run {
        db::get_run(conn, r.id).unwrap().unwrap()
    }

    #[test]
    fn run_fields_and_nothing_secret() {
        let mut conn = db::open_memory();
        let r = test_run(&conn, Some("disk-check"), None);
        let v = run_json(&r);
        assert_eq!(v["label"], json!(r.id.to_string()));
        assert_eq!(v["state"], json!("pending"));
        assert_eq!(v["name"], json!("disk-check"));
        assert_eq!(v["bytes"], json!(0));
        assert!(v.get("exit").is_none() && v.get("host").is_none());
        assert!(v.get("multi").is_none() && v.get("deleted").is_none());

        db::claim_fetch(&conn, r.id, "https://tunnel-secret.example").unwrap();
        db::mark_started(&conn, r.id, "web-1", "root").unwrap();
        db::append_output(&mut conn, r.id, 0, b"OUTPUT-SECRET\n").unwrap();
        db::mark_done(&conn, r.id, 3, Some(14)).unwrap();
        let from = db::FetchInfo {
            addr: Some("203.0.113.77".into()),
            country: Some("ZQ".into()),
            agent: Some("agent-SECRET/1".into()),
        };
        db::set_fetch_info(&conn, r.id, &from).unwrap();
        db::set_far_side(&conn, r.id, "os-SECRET 1 x", "/dev/pts/SECRET").unwrap();
        let done = reread(&conn, &r);
        assert_eq!(done.fetch_addr.as_deref(), Some("203.0.113.77"));
        let v = run_json(&done);
        assert_eq!(v["state"], json!("done"));
        assert_eq!(v["exit"], json!(3));
        assert_eq!(v["host"], json!("web-1"));
        assert_eq!(v["user"], json!("root"));
        assert_eq!(v["bytes"], json!(14));
        assert_eq!(v["reported"], json!(14));

        let text = runs_json(&[done.clone()]).to_string();
        for secret in [
            done.token.as_str(),
            done.script_sha.as_str(),
            "SECRET",
            "tunnel-secret",
            "203.0.113",
            "ZQ",
        ] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        for key in v.as_object().unwrap().keys() {
            assert!(RUN_KEYS.contains(&key.as_str()), "{key}");
        }
    }

    #[test]
    fn executions_and_multi_runs() {
        let mut conn = db::open_memory();
        let parent = test_run(&conn, None, Some(2));
        let e = db::claim_execution(&mut conn, parent.id, "http", Some("http://x"), None)
            .unwrap()
            .unwrap();
        let p = run_json(&reread(&conn, &parent));
        assert_eq!(p["multi"], json!(true));
        assert_eq!(p["uses"], json!(2));
        assert_eq!(p["fetches"], json!(1));
        assert!(p.get("parent").is_none());
        let c = run_json(&e);
        assert_eq!(c["label"], json!(format!("{}.1", parent.id)));
        assert_eq!(c["parent"], json!(parent.id));
        assert_eq!(c["seq"], json!(1));
        assert_eq!(c["state"], json!("fetched"));
        assert!(c.get("multi").is_none());
        let all = runs_json(&[reread(&conn, &parent), e.clone()]);
        assert!(all.get(parent.id.to_string()).is_some() && all.get(e.id.to_string()).is_some());
    }

    #[test]
    fn daemon_health_has_no_address() {
        let mut info = DaemonInfo {
            alive: true,
            tunnel_state: db::TUNNEL_UP.into(),
            tunnel_url: Some("https://abc.trycloudflare.com".into()),
            tunnel_detail: Some("detail".into()),
            port: Some(7411),
            ..Default::default()
        };
        assert_eq!(daemon_json(&info), json!({"running": true, "tunnel": "up"}));
        info.alive = false;
        assert_eq!(
            daemon_json(&info),
            json!({"running": false, "tunnel": "down"})
        );
    }

    fn changes(old: &Value, new: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        diff("", old, new, &mut out);
        out
    }

    #[test]
    fn deltas_are_small() {
        let old = json!({"db": "ok", "runs": {"1": {"state": "running", "bytes": 10, "host": "a"}}});
        assert!(changes(&old, &old).is_empty());
        // more output: one change
        let new = json!({"db": "ok", "runs": {"1": {"state": "running", "bytes": 25, "host": "a"}}});
        assert_eq!(
            changes(&old, &new),
            vec![json!({"path": "runs.1.bytes", "value": 25})]
        );
        // a new run arrives whole, a removed one is a null, a dropped field too
        let new = json!({"db": "ok", "runs": {"1": {"state": "done", "bytes": 10}, "2": {"state": "pending"}}});
        let mut c = changes(&old, &new);
        c.sort_by_key(|v| v["path"].as_str().unwrap().to_string());
        assert_eq!(
            c,
            vec![
                json!({"path": "runs.1.host", "value": null}),
                json!({"path": "runs.1.state", "value": "done"}),
                json!({"path": "runs.2", "value": {"state": "pending"}}),
            ]
        );
        let gone = json!({"db": "missing", "runs": {}});
        let mut c = changes(&new, &gone);
        c.sort_by_key(|v| v["path"].as_str().unwrap().to_string());
        assert_eq!(
            c,
            vec![
                json!({"path": "db", "value": "missing"}),
                json!({"path": "runs.1", "value": null}),
                json!({"path": "runs.2", "value": null}),
            ]
        );
    }

    fn titles(n: Vec<Notice>) -> Vec<(&'static str, String)> {
        n.into_iter().map(|n| (n.kind, n.title)).collect()
    }

    #[test]
    fn notices_for_runs_that_end() {
        let mut conn = db::open_memory();
        let old = test_run(&conn, Some("old"), None);
        db::claim_fetch(&conn, old.id, "http://x").unwrap();
        db::mark_done(&conn, old.id, 1, None).unwrap();
        let a = test_run(&conn, Some("disk-check"), None);
        let b = test_run(&conn, None, None);
        let c = test_run(&conn, Some("gone"), None);
        let d = test_run(&conn, Some("stopped"), None);
        let all = |conn: &rusqlite::Connection| -> Vec<Run> {
            [&old, &a, &b, &c, &d]
                .iter()
                .map(|r| reread(conn, r))
                .collect()
        };
        let mut t = Tracker::default();
        // the first look announces nothing, not even the failed old run
        assert!(t.observe(&all(&conn)).is_empty());
        assert!(t.observe(&all(&conn)).is_empty());

        for r in [&a, &b, &c] {
            db::claim_fetch(&conn, r.id, "http://x").unwrap();
        }
        assert!(t.observe(&all(&conn)).is_empty(), "fetched is not an end");
        db::mark_done(&conn, a.id, 0, None).unwrap();
        db::mark_done(&conn, b.id, 3, None).unwrap();
        db::append_output(&mut conn, c.id, 0, b"OUTPUT-SECRET").unwrap();
        conn.execute("UPDATE runs SET state = 'lost' WHERE id = ?1", [c.id])
            .unwrap();
        db::cancel_run(&conn, d.id).unwrap();
        assert_eq!(
            titles(t.observe(&all(&conn))),
            vec![
                ("finished", "disk-check finished".to_string()),
                ("failed", format!("#{} failed (exit 3)", b.id)),
                ("lost", "gone was lost".to_string()),
            ],
            "and nothing for the cancelled one"
        );
        // each end is announced once
        assert!(t.observe(&all(&conn)).is_empty());
        assert!(t.missing(&all(&conn)).is_empty());
    }

    #[test]
    fn notices_for_expiry_and_fast_executions() {
        let mut conn = db::open_memory();
        let mut t = Tracker::default();
        assert!(t.observe(&[]).is_empty());
        // a link that expires unused
        let e = test_run(&conn, Some("late"), None);
        assert!(t.observe(&[reread(&conn, &e)]).is_empty());
        conn.execute("UPDATE runs SET expires_at = 0 WHERE id = ?1", [e.id])
            .unwrap();
        db::expire_pending(&conn).unwrap();
        assert_eq!(
            titles(t.observe(&[reread(&conn, &e)])),
            vec![("expired", "late expired".to_string())]
        );
        // an execution that starts and ends between two looks is still announced,
        // with the run's name and its own number; the closed link is not
        let p = test_run(&conn, Some("fleet"), Some(2));
        assert!(t.observe(&[reread(&conn, &p)]).is_empty());
        let mut execs = Vec::new();
        for code in [0, 7] {
            let x = db::claim_execution(&mut conn, p.id, "http", Some("http://x"), None)
                .unwrap()
                .unwrap();
            db::mark_done(&conn, x.id, code, None).unwrap();
            execs.push(reread(&conn, &x));
        }
        let parent = reread(&conn, &p);
        assert_eq!(parent.state, db::ST_CLOSED);
        assert_eq!(
            titles(t.observe(&[parent, execs[0].clone(), execs[1].clone()])),
            vec![
                ("finished", format!("fleet #{}.1 finished", p.id)),
                ("failed", format!("fleet #{}.2 failed (exit 7)", p.id)),
            ]
        );
    }

    #[test]
    fn open_runs_that_leave_the_window() {
        let conn = db::open_memory();
        let a = test_run(&conn, Some("a"), None);
        let mut t = Tracker::default();
        t.observe(&[a.clone()]);
        assert_eq!(t.missing(&[]), vec![a.id]);
        db::claim_fetch(&conn, a.id, "http://x").unwrap();
        db::mark_done(&conn, a.id, 0, None).unwrap();
        // the caller found it again by id
        assert_eq!(
            titles(t.observe(&[reread(&conn, &a)])),
            vec![("finished", "a finished".to_string())]
        );
        // a removed one is forgotten without a word
        let b = test_run(&conn, Some("b"), None);
        t.observe(&[b.clone()]);
        t.forget(b.id);
        assert!(t.missing(&[]).is_empty());
        assert!(t.observe(&[]).is_empty());
    }

    #[test]
    fn titles_never_carry_control_characters() {
        let conn = db::open_memory();
        let r = test_run(&conn, Some("a\nb\x1b[31m  "), None);
        db::claim_fetch(&conn, r.id, "http://x").unwrap();
        db::mark_done(&conn, r.id, 0, None).unwrap();
        assert_eq!(notice(&reread(&conn, &r)).unwrap().title, "ab[31m finished");
        let long = test_run(&conn, Some(&"x".repeat(500)), None);
        assert!(who(&long).len() == 60);
    }

    #[test]
    fn utf8_boundaries() {
        let s = "aé€😀".as_bytes(); // 1 + 2 + 3 + 4 bytes
        assert_eq!(incomplete_tail(s), 0);
        assert_eq!(incomplete_tail(&s[..9]), 3);
        assert_eq!(incomplete_tail(&s[..8]), 2);
        assert_eq!(incomplete_tail(&s[..7]), 1);
        assert_eq!(incomplete_tail(&s[..6]), 0);
        assert_eq!(incomplete_tail(&s[..5]), 2);
        assert_eq!(incomplete_tail(&s[..2]), 1);
        assert_eq!(incomplete_tail(b""), 0);
        assert_eq!(incomplete_tail(b"abc"), 0);
        // bytes that are no UTF-8 at all are not held back for ever
        assert_eq!(incomplete_tail(&[b'a', 0x80, 0x80, 0x80]), 0);
        assert_eq!(incomplete_tail(&[0xFF]), 0);
        assert_eq!(leading_continuation(&s[2..]), 1);
        assert_eq!(leading_continuation(&s[7..]), 3);
        assert_eq!(leading_continuation(s), 0);
        assert_eq!(leading_continuation(&[0x80; 9]), 3);
    }

    #[test]
    fn view_ranges() {
        // first call: the newest `cap` bytes
        assert_eq!(view_range(10, None, 100, false), (0, 10));
        assert_eq!(view_range(1000, None, 100, false), (900, 1000));
        // following: from where the page is
        assert_eq!(view_range(1000, Some(950), 100, false), (950, 1000));
        assert_eq!(view_range(1000, Some(1000), 100, false), (1000, 1000));
        // fell behind by more than the cap: skip to the newest
        assert_eq!(view_range(1000, Some(100), 100, false), (900, 1000));
        // the output shrank (deleted): start over
        assert_eq!(view_range(0, Some(500), 100, false), (0, 0));
        assert_eq!(view_range(50, Some(500), 100, false), (0, 50));
        // paging forward: nothing is skipped, each call ends a cap further on
        assert_eq!(view_range(1000, None, 100, true), (0, 100));
        assert_eq!(view_range(1000, Some(100), 100, true), (100, 200));
        assert_eq!(view_range(1000, Some(950), 100, true), (950, 1000));
        assert_eq!(view_range(1000, Some(5000), 100, true), (1000, 1000));
        assert_eq!(view_range(0, Some(0), 100, true), (0, 0));
    }
}
