//! `nebula daemon`: the HTTP listener with the public run routes, the tunnel
//! supervisor, the sweeper (expiry, lost runs) and the completion hooks.
//! There is deliberately no control API here: the tunnel exposes this whole
//! listener to the internet.

use crate::db::{self, Append};
use crate::hooks;
use crate::tunnel;
use crate::util::{clean_label, now_ms, sha256_hex, valid_token_format};
use crate::wrapper;
use anyhow::{Context, Result, bail};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderMap};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use rusqlite::Connection;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Semaphore, watch};

pub const DEFAULT_PORT: u16 = 7411;
/// Body cap for an output chunk request (the wrapper sends at most 1 MiB).
pub const MAX_OUT_BODY: usize = 8 * 1024 * 1024;
/// Body cap for the small started / heartbeat / done requests.
pub const MAX_SMALL_BODY: usize = 4096;
/// Output uploads buffered at the same time; bounds body memory to
/// `MAX_UPLOADS * MAX_OUT_BODY` (32 MiB).
pub const MAX_UPLOADS: usize = 4;
/// Open connections at the same time; more are closed on accept.
pub const MAX_CONNECTIONS: usize = 128;
/// A connection must send its first byte, and then its complete request
/// head, within this time each.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// A new connection must send its first byte within this time.
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(3);
/// After this much time a request body must have arrived at an average of
/// at least `MIN_BODY_RATE` bytes per second, or the request is dropped
/// (a trickling upload must not hold one of the `MAX_UPLOADS` slots).
const BODY_GRACE: Duration = Duration::from_secs(10);
const MIN_BODY_RATE: f64 = 2.0 * 1024.0;
/// How long an upload may wait for one of the `MAX_UPLOADS` slots.
const UPLOAD_SLOT_WAIT: Duration = Duration::from_secs(15);
/// Time allowed for a request body to arrive.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(60);
/// Hard cap on the life of one connection (one request each, no keep-alive).
const CONN_TIMEOUT: Duration = Duration::from_secs(100);

pub struct App {
    pub db: Mutex<Connection>,
    pub port: u16,
    uploads: Semaphore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Fetch,
    Started,
    Out,
    Heartbeat,
    Done,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Route<'a> {
    pub token: &'a str,
    pub action: Action,
}

/// The complete routing table. Anything this rejects is an empty 404.
pub fn parse_route<'a>(method: &Method, path: &'a str) -> Option<Route<'a>> {
    let rest = path.strip_prefix("/r/")?;
    let (token, action) = match rest.split_once('/') {
        None if method == Method::GET => (rest, Action::Fetch),
        Some((token, tail)) if method == Method::POST => {
            let action = match tail {
                "started" => Action::Started,
                "out" => Action::Out,
                "hb" => Action::Heartbeat,
                "done" => Action::Done,
                _ => return None,
            };
            (token, action)
        }
        _ => return None,
    };
    valid_token_format(token).then_some(Route { token, action })
}

/// `off=<n>` and nothing else.
pub fn parse_offset(query: Option<&str>) -> Option<u64> {
    let v = query?.strip_prefix("off=")?;
    if v.is_empty() || v.len() > 15 || !v.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    v.parse().ok()
}

fn body_field<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    body.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
}

/// Requests arriving through cloudflared carry these; local ones do not.
fn came_through_proxy(h: &HeaderMap) -> bool {
    h.contains_key("cf-connecting-ip")
        || h.contains_key("cf-ray")
        || h.contains_key("x-forwarded-for")
}

/// Longest User-Agent / OS text kept.
pub const MAX_INFO_CHARS: usize = 120;

/// A header's value as text; bytes that are not UTF-8 are replaced, and
/// every user of it cleans or parses the result.
fn header_text(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
}

