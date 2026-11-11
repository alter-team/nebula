//! The commands an agent runs: run, wait, status, list, show, output, cancel, rm.

use crate::clipboard::{self, Hold, Outcome};
use crate::db::{self, DaemonInfo, Run};
use crate::hooks;
use crate::util::{self, exit, fmt_opt_time, fmt_time, now_ms};
use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// Largest script `nebula run` accepts.
const MAX_SCRIPT_BYTES: usize = 4 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(250);
/// A started run with no sign of life for this long gets a warning.
const SILENCE_WARN_MS: i64 = 60_000;
/// How long `run --remote` waits for a tunnel address.
const TUNNEL_WAIT: Duration = Duration::from_secs(30);
/// How long a remote run may sit with the daemon gone before `run`/`wait` give up.
const DAEMON_GRACE: Duration = Duration::from_secs(30);

pub struct RunArgs {
    pub remote: bool,
    pub name: Option<String>,
    pub note: Option<String>,
    pub ttl: u64,
    pub timeout: u64,
    pub echo: bool,
    pub delete_after: bool,
    pub no_clipboard: bool,
    pub max_print: u64,
    pub stale_after: u64,
    pub on_done: Option<String>,
    /// `Some("")` means "this pane" (from $TMUX_PANE).
    pub tmux: Option<String>,
    pub tmux_inline: bool,
    pub detach: bool,
    /// How many times the link may be fetched (1 = single use, 0 = until the TTL).
    pub uses: u64,
    pub script: String,
}

/// Write to stdout; a closed pipe ends the command quietly.
fn emit(bytes: &[u8]) {
    let mut out = std::io::stdout().lock();
    if out.write_all(bytes).and_then(|_| out.flush()).is_err() {
        std::process::exit(141);
    }
}

fn say(line: &str) {
    emit(format!("{line}\n").as_bytes());
}

pub fn remote_one_liner(tunnel_url: &str, token: &str) -> String {
    let url = format!("{tunnel_url}/r/{token}");
    // Download first, run only a non-empty download, and fail loudly otherwise: piping straight
    // into `sh` made a failed download look like a successful run (exit 0, nothing executed).
    // `sh -c '...'` keeps it working when the pasting shell is not POSIX (zsh, fish).
    format!("sh -c '{REMOTE_BODY}' nebula {url}")
}

/// Body of the remote one-liner; `$1` is the link. Single quotes are not allowed inside it.
const REMOTE_BODY: &str = "u=$1; s=$( (command -v curl >/dev/null 2>&1 && curl -fsS \"$u\") \
|| wget -qO- \"$u\") || s=; [ -n \"$s\" ] || { echo \"nebula: could not download the script \
(link used, expired or unreachable)\" >&2; exit 1; }; printf \"%s\\n\" \"$s\" | sh";

pub fn local_curl_one_liner(port: u16, token: &str) -> String {
    format!("curl -fsS http://127.0.0.1:{port}/r/{token} | sh")
}

fn read_script(arg: &str) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    if arg == "-" {
        std::io::stdin()
            .lock()
            .take(MAX_SCRIPT_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .context("cannot read the script from stdin")?;
    } else {
        std::fs::File::open(arg)
            .with_context(|| format!("cannot open script {arg}"))?
            .take(MAX_SCRIPT_BYTES as u64 + 1)
            .read_to_end(&mut body)
            .with_context(|| format!("cannot read script {arg}"))?;
    }
    if body.len() > MAX_SCRIPT_BYTES {
        bail!("script is larger than {MAX_SCRIPT_BYTES} bytes");
    }
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        bail!("script is empty");
    }
    if body.contains(&0) {
        bail!("script contains NUL bytes; it must be a text shell script");
    }
    // A script always ends with a newline. Stored this way, the bytes and the
    // sha256 are the same everywhere: `nebula show`, `nebula go`, and the
    // heredoc in the wrapper (which cannot express a missing final newline).
    if !body.ends_with(b"\n") {
        body.push(b'\n');
    }
    Ok(body)
}

fn describe_tunnel(info: &DaemonInfo) -> String {
    let detail = info
        .tunnel_detail
        .as_deref()
        .map(|d| format!(" ({d})"))
        .unwrap_or_default();
    match (info.tunnel_state.as_str(), &info.tunnel_url) {
        (db::TUNNEL_UP, Some(u)) => format!("up {u}{detail}"),
        (s, _) => format!("{s}{detail}"),
    }
}

fn copy_to_clipboard(text: &str, hold: &mut Option<Hold>) -> String {
    match clipboard::copy(text) {
        Outcome::Copied { how, hold: h, .. } => {
            // replace (and thereby release) any earlier ownership
            *hold = h;
            format!("copied to the clipboard ({how})")
        }
        Outcome::NoDisplay => "not copied to the clipboard: this session has no display".into(),
        Outcome::Failed(e) => {
            eprintln!("nebula: warning: could not copy to the clipboard: {e}");
            "NOT on the clipboard, copy it from here".into()
        }
    }
}

/// `--tmux` / `--tmux=TARGET` to (target, server socket). Both come from the
/// environment of this command, because the daemon that sends the notice
/// later runs outside tmux.
fn resolve_tmux(arg: Option<&str>) -> Result<Option<(String, Option<String>)>> {
    let Some(arg) = arg else { return Ok(None) };
    let target = if arg.is_empty() {
        std::env::var("TMUX_PANE")
            .ok()
            .filter(|p| !p.is_empty())
            .context(
                "--tmux: this command is not running inside tmux ($TMUX_PANE is not set); \
                 name a pane with --tmux=<target>",
            )?
    } else {
        arg.to_string()
    };
    let socket = std::env::var("TMUX")
        .ok()
        .and_then(|t| t.split(',').next().map(str::to_string))
        .filter(|s| !s.is_empty());
    Ok(Some((target, socket)))
}

