//! FIX-01: a committed mutation's reply must survive the owner retirement
//! boundary (the "write may have committed; verify before retrying" /
//! os error 10054 class).
//!
//! The owner runs `run_owner_server` in a REAL child process — this test
//! binary re-executed with `TURNKEY_OWNER_RACE_CHILD_DIR` set — because the
//! owner loop exits the process when it retires. The parent starts a
//! deliberately slow mutation, retires the owner mid-handler with a
//! token-authenticated `owner/shutdown`, and asserts the in-flight reply
//! still arrives instead of a connection reset (`ResponseLost`).

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use mcp_product_infra::sidecar::{run_owner_server, send_line};
use mcp_product_infra::{OwnerEndpoint, SidecarConfig};

const CHILD_DIR_ENV: &str = "TURNKEY_OWNER_RACE_CHILD_DIR";

/// How long the child's slow handler holds the request. Long enough that the
/// shutdown is accepted and processed while this handler is still running,
/// short enough to finish well inside the owner's in-flight retirement wait.
const SLOW_HANDLER_HOLD: Duration = Duration::from_millis(900);

fn child_config(dir: &Path) -> SidecarConfig {
    // A short idle timeout keeps the accept loop's poll slices small (so the
    // shutdown connection is accepted while the slow handler still runs) and
    // self-reaps the child if a failing parent leaks it.
    SidecarConfig::new("owner-race-test", dir, dir.join("cache"))
        .app_version("0.0.1")
        .idle_timeout(Duration::from_secs(5))
}

/// Child mode: with `TURNKEY_OWNER_RACE_CHILD_DIR` set, this "test" is the
/// owner process. Without it (the normal suite run) it is an inert pass.
#[test]
fn child_owner_serves_until_shutdown() {
    let dir = match std::env::var(CHILD_DIR_ENV) {
        Ok(dir) => dir,
        Err(_) => return,
    };
    let config = child_config(Path::new(&dir));
    run_owner_server(
        config,
        || Ok(()),
        |line| {
            let method = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v.get("method")?.as_str().map(str::to_string));
            if method.as_deref() == Some("app/slow") {
                thread::sleep(SLOW_HANDLER_HOLD);
                Some(r#"{"jsonrpc":"2.0","id":7,"result":{"slow":"done"}}"#.to_string())
            } else {
                Some(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#.to_string())
            }
        },
    )
    .expect("child owner runs");
    // Reached only on the lock-contention early return; the retirement path
    // exits the process. Either way the child must not fall through to run
    // other tests.
    std::process::exit(0);
}

#[test]
fn committed_mutation_reply_survives_owner_shutdown() {
    // Deterministic before/after: with the pre-fix `process::exit(0)` shutdown
    // path this lost the in-flight reply on every iteration; a couple of
    // rounds guard against a fluky single pass.
    for round in 0..3 {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = child_config(dir.path());

        let exe = std::env::current_exe().expect("test binary path");
        let mut child = Command::new(exe)
            .arg("child_owner_serves_until_shutdown")
            .arg("--exact")
            .env(CHILD_DIR_ENV, dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child owner");

        let endpoint = wait_for_endpoint(&config).unwrap_or_else(|| {
            let _ = child.kill();
            panic!("round {round}: child owner never published its endpoint");
        });

        // In-flight mutation: the handler holds it for SLOW_HANDLER_HOLD.
        let slow_endpoint = endpoint.clone();
        let slow = thread::spawn(move || {
            send_line(
                &slow_endpoint,
                r#"{"jsonrpc":"2.0","id":7,"method":"app/slow"}"#,
            )
        });

        // Let the slow request reach the owner, then retire it mid-handler.
        thread::sleep(Duration::from_millis(100));
        let ack = send_line(
            &endpoint,
            r#"{"jsonrpc":"2.0","id":0,"method":"owner/shutdown"}"#,
        );
        let ack = ack.unwrap_or_else(|e| {
            let _ = child.kill();
            panic!("round {round}: owner/shutdown must ack cleanly: {e}");
        });
        assert!(
            ack.as_deref().unwrap_or("").contains("shutting_down"),
            "round {round}: unexpected shutdown ack: {ack:?}"
        );

        // The heart of FIX-01: the reply committed before retirement must
        // arrive, never be torn down with the exiting owner.
        let reply = slow.join().expect("slow sender thread");
        match reply {
            Ok(Some(body)) => assert!(
                body.contains(r#""slow":"done""#),
                "round {round}: unexpected slow reply: {body}"
            ),
            other => {
                let _ = child.kill();
                panic!("round {round}: in-flight reply lost across owner shutdown: {other:?}");
            }
        }

        assert!(
            wait_for_exit(&mut child, Duration::from_secs(5)),
            "round {round}: retired owner must exit"
        );
    }
}

fn wait_for_endpoint(config: &SidecarConfig) -> Option<OwnerEndpoint> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(raw) = std::fs::read_to_string(config.endpoint_path()) {
            if let Ok(endpoint) = serde_json::from_str::<OwnerEndpoint>(&raw) {
                return Some(endpoint);
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

fn wait_for_exit(child: &mut std::process::Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(_) => return false,
        }
    }
    let _ = child.kill();
    false
}
