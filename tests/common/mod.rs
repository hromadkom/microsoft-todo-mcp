//! Hand-rolled fixture Entra/Graph server on `tiny_http` (already a
//! dependency) — no `wiremock`, no dev-dependencies. Serves an in-memory To Do
//! model with paging, `$batch`, and a queue of scripted overrides for
//! injecting 429/5xx; records every request so tests assert on counts.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use serde_json::{Value, json};

use microsoft_todo_mcp::auth::store::{self, SaveMode, TokenFile};
use microsoft_todo_mcp::auth::{
    AuthError, Grant, Secret, TokenEndpoint, TokenProvider, TokenSuccess,
};
use microsoft_todo_mcp::clock::FixedClock;
use microsoft_todo_mcp::config::{Config, Need, load_config};
use microsoft_todo_mcp::graph::client::GraphClient;
use microsoft_todo_mcp::graph::{GraphReq, GraphRes, GraphTransport, TransportFailure};
use microsoft_todo_mcp::sem::Semaphore;
use microsoft_todo_mcp::server::ServerState;

pub const CLIENT_ID: &str = "12345678-abcd-4321-9876-0123456789ab";
pub const GRAPH: &str = "https://graph.microsoft.com";

#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    /// Path + query, as received.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Override {
    pub path_contains: String,
    pub method: Option<String>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub remaining: usize,
}

#[derive(Default)]
pub struct Store {
    pub lists: Vec<Value>,
    pub tasks: HashMap<String, Vec<Value>>,
    pub checklist: HashMap<String, Vec<Value>>,
    pub page_size: usize,
    pub next_id: u64,
    /// Sub-response nextLink to emit inside `$batch` (for the origin test).
    pub batch_next_link_override: Option<String>,
}

impl Store {
    fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}=", self.next_id)
    }
}

pub struct Fixture {
    pub base: String,
    pub log: Arc<Mutex<Vec<Recorded>>>,
    pub store: Arc<Mutex<Store>>,
    pub overrides: Arc<Mutex<VecDeque<Override>>>,
    pub in_flight: Arc<AtomicUsize>,
    pub peak_in_flight: Arc<AtomicUsize>,
    pub foreign_host_hits: Arc<AtomicUsize>,
    _thread: std::thread::JoinHandle<()>,
}

