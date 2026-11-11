//! Completion hooks: `--on-done <command>` and the built-in `--tmux` notice.
//!
//! A hook fires once per run when the run reaches an end state (done,
//! cancelled, expired, lost). The exactly-once guard is a conditional UPDATE
//! in the database (`db::claim_hook`), so it does not matter which process
//! notices first: the daemon (every 500 ms), `nebula go` for its own run, or
//! a CLI command when no daemon is running. The claimant does not run the
//! hook itself: it starts a detached `nebula hook-run <id>` in its own process
//! group, which takes a second claim (`db::claim_hook_runner`, recording its
//! pid), runs the hook with its timeout, and records the result. So `go` and
//! `cancel` return at once, and a dying daemon does not take a hook with it.
//! A claim whose runner died is later marked "unknown (runner died)"; a hook
//! is never fired twice (exactly-once wins over at-least-once).

use crate::db::{self, Run};
use rusqlite::Connection;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A hook command that runs longer than this is killed.
pub const HOOK_TIMEOUT: Duration = Duration::from_secs(30);
/// Output longer than this (after trimming) is never inlined into a notice.
pub const INLINE_MAX_BYTES: usize = 200;
/// Stored output larger than this is not even looked at for inlining.
const INLINE_MAX_RAW: i64 = 4096;
const NAME_MAX_CHARS: usize = 40;

fn is_format_char(c: char) -> bool {
    // zero-width and bidirectional controls: invisible, can reorder or hide text
    matches!(c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}' | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}' | '\u{E0000}'..='\u{E007F}')
}

/// Characters that may appear in text taken from a run (the inlined result
/// and the name) when it is typed into a terminal. Nothing in this set means
/// anything to sh, bash, dash or zsh inside double quotes, or to a line
/// editor: no `$`, backtick, quotes, `\\`, `!`, `;`, `&`, `|`, `<>`, `()`,
/// `{}`, `*?`, `~`, `@`, `#`, brackets, and no non-ASCII at all.
pub fn is_allowed(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, ' ' | '.' | ',' | ':' | '-' | '_' | '/' | '=' | '+' | '%')
}

