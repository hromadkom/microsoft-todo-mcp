//! MCP Streamable HTTP transport over tiny_http (m2 §3–§4, m7 §2).
//!
//! `POST /mcp` carries JSON-RPC; `GET /healthz` is unauthenticated and never
//! touches Graph or the token store; everything else is 404/405. Guards run
//! outermost first — bearer, then `Origin`, then `Host`, then body size — so an
//! unauthenticated request is rejected before any handler work.
//!
//! This file is one of the two places (with `graph/client.rs`) where the
//! string `"Authorization"` may appear.

use std::io::Read;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use subtle::ConstantTimeEq;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::logger;
use crate::mcp::{McpServer, ToolProvider, error_response};

/// Requests are single JSON-RPC messages — anything above this is abuse.
pub const MAX_BODY_BYTES: u64 = 1_048_576;
pub const WORKERS: usize = 4;
/// rustls handshakes want real stack; musl's default thread stack is small.
const WORKER_STACK: usize = 512 * 1024;
/// Bounded respawns before the process gives up and exits 1.
const MAX_RESPAWNS: usize = 16;
/// Drain window after `stopping` flips (m7 §2 `GRACE`). Under compose
/// `stop_grace_period: 10s` and `docker stop`'s default 10 s, so our hard exit,
/// not SIGKILL, bounds a stuck Graph call.
const DRAIN: Duration = Duration::from_secs(5);

/// Everything the guards need. Cloned into each worker.
#[derive(Clone)]
pub struct HttpGuards {
    pub bearer: Arc<Vec<u8>>,
    /// Lower-cased hostnames accepted in `Host` (port stripped).
    pub allowed_hosts: Arc<Vec<String>>,
}

impl HttpGuards {
    pub fn new(bearer: &str, bind: SocketAddr, extra: &[String]) -> Self {
        let mut hosts = vec![
            "localhost".to_string(),
            "127.0.0.1".to_string(),
            "[::1]".to_string(),
        ];
        if !bind.ip().is_unspecified() {
            hosts.push(host_of(&bind.to_string()));
        }
        hosts.extend(extra.iter().map(|h| host_of(h)));
        Self {
            bearer: Arc::new(bearer.trim().as_bytes().to_vec()),
            allowed_hosts: Arc::new(hosts),
        }
    }
}

/// Run the server until `stopping` flips, then drain for up to `DRAIN`.
///
/// Returns `Ok` whether or not the drain finished; a drain that hits its
/// deadline logs a warning with the number of workers it abandoned.
pub fn run_http<T: ToolProvider + 'static>(
    mcp: Arc<McpServer<T>>,
    bind: SocketAddr,
    guards: HttpGuards,
    stopping: Arc<AtomicBool>,
) -> Result<(), String> {
    let server = Server::http(bind).map_err(|e| format!("failed to bind {bind}: {e}"))?;
    let actual = server
        .server_addr()
        .to_ip()
        .map(|a| a.to_string())
        .unwrap_or_else(|| bind.to_string());
    logger::info(
        "http transport listening",
        &[("bind", json!(actual)), ("workers", json!(WORKERS))],
    );
    let server = Arc::new(server);
    let respawns = AtomicUsize::new(0);

    let spawn = |i: usize| {
        let server = Arc::clone(&server);
        let mcp = Arc::clone(&mcp);
        let guards = guards.clone();
        let stopping = Arc::clone(&stopping);
        std::thread::Builder::new()
            .name(format!("mcp-worker-{i}"))
            .stack_size(WORKER_STACK)
            .spawn(move || worker_loop(&server, &mcp, &guards, &stopping))
            .map_err(|e| format!("failed to spawn worker: {e}"))
    };

    let mut workers: Vec<(usize, std::thread::JoinHandle<()>)> = Vec::new();
    for i in 0..WORKERS {
        workers.push((i, spawn(i)?));
    }

    // Supervise. A worker that returns while we are NOT stopping panicked
    // outside the per-request catch_unwind (or hit an accept error) and is
    // replaced, bounded; a worker returning during a drain is normal.
    while !stopping.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(200));
        let mut i = 0;
        while i < workers.len() {
            if workers[i].1.is_finished() {
                let (idx, handle) = workers.remove(i);
                let _ = handle.join();
                if stopping.load(Ordering::SeqCst) {
                    continue;
                }
                let n = respawns.fetch_add(1, Ordering::SeqCst) + 1;
                logger::error(
                    "worker exited; respawning",
                    &[("worker", json!(idx)), ("respawn", json!(n))],
                );
                if n > MAX_RESPAWNS {
                    logger::error("too many worker respawns; exiting", &[]);
                    return Err("worker pool collapsed".to_string());
                }
                workers.push((idx, spawn(idx)?));
            } else {
                i += 1;
            }
        }
    }

    logger::info("shutting down: draining in-flight requests", &[]);
    server.unblock();
    let deadline = Instant::now() + DRAIN;
    let mut unfinished = 0usize;
    for (_, handle) in workers {
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            unfinished += 1;
        }
    }
    // Anything still running is abandoned: the hard exit is safe because a
    // refresh interrupted before its durable write leaves the old refresh
    // token valid, and the kernel releases the flock (m7 §2). Said out loud,
    // because `serve` logs `stopped` and exits 0 either way.
    if unfinished > 0 {
        logger::warn(
            "drain deadline reached; abandoning in-flight requests",
            &[("unfinished", json!(unfinished))],
        );
    }
    Ok(())
}

