//! The five write tools (m6). Best-effort per item — there is no transaction
//! on `/me/todo`, so every result splits into success and failure arrays.
//! Guards run pre-flight with zero HTTP; every successful or unknown-outcome
//! mutation invalidates that list's cache entry (m4 §2).

use std::time::Instant;

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::guards::{
    REFUSE_CHECKLIST_REMOVE_CONFIRM, REFUSE_DELETE_CONFIRM, is_flagged_emails, refuse_batch_cap,
    refuse_flagged_delete,
};
use super::render::{error, failure, incomplete_walk, invalid_args, success, task_summary};
use super::{
    arg_array, arg_bool, arg_date, arg_enum, arg_local_datetime, arg_str, arg_str_max,
    arg_string_array, arg_timezone, reject_unknown_keys,
};
use crate::domain::datetime::{graph_midnight, graph_wall_clock, outbound_tz_name};
use crate::domain::recurrence::check_shape;
use crate::domain::text::escape_html;
use crate::errors::AppError;
use crate::graph::Budget;
use crate::graph::models::{ChecklistItem, Task, TaskList};
use crate::graph::tasks::Walk;
use crate::server::{Coverage, ServerState};

// ---------------------------------------------------------------------------
// Patch<T> — the serde trap (m6 §2)

/// Tri-state whose `Serialize` ERRORS in the `Absent` state, so a forgotten
/// `skip_serializing_if` is loud, not a silent clear.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Patch<T> {
    #[default]
    Absent,
    Null,
    Set(T),
}

impl<T> Patch<T> {
    pub fn is_absent(&self) -> bool {
        matches!(self, Patch::Absent)
    }
}

impl<T: Serialize> Serialize for Patch<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Patch::Null => s.serialize_none(),
            Patch::Set(v) => v.serialize(s),
            Patch::Absent => Err(serde::ser::Error::custom(
                "Patch::Absent must be skipped with skip_serializing_if = \"Patch::is_absent\"",
            )),
        }
    }
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskPatch {
    #[serde(skip_serializing_if = "Patch::is_absent")]
    pub title: Patch<String>,
    #[serde(skip_serializing_if = "Patch::is_absent")]
    pub due_date_time: Patch<Value>,
    #[serde(skip_serializing_if = "Patch::is_absent")]
    pub start_date_time: Patch<Value>,
    #[serde(skip_serializing_if = "Patch::is_absent")]
    pub reminder_date_time: Patch<Value>,
    #[serde(skip_serializing_if = "Patch::is_absent")]
    pub is_reminder_on: Patch<bool>,
    #[serde(skip_serializing_if = "Patch::is_absent")]
    pub importance: Patch<String>,
    #[serde(skip_serializing_if = "Patch::is_absent")]
    pub status: Patch<String>,
    #[serde(skip_serializing_if = "Patch::is_absent")]
    pub completed_date_time: Patch<Value>,
    #[serde(skip_serializing_if = "Patch::is_absent")]
    pub categories: Patch<Vec<String>>,
    #[serde(skip_serializing_if = "Patch::is_absent")]
    pub body: Patch<Value>,
    #[serde(skip_serializing_if = "Patch::is_absent")]
    pub recurrence: Patch<Value>,
}

pub fn body_value(text: &str) -> Value {
    json!({ "content": escape_html(text), "contentType": "html" })
}

pub fn date_value(date: chrono::NaiveDate, tz_name: &str) -> Value {
    json!({ "dateTime": graph_midnight(date), "timeZone": tz_name })
}

pub fn datetime_value(dt: chrono::NaiveDateTime, tz_name: &str) -> Value {
    json!({ "dateTime": graph_wall_clock(dt), "timeZone": tz_name })
}

// ---------------------------------------------------------------------------
// Shared plumbing

fn failed(
    index: usize,
    task_id: Option<&str>,
    title: Option<&str>,
    code: &str,
    message: String,
) -> Value {
    json!({ "index": index, "task_id": task_id, "title": title, "error_code": code, "message": message })
}

fn app_error_parts(e: &AppError) -> (String, String) {
    (e.code().to_ascii_lowercase(), e.message())
}

/// Any mutation that succeeded or whose outcome is unknown drops the list.
fn invalidate_after(state: &ServerState, list_id: &str, result: &Result<(), AppError>) {
    let invalidate = match result {
        Ok(()) => true,
        Err(AppError::Graph { status, .. }) => *status >= 500,
        Err(AppError::Transport(_) | AppError::Throttled { .. }) => true,
        Err(_) => false,
    };
    if invalidate {
        state.cache_write().invalidate_list(list_id);
    }
}

/// The per-item message for an item the write phase never reached.
const DEADLINE_MSG: &str = "the tool deadline elapsed before this item was attempted";

/// The text of an `isError` result.
fn error_text(v: &Value) -> String {
    v["content"][0]["text"].as_str().unwrap_or("").to_string()
}

fn deadline_op(op: &str, item: &str) -> Value {
    json!({ "op": op, "item": item, "error_code": "deadline", "message": DEADLINE_MSG })
}

