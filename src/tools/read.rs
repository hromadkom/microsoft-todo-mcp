//! The five read tools (m4 §4, m5 §7). Read-only is enforced by there being
//! no write path in this file, not by the annotations.

use chrono::NaiveDate;
use chrono_tz::Tz;
use serde_json::{Value, json};

use super::cursor::{self, Cursor};
use super::render::{
    self, BODY_MAX_BYTES, drop_previews, error, failure, invalid_args, success, task_summary,
};
use super::{
    arg_bool, arg_date, arg_enum, arg_str, arg_str_max, arg_string_array, arg_timezone, arg_u64,
    reject_unknown_keys,
};
use crate::auth::{Grant, scope_short_names};
use crate::domain::datetime::{add_days, local_date, parse_instant, sub_days, today_in};
use crate::domain::filter::{self, Criteria, DueFilter, Sort, StatusFilter, score, sort_scored};
use crate::domain::resolve::fold;
use crate::domain::text::{html_to_text, truncate_bytes};
use crate::errors::AppError;
use crate::graph::models::{Task, TaskList};
use crate::graph::tasks::{Walk, lists_probe_url};
use crate::server::{Coverage, ListSnapshot, ServerState, SyncOutcome};

const ACCOUNT_ID: &str = "default";

/// A single-list read has trivially complete coverage.
fn single_list_coverage(
    list: &TaskList,
    complete: bool,
    tasks: usize,
    budget: &crate::graph::Budget,
) -> Coverage {
    Coverage {
        lists_total: 1,
        lists_complete: usize::from(complete),
        lists_partial: if complete {
            vec![]
        } else {
            vec![list.display_name.clone()]
        },
        lists_missing: vec![],
        tasks_seen: tasks,
        stopped_because: if complete {
            "complete".into()
        } else {
            "page_cap".into()
        },
        elapsed_ms: budget.elapsed_ms(),
        graph_requests: budget.requests_used,
        banner: if complete {
            None
        } else {
            Some(format!(
                "PARTIAL RESULT — list \"{}\" was cut short by the page cap. Call again to continue.",
                list.display_name
            ))
        },
        retry_hint: if complete {
            None
        } else {
            Some("Call the same tool again with the same arguments.".into())
        },
    }
}

// ---------------------------------------------------------------------------

pub fn todo_lists(state: &ServerState, args: &Value) -> Value {
    if let Err(e) = reject_unknown_keys(args, &["include_counts", "timezone"]) {
        return invalid_args("todo_lists", &e);
    }
    let include_counts = match arg_bool(args, "include_counts") {
        Ok(v) => v.unwrap_or(true),
        Err(e) => return invalid_args("todo_lists", &e),
    };
    let tz = match arg_timezone(args, state) {
        Ok(tz) => tz,
        Err(e) => return invalid_args("todo_lists", &e),
    };
    let mut budget = state.budget();
    let lists = match state.catalogue(&mut budget) {
        Ok(l) => l,
        Err(e) => return failure(&e, state),
    };
    let today = today_in(state.clock.now(), tz);
    let now = std::time::Instant::now();
    let cache = state.cache_read();
    let mut counts_from_cache = false;
    let rendered: Vec<Value> = lists
        .iter()
        .map(|l| {
            let warm = cache.list(&l.id, now);
            let (open, overdue) = match warm {
                Some(e) if include_counts => {
                    counts_from_cache = true;
                    let open = e.tasks.iter().filter(|t| !t.is_completed()).count();
                    let overdue = e
                        .tasks
                        .iter()
                        .filter(|t| !t.is_completed())
                        .filter(|t| {
                            t.due_date_time
                                .as_ref()
                                .and_then(|d| local_date(d, tz).ok())
                                .is_some_and(|d| d < today)
                        })
                        .count();
                    (json!(open), json!(overdue))
                }
                _ => (Value::Null, Value::Null),
            };
            json!({
                "list_id": l.id,
                "name": l.display_name,
                "wellknown_list_name": l.wellknown_list_name,
                "is_owner": l.is_owner,
                "is_shared": l.is_shared,
                "open_count": open,
                "overdue_count": overdue,
                "cached": warm.is_some(),
            })
        })
        .collect();
    drop(cache);
    let n = rendered.len();
    let structured = json!({
        "account_id": ACCOUNT_ID,
        "timezone": tz.name(),
        "counts_from_cache": counts_from_cache,
        "lists": rendered,
    });
    render::finish(
        state,
        structured,
        n,
        &mut |step, s| {
            if step > 0 {
                return false;
            }
            if let Some(arr) = s["lists"].as_array_mut() {
                for l in arr {
                    l["open_count"] = Value::Null;
                    l["overdue_count"] = Value::Null;
                }
            }
            s["counts_from_cache"] = json!(false);
            true
        },
        &|_| "Result exceeds the response cap even without counts.".to_string(),
    )
}

