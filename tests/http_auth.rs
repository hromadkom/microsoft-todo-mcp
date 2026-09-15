//! The HTTP guards, in order, against a real listening server; plus panic
//! isolation and the one test that proves token separation.

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Value, json};

use common::harness;
use microsoft_todo_mcp::http::{HttpGuards, run_http};
use microsoft_todo_mcp::mcp::{McpServer, RpcError, ToolProvider};

const BEARER: &str = "0123456789abcdef0123456789abcdef";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Running {
    base: String,
    stopping: Arc<AtomicBool>,
    agent: ureq::Agent,
}

impl Running {
    fn start<T: ToolProvider + 'static>(mcp: McpServer<T>, extra_hosts: &[String]) -> Self {
        let port = free_port();
        let bind: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let stopping = Arc::new(AtomicBool::new(false));
        let guards = HttpGuards::new(BEARER, bind, extra_hosts);
        let s2 = stopping.clone();
        std::thread::spawn(move || {
            run_http(Arc::new(mcp), bind, guards, s2).unwrap();
        });
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        let base = format!("http://127.0.0.1:{port}");
        // Wait for the listener.
        for _ in 0..100 {
            if agent.get(format!("{base}/healthz")).call().is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Self {
            base,
            stopping,
            agent,
        }
    }

    fn post(&self, headers: &[(&str, &str)], body: &str) -> (u16, Vec<(String, String)>, String) {
        let mut b = self.agent.post(format!("{}/mcp", self.base));
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let mut res = b.send(body).unwrap();
        let status = res.status().as_u16();
        let headers = res
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = res.body_mut().read_to_string().unwrap_or_default();
        (status, headers, body)
    }

    fn rpc(&self, body: &str) -> (u16, Value) {
        let auth = format!("Bearer {BEARER}");
        let (s, _, b) = self.post(&[("Authorization", &auth)], body);
        (s, serde_json::from_str(&b).unwrap_or(Value::Null))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stopping
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn has_header(h: &[(String, String)], k: &str, v: &str) -> bool {
    h.iter()
        .any(|(hk, hv)| hk.eq_ignore_ascii_case(k) && hv == v)
}

#[test]
fn guards_run_in_order_and_a_bad_bearer_never_reaches_graph() {
    let h = harness(&[]);
    h.fx.add_list("Tasks", Some("defaultList"));
    let state = Arc::clone(&h.state);
    struct Shared(Arc<microsoft_todo_mcp::server::ServerState>);
    impl ToolProvider for Shared {
        fn list_tools(&self) -> Value {
            self.0.list_tools()
        }
        fn call_tool(&self, name: &str, args: &Value) -> Result<Value, RpcError> {
            self.0.call_tool(name, args)
        }
    }
    let srv = Running::start(
        McpServer {
            name: "t",
            version: "0",
            tools: Shared(state),
        },
        &["nas.local".into()],
    );
    let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"todo_lists","arguments":{}}}"#;

    // Wrong / absent bearer → 401 + WWW-Authenticate, and ZERO Graph requests.
    let (s, hd, _) = srv.post(&[("Authorization", "Bearer nope")], call);
    assert_eq!(s, 401);
    assert!(has_header(&hd, "www-authenticate", "Bearer"));
    let (s, _, _) = srv.post(&[], call);
    assert_eq!(s, 401);
    // A Graph access token is not MCP auth.
    let (s, _, _) = srv.post(&[("Authorization", "Bearer AT-1")], call);
    assert_eq!(s, 401);
    assert_eq!(
        h.fx.request_count(),
        0,
        "a rejected bearer must not reach Graph"
    );

    let auth = format!("Bearer {BEARER}");
    // Surrounding whitespace is trimmed (the raw `\r` case is a unit test in http.rs —
    // the HTTP client refuses to send one).
    let padded = format!("Bearer {BEARER} ");
    let (s, _, _) = srv.post(
        &[("Authorization", &padded)],
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
    );
    assert_eq!(s, 200);
    // Absent Origin → 200; foreign Origin → 403; loopback Origin → 200.
    let (s, hd, body) = srv.post(&[("Authorization", &auth)], call);
    assert_eq!(s, 200, "{body}");
    assert!(has_header(&hd, "cache-control", "no-store"));
    let (s, _, _) = srv.post(
        &[("Authorization", &auth), ("Origin", "https://evil.example")],
        call,
    );
    assert_eq!(s, 403);
    let (s, _, _) = srv.post(
        &[
            ("Authorization", &auth),
            ("Origin", "http://localhost:3000"),
        ],
        call,
    );
    assert_eq!(s, 200);
    // Foreign Host → 403 (DNS rebinding); allow-listed Host → 200.
    let (s, _, _) = srv.post(&[("Authorization", &auth), ("Host", "evil.example")], call);
    assert_eq!(s, 403);
    let (s, _, _) = srv.post(
        &[("Authorization", &auth), ("Host", "nas.local:8591")],
        call,
    );
    assert_eq!(s, 200);
    // GET /mcp → 405; unknown path → 404; /healthz unauthenticated 200.
    let s = srv
        .agent
        .get(format!("{}/mcp", srv.base))
        .call()
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(s, 405);
    let s = srv
        .agent
        .get(format!("{}/nope", srv.base))
        .call()
        .unwrap()
        .status()
        .as_u16();
    assert_eq!(s, 404);
    let mut res = srv
        .agent
        .get(format!("{}/healthz", srv.base))
        .call()
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert!(
        res.body_mut()
            .read_to_string()
            .unwrap()
            .contains("\"status\":\"ok\"")
    );
    // 2 MiB body → 413.
    let big = "x".repeat(2 * 1024 * 1024);
    let (s, _, _) = srv.post(&[("Authorization", &auth)], &big);
    assert_eq!(s, 413);
    // Notification → 202 with an empty body.
    let (s, _, b) = srv.post(
        &[("Authorization", &auth)],
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    );
    assert_eq!(s, 202);
    assert!(b.is_empty());
    // The full handshake a legacy client performs.
    let (_, init) = srv.rpc(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#);
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    let (_, list) = srv.rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 10);
    let (_, lists) = srv.rpc(call);
    assert_eq!(
        lists["result"]["structuredContent"]["lists"][0]["name"],
        "Tasks"
    );
}

#[test]
fn a_handler_panic_is_isolated_and_the_worker_survives() {
    struct Boom;
    impl ToolProvider for Boom {
        fn list_tools(&self) -> Value {
            json!([])
        }
        fn call_tool(&self, name: &str, _args: &Value) -> Result<Value, RpcError> {
            if name == "boom" {
                panic!("handler panic");
            }
            Ok(json!({ "content": [], "structuredContent": {} }))
        }
    }
    let srv = Running::start(
        McpServer {
            name: "t",
            version: "0",
            tools: Boom,
        },
        &[],
    );
    for round in 0..6 {
        let (s, v) =
            srv.rpc(r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"boom"}}"#);
        assert_eq!(s, 200, "round {round}");
        assert_eq!(v["error"]["code"], -32603);
        assert_eq!(v["id"], 7);
        let (s, v) =
            srv.rpc(r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"fine"}}"#);
        assert_eq!(s, 200);
        assert!(v["result"].is_object(), "worker died after a panic: {v}");
    }
}