/// Where one task lives, settled before the batch's first mutation.
struct Target {
    list: TaskList,
    /// The task as fetched in this call or fresh in the cache — never an
    /// expired entry, so `unchanged[]` is never decided from stale state.
    seen: Option<Task>,
    /// For reporting only (delete titles), so an expired entry is good enough.
    /// Captured before any mutation invalidates the list.
    title: Option<String>,
}

/// `Err((error_code, message))` for an id that could not be placed in a list.
type Resolved = Result<Target, (String, String)>;

/// Why a lookup that stopped early did not place `id`, and what to do next.
fn sync_incomplete_message(id: &str, cov: &Coverage, cache_disabled: bool) -> String {
    let next = if cache_disabled {
        "Pass \"list\" to skip the lookup; the cache is disabled (TODO_MCP_CACHE_TTL_SECONDS=0), so calling again repeats the same lookup."
    } else {
        "Pass \"list\" to skip the lookup, or call again with the same arguments (lists already synced are cached)."
    };
    format!(
        "task {id} was not changed: the lookup of its list stopped early (stopped: {}; {} of {} lists synced). {next}",
        cov.stopped_because, cov.lists_complete, cov.lists_total
    )
}

/// Resolve every id ONCE, before the first mutation: the `list` argument, else
/// any cached entry (an id never changes list), else ONE sync for all the ids
/// still unplaced. A batch never re-syncs because its own mutations
/// invalidated a list. "Not found" is claimed only after a complete sync; a
/// lookup cut short is `sync_incomplete`, and a hard error keeps its code.
fn resolve_targets(
    state: &ServerState,
    wanted: &[(&str, Option<&str>)],
    lists: &[TaskList],
    budget: &mut Budget,
) -> Vec<Resolved> {
    let now = Instant::now();
    let mut out: Vec<Option<Resolved>> = Vec::with_capacity(wanted.len());
    {
        let c = state.cache_read();
        let target = |l: &TaskList, id: &str| {
            let seen = c.list(&l.id, now).and_then(|e| e.get(id)).cloned();
            let title = seen.as_ref().map(|t| t.title.clone()).or_else(|| {
                c.list_any(&l.id)
                    .and_then(|e| e.get(id))
                    .map(|t| t.title.clone())
            });
            Target {
                list: l.clone(),
                seen,
                title,
            }
        };
        for (id, list_arg) in wanted {
            out.push(match list_arg {
                Some(name) => Some(match state.resolve_list(name, lists) {
                    Ok(l) => Ok(target(l, id)),
                    Err(v) => Err(("list_not_found".to_string(), error_text(&v))),
                }),
                None => c
                    .find_task(id)
                    .and_then(|(lid, _)| lists.iter().find(|l| l.id == lid))
                    .map(|l| Ok(target(l, id))),
            });
        }
    } // The read guard is dropped here: sync_lists takes the cache write lock.
    if out.iter().any(Option::is_none) {
        let sync = state.sync_lists(lists, budget);
        let cache_disabled = state.cfg.cache_ttl_seconds == 0;
        for (slot, (id, _)) in out.iter_mut().zip(wanted) {
            if slot.is_some() {
                continue;
            }
            *slot = Some(match &sync {
                Ok(o) => match o
                    .snapshots
                    .iter()
                    .find_map(|s| s.tasks.iter().find(|t| t.id == *id).map(|t| (s, t)))
                {
                    Some((s, t)) => Ok(Target {
                        list: s.list.clone(),
                        seen: Some(t.clone()),
                        title: Some(t.title.clone()),
                    }),
                    None if o.coverage.is_complete() => Err((
                        "list_not_found".to_string(),
                        format!(
                            "task {id} was not found in any list; pass \"list\" if you know it"
                        ),
                    )),
                    None => Err((
                        "sync_incomplete".to_string(),
                        sync_incomplete_message(id, &o.coverage, cache_disabled),
                    )),
                },
                Err(e) => Err((
                    e.code().to_ascii_lowercase(),
                    error_text(&failure(e, state)),
                )),
            });
        }
    }
    out.into_iter()
        .map(|r| r.unwrap_or_else(|| Err(("internal".into(), "the id was not resolved".into()))))
        .collect()
}

fn outbound_tz(state: &ServerState, tz: chrono_tz::Tz) -> Result<String, Value> {
    outbound_tz_name(tz, state.clock.now()).map_err(error)
}

// ---------------------------------------------------------------------------

