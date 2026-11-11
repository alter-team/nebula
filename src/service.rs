//! `nebula service install | uninstall | status`: the systemd user unit.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const UNIT_NAME: &str = "nebula.service";

pub fn unit_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .context("HOME is not set")?,
        )
        .join(".config"),
    };
    Ok(base.join("systemd/user").join(UNIT_NAME))
}

/// Quote a value for a systemd unit file (`ExecStart=` word or `Environment=` item).
fn unit_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The unit file text. `env` holds NEBULA_* settings to carry into the service.
pub fn unit_text(exe: &Path, env: &[(String, String)]) -> String {
    let mut s = String::new();
    s.push_str("[Unit]\n");
    s.push_str("Description=Nebula - script relay daemon for AI coding agents\n");
    s.push_str("After=network-online.target\n");
    s.push_str("Wants=network-online.target\n");
    s.push_str("StartLimitIntervalSec=0\n\n");
    s.push_str("[Service]\n");
    s.push_str("Type=simple\n");
    s.push_str(&format!(
        "ExecStart={} daemon\n",
        unit_quote(&exe.to_string_lossy())
    ));
    for (k, v) in env {
        s.push_str(&format!(
            "Environment={}\n",
            unit_quote(&format!("{k}={v}"))
        ));
    }
    s.push_str("Restart=always\n");
    // Stopping or restarting the daemon must not kill completion hooks that
    // are still running in their detached `nebula hook-run` processes (each
    // bounded by its 30 s timeout). cloudflared dies with the daemon anyway
    // (parent-death signal).
    s.push_str("KillMode=process\n");
    s.push_str("RestartSec=2\n\n");
    s.push_str("[Install]\n");
    s.push_str("WantedBy=default.target\n");
    s
}

/// NEBULA_* variables set right now that the daemon should also see.
fn carried_env() -> Vec<(String, String)> {
    [
        "NEBULA_HOME",
        "NEBULA_PORT",
        "NEBULA_NO_TUNNEL",
        "NEBULA_TUNNEL_URL",
        "NEBULA_CLOUDFLARED",
    ]
    .iter()
    .filter_map(|k| {
        std::env::var(k)
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| (k.to_string(), v))
    })
    .collect()
}

fn systemctl(args: &[&str], dry_run: bool) -> Result<bool> {
    println!("+ systemctl --user {}", args.join(" "));
    if dry_run {
        return Ok(true);
    }
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("cannot run systemctl")?;
    Ok(status.success())
}

pub fn install(dry_run: bool) -> Result<i32> {
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("cannot determine the path of this executable")?;
    let path = unit_path()?;
    let text = unit_text(&exe, &carried_env());
    if dry_run {
        println!("# would write {}:", path.display());
        print!("{text}");
    } else {
        let dir = path.parent().context("unit path has no parent")?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        std::fs::write(&path, &text).with_context(|| format!("cannot write {}", path.display()))?;
        println!("wrote {}", path.display());
    }
    // If the service is already running, `enable --now` would leave the old
    // process in place; restart it then so it picks up the new unit/binary.
    let was_active = !dry_run
        && Command::new("systemctl")
            .args(["--user", "--quiet", "is-active", UNIT_NAME])
            .status()
            .is_ok_and(|s| s.success());
    if !systemctl(&["daemon-reload"], dry_run)? {
        bail!("systemctl --user daemon-reload failed");
    }
    if !systemctl(&["enable", "--now", UNIT_NAME], dry_run)? {
        bail!("systemctl --user enable --now {UNIT_NAME} failed");
    }
    if was_active && !systemctl(&["restart", UNIT_NAME], dry_run)? {
        bail!("systemctl --user restart {UNIT_NAME} failed");
    }
    if !dry_run {
        println!("nebula service installed; check it with: nebula status");
    }
    Ok(0)
}

pub fn uninstall(dry_run: bool) -> Result<i32> {
    let path = unit_path()?;
    if path.exists() || dry_run {
        let _ = systemctl(&["disable", "--now", UNIT_NAME], dry_run)?;
    }
    if dry_run {
        println!("# would remove {}", path.display());
    } else if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
        println!("removed {}", path.display());
    } else {
        println!("{} does not exist, nothing to remove", path.display());
    }
    let _ = systemctl(&["daemon-reload"], dry_run)?;
    Ok(0)
}

pub fn status() -> Result<i32> {
    let path = unit_path()?;
    if !path.exists() {
        println!("not installed ({} does not exist)", path.display());
        return Ok(3);
    }
    println!("unit file: {}", path.display());
    let status = Command::new("systemctl")
        .args(["--user", "--no-pager", "status", UNIT_NAME])
        .status()
        .context("cannot run systemctl")?;
    Ok(status.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_file() {
        let t = unit_text(
            Path::new("/home/u/bin/nebula"),
            &[("NEBULA_PORT".into(), "7500".into())],
        );
        assert!(t.contains("ExecStart=\"/home/u/bin/nebula\" daemon\n"));
        assert!(t.contains("Environment=\"NEBULA_PORT=7500\"\n"));
        assert!(t.contains("Restart=always\n"));
        assert!(t.contains("KillMode=process\n"));
        assert!(t.contains("WantedBy=default.target\n"));
    }

    #[test]
    fn quoting() {
        let t = unit_text(Path::new("/opt/my tools/100%/ne\"bula"), &[]);
        assert!(t.contains("ExecStart=\"/opt/my tools/100%%/ne\\\"bula\" daemon\n"));
    }
}
