//! Paging, the nextLink origin check, retry/throttle, `$batch`, the
//! concurrency ceiling, and token separation — over the fixture.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::json;

use common::{Override, harness};
use microsoft_todo_mcp::errors::AppError;
use microsoft_todo_mcp::graph::tasks::tasks_url;
use microsoft_todo_mcp::graph::{Budget, Method};

#[test]
fn three_page_walk_follows_next_link_verbatim() {
    let h = harness(&[]);
    h.fx.set_page_size(100);
    let l = h.fx.add_list("Big", Some("defaultList"));
    for i in 0..250 {
        h.fx.add_task(&l, &format!("t{i:03}"), None, "notStarted");
    }
    let mut budget = Budget::for_tool(10_000, 50);
    let (tasks, complete) = h.state.graph.list_tasks(&l, &mut budget).unwrap();
    assert!(complete);
    assert_eq!(tasks.len(), 250);
    assert_eq!(tasks[0].title, "t000");
    assert_eq!(tasks[249].title, "t249");
    let reqs = h.fx.requests();
    assert_eq!(reqs.len(), 3);
    assert!(
        reqs[0].path.ends_with("/tasks?$top=100"),
        "{}",
        reqs[0].path
    );
    // The fixture's nextLink came back byte-identically (host rewritten by the test transport only).
    assert!(
        reqs[1].path.ends_with("/tasks?$top=100&$skip=100"),
        "{}",
        reqs[1].path
    );
    assert!(
        reqs[2].path.ends_with("/tasks?$top=100&$skip=200"),
        "{}",
        reqs[2].path
    );
    for r in &reqs {
        assert!(
            r.header("prefer")
                .unwrap()
                .contains("odata.maxpagesize=100")
        );
        assert!(
            r.header("prefer")
                .unwrap()
                .contains("outlook.timezone=\"Central Europe Standard Time\"")
        );
    }
    // Page cap: complete == false, no error, first two pages present.
    h.fx.reset_log();
    h.state.cache_write().invalidate_list(&l);
    let mut budget = Budget::for_tool(10_000, 2);
    let (tasks, complete) = h.state.graph.list_tasks(&l, &mut budget).unwrap();
    assert!(!complete);
    assert_eq!(tasks.len(), 200);
}

#[test]
fn off_origin_next_links_are_refused_before_any_request() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.add_task(&l, "a", None, "notStarted");
    for bad in [
        "http://graph.microsoft.com/v1.0/x",
        "https://evil.example/v1.0/x",
        "https://graph.microsoft.com.evil.example/v1.0/x",
        "https://graph.microsoft.com:8443/v1.0/x",
        "https://x@graph.microsoft.com/v1.0/x",
    ] {
        h.fx.reset_log();
        h.fx.push_override(Override {
            path_contains: "/tasks".into(),
            method: Some("GET".into()),
            status: 200,
            headers: vec![],
            body: json!({ "value": [], "@odata.nextLink": bad }).to_string(),
            remaining: 1,
        });
        let mut budget = Budget::for_tool(10_000, 50);
        let err = h.state.graph.list_tasks(&l, &mut budget).unwrap_err();
        assert_eq!(err.code(), "TRANSPORT", "{bad}");
        assert!(
            !err.message().contains("/x"),
            "leaked the URL: {}",
            err.message()
        );
        assert_eq!(h.fx.request_count(), 1, "{bad}: a second request was made");
    }
    assert_eq!(h.fx.foreign_host_hits.load(Ordering::SeqCst), 0);
    // The same check guards a $batch sub-response nextLink.
    h.fx.store.lock().unwrap().batch_next_link_override =
        Some("https://evil.example/v1.0/x".into());
    h.fx.reset_log();
    let r = h.call("todo_search_tasks", json!({}));
    // Refused as a sync failure, never fetched: partial coverage, no error.
    let cov = &r["structuredContent"]["coverage"];
    assert_eq!(cov["stopped_because"], "graph_error", "{r}");
    assert_eq!(r["structuredContent"]["complete"], false);
    assert!(h.fx.requests().iter().all(|q| !q.path.contains("evil")));
    assert_eq!(h.fx.foreign_host_hits.load(Ordering::SeqCst), 0);
}

#[test]
fn retry_after_is_honoured_exactly_with_one_retry() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.add_task(&l, "a", None, "notStarted");
    h.fx.push_override(Override {
        path_contains: "/tasks".into(),
        method: Some("GET".into()),
        status: 429,
        headers: vec![("Retry-After".into(), "2".into())],
        body: String::new(),
        remaining: 1,
    });
    let mut budget = Budget::for_tool(10_000, 50);
    let (tasks, _) = h.state.graph.list_tasks(&l, &mut budget).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(h.fx.count_matching("GET", "/tasks"), 2, "exactly one retry");
    assert_eq!(*h.sleeps.lock().unwrap(), vec![Duration::from_secs(2)]);
    let stats = h.state.graph.stats();
    assert_eq!(stats.throttled_429, 1);
    assert_eq!(stats.retries, 1);
    assert_eq!(stats.last_retry_after_secs, Some(2));
}