/// The one-liner for a pending run as of now (None: remote and no tunnel).
fn current_one_liner(run: &Run, info: &DaemonInfo) -> Option<String> {
    if !run.is_remote() {
        return Some(format!("nebula go {}", run.id));
    }
    info.tunnel_url
        .as_deref()
        .filter(|_| info.alive && info.tunnel_state == db::TUNNEL_UP)
        .map(|url| remote_one_liner(url, &run.token))
}

/// With `--detach` the clipboard cannot be owned by this (exiting) process:
/// start `nebula clipboard-hold <id>` in the background and relay its report.
fn spawn_clipboard_holder(id: i64) -> String {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return format!("NOT on the clipboard ({e}), copy it from here"),
    };
    let child = std::process::Command::new(exe)
        .arg("clipboard-hold")
        .arg(id.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0) // not taken down with this command's process group
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return format!("NOT on the clipboard ({e}), copy it from here"),
    };
    let Some(stdout) = child.stdout.take() else {
        return "NOT on the clipboard, copy it from here".into();
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(line) if !line.trim().is_empty() => line.trim().to_string(),
        _ => "clipboard state unknown (the holder did not answer), copy it from here".into(),
    }
}

/// `nebula clipboard-hold <id>` (internal): own the clipboard for a detached
/// run until the link is fetched, the run ends, or the TTL passes.
pub fn cmd_clipboard_hold(id: i64) -> Result<i32> {
    let conn = db::open()?;
    let Some(run) = db::get_run(&conn, id)? else {
        say("NOT on the clipboard (run not found), copy it from here");
        return Ok(1);
    };
    let mut hold: Option<Hold> = None;
    let mut shown = current_one_liner(&run, &db::daemon_info(&conn)?);
    let Some(line) = shown.clone() else {
        say("NOT on the clipboard (no tunnel address), copy it from here");
        return Ok(1);
    };
    let (note, stay) = match clipboard::copy(&line) {
        Outcome::Copied {
            how,
            hold: h,
            in_process,
        } => {
            hold = h;
            (
                format!("copied to the clipboard ({how}, kept until the link is used)"),
                in_process,
            )
        }
        Outcome::NoDisplay => (
            "not copied to the clipboard: this session has no display".into(),
            false,
        ),
        Outcome::Failed(e) => (
            format!("NOT on the clipboard ({e}), copy it from here"),
            false,
        ),
    };
    say(&note);
    if !stay {
        return Ok(0);
    }
    // Bounded by the TTL: a pending run cannot outlive it.
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let Some(run) = db::get_run(&conn, id)? else {
            break;
        };
        if run.state != db::ST_PENDING || now_ms() >= run.expires_at {
            break;
        }
        // a restarted tunnel means a new address: keep the clipboard current
        if run.is_remote() {
            let now = current_one_liner(&run, &db::daemon_info(&conn)?);
            if now.is_some() && now != shown {
                if let Outcome::Copied { hold: h, .. } =
                    clipboard::copy(now.as_deref().unwrap_or_default())
                {
                    hold = h;
                }
                shown = now;
            }
        }
    }
    drop(hold);
    Ok(0)
}

