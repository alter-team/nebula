//! `nebula go <id>`: run a local run's script in the human's own terminal,
//! without HTTP and without the daemon. Output is shown live and recorded.

use crate::db::{self, Append};
use crate::hooks;
use crate::util::{self, exit, sha256_hex};
use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

struct TempScript(PathBuf);

impl Drop for TempScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_temp_script(body: &[u8]) -> Result<TempScript> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let name = format!("nebula-go-{}.sh", util::new_token()?);
    let path = dir.join(name);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    let guard = TempScript(path);
    f.write_all(body)?;
    f.flush()?;
    Ok(guard)
}

/// Keep `nebula go` alive on Ctrl-C (the script gets the signal from the
/// terminal itself and we still have to record its exit code). SIGTERM/SIGHUP
/// aimed at us are passed on to the script.
fn install_signal_handling(child_pid: Arc<AtomicU32>, got_stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        rt.block_on(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let (Ok(mut int), Ok(mut term), Ok(mut hup)) = (
                signal(SignalKind::interrupt()),
                signal(SignalKind::terminate()),
                signal(SignalKind::hangup()),
            ) else {
                return;
            };
            loop {
                tokio::select! {
                    _ = int.recv() => {}
                    _ = term.recv() => {
                        got_stop.store(true, Ordering::SeqCst);
                        signal_child(child_pid.load(Ordering::SeqCst));
                    }
                    _ = hup.recv() => {
                        got_stop.store(true, Ordering::SeqCst);
                        signal_child(child_pid.load(Ordering::SeqCst));
                    }
                }
            }
        });
    });
    // give the handler thread a moment to install the handlers
    std::thread::sleep(Duration::from_millis(30));
}

/// Stop the script and everything it started (see `util::signal_tree` for
/// why this is not a process-group kill).
fn signal_child(pid: u32) {
    util::signal_tree(pid, rustix::process::Signal::TERM);
}