// ---------------------------------------------------------------------------

struct SearchArgs<'a> {
    list: Option<&'a str>,
    query: Option<&'a str>,
    status: &'a str,
    due: &'a str,
    due_from: Option<NaiveDate>,
    due_to: Option<NaiveDate>,
    importance: Option<&'a str>,
    categories: Vec<String>,
    sort: &'a str,
    limit: usize,
    tz: Tz,
}

impl SearchArgs<'_> {
    fn canonical(&self) -> String {
        let mut cats: Vec<String> = self.categories.iter().map(|c| fold(c)).collect();
        cats.sort();
        format!(
            "list={}|query={}|status={}|due={}|due_from={}|due_to={}|importance={}|categories={}|sort={}|timezone={}",
            self.list.unwrap_or(""),
            self.query.unwrap_or(""),
            self.status,
            self.due,
            self.due_from.map(|d| d.to_string()).unwrap_or_default(),
            self.due_to.map(|d| d.to_string()).unwrap_or_default(),
            self.importance.unwrap_or(""),
            cats.join(","),
            self.sort,
            self.tz.name(),
        )
    }
}

const SEARCH_KEYS: [&str; 12] = [
    "list",
    "query",
    "status",
    "due",
    "due_from",
    "due_to",
    "importance",
    "categories",
    "sort",
    "limit",
    "cursor",
    "timezone",
];
const FILTER_KEYS: [&str; 10] = [
    "list",
    "query",
    "status",
    "due",
    "due_from",
    "due_to",
    "importance",
    "categories",
    "sort",
    "timezone",
];

fn parse_search_args<'a>(state: &ServerState, args: &'a Value) -> Result<SearchArgs<'a>, String> {
    reject_unknown_keys(args, &SEARCH_KEYS)?;
    let cursor = arg_str(args, "cursor")?;
    if cursor.is_some()
        && FILTER_KEYS
            .iter()
            .any(|k| args.get(k).is_some_and(|v| !v.is_null()))
    {
        return Err(cursor::REFUSE_MIXED.to_string());
    }
    let due = arg_enum(
        args,
        "due",
        &[
            "any",
            "overdue",
            "today",
            "tomorrow",
            "this_week",
            "next_7_days",
            "no_due_date",
            "has_due_date",
        ],
    )?;
    let due_from = arg_date(args, "due_from")?;
    let due_to = arg_date(args, "due_to")?;
    if due.is_some() && (due_from.is_some() || due_to.is_some()) {
        return Err("due and due_from/due_to are mutually exclusive; pass one or the other".into());
    }
    if let (Some(f), Some(t)) = (due_from, due_to)
        && f > t
    {
        return Err("due_from must not be after due_to".into());
    }
    Ok(SearchArgs {
        list: arg_str(args, "list")?,
        query: arg_str_max(args, "query", 200)?,
        status: arg_enum(args, "status", &["open", "completed", "any"])?.unwrap_or("open"),
        due: due.unwrap_or("any"),
        due_from,
        due_to,
        importance: arg_enum(args, "importance", &["low", "normal", "high"])?,
        categories: arg_string_array(args, "categories", 20)?.unwrap_or_default(),
        sort: arg_enum(
            args,
            "sort",
            &[
                "due_asc",
                "due_desc",
                "created_desc",
                "created_asc",
                "modified_desc",
                "title_asc",
                "importance_desc",
            ],
        )?
        .unwrap_or("due_asc"),
        limit: arg_u64(args, "limit", 1, 200)?.unwrap_or(50) as usize,
        tz: arg_timezone(args, state)?,
    })
}

