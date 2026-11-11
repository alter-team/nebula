//! The SQLite database: the only channel between CLI, daemon and `nebula go`,
//! and the permanent history.

use crate::util::{ct_eq, now_ms, sha256_hex};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Hard cap on stored output per run.
pub const OUTPUT_CAP: u64 = 50 * 1024 * 1024;

pub const ST_PENDING: &str = "pending";
pub const ST_FETCHED: &str = "fetched";
pub const ST_RUNNING: &str = "running";
pub const ST_DONE: &str = "done";
pub const ST_CANCELLED: &str = "cancelled";
pub const ST_EXPIRED: &str = "expired";
/// Fetched but never started, or running without a sign of life for too long.
pub const ST_LOST: &str = "lost";
/// A multi-use link that will not be fetched any more (all uses taken, or
/// the TTL passed after at least one use). Its executions carry the results.
pub const ST_CLOSED: &str = "closed";

/// Largest `--uses` value; 0 means unlimited until the TTL.
pub const MAX_USES: i64 = 1000;
/// Smallest allowed `--stale-after`: at least 3x the wrapper's ~10 s heartbeat.
pub const MIN_STALE_SECS: i64 = 30;
/// A claimed hook not finished this long after the claim, whose runner is
/// gone, is recorded as "unknown (runner died)" (hook timeout 30 s + margin).
pub const HOOK_STALE_MS: i64 = 45_000;

/// A fetched run that has not reported "started" within this time is lost.
pub const FETCH_START_GRACE_MS: i64 = 120_000;
/// Default for `--stale-after`: a running run silent for this long is lost.
pub const DEFAULT_STALE_SECS: i64 = 600;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS scripts (
    sha256      TEXT PRIMARY KEY,
    body        BLOB NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS runs (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    token            TEXT NOT NULL,
    token_hash       TEXT NOT NULL UNIQUE,
    target           TEXT NOT NULL CHECK (target IN ('local','remote')),
    name             TEXT,
    note             TEXT,
    script_sha       TEXT NOT NULL REFERENCES scripts(sha256),
    state            TEXT NOT NULL,
    echo             INTEGER NOT NULL DEFAULT 0,
    delete_after     INTEGER NOT NULL DEFAULT 0,
    ttl_secs         INTEGER NOT NULL,
    created_at       INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL,
    fetched_at       INTEGER,
    started_at       INTEGER,
    last_seen_at     INTEGER,
    finished_at      INTEGER,
    exit_code        INTEGER,
    host             TEXT,
    user             TEXT,
    via              TEXT,
    base_url         TEXT,
    output_bytes     INTEGER NOT NULL DEFAULT 0,
    reported_bytes   INTEGER,
    output_truncated INTEGER NOT NULL DEFAULT 0,
    output_deleted   INTEGER NOT NULL DEFAULT 0,
    stale_secs       INTEGER NOT NULL DEFAULT 600,
    on_done          TEXT,
    tmux_target      TEXT,
    tmux_socket      TEXT,
    tmux_inline      INTEGER NOT NULL DEFAULT 0,
    hook_claimed_at  INTEGER,
    hook_finished_at INTEGER,
    hook_result      TEXT,
    hook_pid         INTEGER,
    parent_id        INTEGER,
    seq              INTEGER,
    uses             INTEGER NOT NULL DEFAULT 1,
    fetch_count      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS runs_state ON runs(state);
CREATE TABLE IF NOT EXISTS chunks (
    run_id  INTEGER NOT NULL,
    off     INTEGER NOT NULL,
    data    BLOB NOT NULL,
    PRIMARY KEY (run_id, off)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS daemon (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL
) WITHOUT ROWID;
";

/// Directory holding `nebula.db`: `$NEBULA_HOME`, else `$XDG_DATA_HOME/nebula`,
/// else `~/.local/share/nebula`.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("NEBULA_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d).join("nebula"));
    }
    let home = std::env::var_os("HOME")
        .filter(|d| !d.is_empty())
        .context("neither NEBULA_HOME, XDG_DATA_HOME nor HOME is set")?;
    Ok(PathBuf::from(home).join(".local/share/nebula"))
}

pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("nebula.db"))
}

/// True for an empty directory or one with nothing but Nebula's database files.
fn holds_only_nebula_files(dir: &Path) -> bool {
    const OURS: [&str; 4] = [
        "nebula.db",
        "nebula.db-wal",
        "nebula.db-shm",
        "nebula.db-journal",
    ];
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.flatten().all(|e| {
            e.file_name().to_str().is_some_and(|n| OURS.contains(&n))
                && e.file_type().is_ok_and(|t| t.is_file())
        }),
        Err(_) => false,
    }
}

/// Open (creating if needed) the database in the configured data directory.
pub fn open() -> Result<Connection> {
    let dir = data_dir()?;
    open_in(&dir)
}

pub fn open_in(dir: &Path) -> Result<Connection> {
    if !dir.is_dir() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        // `mode` only applies to the directories we created; make sure of the last one.
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    } else if holds_only_nebula_files(dir) {
        // Nebula's own directory (or an empty one): keep it private.
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    } else if let Ok(meta) = std::fs::metadata(dir)
        && meta.permissions().mode() & 0o077 != 0
    {
        // Somebody else's directory (NEBULA_HOME=~/Desktop, say): not ours to
        // lock. The database file itself is still 0600.
        eprintln!(
            "nebula: warning: {} (holding the database) is accessible to other users \
             (mode {:o}) and holds other files, so it is left alone",
            dir.display(),
            meta.permissions().mode() & 0o777
        );
    }
    let path = dir.join("nebula.db");
    // Create the file ourselves so it is 0600 from the first byte; SQLite gives
    // the -wal and -shm files the same mode as the main file.
    //
    // Only when it does not exist yet: opening and closing our own descriptor
    // on an existing database would drop every POSIX lock this process holds
    // on it through other connections (the daemon has two), and another
    // process could then decide it is the last user and delete the WAL.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("cannot create {}", path.display())),
    }
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let conn =
        Connection::open(&path).with_context(|| format!("cannot open {}", path.display()))?;
    init(&conn)?;
    Ok(conn)
}

