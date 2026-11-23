//! Generation of the POSIX `sh` wrapper served at `GET /r/<token>`.
//!
//! The wrapper must run under BusyBox `sh` with only BusyBox `wget`, so it
//! uses nothing beyond: mktemp, cat, tail -c, head -c, wc -c, sleep, rm,
//! hostname/uname, id, and optionally sha256sum.

use crate::util::{hex, random_bytes, sha256_hex};
use std::borrow::Cow;

/// Largest output chunk the wrapper posts in one request.
pub const CHUNK_BYTES: usize = 256 * 1024;
/// The wrapper halves its chunk after a failed post, down to this size.
pub const MIN_CHUNK_BYTES: usize = 16 * 1024;

pub struct WrapperParams<'a> {
    /// `42`, or `42.2` for an execution of a multi-use run (digits and `.` only).
    pub run_id: &'a str,
    /// `<scheme>://<host>/r/<token>`: no trailing slash, no shell metacharacters.
    pub base: &'a str,
    pub script: &'a [u8],
    pub echo: bool,
}

/// What the heredoc will write to disk: the script, with a final newline
/// added when it lacks one (a heredoc cannot express a missing one).
pub fn heredoc_payload(script: &[u8]) -> Cow<'_, [u8]> {
    if script.ends_with(b"\n") {
        Cow::Borrowed(script)
    } else {
        let mut v = script.to_vec();
        v.push(b'\n');
        Cow::Owned(v)
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Pick a heredoc delimiter that does not occur anywhere in the script.
/// `candidates` yields random suffixes; it is a parameter so that collision
/// handling can be tested deterministically.
pub fn pick_delimiter(script: &[u8], mut candidates: impl FnMut() -> String) -> String {
    loop {
        let delim = format!("NEBULA_EOF_{}", candidates());
        if !contains(script, delim.as_bytes()) {
            return delim;
        }
    }
}

fn random_suffix() -> String {
    // Falls back to a time-derived value if /dev/urandom is unreadable; the
    // collision loop in `pick_delimiter` keeps the guarantee either way.
    match random_bytes(12) {
        Ok(b) => hex(&b),
        Err(_) => {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            format!("{:x}_{n}", crate::util::now_ms())
        }
    }
}

/// True when `base` is safe to embed inside single quotes in the wrapper.
pub fn base_is_safe(base: &str) -> bool {
    !base.is_empty()
        && base
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/:.-_[]".contains(&b))
}

pub fn generate(p: &WrapperParams<'_>) -> Vec<u8> {
    generate_with(p, random_suffix)
}

pub fn generate_with(p: &WrapperParams<'_>, candidates: impl FnMut() -> String) -> Vec<u8> {
    assert!(base_is_safe(p.base), "unsafe base url");
    assert!(
        !p.run_id.is_empty() && p.run_id.bytes().all(|b| b.is_ascii_digit() || b == b'.'),
        "bad run label"
    );
    let payload = heredoc_payload(p.script);
    let delim = pick_delimiter(&payload, candidates);
    let sha = sha256_hex(&payload);

    let mut out = Vec::with_capacity(payload.len() + 8192);
    out.extend_from_slice(
        HEAD.replace("@RUN@", p.run_id)
            .replace("@BASE@", p.base)
            .replace("@SHA@", &sha)
            .replace("@ECHO@", if p.echo { "1" } else { "0" })
            .replace("@DELIM@", &delim)
            .as_bytes(),
    );
    out.extend_from_slice(&payload);
    out.extend_from_slice(delim.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(
        TAIL.replace("@CHUNK@", &CHUNK_BYTES.to_string())
            .replace("@MINCHUNK@", &MIN_CHUNK_BYTES.to_string())
            .as_bytes(),
    );
    out
}

// Everything is inside nebula_main(), which is only called on the very last
// line: a download cut off anywhere leaves an incomplete function definition
// (a syntax error) or an uncalled function, so nothing executes.
const HEAD: &str = r#"#!/bin/sh
# Nebula run @RUN@ - generated wrapper, single use.
nebula_main() {
NB_RUN=@RUN@
NB_BASE='@BASE@'
NB_SHA='@SHA@'
NB_ECHO=@ECHO@

nb_http=
case "${NEBULA_HTTP:-}" in
    curl|wget) nb_http=$NEBULA_HTTP ;;
esac
if [ -z "$nb_http" ]; then
    if command -v curl >/dev/null 2>&1; then
        nb_http=curl
    elif command -v wget >/dev/null 2>&1; then
        nb_http=wget
    else
        echo "nebula: neither curl nor wget found, nothing was run" >&2
        return 1
    fi
fi

nb_dir=$(mktemp -d "${TMPDIR:-/tmp}/nebula.XXXXXX") || {
    echo "nebula: mktemp failed, nothing was run" >&2
    return 1
}
nb_keep=0
nb_cleanup() {
    if [ "$nb_keep" = 1 ]; then
        rm -f "$nb_dir/script" "$nb_dir/chunk" "$nb_dir/show" "$nb_dir/msg" \
            "$nb_dir/resp" "$nb_dir/code" "$nb_dir/code.tmp" "$nb_dir/delivered" \
            "$nb_dir/pid"
    else
        rm -rf "$nb_dir"
    fi
}
trap nb_cleanup EXIT
# Ctrl-C / hangup must reach the script, not kill the wrapper before it reports.
trap : INT HUP TERM

cat >"$nb_dir/script" <<'@DELIM@'
"#;

const TAIL: &str = r#"
if command -v sha256sum >/dev/null 2>&1; then
    nb_got=$(sha256sum "$nb_dir/script")
    nb_got=${nb_got%% *}
else
    nb_got=$NB_SHA
fi

# GNU wget retries on its own (20 times by default); we do the retrying.
# BusyBox wget has no such option and no such behaviour.
nb_wget_opts=
if [ "$nb_http" = wget ] && wget --help 2>&1 | grep -q -e '--tries'; then
    nb_wget_opts='-t 1'
fi
# nb_post1 PATH FILE: one POST of FILE, response body to $nb_dir/resp
nb_maxtime=30
nb_post1() {
    if [ "$nb_http" = curl ]; then
        curl -fsS --connect-timeout 15 -m "$nb_maxtime" -X POST \
            -H 'Content-Type: application/octet-stream' \
            --data-binary "@$2" "$NB_BASE/$1"
    else
        wget -q $nb_wget_opts -T 60 -O - --header 'Content-Type: application/octet-stream' \
            --post-file "$2" "$NB_BASE/$1"
    fi
}
# nb_post PATH FILE TRIES
nb_post() {
    nb_try=0
    while :; do
        nb_post1 "$1" "$2" >"$nb_dir/resp" 2>/dev/null && return 0
        nb_try=$((nb_try + 1))
        [ "$nb_try" -ge "$3" ] && return 1
        sleep "$nb_try"
    done
}
nb_size() {
    nb_sz=$(wc -c <"$1" 2>/dev/null) || nb_sz=0
    echo $((nb_sz + 0))
}
# Output goes up in chunks of $nb_chunk bytes; a failed post halves it (down
# to @MINCHUNK@), so a slow link still makes progress.
nb_chunk=@CHUNK@
if [ "$(printf abc | head -c 2 2>/dev/null)" = ab ]; then
    nb_head() { head -c "$nb_chunk"; }
else
    nb_head() { cat; }
fi

nb_host=$(hostname 2>/dev/null) || nb_host=$(uname -n 2>/dev/null) || nb_host=unknown
nb_user=$(id -un 2>/dev/null) || nb_user=${USER:-unknown}
nb_os=$(uname -srm 2>/dev/null) || nb_os=
printf 'host=%s\nuser=%s\nos=%s\n' "$nb_host" "$nb_user" "$nb_os" >"$nb_dir/msg"
if ! nb_post started "$nb_dir/msg" 3; then
    echo "nebula: cannot reach the nebula server, nothing was run" >&2
    return 1
fi
echo "nebula: run $NB_RUN started" >&2

: >"$nb_dir/out"
nb_code=
if [ "$nb_got" != "$NB_SHA" ]; then
    echo "nebula: script hash mismatch (download damaged?), refusing to run" >&2
    echo "nebula: script hash mismatch on the far side, script was NOT run" >"$nb_dir/out"
    nb_code=126
fi

# Background poster: sends new output every ~2 s, a heartbeat when idle,
# and the remainder plus the exit code at the end. If the wrapper shell
# itself disappears (killed), it stops the script, reports exit 137 and
# cleans up, instead of heartbeating forever.
nb_main=$$
(
    trap - EXIT
    # must survive Ctrl-C and a closed terminal to deliver the result
    trap '' INT HUP TERM
    nb_off=0
    nb_shown=0
    nb_idle=0
    nb_full=0
    nb_tick=0
    nb_show() {
        [ "$NB_ECHO" = 1 ] || return 0
        tail -c "+$((nb_shown + 1))" "$nb_dir/out" >"$nb_dir/show" 2>/dev/null
        nb_n=$(nb_size "$nb_dir/show")
        [ "$nb_n" -gt 0 ] || return 0
        cat "$nb_dir/show"
        nb_shown=$((nb_shown + nb_n))
    }
    # nb_flush FAILS: post everything not yet delivered; give up after FAILS
    # failed posts in a row (each failure halves the chunk size first)
    nb_flush() {
        nb_fails=0
        while [ "$nb_full" = 0 ]; do
            tail -c "+$((nb_off + 1))" "$nb_dir/out" 2>/dev/null | nb_head >"$nb_dir/chunk"
            nb_n=$(nb_size "$nb_dir/chunk")
            [ "$nb_n" -gt 0 ] || return 0
            if ! nb_post1 "out?off=$nb_off" "$nb_dir/chunk" >"$nb_dir/resp" 2>/dev/null; then
                nb_fails=$((nb_fails + 1))
                if [ "$nb_chunk" -gt @MINCHUNK@ ]; then
                    nb_chunk=$((nb_chunk / 2))
                    [ "$nb_chunk" -lt @MINCHUNK@ ] && nb_chunk=@MINCHUNK@
                fi
                [ "$nb_fails" -ge "$1" ] && return 1
                sleep "$nb_fails"
                continue
            fi
            nb_fails=0
            nb_off=$((nb_off + nb_n))
            nb_idle=0
            nb_r=
            read -r nb_r <"$nb_dir/resp" || :
            [ "$nb_r" = full ] && nb_full=1
        done
        return 0
    }
    nb_orphan=0
    while :; do
        if [ ! -f "$nb_dir/code" ] && ! kill -0 "$nb_main" 2>/dev/null; then
            nb_orphan=1
            nb_spid=
            read -r nb_spid <"$nb_dir/pid" 2>/dev/null || :
            if [ -n "$nb_spid" ] && kill "$nb_spid" 2>/dev/null; then
                # a script that traps TERM gets 5 s, then KILL
                nb_w=0
                while [ "$nb_w" -lt 5 ] && kill -0 "$nb_spid" 2>/dev/null; do
                    sleep 1
                    nb_w=$((nb_w + 1))
                done
                kill -9 "$nb_spid" 2>/dev/null
            fi
            sleep 1
            echo "nebula: the wrapper shell was killed before the script finished" >>"$nb_dir/out"
            echo 137 >"$nb_dir/code"
        fi
        if [ -f "$nb_dir/code" ]; then
            nb_c=
            read -r nb_c <"$nb_dir/code" || :
            nb_show
            if nb_flush 5; then
                printf 'code=%s\nbytes=%s\n' "$nb_c" "$(nb_size "$nb_dir/out")" >"$nb_dir/msg"
                nb_post done "$nb_dir/msg" 5 && : >"$nb_dir/delivered"
            fi
            if ! kill -0 "$nb_main" 2>/dev/null; then
                # nobody is left to clean up after us
                if [ -f "$nb_dir/delivered" ]; then
                    rm -rf "$nb_dir"
                else
                    rm -f "$nb_dir/script" "$nb_dir/chunk" "$nb_dir/show" "$nb_dir/msg" \
                        "$nb_dir/resp" "$nb_dir/code" "$nb_dir/pid"
                    echo "nebula: run $NB_RUN: result could NOT be delivered, output kept in $nb_dir/out" >&2
                fi
            fi
            exit 0
        fi
        nb_tick=$((nb_tick + 1))
        nb_show
        if [ $((nb_tick % 2)) = 0 ]; then
            nb_flush 1 || :
            nb_idle=$((nb_idle + 1))
            if [ "$nb_idle" -ge 5 ]; then
                echo hb >"$nb_dir/msg"
                nb_post hb "$nb_dir/msg" 1 || :
                nb_idle=0
            fi
        fi
        sleep 1
    done
) &
nb_poster=$!

if [ -z "$nb_code" ]; then
    # Foreground, same process group and controlling terminal as the wrapper:
    # the script may read a confirmation from /dev/tty. stdin is /dev/null.
    # (The inner sh records its pid and then becomes the script's shell.)
    sh -c 'echo "$$" >"$1" && exec sh "$2"' nebula "$nb_dir/pid" "$nb_dir/script" \
        >>"$nb_dir/out" 2>&1 </dev/null
    nb_code=$?
fi
echo "$nb_code" >"$nb_dir/code.tmp" && mv "$nb_dir/code.tmp" "$nb_dir/code"
while kill -0 "$nb_poster" 2>/dev/null; do
    wait "$nb_poster"
done

nb_total=$(nb_size "$nb_dir/out")
if [ -f "$nb_dir/delivered" ]; then
    echo "nebula: run $NB_RUN done exit=$nb_code bytes=$nb_total" >&2
else
    nb_keep=1
    echo "nebula: run $NB_RUN done exit=$nb_code bytes=$nb_total - result could NOT be delivered" >&2
    echo "nebula: output kept in $nb_dir/out" >&2
fi
return "$nb_code"
}
nebula_main
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn gen_default(script: &[u8]) -> String {
        String::from_utf8(generate(&WrapperParams {
            run_id: "7",
            base: "http://127.0.0.1:7411/r/abcDEF_-",
            script,
            echo: false,
        }))
        .unwrap()
    }

    #[test]
    fn delimiter_collision_is_avoided() {
        let script = b"echo NEBULA_EOF_aaaa\ncat <<X\nNEBULA_EOF_bbbb\nX\n";
        let mut seq = vec!["cccc", "bbbb", "aaaa"];
        let d = pick_delimiter(script, || seq.pop().unwrap().to_string());
        assert_eq!(d, "NEBULA_EOF_cccc");
        assert!(seq.is_empty(), "both colliding candidates must be tried");
    }

    #[test]
    fn delimiter_collision_in_generated_wrapper() {
        let script = b"echo NEBULA_EOF_one\n";
        let mut seq = vec!["two", "one"];
        let w = generate_with(
            &WrapperParams {
                run_id: "1",
                base: "http://127.0.0.1:1/r/t",
                script,
                echo: false,
            },
            || seq.pop().unwrap().to_string(),
        );
        let w = String::from_utf8(w).unwrap();
        assert!(w.contains("<<'NEBULA_EOF_two'\necho NEBULA_EOF_one\nNEBULA_EOF_two\n"));
        assert!(!w.contains("<<'NEBULA_EOF_one'"));
    }

    #[test]
    fn random_delimiter_never_occurs_in_script() {
        for _ in 0..50 {
            let w = gen_default(b"echo hello\n");
            let start = w.find("<<'").unwrap() + 3;
            let delim = &w[start..start + w[start..].find('\'').unwrap()];
            assert!(delim.starts_with("NEBULA_EOF_"));
            // opening + closing occurrence only
            assert_eq!(w.matches(delim).count(), 2);
        }
    }

    #[test]
    fn structure() {
        let w = gen_default(b"echo 'it''s' \"$HOME\" `x` \\\n");
        assert!(w.starts_with("#!/bin/sh\n"));
        assert!(w.ends_with("\nnebula_main\n"));
        // script embedded byte for byte
        assert!(w.contains("\necho 'it''s' \"$HOME\" `x` \\\nNEBULA_EOF_"));
        assert!(w.contains("NB_BASE='http://127.0.0.1:7411/r/abcDEF_-'\n"));
        assert!(w.contains("NB_ECHO=0\n"));
        for ph in ["@RUN@", "@BASE@", "@SHA@", "@ECHO@", "@DELIM@", "@CHUNK@"] {
            assert!(!w.contains(ph), "placeholder {ph} left in wrapper");
        }
        // exactly one top-level call of the function
        assert_eq!(w.matches("\nnebula_main\n").count(), 1);
    }

    #[test]
    fn missing_final_newline_is_hashed_as_written() {
        let w = gen_default(b"echo no-newline");
        assert!(w.contains(&format!("NB_SHA='{}'", sha256_hex(b"echo no-newline\n"))));
        let w = gen_default(b"echo newline\n");
        assert!(w.contains(&format!("NB_SHA='{}'", sha256_hex(b"echo newline\n"))));
    }

    #[test]
    fn echo_flag() {
        let w = String::from_utf8(generate(&WrapperParams {
            run_id: "7",
            base: "https://a-b.trycloudflare.com/r/tok",
            script: b"true\n",
            echo: true,
        }))
        .unwrap();
        assert!(w.contains("NB_ECHO=1\n"));
    }

    #[test]
    fn base_safety() {
        assert!(base_is_safe("https://a-b.trycloudflare.com/r/abc_-9"));
        assert!(base_is_safe("http://127.0.0.1:7411/r/x"));
        assert!(!base_is_safe("http://x/r/a'b"));
        assert!(!base_is_safe("http://x/r/a b"));
        assert!(!base_is_safe("http://x/$(id)"));
        assert!(!base_is_safe(""));
    }
}
