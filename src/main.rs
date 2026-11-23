//! Nebula: lets an AI coding agent hand a shell script to a human to run
//! somewhere the agent cannot reach, and wakes the agent when it is done.

mod cli;
mod clipboard;
mod daemon;
mod db;
mod go;
mod hooks;
mod motherapp;
mod service;
mod tunnel;
mod util;
mod wrapper;

use clap::{ArgGroup, Parser, Subcommand};

const SKILL_MD: &str = include_str!("../skill/SKILL.md");

const EXIT_CODES: &str = "\
Exit codes of `run` and `wait`:
  0-255  the script's own exit code (see the final `nebula: run N finished exit=...` line)
  120    --timeout elapsed; the run stays valid, re-attach with `nebula wait <id>`
  121    the link expired before it was fetched (register a new run)
  122    daemon not running or no tunnel
  123    the run was cancelled
  124    the run was lost (fetched but never started, or silent longer than --stale-after)
With --uses other than 1: the highest of these Nebula codes among the executions if
any, else the highest script exit code.
  125    other nebula error
  2      bad command line";

#[derive(Parser)]
#[command(
    name = "nebula",
    version,
    about = "Hand a shell script to a human to run on a remote server or in their own terminal, capture the output, and wake the agent when it is done.",
    after_help = EXIT_CODES
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Register a script, print the one-liner for the human, block until the run finishes
    #[command(
        group(ArgGroup::new("target").required(true).args(["remote", "local"])),
        after_help = EXIT_CODES
    )]
    Run {
        /// Run on a remote machine: the human pastes a curl/wget one-liner there
        #[arg(long)]
        remote: bool,
        /// Run on this machine in the human's own terminal (`nebula go <id>`), e.g. for sudo
        #[arg(long)]
        local: bool,
        /// Short name shown in `nebula list`
        #[arg(long, value_name = "N")]
        name: Option<String>,
        /// Free-text note stored with the run
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
        /// Seconds the link stays valid while unfetched
        #[arg(long, value_name = "SECS", default_value_t = 900)]
        ttl: u64,
        /// Seconds this command waits before giving up (the run itself stays valid)
        #[arg(long, value_name = "SECS", default_value_t = 3600)]
        timeout: u64,
        /// Also show the script's output on the far terminal (default: status lines only)
        #[arg(long)]
        echo: bool,
        /// Delete the stored output once this command has printed it
        #[arg(long)]
        delete_after: bool,
        /// Do not copy the one-liner to the clipboard
        #[arg(long)]
        no_clipboard: bool,
        /// Print at most this many output bytes here (the rest: `nebula output <id>`)
        #[arg(long, value_name = "BYTES", default_value_t = 262_144)]
        max_print: u64,
        /// Declare a started run lost after this many seconds without a sign of life
        #[arg(long, value_name = "SECS", default_value_t = 600,
              value_parser = clap::value_parser!(u64).range(30..))]
        stale_after: u64,
        /// How many times the link may be fetched (default 1: single use). Each fetch is a
        /// separate execution (`42.1`, `42.2`, ...). 0 = unlimited until --ttl. At most 1000
        #[arg(long, value_name = "N", default_value_t = 1,
              value_parser = clap::value_parser!(u64).range(0..=1000))]
        uses: u64,
        /// Command run once (sh -c) when the run ends, with NEBULA_RUN, NEBULA_STATE,
        /// NEBULA_EXIT, NEBULA_BYTES, NEBULA_HOST, NEBULA_NAME, NEBULA_TARGET set
        #[arg(long, value_name = "COMMAND")]
        on_done: Option<String>,
        /// When the run ends, type a `[nebula: ...]` line into a tmux pane and press Enter.
        /// Without a value: this pane ($TMUX_PANE). Explicit pane: --tmux=TARGET
        #[arg(
            long,
            value_name = "TARGET",
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = ""
        )]
        tmux: Option<String>,
        /// With --tmux: include the output in the notice when it is one short line (<= 200 bytes)
        #[arg(long)]
        tmux_inline: bool,
        /// Print the id and one-liner and return at once instead of blocking
        /// (use with --tmux or --on-done)
        #[arg(long)]
        detach: bool,
        /// Path of the script, or `-` for stdin
        #[arg(value_name = "SCRIPT")]
        script: String,
    },
    /// Execute a local run in this terminal (the human runs this)
    Go {
        /// Run id (`42`, or `42.2` for an execution of a multi-use run)
        id: String,
    },
    /// Re-attach to a run and block until it finishes
    #[command(after_help = EXIT_CODES)]
    Wait {
        /// Run id (`42`, or `42.2` for an execution of a multi-use run)
        id: String,
        /// Seconds to wait before giving up (the run itself stays valid)
        #[arg(long, value_name = "SECS", default_value_t = 3600)]
        timeout: u64,
        /// Print at most this many output bytes
        #[arg(long, value_name = "BYTES", default_value_t = 262_144)]
        max_print: u64,
    },
    /// Show whether the daemon runs, its port, the tunnel address, open runs
    Status,
    /// List recent runs
    List {
        /// How many runs to show
        #[arg(short = 'n', value_name = "N", default_value_t = 20,
              value_parser = clap::value_parser!(i64).range(1..))]
        n: i64,
    },
    /// Show a run's metadata and script
    Show {
        /// Run id (`42`, or `42.2` for an execution of a multi-use run)
        id: String,
    },
    /// Print a run's complete raw output to stdout
    Output {
        /// Run id (`42`, or `42.2` for an execution of a multi-use run)
        id: String,
    },
    /// Cancel a pending or running run
    Cancel {
        /// Run id (`42`, or `42.2` for an execution of a multi-use run)
        id: String,
    },
    /// Remove a finished run from the history
    Rm {
        /// Run id (`42`, or `42.2` for an execution of a multi-use run)
        id: String,
        /// Delete only the stored output, keep the metadata
        #[arg(long)]
        output_only: bool,
    },
    /// Print the embedded SKILL.md (for Claude Code) to stdout
    #[command(name = "skill", alias = "skill.md")]
    Skill,
    /// Run the daemon (HTTP listener on 127.0.0.1 + cloudflared tunnel); normally a systemd user service
    Daemon,
    /// Manage the systemd user service that runs the daemon
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
    /// Show the runs and their output in motherapp (a locally served control panel)
    Motherapp {
        #[command(subcommand)]
        action: MotherappCmd,
    },
    /// Run the claimed completion hook of a run (internal)
    #[command(name = "hook-run", hide = true)]
    HookRun { id: i64 },
    /// Keep the clipboard for a detached run (internal)
    #[command(name = "clipboard-hold", hide = true)]
    ClipboardHold { id: i64 },
    /// Become cloudflared with a parent-death signal (internal, used by the daemon)
    #[command(name = "tunnel-exec", hide = true)]
    TunnelExec {
        cloudflared: std::path::PathBuf,
        port: u16,
    },
    /// Report what is on the clipboard and through which backend (diagnostic)
    #[command(name = "clipboard-check", hide = true)]
    ClipboardCheck,
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Write ~/.config/systemd/user/nebula.service for this executable and enable it now
    Install {
        /// Only print the unit file and the commands, change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Disable the service and remove the unit file
    Uninstall {
        /// Only print what would be done
        #[arg(long)]
        dry_run: bool,
    },
    /// Show the systemd status of the service
    Status,
}

