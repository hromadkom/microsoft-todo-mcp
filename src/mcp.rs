//! Hand-rolled JSON-RPC 2.0 / MCP protocol layer. **No SDK.**
//!
//! Ported from calendar-ics-mcp (<https://github.com/hromadkom/calendar-ics-mcp>)
//! with one structural change: `&self` throughout, so four HTTP workers share
//! one `Arc<McpServer<_>>` with no global mutex — mutability lives in
//! per-concern locks inside the provider.
//!
//! We target the **legacy** protocol family (`2025-11-25` and below — the ones
//! with the `initialize` handshake). `2026-07-28` is a different protocol, not
//! a version bump: never label this server with it (m2 §1).

use serde_json::{Value, json};

pub const LATEST_PROTOCOL_VERSION: &str = "2025-11-25";
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

pub trait ToolProvider: Send + Sync {
    /// The complete `tools` array for `tools/list`.
    fn list_tools(&self) -> Value;
    /// Execute a tool; `Err` becomes a JSON-RPC error (unknown tool), domain
    /// failures are `Ok` results carrying `isError: true`.
    fn call_tool(&self, name: &str, args: &Value) -> Result<Value, RpcError>;
}

pub struct McpServer<T: ToolProvider> {
    pub name: &'static str,
    pub version: &'static str,
    pub tools: T,
}

impl<T: ToolProvider> McpServer<T> {
    /// Handle one raw message. `None` means "no response" (notification, blank
    /// input, or a client response we never asked for). Pure: every transport
    /// and test drives it directly.
    pub fn handle_message(&self, raw: &str) -> Option<Value> {
        let raw = raw.trim_start_matches('\u{feff}').trim();
        if raw.is_empty() {
            return None;
        }
        let Ok(msg) = serde_json::from_str::<Value>(raw) else {
            return Some(error_response(Value::Null, -32700, "Parse error"));
        };
        match msg {
            // Pre-2025-06-18 clients may send JSON-RPC batches.
            Value::Array(batch) => {
                if batch.is_empty() {
                    return Some(error_response(Value::Null, -32600, "Invalid Request"));
                }
                let responses: Vec<Value> =
                    batch.iter().filter_map(|m| self.handle_value(m)).collect();
                (!responses.is_empty()).then_some(Value::Array(responses))
            }
            other => self.handle_value(&other),
        }
    }

    fn handle_value(&self, msg: &Value) -> Option<Value> {
        if !msg.is_object() {
            return Some(error_response(Value::Null, -32600, "Invalid Request"));
        }
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        if id.is_null() {
            // Notification (notifications/initialized, notifications/cancelled)
            // or an unanswerable message: never respond.
            return None;
        }
        let Some(method) = method else {
            // A response from the client — we never send requests; ignore.
            return None;
        };
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

        let result = match method {
            "initialize" => {
                let requested = params.get("protocolVersion").and_then(Value::as_str);
                let negotiated = match requested {
                    Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
                    _ => LATEST_PROTOCOL_VERSION,
                };
                json!({
                    "protocolVersion": negotiated,
                    // No `listChanged`: the tool list is frozen for the process.
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": self.name, "version": self.version },
                    "instructions": "Microsoft To Do for one signed-in account. Start with todo_lists to learn list names, todo_agenda to plan a day, todo_search_tasks for everything else. Dates are local civil dates in the server's configured time zone.",
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({ "tools": self.tools.list_tools() }),
            "tools/call" => {
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Some(error_response(
                        id,
                        -32602,
                        "Invalid params: missing tool name",
                    ));
                };
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.tools.call_tool(name, &args) {
                    Ok(result) => result,
                    Err(rpc) => return Some(error_response(id, rpc.code, &rpc.message)),
                }
            }
            _ => return Some(error_response(id, -32601, "Method not found")),
        };
        Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
    }
}

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    struct DummyTools;

    impl ToolProvider for DummyTools {
        fn list_tools(&self) -> Value {
            json!([{ "name": "dummy" }])
        }

        fn call_tool(&self, name: &str, args: &Value) -> Result<Value, RpcError> {
            match name {
                "dummy" => Ok(json!({ "content": [], "echo": args })),
                "boom" => panic!("handler panic"),
                _ => Err(RpcError {
                    code: -32602,
                    message: format!("Unknown tool: {name}"),
                }),
            }
        }
    }

    fn server() -> McpServer<DummyTools> {
        McpServer {
            name: "microsoft-todo-mcp",
            version: "0.0.0-test",
            tools: DummyTools,
        }
    }

    fn handle(raw: &str) -> Option<Value> {
        server().handle_message(raw)
    }

    #[test]
    fn initialize_echoes_a_supported_version_and_falls_back_otherwise() {
        let res = handle(r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#).unwrap();
        assert_eq!(res["result"]["protocolVersion"], "2025-03-26");
        assert!(res["result"]["capabilities"]["tools"].is_object());
        assert!(
            res["result"]["capabilities"]["tools"]
                .get("listChanged")
                .is_none()
        );
        let res = handle(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2026-07-28"}}"#).unwrap();
        assert_eq!(res["result"]["protocolVersion"], LATEST_PROTOCOL_VERSION);
        assert_ne!(LATEST_PROTOCOL_VERSION, "2026-07-28");
    }

    #[test]
    fn error_codes() {
        assert_eq!(handle("{not json").unwrap()["error"]["code"], -32700);
        assert_eq!(handle("[]").unwrap()["error"]["code"], -32600);
        assert_eq!(
            handle(r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#).unwrap()["error"]["code"],
            -32601
        );
        assert_eq!(
            handle(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"nope"}}"#)
                .unwrap()["error"]["code"],
            -32602
        );
        assert_eq!(
            handle(r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{}}"#).unwrap()["error"]
                ["code"],
            -32602
        );
    }

    #[test]
    fn notifications_and_client_responses_get_none() {
        assert!(handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        assert!(handle(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#).is_none());
        assert!(handle("  ").is_none());
    }

    #[test]
    fn batches_collect_responses_and_drop_notifications() {
        let res = handle(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#).unwrap();
        let arr = res.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], 1);
        assert_eq!(arr[0]["result"], json!({}));
    }

    #[test]
    fn tools_call_dispatches_with_default_arguments() {
        let res = handle(
            r#"{"jsonrpc":"2.0","id":"abc","method":"tools/call","params":{"name":"dummy"}}"#,
        )
        .unwrap();
        assert_eq!(res["id"], "abc");
        assert_eq!(res["result"]["echo"], json!({}));
    }
}
