/// <reference lib="dom" />
// Nebula's page in motherapp (a light-DOM custom element; motherapp's docs/extension-contract.md
// § 6). `nebula motherapp install` writes this file next to the manifest, and motherapp's core
// serves it with the types stripped and nothing else: no bundler and no module resolution. So it
// is one self-contained file, its only imports are type imports, and it uses no TypeScript
// syntax that needs compiling.
//
// The run list on one side, the selected run with its script and output on the other, stacked
// on a narrow screen. The list comes from state, which `nebula motherapp serve` keeps current.
// Output, script and the one-liner come from the `view` and `oneLiner` actions, which are local
// only: motherapp's core refuses them for another machine, and then this page shows the list
// and the states and nothing of the contents.
//
// The script is shown with shell syntax colours (a small tokenizer below; a wrong guess costs a
// colour, never text) and the output with its ANSI SGR colours; other escape sequences are
// dropped from display. Colours are the shared theme's variables, so light and dark both work.
//
// Everything from a run goes in through textContent, never as HTML.
//
// The pure functions are exported so that motherapp/page.test.mjs can test them under Node;
// the shell only uses the default export.
import type { JsonValue } from "@motherapp/protocol";
import type { PageContext } from "@motherapp/protocol/page";

/** One run as published in state (src/motherapp/state.rs). Keyed by row id. */
interface Run {
  label: string;
  target: string;
  state: string;
  bytes: number;
  created: number;
  expires: number;
  parent?: number;
  seq?: number;
  exit?: number;
  reported?: number;
  fetched?: number;
  started?: number;
  seen?: number;
  finished?: number;
  name?: string;
  host?: string;
  user?: string;
  truncated?: boolean;
  deleted?: boolean;
  multi?: boolean;
  uses?: number;
  fetches?: number;
}

interface Details {
  sha256?: string;
  echo?: boolean;
  deleteAfter?: boolean;
  ttlSecs?: number;
  staleSecs?: number;
  note?: string;
  via?: string;
  onDone?: string;
  tmux?: string;
  hook?: string;
  /** Where the link was fetched from, as the request said: untrusted. */
  fetchAddr?: string;
  fetchCountry?: string;
  fetchAgent?: string;
  /** `uname -srm` of the machine that ran it, as its wrapper reported. */
  os?: string;
  tty?: string;
}

interface ViewResult {
  id: string;
  state: string;
  bytes: number;
  from: number;
  next: number;
  cap: number;
  text: string;
  deleted?: boolean;
  truncated?: boolean;
  script?: string;
  scriptCut?: boolean;
  scriptNext?: number;
  scriptBytes?: number;
  scriptMissing?: boolean;
  details?: Details;
}

/** motherapp's code for a local-only action called from another machine. */
const LOCAL_ONLY = -32004;
const VIEW_MS = 1500;
const WIDE_PX = 820;
/** Within this of the bottom counts as at the bottom. */
const BOTTOM_PX = 24;
/** Most characters of output kept on the page; the oldest go first. */
const MAX_SHOWN = 2_000_000;
const OPEN_STATES = ["pending", "fetched", "running"];
/** Most output "copy output" fetches; of a longer one, the newest part. */
const COPY_MAX = 8 * 1024 * 1024;
const COPIED_MS = 1500;

