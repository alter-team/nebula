#!/usr/bin/env bash
# End-to-end test of nebula. Starts its own daemons on spare ports with a
# temporary NEBULA_HOME and NEBULA_NO_TUNNEL=1; never touches the real data
# directory, systemd or the network.
#
#   tests/e2e.sh [path/to/nebula]        (default: target/debug/nebula)
#   E2E_PORT=17411 E2E_PORT2=17412 tests/e2e.sh
set -u

NEBULA=${1:-${NEBULA_BIN:-target/debug/nebula}}
NEBULA=$(readlink -f "$NEBULA")
PORT=${E2E_PORT:-17411}
PORT2=${E2E_PORT2:-$((PORT + 1))}
[ -x "$NEBULA" ] || { echo "no nebula binary at $NEBULA" >&2; exit 2; }

WORK=$(mktemp -d "${TMPDIR:-/tmp}/nebula-e2e.XXXXXX")
export NEBULA_HOME="$WORK/home"
export NEBULA_PORT="$PORT"
export NEBULA_NO_TUNNEL=1
unset NEBULA_TUNNEL_URL NEBULA_HTTP NEBULA_CLOUDFLARED TMUX TMUX_PANE
export TMPDIR="$WORK/tmp"
mkdir -p "$TMPDIR"
BASE="http://127.0.0.1:$PORT"
DB="$NEBULA_HOME/nebula.db"

PASS=0
FAIL=0
DAEMON_PID=
DAEMON2_PID=

cleanup() {
    [ -n "$DAEMON_PID" ] && kill "$DAEMON_PID" 2>/dev/null
    [ -n "$DAEMON2_PID" ] && kill "$DAEMON2_PID" 2>/dev/null
    wait 2>/dev/null
    if [ "$FAIL" = 0 ] && [ -z "${E2E_KEEP:-}" ]; then
        rm -rf "$WORK"
    else
        echo "work dir kept: $WORK"
    fi
}
trap cleanup EXIT