#[derive(Subcommand)]
enum MotherappCmd {
    /// Write the tool's manifest.json and page.ts where motherapp finds them
    /// (~/.config/motherapp/tools/nebula), pointing at this executable
    Install {
        /// The tool's folder, instead of $XDG_CONFIG_HOME/motherapp/tools/nebula
        #[arg(long, value_name = "DIR")]
        dir: Option<std::path::PathBuf>,
        /// Only print the manifest and the paths, change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove the files `install` wrote
    Uninstall {
        /// The tool's folder, instead of $XDG_CONFIG_HOME/motherapp/tools/nebula
        #[arg(long, value_name = "DIR")]
        dir: Option<std::path::PathBuf>,
        /// Only print what would be done
        #[arg(long)]
        dry_run: bool,
    },
    /// Serve mode: JSON-RPC on stdin/stdout, started by motherapp, not by hand
    Serve {
        /// Directory of nebula.db, instead of $NEBULA_HOME or the default
        #[arg(long, value_name = "DIR")]
        home: Option<std::path::PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::Run {
            remote,
            local: _,
            name,
            note,
            ttl,
            timeout,
            echo,
            delete_after,
            no_clipboard,
            max_print,
            stale_after,
            on_done,
            tmux,
            tmux_inline,
            detach,
            uses,
            script,
        } => cli::cmd_run(cli::RunArgs {
            remote,
            name,
            note,
            ttl,
            timeout,
            echo,
            delete_after,
            no_clipboard,
            max_print,
            stale_after,
            on_done,
            tmux,
            tmux_inline,
            detach,
            uses,
            script,
        }),
        Cmd::Go { id } => go::run(&id),
        Cmd::Wait {
            id,
            timeout,
            max_print,
        } => cli::cmd_wait(&id, timeout, max_print),
        Cmd::Status => cli::cmd_status(),
        Cmd::List { n } => cli::cmd_list(n),
        Cmd::Show { id } => cli::cmd_show(&id),
        Cmd::Output { id } => cli::cmd_output(&id),
        Cmd::Cancel { id } => cli::cmd_cancel(&id),
        Cmd::Rm { id, output_only } => cli::cmd_rm(&id, output_only),
        Cmd::Skill => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(SKILL_MD.as_bytes());
            Ok(0)
        }
        Cmd::Daemon => daemon::run(),
        Cmd::ClipboardHold { id } => cli::cmd_clipboard_hold(id),
        Cmd::HookRun { id } => hooks::run_claimed(id),
        Cmd::TunnelExec { cloudflared, port } => Err(tunnel::exec_cloudflared(&cloudflared, port)),
        Cmd::Service { action } => match action {
            ServiceCmd::Install { dry_run } => service::install(dry_run),
            ServiceCmd::Uninstall { dry_run } => service::uninstall(dry_run),
            ServiceCmd::Status => service::status(),
        },
        Cmd::Motherapp { action } => match action {
            MotherappCmd::Install { dir, dry_run } => motherapp::install(dir, dry_run),
            MotherappCmd::Uninstall { dir, dry_run } => motherapp::uninstall(dir, dry_run),
            MotherappCmd::Serve { home } => motherapp::serve::run(home.as_deref()),
        },
        Cmd::ClipboardCheck => match clipboard::read_back() {
            Ok((text, how)) => {
                println!("backend: {how}");
                println!("content: {text}");
                Ok(0)
            }
            Err(e) => {
                eprintln!("nebula: cannot read the clipboard: {e}");
                Ok(1)
            }
        },
    };
    let code = match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("nebula: error: {e:#}");
            util::exit::ERROR
        }
    };
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_requires_exactly_one_target() {
        assert!(Cli::try_parse_from(["nebula", "run", "x.sh"]).is_err());
        assert!(Cli::try_parse_from(["nebula", "run", "--remote", "--local", "x.sh"]).is_err());
        assert!(Cli::try_parse_from(["nebula", "run", "--remote", "x.sh"]).is_ok());
        assert!(Cli::try_parse_from(["nebula", "run", "--local", "-"]).is_ok());
    }

    #[test]
    fn tmux_flag_forms() {
        let parse = |args: &[&str]| match Cli::try_parse_from(args).ok()?.command {
            Cmd::Run { tmux, script, .. } => Some((tmux, script)),
            _ => None,
        };
        // bare --tmux never swallows the script path
        assert_eq!(
            parse(&["nebula", "run", "--local", "--tmux", "x.sh"]),
            Some((Some(String::new()), "x.sh".into()))
        );
        assert_eq!(
            parse(&["nebula", "run", "--local", "--tmux=%3", "x.sh"]),
            Some((Some("%3".into()), "x.sh".into()))
        );
        assert_eq!(
            parse(&["nebula", "run", "--local", "x.sh"]),
            Some((None, "x.sh".into()))
        );
        assert!(Cli::try_parse_from(["nebula", "list", "-n", "0"]).is_err());
        assert!(Cli::try_parse_from(["nebula", "list", "-n", "3"]).is_ok());
    }

    #[test]
    fn skill_alias_and_frontmatter() {
        assert!(Cli::try_parse_from(["nebula", "skill"]).is_ok());
        assert!(Cli::try_parse_from(["nebula", "skill.md"]).is_ok());
        assert!(SKILL_MD.starts_with("---\nname: nebula\n"));
        assert!(SKILL_MD.contains("\ndescription: "));
    }
}