pub fn cmd_run(a: RunArgs) -> Result<i32> {
    let body = read_script(&a.script)?;
    if a.ttl == 0 {
        bail!("--ttl must be at least 1 second");
    }
    if (a.stale_after as i64) < db::MIN_STALE_SECS {
        bail!(
            "--stale-after must be at least {} seconds",
            db::MIN_STALE_SECS
        );
    }
    if a.uses as i64 > db::MAX_USES {
        bail!("--uses must be at most {}", db::MAX_USES);
    }
    let tmux = resolve_tmux(a.tmux.as_deref())?;
    if a.tmux_inline && tmux.is_none() {
        bail!("--tmux-inline needs --tmux");
    }
    if a.on_done.as_deref().is_some_and(|c| c.trim().is_empty()) {
        bail!("--on-done needs a command");
    }
    let conn = db::open()?;
    let mut info = db::daemon_info(&conn)?;

    if a.remote {
        if !info.alive {
            eprintln!(
                "nebula: the daemon is not running, a remote run cannot be served.\n\
                 Start it with `nebula service install` (or `nebula daemon` in a terminal)."
            );
            return Ok(exit::DAEMON);
        }
        let started = Instant::now();
        let mut announced = false;
        while info.tunnel_url.is_none() || info.tunnel_state != db::TUNNEL_UP {
            if matches!(
                info.tunnel_state.as_str(),
                db::TUNNEL_DISABLED | db::TUNNEL_MISSING
            ) {
                eprintln!(
                    "nebula: no tunnel: {}. A remote run needs the cloudflared tunnel.",
                    describe_tunnel(&info)
                );
                return Ok(exit::DAEMON);
            }
            if started.elapsed() >= TUNNEL_WAIT {
                eprintln!(
                    "nebula: no tunnel address after {} s: {}",
                    TUNNEL_WAIT.as_secs(),
                    describe_tunnel(&info)
                );
                return Ok(exit::DAEMON);
            }
            if !announced {
                eprintln!("nebula: waiting for the tunnel address...");
                announced = true;
            }
            std::thread::sleep(POLL);
            info = db::daemon_info(&conn)?;
            if !info.alive {
                eprintln!("nebula: the daemon stopped while waiting for the tunnel");
                return Ok(exit::DAEMON);
            }
        }
    }

    let sha = db::put_script(&conn, &body)?;
    let token = util::new_token()?;
    let id = db::create_run(
        &conn,
        &db::NewRun {
            token: &token,
            target: if a.remote { "remote" } else { "local" },
            name: a.name.as_deref(),
            note: a.note.as_deref(),
            script_sha: &sha,
            echo: a.echo,
            delete_after: a.delete_after,
            ttl_secs: i64::try_from(a.ttl).unwrap_or(i64::MAX / 2000),
            stale_secs: i64::try_from(a.stale_after).unwrap_or(i64::MAX / 2000),
            on_done: a.on_done.as_deref(),
            tmux_target: tmux.as_ref().map(|t| t.0.as_str()),
            tmux_socket: tmux.as_ref().and_then(|t| t.1.as_deref()),
            tmux_inline: a.tmux_inline,
            uses: Some(a.uses as i64),
        },
    )?;

    let mut hold: Option<Hold> = None;
    let one_liner = if a.remote {
        remote_one_liner(info.tunnel_url.as_deref().unwrap_or_default(), &token)
    } else {
        format!("nebula go {id}")
    };
    let clip_note = if a.no_clipboard {
        "not copied to the clipboard (--no-clipboard)".to_string()
    } else if a.detach {
        spawn_clipboard_holder(id)
    } else {
        copy_to_clipboard(&one_liner, &mut hold)
    };
    let label = a
        .name
        .as_deref()
        .map(|n| format!(" \"{n}\""))
        .unwrap_or_default();
    let uses = match a.uses {
        1 => "single use".to_string(),
        0 => "any number of uses until then - each one is a separate execution".to_string(),
        n => format!("up to {n} uses - each one is a separate execution"),
    };
    say(&format!(
        "nebula: run {id}{label} registered ({}, script sha256 {}, link valid {} s, {uses})",
        if a.remote { "remote" } else { "local" },
        &sha[..12],
        a.ttl
    ));
    if a.remote {
        say(&format!(
            "nebula: paste this one-liner on the remote machine - {clip_note}:"
        ));
        say("");
        say(&format!("    {one_liner}"));
        say("");
    } else {
        say(&format!(
            "nebula: run this in your own terminal on this machine - {clip_note}:"
        ));
        say("");
        say(&format!("    {one_liner}"));
        say("");
        match (info.alive, info.port) {
            (true, Some(port)) => say(&format!(
                "nebula: equivalent without the nebula binary: {}",
                local_curl_one_liner(port, &token)
            )),
            _ => say("nebula: (daemon not running: only `nebula go` works for this run)"),
        }
    }
    if let Some((target, _)) = &tmux {
        say(&format!(
            "nebula: when the run ends, a line starting with \"[nebula: \" is typed into \
             tmux pane {target}"
        ));
    }
    if a.detach {
        if tmux.is_none() && a.on_done.is_none() {
            eprintln!(
                "nebula: warning: --detach without --tmux or --on-done: nothing will tell you \
                 when run {id} ends; poll with `nebula wait {id}`"
            );
        }
        say(&format!(
            "nebula: detached, not waiting. Result later: nebula wait {id} (or nebula output {id})"
        ));
        return Ok(0);
    }
    say(&format!("nebula: waiting for run {id}..."));

    let opts = WaitOpts {
        timeout: Duration::from_secs(a.timeout),
        max_print: a.max_print,
        clipboard: !a.no_clipboard,
        known_tunnel: info.tunnel_url.clone(),
    };
    let code = if a.uses == 1 {
        wait_loop(conn, id, opts, &mut hold)?
    } else {
        multi_wait_loop(conn, id, opts, &mut hold)?
    };
    drop(hold);
    Ok(code)
}

pub fn cmd_wait(reference: &str, timeout: u64, max_print: u64) -> Result<i32> {
    let conn = db::open()?;
    let Some(run) = db::resolve(&conn, reference)? else {
        eprintln!("nebula: no run {reference}");
        return Ok(exit::ERROR);
    };
    let info = db::daemon_info(&conn)?;
    say(&format!(
        "nebula: attached to run {} ({}, state {})",
        run.label(),
        run.target,
        run.state
    ));
    let mut hold = None;
    let opts = WaitOpts {
        timeout: Duration::from_secs(timeout),
        max_print,
        clipboard: false,
        known_tunnel: info.tunnel_url,
    };
    if run.is_multi() {
        multi_wait_loop(conn, run.id, opts, &mut hold)
    } else {
        wait_loop(conn, run.id, opts, &mut hold)
    }
}

struct WaitOpts {
    timeout: Duration,
    max_print: u64,
    clipboard: bool,
    /// Tunnel address the currently shown one-liner was built from.
    known_tunnel: Option<String>,
}

