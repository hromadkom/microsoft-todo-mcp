//! Write tools, guards and scope-derived registration over the fixture.

mod common;

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{
    Harness, Override, harness, harness_unsigned, harness_with_grant, seed_22_lists,
    write_token_file,
};
use microsoft_todo_mcp::auth::Grant;
use microsoft_todo_mcp::errors::{LOGIN_HINT, RESTART_HINT};
use microsoft_todo_mcp::mcp::ToolProvider;
use microsoft_todo_mcp::tools::guards;

#[test]
fn tools_list_is_ten_five_five_by_grant() {
    for (grant, n) in [
        (Grant::ReadWrite, 10),
        (Grant::ReadOnly, 5),
        (Grant::None, 5),
    ] {
        let h = harness_with_grant(&[], grant);
        let tools = h.state.list_tools();
        assert_eq!(tools.as_array().unwrap().len(), n, "{grant:?}");
        if n == 5 {
            for t in tools.as_array().unwrap() {
                assert_eq!(t["annotations"]["readOnlyHint"], true);
            }
            // A write tool called anyway is refused, never a protocol error.
            let r = h.call("todo_create_tasks", json!({ "tasks": [{ "title": "x" }] }));
            assert_eq!(r["isError"], true);
            assert!(
                r["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("Tasks.ReadWrite")
            );
        }
    }
}

#[test]
fn a_write_under_a_read_config_names_the_setting_not_a_restart() {
    let h = harness_with_grant(&[("TODO_MCP_SCOPE", "Tasks.Read")], Grant::ReadOnly);
    let r = h.call("todo_create_tasks", json!({ "tasks": [{ "title": "x" }] }));
    assert_eq!(r["isError"], true, "{r}");
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("TODO_MCP_SCOPE=Tasks.Read"), "{text}");
    assert!(text.contains("restarting will not change that"), "{text}");
    assert!(text.contains("TODO_MCP_SCOPE=Tasks.ReadWrite"), "{text}");
    assert!(!text.contains(LOGIN_HINT), "{text}");
    assert!(!text.contains(RESTART_HINT), "{text}");
    assert_eq!(h.graph_requests(), 0);

    // Under the default (ReadWrite) config a narrower grant still says sign in
    // again and restart, in the runnable host and compose forms.
    let h = harness_with_grant(&[], Grant::ReadOnly);
    let r = h.call("todo_create_tasks", json!({ "tasks": [{ "title": "x" }] }));
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.contains(LOGIN_HINT), "{text}");
    assert!(text.contains(RESTART_HINT), "{text}");
    assert_eq!(h.graph_requests(), 0);
}

#[test]
fn first_write_after_a_readonly_login_refuses_before_graph() {
    let h = harness_unsigned(&[], "https://graph.microsoft.com/Tasks.Read offline_access");
    write_token_file(
        &h.dir,
        "RT-OLD",
        "https://graph.microsoft.com/Tasks.ReadWrite offline_access",
    );
    let r = h.call("todo_create_tasks", json!({ "tasks": [{ "title": "x" }] }));
    assert_eq!(r["isError"], true, "{r}");
    assert!(
        r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("current Microsoft Graph grant is `Tasks.Read`"),
        "{r}"
    );
    assert_eq!(h.graph_requests(), 0);
    assert_eq!(h.fx.count_matching("POST", "/tasks"), 0);
}

#[test]
fn five_groceries_is_one_call_and_five_posts() {
    let h = harness(&[]);
    let l = h.fx.add_list("Shopping", None);
    h.fx.add_list("Tasks", Some("defaultList"));
    let r = h.ok("todo_create_tasks", json!({
        "list": "shopping",
        "tasks": [
            { "title": "bread" }, { "title": "milk", "due_date": "2026-08-26" },
            { "title": "eggs", "checklist": ["brown", "white"] }, { "title": "coffee", "importance": "high" },
            { "title": "rice", "body": "long grain <basmati>" }
        ]
    }));
    assert_eq!(r["created"].as_array().unwrap().len(), 5, "{r}");
    assert!(r["failed"].as_array().unwrap().is_empty());
    assert_eq!(
        h.fx.count_matching("POST", &format!("/me/todo/lists/{l}/tasks")),
        5 + 2
    );
    let posts: Vec<_> =
        h.fx.requests()
            .into_iter()
            .filter(|q| q.method == "POST" && q.path.ends_with("/tasks"))
            .collect();
    let milk: serde_json::Value = serde_json::from_str(&posts[1].body).unwrap();
    assert_eq!(
        milk["dueDateTime"],
        json!({ "dateTime": "2026-08-26T00:00:00.0000000", "timeZone": "Central Europe Standard Time" })
    );
    let rice: serde_json::Value = serde_json::from_str(&posts[4].body).unwrap();
    assert_eq!(
        rice["body"],
        json!({ "content": "long grain &lt;basmati&gt;", "contentType": "html" })
    );
    assert_eq!(r["created"][1]["due"], "2026-08-26");
    assert_eq!(r["created"][1]["list_name"], "Shopping");
}