impl Fixture {
    pub fn start() -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind fixture");
        let port = server.server_addr().to_ip().expect("ip").port();
        let base = format!("http://127.0.0.1:{port}");
        let log = Arc::new(Mutex::new(Vec::new()));
        let store = Arc::new(Mutex::new(Store {
            page_size: 100,
            ..Default::default()
        }));
        let overrides = Arc::new(Mutex::new(VecDeque::new()));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let foreign = Arc::new(AtomicUsize::new(0));
        let (l, s, o, i, p, f) = (
            log.clone(),
            store.clone(),
            overrides.clone(),
            in_flight.clone(),
            peak.clone(),
            foreign.clone(),
        );
        let thread = std::thread::spawn(move || {
            let server = Arc::new(server);
            // A few workers so the concurrency ceiling is observable.
            let mut hs = Vec::new();
            for _ in 0..8 {
                let (server, l, s, o, i, p, f) = (
                    server.clone(),
                    l.clone(),
                    s.clone(),
                    o.clone(),
                    i.clone(),
                    p.clone(),
                    f.clone(),
                );
                hs.push(std::thread::spawn(move || {
                    for mut req in server.incoming_requests() {
                        let now = i.fetch_add(1, Ordering::SeqCst) + 1;
                        p.fetch_max(now, Ordering::SeqCst);
                        let mut body = String::new();
                        let _ = req.as_reader().read_to_string(&mut body);
                        let rec = Recorded {
                            method: req.method().as_str().to_string(),
                            path: req.url().to_string(),
                            headers: req
                                .headers()
                                .iter()
                                .map(|h| {
                                    (h.field.as_str().to_string(), h.value.as_str().to_string())
                                })
                                .collect(),
                            body,
                        };
                        if rec.header("host").is_some_and(|h| h.contains("evil")) {
                            f.fetch_add(1, Ordering::SeqCst);
                        }
                        l.lock().unwrap().push(rec.clone());
                        // Simulate a little latency so concurrency overlaps.
                        std::thread::sleep(Duration::from_millis(5));
                        let (status, headers, body) = route(&rec, &s, &o);
                        let mut resp =
                            tiny_http::Response::from_string(body).with_status_code(status);
                        for (k, v) in headers {
                            if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes())
                            {
                                resp = resp.with_header(h);
                            }
                        }
                        if let Ok(h) = tiny_http::Header::from_bytes(
                            &b"Content-Type"[..],
                            &b"application/json"[..],
                        ) {
                            resp = resp.with_header(h);
                        }
                        let _ = req.respond(resp);
                        i.fetch_sub(1, Ordering::SeqCst);
                    }
                }));
            }
            for h in hs {
                let _ = h.join();
            }
        });
        Self {
            base,
            log,
            store,
            overrides,
            in_flight,
            peak_in_flight: peak,
            foreign_host_hits: foreign,
            _thread: thread,
        }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.log.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    pub fn reset_log(&self) {
        self.log.lock().unwrap().clear();
    }

    pub fn count_matching(&self, method: &str, path_contains: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.method == method && r.path.contains(path_contains))
            .count()
    }

    pub fn push_override(&self, o: Override) {
        self.overrides.lock().unwrap().push_back(o);
    }

    /// Seed a list. Returns its id.
    pub fn add_list(&self, name: &str, wellknown: Option<&str>) -> String {
        let mut s = self.store.lock().unwrap();
        let id = s.fresh_id("L");
        s.lists.push(json!({
            "@odata.etag": format!("W/\"{id}\""),
            "id": id, "displayName": name, "isOwner": true, "isShared": false,
            "wellknownListName": wellknown.unwrap_or("none"),
        }));
        s.tasks.entry(id.clone()).or_default();
        id
    }

    /// Seed a task. `due` is a `YYYY-MM-DD` interpreted as UTC midnight (the
    /// shape Graph returns for a western-hemisphere mailbox is tested separately).
    pub fn add_task(&self, list_id: &str, title: &str, due: Option<&str>, status: &str) -> String {
        self.add_task_full(
            list_id,
            json!({ "title": title, "status": status }),
            due.map(|d| (format!("{d}T00:00:00.0000000"), "UTC".to_string())),
        )
    }

    pub fn add_task_full(
        &self,
        list_id: &str,
        mut fields: Value,
        due: Option<(String, String)>,
    ) -> String {
        let mut s = self.store.lock().unwrap();
        let id = s.fresh_id("T");
        let created = format!("2026-08-{:02}T09:00:00.0000000Z", (s.next_id % 28) + 1);
        let obj = fields.as_object_mut().unwrap();
        obj.entry("id").or_insert(json!(id));
        obj.entry("importance").or_insert(json!("normal"));
        obj.entry("isReminderOn").or_insert(json!(false));
        obj.entry("status").or_insert(json!("notStarted"));
        obj.entry("categories").or_insert(json!([]));
        obj.entry("hasAttachments").or_insert(json!(false));
        obj.entry("createdDateTime").or_insert(json!(created));
        obj.entry("lastModifiedDateTime").or_insert(json!(created));
        obj.entry("body")
            .or_insert(json!({ "content": "", "contentType": "text" }));
        obj.insert("@odata.etag".into(), json!(format!("W/\"{id}\"")));
        if let Some((dt, tz)) = due {
            obj.insert(
                "dueDateTime".into(),
                json!({ "dateTime": dt, "timeZone": tz }),
            );
        }
        s.tasks.entry(list_id.to_string()).or_default().push(fields);
        id
    }

    pub fn set_page_size(&self, n: usize) {
        self.store.lock().unwrap().page_size = n;
    }

    pub fn task(&self, list_id: &str, task_id: &str) -> Option<Value> {
        self.store
            .lock()
            .unwrap()
            .tasks
            .get(list_id)?
            .iter()
            .find(|t| t["id"] == task_id)
            .cloned()
    }
}

fn split_path(p: &str) -> (String, Vec<(String, String)>) {
    let (path, query) = p.split_once('?').unwrap_or((p, ""));
    let q = query
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k.to_string(), v.to_string())
        })
        .collect();
    (path.to_string(), q)
}

