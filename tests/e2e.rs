//! Runs `tests/e2e.sh` against the freshly built binary on two free ports.
//! The script uses a temporary NEBULA_HOME and NEBULA_NO_TUNNEL=1.

use std::net::TcpListener;
use std::process::Command;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn end_to_end() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/e2e.sh");
    let (p1, p2) = (free_port(), free_port());
    assert_ne!(p1, p2);
    let out = Command::new("bash")
        .arg(script)
        .arg(env!("CARGO_BIN_EXE_nebula"))
        .env("E2E_PORT", p1.to_string())
        .env("E2E_PORT2", p2.to_string())
        .env_remove("NEBULA_HOME")
        .env_remove("NEBULA_PORT")
        .output()
        .expect("cannot run bash tests/e2e.sh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "e2e failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    println!("{stdout}");
}
