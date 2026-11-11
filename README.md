# Nebula

Nebula lets an AI coding agent (Claude Code) get a shell script run somewhere it cannot reach:

- on a **remote server** where the human only has a (web) terminal, or
- on **this machine in the human's own terminal**, typically because the script needs `sudo`.

The agent registers a script. The human gets a short one-liner (printed, and already on the clipboard) and pastes it. Nebula captures the output, and the agent's blocking `nebula run` command exits when the run is done, which wakes the agent. Nobody copy-pastes output.

```
agent                                  human
-----                                  -----
nebula run --remote check.sh   --->    pastes:  (curl … || wget -qO- …) | sh     on the server
   (blocks, streams output)    <---    output + exit code come back through the tunnel
exits with the script's exit code
```

One binary, three roles: `nebula daemon` (systemd user service), the CLI the agent uses, and `nebula go <id>` which the human runs for local runs.

## Security model, in plain words

- **Nebula does not check, sandbox or block what a script does.** Whoever pastes the one-liner runs that script with their own rights. The agent is instructed (see `nebula skill`) to say clearly beforehand whether a script only reads or also changes things. Read that statement; `nebula show <id>` prints the exact script.
- **The human triggers every run by hand.** The daemon never executes anything. It only hands out a script when asked for it with the right link, and stores what comes back.
- **Links are unguessable and single use** by default. Each run has a random 192-bit token in its URL. `GET /r/<token>` works exactly once, only while the run is pending, and only until its TTL (15 minutes by default). After that it is an empty 404 forever. Result uploads are accepted only between that fetch and the "done" message. *Any* single request uses the link up, so paste the one-liner only into the target terminal: a chat app that fetches a link preview would burn it (nothing runs, but you need a new run). `--uses N` (opt-in, see below) lets one link be fetched up to N times; each fetch gets its own execution with its own secret callback token, so one execution can never write into another's output. Unknown, used-up and expired links all get the same empty 404.
- **The listener is on `127.0.0.1` only.** The outside world reaches it solely through the `cloudflared` quick tunnel, which exposes the whole listener. Therefore the listener has **no control or admin API at all**: only the per-run routes exist, everything else is an empty 404. Everything else (registering, listing, cancelling) goes through the local database file.
- **Local runs are never served through the tunnel**, and the callback address inside a script wrapper comes from Nebula's own records, not from the request's `Host` header.
- **The database is plain, unencrypted SQLite** at `~/.local/share/nebula/nebula.db` (file mode 0600, always). Its directory is created 0700; an existing directory is tightened to 0700 only if it holds nothing but Nebula's files, otherwise (e.g. `NEBULA_HOME=~/Desktop`) it is left alone and a warning is printed if other users can access it. It holds every script and every captured output, permanently, until you `nebula rm` them. Anyone who can read your files can read it. Output also ends up in the agent's transcript. For sensitive output use `--delete-after`; better, write scripts that print counts instead of rows.
- **Deleting really removes the bytes from the database files.** `--delete-after`, `nebula rm --output-only` and `nebula rm` overwrite the deleted rows with zeros in `nebula.db` (SQLite `secure_delete`) and then fold and truncate the write-ahead log, so neither the output nor (after `rm`) the script and the token can be found in `nebula.db`, `-wal` or `-shm` afterwards. The truncation happens right after the delete; if another Nebula process is in the middle of a read at that moment it is retried for 3 seconds, and if that still fails it happens with the next deletion or the daemon's checkpoint (every 30 s while the daemon runs). Not covered: what the filesystem or SSD keeps of overwritten blocks, backups and snapshots, the agent's transcript, terminal scrollback.
- **BusyBox `wget` does not verify TLS certificates.** On a remote host that has only BusyBox, the script is downloaded (and results are uploaded) over TLS without checking the certificate; someone who can intercept that host's traffic could substitute the script. With curl or GNU wget certificates are verified.
- **Scripts and output travel through Cloudflare** for remote runs (TLS to Cloudflare, then the tunnel). Do not put secrets in scripts.
- By default the far terminal shows only short `nebula:` status lines, not the script's output, because that screen may be logged. `--echo` shows it there too.
- A download that is cut off executes nothing (the wrapper is one function, called on its last line), and the wrapper refuses to run if the script's sha256 does not match.
- Output is capped at 50 MB per run; request bodies are capped (8 MiB per output request, 4 KiB for the others). The wrapper uploads output in chunks of 256 KiB and halves the chunk after every failed post, down to 16 KiB, so a slow link (tested at 6 KiB/s) still delivers everything.
- **The listener is defensive**, because the internet can reach it: a request for an unknown, finished or not-yet-fetched run is answered 404 before any of its body is read; at most 4 output uploads are buffered at a time (32 MiB); a connection must send its first byte within 3 s and its complete request head within 5 s; a body must arrive within 60 s and, after the first 5 s, at an average of at least 8 KiB/s (a trickling upload is dropped with 408 instead of holding an upload slot); a connection serves one request and lives at most 100 s; at most 128 connections are open at once. These limits stop slow or idle clients from the internet. **Known local-only limit:** a process on this machine that keeps opening idle connections (128 of them, re-opened every few seconds) or slow uploads can keep the listener busy and delay or block real runs. This cannot be done through the tunnel (cloudflared holds its own few connections to the listener); it needs code running locally as some user, which could do worse anyway, so it is accepted and not defended against.
- **Inlined results are untrusted.** With `--tmux-inline` a short output line from the far side is typed into the agent's tmux pane (which may be a shell prompt). Escape sequences are removed and then every character outside a fixed ASCII allow-list (letters, digits, space, `. , : - _ / = + %`) is replaced by `_`, with runs collapsed. The same is done to the run's name. So the typed line contains no `$`, backtick, quote, backslash, `!`, `;`, `&`, `|`, `<>`, `()`, `{}`, `*?`, `~`, `@`, `#`, bracket or any non-ASCII character from the run; the fixed parts are only `[nebula:`, the state, `#<id>`, `exit:<n>`, `result` and the two pairs of quotes around name and result. Typed at an idle zsh (also `zsh -f`), bash or dash prompt such a line executes nothing (at worst "command not found" or "bad pattern"); the e2e test checks this in real panes. The *words* still come from the other machine, and an agent must treat them as data. The inlined text is never stored: the run records `result <N bytes inlined>` instead.

