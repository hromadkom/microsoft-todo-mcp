//! Read tools over the fixture Graph server: cache TTL, cursors, agenda
//! buckets, and the "never triggers a sync" rules.

mod common;

use std::time::Instant;

use chrono::{Duration, TimeZone, Utc};
use serde_json::{Value, json};

use common::{
    Override, harness, harness_booted, harness_unsigned, pinned_now, seed_22_lists,
    write_token_file,
};
use microsoft_todo_mcp::auth::Grant;
use microsoft_todo_mcp::cache::log_ref;
use microsoft_todo_mcp::mcp::ToolProvider;

const RW_SCOPE: &str = "https://graph.microsoft.com/Tasks.ReadWrite offline_access";
const READ_SCOPE: &str = "https://graph.microsoft.com/Tasks.Read offline_access";

#[test]
fn an_unsigned_server_recovers_on_the_first_call_after_login() {
    let h = harness_unsigned(&[], RW_SCOPE);
    assert!(!h.dir.join("token.json").exists());

    let lists = h.call("todo_lists", json!({}));
    assert_eq!(lists["isError"], true, "{lists}");
    assert!(
        lists["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("auth_required"),
        "{lists}"
    );
    let status = h.call("todo_account_status", json!({}));
    assert_eq!(status["structuredContent"]["grant"], "none", "{status}");
    assert_eq!(
        status["structuredContent"]["write_tools_enabled"], false,
        "{status}"
    );
    assert_eq!(
        status["structuredContent"]["token"]["present"], false,
        "{status}"
    );
    let write = h.call("todo_create_tasks", json!({"tasks": [{"title": "x"}]}));
    assert_eq!(write["isError"], true, "{write}");
    assert!(
        write["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("auth_required"),
        "{write}"
    );

    write_token_file(&h.dir, "RT-OLD", RW_SCOPE);
    let lists = h.call("todo_lists", json!({}));
    assert!(lists.get("isError").is_none(), "{lists}");
    assert_eq!(h.state.effective_grant(), Some(Grant::ReadWrite));
    let status = h.call("todo_account_status", json!({}));
    assert_eq!(
        status["structuredContent"]["grant"], "Tasks.ReadWrite",
        "{status}"
    );
    assert_eq!(
        status["structuredContent"]["write_tools_enabled"], true,
        "{status}"
    );
}

#[test]
fn an_unsigned_server_refuses_writes_after_a_narrower_grant_lands() {
    let h = harness_unsigned(&[], READ_SCOPE);
    write_token_file(&h.dir, "RT-OLD", RW_SCOPE);
    let lists = h.call("todo_lists", json!({}));
    assert!(lists.get("isError").is_none(), "{lists}");
    let write = h.call("todo_create_tasks", json!({"tasks": [{"title": "x"}]}));
    assert_eq!(write["isError"], true, "{write}");
    let text = write["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("current Microsoft Graph grant is `Tasks.Read`"),
        "{text}"
    );
}

#[test]
fn an_unsigned_read_ceiling_has_five_tools_and_refuses_writes_as_read_only() {
    let h = harness_unsigned(&[("TODO_MCP_SCOPE", "Tasks.Read")], READ_SCOPE);
    assert_eq!(h.state.list_tools().as_array().unwrap().len(), 5);

    let write = h.call("todo_create_tasks", json!({"tasks": [{"title": "x"}]}));
    assert_eq!(write["isError"], true, "{write}");
    let text = write["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("TODO_MCP_SCOPE=Tasks.Read"), "{text}");
    assert_eq!(h.graph_requests(), 0);

    let lists = h.call("todo_lists", json!({}));
    assert_eq!(lists["isError"], true, "{lists}");
    assert!(
        lists["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("auth_required"),
        "{lists}"
    );
}

#[test]
fn an_unsigned_server_follows_a_widened_grant_without_restart() {
    let h = harness_unsigned(&[], READ_SCOPE);
    write_token_file(&h.dir, "RT-OLD", RW_SCOPE);
    h.fx.add_list("Tasks", Some("defaultList"));

    let first = h.call("todo_lists", json!({}));
    assert!(first.get("isError").is_none(), "{first}");
    assert_eq!(h.state.effective_grant(), Some(Grant::ReadOnly));

    *h.endpoint.scope.lock().unwrap() = Some(RW_SCOPE.to_string());
    write_token_file(&h.dir, "RT-NEW", RW_SCOPE);
    h.clock.set(pinned_now() + Duration::seconds(3600));
    let status = h.call("todo_account_status", json!({"check_connectivity": true}));
    assert_eq!(
        status["structuredContent"]["restart_required"], false,
        "{status}"
    );
    assert_eq!(
        status["structuredContent"]["grant"], "Tasks.ReadWrite",
        "{status}"
    );

    let write = h.call(
        "todo_create_tasks",
        json!({"list": "Tasks", "tasks": [{"title": "x"}]}),
    );
    assert!(write.get("isError").is_none(), "{write}");
}

#[test]
fn harness_booted_readwrite_scope_builds_all_ten_tools() {
    let h = harness_booted(&[], RW_SCOPE);
    assert_eq!(h.state.list_tools().as_array().unwrap().len(), 10);
}

#[test]
fn a_read_config_caps_a_readwrite_consent_in_tools_list_and_account_status() {
    // Microsoft returns ReadWrite to a Read request: the audit's scenario.
    let h = harness_booted(&[("TODO_MCP_SCOPE", "Tasks.Read")], RW_SCOPE);
    assert_eq!(h.state.list_tools().as_array().unwrap().len(), 5);
    let r = h.call("todo_account_status", json!({}));
    let s = &r["structuredContent"];
    assert_eq!(s["grant"], "Tasks.Read", "{r}");
    assert_eq!(s["write_tools_enabled"], false, "{r}");
    assert_eq!(s["restart_required"], false, "{r}");
    // The raw scopes stay visible.
    assert_eq!(
        s["scopes_granted"],
        json!(["Tasks.ReadWrite", "offline_access"]),
        "{r}"
    );
    assert_eq!(r["content"].as_array().unwrap().len(), 1, "{r}");
}

#[test]
fn a_widening_after_boot_asks_for_a_restart_only_under_a_readwrite_config() {
    for (cfg, expect) in [("Tasks.Read", false), ("Tasks.ReadWrite", true)] {
        let h = harness_booted(&[("TODO_MCP_SCOPE", cfg)], READ_SCOPE);
        assert_eq!(h.state.list_tools().as_array().unwrap().len(), 5, "{cfg}");
        h.fx.add_list("Tasks", Some("defaultList"));
        *h.endpoint.scope.lock().unwrap() = Some(RW_SCOPE.to_string());
        h.clock.set(pinned_now() + Duration::seconds(3600));
        let r = h.call("todo_account_status", json!({ "check_connectivity": true }));
        assert_eq!(h.endpoint.calls(), 2, "{cfg}: the probe refreshed: {r}");
        assert_eq!(
            r["structuredContent"]["restart_required"], expect,
            "{cfg}: {r}"
        );
        let content = r["content"].as_array().unwrap();
        assert_eq!(content.len(), 1 + usize::from(expect), "{cfg}: {r}");
        if expect {
            let text = content[1]["text"].as_str().unwrap();
            assert!(text.contains("docker compose restart"), "{text}");
        }
    }
}

const LOG_HYGIENE_CHILD: &str = "TODO_MCP_LOG_HYGIENE_CHILD";
const SECRET_LIST: &str = "Project Nightingale";
const OTHER_LIST: &str = "Errands for Mum";
const SECRET_TITLE: &str = "Call the oncologist";

/// No log line carries user content. stderr is what the compose `json-file`
/// driver persists to the host disk. `logger` writes straight to fd 2 through
/// `io::stderr().lock()`, which libtest does not capture, so this re-runs THIS
/// binary as a child with `env_clear()` and reads the child's real stderr.
#[test]
fn a_failed_batch_sub_request_logs_neither_the_list_name_nor_its_id() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .env(LOG_HYGIENE_CHILD, "1")
        .args([
            "--exact",
            "child_sync_with_a_failing_sub_request",
            "--test-threads=1",
            "--nocapture",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed:\n{stdout}\n{stderr}");
    // Proves the child really ran (a filter typo would run zero tests and pass).
    assert!(stdout.contains("1 passed"), "child did not run:\n{stdout}");
    let lines: Vec<Value> = stderr
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    let hit = lines
        .iter()
        .find(|v| v["msg"] == "batch sub-request failed")
        .unwrap_or_else(|| panic!("the child never logged the failure:\n{stderr}"));
    assert_eq!(hit["status"], 403, "{hit}");
    // The fixture's first list id is deterministic; asserting through the helper
    // makes a fixture change fail loudly instead of passing vacuously.
    assert_eq!(hit["list_ref"], log_ref("lst", "L1="), "{hit}");
    for needle in [SECRET_LIST, OTHER_LIST, SECRET_TITLE, "L1=", "L2="] {
        assert!(
            !stderr.contains(needle),
            "{needle:?} reached stderr:\n{stderr}"
        );
    }
}

/// Runs only as the parent's child; a no-op in the normal suite.
#[test]
fn child_sync_with_a_failing_sub_request() {
    if std::env::var_os(LOG_HYGIENE_CHILD).is_none() {
        return;
    }
    let h = harness(&[]);
    // defaultList sorts first, so the override hits the first sub-request: L1=.
    let failing = h.fx.add_list(SECRET_LIST, Some("defaultList"));
    assert_eq!(failing, "L1=");
    let other = h.fx.add_list(OTHER_LIST, None);
    // The title reaches memory through the succeeding list, so its absence from
    // stderr is a real claim, not a vacuous one.
    h.fx.add_task(&other, SECRET_TITLE, None, "notStarted");
    // Graph's own error text may echo content; prove it is not logged either.
    h.fx.push_override(Override {
        path_contains: format!("/lists/{failing}/tasks"),
        method: Some("GET".into()),
        status: 403,
        headers: vec![],
        body: json!({ "error": { "code": "ErrorAccessDenied", "message": format!("Access to {SECRET_LIST} is denied") } })
            .to_string(),
        remaining: 1,
    });
    let r = h.ok("todo_search_tasks", json!({}));
    assert_eq!(r["complete"], false, "{r}");
    assert_eq!(r["coverage"]["stopped_because"], "graph_error", "{r}");
    assert_eq!(titles(&r["tasks"]), [SECRET_TITLE], "{r}");
}

#[test]
fn todo_lists_never_fetches_tasks_and_counts_are_null_cold() {
    let h = harness(&[]);
    h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.add_list("Work", None);
    let r = h.ok("todo_lists", json!({ "include_counts": true }));
    assert_eq!(r["lists"].as_array().unwrap().len(), 2);
    assert_eq!(r["counts_from_cache"], false);
    assert!(r["lists"][0]["open_count"].is_null());
    assert_eq!(
        h.fx.count_matching("GET", "/tasks"),
        0,
        "todo_lists must never fetch tasks"
    );
    assert_eq!(h.fx.count_matching("GET", "/me/todo/lists"), 1);
    // Second call inside the TTL: zero requests.
    h.ok("todo_lists", json!({}));
    assert_eq!(h.graph_requests(), 1);
}

#[test]
fn search_inside_ttl_issues_zero_requests_and_refetches_after_expiry() {
    let h = harness(&[("TODO_MCP_CACHE_TTL_SECONDS", "120")]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.add_task(&l, "Buy milk", Some("2026-08-24"), "notStarted");
    h.fx.add_task(&l, "Done thing", None, "completed");
    let r = h.ok("todo_search_tasks", json!({}));
    assert_eq!(r["complete"], true);
    assert_eq!(r["total_matched"], 1, "{r}");
    assert_eq!(r["tasks"][0]["title"], "Buy milk");
    assert_eq!(r["tasks"][0]["due"], "2026-08-24");
    let n = h.graph_requests();
    assert!(n >= 2, "catalogue + batch/first page, got {n}");
    h.ok("todo_search_tasks", json!({}));
    assert_eq!(
        h.graph_requests(),
        n,
        "a repeat search inside the TTL must issue zero Graph requests"
    );
    // Counts are now warm for todo_lists.
    let lists = h.ok("todo_lists", json!({}));
    assert_eq!(lists["lists"][0]["open_count"], 1);
    assert_eq!(lists["lists"][0]["overdue_count"], 1);
    assert_eq!(lists["counts_from_cache"], true);
}

#[test]
fn ttl_zero_refetches_every_call() {
    let h = harness(&[("TODO_MCP_CACHE_TTL_SECONDS", "0")]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.add_task(&l, "a", None, "notStarted");
    h.ok("todo_search_tasks", json!({}));
    let n = h.graph_requests();
    h.ok("todo_search_tasks", json!({}));
    assert!(h.graph_requests() > n);
}

#[test]
fn cursor_pages_every_id_exactly_once_and_refuses_after_a_mutation() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    for i in 0..137 {
        h.fx.add_task(&l, &format!("task {i:03}"), None, "notStarted");
    }
    let mut seen = Vec::new();
    let mut r = h.ok(
        "todo_search_tasks",
        json!({ "limit": 50, "sort": "title_asc" }),
    );
    assert_eq!(r["total_matched"], 137);
    loop {
        for t in r["tasks"].as_array().unwrap() {
            seen.push(t["task_id"].as_str().unwrap().to_string());
        }
        let Some(c) = r["next_cursor"].as_str() else {
            break;
        };
        assert!(
            c.len() < 400,
            "cursor too long for a model to copy: {}",
            c.len()
        );
        r = h.ok("todo_search_tasks", json!({ "cursor": c, "limit": 50 }));
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 137);
    assert_eq!(
        h.graph_requests(),
        3,
        "paging is a cache read, not a refetch"
    );

    // Check 1: filters alongside a cursor.
    let first = h.ok(
        "todo_search_tasks",
        json!({ "limit": 10, "sort": "title_asc" }),
    );
    let c = first["next_cursor"].as_str().unwrap().to_string();
    let e = h.err("todo_search_tasks", json!({ "cursor": c, "status": "any" }));
    assert_eq!(e, microsoft_todo_mcp::tools::cursor::REFUSE_MIXED);
    // Check 2: garbage.
    let e = h.err("todo_search_tasks", json!({ "cursor": "c1_zzz" }));
    assert_eq!(e, microsoft_todo_mcp::tools::cursor::REFUSE_INVALID);
    // Check 6: a mutation (create in an unrelated list) bumps the generation.
    let other = h.fx.add_list("Other", None);
    h.state.cache_write().invalidate_catalogue();
    h.ok(
        "todo_create_tasks",
        json!({ "list": other, "tasks": [{ "title": "x" }] }),
    );
    let e = h.err("todo_search_tasks", json!({ "cursor": c }));
    assert_eq!(e, microsoft_todo_mcp::tools::cursor::REFUSE_STALE);
    // Check 5: expiry by the injected clock.
    let fresh = h.ok(
        "todo_search_tasks",
        json!({ "limit": 10, "sort": "title_asc" }),
    );
    let c = fresh["next_cursor"].as_str().unwrap().to_string();
    h.clock.set(common::pinned_now() + Duration::seconds(901));
    let e = h.err("todo_search_tasks", json!({ "cursor": c }));
    assert_eq!(e, microsoft_todo_mcp::tools::cursor::REFUSE_EXPIRED);
}

#[test]
fn search_pre_flight_refusals_make_zero_requests() {
    let h = harness(&[]);
    h.fx.add_list("Tasks", Some("defaultList"));
    let e = h.err(
        "todo_search_tasks",
        json!({ "due": "today", "due_from": "2026-01-01" }),
    );
    assert!(e.contains("mutually exclusive"), "{e}");
    let e = h.err("todo_search_tasks", json!({ "bogus": 1 }));
    assert!(e.contains("unknown argument"), "{e}");
    let e = h.err("todo_search_tasks", json!({ "timezone": "Mars/Olympus" }));
    assert!(e.contains("unknown timezone \"Mars/Olympus\""), "{e}");
    assert_eq!(h.graph_requests(), 0);
}

#[test]
fn search_filters_are_client_side_and_never_reach_a_url() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.add_task_full(&l, json!({ "title": "Call Anna", "importance": "high", "categories": ["Work"], "body": {"content": "<p>about the <b>budget</b></p>", "contentType": "html"} }), None);
    h.fx.add_task(&l, "Water plants", Some("2026-08-26"), "notStarted");
    let r = h.ok(
        "todo_search_tasks",
        json!({ "query": "budget anna", "importance": "high", "categories": ["work"] }),
    );
    assert_eq!(r["total_matched"], 1);
    assert_eq!(r["tasks"][0]["title"], "Call Anna");
    let r = h.ok("todo_search_tasks", json!({ "due": "tomorrow" }));
    assert_eq!(r["total_matched"], 1);
    assert_eq!(r["tasks"][0]["title"], "Water plants");
    for req in h.fx.requests() {
        assert!(
            !req.path.contains("$filter")
                && !req.path.contains("$orderby")
                && !req.path.contains("$search"),
            "{}",
            req.path
        );
        assert!(
            req.path.contains("$top=100")
                || !req.path.contains("/tasks")
                || req.path.contains("$batch"),
            "{}",
            req.path
        );
    }
}

#[test]
fn agenda_buckets_differ_by_timezone_and_unresolved_zones_land_nowhere() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let flagged = h.fx.add_list("Flagged email", Some("flaggedEmails"));
    // Stored as 22:00Z on the 24th = the 25th in Prague, the 24th in LA.
    h.fx.add_task_full(
        &l,
        json!({ "title": "edge" }),
        Some(("2026-08-24T22:00:00.0000000".into(), "UTC".into())),
    );
    h.fx.add_task(&l, "old", Some("2026-08-01"), "notStarted");
    h.fx.add_task(&l, "soon", Some("2026-08-28"), "notStarted");
    h.fx.add_task(&l, "far", Some("2026-12-01"), "notStarted");
    h.fx.add_task(&l, "someday", None, "notStarted");
    h.fx.add_task_full(
        &l,
        json!({ "title": "weird" }),
        Some((
            "2026-08-25T00:00:00".into(),
            "tzone://Microsoft/Custom".into(),
        )),
    );
    h.fx.add_task_full(&l, json!({ "title": "done", "status": "completed", "completedDateTime": {"dateTime": "2026-08-25T08:00:00.0000000", "timeZone": "UTC"} }), None);
    h.fx.add_task(&flagged, "mail", None, "notStarted");

    let prague = h.ok("todo_agenda", json!({}));
    assert_eq!(prague["today"], "2026-08-25");
    assert_eq!(prague["complete"], true);
    let titles = |v: &serde_json::Value, s: &str| -> Vec<String> {
        v["sections"][s]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["title"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(titles(&prague, "due_today"), ["edge"]);
    assert_eq!(titles(&prague, "overdue"), ["old"]);
    assert_eq!(titles(&prague, "due_soon"), ["soon"]);
    assert_eq!(titles(&prague, "no_due_date"), ["someday"]);
    assert_eq!(titles(&prague, "flagged_emails"), ["mail"]);
    assert_eq!(titles(&prague, "recently_completed"), ["done"]);
    // "weird" appears in no section, but is listed by search with due_unresolved.
    let all: Vec<String> = ["overdue", "due_today", "due_soon", "no_due_date"]
        .iter()
        .flat_map(|s| titles(&prague, s))
        .collect();
    assert!(!all.contains(&"weird".to_string()));
    let s = h.ok("todo_search_tasks", json!({ "query": "weird" }));
    assert_eq!(s["tasks"][0]["due"], serde_json::Value::Null);
    assert_eq!(s["tasks"][0]["due_unresolved"], "tzone://Microsoft/Custom");

    let la = h.ok("todo_agenda", json!({ "timezone": "America/Los_Angeles" }));
    assert_eq!(la["today"], "2026-08-25");
    assert_eq!(
        titles(&la, "overdue"),
        ["old", "edge"],
        "edge is the 24th in LA"
    );
    assert!(titles(&la, "due_today").is_empty());
    // Warm agenda: zero Graph requests.
    let n = h.graph_requests();
    h.ok(
        "todo_agenda",
        json!({ "horizon_days": 3, "max_per_section": 1 }),
    );
    assert_eq!(h.graph_requests(), n);
}

#[test]
fn year_boundary_in_both_zones() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    // Stored as 2027-01-01T00:00Z: 01:00 on the 1st in Prague, the 1st in UTC.
    h.fx.add_task_full(
        &l,
        json!({ "title": "nye" }),
        Some(("2027-01-01T00:00:00.0000000".into(), "UTC".into())),
    );
    h.clock
        .set(Utc.with_ymd_and_hms(2026, 12, 31, 23, 30, 0).unwrap());
    let prague = h.ok("todo_agenda", json!({}));
    assert_eq!(prague["today"], "2027-01-01");
    assert_eq!(prague["sections"]["due_today"]["count"], 1);
    let utc = h.ok("todo_agenda", json!({ "timezone": "UTC" }));
    assert_eq!(utc["today"], "2026-12-31");
    assert_eq!(utc["sections"]["due_soon"]["count"], 1);
    assert_eq!(utc["sections"]["due_today"]["count"], 0);
}

#[test]
fn get_task_returns_full_fidelity_in_two_requests_when_warm() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let big = "x".repeat(9000);
    let t = h.fx.add_task_full(&l, json!({ "title": "big", "body": {"content": big, "contentType": "text"}, "recurrence": {"pattern": {"type": "daily", "interval": 1}, "range": {"type": "noEnd", "startDate": "2026-08-25"}} }), None);
    h.fx.store.lock().unwrap().checklist.insert(
        t.clone(),
        vec![
            json!({"id": "C1", "displayName": "one", "isChecked": false}),
            json!({"id": "C2", "displayName": "two", "isChecked": true}),
        ],
    );
    h.ok("todo_search_tasks", json!({}));
    let before = h.graph_requests();
    let r = h.ok("todo_get_task", json!({ "task_id": t, "list": "tasks" }));
    assert_eq!(h.graph_requests() - before, 2);
    assert_eq!(r["task"]["is_recurring"], true);
    assert_eq!(r["task"]["checklist_open"], 1);
    assert_eq!(r["task"]["checklist_total"], 2);
    assert_eq!(r["body_truncated"], true);
    assert_eq!(r["body_bytes_total"], 9000);
    assert_eq!(r["body"].as_str().unwrap().len(), 8192);
    assert!(r["web_link"].is_null());
    assert_eq!(r["checklist_items"].as_array().unwrap().len(), 2);
    // The cached preview kept the byte total from before ingest truncation.
    let s = h.ok("todo_search_tasks", json!({}));
    assert_eq!(s["tasks"][0]["body_bytes_total"], 9000);
    assert_eq!(s["tasks"][0]["body_truncated"], true);
    // Unknown id without a list: a sync then a clear error.
    let e = h.err("todo_get_task", json!({ "task_id": "nope=" }));
    assert!(e.contains("not found"), "{e}");
}

#[test]
fn account_status_makes_no_me_call_and_reports_the_grant() {
    let h = harness(&[]);
    let r = h.ok("todo_account_status", json!({}));
    assert_eq!(r["account_id"], "default");
    assert_eq!(r["identity"], "not requested (no openid scope)");
    let login = r["login_command"].as_str().unwrap_or_default();
    assert!(login.contains("`todo-mcp login`"), "{r}");
    assert!(
        login.contains("`docker compose run --rm todo-mcp login`"),
        "{r}"
    );
    assert_eq!(r["write_tools_enabled"], true);
    assert_eq!(r["restart_required"], false);
    assert_eq!(h.graph_requests(), 0);
    let r = h.ok("todo_account_status", json!({ "check_connectivity": true }));
    assert_eq!(r["connectivity"]["ok"], true);
    assert_eq!(r["grant"], "Tasks.ReadWrite");
    assert_eq!(
        r["scopes_granted"],
        json!(["Tasks.ReadWrite", "offline_access"])
    );
    assert_eq!(r["timezone"], "Europe/Prague");
    let reqs = h.fx.requests();
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0].path.contains("/me/todo/lists?$top=1"),
        "{}",
        reqs[0].path
    );
    assert!(
        !reqs
            .iter()
            .any(|r| r.path == "/v1.0/me" || r.path.starts_with("/v1.0/me?"))
    );
    let text = r.to_string();
    assert!(!text.contains("AT-"), "token material leaked: {text}");
    assert!(!text.contains("RT-"), "token material leaked: {text}");
}

#[test]
fn partial_sync_reports_coverage_and_converges() {
    let h = harness(&[("TODO_MCP_MAX_PAGES", "3")]);
    h.fx.set_page_size(2);
    let a = h.fx.add_list("A", Some("defaultList"));
    let b = h.fx.add_list("B", None);
    for i in 0..5 {
        h.fx.add_task(&a, &format!("a{i}"), None, "notStarted");
        h.fx.add_task(&b, &format!("b{i}"), None, "notStarted");
    }
    let first = h.ok("todo_search_tasks", json!({ "limit": 200 }));
    assert_eq!(first["complete"], false, "{first}");
    assert_ne!(first["coverage"]["stopped_because"], "complete");
    assert!(
        first["coverage"]["banner"]
            .as_str()
            .unwrap()
            .starts_with("PARTIAL RESULT")
    );
    assert!(first["coverage"]["retry_hint"].is_string());
    let mut r = first;
    for _ in 0..4 {
        if r["complete"] == true {
            break;
        }
        r = h.ok("todo_search_tasks", json!({ "limit": 200 }));
    }
    assert_eq!(r["complete"], true, "did not converge: {}", r["coverage"]);
    assert_eq!(r["total_matched"], 10);
    assert!(r["coverage"]["banner"].is_null());
}

// ---------------------------------------------------------------------------
// Sync results are built from what the call holds, never re-read from the cache.

fn titles(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn ttl_zero_returns_complete_results_and_keeps_nothing_between_calls() {
    let h = harness(&[("TODO_MCP_CACHE_TTL_SECONDS", "0")]);
    let d = h.fx.add_list("Tasks", Some("defaultList"));
    let w = h.fx.add_list("Work", None);
    h.fx.add_task(&d, "alpha", None, "notStarted");
    h.fx.add_task(&d, "done", None, "completed");
    let c =
        h.fx.add_task(&w, "charlie", Some("2026-08-24"), "notStarted");
    h.fx.add_task(&w, "delta", Some("2026-08-28"), "notStarted");

    // 1. The all-list search is complete although the cache stores nothing.
    let r = h.ok("todo_search_tasks", json!({}));
    assert_eq!(r["complete"], true, "{r}");
    assert_eq!(r["total_matched"], 3, "{r}");
    assert_eq!(titles(&r["tasks"]), ["charlie", "delta", "alpha"], "{r}");
    assert_eq!(r["coverage"]["lists_total"], 2, "{r}");
    assert_eq!(r["coverage"]["lists_missing"], json!([]), "{r}");
    assert_eq!(r["coverage"]["stopped_because"], "complete", "{r}");
    assert!(r["coverage"]["banner"].is_null(), "{r}");
    // 2.
    let any = h.ok("todo_search_tasks", json!({ "status": "any" }));
    assert_eq!(any["total_matched"], 4, "{any}");
    // 3. The pinned clock is 2026-08-25 in Europe/Prague.
    let a = h.ok("todo_agenda", json!({}));
    assert_eq!(a["complete"], true, "{a}");
    assert_eq!(
        titles(&a["sections"]["overdue"]["tasks"]),
        ["charlie"],
        "{a}"
    );
    assert_eq!(
        titles(&a["sections"]["due_soon"]["tasks"]),
        ["delta"],
        "{a}"
    );
    assert_eq!(
        titles(&a["sections"]["no_due_date"]["tasks"]),
        ["alpha"],
        "{a}"
    );
    // 4. Nothing kept but the mailbox label cursors bind to.
    {
        let s = h.state.cache_read().stats(Instant::now());
        assert_eq!(s.tasks_cached, 0);
        assert_eq!(s.lists_cached, 0);
        assert!(!s.catalogue_cached);
        assert!(s.mailbox_id.starts_with("mbx_"), "{}", s.mailbox_id);
        assert_ne!(s.mailbox_id, "mbx_pending");
    }
    // 5. Every call re-reads: catalogue + one $batch.
    let n = h.graph_requests();
    h.ok("todo_search_tasks", json!({}));
    assert_eq!(h.graph_requests() - n, 2);
    // 6. Cursors still work.
    let p = h.ok("todo_search_tasks", json!({ "limit": 2 }));
    let cursor = p["next_cursor"]
        .as_str()
        .expect("a next_cursor")
        .to_string();
    let q = h.ok("todo_search_tasks", json!({ "cursor": cursor }));
    assert_eq!(q["returned"], 1, "{q}");
    // 7. get_task without list finds the task in what its sync returned.
    let g = h.ok("todo_get_task", json!({ "task_id": c }));
    assert_eq!(g["task"]["title"], "charlie", "{g}");
    assert_eq!(g["task"]["list_name"], "Work", "{g}");
    // 8.
    let l = h.ok("todo_lists", json!({}));
    assert_eq!(l["counts_from_cache"], false, "{l}");
    assert!(l["lists"][0]["open_count"].is_null(), "{l}");
    // 9. A write without list resolves the id from its own sync.
    let done = h.ok("todo_complete_tasks", json!({ "task_ids": [c] }));
    assert_eq!(done["changed"].as_array().unwrap().len(), 1, "{done}");
}

#[test]
fn a_partial_result_at_ttl_zero_never_claims_the_cache_kept_anything() {
    let off = [
        ("TODO_MCP_CACHE_TTL_SECONDS", "0"),
        ("TODO_MCP_MAX_PAGES", "2"),
    ];
    let h = harness(&off);
    let s = seed_22_lists(&h.fx);
    let r = h.ok("todo_search_tasks", json!({}));
    assert_eq!(r["complete"], false, "{r}");
    assert_eq!(r["coverage"]["stopped_because"], "request_budget", "{r}");
    let hint = r["coverage"]["retry_hint"].as_str().unwrap();
    assert!(!hint.contains("cache kept"), "{hint}");
    assert!(hint.contains("TODO_MCP_CACHE_TTL_SECONDS=0"), "{hint}");
    let banner = r["coverage"]["banner"].as_str().unwrap();
    assert!(banner.starts_with("PARTIAL RESULT"), "{banner}");
    assert!(!banner.contains("Call again"), "{banner}");
    // The write lookup and get_task follow the same rule.
    let w = h.ok("todo_complete_tasks", json!({ "task_ids": [s.far] }));
    assert_eq!(w["failed"][0]["error_code"], "sync_incomplete", "{w}");
    let msg = w["failed"][0]["message"].as_str().unwrap();
    assert!(msg.contains("TODO_MCP_CACHE_TTL_SECONDS=0"), "{msg}");
    assert!(!msg.contains("are cached"), "{msg}");
    let e = h.err("todo_get_task", json!({ "task_id": s.far }));
    assert!(e.contains("TODO_MCP_CACHE_TTL_SECONDS=0"), "{e}");
    assert!(!e.contains("are cached"), "{e}");

    // With the cache on, the same partial result does promise to resume.
    let h = harness(&[("TODO_MCP_MAX_PAGES", "2")]);
    seed_22_lists(&h.fx);
    let r = h.ok("todo_search_tasks", json!({}));
    assert_eq!(r["complete"], false, "{r}");
    let hint = r["coverage"]["retry_hint"].as_str().unwrap();
    assert!(hint.contains("cache kept"), "{hint}");
}

#[test]
fn all_list_search_is_complete_even_when_the_cache_evicts_lists() {
    let h = harness(&[("TODO_MCP_CACHE_MAX_TASKS", "100")]);
    for (name, well) in [("Tasks", Some("defaultList")), ("B", None), ("C", None)] {
        let l = h.fx.add_list(name, well);
        for i in 0..60 {
            h.fx.add_task(&l, &format!("{name}{i:02}"), None, "notStarted");
        }
    }
    let r = h.ok("todo_search_tasks", json!({ "limit": 200 }));
    assert_eq!(r["complete"], true, "{}", r["coverage"]);
    assert_eq!(r["total_matched"], 180, "{}", r["coverage"]);
    assert_eq!(r["returned"], 180, "{}", r["coverage"]);
    assert_eq!(
        r["coverage"]["lists_missing"],
        json!([]),
        "{}",
        r["coverage"]
    );
    assert_eq!(h.graph_requests(), 2);
    assert_eq!(
        h.state.cache_read().total_tasks(),
        60,
        "eviction happened while the result stayed complete"
    );
}

#[test]
fn get_task_errors_when_its_checklist_or_attachments_cannot_be_read_completely() {
    // Catalogue + one $batch + the task GET leave nothing for the checklist.
    let h = harness(&[("TODO_MCP_MAX_PAGES", "3")]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let t = h.fx.add_task(&l, "t", None, "notStarted");
    let e = h.err("todo_get_task", json!({ "task_id": t }));
    assert!(e.contains("budget") && e.contains("checklist"), "{e}");
    assert!(e.contains("Pass \"list\""), "{e}");
    assert!(e.contains("call again"), "{e}");
    assert!(!e.contains("cache is disabled"), "{e}");
    assert_eq!(h.fx.count_matching("GET", "/checklistItems"), 0);

    // At TTL 0 calling again repeats the same lookup, and the text says so.
    let h = harness(&[
        ("TODO_MCP_MAX_PAGES", "3"),
        ("TODO_MCP_CACHE_TTL_SECONDS", "0"),
    ]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let t = h.fx.add_task(&l, "t", None, "notStarted");
    let e = h.err("todo_get_task", json!({ "task_id": t }));
    assert!(e.contains("budget") && e.contains("checklist"), "{e}");
    assert!(e.contains("cache is disabled"), "{e}");
    assert!(e.contains("repeats the same lookup"), "{e}");
    assert_eq!(h.fx.count_matching("GET", "/checklistItems"), 0);

    // With `list` passed no lookup ran: catalogue + the task GET leave nothing,
    // and passing list is not offered as the fix.
    let h = harness(&[("TODO_MCP_MAX_PAGES", "2")]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let t = h.fx.add_task(&l, "t", None, "notStarted");
    let e = h.err("todo_get_task", json!({ "task_id": t, "list": "Tasks" }));
    assert!(e.contains("budget") && e.contains("checklist"), "{e}");
    assert!(!e.contains("Pass \"list\""), "{e}");
    assert!(e.contains("TODO_MCP_MAX_PAGES"), "{e}");
    assert_eq!(h.fx.count_matching("GET", "/checklistItems"), 0);

    // One more request reaches the checklist; the attachments then get none.
    let h = harness(&[("TODO_MCP_MAX_PAGES", "4")]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let t =
        h.fx.add_task_full(&l, json!({ "title": "t", "hasAttachments": true }), None);
    let e = h.err("todo_get_task", json!({ "task_id": t }));
    assert!(e.contains("budget") && e.contains("attachment"), "{e}");

    // With room for every read the same call succeeds.
    let h = harness(&[("TODO_MCP_MAX_PAGES", "5")]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let t =
        h.fx.add_task_full(&l, json!({ "title": "t", "hasAttachments": true }), None);
    let g = h.ok("todo_get_task", json!({ "task_id": t }));
    assert_eq!(g["attachments_total"], 0, "{g}");
}