#[test]
fn one_failure_lands_in_failed_and_a_checklist_failure_is_a_warning() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.push_override(common::Override {
        path_contains: format!("/lists/{l}/tasks"),
        method: Some("POST".into()),
        status: 400,
        headers: vec![],
        body: json!({"error": {"code": "InvalidRequest", "message": "nope"}}).to_string(),
        remaining: 1,
    });
    let r = h.ok(
        "todo_create_tasks",
        json!({ "tasks": [{ "title": "bad" }, { "title": "good", "checklist": ["c"] }] }),
    );
    assert_eq!(r["failed"].as_array().unwrap().len(), 1);
    assert_eq!(r["failed"][0]["index"], 0);
    assert_eq!(r["failed"][0]["error_code"], "graph");
    assert_eq!(r["created"].as_array().unwrap().len(), 1);
    // Now a checklist failure.
    h.fx.push_override(common::Override {
        path_contains: "/checklistItems".into(),
        method: Some("POST".into()),
        status: 500,
        headers: vec![],
        body: String::new(),
        remaining: 1,
    });
    let r = h.ok(
        "todo_create_tasks",
        json!({ "tasks": [{ "title": "with list", "checklist": ["c1"] }] }),
    );
    assert_eq!(r["created"].as_array().unwrap().len(), 1);
    assert_eq!(r["warnings"].as_array().unwrap().len(), 1);
    assert!(r["warnings"][0].as_str().unwrap().contains("c1"));
}

#[test]
fn delete_guards_refuse_with_zero_requests() {
    let h = harness(&[]);
    let flagged = h.fx.add_list("Flagged email", Some("flaggedEmails"));
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let f = h.fx.add_task(&flagged, "mail", None, "notStarted");
    let t = h.fx.add_task(&l, "normal", None, "notStarted");
    // G2 with confirm:false and absent.
    assert_eq!(
        h.err(
            "todo_delete_tasks",
            json!({ "task_ids": [t], "confirm": false })
        ),
        guards::REFUSE_DELETE_CONFIRM
    );
    assert_eq!(
        h.err("todo_delete_tasks", json!({ "task_ids": [t] })),
        guards::REFUSE_DELETE_CONFIRM
    );
    // G4.
    let many: Vec<String> = (0..51).map(|i| format!("id{i}")).collect();
    assert!(
        h.err(
            "todo_delete_tasks",
            json!({ "task_ids": many, "confirm": true })
        )
        .contains("at most 50")
    );
    assert_eq!(h.graph_requests(), 0);
    // G1: warm the catalogue, reset, then refuse with zero requests.
    h.ok("todo_search_tasks", json!({}));
    h.fx.reset_log();
    let r = h.ok(
        "todo_delete_tasks",
        json!({ "task_ids": [f], "confirm": true }),
    );
    assert_eq!(r["refused"].as_array().unwrap().len(), 1);
    assert!(
        r["refused"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("Flagged emails")
    );
    assert_eq!(h.graph_requests(), 0);
    // Update and complete on flaggedEmails succeed.
    let r = h.ok("todo_complete_tasks", json!({ "task_ids": [f] }));
    assert_eq!(r["changed"].as_array().unwrap().len(), 1);
    // A real delete, then a repeat is already_absent.
    let r = h.ok(
        "todo_delete_tasks",
        json!({ "task_ids": [t], "confirm": true }),
    );
    assert_eq!(r["deleted"].as_array().unwrap().len(), 1);
    let r = h.ok(
        "todo_delete_tasks",
        json!({ "task_ids": [t], "confirm": true, "list": "tasks" }),
    );
    assert_eq!(r["already_absent"], json!([t]));
    assert!(guards::is_protected(Some("unknownFutureValue")));
}

#[test]
fn update_conflicts_are_refused_pre_flight_and_clears_go_on_the_wire() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let t = h.fx.add_task(&l, "thing", Some("2026-08-30"), "notStarted");
    let e = h.err(
        "todo_update_tasks",
        json!({ "updates": [{ "task_id": t, "due_date": "2026-09-01", "clear_due_date": true }] }),
    );
    assert!(e.contains("mutually exclusive"), "{e}");
    assert_eq!(h.graph_requests(), 0);
    let r = h.ok("todo_update_tasks", json!({ "updates": [{
        "task_id": t, "list": "tasks", "title": "renamed", "clear_due_date": true, "clear_reminder": true,
        "clear_categories": true, "clear_body": true, "clear_recurrence": true, "importance": "high"
    }] }));
    assert_eq!(r["updated"][0]["title"], "renamed");
    assert_eq!(
        r["updated"][0]["fields_changed"],
        json!(["title", "importance"])
    );
    assert_eq!(
        r["updated"][0]["cleared"],
        json!(["body", "due_date", "reminder", "categories", "recurrence"])
    );
    let patch =
        h.fx.requests()
            .into_iter()
            .find(|q| q.method == "PATCH")
            .unwrap();
    let body: serde_json::Value = serde_json::from_str(&patch.body).unwrap();
    assert_eq!(
        body,
        json!({
            "title": "renamed", "dueDateTime": null, "reminderDateTime": null, "isReminderOn": false,
            "importance": "high", "categories": [], "body": {"content": "", "contentType": "html"}, "recurrence": null
        })
    );
    // The fixture applied the null: due date is gone on re-read.
    let s = h.ok("todo_search_tasks", json!({}));
    assert!(s["tasks"][0]["due"].is_null());
    assert_eq!(s["tasks"][0]["title"], "renamed");
}