/// Fetch the tasks a search or agenda works over: one list, a named subset,
/// or everything via the bounded sync.
fn gather(
    state: &ServerState,
    list_names: &[&str],
    budget: &mut crate::graph::Budget,
) -> Result<SyncOutcome, Value> {
    if list_names.is_empty() {
        return state.sync_all(budget).map_err(|e| failure(&e, state));
    }
    let lists = state.catalogue(budget).map_err(|e| failure(&e, state))?;
    let mut snapshots = Vec::new();
    let mut chosen = Vec::new();
    let mut partial = Vec::new();
    let mut tasks_seen = 0;
    for name in list_names {
        let l = state.resolve_list(name, &lists)?;
        if chosen.iter().any(|c: &TaskList| c.id == l.id) {
            continue;
        }
        let (tasks, complete) = state
            .list_tasks(l, budget)
            .map_err(|e| failure(&e, state))?;
        tasks_seen += tasks.len();
        if !complete {
            partial.push(l.display_name.clone());
        }
        snapshots.push(ListSnapshot {
            list: l.clone(),
            tasks,
            complete,
            present: true,
        });
        chosen.push(l.clone());
    }
    let complete = partial.is_empty();
    let coverage = if chosen.len() == 1 {
        single_list_coverage(&chosen[0], complete, tasks_seen, budget)
    } else {
        Coverage {
            lists_total: chosen.len(),
            lists_complete: chosen.len() - partial.len(),
            lists_partial: partial,
            lists_missing: vec![],
            tasks_seen,
            stopped_because: if complete {
                "complete".into()
            } else {
                "page_cap".into()
            },
            elapsed_ms: budget.elapsed_ms(),
            graph_requests: budget.requests_used,
            banner: if complete {
                None
            } else {
                Some("PARTIAL RESULT — one or more lists were cut short by the page cap. Call again to continue.".into())
            },
            retry_hint: if complete {
                None
            } else {
                Some("Call the same tool again with the same arguments.".into())
            },
        }
    };
    Ok(SyncOutcome {
        lists: chosen,
        snapshots,
        coverage,
    })
}