/// Terminal escape sequences removed, control characters turned into spaces,
/// invisible formatting characters dropped. Not yet safe to type anywhere.
fn strip_escapes(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let mut plain = String::with_capacity(text.len());
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match it.next() {
                // CSI: parameters, then one final byte in @..~
                Some('[') => {
                    for n in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC / DCS / SOS / PM / APC: a string up to BEL or ST (ESC \)
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(n) = it.next() {
                        if n == '\u{07}' || n == '\u{9c}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            if it.peek() == Some(&'\\') {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                // two-character escape (or a lone ESC at the end)
                _ => {}
            },
            // 8-bit CSI
            '\u{9b}' => {
                for n in it.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            c if c.is_control() => plain.push(' '),
            c if is_format_char(c) => {}
            c => plain.push(c),
        }
    }
    plain
}

/// Make arbitrary (remote, untrusted) bytes safe to type into a shell prompt
/// inside `"..."`: escape sequences are removed, then every character outside
/// the ASCII allow-list ([`is_allowed`]) becomes `_`, runs of `_` and of
/// whitespace are collapsed to one, and the ends are trimmed.
pub fn sanitize(raw: &[u8]) -> String {
    let plain = strip_escapes(raw);
    let mut out = String::with_capacity(plain.len());
    for c in plain.chars() {
        let c = if c.is_whitespace() {
            ' '
        } else if is_allowed(c) {
            c
        } else {
            '_'
        };
        let last = out.chars().last();
        if (c == ' ' || c == '_') && last == Some(c) {
            continue;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// The output as it may be inlined into a notice, or None when it does not
/// qualify: it must be a single line after trimming and at most
/// [`INLINE_MAX_BYTES`] long.
pub fn inline_result(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let trimmed = text.trim();
    if trimmed.is_empty()
        || trimmed.len() > INLINE_MAX_BYTES
        || trimmed.contains([
            '\n', '\r', '\u{0b}', '\u{0c}', '\u{85}', '\u{2028}', '\u{2029}',
        ])
    {
        return None;
    }
    let clean = sanitize(trimmed.as_bytes());
    (!clean.is_empty()).then_some(clean)
}

/// The one line typed into the tmux pane.
///
/// `[nebula: <state> #<id> ["<name>"] [exit:<n>] [result "<text>"]]`
///
/// The fixed parts are made of letters, digits, `[`, `]`, `:`, `#` and spaces
/// only; the name and the result are passed through [`sanitize`].
pub fn notice(run: &Run, inline: Option<&str>) -> String {
    notice_with(run, inline.map(|t| format!("\"{t}\"")))
}

/// The notice as it is stored on the run: the inlined text is never written
/// to the database (it would outlive `--delete-after` / `rm --output-only`).
pub fn notice_redacted(run: &Run, inline: Option<&str>) -> String {
    notice_with(run, inline.map(|t| format!("<{} bytes inlined>", t.len())))
}

fn notice_with(run: &Run, result: Option<String>) -> String {
    let mut s = format!("[nebula: {} #{}", run.state, run.label());
    if let Some(name) = run.name.as_deref() {
        let clean = sanitize(name.as_bytes());
        let short: String = clean.chars().take(NAME_MAX_CHARS).collect();
        let short = short.trim_end();
        if !short.is_empty() {
            s.push_str(&format!(" \"{short}\""));
        }
    }
    if run.state == db::ST_DONE {
        if let Some(code) = run.exit_code {
            s.push_str(&format!(" exit:{code}"));
        }
        if let Some(r) = result {
            s.push_str(&format!(" result {r}"));
        }
    }
    s.push(']');
    s
}

/// Remove an inlined result from a stored hook result written by an older
/// version (which kept the text): ` result "..."]` becomes ` result <redacted>]`.
pub fn redact_stored(result: &str) -> Option<String> {
    let start = result.find(" result \"")?;
    let end = result.rfind("\"]").filter(|e| *e > start)?;
    Some(format!(
        "{} result <redacted>{}",
        &result[..start],
        &result[end + 1..]
    ))
}

/// Run a command with a time limit. Returns (succeeded, description).
fn run_timed(mut cmd: Command, capture_stderr: bool) -> (bool, String) {
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    cmd.stderr(if capture_stderr {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    // own process group: a timeout kills the command and what it started
    cmd.process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (false, format!("could not start: {e}")),
    };
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if started.elapsed() >= HOOK_TIMEOUT => {
                if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                return (
                    false,
                    format!("killed after {} s timeout", HOOK_TIMEOUT.as_secs()),
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return (false, format!("wait failed: {e}")),
        }
    };
    let mut err = String::new();
    if let Some(stderr) = child.stderr.take() {
        let mut buf = Vec::new();
        let _ = stderr.take(300).read_to_end(&mut buf);
        err = sanitize(&buf);
    }
    let what = match status.code() {
        Some(c) => format!("exit {c}"),
        None => "killed by a signal".to_string(),
    };
    if err.is_empty() {
        (status.success(), what)
    } else {
        (status.success(), format!("{what}: {err}"))
    }
}

fn run_on_done(run: &Run, command: &str) -> String {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        // `42`, or `42.2` for an execution of a multi-use run
        .env("NEBULA_RUN", run.label())
        .env(
            "NEBULA_EXECUTION",
            run.seq.map(|n| n.to_string()).unwrap_or_default(),
        )
        .env("NEBULA_STATE", &run.state)
        .env(
            "NEBULA_EXIT",
            run.exit_code.map(|c| c.to_string()).unwrap_or_default(),
        )
        .env("NEBULA_BYTES", run.output_bytes.to_string())
        .env("NEBULA_HOST", run.host.as_deref().unwrap_or(""))
        .env("NEBULA_USER", run.user.as_deref().unwrap_or(""))
        .env("NEBULA_NAME", run.name.as_deref().unwrap_or(""))
        .env("NEBULA_NOTE", run.note.as_deref().unwrap_or(""))
        .env("NEBULA_TARGET", &run.target);
    format!("on-done: {}", run_timed(cmd, false).1)
}

fn tmux_cmd(socket: Option<&str>) -> Command {
    let mut cmd = Command::new("tmux");
    if let Some(s) = socket.filter(|s| !s.is_empty()) {
        cmd.arg("-S").arg(s);
    }
    cmd
}

fn run_tmux(run: &Run, target: &str, inline: Option<&str>) -> String {
    let text = notice(run, inline);
    let socket = run.tmux_socket.as_deref();
    // No look at the pane first, by design: the line is typed wherever the
    // pane is, even if someone is typing there.
    let mut type_it = tmux_cmd(socket);
    type_it.args(["send-keys", "-t", target, "-l", "--", &text]);
    let (ok, what) = run_timed(type_it, true);
    if !ok {
        // pane closed, server gone, tmux not installed: nothing to notify
        return format!("tmux: skipped ({what})");
    }
    let mut enter = tmux_cmd(socket);
    enter.args(["send-keys", "-t", target, "Enter"]);
    let (ok, what) = run_timed(enter, true);
    if ok {
        format!("tmux: sent {}", notice_redacted(run, inline))
    } else {
        format!("tmux: typed but Enter failed ({what})")
    }
}

fn read_inline(conn: &Connection, run: &Run) -> Option<String> {
    if !run.tmux_inline
        || run.state != db::ST_DONE
        || run.output_deleted
        || run.output_truncated
        || run.output_bytes == 0
        || run.output_bytes > INLINE_MAX_RAW
    {
        return None;
    }
    let mut raw = Vec::new();
    let mut next = 0u64;
    loop {
        let chunks = db::chunks_from(conn, run.id, next).ok()?;
        if chunks.is_empty() {
            break;
        }
        for (off, data) in chunks {
            next = off + data.len() as u64;
            raw.extend_from_slice(&data);
        }
    }
    if raw.len() as i64 != run.output_bytes {
        return None;
    }
    inline_result(&raw)
}

/// Claim the hook of one finished run and start a detached runner for it.
/// Returns true if this call claimed it. Never waits for the hook.
pub fn fire(conn: &Connection, id: i64) -> bool {
    match db::claim_hook(conn, id) {
        Ok(true) => {}
        Ok(false) => return false,
        Err(e) => {
            eprintln!("nebula: hook of run {id}: {e:#}");
            return false;
        }
    }
    let spawned = std::env::current_exe().and_then(|exe| {
        Command::new(exe)
            .arg("hook-run")
            .arg(id.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // not in the terminal's foreground group: Ctrl-C, a closed
            // terminal or the end of the firing process do not reach it
            .process_group(0)
            .spawn()
    });
    match spawned {
        Ok(mut child) => {
            // reap it when it ends (matters for the long-running daemon)
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => {
            let _ = db::finish_hook(conn, id, &format!("could not start the hook runner: {e}"));
        }
    }
    true
}

/// `nebula hook-run <id>` (internal): run the claimed hook of one run.
pub fn run_claimed(id: i64) -> anyhow::Result<i32> {
    let conn = db::open()?;
    if !db::claim_hook_runner(&conn, id, std::process::id())? {
        // not claimed, already finished, or another runner has it
        return Ok(1);
    }
    let Some(run) = db::get_run(&conn, id)? else {
        return Ok(1);
    };
    let inline = read_inline(&conn, &run);
    let mut results = Vec::new();
    if let Some(cmd) = run.on_done.as_deref() {
        results.push(run_on_done(&run, cmd));
    }
    if let Some(target) = run.tmux_target.as_deref() {
        results.push(run_tmux(&run, target, inline.as_deref()));
    }
    db::finish_hook(&conn, run.id, &results.join("; "))?;
    Ok(0)
}

/// Start the hooks of all finished runs that still have one outstanding.
pub fn fire_pending(conn: &Connection) -> usize {
    db::pending_hooks(conn)
        .unwrap_or_default()
        .into_iter()
        .filter(|id| fire(conn, *id))
        .count()
}

/// For CLI commands that may have just ended a run (cancel, expiry or loss
/// noticed while waiting): if no daemon is there to do it, fire the hooks
/// from here (detached; this does not wait for them).
pub fn fire_if_no_daemon(conn: &Connection) {
    if db::pending_hooks(conn).is_ok_and(|p| p.is_empty()) {
        return;
    }
    if db::daemon_info(conn).is_ok_and(|i| i.alive) {
        return;
    }
    fire_pending(conn);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(state: &str, id: i64, name: Option<&str>, exit: Option<i64>) -> Run {
        let conn = db::open_memory();
        let sha = db::put_script(&conn, b"true\n").unwrap();
        let rid = db::create_run(
            &conn,
            &db::NewRun {
                token: "t",
                target: "remote",
                name,
                script_sha: &sha,
                ttl_secs: 60,
                ..Default::default()
            },
        )
        .unwrap();
        let mut r = db::get_run(&conn, rid).unwrap().unwrap();
        r.id = id;
        r.state = state.into();
        r.exit_code = exit;
        r
    }

    #[test]
    fn notice_formats() {
        assert_eq!(
            notice(&run("done", 42, None, Some(0)), None),
            "[nebula: done #42 exit:0]"
        );
        assert_eq!(
            notice(&run("done", 42, None, Some(3)), None),
            "[nebula: done #42 exit:3]"
        );
        assert_eq!(
            notice(&run("expired", 42, None, None), None),
            "[nebula: expired #42]"
        );
        assert_eq!(
            notice(&run("cancelled", 7, None, None), None),
            "[nebula: cancelled #7]"
        );
        assert_eq!(
            notice(&run("lost", 7, None, None), None),
            "[nebula: lost #7]"
        );
        let mut exec = run("done", 50, None, Some(0));
        exec.parent_id = Some(42);
        exec.seq = Some(2);
        assert_eq!(notice(&exec, None), "[nebula: done #42.2 exit:0]");
        assert_eq!(
            notice(
                &run("done", 42, Some("db-import"), Some(0)),
                Some("Db inserted 42 lines")
            ),
            "[nebula: done #42 \"db-import\" exit:0 result \"Db inserted 42 lines\"]"
        );
        // inline text is only ever attached to a finished run
        assert_eq!(
            notice(&run("lost", 42, None, None), Some("x")),
            "[nebula: lost #42]"
        );
    }

    #[test]
    fn notice_fixed_parts_are_shell_inert() {
        for state in ["done", "expired", "cancelled", "lost"] {
            let n = notice(&run(state, 123456, None, Some(255)), None);
            assert!(
                n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || " []:#.".contains(c)),
                "{n}"
            );
        }
    }

    #[test]
    fn hostile_name_cannot_break_the_notice() {
        let n = notice(
            &run(
                "done",
                1,
                Some("a\"] rm -rf /\n[nebula: done #2 exit:0"),
                Some(1),
            ),
            None,
        );
        assert_eq!(
            n,
            "[nebula: done #1 \"a_ rm -rf / _nebula: done _2 exit:0\" exit:1]"
        );
        // a name cut at 40 characters cannot leave an open `$(` or quote:
        // those characters do not survive sanitising in the first place
        let n = notice(
            &run(
                "done",
                1,
                Some("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx$(touch N1) `touch N2`"),
                Some(0),
            ),
            None,
        );
        assert_eq!(
            n,
            "[nebula: done #1 \"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx_to\" exit:0]"
        );
        let long = "x".repeat(500);
        let n = notice(&run("done", 1, Some(&long), Some(0)), None);
        assert_eq!(n.len(), "[nebula: done #1 \"\" exit:0]".len() + 40);
        // a name made only of forbidden characters becomes `_`
        assert_eq!(
            notice(&run("lost", 3, Some("$$$"), None), None),
            "[nebula: lost #3 \"_\"]"
        );
        assert_eq!(
            notice(&run("lost", 3, Some("  "), None), None),
            "[nebula: lost #3]"
        );
    }

    #[test]
    fn stored_notice_never_contains_the_result() {
        let r = run("done", 9, Some("imp"), Some(0));
        assert_eq!(
            notice_redacted(&r, Some("SECRET 42")),
            "[nebula: done #9 \"imp\" exit:0 result <9 bytes inlined>]"
        );
        assert_eq!(
            redact_stored("tmux: sent [nebula: done #9 exit:0 result \"SECRET \\\" x\"]")
                .as_deref(),
            Some("tmux: sent [nebula: done #9 exit:0 result <redacted>]")
        );
        assert_eq!(
            redact_stored("on-done: exit 0; tmux: sent [nebula: done #9 exit:0 result \"S\"]")
                .as_deref(),
            Some("on-done: exit 0; tmux: sent [nebula: done #9 exit:0 result <redacted>]")
        );
        assert_eq!(redact_stored("tmux: sent [nebula: done #9 exit:0]"), None);
    }

    #[test]
    fn allow_list() {
        let allowed = "abcXYZ019 .,:-_/=+%";
        assert!(allowed.chars().all(is_allowed));
        for c in
            "$`\"'\\!;&|<>(){}[]*?~@#^\t\n\x00\x7f\u{ff02}\u{ff3d}\u{ff04}\u{e9}\u{fffd}\u{2028}"
                .chars()
        {
            assert!(!is_allowed(c), "{c:?} must not be allowed");
        }
        // every output byte of sanitize is in the allow-list, for all bytes
        let all: Vec<u8> = (0..=255u8).collect();
        assert!(sanitize(&all).chars().all(is_allowed));
    }

    #[test]
    fn sanitize_removes_shell_syntax() {
        assert_eq!(sanitize(b"$(touch MARK)"), "_touch MARK_");
        assert_eq!(sanitize(b"`touch MARK`"), "_touch MARK_");
        assert_eq!(sanitize(b"\"; touch M; echo \""), "_ touch M_ echo _");
        assert_eq!(
            sanitize(b"a && b || c | d > e < f & g"),
            "a _ b _ c _ d _ e _ f _ g"
        );
        assert_eq!(
            sanitize(b"${HOME} ~root !! *.? @src/main.rs #x"),
            "_HOME_ _root _ _._ _src/main.rs _x"
        );
        // full-width look-alikes of " ] $ and other non-ASCII
        assert_eq!(
            sanitize("\u{ff02}\u{ff3d}\u{ff04}(x) caf\u{e9}".as_bytes()),
            "_x_ caf_"
        );
        // runs collapse
        assert_eq!(sanitize(b"a$$$$b    c__d"), "a_b c_d");
        // what is allowed passes unchanged
        assert_eq!(
            sanitize(b"Db inserted 42 lines: 3.5% ok, a/b=c+d"),
            "Db inserted 42 lines: 3.5% ok, a/b=c+d"
        );
    }

    #[test]
    fn sanitize_strips_escape_sequences_and_controls() {
        // colours, cursor movement, OSC title / hyperlink, DCS, lone ESC
        assert_eq!(sanitize(b"\x1b[31mred\x1b[0m"), "red");
        assert_eq!(sanitize(b"a\x1b[2J\x1b[1;1Hb"), "ab");
        assert_eq!(sanitize(b"\x1b]0;evil title\x07ok"), "ok");
        assert_eq!(
            sanitize(b"\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
        assert_eq!(sanitize(b"\x1bPq#0;2;0;0;0\x1b\\after"), "after");
        assert_eq!(sanitize(b"x\x1b"), "x");
        assert_eq!(sanitize(b"\x1bcreset"), "reset");
        // 8-bit CSI (U+009B) and other C1 controls
        assert_eq!(sanitize("a\u{9b}31mb\u{85}c".as_bytes()), "ab c");
        // control keys that would act in a terminal or a line editor
        assert_eq!(
            sanitize(b"one\r\ntwo\tthree\x03\x04\x15\x17\x7f\x00four\x08"),
            "one two three four"
        );
        // invisible / bidi characters are dropped
        assert_eq!(sanitize("a\u{202e}b\u{200b}c\u{feff}".as_bytes()), "abc");
    }

    #[test]
    fn sanitize_handles_non_utf8_and_whitespace() {
        assert_eq!(sanitize(b"caf\xe9 \xff\xfe ok"), "caf_ _ ok");
        assert_eq!(sanitize(b"   a   b  \n\n c "), "a b c");
        assert_eq!(sanitize(b""), "");
        assert_eq!(sanitize(b"\x1b[0m\n"), "");
    }

    #[test]
    fn inline_only_when_short_single_line() {
        assert_eq!(inline_result(b"42\n").as_deref(), Some("42"));
        assert_eq!(
            inline_result(b"\n  Db inserted 42 lines \n\n").as_deref(),
            Some("Db inserted 42 lines")
        );
        assert_eq!(inline_result(b""), None);
        assert_eq!(inline_result(b"\n\n"), None);
        assert_eq!(inline_result(b"one\ntwo\n"), None);
        assert_eq!(inline_result(b"one\rtwo"), None);
        assert_eq!(inline_result("one\u{2028}two".as_bytes()), None);
        assert_eq!(inline_result(&[b'x'; 200]).map(|s| s.len()), Some(200));
        assert_eq!(inline_result(&[b'x'; 201]), None);
        assert_eq!(inline_result(&vec![b'x'; 100_000]), None);
        // sanitised, not rejected
        assert_eq!(
            inline_result(b"\x1b[32mok\x1b[0m \"quoted\"]").as_deref(),
            Some("ok _quoted_")
        );
        assert_eq!(inline_result(b"\x1b[32m\x1b[0m"), None);
        assert_eq!(
            inline_result(b"bad \xff byte").as_deref(),
            Some("bad _ byte")
        );
    }
}
