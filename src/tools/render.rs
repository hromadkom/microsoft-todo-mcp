//! The output contract (m2 §7): every success carries `structuredContent` and
//! `content[0].text` is its exact serialization; errors are `isError` with no
//! structured content. Plus `TaskSummary` and the response-cap ladder.

use chrono_tz::Tz;
use serde_json::{Value, json};

use crate::domain::datetime::{local_date, parse_instant, resolve};
use crate::domain::text::{html_to_text, truncate_bytes};
use crate::errors::AppError;
use crate::graph::models::Task;
use crate::server::ServerState;

pub const BODY_PREVIEW_BYTES: usize = 280;
pub const BODY_MAX_BYTES: usize = 8192;

/// Success: one serialization, no drift.
pub fn success(structured: Value) -> Value {
    let text = structured.to_string();
    json!({ "content": [{ "type": "text", "text": text }], "structuredContent": structured })
}

pub fn error(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

/// Domain failure → `isError`. `AUTH_REQUIRED` carries the sign-in instruction.
pub fn failure(err: &AppError, state: &ServerState) -> Value {
    match err {
        AppError::NotLoggedIn => error(format!(
            "auth_required: no usable Microsoft sign-in. The user must {}.",
            state.login_command()
        )),
        AppError::Auth {
            code,
            summary,
            remediation,
        } => error(format!("auth_failed ({code}): {summary}. {remediation}")),
        other => error(other.message()),
    }
}

pub fn invalid_args(tool: &str, detail: &str) -> Value {
    error(format!("Invalid arguments for tool {tool}: {detail}"))
}

/// A checklist or attachment read that this call's read budget or deadline cut
/// short (`graph::tasks::Walk::Incomplete`). The remedy depends on whether the
/// caller passed `list`: without it a lookup may have spent the budget, and at
/// `TODO_MCP_CACHE_TTL_SECONDS=0` calling again repeats that lookup; with it,
/// passing `list` is no advice at all.
pub fn incomplete_walk(state: &ServerState, what: &str, list_passed: bool) -> Value {
    let next = if list_passed {
        "The task's own reads need more than this call allows: the operator can raise TODO_MCP_MAX_PAGES or TODO_MCP_TOOL_DEADLINE_MS, and calling again helps only if the deadline was the cause."
    } else if state.cfg.cache_ttl_seconds == 0 {
        "Pass \"list\" to skip the task lookup; the cache is disabled (TODO_MCP_CACHE_TTL_SECONDS=0), so calling again repeats the same lookup."
    } else {
        "Pass \"list\" to skip the task lookup, or call again (lists already synced are cached)."
    };
    error(format!(
        "the request budget or deadline for this tool call ran out before the task's {what} was read completely. {next}"
    ))
}

/// Body text (HTML converted) and its byte total.
pub fn body_text(task: &Task) -> (String, usize) {
    let Some(b) = &task.body else {
        return (String::new(), 0);
    };
    let text = if b.content_type.eq_ignore_ascii_case("html") {
        html_to_text(&b.content)
    } else {
        b.content.trim().to_string()
    };
    let total = task.body_bytes_total.unwrap_or(text.len()).max(text.len());
    (text, total)
}

/// A wall-clock rendering of a Graph date-time in `tz`, `YYYY-MM-DDTHH:MM`.
fn local_datetime_str(dtz: &crate::graph::models::DateTimeTimeZone, tz: Tz) -> Option<String> {
    resolve(dtz, tz)
        .ok()
        .map(|r| r.local.format("%Y-%m-%dT%H:%M").to_string())
}

fn instant_str(s: &Option<String>) -> Value {
    match s.as_deref().and_then(parse_instant) {
        Some(t) => json!(t.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        None => Value::Null,
    }
}

/// The item type of every multi-task result (m4 §4).
pub fn task_summary(
    task: &Task,
    list_id: &str,
    list_name: &str,
    tz: Tz,
    state: &ServerState,
) -> Value {
    let (due, due_unresolved) = match &task.due_date_time {
        None => (Value::Null, Value::Null),
        Some(d) => match local_date(d, tz) {
            Ok(date) => (json!(date.format("%Y-%m-%d").to_string()), Value::Null),
            Err(_) => {
                state.warn_tz_once(&d.time_zone);
                (Value::Null, json!(d.time_zone))
            }
        },
    };
    let start = task
        .start_date_time
        .as_ref()
        .and_then(|d| local_date(d, tz).ok())
        .map(|d| json!(d.format("%Y-%m-%d").to_string()))
        .unwrap_or(Value::Null);
    let completed_at = task
        .completed_date_time
        .as_ref()
        .and_then(|d| local_datetime_str(d, tz))
        .map(Value::String)
        .unwrap_or(Value::Null);
    let reminder_at = if task.is_reminder_on {
        task.reminder_date_time
            .as_ref()
            .and_then(|d| local_datetime_str(d, tz))
            .map(Value::String)
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    let (text, total) = body_text(task);
    let (preview, truncated) = truncate_bytes(&text, BODY_PREVIEW_BYTES);
    let preview = if preview.is_empty() {
        Value::Null
    } else {
        json!(preview)
    };
    let (checklist_open, checklist_total, has_checklist) = match &task.checklist_items {
        Some(items) => (
            json!(items.iter().filter(|i| !i.is_checked).count()),
            json!(items.len()),
            json!(!items.is_empty()),
        ),
        None => (Value::Null, Value::Null, Value::Null),
    };
    json!({
        "task_id": task.id,
        "list_id": list_id,
        "list_name": list_name,
        "title": task.title,
        "status": task.status,
        "importance": task.importance,
        "due": due,
        "due_unresolved": due_unresolved,
        "start": start,
        "completed_at": completed_at,
        "created_at": instant_str(&task.created_date_time),
        "modified_at": instant_str(&task.last_modified_date_time),
        "categories": task.categories,
        "has_checklist": has_checklist,
        "checklist_open": checklist_open,
        "checklist_total": checklist_total,
        "is_recurring": task.recurrence.is_some(),
        "reminder_at": reminder_at,
        "body_preview": preview,
        "body_truncated": truncated || total > text.len(),
        "body_bytes_total": total,
        "etag": task.etag,
    })
}

/// Apply the per-tool response-cap ladder (m4 §7). `shrink(step, structured)`
/// applies step `step` (0-based) and returns `false` when no step remains.
/// Always writes `truncation` into the result.
pub fn finish(
    state: &ServerState,
    mut structured: Value,
    original_items: usize,
    shrink: &mut dyn FnMut(usize, &mut Value) -> bool,
    terminal_error: &dyn Fn(&Value) -> String,
) -> Value {
    let cap = state.cfg.tool_result_max_bytes;
    let mut steps: Vec<&'static str> = Vec::new();
    let mut bytes = structured.to_string().len();
    let mut step = 0;
    while bytes > cap && step < 8 {
        if !shrink(step, &mut structured) {
            return error(terminal_error(&structured));
        }
        steps.push(match step {
            0 => "bodies_dropped",
            _ => "page_halved",
        });
        step += 1;
        bytes = structured.to_string().len();
    }
    if bytes > cap {
        return error(terminal_error(&structured));
    }
    let returned = structured
        .get("returned")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(original_items);
    if let Some(obj) = structured.as_object_mut() {
        obj.insert(
            "truncation".into(),
            json!({
                "applied": !steps.is_empty(),
                "steps": steps,
                "original_items": original_items,
                "returned_items": returned,
                "bytes": bytes,
            }),
        );
    }
    success(structured)
}

/// Drop every `body_preview` in an array of summaries.
pub fn drop_previews(tasks: &mut Value) {
    if let Some(arr) = tasks.as_array_mut() {
        for t in arr {
            if let Some(o) = t.as_object_mut() {
                o.insert("body_preview".into(), Value::Null);
                o.insert("body_omitted_for_size".into(), json!(true));
            }
        }
    }
}