pub fn run(reference: &str) -> Result<i32> {
    if util::is_root() && std::env::var_os("NEBULA_HOME").is_none_or(|v| v.is_empty()) {
        eprintln!(
            "nebula: refusing to run as root: root has its own (empty) nebula database.\n\
             Run `nebula go {reference}` as your normal user and put `sudo` on the lines of \
             the script that need it.\n\
             (To override, point NEBULA_HOME at the directory holding nebula.db.)"
        );
        return Ok(exit::ERROR);
    }
    let mut conn = db::open()?;
    db::sweep(&conn)?;
    let Some(run) = db::resolve(&conn, reference)? else {
        eprintln!("nebula: no run {reference}");
        return Ok(exit::ERROR);
    };
    if run.is_remote() {
        eprintln!(
            "nebula: run {reference} is a remote run; paste its one-liner on the remote machine \
             instead"
        );
        return Ok(exit::ERROR);
    }
    if let Some(parent) = run.parent_id {
        eprintln!(
            "nebula: {reference} is an execution that has already started; `nebula go {parent}` \
             starts a new one if the run allows more uses"
        );
        return Ok(exit::ERROR);
    }
    if run.state != db::ST_PENDING {
        let what = if run.is_multi() {
            "its link takes no more uses"
        } else {
            "only a pending run can be started (each run is single use)"
        };
        eprintln!("nebula: run {reference} is {} - {what}", run.state);
        return Ok(exit::ERROR);
    }
    let body = db::get_script(&conn, &run.script_sha)?
        .with_context(|| format!("script of run {reference} is missing from the database"))?;
    if sha256_hex(&body) != run.script_sha {
        eprintln!(
            "nebula: script of run {reference} does not match its recorded sha256, refusing to run"
        );
        return Ok(exit::ERROR);
    }
    let script = write_temp_script(&body)?;

    let (host, user) = (util::hostname(), util::username());
    let (id, lbl) = if run.is_multi() {
        match db::claim_execution(&mut conn, run.id, "go", None, Some((&host, &user)))? {
            Some(exec) => (exec.id, exec.label()),
            None => {
                eprintln!("nebula: run {reference}: its link was just used up or expired");
                return Ok(exit::ERROR);
            }
        }
    } else {
        if !db::claim_go(&conn, run.id, &host, &user)? {
            eprintln!("nebula: run {reference} was just taken or expired");
            return Ok(exit::ERROR);
        }
        (run.id, run.label())
    };
    eprintln!("nebula: run {lbl} started");

    // Handlers go in before the script starts; exec resets them to default
    // in the child, so the script itself still reacts to Ctrl-C normally.
    let got_stop = Arc::new(AtomicBool::new(false));
    let child_pid = Arc::new(AtomicU32::new(0));
    install_signal_handling(child_pid.clone(), got_stop.clone());

    let (mut reader, writer) = std::io::pipe()?;
    let mut cmd = Command::new("sh");
    cmd.arg(&script.0)
        .stdin(Stdio::inherit())
        .stdout(writer.try_clone()?)
        .stderr(writer);
    let spawned = cmd.spawn();
    drop(cmd); // closes our copies of the pipe's write end
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("nebula: cannot start sh: {e}\n");
            let _ = db::append_output(&mut conn, id, 0, msg.as_bytes());
            let _ = db::mark_done(&conn, id, 127, None);
            eprint!("{msg}");
            return Ok(127);
        }
    };

    child_pid.store(child.id(), Ordering::SeqCst);

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let stdout = std::io::stdout();
    let mut pending: Vec<u8> = Vec::new();
    let mut off: u64 = 0;
    let mut total: u64 = 0;
    let mut full = false;
    let mut eof = false;
    let mut status = None;
    let mut last_flush = Instant::now();
    let mut last_touch = Instant::now();
    let mut last_data = Instant::now();
    let mut last_state_check = Instant::now();
    let mut cancelled = false;
    let mut stop_seen: Option<Instant> = None;
    let mut force_killed = false;

    loop {
        if eof {
            std::thread::sleep(Duration::from_millis(50));
        } else {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(buf) => {
                    // the terminal may be gone (hangup); keep recording regardless
                    let mut out = stdout.lock();
                    let _ = out.write_all(&buf);
                    let _ = out.flush();
                    total += buf.len() as u64;
                    last_data = Instant::now();
                    if !full {
                        pending.extend_from_slice(&buf);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => eof = true,
            }
        }
        if status.is_none() {
            status = child.try_wait()?;
        }
        let finished =
            status.is_some() && (eof || last_data.elapsed() > Duration::from_millis(1500));
        if !pending.is_empty()
            && (finished
                || eof
                || pending.len() >= 256 * 1024
                || last_flush.elapsed() >= Duration::from_millis(300))
        {
            match db::append_output(&mut conn, id, off, &pending)? {
                Append::Full | Append::NotActive => full = true,
                _ => {}
            }
            off += pending.len() as u64;
            pending.clear();
            last_flush = Instant::now();
            last_touch = Instant::now();
        }
        if finished {
            break;
        }
        if last_touch.elapsed() >= Duration::from_secs(10) {
            let _ = db::touch(&conn, id);
            last_touch = Instant::now();
        }
        if stop_seen.is_none() && (cancelled || got_stop.load(Ordering::SeqCst)) {
            stop_seen = Some(Instant::now());
        }
        // a script that ignores SIGTERM does not get to run on after a cancel
        if !force_killed
            && status.is_none()
            && stop_seen.is_some_and(|t| t.elapsed() >= Duration::from_secs(5))
        {
            force_killed = true;
            util::signal_tree(child.id(), rustix::process::Signal::KILL);
        }
        if !cancelled && last_state_check.elapsed() >= Duration::from_secs(1) {
            last_state_check = Instant::now();
            if db::get_run(&conn, id)?.is_none_or(|r| r.state == db::ST_CANCELLED) {
                cancelled = true;
                eprintln!("nebula: run {lbl} was cancelled, stopping the script");
                signal_child(child.id());
            }
        }
    }

    let status = status.expect("loop ends only with a status");
    let code = status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1);
    let recorded = db::mark_done(&conn, id, code as i64, Some(total as i64))?;
    drop(script);
    // Local runs must work without a daemon, so the completion hook of this
    // run is fired from here (the database guard keeps it to exactly once).
    // The hook runs detached: this returns at once.
    if recorded {
        hooks::fire(&conn, id);
    } else {
        hooks::fire_if_no_daemon(&conn);
    }
    if !recorded {
        // somebody else ended the run first: say what happened to it
        let state = db::get_run(&conn, id)?
            .map(|r| r.state)
            .unwrap_or_else(|| "removed".into());
        let why = match state.as_str() {
            db::ST_CANCELLED => "it was cancelled",
            db::ST_LOST => {
                "it had been declared lost (no sign of life for longer than --stale-after)"
            }
            "removed" => "it was removed",
            _ => "it had already ended",
        };
        eprintln!(
            "nebula: run {lbl} ended exit={code}, but the result was NOT recorded: {why} (state {state})"
        );
    } else if cancelled {
        eprintln!("nebula: run {lbl} ended exit={code} (cancelled)");
    } else if got_stop.load(Ordering::SeqCst) {
        eprintln!("nebula: run {lbl} stopped exit={code} bytes={total}");
    } else {
        eprintln!("nebula: run {lbl} done exit={code} bytes={total}");
    }
    Ok(code)
}