pub fn todo_create_tasks(state: &ServerState, args: &Value) -> Value {
    const TOOL: &str = "todo_create_tasks";
    if let Err(e) = reject_unknown_keys(args, &["list", "tasks", "timezone"]) {
        return invalid_args(TOOL, &e);
    }
    let items = match arg_array(args, "tasks", usize::MAX) {
        Ok(Some(v)) if !v.is_empty() => v,
        Ok(_) => return invalid_args(TOOL, "tasks must be a non-empty array"),
        Err(e) => return invalid_args(TOOL, &e),
    };
    if items.len() > 25 {
        return error(refuse_batch_cap(TOOL, 25, items.len()));
    }
    let default_list = match arg_str(args, "list") {
        Ok(v) => v,
        Err(e) => return invalid_args(TOOL, &e),
    };
    let tz = match arg_timezone(args, state) {
        Ok(tz) => tz,
        Err(e) => return invalid_args(TOOL, &e),
    };

    // Pre-flight every item before any request.
    struct Prepared {
        list_name: Option<String>,
        body: Value,
        checklist: Vec<String>,
        title: String,
    }
    let mut prepared: Vec<Prepared> = Vec::new();
    let mut needs_tz = false;
    for (i, item) in items.iter().enumerate() {
        let p = (|| -> Result<Prepared, String> {
            reject_unknown_keys(
                item,
                &[
                    "title",
                    "list",
                    "body",
                    "due_date",
                    "start_date",
                    "reminder_at",
                    "importance",
                    "categories",
                    "checklist",
                    "recurrence",
                ],
            )?;
            let title = arg_str_max(item, "title", 1000)?
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or("title is required")?;
            let mut body = Map::new();
            body.insert("title".into(), json!(title));
            if let Some(text) = arg_str_max(item, "body", 20_000)? {
                body.insert("body".into(), body_value(text));
            }
            if let Some(d) = arg_date(item, "due_date")? {
                needs_tz = true;
                body.insert("dueDateTime".into(), json!({ "date": d.to_string() }));
            }
            if let Some(d) = arg_date(item, "start_date")? {
                needs_tz = true;
                body.insert("startDateTime".into(), json!({ "date": d.to_string() }));
            }
            if let Some(dt) = arg_local_datetime(item, "reminder_at")? {
                needs_tz = true;
                body.insert(
                    "reminderDateTime".into(),
                    json!({ "datetime": dt.format("%Y-%m-%dT%H:%M:%S").to_string() }),
                );
                body.insert("isReminderOn".into(), json!(true));
            }
            if let Some(imp) = arg_enum(item, "importance", &["low", "normal", "high"])? {
                body.insert("importance".into(), json!(imp));
            }
            if let Some(cats) = arg_string_array(item, "categories", 20)? {
                body.insert("categories".into(), json!(cats));
            }
            if let Some(r) = item.get("recurrence").filter(|v| !v.is_null()) {
                check_shape(r)?;
                body.insert("recurrence".into(), r.clone());
            }
            Ok(Prepared {
                list_name: arg_str(item, "list")?.or(default_list).map(str::to_string),
                body: Value::Object(body),
                checklist: arg_string_array(item, "checklist", 20)?.unwrap_or_default(),
                title: title.to_string(),
            })
        })();
        match p {
            Ok(p) => prepared.push(p),
            Err(e) => return invalid_args(TOOL, &format!("tasks[{i}]: {e}")),
        }
    }
    let tz_name = if needs_tz {
        match outbound_tz(state, tz) {
            Ok(n) => Some(n),
            Err(v) => return v,
        }
    } else {
        None
    };
    // Materialise the date placeholders now that the zone is known.
    for p in &mut prepared {
        if let Some(obj) = p.body.as_object_mut()
            && let Some(tzn) = &tz_name
        {
            for key in ["dueDateTime", "startDateTime"] {
                if let Some(d) = obj
                    .get(key)
                    .and_then(|v| v.get("date"))
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<chrono::NaiveDate>().ok())
                {
                    obj.insert(key.into(), date_value(d, tzn));
                }
            }
            if let Some(dt) = obj
                .get("reminderDateTime")
                .and_then(|v| v.get("datetime"))
                .and_then(Value::as_str)
                .and_then(|s| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok())
            {
                obj.insert("reminderDateTime".into(), datetime_value(dt, tzn));
            }
        }
    }

    let mut budget = state.budget();
    let lists = match state.catalogue(&mut budget) {
        Ok(l) => l,
        Err(e) => return failure(&e, state),
    };
    // Resolve every list name before the first POST.
    let mut targets: Vec<TaskList> = Vec::new();
    for (i, p) in prepared.iter().enumerate() {
        let list = match &p.list_name {
            Some(name) => match state.resolve_list(name, &lists) {
                Ok(l) => l.clone(),
                Err(v) => return v,
            },
            None => match lists
                .iter()
                .find(|l| l.wellknown_list_name.as_deref() == Some("defaultList"))
            {
                Some(l) => l.clone(),
                None => {
                    return invalid_args(
                        TOOL,
                        &format!("tasks[{i}]: no list given and the account has no default list"),
                    );
                }
            },
        };
        targets.push(list);
    }

    // Every task and every checklist item is one mutation, sent in order.
    let work: usize = prepared.iter().map(|p| 1 + p.checklist.len()).sum();
    let mut writes = state.write_budget(&budget, work);
    let mut created = Vec::new();
    let mut failed_items = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for (i, (p, list)) in prepared.iter().zip(&targets).enumerate() {
        if writes.expired() {
            failed_items.push(failed(
                i,
                None,
                Some(&p.title),
                "deadline",
                DEADLINE_MSG.into(),
            ));
            continue;
        }
        match state
            .graph
            .create_task(&list.id, p.body.clone(), &mut writes)
        {
            Ok(task) => {
                for name in &p.checklist {
                    if writes.expired() {
                        warnings.push(format!(
                            "task \"{}\" was created but checklist item \"{name}\" was not attempted: the tool deadline elapsed",
                            p.title
                        ));
                        continue;
                    }
                    let body = json!({ "displayName": name });
                    if let Err(e) =
                        state
                            .graph
                            .create_checklist_item(&list.id, &task.id, body, &mut writes)
                    {
                        warnings.push(format!(
                            "task \"{}\" was created but checklist item \"{name}\" failed: {}",
                            p.title,
                            e.message()
                        ));
                    }
                }
                let mut full = task.clone();
                if !p.checklist.is_empty() {
                    full.checklist_items = None;
                }
                created.push(task_summary(&full, &list.id, &list.display_name, tz, state));
                invalidate_after(state, &list.id, &Ok(()));
            }
            Err(e) => {
                let (code, msg) = app_error_parts(&e);
                failed_items.push(failed(i, None, Some(&p.title), &code, msg));
                invalidate_after(state, &list.id, &Err(e));
            }
        }
    }
    success(json!({ "created": created, "failed": failed_items, "warnings": warnings }))
}