fn page(items: &[Value], page_size: usize, skip: usize, base_path: &str) -> Value {
    let end = (skip + page_size).min(items.len());
    let mut v = json!({ "value": items[skip.min(items.len())..end] });
    if end < items.len() {
        v["@odata.nextLink"] = json!(format!(
            "{GRAPH}/v1.0{base_path}?$top={page_size}&$skip={end}"
        ));
    }
    v
}

type Reply = (u16, Vec<(String, String)>, String);

fn route(rec: &Recorded, store: &Mutex<Store>, overrides: &Mutex<VecDeque<Override>>) -> Reply {
    // Scripted overrides first.
    {
        let mut ov = overrides.lock().unwrap();
        if let Some(pos) = ov.iter().position(|o| {
            rec.path.contains(&o.path_contains)
                && o.method.as_deref().is_none_or(|m| m == rec.method)
                && o.remaining > 0
        }) {
            let o = &mut ov[pos];
            o.remaining -= 1;
            let reply = (o.status, o.headers.clone(), o.body.clone());
            if o.remaining == 0 {
                ov.remove(pos);
            }
            return reply;
        }
    }
    let (path, query) = split_path(&rec.path);
    // Entra
    if path.ends_with("/oauth2/v2.0/token") {
        let form: HashMap<String, String> = rec
            .body
            .split('&')
            .filter_map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.replace('+', " ")))
            })
            .collect();
        let scope = form
            .get("scope")
            .map(|s| percent_decode(s))
            .unwrap_or_else(|| "Tasks.ReadWrite offline_access".into());
        return (
            200,
            vec![],
            json!({
                "token_type": "Bearer", "expires_in": 3599, "scope": scope,
                "access_token": "AT-fixture", "refresh_token": "RT-fixture-new"
            })
            .to_string(),
        );
    }
    if path.ends_with("/oauth2/v2.0/devicecode") {
        return (200, vec![], json!({
            "device_code": "DC-fixture", "user_code": "ABCD-EFGH",
            "verification_uri": "https://example.invalid/device", "expires_in": 900, "interval": 1,
            "message": "To sign in, use a web browser to open the page https://example.invalid/device and enter the code ABCD-EFGH to authenticate."
        }).to_string());
    }
    if path == "/v1.0/$batch" && rec.method == "POST" {
        let env: Value = serde_json::from_str(&rec.body).unwrap_or(json!({}));
        let mut responses = Vec::new();
        for sub in env["requests"].as_array().cloned().unwrap_or_default() {
            let sub_rec = Recorded {
                method: sub["method"].as_str().unwrap_or("GET").to_string(),
                path: format!("/v1.0{}", sub["url"].as_str().unwrap_or("")),
                headers: sub["headers"]
                    .as_object()
                    .map(|h| {
                        h.iter()
                            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                            .collect()
                    })
                    .unwrap_or_default(),
                body: sub["body"].as_str().unwrap_or("").to_string(),
            };
            let (status, headers, body) = route(&sub_rec, store, overrides);
            let mut body_v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            if let Some(link) = store.lock().unwrap().batch_next_link_override.clone()
                && status == 200
            {
                body_v["@odata.nextLink"] = json!(link);
            }
            let hmap: serde_json::Map<String, Value> =
                headers.into_iter().map(|(k, v)| (k, json!(v))).collect();
            responses.push(
                json!({ "id": sub["id"], "status": status, "headers": hmap, "body": body_v }),
            );
        }
        return (200, vec![], json!({ "responses": responses }).to_string());
    }
    let segs: Vec<&str> = path
        .trim_start_matches("/v1.0")
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let skip: usize = query
        .iter()
        .find(|(k, _)| k == "$skip")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut s = store.lock().unwrap();
    let ps = s.page_size;
    match (rec.method.as_str(), segs.as_slice()) {
        ("GET", ["me", "todo", "lists"]) => (
            200,
            vec![],
            page(&s.lists, ps, skip, "/me/todo/lists").to_string(),
        ),
        ("GET", ["me", "todo", "lists", lid, "tasks"]) => match s.tasks.get(*lid) {
            Some(tasks) => (
                200,
                vec![],
                page(tasks, ps, skip, &format!("/me/todo/lists/{lid}/tasks")).to_string(),
            ),
            None => not_found(),
        },
        ("POST", ["me", "todo", "lists", lid, "tasks"]) => {
            if !s.tasks.contains_key(*lid) {
                return not_found();
            }
            let mut t: Value = match serde_json::from_str(&rec.body) {
                Ok(v) => v,
                Err(_) => {
                    return (
                        400,
                        vec![],
                        json!({"error": {"code": "BadRequest", "message": "invalid JSON"}})
                            .to_string(),
                    );
                }
            };
            let id = s.fresh_id("T");
            let obj = t.as_object_mut().unwrap();
            obj.insert("id".into(), json!(id));
            obj.entry("status").or_insert(json!("notStarted"));
            obj.entry("importance").or_insert(json!("normal"));
            obj.entry("categories").or_insert(json!([]));
            obj.entry("isReminderOn").or_insert(json!(false));
            obj.entry("hasAttachments").or_insert(json!(false));
            obj.insert(
                "createdDateTime".into(),
                json!("2026-08-25T10:00:00.0000000Z"),
            );
            obj.insert(
                "lastModifiedDateTime".into(),
                json!("2026-08-25T10:00:00.0000000Z"),
            );
            obj.insert("@odata.etag".into(), json!(format!("W/\"{id}\"")));
            s.tasks.get_mut(*lid).unwrap().push(t.clone());
            (201, vec![], t.to_string())
        }
        ("GET", ["me", "todo", "lists", lid, "tasks", tid]) => match s
            .tasks
            .get(*lid)
            .and_then(|ts| ts.iter().find(|t| t["id"] == *tid))
        {
            Some(t) => (200, vec![], t.to_string()),
            None => not_found(),
        },
        ("PATCH", ["me", "todo", "lists", lid, "tasks", tid]) => {
            let patch: Value = match serde_json::from_str(&rec.body) {
                Ok(v) => v,
                Err(_) => {
                    return (
                        400,
                        vec![],
                        json!({"error": {"code": "BadRequest", "message": "invalid JSON"}})
                            .to_string(),
                    );
                }
            };
            let Some(t) = s
                .tasks
                .get_mut(*lid)
                .and_then(|ts| ts.iter_mut().find(|t| t["id"] == *tid))
            else {
                return not_found();
            };
            for (k, v) in patch.as_object().cloned().unwrap_or_default() {
                if v.is_null() {
                    t.as_object_mut().unwrap().remove(&k);
                } else {
                    t[k] = v;
                }
            }
            if t["status"] == "completed" && t.get("completedDateTime").is_none() {
                t["completedDateTime"] =
                    json!({ "dateTime": "2026-08-25T10:00:00.0000000", "timeZone": "UTC" });
            }
            (200, vec![], t.to_string())
        }
        ("DELETE", ["me", "todo", "lists", lid, "tasks", tid]) => {
            let Some(ts) = s.tasks.get_mut(*lid) else {
                return not_found();
            };
            let before = ts.len();
            ts.retain(|t| t["id"] != *tid);
            if ts.len() == before {
                not_found()
            } else {
                (204, vec![], String::new())
            }
        }
        ("GET", ["me", "todo", "lists", _, "tasks", tid, "checklistItems"]) => {
            let items = s.checklist.get(*tid).cloned().unwrap_or_default();
            (200, vec![], json!({ "value": items }).to_string())
        }
        ("POST", ["me", "todo", "lists", _, "tasks", tid, "checklistItems"]) => {
            let mut item: Value = serde_json::from_str(&rec.body).unwrap_or(json!({}));
            let id = s.fresh_id("C");
            item["id"] = json!(id);
            item.as_object_mut()
                .unwrap()
                .entry("isChecked")
                .or_insert(json!(false));
            s.checklist
                .entry(tid.to_string())
                .or_default()
                .push(item.clone());
            (201, vec![], item.to_string())
        }
        (
            "PATCH",
            [
                "me",
                "todo",
                "lists",
                _,
                "tasks",
                tid,
                "checklistItems",
                cid,
            ],
        ) => {
            let patch: Value = serde_json::from_str(&rec.body).unwrap_or(json!({}));
            let Some(item) = s
                .checklist
                .get_mut(*tid)
                .and_then(|cs| cs.iter_mut().find(|c| c["id"] == *cid))
            else {
                return not_found();
            };
            for (k, v) in patch.as_object().cloned().unwrap_or_default() {
                item[k] = v;
            }
            (200, vec![], item.to_string())
        }
        (
            "DELETE",
            [
                "me",
                "todo",
                "lists",
                _,
                "tasks",
                tid,
                "checklistItems",
                cid,
            ],
        ) => {
            let Some(cs) = s.checklist.get_mut(*tid) else {
                return not_found();
            };
            let before = cs.len();
            cs.retain(|c| c["id"] != *cid);
            if cs.len() == before {
                not_found()
            } else {
                (204, vec![], String::new())
            }
        }
        ("GET", ["me", "todo", "lists", _, "tasks", _, "attachments"]) => {
            (200, vec![], json!({ "value": [] }).to_string())
        }
        _ => not_found(),
    }
}

