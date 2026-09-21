//! The ten tools: registry, dispatch, and the argument helpers they share.
//! Schemas live in `schema`, the output contract in `render`, guards in
//! `guards`; `read`/`write` hold the handlers.

pub mod cursor;
pub mod guards;
pub mod read;
pub mod render;
pub mod schema;
pub mod write;

use chrono::{NaiveDate, NaiveDateTime};
use chrono_tz::Tz;
use serde_json::Value;

use crate::auth::Grant;
use crate::config::{ScopeChoice, is_valid_iana};
use crate::errors::{AppError, LOGIN_HINT, RESTART_HINT};
use crate::mcp::RpcError;
use crate::server::ServerState;

pub const READ_TOOLS: [&str; 5] = [
    "todo_lists",
    "todo_search_tasks",
    "todo_agenda",
    "todo_get_task",
    "todo_account_status",
];

pub const WRITE_TOOLS: [&str; 5] = [
    "todo_create_tasks",
    "todo_update_tasks",
    "todo_complete_tasks",
    "todo_delete_tasks",
    "todo_manage_checklist",
];

/// `tools/list` for a grant: 10 under `ReadWrite`, 5 otherwise. Computed once
/// at construction and frozen (m6 §7); startup without a token uses the
/// configured scope ceiling here.
pub fn build_tools(grant: Grant, tz: &str) -> Value {
    let all = schema::all(tz);
    let keep: Vec<Value> = all
        .into_iter()
        .filter(|t| {
            let name = t["name"].as_str().unwrap_or("");
            READ_TOOLS.contains(&name) || (grant == Grant::ReadWrite && WRITE_TOOLS.contains(&name))
        })
        .collect();
    Value::Array(keep)
}

pub fn dispatch(state: &ServerState, name: &str, args: &Value) -> Result<Value, RpcError> {
    if !args.is_object() {
        return Ok(render::error("arguments must be a JSON object".into()));
    }
    let mut grant = state.effective_grant();
    if WRITE_TOOLS.contains(&name) && grant.is_none() && state.cfg.scope != ScopeChoice::Read {
        match state.graph.tokens().access_token() {
            Ok(_) => grant = state.effective_grant(),
            Err(e) => return Ok(render::failure(&AppError::from_auth(&e), state)),
        }
    }
    match grant {
        Some(g) if WRITE_TOOLS.contains(&name) && g != Grant::ReadWrite => {
            // Under a Read config the grant is capped at Tasks.Read (auth::vet_grant),
            // so "re-run login and restart" would change nothing.
            let text = if state.cfg.scope == ScopeChoice::Read {
                "Refused: this server is configured read-only (TODO_MCP_SCOPE=Tasks.Read), so write tools are not available and restarting will not change that. To enable them the operator sets TODO_MCP_SCOPE=Tasks.ReadWrite, runs login again, then restarts the server.".to_string()
            } else {
                format!(
                    "Refused: the current Microsoft Graph grant is `{}`. Write tools require Tasks.ReadWrite. To enable them, {LOGIN_HINT}. {RESTART_HINT}.",
                    g.as_str()
                )
            };
            return Ok(render::error(text));
        }
        None if WRITE_TOOLS.contains(&name) && state.cfg.scope == ScopeChoice::Read => {
            let text = "Refused: this server is configured read-only (TODO_MCP_SCOPE=Tasks.Read), so write tools are not available and restarting will not change that. To enable them the operator sets TODO_MCP_SCOPE=Tasks.ReadWrite, runs login again, then restarts the server.".to_string();
            return Ok(render::error(text));
        }
        _ => {}
    };
    let result = match name {
        "todo_lists" => read::todo_lists(state, args),
        "todo_search_tasks" => read::todo_search_tasks(state, args),
        "todo_agenda" => read::todo_agenda(state, args),
        "todo_get_task" => read::todo_get_task(state, args),
        "todo_account_status" => read::todo_account_status(state, args),
        "todo_create_tasks" => write::todo_create_tasks(state, args),
        "todo_update_tasks" => write::todo_update_tasks(state, args),
        "todo_complete_tasks" => write::todo_complete_tasks(state, args),
        "todo_delete_tasks" => write::todo_delete_tasks(state, args),
        "todo_manage_checklist" => write::todo_manage_checklist(state, args),
        _ => {
            return Err(RpcError {
                code: -32602,
                message: format!("Unknown tool: {name}"),
            });
        }
    };
    Ok(result)
}