// ---------------------------------------------------------------------------

const UPDATE_KEYS: [&str; 18] = [
    "task_id",
    "list",
    "title",
    "body",
    "due_date",
    "start_date",
    "reminder_at",
    "importance",
    "status",
    "categories",
    "recurrence",
    "clear_due_date",
    "clear_start_date",
    "clear_reminder",
    "clear_body",
    "clear_categories",
    "clear_recurrence",
    "clear_importance",
];

pub fn todo_update_tasks(state: &ServerState, args: &Value) -> Value {
    const TOOL: &str = "todo_update_tasks";
    if let Err(e) = reject_unknown_keys(args, &["updates", "timezone"]) {
        return invalid_args(TOOL, &e);
    }
    let items = match arg_array(args, "updates", usize::MAX) {
        Ok(Some(v)) if !v.is_empty() => v,
        Ok(_) => return invalid_args(TOOL, "updates must be a non-empty array"),
        Err(e) => return invalid_args(TOOL, &e),
    };
    if items.len() > 25 {
        return error(refuse_batch_cap(TOOL, 25, items.len()));
    }
    let tz = match arg_timezone(args, state) {
        Ok(tz) => tz,
        Err(e) => return invalid_args(TOOL, &e),
    };

    struct Prepared {
        task_id: String,
        list: Option<String>,
        patch: TaskPatch,
        changed: Vec<String>,
        cleared: Vec<String>,
        dates: Vec<(&'static str, Value)>,
    }
    let mut prepared = Vec::new();
    let mut needs_tz = false;
    for (i, item) in items.iter().enumerate() {
        let p = (|| -> Result<Prepared, String> {
            reject_unknown_keys(item, &UPDATE_KEYS)?;
            let task_id = arg_str(item, "task_id")?
                .filter(|s| !s.is_empty())
                .ok_or("task_id is required")?
                .to_string();
            let flag = |k: &str| arg_bool(item, k).map(|v| v.unwrap_or(false));
            let conflicts = [
                ("due_date", "clear_due_date"),
                ("start_date", "clear_start_date"),
                ("reminder_at", "clear_reminder"),
                ("body", "clear_body"),
                ("categories", "clear_categories"),
                ("recurrence", "clear_recurrence"),
                ("importance", "clear_importance"),
            ];
            for (field, clear) in conflicts {
                if item.get(field).is_some_and(|v| !v.is_null()) && flag(clear)? {
                    return Err(format!(
                        "Update {} (task `{task_id}`): {field} and {clear} are mutually exclusive. Pass one or neither.",
                        i + 1
                    ));
                }
            }
            let mut patch = TaskPatch::default();
            let mut changed = Vec::new();
            let mut cleared = Vec::new();
            let mut dates = Vec::new();
            if let Some(t) = arg_str_max(item, "title", 1000)?
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                patch.title = Patch::Set(t.to_string());
                changed.push("title".to_string());
            }
            if let Some(b) = arg_str_max(item, "body", 20_000)? {
                patch.body = Patch::Set(body_value(b));
                changed.push("body".into());
            } else if flag("clear_body")? {
                patch.body = Patch::Set(json!({ "content": "", "contentType": "html" }));
                cleared.push("body".into());
            }
            if let Some(d) = arg_date(item, "due_date")? {
                needs_tz = true;
                dates.push(("dueDateTime", json!(d.to_string())));
                changed.push("due_date".into());
            } else if flag("clear_due_date")? {
                patch.due_date_time = Patch::Null;
                cleared.push("due_date".into());
            }
            if let Some(d) = arg_date(item, "start_date")? {
                needs_tz = true;
                dates.push(("startDateTime", json!(d.to_string())));
                changed.push("start_date".into());
            } else if flag("clear_start_date")? {
                patch.start_date_time = Patch::Null;
                cleared.push("start_date".into());
            }
            if let Some(dt) = arg_local_datetime(item, "reminder_at")? {
                needs_tz = true;
                dates.push((
                    "reminderDateTime",
                    json!(dt.format("%Y-%m-%dT%H:%M:%S").to_string()),
                ));
                patch.is_reminder_on = Patch::Set(true);
                changed.push("reminder_at".into());
            } else if flag("clear_reminder")? {
                // The pair is one operation: null alone leaves a bell with nothing behind it.
                patch.reminder_date_time = Patch::Null;
                patch.is_reminder_on = Patch::Set(false);
                cleared.push("reminder".into());
            }
            if let Some(imp) = arg_enum(item, "importance", &["low", "normal", "high"])? {
                patch.importance = Patch::Set(imp.to_string());
                changed.push("importance".into());
            } else if flag("clear_importance")? {
                patch.importance = Patch::Set("normal".into());
                cleared.push("importance".into());
            }
            if let Some(s) = arg_enum(
                item,
                "status",
                &[
                    "notStarted",
                    "inProgress",
                    "completed",
                    "waitingOnOthers",
                    "deferred",
                ],
            )? {
                patch.status = Patch::Set(s.to_string());
                if s != "completed" {
                    patch.completed_date_time = Patch::Null;
                }
                changed.push("status".into());
            }
            if let Some(c) = arg_string_array(item, "categories", 20)? {
                patch.categories = Patch::Set(c);
                changed.push("categories".into());
            } else if flag("clear_categories")? {
                patch.categories = Patch::Set(vec![]);
                cleared.push("categories".into());
            }
            if let Some(r) = item.get("recurrence").filter(|v| !v.is_null()) {
                check_shape(r)?;
                patch.recurrence = Patch::Set(r.clone());
                changed.push("recurrence".into());
            } else if flag("clear_recurrence")? {
                patch.recurrence = Patch::Null;
                cleared.push("recurrence".into());
            }
            if changed.is_empty() && cleared.is_empty() {
                return Err(format!(
                    "Update {} (task `{task_id}`): nothing to change",
                    i + 1
                ));
            }
            Ok(Prepared {
                task_id,
                list: arg_str(item, "list")?.map(str::to_string),
                patch,
                changed,
                cleared,
                dates,
            })
        })();
        match p {
            Ok(p) => prepared.push(p),
            Err(e) => return invalid_args(TOOL, &e),
        }
    }
    let tz_name = if needs_tz {
        match outbound_tz(state, tz) {
            Ok(n) => Some(n),
            Err(v) => return v,
        }
    } else {
        None
    };
    for p in &mut prepared {
        for (key, raw) in std::mem::take(&mut p.dates) {
            let Some(tzn) = &tz_name else { continue };
            let s = raw.as_str().unwrap_or("");
            let v = match key {
                "reminderDateTime" => chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                    .map(|dt| datetime_value(dt, tzn))
                    .ok(),
                _ => s
                    .parse::<chrono::NaiveDate>()
                    .map(|d| date_value(d, tzn))
                    .ok(),
            };
            match (key, v) {
                ("dueDateTime", Some(v)) => p.patch.due_date_time = Patch::Set(v),
                ("startDateTime", Some(v)) => p.patch.start_date_time = Patch::Set(v),
                ("reminderDateTime", Some(v)) => p.patch.reminder_date_time = Patch::Set(v),
                _ => {}
            }
        }
    }

    let mut budget = state.budget();
    let lists = match state.catalogue(&mut budget) {
        Ok(l) => l,
        Err(e) => return failure(&e, state),
    };
    let wanted: Vec<(&str, Option<&str>)> = prepared
        .iter()
        .map(|p| (p.task_id.as_str(), p.list.as_deref()))
        .collect();
    let targets = resolve_targets(state, &wanted, &lists, &mut budget);
    let mut writes = state.write_budget(&budget, prepared.len());
    let mut updated = Vec::new();
    let mut failed_items = Vec::new();
    for (i, (p, target)) in prepared.iter().zip(targets).enumerate() {
        let list = match target {
            Ok(t) => t.list,
            Err((code, msg)) => {
                failed_items.push(failed(i, Some(&p.task_id), None, &code, msg));
                continue;
            }
        };
        let body = match serde_json::to_value(&p.patch) {
            Ok(b) => b,
            Err(e) => {
                failed_items.push(failed(i, Some(&p.task_id), None, "internal", e.to_string()));
                continue;
            }
        };
        if writes.expired() {
            failed_items.push(failed(
                i,
                Some(&p.task_id),
                None,
                "deadline",
                DEADLINE_MSG.into(),
            ));
            continue;
        }
        match state
            .graph
            .patch_task(&list.id, &p.task_id, body, &mut writes)
        {
            Ok(task) => {
                updated.push(json!({ "task_id": task.id, "title": task.title, "fields_changed": p.changed, "cleared": p.cleared }));
                invalidate_after(state, &list.id, &Ok(()));
            }
            Err(e) => {
                let (code, msg) = app_error_parts(&e);
                let code = if matches!(e, AppError::Graph { status: 404, .. }) {
                    "not_found".to_string()
                } else {
                    code
                };
                failed_items.push(failed(i, Some(&p.task_id), None, &code, msg));
                invalidate_after(state, &list.id, &Err(e));
            }
        }
    }
    success(json!({ "updated": updated, "failed": failed_items }))
}