/// Block until the run finishes, printing state changes and output.
fn wait_loop(
    mut conn: Connection,
    id: i64,
    opts: WaitOpts,
    hold: &mut Option<Hold>,
) -> Result<i32> {
    let started = Instant::now();
    let lbl = db::get_run(&conn, id)?
        .map(|r| r.label())
        .unwrap_or_else(|| id.to_string());
    let mut said_fetched = false;
    let mut said_started = false;
    let mut next_off: u64 = 0; // next output byte to look at
    let mut printed: u64 = 0;
    let mut truncated_note = false;
    let mut at_line_start = true;
    let mut warned_silent_at: Option<i64> = None;
    let mut known_tunnel = opts.known_tunnel;
    let mut tunnel_down_noted = false;
    let mut warned_tunnel_moved = false;
    let mut daemon_down_since: Option<Instant> = None;
    let mut last_daemon_check: Option<Instant> = None;

    // status lines never get glued to the end of an unterminated output line
    fn status(at_line_start: &mut bool, line: &str) {
        if !*at_line_start {
            emit(b"\n");
            *at_line_start = true;
        }
        say(line);
    }

    loop {
        let Some(mut run) = db::get_run(&conn, id)? else {
            eprintln!("nebula: run {lbl} no longer exists (removed)");
            return Ok(exit::ERROR);
        };
        if db::is_overdue(&run) {
            // The daemon's sweeper normally does this; do it here too so a
            // local run without a daemon also expires or is declared lost.
            db::sweep(&conn)?;
            run = db::get_run(&conn, id)?.context("run disappeared")?;
        }

        // ---- state changes (taken from the timestamps, so a step that came
        // and went between two polls is still reported)
        if !said_fetched && run.fetched_at.is_some() && run.via.as_deref() == Some("http") {
            said_fetched = true;
            status(
                &mut at_line_start,
                &match run.seq {
                    Some(n) => format!("nebula: run {lbl} fetched (use {n} of its run's link)"),
                    None => format!("nebula: run {lbl} fetched (the link is now used up)"),
                },
            );
        }
        if !said_started && run.started_at.is_some() {
            said_started = true;
            status(
                &mut at_line_start,
                &format!(
                    "nebula: run {lbl} started on host {} as {}",
                    run.host.as_deref().filter(|h| !h.is_empty()).unwrap_or("?"),
                    run.user.as_deref().filter(|h| !h.is_empty()).unwrap_or("?")
                ),
            );
        }

        // ---- output
        let total = run.output_bytes as u64;
        let mut unprinted = false;
        while next_off < total && printed < opts.max_print {
            let chunks = db::chunks_from(&conn, id, next_off)?;
            if chunks.is_empty() {
                // output was deleted underneath us (rm --output-only)
                next_off = total;
                break;
            }
            for (off, data) in chunks {
                let room = (opts.max_print - printed) as usize;
                let part = &data[..data.len().min(room)];
                if !part.is_empty() {
                    emit(part);
                    printed += part.len() as u64;
                    at_line_start = part.ends_with(b"\n");
                }
                next_off = off + data.len() as u64;
                if part.len() < data.len() {
                    // the cut fell inside this chunk
                    unprinted = true;
                    break;
                }
            }
        }
        if next_off < total && printed >= opts.max_print {
            unprinted = true;
            next_off = total;
        }
        // Whenever anything was left out, say so (once).
        if unprinted && !truncated_note {
            truncated_note = true;
            let fate = if run.delete_after {
                "the rest is NOT printed and is DELETED together with the stored output \
                 (--delete-after); use a larger --max-print next time"
                    .to_string()
            } else {
                format!("full output: nebula output {lbl}")
            };
            status(
                &mut at_line_start,
                &format!("nebula: output truncated after {printed} bytes (--max-print); {fate}"),
            );
        }

        // ---- end states
        if run.is_finished() {
            // normally the daemon's job; without one, do it here
            hooks::fire_if_no_daemon(&conn);
        }
        match run.state.as_str() {
            db::ST_DONE => {
                let code = run.exit_code.unwrap_or(exit::ERROR as i64);
                let mut line = format!(
                    "nebula: run {lbl} finished exit={code} bytes={} host={}",
                    run.output_bytes,
                    run.host.as_deref().filter(|h| !h.is_empty()).unwrap_or("?")
                );
                if run.output_truncated {
                    line.push_str(" output-capped=50MB");
                }
                if run.output_deleted {
                    line.push_str(" output-deleted=yes");
                }
                if run.delete_after && !run.output_deleted {
                    db::delete_output(&mut conn, id)?;
                    line.push_str(" output-deleted=yes(--delete-after)");
                }
                status(&mut at_line_start, &line);
                return Ok((code as i32).clamp(0, 255));
            }
            db::ST_CANCELLED => {
                status(
                    &mut at_line_start,
                    &format!("nebula: run {lbl} was cancelled"),
                );
                return Ok(exit::CANCELLED);
            }
            db::ST_EXPIRED => {
                status(
                    &mut at_line_start,
                    &format!(
                        "nebula: run {lbl} expired: the link was not fetched within {} s and is dead; \
                         register a new run",
                        run.ttl_secs
                    ),
                );
                return Ok(exit::EXPIRED);
            }
            db::ST_LOST => {
                let why = if run.started_at.is_none() {
                    format!(
                        "it was fetched but did not start within {} s",
                        db::FETCH_START_GRACE_MS / 1000
                    )
                } else {
                    format!(
                        "no sign of life from the far side for over {} s (--stale-after)",
                        run.stale_secs
                    )
                };
                status(
                    &mut at_line_start,
                    &format!(
                        "nebula: run {lbl} is lost: {why}. Its result will not arrive any more; \
                         {} bytes of output were received (nebula output {lbl})",
                        run.output_bytes
                    ),
                );
                return Ok(exit::LOST);
            }
            _ => {}
        }

        // ---- timeout
        if started.elapsed() >= opts.timeout {
            status(
                &mut at_line_start,
                &format!(
                    "nebula: timed out after {} s waiting for run {lbl} (state {}); the run stays \
                     valid - re-attach with: nebula wait {lbl}",
                    opts.timeout.as_secs(),
                    run.state
                ),
            );
            return Ok(exit::TIMEOUT);
        }

        // ---- silence warning
        if run.is_active() {
            let seen = run
                .last_seen_at
                .or(run.fetched_at)
                .unwrap_or(run.created_at);
            if now_ms() - seen > SILENCE_WARN_MS && warned_silent_at != Some(seen) {
                warned_silent_at = Some(seen);
                eprintln!(
                    "nebula: warning: run {lbl} has been silent for over {} s (last sign of life \
                     {}); the far side may have lost its connection or been closed",
                    SILENCE_WARN_MS / 1000,
                    fmt_time(seen)
                );
            }
        }

        // ---- daemon / tunnel watch (remote runs need both until they finish)
        if run.is_remote()
            && last_daemon_check.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            last_daemon_check = Some(Instant::now());
            let info = db::daemon_info(&conn)?;
            if !info.alive {
                let since = *daemon_down_since.get_or_insert_with(|| {
                    eprintln!(
                        "nebula: warning: the daemon is not running; run {lbl} cannot be served \
                         or report back until it is"
                    );
                    Instant::now()
                });
                if since.elapsed() >= DAEMON_GRACE {
                    status(
                        &mut at_line_start,
                        &format!(
                            "nebula: giving up: daemon down for {} s (run {lbl} is {}); once it is \
                             back: nebula wait {lbl}",
                            DAEMON_GRACE.as_secs(),
                            run.state
                        ),
                    );
                    return Ok(exit::DAEMON);
                }
            } else {
                daemon_down_since = None;
                let current = info
                    .tunnel_url
                    .clone()
                    .filter(|_| info.tunnel_state == db::TUNNEL_UP);
                if run.state == db::ST_PENDING {
                    match &current {
                        Some(url) if known_tunnel.as_deref() != Some(url.as_str()) => {
                            let one_liner = remote_one_liner(url, &run.token);
                            let note = if opts.clipboard {
                                copy_to_clipboard(&one_liner, hold)
                            } else {
                                "not copied to the clipboard".to_string()
                            };
                            status(
                                &mut at_line_start,
                                &format!(
                                    "nebula: the tunnel address changed; the OLD one-liner is \
                                     dead. New one-liner - {note}:"
                                ),
                            );
                            say("");
                            say(&format!("    {one_liner}"));
                            say("");
                            known_tunnel = current.clone();
                            tunnel_down_noted = false;
                        }
                        None if !tunnel_down_noted => {
                            tunnel_down_noted = true;
                            known_tunnel = None;
                            status(
                                &mut at_line_start,
                                &format!(
                                    "nebula: the tunnel is {}; the one-liner will not work until a \
                                     new address is printed here",
                                    describe_tunnel(&info)
                                ),
                            );
                        }
                        _ => {}
                    }
                } else if run.is_active()
                    && !warned_tunnel_moved
                    && run.base_url.is_some()
                    && current != run.base_url
                {
                    warned_tunnel_moved = true;
                    eprintln!(
                        "nebula: warning: the tunnel address changed while run {lbl} is in \
                         progress; the far side can no longer report back (it keeps its output \
                         in a temp file and prints the path there)"
                    );
                }
            }
        }

        std::thread::sleep(POLL);
    }
}