fn not_found() -> Reply {
    (404, vec![], json!({"error": {"code": "ErrorItemNotFound", "message": "The specified object was not found in the store."}}).to_string())
}

fn percent_decode(s: &str) -> String {
    microsoft_todo_mcp::auth::percent_decode_lossy(s)
}

// ---------------------------------------------------------------------------
// The test transport: rewrites the Graph host to the fixture and records
// whether a bearer was attached. Origin checks happen BEFORE this layer, so an
// off-origin nextLink never reaches it.

pub struct FixtureTransport {
    base: String,
    agent: ureq::Agent,
    pub bearers_seen: Arc<Mutex<Vec<String>>>,
}

impl FixtureTransport {
    pub fn new(base: &str) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        Self {
            base: base.to_string(),
            agent,
            bearers_seen: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl GraphTransport for FixtureTransport {
    fn send(&self, req: &GraphReq, bearer: &str) -> Result<GraphRes, TransportFailure> {
        self.bearers_seen.lock().unwrap().push(bearer.to_string());
        let url = req.url.replacen(GRAPH, &self.base, 1);
        assert!(
            url.starts_with(&self.base),
            "transport asked to fetch a foreign URL: {}",
            req.url
        );
        let auth = format!("Bearer {bearer}");
        let result = match req.method {
            microsoft_todo_mcp::graph::Method::Get => {
                let mut b = self
                    .agent
                    .get(&url)
                    .header("Authorization", auth.as_str())
                    .header("client-request-id", req.client_request_id.as_str());
                for (k, v) in &req.headers {
                    b = b.header(k.as_str(), v.as_str());
                }
                b.call()
            }
            microsoft_todo_mcp::graph::Method::Delete => {
                let mut b = self
                    .agent
                    .delete(&url)
                    .header("Authorization", auth.as_str());
                for (k, v) in &req.headers {
                    b = b.header(k.as_str(), v.as_str());
                }
                b.call()
            }
            microsoft_todo_mcp::graph::Method::Post | microsoft_todo_mcp::graph::Method::Patch => {
                let mut b = if req.method == microsoft_todo_mcp::graph::Method::Post {
                    self.agent.post(&url)
                } else {
                    self.agent.patch(&url)
                };
                b = b.header("Authorization", auth.as_str());
                for (k, v) in &req.headers {
                    b = b.header(k.as_str(), v.as_str());
                }
                b.send(req.body.as_deref().unwrap_or(""))
            }
        };
        let mut res = result.map_err(|e| TransportFailure::Other(format!("{e:?}")))?;
        let status = res.status().as_u16();
        let headers = res
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_ascii_lowercase(), v.to_string()))
            })
            .collect();
        let body = res
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_vec()
            .map_err(|e| TransportFailure::Other(format!("{e:?}")))?;
        Ok(GraphRes {
            status,
            headers,
            body,
        })
    }
}