ok() { PASS=$((PASS + 1)); echo "  ok    $1"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL  $1"; [ $# -gt 1 ] && echo "        $2"; }
check() { # check DESC CMD...
    local d=$1; shift
    if "$@" >/dev/null 2>&1; then ok "$d"; else bad "$d" "command failed: $*"; fi
}
eq() { # eq DESC EXPECTED ACTUAL
    if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "expected [$2] got [$3]"; fi
}
has() { # has DESC FILE PATTERN
    if grep -qE -- "$3" "$2"; then ok "$1"; else bad "$1" "[$3] not in $2: $(head -c 600 "$2")"; fi
}
hasnt() {
    if grep -qE -- "$3" "$2"; then bad "$1" "[$3] unexpectedly in $2"; else ok "$1"; fi
}
section() { echo; echo "== $1"; }
http_code() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
sql() { sqlite3 -cmd '.timeout 5000' "${SQLDB:-$DB}" "$1"; }

wait_for() { # wait_for SECONDS CMD...
    local n=$(( $1 * 10 )); shift
    while [ "$n" -gt 0 ]; do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.1
        n=$((n - 1))
    done
    return 1
}

start_daemon() {
    "$NEBULA" daemon 2>>"$WORK/daemon.log" &
    DAEMON_PID=$!
    wait_for 10 curl -s -o /dev/null "$BASE/" || { echo "daemon did not start" >&2; cat "$WORK/daemon.log" >&2; exit 2; }
}

# reg NAME FILE [run options...]: start `nebula run` in the background.
# Sets RUN_PID, RUN_OUT (its combined output), RUN_ID, RUN_URL.
reg() {
    local name=$1 file=$2; shift 2
    RUN_OUT="$WORK/run-$name.out"
    "$NEBULA" run --local --no-clipboard --name "$name" "$@" "$file" >"$RUN_OUT" 2>&1 &
    RUN_PID=$!
    wait_for 10 grep -q 'waiting for run' "$RUN_OUT" || { bad "register $name" "$(cat "$RUN_OUT")"; return 1; }
    RUN_ID=$(sed -n 's/^nebula: run \([0-9]*\) .*registered.*/\1/p' "$RUN_OUT" | head -n1)
    RUN_URL=$(grep -o "http://127.0.0.1:$PORT/r/[A-Za-z0-9_-]*" "$RUN_OUT" | head -n1)
}
finish() { wait "$RUN_PID"; RUN_RC=$?; }

mkscript() { # mkscript NAME  (body on stdin)
    cat >"$WORK/$1.sh"
    echo "$WORK/$1.sh"
}

# ------------------------------------------------------------------------
section "cli basics"
check "nebula --help" "$NEBULA" --help
check "nebula help" "$NEBULA" help
check "nebula run --help" "$NEBULA" run --help
check "nebula help run" "$NEBULA" help run
"$NEBULA" skill >"$WORK/skill.md"
eq "skill starts with frontmatter" "---" "$(head -n1 "$WORK/skill.md")"
has "skill has name" "$WORK/skill.md" '^name: nebula$'
eq "skill.md alias prints the same" "$(sha256sum <"$WORK/skill.md")" "$("$NEBULA" skill.md | sha256sum)"
"$NEBULA" run x.sh >/dev/null 2>&1; eq "run without target is a usage error" 2 $?
"$NEBULA" service install --dry-run >"$WORK/svc.out" 2>&1
has "service install --dry-run shows ExecStart" "$WORK/svc.out" "^ExecStart=\"$NEBULA\" daemon$"
has "service install --dry-run shows Restart=always" "$WORK/svc.out" '^Restart=always$'

section "no daemon"
S=$(mkscript hello <<'EOF'
echo hello
EOF
)
"$NEBULA" run --remote --no-clipboard "$S" >"$WORK/nodaemon.out" 2>&1
eq "run --remote without daemon exits 122" 122 $?
"$NEBULA" status >"$WORK/status0.out" 2>&1
has "status says not running" "$WORK/status0.out" 'daemon: +not running'

section "daemon and routes"
start_daemon
"$NEBULA" status >"$WORK/status.out" 2>&1
has "status says running" "$WORK/status.out" "daemon: +running .*port $PORT"
has "status says tunnel disabled" "$WORK/status.out" 'tunnel: +disabled'
eq "db file mode 600" 600 "$(stat -c %a "$DB")"
eq "data dir mode 700" 700 "$(stat -c %a "$NEBULA_HOME")"
eq "listener is on 127.0.0.1 only" "" "$(ss -Hltn "sport = :$PORT" 2>/dev/null | grep -v '127.0.0.1' || true)"
for path in / /r /r/ /status /admin /api /list /favicon.ico /r/short /r/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa \
    /r/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/out /r/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/x; do
    eq "GET $path is 404" 404 "$(http_code "$BASE$path")"
done
eq "404 body is empty" 0 "$(curl -s "$BASE/status" | wc -c)"
eq "POST unknown token is 404" 404 "$(http_code -X POST --data x "$BASE/r/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/out?off=0")"
eq "PUT is 404" 404 "$(http_code -X PUT --data x "$BASE/r/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")"
"$NEBULA" run --remote --no-clipboard "$S" >"$WORK/notunnel.out" 2>&1
eq "run --remote with tunnel disabled exits 122" 122 $?

section "local run via curl | sh (exit 0)"
S=$(mkscript ok <<'EOF'
set -eu
echo "hello from $(id -un)"
echo "to stderr" >&2
EOF
)
reg ok "$S"
has "one-liner 'nebula go' is printed" "$RUN_OUT" "^    nebula go $RUN_ID\$"
[ -n "$RUN_URL" ] && ok "curl alternative is printed" || bad "curl alternative is printed"
curl -fsS "$RUN_URL" | sh >"$WORK/far.out" 2>"$WORK/far.err"
eq "far side exit code 0" 0 $?
finish
eq "nebula run exit code 0" 0 "$RUN_RC"
has "output captured" "$RUN_OUT" "^hello from $(id -un)\$"
has "stderr captured" "$RUN_OUT" '^to stderr$'
has "fetched reported" "$RUN_OUT" "^nebula: run $RUN_ID fetched"
has "started reported" "$RUN_OUT" "^nebula: run $RUN_ID started on host $(hostname) as $(id -un)\$"
has "summary line" "$RUN_OUT" "^nebula: run $RUN_ID finished exit=0 bytes=[0-9]+ host=$(hostname)\$"
eq "far side shows no output by default" 0 "$(wc -c <"$WORK/far.out")"
has "far side status: started" "$WORK/far.err" "^nebula: run $RUN_ID started\$"
has "far side status: done" "$WORK/far.err" "^nebula: run $RUN_ID done exit=0 bytes=[0-9]+\$"
eq "second GET is 404 (single use)" 404 "$(http_code "$RUN_URL")"
eq "POST after done is 404" 404 "$(http_code -X POST --data x "$RUN_URL/out?off=0")"
eq "POST done after done is 404" 404 "$(http_code -X POST --data code=0 "$RUN_URL/done")"
eq "no temp files left" 0 "$(find "$TMPDIR" -mindepth 1 | wc -l)"
eq "nebula output" "$(printf 'hello from %s\nto stderr\n' "$(id -un)")" "$("$NEBULA" output "$RUN_ID")"
"$NEBULA" show "$RUN_ID" >"$WORK/show.out"
has "show has state" "$WORK/show.out" '^state: +done$'
has "show has script body" "$WORK/show.out" '^echo "to stderr" >&2$'
has "show has the far side's OS" "$WORK/show.out" "^os: +$(uname -srm)\$"
has "show has the fetching address" "$WORK/show.out" '^fetched from: +127\.0\.0\.1$'
has "show has the fetching client" "$WORK/show.out" '^fetched with: +curl/[0-9]'
"$NEBULA" list >"$WORK/list.out"
has "list shows the run" "$WORK/list.out" "^ +$RUN_ID +local +done +0 "

section "local run via wget -qO- | sh (exit 7, callbacks via wget)"
S=$(mkscript seven <<'EOF'
echo before
exit 7
EOF
)
reg seven "$S"
wget -qO- "$RUN_URL" | NEBULA_HTTP=wget sh >"$WORK/far.out" 2>"$WORK/far.err"
eq "far side exit code 7" 7 $?
finish
eq "nebula run exit code 7" 7 "$RUN_RC"
has "output captured" "$RUN_OUT" '^before$'
has "summary line" "$RUN_OUT" "^nebula: run $RUN_ID finished exit=7 bytes=7 "
"$NEBULA" show "$RUN_ID" >"$WORK/show.out"
has "wget: fetching client recorded" "$WORK/show.out" '^fetched with: +[Ww]get'
has "wget: OS recorded" "$WORK/show.out" "^os: +$(uname -srm)\$"
eq "second GET is 404 (single use)" 404 "$(http_code "$RUN_URL")"

if command -v busybox >/dev/null 2>&1; then
    section "busybox sh + busybox wget only"
    BB="$WORK/bb"
    mkdir -p "$BB"
    busybox --install -s "$BB"
    S=$(mkscript bb <<'EOF'
set -eu
echo "shell: $(readlink /proc/$$/exe)"
i=0
while [ "$i" -lt 3 ]; do echo "line $i"; i=$((i + 1)); done
command -v curl >/dev/null 2>&1 && echo "curl present" || echo "no curl"
exit 4
EOF
)
    reg bb "$S"
    env -i PATH="$BB" TMPDIR="$TMPDIR" "$BB/sh" -c "wget -qO- '$RUN_URL' | sh" >"$WORK/far.out" 2>"$WORK/far.err"
    eq "busybox far side exit code 4" 4 $?
    finish
    eq "nebula run exit code 4" 4 "$RUN_RC"
    has "ran under busybox" "$RUN_OUT" '^shell: .*busybox$'
    has "no curl in that environment" "$RUN_OUT" '^no curl$'
    has "output captured" "$RUN_OUT" '^line 2$'
    has "far side done line" "$WORK/far.err" "^nebula: run $RUN_ID done exit=4 "
    "$NEBULA" show "$RUN_ID" >"$WORK/show.out"
    has "busybox: OS recorded (busybox uname)" "$WORK/show.out" "^os: +$(uname -srm)\$"
    has "busybox: fetching client recorded" "$WORK/show.out" '^fetched with: +[Ww]get'
    eq "no temp files left" 0 "$(find "$TMPDIR" -mindepth 1 | wc -l)"

    # multi-chunk under busybox too
    S=$(mkscript bbslow <<'EOF'
echo one; sleep 3; echo two
EOF
)
    reg bbslow "$S"
    env -i PATH="$BB" TMPDIR="$TMPDIR" "$BB/sh" -c "wget -qO- '$RUN_URL' | sh" >/dev/null 2>&1
    finish
    eq "busybox multi-chunk run exit 0" 0 "$RUN_RC"
    c=$(sql "select count(*) from chunks where run_id=$RUN_ID")
    [ "$c" -ge 2 ] && ok "busybox: output arrived in $c chunks" || bad "busybox: more than one chunk" "got $c"
else
    section "busybox"
    echo "  SKIP  busybox is not installed"
fi

section "TTL expiry"
reg ttl "$S" --ttl 1
sleep 2.5
eq "GET after TTL is 404" 404 "$(http_code "$RUN_URL")"
finish
eq "nebula run exits 121 (expired)" 121 "$RUN_RC"
has "expiry is reported" "$RUN_OUT" "^nebula: run $RUN_ID expired"
"$NEBULA" go "$RUN_ID" >/dev/null 2>&1
eq "go on an expired run is refused" 125 $?

section "multi-second script, output in several chunks"
S=$(mkscript slow <<'EOF'
echo first
sleep 3
echo second
sleep 3
echo third
EOF
)
reg slow "$S"
t0=$(date +%s)
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1 &
far=$!
# the first line must show up in `nebula run` long before the script ends
wait_for 5 grep -q '^first$' "$RUN_OUT" && ok "first chunk streamed while script still runs" || bad "first chunk streamed while script still runs"
hasnt "third not there yet" "$RUN_OUT" '^third$'
wait "$far"
finish
eq "exit code 0" 0 "$RUN_RC"
c=$(sql "select count(*) from chunks where run_id=$RUN_ID")
[ "$c" -ge 2 ] && ok "output arrived in $c chunks" || bad "output arrived in more than one chunk" "got $c"
eq "output complete and in order" "$(printf 'first\nsecond\nthird\n')" "$("$NEBULA" output "$RUN_ID")"
[ $(( $(date +%s) - t0 )) -ge 6 ] && ok "took at least 6 s" || bad "took at least 6 s"

section "nebula go"
S=$(mkscript go <<'EOF'
echo "go stdout"
echo "go stderr" >&2
read -r line || line=EOF
echo "stdin gave: $line"
exit 5
EOF
)
reg go "$S"
echo "typed-by-human" | "$NEBULA" go "$RUN_ID" >"$WORK/go.out" 2>"$WORK/go.err"
eq "go exit code 5" 5 $?
finish
eq "nebula run exit code 5" 5 "$RUN_RC"
has "go shows output live in its terminal" "$WORK/go.out" '^go stdout$'
has "go shows stderr too" "$WORK/go.out" '^go stderr$'
has "go passes the terminal's stdin" "$WORK/go.out" '^stdin gave: typed-by-human$'
has "go output recorded" "$RUN_OUT" '^stdin gave: typed-by-human$'
has "summary line" "$RUN_OUT" "^nebula: run $RUN_ID finished exit=5 bytes=[0-9]+ host=$(hostname)\$"
"$NEBULA" show "$RUN_ID" >"$WORK/show.out"
has "go: OS recorded" "$WORK/show.out" "^os: +$(uname -sr) "
has "go: no terminal on a pipe" "$WORK/show.out" '^terminal: +-$'
hasnt "go: no fetch address" "$WORK/show.out" '^fetched '
hasnt "no 'fetched' for a go run" "$RUN_OUT" 'fetched'
eq "recorded via go" go "$(sql "select via from runs where id=$RUN_ID")"
"$NEBULA" go "$RUN_ID" >/dev/null 2>"$WORK/go2.err"
eq "second go is refused" 125 $?
has "refusal explains" "$WORK/go2.err" 'only a pending run'
eq "GET for a run executed by go is 404" 404 "$(http_code "$RUN_URL")"
"$NEBULA" go 99999 >/dev/null 2>&1
eq "go on unknown run is refused" 125 $?
eq "go leaves no temp script" 0 "$(find "${XDG_RUNTIME_DIR:-/nonexistent}" "$TMPDIR" -maxdepth 1 -name 'nebula-go-*' 2>/dev/null | wc -l)"

if command -v script >/dev/null 2>&1; then
    section "nebula go in a real terminal (pty): /dev/tty prompt, Ctrl-C"
    S=$(mkscript gotty <<'EOF'
printf 'password: ' >/dev/tty
read -r pw </dev/tty
echo "prompt answered with ${#pw} chars"
[ -t 0 ] && echo "stdin is the terminal"
EOF
)
    reg gotty "$S"
    (sleep 1.5; echo hunter2) | script -qec "'$NEBULA' go $RUN_ID" /dev/null >"$WORK/tty.out" 2>&1
    finish
    eq "go with a /dev/tty prompt exit 0" 0 "$RUN_RC"
    has "prompt was answered" "$RUN_OUT" '^prompt answered with 7 chars$'
    has "stdin is the terminal" "$RUN_OUT" '^stdin is the terminal$'
    has "prompt appeared on the terminal" "$WORK/tty.out" 'password: '
    S=$(mkscript gointr <<'EOF'
echo begin
sleep 30
echo never
EOF
)
    reg gointr "$S"
    (sleep 2; printf '\003') | script -qec "'$NEBULA' go $RUN_ID" /dev/null >"$WORK/tty.out" 2>&1
    finish
    eq "Ctrl-C during go is recorded as exit 130" 130 "$RUN_RC"
    has "output before the interrupt is kept" "$RUN_OUT" '^begin$'
    # cancel stops a go run
    reg gocancel "$S"
    "$NEBULA" go "$RUN_ID" >/dev/null 2>"$WORK/go.err" </dev/null &
    gopid=$!
    wait_for 5 grep -q '^begin$' "$RUN_OUT"
    "$NEBULA" cancel "$RUN_ID" >/dev/null
    wait "$gopid"
    eq "cancelled go run was terminated (143)" 143 $?
    finish
    eq "nebula run exits 123" 123 "$RUN_RC"
fi

section "nebula go without a daemon, multi-chunk"
kill "$DAEMON_PID"; wait "$DAEMON_PID" 2>/dev/null; DAEMON_PID=
S=$(mkscript go2 <<'EOF'
echo a; sleep 2; echo b
EOF
)
RUN_OUT="$WORK/run-go2.out"
"$NEBULA" run --local --no-clipboard "$S" >"$RUN_OUT" 2>&1 &
RUN_PID=$!
wait_for 10 grep -q 'waiting for run' "$RUN_OUT"
RUN_ID=$(sed -n 's/^nebula: run \([0-9]*\) .*registered.*/\1/p' "$RUN_OUT" | head -n1)
has "says only go works" "$RUN_OUT" 'daemon not running'
"$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
eq "go without daemon exit 0" 0 $?
finish
eq "nebula run exit code 0" 0 "$RUN_RC"
c=$(sql "select count(*) from chunks where run_id=$RUN_ID")
[ "$c" -ge 2 ] && ok "go recorded output in $c chunks" || bad "go recorded more than one chunk" "got $c"
start_daemon

section "awkward scripts"
S="$WORK/awkward.sh"
cat >"$S" <<'AWKWARD_EOF'
x='single "$HOME" `not run` $(not run) \ '
printf '%s\n' "$x" "it's"
cat <<EOF
NEBULA_EOF_inner $((1+2))
EOF
cat <<'NEBULA_EOF'
literal $x \\ \t
NEBULA_EOF
printf 'tab\there\n'
AWKWARD_EOF
printf 'echo; echo last-line-without-newline' >>"$S"
sh "$S" >"$WORK/awkward.expected" 2>&1
reg awkward "$S"
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
eq "exit 0" 0 "$RUN_RC"
"$NEBULA" output "$RUN_ID" >"$WORK/awkward.actual"
check "output identical to running the script directly" cmp "$WORK/awkward.expected" "$WORK/awkward.actual"

section "script from stdin"
RUN_OUT="$WORK/run-stdin.out"
echo 'echo from-stdin' | "$NEBULA" run --local --no-clipboard - >"$RUN_OUT" 2>&1 &
RUN_PID=$!
wait_for 10 grep -q 'waiting for run' "$RUN_OUT"
RUN_URL=$(grep -o "http://127.0.0.1:$PORT/r/[A-Za-z0-9_-]*" "$RUN_OUT" | head -n1)
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
eq "exit 0" 0 "$RUN_RC"
has "output" "$RUN_OUT" '^from-stdin$'
: >"$WORK/empty.sh"
"$NEBULA" run --local --no-clipboard "$WORK/empty.sh" >/dev/null 2>&1
eq "empty script is refused (125)" 125 $?
"$NEBULA" run --local --no-clipboard "$WORK/does-not-exist.sh" >/dev/null 2>&1
eq "missing script file is refused (125)" 125 $?

section "large output, --max-print, binary safety"
S=$(mkscript big <<'EOF'
head -c 3000000 /dev/urandom | od -An -v -tx1 | head -c 3000000
EOF
)
reg big "$S" --max-print 1000
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
eq "exit 0" 0 "$RUN_RC"
has "summary bytes=3000000" "$RUN_OUT" "^nebula: run $RUN_ID finished exit=0 bytes=3000000 "
has "truncation notice" "$RUN_OUT" "output truncated after 1000 bytes.*nebula output $RUN_ID"
[ "$(wc -c <"$RUN_OUT")" -lt 4000 ] && ok "printed output was limited" || bad "printed output was limited"
eq "nebula output returns everything" 3000000 "$("$NEBULA" output "$RUN_ID" | wc -c)"
c=$(sql "select count(*) from chunks where run_id=$RUN_ID")
[ "$c" -ge 3 ] && ok "3 MB went over in $c requests (1 MiB cap each)" || bad "chunking of large output" "got $c"
S=$(mkscript binary <<'EOF'
printf 'a\000b\377c\r\n'
EOF
)
reg binary "$S"
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
eq "binary output byte-exact" "$(printf 'a\000b\377c\r\n' | sha256sum)" "$("$NEBULA" output "$RUN_ID" | sha256sum)"

section "--echo"
S=$(mkscript echo <<'EOF'
echo visible-on-far-side
EOF
)
reg echo "$S" --echo
curl -fsS "$RUN_URL" | sh >"$WORK/far.out" 2>/dev/null
finish
has "far side shows the output with --echo" "$WORK/far.out" '^visible-on-far-side$'
has "and it is captured" "$RUN_OUT" '^visible-on-far-side$'

section "--delete-after"
reg delafter "$S" --delete-after
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
has "output printed" "$RUN_OUT" '^visible-on-far-side$'
has "summary mentions deletion" "$RUN_OUT" 'output-deleted=yes'
eq "chunks deleted" 0 "$(sql "select count(*) from chunks where run_id=$RUN_ID")"
eq "metadata row stays" done "$(sql "select state from runs where id=$RUN_ID")"
"$NEBULA" output "$RUN_ID" >/dev/null 2>&1
eq "nebula output reports deleted" 1 $?

section "timeout, wait, cancel, rm"
S=$(mkscript quick <<'EOF'
echo quick
EOF
)
reg tmo "$S" --timeout 1
finish
eq "nebula run exits 120 on timeout" 120 "$RUN_RC"
has "timeout message points at wait" "$RUN_OUT" "nebula wait $RUN_ID"
"$NEBULA" wait "$RUN_ID" >"$WORK/wait.out" 2>&1 &
wpid=$!
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
wait "$wpid"
eq "nebula wait exits with the script's code" 0 $?
has "wait prints the output" "$WORK/wait.out" '^quick$'
has "wait prints the summary" "$WORK/wait.out" "^nebula: run $RUN_ID finished exit=0 "
"$NEBULA" wait "$RUN_ID" >"$WORK/wait2.out" 2>&1
eq "wait on a finished run returns at once" 0 $?
has "and replays the output" "$WORK/wait2.out" '^quick$'
tid=$RUN_ID
reg cancel "$S"
"$NEBULA" rm "$RUN_ID" >/dev/null 2>&1
eq "rm refuses an open run" 1 $?
check "cancel" "$NEBULA" cancel "$RUN_ID"
finish
eq "nebula run exits 123 when cancelled" 123 "$RUN_RC"
eq "GET after cancel is 404" 404 "$(http_code "$RUN_URL")"
"$NEBULA" cancel "$RUN_ID" >/dev/null 2>&1
eq "second cancel exits 1" 1 $?
check "rm --output-only" "$NEBULA" rm "$tid" --output-only
eq "output gone" 0 "$(sql "select count(*) from chunks where run_id=$tid")"
eq "row kept" 1 "$(sql "select count(*) from runs where id=$tid")"
check "rm" "$NEBULA" rm "$tid"
eq "row gone" 0 "$(sql "select count(*) from runs where id=$tid")"
"$NEBULA" show "$tid" >/dev/null 2>&1
eq "show on removed run exits 125" 125 $?

section "raw protocol: idempotent offsets, caps"
reg raw "$S"
post() { curl -s -o "$WORK/resp" -w '%{http_code}' -X POST -H 'Content-Type: application/octet-stream' "${@:2}" "$RUN_URL/$1"; }
eq "POST before fetch is 404" 404 "$(post started --data-binary 'host=h')"
curl -fsS "$RUN_URL" >"$WORK/wrapper.sh"
eq "wrapper ends with the function call" "nebula_main" "$(tail -n1 "$WORK/wrapper.sh")"
check "wrapper is valid sh" sh -n "$WORK/wrapper.sh"
eq "started" 200 "$(post started --data-binary $'host=far-box\nuser=someone\n')"
eq "out off=0" 200 "$(post 'out?off=0' --data-binary 'hello ')"
eq "retry of off=0 is accepted" 200 "$(post 'out?off=0' --data-binary 'hello ')"
eq "overlapping retry is accepted" 200 "$(post 'out?off=3' --data-binary 'lo world')"
eq "gap is refused with 409" 409 "$(post 'out?off=500' --data-binary 'x')"
eq "gap response names the expected offset" "gap 11" "$(cat "$WORK/resp")"
eq "out without offset is 400" 400 "$(post out --data-binary 'x')"
eq "heartbeat" 200 "$(post hb --data-binary hb)"
head -c 200000 /dev/zero >"$WORK/trickle"
t0=$(date +%s)
eq "an upload trickling at 512 B/s is dropped (408)" 408 "$(post 'out?off=11' -H 'Expect:' --limit-rate 512 --data-binary "@$WORK/trickle")"
el=$(( $(date +%s) - t0 ))
[ "$el" -le 18 ] && ok "...after $el s, not 60" || bad "trickling upload held for $el s"
eq "nothing of it was stored" 11 "$(sql "select output_bytes from runs where id=$RUN_ID")"
head -c 9000000 /dev/zero >"$WORK/toobig"
eq "oversized out body is 413" 413 "$(post 'out?off=11' --data-binary "@$WORK/toobig")"
head -c 5000 /dev/zero | tr '\0' 'a' >"$WORK/toobig"
eq "oversized done body is 413" 413 "$(post done --data-binary "@$WORK/toobig")"
eq "bad exit code is 400" 400 "$(post done --data-binary 'code=banana')"
eq "GET of unknown sub-route is 404" 404 "$(http_code "$RUN_URL/out")"
eq "done" 200 "$(post done --data-binary $'code=9\nbytes=11\n')"
finish
eq "nebula run exit code 9" 9 "$RUN_RC"
eq "stored output has no duplicates" "hello world" "$("$NEBULA" output "$RUN_ID")"
has "host from the far side" "$RUN_OUT" "started on host far-box as someone"

section "truncated download executes nothing"
S=$(mkscript marker <<EOF
touch "$WORK/MARKER"
EOF
)
reg marker "$S"
curl -fsS "$RUN_URL" >"$WORK/wrapper.sh"
len=$(wc -c <"$WORK/wrapper.sh")
n=0
cut_at=1
while [ "$cut_at" -le $((len - 2)) ]; do
    head -c "$cut_at" "$WORK/wrapper.sh" | sh >/dev/null 2>&1
    n=$((n + 1))
    if [ "$cut_at" -lt $((len - 300)) ]; then cut_at=$((cut_at + 37)); else cut_at=$((cut_at + 1)); fi
done
[ ! -e "$WORK/MARKER" ] && ok "none of $n truncated prefixes ran the script" || bad "a truncated wrapper ran the script"
eq "truncated runs left no temp files" 0 "$(find "$TMPDIR" -mindepth 1 | wc -l)"
eq "run still only fetched" fetched "$(sql "select state from runs where id=$RUN_ID")"

section "hash mismatch refuses to run"
sed "s|^touch |touch /dev/null; touch |" "$WORK/wrapper.sh" >"$WORK/tampered.sh"
sh "$WORK/tampered.sh" >/dev/null 2>"$WORK/far.err"
eq "tampered wrapper exits 126" 126 $?
finish
eq "nebula run exit code 126" 126 "$RUN_RC"
has "far side refuses" "$WORK/far.err" 'hash mismatch'
has "agent is told" "$RUN_OUT" 'hash mismatch'
[ ! -e "$WORK/MARKER" ] && ok "tampered script did not run" || bad "tampered script ran"

section "/dev/tty confirmation on the far side"
if command -v script >/dev/null 2>&1; then
    S=$(mkscript tty <<'EOF'
printf 'Type yes to continue: ' >/dev/tty
read -r answer </dev/tty
[ "$answer" = yes ] || { echo "aborted ($answer)"; exit 1; }
echo confirmed
EOF
)
    reg tty "$S"
    (sleep 1.5; echo yes) | script -qec "curl -fsS '$RUN_URL' | sh" /dev/null >"$WORK/tty.out" 2>&1
    finish
    eq "script that read 'yes' from /dev/tty exit 0" 0 "$RUN_RC"
    has "confirmed" "$RUN_OUT" '^confirmed$'
    has "prompt appeared on the terminal" "$WORK/tty.out" 'Type yes to continue'
    reg tty2 "$S"
    (sleep 1.5; echo no) | script -qec "curl -fsS '$RUN_URL' | sh" /dev/null >"$WORK/tty.out" 2>&1
    finish
    eq "answer 'no' aborts with exit 1" 1 "$RUN_RC"
    has "aborted" "$RUN_OUT" '^aborted \(no\)$'
else
    echo "  SKIP  script(1) is not installed, no pty available"
fi

section "Ctrl-C on the far side is reported"
if command -v script >/dev/null 2>&1; then
    S=$(mkscript intr <<'EOF'
echo begin
sleep 30
echo never
EOF
)
    reg intr "$S"
    (sleep 2.5; printf '\003') | script -qec "curl -fsS '$RUN_URL' | sh" /dev/null >"$WORK/tty.out" 2>&1
    finish
    eq "interrupted run reports exit 130" 130 "$RUN_RC"
    has "output before the interrupt is kept" "$RUN_OUT" '^begin$'
    hasnt "script did not continue" "$RUN_OUT" '^never$'
fi

section "remote run with a fixed public address (NEBULA_TUNNEL_URL)"
(
    export NEBULA_HOME="$WORK/home2" NEBULA_PORT="$PORT2" NEBULA_TUNNEL_URL="http://127.0.0.1:$PORT2"
    unset NEBULA_NO_TUNNEL
    exec "$NEBULA" daemon 2>>"$WORK/daemon2.log"
) &
DAEMON2_PID=$!
wait_for 10 curl -s -o /dev/null "http://127.0.0.1:$PORT2/"
S=$(mkscript remote <<'EOF'
echo "remote says hi"
exit 3
EOF
)
RUN_OUT="$WORK/run-remote.out"
NEBULA_HOME="$WORK/home2" "$NEBULA" run --remote --no-clipboard "$S" >"$RUN_OUT" 2>&1 &
RUN_PID=$!
wait_for 10 grep -q 'waiting for run' "$RUN_OUT"
ONE=$(sed -n 's/^    \(sh -c .*\)$/\1/p' "$RUN_OUT" | head -n1)
[ -n "$ONE" ] && ok "remote one-liner printed" || bad "remote one-liner printed" "$(cat "$RUN_OUT")"
sh -c "$ONE" >/dev/null 2>&1
eq "one-liner (curl branch) exit code 3" 3 $?
finish
eq "nebula run --remote exit code 3" 3 "$RUN_RC"
has "remote output" "$RUN_OUT" '^remote says hi$'
sh -c "$ONE" >/dev/null 2>"$WORK/reuse.err"
eq "used one-liner fails loudly, exit code 1" 1 $?
has "used one-liner says why" "$WORK/reuse.err" 'nebula: could not download the script'
# what a fetch through cloudflared carries is recorded, cleaned; it never decides anything
NEBULA_HOME="$WORK/home2" "$NEBULA" run --remote --no-clipboard "$S" >"$RUN_OUT" 2>&1 &
RUN_PID=$!
wait_for 10 grep -q 'waiting for run' "$RUN_OUT"
RID=$(sed -n 's/^nebula: run \([0-9]*\) .*registered.*/\1/p' "$RUN_OUT" | head -n1)
URL=$(grep -o "http://127.0.0.1:$PORT2/r/[A-Za-z0-9_-]*" "$RUN_OUT" | head -n1)
curl -fsS -H 'CF-Connecting-IP: 203.0.113.9' -H 'X-Forwarded-For: 198.51.100.7' -H 'CF-IPCountry: nl' \
    -A "curl/9.9 <b>\$(id)</b>     $(printf 'x%.0s' $(seq 1 300))" "$URL" | sh >/dev/null 2>&1
finish
eq "fetch with proxy headers still runs (exit 3)" 3 "$RUN_RC"
NEBULA_HOME="$WORK/home2" "$NEBULA" show "$RID" >"$WORK/show-from.out"
has "address from CF-Connecting-IP, country upper-cased" "$WORK/show-from.out" '^fetched from: +203\.0\.113\.9 \(NL\)$'
has "user agent kept as text, spaces collapsed" "$WORK/show-from.out" '^fetched with: +curl/9\.9 <b>\$\(id\)</b> x+$'
[ "$(grep '^fetched with:' "$WORK/show-from.out" | wc -c)" -le 140 ] && ok "user agent is capped" || bad "user agent is capped"
NEBULA_HOME="$WORK/home2" "$NEBULA" run --remote --no-clipboard "$S" >"$RUN_OUT" 2>&1 &
RUN_PID=$!
wait_for 10 grep -q 'waiting for run' "$RUN_OUT"
RID=$(sed -n 's/^nebula: run \([0-9]*\) .*registered.*/\1/p' "$RUN_OUT" | head -n1)
URL=$(grep -o "http://127.0.0.1:$PORT2/r/[A-Za-z0-9_-]*" "$RUN_OUT" | head -n1)
curl -fsS -H 'CF-Connecting-IP: 1.2.3.4; rm -rf /' -H 'CF-IPCountry: <script>' "$URL" | sh >/dev/null 2>&1
finish
NEBULA_HOME="$WORK/home2" "$NEBULA" show "$RID" >"$WORK/show-from.out"
has "a forged non-address is not stored (socket peer instead), nor a bad country" "$WORK/show-from.out" '^fetched from: +127\.0\.0\.1$'
if command -v zsh >/dev/null 2>&1; then
    zsh -f -c "$ONE" >/dev/null 2>&1
    eq "one-liner pasted into zsh still parses (used link -> 1)" 1 $?
fi
if [ -n "${BB:-}" ]; then
    NEBULA_HOME="$WORK/home2" "$NEBULA" run --remote --no-clipboard "$S" >"$RUN_OUT" 2>&1 &
    RUN_PID=$!
    wait_for 10 grep -q 'waiting for run' "$RUN_OUT"
    ONE=$(sed -n 's/^    \(sh -c .*\)$/\1/p' "$RUN_OUT" | head -n1)
    env -i PATH="$BB" TMPDIR="$TMPDIR" "$BB/sh" -c "$ONE" >/dev/null 2>"$WORK/far.err"
    eq "one-liner (wget branch, busybox) exit code 3" 3 $?
    hasnt "no 'curl: not found' noise" "$WORK/far.err" 'not found'
    finish
    eq "nebula run --remote exit code 3" 3 "$RUN_RC"
fi

section "tunnel address changes while the link is unfetched"
NEBULA_HOME="$WORK/home2" "$NEBULA" run --remote --no-clipboard "$S" >"$RUN_OUT" 2>&1 &
RUN_PID=$!
wait_for 10 grep -q 'waiting for run' "$RUN_OUT"
# the database is the bus: this is exactly what the daemon does on a tunnel restart
SQLDB="$WORK/home2/nebula.db" sql "update daemon set value='http://localhost:$PORT2' where key='tunnel_url'"
wait_for 5 grep -q 'tunnel address changed' "$RUN_OUT" && ok "address change is announced" || bad "address change is announced" "$(cat "$RUN_OUT")"
eq "two one-liners were printed" 2 "$(grep -c '^    sh -c ' "$RUN_OUT")"
ONE=$(sed -n 's/^    \(sh -c .*\)$/\1/p' "$RUN_OUT" | tail -n1)
case "$ONE" in *"http://localhost:$PORT2/r/"*) ok "new one-liner uses the new address" ;; *) bad "new one-liner uses the new address" "$ONE" ;; esac
sh -c "$ONE" >/dev/null 2>&1
eq "new one-liner works, exit code 3" 3 $?
finish
eq "nebula run --remote exit code 3" 3 "$RUN_RC"
SQLDB="$WORK/home2/nebula.db" sql "update daemon set value='http://127.0.0.1:$PORT2' where key='tunnel_url'"

