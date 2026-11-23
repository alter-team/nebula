//! Small helpers: time, randomness, encoding, hashing, identity.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

/// Exit codes of the blocking commands (`run`, `wait`) that are not the
/// script's own exit code. Documented in `--help`, README and SKILL.md.
pub mod exit {
    /// `--timeout` elapsed; the run stays valid, `nebula wait` re-attaches.
    pub const TIMEOUT: i32 = 120;
    /// The link was never fetched before its TTL ran out.
    pub const EXPIRED: i32 = 121;
    /// Daemon not running / tunnel unavailable.
    pub const DAEMON: i32 = 122;
    /// The run was cancelled with `nebula cancel`.
    pub const CANCELLED: i32 = 123;
    /// The run was lost: fetched but never started, or silent for too long.
    pub const LOST: i32 = 124;
    /// Any other Nebula error (bad run id, database problem, ...).
    pub const ERROR: i32 = 125;
}

/// Number of random bytes in a run token (192 bits).
pub const TOKEN_BYTES: usize = 24;
/// Length of the encoded token (base64url, no padding).
pub const TOKEN_LEN: usize = 32;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn random_bytes(n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(A[(n >> 18) as usize & 63] as char);
        out.push(A[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(A[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(A[n as usize & 63] as char);
        }
    }
    out
}

pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// Constant-time byte comparison (length is not secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn new_token() -> std::io::Result<String> {
    Ok(b64url(&random_bytes(TOKEN_BYTES)?))
}

/// Cheap syntactic check done before any database lookup.
pub fn valid_token_format(s: &str) -> bool {
    s.len() == TOKEN_LEN
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `2026-11-10 09:44:01Z` from unix milliseconds (UTC, no external crate).
pub fn fmt_time(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub fn fmt_opt_time(ms: Option<i64>) -> String {
    ms.map(fmt_time).unwrap_or_else(|| "-".into())
}

/// All descendants of `pid` (children, their children, ...) from /proc.
pub fn descendants(pid: u32) -> Vec<u32> {
    let mut parent_of: Vec<(u32, u32)> = Vec::new();
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for entry in dir.flatten() {
            let Some(p) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            // "pid (comm) state ppid ..."; comm may contain spaces and ')'
            if let Ok(stat) = std::fs::read_to_string(entry.path().join("stat"))
                && let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..])
                && let Some(ppid) = rest.split_whitespace().nth(1).and_then(|v| v.parse().ok())
            {
                parent_of.push((p, ppid));
            }
        }
    }
    let mut found = vec![pid];
    let mut i = 0;
    while i < found.len() {
        let cur = found[i];
        for (p, pp) in &parent_of {
            if *pp == cur && !found.contains(p) {
                found.push(*p);
            }
        }
        i += 1;
    }
    found.remove(0);
    found
}

/// Signal a process and everything it started. The processes are not moved
/// to another process group (they must stay in the terminal's foreground
/// group so sudo and /dev/tty prompts work), so they are found through /proc.
pub fn signal_tree(pid: u32, sig: rustix::process::Signal) {
    if pid == 0 {
        return;
    }
    let mut all = descendants(pid);
    all.insert(0, pid);
    for p in all {
        if let Some(p) = rustix::process::Pid::from_raw(p as i32) {
            let _ = rustix::process::kill_process(p, sig);
        }
    }
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}

pub fn is_root() -> bool {
    rustix::process::geteuid().is_root()
}

pub fn username() -> String {
    let uid = rustix::process::geteuid().as_raw();
    if let Ok(passwd) = std::fs::read_to_string("/etc/passwd") {
        for line in passwd.lines() {
            let mut f = line.split(':');
            if let (Some(name), Some(_), Some(id)) = (f.next(), f.next(), f.next())
                && id.parse::<u32>() == Ok(uid)
            {
                return name.to_string();
            }
        }
    }
    std::env::var("USER").unwrap_or_else(|_| format!("uid{uid}"))
}

/// Strip control characters and clamp the length of a far-side supplied label.
pub fn clean_label(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(255)
        .collect::<String>()
        .trim()
        .to_string()
}

/// A short free-text value from the other side (a User-Agent, `uname`
/// output): printable ASCII only, runs of spaces collapsed, at most `max`
/// characters. Everything else is dropped, so it can be printed anywhere.
pub fn clean_ascii(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = if c == '\t' { ' ' } else { c };
        if !(' '..='~').contains(&c) || (c == ' ' && (out.is_empty() || out.ends_with(' '))) {
            continue;
        }
        if out.len() >= max {
            break;
        }
        out.push(c);
    }
    out.trim_end().to_string()
}

/// The terminal on stdin (`/dev/pts/3`), if there is one.
pub fn stdin_tty() -> Option<String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return None;
    }
    let link = std::fs::read_link("/proc/self/fd/0").ok()?;
    let name = clean_ascii(&link.to_string_lossy(), 64);
    name.starts_with("/dev/").then_some(name)
}

/// This machine's `uname -srm`, from /proc (no process is started).
pub fn local_os() -> String {
    let read = |f: &str| {
        std::fs::read_to_string(format!("/proc/sys/kernel/{f}"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    clean_ascii(
        &format!("{} {} {}", read("ostype"), read("osrelease"), std::env::consts::ARCH),
        120,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_shape() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_ne!(a, b);
        assert_eq!(a.len(), TOKEN_LEN);
        assert!(valid_token_format(&a));
        assert!(!valid_token_format(&a[1..]));
        assert!(!valid_token_format(&format!("{}/", &a[1..])));
        assert!(!valid_token_format(""));
    }

    #[test]
    fn b64url_vectors() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn sha_and_time() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(fmt_time(0), "1970-01-01 00:00:00Z");
        assert_eq!(fmt_time(1_794_303_841_000), "2026-11-10 09:44:01Z");
    }

    #[test]
    fn finds_descendants() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & sleep 30 & wait"])
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let kids = descendants(child.id());
        assert_eq!(kids.len(), 2, "{kids:?}");
        signal_tree(child.id(), rustix::process::Signal::TERM);
        let _ = child.wait();
        std::thread::sleep(std::time::Duration::from_millis(200));
        for k in kids {
            let gone = std::fs::read_to_string(format!("/proc/{k}/stat"))
                .map(|s| s.contains(") Z "))
                .unwrap_or(true);
            assert!(gone, "child {k} survived");
        }
    }

    #[test]
    fn ascii_cleaning() {
        assert_eq!(clean_ascii("curl/8.5.0", 120), "curl/8.5.0");
        assert_eq!(clean_ascii("  Wget/1.21.4 \t (linux-gnu)  ", 120), "Wget/1.21.4 (linux-gnu)");
        assert_eq!(clean_ascii("a\x1b[31mb\r\nc\u{202e}d\0é", 120), "a[31mbcd");
        assert_eq!(clean_ascii(&"x".repeat(500), 120).len(), 120);
        assert_eq!(clean_ascii("ab   ", 3), "ab");
        assert_eq!(clean_ascii("", 10), "");
        assert_eq!(clean_ascii("\u{1F600}\n", 10), "");
        let os = local_os();
        assert!(os.starts_with("Linux ") && os.split(' ').count() == 3, "{os}");
    }

    #[test]
    fn labels_are_cleaned() {
        assert_eq!(clean_label(" web-1\n\x1b[31m "), "web-1[31m");
    }
}
