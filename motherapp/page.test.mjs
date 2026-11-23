// Tests for the pure functions of page.ts, run under Node (tests/page.rs starts this file):
//   node [--experimental-strip-types] motherapp/page.test.mjs
// The page's default export is a custom element class; Node has no HTMLElement, so a stand-in
// is defined before the module is loaded. Nothing here touches the DOM.
import assert from "node:assert/strict";
import { test } from "node:test";

globalThis.HTMLElement = class {};
const { tokenizeShell, isShellScript, ansiFeed, stripAnsi } = await import("./page.ts");

const joined = (src) => tokenizeShell(src).map(([, text]) => text).join("");
const kinds = (src) => tokenizeShell(src).filter(([kind]) => kind !== "");
const KINDS = new Set(["", "c", "s", "v", "k", "f", "o"]);

/** The one promise: the pieces joined are the input. */
function roundTrip(src) {
  const tokens = tokenizeShell(src);
  assert.equal(tokens.map(([, text]) => text).join(""), src);
  for (const [kind, text] of tokens) {
    assert.ok(KINDS.has(kind), `kind ${kind}`);
    assert.notEqual(text, "");
  }
}

const NASTY = [
  "",
  "\n",
  "#",
  "#!/bin/sh\n",
  "echo hi",
  "echo 'unterminated",
  'echo "unterminated $(',
  'echo "unterminated ${',
  "echo `unterminated",
  "echo $",
  "echo $(",
  "echo ${",
  "echo \\",
  "\\",
  "'",
  '"',
  "`",
  "$",
  "$$$$",
  "<<",
  "<<-",
  "cat <<",
  "cat <<EOF",
  "cat <<EOF\n",
  "cat <<EOF\nnever closed\n$(still body)",
  "cat <<'EOF' <<-\tTWO\none $x\nEOF\n\t\ttwo\n\tTWO\necho after\n",
  "cat <<\\EOF\nbody\nEOF",
  "a <<< 'here string'\n",
  "))))((((",
  "}}}}{{{{",
  ";;;;;; ;; & && | || |&",
  "case",
  "case x in",
  "case $x in a|b) echo 1;; *) echo 2 ;; esac\n",
  "case \"$1\" in\n  -h|--help) usage; exit 0 ;;\n  (start) run ;;\n  *)\n    echo \"what: $1\" >&2\n    ;;\nesac\n",
  "for x in a b c; do echo \"$x\"; done\n",
  "for x; do :; done",
  "for",
  "if then else elif fi for while until do done case esac in function",
  "function f { echo x; }\nf() { echo y; }\n",
  "FOO=bar BAZ=\"q u x\" env X+=1 cmd arg\n",
  "x=$(( 1 + (2 * 3) ))\n",
  "echo \"a $(echo \"b $(echo 'c)' \"d\")\" )\" e\n",
  "echo \"${x:-\"def}ault\"}\" '${not}' \"\\$not \\\" still\"\n",
  "echo \"it's\" 'say \"hi\"' it\\'s\n",
  "echo `date +%s` \"`hostname`\" `echo \\`nested\\``\n",
  "cmd 2>&1 >/dev/null </dev/null 3<>f >|g >>h 2>>i &\n",
  "echo a#b # real comment\n#also\n\t# and this\n",
  "echo $1 $@ $* $# $? $$ $! $- ${10} $_x $x_y$z\n",
  "sudo -n true && echo ok || { echo no; exit 1; }\n",
  "( cd /tmp && ls ) | while read -r l; do printf '%s\\n' \"$l\"; done\n",
  "echo é€😀 'é' \"😀$x😀\"\n",
  "😀=1 \ud83d",
  "echo a\r\necho b\r\n",
  "echo a\\\n  b\\\n  c\n",
  "\u0000\u0001\u001b[31m\u007f",
  "\t\t  \t",
  "$(((",
  "$(echo $(echo $(echo $(echo $(echo $(echo $(echo $(echo $(echo $(echo deep))))))))))",
  "a=\"$(cat <<EOF\nin a substitution\nEOF\n)\"\n",
  "!",
  "! true",
  "time sleep 1",
  "{ echo; }",
  "[ -f x ] && [[ $a == b* ]]",
  "select x in a b; do break; done",
  "echo 'a\nb\nc' \"d\ne\"\n",
];

test("tokenizer: the pieces joined are the input, on nasty inputs", () => {
  for (const src of NASTY) roundTrip(src);
  // every prefix and every suffix of every one of them, too: a cut can land anywhere
  for (const src of NASTY) {
    for (let i = 0; i <= src.length; i++) {
      roundTrip(src.slice(0, i));
      roundTrip(src.slice(i));
    }
  }
});