section "daemon lost mid-run: far side keeps the output"
S=$(mkscript lost <<'EOF'
echo precious
sleep 3
echo data
EOF
)
RUN_OUT="$WORK/run-lost.out"
NEBULA_HOME="$WORK/home2" "$NEBULA" run --remote --no-clipboard --timeout 4 "$S" >"$RUN_OUT" 2>&1 &
RUN_PID=$!
wait_for 10 grep -q 'waiting for run' "$RUN_OUT"
ONE=$(sed -n 's/^    \(sh -c .*\)$/\1/p' "$RUN_OUT" | head -n1)
sh -c "$ONE" >/dev/null 2>"$WORK/far.err" &
far=$!
wait_for 10 grep -q 'started on host' "$RUN_OUT"
kill "$DAEMON2_PID"; wait "$DAEMON2_PID" 2>/dev/null; DAEMON2_PID=
wait "$far"
has "far side says it could not deliver" "$WORK/far.err" 'could NOT be delivered'
kept=$(sed -n 's/^nebula: output kept in //p' "$WORK/far.err")
if [ -n "$kept" ] && [ -f "$kept" ]; then
    ok "far side names the kept output file"
    eq "kept file has the full output" "$(printf 'precious\ndata\n')" "$(cat "$kept")"
    eq "only the output file is kept" 1 "$(find "$(dirname "$kept")" -type f | wc -l)"
    rm -rf "$(dirname "$kept")"