## Install

Needs Rust (built with 1.94), a C compiler (bundled SQLite), and `cloudflared` in `PATH` for remote runs.

```sh
cargo build --release
install -m 755 target/release/nebula ~/.local/bin/nebula    # or anywhere in PATH
nebula service install        # writes ~/.config/systemd/user/nebula.service, enables and starts it
nebula status
```

`nebula service install --dry-run` prints the unit file and the `systemctl` commands without doing anything. `nebula service uninstall` removes it again. Without systemd, run `nebula daemon` in a terminal.

Give the skill to Claude Code:

```sh
mkdir -p ~/.claude/skills/nebula && nebula skill > ~/.claude/skills/nebula/SKILL.md
```

Clipboard: uses `wl-copy`, `xclip` or `xsel` if installed; otherwise the Wayland data-control protocol; otherwise X11 (XWayland on GNOME). No external tool is required. The clipboard content lives in the `nebula run` process, so it is available until that command exits.

## Usage

The agent:

```sh
nebula run --remote --name disk-check /tmp/disk-check.sh    # in the background; blocks until done
nebula run --local  --name restart-nginx /tmp/restart.sh
```

`nebula run` prints the run id and the one-liner, then blocks, printing state changes and the output as it arrives, and ends with

```
nebula: run 42 finished exit=0 bytes=1234 host=web-1
```