// ---------------------------------------------------------------------------

pub fn todo_complete_tasks(state: &ServerState, args: &Value) -> Value {
    const TOOL: &str = "todo_complete_tasks";
    if let Err(e) = reject_unknown_keys(args, &["task_ids", "completed", "list"]) {
        return invalid_args(TOOL, &e);
    }
    let ids = match arg_string_array(args, "task_ids", usize::MAX) {
        Ok(Some(v)) if !v.is_empty() => v,
        Ok(_) => return invalid_args(TOOL, "task_ids must be a non-empty array"),
        Err(e) => return invalid_args(TOOL, &e),
    };
    if ids.len() > 50 {
        return error(refuse_batch_cap(TOOL, 50, ids.len()));
    }
    let completed = match arg_bool(args, "completed") {
        Ok(v) => v.unwrap_or(true),
        Err(e) => return invalid_args(TOOL, &e),
    };
    let list_arg = match arg_str(args, "list") {
        Ok(v) => v,
        Err(e) => return invalid_args(TOOL, &e),
    };
    let mut budget = state.budget();
    let lists = match state.catalogue(&mut budget) {
        Ok(l) => l,
        Err(e) => return failure(&e, state),
    };
    let body = if completed {
        json!({ "status": "completed" })
    } else {
        json!({ "status": "notStarted", "completedDateTime": Value::Null })
    };
    let wanted: Vec<(&str, Option<&str>)> = ids.iter().map(|id| (id.as_str(), list_arg)).collect();
    let targets = resolve_targets(state, &wanted, &lists, &mut budget);
    let mut writes = state.write_budget(&budget, ids.len());
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();
    let mut failed_items = Vec::new();
    for (i, (id, target)) in ids.iter().zip(targets).enumerate() {
        let Target { list, seen, .. } = match target {
            Ok(t) => t,
            Err((code, msg)) => {
                failed_items.push(failed(i, Some(id), None, &code, msg));
                continue;
            }
        };
        // Decided only from a fresh or this-call view; needs no request, so it
        // is reported even after the deadline.
        if seen.as_ref().map(Task::is_completed) == Some(completed) {
            unchanged.push(json!({ "task_id": id, "reason": if completed { "already completed" } else { "already open" } }));
            continue;
        }
        if writes.expired() {
            failed_items.push(failed(i, Some(id), None, "deadline", DEADLINE_MSG.into()));
            continue;
        }
        match state
            .graph
            .patch_task(&list.id, id, body.clone(), &mut writes)
        {
            Ok(task) => {
                changed.push(
                    json!({ "task_id": task.id, "title": task.title, "status": task.status }),
                );
                invalidate_after(state, &list.id, &Ok(()));
            }
            Err(e) => {
                let (code, msg) = app_error_parts(&e);
                failed_items.push(failed(i, Some(id), None, &code, msg));
                invalidate_after(state, &list.id, &Err(e));
            }
        }
    }
    success(json!({ "changed": changed, "unchanged": unchanged, "failed": failed_items }))
}