#[test]
fn complete_and_reopen_bodies() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let t = h.fx.add_task(&l, "thing", None, "notStarted");
    let r = h.ok("todo_complete_tasks", json!({ "task_ids": [t] }));
    assert_eq!(r["changed"][0]["status"], "completed");
    let r = h.ok("todo_complete_tasks", json!({ "task_ids": [t] }));
    assert_eq!(r["unchanged"][0]["reason"], "already completed");
    let r = h.ok(
        "todo_complete_tasks",
        json!({ "task_ids": [t], "completed": false }),
    );
    assert_eq!(r["changed"][0]["status"], "notStarted");
    let patches: Vec<serde_json::Value> =
        h.fx.requests()
            .into_iter()
            .filter(|q| q.method == "PATCH")
            .map(|q| serde_json::from_str(&q.body).unwrap())
            .collect();
    assert_eq!(patches[0], json!({ "status": "completed" }));
    assert_eq!(
        patches[1],
        json!({ "status": "notStarted", "completedDateTime": null })
    );
}

#[test]
fn checklist_ops_apply_in_fixed_order_with_confirm_for_remove() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let t = h.fx.add_task(&l, "party", None, "notStarted");
    h.fx.store.lock().unwrap().checklist.insert(
        t.clone(),
        vec![
            json!({"id": "C1", "displayName": "cake", "isChecked": false}),
            json!({"id": "C2", "displayName": "Balloons", "isChecked": true}),
            json!({"id": "C3", "displayName": "dup", "isChecked": false}),
            json!({"id": "C4", "displayName": "DUP", "isChecked": false}),
        ],
    );
    assert_eq!(
        h.err(
            "todo_manage_checklist",
            json!({ "task_id": t, "remove": ["cake"] })
        ),
        guards::REFUSE_CHECKLIST_REMOVE_CONFIRM
    );
    assert_eq!(h.graph_requests(), 0);
    let r = h.ok("todo_manage_checklist", json!({
        "task_id": t, "list": "tasks", "remove": ["cake"], "confirm": true, "rename": [{"from": "balloons", "to": "Balloons (20)"}],
        "uncheck": ["balloons (20)"], "check": ["dup"], "add": ["candles"]
    }));
    assert_eq!(r["removed"], json!(["cake"]));
    assert_eq!(r["renamed"], json!(["Balloons → Balloons (20)"]));
    assert_eq!(r["unchecked"], json!(["Balloons (20)"]));
    assert_eq!(r["added"], json!(["candles"]));
    assert_eq!(r["refused"][0]["op"], "check");
    assert_eq!(r["refused"][0]["reason"], "ambiguous_name");
    assert_eq!(r["checklist_after"].as_array().unwrap().len(), 4);
    let methods: Vec<String> =
        h.fx.requests()
            .into_iter()
            .filter(|q| q.path.contains("checklistItems"))
            .map(|q| q.method)
            .collect();
    assert_eq!(methods, ["GET", "DELETE", "PATCH", "PATCH", "POST"]);
}

#[test]
fn write_outcomes_invalidate_the_list_so_the_next_read_refetches() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.add_task(&l, "a", None, "notStarted");
    h.ok("todo_search_tasks", json!({}));
    let n = h.graph_requests();
    h.ok("todo_create_tasks", json!({ "tasks": [{ "title": "b" }] }));
    let r = h.ok("todo_search_tasks", json!({ "sort": "title_asc" }));
    assert_eq!(r["total_matched"], 2);
    assert!(
        h.graph_requests() > n + 1,
        "create must invalidate, not update, the cache"
    );
}