The human, for a remote run, pastes on the server (works with curl or with only BusyBox `wget`):

```sh
sh -c 'u=$1; s=$( (command -v curl >/dev/null 2>&1 && curl -fsS "$u") || wget -qO- "$u") || s=; [ -n "$s" ] || { echo "nebula: could not download the script ..." >&2; exit 1; }; printf "%s\n" "$s" | sh' nebula https://….trycloudflare.com/r/<token>
```

and for a local run, in their own terminal on this machine:

```sh
nebula go 42
```

`nebula go` needs neither the daemon nor HTTP; it runs the script in the terminal's foreground (so `sudo` can prompt), shows the output live and records it. Put `sudo` on the lines that need it inside the script; `sudo nebula go` is refused, because root has a different database.

A script that wants a typed confirmation reads it from `/dev/tty`; that works in both modes. Do not read stdin: through the pasted one-liner it is `/dev/null`, with `nebula go` it is the terminal.

Other commands:

```
nebula wait <id> [--timeout SECS]     re-attach to a run and block until it finishes
nebula output <id>                    complete raw output
nebula status                         daemon, port, tunnel address, open runs
nebula list [-n N]                    recent runs
nebula show <id>                      metadata and script
nebula cancel <id>                    cancel a pending or running run
nebula rm <id> [--output-only]        delete a finished run, or only its output
nebula skill                          print the embedded SKILL.md
```

`run` options: `--name`, `--note`, `--ttl SECS` (link lifetime, default 900), `--timeout SECS` (how long the command waits, default 3600; the run stays valid), `--echo`, `--delete-after`, `--no-clipboard`, `--max-print BYTES` (default 262144), `--stale-after SECS` (default 600, minimum 30, see "Lost runs"), `--uses N` (default 1, see below), and the completion options below.

### Being told when a run is over: `--on-done`, `--tmux`, `--detach`

By default the agent keeps `nebula run` blocking in the background and is woken when it exits. The alternative needs no waiting process:

```sh
nebula run --remote --detach --tmux --name disk-check /tmp/disk-check.sh
```

- `--detach` prints the id and the one-liner and returns 0 at once. A small background process (`nebula clipboard-hold`) keeps the clipboard until the link is fetched, the run ends or the TTL passes, then exits; it is only started when the clipboard content would otherwise vanish with the command.
- `--tmux` types one line into a tmux pane and presses Enter when the run reaches an end state. Without a value the pane is `$TMUX_PANE` and the server socket comes from `$TMUX`, both read when the run is registered and stored with it (the daemon runs outside tmux). `--tmux=TARGET` names any pane `tmux -t` accepts. The pane is not looked at first: the line is typed even if somebody is typing there. If the pane or server is gone, the notice is skipped and that is recorded.
- The line is one of

  ```
  [nebula: done #42 exit:0]
  [nebula: done #42 "disk-check" exit:3]
  [nebula: done #42 "db-import" exit:0 result "Db inserted 42 lines"]
  [nebula: expired #42]   [nebula: cancelled #42]   [nebula: lost #42]
  ```

  i.e. `[nebula: <state> #<id> ["<name>"] [exit:<code>] [result "<text>"]]`.
