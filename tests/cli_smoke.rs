//! Real-subprocess smoke tests: spawn the actual binary the way Docker will.
//!
//! `env_clear()` matters — it proves the binary does not silently depend on a
//! variable that happens to be set in the developer's shell.

use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_todo-mcp"));
    c.env_clear();
    c
}

#[test]
fn version_goes_to_stdout_and_exits_zero() {
    let out = bin().arg("--version").output().expect("spawned");
    assert!(out.status.success(), "status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("todo-mcp "), "stdout: {stdout:?}");
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")));
    // Nothing operational may contaminate stdout — `token` is captured by shell
    // substitution into an Authorization header.
    assert!(out.stderr.is_empty(), "stderr: {:?}", out.stderr);
}

#[test]
fn help_exits_zero_and_names_every_subcommand() {
    let out = bin().arg("--help").output().expect("spawned");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for cmd in ["serve", "login", "logout", "token", "doctor", "healthcheck"] {
        assert!(stdout.contains(cmd), "help omits {cmd}: {stdout}");
    }
    assert!(
        stdout.contains("EXIT CODES:"),
        "help omits exit codes: {stdout}"
    );
}

#[test]
fn no_arguments_prints_usage_rather_than_failing() {
    let out = bin().output().expect("spawned");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("USAGE:"));
}

#[test]
fn unknown_command_exits_usage_and_logs_json_to_stderr() {
    let out = bin().arg("frobnicate").output().expect("spawned");
    assert_eq!(out.status.code(), Some(2), "expected exit::USAGE");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let first = stderr.lines().next().unwrap_or_default();
    let parsed: serde_json::Value =
        serde_json::from_str(first).unwrap_or_else(|e| panic!("stderr not JSON: {first:?} ({e})"));
    assert_eq!(parsed["level"], "error");
    assert_eq!(parsed["code"], "CONFIG");
}

/// With an empty environment nothing can silently succeed: `serve`/`login`
/// lack a client id (usage, 2), `doctor` reports findings (1), and `healthcheck`
/// finds nothing listening (unhealthy, 1 — Docker's code; 2 is reserved). A
/// subcommand that exits 0 here would be doing nothing and pretending.
///
/// `healthcheck` alone gets `TODO_MCP_BIND` pointing at a port just released by
/// this test: its default, 127.0.0.1:8591, is what the shipped compose.yaml and a
/// host `serve` listen on, and a developer's running server would answer 200.
#[test]
fn subcommands_fail_loudly_in_an_empty_environment() {
    let closed_port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    }; // Dropped: nothing listens there now.
    for (cmd, code) in [("serve", 2), ("login", 2), ("healthcheck", 1)] {
        let mut c = bin();
        c.arg(cmd);
        if cmd == "healthcheck" {
            c.env("TODO_MCP_BIND", format!("127.0.0.1:{closed_port}"));
        }
        let out = c.output().expect("spawned");
        assert_eq!(
            out.status.code(),
            Some(code),
            "{cmd}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stdout.is_empty(),
            "{cmd} wrote to stdout: {:?}",
            out.stdout
        );
    }
    // `doctor` never refuses to run but reports findings with a non-zero exit.
    let out = bin().arg("doctor").output().expect("spawned");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("FINDING"));
}

/// Docker reads 1 as unhealthy and reserves 2, so even a configuration error
/// inside `healthcheck` must not surface as exit::USAGE.
#[test]
fn healthcheck_reports_invalid_configuration_as_unhealthy_not_usage() {
    let out = bin()
        .arg("healthcheck")
        .env("TODO_MCP_BIND", "not-an-address")
        .output()
        .expect("spawned");
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty(), "{:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let first = stderr.lines().next().unwrap_or_default();
    let parsed: serde_json::Value =
        serde_json::from_str(first).unwrap_or_else(|e| panic!("stderr not JSON: {first:?} ({e})"));
    assert_eq!(parsed["code"], "CONFIG");
}

/// Waits for `child` to connect, failing fast (instead of hanging the suite) if
/// it exits first or never connects.
fn accept_from(listener: &TcpListener, child: &mut Child) -> TcpStream {
    listener.set_nonblocking(true).expect("nonblocking");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("accept failed: {e}"),
        }
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("healthcheck exited before connecting: {status:?}");
        }
        assert!(Instant::now() < deadline, "healthcheck never connected");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// As PID 1 (plain `docker run`, no `--init`) a signal left at its default