#[test]
fn a_retry_after_beyond_the_deadline_fails_fast() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.push_override(Override {
        path_contains: "/tasks".into(),
        method: None,
        status: 429,
        headers: vec![("Retry-After".into(), "300".into())],
        body: String::new(),
        remaining: 5,
    });
    let started = std::time::Instant::now();
    let mut budget = Budget::for_tool(25_000, 50);
    let err = h.state.graph.list_tasks(&l, &mut budget).unwrap_err();
    assert!(
        matches!(
            err,
            AppError::Throttled {
                retry_after_secs: 120
            }
        ),
        "{err:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(h.fx.count_matching("GET", "/tasks"), 1);
    assert!(h.sleeps.lock().unwrap().is_empty());
}

#[test]
fn backoff_without_a_header_and_the_idempotency_rule() {
    let h = harness(&[("TODO_MCP_MAX_ATTEMPTS", "3")]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    // 503 with no header → exponential backoff, attempts == MAX_ATTEMPTS.
    h.fx.push_override(Override {
        path_contains: "/tasks".into(),
        method: Some("GET".into()),
        status: 503,
        headers: vec![],
        body: String::new(),
        remaining: 10,
    });
    let mut budget = Budget::for_tool(60_000, 50);
    assert!(h.state.graph.list_tasks(&l, &mut budget).is_err());
    assert_eq!(h.fx.count_matching("GET", "/tasks"), 3);
    let sleeps = h.sleeps.lock().unwrap().clone();
    assert_eq!(sleeps.len(), 2);
    assert!(sleeps[0] >= Duration::from_millis(500) && sleeps[0] < Duration::from_millis(700));
    assert!(sleeps[1] >= Duration::from_millis(1000) && sleeps[1] < Duration::from_millis(1300));
    h.overrides_clear();
    // 500 on POST is NOT retried; 500 on GET is.
    h.fx.reset_log();
    h.sleeps.lock().unwrap().clear();
    h.fx.push_override(Override {
        path_contains: "/tasks".into(),
        method: Some("POST".into()),
        status: 500,
        headers: vec![],
        body: String::new(),
        remaining: 10,
    });
    let mut budget = Budget::for_tool(60_000, 50);
    assert!(
        h.state
            .graph
            .create_task(&l, json!({ "title": "x" }), &mut budget)
            .is_err()
    );
    assert_eq!(h.fx.count_matching("POST", "/tasks"), 1);
    h.overrides_clear();
    h.fx.reset_log();
    h.fx.push_override(Override {
        path_contains: "/tasks".into(),
        method: Some("GET".into()),
        status: 500,
        headers: vec![],
        body: String::new(),
        remaining: 1,
    });
    let mut budget = Budget::for_tool(60_000, 50);
    assert!(h.state.graph.list_tasks(&l, &mut budget).is_ok());
    assert_eq!(h.fx.count_matching("GET", "/tasks"), 2);
}

#[test]
fn a_sleeping_caller_does_not_hold_a_permit() {
    let h = harness(&[("TODO_MCP_GRAPH_CONCURRENCY", "1")]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.add_task(&l, "a", None, "notStarted");
    h.fx.push_override(Override {
        path_contains: "/lists?".into(),
        method: Some("GET".into()),
        status: 429,
        headers: vec![("Retry-After".into(), "1".into())],
        body: String::new(),
        remaining: 1,
    });
    // Park the sleeper: caller A will sit "asleep" on its Retry-After.
    h.block_sleeps(true);
    let a = {
        let s = h.state.clone();
        std::thread::spawn(move || {
            let mut budget = Budget::for_tool(10_000, 50);
            s.graph.list_lists(&mut budget).unwrap()
        })
    };
    while h.sleeps.lock().unwrap().is_empty() {
        std::thread::sleep(Duration::from_millis(5));
    }
    // Caller B, unrelated, must acquire the single permit while A sleeps.
    // Written the obvious way (permit held across the sleep) this deadlocks.
    let mut budget = Budget::for_tool(2_000, 50);
    let (tasks, _) = h
        .state
        .graph
        .list_tasks(&l, &mut budget)
        .expect("B must not be starved by a sleeping A");
    assert_eq!(tasks.len(), 1);
    h.block_sleeps(false);
    let (lists, _) = a.join().unwrap();
    assert_eq!(lists.len(), 1);
}

#[test]
fn observed_concurrency_never_exceeds_the_ceiling() {
    let h = harness(&[("TODO_MCP_GRAPH_CONCURRENCY", "4")]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.add_task(&l, "a", None, "notStarted");
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let s = h.state.clone();
            let url = tasks_url(&l);
            std::thread::spawn(move || {
                let mut budget = Budget::for_tool(10_000, 50);
                let req = s.graph.request(Method::Get, &url, None);
                s.graph.send(&req, &mut budget).unwrap();
            })
        })
        .collect();
    for hd in handles {
        hd.join().unwrap();
    }
    assert_eq!(h.fx.request_count(), 16);
    let peak = h.fx.peak_in_flight.load(Ordering::SeqCst);
    assert!(peak <= 4, "peak in-flight {peak} exceeded the ceiling");
    assert!(
        peak >= 2,
        "the fixture never saw overlap (peak {peak}); the test is not exercising concurrency"
    );
}

#[test]
fn the_graph_bearer_comes_from_the_token_store_never_from_mcp() {
    let h = harness(&[]);
    h.fx.add_list("Tasks", Some("defaultList"));
    let mut budget = Budget::for_tool(10_000, 50);
    h.state.graph.list_lists(&mut budget).unwrap();
    let req = &h.fx.requests()[0];
    assert_eq!(req.header("authorization"), Some("Bearer AT-1"));
    assert!(
        req.header("client-request-id")
            .is_some_and(|v| v.len() == 32)
    );
    assert_eq!(h.endpoint.calls(), 1);
    // A second call reuses the cached access token: no second refresh.
    h.state.graph.list_lists(&mut budget).unwrap();
    assert_eq!(h.endpoint.calls(), 1);
    // A 401 invalidates and re-reads once.
    h.fx.push_override(Override {
        path_contains: "/lists".into(),
        method: None,
        status: 401,
        headers: vec![],
        body: String::new(),
        remaining: 1,
    });
    h.state.graph.list_lists(&mut budget).unwrap();
    assert_eq!(h.endpoint.calls(), 2);
}

#[test]
fn batch_sub_429_is_reissued_alone_after_the_largest_retry_after() {
    let h = harness(&[]);
    let a = h.fx.add_list("A", Some("defaultList"));
    let b = h.fx.add_list("B", None);
    h.fx.add_task(&a, "a1", None, "notStarted");
    h.fx.add_task(&b, "b1", None, "notStarted");
    // Only list B's first page throttles, once, inside the batch.
    h.fx.push_override(Override {
        path_contains: format!("/lists/{b}/tasks"),
        method: Some("GET".into()),
        status: 429,
        headers: vec![("Retry-After".into(), "3".into())],
        body: String::new(),
        remaining: 1,
    });
    let r = h.ok("todo_search_tasks", json!({}));
    assert_eq!(r["complete"], true, "{r}");
    assert_eq!(r["total_matched"], 2);
    let batches: Vec<serde_json::Value> =
        h.fx.requests()
            .into_iter()
            .filter(|q| q.path.ends_with("/$batch"))
            .map(|q| serde_json::from_str(&q.body).unwrap())
            .collect();
    assert_eq!(batches.len(), 2, "one batch plus one follow-up");
    assert_eq!(batches[0]["requests"].as_array().unwrap().len(), 2);
    assert_eq!(
        batches[1]["requests"].as_array().unwrap().len(),
        1,
        "only the failed id is re-issued"
    );
    assert!(
        batches[1]["requests"][0]["url"]
            .as_str()
            .unwrap()
            .contains(&b)
    );
    assert!(h.sleeps.lock().unwrap().contains(&Duration::from_secs(3)));
}

impl common::Harness {
    fn overrides_clear(&self) {
        self.fx.overrides.lock().unwrap().clear();
    }
}

#[test]
fn a_400_on_the_timezone_preference_degrades_once_and_reads_keep_working() {
    let h = harness(&[]);
    let l = h.fx.add_list("Tasks", Some("defaultList"));
    h.fx.add_task(&l, "a", None, "notStarted");
    h.fx.push_override(Override {
        path_contains: "/tasks".into(),
        method: Some("GET".into()),
        status: 400,
        headers: vec![],
        body: json!({"error": {"code": "ErrorInvalidRequest", "message": "The Prefer header is not supported."}}).to_string(),
        remaining: 1,
    });
    let mut budget = Budget::for_tool(10_000, 50);
    let (tasks, _) = h.state.graph.list_tasks(&l, &mut budget).unwrap();
    assert_eq!(tasks.len(), 1);
    let reqs: Vec<_> =
        h.fx.requests()
            .into_iter()
            .filter(|q| q.path.contains("/tasks"))
            .collect();
    assert_eq!(reqs.len(), 2);
    assert!(
        reqs[0]
            .header("prefer")
            .unwrap()
            .contains("outlook.timezone")
    );
    assert!(
        !reqs[1]
            .header("prefer")
            .unwrap()
            .contains("outlook.timezone")
    );
    assert_eq!(h.state.graph.stats().timezone_mode, Some("client_side"));
    // A second 400 is a real error now, not another retry.
    h.fx.push_override(Override {
        path_contains: "/tasks".into(),
        method: Some("GET".into()),
        status: 400,
        headers: vec![],
        body: String::new(),
        remaining: 1,
    });
    h.state.cache_write().invalidate_list(&l);
    assert!(h.state.graph.list_tasks(&l, &mut budget).is_err());
}