// ---------------------------------------------------------------------------
// Argument helpers. Every failure is a String the caller wraps in `isError`.

pub fn reject_unknown_keys(args: &Value, allowed: &[&str]) -> Result<(), String> {
    let Some(obj) = args.as_object() else {
        return Ok(());
    };
    let unknown: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.contains(k))
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "unknown argument(s): {}; allowed: {}",
            unknown.join(", "),
            allowed.join(", ")
        ))
    }
}

pub fn arg_str<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

pub fn arg_str_max<'a>(
    args: &'a Value,
    key: &str,
    max_chars: usize,
) -> Result<Option<&'a str>, String> {
    let v = arg_str(args, key)?;
    if let Some(s) = v
        && s.chars().count() > max_chars
    {
        return Err(format!("{key} must be at most {max_chars} characters"));
    }
    Ok(v)
}

pub fn arg_bool(args: &Value, key: &str) -> Result<Option<bool>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("{key} must be a boolean")),
    }
}

pub fn arg_u64(args: &Value, key: &str, lo: u64, hi: u64) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if (lo..=hi).contains(&n) => Ok(Some(n)),
            _ => Err(format!("{key} must be an integer between {lo} and {hi}")),
        },
    }
}

pub fn arg_array<'a>(
    args: &'a Value,
    key: &str,
    max: usize,
) -> Result<Option<&'a Vec<Value>>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => {
            if a.len() > max {
                Err(format!("{key} accepts at most {max} items"))
            } else {
                Ok(Some(a))
            }
        }
        Some(_) => Err(format!("{key} must be an array")),
    }
}

pub fn arg_string_array(
    args: &Value,
    key: &str,
    max: usize,
) -> Result<Option<Vec<String>>, String> {
    match arg_array(args, key, max)? {
        None => Ok(None),
        Some(items) => items
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{key} must contain only strings"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
    }
}

pub fn arg_date(args: &Value, key: &str) -> Result<Option<NaiveDate>, String> {
    match arg_str(args, key)? {
        None => Ok(None),
        Some(s) => NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map(Some)
            .map_err(|_| format!("{key} must be a calendar date in YYYY-MM-DD format")),
    }
}

/// `YYYY-MM-DDTHH:MM` (optionally with seconds), a wall clock in the effective zone.
pub fn arg_local_datetime(args: &Value, key: &str) -> Result<Option<NaiveDateTime>, String> {
    match arg_str(args, key)? {
        None => Ok(None),
        Some(s) => NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M")
            .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S"))
            .map(Some)
            .map_err(|_| {
                format!("{key} must be a local date-time in YYYY-MM-DDTHH:MM format (no offset)")
            }),
    }
}

/// The per-call `timezone` override, else the server's effective zone.
pub fn arg_timezone(args: &Value, state: &ServerState) -> Result<Tz, String> {
    match arg_str(args, "timezone")? {
        None => Ok(state.tz),
        Some(s) => is_valid_iana(s).ok_or_else(|| {
            format!("unknown timezone \"{s}\"; expected an IANA name like \"Europe/Prague\".")
        }),
    }
}

pub fn arg_enum<'a>(
    args: &'a Value,
    key: &str,
    allowed: &[&str],
) -> Result<Option<&'a str>, String> {
    match arg_str(args, key)? {
        None => Ok(None),
        Some(s) if allowed.contains(&s) => Ok(Some(s)),
        Some(_) => Err(format!("{key} must be one of: {}", allowed.join(", "))),
    }
}