test("tokenizer: random soup of shell punctuation", () => {
  const alphabet = ["'", '"', "`", "$", "(", ")", "{", "}", "\\", "\n", " ", "\t", "#", ";", "&", "|", "<", ">", "-", "=", "a", "if", "fi", "case", "esac", "in", "do", "done", "EOF", "<<", "$(", "${", "€", "😀", "\r", "1", "2>"];
  let seed = 12345;
  const rnd = () => (seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff;
  for (let n = 0; n < 3000; n++) {
    let src = "";
    const len = Math.floor(rnd() * 40);
    for (let i = 0; i < len; i++) src += alphabet[Math.floor(rnd() * alphabet.length)];
    roundTrip(src);
  }
});

test("tokenizer: stays fast on a large and on a hostile script", () => {
  const line = 'if [ "$x" = "a b" ]; then echo "value: ${x:-none} $(date)" >> /tmp/log 2>&1; fi # note\n';
  const big = line.repeat(Math.ceil((256 * 1024) / line.length));
  let t = performance.now();
  roundTrip(big);
  const normal = performance.now() - t;
  // openers that never close: each would search to the end if nothing stopped it
  for (const hostile of ["$(".repeat(130_000), '"$('.repeat(90_000), "${".repeat(130_000), "`".repeat(260_000), "'".repeat(260_001), "<<A ".repeat(60_000) + "\n", "((((".repeat(60_000)]) {
    t = performance.now();
    roundTrip(hostile);
    const took = performance.now() - t;
    assert.ok(took < 3000, `hostile input took ${Math.round(took)} ms`);
  }
  assert.ok(normal < 3000, `256 KiB took ${Math.round(normal)} ms`);
});

test("tokenizer: what gets which colour", () => {
  assert.deepEqual(kinds("#!/bin/sh\n# note\necho hi # there\n"), [
    ["c", "#!/bin/sh"],
    ["c", "# note"],
    ["f", "echo"],
    ["c", "# there"],
  ]);
  assert.deepEqual(kinds("echo a#b"), [["f", "echo"]]);
  assert.deepEqual(kinds(`echo 'it $x' "is $x and \${y:-z} and $(date +%s)!" $z`), [
    ["f", "echo"],
    ["s", "'it $x'"],
    ["s", '"is '],
    ["v", "$x"],
    ["s", " and "],
    ["v", "${y:-z}"],
    ["s", " and "],
    ["v", "$("],
    ["f", "date"],
    ["v", ")"],
    ["s", '!"'],
    ["v", "$z"],
  ]);
  assert.deepEqual(kinds("if true; then echo a; elif false; then :; else echo b; fi"), [
    ["k", "if"],
    ["f", "true"],
    ["o", ";"],
    ["k", "then"],
    ["f", "echo"],
    ["o", ";"],
    ["k", "elif"],
    ["f", "false"],
    ["o", ";"],
    ["k", "then"],
    ["f", ":"],
    ["o", ";"],
    ["k", "else"],
    ["f", "echo"],
    ["o", ";"],
    ["k", "fi"],
  ]);
  assert.deepEqual(kinds("for f in a b; do rm \"$f\"; done"), [
    ["k", "for"],
    ["k", "in"],
    ["o", ";"],
    ["k", "do"],
    ["f", "rm"],
    ["s", '"'],
    ["v", "$f"],
    ["s", '"'],
    ["o", ";"],
    ["k", "done"],
  ]);
  // a keyword is one only where a command could stand
  assert.deepEqual(kinds("echo if then done in"), [["f", "echo"]]);
  // patterns of a case are no commands
  assert.deepEqual(kinds("case $1 in\n  start|go) run it ;;\n  *) usage ;;\nesac\n"), [
    ["k", "case"],
    ["v", "$1"],
    ["k", "in"],
    ["o", "|"],
    ["o", ")"],
    ["f", "run"],
    ["o", ";;"],
    ["o", ")"],
    ["f", "usage"],
    ["o", ";;"],
    ["k", "esac"],
  ]);
  // assignments before a command, pipes, logic, redirections
  assert.deepEqual(kinds("LC_ALL=C sort <in >out 2>&1 | uniq -c && echo ok || exit 1 &"), [
    ["v", "LC_ALL"],
    ["f", "sort"],
    ["o", "<"],
    ["o", ">"],
    ["o", "2>&"],
    ["o", "|"],
    ["f", "uniq"],
    ["o", "&&"],
    ["f", "echo"],
    ["o", "||"],
    ["f", "exit"],
    ["o", "&"],
  ]);
  // a here-document is one string up to its end word; the command after it is a command again
  assert.deepEqual(kinds("cat <<EOF >f\n$x if 'y\nEOF\necho done\n"), [
    ["f", "cat"],
    ["o", "<<"],
    ["s", "EOF"],
    ["o", ">"],
    ["s", "$x if 'y\nEOF\n"],
    ["f", "echo"],
  ]);
  assert.deepEqual(kinds("cat <<-'E'\n\tbody\n\tE\nls"), [
    ["f", "cat"],
    ["o", "<<-"],
    ["s", "'E'"],
    ["s", "\tbody\n\tE\n"],
    ["f", "ls"],
  ]);
  assert.deepEqual(kinds("x=`uname -r` y=$((1+2))"), [
    ["v", "x"],
    ["v", "`uname -r`"],
    ["v", "y"],
    ["v", "$("],
    ["v", ")"],
  ]);
});

test("shebang: only a shell is coloured as shell", () => {
  for (const sh of ["", "echo hi\n", "#!/bin/sh\n", "#!/bin/bash -eu\n", "#! /usr/bin/env bash\n", "#!/usr/bin/env -S dash -e\n", "#!/bin/busybox sh\n", "#!/usr/bin/zsh", "#!\n", "#!/usr/bin/env VAR=1 sh\n"]) {
    assert.equal(isShellScript(sh), true, JSON.stringify(sh));
  }
  for (const other of ["#!/usr/bin/python3\nprint(1)\n", "#!/usr/bin/env python\n", "#!/usr/bin/perl -w\n", "#!/usr/bin/env node\n", "#!/usr/bin/env -S deno run\n", "#!/bin/sed -f\n", "#!/usr/bin/awk -f", "#!/usr/bin/ruby\n", "#!/opt/fish\n"]) {
    assert.equal(isShellScript(other), false, JSON.stringify(other));
  }
});

const texts = (runs) => runs.map((r) => r.text).join("");

test("ansi: colours, and nothing but the text is shown", () => {
  const st = {};
  assert.deepEqual(ansiFeed(st, "plain\n"), [{ text: "plain\n", cls: "" }]);
  assert.deepEqual(ansiFeed(st, "\x1b[31mred\x1b[0m \x1b[1;32mbold green\x1b[m\n"), [
    { text: "red", cls: "neb-a-f1" },
    { text: " ", cls: "" },
    { text: "bold green", cls: "neb-a-f2 neb-a-bold" },
    { text: "\n", cls: "" },
  ]);
  // bright, background, 256 colours, true colour, the rest of SGR
  assert.deepEqual(ansiFeed(st, "\x1b[93;44mA\x1b[38;5;196mB\x1b[48;2;1;2;3mC\x1b[38;5;4mD\x1b[0;3;4;9;2mE\x1b[7mF\x1b[0m"), [
    { text: "A", cls: "neb-a-f11 neb-a-b4" },
    { text: "B", cls: "neb-a-b4", fg: "#ff0000" },
    { text: "C", cls: "", fg: "#ff0000", bg: "#010203" },
    { text: "D", cls: "neb-a-f4", bg: "#010203" },
    { text: "E", cls: "neb-a-dim neb-a-ital neb-a-ul neb-a-strike" },
    { text: "F", cls: "neb-a-rev neb-a-dim neb-a-ital neb-a-ul neb-a-strike" },
  ]);
  // other sequences are dropped: cursor movement, erase, a window title, a hyperlink, charset
  const noisy = "a\x1b[2K\x1b[1Ab\x1b]0;title\x07c\x1b]8;;http://x\x1b\\d\x1b]8;;\x1b\\\x1b(Be\x1b[?25lf";
  assert.equal(texts(ansiFeed({}, noisy)), "abcdef");
  assert.equal(stripAnsi(noisy), "abcdef");
  assert.equal(stripAnsi("\x1b[31mred\x1b[0m"), "red");
  // text that only looks like markup is text
  assert.deepEqual(ansiFeed({}, "<b>&amp;</b>"), [{ text: "<b>&amp;</b>", cls: "" }]);
});

test("ansi: fed in pieces gives what the whole gives", () => {
  const whole = "start \x1b[1;38;2;10;20;30mcoloured\x1b[0m mid \x1b]0;a title\x07 \x1b[4munder\x1b[24m end\x1b[31m\nred line\n\x1b[0m";
  const flat = (runs) => runs.flatMap((r) => [...r.text].map((ch) => JSON.stringify([ch, r.cls, r.fg, r.bg])));
  const expected = flat(ansiFeed({}, whole));
  for (let size = 1; size <= 9; size++) {
    const st = {};
    const got = [];
    for (let i = 0; i < whole.length; i += size) got.push(...flat(ansiFeed(st, whole.slice(i, i + size))));
    assert.deepEqual(got, expected, `pieces of ${size}`);
    assert.equal(st.rest, "");
  }
  // the colour in force carries over to the next piece
  const st = {};
  ansiFeed(st, "\x1b[32mgreen ");
  assert.deepEqual(ansiFeed(st, "still green"), [{ text: "still green", cls: "neb-a-f2" }]);
  // a lone ESC at the very end waits; an endless "sequence" does not hold text back for ever
  const cut = {};
  assert.equal(texts(ansiFeed(cut, "x\x1b")), "x");
  assert.equal(cut.rest, "\x1b");
  assert.deepEqual(ansiFeed(cut, "[35my"), [{ text: "y", cls: "neb-a-f5" }]);
  const stuck = {};
  ansiFeed(stuck, "\x1b]0;" + "t".repeat(1000));
  assert.equal(stuck.rest, "");
});