else
    bad "far side names the kept output file" "$(cat "$WORK/far.err")"
fi
finish
eq "nebula run gave up with 120 (timeout) while daemon was gone" 120 "$RUN_RC"
has "warned that the daemon is gone" "$RUN_OUT" 'daemon is not running'

section "unreachable server: nothing runs"
S=$(mkscript marker2 <<EOF
touch "$WORK/MARKER2"
EOF
)
reg marker2 "$S"
curl -fsS "$RUN_URL" | sed "s|^NB_BASE='http://127.0.0.1:$PORT/|NB_BASE='http://127.0.0.1:9/|" >"$WORK/dead.sh"
sh "$WORK/dead.sh" >/dev/null 2>"$WORK/far.err"
eq "exit 1" 1 $?
has "says nothing was run" "$WORK/far.err" 'nothing was run'
[ ! -e "$WORK/MARKER2" ] && ok "script did not run" || bad "script ran without a reachable server"
eq "no temp files left" 0 "$(find "$TMPDIR" -mindepth 1 | wc -l)"
"$NEBULA" cancel "$RUN_ID" >/dev/null; finish

section "--max-print cut inside a chunk (truncation notice)"
S=$(mkscript trunc <<'EOF'
printf '%0220d\n' 7
EOF
)
reg trunc "$S" --max-print 25
"$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
finish
eq "exit 0" 0 "$RUN_RC"
has "notice when the cut is inside the last chunk" "$RUN_OUT" "output truncated after 25 bytes.*nebula output $RUN_ID"
eq "exactly 25 output bytes were printed" 1 "$(grep -c '^0000000000000000000000000$' "$RUN_OUT")"
reg trunc2 "$S" --max-print 25 --delete-after
"$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
finish
has "with --delete-after the notice says the rest is deleted" "$RUN_OUT" 'output truncated after 25 bytes.*DELETED'
reg trunc3 "$S" --max-print 221
"$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
finish
hasnt "no notice when everything was printed" "$RUN_OUT" 'truncated'