pub fn todo_search_tasks(state: &ServerState, args: &Value) -> Value {
    // A cursor passed alone carries its own filters: restore them, keeping
    // only `limit` from the new call. Check 1 (mixed filters) and check 2
    // (undecodable) are pre-flight; 3–7 run once the cache is settled.
    let restored: Value;
    let (args, continuing): (&Value, Option<Cursor>) =
        match args.get("cursor").and_then(Value::as_str) {
            Some(raw) => {
                if let Err(e) = reject_unknown_keys(args, &SEARCH_KEYS) {
                    return invalid_args("todo_search_tasks", &e);
                }
                if FILTER_KEYS
                    .iter()
                    .any(|k| args.get(k).is_some_and(|v| !v.is_null()))
                {
                    return error(cursor::REFUSE_MIXED.to_string());
                }
                let Some(c) = cursor::decode(raw) else {
                    return error(cursor::REFUSE_INVALID.to_string());
                };
                let mut merged = c.f.clone();
                if !merged.is_object() {
                    return error(cursor::REFUSE_INVALID.to_string());
                }
                if let Some(l) = args.get("limit") {
                    merged["limit"] = l.clone();
                }
                restored = merged;
                (&restored, Some(c))
            }
            None => (args, None),
        };
    let a = match parse_search_args(state, args) {
        Ok(a) => a,
        Err(e) => return invalid_args("todo_search_tasks", &e),
    };
    let q = cursor::args_hash(&a.canonical());
    // The filters a continuation restores: everything but limit and cursor.
    let filters: Value = {
        let mut f = serde_json::Map::new();
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                if FILTER_KEYS.contains(&k.as_str()) && !v.is_null() {
                    f.insert(k.clone(), v.clone());
                }
            }
        }
        Value::Object(f)
    };
    let mut budget = state.budget();
    let names: Vec<&str> = a.list.into_iter().collect();
    let outcome = match gather(state, &names, &mut budget) {
        Ok(o) => o,
        Err(v) => return v,
    };
    // Checks 2–7 run against the cache the gather just settled: the mailbox
    // label is only known once the catalogue has been fetched.
    let (mailbox, generation) = {
        let c = state.cache_read();
        (c.mailbox_id.clone(), c.generation)
    };
    let now_unix = state.clock.now().timestamp();
    let offset = match &continuing {
        None => 0,
        Some(c) => match cursor::validate(&cursor::encode(c), &mailbox, now_unix, generation, q) {
            Ok(c) => c.o,
            Err(msg) => return error(msg.to_string()),
        },
    };
    let today = today_in(state.clock.now(), a.tz);
    let criteria = Criteria {
        query_terms: a.query.map(filter::query_terms).unwrap_or_default(),
        status: match a.status {
            "completed" => StatusFilter::Completed,
            "any" => StatusFilter::Any,
            _ => StatusFilter::Open,
        },
        due: DueFilter::parse(a.due).unwrap_or(DueFilter::Any),
        due_from: a.due_from,
        due_to: a.due_to,
        importance: a.importance.map(str::to_string),
        categories: a.categories.iter().map(|c| fold(c)).collect(),
        sort: Sort::parse(a.sort).unwrap_or(Sort::DueAsc),
    };
    let mut matched: Vec<(&ListSnapshot, filter::Scored<'_>)> = Vec::new();
    for snap in &outcome.snapshots {
        for t in &snap.tasks {
            let s = score(t, a.tz);
            if filter::matches(&s, &criteria, today) {
                matched.push((snap, s));
            }
        }
    }
    // Sort by the scored view, then reattach lists.
    let mut scored: Vec<filter::Scored<'_>> = matched
        .iter()
        .map(|(_, s)| filter::Scored {
            task: s.task,
            due_local: s.due_local,
            due_unresolved: s.due_unresolved,
        })
        .collect();
    sort_scored(&mut scored, criteria.sort);
    let list_of = |task_id: &str| -> &ListSnapshot {
        matched
            .iter()
            .find(|(_, s)| s.task.id == task_id)
            .map(|(l, _)| *l)
            .unwrap_or(&outcome.snapshots[0])
    };
    let total = scored.len();

    let page: Vec<Value> = scored
        .iter()
        .skip(offset)
        .take(a.limit)
        .map(|s| {
            let snap = list_of(&s.task.id);
            task_summary(s.task, &snap.list.id, &snap.list.display_name, a.tz, state)
        })
        .collect();
    let returned = page.len();
    let make_cursor = |o: usize, n: usize| -> Value {
        if o + n >= total {
            Value::Null
        } else {
            json!(cursor::encode(&Cursor {
                v: 1,
                o: o + n,
                a: mailbox.clone(),
                n,
                q,
                t: now_unix,
                g: generation,
                f: filters.clone(),
            }))
        }
    };
    let structured = json!({
        "account_id": ACCOUNT_ID,
        "timezone": a.tz.name(),
        "complete": outcome.coverage.is_complete(),
        "coverage": outcome.coverage.to_json(),
        "total_matched": total,
        "returned": returned,
        "next_cursor": make_cursor(offset, returned),
        "tasks": page,
    });
    render::finish(
        state,
        structured,
        returned,
        &mut |step, s| {
            if step == 0 {
                drop_previews(&mut s["tasks"]);
                s["body_omitted_for_size"] = json!(true);
                return true;
            }
            let current = s["tasks"].as_array().map(Vec::len).unwrap_or(0);
            if current <= 1 {
                return false;
            }
            let keep = current / 2;
            if let Some(arr) = s["tasks"].as_array_mut() {
                arr.truncate(keep);
            }
            s["returned"] = json!(keep);
            s["next_cursor"] = make_cursor(offset, keep);
            true
        },
        &|_| {
            "Result exceeds the response cap even for a single task. Use todo_get_task with body_format:\"none\" or narrow the search.".to_string()
        },
    )
}

// ---------------------------------------------------------------------------