fn worker_loop<T: ToolProvider>(
    server: &Server,
    mcp: &McpServer<T>,
    guards: &HttpGuards,
    stopping: &AtomicBool,
) {
    // catch_unwind around the WHOLE loop, not just the handler: a panic in
    // recv, in the guard code or in the response write must not leave a
    // listening socket with no acceptors.
    let result = catch_unwind(AssertUnwindSafe(|| {
        while !stopping.load(Ordering::SeqCst) {
            match server.recv_timeout(Duration::from_millis(500)) {
                Ok(Some(req)) => handle_request(req, mcp, guards),
                Ok(None) => continue,
                Err(_) => break,
            }
        }
    }));
    if result.is_err() {
        logger::error("worker loop panicked", &[]);
    }
}

fn handle_request<T: ToolProvider>(mut req: Request, mcp: &McpServer<T>, guards: &HttpGuards) {
    let path = req.url().split('?').next().unwrap_or("").to_string();
    match (req.method().clone(), path.as_str()) {
        (Method::Get, "/healthz") => {
            // Never touches Graph, the token store, or Server::num_connections()
            // (which is unimplemented!() in tiny_http 0.12).
            let body = json!({ "status": "ok", "version": env!("CARGO_PKG_VERSION") });
            respond(req, 200, Some(body.to_string()));
        }
        (Method::Post, "/mcp") => {
            if !bearer_ok(&req, &guards.bearer) {
                return respond_error(req, 401, "missing or invalid bearer token");
            }
            if !origin_allowed(&req) {
                return respond_error(req, 403, "forbidden origin");
            }
            if !host_allowed(&req, &guards.allowed_hosts) {
                return respond_error(req, 403, "forbidden host");
            }
            let mut body = Vec::new();
            let mut limited = req.as_reader().take(MAX_BODY_BYTES + 1);
            if limited.read_to_end(&mut body).is_err() {
                return respond_error(req, 400, "unreadable request body");
            }
            if body.len() as u64 > MAX_BODY_BYTES {
                return respond_error(req, 413, "request body too large");
            }
            let body = String::from_utf8_lossy(&body).into_owned();
            let response = match catch_unwind(AssertUnwindSafe(|| mcp.handle_message(&body))) {
                Ok(r) => r,
                Err(_) => {
                    let id = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| v.get("id").cloned())
                        .unwrap_or(serde_json::Value::Null);
                    Some(error_response(id, -32603, "Internal error"))
                }
            };
            match response {
                Some(value) => respond(req, 200, Some(value.to_string())),
                // Notifications only — accepted, nothing to say.
                None => respond(req, 202, None),
            }
        }
        (_, "/mcp") => {
            // No SSE stream is offered; only POST carries messages.
            respond_error(req, 405, "method not allowed; POST JSON-RPC to /mcp");
        }
        _ => respond_error(req, 404, "not found"),
    }
}