fn init(conn: &Connection) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(15))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Deleted rows are overwritten with zeros in the database file (per
    // connection setting, so every connection sets it).
    let _: i64 = conn.query_row("PRAGMA secure_delete = ON", [], |r| r.get(0))?;
    conn.execute_batch(SCHEMA)?;
    migrate(conn)?;
    Ok(())
}

/// Add columns introduced after the first version to an existing database.
fn migrate(conn: &Connection) -> Result<()> {
    let mut have = Vec::new();
    {
        let mut st = conn.prepare("SELECT name FROM pragma_table_info('runs')")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        for r in rows {
            have.push(r?);
        }
    }
    for (col, decl) in [
        ("stale_secs", "INTEGER NOT NULL DEFAULT 600"),
        ("on_done", "TEXT"),
        ("tmux_target", "TEXT"),
        ("tmux_socket", "TEXT"),
        ("tmux_inline", "INTEGER NOT NULL DEFAULT 0"),
        ("hook_claimed_at", "INTEGER"),
        ("hook_finished_at", "INTEGER"),
        ("hook_result", "TEXT"),
        ("hook_pid", "INTEGER"),
        ("parent_id", "INTEGER"),
        ("seq", "INTEGER"),
        ("uses", "INTEGER NOT NULL DEFAULT 1"),
        ("fetch_count", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !have.iter().any(|h| h == col) {
            conn.execute_batch(&format!("ALTER TABLE runs ADD COLUMN {col} {decl}"))?;
        }
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS runs_parent ON runs(parent_id, seq) WHERE parent_id IS NOT NULL",
    )?;
    Ok(())
}

/// Fold the write-ahead log into the database file and truncate it to zero
/// bytes, so that deleted data does not survive in old WAL frames. Returns
/// false if another connection was in the way the whole time (the daemon
/// repeats this periodically, and every later deletion tries again).
pub fn checkpoint_truncate(conn: &Connection) -> bool {
    for _ in 0..60 {
        let busy: rusqlite::Result<i64> =
            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0));
        if busy == Ok(0) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

#[cfg(test)]
pub fn open_memory() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: i64,
    pub token: String,
    pub target: String,
    pub name: Option<String>,
    pub note: Option<String>,
    pub script_sha: String,
    pub state: String,
    pub echo: bool,
    pub delete_after: bool,
    pub ttl_secs: i64,
    pub created_at: i64,
    pub expires_at: i64,
    pub fetched_at: Option<i64>,
    pub started_at: Option<i64>,
    pub last_seen_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub exit_code: Option<i64>,
    pub host: Option<String>,
    pub user: Option<String>,
    pub via: Option<String>,
    pub base_url: Option<String>,
    pub output_bytes: i64,
    pub reported_bytes: Option<i64>,
    pub output_truncated: bool,
    pub output_deleted: bool,
    pub stale_secs: i64,
    pub on_done: Option<String>,
    pub tmux_target: Option<String>,
    pub tmux_socket: Option<String>,
    pub tmux_inline: bool,
    pub hook_claimed_at: Option<i64>,
    pub hook_finished_at: Option<i64>,
    pub hook_result: Option<String>,
    pub hook_pid: Option<i64>,
    /// Set on an execution of a multi-use run: the run it belongs to.
    pub parent_id: Option<i64>,
    /// Execution number within the parent (1, 2, ...).
    pub seq: Option<i64>,
    /// How many fetches the link allows: 1 (default, single use), N, or 0
    /// for unlimited until the TTL. Only meaningful on a top-level run.
    pub uses: i64,
    /// Executions started from a multi-use run so far.
    pub fetch_count: i64,
}

impl Run {
    /// What humans and the CLI call it: `42`, or `42.2` for an execution.
    pub fn label(&self) -> String {
        match (self.parent_id, self.seq) {
            (Some(p), Some(n)) => format!("{p}.{n}"),
            _ => self.id.to_string(),
        }
    }
    /// A top-level run whose link may be fetched more than once.
    pub fn is_multi(&self) -> bool {
        self.parent_id.is_none() && self.uses != 1
    }
    pub fn is_remote(&self) -> bool {
        self.target == "remote"
    }
    pub fn is_finished(&self) -> bool {
        matches!(
            self.state.as_str(),
            ST_DONE | ST_CANCELLED | ST_EXPIRED | ST_LOST | ST_CLOSED
        )
    }
    pub fn has_hook(&self) -> bool {
        self.on_done.is_some() || self.tmux_target.is_some()
    }
    pub fn is_active(&self) -> bool {
        matches!(self.state.as_str(), ST_FETCHED | ST_RUNNING)
    }
}

const RUN_COLS: &str = "id, token, target, name, note, script_sha, state, echo, delete_after, \
    ttl_secs, created_at, expires_at, fetched_at, started_at, last_seen_at, finished_at, \
    exit_code, host, user, via, base_url, output_bytes, reported_bytes, output_truncated, \
    output_deleted, stale_secs, on_done, tmux_target, tmux_socket, tmux_inline, \
    hook_claimed_at, hook_finished_at, hook_result, hook_pid, parent_id, seq, uses, \
    fetch_count";

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: r.get(0)?,
        token: r.get(1)?,
        target: r.get(2)?,
        name: r.get(3)?,
        note: r.get(4)?,
        script_sha: r.get(5)?,
        state: r.get(6)?,
        echo: r.get(7)?,
        delete_after: r.get(8)?,
        ttl_secs: r.get(9)?,
        created_at: r.get(10)?,
        expires_at: r.get(11)?,
        fetched_at: r.get(12)?,
        started_at: r.get(13)?,
        last_seen_at: r.get(14)?,
        finished_at: r.get(15)?,
        exit_code: r.get(16)?,
        host: r.get(17)?,
        user: r.get(18)?,
        via: r.get(19)?,
        base_url: r.get(20)?,
        output_bytes: r.get(21)?,
        reported_bytes: r.get(22)?,
        output_truncated: r.get(23)?,
        output_deleted: r.get(24)?,
        stale_secs: r.get(25)?,
        on_done: r.get(26)?,
        tmux_target: r.get(27)?,
        tmux_socket: r.get(28)?,
        tmux_inline: r.get(29)?,
        hook_claimed_at: r.get(30)?,
        hook_finished_at: r.get(31)?,
        hook_result: r.get(32)?,
        hook_pid: r.get(33)?,
        parent_id: r.get(34)?,
        seq: r.get(35)?,
        uses: r.get(36)?,
        fetch_count: r.get(37)?,
    })
}