/// A run whose link may be fetched several times: report every execution as
/// it goes, and finish when the link takes no more fetches and every
/// execution has ended. Output is printed per execution when it has ended
/// (never interleaved), within one shared `--max-print` budget.
fn multi_wait_loop(
    mut conn: Connection,
    id: i64,
    opts: WaitOpts,
    hold: &mut Option<Hold>,
) -> Result<i32> {
    #[derive(Default)]
    struct Seen {
        fetched: bool,
        started: bool,
        finished: bool,
    }
    let started = Instant::now();
    let mut seen: std::collections::HashMap<i64, Seen> = std::collections::HashMap::new();
    let mut printed: u64 = 0;
    let mut known_tunnel = opts.known_tunnel;
    let mut tunnel_down_noted = false;
    let mut daemon_down_since: Option<Instant> = None;
    let mut last_daemon_check: Option<Instant> = None;
    let host_of = |r: &Run| {
        r.host
            .clone()
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| "?".into())
    };
    loop {
        let Some(mut parent) = db::get_run(&conn, id)? else {
            eprintln!("nebula: run {id} no longer exists (removed)");
            return Ok(exit::ERROR);
        };
        let mut execs = db::executions(&conn, id)?;
        if db::is_overdue(&parent) || execs.iter().any(db::is_overdue) {
            db::sweep(&conn)?;
            parent = db::get_run(&conn, id)?.context("run disappeared")?;
            execs = db::executions(&conn, id)?;
        }

        for e in &execs {
            let lbl = e.label();
            let st = seen.entry(e.id).or_default();
            if !st.fetched && e.via.as_deref() == Some("http") {
                st.fetched = true;
                say(&format!(
                    "nebula: [{lbl}] fetched (use {} of the link)",
                    e.seq.unwrap_or(0)
                ));
            }
            if !st.started && e.started_at.is_some() {
                st.started = true;
                say(&format!(
                    "nebula: [{lbl}] started on host {} as {}",
                    host_of(e),
                    e.user.as_deref().filter(|u| !u.is_empty()).unwrap_or("?")
                ));
            }
            if st.finished || !e.is_finished() {
                continue;
            }
            st.finished = true;
            // the output of this execution, as one block
            let total = e.output_bytes as u64;
            if total > 0 && printed < opts.max_print {
                say(&format!("nebula: --- output of {lbl} ({total} bytes) ---"));
                let mut next = 0u64;
                let mut ends_nl = true;
                'out: while next < total {
                    let chunks = db::chunks_from(&conn, e.id, next)?;
                    if chunks.is_empty() {
                        break;
                    }
                    for (off, data) in chunks {
                        let room = (opts.max_print - printed) as usize;
                        let part = &data[..data.len().min(room)];
                        if !part.is_empty() {
                            emit(part);
                            printed += part.len() as u64;
                            ends_nl = part.ends_with(b"\n");
                        }
                        next = off + data.len() as u64;
                        if part.len() < data.len() {
                            next = off + part.len() as u64;
                            break 'out;
                        }
                    }
                }
                if !ends_nl {
                    emit(b"\n");
                }
                if next < total {
                    say(&format!(
                        "nebula: output truncated (--max-print); {}",
                        if e.delete_after {
                            "the rest is NOT printed and is DELETED (--delete-after)".to_string()
                        } else {
                            format!("full output: nebula output {lbl}")
                        }
                    ));
                }
            } else if total > 0 {
                say(&format!(
                    "nebula: output of {lbl} not printed (--max-print used up); {}",
                    if e.delete_after {
                        "it is DELETED (--delete-after)".to_string()
                    } else {
                        format!("nebula output {lbl}")
                    }
                ));
            }
            let mut line = match e.state.as_str() {
                db::ST_DONE => format!(
                    "nebula: [{lbl}] finished exit={} bytes={total} host={}",
                    e.exit_code.unwrap_or(exit::ERROR as i64),
                    host_of(e)
                ),
                db::ST_LOST => format!(
                    "nebula: [{lbl}] lost (no sign of life from host {}); {total} bytes received",
                    host_of(e)
                ),
                other => format!("nebula: [{lbl}] {other}"),
            };
            if e.delete_after && !e.output_deleted {
                db::delete_output(&mut conn, e.id)?;
                line.push_str(" output-deleted=yes(--delete-after)");
            }
            say(&line);
        }

        // ---- the end: no more fetches, and every execution has ended
        let all_ended = execs.iter().all(|e| e.is_finished());
        if parent.state != db::ST_PENDING && all_ended {
            hooks::fire_if_no_daemon(&conn);
            let mut nebula_code: Option<i32> = None;
            let mut bump = |c: i32| nebula_code = Some(nebula_code.map_or(c, |n| n.max(c)));
            let mut worst_exit: i32 = 0;
            for e in &execs {
                match e.state.as_str() {
                    db::ST_DONE => {
                        worst_exit = worst_exit.max(e.exit_code.unwrap_or(0).clamp(0, 255) as i32)
                    }
                    db::ST_LOST => bump(exit::LOST),
                    db::ST_CANCELLED => bump(exit::CANCELLED),
                    _ => bump(exit::ERROR),
                }
            }
            if execs.is_empty() {
                bump(if parent.state == db::ST_CANCELLED {
                    exit::CANCELLED
                } else {
                    exit::EXPIRED
                });
            } else if parent.state == db::ST_CANCELLED {
                bump(exit::CANCELLED);
            }
            let code = nebula_code.unwrap_or(worst_exit);
            let list: Vec<String> = execs
                .iter()
                .map(|e| match e.state.as_str() {
                    db::ST_DONE => format!(
                        "{} exit={} host={}",
                        e.label(),
                        e.exit_code.unwrap_or(-1),
                        host_of(e)
                    ),
                    other => format!("{} {other}", e.label()),
                })
                .collect();
            let why = match parent.state.as_str() {
                db::ST_CANCELLED => "cancelled".to_string(),
                _ if parent.uses > 0 && parent.fetch_count >= parent.uses => {
                    format!("all {} uses taken", parent.uses)
                }
                _ => "link expired".to_string(),
            };
            say(&format!(
                "nebula: run {id} finished ({why}): {} execution(s){}{}; exit={code}",
                execs.len(),
                if list.is_empty() { "" } else { ": " },
                list.join(", ")
            ));
            return Ok(code);
        }

        // ---- timeout
        if started.elapsed() >= opts.timeout {
            say(&format!(
                "nebula: timed out after {} s waiting for run {id} ({} execution(s) so far, link \
                 {}); the run stays valid - re-attach with: nebula wait {id}",
                opts.timeout.as_secs(),
                execs.len(),
                parent.state
            ));
            return Ok(exit::TIMEOUT);
        }

        // ---- daemon / tunnel watch for remote runs
        if parent.is_remote()
            && last_daemon_check.is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
        {
            last_daemon_check = Some(Instant::now());
            let info = db::daemon_info(&conn)?;
            if !info.alive {
                let since = *daemon_down_since.get_or_insert_with(|| {
                    eprintln!(
                        "nebula: warning: the daemon is not running; run {id} cannot be served \
                         or report back until it is"
                    );
                    Instant::now()
                });
                if since.elapsed() >= DAEMON_GRACE {
                    say(&format!(
                        "nebula: giving up: daemon down for {} s; once it is back: nebula wait {id}",
                        DAEMON_GRACE.as_secs()
                    ));
                    return Ok(exit::DAEMON);
                }
            } else {
                daemon_down_since = None;
                let current = info
                    .tunnel_url
                    .clone()
                    .filter(|_| info.tunnel_state == db::TUNNEL_UP);
                if parent.state == db::ST_PENDING {
                    match &current {
                        Some(url) if known_tunnel.as_deref() != Some(url.as_str()) => {
                            let one_liner = remote_one_liner(url, &parent.token);
                            let note = if opts.clipboard {
                                copy_to_clipboard(&one_liner, hold)
                            } else {
                                "not copied to the clipboard".to_string()
                            };
                            say(&format!(
                                "nebula: the tunnel address changed; the OLD one-liner is dead. \
                                 New one-liner - {note}:"
                            ));
                            say("");
                            say(&format!("    {one_liner}"));
                            say("");
                            known_tunnel = current.clone();
                            tunnel_down_noted = false;
                        }
                        None if !tunnel_down_noted => {
                            tunnel_down_noted = true;
                            known_tunnel = None;
                            say(&format!(
                                "nebula: the tunnel is {}; the one-liner will not work until a \
                                 new address is printed here",
                                describe_tunnel(&info)
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

fn age(ms: i64) -> String {
    let s = (now_ms() - ms).max(0) / 1000;
    match s {
        0..=119 => format!("{s}s"),
        120..=7199 => format!("{}m", s / 60),
        7200..=172_799 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn run_row(r: &Run) -> String {
    format!(
        "{:>5}  {:<6}  {:<9}  {:>4}  {:>9}  {:>4}  {:<16}  {}",
        r.label(),
        r.target,
        r.state,
        r.exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into()),
        r.output_bytes,
        age(r.created_at),
        if r.is_multi() {
            format!(
                "{} exec{}",
                r.fetch_count,
                if r.uses > 0 {
                    format!("/{}", r.uses)
                } else {
                    String::new()
                }
            )
        } else {
            r.host
                .as_deref()
                .filter(|h| !h.is_empty())
                .unwrap_or("-")
                .to_string()
        },
        r.name.as_deref().unwrap_or("")
    )
}

const RUN_HEADER: &str = "   ID  TARGET  STATE      EXIT      BYTES   AGE  HOST              NAME";

pub fn cmd_status() -> Result<i32> {
    let conn = db::open()?;
    db::sweep(&conn)?;
    hooks::fire_if_no_daemon(&conn);
    let info = db::daemon_info(&conn)?;
    if info.alive {
        say(&format!(
            "daemon:  running (pid {}, port {}, since {})",
            info.pid.unwrap_or(0),
            info.port.unwrap_or(0),
            fmt_opt_time(info.started_at)
        ));
        say(&format!("tunnel:  {}", describe_tunnel(&info)));
    } else {
        say("daemon:  not running");
        say("tunnel:  down (no daemon)");
    }
    say(&format!("db:      {}", db::db_path()?.display()));
    let open = db::open_runs(&conn)?;
    if open.is_empty() {
        say("runs:    none pending or running");
    } else {
        say(&format!("runs:    {} pending or running", open.len()));
        say(RUN_HEADER);
        for r in &open {
            say(&run_row(r));
        }
    }
    Ok(if info.alive { 0 } else { 1 })
}

pub fn cmd_list(n: i64) -> Result<i32> {
    let conn = db::open()?;
    db::sweep(&conn)?;
    hooks::fire_if_no_daemon(&conn);
    let runs = db::list_runs(&conn, n)?;
    if runs.is_empty() {
        say("no runs");
        return Ok(0);
    }
    say(RUN_HEADER);
    for r in &runs {
        say(&run_row(r));
    }
    Ok(0)
}

pub fn cmd_show(reference: &str) -> Result<i32> {
    let conn = db::open()?;
    db::sweep(&conn)?;
    hooks::fire_if_no_daemon(&conn);
    let Some(r) = db::resolve(&conn, reference)? else {
        eprintln!("nebula: no run {reference}");
        return Ok(exit::ERROR);
    };
    let opt = |s: &Option<String>| {
        s.clone()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "-".into())
    };
    match r.parent_id {
        Some(p) => say(&format!(
            "run:          {} (execution {} of run {p})",
            r.label(),
            r.seq.unwrap_or(0)
        )),
        None => say(&format!("run:          {}", r.id)),
    }
    if r.is_multi() {
        say(&format!(
            "uses:         {} of {} taken",
            r.fetch_count,
            if r.uses == 0 {
                "unlimited (until the link expires)".to_string()
            } else {
                r.uses.to_string()
            }
        ));
    }
    say(&format!("target:       {}", r.target));
    say(&format!("state:        {}", r.state));
    say(&format!("name:         {}", opt(&r.name)));
    say(&format!("note:         {}", opt(&r.note)));
    say(&format!(
        "exit code:    {}",
        r.exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".into())
    ));
    say(&format!("host:         {}", opt(&r.host)));
    say(&format!("user:         {}", opt(&r.user)));
    say(&format!("executed via: {}", opt(&r.via)));
    say(&format!("created:      {}", fmt_time(r.created_at)));
    say(&format!(
        "link expires: {} (ttl {} s)",
        fmt_time(r.expires_at),
        r.ttl_secs
    ));
    say(&format!("fetched:      {}", fmt_opt_time(r.fetched_at)));
    say(&format!("started:      {}", fmt_opt_time(r.started_at)));
    say(&format!("last seen:    {}", fmt_opt_time(r.last_seen_at)));
    say(&format!("finished:     {}", fmt_opt_time(r.finished_at)));
    let mut out = format!("{} bytes stored", r.output_bytes);
    if let Some(n) = r.reported_bytes {
        out.push_str(&format!(", {n} produced"));
    }
    if r.output_truncated {
        out.push_str(", capped at 50 MB");
    }
    if r.output_deleted {
        out.push_str(", DELETED");
    }
    say(&format!("output:       {out}"));
    say(&format!(
        "flags:        echo={} delete-after={}",
        r.echo, r.delete_after
    ));
    say(&format!(
        "stale after:  {} s without a sign of life",
        r.stale_secs
    ));
    say(&format!("on-done:      {}", opt(&r.on_done)));
    match &r.tmux_target {
        Some(t) => say(&format!(
            "tmux notice:  pane {t}, socket {}, inline result: {}",
            r.tmux_socket.as_deref().unwrap_or("(default)"),
            if r.tmux_inline { "yes" } else { "no" }
        )),
        None => say("tmux notice:  -"),
    }
    if r.has_hook() {
        say(&format!(
            "hook:         {}",
            match (r.hook_claimed_at, r.hook_finished_at) {
                (None, _) => "not fired yet".to_string(),
                (Some(t), None) => format!(
                    "fired {}, still running{}",
                    fmt_time(t),
                    r.hook_pid
                        .map(|p| format!(" (runner pid {p})"))
                        .unwrap_or_default()
                ),
                (Some(_), Some(t)) => format!(
                    "fired {}: {}",
                    fmt_time(t),
                    r.hook_result.as_deref().unwrap_or("")
                ),
            }
        ));
    }
    say(&format!("script:       sha256 {}", r.script_sha));
    if r.is_multi() {
        let execs = db::executions(&conn, r.id)?;
        if execs.is_empty() {
            say("executions:   none yet");
        } else {
            say("executions:");
            say(RUN_HEADER);
            for e in &execs {
                say(&run_row(e));
            }
        }
    }
    if r.state == db::ST_PENDING {
        let line = current_one_liner(&r, &db::daemon_info(&conn)?)
            .unwrap_or_else(|| "(no tunnel address right now)".into());
        say(&format!("one-liner:    {line}"));
    }
    say("--- script ---");
    match db::get_script(&conn, &r.script_sha)? {
        Some(body) => {
            emit(&body);
            if !body.ends_with(b"\n") {
                emit(b"\n");
            }
        }
        None => say("(script body missing)"),
    }
    Ok(0)
}

pub fn cmd_output(reference: &str) -> Result<i32> {
    let conn = db::open()?;
    let Some(r) = db::resolve(&conn, reference)? else {
        eprintln!("nebula: no run {reference}");
        return Ok(exit::ERROR);
    };
    if r.is_multi() {
        let execs = db::executions(&conn, r.id)?;
        if execs.is_empty() {
            eprintln!("nebula: run {reference} can be run several times and has no executions yet");
        } else {
            eprintln!(
                "nebula: run {reference} can be run several times; name one execution, e.g. \
                 `nebula output {}`:",
                execs[0].label()
            );
            for e in &execs {
                eprintln!(
                    "  {:<8} {:<9} exit={:<4} bytes={:<9} host={}",
                    e.label(),
                    e.state,
                    e.exit_code
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "-".into()),
                    e.output_bytes,
                    e.host.as_deref().unwrap_or("-")
                );
            }
        }
        return Ok(1);
    }
    let id = r.id;
    let lbl = r.label();
    if r.output_deleted {
        eprintln!("nebula: the output of run {lbl} was deleted");
        return Ok(1);
    }
    let mut next = 0u64;
    loop {
        let chunks = db::chunks_from(&conn, id, next)?;
        if chunks.is_empty() {
            break;
        }
        for (off, data) in chunks {
            emit(&data);
            next = off + data.len() as u64;
        }
    }
    if r.output_truncated {
        eprintln!("nebula: note: the output of run {lbl} was capped at 50 MB");
    }
    Ok(0)
}

pub fn cmd_cancel(reference: &str) -> Result<i32> {
    let conn = db::open()?;
    let Some(r) = db::resolve(&conn, reference)? else {
        eprintln!("nebula: no run {reference}");
        return Ok(exit::ERROR);
    };
    let id = r.label();
    if db::cancel_run(&conn, r.id)? {
        hooks::fire_if_no_daemon(&conn);
        say(&format!("nebula: run {id} cancelled (was {})", r.state));
        if r.is_active() && r.via.as_deref() != Some("go") {
            say(
                "nebula: note: a script already running on the far side is NOT stopped; \
                 its link and callbacks are dead and its result will be discarded",
            );
        }
        Ok(0)
    } else {
        eprintln!("nebula: run {id} is already {}, nothing to cancel", r.state);
        Ok(1)
    }
}

pub fn cmd_rm(reference: &str, output_only: bool) -> Result<i32> {
    let mut conn = db::open()?;
    let Some(r) = db::resolve(&conn, reference)? else {
        eprintln!("nebula: no run {reference}");
        return Ok(exit::ERROR);
    };
    let id = r.label();
    let busy = db::executions(&conn, r.id)?
        .into_iter()
        .find(|e| !e.is_finished());
    if let Some(e) = busy {
        eprintln!(
            "nebula: execution {} of run {id} is {}; cancel it first (nebula cancel {})",
            e.label(),
            e.state,
            e.label()
        );
        return Ok(1);
    }
    if !r.is_finished() {
        eprintln!(
            "nebula: run {id} is {}; cancel it first (nebula cancel {id})",
            r.state
        );
        return Ok(1);
    }
    if output_only {
        db::delete_output(&mut conn, r.id)?;
        say(&format!(
            "nebula: output of run {id} deleted ({} bytes); metadata kept",
            r.output_bytes
        ));
    } else {
        db::delete_run(&mut conn, r.id)?;
        say(&format!("nebula: run {id} removed"));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_liners() {
        let l = remote_one_liner("https://a-b.trycloudflare.com", "TOK");
        assert!(l.starts_with("sh -c 'u=$1; "), "{l}");
        assert!(
            l.ends_with("' nebula https://a-b.trycloudflare.com/r/TOK"),
            "{l}"
        );
        assert!(!REMOTE_BODY.contains('\''));
        assert_eq!(
            local_curl_one_liner(7411, "TOK"),
            "curl -fsS http://127.0.0.1:7411/r/TOK | sh"
        );
    }
}