/// A `TokenEndpoint` that never touches the network.
pub struct StubEndpoint {
    pub calls: AtomicUsize,
    pub scope: Mutex<Option<String>>,
    pub seen_rts: Mutex<Vec<String>>,
    pub barrier: Mutex<Option<Arc<std::sync::Barrier>>>,
    pub fail_with: Mutex<Option<String>>,
}

impl StubEndpoint {
    pub fn new(scope: &str) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            scope: Mutex::new(Some(scope.to_string())),
            seen_rts: Mutex::new(Vec::new()),
            barrier: Mutex::new(None),
            fail_with: Mutex::new(None),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl TokenEndpoint for StubEndpoint {
    fn redeem_refresh_token(&self, rt: &str, _scope: &str) -> Result<TokenSuccess, AuthError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_rts.lock().unwrap().push(rt.to_string());
        if let Some(b) = self.barrier.lock().unwrap().clone() {
            b.wait();
        }
        if let Some(msg) = self.fail_with.lock().unwrap().clone() {
            return Err(AuthError::Transport(msg));
        }
        Ok(TokenSuccess {
            access_token: Secret::new(format!("AT-{}", self.calls())),
            token_type: "Bearer".into(),
            expires_in: 3600,
            scope: self.scope.lock().unwrap().clone(),
            refresh_token: Some(Secret::new(format!("{rt}-r"))),
        })
    }
}