fn header_value(req: &Request, name: &'static str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

/// Constant-time compare, both sides trimmed (a TTY-allocated `docker compose
/// run` appends `\r` to a captured token).
fn bearer_ok(req: &Request, expected: &[u8]) -> bool {
    let Some(v) = header_value(req, "Authorization") else {
        return false;
    };
    bearer_matches(&v, expected)
}

/// The pure half of the bearer guard, so the `\r` trim is unit-testable.
pub fn bearer_matches(value: &str, expected: &[u8]) -> bool {
    let v = value.trim();
    let Some(token) = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))
    else {
        return false;
    };
    let token = token.trim().as_bytes();
    !expected.is_empty() && bool::from(token.ct_eq(expected))
}

/// Absent Origin (non-browser client) is allowed. A present Origin must be a
/// localhost variant or match the request's Host.
fn origin_allowed(req: &Request) -> bool {
    let Some(origin) = header_value(req, "Origin") else {
        return true;
    };
    if origin.trim().eq_ignore_ascii_case("null") {
        return false;
    }
    let Some(origin_host) = crate::errors::parse_url(&origin).map(|p| p.host) else {
        return false;
    };
    if matches!(origin_host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
        return true;
    }
    header_value(req, "Host")
        .map(|h| host_of(&h))
        .is_some_and(|request_host| request_host == origin_host)
}

/// Origin alone does not cover DNS rebinding; the Host must be one we serve.
fn host_allowed(req: &Request, allowed: &[String]) -> bool {
    let Some(h) = header_value(req, "Host") else {
        return false;
    };
    let host = host_of(&h);
    allowed.contains(&host)
}

/// Hostname, lower-cased, port stripped, IPv6 brackets kept.
fn host_of(hostport: &str) -> String {
    let s = hostport.trim();
    if s.starts_with('[') {
        match s.find(']') {
            Some(i) => s[..=i].to_ascii_lowercase(),
            None => s.to_ascii_lowercase(),
        }
    } else {
        s.split(':').next().unwrap_or("").to_ascii_lowercase()
    }
}

fn respond(req: Request, status: u16, json_body: Option<String>) {
    let mut response =
        Response::from_string(json_body.unwrap_or_default()).with_status_code(status);
    for (k, v) in [
        ("Content-Type", "application/json"),
        ("Cache-Control", "no-store"),
    ] {
        if let Ok(header) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
            response = response.with_header(header);
        }
    }
    if status == 401
        && let Ok(header) = Header::from_bytes(&b"WWW-Authenticate"[..], &b"Bearer"[..])
    {
        response = response.with_header(header);
    }
    let _ = req.respond(response);
}

fn respond_error(req: Request, status: u16, message: &str) {
    respond(req, status, Some(json!({ "error": message }).to_string()));
}

/// The `healthcheck` subcommand: a raw `GET /healthz`. The scratch image has
/// no shell and no curl, so the binary is its own probe. Never touches `/data`.
pub fn health_probe(bind: SocketAddr) -> bool {
    use std::io::Write as _;

    let mut addr = bind;
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        } else {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        });
    }
    let timeout = Duration::from_secs(3);
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    if stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut response = String::new();
    let _ = stream.take(1024).read_to_string(&mut response);
    response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_compare_trims_the_tty_carriage_return_and_is_exact() {
        assert!(bearer_matches("Bearer abc123\r\n", b"abc123"));
        assert!(bearer_matches("  Bearer abc123 ", b"abc123"));
        assert!(bearer_matches("bearer abc123", b"abc123"));
        assert!(!bearer_matches("Bearer abc12", b"abc123"));
        assert!(!bearer_matches("Bearer abc1234", b"abc123"));
        assert!(!bearer_matches("abc123", b"abc123"));
        assert!(!bearer_matches("Bearer ", b""));
    }

    #[test]
    fn host_of_strips_ports_and_keeps_brackets() {
        assert_eq!(host_of("127.0.0.1:8591"), "127.0.0.1");
        assert_eq!(host_of("LocalHost"), "localhost");
        assert_eq!(host_of("[::1]:8591"), "[::1]");
    }

    #[test]
    fn guards_include_loopback_bind_and_extras() {
        let g = HttpGuards::new(
            " tok\r\n",
            "192.168.1.5:8591"
                .parse()
                .unwrap_or_else(|_| unreachable!()),
            &["nas.local:8591".into()],
        );
        assert_eq!(g.bearer.as_slice(), b"tok");
        assert!(g.allowed_hosts.contains(&"192.168.1.5".to_string()));
        assert!(g.allowed_hosts.contains(&"nas.local".to_string()));
        assert!(g.allowed_hosts.contains(&"localhost".to_string()));
    }
}
