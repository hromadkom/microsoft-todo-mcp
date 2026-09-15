//! Shutdown against real processes and real signals (m7 §2, §10).
//!
//! `serve` cannot reach its listener offline: the boot refresh needs a live
//! Entra (https-only, a fixed authority host, and the access token is never
//! persisted), and a release-shaped binary has no fixture seam. So two kinds of
//! subprocess, both started with `env_clear()`:
//!
//! - the real binary, parked inside the boot refresh by this test's own flock
//!   on `.token.lock`, which proves the handlers exist during the refresh;
//! - this test binary re-executed as `child_process_entry`, running the same
//!   `Shutdown` + `run_http` that `serve` runs with a stub tool, which proves the
//!   drain, an in-flight response, the drain-deadline warning and the
//!   second-signal exit.
//!
//! Only the refresh window of boot is regression-tested. Nothing in config, the
//! `/data` probe or bearer generation blocks hermetically, so a regression that
//! moved `Shutdown::install()` below them but still above `access_token()` would
//! pass here.
//!
//! No token.json is ever written, so nothing here can reach the network even if
//! a synchronisation assumption breaks: `serve` would exit 3 instead.
//!
//! Never call `Shutdown::install()` in a normal test: outside the re-executed
//! child it would make Ctrl-C `_exit(0)` the test runner.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::os::unix::fs::DirBuilderExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use microsoft_todo_mcp::cli::commands::Shutdown;
use microsoft_todo_mcp::http::{HttpGuards, run_http};
use microsoft_todo_mcp::logger;
use microsoft_todo_mcp::mcp::{McpServer, RpcError, ToolProvider};

const CHILD_ENV: &str = "TODO_MCP_SHUTDOWN_TEST_CHILD";
const CHILD_TEST: &str = "child_process_entry";
const BEARER: &str = "0123456789abcdef0123456789abcdef";
const ENTERED: &str = "shutdown-test: tool entered";
const RETURNED: &str = "shutdown-test: run_http returned";
const ABANDONED: &str = "drain deadline reached; abandoning in-flight requests";
/// `http::DRAIN`, restated: the documented 5 s is the claim under test.
const DRAIN: Duration = Duration::from_secs(5);
/// Generous: only the post-signal budgets are claims under test.
const STARTUP: Duration = Duration::from_secs(20);
const CALL: &str =
    r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"slow","arguments":{}}}"#;

/// SIGKILLs and reaps on drop, so a failed assertion never leaks a process.
struct Proc {
    child: Child,
    lines: Receiver<String>,
    seen: Vec<String>,
}