pub fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "todo-mcp-it-{tag}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub fn write_token_file(dir: &std::path::Path, rt: &str, requested: &str) {
    let f = TokenFile {
        schema_version: store::SCHEMA_VERSION,
        account_id: "default".into(),
        client_id: CLIENT_ID.into(),
        authority: "https://login.microsoftonline.com/common".into(),
        requested_scope: requested.into(),
        granted_scope: requested.into(),
        refresh_token: Secret::new(rt),
        obtained_at: "2026-08-25T13:49:05.113000Z".into(),
        obtained_by: "device_code".into(),
    };
    store::save_atomic(dir, &f, None, SaveMode::Login).unwrap();
}

pub fn pinned_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 25, 12, 0, 0).unwrap()
}

pub fn config(dir: &std::path::Path, extra: &[(&str, &str)]) -> Config {
    load_config(
        |k| {
            extra
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
                .or_else(|| match k {
                    "TODO_MCP_CLIENT_ID" => Some(CLIENT_ID.into()),
                    "TODO_MCP_DATA_DIR" => Some(dir.display().to_string()),
                    "TODO_MCP_TZ" => Some("Europe/Prague".into()),
                    _ => None,
                })
        },
        Need::ClientId,
    )
    .unwrap()
}

/// Everything a tool-level test needs, wired to a fixture.
pub struct Harness {
    pub fx: Fixture,
    pub state: Arc<ServerState>,
    pub clock: Arc<FixedClock>,
    pub sleeps: Arc<Mutex<Vec<Duration>>>,
    pub sleep_block: Arc<(Mutex<bool>, std::sync::Condvar)>,
    pub endpoint: Arc<StubEndpoint>,
    pub dir: std::path::PathBuf,
}

enum Boot {
    Grant(Grant),
    Refresh,
    Unsigned,
    FollowSigned,
}

pub fn harness(extra: &[(&str, &str)]) -> Harness {
    harness_with_grant(extra, Grant::ReadWrite)
}

pub fn harness_with_grant(extra: &[(&str, &str)], grant: Grant) -> Harness {
    let scope = match grant {
        Grant::ReadWrite => "https://graph.microsoft.com/Tasks.ReadWrite offline_access",
        Grant::ReadOnly => "https://graph.microsoft.com/Tasks.Read offline_access",
        Grant::None => "offline_access",
    };
    build_harness_inner(extra, scope, Boot::Grant(grant))
}

/// Boot the way `serve` does (`cli/commands.rs::serve`): one refresh through the
/// real TokenProvider, then ServerState from `initial_grant()`, so the
/// TODO_MCP_SCOPE cap and the `.All` refusal are exercised, not bypassed.
/// `granted_scope` is what the stub token endpoint says Microsoft granted.
pub fn harness_booted(extra: &[(&str, &str)], granted_scope: &str) -> Harness {
    build_harness_inner(extra, granted_scope, Boot::Refresh)
}

pub fn harness_follow_signed(extra: &[(&str, &str)], endpoint_scope: &str) -> Harness {
    build_harness_inner(extra, endpoint_scope, Boot::FollowSigned)
}

/// Build the opt-in startup state before a token has landed on disk.
pub fn harness_unsigned(extra: &[(&str, &str)], endpoint_scope: &str) -> Harness {
    build_harness_inner(extra, endpoint_scope, Boot::Unsigned)
}