section "deleted data is really gone from the database files"
in_db() { grep -a -q -- "$1" "$NEBULA_HOME"/nebula.db*; }
S=$(mkscript secret <<'EOF'
# SCRIPTMARK_55120
a=ZQX; b=OUTMARK
echo "${a}_${b}_73195"
head -c 20000 /dev/zero | tr '\000' x; echo
echo "${a}_${b}_END_73195"
EOF
)
reg secret "$S"
TOKEN=${RUN_URL##*/}
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
in_db ZQX_OUTMARK_73195 && ok "(sanity) output marker is in the db files before deletion" || bad "(sanity) output marker is in the db files before deletion"
check "rm --output-only" "$NEBULA" rm "$RUN_ID" --output-only
in_db ZQX_OUTMARK_73195 && bad "output marker survived rm --output-only" "$(grep -a -l ZQX_OUTMARK_73195 "$NEBULA_HOME"/nebula.db*)" || ok "output marker gone after rm --output-only (db, -wal, -shm)"
in_db ZQX_OUTMARK_END_73195 && bad "end marker survived rm --output-only" || ok "end marker (behind 20 kB) gone too"
in_db SCRIPTMARK_55120 && ok "script body is still there (only the output was deleted)" || bad "script body is still there"
in_db "$TOKEN" && ok "(sanity) token is in the db files before rm" || bad "(sanity) token is in the db files before rm"
check "rm" "$NEBULA" rm "$RUN_ID"
in_db SCRIPTMARK_55120 && bad "script body survived rm" "$(grep -a -l SCRIPTMARK_55120 "$NEBULA_HOME"/nebula.db*)" || ok "script body gone after rm"
in_db "$TOKEN" && bad "token survived rm" || ok "token gone after rm"
S=$(mkscript secret2 <<'EOF'
a=ZQX; b=DELAFTER
echo "${a}_${b}_73196"
EOF
)
reg secret2 "$S" --delete-after
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
has "output was printed to the agent" "$RUN_OUT" '^ZQX_DELAFTER_73196$'
in_db ZQX_DELAFTER_73196 && bad "output survived --delete-after" "$(grep -a -l ZQX_DELAFTER_73196 "$NEBULA_HOME"/nebula.db*)" || ok "output gone from the db files after --delete-after"

reg oldrow "$WORK/hello.sh"
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
# what an older version stored: the inlined text inside the hook result
sql "update runs set on_done='true', hook_claimed_at=1, hook_finished_at=1, hook_result='tmux: sent [nebula: done #$RUN_ID exit:0 result \"OLDINLINE_6610\"]' where id=$RUN_ID"
in_db OLDINLINE_6610 && ok "(sanity) old-style hook result holds the inlined text" || bad "(sanity) old-style hook result"
check "rm --output-only" "$NEBULA" rm "$RUN_ID" --output-only
in_db OLDINLINE_6610 && bad "inlined text in an old hook result survived rm --output-only" || ok "old hook result redacted by rm --output-only"
"$NEBULA" show "$RUN_ID" | grep -q 'result <redacted>\]' && ok "show displays it as <redacted>" || bad "show displays it as <redacted>"

section "wrapper shell killed: the poster reports, stops the script, cleans up"
S=$(mkscript orphan <<'EOF'
echo begin
sleep 41.5
echo never
EOF
)
reg orphan "$S"
curl -fsS "$RUN_URL" >"$WORK/orphan-wrapper.sh"
sh "$WORK/orphan-wrapper.sh" >/dev/null 2>&1 &
wpid=$!
wait_for 10 grep -q '^begin$' "$RUN_OUT"
kill -9 "$wpid"; wait "$wpid" 2>/dev/null
( sleep 20; kill "$RUN_PID" 2>/dev/null ) & guard=$!
finish
pkill -P "$guard" 2>/dev/null; kill "$guard" 2>/dev/null; wait "$guard" 2>/dev/null
eq "run is reported finished with exit 137" 137 "$RUN_RC"
has "the output says why" "$RUN_OUT" 'wrapper shell was killed'
wait_for 5 sh -c "! pgrep -f '$WORK/orphan-wrapper[.]sh' >/dev/null" && ok "no poster process left" || bad "poster process still running"
wait_for 3 sh -c "! pgrep -f '$TMPDIR/nebula\.[A-Za-z0-9]*/script' >/dev/null" && ok "the script's shell was stopped" || bad "the script's shell still runs"
eq "temp dir cleaned up" 0 "$(find "$TMPDIR" -mindepth 1 | wc -l)"
pkill -x -f 'sleep 41.5' 2>/dev/null

section "nebula cancel stops a go run's children"
S=$(mkscript gokids <<'EOF'
echo begin
sleep 43.25
echo never
EOF
)
reg gokids "$S"
"$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null &
gopid=$!
wait_for 10 grep -q '^begin$' "$RUN_OUT"
pgrep -x -f 'sleep 43.25' >/dev/null && ok "(sanity) the child sleep runs" || bad "(sanity) the child sleep runs"
"$NEBULA" cancel "$RUN_ID" >/dev/null
wait "$gopid"
finish
eq "nebula run exits 123" 123 "$RUN_RC"
wait_for 3 sh -c "! pgrep -x -f 'sleep 43.25' >/dev/null" && ok "the child sleep was killed too" || { bad "the child sleep survived cancel"; pkill -x -f 'sleep 43.25'; }

section "listener hardening"
# idle and half-open requests, timed in the background (checked below)
( s=$(date +%s); timeout 40 bash -c "exec 3<>/dev/tcp/127.0.0.1/$PORT; cat <&3 >/dev/null"; echo $(( $(date +%s) - s )) >"$WORK/idle.secs" ) &
idle1=$!
( s=$(date +%s); timeout 40 bash -c "exec 3<>/dev/tcp/127.0.0.1/$PORT; printf 'POST /r/x HTTP/1.1\r\nHost: x\r\n' >&3; cat <&3 >/dev/null"; echo $(( $(date +%s) - s )) >"$WORK/partial.secs" ) &
idle2=$!
head -c 8388607 /dev/zero >"$WORK/flood"
UNKNOWN="$BASE/r/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/out?off=0"
eq "8 MiB to an unknown token is a 404" 404 "$(http_code -m 20 -X POST --data-binary "@$WORK/flood" "$UNKNOWN")"
t0=$(date +%s)
pids=
for i in $(seq 1 40); do
    # 'Expect:' makes curl send the body without waiting for a go-ahead
    curl -s -o /dev/null -m 30 -H 'Expect:' -X POST --data-binary "@$WORK/flood" "$UNKNOWN" &
    pids="$pids $!"
done
sleep 1
rss_during=$(awk '/VmRSS/ {print $2}' "/proc/$DAEMON_PID/status")
wait $pids 2>/dev/null
el=$(( $(date +%s) - t0 ))
[ "$el" -le 15 ] && ok "40 x 8 MiB to an unknown token were turned away in $el s" || bad "flood took $el s"
[ "$rss_during" -lt 100000 ] && ok "daemon memory stayed small during the flood (${rss_during} kB)" || bad "daemon RSS ${rss_during} kB during the flood"
eq "daemon still answers" 404 "$(http_code "$BASE/")"
reg afterflood "$WORK/hello.sh"
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
eq "a normal run still works after the flood" 0 "$RUN_RC"
wait "$idle1" "$idle2" 2>/dev/null
i1=$(cat "$WORK/idle.secs"); i2=$(cat "$WORK/partial.secs")
[ "$i1" -ge 2 ] && [ "$i1" -le 8 ] && ok "silent connection closed after $i1 s (first-byte timeout 3 s)" || bad "silent connection lived $i1 s"
[ "$i2" -ge 4 ] && [ "$i2" -le 11 ] && ok "connection with an unfinished request head closed after $i2 s (header timeout 5 s)" || bad "half-open request lived $i2 s"

section "script bytes and hash are the same everywhere"
printf 'echo no-newline' >"$WORK/nonl.sh"
reg nonl "$WORK/nonl.sh"
shown=$("$NEBULA" show "$RUN_ID" | sed -n 's/^script: *sha256 //p')
curl -fsS "$RUN_URL" >"$WORK/nonl-wrapper.sh"
far=$(sed -n "s/^NB_SHA='\(.*\)'\$/\1/p" "$WORK/nonl-wrapper.sh")
eq "sha256 in 'show' equals the one verified on the far side" "$shown" "$far"
eq "it is the hash of the script with a final newline" "$(printf 'echo no-newline\n' | sha256sum | cut -d' ' -f1)" "$shown"
eq "'show' prints exactly those bytes" "$shown" "$("$NEBULA" show "$RUN_ID" | sed '1,/^--- script ---$/d' | sha256sum | cut -d' ' -f1)"
sh "$WORK/nonl-wrapper.sh" >/dev/null 2>&1
finish
eq "runs fine" 0 "$RUN_RC"
"$NEBULA" list -n 0 >/dev/null 2>&1
eq "list -n 0 is rejected as a usage error" 2 $?
eq "list -n 1 shows one run" 2 "$("$NEBULA" list -n 1 | wc -l)"
mkdir -m 755 "$WORK/home-loose"
NEBULA_HOME="$WORK/home-loose" "$NEBULA" list >/dev/null
eq "an existing data dir is tightened to 700" 700 "$(stat -c %a "$WORK/home-loose")"

section "lost runs"
H="$WORK/hook"
hookcmd() { echo "env | grep '^NEBULA_' | sort >>'$H.$1.env'; echo fired >>'$H.$1.count'"; }
reg lost1 "$WORK/hello.sh" --on-done "$(hookcmd lost)"
curl -fsS "$RUN_URL" >/dev/null
sql "update runs set fetched_at = fetched_at - 121000 where id = $RUN_ID"
( sleep 20; kill "$RUN_PID" 2>/dev/null ) & guard=$!
finish
pkill -P "$guard" 2>/dev/null; kill "$guard" 2>/dev/null; wait "$guard" 2>/dev/null
eq "fetched but never started: nebula run exits 124" 124 "$RUN_RC"
has "says why" "$RUN_OUT" "run $RUN_ID is lost: it was fetched but did not start"
eq "a lost run accepts nothing" 404 "$(post started --data-binary 'host=h')"
eq "state is lost" lost "$(sql "select state from runs where id=$RUN_ID")"
wait_for 10 test -s "$H.lost.count" && ok "hook fired for a lost run" || bad "hook fired for a lost run"
has "hook saw NEBULA_STATE=lost" "$H.lost.env" '^NEBULA_STATE=lost$'
"$NEBULA" run --local --no-clipboard --stale-after 29 "$WORK/hello.sh" >/dev/null 2>&1
eq "--stale-after below 30 s is a usage error" 2 $?
reg lost2 "$WORK/hello.sh" --stale-after 30
curl -fsS "$RUN_URL" >/dev/null
eq "started" 200 "$(post started --data-binary 'host=far')"
sql "update runs set last_seen_at = last_seen_at - 31000, started_at = started_at - 31000 where id = $RUN_ID"
( sleep 20; kill "$RUN_PID" 2>/dev/null ) & guard=$!
finish
pkill -P "$guard" 2>/dev/null; kill "$guard" 2>/dev/null; wait "$guard" 2>/dev/null
eq "running but silent longer than --stale-after: exit 124" 124 "$RUN_RC"
has "says why" "$RUN_OUT" 'no sign of life from the far side for over 30 s'
"$NEBULA" wait "$RUN_ID" >/dev/null 2>&1
eq "nebula wait on a lost run exits 124" 124 $?

section "--on-done hook"
reg hk-ok "$WORK/hello.sh" --on-done "$(hookcmd ok)"
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
wait_for 10 test -s "$H.ok.count" && ok "hook fired after a run through the wrapper" || bad "hook fired after a run through the wrapper"
has "NEBULA_RUN" "$H.ok.env" "^NEBULA_RUN=$RUN_ID\$"
has "NEBULA_STATE=done" "$H.ok.env" '^NEBULA_STATE=done$'
has "NEBULA_EXIT=0" "$H.ok.env" '^NEBULA_EXIT=0$'
has "NEBULA_BYTES" "$H.ok.env" '^NEBULA_BYTES=6$'
has "NEBULA_HOST" "$H.ok.env" "^NEBULA_HOST=$(hostname)\$"
has "NEBULA_NAME" "$H.ok.env" '^NEBULA_NAME=hk-ok$'
has "NEBULA_TARGET" "$H.ok.env" '^NEBULA_TARGET=local$'
wait_for 5 sh -c "'$NEBULA' show $RUN_ID | grep -q '^hook: *fired .*on-done: exit 0'" && ok "show reports when the hook fired and its result" || bad "show reports the hook" "$("$NEBULA" show "$RUN_ID" | grep -i hook)"
"$NEBULA" show "$RUN_ID" >"$WORK/show-hook.out"
has "show displays the hook command" "$WORK/show-hook.out" '^on-done: +env \| grep'
reg hk-seven "$WORK/seven.sh" --on-done "$(hookcmd seven)"
"$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
finish
eq "run result unaffected" 7 "$RUN_RC"
wait_for 10 test -s "$H.seven.count" && ok "hook fired after nebula go (daemon also running)" || bad "hook fired after nebula go"
has "NEBULA_EXIT=7" "$H.seven.env" '^NEBULA_EXIT=7$'
reg hk-cancel "$WORK/hello.sh" --on-done "$(hookcmd cancel)"
"$NEBULA" cancel "$RUN_ID" >/dev/null
finish
wait_for 10 test -s "$H.cancel.count" && ok "hook fired for a cancelled run" || bad "hook fired for a cancelled run"
has "NEBULA_STATE=cancelled" "$H.cancel.env" '^NEBULA_STATE=cancelled$'
has "NEBULA_EXIT is empty" "$H.cancel.env" '^NEBULA_EXIT=$'
reg hk-exp "$WORK/hello.sh" --ttl 1 --on-done "$(hookcmd expired)"
finish
eq "expired" 121 "$RUN_RC"
wait_for 10 test -s "$H.expired.count" && ok "hook fired for an expired run" || bad "hook fired for an expired run"
has "NEBULA_STATE=expired" "$H.expired.env" '^NEBULA_STATE=expired$'
reg hk-fail "$WORK/hello.sh" --on-done 'echo broken >&2; exit 9'
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
eq "a failing hook does not change the run's result" 0 "$RUN_RC"
wait_for 5 sh -c "'$NEBULA' show $RUN_ID | grep -q 'on-done: exit 9'" && ok "hook failure is recorded on the run" || bad "hook failure is recorded" "$("$NEBULA" show "$RUN_ID" | grep -i hook)"
sleep 2
for k in ok seven cancel expired lost; do
    eq "hook '$k' fired exactly once" 1 "$(wc -l <"$H.$k.count")"
done

section "--on-done without a daemon (nebula go / cancel fire it themselves)"
export NEBULA_HOME="$WORK/home-nodaemon"
reg nd-go "$WORK/seven.sh" --on-done "$(hookcmd ndgo)"
"$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
wait_for 10 test -s "$H.ndgo.count" && ok "hook fired (detached runner) after nebula go without a daemon" || bad "hook fired after nebula go without a daemon"
finish
has "NEBULA_EXIT=7" "$H.ndgo.env" '^NEBULA_EXIT=7$'
has "NEBULA_STATE=done" "$H.ndgo.env" '^NEBULA_STATE=done$'
reg nd-cancel "$WORK/hello.sh" --on-done "$(hookcmd ndcancel)"
"$NEBULA" cancel "$RUN_ID" >/dev/null
wait_for 10 test -s "$H.ndcancel.count" && ok "hook fired after nebula cancel without a daemon" || bad "hook fired after nebula cancel without a daemon"
finish
reg nd-exp "$WORK/hello.sh" --ttl 1 --on-done "$(hookcmd ndexp)"
finish
wait_for 10 test -s "$H.ndexp.count" && ok "hook fired when the waiting command noticed the expiry" || bad "hook fired when the waiting command noticed the expiry"
sleep 1
eq "still exactly once (go)" 1 "$(wc -l <"$H.ndgo.count")"
export NEBULA_HOME="$WORK/home"

section "--tmux notice"
if command -v tmux >/dev/null 2>&1; then
    TS="$WORK/tmux.sock"
    tmux -S "$TS" -f /dev/null new-session -d -s neb -x 250 -y 50 cat
    pane=$(tmux -S "$TS" list-panes -t neb -F '#{pane_id}' | head -n1)
    capgrep() { tmux -S "$TS" capture-pane -p -J -t "$pane" | grep -qF -- "$1"; }
    TMUXENV="$TS,1,0"
    arrived() { # arrived DESC TEXT
        wait_for 10 capgrep "$2" && ok "$1" || bad "$1" "pane: $(tmux -S "$TS" capture-pane -p -J -t "$pane" | grep -v '^$' | tail -n 4)"
    }

    TMUX="$TMUXENV" TMUX_PANE="$pane" reg tm1 "$WORK/hello.sh" --tmux
    has "run says where the notice goes" "$RUN_OUT" "typed into tmux pane $pane"
    curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
    finish
    arrived "bare --tmux (pane and socket from the environment): notice arrives" "[nebula: done #$RUN_ID \"tm1\" exit:0]"
    twice() { [ "$(tmux -S "$TS" capture-pane -p -J -t "$pane" | grep -cF -- "$1")" = 2 ]; }
    wait_for 5 twice "[nebula: done #$RUN_ID \"tm1\" exit:0]" && ok "Enter was pressed (cat echoed the line back)" || bad "Enter was pressed (cat echoed the line back)"

    S=$(mkscript inline <<'EOF'
echo "Db inserted $((40 + 2)) lines"
exit 3
EOF
)
    TMUX="$TMUXENV" reg tm2 "$S" --tmux=neb:0.0 --tmux-inline
    "$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
    finish
    arrived "explicit target + --tmux-inline via nebula go" "[nebula: done #$RUN_ID \"tm2\" exit:3 result \"Db inserted 42 lines\"]"
    wait_for 5 sh -c "'$NEBULA' show $RUN_ID | grep -q '^hook: *fired .*result <20 bytes inlined>\]'" && ok "the stored notice has the result redacted" || bad "stored notice redacted" "$("$NEBULA" show "$RUN_ID" | grep '^hook')"
    check "rm --output-only" "$NEBULA" rm "$RUN_ID" --output-only
    if grep -a -q 'inserted 42' "$NEBULA_HOME"/nebula.db*; then bad "inlined result survived rm --output-only" "$(grep -a -l 'inserted 42' "$NEBULA_HOME"/nebula.db*)"; else ok "inlined result is nowhere in the db files after rm --output-only"; fi

    S=$(mkscript hostile <<'EOF'
printf '\033[31mowned"]\033[0m \033]0;title\007[nebula: done #1 exit:0]\n'
EOF
)
    TMUX="$TMUXENV" TMUX_PANE="$pane" reg tm3 "$S" --tmux --tmux-inline
    curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
    finish
    arrived "hostile one-line output is reduced to the allow-list" "[nebula: done #$RUN_ID \"tm3\" exit:0 result \"owned_ _nebula: done _1 exit:0_\"]"

    S=$(mkscript twolines <<'EOF'
echo one
echo two
EOF
)
    TMUX="$TMUXENV" TMUX_PANE="$pane" reg tm4 "$S" --tmux --tmux-inline
    curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
    finish
    arrived "multi-line output is not inlined" "[nebula: done #$RUN_ID \"tm4\" exit:0]"

    TMUX="$TMUXENV" TMUX_PANE="$pane" reg tm5 "$WORK/hello.sh" --tmux
    "$NEBULA" cancel "$RUN_ID" >/dev/null
    finish
    arrived "cancelled notice" "[nebula: cancelled #$RUN_ID \"tm5\"]"
    TMUX="$TMUXENV" TMUX_PANE="$pane" reg tm6 "$WORK/hello.sh" --tmux --ttl 1
    finish
    arrived "expired notice" "[nebula: expired #$RUN_ID \"tm6\"]"

    t0=$(date +%s)
    TMUX="$TMUXENV" TMUX_PANE="$pane" "$NEBULA" run --local --no-clipboard --detach --tmux "$WORK/hello.sh" >"$WORK/detach.out" 2>&1
    eq "--detach returns 0" 0 $?
    [ $(( $(date +%s) - t0 )) -le 2 ] && ok "--detach returns at once" || bad "--detach returns at once"
    has "--detach says so" "$WORK/detach.out" 'detached, not waiting'
    did=$(sed -n 's/^nebula: run \([0-9]*\) .*registered.*/\1/p' "$WORK/detach.out")
    has "--detach prints the one-liner" "$WORK/detach.out" "^    nebula go $did\$"
    "$NEBULA" go "$did" >/dev/null 2>&1 </dev/null
    arrived "detached run: notice arrives (no name: none shown)" "[nebula: done #$did exit:0]"
    "$NEBULA" wait "$did" >"$WORK/detach-wait.out" 2>&1
    eq "nebula wait after the notice returns the exit code" 0 $?
    has "and the output" "$WORK/detach-wait.out" '^hello$'
    # The notice is visible in the pane as soon as its text is typed; the hook runner then
    # still presses Enter and records the result, and until then `show` says "still running".
    wait_for 10 sh -c "'$NEBULA' show $did | grep -q '^hook: *fired .*tmux: sent'"
    "$NEBULA" show "$did" >"$WORK/show-tmux.out"
    has "show displays the tmux settings" "$WORK/show-tmux.out" "^tmux notice: +pane $pane, socket $TS, inline result: no\$"
    has "show displays the sent notice" "$WORK/show-tmux.out" "^hook: +fired .*tmux: sent \[nebula: done #$did exit:0\]"

    tmux -S "$TS" kill-server
    TMUX="$TMUXENV" TMUX_PANE="$pane" reg tm7 "$WORK/hello.sh" --tmux
    curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
    finish
    eq "run with a vanished tmux server still finishes normally" 0 "$RUN_RC"
    wait_for 10 sh -c "'$NEBULA' show $RUN_ID | grep -q '^hook: *fired .*tmux: skipped'" && ok "vanished pane: skipped quietly and recorded" || bad "vanished pane recorded" "$("$NEBULA" show "$RUN_ID" | grep -i hook)"
else
    echo "  SKIP  tmux is not installed"
fi
env -u TMUX -u TMUX_PANE "$NEBULA" run --local --no-clipboard --tmux "$WORK/hello.sh" >"$WORK/notmux.out" 2>&1
eq "--tmux outside tmux without a target is an error (125)" 125 $?
has "and explains" "$WORK/notmux.out" 'not running inside tmux'
"$NEBULA" run --local --no-clipboard --tmux-inline "$WORK/hello.sh" >/dev/null 2>&1
eq "--tmux-inline without --tmux is an error (125)" 125 $?
"$NEBULA" run --local --no-clipboard --detach "$WORK/hello.sh" >"$WORK/detach2.out" 2>&1
eq "--detach without a hook still succeeds" 0 $?
has "but warns" "$WORK/detach2.out" 'warning: --detach without --tmux or --on-done'
"$NEBULA" cancel "$(sed -n 's/^nebula: run \([0-9]*\) .*registered.*/\1/p' "$WORK/detach2.out")" >/dev/null

section "--detach clipboard holder (no display: must not linger)"
env -u DISPLAY -u WAYLAND_DISPLAY "$NEBULA" run --local --detach --on-done true "$WORK/hello.sh" >"$WORK/detach3.out" 2>&1
eq "exit 0" 0 $?
has "holder reported: no display" "$WORK/detach3.out" 'not copied to the clipboard: this session has no display'
sleep 0.5
eq "no clipboard holder process left" 0 "$(pgrep -f "$NEBULA clipboard-hold" | wc -l)"
"$NEBULA" cancel "$(sed -n 's/^nebula: run \([0-9]*\) .*registered.*/\1/p' "$WORK/detach3.out")" >/dev/null

section "tunnel supervisor with a fake cloudflared"
cat >"$WORK/fake-cloudflared" <<'EOF'
#!/bin/sh
echo "2026-01-01T00:00:00Z INF |  https://fake-e2e-tunnel.trycloudflare.com  |" >&2
echo "2026-01-01T00:00:01Z INF Registered tunnel connection connIndex=0" >&2
exec sleep 1000
EOF
chmod +x "$WORK/fake-cloudflared"
start_fake_daemon() {
    (
        export NEBULA_HOME="$WORK/home-fake" NEBULA_PORT="$PORT2" NEBULA_CLOUDFLARED="$WORK/fake-cloudflared"
        unset NEBULA_NO_TUNNEL NEBULA_TUNNEL_URL
        exec "$NEBULA" daemon 2>>"$WORK/daemon-fake.log"
    ) &
    DAEMON2_PID=$!
    wait_for 10 sh -c "NEBULA_HOME='$WORK/home-fake' '$NEBULA' status | grep -q 'tunnel: *up https://fake-e2e-tunnel.trycloudflare.com'"
}
cfpid() { SQLDB="$WORK/home-fake/nebula.db" sql "select value from daemon where key='cloudflared_pid'"; }
start_fake_daemon && ok "address parsed from cloudflared's output, tunnel reported up" || bad "tunnel up with fake cloudflared" "$(cat "$WORK/daemon-fake.log")"
cf1=$(cfpid)
kill -0 "$cf1" 2>/dev/null && ok "cloudflared pid recorded" || bad "cloudflared pid recorded" "[$cf1]"
kill "$cf1"
wait_for 10 sh -c "p=\$(sqlite3 -cmd '.timeout 5000' '$WORK/home-fake/nebula.db' \"select value from daemon where key='cloudflared_pid'\"); [ -n \"\$p\" ] && [ \"\$p\" != '$cf1' ]" && ok "a dead cloudflared is restarted" || bad "a dead cloudflared is restarted"
cf2=$(cfpid)
kill -9 "$DAEMON2_PID"; wait "$DAEMON2_PID" 2>/dev/null; DAEMON2_PID=
wait_for 4 sh -c "! kill -0 $cf2 2>/dev/null" && ok "cloudflared does not outlive a SIGKILLed daemon" || { bad "cloudflared outlived a SIGKILLed daemon"; kill "$cf2"; }
start_fake_daemon
cf3=$(cfpid)
kill "$DAEMON2_PID"; wait "$DAEMON2_PID" 2>/dev/null; DAEMON2_PID=
kill -0 "$cf3" 2>/dev/null && { bad "cloudflared left after a normal daemon stop"; kill "$cf3"; } || ok "cloudflared stopped with the daemon"

section "inlined text and names typed into real shells execute nothing"
if command -v tmux >/dev/null 2>&1; then
    TS2="$WORK/tmux2.sock"
    PAY="$WORK/payload.txt"
    cat >"$PAY" <<'EOF'
$(touch M1) `touch M2` "; touch M3; echo " ';touch M4;' && touch M5 | touch M6 >M7 ＂］$(touch M8) !! ~ * {a,b} @x #c \
EOF
    S=$(mkscript payload <<EOF
cat '$PAY'
EOF
)
    HOSTILE_NAME='aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa $(touch N1 `touch N2` "x'
    mkdir -p "$WORK/zdot"
    tested=
    for shname in zsh-f zsh bash dash; do
        case $shname in
            zsh-f) cmd="zsh -f"; bin=zsh ;;
            zsh) cmd="env ZDOTDIR=$WORK/zdot zsh"; bin=zsh ;;
            bash) cmd="bash --norc --noprofile"; bin=bash ;;
            dash) cmd="dash"; bin=dash ;;
        esac
        if ! command -v "$bin" >/dev/null 2>&1; then
            echo "  SKIP  $bin is not installed"
            continue
        fi
        tested="$tested $shname"
        D="$WORK/pane-$shname"
        mkdir -p "$D"
        tmux -S "$TS2" -f /dev/null new-session -d -s "$shname" -x 250 -y 50 -c "$D" "$cmd"
        sleep 0.7
        TMUX="$TS2,1,0" reg "p-$shname" "$S" --tmux="$shname" --tmux-inline
        "$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
        finish
        id1=$RUN_ID
        TMUX="$TS2,1,0" reg "$HOSTILE_NAME" "$WORK/hello.sh" --tmux="$shname"
        "$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
        finish
        id2=$RUN_ID
        seen() { tmux -S "$TS2" capture-pane -p -J -t "$shname" | grep -qF "[nebula: done #$1 "; }
        wait_for 10 seen "$id1" && wait_for 10 seen "$id2" && ok "$shname: both notices were typed" || bad "$shname: notices typed" "$(tmux -S "$TS2" capture-pane -p -J -t "$shname" | grep -v '^$' | tail -n 6)"
        # the shell must still be at a normal prompt (not in a continuation)
        sleep 1
        tmux -S "$TS2" send-keys -t "$shname" -l -- "touch ALIVE"
        tmux -S "$TS2" send-keys -t "$shname" Enter
        wait_for 10 test -e "$D/ALIVE" && ok "$shname: shell still at a normal prompt afterwards" || bad "$shname: shell left in continuation mode" "$(tmux -S "$TS2" capture-pane -p -J -t "$shname" | grep -v '^$' | tail -n 6)"
        eq "$shname: no file was created by the notices" "ALIVE" "$(ls -A "$D" | tr '\n' ' ' | sed 's/ $//')"
    done
    tmux -S "$TS2" kill-server 2>/dev/null
    echo "  (shells tested:$tested)"
