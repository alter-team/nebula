//! `nebula motherapp serve | install | uninstall`: Nebula as a tool of
//! motherapp, a locally served control panel. `serve` is the process its core
//! starts (see `serve.rs`); `install` puts the manifest and the page where
//! the core looks for tools that live outside its own checkout.

mod rpc;
pub mod serve;
mod state;

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// The sources are in `motherapp/` of the repo; `serve` in the manifest is a
/// placeholder that `install` replaces.
const MANIFEST_JSON: &str = include_str!("../../motherapp/manifest.json");
const PAGE_TS: &str = include_str!("../../motherapp/page.ts");
const SERVE_PLACEHOLDER: &str = r#""serve": ["nebula", "motherapp", "serve"]"#;
const TOOL_ID: &str = "nebula";
const FILES: [&str; 2] = ["manifest.json", "page.ts"];

/// A line on stdout. A closed pipe (`nebula motherapp install | head -1`) must
/// not stop the work half way, as a panicking `println!` would.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stdout(), $($arg)*);
    }};
}

/// `${XDG_CONFIG_HOME:-~/.config}/motherapp/tools/nebula`.
pub fn default_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .context("HOME is not set")?,
        )
        .join(".config"),
    };
    Ok(base.join("motherapp/tools").join(TOOL_ID))
}