/// disposition is dropped, so Ctrl-C could not stop `login`'s device-code wait.
/// `healthcheck` against a listener that never answers is the hermetic stand-in
/// for a blocked one-shot command. Only an installed handler exits 130 / 143: the
/// default disposition reports death by signal (code None), and the probe's own
/// 3 s read timeout reports 1.
#[test]
fn a_blocked_one_shot_command_exits_130_on_sigint_and_143_on_sigterm() {
    for (signal, code) in [("INT", 130), ("TERM", 143)] {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let mut child = bin()
            .arg("healthcheck")
            .env("TODO_MCP_BIND", format!("127.0.0.1:{port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawned");
        // The probe connects only after main() installed the handler.
        let _held_open = accept_from(&listener, &mut child);
        let sent = Command::new("sh")
            .arg("-c")
            .arg(format!("kill -{signal} {}", child.id()))
            .status()
            .expect("sh");
        assert!(sent.success(), "kill -{signal} failed");
        let status = child.wait().expect("waited");
        assert_eq!(
            status.code(),
            Some(code),
            "SIG{signal}: {status:?}. Some(1) means the probe's 3 s read timeout elapsed \
             before the signal arrived; None means no handler was installed"
        );
    }
}

/// `logout` removes the Microsoft sign-in and its debris, never the MCP bearer,
/// and says so. It keeps `.token.lock` too: unlinking a flock file another
/// process holds or waits on breaks mutual exclusion.
#[test]
fn logout_removes_the_sign_in_but_keeps_the_bearer() {
    let dir = std::env::temp_dir().join(format!("todo-mcp-smoke-logout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let run = |cmd: &str| {
        bin()
            .arg(cmd)
            .env("TODO_MCP_DATA_DIR", &dir)
            .output()
            .expect("spawned")
    };
    let bearer = run("token");
    assert!(
        bearer.status.success(),
        "{}",
        String::from_utf8_lossy(&bearer.stderr)
    );
    let sign_in = ["token.json", "token.json.tmp.1.1"];
    for name in sign_in {
        std::fs::write(dir.join(name), b"{}").expect("seeded");
    }
    std::fs::write(dir.join(".token.lock"), b"").expect("seeded");
    let out = run("logout");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for name in sign_in {
        assert!(!dir.join(name).exists(), "{name} survived logout");
    }
    assert!(
        dir.join(".token.lock").exists(),
        "logout removed .token.lock"
    );
    assert_eq!(
        std::fs::read(dir.join("bearer.token")).expect("bearer kept"),
        bearer.stdout
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("token.json deleted"), "{stdout}");
    // Both account kinds' revocation paths, not only the personal one.
    assert!(
        stdout.contains("https://account.microsoft.com/privacy/app-access")
            && stdout.contains("work or school"),
        "{stdout}"
    );
    assert!(
        stdout.contains("bearer.token") && stdout.contains("was kept"),
        "{stdout}"
    );
    let again = run("logout");
    assert!(again.status.success());
    let stdout = String::from_utf8_lossy(&again.stdout);
    assert!(
        stdout.contains("Nothing to do") && stdout.contains("was kept"),
        "{stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// With no bearer file, `logout` must not claim it kept one (and must not
/// create one).
#[test]
fn logout_without_a_bearer_does_not_claim_to_have_kept_one() {
    let dir = std::env::temp_dir().join(format!(
        "todo-mcp-smoke-logout-no-bearer-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("created");
    std::fs::write(dir.join("token.json"), b"{}").expect("seeded");
    let out = bin()
        .arg("logout")
        .env("TODO_MCP_DATA_DIR", &dir)
        .output()
        .expect("spawned");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("token.json deleted"), "{stdout}");
    assert!(!stdout.contains("was kept"), "{stdout}");
    assert!(
        !dir.join("bearer.token").exists(),
        "logout created a bearer"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `token` in a writable data dir generates a 64-hex bearer with NO trailing
/// newline and nothing else on stdout, and reprints the same one next time.
#[test]
fn token_generates_once_and_prints_raw() {
    let dir = std::env::temp_dir().join(format!("todo-mcp-smoke-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let run = || {
        bin()
            .arg("token")
            .env("TODO_MCP_DATA_DIR", &dir)
            .output()
            .expect("spawned")
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let a = String::from_utf8_lossy(&first.stdout).to_string();
    assert_eq!(a.len(), 64, "{a:?}");
    assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
    let second = run();
    assert_eq!(String::from_utf8_lossy(&second.stdout), a);
    // Nothing operational leaked into stdout on the second run either.
    assert!(second.stderr.is_empty(), "{:?}", second.stderr);
    let _ = std::fs::remove_dir_all(&dir);
}