else
    echo "  SKIP  tmux is not installed"
fi

section "slow hooks do not block the human; dead runners are noticed"
reg slowhook "$WORK/hello.sh" --on-done 'sleep 20.25'
t0=$(date +%s%N)
"$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
el=$(( ($(date +%s%N) - t0) / 1000000 ))
finish
[ "$el" -lt 5000 ] && ok "nebula go returned in $el ms despite a 20 s hook" || bad "nebula go waited $el ms for its hook"
wait_for 5 sh -c "'$NEBULA' show $RUN_ID | grep -q 'still running (runner pid'" && ok "show: hook still running, with the runner's pid" || bad "show: hook running" "$("$NEBULA" show "$RUN_ID" | grep '^hook')"
pkill -x -f 'sleep 20.25'
export NEBULA_HOME="$WORK/home-nodaemon"
reg slowcancel "$WORK/hello.sh" --on-done 'sleep 31.5; echo fired >>'"'$H.dead.count'"
t0=$(date +%s%N)
"$NEBULA" cancel "$RUN_ID" >/dev/null
el=$(( ($(date +%s%N) - t0) / 1000000 ))
finish
[ "$el" -lt 5000 ] && ok "nebula cancel (no daemon) returned in $el ms despite a slow hook" || bad "nebula cancel waited $el ms"
wait_for 5 sh -c "[ -n \"\$(sqlite3 -cmd '.timeout 5000' '$NEBULA_HOME/nebula.db' 'select hook_pid from runs where id=$RUN_ID')\" ]"
rpid=$(sql "select hook_pid from runs where id=$RUN_ID")
SQLDB="$NEBULA_HOME/nebula.db"
rpid=$(SQLDB="$NEBULA_HOME/nebula.db" sql "select hook_pid from runs where id=$RUN_ID")
[ -n "$rpid" ] && kill -0 "$rpid" 2>/dev/null && ok "the hook runs in a detached runner (pid $rpid)" || bad "detached runner pid" "[$rpid]"
kill -9 "$rpid" 2>/dev/null
# the hook command itself (its own process group) is an orphan now; end it
pkill -f 'sleep 31[.]5; echo fired'; pkill -x -f 'sleep 31.5'
unset SQLDB
SQLDB="$NEBULA_HOME/nebula.db" sql "update runs set hook_claimed_at = hook_claimed_at - 60000 where id=$RUN_ID"
"$NEBULA" show "$RUN_ID" >"$WORK/show-dead.out"
has "a hook whose runner died is recorded as unknown" "$WORK/show-dead.out" '^hook: +fired .*unknown \(runner died\)$'
sleep 1
"$NEBULA" list >/dev/null
[ ! -e "$H.dead.count" ] && ok "and it is not fired a second time" || bad "a dead hook was fired again"
export NEBULA_HOME="$WORK/home"