const STYLE = `
.neb { display: grid; gap: 12px; color: var(--fg); container-type: inline-size; min-width: 0; }
.neb-bar { display: flex; flex-wrap: wrap; align-items: baseline; gap: 4px 14px; min-height: 24px; font-size: 0.875rem; color: var(--muted); }
.neb-health { display: inline-flex; align-items: center; gap: 6px; }
.neb-health[data-level="warn"] { color: var(--fg); padding: 4px 10px; border: 1px solid var(--warn); border-radius: 8px; }
.neb-health[data-level="err"] { color: var(--fg); padding: 4px 10px; border: 1px solid var(--err); border-radius: 8px; }
.neb-grid { display: grid; gap: 16px; grid-template-columns: minmax(0, 1fr); }
@container (min-width: ${WIDE_PX}px) {
  .neb-grid { grid-template-columns: minmax(280px, 360px) minmax(0, 1fr); align-items: start; }
  .neb-list { max-height: calc(100vh - var(--page-header-h, 0px) - 110px); overflow-y: auto; padding-right: 4px; }
}
.neb-list { display: grid; gap: 6px; min-width: 0; align-content: start; }
.neb-list[data-stale="true"] { opacity: 0.6; }
.neb-empty { color: var(--muted); padding: 12px; }
.neb-group { padding: 6px 2px 0; font-size: 0.8125rem; font-weight: 600; color: var(--muted); }
.neb-group:first-child { padding-top: 0; }
.neb-row { display: grid; gap: 3px; width: 100%; min-width: 0; padding: 8px 10px; text-align: start; font: inherit; color: var(--fg); cursor: pointer;
  background: var(--surface); border: 1px solid var(--border); border-radius: 8px; }
.neb-row:hover { border-color: var(--muted); }
.neb-row:focus-visible, .neb-btn:focus-visible { outline: 2px solid var(--accent); outline-offset: 1px; }
/* The selected run is tinted all over; no bar on one side. */
.neb-row[aria-current="true"] { background: color-mix(in srgb, var(--accent) 18%, var(--surface)); }
.neb-row[data-exec="true"] { width: auto; margin-inline-start: 20px; }
.neb-top, .neb-mid { display: flex; align-items: baseline; gap: 8px; min-width: 0; }
.neb-id { flex: none; font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 0.8125rem; color: var(--muted); }
.neb-name { font-weight: 600; flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.neb-name[data-unnamed="true"] { font-weight: 400; color: var(--muted); }
.neb-state { flex: none; display: inline-flex; align-items: center; gap: 5px; font-size: 0.8125rem; color: var(--muted); white-space: nowrap; }
.neb-dot { width: 8px; height: 8px; border-radius: 50%; border: 2px solid var(--muted); box-sizing: border-box; flex: none; }
[data-tone="wait"] > .neb-dot { border-color: var(--warn); }
[data-tone="live"] > .neb-dot { background: var(--ok); border-color: var(--ok); }
[data-tone="ok"] > .neb-dot { background: var(--muted); }
[data-tone="bad"] > .neb-dot { background: var(--err); border-color: var(--err); }
.neb-state[data-tone="wait"], .neb-state[data-tone="live"], .neb-state[data-tone="bad"] { color: var(--fg); }
.neb-sub { flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--muted); font-size: 0.8125rem; }
.neb-age { flex: none; font-size: 0.75rem; color: var(--muted); font-variant-numeric: tabular-nums; }
.neb-main { display: grid; gap: 10px; min-width: 0; align-content: start; scroll-margin-top: calc(var(--page-header-h, 0px) + 8px); }
.neb-card { background: var(--surface); border: 1px solid var(--border); border-radius: 8px; padding: 10px 14px; min-width: 0; }
.neb-head { display: flex; flex-wrap: wrap; align-items: baseline; gap: 4px 10px; }
.neb-head h3 { margin: 0; font-size: 1.05rem; overflow-wrap: anywhere; min-width: 0; }
.neb-facts { display: grid; grid-template-columns: max-content minmax(0, 1fr); gap: 3px 12px; margin: 8px 0 0; font-size: 0.875rem; }
.neb-facts dt { color: var(--muted); }
.neb-facts dd { margin: 0; min-width: 0; overflow-wrap: anywhere; white-space: pre-wrap; }
.neb-actions { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; margin-top: 10px; }
.neb-actions:empty { display: none; }
.neb-actions > div { display: contents; }
.neb-btn { padding: 6px 12px; border: 1px solid var(--border); border-radius: 6px; background: var(--surface); color: var(--fg); cursor: pointer; font: inherit; white-space: nowrap; }
.neb-btn:hover { border-color: var(--accent); }
.neb-btn:disabled { opacity: 0.5; cursor: default; }
.neb-note { margin: 6px 0 0; font-size: 0.875rem; color: var(--muted); overflow-wrap: anywhere; }
.neb-note[data-error="true"] { color: var(--err); }
.neb-line { margin: 8px 0 0; padding: 6px 8px; width: 100%; box-sizing: border-box; resize: none; border: 1px solid var(--border); border-radius: 6px; background: var(--bg); color: var(--fg);
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 0.8125rem; line-height: 1.35; overflow-wrap: anywhere; }
.neb-execs { display: grid; gap: 6px; }
.neb-execs h4, .neb-outbar h4 { margin: 0; font-size: 0.875rem; font-weight: 600; color: var(--fg); }
.neb-script summary { cursor: pointer; font-size: 0.875rem; font-weight: 600; }
.neb-pre { margin: 0; padding: 8px 10px; overflow: auto; border-radius: 8px; border: 1px solid var(--border); background: var(--bg); color: var(--fg);
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; font-size: 0.8125rem; line-height: 1.35; white-space: pre-wrap; overflow-wrap: anywhere; tab-size: 8; }
.neb-script .neb-pre { margin-top: 8px; max-height: 40vh; }
/* Narrow: the heading and the copy button on one line, the status under them. */
.neb-outbar { display: flex; flex-wrap: wrap; align-items: center; gap: 4px 10px; min-height: 24px; font-size: 0.8125rem; color: var(--muted); }
.neb-outbar > .neb-copybox { margin-inline-start: auto; }
.neb-outbar > span:not(.neb-copybox) { order: 3; flex: 1 1 100%; min-width: 0; }
@container (min-width: ${WIDE_PX}px) { .neb-outbar { flex-wrap: nowrap; } .neb-outbar > span:not(.neb-copybox) { order: 0; flex: 1 1 0; } }
.neb-out { min-height: 120px; max-height: 60vh; }
@container (min-width: ${WIDE_PX}px) { .neb-out { height: 52vh; max-height: none; } }
.neb-script { position: relative; }
.neb-script summary { padding-inline-end: 40px; }
.neb-copybox { display: inline-flex; align-items: center; gap: 8px; flex: none; }
.neb-script > .neb-copybox { position: absolute; top: 6px; inset-inline-end: 8px; }
.neb-copied { font-size: 0.8125rem; color: var(--muted); }
.neb-copied[data-error="true"] { color: var(--err); }
.neb-icon { display: inline-flex; align-items: center; justify-content: center; width: 28px; height: 28px; padding: 0; flex: none; border: 1px solid var(--border); border-radius: 6px;
  background: var(--surface); color: var(--muted); cursor: pointer; }
.neb-icon:hover { border-color: var(--accent); color: var(--fg); }
.neb-icon:focus-visible { outline: 2px solid var(--accent); outline-offset: 1px; }
.neb-icon:disabled { opacity: 0.5; cursor: default; }
.neb-icon svg { width: 16px; height: 16px; fill: none; stroke: currentColor; stroke-width: 2; stroke-linecap: round; stroke-linejoin: round; }
.neb-icon .neb-i-done, .neb-icon[data-state="copied"] .neb-i-copy { display: none; }
.neb-icon[data-state="copied"] .neb-i-done { display: block; }
.neb-icon[data-state="copied"] { color: var(--ok); border-color: var(--ok); }
/* Syntax and ANSI colours: the theme's text colours (each a light/dark pair), drawn a little
   towards the text colour so that every one of them reads on the box's background (measured
   in both schemes: 4.5:1 or better). */
.neb { --neb-red: color-mix(in oklab, var(--err) 85%, var(--fg)); --neb-green: color-mix(in oklab, var(--ok) 72%, var(--fg)); --neb-yellow: color-mix(in oklab, var(--warn) 68%, var(--fg));
  --neb-blue: color-mix(in oklab, var(--bs-primary-fg, var(--accent)) 85%, var(--fg)); --neb-violet: var(--bs-accent-fg, var(--accent)); --neb-cyan: color-mix(in oklab, var(--bs-info-fg, var(--accent)) 62%, var(--fg)); }
.neb-t-c { color: var(--muted); font-style: italic; }
.neb-t-s { color: var(--neb-green); }
.neb-t-v { color: var(--neb-cyan); }
.neb-t-k { color: var(--neb-violet); font-weight: 600; }
.neb-t-f { color: var(--neb-blue); }
.neb-t-o { color: var(--neb-yellow); }
/* ANSI colours 0-7 (and their bright twins 8-15) in the same terms. */
.neb-a-f0, .neb-a-f8 { color: var(--muted); }
.neb-a-f1, .neb-a-f9 { color: var(--neb-red); }
.neb-a-f2, .neb-a-f10 { color: var(--neb-green); }
.neb-a-f3, .neb-a-f11 { color: var(--neb-yellow); }
.neb-a-f4, .neb-a-f12 { color: var(--neb-blue); }
.neb-a-f5, .neb-a-f13 { color: var(--neb-violet); }
.neb-a-f6, .neb-a-f14 { color: var(--neb-cyan); }
.neb-a-f7, .neb-a-f15 { color: var(--fg); }
/* A background is a light wash of its colour, so any text colour still reads on it. */
.neb-a-b0, .neb-a-b8 { background: color-mix(in srgb, var(--muted) 16%, transparent); }
.neb-a-b1, .neb-a-b9 { background: color-mix(in srgb, var(--err) 16%, transparent); }
.neb-a-b2, .neb-a-b10 { background: color-mix(in srgb, var(--ok) 16%, transparent); }
.neb-a-b3, .neb-a-b11 { background: color-mix(in srgb, var(--warn) 16%, transparent); }
.neb-a-b4, .neb-a-b12 { background: color-mix(in srgb, var(--bs-primary-fg, var(--accent)) 16%, transparent); }
.neb-a-b5, .neb-a-b13 { background: color-mix(in srgb, var(--bs-accent-fg, var(--accent)) 16%, transparent); }
.neb-a-b6, .neb-a-b14 { background: color-mix(in srgb, var(--bs-info-fg, var(--accent)) 16%, transparent); }
.neb-a-b7, .neb-a-b15 { background: color-mix(in srgb, var(--fg) 10%, transparent); }
.neb-a-bold { font-weight: 700; }
.neb-a-dim { opacity: 0.65; }
.neb-a-ital { font-style: italic; }
.neb-a-ul { text-decoration: underline; }
.neb-a-strike { text-decoration: line-through; }
.neb-a-ul.neb-a-strike { text-decoration: underline line-through; }
.neb-a-rev { background: var(--fg); color: var(--bg); }
.neb [hidden] { display: none; }
@media (max-width: 720px) { .neb-facts { grid-template-columns: minmax(0, 1fr); } .neb-facts dd { margin-bottom: 4px; } }
`;

function el<K extends keyof HTMLElementTagNameMap>(tag: K, props: { class?: string; text?: string } = {}, ...kids: (Node | string)[]): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  if (props.class) e.className = props.class;
  if (props.text !== undefined) e.textContent = props.text;
  e.append(...kids);
  return e;
}

const asRuns = (v: JsonValue | undefined): Record<string, Run> => (v && typeof v === "object" && !Array.isArray(v) ? (v as unknown as Record<string, Run>) : {});
/** A confirm outside a list has no row to fill in, so `{word}` in a name must stay as it is. */
const noPlaceholders = (s: string) => s.replace(/\{/g, "{​");
const isOpen = (r: Run) => OPEN_STATES.includes(r.state);

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(n < 10 * 1024 ? 1 : 0)} KiB`;
  return `${(n / 1024 / 1024).toFixed(n < 10 * 1024 * 1024 ? 1 : 0)} MiB`;
}

function fmtSpan(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 120) return `${s} s`;
  if (s < 7200) return `${Math.floor(s / 60)} min`;
  if (s < 172800) return `${Math.floor(s / 3600)} h`;
  return `${Math.floor(s / 86400)} d`;
}

function fmtTime(ms: number): string {
  const d = new Date(ms);
  const sameDay = d.toDateString() === new Date().toDateString();
  const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
  return sameDay ? time : `${d.toLocaleDateString()} ${time}`;
}

/** The state in words, and how loud it is: wait (on the human), live, ok, bad, or quiet. */
function stateOf(r: Run): { text: string; tone: string } {
  switch (r.state) {
    case "pending":
      if (r.multi) return { text: `link open, ${usesText(r)}`, tone: "wait" };
      return { text: "waiting for the paste", tone: "wait" };
    case "fetched":
      return { text: "fetched", tone: "live" };
    case "running":
      return { text: "running", tone: "live" };
    case "done":
      return r.exit === undefined || r.exit === 0 ? { text: "finished", tone: "ok" } : { text: `failed, exit ${r.exit}`, tone: "bad" };
    case "lost":
      return { text: "lost", tone: "bad" };
    case "closed":
      return { text: `link closed, ${usesText(r)}`, tone: "quiet" };
    default:
      return { text: r.state, tone: "quiet" };
  }
}

function usesText(r: Run): string {
  const n = r.fetches ?? 0;
  return r.uses ? `${n} of ${r.uses} used` : `${n} used`;
}

const titleOf = (r: Run) => r.name ?? (r.parent !== undefined ? "execution" : `${r.target} run`);

// ---- shell syntax ----

const SH_KEYWORDS = new Set(["if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "function", "select", "time", "{", "}", "!"]);
/** Keywords after which a command follows. */
const SH_BEFORE_COMMAND = new Set(["if", "then", "else", "elif", "while", "until", "do", "{", "!", "time"]);
const SH_SHELLS = new Set(["sh", "bash", "dash", "ash", "zsh", "ksh", "mksh", "busybox"]);

/**
 * False when the first line is a `#!` naming something that clearly is no shell (python, perl,
 * node, ...). No shebang counts as shell: Nebula runs every script with `sh`.
 */