/// Who fetched a link, as far as the request says. Through the tunnel the
/// socket peer is always cloudflared on 127.0.0.1, so the address comes from
/// `CF-Connecting-IP`, else the first `X-Forwarded-For` entry, else the
/// peer. An address is kept only if it parses as an IP address, the country
/// only as two letters or digits, the User-Agent as capped printable ASCII.
/// None of it is proof: a local process can send any header it likes.
pub fn fetch_info(h: &HeaderMap, peer: Option<std::net::IpAddr>) -> db::FetchInfo {
    let ip = |s: &str| s.trim().parse::<std::net::IpAddr>().ok();
    let addr = header_text(h, "cf-connecting-ip")
        .and_then(|v| ip(&v))
        .or_else(|| {
            header_text(h, "x-forwarded-for").and_then(|v| v.split(',').next().and_then(ip))
        })
        .or(peer)
        .map(|a| a.to_string());
    let country = header_text(h, "cf-ipcountry")
        .map(|c| c.trim().to_ascii_uppercase())
        .filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphanumeric()));
    let agent = header_text(h, "user-agent")
        .map(|a| crate::util::clean_ascii(&a, MAX_INFO_CHARS))
        .filter(|a| !a.is_empty());
    db::FetchInfo {
        addr,
        country,
        agent,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Fetched {
    Wrapper(Vec<u8>),
    NotFound,
    /// A remote run was asked for while no tunnel address is known. The link
    /// is not consumed.
    Unavailable,
}

/// `GET /r/<token>`: single use, pending only, before the TTL.
#[cfg(test)]
pub fn do_fetch(conn: &mut Connection, token: &str, via_proxy: bool, port: u16) -> Result<Fetched> {
    do_fetch_from(conn, token, via_proxy, port, &db::FetchInfo::default())
}

/// [`do_fetch`], recording `from` on the run or execution the fetch starts.
/// `from` never decides anything: which requests get the wrapper is the same
/// with or without it.
pub fn do_fetch_from(
    conn: &mut Connection,
    token: &str,
    via_proxy: bool,
    port: u16,
    from: &db::FetchInfo,
) -> Result<Fetched> {
    let Some(run) = db::get_run_by_token(conn, token)? else {
        return Ok(Fetched::NotFound);
    };
    // only a top-level run's link can be fetched; executions have no link
    if run.parent_id.is_some()
        || run.state != db::ST_PENDING
        || run.fetched_at.is_some() && !run.is_multi()
        || now_ms() >= run.expires_at
    {
        return Ok(Fetched::NotFound);
    }
    // The callback address comes from our own records, never from the Host header.
    let origin = if run.is_remote() {
        let state = db::get_kv(conn, "tunnel_state")?.unwrap_or_default();
        match db::get_kv(conn, "tunnel_url")?.filter(|u| !u.is_empty()) {
            Some(u) if state == db::TUNNEL_UP => u,
            _ => return Ok(Fetched::Unavailable),
        }
    } else {
        if via_proxy {
            // local runs are never served through the tunnel
            return Ok(Fetched::NotFound);
        }
        format!("http://127.0.0.1:{port}")
    };
    if !wrapper::base_is_safe(&format!("{origin}/r/{}", run.token)) {
        return Ok(Fetched::Unavailable);
    }
    let Some(script) = db::get_script(conn, &run.script_sha)? else {
        return Ok(Fetched::NotFound);
    };
    if sha256_hex(&script) != run.script_sha {
        eprintln!(
            "nebula daemon: run {} script body does not match its hash",
            run.id
        );
        return Ok(Fetched::NotFound);
    }
    // The wrapper reports to the token of what it executes: the run itself,
    // or, for a multi-use link, a fresh execution with its own token, so one
    // execution can never post into another's output.
    let (exec_id, exec_token, label) = if run.is_multi() {
        match db::claim_execution(conn, run.id, "http", Some(&origin), None)? {
            Some(exec) => (exec.id, exec.token.clone(), exec.label()),
            None => return Ok(Fetched::NotFound),
        }
    } else {
        if !db::claim_fetch(conn, run.id, &origin)? {
            return Ok(Fetched::NotFound);
        }
        (run.id, run.token.clone(), run.label())
    };
    // Bookkeeping only; a failure here must not cost the fetch its wrapper.
    if let Err(e) = db::set_fetch_info(conn, exec_id, from) {
        eprintln!("nebula daemon: run {label}: could not record the fetch address: {e:#}");
    }
    eprintln!("nebula daemon: run {label} fetched");
    let base = format!("{origin}/r/{exec_token}");
    Ok(Fetched::Wrapper(wrapper::generate(
        &wrapper::WrapperParams {
            run_id: &label,
            base: &base,
            script: &script,
            echo: run.echo,
        },
    )))
}

/// Cheap check done BEFORE any request body is read: is there a run with
/// this token that currently accepts posts (from this side)?
///
/// An accepted request is a sign of life for stale/lost detection from the
/// moment it starts, so a long upload over a slow link keeps its run alive.
pub fn post_allowed(conn: &Connection, token: &str, via_proxy: bool) -> Result<bool> {
    let Some(run) = db::get_run_by_token(conn, token)? else {
        return Ok(false);
    };
    if !run.is_active() || (via_proxy && !run.is_remote()) {
        return Ok(false);
    }
    db::touch(conn, run.id)?;
    Ok(true)
}

/// The POST routes. Returns (status, body). Valid only between fetch and done.
pub fn do_post(
    conn: &mut Connection,
    token: &str,
    action: Action,
    via_proxy: bool,
    offset: Option<u64>,
    body: &[u8],
) -> Result<(u16, String)> {
    const NOT_FOUND: (u16, String) = (404, String::new());
    let Some(run) = db::get_run_by_token(conn, token)? else {
        return Ok(NOT_FOUND);
    };
    if !run.is_active() || (via_proxy && !run.is_remote()) {
        return Ok(NOT_FOUND);
    }
    let ok = |done: bool| {
        if done {
            (200, "ok\n".to_string())
        } else {
            NOT_FOUND
        }
    };
    Ok(match action {
        Action::Fetch => NOT_FOUND,
        Action::Started => {
            let text = String::from_utf8_lossy(body);
            let host = clean_label(body_field(&text, "host").unwrap_or(""));
            let user = clean_label(body_field(&text, "user").unwrap_or(""));
            // optional: wrappers from before this field do not send it
            let os = crate::util::clean_ascii(body_field(&text, "os").unwrap_or(""), MAX_INFO_CHARS);
            let started = db::mark_started(conn, run.id, &host, &user)?;
            if started && !os.is_empty() {
                db::set_far_side(conn, run.id, &os, "")?;
            }
            ok(started)
        }
        Action::Out => {
            let Some(off) = offset else {
                return Ok((400, String::new()));
            };
            match db::append_output(conn, run.id, off, body)? {
                Append::Appended | Append::Duplicate => (200, "ok\n".into()),
                Append::Full => (200, "full\n".into()),
                Append::Gap { expected } => (409, format!("gap {expected}\n")),
                Append::NotActive => NOT_FOUND,
            }
        }
        Action::Heartbeat => ok(db::touch(conn, run.id)?),
        Action::Done => {
            let text = String::from_utf8_lossy(body);
            let code = body_field(&text, "code").and_then(|c| c.trim().parse::<i64>().ok());
            let Some(code) = code.filter(|c| (0..=255).contains(c)) else {
                return Ok((400, String::new()));
            };
            let bytes = body_field(&text, "bytes")
                .and_then(|c| c.trim().parse::<i64>().ok())
                .filter(|b| *b >= 0);
            let done = db::mark_done(conn, run.id, code, bytes)?;
            if done {
                eprintln!("nebula daemon: run {} done exit={code}", run.id);
            }
            ok(done)
        }
    })
}

type Resp = Response<Full<Bytes>>;

fn reply(status: u16, content_type: Option<&'static str>, body: Vec<u8>) -> Resp {
    let mut b = Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::NOT_FOUND))
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::CONNECTION, "close");
    if let Some(ct) = content_type {
        b = b.header(header::CONTENT_TYPE, ct);
    }
    b.body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

