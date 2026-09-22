//! Tool declarations: names, descriptions, `inputSchema`, `outputSchema`,
//! annotations. Every schema is `additionalProperties: false` with a full
//! `required` list, and every nullable is typed `["…","null"]` rather than
//! omitted — omission and null must never be confusable.

use serde_json::{Value, json};

fn read_annotations() -> Value {
    json!({
        "readOnlyHint": true,
        "destructiveHint": false,
        "idempotentHint": true,
        "openWorldHint": true,
    })
}

fn write_annotations(destructive: bool, idempotent: bool) -> Value {
    json!({
        "readOnlyHint": false,
        "destructiveHint": destructive,
        "idempotentHint": idempotent,
        "openWorldHint": true,
    })
}

fn ns() -> Value {
    json!({ "type": ["string", "null"] })
}

fn ni() -> Value {
    json!({ "type": ["integer", "null"] })
}

fn nb() -> Value {
    json!({ "type": ["boolean", "null"] })
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

fn string_array(max: usize) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "maxItems": max })
}

const DATE: &str = "^\\d{4}-\\d{2}-\\d{2}$";
const LOCAL_DT: &str = "^\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}(:\\d{2})?$";

pub fn task_summary_schema() -> Value {
    obj(
        json!({
            "task_id": { "type": "string" },
            "list_id": { "type": "string" },
            "list_name": { "type": "string" },
            "title": { "type": "string" },
            "status": { "type": "string", "enum": ["notStarted", "inProgress", "completed", "waitingOnOthers", "deferred"] },
            "importance": { "type": "string", "enum": ["low", "normal", "high"] },
            "due": { "type": ["string", "null"], "description": "Local civil date YYYY-MM-DD in the effective time zone; never an instant." },
            "due_unresolved": { "type": ["string", "null"], "description": "Set to the raw Graph timeZone string when the due date could not be placed; the task then belongs to no due bucket." },
            "start": ns(),
            "completed_at": ns(),
            "created_at": ns(),
            "modified_at": ns(),
            "categories": { "type": "array", "items": { "type": "string" } },
            "has_checklist": nb(),
            "checklist_open": ni(),
            "checklist_total": ni(),
            "is_recurring": { "type": "boolean" },
            "reminder_at": ns(),
            "body_preview": ns(),
            "body_truncated": { "type": "boolean" },
            "body_bytes_total": { "type": "integer" },
            "etag": ns(),
        }),
        &[
            "task_id",
            "list_id",
            "list_name",
            "title",
            "status",
            "importance",
            "due",
            "due_unresolved",
            "start",
            "completed_at",
            "created_at",
            "modified_at",
            "categories",
            "has_checklist",
            "checklist_open",
            "checklist_total",
            "is_recurring",
            "reminder_at",
            "body_preview",
            "body_truncated",
            "body_bytes_total",
            "etag",
        ],
    )
}

pub fn coverage_schema() -> Value {
    obj(
        json!({
            "lists_total": { "type": "integer" },
            "lists_complete": { "type": "integer" },
            "lists_partial": { "type": "array", "items": { "type": "string" } },
            "lists_missing": { "type": "array", "items": { "type": "string" } },
            "tasks_seen": { "type": "integer" },
            "stopped_because": { "type": "string", "enum": ["complete", "sync_timeout", "request_budget", "page_cap", "cache_cap", "throttled", "graph_error"] },
            "elapsed_ms": { "type": "integer" },
            "graph_requests": { "type": "integer" },
            "banner": ns(),
            "retry_hint": ns(),
        }),
        &[
            "lists_total",
            "lists_complete",
            "lists_partial",
            "lists_missing",
            "tasks_seen",
            "stopped_because",
            "elapsed_ms",
            "graph_requests",
            "banner",
            "retry_hint",
        ],
    )
}

pub fn truncation_schema() -> Value {
    obj(
        json!({
            "applied": { "type": "boolean" },
            "steps": { "type": "array", "items": { "type": "string" } },
            "original_items": { "type": "integer" },
            "returned_items": { "type": "integer" },
            "bytes": { "type": "integer" },
        }),
        &[
            "applied",
            "steps",
            "original_items",
            "returned_items",
            "bytes",
        ],
    )
}