export function isShellScript(src: string): boolean {
  if (!src.startsWith("#!")) return true;
  const nl = src.indexOf("\n");
  const words = src
    .slice(2, nl < 0 ? Math.min(src.length, 200) : Math.min(nl, 200))
    .trim()
    .split(/\s+/);
  let name = (words[0] ?? "").split("/").pop() ?? "";
  if (name === "env") name = (words.slice(1).find((w) => !w.startsWith("-") && !w.includes("=")) ?? "").split("/").pop() ?? "";
  return name === "" || SH_SHELLS.has(name);
}

/**
 * Splits a POSIX shell script into `[kind, text]` pieces for colouring. Kinds: "" plain,
 * "c" comment, "s" string or here-document, "v" variable or substitution, "k" keyword,
 * "f" the command word of a simple command, "o" operator or redirection.
 *
 * It is a colouring aid, not a parser. The one promise: the texts joined are the input,
 * character for character, whatever the input is. Every step below takes a slice of `src`
 * that starts where the last one ended.
 */
export function tokenizeShell(src: string): [string, string][] {
  const out: [string, string][] = [];
  const push = (kind: string, from: number, to: number): void => {
    if (to <= from) return;
    const last = out[out.length - 1];
    if (last && last[0] === kind) last[1] += src.slice(from, to);
    else out.push([kind, src.slice(from, to)]);
  };
  // Looking for a closing bracket that never comes must not be repeated for every opener.
  let budget = src.length * 4 + 1000;
  const closing = (from: number, end: number, open: string, close: string): number => {
    let depth = 1;
    for (let j = from; j < end; j++) {
      if (--budget < 0) return -1;
      const ch = src[j];
      if (ch === "\\") j++;
      else if (ch === "'") {
        const q = src.indexOf("'", j + 1);
        if (q < 0 || q >= end) return -1;
        budget -= q - j;
        j = q;
      } else if (ch === open) depth++;
      else if (ch === close && --depth === 0) return j;
    }
    return -1;
  };
  /** The end of a backtick substitution starting at `i`, or -1. */
  const backtick = (i: number, end: number): number => {
    for (let j = i + 1; j < end; j++) {
      if (src[j] === "\\") j++;
      else if (src[j] === "`") return j;
    }
    return -1;
  };
  const NAME = /[A-Za-z_][A-Za-z0-9_]*|[0-9@*#?$!-]/y;
  /** `$name`, `${...}`, `$(...)` at `i`; anything else is one character of `fallback`. Returns the next index. */
  const dollar = (i: number, end: number, depth: number, fallback: string): number => {
    const n = src[i + 1];
    if (n === "(" && i + 1 < end) {
      const j = depth < 8 ? closing(i + 2, end, "(", ")") : -1;
      if (j >= 0) {
        push("v", i, i + 2);
        if (src[i + 2] === "(") push("", i + 2, j);
        else scan(i + 2, j, depth + 1);
        push("v", j, j + 1);
        return j + 1;
      }
    } else if (n === "{" && i + 1 < end) {
      const j = closing(i + 2, end, "{", "}");
      if (j >= 0) {
        push("v", i, j + 1);
        return j + 1;
      }
    } else {
      NAME.lastIndex = i + 1;
      const m = NAME.exec(src);
      if (m && NAME.lastIndex <= end) {
        push("v", i, NAME.lastIndex);
        return NAME.lastIndex;
      }
    }
    push(fallback, i, i + 1);
    return i + 1;
  };
  /** A double-quoted string starting at `i`, with the expansions inside it. Returns the next index. */
  const quoted = (i: number, end: number, depth: number): number => {
    let seg = i;
    let j = i + 1;
    while (j < end) {
      const ch = src[j];
      if (ch === "\\") j += 2;
      else if (ch === '"') {
        j++;
        break;
      } else if (ch === "$") {
        push("s", seg, j);
        j = dollar(j, end, depth, "s");
        seg = j;
      } else if (ch === "`") {
        const k = backtick(j, end);
        if (k < 0) j++;
        else {
          push("s", seg, j);
          push("v", j, k + 1);
          j = k + 1;
          seg = j;
        }
      } else j++;
    }
    j = Math.min(j, end);
    push("s", seg, j);
    return j;
  };
  const OPERATOR = /\d*(?:<<-|<<<|<<|>>|>&|<&|<>|>\||>|<)|;;|&&|\|\||[;&|()]/y;
  const WORD = /[^\s;&|()<>'"`$\\]+/y;
  const BLANK = /[ \t\r]+/y;
  const ASSIGN = /^[A-Za-z_][A-Za-z0-9_]*\+?=/;

  function scan(start: number, end: number, depth: number): void {
    let i = start;
    /** The next word is the command of a simple command. */
    let cmd = true;
    /** In `NAME=value`, until the next blank: the value is not a command. */
    let assign = false;
    /** The next word is the target of a redirection. */
    let redirect = false;
    /** Words to go in which `in` is the keyword of `for`, `case` or `select`. */
    let expectIn = 0;
    let caseIn = false;
    /** Where a `case` pattern stands: its words are no commands. */
    let pattern = false;
    const heredocs: { word: string; tabs: boolean }[] = [];
    /** A quote or expansion was a piece of a word. */
    const piece = (): void => {
      if (redirect) redirect = false;
      else if (cmd && !assign && !pattern) cmd = false;
    };
    while (i < end) {
      const c = src[i]!;
      if (c === "\n") {
        push("", i, i + 1);
        i++;
        cmd = true;
        assign = false;
        redirect = false;
        while (heredocs.length > 0 && i < end) {
          const h = heredocs.shift()!;
          const body = i;
          for (;;) {
            let nl = src.indexOf("\n", i);
            if (nl < 0 || nl >= end) nl = end;
            let line = src.slice(i, nl);
            if (h.tabs) line = line.replace(/^\t+/, "");
            i = Math.min(nl + 1, end);
            if (line === h.word || nl >= end) break;
          }
          push("s", body, i);
        }
        continue;
      }
      BLANK.lastIndex = i;
      if (BLANK.test(src)) {
        const to = Math.min(BLANK.lastIndex, end);
        push("", i, to);
        i = to;
        assign = false;
        continue;
      }
      if (c === "#" && (i === start || /[\s;&|()]/.test(src[i - 1]!))) {
        let nl = src.indexOf("\n", i);
        if (nl < 0 || nl > end) nl = end;
        push("c", i, nl);
        i = nl;
        continue;
      }
      if (c === "\\") {
        const to = Math.min(i + 2, end);
        push("", i, to);
        if (src[i + 1] !== "\n") piece();
        i = to;
        continue;
      }
      if (c === "'") {
        let q = src.indexOf("'", i + 1);
        if (q < 0 || q >= end) q = end - 1;
        push("s", i, q + 1);
        i = q + 1;
        piece();
        continue;
      }
      if (c === '"') {
        i = quoted(i, end, depth);
        piece();
        continue;
      }
      if (c === "`") {
        const k = backtick(i, end);
        if (k < 0) {
          push("", i, i + 1);
          i++;
        } else {
          push("v", i, k + 1);
          i = k + 1;
        }
        piece();
        continue;
      }
      if (c === "$") {
        i = dollar(i, end, depth, "");
        piece();
        continue;
      }
      OPERATOR.lastIndex = i;
      const op = OPERATOR.exec(src);
      if (op && OPERATOR.lastIndex <= end) {
        const text = op[0];
        push("o", i, OPERATOR.lastIndex);
        i = OPERATOR.lastIndex;
        expectIn = 0;
        if (/<<-?$/.test(text) && !text.endsWith("<<<")) {
          // The word after it ends the here-document; its body starts on the next line.
          BLANK.lastIndex = i;
          if (BLANK.test(src) && BLANK.lastIndex <= end) {
            push("", i, BLANK.lastIndex);
            i = BLANK.lastIndex;
          }
          let j = i;
          while (j < end && !/[\s;&|()<>]/.test(src[j]!)) j++;
          const word = src.slice(i, j).replace(/['"\\]/g, "");
          if (word !== "") heredocs.push({ word, tabs: text.endsWith("-") });
          push("s", i, j);
          i = j;
        } else if (/[<>]/.test(text)) redirect = true;
        else {
          if (text === ";;") pattern = true;
          else if (text === ")") pattern = false;
          cmd = true;
          assign = false;
        }
        continue;
      }
      WORD.lastIndex = i;
      const w = WORD.exec(src);
      if (!w || WORD.lastIndex > end) {
        push("", i, i + 1);
        i++;
        continue;
      }
      const word = w[0];
      const to = WORD.lastIndex;
      if (redirect) {
        redirect = false;
        push("", i, to);
      } else if (pattern) {
        if (cmd && word === "esac") {
          pattern = false;
          cmd = false;
          push("k", i, to);
        } else {
          cmd = false;
          push("", i, to);
        }
      } else if (expectIn > 0 && word === "in") {
        expectIn = 0;
        pattern = caseIn;
        cmd = false;
        push("k", i, to);
      } else if (cmd && !assign) {
        if (SH_KEYWORDS.has(word)) {
          push("k", i, to);
          cmd = SH_BEFORE_COMMAND.has(word);
          if (word === "for" || word === "case" || word === "select") {
            expectIn = 3;
            caseIn = word === "case";
          }
        } else {
          const a = ASSIGN.exec(word);
          if (a) {
            const eq = i + a[0].length - 1;
            push("v", i, eq);
            push("", eq, to);
            assign = true;
          } else {
            push("f", i, to);
            cmd = false;
          }
        }
      } else push("", i, to);
      if (expectIn > 0) expectIn--;
      i = to;
    }
  }
  scan(0, src.length, 0);
  return out;
}

// ---- ANSI colours in output ----

/** A colour: 0-15 is one of the sixteen (shown in the theme's colours), a string is a CSS colour. */
type AnsiColor = number | string | undefined;

export interface AnsiState {
  fg?: AnsiColor;
  bg?: AnsiColor;
  bold?: boolean;
  dim?: boolean;
  italic?: boolean;
  underline?: boolean;
  strike?: boolean;
  reverse?: boolean;
  /** The start of an escape sequence whose rest has not arrived yet. */
  rest?: string;
}

export interface AnsiRun {
  text: string;
  /** Class names, "" for plain text. */
  cls: string;
  fg?: string;
  bg?: string;
}

const hex2 = (n: number) => (n & 255).toString(16).padStart(2, "0");
const rgb = (r: number, g: number, b: number) => `#${hex2(r)}${hex2(g)}${hex2(b)}`;

function color256(n: number): AnsiColor {
  if (!Number.isInteger(n) || n < 0 || n > 255) return undefined;
  if (n < 16) return n;
  if (n >= 232) {
    const v = 8 + (n - 232) * 10;
    return rgb(v, v, v);
  }
  const level = (i: number) => (i === 0 ? 0 : 55 + i * 40);
  const i = n - 16;
  return rgb(level(Math.floor(i / 36)), level(Math.floor(i / 6) % 6), level(i % 6));
}

/** 38;5;n or 38;2;r;g;b, starting at codes[i] (the 5 or 2). Returns the colour and the codes used. */
function extendedColor(codes: number[], i: number): [AnsiColor, number] {
  if (codes[i] === 5) return [color256(codes[i + 1] ?? -1), 2];
  if (codes[i] === 2) {
    const [r, g, b] = [codes[i + 1], codes[i + 2], codes[i + 3]];
    return [r !== undefined && g !== undefined && b !== undefined ? rgb(r, g, b) : undefined, 4];
  }
  return [undefined, 0];
}

function applySgr(pen: AnsiState, params: string): void {
  // "4:3" (curly underline) and the like: the first number says what it is.
  const codes = (params === "" ? ["0"] : params.split(";")).map((p) => Number(p.split(":")[0] || "0"));
  for (let i = 0; i < codes.length; i++) {
    const c = codes[i]!;
    if (c === 0) {
      pen.fg = pen.bg = undefined;
      pen.bold = pen.dim = pen.italic = pen.underline = pen.strike = pen.reverse = false;
    } else if (c === 1) pen.bold = true;
    else if (c === 2) pen.dim = true;
    else if (c === 3) pen.italic = true;
    else if (c === 4) pen.underline = true;
    else if (c === 7) pen.reverse = true;
    else if (c === 9) pen.strike = true;
    else if (c === 22) pen.bold = pen.dim = false;
    else if (c === 23) pen.italic = false;
    else if (c === 24) pen.underline = false;
    else if (c === 27) pen.reverse = false;
    else if (c === 29) pen.strike = false;
    else if (c >= 30 && c <= 37) pen.fg = c - 30;
    else if (c >= 90 && c <= 97) pen.fg = c - 90 + 8;
    else if (c >= 40 && c <= 47) pen.bg = c - 40;
    else if (c >= 100 && c <= 107) pen.bg = c - 100 + 8;
    else if (c === 39) pen.fg = undefined;
    else if (c === 49) pen.bg = undefined;
    else if (c === 38 || c === 48) {
      const [value, used] = extendedColor(codes, i + 1);
      if (c === 38) pen.fg = value;
      else pen.bg = value;
      i += used;
    }
  }
}

function ansiRun(text: string, pen: AnsiState): AnsiRun {
  const cls: string[] = [];
  const run: AnsiRun = { text, cls: "" };
  // Reversed text is shown as the theme's own inverse, whatever its colours were.
  if (pen.reverse) cls.push("neb-a-rev");
  else {
    if (typeof pen.fg === "number") cls.push(`neb-a-f${pen.fg}`);
    else if (pen.fg) run.fg = pen.fg;
    if (typeof pen.bg === "number") cls.push(`neb-a-b${pen.bg}`);
    else if (pen.bg) run.bg = pen.bg;
  }
  if (pen.bold) cls.push("neb-a-bold");
  if (pen.dim) cls.push("neb-a-dim");
  if (pen.italic) cls.push("neb-a-ital");
  if (pen.underline) cls.push("neb-a-ul");
  if (pen.strike) cls.push("neb-a-strike");
  run.cls = cls.join(" ");
  return run;
}

// CSI, OSC (window titles, hyperlinks; ended by BEL or ST), an escape with intermediate bytes
// (character sets, "ESC ( B"), and any other two-byte escape.
const ANSI_ESCAPE = /\x1b(?:\[([0-9;:?<=>]*)([@-~])|\][^\x07\x1b]*(?:\x07|\x1b\\)?|[ -/]+[0-~]|[^[\]])/g;
const ANSI_COMPLETE = /^\x1b(?:\[[0-9;:?<=>]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[ -/]+[0-~]|[^[\] -/])/;
/** An escape sequence cut off at the end of a piece waits for the next piece, up to this length. */
const ANSI_REST_MAX = 256;

/**
 * The next piece of output as styled runs. `state` carries the colours, and the start of an
 * escape sequence cut off at the end of `chunk`, over to the next call, so output can be fed
 * as it arrives. Only SGR (colours, bold, ...) is understood; every other escape sequence is
 * dropped. The runs' texts are for textContent.
 */
export function ansiFeed(state: AnsiState, chunk: string): AnsiRun[] {
  let text = (state.rest ?? "") + chunk;
  state.rest = "";
  const esc = text.lastIndexOf("\x1b");
  if (esc >= 0 && text.length - esc <= ANSI_REST_MAX && !ANSI_COMPLETE.test(text.slice(esc))) {
    state.rest = text.slice(esc);
    text = text.slice(0, esc);
  }
  const runs: AnsiRun[] = [];
  let at = 0;
  const push = (end: number): void => {
    if (end <= at) return;
    const run = ansiRun(text.slice(at, end), state);
    const last = runs[runs.length - 1];
    if (last && last.cls === run.cls && last.fg === run.fg && last.bg === run.bg) last.text += run.text;
    else runs.push(run);
  };
  for (const m of text.matchAll(ANSI_ESCAPE)) {
    push(m.index);
    at = m.index + m[0].length;
    if (m[2] === "m") applySgr(state, m[1] ?? "");
  }
  push(text.length);
  return runs;
}

/** Output as plain text: what `ansiFeed` would show, without the styles. */
export function stripAnsi(text: string): string {
  return text.replace(ANSI_ESCAPE, "");
}

// ---- clipboard ----

/**
 * Puts text on the clipboard. The text may still be on its way: handing the browser a promise
 * right in the click keeps the permission that a click gives, however long the text takes.
 * Falls back to writeText and then to a hidden textarea, as the shell's own copy buttons do.
 */
async function copyText(text: Promise<string>): Promise<boolean> {
  try {
    if (typeof ClipboardItem !== "undefined" && navigator.clipboard?.write) {
      await navigator.clipboard.write([new ClipboardItem({ "text/plain": text.then((t) => new Blob([t], { type: "text/plain" })) })]);
      return true;
    }
  } catch {
    // no permission, no focus, or a browser that takes no promise: try the older ways
  }
  const value = await text;
  try {
    await navigator.clipboard.writeText(value);
    return true;
  } catch {
    const box = document.createElement("textarea");
    box.value = value;
    box.setAttribute("aria-hidden", "true");
    box.style.cssText = "position:fixed;top:0;left:0;width:1px;height:1px;opacity:0";
    document.body.append(box);
    box.select();
    let ok = false;
    try {
      ok = document.execCommand("copy");
    } catch {
      ok = false;
    }
    box.remove();
    return ok;
  }
}

const SVG_NS = "http://www.w3.org/2000/svg";

function icon(cls: string, paths: string[]): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("class", cls);
  svg.setAttribute("aria-hidden", "true");
  for (const d of paths) {
    const p = document.createElementNS(SVG_NS, "path");
    p.setAttribute("d", d);
    svg.append(p);
  }
  return svg;
}

interface CopyButton {
  root: HTMLElement;
  button: HTMLButtonElement;
  /** Shows the outcome beside the icon for a moment; an empty text shows only the tick. */
  done(ok: boolean, text: string): void;
  reset(): void;
}

/** A small icon-only button with a name for screen readers and a tooltip. */
function copyButton(label: string, onClick: () => void): CopyButton {
  const button = el("button", { class: "neb-icon" });
  button.type = "button";
  button.title = label;
  button.setAttribute("aria-label", label);
  button.append(icon("neb-i-copy", ["M9 9h10a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H9a1 1 0 0 1-1-1V10a1 1 0 0 1 1-1z", "M5 15H4a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1"]), icon("neb-i-done", ["M5 12l5 5 9-10"]));
  const said = el("span", { class: "neb-copied" });
  said.setAttribute("role", "status");
  const root = el("span", { class: "neb-copybox" }, said, button);
  let timer: ReturnType<typeof setTimeout> | undefined;
  const reset = (): void => {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
    delete button.dataset.state;
    button.title = label;
    button.setAttribute("aria-label", label);
    said.textContent = "";
  };
  button.addEventListener("click", (e) => {
    e.preventDefault();
    onClick();
  });
  return {
    root,
    button,
    reset,
    done(ok, text) {
      reset();
      if (ok) {
        button.dataset.state = "copied";
        button.title = "Copied";
        button.setAttribute("aria-label", "Copied");
      }
      said.textContent = text || (ok ? "Copied" : "");
      said.dataset.error = String(!ok);
      timer = setTimeout(reset, ok && !text ? COPIED_MS : 4 * COPIED_MS);
    },
  };
}

interface RowEls {
  root: HTMLButtonElement;
  id: HTMLElement;
  name: HTMLElement;
  state: HTMLElement;
  stateText: HTMLElement;
  sub: HTMLElement;
  age: HTMLElement;
}

export default class NebulaPage extends HTMLElement {
  ctx?: PageContext;
  private off: (() => void)[] = [];
  private runs: Record<string, Run> = {};
  private rows = new Map<string, RowEls>();
  private order: string[] = [];
  private selected: string | undefined;
  /** The run the user clicked; without one the selection follows the first row. */
  private picked: string | undefined;
  /** Known after the first `view` or `oneLiner` answer. */
  private local: boolean | undefined;
  private viewTimer: ReturnType<typeof setTimeout> | undefined;
  private viewSeq = 0;
  /** Where the next `view` continues; undefined before the first answer for this run. */
  private offset: number | undefined;
  /** Output bytes before this are not on the page. */
  private shownFrom = 0;
  private shownChars = 0;
  private dropped = false;
  private haveScript = false;
  /** The run's state when the details were last fetched: they change with it (fetched from, OS). */
  private detailsState = "";
  private scriptText = "";
  /** The script the <pre> shows now; it is built when the box is opened. */
  private scriptDrawn: string | undefined;
  private scriptCut = false;
  /** Where the script goes on, in bytes, when only its first part came. */
  private scriptNext = 0;
  private ansi: AnsiState = {};
  /** Nothing more to fetch until the run changes. */
  private viewDone = false;
  /** bytes, state and deleted of the selected run when `view` last answered. */
  private viewSig = "";
  private last: ViewResult | undefined;
  private revealMain = false;
  private actionsKey = "";
  private actionsOff: (() => void)[] = [];
  private ageTimer: ReturnType<typeof setInterval> | undefined;

  private root = el("div", { class: "neb" });
  private health = el("span", { class: "neb-health" });
  private count = el("span");
  private bar = el("div", { class: "neb-bar" }, this.health, this.count);
  private list = el("div", { class: "neb-list" });
  private openHead = el("div", { class: "neb-group", text: "Open" });
  private doneHead = el("div", { class: "neb-group", text: "History" });
  private main = el("div", { class: "neb-main" });
  private heading = el("h3");
  private stateBox = el("span", { class: "neb-state" });
  private facts = el("dl", { class: "neb-facts" });
  private copyBtn = el("button", { class: "neb-btn", text: "Copy one-liner" });
  private actionsBox = el("div");
  private actions = el("div", { class: "neb-actions" }, this.copyBtn, this.actionsBox);
  private line = el("textarea", { class: "neb-line" });
  private note = el("p", { class: "neb-note" });
  private execList = el("div", { class: "neb-execs" });
  private execCard = el("div", { class: "neb-card" }, this.execList);
  private scriptPre = el("pre", { class: "neb-pre" });
  private scriptNote = el("p", { class: "neb-note" });
  private moreFacts = el("dl", { class: "neb-facts" });
  private scriptSummary = el("summary", { text: "Script" });
  private copyScriptBtn = copyButton("Copy the script", () => void this.copyScript());
  private script = el("details", { class: "neb-card neb-script" }, this.scriptSummary, this.copyScriptBtn.root, this.scriptPre, this.scriptNote);
  private outStatus = el("span");
  private copyOutBtn = copyButton("Copy the complete output", () => void this.copyOutput());
  private outBar = el("div", { class: "neb-outbar" }, el("h4", { text: "Output" }), this.outStatus, this.copyOutBtn.root);
  private out = el("pre", { class: "neb-pre neb-out" });
  private localNote = el("p", { class: "neb-note" });

  connectedCallback(): void {
    const ctx = this.ctx;
    if (!ctx) return;
    const style = el("style");
    style.textContent = STYLE;
    this.build();
    this.replaceChildren(style, this.root);
    this.off.push(ctx.subscribe("runs", (v) => this.onRuns(v)));
    this.off.push(ctx.subscribe("daemon", () => this.renderBar()));
    this.off.push(ctx.subscribe("db", () => this.renderBar()));
    this.off.push(ctx.subscribe("counts", () => this.renderBar()));
    const onVisible = () => {
      if (document.visibilityState === "visible") this.refreshSoon(0);
      else this.stopView();
    };
    document.addEventListener("visibilitychange", onVisible);
    this.off.push(() => document.removeEventListener("visibilitychange", onVisible));
    // Ages ("5 min ago") move on without any change in state.
    this.ageTimer = setInterval(() => {
      if (document.visibilityState !== "visible") return;
      for (const id of this.order) this.updateRow(id);
      this.renderSelected();
    }, 20_000);
    this.refreshSoon(0);
  }

  disconnectedCallback(): void {
    this.off.forEach((f) => f());
    this.off = [];
    this.actionsOff.forEach((f) => f());
    this.actionsOff = [];
    if (this.ageTimer !== undefined) clearInterval(this.ageTimer);
    this.stopView();
    this.viewSeq++;
  }

  private build(): void {
    this.copyBtn.type = "button";
    this.copyBtn.addEventListener("click", () => void this.copyOneLiner());
    this.line.readOnly = true;
    this.line.rows = 3;
    this.line.spellcheck = false;
    this.line.setAttribute("aria-label", "The one-liner to paste");
    this.line.hidden = true;
    this.note.hidden = true;
    this.execCard.hidden = true;
    this.script.hidden = true;
    this.scriptNote.hidden = true;
    this.localNote.hidden = true;
    this.out.tabIndex = 0;
    this.out.setAttribute("aria-label", "Output of the run");
    const head = el("div", { class: "neb-head" }, this.heading, this.stateBox);
    this.script.addEventListener("toggle", () => this.drawScript());
    this.moreFacts.hidden = true;
    this.main.append(el("div", { class: "neb-card" }, head, this.facts, this.moreFacts, this.actions, this.line, this.note), this.execCard, this.script, this.outBar, this.out, this.localNote);
    this.main.hidden = true;
    this.root.append(this.bar, el("div", { class: "neb-grid" }, this.list, this.main));
  }

  // ---- bar ----

  private renderBar(): void {
    const ctx = this.ctx;
    if (!ctx) return;
    const db = ctx.get("db");
    const daemon = ctx.get("daemon") as { running?: boolean; tunnel?: string } | undefined;
    const counts = ctx.get("counts") as { total?: number; limit?: number } | undefined;
    let text: string;
    let level = "";
    if (db === "missing") {
      text = "Nebula has no database yet. Runs appear here once an agent registers one.";
    } else if (db === "error") {
      text = "Nebula's database can't be read right now (see Logs). What is shown may be out of date.";
      level = "err";
    } else if (!daemon?.running) {
      text = "The Nebula daemon is not running: remote runs can't be fetched and nothing expires. Start it with: nebula service install";
      level = "warn";
    } else {
      const tunnel: Record<string, string> = {
        up: "tunnel up",
        starting: "tunnel starting",
        down: "tunnel down: remote runs can't be fetched",
        disabled: "tunnel switched off: local runs only",
        missing: "cloudflared is not installed: local runs only",
      };
      const t = daemon.tunnel ?? "down";
      text = `Daemon running, ${tunnel[t] ?? `tunnel ${t}`}`;
      if (t === "down" || t === "missing") level = "warn";
    }
    this.health.textContent = text;
    this.health.dataset.level = level;
    const total = counts?.total ?? 0;
    const limit = counts?.limit ?? 0;
    this.count.textContent = limit > 0 && total > limit ? `Showing the newest ${limit} of ${total} runs.` : "";
  }

  // ---- list ----

  /** Runs with something still open first, then the rest, newest first; executions under their run. */
  private ordered(): { open: string[]; done: string[] } {
    const kids = new Map<string, string[]>();
    const tops: string[] = [];
    for (const [id, r] of Object.entries(this.runs)) {
      const parent = r.parent === undefined ? undefined : String(r.parent);
      if (parent !== undefined && this.runs[parent]) {
        const list = kids.get(parent);
        if (list) list.push(id);
        else kids.set(parent, [id]);
      } else tops.push(id);
    }
    tops.sort((a, b) => Number(b) - Number(a));
    const open: string[] = [];
    const done: string[] = [];
    for (const id of tops) {
      const mine = (kids.get(id) ?? []).sort((a, b) => (this.runs[a]!.seq ?? 0) - (this.runs[b]!.seq ?? 0));
      const live = isOpen(this.runs[id]!) || mine.some((k) => isOpen(this.runs[k]!));
      (live ? open : done).push(id, ...mine);
    }
    return { open, done };
  }

  private onRuns(v: JsonValue | undefined): void {
    // A row without a label isn't a run yet.
    this.runs = Object.fromEntries(Object.entries(asRuns(v)).filter(([, r]) => typeof r.label === "string" && typeof r.state === "string"));
    const { open, done } = this.ordered();
    this.order = [...open, ...done];
    if (this.picked !== undefined && !this.runs[this.picked]) this.picked = undefined;
    const wanted = this.picked ?? this.order[0];
    if (wanted !== this.selected) this.select(wanted, false);
    for (const [id, r] of this.rows) {
      if (this.runs[id]) continue;
      r.root.remove();
      this.rows.delete(id);
    }
    const nodes: Element[] = [];
    const add = (head: HTMLElement, ids: string[]) => {
      if (ids.length === 0) return;
      nodes.push(head);
      for (const id of ids) {
        this.updateRow(id);
        nodes.push(this.rows.get(id)!.root);
      }
    };
    add(this.openHead, open);
    add(this.doneHead, done);
    if (nodes.length === 0) this.list.replaceChildren(el("div", { class: "neb-empty", text: "No runs." }));
    else if (nodes.some((e, i) => this.list.children[i] !== e) || this.list.children.length !== nodes.length) this.list.replaceChildren(...nodes);
    this.list.dataset.stale = String(this.ctx?.isStale("runs") === true);
    this.renderSelected();
    // More output, another state, output deleted: fetch again.
    const s = this.current();
    if (s && this.sig(s) !== this.viewSig && this.viewDone) {
      this.viewDone = false;
      this.refreshSoon(0);
    }
  }

  private makeRow(id: string): RowEls {
    const idBox = el("span", { class: "neb-id" });
    const name = el("span", { class: "neb-name" });
    const stateText = el("span");
    const state = el("span", { class: "neb-state" }, el("span", { class: "neb-dot" }), stateText);
    const sub = el("span", { class: "neb-sub" });
    const age = el("span", { class: "neb-age" });
    const root = el("button", { class: "neb-row" }, el("span", { class: "neb-top" }, idBox, name, state), el("span", { class: "neb-mid" }, sub, age));
    root.type = "button";
    root.addEventListener("click", () => this.select(id, true));
    return { root, id: idBox, name, state, stateText, sub, age };
  }

  private updateRow(id: string): void {
    const r = this.runs[id];
    if (!r) return;
    let row = this.rows.get(id);
    if (!row) {
      row = this.makeRow(id);
      this.rows.set(id, row);
    }
    const st = stateOf(r);
    const set = (e: HTMLElement, text: string) => {
      if (e.textContent !== text) e.textContent = text;
    };
    row.root.dataset.exec = String(r.parent !== undefined && this.runs[String(r.parent)] !== undefined);
    row.root.setAttribute("aria-current", String(id === this.selected));
    set(row.id, `#${r.label}`);
    set(row.name, titleOf(r));
    row.name.title = titleOf(r);
    row.name.dataset.unnamed = String(r.name === undefined);
    set(row.stateText, st.text);
    row.state.dataset.tone = st.tone;
    const where = r.host ? `${r.user ? `${r.user}@` : ""}${r.host}` : r.target;
    const parts = [where];
    if (r.deleted) parts.push("output deleted");
    else if (r.bytes > 0) parts.push(fmtBytes(r.bytes));
    set(row.sub, parts.join(" · "));
    row.sub.title = row.sub.textContent ?? "";
    const at = r.finished ?? r.started ?? r.created;
    set(row.age, `${fmtSpan(Date.now() - at)} ago`);
  }

  private current(): Run | undefined {
    return this.selected === undefined ? undefined : this.runs[this.selected];
  }

  private sig(r: Run): string {
    return `${r.state}/${r.bytes}/${r.deleted === true}`;
  }

  private select(id: string | undefined, byUser: boolean): void {
    if (id === this.selected && !byUser) return;
    const changed = id !== this.selected;
    this.selected = id;
    if (byUser) this.picked = id;
    for (const [k, r] of this.rows) r.root.setAttribute("aria-current", String(k === id));
    if (changed) {
      this.viewSeq++;
      this.resetView();
      this.line.hidden = true;
      this.line.value = "";
      this.note.hidden = true;
    }
    this.renderSelected();
    this.refreshSoon(0);
    this.revealMain = byUser && this.root.clientWidth < WIDE_PX;
  }

  // ---- selected run ----

  private fill(dl: HTMLElement, rows: [string, string][]): void {
    const dts = [...dl.querySelectorAll("dt")];
    if (dts.length !== rows.length || rows.some(([k], i) => dts[i]!.textContent !== k)) {
      dl.replaceChildren(...rows.flatMap(([k, v]) => [el("dt", { text: k }), el("dd", { text: v })]));
      return;
    }
    rows.forEach(([, v], i) => {
      const dd = dts[i]!.nextElementSibling!;
      if (dd.textContent !== v) dd.textContent = v;
    });
  }

  private renderSelected(): void {
    const r = this.current();
    this.main.hidden = !r;
    if (!r) return;
    const st = stateOf(r);
    this.heading.textContent = `#${r.label} ${titleOf(r)}`;
    this.stateBox.dataset.tone = st.tone;
    this.stateBox.replaceChildren(el("span", { class: "neb-dot" }), st.text);
    const now = Date.now();
    const facts: [string, string][] = [["Target", r.target === "local" ? "this machine (nebula go)" : "remote (pasted one-liner)"]];
    if (r.host) facts.push(["Ran on", `${r.host}${r.user ? ` as ${r.user}` : ""}`]);
    facts.push(["Registered", `${fmtTime(r.created)} (${fmtSpan(now - r.created)} ago)`]);
    if (r.state === "pending") facts.push(["Link", r.expires > now ? `valid for another ${fmtSpan(r.expires - now)}` : "past its time; the daemon will expire it"]);
    if (r.multi) facts.push(["Uses", r.uses ? `${r.fetches ?? 0} of ${r.uses} taken` : `${r.fetches ?? 0} taken, any number until the link expires`]);
    if (r.started !== undefined) facts.push(["Started", fmtTime(r.started)]);
    if (r.finished !== undefined) {
      const took = r.started !== undefined ? ` (took ${fmtSpan(r.finished - r.started)})` : "";
      facts.push([r.state === "done" ? "Finished" : "Ended", `${fmtTime(r.finished)}${took}`]);
    } else if (r.state === "running" && r.seen !== undefined) {
      facts.push(["Last sign of life", `${fmtSpan(now - r.seen)} ago`]);
    }
    if (r.exit !== undefined) facts.push(["Exit code", String(r.exit)]);
    if (!r.multi) {
      let out = r.deleted ? "deleted" : `${fmtBytes(r.bytes)} stored`;
      if (r.reported !== undefined && r.reported !== r.bytes) out += `, ${fmtBytes(r.reported)} produced`;
      if (r.truncated) out += ", capped at 50 MB";
      facts.push(["Output", out]);
    }
    this.fill(this.facts, facts);
    this.renderExecutions(r);
    this.renderActions(r);
    this.renderOutBar();
    const contents = this.local !== false;
    this.outBar.hidden = !contents || r.multi === true;
    // An empty box says nothing the status line doesn't.
    this.out.hidden = !contents || r.multi === true || this.shownChars === 0;
    this.script.hidden = !contents || !this.haveScript;
    this.moreFacts.hidden = !contents || this.moreFacts.childElementCount === 0;
    this.copyOutBtn.root.hidden = r.deleted === true || r.bytes === 0;
    this.localNote.hidden = contents;
    if (!contents) this.localNote.textContent = "Output, script and the one-liner are shown only on the computer motherapp runs on.";
  }

  /** A run whose link takes several fetches has no output of its own: its executions do. */
  private renderExecutions(r: Run): void {
    const kids = r.multi ? this.order.filter((id) => String(this.runs[id]?.parent) === this.selected) : [];
    this.execCard.hidden = !r.multi;
    if (!r.multi) return;
    const key = kids.map((id) => `${id}:${stateOf(this.runs[id]!).text}`).join(",");
    if (this.execList.dataset.key === key) return;
    this.execList.dataset.key = key;
    const nodes: Node[] = [el("h4", { text: kids.length === 0 ? "No executions yet" : "Executions (each has its own output)" })];
    for (const id of kids) {
      const k = this.runs[id]!;
      const b = el("button", { class: "neb-btn", text: `#${k.label}  ${stateOf(k).text}${k.host ? `  ${k.host}` : ""}` });
      b.type = "button";
      b.style.textAlign = "start";
      b.addEventListener("click", () => this.select(id, true));
      nodes.push(b);
    }
    this.execList.replaceChildren(...nodes);
  }

  /** ctx.render gives the buttons the shell's confirm dialog and status dot. */
  private renderActions(r: Run): void {
    const id = this.selected!;
    const kidsOpen = this.order.some((k) => String(this.runs[k]?.parent) === id && isOpen(this.runs[k]!));
    const open = isOpen(r);
    const pasteable = r.state === "pending" && r.parent === undefined && this.local !== false;
    this.copyBtn.hidden = !pasteable;
    if (!pasteable) this.line.hidden = true;
    const key = JSON.stringify([id, r.label, r.name, r.state, r.bytes > 0, r.deleted, r.multi, kidsOpen]);
    if (key === this.actionsKey || !this.ctx) return;
    this.actionsKey = key;
    this.actionsOff.forEach((f) => f());
    const what = noPlaceholders(`#${r.label}${r.name ? ` '${r.name}'` : ""}`);
    const widgets: { type: "button"; label: string; action: string; args: { id: string }; confirm: string }[] = [];
    if (open || kidsOpen) {
      const far = r.state !== "pending" || kidsOpen ? " A script already running on another machine is not stopped; its result is discarded." : " Its link stops working.";
      widgets.push({ type: "button", label: "Cancel this run", action: "cancel", args: { id: r.label }, confirm: `Cancel run ${what}?${far}` });
    } else {
      if (r.multi || (r.bytes > 0 && !r.deleted)) {
        const size = r.multi ? "of all its executions" : `(${fmtBytes(r.bytes)})`;
        widgets.push({ type: "button", label: "Delete output", action: "removeOutput", args: { id: r.label }, confirm: `Delete the stored output of run ${what} ${size}? The entry stays in the history. This cannot be undone.` });
      }
      widgets.push({ type: "button", label: "Delete run", action: "remove", args: { id: r.label }, confirm: `Delete run ${what} from Nebula's history, with its output${r.multi ? " and all its executions" : ""}? This cannot be undone.` });
    }
    this.actionsOff = [this.ctx.render(widgets, this.actionsBox)];
  }

  /** Asks for the one-liner (local only), puts it on the clipboard and shows it. */
  private async copyOneLiner(): Promise<void> {
    const r = this.current();
    const id = this.selected;
    if (!r || !this.ctx) return;
    this.copyBtn.disabled = true;
    try {
      const res = (await this.ctx.action("oneLiner", { id: r.label })) as unknown as { text: string };
      if (id !== this.selected) return;
      this.local = true;
      this.line.value = res.text;
      this.line.hidden = false;
      let copied = false;
      try {
        await navigator.clipboard.writeText(res.text);
        copied = true;
      } catch {
        this.line.focus();
        this.line.select();
        try {
          copied = document.execCommand("copy");
        } catch {
          copied = false;
        }
      }
      const where = r.target === "local" ? "Run it in a terminal on this machine." : "Paste it only into the terminal of the target machine: any fetch uses the link up.";
      this.say(copied ? `Copied to the clipboard. ${where}` : `Could not reach the clipboard: press Ctrl+C to copy it. ${where}`, false);
    } catch (e) {
      if (id !== this.selected) return;
      if ((e as { code?: number }).code === LOCAL_ONLY) {
        this.local = false;
        this.renderSelected();
        return;
      }
      this.say(`No one-liner: ${(e as Error).message}`, true);
    } finally {
      this.copyBtn.disabled = false;
    }
  }

  private say(text: string, error: boolean): void {
    this.note.textContent = text;
    this.note.dataset.error = String(error);
    this.note.hidden = false;
  }

  // ---- script and output ----

  private stopView(): void {
    if (this.viewTimer !== undefined) clearTimeout(this.viewTimer);
    this.viewTimer = undefined;
  }

  private refreshSoon(ms: number): void {
    this.stopView();
    if (this.local === false || this.viewDone || this.selected === undefined || document.visibilityState !== "visible") return;
    this.viewTimer = setTimeout(() => void this.refreshView(), ms);
  }

  private resetView(): void {
    this.offset = undefined;
    this.shownFrom = 0;
    this.shownChars = 0;
    this.dropped = false;
    this.haveScript = false;
    this.viewDone = false;
    this.viewSig = "";
    this.last = undefined;
    this.out.replaceChildren();
    this.ansi = {};
    this.detailsState = "";
    this.scriptText = "";
    this.scriptDrawn = undefined;
    this.scriptCut = false;
    this.scriptPre.replaceChildren();
    this.moreFacts.replaceChildren();
    this.copyScriptBtn.reset();
    this.copyOutBtn.reset();
    this.script.open = false;
    this.script.hidden = true;
    this.outStatus.textContent = "";
  }

  private async refreshView(): Promise<void> {
    const ctx = this.ctx;
    const run = this.current();
    if (!ctx || !run) return;
    const seq = ++this.viewSeq;
    const sig = this.sig(run);
    try {
      const args: { id: string; offset?: number; script?: boolean } = { id: run.label };
      if (this.offset !== undefined) args.offset = this.offset;
      const withDetails = !this.haveScript || run.state !== this.detailsState;
      if (withDetails) args.script = true;
      const r = (await ctx.action("view", args)) as unknown as ViewResult;
      if (seq !== this.viewSeq) return;
      if (withDetails) this.detailsState = run.state;
      if (this.local !== true) {
        this.local = true;
        this.actionsKey = "";
      }
      this.viewSig = sig;
      this.applyView(r);
      // Over and all of it here: nothing more to ask until the run changes.
      this.viewDone = run.multi === true || (!OPEN_STATES.includes(r.state) && r.next >= r.bytes);
    } catch (e) {
      if (seq !== this.viewSeq) return;
      if ((e as { code?: number }).code === LOCAL_ONLY) {
        this.local = false;
        this.actionsKey = "";
        this.renderSelected();
        return;
      }
      this.outStatus.textContent = `Can't read the output: ${(e as Error).message}`;
      this.viewSig = sig;
      this.viewDone = true;
    }
    this.renderSelected();
    if (this.revealMain) {
      this.revealMain = false;
      this.main.scrollIntoView({ block: "start" });
    }
    this.refreshSoon(VIEW_MS);
  }

  private applyView(r: ViewResult): void {
    this.last = r;
    if (r.script !== undefined || r.scriptMissing || r.details) {
      this.haveScript = true;
      this.scriptText = r.script ?? "";
      this.scriptCut = r.scriptCut === true;
      this.scriptNext = r.scriptNext ?? 0;
      this.copyScriptBtn.root.hidden = r.script === undefined;
      this.drawScript();
      const summary = r.scriptBytes !== undefined ? `Script (${fmtBytes(r.scriptBytes)})` : "Script";
      if (this.scriptSummary.textContent !== summary) this.scriptSummary.textContent = summary;
      this.scriptNote.hidden = !r.scriptCut && !r.scriptMissing;
      this.scriptNote.textContent = r.scriptMissing ? "The script is no longer in the database." : "Only the first part is shown. The copy button copies all of it.";
      const d = r.details ?? {};
      const more: [string, string][] = [];
      if (d.note) more.push(["Note", d.note]);
      if (d.via) more.push(["Executed via", d.via === "go" ? "nebula go" : d.via]);
      // As the request and the far side said: recorded, not verified.
      if (d.fetchAddr) more.push(["Fetched from", `${d.fetchAddr}${d.fetchCountry ? ` (${d.fetchCountry})` : ""}`]);
      if (d.fetchAgent) more.push(["Fetched with", d.fetchAgent]);
      if (d.os) more.push(["System", d.os]);
      if (d.tty) more.push(["Terminal", d.tty]);
      if (d.onDone) more.push(["On done", d.onDone]);
      if (d.tmux) more.push(["tmux notice", `pane ${d.tmux}`]);
      if (d.hook) more.push(["Hook result", d.hook]);
      const flags = [...(d.echo ? ["output also shown on the far terminal"] : []), ...(d.deleteAfter ? ["output deleted once printed"] : [])];
      if (flags.length > 0) more.push(["Flags", flags.join(", ")]);
      if (d.sha256) more.push(["Script sha256", d.sha256]);
      this.fill(this.moreFacts, more);
    }
    // A run that is over is read from its start; one still going is followed at its end.
    const follow = this.shownChars === 0 ? OPEN_STATES.includes(r.state) : this.atBottom();
    if (this.offset === undefined || r.from !== this.offset) {
      // The first answer, or the tool skipped ahead (newest kept): start over from there.
      this.out.replaceChildren();
      this.ansi = {};
      this.shownChars = 0;
      this.dropped = false;
      this.shownFrom = r.from;
    }
    if (r.text !== "") {
      // Only the new piece is looked at; the colours in force carry over in `this.ansi`.
      for (const run of ansiFeed(this.ansi, r.text)) {
        if (run.cls === "" && run.fg === undefined && run.bg === undefined) {
          this.out.append(document.createTextNode(run.text));
        } else {
          const span = el("span", { text: run.text });
          if (run.cls) span.className = run.cls;
          // A colour given as a number (256 colours, RGB) knows nothing of the theme: half of
          // it is the text colour, and a background is a wash, so it reads in light and in dark.
          if (run.fg) span.style.color = `color-mix(in oklab, ${run.fg} 50%, var(--fg))`;
          if (run.bg) span.style.background = `color-mix(in srgb, ${run.bg} 16%, transparent)`;
          this.out.append(span);
        }
        this.shownChars += run.text.length;
      }
      while (this.shownChars > MAX_SHOWN && this.out.childNodes.length > 1) {
        const first = this.out.firstChild!;
        this.shownChars -= first.textContent?.length ?? 0;
        first.remove();
        this.dropped = true;
      }
    }
    this.offset = r.next;
    this.out.hidden = this.shownChars === 0;
    if (follow) this.out.scrollTop = this.out.scrollHeight;
    this.renderOutBar();
  }

  /** Builds the script's nodes, once per script and only while its box is open. */
  private drawScript(): void {
    if (!this.script.open || this.scriptDrawn === this.scriptText) return;
    const text = this.scriptText;
    this.scriptDrawn = text;
    if (!isShellScript(text)) {
      this.scriptPre.textContent = text;
      return;
    }
    const frag = document.createDocumentFragment();
    for (const [kind, piece] of tokenizeShell(text)) {
      if (kind === "") frag.append(piece);
      else frag.append(el("span", { class: `neb-t-${kind}`, text: piece }));
    }
    this.scriptPre.replaceChildren(frag);
  }

  /** The whole script, also the part of a long one that isn't shown. */
  private async wholeScript(label: string): Promise<string> {
    let text = this.scriptText;
    if (!this.scriptCut || !this.ctx) return text;
    let at = this.scriptNext;
    for (let i = 0; i < 64; i++) {
      // An offset past the end with `forward`: no output comes along, only the script.
      const r = (await this.ctx.action("view", { id: label, offset: Number.MAX_SAFE_INTEGER, forward: true, scriptOffset: at })) as unknown as ViewResult;
      text += r.script ?? "";
      const next = r.scriptNext ?? at;
      if (next <= at || next >= (r.scriptBytes ?? 0)) break;
      at = next;
    }
    return text;
  }

  private async copyScript(): Promise<void> {
    const run = this.current();
    if (!run) return;
    const btn = this.copyScriptBtn;
    btn.button.disabled = true;
    try {
      const ok = await copyText(this.wholeScript(run.label));
      btn.done(ok, ok ? "" : "Could not reach the clipboard");
    } catch (e) {
      btn.done(false, `Not copied: ${(e as Error).message}`);
    } finally {
      btn.button.disabled = false;
    }
  }

  /**
   * The run's complete output as plain text, fetched page by page, whatever the box shows. Of
   * more than COPY_MAX the newest part; `cut` says so.
   */
  private async wholeOutput(label: string, total: number): Promise<{ text: string; cut: boolean; bytes: number }> {
    const ctx = this.ctx!;
    const start = Math.max(0, total - COPY_MAX);
    const parts: string[] = [];
    let at = start;
    let bytes = total;
    for (let i = 0; i < 4096; i++) {
      const r = (await ctx.action("view", { id: label, offset: at, forward: true })) as unknown as ViewResult;
      parts.push(r.text);
      bytes = r.bytes;
      if (r.next <= at || r.next >= r.bytes) break;
      at = r.next;
    }
    let text = stripAnsi(parts.join(""));
    // Started in the middle of the output, maybe in the middle of a character.
    if (start > 0) text = text.replace(/^\ufffd+/, "");
    return { text, cut: start > 0, bytes };
  }

  private async copyOutput(): Promise<void> {
    const run = this.current();
    if (!run || !this.ctx) return;
    const btn = this.copyOutBtn;
    btn.button.disabled = true;
    try {
      const whole = this.wholeOutput(run.label, this.last?.bytes ?? run.bytes);
      const ok = await copyText(whole.then((w) => w.text));
      const w = await whole;
      if (!ok) btn.done(false, "Could not reach the clipboard");
      else btn.done(true, w.cut ? `Copied the newest ${fmtBytes(COPY_MAX)} of ${fmtBytes(w.bytes)}` : `Copied all ${fmtBytes(w.bytes)}`);
    } catch (e) {
      btn.done(false, `Not copied: ${(e as Error).message}`);
    } finally {
      btn.button.disabled = false;
    }
  }

  private atBottom(): boolean {
    return this.out.scrollHeight - this.out.scrollTop - this.out.clientHeight < BOTTOM_PX;
  }

  private renderOutBar(): void {
    const r = this.last;
    const run = this.current();
    if (!r || !run) return;
    const parts: string[] = [];
    if (r.deleted) parts.push("The output was deleted.");
    else if (r.bytes === 0) parts.push(isOpen(run) ? "Nothing yet." : "This run printed nothing.");
    else parts.push(isOpen(run) ? `${fmtBytes(r.bytes)} so far, following` : fmtBytes(r.bytes));
    if (this.shownFrom > 0 || this.dropped) parts.push(`only the newest part is shown here; nebula output ${r.id} prints all of it`);
    if (r.truncated) parts.push("capped at 50 MB, the rest was not stored");
    this.outStatus.textContent = parts.join(" · ");
  }
}