fn empty(status: u16) -> Resp {
    reply(status, None, Vec::new())
}

async fn handle(
    app: Arc<App>,
    peer: Option<std::net::IpAddr>,
    req: Request<Incoming>,
) -> Result<Resp, Infallible> {
    Ok(respond(app, peer, req).await)
}

async fn respond(app: Arc<App>, peer: Option<std::net::IpAddr>, req: Request<Incoming>) -> Resp {
    let (parts, body) = req.into_parts();
    let Some(route) = parse_route(&parts.method, parts.uri.path()) else {
        return empty(404);
    };
    let token = route.token.to_string();
    let action = route.action;
    let via_proxy = came_through_proxy(&parts.headers);

    if action == Action::Fetch {
        // read from headers hyper has already parsed and bounded; nothing more is read for it
        let from = fetch_info(&parts.headers, peer);
        let res = tokio::task::spawn_blocking(move || {
            let mut conn = app.db.lock().unwrap_or_else(|e| e.into_inner());
            do_fetch_from(&mut conn, &token, via_proxy, app.port, &from)
        })
        .await;
        return match res {
            Ok(Ok(Fetched::Wrapper(w))) => reply(200, Some("text/x-shellscript; charset=utf-8"), w),
            Ok(Ok(Fetched::NotFound)) => empty(404),
            Ok(Ok(Fetched::Unavailable)) => empty(503),
            Ok(Err(e)) => {
                eprintln!("nebula daemon: fetch failed: {e:#}");
                empty(500)
            }
            Err(_) => empty(500),
        };
    }

    // Everything below is decided before a single body byte is read.
    let offset = parse_offset(parts.uri.query());
    if action == Action::Out && offset.is_none() {
        return empty(400);
    }
    let limit = if action == Action::Out {
        MAX_OUT_BODY
    } else {
        MAX_SMALL_BODY
    };
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > limit) {
        return empty(413);
    }
    let allowed = {
        let app = app.clone();
        let token = token.clone();
        tokio::task::spawn_blocking(move || {
            let conn = app.db.lock().unwrap_or_else(|e| e.into_inner());
            post_allowed(&conn, &token, via_proxy)
        })
        .await
    };
    match allowed {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => return empty(404),
        _ => return empty(500),
    }
    // Only a limited number of large bodies are buffered at once.
    let _slot = if action == Action::Out {
        match tokio::time::timeout(UPLOAD_SLOT_WAIT, app.uploads.acquire()).await {
            Ok(Ok(permit)) => Some(permit),
            _ => return empty(503),
        }
    } else {
        None
    };
    let bytes = match read_body(body, limit).await {
        Ok(b) => b,
        Err(status) => return empty(status),
    };
    let res = {
        let app = app.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = app.db.lock().unwrap_or_else(|e| e.into_inner());
            do_post(&mut conn, &token, action, via_proxy, offset, &bytes)
        })
        .await
    };
    match res {
        Ok(Ok((status, body))) if body.is_empty() => empty(status),
        Ok(Ok((status, body))) => reply(status, Some("text/plain"), body.into_bytes()),
        Ok(Err(e)) => {
            eprintln!("nebula daemon: post failed: {e:#}");
            empty(500)
        }
        Err(_) => empty(500),
    }
}

/// Read a request body of at most `limit` bytes, giving up (408) after
/// `BODY_TIMEOUT` or as soon as it arrives slower than the rate floor.
async fn read_body(mut body: Incoming, limit: usize) -> Result<Vec<u8>, u16> {
    let start = tokio::time::Instant::now();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if start.elapsed() >= BODY_TIMEOUT {
            return Err(408);
        }
        match tokio::time::timeout(Duration::from_secs(1), body.frame()).await {
            Ok(None) => return Ok(buf),
            Ok(Some(Err(_))) => return Err(400),
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    if buf.len() + data.len() > limit {
                        return Err(413);
                    }
                    buf.extend_from_slice(&data);
                }
            }
            Err(_) => {} // nothing this second: check the floor
        }
        let el = start.elapsed();
        if el > BODY_GRACE && (buf.len() as f64) < (el - BODY_GRACE).as_secs_f64() * MIN_BODY_RATE {
            return Err(408);
        }
    }
}

/// One connection: first byte within `FIRST_BYTE_TIMEOUT`, request head within `HEADER_TIMEOUT`
/// each, one request, no keep-alive, hard cap on the total time.
async fn serve_connection(app: Arc<App>, stream: tokio::net::TcpStream) {
    let mut first = [0u8; 1];
    match tokio::time::timeout(FIRST_BYTE_TIMEOUT, stream.peek(&mut first)).await {
        Ok(Ok(n)) if n > 0 => {}
        _ => return, // idle, closed, or failed: drop it
    }
    let peer = stream.peer_addr().ok().map(|a| a.ip());
    let service = hyper::service::service_fn(move |req| handle(app.clone(), peer, req));
    let conn = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_TIMEOUT)
        .keep_alive(false)
        .serve_connection(TokioIo::new(stream), service);
    let _ = tokio::time::timeout(CONN_TIMEOUT, conn).await;
}