/// The manifest for this executable. The path is absolute because the
/// motherapp service's PATH may lack `~/.local/bin`. `home` (a NEBULA_HOME
/// set at install time) is passed on, since the service will not have it.
pub fn manifest_text(exe: &Path, home: Option<&Path>) -> Result<String> {
    let mut serve = vec![
        exe.to_string_lossy().into_owned(),
        "motherapp".into(),
        "serve".into(),
    ];
    if let Some(h) = home {
        serve.push("--home".into());
        serve.push(h.to_string_lossy().into_owned());
    }
    let quoted = serve
        .iter()
        .map(serde_json::to_string)
        .collect::<serde_json::Result<Vec<_>>>()?;
    if !MANIFEST_JSON.contains(SERVE_PLACEHOLDER) {
        bail!("the embedded manifest has no serve placeholder");
    }
    Ok(MANIFEST_JSON.replace(
        SERVE_PLACEHOLDER,
        &format!(r#""serve": [{}]"#, quoted.join(", ")),
    ))
}

/// True if `dir/manifest.json` is a manifest of this tool (so the folder is ours).
fn is_ours(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("manifest.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|m| m["id"] == TOOL_ID)
}

pub fn install(dir: Option<PathBuf>, dry_run: bool) -> Result<i32> {
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("cannot determine the path of this executable")?;
    let dir = match dir {
        Some(d) => d,
        None => default_dir()?,
    };
    let home = match std::env::var_os("NEBULA_HOME").filter(|v| !v.is_empty()) {
        Some(h) => Some(
            std::path::absolute(PathBuf::from(h)).context("cannot make NEBULA_HOME absolute")?,
        ),
        None => None,
    };
    let manifest = manifest_text(&exe, home.as_deref())?;
    if dry_run {
        say!("# would write {}:", dir.join("manifest.json").display());
        say!("{}", manifest.trim_end());
        say!(
            "# would write {} ({} bytes, the page; see motherapp/page.ts in the repo)",
            dir.join("page.ts").display(),
            PAGE_TS.len()
        );
        return Ok(0);
    }
    if dir.join("manifest.json").exists() && !is_ours(&dir) {
        bail!(
            "{} holds another tool's manifest.json; not overwriting it",
            dir.display()
        );
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    for (name, text) in [("manifest.json", manifest.as_str()), ("page.ts", PAGE_TS)] {
        let path = dir.join(name);
        // write beside it and rename, so the core never reads half a file
        let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, &path))
            .with_context(|| format!("cannot write {}", path.display()))?;
        say!("wrote {}", path.display());
    }
    if let Some(h) = &home {
        say!(
            "NEBULA_HOME is set: the tool will read {}",
            h.join("nebula.db").display()
        );
    }
    say!(
        "Nebula is installed as a motherapp tool. Restart motherapp to pick it up \
         (systemctl --user restart motherapp), and again after updating this binary."
    );
    Ok(0)
}

pub fn uninstall(dir: Option<PathBuf>, dry_run: bool) -> Result<i32> {
    let dir = match dir {
        Some(d) => d,
        None => default_dir()?,
    };
    if !dir.join("manifest.json").exists() {
        say!(
            "{} does not exist, nothing to remove",
            dir.join("manifest.json").display()
        );
        return Ok(0);
    }
    if !is_ours(&dir) {
        bail!(
            "{} holds another tool's manifest.json; not removing anything",
            dir.display()
        );
    }
    // the manifest goes last, so an interrupted run can be repeated
    for name in FILES.iter().rev() {
        let path = dir.join(name);
        if dry_run {
            say!("# would remove {}", path.display());
        } else if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
            say!("removed {}", path.display());
        }
    }
    if dry_run {
        say!("# would remove {} if it is empty then", dir.display());
        return Ok(0);
    }
    // only an empty folder: anything else in it is not ours
    if std::fs::remove_dir(&dir).is_ok() {
        say!("removed {}", dir.display());
    }
    say!("Restart motherapp so that it drops the tool (systemctl --user restart motherapp).");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn manifest_is_what_motherapp_expects() {
        let text = manifest_text(Path::new("/home/u/my bin/ne\"bula"), None).unwrap();
        let m: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(m["id"], json!("nebula"));
        assert_eq!(m["page"], json!("page.ts"));
        assert_eq!(
            m["serve"],
            json!(["/home/u/my bin/ne\"bula", "motherapp", "serve"])
        );
        // what must not leave this machine
        assert_eq!(m["localActions"], json!(["view", "oneLiner"]));
        let kinds: Vec<&str> = m["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| {
                assert!(k["label"].as_str().is_some_and(|l| !l.is_empty()));
                k["id"].as_str().unwrap()
            })
            .collect();
        assert_eq!(kinds, ["finished", "failed", "lost", "expired"]);
        assert_eq!(serve::TITLE, m["title"].as_str().unwrap());
        assert_eq!(serve::ICON, m["icon"].as_str().unwrap());

        let text = manifest_text(Path::new("/bin/nebula"), Some(Path::new("/data/n"))).unwrap();
        let m: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            m["serve"],
            json!(["/bin/nebula", "motherapp", "serve", "--home", "/data/n"])
        );
    }

    /// motherapp serves the page with the types stripped and nothing else:
    /// no bundler, no module resolution.
    #[test]
    fn page_is_one_self_contained_module() {
        for line in PAGE_TS.lines() {
            let l = line.trim_start();
            if l.starts_with("import ") {
                assert!(l.starts_with("import type "), "{l}");
            }
            for word in ["enum ", "namespace ", "declare ", "require("] {
                assert!(!l.starts_with(word) && !l.contains(&format!(" {word}")), "{l}");
            }
        }
        assert!(PAGE_TS.contains("export default class "));
        // text from a run goes in as text
        for bad in ["innerHTML", "outerHTML", "insertAdjacentHTML", "document.write"] {
            assert!(!PAGE_TS.contains(bad), "{bad}");
        }
    }

    #[test]
    fn install_and_uninstall_touch_only_their_files() {
        let base =
            std::env::temp_dir().join(format!("nebula-ma-{}", crate::util::new_token().unwrap()));
        let dir = base.join("tools/nebula");
        assert_eq!(install(Some(dir.clone()), true).unwrap(), 0);
        assert!(!base.exists(), "--dry-run writes nothing");
        assert_eq!(install(Some(dir.clone()), false).unwrap(), 0);
        assert_eq!(install(Some(dir.clone()), false).unwrap(), 0, "again: an update");
        let m: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();
        let exe = m["serve"][0].as_str().unwrap();
        assert!(Path::new(exe).is_absolute() && Path::new(exe).exists(), "{exe}");
        assert_eq!(std::fs::read_to_string(dir.join("page.ts")).unwrap(), PAGE_TS);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "no temp file left");

        // a file of the user's stays, and so does the folder
        std::fs::write(dir.join("notes.txt"), "mine").unwrap();
        assert_eq!(uninstall(Some(dir.clone()), true).unwrap(), 0);
        assert!(dir.join("page.ts").exists());
        assert_eq!(uninstall(Some(dir.clone()), false).unwrap(), 0);
        assert!(!dir.join("manifest.json").exists() && !dir.join("page.ts").exists());
        assert!(dir.join("notes.txt").exists());
        assert_eq!(uninstall(Some(dir.clone()), false).unwrap(), 0, "nothing left to do");

        // another tool's folder is refused both ways
        std::fs::write(dir.join("manifest.json"), r#"{"id":"other"}"#).unwrap();
        assert!(install(Some(dir.clone()), false).is_err());
        assert!(uninstall(Some(dir.clone()), false).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("manifest.json")).unwrap(),
            r#"{"id":"other"}"#
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