impl Proc {
    fn spawn(mut cmd: Command) -> Self {
        let mut child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawned");
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            seen: Vec::new(),
        }
    }

    /// The first stderr JSON line whose `msg` equals `msg`.
    fn wait_for(&mut self, msg: &str) -> Value {
        let deadline = Instant::now() + STARTUP;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let Ok(line) = self.lines.recv_timeout(left) else {
                break;
            };
            self.seen.push(line.clone());
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && v["msg"] == msg
            {
                return v;
            }
        }
        panic!("no {msg:?} on stderr; saw: {:#?}", self.seen);
    }

    fn signal(&self, sig: &str) {
        // std's Child::kill is SIGKILL. `kill` is a POSIX sh builtin, so this
        // needs no procps binary in a slim image.
        let pid = self.child.id().to_string();
        let ok = Command::new("sh")
            .args(["-c", "kill -s \"$1\" \"$2\"", "sh", sig, &pid])
            .status()
            .expect("sh")
            .success();
        assert!(ok, "kill -s {sig} {pid} failed");
    }

    fn running(&mut self) -> bool {
        self.child.try_wait().expect("try_wait").is_none()
    }

    fn exit_within(&mut self, budget: Duration) -> ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(
                start.elapsed() < budget,
                "still running {budget:?} after the signal; saw: {:#?}",
                self.seen
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Everything the child wrote; call after it has exited (the pipe is at EOF).
    fn log(&mut self) -> Vec<String> {
        while let Ok(line) = self.lines.recv_timeout(Duration::from_secs(5)) {
            self.seen.push(line);
        }
        self.seen.clone()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn has(log: &[String], needle: &str) -> bool {
    log.iter().any(|l| l.contains(needle))
}

fn child(mode: &str) -> Proc {
    let mut cmd = Command::new(std::env::current_exe().expect("test binary path"));
    cmd.env_clear().env(CHILD_ENV, mode).args([
        CHILD_TEST,
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ]);
    Proc::spawn(cmd)
}

fn base_url(p: &mut Proc) -> String {
    let v = p.wait_for("http transport listening");
    format!("http://{}", v["bind"].as_str().expect("bind"))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into()
}

type CallResult = Result<(u16, String), ureq::Error>;

fn call_in_background(base: &str) -> JoinHandle<CallResult> {
    let url = format!("{base}/mcp");
    std::thread::spawn(move || {
        let auth = format!("Bearer {BEARER}");
        let mut res = agent()
            .post(&url)
            .header("Authorization", &auth)
            .send(CALL)?;
        let status = res.status().as_u16();
        Ok((status, res.body_mut().read_to_string()?))
    })
}

#[test]
fn a_signal_during_the_boot_refresh_exits_zero_at_once() {
    for sig in ["TERM", "INT"] {
        let dir =
            std::env::temp_dir().join(format!("todo-mcp-shutdown-{}-{sig}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .expect("data dir");
        // Hold the token-store lock: `serve` gets past config, the /data probe
        // and the bearer, then blocks inside `access_token()`, the boot refresh.
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(dir.join(".token.lock"))
            .expect("lock file");
        lock.lock().expect("flock");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_todo-mcp"));
        cmd.env_clear()
            .arg("serve")
            .env("TODO_MCP_CLIENT_ID", "00000000-0000-0000-0000-000000000000")
            .env("TODO_MCP_DATA_DIR", &dir)
            .env("TODO_MCP_BIND", "127.0.0.1:0")
            .env("TODO_MCP_TZ", "UTC");
        let mut p = Proc::spawn(cmd);
        p.wait_for("generated a new MCP bearer token");
        assert!(p.running(), "serve should be parked on .token.lock");

        p.signal(sig);
        let status = p.exit_within(Duration::from_secs(2));
        // Without a handler during boot the child dies by the signal (code None).
        assert_eq!(status.code(), Some(0), "SIG{sig} during boot: {status:?}");
        let log = p.log();
        assert!(!has(&log, "microsoft-todo-mcp started"), "{log:#?}");
        assert!(!dir.join("token.json").exists());

        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn sigterm_to_an_idle_server_exits_zero_within_two_seconds() {
    let mut p = child("idle");
    let base = base_url(&mut p);
    let ok = agent()
        .get(format!("{base}/healthz"))
        .call()
        .expect("healthz");
    assert_eq!(ok.status().as_u16(), 200);

    p.signal("TERM");
    let status = p.exit_within(Duration::from_secs(2));
    assert_eq!(status.code(), Some(0), "{status:?}");
    let log = p.log();
    assert!(has(&log, RETURNED), "drain did not complete: {log:#?}");
    assert!(!has(&log, ABANDONED), "{log:#?}");
    // m7 §2's treadmill: a worker leaving during the drain must not be respawned.
    assert!(!has(&log, "respawning"), "{log:#?}");
}

#[test]
fn an_in_flight_request_is_answered_before_exit() {
    let mut p = child("slow");
    let base = base_url(&mut p);
    let call = call_in_background(&base);
    p.wait_for(ENTERED);

    p.signal("TERM");
    let (status, body) = call
        .join()
        .expect("client thread")
        .expect("the response is written before exit");
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(v["result"]["structuredContent"]["done"], true, "{body}");
    let exit = p.exit_within(Duration::from_secs(5));
    assert_eq!(exit.code(), Some(0), "{exit:?}");
    let log = p.log();
    assert!(has(&log, RETURNED), "{log:#?}");
    assert!(!has(&log, ABANDONED), "{log:#?}");
}

#[test]
fn a_drain_that_hits_its_deadline_says_so_and_exits_zero() {
    let mut p = child("stuck");
    let base = base_url(&mut p);
    let _call = call_in_background(&base); // never answered; dies with the child
    p.wait_for(ENTERED);

    let signalled = Instant::now();
    p.signal("TERM");
    let status = p.exit_within(DRAIN + Duration::from_secs(2));
    let waited = signalled.elapsed();
    assert_eq!(status.code(), Some(0), "{status:?}");
    // The child starts its deadline only after it saw the signal, so it cannot
    // exit sooner than DRAIN after we sent it.
    assert!(
        waited >= DRAIN - Duration::from_millis(100),
        "exited {waited:?} after SIGTERM; the drain should wait {DRAIN:?}"
    );
    let log = p.log();
    let warn = log
        .iter()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["msg"] == ABANDONED)
        .unwrap_or_else(|| panic!("no drain-deadline warning: {log:#?}"));
    assert_eq!(warn["level"], "warn", "{warn}");
    // Only the stuck worker; the idle three leave on their recv_timeout.
    assert_eq!(warn["unfinished"], 1, "{warn}");
    // run_http still returns Ok, so `serve` would log `stopped` after this.
    assert!(has(&log, RETURNED), "{log:#?}");
}

#[test]
fn a_second_signal_abandons_the_drain_and_exits_zero() {
    let mut p = child("stuck");
    let base = base_url(&mut p);
    let _call = call_in_background(&base); // never answered; dies with the child
    p.wait_for(ENTERED);

    p.signal("TERM");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        p.running(),
        "the first signal must drain, not exit, while a call is in flight"
    );
    p.signal("TERM");
    let status = p.exit_within(Duration::from_secs(1));
    assert_eq!(status.code(), Some(0), "{status:?}");
    let log = p.log();
    assert!(
        !has(&log, RETURNED),
        "exited via the drain, not the second signal: {log:#?}"
    );
    assert!(!has(&log, ABANDONED), "{log:#?}");
}

/// Not a test: the body of the re-executed child. A no-op in a normal run.
#[test]
fn child_process_entry() {
    let Ok(mode) = std::env::var(CHILD_ENV) else {
        return;
    };
    let delay = match mode.as_str() {
        "slow" => Duration::from_millis(1_000),
        "stuck" => Duration::from_secs(60),
        _ => Duration::ZERO,
    };
    let shutdown = Shutdown::install().expect("signal handlers");
    let stopping = shutdown.serving();
    let bind: std::net::SocketAddr = "127.0.0.1:0".parse().expect("addr");
    let mcp = McpServer {
        name: "shutdown-test",
        version: "0",
        tools: Sleeper(delay),
    };
    let code = match run_http(
        Arc::new(mcp),
        bind,
        HttpGuards::new(BEARER, bind, &[]),
        stopping,
    ) {
        Ok(()) => {
            logger::info(RETURNED, &[]);
            0
        }
        Err(e) => {
            logger::error(&e, &[]);
            1
        }
    };
    std::process::exit(code);
}

struct Sleeper(Duration);

impl ToolProvider for Sleeper {
    fn list_tools(&self) -> Value {
        json!([])
    }
    fn call_tool(&self, _name: &str, _args: &Value) -> Result<Value, RpcError> {
        logger::info(ENTERED, &[]);
        std::thread::sleep(self.0);
        Ok(json!({ "content": [], "structuredContent": { "done": true } }))
    }
}