// ---------------------------------------------------------------------------

pub fn todo_delete_tasks(state: &ServerState, args: &Value) -> Value {
    const TOOL: &str = "todo_delete_tasks";
    if let Err(e) = reject_unknown_keys(args, &["task_ids", "confirm", "list"]) {
        return invalid_args(TOOL, &e);
    }
    // G2 first: zero HTTP.
    match arg_bool(args, "confirm") {
        Ok(Some(true)) => {}
        Ok(_) => return error(REFUSE_DELETE_CONFIRM.into()),
        Err(e) => return invalid_args(TOOL, &e),
    }
    let ids = match arg_string_array(args, "task_ids", usize::MAX) {
        Ok(Some(v)) if !v.is_empty() => v,
        Ok(_) => return invalid_args(TOOL, "task_ids must be a non-empty array"),
        Err(e) => return invalid_args(TOOL, &e),
    };
    if ids.len() > 50 {
        return error(refuse_batch_cap(TOOL, 50, ids.len()));
    }
    let list_arg = match arg_str(args, "list") {
        Ok(v) => v,
        Err(e) => return invalid_args(TOOL, &e),
    };
    let mut budget = state.budget();
    let lists = match state.catalogue(&mut budget) {
        Ok(l) => l,
        Err(e) => return failure(&e, state),
    };
    let wanted: Vec<(&str, Option<&str>)> = ids.iter().map(|id| (id.as_str(), list_arg)).collect();
    let targets = resolve_targets(state, &wanted, &lists, &mut budget);
    let mut writes = state.write_budget(&budget, ids.len());
    let mut deleted = Vec::new();
    let mut absent = Vec::new();
    let mut refused = Vec::new();
    let mut failed_items = Vec::new();
    for (i, (id, target)) in ids.iter().zip(targets).enumerate() {
        let Target { list, title, .. } = match target {
            Ok(t) => t,
            Err((code, msg)) => {
                failed_items.push(failed(i, Some(id), None, &code, msg));
                continue;
            }
        };
        // G1: the catalogue the delete needed anyway; never a per-task fetch.
        if is_flagged_emails(list.wellknown_list_name.as_deref()) {
            refused.push(json!({ "task_id": id, "reason": refuse_flagged_delete(title.as_deref().unwrap_or(id)) }));
            continue;
        }
        if writes.expired() {
            failed_items.push(failed(
                i,
                Some(id),
                title.as_deref(),
                "deadline",
                DEADLINE_MSG.into(),
            ));
            continue;
        }
        match state.graph.delete_task(&list.id, id, &mut writes) {
            Ok(true) => {
                deleted.push(json!({ "task_id": id, "title": title }));
                invalidate_after(state, &list.id, &Ok(()));
            }
            Ok(false) => {
                absent.push(json!(id));
                invalidate_after(state, &list.id, &Ok(()));
            }
            Err(e) => {
                let (code, msg) = app_error_parts(&e);
                failed_items.push(failed(i, Some(id), title.as_deref(), &code, msg));
                invalidate_after(state, &list.id, &Err(e));
            }
        }
    }
    success(
        json!({ "deleted": deleted, "already_absent": absent, "refused": refused, "failed": failed_items }),
    )
}