pub fn todo_agenda(state: &ServerState, args: &Value) -> Value {
    if let Err(e) = reject_unknown_keys(
        args,
        &[
            "timezone",
            "horizon_days",
            "include_completed_since_days",
            "max_per_section",
            "lists",
        ],
    ) {
        return invalid_args("todo_agenda", &e);
    }
    let parsed = (|| -> Result<(Tz, u64, u64, usize, Vec<String>), String> {
        Ok((
            arg_timezone(args, state)?,
            arg_u64(args, "horizon_days", 1, 30)?.unwrap_or(7),
            arg_u64(args, "include_completed_since_days", 0, 30)?.unwrap_or(1),
            arg_u64(args, "max_per_section", 1, 50)?.unwrap_or(15) as usize,
            arg_string_array(args, "lists", 20)?.unwrap_or_default(),
        ))
    })();
    let (tz, horizon, completed_days, max_per, list_names) = match parsed {
        Ok(v) => v,
        Err(e) => return invalid_args("todo_agenda", &e),
    };
    let mut budget = state.budget();
    let names: Vec<&str> = list_names.iter().map(String::as_str).collect();
    let outcome = match gather(state, &names, &mut budget) {
        Ok(o) => o,
        Err(v) => return v,
    };
    let now = state.clock.now();
    let today = today_in(now, tz);
    let soon_end = add_days(today, horizon);
    let completed_since = sub_days(today, completed_days);

    struct Row<'a> {
        snap: &'a ListSnapshot,
        task: &'a Task,
        due: Option<NaiveDate>,
        created: i64,
        completed: Option<NaiveDate>,
    }
    let mut overdue = Vec::new();
    let mut due_today = Vec::new();
    let mut due_soon = Vec::new();
    let mut no_due = Vec::new();
    let mut flagged = Vec::new();
    let mut recent = Vec::new();
    for snap in &outcome.snapshots {
        let is_flagged = snap.list.wellknown_list_name.as_deref() == Some("flaggedEmails");
        for t in &snap.tasks {
            let s = score(t, tz);
            let row = Row {
                snap,
                task: t,
                due: s.due_local,
                created: t
                    .created_date_time
                    .as_deref()
                    .and_then(parse_instant)
                    .map(|d| d.timestamp())
                    .unwrap_or(0),
                completed: t
                    .completed_date_time
                    .as_ref()
                    .and_then(|d| local_date(d, tz).ok()),
            };
            if s.due_unresolved {
                state.warn_tz_once(
                    &t.due_date_time
                        .as_ref()
                        .map(|d| d.time_zone.clone())
                        .unwrap_or_default(),
                );
            }
            if t.is_completed() {
                if row.completed.is_some_and(|d| d >= completed_since) {
                    recent.push(row);
                }
                continue;
            }
            if is_flagged {
                flagged.push(row);
                continue;
            }
            match row.due {
                Some(d) if d < today => overdue.push(row),
                Some(d) if d == today => due_today.push(row),
                Some(d) if d > today && d <= soon_end => due_soon.push(row),
                Some(_) => {}
                // An unresolved zone belongs to NO bucket (m5 §6).
                None if t.due_date_time.is_none() => no_due.push(row),
                None => {}
            }
        }
    }
    overdue.sort_by(|a, b| {
        a.due
            .cmp(&b.due)
            .then_with(|| a.task.title.cmp(&b.task.title))
    });
    due_today.sort_by(|a, b| a.task.title.cmp(&b.task.title));
    due_soon.sort_by(|a, b| {
        a.due
            .cmp(&b.due)
            .then_with(|| a.task.title.cmp(&b.task.title))
    });
    no_due.sort_by_key(|r| std::cmp::Reverse(r.created));
    flagged.sort_by(|a, b| a.due.cmp(&b.due).then_with(|| b.created.cmp(&a.created)));
    recent.sort_by_key(|r| std::cmp::Reverse(r.completed));

    let section = |rows: &[Row<'_>], cap: usize| -> Value {
        json!({
            "count": rows.len(),
            "truncated": rows.len() > cap,
            "tasks": rows.iter().take(cap).map(|r| task_summary(r.task, &r.snap.list.id, &r.snap.list.display_name, tz, state)).collect::<Vec<_>>(),
        })
    };
    let build = |cap: usize| -> Value {
        json!({
            "overdue": section(&overdue, cap),
            "due_today": section(&due_today, cap),
            "due_soon": section(&due_soon, cap),
            "no_due_date": section(&no_due, cap),
            "flagged_emails": section(&flagged, cap),
            "recently_completed": section(&recent, cap),
        })
    };
    let structured = json!({
        "account_id": ACCOUNT_ID,
        "timezone": tz.name(),
        "today": today.to_string(),
        "complete": outcome.coverage.is_complete(),
        "coverage": outcome.coverage.to_json(),
        "sections": build(max_per),
    });
    let total_items = overdue.len()
        + due_today.len()
        + due_soon.len()
        + no_due.len()
        + flagged.len()
        + recent.len();
    let mut cap = max_per;
    render::finish(
        state,
        structured,
        total_items,
        &mut |step, s| {
            if step == 0 {
                if let Some(secs) = s["sections"].as_object_mut() {
                    for (_, sec) in secs.iter_mut() {
                        drop_previews(&mut sec["tasks"]);
                    }
                }
                return true;
            }
            if cap <= 1 {
                return false;
            }
            cap /= 2;
            s["sections"] = build(cap);
            if let Some(secs) = s["sections"].as_object_mut() {
                for (_, sec) in secs.iter_mut() {
                    drop_previews(&mut sec["tasks"]);
                }
            }
            true
        },
        &|_| {
            "Agenda exceeds the response cap even at one task per section. Use todo_search_tasks per list.".to_string()
        },
    )
}

