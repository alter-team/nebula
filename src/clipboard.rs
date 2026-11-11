//! Copy the one-liner to the clipboard.
//!
//! Order: an external tool if one is installed (`wl-copy`, `xclip`, `xsel`),
//! else the Wayland data-control protocol, else X11 (which is XWayland on a
//! Wayland session without data-control, e.g. GNOME). On Wayland and X11 the
//! clipboard content lives in the process that set it, so the in-process
//! backends return a [`Hold`] that must be kept alive; `nebula run` keeps it
//! until it exits.

use std::io::Write;
use std::process::{Command, Stdio};

/// Keeps in-process clipboard ownership alive.
pub struct Hold {
    _x11: Option<arboard::Clipboard>,
}

pub enum Outcome {
    /// Copied; `how` names the backend.
    Copied {
        how: String,
        hold: Option<Hold>,
        /// True when the content is served by this process and vanishes when it exits.
        in_process: bool,
    },
    /// No display in this session: nothing to copy to.
    NoDisplay,
    Failed(String),
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

fn in_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()))
        .unwrap_or(false)
}

fn pipe_to(tool: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{tool}: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("{tool}: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("{tool}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{tool} exited with {status}"))
    }
}

fn wayland_data_control(text: &str) -> Result<(), String> {
    use wl_clipboard_rs::copy::{MimeType, Options, Source};
    // Not "foreground": the library serves paste requests from a background
    // thread for as long as this process lives.
    Options::new()
        .copy(
            Source::Bytes(text.as_bytes().to_vec().into_boxed_slice()),
            MimeType::Text,
        )
        .map_err(|e| e.to_string())
}

fn x11(text: &str) -> Result<Hold, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_text(text.to_string()).map_err(|e| e.to_string())?;
    Ok(Hold { _x11: Some(cb) })
}

pub fn copy(text: &str) -> Outcome {
    let wayland = env_set("WAYLAND_DISPLAY");
    let x = env_set("DISPLAY");
    if !wayland && !x {
        return Outcome::NoDisplay;
    }
    let mut errors: Vec<String> = Vec::new();

    let mut tools: Vec<(&str, &[&str])> = Vec::new();
    if wayland {
        tools.push(("wl-copy", &[]));
    }
    if x {
        tools.push(("xclip", &["-selection", "clipboard"]));
        tools.push(("xsel", &["--clipboard", "--input"]));
    }
    for (tool, args) in tools {
        if in_path(tool) {
            match pipe_to(tool, args, text) {
                Ok(()) => {
                    return Outcome::Copied {
                        how: tool.to_string(),
                        hold: None,
                        in_process: false,
                    };
                }
                Err(e) => errors.push(e),
            }
        }
    }
    if wayland {
        match wayland_data_control(text) {
            Ok(()) => {
                return Outcome::Copied {
                    how: "wayland data-control".into(),
                    hold: None,
                    in_process: true,
                };
            }
            Err(e) => errors.push(format!("wayland data-control: {e}")),
        }
    }
    if x {
        match x11(text) {
            Ok(hold) => {
                return Outcome::Copied {
                    how: if wayland { "x11 via xwayland" } else { "x11" }.into(),
                    hold: Some(hold),
                    in_process: true,
                };
            }
            Err(e) => errors.push(format!("x11: {e}")),
        }
    }
    Outcome::Failed(errors.join("; "))
}

/// Read the clipboard back (used by the hidden `clipboard-check` command to
/// verify from a second process that a copy really landed).
pub fn read_back() -> Result<(String, &'static str), String> {
    if env_set("WAYLAND_DISPLAY") {
        use std::io::Read;
        use wl_clipboard_rs::paste::{ClipboardType, MimeType, Seat, get_contents};
        match get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text) {
            Ok((mut pipe, _)) => {
                let mut s = String::new();
                pipe.read_to_string(&mut s).map_err(|e| e.to_string())?;
                return Ok((s, "wayland data-control"));
            }
            Err(e) if !env_set("DISPLAY") => return Err(e.to_string()),
            Err(_) => {}
        }
    }
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.get_text().map(|t| (t, "x11")).map_err(|e| e.to_string())
}