// ---------------------------------------------------------------- scripts

/// Store a script body (deduplicated by hash) and return its sha256.
pub fn put_script(conn: &Connection, body: &[u8]) -> Result<String> {
    let sha = sha256_hex(body);
    conn.execute(
        "INSERT OR IGNORE INTO scripts (sha256, body, created_at) VALUES (?1, ?2, ?3)",
        params![sha, body, now_ms()],
    )?;
    Ok(sha)
}

pub fn get_script(conn: &Connection, sha: &str) -> Result<Option<Vec<u8>>> {
    Ok(conn
        .query_row("SELECT body FROM scripts WHERE sha256 = ?1", [sha], |r| {
            r.get(0)
        })
        .optional()?)
}

// ------------------------------------------------------------------- runs

#[derive(Default)]
pub struct NewRun<'a> {
    pub token: &'a str,
    pub target: &'a str,
    pub name: Option<&'a str>,
    pub note: Option<&'a str>,
    pub script_sha: &'a str,
    pub echo: bool,
    pub delete_after: bool,
    pub ttl_secs: i64,
    /// 0 means the default.
    pub stale_secs: i64,
    pub on_done: Option<&'a str>,
    pub tmux_target: Option<&'a str>,
    pub tmux_socket: Option<&'a str>,
    pub tmux_inline: bool,
    /// `None`: single use (the default). `Some(n)`: the link may be fetched
    /// n times (0 = unlimited until the TTL).
    pub uses: Option<i64>,
}