// ---------------------------------------------------------------------------

pub fn todo_get_task(state: &ServerState, args: &Value) -> Value {
    if let Err(e) = reject_unknown_keys(args, &["task_id", "list", "body_format", "timezone"]) {
        return invalid_args("todo_get_task", &e);
    }
    let parsed = (|| -> Result<(String, Option<&str>, &str, Tz), String> {
        let task_id = arg_str(args, "task_id")?
            .filter(|s| !s.trim().is_empty())
            .ok_or("task_id is required")?;
        Ok((
            task_id.to_string(),
            arg_str(args, "list")?,
            arg_enum(args, "body_format", &["text", "html", "none"])?.unwrap_or("text"),
            arg_timezone(args, state)?,
        ))
    })();
    let (task_id, list_name, body_format, tz) = match parsed {
        Ok(v) => v,
        Err(e) => return invalid_args("todo_get_task", &e),
    };
    let mut budget = state.budget();
    let lists = match state.catalogue(&mut budget) {
        Ok(l) => l,
        Err(e) => return failure(&e, state),
    };
    // Which list holds the task?
    let list: TaskList = match list_name {
        Some(name) => match state.resolve_list(name, &lists) {
            Ok(l) => l.clone(),
            Err(v) => return v,
        },
        None => {
            // Any cached entry places the id (an id never changes list).
            let cached: Option<TaskList> = {
                let c = state.cache_read();
                c.find_task(&task_id)
                    .and_then(|(lid, _)| lists.iter().find(|l| l.id == lid))
                    .cloned()
            };
            match cached {
                Some(l) => l,
                None => {
                    // Looked up in what the sync returned, never the cache:
                    // at TTL 0 the cache holds nothing afterwards.
                    let outcome = match state.sync_lists(&lists, &mut budget) {
                        Ok(o) => o,
                        Err(e) => return failure(&e, state),
                    };
                    let found = outcome
                        .snapshots
                        .iter()
                        .find(|s| s.tasks.iter().any(|t| t.id == task_id))
                        .map(|s| s.list.clone());
                    match found {
                        Some(l) => l,
                        None if outcome.coverage.is_complete() => {
                            return error(format!(
                                "task {task_id} was not found in any list (pass \"list\" if you know it; the id may be stale)"
                            ));
                        }
                        None => {
                            let cov = &outcome.coverage;
                            let next = if state.cfg.cache_ttl_seconds == 0 {
                                "Pass \"list\" if you know it; the cache is disabled (TODO_MCP_CACHE_TTL_SECONDS=0), so calling again repeats the same sync."
                            } else {
                                "Pass \"list\" if you know it, or call again (lists already synced are cached)."
                            };
                            return error(format!(
                                "task {task_id} is not in the {} of {} lists synced so far: the sync stopped early (stopped: {}). {next}",
                                cov.lists_complete, cov.lists_total, cov.stopped_because
                            ));
                        }
                    }
                }
            }
        }
    };
    let task = match state.graph.get_task(&list.id, &task_id, &mut budget) {
        Ok(t) => t,
        Err(AppError::Graph { status: 404, .. }) => {
            return error(format!(
                "task {task_id} does not exist in list \"{}\"",
                list.display_name
            ));
        }
        Err(e) => return failure(&e, state),
    };
    let checklist = match state.graph.list_checklist(&list.id, &task_id, &mut budget) {
        Ok(Walk::Complete(c)) => c,
        Ok(Walk::Incomplete) => {
            return render::incomplete_walk(state, "checklist", list_name.is_some());
        }
        Err(e) => return failure(&e, state),
    };
    let attachments = if task.has_attachments {
        match state
            .graph
            .list_attachments(&list.id, &task_id, &mut budget)
        {
            Ok(Walk::Complete(a)) => a,
            Ok(Walk::Incomplete) => {
                return render::incomplete_walk(state, "attachment list", list_name.is_some());
            }
            Err(e) => return failure(&e, state),
        }
    } else {
        vec![]
    };
    let mut full = task.clone();
    full.checklist_items = Some(checklist.clone());
    let summary = task_summary(&full, &list.id, &list.display_name, tz, state);

    let (body, body_ct, body_truncated, body_total) = match (&task.body, body_format) {
        (_, "none") | (None, _) => (Value::Null, Value::Null, false, 0usize),
        (Some(b), "html") => {
            let (cut, tr) = truncate_bytes(&b.content, BODY_MAX_BYTES);
            (json!(cut), json!(b.content_type), tr, b.content.len())
        }
        (Some(b), _) => {
            let text = if b.content_type.eq_ignore_ascii_case("html") {
                html_to_text(&b.content)
            } else {
                b.content.clone()
            };
            let (cut, tr) = truncate_bytes(&text, BODY_MAX_BYTES);
            (json!(cut), json!("text"), tr, text.len())
        }
    };
    let linked = task.linked_resources.clone().unwrap_or_default();
    let structured = json!({
        "account_id": ACCOUNT_ID,
        "timezone": tz.name(),
        "task": summary,
        "body": body,
        "body_content_type": body_ct,
        "body_truncated": body_truncated,
        "body_bytes_total": body_total,
        "recurrence": task.recurrence,
        "checklist_items": checklist.iter().take(200).map(|c| json!({
            "item_id": c.id, "name": c.display_name, "checked": c.is_checked,
            "created_at": c.created_date_time, "checked_at": c.checked_date_time,
        })).collect::<Vec<_>>(),
        "checklist_truncated": checklist.len() > 200,
        "checklist_total": checklist.len(),
        "linked_resources": linked.iter().take(50).map(|r| json!({
            "id": r.id, "web_url": r.web_url, "application_name": r.application_name,
            "display_name": r.display_name, "external_id": r.external_id,
        })).collect::<Vec<_>>(),
        "linked_resources_truncated": linked.len() > 50,
        "linked_resources_total": linked.len(),
        "attachments": attachments.iter().take(50).map(|a| json!({
            "id": a.id, "name": a.name, "content_type": a.content_type, "size": a.size,
            "last_modified_at": a.last_modified_date_time,
        })).collect::<Vec<_>>(),
        "attachments_truncated": attachments.len() > 50,
        "attachments_total": attachments.len(),
        "has_attachments": task.has_attachments,
        "etag": task.etag,
        "web_link": Value::Null,
    });
    let tid = task_id.clone();
    render::finish(
        state,
        structured,
        1,
        &mut |step, s| {
            let halve = |s: &mut Value, key: &str, floor: usize, flag: &str| {
                if let Some(arr) = s[key].as_array_mut() {
                    let keep = (arr.len() / 2).max(floor).min(arr.len());
                    if keep < arr.len() {
                        arr.truncate(keep);
                        s[flag] = json!(true);
                    }
                }
            };
            match step {
                0 => {
                    halve(s, "checklist_items", 10, "checklist_truncated");
                    halve(s, "linked_resources", 5, "linked_resources_truncated");
                    halve(s, "attachments", 5, "attachments_truncated");
                    true
                }
                1 => {
                    s["body"] = Value::Null;
                    s["body_truncated"] = json!(true);
                    true
                }
                _ => false,
            }
        },
        &|s| {
            format!(
                "Result exceeds the {} KiB response cap even for a single task (task `{tid}`, ~{} KiB). Call todo_get_task with body_format:\"none\" to inspect it.",
                state.cfg.tool_result_max_bytes / 1024,
                s.to_string().len() / 1024
            )
        },
    )
}