pub fn configured_port() -> Result<u16> {
    match std::env::var("NEBULA_PORT") {
        Ok(v) if !v.trim().is_empty() => v
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .with_context(|| format!("NEBULA_PORT={v:?} is not a valid port")),
        _ => Ok(DEFAULT_PORT),
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

/// Housekeeping on its own thread and its own connection, so a slow hook
/// never holds up HTTP requests: expire links, mark lost runs, fire
/// completion hooks, and keep the WAL from holding on to deleted data.
fn housekeeping(stop: Arc<AtomicBool>) {
    let conn = match db::open() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nebula daemon: housekeeping cannot open the database: {e:#}");
            return;
        }
    };
    let wal = db::db_path()
        .map(|p| p.with_extension("db-wal"))
        .unwrap_or_default();
    let mut tick: u64 = 0;
    while !stop.load(Ordering::SeqCst) {
        if tick.is_multiple_of(4)
            && let Err(e) = db::sweep(&conn)
        {
            eprintln!("nebula daemon: sweeper: {e:#}");
        }
        // not joined: each hook runs (with its own timeout) on its own thread
        let _ = hooks::fire_pending(&conn);
        if tick % 60 == 59 && std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
            let _: rusqlite::Result<i64> =
                conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0));
        }
        tick += 1;
        std::thread::sleep(Duration::from_millis(500));
    }
}

pub fn run() -> Result<i32> {
    let port = configured_port()?;
    let conn = db::open()?;
    let prev = db::daemon_info(&conn)?;
    if let Some(pid) = prev.pid
        && pid != std::process::id()
        && db::pid_is_nebula_daemon(pid)
    {
        bail!(
            "a nebula daemon is already running on this database (pid {pid}, port {})",
            prev.port.map(|p| p.to_string()).unwrap_or_default()
        );
    }
    tunnel::reap_stale_cloudflared(&conn);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(serve(conn, port))?;
    Ok(0)
}

