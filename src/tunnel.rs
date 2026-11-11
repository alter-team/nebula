//! Supervision of the `cloudflared` quick tunnel.

use crate::daemon::App;
use crate::db;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

/// Extract `https://<name>.trycloudflare.com` from a cloudflared log line.
pub fn parse_tunnel_url(line: &str) -> Option<String> {
    const SUFFIX: &str = ".trycloudflare.com";
    let mut rest = line;
    while let Some(i) = rest.find("https://") {
        let after = &rest[i + 8..];
        let name_len = after
            .bytes()
            .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
            .count();
        let name = &after[..name_len];
        let tail = &after[name_len..];
        if !name.is_empty()
            && name != "api"
            && tail.starts_with(SUFFIX)
            && !tail[SUFFIX.len()..]
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            return Some(format!("https://{name}{SUFFIX}"));
        }
        rest = &after[name_len..];
    }
    None
}

/// Validate a user supplied `NEBULA_TUNNEL_URL` (scheme://host[:port], nothing else).
pub fn normalize_external_url(raw: &str) -> Option<String> {
    let url = raw.trim().trim_end_matches('/');
    let host = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
    {
        return None;
    }
    Some(url.to_string())
}

pub fn cloudflared_args(port: u16) -> [String; 4] {
    [
        "tunnel".into(),
        "--no-autoupdate".into(),
        "--url".into(),
        format!("http://127.0.0.1:{port}"),
    ]
}

/// `nebula tunnel-exec <cloudflared> <port>` (internal): become cloudflared,
/// with a parent-death signal so it never outlives the daemon.
pub fn exec_cloudflared(bin: &std::path::Path, port: u16) -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let parent = rustix::process::getppid();
    if let Err(e) =
        rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))
    {
        eprintln!("nebula: warning: cannot set the parent-death signal: {e}");
    }
    // the daemon may have died between fork and the call above
    if rustix::process::getppid() != parent {
        return anyhow::anyhow!("the daemon is gone, not starting cloudflared");
    }
    let err = std::process::Command::new(bin)
        .args(cloudflared_args(port))
        .exec();
    anyhow::anyhow!("cannot exec {}: {err}", bin.display())
}

pub fn find_cloudflared() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NEBULA_CLOUDFLARED").filter(|p| !p.is_empty()) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    dirs.extend(["/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from));
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".local/bin"));
    }
    dirs.into_iter()
        .map(|d| d.join("cloudflared"))
        .find(|p| p.is_file())
}

async fn record(app: &Arc<App>, state: &'static str, url: Option<String>, detail: String) {
    let app = app.clone();
    let log_url = url.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let conn = app.db.lock().unwrap_or_else(|e| e.into_inner());
        db::set_kv(&conn, "tunnel_state", state)?;
        db::set_kv(&conn, "tunnel_url", url.as_deref().unwrap_or(""))?;
        db::set_kv(&conn, "tunnel_detail", &detail)?;
        Ok(())
    })
    .await;
    match res {
        Ok(Ok(())) => eprintln!(
            "nebula daemon: tunnel {state} {}",
            log_url.as_deref().unwrap_or("")
        ),
        Ok(Err(e)) => eprintln!("nebula daemon: cannot record tunnel state: {e:#}"),
        Err(e) => eprintln!("nebula daemon: cannot record tunnel state: {e}"),
    }
}

async fn set_kv(app: &Arc<App>, key: &'static str, value: String) {
    let app = app.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let conn = app.db.lock().unwrap_or_else(|e| e.into_inner());
        let _ = db::set_kv(&conn, key, &value);
    })
    .await;
}

/// Sleep unless shutdown is requested first. Returns true on shutdown.
async fn sleep_or_shutdown(d: Duration, shutdown: &mut watch::Receiver<bool>) -> bool {
    if *shutdown.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(d) => false,
        _ = shutdown.changed() => true,
    }
}

async fn terminate(child: &mut tokio::process::Child) {
    if let Some(pid) = child
        .id()
        .and_then(|p| rustix::process::Pid::from_raw(p as i32))
    {
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
    if tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
}

/// Kill a cloudflared left behind by a previous daemon that died without cleanup.
pub fn reap_stale_cloudflared(conn: &rusqlite::Connection) {
    let Ok(Some(pid)) = db::get_kv(conn, "cloudflared_pid") else {
        return;
    };
    let _ = db::set_kv(conn, "cloudflared_pid", "");
    let Ok(pid) = pid.parse::<i32>() else { return };
    let is_ours = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| {
            let s = String::from_utf8_lossy(&c);
            s.contains("cloudflared") && s.contains("--no-autoupdate") && s.contains("127.0.0.1")
        })
        .unwrap_or(false);
    if is_ours && let Some(p) = rustix::process::Pid::from_raw(pid) {
        eprintln!("nebula daemon: stopping stale cloudflared (pid {pid})");
        let pid = p;
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
    }
}

