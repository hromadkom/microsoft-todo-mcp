//! Real-subprocess smoke tests: spawn the actual binary the way Docker will.
//!
//! `env_clear()` matters — it proves the binary does not silently depend on a
//! variable that happens to be set in the developer's shell.

use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
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

/// The right shape for an Application (client) ID, with no registration behind it.
const CLIENT_ID: &str = "12345678-abcd-4321-9876-0123456789ab";

/// A fresh 0700 directory, so `doctor` reports no loose-mode finding for it.
fn private_dir(tag: &str) -> PathBuf {
    use std::os::unix::fs::DirBuilderExt;
    let dir = std::env::temp_dir().join(format!("todo-mcp-smoke-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .expect("created");
    dir
}

/// `doctor`'s FINDING lines that name `var`.
fn findings_naming<'a>(stdout: &'a str, var: &str) -> Vec<&'a str> {
    stdout
        .lines()
        .filter(|l| l.trim_start().starts_with("FINDING") && l.contains(var))
        .collect()
}

/// One rejected variable is one finding, and `doctor` goes on to the data
/// directory and token store. Only the Graph check, which needs the client id,
/// tenant, scope and data dir, is skipped for one of those, and says so. The
/// exact count proves nothing is reported twice: each case is the issue plus the
/// "no token.json" finding.
#[test]
fn doctor_reports_each_configuration_issue_and_keeps_going() {
    let dir = private_dir("doctor-issues");
    let doctor = |vars: &[(&str, &str)]| {
        let out = bin()
            .arg("doctor")
            .env("TODO_MCP_CLIENT_ID", CLIENT_ID)
            .env("TODO_MCP_TZ", "Europe/Prague")
            .env("TODO_MCP_DATA_DIR", &dir)
            .envs(vars.iter().copied())
            .output()
            .expect("spawned");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert_eq!(out.status.code(), Some(1), "{vars:?}: {stdout}");
        stdout
    };
    for (var, value, graph_needs_it) in [
        ("TODO_MCP_TZ", "Mars/Olympus_Mons", false),
        ("TODO_MCP_TENANT", "bad tenant/value", true),
        ("TODO_MCP_SCOPE", "Mail.ReadWrite", true),
        ("TODO_MCP_CLIENT_ID", "not-a-guid-SECRETVALUE", true),
    ] {
        let stdout = doctor(&[(var, value)]);
        assert_eq!(findings_naming(&stdout, var).len(), 1, "{var}: {stdout}");
        assert!(!stdout.contains(value), "{var} echoed its value: {stdout}");
        assert!(
            stdout.contains("\ndata directory\n  path:")
                && stdout.contains("\ntoken store\n  FINDING  no token.json"),
            "{var}: {stdout}"
        );
        assert!(stdout.contains("\n2 finding(s)."), "{var}: {stdout}");
        assert_eq!(
            stdout.contains(&format!("microsoft graph\n  skipped: {var} rejected above")),
            graph_needs_it,
            "{var}: {stdout}"
        );
    }

    // Two issues are two findings, never one joined line.
    let stdout = doctor(&[
        ("TODO_MCP_TZ", "Mars/Olympus_Mons"),
        ("TODO_MCP_TENANT", "bad tenant/value"),
    ]);
    let tz = findings_naming(&stdout, "TODO_MCP_TZ");
    let tenant = findings_naming(&stdout, "TODO_MCP_TENANT");
    assert!(
        tz.len() == 1 && tenant.len() == 1 && tz != tenant,
        "{stdout}"
    );
    assert!(stdout.contains("\n3 finding(s)."), "{stdout}");

    // A relative data dir: both sections that need it are skipped, and nothing is
    // created relative to the working directory.
    let out = bin()
        .arg("doctor")
        .current_dir(&dir)
        .env("TODO_MCP_CLIENT_ID", CLIENT_ID)
        .env("TODO_MCP_TZ", "Europe/Prague")
        .env("TODO_MCP_DATA_DIR", "relative-data-dir")
        .output()
        .expect("spawned");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(!stdout.contains("relative-data-dir"), "{stdout}");
    for section in ["data directory", "token store"] {
        assert!(
            stdout.contains(&format!(
                "\n{section}\n  skipped: TODO_MCP_DATA_DIR was rejected above\n"
            )),
            "{section}: {stdout}"
        );
    }
    assert!(
        stdout.contains("microsoft graph\n  skipped: TODO_MCP_DATA_DIR rejected above"),
        "{stdout}"
    );
    assert!(stdout.contains("\n1 finding(s)."), "{stdout}");
    assert!(
        !dir.join("relative-data-dir").exists(),
        "doctor created the relative data dir"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rejected tenant means the fallback authority is not the operator's: no
/// refresh may run against it, even with a matching token.json. A refresh that
/// ran would add a finding (Microsoft's refusal or the transport error), so the
/// count of exactly one, the tenant, proves it did not.
#[test]
fn doctor_with_a_rejected_tenant_never_refreshes_a_present_token() {
    use microsoft_todo_mcp::auth::Secret;
    use microsoft_todo_mcp::auth::store::{self, SaveMode, TokenFile};

    let dir = private_dir("doctor-tenant-token");
    let token = TokenFile {
        schema_version: store::SCHEMA_VERSION,
        account_id: "default".into(),
        client_id: CLIENT_ID.into(),
        authority: "https://login.microsoftonline.com/common".into(),
        requested_scope: "https://graph.microsoft.com/Tasks.ReadWrite offline_access".into(),
        granted_scope: "https://graph.microsoft.com/Tasks.ReadWrite offline_access".into(),
        refresh_token: Secret::new("rt-fake"),
        obtained_at: "2026-08-25T13:49:05.113000Z".into(),
        obtained_by: "device_code".into(),
        rotated_from: None,
    };
    store::save_atomic(&dir, &token, None, SaveMode::Login).expect("saved");
    let out = bin()
        .arg("doctor")
        .env("TODO_MCP_CLIENT_ID", CLIENT_ID)
        .env("TODO_MCP_TZ", "Europe/Prague")
        .env("TODO_MCP_DATA_DIR", &dir)
        .env("TODO_MCP_HTTP_TIMEOUT_MS", "1000")
        .env("TODO_MCP_TENANT", "bad tenant/value")
        .output()
        .expect("spawned");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("  token.json: present"), "{stdout}");
    assert!(
        stdout.contains("microsoft graph\n  skipped: TODO_MCP_TENANT rejected above"),
        "{stdout}"
    );
    assert!(!stdout.contains("refresh:"), "{stdout}");
    assert!(stdout.contains("\n1 finding(s)."), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// What `doctor` works around, every other subcommand still refuses: `serve`,
/// `login`, `logout` and `token` with exit::USAGE before touching the data dir,
/// `healthcheck` as unhealthy (Docker reserves 2). Neither echoes the value.
#[test]
fn the_other_commands_still_refuse_what_doctor_works_around() {
    let closed_port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    }; // Dropped: nothing listens there now.
    let dir = private_dir("refusals");
    for (var, value) in [
        ("TODO_MCP_TZ", "Mars/Olympus_Mons"),
        ("TODO_MCP_TENANT", "bad tenant/value"),
    ] {
        for (cmd, code) in [
            ("serve", 2),
            ("login", 2),
            ("logout", 2),
            ("token", 2),
            ("healthcheck", 1),
        ] {
            let out = bin()
                .arg(cmd)
                .env("TODO_MCP_CLIENT_ID", CLIENT_ID)
                .env("TODO_MCP_DATA_DIR", &dir)
                .env("TODO_MCP_BIND", format!("127.0.0.1:{closed_port}"))
                .env("TODO_MCP_HTTP_TIMEOUT_MS", "1000")
                .env(var, value)
                .output()
                .expect("spawned");
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert_eq!(out.status.code(), Some(code), "{cmd} {var}: {stderr}");
            assert!(
                out.stdout.is_empty(),
                "{cmd} {var} wrote to stdout: {:?}",
                out.stdout
            );
            let first = stderr.lines().next().unwrap_or_default();
            let parsed: serde_json::Value = serde_json::from_str(first)
                .unwrap_or_else(|e| panic!("{cmd} {var}: stderr not JSON: {first:?} ({e})"));
            assert_eq!(parsed["code"], "CONFIG", "{cmd} {var}: {stderr}");
            assert!(
                parsed["msg"].as_str().unwrap_or_default().contains(var),
                "{cmd} {var}: {stderr}"
            );
            assert!(!stderr.contains(value), "{cmd} echoed {var}: {stderr}");
            assert!(
                !dir.join("bearer.token").exists(),
                "{cmd} {var} created a bearer"
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
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