// ---------------------------------------------------------------------------

pub fn todo_account_status(state: &ServerState, args: &Value) -> Value {
    if let Err(e) = reject_unknown_keys(args, &["check_connectivity"]) {
        return invalid_args("todo_account_status", &e);
    }
    let check = match arg_bool(args, "check_connectivity") {
        Ok(v) => v.unwrap_or(false),
        Err(e) => return invalid_args("todo_account_status", &e),
    };
    let connectivity = if check {
        let mut budget = state.budget();
        match state.graph.get_json(&lists_probe_url(), &mut budget) {
            Ok(_) => json!({ "checked": true, "ok": true, "detail": Value::Null }),
            Err(e) => json!({ "checked": true, "ok": false, "detail": e.message() }),
        }
    } else {
        json!({ "checked": false, "ok": Value::Null, "detail": Value::Null })
    };
    let ts = state.graph.tokens().status();
    let eff = if state.follows_logins() {
        ts.live_scope.is_some().then_some(ts.live_grant)
    } else {
        state.boot_grant()
    };
    let live = if ts.live_scope.is_some() {
        ts.live_grant
    } else {
        eff.unwrap_or(Grant::None)
    };
    let restart_reason = state.restart_reason_for(ts.live_scope.is_some().then_some(ts.live_grant));
    let write_enabled = if state.follows_logins() {
        eff == Some(Grant::ReadWrite) && ts.live_scope.is_some()
    } else {
        state.boot_grant() == Some(Grant::ReadWrite) && ts.file_present
    };
    let widened = !write_enabled && live == Grant::ReadWrite;
    let stats = state.cache_read().stats(std::time::Instant::now());
    let gs = state.graph.stats();
    let client_tail: String = state
        .cfg
        .client_id
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let structured = json!({
        "account_id": ACCOUNT_ID,
        "identity": "not requested (no openid scope)",
        "grant": live.as_str(),
        "scopes_granted": ts.live_scope.as_deref().map(scope_short_names).unwrap_or_default(),
        "write_tools_enabled": write_enabled,
        "restart_required": restart_reason.is_some(),
        "restart_reason": restart_reason,
        "token": {
            "present": ts.file_present,
            "access_token_expires_at": ts.expires_at.map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            "refresh_token_obtained_at": ts.obtained_at,
            "refreshes": ts.refreshes,
            "client_id_tail": if client_tail.is_empty() { Value::Null } else { json!(format!("…{client_tail}")) },
        },
        "login_command": state.login_command(),
        "timezone": state.tz.name(),
        "timezone_configured": state.cfg.tz.is_some(),
        "timezone_mode": gs.timezone_mode.unwrap_or("unknown"),
        "connectivity": connectivity,
        "cache": stats,
        "graph": {
            "requests": gs.requests,
            "throttled_429": gs.throttled_429,
            "retries": gs.retries,
            "transport_failures": gs.transport_failures,
            "last_status": gs.last_status,
            "last_retry_after_secs": gs.last_retry_after_secs,
        },
        "server": {
            "version": env!("CARGO_PKG_VERSION"),
            "protocol_version": crate::mcp::LATEST_PROTOCOL_VERSION,
            "uptime_seconds": state.started.elapsed().as_secs(),
        },
    });
    let mut out = success(structured);
    if widened && let Some(content) = out["content"].as_array_mut() {
        // The model cannot restart a container: say the command in TEXT, as a
        // second block so content[0] stays the exact JSON (m2 §7).
        content.push(json!({
            "type": "text",
            "text": format!("{} to expose the write tools.", crate::errors::RESTART_HINT)
        }));
    }
    out
}