// ---------------------------------------------------------------------------

pub fn todo_manage_checklist(state: &ServerState, args: &Value) -> Value {
    const TOOL: &str = "todo_manage_checklist";
    if let Err(e) = reject_unknown_keys(
        args,
        &[
            "task_id", "list", "add", "check", "uncheck", "rename", "remove", "confirm",
        ],
    ) {
        return invalid_args(TOOL, &e);
    }
    let parsed = (|| -> Result<_, String> {
        let task_id = arg_str(args, "task_id")?
            .filter(|s| !s.is_empty())
            .ok_or("task_id is required")?
            .to_string();
        let add = arg_string_array(args, "add", 20)?.unwrap_or_default();
        let check = arg_string_array(args, "check", 20)?.unwrap_or_default();
        let uncheck = arg_string_array(args, "uncheck", 20)?.unwrap_or_default();
        let remove = arg_string_array(args, "remove", 20)?.unwrap_or_default();
        let rename: Vec<(String, String)> = arg_array(args, "rename", 20)?
            .map(|items| {
                items
                    .iter()
                    .map(|v| {
                        let from = v
                            .get("from")
                            .and_then(Value::as_str)
                            .ok_or("rename items need \"from\"")?;
                        let to = v
                            .get("to")
                            .and_then(Value::as_str)
                            .filter(|t| !t.trim().is_empty())
                            .ok_or("rename items need a non-empty \"to\"")?;
                        Ok((from.to_string(), to.to_string()))
                    })
                    .collect::<Result<Vec<_>, String>>()
            })
            .transpose()?
            .unwrap_or_default();
        let confirm = arg_bool(args, "confirm")?.unwrap_or(false);
        let list = arg_str(args, "list")?.map(str::to_string);
        Ok((task_id, list, add, check, uncheck, rename, remove, confirm))
    })();
    let (task_id, list_arg, add, check, uncheck, rename, remove, confirm) = match parsed {
        Ok(v) => v,
        Err(e) => return invalid_args(TOOL, &e),
    };
    // G3: zero HTTP.
    if !remove.is_empty() && !confirm {
        return error(REFUSE_CHECKLIST_REMOVE_CONFIRM.into());
    }
    if add.is_empty()
        && check.is_empty()
        && uncheck.is_empty()
        && rename.is_empty()
        && remove.is_empty()
    {
        return invalid_args(
            TOOL,
            "nothing to do: pass at least one of add, check, uncheck, rename, remove",
        );
    }
    let mut budget = state.budget();
    let lists = match state.catalogue(&mut budget) {
        Ok(l) => l,
        Err(e) => return failure(&e, state),
    };
    let wanted = [(task_id.as_str(), list_arg.as_deref())];
    let list = match resolve_targets(state, &wanted, &lists, &mut budget).pop() {
        Some(Ok(t)) => t.list,
        Some(Err((_, msg))) => return error(msg),
        None => return error(format!("task {task_id} could not be resolved to a list")),
    };
    // The checklist read stays on the READ budget; an incomplete read is an
    // error (graph::tasks::Walk::Incomplete), never a short checklist.
    let mut items: Vec<ChecklistItem> =
        match state.graph.list_checklist(&list.id, &task_id, &mut budget) {
            Ok(Walk::Complete(c)) => c,
            Ok(Walk::Incomplete) => {
                return incomplete_walk(state, "checklist", list_arg.is_some());
            }
            Err(AppError::Graph { status: 404, .. }) => {
                return error(format!(
                    "task {task_id} does not exist in list \"{}\"",
                    list.display_name
                ));
            }
            Err(e) => return failure(&e, state),
        };

    let mut writes = state.write_budget(
        &budget,
        remove.len() + rename.len() + uncheck.len() + check.len() + add.len(),
    );
    let mut added = Vec::new();
    let mut checked = Vec::new();
    let mut unchecked = Vec::new();
    let mut renamed = Vec::new();
    let mut removed = Vec::new();
    let mut refused = Vec::new();
    let mut failed_ops = Vec::new();
    let mut touched = false;

    // Name or id → exactly one item, or a refusal for that op only.
    fn find(items: &[ChecklistItem], key: &str) -> Result<usize, &'static str> {
        if let Some(i) = items.iter().position(|c| c.id == key) {
            return Ok(i);
        }
        let matches: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, c)| c.display_name.trim().eq_ignore_ascii_case(key.trim()))
            .map(|(i, _)| i)
            .collect();
        match matches.len() {
            1 => Ok(matches[0]),
            0 => Err("not_found"),
            _ => Err("ambiguous_name"),
        }
    }

    for name in &remove {
        match find(&items, name) {
            Ok(i) => {
                if writes.expired() {
                    failed_ops.push(deadline_op("remove", name));
                    continue;
                }
                let item = items[i].clone();
                match state
                    .graph
                    .delete_checklist_item(&list.id, &task_id, &item.id, &mut writes)
                {
                    Ok(_) => {
                        removed.push(item.display_name.clone());
                        items.remove(i);
                        touched = true;
                    }
                    Err(e) => {
                        let (code, msg) = app_error_parts(&e);
                        failed_ops.push(json!({ "op": "remove", "item": name, "error_code": code, "message": msg }));
                        touched = true;
                    }
                }
            }
            Err(reason) => refused.push(json!({ "op": "remove", "item": name, "reason": reason })),
        }
    }
    for (from, to) in &rename {
        match find(&items, from) {
            Ok(i) => {
                if writes.expired() {
                    failed_ops.push(deadline_op("rename", from));
                    continue;
                }
                match state.graph.patch_checklist_item(
                    &list.id,
                    &task_id,
                    &items[i].id,
                    json!({ "displayName": to }),
                    &mut writes,
                ) {
                    Ok(updated) => {
                        renamed.push(format!(
                            "{} → {}",
                            items[i].display_name, updated.display_name
                        ));
                        items[i] = updated;
                        touched = true;
                    }
                    Err(e) => {
                        let (code, msg) = app_error_parts(&e);
                        failed_ops.push(json!({ "op": "rename", "item": from, "error_code": code, "message": msg }));
                        touched = true;
                    }
                }
            }
            Err(reason) => refused.push(json!({ "op": "rename", "item": from, "reason": reason })),
        }
    }
    for (op, names, target, out) in [
        ("uncheck", &uncheck, false, &mut unchecked),
        ("check", &check, true, &mut checked),
    ] {
        for name in names {
            match find(&items, name) {
                Ok(i) => {
                    if items[i].is_checked == target {
                        out.push(items[i].display_name.clone());
                        continue;
                    }
                    if writes.expired() {
                        failed_ops.push(deadline_op(op, name));
                        continue;
                    }
                    match state.graph.patch_checklist_item(
                        &list.id,
                        &task_id,
                        &items[i].id,
                        json!({ "isChecked": target }),
                        &mut writes,
                    ) {
                        Ok(updated) => {
                            out.push(updated.display_name.clone());
                            items[i] = updated;
                            touched = true;
                        }
                        Err(e) => {
                            let (code, msg) = app_error_parts(&e);
                            failed_ops.push(json!({ "op": op, "item": name, "error_code": code, "message": msg }));
                            touched = true;
                        }
                    }
                }
                Err(reason) => refused.push(json!({ "op": op, "item": name, "reason": reason })),
            }
        }
    }
    for name in &add {
        if name.trim().is_empty() {
            refused.push(json!({ "op": "add", "item": name, "reason": "empty_name" }));
            continue;
        }
        if writes.expired() {
            failed_ops.push(deadline_op("add", name));
            continue;
        }
        match state.graph.create_checklist_item(
            &list.id,
            &task_id,
            json!({ "displayName": name }),
            &mut writes,
        ) {
            Ok(item) => {
                added.push(item.display_name.clone());
                items.push(item);
                touched = true;
            }
            Err(e) => {
                let (code, msg) = app_error_parts(&e);
                failed_ops
                    .push(json!({ "op": "add", "item": name, "error_code": code, "message": msg }));
                touched = true;
            }
        }
    }
    if touched {
        invalidate_after(state, &list.id, &Ok(()));
    }
    success(json!({
        "task_id": task_id,
        "added": added, "checked": checked, "unchecked": unchecked, "renamed": renamed, "removed": removed,
        "refused": refused, "failed": failed_ops,
        "checklist_after": items.iter().map(|c| json!({ "item_id": c.id, "name": c.display_name, "checked": c.is_checked })).collect::<Vec<_>>(),
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn patch_wire_matches_the_verified_output() {
        let p = TaskPatch {
            title: Patch::Set("Buy milk".into()),
            due_date_time: Patch::Null,
            reminder_date_time: Patch::Null,
            is_reminder_on: Patch::Set(false),
            categories: Patch::Set(vec![]),
            body: Patch::Set(json!({"content": "", "contentType": "html"})),
            recurrence: Patch::Null,
            ..TaskPatch::default()
        };
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            r#"{"title":"Buy milk","dueDateTime":null,"reminderDateTime":null,"isReminderOn":false,"categories":[],"body":{"content":"","contentType":"html"},"recurrence":null}"#
        );
        assert_eq!(serde_json::to_string(&TaskPatch::default()).unwrap(), "{}");
    }

    #[test]
    fn absent_without_skip_is_an_error() {
        #[derive(Serialize)]
        struct Naked {
            x: Patch<u8>,
        }
        assert!(serde_json::to_string(&Naked { x: Patch::Absent }).is_err());
    }

    #[test]
    fn outbound_dates_use_graphs_seven_digit_form() {
        let d = date_value(
            "2026-08-25".parse().unwrap(),
            "Central Europe Standard Time",
        );
        assert_eq!(
            d,
            json!({"dateTime": "2026-08-25T00:00:00.0000000", "timeZone": "Central Europe Standard Time"})
        );
        let dt = datetime_value("2026-08-25T09:00:00".parse().unwrap(), "UTC");
        assert_eq!(dt["dateTime"], "2026-08-25T09:00:00.0000000");
    }
}