#[test]
fn unwritable_zone_refuses_dated_writes_with_a_real_suggestion() {
    let h = harness(&[("TODO_MCP_TZ", "Antarctica/Troll")]);
    h.fx.add_list("Tasks", Some("defaultList"));
    let e = h.err(
        "todo_create_tasks",
        json!({ "tasks": [{ "title": "x", "due_date": "2026-09-01" }] }),
    );
    assert!(
        e.starts_with("Cannot write a date in time zone \"Antarctica/Troll\""),
        "{e}"
    );
    assert_eq!(h.graph_requests(), 0);
    // Undated writes still work under that zone.
    let r = h.ok("todo_create_tasks", json!({ "tasks": [{ "title": "x" }] }));
    assert_eq!(r["created"].as_array().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Write batches: resolve once, then mutate on a budget derived from the work.

fn arr(v: &Value) -> &Vec<Value> {
    v.as_array().expect("array")
}

#[test]
fn create_25_tasks_with_20_checklist_items_each_is_not_capped_by_max_pages() {
    let h = harness(&[]);
    h.fx.add_list("Groceries", None);
    h.fx.add_list("Tasks", Some("defaultList"));
    let tasks: Vec<Value> = (0..25)
        .map(|i| {
            json!({
                "title": format!("t{i:02}"),
                "checklist": (0..20).map(|j| format!("c{j:02}")).collect::<Vec<_>>(),
            })
        })
        .collect();
    let r = h.ok(
        "todo_create_tasks",
        json!({ "list": "groceries", "tasks": tasks }),
    );
    assert_eq!(arr(&r["created"]).len(), 25, "{r}");
    assert!(arr(&r["failed"]).is_empty(), "{r}");
    assert!(arr(&r["warnings"]).is_empty(), "{r}");
    let reqs = h.fx.requests();
    let posts = |suffix: &str| {
        reqs.iter()
            .filter(|q| q.method == "POST" && q.path.ends_with(suffix))
            .count()
    };
    assert_eq!(posts("/tasks"), 25);
    assert_eq!(posts("/checklistItems"), 500);
    assert_eq!(h.fx.count_matching("GET", "/me/todo/lists"), 1);
    assert_eq!(
        h.graph_requests(),
        526,
        "more than 10x TODO_MCP_MAX_PAGES, none of it refused"
    );
    let store = h.fx.store.lock().unwrap();
    for c in arr(&r["created"]) {
        let id = c["task_id"].as_str().unwrap();
        assert_eq!(store.checklist[id].len(), 20, "{id}");
    }
}

#[test]
fn fifty_completes_in_one_list_fit_cold_with_list_and_resolve_once_without_it() {
    let h = harness(&[]);
    h.fx.add_list("Tasks", Some("defaultList"));
    let w = h.fx.add_list("Work", None);
    let ids: Vec<String> = (0..50)
        .map(|i| h.fx.add_task(&w, &format!("w{i:02}"), None, "notStarted"))
        .collect();

    // A: cold, with list — no lookup at all.
    let a = h.ok(
        "todo_complete_tasks",
        json!({ "task_ids": ids, "list": "work" }),
    );
    assert_eq!(arr(&a["changed"]).len(), 50, "{a}");
    assert!(arr(&a["unchanged"]).is_empty(), "{a}");
    assert!(arr(&a["failed"]).is_empty(), "{a}");
    assert_eq!(h.fx.count_matching("PATCH", "/tasks/"), 50);
    assert_eq!(h.fx.count_matching("GET", "/me/todo/lists"), 1);
    assert_eq!(h.fx.count_matching("POST", "$batch"), 0);
    assert_eq!(h.graph_requests(), 51);
    for id in &ids {
        assert_eq!(h.fx.task(&w, id).unwrap()["status"], "completed", "{id}");
    }

    // B: no list; the catalogue is warm and A invalidated Work. ONE sync for
    // all fifty ids, never a re-sync per id.
    h.fx.reset_log();
    let b = h.ok(
        "todo_complete_tasks",
        json!({ "task_ids": ids, "completed": false }),
    );
    assert_eq!(arr(&b["changed"]).len(), 50, "{b}");
    assert!(arr(&b["failed"]).is_empty(), "{b}");
    assert_eq!(h.fx.count_matching("POST", "$batch"), 1);
    assert_eq!(h.fx.count_matching("PATCH", "/tasks/"), 50);
    assert_eq!(h.graph_requests(), 51);

    // C: the same again. `unchanged` survives resolve-once: it is decided from
    // the view this call's sync fetched.
    h.fx.reset_log();
    let c = h.ok(
        "todo_complete_tasks",
        json!({ "task_ids": ids, "completed": false }),
    );
    assert_eq!(arr(&c["unchanged"]).len(), 50, "{c}");
    assert!(
        arr(&c["unchanged"])
            .iter()
            .all(|u| u["reason"] == "already open"),
        "{c}"
    );
    assert!(arr(&c["changed"]).is_empty(), "{c}");
    assert_eq!(h.fx.count_matching("PATCH", "/tasks/"), 0);
    assert_eq!(h.fx.count_matching("POST", "$batch"), 1);
    assert_eq!(h.graph_requests(), 1);
}

#[test]
fn fifty_deletes_spread_over_three_lists_without_list_sync_once() {
    let h = harness(&[]);
    let a = h.fx.add_list("Tasks", Some("defaultList"));
    let b = h.fx.add_list("Work", None);
    let c = h.fx.add_list("Home", None);
    let lists = [a.clone(), b.clone(), c.clone()];
    let ids: Vec<String> = (0..50)
        .map(|i| {
            h.fx.add_task(&lists[i % 3], &format!("d{i:02}"), None, "notStarted")
        })
        .collect();
    let r = h.ok(
        "todo_delete_tasks",
        json!({ "task_ids": ids, "confirm": true }),
    );
    let deleted = arr(&r["deleted"]);
    assert_eq!(deleted.len(), 50, "{r}");
    for (i, d) in deleted.iter().enumerate() {
        assert_eq!(d["title"], format!("d{i:02}"), "title from the sync: {r}");
    }
    assert!(arr(&r["already_absent"]).is_empty(), "{r}");
    assert!(arr(&r["refused"]).is_empty(), "{r}");
    assert!(arr(&r["failed"]).is_empty(), "{r}");
    assert_eq!(h.fx.count_matching("DELETE", "/tasks/"), 50);
    assert_eq!(h.fx.count_matching("POST", "$batch"), 1);
    assert_eq!(h.fx.count_matching("GET", "/me/todo/lists"), 1);
    assert_eq!(h.graph_requests(), 52);
    let store = h.fx.store.lock().unwrap();
    for l in &lists {
        assert!(store.tasks[l].is_empty(), "{l} still has tasks");
    }
}

#[test]
fn an_id_the_lookup_could_not_reach_is_sync_incomplete_never_list_not_found() {
    let cfg = [("TODO_MCP_MAX_PAGES", "2")];

    // Catalogue (1) + one $batch (Tasks, L01..L19) spends the read budget.
    let h = harness(&cfg);
    let s = seed_22_lists(&h.fx);
    let r = h.ok(
        "todo_complete_tasks",
        json!({ "task_ids": [s.near, s.far] }),
    );
    assert_eq!(arr(&r["changed"]).len(), 1, "{r}");
    assert_eq!(
        r["changed"][0]["task_id"], s.near,
        "the write budget is independent of the spent read budget: {r}"
    );
    let failed = arr(&r["failed"]);
    assert_eq!(failed.len(), 1, "{r}");
    assert_eq!(failed[0]["index"], 1, "{r}");
    assert_eq!(failed[0]["task_id"], s.far, "{r}");
    assert_eq!(failed[0]["error_code"], "sync_incomplete", "{r}");
    let msg = failed[0]["message"].as_str().unwrap();
    assert!(msg.contains("request_budget"), "{msg}");
    assert!(!msg.contains("not found in any list"), "{msg}");
    assert_eq!(h.fx.count_matching("PATCH", "/tasks/"), 1);
    assert_eq!(
        h.fx.task(&s.far_list, &s.far).unwrap()["status"],
        "notStarted"
    );
    // Calling again continues: L01..L19 are cached, so L20/L21 fit.
    let r = h.ok("todo_complete_tasks", json!({ "task_ids": [s.far] }));
    assert_eq!(arr(&r["changed"]).len(), 1, "{r}");

    // todo_get_task says the sync stopped early, never "not found".
    let h2 = harness(&cfg);
    let s2 = seed_22_lists(&h2.fx);
    let e = h2.err("todo_get_task", json!({ "task_id": s2.far }));
    assert!(e.contains("stopped early"), "{e}");
    assert!(e.contains("request_budget"), "{e}");
    assert!(!e.contains("not found in any list"), "{e}");

    // The same wiring in update, delete and manage_checklist: nothing is sent.
    let h3 = harness(&cfg);
    let s3 = seed_22_lists(&h3.fx);
    let r = h3.ok(
        "todo_update_tasks",
        json!({ "updates": [{ "task_id": s3.far, "title": "x" }] }),
    );
    assert_eq!(r["failed"][0]["error_code"], "sync_incomplete", "{r}");
    assert!(arr(&r["updated"]).is_empty(), "{r}");
    assert_eq!(h3.fx.count_matching("PATCH", "/me/todo/"), 0);

    let h4 = harness(&cfg);
    let s4 = seed_22_lists(&h4.fx);
    let r = h4.ok(
        "todo_delete_tasks",
        json!({ "task_ids": [s4.far], "confirm": true }),
    );
    assert_eq!(r["failed"][0]["error_code"], "sync_incomplete", "{r}");
    assert!(arr(&r["deleted"]).is_empty(), "{r}");
    assert_eq!(h4.fx.count_matching("DELETE", "/me/todo/"), 0);

    let h5 = harness(&cfg);
    let s5 = seed_22_lists(&h5.fx);
    let e = h5.err(
        "todo_manage_checklist",
        json!({ "task_id": s5.far, "add": ["a"] }),
    );
    assert!(e.contains("stopped"), "{e}");
    assert!(!e.contains("not found in any list"), "{e}");
    assert_eq!(h5.fx.count_matching("POST", "/checklistItems"), 0);
    assert_eq!(h5.fx.count_matching("PATCH", "/me/todo/"), 0);
    assert_eq!(h5.fx.count_matching("DELETE", "/me/todo/"), 0);

    // A COMPLETE lookup that lacks the id, or a list argument that does not
    // resolve, is list_not_found.
    let h6 = harness(&[]);
    h6.fx.add_list("Tasks", Some("defaultList"));
    let r = h6.ok("todo_complete_tasks", json!({ "task_ids": ["nope="] }));
    assert_eq!(r["failed"][0]["error_code"], "list_not_found", "{r}");
    let r = h6.ok(
        "todo_complete_tasks",
        json!({ "task_ids": ["nope="], "list": "no such list" }),
    );
    assert_eq!(r["failed"][0]["error_code"], "list_not_found", "{r}");
    assert!(
        r["failed"][0]["message"]
            .as_str()
            .unwrap()
            .contains("unknown list"),
        "{r}"
    );
}

#[test]
fn a_failed_batch_sub_request_is_graph_error_not_request_budget() {
    let h = harness(&[]);
    let d = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.add_task(&d, "mine", None, "notStarted");
    let shared = h.fx.add_list("Shared", None);
    let forbid = || {
        h.fx.push_override(Override {
            path_contains: format!("/lists/{shared}/tasks"),
            method: Some("GET".into()),
            status: 403,
            headers: vec![],
            body: json!({"error": {"code": "ErrorAccessDenied", "message": "Access is denied."}})
                .to_string(),
            remaining: 1,
        });
    };
    forbid();
    let r = h.ok("todo_complete_tasks", json!({ "task_ids": ["nope="] }));
    assert_eq!(r["failed"][0]["error_code"], "sync_incomplete", "{r}");
    let msg = r["failed"][0]["message"].as_str().unwrap();
    assert!(msg.contains("graph_error"), "{msg}");
    assert!(!msg.contains("request_budget"), "{msg}");
    assert_eq!(h.fx.count_matching("PATCH", "/me/todo/"), 0);

    // Read coverage carries the same stop reason.
    forbid();
    let s = h.ok("todo_search_tasks", json!({}));
    assert_eq!(s["complete"], false, "{s}");
    assert_eq!(s["coverage"]["stopped_because"], "graph_error", "{s}");
    assert_eq!(s["coverage"]["lists_missing"], json!(["Shared"]), "{s}");
    assert_eq!(s["total_matched"], 1, "{s}");
}

#[test]
fn manage_checklist_applies_one_hundred_operations_in_one_call() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let t = h.fx.add_task(&l, "party", None, "notStarted");
    let mut items = Vec::new();
    for (prefix, id, checked) in [
        ("r", "CR", false),
        ("n", "CN", false),
        ("u", "CU", true),
        ("k", "CK", false),
    ] {
        for j in 0..20 {
            items.push(json!({
                "id": format!("{id}{j:02}"), "displayName": format!("{prefix}{j:02}"), "isChecked": checked
            }));
        }
    }
    h.fx.store
        .lock()
        .unwrap()
        .checklist
        .insert(t.clone(), items);
    let names = |p: &str| (0..20).map(|j| format!("{p}{j:02}")).collect::<Vec<_>>();
    let rename: Vec<Value> = (0..20)
        .map(|j| json!({ "from": format!("n{j:02}"), "to": format!("N{j:02} renamed") }))
        .collect();
    let r = h.ok(
        "todo_manage_checklist",
        json!({
            "task_id": t, "remove": names("r"), "confirm": true, "rename": rename,
            "uncheck": names("u"), "check": names("k"), "add": names("a"),
        }),
    );
    for key in ["removed", "renamed", "unchecked", "checked", "added"] {
        assert_eq!(arr(&r[key]).len(), 20, "{key}: {r}");
    }
    assert!(arr(&r["refused"]).is_empty(), "{r}");
    assert!(arr(&r["failed"]).is_empty(), "{r}");
    assert_eq!(arr(&r["checklist_after"]).len(), 80, "{r}");
    let reqs = h.fx.requests();
    let on_items = |m: &str| {
        reqs.iter()
            .filter(|q| q.method == m && q.path.contains("checklistItems"))
            .count()
    };
    assert_eq!(on_items("DELETE"), 20);
    assert_eq!(on_items("PATCH"), 60);
    assert_eq!(on_items("POST"), 20);
    assert_eq!(on_items("GET"), 1);
    assert_eq!(
        h.graph_requests(),
        103,
        "catalogue + $batch + checklist GET + 100 changes"
    );
}

#[test]
fn an_incomplete_checklist_read_is_an_error_never_an_empty_checklist() {
    // The lookup spends the whole read budget, so the checklist GET cannot run.
    let h = harness(&[("TODO_MCP_MAX_PAGES", "2")]);
    let s = seed_22_lists(&h.fx);
    h.fx.store.lock().unwrap().checklist.insert(
        s.near.clone(),
        vec![json!({"id": "CX", "displayName": "x", "isChecked": false})],
    );
    let e = h.err(
        "todo_manage_checklist",
        json!({ "task_id": s.near, "check": ["x"], "add": ["y"] }),
    );
    assert!(e.contains("budget"), "{e}");
    assert!(e.contains("checklist"), "{e}");
    for m in ["GET", "POST", "PATCH", "DELETE"] {
        assert_eq!(h.fx.count_matching(m, "/checklistItems"), 0, "{m}");
    }
}

#[test]
fn complete_never_trusts_an_expired_cache_entry_for_unchanged() {
    let h = harness(&[("TODO_MCP_CACHE_TTL_SECONDS", "1")]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let t = h.fx.add_task(&l, "t", None, "completed");
    h.ok("todo_search_tasks", json!({ "status": "any" }));
    // TTL is measured on Instant, so the injected clock cannot expire it.
    std::thread::sleep(Duration::from_millis(1100));
    // Reopened on another device after the entry expired.
    {
        let mut store = h.fx.store.lock().unwrap();
        let task = store
            .tasks
            .get_mut(&l)
            .unwrap()
            .iter_mut()
            .find(|x| x["id"] == t.as_str())
            .unwrap();
        task["status"] = json!("notStarted");
    }
    h.fx.reset_log();
    let r = h.ok(
        "todo_complete_tasks",
        json!({ "task_ids": [t], "list": "tasks" }),
    );
    assert_eq!(arr(&r["changed"]).len(), 1, "{r}");
    assert!(arr(&r["unchanged"]).is_empty(), "{r}");
    assert_eq!(h.fx.count_matching("PATCH", "/tasks/"), 1);
    assert_eq!(h.fx.task(&l, &t).unwrap()["status"], "completed");
}

// --- deadline exhaustion: one harness per part ------------------------------

/// A 1 s tool deadline, one list of three tasks, catalogue and list warm.
fn deadline_harness() -> (Harness, Vec<String>) {
    let h = harness(&[("TODO_MCP_TOOL_DEADLINE_MS", "1000")]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    let ids = (0..3)
        .map(|i| h.fx.add_task(&l, &format!("t{i}"), None, "notStarted"))
        .collect();
    h.ok("todo_search_tasks", json!({}));
    h.fx.reset_log();
    (h, ids)
}

fn throttle_once(h: &Harness, path_contains: String, method: &str) {
    h.fx.push_override(Override {
        path_contains,
        method: Some(method.into()),
        status: 429,
        headers: vec![("Retry-After".into(), "0".into())],
        body: json!({"error": {"code": "TooManyRequests", "message": "slow down"}}).to_string(),
        remaining: 1,
    });
}

/// Run `tool` on a thread, park its retry sleep until the 1 s tool deadline has
/// certainly passed, then release it. The throttled item's retry finds the
/// deadline gone; every later item was never attempted.
fn call_past_the_deadline(h: &Harness, tool: &str, args: Value) -> Value {
    assert!(h.sleeps.lock().unwrap().is_empty());
    h.block_sleeps(true);
    let state = h.state.clone();
    let name = tool.to_string();
    let worker = std::thread::spawn(move || state.call_tool(&name, &args).expect("tool exists"));
    let started = Instant::now();
    while h.sleeps.lock().unwrap().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{tool}: the throttled request never slept"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The call's budget was stamped before this sleep began.
    std::thread::sleep(Duration::from_millis(1100));
    h.block_sleeps(false);
    let r = worker.join().expect("tool thread panicked");
    assert!(r.get("isError").is_none(), "{tool} failed: {r}");
    r["structuredContent"].clone()
}

fn assert_throttled_then_deadline(r: &Value, failed: &[Value]) {
    assert_eq!(failed.len(), 3, "{r}");
    assert_eq!(
        failed[0]["error_code"], "throttled",
        "Graph really throttled it: {r}"
    );
    for f in &failed[1..] {
        assert_eq!(f["error_code"], "deadline", "{r}");
        assert_eq!(
            f["message"], "the tool deadline elapsed before this item was attempted",
            "{r}"
        );
    }
}

#[test]
fn deadline_exhaustion_is_reported_as_deadline_not_throttled() {
    let (h, ids) = deadline_harness();
    throttle_once(&h, format!("/tasks/{}", ids[0]), "PATCH");
    let r = call_past_the_deadline(
        &h,
        "todo_complete_tasks",
        json!({ "task_ids": ids, "list": "tasks" }),
    );
    assert_throttled_then_deadline(&r, arr(&r["failed"]));
    assert!(arr(&r["changed"]).is_empty(), "{r}");
    assert_eq!(h.fx.count_matching("PATCH", "/tasks/"), 1);
}

#[test]
fn deadline_exhaustion_is_reported_as_deadline_not_throttled_update() {
    let (h, ids) = deadline_harness();
    throttle_once(&h, format!("/tasks/{}", ids[0]), "PATCH");
    let updates: Vec<Value> = ids
        .iter()
        .map(|id| json!({ "task_id": id, "list": "tasks", "title": "renamed" }))
        .collect();
    let r = call_past_the_deadline(&h, "todo_update_tasks", json!({ "updates": updates }));
    assert_throttled_then_deadline(&r, arr(&r["failed"]));
    assert!(arr(&r["updated"]).is_empty(), "{r}");
    assert_eq!(h.fx.count_matching("PATCH", "/tasks/"), 1);
}

#[test]
fn deadline_exhaustion_is_reported_as_deadline_not_throttled_delete() {
    let (h, ids) = deadline_harness();
    throttle_once(&h, format!("/tasks/{}", ids[0]), "DELETE");
    let r = call_past_the_deadline(
        &h,
        "todo_delete_tasks",
        json!({ "task_ids": ids, "confirm": true, "list": "tasks" }),
    );
    let failed = arr(&r["failed"]);
    assert_throttled_then_deadline(&r, failed);
    assert_eq!(failed[1]["title"], "t1", "{r}");
    assert_eq!(failed[2]["title"], "t2", "{r}");
    assert!(arr(&r["deleted"]).is_empty(), "{r}");
    assert_eq!(h.fx.count_matching("DELETE", "/tasks/"), 1);
}

#[test]
fn deadline_exhaustion_is_reported_as_deadline_not_throttled_create_checklist() {
    let (h, _) = deadline_harness();
    throttle_once(&h, "/checklistItems".into(), "POST");
    let r = call_past_the_deadline(
        &h,
        "todo_create_tasks",
        json!({ "tasks": [{ "title": "x", "checklist": ["c1", "c2"] }, { "title": "y" }] }),
    );
    assert_eq!(arr(&r["created"]).len(), 1, "{r}");
    assert_eq!(r["created"][0]["title"], "x", "{r}");
    let warnings = arr(&r["warnings"]);
    assert_eq!(warnings.len(), 2, "{r}");
    assert!(warnings[0].as_str().unwrap().contains("c1"), "{r}");
    let w1 = warnings[1].as_str().unwrap();
    assert!(
        w1.contains("c2") && w1.contains("not attempted") && w1.contains("deadline"),
        "{r}"
    );
    let failed = arr(&r["failed"]);
    assert_eq!(failed.len(), 1, "{r}");
    assert_eq!(failed[0]["index"], 1, "{r}");
    assert_eq!(failed[0]["error_code"], "deadline", "{r}");
    assert_eq!(h.fx.count_matching("POST", "/checklistItems"), 1);
}

#[test]
fn deadline_exhaustion_is_reported_as_deadline_not_throttled_manage_checklist() {
    let (h, ids) = deadline_harness();
    throttle_once(&h, "/checklistItems".into(), "POST");
    let r = call_past_the_deadline(
        &h,
        "todo_manage_checklist",
        json!({ "task_id": ids[1], "list": "tasks", "add": ["a1", "a2"] }),
    );
    let failed = arr(&r["failed"]);
    assert_eq!(failed.len(), 2, "{r}");
    assert_eq!(
        (
            &failed[0]["op"],
            &failed[0]["item"],
            &failed[0]["error_code"]
        ),
        (&json!("add"), &json!("a1"), &json!("throttled")),
        "{r}"
    );
    assert_eq!(
        (
            &failed[1]["op"],
            &failed[1]["item"],
            &failed[1]["error_code"]
        ),
        (&json!("add"), &json!("a2"), &json!("deadline")),
        "{r}"
    );
    assert!(arr(&r["added"]).is_empty(), "{r}");
    assert_eq!(h.fx.count_matching("POST", "/checklistItems"), 1);
}