async fn serve(conn: Connection, port: u16) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?;

    let no_tunnel = std::env::var("NEBULA_NO_TUNNEL").is_ok_and(|v| !v.is_empty() && v != "0");
    let external = match std::env::var("NEBULA_TUNNEL_URL") {
        Ok(v) if !v.trim().is_empty() => Some(
            tunnel::normalize_external_url(&v)
                .with_context(|| format!("NEBULA_TUNNEL_URL={v:?} is not scheme://host[:port]"))?,
        ),
        _ => None,
    };

    db::set_kv(&conn, "pid", &std::process::id().to_string())?;
    db::set_kv(&conn, "port", &port.to_string())?;
    db::set_kv(&conn, "started_at", &now_ms().to_string())?;
    db::set_kv(&conn, "version", env!("CARGO_PKG_VERSION"))?;
    let (state, url, detail) = if let Some(u) = &external {
        (
            db::TUNNEL_UP,
            u.as_str(),
            "fixed address from NEBULA_TUNNEL_URL",
        )
    } else if no_tunnel {
        (db::TUNNEL_DISABLED, "", "disabled by NEBULA_NO_TUNNEL")
    } else {
        (db::TUNNEL_STARTING, "", "")
    };
    db::set_kv(&conn, "tunnel_state", state)?;
    db::set_kv(&conn, "tunnel_url", url)?;
    db::set_kv(&conn, "tunnel_detail", detail)?;
    db::sweep(&conn)?;

    let app = Arc::new(App {
        db: Mutex::new(conn),
        port,
        uploads: Semaphore::new(MAX_UPLOADS),
    });
    eprintln!(
        "nebula daemon: listening on 127.0.0.1:{port}, database {}",
        db::db_path()?.display()
    );

    let (stop_tx, stop_rx) = watch::channel(false);
    let tunnel_task = if external.is_none() && !no_tunnel {
        Some(tokio::spawn(tunnel::supervise(app.clone(), port, stop_rx)))
    } else {
        None
    };

    let stop_housekeeping = Arc::new(AtomicBool::new(false));
    let housekeeper = {
        let stop = stop_housekeeping.clone();
        std::thread::spawn(move || housekeeping(stop))
    };

    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut shutdown = std::pin::pin!(shutdown_signal());
    loop {
        let accepted = tokio::select! {
            _ = &mut shutdown => break,
            a = listener.accept() => a,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(e) => {
                eprintln!("nebula daemon: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // over the limit: the connection is closed right away
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            continue;
        };
        let app = app.clone();
        tokio::spawn(async move {
            serve_connection(app, stream).await;
            drop(permit);
        });
    }

    eprintln!("nebula daemon: shutting down");
    drop(listener);
    stop_housekeeping.store(true, Ordering::SeqCst);
    let _ = stop_tx.send(true);
    if let Some(t) = tunnel_task {
        let _ = tokio::time::timeout(Duration::from_secs(10), t).await;
    }
    let _ = tokio::task::spawn_blocking(move || housekeeper.join()).await;
    {
        let conn = app.db.lock().unwrap_or_else(|e| e.into_inner());
        let _ = db::set_kv(&conn, "pid", "");
        let _ = db::set_kv(&conn, "tunnel_state", db::TUNNEL_DOWN);
        let _ = db::set_kv(&conn, "tunnel_url", "");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::NewRun;

    const TOK: &str = "abcdefghijklmnopqrstuvwxyz012345";

    #[test]
    fn routes() {
        let get = Method::GET;
        let post = Method::POST;
        assert_eq!(
            parse_route(&get, &format!("/r/{TOK}")),
            Some(Route {
                token: TOK,
                action: Action::Fetch
            })
        );
        for (tail, action) in [
            ("started", Action::Started),
            ("out", Action::Out),
            ("hb", Action::Heartbeat),
            ("done", Action::Done),
        ] {
            assert_eq!(
                parse_route(&post, &format!("/r/{TOK}/{tail}")),
                Some(Route { token: TOK, action })
            );
            // wrong method
            assert_eq!(parse_route(&get, &format!("/r/{TOK}/{tail}")), None);
        }
        // everything else is rejected
        for (m, p) in [
            (&get, "/".to_string()),
            (&get, "/r".to_string()),
            (&get, "/r/".to_string()),
            (&get, "/status".to_string()),
            (&get, "/admin".to_string()),
            (&get, format!("/r/{TOK}/")),
            (&get, format!("/r/{}", &TOK[1..])),
            (&get, format!("/r/{TOK}x")),
            (&get, format!("/R/{TOK}")),
            (&get, format!("//r/{TOK}")),
            (&get, format!("/r/{}%41", &TOK[1..])),
            (&post, format!("/r/{TOK}")),
            (&post, format!("/r/{TOK}/out/")),
            (&post, format!("/r/{TOK}/out/x")),
            (&post, format!("/r/{TOK}/cancel")),
            (&post, format!("/r/../{TOK}/out")),
            (&Method::PUT, format!("/r/{TOK}/out")),
            (&Method::HEAD, format!("/r/{TOK}")),
            (&Method::DELETE, format!("/r/{TOK}")),
        ] {
            assert_eq!(parse_route(m, &p), None, "{m} {p}");
        }
    }

    #[test]
    fn offsets() {
        assert_eq!(parse_offset(Some("off=0")), Some(0));
        assert_eq!(parse_offset(Some("off=1048576")), Some(1_048_576));
        assert_eq!(parse_offset(None), None);
        assert_eq!(parse_offset(Some("off=")), None);
        assert_eq!(parse_offset(Some("off=-1")), None);
        assert_eq!(parse_offset(Some("off=1&x=2")), None);
        assert_eq!(parse_offset(Some("offset=1")), None);
        assert_eq!(parse_offset(Some("off=99999999999999999999")), None);
    }

    fn mk(conn: &Connection, target: &str, ttl: i64) -> (i64, String) {
        let sha = db::put_script(conn, b"echo hi\n").unwrap();
        let token = crate::util::new_token().unwrap();
        let id = db::create_run(
            conn,
            &NewRun {
                token: &token,
                target,
                name: None,
                note: None,
                script_sha: &sha,
                echo: false,
                delete_after: false,
                ttl_secs: ttl,
                ..Default::default()
            },
        )
        .unwrap();
        (id, token)
    }

    fn wrapper_text(f: Fetched) -> String {
        match f {
            Fetched::Wrapper(w) => String::from_utf8(w).unwrap(),
            other => panic!("expected wrapper, got {other:?}"),
        }
    }

    #[test]
    fn local_fetch_lifecycle() {
        let mut conn = db::open_memory();
        let (id, token) = mk(&conn, "local", 60);

        // posts before the fetch are refused
        assert_eq!(
            do_post(&mut conn, &token, Action::Started, false, None, b"host=a\n")
                .unwrap()
                .0,
            404
        );
        // not through the tunnel
        assert_eq!(
            do_fetch(&mut conn, &token, true, 7411).unwrap(),
            Fetched::NotFound
        );
        // unknown token
        assert_eq!(
            do_fetch(&mut conn, TOK, false, 7411).unwrap(),
            Fetched::NotFound
        );

        let w = wrapper_text(do_fetch(&mut conn, &token, false, 7411).unwrap());
        assert!(w.contains(&format!("NB_BASE='http://127.0.0.1:7411/r/{token}'")));
        // single use
        assert_eq!(
            do_fetch(&mut conn, &token, false, 7411).unwrap(),
            Fetched::NotFound
        );

        let post = |conn: &mut Connection, a, off, body: &[u8]| {
            do_post(conn, &token, a, false, off, body).unwrap()
        };
        assert_eq!(
            post(&mut conn, Action::Started, None, b"host=web-1\nuser=root\n"),
            (200, "ok\n".into())
        );
        assert_eq!(post(&mut conn, Action::Out, None, b"x").0, 400);
        assert_eq!(post(&mut conn, Action::Out, Some(0), b"abc").0, 200);
        assert_eq!(post(&mut conn, Action::Out, Some(0), b"abc").0, 200);
        assert_eq!(
            post(&mut conn, Action::Out, Some(9), b"abc"),
            (409, "gap 3\n".into())
        );
        assert_eq!(post(&mut conn, Action::Heartbeat, None, b"hb").0, 200);
        assert_eq!(post(&mut conn, Action::Done, None, b"code=abc\n").0, 400);
        assert_eq!(post(&mut conn, Action::Done, None, b"code=999\n").0, 400);
        // posts through the tunnel are refused for a local run
        assert_eq!(
            do_post(&mut conn, &token, Action::Done, true, None, b"code=0\n")
                .unwrap()
                .0,
            404
        );
        assert_eq!(
            post(&mut conn, Action::Done, None, b"code=7\nbytes=3\n"),
            (200, "ok\n".into())
        );
        // nothing is accepted after done
        for a in [
            Action::Started,
            Action::Out,
            Action::Heartbeat,
            Action::Done,
        ] {
            assert_eq!(post(&mut conn, a, Some(3), b"code=0\n").0, 404, "{a:?}");
        }
        let run = db::get_run(&conn, id).unwrap().unwrap();
        assert_eq!(run.state, db::ST_DONE);
        assert_eq!(run.exit_code, Some(7));
        assert_eq!(run.host.as_deref(), Some("web-1"));
        assert_eq!(run.user.as_deref(), Some("root"));
        assert_eq!(run.output_bytes, 3);
    }

    #[test]
    fn posts_are_refused_before_the_body_is_read() {
        let conn = db::open_memory();
        let (id, token) = mk(&conn, "local", 60);
        // unknown token, pending run
        assert!(!post_allowed(&conn, TOK, false).unwrap());
        assert!(!post_allowed(&conn, &token, false).unwrap());
        db::claim_fetch(&conn, id, "http://x").unwrap();
        assert!(post_allowed(&conn, &token, false).unwrap());
        // a local run never through the tunnel
        assert!(!post_allowed(&conn, &token, true).unwrap());
        db::mark_done(&conn, id, 0, None).unwrap();
        assert!(!post_allowed(&conn, &token, false).unwrap());
        let (id, token) = mk(&conn, "remote", 60);
        db::claim_fetch(&conn, id, "http://x").unwrap();
        assert!(post_allowed(&conn, &token, true).unwrap());
        conn.execute("UPDATE runs SET state = 'lost' WHERE id = ?1", [id])
            .unwrap();
        assert!(!post_allowed(&conn, &token, true).unwrap());
    }

    #[test]
    fn expired_link_is_not_served() {
        let mut conn = db::open_memory();
        let (_, token) = mk(&conn, "local", 0);
        assert_eq!(
            do_fetch(&mut conn, &token, false, 7411).unwrap(),
            Fetched::NotFound
        );
    }

    #[test]
    fn remote_fetch_uses_recorded_tunnel_address() {
        let mut conn = db::open_memory();
        let (id, token) = mk(&conn, "remote", 60);
        // no tunnel: unavailable, and the link is not consumed
        assert_eq!(
            do_fetch(&mut conn, &token, true, 7411).unwrap(),
            Fetched::Unavailable
        );
        assert_eq!(
            db::get_run(&conn, id).unwrap().unwrap().state,
            db::ST_PENDING
        );
        db::set_kv(&conn, "tunnel_state", db::TUNNEL_UP).unwrap();
        db::set_kv(&conn, "tunnel_url", "https://a-b.trycloudflare.com").unwrap();
        let w = wrapper_text(do_fetch(&mut conn, &token, true, 7411).unwrap());
        assert!(w.contains(&format!(
            "NB_BASE='https://a-b.trycloudflare.com/r/{token}'"
        )));
        assert_eq!(
            do_fetch(&mut conn, &token, true, 7411).unwrap(),
            Fetched::NotFound
        );
    }

    #[test]
    fn cancelled_run_exposes_nothing() {
        let mut conn = db::open_memory();
        let (id, token) = mk(&conn, "local", 60);
        db::cancel_run(&conn, id).unwrap();
        assert_eq!(
            do_fetch(&mut conn, &token, false, 7411).unwrap(),
            Fetched::NotFound
        );
        assert_eq!(
            do_post(&mut conn, &token, Action::Heartbeat, false, None, b"hb")
                .unwrap()
                .0,
            404
        );
    }

    fn mk_multi(conn: &Connection, target: &str, uses: i64, ttl: i64) -> (i64, String) {
        let sha = db::put_script(conn, b"echo hi\n").unwrap();
        let token = crate::util::new_token().unwrap();
        let id = db::create_run(
            conn,
            &NewRun {
                token: &token,
                target,
                script_sha: &sha,
                ttl_secs: ttl,
                uses: Some(uses),
                on_done: Some("true"),
                ..Default::default()
            },
        )
        .unwrap();
        (id, token)
    }

    fn exec_token(w: &str) -> String {
        let start = w.find("NB_BASE='").unwrap() + 9;
        let base = &w[start..start + w[start..].find('\'').unwrap()];
        base.rsplit('/').next().unwrap().to_string()
    }

    fn headers(pairs: &[(&'static str, &[u8])]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            // a header value cannot hold control characters at all, so there is nothing to test
            if let Ok(v) = hyper::header::HeaderValue::from_bytes(v) {
                h.insert(*k, v);
            }
        }
        h
    }

    #[test]
    fn fetch_info_from_headers() {
        let peer = Some("127.0.0.1".parse().unwrap());
        let info = |pairs: &[(&'static str, &[u8])]| fetch_info(&headers(pairs), peer);
        let some = |s: &str| Some(s.to_string());
        // nothing but the socket: the peer
        assert_eq!(
            info(&[]),
            db::FetchInfo {
                addr: some("127.0.0.1"),
                country: None,
                agent: None
            }
        );
        assert_eq!(fetch_info(&HeaderMap::new(), None), db::FetchInfo::default());
        // through cloudflared
        assert_eq!(
            info(&[
                ("cf-connecting-ip", b"203.0.113.9"),
                ("x-forwarded-for", b"198.51.100.1, 10.0.0.1"),
                ("cf-ipcountry", b"de"),
                ("user-agent", b"curl/8.5.0"),
            ]),
            db::FetchInfo {
                addr: some("203.0.113.9"),
                country: some("DE"),
                agent: some("curl/8.5.0")
            }
        );
        // another proxy: the first forwarded entry; IPv6 is normalised
        assert_eq!(
            info(&[("x-forwarded-for", b" 2001:DB8::0:1 , 10.0.0.1")]).addr,
            some("2001:db8::1")
        );
        // anything that is not an address is not stored: the next source is used
        for bad in [
            &b"203.0.113.9; rm -rf /"[..],
            b"<script>",
            b"999.1.1.1",
            b"",
            b"203.0.113.9\x1b[31m",
            b"\xff\xfe",
        ] {
            assert_eq!(info(&[("cf-connecting-ip", bad)]).addr, some("127.0.0.1"), "{bad:?}");
            assert_eq!(
                info(&[("cf-connecting-ip", bad), ("x-forwarded-for", b"198.51.100.1")]).addr,
                some("198.51.100.1")
            );
            assert_eq!(info(&[("x-forwarded-for", bad)]).addr, some("127.0.0.1"));
        }
        for bad in [&b"DEU"[..], b"D", b"", b"D\x1b", b"<>", b"\xc3\xa9"] {
            assert_eq!(info(&[("cf-ipcountry", bad)]).country, None, "{bad:?}");
        }
        assert_eq!(info(&[("cf-ipcountry", b"T1")]).country, some("T1"));
        // the User-Agent: printable ASCII, capped
        let long = [b'A'; 5000];
        assert_eq!(info(&[("user-agent", &long)]).agent.unwrap().len(), MAX_INFO_CHARS);
        assert_eq!(
            info(&[("user-agent", b"Wget/1.21.4\xff\xc3\xa9[2J  \t<b>x</b>")]).agent,
            some("Wget/1.21.4[2J <b>x</b>")
        );
        assert_eq!(info(&[("user-agent", b"\xff\xfe")]).agent, None);
        assert_eq!(info(&[("user-agent", b"   ")]).agent, None);
    }

    #[test]
    fn fetch_address_and_os_are_recorded_per_execution() {
        let mut conn = db::open_memory();
        let from = |n: u8| db::FetchInfo {
            addr: Some(format!("203.0.113.{n}")),
            country: Some("NL".into()),
            agent: Some(format!("curl/8.{n}")),
        };
        // a single-use run
        let (id, token) = mk(&conn, "remote", 60);
        db::set_kv(&conn, "tunnel_state", db::TUNNEL_UP).unwrap();
        db::set_kv(&conn, "tunnel_url", "https://t.example").unwrap();
        assert!(matches!(
            do_fetch_from(&mut conn, &token, true, 7411, &from(1)).unwrap(),
            Fetched::Wrapper(_)
        ));
        // a second fetch is refused as before and changes nothing
        assert_eq!(
            do_fetch_from(&mut conn, &token, true, 7411, &from(2)).unwrap(),
            Fetched::NotFound
        );
        let r = db::get_run(&conn, id).unwrap().unwrap();
        assert_eq!(r.fetch_addr.as_deref(), Some("203.0.113.1"));
        assert_eq!(r.fetch_country.as_deref(), Some("NL"));
        assert_eq!(r.fetch_agent.as_deref(), Some("curl/8.1"));
        assert!(r.far_os.is_none());
        // the wrapper's "started" report carries the OS; it is cleaned and capped
        let body = format!("host=web-1\nuser=root\nos=Linux 6.8.0\x1b[31m  x86_64{}\n", "y".repeat(500));
        assert_eq!(
            do_post(&mut conn, &token, Action::Started, true, None, body.as_bytes()).unwrap().0,
            200
        );
        let r = db::get_run(&conn, id).unwrap().unwrap();
        let os = r.far_os.unwrap();
        assert!(os.starts_with("Linux 6.8.0[31m x86_64y") && os.len() == MAX_INFO_CHARS, "{os}");
        // an old wrapper sends no os: still started, nothing stored
        let (old, old_token) = mk(&conn, "remote", 60);
        do_fetch(&mut conn, &old_token, true, 7411).unwrap();
        assert_eq!(
            do_post(&mut conn, &old_token, Action::Started, true, None, b"host=h\nuser=u\n").unwrap().0,
            200
        );
        let r = db::get_run(&conn, old).unwrap().unwrap();
        assert_eq!(r.state, db::ST_RUNNING);
        assert!(r.far_os.is_none() && r.fetch_addr.is_none());

        // a multi-use link: each execution has its own values, the run itself none
        let (multi, mtoken) = mk_multi(&conn, "remote", 2, 60);
        let mut execs = Vec::new();
        for n in [7, 8] {
            let w = wrapper_text(do_fetch_from(&mut conn, &mtoken, true, 7411, &from(n)).unwrap());
            let t = exec_token(&w);
            let os = format!("host=h{n}\nuser=u\nos=Linux 6.{n} arm64\n");
            do_post(&mut conn, &t, Action::Started, true, None, os.as_bytes()).unwrap();
            execs.push(db::get_run_by_token(&conn, &t).unwrap().unwrap());
        }
        assert_eq!(execs[0].fetch_addr.as_deref(), Some("203.0.113.7"));
        assert_eq!(execs[1].fetch_addr.as_deref(), Some("203.0.113.8"));
        assert_eq!(execs[0].far_os.as_deref(), Some("Linux 6.7 arm64"));
        assert_eq!(execs[1].far_os.as_deref(), Some("Linux 6.8 arm64"));
        assert_eq!(execs[1].fetch_agent.as_deref(), Some("curl/8.8"));
        let parent = db::get_run(&conn, multi).unwrap().unwrap();
        assert!(parent.fetch_addr.is_none() && parent.far_os.is_none());
    }

    #[test]
    fn multi_use_link() {
        let mut conn = db::open_memory();
        let (id, token) = mk_multi(&conn, "local", 3, 60);
        let mut tokens = Vec::new();
        for n in 1..=3 {
            let w = wrapper_text(do_fetch(&mut conn, &token, false, 7411).unwrap());
            assert!(w.contains(&format!("NB_RUN={id}.{n}\n")), "{n}");
            let t = exec_token(&w);
            assert_ne!(t, token, "an execution posts to its own token");
            tokens.push(t);
        }
        // the 4th fetch: used up, plain 404
        assert_eq!(
            do_fetch(&mut conn, &token, false, 7411).unwrap(),
            Fetched::NotFound
        );
        let parent = db::get_run(&conn, id).unwrap().unwrap();
        assert_eq!(parent.state, db::ST_CLOSED);
        assert_eq!(parent.fetch_count, 3);
        // the parent token takes no posts, an execution token cannot be fetched
        assert_eq!(
            do_post(&mut conn, &token, Action::Heartbeat, false, None, b"hb")
                .unwrap()
                .0,
            404
        );
        assert_eq!(
            do_fetch(&mut conn, &tokens[0], false, 7411).unwrap(),
            Fetched::NotFound
        );
        // each execution has its own output and state
        let post = |conn: &mut Connection, t: &str, a, off, body: &[u8]| {
            do_post(conn, t, a, false, off, body).unwrap().0
        };
        assert_eq!(
            post(&mut conn, &tokens[0], Action::Out, Some(0), b"one"),
            200
        );
        assert_eq!(
            post(&mut conn, &tokens[1], Action::Out, Some(0), b"two!"),
            200
        );
        assert_eq!(
            post(&mut conn, &tokens[0], Action::Done, None, b"code=0"),
            200
        );
        assert_eq!(
            post(&mut conn, &tokens[1], Action::Done, None, b"code=4"),
            200
        );
        // a finished execution's token is dead, the others are unaffected
        assert_eq!(post(&mut conn, &tokens[0], Action::Out, Some(3), b"x"), 404);
        assert_eq!(
            post(&mut conn, &tokens[2], Action::Heartbeat, None, b"hb"),
            200
        );
        let ex = db::executions(&conn, id).unwrap();
        assert_eq!(ex.len(), 3);
        assert_eq!(
            ex.iter().map(|e| e.output_bytes).collect::<Vec<_>>(),
            vec![3, 4, 0]
        );
        assert_eq!(ex[1].exit_code, Some(4));
        assert_eq!(ex[1].label(), format!("{id}.2"));
        // resolve: N.M finds the execution, its row id as N does not
        assert_eq!(
            db::resolve(&conn, &format!("{id}.2")).unwrap().unwrap().id,
            ex[1].id
        );
        assert!(db::resolve(&conn, &ex[1].id.to_string()).unwrap().is_none());
        assert!(db::resolve(&conn, &format!("{id}.9")).unwrap().is_none());
        assert!(db::resolve(&conn, "x").is_err());
        // hooks: one per finished execution, none for the parent
        assert_eq!(db::pending_hooks(&conn).unwrap(), vec![ex[0].id, ex[1].id]);
        assert!(!db::claim_hook(&conn, id).unwrap());
        // `go` on a multi-use link takes a use too
        let (id2, _) = mk_multi(&conn, "local", 2, 60);
        let g = db::claim_execution(&mut conn, id2, "go", None, Some(("h", "u")))
            .unwrap()
            .unwrap();
        assert_eq!(g.state, db::ST_RUNNING);
        assert_eq!(g.label(), format!("{id2}.1"));
        // ...but never on a remote one
        let (id3, _) = mk_multi(&conn, "remote", 2, 60);
        assert!(
            db::claim_execution(&mut conn, id3, "go", None, Some(("h", "u")))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unlimited_link_until_ttl() {
        let mut conn = db::open_memory();
        let (id, token) = mk_multi(&conn, "local", 0, 60);
        for _ in 0..20 {
            assert!(matches!(
                do_fetch(&mut conn, &token, false, 7411).unwrap(),
                Fetched::Wrapper(_)
            ));
        }
        assert_eq!(
            db::get_run(&conn, id).unwrap().unwrap().state,
            db::ST_PENDING
        );
        conn.execute("UPDATE runs SET expires_at = 0 WHERE id = ?1", [id])
            .unwrap();
        assert_eq!(
            do_fetch(&mut conn, &token, false, 7411).unwrap(),
            Fetched::NotFound
        );
        db::sweep(&conn).unwrap();
        // used at least once: closed, not expired
        assert_eq!(
            db::get_run(&conn, id).unwrap().unwrap().state,
            db::ST_CLOSED
        );
        // executions already running are unaffected by the TTL
        assert!(
            db::executions(&conn, id)
                .unwrap()
                .iter()
                .all(|e| e.state == db::ST_FETCHED)
        );
        // a multi-use link nobody fetched expires like a single-use one
        let (id2, _) = mk_multi(&conn, "local", 3, 0);
        db::sweep(&conn).unwrap();
        assert_eq!(
            db::get_run(&conn, id2).unwrap().unwrap().state,
            db::ST_EXPIRED
        );
        assert!(db::pending_hooks(&conn).unwrap().contains(&id2));
    }
}