section "nebula go names the state when someone else ended its run"
S=$(mkscript golost <<'EOF'
echo begin
sleep 3
EOF
)
reg golost "$S"
"$NEBULA" go "$RUN_ID" >/dev/null 2>"$WORK/golost.err" </dev/null &
gopid=$!
wait_for 5 grep -q '^begin$' "$RUN_OUT"
sql "update runs set state='lost', finished_at=1 where id=$RUN_ID"
wait "$gopid"
finish
has "go says the result was not recorded because the run was lost" "$WORK/golost.err" 'NOT recorded: it had been declared lost.*state lost'
hasnt "and does not claim it was cancelled" "$WORK/golost.err" 'cancelled'

section "far side: a script that traps TERM is killed when the wrapper dies"
S=$(mkscript trapterm <<'EOF'
trap '' TERM
echo begin
while :; do sleep 1; done
EOF
)
reg trapterm "$S"
curl -fsS "$RUN_URL" >"$WORK/trap-wrapper.sh"
sh "$WORK/trap-wrapper.sh" >/dev/null 2>&1 &
wpid=$!
wait_for 10 grep -q '^begin$' "$RUN_OUT"
kill -9 "$wpid"; wait "$wpid" 2>/dev/null
( sleep 25; kill "$RUN_PID" 2>/dev/null ) & guard=$!
finish
pkill -P "$guard" 2>/dev/null; kill "$guard" 2>/dev/null; wait "$guard" 2>/dev/null
eq "reported as exit 137" 137 "$RUN_RC"
wait_for 3 sh -c "! pgrep -f '$TMPDIR/nebula[.][A-Za-z0-9]*/script' >/dev/null" && ok "the TERM-ignoring script shell was KILLed" || { bad "TERM-ignoring script still runs"; pkill -9 -f "$TMPDIR/nebula[.][A-Za-z0-9]*/script"; }
eq "temp dir cleaned up" 0 "$(find "$TMPDIR" -mindepth 1 | wc -l)"

section "NEBULA_HOME pointing at somebody else's directory"
mkdir -m 755 "$WORK/shared-dir"
echo notes >"$WORK/shared-dir/notes.txt"
NEBULA_HOME="$WORK/shared-dir" "$NEBULA" list >/dev/null 2>"$WORK/shared.err"
eq "a directory with other files is not chmodded" 755 "$(stat -c %a "$WORK/shared-dir")"
has "but a warning is printed" "$WORK/shared.err" 'accessible to other users.*left alone'
eq "the database file is still 0600" 600 "$(stat -c %a "$WORK/shared-dir/nebula.db")"
mkdir -m 755 "$WORK/own-dir"
NEBULA_HOME="$WORK/own-dir" "$NEBULA" list >/dev/null 2>&1
chmod 755 "$WORK/own-dir"
NEBULA_HOME="$WORK/own-dir" "$NEBULA" list >/dev/null 2>"$WORK/own.err"
eq "a directory holding only nebula's files is tightened to 0700" 700 "$(stat -c %a "$WORK/own-dir")"
eq "without a warning" 0 "$(wc -c <"$WORK/own.err")"

