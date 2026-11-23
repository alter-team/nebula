//! Runs `motherapp/page.test.mjs`: the unit tests of the pure functions in
//! the motherapp page (shell tokenizer, ANSI colours). The page is
//! TypeScript, so this needs a Node that can load it: 22.6 or newer. Without
//! one the test says so and passes; set `NEBULA_TEST_NODE` to name a binary.

use std::process::Command;

/// (major, minor) of a `node` binary, if it runs.
fn version(node: &str) -> Option<(u32, u32)> {
    let out = Command::new(node).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut parts = text.trim().trim_start_matches('v').split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

#[test]
fn page_unit_tests() {
    let named = std::env::var("NEBULA_TEST_NODE").ok();
    let candidates: Vec<&str> = match &named {
        Some(n) => vec![n.as_str()],
        None => vec!["node", "/usr/bin/node", "/usr/local/bin/node"],
    };
    let found = candidates
        .iter()
        .filter_map(|n| version(n).map(|v| (*n, v)))
        .find(|(_, v)| *v >= (22, 6));
    let Some((node, v)) = found else {
        assert!(
            named.is_none(),
            "NEBULA_TEST_NODE is not a Node 22.6 or newer"
        );
        println!("SKIPPED: no Node 22.6+ found to run motherapp/page.test.mjs");
        return;
    };
    let mut cmd = Command::new(node);
    if v < (23, 6) {
        // newer ones strip types without being asked
        cmd.arg("--experimental-strip-types");
    }
    let out = cmd
        .arg("--no-warnings")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/motherapp/page.test.mjs"))
        .output()
        .expect("cannot run node");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "page tests failed on {node}:\n{text}");
    assert!(text.contains("fail 0") && !text.contains("pass 0"), "{text}");
    println!("{text}");
}
