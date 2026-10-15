//! `rho acp --daemon` must exit when the daemon goes away, even though the
//! editor keeps its stdin open.

use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn bridge_exits_when_daemon_drops_connection() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("rho/daemon");
    std::fs::create_dir_all(&dir).unwrap();
    let listener = UnixListener::bind(dir.join("acp.sock")).unwrap();
    let daemon = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let ws = tungstenite::accept(stream).unwrap();
        // Long enough for the bridge to wait on stdin.
        std::thread::sleep(Duration::from_millis(300));
        drop(ws);
    });

    let mut child = Command::new(env!("CARGO_BIN_EXE_rho"))
        .args(["acp", "--daemon"])
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Kept open, like an editor would.
    let _stdin = child.stdin.take();
    daemon.join().unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("rho acp --daemon did not exit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