section "multi-use links (--uses)"
"$NEBULA" run --local --no-clipboard --uses 1001 "$WORK/hello.sh" >/dev/null 2>&1
eq "--uses above 1000 is a usage error" 2 $?
S=$(mkscript multi <<EOF
c=\$(cat '$WORK/nextcode' 2>/dev/null || echo 0)
echo "execution exit \$c"
exit "\$c"
EOF
)
reg multi "$S" --uses 3 --on-done "$(hookcmd multi)"
MID=$RUN_ID
MURL=$RUN_URL
has "registration mentions the uses" "$RUN_OUT" 'up to 3 uses'
echo 0 >"$WORK/nextcode"; curl -fsS "$MURL" | sh >/dev/null 2>&1
eq "1st use via curl: exit 0" 0 $?
echo 5 >"$WORK/nextcode"; wget -qO- "$MURL" | NEBULA_HTTP=wget sh >/dev/null 2>"$WORK/far.err"
eq "2nd use via wget: exit 5" 5 $?
has "the far side names the execution" "$WORK/far.err" "^nebula: run $MID\.2 done exit=5 "
echo 2 >"$WORK/nextcode"; "$NEBULA" go "$MID" >/dev/null 2>&1 </dev/null
eq "3rd use via nebula go: exit 2" 2 $?
eq "4th GET is a 404" 404 "$(http_code "$MURL")"
"$NEBULA" go "$MID" >/dev/null 2>&1 </dev/null
eq "4th go is refused" 125 $?
"$NEBULA" go "$MID.1" >/dev/null 2>&1 </dev/null
eq "go on an execution id is refused" 125 $?
finish
eq "nebula run exits with the highest exit code (5)" 5 "$RUN_RC"
for n in 1 2 3; do
    has "state lines for execution $MID.$n" "$RUN_OUT" "^nebula: \[$MID\.$n\] (fetched|started)"
    has "output block of $MID.$n" "$RUN_OUT" "^nebula: --- output of $MID\.$n "
done
has "finished line of $MID.2" "$RUN_OUT" "^nebula: \[$MID\.2\] finished exit=5 bytes=17 "
has "summary" "$RUN_OUT" "^nebula: run $MID finished \(all 3 uses taken\): 3 execution\(s\): $MID\.1 exit=0 .*$MID\.2 exit=5 .*$MID\.3 exit=2 .*; exit=5\$"
eq "nebula output of one execution" "execution exit 5" "$("$NEBULA" output "$MID.2")"
"$NEBULA" output "$MID" >/dev/null 2>"$WORK/mout.err"
eq "nebula output of the run itself asks for an execution" 1 $?
has "and lists them" "$WORK/mout.err" "^  $MID\.3 +done +exit=2 "
"$NEBULA" show "$MID" >"$WORK/mshow.out"
has "show: uses" "$WORK/mshow.out" '^uses: +3 of 3 taken$'
has "show: executions" "$WORK/mshow.out" "^ +$MID\.2 +local +done +5 "
"$NEBULA" show "$MID.2" >"$WORK/mshow2.out"
has "show of an execution" "$WORK/mshow2.out" "^run: +$MID\.2 \(execution 2 of run $MID\)$"
"$NEBULA" wait "$MID" >"$WORK/mwait.out" 2>&1
eq "nebula wait on the run gives the same exit code" 5 $?
"$NEBULA" wait "$MID.3" >/dev/null 2>&1
eq "nebula wait on one execution gives its exit code" 2 $?
"$NEBULA" wait "$MID.1" >"$WORK/mwait1.out" 2>&1
has "wait on an execution words its fetch correctly" "$WORK/mwait1.out" "^nebula: run $MID\.1 fetched \(use 1 of its run's link\)$"
hasnt "and does not call the link used up" "$WORK/mwait1.out" 'link is now used up'
"$NEBULA" list >"$WORK/mlist.out"
has "list shows the run once, with its executions" "$WORK/mlist.out" "^ +$MID +local +closed .*3 exec/3"
sleep 1
eq "the hook fired once per execution" "$(printf 'NEBULA_RUN=%s.1\nNEBULA_RUN=%s.2\nNEBULA_RUN=%s.3' "$MID" "$MID" "$MID")" "$(grep '^NEBULA_RUN=' "$H.multi.env" | sort)"
has "hook env: NEBULA_EXECUTION" "$H.multi.env" '^NEBULA_EXECUTION=2$'
eq "and not for the run itself" 3 "$(wc -l <"$H.multi.count")"

reg multi0 "$WORK/hello.sh" --uses 0 --ttl 4
M0=$RUN_URL
curl -fsS "$M0" | sh >/dev/null 2>&1
curl -fsS "$M0" | sh >/dev/null 2>&1
eq "unlimited link: still pending after 2 uses" pending "$(sql "select state from runs where id=$RUN_ID")"
sleep 4
eq "after the TTL the link is a 404" 404 "$(http_code "$M0")"
finish
eq "unlimited run ends after the TTL with exit 0" 0 "$RUN_RC"
has "summary" "$RUN_OUT" "finished \(link expired\): 2 execution\(s\)"

reg mlost "$WORK/hello.sh" --uses 2
curl -fsS "$RUN_URL" >/dev/null
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
sql "update runs set fetched_at = fetched_at - 121000 where parent_id = $RUN_ID and seq = 1"
( sleep 20; kill "$RUN_PID" 2>/dev/null ) & guard=$!
finish
pkill -P "$guard" 2>/dev/null; kill "$guard" 2>/dev/null; wait "$guard" 2>/dev/null
eq "a lost execution makes the run exit 124 (Nebula codes win)" 124 "$RUN_RC"

section "404s are identical; single-use stays single-use under concurrency"
resp() { curl -si "$1" | tr -d '\r' | grep -vi '^date:' | sha256sum; }
reg used1 "$WORK/hello.sh"
curl -fsS "$RUN_URL" | sh >/dev/null 2>&1
finish
USED=$RUN_URL
reg exp1 "$WORK/hello.sh" --ttl 1
sleep 1.5
EXPIRED=$RUN_URL
finish
UNKNOWN_R=$(resp "$BASE/r/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
eq "used link: byte-identical 404" "$UNKNOWN_R" "$(resp "$USED")"
eq "expired link: byte-identical 404" "$UNKNOWN_R" "$(resp "$EXPIRED")"
eq "used-up multi-use link: byte-identical 404" "$UNKNOWN_R" "$(resp "$MURL")"
eq "no route: byte-identical 404" "$UNKNOWN_R" "$(resp "$BASE/nothing")"
race() { # race URL -> number of 200s among 50 parallel GETs
    rm -f "$WORK"/race.*
    for i in $(seq 1 50); do curl -s -o /dev/null -w '%{http_code}\n' "$1" >"$WORK/race.$i" & done
    wait $(jobs -p | grep -v "^$RUN_PID\$") 2>/dev/null
    cat "$WORK"/race.* | grep -c '^200$'
}
reg race1 "$WORK/hello.sh"
eq "50 parallel GETs of a single-use link: exactly one 200" 1 "$(race "$RUN_URL")"
"$NEBULA" cancel "$RUN_ID" >/dev/null; finish
reg race3 "$WORK/hello.sh" --uses 3
eq "50 parallel GETs of a 3-use link: exactly three 200" 3 "$(race "$RUN_URL")"
eq "three executions were recorded" 3 "$(sql "select count(*) from runs where parent_id=$RUN_ID")"
"$NEBULA" cancel "$RUN_ID" >/dev/null; finish
eq "cancelling the run cancels its executions" 3 "$(sql "select count(*) from runs where parent_id=$RUN_ID and state='cancelled'")"
eq "and nebula run exits 123" 123 "$RUN_RC"

if command -v tmux >/dev/null 2>&1; then
    section "tmux notice per execution"
    TS3="$WORK/tmux3.sock"
    tmux -S "$TS3" -f /dev/null new-session -d -s m -x 250 -y 50 cat
    TMUX="$TS3,1,0" reg mtmux "$WORK/hello.sh" --uses 2 --tmux=m
    "$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
    "$NEBULA" go "$RUN_ID" >/dev/null 2>&1 </dev/null
    finish
    mseen() { tmux -S "$TS3" capture-pane -p -J -t m | grep -qF -- "$1"; }
    wait_for 10 mseen "[nebula: done #$RUN_ID.1 \"mtmux\" exit:0]" && ok "notice for execution .1" || bad "notice for execution .1" "$(tmux -S "$TS3" capture-pane -p -J -t m | grep -v '^$')"
    wait_for 10 mseen "[nebula: done #$RUN_ID.2 \"mtmux\" exit:0]" && ok "notice for execution .2" || bad "notice for execution .2"
    hasnt_pane() { tmux -S "$TS3" capture-pane -p -J -t m | grep -qF "[nebula: done #$RUN_ID \"" ; }
    hasnt_pane && bad "a notice was sent for the run itself" || ok "no notice for the run itself"
    tmux -S "$TS3" kill-server
fi

section "slow link: a large output burst still arrives (chunk halving)"
SHIM="$WORK/slowcurl"
mkdir -p "$SHIM"
REALCURL=$(command -v curl)
printf '#!/bin/sh\nexec %s --limit-rate 6k "$@"\n' "$REALCURL" >"$SHIM/curl"
chmod +x "$SHIM/curl"
head -c 225000 /dev/urandom | od -An -v -tx1 | head -c 300000 >"$WORK/burst.txt"
S=$(mkscript burst <<EOF
cat '$WORK/burst.txt'
EOF
)
reg burst "$S"
t0=$(date +%s)
curl -fsS "$RUN_URL" | PATH="$SHIM:$PATH" sh >/dev/null 2>"$WORK/burst.err"
eq "far side exit 0 through a 6 KiB/s link" 0 $?
finish
eq "nebula run exit 0" 0 "$RUN_RC"
hasnt "the far side delivered everything" "$WORK/burst.err" 'could NOT be delivered'
eq "300 kB arrived byte-exact" "$(sha256sum <"$WORK/burst.txt")" "$("$NEBULA" output "$RUN_ID" | sha256sum)"
eq "the run was never declared lost" done "$(sql "select state from runs where id=$RUN_ID")"
echo "        (took $(( $(date +%s) - t0 )) s)"

section "shutdown"
kill "$DAEMON_PID"; wait "$DAEMON_PID" 2>/dev/null; DAEMON_PID=
"$NEBULA" status >"$WORK/status3.out" 2>&1
has "status says not running after stop" "$WORK/status3.out" 'daemon: +not running'
eq "nothing listens on the port any more" 000 "$(http_code "$BASE/")"

echo
echo "passed: $PASS   failed: $FAIL"
[ "$FAIL" = 0 ]