fn build_harness_inner(extra: &[(&str, &str)], endpoint_scope: &str, boot: Boot) -> Harness {
    let fx = Fixture::start();
    let dir = temp_dir("harness");
    let cfg = config(&dir, extra);
    let requested = cfg.scope.requested();
    if !matches!(boot, Boot::Unsigned) {
        write_token_file(&dir, "RT-OLD", &requested);
    }
    let clock = Arc::new(FixedClock::at(pinned_now()));
    let endpoint = Arc::new(StubEndpoint::new(endpoint_scope));
    struct Shared(Arc<StubEndpoint>);
    impl TokenEndpoint for Shared {
        fn redeem_refresh_token(&self, rt: &str, scope: &str) -> Result<TokenSuccess, AuthError> {
            self.0.redeem_refresh_token(rt, scope)
        }
    }
    let tokens = Arc::new(TokenProvider::new(
        Shared(endpoint.clone()),
        dir.clone(),
        &cfg.client_id,
        &cfg.authority(),
        &requested,
        clock.clone(),
    ));
    let boot_grant = match boot {
        Boot::Grant(g) => Some(g),
        Boot::Refresh => {
            tokens.access_token().expect("boot refresh");
            Some(tokens.initial_grant().expect("grant after boot"))
        }
        Boot::FollowSigned => {
            tokens.access_token().expect("boot refresh");
            None
        }
        Boot::Unsigned => None,
    };
    let sleeps = Arc::new(Mutex::new(Vec::new()));
    let s2 = sleeps.clone();
    let sleep_block = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let b2 = sleep_block.clone();
    let transport = FixtureTransport::new(&fx.base);
    let gate = Arc::new(Semaphore::new(cfg.graph_concurrency));
    let graph = GraphClient::new(
        transport,
        tokens,
        gate,
        cfg.max_attempts,
        Some("Central Europe Standard Time".into()),
    )
    .with_sleep(move |d| {
        s2.lock().unwrap().push(d);
        // Tests may park the sleeper to prove no permit is held across it.
        let (m, cv) = &*b2;
        let mut blocked = m.lock().unwrap();
        while *blocked {
            blocked = cv.wait(blocked).unwrap();
        }
    });
    let state = Arc::new(ServerState::new(cfg, graph, clock.clone(), boot_grant));
    Harness {
        fx,
        state,
        clock,
        sleeps,
        sleep_block,
        endpoint,
        dir,
    }
}

impl Harness {
    pub fn block_sleeps(&self, on: bool) {
        let (m, cv) = &*self.sleep_block;
        *m.lock().unwrap() = on;
        cv.notify_all();
    }

    pub fn call(&self, tool: &str, args: Value) -> Value {
        use microsoft_todo_mcp::mcp::ToolProvider;
        self.state.call_tool(tool, &args).expect("tool exists")
    }

    /// The structured content of a success, panicking on `isError`.
    pub fn ok(&self, tool: &str, args: Value) -> Value {
        let r = self.call(tool, args);
        assert!(
            r.get("isError").is_none(),
            "{tool} failed: {}",
            r["content"][0]["text"]
        );
        // The compatibility contract: content[0].text parses to exactly structuredContent.
        let text: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            text, r["structuredContent"],
            "text block drifted from structuredContent"
        );
        r["structuredContent"].clone()
    }

    /// The error text of a failure, panicking on success.
    pub fn err(&self, tool: &str, args: Value) -> String {
        let r = self.call(tool, args);
        assert_eq!(r["isError"], true, "{tool} unexpectedly succeeded: {r}");
        assert!(r.get("structuredContent").is_none());
        r["content"][0]["text"].as_str().unwrap().to_string()
    }

    pub fn graph_requests(&self) -> usize {
        self.fx
            .requests()
            .iter()
            .filter(|r| r.path.starts_with("/v1.0"))
            .count()
    }
}

/// The lookup-budget seed: `Tasks` (defaultList) holding "near", then lists
/// L01..L21 with "far" in L21. `prioritise` puts Tasks first and the rest by
/// name, so under `TODO_MCP_MAX_PAGES=2` a cold lookup spends the catalogue GET
/// and ONE `$batch` (Tasks, L01..L19) and never reaches L20 or L21.
pub struct Seed22 {
    pub near_list: String,
    pub near: String,
    pub far_list: String,
    pub far: String,
}

pub fn seed_22_lists(fx: &Fixture) -> Seed22 {
    let near_list = fx.add_list("Tasks", Some("defaultList"));
    let mut far_list = String::new();
    for i in 1..=21 {
        far_list = fx.add_list(&format!("L{i:02}"), None);
    }
    let near = fx.add_task(&near_list, "near", None, "notStarted");
    let far = fx.add_task(&far_list, "far", None, "notStarted");
    Seed22 {
        near_list,
        near,
        far_list,
        far,
    }
}