- `--tmux-inline` adds `result "<text>"` when the complete output is a single line of at most 200 bytes after trimming; otherwise the plain form is sent and the output is read with `nebula output <id>`. The text is reduced to the allow-list (see the security model), so it is a lossy preview, not the output.
- `--on-done '<command>'` is the generic hook: run once through `sh -c` with `NEBULA_RUN`, `NEBULA_STATE` (`done`, `cancelled`, `expired`, `lost`), `NEBULA_EXIT` (empty if there is none), `NEBULA_BYTES`, `NEBULA_HOST`, `NEBULA_USER`, `NEBULA_NAME`, `NEBULA_NOTE`, `NEBULA_TARGET`, and `NEBULA_EXECUTION` (see `--uses`; `NEBULA_RUN` is then `42.2`). It is killed after 30 seconds. Its exit status is recorded on the run (`nebula show <id>`) and never affects the run's result.
- A hook fires **at most once per run** (per execution with `--uses`); the guard is a conditional update in the database. The daemon notices finished runs twice a second, `nebula go` notices its own run, and when no daemon is running the Nebula command that ends or notices the end of a run (`cancel`, `wait`, `status`, `list`, `show`) does. Whoever notices claims the hook and starts a detached `nebula hook-run <id>` in its own process group, which records its pid on the run, runs the hook (30 s limit) and records the result; `nebula go` and `nebula cancel` return at once. The hook runs with the environment and working directory of whichever process started it, normally the daemon's: rely only on the `NEBULA_*` variables. The systemd unit uses `KillMode=process` so a daemon restart does not kill running hooks. Without a daemon, an expired or lost run's hook fires only when one of those commands is next run.
- **Exactly-once wins over at-least-once.** If the runner dies (killed, machine reboot) or the claimant dies between claiming and starting it, the hook is not retried: 45 s after the claim, with no live runner, it is recorded as `unknown (runner died)` (`nebula show`). A hook command whose runner was killed is not killed with it and may still finish on its own.
- `--tmux`/`--on-done` also work without `--detach`; then both the blocking command and the hook report. Note that with `--delete-after` a blocking `nebula run` may erase the output before the hook reads it, in which case the notice has no inline result.

### One link, several executions: `--uses N`

Single use is the default and should stay the normal case. `--uses N` (1-1000, default 1) lets the link be fetched up to N times, `--uses 0` any number of times until `--ttl`; the TTL keeps its meaning (no new fetches after it), executions already running carry on. Each fetch, and each `nebula go <id>` for a local run, starts a separate **execution** `42.1`, `42.2`, ... with its own state, output, exit code, host, lost detection and hook (`[nebula: done #42.2 exit:0]`). Internally an execution is a row of its own linked to the run, so execution rows also use up numbers: run ids may skip.

- A blocking `nebula run` / `nebula wait 42` prints every execution's state lines prefixed with its id (`nebula: [42.2] started on host web-2 as root`) and each execution's output as one block when it has ended (within one shared `--max-print`), then a summary. It exits when the link takes no more fetches (N reached, TTL passed, or cancelled) and every execution has ended. Exit code: the highest Nebula code among the executions if there is one (124 lost, 123 cancelled), else the highest script exit code; 121 if the link expired with no execution, 123 if it was cancelled.
- `nebula output 42.2`, `nebula wait 42.2`, `nebula show 42.2`, `nebula cancel 42.2`, `nebula rm 42.2` address one execution. `nebula output 42` refuses and lists the executions; `nebula show 42` lists them; `nebula cancel 42` closes the link and cancels the executions in progress; `nebula rm 42` removes the run with all executions.
- With N=1 nothing changes: no executions, the run itself is fetched once.

### Lost runs

A run that was fetched but does not report "started" within 2 minutes, or that is running and has shown no sign of life (an output post, also one still uploading, or a heartbeat, which the wrapper sends after ~10 s without anything else) for `--stale-after` seconds (default 600, minimum 30 = 3x the heartbeat), is moved to the end state `lost`. `nebula run`/`wait` then exit 124 and hooks fire. Whatever output arrived stays readable. A lost run accepts no further posts; if the far side comes back later it keeps its output in a temp file and prints the path. The daemon checks every 2 seconds; without a daemon the waiting command does.

### Exit codes of `run` and `wait`

| code | meaning |
|------|---------|
| 0-255 | the script's exit code (the `finished exit=N` line is authoritative) |
| 120 | `--timeout` elapsed; re-attach with `nebula wait <id>` |
| 121 | the link expired before it was fetched |
| 122 | daemon not running, or no tunnel |
| 123 | the run was cancelled |
| 124 | the run was lost (see "Lost runs") |