/// Keep a quick tunnel to `127.0.0.1:<port>` running until shutdown.
pub async fn supervise(app: Arc<App>, port: u16, mut shutdown: watch::Receiver<bool>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            return;
        }
        let Some(bin) = find_cloudflared() else {
            record(
                &app,
                db::TUNNEL_MISSING,
                None,
                "cloudflared not found in PATH".into(),
            )
            .await;
            // Look again now and then, in case it gets installed later.
            loop {
                if sleep_or_shutdown(Duration::from_secs(60), &mut shutdown).await {
                    return;
                }
                if find_cloudflared().is_some() {
                    break;
                }
            }
            continue;
        };

        record(&app, db::TUNNEL_STARTING, None, String::new()).await;
        // Started through `nebula tunnel-exec`, which asks the kernel to
        // SIGTERM cloudflared if this daemon dies (even by SIGKILL) and then
        // exec()s it, so the pid stays the same.
        let mut command = match std::env::current_exe() {
            Ok(me) => {
                let mut c = tokio::process::Command::new(me);
                c.arg("tunnel-exec").arg(&bin).arg(port.to_string());
                c
            }
            Err(_) => {
                let mut c = tokio::process::Command::new(&bin);
                c.args(cloudflared_args(port));
                c
            }
        };
        let spawned = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                record(
                    &app,
                    db::TUNNEL_DOWN,
                    None,
                    format!("cannot start {}: {e}", bin.display()),
                )
                .await;
                if sleep_or_shutdown(backoff, &mut shutdown).await {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        set_kv(
            &app,
            "cloudflared_pid",
            child.id().map(|p| p.to_string()).unwrap_or_default(),
        )
        .await;

        let stderr = child.stderr.take().expect("stderr is piped");
        let mut lines = BufReader::new(stderr).lines();
        let mut url: Option<String> = None;
        let mut url_seen_at: Option<Instant> = None;
        let mut up_since: Option<Instant> = None;
        let mut last_err = String::new();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    if url.is_none() && let Some(u) = parse_tunnel_url(&line) {
                        url = Some(u);
                        url_seen_at = Some(Instant::now());
                    }
                    if line.contains(" ERR ") {
                        last_err = line.chars().take(300).collect();
                    }
                    // The address is announced before the tunnel is connected;
                    // only call it "up" once an edge connection is registered.
                    if up_since.is_none() && url.is_some()
                        && line.contains("Registered tunnel connection")
                    {
                        up_since = Some(Instant::now());
                        record(&app, db::TUNNEL_UP, url.clone(), String::new()).await;
                    }
                }
                _ = tick.tick() => {
                    // Fallback if the log wording ever changes.
                    if up_since.is_none()
                        && url_seen_at.is_some_and(|t| t.elapsed() > Duration::from_secs(20))
                    {
                        up_since = Some(Instant::now());
                        record(&app, db::TUNNEL_UP, url.clone(), String::new()).await;
                    }
                }
                _ = shutdown.changed() => {
                    terminate(&mut child).await;
                    set_kv(&app, "cloudflared_pid", String::new()).await;
                    record(&app, db::TUNNEL_DOWN, None, "daemon stopped".into()).await;
                    return;
                }
            }
        }
        // stderr closed: cloudflared is gone (or going).
        terminate(&mut child).await;
        set_kv(&app, "cloudflared_pid", String::new()).await;
        let detail = if last_err.is_empty() {
            "cloudflared exited".to_string()
        } else {
            format!("cloudflared exited: {last_err}")
        };
        record(&app, db::TUNNEL_DOWN, None, detail).await;
        if up_since.is_some_and(|t| t.elapsed() > Duration::from_secs(60)) {
            backoff = Duration::from_secs(1);
        }
        if sleep_or_shutdown(backoff, &mut shutdown).await {
            return;
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quick_tunnel_address() {
        let line = "2026-11-10T09:00:00Z INF |  https://calm-river-fancy-words.trycloudflare.com                                 |";
        assert_eq!(
            parse_tunnel_url(line).as_deref(),
            Some("https://calm-river-fancy-words.trycloudflare.com")
        );
        assert_eq!(
            parse_tunnel_url("url=https://a1-b2.trycloudflare.com/ ok").as_deref(),
            Some("https://a1-b2.trycloudflare.com")
        );
    }

    #[test]
    fn ignores_other_addresses() {
        assert_eq!(
            parse_tunnel_url("Post \"https://api.trycloudflare.com/tunnel\": EOF"),
            None
        );
        assert_eq!(
            parse_tunnel_url("see https://www.cloudflare.com/website-terms/"),
            None
        );
        assert_eq!(
            parse_tunnel_url("https://evil.trycloudflare.com.example.org"),
            None
        );
        assert_eq!(parse_tunnel_url("https://.trycloudflare.com"), None);
        assert_eq!(parse_tunnel_url("no url here"), None);
        // an api line followed by the real one on the same line
        assert_eq!(
            parse_tunnel_url("https://api.trycloudflare.com https://real-one.trycloudflare.com")
                .as_deref(),
            Some("https://real-one.trycloudflare.com")
        );
    }

    #[test]
    fn external_url_validation() {
        assert_eq!(
            normalize_external_url("https://relay.example.org/").as_deref(),
            Some("https://relay.example.org")
        );
        assert_eq!(
            normalize_external_url("http://127.0.0.1:9000").as_deref(),
            Some("http://127.0.0.1:9000")
        );
        assert_eq!(normalize_external_url("ftp://x"), None);
        assert_eq!(normalize_external_url("https://x/path"), None);
        assert_eq!(normalize_external_url("https://x'y"), None);
        assert_eq!(normalize_external_url("https://"), None);
    }
}