fn failed_item() -> Value {
    obj(
        json!({
            "index": { "type": "integer" },
            "task_id": ns(),
            "title": ns(),
            "error_code": { "type": "string" },
            "message": { "type": "string" },
        }),
        &["index", "task_id", "title", "error_code", "message"],
    )
}

pub fn all(tz: &str) -> Vec<Value> {
    let tz_arg = json!({
        "type": "string",
        "description": format!("IANA zone overriding the server default ({tz}) for this call only, e.g. \"Europe/Prague\".")
    });
    let list_arg = json!({
        "type": "string",
        "description": "List name (case- and accent-insensitive; exact, then unique prefix, then unique substring) or list_id. Call todo_lists for names."
    });

    vec![
        json!({
            "name": "todo_lists",
            "title": "List task lists",
            "description": "All Microsoft To Do lists with their ids and, when the cache is warm, open/overdue counts. Cheap — never fetches tasks. Use it first to learn list names; the built-in lists carry wellknown_list_name \"defaultList\" (Tasks) and \"flaggedEmails\" (Flagged email).",
            "inputSchema": obj(json!({
                "include_counts": { "type": "boolean", "description": "Fill open/overdue counts for lists already in the cache (default true). Never triggers a sync." },
                "timezone": tz_arg,
            }), &[]),
            "outputSchema": obj(json!({
                "account_id": { "type": "string" },
                "timezone": { "type": "string" },
                "counts_from_cache": { "type": "boolean" },
                "lists": { "type": "array", "items": obj(json!({
                    "list_id": { "type": "string" },
                    "name": { "type": "string" },
                    "wellknown_list_name": ns(),
                    "is_owner": nb(),
                    "is_shared": nb(),
                    "open_count": ni(),
                    "overdue_count": ni(),
                    "cached": { "type": "boolean" },
                }), &["list_id", "name", "wellknown_list_name", "is_owner", "is_shared", "open_count", "overdue_count", "cached"]) },
                "truncation": truncation_schema(),
            }), &["account_id", "timezone", "counts_from_cache", "lists", "truncation"]),
            "annotations": read_annotations(),
        }),
        json!({
            "name": "todo_search_tasks",
            "title": "Search tasks",
            "description": "The universal read: tasks from one list or all lists, filtered by status, due window, importance, categories and free text; sorted; paginated with an opaque cursor. All filtering is client-side over a short-lived cache. A first all-lists call may return complete:false with a coverage block — call again with the same arguments to continue. Dates are local civil dates in the effective time zone.",
            "inputSchema": obj(json!({
                "list": list_arg,
                "query": { "type": "string", "maxLength": 200, "description": "Case-insensitive substring over title, body text and category names; whitespace-separated terms are ANDed." },
                "status": { "type": "string", "enum": ["open", "completed", "any"], "description": "Default open (= not completed)." },
                "due": { "type": "string", "enum": ["any", "overdue", "today", "tomorrow", "this_week", "next_7_days", "no_due_date", "has_due_date"] },
                "due_from": { "type": "string", "pattern": DATE, "description": "Inclusive lower bound on the local due date. Mutually exclusive with due." },
                "due_to": { "type": "string", "pattern": DATE, "description": "Inclusive upper bound. Mutually exclusive with due." },
                "importance": { "type": "string", "enum": ["low", "normal", "high"] },
                "categories": { "type": "array", "items": { "type": "string" }, "maxItems": 20, "description": "Match tasks carrying ANY of these categories." },
                "sort": { "type": "string", "enum": ["due_asc", "due_desc", "created_desc", "created_asc", "modified_desc", "title_asc", "importance_desc"], "description": "Default due_asc, tasks without a due date last." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Page size, default 50." },
                "cursor": { "type": "string", "description": "From next_cursor of a previous result. Pass alone (optionally with limit); never with filters." },
                "timezone": tz_arg,
            }), &[]),
            "outputSchema": obj(json!({
                "account_id": { "type": "string" },
                "timezone": { "type": "string" },
                "complete": { "type": "boolean" },
                "coverage": coverage_schema(),
                "truncation": truncation_schema(),
                "total_matched": { "type": "integer" },
                "returned": { "type": "integer" },
                "next_cursor": ns(),
                "tasks": { "type": "array", "items": task_summary_schema() },
            }), &["account_id", "timezone", "complete", "coverage", "truncation", "total_matched", "returned", "next_cursor", "tasks"]),
            "annotations": read_annotations(),
        }),
        json!({
            "name": "todo_agenda",
            "title": "Agenda (plan my day)",
            "description": "One call for \"plan my day\": overdue, due today, due soon, no due date (capped, newest first), flagged emails, and recently completed — each a section with its true count and a truncated flag. Buckets are local civil dates in the effective time zone. A cold first call may be partial (see coverage); call again to complete.",
            "inputSchema": obj(json!({
                "timezone": tz_arg,
                "horizon_days": { "type": "integer", "minimum": 1, "maximum": 30, "description": "due_soon covers tomorrow through today+horizon_days (default 7)." },
                "include_completed_since_days": { "type": "integer", "minimum": 0, "maximum": 30, "description": "recently_completed window in local days (default 1)." },
                "max_per_section": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Default 15." },
                "lists": { "type": "array", "items": { "type": "string" }, "maxItems": 20, "description": "Restrict to these list names; omit for all." },
            }), &[]),
            "outputSchema": obj(json!({
                "account_id": { "type": "string" },
                "timezone": { "type": "string" },
                "today": { "type": "string" },
                "complete": { "type": "boolean" },
                "coverage": coverage_schema(),
                "truncation": truncation_schema(),
                "sections": obj(json!({
                    "overdue": section_schema(),
                    "due_today": section_schema(),
                    "due_soon": section_schema(),
                    "no_due_date": section_schema(),
                    "flagged_emails": section_schema(),
                    "recently_completed": section_schema(),
                }), &["overdue", "due_today", "due_soon", "no_due_date", "flagged_emails", "recently_completed"]),
            }), &["account_id", "timezone", "today", "complete", "coverage", "truncation", "sections"]),
            "annotations": read_annotations(),
        }),
        json!({
            "name": "todo_get_task",
            "title": "Get one task",
            "description": "Full fidelity for one task: body (text or html), checklist items, linked resources, attachment metadata (never bytes), raw recurrence, etag. Pass list to skip an id lookup. web_link is always null: Microsoft To Do exposes no per-task deep link through Graph. etag is informational only — the server never sends If-Match.",
            "inputSchema": obj(json!({
                "task_id": { "type": "string" },
                "list": list_arg,
                "body_format": { "type": "string", "enum": ["text", "html", "none"], "description": "Default text." },
                "timezone": tz_arg,
            }), &["task_id"]),
            "outputSchema": obj(json!({
                "account_id": { "type": "string" },
                "timezone": { "type": "string" },
                "task": task_summary_schema(),
                "body": ns(),
                "body_content_type": ns(),
                "body_truncated": { "type": "boolean" },
                "body_bytes_total": { "type": "integer" },
                "recurrence": {},
                "checklist_items": { "type": "array", "items": obj(json!({
                    "item_id": { "type": "string" }, "name": { "type": "string" }, "checked": { "type": "boolean" },
                    "created_at": ns(), "checked_at": ns(),
                }), &["item_id", "name", "checked", "created_at", "checked_at"]) },
                "checklist_truncated": { "type": "boolean" },
                "checklist_total": { "type": "integer" },
                "linked_resources": { "type": "array", "items": obj(json!({
                    "id": { "type": "string" }, "web_url": ns(), "application_name": ns(), "display_name": ns(), "external_id": ns(),
                }), &["id", "web_url", "application_name", "display_name", "external_id"]) },
                "linked_resources_truncated": { "type": "boolean" },
                "linked_resources_total": { "type": "integer" },
                "attachments": { "type": "array", "items": obj(json!({
                    "id": { "type": "string" }, "name": ns(), "content_type": ns(), "size": ni(), "last_modified_at": ns(),
                }), &["id", "name", "content_type", "size", "last_modified_at"]) },
                "attachments_truncated": { "type": "boolean" },
                "attachments_total": { "type": "integer" },
                "has_attachments": { "type": "boolean" },
                "etag": ns(),
                "web_link": { "type": "null", "description": "Microsoft To Do exposes no per-task deep link through Graph." },
                "truncation": truncation_schema(),
            }), &["account_id", "timezone", "task", "body", "body_content_type", "body_truncated", "body_bytes_total", "recurrence", "checklist_items", "checklist_truncated", "checklist_total", "linked_resources", "linked_resources_truncated", "linked_resources_total", "attachments", "attachments_truncated", "attachments_total", "has_attachments", "etag", "web_link", "truncation"]),
            "annotations": read_annotations(),
        }),
        json!({
            "name": "todo_account_status",
            "title": "Account and server status",
            "description": "The signed-in grant, granted scopes, whether write tools are enabled (frozen at startup, or following the live grant with TODO_MCP_START_WITHOUT_TOKEN) and whether a restart is required, token horizon, time zone, cache and Graph counters. In follow mode the grant is unknown until the first Graph call after a login, so write_tools_enabled is null during that interval. No token material. check_connectivity:true issues one tiny Graph request (plus any retries); false touches no network. Never calls the profile endpoint GET /me (every request is under /me/todo/, directly or inside $batch); identity is not requested.",
            "inputSchema": obj(json!({
                "check_connectivity": { "type": "boolean", "description": "Default false." },
            }), &[]),
            "outputSchema": obj(json!({
                "account_id": { "type": "string" },
                "identity": { "type": "string" },
                "grant": { "type": "string", "enum": ["Tasks.ReadWrite", "Tasks.Read", "none"] },
                "scopes_granted": { "type": "array", "items": { "type": "string" } },
                "write_tools_enabled": { "type": ["boolean", "null"], "description": "Null in follow mode until the first Graph call after a login establishes the live grant." },
                "restart_required": { "type": "boolean" },
                "restart_reason": ns(),
                "token": obj(json!({
                    "present": { "type": "boolean" },
                    "access_token_expires_at": ns(),
                    "refresh_token_obtained_at": ns(),
                    "refreshes": { "type": "integer" },
                    "client_id_tail": ns(),
                }), &["present", "access_token_expires_at", "refresh_token_obtained_at", "refreshes", "client_id_tail"]),
                "login_command": { "type": "string" },
                "timezone": { "type": "string" },
                "timezone_configured": { "type": "boolean" },
                "timezone_mode": { "type": "string", "enum": ["server_side", "client_side", "unknown"] },
                "connectivity": obj(json!({
                    "checked": { "type": "boolean" },
                    "ok": nb(),
                    "detail": ns(),
                }), &["checked", "ok", "detail"]),
                "cache": obj(json!({
                    "mailbox_id": { "type": "string" }, "lists_cached": { "type": "integer" }, "tasks_cached": { "type": "integer" },
                    "max_tasks": { "type": "integer" }, "ttl_seconds": { "type": "integer" }, "generation": { "type": "integer" },
                    "oldest_entry_age_seconds": ni(), "hits": { "type": "integer" }, "misses": { "type": "integer" },
                    "catalogue_cached": { "type": "boolean" },
                }), &["mailbox_id", "lists_cached", "tasks_cached", "max_tasks", "ttl_seconds", "generation", "oldest_entry_age_seconds", "hits", "misses", "catalogue_cached"]),
                "graph": obj(json!({
                    "requests": { "type": "integer" }, "throttled_429": { "type": "integer" }, "retries": { "type": "integer" },
                    "transport_failures": { "type": "integer" }, "last_status": ni(), "last_retry_after_secs": ni(),
                }), &["requests", "throttled_429", "retries", "transport_failures", "last_status", "last_retry_after_secs"]),
                "server": obj(json!({
                    "version": { "type": "string" }, "protocol_version": { "type": "string" }, "uptime_seconds": { "type": "integer" },
                }), &["version", "protocol_version", "uptime_seconds"]),
            }), &["account_id", "identity", "grant", "scopes_granted", "write_tools_enabled", "restart_required", "restart_reason", "token", "login_command", "timezone", "timezone_configured", "timezone_mode", "connectivity", "cache", "graph", "server"]),
            "annotations": read_annotations(),
        }),
        // ------------------------------------------------------------- writes
        json!({
            "name": "todo_create_tasks",
            "title": "Create tasks",
            "description": "Create 1–25 tasks in one call. Purely additive: calling twice makes two tasks. Each item may set body, due_date, start_date, reminder_at, importance, categories, an inline checklist and a recurrence. A top-level list applies to items that do not name their own. Results split into created[], failed[] and warnings[] (a checklist item that failed to attach is a warning, never a failed task). Each task and each checklist item is one request, sent in order, so a large batch can outrun the tool deadline; anything not reached before it is reported (failed[] with error_code deadline, or warnings[] for a checklist item).",
            "inputSchema": obj(json!({
                "list": list_arg,
                "tasks": { "type": "array", "minItems": 1, "maxItems": 25, "items": obj(json!({
                    "title": { "type": "string", "minLength": 1, "maxLength": 1000 },
                    "list": list_arg,
                    "body": { "type": "string", "maxLength": 20000, "description": "Plain text; stored as HTML." },
                    "due_date": { "type": "string", "pattern": DATE },
                    "start_date": { "type": "string", "pattern": DATE },
                    "reminder_at": { "type": "string", "pattern": LOCAL_DT, "description": "Local wall clock in the effective time zone." },
                    "importance": { "type": "string", "enum": ["low", "normal", "high"] },
                    "categories": string_array(20),
                    "checklist": string_array(20),
                    "recurrence": { "type": "object", "description": "A Graph patternedRecurrence object ({pattern:{type,interval,…}, range:{type,startDate,…}}), passed through verbatim. With a recurrence Graph derives due_date and start_date from range.startDate and ignores the values sent (a daily pattern in a non-UTC zone lands one interval later); read the created task back for the dates that stuck." },
                }), &["title"]) },
                "timezone": tz_arg,
            }), &["tasks"]),
            "outputSchema": obj(json!({
                "created": { "type": "array", "items": task_summary_schema() },
                "failed": { "type": "array", "items": failed_item() },
                "warnings": { "type": "array", "items": { "type": "string" } },
            }), &["created", "failed", "warnings"]),
            "annotations": write_annotations(false, false),
        }),
        json!({
            "name": "todo_update_tasks",
            "title": "Update tasks",
            "description": "Update 1–25 tasks. Set fields by value; clear them with explicit clear_* booleans (clear_due_date, clear_start_date, clear_reminder, clear_body, clear_categories, clear_recurrence, clear_importance resets to normal). Same arguments twice yield the same end state. A field and its clear_* flag together are refused before any request. Last writer wins; no optimistic concurrency. Each task_id is matched to its list once before any change (pass list to skip that lookup); then each task is one request, sent in order. In failed[], error_code sync_incomplete means the lookup stopped before reaching the id (nothing changed; pass list, or call again), list_not_found means the list argument did not resolve or no list contains the id, and deadline means the item was not attempted before the tool deadline.",
            "inputSchema": obj(json!({
                "updates": { "type": "array", "minItems": 1, "maxItems": 25, "items": obj(json!({
                    "task_id": { "type": "string" },
                    "list": list_arg,
                    "title": { "type": "string", "minLength": 1, "maxLength": 1000 },
                    "body": { "type": "string", "maxLength": 20000 },
                    "due_date": { "type": "string", "pattern": DATE },
                    "start_date": { "type": "string", "pattern": DATE },
                    "reminder_at": { "type": "string", "pattern": LOCAL_DT },
                    "importance": { "type": "string", "enum": ["low", "normal", "high"] },
                    "status": { "type": "string", "enum": ["notStarted", "inProgress", "completed", "waitingOnOthers", "deferred"] },
                    "categories": string_array(20),
                    "recurrence": { "type": "object" },
                    "clear_due_date": { "type": "boolean", "description": "Graph also removes start_date and recurrence with the due date." },
                    "clear_start_date": { "type": "boolean" },
                    "clear_reminder": { "type": "boolean" },
                    "clear_body": { "type": "boolean" },
                    "clear_categories": { "type": "boolean" },
                    "clear_recurrence": { "type": "boolean" },
                    "clear_importance": { "type": "boolean" },
                }), &["task_id"]) },
                "timezone": tz_arg,
            }), &["updates"]),
            "outputSchema": obj(json!({
                "updated": { "type": "array", "items": obj(json!({
                    "task_id": { "type": "string" }, "title": { "type": "string" },
                    "fields_changed": { "type": "array", "items": { "type": "string" } },
                    "cleared": { "type": "array", "items": { "type": "string" } },
                }), &["task_id", "title", "fields_changed", "cleared"]) },
                "failed": { "type": "array", "items": failed_item() },
            }), &["updated", "failed"]),
            "annotations": write_annotations(true, true),
        }),
        json!({
            "name": "todo_complete_tasks",
            "title": "Complete or reopen tasks",
            "description": "Mark 1–50 tasks completed (completed:true, the default) or reopen them (completed:false → status notStarted; the prior status is not recorded anywhere). Reversible; nothing is destroyed. Tasks already in the requested state are reported in unchanged[]. Each task_id is matched to its list once before any change (pass list to skip that lookup); then each task is one request, sent in order. In failed[], error_code sync_incomplete means the lookup stopped before reaching the id (nothing changed; pass list, or call again), list_not_found means the list argument did not resolve or no list contains the id, and deadline means the item was not attempted before the tool deadline.",
            "inputSchema": obj(json!({
                "task_ids": { "type": "array", "minItems": 1, "maxItems": 50, "items": { "type": "string" } },
                "completed": { "type": "boolean", "description": "Default true." },
                "list": list_arg,
            }), &["task_ids"]),
            "outputSchema": obj(json!({
                "changed": { "type": "array", "items": obj(json!({ "task_id": { "type": "string" }, "title": { "type": "string" }, "status": { "type": "string" } }), &["task_id", "title", "status"]) },
                "unchanged": { "type": "array", "items": obj(json!({ "task_id": { "type": "string" }, "reason": { "type": "string" } }), &["task_id", "reason"]) },
                "failed": { "type": "array", "items": failed_item() },
            }), &["changed", "unchanged", "failed"]),
            "annotations": write_annotations(false, true),
        }),
        json!({
            "name": "todo_delete_tasks",
            "title": "Delete tasks",
            "description": "Permanently delete 1–50 tasks. Requires confirm: true (enforced server-side). Refuses tasks in the built-in Flagged emails list (deleting there would not unflag the mail). An already-absent task is reported in already_absent[], not as a failure. Each task_id is matched to its list once before any change (pass list to skip that lookup); then each task is one request, sent in order. In failed[], error_code sync_incomplete means the lookup stopped before reaching the id (nothing changed; pass list, or call again), list_not_found means the list argument did not resolve or no list contains the id, and deadline means the item was not attempted before the tool deadline.",
            "inputSchema": obj(json!({
                "task_ids": { "type": "array", "minItems": 1, "maxItems": 50, "items": { "type": "string" } },
                "confirm": { "type": "boolean", "const": true, "description": "Must be true. Nothing is deleted otherwise." },
                "list": list_arg,
            }), &["task_ids", "confirm"]),
            "outputSchema": obj(json!({
                "deleted": { "type": "array", "items": obj(json!({ "task_id": { "type": "string" }, "title": ns() }), &["task_id", "title"]) },
                "already_absent": { "type": "array", "items": { "type": "string" } },
                "refused": { "type": "array", "items": obj(json!({ "task_id": { "type": "string" }, "reason": { "type": "string" } }), &["task_id", "reason"]) },
                "failed": { "type": "array", "items": failed_item() },
            }), &["deleted", "already_absent", "refused", "failed"]),
            "annotations": write_annotations(true, true),
        }),
        json!({
            "name": "todo_manage_checklist",
            "title": "Manage a task's checklist",
            "description": "Declarative checklist edits on ONE task in one call: remove (requires confirm:true), rename, uncheck, check, add — applied in that fixed order, at most 20 per verb. Items are addressed by name (case-insensitive) or item_id; a name matching two items refuses that op only. Returns the checklist after the edits. The task is matched to its list first (pass list to skip that lookup); then each change is one request, sent in order, so a large call can outrun the tool deadline, and a change not reached before it is reported in failed[] with error_code deadline.",
            "inputSchema": obj(json!({
                "task_id": { "type": "string" },
                "list": list_arg,
                "add": string_array(20),
                "check": string_array(20),
                "uncheck": string_array(20),
                "rename": { "type": "array", "maxItems": 20, "items": obj(json!({ "from": { "type": "string" }, "to": { "type": "string" } }), &["from", "to"]) },
                "remove": string_array(20),
                "confirm": { "type": "boolean", "description": "Required (true) when remove is non-empty." },
            }), &["task_id"]),
            "outputSchema": obj(json!({
                "task_id": { "type": "string" },
                "added": { "type": "array", "items": { "type": "string" } },
                "checked": { "type": "array", "items": { "type": "string" } },
                "unchecked": { "type": "array", "items": { "type": "string" } },
                "renamed": { "type": "array", "items": { "type": "string" } },
                "removed": { "type": "array", "items": { "type": "string" } },
                "refused": { "type": "array", "items": obj(json!({ "op": { "type": "string" }, "item": { "type": "string" }, "reason": { "type": "string" } }), &["op", "item", "reason"]) },
                "failed": { "type": "array", "items": obj(json!({ "op": { "type": "string" }, "item": { "type": "string" }, "error_code": { "type": "string" }, "message": { "type": "string" } }), &["op", "item", "error_code", "message"]) },
                "checklist_after": { "type": "array", "items": obj(json!({ "item_id": { "type": "string" }, "name": { "type": "string" }, "checked": { "type": "boolean" } }), &["item_id", "name", "checked"]) },
            }), &["task_id", "added", "checked", "unchecked", "renamed", "removed", "refused", "failed", "checklist_after"]),
            "annotations": write_annotations(true, false),
        }),
    ]
}

fn section_schema() -> Value {
    obj(
        json!({
            "count": { "type": "integer", "description": "True pre-truncation count." },
            "truncated": { "type": "boolean" },
            "tasks": { "type": "array", "items": task_summary_schema() },
        }),
        &["count", "truncated", "tasks"],
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn check_closed(schema: &Value, path: &str) {
        if schema.get("type") == Some(&json!("object")) && schema.get("properties").is_some() {
            assert_eq!(schema["additionalProperties"], false, "{path}: open object");
            let props: Vec<&String> = schema["properties"].as_object().unwrap().keys().collect();
            let required: Vec<&str> = schema["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            for r in &required {
                assert!(
                    props.iter().any(|p| p == r),
                    "{path}: required {r} not a property"
                );
            }
            for (k, v) in schema["properties"].as_object().unwrap() {
                check_closed(v, &format!("{path}.{k}"));
            }
        }
        if let Some(items) = schema.get("items") {
            check_closed(items, &format!("{path}[]"));
        }
    }

    #[test]
    fn ten_tools_all_closed_with_explicit_annotations() {
        let tools = all("Europe/Prague");
        assert_eq!(tools.len(), 10);
        for t in &tools {
            let name = t["name"].as_str().unwrap();
            check_closed(&t["inputSchema"], &format!("{name}.input"));
            check_closed(&t["outputSchema"], &format!("{name}.output"));
            // Every output schema is fully required.
            let out = &t["outputSchema"];
            assert_eq!(
                out["properties"].as_object().unwrap().len(),
                out["required"].as_array().unwrap().len(),
                "{name}: output not fully required"
            );
            for hint in [
                "readOnlyHint",
                "destructiveHint",
                "idempotentHint",
                "openWorldHint",
            ] {
                assert!(t["annotations"][hint].is_boolean(), "{name} lacks {hint}");
            }
            assert_eq!(t["annotations"]["openWorldHint"], true, "{name}");
        }
        let del = tools
            .iter()
            .find(|t| t["name"] == "todo_delete_tasks")
            .unwrap();
        assert_eq!(del["inputSchema"]["properties"]["confirm"]["const"], true);
        assert!(
            del["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("confirm"))
        );
    }
}