With `--uses` other than 1 see the rule in "One link, several executions".
| 125 | other Nebula error |
| 2 | bad command line |

### Environment

| variable | effect |
|----------|--------|
| `NEBULA_HOME` | directory of `nebula.db` (default `$XDG_DATA_HOME/nebula`, i.e. `~/.local/share/nebula`) |
| `NEBULA_PORT` | daemon port (default 7411). Clients read the port from the database. |
| `NEBULA_NO_TUNNEL=1` | daemon does not start `cloudflared` (local runs only) |
| `NEBULA_TUNNEL_URL` | daemon does not start `cloudflared` and announces this fixed public address instead (your own reverse proxy or named tunnel; also used by the tests) |
| `NEBULA_CLOUDFLARED` | path of the `cloudflared` binary |

The daemon is built on `tokio` and `hyper` (not `axum`: its `serve` has no header-read or idle timeouts, which a listener exposed through a tunnel needs).

`nebula service install` copies whichever of these are set into the unit file.

## How it works

The SQLite database is the only channel between the parts: the CLI inserts a run and polls it every 250 ms; the daemon serves and updates the same rows; `nebula go` writes to it directly. Several agents can use it at the same time, each run is independent.

The daemon supervises `cloudflared tunnel --no-autoupdate --url http://127.0.0.1:<port>`, records the `https://….trycloudflare.com` address, and restarts it with backoff when it dies. cloudflared is started through `nebula tunnel-exec`, which sets a parent-death signal before exec, so it is terminated by the kernel even if the daemon is killed with SIGKILL (a leftover from an older daemon is also stopped on the next start). A quick tunnel gets a new address on every start; if that happens while a link is still unfetched, `nebula run` prints the new one-liner.

`GET /r/<token>` returns a POSIX `sh` wrapper that embeds the script in a quoted heredoc, runs it with `sh`, and posts output every ~2 s (with byte offsets, so retries never duplicate), a heartbeat when idle, and finally the exit code. If the result cannot be delivered it keeps the output in a temp file and prints its path. If the wrapper's own shell is killed, its background poster notices within a second, sends the script's shell SIGTERM and, if it is still alive 5 s later, SIGKILL, then reports exit 137 and removes the temp files.

A script is stored with a final newline (one is added if missing), so its bytes and sha256 are the same in `nebula show`, in `nebula go` and on the far side.

## Tests

```sh
cargo test                 # unit tests + tests/e2e.sh on two free ports (temp NEBULA_HOME, no tunnel)
tests/e2e.sh target/release/nebula
```

The end-to-end script uses `curl`, `wget`, `sqlite3`, and, when installed, `busybox`, `script` (for a pty) and `tmux` (a private server on its own socket). It takes about three minutes.

## Limits

- Linux only (systemd user service, `/proc`).
- `nebula cancel` cannot stop a script that is already running on a remote machine; it only invalidates the run. A local `nebula go` run is stopped together with the processes it started (SIGTERM, SIGKILL after 5 s). They are found through `/proc` rather than by process group, because the script has to stay in the terminal's foreground group for `sudo` and `/dev/tty`; processes running as root through `sudo`, and ones that detached themselves, cannot be signalled.
- When the far-side wrapper is killed, the script's own shell is stopped (TERM, then KILL after 5 s), but commands that shell had started may run on.
- Output passes through pipes/files, not a pty, so programs see a non-terminal stdout (no colours, block buffering in some tools).
- The `wget` on the far side must support `--post-file` and `--header` (GNU wget does; BusyBox wget does when built with long options, which is the usual configuration - tested with BusyBox 1.36.1). Very old BusyBox builds without it cannot report back; the wrapper then says so and runs nothing.

## License

[PolyForm Noncommercial 1.0.0](LICENSE.md): free to use, change and share for any noncommercial purpose; selling it or using it commercially needs separate permission from the author.