pub fn create_run(conn: &Connection, n: &NewRun<'_>) -> Result<i64> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO runs (token, token_hash, target, name, note, script_sha, state, echo, \
         delete_after, ttl_secs, created_at, expires_at, stale_secs, on_done, tmux_target, \
         tmux_socket, tmux_inline, uses) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
         ?17)",
        params![
            n.token,
            sha256_hex(n.token.as_bytes()),
            n.target,
            n.name,
            n.note,
            n.script_sha,
            n.echo,
            n.delete_after,
            n.ttl_secs,
            now,
            now.saturating_add(n.ttl_secs.saturating_mul(1000)),
            if n.stale_secs > 0 {
                n.stale_secs
            } else {
                DEFAULT_STALE_SECS
            },
            n.on_done,
            n.tmux_target,
            n.tmux_socket,
            n.tmux_inline,
            n.uses.unwrap_or(1),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_run(conn: &Connection, id: i64) -> Result<Option<Run>> {
    Ok(conn
        .query_row(
            &format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1"),
            [id],
            row_to_run,
        )
        .optional()?)
}

/// Look a run up by its secret token. The index lookup goes through the
/// token's sha256 (so index timing reveals nothing useful) and the stored
/// token is then compared in constant time.
pub fn get_run_by_token(conn: &Connection, token: &str) -> Result<Option<Run>> {
    let run = conn
        .query_row(
            &format!("SELECT {RUN_COLS} FROM runs WHERE token_hash = ?1"),
            [sha256_hex(token.as_bytes())],
            row_to_run,
        )
        .optional()?;
    Ok(run.filter(|r| ct_eq(r.token.as_bytes(), token.as_bytes())))
}

pub fn list_runs(conn: &Connection, limit: i64) -> Result<Vec<Run>> {
    let mut st = conn.prepare(&format!(
        "SELECT {RUN_COLS} FROM runs WHERE parent_id IS NULL ORDER BY id DESC LIMIT ?1"
    ))?;
    let rows = st.query_map([limit], row_to_run)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The executions of a multi-use run, in order.
pub fn executions(conn: &Connection, parent: i64) -> Result<Vec<Run>> {
    let mut st = conn.prepare(&format!(
        "SELECT {RUN_COLS} FROM runs WHERE parent_id = ?1 ORDER BY seq"
    ))?;
    let rows = st.query_map([parent], row_to_run)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Resolve what a human typed: `42` (a top-level run) or `42.2` (execution
/// 2 of run 42). Internal row ids of executions are not accepted as `N`.
pub fn resolve(conn: &Connection, reference: &str) -> Result<Option<Run>> {
    let parse = |s: &str| s.parse::<i64>().ok().filter(|n| *n > 0);
    match reference.split_once('.') {
        None => {
            let Some(id) = parse(reference) else {
                bail!("{reference:?} is not a run id (expected N or N.M)");
            };
            Ok(get_run(conn, id)?.filter(|r| r.parent_id.is_none()))
        }
        Some((p, n)) => {
            let (Some(p), Some(n)) = (parse(p), parse(n)) else {
                bail!("{reference:?} is not a run id (expected N or N.M)");
            };
            Ok(conn
                .query_row(
                    &format!("SELECT {RUN_COLS} FROM runs WHERE parent_id = ?1 AND seq = ?2"),
                    [p, n],
                    row_to_run,
                )
                .optional()?)
        }
    }
}

/// Start an execution of a multi-use run: one use of its link. Returns the
/// new execution, or None if the link is used up, expired or not pending.
/// `via` is "http" (state fetched) or "go" (state running, host/user set).
pub fn claim_execution(
    conn: &mut Connection,
    parent: i64,
    via: &str,
    base_url: Option<&str>,
    host_user: Option<(&str, &str)>,
) -> Result<Option<Run>> {
    let now = now_ms();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let p = tx
        .query_row(
            &format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1"),
            [parent],
            row_to_run,
        )
        .optional()?;
    let Some(p) = p else { return Ok(None) };
    if !p.is_multi()
        || p.state != ST_PENDING
        || now >= p.expires_at
        || (p.uses > 0 && p.fetch_count >= p.uses)
        || (via == "go" && p.is_remote())
    {
        return Ok(None);
    }
    let seq = p.fetch_count + 1;
    let token = crate::util::new_token()?;
    let (state, started) = if via == "go" {
        (ST_RUNNING, Some(now))
    } else {
        (ST_FETCHED, None)
    };
    tx.execute(
        "INSERT INTO runs (token, token_hash, target, name, note, script_sha, state, echo, \
         delete_after, ttl_secs, created_at, expires_at, stale_secs, on_done, tmux_target, \
         tmux_socket, tmux_inline, uses, parent_id, seq, fetched_at, last_seen_at, via, \
         base_url, started_at, host, user) \
         SELECT ?2, ?3, target, name, note, script_sha, ?4, echo, delete_after, ttl_secs, ?5, \
         expires_at, stale_secs, on_done, tmux_target, tmux_socket, tmux_inline, 1, id, ?6, ?5, \
         ?5, ?7, ?8, ?9, ?10, ?11 FROM runs WHERE id = ?1",
        params![
            parent,
            token,
            sha256_hex(token.as_bytes()),
            state,
            now,
            seq,
            via,
            base_url,
            started,
            host_user.map(|h| h.0),
            host_user.map(|h| h.1),
        ],
    )?;
    let child = tx.last_insert_rowid();
    let closes = p.uses > 0 && seq >= p.uses;
    tx.execute(
        "UPDATE runs SET fetch_count = ?2, fetched_at = COALESCE(fetched_at, ?3), \
         state = CASE WHEN ?4 THEN 'closed' ELSE state END, \
         finished_at = CASE WHEN ?4 THEN ?3 ELSE finished_at END WHERE id = ?1",
        params![parent, seq, now, closes],
    )?;
    let run = tx.query_row(
        &format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1"),
        [child],
        row_to_run,
    )?;
    tx.commit()?;
    Ok(Some(run))
}

pub fn open_runs(conn: &Connection) -> Result<Vec<Run>> {
    let mut st = conn.prepare(&format!(
        "SELECT {RUN_COLS} FROM runs WHERE state IN ('pending','fetched','running') ORDER BY id"
    ))?;
    let rows = st.query_map([], row_to_run)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Mark every pending run whose TTL has passed as expired. Returns how many.
pub fn expire_pending(conn: &Connection) -> Result<usize> {
    let now = now_ms();
    let closed = conn.execute(
        "UPDATE runs SET state = 'closed', finished_at = ?1 \
         WHERE state = 'pending' AND expires_at <= ?1 AND fetch_count > 0",
        [now],
    )?;
    Ok(closed
        + conn.execute(
            "UPDATE runs SET state = 'expired', finished_at = ?1 \
             WHERE state = 'pending' AND expires_at <= ?1",
            [now],
        )?)
}

/// Mark stale runs as lost: fetched but not started within two minutes, or
/// running with no sign of life for the run's `stale_secs`. Returns how many.
pub fn mark_lost(conn: &Connection) -> Result<usize> {
    let now = now_ms();
    let a = conn.execute(
        "UPDATE runs SET state = 'lost', finished_at = ?1 \
         WHERE state = 'fetched' AND started_at IS NULL AND fetched_at <= ?2",
        params![now, now - FETCH_START_GRACE_MS],
    )?;
    let b = conn.execute(
        "UPDATE runs SET state = 'lost', finished_at = ?1 \
         WHERE state = 'running' \
         AND COALESCE(last_seen_at, started_at, fetched_at, created_at) <= ?1 - stale_secs * 1000",
        [now],
    )?;
    Ok(a + b)
}

/// Whether [`sweep`] would change this run right now.
pub fn is_overdue(run: &Run) -> bool {
    let now = now_ms();
    match run.state.as_str() {
        ST_PENDING => now >= run.expires_at,
        ST_FETCHED => {
            run.started_at.is_none()
                && run
                    .fetched_at
                    .is_some_and(|t| t <= now - FETCH_START_GRACE_MS)
        }
        ST_RUNNING => {
            let seen = run
                .last_seen_at
                .or(run.started_at)
                .or(run.fetched_at)
                .unwrap_or(run.created_at);
            seen <= now - run.stale_secs.saturating_mul(1000)
        }
        _ => false,
    }
}

/// Expire unfetched links and mark stale runs as lost.
pub fn sweep(conn: &Connection) -> Result<usize> {
    Ok(expire_pending(conn)? + mark_lost(conn)? + reap_dead_hooks(conn)?)
}

/// Whether `pid` is a live `nebula hook-run` process.
pub fn pid_is_hook_runner(pid: i64) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| {
            let args: Vec<&[u8]> = c.split(|b| *b == 0).collect();
            args.iter().any(|a| *a == b"hook-run")
                && std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .is_ok_and(|s| !s.rsplit(')').next().unwrap_or("").starts_with(" Z"))
        })
        .unwrap_or(false)
}

/// Hooks that were claimed but whose runner died (or never started) are
/// recorded as "unknown (runner died)". They are NOT fired again: a hook
/// fires at most once, even if that means it may not have fired at all.
pub fn reap_dead_hooks(conn: &Connection) -> Result<usize> {
    let cutoff = now_ms() - HOOK_STALE_MS;
    let stuck: Vec<(i64, Option<i64>)> = {
        let mut st = conn.prepare(
            "SELECT id, hook_pid FROM runs WHERE hook_claimed_at IS NOT NULL \
             AND hook_finished_at IS NULL AND hook_claimed_at <= ?1",
        )?;
        let rows = st.query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut n = 0;
    for (id, pid) in stuck {
        if pid.is_some_and(pid_is_hook_runner) {
            continue;
        }
        n += conn.execute(
            "UPDATE runs SET hook_finished_at = ?2, hook_result = 'unknown (runner died)' \
             WHERE id = ?1 AND hook_finished_at IS NULL",
            params![id, now_ms()],
        )?;
    }
    Ok(n)
}

// ------------------------------------------------------------------ hooks

/// Finished runs whose completion hook has not been claimed yet.
pub fn pending_hooks(conn: &Connection) -> Result<Vec<i64>> {
    let mut st = conn.prepare(
        "SELECT id FROM runs WHERE hook_claimed_at IS NULL \
         AND (on_done IS NOT NULL OR tmux_target IS NOT NULL) \
         AND state IN ('done','cancelled','expired','lost') AND fetch_count = 0 ORDER BY id",
    )?;
    let rows = st.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The exactly-once guard: true for the one caller that may run the hook.
pub fn claim_hook(conn: &Connection, id: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE runs SET hook_claimed_at = ?2 WHERE id = ?1 AND hook_claimed_at IS NULL \
         AND (on_done IS NOT NULL OR tmux_target IS NOT NULL) \
         AND state IN ('done','cancelled','expired','lost') AND fetch_count = 0",
        params![id, now_ms()],
    )?;
    Ok(n == 1)
}

/// Second guard, taken by the `nebula hook-run` process itself: exactly one
/// runner per claimed hook, and its pid is recorded so that a dead runner
/// can be told from a slow one.
pub fn claim_hook_runner(conn: &Connection, id: i64, pid: u32) -> Result<bool> {
    let n = conn.execute(
        "UPDATE runs SET hook_pid = ?2 WHERE id = ?1 AND hook_claimed_at IS NOT NULL \
         AND hook_finished_at IS NULL AND hook_pid IS NULL",
        params![id, pid],
    )?;
    Ok(n == 1)
}

pub fn finish_hook(conn: &Connection, id: i64, result: &str) -> Result<()> {
    conn.execute(
        "UPDATE runs SET hook_finished_at = ?2, hook_result = ?3 WHERE id = ?1",
        params![id, now_ms(), result],
    )?;
    Ok(())
}

/// Single-use claim of a pending run by the HTTP fetch. Returns true exactly
/// once per run, and only before the TTL.
pub fn claim_fetch(conn: &Connection, id: i64, base_url: &str) -> Result<bool> {
    let now = now_ms();
    let n = conn.execute(
        "UPDATE runs SET state = 'fetched', fetched_at = ?2, last_seen_at = ?2, via = 'http', \
         base_url = ?3 \
         WHERE id = ?1 AND state = 'pending' AND fetched_at IS NULL AND expires_at > ?2",
        params![id, now, base_url],
    )?;
    Ok(n == 1)
}

/// Single-use claim of a pending local run by `nebula go`.
pub fn claim_go(conn: &Connection, id: i64, host: &str, user: &str) -> Result<bool> {
    let now = now_ms();
    let n = conn.execute(
        "UPDATE runs SET state = 'running', fetched_at = ?2, started_at = ?2, last_seen_at = ?2, \
         via = 'go', host = ?3, user = ?4 \
         WHERE id = ?1 AND target = 'local' AND state = 'pending' AND fetched_at IS NULL \
         AND expires_at > ?2",
        params![id, now, host, user],
    )?;
    Ok(n == 1)
}

pub fn mark_started(conn: &Connection, id: i64, host: &str, user: &str) -> Result<bool> {
    let now = now_ms();
    let n = conn.execute(
        "UPDATE runs SET state = 'running', started_at = COALESCE(started_at, ?2), \
         last_seen_at = ?2, host = ?3, user = ?4 \
         WHERE id = ?1 AND state IN ('fetched','running')",
        params![id, now, host, user],
    )?;
    Ok(n == 1)
}

pub fn touch(conn: &Connection, id: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE runs SET last_seen_at = ?2 WHERE id = ?1 AND state IN ('fetched','running')",
        params![id, now_ms()],
    )?;
    Ok(n == 1)
}

pub fn mark_done(conn: &Connection, id: i64, code: i64, reported: Option<i64>) -> Result<bool> {
    let now = now_ms();
    let n = conn.execute(
        "UPDATE runs SET state = 'done', finished_at = ?2, last_seen_at = ?2, exit_code = ?3, \
         reported_bytes = ?4 \
         WHERE id = ?1 AND state IN ('fetched','running')",
        params![id, now, code, reported],
    )?;
    Ok(n == 1)
}

/// Cancel a run. For a multi-use run this also cancels its executions that
/// are still in progress (and its link).
pub fn cancel_run(conn: &Connection, id: i64) -> Result<bool> {
    let now = now_ms();
    let n = conn.execute(
        "UPDATE runs SET state = 'cancelled', finished_at = ?2 \
         WHERE id = ?1 AND state IN ('pending','fetched','running')",
        params![id, now],
    )?;
    let m = conn.execute(
        "UPDATE runs SET state = 'cancelled', finished_at = ?2 \
         WHERE parent_id = ?1 AND state IN ('fetched','running')",
        params![id, now],
    )?;
    if m > 0 {
        // a closed multi-use run with executions in progress: close it for good
        conn.execute(
            "UPDATE runs SET state = 'cancelled' WHERE id = ?1 AND state = 'closed'",
            [id],
        )?;
    }
    Ok(n + m > 0)
}

// ----------------------------------------------------------------- output

#[derive(Debug, PartialEq, Eq)]
pub enum Append {
    /// New bytes were stored.
    Appended,
    /// Everything in this request was already stored (a retry).
    Duplicate,
    /// The offset is beyond what is stored; `expected` is the next offset.
    Gap { expected: u64 },
    /// The 50 MB cap is reached; anything beyond it was dropped.
    Full,
    /// The run is not in a state that accepts output.
    NotActive,
}

/// Append output at a byte offset. Idempotent: re-sending bytes that are
/// already stored is a no-op, a partially overlapping request stores only
/// the new tail.
pub fn append_output(conn: &mut Connection, id: i64, off: u64, data: &[u8]) -> Result<Append> {
    append_output_capped(conn, id, off, data, OUTPUT_CAP)
}

pub fn append_output_capped(
    conn: &mut Connection,
    id: i64,
    off: u64,
    data: &[u8],
    cap: u64,
) -> Result<Append> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row: Option<(String, i64, bool)> = tx
        .query_row(
            "SELECT state, output_bytes, output_truncated FROM runs WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((state, total, truncated)) = row else {
        return Ok(Append::NotActive);
    };
    if state != ST_FETCHED && state != ST_RUNNING {
        return Ok(Append::NotActive);
    }
    let total = total as u64;
    let now = now_ms();
    let result = if truncated {
        Append::Full
    } else if off > total {
        Append::Gap { expected: total }
    } else {
        let skip = (total - off) as usize;
        if skip >= data.len() {
            Append::Duplicate
        } else {
            let fresh = &data[skip..];
            let room = cap.saturating_sub(total) as usize;
            let (store, full) = if fresh.len() > room {
                (&fresh[..room], true)
            } else {
                (fresh, false)
            };
            if !store.is_empty() {
                tx.execute(
                    "INSERT INTO chunks (run_id, off, data) VALUES (?1, ?2, ?3)",
                    params![id, total as i64, store],
                )?;
            }
            tx.execute(
                "UPDATE runs SET output_bytes = ?2, output_truncated = ?3 WHERE id = ?1",
                params![id, (total + store.len() as u64) as i64, full],
            )?;
            if full { Append::Full } else { Append::Appended }
        }
    };
    tx.execute(
        "UPDATE runs SET last_seen_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    tx.commit()?;
    Ok(result)
}

/// Chunks starting at or after `from` (chunks are contiguous and never overlap).
pub fn chunks_from(conn: &Connection, id: i64, from: u64) -> Result<Vec<(u64, Vec<u8>)>> {
    let mut st = conn.prepare(
        "SELECT off, data FROM chunks WHERE run_id = ?1 AND off >= ?2 ORDER BY off LIMIT 16",
    )?;
    let rows = st.query_map(params![id, from as i64], |r| {
        Ok((r.get::<_, i64>(0)? as u64, r.get::<_, Vec<u8>>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Delete the stored output of a run (and of all its executions). Also
/// removes an inlined result from a stored hook result (older versions kept it).
pub fn delete_output(conn: &mut Connection, id: i64) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let ids: Vec<i64> = {
        let mut st = tx.prepare("SELECT id FROM runs WHERE id = ?1 OR parent_id = ?1")?;
        let rows = st.query_map([id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for rid in ids {
        tx.execute("DELETE FROM chunks WHERE run_id = ?1", [rid])?;
        tx.execute("UPDATE runs SET output_deleted = 1 WHERE id = ?1", [rid])?;
        let stored: Option<String> = tx
            .query_row("SELECT hook_result FROM runs WHERE id = ?1", [rid], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        if let Some(clean) = stored.as_deref().and_then(crate::hooks::redact_stored) {
            tx.execute(
                "UPDATE runs SET hook_result = ?2 WHERE id = ?1",
                params![rid, clean],
            )?;
        }
    }
    tx.commit()?;
    checkpoint_truncate(conn);
    Ok(())
}

/// Remove a run entirely (row, output, and the script body if no other run uses it).
pub fn delete_run(conn: &mut Connection, id: i64) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let sha: Option<String> = tx
        .query_row("SELECT script_sha FROM runs WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(sha) = sha else {
        bail!("no run {id}");
    };
    tx.execute(
        "DELETE FROM chunks WHERE run_id = ?1 OR run_id IN (SELECT id FROM runs WHERE parent_id = ?1)",
        [id],
    )?;
    tx.execute("DELETE FROM runs WHERE id = ?1 OR parent_id = ?1", [id])?;
    tx.execute(
        "DELETE FROM scripts WHERE sha256 = ?1 \
         AND NOT EXISTS (SELECT 1 FROM runs WHERE script_sha = ?1)",
        [&sha],
    )?;
    tx.commit()?;
    checkpoint_truncate(conn);
    Ok(())
}

// ----------------------------------------------------------- daemon state

pub fn set_kv(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO daemon (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [key, value],
    )?;
    Ok(())
}

pub fn get_kv(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM daemon WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .optional()?)
}

pub const TUNNEL_UP: &str = "up";
pub const TUNNEL_STARTING: &str = "starting";
pub const TUNNEL_DOWN: &str = "down";
pub const TUNNEL_DISABLED: &str = "disabled";
pub const TUNNEL_MISSING: &str = "missing";

#[derive(Debug, Clone, Default)]
pub struct DaemonInfo {
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub started_at: Option<i64>,
    /// pid exists, looks like a nebula daemon, and the port accepts connections.
    pub alive: bool,
    /// One of the TUNNEL_* values (meaningful only while `alive`).
    pub tunnel_state: String,
    /// Public base address, e.g. `https://x.trycloudflare.com` (no trailing slash).
    pub tunnel_url: Option<String>,
    pub tunnel_detail: Option<String>,
}

pub fn pid_is_nebula_daemon(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|c| {
            let args: Vec<&[u8]> = c.split(|b| *b == 0).collect();
            args.first()
                .is_some_and(|a| String::from_utf8_lossy(a).contains("nebula"))
                && args.iter().any(|a| *a == b"daemon")
        })
        .unwrap_or(false)
}

pub fn daemon_info(conn: &Connection) -> Result<DaemonInfo> {
    let pid = get_kv(conn, "pid")?.and_then(|v| v.parse::<u32>().ok());
    let port = get_kv(conn, "port")?.and_then(|v| v.parse::<u16>().ok());
    let alive = match (pid, port) {
        (Some(pid), Some(port)) => {
            pid_is_nebula_daemon(pid)
                && std::net::TcpStream::connect_timeout(
                    &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    std::time::Duration::from_millis(500),
                )
                .is_ok()
        }
        _ => false,
    };
    Ok(DaemonInfo {
        pid,
        port,
        started_at: get_kv(conn, "started_at")?.and_then(|v| v.parse().ok()),
        alive,
        tunnel_state: get_kv(conn, "tunnel_state")?.unwrap_or_else(|| TUNNEL_DOWN.into()),
        tunnel_url: get_kv(conn, "tunnel_url")?.filter(|u| !u.is_empty()),
        tunnel_detail: get_kv(conn, "tunnel_detail")?.filter(|u| !u.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(conn: &Connection, target: &str, ttl: i64) -> (i64, String) {
        let sha = put_script(conn, b"echo hi\n").unwrap();
        let token = crate::util::new_token().unwrap();
        let id = create_run(
            conn,
            &NewRun {
                token: &token,
                target,
                name: None,
                note: None,
                script_sha: &sha,
                echo: false,
                delete_after: false,
                ttl_secs: ttl,
                ..Default::default()
            },
        )
        .unwrap();
        (id, token)
    }

    fn all_output(conn: &Connection, id: i64) -> Vec<u8> {
        chunks_from(conn, id, 0)
            .unwrap()
            .into_iter()
            .flat_map(|(_, d)| d)
            .collect()
    }

    #[test]
    fn scripts_are_deduplicated() {
        let conn = open_memory();
        let a = put_script(&conn, b"echo 1\n").unwrap();
        let b = put_script(&conn, b"echo 1\n").unwrap();
        assert_eq!(a, b);
        let n: i64 = conn
            .query_row("SELECT count(*) FROM scripts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(get_script(&conn, &a).unwrap().unwrap(), b"echo 1\n");
    }

    #[test]
    fn token_lookup() {
        let conn = open_memory();
        let (id, token) = mk(&conn, "local", 60);
        assert_eq!(get_run_by_token(&conn, &token).unwrap().unwrap().id, id);
        let mut other = token.clone().into_bytes();
        other[0] = if other[0] == b'A' { b'B' } else { b'A' };
        let other = String::from_utf8(other).unwrap();
        assert!(get_run_by_token(&conn, &other).unwrap().is_none());
        assert!(get_run_by_token(&conn, "").unwrap().is_none());
    }

    #[test]
    fn fetch_is_single_use() {
        let conn = open_memory();
        let (id, _) = mk(&conn, "local", 60);
        assert!(claim_fetch(&conn, id, "http://x").unwrap());
        assert!(!claim_fetch(&conn, id, "http://x").unwrap());
        assert!(!claim_go(&conn, id, "h", "u").unwrap());
        assert_eq!(get_run(&conn, id).unwrap().unwrap().state, ST_FETCHED);
    }

    #[test]
    fn ttl_expiry() {
        let conn = open_memory();
        let (id, _) = mk(&conn, "local", 0);
        assert!(!claim_fetch(&conn, id, "http://x").unwrap());
        assert!(!claim_go(&conn, id, "h", "u").unwrap());
        assert_eq!(expire_pending(&conn).unwrap(), 1);
        assert_eq!(get_run(&conn, id).unwrap().unwrap().state, ST_EXPIRED);
        // an unexpired run is untouched
        let (id2, _) = mk(&conn, "local", 60);
        assert_eq!(expire_pending(&conn).unwrap(), 0);
        assert_eq!(get_run(&conn, id2).unwrap().unwrap().state, ST_PENDING);
    }

    #[test]
    fn go_only_claims_local_runs() {
        let conn = open_memory();
        let (id, _) = mk(&conn, "remote", 60);
        assert!(!claim_go(&conn, id, "h", "u").unwrap());
        let (id, _) = mk(&conn, "local", 60);
        assert!(claim_go(&conn, id, "h", "u").unwrap());
        assert!(!claim_go(&conn, id, "h", "u").unwrap());
    }

    #[test]
    fn output_only_between_fetch_and_done() {
        let mut conn = open_memory();
        let (id, _) = mk(&conn, "local", 60);
        assert_eq!(
            append_output(&mut conn, id, 0, b"x").unwrap(),
            Append::NotActive
        );
        assert!(!mark_started(&conn, id, "h", "u").unwrap());
        assert!(!mark_done(&conn, id, 0, None).unwrap());
        assert!(claim_fetch(&conn, id, "http://x").unwrap());
        assert!(mark_started(&conn, id, "h", "u").unwrap());
        assert_eq!(
            append_output(&mut conn, id, 0, b"x").unwrap(),
            Append::Appended
        );
        assert!(mark_done(&conn, id, 3, Some(1)).unwrap());
        assert_eq!(
            append_output(&mut conn, id, 1, b"y").unwrap(),
            Append::NotActive
        );
        assert!(!touch(&conn, id).unwrap());
        assert!(!mark_done(&conn, id, 0, None).unwrap());
        let run = get_run(&conn, id).unwrap().unwrap();
        assert_eq!(run.exit_code, Some(3));
        assert_eq!(run.output_bytes, 1);
    }

    #[test]
    fn offsets_are_idempotent() {
        let mut conn = open_memory();
        let (id, _) = mk(&conn, "local", 60);
        claim_fetch(&conn, id, "http://x").unwrap();
        assert_eq!(
            append_output(&mut conn, id, 0, b"hello ").unwrap(),
            Append::Appended
        );
        // exact retry
        assert_eq!(
            append_output(&mut conn, id, 0, b"hello ").unwrap(),
            Append::Duplicate
        );
        // retry of a prefix
        assert_eq!(
            append_output(&mut conn, id, 0, b"hel").unwrap(),
            Append::Duplicate
        );
        // overlapping retry that carries new bytes: only the tail is stored
        assert_eq!(
            append_output(&mut conn, id, 3, b"lo world").unwrap(),
            Append::Appended
        );
        // gap
        assert_eq!(
            append_output(&mut conn, id, 100, b"zzz").unwrap(),
            Append::Gap { expected: 11 }
        );
        assert_eq!(
            append_output(&mut conn, id, 11, b"!").unwrap(),
            Append::Appended
        );
        assert_eq!(all_output(&conn, id), b"hello world!");
        assert_eq!(get_run(&conn, id).unwrap().unwrap().output_bytes, 12);
    }

    #[test]
    fn output_cap() {
        let mut conn = open_memory();
        let (id, _) = mk(&conn, "local", 60);
        claim_fetch(&conn, id, "http://x").unwrap();
        assert_eq!(
            append_output_capped(&mut conn, id, 0, b"12345678", 10).unwrap(),
            Append::Appended
        );
        assert_eq!(
            append_output_capped(&mut conn, id, 8, b"abcdef", 10).unwrap(),
            Append::Full
        );
        assert_eq!(
            append_output_capped(&mut conn, id, 14, b"more", 10).unwrap(),
            Append::Full
        );
        let run = get_run(&conn, id).unwrap().unwrap();
        assert_eq!(run.output_bytes, 10);
        assert!(run.output_truncated);
        assert_eq!(all_output(&conn, id), b"12345678ab");
        // done is still accepted after the cap
        assert!(mark_done(&conn, id, 0, Some(18)).unwrap());
    }

    #[test]
    fn cancel_and_delete() {
        let mut conn = open_memory();
        let (id, token) = mk(&conn, "local", 60);
        let (id2, _) = mk(&conn, "local", 60);
        assert!(cancel_run(&conn, id).unwrap());
        assert!(!cancel_run(&conn, id).unwrap());
        assert!(!claim_fetch(&conn, id, "http://x").unwrap());
        delete_run(&mut conn, id).unwrap();
        assert!(get_run_by_token(&conn, &token).unwrap().is_none());
        // script still referenced by id2
        let sha = get_run(&conn, id2).unwrap().unwrap().script_sha;
        assert!(get_script(&conn, &sha).unwrap().is_some());
        delete_run(&mut conn, id2).unwrap();
        assert!(get_script(&conn, &sha).unwrap().is_none());
        assert!(delete_run(&mut conn, id2).is_err());
    }

    #[test]
    fn stale_runs_become_lost() {
        let conn = open_memory();
        // fetched, never started
        let (a, _) = mk(&conn, "local", 60);
        claim_fetch(&conn, a, "http://x").unwrap();
        assert_eq!(mark_lost(&conn).unwrap(), 0);
        conn.execute(
            "UPDATE runs SET fetched_at = fetched_at - ?2 WHERE id = ?1",
            params![a, FETCH_START_GRACE_MS + 1],
        )
        .unwrap();
        assert!(is_overdue(&get_run(&conn, a).unwrap().unwrap()));
        // running, silent for longer than its stale_secs (default 600)
        let (b, _) = mk(&conn, "local", 60);
        claim_fetch(&conn, b, "http://x").unwrap();
        mark_started(&conn, b, "h", "u").unwrap();
        conn.execute(
            "UPDATE runs SET last_seen_at = last_seen_at - 601000, fetched_at = fetched_at - 601000 \
             WHERE id = ?1",
            [b],
        )
        .unwrap();
        // running and recently seen
        let (c, _) = mk(&conn, "local", 60);
        claim_fetch(&conn, c, "http://x").unwrap();
        mark_started(&conn, c, "h", "u").unwrap();
        assert!(!is_overdue(&get_run(&conn, c).unwrap().unwrap()));
        assert_eq!(sweep(&conn).unwrap(), 2);
        assert_eq!(get_run(&conn, a).unwrap().unwrap().state, ST_LOST);
        assert_eq!(get_run(&conn, b).unwrap().unwrap().state, ST_LOST);
        assert_eq!(get_run(&conn, c).unwrap().unwrap().state, ST_RUNNING);
        // a lost run accepts nothing any more
        assert!(!mark_done(&conn, a, 0, None).unwrap());
        assert!(!touch(&conn, b).unwrap());
        assert!(get_run(&conn, a).unwrap().unwrap().is_finished());
    }

    #[test]
    fn hook_is_claimed_exactly_once() {
        let conn = open_memory();
        let sha = put_script(&conn, b"true\n").unwrap();
        let token = crate::util::new_token().unwrap();
        let id = create_run(
            &conn,
            &NewRun {
                token: &token,
                target: "local",
                script_sha: &sha,
                ttl_secs: 60,
                on_done: Some("true"),
                ..Default::default()
            },
        )
        .unwrap();
        let (plain, _) = mk(&conn, "local", 60);
        // not finished yet: nothing to claim
        assert!(pending_hooks(&conn).unwrap().is_empty());
        assert!(!claim_hook(&conn, id).unwrap());
        cancel_run(&conn, id).unwrap();
        cancel_run(&conn, plain).unwrap();
        assert_eq!(pending_hooks(&conn).unwrap(), vec![id]);
        assert!(claim_hook(&conn, id).unwrap());
        assert!(!claim_hook(&conn, id).unwrap());
        assert!(!claim_hook(&conn, plain).unwrap(), "run without a hook");
        assert!(pending_hooks(&conn).unwrap().is_empty());
        finish_hook(&conn, id, "exit 0").unwrap();
        let run = get_run(&conn, id).unwrap().unwrap();
        assert_eq!(run.hook_result.as_deref(), Some("exit 0"));
        assert!(run.hook_finished_at.is_some());
        assert_eq!(run.stale_secs, DEFAULT_STALE_SECS);
    }

    #[test]
    fn existing_dir_is_tightened_and_old_db_migrated() {
        use std::os::unix::fs::PermissionsExt;
        let dir =
            std::env::temp_dir().join(format!("nebula-test-{}", crate::util::new_token().unwrap()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // a database from the first version: no hook columns
        {
            let old = Connection::open(dir.join("nebula.db")).unwrap();
            old.execute_batch(
                "CREATE TABLE runs (id INTEGER PRIMARY KEY AUTOINCREMENT, token TEXT NOT NULL, \
                 token_hash TEXT NOT NULL UNIQUE, target TEXT NOT NULL, name TEXT, note TEXT, \
                 script_sha TEXT NOT NULL, state TEXT NOT NULL, echo INTEGER NOT NULL DEFAULT 0, \
                 delete_after INTEGER NOT NULL DEFAULT 0, ttl_secs INTEGER NOT NULL, \
                 created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, fetched_at INTEGER, \
                 started_at INTEGER, last_seen_at INTEGER, finished_at INTEGER, exit_code INTEGER, \
                 host TEXT, user TEXT, via TEXT, base_url TEXT, \
                 output_bytes INTEGER NOT NULL DEFAULT 0, reported_bytes INTEGER, \
                 output_truncated INTEGER NOT NULL DEFAULT 0, \
                 output_deleted INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO runs (token, token_hash, target, script_sha, state, ttl_secs, \
                 created_at, expires_at) VALUES ('t', 'h', 'local', 's', 'done', 1, 1, 2);",
            )
            .unwrap();
        }
        let conn = open_in(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let run = get_run(&conn, 1).unwrap().unwrap();
        assert_eq!(run.stale_secs, 600);
        assert!(run.on_done.is_none() && !run.tmux_inline);
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn file_modes() {
        let dir =
            std::env::temp_dir().join(format!("nebula-test-{}", crate::util::new_token().unwrap()));
        let home = dir.join("nebula");
        let conn = open_in(&home).unwrap();
        set_kv(&conn, "k", "v").unwrap();
        assert_eq!(get_kv(&conn, "k").unwrap().as_deref(), Some("v"));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&home.join("nebula.db")), 0o600);
        assert_eq!(mode(&home.join("nebula.db-wal")), 0o600);
        drop(conn);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
